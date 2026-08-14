use crate::config::{Config, load_config};
use crate::lock::load_lock;
use crate::monitor::file_watcher::{FsEvent, watch};
use crate::project::{
    ProjectPaths, ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::prompts::pm_prompt;
use crate::pty;
use crate::tui::app::{App, AppMode};
use crate::tui::event::{AppEvent, map_key_event};
use crate::tui::graph::{GraphNode, build_nodes, role_label, status_symbol};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
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

fn ensure_agent_registered(config: &Config, agent_cli: &str) -> anyhow::Result<()> {
    if config.models.contains_key(agent_cli) {
        Ok(())
    } else {
        anyhow::bail!(
            "agent '{agent_cli}' non enregistre. Lancez 'mana install' pour l'enregistrer."
        )
    }
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
    let cwd = std::env::current_dir()?;
    let project_name = project_name_from_dir(&cwd);
    let paths = resolve_project_paths(&home, &project_name);
    ensure_project_structure(&paths)?;

    let config = load_config(&home.join("config.yaml"))?;
    ensure_agent_registered(&config, agent_cli)?;

    let mut session = pty::spawn(agent_cli, &[])?;
    session
        .writer
        .write_all(pm_prompt(&project_name).as_bytes())?;
    session.writer.write_all(b"\n")?;

    let (_fs_watcher, fs_events) = watch(&paths.root)?;
    let pty_output = spawn_pty_reader(session.reader);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let result = run_event_loop(
        &mut terminal,
        &mut app,
        &mut session.writer,
        &pty_output,
        &fs_events,
        &paths,
    );

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    writer: &mut Box<dyn Write + Send>,
    pty_output: &Receiver<Vec<u8>>,
    fs_events: &Receiver<FsEvent>,
    paths: &ProjectPaths,
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

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            match map_key_event(key.code, key.modifiers) {
                Some(AppEvent::Quit) => return Ok(()),
                Some(AppEvent::ToggleGraph) => app.toggle_graph(),
                Some(AppEvent::Key(c)) => app.input.push(c),
                Some(AppEvent::Backspace) => {
                    app.input.pop();
                }
                Some(AppEvent::Enter) => {
                    let message = std::mem::take(&mut app.input);
                    if !message.is_empty() {
                        writer.write_all(message.as_bytes())?;
                        writer.write_all(b"\n")?;
                    }
                }
                None => {}
            }
        }
    }
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

    #[test]
    fn ensure_agent_registered_rejects_unknown_agent() {
        let config = crate::config::Config::default();
        assert!(ensure_agent_registered(&config, "claude").is_err());
    }

    #[test]
    fn ensure_agent_registered_accepts_known_agent() {
        let mut config = crate::config::Config::default();
        config.models.insert(
            "claude".to_string(),
            crate::config::AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/usr/local/bin/claude".into(),
            },
        );
        assert!(ensure_agent_registered(&config, "claude").is_ok());
    }
}
