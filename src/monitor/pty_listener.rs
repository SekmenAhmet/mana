use crate::log::{append_log, now_iso8601, LogEntry, Status};
use std::io::Read;
use std::path::PathBuf;

pub fn strip_ansi(bytes: &[u8]) -> String {
    let stripped = strip_ansi_escapes::strip(bytes);
    String::from_utf8_lossy(&stripped).to_string()
}

/// Best-effort: looks for `Bash(<command>)`-shaped substrings, as printed by
/// Claude Code when it invokes its Bash tool, and returns the inner command
/// text. Misses are non-fatal — logging stays incomplete, nothing breaks.
pub fn extract_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let marker = "Bash(";
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find(marker) {
        let abs_start = search_from + start + marker.len();
        if let Some(end_offset) = text[abs_start..].find(')') {
            let cmd = text[abs_start..abs_start + end_offset].trim();
            if !cmd.is_empty() {
                commands.push(cmd.to_string());
            }
            search_from = abs_start + end_offset + 1;
        } else {
            break;
        }
    }
    commands
}

pub fn spawn_listener(mut reader: Box<dyn Read + Send>, log_path: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = strip_ansi(&buf[..n]);
                    for cmd in extract_commands(&text) {
                        let _ = append_log(
                            &log_path,
                            &LogEntry { status: Status::Running, action: format!("cmd:{cmd}"), timestamp: now_iso8601() },
                        );
                    }
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::read_last_status;

    #[test]
    fn strip_ansi_removes_escape_codes() {
        let input = b"\x1b[1;32mhello\x1b[0m world";
        assert_eq!(strip_ansi(input), "hello world");
    }

    #[test]
    fn extract_commands_finds_single_bash_call() {
        let text = "some text \u{23fa} Bash(cargo test) more text";
        assert_eq!(extract_commands(text), vec!["cargo test".to_string()]);
    }

    #[test]
    fn extract_commands_finds_multiple_bash_calls() {
        let text = "Bash(cargo build)\nsome output\nBash(cargo test)\n";
        assert_eq!(extract_commands(text), vec!["cargo build".to_string(), "cargo test".to_string()]);
    }

    #[test]
    fn extract_commands_ignores_text_without_bash_calls() {
        let text = "no tool calls here, just prose.";
        assert!(extract_commands(text).is_empty());
    }

    #[test]
    fn spawn_listener_logs_detected_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.jsonl");
        let data = b"prefix Bash(echo hi) suffix".to_vec();
        let reader: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(data));
        let handle = spawn_listener(reader, log_path.clone());
        handle.join().unwrap();
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("cmd:echo hi"), "got: {contents}");
        assert!(read_last_status(&log_path).unwrap().is_some());
    }
}
