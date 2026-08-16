//! Plain-process spawner: the one way mana runs a sub-agent.
//!
//! Sub-agents speak no protocol. mana runs a headless CLI invocation, hands it
//! exactly one prompt, and observes it from the outside -- exit code, output,
//! duration. Headless `-p`/`--print` modes are built for pipes, so this is
//! `std::process::Command` and not a PTY: a terminal emulator between mana and
//! bytes it already receives verbatim would only add a screen to re-parse.
//!
//! Nothing here knows any CLI's name, flag or quirk. argv arrives fully
//! substituted (see `catalog::substitute`); this module only spawns, feeds,
//! times and captures.
//!
//! `run` is blocking and always ends on the same two steps -- reap or kill,
//! then report -- so no path leaves a sub-agent running behind mana's back.

use crate::catalog::PromptMode;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Hard ceiling on each captured stream. A runaway agent can print without
/// bound, and mana holds this in memory before it reaches a log line.
pub const OUTPUT_CAP_BYTES: usize = 8 * 1024 * 1024;

/// How often the wait loop asks whether the child is done. Small enough that a
/// sub-second timeout is still honoured, large enough not to spin a core while
/// an agent thinks for ten minutes.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long the capture threads get to reach EOF once the child is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

const READ_CHUNK: usize = 8 * 1024;

/// Marker appended to a stream that hit the cap, so a truncated capture can
/// never be mistaken for the CLI's complete output.
pub fn truncation_marker(cap: usize) -> String {
    format!("\n[mana: output truncated -- only the first {cap} bytes were kept]\n")
}

/// How the prompt reaches the process.
#[derive(Debug)]
pub enum PromptDelivery {
    /// The prompt is already one of `SpawnSpec::args`. Assembling argv from the
    /// catalogue template is the caller's job (`catalog::substitute`), because
    /// where the prompt sits in argv is catalogue knowledge and the spawner is
    /// meant to hold none.
    Argv,
    /// The prompt is written to stdin, which is then closed.
    Stdin(String),
}

impl PromptDelivery {
    /// Maps a catalogue `prompt` mode onto a sub-agent delivery.
    pub fn from_mode(mode: PromptMode, prompt: &str) -> Result<Self> {
        match mode {
            PromptMode::Argv => Ok(PromptDelivery::Argv),
            PromptMode::Stdin => Ok(PromptDelivery::Stdin(prompt.to_string())),
            // Not a gap to fill later: framing turns as JSONL implies a session
            // that stays open and answers back, which is the PM drivers' whole
            // job. A sub-agent gets one prompt and is judged from the outside.
            PromptMode::StdinJsonl => bail!(
                "prompt mode 'stdin-jsonl' is a PM transport, not a sub-agent delivery: \
                 a sub-agent takes a single prompt over argv or stdin"
            ),
        }
    }
}

pub struct SpawnSpec {
    /// Already resolved to a path on disk by `CliMeta::resolve`, so this module
    /// never asks PATH a question the rest of mana answered differently.
    pub bin: PathBuf,
    pub args: Vec<String>,
    /// Where the process runs -- a task's worktree for write roles.
    pub cwd: PathBuf,
    pub prompt: PromptDelivery,
    /// Budget for the whole run, counted from the spawn.
    pub timeout: Duration,
}

#[derive(Debug)]
pub struct SpawnOutcome {
    /// Also delivered via `on_spawn`; kept on the outcome for symmetry.
    /// Unread until `mana ps` (4.2) — the registry row is the live consumer.
    #[allow(dead_code)]
    pub pid: u32,
    /// `None` when the process was killed by a signal, which on unix is what a
    /// timeout looks like -- read it together with `timed_out`.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Wall clock from spawn to reap. Feeds `gen_ai.client.operation.duration`.
    pub duration: Duration,
    pub timed_out: bool,
}

/// Runs one sub-agent to completion (or to its timeout) and reports what
/// happened. Blocking: the caller owns the concurrency policy, since the
/// catalogue's `max_concurrent` is per CLI and this module sees one process.
///
/// `on_spawn` fires with the pid the moment the process exists, before any
/// waiting, so the caller can write its registry record while the process is
/// still alive -- a pid recorded after the fact is of no use to `mana kill`.
pub fn run(spec: &SpawnSpec, on_spawn: impl FnOnce(u32)) -> Result<SpawnOutcome> {
    // Checked here so a worktree that was never created says so, instead of
    // surfacing as the same ENOENT that a missing binary produces.
    if !spec.cwd.is_dir() {
        bail!("working directory does not exist: {}", spec.cwd.display());
    }

    let mut command = Command::new(&spec.bin);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(match spec.prompt {
            // A sub-agent runs unattended: inheriting mana's stdin would let a
            // CLI that asks a question wait forever on a terminal nobody is
            // watching. `null` turns the question into an immediate EOF.
            PromptDelivery::Argv => Stdio::null(),
            PromptDelivery::Stdin(_) => Stdio::piped(),
        });
    set_process_group(&mut command);

    let started = Instant::now();
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn '{}' in {}",
            spec.bin.display(),
            spec.cwd.display()
        )
    })?;
    let pid = child.id();
    on_spawn(pid);

    // One thread per pipe: a child blocks in write() once a pipe buffer fills
    // (64 KiB), so draining stdout to EOF on a single thread deadlocks any CLI
    // that also writes more than a bufferful to stderr -- and both are exactly
    // what a verbose agent does.
    let (stdout, stdout_reader) = capture(child.stdout.take().expect("stdout was piped"));
    let (stderr, stderr_reader) = capture(child.stderr.take().expect("stderr was piped"));

    if let PromptDelivery::Stdin(prompt) = &spec.prompt {
        deliver(child.stdin.take().expect("stdin was piped"), prompt.clone());
    }

    let (exit_code, timed_out) = wait(&mut child, pid, started + spec.timeout)?;
    // Taken before the drain window: that window is mana's bookkeeping, not
    // time the agent spent working.
    let duration = started.elapsed();

    let drain_deadline = Instant::now() + DRAIN_GRACE;
    await_eof(stdout_reader, drain_deadline);
    await_eof(stderr_reader, drain_deadline);

    Ok(SpawnOutcome {
        pid,
        exit_code,
        stdout: stdout.lock().unwrap().snapshot(),
        stderr: stderr.lock().unwrap().snapshot(),
        duration,
        timed_out,
    })
}

/// Polls for exit until `deadline`, then kills. Returns the exit code (`None`
/// if signalled) and whether the deadline was the reason the process stopped.
fn wait(child: &mut Child, pid: u32, deadline: Instant) -> Result<(Option<i32>, bool)> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status.code(), false)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_group(child, pid);
                    // SIGKILL cannot be caught, so this reaps rather than waits.
                    let status = child.wait().context("reaping a killed sub-agent")?;
                    return Ok((status.code(), true));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                // Reporting the error while leaving the process running would
                // leak an agent that still holds a quota slot. Kill, then fail.
                kill_group(child, pid);
                let _ = child.wait();
                return Err(err).context("polling a sub-agent for exit");
            }
        }
    }
}

/// Waits for a capture thread to hit EOF, but never past `deadline`.
///
/// Unbounded joining would hand mana's timeout enforcement to the very process
/// that ignored it: a grandchild that somehow survived the kill still holds the
/// write end of the pipe, so the reader would never see EOF. An abandoned
/// thread keeps appending to a capped buffer nobody reads, which costs a
/// bounded amount of memory and no correctness.
fn await_eof(reader: JoinHandle<()>, deadline: Instant) {
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn capture<R: Read + Send + 'static>(mut pipe: R) -> (Arc<Mutex<CappedBuffer>>, JoinHandle<()>) {
    let buffer = Arc::new(Mutex::new(CappedBuffer::new(OUTPUT_CAP_BYTES)));
    let sink = Arc::clone(&buffer);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break, // every write end closed
                Ok(read) => sink.lock().unwrap().push(&chunk[..read]),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                // A broken pipe is the normal end of a killed child, and any
                // other error is just as terminal for a stream we cannot read.
                Err(_) => break,
            }
        }
    });
    (buffer, reader)
}

/// Writes the prompt and closes stdin, on its own thread.
///
/// The thread is not an optimisation: a prompt larger than the pipe buffer
/// blocks in write() until the CLI reads it, and on the main thread that would
/// block before the timeout loop ever starts -- leaving the timeout
/// unenforceable for precisely the briefs big enough to be slow.
fn deliver(mut pipe: ChildStdin, prompt: String) {
    std::thread::spawn(move || {
        // A CLI that exits without reading its prompt yields EPIPE here, and
        // that failure is already visible in the exit code and the output.
        let _ = pipe.write_all(prompt.as_bytes());
        // Dropping the handle closes stdin: a CLI that reads to EOF hangs
        // forever otherwise.
    });
}

#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // 0 = "become the leader of your own group", so the child's pgid equals
    // its pid. Without this the child shares mana's group, and killing that
    // group on timeout would kill mana.
    command.process_group(0);
}

#[cfg(windows)]
fn set_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // The child and its descendants form a group of their own, so a console
    // control event aimed at a sub-agent cannot travel back up into the TUI.
    // See `kill_group` for what a timeout actually kills here.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn kill_group(child: &mut Child, pid: u32) {
    // The child leads its own group (see `set_process_group`), so its pgid is
    // its pid and the group covers whatever it started -- a CLI that
    // backgrounds a helper, a shell wrapper, a language runtime's children.
    // SIGKILL rather than SIGTERM: the run has already blown its budget, and a
    // CLI with a TERM handler could otherwise ignore the timeout outright.
    //
    // The pid cannot have been recycled at this point: the child is unreaped
    // (`wait` follows immediately), so the kernel still holds its pid, and
    // with it the group id.
    if pid != 0 {
        // killpg(0) would signal *our own* group, mana included.
        if let Ok(pgid) = i32::try_from(pid) {
            // SAFETY: killpg takes two integers, dereferences nothing, and
            // reports an already-gone group as ESRCH -- which we ignore anyway.
            let _ = unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
    // Then the child by name, in case the group signal reached nothing: a CLI
    // that calls setsid() has left our group, and killpg would return ESRCH
    // while the process kept running -- turning the wait that follows into an
    // unbounded one. Measured with the group kill deliberately disabled: the
    // timeout test's child ran its full 30s. Descendants are unreachable in
    // that case, but the timeout stays a timeout.
    let _ = child.kill();
}

#[cfg(windows)]
fn kill_group(child: &mut Child, pid: u32) {
    // Windows has no killpg, and CREATE_NEW_PROCESS_GROUP only makes a
    // CTRL_BREAK possible -- which reaches processes sharing mana's console,
    // which a TUI cannot guarantee, and which a CLI may ignore. `Child::kill`
    // is TerminateProcess on the named process alone, and since the catalogue
    // resolves the npm CLIs to their `.cmd` shims that named process is
    // `cmd.exe`: the agent is its child and would survive its own timeout,
    // still burning quota, still holding the worktree's files and the capture
    // pipes' write ends open.
    //
    // `mana kill` already walks the tree here (`taskkill /T /F`), so this calls
    // the same function rather than growing a second Windows kill: a timeout
    // and an operator kill have to leave the machine in the same state, and a
    // Job Object would mean a dependency for what one shell-out already does.
    let _ = crate::cli::kill::signal(pid);
    // Then the child by name, for the same reason as unix: if `taskkill` is
    // missing or refuses the tree, the direct child still has to die or the
    // `child.wait()` that follows this call never returns.
    let _ = child.kill();
}

/// Output capture with a hard ceiling, keeping the **first** bytes.
///
/// Head and not tail because the signatures the observer matches on -- a quota
/// refusal, an auth error, a "402" -- are printed when the CLI gives up, which
/// is early; a runaway agent's later megabytes are noise. Reading continues
/// past the cap, discarding, because a reader that stops reading blocks the
/// child on a full pipe.
struct CappedBuffer {
    bytes: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl CappedBuffer {
    fn new(cap: usize) -> Self {
        CappedBuffer {
            bytes: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        // Saturating because a panic here would poison the mutex and take the
        // whole dispatch down over an arithmetic detail in a capture thread.
        let room = self.cap.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..room.min(chunk.len())]);
    }

    /// Lossy on purpose: the cap can land mid-codepoint, and a CLI's output is
    /// evidence to be read, never something to reject for encoding.
    fn snapshot(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            text.push_str(&truncation_marker(self.cap));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_jsonl_is_rejected_as_a_pm_driver_concern() {
        let err = PromptDelivery::from_mode(PromptMode::StdinJsonl, "brief").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("stdin-jsonl"), "{rendered}");
    }

    #[test]
    fn argv_mode_expects_the_caller_to_have_substituted_the_prompt() {
        assert!(matches!(
            PromptDelivery::from_mode(PromptMode::Argv, "brief").unwrap(),
            PromptDelivery::Argv
        ));
    }

    #[test]
    fn stdin_mode_carries_the_prompt() {
        let delivery = PromptDelivery::from_mode(PromptMode::Stdin, "brief").unwrap();
        assert!(matches!(delivery, PromptDelivery::Stdin(text) if text == "brief"));
    }

    #[test]
    fn capped_buffer_keeps_the_head_and_marks_the_truncation() {
        let mut buffer = CappedBuffer::new(6);
        buffer.push(b"head");
        buffer.push(b"-and-tail");
        assert_eq!(buffer.snapshot(), format!("head-a{}", truncation_marker(6)));
    }

    #[test]
    fn capped_buffer_leaves_output_under_the_ceiling_untouched() {
        let mut buffer = CappedBuffer::new(64);
        buffer.push(b"exit 1: rate limit reached");
        assert_eq!(buffer.snapshot(), "exit 1: rate limit reached");
    }
}

/// The Windows twin of `process_tests::timeout_kills_the_whole_process_group`.
/// Compiled on every host (`cargo check --target x86_64-pc-windows-msvc`) and
/// run only where it can be: the guarantee it asserts is the one this platform
/// did not have, so it is worth carrying even before a Windows runner exists.
#[cfg(all(test, windows))]
mod windows_process_tests {
    use super::*;

    /// Both halves of the Windows bug in one fixture. The spec's `bin` is a
    /// `.cmd`, which is what the catalogue resolves an npm-installed CLI to and
    /// what makes std put a `cmd.exe` between mana and the agent; the fixture
    /// then backgrounds a grandchild, which stands for the agent itself. A
    /// `Child::kill` on the wrapper leaves it running, and the marker file it
    /// writes after the kill is how that leak becomes visible.
    ///
    /// Batch files rather than one `cmd /C` string on purpose: quoting a nested
    /// `start "" /b cmd /c "..."` correctly is its own bug, and a file mana
    /// writes has no quoting at all. `ping` and not `timeout`, because
    /// `timeout` refuses to run with stdin redirected -- which is exactly how
    /// every sub-agent is spawned (`Stdio::null`).
    #[test]
    fn timeout_kills_the_whole_process_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("grandchild-ran.txt");
        let grandchild = tmp.path().join("grandchild.cmd");
        let agent = tmp.path().join("agent.cmd");
        std::fs::write(
            &grandchild,
            format!(
                "@echo off\r\nping -n 4 127.0.0.1 >nul\r\necho alive> \"{}\"\r\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &agent,
            format!(
                "@echo off\r\nstart \"\" /b \"{}\"\r\nping -n 60 127.0.0.1 >nul\r\n",
                grandchild.display()
            ),
        )
        .unwrap();

        let spec = SpawnSpec {
            bin: agent.clone(),
            args: Vec::new(),
            cwd: tmp.path().to_path_buf(),
            prompt: PromptDelivery::Argv,
            timeout: Duration::from_millis(800),
        };
        let outcome = run(&spec, |_| {}).unwrap();
        assert!(outcome.timed_out);

        // Well past the grandchild's own sleep: if the tree kill missed it, it
        // wakes up and writes, which is the leak in file form.
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            assert!(
                !marker.exists(),
                "the grandchild survived the timeout kill and kept working"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(all(test, unix))] // every test here execs unix shell fixtures
mod process_tests {
    use super::*;

    fn sh(script: &str, cwd: &std::path::Path, timeout_ms: u64) -> SpawnSpec {
        SpawnSpec {
            bin: "sh".into(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: cwd.to_path_buf(),
            prompt: PromptDelivery::Argv,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    fn process_is_alive(pid: i32) -> bool {
        // SAFETY: signal 0 runs the existence/permission check only; nothing is
        // delivered and nothing is dereferenced.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn propagates_the_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run(&sh("exit 3", tmp.path(), 5_000), |_| {}).unwrap();
        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.timed_out);
    }

    #[test]
    fn captures_stdout_and_stderr_separately() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run(
            &sh("echo to-stdout; echo to-stderr >&2", tmp.path(), 5_000),
            |_| {},
        )
        .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout.contains("to-stdout"), "{}", outcome.stdout);
        assert!(!outcome.stdout.contains("to-stderr"), "{}", outcome.stdout);
        assert!(outcome.stderr.contains("to-stderr"), "{}", outcome.stderr);
    }

    #[test]
    fn runs_in_the_requested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run(&sh("pwd -P", tmp.path(), 5_000), |_| {}).unwrap();
        let expected = tmp.path().canonicalize().unwrap();
        assert_eq!(outcome.stdout.trim(), expected.to_string_lossy());
    }

    #[test]
    fn delivers_the_prompt_on_stdin_and_closes_it() {
        let tmp = tempfile::tempdir().unwrap();
        // `cat` exits only once stdin reaches EOF, so a spawner that wrote the
        // prompt without closing the pipe would hit the timeout instead.
        let spec = SpawnSpec {
            bin: "cat".into(),
            args: Vec::new(),
            cwd: tmp.path().to_path_buf(),
            prompt: PromptDelivery::Stdin("the brief, on stdin".to_string()),
            timeout: Duration::from_secs(5),
        };
        let outcome = run(&spec, |_| {}).unwrap();
        assert!(!outcome.timed_out, "stdin was never closed");
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, "the brief, on stdin");
    }

    #[test]
    fn stdin_is_closed_for_argv_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        // A CLI that reads stdin although it was given an argv prompt must see
        // EOF, not mana's own terminal: inheriting it would hang the dispatch.
        let outcome = run(&sh("cat; echo done", tmp.path(), 5_000), |_| {}).unwrap();
        assert!(!outcome.timed_out);
        assert_eq!(outcome.stdout.trim(), "done");
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        // The backgrounded sleep is the grandchild: killing the direct shell
        // alone would leave it running for another 30s, holding a quota slot
        // and possibly still writing into the worktree.
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pidfile.display());

        let started = Instant::now();
        let outcome = run(&sh(&script, tmp.path(), 400), |_| {}).unwrap();

        assert!(outcome.timed_out);
        assert_eq!(
            outcome.exit_code, None,
            "a signalled process reports no code"
        );
        assert!(outcome.duration >= Duration::from_millis(400));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );

        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Reparenting to init and the reap that follows are not instantaneous.
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_is_alive(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_is_alive(grandchild),
            "grandchild {grandchild} survived the timeout kill"
        );
    }

    #[test]
    fn caps_output_at_the_ceiling_and_keeps_the_head() {
        let tmp = tempfile::tempdir().unwrap();
        // Well past the cap, so a clean exit also proves the child was never
        // blocked by a reader that stopped reading at the ceiling.
        let script = "echo FIRST-LINE; yes filler-line | head -c 9000000";
        let outcome = run(&sh(script, tmp.path(), 30_000), |_| {}).unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        assert!(outcome.stdout.starts_with("FIRST-LINE\n"), "head lost");
        let marker = truncation_marker(OUTPUT_CAP_BYTES);
        assert!(outcome.stdout.ends_with(&marker), "no truncation marker");
        assert_eq!(outcome.stdout.len(), OUTPUT_CAP_BYTES + marker.len());
    }

    #[test]
    fn on_spawn_reports_a_live_pid_before_the_process_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let seen = std::cell::Cell::new(None);
        let outcome = run(&sh("sleep 0.3", tmp.path(), 5_000), |pid| {
            assert!(
                process_is_alive(pid as i32),
                "callback fired after the process was gone"
            );
            seen.set(Some(pid));
        })
        .unwrap();
        assert_eq!(seen.get(), Some(outcome.pid));
        assert_eq!(outcome.exit_code, Some(0));
    }

    #[test]
    fn missing_working_directory_is_named_in_the_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-worktree");
        let err = run(&sh("true", &missing, 5_000), |_| {}).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("no-such-worktree"), "{rendered}");
    }
}
