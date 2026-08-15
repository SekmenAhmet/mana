//! Key events, decoded once and away from the render loop so the mapping is
//! testable without a terminal.

#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Key(char),
    Enter,
    Backspace,
    ToggleGraph,
    Quit,
}

/// Translates a crossterm key event into an `AppEvent`, or `None` for a key
/// mana has nothing to do with.
///
/// Escape is deliberately absent. In v1 it quit the session, which was wrong
/// on both ends: it is the key every agent CLI uses to interrupt itself, so
/// the reflex of pressing it to stop a runaway answer killed the whole PM
/// instead. Ctrl+C is the one way out now. mana does not forward Escape
/// either -- the stream transport carries turns as JSON frames, and there is
/// no keypress channel to a PM that is not attached to a terminal.
pub fn map_key_event(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<AppEvent> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match (code, modifiers) {
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => Some(AppEvent::ToggleGraph),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(AppEvent::Quit),
        (KeyCode::Enter, _) => Some(AppEvent::Enter),
        (KeyCode::Backspace, _) => Some(AppEvent::Backspace),
        (KeyCode::Char(c), KeyModifiers::NONE) => Some(AppEvent::Key(c)),
        (KeyCode::Char(c), KeyModifiers::SHIFT) => Some(AppEvent::Key(c)),
        _ => None,
    }
}

/// Source of raw key events for the PM's render loop. The loop depends on
/// this instead of calling `crossterm::event::poll`/`read` directly, so its
/// per-tick logic is testable against a scripted key sequence.
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
            // Windows reports press *and* release for every key; without this
            // filter each character would be typed twice there.
            && key.kind == crossterm::event::KeyEventKind::Press
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
    /// `AppEvent::Quit` (Ctrl+C) -- once exhausted, `poll_key` returns an
    /// error instead of blocking forever, so a test that forgot the trailing
    /// quit key fails fast instead of hanging.
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
                    "FakeEventSource exhausted without a Quit-mapped key -- the test likely \
                     forgot to end its scripted sequence with Ctrl+C, which would otherwise \
                     loop forever against a real EventSource"
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

    /// The v1 behaviour that had to go: Escape is the interrupt key of every
    /// agent CLI, and quitting the session on it is the opposite of what the
    /// reflex means.
    #[test]
    fn escape_no_longer_quits() {
        assert_eq!(map_key_event(KeyCode::Esc, KeyModifiers::NONE), None);
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
