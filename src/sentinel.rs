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
//! ```mana
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

/// The fence the PM opens. Anything else fenced (` ```json `, ` ``` `) is the
/// PM's own prose and is skipped whole, so a `mana` block quoted inside a
/// markdown example never fires.
const FENCE: &str = "```";
const LANGUAGE: &str = "mana";

/// mana's half of the sentinel channel: the parser and the sole executor.
pub struct Sentinel {
    tools: ManaTools,
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
        }
    }

    /// Scans one PM message, executes every block it carries, and reports what
    /// to render and what to send back.
    pub fn handle(&self, text: &str) -> Outcome {
        let (prose, blocks) = scan(text);
        if blocks.is_empty() {
            return Outcome {
                prose,
                log: Vec::new(),
                reply: None,
            };
        }
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
        Outcome {
            prose,
            log,
            reply: Some(reply(&results)),
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
const CORRECTION: &str = "One call per ```mana block, exactly \
    {\"tool\": \"<name>\", \"args\": {...}}. Tools: create_task, launch_subagent, \
    get_review, list_agents.";

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

/// Splits a PM message into the prose to render and the bodies of its `mana`
/// blocks.
///
/// Line-based, and it tracks the fence it is inside rather than looking for
/// ` ```mana ` anywhere: a `mana` block quoted inside a ` ```markdown ` example
/// belongs to the PM's prose, and executing it would let a PM explaining the
/// format accidentally use it.
fn scan(text: &str) -> (String, Vec<String>) {
    let mut prose: Vec<&str> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut body: Option<Vec<&str>> = None;
    let mut other_fence = false;

    for line in text.lines() {
        let trimmed = line.trim();
        match &mut body {
            // Inside a mana block: the first closing fence ends it. A block
            // whose body contains its own fence therefore ends early and comes
            // back as malformed JSON, which is a corrective message rather
            // than a silently truncated tool call.
            Some(collected) => {
                if trimmed.starts_with(FENCE) {
                    blocks.push(collected.join("\n"));
                    body = None;
                } else {
                    collected.push(line);
                }
            }
            None if other_fence => {
                prose.push(line);
                if trimmed.starts_with(FENCE) {
                    other_fence = false;
                }
            }
            None => {
                if let Some(language) = trimmed.strip_prefix(FENCE) {
                    if language.trim() == LANGUAGE {
                        body = Some(Vec::new());
                    } else {
                        other_fence = true;
                        prose.push(line);
                    }
                } else {
                    prose.push(line);
                }
            }
        }
    }
    // A block the PM never closed: the CLI truncated the turn, or the model
    // stopped mid-write. Kept rather than dropped so it fails as malformed
    // JSON and the PM is told, instead of the call vanishing.
    if let Some(collected) = body {
        blocks.push(collected.join("\n"));
    }
    (prose.join("\n").trim().to_string(), blocks)
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

    fn block(body: &str) -> String {
        format!("```mana\n{body}\n```")
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
            block(r#"{"tool": "list_agents"}"#)
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
            block(r#"{"tool": "create_task", "args": {"title": "One", "prompt": "do one"}}"#),
            block(r#"{"tool": "create_task", "args": {"title": "Two", "prompt": "do two"}}"#),
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
            .handle(&block("create_task(title=\"One\")"));

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
        assert!(reply.contains("One call per ```mana block"), "{reply}");
        assert!(reply.contains("create_task, launch_subagent"), "{reply}");
    }

    #[test]
    fn a_block_naming_a_tool_that_does_not_exist_says_which_ones_do() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&block(r#"{"tool": "delete_task", "args": {}}"#))
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
            .handle(&block(
                r#"{"tool": "create_task", "args": {"title": "One"}}"#,
            ))
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
            .handle(&block(r#"{"tool": "get_review", "args": "task-1"}"#))
            .reply
            .unwrap();
        assert!(reply.contains("must be an object"), "{reply}");
    }

    /// The PM explaining the format, or quoting a file, must not fire a tool.
    #[test]
    fn a_mana_block_quoted_inside_another_fence_is_prose() {
        let fixture = Fixture::new();
        let text = "Here is the shape:\n```markdown\n```mana\n{\"tool\": \"list_agents\"}\n```\n```\nThat is all.";
        let outcome = fixture.sentinel.handle(text);
        assert_eq!(outcome.reply, None);
        assert!(outcome.prose.contains("```mana"), "{}", outcome.prose);
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

    /// Backticks inside the JSON are ordinary characters -- a brief that shows
    /// the executor a fenced snippet is a normal brief, and losing it would
    /// make the channel unusable for the thing the PM does most.
    #[test]
    fn backticks_inside_the_body_are_part_of_the_call() {
        let fixture = Fixture::new();
        let reply = fixture
            .sentinel
            .handle(&block(
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
            .handle("```mana\n{\"tool\": \"create_task\",\n```\n\"args\": {}}\n```")
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
            .handle("```mana\n{\"tool\": \"list_agents\"")
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
            .handle(&block(
                r#"{"tool": "get_review", "args": {"task_id": "nope"}}"#,
            ))
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
            .handle(&block(r#"{"tool": "list_agents"}"#));
        let mcp = serde_json::to_value(fixture.tools.list_agents_impl().unwrap()).unwrap();
        assert!(sentinel.reply.unwrap().contains(&mcp.to_string()));
    }
}
