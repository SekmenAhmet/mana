# Real Terminal Emulation for the PM Chat Pane — Design

**Date:** 2026-08-15
**Status:** Approved

## Purpose

Manual testing (`mana launch claude`) showed the PM chat pane is unusable:
Claude Code is itself a full-screen TUI (cursor movement, redraws, box
drawing, live menus like `/effort`). `mana`'s current approach — strip ANSI
codes from each PTY chunk and append the result as flat lines to
`App::chat_lines` — treats a full-screen redraw as a stream of new chat
messages, producing garbled, duplicated text. Typed input only reaches the
child on `Enter` (buffered in `App::input`), so none of Claude Code's
live/interactive features (slash-command autocomplete, `/effort` menu) work
— the child never sees keystrokes as they happen.

This replaces the flat-line chat model with real VT100 terminal emulation:
the PM's PTY output is fed into a virtual terminal (`vt100::Parser`) and the
resulting screen is rendered directly as a `ratatui` widget
(`tui-term`'s `PseudoTerminal`), the same technique tools like `wezterm`,
`mprocs`, or IDE-embedded terminals use to host a child TUI inside their
own. Keystrokes are forwarded to the child in real time instead of being
buffered locally.

## Design

### Dependency bump (prerequisite, its own task before any new code)

`vt100 0.16` requires `unicode-width ^0.2.1`; `ratatui 0.29` (current) pins
`unicode-width =0.2.0` exactly — hard conflict, verified via a scratch
`cargo build`. `ratatui 0.30` (which also restructured internally into
`ratatui-core`/`ratatui-widgets`/`ratatui-crossterm`) resolves cleanly with
`vt100`/`tui-term`, but pulls `crossterm 0.29` (currently on `0.28`).

So: bump `ratatui` `0.29 → 0.30` and `crossterm` `0.28 → 0.29` first, get
the existing test suite green on the new versions with zero behavior
change, *then* add `vt100`/`tui-term` and build the new feature. Isolates
"did the version bump break something" from "did the new feature work" —
two different failure modes, two different tasks.

New dependencies: `vt100 = "0.16"`, `tui-term = { version = "0.3", features = ["vt100"] }`.

### `App` — replaces the flat chat-line model

```rust
pub struct App {
    pub terminal: vt100::Parser,   // replaces chat_lines: Vec<String>
    pub mode: AppMode,
    pub started_at: Instant,       // unchanged, drives the graph blink
}
```

No scrollback: `vt100::Parser::new(rows, cols, 0)` — the pane mirrors
exactly what the child's screen shows right now, like a real terminal, not
an accumulating log. `App::push_lines`/`push_output` are deleted along with
`AppMode`'s reliance on a separate `input: String` buffer (see Input
Handling below).

### PTY output: two independent consumers of the same bytes

`run_event_loop` still drains `pty_output: Receiver<Vec<u8>>` per chunk, and
now does two things with each chunk, neither one consuming/altering it for
the other:

1. `app.terminal.process(&chunk)` — feeds the virtual terminal for
   rendering. This is the only consumer that needs ANSI/cursor
   interpretation; `strip_ansi` is no longer used for the chat pane.
2. `intercept_subagent_launches(&strip_ansi(&chunk))` — **unchanged**,
   still scans raw text for `mana launch --subagent ...` and dispatches to
   `SubagentLauncher`. This keeps working exactly as before: it never
   needed cursor-aware rendering, only substring matching on decoded text.

**Behavior change, confirmed acceptable:** the interception no longer
*hides* the matched line from what's displayed — the chat pane now mirrors
the child's real screen faithfully, so a `Bash(mana launch --subagent ...)`
tool call Claude Code renders will be visible, exactly as it would be
running Claude Code directly without `mana`. The background dispatch to
`SubagentLauncher` (the actual orchestration — spawning the sub-agent,
writing lock/log files) is unaffected; only the "invisible in the chat"
part of the original design goal is dropped, deliberately, in favor of a
chat pane that actually renders correctly.

### Input handling: real-time passthrough, not a buffered box

New pure function, `tui/event.rs`:

```rust
/// Encodes a key event as the raw bytes a real terminal would send to a
/// child process — printable chars as UTF-8, control keys as their
/// conventional escape sequences. Returns None for keys mana reserves for
/// itself (Ctrl+G) or doesn't know how to encode.
pub fn encode_key_for_pty(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    match (code, modifiers) {
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => None, // reserved: graph toggle
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            Some(c.to_string().into_bytes())
        }
        (KeyCode::Char(c), KeyModifiers::CONTROL) => {
            // Ctrl+A..Z -> control bytes 0x01..0x1a
            let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
            Some(vec![byte])
        }
        (KeyCode::Enter, _) => Some(b"\r".to_vec()),
        (KeyCode::Backspace, _) => Some(vec![0x7f]),
        (KeyCode::Tab, _) => Some(b"\t".to_vec()),
        (KeyCode::Esc, _) => Some(vec![0x1b]),
        (KeyCode::Up, _) => Some(b"\x1b[A".to_vec()),
        (KeyCode::Down, _) => Some(b"\x1b[B".to_vec()),
        (KeyCode::Right, _) => Some(b"\x1b[C".to_vec()),
        (KeyCode::Left, _) => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}
```

`run_event_loop` calls this directly on every key event and, if `Some(bytes)`,
writes them straight to the PTY writer immediately — no `AppEvent::Key`
accumulation, no `App::input`, no `Enter`-triggered flush. `Ctrl+G` stays
the sole reserved combo (`encode_key_for_pty` returns `None` for it,
`run_event_loop` checks for it explicitly first and calls
`app.toggle_graph()` instead of forwarding). `Esc` is no longer "quit" —
quitting the PM session moves to `Ctrl+C` only (`Esc` is a real key Claude
Code itself uses; stealing it breaks the child).

`AppEvent`, `map_key_event`, and `apply_app_event` (the old
key→intent→PTY-write pipeline, including the now-removed `/graph` text
command) are deleted — passthrough replaces them entirely.

### `draw()`: render the virtual screen, drop the separate Input box

```rust
frame.render_widget(
    tui_term::widget::PseudoTerminal::new(app.terminal.screen()),
    chat_area,
);
```

No more `List` of `chat_items`, no more bottom `Input` block — the
child's own screen already shows its prompt/cursor/input line, a second
input box would be redundant and is exactly what made typing feel
disconnected. Layout: chat pane takes the full pane (or the left 60% in
Graph mode, same split as before) with no reserved input row underneath.

### PTY resizing

`pty::Spawner`/`PtySession` currently has no way to resize a live PTY after
spawn, and `pty::spawn` hardcodes `PtySize { rows: 40, cols: 120, .. }`
regardless of the actual chat-area size. `vt100::Parser` needs to be told
the real terminal size to render correctly, and the child needs to be told
too (so Claude Code wraps output to the right width). Add a resize
capability to `PtySession` (exposing `portable_pty::MasterPty::resize`).

Where it's called matters for testability: `prepare_session` (tested today
against a `FakeSpawner`, no real terminal) stays exactly as untouched as
possible — it keeps spawning at the existing default size. The actual
resize-to-real-dimensions call happens in `run()`, after the `ratatui`
`Terminal` is constructed and its initial size is known
(`terminal.size()`), right before entering `run_event_loop`. `run()` is
already the thin, untested wrapper (real raw-mode/alternate-screen setup
lives there); adding one more real-environment call there costs nothing
`prepare_session`'s unit tests would need to route around. Runtime resize
(the user's terminal window changing size mid-session) is out of scope for
this pass — v1 sizes once at startup, matching what `mana launch` already
does for its own outer terminal.

### Testing

- `encode_key_for_pty` — pure, fully unit-tested: every branch above, plus
  confirming `Ctrl+G` returns `None`.
- `vt100::Parser` processing is testable without a real terminal — it's a
  pure byte-stream state machine. A test can feed known bytes (including
  ANSI sequences) and assert on `parser.screen().contents()` or a specific
  cell's contents, without spawning any process.
- `draw()` — same `TestBackend` pattern already used elsewhere in
  `launch_pm.rs`: feed a `vt100::Parser` known bytes, draw, assert the
  rendered buffer contains the expected text.
- `intercept_subagent_launches`/`SubagentLauncher` — unchanged, existing
  tests remain valid as-is (operate on raw bytes, independent of rendering).
- PTY resize wiring itself (the real `portable_pty` resize call) — a real
  OS/PTY boundary, excluded from unit coverage by the same established
  convention as `native_pty_system()`/`enable_raw_mode()`.

## Out of scope

- Scrollback / chat history (explicitly declined — current-screen mirror
  only, like a real terminal).
- Runtime PTY resize on terminal window resize (sized once at startup).
- Restoring any form of "hide this line from the chat" filtering — dropped
  in favor of faithful rendering (see Behavior change above).
- The `/graph` text command (shipped earlier this session) — removed;
  incompatible with real-time keystroke passthrough. `Ctrl+G` remains the
  only way to toggle the graph pane.

## Testing (validation of this spec itself)

Same as `mana upgrade`'s spec: unit tests carry the pure logic
(`encode_key_for_pty`, vt100 processing, `draw()` via `TestBackend`); the
real end-to-end behavior (does Claude Code's `/effort` menu actually work
now, does the screen render cleanly) can only be confirmed by Ahmet running
`mana launch claude` again after this ships — same limitation as the PM
subagent interception work, which also could only be verified by hand.
