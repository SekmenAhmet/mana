use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub status: Status,
    pub action: String,
    pub timestamp: String,
}

pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn append_log(path: &Path, entry: &LogEntry) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read_last_status(path: &Path) -> anyhow::Result<Option<Status>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    let last_line = contents.lines().filter(|l| !l.trim().is_empty()).last();
    match last_line {
        None => Ok(None),
        Some(line) => {
            let entry: LogEntry = serde_json::from_str(line)?;
            Ok(Some(entry.status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_last_status_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logs/agent.jsonl");
        assert_eq!(read_last_status(&path).unwrap(), None);
    }

    #[test]
    fn append_then_read_last_status() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logs/agent.jsonl");
        append_log(
            &path,
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        append_log(
            &path,
            &LogEntry {
                status: Status::Running,
                action: "cmd:cargo test".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        assert_eq!(read_last_status(&path).unwrap(), Some(Status::Running));
        append_log(
            &path,
            &LogEntry {
                status: Status::Done,
                action: "exited".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        assert_eq!(read_last_status(&path).unwrap(), Some(Status::Done));
    }

    #[test]
    fn each_line_is_independent_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.jsonl");
        append_log(
            &path,
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: "2026-08-13T19:00:00Z".into(),
            },
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(serde_json::from_str::<LogEntry>(lines[0]).is_ok());
    }
}
