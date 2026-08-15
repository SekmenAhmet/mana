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
    CLOSE_GRACE, DRAIN_GRACE, POLL_INTERVAL, join_or_detach, kill_group, pump_stderr, read_lines,
    reap, set_process_group,
};
use super::events::EventMap;
use super::{PmEvent, PmTransport};
use crate::catalog::{CliEntry, PmDriver, PromptMode, substitute};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A live PM session.
///
/// Ownership is split three ways on purpose: the caller holds stdin (turns go
/// out synchronously, so a failed write is reported to whoever typed it), a
/// reader thread holds stdout (a PM is silent for minutes at a time and mana
/// must stay responsive), and the `Child` is shared between the reader and
/// shutdown because both need to reap it and only one may do so at a time.
pub struct StreamDriver {
    child: Arc<Mutex<Child>>,
    /// Captured at spawn: after the child is reaped the pid is meaningless,
    /// and `kill_group` must never signal a recycled one.
    pid: u32,
    /// `None` once stdin has been closed -- the polite half of shutdown.
    stdin: Option<ChildStdin>,
    /// Decides how a turn is framed. Catalogue data, not a per-CLI branch.
    prompt: PromptMode,
    events: Receiver<PmEvent>,
    /// Taken by `shutdown`, which joins it so `Exited` is queued before it
    /// returns.
    reader: Option<JoinHandle<()>>,
    close_grace: Duration,
}

impl StreamDriver {
    /// Starts the PM described by `entry`, appending `extra_args` (the tool
    /// channel's already-substituted flags) after the catalogue's own.
    ///
    /// Runs in mana's working directory, which is the project the user invoked
    /// mana from -- exactly where a PM should be looking.
    pub fn start(entry: &CliEntry, extra_args: &[String]) -> Result<Self> {
        Self::start_in(entry, extra_args, None)
    }

    /// `start` with an explicit working directory, for callers that resolve the
    /// project elsewhere than the current directory.
    pub fn start_in(entry: &CliEntry, extra_args: &[String], cwd: Option<&Path>) -> Result<Self> {
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
        let args = substitute(&entry.pm.args, &HashMap::new())
            .with_context(|| format!("{id}: [pm].args"))?;

        let mut command = Command::new(entry.cli.bin());
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
                "failed to start {} as PM ('{}' -- is it installed and on PATH?)",
                entry.cli.name,
                entry.cli.bin()
            )
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (sender, events) = channel();
        let stderr_reader = pump_stderr(stderr, sender.clone());

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
            child,
            pid,
            stdin: Some(stdin),
            prompt,
            events,
            reader: Some(reader),
            close_grace: CLOSE_GRACE,
        })
    }

    /// Writes one user turn and flushes it.
    ///
    /// Synchronous, and deliberately so: a turn is the size of something a
    /// person typed, so the write lands in the pipe buffer and returns. A turn
    /// large enough to fill that buffer would block until the CLI reads it,
    /// which is the right back-pressure -- the alternative is queueing turns
    /// for a PM that stopped listening.
    pub fn send_user(&mut self, text: &str) -> Result<()> {
        let prompt = self.prompt;
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            anyhow!("the PM session is closed: its stdin was shut down, so no turn can be sent")
        })?;
        let frame = match prompt {
            PromptMode::StdinJsonl => user_frame(text),
            // A CLI that reads plain lines from a persistent stdin. No such
            // entry ships today; supporting it costs one arm of this match and
            // keeps the driver decided by catalogue data rather than by which
            // CLIs happened to exist when it was written.
            PromptMode::Stdin => format!("{text}\n"),
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

    /// The session's event stream. Borrowed rather than moved so the driver
    /// stays the single owner of the process and its channel; `recv` and
    /// `try_recv` both take `&self`, so a caller loses nothing.
    pub fn events(&self) -> &Receiver<PmEvent> {
        &self.events
    }

    /// Ends the session: close stdin, wait, kill what is left.
    ///
    /// Waits for the reader thread to report the exit before returning, so a
    /// caller may drain the channel afterwards without racing it. The wait is
    /// bounded (`DRAIN_GRACE`): if some surviving grandchild still holds the
    /// stdout pipe open, mana's own shutdown must not hang on it.
    pub fn shutdown(&mut self) -> Result<()> {
        // Closing stdin is the polite exit: a CLI that reads turns until EOF
        // ends its own session and flushes whatever it still owes.
        self.stdin = None;
        let deadline = Instant::now() + self.close_grace;
        while !self.has_exited() && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        self.kill_if_alive();
        if let Some(reader) = self.reader.take() {
            join_or_detach(reader, Instant::now() + DRAIN_GRACE);
        }
        Ok(())
    }

    /// The PM's pid, for the process registry (`mana ps`/`mana kill`).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// How long `shutdown` waits before killing. Tests shorten it; a caller
    /// with a CLI known to linger could lengthen it.
    pub fn set_close_grace(&mut self, grace: Duration) {
        self.close_grace = grace;
    }

    fn has_exited(&self) -> bool {
        // An error means the child is unreachable, which is as final as an exit
        // status for every purpose here.
        !matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }

    fn kill_if_alive(&self) {
        let mut child = self.child.lock().unwrap();
        // Under the lock, and only while `try_wait` reports the child unreaped:
        // the kernel still holds its pid, so the group id cannot yet have been
        // recycled onto somebody else's processes.
        if let Ok(None) = child.try_wait() {
            kill_group(&mut child, self.pid);
            // SIGKILL cannot be caught, so this reaps rather than waits.
            let _ = child.wait();
        }
    }
}

impl Drop for StreamDriver {
    fn drop(&mut self) {
        // A PM that outlives mana is v1's zombie: it holds a quota slot, keeps
        // whatever it spawned (its MCP server, for one) alive, and answers to
        // nobody. Cheap when `shutdown` was already called -- the child is
        // reaped and every wait below returns at once.
        let _ = self.shutdown();
    }
}

impl PmTransport for StreamDriver {
    fn send_user(&mut self, text: &str) -> Result<()> {
        StreamDriver::send_user(self, text)
    }

    fn events(&self) -> &Receiver<PmEvent> {
        StreamDriver::events(self)
    }

    fn shutdown(&mut self) -> Result<()> {
        StreamDriver::shutdown(self)
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
/// here.
fn user_frame(text: &str) -> String {
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
        match StreamDriver::start(entry, &[]) {
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
    use super::super::events::fixture;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const TEXT_PATH: &str = "$.message.content[?@.type=='text'].text";
    const ACK: &str =
        r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"ack"}]}}'"#;

    /// Writes an executable fake CLI and returns its path.
    fn script(dir: &Path, body: &str) -> String {
        let path = dir.join("fake-cli");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn driver(bin: &str, args: &[&str]) -> StreamDriver {
        let entry = fixture::entry(bin, args, "stdin-jsonl", TEXT_PATH, Some("$.usage"));
        StreamDriver::start(&entry, &[]).unwrap()
    }

    /// A driver that gives up on the polite exit almost at once -- what the
    /// kill path needs, and nothing a test should sit five seconds through.
    /// Never use it where the child is supposed to leave on its own: macOS
    /// spends ~345 ms on the *first* exec of a freshly written script (measured
    /// 2026-08-15; ~6 ms on every run after), so a short window would race the
    /// kernel rather than the driver.
    fn impatient_driver(bin: &str, args: &[&str]) -> StreamDriver {
        let mut driver = driver(bin, args);
        driver.set_close_grace(Duration::from_millis(200));
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
        let pid = driver.pid() as i32;
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
            Some(&PmEvent::Raw("boom: no credentials found".to_string()))
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
        let driver = StreamDriver::start(&entry, &extra).unwrap();

        let events = drain_to_exit(&driver);
        assert_eq!(
            events.first(),
            Some(&PmEvent::Raw(
                "argv: --from-catalogue --mcp-config /tmp/mana-mcp.json".to_string()
            ))
        );
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
        let pid = driver.pid() as i32;
        drop(driver);

        let deadline = Instant::now() + Duration::from_secs(3);
        while process_is_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        assert!(!process_is_alive(pid), "the PM survived its driver");
    }
}
