use crate::lock::Lock;
use crate::log::{Status, read_last_status};
use std::path::Path;

/// A dependency task-uuid counts as satisfied if `subagent-lock.yaml` has at
/// least one entry whose `task-uuid` matches it, and that entry's log file's
/// last status is `done`. Returns the list of dependency task-uuids that are
/// NOT yet satisfied (empty means all satisfied / no dependencies).
pub fn unmet_dependencies(
    lock: &Lock,
    logs_dir: &Path,
    depends_on: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut unmet = Vec::new();
    for dep in depends_on {
        let mut satisfied = false;
        for (agent_uuid, entry) in lock.iter() {
            if &entry.task_uuid != dep {
                continue;
            }
            let log_path = logs_dir.join(format!("{agent_uuid}.jsonl"));
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
    use crate::lock::LockEntry;
    use crate::log::{LogEntry, append_log, now_iso8601};
    use crate::task::Role;

    #[test]
    fn no_dependencies_means_nothing_unmet() {
        let lock = Lock::new();
        let tmp = tempfile::tempdir().unwrap();
        let unmet = unmet_dependencies(&lock, tmp.path(), &[]).unwrap();
        assert!(unmet.is_empty());
    }

    #[test]
    fn dependency_with_no_matching_agent_is_unmet() {
        let lock = Lock::new();
        let tmp = tempfile::tempdir().unwrap();
        let unmet = unmet_dependencies(&lock, tmp.path(), &["task-a".to_string()]).unwrap();
        assert_eq!(unmet, vec!["task-a".to_string()]);
    }

    #[test]
    fn dependency_running_but_not_done_is_unmet() {
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
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
        let unmet = unmet_dependencies(&lock, tmp.path(), &["task-a".to_string()]).unwrap();
        assert_eq!(unmet, vec!["task-a".to_string()]);
    }

    #[test]
    fn dependency_done_is_satisfied() {
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
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
        let unmet = unmet_dependencies(&lock, tmp.path(), &["task-a".to_string()]).unwrap();
        assert!(unmet.is_empty());
    }

    #[test]
    fn multiple_entries_same_task_one_done_is_satisfied() {
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
        lock.insert(
            "agent-2".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
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
        let unmet = unmet_dependencies(&lock, tmp.path(), &["task-a".to_string()]).unwrap();
        assert!(unmet.is_empty());
    }

    #[test]
    fn multiple_entries_same_task_all_not_done_is_unmet() {
        let mut lock = Lock::new();
        lock.insert(
            "agent-1".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
        lock.insert(
            "agent-2".to_string(),
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-a".into(),
            },
        );
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
        let unmet = unmet_dependencies(&lock, tmp.path(), &["task-a".to_string()]).unwrap();
        assert_eq!(unmet, vec!["task-a".to_string()]);
    }
}
