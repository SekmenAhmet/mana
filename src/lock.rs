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
use anyhow::Context;
use serde::{Deserialize, Serialize};
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
/// keeps append order (oldest first), which is the order every reader wants:
/// `ps` prints it, `counters` and the graph pane fold over all of it, and
/// `kill` resolves an unambiguous *prefix*. None of them wants a point
/// lookup, so there is no id index to keep in step with the vector.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registry {
    pub records: Vec<SubagentRecord>,
    /// One ready-to-print message per line `load_registry` could not read,
    /// naming the file and the 1-based line number. Data rather than an
    /// `eprintln!` at the reader: the TUI's graph pane re-reads this file
    /// every two seconds and would repaint the warning over its own screen
    /// forever, while a CLI wants to say it once. Empty in every ordinary
    /// case, so a caller that does not care can keep ignoring it.
    pub skipped: Vec<String>,
}

impl Registry {
    /// A registry over records that never came off disk — the shape test
    /// fixtures across the tree build, so `skipped` stays private to the
    /// reader that can actually produce one.
    pub fn from_records(records: Vec<SubagentRecord>) -> Self {
        Registry {
            records,
            skipped: Vec::new(),
        }
    }
}

/// Missing file reads as an empty registry — a project with no dispatches
/// yet never had a reason to create `subagents.jsonl` (see
/// `project::ensure_project_structure`).
///
/// A line that does not parse is **skipped and reported**, never fatal. This
/// reader used to be the one strict one in the tree (every sibling —
/// `log::read_last_exit`, the PM driver, the graph pane — already skipped what
/// it could not read), and strictness here is unrecoverable by construction:
/// the file is append-only, so one truncated line took `mana ps`, `mana kill`
/// and `mana doctor` down *permanently*, and blanked the TUI's graph pane into
/// claiming nothing was running. A torn line is a fact about the past that
/// says nothing about the records around it; dropping the one line loses one
/// dispatch, while failing the read loses every dispatch and the command with
/// it. `skipped` carries what to say about it — see the field.
pub fn load_registry(path: &Path) -> anyhow::Result<Registry> {
    if !path.exists() {
        return Ok(Registry::default());
    }
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut records = Vec::new();
    let mut skipped = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            // The line number is the actionable half: the file is append-only
            // and mana will not rewrite it, so the only repair is a human
            // deleting that line.
            Err(error) => skipped.push(format!(
                "{}: line {} is not a readable dispatch record and was skipped ({error})",
                path.display(),
                index + 1,
            )),
        }
    }
    Ok(Registry {
        skipped,
        ..Registry::from_records(records)
    })
}

/// Appends one record and nothing else — no read of the existing file, no
/// rewrite. That's the whole fix: the v1 race lived in `load` + mutate +
/// `save`, and there is no such sequence here. What it is *not* a fix for is
/// two writers tearing a single line in half; that is `log::append_line`'s
/// job, and this routes through it rather than opening the file itself.
pub fn append_record(path: &Path, record: &SubagentRecord) -> anyhow::Result<()> {
    crate::log::append_line(path, &serde_json::to_string(record)?)
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

        // A pid that was reported and one that was not both survive the
        // round trip — `None` must come back as `None`, not as a missing key.
        assert_eq!(registry.records[0].pid, Some(1234));
        assert_eq!(registry.records[1].pid, None);
        assert_eq!(registry.records[0].task_id, "task-1");
    }

    /// One torn line — what a crash mid-write, or the pre-`append_line` race,
    /// leaves behind — must cost exactly that line. Failing the read instead
    /// took `mana ps`, `kill` and `doctor` down for good, since nothing ever
    /// rewrites an append-only file.
    #[test]
    fn a_torn_line_costs_that_line_and_is_reported_with_its_file_and_number() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        append_record(&path, &sample("agent-1", "task-1", Some(1))).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}{{\"agent_id\":\"agent-2\"\n",
                std::fs::read_to_string(&path).unwrap()
            ),
        )
        .unwrap();
        append_record(&path, &sample("agent-3", "task-3", Some(3))).unwrap();

        let registry = load_registry(&path).unwrap();
        let ids: Vec<&str> = registry
            .records
            .iter()
            .map(|r| r.agent_id.as_str())
            .collect();
        assert_eq!(ids, ["agent-1", "agent-3"]);

        assert_eq!(registry.skipped.len(), 1, "{:?}", registry.skipped);
        let message = &registry.skipped[0];
        assert!(message.contains("subagents.jsonl"), "{message}");
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn a_clean_registry_reports_nothing_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents.jsonl");
        append_record(&path, &sample("agent-1", "task-1", None)).unwrap();
        assert!(load_registry(&path).unwrap().skipped.is_empty());
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
