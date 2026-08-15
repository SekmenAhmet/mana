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
/// be schema for its own sake.
const EXECUTOR_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Reviewing reads a diff and runs the existing tests -- it never writes the
/// code -- so it gets two thirds of the executor's budget.
const REVIEWER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What mana observed of one sub-agent run. Sub-agents speak no protocol, so
/// this *is* the whole report: everything here is measured from outside the
/// process.
///
/// `agent_id` and the two captured streams have no live reader yet -- they
/// are for `mana ps` (task 4.2) and for the usage enrichment that reads a
/// CLI's structured output -- and are proven by this module's tests in the
/// meantime, the same way `task.rs` carries schema ahead of its consumer.
#[allow(dead_code)]
#[derive(Debug)]
pub struct DispatchOutcome {
    pub agent_id: String,
    /// `None` when the process was killed by a signal -- read together with
    /// `timed_out`.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    /// `Some` when one of the catalogue's ordered failure signatures matched
    /// (design §8): the routing signal that puts a pool on cooldown.
    pub failure_means: Option<FailureMeans>,
    pub stdout: String,
    pub stderr: String,
    pub log_path: PathBuf,
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

/// Dispatches `task` to an executor: a fresh worktree, the executor role
/// prompt, and one process observed to completion.
pub fn dispatch_executor(
    entry: &CliEntry,
    model: &str,
    task: &Task,
    project_root: &Path,
    mana_home: &Path,
) -> Result<ExecutorRun> {
    // The write-role gate (design §9) runs first, before mana creates or
    // writes anything: on a non-git project the user must see "run git init",
    // not whichever git call downstream happened to fail first. `create`
    // re-checks -- cheap, and it keeps that function safe to call on its own.
    worktree::ensure_git_repo(project_root)?;

    let paths = project_paths(project_root, mana_home)?;
    let task_id = &task.frontmatter.id;
    let worktree = worktree::create(project_root, mana_home, task_id)?;
    let worktree_path = worktree.path.to_string_lossy().into_owned();

    let prompt = fill(
        template_body(EXECUTOR_TEMPLATE, "executor.md")?,
        &[
            ("{worktree}", worktree_path.as_str()),
            ("{task_id}", task_id),
            ("{task_title}", &task.frontmatter.title),
            ("{task_body}", &task.body),
        ],
    );

    let outcome = run_dispatch(Plan {
        entry,
        model,
        task_id,
        role: Role::Executor,
        prompt,
        cwd: &worktree.path,
        timeout: EXECUTOR_TIMEOUT,
        paths: &paths,
    })?;

    Ok(ExecutorRun { outcome, worktree })
}

/// Dispatches the review of a finished task.
///
/// No worktree is created: the reviewer is read-only, and the thing it has to
/// read is the executor's checkout, so it runs there with `{base_ref}` from
/// that same worktree. "Read-only" is a prompt contract, not a sandbox --
/// making it mechanical needs per-CLI permission flags, which is task 2.3's
/// problem.
///
/// `correction` is appended to the prompt, and exists for exactly one caller:
/// the single corrective re-dispatch after an unusable verdict (plan 1.4).
pub fn dispatch_reviewer(
    entry: &CliEntry,
    model: &str,
    task: &Task,
    project_root: &Path,
    mana_home: &Path,
    worktree: &WorktreeInfo,
    correction: Option<&str>,
) -> Result<ReviewerRun> {
    let paths = project_paths(project_root, mana_home)?;
    let task_id = &task.frontmatter.id;
    let review_path = paths.reviews.join(format!("{task_id}.json"));

    // A verdict left by an earlier run -- a retry, an abandoned attempt at the
    // same task -- would otherwise be read back as this reviewer's answer, and
    // the corrective re-dispatch would keep "succeeding" on stale bytes.
    if let Err(error) = std::fs::remove_file(&review_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("clearing the previous verdict {}", review_path.display()));
    }

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

    let outcome = run_dispatch(Plan {
        entry,
        model,
        task_id,
        role: Role::Reviewer,
        prompt,
        cwd: &worktree.path,
        timeout: REVIEWER_TIMEOUT,
        paths: &paths,
    })?;

    Ok(ReviewerRun {
        outcome,
        review_path,
    })
}

/// Everything one dispatch needs that the two roles disagree about. A struct
/// rather than eight positional parameters, since half of them are strings
/// and swapping two would compile.
struct Plan<'a> {
    entry: &'a CliEntry,
    model: &'a str,
    task_id: &'a str,
    role: Role,
    prompt: String,
    /// Where the process runs: the task worktree for both roles.
    cwd: &'a Path,
    timeout: Duration,
    paths: &'a ProjectPaths,
}

/// Spawn, observe, record. The one place a sub-agent process is created.
fn run_dispatch(plan: Plan<'_>) -> Result<DispatchOutcome> {
    // Compiled before the spawn, not after: a typo in a catalogue regex is a
    // data bug, and finding it only once a real agent has already burned a
    // quota slot would be the most expensive possible moment to learn about it.
    let signatures = compile_signatures(&plan.entry.failure)
        .with_context(|| format!("{}: invalid failure signature", plan.entry.cli.id))?;

    let args = build_argv(plan.entry, plan.model, &plan.prompt, plan.cwd)?;
    let prompt_delivery = PromptDelivery::from_mode(plan.entry.subagent.prompt, &plan.prompt)?;

    let agent_id = uuid::Uuid::new_v4().to_string();
    let log_path = plan.paths.logs.join(format!("{agent_id}.jsonl"));

    let spec = SpawnSpec {
        bin: plan.entry.cli.bin().to_string(),
        args,
        cwd: plan.cwd.to_path_buf(),
        prompt: prompt_delivery,
        timeout: plan.timeout,
    };

    // The registry record carries the pid, which exists only once the process
    // does -- hence writing it from `on_spawn` rather than before or after the
    // run. The callback cannot fail the spawn, so its result is carried out
    // and checked here.
    let mut registered: Option<Result<()>> = None;
    let outcome = spawn::run(&spec, |pid| {
        registered = Some(record_start(
            plan.paths,
            &log_path,
            &SubagentRecord {
                agent_id: agent_id.clone(),
                cli: plan.entry.cli.id.clone(),
                model: plan.model.to_string(),
                role: plan.role.clone(),
                task_id: plan.task_id.to_string(),
                pid: Some(pid),
                started_at: now_iso8601(),
            },
        ));
    })?;
    if let Some(result) = registered {
        result.context("recording the dispatch in subagents.jsonl")?;
    }

    let failure_means = match_signatures(&signatures, &outcome);
    append_log(
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
            input_tokens: None,
            output_tokens: None,
            failure_means: failure_means.map(|means| failure_wire_name(means).to_string()),
        },
    )?;

    Ok(DispatchOutcome {
        agent_id,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        duration: outcome.duration,
        failure_means,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        log_path,
    })
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
    use crate::catalog::{CliEntry, parse_entry};

    /// The `[subagent]` keys a test supplies when the invocation shape is not
    /// what it is testing: no flags, prompt as the trailing positional.
    pub const PLAIN_SUBAGENT: &str = "args = []\nprompt = \"argv\"";

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
discovery_args = []

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
    use std::os::unix::fs::PermissionsExt;
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
            let path = self.bin_dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
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

#[cfg(test)]
mod tests {
    use super::test_support::{PLAIN_SUBAGENT, fixture_entry};
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
        // The claude entry's real signature: a bare `rate.?limit` on stdout. An
        // executor that succeeded at a task *about* rate limiting would put the
        // whole pool on cooldown if success were matched.
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
    use super::test_support::{PLAIN_SUBAGENT, fixture_entry};
    use super::*;
    use crate::lock::load_registry;
    use std::process::Command;

    /// Runs the executor with a fake CLI whose only distinguishing feature is
    /// its script body -- the shape every test here starts from.
    fn executor_with(fixture: &Fixture, script: &str) -> ExecutorRun {
        let bin = fixture.script("exec-cli", script);
        let entry = fixture_entry(&bin, PLAIN_SUBAGENT, "");
        dispatch_executor(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap()
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
        let record = registry.get(&run.outcome.agent_id).unwrap();
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

        let run = dispatch_executor(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

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

    #[test]
    fn a_failure_with_no_matching_signature_records_no_meaning() {
        let fixture = Fixture::new();
        let bin = fixture.script("broke-cli", "echo 'something else broke' >&2\nexit 4\n");
        let entry = fixture_entry(
            &bin,
            PLAIN_SUBAGENT,
            "[[failure]]\nmeans = \"quota_exhausted\"\nexit_codes = [1]\nstdout_regex = \"402\"\n",
        );

        let run = dispatch_executor(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

        assert_eq!(run.outcome.exit_code, Some(4));
        assert_eq!(run.outcome.failure_means, None);
        assert!(run.outcome.stderr.contains("something else broke"));
        let log = std::fs::read_to_string(&run.outcome.log_path).unwrap();
        assert!(!log.contains("failure_means"), "{log}");
    }

    #[test]
    fn a_write_role_dispatch_on_a_non_git_project_is_refused_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        let mana_home = tmp.path().join("mana-home");
        std::fs::create_dir_all(&plain).unwrap();
        let entry = fixture_entry("/bin/true", PLAIN_SUBAGENT, "");

        let error = dispatch_executor(&entry, "cheapo", &task(), &plain, &mana_home).unwrap_err();
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

        let run = dispatch_executor(
            &entry,
            "cheapo",
            &task(),
            &fixture.project,
            &fixture.mana_home,
        )
        .unwrap();

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
            &entry,
            "cheapo",
            &task,
            &fixture.project,
            &fixture.mana_home,
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
            &entry,
            "cheapo",
            &task,
            &fixture.project,
            &fixture.mana_home,
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
            &entry,
            "cheapo",
            &task,
            &fixture.project,
            &fixture.mana_home,
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
            &entry,
            "cheapo",
            &task,
            &fixture.project,
            &fixture.mana_home,
            &executor.worktree,
            None,
        )
        .unwrap();

        assert!(!run.review_path.exists());
    }
}
