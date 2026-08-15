//! The process plumbing both long-lived PM drivers need: read a pipe line by
//! line, wait for a child, kill it and everything it started.
//!
//! Extracted when the second driver arrived. `stream` and `acp` frame their
//! messages completely differently -- newline-delimited vendor JSON versus
//! JSON-RPC 2.0 -- but underneath they are the same shape: one persistent
//! child, one thread per pipe, a channel out, and a kill that must reach the
//! MCP server the CLI spawned rather than only the CLI itself. Copying those
//! hundred lines into the second driver would have meant fixing the next
//! shutdown bug twice.
//!
//! Deliberately not shared with `spawn.rs`, which does something that looks
//! the same for a different policy: a sub-agent is killed the moment it blows
//! its budget, while a PM is only killed after the graceful close window has
//! passed. Folding both into one helper would mean a policy parameter around
//! two libc calls.

use super::PmEvent;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a wait loop asks whether the child is done. Small enough that
/// shutdown feels immediate, large enough not to spin a core while a PM thinks.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long shutdown lets the PM leave on its own after stdin closes, before
/// killing it. Generous on purpose: an agent mid-turn may still be flushing a
/// last message, and losing it to an impatient SIGKILL would look like the bug
/// these drivers exist to fix.
pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// How long a reader thread gets to finish once its pipe is at EOF.
pub(super) const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Reads `source` line by line until EOF, stopping early if `sink` says the
/// other end is gone.
pub(super) fn read_lines(source: impl Read, mut sink: impl FnMut(String) -> bool) {
    let mut reader = BufReader::new(source);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => return, // every write end closed
            Ok(_) => {
                while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                    buffer.pop();
                }
                // Lossy on purpose: a CLI's output is evidence to be read,
                // never something to reject for encoding.
                if !sink(String::from_utf8_lossy(&buffer).into_owned()) {
                    return;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            // A broken pipe is the normal end of a killed child, and any other
            // error is just as terminal for a stream we cannot read.
            Err(_) => return,
        }
    }
}

/// Spawns a thread that turns each line of `source` into events.
pub(super) fn pump(
    source: impl Read + Send + 'static,
    sender: Sender<PmEvent>,
    to_events: impl Fn(String) -> Vec<PmEvent> + Send + 'static,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        read_lines(source, |line| {
            to_events(line)
                .into_iter()
                .all(|event| sender.send(event).is_ok())
        });
    })
}

/// Drains a PM's stderr into visible chat lines.
///
/// Not discarded, because a PM that dies on startup says why there and
/// swallowing it is how v1 left users with a silently dead session. It is also
/// why an unread pipe is not an option -- a full stderr buffer blocks the
/// child mid-write.
pub(super) fn pump_stderr(
    source: impl Read + Send + 'static,
    sender: Sender<PmEvent>,
) -> JoinHandle<()> {
    pump(source, sender, |line| vec![PmEvent::Raw(line)])
}

/// Waits for the child to be gone and reports its exit code.
///
/// Polls instead of blocking in `wait` because the lock is shared with
/// `shutdown`: a blocking wait held under the mutex would deadlock the one call
/// meant to end a PM that stopped listening.
pub(super) fn reap(child: &Mutex<Child>) -> Option<i32> {
    loop {
        match child.lock().unwrap().try_wait() {
            Ok(Some(status)) => return status.code(),
            // Unreachable child: reporting no code is the honest answer and
            // keeps `Exited` on its promise to always arrive.
            Err(_) => return None,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Waits for a thread to finish, but never past `deadline`.
///
/// Waiting longer would hand mana's shutdown timing to whatever still holds the
/// write end of a pipe -- a grandchild that survived the kill, say. A detached
/// thread costs a blocked read on a stream nobody reads and no correctness.
pub(super) fn join_or_detach(handle: JoinHandle<()>, deadline: Instant) {
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
pub(super) fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // 0 = "become the leader of your own group", so the child's pgid equals its
    // pid. Without this the PM shares mana's group, and killing that group on
    // shutdown would kill mana -- while a Ctrl-C meant for the TUI would go to
    // the PM instead.
    command.process_group(0);
}

#[cfg(windows)]
pub(super) fn set_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // The PM and its descendants form a group of their own, so a console
    // control event cannot travel back up into mana's TUI.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Kills the PM and everything it started.
#[cfg(unix)]
pub(super) fn kill_group(child: &mut Child, pid: u32) {
    // The PM leads its own group (see `set_process_group`), so the group covers
    // what it spawned -- its MCP server above all, which would otherwise
    // survive as a stdio process talking to nobody.
    // The `!= 0` guard is not paranoia: killpg(0) signals *our own* group, mana
    // included.
    if let Ok(pgid) = i32::try_from(pid)
        && pgid != 0
    {
        // SAFETY: killpg takes two integers, dereferences nothing, and reports
        // an already-gone group as ESRCH -- which we ignore anyway.
        let _ = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    // Then the child by name, in case the group signal reached nothing: a CLI
    // that called setsid() has left our group, and killpg would return ESRCH
    // while the process kept running -- turning the wait that follows into an
    // unbounded one.
    let _ = child.kill();
}

#[cfg(windows)]
pub(super) fn kill_group(child: &mut Child, _pid: u32) {
    // Windows has no killpg. `Child::kill` is TerminateProcess, which ends the
    // named process only -- anything the PM spawned survives it. Killing the
    // tree properly needs a Job Object (`windows-sys`); until a Windows PM
    // session has actually been measured, the honest statement is: the PM dies,
    // its children may leak.
    let _ = child.kill();
}
