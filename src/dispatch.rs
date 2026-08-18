//! Sub-agent dispatch: run one CLI on one task and judge it from the outside.
//!
//! This is the layer between the catalogue (what to spawn) and the spawner
//! (how to spawn it). It fills a role template, assembles argv, spawns the
//! process in the right directory, and turns what came back -- exit code,
//! duration, output -- into the two files that make a dispatch observable:
//! a registry record (`subagents.jsonl`) and a log (`logs/<agent>.jsonl`).
//!
//! Two roles, one code path, because the difference between them is data and
//! not control flow (design §9): the executor writes and therefore gets an
//! isolated worktree; the reviewer reads and therefore runs *in* the
//! executor's worktree and creates nothing. Nothing below knows a CLI's name.

use crate::catalog::{CliEntry, Failure, FailureMeans, PromptMode, substitute};
use crate::lock::{SubagentRecord, append_record};
use crate::log::{ExitEntry, LogEntry, Status, append_log, now_iso8601};
use crate::project::{
    ProjectPaths, ensure_project_structure, project_name_from_dir, resolve_project_paths,
};
use crate::spawn::{self, PromptDelivery, SpawnOutcome, SpawnSpec};
use crate::task::{Role, Task};
use crate::worktree::{self, WorktreeInfo};
use anyhow::{Context, Result, bail};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The role prompts ship with the binary for the same reason the catalogue
/// does: text and code that must agree about placeholder names have no
/// business being separately installable.
const EXECUTOR_TEMPLATE: &str = include_str!("../assets/roles/executor.md");
const REVIEWER_TEMPLATE: &str = include_str!("../assets/roles/reviewer.md");

/// Wall-clock budget for one executor run. Long, because a real task on a
/// cheap model spends minutes thinking and running tests; finite, because a
/// CLI waiting on a prompt nobody will answer would otherwise hold a
/// concurrency slot forever. A constant rather than a catalogue field on
/// purpose: no per-CLI measurement disagrees with it yet, and inventing
/// `[subagent].timeout_minutes` before there is evidence to put in it would
/// be schema for its own sake. Stated in prose in assets/roles/executor.md
/// ("about fifteen minutes") — change both together.
const EXECUTOR_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Reviewing reads a diff and runs the existing tests -- it never writes the
/// code -- so it gets two thirds of the executor's budget. Stated in prose in
/// assets/roles/reviewer.md ("about ten minutes") — change both together.
const REVIEWER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What mana observed of one sub-agent run. Sub-agents speak no protocol, so
/// this *is* the whole report: everything here is measured from outside the
/// process.
///
/// The two captured streams die with this struct unless somebody copies what
/// matters out of them first, which is what `mcp::failure_tail` now does for a
/// failed run (#32) -- before that they were matched against the catalogue's
/// failure signatures and dropped, and the reason a dispatch failed was gone.
/// `agent_id` still has no reader outside this module's tests: `mana ps` was
/// the expected one and turned out to read the registry and the per-agent
/// JSONL instead, both of which outlive the process.
#[allow(dead_code)]
#[derive(Debug)]
pub struct DispatchOutcome {
    pub agent_id: String,
    /// `None` when the process was killed by a signal -- read together with
    /// `timed_out` and `killed`.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// `true` when an operator asked for this death: `mana kill`, or the
    /// session teardown that shares its code path. A signalled process is
    /// otherwise indistinguishable from one the vendor cut off, and the two
    /// deserve opposite reactions -- see `run_dispatch`.
    pub killed: bool,
    pub duration: Duration,
    /// `Some` when one of the catalogue's ordered failure signatures matched
    /// (design §8): the routing signal that puts a pool on cooldown.
    pub failure_means: Option<FailureMeans>,
    pub stdout: String,
    pub stderr: String,
    pub log_path: PathBuf,
    /// What mana failed to write down about a run that nevertheless happened,
    /// already rendered. Carried rather than returned as an error: once the
    /// process has been spawned the tokens are spent, and a `~/.mana` that
    /// cannot be written to must not turn a finished, billed dispatch into
    /// "never started" -- that report buys a second full relaunch for a local
    /// failure no relaunch can fix.
    pub bookkeeping_error: Option<String>,
}

impl DispatchOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// An executor run plus the worktree it produced. The worktree outlives the
/// dispatch: the reviewer reads its diff, and the PM merges its branch.
#[derive(Debug)]
pub struct ExecutorRun {
    pub outcome: DispatchOutcome,
    pub worktree: WorktreeInfo,
}

/// A reviewer run plus the path its verdict was supposed to land on. Reading
/// and validating that file is `crate::review`'s job -- keeping it out of
/// here is what lets the caller retry a malformed verdict without re-running
/// anything else.
#[derive(Debug)]
pub struct ReviewerRun {
    pub outcome: DispatchOutcome,
    pub review_path: PathBuf,
}

/// Who runs what, and where -- the half of a dispatch both roles agree on.
///
/// A struct rather than six positional parameters, for the same reason `Plan`
/// is one a layer down: four of these are `&str`/`&Path`, and a future edit
/// that swapped two would compile happily and dispatch the wrong model into the
/// wrong directory.
pub struct Assignment<'a> {
    /// The dispatch's id, minted by the *caller*: `launch_subagent` has to hand
    /// the PM an id before the process it names exists, and a second id minted
    /// down here meant the PM announcing (and `mana ps`/`mana kill` refusing) a
    /// string that matched no dispatch. One id, minted once by whoever has to
    /// speak about the dispatch first. It goes into the registry record and
    /// names the log file, so it must be unique per dispatch -- a corrective
    /// re-dispatch is a new dispatch and mints a new one.
    pub agent_id: &'a str,
    pub entry: &'a CliEntry,
    pub model: &'a str,
    pub task: &'a Task,
    pub project_root: &'a Path,
    pub mana_home: &'a Path,
}

/// Dispatches the task to an executor: a fresh worktree, the executor role
/// prompt, and one process observed to completion.
/// Removes a task's verdict, treating "there was none" as success.
///
/// Two callers, two different transitions, one rule: a verdict describes one
/// attempt, and whatever invalidates that attempt invalidates the verdict with
/// it. The executor path clears it because the branch was just reset under it;
/// the reviewer path clears it because a corrective re-dispatch must not read
/// the answer it was sent to replace.
fn clear_verdict(review_path: &Path) -> Result<()> {
    if let Err(error) = std::fs::remove_file(review_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("clearing the previous verdict {}", review_path.display()));
    }
    Ok(())
}

pub fn dispatch_executor(assignment: &Assignment<'_>) -> Result<ExecutorRun> {
    let Assignment {
        agent_id,
        entry,
        model,
        task,
        project_root,
        mana_home,
    } = *assignment;

    // The write-role gate (design §9) runs first, before mana creates or
    // writes anything: on a non-git project the user must see "run git init",
    // not whichever git call downstream happened to fail first. `create`
    // re-checks -- cheap, and it keeps that function safe to call on its own.
    worktree::ensure_git_repo(project_root)?;

    let paths = project_paths(project_root, mana_home)?;
    let task_id = &task.frontmatter.id;

    // `worktree::create` builds its path from the task id with this exact
    // function, so computing it here ahead of `create` costs nothing and
    // names the real directory -- not a placeholder -- for the checks and the
    // prompt below.
    let worktree_path = worktree::worktree_path(mana_home, project_root, task_id);
    let worktree_path_str = worktree_path.to_string_lossy().into_owned();

    let prompt = fill(
        template_body(EXECUTOR_TEMPLATE, "executor.md")?,
        &[
            ("{worktree}", worktree_path_str.as_str()),
            ("{task_id}", task_id),
            ("{task_title}", &task.frontmatter.title),
            ("{task_body}", &task.body),
        ],
    );

    // #170: a sub-agent that cannot be spawned must fail before a worktree
    // and a branch exist for it. Every catalogue/environment check that can
    // fail this dispatch therefore runs here, against the worktree's future
    // path, before `worktree::create` puts anything on disk or in the repo.
    let prepared = prepare_spawn(entry, model, &prompt, &worktree_path, EXECUTOR_TIMEOUT)?;

    let worktree = worktree::create(project_root, mana_home, task_id)?;

    // A verdict's lifetime is its branch's. `create` above force-resets
    // `mana/<task id>` onto a fresh base, so a verdict written about the
    // previous attempt now describes code that is nowhere -- and `branch` and
    // `worktree` are deterministic from the task id, so `get_review` would
    // hand the PM that stale rejection paired with the *new* branch, and the
    // retry it prescribes is the dispatch that just ran (#153). Cleared after
    // `create` rather than before: a `create` that failed reset nothing, and
    // the verdict it would have invalidated is still the truth.
    clear_verdict(&paths.reviews.join(format!("{task_id}.json")))?;

    let outcome = run_dispatch(
        Plan {
            agent_id,
            entry,
            model,
            task_id,
            role: Role::Executor,
            paths: &paths,
        },
        prepared,
    )?;

    Ok(ExecutorRun { outcome, worktree })
}

/// Dispatches the review of a finished task.
///
/// No worktree is created: the reviewer runs in the executor's worktree,
/// reads the diff, and writes its verdict JSON to a file. "Read-only" is a
/// prompt contract, not a sandbox — the reviewer receives the same
/// `[subagent].auto_approve_args` as the executor. Mechanically enforcing
/// read-only-ness (via per-CLI permission flags) requires changing the
/// verdict channel from file-based to stdout, which is deferred beyond v2.
///
/// `correction` is appended to the prompt, and exists for exactly one caller:
/// the single corrective re-dispatch after an unusable verdict (plan 1.4).
pub fn dispatch_reviewer(
    assignment: &Assignment<'_>,
    worktree: &WorktreeInfo,
    correction: Option<&str>,
) -> Result<ReviewerRun> {
    let Assignment {
        agent_id,
        entry,
        model,
        task,
        project_root,
        mana_home,
    } = *assignment;

    let paths = project_paths(project_root, mana_home)?;
    let task_id = &task.frontmatter.id;
    // Validated on the way back out of the file as well as on the way in, for
    // the reason `mcp::unmet_dependency` gives about a `depends_on` id: the
    // MCP entry point checked the id the PM named, but this one was read from
    // plain text on disk and is about to become a path that gets deleted.
    // `dispatch_executor` gets the same check from `worktree::create`; the
    // reviewer creates no worktree, so it asks for it here (#187).
    worktree::validate_task_id(task_id)?;
    let review_path = paths.reviews.join(format!("{task_id}.json"));

    // A verdict left by an earlier *reviewer* -- the corrective re-dispatch
    // after an unusable one -- would otherwise be read back as this reviewer's
    // answer, and that re-dispatch would keep "succeeding" on stale bytes.
    clear_verdict(&review_path)?;

    let worktree_path = worktree.path.to_string_lossy().into_owned();
    let mut prompt = fill(
        template_body(REVIEWER_TEMPLATE, "reviewer.md")?,
        &[
            ("{worktree}", worktree_path.as_str()),
            ("{base_ref}", &worktree.base_ref),
            ("{review_path}", &review_path.to_string_lossy()),
            ("{task_id}", task_id),
            ("{task_title}", &task.frontmatter.title),
            ("{task_body}", &task.body),
        ],
    );
    if let Some(correction) = correction {
        prompt.push_str("\n\n");
        prompt.push_str(correction);
        prompt.push('\n');
    }

    let prepared = prepare_spawn(entry, model, &prompt, &worktree.path, REVIEWER_TIMEOUT)?;
    let outcome = run_dispatch(
        Plan {
            agent_id,
            entry,
            model,
            task_id,
            role: Role::Reviewer,
            paths: &paths,
        },
        prepared,
    )?;

    Ok(ReviewerRun {
        outcome,
        review_path,
    })
}

/// Everything one dispatch needs that the two roles disagree about. A struct
/// rather than six positional parameters, since half of them are strings and
/// swapping two would compile.
struct Plan<'a> {
    agent_id: &'a str,
    entry: &'a CliEntry,
    model: &'a str,
    task_id: &'a str,
    role: Role,
    paths: &'a ProjectPaths,
}

/// Everything about a dispatch that can fail before a process is spawned:
/// catalogue data (a failure signature, an argv template) and the local
/// environment (is the CLI even on PATH). Built once, ahead of the spawn, so
/// `dispatch_executor` can run it before `worktree::create` and satisfy #170
/// -- a sub-agent that cannot be spawned must fail before a worktree and a
/// branch exist for it. `dispatch_reviewer` creates no worktree and gets no
/// benefit from the reordering, but shares this preparation step anyway: one
/// path for "can this dispatch even run" is cheaper to keep correct than two.
struct PreparedSpawn {
    spec: SpawnSpec,
    signatures: Vec<Signature>,
}

fn prepare_spawn(
    entry: &CliEntry,
    model: &str,
    prompt: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<PreparedSpawn> {
    // Compiled before the spawn, not after: a typo in a catalogue regex is a
    // data bug, and finding it only once a real agent has already burned a
    // quota slot would be the most expensive possible moment to learn about it.
    let signatures = compile_signatures(&entry.failure)
        .with_context(|| format!("{}: invalid failure signature", entry.cli.id))?;

    let args = build_argv(entry, model, prompt, cwd)?;
    let prompt_delivery = PromptDelivery::from_mode(entry.subagent.prompt, prompt)?;

    // Resolved here rather than inside the spawner: `which` answers the same
    // question `mana doctor` and the PM's routing table ask, and a sub-agent
    // that cannot be spawned must fail before a worktree and a log record
    // exist for it. See `CliMeta::resolve`.
    let bin = entry
        .cli
        .resolve()
        .with_context(|| format!("cannot dispatch to {}", entry.cli.name))?;

    Ok(PreparedSpawn {
        spec: SpawnSpec {
            bin,
            args,
            cwd: cwd.to_path_buf(),
            prompt: prompt_delivery,
            timeout,
        },
        signatures,
    })
}

/// Spawn, observe, record. The one place a sub-agent process is created.
/// Every check that could still fail the dispatch has already run, in
/// `prepare_spawn`.
fn run_dispatch(plan: Plan<'_>, prepared: PreparedSpawn) -> Result<DispatchOutcome> {
    let PreparedSpawn { spec, signatures } = prepared;

    let agent_id = plan.agent_id;
    let log_path = plan.paths.logs.join(format!("{agent_id}.jsonl"));

    // The registry record carries the pid, which exists only once the process
    // does -- hence writing it from `on_spawn` rather than before or after the
    // run. The callback cannot fail the spawn, so its result is carried out
    // and read here.
    let mut registered: Option<Result<()>> = None;
    let outcome = spawn::run(&spec, |pid| {
        registered = Some(record_start(
            plan.paths,
            &log_path,
            &SubagentRecord {
                agent_id: agent_id.to_string(),
                cli: plan.entry.cli.id.clone(),
                model: plan.model.to_string(),
                role: plan.role.clone(),
                task_id: plan.task_id.to_string(),
                pid: Some(pid),
                started_at: now_iso8601(),
            },
        ));
    })?;

    // Past this line the process has run: it took a quota slot and it cost
    // money, whatever mana manages to write about it. Bookkeeping failures are
    // therefore collected and reported, never `?`-ed -- an unwritable `~/.mana`
    // used to turn a finished executor into `never started:`, which reads to
    // the PM as "nothing happened, dispatch it again" and pays for the same
    // work twice over a local, deterministic failure.
    let mut unrecorded: Vec<String> = Vec::new();
    if let Some(Err(error)) = registered {
        unrecorded.push(format!("registry record: {error:#}"));
    }

    // `mana kill` appends a `killed` line to this agent's log *before* it
    // signals the process group, so the marker is already on disk by the time
    // the spawner returns. It has to be read before the signatures get a look
    // in: an operator-killed run is otherwise classified on its captured
    // output alone, and a CLI that merely printed the words "rate limit"
    // would have an operator's deliberate kill recorded as the vendor's
    // quota failure -- resting the busiest pool for an hour and sending the PM
    // to a pricier model for no reason. See `classify_killed` for the rule.
    let killed = classify_killed(
        outcome.exit_code,
        outcome.timed_out,
        killed_by_operator(&log_path),
    );
    let failure_means = if killed {
        None
    } else {
        match_signatures(&signatures, &outcome)
    };
    if let Err(error) = append_log(
        &log_path,
        &ExitEntry {
            status: Status::Done,
            action: "exited".to_string(),
            timestamp: now_iso8601(),
            exit_code: outcome.exit_code,
            // Saturating rather than failing: a duration that overflows a u64
            // of milliseconds (584 million years) is not worth a branch that
            // could lose the whole exit record.
            duration_ms: Some(u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX)),
            // Token usage needs the CLI's structured output parsed, which is
            // the PM driver's half of the catalogue (`[pm.events].usage`).
            // Sub-agent enrichment is deliberately left for later; absent is
            // honest, zero would not be.
            failure_means: failure_means.map(|means| failure_wire_name(means).to_string()),
        },
    ) {
        unrecorded.push(format!("exit record: {error:#}"));
    }

    Ok(DispatchOutcome {
        agent_id: agent_id.to_string(),
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        killed,
        duration: outcome.duration,
        failure_means,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        log_path,
        bookkeeping_error: (!unrecorded.is_empty()).then(|| unrecorded.join("; ")),
    })
}

/// Whether an operator asked for this dispatch to die.
///
/// The marker is the `killed` action `cli::kill` appends to the agent's own log
/// on the way to the signal -- the same file, because `agent_id` is now one id
/// end to end. Read from disk rather than passed in memory because the killer
/// is usually a *different* process (`mana kill`, or the teardown at the end of
/// a PM session) than the one supervising the run.
///
/// An unreadable log answers "no": guessing "deliberate" for a death mana
/// cannot account for would suppress a real quota signature, and that is the
/// expensive direction to be wrong in.
fn killed_by_operator(log_path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return false;
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<LogEntry>(line).ok())
        .any(|entry| entry.action == "killed")
}

/// Whether a finished run counts as an operator kill rather than a vendor
/// failure to run the signatures against.
///
/// The marker is the fact; the exit code is only a platform detail of how a
/// kill ends a process -- unix leaves no exit code (a signal), Windows's
/// `taskkill /T /F` leaves one, same as a timeout does there too. So this
/// asks the marker, not the exit code, and only excludes two cases: a
/// timeout, which is classified as a timeout everywhere and never a kill;
/// and a clean exit (code 0), because the marker can land on disk just as a
/// process is already finishing on its own, and a run that succeeded by
/// itself must not be reported as killed just because the marker exists.
/// One rule for both platforms -- no `cfg(windows)` needed.
fn classify_killed(exit_code: Option<i32>, timed_out: bool, operator_marker: bool) -> bool {
    !timed_out && operator_marker && exit_code != Some(0)
}

/// One line of fact about a finished run: what happened, how long it took, the
/// failure signature if the catalogue recognised one, and anything mana failed
/// to write down about it.
///
/// Lives here, next to the outcome it renders, because both readers must agree:
/// `mana dev` prints it for a human and `crate::mcp` puts it in front of the PM
/// (with a cause-matched next step appended). Two copies of this drifted apart
/// once already -- one of them still called an operator's kill a signal death.
pub fn describe(outcome: &DispatchOutcome) -> String {
    let status = if outcome.timed_out {
        "timed out".to_string()
    } else if outcome.killed {
        "killed by `mana kill`".to_string()
    } else {
        match outcome.exit_code {
            Some(code) => format!("exit {code}"),
            None => "killed by a signal".to_string(),
        }
    };
    let failure = outcome
        .failure_means
        .map(|means| format!(", {}", failure_wire_name(means)))
        .unwrap_or_default();
    let unrecorded = outcome
        .bookkeeping_error
        .as_deref()
        .map(|error| format!(" (it ran, but mana could not record it -- {error})"))
        .unwrap_or_default();
    format!(
        "{status} in {:.1}s{failure}{unrecorded}",
        outcome.duration.as_secs_f64()
    )
}

/// Writes both "this agent exists" files. Together, because an agent visible
/// in one and not the other is a state every reader would have to handle.
fn record_start(paths: &ProjectPaths, log_path: &Path, record: &SubagentRecord) -> Result<()> {
    append_record(&paths.subagents_file, record)?;
    append_log(
        log_path,
        &LogEntry {
            status: Status::Running,
            action: "started".to_string(),
            timestamp: now_iso8601(),
        },
    )
}

fn project_paths(project_root: &Path, mana_home: &Path) -> Result<ProjectPaths> {
    let paths = resolve_project_paths(mana_home, &project_name_from_dir(project_root));
    ensure_project_structure(&paths)?;
    Ok(paths)
}

/// Assembles the sub-agent argv from the catalogue: base args, then the
/// auto-approve flags (a sub-agent runs unattended -- an interactive
/// permission prompt would hang it until its timeout), then the model flags.
fn build_argv(entry: &CliEntry, model: &str, prompt: &str, cwd: &Path) -> Result<Vec<String>> {
    let cwd = cwd.to_string_lossy();
    let vars = HashMap::from([("model", model), ("prompt", prompt), ("cwd", cwd.as_ref())]);

    let subagent = &entry.subagent;
    let templates = [
        ("[subagent].args", &subagent.args),
        ("[subagent].auto_approve_args", &subagent.auto_approve_args),
        ("[subagent].model_args", &subagent.model_args),
    ];
    let mut argv = Vec::new();
    for (field, template) in templates {
        argv.extend(
            substitute(template, &vars).with_context(|| format!("{}: {field}", entry.cli.id))?,
        );
    }

    // Where the prompt sits in argv is catalogue knowledge, but no measured
    // entry states it for sub-agents: every headless invocation takes it as
    // the trailing positional. An entry that does place it explicitly (a
    // `{prompt}` placeholder, the way the oneshot PM args do) is honoured as
    // written and gets nothing appended.
    let placed_by_template = templates
        .iter()
        .flat_map(|(_, template)| template.iter())
        .any(|arg| arg.contains("{prompt}"));
    if subagent.prompt == PromptMode::Argv && !placed_by_template {
        argv.push(prompt.to_string());
    }
    Ok(argv)
}

/// Strips a role template's maintainer header: `#` comment lines documenting
/// the placeholders, ended by a line that is exactly `---`.
///
/// Line-exact, and not a search for the first `---` anywhere: the executor
/// prompt ends with a `--- Task {task_id}: ... ---` banner the agent is meant
/// to see, and a looser split would cut the template in half.
fn template_body<'a>(source: &'a str, name: &str) -> Result<&'a str> {
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Ok(&source[offset..]);
        }
    }
    bail!("role template {name} has no '---' line ending its header comment");
}

/// Fills a role template.
///
/// Plain `.replace()` rather than `catalog::substitute`: that function closes
/// the placeholder set to argv tokens and rejects everything else, and the
/// reviewer template *shows the agent a literal JSON object* -- `{ "verdict":
/// ... }` would be read as an unknown placeholder and fail every review
/// dispatch. A role template is prose for a model, not an argv array; nothing
/// downstream can mistake a leftover brace for a flag.
///
/// Replacements are applied in order and each pass rescans the whole string,
/// so free text goes last: a task body that happens to contain the literal
/// `{worktree}` must reach the agent as written, not be substituted twice.
fn fill(template: &str, replacements: &[(&str, &str)]) -> String {
    replacements
        .iter()
        .fold(template.to_string(), |filled, (token, value)| {
            filled.replace(token, value)
        })
}

/// One catalogue failure signature, regexes compiled.
#[derive(Debug)]
struct Signature {
    means: FailureMeans,
    exit_codes: Option<Vec<i32>>,
    stderr: Option<Regex>,
    stdout: Option<Regex>,
}

fn compile_signatures(failures: &[Failure]) -> Result<Vec<Signature>> {
    failures
        .iter()
        .enumerate()
        .map(|(index, failure)| {
            Ok(Signature {
                means: failure.means,
                exit_codes: failure.exit_codes.clone(),
                stderr: compile(failure.stderr_regex.as_deref(), index, "stderr_regex")?,
                stdout: compile(failure.stdout_regex.as_deref(), index, "stdout_regex")?,
            })
        })
        .collect()
}

fn compile(pattern: Option<&str>, index: usize, field: &str) -> Result<Option<Regex>> {
    pattern
        .map(|pattern| {
            Regex::new(pattern).with_context(|| {
                format!(
                    "[[failure]] #{} has an unusable {field} {pattern:?}",
                    index + 1
                )
            })
        })
        .transpose()
}

/// Matches a finished run against the catalogue's ordered signatures: the
/// first one that matches wins, so the file's order is load-bearing (design
/// §8). Every constraint a signature declares must hold; the ones it omits
/// are simply not checked.
///
/// Only a failed run is matched. Signatures are patterns like `rate.?limit`
/// with no exit code attached, and an executor that *succeeded* while writing
/// about rate limiting would otherwise put the whole quota pool on cooldown --
/// the schema is explicit that exhaustion is read "from the output of a run
/// that already failed".
///
/// The outputs may have been truncated at the capture ceiling, which is the
/// right trade: a CLI that gives up prints its refusal early, so the
/// signature is in the head that was kept.
fn match_signatures(signatures: &[Signature], outcome: &SpawnOutcome) -> Option<FailureMeans> {
    if outcome.exit_code == Some(0) {
        return None;
    }
    signatures
        .iter()
        .find(|signature| {
            signature
                .exit_codes
                .as_ref()
                .is_none_or(|codes| outcome.exit_code.is_some_and(|code| codes.contains(&code)))
                && signature
                    .stderr
                    .as_ref()
                    .is_none_or(|regex| regex.is_match(&outcome.stderr))
                && signature
                    .stdout
                    .as_ref()
                    .is_none_or(|regex| regex.is_match(&outcome.stdout))
        })
        .map(|signature| signature.means)
}

/// The name a matched signature gets on the wire.
///
/// Spelled out here rather than derived from the catalogue enum because the
/// two are different contracts that only happen to agree today: the catalogue
/// is parse-only input from outside humans, while these strings are mana's
/// own log format, already matched literally by `log::counters`. A test pins
/// them to each other so the coincidence cannot drift silently.
pub fn failure_wire_name(means: FailureMeans) -> &'static str {
    match means {
        FailureMeans::QuotaExhausted => "quota_exhausted",
        FailureMeans::RateLimited => "rate_limited",
        FailureMeans::AuthExpired => "auth_expired",
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::catalog::{Catalog, CliEntry, Failure, parse_entry};

    /// The `[subagent]` keys a test supplies when the invocation shape is not
    /// what it is testing: no flags, prompt as the trailing positional.
    pub const PLAIN_SUBAGENT: &str = "args = []\nprompt = \"argv\"";

    /// The healthy `rate_limit_event` frame as claude really prints it: one
    /// whole JSONL line, straight out of the recorded golden rather than
    /// hand-written, so a test cannot assert against a claude that never was.
    pub fn golden_rate_limit_frame() -> String {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/catalog/goldens/claude/single-turn.jsonl"
        ))
        .expect("the claude golden is in the repo");
        golden
            .lines()
            .find(|line| line.contains(r#""type":"rate_limit_event""#))
            .expect("the golden carries a rate_limit_event frame")
            .to_string()
    }

    /// claude's failure signatures as shipped, never a copy of them.
    pub fn shipped_claude_signatures() -> Vec<Failure> {
        Catalog::embedded()
            .expect("the embedded catalogue parses")
            .get("claude")
            .expect("claude is in the embedded catalogue")
            .failure
            .clone()
    }

    /// A complete catalogue entry for a CLI that does not exist, so a test can
    /// dispatch a shell script and still go through the real parse/validate
    /// path -- a hand-built struct would drift from the schema the moment a
    /// field is added.
    ///
    /// `subagent` fills the rest of the `[subagent]` table (`args` and
    /// `prompt` are required there); `failures` appends `[[failure]]` tables,
    /// which have to come after every other table header.
    pub fn fixture_entry(bin: &str, subagent: &str, failures: &str) -> CliEntry {
        let source = format!(
            r#"
schema = 1
notes = "test fixture"

[cli]
id = "fixture"
name = "Fixture CLI"
bin = "{bin}"
version_args = ["--version"]

[pm]
driver = "stream"
args = ["-p"]
prompt = "stdin-jsonl"

[pm.events]
text = "$.text"

[tools]
channel = "mcp"

[subagent]
max_concurrent = 0
cwd_required_in_brief = false
{subagent}

[models]
# Discovery declared and no curated list, so any model id a test invents is
# accepted by `routing::validate_model` -- the escape hatch that entry is for.
discovery_args = ["--models"]

[skills]
dirs = []

[install]
url = "https://example.invalid/fixture"
{failures}
"#
        );
        parse_entry(&source).expect("fixture entry must parse")
    }
}

/// The shared repo fixture: a throwaway git project, a throwaway `~/.mana`,
/// and a directory to drop fake CLIs into. Lives here rather than in each test
/// module because `cli::dev` drives the very same dispatches end to end.
#[cfg(all(test, unix))] // fake CLIs are shell scripts
pub(crate) mod test_fixture {
    use crate::project::{ProjectPaths, project_name_from_dir, resolve_project_paths};
    use crate::task::{Role, Task, TaskFrontmatter};
    use std::path::PathBuf;
    use std::process::Command;

    pub const TASK_ID: &str = "task-0001";

    /// The executor half of a fake CLI: record the prompt, commit something,
    /// report. `$1` is mana's prompt, which is what lets a test assert that
    /// the role template really was filled and delivered.
    pub const COMMITTING_EXECUTOR: &str = r#"
printf '%s' "$1" > prompt.txt
echo work > done.txt
git add -A
git commit -q -m "mana: fixture executor"
echo "executor finished"
"#;

    pub struct Fixture {
        _tmp: tempfile::TempDir,
        pub project: PathBuf,
        pub mana_home: PathBuf,
        pub bin_dir: PathBuf,
    }

    impl Fixture {
        pub fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            let bin_dir = tmp.path().join("bin");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::create_dir_all(&bin_dir).unwrap();

            let fixture = Fixture {
                mana_home: tmp.path().join("mana-home"),
                _tmp: tmp,
                project,
                bin_dir,
            };
            fixture.git(&["-c", "init.defaultBranch=main", "init", "-q"]);
            std::fs::write(fixture.project.join("README.md"), "base\n").unwrap();
            fixture.git(&["add", "-A"]);
            fixture.git(&["commit", "-q", "-m", "base"]);
            fixture
        }

        /// Identity on the command line rather than in the environment: these
        /// tests must not depend on the developer's global git config, and the
        /// executor's own commits get theirs from the worktree config mana
        /// writes.
        pub fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.project)
                .args([
                    "-c",
                    "user.name=seed",
                    "-c",
                    "user.email=seed@example.invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        /// Writes an executable fake CLI and returns its absolute path, ready
        /// to be a catalogue entry's `bin`.
        pub fn script(&self, name: &str, body: &str) -> String {
            crate::subprocess::write_executable(&self.bin_dir, name, &format!("#!/bin/sh\n{body}"))
                .to_string_lossy()
                .into_owned()
        }

        pub fn paths(&self) -> ProjectPaths {
            resolve_project_paths(&self.mana_home, &project_name_from_dir(&self.project))
        }

        pub fn review_path(&self) -> PathBuf {
            self.paths().reviews.join(format!("{TASK_ID}.json"))
        }
    }

    pub fn task() -> Task {
        Task {
            frontmatter: TaskFrontmatter {
                id: TASK_ID.to_string(),
                title: "Add a marker file".to_string(),
                role: Role::Executor,
                depends_on: vec![],
            },
            body: "# Brief\n\nCreate done.txt and commit it.\n".to_string(),
        }
    }
}

// Ungated (no `unix` gate) on purpose: `dispatch_tests` below spawns
// shell-script fixtures and compiles out on Windows, so the pure-function
// tests in here -- including `classify_killed`'s -- are the only coverage
// this module's Windows-relevant logic gets on a Windows build.
#[cfg(test)]
mod tests {
    use super::test_support::{
        PLAIN_SUBAGENT, fixture_entry, golden_rate_limit_frame, shipped_claude_signatures,
    };
    use super::*;

    fn outcome(exit_code: Option<i32>, stdout: &str, stderr: &str) -> SpawnOutcome {
        SpawnOutcome {
            pid: 1234,
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            duration: Duration::from_millis(10),
            timed_out: false,
        }
    }

    #[test]
    fn classify_killed_an_exit_code_with_the_marker_is_killed() {
        // Windows shape: `taskkill /T /F` ends the process with a real exit
        // code, not a signal, so a non-zero code must not by itself defeat
        // the marker.
        assert!(classify_killed(Some(1), false, true));
    }

    #[test]
    fn classify_killed_the_same_exit_code_without_the_marker_is_not_killed() {
        assert!(!classify_killed(Some(1), false, false));
    }

    #[test]
    fn classify_killed_a_clean_exit_with_the_marker_is_not_killed() {
        // The marker can land on disk just as the process is already
        // exiting on its own; a successful run must not be reported as
        // killed just because it exists.
        assert!(!classify_killed(Some(0), false, true));
    }

    #[test]
    fn classify_killed_a_timeout_is_never_a_kill() {
        assert!(!classify_killed(Some(1), true, false));
    }

    #[test]
    fn template_body_drops_the_header_up_to_the_separator_line() {
        let source = "# header\n#   {token}   what it is\n---\nprompt line\n";
        assert_eq!(template_body(source, "x.md").unwrap(), "prompt line\n");
    }

    #[test]
    fn template_body_keeps_a_separator_that_is_not_alone_on_its_line() {
        let source = "# header\n---\n--- Task 1: title ---\nbody\n";
        assert_eq!(
            template_body(source, "x.md").unwrap(),
            "--- Task 1: title ---\nbody\n"
        );
    }

    #[test]
    fn template_body_without_a_separator_is_an_error_naming_the_file() {
        let error = template_body("# only a header\n", "executor.md").unwrap_err();
        assert!(error.to_string().contains("executor.md"), "{error}");
    }

    #[test]
    fn embedded_templates_lose_their_header_and_keep_their_placeholders() {
        for (name, source, tokens) in [
            (
                "executor.md",
                EXECUTOR_TEMPLATE,
                &["{task_id}", "{task_title}", "{task_body}", "{worktree}"][..],
            ),
            (
                "reviewer.md",
                REVIEWER_TEMPLATE,
                &[
                    "{task_id}",
                    "{task_title}",
                    "{task_body}",
                    "{worktree}",
                    "{base_ref}",
                    "{review_path}",
                ][..],
            ),
        ] {
            let body = template_body(source, name).unwrap();
            assert!(
                !body.contains("# mana"),
                "{name} kept its maintainer header"
            );
            for token in tokens {
                assert!(body.contains(token), "{name} lost {token}");
            }
        }
    }

    #[test]
    fn fill_substitutes_every_token_and_leaves_literal_json_braces_alone() {
        // The reason this uses `.replace()` and not `catalog::substitute`:
        // the reviewer template hands the agent a literal JSON object.
        let body = template_body(REVIEWER_TEMPLATE, "reviewer.md").unwrap();
        let filled = fill(
            body,
            &[
                ("{worktree}", "/tmp/wt"),
                ("{base_ref}", "abc123"),
                ("{review_path}", "/tmp/reviews/t1.json"),
                ("{task_id}", "t1"),
                ("{task_title}", "Title"),
                ("{task_body}", "Brief"),
            ],
        );
        for token in [
            "{worktree}",
            "{base_ref}",
            "{review_path}",
            "{task_id}",
            "{task_title}",
            "{task_body}",
        ] {
            assert!(!filled.contains(token), "{token} survived substitution");
        }
        assert!(filled.contains("\"verdict\": \"validated\""), "{filled}");
        assert!(filled.contains("/tmp/reviews/t1.json"), "{filled}");
    }

    #[test]
    fn fill_does_not_rescan_substituted_free_text() {
        let filled = fill(
            "worktree={worktree}\nbody={task_body}\n",
            &[
                ("{worktree}", "/tmp/wt"),
                ("{task_body}", "mention {worktree} verbatim"),
            ],
        );
        assert_eq!(
            filled,
            "worktree=/tmp/wt\nbody=mention {worktree} verbatim\n"
        );
    }

    #[test]
    fn executor_and_reviewer_templates_agree_on_test_scope() {
        // Ensures executor permission and reviewer exception stay in sync.
        // Failure: executor says tests that cover the brief are OK, but reviewer
        // rejects them as scope violations (out-of-sync templates cost a rerun).
        // This assertion is deliberately loose to tolerate rewording: match on
        // durable tokens (mentions of tests, and scope-violation rules) within
        // relevant passages, not on exact sentences. A test pinned to prose pays
        // for every doc edit.
        let executor_body = template_body(EXECUTOR_TEMPLATE, "executor.md").unwrap();
        let reviewer_body = template_body(REVIEWER_TEMPLATE, "reviewer.md").unwrap();

        let executor_grants_tests = executor_body
            .split("\n\n")
            .any(|para| para.contains("test") && para.contains("this brief"));
        assert!(
            executor_grants_tests,
            "executor template must grant permission for tests that cover the brief"
        );

        let reviewer_exempts_tests = reviewer_body
            .split("\n\n")
            .any(|para| para.contains("test") && para.contains("scope violation"));
        assert!(
            reviewer_exempts_tests,
            "reviewer template must exempt tests from scope violation rejection"
        );
    }

    #[test]
    fn argv_is_base_then_auto_approve_then_model_then_the_prompt() {
        let entry = fixture_entry(
            "fixture",
            r#"args = ["-p"]
auto_approve_args = ["--yes"]
model_args = ["--model", "{model}"]
prompt = "argv""#,
            "",
        );
        let argv = build_argv(&entry, "haiku", "the brief", Path::new("/tmp/wt")).unwrap();
        assert_eq!(argv, ["-p", "--yes", "--model", "haiku", "the brief"]);
    }

    #[test]
    fn argv_honours_a_template_that_places_the_prompt_itself() {
        let entry = fixture_entry(
            "fixture",
            "args = [\"-p\", \"{prompt}\"]\nprompt = \"argv\"",
            "",
        );
        let argv = build_argv(&entry, "haiku", "the brief", Path::new("/tmp/wt")).unwrap();
        assert_eq!(argv, ["-p", "the brief"]);
    }

    #[test]
    fn argv_leaves_the_prompt_out_when_it_travels_on_stdin() {
        let entry = fixture_entry("fixture", "args = [\"-p\"]\nprompt = \"stdin\"", "");
        let argv = build_argv(&entry, "haiku", "the brief", Path::new("/tmp/wt")).unwrap();
        assert_eq!(argv, ["-p"]);
    }

    #[test]
    fn argv_fills_the_cwd_placeholder_with_the_worktree() {
        let entry = fixture_entry(
            "fixture",
            "args = [\"--cwd\", \"{cwd}\"]\nprompt = \"argv\"",
            "",
        );
        let argv = build_argv(&entry, "haiku", "brief", Path::new("/tmp/wt")).unwrap();
        assert_eq!(argv, ["--cwd", "/tmp/wt", "brief"]);
    }

    #[test]
    fn build_argv_is_role_agnostic_so_all_dispatches_receive_auto_approve_args() {
        // This documents the current state: build_argv has no role parameter,
        // so every dispatch -- executor or reviewer -- receives the same
        // auto_approve_args from the catalogue. The reviewer is "read-only"
        // only via a prompt contract, not by mechanical enforcement. See design §9.
        let entry = fixture_entry(
            "fixture",
            r#"args = ["-p"]
auto_approve_args = ["--dangerously-skip-permissions"]
model_args = ["--model", "{model}"]
prompt = "argv""#,
            "",
        );
        let argv = build_argv(&entry, "haiku", "the brief", Path::new("/tmp/wt")).unwrap();
        assert!(
            argv.contains(&"--dangerously-skip-permissions".to_string()),
            "auto_approve_args missing from argv: {argv:?}"
        );
    }

    #[test]
    fn a_template_placeholder_with_no_sub_agent_value_names_the_field() {
        // `{config_path}` is a PM/MCP token: a sub-agent dispatch has no value
        // for it, and the error has to say which field asked.
        let entry = fixture_entry(
            "fixture",
            "args = [\"--mcp\", \"{config_path}\"]\nprompt = \"argv\"",
            "",
        );
        let error = build_argv(&entry, "haiku", "brief", Path::new("/tmp/wt")).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("config_path"), "{rendered}");
        assert!(rendered.contains("[subagent].args"), "{rendered}");
    }

    #[test]
    fn first_matching_signature_wins() {
        let entry = fixture_entry(
            "fixture",
            PLAIN_SUBAGENT,
            r#"
[[failure]]
means = "auth_expired"
stderr_regex = "not logged in"

[[failure]]
means = "quota_exhausted"
exit_codes = [1]
stdout_regex = "402"

[[failure]]
means = "rate_limited"
stdout_regex = "4"
"#,
        );
        let signatures = compile_signatures(&entry.failure).unwrap();
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(1), "error 402", "")),
            Some(FailureMeans::QuotaExhausted)
        );
        // Same output, wrong exit code: the quota signature declares
        // `exit_codes`, so it must not match, and the looser one behind it does.
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(2), "error 402", "")),
            Some(FailureMeans::RateLimited)
        );
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(2), "", "not logged in")),
            Some(FailureMeans::AuthExpired)
        );
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(1), "just broken", "")),
            None
        );
    }

    #[test]
    fn a_successful_run_never_matches_a_signature() {
        // A bare `rate.?limit` on stdout -- the shape claude's entry carried
        // until #164. An executor that succeeded at a task *about* rate
        // limiting would put the whole pool on cooldown if success were
        // matched.
        let entry = fixture_entry(
            "fixture",
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"rate_limited\"\nstdout_regex = \"rate.?limit\"\n",
        );
        let signatures = compile_signatures(&entry.failure).unwrap();
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(0), "added rate-limit tests", "")),
            None
        );
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(1), "rate limit reached", "")),
            Some(FailureMeans::RateLimited)
        );
    }

    /// The other half of #164: narrowing claude's signature must not blind
    /// mana to a refusal the vendor really issued.
    ///
    /// What this can and cannot claim. The repo holds no recorded claude
    /// refusal -- `catalog/goldens/` has a captured quota failure for copilot
    /// only, and both claude goldens are healthy runs. So the second assertion
    /// pins the *assumption the fix rests on*, spelled out where it can be
    /// read: a refused frame is the golden's frame with a different
    /// `rate_limit_info.status`. If claude words its refusal some other way,
    /// the signature is dead weight -- the same blindness as declaring none,
    /// which is what agy and opencode already do, and never a false cooldown.
    /// The first assertion is the part that is measured: the healthy frame,
    /// at a failing exit code, must match nothing.
    #[test]
    fn claudes_signature_separates_a_refused_frame_from_the_healthy_one() {
        let signatures = compile_signatures(&shipped_claude_signatures()).unwrap();
        let healthy = golden_rate_limit_frame();
        let refused = healthy.replace(r#""status":"allowed""#, r#""status":"rejected""#);
        assert_ne!(refused, healthy, "the golden frame is no longer 'allowed'");

        assert_eq!(
            match_signatures(&signatures, &outcome(Some(1), &healthy, "")),
            None,
            "a healthy frame was read as the vendor's quota failure"
        );
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(1), &refused, "")),
            Some(FailureMeans::RateLimited),
            "a refused frame no longer matches: mana is blind to a real limit"
        );
    }

    #[test]
    fn a_signalled_run_is_matched_on_its_output_alone() {
        // A timeout kills the process, so there is no exit code to match on --
        // a signature that only constrains output must still apply.
        let entry = fixture_entry(
            "fixture",
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"quota_exhausted\"\nstdout_regex = \"quota\"\n",
        );
        let signatures = compile_signatures(&entry.failure).unwrap();
        assert_eq!(
            match_signatures(&signatures, &outcome(None, "out of quota", "")),
            Some(FailureMeans::QuotaExhausted)
        );
    }

    #[test]
    fn an_exit_code_only_signature_ignores_the_output() {
        let entry = fixture_entry(
            "fixture",
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"auth_expired\"\nexit_codes = [7]\n",
        );
        let signatures = compile_signatures(&entry.failure).unwrap();
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(7), "", "")),
            Some(FailureMeans::AuthExpired)
        );
        assert_eq!(
            match_signatures(&signatures, &outcome(Some(8), "", "")),
            None
        );
    }

    #[test]
    fn an_unusable_catalogue_regex_is_loud() {
        // Not reachable through `parse_entry` -- catalogue validation checks
        // that a signature has *something* to match on, not that it compiles --
        // so the dispatcher is where a broken pattern has to be caught.
        let failures = vec![Failure {
            means: FailureMeans::RateLimited,
            exit_codes: None,
            stderr_regex: None,
            stdout_regex: Some("rate(limit".to_string()),
            cooldown_minutes: None,
        }];
        let error = compile_signatures(&failures).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("stdout_regex"), "{rendered}");
        assert!(rendered.contains("rate(limit"), "{rendered}");
    }

    #[test]
    fn failure_wire_names_match_the_catalogue_spelling() {
        // The log strings and the catalogue's own enum names must stay equal:
        // `log::counters` matches "quota_exhausted" literally.
        for name in ["quota_exhausted", "rate_limited", "auth_expired"] {
            let entry = fixture_entry(
                "fixture",
                PLAIN_SUBAGENT,
                &format!("[[failure]]\nmeans = \"{name}\"\nexit_codes = [1]\n"),
            );
            assert_eq!(failure_wire_name(entry.failure[0].means), name);
        }
    }

    #[test]
    fn the_shipped_catalogue_signatures_all_compile() {
        // The embedded entries are the ones that will actually run; a pattern
        // that only fails at dispatch time would fail in front of a user.
        for entry in crate::catalog::Catalog::embedded().unwrap().entries() {
            compile_signatures(&entry.failure)
                .unwrap_or_else(|error| panic!("{}: {error:#}", entry.cli.id));
        }
    }

    #[test]
    fn the_fixture_entry_is_a_valid_catalogue_entry() {
        // Guards the tests themselves: if the schema gains a required field,
        // this fails here rather than in every dispatch test at once.
        let entry = fixture_entry("bin", PLAIN_SUBAGENT, "");
        assert_eq!(entry.cli.id, "fixture");
        assert_eq!(entry.cli.bin(), "bin");
    }
}

#[cfg(all(test, unix))] // every test here dispatches a shell-script CLI
mod dispatch_tests {
    use super::test_fixture::{COMMITTING_EXECUTOR, Fixture, TASK_ID, task};
    use super::test_support::{
        PLAIN_SUBAGENT, fixture_entry, golden_rate_limit_frame, shipped_claude_signatures,
    };
    use super::*;
    use crate::lock::load_registry;
    use std::process::Command;

    /// A fresh dispatch id, the way every caller mints one.
    fn agent_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// One assignment against the shared fixture, on the one model these tests
    /// care nothing about.
    fn assign<'a>(
        agent_id: &'a str,
        entry: &'a CliEntry,
        task: &'a Task,
        fixture: &'a Fixture,
    ) -> Assignment<'a> {
        Assignment {
            agent_id,
            entry,
            model: "cheapo",
            task,
            project_root: &fixture.project,
            mana_home: &fixture.mana_home,
        }
    }

    /// Runs the executor with a fake CLI whose only distinguishing feature is
    /// its script body -- the shape every test here starts from.
    fn executor_with(fixture: &Fixture, script: &str) -> ExecutorRun {
        let bin = fixture.script("exec-cli", script);
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");
        dispatch_executor(&assign(&agent_id(), &entry, &task(), fixture)).unwrap()
    }

    #[test]
    fn executor_runs_in_a_fresh_worktree_and_its_commit_lands_in_the_diff() {
        let fixture = Fixture::new();
        let run = executor_with(&fixture, COMMITTING_EXECUTOR);

        assert_eq!(run.outcome.exit_code, Some(0));
        assert!(!run.outcome.timed_out);
        assert!(run.outcome.succeeded());
        assert_eq!(run.outcome.failure_means, None);
        assert!(run.outcome.stdout.contains("executor finished"));
        assert_eq!(run.worktree.branch, format!("mana/{TASK_ID}"));

        // The agent worked in the worktree, not in the project checkout.
        assert!(run.worktree.path.join("done.txt").exists());
        assert!(!fixture.project.join("done.txt").exists());

        let diff = Command::new("git")
            .arg("-C")
            .arg(&run.worktree.path)
            .args(["diff", "--name-only"])
            .arg(format!("{}..HEAD", run.worktree.base_ref))
            .output()
            .unwrap();
        let changed = String::from_utf8_lossy(&diff.stdout);
        assert!(changed.contains("done.txt"), "{changed}");
    }

    #[test]
    fn executor_prompt_is_the_filled_role_template() {
        let fixture = Fixture::new();
        let run = executor_with(&fixture, COMMITTING_EXECUTOR);

        let prompt = std::fs::read_to_string(run.worktree.path.join("prompt.txt")).unwrap();
        assert!(prompt.starts_with("You are an executor"), "{prompt}");
        assert!(
            !prompt.contains("# mana executor prompt template"),
            "the maintainer header reached the agent"
        );
        assert!(prompt.contains(&run.worktree.path.to_string_lossy().to_string()));
        assert!(prompt.contains("Create done.txt and commit it."));
        assert!(prompt.contains(&format!("mana:{TASK_ID}")));
        assert!(prompt.contains("Add a marker file"));
    }

    #[test]
    fn a_dispatch_writes_a_registry_record_and_an_exit_record() {
        let fixture = Fixture::new();
        let run = executor_with(&fixture, COMMITTING_EXECUTOR);

        let paths = fixture.paths();
        let registry = load_registry(&paths.subagents_file).unwrap();
        assert_eq!(registry.records.len(), 1);
        let record = registry
            .records
            .iter()
            .find(|r| r.agent_id == run.outcome.agent_id)
            .unwrap();
        assert_eq!(record.cli, "fixture");
        assert_eq!(record.model, "cheapo");
        assert_eq!(record.role, Role::Executor);
        assert_eq!(record.task_id, TASK_ID);
        assert!(record.pid.is_some(), "a real spawn must report a pid");

        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].contains("\"action\":\"started\""), "{log}");
        assert!(lines[0].contains("\"status\":\"running\""), "{log}");

        let exit: ExitEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(exit.action, "exited");
        assert_eq!(exit.status, Status::Done);
        assert_eq!(exit.exit_code, Some(0));
        assert!(exit.duration_ms.is_some());
        assert_eq!(exit.failure_means, None);
        // The OTEL key is a wire contract, so assert the raw line too.
        assert!(
            lines[1].contains("\"gen_ai.client.operation.duration\""),
            "{log}"
        );
    }

    #[test]
    fn a_matching_failure_signature_reaches_the_outcome_and_the_log() {
        let fixture = Fixture::new();
        // The copilot-shaped signature: exit 1, "402" on stdout, empty stderr.
        let bin = fixture.script("broke-cli", "echo 'HTTP 402 payment required'\nexit 1\n");
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"quota_exhausted\"\nexit_codes = [1]\nstdout_regex = \"402\"\ncooldown_minutes = 60\n",
        );

        let run = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap();

        assert_eq!(run.outcome.exit_code, Some(1));
        assert!(!run.outcome.succeeded());
        assert_eq!(
            run.outcome.failure_means,
            Some(FailureMeans::QuotaExhausted)
        );
        assert!(run.outcome.stderr.is_empty(), "{}", run.outcome.stderr);

        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        assert!(
            log.contains("\"failure_means\":\"quota_exhausted\""),
            "{log}"
        );
        assert!(log.contains("\"exit_code\":1"), "{log}");
    }

    /// #164: claude's only signature was a bare `rate.?limit` on stdout, and
    /// claude prints a `rate_limit_event` frame on every *healthy* turn -- both
    /// recorded goldens carry one, with `"status":"allowed"`. So any claude
    /// dispatch that failed for an unrelated reason was filed as a vendor quota
    /// failure, and `pool_scope = "global"` on pool `plan` put haiku, sonnet
    /// and opus on the same 60-minute cooldown.
    ///
    /// Deliberately not a hand-written string: the stub prints the frame
    /// exactly as it sits in `catalog/goldens/claude/single-turn.jsonl`, and
    /// the signatures are the shipped ones, read out of the embedded
    /// catalogue. Neither half can drift away from what claude really emits.
    #[test]
    fn claudes_healthy_telemetry_is_not_read_as_the_vendors_quota_failure() {
        let fixture = Fixture::new();
        let bin = fixture.script(
            "claude-ish",
            &format!(
                "printf '%s\\n' '{}'\necho 'fatal: the model returned a malformed tool call' >&2\nexit 1\n",
                golden_rate_limit_frame()
            ),
        );
        let mut entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");
        entry.failure = shipped_claude_signatures();

        let run = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap();

        assert_eq!(run.outcome.exit_code, Some(1));
        assert!(
            run.outcome.stdout.contains("rate_limit_event"),
            "the stub never printed the golden frame: {}",
            run.outcome.stdout
        );
        assert_eq!(
            run.outcome.failure_means, None,
            "a healthy rate_limit_event frame was blamed on the vendor's quota"
        );
        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        assert!(!log.contains("rate_limited"), "{log}");
    }

    /// The incident this guards: killing a reviewer produced "killed by a
    /// signal in 44.4s, rate_limited" because claude's `rate.?limit` signature
    /// matched output it had already printed. The PM believed it, told the user
    /// the vendor had throttled them, and rerouted its retry to a pricier
    /// model -- and the same path is one line away from a 60-minute cooldown on
    /// the busiest pool, for a death an operator asked for.
    #[test]
    fn an_operators_kill_is_not_read_as_the_vendors_quota_failure() {
        let fixture = Fixture::new();
        let agent = agent_id();
        let log_path = fixture.paths().logs.join(format!("{agent}.jsonl"));
        // Exactly `mana kill`'s sequence, from inside the process it kills: the
        // `killed` marker into the agent's own log, then the signal.
        let bin = fixture.script(
            "killed-cli",
            &format!(
                "echo 'rate limit reached'\n\
                 printf '%s\\n' '{{\"status\":\"running\",\"action\":\"killed\",\"timestamp\":\"t\"}}' >> '{}'\n\
                 kill -9 $$\n",
                log_path.display()
            ),
        );
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"rate_limited\"\nstdout_regex = \"rate.?limit\"\ncooldown_minutes = 60\n",
        );

        let run = dispatch_executor(&assign(&agent, &entry, &task(), &fixture)).unwrap();

        assert_eq!(run.outcome.exit_code, None);
        assert!(run.outcome.killed, "the kill marker was not read");
        assert_eq!(
            run.outcome.failure_means, None,
            "an operator's kill was blamed on the vendor's quota"
        );
        let line = describe(&run.outcome);
        assert!(line.starts_with("killed by `mana kill`"), "{line}");
        assert!(!line.contains("rate_limited"), "{line}");
        // ...and nothing in the log can put the pool on a cooldown either --
        // `Observations::gather` reads exactly this field.
        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        assert!(!log.contains("rate_limited"), "{log}");
    }

    /// #199: `kill_dispatch` used to signal first and write the `killed`
    /// marker afterwards, so the supervising dispatch had already read the log
    /// and found nothing. The operator's kill was then classified on the
    /// sub-agent's output alone, and a CLI that had merely printed the words
    /// "rate limit" got its whole pool rested for an hour.
    ///
    /// Unlike `an_operators_kill_is_not_read_as_the_vendors_quota_failure`,
    /// which has the doomed process append its own marker, this drives the
    /// real `mana kill` against a real dispatch — the ordering under test only
    /// exists in `kill_dispatch`.
    #[test]
    fn a_real_mana_kill_beats_the_signature_match_to_the_log() {
        let fixture = Fixture::new();
        let agent = agent_id();
        let bin = fixture.script("slow-cli", "echo 'rate limit reached'\nsleep 30\n");
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"rate_limited\"\nstdout_regex = \"rate.?limit\"\ncooldown_minutes = 60\n",
        );

        // The killer runs `mana kill`'s real path as soon as the dispatch has
        // registered a pid, exactly as an operator at another terminal would.
        let mana_home = fixture.mana_home.clone();
        let project_root = fixture.project.clone();
        let wanted = agent.clone();
        let killer = std::thread::spawn(move || {
            let project = crate::project::project_name_from_dir(&project_root);
            let paths = crate::project::resolve_project_paths(&mana_home, &project);
            for _ in 0..600 {
                if let Ok(found) = crate::status::dispatches_in(&mana_home, &project)
                    && let Some(dispatch) = found
                        .iter()
                        .find(|d| d.record.agent_id == wanted && d.record.pid.is_some())
                {
                    return crate::cli::kill::kill_dispatch(&paths, dispatch, chrono::Utc::now())
                        .map(|_| ());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(anyhow::anyhow!("the dispatch never registered a pid"))
        });

        let run = dispatch_executor(&assign(&agent, &entry, &task(), &fixture)).unwrap();
        killer.join().unwrap().expect("mana kill failed");

        assert!(
            run.outcome.killed,
            "the kill marker was not on disk in time"
        );
        assert_eq!(
            run.outcome.failure_means, None,
            "an operator's kill was blamed on the vendor's quota"
        );
        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        assert!(!log.contains("rate_limited"), "{log}");
    }

    /// A signalled run with no kill marker is still classified: the fix must
    /// not blind mana to a CLI the vendor really did cut off mid-run.
    #[test]
    fn a_signalled_run_nobody_asked_for_is_still_matched_against_the_signatures() {
        let fixture = Fixture::new();
        let bin = fixture.script("cut-off-cli", "echo 'rate limit reached'\nkill -9 $$\n");
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"rate_limited\"\nstdout_regex = \"rate.?limit\"\n",
        );

        let run = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap();

        assert!(!run.outcome.killed);
        assert_eq!(run.outcome.failure_means, Some(FailureMeans::RateLimited));
        assert!(describe(&run.outcome).starts_with("killed by a signal"));
    }

    /// The run happened and was billed; `~/.mana` failing to write it down is a
    /// separate, local fact. Reporting it as an error made the whole dispatch
    /// read as "never started", which costs a second full relaunch.
    #[test]
    fn a_finished_run_is_still_a_finished_run_when_mana_cannot_record_it() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        ensure_project_structure(&paths).unwrap();
        // `subagents.jsonl` as a directory: the append fails the way an
        // unwritable `~/.mana` does, and only that append.
        std::fs::create_dir_all(&paths.subagents_file).unwrap();

        let run = executor_with(&fixture, COMMITTING_EXECUTOR);

        assert!(
            run.outcome.succeeded(),
            "a finished executor was turned into a failure"
        );
        let error = run
            .outcome
            .bookkeeping_error
            .as_deref()
            .expect("the lost registry record was never reported");
        assert!(error.contains("registry record"), "{error}");
        let line = describe(&run.outcome);
        assert!(line.starts_with("exit 0 in"), "{line}");
        assert!(line.contains("could not record"), "{line}");
        // The work itself is intact: the whole point of not failing here.
        assert!(run.worktree.path.join("done.txt").exists());
    }

    #[test]
    fn a_failure_with_no_matching_signature_records_no_meaning() {
        let fixture = Fixture::new();
        let bin = fixture.script("broke-cli", "echo 'something else broke' >&2\nexit 4\n");
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"quota_exhausted\"\nexit_codes = [1]\nstdout_regex = \"402\"\n",
        );

        let run = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap();

        assert_eq!(run.outcome.exit_code, Some(4));
        assert_eq!(run.outcome.failure_means, None);
        assert!(run.outcome.stderr.contains("something else broke"));
        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        assert!(!log.contains("failure_means"), "{log}");

        // #170's opposite direction: a run that *did* spawn keeps its worktree
        // and branch even though it exited non-zero -- that quota slot was
        // spent and the executor's (however useless) work lives there.
        assert!(
            run.worktree.path.exists(),
            "a spawned run's worktree was removed"
        );
        // `+` rather than `*` marks a branch checked out in another worktree
        // (ours), not the current one.
        assert_eq!(
            fixture.git(&["branch", "--list", &run.worktree.branch]),
            format!("+ {}", run.worktree.branch)
        );
    }

    /// #170: the CLI resolution check in `prepare_spawn` is one of the
    /// pre-spawn steps that must fail before `worktree::create` runs.
    #[test]
    fn a_dispatch_whose_cli_does_not_resolve_leaves_no_worktree_or_branch() {
        let fixture = Fixture::new();
        let entry = fixture_entry("mana-fixture-cli-does-not-exist", PLAIN_SUBAGENT, "");

        let error = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap_err();

        assert!(error.to_string().contains("cannot dispatch to"), "{error}");
        let worktree_path = worktree::worktree_path(&fixture.mana_home, &fixture.project, TASK_ID);
        assert!(
            !worktree_path.exists(),
            "a dispatch that could not resolve its CLI left a worktree behind"
        );
        assert!(
            fixture
                .git(&["branch", "--list", &format!("mana/{TASK_ID}")])
                .is_empty(),
            "a dispatch that could not resolve its CLI left a branch behind"
        );
    }

    /// #170: an unusable catalogue regex is caught by `prepare_spawn`'s call to
    /// `compile_signatures`, also before `worktree::create` runs.
    #[test]
    fn a_dispatch_with_an_unusable_failure_regex_leaves_no_worktree_or_branch() {
        let fixture = Fixture::new();
        let bin = fixture.script("exec-cli", COMMITTING_EXECUTOR);
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"rate_limited\"\nstdout_regex = \"rate(limit\"\n",
        );

        let error = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap_err();

        assert!(
            error.to_string().contains("invalid failure signature"),
            "{error}"
        );
        let worktree_path = worktree::worktree_path(&fixture.mana_home, &fixture.project, TASK_ID);
        assert!(
            !worktree_path.exists(),
            "a dispatch with an unusable failure regex left a worktree behind"
        );
        assert!(
            fixture
                .git(&["branch", "--list", &format!("mana/{TASK_ID}")])
                .is_empty(),
            "a dispatch with an unusable failure regex left a branch behind"
        );
    }

    #[test]
    fn a_write_role_dispatch_on_a_non_git_project_is_refused_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        let mana_home = tmp.path().join("mana-home");
        std::fs::create_dir_all(&plain).unwrap();
        let entry = fixture_entry("/bin/true", PLAIN_SUBAGENT, "");

        let error = dispatch_executor(&Assignment {
            agent_id: &agent_id(),
            entry: &entry,
            model: "cheapo",
            task: &task(),
            project_root: &plain,
            mana_home: &mana_home,
        })
        .unwrap_err();
        assert!(error.to_string().contains("git init"), "{error}");
        assert!(
            !mana_home.exists(),
            "a refused dispatch left state behind in ~/.mana"
        );
    }

    #[test]
    fn the_prompt_can_travel_on_stdin_instead_of_argv() {
        let fixture = Fixture::new();
        let bin = fixture.script("exec-cli", "cat\n");
        let entry = fixture_entry(&bin, "args = []\nprompt = \"stdin\"", "");

        let run = dispatch_executor(&assign(&agent_id(), &entry, &task(), &fixture)).unwrap();

        assert_eq!(run.outcome.exit_code, Some(0));
        assert!(run.outcome.stdout.starts_with("You are an executor"));
    }

    #[test]
    fn reviewer_runs_in_the_executors_worktree_and_writes_its_verdict() {
        let fixture = Fixture::new();
        let task = task();
        let executor = executor_with(&fixture, COMMITTING_EXECUTOR);

        let review_path = fixture.review_path();
        // The fake reviewer proves the substitution end to end: it echoes the
        // prompt it was handed, and writes its verdict where mana asked.
        let bin = fixture.script(
            "review-cli",
            &format!(
                "printf '%s' \"$1\"\nprintf '%s' '{{\"verdict\":\"validated\",\"issues\":[]}}' > '{}'\n",
                review_path.display()
            ),
        );
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");

        let run = dispatch_reviewer(
            &assign(&agent_id(), &entry, &task, &fixture),
            &executor.worktree,
            None,
        )
        .unwrap();

        assert_eq!(run.outcome.exit_code, Some(0));
        assert_eq!(run.review_path, review_path);
        assert_eq!(
            crate::review::read_verdict(&run.review_path)
                .unwrap()
                .verdict,
            crate::review::Decision::Validated
        );

        let prompt = &run.outcome.stdout;
        assert!(prompt.starts_with("You are a reviewer"), "{prompt}");
        assert!(prompt.contains(&executor.worktree.base_ref), "{prompt}");
        assert!(
            prompt.contains(&executor.worktree.path.to_string_lossy().to_string()),
            "{prompt}"
        );
        assert!(prompt.contains(&review_path.to_string_lossy().to_string()));
        // The original brief, not a re-derived one.
        assert!(
            prompt.contains("Create done.txt and commit it."),
            "{prompt}"
        );

        // Both roles are in the registry, under the same task.
        let registry = load_registry(&fixture.paths().subagents_file).unwrap();
        assert_eq!(registry.records.len(), 2);
        assert_eq!(registry.records[0].role, Role::Executor);
        assert_eq!(registry.records[1].role, Role::Reviewer);
        assert!(registry.records.iter().all(|r| r.task_id == TASK_ID));
    }

    #[test]
    fn reviewer_creates_no_worktree_of_its_own() {
        let fixture = Fixture::new();
        let task = task();
        let executor = executor_with(&fixture, COMMITTING_EXECUTOR);
        let worktrees_before = fixture.git(&["worktree", "list", "--porcelain"]);

        let bin = fixture.script("review-cli", "echo reviewed\n");
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");
        let run = dispatch_reviewer(
            &assign(&agent_id(), &entry, &task, &fixture),
            &executor.worktree,
            None,
        )
        .unwrap();

        assert_eq!(run.outcome.exit_code, Some(0));
        assert_eq!(
            fixture.git(&["worktree", "list", "--porcelain"]),
            worktrees_before
        );
    }

    #[test]
    fn a_correction_is_appended_to_the_reviewer_prompt() {
        let fixture = Fixture::new();
        let task = task();
        let executor = executor_with(&fixture, COMMITTING_EXECUTOR);

        let bin = fixture.script("review-cli", "printf '%s' \"$1\"\n");
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");
        let run = dispatch_reviewer(
            &assign(&agent_id(), &entry, &task, &fixture),
            &executor.worktree,
            Some("Write ONLY the JSON object."),
        )
        .unwrap();

        assert!(
            run.outcome
                .stdout
                .trim_end()
                .ends_with("Write ONLY the JSON object."),
            "{}",
            run.outcome.stdout
        );
    }

    #[test]
    fn a_stale_verdict_is_cleared_before_the_reviewer_runs() {
        let fixture = Fixture::new();
        let task = task();
        let executor = executor_with(&fixture, COMMITTING_EXECUTOR);
        std::fs::write(
            fixture.review_path(),
            r#"{"verdict":"validated","issues":[]}"#,
        )
        .unwrap();

        // A reviewer that writes nothing must leave no verdict behind, or the
        // retry loop would validate a task on a previous run's answer.
        let bin = fixture.script("review-cli", "echo 'I wrote nothing'\n");
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");
        let run = dispatch_reviewer(
            &assign(&agent_id(), &entry, &task, &fixture),
            &executor.worktree,
            None,
        )
        .unwrap();

        assert!(!run.review_path.exists());
    }

    /// The verdict's lifetime is the branch's: `worktree::create` force-resets
    /// `mana/<task id>` on every attempt (#153), so a verdict written about
    /// the previous attempt describes code that is no longer there.
    #[test]
    fn relaunching_the_executor_clears_the_verdict_that_judged_the_old_branch() {
        let fixture = Fixture::new();
        executor_with(&fixture, COMMITTING_EXECUTOR);
        std::fs::write(
            fixture.review_path(),
            r#"{"verdict":"rejected","attribution":"code","issues":["missing error handling"]}"#,
        )
        .unwrap();

        // The retry `get_review`'s own message prescribes.
        executor_with(&fixture, COMMITTING_EXECUTOR);

        assert!(
            !fixture.review_path().exists(),
            "the previous attempt's verdict survived the branch reset"
        );
    }

    /// The verdict path is built from the id in the *task file*, and a task
    /// file is plain text on disk -- the same reason `unmet_dependency`
    /// re-validates a `depends_on` id it has just read back out of one.
    /// Unvalidated, a `..` in that id resolved outside `reviews/` and the
    /// stale-verdict cleanup deleted whatever was there (#187).
    #[test]
    fn a_task_id_that_would_escape_the_reviews_directory_is_refused() {
        let fixture = Fixture::new();
        let executor = executor_with(&fixture, COMMITTING_EXECUTOR);

        let outside = fixture.paths().reviews.join("../canary.json");
        std::fs::write(&outside, "not the reviewer's to delete").unwrap();

        let mut hostile = task();
        hostile.frontmatter.id = "../canary".to_string();
        let bin = fixture.script("review-cli", "echo reviewed\n");
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");

        let error = dispatch_reviewer(
            &assign(&agent_id(), &entry, &hostile, &fixture),
            &executor.worktree,
            None,
        )
        .unwrap_err();

        // Specifically rejected as an id -- not incidentally failing later on
        // some call that happened to choke on it.
        assert!(error.to_string().contains("invalid task id"), "{error}");
        assert!(
            outside.exists(),
            "the reviewer deleted a file outside {}",
            fixture.paths().reviews.display()
        );
    }
}
