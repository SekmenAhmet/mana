//! PM transport drivers: the one interactive session mana holds with an agent
//! CLI acting as project manager.
//!
//! Three transports are in the design (§4) -- `stream`, `acp`,
//! `oneshot-continue` -- and the catalogue's `[pm].driver` field says which one
//! a CLI needs. All three exist; between them they cover every agent CLI the
//! catalogue has met, which is what makes a new CLI a data change.
//!
//! The shape here is a trait plus one factory, rather than an enum the callers
//! match on. Both dispatch fine; the difference is where the knowledge sits. An
//! enum would put a `match` at every call site in the TUI and the launch flow,
//! and adding ACP would mean editing all of them -- which is how "the catalogue
//! decides" quietly becomes "the code decides" again. With a trait, `start` is
//! the single place that knows the driver set exists, and everything
//! downstream holds a `Box<dyn PmTransport>` it cannot branch on.
//!
//! ## Using it
//!
//! ```ignore
//! let mut pm = pm::start(entry, &mcp_args)?;   // spawns the CLI
//! pm.send_user("You are the mana PM for this session.")?;
//! while let Ok(event) = pm.events().recv() {
//!     match event {
//!         PmEvent::Text(text) => chat.push(text),
//!         PmEvent::Usage(usage) => log.enrich(usage),
//!         PmEvent::Raw(line) => chat.push_degraded(line),
//!         PmEvent::PermissionRequest { id, options, .. } => {
//!             pm.answer_permission(id, &options[0].id)?
//!         }
//!         PmEvent::TurnEnded => queue.release_one(&mut pm)?,
//!         PmEvent::Exited { code } => break,
//!     }
//! }
//! pm.shutdown()?;
//! ```

mod acp;
mod child;
mod events;
mod oneshot;
mod stream;

pub use acp::AcpDriver;
pub use oneshot::OneshotDriver;
pub use stream::StreamDriver;

use crate::catalog::{CliEntry, PmDriver};
use anyhow::{Result, bail};
use std::sync::mpsc::Receiver;

/// Everything mana is willing to learn from a PM session.
///
/// Six variants and no more, because the contract is thin on purpose (design
/// §4): tool calls and session control travel over MCP, ACP or the sentinel
/// channel, never parsed out of a CLI's proprietary stream. That is the lesson
/// vibe-kanban paid for -- parsing whole streams forces one Rust module per
/// CLI.
///
/// `PermissionRequest` is the first addition ACP forced (task 3.1), and
/// `TurnEnded` is the second; both earn their place for the same reason the
/// others do: they are *typed protocol*, not something scraped out of a
/// stream. Every driver already holds the fact -- a process that exited, an
/// ACP response, a frame the catalogue names -- and none of them has to read a
/// word of what the PM said to produce it. A CLI with no permission protocol
/// simply never sends the first.
#[derive(Debug, Clone, PartialEq)]
pub enum PmEvent {
    /// Assistant prose, ready to render in the chat pane. One event per match:
    /// a single frame may carry several text blocks, and a chunked transport
    /// coalesces its chunks into whole lines before emitting them.
    Text(String),
    /// A usage snapshot exactly as the CLI reported it, for log enrichment.
    /// Opaque JSON: every CLI counts differently and mana only records.
    Usage(serde_json::Value),
    /// Something outside the thin contract, shown rather than dropped. On a
    /// stream transport that is a line no path matched or that was not JSON at
    /// all; on ACP it is also activity mana understood but that is not the PM
    /// talking (a tool call, a thought, a turn that ended badly). The visible
    /// half of "degraded, never silent": a CLI that changes shape shows up as
    /// ugly lines in the chat pane, not as a PM that went quiet.
    Raw(String),
    /// The PM is waiting on a human decision before it can act (ACP's
    /// `session/request_permission`). Answer it with
    /// `PmTransport::answer_permission`; leaving it unanswered stalls the PM's
    /// turn but never mana, which keeps reading either way.
    PermissionRequest {
        /// Answers this request, and only this one. Opaque on purpose: the
        /// transport's own request id may be a number or a string and nothing
        /// above this module should have to know which.
        id: u64,
        /// What the PM wants to do, in one line.
        description: String,
        /// Never empty -- a request nobody could answer is refused by the
        /// transport instead of surfaced.
        options: Vec<PermissionChoice>,
    },
    /// The PM finished the turn it was on and is listening again.
    ///
    /// Not a display event: nothing renders it. It exists so mana can tell a
    /// PM that is mid-answer from one that is idle, which is what makes the
    /// input queue possible -- a line typed into a busy PM used to go straight
    /// down the transport and land in the middle of its turn.
    ///
    /// Arrives once per turn on the transports that can say so, and never at
    /// all on a `stream` entry whose catalogue declares no
    /// `[pm.events].turn_end` (see `PmTransport::tracks_turn_end`). A turn that
    /// ended badly still ends: the queue must drain even when the PM refused,
    /// ran out of tokens or errored.
    TurnEnded,
    /// The PM process is gone. Always arrives, exactly once, last -- v1 could
    /// not tell a thinking PM from a dead one and waited forever on both.
    /// `None` means the process was signalled rather than exited.
    Exited { code: Option<i32> },
}

/// One answer the PM offered to a permission request.
///
/// `allows` is what lets the interface bind one key to yes and one to no
/// however many options an agent lists: the protocol says which way each
/// option goes, so mana does not have to read the label to find out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChoice {
    pub id: String,
    pub label: String,
    pub allows: bool,
}

/// What every PM transport owes its caller: send a turn, read events, end the
/// session. Deliberately small -- anything wider would be a place for per-CLI
/// behaviour to reappear.
pub trait PmTransport: Send {
    /// Sends one user turn. Errors when the session is already closed or the
    /// process died between turns.
    fn send_user(&mut self, text: &str) -> Result<()>;

    /// The session's event stream, borrowed so the transport stays the single
    /// owner of its process. `recv`/`try_recv` take `&self`, so nothing is lost.
    fn events(&self) -> &Receiver<PmEvent>;

    /// Answers a `PermissionRequest` with one of the option ids it carried.
    ///
    /// Defaulted rather than required: a transport that never asks has nothing
    /// to answer, and making every driver write an unreachable arm would be
    /// ceremony. The default is loud instead of silent because reaching it
    /// means the interface offered a decision the transport cannot deliver.
    fn answer_permission(&mut self, id: u64, option_id: &str) -> Result<()> {
        let _ = option_id;
        bail!("this PM transport never asks for permission, so there is nothing to answer ({id})")
    }

    /// Whether this session reports `TurnEnded`, and so whether anything above
    /// may hold a turn back until the PM is free.
    ///
    /// True for every transport that reads the boundary off its own protocol
    /// (`acp`, `oneshot-continue`) and for a `stream` entry whose catalogue
    /// names the closing frame. False is the honest answer for a `stream`
    /// entry that does not: mana would otherwise open a turn that nothing ever
    /// closes and queue the session into silence. The caller degrades to v2.1
    /// behaviour -- every turn goes out at once -- and says so.
    fn tracks_turn_end(&self) -> bool {
        true
    }

    /// The id this conversation can be reopened by, when the transport's
    /// protocol has one.
    ///
    /// `None` on the two drivers whose CLI keys its conversation store by
    /// working directory (`stream`, `oneshot-continue`): there is no
    /// identifier to store, and inventing one would be a promise mana cannot
    /// keep. Only ACP hands out a session id, and only ACP takes one back
    /// (`session/load`), so the launch flow stores whatever this returns and
    /// hands it to `Resume` next time.
    fn session_id(&self) -> Option<&str> {
        None
    }

    /// Ends the session and guarantees `Exited` is queued before returning.
    fn shutdown(&mut self) -> Result<()>;
}

/// "Pick the conversation back up" -- what `mana launch --continue` asks the
/// driver for.
///
/// One type rather than a bare `bool` because the three drivers need different
/// amounts of it, and the difference is not the caller's business: `stream`
/// appends `[pm].resume_args`, `oneshot-continue` starts its first turn from
/// `[pm].continue_args`, and `acp` replays a session id mana recorded when the
/// last one ended -- which is the only fact that has to travel, so it is the
/// only field. A driver whose CLI cannot resume refuses the launch instead of
/// quietly opening a fresh session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resume<'a> {
    /// The session the last ACP run ended with, when mana stored one. `None`
    /// on the other drivers, which resume from the CLI's own store keyed by
    /// the working directory, and on an ACP project mana has never recorded.
    pub session_id: Option<&'a str>,
}

/// Starts the PM session `entry` describes, appending `extra_args` (the tool
/// channel's already-substituted flags) after the catalogue's own.
/// `resume` is `None` for a fresh conversation.
///
/// The one place in mana that knows more than one driver exists.
pub fn start(
    entry: &CliEntry,
    extra_args: &[String],
    resume: Option<Resume<'_>>,
) -> Result<Box<dyn PmTransport>> {
    match entry.pm.driver {
        PmDriver::Acp => Ok(Box::new(AcpDriver::start(entry, extra_args, resume)?)),
        PmDriver::Stream => Ok(Box::new(StreamDriver::start(entry, extra_args, resume)?)),
        PmDriver::OneshotContinue => Ok(Box::new(OneshotDriver::start(entry, extra_args, resume)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(driver: &str) -> CliEntry {
        events::fixture::parse(
            &events::fixture::source("no-such-binary", &[], "stdin-jsonl", "$.text", None)
                .replace(r#"driver = "stream""#, &format!("driver = {driver:?}"))
                // A oneshot entry is invalid without its per-turn argv.
                .replace(
                    r#"prompt = "stdin-jsonl""#,
                    "first_args = [\"--print\"]\ncontinue_args = [\"--continue\"]\n\
                     prompt = \"stdin-jsonl\"",
                ),
        )
    }

    /// An ACP entry carries neither of the fields a stream entry needs, so it
    /// cannot be built by patching one -- which is the catalogue rule working.
    fn acp_entry() -> CliEntry {
        events::fixture::parse(
            &events::fixture::source("no-such-binary", &[], "stdin-jsonl", "$.text", None)
                .replace(r#"driver = "stream""#, r#"driver = "acp""#)
                .replace("[pm.events]\ntext = \"$.text\"\n", "")
                .replace("prompt = \"stdin-jsonl\"\n", ""),
        )
    }

    /// `unwrap_err` would need `Debug` on a boxed live session, which is not
    /// worth requiring of every driver for the sake of a test message.
    fn start_err(entry: &CliEntry) -> String {
        match start(entry, &[], None) {
            Ok(_) => panic!("a session started that should have been refused"),
            Err(err) => format!("{err:#}"),
        }
    }

    /// The factory reaches every driver in the set, and each one then fails on
    /// its own terms (there is no such binary) rather than on the driver being
    /// unknown. With 3.2 the set is closed: no `[pm].driver` value the
    /// catalogue accepts is left without a transport.
    #[test]
    fn every_driver_is_reached_and_fails_on_its_own_terms() {
        for entry in [
            entry_with("stream"),
            entry_with("oneshot-continue"),
            acp_entry(),
        ] {
            let rendered = start_err(&entry);
            assert!(rendered.contains("no-such-binary"), "{rendered}");
        }
    }
}
