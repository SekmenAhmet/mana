//! The graph pane's model: one node per dispatch mana ever made for this
//! project, with whatever is currently known about how it went.
//!
//! Everything here is read back from files (`subagents.jsonl`, `logs/*.jsonl`,
//! `reviews/*.json`) because that is where the truth lives -- the dispatches
//! run on their own threads inside the MCP server and report by writing.
//! What changed in v2 is *when*: v1 re-read every one of those files inside
//! the draw call, twenty times a second, for a picture that only moves when a
//! sub-agent finishes. `GraphCache` rebuilds on that event, or on a slow
//! timer as a safety net, and the render path just formats what it holds.

use crate::lock::Registry;
use crate::log::{Status, read_last_status};
use crate::project::ProjectPaths;
use crate::review::{Decision, read_verdict};
use crate::task::Role;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long each on/off half-cycle of the "running" blink lasts.
pub const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// The safety net between event-driven rebuilds.
///
/// Notifications are the real trigger, but they only cover dispatches mana
/// started in *this* session: a reviewer launched from another window, or a
/// log line written while the PM was quiet, would otherwise never show up.
/// Two seconds is slow enough to be free and fast enough that nobody notices.
pub const GRAPH_REFRESH: Duration = Duration::from_secs(2);

/// How much of a task id the pane shows. Ids are v4 UUIDs (36 characters),
/// which is wider than the whole pane; the first block is plenty to tell two
/// tasks apart by eye and matches how one refers to a commit.
const SHORT_ID: usize = 8;

/// Pure on/off decision for the blink, given how long the TUI has been
/// running. Kept separate from any real clock so it is testable without
/// sleeping or faking a `Terminal`.
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
    /// Which CLI ran it -- the point of the whole system is that this differs
    /// between nodes, so showing only the model would hide the arbitrage.
    pub cli: String,
    pub model: String,
    pub task_id: String,
    pub status: Option<Status>,
    /// The verdict on this node's *task*, not on the node itself. Both the
    /// executor and the reviewer of a task carry it: it is the one thing the
    /// user is waiting for, and hiding it on the executor row would mean the
    /// answer only appears next to whichever row happens to be last.
    pub verdict: Option<Decision>,
}

/// The pane's contents, rebuilt on demand rather than on every frame.
pub struct GraphCache {
    nodes: Vec<GraphNode>,
    /// `None` until the first rebuild, so the pane is never empty on a fresh
    /// session that already has history.
    next_rebuild: Option<Instant>,
}

impl GraphCache {
    pub fn new() -> Self {
        GraphCache {
            nodes: Vec::new(),
            next_rebuild: None,
        }
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    /// Rebuilds when something reported in (`changed`), when the timer is up,
    /// or on the very first call. Reading the files is a handful of small
    /// `read_to_string`s, so a failure anywhere degrades to the node simply
    /// not knowing its status -- never to a crashed render loop.
    pub fn refresh(&mut self, paths: &ProjectPaths, now: Instant, changed: bool) {
        let due = self.next_rebuild.is_none_or(|next| now >= next);
        if !changed && !due {
            return;
        }
        self.next_rebuild = Some(now + GRAPH_REFRESH);
        let registry = crate::lock::load_registry(&paths.subagents_file).unwrap_or_default();
        self.nodes = build_nodes(&registry, &paths.logs, &paths.reviews);
    }
}

/// Turns the dispatch registry into displayable nodes, joining each one to its
/// log (for status) and its task's verdict file (for the ✅/❌).
pub fn build_nodes(registry: &Registry, logs_dir: &Path, reviews_dir: &Path) -> Vec<GraphNode> {
    let mut nodes: Vec<GraphNode> = registry
        .records
        .iter()
        .map(|record| GraphNode {
            agent_id: record.agent_id.clone(),
            role: record.role.clone(),
            cli: record.cli.clone(),
            model: record.model.clone(),
            task_id: record.task_id.clone(),
            status: read_last_status(&logs_dir.join(format!("{}.jsonl", record.agent_id)))
                .unwrap_or(None),
            // A missing file is the ordinary case (nobody has reviewed this
            // task yet) and a malformed one is the reviewer's failure, which
            // `get_review` reports to the PM in words. Neither is worth a
            // broken pane, so both read as "no verdict".
            verdict: read_verdict(&reviews_dir.join(format!("{}.json", record.task_id)))
                .ok()
                .map(|verdict| verdict.verdict),
        })
        .collect();
    nodes.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    nodes
}

/// One node as a single line: status, role, where it ran, which task, verdict.
pub fn node_line(node: &GraphNode, blink_visible: bool) -> String {
    format!(
        "{} [{}] {}/{} {}{}",
        status_symbol(&node.status, blink_visible),
        role_label(&node.role),
        node.cli,
        node.model,
        short_id(&node.task_id),
        verdict_symbol(&node.verdict),
    )
}

fn short_id(task_id: &str) -> String {
    task_id.chars().take(SHORT_ID).collect()
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
        Some(Status::Done) => "\u{25cb}", // ○, fixed -- not blinking
        None => "?",
    }
}

/// Nothing at all until a reviewer has spoken: an empty column is honest,
/// whereas a placeholder glyph reads as a verdict of its own.
fn verdict_symbol(verdict: &Option<Decision>) -> &'static str {
    match verdict {
        Some(Decision::Validated) => " \u{2705}", // ✅
        Some(Decision::Rejected) => " \u{274c}",  // ❌
        None => "",
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
    use crate::project::{ensure_project_structure, resolve_project_paths};

    fn record(agent_id: &str, role: Role, task_id: &str) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "claude".into(),
            model: "haiku".into(),
            role,
            task_id: task_id.to_string(),
            pid: None,
            started_at: now_iso8601(),
        }
    }

    fn fixture() -> (tempfile::TempDir, ProjectPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        ensure_project_structure(&paths).unwrap();
        (tmp, paths)
    }

    #[test]
    fn build_nodes_reads_status_from_logs_and_carries_the_cli() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![record("agent-1", Role::Executor, "task-a")]);
        append_log(
            &paths.logs.join("agent-1.jsonl"),
            &LogEntry {
                status: Status::Running,
                action: "started".into(),
                timestamp: now_iso8601(),
            },
        )
        .unwrap();

        let nodes = build_nodes(&registry, &paths.logs, &paths.reviews);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].status, Some(Status::Running));
        assert_eq!(nodes[0].task_id, "task-a");
        assert_eq!(nodes[0].cli, "claude");
        assert_eq!(nodes[0].model, "haiku");
        assert_eq!(nodes[0].verdict, None);
    }

    #[test]
    fn build_nodes_handles_missing_log_as_none() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![record("agent-1", Role::Reviewer, "task-a")]);
        assert_eq!(
            build_nodes(&registry, &paths.logs, &paths.reviews)[0].status,
            None
        );
    }

    #[test]
    fn build_nodes_degrades_to_none_on_malformed_log_and_verdict() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![record("agent-1", Role::Executor, "task-a")]);
        std::fs::write(paths.logs.join("agent-1.jsonl"), "not valid json\n").unwrap();
        std::fs::write(paths.reviews.join("task-a.json"), "Sure, looks fine!").unwrap();

        let nodes = build_nodes(&registry, &paths.logs, &paths.reviews);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].status, None);
        assert_eq!(nodes[0].verdict, None);
    }

    /// The ✅ the milestone is defined by: a verdict on disk reaches both of
    /// the task's nodes.
    #[test]
    fn a_verdict_reaches_every_node_of_its_task() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![
            record("agent-1", Role::Executor, "task-a"),
            record("agent-2", Role::Reviewer, "task-a"),
            record("agent-3", Role::Executor, "task-b"),
        ]);
        std::fs::write(
            paths.reviews.join("task-a.json"),
            r#"{"verdict":"validated","issues":[]}"#,
        )
        .unwrap();

        let nodes = build_nodes(&registry, &paths.logs, &paths.reviews);
        assert_eq!(nodes[0].verdict, Some(Decision::Validated));
        assert_eq!(nodes[1].verdict, Some(Decision::Validated));
        assert_eq!(nodes[2].verdict, None);
    }

    #[test]
    fn a_rejection_is_rendered_as_a_cross() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![record("agent-1", Role::Executor, "task-a")]);
        std::fs::write(
            paths.reviews.join("task-a.json"),
            r#"{"verdict":"rejected","attribution":"code","issues":["nope"]}"#,
        )
        .unwrap();

        let node = &build_nodes(&registry, &paths.logs, &paths.reviews)[0];
        assert_eq!(node.verdict, Some(Decision::Rejected));
        assert!(node_line(node, true).ends_with("\u{274c}"));
    }

    #[test]
    fn a_node_line_names_the_role_the_pair_and_the_task() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![record(
            "agent-1",
            Role::Reviewer,
            "3f2a1b6c-0000-4000-8000-000000000000",
        )]);
        let line = node_line(
            &build_nodes(&registry, &paths.logs, &paths.reviews)[0],
            true,
        );
        assert_eq!(line, "? [REV] claude/haiku 3f2a1b6c");
    }

    /// The v1 defect this cache exists to fix: every frame re-read every file.
    #[test]
    fn the_cache_rebuilds_on_the_first_call_then_only_when_asked() {
        let (_tmp, paths) = fixture();
        let mut cache = GraphCache::new();
        let start = Instant::now();

        cache.refresh(&paths, start, false);
        assert!(cache.nodes().is_empty());

        crate::lock::append_record(
            &paths.subagents_file,
            &record("agent-1", Role::Executor, "task-a"),
        )
        .unwrap();

        // Same instant, nothing reported in: the pane keeps what it had.
        cache.refresh(&paths, start, false);
        assert!(cache.nodes().is_empty());

        // A dispatch reported in -- rebuild now, whatever the timer says.
        cache.refresh(&paths, start, true);
        assert_eq!(cache.nodes().len(), 1);
    }

    #[test]
    fn the_cache_rebuilds_once_the_refresh_interval_has_passed() {
        let (_tmp, paths) = fixture();
        let mut cache = GraphCache::new();
        let start = Instant::now();
        cache.refresh(&paths, start, false);

        crate::lock::append_record(
            &paths.subagents_file,
            &record("agent-1", Role::Executor, "task-a"),
        )
        .unwrap();
        cache.refresh(&paths, start + GRAPH_REFRESH, false);
        assert_eq!(cache.nodes().len(), 1);
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
