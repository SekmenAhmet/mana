//! The reviewer's verdict: the one outcome signal mana keeps.
//!
//! v1 had the reviewer write markdown and then scraped it back with a parser
//! that hunted for "✅ Validated" and renumbered lists. That is a rendering
//! format being used as a data format: every sentence the model chose to add
//! was a chance to change the meaning, and nothing could be *required* --
//! there was no way to say "a rejection must say whose fault it is".
//!
//! JSON with `deny_unknown_fields` inverts that. The reviewer writes one
//! object at a path mana chose; anything else is a malformed verdict, which
//! the dispatch layer answers with exactly one corrective re-prompt. The
//! shape is small on purpose -- it is what routing needs (design §8) and
//! nothing more.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Validated,
    Rejected,
}

impl Decision {
    /// The name this variant answers to in prose and on the wire.
    ///
    /// On the type rather than in the module that prints it: the same mapping was
    /// hand-written in two places and drifted apart nowhere only by luck (#53).
    /// A reader who adds a variant now gets a non-exhaustive-match error here
    /// instead of a silently missing word at some call site.
    pub fn word(self) -> &'static str {
        match self {
            Decision::Validated => "validated",
            Decision::Rejected => "rejected",
        }
    }
}

/// Whose failure a rejection is. The field exists because a bad brief must
/// not condemn a good model: only `Code` rejections count against the (CLI ×
/// model) pair the router reads (design §8) -- the agy-cwd incident is what
/// this prevents from being recorded as "agy is unreliable".
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Attribution {
    Code,
    Brief,
}

impl Attribution {
    /// The name this variant answers to in prose and on the wire.
    ///
    /// On the type rather than in the module that prints it: the same mapping was
    /// hand-written in two places and drifted apart nowhere only by luck (#53).
    /// A reader who adds a variant now gets a non-exhaustive-match error here
    /// instead of a silently missing word at some call site.
    pub fn word(self) -> &'static str {
        match self {
            Attribution::Code => "code",
            Attribution::Brief => "brief",
        }
    }
}

/// The verdict file, exactly as `assets/roles/reviewer.md` asks for it.
///
/// `deny_unknown_fields` for the same reason the catalogue carries it: a key
/// the reviewer invented (`"confidence"`, `"summary"`) means it was working
/// from a shape mana does not know, and silently dropping it would leave that
/// divergence to be discovered much later, in a routing decision.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// Named `verdict` inside `Verdict` because the JSON contract is what
    /// matters here, and the agent-facing template spells the key out.
    pub verdict: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
    /// Defaulted rather than required: a validated verdict that omitted the
    /// empty array said everything it needed to, and burning a second real
    /// reviewer run over a missing `[]` would be an expensive way to enforce
    /// punctuation.
    #[serde(default)]
    pub issues: Vec<String>,
}

impl Verdict {
    /// The routing question this file exists to answer: does this failure go
    /// on the model's record? A rejection blamed on the brief does not -- the
    /// instruction gets fixed, the model was never at fault.
    pub fn counts_against_model(&self) -> bool {
        self.verdict == Decision::Rejected && self.attribution == Some(Attribution::Code)
    }
}

/// Reads and validates the verdict a reviewer was asked to write.
///
/// A missing file is the single most likely failure -- the reviewer answered
/// in its own output instead of writing anything -- so the error names the
/// exact path that was expected, which is also what the corrective re-prompt
/// repeats back to the model.
pub fn read_verdict(path: &Path) -> Result<Verdict> {
    if !path.exists() {
        bail!(
            "the reviewer wrote no verdict at {} -- the file does not exist",
            path.display()
        );
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading the verdict at {}", path.display()))?;
    parse(&contents).with_context(|| format!("invalid verdict at {}", path.display()))
}

/// Parses one verdict document. Split from `read_verdict` so the contract can
/// be tested from a string, and so the file-level failures (absent,
/// unreadable) stay separate from the content-level ones.
pub fn parse(source: &str) -> Result<Verdict> {
    let verdict: Verdict = serde_json::from_str(source.trim())
        .context("expected a single JSON object: {\"verdict\": ..., \"issues\": [...]}")?;
    validate(&verdict)?;
    Ok(verdict)
}

/// The two rules JSON types cannot express. Both are contradictions rather
/// than style: a verdict that breaks either one cannot be acted on.
fn validate(verdict: &Verdict) -> Result<()> {
    match verdict.verdict {
        // Without attribution a rejection has nowhere to land: counting it
        // against the model would be a guess, and dropping it would lose the
        // only outcome signal mana collects.
        Decision::Rejected if verdict.attribution.is_none() => {
            bail!(
                "a rejected verdict must carry an attribution (\"code\" if the implementation \
                 fails a clear brief, \"brief\" if the brief itself was at fault)"
            )
        }
        // "Validated, but here are the problems" is not a verdict; one of the
        // two halves is wrong and mana cannot tell which.
        Decision::Validated if !verdict.issues.is_empty() => {
            bail!(
                "a validated verdict must list no issues, but {} were reported -- \
                 reject the work or drop the issues",
                verdict.issues.len()
            )
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_validated_verdict() {
        let verdict = parse(r#"{"verdict": "validated", "issues": []}"#).unwrap();
        assert_eq!(verdict.verdict, Decision::Validated);
        assert_eq!(verdict.attribution, None);
        assert!(verdict.issues.is_empty());
        assert!(!verdict.counts_against_model());
    }

    #[test]
    fn parses_a_validated_verdict_that_omitted_the_empty_issue_list() {
        let verdict = parse(r#"{"verdict": "validated"}"#).unwrap();
        assert_eq!(verdict.verdict, Decision::Validated);
    }

    #[test]
    fn parses_a_rejection_attributed_to_the_code() {
        let verdict = parse(
            r#"{
                "verdict": "rejected",
                "attribution": "code",
                "issues": [
                    "src/foo.rs:42 — criterion 2 unmet",
                    "tests weakened: `test_roundtrip` deleted"
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(verdict.verdict, Decision::Rejected);
        assert_eq!(verdict.attribution, Some(Attribution::Code));
        assert_eq!(verdict.issues.len(), 2);
        assert!(verdict.issues[0].contains("src/foo.rs:42"));
        assert!(verdict.counts_against_model());
    }

    #[test]
    fn a_rejection_blamed_on_the_brief_does_not_count_against_the_model() {
        let verdict = parse(
            r#"{"verdict": "rejected", "attribution": "brief", "issues": ["criterion 3 is self-contradictory"]}"#,
        )
        .unwrap();
        assert_eq!(verdict.attribution, Some(Attribution::Brief));
        assert!(!verdict.counts_against_model());
    }

    #[test]
    fn a_rejection_without_attribution_is_rejected() {
        let error = parse(r#"{"verdict": "rejected", "issues": ["something"]}"#).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("attribution"), "{rendered}");
    }

    #[test]
    fn a_validated_verdict_with_issues_is_rejected() {
        let error = parse(r#"{"verdict": "validated", "issues": ["but actually this is broken"]}"#)
            .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("no issues"), "{rendered}");
    }

    #[test]
    fn an_unknown_verdict_value_is_rejected() {
        let error = parse(r#"{"verdict": "maybe", "issues": []}"#).unwrap_err();
        assert!(format!("{error:#}").contains("maybe"), "{error:#}");
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        // The reviewer inventing keys means it is working from a shape mana
        // does not know -- worth one corrective re-prompt, not a silent drop.
        let error =
            parse(r#"{"verdict": "validated", "issues": [], "confidence": 0.7}"#).unwrap_err();
        assert!(format!("{error:#}").contains("confidence"), "{error:#}");
    }

    #[test]
    fn a_missing_verdict_field_is_rejected() {
        let error = parse(r#"{"issues": []}"#).unwrap_err();
        assert!(format!("{error:#}").contains("verdict"), "{error:#}");
    }

    #[test]
    fn prose_around_the_json_is_rejected() {
        // The most common malformed answer: the model narrates its verdict.
        let error = parse("Here is my verdict:\n{\"verdict\": \"validated\"}\n").unwrap_err();
        assert!(format!("{error:#}").contains("JSON object"), "{error:#}");
    }

    #[test]
    fn an_empty_file_is_rejected() {
        assert!(parse("").is_err());
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        // Trailing newlines are how every shell redirect writes a file.
        assert_eq!(
            parse("\n  {\"verdict\": \"validated\"}\n\n")
                .unwrap()
                .verdict,
            Decision::Validated
        );
    }

    #[test]
    fn read_verdict_names_the_path_it_expected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("reviews/task-1.json");
        let error = read_verdict(&path).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("task-1.json"), "{rendered}");
        assert!(rendered.contains("does not exist"), "{rendered}");
    }

    #[test]
    fn read_verdict_names_the_path_when_the_content_is_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task-1.json");
        std::fs::write(&path, "not json at all").unwrap();
        let rendered = format!("{:#}", read_verdict(&path).unwrap_err());
        assert!(rendered.contains("task-1.json"), "{rendered}");
    }

    #[test]
    fn read_verdict_round_trips_a_file_written_by_a_reviewer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task-1.json");
        std::fs::write(
            &path,
            r#"{"verdict":"rejected","attribution":"code","issues":["criterion 1 unmet"]}"#,
        )
        .unwrap();
        let verdict = read_verdict(&path).unwrap();
        assert!(verdict.counts_against_model());
        // Serializing back gives the same document: the type is the contract,
        // in both directions (the MCP `get_review` tool hands this to the PM).
        assert_eq!(
            serde_json::to_string(&verdict).unwrap(),
            r#"{"verdict":"rejected","attribution":"code","issues":["criterion 1 unmet"]}"#
        );
    }
}
