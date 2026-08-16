//! What mana can honestly say about a dispatch it recorded.
//!
//! `subagents.jsonl` is append-only and a record never changes after it is
//! written (see `crate::lock`), so "is this agent still running" is not stored
//! anywhere — it is *derived*, here, from two things mana can observe right
//! now: the agent's own log, and the operating system's answer about its pid.
//! `mana ps`, `mana kill` and `mana doctor` all ask that same question, so it
//! is answered once, in this module, rather than three times with three
//! slightly different notions of "stale".
//!
//! ## What counts as finished
//!
//! A dispatch is `done` when its log (`logs/<agent_id>.jsonl`) carries an
//! `exited` record — `crate::dispatch` appends exactly one, for every run,
//! success or failure, and `mana kill` appends one too. That is the only
//! per-dispatch completion signal mana has.
//!
//! It is deliberately *not* `runs/<task_id>.json` nor `notifications.jsonl`:
//! both are keyed on the task, and both carry the *PM's* agent id, which is a
//! different UUID from the registry's (`mcp.rs` mints one for the tool reply,
//! `dispatch.rs` mints another for the registry row). A second executor on a
//! retried task would therefore read as finished the moment the first one's
//! run record landed. The log is per-agent and cannot lie that way.
//!
//! ## What liveness can and cannot promise
//!
//! The probe never signals the process (`kill(pid, 0)` on unix is an
//! existence/permission check and delivers nothing), so asking is free of side
//! effects. It still cannot promise much:
//!
//! - A **zombie** — exited, not yet reaped by the mana process that spawned it
//!   — still answers "alive". The spawner reaps within ~20ms, so the window is
//!   small, but a dispatch can read `running` for a moment after it ended.
//! - A **recycled pid** reads as alive even though the dispatch is long gone.
//!   That is what `guard` exists to catch before anything is killed.
//! - On **Windows** there is no free existence check without linking
//!   `windows-sys`; `tasklist` is shelled out to instead, and any doubt reads
//!   as `unknown` rather than as a guess.

use crate::lock::{SubagentRecord, load_registry};
use crate::log::read_last_exit;
use crate::project::resolve_project_paths;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use std::path::Path;

/// How much of a UUID a table column shows. Eight hex chars is the same
/// budget `worktree` spends on a task directory, and it is what makes
/// `mana kill 3f2a1b6c` a thing a human can type off a `mana ps` row.
pub const SHORT_ID_CHARS: usize = 8;

/// How far the process's own age may fall short of the record's age before
/// `guard` calls the pid recycled. Generous on purpose: the only legitimate
/// sources of a gap are `ps` truncating to whole seconds and the system clock
/// being stepped (NTP) between the dispatch and now, while wrapping the pid
/// space takes minutes at the very least.
const REUSE_TOLERANCE: TimeDelta = TimeDelta::seconds(120);

/// What the operating system says about a pid, without signalling it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    /// The question could not be asked (no pid recorded, an unusable pid, or
    /// a platform with no free way to ask). Never a synonym for dead.
    Unknown,
}

/// The status a dispatch is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    /// No exit record, and the pid answers.
    Running,
    /// The log carries an `exited` record.
    Done,
    /// No exit record, and the pid is gone: the crash / `kill -9` / reboot
    /// leftover. Nothing will ever finish this dispatch.
    Stale,
    /// No exit record and mana cannot tell whether the process is alive.
    Unknown,
}

impl DispatchStatus {
    pub fn word(self) -> &'static str {
        match self {
            DispatchStatus::Running => "running",
            DispatchStatus::Done => "done",
            DispatchStatus::Stale => "stale",
            DispatchStatus::Unknown => "unknown",
        }
    }
}

/// One registry record joined with everything mana can observe about it now.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub record: SubagentRecord,
    /// Which `~/.mana/projects/<name>` this record was read from. Carried so
    /// `mana ps --all` can name it and `mana kill` can find its way back to
    /// the right logs directory.
    pub project: String,
    pub status: DispatchStatus,
    pub liveness: Liveness,
    /// The timestamp on the `exited` record, when there is one.
    pub finished_at: Option<String>,
}

impl Dispatch {
    pub fn short_agent(&self) -> &str {
        short(&self.record.agent_id)
    }

    pub fn short_task(&self) -> &str {
        short(&self.record.task_id)
    }

    /// How long ago the dispatch started. `None` when `started_at` cannot be
    /// parsed — a log mana cannot date is not one to invent a duration for.
    pub fn age(&self, now: DateTime<Utc>) -> Option<TimeDelta> {
        parse_started_at(&self.record.started_at).map(|started| now - started)
    }
}

/// The first `SHORT_ID_CHARS` of an id, or all of it when it is shorter.
/// Byte-sliced: every id mana mints is a hyphenated ASCII UUID, and `get`
/// returns `None` rather than panicking if some future id is not.
pub fn short(id: &str) -> &str {
    id.get(..SHORT_ID_CHARS).unwrap_or(id)
}

fn parse_started_at(started_at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(started_at)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

/// `12s`, `4m`, `3h`, `2d` — one unit, never two. A dispatch table is scanned,
/// not read, and "1d 4h 12m 3s" is four numbers where one would do.
pub fn render_age(age: Option<TimeDelta>) -> String {
    let Some(age) = age else {
        return "?".to_string();
    };
    // A clock stepped backwards would otherwise print a negative age, which
    // reads as a bug in mana rather than as one in the clock.
    let seconds = age.num_seconds().max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Everything the OS is asked about a pid, in one place.
///
/// Signal 0 is the POSIX existence check: it validates the pid and the
/// permission to signal it, and delivers nothing. `EPERM` means the process
/// exists and belongs to somebody else, which is still "alive".
#[cfg(unix)]
pub fn probe(pid: u32) -> Liveness {
    // pid 0 means "my own process group" to `kill`, and a negative pid means
    // a group. Neither is a sub-agent, and both would make the probe answer
    // about mana itself.
    let Ok(pid) = i32::try_from(pid) else {
        return Liveness::Unknown;
    };
    if pid <= 0 {
        return Liveness::Unknown;
    }
    // SAFETY: `kill` takes two integers and dereferences nothing. Signal 0
    // delivers nothing at all.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Liveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) => Liveness::Alive,
        Some(libc::ESRCH) => Liveness::Dead,
        _ => Liveness::Unknown,
    }
}

/// Windows has no free existence check without linking `windows-sys`, so this
/// shells out to `tasklist`, which ships with the OS. The CSV row's second
/// field is the pid in quotes, which is what makes the match locale-proof —
/// the "no tasks match" line tasklist prints instead is prose, and prose in
/// any language contains no `"1234"`.
///
/// Unmeasured on a real Windows box (v2 ships no Windows CI). Every failure
/// reads as `Unknown`, so the worst case is a `mana ps` that says it does not
/// know — never one that claims a dead agent is running.
#[cfg(windows)]
pub fn probe(pid: u32) -> Liveness {
    if pid == 0 {
        return Liveness::Unknown;
    }
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    let Ok(output) = output else {
        return Liveness::Unknown;
    };
    if !output.status.success() {
        return Liveness::Unknown;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains(&format!("\"{pid}\"")) {
        Liveness::Alive
    } else {
        Liveness::Dead
    }
}

/// Derives one record's status. Split from `dispatches_in` so the
/// classification can be tested against hand-built records without a registry
/// file in the way.
pub fn classify(
    record: &SubagentRecord,
    logs_dir: &Path,
) -> Result<(DispatchStatus, Liveness, Option<String>)> {
    let log_path = logs_dir.join(format!("{}.jsonl", record.agent_id));
    // Asked before the pid is probed, and answered without probing at all: a
    // finished dispatch's pid may well have been handed to something else by
    // now, and the exit record is the fact while the pid is only evidence.
    if let Some(exit) = read_last_exit(&log_path)? {
        return Ok((
            DispatchStatus::Done,
            Liveness::Unknown,
            Some(exit.timestamp),
        ));
    }
    let Some(pid) = record.pid else {
        return Ok((DispatchStatus::Unknown, Liveness::Unknown, None));
    };
    let liveness = probe(pid);
    let status = match liveness {
        Liveness::Alive => DispatchStatus::Running,
        Liveness::Dead => DispatchStatus::Stale,
        Liveness::Unknown => DispatchStatus::Unknown,
    };
    Ok((status, liveness, None))
}

/// Every dispatch mana ever recorded for one project, oldest first (the
/// registry's own append order).
pub fn dispatches_in(mana_home: &Path, project: &str) -> Result<Vec<Dispatch>> {
    let paths = resolve_project_paths(mana_home, project);
    let registry = load_registry(&paths.subagents_file)?;
    // Every caller of this function is a CLI command (`ps`, `kill`, `doctor`,
    // and the quit-time sweep), so a corrupt line gets said out loud exactly
    // once per command, on stderr: the listing on stdout stays pipeable and
    // the exit code stays whatever the command decided. The TUI's graph pane
    // does not come through here — it reads the registry directly and would
    // otherwise repaint this every two seconds over its own screen.
    for message in &registry.skipped {
        eprintln!("warning: {message}");
    }
    registry
        .records
        .into_iter()
        .map(|record| {
            let (status, liveness, finished_at) = classify(&record, &paths.logs)?;
            Ok(Dispatch {
                record,
                project: project.to_string(),
                status,
                liveness,
                finished_at,
            })
        })
        .collect()
}

/// Every project mana has state for, in directory-name order. A missing
/// `projects/` directory is a mana that has never dispatched anything, which
/// is an empty list and not an error.
pub fn project_names(mana_home: &Path) -> Result<Vec<String>> {
    let root = mana_home.join("projects");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", root.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("reading {}", entry.path().display()))?
            .is_dir()
        {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The same, across every project under `<mana_home>/projects`.
pub fn all_dispatches(mana_home: &Path) -> Result<Vec<Dispatch>> {
    let mut all = Vec::new();
    for project in project_names(mana_home)? {
        all.extend(dispatches_in(mana_home, &project)?);
    }
    Ok(all)
}

/// Whether a live pid is plausibly still the dispatch's own process.
///
/// See `guard` for what each answer is worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    /// Every check that could run passed. How many that is depends on the
    /// platform (two on unix, one on Windows) -- see `guard`.
    Ours,
    /// A check failed outright: this pid is somebody else's process.
    NotOurs(String),
    /// Not a single check could run. The caller may proceed, loudly.
    Unverified(String),
}

/// Sanity-checks that `pid` still belongs to the dispatch recorded at
/// `started_at`, before anything is signalled.
///
/// Two checks, in order of how much they cost:
///
/// 1. **Process group.** `crate::spawn` puts every sub-agent in a process
///    group of its own (`process_group(0)`), so a mana sub-agent's pgid always
///    equals its pid. A pid that does not lead its own group cannot be one.
/// 2. **Age.** The process's elapsed time (via `ps -o etime=`) must not be
///    dramatically *shorter* than the record's age: a recycled pid was, by
///    definition, handed out after the original process died, so its process
///    is younger than the record.
///
/// What this cannot promise: a pid recycled onto another *group leader*
/// (every shell is one) within `REUSE_TOLERANCE` of the dispatch's own age
/// passes both checks. Pid recycling that fast requires wrapping the pid space
/// in seconds, which no realistic machine does — but "unlikely" is the whole
/// guarantee, not "impossible". Windows has only the second check; see the
/// `#[cfg(windows)]` twin below.
#[cfg(unix)]
pub fn guard(pid: u32, started_at: &str, now: DateTime<Utc>) -> Guard {
    let Ok(signed) = i32::try_from(pid) else {
        return Guard::Unverified(format!("pid {pid} does not fit a pid_t"));
    };
    // SAFETY: `getpgid` takes an integer and dereferences nothing; it reports
    // an unknown pid as -1/ESRCH, which is handled below.
    let pgid = unsafe { libc::getpgid(signed) };
    if pgid < 0 {
        return Guard::Unverified(format!(
            "the system would not report a process group for pid {pid} ({})",
            std::io::Error::last_os_error()
        ));
    }
    if pgid != signed {
        return Guard::NotOurs(format!(
            "pid {pid} is not the leader of its own process group (its group is {pgid}). \
             Every mana sub-agent leads its own group, so this pid has been recycled onto \
             an unrelated process"
        ));
    }

    // The group check already passed, so an age check that cannot run does not
    // undo it.
    //
    // ponytail: that fallback silently downgrades a two-check `Ours` to a
    // one-check `Ours` — `elapsed_secs` shells out to `ps`, which can fail for
    // reasons that have nothing to do with the pid (fork failure under load,
    // a hardened container with no `ps`), and the caller cannot tell the two
    // apart. Left as is deliberately: `Guard` is what `mana kill` branches on,
    // and a third "verified once" arm would put a new prompt in front of every
    // kill on any box where `ps` is unavailable — worse than the ceiling it
    // removes. Upgrade path if it ever matters: carry the count of checks that
    // ran alongside `Ours` and let `kill` decide whether to say so.
    age_check(pid, started_at, now, Guard::Ours)
}

/// Windows has no process group for a sub-agent to lead, so only the second of
/// the two checks exists here — but it does exist, and a pid that fails it is
/// refused exactly as on unix rather than downgraded to "killing anyway". That
/// matters more here than anywhere: `tasklist` answers for *any* pid, so a
/// stale record classifies as `Running`, and the quit-time sweep in
/// `cli::launch_pm` reaches this path with nobody at the keyboard to veto it.
///
/// What it still cannot promise, and the README says so: a pid recycled onto a
/// process of roughly the dispatch's own age passes, where unix would also have
/// asked whether it leads its own group.
#[cfg(windows)]
pub fn guard(pid: u32, started_at: &str, now: DateTime<Utc>) -> Guard {
    age_check(
        pid,
        started_at,
        now,
        // No group check ran before this one, so there is nothing left standing
        // when the age cannot be read either -- unlike unix, this really is
        // "not a single check could run".
        Guard::Unverified(format!(
            "mana could not read a creation time for pid {pid}, so it cannot check that this \
             is still the dispatch's own process"
        )),
    )
}

/// The age half of `guard`, shared by both platforms because the reasoning is:
/// a recycled pid was handed out *after* the original process died, so its
/// process is younger than the record. `unanswerable` is what to return when
/// the process's age cannot be read at all — the two platforms differ only in
/// how much evidence is left standing at that point.
fn age_check(pid: u32, started_at: &str, now: DateTime<Utc>, unanswerable: Guard) -> Guard {
    let (Some(elapsed), Some(started)) = (elapsed_secs(pid), parse_started_at(started_at)) else {
        return unanswerable;
    };
    let record_age = (now - started).num_seconds();
    if recycled_by_age(elapsed, record_age) {
        return Guard::NotOurs(format!(
            "pid {pid} has been running for {elapsed}s but the dispatch was started \
             {record_age}s ago, so this pid has been recycled onto a younger process"
        ));
    }
    Guard::Ours
}

/// The refusal decision itself, with no process anywhere near it: is a process
/// of `elapsed` seconds too young to be a dispatch recorded `record_age`
/// seconds ago? Split out so the one branch that decides whether mana kills a
/// bystander is testable on a host that cannot run the platform's age lookup.
fn recycled_by_age(elapsed: i64, record_age: i64) -> bool {
    TimeDelta::seconds(elapsed) + REUSE_TOLERANCE < TimeDelta::seconds(record_age)
}

/// How long the process behind `pid` has been running, in seconds.
///
/// `ps -o etime=` rather than a `/proc` read or a `sysctl` call: `/proc` does
/// not exist on macOS, `sysctl(KERN_PROC)` means declaring `kinfo_proc` layouts
/// per platform, and `etime`'s `[[DD-]HH:]MM:SS` is specified by POSIX and
/// printed identically by both. `None` on anything unexpected — the caller
/// treats an unanswerable check as a check that did not run.
#[cfg(unix)]
fn elapsed_secs(pid: u32) -> Option<i64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_etime(std::str::from_utf8(&output.stdout).ok()?.trim())
}

/// Parses POSIX `etime`: `[[DD-]HH:]MM:SS`.
#[cfg(unix)]
fn parse_etime(text: &str) -> Option<i64> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.trim().parse::<i64>().ok()?, rest),
        None => (0, text),
    };
    let mut parts = Vec::new();
    for part in rest.split(':') {
        parts.push(part.trim().parse::<i64>().ok()?);
    }
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (0, *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => return None,
    };
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// The same, on Windows. Nothing here is free: `tasklist` prints no age column,
/// `wmic` was removed in Windows 11 24H2, and a `NtQueryInformationProcess` /
/// `GetProcessTimes` call means linking `windows-sys` for one number. Windows
/// PowerShell ships with every supported release and is already this module's
/// idiom for asking the OS a question (see `probe`), so the creation time the
/// kernel keeps anyway costs a process rather than a dependency.
///
/// `ToUniversalTime().ToString("o")` and not the default `ToString()`: the
/// round-trip format is culture-invariant, and a check that a bystander's fate
/// depends on must not turn on the box's locale. `-ErrorAction Stop` makes an
/// unknown pid — or a process whose start time is not readable — a non-zero
/// exit rather than an empty success. `None` on anything unexpected; the caller
/// treats an unanswerable check as a check that did not run.
#[cfg(windows)]
fn elapsed_secs(pid: u32) -> Option<i64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "(Get-Process -Id {pid} -ErrorAction Stop).StartTime.ToUniversalTime()\
                 .ToString('o')"
            ),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let created = parse_started_at(std::str::from_utf8(&output.stdout).ok()?.trim())?;
    // The caller's `now` would be marginally more consistent, but it is the
    // same clock a few milliseconds earlier and the tolerance is 120 seconds.
    Some((Utc::now() - created).num_seconds())
}

/// Left-aligns a table by column, two spaces between columns, no trailing
/// whitespace. A whole table crate for four columns would be a dependency to
/// audit in exchange for arithmetic that fits on a screen — and `mana ps` has
/// to stay greppable, which most of them are not by default.
///
/// Rows may be ragged; a short row simply ends early.
pub fn render_table(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (index, cell) in row.iter().enumerate() {
                let last = index + 1 == row.len();
                line.push_str(cell);
                if !last {
                    let padding = widths[index].saturating_sub(cell.chars().count()) + 2;
                    line.push_str(&" ".repeat(padding));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ExitEntry, LogEntry, Status, append_log};
    use crate::task::Role;

    pub(super) fn record(agent_id: &str, pid: Option<u32>, started_at: &str) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "fixture".into(),
            model: "cheapo".into(),
            role: Role::Executor,
            task_id: "3f2a1b6c-9d4e-4a7b-8c1d-2e5f0a9b8c7d".into(),
            pid,
            started_at: started_at.to_string(),
        }
    }

    fn write_exit(logs: &Path, agent_id: &str) {
        append_log(
            &logs.join(format!("{agent_id}.jsonl")),
            &ExitEntry {
                status: Status::Done,
                action: "exited".into(),
                timestamp: "2026-08-15T10:05:00Z".into(),
                exit_code: Some(0),
                duration_ms: Some(1000),
                input_tokens: None,
                output_tokens: None,
                failure_means: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn an_exit_record_wins_over_a_pid_that_still_answers() {
        // pid 1 is alive on every unix and absent on Windows -- either way the
        // exit record has to decide, which is the point: a finished agent's
        // pid belongs to somebody else by now.
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        write_exit(&logs, "agent-1");

        let (status, _, finished_at) =
            classify(&record("agent-1", Some(1), "2026-08-15T10:00:00Z"), &logs).unwrap();
        assert_eq!(status, DispatchStatus::Done);
        assert_eq!(finished_at.as_deref(), Some("2026-08-15T10:05:00Z"));
    }

    #[test]
    fn a_started_line_alone_is_not_a_completion_record() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        append_log(
            &logs.join("agent-1.jsonl"),
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: "2026-08-15T10:00:00Z".into(),
            },
        )
        .unwrap();

        let (status, _, _) =
            classify(&record("agent-1", None, "2026-08-15T10:00:00Z"), &logs).unwrap();
        assert_ne!(status, DispatchStatus::Done);
    }

    #[test]
    fn a_record_without_a_pid_is_unknown_rather_than_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let (status, liveness, _) = classify(
            &record("agent-1", None, "2026-08-15T10:00:00Z"),
            &tmp.path().join("logs"),
        )
        .unwrap();
        assert_eq!(status, DispatchStatus::Unknown);
        assert_eq!(liveness, Liveness::Unknown);
    }

    #[test]
    fn probe_refuses_pid_zero_rather_than_signalling_our_own_group() {
        assert_eq!(probe(0), Liveness::Unknown);
    }

    #[test]
    fn render_age_uses_one_unit() {
        assert_eq!(render_age(Some(TimeDelta::seconds(12))), "12s");
        assert_eq!(render_age(Some(TimeDelta::seconds(59))), "59s");
        assert_eq!(render_age(Some(TimeDelta::seconds(60))), "1m");
        assert_eq!(render_age(Some(TimeDelta::minutes(59))), "59m");
        assert_eq!(render_age(Some(TimeDelta::hours(1))), "1h");
        assert_eq!(render_age(Some(TimeDelta::hours(47))), "47h");
        assert_eq!(render_age(Some(TimeDelta::hours(48))), "2d");
        assert_eq!(render_age(Some(TimeDelta::days(9))), "9d");
    }

    #[test]
    fn render_age_clamps_a_clock_that_went_backwards() {
        assert_eq!(render_age(Some(TimeDelta::seconds(-30))), "0s");
    }

    #[test]
    fn render_age_says_it_does_not_know_rather_than_guessing() {
        assert_eq!(render_age(None), "?");
    }

    #[test]
    fn age_is_none_for_an_undatable_record() {
        let dispatch = Dispatch {
            record: record("agent-1", None, "not a timestamp"),
            project: "demo".into(),
            status: DispatchStatus::Unknown,
            liveness: Liveness::Unknown,
            finished_at: None,
        };
        assert!(dispatch.age(Utc::now()).is_none());
    }

    /// The refusal decision, on every platform including the one whose age
    /// lookup this host cannot run. Both `guard`s route through it, so this is
    /// the branch that decides whether `mana kill` signals a bystander.
    #[test]
    fn a_process_younger_than_its_record_by_more_than_the_tolerance_is_recycled() {
        let slack = REUSE_TOLERANCE.num_seconds();
        // A dispatch started an hour ago whose pid is now three seconds old.
        assert!(recycled_by_age(3, 3600));
        // Exactly at the tolerance is still ours: the gap is what `ps` truncates
        // and what a stepped clock produces, not evidence of reuse.
        assert!(!recycled_by_age(0, slack));
        assert!(recycled_by_age(0, slack + 1));
        // The ordinary case -- a process about as old as its record.
        assert!(!recycled_by_age(300, 300));
        // A process *older* than its record is never reuse (a clock stepped
        // forward between the spawn and now produces exactly this).
        assert!(!recycled_by_age(3600, 10));
    }

    /// The Windows age lookup reads `StartTime.ToUniversalTime().ToString("o")`
    /// back through `parse_started_at`, so the round-trip format has to survive
    /// it -- seven fractional digits and all. Cheap to assert here, and the
    /// alternative is discovering it on a Windows box during an incident.
    #[test]
    fn the_powershell_round_trip_timestamp_parses() {
        let parsed = parse_started_at("2026-08-16T12:34:56.7890123Z").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-08-16T12:34:56.789012300+00:00");
        // The same instant with an offset, which is what `ToString("o")` emits
        // if the DateTime's Kind is ever Local rather than Utc.
        assert_eq!(
            parse_started_at("2026-08-16T14:34:56.7890123+02:00").unwrap(),
            parsed
        );
        assert!(parse_started_at("").is_none());
    }

    #[test]
    fn short_ids_are_the_first_eight_chars_and_never_panic() {
        assert_eq!(short("3f2a1b6c-9d4e-4a7b"), "3f2a1b6c");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn dispatches_in_reads_the_registry_in_append_order() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        crate::lock::append_record(
            &paths.subagents_file,
            &record("agent-1", None, "2026-08-15T10:00:00Z"),
        )
        .unwrap();
        crate::lock::append_record(
            &paths.subagents_file,
            &record("agent-2", None, "2026-08-15T10:01:00Z"),
        )
        .unwrap();
        write_exit(&paths.logs, "agent-2");

        let dispatches = dispatches_in(tmp.path(), "demo").unwrap();
        assert_eq!(dispatches.len(), 2);
        assert_eq!(dispatches[0].record.agent_id, "agent-1");
        assert_eq!(dispatches[0].status, DispatchStatus::Unknown);
        assert_eq!(dispatches[1].status, DispatchStatus::Done);
        assert_eq!(dispatches[1].project, "demo");
    }

    /// `mana ps`, `mana kill` and `mana doctor` all reach the registry through
    /// here, and all three died on the first unreadable line — including the
    /// one command whose entire job is to diagnose that state.
    #[test]
    fn a_torn_registry_line_does_not_take_the_listing_down_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        crate::lock::append_record(
            &paths.subagents_file,
            &record("agent-1", None, "2026-08-15T10:00:00Z"),
        )
        .unwrap();
        std::fs::write(
            &paths.subagents_file,
            format!(
                "{}{{\"agent_id\":\"agent-2\"\n",
                std::fs::read_to_string(&paths.subagents_file).unwrap()
            ),
        )
        .unwrap();

        let dispatches = dispatches_in(tmp.path(), "demo").unwrap();
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].record.agent_id, "agent-1");
        assert!(all_dispatches(tmp.path()).is_ok());
    }

    #[test]
    fn a_project_with_no_registry_file_has_no_dispatches() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            dispatches_in(tmp.path(), "never-dispatched")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn project_names_is_empty_for_a_mana_home_that_never_ran() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(project_names(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn all_dispatches_spans_every_project_in_name_order() {
        let tmp = tempfile::tempdir().unwrap();
        for project in ["zeta", "alpha"] {
            let paths = resolve_project_paths(tmp.path(), project);
            crate::lock::append_record(
                &paths.subagents_file,
                &record(&format!("agent-{project}"), None, "2026-08-15T10:00:00Z"),
            )
            .unwrap();
        }
        // A stray file next to the project directories must not be mistaken
        // for a project.
        std::fs::write(tmp.path().join("projects/stray.txt"), "x").unwrap();

        let projects: Vec<String> = all_dispatches(tmp.path())
            .unwrap()
            .into_iter()
            .map(|d| d.project)
            .collect();
        assert_eq!(projects, ["alpha", "zeta"]);
    }

    #[test]
    fn render_table_aligns_columns_and_leaves_no_trailing_space() {
        let rows = vec![
            vec!["AGENT".into(), "ROLE".into(), "STATUS".into()],
            vec!["3f2a1b6c".into(), "executor".into(), "running".into()],
            vec!["a1".into(), "reviewer".into(), "done".into()],
        ];
        let rendered = render_table(&rows);
        assert_eq!(rendered[0], "AGENT     ROLE      STATUS");
        assert_eq!(rendered[1], "3f2a1b6c  executor  running");
        assert_eq!(rendered[2], "a1        reviewer  done");
        for line in &rendered {
            assert_eq!(line.trim_end(), *line, "trailing whitespace in {line:?}");
        }
    }

    #[test]
    fn render_table_tolerates_a_short_row() {
        let rendered = render_table(&[
            vec!["a".into(), "bb".into(), "ccc".into()],
            vec!["dddd".into()],
        ]);
        assert_eq!(rendered[1], "dddd");
    }
}

#[cfg(all(test, unix))] // every test here spawns and inspects real processes
mod process_tests {
    use super::tests::record;
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// A real process that outlives the call under test, killed on drop so a
    /// failing assertion cannot leak a sleeper into the test runner's session.
    struct Sleeper(Child);

    impl Sleeper {
        /// `own_group` mirrors what `crate::spawn` does for every sub-agent.
        /// A child spawned without it stays in the test runner's process
        /// group, which is exactly the shape `guard`'s first check rejects.
        fn new(own_group: bool) -> Sleeper {
            let mut command = Command::new("sleep");
            command
                .arg("30")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if own_group {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            Sleeper(command.spawn().unwrap())
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A pid that is definitely free: spawn something trivial, reap it, and
    /// hand back the pid the kernel just released.
    fn reaped_pid() -> u32 {
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn probe_sees_a_live_process_and_a_reaped_one() {
        let sleeper = Sleeper::new(true);
        assert_eq!(probe(sleeper.pid()), Liveness::Alive);
        assert_eq!(probe(reaped_pid()), Liveness::Dead);
    }

    #[test]
    fn a_live_pid_with_no_exit_record_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let sleeper = Sleeper::new(true);
        let (status, liveness, finished_at) = classify(
            &record("agent-1", Some(sleeper.pid()), &crate::log::now_iso8601()),
            &tmp.path().join("logs"),
        )
        .unwrap();
        assert_eq!(status, DispatchStatus::Running);
        assert_eq!(liveness, Liveness::Alive);
        assert!(finished_at.is_none());
    }

    /// The crash leftover: mana wrote the record, the process died without
    /// ever reaching its exit log. Nothing will finish it, and nothing else
    /// in mana would notice.
    #[test]
    fn a_dead_pid_with_no_exit_record_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let (status, liveness, _) = classify(
            &record("agent-1", Some(reaped_pid()), &crate::log::now_iso8601()),
            &tmp.path().join("logs"),
        )
        .unwrap();
        assert_eq!(status, DispatchStatus::Stale);
        assert_eq!(liveness, Liveness::Dead);
    }

    #[test]
    fn guard_accepts_a_fresh_group_leader() {
        let sleeper = Sleeper::new(true);
        assert_eq!(
            guard(sleeper.pid(), &crate::log::now_iso8601(), Utc::now()),
            Guard::Ours
        );
    }

    #[test]
    fn guard_refuses_a_pid_that_does_not_lead_its_own_group() {
        // Same shape a recycled pid usually takes: a perfectly real process
        // that mana never spawned, and therefore not a group leader.
        let sleeper = Sleeper::new(false);
        let verdict = guard(sleeper.pid(), &crate::log::now_iso8601(), Utc::now());
        match verdict {
            Guard::NotOurs(reason) => {
                assert!(reason.contains("process group"), "{reason}");
                assert!(reason.contains("recycled"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The other half of pid reuse: the pid *is* a group leader (a shell, a
    /// daemon), but it cannot be ours because it started long after the
    /// dispatch did.
    ///
    /// The refusal only exists when the age lookup answered, and `elapsed_secs`
    /// shells out to `ps`, which can fail transiently — that is exactly what
    /// happened on CI run 31952060252, where `guard` fell into its "the age
    /// check could not run" branch and returned `Ours`. Asserting a refusal
    /// unconditionally was therefore asserting that `ps` always works. When it
    /// does not, this test has nothing to conclude and says so rather than
    /// failing; `recycled_by_age`'s own test covers the decision itself on
    /// every host, `ps` or no `ps`.
    #[test]
    fn guard_refuses_a_group_leader_younger_than_the_record() {
        let sleeper = Sleeper::new(true);
        let long_ago = (Utc::now() - TimeDelta::hours(3)).to_rfc3339();
        match guard(sleeper.pid(), &long_ago, Utc::now()) {
            Guard::NotOurs(reason) => assert!(reason.contains("recycled"), "{reason}"),
            Guard::Ours if elapsed_secs(sleeper.pid()).is_none() => {
                eprintln!(
                    "inconclusive: `ps` would not report an age for pid {}, so guard's age \
                     check did not run",
                    sleeper.pid()
                );
            }
            other => panic!(
                "expected a refusal, got {other:?}; the age lookup answered {:?} and the \
                 record parsed as {:?}",
                elapsed_secs(sleeper.pid()),
                parse_started_at(&long_ago),
            ),
        }
    }

    #[test]
    fn guard_tolerates_a_record_dated_a_moment_in_the_future() {
        // A clock stepped forward between the spawn and now must not read as
        // pid reuse: the tolerance exists for exactly this.
        let sleeper = Sleeper::new(true);
        let slightly_ahead = (Utc::now() + TimeDelta::seconds(30)).to_rfc3339();
        assert_eq!(
            guard(sleeper.pid(), &slightly_ahead, Utc::now()),
            Guard::Ours
        );
    }

    #[test]
    fn guard_cannot_judge_a_pid_that_is_gone() {
        match guard(reaped_pid(), &crate::log::now_iso8601(), Utc::now()) {
            Guard::Unverified(reason) => assert!(reason.contains("process group"), "{reason}"),
            other => panic!("expected 'unverified', got {other:?}"),
        }
    }

    #[test]
    fn parse_etime_reads_every_posix_shape() {
        assert_eq!(parse_etime("05"), None); // seconds alone is not a shape ps prints
        assert_eq!(parse_etime("00:12"), Some(12));
        assert_eq!(parse_etime("01:30"), Some(90));
        assert_eq!(parse_etime("02:01:30"), Some(7_290));
        assert_eq!(parse_etime("3-02:01:30"), Some(266_490));
        assert_eq!(parse_etime("  01:30  ".trim()), Some(90));
        assert_eq!(parse_etime("not a duration"), None);
    }
}
