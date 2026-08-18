//! `mana kill` — stop a sub-agent the PM launched and cannot stop itself.
//!
//! Three things have to be true for this to be safe, and each is a section
//! below:
//!
//! 1. **The right process dies.** `crate::spawn` puts every sub-agent in a
//!    process group of its own precisely so a CLI that backgrounded a helper
//!    dies with it; this kills the same group the same way (`killpg`, then the
//!    pid directly for a CLI that left the group with `setsid`).
//! 2. **Only that process dies.** A pid is a recycled resource. The registry
//!    record may be hours old, and the kernel may well have handed its pid to
//!    something else since. `status::guard` is asked first, and a refusal
//!    stops the command rather than being downgraded to a warning — see that
//!    function for exactly what the guard can and cannot promise.
//! 3. **Everyone finds out.** A killed dispatch would otherwise stay `running`
//!    in `mana ps` forever (the process that would have written its exit
//!    record is the one that just died) and the PM would wait for a
//!    notification that never comes. So a kill appends the same two things a
//!    normal completion appends: an `exited` record in the agent's log, and a
//!    line in `notifications.jsonl`. No new format, no second code path.
//!
//! One consequence worth knowing: if the mana process that spawned the
//! sub-agent is still alive, it will notice the death and append *its* own
//! exit record and notification too. Two notifications for one dispatch is
//! better than a kill that stayed invisible to the PM, and they now agree:
//! the `killed` line below is written *before* the signal precisely so the
//! supervising dispatch finds it and reports the death as this kill rather
//! than classifying the signal against the catalogue's failure signatures —
//! see `dispatch::killed_by_operator`. Move that append after the signal and
//! an operator's kill starts reading as the vendor's quota failure again.
//!
//! ## Two callers, one kill
//!
//! `kill_dispatch` is the whole of the three sections above, and the command
//! below is only the half that turns an id prefix into a record. The other
//! caller is the end of a PM session (`cli::launch_pm`): quitting mana stops
//! the sub-agents it had in flight, because nothing else ever would. They must
//! not be two kills — a teardown that skipped the guard would signal recycled
//! pids, and one that skipped the exit record would leave `mana ps` calling a
//! dead agent `running` for ever.

use crate::cli::ps::{Scope, resolve_project_name};
use crate::log::{ExitEntry, LogEntry, Status, append_log, now_iso8601};
use crate::mcp::runs::{Notification, notify};
use crate::project::{ProjectPaths, mana_home, resolve_project_paths};
use crate::status::{self, Dispatch, DispatchStatus, Guard, Liveness};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long to wait for a signalled process to actually disappear before
/// reporting what is still true. Short because the only party that can make it
/// disappear is its parent (the mana process that spawned it), which polls
/// every 20ms; if it has not gone by now, it is not going to in this command's
/// lifetime, and saying so beats blocking.
const DEATH_GRACE: Duration = Duration::from_millis(500);

pub fn run(agent_id: &str, all: bool, project: Option<&Path>) -> Result<()> {
    let home = mana_home()?;
    let scope = if all {
        Scope::All
    } else {
        Scope::One(resolve_project_name(project)?)
    };
    for line in kill_at(&home, &scope, agent_id, Utc::now())? {
        println!("{line}");
    }
    Ok(())
}

/// Everything `run` does except resolving the home directory and printing.
/// Returns what it would have printed, so the whole flow is testable against a
/// throwaway `~/.mana` and a throwaway process.
fn kill_at(
    mana_home: &Path,
    scope: &Scope,
    prefix: &str,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let dispatches = match scope {
        Scope::All => status::all_dispatches(mana_home)?,
        Scope::One(project) => status::dispatches_in(mana_home, project)?,
    };
    let dispatch = resolve(&dispatches, prefix, scope)?;
    let paths = resolve_project_paths(mana_home, &dispatch.project);
    let report = kill_dispatch(&paths, dispatch, now)?;
    // The command's one hard stop, and it is the *command's*: the shared
    // function reports a refusal as an outcome because its other caller (the
    // teardown sweep in `cli::launch_pm`) has several dispatches to get
    // through and must not stop at the first pid it may not touch.
    if let Verdict::Refused(reason) = report.verdict {
        bail!("{reason}");
    }
    Ok(report.lines)
}

/// What one kill attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The process group was signalled and the completion recorded.
    Signalled,
    /// The pid was already gone. Not an error -- the requested end state is
    /// the actual one -- and the completion is recorded all the same.
    AlreadyGone,
    /// The dispatch had already finished. Nothing signalled, nothing written.
    AlreadyFinished,
    /// `status::guard` says this pid is not the dispatch's process, so nothing
    /// was signalled and nothing was recorded. Carries the whole sentence,
    /// because both callers say the same thing about it.
    Refused(String),
}

/// One kill attempt, told.
#[derive(Debug)]
pub struct KillReport {
    pub verdict: Verdict,
    /// What happened, in mana's voice, ready to print.
    pub lines: Vec<String>,
}

/// Kills one recorded dispatch: guard, signal, record.
///
/// The single kill path in mana. `mana kill` reaches it after resolving an id
/// prefix; the end of a PM session reaches it for every dispatch still running
/// (`cli::launch_pm`), because quitting mana must not leave sub-agents behind
/// with nobody left to watch or stop them. Both therefore get the same guard,
/// the same signal, the same exit record and the same notification -- which is
/// the property that would rot immediately if this were written twice.
///
/// `Err` only for a dispatch mana cannot act on at all (no pid was ever
/// recorded) or a failure to write the completion records. A pid the guard
/// refuses is a `Verdict`, not an error: see `Verdict::Refused`.
pub fn kill_dispatch(
    paths: &ProjectPaths,
    dispatch: &Dispatch,
    now: DateTime<Utc>,
) -> Result<KillReport> {
    let agent = &dispatch.record.agent_id;

    match dispatch.status {
        // Already over. Recording a second completion would only add a second
        // notification for something the PM was told about already.
        DispatchStatus::Done => {
            let when = dispatch.finished_at.as_deref().unwrap_or("an unknown time");
            return Ok(KillReport {
                verdict: Verdict::AlreadyFinished,
                lines: vec![format!("{agent} finished at {when}; nothing to kill")],
            });
        }
        // No pid means mana never learned which process this was. Killing by
        // guesswork is exactly what the guard exists to prevent.
        DispatchStatus::Unknown if dispatch.record.pid.is_none() => bail!(
            "{agent} was recorded without a pid, so mana has nothing to signal. \
             It is listed as 'unknown' by `mana ps` and will stay that way."
        ),
        _ => {}
    }

    let pid = dispatch
        .record
        .pid
        .expect("a dispatch with no pid returned above");
    let mut lines = Vec::new();

    // Already gone, and nobody recorded it: the crash leftover. Not an error —
    // the requested end state is the actual end state — but the completion
    // record still has to be written, or `mana ps` keeps calling it stale.
    if dispatch.status == DispatchStatus::Stale {
        lines.push(format!("{agent}: pid {pid} is already gone"));
        mark_killed(paths, dispatch)?;
        record_completion(
            paths,
            dispatch,
            now,
            &format!("found already dead by `mana kill` (pid {pid})"),
        )?;
        lines.push(format!("{agent}: recorded as finished"));
        return Ok(KillReport {
            verdict: Verdict::AlreadyGone,
            lines,
        });
    }

    match status::guard(pid, &dispatch.record.started_at, now) {
        Guard::Ours => {}
        // The one hard stop. mana knows this pid is not the dispatch's, so
        // signalling it would kill a bystander; and it will not quietly mark
        // the dispatch finished either, because a `setsid`-ing CLI produces
        // the same evidence and may still be running.
        Guard::NotOurs(reason) => {
            return Ok(KillReport {
                verdict: Verdict::Refused(format!(
                    "refusing to kill {agent}: {reason}. Nothing was signalled and nothing was \
                     recorded. If you are certain, signal it yourself (`kill -9 {pid}`)."
                )),
                lines,
            });
        }
        Guard::Unverified(reason) => {
            lines.push(format!("warning: {reason}; killing anyway"));
        }
    }
    if dispatch.liveness == Liveness::Unknown {
        lines.push(format!(
            "warning: mana could not confirm that pid {pid} is alive before signalling it"
        ));
    }

    // Before the signal, not after: see `mark_killed`. The supervising
    // dispatch reads this the moment the child dies.
    mark_killed(paths, dispatch)?;
    signal(pid).with_context(|| format!("killing {agent} (pid {pid})"))?;
    lines.push(match wait_for_death(pid) {
        true => format!("{agent}: killed (pid {pid} and its process group are gone)"),
        false => format!(
            "{agent}: signalled pid {pid} and its process group, but it still answers — \
             its parent has not reaped it yet"
        ),
    });

    record_completion(
        paths,
        dispatch,
        now,
        &format!("killed by `mana kill` (pid {pid})"),
    )?;
    lines.push(format!("{agent}: recorded as finished"));
    Ok(KillReport {
        verdict: Verdict::Signalled,
        lines,
    })
}

/// Finds the one dispatch a prefix names, or refuses.
///
/// Ambiguity is refused loudly rather than resolved by "the first one" or "the
/// newest one": every rule that picks a winner picks the wrong one eventually,
/// and the wrong one here is somebody else's running agent.
fn resolve<'a>(dispatches: &'a [Dispatch], prefix: &str, scope: &Scope) -> Result<&'a Dispatch> {
    if prefix.is_empty() {
        bail!("an empty agent id matches every dispatch -- pass an id from `mana ps`");
    }
    // An id given in full wins over anything it happens to be a prefix of.
    if let Some(exact) = dispatches.iter().find(|d| d.record.agent_id == prefix) {
        return Ok(exact);
    }
    let matches: Vec<&Dispatch> = dispatches
        .iter()
        .filter(|d| d.record.agent_id.starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!(
            "no dispatch matching '{prefix}' {}. Run `mana ps{}` to see the ids.",
            where_it_looked(scope),
            if matches!(scope, Scope::All) {
                " --all"
            } else {
                ""
            }
        ),
        many => bail!(
            "'{prefix}' matches {} dispatches -- give more characters:\n{}",
            many.len(),
            many.iter()
                .map(|d| format!(
                    "  {} ({}/{}, {}, {})",
                    d.record.agent_id,
                    d.record.cli,
                    d.record.model,
                    d.status.word(),
                    d.project
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

fn where_it_looked(scope: &Scope) -> String {
    match scope {
        Scope::All => "in any project".to_string(),
        Scope::One(project) => format!("in project '{project}'"),
    }
}

/// Writes the two records a finished dispatch always leaves behind.
///
/// The `killed` line before the exit record is the audit trail: an exit record
/// with no code and no timeout looks exactly like any other signalled run, and
/// six months later "why did this one stop" has to be answerable from the log
/// itself. `read_last_exit` scans backwards for `action == "exited"`, so the
/// extra line changes nothing for any reader — and it is what the supervising
/// dispatch reads to tell an operator's kill apart from a vendor cutting the
/// process off (`dispatch::killed_by_operator`).
/// Says "an operator asked for this" in the agent's own log, and must run
/// *before* the signal.
///
/// `dispatch::run_dispatch` reads this marker the moment the child dies, to
/// tell a deliberate kill apart from a vendor cutting the process off. Written
/// afterwards it is always too late: the supervisor has already classified the
/// run on its captured output alone, and a CLI that merely printed the words
/// "rate limit" gets its whole pool rested for an hour (#199, #38).
fn mark_killed(paths: &ProjectPaths, dispatch: &Dispatch) -> Result<()> {
    append_log(
        &paths
            .logs
            .join(format!("{}.jsonl", dispatch.record.agent_id)),
        &LogEntry {
            status: Status::Running,
            action: "killed".to_string(),
            timestamp: now_iso8601(),
        },
    )
}

fn record_completion(
    paths: &ProjectPaths,
    dispatch: &Dispatch,
    now: DateTime<Utc>,
    outcome: &str,
) -> Result<()> {
    let record = &dispatch.record;
    let log_path = paths.logs.join(format!("{}.jsonl", record.agent_id));
    let timestamp = now_iso8601();

    append_log(
        &log_path,
        &ExitEntry {
            status: Status::Done,
            action: "exited".to_string(),
            timestamp: timestamp.clone(),
            // The same shape a SIGKILLed process produces on its own: no code,
            // because it never got to choose one.
            exit_code: None,
            duration_ms: dispatch
                .age(now)
                .and_then(|age| u64::try_from(age.num_milliseconds()).ok()),
            // Deliberately empty: the catalogue's failure vocabulary is about
            // what a *CLI* did (quota, rate limit, auth), and a cooldown for
            // "the operator pressed kill" would rest a pool for no reason.
            failure_means: None,
        },
    )?;

    notify(
        paths,
        &Notification {
            ts: timestamp,
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            agent_id: record.agent_id.clone(),
            outcome: outcome.to_string(),
        },
    )
}

/// Polls until the pid stops answering, or until `DEATH_GRACE` runs out.
fn wait_for_death(pid: u32) -> bool {
    let deadline = Instant::now() + DEATH_GRACE;
    loop {
        if status::probe(pid) == Liveness::Dead {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// SIGKILL to the group, then to the pid — the same pair, in the same order,
/// as `spawn::kill_group`, and for the same two reasons: the group covers
/// whatever the CLI started, and the direct signal covers a CLI that called
/// `setsid()` and left the group behind (where `killpg` reports ESRCH while the
/// process keeps running). SIGKILL and not SIGTERM because a sub-agent with a
/// TERM handler could otherwise ignore the operator outright.
#[cfg(unix)]
pub(crate) fn signal(pid: u32) -> Result<()> {
    let signed = i32::try_from(pid).map_err(|_| anyhow::anyhow!("pid {pid} is not a valid pid"))?;
    if signed <= 0 {
        bail!("refusing to signal pid {pid}: 0 and negative pids address whole groups");
    }
    // SAFETY: both calls take two integers, dereference nothing, and report an
    // already-gone target as ESRCH, which is handled below.
    let group = unsafe { libc::killpg(signed, libc::SIGKILL) };
    let group_error = (group != 0).then(std::io::Error::last_os_error);
    let direct = unsafe { libc::kill(signed, libc::SIGKILL) };
    let direct_error = (direct != 0).then(std::io::Error::last_os_error);

    match (group_error, direct_error) {
        // Neither reached anything: the process is not ours to signal, or it
        // vanished between the probe and here. Both are worth saying.
        (Some(group_error), Some(direct_error)) => bail!(
            "neither the process group nor the process itself could be signalled \
             (killpg: {group_error}; kill: {direct_error})"
        ),
        _ => Ok(()),
    }
}

/// Windows has no process group to signal, so this shells out to `taskkill`,
/// whose `/T` walks the process tree and `/F` is the TerminateProcess this
/// needs. `spawn::kill_group` calls this same function when a dispatch blows
/// its timeout: an operator kill and a timeout must give the same guarantee on
/// the same platform, and the tree walk is the only thing that reaches the
/// agent once the direct child is the `.cmd` shim's `cmd.exe`.
/// Executed on `windows-latest` by `spawn::windows_process_tests`, which
/// reaches it through `spawn::kill_group` and asserts a backgrounded grandchild
/// really died — so the `/T /F` pair is measured, on the `.cmd`-shim shape that
/// made the tree walk necessary. `mana kill`'s own path to it is not: the tests
/// that signal a real process directly are `#[cfg(all(test, unix))]`.
#[cfg(windows)]
pub(crate) fn signal(pid: u32) -> Result<()> {
    let output = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .context("running taskkill")?;
    if !output.status.success() {
        bail!(
            "taskkill refused to kill pid {pid} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{SubagentRecord, append_record};
    use crate::mcp::runs::notifications_path;
    use crate::status::{Dispatch, DispatchStatus, Liveness};
    use crate::task::Role;

    pub(super) fn record(agent_id: &str, pid: Option<u32>) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "claude".into(),
            model: "haiku".into(),
            role: Role::Executor,
            task_id: "3f2a1b6c-9d4e-4a7b-8c1d-2e5f0a9b8c7d".into(),
            pid,
            started_at: now_iso8601(),
        }
    }

    pub(super) fn seed(mana_home: &Path, project: &str, record: &SubagentRecord) {
        append_record(
            &resolve_project_paths(mana_home, project).subagents_file,
            record,
        )
        .unwrap();
    }

    fn dispatch(agent_id: &str, status: DispatchStatus) -> Dispatch {
        Dispatch {
            record: record(agent_id, Some(1)),
            project: "demo".into(),
            status,
            liveness: Liveness::Unknown,
            finished_at: None,
        }
    }

    pub(super) fn notifications(mana_home: &Path, project: &str) -> Vec<Notification> {
        let path = notifications_path(&resolve_project_paths(mana_home, project));
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_unique_prefix_resolves() {
        let dispatches = vec![
            dispatch("3f2a1b6c-aaaa", DispatchStatus::Running),
            dispatch("9d4e4a7b-bbbb", DispatchStatus::Done),
        ];
        let found = resolve(&dispatches, "3f2a", &Scope::One("demo".into())).unwrap();
        assert_eq!(found.record.agent_id, "3f2a1b6c-aaaa");
    }

    #[test]
    fn an_ambiguous_prefix_is_refused_and_lists_every_candidate() {
        let dispatches = vec![
            dispatch("3f2a1b6c-aaaa", DispatchStatus::Running),
            dispatch("3f2a1b6c-bbbb", DispatchStatus::Stale),
        ];
        let error = resolve(&dispatches, "3f2a", &Scope::One("demo".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("matches 2 dispatches"), "{error}");
        assert!(error.contains("3f2a1b6c-aaaa"), "{error}");
        assert!(error.contains("3f2a1b6c-bbbb"), "{error}");
        // The status is in the listing because it is usually what tells the
        // two apart -- one of them is the one still running.
        assert!(error.contains("running"), "{error}");
        assert!(error.contains("stale"), "{error}");
    }

    #[test]
    fn a_full_id_wins_over_the_longer_ids_it_prefixes() {
        let dispatches = vec![
            dispatch("3f2a1b6c", DispatchStatus::Running),
            dispatch("3f2a1b6c-bbbb", DispatchStatus::Running),
        ];
        let found = resolve(&dispatches, "3f2a1b6c", &Scope::One("demo".into())).unwrap();
        assert_eq!(found.record.agent_id, "3f2a1b6c");
    }

    #[test]
    fn an_unknown_prefix_names_where_it_looked() {
        let error = resolve(&[], "nope", &Scope::One("my-api".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("my-api"), "{error}");
        assert!(error.contains("mana ps"), "{error}");
    }

    #[test]
    fn an_unknown_prefix_across_projects_suggests_the_all_listing() {
        let error = resolve(&[], "nope", &Scope::All).unwrap_err().to_string();
        assert!(error.contains("any project"), "{error}");
        assert!(error.contains("mana ps --all"), "{error}");
    }

    #[test]
    fn an_empty_prefix_is_refused() {
        let error = resolve(&[], "", &Scope::One("demo".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty agent id"), "{error}");
    }

    #[test]
    fn killing_an_already_finished_dispatch_records_nothing_more() {
        let tmp = tempfile::tempdir().unwrap();
        let record = record("agent-done", Some(1));
        seed(tmp.path(), "demo", &record);
        let paths = resolve_project_paths(tmp.path(), "demo");
        append_log(
            &paths.logs.join("agent-done.jsonl"),
            &ExitEntry {
                status: Status::Done,
                action: "exited".into(),
                timestamp: "2026-08-15T10:05:00Z".into(),
                exit_code: Some(0),
                duration_ms: Some(10),
                failure_means: None,
            },
        )
        .unwrap();

        let lines = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-done",
            Utc::now(),
        )
        .unwrap();
        assert!(lines[0].contains("nothing to kill"), "{}", lines[0]);
        assert!(lines[0].contains("2026-08-15T10:05:00Z"), "{}", lines[0]);
        assert!(notifications(tmp.path(), "demo").is_empty());
    }

    #[test]
    fn killing_a_pidless_dispatch_is_refused_with_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "demo", &record("agent-nopid", None));
        let error = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-nopid",
            Utc::now(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("without a pid"), "{error}");
        assert!(notifications(tmp.path(), "demo").is_empty());
    }
}

#[cfg(all(test, unix))] // every test here signals a real process group
mod process_tests {
    use super::tests::{notifications, record, seed};
    use super::*;
    use crate::log::read_last_exit;
    use crate::status::probe;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    /// A sub-agent's shape, faithfully: a group leader that has backgrounded a
    /// child of its own. Killing the leader alone leaves the grandchild
    /// running for another 30s, which is exactly the leak `killpg` prevents.
    struct Group {
        child: Child,
        pidfile: std::path::PathBuf,
    }

    impl Group {
        fn spawn(dir: &Path) -> Group {
            let pidfile = dir.join("grandchild.pid");
            let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pidfile.display());
            let mut command = Command::new("sh");
            command
                .args(["-c", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command.process_group(0); // what crate::spawn does for every sub-agent
            let child = command.spawn().unwrap();
            Group { child, pidfile }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        /// Waits for the pidfile the shell writes, so the grandchild is known
        /// to exist before anything is killed.
        fn grandchild(&self) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(text) = std::fs::read_to_string(&self.pidfile)
                    && let Ok(pid) = text.trim().parse::<u32>()
                {
                    return pid;
                }
                assert!(Instant::now() < deadline, "the fixture never started");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn wait_until_dead(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if probe(pid) == Liveness::Dead {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn killing_a_dispatch_takes_down_its_whole_process_group_and_records_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut group = Group::spawn(tmp.path());
        let grandchild = group.grandchild();
        assert_eq!(probe(grandchild), Liveness::Alive);

        seed(tmp.path(), "demo", &record("agent-live", Some(group.pid())));
        let lines = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-l",
            Utc::now(),
        )
        .unwrap();

        // The leader was really signalled: it is our own child here, so its
        // status is observable rather than inferred.
        let status = group.child.wait().unwrap();
        assert!(!status.success(), "the group leader exited cleanly");
        // The grandchild is the point. It is nobody's child now (init reaps
        // it), so only the group signal could have reached it.
        assert!(
            wait_until_dead(grandchild),
            "grandchild {grandchild} survived the kill"
        );

        assert!(
            lines.iter().any(|l| l.contains("recorded as finished")),
            "{lines:?}"
        );

        // `mana ps` must stop calling it running, which means an exit record.
        let paths = resolve_project_paths(tmp.path(), "demo");
        let exit = read_last_exit(&paths.logs.join("agent-live.jsonl"))
            .unwrap()
            .expect("no exit record written");
        assert_eq!(exit.exit_code, None);
        assert!(exit.duration_ms.is_some());
        assert_eq!(exit.failure_means, None);
        // ...and the audit line that says why it stopped.
        let log = std::fs::read_to_string(paths.logs.join("agent-live.jsonl")).unwrap();
        assert!(log.contains("\"killed\""), "{log}");

        // ...and the PM has to hear about it, in the format it already reads.
        let notifications = notifications(tmp.path(), "demo");
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0].outcome.contains("mana kill"),
            "{:?}",
            notifications[0]
        );
        assert_eq!(notifications[0].agent_id, "agent-live");

        let dispatches = status::dispatches_in(tmp.path(), "demo").unwrap();
        assert_eq!(dispatches[0].status, DispatchStatus::Done);
    }

    #[test]
    fn killing_a_pid_that_is_already_gone_is_a_clean_no_op_that_still_records() {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();

        seed(tmp.path(), "demo", &record("agent-gone", Some(pid)));
        let lines = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-gone",
            Utc::now(),
        )
        .unwrap();

        assert!(lines[0].contains("already gone"), "{}", lines[0]);
        assert!(lines[1].contains("recorded as finished"), "{}", lines[1]);
        // The stale entry is reaped: this is how `mana ps` stops listing it.
        let dispatches = status::dispatches_in(tmp.path(), "demo").unwrap();
        assert_eq!(dispatches[0].status, DispatchStatus::Done);
        assert_eq!(notifications(tmp.path(), "demo").len(), 1);
    }

    #[test]
    fn a_recycled_pid_is_refused_and_nothing_is_signalled_or_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        // A live process that mana never spawned: it does not lead its own
        // group, which is what the guard notices.
        let mut bystander = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = bystander.id();

        seed(tmp.path(), "demo", &record("agent-recycled", Some(pid)));
        let error = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-recycled",
            Utc::now(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("refusing to kill"), "{error}");
        assert!(error.contains("recycled"), "{error}");
        assert!(error.contains(&format!("kill -9 {pid}")), "{error}");
        // The bystander is untouched...
        assert_eq!(probe(pid), Liveness::Alive);
        // ...and so is the dispatch's record: no phantom completion.
        assert!(notifications(tmp.path(), "demo").is_empty());
        let dispatches = status::dispatches_in(tmp.path(), "demo").unwrap();
        assert_eq!(dispatches[0].status, DispatchStatus::Running);

        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    #[test]
    fn a_group_leader_much_younger_than_its_record_is_refused_too() {
        let tmp = tempfile::tempdir().unwrap();
        let group = Group::spawn(tmp.path());
        let mut aged = record("agent-aged", Some(group.pid()));
        aged.started_at = (Utc::now() - chrono::TimeDelta::hours(4)).to_rfc3339();
        seed(tmp.path(), "demo", &aged);

        let error = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-aged",
            Utc::now(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("recycled"), "{error}");
        assert_eq!(probe(group.pid()), Liveness::Alive);
    }

    #[test]
    fn kill_finds_a_dispatch_in_another_project_only_with_all() {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        seed(tmp.path(), "other", &record("agent-elsewhere", Some(pid)));

        let error = kill_at(
            tmp.path(),
            &Scope::One("demo".into()),
            "agent-elsewhere",
            Utc::now(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("demo"), "{error}");

        let lines = kill_at(tmp.path(), &Scope::All, "agent-elsewhere", Utc::now()).unwrap();
        assert!(lines[0].contains("already gone"), "{}", lines[0]);
    }
}
