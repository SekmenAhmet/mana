//! Drawing: `App` + graph nodes in, one frame out. No state, no I/O.
//!
//! ```text
//! ┌ ◆ mana · PM opencode ─────────┬ Graph ─────────┐
//! │ mana's own notices, the PM's  │ ◐ [EXE] ...    │
//! │ prose, the user's turns, and  │ ○ [REV] ... ✅ │
//! │ whatever it is blocked on     │                │
//! │ · 42 technical lines (Ctrl+O) │                │
//! ├───────────────────────────────┴────────────────┤
//! │ PM opencode · Ctrl+O raw off · … · output 44   │  status
//! ├─ You ──────────────────────────────────────────┤
//! │ › what the user is typing                      │  input
//! └────────────────────────────────────────────────┘
//! ```
//!
//! The graph pane is hidden until Ctrl+G, so a session that is just a
//! conversation looks like one. The machinery -- reasoning, tool activity,
//! stderr, frames no map matched -- is hidden until Ctrl+O for the same
//! reason, and counted on one line so hiding it is never silence.
//!
//! Every colour on that frame comes from [`super::theme`]; this file names no
//! colour of its own. What it does own is *which* role each element takes --
//! the mark in the corner is the accent, the PM's prose is left uncoloured,
//! and the permission banner is the only thing allowed to be warm.

use super::app::{App, AppMode, ChatLine, PendingPermission, Source};
use super::graph::{GraphNode, SPINNER_INTERVAL, node_body, running_frame, verdict_symbol};
use super::theme;
use crate::review::Decision;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Two columns of gutter per chat line: the marker plus its space. Continuation
/// rows of a wrapped line are indented by the same amount, so a paragraph reads
/// as one block rather than as several unrelated lines.
const GUTTER: usize = 2;

/// The separator between segments of the status bar, as its own span so the
/// punctuation can sit a shade below the things it separates.
const SEPARATOR: &str = " · ";

/// What stands in the gutter of a turn the session has not sent yet. One
/// text-width character, like every other marker in that column, so a queued
/// line does not shift the text it belongs to.
const PENDING_MARKER: &str = "…";

pub fn draw(frame: &mut Frame, app: &App, nodes: &[GraphNode]) {
    let [main, status, input] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    let (chat_area, graph_area) = match app.mode {
        AppMode::Graph => {
            let [chat, graph] =
                Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(main);
            (chat, Some(graph))
        }
        AppMode::Chat => (main, None),
    };

    draw_chat(frame, app, chat_area);
    if let Some(area) = graph_area {
        draw_graph(frame, app, nodes, area);
    }
    draw_status(frame, app, status);
    draw_input(frame, app, input);
}

/// The chat pane, titled with the one thing that is always on screen.
///
/// `◆ mana · PM <cli>` rather than "Chat": the pane is obviously a chat, and
/// the corner of a bordered box is the one place a mark can live permanently
/// without costing a row. It also answers, at a glance and for the whole
/// session, the question a multi-CLI tool has to keep answering -- *which*
/// agent is being talked to.
fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let inner = inner_size(area);
    frame.render_widget(
        Paragraph::new(visible_lines(app, inner.0, inner.1)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::BORDER)
                .title(Span::styled(
                    format!(" {} mana · PM {} ", theme::MARK, app.cli_name),
                    theme::TITLE,
                )),
        ),
        area,
    );
}

/// The last `height` rows of the scrollback, wrapped to `width`.
///
/// Wrapping here rather than handing ratatui a `Wrap` matters: `Wrap` happens
/// after the widget has been given its text, so a long PM paragraph would push
/// the newest lines off the bottom of the pane -- the one thing a chat window
/// must never do. Walking the ring backwards also means a full scrollback
/// costs a screenful of work per frame, not two thousand lines of it.
///
/// `Raw` lines are skipped while the raw view is closed (Ctrl+O) and replaced,
/// at the very bottom, by one dim line counting them. Everything else always
/// renders: the PM's prose, mana's notices, the user's turns, the permission
/// the PM is blocked on.
fn visible_lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line> = Vec::new();
    // Pushed first because the window is built newest-first and reversed, so
    // this ends up on the last row of the pane -- where the eye already is.
    if let Some(summary) = app.raw_summary() {
        rows.extend(render_rows(&summary, &COUNTER, width).into_iter().rev());
        if rows.len() >= height {
            rows.truncate(height);
            rows.reverse();
            return rows;
        }
    }
    for line in app.lines().rev() {
        if line.source == Source::Raw && !app.show_raw {
            continue;
        }
        let wrapped = render_line(line, width);
        for row in wrapped.into_iter().rev() {
            rows.push(row);
            if rows.len() == height {
                rows.reverse();
                return rows;
            }
        }
    }
    rows.reverse();
    rows
}

/// How one chat line looks: what sits in its gutter and how its text reads.
///
/// The two styles are separate because the gutter is chrome and the text is
/// content. That is where mana's violet lives: a coloured marker in a
/// two-column gutter gives the pane its identity on every line, while the
/// words themselves stay whatever colour reads best.
struct Decoration {
    marker: &'static str,
    gutter: Style,
    text: Style,
}

/// The line standing in for everything Ctrl+O hides. Marked like the technical
/// lines it counts -- it stands where they would -- but neither dim nor grey:
/// it is an offer to open them, and an offer nobody can read is not one.
const COUNTER: Decoration = Decoration {
    marker: "·",
    gutter: theme::COUNTER,
    text: theme::COUNTER,
};

/// One chat line as one or more rows: a marker in the gutter, then the text.
///
/// A line still waiting in the queue keeps its own colours and swaps its
/// marker for an ellipsis: the words are the user's either way, and the one
/// thing that differs is whether the PM has them yet. The gutter is the right
/// place for that -- the same column that already distinguishes who spoke --
/// and it costs no colour the theme has not already spent.
fn render_line(line: &ChatLine, width: usize) -> Vec<Line<'static>> {
    let mut decoration = decoration(line.source);
    if line.pending {
        decoration.marker = PENDING_MARKER;
    }
    render_rows(&line.text, &decoration, width)
}

/// Wraps `text` to the pane and hangs `decoration`'s marker off the first row.
fn render_rows(text: &str, decoration: &Decoration, width: usize) -> Vec<Line<'static>> {
    let text_width = width.saturating_sub(GUTTER).max(1);
    let marker = decoration.marker;
    wrap(text, text_width)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let gutter = if index == 0 {
                format!("{marker:<GUTTER$}")
            } else {
                " ".repeat(GUTTER)
            };
            Line::from(vec![
                Span::styled(gutter, decoration.gutter),
                Span::styled(chunk, decoration.text),
            ])
        })
        .collect()
}

/// How each source looks. `Raw` is dimmed and marked because it means the
/// catalogue's event map stopped matching this CLI's stream: the user must be
/// able to see mana degrading rather than wonder why the PM sounds like JSON.
fn decoration(source: Source) -> Decoration {
    match source {
        // The bulk of the pane, and the one thing violet never touches: the
        // PM's answer renders in the terminal's own foreground.
        Source::Pm => Decoration {
            marker: "",
            gutter: theme::PM_TEXT,
            text: theme::PM_TEXT,
        },
        // A violet chevron in front of near-white text: the turn is the user's
        // own words, so it is accented in the gutter rather than in the words.
        Source::User => Decoration {
            marker: "›",
            gutter: theme::INPUT_CARET,
            text: theme::USER_TEXT,
        },
        Source::Mana => Decoration {
            marker: "*",
            gutter: theme::MANA_TEXT,
            text: theme::MANA_TEXT,
        },
        // Warm and bold because it is the only line in the pane that is
        // waiting on the reader rather than informing them.
        Source::Permission => Decoration {
            marker: "?",
            gutter: theme::PERMISSION_TEXT,
            text: theme::PERMISSION_TEXT,
        },
        Source::Raw => Decoration {
            marker: "·",
            gutter: theme::RAW_MARK,
            text: theme::RAW_TEXT,
        },
    }
}

/// Greedy word wrap, falling back to a hard break for a word longer than the
/// pane (a path, a URL, a base64 blob -- all things an agent prints).
fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split(' ') {
        let mut word = word;
        loop {
            let free = width.saturating_sub(row.chars().count());
            let needed = word.chars().count() + usize::from(!row.is_empty());
            if needed <= free {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(word);
                break;
            }
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                continue;
            }
            // An empty row that still cannot hold the word: split it.
            let head: String = word.chars().take(width).collect();
            let consumed = head.len();
            rows.push(head);
            word = &word[consumed..];
        }
    }
    rows.push(row);
    rows
}

fn draw_graph(frame: &mut Frame, app: &App, nodes: &[GraphNode], area: Rect) {
    let running = running_frame(app.started_at.elapsed(), SPINNER_INTERVAL);
    let rows: Vec<Line> = if nodes.is_empty() {
        vec![Line::styled("no dispatches yet", theme::GRAPH_EMPTY)]
    } else {
        nodes
            .iter()
            // Two spans, because the verdict wears the one pair of semantic
            // colours in the interface: green for pass, rose for fail,
            // readable without a legend -- and never violet, so "did it
            // work?" is not answered by the brand.
            .map(|node| {
                let verdict_style = match node.verdict {
                    Some(Decision::Validated) => theme::VERDICT_OK,
                    Some(Decision::Rejected) => theme::VERDICT_FAIL,
                    None => theme::GRAPH_NODE,
                };
                Line::from(vec![
                    Span::styled(node_body(node, running), theme::GRAPH_NODE),
                    Span::styled(verdict_symbol(&node.verdict), verdict_style),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::BORDER)
                .title(Span::styled(" Graph ", theme::TITLE_MINOR)),
        ),
        area,
    );
}

/// One line, no border: which CLI is driving the PM, what the last turn cost,
/// and the keys that do anything.
///
/// Three roles share the line and are coloured apart, because they are read at
/// different moments: the keys (lavender) are what the user acts on, the
/// figures (plum) are what they glance at, and the separators (deepest violet)
/// are punctuation. The state of a toggle is coloured on top of that -- `on` in
/// the accent, `off` muted -- so "am I showing the machinery?" is answerable
/// from the corner of an eye rather than by reading a word.
///
/// While the PM is waiting on a permission the whole line says so instead, in
/// warm peach: the agent is blocked until somebody answers, and burying that
/// behind the usual usage figures is how a session looks hung.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.pending_permission {
        Some(pending) => Line::styled(
            format!(
                " PERMISSION: {} · {} · {}",
                pending.description,
                key_hint(pending, true, "Ctrl+Y"),
                key_hint(pending, false, "Ctrl+N"),
            ),
            theme::STATUS_ALERT,
        ),
        // Keys first, figures last, because this line is truncated and not
        // wrapped: an ACP agent reports five token counters (68 columns of
        // them on opencode), which pushed `Ctrl+C quit` off the end of a
        // 120-column terminal. What a truncated status bar loses should be the
        // arithmetic, never the way out. The PM's name is not repeated here —
        // the chat pane's title already carries it, and those columns are
        // better spent on the figures.
        None => Line::from(
            vec![
                Span::styled(" Ctrl+G graph", theme::STATUS_KEY),
                Span::styled(SEPARATOR, theme::STATUS_SEPARATOR),
                // Named rather than implied: the pane hides most of what the
                // session produced, and a key nobody knows about is a feature
                // nobody has.
                Span::styled("Ctrl+O raw ", theme::STATUS_KEY),
                if app.show_raw {
                    Span::styled("on", theme::STATUS_ON)
                } else {
                    Span::styled("off", theme::STATUS_OFF)
                },
                Span::styled(SEPARATOR, theme::STATUS_SEPARATOR),
                Span::styled("Ctrl+C quit", theme::STATUS_KEY),
                Span::styled(SEPARATOR, theme::STATUS_SEPARATOR),
            ]
            .into_iter()
            // Between the keys and the figures, and absent entirely when the queue
            // is empty (which is most of a session): it is the one fact on this
            // line that is about to change, and a truncated status bar should lose
            // arithmetic rather than the answer to "where did my message go?".
            .chain(queued_segment(app.queued))
            .chain([Span::styled(
                app.usage
                    .clone()
                    .unwrap_or_else(|| "no usage reported yet".to_string()),
                theme::STATUS_FACT,
            )])
            .collect::<Vec<Span>>(),
        ),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// `N queued · `, or nothing at all.
fn queued_segment(queued: usize) -> Vec<Span<'static>> {
    if queued == 0 {
        return Vec::new();
    }
    vec![
        Span::styled(format!("{queued} queued"), theme::STATUS_ON),
        Span::styled(SEPARATOR, theme::STATUS_SEPARATOR),
    ]
}

/// The agent's own wording for a key, so the operator approves the thing the
/// agent named rather than mana's paraphrase of it.
fn key_hint(pending: &PendingPermission, allow: bool, key: &str) -> String {
    match pending.choice(allow) {
        Some(choice) => format!("{key} {}", choice.label),
        // An agent that offered no way to say no. Saying so beats offering a
        // key that would do nothing.
        None => format!("{key} unavailable"),
    }
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            // The caret is the accent; what is typed next to it is not. The
            // same split as the chat pane's gutter, for the same reason.
            Span::styled("› ", theme::INPUT_CARET),
            Span::styled(app.input.clone(), theme::INPUT_TEXT),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::BORDER)
                .title(Span::styled(" You ", theme::TITLE_MINOR)),
        ),
        area,
    );
}

/// A bordered block's usable interior, in characters.
///
/// A conversion, not a reimplementation of `Rect::inner` (#68): the layout
/// speaks `Rect` (`u16` cells) and everything below speaks `usize` chars, so
/// the cast has to happen somewhere, and once here beats twice per caller.
/// The `.max(1)` floor is belt-and-braces -- `render_rows` re-guards the
/// width it is handed -- but it keeps "a pane is at least one column wide"
/// true at the boundary rather than in whichever consumer remembers to.
fn inner_size(area: Rect) -> (usize, usize) {
    (
        area.width.saturating_sub(2).max(1) as usize,
        area.height.saturating_sub(2) as usize,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Status;
    use crate::review::Decision;
    use crate::task::Role;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;

    fn node(
        role: Role,
        task: &str,
        status: Option<Status>,
        verdict: Option<Decision>,
    ) -> GraphNode {
        GraphNode {
            agent_id: format!("agent-{task}"),
            role,
            cli: "claude".to_string(),
            model: "haiku".to_string(),
            task_id: task.to_string(),
            status,
            verdict,
        }
    }

    /// Renders one frame and hands back the cells, styles and all.
    fn buffer(app: &App, nodes: &[GraphNode], width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app, nodes)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Renders one frame and returns the screen as text, one string per row.
    fn screen(app: &App, nodes: &[GraphNode], width: u16, height: u16) -> Vec<String> {
        rows_of(&buffer(app, nodes, width, height), width, height)
    }

    fn rows_of(buffer: &Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn dump(rows: &[String]) -> String {
        rows.join("\n")
    }

    /// The style of the cell where `needle` starts, found by reading the
    /// screen the way the user does rather than by counting columns -- a test
    /// that hard-codes coordinates only survives until the next line moves.
    fn style_of(buffer: &Buffer, width: u16, height: u16, needle: &str) -> Style {
        let rows = rows_of(buffer, width, height);
        let (x, y) = rows
            .iter()
            .enumerate()
            .find_map(|(y, row)| {
                row.find(needle)
                    .map(|byte| (row[..byte].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", dump(&rows)));
        buffer[(x, y)].style()
    }

    #[test]
    fn the_chat_pane_shows_the_conversation_and_the_status_line() {
        let mut app = App::new("Claude Code");
        app.push(Source::User, "add a healthcheck endpoint");
        app.push(Source::Pm, "Breaking that into one task.");
        app.push(Source::Mana, "[mana] executor finished for task 3f2a1b6c");
        app.usage = Some("output 44".to_string());
        app.input = "and a test".to_string();

        let rows = screen(&app, &[], 100, 12);
        let rendered = dump(&rows);
        // The mark and which CLI is answering, in the corner, all session.
        assert!(rendered.contains("◆ mana · PM Claude Code"), "{rendered}");
        assert!(
            rendered.contains("› add a healthcheck endpoint"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Breaking that into one task."),
            "{rendered}"
        );
        assert!(
            rendered.contains("* [mana] executor finished"),
            "{rendered}"
        );
        // The keys come first and the figures last: a narrow terminal that has
        // to lose something loses the arithmetic, not the way out. The PM's
        // name lives in the pane title alone — the bar spends its columns on
        // keys and figures.
        assert!(
            rendered.contains("Ctrl+G graph · Ctrl+O raw off · Ctrl+C quit"),
            "{rendered}"
        );
        assert!(rendered.contains("output 44"), "{rendered}");
        assert!(rendered.contains("› and a test"), "{rendered}");
        // The graph is hidden until asked for.
        assert!(!rendered.contains("Graph"), "{rendered}");
    }

    /// The queue, as the operator sees it: their words are in the transcript
    /// the moment they typed them, marked as not yet handed over, and the bar
    /// says how many are waiting.
    #[test]
    fn queued_turns_are_marked_in_the_pane_and_counted_in_the_status_bar() {
        let mut app = App::new("Claude Code");
        app.push(Source::User, "add a healthcheck endpoint");
        app.push_pending(Source::User, "and a test for it");
        app.push_pending(Source::User, "then open the PR");
        app.queued = 2;
        app.usage = Some("output 44".to_string());

        let rendered = dump(&screen(&app, &[], 100, 12));
        // Sent: the usual chevron. Waiting: an ellipsis in the same column, so
        // the text does not shift when it finally goes out.
        assert!(
            rendered.contains("› add a healthcheck endpoint"),
            "{rendered}"
        );
        assert!(rendered.contains("… and a test for it"), "{rendered}");
        assert!(rendered.contains("… then open the PR"), "{rendered}");
        // Between the keys and the figures.
        assert!(
            rendered.contains("Ctrl+C quit · 2 queued · output 44"),
            "{rendered}"
        );
    }

    /// Nothing waiting, nothing said: the bar reads exactly as it did before
    /// the queue existed, which is what most of a session looks like.
    #[test]
    fn an_empty_queue_says_nothing_at_all() {
        let app = App::new("Claude Code");
        let rendered = dump(&screen(&app, &[], 100, 8));
        assert!(!rendered.contains("queued"), "{rendered}");
        assert!(
            rendered.contains("Ctrl+C quit · no usage reported yet"),
            "{rendered}"
        );
    }

    /// A released turn loses its mark in place, rather than being written into
    /// the transcript a second time.
    #[test]
    fn releasing_a_queued_turn_restores_the_ordinary_user_marker() {
        let mut app = App::new("Claude Code");
        app.push_pending(Source::User, "and a test for it");
        app.release_pending();
        app.queued = 0;

        let rendered = dump(&screen(&app, &[], 100, 8));
        assert!(rendered.contains("› and a test for it"), "{rendered}");
        assert!(!rendered.contains("… and a test"), "{rendered}");
    }

    /// The status bar is truncated, not wrapped, and an ACP agent's usage
    /// string is long enough to push the keys off the end of a real terminal
    /// (opencode reports five counters -- 68 columns of them).
    #[test]
    fn a_long_usage_string_cannot_push_the_keys_off_the_status_bar() {
        let mut app = App::new("opencode");
        app.usage = Some(
            "cached read 84352 · input 357 · output 29 · thought 57 · total 84795".to_string(),
        );
        let rendered = dump(&screen(&app, &[], 120, 8));
        assert!(rendered.contains("Ctrl+O raw off"), "{rendered}");
        assert!(rendered.contains("Ctrl+C quit"), "{rendered}");
    }

    /// The v2.1 content rule, in one frame: the two lines the operator has to
    /// read are on screen, and the machinery around them is one dim counter.
    #[test]
    fn technical_lines_collapse_to_one_counter_until_the_raw_view_is_opened() {
        let mut app = App::new("opencode");
        app.push(Source::Raw, "session initialized");
        app.push(Source::Pm, "What are we building?");
        for line in [
            "The user wants me to",
            "⚙ mana_list_agents …",
            "⚙ mana_list_agents ✓",
        ] {
            app.push(Source::Raw, line);
        }
        app.push(Source::Mana, "[mana] executor finished for task 3f2a1b6c");

        let closed = dump(&screen(&app, &[], 100, 12));
        assert!(closed.contains("What are we building?"), "{closed}");
        assert!(closed.contains("* [mana] executor finished"), "{closed}");
        assert!(closed.contains("· 4 technical lines (Ctrl+O)"), "{closed}");
        // None of the collapsed lines leaked through.
        for hidden in [
            "session initialized",
            "The user wants me to",
            "mana_list_agents",
        ] {
            assert!(!closed.contains(hidden), "{hidden} rendered: {closed}");
        }
        // ...and the key that opens them is named where the keys live.
        assert!(closed.contains("Ctrl+O raw off"), "{closed}");

        app.toggle_raw();
        let open = dump(&screen(&app, &[], 100, 12));
        for shown in [
            "· session initialized",
            "· The user wants me to",
            "· ⚙ mana_list_agents …",
            "· ⚙ mana_list_agents ✓",
        ] {
            assert!(open.contains(shown), "{shown} missing: {open}");
        }
        // The counter is gone: the lines themselves are the answer now.
        assert!(!open.contains("technical lines (Ctrl+O)"), "{open}");
        assert!(open.contains("Ctrl+O raw on"), "{open}");
        // What always renders, renders in both states.
        assert!(open.contains("What are we building?"), "{open}");
    }

    /// The summary is the newest thing in the pane, so it comes after the
    /// conversation rather than above it -- a counter the eye has to hunt for
    /// is a counter nobody reads.
    #[test]
    fn the_counter_comes_after_everything_else_in_the_chat_pane() {
        let mut app = App::new("opencode");
        app.push(Source::Raw, "hidden");
        app.push(Source::Pm, "first");
        let rows = screen(&app, &[], 40, 8);
        // The chat pane is everything between its own two borders.
        let chat: Vec<&String> = rows
            .iter()
            .take_while(|row| !row.starts_with('└'))
            .filter(|row| row.starts_with('│') && !row.trim_matches(['│', ' ']).is_empty())
            .collect();
        let last = chat.last().expect("the chat pane rendered something");
        assert!(last.contains("· 1 technical line (Ctrl+O)"), "{rows:#?}");
    }

    /// A pane with nothing hidden says nothing about hiding: a session that
    /// never degraded must look like a plain conversation.
    #[test]
    fn no_technical_lines_means_no_counter_at_all() {
        let mut app = App::new("opencode");
        app.push(Source::Pm, "hello");
        let rendered = dump(&screen(&app, &[], 40, 8));
        assert!(!rendered.contains("technical"), "{rendered}");
    }

    /// Visible degradation (design §4): a line the event map did not match is
    /// marked and dimmed, never dropped and never mistaken for the PM talking.
    #[test]
    fn a_raw_line_is_marked_and_dimmed() {
        let mut app = App::new("Claude Code");
        app.show_raw = true;
        app.push(Source::Raw, "boom: no credentials found");

        let buffer = buffer(&app, &[], 40, 8);
        let row: String = (0..40).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(row.contains("· boom: no credentials found"), "{row}");
        // The style is the other half of "visible": both the marker (column 1)
        // and the text are the muted violet-grey the theme keeps for
        // machinery, and only the text is dimmed -- the marker stays a shade up
        // so the line reads as *marked* rather than as one that failed to
        // render.
        assert_eq!(buffer[(1, 1)].style().fg, Some(theme::MIST));
        assert!(!buffer[(1, 1)].style().add_modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(3, 1)].style().fg, Some(theme::MIST));
        assert!(
            buffer[(5, 1)].style().add_modifier.contains(Modifier::DIM),
            "{:?}",
            buffer[(5, 1)].style()
        );
    }

    /// The permission prompt, in both places it lives: a marked line in the
    /// transcript and the keys taking over the status bar. An agent is blocked
    /// until somebody answers, so this must not look like ordinary chatter.
    #[test]
    fn a_pending_permission_takes_over_the_status_bar_and_marks_its_chat_line() {
        let mut app = App::new("opencode");
        app.usage = Some("output 44".to_string());
        app.apply(&crate::pm::PmEvent::PermissionRequest {
            id: 1,
            description: "write README.md".to_string(),
            options: vec![
                crate::pm::PermissionChoice {
                    id: "once".to_string(),
                    label: "Allow once".to_string(),
                    allows: true,
                },
                crate::pm::PermissionChoice {
                    id: "no".to_string(),
                    label: "Reject".to_string(),
                    allows: false,
                },
            ],
        });

        let rows = screen(&app, &[], 78, 10);
        let rendered = dump(&rows);
        assert!(
            rendered.contains("? the PM asks permission to: write README.md"),
            "{rendered}"
        );
        // The status line names the agent's own options, not mana's paraphrase.
        assert!(
            rendered.contains("PERMISSION: write README.md · Ctrl+Y Allow once · Ctrl+N Reject"),
            "{rendered}"
        );
        // ...and the usual status content is gone while the PM is waiting.
        assert!(!rendered.contains("output 44"), "{rendered}");

        // Warm, because it is the one thing on screen waiting on the reader --
        // and the one place the interface leaves its violet, so "somebody has
        // to answer" cannot be mistaken for mana talking about itself.
        let buffer = buffer(&app, &[], 78, 10);
        let banner = style_of(&buffer, 78, 10, "PERMISSION:");
        assert_eq!(banner.fg, Some(theme::EMBER));
        assert!(banner.add_modifier.contains(Modifier::BOLD));
        // Both halves of the chat line it left behind, too.
        assert_eq!(
            style_of(&buffer, 78, 10, "? the PM asks").fg,
            Some(theme::EMBER)
        );
        assert_eq!(
            style_of(&buffer, 78, 10, "the PM asks permission").fg,
            Some(theme::EMBER)
        );
    }

    /// An agent that offered no way to refuse must not be answered by a key
    /// that silently does nothing.
    #[test]
    fn a_missing_answer_is_named_in_the_status_bar_rather_than_offered() {
        let mut app = App::new("opencode");
        app.pending_permission = Some(PendingPermission {
            id: 1,
            description: "run tests".to_string(),
            options: vec![crate::pm::PermissionChoice {
                id: "once".to_string(),
                label: "Allow once".to_string(),
                allows: true,
            }],
        });
        let rendered = dump(&screen(&app, &[], 78, 10));
        assert!(rendered.contains("Ctrl+N unavailable"), "{rendered}");
    }

    #[test]
    fn the_graph_pane_shows_a_node_per_dispatch_with_its_verdict() {
        let mut app = App::new("Claude Code");
        app.toggle_graph();
        let nodes = [
            node(
                Role::Executor,
                "3f2a1b6c-0000-4000-8000-000000000000",
                Some(Status::Done),
                Some(Decision::Validated),
            ),
            node(Role::Reviewer, "9c0d1e2f", Some(Status::Running), None),
        ];

        let rendered = dump(&screen(&app, &nodes, 100, 12));
        assert!(rendered.contains("Graph"), "{rendered}");
        assert!(
            rendered.contains("[EXE] claude/haiku 3f2a1b6c ✓"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[REV] claude/haiku 9c0d1e2f"),
            "{rendered}"
        );
        // The chat pane is still there, narrower, mark and all.
        assert!(rendered.contains("◆ mana · PM Claude Code"), "{rendered}");
    }

    /// The identity pass, in one frame: every element that carries a colour
    /// carries the role the theme gave it. Compared against the theme's names
    /// rather than raw indexes on purpose -- `theme` pins the indexes once, and
    /// this pins which element wears which of them.
    #[test]
    fn every_element_wears_the_role_the_theme_gave_it() {
        let mut app = App::new("Claude Code");
        app.push(Source::User, "add a healthcheck endpoint");
        app.push(Source::Pm, "Breaking that into one task.");
        app.push(Source::Mana, "[mana] executor finished for task 3f2a1b6c");
        app.push(Source::Raw, "hidden machinery");
        app.usage = Some("output 44".to_string());
        app.input = "and a test".to_string();

        let (width, height) = (100, 14);
        let buffer = buffer(&app, &[], width, height);
        let style = |needle: &str| style_of(&buffer, width, height, needle);

        // The mark and its title: the accent, and the only bold one.
        assert_eq!(style("◆").fg, Some(theme::MANA));
        assert_eq!(style("mana · PM Claude Code").fg, Some(theme::MANA));
        assert!(style("◆").add_modifier.contains(Modifier::BOLD));
        // The frame around it stays out of the way.
        assert_eq!(style("┌").fg, Some(theme::DUSK));
        assert_eq!(style("└").fg, Some(theme::DUSK));

        // mana's own voice, in mana's own colour (it used to be cyan).
        assert_eq!(style("[mana] executor").fg, Some(theme::MANA));
        // The user's turn: a violet chevron in front of near-white text.
        assert_eq!(style("›").fg, Some(theme::MANA));
        assert_eq!(style("add a healthcheck").fg, Some(theme::GLOW));
        // The PM's prose keeps the terminal's own foreground. This is the
        // readability constraint the whole palette is built around: if this
        // assertion ever needs updating, the change is wrong.
        assert_eq!(
            style("Breaking that into").fg,
            Some(theme::TERMINAL_DEFAULT)
        );

        // The counter is readable, not dim: it is an offer, not debris.
        assert_eq!(
            style("1 technical line").fg,
            Some(theme::COUNTER.fg.unwrap())
        );
        assert!(
            !style("1 technical line")
                .add_modifier
                .contains(Modifier::DIM)
        );

        // Status bar, segment by segment: keys, state, separators, figures.
        assert_eq!(style("Ctrl+C quit").fg, Some(theme::LAVENDER));
        // The CLI's name lives in the pane title alone; the bar's only facts
        // are the figures.
        assert_eq!(style("output 44").fg, Some(theme::PLUM));
        assert_eq!(style(" · Ctrl+O").fg, Some(theme::SHADOW));

        // The input line: accented caret, plain text, minor title.
        assert_eq!(style("You").fg, Some(theme::LAVENDER));
        assert_eq!(style("and a test").fg, Some(theme::TERMINAL_DEFAULT));
    }

    /// The toggle has to be readable as a state, not just as a word: `on`
    /// takes the accent, `off` stays muted.
    #[test]
    fn the_raw_toggle_looks_different_when_it_is_on() {
        let mut app = App::new("opencode");
        let (width, height) = (80, 8);

        let off = style_of(&buffer(&app, &[], width, height), width, height, "off");
        assert_eq!(off.fg, Some(theme::PLUM));
        assert!(!off.add_modifier.contains(Modifier::BOLD));

        app.toggle_raw();
        let on = style_of(&buffer(&app, &[], width, height), width, height, "on");
        assert_eq!(on.fg, Some(theme::MANA));
        assert!(on.add_modifier.contains(Modifier::BOLD));
    }

    /// The verdicts are the one thing the theme does not get to touch: ✅ and
    /// ❌ mean pass and fail without a legend, and a branded tick would turn
    /// the answer into a decoration.
    #[test]
    fn the_graph_is_themed_but_its_verdicts_wear_semantic_colours() {
        let mut app = App::new("Claude Code");
        app.toggle_graph();
        let nodes = [
            node(
                Role::Executor,
                "3f2a1b6c",
                Some(Status::Done),
                Some(Decision::Validated),
            ),
            node(
                Role::Reviewer,
                "9c0d1e2f",
                Some(Status::Done),
                Some(Decision::Rejected),
            ),
        ];
        let (width, height) = (100, 12);
        let buffer = buffer(&app, &nodes, width, height);

        assert_eq!(
            style_of(&buffer, width, height, "Graph").fg,
            Some(theme::LAVENDER)
        );
        assert_eq!(
            style_of(&buffer, width, height, "[EXE]").fg,
            Some(theme::GRAPH_NODE.fg.unwrap())
        );
        assert_eq!(
            style_of(&buffer, width, height, "✓").fg,
            theme::VERDICT_OK.fg
        );
        assert_eq!(
            style_of(&buffer, width, height, "✗").fg,
            theme::VERDICT_FAIL.fg
        );
    }

    #[test]
    fn an_empty_graph_says_so_rather_than_showing_an_empty_box() {
        let mut app = App::new("Claude Code");
        app.toggle_graph();
        let rendered = dump(&screen(&app, &[], 80, 10));
        assert!(rendered.contains("no dispatches yet"), "{rendered}");
    }

    /// The newest line is the one that must always be on screen -- the reason
    /// wrapping happens before the window is chosen rather than after.
    #[test]
    fn the_newest_lines_win_when_the_pane_is_too_short() {
        let mut app = App::new("Claude Code");
        for index in 0..40 {
            app.push(Source::Pm, &format!("line {index}"));
        }
        let rows = screen(&app, &[], 40, 8);
        let rendered = dump(&rows);
        assert!(rendered.contains("line 39"), "{rendered}");
        assert!(!rendered.contains("line 30"), "{rendered}");
    }

    #[test]
    fn a_long_paragraph_wraps_instead_of_being_cut_off() {
        let mut app = App::new("Claude Code");
        app.push(
            Source::Pm,
            "the reviewer rejected the task because the acceptance criteria were not met",
        );
        let rendered = dump(&screen(&app, &[], 30, 12));
        // Every word survives, across as many rows as it takes.
        assert!(rendered.contains("the reviewer rejected the"), "{rendered}");
        assert!(rendered.contains("acceptance criteria were"), "{rendered}");
        assert!(rendered.contains("not met"), "{rendered}");
    }

    #[test]
    fn wrap_breaks_on_spaces_and_hard_breaks_a_word_that_cannot_fit() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("/very/long/path", 5), ["/very", "/long", "/path"]);
        // A long word after a short one starts on its own row.
        assert_eq!(
            wrap("a /very/long/path", 6),
            ["a", "/very/", "long/p", "ath"]
        );
    }

    /// Multi-byte text must not be cut mid-character, and the hard break is
    /// the one place that indexes into a word.
    #[test]
    fn wrap_hard_breaks_on_character_boundaries() {
        assert_eq!(wrap("ééééé", 2), ["éé", "éé", "é"]);
    }

    #[test]
    fn a_pane_too_small_to_hold_anything_still_renders() {
        let app = App::new("Claude Code");
        // Four rows is one status line, a three-row input box and nothing
        // left for the chat: a terminal this size must not panic.
        screen(&app, &[], 8, 4);
    }
}
