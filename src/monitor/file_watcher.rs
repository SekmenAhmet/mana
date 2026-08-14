use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
