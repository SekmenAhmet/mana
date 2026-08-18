//! The `stream` driver: one persistent child, bidirectional JSONL.
//!
//! The CLI is started once and stays up for the whole session -- verified
//! multi-turn against claude on 2026-08-15: one process, session continuity
//! held, clean exit. mana writes each user turn as a JSON frame on stdin and
//! reads the CLI's own event stream back on stdout, one JSON object per line.
//!
//! Plain pipes, no PTY. The stream is already structured; putting a terminal
//! emulator in the middle would hand mana a rendered screen to re-parse, which
//! is the opposite of what the design asks for (§3): consume events, choose
//! what to render.
//!
//! Nothing here knows which CLI it is driving. The argv comes from the
//! catalogue, the meaning of each line comes from `[pm.events]`, and the only
//! shape this module hardcodes is the one the catalogue's `prompt` field names.

use super::child::{
    DRAIN_GRACE, PersistentChild, join_or_detach, pump_stderr, read_lines, reap, set_process_group,
};
use super::events::EventMap;
use super::{PmEvent, PmTransport, Resume};
use crate::catalog::{CliEntry, PmDriver, PromptMode, substitute};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A live PM session.
///
/// Ownership is split three ways on purpose: the caller holds stdin (turns go
/// out synchronously, so a failed write is reported to whoever typed it), a
/// reader thread holds stdout (a PM is silent for minutes at a time and mana
/// must stay responsive), and the `Child` is shared between the reader and
/// shutdown because both need to reap it and only one may do so at a time.
pub struct StreamDriver {
    /// The process, its reader thread and the shutdown that ends both -- the
    /// part the ACP driver holds identically (see `PersistentChild`).
    child: PersistentChild,
    /// `None` once stdin has been closed -- the polite half of shutdown. Owned
    /// outright, unlike ACP's, which is the one reason the two shutdowns are
    /// not the same function.
    stdin: Option<ChildStdin>,
    /// Decides how a turn is framed. Catalogue data, not a per-CLI branch.
    prompt: PromptMode,
    events: Receiver<PmEvent>,
    /// Read off the map before it was handed to the reader thread: whether this
    /// entry names the frame that closes a turn.
    tracks_turn_end: bool,
}

impl StreamDriver {
    /// Starts the PM described by `entry`, appending `extra_args` (the tool
    /// channel's already-substituted flags) after the catalogue's own.
    ///
    /// Runs in mana's working directory, which is the project the user invoked
    /// mana from -- exactly where a PM should be looking.
    pub fn start(
        entry: &CliEntry,
        extra_args: &[String],
        resume: Option<Resume<'_>>,
    ) -> Result<Self> {
        Self::start_in(entry, extra_args, None, resume)
    }

    /// `start` with an explicit working directory, for callers that resolve the
    /// project elsewhere than the current directory.
    pub fn start_in(
        entry: &CliEntry,
        extra_args: &[String],
        cwd: Option<&Path>,
        resume: Option<Resume<'_>>,
    ) -> Result<Self> {
        let id = &entry.cli.id;
        // An internal guard: the factory in `pm::start` dispatches on this
        // field, so reaching here with anything else is a code bug, not a
        // catalogue one.
        if entry.pm.driver != PmDriver::Stream {
            bail!(
                "{id}: the stream driver was handed a {:?} entry",
                entry.pm.driver
            );
        }
        let map = EventMap::for_entry(entry)?;
        // Absent only for `acp` entries, which the catalogue validates and the
        // factory never routes here; saying so beats an `unwrap` that would
        // panic on a hand-written local override.
        let prompt = entry.pm.prompt.ok_or_else(|| {
            anyhow!("{id}: [pm].prompt is missing, and the stream driver needs it to frame a turn")
        })?;
        match prompt {
            PromptMode::StdinJsonl | PromptMode::Stdin => {}
            // A one-shot argv prompt and a session that answers back are
            // mutually exclusive: there is no second argv to write into.
            PromptMode::Argv => bail!(
                "{id}: [pm].prompt is 'argv', which cannot carry a second turn -- the stream \
                 driver keeps one process alive for the whole session, so turns go to stdin"
            ),
        }
        // Nothing to substitute: a persistent session has no model, prompt,
        // cwd or config path to fill in here (the tool channel's flags arrive
        // ready-made in `extra_args`). Running the template through an empty
        // substitution is how a leftover `{placeholder}` becomes a startup
        // error instead of a literal argument handed to the CLI.
        let mut args = substitute(&entry.pm.args, &HashMap::new())
            .with_context(|| format!("{id}: [pm].args"))?;

        // Resuming is one appended flag on this driver (measured on claude:
        // `--continue` composes with the stream-json argv rather than replacing
        // it). An entry with nothing to append cannot resume, and saying so
        // beats starting a fresh conversation under a flag that promised the
        // old one -- the user would only find out by asking the PM something it
        // was supposed to remember.
        if resume.is_some() {
            if entry.pm.resume_args.is_empty() {
                bail!(
                    "{} cannot resume a conversation: its catalogue entry declares no \
                     [pm].resume_args, so there is no flag to ask it with. Launch it without \
                     --continue to start a fresh session.",
                    entry.cli.name
                );
            }
            args.extend(
                substitute(&entry.pm.resume_args, &HashMap::new())
                    .with_context(|| format!("{id}: [pm].resume_args"))?,
            );
        }

        // Resolved rather than spawned by name: see `CliMeta::resolve`.
        let program = entry
            .cli
            .resolve()
            .with_context(|| format!("failed to start {} as PM", entry.cli.name))?;
        let mut command = Command::new(&program);
        command
            .args(&args)
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        set_process_group(&mut command);

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start {} as PM ({})",
                entry.cli.name,
                program.display()
            )
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (sender, events) = channel();
        let stderr_reader = pump_stderr(stderr, sender.clone());
        // Asked before the map moves into the reader thread, which owns it for
        // the rest of the session.
        let tracks_turn_end = map.tracks_turn_end();

        let child = Arc::new(Mutex::new(child));
        let reaped = Arc::clone(&child);
        let reader = std::thread::spawn(move || {
            read_lines(stdout, |line| {
                map.extract(&line)
                    .into_iter()
                    .all(|event| sender.send(event).is_ok())
            });
            // stdout is at EOF, so every write end is closed and the child is
            // on its way out. Drain stderr before reporting the exit: when a PM
            // dies, the reason is on stderr and it must reach the user ahead of
            // the code that only says it happened.
            join_or_detach(stderr_reader, Instant::now() + DRAIN_GRACE);
            let code = reap(&reaped);
            // Sent last and always. v1 could not tell a thinking PM from a dead
            // one, so its chat pane waited forever on a process that was gone.
            let _ = sender.send(PmEvent::Exited { code });
        });

        Ok(StreamDriver {
            child: PersistentChild::new(child, pid, reader),
            stdin: Some(stdin),
            prompt,
            events,
            tracks_turn_end,
        })
    }
}

impl Drop for StreamDriver {
    fn drop(&mut self) {
        // A PM that outlives mana is v1's zombie: it holds a quota slot, keeps
        // whatever it spawned (its MCP server, for one) alive, and answers to
        // nobody. Cheap when `shutdown` was already called -- the child is
        // reaped and every wait it makes returns at once.
        let _ = self.shutdown();
    }
}

impl PmTransport for StreamDriver {
    /// Writes one user turn and flushes it.
    ///
    /// Synchronous, and deliberately so: a turn is the size of something a
    /// person typed, so the write lands in the pipe buffer and returns. A turn
    /// large enough to fill that buffer would block until the CLI reads it,
    /// which is the right back-pressure -- the alternative is queueing turns
    /// for a PM that stopped listening.
    fn send_user(&mut self, text: &str) -> Result<()> {
        let prompt = self.prompt;
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            anyhow!("the PM session is closed: its stdin was shut down, so no turn can be sent")
        })?;
        let frame = match prompt {
            PromptMode::StdinJsonl => user_frame(text),
            // Speculative, and wrong today: no catalogue entry selects this
            // mode for this driver, and this driver's stdin is line-delimited
            // one line per turn -- so writing `{text}\n` verbatim would slice
            // a multi-line prompt (the activation message always is one) into
            // as many turns as it has lines, not send it as one. Refuse
            // rather than silently mangle the first prompt that hits this.
            // Write this arm for real the day a catalogue entry needs it, and
            // get right what this stub didn't: joining or escaping embedded
            // newlines before the line goes to the child.
            PromptMode::Stdin => bail!(
                "[pm].prompt is 'stdin', which this driver does not support: its stdin is \
                 line-delimited one line per turn, so a multi-line prompt would be split \
                 into multiple turns instead of sent as one"
            ),
            PromptMode::Argv => unreachable!("rejected at start"),
        };
        stdin
            .write_all(frame.as_bytes())
            .and_then(|()| stdin.flush())
            // Almost always EPIPE: the PM died between turns. The `Exited`
            // event says so too, but the caller who typed this deserves an
            // answer now rather than on the next poll.
            .context("writing a user turn to the PM (has it exited?)")
    }

    fn events(&self) -> &Receiver<PmEvent> {
        &self.events
    }

    /// One process for the whole session, so the turn boundary exists only in
    /// the CLI's own stream: this driver knows it exactly when the catalogue
    /// names the frame that carries it, and says so rather than leaving a
    /// caller waiting on an event that will never come.
    fn tracks_turn_end(&self) -> bool {
        self.tracks_turn_end
    }

    /// Ends the session: close stdin, wait, kill what is left.
    ///
    /// Only the first line is this driver's own -- dropping the handle is what
    /// closes stdin here. Everything after it is the shutdown the ACP driver
    /// runs too, down to the bounded wait that guarantees `Exited` is queued
    /// before this returns, so a caller may drain the channel without racing it.
    fn shutdown(&mut self) -> Result<()> {
        // Closing stdin is the polite exit: a CLI that reads turns until EOF
        // ends its own session and flushes whatever it still owes.
        self.stdin = None;
        self.child.close_and_wait();
        Ok(())
    }
}

/// The stream-json user frame, verified against a real session on 2026-08-15.
///
/// A struct rather than a `json!` literal because serde writes struct fields in
/// declaration order while a `Map` sorts them -- and the bytes on the wire are
/// the thing that was verified.
#[derive(Serialize)]
struct UserFrame<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    message: UserMessage<'a>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Frames one user turn, newline-terminated: the newline is the frame
/// delimiter, so a turn without it is a turn the CLI never sees.
///
/// This shape belongs to `prompt = "stdin-jsonl"`, not to any CLI. A vendor
/// framing turns differently gets a new value for that field, never a branch
/// here. Shared with the oneshot driver for exactly that reason: the frame
/// belongs to the prompt mode, and two copies of it would be free to drift.
pub(super) fn user_frame(text: &str) -> String {
    let frame = UserFrame {
        kind: "user",
        message: UserMessage {
            role: "user",
            content: text,
        },
    };
    // Infallible: every field is a string.
    format!(
        "{}\n",
        serde_json::to_string(&frame).expect("frame is plain strings")
    )
}

#[cfg(test)]
mod tests {
    use super::super::events::fixture;
    use super::*;

    const TEXT_PATH: &str = "$.message.content[?@.type=='text'].text";

    /// `unwrap_err` would need `Debug` on a live process handle, which is not
    /// worth deriving for a type nobody prints.
    fn start_err(entry: &CliEntry) -> String {
        match StreamDriver::start(entry, &[], None) {
            Ok(_) => panic!("the driver started a session it should have refused"),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn a_user_turn_is_the_verified_stream_json_frame() {
        // Byte for byte, including the trailing newline that delimits it.
        assert_eq!(
            user_frame("hello \"world\""),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello \\\"world\\\"\"}}\n"
        );
    }

    #[test]
    fn an_argv_prompt_is_rejected_before_anything_is_spawned() {
        let entry = fixture::entry("no-such-binary", &[], "argv", TEXT_PATH, None);
        let rendered = start_err(&entry);
        assert!(rendered.contains("argv"), "{rendered}");
        assert!(rendered.contains("second turn"), "{rendered}");
    }

    /// `mana launch <cli> --continue` against a CLI whose entry declares no
    /// way to continue. Refused at the launch, before a process exists: the
    /// alternative is a fresh conversation the user believes is the old one,
    /// and they find that out by asking the PM something it should remember.
    #[test]
    fn resuming_a_cli_with_no_resume_args_is_refused_before_anything_is_spawned() {
        let entry = fixture::entry("no-such-binary", &[], "stdin-jsonl", TEXT_PATH, None);
        let rendered = match StreamDriver::start(&entry, &[], Some(Resume::default())) {
            Ok(_) => panic!("the driver resumed a session it could not resume"),
            Err(err) => format!("{err:#}"),
        };
        assert!(rendered.contains("resume_args"), "{rendered}");
        assert!(rendered.contains("Fake CLI"), "{rendered}");
        assert!(rendered.contains("--continue"), "{rendered}");
    }

    #[test]
    fn a_missing_event_map_is_reported_before_anything_is_spawned() {
        let entry = fixture::parse(
            &fixture::source("no-such-binary", &[], "stdin-jsonl", "$.text", None)
                .replace("[pm.events]\ntext = \"$.text\"\n", ""),
        );
        let rendered = start_err(&entry);
        assert!(rendered.contains("[pm.events]"), "{rendered}");
    }

    /// A `{model}` left in `[pm].args` has no value at this point, and passing
    /// it through would hand the CLI a literal brace.
    #[test]
    fn an_unfilled_placeholder_in_pm_args_is_a_startup_error_naming_the_field() {
        let entry = fixture::entry(
            "no-such-binary",
            &["--model", "{model}"],
            "stdin-jsonl",
            TEXT_PATH,
            None,
        );
        let rendered = start_err(&entry);
        assert!(rendered.contains("[pm].args"), "{rendered}");
        assert!(rendered.contains("model"), "{rendered}");
    }

    #[test]
    fn a_missing_binary_is_reported_with_the_cli_that_owns_it() {
        let entry = fixture::entry("mana-no-such-binary", &[], "stdin-jsonl", TEXT_PATH, None);
        let rendered = start_err(&entry);
        assert!(rendered.contains("Fake CLI"), "{rendered}");
        assert!(rendered.contains("mana-no-such-binary"), "{rendered}");
    }
}

#[cfg(all(test, unix))] // every test here execs a unix shell fixture
mod process_tests {
    use super::super::child::{CLOSE_GRACE, POLL_INTERVAL};
    use super::super::events::fixture;
    use super::*;
    use std::time::Duration;

    const TEXT_PATH: &str = "$.message.content[?@.type=='text'].text";
    const ACK: &str =
        r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"ack"}]}}'"#;

    /// Writes an executable fake CLI and returns its path.
    fn script(dir: &Path, body: &str) -> String {
        crate::subprocess::write_executable(dir, "fake-cli", &format!("#!/bin/sh\n{body}\n"))
            .to_string_lossy()
            .into_owned()
    }

    fn driver(bin: &str, args: &[&str]) -> StreamDriver {
        let entry = fixture::entry(bin, args, "stdin-jsonl", TEXT_PATH, Some("$.usage"));
        StreamDriver::start(&entry, &[], None).unwrap()
    }

    /// A driver that gives up on the polite exit almost at once -- what the
    /// kill path needs, and nothing a test should sit five seconds through.
    /// Never use it where the child is supposed to leave on its own: macOS
    /// spends ~345 ms on the *first* exec of a freshly written script (measured
    /// 2026-08-15; ~6 ms on every run after), so a short window would race the
    /// kernel rather than the driver.
    fn impatient_driver(bin: &str, args: &[&str]) -> StreamDriver {
        let mut driver = driver(bin, args);
        driver.child.set_close_grace(Duration::from_millis(200));
        driver
    }

    fn next(driver: &StreamDriver) -> PmEvent {
        driver
            .events()
            .recv_timeout(Duration::from_secs(10))
            .expect("the PM produced no further event")
    }

    /// Everything up to and including `Exited`, which always terminates the
    /// stream.
    fn drain_to_exit(driver: &StreamDriver) -> Vec<PmEvent> {
        let mut seen = Vec::new();
        loop {
            let event = next(driver);
            let done = matches!(event, PmEvent::Exited { .. });
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    fn process_is_alive(pid: i32) -> bool {
        // SAFETY: signal 0 runs the existence/permission check only; nothing is
        // delivered and nothing is dereferenced.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// The round trip: a turn goes out as a frame, the answer comes back as
    /// `Text`, and the frames on the wire are exactly what was verified against
    /// a real CLI.
    #[test]
    fn user_turns_reach_the_child_as_stream_json_frames() {
        let tmp = tempfile::tempdir().unwrap();
        let received = tmp.path().join("received.jsonl");
        let bin = script(
            tmp.path(),
            &format!(
                "echo '{{\"type\":\"system\",\"subtype\":\"init\"}}'\n\
                 while IFS= read -r line; do\n\
                 \x20 printf '%s\\n' \"$line\" >> \"$1\"\n\
                 \x20 {ACK}\n\
                 done"
            ),
        );
        let mut driver = driver(&bin, &[received.to_string_lossy().as_ref()]);

        // The init banner matches neither path: degraded, visible, harmless.
        assert!(matches!(next(&driver), PmEvent::Raw(line) if line.contains("init")));

        driver.send_user("hello \"world\"").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        driver.send_user("second turn").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));

        driver.shutdown().unwrap();
        assert_eq!(
            std::fs::read_to_string(&received).unwrap(),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello \\\"world\\\"\"}}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"second turn\"}}\n"
        );
    }

    /// The turn boundary on a driver whose process outlives the turn: it comes
    /// out of the frame the catalogue names, and it arrives after everything
    /// the turn carried.
    #[test]
    fn a_turn_ends_on_the_frame_the_catalogue_names() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            &format!(
                "while IFS= read -r line; do\n\
                 \x20 {ACK}\n\
                 \x20 echo '{{\"type\":\"result\",\"usage\":{{\"output_tokens\":7}}}}'\n\
                 done"
            ),
        );
        let entry = fixture::parse(&fixture::with_turn_end(
            &fixture::source(&bin, &[], "stdin-jsonl", TEXT_PATH, Some("$.usage")),
            "$.type",
            "result",
        ));
        let mut driver = StreamDriver::start(&entry, &[], None).unwrap();
        assert!(driver.tracks_turn_end());

        driver.send_user("hello").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(
            next(&driver),
            PmEvent::Usage(serde_json::json!({"output_tokens": 7}))
        );
        assert_eq!(next(&driver), PmEvent::TurnEnded);

        // A second turn ends the same way: the process is the session, not the
        // turn, so nothing about the first one is consumed.
        driver.send_user("again").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert!(matches!(next(&driver), PmEvent::Usage(_)));
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        driver.shutdown().unwrap();
    }

    /// An entry that never names the closing frame must say so rather than let
    /// a caller wait on an event that cannot arrive.
    #[test]
    fn a_stream_entry_without_a_turn_end_admits_it_cannot_track_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let driver = driver(&script(tmp.path(), "cat > /dev/null"), &[]);
        assert!(!driver.tracks_turn_end());
    }

    /// Closing stdin is enough for a CLI that reads turns to EOF -- no kill, so
    /// the exit code survives (a signalled process reports none).
    #[test]
    fn shutdown_lets_a_well_behaved_pm_leave_on_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            &format!("while IFS= read -r line; do :; done\n{ACK}"),
        );
        let mut driver = driver(&bin, &[]);

        driver.send_user("a turn").unwrap();
        let started = Instant::now();
        driver.shutdown().unwrap();

        // Returning before the grace expires is what proves the child was
        // observed leaving, rather than waited out and killed.
        assert!(started.elapsed() < CLOSE_GRACE, "the grace window expired");
        let events = drain_to_exit(&driver);
        assert_eq!(events.last(), Some(&PmEvent::Exited { code: Some(0) }));
        assert!(
            events.contains(&PmEvent::Text("ack".to_string())),
            "{events:?}"
        );
    }

    /// The v1 zombie, structurally: a PM that ignores the polite exit is killed
    /// together with everything it spawned.
    #[test]
    fn shutdown_kills_a_pm_that_ignores_the_close_and_takes_its_children_along() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        // The backgrounded sleep stands in for the PM's MCP server: killing the
        // CLI alone would leave it running, holding a pipe and answering nobody.
        let bin = script(
            tmp.path(),
            "sleep 30 &\necho $! > \"$1\"\n\
             echo '{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}'\n\
             sleep 30",
        );
        let mut driver = impatient_driver(&bin, &[pidfile.to_string_lossy().as_ref()]);
        let pid = driver.child.pid() as i32;
        assert_eq!(next(&driver), PmEvent::Text("hi".to_string()));

        let started = Instant::now();
        driver.shutdown().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "shutdown hung");

        // The promise: `Exited` arrives even when the PM had to be killed.
        assert_eq!(
            drain_to_exit(&driver).last(),
            Some(&PmEvent::Exited { code: None })
        );

        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for (label, victim) in [("the PM", pid), ("its child", grandchild)] {
            // Reparenting to init and the reap that follows are not instant.
            let deadline = Instant::now() + Duration::from_secs(3);
            while process_is_alive(victim) && Instant::now() < deadline {
                std::thread::sleep(POLL_INTERVAL);
            }
            assert!(
                !process_is_alive(victim),
                "{label} ({victim}) survived shutdown"
            );
        }
    }

    /// PM death must be detectable without anyone asking: the reader thread
    /// reports it, even when the CLI never said a word.
    #[test]
    fn a_pm_that_dies_immediately_still_reports_its_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let driver = driver(&script(tmp.path(), "exit 3"), &[]);
        assert_eq!(
            drain_to_exit(&driver),
            vec![PmEvent::Exited { code: Some(3) }]
        );
    }

    /// A crash on startup is the case where stderr is the only explanation
    /// there is, and it must arrive before the exit that follows it.
    #[test]
    fn stderr_reaches_the_caller_before_the_exit_it_explains() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "echo 'boom: no credentials found' >&2\nexit 7");
        let events = drain_to_exit(&driver(&bin, &[]));

        assert_eq!(events.last(), Some(&PmEvent::Exited { code: Some(7) }));
        assert_eq!(
            events.first(),
            Some(&PmEvent::Stderr("boom: no credentials found".to_string()))
        );
    }

    /// The tool channel's flags come from `extra_args` and must land after the
    /// catalogue's own, where a CLI expects trailing options.
    #[test]
    fn extra_args_are_appended_after_the_catalogue_args() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "echo \"argv: $*\" >&2");
        let entry = fixture::entry(&bin, &["--from-catalogue"], "stdin-jsonl", TEXT_PATH, None);
        let extra = ["--mcp-config".to_string(), "/tmp/mana-mcp.json".to_string()];
        let driver = StreamDriver::start(&entry, &extra, None).unwrap();

        let events = drain_to_exit(&driver);
        assert_eq!(
            events.first(),
            Some(&PmEvent::Stderr(
                "argv: --from-catalogue --mcp-config /tmp/mana-mcp.json".to_string()
            ))
        );
    }

    /// Resuming on this driver is one flag, and where it lands matters: after
    /// the catalogue's own argv (which is what puts claude in stream-json mode
    /// -- the two compose, measured 2026-08-15) and before the tool channel's.
    #[test]
    fn resume_args_land_between_the_catalogue_args_and_the_tool_channel_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "echo \"argv: $*\" >&2");
        let entry = fixture::parse(
            &fixture::source(&bin, &["--from-catalogue"], "stdin-jsonl", TEXT_PATH, None)
                // The [pm] one, not [subagent]'s (which is `argv`).
                .replace(
                    "prompt = \"stdin-jsonl\"",
                    "resume_args = [\"--continue\"]\nprompt = \"stdin-jsonl\"",
                ),
        );
        let extra = ["--mcp-config".to_string(), "/tmp/mana-mcp.json".to_string()];

        // Without --continue the flag is absent...
        let fresh = drain_to_exit(&StreamDriver::start(&entry, &extra, None).unwrap());
        assert_eq!(
            fresh.first(),
            Some(&PmEvent::Stderr(
                "argv: --from-catalogue --mcp-config /tmp/mana-mcp.json".to_string()
            ))
        );

        // ...and with it, it is exactly one flag in exactly one place.
        let resumed =
            drain_to_exit(&StreamDriver::start(&entry, &extra, Some(Resume::default())).unwrap());
        assert_eq!(
            resumed.first(),
            Some(&PmEvent::Stderr(
                "argv: --from-catalogue --continue --mcp-config /tmp/mana-mcp.json".to_string()
            ))
        );
    }

    /// The guard against the exact bug this arm used to be: a silent
    /// `format!("{text}\n")` would turn one multi-line prompt into as many
    /// turns as it has lines. This fails if that line is ever restored.
    #[test]
    fn a_stdin_prompt_mode_refuses_a_multiline_turn_instead_of_splitting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "cat > /dev/null");
        let entry = fixture::entry(&bin, &[], "stdin", TEXT_PATH, None);
        let mut driver = StreamDriver::start(&entry, &[], None).unwrap();

        let err = driver.send_user("line one\nline two").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("'stdin'"), "{rendered}");
        assert!(rendered.contains("does not support"), "{rendered}");

        driver.shutdown().unwrap();
    }

    #[test]
    fn a_turn_sent_after_shutdown_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut driver = impatient_driver(&script(tmp.path(), "cat > /dev/null"), &[]);
        driver.shutdown().unwrap();

        let err = driver.send_user("too late").unwrap_err();
        assert!(format!("{err:#}").contains("closed"), "{err:#}");
    }

    /// Dropping the driver must not leave the PM running: nothing else in mana
    /// knows the pid once the handle is gone.
    #[test]
    fn dropping_the_driver_kills_the_pm() {
        let tmp = tempfile::tempdir().unwrap();
        let driver = impatient_driver(&script(tmp.path(), "sleep 30"), &[]);
        let pid = driver.child.pid() as i32;
        drop(driver);

        let deadline = Instant::now() + Duration::from_secs(3);
        while process_is_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        assert!(!process_is_alive(pid), "the PM survived its driver");
    }
}
