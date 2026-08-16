//! `mana dev` -- the scaffolding that lets a human drive the pipeline without
//! the PM.
//!
//! Milestone 1 is "task → executor → reviewer → verdict, against a real CLI",
//! and this predates the PM that issues those dispatches (`crate::mcp`,
//! `cli::launch_pm`, both landed since). Rather than wait, this command
//! composes the same functions the MCP tools call, so the loop was exercised
//! end to end months before its caller existed -- v1's core defect was five
//! phases that had never once run together. It stays because driving one task
//! against a named CLI and model, with no PM session in the way, is still the
//! shortest way to exercise the loop.
//!
//! Hidden from `--help` because it is developer scaffolding with an unstable
//! surface: it takes a CLI *and* a model as arguments to bypass `mcp::routing`,
//! which is what picks them for a real dispatch.

use crate::catalog::{Catalog, CliEntry};
use crate::dispatch::{self, DispatchOutcome, ExecutorRun, ReviewerRun};
use crate::project::mana_home;
use crate::review::{self, Decision, Verdict};
use crate::task::{Task, read_task};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum DevCommand {
    /// Run one task file through an executor, then a reviewer, then a verdict
    RunTask(RunTaskArgs),
}

#[derive(Args)]
pub struct RunTaskArgs {
    /// Task file (frontmatter + brief) to dispatch
    pub task_file: PathBuf,
    /// Catalogue id of the CLI to run both roles with (e.g. claude)
    #[arg(long)]
    pub cli: String,
    /// Model id handed to that CLI (e.g. haiku)
    #[arg(long)]
    pub model: String,
}

pub fn run(command: &DevCommand) -> Result<()> {
    match command {
        DevCommand::RunTask(args) => run_task(args),
    }
}

fn run_task(args: &RunTaskArgs) -> Result<()> {
    let home = mana_home()?;
    let catalog = Catalog::load(Some(&home.join(crate::catalog::CATALOG_OVERRIDE)))?;
    let entry = catalog.get(&args.cli).with_context(|| {
        format!(
            "unknown CLI id '{}' -- the catalogue knows: {}",
            args.cli,
            catalog.ids().join(", ")
        )
    })?;

    // The project is the current directory, as it is for every other mana
    // command; the task file is just an input and may live anywhere.
    let project_root = std::env::current_dir()?;
    let task = read_task(&args.task_file)
        .with_context(|| format!("reading task file {}", args.task_file.display()))?;

    let driven = drive(entry, &args.model, &task, &project_root, &home)?;
    println!("{}", render_summary(&driven, &project_root));
    exit_status(&driven)
}

/// What one end-to-end run produced. Carries the failures instead of
/// returning them so the summary -- and with it the worktree path the user
/// needs -- is printed on every path, including the ones that failed.
struct Driven {
    executor: ExecutorRun,
    reviewer: Option<ReviewerRun>,
    verdict: Option<Verdict>,
    /// Why the verdict is missing, already rendered: two reviewer runs both
    /// failed to leave a usable one.
    verdict_error: Option<String>,
    /// Whether the corrective re-dispatch was needed. Worth surfacing: a
    /// model that needs it every time is a role-text problem, not a fluke.
    retried: bool,
}

/// Executor in a fresh worktree, then the reviewer on its diff, with exactly
/// one corrective re-dispatch when the verdict file is missing or malformed.
///
/// One retry and no more: a model that ignored an explicit format twice is
/// not going to be talked into it a third time, and every attempt is a real
/// paid run.
fn drive(
    entry: &CliEntry,
    model: &str,
    task: &Task,
    project_root: &Path,
    mana_home: &Path,
) -> Result<Driven> {
    // A fresh id per dispatch: each one is a separate registry record with a
    // log of its own, and `mana ps`/`mana kill` resolve agents by that id.
    let executor_id = new_agent_id();
    let executor = dispatch::dispatch_executor(&dispatch::Assignment {
        agent_id: &executor_id,
        entry,
        model,
        task,
        project_root,
        mana_home,
    })?;
    if !executor.outcome.succeeded() {
        // No review: there is no reason to pay for a verdict on work that
        // never finished, and the executor's own failure is the finding.
        return Ok(Driven {
            executor,
            reviewer: None,
            verdict: None,
            verdict_error: None,
            retried: false,
        });
    }

    let first_review_id = new_agent_id();
    let mut reviewer = dispatch::dispatch_reviewer(
        &dispatch::Assignment {
            agent_id: &first_review_id,
            entry,
            model,
            task,
            project_root,
            mana_home,
        },
        &executor.worktree,
        None,
    )?;

    let mut retried = false;
    let mut verdict = None;
    let mut verdict_error = None;
    match review::read_verdict(&reviewer.review_path) {
        Ok(first) => verdict = Some(first),
        Err(error) => {
            retried = true;
            println!("unusable verdict ({error:#}); re-dispatching the reviewer once");
            // A new id: the corrective run is a second dispatch with a second
            // registry record and a log of its own, and reusing the first id
            // would make `mana ps`/`mana kill` see one agent twice.
            let retry_id = new_agent_id();
            reviewer = dispatch::dispatch_reviewer(
                &dispatch::Assignment {
                    agent_id: &retry_id,
                    entry,
                    model,
                    task,
                    project_root,
                    mana_home,
                },
                &executor.worktree,
                Some(&correction(&error, &reviewer.review_path)),
            )?;
            match review::read_verdict(&reviewer.review_path) {
                Ok(second) => verdict = Some(second),
                Err(second_error) => verdict_error = Some(format!("{second_error:#}")),
            }
        }
    }

    Ok(Driven {
        executor,
        reviewer: Some(reviewer),
        verdict,
        verdict_error,
        retried,
    })
}

/// One dispatch id. Minted per dispatch here because `mana dev` is the whole
/// caller: the MCP path mints its earlier, when it has to answer the PM with an
/// id before the process exists.
fn new_agent_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The corrective sentence appended to the second reviewer prompt. It repeats
/// the exact path and quotes the first failure: "write valid JSON" is advice,
/// "your file at /x/y.json was not valid JSON" is a correction.
fn correction(error: &anyhow::Error, review_path: &Path) -> String {
    format!(
        "CORRECTION -- your previous attempt produced no usable verdict: {error:#}. \
         Write the JSON object described above to {}, with nothing else in the file: \
         no prose, no code fences, no extra keys. Then stop.",
        review_path.display()
    )
}

/// A compact, human-readable report. Deliberately printed on success and
/// failure alike: the worktree is kept for inspection, so its path is part of
/// the result, not an error detail.
fn render_summary(driven: &Driven, project_root: &Path) -> String {
    let mut out = String::new();
    let worktree = &driven.executor.worktree;

    out.push_str(&format!(
        "executor  {}\n",
        describe(&driven.executor.outcome)
    ));
    if let Some(reviewer) = &driven.reviewer {
        out.push_str(&format!("reviewer  {}\n", describe(&reviewer.outcome)));
        if driven.retried {
            out.push_str("          (re-dispatched once after an unusable verdict)\n");
        }
    } else {
        out.push_str("reviewer  not run -- the executor did not finish cleanly\n");
        // The first real M1 dispatch failed in 2.5s on a CLI flag error that
        // was invisible here — the exit entry carries no output. Surface the
        // head of stderr so a bad invocation diagnoses itself.
        let err_head: String = driven
            .executor
            .outcome
            .stderr
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n          ");
        if !err_head.is_empty() {
            out.push_str(&format!("          stderr: {err_head}\n"));
        }
    }

    match (&driven.verdict, &driven.verdict_error) {
        (Some(verdict), _) => {
            let attribution = verdict
                .attribution
                .map(|attribution| format!(" [{attribution:?}]").to_lowercase())
                .unwrap_or_default();
            out.push_str(&format!("verdict   {:?}{attribution}\n", verdict.verdict));
            for issue in &verdict.issues {
                out.push_str(&format!("          - {issue}\n"));
            }
            if verdict.counts_against_model() {
                out.push_str("          counts against this model's record\n");
            }
        }
        (None, Some(error)) => out.push_str(&format!("verdict   none: {error}\n")),
        (None, None) => out.push_str("verdict   none\n"),
    }

    out.push_str(&format!(
        "branch    {} ({}..HEAD)\n",
        worktree.branch, worktree.base_ref
    ));
    // Cleanup stays manual for milestone 1: the whole point of the worktree
    // is that a human can go and look at what the agent actually did.
    out.push_str(&format!(
        "worktree  {} (kept)\n          cleanup: git -C {} worktree remove --force {}",
        worktree.path.display(),
        project_root.display(),
        worktree.path.display()
    ));
    out
}

/// The shared outcome line plus the one thing only a human at a terminal wants:
/// the log to open. Everything else comes from `dispatch::describe`, so this
/// summary and the PM's notification cannot disagree about what a run did.
fn describe(outcome: &DispatchOutcome) -> String {
    format!(
        "{}  log: {}",
        dispatch::describe(outcome),
        outcome.log_path.display()
    )
}

/// Non-zero exit on anything that is not a clean validation, so the command
/// composes in a script the way any other tool would.
fn exit_status(driven: &Driven) -> Result<()> {
    if !driven.executor.outcome.succeeded() {
        bail!("the executor did not finish cleanly -- see the summary above");
    }
    match (&driven.verdict, &driven.verdict_error) {
        (Some(verdict), _) if verdict.verdict == Decision::Validated => Ok(()),
        (Some(verdict), _) => bail!(
            "the reviewer rejected this task ({} issue(s))",
            verdict.issues.len()
        ),
        (None, Some(error)) => bail!("no usable verdict after two reviewer runs: {error}"),
        (None, None) => bail!("the reviewer produced no verdict"),
    }
}

#[cfg(all(test, unix))] // drives real dispatches against shell-script CLIs
mod tests {
    use super::*;
    use crate::dispatch::test_fixture::{COMMITTING_EXECUTOR, Fixture, TASK_ID, task};
    use crate::dispatch::test_support::{PLAIN_SUBAGENT, fixture_entry};
    use crate::review::Attribution;

    /// One fake CLI playing both roles, the way `dev run-task` uses one entry
    /// for the whole run. It tells them apart by the role template it was
    /// handed, which also proves the right prompt reached the right run, and
    /// echoes the reviewer prompt so a test can assert what was in it.
    fn both_roles(fixture: &Fixture, reviewer_body: &str) -> CliEntry {
        let bin = fixture.script(
            "dual-cli",
            &format!(
                r#"
case "$1" in
  "You are a reviewer"*)
    printf '%s' "$1"
{reviewer_body}
    ;;
  *)
{COMMITTING_EXECUTOR}
    ;;
esac
"#
            ),
        );
        fixture_entry(&bin, PLAIN_SUBAGENT, "")
    }

    fn write_verdict(json: &str, review_path: &Path) -> String {
        format!(
            "    printf '%s' '{json}' > '{}'\n    echo verdict written",
            review_path.display()
        )
    }

    #[test]
    fn a_validated_task_drives_executor_then_reviewer_then_verdict() {
        let fixture = Fixture::new();
        let entry = both_roles(
            &fixture,
            &write_verdict(
                r#"{"verdict":"validated","issues":[]}"#,
                &fixture.review_path(),
            ),
        );

        let driven = drive(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        assert!(driven.executor.outcome.succeeded());
        assert!(!driven.retried);
        assert_eq!(
            driven.verdict.as_ref().unwrap().verdict,
            Decision::Validated
        );
        assert!(exit_status(&driven).is_ok());

        // The reviewer saw the reviewer template, uncorrected.
        let reviewer_prompt = &driven.reviewer.as_ref().unwrap().outcome.stdout;
        assert!(reviewer_prompt.starts_with("You are a reviewer"));
        assert!(!reviewer_prompt.contains("CORRECTION"));

        let summary = render_summary(&driven, &fixture.project);
        assert!(summary.contains("verdict   Validated"), "{summary}");
        assert!(summary.contains("worktree"), "{summary}");
        assert!(summary.contains("worktree remove --force"), "{summary}");
        // The worktree survives the run: milestone 1 wants it inspectable.
        assert!(driven.executor.worktree.path.join("done.txt").exists());
    }

    #[test]
    fn a_rejected_verdict_is_reported_and_exits_non_zero() {
        let fixture = Fixture::new();
        let entry = both_roles(
            &fixture,
            &write_verdict(
                r#"{"verdict":"rejected","attribution":"code","issues":["src/a.rs:1 criterion 2 unmet"]}"#,
                &fixture.review_path(),
            ),
        );

        let driven = drive(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        let verdict = driven.verdict.as_ref().unwrap();
        assert_eq!(verdict.verdict, Decision::Rejected);
        assert_eq!(verdict.attribution, Some(Attribution::Code));
        assert!(verdict.counts_against_model());

        let summary = render_summary(&driven, &fixture.project);
        assert!(summary.contains("verdict   Rejected [code]"), "{summary}");
        assert!(summary.contains("criterion 2 unmet"), "{summary}");
        assert!(summary.contains("counts against"), "{summary}");

        let error = exit_status(&driven).unwrap_err().to_string();
        assert!(error.contains("rejected"), "{error}");
    }

    #[test]
    fn a_malformed_verdict_buys_exactly_one_corrective_re_dispatch() {
        let fixture = Fixture::new();
        let marker = fixture.bin_dir.join("reviewed-once");
        let review_path = fixture.review_path();
        // First run answers with prose, second with the real thing -- the
        // marker file is the fake CLI's only memory.
        let reviewer = format!(
            r#"    if [ -f '{marker}' ]; then
      printf '%s' '{{"verdict":"validated","issues":[]}}' > '{review}'
    else
      : > '{marker}'
      printf '%s' 'Sure! The work looks good to me.' > '{review}'
    fi"#,
            marker = marker.display(),
            review = review_path.display(),
        );
        let entry = both_roles(&fixture, &reviewer);

        let driven = drive(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        assert!(driven.retried);
        assert_eq!(
            driven.verdict.as_ref().unwrap().verdict,
            Decision::Validated
        );
        assert!(driven.verdict_error.is_none());
        assert!(exit_status(&driven).is_ok());

        // The correction reached the second run, and it named the path and the
        // first failure rather than repeating generic advice.
        let second_prompt = &driven.reviewer.as_ref().unwrap().outcome.stdout;
        assert!(second_prompt.contains("CORRECTION"), "{second_prompt}");
        assert!(
            second_prompt.contains(&review_path.to_string_lossy().to_string()),
            "{second_prompt}"
        );

        let summary = render_summary(&driven, &fixture.project);
        assert!(summary.contains("re-dispatched once"), "{summary}");

        // Three dispatches in the registry: executor, reviewer, reviewer.
        let registry = crate::lock::load_registry(&fixture.paths().subagents_file).unwrap();
        assert_eq!(registry.records.len(), 3);
        assert_eq!(registry.records[2].task_id, TASK_ID);
    }

    #[test]
    fn a_reviewer_that_never_produces_a_verdict_fails_after_the_second_try() {
        let fixture = Fixture::new();
        let entry = both_roles(&fixture, "    echo 'the work is fine'");

        let driven = drive(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        assert!(driven.retried);
        assert!(driven.verdict.is_none());
        let error = driven.verdict_error.as_ref().unwrap();
        assert!(error.contains("does not exist"), "{error}");

        // Still a full summary, because the worktree is still what the user
        // has to go and look at.
        let summary = render_summary(&driven, &fixture.project);
        assert!(summary.contains("verdict   none"), "{summary}");
        assert!(
            summary.contains(&driven.executor.worktree.branch),
            "{summary}"
        );
        assert!(exit_status(&driven).is_err());
    }

    #[test]
    fn a_failed_executor_skips_the_review_entirely() {
        let fixture = Fixture::new();
        let bin = fixture.script("failing-cli", "echo 'I give up' >&2\nexit 2\n");
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");

        let driven = drive(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        assert_eq!(driven.executor.outcome.exit_code, Some(2));
        assert!(driven.reviewer.is_none());
        assert!(driven.verdict.is_none());

        let summary = render_summary(&driven, &fixture.project);
        assert!(summary.contains("reviewer  not run"), "{summary}");
        assert!(summary.contains("exit 2"), "{summary}");

        // One dispatch only: no reviewer was paid for.
        let registry = crate::lock::load_registry(&fixture.paths().subagents_file).unwrap();
        assert_eq!(registry.records.len(), 1);
        assert!(exit_status(&driven).is_err());
    }

    #[test]
    fn the_summary_reports_a_matched_failure_signature() {
        let fixture = Fixture::new();
        let bin = fixture.script("broke-cli", "echo 'error 402'\nexit 1\n");
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"quota_exhausted\"\nexit_codes = [1]\nstdout_regex = \"402\"\n",
        );

        let driven = drive(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        let summary = render_summary(&driven, &fixture.project);
        assert!(summary.contains("quota_exhausted"), "{summary}");
        assert!(summary.contains("reviewer  not run"), "{summary}");
    }
}
