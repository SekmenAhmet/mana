#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Key(char),
    Enter,
    Backspace,
    ToggleGraph,
    Quit,
}

/// Translates a crossterm key event into an AppEvent. Kept as a pure
/// function so the mapping is unit-testable without a real terminal.
pub fn map_key_event(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<AppEvent> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match (code, modifiers) {
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => Some(AppEvent::ToggleGraph),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(AppEvent::Quit),
        (KeyCode::Esc, _) => Some(AppEvent::Quit),
        (KeyCode::Enter, _) => Some(AppEvent::Enter),
        (KeyCode::Backspace, _) => Some(AppEvent::Backspace),
        (KeyCode::Char(c), KeyModifiers::NONE) => Some(AppEvent::Key(c)),
        (KeyCode::Char(c), KeyModifiers::SHIFT) => Some(AppEvent::Key(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn ctrl_g_toggles_graph() {
        assert_eq!(
            map_key_event(KeyCode::Char('g'), KeyModifiers::CONTROL),
            Some(AppEvent::ToggleGraph)
        );
    }

    #[test]
    fn plain_char_becomes_key_event() {
        assert_eq!(
            map_key_event(KeyCode::Char('x'), KeyModifiers::NONE),
            Some(AppEvent::Key('x'))
        );
    }

    #[test]
    fn escape_quits() {
        assert_eq!(
            map_key_event(KeyCode::Esc, KeyModifiers::NONE),
            Some(AppEvent::Quit)
        );
    }
}
