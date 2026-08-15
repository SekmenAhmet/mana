//! Drawing: `App` + graph nodes in, one frame out. No state, no I/O.
//!
//! ```text
//! ┌ chat ─────────────────────────┬ graph (Ctrl+G) ┐
//! │ mana's own notices, the PM's  │ ◉ [EXE] ...    │
//! │ prose, the user's turns, and  │ ○ [REV] ... ✅ │
//! │ raw lines when a stream stops │                │
//! │ matching the catalogue        │                │
//! ├───────────────────────────────┴────────────────┤
//! │ PM Claude Code · output 44 · Ctrl+G · Ctrl+C   │  status
//! ├────────────────────────────────────────────────┤
//! │ > what the user is typing                      │  input
//! └────────────────────────────────────────────────┘
//! ```
//!
//! The graph pane is hidden until Ctrl+G, so a session that is just a
//! conversation looks like one.

use super::app::{App, AppMode, ChatLine, PendingPermission, Source};
use super::graph::{BLINK_INTERVAL, GraphNode, is_blink_visible, node_line};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Two columns of gutter per chat line: the marker plus its space. Continuation
/// rows of a wrapped line are indented by the same amount, so a paragraph reads
/// as one block rather than as several unrelated lines.
const GUTTER: usize = 2;

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

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let inner = inner_size(area);
    frame.render_widget(
        Paragraph::new(visible_lines(app, inner.0, inner.1))
            .block(Block::default().borders(Borders::ALL).title("Chat")),
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
fn visible_lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line> = Vec::new();
    for line in app.lines().rev() {
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

/// One chat line as one or more rows: a marker in the gutter, then the text.
fn render_line(line: &ChatLine, width: usize) -> Vec<Line<'static>> {
    let (marker, style) = decoration(line.source);
    let text_width = width.saturating_sub(GUTTER).max(1);
    wrap(&line.text, text_width)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let gutter = if index == 0 {
                format!("{marker:<GUTTER$}")
            } else {
                " ".repeat(GUTTER)
            };
            Line::from(vec![
                Span::styled(gutter, Style::default().fg(Color::DarkGray)),
                Span::styled(chunk, style),
            ])
        })
        .collect()
}

/// How each source looks. `Raw` is dimmed and marked because it means the
/// catalogue's event map stopped matching this CLI's stream: the user must be
/// able to see mana degrading rather than wonder why the PM sounds like JSON.
fn decoration(source: Source) -> (&'static str, Style) {
    match source {
        Source::Pm => ("", Style::default()),
        Source::User => ("›", Style::default().add_modifier(Modifier::BOLD)),
        Source::Mana => ("*", Style::default().fg(Color::Cyan)),
        // Yellow and bold because it is the only line in the pane that is
        // waiting on the reader rather than informing them.
        Source::Permission => (
            "?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Source::Raw => (
            "·",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
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
    let blink = is_blink_visible(app.started_at.elapsed(), BLINK_INTERVAL);
    let rows: Vec<Line> = if nodes.is_empty() {
        vec![Line::styled(
            "no dispatches yet",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        nodes
            .iter()
            .map(|node| Line::from(node_line(node, blink)))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title("Graph")),
        area,
    );
}

/// One line, no border: which CLI is driving the PM, what the last turn cost,
/// and the keys that do anything.
///
/// While the PM is waiting on a permission it says so instead, in full width
/// and in yellow: the agent is blocked until somebody answers, and burying
/// that behind the usual usage figures is how a session looks hung.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (text, style) = match &app.pending_permission {
        Some(pending) => (
            format!(
                " PERMISSION: {} · {} · {}",
                pending.description,
                key_hint(pending, true, "Ctrl+Y"),
                key_hint(pending, false, "Ctrl+N"),
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        None => (
            format!(
                " PM {} · {} · Ctrl+G graph · Ctrl+C quit",
                app.cli_name,
                app.usage.as_deref().unwrap_or("no usage reported yet")
            ),
            Style::default().fg(Color::DarkGray),
        ),
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
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
        Paragraph::new(format!("› {}", app.input))
            .block(Block::default().borders(Borders::ALL).title("You")),
        area,
    );
}

/// A bordered block's usable interior.
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

    /// Renders one frame and returns the screen as text, one string per row.
    fn screen(app: &App, nodes: &[GraphNode], width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app, nodes)).unwrap();
        let buffer = terminal.backend().buffer().clone();
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

    #[test]
    fn the_chat_pane_shows_the_conversation_and_the_status_line() {
        let mut app = App::new("Claude Code");
        app.push(Source::User, "add a healthcheck endpoint");
        app.push(Source::Pm, "Breaking that into one task.");
        app.push(Source::Mana, "[mana] executor finished for task 3f2a1b6c");
        app.usage = Some("output 44".to_string());
        app.input = "and a test".to_string();

        let rows = screen(&app, &[], 60, 12);
        let rendered = dump(&rows);
        assert!(rendered.contains("Chat"), "{rendered}");
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
        assert!(
            rendered.contains("PM Claude Code · output 44"),
            "{rendered}"
        );
        assert!(rendered.contains("› and a test"), "{rendered}");
        // The graph is hidden until asked for.
        assert!(!rendered.contains("Graph"), "{rendered}");
    }

    /// Visible degradation (design §4): a line the event map did not match is
    /// marked and dimmed, never dropped and never mistaken for the PM talking.
    #[test]
    fn a_raw_line_is_marked_and_dimmed() {
        let mut app = App::new("Claude Code");
        app.push(Source::Raw, "boom: no credentials found");

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let row: String = (0..40).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(row.contains("· boom: no credentials found"), "{row}");
        // The style is the other half of "visible": the marker column is grey
        // and the text is dim.
        assert_eq!(buffer[(3, 1)].style().fg, Some(Color::DarkGray));
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

        // Yellow, because it is the one line waiting on the reader.
        let mut terminal = Terminal::new(TestBackend::new(78, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert_eq!(buffer[(1, 6)].style().fg, Some(Color::Yellow));
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
            rendered.contains("[EXE] claude/haiku 3f2a1b6c ✅"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[REV] claude/haiku 9c0d1e2f"),
            "{rendered}"
        );
        // The chat pane is still there, narrower.
        assert!(rendered.contains("Chat"), "{rendered}");
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
