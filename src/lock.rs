use crate::task::Role;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LockEntry {
    pub model: String,
    pub role: Role,
    #[serde(rename = "task-uuid")]
    pub task_uuid: String,
}

pub type Lock = BTreeMap<String, LockEntry>;

pub fn load_lock(path: &Path) -> anyhow::Result<Lock> {
    if !path.exists() {
        return Ok(Lock::new());
    }
    let contents = std::fs::read_to_string(path)?;
    if contents.trim().is_empty() {
        return Ok(Lock::new());
    }
    Ok(serde_yaml::from_str(&contents)?)
}

pub fn save_lock(path: &Path, lock: &Lock) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(lock)?;
    std::fs::write(path, yaml)?;
    Ok(())
}

pub fn append_entry(path: &Path, agent_uuid: &str, entry: LockEntry) -> anyhow::Result<()> {
    let mut lock = load_lock(path)?;
    lock.insert(agent_uuid.to_string(), entry);
    save_lock(path, &lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_lock_missing_or_empty_file_is_empty_map() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagent-lock.yaml");
        assert!(load_lock(&path).unwrap().is_empty());
        std::fs::write(&path, "").unwrap();
        assert!(load_lock(&path).unwrap().is_empty());
    }

    #[test]
    fn append_entry_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagent-lock.yaml");
        append_entry(
            &path,
            "agent-1",
            LockEntry {
                model: "claude".into(),
                role: Role::Executor,
                task_uuid: "task-1".into(),
            },
        )
        .unwrap();
        append_entry(
            &path,
            "agent-2",
            LockEntry {
                model: "claude".into(),
                role: Role::Reviewer,
                task_uuid: "task-1".into(),
            },
        )
        .unwrap();
        let lock = load_lock(&path).unwrap();
        assert_eq!(lock.len(), 2);
        assert_eq!(lock.get("agent-1").unwrap().role, Role::Executor);
        assert_eq!(lock.get("agent-2").unwrap().task_uuid, "task-1");
    }
}
