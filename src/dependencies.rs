use crate::lock::Registry;
use crate::log::{Status, read_last_status};
use std::path::Path;

/// A dependency task-id counts as satisfied if `subagents.jsonl` has at
/// least one record whose `task_id` matches it, and that record's log
/// file's last status is `done`. Returns the list of dependency task-ids
/// that are NOT yet satisfied (empty means all satisfied / no dependencies).
pub fn unmet_dependencies(
    registry: &Registry,
    logs_dir: &Path,
    depends_on: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut unmet = Vec::new();
    for dep in depends_on {
        let mut satisfied = false;
        for record in &registry.records {
            if &record.task_id != dep {
                continue;
            }
            let log_path = logs_dir.join(format!("{}.jsonl", record.agent_id));
            if read_last_status(&log_path)? == Some(Status::Done) {
                satisfied = true;
                break;
            }
        }
        if !satisfied {
            unmet.push(dep.clone());
        }
    }
    Ok(unmet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::SubagentRecord;
    use crate::log::{LogEntry, append_log, now_iso8601};
    use crate::task::Role;

    fn record(agent_id: &str, task_id: &str) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "claude".into(),
            model: "claude".into(),
            role: Role::Executor,
            task_id: task_id.to_string(),
            pid: None,
            started_at: now_iso8601(),
        }
    }

    #[test]
    fn no_dependencies_means_nothing_unmet() {
        let registry = Registry::default();
        let tmp = tempfile::tempdir().unwrap();
        let unmet = unmet_dependencies(&registry, tmp.path(), &[]).unwrap();
        assert!(unmet.is_empty());
    }

    #[test]
    fn dependency_with_no_matching_agent_is_unmet() {
        let registry = Registry::default();
        let tmp = tempfile::tempdir().unwrap();
        let unmet = unmet_dependencies(&registry, tmp.path(), &["task-a".to_string()]).unwrap();
        assert_eq!(unmet, vec!["task-a".to_string()]);
    }

    #[test]
    fn dependency_running_but_not_done_is_unmet() {
        let registry = Registry::from_records(vec![record("agent-1", "task-a")]);
        let tmp = tempfile::tempdir().unwrap();
        append_log(
            &tmp.path().join("agent-1.jsonl"),
            &LogEntry {
                status: crate::log::Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        let unmet = unmet_dependencies(&registry, tmp.path(), &["task-a".to_string()]).unwrap();
        assert_eq!(unmet, vec!["task-a".to_string()]);
    }

    #[test]
    fn dependency_done_is_satisfied() {
        let registry = Registry::from_records(vec![record("agent-1", "task-a")]);
        let tmp = tempfile::tempdir().unwrap();
        append_log(
            &tmp.path().join("agent-1.jsonl"),
            &LogEntry {
                status: crate::log::Status::Done,
                action: "exited".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        let unmet = unmet_dependencies(&registry, tmp.path(), &["task-a".to_string()]).unwrap();
        assert!(unmet.is_empty());
    }

    #[test]
    fn multiple_entries_same_task_one_done_is_satisfied() {
        let registry = Registry::from_records(vec![
            record("agent-1", "task-a"),
            record("agent-2", "task-a"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        append_log(
            &tmp.path().join("agent-1.jsonl"),
            &LogEntry {
                status: crate::log::Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        append_log(
            &tmp.path().join("agent-2.jsonl"),
            &LogEntry {
                status: crate::log::Status::Done,
                action: "exited".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        let unmet = unmet_dependencies(&registry, tmp.path(), &["task-a".to_string()]).unwrap();
        assert!(unmet.is_empty());
    }

    #[test]
    fn multiple_entries_same_task_all_not_done_is_unmet() {
        let registry = Registry::from_records(vec![
            record("agent-1", "task-a"),
            record("agent-2", "task-a"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        append_log(
            &tmp.path().join("agent-1.jsonl"),
            &LogEntry {
                status: crate::log::Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        append_log(
            &tmp.path().join("agent-2.jsonl"),
            &LogEntry {
                status: crate::log::Status::Running,
                action: "relaunched".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();
        let unmet = unmet_dependencies(&registry, tmp.path(), &["task-a".to_string()]).unwrap();
        assert_eq!(unmet, vec!["task-a".to_string()]);
    }
}
