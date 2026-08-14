use crate::monitor::pty_listener::strip_ansi;

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Chat,
    Graph,
}

pub struct App {
    pub chat_lines: Vec<String>,
    pub input: String,
    pub mode: AppMode,
}

impl App {
    pub fn new() -> Self {
        App {
            chat_lines: Vec::new(),
            input: String::new(),
            mode: AppMode::Chat,
        }
    }

    /// Appends raw PTY output (ANSI stripped) as chat lines, one entry per
    /// `\n`-terminated line. A trailing partial line (no newline yet) is
    /// pushed as its own entry too — v1 doesn't merge it with later output.
    pub fn push_output(&mut self, raw: &[u8]) {
        let text = strip_ansi(raw);
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            let is_last = parts.peek().is_none();
            if is_last && part.is_empty() {
                continue;
            }
            self.chat_lines.push(part.to_string());
        }
    }

    pub fn toggle_graph(&mut self) {
        self.mode = match self.mode {
            AppMode::Chat => AppMode::Graph,
            AppMode::Graph => AppMode::Chat,
        };
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_output_splits_into_lines() {
        let mut app = App::new();
        app.push_output(b"line one\nline two\n");
        assert_eq!(
            app.chat_lines,
            vec!["line one".to_string(), "line two".to_string()]
        );
    }

    #[test]
    fn push_output_strips_ansi_codes() {
        let mut app = App::new();
        app.push_output(b"\x1b[31mred text\x1b[0m\n");
        assert_eq!(app.chat_lines, vec!["red text".to_string()]);
    }

    #[test]
    fn toggle_graph_switches_mode_both_ways() {
        let mut app = App::new();
        assert_eq!(app.mode, AppMode::Chat);
        app.toggle_graph();
        assert_eq!(app.mode, AppMode::Graph);
        app.toggle_graph();
        assert_eq!(app.mode, AppMode::Chat);
    }

    #[test]
    fn push_output_keeps_trailing_partial_line_without_newline() {
        let mut app = App::new();
        app.push_output(b"partial");
        assert_eq!(app.chat_lines, vec!["partial".to_string()]);
    }

    #[test]
    fn push_output_across_multiple_calls_appends_new_entries() {
        let mut app = App::new();
        app.push_output(b"first");
        app.push_output(b"second");
        assert_eq!(
            app.chat_lines,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn push_output_ignores_empty_input() {
        let mut app = App::new();
        app.push_output(b"");
        assert!(app.chat_lines.is_empty());
    }

    #[test]
    fn default_matches_new() {
        let app = App::default();
        assert_eq!(app.mode, AppMode::Chat);
        assert!(app.chat_lines.is_empty());
        assert!(app.input.is_empty());
    }
}
