//! What the TUI shows, as data: the chat scrollback, the input line, which
//! pane is up, and the last usage figures the PM reported.
//!
//! Everything here is a plain value with no I/O in sight, so the render tests
//! drive it directly and the launch flow only has to push events at it.

use crate::pm::PmEvent;
use std::collections::VecDeque;
use std::time::Instant;

/// How many chat lines the scrollback keeps.
///
/// v1 held a `Vec<String>` that only ever grew: a PM session that ran for an
/// afternoon kept every byte it had ever printed, and nothing above the last
/// screenful was reachable anyway. Two thousand lines is dozens of screens of
/// history at a bounded cost, and the oldest line falling off is exactly what
/// a terminal does.
pub const CHAT_CAPACITY: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Chat,
    Graph,
}

/// Who produced a chat line. Rendering is the only consumer, but the
/// distinction is not cosmetic: a `Raw` line is mana telling the user that
/// the catalogue's event map stopped matching this CLI's stream, and it has
/// to look different from something the PM actually said (design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Assistant prose, extracted through `[pm.events].text`.
    Pm,
    /// A line no path matched. Visible degradation, never silence.
    Raw,
    /// mana's own voice: completion notifications, session notices.
    Mana,
    /// Echoed back so the transcript reads as a conversation -- the PM's
    /// answer arrives minutes later, and a pane that showed only its half
    /// would leave the user guessing what they had asked.
    User,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatLine {
    pub source: Source,
    pub text: String,
}

pub struct App {
    /// A ring: pushing past `CHAT_CAPACITY` drops from the front.
    chat: VecDeque<ChatLine>,
    pub input: String,
    pub mode: AppMode,
    /// Display name of the CLI driving the PM, for the status bar.
    pub cli_name: String,
    /// The latest `PmEvent::Usage`, already summarised. `None` until the PM
    /// finishes a turn -- most CLIs report totals only at the end of one.
    pub usage: Option<String>,
    /// The last `Raw` line seen. When a PM dies, the reason is on its stderr,
    /// which arrives here -- and the chat pane is gone by the time mana can
    /// print anything, so the launch flow reads it back from here.
    pub last_raw: Option<String>,
    /// When this session started, which drives the graph's running blink
    /// without a render-loop clock in the tests (`graph::is_blink_visible`).
    pub started_at: Instant,
}

impl App {
    pub fn new(cli_name: &str) -> Self {
        App {
            chat: VecDeque::new(),
            input: String::new(),
            mode: AppMode::Chat,
            cli_name: cli_name.to_string(),
            usage: None,
            last_raw: None,
            started_at: Instant::now(),
        }
    }

    /// Routes one PM event to wherever it belongs. `Exited` is not handled
    /// here: it ends the session, which is the loop's decision, not the
    /// view's.
    pub fn apply(&mut self, event: &PmEvent) {
        match event {
            PmEvent::Text(text) => self.push(Source::Pm, text),
            PmEvent::Raw(line) => {
                self.last_raw = Some(line.clone());
                self.push(Source::Raw, line);
            }
            PmEvent::Usage(usage) => self.usage = summarize_usage(usage),
            PmEvent::Exited { .. } => {}
        }
    }

    /// Appends text as chat lines, one entry per `\n`-separated line, dropping
    /// the oldest once the ring is full.
    pub fn push(&mut self, source: Source, text: &str) {
        for line in text.split('\n') {
            // A trailing newline is framing, not an empty line worth keeping;
            // blank lines *inside* a paragraph are the PM's own formatting and
            // stay.
            self.chat.push_back(ChatLine {
                source,
                text: line.trim_end_matches('\r').to_string(),
            });
            while self.chat.len() > CHAT_CAPACITY {
                self.chat.pop_front();
            }
        }
        while matches!(self.chat.back(), Some(line) if line.text.is_empty()) {
            self.chat.pop_back();
        }
    }

    /// Oldest first.
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &ChatLine> {
        self.chat.iter()
    }

    pub fn toggle_graph(&mut self) {
        self.mode = match self.mode {
            AppMode::Chat => AppMode::Graph,
            AppMode::Graph => AppMode::Chat,
        };
    }
}

/// One short line of token counts, or `None` when the CLI reported nothing
/// countable.
///
/// Deliberately generic over the shape: `[pm.events].usage` hands over
/// whatever object that CLI happened to emit, and mana promised only to record
/// it (design §4). So every numeric field whose name mentions tokens is shown,
/// in the map's own (sorted) order, and a vendor who names things differently
/// still gets a status bar instead of a special case in this function.
fn summarize_usage(usage: &serde_json::Value) -> Option<String> {
    let fields = usage.as_object()?;
    let parts: Vec<String> = fields
        .iter()
        .filter(|(name, _)| name.contains("token"))
        .filter_map(|(name, value)| {
            let count = value.as_u64()?;
            Some(format!(
                "{} {count}",
                name.trim_end_matches("_tokens").replace('_', " ")
            ))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(app: &App) -> Vec<&str> {
        app.lines().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn pm_text_becomes_one_chat_line_per_newline() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Text("line one\nline two".to_string()));
        assert_eq!(texts(&app), ["line one", "line two"]);
        assert!(app.lines().all(|line| line.source == Source::Pm));
    }

    #[test]
    fn a_trailing_newline_does_not_leave_a_blank_line_behind() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Text("hello\n".to_string()));
        assert_eq!(texts(&app), ["hello"]);
    }

    /// A PM's own paragraph breaks are how its answer reads; only the framing
    /// newline at the very end is noise.
    #[test]
    fn blank_lines_inside_a_paragraph_are_kept() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Text("first\n\nsecond".to_string()));
        assert_eq!(texts(&app), ["first", "", "second"]);
    }

    #[test]
    fn raw_lines_are_marked_as_degraded_and_remembered() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Raw("boom: no credentials".to_string()));
        assert_eq!(app.lines().next().unwrap().source, Source::Raw);
        assert_eq!(app.last_raw.as_deref(), Some("boom: no credentials"));
    }

    /// The v1 defect this type exists to fix: an unbounded buffer.
    #[test]
    fn the_scrollback_is_a_ring_that_drops_its_oldest_line() {
        let mut app = App::new("Fixture CLI");
        for index in 0..CHAT_CAPACITY + 50 {
            app.push(Source::Pm, &format!("line {index}"));
        }
        assert_eq!(app.lines().count(), CHAT_CAPACITY);
        assert_eq!(app.lines().next().unwrap().text, "line 50");
        assert_eq!(
            app.lines().next_back().unwrap().text,
            format!("line {}", CHAT_CAPACITY + 49)
        );
    }

    /// One `push` of a multi-line block must obey the cap too -- a PM that
    /// pastes a whole file in one event would otherwise blow straight past it.
    #[test]
    fn a_single_oversized_push_is_capped_as_well() {
        let mut app = App::new("Fixture CLI");
        let block: Vec<String> = (0..CHAT_CAPACITY + 10)
            .map(|index| format!("line {index}"))
            .collect();
        let block = block.join("\n");
        app.push(Source::Pm, &block);
        assert_eq!(app.lines().count(), CHAT_CAPACITY);
    }

    #[test]
    fn usage_is_summarised_into_the_fields_that_count_tokens() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Usage(json!({
            "input_tokens": 12,
            "output_tokens": 44,
            "cache_read_input_tokens": 900,
            "service_tier": "standard"
        })));
        assert_eq!(
            app.usage.as_deref(),
            Some("cache read input 900 · input 12 · output 44")
        );
    }

    #[test]
    fn a_usage_object_with_nothing_countable_leaves_the_status_bar_alone() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Usage(json!({"service_tier": "standard"})));
        assert!(app.usage.is_none());
        app.apply(&PmEvent::Usage(json!("nonsense")));
        assert!(app.usage.is_none());
    }

    #[test]
    fn the_latest_usage_replaces_the_previous_one() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Usage(json!({"output_tokens": 1})));
        app.apply(&PmEvent::Usage(json!({"output_tokens": 2})));
        assert_eq!(app.usage.as_deref(), Some("output 2"));
    }

    #[test]
    fn toggle_graph_switches_mode_both_ways() {
        let mut app = App::new("Fixture CLI");
        assert_eq!(app.mode, AppMode::Chat);
        app.toggle_graph();
        assert_eq!(app.mode, AppMode::Graph);
        app.toggle_graph();
        assert_eq!(app.mode, AppMode::Chat);
    }

    #[test]
    fn an_exit_event_changes_nothing_in_the_view() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Exited { code: Some(1) });
        assert_eq!(app.lines().count(), 0);
    }
}
