//! `mana mcp-server` -- the four orchestration tools, spoken over MCP on stdio.
//!
//! This is the PM's hands. The PM plans and talks to the user; everything it
//! *does* -- create a task, dispatch it, read the verdict, see what is
//! installed -- goes through the tools below. Design §5 explains why they are
//! tools and not prompt text: the contract is enforced by a schema instead of
//! by a model's willingness to follow prose, and it kills v1's three launch
//! blockers at the root. The PM never sees a path or invents an id, so a
//! prompt/code path mismatch has nowhere to happen; mana is the sole executor,
//! so a "launch" cannot fire twice; and mana registers itself by
//! `current_exe()`, so `$PATH` stops mattering.
//!
//! **SDK choice: `rmcp`.** Both candidates from the plan build a stdio server
//! on tokio, so the tiebreaker is who will still be maintaining it when a
//! protocol revision lands -- the exact risk that made per-CLI Rust modules
//! unacceptable in design §2. `rmcp` is the SDK in the `modelcontextprotocol`
//! org itself (3.1.2, released 2026-08-07, ~10.4M recent downloads,
//! Apache-2.0); `rust-mcp-sdk` is a third-party stack (1.0.1, 2026-07-26,
//! ~102k recent downloads, MIT). Same protocol, one-hundredth of the users and
//! no upstream affiliation: choosing it would be taking on the maintenance
//! risk this project spent a design section avoiding. Features are cut to
//! `server` + `transport-io` + `macros`; the HTTP, SSE, OAuth and client
//! transports are dead weight in a process that speaks to one parent over a
//! pipe.
//!
//! Nothing here holds state between calls. Every tool reads
//! `~/.mana/projects/<project>/` fresh -- registry, logs, reviews, runs -- so
//! this server can be restarted mid-session and the PM cannot tell.

// `pub(crate)` for the same reason `runs` is, one consumer later: `mana doctor`
// (task 4.3) reports which (CLI x model) pairs are resting on a quota cooldown
// right now, and that state is computed here, from the logs, by
// `Observations::gather`. Recomputing it in the doctor would be a second
// implementation of the cooldown rules, free to disagree with the one the
// router actually routes on. Visibility only -- nothing in this module changed.
pub(crate) mod routing;
// `pub(crate)` for the consumer named in its own module doc: the PM driver
// (`cli::launch_pm`) tails `notifications.jsonl` and injects each line into the
// PM session. It reads that file back through `runs::Notification`, so the
// field names stay one contract with one owner instead of two structs free to
// drift apart.
pub(crate) mod runs;

use crate::catalog::{Catalog, CliEntry, CostClass};
use crate::dispatch::{self, DispatchOutcome};
use crate::project::{
    ProjectPaths, ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::review::{self, Attribution, Decision, Verdict};
use crate::task::{Role, Task, TaskFrontmatter, read_task, write_task};
use crate::worktree::WorktreeInfo;
use anyhow::{Context, Result, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::io::stdio;
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use routing::{Observations, Request, Stats};
use runs::{Notification, RunRecord};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The user's catalogue escape valve (design §7). Read from the same place
/// every other command reads it; a missing file is normal.
const CATALOG_OVERRIDE: &str = "catalog.local.toml";

/// Serves MCP on stdin/stdout until the parent closes the pipe.
///
/// `project_root` is passed in rather than taken from the working directory:
/// the PM CLI spawns this process itself, from whatever directory it happens to
/// be in, and the project a task belongs to is not negotiable at that point.
pub fn serve(project_root: &Path) -> Result<()> {
    if !project_root.is_absolute() {
        bail!(
            "--project-root must be an absolute path, got {}",
            project_root.display()
        );
    }
    if !project_root.is_dir() {
        bail!(
            "--project-root {} is not a directory",
            project_root.display()
        );
    }

    let home = mana_home()?;
    let catalog = Catalog::load(Some(&home.join(CATALOG_OVERRIDE)))?;
    let tools = ManaTools::new(project_root.to_path_buf(), home, catalog);

    // One current-thread runtime, built here and nowhere else: this process
    // exists to shuttle small JSON messages, and the work it starts runs on
    // plain threads. Making mana async for this would colour the entire
    // codebase for one subcommand.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime for the MCP server")?;
    runtime.block_on(async move {
        let running = tools
            .serve(stdio())
            .await
            .context("the MCP handshake with the PM failed")?;
        // Returns when the PM closes stdin (EOF) or shuts the session down --
        // the process must not outlive the session that started it.
        //
        // Dispatches still running when that happens are abandoned with it, on
        // purpose: a sub-agent belongs to the session that asked for it, and a
        // PM the user just quit has nobody left to report to. What survives is
        // what always survives -- the worktree, the branch and the registry
        // record; only the run record and the notification are lost, so a
        // relaunched PM sees the task as never dispatched and can say so.
        // Supervising agents across a restart is `mana ps`/`kill` (task 4.2).
        running
            .waiting()
            .await
            .context("the MCP session ended badly")?;
        Ok(())
    })
}

/// The server. Cheap to clone (the catalogue is a handful of parsed structs),
/// which is what the SDK asks for. `in_flight` is the one piece of in-process
/// state, and it is legitimately irrecoverable-from-disk: it exists to honour
/// `[subagent].max_concurrent` (the PM skill promises "mana enforces per-CLI
/// concurrency limits"), and the registry row that would prove a dispatch is
/// live is written only *after* the spawn — by which time a second dispatch
/// against a `max_concurrent = 1` CLI has already killed both (agy, 8s).
#[derive(Clone)]
pub struct ManaTools {
    project_root: PathBuf,
    mana_home: PathBuf,
    catalog: Catalog,
    in_flight: Arc<Mutex<HashMap<String, u32>>>,
    tool_router: ToolRouter<Self>,
}

impl ManaTools {
    pub fn new(project_root: PathBuf, mana_home: PathBuf, catalog: Catalog) -> Self {
        Self {
            project_root,
            mana_home,
            catalog,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }
}

/// Decrements the per-CLI in-flight count when a background dispatch ends,
/// however it ends — a panic in the dispatch path must not permanently eat a
/// concurrency slot, hence a drop guard rather than a call at the end of
/// `run()`.
struct InFlightSlot {
    counts: Arc<Mutex<HashMap<String, u32>>>,
    cli: String,
}

impl Drop for InFlightSlot {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.counts.lock()
            && let Some(count) = counts.get_mut(&self.cli)
        {
            *count = count.saturating_sub(1);
        }
    }
}

// The tool descriptions below are what the PM reads at every turn. They repeat
// the wording of `assets/roles/pm/SKILL.md` deliberately: the skill teaches the
// loop, these teach the call, and a disagreement between them is a PM that
// does the wrong thing for a reason nobody can see.
#[tool_router(router = tool_router)]
impl ManaTools {
    /// Create a task. mana generates the id, writes the file and owns every
    /// path -- never write task files yourself. `prompt` is the brief the
    /// sub-agent will see, and it is the sub-agent's entire world: state the
    /// objective, absolute paths, acceptance criteria a reviewer can check
    /// mechanically, and what is out of scope.
    #[tool]
    async fn create_task(
        &self,
        params: Parameters<CreateTaskParams>,
    ) -> Result<Json<CreateTaskOut>, ErrorData> {
        self.create_task_impl(params.0).map(Json)
    }

    /// Dispatch a task to a sub-agent and return immediately -- the run takes
    /// minutes and finishes in the background. Prefer passing a `cost_class`
    /// and letting mana pick the concrete model: mana knows the live quota
    /// state, you do not. Name an exact `model` only when the task truly needs
    /// one. Use role `executor` to do the work, then `reviewer` on the same
    /// task once the executor has finished.
    #[tool]
    async fn launch_subagent(
        &self,
        params: Parameters<LaunchSubagentParams>,
    ) -> Result<Json<LaunchSubagentOut>, ErrorData> {
        self.launch_subagent_impl(params.0).map(Json)
    }

    /// Read the reviewer's verdict for a task: `validated`, or `rejected` with
    /// an attribution -- `code` means relaunch the executor with the issues
    /// appended to the brief, `brief` means the fault is yours to fix and the
    /// model is not penalised.
    #[tool]
    async fn get_review(&self, params: Parameters<TaskRef>) -> Result<Json<ReviewOut>, ErrorData> {
        self.get_review_impl(params.0).map(Json)
    }

    /// The installed CLIs, their models with an ordinal cost class, and the
    /// observed history of each (CLI x model) pair: dispatches, validations,
    /// rejections blamed on the code, quota failures, and any active cooldown.
    /// Call it before routing decisions -- it is always current, your memory of
    /// it may not be.
    #[tool]
    async fn list_agents(&self) -> Result<Json<ListAgentsOut>, ErrorData> {
        self.list_agents_impl().map(Json)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ManaTools {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        // Without this the SDK reports its own crate name, and the PM's tool
        // namespace would say "rmcp" where the user expects "mana".
        info.server_info = Implementation::new("mana", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "mana's orchestration tools. You are the PM: plan, delegate, and \
             read verdicts -- never write code yourself. Call list_agents \
             before routing; mana owns task ids, file paths and model choice."
                .to_string(),
        );
        info
    }
}

// -- tool implementations ---------------------------------------------------
//
// Split from the `#[tool]` methods above so they can be called directly by
// tests, without standing up a client, a transport and a request context to
// exercise a file write. Every one of them is synchronous: the work is a few
// small file reads, and the one operation that is not -- a dispatch, which
// takes minutes -- is explicitly moved off this thread.
//
// `pub(crate)` for the second caller design §5 gives these tools: on a CLI with
// no MCP surface, `crate::sentinel` parses the PM's fenced block and calls the
// very same methods (task 3.2). Two channels, one implementation -- a sentinel
// executor of its own would be a second validator free to disagree with this
// one about what `launch_subagent` accepts.

impl ManaTools {
    pub(crate) fn create_task_impl(
        &self,
        params: CreateTaskParams,
    ) -> Result<CreateTaskOut, ErrorData> {
        let paths = self.paths()?;
        let depends_on = params.depends_on.unwrap_or_default();
        // Every dependency must already exist, since ids only come into being
        // when `create_task` returns one. Checked now rather than at dispatch:
        // a mistyped id would otherwise sit in the file as a dependency that
        // can never be satisfied, and the PM would be waiting on nothing.
        for dependency in &depends_on {
            let dependency = validated_task_id(dependency)?;
            if !task_path(&paths, dependency).exists() {
                return Err(invalid(format!(
                    "unknown dependency {dependency:?} -- depends_on takes ids \
                     create_task returned"
                )));
            }
        }

        // mana mints the id: design §5 is explicit that the PM never supplies
        // one, which is also what makes every id below safe to put in a path.
        let task_id = uuid::Uuid::new_v4().to_string();
        let task = Task {
            frontmatter: TaskFrontmatter {
                id: task_id.clone(),
                title: params.title,
                // The task's default execution role. A reviewer runs on the
                // same task file, so this records who the brief was written
                // for, not the only role that may ever read it.
                role: Role::Executor,
                depends_on,
            },
            body: params.prompt,
        };
        write_task(&task_path(&paths, &task_id), &task)
            .map_err(|error| internal(&error, "writing the task file"))?;
        Ok(CreateTaskOut { task_id })
    }

    pub(crate) fn launch_subagent_impl(
        &self,
        params: LaunchSubagentParams,
    ) -> Result<LaunchSubagentOut, ErrorData> {
        let paths = self.paths()?;
        let task_id = validated_task_id(&params.task_id)?;
        let role = Role::from(params.role);
        let task = read_task(&task_path(&paths, task_id)).map_err(|error| {
            invalid(format!(
                "unknown task {task_id:?} ({error}) -- create it with create_task first"
            ))
        })?;

        // Checked here and not on the dispatch thread: "the executor has not
        // finished" is the PM's mistake to fix, and it has to come back as a
        // failed tool call rather than as a notification minutes later. It is
        // also checked before CLI resolution on purpose -- a broken request
        // deserves the answer about the request, not about the machine.
        let worktree = match role {
            Role::Executor => None,
            Role::Reviewer => Some(self.reviewable_worktree(&paths, task_id)?),
        };

        let entries = self.catalog.entries();
        let observations = Observations::gather(&paths, entries, chrono::Utc::now())
            .map_err(|error| internal(&error, "reading this project's dispatch history"))?;
        let resolved = routing::resolve(
            entries,
            &installed_ids(entries),
            &observations,
            &Request {
                cli: params.cli.as_deref(),
                model: params.model.as_deref(),
                cost_class: params.cost_class.map(CostClass::from),
            },
        )
        .map_err(|error| invalid(format!("{error:#}")))?;
        let entry = entries
            .iter()
            .find(|entry| entry.cli.id == resolved.cli)
            .expect("the resolver only ever returns a catalogued id")
            .clone();

        // The id the PM is handed is minted here because the dispatcher's own
        // id does not exist until the process is spawned -- which is after this
        // call has to have returned. The registry keeps its own; the two are
        // joined by (task_id, role), and this is the one that appears in
        // `notifications.jsonl` so the PM sees the id it was given.
        let agent_id = uuid::Uuid::new_v4().to_string();

        // Reserve the concurrency slot before spawning the thread: checking
        // after would leave a window where two calls both pass the limit.
        let slot = {
            let mut counts = self
                .in_flight
                .lock()
                .map_err(|_| internal_msg("concurrency state poisoned"))?;
            let count = counts.entry(entry.cli.id.clone()).or_insert(0);
            let max = entry.subagent.max_concurrent;
            if max != 0 && *count >= max {
                return Err(invalid(format!(
                    "{} is at its concurrency limit ({max} in flight). Wait for a \
                     [mana] completion notification, then dispatch again.",
                    entry.cli.id
                )));
            }
            *count += 1;
            InFlightSlot {
                counts: Arc::clone(&self.in_flight),
                cli: entry.cli.id.clone(),
            }
        };

        let background = BackgroundDispatch {
            agent_id: agent_id.clone(),
            entry,
            model: resolved.model.clone(),
            task,
            worktree,
            project_root: self.project_root.clone(),
            mana_home: self.mana_home.clone(),
        };
        std::thread::spawn(move || {
            let _slot = slot; // held for the whole dispatch, released on drop
            background.run()
        });

        Ok(LaunchSubagentOut {
            agent_id,
            resolved: ResolvedOut {
                cli: resolved.cli,
                model: resolved.model,
            },
        })
    }

    pub(crate) fn get_review_impl(&self, params: TaskRef) -> Result<ReviewOut, ErrorData> {
        let paths = self.paths()?;
        let task_id = validated_task_id(&params.task_id)?;
        let path = paths.reviews.join(format!("{task_id}.json"));
        if !path.exists() {
            // Two different mistakes with two different fixes: an id that was
            // never issued, versus a real task nobody has reviewed yet.
            if !task_path(&paths, task_id).exists() {
                return Err(invalid(format!(
                    "unknown task {task_id:?} -- use the id create_task returned"
                )));
            }
            return Err(invalid(format!(
                "no verdict for task {task_id} yet -- launch a reviewer on it, \
                 or wait for the one already running to finish"
            )));
        }
        let verdict = review::read_verdict(&path).map_err(|error| {
            invalid(format!(
                "the verdict for task {task_id} is unusable: {error:#}"
            ))
        })?;
        Ok(ReviewOut::from(&verdict))
    }

    pub(crate) fn list_agents_impl(&self) -> Result<ListAgentsOut, ErrorData> {
        let paths = self.paths()?;
        let entries = self.catalog.entries();
        let observations = Observations::gather(&paths, entries, chrono::Utc::now())
            .map_err(|error| internal(&error, "reading this project's dispatch history"))?;
        let installed = installed_ids(entries);

        Ok(ListAgentsOut {
            agents: entries
                .iter()
                .map(|entry| AgentOut {
                    cli: entry.cli.id.clone(),
                    name: entry.cli.name.clone(),
                    installed: installed.contains(&entry.cli.id),
                    models: entry
                        .models
                        .static_models
                        .iter()
                        .map(|model| ModelOut {
                            id: model.id.clone(),
                            cost_class: routing::class_name(model.cost_class).to_string(),
                            stats: observations.stats(&entry.cli.id, &model.id),
                            cooldown_until: observations
                                .cooldown_until(&entry.cli.id, &model.id)
                                .map(|until| until.to_rfc3339()),
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    /// The worktree a reviewer will run in, or the reason there is none yet.
    fn reviewable_worktree(
        &self,
        paths: &ProjectPaths,
        task_id: &str,
    ) -> Result<WorktreeInfo, ErrorData> {
        let record = runs::read_run(paths, task_id)
            .map_err(|error| internal(&error, "reading the executor's run record"))?
            .ok_or_else(|| {
                invalid(format!(
                    "no executor has finished task {task_id} yet, so there is no \
                     branch to review -- launch an executor first, and wait for \
                     mana to report that it finished"
                ))
            })?;
        if !record.succeeded {
            // Reviewing work that never landed costs a real dispatch and can
            // only report the executor's own failure back.
            return Err(invalid(format!(
                "the executor for task {task_id} did not finish cleanly ({}), so \
                 there is nothing to review -- fix the brief and relaunch the \
                 executor",
                record.outcome
            )));
        }
        Ok(record.worktree_info())
    }

    fn paths(&self) -> Result<ProjectPaths, ErrorData> {
        let paths =
            resolve_project_paths(&self.mana_home, &project_name_from_dir(&self.project_root));
        ensure_project_structure(&paths)
            .map_err(|error| internal(&error, "preparing this project's state directory"))?;
        Ok(paths)
    }
}

/// Everything a dispatch needs, owned, so it can outlive the tool call.
struct BackgroundDispatch {
    agent_id: String,
    entry: CliEntry,
    model: String,
    task: Task,
    /// `Some` for a reviewer -- the executor's checkout, which it reads and
    /// never creates. `None` makes this an executor, which gets a fresh one.
    worktree: Option<WorktreeInfo>,
    project_root: PathBuf,
    mana_home: PathBuf,
}

impl BackgroundDispatch {
    /// Runs to completion on its own thread and reports by writing files. It
    /// cannot return an error to anyone -- the tool call that started it
    /// returned minutes ago -- so every failure ends up in the notification
    /// text instead, where the PM will actually read it.
    fn run(self) {
        let paths =
            resolve_project_paths(&self.mana_home, &project_name_from_dir(&self.project_root));
        let task_id = self.task.frontmatter.id.clone();
        let role = if self.worktree.is_some() {
            Role::Reviewer
        } else {
            Role::Executor
        };

        let outcome = match &self.worktree {
            None => dispatch::dispatch_executor(
                &self.entry,
                &self.model,
                &self.task,
                &self.project_root,
                &self.mana_home,
            )
            .map(|run| {
                let summary = describe(&run.outcome);
                let record = RunRecord {
                    task_id: task_id.clone(),
                    agent_id: self.agent_id.clone(),
                    cli: self.entry.cli.id.clone(),
                    model: self.model.clone(),
                    worktree: run.worktree.path.clone(),
                    branch: run.worktree.branch.clone(),
                    base_ref: run.worktree.base_ref.clone(),
                    succeeded: run.outcome.succeeded(),
                    outcome: summary.clone(),
                    finished_at: crate::log::now_iso8601(),
                };
                match runs::write_run(&paths, &record) {
                    Ok(()) => summary,
                    // A lost run record means the reviewer will be refused, so
                    // it has to be said out loud rather than swallowed.
                    Err(error) => format!("{summary} (mana could not record the run: {error:#})"),
                }
            }),
            Some(worktree) => dispatch::dispatch_reviewer(
                &self.entry,
                &self.model,
                &self.task,
                &self.project_root,
                &self.mana_home,
                worktree,
                None,
            )
            .map(|run| describe(&run.outcome)),
        };

        let outcome = match outcome {
            Ok(summary) => summary,
            Err(error) => format!("never started: {error:#}"),
        };
        // The PM driver (task 2.3) tails this file and injects the line into
        // the PM session as a "[mana] ..." message -- this append is how the PM
        // finds out a dispatch it started minutes ago is done, without polling.
        let _ = runs::notify(
            &paths,
            &Notification {
                ts: crate::log::now_iso8601(),
                task_id,
                role,
                agent_id: self.agent_id,
                outcome,
            },
        );
    }
}

/// One line describing a finished run: what happened, how long it took, and
/// the failure signature if the catalogue recognised one.
fn describe(outcome: &DispatchOutcome) -> String {
    let status = if outcome.timed_out {
        "timed out".to_string()
    } else {
        match outcome.exit_code {
            Some(code) => format!("exit {code}"),
            None => "killed by a signal".to_string(),
        }
    };
    let failure = outcome
        .failure_means
        .map(|means| format!(", {}", dispatch::failure_wire_name(means)))
        .unwrap_or_default();
    format!(
        "{status} in {:.1}s{failure}",
        outcome.duration.as_secs_f64()
    )
}

/// Which catalogued CLIs are actually on this machine. Resolved through `which`
/// on every call rather than cached: a user installing a CLI mid-session must
/// not have to restart the PM to use it.
fn installed_ids(entries: &[CliEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|entry| which::which(entry.cli.bin()).is_ok())
        .map(|entry| entry.cli.id.clone())
        .collect()
}

fn task_path(paths: &ProjectPaths, task_id: &str) -> PathBuf {
    paths.tasks.join(format!("{task_id}.md"))
}

/// Task ids come back from a language model, and this is where they first
/// reach the filesystem. Same rule as `worktree`'s own gate: without it a `..`
/// or a separator would read and write outside the project's state directory.
/// mana mints these as UUIDs, so nothing legitimate is turned away.
fn validated_task_id(task_id: &str) -> Result<&str, ErrorData> {
    let acceptable = !task_id.is_empty()
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if acceptable {
        Ok(task_id)
    } else {
        Err(invalid(format!(
            "invalid task id {task_id:?} -- use the id create_task returned"
        )))
    }
}

/// A failure the PM can fix by calling again with different arguments: an
/// unknown id, a model that does not exist, a reviewer launched too early.
fn invalid(message: String) -> ErrorData {
    ErrorData::invalid_params(message, None)
}

/// A failure the PM can do nothing about -- mana could not read or write its
/// own state. Worded so the user, who sees it in the transcript, knows it is
/// not their prompt that was wrong.
fn internal_msg(message: &str) -> ErrorData {
    ErrorData::internal_error(message.to_string(), None)
}

fn internal(error: &anyhow::Error, doing: &str) -> ErrorData {
    ErrorData::internal_error(format!("mana failed while {doing}: {error:#}"), None)
}

// -- wire types -------------------------------------------------------------
//
// Written out here rather than reusing the internal structs because these are
// the schema the PM is shown: the field names, the enum spellings and the doc
// comments are the contract, and they must not change because a private struct
// was refactored. The catalogue and task types also carry `Deserialize` only,
// or no `JsonSchema` at all, which is the right shape for them and the wrong
// one for a tool boundary.

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CreateTaskParams {
    /// One line naming what this task delivers.
    title: String,
    /// The brief handed to the sub-agent, verbatim. It sees nothing of your
    /// conversation with the user.
    prompt: String,
    /// Ids of tasks that must be validated before this one is dispatched.
    depends_on: Option<Vec<String>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CreateTaskOut {
    task_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LaunchSubagentParams {
    /// The id `create_task` returned.
    task_id: String,
    /// `executor` to do the work, `reviewer` to judge what an executor
    /// already produced for this task.
    role: RoleParam,
    /// Catalogue id of the CLI to run on. Leave it out unless you need a
    /// specific one; `list_agents` names them.
    cli: Option<String>,
    /// Exact model id. Leave it out and pass `cost_class` instead unless the
    /// task truly needs one particular model.
    model: Option<String>,
    /// How hard the task is. mana resolves the cheapest available model of that
    /// class, skipping anything on a quota cooldown. Defaults to `cheap`.
    cost_class: Option<CostClassParam>,
}

/// What the sub-agent is being asked to do.
//
// Doc comments on these wire types are shown to the PM verbatim, so anything a
// maintainer needs to know goes in a `//` comment like this one. `inline` puts
// the allowed words in the tool schema itself rather than behind a `$ref`: the
// reader is a language model, and an enumeration it can see is one it cannot
// hallucinate past. The role is not a task taxonomy -- it decides permissions
// and isolation (design §9).
#[derive(Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum RoleParam {
    /// Writes code, in a git worktree of its own.
    Executor,
    /// Reads the executor's diff and writes a verdict. Never writes code.
    Reviewer,
}

impl From<RoleParam> for Role {
    fn from(role: RoleParam) -> Self {
        match role {
            RoleParam::Executor => Role::Executor,
            RoleParam::Reviewer => Role::Reviewer,
        }
    }
}

/// How much the task is worth spending on.
//
// An ordinal, not a price: mana routes cheapest-first and escalates on
// failure, which needs an order and nothing else (design §7). Inlined into the
// schema for the same reason as `RoleParam`.
#[derive(Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum CostClassParam {
    Cheap,
    Mid,
    Expensive,
}

impl From<CostClassParam> for CostClass {
    fn from(class: CostClassParam) -> Self {
        match class {
            CostClassParam::Cheap => CostClass::Cheap,
            CostClassParam::Mid => CostClass::Mid,
            CostClassParam::Expensive => CostClass::Expensive,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LaunchSubagentOut {
    agent_id: String,
    /// What mana actually picked, which is worth reading back when you asked
    /// for a cost class rather than a model.
    resolved: ResolvedOut,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResolvedOut {
    cli: String,
    model: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskRef {
    /// The id `create_task` returned.
    task_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReviewOut {
    /// `validated` or `rejected`.
    verdict: String,
    /// On a rejection: `code` if the implementation failed a clear brief,
    /// `brief` if the brief itself was at fault.
    #[serde(skip_serializing_if = "Option::is_none")]
    attribution: Option<String>,
    issues: Vec<String>,
    /// Whether this verdict counts against the model's record. A brief you
    /// wrote badly does not.
    counts_against_model: bool,
}

impl From<&Verdict> for ReviewOut {
    fn from(verdict: &Verdict) -> Self {
        ReviewOut {
            verdict: decision_name(verdict.verdict).to_string(),
            attribution: verdict.attribution.map(|a| attribution_name(a).to_string()),
            issues: verdict.issues.clone(),
            counts_against_model: verdict.counts_against_model(),
        }
    }
}

/// The verdict file's own spellings, so the PM reads back exactly what the
/// reviewer wrote. `review::Verdict` is `Serialize`, but re-serializing it
/// whole would also re-export its shape as the tool's output schema, and those
/// are two contracts that only happen to agree today.
fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Validated => "validated",
        Decision::Rejected => "rejected",
    }
}

fn attribution_name(attribution: Attribution) -> &'static str {
    match attribution {
        Attribution::Code => "code",
        Attribution::Brief => "brief",
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListAgentsOut {
    agents: Vec<AgentOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AgentOut {
    cli: String,
    name: String,
    /// Whether the binary is on PATH right now. An uninstalled CLI is listed
    /// so you can see it exists, but nothing can be dispatched to it.
    installed: bool,
    /// Empty when the catalogue leaves this CLI's models to runtime discovery;
    /// such a CLI can only be used by naming both `cli` and `model`.
    models: Vec<ModelOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ModelOut {
    id: String,
    cost_class: String,
    /// Raw counters, never a score: "4 validated of 6" is a small sample you
    /// can discount yourself, a rating from other sessions is not.
    stats: Stats,
    /// Set while a quota failure is still resting this model. Skip it until
    /// then.
    #[serde(skip_serializing_if = "Option::is_none")]
    cooldown_until: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::read_task;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Writes one JSON-RPC frame the way the transport expects it: one line,
    /// flushed, because the reader on the other side blocks on the newline.
    async fn send<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        tools: ManaTools,
        paths: ProjectPaths,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("demo");
            let mana_home = tmp.path().join("mana-home");
            std::fs::create_dir_all(&project).unwrap();
            let paths = resolve_project_paths(&mana_home, "demo");
            Fixture {
                tools: ManaTools::new(
                    project,
                    mana_home,
                    Catalog::embedded().expect("the shipped catalogue must parse"),
                ),
                paths,
                _tmp: tmp,
            }
        }

        fn create(&self, title: &str, prompt: &str, depends_on: Option<Vec<String>>) -> String {
            self.tools
                .create_task_impl(CreateTaskParams {
                    title: title.to_string(),
                    prompt: prompt.to_string(),
                    depends_on,
                })
                .unwrap()
                .task_id
        }
    }

    #[test]
    fn the_tool_surface_is_exactly_the_four_documented_tools() {
        let router = ManaTools::tool_router();
        let mut names: Vec<String> = router
            .list_all()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "create_task",
                "get_review",
                "launch_subagent",
                "list_agents"
            ]
        );
        for tool in router.list_all() {
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "{} has no description for the PM to read",
                tool.name
            );
        }
    }

    /// The input schemas are the half of the contract the PM is actually
    /// bound by -- a renamed field here is a silently broken PM.
    #[test]
    fn the_input_schemas_name_the_documented_parameters() {
        let router = ManaTools::tool_router();
        let schema = |name: &str| {
            router
                .list_all()
                .into_iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("no tool named {name}"))
                .input_schema
                .as_ref()
                .clone()
        };

        let create = schema("create_task");
        let properties = create.get("properties").unwrap();
        for field in ["title", "prompt", "depends_on"] {
            assert!(properties.get(field).is_some(), "create_task lost {field}");
        }
        let required: Vec<&str> = create
            .get("required")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(required, ["title", "prompt"]);

        let launch = schema("launch_subagent");
        let properties = launch.get("properties").unwrap();
        for field in ["task_id", "role", "cli", "model", "cost_class"] {
            assert!(
                properties.get(field).is_some(),
                "launch_subagent lost {field}"
            );
        }
        // The two enums are spelled out inline, so a PM that never resolves a
        // `$ref` still cannot invent a third role or a fourth cost class.
        assert!(launch.get("$defs").is_none(), "{launch:?}");
        let rendered = serde_json::to_string(properties).unwrap();
        for value in ["executor", "reviewer", "cheap", "mid", "expensive"] {
            assert!(rendered.contains(value), "launch_subagent lost {value}");
        }

        assert!(
            schema("get_review")
                .get("properties")
                .unwrap()
                .get("task_id")
                .is_some()
        );
        // No parameters at all: freshness is the whole point of the tool.
        assert_eq!(
            schema("list_agents")
                .get("properties")
                .and_then(|properties| properties.as_object())
                .map(serde_json::Map::len),
            Some(0)
        );
    }

    #[test]
    fn create_task_writes_a_task_file_mana_can_read_back() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Add a marker file", "# Brief\n\nCreate done.txt.\n", None);

        // A v4 UUID, not something the PM could have supplied.
        assert_eq!(task_id.len(), 36, "{task_id}");
        assert!(uuid::Uuid::parse_str(&task_id).is_ok(), "{task_id}");

        let task = read_task(&task_path(&fixture.paths, &task_id)).unwrap();
        assert_eq!(task.frontmatter.id, task_id);
        assert_eq!(task.frontmatter.title, "Add a marker file");
        assert_eq!(task.frontmatter.role, Role::Executor);
        assert!(task.frontmatter.depends_on.is_empty());
        assert_eq!(task.body, "# Brief\n\nCreate done.txt.\n");
    }

    #[test]
    fn create_task_keeps_the_declared_dependencies() {
        let fixture = Fixture::new();
        let first = fixture.create("First", "do a thing", None);
        let second = fixture.create("Second", "do the next thing", Some(vec![first.clone()]));

        let task = read_task(&task_path(&fixture.paths, &second)).unwrap();
        assert_eq!(task.frontmatter.depends_on, vec![first]);
    }

    #[test]
    fn a_dependency_on_a_task_that_does_not_exist_is_refused() {
        let fixture = Fixture::new();
        let error = fixture
            .tools
            .create_task_impl(CreateTaskParams {
                title: "Second".to_string(),
                prompt: "brief".to_string(),
                depends_on: Some(vec!["a0000000-0000-4000-8000-000000000000".to_string()]),
            })
            .unwrap_err();
        assert!(
            error.message.contains("unknown dependency"),
            "{}",
            error.message
        );
        // ...and the same gate covers a dependency id that would escape.
        let error = fixture
            .tools
            .create_task_impl(CreateTaskParams {
                title: "Second".to_string(),
                prompt: "brief".to_string(),
                depends_on: Some(vec!["../../etc/passwd".to_string()]),
            })
            .unwrap_err();
        assert!(
            error.message.contains("invalid task id"),
            "{}",
            error.message
        );
    }

    #[test]
    fn every_task_gets_its_own_id_and_its_own_file() {
        let fixture = Fixture::new();
        let first = fixture.create("First", "brief", None);
        let second = fixture.create("Second", "brief", None);
        assert_ne!(first, second);
        assert!(task_path(&fixture.paths, &first).exists());
        assert!(task_path(&fixture.paths, &second).exists());
    }

    #[test]
    fn a_task_id_that_would_escape_the_project_directory_is_refused() {
        let fixture = Fixture::new();
        for hostile in ["../../etc/passwd", "a/b", "", "id with spaces"] {
            let error = fixture
                .tools
                .get_review_impl(TaskRef {
                    task_id: hostile.to_string(),
                })
                .unwrap_err();
            assert!(
                error.message.contains("invalid task id"),
                "{hostile:?} was not rejected as an id: {}",
                error.message
            );
        }
    }

    #[test]
    fn get_review_before_a_reviewer_has_run_says_so() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Some task", "brief", None);
        let error = fixture
            .tools
            .get_review_impl(TaskRef {
                task_id: task_id.clone(),
            })
            .unwrap_err();
        assert!(error.message.contains("no verdict"), "{}", error.message);
        assert!(error.message.contains(&task_id), "{}", error.message);

        // An id that was never issued is a different mistake with a different
        // fix, and must not read as "the reviewer is still working".
        let error = fixture
            .tools
            .get_review_impl(TaskRef {
                task_id: "a0000000-0000-4000-8000-000000000000".to_string(),
            })
            .unwrap_err();
        assert!(error.message.contains("unknown task"), "{}", error.message);
    }

    #[test]
    fn get_review_returns_the_verdict_and_who_it_counts_against() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Some task", "brief", None);
        std::fs::create_dir_all(&fixture.paths.reviews).unwrap();
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            r#"{"verdict":"rejected","attribution":"code","issues":["src/a.rs:1 criterion 2 unmet"]}"#,
        )
        .unwrap();

        let review = fixture
            .tools
            .get_review_impl(TaskRef {
                task_id: task_id.clone(),
            })
            .unwrap();
        assert_eq!(review.verdict, "rejected");
        assert_eq!(review.attribution.as_deref(), Some("code"));
        assert_eq!(review.issues, ["src/a.rs:1 criterion 2 unmet"]);
        assert!(review.counts_against_model);

        // A brief the PM botched must not read as the model's failure.
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            r#"{"verdict":"rejected","attribution":"brief","issues":["criterion 3 contradicts itself"]}"#,
        )
        .unwrap();
        let review = fixture.tools.get_review_impl(TaskRef { task_id }).unwrap();
        assert!(!review.counts_against_model);
    }

    #[test]
    fn get_review_on_a_malformed_verdict_is_loud_rather_than_empty() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Some task", "brief", None);
        std::fs::create_dir_all(&fixture.paths.reviews).unwrap();
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            "Sure! The work looks good to me.",
        )
        .unwrap();
        let error = fixture
            .tools
            .get_review_impl(TaskRef { task_id })
            .unwrap_err();
        assert!(error.message.contains("unusable"), "{}", error.message);
    }

    #[test]
    fn a_reviewer_launched_before_its_executor_finished_is_refused_immediately() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Some task", "brief", None);
        let error = fixture
            .tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.clone(),
                role: RoleParam::Reviewer,
                cli: None,
                model: None,
                cost_class: None,
            })
            .unwrap_err();
        assert!(
            error.message.contains("no executor has finished"),
            "{}",
            error.message
        );
        assert!(error.message.contains(&task_id), "{}", error.message);
    }

    #[test]
    fn a_reviewer_on_a_failed_executor_is_refused_with_that_failure() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Some task", "brief", None);
        ensure_project_structure(&fixture.paths).unwrap();
        runs::write_run(
            &fixture.paths,
            &RunRecord {
                task_id: task_id.clone(),
                agent_id: "agent-1".to_string(),
                cli: "fixture".to_string(),
                model: "cheapo".to_string(),
                worktree: fixture.paths.root.join("wt"),
                branch: format!("mana/{task_id}"),
                base_ref: "abc123".to_string(),
                succeeded: false,
                outcome: "exit 2 in 3.4s".to_string(),
                finished_at: crate::log::now_iso8601(),
            },
        )
        .unwrap();

        let error = fixture
            .tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id,
                role: RoleParam::Reviewer,
                cli: None,
                model: None,
                cost_class: None,
            })
            .unwrap_err();
        assert!(
            error.message.contains("did not finish cleanly"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("exit 2 in 3.4s"),
            "{}",
            error.message
        );
    }

    #[test]
    fn launching_an_unknown_task_names_the_tool_that_creates_one() {
        let fixture = Fixture::new();
        let error = fixture
            .tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id: "a0000000-0000-4000-8000-000000000000".to_string(),
                role: RoleParam::Executor,
                cli: None,
                model: None,
                cost_class: None,
            })
            .unwrap_err();
        assert!(error.message.contains("unknown task"), "{}", error.message);
        assert!(error.message.contains("create_task"), "{}", error.message);
    }

    #[test]
    fn list_agents_reports_every_catalogued_cli_with_its_models() {
        let fixture = Fixture::new();
        let listed = fixture.tools.list_agents_impl().unwrap();
        let catalog = Catalog::embedded().unwrap();

        assert_eq!(listed.agents.len(), catalog.entries().len());
        for entry in catalog.entries() {
            let agent = listed
                .agents
                .iter()
                .find(|agent| agent.cli == entry.cli.id)
                .unwrap_or_else(|| panic!("{} is missing from list_agents", entry.cli.id));
            assert_eq!(agent.name, entry.cli.name);
            assert_eq!(agent.models.len(), entry.models.static_models.len());
            for model in &agent.models {
                // The three words the PM skill promises, and nothing else.
                assert!(
                    ["cheap", "mid", "expensive"].contains(&model.cost_class.as_str()),
                    "{} has an unroutable cost class {:?}",
                    model.id,
                    model.cost_class
                );
                // A project with no history reads as zeros, not as absent.
                assert_eq!(model.stats, Stats::default());
                assert_eq!(model.cooldown_until, None);
            }
        }
    }

    #[test]
    fn list_agents_serializes_compactly_and_drops_what_is_unset() {
        let fixture = Fixture::new();
        let json = serde_json::to_string(&fixture.tools.list_agents_impl().unwrap()).unwrap();
        assert!(!json.contains("cooldown_until"), "{json}");
        assert!(!json.contains("avg_duration_ms"), "{json}");
        assert!(json.contains("\"dispatched\":0"), "{json}");
    }

    /// The one test that goes through a real transport: the handshake, the
    /// framing and the shutdown are the SDK's job, and this is what proves the
    /// wiring holds. An in-memory duplex rather than a child process, so it
    /// runs identically on every platform and needs no built binary.
    #[test]
    fn the_server_completes_a_handshake_lists_its_tools_and_quits_on_eof() {
        let fixture = Fixture::new();
        let tools = fixture.tools.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async move {
            let (server_side, client_side) = tokio::io::duplex(64 * 1024);
            let (server_read, server_write) = tokio::io::split(server_side);
            let (client_read, mut client_write) = tokio::io::split(client_side);
            let mut client_read = BufReader::new(client_read);

            let server = tokio::spawn(async move {
                tools
                    .serve((server_read, server_write))
                    .await
                    .expect("handshake")
                    .waiting()
                    .await
                    .expect("clean shutdown")
            });

            send(
                &mut client_write,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": rmcp::model::ProtocolVersion::default(),
                        "capabilities": {},
                        "clientInfo": {"name": "mana-test", "version": "0"}
                    }
                }),
            )
            .await;

            let mut line = String::new();
            tokio::time::timeout(
                Duration::from_secs(10),
                client_read.read_line(&mut line),
            )
            .await
            .expect("the server never answered initialize")
            .unwrap();
            let initialized: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(initialized["result"]["serverInfo"]["name"], "mana");
            assert!(initialized["result"]["capabilities"]["tools"].is_object());

            send(
                &mut client_write,
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .await;
            send(
                &mut client_write,
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
            )
            .await;

            line.clear();
            tokio::time::timeout(
                Duration::from_secs(10),
                client_read.read_line(&mut line),
            )
            .await
            .expect("the server never answered tools/list")
            .unwrap();
            let listed: serde_json::Value = serde_json::from_str(&line).unwrap();
            let mut names: Vec<&str> = listed["result"]["tools"]
                .as_array()
                .expect("tools/list must return an array")
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                ["create_task", "get_review", "launch_subagent", "list_agents"]
            );

            // Closing the pipe is how the PM ends a session; the server must
            // not outlive it (v1's zombie-process bug, from the other side).
            drop(client_write);
            drop(client_read);
            tokio::time::timeout(Duration::from_secs(10), server)
                .await
                .expect("the server did not quit when its input closed")
                .unwrap();
        });
    }
}

/// The half of `launch_subagent` that only a real dispatch can prove: that the
/// tool returns while the sub-agent is still running, and that what the thread
/// leaves behind is what the next tool call reads.
///
/// Unix-only because the fake CLIs are shell scripts, same as every other
/// dispatch test in the tree.
#[cfg(all(test, unix))]
mod dispatch_tests {
    use super::*;
    use crate::dispatch::test_fixture::{COMMITTING_EXECUTOR, Fixture as Repo};
    use std::time::{Duration, Instant};

    /// A catalogue entry for a CLI that does not exist, written where mana
    /// reads the user's override from. Going through the real override path
    /// rather than a hand-built `Catalog` keeps the test on the same code the
    /// escape valve uses (design §7).
    fn install_override(repo: &Repo, bin: &str) -> Catalog {
        let source = install_override_source(repo, bin);
        let path = repo.mana_home.join(CATALOG_OVERRIDE);
        std::fs::create_dir_all(&repo.mana_home).unwrap();
        std::fs::write(&path, source).unwrap();
        Catalog::load(Some(&path)).unwrap()
    }

    fn install_override_source(_repo: &Repo, bin: &str) -> String {
        format!(
            r#"
schema = 1
notes = "mcp dispatch fixture"

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
args = []
prompt = "argv"
max_concurrent = 0
cwd_required_in_brief = false

[models]
discovery_args = []

[[models.static]]
id = "cheapo"
cost_class = "cheap"
pool = "main"

[[quota.pools]]
id = "main"
kind = "requests"
period = "monthly"
pool_scope = "global"

[skills]
dirs = []

[install]
url = "https://example.invalid/fixture"
"#
        )
    }

    fn tools_for(repo: &Repo, bin: &str) -> ManaTools {
        ManaTools::new(
            repo.project.clone(),
            repo.mana_home.clone(),
            install_override(repo, bin),
        )
    }

    /// Blocks until the background thread has appended `count` notifications,
    /// or gives up. A poll rather than a channel because this is exactly what
    /// the PM driver (task 2.3) will do with the same file.
    fn wait_for_notifications(paths: &ProjectPaths, count: usize) -> Vec<Notification> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let lines: Vec<Notification> = std::fs::read_to_string(runs::notifications_path(paths))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            if lines.len() >= count {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "only {} of {count} dispatches reported in",
                lines.len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn max_concurrent_is_enforced_and_the_slot_frees_on_completion() {
        let repo = Repo::new();
        // A deliberately slow executor so the first dispatch is still running
        // when the second one asks for the only slot.
        let slow = repo.script("slow-cli", "#!/bin/sh\nsleep 2\nexit 0\n");
        let catalog = {
            let source = install_override_source(&repo, &slow)
                .replace("max_concurrent = 0", "max_concurrent = 1");
            let path = repo.mana_home.join(CATALOG_OVERRIDE);
            std::fs::create_dir_all(&repo.mana_home).unwrap();
            std::fs::write(&path, source).unwrap();
            Catalog::load(Some(&path)).unwrap()
        };
        let tools = ManaTools::new(repo.project.clone(), repo.mana_home.clone(), catalog);
        let paths = repo.paths();

        let first = tools
            .create_task_impl(CreateTaskParams {
                title: "one".to_string(),
                prompt: "# a\n".to_string(),
                depends_on: None,
            })
            .unwrap()
            .task_id;
        let second = tools
            .create_task_impl(CreateTaskParams {
                title: "two".to_string(),
                prompt: "# b\n".to_string(),
                depends_on: None,
            })
            .unwrap()
            .task_id;

        let launch = |task_id: &str| {
            tools.launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.to_string(),
                role: RoleParam::Executor,
                cli: Some("fixture".to_string()),
                model: None,
                cost_class: None,
            })
        };

        launch(&first).unwrap();
        let refused = launch(&second).unwrap_err();
        assert!(
            refused.message.contains("concurrency limit"),
            "{}",
            refused.message
        );
        assert!(refused.message.contains("fixture"), "{}", refused.message);

        // Once the slow dispatch reports in, its drop guard has freed the
        // slot and the same launch goes through.
        wait_for_notifications(&paths, 1);
        launch(&second).unwrap();
        wait_for_notifications(&paths, 2);
    }

    #[test]
    fn a_launch_returns_at_once_and_the_dispatch_reports_in_afterwards() {
        let repo = Repo::new();
        let executor = repo.script("exec-cli", COMMITTING_EXECUTOR);
        let tools = tools_for(&repo, &executor);
        let paths = repo.paths();

        let task_id = tools
            .create_task_impl(CreateTaskParams {
                title: "Add a marker file".to_string(),
                prompt: "# Brief\n\nCreate done.txt and commit it.\n".to_string(),
                depends_on: None,
            })
            .unwrap()
            .task_id;

        // No model named: the cost class defaults to cheap and the resolver
        // picks the only cheap model the fixture CLI declares.
        let launched = tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.clone(),
                role: RoleParam::Executor,
                cli: Some("fixture".to_string()),
                model: None,
                cost_class: None,
            })
            .unwrap();
        assert_eq!(launched.resolved.cli, "fixture");
        assert_eq!(launched.resolved.model, "cheapo");

        let notifications = wait_for_notifications(&paths, 1);
        assert_eq!(notifications[0].task_id, task_id);
        assert_eq!(notifications[0].role, Role::Executor);
        // The id the PM was handed is the id it sees come back.
        assert_eq!(notifications[0].agent_id, launched.agent_id);
        assert!(
            notifications[0].outcome.starts_with("exit 0 in"),
            "{}",
            notifications[0].outcome
        );

        // The handoff the reviewer will read, written by the executor thread.
        let run = runs::read_run(&paths, &task_id).unwrap().unwrap();
        assert!(run.succeeded);
        assert_eq!(run.branch, format!("mana/{task_id}"));
        assert_eq!(run.cli, "fixture");
        assert_eq!(run.model, "cheapo");
        assert!(run.worktree.join("done.txt").exists());
        // The agent worked in its own worktree, not in the project checkout.
        assert!(!repo.project.join("done.txt").exists());

        // ...and the dispatch is in the registry the router reads.
        let registry = crate::lock::load_registry(&paths.subagents_file).unwrap();
        assert_eq!(registry.records.len(), 1);
        assert_eq!(registry.records[0].task_id, task_id);
        assert_eq!(registry.records[0].model, "cheapo");
    }

    #[test]
    fn a_reviewer_runs_on_the_executors_branch_and_its_verdict_reaches_get_review() {
        let repo = Repo::new();
        let tools = tools_for(&repo, &repo.script("exec-cli", COMMITTING_EXECUTOR));
        let paths = repo.paths();

        let task_id = tools
            .create_task_impl(CreateTaskParams {
                title: "Add a marker file".to_string(),
                prompt: "# Brief\n\nCreate done.txt and commit it.\n".to_string(),
                depends_on: None,
            })
            .unwrap()
            .task_id;
        tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.clone(),
                role: RoleParam::Executor,
                cli: Some("fixture".to_string()),
                model: None,
                cost_class: None,
            })
            .unwrap();
        wait_for_notifications(&paths, 1);

        // A second server over the same project, with the reviewer's script
        // now that the task id (and so the verdict path) is known. Restarting
        // is the point: nothing about the session lived in the first one.
        let review_path = paths.reviews.join(format!("{task_id}.json"));
        let reviewer = repo.script(
            "review-cli",
            &format!(
                "printf '%s' \"$1\" > review-prompt.txt\nprintf '%s' '{{\"verdict\":\"validated\",\"issues\":[]}}' > '{}'\n",
                review_path.display()
            ),
        );
        let tools = tools_for(&repo, &reviewer);
        let launched = tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.clone(),
                role: RoleParam::Reviewer,
                cli: Some("fixture".to_string()),
                model: None,
                cost_class: None,
            })
            .unwrap();

        let notifications = wait_for_notifications(&paths, 2);
        assert_eq!(notifications[1].role, Role::Reviewer);
        assert_eq!(notifications[1].agent_id, launched.agent_id);

        let review = tools
            .get_review_impl(TaskRef {
                task_id: task_id.clone(),
            })
            .unwrap();
        assert_eq!(review.verdict, "validated");
        assert!(!review.counts_against_model);

        // The reviewer ran in the executor's worktree, on its diff -- proven
        // by where the prompt it echoed landed, and by what it says.
        let run = runs::read_run(&paths, &task_id).unwrap().unwrap();
        let prompt = std::fs::read_to_string(run.worktree.join("review-prompt.txt")).unwrap();
        assert!(prompt.starts_with("You are a reviewer"), "{prompt}");
        assert!(prompt.contains(&run.base_ref), "{prompt}");
        assert!(
            prompt.contains("Create done.txt and commit it."),
            "{prompt}"
        );

        // The verdict now counts for the model that earned it.
        let observations =
            Observations::gather(&paths, tools.catalog.entries(), chrono::Utc::now()).unwrap();
        assert_eq!(observations.stats("fixture", "cheapo").validated, 1);
    }

    #[test]
    fn a_dispatch_that_never_starts_still_reports_in() {
        let repo = Repo::new();
        // A catalogue entry pointing at a binary that is not there: the spawn
        // fails inside the thread, long after the tool call returned, so the
        // notification is the only place the PM can learn about it.
        let tools = tools_for(&repo, &repo.bin_dir.join("absent-cli").to_string_lossy());
        let paths = repo.paths();

        let task_id = tools
            .create_task_impl(CreateTaskParams {
                title: "Doomed".to_string(),
                prompt: "brief".to_string(),
                depends_on: None,
            })
            .unwrap()
            .task_id;
        // `which` cannot see it either, so the resolver refuses first -- which
        // is the better failure. Force the dispatch past that gate the way a
        // CLI uninstalled mid-session would.
        let entry = tools.catalog.get("fixture").unwrap().clone();
        BackgroundDispatch {
            agent_id: "agent-1".to_string(),
            entry,
            model: "cheapo".to_string(),
            task: read_task(&task_path(&paths, &task_id)).unwrap(),
            worktree: None,
            project_root: repo.project.clone(),
            mana_home: repo.mana_home.clone(),
        }
        .run();

        let notifications = wait_for_notifications(&paths, 1);
        assert!(
            notifications[0].outcome.contains("never started"),
            "{}",
            notifications[0].outcome
        );
        // Nothing to review, and the reviewer is told so rather than paid for.
        assert!(runs::read_run(&paths, &task_id).unwrap().is_none());
    }
}
