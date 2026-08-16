//! Key events, decoded once and away from the render loop so the mapping is
//! testable without a terminal.

#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Key(char),
    Enter,
    Backspace,
    ToggleGraph,
    /// Shows or hides the technical lines the chat pane collapses by default
    /// (thoughts, tool activity, stderr, frames no map matched).
    ToggleRaw,
    /// Answers the permission the PM is waiting on: `true` allows, `false`
    /// rejects. Does nothing when there is none.
    AnswerPermission(bool),
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
///
/// Permission answers are Ctrl+Y and Ctrl+N rather than bare `y`/`n` for the
/// same class of reason: a bare letter is a letter somebody is in the middle
/// of typing, and stealing it the instant an agent asks for permission would
/// mangle the turn being written and answer on the user's behalf.
///
/// Ctrl+O opens the technical lines. It is free everywhere mana runs: the
/// terminal's own `discard` character (`^O` on macOS) is handled by `IEXTEN`,
/// which raw mode turns off, so the byte reaches the application like any
/// other control key.
pub fn map_key_event(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<AppEvent> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match (code, modifiers) {
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => Some(AppEvent::ToggleGraph),
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => Some(AppEvent::ToggleRaw),
        (KeyCode::Char('y'), KeyModifiers::CONTROL) => Some(AppEvent::AnswerPermission(true)),
        (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(AppEvent::AnswerPermission(false)),
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
///
/// A trait rather than a `&mut dyn FnMut(Duration) -> …` parameter (#69):
/// every implementation carries behaviour that wants a name and a comment --
/// the Windows key-release filter below, the fail-fast contract on
/// `test_support::FakeEventSource`, the deadline on `launch_pm`'s `Idle`.
/// Closures would relocate all three into anonymous lambdas at their call
/// sites, not delete them.
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

// unix-gated with its only consumer (launch_pm's session tests, which drive a
// real child process): on Windows the struct would be dead code under
// -D warnings. Widen back to plain cfg(test) with the first Windows user.
#[cfg(all(test, unix))]
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

    /// The chat pane collapses technical lines by default, so the key that
    /// brings them back has to exist -- and the bare letter must still type.
    #[test]
    fn ctrl_o_toggles_the_raw_view_while_the_bare_letter_still_types() {
        assert_eq!(
            map_key_event(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Some(AppEvent::ToggleRaw)
        );
        assert_eq!(
            map_key_event(KeyCode::Char('o'), KeyModifiers::NONE),
            Some(AppEvent::Key('o'))
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

    /// Bare `y`/`n` are letters somebody may be halfway through typing; the
    /// answer keys must never steal them.
    #[test]
    fn ctrl_y_allows_and_ctrl_n_rejects_while_the_bare_letters_still_type() {
        assert_eq!(
            map_key_event(KeyCode::Char('y'), KeyModifiers::CONTROL),
            Some(AppEvent::AnswerPermission(true))
        );
        assert_eq!(
            map_key_event(KeyCode::Char('n'), KeyModifiers::CONTROL),
            Some(AppEvent::AnswerPermission(false))
        );
        assert_eq!(
            map_key_event(KeyCode::Char('y'), KeyModifiers::NONE),
            Some(AppEvent::Key('y'))
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
