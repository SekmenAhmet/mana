//! `mana ps` — what mana has dispatched, and what became of it.
//!
//! One row per record in `subagents.jsonl`, with the status derived by
//! `crate::status` rather than stored: the registry is append-only and says
//! only what was *started*. See that module for what `running`, `done`,
//! `stale` and `unknown` are each worth.
//!
//! Exit code is always 0, including when a stale dispatch is listed. `ps` is a
//! listing, and a listing that fails is one no script can pipe; `mana doctor`
//! is the command whose exit code is a verdict.

use crate::project::{mana_home, project_name_from_dir};
use crate::status::{self, Dispatch, DispatchStatus, Liveness};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::Path;

/// Which projects a listing covers.
pub enum Scope {
    /// Every project under `<mana_home>/projects`. Adds a PROJECT column,
    /// because two rows from two projects are otherwise indistinguishable.
    All,
    One(String),
}

pub fn run(all: bool, project: Option<&Path>) -> Result<()> {
    let home = mana_home()?;
    let scope = if all {
        Scope::All
    } else {
        Scope::One(resolve_project_name(project)?)
    };
    for line in report(&home, &scope, Utc::now())? {
        println!("{line}");
    }
    Ok(())
}

/// The project a command with no `--all` acts on: the directory named by
/// `--project`, else the working directory. Only the basename matters —
/// that is how mana has always keyed project state (`project_name_from_dir`),
/// and `mana doctor` prints the resolved path so the aliasing is visible.
pub fn resolve_project_name(project: Option<&Path>) -> Result<String> {
    let dir = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    Ok(project_name_from_dir(&dir))
}

/// The rendered listing, as lines. Separated from `run` so the table can be
/// asserted on without capturing stdout.
pub fn report(mana_home: &Path, scope: &Scope, now: DateTime<Utc>) -> Result<Vec<String>> {
    let dispatches = match scope {
        Scope::All => status::all_dispatches(mana_home)?,
        Scope::One(project) => status::dispatches_in(mana_home, project)?,
    };
    if dispatches.is_empty() {
        return Ok(vec![empty_message(mana_home, scope)]);
    }
    Ok(table(&dispatches, matches!(scope, Scope::All), now))
}

/// The aligned listing itself, plus whatever notes the rows need. Public
/// because `mana doctor` shows the same rows for the dispatches that are still
/// in flight or stale — one table, one set of columns, one place where
/// "unknown" is explained.
pub fn table(dispatches: &[Dispatch], with_project: bool, now: DateTime<Utc>) -> Vec<String> {
    let mut rows = vec![header(with_project)];
    rows.extend(dispatches.iter().map(|d| row(d, with_project, now)));
    let mut lines = status::render_table(&rows);
    lines.extend(notes(dispatches));
    lines
}

fn empty_message(mana_home: &Path, scope: &Scope) -> String {
    match scope {
        Scope::All => format!(
            "no dispatches recorded under {}",
            mana_home.join("projects").display()
        ),
        Scope::One(project) => format!(
            "no dispatches recorded for project '{project}' ({})",
            crate::project::resolve_project_paths(mana_home, project)
                .subagents_file
                .display()
        ),
    }
}

fn header(with_project: bool) -> Vec<String> {
    let mut columns: Vec<String> = Vec::new();
    if with_project {
        columns.push("PROJECT".into());
    }
    columns.extend(
        ["AGENT", "ROLE", "CLI/MODEL", "TASK", "PID", "AGE", "STATUS"]
            .iter()
            .map(|c| (*c).to_string()),
    );
    columns
}

fn row(dispatch: &Dispatch, with_project: bool, now: DateTime<Utc>) -> Vec<String> {
    let record = &dispatch.record;
    let mut columns: Vec<String> = Vec::new();
    if with_project {
        columns.push(dispatch.project.clone());
    }
    columns.push(dispatch.short_agent().to_string());
    columns.push(role_word(&record.role).to_string());
    columns.push(format!("{}/{}", record.cli, record.model));
    columns.push(dispatch.short_task().to_string());
    columns.push(match record.pid {
        Some(pid) => pid.to_string(),
        None => "-".to_string(),
    });
    columns.push(status::render_age(dispatch.age(now)));
    columns.push(dispatch.status.word().to_string());
    columns
}

/// Why any row reads `unknown`, said once per reason rather than once per row.
/// Two very different situations wear the same word, and neither is a state a
/// user can act on without being told which one it is.
fn notes(dispatches: &[Dispatch]) -> Vec<String> {
    let unknown: Vec<&Dispatch> = dispatches
        .iter()
        .filter(|d| d.status == DispatchStatus::Unknown)
        .collect();
    let mut notes = Vec::new();
    let pidless = unknown.iter().filter(|d| d.record.pid.is_none()).count();
    if pidless > 0 {
        notes.push(format!(
            "note: {pidless} dispatch(es) were recorded without a pid, so mana cannot tell \
             whether they are still running."
        ));
    }
    let unanswered = unknown
        .iter()
        .filter(|d| d.record.pid.is_some() && d.liveness == Liveness::Unknown)
        .count();
    if unanswered > 0 {
        notes.push(format!(
            "note: this platform gave mana no answer about {unanswered} pid(s); their status \
             is genuinely unknown, not 'done'."
        ));
    }
    notes
}

/// The same word the PM, the notifications and the task frontmatter use.
fn role_word(role: &crate::task::Role) -> &'static str {
    match role {
        crate::task::Role::Executor => "executor",
        crate::task::Role::Reviewer => "reviewer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{SubagentRecord, append_record};
    use crate::log::{ExitEntry, Status, append_log, now_iso8601};
    use crate::project::resolve_project_paths;
    use crate::task::Role;

    fn record(agent_id: &str, role: Role, pid: Option<u32>) -> SubagentRecord {
        SubagentRecord {
            agent_id: format!("{agent_id}-9d4e-4a7b-8c1d-2e5f0a9b8c7d"),
            cli: "claude".into(),
            model: "haiku".into(),
            role,
            task_id: "3f2a1b6c-9d4e-4a7b-8c1d-2e5f0a9b8c7d".into(),
            pid,
            started_at: now_iso8601(),
        }
    }

    fn seed(mana_home: &Path, project: &str, record: &SubagentRecord, finished: bool) {
        let paths = resolve_project_paths(mana_home, project);
        append_record(&paths.subagents_file, record).unwrap();
        if finished {
            append_log(
                &paths.logs.join(format!("{}.jsonl", record.agent_id)),
                &ExitEntry {
                    status: Status::Done,
                    action: "exited".into(),
                    timestamp: now_iso8601(),
                    exit_code: Some(0),
                    duration_ms: Some(1200),
                    input_tokens: None,
                    output_tokens: None,
                    failure_means: None,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn an_empty_project_says_where_it_looked() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = report(tmp.path(), &Scope::One("demo".into()), Utc::now()).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("demo"), "{}", lines[0]);
        assert!(lines[0].contains("subagents.jsonl"), "{}", lines[0]);
    }

    #[test]
    fn a_finished_dispatch_lists_as_done_with_its_columns() {
        let tmp = tempfile::tempdir().unwrap();
        seed(
            tmp.path(),
            "demo",
            &record("aaaaaaaa", Role::Reviewer, Some(4321)),
            true,
        );

        let lines = report(tmp.path(), &Scope::One("demo".into()), Utc::now()).unwrap();
        assert!(lines[0].starts_with("AGENT"), "{}", lines[0]);
        assert!(lines[0].contains("PID"), "{}", lines[0]);
        let row = &lines[1];
        assert!(row.starts_with("aaaaaaaa"), "{row}");
        assert!(row.contains("reviewer"), "{row}");
        assert!(row.contains("claude/haiku"), "{row}");
        assert!(row.contains("3f2a1b6c"), "{row}");
        assert!(row.contains("4321"), "{row}");
        assert!(row.ends_with("done"), "{row}");
        // No PROJECT column without --all: one project needs no disambiguation.
        assert!(!lines[0].contains("PROJECT"), "{}", lines[0]);
    }

    #[test]
    fn a_pidless_unfinished_dispatch_is_unknown_and_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        seed(
            tmp.path(),
            "demo",
            &record("bbbbbbbb", Role::Executor, None),
            false,
        );

        let lines = report(tmp.path(), &Scope::One("demo".into()), Utc::now()).unwrap();
        assert!(lines[1].ends_with("unknown"), "{}", lines[1]);
        assert!(lines[1].contains(" -  "), "{}", lines[1]);
        let note = lines.last().unwrap();
        assert!(note.starts_with("note:"), "{note}");
        assert!(note.contains("without a pid"), "{note}");
    }

    #[test]
    fn all_adds_a_project_column_and_spans_projects() {
        let tmp = tempfile::tempdir().unwrap();
        seed(
            tmp.path(),
            "zeta",
            &record("cccccccc", Role::Executor, None),
            true,
        );
        seed(
            tmp.path(),
            "alpha",
            &record("dddddddd", Role::Executor, None),
            true,
        );

        let lines = report(tmp.path(), &Scope::All, Utc::now()).unwrap();
        assert!(lines[0].starts_with("PROJECT"), "{}", lines[0]);
        assert!(lines[1].starts_with("alpha"), "{}", lines[1]);
        assert!(lines[2].starts_with("zeta"), "{}", lines[2]);
    }

    #[test]
    fn all_on_an_untouched_mana_home_names_the_directory_it_read() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = report(tmp.path(), &Scope::All, Utc::now()).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("projects"), "{}", lines[0]);
    }

    #[test]
    fn rows_stay_aligned_across_differently_sized_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let mut short_task = record("eeeeeeee", Role::Executor, Some(7));
        short_task.task_id = "ab".into();
        seed(tmp.path(), "demo", &short_task, true);
        seed(
            tmp.path(),
            "demo",
            &record("ffffffff", Role::Executor, Some(123456)),
            true,
        );

        let lines = report(tmp.path(), &Scope::One("demo".into()), Utc::now()).unwrap();
        // Every row's STATUS column starts at the same offset -- the whole
        // point of aligning by hand instead of joining with tabs.
        let offset = |line: &str| line.rfind("done").or_else(|| line.rfind("STATUS")).unwrap();
        assert_eq!(offset(&lines[0]), offset(&lines[1]));
        assert_eq!(offset(&lines[1]), offset(&lines[2]));
    }
}

#[cfg(all(test, unix))] // needs a real process to be alive
mod process_tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn a_live_pid_lists_as_running_and_a_dead_one_as_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let sleeper = Sleeper::new();
        seed_pid(tmp.path(), "demo", "aaaaaaaa", sleeper.pid());
        seed_pid(tmp.path(), "demo", "bbbbbbbb", reaped_pid());

        let lines = report(tmp.path(), &Scope::One("demo".into()), Utc::now()).unwrap();
        assert!(lines[1].ends_with("running"), "{}", lines[1]);
        assert!(lines[2].ends_with("stale"), "{}", lines[2]);
    }
}

/// Real-process fixtures, gated with the only tests that consume them: on
/// Windows this module does not exist, so neither does the dead code.
#[cfg(all(test, unix))]
mod tests_support {
    use crate::lock::{SubagentRecord, append_record};
    use crate::log::now_iso8601;
    use crate::project::resolve_project_paths;
    use crate::task::Role;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    pub(super) struct Sleeper(Child);

    impl Sleeper {
        pub(super) fn new() -> Sleeper {
            Sleeper(
                Command::new("sleep")
                    .arg("30")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }

        pub(super) fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub(super) fn reaped_pid() -> u32 {
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    pub(super) fn seed_pid(mana_home: &Path, project: &str, agent: &str, pid: u32) {
        let paths = resolve_project_paths(mana_home, project);
        append_record(
            &paths.subagents_file,
            &SubagentRecord {
                agent_id: format!("{agent}-9d4e-4a7b-8c1d-2e5f0a9b8c7d"),
                cli: "claude".into(),
                model: "haiku".into(),
                role: Role::Executor,
                task_id: "3f2a1b6c-9d4e-4a7b-8c1d-2e5f0a9b8c7d".into(),
                pid: Some(pid),
                started_at: now_iso8601(),
            },
        )
        .unwrap();
    }
}
