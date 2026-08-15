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

/// Source of raw key events for the PM's render loop. `launch_pm::run`
/// depends on this instead of calling `crossterm::event::poll`/`read`
/// directly, so the loop's per-tick logic is testable against a scripted
/// key sequence instead of only against a real terminal's stdin.
pub trait EventSource {
    fn poll_key(
        &mut self,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<crossterm::event::KeyEvent>>;
}

pub struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll_key(
        &mut self,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<crossterm::event::KeyEvent>> {
        if crossterm::event::poll(timeout)?
            && let crossterm::event::Event::Key(key) = crossterm::event::read()?
        {
            return Ok(Some(key));
        }
        Ok(None)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::EventSource;
    use crossterm::event::KeyEvent;
    use std::collections::VecDeque;

    /// Replays a fixed sequence of key events, one per `poll_key` call,
    /// ignoring the timeout entirely (tests don't want to actually wait).
    /// The sequence MUST end with a key that `map_key_event` maps to
    /// `AppEvent::Quit` (e.g. `KeyCode::Esc`) — once exhausted, `poll_key`
    /// returns an error instead of blocking forever, so a test that forgot
    /// the trailing quit key fails fast instead of hanging.
    pub(crate) struct FakeEventSource {
        queue: VecDeque<KeyEvent>,
    }

    impl FakeEventSource {
        pub(crate) fn new(events: impl IntoIterator<Item = KeyEvent>) -> Self {
            FakeEventSource {
                queue: events.into_iter().collect(),
            }
        }
    }

    impl EventSource for FakeEventSource {
        fn poll_key(&mut self, _timeout: std::time::Duration) -> anyhow::Result<Option<KeyEvent>> {
            self.queue.pop_front().map(Some).ok_or_else(|| {
                anyhow::anyhow!(
                    "FakeEventSource exhausted without a Quit-mapped key — the test likely \
                     forgot to end its scripted sequence with one, which would otherwise loop \
                     forever against a real EventSource"
                )
            })
        }
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

    #[test]
    fn ctrl_c_quits() {
        assert_eq!(
            map_key_event(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(AppEvent::Quit)
        );
    }

    #[test]
    fn enter_becomes_enter_event() {
        assert_eq!(
            map_key_event(KeyCode::Enter, KeyModifiers::NONE),
            Some(AppEvent::Enter)
        );
    }

    #[test]
    fn backspace_becomes_backspace_event() {
        assert_eq!(
            map_key_event(KeyCode::Backspace, KeyModifiers::NONE),
            Some(AppEvent::Backspace)
        );
    }

    #[test]
    fn shift_char_becomes_key_event() {
        assert_eq!(
            map_key_event(KeyCode::Char('X'), KeyModifiers::SHIFT),
            Some(AppEvent::Key('X'))
        );
    }

    #[test]
    fn unhandled_combo_returns_none() {
        assert_eq!(map_key_event(KeyCode::Char('g'), KeyModifiers::ALT), None);
    }
}
