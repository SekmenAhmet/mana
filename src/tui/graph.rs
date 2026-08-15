use crate::lock::Registry;
use crate::log::{Status, read_last_status};
use crate::task::Role;
use std::path::Path;
use std::time::Duration;

/// How long each on/off half-cycle of the "running" blink lasts. TUI.md
/// calls for `◉` "clignotant" on running agents — `done` stays a fixed `○`.
pub const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Pure on/off decision for the blink, given how long the TUI has been
/// running. Kept separate from any real clock/render loop so it's testable
/// without sleeping or faking a `Terminal`.
pub fn is_blink_visible(elapsed: Duration, interval: Duration) -> bool {
    if interval.is_zero() {
        return true;
    }
    (elapsed.as_millis() / interval.as_millis()).is_multiple_of(2)
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub agent_id: String,
    pub role: Role,
    pub model: String,
    pub task_id: String,
    pub status: Option<Status>,
}

pub fn build_nodes(registry: &Registry, logs_dir: &Path) -> anyhow::Result<Vec<GraphNode>> {
    let mut nodes = Vec::new();
    for record in &registry.records {
        let log_path = logs_dir.join(format!("{}.jsonl", record.agent_id));
        let status = read_last_status(&log_path).unwrap_or(None);
        nodes.push(GraphNode {
            agent_id: record.agent_id.clone(),
            role: record.role.clone(),
            model: record.model.clone(),
            task_id: record.task_id.clone(),
            status,
        });
    }
    nodes.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    Ok(nodes)
}

pub fn status_symbol(status: &Option<Status>, blink_visible: bool) -> &'static str {
    match status {
        Some(Status::Running) => {
            if blink_visible {
                "\u{25c9}" // ◉
            } else {
                " "
            }
        }
        Some(Status::Done) => "\u{25cb}", // ○, fixed — not blinking
        None => "?",
    }
}

pub fn role_label(role: &Role) -> &'static str {
    match role {
        Role::Executor => "EXE",
        Role::Reviewer => "REV",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::SubagentRecord;
    use crate::log::{LogEntry, append_log, now_iso8601};

    fn record(agent_id: &str, role: Role, task_id: &str) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "claude".into(),
            model: "claude".into(),
            role,
            task_id: task_id.to_string(),
            pid: None,
            started_at: now_iso8601(),
        }
    }

    #[test]
    fn build_nodes_reads_status_from_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::from_records(vec![record("agent-1", Role::Executor, "task-a")]);
        append_log(
            &tmp.path().join("agent-1.jsonl"),
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();

        let nodes = build_nodes(&registry, tmp.path()).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].status, Some(Status::Running));
        assert_eq!(nodes[0].task_id, "task-a");
    }

    #[test]
    fn build_nodes_handles_missing_log_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::from_records(vec![record("agent-1", Role::Reviewer, "task-a")]);

        let nodes = build_nodes(&registry, tmp.path()).unwrap();
        assert_eq!(nodes[0].status, None);
    }

    #[test]
    fn build_nodes_degrades_to_none_status_on_malformed_log() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::from_records(vec![record("agent-1", Role::Executor, "task-a")]);
        std::fs::write(tmp.path().join("agent-1.jsonl"), "not valid json\n").unwrap();

        let nodes = build_nodes(&registry, tmp.path()).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].status, None);
    }

    #[test]
    fn status_symbols_distinguish_running_and_done() {
        assert_eq!(status_symbol(&Some(Status::Running), true), "\u{25c9}");
        assert_eq!(status_symbol(&Some(Status::Done), true), "\u{25cb}");
        assert_eq!(status_symbol(&None, true), "?");
    }

    #[test]
    fn running_status_blinks_off_when_not_visible() {
        assert_eq!(status_symbol(&Some(Status::Running), false), " ");
    }

    #[test]
    fn done_status_never_blinks() {
        assert_eq!(status_symbol(&Some(Status::Done), false), "\u{25cb}");
    }

    #[test]
    fn is_blink_visible_alternates_every_interval() {
        let interval = Duration::from_millis(500);
        assert!(is_blink_visible(Duration::from_millis(0), interval));
        assert!(is_blink_visible(Duration::from_millis(499), interval));
        assert!(!is_blink_visible(Duration::from_millis(500), interval));
        assert!(!is_blink_visible(Duration::from_millis(999), interval));
        assert!(is_blink_visible(Duration::from_millis(1000), interval));
    }

    #[test]
    fn is_blink_visible_treats_zero_interval_as_always_visible() {
        assert!(is_blink_visible(
            Duration::from_millis(1234),
            Duration::ZERO
        ));
    }

    #[test]
    fn role_labels_are_short_codes() {
        assert_eq!(role_label(&Role::Executor), "EXE");
        assert_eq!(role_label(&Role::Reviewer), "REV");
    }
}
