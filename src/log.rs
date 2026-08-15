use crate::lock::Registry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// The record appended when a sub-agent process exits. A distinct type from
/// `LogEntry` rather than packing everything into `action` as a string
/// (v1 wrote `exited(code=3)`) — that string-packing only existed to make a
/// regex-based counters pass find the exit code again later, which is
/// exactly the kind of cleverness a typed field removes. Duration and token
/// fields use the exact OTEL GenAI semantic-convention key names (see
/// design doc §8) so a future OTEL exporter is a pipe change, not a rename;
/// they're `Option` because this dispatcher (task 1.3) fills `exit_code`
/// and `duration_ms` but usage/failure enrichment lands later and must be
/// absent from the wire, not `null`, until then.
///
/// Not constructed by production code yet — only by this file's own tests
/// (see `counters`) — until task 1.3 rewires the real dispatcher to write
/// it; `#[allow(dead_code)]` matches the precedent in `task.rs` for
/// schema/API surface proven by tests ahead of its consumer landing.
#[allow(dead_code)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ExitEntry {
    pub status: Status,
    /// Always `"exited"` — kept as a plain tag (not string-packed with the
    /// code) so anything still reading the stream generically via
    /// `LogEntry`/`read_last_status` (file watcher, pty listener) keeps
    /// working unchanged; serde ignores the extra typed fields it doesn't
    /// know about.
    pub action: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `gen_ai.client.operation.duration` — stored as whole milliseconds
    /// here; OTEL's own convention favors seconds/double, but converting is
    /// the future exporter's job, not this file's.
    #[serde(
        rename = "gen_ai.client.operation.duration",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub duration_ms: Option<u64>,
    #[serde(
        rename = "gen_ai.usage.input_tokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_tokens: Option<u64>,
    #[serde(
        rename = "gen_ai.usage.output_tokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_tokens: Option<u64>,
    /// `Some("quota_exhausted")` etc. once the catalogue's ordered failure
    /// signatures (design doc §8) match this run's exit code/stderr/stdout.
    /// The dispatcher populates this in task 1.3; `counters` already knows
    /// how to read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_means: Option<String>,
}

pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Generic over the entry type so both the plain `{status, action,
/// timestamp}` events (started, file:modified:...) and the richer
/// `ExitEntry` share one append-and-create-parents implementation.
pub fn append_log<T: Serialize>(path: &Path, entry: &T) -> anyhow::Result<()> {
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
    let last_line = contents.lines().rfind(|l| !l.trim().is_empty());
    match last_line {
        None => Ok(None),
        Some(line) => {
            let entry: LogEntry = serde_json::from_str(line)?;
            Ok(Some(entry.status))
        }
    }
}

/// Scans from the end for the last line tagged `action == "exited"` rather
/// than trusting it's literally the last line in the file: the file watcher
/// and the exit watcher run on separate threads (see
/// `cli::launch_subagent::run_at`), so a trailing `file:modified:` event
/// can in principle land after the exit record. Any line failing to parse
/// as `ExitEntry` (or any other action) is skipped, not fatal — the file is
/// still being written by another agent's run.
///
/// Only called by `counters` today (see its own `#[allow(dead_code)]` note).
#[allow(dead_code)]
fn read_last_exit(path: &Path) -> anyhow::Result<Option<ExitEntry>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    for line in contents.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ExitEntry>(line)
            && entry.action == "exited"
        {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Per-(cli, model) raw counters — dispatched, quota failures, average
/// duration. Deliberately not a synthesized score: design doc §8 is
/// explicit that with dozens of cells and few tasks, a score can't stay
/// calibrated, but "4 of 6" lets the PM discount a small sample itself.
/// `validated`/`rejected` are left out rather than stubbed — they only
/// exist once reviewer verdicts land in task 1.4.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub dispatched: u32,
    pub quota_failures: u32,
    pub avg_duration_ms: Option<u64>,
}

/// Joins `registry`'s dispatches with their log files to produce routing
/// counters. Every record counts toward `dispatched` — including ones with
/// no exit record yet (still running) — because "how many times has mana
/// sent work to this (cli, model) pair" is the whole point of a raw
/// counter; only `quota_failures` and the duration average need an actual
/// exit record to say anything.
///
/// Not wired into a live command yet (that's the TUI/MCP `list_agents`
/// surface, later tasks) — exercised directly by this file's tests for now.
#[allow(dead_code)]
pub fn counters(
    logs_dir: &Path,
    registry: &Registry,
) -> anyhow::Result<BTreeMap<(String, String), Counts>> {
    #[derive(Default)]
    struct Accumulator {
        dispatched: u32,
        quota_failures: u32,
        duration_sum_ms: u64,
        duration_samples: u32,
    }

    let mut totals: BTreeMap<(String, String), Accumulator> = BTreeMap::new();

    for record in &registry.records {
        let key = (record.cli.clone(), record.model.clone());
        let acc = totals.entry(key).or_default();
        acc.dispatched += 1;

        let log_path = logs_dir.join(format!("{}.jsonl", record.agent_id));
        if let Some(exit) = read_last_exit(&log_path)? {
            if exit.failure_means.as_deref() == Some("quota_exhausted") {
                acc.quota_failures += 1;
            }
            if let Some(ms) = exit.duration_ms {
                acc.duration_sum_ms += ms;
                acc.duration_samples += 1;
            }
        }
    }

    Ok(totals
        .into_iter()
        .map(|(key, acc)| {
            let avg_duration_ms = (acc.duration_samples > 0)
                .then(|| acc.duration_sum_ms / u64::from(acc.duration_samples));
            (
                key,
                Counts {
                    dispatched: acc.dispatched,
                    quota_failures: acc.quota_failures,
                    avg_duration_ms,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::SubagentRecord;
    use crate::task::Role;

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

    fn sample_exit(duration_ms: Option<u64>, failure_means: Option<&str>) -> ExitEntry {
        ExitEntry {
            status: Status::Done,
            action: "exited".into(),
            timestamp: now_iso8601(),
            exit_code: Some(if failure_means.is_some() { 1 } else { 0 }),
            duration_ms,
            input_tokens: None,
            output_tokens: None,
            failure_means: failure_means.map(str::to_string),
        }
    }

    #[test]
    fn exit_entry_serde_round_trip_uses_literal_otel_keys() {
        // The exact key strings are a wire contract (future OTEL exporters,
        // `mana ps`/counters readers) — assert them literally, not just
        // that round-tripping happens to work.
        let entry = ExitEntry {
            status: Status::Done,
            action: "exited".into(),
            timestamp: "2026-08-15T10:00:00Z".into(),
            exit_code: Some(3),
            duration_ms: Some(1500),
            input_tokens: Some(120),
            output_tokens: Some(340),
            failure_means: Some("quota_exhausted".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains("\"gen_ai.client.operation.duration\":1500"),
            "{json}"
        );
        assert!(json.contains("\"gen_ai.usage.input_tokens\":120"), "{json}");
        assert!(
            json.contains("\"gen_ai.usage.output_tokens\":340"),
            "{json}"
        );
        assert!(json.contains("\"exit_code\":3"), "{json}");
        assert!(
            json.contains("\"failure_means\":\"quota_exhausted\""),
            "{json}"
        );

        let round_tripped: ExitEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, entry);
    }

    #[test]
    fn exit_entry_omits_unset_optional_fields_from_the_wire() {
        let entry = sample_exit(None, None);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("gen_ai.client.operation.duration"), "{json}");
        assert!(!json.contains("gen_ai.usage.input_tokens"), "{json}");
        assert!(!json.contains("gen_ai.usage.output_tokens"), "{json}");
        assert!(!json.contains("failure_means"), "{json}");
    }

    #[test]
    fn a_trailing_file_modified_line_does_not_hide_the_exit_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.jsonl");
        append_log(
            &path,
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        append_log(&path, &sample_exit(Some(2000), None)).unwrap();
        // A file-watcher event that raced past the exit record on its own
        // thread — still `Running`, appended after `exited`.
        append_log(
            &path,
            &LogEntry {
                status: Status::Running,
                action: "file:modified:src/main.rs".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();

        let exit = read_last_exit(&path).unwrap().unwrap();
        assert_eq!(exit.duration_ms, Some(2000));
    }

    fn record(agent_id: &str, cli: &str, model: &str) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: cli.to_string(),
            model: model.to_string(),
            role: Role::Executor,
            task_id: format!("task-for-{agent_id}"),
            pid: None,
            started_at: now_iso8601(),
        }
    }

    #[test]
    fn counters_derives_per_cli_model_pair_across_two_clis_with_a_quota_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let logs_dir = tmp.path().join("logs");

        let registry = Registry::from_records(vec![
            record("agent-1", "claude", "opus"),
            record("agent-2", "claude", "opus"),
            record("agent-3", "agy", "default"),
        ]);

        append_log(
            &logs_dir.join("agent-1.jsonl"),
            &sample_exit(Some(1000), None),
        )
        .unwrap();
        append_log(
            &logs_dir.join("agent-2.jsonl"),
            &sample_exit(Some(2000), Some("quota_exhausted")),
        )
        .unwrap();
        append_log(
            &logs_dir.join("agent-3.jsonl"),
            &sample_exit(Some(3000), None),
        )
        .unwrap();

        let result = counters(&logs_dir, &registry).unwrap();

        let claude_opus = result
            .get(&("claude".to_string(), "opus".to_string()))
            .unwrap();
        assert_eq!(claude_opus.dispatched, 2);
        assert_eq!(claude_opus.quota_failures, 1);
        assert_eq!(claude_opus.avg_duration_ms, Some(1500)); // (1000 + 2000) / 2

        let agy_default = result
            .get(&("agy".to_string(), "default".to_string()))
            .unwrap();
        assert_eq!(agy_default.dispatched, 1);
        assert_eq!(agy_default.quota_failures, 0);
        assert_eq!(agy_default.avg_duration_ms, Some(3000));
    }

    #[test]
    fn counters_counts_a_still_running_agent_as_dispatched_without_a_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let logs_dir = tmp.path().join("logs");
        let registry = Registry::from_records(vec![record("agent-1", "claude", "opus")]);
        append_log(
            &logs_dir.join("agent-1.jsonl"),
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();

        let result = counters(&logs_dir, &registry).unwrap();
        let counts = result
            .get(&("claude".to_string(), "opus".to_string()))
            .unwrap();
        assert_eq!(counts.dispatched, 1);
        assert_eq!(counts.quota_failures, 0);
        assert_eq!(counts.avg_duration_ms, None);
    }

    #[test]
    fn counters_on_empty_registry_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let result = counters(&tmp.path().join("logs"), &Registry::default()).unwrap();
        assert!(result.is_empty());
    }
}
