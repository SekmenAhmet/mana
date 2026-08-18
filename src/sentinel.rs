//! The `sentinel` tool channel: the PM writes a fenced block, mana executes it.
//!
//! Design §5 gives a CLI two ways to reach mana's four orchestration tools. The
//! first is MCP, and it is the one to want. The second is this: for a CLI with
//! no MCP surface at all (agy, measured 2026-08-15), the PM emits a fenced
//! ```` ```mana ```` block in its own text, mana finds it in the *structured
//! event stream*, executes it, and injects the result back as the next user
//! turn.
//!
//! ## Why this is not v1's `Bash(...)` scraping
//!
//! v1 watched a rendered terminal for command lines and re-ran what it found,
//! which double-launched every sub-agent: the agent's own execution had already
//! happened, and mana's copy was a second one. Two things are different here.
//! The text is read from the CLI's structured output (`[pm.events].text`), not
//! from a screen full of box-drawing characters. And the block is **inert**: it
//! is JSON in a message, it executes nowhere else, and mana is the sole actor.
//! A PM that writes the same block twice gets two tool calls because it asked
//! for two, which is exactly what an MCP client would get.
//!
//! ## The format, and who owns it
//!
//! `assets/roles/pm/SKILL.md` teaches the PM one shape and this module parses
//! that shape:
//!
//! ```text
//! ```mana:<nonce>
//! {"tool": "create_task", "args": {"title": "...", "prompt": "..."}}
//! ```
//! ```
//!
//! `args` is validated against the same `Deserialize` types the MCP server
//! exposes and executed through the same `*_impl` methods, so a sentinel call
//! and an MCP call cannot drift: there is one implementation, one validator and
//! one set of error messages. Anything malformed comes back as **one**
//! corrective message naming what was wrong -- the PM is told once and moves
//! on, rather than being argued with turn after turn.
//!
//! ## Why the info string carries a nonce (#140)
//!
//! Without it, *the PM decided to run this* and *the PM is showing you
//! something it read* are the same bytes in the same position, and mana ran
//! both. Any text a PM can be induced to quote -- an issue body written by a
//! stranger, a README, a dependency's changelog, a log line -- was therefore a
//! command channel the moment the PM repeated it, which is an ordinary thing
//! for a PM to do while explaining what it found. The mitigation shipped in
//! #47 was a sentence in the skill telling the PM never to reproduce such a
//! block: that is compliance from the same model the attacker is talking to,
//! not enforcement.
//!
//! So the fence carries a secret the quoted text cannot: a token minted per
//! `Sentinel` (i.e. per session), handed to the PM in its activation turn and
//! written nowhere else -- not to `state.toml`, not into the skill file, not
//! into a log. Quoted content is from disk or from another session, so it
//! cannot carry this session's token, and a fence without it is left in the
//! prose exactly as written.
//!
//! The alternative considered was a per-turn "no commands here" mode the PM
//! opts into before quoting. It costs the same to implement and fails the
//! other way round: forgetting to enter it executes the quote, whereas
//! forgetting the nonce merely fails to run a call the PM meant -- which the
//! corrective below makes recoverable.

use crate::catalog::Catalog;
use crate::mcp::ManaTools;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

/// The tools a fenced block may name. Spelled out so an unknown name can be
/// refused with the list of real ones -- a model that invented `delete_task`
/// needs to see what does exist, not just that it was wrong.
const TOOLS: [&str; 4] = [
    "create_task",
    "launch_subagent",
    "get_review",
    "list_agents",
];

/// The shortest fence the PM may open -- a longer run of backticks opens one
/// too, see `fence`. Anything else fenced (` ```json `, ` ``` `) is the PM's
/// own prose and is skipped whole, so a bare `mana` block quoted inside a
/// markdown example never fires.
const FENCE: &str = "```";
const LANGUAGE: &str = "mana";

/// mana's half of the sentinel channel: the parser and the sole executor.
pub struct Sentinel {
    tools: ManaTools,
    /// This session's token, and the whole of the authorization: a fence is a
    /// call because it carries this, and nothing else about the block matters.
    /// Kept in memory only -- persisting it anywhere would put it in reach of
    /// the quoted files it exists to disarm.
    nonce: String,
}

/// What one PM message produced.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    /// The message with every `mana` block taken out -- what the chat pane
    /// renders as the PM talking. Blocks are machinery, not conversation.
    pub prose: String,
    /// One compact line per block, so the operator sees tool activity without
    /// a wall of JSON.
    pub log: Vec<ToolLine>,
    /// The turn to inject, or `None` when the message carried no block at all.
    pub reply: Option<String>,
}

/// One line of tool activity, and how loudly it has to be said.
///
/// A call that worked is annotation: the same `⚙ name ✓` an ACP agent's tool
/// call renders as, and it collapses with the rest of the machinery under the
/// interface's raw view. A call that did *not* work is news -- mana refused
/// the block, or the tool returned an error, and the PM is about to act on
/// whatever it was told -- so it keeps mana's own voice and stays on screen.
#[derive(Debug, PartialEq)]
pub struct ToolLine {
    pub text: String,
    pub failed: bool,
}

impl Sentinel {
    pub fn new(project_root: &Path, mana_home: &Path, catalog: Catalog) -> Self {
        Sentinel {
            tools: ManaTools::new(project_root.to_path_buf(), mana_home.to_path_buf(), catalog),
            // The same generator that mints task and agent ids, in its
            // hyphen-free spelling so the fence info string is one word. A v4
            // UUID is 122 unguessable bits, which is far more than a token
            // that only has to survive one session needs -- and reusing what
            // is already in the tree beats reasoning about a second one.
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    /// The token this session's fences must carry. Read by the launch, which
    /// is the only thing that may tell the PM what it is.
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// Scans one PM message, executes every block it carries, and reports what
    /// to render and what to send back.
    pub fn handle(&self, text: &str) -> Outcome {
        let scanned = scan(text, &self.nonce);
        // A `mana` fence without this session's nonce stays in the prose, and
        // the PM hears about it only when the block *would* have been a call.
        // That asymmetry is the point: a quoted block is usually quoted
        // because it is a real call somebody else wrote, so answering every
        // one of them would hand injected text a reply channel and teach the
        // PM to argue with documentation it merely read. A block that parses
        // is the one case where staying silent costs something -- a PM that
        // meant to dispatch and got nothing back would wait for a result no
        // one is coming to send.
        let misfenced = scanned.quoted.iter().any(|body| parse(body).is_ok());
        if scanned.blocks.is_empty() && !misfenced {
            return Outcome {
                prose: scanned.prose,
                log: Vec::new(),
                reply: None,
            };
        }
        let blocks = scanned.blocks;
        let mut log = Vec::with_capacity(blocks.len());
        let mut results = Vec::with_capacity(blocks.len());
        for block in &blocks {
            let (line, result) = match parse(block) {
                Ok(call) => {
                    let outcome = self.execute(&call);
                    (call.log_line(&outcome), call.result_line(&outcome))
                }
                Err(reason) => (
                    ToolLine {
                        text: format!("[mana] block not executed: {reason}"),
                        failed: true,
                    },
                    format!("not executed: {reason} {}", CORRECTION),
                ),
            };
            log.push(line);
            results.push(result);
        }
        let mut reply = (!results.is_empty()).then(|| reply(&results));
        if misfenced {
            // Said out loud in the pane rather than collapsed with the routine
            // activity: "mana declined to run a block" is either the operator's
            // PM making a mistake or something in the project trying to reach
            // these tools, and both are news.
            log.push(ToolLine {
                text: "[mana] a ```mana block without this session's nonce was left as prose"
                    .to_string(),
                failed: true,
            });
            // Appended after the results rather than folded into them: those
            // are numbered answers to calls the PM made, and this is about a
            // call it did not manage to make. The reply also has to escape
            // `reply`'s closing "do not repeat the calls above" -- here
            // repeating the block, correctly fenced, is exactly the fix.
            reply = Some(match reply {
                Some(results) => format!("{results}\n{MISFENCED}"),
                None => MISFENCED.to_string(),
            });
        }
        Outcome {
            prose: scanned.prose,
            log,
            reply,
        }
    }

    /// Runs one call through the same implementation the MCP server calls.
    ///
    /// Every arm deserializes into the type the tool's schema is generated
    /// from, so the parameters a sentinel PM may send are exactly the
    /// parameters an MCP PM may send -- there is no second validator here to
    /// disagree with the first.
    fn execute(&self, call: &Call) -> Result<Value, String> {
        match call.tool.as_str() {
            "create_task" => json(self.tools.create_task_impl(call.args()?)),
            "launch_subagent" => json(self.tools.launch_subagent_impl(call.args()?)),
            "get_review" => json(self.tools.get_review_impl(call.args()?)),
            "list_agents" => json(self.tools.list_agents_impl()),
            // Unreachable: `parse` refuses an unknown name before this point.
            other => Err(format!("unknown tool {other:?}")),
        }
    }
}

/// One parsed block.
#[derive(Debug, PartialEq)]
struct Call {
    tool: String,
    args: Value,
}

impl Call {
    /// The parameters, in the tool's own type. The error names the tool and
    /// quotes serde, which is what says *which* field was missing or misspelt.
    fn args<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_value(self.args.clone())
            .map_err(|error| format!("invalid arguments for {}: {error}", self.tool))
    }

    /// The compact chat line: the tool, and how it went.
    ///
    /// A call that worked is one glyph and a name -- the same shape the ACP
    /// driver renders a tool call as, because it is the same event and the
    /// operator should not have to learn two vocabularies for it. A call that
    /// failed gets mana's voice, a glance at what was sent and the error
    /// itself: that line has to be actionable, and arguments are trimmed
    /// because the operator is reading a conversation, not a request log.
    fn log_line(&self, outcome: &Result<Value, String>) -> ToolLine {
        match outcome {
            Ok(_) => ToolLine {
                text: format!("⚙ {} ✓", self.tool),
                failed: false,
            },
            Err(error) => {
                let args = match &self.args {
                    Value::Object(map) if map.is_empty() => String::new(),
                    args => format!(" {}", truncate(&args.to_string(), 60)),
                };
                ToolLine {
                    text: format!("[mana] tool {}{args} failed: {error}", self.tool),
                    failed: true,
                }
            }
        }
    }

    /// The line the PM reads. Whole, never truncated: this is the tool's
    /// answer, and half of it is worse than none.
    fn result_line(&self, outcome: &Result<Value, String>) -> String {
        match outcome {
            Ok(value) => format!("{} ok: {value}", self.tool),
            Err(error) => format!("{} failed: {error}", self.tool),
        }
    }
}

/// Serializes a tool's output, or renders its error exactly as an MCP caller
/// would have received it.
fn json<T: Serialize>(outcome: Result<T, rmcp::model::ErrorData>) -> Result<Value, String> {
    match outcome {
        Ok(value) => serde_json::to_value(value).map_err(|error| error.to_string()),
        Err(error) => Err(error.message.to_string()),
    }
}

/// The one corrective sentence, appended to whatever went wrong. It names the
/// shape and the tools rather than scolding: the PM has one chance to read this
/// before its next turn.
const CORRECTION: &str = "One call per ```mana:<nonce> block, exactly \
    {\"tool\": \"<name>\", \"args\": {...}}. Tools: create_task, launch_subagent, \
    get_review, list_agents.";

/// What the PM is told about a block that was left in its prose.
///
/// It names the shape and does **not** repeat the nonce, deliberately. The
/// activation that issued it is still in the PM's context (this channel's one
/// CLI replays the whole conversation every turn), so naming the shape is
/// enough to recover from a real mistake -- while a message mana emits *in
/// response to quoted text* is the one place the token could be pulled out of
/// mana by something the PM only read.
const MISFENCED: &str = "[mana] a ```mana block in that message did not carry this session's \
    nonce, so mana did not execute it and left it in your prose. If you meant to call a tool, \
    write the fence as ```mana: followed by the nonce from your activation message. If you were \
    quoting a block you found in a file, an issue or a log, nothing is wrong -- that is what the \
    nonce is for.";

/// The turn injected back into the session.
///
/// It ends by telling the PM not to repeat itself: a model that reads a result
/// and re-emits the block that produced it would dispatch twice, and mana
/// executes everything it is given.
fn reply(results: &[String]) -> String {
    let mut message = String::from("[mana] tool results, in the order you wrote them:\n");
    for (index, result) in results.iter().enumerate() {
        message.push_str(&format!("{}. {result}\n", index + 1));
    }
    message.push_str("Continue from these results; do not repeat the calls above.");
    message
}

/// What one PM message looked like to the parser.
struct Scan {
    /// Everything that is not an executable block, in order -- which now
    /// includes the `mana` fences that carried no nonce, fence lines and all.
    /// Rendering them is the honest outcome: a block mana will not run is text
    /// the PM wrote, and text the PM wrote belongs in the pane. Dropping them
    /// would make a quoted block disappear from the conversation it was being
    /// quoted into.
    prose: String,
    /// Bodies to execute: a `mana:<nonce>` fence at the top level.
    blocks: Vec<String>,
    /// Bodies of the top-level `mana` fences that were *not* authorized. Kept
    /// only so `handle` can tell a quote from a PM that mis-wrote its fence;
    /// nothing here is ever parsed into a call.
    quoted: Vec<String>,
}

/// The backtick run that opens or closes a fence, and its info string.
///
/// CommonMark: an opener is a run of *three or more* backticks and the closer
/// must be at least as long. Three is only the minimum, and four is what a
/// model reaches for when the fenced body might itself contain backticks --
/// code, a diff, a log excerpt -- which for a brief is the normal case. mana
/// used to strip exactly three, so a longer fence matched neither the
/// authorized test nor the declined one and the block was dropped with no run
/// and no corrective (#190).
fn fence(trimmed: &str) -> Option<(usize, &str)> {
    let info = trimmed.trim_start_matches('`');
    let run = trimmed.len() - info.len();
    (run >= FENCE.len()).then_some((run, info.trim()))
}

/// Splits a PM message into the prose to render, the bodies of its authorized
/// `mana` blocks, and the bodies of the ones it declined.
///
/// Line-based, and it tracks the fence it is inside: a *bare* `mana` block
/// quoted inside a ` ```markdown ` example belongs to the PM's prose. The one
/// thing nesting does not override is the nonce (#140), which is the whole of
/// the authorization -- an authorized fence is a call wherever it stands, so a
/// fence the PM left unbalanced cannot swallow the call after it (#156).
fn scan(text: &str, nonce: &str) -> Scan {
    let authorized = format!("{LANGUAGE}:{nonce}");
    let mut prose: Vec<&str> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut quoted: Vec<String> = Vec::new();
    let mut body: Option<Vec<&str>> = None;
    // The backtick run that opened `body`, since the closer must match it.
    let mut open = 0;
    // `Some(run)` while inside a fence that is not a call, carrying the run
    // that opened it for the same reason.
    let mut other_fence: Option<usize> = None;
    // `Some` while inside a `mana` fence that is being rendered rather than
    // run, collecting what it would have been.
    let mut declined: Option<Vec<&str>> = None;

    for line in text.lines() {
        match (&mut body, fence(line.trim())) {
            // Inside a mana block: the first fence at least as long as the one
            // that opened it ends it. A block whose body contains such a fence
            // therefore ends early and comes back as malformed JSON, which is
            // a corrective message rather than a silently truncated tool call.
            (Some(collected), Some((run, _))) if run >= open => {
                blocks.push(collected.join("\n"));
                body = None;
            }
            (Some(collected), _) => collected.push(line),
            // An authorized fence is a call wherever it stands, including
            // inside a fence the PM opened and never closed -- which used to
            // consume it as its own closer and drop the dispatch in silence
            // (#156). Since #140 the nesting guard below only stops the PM
            // quoting *this session's* nonce, which nothing it reads can
            // carry and its skill already forbids: cheaper to give up than a
            // call that vanishes.
            (None, Some((run, info))) if info == authorized => {
                open = run;
                body = Some(Vec::new());
            }
            (None, fenced) => match (other_fence, fenced) {
                (Some(opened), Some((run, _))) if run >= opened => {
                    prose.push(line);
                    other_fence = None;
                    if let Some(collected) = declined.take() {
                        quoted.push(collected.join("\n"));
                    }
                }
                (Some(_), _) => {
                    prose.push(line);
                    if let Some(collected) = &mut declined {
                        collected.push(line);
                    }
                }
                // Every fence that is not authorized is skipped whole, and a
                // `mana` one is skipped *while being watched*: it is either
                // the PM forgetting its nonce or content it copied, and only
                // its body can tell those apart.
                (None, Some((run, info))) => {
                    declined = info
                        .strip_prefix(LANGUAGE)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
                        .then(Vec::new);
                    other_fence = Some(run);
                    prose.push(line);
                }
                (None, None) => prose.push(line),
            },
        }
    }
    // A block the PM never closed: the CLI truncated the turn, or the model
    // stopped mid-write. Kept rather than dropped so it fails as malformed
    // JSON and the PM is told, instead of the call vanishing.
    if let Some(collected) = body {
        blocks.push(collected.join("\n"));
    }
    if let Some(collected) = declined {
        quoted.push(collected.join("\n"));
    }
    Scan {
        prose: prose.join("\n").trim().to_string(),
        blocks,
        quoted,
    }
}

/// Turns one block body into a call, or into the reason it is not one.
fn parse(body: &str) -> Result<Call, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("that block is not valid JSON ({error})."))?;
    let Some(object) = value.as_object() else {
        return Err("that block is JSON but not an object.".to_string());
    };
    let Some(tool) = object.get("tool").and_then(Value::as_str) else {
        return Err("that block has no \"tool\" name.".to_string());
    };
    if !TOOLS.contains(&tool) {
        return Err(format!("there is no tool called {tool:?}."));
    }
    let args = match object.get("args") {
        // Absent is legitimate: `list_agents` takes none, and a tool that
        // needs some says which through its own deserializer below.
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(_) => return Err(format!("the \"args\" of {tool:?} must be an object.")),
    };
    Ok(Call {
        tool: tool.to_string(),
        args,
    })
}

/// Shortens a value for the chat pane, on a character boundary.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{ProjectPaths, resolve_project_paths};

    struct Fixture {
        _tmp: tempfile::TempDir,
        sentinel: Sentinel,
        tools: ManaTools,
        paths: ProjectPaths,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("demo");
            let home = tmp.path().join("mana-home");
            std::fs::create_dir_all(&project).unwrap();
            let catalog = Catalog::embedded().expect("the shipped catalogue must parse");
            let paths =
                resolve_project_paths(&home, &crate::project::project_name_from_dir(&project));
            Fixture {
                sentinel: Sentinel::new(&project, &home, catalog.clone()),
                // A second handle on the same state, standing in for the MCP
                // server: same project, same home, same catalogue.
                tools: ManaTools::new(project, home.clone(), catalog),
                paths,
                _tmp: tmp,
            }
        }
    }

    impl Fixture {
        /// A block fenced the way this session authorizes. Every test that
        /// means "the PM called a tool" goes through here, so the day the
        /// fence shape changes there is one place to change.
        fn block(&self, body: &str) -> String {
            format!("```mana:{}\n{body}\n```", self.sentinel.nonce())
        }
    }

    #[test]
    fn a_message_with_no_block_is_left_alone_and_answers_nothing() {
        let outcome = Sentinel::new(
            Path::new("/nowhere"),
            Path::new("/nowhere"),
            Catalog::embedded().unwrap(),
        )
        .handle("I'll start by breaking this into two tasks.");
        assert_eq!(outcome.prose, "I'll start by breaking this into two tasks.");
        assert!(outcome.log.is_empty());
        assert_eq!(outcome.reply, None);
    }

    #[test]
    fn a_valid_block_is_executed_and_its_result_comes_back_as_a_turn() {
        let fixture = Fixture::new();
        let outcome = fixture.sentinel.handle(&format!(
            "Listing what is installed.\n{}",
            fixture.block(r#"{"tool": "list_agents"}"#)
        ));

        assert_eq!(outcome.prose, "Listing what is installed.");
        // The same shape an ACP agent's tool call renders as: one glyph, the
        // tool's name, and how it went. It is the same event.
        assert_eq!(
            outcome.log,
            [ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false
            }]
        );
        let reply = outcome.reply.unwrap();
        assert!(reply.starts_with("[mana] tool results"), "{reply}");
        assert!(
            reply.contains("1. list_agents ok: {\"agents\":["),
            "{reply}"
        );
        assert!(reply.contains("do not repeat"), "{reply}");
    }

    /// The skill tells the PM to dispatch independent tasks in one turn, so
    /// several blocks in one message is the normal case, not an edge case.
    #[test]
    fn every_block_in_one_message_runs_in_the_order_it_was_written() {
        let fixture = Fixture::new();
        let outcome = fixture.sentinel.handle(&format!(
            "Two tasks.\n{}\nand\n{}",
            fixture
                .block(r#"{"tool": "create_task", "args": {"title": "One", "prompt": "do one"}}"#),
            fixture
                .block(r#"{"tool": "create_task", "args": {"title": "Two", "prompt": "do two"}}"#),
        ));

        assert_eq!(outcome.prose, "Two tasks.\nand");
        assert_eq!(outcome.log.len(), 2);
        // Two calls, two identical lines: which task each one made is in the
        // PM's own prose and in the reply it was handed, and repeating the
        // arguments in the pane is the wall of JSON this line exists to avoid.
        assert_eq!(outcome.log[0].text, "⚙ create_task ✓");
        assert!(!outcome.log[0].failed);
        let reply = outcome.reply.unwrap();
        assert!(
            reply.contains("1. create_task ok: {\"task_id\":"),
            "{reply}"
        );
        assert!(
            reply.contains("2. create_task ok: {\"task_id\":"),
            "{reply}"
        );
        // Two calls, two task files: the executor is not deduplicating.
        assert_eq!(std::fs::read_dir(&fixture.paths.tasks).unwrap().count(), 2);
    }

    #[test]
    fn a_block_that_is_not_json_comes_back_as_one_corrective_message() {
        let fixture = Fixture::new();
        let outcome = fixture
            .sentinel
            .handle(&fixture.block("create_task(title=\"One\")"));

        assert_eq!(outcome.log.len(), 1);
        // A block mana refused is news: it stays on screen rather than
        // collapsing with the routine activity.
        assert!(
            outcome.log[0].text.contains("not executed"),
            "{:?}",
            outcome.log
        );
        assert!(outcome.log[0].failed);
        let reply = outcome.reply.unwrap();
        assert!(reply.contains("not valid JSON"), "{reply}");
        assert!(
            reply.contains("One call per ```mana:<nonce> block"),
            "{reply}"
        );
        assert!(reply.contains("create_task, launch_subagent"), "{reply}");
    }

    #[test]
    fn a_block_naming_a_tool_that_does_not_exist_says_which_ones_do() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&fixture.block(r#"{"tool": "delete_task", "args": {}}"#))
            .reply
            .unwrap();
        assert!(reply.contains("no tool called \"delete_task\""), "{reply}");
        assert!(reply.contains("list_agents"), "{reply}");
    }

    #[test]
    fn a_block_missing_a_required_argument_is_refused_by_the_tools_own_schema() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&fixture.block(r#"{"tool": "create_task", "args": {"title": "One"}}"#))
            .reply
            .unwrap();
        assert!(
            reply.contains("invalid arguments for create_task"),
            "{reply}"
        );
        assert!(reply.contains("prompt"), "{reply}");
    }

    #[test]
    fn a_block_whose_args_are_not_an_object_is_refused() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&fixture.block(r#"{"tool": "get_review", "args": "task-1"}"#))
            .reply
            .unwrap();
        assert!(reply.contains("must be an object"), "{reply}");
    }

    /// #156: a fence the PM left unbalanced must not swallow the call that
    /// follows it. An authorized fence is a call wherever it appears, and a
    /// dispatch that vanishes leaves the PM waiting for a result nobody sends.
    #[test]
    fn a_block_after_an_unbalanced_fence_is_still_executed() {
        let fixture = Fixture::new();
        let outcome = fixture.sentinel.handle(&format!(
            "For example:\n```json\n{{\"a\": 1}}\n{}",
            fixture.block(r#"{"tool": "list_agents"}"#)
        ));

        assert_eq!(
            outcome.log,
            [ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false
            }]
        );
        let reply = outcome
            .reply
            .expect("the call after the stray fence must be answered");
        assert!(
            reply.contains("1. list_agents ok: {\"agents\":["),
            "{reply}"
        );
    }

    /// A *bare* `mana` block quoted inside another fence is still prose: the
    /// PM explaining the format, or reproducing something it read, fires
    /// nothing. What no longer protects it is nesting alone -- see
    /// `a_block_carrying_this_sessions_nonce_runs_inside_another_fence`.
    #[test]
    fn a_mana_block_quoted_inside_another_fence_is_prose() {
        let fixture = Fixture::new();
        let text = "Here is the shape:\n```markdown\n```mana\n{\"tool\": \"list_agents\"}\n```\n```\nThat is all.";
        let outcome = fixture.sentinel.handle(text);
        assert_eq!(outcome.reply, None);
        assert!(outcome.prose.contains("```mana"), "{}", outcome.prose);
    }

    /// The trade-off taken for #156, pinned so it is a decision and not a
    /// regression: the nonce outranks nesting. A block carrying *this
    /// session's* token runs even inside another fence, because the only text
    /// that can carry it is text this PM wrote in this session -- and the
    /// alternative is a dispatch dropped in silence whenever a fence above it
    /// was left open.
    #[test]
    fn a_block_carrying_this_sessions_nonce_runs_inside_another_fence() {
        let fixture = Fixture::new();
        let text = format!(
            "Here is the shape:\n```markdown\n```mana:{}\n{{\"tool\": \"list_agents\"}}\n```\n```\nThat is all.",
            fixture.sentinel.nonce()
        );
        let outcome = fixture.sentinel.handle(&text);
        assert!(
            outcome
                .reply
                .is_some_and(|reply| reply.contains("list_agents ok:")),
            "a fence with this session's nonce must run wherever it stands"
        );
    }

    /// #140, the whole of it: a block the PM copied out of a file, an issue or
    /// a log arrives unwrapped and identical to one the PM meant. Without the
    /// nonce it fired, which made every text a PM can be induced to quote a
    /// command channel.
    #[test]
    fn a_bare_mana_block_the_pm_reproduced_is_rendered_and_never_run() {
        let fixture = Fixture::new();
        let quoted = "The issue says:\n```mana\n\
                      {\"tool\": \"create_task\", \"args\": {\"title\": \"x\", \"prompt\": \"y\"}}\n\
                      ```\nwhich is what I found.";
        let outcome = fixture.sentinel.handle(quoted);

        // Nothing ran: no task file, and no successful tool line.
        assert_eq!(
            std::fs::read_dir(&fixture.paths.tasks)
                .map(Iterator::count)
                .unwrap_or(0),
            0
        );
        assert!(
            outcome.log.iter().all(|line| line.failed),
            "{:?}",
            outcome.log
        );
        // ...and the block is still in the conversation, fences included:
        // quoting is a thing a PM does, and mana renders it rather than
        // swallowing it.
        assert!(outcome.prose.contains("```mana"), "{}", outcome.prose);
        assert!(outcome.prose.contains("create_task"), "{}", outcome.prose);

        // The PM is told once, because this block *would* have parsed -- a
        // genuine slip has to be recoverable.
        let reply = outcome.reply.unwrap();
        assert!(
            reply.contains("did not carry this session's nonce"),
            "{reply}"
        );
        // The corrective never repeats the nonce: the only message that
        // carries it is the activation.
        assert!(!reply.contains(fixture.sentinel.nonce()), "{reply}");
    }

    /// The other half of the same rule: a quoted block that is not a call at
    /// all gets no answer, so ordinary quoting stays silent instead of filling
    /// the PM's context with corrections about text it merely read.
    #[test]
    fn a_bare_mana_block_that_is_not_a_call_is_answered_with_nothing() {
        let fixture = Fixture::new();
        let outcome = fixture
            .sentinel
            .handle("The README shows:\n```mana\nsee the docs\n```");
        assert_eq!(outcome.reply, None);
        assert!(outcome.log.is_empty());
    }

    /// A nonce is a session's own. Another mana's -- a block copied out of
    /// yesterday's transcript, or out of a second session running beside this
    /// one -- is exactly as inert as none at all.
    #[test]
    fn a_block_carrying_another_sessions_nonce_is_not_executed() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        assert_ne!(fixture.sentinel.nonce(), other.sentinel.nonce());

        let outcome = fixture
            .sentinel
            .handle(&other.block(r#"{"tool": "list_agents"}"#));
        assert!(
            outcome
                .reply
                .unwrap()
                .contains("did not carry this session's nonce"),
            "another session's nonce executed"
        );
        assert!(outcome.prose.contains(other.sentinel.nonce()));
    }

    #[test]
    fn a_json_fence_is_left_as_prose() {
        let fixture = Fixture::new();
        let outcome = fixture
            .sentinel
            .handle("Result:\n```json\n{\"tool\": \"list_agents\"}\n```");
        assert_eq!(outcome.reply, None);
        assert!(outcome.prose.contains("```json"), "{}", outcome.prose);
    }

    /// #190: three backticks are the *minimum*, not the shape. A model
    /// reaches for four whenever the fenced body might itself contain
    /// backticks -- a brief carrying code, a diff, a log excerpt -- which is
    /// the normal case here, and the block used to be dropped outright.
    #[test]
    fn a_block_opened_with_four_backticks_is_executed() {
        let fixture = Fixture::new();
        let outcome = fixture.sentinel.handle(&format!(
            "````mana:{}\n{{\"tool\": \"list_agents\"}}\n````",
            fixture.sentinel.nonce()
        ));

        assert_eq!(
            outcome.log,
            [ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false
            }]
        );
        let reply = outcome.reply.expect("a longer fence is still a call");
        assert!(
            reply.contains("1. list_agents ok: {\"agents\":["),
            "{reply}"
        );
    }

    /// The other half of #190: a longer fence without the nonce is watched
    /// like any other `mana` fence, so a PM that reached for four backticks
    /// *and* forgot its nonce is told once instead of ignored twice over.
    #[test]
    fn a_bare_mana_block_behind_a_longer_fence_is_still_reported() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(
                "````mana\n{\"tool\": \"create_task\", \"args\": {\"title\": \"x\", \"prompt\": \"y\"}}\n````",
            )
            .reply
            .expect("a block that would have parsed is never answered with silence");
        assert!(
            reply.contains("did not carry this session's nonce"),
            "{reply}"
        );
    }

    /// A closer shorter than its opener does not close it (CommonMark), so a
    /// fenced snippet inside a four-backtick brief stays in the body instead
    /// of cutting the call in half.
    #[test]
    fn a_shorter_fence_does_not_close_a_longer_block() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&format!(
                "````mana:{}\n{{\"tool\": \"list_agents\"}}\n```\n````",
                fixture.sentinel.nonce()
            ))
            .reply
            .unwrap();
        // The ``` line is body, not the closer -- so the body is the JSON plus
        // that line, which is malformed and comes back as a corrective rather
        // than as a call assembled out of half a block.
        assert!(reply.contains("not valid JSON"), "{reply}");
    }

    /// Backticks inside the JSON are ordinary characters -- a brief that shows
    /// the executor a fenced snippet is a normal brief, and losing it would
    /// make the channel unusable for the thing the PM does most.
    #[test]
    fn backticks_inside_the_body_are_part_of_the_call() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&fixture.block(
                r#"{"tool": "create_task", "args": {"title": "One", "prompt": "run ```sh\nls\n```"}}"#,
            ))
            .reply
            .unwrap();
        assert!(reply.contains("1. create_task ok:"), "{reply}");
    }

    /// A fence on a line of its own closes the block, so the JSON is cut in
    /// half. That must be a corrective message, never a half-executed call.
    #[test]
    fn a_fence_line_inside_the_body_is_malformed_rather_than_truncated() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&format!(
                "```mana:{}\n{{\"tool\": \"create_task\",\n```\n\"args\": {{}}}}\n```",
                fixture.sentinel.nonce()
            ))
            .reply
            .unwrap();
        assert!(reply.contains("not valid JSON"), "{reply}");
    }

    /// A turn cut off mid-block (a CLI's own timeout, a token limit) leaves an
    /// unclosed fence. Dropping it would lose the call silently.
    #[test]
    fn an_unclosed_block_is_reported_rather_than_dropped() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&format!(
                "```mana:{}\n{{\"tool\": \"list_agents\"",
                fixture.sentinel.nonce()
            ))
            .reply
            .unwrap();
        assert!(reply.contains("not valid JSON"), "{reply}");
    }

    /// The point of routing through `*_impl`: one implementation, one set of
    /// error messages. If this ever diverges, the sentinel has grown a second
    /// resolver -- which is the failure this channel exists to avoid.
    #[test]
    fn a_sentinel_call_and_an_mcp_call_produce_the_same_answer() {
        let fixture = Fixture::new();

        // The same failure: the id was never issued.
        let sentinel = fixture
            .sentinel
            .handle(&fixture.block(r#"{"tool": "get_review", "args": {"task_id": "nope"}}"#))
            .reply
            .unwrap();
        let mcp = fixture
            .tools
            .get_review_impl(
                serde_json::from_value(serde_json::json!({"task_id": "nope"})).unwrap(),
            )
            .unwrap_err();
        assert!(sentinel.contains(mcp.message.as_ref()), "{sentinel}");

        // ...and the same success, field for field. Compared as JSON values
        // rather than as bytes: both sides serialize the very same struct, and
        // pinning its field order here would be testing serde.
        let sentinel = fixture
            .sentinel
            .handle(&fixture.block(r#"{"tool": "list_agents"}"#));
        let mcp = serde_json::to_value(fixture.tools.list_agents_impl().unwrap()).unwrap();
        assert!(sentinel.reply.unwrap().contains(&mcp.to_string()));
    }
}
