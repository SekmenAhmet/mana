//! Append-only registry of sub-agent dispatches (`subagents.jsonl`).
//!
//! Replaces the v1 YAML lock: a `BTreeMap<agent_uuid, LockEntry>` rewritten
//! wholesale (read the whole map, insert one key, write the whole map back)
//! on every single dispatch. That shape is a read-modify-write race waiting
//! to happen the moment two dispatches land close together (v1 invited it by
//! having each sub-agent process write the lock itself; in v2 mana dispatches
//! them). mana is this file's only writer, and a dispatch record never
//! changes after it's written, so a strict-append JSONL format removes the
//! race by construction — there is nothing to "modify", only to append.
//!
//! Live status (running/done) is deliberately not stored here — it's
//! derived per-agent from `crate::log`, same as in v1. This file only
//! answers "what did mana ever dispatch, and with what pid".

use crate::task::Role;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// One sub-agent dispatch. Written exactly once, right after the process is
/// actually spawned — not before — because `pid` only exists once the
/// spawn succeeded, and an append-only record can't be revised later to
/// fill it in.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SubagentRecord {
    pub agent_id: String,
    pub cli: String,
    pub model: String,
    pub role: Role,
    pub task_id: String,
    /// `None` when the spawner can't report one (e.g. the platform refuses,
    /// or a future spawner just doesn't wire it up) — never a reason to
    /// fail the dispatch. Unblocks `mana ps`/`mana kill` later.
    pub pid: Option<u32>,
    pub started_at: String,
}

/// The full dispatch history for a project, loaded fresh from disk on every
/// call — state lives in the file, not across `Registry` values. `records`
/// keeps append order (oldest first); `by_agent_id` is the common lookup
/// ("what did mana launch for this agent-id") without every caller writing
/// its own linear scan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registry {
    pub records: Vec<SubagentRecord>,
    pub by_agent_id: BTreeMap<String, SubagentRecord>,
}

impl Registry {
    /// Builds both views from a flat list of records — the shape both
    /// `load_registry` and test fixtures need, so it's one function instead
    /// of two copies of the same indexing loop.
    pub fn from_records(records: Vec<SubagentRecord>) -> Self {
        let by_agent_id = records
            .iter()
            .map(|r| (r.agent_id.clone(), r.clone()))
            .collect();
        Registry {
            records,
            by_agent_id,
        }
    }

    /// Not called from a live command yet — `mana ps`/`kill` (task 4.2) and
    /// the MCP `list_agents` tool are the intended consumers. Proven by
    /// this file's own round-trip test in the meantime; see `task.rs` for
    /// the same "tested ahead of its consumer" pattern.
    #[allow(dead_code)]
    pub fn get(&self, agent_id: &str) -> Option<&SubagentRecord> {
        self.by_agent_id.get(agent_id)
    }
}

/// Missing file reads as an empty registry — a project with no dispatches
/// yet never had a reason to create `subagents.jsonl` (see
/// `project::ensure_project_structure`).
pub fn load_registry(path: &Path) -> anyhow::Result<Registry> {
    if !path.exists() {
        return Ok(Registry::default());
    }
    let contents = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        records.push(serde_json::from_str(line)?);
    }
    Ok(Registry::from_records(records))
}

/// Appends one record and nothing else — no read of the existing file, no
/// rewrite. That's the whole fix: the v1 race lived in `load` + mutate +
/// `save`, and there is no such sequence here.
pub fn append_record(path: &Path, record: &SubagentRecord) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(record)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(agent_id: &str, task_id: &str, pid: Option<u32>) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "claude".into(),
            model: "claude".into(),
            role: Role::Executor,
            task_id: task_id.to_string(),
            pid,
            started_at: "2026-08-15T10:00:00Z".into(),
        }
    }

    #[test]
    fn load_registry_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        let registry = load_registry(&path).unwrap();
        assert!(registry.records.is_empty());
        assert!(registry.by_agent_id.is_empty());
    }

    #[test]
    fn load_registry_empty_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(load_registry(&path).unwrap().records.is_empty());
    }

    #[test]
    fn append_then_load_roundtrips_with_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        append_record(&path, &sample("agent-1", "task-1", Some(1234))).unwrap();
        append_record(&path, &sample("agent-2", "task-1", None)).unwrap();

        let registry = load_registry(&path).unwrap();
        assert_eq!(registry.records.len(), 2);
        // Append order is preserved, not re-sorted.
        assert_eq!(registry.records[0].agent_id, "agent-1");
        assert_eq!(registry.records[1].agent_id, "agent-2");

        assert_eq!(registry.get("agent-1").unwrap().pid, Some(1234));
        assert_eq!(registry.get("agent-2").unwrap().pid, None);
        assert_eq!(registry.get("agent-1").unwrap().task_id, "task-1");
        assert!(registry.get("does-not-exist").is_none());
    }

    #[test]
    fn append_never_rewrites_earlier_records() {
        // The whole point versus the YAML lock: appending a second record
        // must not touch bytes already on disk for the first one.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        append_record(&path, &sample("agent-1", "task-1", Some(1))).unwrap();
        let first_write = std::fs::read_to_string(&path).unwrap();

        append_record(&path, &sample("agent-2", "task-2", Some(2))).unwrap();
        let after_second_write = std::fs::read_to_string(&path).unwrap();

        assert!(after_second_write.starts_with(&first_write));
    }

    #[test]
    fn registry_serializes_the_documented_field_names() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        append_record(&path, &sample("agent-1", "task-1", Some(4321))).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        for key in [
            "\"agent_id\"",
            "\"cli\"",
            "\"model\"",
            "\"role\"",
            "\"task_id\"",
            "\"pid\"",
            "\"started_at\"",
        ] {
            assert!(contents.contains(key), "missing {key} in: {contents}");
        }
    }
}
