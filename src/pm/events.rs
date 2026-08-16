//! The extraction layer: one line of a PM's output in, zero or more
//! `PmEvent`s out.
//!
//! This is deliberately a pure function over a string. The process, the
//! threads and the channel live in the drivers; everything that decides *what
//! a line means* lives here, so a recorded transcript can be replayed through
//! the exact code path a live session uses -- which is what makes the golden
//! test at the bottom a real CI guard rather than a parallel implementation.
//!
//! Nothing here knows any CLI. The JSONPaths in `[pm.events]` are the whole of
//! mana's per-CLI knowledge; the code applies them to every line and lets
//! non-matches fall through. Hardcoding "claude puts text on lines of type
//! assistant" would move that knowledge back into Rust, where it rots.

use super::PmEvent;
use crate::catalog::CliEntry;
use anyhow::{Context, Result};
use serde_json::Value;
use serde_json_path::JsonPath;

/// A compiled `[pm.events]` map.
///
/// Compiled once at session start: a JSONPath is parsed into a query tree, and
/// re-parsing it for every line of a chatty stream would burn CPU on a string
/// that cannot change mid-session. Compiling early also turns a typo in the
/// catalogue into a startup error instead of a silently empty chat pane.
#[derive(Debug)]
pub struct EventMap {
    text: JsonPath,
    /// Optional: a CLI that reports no token counts simply omits the field.
    usage: Option<JsonPath>,
    /// Optional: only a driver whose process outlives a turn needs to read the
    /// boundary out of the stream.
    turn_end: Option<(JsonPath, String)>,
}

impl EventMap {
    /// Compiles the entry's `[pm.events]`, naming the CLI and the field on any
    /// failure -- the person who has to fix it is holding a TOML file, not a
    /// stack trace.
    pub fn for_entry(entry: &CliEntry) -> Result<Self> {
        let id = &entry.cli.id;
        let events = entry.pm.events.as_ref().with_context(|| {
            format!(
                "{id}: [pm.events] is missing -- every driver but 'acp' reads the stream through \
                 this map (acp carries its own typed notifications)"
            )
        })?;
        let text = compile(&events.text)
            .with_context(|| format!("{id}: [pm.events].text is not a valid JSONPath"))?;
        let usage = events
            .usage
            .as_deref()
            .map(compile)
            .transpose()
            .with_context(|| format!("{id}: [pm.events].usage is not a valid JSONPath"))?;
        let turn_end = events
            .turn_end
            .as_ref()
            .map(|end| compile(&end.path).map(|path| (path, end.equals.clone())))
            .transpose()
            .with_context(|| format!("{id}: [pm.events].turn_end.path is not a valid JSONPath"))?;
        Ok(EventMap {
            text,
            usage,
            turn_end,
        })
    }

    /// Whether this map can tell mana a turn is over.
    pub fn tracks_turn_end(&self) -> bool {
        self.turn_end.is_some()
    }

    /// Turns one line of the PM's output into the events it carries.
    ///
    /// The contract is deliberately thin (design §4): text to display, usage to
    /// record, the frame that closes a turn, and nothing else. Anything the map
    /// did not match comes back as `Raw` -- degraded and visible, never silent,
    /// because a CLI that changes its stream shape must show up as ugly lines
    /// in the chat pane rather than as a PM that mysteriously went quiet.
    ///
    /// Order within a line: every `text` match in document order, then `usage`,
    /// then `turn_end`. A line that yields anything at all never also yields
    /// `Raw`.
    pub fn extract(&self, line: &str) -> Vec<PmEvent> {
        // Blank lines are framing, not degradation: emitting `Raw("")` for the
        // gaps between frames would fill the chat pane with nothing.
        if line.trim().is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // Not JSON at all -- a warning, a banner, a progress line. Exactly
            // what the raw fallback exists for.
            return vec![PmEvent::Raw(line.to_string())];
        };

        let mut events = Vec::new();
        for node in self.text.query(&value).all() {
            // Only strings are displayable. A path that matches an object is a
            // catalogue mistake, and letting the line fall through to `Raw`
            // shows the user the whole frame instead of a rendered `{...}`.
            if let Some(text) = node.as_str() {
                events.push(PmEvent::Text(text.to_string()));
            }
        }
        if let Some(usage) = &self.usage {
            // First match only: usage is a snapshot of the turn, so a path that
            // matched several nodes has picked up siblings rather than a stream.
            if let Some(node) = usage.query(&value).first() {
                events.push(PmEvent::Usage(node.clone()));
            }
        }
        if let Some((path, expected)) = &self.turn_end
            && path.query(&value).first().and_then(Value::as_str) == Some(expected.as_str())
        {
            // Last of the three, so everything the closing frame carried --
            // the answer, the totals -- is already in hand when whoever is
            // waiting on the turn is told it may speak.
            events.push(PmEvent::TurnEnded);
        }
        if events.is_empty() {
            events.push(PmEvent::Raw(line.to_string()));
        }
        events
    }
}

fn compile(path: &str) -> Result<JsonPath> {
    // The parse error already quotes the offending position; the caller adds
    // which field it came from.
    JsonPath::parse(path).map_err(|err| anyhow::anyhow!("{err}"))
}

#[cfg(test)]
pub(super) mod fixture {
    use crate::catalog::CliEntry;

    /// A minimal, valid `stream` entry for a CLI that does not exist.
    ///
    /// Built through `catalog::parse_entry` on purpose: a driver test that
    /// hand-built a struct would be testing a shape the catalogue can never
    /// produce, and every field below is one a real entry must supply.
    pub fn entry(
        bin: &str,
        args: &[&str],
        prompt: &str,
        text: &str,
        usage: Option<&str>,
    ) -> CliEntry {
        parse(&source(bin, args, prompt, text, usage))
    }

    pub fn parse(source: &str) -> CliEntry {
        crate::catalog::parse_entry(source).expect("fixture entry must be valid")
    }

    /// Adds `[pm.events].turn_end` to a source built by `source`.
    ///
    /// A patch rather than a sixth parameter: only the turn-taking tests care,
    /// and every other caller would have to grow a `None` for it.
    pub fn with_turn_end(source: &str, path: &str, equals: &str) -> String {
        source.replace(
            "\n[tools]",
            &format!("turn_end = {{ path = \"{path}\", equals = \"{equals}\" }}\n\n[tools]"),
        )
    }

    pub fn source(
        bin: &str,
        args: &[&str],
        prompt: &str,
        text: &str,
        usage: Option<&str>,
    ) -> String {
        // A JSON array of strings is also a valid TOML array, so serde_json
        // does the quoting -- including for the temp paths these tests spawn.
        let args = serde_json::to_string(args).unwrap();
        let usage = match usage {
            Some(path) => format!("usage = \"{path}\"\n"),
            None => String::new(),
        };
        format!(
            r#"
schema = 1
notes = "test fixture"

[cli]
id = "fake"
name = "Fake CLI"
bin = {bin:?}
version_args = ["--version"]

[pm]
driver = "stream"
args = {args}
prompt = "{prompt}"

[pm.events]
text = "{text}"
{usage}
[tools]
channel = "mcp"
mcp_args = []

[subagent]
args = ["-p"]
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEXT_PATH: &str = "$.message.content[?@.type=='text'].text";

    fn map(text: &str, usage: Option<&str>) -> EventMap {
        EventMap::for_entry(&fixture::entry("fake", &[], "stdin-jsonl", text, usage)).unwrap()
    }

    fn map_with_turn_end(text: &str, path: &str, equals: &str) -> EventMap {
        let source = fixture::source("fake", &[], "stdin-jsonl", text, Some("$.usage"));
        EventMap::for_entry(&fixture::parse(&fixture::with_turn_end(
            &source, path, equals,
        )))
        .unwrap()
    }

    /// The bug this scoping exists for, in one line: claude echoes a loaded
    /// skill back as a `user` frame shaped exactly like an assistant one, and
    /// an unscoped filter renders the whole SKILL.md as if the PM had said it.
    ///
    /// RFC 9535 allows an absolute (`$`-rooted) query inside a filter
    /// expression, and serde_json_path implements it -- which is what keeps
    /// this fix inside the catalogue instead of inside a Rust `if`.
    #[test]
    fn a_filter_may_scope_a_text_path_to_the_frame_that_carries_it() {
        const SCOPED: &str = "$.message.content[?@.type=='text' && $.type=='assistant'].text";
        let map = map(SCOPED, None);

        assert_eq!(
            map.extract(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"prose"}]}}"#
            ),
            vec![PmEvent::Text("prose".to_string())]
        );
        // Same shape, different frame: not the PM talking, so not chat.
        let echo = r##"{"type":"user","isSynthetic":true,"message":{"role":"user","content":[{"type":"text","text":"# mana PM"}]}}"##;
        assert_eq!(map.extract(echo), vec![PmEvent::Raw(echo.to_string())]);
    }

    #[test]
    fn the_frame_the_catalogue_names_ends_the_turn() {
        let map = map_with_turn_end(TEXT_PATH, "$.type", "result");
        assert_eq!(
            map.extract(r#"{"type":"result","usage":{"output_tokens":4}}"#),
            vec![
                PmEvent::Usage(json!({"output_tokens": 4})),
                PmEvent::TurnEnded,
            ]
        );
        // Mid-turn frames leave it open.
        assert_eq!(
            map.extract(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#
            ),
            vec![PmEvent::Text("hi".to_string())]
        );
    }

    /// A frame that ends a turn and says nothing else is still a turn ending,
    /// not an unmatched line -- otherwise the queue would wait on a `Raw`.
    #[test]
    fn a_bare_turn_end_frame_is_not_degraded_to_raw() {
        let map = map_with_turn_end(TEXT_PATH, "$.kind", "done");
        assert_eq!(map.extract(r#"{"kind":"done"}"#), vec![PmEvent::TurnEnded]);
    }

    /// The value has to match, not merely exist: `stopReason: max_tokens` and
    /// `stopReason: end_turn` are different frames on some CLIs.
    #[test]
    fn a_turn_end_path_that_resolves_to_another_value_ends_nothing() {
        let map = map_with_turn_end(TEXT_PATH, "$.type", "result");
        let line = r#"{"type":"system","subtype":"init"}"#;
        assert_eq!(map.extract(line), vec![PmEvent::Raw(line.to_string())]);
    }

    #[test]
    fn a_map_without_a_turn_end_never_claims_to_track_turns() {
        assert!(!map(TEXT_PATH, None).tracks_turn_end());
        assert!(map_with_turn_end(TEXT_PATH, "$.type", "result").tracks_turn_end());
    }

    #[test]
    fn a_bad_turn_end_path_is_a_loud_startup_error_naming_the_field() {
        let source = fixture::source("fake", &[], "stdin-jsonl", TEXT_PATH, None);
        let entry = fixture::parse(&fixture::with_turn_end(&source, "$.[", "result"));
        let err = EventMap::for_entry(&entry).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("[pm.events].turn_end.path"), "{rendered}");
        assert!(rendered.contains("fake"), "{rendered}");
    }

    #[test]
    fn a_matching_line_yields_the_text_it_carries() {
        let events = map(TEXT_PATH, None)
            .extract(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        assert_eq!(events, vec![PmEvent::Text("hi".to_string())]);
    }

    /// One frame can carry several text blocks, and dropping the tail would
    /// lose half of what the PM said.
    #[test]
    fn every_text_match_becomes_its_own_event_in_document_order() {
        let events = map(TEXT_PATH, None).extract(
            r#"{"message":{"content":[{"type":"text","text":"one"},{"type":"thinking","thinking":"x"},{"type":"text","text":"two"}]}}"#,
        );
        assert_eq!(
            events,
            vec![
                PmEvent::Text("one".to_string()),
                PmEvent::Text("two".to_string()),
            ]
        );
    }

    #[test]
    fn a_line_the_map_does_not_match_falls_through_raw() {
        let line = r#"{"type":"system","subtype":"init"}"#;
        assert_eq!(
            map(TEXT_PATH, Some("$.usage")).extract(line),
            vec![PmEvent::Raw(line.to_string())]
        );
    }

    #[test]
    fn a_line_that_is_not_json_falls_through_raw() {
        let line = "warning: your session will expire soon";
        assert_eq!(
            map(TEXT_PATH, None).extract(line),
            vec![PmEvent::Raw(line.to_string())]
        );
    }

    #[test]
    fn blank_lines_produce_nothing() {
        assert!(map(TEXT_PATH, None).extract("   ").is_empty());
        assert!(map(TEXT_PATH, None).extract("").is_empty());
    }

    #[test]
    fn usage_is_extracted_whole_and_never_rendered_as_text() {
        let events = map(TEXT_PATH, Some("$.usage"))
            .extract(r#"{"type":"result","usage":{"output_tokens":44}}"#);
        assert_eq!(events, vec![PmEvent::Usage(json!({"output_tokens": 44}))]);
    }

    #[test]
    fn text_comes_before_usage_when_a_line_carries_both() {
        let events = map("$.text", Some("$.usage"))
            .extract(r#"{"text":"done","usage":{"output_tokens":1}}"#);
        assert_eq!(
            events,
            vec![
                PmEvent::Text("done".to_string()),
                PmEvent::Usage(json!({"output_tokens": 1})),
            ]
        );
    }

    /// A `usage` path is optional, and a CLI that reports nothing must not
    /// turn every line into an enrichment event.
    #[test]
    fn no_usage_path_means_no_usage_events() {
        let events = map(TEXT_PATH, None).extract(r#"{"usage":{"output_tokens":44}}"#);
        assert!(matches!(events.as_slice(), [PmEvent::Raw(_)]), "{events:?}");
    }

    /// A path that matches something undisplayable is a catalogue bug; showing
    /// the whole frame is more useful than showing `{"a":1}` as if the PM had
    /// said it.
    #[test]
    fn a_non_string_text_match_degrades_to_raw() {
        let line = r#"{"text":{"a":1}}"#;
        assert_eq!(
            map("$.text", None).extract(line),
            vec![PmEvent::Raw(line.to_string())]
        );
    }

    #[test]
    fn a_bad_text_path_is_a_loud_startup_error_naming_the_field() {
        let entry = fixture::entry("fake", &[], "stdin-jsonl", "$.[", None);
        let err = EventMap::for_entry(&entry).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("[pm.events].text"), "{rendered}");
        assert!(rendered.contains("fake"), "{rendered}");
    }

    #[test]
    fn a_bad_usage_path_is_a_loud_startup_error_naming_the_field() {
        let entry = fixture::entry("fake", &[], "stdin-jsonl", "$.text", Some("$.["));
        let err = EventMap::for_entry(&entry).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("[pm.events].usage"), "{rendered}");
    }

    #[test]
    fn a_missing_event_map_is_a_loud_startup_error() {
        // An `acp` entry legitimately has none, which is why the check lives
        // here and not in the catalogue's own validation.
        let entry = fixture::parse(
            &fixture::source("fake", &[], "stdin-jsonl", "$.text", None)
                .replace("[pm.events]\ntext = \"$.text\"\n", ""),
        );
        let err = EventMap::for_entry(&entry).unwrap_err();
        assert!(format!("{err:#}").contains("[pm.events]"), "{err:#}");
    }
}

/// The CI guard the design asks for: a real recorded transcript replayed
/// through the shipped `[pm.events]` map.
///
/// It uses the embedded catalogue rather than a fixture on purpose -- the
/// thing under test is whether *claude's shipped paths* still describe
/// *claude's real output*. A fixture would pass forever while the shipped
/// entry rotted.
#[cfg(test)]
mod golden {
    use super::*;
    use crate::catalog::Catalog;

    /// Recorded from a real `claude -p --input-format stream-json
    /// --output-format stream-json --verbose` turn on 2026-08-15, prompt:
    /// "reply with exactly: GOLDEN".
    const SINGLE_TURN: &str = include_str!("../../catalog/goldens/claude/single-turn.jsonl");

    /// Four turns of a real `mana launch agy` session (2026-08-15,
    /// gemini-3.7-flash-low, the oneshot-continue driver): the PM called
    /// `list_agents`, then `create_task`, both over the sentinel channel, and
    /// mana wrote the task file the last turn names. Four processes, one
    /// conversation -- which is why there are four `init` frames.
    const AGY_SESSION: &str = include_str!("../../catalog/goldens/agy/sentinel-turn.jsonl");

    /// The same session shape, from the run *before* the role text was
    /// inlined: the PM guessed the format (a bare `list_agents` in the fence),
    /// mana answered with one corrective message, and the next turn was
    /// well-formed -- pretty-printed across five lines, which is the shape the
    /// scanner has to survive.
    const AGY_MALFORMED: &str = include_str!("../../catalog/goldens/agy/malformed-block.jsonl");

    /// The bug the user reported, recorded: a real `mana launch claude`
    /// activation turn (2026-08-16, haiku, this entry's exact argv, the skill
    /// installed at `.claude/skills/mana-pm`) in which the PM called the
    /// `Skill` tool and the CLI echoed the whole of SKILL.md back into the
    /// stream. Nine frames out of the 53 that turn produced: the two assistant
    /// texts, the thinking blocks around them, the tool_use, its tool_result,
    /// the echo itself, a rate-limit notice, and the closing `result`.
    const SKILL_ECHO: &str = include_str!("../../catalog/goldens/claude/skill-echo.jsonl");

    fn replay(cli: &str, transcript: &str) -> Vec<Vec<PmEvent>> {
        let catalog = Catalog::embedded().unwrap();
        let map = EventMap::for_entry(catalog.get(cli).expect("catalogue entry")).unwrap();
        transcript.lines().map(|line| map.extract(line)).collect()
    }

    #[test]
    fn claude_single_turn_replays_to_one_text_one_usage_and_visible_raw() {
        let lines: Vec<&str> = SINGLE_TURN.lines().collect();
        let events = replay("claude", SINGLE_TURN);
        assert_eq!(events.len(), lines.len(), "a line produced no verdict");

        // Line 7 is the only assistant frame carrying a text block. This is the
        // whole point of the map: the chat pane shows this and nothing else.
        assert_eq!(events[6], vec![PmEvent::Text("GOLDEN".to_string())]);
        let texts: Vec<&PmEvent> = events
            .iter()
            .flatten()
            .filter(|e| matches!(e, PmEvent::Text(_)))
            .collect();
        assert_eq!(texts.len(), 1, "extra text leaked into the chat pane");

        // Line 9, the result frame, is where claude reports the turn's totals
        // -- and, since 2026-08-16, where it closes the turn.
        let PmEvent::Usage(usage) = &events[8][0] else {
            panic!("the result line lost its usage: {:?}", events[8]);
        };
        assert_eq!(events[8].len(), 2);
        assert_eq!(events[8][1], PmEvent::TurnEnded);
        assert_eq!(usage["output_tokens"], 44);

        // Line 6 is an assistant frame whose only content block is `thinking`,
        // and it carries a `message.usage` object. Both of the shipped paths
        // must miss it: the text filter is `?@.type=='text'`, and the usage
        // path is anchored at `$.usage`, not `$.message.usage`. Thinking is
        // not something the user asked to read, and per-block usage would
        // double-count the totals the result line already reports.
        assert_eq!(events[5], vec![PmEvent::Raw(lines[5].to_string())]);

        // Everything else -- the init banner, four thinking-token counters, a
        // rate-limit notice -- is outside the thin contract and degrades
        // visibly instead of disappearing.
        for index in [0, 1, 2, 3, 4, 7] {
            assert_eq!(
                events[index],
                vec![PmEvent::Raw(lines[index].to_string())],
                "line {} changed meaning",
                index + 1
            );
        }
    }

    /// The regression guard for the reported bug: a session where the PM loads
    /// its role must open on the PM's greeting, not on the role text.
    ///
    /// The frame that caused it is line 5 of the golden. It is a *user* frame
    /// -- `{"type":"user","isSynthetic":true,...}` -- whose message content is
    /// one ordinary `{"type":"text"}` block holding the entire SKILL.md, so the
    /// only thing that can tell it apart from the PM talking is the frame it
    /// arrived in.
    #[test]
    fn claude_never_renders_a_loaded_skill_as_something_the_pm_said() {
        let lines: Vec<&str> = SKILL_ECHO.lines().collect();
        let events = replay("claude", SKILL_ECHO);
        assert_eq!(events.len(), lines.len(), "a line produced no verdict");

        let texts: Vec<&str> = events
            .iter()
            .flatten()
            .filter_map(|event| match event {
                PmEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Exactly the two things the PM said, in order, and nothing else.
        assert_eq!(
            texts,
            [
                "I'll load the mana-pm skill to get the full instructions.",
                "Understood. I'm your PM for this session.\n\nBefore I start breaking work into \
                 tasks, I need to understand what we're building. What's the objective?",
            ]
        );
        assert!(
            !texts.iter().any(|text| text.contains("# mana PM")),
            "the role text reached the chat pane"
        );

        // The echo is still visible as degradation rather than dropped: it goes
        // to the raw view, behind the counter, like every other frame mana
        // understands but did not ask for.
        assert_eq!(events[4], vec![PmEvent::Raw(lines[4].to_string())]);
        assert!(lines[4].contains("# mana PM"), "wrong golden line");

        // The turn closes exactly once, on the last frame.
        let ends = events
            .iter()
            .flatten()
            .filter(|event| matches!(event, PmEvent::TurnEnded))
            .count();
        assert_eq!(ends, 1);
        assert_eq!(events[8].last(), Some(&PmEvent::TurnEnded));
    }

    /// agy's shipped paths against agy's real stream. Both were wrong until
    /// this transcript was recorded (they had been copied from claude's entry
    /// and matched nothing), which is exactly what a golden is for.
    #[test]
    fn agy_replays_to_one_whole_answer_and_one_usage_per_turn() {
        let events = replay("agy", AGY_SESSION);
        let texts: Vec<&PmEvent> = events
            .iter()
            .flatten()
            .filter(|event| matches!(event, PmEvent::Text(_)))
            .collect();
        // Four turns, four processes, four answers -- and each arrives whole,
        // once, rather than as the dozens of `text_delta` chunks the same
        // stream also carries.
        assert_eq!(texts.len(), 4, "{texts:?}");
        let PmEvent::Text(first) = texts[0] else {
            unreachable!("filtered above")
        };
        assert!(
            first.starts_with("```mana\n{\"tool\": \"list_agents\""),
            "{first}"
        );

        let usage: Vec<&PmEvent> = events
            .iter()
            .flatten()
            .filter(|event| matches!(event, PmEvent::Usage(_)))
            .collect();
        assert_eq!(usage.len(), 4);
        let PmEvent::Usage(last) = usage[3] else {
            unreachable!("filtered above")
        };
        // Cumulative over the conversation, not per turn: agy reports the
        // session total on every result frame (turn 1 alone was 18,245).
        assert_eq!(last["total_tokens"], 34765);

        // Everything else is framing -- init banners and step updates -- and
        // shows as raw lines rather than disappearing.
        let raw = events
            .iter()
            .flatten()
            .filter(|event| matches!(event, PmEvent::Raw(_)))
            .count();
        assert_eq!(
            raw + texts.len() + usage.len(),
            events.iter().flatten().count()
        );
        assert_eq!(raw, AGY_SESSION.lines().count() - 4);
    }

    /// The whole channel, replayed: a recorded agy line, through the shipped
    /// event map, into the sentinel, out as an executed tool call. Nothing in
    /// this path is a test double except the project directory.
    #[test]
    fn a_recorded_agy_turn_executes_its_block_through_the_sentinel() {
        use crate::sentinel::{Sentinel, ToolLine};

        let tmp = tempfile::tempdir().unwrap();
        let sentinel = Sentinel::new(
            tmp.path(),
            tmp.path(),
            Catalog::embedded().expect("the shipped catalogue must parse"),
        );
        let answers = |transcript| {
            replay("agy", transcript)
                .into_iter()
                .flatten()
                .filter_map(|event| match event {
                    PmEvent::Text(text) => Some(text),
                    _ => None,
                })
                .collect::<Vec<String>>()
        };

        // The first turn of the working run: a call, and a sentence of prose
        // that the operator sees without the block in the middle of it.
        let outcome = sentinel.handle(&answers(AGY_SESSION)[0]);
        assert_eq!(
            outcome.log,
            [ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false
            }]
        );
        assert!(
            outcome
                .prose
                .starts_with("I am ready to act as your mana PM")
        );
        assert!(
            outcome
                .reply
                .unwrap()
                .contains("1. list_agents ok: {\"agents\":[")
        );

        // The run before the role text was inlined: the PM guessed, and got
        // one corrective message naming what was wrong...
        let guessed = answers(AGY_MALFORMED);
        let corrected = sentinel.handle(&guessed[1]).reply.unwrap();
        assert!(corrected.contains("not valid JSON"), "{corrected}");
        assert!(
            corrected.contains("One call per ```mana block"),
            "{corrected}"
        );

        // ...after which the very next turn was well-formed, across five
        // lines, and ran.
        let outcome = sentinel.handle(&guessed[2]);
        assert_eq!(
            outcome.log,
            [ToolLine {
                text: "⚙ list_agents ✓".to_string(),
                failed: false
            }]
        );
        assert_eq!(outcome.prose, "");
    }
}
