//! One mana session per project, enforced at launch.
//!
//! Nothing used to stop a second `mana launch` in a second terminal on the
//! same project, and everything mana keeps for a project is shared by name —
//! so the second session was not a second workspace, it was a second writer
//! into the first one's state:
//!
//! - the `[subagent].max_concurrent` limit is counted in the MCP server's own
//!   memory, and a second server has a second counter: a CLI catalogued at one
//!   dispatch at a time ran two side by side (#35);
//! - the quit-time sweep kills every dispatch of the project whose derived
//!   status is `running`, with no notion of who started it: quitting one
//!   session stopped the other's sub-agents mid-run (#42).
//!
//! Both have the same shape — per-session state, project-wide effects — and
//! both are answered by refusing the second session rather than by teaching
//! every piece of state who owns it. That is the smaller change, and it is
//! also the honest one: mana's model is one PM per project, and two PMs on one
//! project would still contend for the same worktrees and the same registry
//! even if the counters and the sweep were fixed.
//!
//! The claim is a file, created with `create_new` so the create itself is the
//! race-free part, holding the pid that owns it. A lock left behind by a
//! session that crashed is taken over rather than obeyed: the pid is probed
//! the same way `mana ps` probes a dispatch, and a dead holder's file is
//! removed. That is the only lock ever removed: a holder mana cannot decide
//! about, and a lock mana cannot read at all, are both treated as live, and
//! the refusal names the file so the operator can settle it.
//!
//! ponytail: `mana dev` and a hand-started `mana mcp-server` take no lock —
//! they are the developer paths that drive the pipeline by hand, and locking
//! them would mean refusing them while a session they were started to
//! inspect is open.

use crate::project::ProjectPaths;
use crate::status::{Liveness, probe, render_age};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Named for what it is rather than for what it holds: a session, not a
/// directory. Lives beside `state.toml` in the project's mana state.
const LOCK_FILE: &str = "session.lock";

/// Who holds the lock. Written once and never revised — a session that ends
/// deletes the file rather than editing it.
#[derive(Serialize, Deserialize, Debug)]
struct Holder {
    pid: u32,
    started_at: String,
}

/// The claim, released when it drops. `mana launch` binds it for the whole
/// session, so every way out — a clean quit, a `?`, a panic unwinding through
/// the render loop — releases it, and the next launch starts cleanly.
#[derive(Debug)]
pub struct SessionLock {
    path: PathBuf,
    pid: u32,
}

/// Claims this project for the calling process, or explains who has it.
pub fn acquire(paths: &ProjectPaths, now: DateTime<Utc>) -> Result<SessionLock> {
    crate::project::create_dir_all(&paths.root)
        .with_context(|| format!("creating {}", paths.root.display()))?;
    let path = paths.root.join(LOCK_FILE);
    let pid = std::process::id();

    // Twice at most. The second attempt happens only after this process has
    // cleared a lock whose holder is gone, and if it loses that create to
    // somebody else, that somebody is a live session — a refusal, not a
    // reason to keep spinning.
    for _ in 0..2 {
        match crate::project::create_new(&path) {
            Ok(mut file) => {
                let holder = Holder {
                    pid,
                    started_at: now.to_rfc3339(),
                };
                file.write_all(serde_json::to_string(&holder)?.as_bytes())
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(SessionLock { path, pid });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if let Some(reason) = refusal(&path, now) {
                    bail!(reason);
                }
                // Stale: a holder record that parsed, naming a pid that is
                // gone. Removing it is what makes a crashed session
                // recoverable without the operator being told to delete a
                // file. It is the only lock ever removed -- see `refusal`.
                let _ = std::fs::remove_file(&path);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!(
        "another mana session claimed this project while this one was starting -- \
         run `mana launch` again"
    )
}

/// Why the existing lock stands, or `None` when it does not.
fn refusal(path: &Path, now: DateTime<Utc>) -> Option<String> {
    let Some(holder) = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Holder>(&text).ok())
    else {
        // A lock mana cannot read used to mean "no session can be behind
        // this", and the caller deleted it. But the claim is created and
        // written in two steps, so that is exactly what a claim being made
        // right now looks like from the outside -- zero bytes -- and a second
        // launch reading it there deleted the winner's file and took the
        // project: two PMs on one registry (#200, reviving #35 and #42).
        // Unreadable is live until an operator says otherwise. The wrong
        // guess this way costs one stuck project and names the file to
        // unstick it; the wrong guess the other way costs the guarantee the
        // lock exists for.
        return Some(format!(
            "this project has a mana session lock mana cannot read ({}). Either a session is \
             claiming the project right now -- run `mana launch` again -- or one died while \
             claiming it, in which case delete that file.",
            path.display()
        ));
    };
    let liveness = probe(holder.pid);
    if liveness == Liveness::Dead {
        return None;
    }
    let age = chrono::DateTime::parse_from_rfc3339(&holder.started_at)
        .ok()
        .map(|started| now - started.with_timezone(&Utc));
    // Every clause is something the operator has to know to act: which process
    // to look for, why a second session is not simply slower, and what to do
    // when the process it names is not there any more.
    Some(format!(
        "this project already has a mana session{} (pid {}, started {} ago). A second one \
         would not get its own workspace: the two share this project's registry, its \
         worktrees, its notifications, the concurrency limit each counts in its own memory \
         (#35), and the sweep that stops every running dispatch on quit -- so quitting \
         either would kill the other's sub-agents (#42). Quit that session first. If it is \
         gone, delete {}.",
        match liveness {
            Liveness::Unknown => ", or had one mana cannot ask about",
            _ => "",
        },
        holder.pid,
        render_age(age),
        path.display()
    ))
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Only if it is still ours. An operator who deleted the file to clear
        // what looked like a stale lock may already have started the session
        // that now holds it, and deleting *that* claim would hand the project
        // to a third one.
        let mine = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Holder>(&text).ok())
            .is_some_and(|holder| holder.pid == self.pid);
        if mine {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::resolve_project_paths;

    fn paths(home: &Path) -> ProjectPaths {
        resolve_project_paths(home, "demo")
    }

    #[test]
    fn a_first_session_gets_the_lock_and_releases_it_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let lock = acquire(&paths, Utc::now()).unwrap();
        assert!(paths.root.join(LOCK_FILE).exists());
        drop(lock);
        assert!(!paths.root.join(LOCK_FILE).exists());
    }

    /// #42 and #35 in one assertion: the second session never starts, so
    /// there is no second counter and no second sweep.
    #[test]
    fn a_second_session_on_the_same_project_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let _held = acquire(&paths, Utc::now()).unwrap();
        let error = acquire(&paths, Utc::now()).unwrap_err().to_string();
        assert!(error.contains("already has a mana session"), "{error}");
        // The two things the operator needs: which process, and how to clear
        // it if that process is gone.
        assert!(error.contains(&std::process::id().to_string()), "{error}");
        assert!(error.contains(LOCK_FILE), "{error}");
    }

    /// Another project is another lock -- the refusal is per project, not per
    /// machine.
    #[test]
    fn a_second_project_is_unaffected() {
        let tmp = tempfile::tempdir().unwrap();
        let _held = acquire(&paths(tmp.path()), Utc::now()).unwrap();
        acquire(&resolve_project_paths(tmp.path(), "other"), Utc::now()).unwrap();
    }

    /// The crash case: a lock whose holder no longer exists is taken over,
    /// because the alternative is a project no `mana launch` can ever open
    /// again without the operator deleting a file.
    #[test]
    fn a_lock_held_by_a_dead_pid_is_taken_over() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        crate::project::ensure_project_structure(&paths).unwrap();
        // A pid this test can be sure is not running: mana's own reaped
        // child. `false` exits immediately and is waited on here.
        let dead = std::process::Command::new(if cfg!(windows) { "cmd" } else { "false" })
            .args(if cfg!(windows) {
                vec!["/C", "exit", "1"]
            } else {
                vec![]
            })
            .spawn()
            .unwrap();
        let pid = dead.id();
        let mut dead = dead;
        dead.wait().unwrap();
        crate::project::write(
            &paths.root.join(LOCK_FILE),
            serde_json::to_string(&Holder {
                pid,
                started_at: Utc::now().to_rfc3339(),
            })
            .unwrap(),
        )
        .unwrap();
        // Skipped rather than failed where the pid may have been recycled
        // onto a live process between the wait and the probe.
        if probe(pid) == Liveness::Dead {
            acquire(&paths, Utc::now()).unwrap();
        }
    }

    /// A lock mana cannot read is not a lock mana can clear (#200): the
    /// unreadable state is what a claim in progress looks like from the
    /// outside, so clearing it is how a second session used to take a live
    /// project.
    #[test]
    fn an_unreadable_lock_is_refused_rather_than_stolen() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        crate::project::ensure_project_structure(&paths).unwrap();
        let path = paths.root.join(LOCK_FILE);
        crate::project::write(&path, "not json").unwrap();
        let error = acquire(&paths, Utc::now()).unwrap_err().to_string();
        assert!(error.contains(LOCK_FILE), "{error}");
        assert!(path.exists(), "the lock mana could not read was deleted");
    }

    /// Exactly the window in the middle of a claim: `create_new` has
    /// published the name and the holder record is not written yet, so the
    /// file another launch reads is zero bytes (#200).
    #[test]
    fn a_zero_byte_lock_is_refused_rather_than_stolen() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        crate::project::ensure_project_structure(&paths).unwrap();
        let path = paths.root.join(LOCK_FILE);
        crate::project::write(&path, "").unwrap();
        let error = acquire(&paths, Utc::now()).unwrap_err().to_string();
        assert!(error.contains(LOCK_FILE), "{error}");
        assert!(path.exists(), "the lock mana could not read was deleted");
    }

    /// The same window one byte later: a record half on disk parses no
    /// better than none of it, and means no less that a session is there.
    #[test]
    fn a_truncated_lock_is_refused_rather_than_stolen() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        crate::project::ensure_project_structure(&paths).unwrap();
        let path = paths.root.join(LOCK_FILE);
        crate::project::write(&path, "{\"pid\":4242,\"started_").unwrap();
        let error = acquire(&paths, Utc::now()).unwrap_err().to_string();
        assert!(error.contains(LOCK_FILE), "{error}");
        assert!(path.exists(), "the lock mana could not read was deleted");
    }

    /// The lock the session did not take is the lock it must not delete.
    #[test]
    fn dropping_a_lock_leaves_somebody_elses_claim_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let lock = acquire(&paths, Utc::now()).unwrap();
        let path = paths.root.join(LOCK_FILE);
        crate::project::write(
            &path,
            serde_json::to_string(&Holder {
                pid: std::process::id() + 1,
                started_at: Utc::now().to_rfc3339(),
            })
            .unwrap(),
        )
        .unwrap();
        drop(lock);
        assert!(path.exists());
    }
}
