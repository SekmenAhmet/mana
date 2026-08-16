//! The two files a background dispatch leaves behind.
//!
//! An MCP tool call has to return in milliseconds while an executor runs for
//! minutes, so `launch_subagent` answers immediately and the dispatch finishes
//! on a thread nobody is waiting on. That split needs somewhere to put what the
//! thread learned, and mana's rule is that state lives in files under
//! `~/.mana/projects/<project>/` -- the MCP server itself holds nothing across
//! calls, so it can be restarted mid-session without losing a thing.
//!
//! - `runs/<task_id>.json` -- one record, rewritten by each executor: where its
//!   worktree is and what came of it. This is the executor → reviewer handoff.
//!   Recomputing it instead (branch name + path convention + `git merge-base`)
//!   would be a second source of truth for facts the executor already knew.
//! - `notifications.jsonl` -- append-only, one line per finished dispatch.
//!   The PM driver (task 2.3) tails this file and injects `[mana] ...` messages
//!   into the PM session, which is how the PM learns an executor finished
//!   without polling. Nothing here reads it back; that consumer does not exist
//!   yet, and the file has to exist before it can.

use crate::project::ProjectPaths;
use crate::task::Role;
use crate::worktree::WorktreeInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What an executor left for the reviewer that comes after it.
///
/// Written on completion, success or failure alike: a reviewer dispatched on a
/// failed executor must be refused with a reason, and that reason is in here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub task_id: String,
    /// The id the PM was handed by `launch_subagent`.
    pub agent_id: String,
    pub cli: String,
    pub model: String,
    pub worktree: PathBuf,
    pub branch: String,
    /// The commit the worktree started from -- what the reviewer diffs against.
    /// Captured at creation and never recomputed, because the project's own
    /// HEAD moves while the executor works.
    pub base_ref: String,
    pub succeeded: bool,
    /// One human-readable line: exit code or timeout, duration, and the
    /// matched failure signature if there was one.
    pub outcome: String,
    pub finished_at: String,
}

impl RunRecord {
    /// The worktree the reviewer runs in, rebuilt from what the executor wrote.
    pub fn worktree_info(&self) -> WorktreeInfo {
        WorktreeInfo {
            path: self.worktree.clone(),
            branch: self.branch.clone(),
            base_ref: self.base_ref.clone(),
        }
    }
}

/// One finished dispatch, as the PM driver will read it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Notification {
    pub ts: String,
    pub task_id: String,
    pub role: Role,
    pub agent_id: String,
    pub outcome: String,
}

pub fn runs_dir(paths: &ProjectPaths) -> PathBuf {
    paths.root.join("runs")
}

pub fn run_path(paths: &ProjectPaths, task_id: &str) -> PathBuf {
    runs_dir(paths).join(format!("{task_id}.json"))
}

pub fn notifications_path(paths: &ProjectPaths) -> PathBuf {
    paths.root.join("notifications.jsonl")
}

/// Overwrites the task's run record. A retried task has one current executor,
/// so the newest run is the only one a reviewer could be reviewing; keeping a
/// history here would just be a second, staler copy of `subagents.jsonl`.
pub fn write_run(paths: &ProjectPaths, record: &RunRecord) -> Result<()> {
    let path = run_path(paths, &record.task_id);
    let dir = runs_dir(paths);
    crate::project::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let line = serde_json::to_string(record)?;
    crate::project::write(&path, line).with_context(|| format!("writing {}", path.display()))
}

/// `Ok(None)` when no executor has finished this task yet -- the ordinary case
/// for a reviewer dispatched too early, and not an error at this level.
pub fn read_run(paths: &ProjectPaths, task_id: &str) -> Result<Option<RunRecord>> {
    let path = run_path(paths, task_id);
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let record = serde_json::from_str(&contents)
        .with_context(|| format!("invalid run record {}", path.display()))?;
    Ok(Some(record))
}

/// Appends one line and nothing else -- same reasoning as `lock::append_record`:
/// several dispatch threads finish independently, and an append-only file has
/// no read-modify-write window for them to race in. The window they *did* race
/// in was inside the write itself, which is why this goes through
/// `log::append_line` rather than opening the file here — see that function
/// for what makes a single append atomic and what it rests on.
///
/// Says so on stderr as well as returning the error, and that is not
/// belt-and-braces: the caller this file exists for cannot use the `Result`.
/// A dispatch is announced from a thread whose tool call returned minutes ago
/// (`mcp::Dispatch::run`), so there is nobody left to hand a failure to and it
/// drops it — which made *this* append, the one channel that tells the PM a
/// dispatch finished, the only failure in the module mana never mentioned
/// (#72). An executor that ran to completion and was billed would leave the
/// session waiting for a completion that had already happened, with nothing
/// anywhere to say why. Reporting once, here, covers every caller instead of
/// the one the bug was filed against.
///
/// stderr because everything else under `~/.mana/projects/<p>/` shares this
/// file's fate: whatever stopped the append stopped the logs too. On `mana
/// mcp-server` it is the host CLI that keeps that stream, which is where an
/// operator hunting a completion that never arrived ends up. Same convention
/// as `status::dispatches_in`: said out loud exactly once, on stderr, so
/// nothing on stdout changes shape.
pub fn notify(paths: &ProjectPaths, notification: &Notification) -> Result<()> {
    let appended = crate::log::append_line(
        &notifications_path(paths),
        &serde_json::to_string(notification)?,
    );
    if let Err(error) = &appended {
        eprintln!(
            "warning: mana could not announce that {} finished for task {} ({error:#}) -- the \
             PM will never be told, so it may still be waiting on work that is already done",
            notification.agent_id, notification.task_id
        );
    }
    appended
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{ensure_project_structure, resolve_project_paths};

    fn record(task_id: &str, succeeded: bool) -> RunRecord {
        RunRecord {
            task_id: task_id.to_string(),
            agent_id: "agent-1".to_string(),
            cli: "fixture".to_string(),
            model: "cheapo".to_string(),
            worktree: PathBuf::from("/tmp/wt/3f2a1b6c"),
            branch: format!("mana/{task_id}"),
            base_ref: "abc123".to_string(),
            succeeded,
            outcome: "exit 0 in 12.3s".to_string(),
            finished_at: "2026-08-15T10:00:00Z".to_string(),
        }
    }

    fn fixture() -> (tempfile::TempDir, ProjectPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        ensure_project_structure(&paths).unwrap();
        (tmp, paths)
    }

    #[test]
    fn a_run_round_trips_and_rebuilds_the_reviewers_worktree() {
        let (_tmp, paths) = fixture();
        write_run(&paths, &record("task-1", true)).unwrap();

        let read_back = read_run(&paths, "task-1").unwrap().unwrap();
        assert_eq!(read_back, record("task-1", true));

        let worktree = read_back.worktree_info();
        assert_eq!(worktree.path, PathBuf::from("/tmp/wt/3f2a1b6c"));
        assert_eq!(worktree.branch, "mana/task-1");
        assert_eq!(worktree.base_ref, "abc123");
    }

    #[test]
    fn a_task_with_no_finished_executor_reads_as_none() {
        let (_tmp, paths) = fixture();
        assert!(read_run(&paths, "task-1").unwrap().is_none());
    }

    #[test]
    fn a_retry_replaces_the_previous_run_rather_than_appending() {
        let (_tmp, paths) = fixture();
        write_run(&paths, &record("task-1", false)).unwrap();
        write_run(&paths, &record("task-1", true)).unwrap();
        assert!(read_run(&paths, "task-1").unwrap().unwrap().succeeded);
    }

    #[test]
    fn a_corrupt_run_record_names_its_file() {
        let (_tmp, paths) = fixture();
        std::fs::create_dir_all(runs_dir(&paths)).unwrap();
        std::fs::write(run_path(&paths, "task-1"), "half a json").unwrap();
        let error = format!("{:#}", read_run(&paths, "task-1").unwrap_err());
        assert!(error.contains("task-1.json"), "{error}");
    }

    #[test]
    fn notifications_accumulate_one_json_line_per_dispatch() {
        let (_tmp, paths) = fixture();
        for (role, agent) in [(Role::Executor, "agent-1"), (Role::Reviewer, "agent-2")] {
            notify(
                &paths,
                &Notification {
                    ts: "2026-08-15T10:00:00Z".to_string(),
                    task_id: "task-1".to_string(),
                    role,
                    agent_id: agent.to_string(),
                    outcome: "exit 0 in 1.0s".to_string(),
                },
            )
            .unwrap();
        }

        let contents = std::fs::read_to_string(notifications_path(&paths)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "{contents}");
        // The field names are the contract with the PM driver (task 2.3), so
        // assert the raw line, not just that it deserializes.
        assert!(lines[0].contains(r#""role":"executor""#), "{contents}");
        assert!(lines[1].contains(r#""role":"reviewer""#), "{contents}");
        for line in lines {
            let notification: Notification = serde_json::from_str(line).unwrap();
            assert_eq!(notification.task_id, "task-1");
        }
    }

    /// The failure `mcp::Dispatch::run` cannot report (#72): it drops this
    /// `Result`, so the error has to carry enough to be actionable wherever it
    /// is finally read, and the warning has to name the dispatch nobody will
    /// hear about. A directory in the file's place rather than a read-only
    /// parent, because the tests run as root often enough for a `chmod` to
    /// prove nothing.
    #[test]
    fn a_completion_that_cannot_be_appended_names_the_file_it_was_lost_in() {
        let (_tmp, paths) = fixture();
        std::fs::create_dir_all(notifications_path(&paths)).unwrap();

        let error = format!(
            "{:#}",
            notify(
                &paths,
                &Notification {
                    ts: "2026-08-15T10:00:00Z".to_string(),
                    task_id: "task-1".to_string(),
                    role: Role::Executor,
                    agent_id: "agent-1".to_string(),
                    outcome: "exit 0 in 1.0s".to_string(),
                },
            )
            .unwrap_err()
        );
        assert!(error.contains("notifications.jsonl"), "{error}");
    }
}
