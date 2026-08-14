use crate::lock::Lock;
use crate::log::{Status, read_last_status};
use crate::task::Role;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub agent_uuid: String,
    pub role: Role,
    pub model: String,
    pub task_uuid: String,
    pub status: Option<Status>,
}

pub fn build_nodes(lock: &Lock, logs_dir: &Path) -> anyhow::Result<Vec<GraphNode>> {
    let mut nodes = Vec::new();
    for (agent_uuid, entry) in lock.iter() {
        let log_path = logs_dir.join(format!("{agent_uuid}.jsonl"));
        let status = read_last_status(&log_path).unwrap_or(None);
        nodes.push(GraphNode {
            agent_uuid: agent_uuid.clone(),
            role: entry.role.clone(),
            model: entry.model.clone(),
            task_uuid: entry.task_uuid.clone(),
            status,
        });
    }
    nodes.sort_by(|a, b| a.agent_uuid.cmp(&b.agent_uuid));
    Ok(nodes)
}

pub fn status_symbol(status: &Option<Status>) -> &'static str {
    match status {
        Some(Status::Running) => "\u{25c9}", // ◉
        Some(Status::Done) => "\u{25cb}",    // ○
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
    use crate::lock::LockEntry;
    use crate::log::{LogEntry, append_log, now_iso8601};

    #[test]
    fn build_nodes_reads_status_from_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
        append_log(
            &tmp.path().join("agent-1.jsonl"),
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();

        let nodes = build_nodes(&lock, tmp.path()).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].status, Some(Status::Running));
        assert_eq!(nodes[0].task_uuid, "task-a");
    }

    #[test]
    fn build_nodes_handles_missing_log_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Reviewer,
                task_uuid: "task-a".into(),
            },
        );

        let nodes = build_nodes(&lock, tmp.path()).unwrap();
        assert_eq!(nodes[0].status, None);
    }

    #[test]
    fn build_nodes_degrades_to_none_status_on_malformed_log() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
        std::fs::write(tmp.path().join("agent-1.jsonl"), "not valid json\n").unwrap();

        let nodes = build_nodes(&lock, tmp.path()).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].status, None);
    }

    #[test]
    fn status_symbols_distinguish_running_and_done() {
        assert_eq!(status_symbol(&Some(Status::Running)), "\u{25c9}");
        assert_eq!(status_symbol(&Some(Status::Done)), "\u{25cb}");
    }

    #[test]
    fn role_labels_are_short_codes() {
        assert_eq!(role_label(&Role::Executor), "EXE");
        assert_eq!(role_label(&Role::Reviewer), "REV");
    }
}
