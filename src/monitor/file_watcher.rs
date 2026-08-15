use crate::log::{LogEntry, Status, append_log, now_iso8601};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

#[derive(Debug, Clone, PartialEq)]
pub enum FsEvent {
    Changed(PathBuf),
}

pub fn watch(dir: &Path) -> anyhow::Result<(RecommendedWatcher, Receiver<FsEvent>)> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            for path in event.paths {
                let _ = tx.send(FsEvent::Changed(path));
            }
        }
    })?;
    watcher.watch(dir, RecursiveMode::Recursive)?;
    Ok((watcher, rx))
}

/// Directories whose contents are never worth logging as agent activity:
/// VCS internals, build output, and dependency trees. Without this, a
/// single `cargo build` or `npm install` would drown a sub-agent's log in
/// thousands of irrelevant `file:modified:` lines.
const IGNORED_DIR_NAMES: &[&str] = &[".git", "target", "node_modules"];

fn is_noise_path(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, Component::Normal(name) if IGNORED_DIR_NAMES.iter().any(|ignored| name == *ignored))
    })
}

/// Drains `fs_events` on a background thread and appends a `file:modified:`
/// entry to `log_path` for each one, relative to `project_root`. Used to
/// give a sub-agent's log the file-change visibility the log format
/// documents (`file:modified:src/main.rs`), which the agent itself never
/// reports — `mana` observes it independently via the file watcher.
pub fn spawn_logger(
    fs_events: Receiver<FsEvent>,
    project_root: PathBuf,
    log_path: PathBuf,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(FsEvent::Changed(path)) = fs_events.recv() {
            if is_noise_path(&path) {
                continue;
            }
            let relative = path.strip_prefix(&project_root).unwrap_or(&path);
            let _ = append_log(
                &log_path,
                &LogEntry {
                    status: Status::Running,
                    action: format!("file:modified:{}", relative.display()),
                    timestamp: now_iso8601(),
                },
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::read_last_status;
    use std::time::Duration;

    /// Canonical tempdir root in the same shape notify reports events with.
    /// macOS needs canonicalize (`/var` symlinks to `/private/var`); Windows'
    /// canonicalize returns a verbatim `\\?\C:\...` path while notify emits
    /// plain `C:\...`, so the prefix must go or strip_prefix never matches.
    fn watchable_root(tmp: &Path) -> PathBuf {
        let canon = tmp.canonicalize().unwrap();
        #[cfg(windows)]
        {
            let s = canon.display().to_string();
            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(stripped);
            }
        }
        canon
    }

    #[test]
    fn is_noise_path_flags_git_target_and_node_modules() {
        assert!(is_noise_path(Path::new("/proj/.git/HEAD")));
        assert!(is_noise_path(Path::new("/proj/target/debug/mana")));
        assert!(is_noise_path(Path::new("/proj/node_modules/pkg/index.js")));
    }

    #[test]
    fn is_noise_path_allows_regular_source_files() {
        assert!(!is_noise_path(Path::new("/proj/src/main.rs")));
    }

    #[test]
    fn spawn_logger_appends_relative_file_modified_entry() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize: on macOS the tempdir path (`/var/...`) is a symlink
        // to `/private/var/...`, and notify/FSEvents reports paths already
        // resolved through it — stripping the unresolved prefix would fail
        // and silently fall back to the absolute path.
        let root = watchable_root(tmp.path());
        // Create the subdirectory BEFORE watching: on Linux (inotify),
        // recursive watching is emulated by notify adding a watch per
        // subdirectory, and a directory created after the watch started
        // races that registration — a file written into it immediately
        // can be missed entirely. macOS's FSEvents backend has no such
        // race, which is why this only ever failed in CI on ubuntu-latest.
        std::fs::create_dir_all(root.join("src")).unwrap();
        let (_watcher, rx) = watch(&root).unwrap();
        // Log file lives in a sibling tempdir, outside what's being
        // watched, so its own creation/write never shows up as noise.
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("agent.jsonl");
        let _handle = spawn_logger(rx, root.clone(), log_path.clone());

        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                // Diagnostic panic: two blind CI round-trips already went into
                // guessing what Windows' notify backend emits — dump what the
                // logger actually wrote (or that it wrote nothing) instead.
                let seen = std::fs::read_to_string(&log_path)
                    .unwrap_or_else(|_| "<log file never created>".to_string());
                panic!(
                    "timeout waiting for file:modified entry\nroot: {}\nlog contents:\n{seen}",
                    root.display()
                );
            }
            if log_path.exists() {
                let contents = std::fs::read_to_string(&log_path).unwrap();
                // Built with the platform separator: Windows logs `src\main.rs`,
                // and a hardcoded forward slash made this time out there.
                let expected = format!(
                    "file:modified:{}",
                    Path::new("src").join("main.rs").display()
                );
                if contents.contains(&expected) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(read_last_status(&log_path).unwrap(), Some(Status::Running));
    }

    #[test]
    fn spawn_logger_ignores_git_and_target_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = watchable_root(tmp.path());
        let (_watcher, rx) = watch(&root).unwrap();
        // Log file lives in a sibling tempdir, outside what's being
        // watched, so its own creation/write never shows up as noise.
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("agent.jsonl");
        let _handle = spawn_logger(rx, root.clone(), log_path.clone());

        std::thread::sleep(Duration::from_millis(200));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::write(root.join("target/debug/mana"), "binary").unwrap();
        // A real source file after the noise, so we have something concrete
        // to wait for instead of asserting on a fixed sleep.
        std::fs::write(root.join("real.rs"), "fn main() {}").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for file:modified log entry");
            }
            if log_path.exists() {
                let contents = std::fs::read_to_string(&log_path).unwrap();
                if contents.contains("file:modified:real.rs") {
                    assert!(!contents.contains(".git"));
                    assert!(!contents.contains("target"));
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn detects_new_file_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let (_watcher, rx) = watch(tmp.path()).unwrap();

        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(tmp.path().join("subagent-lock.yaml"), "agent-1: {}\n").unwrap();

        // Poll for the file event; may receive directory event first on some platforms
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timeout waiting for file event");
            }
            let event = rx.recv_timeout(remaining).expect("expected a file event");
            match event {
                FsEvent::Changed(path) => {
                    if path.ends_with("subagent-lock.yaml") {
                        break;
                    }
                }
            }
        }
    }
}
