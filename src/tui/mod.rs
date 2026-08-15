//! mana's own interface: a chat pane fed by structured PM events, a graph
//! pane fed by the dispatch files, and nothing else on screen.
//!
//! The v1 pane mirrored the PM's PTY, which meant re-rendering another
//! program's screen faithfully -- the opposite of the goal (design §3). Here
//! mana consumes events and *chooses* what to show: assistant prose, its own
//! notices, and raw lines only when a stream stops matching its catalogue.

pub mod app;
pub mod event;
pub mod graph;
pub mod render;
