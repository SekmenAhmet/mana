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
//! What `spawn.rs` (the sub-agent spawner) shares with this is the plumbing
//! that carries no policy -- putting a child in its own group, joining a reader
//! thread with a deadline -- which lives in `crate::subprocess` and is
//! re-exported below so the drivers keep one door onto their process handling.
//!
//! The kills stay apart. On unix they read alike and are not the same decision:
//! a sub-agent is killed the moment it blows its budget, a PM only after the
//! graceful close window. On Windows they are not even the same code -- a
//! sub-agent's timeout shells out to `taskkill /T /F` to reach the tree behind
//! the `.cmd` shim mana resolves an npm CLI to, while a PM session has never
//! been measured there and says so rather than pretending.

use super::PmEvent;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::process::Child;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) use crate::subprocess::{join_or_detach, set_process_group};

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

/// Drains a PM's stderr into visible chat lines.
///
/// Not discarded, because a PM that dies on startup says why there and
/// swallowing it is how v1 left users with a silently dead session. It is also
/// why an unread pipe is not an option -- a full stderr buffer blocks the
/// child mid-write.
///
/// `Stderr` rather than `Raw`, which renders the same: this is the one place
/// that knows which pipe a line came off, and a report that has to quote the
/// PM's own explanation cannot work that out afterwards (#189).
pub(super) fn pump_stderr(
    source: impl Read + Send + 'static,
    sender: Sender<PmEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        read_lines(source, |line| sender.send(PmEvent::Stderr(line)).is_ok());
    })
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

/// The half of a long-lived PM that `stream` and `acp` held identically: the
/// process, the pid it was spawned with, the thread reading its stdout, and how
/// long a shutdown stays polite before it kills.
///
/// What is *not* here is stdin. `stream` owns its write end outright while
/// `acp` shares it with a reader thread that has to answer the agent's
/// requests, so each driver drops its own and only then hands over to
/// `close_and_wait` -- that one field was the entire difference between two
/// otherwise byte-identical shutdowns, and it is a field, not a lifecycle.
pub(super) struct PersistentChild {
    child: Arc<Mutex<Child>>,
    /// Captured at spawn: after the child is reaped the pid is meaningless, and
    /// `kill_group` must never signal a recycled one.
    pid: u32,
    /// Taken by `close_and_wait`, which joins it so `Exited` is queued before
    /// the shutdown that called it returns.
    reader: Option<JoinHandle<()>>,
    close_grace: Duration,
}

impl PersistentChild {
    /// `child` is shared because the reader thread reaps it too; `reader` is
    /// the thread that owns stdout and sends `Exited` when it reaches EOF.
    pub(super) fn new(child: Arc<Mutex<Child>>, pid: u32, reader: JoinHandle<()>) -> Self {
        PersistentChild {
            child,
            pid,
            reader: Some(reader),
            close_grace: CLOSE_GRACE,
        }
    }

    /// Waits out the exit the caller's stdin close asked for, kills whatever is
    /// still running, and waits for the reader to report it.
    ///
    /// Both waits are bounded: the first by `close_grace` because a PM that
    /// ignores EOF must not hold mana open, the second by `DRAIN_GRACE` because
    /// a surviving grandchild still holding the stdout pipe would otherwise
    /// decide when mana may exit. Safe to run twice -- `shutdown` then `Drop`
    /// is the normal path -- since the second pass finds a reaped child and no
    /// reader.
    pub(super) fn close_and_wait(&mut self) {
        let deadline = Instant::now() + self.close_grace;
        while !self.has_exited() && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        self.kill_if_alive();
        if let Some(reader) = self.reader.take() {
            join_or_detach(reader, Instant::now() + DRAIN_GRACE);
        }
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

    /// The PM's pid. Test-only: nothing in mana registers a PM the way it
    /// registers a sub-agent, so the only callers are the process tests that
    /// check what a shutdown killed. Gated `unix` as well as `test` because
    /// those tests exec a shell fixture and so are themselves unix-only --
    /// a plain `cfg(test)` compiles this on Windows with nobody to call it,
    /// which `-D warnings` rightly rejects.
    #[cfg(all(test, unix))]
    pub(super) fn pid(&self) -> u32 {
        self.pid
    }

    /// How long `close_and_wait` stays polite. Test-only for the same reason:
    /// no caller has ever wanted a window other than `CLOSE_GRACE`, and the
    /// tests want one they need not sit five seconds through. Same `unix`
    /// gate as `pid`, and for the same reason.
    #[cfg(all(test, unix))]
    pub(super) fn set_close_grace(&mut self, grace: Duration) {
        self.close_grace = grace;
    }
}
