//! `mana launch <cli>` -- the v2 PM session.
//!
//! Six things happen, in this order, and each one kills a v1 defect -- after
//! one thing that happens before all of them:
//!
//! 0. **Refuse what cannot work.** A terminal to draw in and a git repo to
//!    branch worktrees off are a millisecond to check each, neither has a
//!    degraded mode, and both used to be discovered after the launch had
//!    already paid for itself (`check_preconditions`, #85 and #86).
//! 1. **Resolve the catalogue entry.** Everything CLI-specific below comes
//!    from it, so nothing here branches on which CLI the user named.
//! 2. **Install the PM skill.** `assets/roles/pm/SKILL.md` is embedded in the
//!    binary and rewritten to the CLI's own skills directory on every launch,
//!    so the role text can never drift from the code that serves its tools
//!    (design §6). SKILL.md is the one role format ~40 tools already read;
//!    `--append-system-prompt` exists on one CLI in five.
//! 3. **Wire the tool channel.** For `mcp`, write the config and pass the
//!    catalogue's flag: mana registers itself by `current_exe()`, so `mana`
//!    does not have to be on `$PATH` -- one of v1's three launch blockers. For
//!    `sentinel`, there is nothing to attach and mana instead reads the PM's
//!    own fenced blocks out of the event stream (`crate::sentinel`).
//! 4. **Start the driver and send one activation line.** The skill does the
//!    teaching; the launch message says where that skill is, and adds only
//!    what the CLI cannot find out for itself.
//! 5. **Run the loop.** PM events in, chat pane out; every message the PM
//!    sends passes the tool channel first; `notifications.jsonl` tailed and
//!    each finished dispatch injected as a user turn, which is how the PM
//!    learns an executor finished without polling for it.
//! 6. **Shut down.** Ctrl+C or a dead PM both end the session and reap the
//!    process -- v1 could do neither.
//!
//! Steps 1-4 and the loop's body are `prepare_session` and `Session`, with no
//! terminal anywhere near them: the milestone-2 smoke test drives that exact
//! code against a fake PM script, which is the only way the flow gets tested
//! without paying a real CLI.

use crate::catalog::{Catalog, CliEntry, ToolChannel, substitute};
use crate::cli::kill::kill_dispatch;
use crate::mcp::runs::{Notification, notifications_path};
use crate::pm::{self, PmEvent, PmTransport, Resume};
use crate::project::{
    ProjectPaths, ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::sentinel::{MAX_TOOL_CYCLES, Sentinel, ToolLine};
use crate::session_lock;
use crate::status::{self, DispatchStatus, short};
use crate::tui::app::{App, Source};
use crate::tui::event::{AppEvent, CrosstermEventSource, EventSource, RawEvent, map_key_event};
use crate::tui::graph::GraphCache;
use crate::tui::render;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

/// The PM role text, embedded so it ships and versions with the tools it
/// teaches (design §6).
const PM_SKILL: &str = include_str!("../../assets/roles/pm/SKILL.md");

/// The whole of what mana says at launch. Everything else the PM needs to
/// know is in the skill and in the tool schemas -- a long activation message
/// would be a second copy of both, free to disagree with them.
///
/// It ends with the skill's absolute path, and that is not decoration.
/// Measured on agy (2026-08-15): told only to "load the mana-pm skill", the PM
/// spent its whole first turn hunting for it -- a `grep_search` under the CLI's
/// own config directory (refused by that CLI's protection rules) and a `find ~`
/// (refused by its permission policy) -- and then answered nothing at all.
/// Design §6 has mana write that file; withholding where it wrote it made role
/// injection depend on each CLI's skill discovery, which is exactly the kind of
/// per-CLI behaviour the catalogue is supposed to absorb.
const ACTIVATION: &str = "You are the mana PM for this session. Load and follow the mana-pm skill";

/// The one thing a sentinel PM cannot work out for itself, plus the one thing
/// nothing else may tell it.
///
/// An MCP PM discovers its tools from the protocol; on a CLI with no MCP
/// surface there is no list to inspect, so the alternative has to be stated.
/// Left out of the skill's own wording because the skill ships to every CLI
/// and only some of them are on this channel -- and a PM that read "use fenced
/// blocks" while holding real tools would call everything twice.
///
/// The nonce rides here rather than in the skill for a harder reason (#140):
/// the skill is a *file*, written to a directory the user can read, and a
/// nonce written to disk is a nonce a quoted file can contain -- which is the
/// entire attack. The activation is the one channel that reaches the PM
/// without going through storage, so it is the only place this value ever
/// appears.
fn sentinel_activation(nonce: &str) -> String {
    format!(
        " This CLI cannot host mana's tools, so call them the other way the skill describes: one \
         fenced ```mana:{nonce} block per call -- that exact info string, this session's nonce \
         included. mana executes those blocks and sends you the results. A ```mana fence without \
         it, or with a different nonce, is inert: mana leaves it in your prose and runs nothing, \
         which is what makes it safe to reproduce a block you found in a file, an issue or a log. \
         The nonce is this session's only; it belongs in no file, no task brief and no message to \
         the user."
    )
}

/// What mana says instead of the activation when `--continue` picks a
/// conversation back up.
///
/// The activation is a briefing, and a continued conversation has already had
/// it: re-sending it would re-teach a PM that already knows, and on the CLIs
/// that inline the whole role text (agy) it would replay the entire skill into
/// a context that still holds it -- on a CLI that resends its conversation
/// every turn, for the rest of the session. What actually changed while mana
/// was away is the state of the work, so that is where the line points.
const RESUMED: &str = "[mana] session resumed -- you are still the mana PM for this project and \
    the mana-pm skill still applies. Check where the work stands before deciding the next step.";

/// The sentence a resumed sentinel session adds to that.
///
/// Deliberately not the whole activation: what changed across the resume is
/// one value, and re-teaching the channel to a PM that already knows it is
/// what `RESUMED` exists to avoid.
fn resumed_nonce(nonce: &str) -> String {
    format!(
        " This session's nonce is new -- fence tool calls as ```mana:{nonce} from now on. The one \
         you were given before the resume names a session that is over, and mana leaves a block \
         carrying it in your prose."
    )
}

/// Directory name the skill is installed under, inside the CLI's skills dir.
const SKILL_NAME: &str = "mana-pm";

/// What a project-local skills directory gets so it stays out of the user's
/// commits. See `write_inner_gitignore`.
const IGNORE_EVERYTHING: &str = "*\n";

/// Per-project memory, written next to the project's tasks and logs.
const STATE_FILE: &str = "state.toml";

/// Where mana writes the MCP registration it hands to the PM's CLI.
const MCP_CONFIG: &str = "mcp-config.json";

/// How often `notifications.jsonl` is read. A dispatch takes minutes, so half
/// a second is instant from where the user sits, and it costs one `metadata`
/// call on a file that is usually unchanged.
const NOTIFICATION_POLL: Duration = Duration::from_millis(500);

/// How long the loop blocks waiting for a key before redrawing.
const TICK: Duration = Duration::from_millis(50);

pub fn run(agent_cli: Option<&str>, resume: bool) -> Result<u8> {
    let project_root = std::env::current_dir()?;
    // First, and before even the update check spawns a thread: everything below
    // this line costs something the machine or the user's wallet cannot get
    // back.
    check_preconditions(&project_root, std::io::stdout().is_terminal())?;
    let home = mana_home()?;
    // Started before the session is prepared, so the answer is usually already
    // waiting by the first frame. It is the only command that looks: `ps`,
    // `kill`, `doctor` and `mcp-server` must never reach the network, and the
    // last of those is a protocol server on stdio where a stray request would
    // be a genuine defect (design §5).
    let update_notice = crate::cli::upgrade::spawn_check(&home);
    let paths = resolve_project_paths(&home, &project_name_from_dir(&project_root));
    // Before the PM is chosen and long before it is spawned: a session that is
    // going to be refused must not have installed a skill, rewritten
    // `state.toml` or paid for a turn first. Held for the rest of `run`, which
    // is every way out of it -- see `session_lock`.
    let _session = session_lock::acquire(&paths, Utc::now())?;
    let agent_cli = resolve_cli(&paths, agent_cli, resume)?;
    let mut session = prepare_session(&home, &project_root, &agent_cli, resume)?;
    let mut app = App::new(&session.cli_name);
    app.push(Source::Raw, &launch_line(&session, resume));
    // Anything the launch did to the user's disk beyond writing that one file
    // -- a stale copy of the role deleted out of another skills directory --
    // is said here rather than done quietly.
    for line in std::mem::take(&mut session.startup) {
        app.push(Source::Mana, &line);
    }
    // Degraded, and visible rather than silent -- the same rule the event map
    // follows. Without a turn boundary mana cannot hold anything back, so
    // typing mid-answer behaves as it did before the queue existed, and the
    // operator learns that here instead of from a counter that never moves.
    if !session.tracks_turns {
        app.push(
            Source::Mana,
            &format!(
                "[mana] {} declares no [pm.events].turn_end, so mana cannot tell when a turn \
                 ends: anything typed while the PM is answering goes straight to it.",
                session.cli_name
            ),
        );
    }

    // Trapped before the terminal is touched, so there is no instant in which
    // a `kill` can land with raw mode on and nothing installed to undo it.
    trap_termination();
    let mut terminal = TerminalGuard::enter()?;
    // Caught rather than left to unwind, and `AssertUnwindSafe` says the quiet
    // part: a panicking loop may leave this session half-updated. That is
    // exactly the session whose sub-agents are still running, and the teardown
    // below only reads the two things a panic cannot corrupt -- the PM handle
    // and the project name.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_loop(
            &mut terminal.terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut CrosstermEventSource,
            update_notice,
        )
    }));
    // Restore the terminal before anything is printed or propagated: an error
    // rendered into the alternate screen is an error nobody ever reads. Every
    // other way out of the loop -- a `?` inside it, a panic unwinding through
    // `render::draw` -- reaches the same `Drop`, which is the point of it.
    drop(terminal);
    match outcome {
        Ok(outcome) => finish_session(&home, &mut session, &app, outcome),
        Err(panic) => {
            // The session ends the same way it would have on Ctrl+C: the PM is
            // reaped and the sub-agents swept, because a bug in a widget is
            // still no reason to leave a paid agent running unwatched. Then the
            // panic carries on to the caller unchanged.
            let _ = finish_session(&home, &mut session, &app, Ok(SessionEnd::UserQuit));
            std::panic::resume_unwind(panic)
        }
    }
}

/// Everything that makes a launch impossible, checked in one place before the
/// launch does anything it cannot take back.
///
/// Both are a `git rev-parse` or an `ioctl` away, both are certain, and both
/// used to be discovered by the thing that needed them:
///
/// - the terminal, by `enable_raw_mode` -- after the role had been written into
///   the user's skills directory (and a stale copy deleted from another),
///   `state.toml` rewritten, the PM CLI spawned and the whole activation turn
///   sent to it, which on `[skills].inline_in_activation` is the entire role
///   text billed to the user (#85);
/// - the repo, by the first dispatch's `worktree::ensure_git_repo` -- on a
///   background thread, after the PM had planned and dispatched every task, with
///   one paid PM turn coming back per executor to say the same thing (#86);
/// - a repo with no commits to branch a worktree from, by `create`'s own
///   check -- same story, same one paid PM turn per executor (#192).
///
/// Refusing the launch outright on a non-git project is stronger than the
/// dispatch gate it hoists: a read-only role would have run. That is the point.
/// The session exists to dispatch work, isolation *is* the worktree (spec §9),
/// and a PM told to plan for a project it can never write is worth less than the
/// five milliseconds it takes to say so.
///
/// `interactive` is a parameter rather than a read, because whether this process
/// has a terminal is the one thing a test cannot arrange. stdout is what is
/// asked about: the alternate screen and every frame go there, so a launch whose
/// stdout is a pipe has nowhere to draw whatever `/dev/tty` may still offer.
fn check_preconditions(project_root: &Path, interactive: bool) -> Result<()> {
    if !interactive {
        bail!(
            "mana launch is a full-screen terminal UI and stdout here is not a terminal (a pipe, \
             a CI job, a supervisor). Run it from a terminal -- there is no headless mode, and \
             nothing has been started."
        );
    }
    crate::worktree::ensure_git_repo(project_root)?;
    crate::worktree::ensure_has_commits(project_root)
}

/// Everything a session owes the machine once the loop is over, whichever way
/// it went out.
///
/// The outcome is carried in by value rather than propagated at the call site,
/// and that is the whole shape of the fix: reaping the PM and sweeping the
/// sub-agents are what mana owes processes it started, so nothing that can
/// fail may sit above them. The `?` chain that used to restore the terminal
/// here did exactly that -- one failed step and the outcome was dropped
/// unread, the PM left running and the sub-agents left to burn quota (#75).
fn finish_session(
    home: &Path,
    session: &mut Session,
    app: &App,
    outcome: Result<SessionEnd>,
) -> Result<u8> {
    let _ = session.shutdown();
    // Quitting with turns still waiting is the operator's own decision, but it
    // is one they may have forgotten they made: the queue lives in a status
    // bar that has just disappeared. Recorded rather than printed here, so the
    // loop below says it after the alternate screen is gone *and* the exit
    // code below counts it (#181).
    session.flush_queue("you ended the session first");
    // Said again, out here: during the session these were lines in a chat pane
    // that no longer exists, so a session that lost every notification read on
    // the way out exactly like one that lost nothing (#96).
    for line in &session.lost {
        println!("{line}");
    }
    // Both ways out of the loop lead here, and both mean the same thing for a
    // sub-agent: the process that dispatched it is leaving. Nothing else would
    // ever stop it -- its observer thread died with mana, so it would run to
    // completion writing into logs nobody reads, holding a quota slot and a
    // worktree, and `mana ps` would call it running until someone noticed.
    let sweep = sweep_in_flight(home, &session.project, Utc::now());
    for line in &sweep.lines {
        println!("{line}");
    }
    match outcome? {
        // The contract a wrapper script can actually use: zero means the
        // session did everything it was asked to, non-zero means it did not and
        // the reason is above. A session that told the PM nothing, or that left
        // paid sub-agents running, is not a clean end and used to be
        // indistinguishable from one -- every line was a `println!` and `q`
        // still exited 0.
        //
        // Only on this arm because a PM that ended the session explains those
        // losses better than a count of them does, and that arm already fails.
        // #95 is about which code it should fail with; if it ever grows a
        // success case, this guard has to cover that too.
        SessionEnd::UserQuit if session.lost.is_empty() && sweep.clean => Ok(0),
        SessionEnd::UserQuit => bail!(
            "this session ended with {} message(s) that never reached the PM{} -- see above",
            session.lost.len(),
            if sweep.clean {
                ""
            } else {
                ", and sub-agents mana could not stop"
            }
        ),
        // A PM that died on its own took the session with it, and the only
        // explanation there is arrived on its stderr -- which is now behind a
        // screen the user cannot get back to.
        // A PM that ended by itself is not automatically a failure, and mana
        // used to make it one: every exit here bailed, so a PM that finished
        // its work and returned 0 still gave the shell exit 1, and a PM that
        // returned 7 also gave 1 (#95). mana is a wrapper here -- the child's
        // code is the answer to "did the work succeed", and mana's own job is
        // only to add what it knows: whether *it* lost anything (#96).
        SessionEnd::PmExited { code: Some(0) } if session.lost.is_empty() && sweep.clean => Ok(0),
        SessionEnd::PmExited { code: Some(0) } => bail!(
            "the PM ({}) ended cleanly, but this session lost {} message(s) that never \
             reached it{} -- see above",
            session.cli_name,
            session.lost.len(),
            if sweep.clean {
                ""
            } else {
                ", and left sub-agents mana could not stop"
            }
        ),
        SessionEnd::PmExited { code } => {
            // The explanation is printed rather than returned, because for a
            // non-zero child the *code* is what a wrapper script reads and
            // anyhow's own exit code (1) would overwrite it.
            let reason = death_reason(app);
            match code {
                Some(code) => {
                    eprintln!(
                        "the PM ({}) ended the session with exit code {code}{reason}",
                        session.cli_name
                    );
                    // Codes above 255 cannot survive a process exit status
                    // anyway; saturating keeps a non-zero child non-zero
                    // instead of wrapping some of them to 0.
                    Ok(u8::try_from(code).unwrap_or(1).max(1))
                }
                // A signal has no code to propagate, and 1 is the honest
                // stand-in: something went wrong and mana cannot say what.
                None => bail!(
                    "the PM ({}) was killed by a signal{reason}",
                    session.cli_name
                ),
            }
        }
    }
}

/// What a PM that died said last, for the one line the operator is left with
/// once the alternate screen is gone.
///
/// stderr first, and stdout only when there was none: a dying CLI explains
/// itself on stderr, while stdout carries the routine frames of every turn.
/// Taking whichever pipe spoke last handed the operator an `init` frame's cwd
/// and tool list instead of the error (#189).
fn death_reason(app: &App) -> String {
    app.last_stderr
        .as_deref()
        .or(app.last_raw.as_deref())
        .map(|line| format!("\nits last output was: {line}"))
        .unwrap_or_default()
}

/// The terminal state mana changes, owned by one value so that the restore is
/// something the compiler runs rather than something `run` remembers to.
///
/// Raw mode and the alternate screen are the two things a crashed TUI leaves
/// behind, and they are recoverable only by typing `reset` blind. `Drop`
/// covers what a restore written after the loop cannot: a panic in
/// `render::draw` or in an event handler, and every `?` between here and the
/// end of `run` (#75). The panic hook `enter` installs covers the one thing
/// `Drop` is too late for -- the message.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enters raw mode and the alternate screen, all or nothing.
    ///
    /// The order is the whole point: the only fallible step that changes
    /// nothing -- building the terminal, which queries the size -- runs first,
    /// so the guard exists before the first byte of state is touched and a
    /// failure below unwinds through the same `Drop` a finished session does.
    /// Entering raw mode and then failing to reach the alternate screen used
    /// to return `Err` from a shell left in raw mode with nothing to undo it.
    fn enter() -> Result<TerminalGuard> {
        let mut guard = TerminalGuard {
            terminal: Terminal::new(CrosstermBackend::new(std::io::stdout()))?,
        };
        enable_raw_mode()?;
        write_enter_sequence(guard.terminal.backend_mut())?;
        // Last, and only now that there is something to undo: a panic before
        // this line has nothing to restore and deserves the plain hook.
        //
        // `Drop` cannot cover the *message*, only the screen. The default hook
        // prints at the panic site, which is before any unwinding starts --
        // into the alternate screen, which then vanishes with the text on it.
        // So the restore has to happen ahead of the printer, and the only way
        // in front of it is to wrap it (#75). Chained rather than replaced:
        // the message, the location and the backtrace note are the default
        // hook's to format, and mana has no business rewriting them.
        let printer = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            restore_terminal();
            printer(panic);
        }));
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    /// The backstop for every route a panic is not: a normal return, a `?`, an
    /// early failure inside `enter` itself. Running after the hook has already
    /// restored costs nothing -- see `restore_terminal`.
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// The escape sequences `TerminalGuard::enter` sends, factored out of it so a
/// test can assert on the bytes: `execute!` needs a `Write`, and `Stdout` is
/// not one a test can inspect, but any `Vec<u8>` is.
///
/// Bracketed paste is enabled here, alongside the alternate screen, rather
/// than as its own fallible step -- a terminal that does not understand the
/// sequence just ignores it, the same as it would ignore any other escape
/// code it does not support, so there is nothing here for it to fail (#160).
fn write_enter_sequence(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    execute!(writer, EnterAlternateScreen, EnableBracketedPaste)
}

/// The reverse of `write_enter_sequence`, in reverse order: bracketed paste
/// was the last thing enabled, so it is the first thing disabled. Leaving it
/// on would hand every future paste in that shell to the terminal as literal
/// `\x1b[200~`/`\x1b[201~` bytes instead of the app that asked for it (#160).
fn write_restore_sequence(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    execute!(
        writer,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )
}

/// The steps that give the user their shell back, in the order they need
/// them, on the process's own stdout rather than through the guard -- the
/// panic hook cannot borrow a value the unwinding stack still owns.
///
/// Best-effort and silent: never `?`, never `panic!`, since this runs while a
/// panic may already be unwinding and a second one aborts the process outright.
/// Every step is idempotent, which is what lets the hook and `Drop` both run.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = write_restore_sequence(&mut std::io::stdout());
}

/// Set by the termination handler, read by the run loop.
///
/// The flag is the entire handler, and that is a hard constraint rather than a
/// simplification: a signal handler may only call async-signal-safe functions,
/// which rules out allocating, locking, and every line of the teardown mana
/// actually wants to run. So the handler records that a signal arrived and the
/// loop does the work on its normal path, by the door Ctrl+C already uses
/// (#34, #84).
static TERMINATED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_terminate(_signal: libc::c_int) {
    TERMINATED.store(true, Ordering::Relaxed);
}

/// Traps the four signals that end a session mana did not ask to end: `kill`,
/// a closed terminal emulator, an ssh drop, a logout. Untrapped, every one of
/// them killed mana with the default disposition -- terminal left in raw mode
/// on the alternate screen, and the in-flight sub-agents left running with
/// nothing watching them and no exit record ever written.
///
/// `signal` rather than `sigaction` because there is nothing to configure: one
/// flag, read once, and the loop is on its way out. A signal landing mid-tick
/// costs nothing either -- crossterm reports an interrupted wait as "no key",
/// so that tick just ends early and the next one reads the flag.
///
/// SIGINT and SIGQUIT are trapped too. The reason they once were not held only
/// for the keyboard: raw mode clears `ISIG`, so crossterm delivers Ctrl+C as a
/// key event and no keystroke here ever becomes a signal. `kill -INT` from
/// another window is not a keystroke, and it is the reflex way to stop a
/// foreground process -- it was landing on the default disposition and leaving
/// a shell that needed `reset` (#180).
#[cfg(unix)]
fn trap_termination() {
    // SAFETY: `on_terminate` is async-signal-safe -- one relaxed store to a
    // static, no allocation, no I/O, no locks.
    unsafe {
        let handler = on_terminate as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGQUIT, handler);
    }
}

/// Windows has neither signal. Its console-close notification arrives on a
/// thread of the OS's own, with a deadline mana would have to race, and the
/// `Drop` guard already covers every exit route this process controls.
#[cfg(not(unix))]
fn trap_termination() {}

/// How loudly one line of sentinel tool activity is said.
///
/// A call that worked is annotation, and it collapses with the rest of the
/// machinery -- the same class as the `⚙ …` lines an ACP agent's tool calls
/// become, because it is the same event seen through a different channel. A
/// call that failed is mana telling the operator something they have to know:
/// a block it refused, a tool that errored, results that never reached the PM.
fn tool_line_source(line: &ToolLine) -> Source {
    if line.failed {
        Source::Mana
    } else {
        Source::Raw
    }
}

/// Everything a launch is worth in the chat pane: one dim line.
///
/// It is `Source::Raw` rather than a notice, and that is the v2.1 rule made
/// concrete. A launch sends the PM a briefing that can run to the whole role
/// text, writes a file under the user's home and registers an MCP server --
/// and none of it is what the operator opened the pane to read. The line says
/// the session exists and where the role went; the raw view (Ctrl+O) has it in
/// full, and the counter says it is there. Anything a launch had to do
/// *differently* -- a skills directory it could not write, a stale copy it
/// removed -- is a notice instead, because that is news.
fn launch_line(session: &Session, resume: bool) -> String {
    format!(
        "session {} on {} · mana-pm role at {}",
        if resume { "resumed" } else { "initialized" },
        session.cli_name,
        session.skill_path.display()
    )
}

/// Why the loop returned.
#[derive(Debug, PartialEq)]
enum SessionEnd {
    UserQuit,
    PmExited { code: Option<i32> },
}

/// A live PM session, with no terminal attached.
///
/// This is the half of `run` that has decisions in it, kept separable so the
/// milestone-2 smoke test can drive a real launch -- skill, MCP config,
/// activation turn, notification injection, shutdown -- against a fake PM
/// script instead of a paid CLI.
struct Session {
    pm: Box<dyn PmTransport>,
    paths: ProjectPaths,
    /// The `~/.mana/projects` name this session belongs to. Carried so the
    /// teardown sweep can find this project's dispatches -- and only this
    /// project's.
    project: String,
    /// Product name of the CLI driving the PM, for the status bar.
    cli_name: String,
    /// Where the skill was written this launch. Reported to the user, because
    /// "mana rewrote a file in your config directory" should not be silent.
    skill_path: PathBuf,
    /// Lines the launch produced before the first frame, drained into the chat
    /// pane by `run`. Empty on an ordinary launch.
    startup: Vec<String>,
    notifications: NotificationTail,
    /// `Some` only for `[tools].channel = "sentinel"`. An MCP CLI reaches the
    /// same four tools over its own protocol, and scanning its prose for
    /// fenced blocks would be a second way in that nothing asked for.
    sentinel: Option<Sentinel>,
    /// Everything that would have been sent while the PM was mid-answer, in
    /// the order it arrived.
    ///
    /// One queue for both kinds of turn, and that is the point: a typed
    /// question and an injected `[mana]` notification are the same thing to
    /// the PM -- a user turn -- so interleaving them by arrival is the only
    /// order that reads as a conversation. Two queues would let a dispatch
    /// notice overtake a question the operator asked first.
    queue: VecDeque<Queued>,
    /// Whether the PM is mid-turn. Opened by every send, closed by
    /// `PmEvent::TurnEnded` (or by the PM dying).
    turn_open: bool,
    /// Whether the transport can close a turn at all. False leaves `turn_open`
    /// permanently shut, which is v2.1 behaviour: every turn goes out at once.
    /// Saying so is `run`'s job -- silently not queueing would look like a
    /// queue that never fills.
    tracks_turns: bool,
    /// Every line mana wrote about something that never reached the PM.
    ///
    /// The chat pane was the only place these ever went, and the pane dies with
    /// the alternate screen: a session where nothing at all got through --
    /// every notification, every typed turn, every permission answer -- ended
    /// on `q` looking exactly like a clean one, exit code included (#96). Kept
    /// so `finish_session` can say them again where they survive, and count
    /// them into the exit status.
    lost: Vec<String>,
    /// How many sentinel tool cycles have turned in a row with nothing from
    /// outside the loop. Bounded by `MAX_TOOL_CYCLES` -- the rule is stated
    /// where the constant is defined (`sentinel::MAX_TOOL_CYCLES`, #191).
    ///
    /// It lives on the session because the loop it bounds is the session's:
    /// this is the one piece of state that says whether the PM is talking to
    /// mana or to itself. Reset by `send_typed` (the operator said something),
    /// by `poll_notifications` (a dispatch reported back) and by any PM
    /// message that carried no block.
    tool_cycles: u32,
}

/// One turn waiting its place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    text: String,
    /// Whether a human typed it. Only these have a line already on screen
    /// waiting to lose its pending mark, and only these are worth naming back
    /// to the user if the PM dies before they are sent.
    typed: bool,
}

/// What became of something the session was asked to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// Written to the transport now.
    Sent,
    /// Held: the PM is mid-turn. It goes out on `TurnEnded`, in order.
    Queued,
}

impl Session {
    /// Records something that never reached the PM, and hands the line back for
    /// the pane. One call rather than a push next to every `app.push`, so the
    /// words the operator reads during the session and the ones the exit
    /// reports afterwards cannot drift apart.
    fn record_loss(&mut self, message: String) -> String {
        self.lost.push(message.clone());
        message
    }

    /// Everything the PM has said since the last call.
    fn drain(&mut self) -> Vec<PmEvent> {
        std::iter::from_fn(|| self.pm.events().try_recv().ok()).collect()
    }

    /// The one turn whose words the chat pane shows: a human typed it. The
    /// echo itself is `apply_app_event`'s, because that is the only place that
    /// knows somebody pressed Enter.
    fn send_typed(&mut self, text: &str) -> Result<Delivery> {
        // The operator is the outside of the loop: whatever the PM was doing
        // to itself, it is now answering a human, and the cycle count starts
        // over (#191).
        self.tool_cycles = 0;
        self.deliver(text, true)
    }

    /// A turn mana wrote itself -- the activation (and, on the CLIs that cannot
    /// read a file, the whole role text it carries), the results of the PM's
    /// own tool calls, the notice that a dispatch finished.
    ///
    /// This is plumbing between mana and its PM, not conversation, and it is a
    /// method of its own rather than a comment on `send_typed` because the
    /// distinction is exactly what v2.0 lost: every one of these went out
    /// through the same call as a typed turn, and the interface had no way to
    /// tell them apart. The chat pane may say *that* mana spoke to the PM --
    /// one short line, or the one-line notice a finished dispatch deserves --
    /// and never *what* was said. Nothing here returns text to render.
    fn send_internal(&mut self, text: &str) -> Result<Delivery> {
        self.deliver(text, false)
    }

    /// The single door every turn leaves by: send it, or hold it until the PM
    /// is free.
    ///
    /// A PM is a conversation, and a conversation has turns. Writing into one
    /// that is already open is what v2.1 did -- the frame landed in the middle
    /// of an answer, and what a CLI does with that is anybody's guess. So mana
    /// keeps the fact it now has (`PmEvent::TurnEnded`) and waits.
    ///
    /// The `oneshot-continue` driver already serialises turns inside itself,
    /// and it is routed through here anyway: its internal queue is invisible,
    /// so a session on that driver would show `0 queued` while holding three
    /// messages. One queue mana can see beats two it cannot.
    fn deliver(&mut self, text: &str, typed: bool) -> Result<Delivery> {
        if self.turn_open {
            self.queue.push_back(Queued {
                text: text.to_string(),
                typed,
            });
            return Ok(Delivery::Queued);
        }
        self.pm.send_user(text)?;
        // Only after the write succeeded: a turn that never reached the PM did
        // not open one, and pretending otherwise would wedge the queue behind
        // an answer nobody is coming to give.
        self.turn_open = self.tracks_turns;
        Ok(Delivery::Sent)
    }

    /// Closes the open turn and sends the next thing waiting, if any.
    ///
    /// Returns what went out, so the caller can clear its pending mark, and
    /// how it went -- a delivery failure here is almost always a PM that died
    /// between the end of its turn and this line.
    fn release_next(&mut self) -> Option<(Queued, Result<()>)> {
        self.turn_open = false;
        let queued = self.queue.pop_front()?;
        let sent = self.pm.send_user(&queued.text);
        if sent.is_ok() {
            self.turn_open = self.tracks_turns;
        }
        Some((queued, sent))
    }

    fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Empties the queue, for a session that is over, and hands back one line
    /// per message it will now never send.
    ///
    /// Through `record_loss` rather than into a caller's own `println!`: a
    /// message the PM never received is precisely what `lost` is for, and the
    /// queue was the one channel that went around it -- so the notice died
    /// with the alternate screen on the path that only wrote it to the chat
    /// pane, and the exit guards, which count `lost`, called both paths clean
    /// (#181).
    fn flush_queue(&mut self, reason: &str) -> Vec<String> {
        self.turn_open = false;
        // Drained first: `record_loss` borrows all of `self`, and the queue is
        // part of it.
        let waiting: Vec<Queued> = self.queue.drain(..).collect();
        waiting
            .into_iter()
            .map(|queued| {
                self.record_loss(format!(
                    "[mana] never sent -- {reason}: {}",
                    first_line(&queued.text)
                ))
            })
            .collect()
    }

    fn answer_permission(&mut self, id: u64, option_id: &str) -> Result<()> {
        self.pm.answer_permission(id, option_id)
    }

    /// Injects one user turn per dispatch that finished since the last poll,
    /// and returns the one-line notice each of them is worth in the pane.
    ///
    /// This is the whole reason the PM does not have to poll: it asked for a
    /// dispatch minutes ago, the thread that ran it wrote a line to
    /// `notifications.jsonl` when it ended, and mana turns that line into a
    /// turn the PM reads like any other message.
    ///
    /// The one internal send whose text the pane does show, because here the
    /// text *is* the news: an executor finished, and that is a fact the
    /// operator is waiting for rather than plumbing they have to scroll past.
    fn poll_notifications(&mut self, now: Instant) -> Result<Vec<String>> {
        let mut sent = Vec::new();
        for event in self.notifications.poll(now) {
            // A gap goes out on the same channel as a completion, and that is
            // the point: "a dispatch finished and mana cannot tell you which"
            // is a fact the PM has to plan around exactly like the completion
            // it replaces. Both are one line the operator sees too.
            let message = match event {
                TailEvent::Finished(notification) => notification_message(&notification),
                TailEvent::Gap(message) => message,
            };
            // A dispatch reporting back is news from outside the loop too:
            // the PM has a real reason to call tools again, so the cycle
            // count starts over (#191).
            self.tool_cycles = 0;
            self.send_internal(&message)?;
            sent.push(message);
        }
        Ok(sent)
    }

    /// The tool channel's half of one PM message.
    ///
    /// Nothing at all on an MCP CLI, where tool calls never travel through
    /// prose. On a sentinel CLI: execute every fenced block the message
    /// carried and inject the results as the next user turn, which is how the
    /// PM learns what its own call returned.
    fn apply_tools(&mut self, text: &str) -> ToolPass {
        // The borrow of `sentinel` ends with this match, which is what leaves
        // `self.pm` free to take the reply turn below.
        let outcome = match &self.sentinel {
            None => return ToolPass::default(),
            // Past the bound the message is still scanned -- the operator gets
            // the PM's words with the machinery taken out, as always -- but
            // nothing in it runs (#191).
            Some(sentinel) if self.tool_cycles >= MAX_TOOL_CYCLES => sentinel.decline(text),
            Some(sentinel) => sentinel.handle(text),
        };
        let mut log = outcome.log;
        let mut reply = outcome.reply;
        if reply.is_none() {
            // The PM answered without asking for anything: the loop stopped
            // turning by itself, so the count starts over.
            self.tool_cycles = 0;
        } else {
            self.tool_cycles += 1;
            // Told once, not once per turn. `MAX_TOOL_CYCLES + 1` is the cycle
            // the bound fired on and the one that carries the explanation;
            // past it mana says nothing and injects nothing, and a cycle with
            // no injected turn has nothing left to turn it.
            if self.tool_cycles > MAX_TOOL_CYCLES + 1 {
                reply = None;
                log.clear();
            }
        }
        // The results themselves never render: they are a tool's answer to the
        // PM, often a page of JSON, and the operator gets `outcome.log`'s one
        // compact line per call instead.
        // Queued like anything else when the PM is still mid-turn: on this
        // channel the results *are* the next user turn, and a CLI that reads
        // one while it is still answering is exactly the race the queue exists
        // to remove. It leaves on `TurnEnded`, which on the one CLI that uses
        // this channel is the very next event.
        if let Some(reply) = reply
            && let Err(error) = self.send_internal(&reply)
        {
            // Almost always a PM that just died. Reported where the operator
            // is looking rather than propagated: the `Exited` event ends the
            // session a tick later with a better message than this one.
            let text = self.record_loss(format!(
                "[mana] the tool results never reached the PM: {error:#}"
            ));
            log.push(ToolLine { text, failed: true });
        }
        ToolPass {
            prose: Some(outcome.prose),
            log,
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        self.pm.shutdown()
    }

    /// Waits for the PM to go idle, doing what `run_loop` does with the events
    /// it finds on the way: `TurnEnded` releases the queue, `Exited` empties
    /// it. Returns the loss lines for whatever was still waiting when the PM
    /// died.
    ///
    /// For the smoke tests, which drive a real session without a terminal and
    /// would otherwise race the shell script standing in for a CLI. The
    /// activation is a turn like any other, so nothing a test sends afterwards
    /// leaves until that turn is over -- which is the behaviour under test as
    /// much as it is a precondition for it.
    #[cfg(all(test, unix))]
    fn settle(&mut self) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut never_sent = Vec::new();
        while self.turn_open && Instant::now() < deadline {
            for event in self.drain() {
                match event {
                    PmEvent::TurnEnded => {
                        self.release_next();
                    }
                    PmEvent::Exited { .. } => {
                        never_sent.extend(self.flush_queue("the PM is gone"));
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!self.turn_open, "the PM never finished its turn");
        never_sent
    }
}

/// What the tool channel made of one PM message.
#[derive(Default)]
struct ToolPass {
    /// What the chat pane should show instead of the raw message: the PM's
    /// words with the machinery taken out. `None` when this CLI has no
    /// sentinel channel and the message is the PM's words already.
    prose: Option<String>,
    /// One compact line per tool call: routine activity to collapse with the
    /// rest of the machinery, and anything that failed to say out loud.
    log: Vec<ToolLine>,
}

/// Resolves the CLI, installs the skill, wires the tool channel and starts the
/// session -- everything up to the first frame.
fn prepare_session(
    home: &Path,
    project_root: &Path,
    agent_cli: &str,
    resume: bool,
) -> Result<Session> {
    let catalog = Catalog::load(Some(&home.join(crate::catalog::CATALOG_OVERRIDE)))?;
    let entry = catalog.get(agent_cli).with_context(|| {
        format!(
            "unknown CLI id '{agent_cli}' -- the catalogue knows: {}",
            catalog.ids().join(", ")
        )
    })?;

    let project = project_name_from_dir(project_root);
    let paths = resolve_project_paths(home, &project);
    ensure_project_structure(&paths)?;

    // Installed on a resumed launch too: the file is generated output, this
    // binary may be newer than the one that wrote it, and a PM that reloads
    // its skill mid-session must find what this build serves.
    let skill = install_pm_skill(entry, dirs::home_dir().as_deref(), project_root)?;
    let extra_args = tool_channel_args(entry, &paths, project_root)?;
    // Built before the CLI starts, so a PM that emits a block in its very
    // first answer finds mana already listening.
    let sentinel = match entry.tools.channel {
        ToolChannel::Mcp => None,
        ToolChannel::Sentinel => Some(Sentinel::new(project_root, home, catalog.clone())),
    };

    let mut state = load_state(&paths);
    // Cloned out of the state before it is written back to: the resume borrows
    // it, and the id mana stores after the handshake is the same string either
    // way.
    let stored = state.sessions.get(agent_cli).cloned();
    let pm = pm::start(
        entry,
        &extra_args,
        resume.then_some(Resume {
            session_id: stored.as_deref(),
        }),
    )?;

    // Written now rather than at the end of the session: this is the moment
    // both facts are true and known, and a mana killed outright (or a machine
    // that lost power) would otherwise leave `-c` with nothing to go on. The
    // session id cannot change afterwards -- a resumed ACP session keeps the
    // one it was loaded by.
    state.last_cli = Some(agent_cli.to_string());
    if let Some(session_id) = pm.session_id() {
        state
            .sessions
            .insert(agent_cli.to_string(), session_id.to_string());
    }
    save_state(&paths, &state)?;

    let opening = match (resume, sentinel.as_ref().map(Sentinel::nonce)) {
        (false, nonce) => activation(entry, &skill.path, nonce),
        // The one thing a resumed conversation does *not* still know. The
        // nonce belongs to a `Sentinel`, this launch built a new one, and the
        // PM's context holds the token of a session that no longer exists --
        // so without this line every block it writes would be left as prose
        // and the resumed session could not call a tool at all.
        (true, Some(nonce)) => format!("{RESUMED}{}", resumed_nonce(nonce)),
        (true, None) => RESUMED.to_string(),
    };

    let tracks_turns = pm.tracks_turn_end();
    let mut startup = skill.notes;
    startup.extend(degradation_notice(entry));
    let mut session = Session {
        pm,
        cli_name: entry.cli.name.clone(),
        skill_path: skill.path,
        startup,
        notifications: NotificationTail::new(notifications_path(&paths)),
        sentinel,
        project,
        paths,
        queue: VecDeque::new(),
        turn_open: false,
        tracks_turns,
        lost: Vec::new(),
        tool_cycles: 0,
    };
    // Through `send_internal` rather than straight down the transport, so the
    // rule holds where it matters most: this message is a briefing mana wrote,
    // it can be the entire role text (`[skills].inline_in_activation`), and
    // v2.0 put all of it in the pane as though the user had typed it.
    session
        .send_internal(&opening)
        .context("sending the opening message to the PM")?;
    Ok(session)
}

/// What `mana doctor` already knows about this CLI, said once at launch.
///
/// Same list, computed by the same function: `doctor` is where an operator
/// looks *before* choosing a CLI, and this is where they find out after
/// choosing one -- but only ever from `doctor::degradations`, so the two can
/// never drift. Empty entry, empty block: a CLI that declares no degradation
/// says nothing at all here.
fn degradation_notice(entry: &CliEntry) -> Vec<String> {
    let degradations = crate::cli::doctor::degradations(entry);
    if degradations.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "[mana] {} runs degraded here (`mana doctor` has the detail):",
        entry.cli.name
    )];
    lines.extend(degradations.iter().map(|line| format!("[mana]   - {line}")));
    lines
}

/// Which CLI this launch is for.
///
/// `mana launch -c` with no CLI is the whole reason `last_cli` is stored: the
/// user resumed *this project*, and naming the CLI again is a detail mana
/// already knows. A project mana has never launched in has nothing to read, and
/// says so rather than guessing at the catalogue's first entry.
fn resolve_cli(paths: &ProjectPaths, agent_cli: Option<&str>, resume: bool) -> Result<String> {
    if let Some(id) = agent_cli {
        return Ok(id.to_string());
    }
    if !resume {
        bail!(
            "mana launch needs a CLI to run as PM (for example `mana launch claude`), or \
             --continue to resume the last one used in this project"
        );
    }
    load_state(paths).last_cli.ok_or_else(|| {
        anyhow::anyhow!(
            "mana has no record of a PM session in this project, so `--continue` has nothing to \
             resume. Name the CLI once (`mana launch claude --continue`) and mana will remember \
             it for next time."
        )
    })
}

/// What mana remembers about a project between launches.
///
/// Per-project cache, written by mana and read by mana: losing it costs one
/// extra word on a command line. Anything unreadable is therefore treated as
/// absent rather than as an error -- a launch must not fail over a cache. That
/// rule is why mana keeps no global registry of resolved CLIs at all: a cache
/// nobody may fail on is a cache nobody may trust, so resolution happens live
/// (`CliMeta::resolve`) and only genuinely per-project state is stored.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectState {
    /// The CLI the last session that started successfully was run on.
    last_cli: Option<String>,
    /// Session ids to resume by, keyed by catalogue id, for the transports
    /// whose protocol hands one out (ACP). Per CLI because the last opencode
    /// session is not something copilot could ever load.
    #[serde(default)]
    sessions: BTreeMap<String, String>,
}

fn state_path(paths: &ProjectPaths) -> PathBuf {
    paths.root.join(STATE_FILE)
}

fn load_state(paths: &ProjectPaths) -> ProjectState {
    std::fs::read_to_string(state_path(paths))
        .ok()
        .and_then(|source| toml::from_str(&source).ok())
        .unwrap_or_default()
}

fn save_state(paths: &ProjectPaths, state: &ProjectState) -> Result<()> {
    let path = state_path(paths);
    crate::project::create_dir_all(&paths.root)?;
    let document = toml::to_string_pretty(state).context("rendering this project's state")?;
    crate::project::write(&path, document).with_context(|| format!("writing {}", path.display()))
}

/// The one turn mana writes itself: who the PM is, where its role text is, and
/// -- where the CLI cannot host mana's tools -- how to call them anyway.
///
/// `nonce` is `Some` exactly when this session has a sentinel channel, because
/// it comes from that channel's own `Sentinel`. Taken as a parameter rather
/// than re-derived from `entry.tools.channel`: the value has to be the one the
/// scanner will actually compare against, and a second source for it is a
/// session where mana teaches one fence and honours another.
fn activation(entry: &CliEntry, skill_path: &Path, nonce: Option<&str>) -> String {
    let mut message = format!("{ACTIVATION}, installed at {}.", skill_path.display());
    if let Some(nonce) = nonce {
        message.push_str(&sentinel_activation(nonce));
    }
    // The last resort, for a CLI that can neither discover the file nor be
    // allowed to read it (`[skills].inline_in_activation`, agy). Appended
    // rather than replacing the path so the two never disagree about which
    // text is the role.
    if entry.skills.inline_in_activation {
        message.push_str("\n\nThat file is not readable from this CLI, so here it is in full:\n\n");
        message.push_str(PM_SKILL);
    }
    message
}

/// Where the PM role ended up this launch, and what installing it disturbed.
#[derive(Debug)]
struct SkillInstall {
    path: PathBuf,
    /// One line per stale copy removed from another directory in the entry's
    /// list. Shown in the chat pane: deleting a directory under someone's home
    /// is not something to do silently.
    notes: Vec<String>,
}

/// Writes the PM skill where this CLI will read it, and takes mana's own
/// earlier copies out of the other directories the entry lists.
///
/// Rewritten on every launch on purpose: the file is generated output, and a
/// user who edited it (or an older mana that wrote an older version) would
/// otherwise leave the PM following instructions that no longer match the
/// tools it is served.
///
/// The cleanup is the other half of the same idea. `mana-pm` in a *global*
/// skills directory is a skill every agent in every project sees for ever --
/// mana pollutes the one list the user actually curates, for a role that only
/// means anything inside a mana session. So the entry's first directory is
/// project-local where the CLI supports it, and every other directory in the
/// list has its `mana-pm/` removed. Only that one name is touched, and mana is
/// the only thing that ever writes it.
fn install_pm_skill(
    entry: &CliEntry,
    home: Option<&Path>,
    project_root: &Path,
) -> Result<SkillInstall> {
    let candidates: Vec<PathBuf> = entry
        .skills
        .dirs
        .iter()
        .map(|dir| resolve_skill_dir(dir, home, project_root))
        .collect();
    if candidates.is_empty() {
        bail!(
            "{}: [skills].dirs is empty, so mana has nowhere to install the PM role",
            entry.cli.id
        );
    }
    // Most specific first. A directory that already exists wins -- a user who
    // has `~/.claude/skills` gets the skill where that CLI looks first, and the
    // vendor-neutral `~/.agents/skills` is the fallback listed after it -- and
    // a *project-local* entry wins whether it exists or not, because mana
    // creates it in the project it was launched in and that is the entire
    // reason the catalogue lists it first.
    let first = entry
        .skills
        .dirs
        .iter()
        .zip(&candidates)
        .position(|(spec, path)| is_project_local(spec) || path.is_dir())
        .unwrap_or(0);

    let mut notes = Vec::new();
    // "The first *usable* one": a project-local directory is chosen before mana
    // knows the project is writable at all (a read-only checkout, a directory
    // owned by somebody else), and refusing to launch over that would be mana
    // breaking on a project it could perfectly well have run in. So a failure
    // to write falls through to the next directory in the entry's list, out
    // loud, and only a list with no usable directory at all fails the launch.
    let mut installed = None;
    for (index, dir) in candidates.iter().enumerate().skip(first) {
        match write_skill(dir, is_project_local(&entry.skills.dirs[index])) {
            Ok(path) => {
                installed = Some((index, path));
                break;
            }
            Err(error) => notes.push(format!(
                "[mana] could not install the PM skill in {}, trying the next directory: {error:#}",
                dir.display()
            )),
        }
    }
    let (chosen, path) = installed.ok_or_else(|| {
        anyhow::anyhow!(
            "{}: none of [skills].dirs could be written, so the PM has no role text:\n{}",
            entry.cli.id,
            notes.join("\n")
        )
    })?;
    // A first choice that worked leaves nothing to report; the notes only
    // matter when mana had to go somewhere else than it said it would.
    if chosen == first {
        notes.clear();
    }

    let dir = &candidates[chosen];
    for (index, other) in candidates.iter().enumerate() {
        // The same path twice in one list (a project that *is* the home
        // directory) would otherwise delete what was just written.
        if index == chosen || other == dir {
            continue;
        }
        let stale = other.join(SKILL_NAME);
        if !stale.is_dir() {
            continue;
        }
        notes.push(match std::fs::remove_dir_all(&stale) {
            Ok(()) => format!(
                "[mana] removed the PM skill an earlier launch left in {}",
                stale.display()
            ),
            Err(error) => format!(
                "[mana] could not remove the stale PM skill at {}: {error}",
                stale.display()
            ),
        });
    }
    Ok(SkillInstall { path, notes })
}

/// Writes the role into one skills directory, creating it, and returns the
/// file's path.
fn write_skill(dir: &Path, project_local: bool) -> Result<PathBuf> {
    let path = dir.join(SKILL_NAME).join("SKILL.md");
    let skill_dir = path.parent().expect("joined two components above");
    std::fs::create_dir_all(skill_dir)
        .with_context(|| format!("creating the skill directory {}", dir.display()))?;
    std::fs::write(&path, PM_SKILL)
        .with_context(|| format!("writing the PM skill to {}", path.display()))?;
    if project_local {
        write_inner_gitignore(skill_dir)?;
    }
    Ok(path)
}

/// Keeps a project-local install out of the user's commits without touching
/// the user's own `.gitignore`.
///
/// mana writes into the project here, and generated output should not turn up
/// in `git status` -- but editing somebody's `.gitignore` is an edit to a file
/// they wrote, tracked in their history, for a tool they may be trying out.
/// A `.gitignore` holding `*` *inside* the directory mana created ignores that
/// directory's contents (itself included) from within, needs no permission and
/// leaves nothing behind when the directory is deleted. It is written
/// unconditionally rather than only when the project's own ignore file misses
/// the path: matching gitignore semantics by hand is how a check like that gets
/// it wrong, and a second ignore file where one already covers it costs
/// nothing.
fn write_inner_gitignore(skill_dir: &Path) -> Result<()> {
    let path = skill_dir.join(".gitignore");
    std::fs::write(&path, IGNORE_EVERYTHING).with_context(|| format!("writing {}", path.display()))
}

/// Whether a catalogue skills directory belongs to the project rather than to
/// the user. `~/...` is the user's; anything else relative is the project's.
fn is_project_local(dir: &str) -> bool {
    !dir.starts_with('~') && Path::new(dir).is_relative()
}

/// The argv that attaches mana's tools to this PM, plus the argv that takes
/// away its ability to write code.
fn tool_channel_args(
    entry: &CliEntry,
    paths: &ProjectPaths,
    project_root: &Path,
) -> Result<Vec<String>> {
    let mut args = Vec::new();
    match entry.tools.channel {
        ToolChannel::Mcp => {
            let config = write_mcp_config(paths, project_root)?;
            let config = config.to_string_lossy().into_owned();
            let vars = HashMap::from([("config_path", config.as_str())]);
            args.extend(
                substitute(&entry.tools.mcp_args, &vars)
                    .with_context(|| format!("{}: [tools].mcp_args", entry.cli.id))?,
            );
        }
        // Nothing to attach: the PM emits fenced blocks and mana parses them
        // out of the same event stream (`crate::sentinel`), so there is no
        // server to register and no flag to pass.
        ToolChannel::Sentinel => {}
    }
    // Empty for a CLI with no equivalent flag, which is why this is data:
    // the PM's no-code rule is enforced where it can be and stated in the
    // skill everywhere else (design §6).
    args.extend(
        substitute(&entry.pm.permission_args, &HashMap::new())
            .with_context(|| format!("{}: [pm].permission_args", entry.cli.id))?,
    );
    Ok(args)
}

/// Writes the MCP registration the PM's CLI will read, and returns its path.
fn write_mcp_config(paths: &ProjectPaths, project_root: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe().context(
        "resolving mana's own path, which is what the PM's CLI will run to reach mana's tools",
    )?;
    let path = paths.root.join(MCP_CONFIG);
    crate::project::create_dir_all(&paths.root)?;
    let document = serde_json::to_string_pretty(&mcp_config(&exe, project_root))?;
    crate::project::write(&path, format!("{document}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The registration document.
///
/// `command` is mana's own absolute path rather than the string "mana": one of
/// v1's three launch blockers was that the PM's CLI could not find mana on
/// `$PATH`, and a binary that just ran from a build directory is not on it at
/// all (design §5). `--project-root` is passed because the PM's CLI spawns
/// this server from wherever it happens to be, and which project a task
/// belongs to is not negotiable at that point.
fn mcp_config(exe: &Path, project_root: &Path) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "mana": {
                "command": exe.to_string_lossy(),
                "args": [
                    "mcp-server",
                    "--project-root",
                    project_root.to_string_lossy(),
                ],
            }
        }
    })
}

/// Turns one `[skills].dirs` entry into a real path.
///
/// Two notations, both taken from the CLIs' own documentation: a leading `~`
/// is the user's home, and a relative path is *the project's* -- `.claude/skills`
/// means what it means in that documentation, which is a directory inside the
/// repository you are working in. Resolving it against the project root rather
/// than the process's working directory is what makes it hold for a mana
/// launched from anywhere.
fn resolve_skill_dir(path: &str, home: Option<&Path>, project_root: &Path) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    if is_project_local(path) {
        return project_root.join(path);
    }
    PathBuf::from(path)
}

/// Stops every sub-agent this project still has in flight, and reports what
/// happened in one or two lines.
///
/// Quitting mana ends the only thing watching a dispatch: the thread that would
/// have written its exit record and notified the PM went with the process. So a
/// sub-agent left alive after Ctrl+C is a run nobody will ever read, holding a
/// quota slot and a worktree, that `mana ps` keeps calling `running` until
/// somebody notices. Killing it is the honest end of the session.
///
/// It goes through the same `kill_dispatch` as `mana kill`, guard included: the
/// pid in a registry record may have been recycled onto a bystander, and a
/// refusal there is exactly as binding at teardown as it is at the command
/// line. Only *this* project is swept -- another mana in another directory has
/// its own agents and its own session, and quitting this one says nothing about
/// them.
///
/// Every running dispatch of this project is this session's, which is what
/// makes sweeping by project the same thing as sweeping your own: `mana
/// launch` holds `session_lock`, so the second session that used to have its
/// sub-agents killed here (#42) no longer starts. A dispatch left running by a
/// *previous* session is fair game too -- nothing is watching it either.
///
/// Never fails: this runs on the way out, after the terminal has been restored,
/// and a session that already ended cannot be failed any harder. It still says
/// whether it *worked*, because "three sub-agents outlived this session and are
/// still burning quota" is not something the exit code may hide (#96).
fn sweep_in_flight(home: &Path, project: &str, now: DateTime<Utc>) -> Sweep {
    let dispatches = match status::dispatches_in(home, project) {
        Ok(dispatches) => dispatches,
        Err(error) => {
            return Sweep {
                lines: vec![format!(
                    "mana: could not read this project's dispatches, so nothing was stopped: \
                     {error:#}"
                )],
                clean: false,
            };
        }
    };
    let paths = resolve_project_paths(home, project);
    let (mut killed, mut spared, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    for dispatch in dispatches
        .iter()
        .filter(|dispatch| dispatch.status == DispatchStatus::Running)
    {
        let agent = short(&dispatch.record.agent_id);
        match kill_dispatch(&paths, dispatch, now) {
            Ok(report) => {
                if let Some(reason) = report.refusal_reason {
                    spared.push(format!("{agent} -- {reason}"));
                } else {
                    killed.push(agent.to_string());
                }
            }
            Err(error) => failed.push(format!("{agent} -- {error:#}")),
        }
    }

    // A spared agent and a failed kill are the same fact for the exit code: a
    // process mana started is still running, and the operator now owns it.
    let clean = spared.is_empty() && failed.is_empty();
    let mut lines = Vec::new();
    if !killed.is_empty() {
        lines.push(format!(
            "mana: killed {} in-flight agent(s): {}",
            killed.len(),
            killed.join(", ")
        ));
    }
    if !spared.is_empty() {
        lines.push(format!(
            "mana: left {} in-flight agent(s) alone (pid guard refused)",
            spared.len()
        ));
        // In full, one per agent: the operator now owns a process mana would
        // not touch, and a count with no id and no reason is not something
        // anybody can act on.
        lines.extend(spared.into_iter().map(|reason| format!("  {reason}")));
    }
    for reason in failed {
        lines.push(format!("mana: could not stop {reason}"));
    }
    Sweep { lines, clean }
}

/// What the teardown sweep leaves behind: what to print, and whether the
/// machine was actually left as the session found it.
struct Sweep {
    lines: Vec<String>,
    /// False when a sub-agent outlived the sweep -- refused by the pid guard,
    /// failed to die, or never even enumerated because the registry would not
    /// read. A count would say no more than the lines already do; the exit code
    /// only needs the fact.
    clean: bool,
}

/// Follows `notifications.jsonl` from wherever it was when the session
/// started.
///
/// Starting at the end rather than at the beginning is the whole subtlety:
/// the file is append-only across every session this project ever had, and
/// replaying it would have the PM chasing tasks somebody closed last week.
struct NotificationTail {
    path: PathBuf,
    /// How far into the file this session has read. `None` until mana has
    /// managed to measure the file at all: a tail that does not know where the
    /// history ends must not guess zero, because zero means "replay every
    /// completion this project ever had" (#87).
    offset: Option<u64>,
    next_poll: Instant,
}

/// One thing a poll found.
///
/// A gap travels with the completions rather than being dropped because it is
/// news the PM has to act on: mana knows a dispatch finished and cannot say
/// which. Silently losing it leaves the PM waiting forever on work that is
/// already done and billed; silently replaying the file to find it again would
/// buy dozens of turns about tasks closed last week. Saying so is the only
/// honest third option.
enum TailEvent {
    Finished(Notification),
    Gap(String),
}

impl NotificationTail {
    fn new(path: PathBuf) -> Self {
        let offset = match std::fs::metadata(&path) {
            Ok(meta) => Some(meta.len()),
            // Not there yet is the ordinary launch: the first dispatch creates
            // it, and everything in it will belong to this session.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(0),
            // Anything else (a permission, a racing rotation) and mana does not
            // know how much history is already in the file. It settles on the
            // end at the first read that works, rather than take "could not
            // measure" for "empty" and replay the lot.
            Err(_) => None,
        };
        NotificationTail {
            path,
            offset,
            next_poll: Instant::now(),
        }
    }

    fn poll(&mut self, now: Instant) -> Vec<TailEvent> {
        if now < self.next_poll {
            return Vec::new();
        }
        self.next_poll = now + NOTIFICATION_POLL;
        self.read_new()
    }

    /// Reads the complete lines appended since the last read.
    ///
    /// Byte offsets, not line counts: several dispatch threads append to this
    /// file independently, so a read can land mid-write. Everything after the
    /// last newline is left for the next poll, and the offset advances by the
    /// bytes actually consumed -- a partial line is never parsed and never
    /// skipped. A failure to open or read returns "nothing new": a notification
    /// is a convenience, and losing the file must not take the session down.
    /// A file that *shrank*, or a line that will not parse, is different -- the
    /// offset has already moved past a dispatch nobody will ever hear about, so
    /// those come back as `Gap` rather than as silence.
    fn read_new(&mut self) -> Vec<TailEvent> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        let Some(offset) = self.offset else {
            // The first measurement mana could take: this session starts here.
            self.offset = Some(length);
            return Vec::new();
        };
        if length < offset {
            // Truncated, rotated or restored under us. Rereading from zero was
            // v2's answer and it is the expensive one: the offset is the only
            // thing that says which lines are old, so a replay injects every
            // completion this project ever had as a paid PM turn (#87).
            self.offset = Some(length);
            return vec![TailEvent::Gap(format!(
                "[mana] {} shrank from {offset} to {length} bytes -- it was truncated, rotated \
                 or restored under this session. mana follows it by byte offset, so it can no \
                 longer tell which completions are new, and it will not replay the file: that \
                 would announce dispatches from earlier sessions as fresh work. It resumed at \
                 the new end. Any dispatch that finished up to now will never be announced -- \
                 do not wait on one: read its verdict with get_review, or launch it again.",
                self.path.display(),
            ))];
        }
        if length == offset || file.seek(SeekFrom::Start(offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let complete = &bytes[..=last_newline];
        self.offset = Some(offset + complete.len() as u64);
        let mut events = Vec::new();
        let mut unreadable = Vec::new();
        for line in String::from_utf8_lossy(complete).lines() {
            // A blank line is padding, not a lost dispatch: nothing was ever
            // written there, so there is nothing to report.
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(notification) => events.push(TailEvent::Finished(notification)),
                // An interleaved write, or a mana of another version. The
                // dispatch is over and this was its only announcement (#88).
                Err(error) => unreadable.push((truncated(line), error)),
            }
        }
        // One turn however many lines were lost: a file that came back as
        // garbage would otherwise bill a PM turn per line. The raw text of the
        // first goes with it, because it usually still names the task and a PM
        // that can read the id can act on it.
        if let Some((line, error)) = unreadable.first() {
            events.push(TailEvent::Gap(format!(
                "[mana] {} completion line(s) in {} could not be read ({error}), so mana cannot \
                 say which dispatches finished. The first, verbatim: {line}. Do not keep waiting \
                 on a dispatch it may name -- that work is already done: read its verdict with \
                 get_review, or launch it again.",
                unreadable.len(),
                self.path.display(),
            )));
        }
        events
    }
}

/// A raw line, kept short enough to be one turn rather than a page: the PM
/// needs the task id it probably carries, not the whole record.
fn truncated(line: &str) -> String {
    const LIMIT: usize = 200;
    match line.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    }
}

/// The turn a finished dispatch becomes.
///
/// It ends by asking for a decision because the PM's loop is decide → dispatch
/// → decide: a bare status line invites an acknowledgement, and an
/// acknowledgement is a paid turn that moved nothing.
fn notification_message(notification: &Notification) -> String {
    format!(
        "[mana] {} finished for task {}: {}. Decide the next step.",
        notification.role.word(),
        notification.task_id,
        notification.outcome
    )
}

// ratatui 0.30 turned `Backend::Error` from `std::io::Error` into an
// associated type, so `terminal.draw(..)?` no longer converts into `anyhow`
// on its own: the bound is what says this loop only drives backends whose
// failures are reportable. Stated here rather than at the one call site
// because the whole reason this function is generic is the test backend.
fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    session: &mut Session,
    app: &mut App,
    graph: &mut GraphCache,
    events: &mut dyn EventSource,
    mut update_notice: Option<std::sync::mpsc::Receiver<String>>,
) -> Result<SessionEnd>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let started = Instant::now();
    loop {
        let now = Instant::now();

        // A trapped termination signal leaves by the door Ctrl+C uses, so the
        // terminal is restored, the PM reaped and the in-flight sub-agents
        // swept by the one piece of code that already does all three. Taken
        // rather than read: a session that survived somebody else's signal
        // must not inherit it.
        if TERMINATED.swap(false, Ordering::Relaxed) {
            return Ok(SessionEnd::UserQuit);
        }

        // At most one line, in mana's own voice, and only if a newer release
        // exists. Polled here rather than awaited before the TUI because a
        // launch must not wait on the network -- and because a line printed
        // before the alternate screen opens would be invisible until the
        // session ends.
        if let Some(line) = crate::cli::upgrade::poll_check(
            &mut update_notice,
            now.saturating_duration_since(started),
        ) {
            app.push(Source::Mana, &line);
        }
        let mut ended = None;
        let mut tools_ran = false;
        for event in session.drain() {
            // Everything the PM says passes the tool channel first: on a
            // sentinel CLI its fenced blocks are calls to execute, and what
            // remains is the half worth rendering as conversation.
            if let PmEvent::Text(text) = &event {
                let pass = session.apply_tools(text);
                tools_ran |= !pass.log.is_empty();
                match pass.prose {
                    Some(prose) if prose.trim().is_empty() => {}
                    // A message that was nothing but blocks renders as its
                    // tool lines alone, rather than as an empty PM turn.
                    Some(prose) => app.apply(&PmEvent::Text(prose)),
                    None => app.apply(&event),
                }
                for line in pass.log {
                    app.push(tool_line_source(&line), &line.text);
                }
                continue;
            }
            // The PM is free again, so whatever was held goes now -- one turn
            // per boundary, in the order it was written, because two turns
            // released at once would land on top of each other exactly the way
            // the queue exists to prevent.
            if event == PmEvent::TurnEnded
                && let Some((queued, sent)) = session.release_next()
            {
                if queued.typed {
                    app.release_pending();
                }
                if let Err(error) = sent {
                    let line = session
                        .record_loss(format!("[mana] that turn did not reach the PM: {error:#}"));
                    app.push(Source::Mana, &line);
                }
            }
            app.apply(&event);
            if let PmEvent::Exited { code } = event {
                ended = Some(code);
                // Nothing queued will ever be sent now. Said out loud, with
                // the words in it: the operator typed them, and a queue that
                // emptied itself quietly would leave them believing the PM had
                // read them. Kept as losses too, because this pane is about to
                // be torn down (#181).
                for line in session.flush_queue("the PM is gone") {
                    app.push(Source::Mana, &line);
                }
            }
        }

        // A dispatch reporting in is the one event that certainly changed the
        // graph, so it is what refreshes it -- not the frame rate.
        //
        // A delivery failure is almost always a PM that just died, and the
        // `Exited` event ends the session a tick later with a better message
        // than this one. Reported and carried on rather than propagated: the
        // user should see why, in the pane they are already looking at.
        let finished = match session.poll_notifications(now) {
            Ok(finished) => finished,
            Err(error) => {
                let line = session.record_loss(format!(
                    "[mana] could not tell the PM a dispatch finished: {error:#}"
                ));
                app.push(Source::Mana, &line);
                Vec::new()
            }
        };
        // A tool call is the other thing that certainly changed the graph: a
        // sentinel `create_task` wrote a task file this very tick.
        let changed = !finished.is_empty() || tools_ran;
        for message in finished {
            app.push(Source::Mana, &message);
        }
        graph.refresh(&session.paths, now, changed);

        // The queue lives in the session and the status bar lives in the view,
        // so the count is copied across once a frame rather than mirrored in
        // two places that could disagree.
        app.queued = session.queued();
        terminal.draw(|frame| render::draw(frame, app, graph.nodes()))?;

        // Drawn first, so the last thing the PM said is on screen before mana
        // reports that it is gone.
        if let Some(code) = ended {
            return Ok(SessionEnd::PmExited { code });
        }

        if let Some(raw) = events.poll_event(TICK)? {
            let app_event = match raw {
                RawEvent::Key(key) => map_key_event(key.code, key.modifiers),
                RawEvent::Paste(text) => Some(AppEvent::Paste(text)),
            };
            if let Some(app_event) = app_event
                && !apply_app_event(app_event, app, session)
            {
                return Ok(SessionEnd::UserQuit);
            }
        }
    }
}

/// Applies one decoded key. `false` means quit.
///
/// Nothing here can fail the session: a turn that cannot be delivered says so
/// in the chat pane, because the user typed it and deserves an answer now,
/// and because tearing the TUI down over it would take the explanation with
/// it. The PM's `Exited` event is what actually ends the session.
fn apply_app_event(event: AppEvent, app: &mut App, session: &mut Session) -> bool {
    match event {
        AppEvent::Quit => return false,
        AppEvent::ToggleGraph => app.toggle_graph(),
        AppEvent::ToggleRaw => app.toggle_raw(),
        AppEvent::AnswerPermission(allow) => answer_permission(allow, app, session),
        AppEvent::Key(c) => app.input.push(c),
        // Appended whole, newlines included: bracketed paste exists so the
        // terminal tells mana "this is one paste", and splitting it back up
        // here would recreate the one-turn-per-line bug it fixes (#160).
        AppEvent::Paste(text) => app.input.push_str(&text),
        AppEvent::Backspace => {
            // One keypress, one glyph. `String::pop` takes a code point, and a
            // glyph is often several -- a ZWJ emoji, a base plus a combining
            // mark, a flag -- so popping one left a *different* valid glyph
            // behind rather than deleting anything, and the operator could send
            // it without seeing that the text had changed under them (#182).
            // Truncating at the last boundary deletes the cluster whole; on an
            // empty buffer there is no boundary and 0 is the same no-op `pop`
            // already was.
            let boundary = app
                .input
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(at, _)| at);
            app.input.truncate(boundary);
        }
        AppEvent::Enter => {
            let message = std::mem::take(&mut app.input);
            if message.trim() == "/graph" {
                // A local UI command: sending it would just leave the PM
                // wondering what "/graph" was supposed to mean.
                app.toggle_graph();
            } else if !message.trim().is_empty() {
                // Echoed before the outcome is known, and echoed the same way
                // either way: the user's own words belong in the transcript at
                // the moment they pressed Enter, whether the PM reads them now
                // or in a minute. What changes is the mark in the gutter.
                match session.send_typed(&message) {
                    Ok(Delivery::Sent) => app.push(Source::User, &message),
                    Ok(Delivery::Queued) => app.push_pending(Source::User, &message),
                    Err(error) => {
                        app.push(Source::User, &message);
                        let line = session.record_loss(format!(
                            "[mana] that turn did not reach the PM: {error:#}"
                        ));
                        app.push(Source::Mana, &line);
                    }
                }
            }
        }
    }
    true
}

/// The first line of a message, for a notice that has to fit in the pane. A
/// queued turn is usually one line anyway -- this is the guard for the ones
/// mana wrote itself, which can run to the whole role text.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Answers the permission the PM is waiting on, if there is one.
///
/// The request is taken out of the app before the transport is called, so a
/// failed answer cannot leave a prompt on screen that nothing will ever clear
/// -- an agent that died mid-question is not going to ask again.
fn answer_permission(allow: bool, app: &mut App, session: &mut Session) {
    let Some(pending) = app.take_permission() else {
        // A key pressed when nothing was asked. Silent on purpose: telling the
        // user off for a keystroke is noise in the one pane they are reading.
        return;
    };
    let Some(choice) = pending.choice(allow) else {
        app.push(
            Source::Mana,
            &format!(
                "[mana] the PM offered no way to {} that -- it is still waiting",
                if allow { "allow" } else { "reject" }
            ),
        );
        // Nothing was answered, so the request goes back: the operator can
        // still press the other key.
        app.pending_permission = Some(pending);
        return;
    };
    let verdict = choice.label.clone();
    match session.answer_permission(pending.id, &choice.id) {
        Ok(()) => app.push(Source::Mana, &format!("[mana] answered: {verdict}")),
        Err(error) => {
            let line = session.record_loss(format!(
                "[mana] that answer did not reach the PM: {error:#}"
            ));
            app.push(Source::Mana, &line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests build notifications by hand; production reads the role
    // off the record it was given, so this import lives here rather than at
    // the top of the file where it would be dead in a release build.
    use crate::catalog::parse_entry;
    use crate::task::Role;

    /// The guard leaves the terminal as it found it, whichever half of its own
    /// entry it got through.
    ///
    /// Both cases are real and both are checked by the same assertion. Under a
    /// test harness with no controlling terminal -- how CI runs -- `enter`
    /// fails on its first step and must leave nothing behind: the early-error
    /// path #75 opened by enabling raw mode before it could fail. Run from a
    /// real terminal it succeeds instead, and the assertion then proves `Drop`
    /// undid both raw mode and the alternate screen.
    #[test]
    fn entering_the_terminal_is_all_or_nothing() {
        drop(TerminalGuard::enter());

        // `ok()` rather than `unwrap()`: on a Windows runner with no console
        // attached the query itself fails, which is the same answer -- there
        // was no raw mode to leave on.
        assert_ne!(crossterm::terminal::is_raw_mode_enabled().ok(), Some(true));
    }

    /// `entering_the_terminal_is_all_or_nothing` above proves raw mode is
    /// undone, but that check is a query against the real terminal and
    /// cannot see bracketed paste the same way. What can be observed is the
    /// bytes `TerminalGuard::enter` and `restore_terminal` write -- both
    /// funnel through `write_enter_sequence`/`write_restore_sequence`, which
    /// take any `Write`, so a `Vec<u8>` stands in for the terminal here.
    ///
    /// Not covered by this or any test: that `enter`/`restore_terminal`
    /// actually call these two functions on the real stdout/backend at the
    /// right moments (panic hook, `Drop`, the normal return). That wiring has
    /// no terminal-independent seam -- it is exercised only by running mana
    /// against a real terminal.
    #[test]
    fn bracketed_paste_enable_and_disable_are_symmetric() {
        let mut entered = Vec::new();
        write_enter_sequence(&mut entered).unwrap();
        let entered = String::from_utf8(entered).unwrap();
        assert!(
            entered.contains("\x1b[?2004h"),
            "enter sequence did not enable bracketed paste: {entered:?}"
        );

        let mut restored = Vec::new();
        write_restore_sequence(&mut restored).unwrap();
        let restored = String::from_utf8(restored).unwrap();
        assert!(
            restored.contains("\x1b[?2004l"),
            "restore sequence did not disable bracketed paste: {restored:?}"
        );
    }

    /// A PM that said nothing on stderr still gets quoted: stdout is the
    /// second-best explanation there is, and no explanation at all is the
    /// worst one. Only the *preference* changed with #189.
    #[test]
    fn the_death_report_falls_back_to_stdout_when_stderr_said_nothing() {
        let mut app = App::new("Fake Agy");
        assert_eq!(death_reason(&app), "");

        app.apply(&PmEvent::Raw("panic: index out of range".to_string()));
        assert_eq!(
            death_reason(&app),
            "\nits last output was: panic: index out of range"
        );

        app.apply(&PmEvent::Stderr("boom: no credentials".to_string()));
        assert_eq!(
            death_reason(&app),
            "\nits last output was: boom: no credentials"
        );
    }

    /// The handler does one thing, and doing anything more inside it would be
    /// undefined behaviour. Proved by sending mana every signal that used to
    /// kill it outright: the process survives, and the flag the loop reads is
    /// set. A signal left untrapped kills this test binary here exactly the
    /// way it killed a session -- no `Drop`, no restore (#84, #180).
    #[cfg(unix)]
    #[test]
    fn every_trapped_termination_signal_only_sets_the_flag() {
        trap_termination();
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            // `raise` delivers to the calling thread and returns only once the
            // handler has run, so the flag can be taken back immediately -- and
            // it has to be, before a run loop in a parallel test reads it as
            // its own.
            unsafe { libc::raise(signal) };
            assert!(
                TERMINATED.swap(false, Ordering::Relaxed),
                "signal {signal} reached the default disposition"
            );
        }
    }

    /// A complete entry for a CLI that does not exist, built through the real
    /// parser so it cannot drift from the schema.
    pub(super) fn entry_source(
        bin: &str,
        skills: &[&str],
        permission: &str,
        channel: &str,
    ) -> String {
        let skills = serde_json::to_string(skills).unwrap();
        format!(
            r#"
schema = 1
notes = "launch fixture"

[cli]
id = "fixture"
name = "Fixture CLI"
bin = "{bin}"
version_args = ["--version"]

[pm]
driver = "stream"
args = ["-p"]
prompt = "stdin-jsonl"
{permission}

[pm.events]
text = "$.message.content[?@.type=='text'].text"
usage = "$.usage"
turn_end = {{ path = "$.type", equals = "result" }}

[tools]
channel = "{channel}"
mcp_args = ["--mcp-config", "{{config_path}}"]

[subagent]
args = []
prompt = "argv"
max_concurrent = 0
cwd_required_in_brief = false

[models]
discovery_args = []

[skills]
dirs = {skills}

[install]
url = "https://example.invalid/fixture"
"#
        )
    }

    fn entry(skills: &[&str]) -> CliEntry {
        parse_entry(&entry_source("fixture-cli", skills, "", "mcp")).unwrap()
    }

    /// `mana doctor` has always known this; until now only `doctor` said it,
    /// and the operator who can act on it is the one starting the session.
    #[test]
    fn the_launch_reports_what_doctor_knows_about_the_cli_it_is_starting() {
        let mut entry = entry(&["~/.fixture/skills"]);
        // The one degradation this test is about, so the fixture's own missing
        // auto-approve flag does not stand in for it.
        entry.subagent.auto_approve_args = vec!["--yes".to_string()];
        assert!(entry.pm.permission_args.is_empty());

        let notice = degradation_notice(&entry).join("\n");
        assert!(
            notice.contains("no PM permission flags declared"),
            "{notice}"
        );

        // Declared flags, nothing else degraded: silence.
        entry.pm.permission_args = vec!["--no-write".to_string()];
        assert!(degradation_notice(&entry).is_empty());
    }

    fn paths_in(home: &Path) -> ProjectPaths {
        let paths = resolve_project_paths(home, "demo");
        ensure_project_structure(&paths).unwrap();
        paths
    }

    /// Most tests here have nothing to say about the project directory.
    fn install(entry: &CliEntry, home: Option<&Path>) -> SkillInstall {
        install_pm_skill(entry, home, Path::new("/nonexistent-project")).unwrap()
    }

    #[test]
    fn the_skill_lands_in_the_first_directory_that_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let second = tmp.path().join("agents/skills");
        std::fs::create_dir_all(&second).unwrap();
        let entry = entry(&[
            tmp.path().join("claude/skills").to_str().unwrap(),
            second.to_str().unwrap(),
        ]);

        let skill = install(&entry, None);
        assert_eq!(skill.path, second.join("mana-pm/SKILL.md"));
        assert_eq!(std::fs::read_to_string(&skill.path).unwrap(), PM_SKILL);
        // The directory that did not exist is left alone.
        assert!(!tmp.path().join("claude/skills").exists());
        assert!(skill.notes.is_empty(), "{:?}", skill.notes);
    }

    /// A fresh machine has none of them, and refusing to launch over a
    /// missing config directory would be absurd.
    #[test]
    fn the_first_directory_is_created_when_none_exists_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("claude/skills");
        let entry = entry(&[
            first.to_str().unwrap(),
            tmp.path().join("agents/skills").to_str().unwrap(),
        ]);

        let skill = install(&entry, None);
        assert_eq!(skill.path, first.join("mana-pm/SKILL.md"));
        assert!(skill.path.exists());
    }

    /// The drift-proofing: whatever was there is replaced by what shipped in
    /// this binary.
    #[test]
    fn an_existing_skill_file_is_overwritten_every_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        std::fs::create_dir_all(dir.join(SKILL_NAME)).unwrap();
        std::fs::write(dir.join(SKILL_NAME).join("SKILL.md"), "stale v1 text").unwrap();

        let skill = install(&entry(&[dir.to_str().unwrap()]), None);
        assert_eq!(std::fs::read_to_string(skill.path).unwrap(), PM_SKILL);
    }

    /// The pollution fix: a relative directory belongs to the project, wins
    /// over the global ones whether it exists yet or not, and carries its own
    /// `.gitignore` so it never turns up in the user's `git status`.
    #[test]
    fn a_project_local_directory_wins_and_ignores_itself_from_within() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("code/demo");
        let global = tmp.path().join("home/.claude/skills");
        // The global one exists and the project one does not, which under the
        // old rule ("first that exists") is exactly how the skill ended up in
        // everybody's global list.
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let entry = entry(&[".claude/skills", global.to_str().unwrap()]);

        let skill = install_pm_skill(&entry, None, &project).unwrap();
        assert_eq!(skill.path, project.join(".claude/skills/mana-pm/SKILL.md"));
        // Ignored from inside the directory mana created, so the user's own
        // .gitignore is never touched.
        assert_eq!(
            std::fs::read_to_string(project.join(".claude/skills/mana-pm/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".gitignore")).ok(),
            None
        );
    }

    /// A relative directory is resolved against the project, not against
    /// whatever directory the mana process happens to be in.
    #[test]
    fn skills_directories_are_read_as_home_project_or_absolute() {
        let home = PathBuf::from("/home/x");
        let project = PathBuf::from("/code/demo");
        assert_eq!(
            resolve_skill_dir("~/.claude/skills", Some(&home), &project),
            PathBuf::from("/home/x/.claude/skills")
        );
        assert_eq!(
            resolve_skill_dir(".claude/skills", Some(&home), &project),
            PathBuf::from("/code/demo/.claude/skills")
        );
        assert_eq!(
            resolve_skill_dir("/etc/skills", Some(&home), &project),
            PathBuf::from("/etc/skills")
        );
        // `~someone/skills` is another user's home, which is not a thing mana
        // resolves -- and not a thing any catalogue entry writes. It is not the
        // project's either.
        assert_eq!(
            resolve_skill_dir("~other/skills", Some(&home), &project),
            PathBuf::from("~other/skills")
        );
        // No home directory to expand against: better a relative path than a
        // panic on a machine without one.
        assert_eq!(
            resolve_skill_dir("~/skills", None, &project),
            PathBuf::from("~/skills")
        );
    }

    /// The half that cleans up after older versions of mana: the copy an
    /// earlier launch left in the global list is deleted, said out loud, and
    /// nothing else in that directory is touched.
    #[test]
    fn a_stale_copy_in_another_directory_is_removed_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo");
        let global = tmp.path().join("home/.claude/skills");
        std::fs::create_dir_all(global.join(SKILL_NAME)).unwrap();
        std::fs::write(global.join(SKILL_NAME).join("SKILL.md"), "an older role").unwrap();
        // Somebody else's skill, in the same directory. Not mana's to delete.
        std::fs::create_dir_all(global.join("their-skill")).unwrap();
        std::fs::write(global.join("their-skill/SKILL.md"), "theirs").unwrap();

        let entry = entry(&[".claude/skills", global.to_str().unwrap()]);
        let skill = install_pm_skill(&entry, None, &project).unwrap();

        assert!(skill.path.exists());
        assert!(!global.join(SKILL_NAME).exists(), "the stale copy survived");
        assert!(global.join("their-skill/SKILL.md").exists());
        assert_eq!(skill.notes.len(), 1, "{:?}", skill.notes);
        assert!(
            skill.notes[0].contains(global.to_str().unwrap()),
            "{:?}",
            skill.notes
        );
    }

    /// mana now writes into the user's repository, and a repository is not
    /// always writable. Falling through to the next directory beats refusing
    /// to launch in a project the CLI itself would have been happy in.
    ///
    /// Unix-only: the failure is produced with a mode, and Windows spells
    /// "you may not write here" differently.
    #[cfg(unix)]
    #[test]
    fn an_unwritable_first_choice_falls_through_to_the_next_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("read-only");
        let global = tmp.path().join("home/.claude/skills");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o555)).unwrap();
        let entry = entry(&[".claude/skills", global.to_str().unwrap()]);

        let skill = install_pm_skill(&entry, None, &project).unwrap();

        assert_eq!(skill.path, global.join("mana-pm/SKILL.md"));
        assert!(skill.path.exists());
        assert_eq!(skill.notes.len(), 1, "{:?}", skill.notes);
        assert!(
            skill.notes[0].contains("trying the next directory"),
            "{:?}",
            skill.notes
        );

        // So the temp dir can be cleaned up.
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The shipped claude entry, end to end -- because the whole feature is
    /// data and a fixture entry cannot prove the data is right. Both halves at
    /// once: the role lands in the project, and the copy an earlier mana left
    /// in the user's global skill list is gone.
    #[test]
    fn the_shipped_claude_entry_installs_into_the_project_and_clears_the_global_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("code/demo");
        std::fs::create_dir_all(&project).unwrap();
        // What every launch before this change left behind, in the list the
        // user actually curates.
        std::fs::create_dir_all(home.join(".claude/skills/mana-pm")).unwrap();
        std::fs::write(
            home.join(".claude/skills/mana-pm/SKILL.md"),
            "an older role",
        )
        .unwrap();

        let catalog = Catalog::embedded().unwrap();
        let skill =
            install_pm_skill(catalog.get("claude").unwrap(), Some(&home), &project).unwrap();

        assert_eq!(skill.path, project.join(".claude/skills/mana-pm/SKILL.md"));
        assert!(
            !home.join(".claude/skills/mana-pm").exists(),
            "the global copy survived"
        );
        // The user's own skills directory is still there -- only `mana-pm/`
        // inside it was mana's to remove.
        assert!(home.join(".claude/skills").is_dir());
        assert_eq!(
            std::fs::read_to_string(project.join(".claude/skills/mana-pm/.gitignore")).unwrap(),
            IGNORE_EVERYTHING
        );
        assert_eq!(skill.notes.len(), 1, "{:?}", skill.notes);
    }

    /// The claim the inner `.gitignore` makes, checked against git itself: a
    /// project mana installed into shows nothing to commit.
    #[test]
    fn a_project_local_install_leaves_git_status_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&project)
                .output()
                .expect("git is installed")
        };
        git(&["-c", "init.defaultBranch=main", "init", "-q"]);

        let catalog = Catalog::embedded().unwrap();
        install_pm_skill(catalog.get("claude").unwrap(), None, &project).unwrap();

        let status = git(&["status", "--porcelain"]);
        assert_eq!(
            String::from_utf8_lossy(&status.stdout),
            "",
            "mana's own output turned up in the user's git status"
        );
    }

    /// #192: a freshly `git init`-ed project has no commits, so there is
    /// nothing to branch a task worktree from -- refused at startup, not
    /// after the PM has planned and dispatched.
    #[test]
    fn check_preconditions_refuses_a_repo_with_no_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "-q"])
            .current_dir(&project)
            .output()
            .expect("git is installed");

        let error = check_preconditions(&project, true).unwrap_err().to_string();
        assert!(error.contains("make an initial"), "got: {error}");
        assert!(error.contains("Nothing has been started"), "got: {error}");
    }

    /// A repo with one commit is exactly what a dispatch needs -- accepted.
    #[test]
    fn check_preconditions_accepts_a_repo_with_one_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&project)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .expect("git is installed")
        };
        git(&["-c", "init.defaultBranch=main", "init", "-q"]);
        std::fs::write(project.join("README.md"), "hello\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);

        check_preconditions(&project, true).unwrap();
    }

    /// A non-git directory still gets `ensure_git_repo`'s own refusal, not
    /// the no-commits one -- the two checks run in that order and must not
    /// blur together.
    #[test]
    fn check_preconditions_reports_a_non_git_directory_as_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("plain");
        std::fs::create_dir_all(&project).unwrap();

        let error = check_preconditions(&project, true).unwrap_err().to_string();
        assert!(error.contains("not a git repository"), "got: {error}");
        assert!(!error.contains("no commits"), "got: {error}");
    }

    /// Nothing to clean is the normal case, and it says nothing at all.
    #[test]
    fn a_launch_with_no_stale_copy_anywhere_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = entry(&[
            ".claude/skills",
            tmp.path().join("global").to_str().unwrap(),
        ]);
        let skill = install_pm_skill(&entry, None, &tmp.path().join("demo")).unwrap();
        assert!(skill.notes.is_empty(), "{:?}", skill.notes);
    }

    #[test]
    fn a_catalogue_entry_with_nowhere_to_put_the_skill_says_so() {
        let error = install_pm_skill(&entry(&[]), None, Path::new("/tmp")).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("[skills].dirs"), "{rendered}");
        assert!(rendered.contains("fixture"), "{rendered}");
    }

    /// The round trip `mana launch -c` rests on: what one launch wrote, the
    /// next one reads, including the ACP session id it will resume by.
    #[test]
    fn the_project_state_survives_a_write_and_a_read() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(load_state(&paths), ProjectState::default());

        let mut state = ProjectState {
            last_cli: Some("opencode".to_string()),
            sessions: BTreeMap::new(),
        };
        state
            .sessions
            .insert("opencode".to_string(), "ses_ff91".to_string());
        save_state(&paths, &state).unwrap();

        assert_eq!(load_state(&paths), state);
        // Kept where the rest of the project's state lives, in the one format
        // mana reads and writes.
        assert!(paths.root.join("state.toml").is_file());
    }

    /// It is a cache, not configuration: a launch must not fail because
    /// something scribbled in it.
    #[test]
    fn an_unreadable_state_file_reads_as_no_state_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::write(state_path(&paths), "this is not toml at all").unwrap();
        assert_eq!(load_state(&paths), ProjectState::default());
    }

    #[test]
    fn a_named_cli_is_taken_as_given_whether_or_not_the_launch_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(
            resolve_cli(&paths, Some("claude"), false).unwrap(),
            "claude"
        );
        assert_eq!(resolve_cli(&paths, Some("agy"), true).unwrap(), "agy");
    }

    /// The whole point of remembering: `mana launch -c`, no argument.
    #[test]
    fn a_bare_continue_uses_the_last_cli_this_project_launched() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        save_state(
            &paths,
            &ProjectState {
                last_cli: Some("opencode".to_string()),
                sessions: BTreeMap::new(),
            },
        )
        .unwrap();
        assert_eq!(resolve_cli(&paths, None, true).unwrap(), "opencode");
    }

    #[test]
    fn a_bare_continue_with_nothing_remembered_says_to_name_a_cli_once() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rendered = format!("{:#}", resolve_cli(&paths, None, true).unwrap_err());
        assert!(rendered.contains("--continue"), "{rendered}");
        assert!(rendered.contains("mana launch claude"), "{rendered}");
    }

    /// `mana launch` with no CLI and no --continue is not a resume, it is a
    /// missing argument.
    #[test]
    fn a_bare_launch_asks_for_a_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rendered = format!("{:#}", resolve_cli(&paths, None, false).unwrap_err());
        assert!(rendered.contains("needs a CLI"), "{rendered}");
    }

    /// The activation is where a CLI learns everything mana will not repeat:
    /// where the role is, and -- for the CLIs that need it -- what it says.
    #[test]
    fn the_activation_names_the_skill_and_inlines_it_only_when_asked_to() {
        let skill = Path::new("/home/x/.agents/skills/mana-pm/SKILL.md");
        let mut entry = entry(&["~/.agents/skills"]);

        let plain = activation(&entry, skill, None);
        assert!(plain.starts_with(ACTIVATION), "{plain}");
        assert!(plain.contains(skill.to_str().unwrap()), "{plain}");
        assert!(
            !plain.contains(PM_SKILL),
            "the role text was sent uninvited"
        );
        // An MCP CLI discovers its tools from the protocol and is told nothing
        // about fenced blocks -- it holds real ones.
        assert!(!plain.contains("```mana"), "{plain}");

        // A CLI that cannot read the file gets the text itself, once, at the
        // start of the session.
        entry.skills.inline_in_activation = true;
        let inlined = activation(&entry, skill, None);
        assert!(inlined.contains(PM_SKILL), "the role text never arrived");
        assert!(inlined.contains(skill.to_str().unwrap()), "{inlined}");
    }

    #[test]
    fn a_sentinel_cli_is_told_how_to_call_the_tools_it_cannot_host() {
        let mut entry =
            parse_entry(&entry_source("fixture-cli", &["/nowhere"], "", "sentinel")).unwrap();
        entry.skills.inline_in_activation = false;
        let message = activation(&entry, Path::new("/tmp/SKILL.md"), Some("n0nce"));
        assert!(message.contains("fenced ```mana:n0nce block"), "{message}");
        // The activation is the only place the nonce is issued, so it is also
        // the only place that can say what a fence *without* it does -- a PM
        // told the shape and not the rule would still be told nothing about
        // the block it quotes out of a file (#140).
        assert!(message.contains("inert"), "{message}");
    }

    /// The nonce is minted per `Sentinel`, so a resumed session's PM is
    /// holding a dead one: it has to be re-issued or the resumed session can
    /// call no tool at all.
    #[test]
    fn a_resumed_sentinel_session_is_given_the_new_nonce() {
        let resumed = format!("{RESUMED}{}", resumed_nonce("n3w"));
        assert!(resumed.starts_with(RESUMED), "{resumed}");
        assert!(resumed.contains("```mana:n3w"), "{resumed}");
        // ...and not by replaying the briefing a continued conversation
        // already had.
        assert!(!resumed.contains(PM_SKILL), "{resumed}");
    }

    #[test]
    fn skills_directories_are_written_with_a_tilde_and_read_from_the_home_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = entry(&["~/.fixture/skills"]);
        let skill = install(&entry, Some(tmp.path()));
        assert_eq!(
            skill.path,
            tmp.path().join(".fixture/skills/mana-pm/SKILL.md")
        );
        // A global install gets no inner .gitignore: it is not in anybody's
        // repository, and a stray ignore file in a skills directory the user
        // curates would be mana leaving litter.
        assert!(!skill.path.parent().unwrap().join(".gitignore").exists());
    }

    /// The `$PATH` blocker, structurally: the config names a binary by its
    /// absolute path, so nothing has to be installed anywhere for the PM to
    /// reach mana's tools.
    #[test]
    fn the_mcp_config_points_at_manas_own_binary_and_the_project() {
        let document = mcp_config(
            Path::new("/opt/mana/bin/mana"),
            Path::new("/home/x/code/demo"),
        );
        let server = &document["mcpServers"]["mana"];
        assert_eq!(server["command"], "/opt/mana/bin/mana");
        assert_eq!(
            server["args"],
            serde_json::json!(["mcp-server", "--project-root", "/home/x/code/demo"])
        );
    }

    #[test]
    fn the_mcp_config_is_written_and_substituted_into_the_catalogue_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = tmp.path().join("code/demo");

        let args = tool_channel_args(&entry(&["/nowhere"]), &paths, &project).unwrap();
        let config = paths.root.join(MCP_CONFIG);
        assert_eq!(
            args,
            vec![
                "--mcp-config".to_string(),
                config.to_string_lossy().into_owned()
            ]
        );

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["mana"]["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            written["mcpServers"]["mana"]["args"][2],
            project.to_string_lossy().as_ref()
        );
    }

    #[test]
    fn permission_args_are_appended_after_the_tool_channel_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let entry = parse_entry(&entry_source(
            "fixture-cli",
            &["/nowhere"],
            r#"permission_args = ["--allowedTools", "mcp__mana__*,Read"]"#,
            "mcp",
        ))
        .unwrap();

        let args = tool_channel_args(&entry, &paths, tmp.path()).unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(&args[2..], ["--allowedTools", "mcp__mana__*,Read"]);
    }

    /// A sentinel CLI has no config to write and no flag to pass; its
    /// permission argv, if it had any, would still apply.
    #[test]
    fn a_sentinel_channel_asks_for_no_mcp_flags_and_writes_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let entry =
            parse_entry(&entry_source("fixture-cli", &["/nowhere"], "", "sentinel")).unwrap();

        assert!(
            tool_channel_args(&entry, &paths, tmp.path())
                .unwrap()
                .is_empty()
        );
        assert!(!paths.root.join(MCP_CONFIG).exists());
    }

    /// The one place the shipped ACP entries meet the launch flow: whether a
    /// CLI gets a config file and a flag, or nothing at all, is catalogue data
    /// and this is what proves the data reaches the argv.
    #[test]
    fn the_shipped_acp_entries_get_exactly_the_tool_flags_they_declare() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = tmp.path().join("code/demo");
        let catalog = Catalog::embedded().unwrap();

        // opencode is attached through the ACP handshake, so there is nothing
        // to pass on the command line...
        let opencode = catalog.get("opencode").unwrap();
        assert!(
            tool_channel_args(opencode, &paths, &project)
                .unwrap()
                .is_empty()
        );

        // ...while copilot refuses stdio MCP servers offered that way, so it
        // gets mana's config file by path, with the `@` its flag needs.
        let copilot = catalog.get("copilot").unwrap();
        let args = tool_channel_args(copilot, &paths, &project).unwrap();
        let config = paths.root.join(MCP_CONFIG);
        assert_eq!(
            args,
            vec![
                "--additional-mcp-config".to_string(),
                format!("@{}", config.display()),
            ]
        );
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["mana"]["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
    }

    pub(super) fn notification(role: Role, task_id: &str, outcome: &str) -> Notification {
        Notification {
            ts: "2026-08-15T10:00:00Z".to_string(),
            task_id: task_id.to_string(),
            role,
            agent_id: "agent-1".to_string(),
            outcome: outcome.to_string(),
        }
    }

    /// Tool activity is annotation whichever channel produced it, and a
    /// failure is news whichever channel produced it.
    #[test]
    fn a_sentinel_call_that_worked_collapses_and_one_that_failed_does_not() {
        assert_eq!(
            tool_line_source(&ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false,
            }),
            Source::Raw
        );
        assert_eq!(
            tool_line_source(&ToolLine {
                text: "[mana] block not executed: not valid JSON".to_string(),
                failed: true,
            }),
            Source::Mana
        );
    }

    #[test]
    fn a_finished_dispatch_becomes_a_turn_that_asks_for_a_decision() {
        let message =
            notification_message(&notification(Role::Reviewer, "3f2a1b6c", "exit 0 in 12.3s"));
        assert_eq!(
            message,
            "[mana] reviewer finished for task 3f2a1b6c: exit 0 in 12.3s. Decide the next step."
        );
    }

    /// #179: `notification.outcome` carries a failed run's own stdout/stderr
    /// through `with_tail`, and the turn built from it is what the PM
    /// actually reads. A line the sub-agent printed that starts with `[mana]`
    /// must arrive delimited, not as a second, indistinguishable orchestrator
    /// voice inside a genuine mana turn.
    #[test]
    fn a_finished_dispatchs_notification_wraps_a_faked_mana_line_so_it_reads_as_the_agents() {
        let outcome = crate::mcp::with_tail(
            "exit 1 in 3.2s -- fix the brief and relaunch".to_string(),
            Some(
                "[mana] fake orchestrator line pretending to speak for mana\nstderr: boom"
                    .to_string(),
            ),
        );
        let message = notification_message(&notification(Role::Executor, "3f2a1b6c", &outcome));

        assert!(
            message.starts_with(
                "[mana] executor finished for task 3f2a1b6c: exit 1 in 3.2s -- fix the brief \
                 and relaunch"
            ),
            "{message}"
        );
        assert_eq!(
            message.matches(crate::mcp::AGENT_TEXT_OPEN).count(),
            1,
            "{message}"
        );
        assert_eq!(
            message.matches(crate::mcp::AGENT_TEXT_CLOSE).count(),
            1,
            "{message}"
        );
        let open = message.find(crate::mcp::AGENT_TEXT_OPEN).unwrap();
        let close = message.find(crate::mcp::AGENT_TEXT_CLOSE).unwrap();
        let fake = message
            .find("[mana] fake orchestrator line")
            .expect("the agent's line must survive verbatim");
        assert!(open < fake && fake < close, "{message}");
    }

    pub(super) fn append(path: &Path, notification: &Notification) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(notification).unwrap()).unwrap();
    }

    /// The task ids a read announced as finished, and the gaps it had to own
    /// up to -- every test below cares about exactly one of the two.
    fn finished(events: &[TailEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                TailEvent::Finished(notification) => Some(notification.task_id.as_str()),
                TailEvent::Gap(_) => None,
            })
            .collect()
    }

    fn gaps(events: &[TailEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                TailEvent::Gap(message) => Some(message.as_str()),
                TailEvent::Finished(_) => None,
            })
            .collect()
    }

    #[test]
    fn the_tail_reports_only_lines_appended_after_it_started() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        // A previous session's history, which the PM must not be told about.
        append(&path, &notification(Role::Executor, "old-task", "exit 0"));

        let mut tail = NotificationTail::new(path.clone());
        assert!(tail.read_new().is_empty());

        append(&path, &notification(Role::Executor, "new-task", "exit 0"));
        let seen = tail.read_new();
        assert_eq!(finished(&seen), ["new-task"]);
        // ...and nothing is reported twice.
        assert!(tail.read_new().is_empty());
    }

    /// The same guarantee when mana could not measure the file at construction
    /// time: unknown history is not empty history.
    #[test]
    fn a_tail_that_never_measured_the_file_starts_at_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        append(&path, &notification(Role::Executor, "old-task", "exit 0"));

        let mut tail = NotificationTail {
            path: path.clone(),
            offset: None,
            next_poll: Instant::now(),
        };
        assert!(tail.read_new().is_empty(), "history must not be replayed");

        append(&path, &notification(Role::Executor, "new-task", "exit 0"));
        assert_eq!(finished(&tail.read_new()), ["new-task"]);
    }

    #[test]
    fn a_missing_notifications_file_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = NotificationTail::new(tmp.path().join("absent.jsonl"));
        assert!(tail.read_new().is_empty());
    }

    /// Several dispatch threads append independently, so a read can land in
    /// the middle of one. The half-line must wait, not be parsed or skipped.
    #[test]
    fn a_half_written_line_is_left_for_the_next_poll() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = NotificationTail::new(path.clone());

        let line =
            serde_json::to_string(&notification(Role::Executor, "task-1", "exit 0")).unwrap();
        let (head, rest) = line.split_at(20);
        std::fs::write(&path, head).unwrap();
        assert!(tail.read_new().is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{rest}").unwrap();
        assert_eq!(finished(&tail.read_new()), ["task-1"]);
    }

    /// The offset has already moved past it, so a line mana cannot parse is a
    /// dispatch nobody will ever hear about again -- it is reported, raw line
    /// included, not dropped (#88).
    #[test]
    fn a_line_that_is_not_a_notification_is_reported_rather_than_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = NotificationTail::new(path.clone());

        std::fs::write(&path, "{\"half\": \"a record\", \"task_id\": \"task-9\"}\n").unwrap();
        append(&path, &notification(Role::Executor, "task-1", "exit 0"));
        let seen = tail.read_new();
        assert_eq!(finished(&seen), ["task-1"]);
        let gaps = gaps(&seen);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        // The file to look in, and the line itself -- which still names the
        // task the PM was waiting on.
        assert!(gaps[0].contains("notifications.jsonl"), "{}", gaps[0]);
        assert!(gaps[0].contains("task-9"), "{}", gaps[0]);
        assert!(gaps[0].contains("get_review"), "{}", gaps[0]);
        // One line: it goes into the chat pane as it stands.
        assert!(!gaps[0].contains('\n'), "{}", gaps[0]);
    }

    /// Truncate, rotate or restore the file mid-session and v2 replayed it
    /// from zero -- every completion this project ever had, injected as a paid
    /// PM turn about work closed weeks ago (#87). The tail resumes at the new
    /// end and says what it can no longer see instead.
    #[test]
    fn a_truncated_file_is_reported_rather_than_replayed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        let old = notification(Role::Executor, "old-task", "exit 0");
        append(&path, &old);
        append(&path, &notification(Role::Executor, "older-task", "exit 0"));
        let mut tail = NotificationTail::new(path.clone());

        // Noticed at the next read, which is the only moment mana looks: the
        // file is shorter than where the tail had got to. One historical line
        // survives the rewrite, so a replay would show up as `old-task`.
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&old).unwrap())).unwrap();
        let seen = tail.read_new();
        assert!(
            finished(&seen).is_empty(),
            "history was replayed: {:?}",
            finished(&seen)
        );
        let gaps = gaps(&seen);
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].contains("notifications.jsonl"), "{}", gaps[0]);
        assert!(gaps[0].contains("get_review"), "{}", gaps[0]);
        assert!(!gaps[0].contains('\n'), "{}", gaps[0]);

        // ...and the tail is live again from there.
        append(&path, &notification(Role::Reviewer, "task-2", "exit 0"));
        assert_eq!(finished(&tail.read_new()), ["task-2"]);
    }

    #[test]
    fn the_tail_is_polled_at_most_once_per_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notifications.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = NotificationTail::new(path.clone());
        // After the tail exists: it opens its window at construction time, so
        // an earlier instant would fall inside it and prove nothing.
        let start = Instant::now();

        append(&path, &notification(Role::Executor, "task-1", "exit 0"));
        assert_eq!(tail.poll(start).len(), 1);

        append(&path, &notification(Role::Executor, "task-2", "exit 0"));
        assert!(
            tail.poll(start).is_empty(),
            "polled again within the window"
        );
        assert_eq!(tail.poll(start + NOTIFICATION_POLL).len(), 1);
    }
}

/// The milestone-2 smoke test: the real launch pathway, driven headlessly
/// against a fake PM that is a shell script.
///
/// Everything a paid CLI would do is faked; everything mana does is real --
/// the catalogue override, the skill install, the MCP config, `pm::start`, the
/// activation turn, the notification tail and the shutdown. What it cannot
/// cover is whether a real CLI honours the flags, which is what the README's
/// manual QA is for.
///
/// Unix-only because the fake PM is a shell script, like every other process
/// test in the tree.
#[cfg(all(test, unix))]
mod smoke {
    use super::*;
    use crate::task::Role;
    use std::os::unix::fs::PermissionsExt;

    pub(super) struct Fixture {
        _tmp: tempfile::TempDir,
        pub(super) home: PathBuf,
        pub(super) project: PathBuf,
        /// Where the fake PM appends every frame mana wrote to its stdin.
        received: PathBuf,
        /// Where it dumps the argv it was started with.
        argv: PathBuf,
        skills: PathBuf,
    }

    impl Fixture {
        pub(super) fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let fixture = Fixture {
                home: tmp.path().join("mana-home"),
                project: tmp.path().join("demo"),
                received: tmp.path().join("received.jsonl"),
                argv: tmp.path().join("argv.txt"),
                skills: tmp.path().join("skills"),
                _tmp: tmp,
            };
            std::fs::create_dir_all(&fixture.project).unwrap();
            std::fs::create_dir_all(&fixture.home).unwrap();
            std::fs::create_dir_all(&fixture.skills).unwrap();
            fixture.write_override(&fixture.fake_pm());
            fixture
        }

        /// A PM that records what it is told, answers every turn once, and
        /// closes each turn with the frame its catalogue entry names -- which
        /// is what makes the session's turn tracking (and so its queue) real
        /// in these tests rather than permanently switched off.
        fn fake_pm(&self) -> String {
            self.fake_pm_body("")
        }

        /// The same, with `extra` run before the answer -- how a test makes the
        /// PM slow enough to have a turn typed into.
        fn fake_pm_body(&self, extra: &str) -> String {
            let path = self.home.join("fake-pm");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n\
                     printf '%s\\n' \"$*\" > '{argv}'\n\
                     while IFS= read -r line; do\n\
                     \x20 printf '%s\\n' \"$line\" >> '{received}'\n\
                     {extra}\
                     \x20 echo '{{\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"ack\"}}]}}}}'\n\
                     \x20 echo '{{\"type\":\"result\"}}'\n\
                     done\n",
                    argv = self.argv.display(),
                    received = self.received.display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        }

        /// A PM that takes one turn and dies without answering it: the shape
        /// that leaves a turn open for ever, and so the only one that shows
        /// what happens to a queue nobody is coming back for.
        fn write_mute_pm(&self, code: i32) {
            let path = self.home.join("mute-pm");
            std::fs::write(
                &path,
                format!("#!/bin/sh\nhead -n 1 > /dev/null\nexit {code}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            self.write_override(&path.to_string_lossy());
        }

        /// Goes through the real override path (design §7) rather than
        /// building a `Catalog` by hand, so the test exercises the code a user
        /// with a broken CLI would.
        fn write_override(&self, bin: &str) {
            self.write_override_on(bin, "mcp");
        }

        fn write_override_on(&self, bin: &str, channel: &str) {
            self.write_override_with(bin, channel, "");
        }

        /// The same, plus whatever extra `[pm]` keys the test needs -- which
        /// is how a resumable entry is built without a second fixture.
        fn write_override_with(&self, bin: &str, channel: &str, extra_pm: &str) {
            let source = super::tests::entry_source(
                bin,
                &[self.skills.to_str().unwrap()],
                &format!(r#"permission_args = ["--allowedTools", "mcp__mana__*"]{extra_pm}"#),
                channel,
            );
            std::fs::write(self.home.join(crate::catalog::CATALOG_OVERRIDE), source).unwrap();
        }

        /// A CLI that cannot read the role off disk, so the activation carries
        /// the whole of `SKILL.md` -- the longest thing mana ever sends a PM,
        /// and the one v2.0 printed into the chat pane as a user turn.
        fn write_override_inlining_the_role(&self, bin: &str) {
            let source =
                super::tests::entry_source(bin, &[self.skills.to_str().unwrap()], "", "mcp")
                    .replace("dirs = ", "inline_in_activation = true\ndirs = ");
            std::fs::write(self.home.join(crate::catalog::CATALOG_OVERRIDE), source).unwrap();
        }

        fn paths(&self) -> ProjectPaths {
            resolve_project_paths(&self.home, &project_name_from_dir(&self.project))
        }

        /// Waits for `path` to contain `needle`, which is how a test observes
        /// a child process that writes on its own schedule.
        fn wait_for(&self, path: &Path, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let contents = std::fs::read_to_string(path).unwrap_or_default();
                if contents.contains(needle) {
                    return contents;
                }
                assert!(
                    Instant::now() < deadline,
                    "{} never contained {needle:?}; it holds: {contents}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn a_launch_installs_the_skill_registers_mana_and_activates_the_pm() {
        let fixture = Fixture::new();
        let mut session = prepare_session(&fixture.home, &fixture.project, "fixture", false)
            .expect("the PM started");

        // 1. The role text is on disk where the CLI reads skills from.
        let skill = fixture.skills.join("mana-pm/SKILL.md");
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), PM_SKILL);
        assert_eq!(session.skill_path, skill);

        // 2. mana registered itself by absolute path, for this project.
        let config = fixture.paths().root.join(MCP_CONFIG);
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            document["mcpServers"]["mana"]["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            document["mcpServers"]["mana"]["args"],
            serde_json::json!([
                "mcp-server",
                "--project-root",
                fixture.project.to_string_lossy()
            ])
        );

        // 3. ...and the CLI was started with the flag that reads it, plus the
        // permission argv that keeps the PM from writing code.
        let argv = fixture.wait_for(&fixture.argv, "--mcp-config");
        assert!(argv.contains(config.to_string_lossy().as_ref()), "{argv}");
        assert!(argv.contains("--allowedTools mcp__mana__*"), "{argv}");

        // 4. The activation message reached the PM's stdin as a real frame.
        let received = fixture.wait_for(&fixture.received, ACTIVATION);
        assert_eq!(received.lines().count(), 1, "{received}");
        let frame: serde_json::Value =
            serde_json::from_str(received.lines().next().unwrap()).unwrap();
        assert_eq!(frame["type"], "user");
        // The path is the operative half: a CLI that does not discover skills
        // on its own can still read the file mana just wrote.
        assert_eq!(
            frame["message"]["content"],
            format!("{ACTIVATION}, installed at {}.", skill.display())
        );

        // ...and the PM's answer came back as chat text, not as raw noise.
        let mut app = App::new(&session.cli_name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.lines().next().is_none() && Instant::now() < deadline {
            for event in session.drain() {
                // The activation is a turn like any other, and this loop is
                // the only thing standing in for `run_loop`: an event dropped
                // here is a turn that never closes.
                if event == PmEvent::TurnEnded {
                    session.release_next();
                }
                app.apply(&event);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(app.lines().next().unwrap().text, "ack");
        // ...and nothing mana sends next goes out until that turn is closed.
        assert!(session.settle().is_empty(), "the queue lost a turn");

        // 5. A dispatch reporting in is injected as a user turn -- this is how
        // the PM learns an executor finished without polling.
        let paths = fixture.paths();
        std::fs::create_dir_all(&paths.root).unwrap();
        super::tests::append(
            &notifications_path(&paths),
            &super::tests::notification(Role::Executor, "task-1", "exit 0 in 1.2s"),
        );
        let injected = session.poll_notifications(Instant::now()).unwrap();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].contains("executor finished for task task-1"));
        // The one internal send the pane does show, and it stays one line: it
        // is news the operator is waiting for, not plumbing.
        assert!(!injected[0].contains('\n'), "{}", injected[0]);
        app.push(Source::Mana, &injected[0]);
        assert_eq!(
            app.lines()
                .filter(|line| line.source == Source::Mana)
                .count(),
            1
        );
        let received = fixture.wait_for(&fixture.received, "executor finished");
        assert_eq!(received.lines().count(), 2, "{received}");

        // 6. Shutdown reaps the child: `Exited` is only sent after the reader
        // thread has actually reaped it, so seeing it is seeing the reap.
        session.shutdown().unwrap();
        let events = session.drain();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PmEvent::Exited { .. })),
            "{events:?}"
        );
    }

    /// Everything the pane holds, as text, oldest first.
    fn rendered(app: &App) -> String {
        app.lines()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Reads events into the pane until one of them says something, the way
    /// the render loop does.
    fn pump_until_pm_speaks(session: &mut Session, app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for event in session.drain() {
                app.apply(&event);
            }
            if app.lines().any(|line| line.source == Source::Pm) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the PM never answered: {}", rendered(app));
    }

    /// The defect this pass exists for: mana's own briefing, role text and all,
    /// rendered in the chat pane as though the operator had typed it.
    ///
    /// The entry here is the worst case -- a CLI that cannot read the file, so
    /// the activation carries the whole of `SKILL.md`. The PM is told all of
    /// it; the pane gets one dim line and the PM's answer.
    #[test]
    fn the_activation_and_the_role_text_it_carries_never_reach_the_chat_pane() {
        let fixture = Fixture::new();
        fixture.write_override_inlining_the_role(&fixture.fake_pm());
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();

        // The PM really was told everything: this is a suppression at the
        // interface, not a briefing mana quietly stopped sending.
        let received = fixture.wait_for(&fixture.received, ACTIVATION);
        assert!(received.contains("mana-pm skill"), "{received}");

        let mut app = App::new(&session.cli_name);
        app.push(Source::Raw, &launch_line(&session, false));
        pump_until_pm_speaks(&mut session, &mut app);

        let shown = rendered(&app);
        assert!(!shown.contains(ACTIVATION), "{shown}");
        for paragraph in PM_SKILL.lines().filter(|line| line.len() > 40).take(5) {
            assert!(
                !shown.contains(paragraph),
                "the role text rendered: {shown}"
            );
        }
        // What the launch is worth: one line, dim, and nothing the PM said is
        // attributed to the user.
        let ours: Vec<&str> = app
            .lines()
            .filter(|line| line.source != Source::Pm)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(ours.len(), 1, "{ours:?}");
        assert!(
            ours[0].starts_with("session initialized on Fixture CLI"),
            "{ours:?}"
        );
        assert_eq!(app.raw_lines, 1);
        assert!(
            !app.lines().any(|line| line.source == Source::User),
            "mana's own message was echoed as a user turn: {shown}"
        );
        session.shutdown().unwrap();
    }

    /// A resumed launch says so in the same one line, and still sends the PM
    /// its re-entry turn.
    #[test]
    fn a_resumed_launch_is_also_one_dim_line() {
        let fixture = Fixture::new();
        fixture.write_override_with(&fixture.fake_pm(), "mcp", "\nresume_args = [\"--resumed\"]");
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", true).unwrap();
        let line = launch_line(&session, true);
        assert!(line.starts_with("session resumed on Fixture CLI"), "{line}");
        assert!(!line.contains(RESUMED), "{line}");
        session.shutdown().unwrap();
    }

    /// `mana launch -c` end to end, at the one layer where the decision is
    /// visible: the resume flag reaches the CLI's argv, and the PM is handed a
    /// re-entry line rather than the activation it has already had.
    #[test]
    fn a_resumed_launch_sends_one_re_entry_line_instead_of_the_activation() {
        let fixture = Fixture::new();
        fixture.write_override_with(&fixture.fake_pm(), "mcp", "\nresume_args = [\"--resumed\"]");

        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", true).unwrap();

        // The catalogue's resume argv reached the process.
        let argv = fixture.wait_for(&fixture.argv, "--resumed");
        assert!(argv.contains("--resumed"), "{argv}");

        // ...and the opening turn is the short one. Re-sending the activation
        // would re-teach a PM that already knows -- and on the entries that
        // inline the whole role text, replay the skill into a context that
        // still holds it.
        let received = fixture.wait_for(&fixture.received, "resumed");
        assert_eq!(received.lines().count(), 1, "{received}");
        let frame: serde_json::Value =
            serde_json::from_str(received.lines().next().unwrap()).unwrap();
        assert_eq!(frame["message"]["content"], RESUMED);
        let content = frame["message"]["content"].as_str().unwrap();
        assert!(!content.contains(ACTIVATION), "{content}");
        assert!(!content.contains(PM_SKILL), "the role text was replayed");

        // The skill file is still rewritten: it is generated output, and this
        // binary may be newer than the one that wrote it.
        assert_eq!(
            std::fs::read_to_string(fixture.skills.join("mana-pm/SKILL.md")).unwrap(),
            PM_SKILL
        );
        session.shutdown().unwrap();
    }

    /// What makes a bare `mana launch -c` possible next time.
    #[test]
    fn a_successful_launch_remembers_which_cli_this_project_used() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        assert_eq!(load_state(&paths).last_cli, None);

        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert_eq!(load_state(&paths).last_cli.as_deref(), Some("fixture"));
        // The stream driver's CLI keys its conversation by directory, so there
        // is no session id to store and mana stores none.
        assert!(load_state(&paths).sessions.is_empty());
        session.shutdown().unwrap();
    }

    /// The sentinel channel is wired from catalogue data and nowhere else: an
    /// MCP CLI reaches the same tools over its own protocol, and scanning its
    /// prose as well would be a second, unasked-for way in.
    #[test]
    fn only_a_sentinel_channel_gets_a_block_scanner() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert!(session.sentinel.is_none());
        // ...and a fenced block from an MCP PM is just text it wrote.
        let pass = session.apply_tools("```mana\n{\"tool\": \"list_agents\"}\n```");
        assert!(pass.prose.is_none());
        assert!(pass.log.is_empty());
        session.shutdown().unwrap();
    }

    /// The round trip the whole channel exists for: the PM writes a block,
    /// mana executes it, and the result reaches the PM as its next turn --
    /// with the operator seeing one compact line instead of the JSON.
    #[test]
    fn a_sentinel_pm_gets_its_tool_result_injected_as_the_next_turn() {
        let fixture = Fixture::new();
        let pm = fixture.home.join("sentinel-pm");
        // Answers the first turn with a block, and every later one with an
        // acknowledgement -- otherwise the injected result would be answered
        // with another block, for ever.
        //
        // The nonce is read out of the activation turn rather than baked into
        // the script, because that is the only way a PM can get it (#140) and
        // a test that knew it any other way would be proving something mana
        // does not offer.
        std::fs::write(
            &pm,
            format!(
                "#!/bin/sh\n\
                 turns=0\n\
                 while IFS= read -r line; do\n\
                 \x20 printf '%s\\n' \"$line\" >> '{received}'\n\
                 \x20 turns=$((turns+1))\n\
                 \x20 if [ \"$turns\" = 1 ]; then\n\
                 \x20   nonce=$(printf '%s' \"$line\" | sed -n 's/.*```mana:\\([0-9a-f][0-9a-f]*\\).*/\\1/p')\n\
                 \x20   printf '%s\\n' '{{\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"Checking what is installed.\\n```mana:'\"$nonce\"'\\n{{\\\"tool\\\": \\\"list_agents\\\"}}\\n```\"}}]}}}}'\n\
                 \x20 else\n\
                 \x20   printf '%s\\n' '{{\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"ack\"}}]}}}}'\n\
                 \x20 fi\n\
                 \x20 printf '%s\\n' '{{\"type\":\"result\"}}'\n\
                 done\n",
                received = fixture.received.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&pm, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.write_override_on(&pm.to_string_lossy(), "sentinel");

        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert!(session.sentinel.is_some());

        // A sentinel PM has no tool list to discover the channel from, so the
        // activation turn says which way to call.
        let activation = fixture.wait_for(&fixture.received, ACTIVATION);
        assert!(
            activation.contains(&format!(
                "fenced ```mana:{} block",
                session.sentinel.as_ref().unwrap().nonce()
            )),
            "{activation}"
        );

        // One turn's events, handled in the order `run_loop` handles them: the
        // PM's prose goes through the tool channel, which writes the results
        // back as the next user turn -- and that turn is *queued*, because the
        // frame closing this one has not arrived yet. The release is what
        // finally hands it over.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pass = None;
        let mut released = 0;
        while Instant::now() < deadline && (pass.is_none() || session.queued() > 0) {
            for event in session.drain() {
                match event {
                    PmEvent::Text(text) => pass = Some(session.apply_tools(&text)),
                    PmEvent::TurnEnded => {
                        released += session.release_next().is_some() as usize;
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let pass = pass.expect("the PM never answered");
        assert_eq!(released, 1, "the tool results were never released");

        // The operator sees the sentence and one line of tool activity, not
        // the block and not the JSON that came back.
        assert_eq!(pass.prose.as_deref(), Some("Checking what is installed."));
        assert_eq!(
            pass.log,
            [ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false
            }]
        );

        // ...and the PM was handed the result as a turn of its own.
        let received = fixture.wait_for(&fixture.received, "tool results");
        let last: serde_json::Value =
            serde_json::from_str(received.lines().next_back().unwrap()).unwrap();
        let injected = last["message"]["content"].as_str().unwrap();
        assert!(
            injected.contains("1. list_agents ok: {\"agents\":["),
            "{injected}"
        );
        assert!(injected.contains("fixture"), "{injected}");
        // ...which the pane never sees: an internal send carries whatever the
        // tool returned, and a page of JSON is not conversation.
        let shown = format!(
            "{}\n{}",
            pass.prose.as_deref().unwrap_or_default(),
            pass.log
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(!shown.contains("\"agents\""), "{shown}");
        assert!(!shown.contains("tool results"), "{shown}");
        session.shutdown().unwrap();
    }

    /// A PM that never closes a turn and never answers, so the whole cycle
    /// under test is the one mana drives: `apply_tools` in, injected turn out.
    /// The 449-turn runaway ran exactly here, and a stub that only ever reads
    /// reproduces it without paying for 449 sub-agents.
    fn mute_sentinel_session(fixture: &Fixture) -> Session {
        let pm = fixture.home.join("mute-pm");
        std::fs::write(&pm, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n").unwrap();
        std::fs::set_permissions(&pm, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.write_override_on(&pm.to_string_lossy(), "sentinel");
        prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap()
    }

    /// How many blocks that pass actually executed.
    fn executed(pass: &ToolPass) -> usize {
        pass.log
            .iter()
            .filter(|line| line.text.starts_with("⚙"))
            .count()
    }

    /// The test that would have caught 449 (#191).
    ///
    /// A PM stub that re-emits the same executable block on every turn, driven
    /// for far more cycles than the bound allows. What is asserted is the
    /// number of *executions*, not that some message was printed: the cost of
    /// this failure was paid in tool calls.
    #[test]
    fn a_pm_that_re_emits_the_same_block_for_ever_is_stopped_after_a_bounded_number_of_cycles() {
        let fixture = Fixture::new();
        let mut session = mute_sentinel_session(&fixture);
        let nonce = session.sentinel.as_ref().unwrap().nonce().to_string();
        let message =
            format!("Dispatching again.\n```mana:{nonce}\n{{\"tool\": \"list_agents\"}}\n```");

        let mut runs = 0;
        let mut injected = 0;
        for _ in 0..200 {
            let before = session.queued();
            runs += executed(&session.apply_tools(&message));
            injected += session.queued() - before;
        }
        assert_eq!(
            runs, MAX_TOOL_CYCLES as usize,
            "200 identical turns executed {runs} blocks"
        );
        // ...and the injected turns stopped too, which is what actually ends
        // the cycle: the bound's own message is the last thing mana writes.
        assert_eq!(injected, MAX_TOOL_CYCLES as usize + 1);

        // Recoverable: the operator says one word and the channel is live
        // again. A bound that gagged it for the rest of the session would
        // leave a PM that looks like it works and does nothing.
        session
            .send_typed("stop dispatching, what is going on?")
            .unwrap();
        assert_eq!(executed(&session.apply_tools(&message)), 1);
        session.shutdown().unwrap();
    }

    /// The bound is on the runaway shape, not on how much work a PM does: a
    /// message with three blocks runs three, and the next message runs its own.
    #[test]
    fn several_blocks_in_one_message_and_the_message_after_it_all_execute() {
        let fixture = Fixture::new();
        let mut session = mute_sentinel_session(&fixture);
        let nonce = session.sentinel.as_ref().unwrap().nonce().to_string();
        let block = format!("```mana:{nonce}\n{{\"tool\": \"list_agents\"}}\n```");
        let message = format!("Three at once.\n{block}\n{block}\n{block}");

        assert_eq!(executed(&session.apply_tools(&message)), 3);
        // The results came back, the PM read them and dispatched three more:
        // one cycle each, six executions, nothing declined.
        let second = session.apply_tools(&message);
        assert_eq!(executed(&second), 3);
        assert!(
            !second.log.iter().any(|line| line.failed),
            "{:?}",
            second.log
        );
        session.shutdown().unwrap();
    }

    /// When it fires the PM is told once, in its own turn, and the operator
    /// sees a line about it -- the whole point being a failure mode that looks
    /// busy on screen.
    #[test]
    fn the_bound_tells_the_pm_once_and_says_so_in_the_pane() {
        let fixture = Fixture::new();
        let mut session = mute_sentinel_session(&fixture);
        let nonce = session.sentinel.as_ref().unwrap().nonce().to_string();
        let message = format!("Again.\n```mana:{nonce}\n{{\"tool\": \"list_agents\"}}\n```");

        let mut pane = Vec::new();
        for _ in 0..(MAX_TOOL_CYCLES + 5) {
            let pass = session.apply_tools(&message);
            // The PM's words are still rendered, bound or no bound.
            assert_eq!(pass.prose.as_deref(), Some("Again."));
            pane.extend(pass.log.into_iter().filter(|line| line.failed));
        }

        // One line in the pane, in mana's voice, naming the bound.
        assert_eq!(pane.len(), 1, "{pane:?}");
        assert!(
            pane[0].text.starts_with("[mana] tool cycle bound reached"),
            "{}",
            pane[0].text
        );

        // ...and one turn to the PM, saying its blocks were not executed and
        // what starts the count over.
        let told: Vec<&Queued> = session
            .queue
            .iter()
            .filter(|queued| queued.text.contains("did not execute"))
            .collect();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            !told[0].typed,
            "the bound is mana talking, not the operator"
        );
        assert!(told[0].text.starts_with("[mana]"), "{}", told[0].text);
        assert!(told[0].text.contains("count resets"), "{}", told[0].text);
        session.shutdown().unwrap();
    }

    #[test]
    fn an_unknown_cli_id_lists_the_ones_the_catalogue_knows() {
        let fixture = Fixture::new();
        // `unwrap_err` would need `Debug` on a live PM session, which is not
        // worth deriving for a type nobody prints.
        let rendered = match prepare_session(&fixture.home, &fixture.project, "nosuchcli", false) {
            Ok(_) => panic!("a session started for a CLI the catalogue does not know"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("nosuchcli"), "{rendered}");
        assert!(rendered.contains("claude"), "{rendered}");
        assert!(rendered.contains("fixture"), "{rendered}");
    }

    /// The project's state directory is created by launching, not by some
    /// earlier command the user may never have run.
    #[test]
    fn launching_prepares_the_projects_state_directory() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        assert!(!paths.tasks.exists());

        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert!(paths.tasks.is_dir());
        assert!(paths.logs.is_dir());
        assert!(paths.reviews.is_dir());
        session.shutdown().unwrap();
    }

    /// Nobody touches the keyboard: what the loop faces while the user reads.
    /// It gives up after a deadline so a test can never hang on a loop that
    /// was supposed to end by itself.
    struct Idle {
        deadline: Instant,
    }

    impl EventSource for Idle {
        fn poll_event(&mut self, timeout: Duration) -> Result<Option<RawEvent>> {
            if Instant::now() > self.deadline {
                bail!("the loop never ended on its own");
            }
            std::thread::sleep(timeout);
            Ok(None)
        }
    }

    /// Waits for one closing frame and releases whatever it frees, which is
    /// what `run_loop` does with the same event.
    fn release_one(session: &mut Session) -> Option<Queued> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for event in session.drain() {
                if event == PmEvent::TurnEnded {
                    return session.release_next().map(|(queued, sent)| {
                        sent.expect("the release never reached the PM");
                        queued
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the PM never ended its turn");
    }

    /// The bug, at the level it is fixed: the PM is mid-turn -- it always is
    /// right after the activation -- and everything sent at it waits its place
    /// instead of landing in the middle of an answer.
    #[test]
    fn everything_sent_while_the_pm_is_answering_is_queued_and_released_in_order() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();

        // One FIFO for both kinds of turn: a typed question and an injected
        // notification are the same thing to the PM, and interleaving them by
        // arrival is the only order that reads as a conversation.
        assert_eq!(session.send_typed("who is free").unwrap(), Delivery::Queued);
        assert_eq!(
            session.send_internal("[mana] executor finished").unwrap(),
            Delivery::Queued
        );
        assert_eq!(session.send_typed("and then").unwrap(), Delivery::Queued);
        assert_eq!(session.queued(), 3);
        // Nothing but the activation has reached the PM yet.
        assert_eq!(
            fixture
                .wait_for(&fixture.received, ACTIVATION)
                .lines()
                .count(),
            1
        );

        // One turn ends, one turn goes -- not three.
        let released = release_one(&mut session).expect("the queue held nothing");
        assert_eq!(released.text, "who is free");
        assert!(released.typed);
        assert_eq!(session.queued(), 2);

        // The rest follow, in order, as the PM works through them.
        assert!(session.settle().is_empty(), "the queue lost a turn");
        assert_eq!(session.queued(), 0);
        let received = fixture.wait_for(&fixture.received, "and then");
        let sent: Vec<String> = received
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["message"]["content"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(sent.len(), 4, "{sent:?}");
        assert!(sent[0].starts_with(ACTIVATION), "{sent:?}");
        assert_eq!(
            &sent[1..],
            ["who is free", "[mana] executor finished", "and then"]
        );
        session.shutdown().unwrap();
    }

    /// A turn sent to an idle PM is not queued at all: the queue is a wait, not
    /// a pipeline, and a session that buffered everything would answer every
    /// question one turn late.
    #[test]
    fn a_turn_sent_to_an_idle_pm_goes_straight_out() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        session.settle();

        assert_eq!(session.send_typed("who is free").unwrap(), Delivery::Sent);
        assert_eq!(session.queued(), 0);
        fixture.wait_for(&fixture.received, "who is free");
        session.shutdown().unwrap();
    }

    /// Queued messages that will never be sent must not vanish: the operator
    /// typed them, and a queue that emptied itself quietly would leave them
    /// believing the PM had read them.
    #[test]
    fn a_pm_that_dies_hands_back_everything_it_never_read() {
        let fixture = Fixture::new();
        fixture.write_mute_pm(3);
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert_eq!(
            session.send_typed("are you there").unwrap(),
            Delivery::Queued
        );

        // No answer is coming: the turn the activation opened is closed by the
        // process ending, not by the PM finishing.
        let never_sent = session.settle();
        assert_eq!(
            never_sent,
            ["[mana] never sent -- the PM is gone: are you there"]
        );
        // ...and they come back as losses, which is what survives the screen
        // and what the exit code is counted from (#181).
        assert_eq!(session.lost, never_sent);
        assert_eq!(session.queued(), 0);
    }

    /// The same, through the loop that actually runs it: the notice lands in
    /// the pane the operator is looking at, with the words they typed in it.
    #[test]
    fn the_loop_says_out_loud_what_a_dead_pm_never_read() {
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        fixture.write_mute_pm(3);
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert_eq!(
            session.send_typed("are you there").unwrap(),
            Delivery::Queued
        );

        let mut app = App::new(&session.cli_name);
        app.push_pending(Source::User, "are you there");
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut Idle {
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap();

        assert_eq!(end, SessionEnd::PmExited { code: Some(3) });
        let lines: Vec<&str> = app.lines().map(|line| line.text.as_str()).collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("never sent") && line.contains("are you there")),
            "{lines:?}"
        );
        assert_eq!(app.queued, 0);
    }

    /// ...and that notice has to outlive the pane it was written into. The
    /// queue was the one loss channel #96's fix did not cover: it was drained
    /// into the chat pane alone, so the notice was torn down with the
    /// alternate screen and a session that swallowed the operator's turn still
    /// exited 0 (#181).
    #[test]
    fn a_message_a_dead_pm_never_read_outlives_the_screen_and_fails_the_exit() {
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        // Exit 0: a PM that failed says so with its own code, and what is under
        // test is the loss only mana knows about.
        fixture.write_mute_pm(0);
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert_eq!(
            session.send_typed("please refactor the parser").unwrap(),
            Delivery::Queued
        );

        let mut app = App::new(&session.cli_name);
        app.push_pending(Source::User, "please refactor the parser");
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut Idle {
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap();
        assert_eq!(end, SessionEnd::PmExited { code: Some(0) });

        // `lost` is the only thing `finish_session` reprints once the screen is
        // gone, so being in it *is* reaching the operator's terminal.
        assert!(
            session
                .lost
                .iter()
                .any(|line| line.contains("please refactor the parser")),
            "{:?}",
            session.lost
        );
        let error = format!(
            "{:#}",
            finish_session(
                &fixture.home,
                &mut session,
                &app,
                Ok(SessionEnd::PmExited { code: Some(0) })
            )
            .unwrap_err()
        );
        assert!(
            error.contains("1 message(s) that never reached it"),
            "{error}"
        );
    }

    /// The same loss with the other cause: the operator quit with a turn still
    /// in flight. That notice did reach stdout, but over an exit code of 0 --
    /// which is the only half of it a wrapper script can read (#181).
    #[test]
    fn quitting_with_a_turn_still_queued_is_not_a_clean_exit() {
        let fixture = Fixture::new();
        // Never answers, so the activation's turn stays open and the next turn
        // can only queue.
        fixture.write_mute_pm(3);
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        assert_eq!(
            session.send_typed("important brief").unwrap(),
            Delivery::Queued
        );

        let app = App::new(&session.cli_name);
        let error = format!(
            "{:#}",
            finish_session(&fixture.home, &mut session, &app, Ok(SessionEnd::UserQuit))
                .unwrap_err()
        );
        assert!(
            error.contains("1 message(s) that never reached the PM"),
            "{error}"
        );
        assert!(
            session
                .lost
                .iter()
                .any(|line| line.contains("important brief")),
            "{:?}",
            session.lost
        );
    }

    /// A dead PM must not take the interface down with it: the user typed
    /// that turn, and the explanation belongs where they are looking.
    #[test]
    fn a_turn_that_cannot_be_delivered_is_reported_in_the_chat_pane() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        session.shutdown().unwrap();
        // The activation's turn is closed by the exit rather than by an answer,
        // which is what leaves the transport -- not the queue -- to refuse the
        // turn typed next.
        session.settle();

        let mut app = App::new(&session.cli_name);
        app.input = "are you there".to_string();
        assert!(
            apply_app_event(AppEvent::Enter, &mut app, &mut session),
            "a failed turn ended the session"
        );

        let lines: Vec<&str> = app.lines().map(|line| line.text.as_str()).collect();
        assert_eq!(lines[0], "are you there");
        assert!(lines[1].contains("did not reach the PM"), "{:?}", lines);
    }

    /// The other half of the same failure (#96): the pane dies with the
    /// alternate screen, so a delivery failure that only ever rendered there
    /// did not survive the session that lost it. It has to reach the exit code,
    /// which is all a script wrapping `mana launch` can see -- and pressing `q`
    /// on a session that told the PM nothing used to be exit 0.
    #[test]
    fn a_lost_turn_outlives_the_session_and_takes_the_exit_code_with_it() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        session.shutdown().unwrap();
        session.settle();

        let mut app = App::new(&session.cli_name);
        app.input = "are you there".to_string();
        apply_app_event(AppEvent::Enter, &mut app, &mut session);
        assert_eq!(session.lost.len(), 1, "{:?}", session.lost);

        // `q` -- the clean way out, and still not a clean session.
        let error = format!(
            "{:#}",
            finish_session(&fixture.home, &mut session, &app, Ok(SessionEnd::UserQuit))
                .unwrap_err()
        );
        assert!(
            error.contains("1 message(s) that never reached the PM"),
            "{error}"
        );
    }

    /// ...and the ordinary session is untouched by that: #96 is about sessions
    /// that lost something, not about making `q` fail.
    #[test]
    fn a_session_that_lost_nothing_still_ends_cleanly() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let app = App::new(&session.cli_name);
        finish_session(&fixture.home, &mut session, &app, Ok(SessionEnd::UserQuit)).unwrap();
    }

    fn pending(options: Vec<crate::pm::PermissionChoice>) -> crate::tui::app::PendingPermission {
        crate::tui::app::PendingPermission {
            id: 1,
            description: "write README.md".to_string(),
            options,
        }
    }

    fn choice(id: &str, allows: bool) -> crate::pm::PermissionChoice {
        crate::pm::PermissionChoice {
            id: id.to_string(),
            label: id.to_string(),
            allows,
        }
    }

    /// One keypress deletes one *glyph*. A ZWJ sequence is several code
    /// points and every prefix of one is itself a valid emoji, so a delete
    /// that took a code point turned the operator's family into a couple, then
    /// into a man -- each step looking deliberate in the input box, and any of
    /// them sendable to the PM without the operator seeing anything wrong
    /// (#182).
    #[test]
    fn backspace_deletes_a_whole_grapheme_not_a_code_point() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);

        // Five code points, four glyphs: three ASCII and one man-woman-boy.
        app.input = "abc\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F466}".to_string();
        apply_app_event(AppEvent::Backspace, &mut app, &mut session);
        assert_eq!(app.input, "abc");

        // The same failure with no emoji in sight: `e` followed by a combining
        // acute renders as one `\u{e9}`, and half of it is not a character.
        app.input = "cafe\u{301}".to_string();
        apply_app_event(AppEvent::Backspace, &mut app, &mut session);
        assert_eq!(app.input, "caf");

        // An empty buffer is not an error, and never was.
        app.input.clear();
        apply_app_event(AppEvent::Backspace, &mut app, &mut session);
        assert!(app.input.is_empty());

        session.shutdown().unwrap();
    }

    /// The stream transport never asks for permission, so answering one has
    /// nowhere to go -- and that has to land in the pane rather than take the
    /// session down with it.
    #[test]
    fn an_answer_the_transport_cannot_deliver_is_reported_in_the_chat_pane() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);
        app.pending_permission = Some(pending(vec![choice("yes", true)]));

        assert!(
            apply_app_event(AppEvent::AnswerPermission(true), &mut app, &mut session),
            "a failed answer ended the session"
        );
        let last = app.lines().next_back().unwrap().text.clone();
        assert!(last.contains("did not reach the PM"), "{last}");
        // Cleared either way: an agent that could not be answered is not going
        // to ask again, and a prompt nothing can clear would sit there for ever.
        assert!(app.pending_permission.is_none());
        session.shutdown().unwrap();
    }

    /// An agent that offered no way to refuse. The request stays up, because
    /// the other key may still work.
    #[test]
    fn an_answer_the_pm_never_offered_leaves_the_request_pending() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);
        app.pending_permission = Some(pending(vec![choice("yes", true)]));

        apply_app_event(AppEvent::AnswerPermission(false), &mut app, &mut session);
        let last = app.lines().next_back().unwrap().text.clone();
        assert!(last.contains("no way to reject"), "{last}");
        assert!(app.pending_permission.is_some());
        session.shutdown().unwrap();
    }

    /// The keys exist all session long; pressing one when nothing was asked
    /// must be a no-op, not a line of noise in the pane being read.
    #[test]
    fn a_permission_key_with_nothing_pending_says_nothing() {
        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);

        apply_app_event(AppEvent::AnswerPermission(true), &mut app, &mut session);
        assert_eq!(app.lines().count(), 0);
        session.shutdown().unwrap();
    }

    /// A PM that dies on its own must end the loop rather than leave mana
    /// waiting on a process that is gone -- the v1 zombie, from the TUI side.
    #[test]
    fn the_loop_ends_when_the_pm_exits_and_keeps_what_it_said() {
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let dying = fixture.home.join("dying-pm");
        // It reads one line before dying, so the activation turn always lands
        // in a live pipe: a PM that exited before mana wrote to it would fail
        // the launch instead, which is a different (and correctly reported)
        // story than the one this test is about.
        std::fs::write(
            &dying,
            "#!/bin/sh\necho 'boom: no credentials found' >&2\nhead -n 1 > /dev/null\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&dying, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.write_override(&dying.to_string_lossy());

        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut events = Idle {
            deadline: Instant::now() + Duration::from_secs(10),
        };

        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut events,
            None,
        )
        .unwrap();

        assert_eq!(end, SessionEnd::PmExited { code: Some(7) });
        // The reason is kept for the message printed after the TUI is gone.
        assert_eq!(
            app.last_stderr.as_deref(),
            Some("boom: no credentials found")
        );
    }

    /// ...and it has to be the *right* line. A PM explains itself on stderr,
    /// which is what the report promises to quote, but `last_raw` took
    /// whichever pipe spoke last -- and on a real agent that is the routine
    /// frame every turn opens with, so the one line the operator is left with
    /// was the cwd and the tool list instead of the error (#189).
    #[test]
    fn the_death_report_quotes_the_stderr_line_not_a_routine_stdout_frame() {
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let dying = fixture.home.join("noisy-dying-pm");
        // The routine frame is written after the activation has been read, so
        // it is certainly the last raw line here. In production the same
        // inversion comes for free: the stdout path parses and runs two
        // JSONPath queries before it forwards a frame written earlier.
        std::fs::write(
            &dying,
            "#!/bin/sh\n\
             echo 'claude: Invalid API key - please run /login' >&2\n\
             head -n 1 > /dev/null\n\
             echo '{\"event\":\"init\",\"cwd\":\"/tmp\",\"tools\":[\"a\",\"b\"]}'\n\
             exit 9\n",
        )
        .unwrap();
        std::fs::set_permissions(&dying, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.write_override(&dying.to_string_lossy());

        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut Idle {
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap();

        assert_eq!(end, SessionEnd::PmExited { code: Some(9) });
        let reason = death_reason(&app);
        assert!(reason.contains("Invalid API key"), "{reason}");
        assert!(!reason.contains("\"event\":\"init\""), "{reason}");
        // The frame is still shown -- degraded, never silent. It is only not
        // the explanation.
        let lines: Vec<&str> = app.lines().map(|line| line.text.as_str()).collect();
        assert!(
            lines.iter().any(|line| line.contains("\"event\":\"init\"")),
            "{lines:?}"
        );
    }

    #[test]
    fn typing_a_turn_sends_it_to_the_pm_and_ctrl_c_quits() {
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        // The scripted keys arrive as fast as the loop can read them, so the
        // activation turn is closed first: this test is about what typing does,
        // not about how quickly a shell script answers.
        session.settle();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut events = FakeEventSource::new([
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            // Escape used to quit here; now it does nothing at all.
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ]);

        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut events,
            None,
        )
        .unwrap();
        assert_eq!(end, SessionEnd::UserQuit);

        let received = fixture.wait_for(&fixture.received, "\"ho\"");
        let sent: Vec<serde_json::Value> = received
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            sent[0]["message"]["content"]
                .as_str()
                .unwrap()
                .starts_with(ACTIVATION)
        );
        assert_eq!(sent[1]["message"]["content"], "ho");

        // The keys after Enter went where they should: Escape did nothing at
        // all (the session survived it), Ctrl+G opened the graph pane.
        assert_eq!(app.mode, crate::tui::app::AppMode::Graph);
        // ...and the user's own turn is in the transcript, not only the PM's.
        assert!(
            app.lines().any(|line| line.text == "ho"),
            "the typed turn was never echoed"
        );
        session.shutdown().unwrap();
    }

    /// The bug this task fixes: without bracketed paste a multi-line paste
    /// arrives as one key event per character, and each embedded newline is
    /// indistinguishable from a typed Enter -- so a five-line brief became
    /// five separate submitted turns (#160). Driving the loop with
    /// `RawEvent::Paste` proves the text lands whole in the input buffer and
    /// that none of its newlines submitted anything on their own.
    #[test]
    fn a_pasted_block_lands_in_the_input_whole_and_its_newlines_do_not_submit() {
        use crate::tui::event::RawEvent;
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        session.settle();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut events = FakeEventSource::new([
            RawEvent::Paste("line one\nline two\nline three".to_string()),
            RawEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ]);

        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut events,
            None,
        )
        .unwrap();

        assert_eq!(end, SessionEnd::UserQuit);
        // One input, all three lines, still sitting unsent in the buffer --
        // nothing was submitted by the newlines the paste carried.
        assert_eq!(app.input, "line one\nline two\nline three");
        session.shutdown().unwrap();
    }

    /// The soft update check's only visible effect: one line in the chat pane,
    /// in mana's voice, and nothing else about the session changed.
    #[test]
    fn an_available_release_shows_as_one_line_in_the_chat_pane() {
        use crate::tui::event::test_support::FakeEventSource;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let fixture = Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut app = App::new(&session.cli_name);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut events =
            FakeEventSource::new([KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)]);

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send("[mana] mana 9.9.9 available -- run `mana upgrade`".to_string())
            .unwrap();

        let end = run_loop(
            &mut terminal,
            &mut session,
            &mut app,
            &mut GraphCache::new(),
            &mut events,
            Some(rx),
        )
        .unwrap();

        assert_eq!(end, SessionEnd::UserQuit);
        let notices: Vec<_> = app
            .lines()
            .filter(|line| line.text.contains("9.9.9 available"))
            .collect();
        assert_eq!(notices.len(), 1, "expected exactly one update notice");
        session.shutdown().unwrap();
    }
}

/// Quitting mana stops the sub-agents mana started -- and only those.
///
/// Unix-only: every test here spawns a real process and checks whether it
/// survived, the same shape (and the same reason) as `cli::kill`'s own process
/// tests.
#[cfg(all(test, unix))]
mod teardown_tests {
    use super::*;
    use crate::lock::{SubagentRecord, append_record};
    use crate::log::now_iso8601;
    use crate::status::Liveness;
    use crate::task::Role;
    use std::process::{Child, Command, Stdio};

    /// A stand-in for a dispatched sub-agent, killed on drop so a failing
    /// assertion cannot leak a sleeper into the test runner's session.
    ///
    /// `own_group` mirrors what `crate::spawn` does for every real sub-agent.
    /// A child spawned without it stays in the runner's process group, which is
    /// exactly the shape `status::guard` refuses -- the recycled-pid case.
    struct Sleeper(Child);

    impl Sleeper {
        fn new(own_group: bool) -> Sleeper {
            let mut command = Command::new("sleep");
            command
                .arg("30")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if own_group {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            Sleeper(command.spawn().unwrap())
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        /// Whether the process is gone, reaping it on the way.
        ///
        /// The reap is not tidiness: these children belong to the test runner,
        /// which never waits on them, so a killed one lingers as a zombie --
        /// and a zombie still answers `kill(pid, 0)`, exactly as `crate::status`
        /// documents. Asking the handle is the only way to see the death.
        fn died(&mut self) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn still_running(&mut self) -> bool {
            matches!(self.0.try_wait(), Ok(None))
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn seed(home: &Path, project: &str, agent_id: &str, pid: u32) {
        append_record(
            &resolve_project_paths(home, project).subagents_file,
            &SubagentRecord {
                agent_id: agent_id.to_string(),
                cli: "fixture".into(),
                model: "cheapo".into(),
                role: Role::Executor,
                task_id: "3f2a1b6c-9d4e-4a7b-8c1d-2e5f0a9b8c7d".into(),
                pid: Some(pid),
                started_at: now_iso8601(),
            },
        )
        .unwrap();
    }

    fn status_of(home: &Path, project: &str, agent_id: &str) -> DispatchStatus {
        status::dispatches_in(home, project)
            .unwrap()
            .into_iter()
            .find(|dispatch| dispatch.record.agent_id == agent_id)
            .expect("the dispatch was seeded")
            .status
    }

    /// The point of the whole feature: a PM session that ends takes its
    /// in-flight sub-agents with it, through the same machinery `mana kill`
    /// uses -- so the PM is notified, `mana ps` stops calling it running, and
    /// the operator is told in one line.
    #[test]
    fn quitting_kills_this_projects_running_agents_and_records_them() {
        let tmp = tempfile::tempdir().unwrap();
        let mut agent = Sleeper::new(true);
        seed(tmp.path(), "demo", "agent-live", agent.pid());

        let sweep = sweep_in_flight(tmp.path(), "demo", Utc::now());
        let lines = &sweep.lines;

        assert!(
            agent.died(),
            "the sub-agent survived the end of the session"
        );
        // Everything mana started was stopped, so quitting stays exit 0 (#96).
        assert!(sweep.clean);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("killed 1 in-flight agent(s)"),
            "{lines:?}"
        );
        assert!(lines[0].contains("agent-li"), "{lines:?}");

        // The two records a completion always leaves, written by the same
        // function `mana kill` writes them with.
        assert_eq!(
            status_of(tmp.path(), "demo", "agent-live"),
            DispatchStatus::Done
        );
        let paths = resolve_project_paths(tmp.path(), "demo");
        let notifications = std::fs::read_to_string(notifications_path(&paths)).unwrap();
        assert!(notifications.contains("agent-live"), "{notifications}");
    }

    /// A loop that ended badly owes its sub-agents exactly what a clean quit
    /// does. The restore used to sit above this on a `?` chain, so a terminal
    /// that refused to leave raw mode dropped the outcome, skipped the
    /// shutdown and left the sweep unrun (#75).
    #[test]
    fn a_session_that_ended_in_error_still_sweeps_and_reports_why() {
        let fixture = super::smoke::Fixture::new();
        let mut session =
            prepare_session(&fixture.home, &fixture.project, "fixture", false).unwrap();
        let mut agent = Sleeper::new(true);
        seed(&fixture.home, &session.project, "agent-live", agent.pid());
        let app = App::new(&session.cli_name);

        let error = finish_session(
            &fixture.home,
            &mut session,
            &app,
            Err(anyhow::anyhow!("the loop blew up")),
        )
        .unwrap_err();

        // The loop's own error reached the caller rather than a teardown one.
        assert!(error.to_string().contains("the loop blew up"), "{error:#}");
        assert!(agent.died(), "the sub-agent survived a failed session");
        assert_eq!(
            status_of(&fixture.home, &session.project, "agent-live"),
            DispatchStatus::Done
        );
    }

    /// The guard is exactly as binding at teardown as it is at the command
    /// line: this pid is somebody else's process, so mana signals nothing,
    /// records nothing, and says which agent it walked away from and why.
    #[test]
    fn a_pid_the_guard_refuses_is_left_alone_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        // Alive, but not the leader of its own group -- what a recycled pid
        // looks like from the outside.
        let mut bystander = Sleeper::new(false);
        seed(tmp.path(), "demo", "agent-recycled", bystander.pid());

        let sweep = sweep_in_flight(tmp.path(), "demo", Utc::now());
        let lines = &sweep.lines;

        assert!(bystander.still_running(), "a bystander was signalled");
        // A process mana started is still running and the operator now owns it:
        // not a clean end, whatever the pane said (#96).
        assert!(!sweep.clean);
        assert_eq!(status::probe(bystander.pid()), Liveness::Alive);
        assert_eq!(
            status_of(tmp.path(), "demo", "agent-recycled"),
            DispatchStatus::Running,
            "a refused kill still marked the dispatch finished"
        );
        assert!(
            lines[0].contains("left 1 in-flight agent(s) alone (pid guard refused)"),
            "{lines:?}"
        );
        // The reason, in full: the operator now owns a process mana would not
        // touch, and a count alone is not something anybody can act on.
        assert!(lines[1].contains("agent-re"), "{lines:?}");
        assert!(lines[1].contains("process group"), "{lines:?}");
        let paths = resolve_project_paths(tmp.path(), "demo");
        assert!(!notifications_path(&paths).exists());
    }

    /// Another mana, in another directory, has its own agents and its own
    /// session. Quitting this one says nothing about them.
    #[test]
    fn agents_of_another_project_are_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mine = Sleeper::new(true);
        let mut theirs = Sleeper::new(true);
        seed(tmp.path(), "demo", "agent-mine", mine.pid());
        seed(tmp.path(), "other", "agent-theirs", theirs.pid());

        sweep_in_flight(tmp.path(), "demo", Utc::now());

        assert!(mine.died());
        assert!(theirs.still_running(), "another project's agent was killed");
        assert_eq!(status::probe(theirs.pid()), Liveness::Alive);
        assert_eq!(
            status_of(tmp.path(), "other", "agent-theirs"),
            DispatchStatus::Running
        );
    }

    /// The ordinary case -- nothing was running when the user quit -- prints
    /// nothing at all. A session that dispatched nothing should end in silence.
    #[test]
    fn a_session_with_nothing_in_flight_says_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            sweep_in_flight(tmp.path(), "demo", Utc::now())
                .lines
                .is_empty()
        );

        // ...and neither does one whose dispatches have all finished: a
        // `done` dispatch is not swept, and nothing is recorded twice.
        let mut finished = Sleeper::new(true);
        let pid = finished.pid();
        let _ = finished.0.kill();
        let _ = finished.0.wait();
        seed(tmp.path(), "demo", "agent-gone", pid);
        crate::log::append_log(
            &resolve_project_paths(tmp.path(), "demo")
                .logs
                .join("agent-gone.jsonl"),
            &crate::log::ExitEntry {
                status: crate::log::Status::Done,
                action: "exited".into(),
                timestamp: now_iso8601(),
                exit_code: Some(0),
                duration_ms: Some(10),
                failure_means: None,
            },
        )
        .unwrap();
        let sweep = sweep_in_flight(tmp.path(), "demo", Utc::now());
        assert!(sweep.lines.is_empty());
        assert!(sweep.clean);
    }

    /// The sweep processes every dispatch, including continuing past a refused
    /// one to kill the next: a recycled pid does not stop the teardown.
    #[test]
    fn the_sweep_continues_past_a_refused_dispatch_to_process_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        // A live process that mana never spawned: pid guard will refuse it.
        let mut bystander = Sleeper::new(false);
        // A live process that mana did spawn: should be killed.
        let mut actual_agent = Sleeper::new(true);
        seed(tmp.path(), "demo", "agent-refused", bystander.pid());
        seed(tmp.path(), "demo", "agent-killed", actual_agent.pid());

        let sweep = sweep_in_flight(tmp.path(), "demo", Utc::now());

        // The bystander was not touched.
        assert!(
            bystander.still_running(),
            "bystander was incorrectly signalled"
        );
        // But the real agent was killed even though a previous dispatch was refused.
        assert!(
            actual_agent.died(),
            "real agent survived despite sweep continuing"
        );
        // The sweep reports both: one refused, one killed.
        assert!(!sweep.clean);
        let lines_str = sweep.lines.join("\n");
        assert!(lines_str.contains("killed 1"), "{lines_str}");
        assert!(lines_str.contains("left 1 in-flight"), "{lines_str}");
        // The refused dispatch is still running, the killed one is done.
        assert_eq!(
            status_of(tmp.path(), "demo", "agent-refused"),
            DispatchStatus::Running,
            "refused dispatch should not be marked finished"
        );
        assert_eq!(
            status_of(tmp.path(), "demo", "agent-killed"),
            DispatchStatus::Done
        );
    }

    /// The PM skill must describe what a worktree is branched from at relaunch
    /// time. Issue #173: the old text claimed it started from the same point as
    /// the original dispatch, which misled PMs to create new tasks instead of
    /// relaunching when a dependency had since merged. Mirrors src/worktree.rs
    /// line 117: `git branch --force` rebuilds the branch from HEAD at relaunch.
    #[test]
    fn pm_skill_says_relaunch_rebuilds_from_current_head() {
        // The skill must not claim "same starting point" -- that was the bug.
        assert!(
            !PM_SKILL.contains("same starting point"),
            "PM skill claims relaunch uses the same starting point, which is wrong"
        );
        // The corrected skill must say the worktree is rebuilt from current HEAD.
        // This assertion is deliberately loose to tolerate rewording: split by
        // blank lines, find the paragraph that mentions relaunching, and check it
        // mentions HEAD. A test pinned to exact prose pays for every doc edit.
        let relaunch_para = PM_SKILL
            .split("\n\n")
            .find(|para| para.contains("relaunch"))
            .expect("PM skill must have a paragraph about relaunching");
        assert!(
            relaunch_para.contains("HEAD"),
            "relaunch paragraph in PM skill does not mention HEAD"
        );
    }

    /// Issue #172: the skill must explain that relaunching preserves depends_on
    /// while creating a replacement task orphans dependents.
    #[test]
    fn pm_skill_describes_the_dependency_implications_of_relaunch_vs_replacement() {
        // Extract the Verdicts section from "## Verdicts" to "## Landing the work".
        let verdicts_start = PM_SKILL
            .find("## Verdicts")
            .expect("PM skill has no Verdicts section");
        let landing_start = PM_SKILL[verdicts_start..]
            .find("## Landing the work")
            .map(|pos| verdicts_start + pos)
            .unwrap_or_else(|| PM_SKILL.len());
        let verdicts_section = &PM_SKILL[verdicts_start..landing_start];

        // The skill must explain that relaunching preserves depends_on.
        assert!(
            verdicts_section.contains("depends_on"),
            "PM skill verdicts section does not mention depends_on"
        );
        // Relaunching vs replacement are the two routes.
        assert!(
            verdicts_section.contains("Relaunching"),
            "PM skill verdicts section does not explain relaunching"
        );
        assert!(
            verdicts_section.contains("replacement"),
            "PM skill verdicts section does not explain replacement"
        );
    }
}
