//! What the TUI shows, as data: the chat scrollback, the input line, which
//! pane is up, and the last usage figures the PM reported.
//!
//! Everything here is a plain value with no I/O in sight, so the render tests
//! drive it directly and the launch flow only has to push events at it.

use crate::pm::{PermissionChoice, PmEvent};
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
    /// The PM is blocked on a human decision. Its own source because it is the
    /// one line in the pane that is not a report but a question: it stays on
    /// screen while the PM waits, and the status bar carries the keys.
    Permission,
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
    /// The permission the PM is waiting on, if any. `Some` blocks nothing in
    /// mana -- the loop keeps drawing and the user keeps typing -- but it is
    /// what the status bar and the answer keys act on.
    ///
    /// One at a time on purpose: an agent asks for permission and then waits
    /// for the answer before doing anything else, so a queue would be a
    /// structure that never holds two.
    pub pending_permission: Option<PendingPermission>,
    /// When this session started, which drives the graph's running blink
    /// without a render-loop clock in the tests (`graph::is_blink_visible`).
    pub started_at: Instant,
}

/// A permission request as the interface holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    pub id: u64,
    pub description: String,
    pub options: Vec<PermissionChoice>,
}

impl PendingPermission {
    /// The option a yes or a no maps to, or `None` when the PM offered no way
    /// to say it.
    ///
    /// First match wins, which is the protocol's own order of preference:
    /// agents list the narrow option ("allow once") before the broad one
    /// ("allow always"), and answering for someone else is a decision that
    /// should stay as small as it can.
    pub fn choice(&self, allow: bool) -> Option<&PermissionChoice> {
        self.options.iter().find(|option| option.allows == allow)
    }
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
            pending_permission: None,
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
            // Kept when a snapshot has nothing countable in it rather than
            // blanked: ACP agents report a context-window update mid-turn and
            // the token totals only at the end, so overwriting on every event
            // would erase the figure the moment the next turn started.
            PmEvent::Usage(usage) => {
                if let Some(summary) = summarize_usage(usage) {
                    self.usage = Some(summary);
                }
            }
            PmEvent::PermissionRequest {
                id,
                description,
                options,
            } => {
                // Written into the transcript as well as held as state: the
                // status bar says what the keys are *now*, and the chat line is
                // what remains afterwards to explain why the PM paused.
                self.push(
                    Source::Permission,
                    &format!("the PM asks permission to: {description}"),
                );
                self.pending_permission = Some(PendingPermission {
                    id: *id,
                    description: description.clone(),
                    options: options.clone(),
                });
            }
            PmEvent::Exited { .. } => {}
        }
    }

    /// Clears the pending request and returns it, so the caller can answer the
    /// transport with one of its option ids.
    pub fn take_permission(&mut self) -> Option<PendingPermission> {
        self.pending_permission.take()
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
/// Deliberately generic over the shape: a usage event hands over whatever
/// object that CLI happened to emit, and mana promised only to record it
/// (design §4). So every numeric field whose name mentions tokens is shown,
/// in the map's own (sorted) order, and a vendor who names things differently
/// still gets a status bar instead of a special case in this function.
///
/// Names are normalised to snake_case first, because that is the one thing
/// that genuinely differs without meaning anything: claude reports
/// `output_tokens` and ACP agents report `outputTokens`, and a case-sensitive
/// match would have left half the CLIs with a permanently empty status bar.
fn summarize_usage(usage: &serde_json::Value) -> Option<String> {
    let fields = usage.as_object()?;
    let parts: Vec<String> = fields
        .iter()
        .map(|(name, value)| (snake_case(name), value))
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

/// `cachedReadTokens` -> `cached_read_tokens`; anything already snake_case is
/// only lowercased.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, character) in name.char_indices() {
        if character.is_ascii_uppercase() && index > 0 {
            out.push('_');
        }
        out.extend(character.to_lowercase());
    }
    out
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

    /// ACP agents report `outputTokens`; claude reports `output_tokens`. One
    /// status bar has to serve both without knowing which CLI it is looking at.
    #[test]
    fn camel_case_usage_fields_are_summarised_like_snake_case_ones() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Usage(json!({
            "totalTokens": 81876,
            "inputTokens": 202,
            "outputTokens": 10,
            "cachedReadTokens": 81664,
        })));
        assert_eq!(
            app.usage.as_deref(),
            Some("cached read 81664 · input 202 · output 10 · total 81876")
        );
    }

    /// ACP sends a context-window update mid-turn and the token totals only at
    /// the end, so a snapshot with nothing countable must not wipe the last
    /// one that had something.
    #[test]
    fn a_usage_snapshot_with_nothing_countable_keeps_the_previous_figures() {
        let mut app = App::new("Fixture CLI");
        app.apply(&PmEvent::Usage(json!({"output_tokens": 44})));
        app.apply(&PmEvent::Usage(json!({"used": 81866, "size": 200_000})));
        assert_eq!(app.usage.as_deref(), Some("output 44"));
    }

    fn permission_request() -> PmEvent {
        PmEvent::PermissionRequest {
            id: 7,
            description: "write README.md".to_string(),
            options: vec![
                PermissionChoice {
                    id: "once".to_string(),
                    label: "Allow once".to_string(),
                    allows: true,
                },
                PermissionChoice {
                    id: "always".to_string(),
                    label: "Always allow".to_string(),
                    allows: true,
                },
                PermissionChoice {
                    id: "no".to_string(),
                    label: "Reject".to_string(),
                    allows: false,
                },
            ],
        }
    }

    #[test]
    fn a_permission_request_becomes_both_a_chat_line_and_pending_state() {
        let mut app = App::new("Fixture CLI");
        app.apply(&permission_request());

        let line = app.lines().next_back().unwrap();
        assert_eq!(line.source, Source::Permission);
        assert_eq!(line.text, "the PM asks permission to: write README.md");
        assert_eq!(app.pending_permission.as_ref().unwrap().id, 7);
    }

    /// Agents list the narrow option before the broad one, and answering for
    /// somebody else is a decision that should stay as small as it can.
    #[test]
    fn a_yes_picks_the_first_allowing_option_and_a_no_the_first_refusing_one() {
        let mut app = App::new("Fixture CLI");
        app.apply(&permission_request());
        let pending = app.pending_permission.as_ref().unwrap();
        assert_eq!(pending.choice(true).unwrap().id, "once");
        assert_eq!(pending.choice(false).unwrap().id, "no");
    }

    /// An agent that offered no way to say no. The interface has to notice
    /// rather than send an "allow" because it was the only option there.
    #[test]
    fn an_answer_the_pm_never_offered_is_reported_as_missing() {
        let PmEvent::PermissionRequest {
            id,
            description,
            options,
        } = permission_request()
        else {
            unreachable!()
        };
        let pending = PendingPermission {
            id,
            description,
            options: options.into_iter().filter(|option| option.allows).collect(),
        };
        assert!(pending.choice(false).is_none());
    }

    #[test]
    fn taking_the_permission_clears_it() {
        let mut app = App::new("Fixture CLI");
        app.apply(&permission_request());
        assert_eq!(app.take_permission().unwrap().id, 7);
        assert!(app.pending_permission.is_none());
        assert!(app.take_permission().is_none());
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
