use crate::log::{LogEntry, Status, append_log, now_iso8601};
use portable_pty::Child;
use std::path::Path;

pub fn watch_and_log(
    mut child: Box<dyn Child + Send + Sync>,
    log_path: &Path,
) -> anyhow::Result<()> {
    let exit_status = child.wait()?;
    let action = if exit_status.success() {
        "exited".to_string()
    } else {
        format!("exited(code={})", exit_status.exit_code())
    };
    append_log(
        log_path,
        &LogEntry {
            status: Status::Done,
            action,
            timestamp: now_iso8601(),
        },
    )?;
    Ok(())
}

#[cfg(all(test, unix))] // every test here execs unix shell fixtures
mod tests {
    use super::*;
    use crate::log::read_last_status;
    use crate::pty;

    #[cfg(unix)]
    #[test]
    fn watch_and_log_appends_done_after_process_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.jsonl");
        let session = pty::spawn("sh", &["-c".to_string(), "exit 0".to_string()]).unwrap();
        watch_and_log(session.child, &log_path).unwrap();
        assert_eq!(read_last_status(&log_path).unwrap(), Some(Status::Done));
    }

    #[cfg(unix)]
    #[test]
    fn watch_and_log_records_nonzero_exit_code_in_action() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.jsonl");
        let session = pty::spawn("sh", &["-c".to_string(), "exit 3".to_string()]).unwrap();
        watch_and_log(session.child, &log_path).unwrap();
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("code=3"), "got: {contents}");
    }
}
