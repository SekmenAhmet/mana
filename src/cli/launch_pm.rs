use crate::config::{ensure_agent_registered, load_config};
use crate::lock::load_lock;
use crate::monitor::file_watcher::{FsEvent, watch};
use crate::project::{
    ProjectPaths, ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::prompts::pm_prompt;
use crate::pty::{PtySession, RealSpawner, Spawner};
use crate::tui::app::{App, AppMode};
use crate::tui::event::{AppEvent, CrosstermEventSource, EventSource, map_key_event};
use crate::tui::graph::{GraphNode, build_nodes, role_label, status_symbol};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// Given a changed-path event from the file watcher, decides whether the PM
/// should be notified, and builds the exact message to inject into its PTY
/// stdin. Only review files (`reviews/<task-uuid>.md`) trigger a
/// notification in v1 — plain status transitions are visible via the graph
/// pane already and don't need a push message.
pub fn build_notification(event_path: &Path, reviews_dir: &Path) -> Option<String> {
    if event_path.parent()? != reviews_dir {
        return None;
    }
    let task_uuid = event_path.file_stem()?.to_string_lossy().to_string();
    Some(format!(
        "[mana] Review disponible pour {task_uuid} : {}",
        event_path.display()
    ))
}

/// Reads the PM's PTY output on a background thread so the render loop never
/// blocks on it — a blocking `reader.read()` in the main loop would freeze
/// keyboard input and screen redraws for as long as the PM stays silent.
fn spawn_pty_reader(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

pub fn run(agent_cli: &str) -> anyhow::Result<()> {
    let home = mana_home()?;
    let (mut session, paths, _project_name) = prepare_session(&home, &RealSpawner, agent_cli)?;

    let (_fs_watcher, fs_events) = watch(&paths.root)?;
    let pty_output = spawn_pty_reader(session.reader);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let mut events = CrosstermEventSource;
    let result = run_event_loop(
        &mut terminal,
        &mut app,
        &mut session.writer,
        &pty_output,
        &fs_events,
        &paths,
        &mut events,
    );

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Does everything `run` needs before it can touch a real terminal: resolve
/// the project's paths, check the agent is registered, spawn its PTY
/// session and send it the PM prompt. Split out so this — the part with
/// actual decisions in it — is testable against a tempdir `home` and a
/// `pty::test_support::FakeSpawner`, without needing a real terminal.
fn prepare_session(
    home: &Path,
    spawner: &dyn Spawner,
    agent_cli: &str,
) -> anyhow::Result<(PtySession, ProjectPaths, String)> {
    let cwd = std::env::current_dir()?;
    let project_name = project_name_from_dir(&cwd);
    let paths = resolve_project_paths(home, &project_name);
    ensure_project_structure(&paths)?;

    let config = load_config(&home.join("config.yaml"))?;
    ensure_agent_registered(&config, agent_cli)?;

    let mut session = spawner.spawn(agent_cli, &[])?;
    session
        .writer
        .write_all(pm_prompt(&project_name).as_bytes())?;
    session.writer.write_all(b"\n")?;

    Ok((session, paths, project_name))
}

fn run_event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    writer: &mut dyn Write,
    pty_output: &Receiver<Vec<u8>>,
    fs_events: &Receiver<FsEvent>,
    paths: &ProjectPaths,
    events: &mut dyn EventSource,
) -> anyhow::Result<()> {
    loop {
        while let Ok(chunk) = pty_output.try_recv() {
            app.push_output(&chunk);
        }

        while let Ok(FsEvent::Changed(path)) = fs_events.try_recv() {
            if let Some(message) = build_notification(&path, &paths.reviews) {
                writer.write_all(message.as_bytes())?;
                writer.write_all(b"\n")?;
            }
        }

        let lock = load_lock(&paths.lock_file).unwrap_or_default();
        let nodes = build_nodes(&lock, &paths.logs).unwrap_or_default();

        terminal.draw(|frame| draw(frame, app, &nodes))?;

        if let Some(key) = events.poll_key(Duration::from_millis(50))?
            && let Some(app_event) = map_key_event(key.code, key.modifiers)
            && !apply_app_event(app_event, app, writer)?
        {
            return Ok(());
        }
    }
}

/// Applies one already-decoded key event to the app state / the PM's PTY
/// stdin. Returns `Ok(false)` to signal the event loop should quit, `Ok(true)`
/// to keep going. Kept separate from `run_event_loop` (and its real
/// `Terminal`/`event::poll`) so the per-key decision logic is testable with
/// just an in-memory `Write` sink.
fn apply_app_event(event: AppEvent, app: &mut App, writer: &mut dyn Write) -> anyhow::Result<bool> {
    match event {
        AppEvent::Quit => return Ok(false),
        AppEvent::ToggleGraph => app.toggle_graph(),
        AppEvent::Key(c) => app.input.push(c),
        AppEvent::Backspace => {
            app.input.pop();
        }
        AppEvent::Enter => {
            let message = std::mem::take(&mut app.input);
            if !message.is_empty() {
                writer.write_all(message.as_bytes())?;
                writer.write_all(b"\n")?;
            }
        }
    }
    Ok(true)
}

fn draw(frame: &mut ratatui::Frame, app: &App, nodes: &[GraphNode]) {
    let size = frame.area();
    let visible = size.height.saturating_sub(2) as usize;
    let start = app.chat_lines.len().saturating_sub(visible);
    let chat_items: Vec<ListItem> = app.chat_lines[start..]
        .iter()
        .map(|l| ListItem::new(l.clone()))
        .collect();

    let (chat_area, input_area, graph_area) = if app.mode == AppMode::Graph {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(size);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(columns[0]);
        (rows[0], rows[1], Some(columns[1]))
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(size);
        (rows[0], rows[1], None)
    };

    frame.render_widget(
        List::new(chat_items).block(Block::default().borders(Borders::ALL).title("Chat")),
        chat_area,
    );
    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("Input")),
        input_area,
    );

    if let Some(graph_area) = graph_area {
        let graph_items: Vec<ListItem> = nodes
            .iter()
            .map(|n| {
                ListItem::new(format!(
                    "[{}] {} {} — {}",
                    role_label(&n.role),
                    status_symbol(&n.status),
                    n.model,
                    n.task_uuid
                ))
            })
            .collect();
        frame.render_widget(
            List::new(graph_items).block(Block::default().borders(Borders::ALL).title("Graph")),
            graph_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_notification_fires_for_review_file() {
        let reviews_dir = Path::new("/home/x/.mana/projects/demo/reviews");
        let event_path = reviews_dir.join("task-1.md");
        let message = build_notification(&event_path, reviews_dir).unwrap();
        assert!(message.contains("task-1"));
        assert!(message.contains("Review disponible"));
    }

    #[test]
    fn build_notification_ignores_non_review_paths() {
        let reviews_dir = Path::new("/home/x/.mana/projects/demo/reviews");
        let logs_path = Path::new("/home/x/.mana/projects/demo/logs/agent-1.jsonl");
        assert!(build_notification(logs_path, reviews_dir).is_none());
    }

    fn agent_config(path: &str) -> crate::config::AgentConfig {
        crate::config::AgentConfig {
            name: "claude".into(),
            version: "1.0".into(),
            path: path.into(),
        }
    }

    #[test]
    fn prepare_session_errors_when_agent_not_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        crate::config::save_config(&home.join("config.yaml"), &crate::config::Config::default())
            .unwrap();

        let spawner = crate::pty::test_support::FakeSpawner::new(vec![]);
        assert!(prepare_session(home, &spawner, "claude").is_err());
    }

    #[test]
    fn prepare_session_creates_project_structure_and_sends_pm_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("claude".to_string(), agent_config("/usr/local/bin/claude"));
        crate::config::save_config(&home.join("config.yaml"), &config).unwrap();

        let spawner = crate::pty::test_support::FakeSpawner::new(vec![]);
        let (_session, paths, project_name) = prepare_session(home, &spawner, "claude").unwrap();

        assert!(paths.tasks.is_dir());
        assert!(paths.logs.is_dir());
        assert!(paths.reviews.is_dir());

        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "claude");

        let sent = String::from_utf8_lossy(&spawner.written.lock().unwrap()).to_string();
        assert!(sent.contains(&project_name));
        assert!(sent.contains("Project Manager"));
    }

    fn fake_paths(home: &Path) -> ProjectPaths {
        let paths = resolve_project_paths(home, "proj");
        ensure_project_structure(&paths).unwrap();
        paths
    }

    #[test]
    fn run_event_loop_pushes_pty_output_into_app_and_quits_on_scripted_key() {
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let tmp = tempfile::tempdir().unwrap();
        let paths = fake_paths(tmp.path());
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();

        let (pty_tx, pty_rx) = channel();
        pty_tx.send(b"hello from pm".to_vec()).unwrap();
        let (_fs_tx, fs_rx) = channel();
        let mut events = FakeEventSource::new([KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)]);

        run_event_loop(
            &mut terminal,
            &mut app,
            &mut writer,
            &pty_rx,
            &fs_rx,
            &paths,
            &mut events,
        )
        .unwrap();

        assert_eq!(app.chat_lines, vec!["hello from pm".to_string()]);
    }

    #[test]
    fn run_event_loop_writes_notification_on_review_fs_event_then_quits() {
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let tmp = tempfile::tempdir().unwrap();
        let paths = fake_paths(tmp.path());
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();

        let (_pty_tx, pty_rx) = channel();
        let (fs_tx, fs_rx) = channel();
        fs_tx
            .send(FsEvent::Changed(paths.reviews.join("task-1.md")))
            .unwrap();
        let mut events = FakeEventSource::new([KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)]);

        run_event_loop(
            &mut terminal,
            &mut app,
            &mut writer,
            &pty_rx,
            &fs_rx,
            &paths,
            &mut events,
        )
        .unwrap();

        let sent = String::from_utf8_lossy(&writer);
        assert!(sent.contains("task-1"));
        assert!(sent.contains("Review disponible"));
    }

    #[test]
    fn run_event_loop_enter_sends_typed_input_to_writer() {
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let tmp = tempfile::tempdir().unwrap();
        let paths = fake_paths(tmp.path());
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();

        let (_pty_tx, pty_rx) = channel();
        let (_fs_tx, fs_rx) = channel();
        let mut events = FakeEventSource::new([
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ]);

        run_event_loop(
            &mut terminal,
            &mut app,
            &mut writer,
            &pty_rx,
            &fs_rx,
            &paths,
            &mut events,
        )
        .unwrap();

        assert_eq!(writer, b"hi\n");
    }

    #[test]
    fn apply_app_event_quit_returns_false() {
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();
        let keep_going = apply_app_event(AppEvent::Quit, &mut app, &mut writer).unwrap();
        assert!(!keep_going);
    }

    #[test]
    fn apply_app_event_toggle_graph_flips_mode() {
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();
        assert_eq!(app.mode, AppMode::Chat);
        apply_app_event(AppEvent::ToggleGraph, &mut app, &mut writer).unwrap();
        assert_eq!(app.mode, AppMode::Graph);
    }

    #[test]
    fn apply_app_event_key_appends_to_input() {
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();
        apply_app_event(AppEvent::Key('h'), &mut app, &mut writer).unwrap();
        apply_app_event(AppEvent::Key('i'), &mut app, &mut writer).unwrap();
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn apply_app_event_backspace_pops_last_char() {
        let mut app = App::new();
        app.input = "hi".to_string();
        let mut writer: Vec<u8> = Vec::new();
        apply_app_event(AppEvent::Backspace, &mut app, &mut writer).unwrap();
        assert_eq!(app.input, "h");
    }

    #[test]
    fn apply_app_event_enter_writes_input_and_clears_it() {
        let mut app = App::new();
        app.input = "hello pm".to_string();
        let mut writer: Vec<u8> = Vec::new();
        apply_app_event(AppEvent::Enter, &mut app, &mut writer).unwrap();
        assert_eq!(writer, b"hello pm\n");
        assert!(app.input.is_empty());
    }

    #[test]
    fn apply_app_event_enter_on_empty_input_writes_nothing() {
        let mut app = App::new();
        let mut writer: Vec<u8> = Vec::new();
        apply_app_event(AppEvent::Enter, &mut app, &mut writer).unwrap();
        assert!(writer.is_empty());
    }

    #[test]
    fn spawn_pty_reader_forwards_bytes_from_the_reader() {
        let cursor = std::io::Cursor::new(b"hello from the pm's pty".to_vec());
        let rx = spawn_pty_reader(Box::new(cursor));
        let received = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(received, b"hello from the pm's pty");
    }

    #[test]
    fn spawn_pty_reader_closes_channel_when_reader_is_empty() {
        let cursor = std::io::Cursor::new(Vec::new());
        let rx = spawn_pty_reader(Box::new(cursor));
        assert!(rx.recv_timeout(std::time::Duration::from_secs(1)).is_err());
    }

    #[test]
    fn draw_renders_chat_and_input_blocks() {
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.push_output(b"salut");

        terminal.draw(|frame| draw(frame, &app, &[])).unwrap();

        let content =
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .fold(String::new(), |mut acc, cell| {
                    acc.push_str(cell.symbol());
                    acc
                });
        assert!(content.contains("Chat"));
        assert!(content.contains("Input"));
        assert!(content.contains("salut"));
    }

    #[test]
    fn draw_renders_graph_pane_in_graph_mode() {
        use crate::task::Role;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.toggle_graph();
        let nodes = vec![GraphNode {
            agent_uuid: "agent-1".to_string(),
            model: "claude".to_string(),
            role: Role::Executor,
            task_uuid: "task-1".to_string(),
            status: None,
        }];

        terminal.draw(|frame| draw(frame, &app, &nodes)).unwrap();

        let content =
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .fold(String::new(), |mut acc, cell| {
                    acc.push_str(cell.symbol());
                    acc
                });
        assert!(content.contains("Graph"));
        assert!(content.contains("task-1"));
    }
}
