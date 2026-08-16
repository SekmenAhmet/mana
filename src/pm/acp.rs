//! The `acp` driver: mana as an Agent Client Protocol *client*, speaking
//! JSON-RPC 2.0 over the CLI's stdio.
//!
//! Five methods carry a whole PM session -- `initialize`, `session/new`,
//! `session/prompt`, the `session/update` notification, and the agent's
//! `session/request_permission` request coming the other way. Everything a
//! non-ACP driver has to guess at (what is prose, what is a tool call, when a
//! turn ended, how a permission is asked for) arrives typed, which is why the
//! design puts ACP first in the driver table and forbids `[pm.events]` here.
//!
//! ## Why this is hand-rolled rather than built on the SDK
//!
//! Two crates were evaluated on 2026-08-15 against this tree, and both were
//! rejected for measured reasons rather than taste.
//!
//! `agent-client-protocol` 2.0.0 (Zed's SDK, Apache-2.0, MSRV 1.88 -- fine
//! against our 1.97 toolchain) is the full stack: JSON-RPC actors, connection
//! builders, typed roles. It pulls **106 crates**, and they are the *smol*
//! family -- `async-io`, `async-process`, `blocking`, `futures-concurrency`.
//! mana is synchronous; the one runtime it contains is a current-thread tokio
//! built inside `mana mcp-server` and nowhere else (see `src/mcp.rs`). Taking
//! this crate means a second async ecosystem in the binary, for five methods,
//! plus its own executor thread to contain it.
//!
//! `agent-client-protocol-schema` 1.5.0 -- the protocol *types* alone, no
//! async, 47 crates -- looked like the right middle ground and is what the
//! structs below would otherwise be. It was rejected on a measured
//! side-effect: it depends on `serde_json` with the `preserve_order` feature,
//! and Cargo unifies features across the whole graph. Adding it swaps
//! `serde_json::Map` from a `BTreeMap` to an `IndexMap` **for all of mana**.
//! Measured in a scratch crate: the same `Value` round-trips as
//! `{"alpha":2,"mid":3,"zebra":1}` without it and `{"zebra":1,"alpha":2,"mid":3}`
//! with it. That silently rewrites the bytes of every JSON mana produces (the
//! MCP tool results, the config it writes for the PM's CLI) and the documented
//! sorted order of `tui::app::summarize_usage`. An ACP dependency has no
//! business changing that.
//!
//! What is left to hand-roll is small: JSON-RPC framing is one line of JSON per
//! message, and mana speaks five methods. The frames below are named after the
//! spec's own types so they stay greppable against it, and every one of them
//! was verified against two real agents (see the goldens).
//!
//! ## Measured against real agents (2026-08-15, this machine)
//!
//! - `opencode acp` -- full loop: `initialize` (protocolVersion 1) →
//!   `session/new` **with `mcpServers`, honoured**: opencode spawned mana's
//!   MCP server and the agent called `mana_list_agents` for real →
//!   `session/prompt` → 39 thought chunks, 1 tool call, 9 message chunks,
//!   `usage_update` → `stopReason: end_turn` carrying a `usage` object. Clean
//!   exit (code 0) on stdin close.
//! - `copilot --acp` -- same handshake, but `session/new`'s `mcpServers` is
//!   **rejected**: its log says `Rejecting non-http/sse MCP server "mana" from
//!   client`. Its catalogue entry therefore attaches mana over argv instead
//!   (`--additional-mcp-config @<file>`, verified to connect), which needs no
//!   code here: those flags arrive in `extra_args` like any other.
//!
//! Both agents stream prose as *many small chunks* -- opencode sent "MAN",
//! "A", "-", "TO", "OLS" as five notifications -- so this driver coalesces
//! chunks into whole lines before emitting them. One `PmEvent::Text` per chunk
//! would put one character per line in the chat pane.

use super::child::{
    CLOSE_GRACE, DRAIN_GRACE, POLL_INTERVAL, join_or_detach, kill_group, pump_stderr, read_lines,
    reap, set_process_group,
};
use super::{PermissionChoice, PmEvent, PmTransport, Resume};
use crate::catalog::{CliEntry, PmDriver, ToolChannel, substitute};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The protocol revision mana speaks. Version 1 is the stable one; both agents
/// measured on 2026-08-15 answered `initialize` with exactly this.
const PROTOCOL_VERSION: u16 = 1;

/// How long the handshake may take before mana gives up.
///
/// Generous because `session/new` is where an agent starts its own machinery
/// -- opencode boots a local server and connects every MCP server named in the
/// request, which took ~1.5 s warm and can take much longer on a cold install.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(90);

/// The name mana's MCP server is registered under, matching the config file
/// the launch flow writes for the argv-based CLIs. The PM sees its tools
/// prefixed with it, and the PM skill and `[pm].permission_args` both say
/// `mana`, so it is one word in three places rather than three.
const MCP_SERVER_NAME: &str = "mana";

/// A live ACP session.
///
/// Ownership follows the stream driver's split for the same reasons: the
/// caller drives turns synchronously, a reader thread owns stdout because a PM
/// is silent for minutes at a time, and the `Child` is shared because the
/// reader and `shutdown` both have to reap it.
///
/// stdin is behind a mutex here where the stream driver holds it outright: the
/// reader thread has to answer requests mana does not serve, and an agent left
/// waiting on a reply it will never get is an agent that hangs.
pub struct AcpDriver {
    child: Arc<Mutex<Child>>,
    /// Captured at spawn: after the child is reaped the pid is meaningless,
    /// and `kill_group` must never signal a recycled one.
    pid: u32,
    stdin: SharedStdin,
    session_id: String,
    /// JSON-RPC request ids mana hands out. Starts after the handshake's.
    next_request: u64,
    /// Permission requests the operator has not answered yet, by the id the
    /// TUI was given. The agent's own JSON-RPC id may be a number or a string
    /// and nothing above this module should have to care, so the mapping stops
    /// here.
    pending: PendingPermissions,
    events: Receiver<PmEvent>,
    /// Taken by `shutdown`, which joins it so `Exited` is queued before it
    /// returns.
    reader: Option<JoinHandle<()>>,
    close_grace: Duration,
}

/// stdin, shared with the reader thread and dropped by `shutdown`.
type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;

/// mana's permission id → the agent's JSON-RPC request id.
type PendingPermissions = Arc<Mutex<HashMap<u64, Value>>>;

impl AcpDriver {
    /// Starts the PM described by `entry` and completes the ACP handshake,
    /// appending `extra_args` (the tool channel's already-substituted flags)
    /// after the catalogue's own.
    ///
    /// Runs in mana's working directory, which is the project the user invoked
    /// mana from -- the same assumption the stream driver makes, and the same
    /// directory the launch flow calls the project root. It is what
    /// `session/new` is told the session's `cwd` is, and what mana's own MCP
    /// server is pointed at.
    pub fn start(
        entry: &CliEntry,
        extra_args: &[String],
        resume: Option<Resume<'_>>,
    ) -> Result<Self> {
        let cwd = std::env::current_dir()
            .context("resolving the working directory to start the PM session in")?;
        Self::start_in(entry, extra_args, &cwd, resume)
    }

    /// `start` with an explicit project directory, for callers (and tests)
    /// that resolve the project elsewhere than the current directory.
    pub fn start_in(
        entry: &CliEntry,
        extra_args: &[String],
        project: &Path,
        resume: Option<Resume<'_>>,
    ) -> Result<Self> {
        let id = &entry.cli.id;
        // An internal guard: the factory in `pm::start` dispatches on this
        // field, so reaching here with anything else is a code bug, not a
        // catalogue one.
        if entry.pm.driver != PmDriver::Acp {
            bail!(
                "{id}: the acp driver was handed a {:?} entry",
                entry.pm.driver
            );
        }
        // Nothing to substitute: an ACP session carries its prompt, its cwd and
        // its MCP servers inside the protocol, so `[pm].args` is only ever the
        // subcommand or flag that puts the CLI into ACP mode. Running the
        // template through an empty substitution is how a leftover
        // `{placeholder}` becomes a startup error rather than a literal brace
        // handed to the CLI.
        let args = substitute(&entry.pm.args, &HashMap::new())
            .with_context(|| format!("{id}: [pm].args"))?;

        // Resolved rather than spawned by name: see `CliMeta::resolve`.
        let program = entry
            .cli
            .resolve()
            .with_context(|| format!("failed to start {} as PM", entry.cli.name))?;
        let mut command = Command::new(&program);
        command
            .args(&args)
            .args(extra_args)
            .current_dir(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        set_process_group(&mut command);

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start {} as PM ({})",
                entry.cli.name,
                program.display()
            )
        })?;
        let pid = child.id();
        let stdin: SharedStdin = Arc::new(Mutex::new(Some(
            child.stdin.take().expect("stdin was piped"),
        )));
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (sender, events) = channel();
        let stderr_reader = pump_stderr(stderr, sender.clone());

        let pending: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
        let mut state = TurnState::new(Arc::clone(&pending));
        let mut handshake = Handshake {
            reader: BufReader::new(stdout),
            stdin: &stdin,
            sender: &sender,
            state: &mut state,
            deadline: Instant::now() + HANDSHAKE_TIMEOUT,
            cli: id,
        };

        // Anything that fails from here on leaves a spawned child behind, so
        // every early return goes through `kill` first: a half-initialised
        // agent holding a quota slot is exactly the v1 zombie.
        let outcome = (|| -> Result<(String, BufReader<ChildStdout>)> {
            let initialized = handshake.call(1, "initialize", initialize_params())?;
            let servers = mcp_servers(entry, project)?;
            // Resuming asks the agent to reopen a session it stored, so the
            // method is `session/load` rather than `session/new` -- and it is
            // only sent when the agent said it serves it. `loadSession` is the
            // ACP capability for exactly this, advertised by both agents
            // measured on 2026-08-15 (opencode 1.14.30, copilot 1.0.80), and a
            // CLI that does not advertise it is refused rather than started
            // fresh under a flag that promised otherwise.
            let session_id = match resume {
                Some(resume) => {
                    let wanted = resume_session_id(entry, &initialized, resume)?.to_string();
                    handshake.call(
                        2,
                        "session/load",
                        load_session_params(&wanted, project, servers),
                    )?;
                    // The id mana asked for, not one read back: opencode
                    // echoes `sessionId` in the reply and the protocol does not
                    // require it, so trusting the request is both simpler and
                    // more portable.
                    wanted
                }
                None => {
                    let session =
                        handshake.call(2, "session/new", new_session_params(project, servers))?;
                    session
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            anyhow!("{id}: session/new answered without a sessionId: {session}")
                        })?
                        .to_string()
                }
            };
            Ok((session_id, handshake.reader))
        })();
        let (session_id, reader) = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                kill_group(&mut child, pid);
                let _ = child.wait();
                return Err(error);
            }
        };

        let child = Arc::new(Mutex::new(child));
        let reaped = Arc::clone(&child);
        let reader_stdin = Arc::clone(&stdin);
        let reader = std::thread::spawn(move || {
            read_lines(reader, |line| {
                let decoded = decode(&line, &mut state);
                if let Some(reply) = decoded.reply {
                    write_line(&reader_stdin, &reply);
                }
                decoded
                    .events
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

        Ok(AcpDriver {
            child,
            pid,
            stdin,
            session_id,
            // 1 and 2 went to the handshake.
            next_request: 3,
            pending,
            events,
            reader: Some(reader),
            close_grace: CLOSE_GRACE,
        })
    }

    /// Sends one user turn as `session/prompt`.
    ///
    /// Fire and forget: the response arrives on the reader thread, minutes
    /// later, and is what ends the turn (flushing whatever prose was still
    /// buffered and reporting the agent's token usage). Waiting for it here
    /// would freeze the interface for the whole turn.
    pub fn send_user(&mut self, text: &str) -> Result<()> {
        let id = self.next_request;
        self.next_request += 1;
        let frame = request(
            id,
            "session/prompt",
            json!({
                "sessionId": self.session_id,
                "prompt": [{"type": "text", "text": text}],
            }),
        );
        self.write(&frame)
            // Almost always EPIPE: the PM died between turns. The `Exited`
            // event says so too, but the caller who typed this deserves an
            // answer now rather than on the next poll.
            .context("writing a user turn to the PM (has it exited?)")
    }

    /// Answers a `PmEvent::PermissionRequest` the operator decided on.
    ///
    /// The agent is blocked on this reply, so an unanswered request costs a
    /// stalled turn -- never a stalled mana: nothing in the read path waits for
    /// it, and shutdown abandons whatever is still pending.
    pub fn answer_permission(&mut self, id: u64, option_id: &str) -> Result<()> {
        let request_id =
            self.pending.lock().unwrap().remove(&id).ok_or_else(|| {
                anyhow!("permission request {id} was already answered or cancelled")
            })?;
        let frame = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"outcome": {"outcome": "selected", "optionId": option_id}},
        }))
        .expect("a reply built from plain JSON values");
        self.write(&frame)
            .context("answering the PM's permission request")
    }

    /// The session's event stream. Borrowed rather than moved so the driver
    /// stays the single owner of the process and its channel.
    pub fn events(&self) -> &Receiver<PmEvent> {
        &self.events
    }

    /// Ends the session: close stdin, wait, kill what is left.
    ///
    /// Both agents measured on 2026-08-15 exit cleanly when stdin closes, so
    /// the polite path is the normal one and the kill is the guarantee.
    pub fn shutdown(&mut self) -> Result<()> {
        *self.stdin.lock().unwrap() = None;
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

    /// How long `shutdown` waits before killing. Tests shorten it.
    pub fn set_close_grace(&mut self, grace: Duration) {
        self.close_grace = grace;
    }

    fn write(&self, frame: &str) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard.as_mut().ok_or_else(|| {
            anyhow!("the PM session is closed: its stdin was shut down, so nothing can be sent")
        })?;
        stdin.write_all(frame.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
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

impl Drop for AcpDriver {
    fn drop(&mut self) {
        // A PM that outlives mana is v1's zombie: it holds a quota slot, keeps
        // whatever it spawned (its MCP server, for one) alive, and answers to
        // nobody.
        let _ = self.shutdown();
    }
}

impl PmTransport for AcpDriver {
    fn send_user(&mut self, text: &str) -> Result<()> {
        AcpDriver::send_user(self, text)
    }

    fn events(&self) -> &Receiver<PmEvent> {
        AcpDriver::events(self)
    }

    fn answer_permission(&mut self, id: u64, option_id: &str) -> Result<()> {
        AcpDriver::answer_permission(self, id, option_id)
    }

    fn session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    fn shutdown(&mut self) -> Result<()> {
        AcpDriver::shutdown(self)
    }
}

// --- the handshake -----------------------------------------------------

/// The two synchronous calls that open a session.
///
/// Synchronous because there is nothing to show the user until they succeed,
/// and because their failures are the ones worth reporting properly: an agent
/// that needs `opencode auth login` says so in the `session/new` error, and
/// that message is more useful than "the PM went quiet".
struct Handshake<'a> {
    reader: BufReader<ChildStdout>,
    stdin: &'a SharedStdin,
    sender: &'a Sender<PmEvent>,
    state: &'a mut TurnState,
    deadline: Instant,
    cli: &'a str,
}

impl Handshake<'_> {
    /// Sends one request and reads until its response, forwarding everything
    /// seen on the way.
    ///
    /// The forwarding is not defensive coding: copilot emits a
    /// `config_option_update` notification *before* it answers `session/new`,
    /// so a handshake that only looked for its own id would drop it.
    fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        write_line_or_err(self.stdin, &request(id, method, params))
            .with_context(|| format!("{}: sending {method}", self.cli))?;

        let mut line = String::new();
        loop {
            if Instant::now() > self.deadline {
                bail!(
                    "{}: no answer to {method} within {} s -- the CLI started but never spoke ACP",
                    self.cli,
                    HANDSHAKE_TIMEOUT.as_secs()
                );
            }
            line.clear();
            let read = std::io::BufRead::read_line(&mut self.reader, &mut line)
                .with_context(|| format!("{}: reading the answer to {method}", self.cli))?;
            if read == 0 {
                bail!(
                    "{}: the CLI exited during {method} without answering -- see the lines above \
                     for what it printed",
                    self.cli
                );
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if let Some(response) = self.route(trimmed, id)? {
                return Ok(response);
            }
        }
    }

    /// Forwards one line, returning the result payload when it is the response
    /// being waited for.
    fn route(&mut self, line: &str, id: u64) -> Result<Option<Value>> {
        if let Ok(frame) = serde_json::from_str::<Value>(line)
            && frame.get("id").and_then(Value::as_u64) == Some(id)
            && frame.get("method").is_none()
        {
            if let Some(error) = frame.get("error") {
                bail!("{}: {}", self.cli, describe_error(error));
            }
            return Ok(Some(frame.get("result").cloned().unwrap_or(Value::Null)));
        }
        let decoded = decode(line, self.state);
        if let Some(reply) = decoded.reply {
            write_line(self.stdin, &reply);
        }
        for event in decoded.events {
            let _ = self.sender.send(event);
        }
        Ok(None)
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        // mana advertises nothing: its file system and terminal belong to the
        // sub-agents, which run in their own worktrees under mana's own
        // observation. A PM that could read and write through the client would
        // be a PM writing code, which is the one thing design §6 forbids.
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
        },
        "clientInfo": {"name": "mana", "version": env!("CARGO_PKG_VERSION")},
    })
}

fn new_session_params(cwd: &Path, mcp_servers: Vec<Value>) -> Value {
    json!({
        "cwd": cwd.to_string_lossy(),
        "mcpServers": mcp_servers,
    })
}

/// `session/load` carries everything `session/new` does, plus the id of the
/// conversation to reopen. The MCP servers are sent again on purpose: a loaded
/// session gets a *new* agent process, and mana's tools have to be attached to
/// it exactly as they were to the first one.
fn load_session_params(session_id: &str, cwd: &Path, mcp_servers: Vec<Value>) -> Value {
    json!({
        "sessionId": session_id,
        "cwd": cwd.to_string_lossy(),
        "mcpServers": mcp_servers,
    })
}

/// The session `--continue` should reopen, or a refusal explaining which half
/// is missing.
///
/// Two things have to be true, and they fail differently: the agent must serve
/// `session/load` (its own `initialize` answer says so, and mana never assumes
/// it), and mana must have recorded a session id for this project. Neither is
/// downgraded to "start a fresh one instead": a PM that silently forgot the
/// conversation is a PM the operator re-briefs by hand after ten minutes of
/// wondering.
fn resume_session_id<'a>(
    entry: &CliEntry,
    initialized: &Value,
    resume: Resume<'a>,
) -> Result<&'a str> {
    if !loads_sessions(initialized) {
        bail!(
            "{} cannot resume a conversation: its ACP handshake does not advertise \
             `loadSession`, so mana has no way to ask it for the previous session. Launch it \
             without --continue to start a fresh one.",
            entry.cli.name
        );
    }
    resume.session_id.ok_or_else(|| {
        anyhow!(
            "mana has no recorded {} session for this project, so there is nothing to resume. \
             Launch it once without --continue.",
            entry.cli.name
        )
    })
}

/// Whether the agent said it serves `session/load`.
///
/// `agentCapabilities.loadSession` is the field the ACP spec puts it in, and
/// both agents measured on 2026-08-15 answer `true` there. Absent reads as
/// "no": a capability nobody advertised is one mana must not exercise.
fn loads_sessions(initialized: &Value) -> bool {
    initialized["agentCapabilities"]["loadSession"]
        .as_bool()
        .unwrap_or(false)
}

/// The `mcpServers` entry that hands the PM mana's four orchestration tools.
///
/// This is the ACP-native equivalent of the config file the argv-based CLIs
/// are pointed at, and it carries the same two facts: mana's own absolute path
/// (`current_exe`, so `mana` does not have to be on `$PATH` -- one of v1's
/// three launch blockers) and which project the tools act on.
///
/// Empty for a `sentinel` entry: injecting an MCP server into a CLI whose
/// catalogue says it has no MCP surface would be the code deciding something
/// the catalogue already decided.
fn mcp_servers(entry: &CliEntry, project: &Path) -> Result<Vec<Value>> {
    if entry.tools.channel != ToolChannel::Mcp {
        return Ok(Vec::new());
    }
    let exe: PathBuf = std::env::current_exe().context(
        "resolving mana's own path, which is what the PM's CLI will run to reach mana's tools",
    )?;
    Ok(vec![json!({
        "name": MCP_SERVER_NAME,
        "command": exe.to_string_lossy(),
        "args": ["mcp-server", "--project-root", project.to_string_lossy()],
        "env": [],
    })])
}

// --- decoding ----------------------------------------------------------

/// What one line of the agent's stdout produced.
#[derive(Debug, Default, PartialEq)]
struct Decoded {
    events: Vec<PmEvent>,
    /// A JSON-RPC reply mana owes the agent, ready to write. Only ever a
    /// "method not found" for a request mana does not serve: JSON-RPC has no
    /// timeout, so an agent that asked something and got nothing waits for
    /// ever.
    reply: Option<String>,
}

impl Decoded {
    fn none() -> Self {
        Decoded::default()
    }

    fn events(events: Vec<PmEvent>) -> Self {
        Decoded {
            events,
            reply: None,
        }
    }

    /// The visible-degradation fallback: a frame mana did not understand is
    /// shown whole rather than dropped (design §4).
    fn raw(line: &str) -> Self {
        Decoded::events(vec![PmEvent::Raw(line.to_string())])
    }
}

/// Which stream a run of chunks belongs to. One buffer with a tag rather than
/// two buffers, so prose and reasoning can never be interleaved into each
/// other: a change of tag flushes what came before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// What the PM is saying. The chat pane.
    Message,
    /// What the PM is thinking. Kept out of `Text` on purpose (design §4: only
    /// what an operator must read is prose) but shown dimmed rather than
    /// dropped -- during opencode's 8-second turn, 39 thought chunks were the
    /// only sign the session was alive.
    Thought,
}

/// Everything the decoder carries between lines.
struct TurnState {
    kind: Option<Chunk>,
    buffer: String,
    pending: PendingPermissions,
    next_permission: u64,
    /// The human name of every tool call still in flight, by `toolCallId`.
    ///
    /// ACP names a call in its `tool_call` frame and is free to leave the
    /// field out of the updates that follow -- opencode sends the `completed`
    /// frame with no `title` at all (kept in the golden). Carrying the name
    /// forward is what lets the finished line say `⚙ mana_list_agents ✓`
    /// instead of quoting `call_00_EqxLWddzxkxCTr9VzOas3550` at somebody.
    /// Entries are dropped when the call ends, so this holds what is running
    /// and not a session's history.
    tool_names: HashMap<String, String>,
}

impl TurnState {
    fn new(pending: PendingPermissions) -> Self {
        TurnState {
            kind: None,
            buffer: String::new(),
            pending,
            next_permission: 1,
            tool_names: HashMap::new(),
        }
    }

    /// Adds a chunk, emitting every line it completed.
    fn push(&mut self, kind: Chunk, text: &str) -> Vec<PmEvent> {
        let mut events = Vec::new();
        if self.kind != Some(kind) {
            events.extend(self.flush());
            self.kind = Some(kind);
        }
        self.buffer.push_str(text);
        while let Some(index) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=index).collect();
            events.push(chunk_event(kind, line.trim_end_matches(['\n', '\r'])));
        }
        events
    }

    /// Emits whatever is buffered as a final line. Called when the stream
    /// changes kind, when the turn ends, and before any interleaved event that
    /// would otherwise appear out of order.
    fn flush(&mut self) -> Vec<PmEvent> {
        let Some(kind) = self.kind.take() else {
            return Vec::new();
        };
        let text = std::mem::take(&mut self.buffer);
        if text.is_empty() {
            Vec::new()
        } else {
            vec![chunk_event(kind, &text)]
        }
    }

    /// Registers a permission request and returns the id the operator answers
    /// with.
    fn remember_permission(&mut self, request_id: &Value) -> u64 {
        let id = self.next_permission;
        self.next_permission += 1;
        self.pending.lock().unwrap().insert(id, request_id.clone());
        id
    }
}

fn chunk_event(kind: Chunk, text: &str) -> PmEvent {
    match kind {
        Chunk::Message => PmEvent::Text(text.to_string()),
        Chunk::Thought => PmEvent::Raw(text.to_string()),
    }
}

/// One line of the agent's stdout in, the events it carries out.
///
/// A pure function over a string, like the non-ACP event map in `events.rs`
/// and for the same reason: a recorded transcript replays through the exact
/// code path a live session uses, which is what makes the golden tests a real
/// guard rather than a parallel implementation.
fn decode(line: &str, state: &mut TurnState) -> Decoded {
    if line.trim().is_empty() {
        // Framing, not degradation: `Raw("")` for the gaps between frames would
        // fill the chat pane with nothing.
        return Decoded::none();
    }
    let Ok(frame) = serde_json::from_str::<Value>(line) else {
        // Not JSON at all -- a banner, a warning, a progress line. Exactly what
        // the raw fallback exists for.
        return Decoded::raw(line);
    };
    let Some(object) = frame.as_object() else {
        // A JSON-RPC batch (an array) or a bare scalar. mana never sends
        // batches, so nothing should answer in one.
        return Decoded::raw(line);
    };

    match (
        object.get("method").and_then(Value::as_str),
        object.get("id"),
    ) {
        (Some("session/update"), _) => Decoded::events(update_events(&frame, line, state)),
        (Some("session/request_permission"), Some(id)) => {
            permission_events(&frame, line, id, state)
        }
        // A request mana does not serve. It advertises no `fs`/`terminal`
        // capabilities, so a conforming agent never sends one -- and a
        // non-conforming one gets an answer instead of a deadlock.
        (Some(method), Some(id)) => Decoded {
            events: vec![PmEvent::Raw(format!(
                "[mana declined {method}: this client serves no filesystem or terminal]"
            ))],
            reply: Some(method_not_found(id, method)),
        },
        // A notification mana has no use for (`session/cancel` echoes and the
        // like): understood enough to be shown, not enough to act on.
        (Some(_), None) => Decoded::raw(line),
        (None, Some(_)) => response_events(&frame, line, state),
        (None, None) => Decoded::raw(line),
    }
}

/// A `session/update` notification, by its `sessionUpdate` discriminator.
///
/// The split is the thin contract of design §4 applied to a protocol that
/// actually names its parts: prose is what the operator reads, everything else
/// is either activity worth a dimmed line or noise mana understood and chose
/// not to render.
fn update_events(frame: &Value, line: &str, state: &mut TurnState) -> Vec<PmEvent> {
    let update = &frame["params"]["update"];
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => chunk(update, line, Chunk::Message, state),
        Some("agent_thought_chunk") => chunk(update, line, Chunk::Thought, state),
        Some("tool_call") => {
            let label = tool_label(update, None, state);
            activity(state, label)
        }
        Some("tool_call_update") => match update["status"].as_str() {
            // The interesting transitions only. A tool call reports pending →
            // in_progress → completed, and three lines per call would bury the
            // conversation it is supposed to annotate.
            Some(status @ ("completed" | "failed")) => {
                let label = tool_label(update, Some(status), state);
                forget_tool(update, state);
                activity(state, label)
            }
            // Nothing to show, but an intermediate frame may still be where
            // the agent named the call (opencode renames one as it learns what
            // it did), and that name is what the finished line will use.
            _ => {
                tool_name(update, state);
                Vec::new()
            }
        },
        // Recorded, never rendered: what a CLI counts is its own business and
        // mana only enriches its logs with it (design §4).
        Some("usage_update") => {
            let mut events = state.flush();
            events.push(PmEvent::Usage(update.clone()));
            events
        }
        // Understood and deliberately not shown. `available_commands_update`
        // alone was 128 KB of skill descriptions in the recorded opencode turn;
        // the rest is session bookkeeping the operator did not ask for.
        Some(
            "plan"
            | "plan_update"
            | "plan_removed"
            | "available_commands_update"
            | "current_mode_update"
            | "config_option_update"
            | "session_info_update"
            | "user_message_chunk",
        ) => Vec::new(),
        // An update this build does not know: a protocol extension, or an
        // agent inventing one. Degraded and visible, never silent.
        _ => vec![PmEvent::Raw(line.to_string())],
    }
}

/// One prose or reasoning chunk. Only text blocks carry something to read;
/// an image or an audio block in a chat pane is a line of JSON either way.
fn chunk(update: &Value, line: &str, kind: Chunk, state: &mut TurnState) -> Vec<PmEvent> {
    match (
        update["content"]["type"].as_str(),
        update["content"]["text"].as_str(),
    ) {
        (Some("text"), Some(text)) => state.push(kind, text),
        _ => vec![PmEvent::Raw(line.to_string())],
    }
}

/// A tool-call line, flushing whatever prose was mid-sentence so the order on
/// screen is the order it happened in.
fn activity(state: &mut TurnState, label: String) -> Vec<PmEvent> {
    let mut events = state.flush();
    events.push(PmEvent::Raw(label));
    events
}

/// One line of tool activity: a glyph, the tool's own name, and where it got
/// to. Started, finished, or failed -- three shapes, all short.
///
/// The wording is the operator's, not the protocol's. `[tool]
/// call_00_wHYHJzGP5X0DJa6kZozS5109 · completed` is a line that costs a full
/// row of the pane to say "a tool finished", and a session doing real work
/// produced dozens of them.
fn tool_label(update: &Value, status: Option<&str>, state: &mut TurnState) -> String {
    let name = tool_name(update, state);
    match status {
        Some("failed") => format!("⚙ {name} ✗"),
        Some(_) => format!("⚙ {name} ✓"),
        None => format!("⚙ {name} …"),
    }
}

/// The name to show for a tool call, remembering it for the frames that leave
/// it out. Never the call id: an opaque identifier is noise wearing the
/// costume of information.
fn tool_name(update: &Value, state: &mut TurnState) -> String {
    let id = update["toolCallId"].as_str().unwrap_or_default();
    // `title` is optional on an update frame, and opencode sends it *empty* on
    // one (measured) and absent on another (in the golden), which is why this
    // filters rather than unwraps.
    if let Some(title) = update["title"].as_str().filter(|title| !title.is_empty()) {
        if !id.is_empty() {
            state.tool_names.insert(id.to_string(), title.to_string());
        }
        return title.to_string();
    }
    state
        .tool_names
        .get(id)
        .cloned()
        // A call whose name mana never saw. Anonymous beats cryptic: the line
        // still says a tool ran, which is all the operator can act on.
        .unwrap_or_else(|| "a tool".to_string())
}

/// Drops a finished call's remembered name, so a long session's decoder holds
/// what is running rather than everything that ever ran.
fn forget_tool(update: &Value, state: &mut TurnState) {
    if let Some(id) = update["toolCallId"].as_str() {
        state.tool_names.remove(id);
    }
}

/// The agent asking the operator for permission. Surfaced, never answered
/// here: the reply comes from whoever is watching the chat pane, through
/// `answer_permission`.
fn permission_events(
    frame: &Value,
    line: &str,
    request_id: &Value,
    state: &mut TurnState,
) -> Decoded {
    let params = &frame["params"];
    let Some(options) = params["options"].as_array() else {
        return Decoded::raw(line);
    };
    let options: Vec<PermissionChoice> = options
        .iter()
        .filter_map(|option| {
            let kind = option["kind"].as_str().unwrap_or_default();
            Some(PermissionChoice {
                id: option["optionId"].as_str()?.to_string(),
                label: option["name"].as_str().unwrap_or(kind).to_string(),
                // `allow_once` / `allow_always` versus `reject_*`: the protocol
                // says which way an option goes, so the operator gets one key
                // for yes and one for no however many options an agent offers.
                allows: kind.starts_with("allow"),
            })
        })
        .collect();
    if options.is_empty() {
        // A request nobody could answer. Shown whole so the operator can see
        // what the agent asked for, and answered so the agent stops waiting.
        return Decoded {
            events: vec![PmEvent::Raw(line.to_string())],
            reply: Some(method_not_found(request_id, "session/request_permission")),
        };
    }

    let description = describe_tool_call(&params["toolCall"], state);
    let mut events = state.flush();
    events.push(PmEvent::PermissionRequest {
        id: state.remember_permission(request_id),
        description,
        options,
    });
    Decoded::events(events)
}

/// What the agent wants permission for, in one line.
///
/// The same name the activity lines use, from the same place, and with the
/// same refusal to quote a call id: this line is a question put to a human,
/// and `allow call_00_wHYHJzGP5X0DJa6kZozS5109?` is not a question anybody can
/// answer.
fn describe_tool_call(tool_call: &Value, state: &mut TurnState) -> String {
    match tool_name(tool_call, state).as_str() {
        "a tool" => "an unnamed tool call".to_string(),
        name => name.to_string(),
    }
}

/// A response to something mana sent. After the handshake mana sends exactly
/// one kind of request, so a response carrying a `stopReason` is the end of a
/// turn and anything else is a surprise worth showing.
fn response_events(frame: &Value, line: &str, state: &mut TurnState) -> Decoded {
    if let Some(error) = frame.get("error") {
        // A turn that failed. opencode and copilot both answer `session/prompt`
        // with a result even when the model refuses, so this is the transport
        // failing rather than the model -- and it is the only notice there is.
        let mut events = state.flush();
        events.push(PmEvent::Raw(format!(
            "[the PM's CLI reported an error] {}",
            describe_error(error)
        )));
        // A failed turn is still a turn that is over, and saying so is what
        // stops mana holding a queued message for a PM that is never going to
        // answer this one.
        events.push(PmEvent::TurnEnded);
        return Decoded::events(events);
    }
    let result = &frame["result"];
    let Some(stop) = result["stopReason"].as_str() else {
        return Decoded::raw(line);
    };
    let mut events = state.flush();
    if stop != "end_turn" {
        // `max_tokens`, `refusal`, `cancelled`, `max_turn_requests`: the turn
        // is over for a reason the operator has to know about, because the PM
        // stopped mid-thought and will not carry on by itself.
        events.push(PmEvent::Raw(format!("[the PM's turn ended: {stop}]")));
    }
    if let Some(usage) = result.get("usage").filter(|usage| !usage.is_null()) {
        events.push(PmEvent::Usage(usage.clone()));
    }
    // Every `stopReason`, not only `end_turn`: the protocol's own word for
    // "mana's request is answered". Last, so the whole turn -- prose, totals,
    // the reason it stopped -- is in hand before anything acts on it.
    events.push(PmEvent::TurnEnded);
    Decoded::events(events)
}

// --- JSON-RPC framing --------------------------------------------------

/// One JSON-RPC request, as a single line.
fn request(id: u64, method: &str, params: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("a request built from plain JSON values")
}

/// The JSON-RPC "method not found" reply (-32601).
fn method_not_found(id: &Value, method: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": format!("mana does not implement {method}"),
        },
    }))
    .expect("an error built from plain JSON values")
}

/// A JSON-RPC error object as one readable sentence.
fn describe_error(error: &Value) -> String {
    let message = error["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    match error["data"].as_str() {
        Some(data) => format!("{message}: {data}"),
        None => message,
    }
}

/// Best-effort write from the reader thread, which has nobody to report to:
/// the failure it would report is a closed pipe, and the `Exited` event that
/// follows says the same thing better.
fn write_line(stdin: &SharedStdin, frame: &str) {
    let _ = write_line_or_err(stdin, frame);
}

fn write_line_or_err(stdin: &SharedStdin, frame: &str) -> Result<()> {
    let mut guard = stdin.lock().unwrap();
    let stdin = guard
        .as_mut()
        .ok_or_else(|| anyhow!("the PM session's stdin is closed"))?;
    stdin.write_all(frame.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete, valid `acp` entry, built through the real parser so it
    /// cannot drift from the schema -- and so the two fields the catalogue
    /// forbids here (`[pm.events]`, `[pm].prompt`) stay forbidden.
    pub(super) fn entry_source(bin: &str, args: &[&str], channel: &str) -> String {
        // A JSON array of strings is also a valid TOML array, so serde_json
        // does the quoting -- including for the temp paths these tests spawn.
        let args = serde_json::to_string(args).unwrap();
        format!(
            r#"
schema = 1
notes = "acp test fixture"

[cli]
id = "fake"
name = "Fake ACP CLI"
bin = {bin:?}
version_args = ["--version"]

[pm]
driver = "acp"
args = {args}

[tools]
channel = "{channel}"

[subagent]
args = []
prompt = "argv"
max_concurrent = 0
cwd_required_in_brief = false

[models]
discovery_args = []

[skills]
dirs = ["~/.fake/skills"]

[install]
url = "https://example.invalid/fake"
"#
        )
    }

    pub(super) fn entry(bin: &str, args: &[&str]) -> CliEntry {
        crate::catalog::parse_entry(&entry_source(bin, args, "mcp")).expect("fixture must be valid")
    }

    fn state() -> TurnState {
        TurnState::new(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Replays lines through the decoder the way the reader thread does.
    fn replay<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<PmEvent> {
        let mut state = state();
        lines
            .into_iter()
            .flat_map(|line| decode(line, &mut state).events)
            .collect()
    }

    fn chunk_line(kind: &str, text: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"{kind}","content":{{"type":"text","text":{}}}}}}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    fn update_line(update: Value) -> String {
        json!({"jsonrpc": "2.0", "method": "session/update",
               "params": {"sessionId": "s", "update": update}})
        .to_string()
    }

    fn text(events: &[PmEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                PmEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn blank_lines_produce_nothing() {
        assert_eq!(decode("", &mut state()), Decoded::none());
        assert_eq!(decode("   ", &mut state()), Decoded::none());
    }

    /// The malformed-frame contract: visible, and the session carries on.
    #[test]
    fn a_line_that_is_not_json_falls_through_raw_and_the_next_frame_still_works() {
        let events = replay([
            "Warning: something happened",
            &chunk_line("agent_message_chunk", "hello\n"),
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Raw("Warning: something happened".to_string()),
                PmEvent::Text("hello".to_string()),
            ]
        );
    }

    /// JSON that is not a JSON-RPC object at all -- a batch, a bare scalar.
    #[test]
    fn json_that_is_not_a_frame_falls_through_raw() {
        for line in ["[1,2]", "\"just a string\"", "{\"nothing\":\"useful\"}"] {
            assert_eq!(replay([line]), vec![PmEvent::Raw(line.to_string())]);
        }
    }

    /// The reason this driver buffers at all: opencode sent nine chunks for
    /// nine characters, and one event each would put one character per line in
    /// the chat pane.
    #[test]
    fn message_chunks_are_coalesced_into_whole_lines() {
        let mut state = state();
        let mut events = Vec::new();
        for part in ["MAN", "A", "-OK", "\nsecond line\n", "a tail"] {
            events.extend(decode(&chunk_line("agent_message_chunk", part), &mut state).events);
        }
        assert_eq!(text(&events), ["MANA-OK", "second line"]);
        // The tail has no newline yet, so it waits for the end of the turn.
        assert_eq!(state.flush(), vec![PmEvent::Text("a tail".to_string())]);
    }

    /// Reasoning is not prose: it is shown, dimmed, but never mistaken for
    /// what the PM said (design §4).
    #[test]
    fn thought_chunks_are_raw_and_switching_stream_flushes_what_came_before() {
        let events = replay([
            chunk_line("agent_thought_chunk", "let me think").as_str(),
            chunk_line("agent_message_chunk", "the answer").as_str(),
            update_line(json!({"sessionUpdate": "usage_update", "used": 1})).as_str(),
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Raw("let me think".to_string()),
                PmEvent::Text("the answer".to_string()),
                PmEvent::Usage(json!({"sessionUpdate": "usage_update", "used": 1})),
            ]
        );
    }

    /// A chunk carrying something that is not text at all.
    #[test]
    fn a_non_text_content_chunk_degrades_to_raw() {
        let line = update_line(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "image", "data": "…", "mimeType": "image/png"},
        }));
        assert_eq!(replay([line.as_str()]), vec![PmEvent::Raw(line)]);
    }

    /// Tool activity is annotation, not conversation: one line when it starts,
    /// one when it ends, and nothing for the transition in between.
    #[test]
    fn tool_calls_are_two_raw_lines_and_flush_the_prose_before_them() {
        let events = replay([
            chunk_line("agent_message_chunk", "working on it").as_str(),
            update_line(json!({"sessionUpdate": "tool_call", "toolCallId": "c1",
                               "title": "mana_list_agents", "status": "pending"}))
            .as_str(),
            update_line(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                               "title": "mana_list_agents", "status": "in_progress"}),
            )
            .as_str(),
            update_line(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                               "title": "", "status": "completed"}),
            )
            .as_str(),
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Text("working on it".to_string()),
                PmEvent::Raw("⚙ mana_list_agents …".to_string()),
                // The empty title opencode really sends falls back to the name
                // the call was opened under -- never to `c1`.
                PmEvent::Raw("⚙ mana_list_agents ✓".to_string()),
            ]
        );
    }

    /// A call that fails says so, in the same shape.
    #[test]
    fn a_failed_tool_call_is_marked_rather_than_reported_as_done() {
        let events = replay([
            update_line(json!({"sessionUpdate": "tool_call", "toolCallId": "c1",
                               "title": "bash", "status": "pending"}))
            .as_str(),
            update_line(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                               "status": "failed"}),
            )
            .as_str(),
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Raw("⚙ bash …".to_string()),
                PmEvent::Raw("⚙ bash ✗".to_string()),
            ]
        );
    }

    /// An agent that renames a call as it goes (opencode does: `skill` becomes
    /// `Loaded skill: mana-pm`) is quoted with whatever it last called it.
    #[test]
    fn a_call_renamed_mid_flight_is_shown_under_its_newest_name() {
        let events = replay([
            update_line(json!({"sessionUpdate": "tool_call", "toolCallId": "c9",
                               "title": "skill", "status": "pending"}))
            .as_str(),
            update_line(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c9",
                               "title": "Loaded skill: mana-pm", "status": "completed"}),
            )
            .as_str(),
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Raw("⚙ skill …".to_string()),
                PmEvent::Raw("⚙ Loaded skill: mana-pm ✓".to_string()),
            ]
        );
    }

    /// A call nobody ever named. Anonymous beats cryptic: the id says nothing
    /// the glyph did not, and it costs a whole row to say it.
    #[test]
    fn a_call_with_no_name_anywhere_is_never_quoted_by_its_id() {
        let events = replay([update_line(json!({"sessionUpdate": "tool_call",
                               "toolCallId": "call_00_wHYHJzGP5X0DJa6kZozS5109"}))
        .as_str()]);
        assert_eq!(events, vec![PmEvent::Raw("⚙ a tool …".to_string())]);
    }

    /// The decoder must not accumulate a session's worth of tool names: a
    /// finished call is forgotten the moment it ends.
    #[test]
    fn a_finished_call_is_forgotten_by_the_decoder() {
        let mut state = state();
        for line in [
            update_line(json!({"sessionUpdate": "tool_call", "toolCallId": "c1",
                               "title": "bash", "status": "pending"})),
            update_line(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                               "status": "completed"}),
            ),
        ] {
            decode(&line, &mut state);
        }
        assert!(state.tool_names.is_empty());
    }

    /// Understood and deliberately dropped -- `available_commands_update` was
    /// 128 KB of skill descriptions in the recorded turn.
    #[test]
    fn session_bookkeeping_updates_render_nothing() {
        for kind in [
            "available_commands_update",
            "current_mode_update",
            "config_option_update",
            "session_info_update",
            "plan",
            "user_message_chunk",
        ] {
            let line = update_line(json!({"sessionUpdate": kind, "anything": [1, 2, 3]}));
            assert!(
                replay([line.as_str()]).is_empty(),
                "{kind} rendered something"
            );
        }
    }

    /// A protocol extension this build has never heard of. The one case where
    /// silence would be wrong.
    #[test]
    fn an_unknown_update_kind_is_visible_rather_than_dropped() {
        let line = update_line(json!({"sessionUpdate": "quantum_update", "value": 1}));
        assert_eq!(replay([line.as_str()]), vec![PmEvent::Raw(line)]);
    }

    #[test]
    fn the_end_of_a_turn_flushes_the_last_line_and_reports_usage() {
        let events = replay([
            chunk_line("agent_message_chunk", "no trailing newline").as_str(),
            r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","usage":{"outputTokens":10}}}"#,
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Text("no trailing newline".to_string()),
                PmEvent::Usage(json!({"outputTokens": 10})),
                // Last, so everything the turn carried is in hand before
                // anything waiting on it is released.
                PmEvent::TurnEnded,
            ]
        );
    }

    /// A turn that ended for any other reason stopped mid-thought, and the
    /// operator has to know rather than wonder why the PM went quiet.
    ///
    /// It is still an ended turn: a queue that only drained on `end_turn`
    /// would hold everything typed after a refusal for the rest of the session.
    #[test]
    fn a_turn_that_ended_badly_says_why_and_still_ends() {
        let events = replay([r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"max_tokens"}}"#]);
        assert_eq!(
            events,
            vec![
                PmEvent::Raw("[the PM's turn ended: max_tokens]".to_string()),
                PmEvent::TurnEnded,
            ]
        );
    }

    #[test]
    fn a_json_rpc_error_reaches_the_chat_pane_and_ends_the_turn() {
        let events = replay([
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"quota exhausted","data":"try tomorrow"}}"#,
        ]);
        assert_eq!(
            events,
            vec![
                PmEvent::Raw(
                    "[the PM's CLI reported an error] quota exhausted: try tomorrow".to_string()
                ),
                PmEvent::TurnEnded,
            ]
        );
    }

    fn permission_line() -> String {
        json!({
            "jsonrpc": "2.0", "id": 77, "method": "session/request_permission",
            "params": {
                "sessionId": "s",
                "toolCall": {"toolCallId": "c1", "title": "write src/main.rs"},
                "options": [
                    {"optionId": "once", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                ],
            },
        })
        .to_string()
    }

    #[test]
    fn a_permission_request_becomes_an_answerable_event() {
        let mut state = state();
        let decoded = decode(&permission_line(), &mut state);
        assert_eq!(decoded.reply, None, "nothing is answered without a human");
        assert_eq!(
            decoded.events,
            vec![PmEvent::PermissionRequest {
                id: 1,
                description: "write src/main.rs".to_string(),
                options: vec![
                    PermissionChoice {
                        id: "once".to_string(),
                        label: "Allow once".to_string(),
                        allows: true
                    },
                    PermissionChoice {
                        id: "always".to_string(),
                        label: "Always allow".to_string(),
                        allows: true
                    },
                    PermissionChoice {
                        id: "no".to_string(),
                        label: "Reject".to_string(),
                        allows: false
                    },
                ],
            }]
        );
        // The agent's own JSON-RPC id is kept here and nowhere else, so the
        // answer can be addressed without the interface ever seeing it.
        assert_eq!(state.pending.lock().unwrap().get(&1), Some(&json!(77)));
    }

    /// Ids are per-request, so two questions cannot be answered by one answer.
    #[test]
    fn each_permission_request_gets_its_own_id() {
        let mut state = state();
        let first = decode(&permission_line(), &mut state);
        let second = decode(
            &permission_line().replace("\"id\":77", "\"id\":78"),
            &mut state,
        );
        let ids: Vec<u64> = [first, second]
            .iter()
            .flat_map(|decoded| decoded.events.iter())
            .filter_map(|event| match event {
                PmEvent::PermissionRequest { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [1, 2]);
        assert_eq!(state.pending.lock().unwrap().len(), 2);
    }

    /// An agent may use a string id; the interface must never find out.
    #[test]
    fn a_string_request_id_is_carried_through_untouched() {
        let mut state = state();
        decode(
            &permission_line().replace("\"id\":77", "\"id\":\"abc\""),
            &mut state,
        );
        assert_eq!(state.pending.lock().unwrap().get(&1), Some(&json!("abc")));
    }

    /// A question with no answers would hang the agent for ever if mana just
    /// showed it, so it is refused and shown.
    #[test]
    fn a_permission_request_with_no_options_is_refused_rather_than_surfaced() {
        let line = json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/request_permission",
            "params": {"sessionId": "s", "toolCall": {}, "options": []},
        })
        .to_string();
        let decoded = decode(&line, &mut state());
        assert_eq!(decoded.events, vec![PmEvent::Raw(line)]);
        let reply: Value = serde_json::from_str(&decoded.reply.unwrap()).unwrap();
        assert_eq!(reply["id"], 5);
        assert_eq!(reply["error"]["code"], -32601);
    }

    /// mana advertises no filesystem and no terminal, so a conforming agent
    /// never asks -- and a non-conforming one gets an answer instead of
    /// waiting for ever on a client that will never reply.
    #[test]
    fn a_request_mana_does_not_serve_is_declined_not_ignored() {
        let line = json!({"jsonrpc": "2.0", "id": 9, "method": "fs/read_text_file",
                          "params": {"path": "/etc/passwd"}})
        .to_string();
        let decoded = decode(&line, &mut state());
        assert!(matches!(decoded.events.as_slice(), [PmEvent::Raw(line)]
                         if line.contains("fs/read_text_file")));
        let reply: Value = serde_json::from_str(&decoded.reply.unwrap()).unwrap();
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn a_request_is_one_json_rpc_line() {
        assert_eq!(
            request(3, "session/prompt", json!({"sessionId": "s"})),
            r#"{"id":3,"jsonrpc":"2.0","method":"session/prompt","params":{"sessionId":"s"}}"#
        );
    }

    /// The tools registration is the ACP-native half of what the launch flow
    /// writes to a file for the argv-based CLIs, and it carries the same two
    /// facts: mana's absolute path and which project it acts on.
    #[test]
    fn the_mcp_registration_names_manas_own_binary_and_the_project() {
        let servers = mcp_servers(&entry("fake", &[]), Path::new("/home/x/code/demo")).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "mana");
        assert_eq!(
            servers[0]["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            servers[0]["args"],
            json!(["mcp-server", "--project-root", "/home/x/code/demo"])
        );
    }

    /// A CLI whose catalogue entry says it has no MCP surface gets no MCP
    /// server, whatever the protocol would allow.
    #[test]
    fn a_sentinel_entry_registers_no_mcp_server() {
        let entry = crate::catalog::parse_entry(&entry_source("fake", &[], "sentinel")).unwrap();
        assert!(mcp_servers(&entry, Path::new("/x")).unwrap().is_empty());
    }

    #[test]
    fn an_unfilled_placeholder_in_pm_args_is_a_startup_error_naming_the_field() {
        let rendered = match AcpDriver::start_in(
            &entry("mana-no-such-binary", &["--model", "{model}"]),
            &[],
            Path::new("/"),
            None,
        ) {
            Ok(_) => panic!("the driver started a session it should have refused"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("[pm].args"), "{rendered}");
        assert!(rendered.contains("model"), "{rendered}");
    }

    #[test]
    fn a_missing_binary_is_reported_with_the_cli_that_owns_it() {
        let rendered = match AcpDriver::start_in(
            &entry("mana-no-such-binary", &[]),
            &[],
            Path::new("/"),
            None,
        ) {
            Ok(_) => panic!("the driver started a session it should have refused"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("Fake ACP CLI"), "{rendered}");
        assert!(rendered.contains("mana-no-such-binary"), "{rendered}");
    }
}

/// The CI guard the design asks for: real recorded ACP sessions replayed
/// through the shipped decoder.
///
/// Unlike the non-ACP goldens there is no per-CLI map to check here -- ACP is
/// one protocol -- so what these pin down is the *mapping decisions*: which
/// notifications become prose, which become dimmed activity, and which are
/// understood and dropped. A vendor that starts chunking differently, or
/// mana that starts rendering thoughts as prose, fails here.
#[cfg(test)]
mod golden {
    use super::*;

    /// Recorded 2026-08-15 from a real `opencode acp` session driven by a
    /// minimal ACP client: the mana MCP server was attached through
    /// `session/new`, and the prompt asked the agent to call
    /// `mana_list_agents` and report how many CLIs it found.
    ///
    /// Verbatim except for one trim: the `available_commands_update` frame's
    /// `availableCommands` array is cut to two entries. It was 128 KB of skill
    /// descriptions, and mana drops the frame whole either way.
    const OPENCODE: &str = include_str!("../../catalog/goldens/opencode/single-turn.jsonl");

    /// Recorded the same day from `copilot --acp`, kept because of what it
    /// shows rather than what it proves: this account's monthly quota was
    /// exhausted, and copilot reported that as an ordinary assistant message
    /// with `stopReason: end_turn`. Over ACP a quota wall looks exactly like an
    /// answer -- which is why copilot's catalogue entry matches its *headless*
    /// stderr instead.
    const COPILOT: &str = include_str!("../../catalog/goldens/copilot/quota-exhausted.jsonl");

    fn replay(transcript: &str) -> Vec<PmEvent> {
        let mut state = TurnState::new(Arc::new(Mutex::new(HashMap::new())));
        transcript
            .lines()
            .flat_map(|line| {
                let decoded = decode(line, &mut state);
                assert_eq!(decoded.reply, None, "a recorded frame needed answering");
                decoded.events
            })
            .collect()
    }

    #[test]
    fn opencode_single_turn_replays_to_one_answer_two_tool_lines_and_its_usage() {
        let events = replay(OPENCODE);

        // 54 frames in, and exactly one of them is something the user asked to
        // read. That ratio is the whole point of the driver.
        let prose: Vec<&PmEvent> = events
            .iter()
            .filter(|event| matches!(event, PmEvent::Text(_)))
            .collect();
        assert_eq!(
            prose,
            [&PmEvent::Text("MANA-TOOLS-OK 2".to_string())],
            "{events:#?}"
        );

        // The nine message chunks arrived as "MAN", "A", "-", "TO", "OLS",
        // "-", "OK", " ", "2".
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PmEvent::Text(_)))
                .count(),
            1
        );

        // The forty thought chunks became one readable dimmed line, not forty.
        let raws: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                PmEvent::Raw(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(raws.len(), 3, "{raws:#?}");
        assert!(
            raws[0].starts_with("The user wants me to call"),
            "{raws:#?}"
        );
        assert!(raws[0].ends_with("Let me do that."), "{raws:#?}");
        // ...and the tool call is two short lines: it started, it finished.
        // The `completed` frame in this recording carries no title at all, and
        // the call id it *does* carry
        // (`call_00_EqxLWddzxkxCTr9VzOas3550`) is exactly what must never
        // reach the pane -- so the name is the one the call was opened under.
        assert_eq!(raws[1], "⚙ mana_list_agents …");
        assert_eq!(raws[2], "⚙ mana_list_agents ✓");
        assert!(
            !raws.iter().any(|line| line.contains("call_00_")),
            "a raw call id reached the chat pane: {raws:#?}"
        );

        // Two usage snapshots, in the order they were reported: the
        // context-window update mid-turn, then the token totals with the
        // stopReason. Both are recorded; only the second has anything the
        // status bar can count.
        let usage: Vec<&Value> = events
            .iter()
            .filter_map(|event| match event {
                PmEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), 2, "{usage:#?}");
        assert_eq!(usage[0]["used"], 81866);
        assert_eq!(usage[1]["outputTokens"], 10);
        assert_eq!(usage[1]["cachedReadTokens"], 81664);

        // One turn, one boundary, and it is the last thing the recording
        // produced -- the response to mana's `session/prompt`.
        assert_eq!(events.last(), Some(&PmEvent::TurnEnded), "{events:#?}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PmEvent::TurnEnded))
                .count(),
            1
        );

        // Nothing else survived: no permission requests, and the 128 KB
        // command list is gone.
        assert_eq!(events.len(), 7, "{events:#?}");
    }

    /// The shape a maintainer needs to recognise: over ACP, a quota wall is
    /// indistinguishable from an answer.
    #[test]
    fn copilots_quota_wall_arrives_as_ordinary_prose() {
        let events = replay(COPILOT);
        assert_eq!(
            events
                .iter()
                .filter(
                    |event| matches!(event, PmEvent::Text(text) if text.contains(
                        "You have exceeded your monthly quota"
                    ))
                )
                .count(),
            1,
            "{events:#?}"
        );
        // ...and it ended the turn as if nothing were wrong, which is why the
        // catalogue entry cannot rely on the ACP side to detect exhaustion.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PmEvent::Raw(line) if line.contains("turn ended"))),
            "{events:#?}"
        );
        // The usage_update it did send has nothing token-shaped in it.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PmEvent::Usage(usage) if usage["used"] == 17484))
        );
    }
}

/// The live half: a scripted ACP agent, driven through the real driver.
///
/// Unix-only because the fake agent is a shell script, like every other
/// process test in the tree. What it cannot cover is whether a real CLI
/// honours the protocol -- that is what the goldens and the notes in
/// `catalog/opencode.toml` / `catalog/copilot.toml` are for.
#[cfg(all(test, unix))]
mod process_tests {
    use super::tests::entry;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A scripted agent: answers the handshake, then behaves according to
    /// `$1`. `$2` is where it records every frame the client sent, which is
    /// how a test asserts what mana put on the wire; `$3` is a pid file the
    /// lingering behaviour writes its background child into.
    ///
    /// Written in `sh` rather than in a real language on purpose: it is the
    /// same shape as the stream driver's fixtures and it adds no interpreter
    /// to what CI has to have installed.
    const AGENT: &str = r#"#!/bin/sh
mode="$1"
record="$2"
pidfile="$3"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$record"
  id=`printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'`
  case "$line" in
    *'"initialize"'*)
      # Only the `loadable` agent advertises session loading, which is what
      # decides whether mana is allowed to resume against it at all.
      case "$mode" in
        loadable)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"fake"}}}\n' "$id"
          ;;
        *)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"fake"}}}\n' "$id"
          ;;
      esac
      ;;
    *'"session/load"'*)
      # Deliberately answers with an empty result: the protocol does not
      # require the id back, so mana must keep the one it asked for.
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"session/new"'*)
      case "$mode" in
        refuse)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"authentication required","data":"run fake login"}}\n' "$id"
          ;;
        *)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"current_mode_update","currentModeId":"agent"}}}\n'
          printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s-1"}}\n' "$id"
          ;;
      esac
      ;;
    *'"session/prompt"'*)
      case "$mode" in
        die)
          echo 'boom: the agent fell over' >&2
          exit 9
          ;;
        noisy)
          # One line on stderr, one on stdout: the shape the duplicated-stderr
          # hunt needed a ruler for.
          echo 'warning: a single line on stderr' >&2
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}\n'
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
          ;;
        linger)
          sleep 30 &
          echo $! > "$pidfile"
          sleep 30
          ;;
        ask)
          printf '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"sessionId":"s-1","toolCall":{"toolCallId":"c1","title":"write README.md"},"options":[{"optionId":"yes","name":"Allow once","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}}\n'
          ;;
        *)
          printf 'a line that is not JSON at all\n'
          for part in ack ed; do
            printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"%s"}}}}\n' "$part"
          done
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","usage":{"outputTokens":4}}}\n' "$id"
          ;;
      esac
      ;;
    *'"optionId"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answered"}}}}\n'
      printf '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}\n'
      ;;
  esac
done
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        bin: String,
        record: PathBuf,
        pidfile: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("fake-acp-agent");
            std::fs::write(&bin, AGENT).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            Fixture {
                bin: bin.to_string_lossy().into_owned(),
                record: dir.path().join("received.jsonl"),
                pidfile: dir.path().join("grandchild.pid"),
                dir,
            }
        }

        fn start(&self, mode: &str) -> Result<AcpDriver> {
            self.start_with(mode, None)
        }

        fn start_with(&self, mode: &str, resume: Option<Resume<'_>>) -> Result<AcpDriver> {
            let args = [
                mode,
                self.record.to_str().unwrap(),
                self.pidfile.to_str().unwrap(),
            ];
            AcpDriver::start_in(&entry(&self.bin, &args), &[], self.dir.path(), resume)
        }

        fn driver(&self, mode: &str) -> AcpDriver {
            self.start(mode)
                .expect("the fake agent completed a handshake")
        }

        /// Every frame the client sent, parsed.
        fn sent(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.record)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn wait_for_sent(&self, count: usize) -> Vec<Value> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let sent = self.sent();
                if sent.len() >= count {
                    return sent;
                }
                assert!(
                    Instant::now() < deadline,
                    "only {} frames reached the agent: {sent:#?}",
                    sent.len()
                );
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    fn next(driver: &AcpDriver) -> PmEvent {
        driver
            .events()
            .recv_timeout(Duration::from_secs(10))
            .expect("the PM produced no further event")
    }

    /// Everything up to and including `Exited`, which always terminates the
    /// stream.
    fn drain_to_exit(driver: &AcpDriver) -> Vec<PmEvent> {
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

    /// Resuming asks for the stored session by name -- and mana keeps the id
    /// it asked for rather than one read back, because the protocol does not
    /// promise the reply carries it (this fake agent answers `{}`; opencode
    /// echoes it).
    #[test]
    fn resuming_an_agent_that_advertises_load_session_sends_session_load() {
        let fixture = Fixture::new();
        let mut driver = fixture
            .start_with(
                "loadable",
                Some(Resume {
                    session_id: Some("s-stored"),
                }),
            )
            .expect("the agent advertised loadSession, so the resume was allowed");

        let sent = fixture.wait_for_sent(2);
        assert_eq!(sent[1]["method"], "session/load");
        assert_eq!(sent[1]["params"]["sessionId"], "s-stored");
        // A loaded session gets a fresh agent process, so mana's tools have to
        // be attached to it exactly as they are to a new one.
        assert_eq!(sent[1]["params"]["mcpServers"][0]["name"], "mana");
        assert_eq!(
            sent[1]["params"]["cwd"],
            fixture.dir.path().to_string_lossy().as_ref()
        );
        assert_eq!(driver.session_id, "s-stored");
        driver.set_close_grace(Duration::from_millis(200));
        driver.shutdown().unwrap();
    }

    /// The refusal that keeps `--continue` honest: an agent that never said it
    /// serves `session/load` is not asked, and the launch stops rather than
    /// opening a fresh conversation under a flag that promised the old one.
    #[test]
    fn an_agent_without_the_capability_is_refused_rather_than_started_fresh() {
        let fixture = Fixture::new();
        let rendered = match fixture.start_with(
            "chunks",
            Some(Resume {
                session_id: Some("s-stored"),
            }),
        ) {
            Ok(_) => panic!("mana resumed against an agent that cannot load sessions"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("loadSession"), "{rendered}");
        assert!(rendered.contains("Fake ACP CLI"), "{rendered}");
        assert!(rendered.contains("--continue"), "{rendered}");

        // Nothing was asked for: the handshake stopped after `initialize`, and
        // no session was opened behind the refusal.
        let sent = fixture.wait_for_sent(1);
        assert_eq!(sent.len(), 1, "{sent:#?}");
        assert_eq!(sent[0]["method"], "initialize");
    }

    /// The other half of the same refusal: the agent can load a session, mana
    /// has never recorded one for this project. Also not silently downgraded.
    #[test]
    fn resuming_without_a_stored_session_id_says_so() {
        let fixture = Fixture::new();
        let rendered = match fixture.start_with("loadable", Some(Resume { session_id: None })) {
            Ok(_) => panic!("mana resumed a session it never recorded"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("no recorded"), "{rendered}");
        assert!(rendered.contains("--continue"), "{rendered}");
    }

    /// The whole round trip: the handshake mana sends, the tools it registers,
    /// a turn out, chunks back in, coalesced into one line.
    #[test]
    fn a_session_handshakes_registers_manas_tools_and_carries_a_turn() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("chunks");

        let sent = fixture.wait_for_sent(2);
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[0]["params"]["protocolVersion"], PROTOCOL_VERSION);
        // mana advertises no filesystem and no terminal: a PM that could write
        // through its client would be a PM writing code (design §6).
        assert_eq!(sent[0]["params"]["clientCapabilities"]["terminal"], false);
        assert_eq!(
            sent[0]["params"]["clientCapabilities"]["fs"]["writeTextFile"],
            false
        );

        assert_eq!(sent[1]["method"], "session/new");
        assert_eq!(
            sent[1]["params"]["cwd"],
            fixture.dir.path().to_string_lossy().as_ref()
        );
        // The ACP-native tool channel: mana registered itself by absolute path,
        // for this project, with no config file anywhere.
        let server = &sent[1]["params"]["mcpServers"][0];
        assert_eq!(server["name"], "mana");
        assert_eq!(
            server["command"],
            std::env::current_exe().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            server["args"],
            json!([
                "mcp-server",
                "--project-root",
                fixture.dir.path().to_string_lossy()
            ])
        );

        driver.send_user("say something").unwrap();
        let sent = fixture.wait_for_sent(3);
        assert_eq!(sent[2]["method"], "session/prompt");
        assert_eq!(sent[2]["params"]["sessionId"], "s-1");
        assert_eq!(
            sent[2]["params"]["prompt"],
            json!([{"type": "text", "text": "say something"}])
        );

        // The junk line survives as visible degradation...
        assert_eq!(
            next(&driver),
            PmEvent::Raw("a line that is not JSON at all".to_string())
        );
        // ...and the two chunks arrive as one line, at the end of the turn.
        assert_eq!(next(&driver), PmEvent::Text("acked".to_string()));
        assert_eq!(next(&driver), PmEvent::Usage(json!({"outputTokens": 4})));

        driver.shutdown().unwrap();
        assert_eq!(
            drain_to_exit(&driver).last(),
            Some(&PmEvent::Exited { code: Some(0) })
        );
    }

    /// The permission loop end to end: the agent asks, mana surfaces it, an
    /// answer goes back addressed to the agent's own request id, the turn
    /// resumes.
    #[test]
    fn a_permission_request_is_surfaced_and_answering_it_unblocks_the_turn() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("ask");
        driver.send_user("edit the readme").unwrap();

        let PmEvent::PermissionRequest {
            id,
            description,
            options,
        } = next(&driver)
        else {
            panic!("the agent's question never reached the interface");
        };
        assert_eq!(description, "write README.md");
        assert_eq!(options.iter().filter(|option| option.allows).count(), 1);

        driver.answer_permission(id, "yes").unwrap();
        let sent = fixture.wait_for_sent(4);
        // Addressed to the agent's id (77), not to mana's own (1).
        assert_eq!(sent[3]["id"], 77);
        assert_eq!(
            sent[3]["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "yes"})
        );
        assert_eq!(next(&driver), PmEvent::Text("answered".to_string()));

        // ...and the same id cannot be answered twice.
        let error = driver.answer_permission(id, "yes").unwrap_err();
        assert!(
            format!("{error:#}").contains("already answered"),
            "{error:#}"
        );
        driver.shutdown().unwrap();
    }

    /// An unanswered question must cost the PM its turn and mana nothing: the
    /// event loop keeps reading, keeps accepting turns, and shuts down.
    #[test]
    fn an_unanswered_permission_request_does_not_block_the_session() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("ask");
        driver.send_user("edit the readme").unwrap();
        assert!(matches!(next(&driver), PmEvent::PermissionRequest { .. }));

        // Nobody answers. mana is still alive and still talking.
        driver.send_user("never mind").unwrap();
        assert_eq!(fixture.wait_for_sent(4)[3]["method"], "session/prompt");
        driver.shutdown().unwrap();
        assert_eq!(
            drain_to_exit(&driver).last(),
            Some(&PmEvent::Exited { code: Some(0) })
        );
    }

    /// PM death must be detectable without anyone asking, and the reason has
    /// to arrive before the exit that follows it.
    #[test]
    fn an_agent_that_dies_mid_turn_reports_its_stderr_and_then_its_exit() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("die");
        driver.send_user("go on then").unwrap();

        let events = drain_to_exit(&driver);
        assert_eq!(events.last(), Some(&PmEvent::Exited { code: Some(9) }));
        assert!(
            events.contains(&PmEvent::Raw("boom: the agent fell over".to_string())),
            "{events:#?}"
        );
        // A turn sent to a corpse fails where the person who typed it can see.
        let error = driver.send_user("hello?").unwrap_err();
        assert!(!format!("{error:#}").is_empty());
    }

    /// One line on the PM's stderr is one line in the pane. Once.
    ///
    /// The v2.0 capture showed every opencode stderr line twice, and the
    /// suspicion was a second path inside mana -- the driver pumping stderr
    /// and something downstream echoing it again. It is not: measured on
    /// 2026-08-15 with a bare Python ACP client (mana's TUI nowhere in the
    /// picture, one `mana mcp-server` process), opencode wrote each of those
    /// schema warnings to its own stderr *twice*, once per pass over the tool
    /// schemas. The warnings themselves are gone now (`src/mcp.rs` stopped
    /// emitting the formats that provoked them); this is the guard that keeps
    /// the mana half honest, so a future reader never has to re-run that
    /// experiment.
    #[test]
    fn one_line_on_the_agents_stderr_is_exactly_one_raw_event() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("noisy");
        driver.send_user("say something").unwrap();
        fixture.wait_for_sent(3);
        driver.shutdown().unwrap();

        let events = drain_to_exit(&driver);
        let noise = PmEvent::Raw("warning: a single line on stderr".to_string());
        assert_eq!(
            events.iter().filter(|event| **event == noise).count(),
            1,
            "{events:#?}"
        );
    }

    /// A handshake the agent refuses is reported with the agent's own words --
    /// which is where "run `opencode auth login`" comes from.
    #[test]
    fn a_refused_session_reports_what_the_agent_said() {
        let fixture = Fixture::new();
        let rendered = match fixture.start("refuse") {
            Ok(_) => panic!("a session started that the agent refused"),
            Err(error) => format!("{error:#}"),
        };
        assert!(rendered.contains("authentication required"), "{rendered}");
        assert!(rendered.contains("run fake login"), "{rendered}");
    }

    /// The v1 zombie, structurally: an agent that ignores the polite exit is
    /// killed together with everything it spawned -- its MCP server above all.
    #[test]
    fn shutdown_kills_an_agent_that_ignores_the_close_and_takes_its_children_along() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("linger");
        let pid = driver.pid() as i32;
        driver.set_close_grace(Duration::from_millis(200));
        driver.send_user("go quiet on me").unwrap();
        fixture.wait_for_sent(3);

        let started = Instant::now();
        driver.shutdown().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "shutdown hung");
        assert_eq!(
            drain_to_exit(&driver).last(),
            Some(&PmEvent::Exited { code: None })
        );

        let grandchild: i32 = std::fs::read_to_string(&fixture.pidfile)
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

    /// Nothing else in mana knows the pid once the handle is gone.
    #[test]
    fn dropping_the_driver_kills_the_agent() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("linger");
        let pid = driver.pid() as i32;
        driver.set_close_grace(Duration::from_millis(200));
        driver.send_user("go quiet on me").unwrap();
        fixture.wait_for_sent(3);
        drop(driver);

        let deadline = Instant::now() + Duration::from_secs(3);
        while process_is_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        assert!(!process_is_alive(pid), "the PM survived its driver");
    }

    #[test]
    fn a_turn_sent_after_shutdown_fails_loudly() {
        let fixture = Fixture::new();
        let mut driver = fixture.driver("chunks");
        driver.shutdown().unwrap();

        let error = driver.send_user("too late").unwrap_err();
        assert!(format!("{error:#}").contains("closed"), "{error:#}");
    }
}
