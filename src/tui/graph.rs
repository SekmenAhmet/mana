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
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long each frame of the "running" marker is held.
///
/// Four frames at 300ms is a 1.2s cycle, deliberately not a divisor of
/// `GRAPH_REFRESH`: anything that only samples this pane on that cadence -- the
/// redraw safety net, a pty rig, an operator glancing every couple of seconds --
/// would otherwise read the same phase every single time (#43).
pub const SPINNER_INTERVAL: Duration = Duration::from_millis(300);

/// The frames of the running marker. Every one of them is a glyph.
///
/// It used to blink between ◉ and a space, so half of every cycle drew a
/// running node with *no marker at all* -- which reads as a node nothing knows
/// anything about, and is what a reader sampling on the wrong phase saw for as
/// long as they looked (#43). Which phase somebody lands on is not something
/// this code gets to control, so the invisible frame had to go rather than be
/// made rarer by redrawing faster.
///
/// U+25D0..U+25D3, the same Geometric Shapes block as the ○ below and as
/// `theme::MARK`: quarter-turns of one circle, so the marker reads as motion
/// and never as the ○ a finished node wears.
const RUNNING_FRAMES: [&str; 4] = ["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"];

/// The safety net between event-driven rebuilds.
///
/// Notifications are the real trigger, but they only cover dispatches mana
/// started in *this* session: a reviewer launched from another window, or a
/// log line written while the PM was quiet, would otherwise never show up.
/// Two seconds is slow enough to be free and fast enough that nobody notices.
pub const GRAPH_REFRESH: Duration = Duration::from_secs(2);

/// Which frame the running marker is on, given how long the TUI has been
/// running. Kept separate from any real clock so it is testable without
/// sleeping or faking a `Terminal`.
pub fn running_frame(elapsed: Duration, interval: Duration) -> &'static str {
    if interval.is_zero() {
        return RUNNING_FRAMES[0];
    }
    let frames = RUNNING_FRAMES.len() as u128;
    RUNNING_FRAMES[(elapsed.as_millis() / interval.as_millis() % frames) as usize]
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
        // `unwrap_or_default` now only swallows a file that could not be
        // *read* (permissions, a directory in its place) -- an unreadable
        // *line* no longer fails the load, which is what used to make one torn
        // append blank this pane permanently and tell the user nothing was
        // running while agents were. `registry.skipped` is deliberately not
        // rendered here: this runs every two seconds, and the pane has no
        // place to put a message that will still be true on the next frame.
        // `mana ps`/`kill`/`doctor` say it once instead (see
        // `status::dispatches_in`).
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
    // Grouped by task, executor above its reviewer, tasks in the order they
    // were first dispatched. Sorting by `agent_id` was deterministic and told
    // the reader nothing -- v4 uuids order by nothing, so a task's two nodes
    // landed at unrelated positions and matching them meant hunting for short
    // ids (#44). This pane is called a graph, and the one edge it has is
    // "this reviewer reviews that executor's task": putting the pair on
    // consecutive rows is that edge, drawn.
    //
    // The registry is append-only, so its own order *is* dispatch order and no
    // timestamp has to be parsed (or trusted) to get chronology between tasks.
    // `sort_by_key` is stable, which keeps that order for everything the key
    // ties -- a task re-dispatched after a rejection stays behind its first try.
    let mut first_dispatch: HashMap<&str, usize> = HashMap::new();
    for (index, record) in registry.records.iter().enumerate() {
        first_dispatch.entry(&record.task_id).or_insert(index);
    }
    nodes.sort_by_key(|node| {
        (
            first_dispatch
                .get(node.task_id.as_str())
                .copied()
                .unwrap_or(usize::MAX),
            matches!(node.role, Role::Reviewer),
        )
    });
    nodes
}

/// One node's line, up to but not including its verdict: status, role, where
/// it ran, which task.
///
/// The verdict is formatted separately ([`verdict_symbol`]) because the two
/// halves are coloured apart: the body is mana's own lavender, while the ✓/✗
/// wear the interface's only semantic colours (`theme::VERDICT_OK` /
/// `theme::VERDICT_FAIL`).
pub fn node_body(node: &GraphNode, running: &'static str) -> String {
    format!(
        "{} [{}] {}/{} {}",
        status_symbol(&node.status, running),
        role_label(&node.role),
        node.cli,
        node.model,
        crate::status::short(&node.task_id),
    )
}

pub fn status_symbol(status: &Option<Status>, running: &'static str) -> &'static str {
    match status {
        Some(Status::Running) => running,
        Some(Status::Done) => "\u{25cb}", // ○, fixed -- the work is over
        None => "?",
    }
}

/// Nothing at all until a reviewer has spoken: an empty column is honest,
/// whereas a placeholder glyph reads as a verdict of its own.
pub fn verdict_symbol(verdict: &Option<Decision>) -> &'static str {
    // Text-presentation glyphs, not the ✅/❌ emoji: emoji are double-width
    // (a latent column-math hazard in any TUI) and missing from plenty of
    // monospace fonts -- the README screenshots rendered them as tofu. The
    // pass/fail colour they used to carry moved into the theme instead.
    match verdict {
        Some(Decision::Validated) => " \u{2713}", // ✓
        Some(Decision::Rejected) => " \u{2717}",  // ✗
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
        // Formatted apart from the body, and rendered as its own span, so the
        // cross wears its own semantic colour instead of the pane's.
        assert_eq!(verdict_symbol(&node.verdict), " \u{2717}");
    }

    #[test]
    fn a_node_body_names_the_role_the_pair_and_the_task() {
        let (_tmp, paths) = fixture();
        let registry = Registry::from_records(vec![record(
            "agent-1",
            Role::Reviewer,
            "3f2a1b6c-0000-4000-8000-000000000000",
        )]);
        let line = node_body(
            &build_nodes(&registry, &paths.logs, &paths.reviews)[0],
            RUNNING_FRAMES[0],
        );
        assert_eq!(line, "? [REV] claude/haiku 3f2a1b6c");
        assert_eq!(verdict_symbol(&None), "");
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

    /// The pane used to go blank forever the moment one line of the registry
    /// was torn, and blank reads as "nothing is running" — the opposite of the
    /// truth while agents are working.
    #[test]
    fn a_torn_registry_line_costs_one_node_rather_than_the_whole_pane() {
        let (_tmp, paths) = fixture();
        crate::lock::append_record(
            &paths.subagents_file,
            &record("agent-1", Role::Executor, "task-a"),
        )
        .unwrap();
        std::fs::write(
            &paths.subagents_file,
            format!(
                "{}{{\"agent_id\":\"agent-2\"\n",
                std::fs::read_to_string(&paths.subagents_file).unwrap()
            ),
        )
        .unwrap();

        let mut cache = GraphCache::new();
        cache.refresh(&paths, Instant::now(), true);
        assert_eq!(cache.nodes().len(), 1);
        assert_eq!(cache.nodes()[0].agent_id, "agent-1");
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
        let running = RUNNING_FRAMES[0];
        assert_eq!(status_symbol(&Some(Status::Running), running), running);
        assert_eq!(status_symbol(&Some(Status::Done), running), "\u{25cb}");
        assert_eq!(status_symbol(&None, running), "?");
    }

    #[test]
    fn only_the_running_marker_moves() {
        assert_eq!(
            status_symbol(&Some(Status::Done), RUNNING_FRAMES[2]),
            "\u{25cb}"
        );
        assert_eq!(status_symbol(&None, RUNNING_FRAMES[2]), "?");
    }

    #[test]
    fn the_marker_advances_one_frame_per_interval_and_wraps() {
        let interval = Duration::from_millis(300);
        for (index, frame) in RUNNING_FRAMES.iter().enumerate() {
            let elapsed = interval * u32::try_from(index).unwrap();
            assert_eq!(running_frame(elapsed, interval), *frame);
            assert_eq!(running_frame(elapsed + interval * 4, interval), *frame);
        }
    }

    /// The bug (#43): a running node whose marker column is empty is a node
    /// with no status at all, and there is no phase this may happen on -- not
    /// the one the render loop lands on, not the one a two-second sampler does.
    #[test]
    fn a_running_node_always_carries_a_marker() {
        for step in 0..200 {
            let frame = running_frame(Duration::from_millis(step * 50), SPINNER_INTERVAL);
            assert!(
                !status_symbol(&Some(Status::Running), frame)
                    .trim()
                    .is_empty(),
                "blank marker at step {step}"
            );
        }
    }

    /// The other half of #43: the phase froze because the redraw safety net was
    /// an exact multiple of the blink's cycle. Sampling on that same cadence
    /// must still show motion, or the interval has been picked badly again.
    #[test]
    fn the_marker_still_moves_when_sampled_at_the_refresh_cadence() {
        let sampled: std::collections::HashSet<&str> = (0..4)
            .map(|tick| running_frame(GRAPH_REFRESH * tick, SPINNER_INTERVAL))
            .collect();
        assert!(
            sampled.len() > 1,
            "the spinner aliases against GRAPH_REFRESH: {sampled:?}"
        );
    }

    #[test]
    fn a_zero_interval_holds_the_first_frame_rather_than_dividing_by_it() {
        assert_eq!(
            running_frame(Duration::from_millis(1234), Duration::ZERO),
            RUNNING_FRAMES[0]
        );
    }

    /// #44: the pane is a graph, so it is read by structure -- each task's
    /// nodes together, executor above the reviewer that judged it, tasks in
    /// the order they were dispatched. Ordering by `agent_id` scattered all
    /// four of these rows.
    #[test]
    fn nodes_are_grouped_by_task_with_the_executor_above_its_reviewer() {
        let (_tmp, paths) = fixture();
        // Registry order is dispatch order, and deliberately not the order the
        // pane must show: task-b's reviewer went out before task-a's, and the
        // uuid-shaped agent ids sort against both.
        let registry = Registry::from_records(vec![
            record(
                "ffff0000-0000-4000-8000-000000000000",
                Role::Executor,
                "task-a",
            ),
            record(
                "00001111-0000-4000-8000-000000000000",
                Role::Executor,
                "task-b",
            ),
            record(
                "aaaa2222-0000-4000-8000-000000000000",
                Role::Reviewer,
                "task-b",
            ),
            record(
                "11113333-0000-4000-8000-000000000000",
                Role::Reviewer,
                "task-a",
            ),
        ]);

        let nodes = build_nodes(&registry, &paths.logs, &paths.reviews);
        assert_eq!(
            nodes
                .iter()
                .map(|node| (node.task_id.as_str(), role_label(&node.role)))
                .collect::<Vec<_>>(),
            [
                ("task-a", "EXE"),
                ("task-a", "REV"),
                ("task-b", "EXE"),
                ("task-b", "REV"),
            ]
        );
    }

    #[test]
    fn role_labels_are_short_codes() {
        assert_eq!(role_label(&Role::Executor), "EXE");
        assert_eq!(role_label(&Role::Reviewer), "REV");
    }
}
