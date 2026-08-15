//! PM transport drivers: the one interactive session mana holds with an agent
//! CLI acting as project manager.
//!
//! Three transports are in the design (§4) -- `stream`, `acp`,
//! `oneshot-continue` -- and the catalogue's `[pm].driver` field says which one
//! a CLI needs. Only `stream` exists so far; the other two land in phase 3.
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
//!         PmEvent::Exited { code } => break,
//!     }
//! }
//! pm.shutdown()?;
//! ```
#![allow(dead_code)] // Consumers land with the launch flow (2.3) and the TUI (2.4).

mod events;
mod stream;

pub use stream::StreamDriver;

use crate::catalog::{CliEntry, PmDriver};
use anyhow::{Result, bail};
use std::sync::mpsc::Receiver;

/// Everything mana is willing to learn from a PM session.
///
/// Four variants and no more, because the contract is thin on purpose (design
/// §4): tool calls, permissions and session control travel over MCP, ACP or the
/// sentinel channel, never parsed out of a CLI's proprietary stream. That is
/// the lesson vibe-kanban paid for -- parsing whole streams forces one Rust
/// module per CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum PmEvent {
    /// Assistant prose, ready to render in the chat pane. One event per match:
    /// a single frame may carry several text blocks.
    Text(String),
    /// A usage snapshot exactly as the CLI reported it, for log enrichment.
    /// Opaque JSON: every CLI counts differently and mana only records.
    Usage(serde_json::Value),
    /// A line no path matched, or that was not JSON at all. The visible half of
    /// "degraded, never silent": a CLI that changes its stream shape shows up
    /// as ugly lines in the chat pane, not as a PM that went quiet.
    Raw(String),
    /// The PM process is gone. Always arrives, exactly once, last -- v1 could
    /// not tell a thinking PM from a dead one and waited forever on both.
    /// `None` means the process was signalled rather than exited.
    Exited { code: Option<i32> },
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

    /// Ends the session and guarantees `Exited` is queued before returning.
    fn shutdown(&mut self) -> Result<()>;
}

/// Starts the PM session `entry` describes, appending `extra_args` (the tool
/// channel's already-substituted flags) after the catalogue's own.
///
/// The one place in mana that knows more than one driver exists.
pub fn start(entry: &CliEntry, extra_args: &[String]) -> Result<Box<dyn PmTransport>> {
    match entry.pm.driver {
        PmDriver::Stream => Ok(Box::new(StreamDriver::start(entry, extra_args)?)),
        // Not a gap in the catalogue but a transport mana cannot speak yet.
        // Naming the task keeps the message useful to whoever hits it.
        PmDriver::Acp => bail!(
            "{}: [pm].driver is 'acp', which mana cannot speak yet (mana v2, task 3.1)",
            entry.cli.id
        ),
        PmDriver::OneshotContinue => bail!(
            "{}: [pm].driver is 'oneshot-continue', which mana cannot speak yet (mana v2, task 3.2)",
            entry.cli.id
        ),
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

    /// `unwrap_err` would need `Debug` on a boxed live session, which is not
    /// worth requiring of every driver for the sake of a test message.
    fn start_err(entry: &CliEntry) -> String {
        match start(entry, &[]) {
            Ok(_) => panic!("a session started that should have been refused"),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn an_unimplemented_driver_says_so_and_names_the_task() {
        for (driver, task) in [("acp", "3.1"), ("oneshot-continue", "3.2")] {
            let rendered = start_err(&entry_with(driver));
            assert!(rendered.contains(driver), "{rendered}");
            assert!(rendered.contains(task), "{rendered}");
        }
    }

    /// The factory reaches the stream driver, which then fails on its own terms
    /// (there is no such binary) rather than on the driver being unknown.
    #[test]
    fn a_stream_entry_reaches_the_stream_driver() {
        let rendered = start_err(&entry_with("stream"));
        assert!(rendered.contains("no-such-binary"), "{rendered}");
    }
}
