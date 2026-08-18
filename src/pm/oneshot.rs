//! The `oneshot-continue` driver: one process per turn, and a session that
//! lives in the CLI's own store between them.
//!
//! Most agent CLIs have no persistent protocol at all. They offer a headless
//! flag (`--print`), a structured output format, and a flag that resumes the
//! last conversation (`--continue`). That is enough to hold a real PM session
//! -- provided mana stops equating "the process is gone" with "the PM is
//! gone", which is the whole difficulty this module exists to handle.
//!
//! ## What a session is here
//!
//! A turn is a process. Between turns there is nothing running: no pipe, no
//! pid, no memory of the conversation anywhere in mana. The conversation lives
//! wherever the CLI keeps it, and the next turn picks it back up through
//! `[pm].continue_args`.
//!
//! Measured on agy 1.1.13 (2026-08-15, this machine), because the design
//! listed it as an open question: `--continue` **does** preserve context
//! across print-mode calls. Turn 1 was asked to remember `PLATYPUS-7734`,
//! turn 2 (`--continue`, a separate process) answered with the token, and both
//! frames carried the same `conversation_id`. It is also **scoped to the
//! working directory**: the same `--continue` run from another directory
//! opened a fresh conversation that knew nothing. So the driver always sets an
//! explicit working directory -- that is what pins a session to a project.
//!
//! ## Death semantics
//!
//! The trait promises `Exited` arrives exactly once and last. A per-turn
//! process would fire it every turn if "process exit" were taken literally, so
//! the rule here is about the *session*:
//!
//! - a turn that exits **0** ends the turn only: one `TurnEnded`, and the
//!   session waits for the next one;
//! - a turn that exits **non-zero or is signalled** ends the session: its
//!   stderr is drained into `Raw` first, then `Exited { code }`. A CLI that
//!   failed this turn fails the next one identically (expired credentials, a
//!   flag it does not know), and v1's lesson is that the operator must be told
//!   now rather than left retrying into silence;
//! - a **spawn that fails** ends it the same way, after a `Raw` carrying the
//!   reason. `start` also refuses upfront when the binary is not on `PATH`, so
//!   the common case fails the launch instead of the session.
//!
//! The `[[failure]]` signature matcher exists (`dispatch::match_signatures`)
//! but is wired to sub-agent dispatches only -- it was specified for the
//! observer, not for this driver, and nothing here consults it. Pointed at a
//! PM turn it would make a quota-shaped exit a `Raw` plus a cooldown rather
//! than a session death; nothing else here needs to change for that except the
//! one comparison in `run_turns`.
//!
//! ## One process at a time
//!
//! Turns are strictly serialized: agy's own entry records that two concurrent
//! instances died within 8 seconds. A turn sent while another is in flight is
//! **queued**, not refused, because mana injects turns of its own -- a finished
//! dispatch becomes a user turn (`cli::launch_pm`) at whatever moment the
//! executor happens to end, including mid-turn. Refusing it loudly would put
//! the message in the chat pane and never in the PM's context, silently
//! breaking the loop that notification exists to close.

use super::child::{
    DRAIN_GRACE, join_or_detach, kill_group, pump_stderr, read_lines, reap, set_process_group,
};
use super::events::EventMap;
use super::{PmEvent, PmTransport, Resume};
use crate::catalog::{CliEntry, PmDriver, PromptMode, substitute};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// The turn in flight, published so `shutdown` and `Drop` can reach a process
/// the worker thread is otherwise the only owner of. `None` between turns,
/// which is where a session spends most of its life.
type InFlight = Arc<Mutex<Option<Turn>>>;

/// A live turn: the child, and the pid captured at spawn because after the
/// reap the handle's own id is meaningless and `kill_group` must never signal
/// a recycled one.
struct Turn {
    child: Arc<Mutex<Child>>,
    pid: u32,
}

/// A live PM session made of processes that do not overlap.
pub struct OneshotDriver {
    /// The turn queue. Dropping it lets the worker leave once it is drained.
    turns: Option<Sender<String>>,
    events: Receiver<PmEvent>,
    in_flight: InFlight,
    /// "This session takes no more turns." Set by `shutdown` before it kills,
    /// so a turn already queued behind the one being killed is never started.
    closed: Arc<AtomicBool>,
    /// The once-guard on `Exited`: whoever sets it first is the one that sent
    /// the event, and both the worker and `shutdown` can get there.
    exited: Arc<AtomicBool>,
    /// Kept so `shutdown` can queue `Exited` when the worker never had to.
    sender: Sender<PmEvent>,
    worker: Option<JoinHandle<()>>,
}

impl OneshotDriver {
    /// Starts the session `entry` describes, appending `extra_args` (the tool
    /// channel's already-substituted flags) after the catalogue's own.
    ///
    /// Nothing is spawned here: the first process carries the first turn.
    pub fn start(
        entry: &CliEntry,
        extra_args: &[String],
        resume: Option<Resume<'_>>,
    ) -> Result<Self> {
        Self::start_in(entry, extra_args, None, resume)
    }

    /// `start` with an explicit working directory.
    ///
    /// It is not a convenience: the CLI's conversation store is keyed by
    /// directory (measured on agy), so the directory is what decides which
    /// session `[pm].continue_args` resumes.
    pub fn start_in(
        entry: &CliEntry,
        extra_args: &[String],
        cwd: Option<&Path>,
        resume: Option<Resume<'_>>,
    ) -> Result<Self> {
        let id = &entry.cli.id;
        // An internal guard: the factory in `pm::start` dispatches on this
        // field, so reaching here with anything else is a code bug.
        if entry.pm.driver != PmDriver::OneshotContinue {
            bail!(
                "{id}: the oneshot-continue driver was handed a {:?} entry",
                entry.pm.driver
            );
        }
        let map = EventMap::for_entry(entry)?;
        let prompt = entry.pm.prompt.ok_or_else(|| {
            anyhow!("{id}: [pm].prompt is missing, and the oneshot driver needs it to carry a turn")
        })?;
        let first = turn_args(entry, entry.pm.first_args.as_deref(), "first_args")?;
        let continued = turn_args(entry, entry.pm.continue_args.as_deref(), "continue_args")?;

        // Where the session will live. Resolved once and always passed to the
        // child, rather than left to whatever directory mana happens to be in
        // when a turn starts.
        let cwd = match cwd {
            Some(dir) => dir.to_path_buf(),
            None => std::env::current_dir()
                .context("resolving the working directory the PM session belongs to")?,
        };
        let cwd_arg = cwd.to_string_lossy().into_owned();

        // Both templates are filled with a probe value now rather than at the
        // first turn: an unknown or unfillable placeholder is a catalogue bug,
        // and a launch is where a catalogue bug should stop.
        for (field, template) in [("first_args", &first), ("continue_args", &continued)] {
            substitute(template, &vars("probe", &cwd_arg))
                .with_context(|| format!("{id}: [pm].{field}"))?;
        }
        if prompt != PromptMode::Argv {
            // A turn travels one way or the other, never both: a `{prompt}`
            // left in argv beside a stdin delivery would send it twice.
            for (field, template) in [("first_args", &first), ("continue_args", &continued)] {
                if template.iter().any(|arg| arg.contains("{prompt}")) {
                    bail!(
                        "{id}: [pm].prompt is {prompt:?}, so the turn goes to stdin -- but \
                         [pm].{field} still carries a {{prompt}} placeholder"
                    );
                }
            }
        }

        // Every other driver refuses a missing binary at `start`, because it
        // spawns there. This one would only find out on the first turn, and a
        // launch that reaches an interactive pane before saying "that CLI is
        // not installed" is the worse of the two failures. The path this
        // resolves is the one every turn then spawns, so the check and the
        // spawn cannot answer differently -- see `CliMeta::resolve`.
        let program = entry
            .cli
            .resolve()
            .with_context(|| format!("failed to start {} as PM", entry.cli.name))?;

        let (sender, events) = channel();
        let (turns, inbox) = channel();
        let in_flight: InFlight = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));

        let spawner = TurnSpawner {
            id: id.clone(),
            cli_name: entry.cli.name.clone(),
            bin: program,
            first,
            continued,
            extra: extra_args.to_vec(),
            prompt,
            cwd,
            cwd_arg,
            map,
        };
        // Resuming is not a flag on this driver, it is *where the session
        // starts*: `continue_args` is already the template that picks the
        // CLI's previous conversation up, so a resumed session simply has no
        // first turn -- turn one continues, like every turn after it. That is
        // also why `[pm].resume_args` is unread here (agy declares none).
        let resumed = resume.is_some();
        let worker = std::thread::spawn({
            let sender = sender.clone();
            let in_flight = Arc::clone(&in_flight);
            let closed = Arc::clone(&closed);
            let exited = Arc::clone(&exited);
            move || {
                run_turns(
                    spawner, inbox, &sender, &in_flight, &closed, &exited, resumed,
                )
            }
        });

        Ok(OneshotDriver {
            turns: Some(turns),
            events,
            in_flight,
            closed,
            exited,
            sender,
            worker: Some(worker),
        })
    }

    /// Kills the running turn and everything it started. `true` if there was
    /// one to kill.
    fn kill_in_flight(&self) -> bool {
        // Taken out from under the lock before anything is killed: holding it
        // while waiting would block the worker's own reap.
        let turn = self.in_flight.lock().unwrap().take();
        let Some(turn) = turn else { return false };
        let mut child = turn.child.lock().unwrap();
        if let Ok(None) = child.try_wait() {
            kill_group(&mut child, turn.pid);
            // SIGKILL cannot be caught, so this reaps rather than waits.
            let _ = child.wait();
        }
        true
    }
}

impl Drop for OneshotDriver {
    fn drop(&mut self) {
        // A turn that outlives mana holds a quota slot and answers to nobody,
        // and on a CLI that refuses to run twice at once it also poisons the
        // next session.
        let _ = self.shutdown();
    }
}

impl PmTransport for OneshotDriver {
    /// Queues one user turn.
    ///
    /// Returns as soon as the turn is queued rather than when the process that
    /// carries it exits: a turn takes seconds to minutes, and the caller is a
    /// render loop that must keep drawing.
    fn send_user(&mut self, text: &str) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("the PM session is closed, so no turn can be sent")
        }
        let turns = self
            .turns
            .as_ref()
            .ok_or_else(|| anyhow!("the PM session is closed, so no turn can be sent"))?;
        turns
            .send(text.to_string())
            .map_err(|_| anyhow!("the PM session ended before this turn could be started"))
    }

    fn events(&self) -> &Receiver<PmEvent> {
        &self.events
    }

    /// Ends the session, killing the turn in flight if there is one.
    ///
    /// No grace period, unlike the long-lived drivers: there is no session
    /// state inside the process to lose. The conversation is in the CLI's own
    /// store, so what a kill costs is the answer to one turn -- and making the
    /// operator wait out an agent that thinks for minutes is not a trade worth
    /// making at Ctrl+C.
    fn shutdown(&mut self) -> Result<()> {
        // Ordered: refuse new turns, stop the worker from starting queued
        // ones, and only then kill. The other order leaves a window where the
        // worker starts the next turn out of a queue nobody will read.
        self.closed.store(true, Ordering::SeqCst);
        self.turns = None;
        let killed = self.kill_in_flight();
        if let Some(worker) = self.worker.take() {
            join_or_detach(worker, Instant::now() + DRAIN_GRACE);
        }
        // A no-op when the worker already reported the death it saw, which is
        // the more accurate report of the two.
        send_exit(
            &self.sender,
            &self.closed,
            &self.exited,
            if killed { None } else { Some(0) },
        );
        Ok(())
    }
}

/// Everything a turn needs, owned by the worker thread.
struct TurnSpawner {
    id: String,
    cli_name: String,
    bin: PathBuf,
    first: Vec<String>,
    continued: Vec<String>,
    extra: Vec<String>,
    prompt: PromptMode,
    cwd: PathBuf,
    cwd_arg: String,
    map: EventMap,
}

impl TurnSpawner {
    /// Spawns the process for one turn: `first_args` while no turn has run
    /// yet, `continue_args` afterwards.
    fn spawn(&self, text: &str, started_any: bool) -> Result<Child> {
        let (field, template) = if started_any {
            ("continue_args", &self.continued)
        } else {
            ("first_args", &self.first)
        };
        let args = substitute(template, &vars(text, &self.cwd_arg))
            .with_context(|| format!("{}: [pm].{field}", self.id))?;

        let mut command = Command::new(&self.bin);
        command
            .args(&args)
            .args(&self.extra)
            .stdin(match self.prompt {
                // Closed rather than inherited: a CLI that reads stdin when it
                // has nothing to read would hang on mana's own terminal.
                PromptMode::Argv => Stdio::null(),
                PromptMode::Stdin | PromptMode::StdinJsonl => Stdio::piped(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&self.cwd);
        set_process_group(&mut command);
        command.spawn().with_context(|| {
            format!(
                "failed to start {} as PM ({})",
                self.cli_name,
                self.bin.display()
            )
        })
    }

    /// Runs one turn to completion and reports how the process ended.
    ///
    /// Blocking on purpose: this *is* the worker thread's work, and reading
    /// stdout here keeps the event map on the stack instead of forcing it
    /// behind an `Arc` for a thread that would immediately be joined.
    fn run(
        &self,
        mut child: Child,
        text: &str,
        sender: &Sender<PmEvent>,
        in_flight: &InFlight,
        closed: &Arc<AtomicBool>,
    ) -> Option<i32> {
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let child = Arc::new(Mutex::new(child));
        *in_flight.lock().unwrap() = Some(Turn {
            child: Arc::clone(&child),
            pid,
        });
        // `shutdown` may have run between the spawn and that line, finding
        // nothing to kill. Nobody is left to read this process, and on a CLI
        // that refuses to run twice at once a leaked turn poisons the next
        // session -- so it ends here instead.
        if closed.load(Ordering::SeqCst) {
            let mut guard = child.lock().unwrap();
            kill_group(&mut guard, pid);
            // SIGKILL cannot be caught, so this reaps rather than waits.
            let _ = guard.wait();
        }

        if let Some(mut stdin) = stdin {
            // The turn travels on stdin for this catalogue entry. Closing it
            // afterwards is what tells a headless CLI the prompt is complete.
            let frame = match self.prompt {
                PromptMode::StdinJsonl => super::stream::user_frame(text),
                _ => format!("{text}\n"),
            };
            if let Err(error) = stdin
                .write_all(frame.as_bytes())
                .and_then(|()| stdin.flush())
            {
                let _ = sender.send(PmEvent::Raw(format!(
                    "mana could not hand this turn to {}: {error}",
                    self.cli_name
                )));
            }
        }

        let stderr_reader = pump_stderr(stderr, sender.clone());
        read_lines(stdout, |line| {
            self.map
                .extract(&line)
                .into_iter()
                // The process ending is this driver's turn boundary (see
                // `run_turns`), so a frame-level one would be a second, racing
                // source for the same fact. An entry that declares
                // `[pm.events].turn_end` anyway gets it ignored rather than
                // doubled -- two `TurnEnded` for one turn would release two
                // queued messages back to back, which is the bug the event
                // exists to prevent.
                .filter(|event| !matches!(event, PmEvent::TurnEnded))
                .all(|event| sender.send(event).is_ok())
        });
        // stdout is at EOF, so the process is on its way out. Drain stderr
        // before reporting anything about the exit: when a turn dies, the
        // reason is there and it must arrive ahead of the code.
        join_or_detach(stderr_reader, Instant::now() + DRAIN_GRACE);
        let code = reap(&child);
        *in_flight.lock().unwrap() = None;
        code
    }
}

/// The worker: one turn at a time, forever, until the session ends.
///
/// `resumed` is the initial value of "a turn has already run": true when the
/// session is picking up a conversation the CLI still holds, which makes the
/// very first process a `continue_args` one.
fn run_turns(
    spawner: TurnSpawner,
    inbox: Receiver<String>,
    sender: &Sender<PmEvent>,
    in_flight: &InFlight,
    closed: &Arc<AtomicBool>,
    exited: &Arc<AtomicBool>,
    resumed: bool,
) {
    let mut started_any = resumed;
    while let Ok(text) = inbox.recv() {
        // Checked here rather than only at `send_user`: `shutdown` can land
        // while this turn was queued, and a killed session must not spawn one
        // more process on its way out.
        if closed.load(Ordering::SeqCst) {
            return;
        }
        let child = match spawner.spawn(&text, started_any) {
            Ok(child) => child,
            Err(error) => {
                // Visible first, then final: a CLI that will not start will
                // not start on the next turn either.
                let _ = sender.send(PmEvent::Raw(format!("{error:#}")));
                send_exit(sender, closed, exited, None);
                return;
            }
        };
        started_any = true;
        let code = spawner.run(child, &text, sender, in_flight, closed);
        if code != Some(0) {
            send_exit(sender, closed, exited, code);
            return;
        }
        // A clean exit is this transport's end-of-turn: the process that held
        // the turn is gone, everything it said is already on the channel, and
        // the session is listening again. Nothing is read out of the stream to
        // decide it -- which is what makes it the same kind of fact as ACP's
        // `stopReason`, not a parse.
        //
        // Only after a *clean* exit: the other branch above already ended the
        // whole session, and whoever is waiting on a turn is released by
        // `Exited` instead.
        if sender.send(PmEvent::TurnEnded).is_err() {
            return;
        }
    }
}

/// Queues `Exited` once for the whole session, whoever gets here first.
fn send_exit(
    sender: &Sender<PmEvent>,
    closed: &Arc<AtomicBool>,
    exited: &Arc<AtomicBool>,
    code: Option<i32>,
) {
    closed.store(true, Ordering::SeqCst);
    if exited.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = sender.send(PmEvent::Exited { code });
}

/// The values a turn's argv template may reference. `{prompt}` is the turn
/// itself; `{cwd}` is where the session lives, which some CLIs want spelled
/// out even when they are started there.
fn vars<'a>(prompt: &'a str, cwd: &'a str) -> HashMap<&'static str, &'a str> {
    HashMap::from([("prompt", prompt), ("cwd", cwd)])
}

// -- the tests below own the fixture the process tests reuse ----------------

/// Reads one of the two per-turn argv templates, naming the field when it is
/// missing. The catalogue validates both for this driver; a hand-written local
/// override is the one way to get here without them.
fn turn_args(entry: &CliEntry, args: Option<&[String]>, field: &str) -> Result<Vec<String>> {
    match args {
        Some(args) if !args.is_empty() => Ok(args.to_vec()),
        _ => bail!(
            "{}: [pm].{field} is missing, and the oneshot-continue driver needs it to start a turn",
            entry.cli.id
        ),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::events::fixture;
    use super::*;

    pub(super) const TEXT_PATH: &str = "$.text";

    /// A valid `oneshot-continue` entry, built by patching the shared stream
    /// fixture through the real parser -- so every field the catalogue
    /// requires is present and none of them is invented here.
    pub(super) fn source(bin: &str, first: &[&str], continued: &[&str], prompt: &str) -> String {
        let first = serde_json::to_string(first).unwrap();
        let continued = serde_json::to_string(continued).unwrap();
        fixture::source(bin, &[], prompt, TEXT_PATH, Some("$.usage"))
            .replace(r#"driver = "stream""#, r#"driver = "oneshot-continue""#)
            // The leading newline keeps this off `[models].discovery_args`,
            // which ends in the same four characters.
            .replace(
                "\nargs = []\n",
                &format!("\nfirst_args = {first}\ncontinue_args = {continued}\n"),
            )
    }

    pub(super) fn entry(bin: &str, first: &[&str], continued: &[&str], prompt: &str) -> CliEntry {
        fixture::parse(&source(bin, first, continued, prompt))
    }

    /// `unwrap_err` would need `Debug` on a live session, which is not worth
    /// requiring of a type nobody prints.
    fn start_err(entry: &CliEntry) -> String {
        match OneshotDriver::start(entry, &[], None) {
            Ok(_) => panic!("the driver started a session it should have refused"),
            Err(err) => format!("{err:#}"),
        }
    }

    /// The driver spawns nothing at `start`, so without this check a CLI that
    /// is not installed would reach an interactive pane and only then die.
    #[test]
    fn a_missing_binary_is_refused_at_launch_rather_than_on_the_first_turn() {
        let rendered = start_err(&entry(
            "mana-no-such-binary",
            &["--print", "{prompt}"],
            &["--continue", "{prompt}"],
            "argv",
        ));
        assert!(rendered.contains("Fake CLI"), "{rendered}");
        assert!(rendered.contains("mana-no-such-binary"), "{rendered}");
    }

    /// A placeholder with no value at turn time would otherwise be handed to
    /// the CLI as a literal brace, one turn after the launch that could have
    /// reported it.
    #[test]
    fn an_unfillable_placeholder_is_a_startup_error_naming_the_field() {
        let rendered = start_err(&entry(
            "sh",
            &["--print", "{prompt}"],
            &["--model", "{model}", "{prompt}"],
            "argv",
        ));
        assert!(rendered.contains("[pm].continue_args"), "{rendered}");
        assert!(rendered.contains("model"), "{rendered}");
    }

    /// A turn travels one way or the other. Both would send it twice, which on
    /// a paid CLI is a bill and a confused agent.
    #[test]
    fn a_stdin_turn_refuses_a_prompt_placeholder_left_in_argv() {
        let rendered = start_err(&entry(
            "sh",
            &["--print", "{prompt}"],
            &["--continue"],
            "stdin",
        ));
        assert!(rendered.contains("first_args"), "{rendered}");
        assert!(rendered.contains("stdin"), "{rendered}");
    }

    /// The catalogue rejects an entry without them, so this is the local
    /// override's error message rather than a shipped one -- it still has to
    /// name the field somebody must add.
    #[test]
    fn a_missing_turn_template_is_reported_naming_the_field() {
        let entry = entry("sh", &["-c"], &["-c"], "argv");
        let err = turn_args(&entry, None, "first_args").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("[pm].first_args"), "{rendered}");
        assert!(rendered.contains("fake"), "{rendered}");
    }

    #[test]
    fn an_entry_for_another_driver_is_refused_before_anything_runs() {
        let stream = fixture::entry("sh", &[], "stdin-jsonl", TEXT_PATH, None);
        let rendered = start_err(&stream);
        assert!(rendered.contains("oneshot-continue"), "{rendered}");
    }
}

/// Everything that needs a real process. Unix-only because the fake CLI is a
/// shell script, like every other process test in the tree.
#[cfg(all(test, unix))]
mod process_tests {
    use super::super::events::fixture;
    use super::tests::{entry, source};
    use super::*;
    use std::time::Duration;

    /// Writes an executable fake CLI and returns its path.
    fn script(dir: &Path, body: &str) -> String {
        crate::subprocess::write_executable(dir, "fake-cli", &format!("#!/bin/sh\n{body}\n"))
            .to_string_lossy()
            .into_owned()
    }

    fn driver(bin: &str, first: &[&str], continued: &[&str]) -> OneshotDriver {
        OneshotDriver::start(&entry(bin, first, continued, "argv"), &[], None).unwrap()
    }

    fn next(driver: &OneshotDriver) -> PmEvent {
        driver
            .events()
            .recv_timeout(Duration::from_secs(10))
            .expect("the PM produced no further event")
    }

    /// Waits for `path` to hold at least `lines` lines and returns them: how a
    /// test observes a child that writes on its own schedule.
    fn wait_for_lines(path: &Path, lines: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let contents = std::fs::read_to_string(path).unwrap_or_default();
            let seen: Vec<String> = contents.lines().map(str::to_string).collect();
            if seen.len() >= lines {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "{} never reached {lines} lines; it holds: {seen:?}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the turn in flight to end.
    ///
    /// The last thing a turn says arrives *before* its process exits, so a
    /// test that cares about the gap between turns has to wait for the reap
    /// rather than for the answer.
    fn wait_until_idle(driver: &OneshotDriver) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while driver.in_flight.lock().unwrap().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            driver.in_flight.lock().unwrap().is_none(),
            "the turn never ended"
        );
    }

    fn process_is_alive(pid: i32) -> bool {
        // SAFETY: signal 0 runs the existence/permission check only; nothing is
        // delivered and nothing is dereferenced.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// The whole point of the driver: turn one is a process started from
    /// `first_args`, turn two is another process started from `continue_args`,
    /// and `{prompt}` carries what the user typed in both.
    #[test]
    fn the_first_turn_uses_first_args_and_every_later_turn_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let argv = tmp.path().join("argv.txt");
        let bin = script(
            tmp.path(),
            // The log path is baked into the script rather than passed as an
            // argument the argv assertion would then have to ignore.
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\necho '{{\"text\":\"ack\"}}'",
                argv.display()
            ),
        );
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("first turn").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        // The process that held the turn is gone, which is this transport's
        // whole answer to "is the PM free?".
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        driver.send_user("second turn").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);

        assert_eq!(
            wait_for_lines(&argv, 2),
            ["--print first turn", "--continue second turn"]
        );
        // Between turns there is no process at all -- that is what makes this
        // driver a session on disk rather than a session in mana.
        wait_until_idle(&driver);
        driver.shutdown().unwrap();
    }

    /// Resuming on this driver: there is no first turn, because the
    /// conversation the CLI still holds *is* the first turns. So turn one is
    /// already a `continue_args` process, and no `[pm].resume_args` is
    /// involved -- the template that continues is the one the entry declares
    /// for every later turn anyway.
    #[test]
    fn a_resumed_session_continues_from_its_very_first_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let argv = tmp.path().join("argv.txt");
        let bin = script(
            tmp.path(),
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\necho '{{\"text\":\"ack\"}}'",
                argv.display()
            ),
        );
        let entry = entry(
            &bin,
            &["--print", "{prompt}"],
            &["--continue", "{prompt}"],
            "argv",
        );
        let mut driver = OneshotDriver::start(&entry, &[], Some(Resume::default())).unwrap();

        driver.send_user("first turn").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        // The process that held the turn is gone, which is this transport's
        // whole answer to "is the PM free?".
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        driver.send_user("second turn").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);

        assert_eq!(
            wait_for_lines(&argv, 2),
            ["--continue first turn", "--continue second turn"]
        );
        driver.shutdown().unwrap();
    }

    /// A turn ending is not a session ending. v1 could not tell the two apart
    /// on any transport; here they are genuinely different events.
    #[test]
    fn a_turn_that_exits_cleanly_leaves_the_session_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "echo '{\"text\":\"ack\"}'\nexit 0");
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("one").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        driver.send_user("two").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        // Nothing else arrived: no `Exited` between the two turns -- a turn
        // ending and a session ending are different events here.
        assert!(driver.events().try_recv().is_err());
        driver.shutdown().unwrap();
    }

    /// Two agy instances at once died in 8 seconds, so a queued turn waits for
    /// the process in flight instead of racing it.
    #[test]
    fn turns_queued_during_a_turn_run_one_after_the_other() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("order.txt");
        let bin = script(
            tmp.path(),
            &format!(
                "printf 'start %s\\n' \"$2\" >> '{log}'\n\
                 sleep 0.3\n\
                 printf 'end %s\\n' \"$2\" >> '{log}'\n\
                 echo '{{\"text\":\"ack\"}}'",
                log = log.display()
            ),
        );
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        // Both sent before the first can possibly have finished.
        driver.send_user("one").unwrap();
        driver.send_user("two").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);

        assert_eq!(
            wait_for_lines(&log, 4),
            ["start one", "end one", "start two", "end two"]
        );
        driver.shutdown().unwrap();
    }

    /// The turn died badly, so the session is over -- and the reason, which is
    /// on stderr, arrives before the code that only says it happened.
    #[test]
    fn a_turn_that_exits_badly_ends_the_session_and_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "echo 'boom: no credentials found' >&2\nexit 7");
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("one").unwrap();
        assert_eq!(
            next(&driver),
            PmEvent::Stderr("boom: no credentials found".to_string())
        );
        assert_eq!(next(&driver), PmEvent::Exited { code: Some(7) });

        // ...and the session refuses further turns rather than starting a
        // process that would fail the same way.
        let err = driver.send_user("two").unwrap_err();
        assert!(format!("{err:#}").contains("closed"), "{err:#}");
    }

    /// A CLI that changes its stream shape shows up as ugly lines in the chat
    /// pane, never as a PM that went quiet.
    #[test]
    fn output_that_matches_nothing_survives_as_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "echo 'warning: your session will expire soon'\necho '{\"text\":\"ack\"}'",
        );
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("one").unwrap();
        assert_eq!(
            next(&driver),
            PmEvent::Raw("warning: your session will expire soon".to_string())
        );
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        driver.shutdown().unwrap();
    }

    /// Usage is enrichment, not prose, and it comes through the same shared
    /// map every other non-ACP driver reads.
    #[test]
    fn usage_is_extracted_through_the_shared_event_map() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "echo '{\"text\":\"ack\",\"usage\":{\"output_tokens\":44}}'",
        );
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("one").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(
            next(&driver),
            PmEvent::Usage(serde_json::json!({"output_tokens": 44}))
        );
        driver.shutdown().unwrap();
    }

    /// A CLI that takes its prompt on stdin instead of argv is a catalogue
    /// value, not a second driver.
    #[test]
    fn a_stdin_prompt_reaches_the_child_and_is_closed_behind_it() {
        let tmp = tempfile::tempdir().unwrap();
        let received = tmp.path().join("received.txt");
        // `cat` returns only at EOF, so this also proves stdin was closed.
        let bin = script(
            tmp.path(),
            &format!(
                "cat > '{}'\necho '{{\"text\":\"ack\"}}'",
                received.display()
            ),
        );
        let entry = fixture::parse(&source(&bin, &["--print"], &["--continue"], "stdin"));
        let mut driver = OneshotDriver::start(&entry, &[], None).unwrap();

        driver.send_user("a turn on stdin").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(
            std::fs::read_to_string(&received).unwrap(),
            "a turn on stdin\n"
        );
        driver.shutdown().unwrap();
    }

    /// The tool channel's flags land after the catalogue's own, where a CLI
    /// expects trailing options.
    #[test]
    fn extra_args_are_appended_after_the_catalogue_args() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "echo \"argv: $*\" >&2");
        let entry = entry(&bin, &["--print", "{prompt}"], &["--continue"], "argv");
        let extra = ["--mcp-config".to_string(), "/tmp/mana-mcp.json".to_string()];
        let mut driver = OneshotDriver::start(&entry, &extra, None).unwrap();

        driver.send_user("hi").unwrap();
        assert_eq!(
            next(&driver),
            PmEvent::Stderr("argv: --print hi --mcp-config /tmp/mana-mcp.json".to_string())
        );
        driver.shutdown().unwrap();
    }

    /// The session is on disk, so killing the turn in flight costs one answer
    /// and nothing else -- but leaving it running would cost the next session,
    /// on a CLI that refuses to run twice at once.
    #[test]
    fn shutdown_kills_the_turn_in_flight_and_takes_its_children_along() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        // The backgrounded sleep stands in for whatever the CLI spawned.
        let bin = script(
            tmp.path(),
            &format!(
                "sleep 30 &\necho $! > '{}'\necho '{{\"text\":\"thinking\"}}'\nsleep 30",
                pidfile.display()
            ),
        );
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("one").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("thinking".to_string()));
        let pid = driver.in_flight.lock().unwrap().as_ref().unwrap().pid as i32;

        let started = Instant::now();
        driver.shutdown().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "shutdown hung");
        assert_eq!(next(&driver), PmEvent::Exited { code: None });

        let grandchild: i32 = wait_for_lines(&pidfile, 1)[0].trim().parse().unwrap();
        for (label, victim) in [("the turn", pid), ("its child", grandchild)] {
            // Reparenting to init and the reap that follows are not instant.
            let deadline = Instant::now() + Duration::from_secs(3);
            while process_is_alive(victim) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !process_is_alive(victim),
                "{label} ({victim}) survived shutdown"
            );
        }
    }

    /// `Exited` is the transport's promise that the session is over, and an
    /// idle session has no process to learn it from.
    #[test]
    fn shutdown_between_turns_still_reports_the_exit_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut driver = driver(
            &script(tmp.path(), "echo '{\"text\":\"ack\"}'"),
            &["--print", "{prompt}"],
            &["--continue", "{prompt}"],
        );
        driver.send_user("one").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("ack".to_string()));
        assert_eq!(next(&driver), PmEvent::TurnEnded);
        wait_until_idle(&driver);

        driver.shutdown().unwrap();
        assert_eq!(next(&driver), PmEvent::Exited { code: Some(0) });
        // Once, and only once: shutdown is idempotent because `Drop` calls it
        // again on every session.
        driver.shutdown().unwrap();
        assert!(driver.events().try_recv().is_err());
    }

    /// A turn queued behind the one being killed must never start: it would be
    /// a process nobody is left to read.
    #[test]
    fn a_turn_queued_when_the_session_closes_is_never_started() {
        let tmp = tempfile::tempdir().unwrap();
        let argv = tmp.path().join("started.txt");
        let bin = script(
            tmp.path(),
            &format!("printf '%s\\n' \"$*\" >> '{}'\nsleep 5", argv.display()),
        );
        let mut driver = driver(&bin, &["--print", "{prompt}"], &["--continue", "{prompt}"]);

        driver.send_user("one").unwrap();
        wait_for_lines(&argv, 1);
        driver.send_user("two").unwrap();
        driver.shutdown().unwrap();

        // The second turn had a whole second to start and never did.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(wait_for_lines(&argv, 1), ["--print one"]);
    }

    /// Dropping the driver must not leave a turn running: nothing else in mana
    /// knows the pid once the handle is gone.
    #[test]
    fn dropping_the_driver_kills_the_turn_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let mut driver = driver(
            &script(tmp.path(), "echo '{\"text\":\"hi\"}'\nsleep 30"),
            &["--print", "{prompt}"],
            &["--continue", "{prompt}"],
        );
        driver.send_user("one").unwrap();
        assert_eq!(next(&driver), PmEvent::Text("hi".to_string()));
        let pid = driver.in_flight.lock().unwrap().as_ref().unwrap().pid as i32;
        drop(driver);

        let deadline = Instant::now() + Duration::from_secs(3);
        while process_is_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!process_is_alive(pid), "the turn survived its driver");
    }
}
