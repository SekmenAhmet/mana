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

use crate::catalog::{Catalog, CliEntry, CostClass, FailureMeans};
use crate::dispatch::{self, DispatchOutcome};
use crate::project::{
    ProjectPaths, ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::review::{self, Decision, Verdict};
use crate::task::{Role, Task, TaskFrontmatter, read_task, write_task};
use crate::worktree::{self, WorktreeInfo};
use anyhow::{Context, Result, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolResponse, CallToolResult, ContentBlock, ErrorCode, ErrorData, Implementation,
    ServerCapabilities, ServerInfo,
};
use rmcp::transport::io::stdio;
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use routing::{Observations, Request, Stats};
use runs::{Notification, RunRecord};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

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
    let catalog = Catalog::load(Some(&home.join(crate::catalog::CATALOG_OVERRIDE)))?;
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
/// `[subagent].max_concurrent` (the PM skill tells the PM an over-limit
/// dispatch is refused and to re-send later), and the registry row that would
/// prove a dispatch is live is written only *after* the spawn — by which time
/// a second dispatch against a `max_concurrent = 1` CLI has already killed
/// both (agy, 8s).
///
/// Counting in one process's memory is only the project's true count because
/// a project has one session: `mana launch` takes `session_lock` and a second
/// launch on the same project is refused. Before that lock, two sessions meant
/// two servers, two counters and two dispatches against a cap of one (#35).
/// Anything that ever lets two servers serve one project has to move this
/// count onto disk with them.
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
            tool_router: Self::portable_tool_router(),
        }
    }

    /// The generated router, with every schema made portable before a client
    /// ever sees it.
    ///
    /// `#[tool_router]` builds the schemas through schemars, which stamps
    /// Rust's integer widths on as `"format": "uint32"` / `"uint64"`. Those are
    /// OpenAPI formats; JSON Schema defines neither, so a client that checks
    /// what it was handed says so -- opencode logs `unknown format "uint64"
    /// ignored in schema at path "#/$defs/Stats/properties/avg_duration_ms"`
    /// for every one of them, twice each, and the operator reads mana's own
    /// stream as noise it did not ask for.
    ///
    /// Fixed here rather than field by field (`#[schemars(with = ...)]` on
    /// each counter) for two reasons: this covers the tools mana has not
    /// written yet, and it needs no annotation on `routing::Stats`, which is an
    /// internal type that happens to be serialised rather than a wire type
    /// anybody should be decorating for a client's benefit. Nothing is lost:
    /// `format` is an annotation, and the constraint that actually binds --
    /// `"type": "integer"`, `"minimum": 0` -- is emitted next to it and stays.
    fn portable_tool_router() -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            route.attr.input_schema = portable_schema(&route.attr.input_schema);
            route.attr.output_schema = route.attr.output_schema.as_ref().map(portable_schema);
        }
        router
    }
}

/// The formats JSON Schema itself defines (2020-12 §7.3). Anything else is a
/// vendor extension, and a client is right to say it does not know it.
const DEFINED_FORMATS: [&str; 19] = [
    "date-time",
    "date",
    "time",
    "duration",
    "email",
    "idn-email",
    "hostname",
    "idn-hostname",
    "ipv4",
    "ipv6",
    "uri",
    "uri-reference",
    "iri",
    "iri-reference",
    "uuid",
    "uri-template",
    "json-pointer",
    "relative-json-pointer",
    "regex",
];

fn portable_schema(schema: &Arc<rmcp::model::JsonObject>) -> Arc<rmcp::model::JsonObject> {
    let mut document = serde_json::Value::Object(schema.as_ref().clone());
    drop_undefined_formats(&mut document);
    match document {
        serde_json::Value::Object(object) => Arc::new(object),
        // Built from an object one line above.
        _ => unreachable!("a JSON object stayed an object"),
    }
}

/// Removes every `format` annotation JSON Schema does not define, at any depth.
///
/// Keyed on the pair (`"format"`, a string value): in a schema, `format` always
/// carries a string, and a *property* called `format` carries a schema object
/// instead -- so a tool that takes a `format` parameter keeps it.
fn drop_undefined_formats(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::String(format)) = object.get("format")
                && !DEFINED_FORMATS.contains(&format.as_str())
            {
                object.remove("format");
            }
            for nested in object.values_mut() {
                drop_undefined_formats(nested);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(drop_undefined_formats),
        _ => {}
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
        // Poisoning is ignored here and at the one other lock site, deliberately.
        // A thread that panics while holding this mutex poisons it for the life
        // of the process, so honouring the `Err` would defeat exactly the case
        // this guard was written for: the slot would never be given back, every
        // later `launch_subagent` on that CLI would be refused with "wait for a
        // [mana] completion notification", and no such notification can ever
        // arrive because there is no dispatch behind the phantom slot. Nothing
        // is being protected by refusing: the map holds counters, and no panic
        // can leave one half-written.
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&self.cli) {
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
    ) -> Result<Json<CreateTaskOut>, Refusal> {
        self.create_task_impl(params.0)
            .map(Json)
            .map_err(Refusal::of)
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
    ) -> Result<Json<LaunchSubagentOut>, Refusal> {
        self.launch_subagent_impl(params.0)
            .map(Json)
            .map_err(Refusal::of)
    }

    /// Read the reviewer's verdict for a task: `validated`, or `rejected` with
    /// an attribution -- `code` means relaunch the executor with the issues
    /// appended to the brief, `brief` means the fault is yours to fix and the
    /// model is not penalised.
    #[tool]
    async fn get_review(&self, params: Parameters<TaskRef>) -> Result<Json<ReviewOut>, Refusal> {
        self.get_review_impl(params.0)
            .map(Json)
            .map_err(Refusal::of)
    }

    /// The installed CLIs, their models with an ordinal cost class, and the
    /// observed history of each (CLI x model) pair: dispatches, validations,
    /// rejections blamed on the code, quota failures, and any active cooldown.
    /// Call it before routing decisions -- it is always current, your memory of
    /// it may not be.
    #[tool]
    async fn list_agents(&self) -> Result<Json<ListAgentsOut>, Refusal> {
        self.list_agents_impl().map(Json).map_err(Refusal::of)
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
        // The caller is a language model, so this is where a brief first has
        // to be real (#45). An empty one is accepted by the schema, dispatches
        // like any other, and spends a full paid run on a sub-agent inventing
        // what it was for -- and the empty *title* is what the PM, `mana ps`
        // and the user identify that run by afterwards. Named field by field
        // because the fix differs: a title is one line, a prompt is the whole
        // brief, and "invalid arguments" would leave the PM guessing which.
        for (field, value) in [("title", &params.title), ("prompt", &params.prompt)] {
            if value.trim().is_empty() {
                return Err(invalid(format!(
                    "create_task was given an empty {field}. Send the call again with a real \
                     one: `title` is the single line naming what the task delivers, `prompt` \
                     is the brief the sub-agent works from and it sees nothing else."
                )));
            }
        }

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
        let path = task_path(&paths, task_id);
        let task = read_task(&path).map_err(|error| {
            // Two remedies, ordered by who can act on them. The PM's role
            // forbids editing or deleting task files, so it gets create_task
            // first -- but a replacement task gets a *new* id, so any task
            // whose depends_on names the broken one has to be recreated
            // against the new id, or it waits forever on a dependency gate
            // that can never open. Repairing the file in place is a human
            // operator's job, not the PM's; the PM can only pass along the
            // path and the parser's own complaint. `{error:#}` rather than
            // `{error}` because the parser's own "line 3 column 8" is the
            // only thing that says where to look.
            if crate::task::read_failure_is_missing(&error) {
                invalid(format!(
                    "unknown task {task_id:?} -- create it with create_task first"
                ))
            } else {
                let path = path.display();
                invalid(format!(
                    "task {task_id} exists at {path} but mana cannot read it -- create a \
                     replacement task with create_task and dispatch that instead; the \
                     replacement gets a new id, so any task whose depends_on names {task_id} \
                     must be recreated against the new id, or it will wait forever on a \
                     dependency gate that can never open. Repairing the file at {path} in place \
                     is a job for the human operator, not the PM: it needs a '+++'-fenced TOML \
                     block with id, title and role, then the brief; mana's parser says: \
                     {error:#}"
                ))
            }
        })?;

        // `depends_on` was recorded and never enforced (#40), while the skill
        // told the PM a `validated` verdict "unblocks its dependents" -- a
        // promise mana did not keep, so the PM planned around ordering it
        // believed was held for it and dispatched out of order. The executor
        // then works in a worktree branched off a base its dependency never
        // landed in, which is a full paid run against a tree missing the thing
        // it was told to build on.
        //
        // A refusal, not a scheduler: nothing is queued and nothing is woken
        // (that is v3). The verdicts are already on disk, `get_review` already
        // reads them, and declining the one dispatch that cannot succeed is
        // the whole of it. Executors only -- a reviewer judges work that
        // already exists, and refusing it would strand a dispatch that has
        // been paid for with no verdict anyone can read.
        if role == Role::Executor {
            self.unmet_dependency(&paths, &task)?;
        }

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

        // The id the PM is handed is minted here because the tool call has to
        // return one before the process it names exists. The dispatcher is then
        // given *this* id rather than minting a second one of its own: it is
        // what the registry record carries, what names the log file, and what
        // comes back in `notifications.jsonl` -- so the string the PM repeats to
        // a human is the string `mana ps` and `mana kill` accept.
        let agent_id = uuid::Uuid::new_v4().to_string();

        // Reserve the concurrency slot before spawning the thread: checking
        // after would leave a window where two calls both pass the limit.
        // Poisoning is discarded for the reason given on `InFlightSlot::drop`.
        let slot = {
            let mut counts = self
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
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
            branch: worktree::branch_name(task_id),
            worktree: self.worktree_path(task_id),
        })
    }

    pub(crate) fn get_review_impl(&self, params: TaskRef) -> Result<ReviewOut, ErrorData> {
        let paths = self.paths()?;
        let task_id = validated_task_id(&params.task_id)?;
        let path = review_path(&paths, task_id);
        if !path.exists() {
            // Two different mistakes with two different fixes: an id that was
            // never issued, versus a real task nobody has reviewed yet.
            if !task_path(&paths, task_id).exists() {
                return Err(invalid(format!(
                    "unknown task {task_id:?} -- use the id create_task returned"
                )));
            }
            // Three states, not two. "Launch a reviewer" is right for a task no
            // reviewer has touched, and wrong -- expensively -- for one where a
            // reviewer already ran and wrote nothing: that dispatch is paid for,
            // and `dispatch_reviewer` deletes any verdict it finds before it
            // starts, so an unthinking relaunch can also destroy a good verdict
            // from an earlier attempt before spending the money.
            return Err(invalid(match last_reviewer_outcome(&paths, task_id) {
                Some(outcome) => format!(
                    "a reviewer has already run on task {task_id} and left no verdict at \
                     {} ({outcome}). It has been paid for; relaunching the same one is \
                     likely to end the same way -- send the retry to a different model, \
                     or read the executor's diff and decide yourself",
                    path.display()
                ),
                None => format!(
                    "no verdict for task {task_id} yet -- launch a reviewer on it, \
                     or wait for the one already running to finish"
                ),
            }));
        }
        let verdict = review::read_verdict(&path).map_err(|error| {
            invalid(format!(
                "the verdict for task {task_id} is unusable: {error:#}"
            ))
        })?;
        Ok(ReviewOut::about(
            &verdict,
            worktree::branch_name(task_id),
            self.worktree_path(task_id),
        ))
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
                            cost_class: model.cost_class.word().to_string(),
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

    /// `Ok(())` when every task this one declares a dependency on has been
    /// validated, and the refusal naming the first one that has not otherwise.
    ///
    /// The message has to leave the PM with a move, not just a "no": which
    /// dependency, what state it is actually in, and what would satisfy it --
    /// otherwise the PM's next act is to call again, identically, because
    /// nothing told it what changed the answer.
    fn unmet_dependency(&self, paths: &ProjectPaths, task: &Task) -> Result<(), ErrorData> {
        for dependency in &task.frontmatter.depends_on {
            // Validated on the way back out of the file as well as on the way
            // in: `create_task` wrote it, but a task file is plain text on
            // disk and this id is about to become a path.
            let dependency = validated_task_id(dependency)?;
            let state_and_move = match review::read_verdict(&review_path(paths, dependency)) {
                Ok(verdict) if verdict.verdict == Decision::Validated => continue,
                Ok(verdict) if verdict.verdict == Decision::Rejected => (
                    format!("its verdict is {}", verdict.verdict.word()),
                    " Relaunching the same task_id preserves this dependency, because this \
                         task still names that id and it can still reach validated. Creating a \
                         replacement task mints a new id, which orphans this task."
                        .to_string(),
                ),
                // A malformed verdict needs the task finished, no special guidance.
                Ok(verdict) => (
                    format!("its verdict is {}", verdict.verdict.word()),
                    String::new(),
                ),
                Err(_) => ("it has no usable verdict yet".to_string(), String::new()),
            };
            return Err(invalid(format!(
                "task {} depends on task {dependency}, and {}. Finish that one first -- \
                 executor, then reviewer, then get_review -- and launch this one when it comes \
                 back validated.{} Nothing is queued in the meantime: send this call again then.",
                task.frontmatter.id,
                state_and_move.0,
                if state_and_move.1.is_empty() {
                    String::new()
                } else {
                    format!(" {}", state_and_move.1)
                }
            )));
        }
        Ok(())
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
            // only report the executor's own failure back. What to do instead
            // is not decided here: `record.outcome` was written by `describe`,
            // which knew the cause, and repeating a fixed prescription on top
            // of it would contradict it every time the cause was not the brief.
            return Err(invalid(format!(
                "the executor for task {task_id} did not finish cleanly ({}), so \
                 there is nothing to review",
                record.outcome
            )));
        }
        Ok(record.worktree_info())
    }

    /// Where a task's worktree is, or will be. Named here rather than at the
    /// two call sites so the string mana promises at dispatch and the one it
    /// repeats at review are the same string `worktree::create` builds.
    fn worktree_path(&self, task_id: &str) -> String {
        worktree::worktree_path(&self.mana_home, &self.project_root, task_id)
            .display()
            .to_string()
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

        // The id the PM was handed travels down with the assignment, so the
        // registry record, the log file, the notification and the run record
        // all name the same dispatch.
        let assignment = dispatch::Assignment {
            agent_id: &self.agent_id,
            entry: &self.entry,
            model: &self.model,
            task: &self.task,
            project_root: &self.project_root,
            mana_home: &self.mana_home,
        };

        let outcome = match &self.worktree {
            None => dispatch::dispatch_executor(&assignment).map(|run| {
                let summary = describe(&run.outcome);
                let tail = failure_tail(&run.outcome);
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
                    output_tail: tail.clone(),
                    finished_at: crate::log::now_iso8601(),
                };
                let summary = with_tail(summary, tail);
                match runs::write_run(&paths, &record) {
                    Ok(()) => summary,
                    // A lost run record means the reviewer will be refused, so
                    // it has to be said out loud rather than swallowed.
                    Err(error) => format!("{summary} (mana could not record the run: {error:#})"),
                }
            }),
            Some(worktree) => dispatch::dispatch_reviewer(&assignment, worktree, None).map(|run| {
                // A reviewer writes no run record, so the notification is the
                // only place its own output can survive at all (#32).
                let summary = with_tail(describe(&run.outcome), failure_tail(&run.outcome));
                // A reviewer that exits 0 without writing its verdict is the
                // failure `review.rs` names as the likeliest one there is, and
                // reporting only the exit code hides it completely: the PM
                // reads "exit 0", calls `get_review`, is told nothing has been
                // reviewed, and buys a second identical dispatch. The verdict
                // is the deliverable, so it is what the notification reports.
                match review::read_verdict(&run.review_path) {
                    Ok(_) => summary,
                    Err(error) => format!(
                        "{summary} -- but it left no usable verdict ({error:#}), and that \
                         dispatch is already paid for. Relaunching the same reviewer will \
                         most likely repeat it: send the retry to a different model, or \
                         read the diff yourself"
                    ),
                }
            }),
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

/// What the PM is told a finished dispatch did, and what to do about it.
///
/// The single place an outcome becomes words the PM acts on: it is the
/// notification line, and it is also `RunRecord::outcome`, which is what
/// `reviewable_worktree` quotes back when a reviewer is refused. The PM spends
/// a real turn -- and a real dispatch -- on whatever this sentence tells it, so
/// the fact comes from `dispatch::describe` and the advice from `next_step`,
/// which reads the cause instead of assuming one.
fn describe(outcome: &DispatchOutcome) -> String {
    match next_step(outcome) {
        Some(step) => format!("{} -- {step}", dispatch::describe(outcome)),
        None => dispatch::describe(outcome),
    }
}

/// How many lines of a failed run's output are worth keeping, and the hard
/// byte ceiling under them.
///
/// A cap rather than the whole streams, deliberately: a sub-agent gets fifteen
/// minutes and can print megabytes in them, this text is re-read from the run
/// record on every reviewer dispatch, and the notification carrying it is
/// injected straight into the PM's context, where every line is paid for on
/// each subsequent turn. What is worth that price is the end -- a CLI's last
/// words are what it died of ("model 'x' does not exist", "not logged in") --
/// so the tail is kept and the run itself is not. Both bounds are needed: the
/// line count is the useful unit, and the byte ceiling is what holds when a
/// CLI prints one megabyte-long JSON line, which is exactly the shape of
/// output these CLIs produce.
const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 2000;

/// The end of what a failed dispatch printed, or `None` when it succeeded or
/// said nothing.
///
/// This closes the gap the failure signatures made obvious (#32): the
/// catalogue matches its regexes against exactly these two strings and then
/// they are dropped with the `DispatchOutcome`, leaving `{succeeded: false,
/// outcome: "exit 1 in 3.5s"}` as the entire record of the run. A PM handed
/// that diagnosed "systematic issue (likely worktree setup or CLI config)"
/// while the real cause -- a model id that did not exist -- was one line the
/// CLI had already printed.
///
/// Both streams, each labelled: a CLI puts its refusal on whichever one it
/// feels like, and the catalogue's own signatures match copilot's quota
/// refusal on *stdout*. Only on failure, because a successful run's output is
/// the work, not evidence.
fn failure_tail(outcome: &DispatchOutcome) -> Option<String> {
    if outcome.succeeded() {
        return None;
    }
    let tail: Vec<String> = [("stderr", &outcome.stderr), ("stdout", &outcome.stdout)]
        .into_iter()
        .filter_map(|(stream, text)| Some(format!("{stream}: {}", tail_of(text)?)))
        .collect();
    (!tail.is_empty()).then(|| tail.join("\n"))
}

/// The last `TAIL_LINES` lines of `text`, then its last `TAIL_BYTES` bytes.
fn tail_of(text: &str) -> Option<String> {
    let text = text.trim_end();
    if text.is_empty() {
        return None;
    }
    let mut lines: Vec<&str> = text.lines().rev().take(TAIL_LINES).collect();
    lines.reverse();
    let kept = lines.join("\n");
    if kept.len() <= TAIL_BYTES {
        return Some(kept);
    }
    // Forward to the next char boundary rather than slicing at the byte
    // offset: this is a model's output, and one multi-byte character
    // straddling the cut would panic the dispatch thread over a log line.
    let cut = (kept.len() - TAIL_BYTES..kept.len())
        .find(|index| kept.is_char_boundary(*index))
        .unwrap_or(kept.len());
    Some(format!("[...] {}", &kept[cut..]))
}

/// Puts the failure tail under the one-line summary, where the PM reads both.
///
/// The summary is `describe`'s sentence -- mana's own voice, stated once and
/// left readable as such -- so only the tail, the sub-agent's own stdout and
/// stderr, goes through `wrap_agent_text` (#179).
pub(crate) fn with_tail(summary: String, tail: Option<String>) -> String {
    match tail {
        Some(tail) => format!("{summary}\nlast output:\n{}", wrap_agent_text(&tail)),
        None => summary,
    }
}

/// The delimiter mana wraps around every span of sub-agent-written text
/// before it is injected into a PM turn: the failure tail under a dispatch
/// notification (`with_tail`) and the reviewer's issues `get_review` returns
/// (`bounded_issues`).
///
/// `[mana]` is the sentinel protocol's authenticated-voice marker (see
/// `sentinel::reply`) -- everything mana says to the PM opens with it, and
/// the PM's skill tells it so. A sub-agent's own stdout, stderr or review
/// prose is copied verbatim into the PM's context, so a line it happens to
/// print starting with `[mana]` would otherwise be indistinguishable from
/// mana speaking (#179). Text between this pair is a quote, not mana: mana
/// itself never emits `AGENT_TEXT_OPEN`/`AGENT_TEXT_CLOSE` anywhere else, so
/// their appearance is itself the guarantee.
pub(crate) const AGENT_TEXT_OPEN: &str = "<<<agent-text>>>";
pub(crate) const AGENT_TEXT_CLOSE: &str = "<<<end-agent-text>>>";

/// Wraps `text`, a sub-agent's own output, in `AGENT_TEXT_OPEN` /
/// `AGENT_TEXT_CLOSE` so the PM can tell it apart from mana's own words.
///
/// Neutralises the delimiter first: a literal `AGENT_TEXT_OPEN` or
/// `AGENT_TEXT_CLOSE` already inside `text` gets a space spliced in before
/// its closing `>>>`. That breaks the exact match without deleting any of
/// the agent's characters, so the delimiter cannot be forged from inside the
/// span it encloses -- the only exact occurrences left in the result are the
/// pair this function adds, one of each.
///
/// This adds `AGENT_TEXT_OPEN.len() + AGENT_TEXT_CLOSE.len() + 2` bytes on
/// top of whatever `TAIL_BYTES`/`bounded_issues` already bounded `text` to;
/// the bounds themselves are unchanged; this is bytes of delimiter, not of
/// agent text.
pub(crate) fn wrap_agent_text(text: &str) -> String {
    let neutralised = text
        .replace(AGENT_TEXT_OPEN, "<<<agent-text >>>")
        .replace(AGENT_TEXT_CLOSE, "<<<end-agent-text >>>");
    format!("{AGENT_TEXT_OPEN}\n{neutralised}\n{AGENT_TEXT_CLOSE}")
}

/// The remedy that fits the cause, or `None` when the run needs no remedy.
///
/// "Fix the brief and relaunch the executor" used to be the answer to every
/// failure, including the two it cannot possibly help with: no edit to a brief
/// refills an exhausted quota pool, and none of it fits fifteen minutes of work
/// into a budget the last attempt already blew. Following that advice costs a
/// full paid dispatch into the same wall, and the PM will follow it -- so each
/// cause states its own next move, and quota failures name where the live
/// cooldown state can actually be read.
fn next_step(outcome: &DispatchOutcome) -> Option<&'static str> {
    if outcome.succeeded() {
        return None;
    }
    if outcome.killed {
        // Not a model failure at all, and nothing here counts against the pair.
        return Some(
            "an operator stopped this dispatch on purpose; nothing is wrong with the CLI \
             or the brief. Relaunch only if the work is still wanted",
        );
    }
    match (outcome.failure_means, outcome.timed_out) {
        (Some(FailureMeans::QuotaExhausted | FailureMeans::RateLimited), _) => Some(
            "the quota pool is out, not the brief. mana is resting this (CLI x model) \
             pair: call list_agents and route the retry to a model with no cooldown \
             (`mana doctor` lists them) rather than re-sending the same one",
        ),
        (Some(FailureMeans::AuthExpired), _) => Some(
            "this CLI is not logged in, so no relaunch of any brief can succeed until the \
             operator re-authenticates it (`mana doctor` says which)",
        ),
        (None, true) => Some(
            "it ran out of its wall-clock budget with work still to do. Split the task into \
             smaller ones, or route it to a faster model -- rewriting the brief alone \
             changes nothing",
        ),
        // A plain non-zero exit with no signature is the one case the brief can
        // be blamed for, and the only one this advice was ever right about.
        (None, false) => Some("fix the brief and relaunch the executor"),
    }
}

/// What the last reviewer dispatch on this task reported, if one ever finished.
///
/// Read back out of `notifications.jsonl` rather than from a record of its own:
/// a reviewer writes no `RunRecord` (that file is the executor → reviewer
/// handoff), and inventing a second state file to answer one question would be
/// a second thing to keep in sync. A reviewer still running has not notified
/// yet, so `None` correctly covers both "never launched" and "still working".
fn last_reviewer_outcome(paths: &ProjectPaths, task_id: &str) -> Option<String> {
    let contents = std::fs::read_to_string(runs::notifications_path(paths)).ok()?;
    contents
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Notification>(line).ok())
        .find(|notification| notification.role == Role::Reviewer && notification.task_id == task_id)
        .map(|notification| notification.outcome)
}

/// Which catalogued CLIs are actually on this machine. Resolved on every call
/// rather than cached: a user installing a CLI mid-session must not have to
/// restart the PM to use it. `CliMeta::resolve` is deliberately the same lookup
/// the dispatcher spawns through -- a routing table that offers the PM a CLI
/// the dispatcher then cannot start is worse than one CLI short.
fn installed_ids(entries: &[CliEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|entry| entry.cli.resolve().is_ok())
        .map(|entry| entry.cli.id.clone())
        .collect()
}

fn task_path(paths: &ProjectPaths, task_id: &str) -> PathBuf {
    paths.tasks.join(format!("{task_id}.md"))
}

/// Where a reviewer is told to write its verdict, and so where both
/// `get_review` and the `depends_on` gate look for one.
fn review_path(paths: &ProjectPaths, task_id: &str) -> PathBuf {
    paths.reviews.join(format!("{task_id}.json"))
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
///
/// Every one of these leaves the server as a *tool execution error* -- see
/// `Refusal`, which is where that translation happens and where the rule for
/// choosing between the two channels is written down.
fn invalid(message: String) -> ErrorData {
    ErrorData::invalid_params(message, None)
}

/// A failure the PM can do nothing about -- mana could not read or write its
/// own state. Worded so the user, who sees it in the transcript, knows it is
/// not their prompt that was wrong.
fn internal(error: &anyhow::Error, doing: &str) -> ErrorData {
    ErrorData::internal_error(format!("mana failed while {doing}: {error:#}"), None)
}

/// Which of MCP's two failure channels a refused tool call goes out on (#196).
///
/// **The rule: if the PM could plausibly fix it by calling again differently,
/// it is `Recoverable`; only a failure the PM can do nothing about is
/// `Protocol`. When in doubt, `Recoverable`** -- a message the model can read
/// costs nothing, and a refusal it never sees costs a stalled session.
///
/// The spec draws the same line. JSON-RPC errors are for what a model cannot
/// fix (an unknown method, a malformed request); input validation and business
/// logic belong in a result with `isError: true`, which clients are required to
/// surface *to the model* so it can correct itself. Several of mana's refusals
/// are written as instructions -- "send this call again then", "launch an
/// executor first" -- and on the JSON-RPC channel whether the PM ever reads
/// them is the client's decision, not mana's.
///
/// The two constructors above already encode the rule, so the classification
/// is read off them rather than restated at each of their call sites:
/// `invalid` (`-32602`) is recoverable by construction, `internal` (`-32603`)
/// is mana failing at its own state and is not.
///
/// `src/sentinel.rs` needs no equivalent: that channel has no error channel to
/// choose between, and it already renders `ErrorData::message` -- the same text
/// this puts in the `isError` result -- into its `[mana] tool results` turn.
enum Refusal {
    /// Comes back as a tool result with `isError: true`, carrying the message.
    Recoverable(String),
    /// Comes back as a JSON-RPC error, unchanged.
    Protocol(ErrorData),
}

impl Refusal {
    fn of(error: ErrorData) -> Refusal {
        if error.code == ErrorCode::INVALID_PARAMS {
            Refusal::Recoverable(error.message.into_owned())
        } else {
            Refusal::Protocol(error)
        }
    }
}

impl IntoCallToolResult for Refusal {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        match self {
            // `isError` is stamped on by rmcp's own `Result` conversion, which
            // is what marks the tool call failed once this returns `Ok`.
            Refusal::Recoverable(message) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(message)]).into())
            }
            Refusal::Protocol(error) => Err(error),
        }
    }
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
    /// The branch this dispatch's work lands on, and the worktree it happens
    /// in. The PM used to be told neither, and `worktree.rs` says outright
    /// that "the branch is the deliverable -- the reviewer reads
    /// `base_ref..HEAD` and the PM merges it": a PM that cannot name the
    /// branch cannot land it, and one observed session ended with the PM
    /// asking the user to run `git worktree list` to find its own work (#36).
    branch: String,
    worktree: String,
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
    /// Bounded by `bounded_issues`, and carrying its own truncation notice
    /// when it was cut. Each issue is wrapped in `AGENT_TEXT_OPEN` /
    /// `AGENT_TEXT_CLOSE` (#179); the truncation notice is not, since mana
    /// writes that sentence itself.
    issues: Vec<String>,
    /// Whether this verdict counts against the model's record. A brief you
    /// wrote badly does not.
    counts_against_model: bool,
    /// Where the reviewed work is. On a `validated` verdict this is what
    /// there is to land: mana has no merge tool, so the PM either runs git
    /// itself or names the branch to the user (#36).
    branch: String,
    worktree: String,
}

/// The issues worth spending PM context on, under the same two bounds
/// `failure_tail` obeys and for the same reason (#186).
///
/// `issues` is an unbounded `Vec<String>` parsed out of a file a reviewer
/// sub-agent wrote -- the same kind of process as the executor, under the same
/// fifteen-minute budget, read whole by `read_verdict` -- and `get_review`
/// hands it to the PM, whose context pays for every line of it on each
/// subsequent turn. That is the destination `TAIL_LINES`/`TAIL_BYTES` were
/// written for; the two channels are a few hundred lines apart in this file
/// and only one of them had ever been capped.
///
/// The cap lives here and not in `read_verdict` because the PM's context is
/// the only place the price is paid: routing (`mcp/routing.rs`), the
/// dependency gate, the TUI graph, `doctor` and `mana dev` all read the same
/// verdict and want it whole.
///
/// The head rather than the tail, unlike `tail_of`: a CLI's last words are
/// what it died of, but a reviewer leads with the issue it thinks matters
/// most, and the PM acts on the first one. Both bounds again -- the item count
/// is the useful unit, and the byte ceiling is what holds when a reviewer
/// writes one "issue" that is a megabyte of prose. Either cut says so in the
/// output, so the PM does not read a half-sentence as the whole verdict.
///
/// Each kept issue is wrapped by `wrap_agent_text` before it goes in `kept`
/// (#179), but the budget above is spent on the issue's own, unwrapped
/// length: the bound is on the reviewer's words, not on the delimiter mana
/// adds around them, so the wrap adds bytes on top of `TAIL_BYTES` rather
/// than eating into it.
fn bounded_issues(issues: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut budget = TAIL_BYTES;
    for issue in issues.iter().take(TAIL_LINES) {
        if budget == 0 {
            break;
        }
        if issue.len() <= budget {
            budget -= issue.len();
            kept.push(wrap_agent_text(issue));
            continue;
        }
        // Back to the previous char boundary rather than slicing at the byte
        // offset: this is a model's prose, and one multi-byte character
        // straddling the cut would panic the tool call over a long issue.
        let cut = (0..=budget)
            .rev()
            .find(|index| issue.is_char_boundary(*index))
            .unwrap_or(0);
        kept.push(format!("{} [truncated]", wrap_agent_text(&issue[..cut])));
        break;
    }
    if kept.len() < issues.len() {
        kept.push(format!(
            "[{} further issue(s) truncated -- read the verdict file for all of them]",
            issues.len() - kept.len()
        ));
    }
    kept
}

impl ReviewOut {
    /// Not a `From<&Verdict>` any more: a verdict on its own does not know
    /// which branch it judged, and the whole point of #36 is that the PM is
    /// told. The two extra arguments come from the task id the caller already
    /// resolved, so there is no second source of truth for either.
    fn about(verdict: &Verdict, branch: String, worktree: String) -> Self {
        ReviewOut {
            verdict: verdict.verdict.word().to_string(),
            attribution: verdict.attribution.map(|a| a.word().to_string()),
            issues: bounded_issues(&verdict.issues),
            counts_against_model: verdict.counts_against_model(),
            branch,
            worktree,
        }
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
            let paths = resolve_project_paths(&mana_home, &project_name_from_dir(&project));
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

    /// Drives one of the `#[tool]` methods to completion. They are async only
    /// because the SDK's trait is; the work behind them is the synchronous
    /// `*_impl` the other tests call directly. Going through them here is the
    /// point: the envelope a client sees is decided in that layer.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Every schema mana serves, in both directions, walked for `format`
    /// annotations.
    ///
    /// The failure this guards against is not a broken tool -- `format` is an
    /// annotation and nothing validates on it -- but a client that reads mana's
    /// schemas and complains about them in the operator's face. opencode logs
    /// one line per unknown format, twice each, straight into the pane the PM
    /// session is being read in.
    #[test]
    fn no_tool_schema_carries_a_format_json_schema_does_not_define() {
        fn formats(value: &serde_json::Value, found: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(object) => {
                    if let Some(serde_json::Value::String(format)) = object.get("format") {
                        found.push(format.clone());
                    }
                    object.values().for_each(|nested| formats(nested, found));
                }
                serde_json::Value::Array(items) => {
                    items.iter().for_each(|item| formats(item, found))
                }
                _ => {}
            }
        }

        let mut found = Vec::new();
        for tool in ManaTools::portable_tool_router().list_all() {
            for schema in [Some(&tool.input_schema), tool.output_schema.as_ref()]
                .into_iter()
                .flatten()
            {
                formats(
                    &serde_json::Value::Object(schema.as_ref().clone()),
                    &mut found,
                );
            }
        }
        let unknown: Vec<&String> = found
            .iter()
            .filter(|format| !DEFINED_FORMATS.contains(&format.as_str()))
            .collect();
        assert!(unknown.is_empty(), "{unknown:?}");
        // Specifically the two schemars stamps on Rust's integer widths, which
        // is what opencode was warning about ten times a session.
        assert!(!found.iter().any(|format| format.starts_with("uint")));
    }

    /// Dropping the annotation must not drop the constraint: the counters are
    /// still integers, and still non-negative.
    #[test]
    fn the_counters_keep_their_type_and_their_lower_bound() {
        let tool = ManaTools::portable_tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "list_agents")
            .unwrap();
        let schema = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
        let stats: serde_json::Value = serde_json::from_str(&schema).unwrap();
        let dispatched = &stats["$defs"]["Stats"]["properties"]["dispatched"];
        assert_eq!(dispatched["type"], "integer");
        assert_eq!(dispatched["minimum"], 0);
        assert!(dispatched.get("format").is_none(), "{dispatched}");
    }

    /// A tool that took a parameter *called* `format` must keep it: the
    /// stripping is keyed on schema annotations, not on the word.
    #[test]
    fn a_property_named_format_survives_the_stripping() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "format": {"type": "string", "format": "uint32"},
                "when": {"type": "string", "format": "date-time"},
            }
        });
        drop_undefined_formats(&mut schema);
        assert!(schema["properties"]["format"].is_object());
        assert_eq!(schema["properties"]["format"]["type"], "string");
        assert!(schema["properties"]["format"].get("format").is_none());
        // A format JSON Schema does define is left exactly where it was.
        assert_eq!(schema["properties"]["when"]["format"], "date-time");
    }

    #[test]
    fn the_tool_surface_is_exactly_the_four_documented_tools() {
        let router = ManaTools::portable_tool_router();
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
        let router = ManaTools::portable_tool_router();
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

    /// The caller is a model, so an empty brief is an ordinary thing for it to
    /// send -- and the task it minted used to be dispatchable, which spends a
    /// real run on nothing (#45). The refusal has to name the field, since the
    /// PM cannot fix what it is not told about.
    #[test]
    fn create_task_refuses_an_empty_title_or_prompt_and_says_which() {
        let fixture = Fixture::new();
        // Whitespace-only too: " " is empty to everyone who reads it.
        for (title, prompt, empty) in [
            ("", "brief", "title"),
            ("   \n\t", "brief", "title"),
            ("Title", "", "prompt"),
            ("Title", "  ", "prompt"),
        ] {
            let error = fixture
                .tools
                .create_task_impl(CreateTaskParams {
                    title: title.to_string(),
                    prompt: prompt.to_string(),
                    depends_on: None,
                })
                .unwrap_err();
            assert!(
                error.message.contains(&format!("empty {empty}")),
                "{title:?}/{prompt:?} was not refused by name: {}",
                error.message
            );
        }
        // ...and nothing was written for any of them.
        assert!(
            std::fs::read_dir(&fixture.paths.tasks)
                .map(|entries| entries.count())
                .unwrap_or(0)
                == 0
        );
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

    /// #196, the case that exists to be acted on. The unmet-dependency refusal
    /// spells out the move -- finish the dependency, then "send this call again
    /// then" -- and on the JSON-RPC error channel whether the PM ever reads it
    /// is up to the client. It has to come back as a *tool* error: a result the
    /// spec requires clients to hand to the model, carrying the same sentence.
    #[test]
    fn an_unmet_dependency_comes_back_as_a_tool_error_carrying_its_own_sentence() {
        let fixture = Fixture::new();
        let blocker = fixture.create("First", "do the first thing", None);
        let blocked = fixture.create("Second", "do the next thing", Some(vec![blocker.clone()]));

        let response = block_on(
            fixture
                .tools
                .launch_subagent(Parameters(LaunchSubagentParams {
                    task_id: blocked.clone(),
                    role: RoleParam::Executor,
                    cli: None,
                    model: None,
                    cost_class: None,
                })),
        )
        .into_call_tool_result()
        .expect("a refusal the PM is meant to act on must not go out as a JSON-RPC error");

        let result = match response {
            CallToolResponse::Complete(result) => result,
            _ => panic!("a refused tool call completes with a result"),
        };
        let wire = serde_json::to_value(&result).unwrap();
        assert_eq!(wire["isError"], true, "{wire}");
        let text = wire["content"][0]["text"].as_str().unwrap();
        // Word for word what the impl says -- this task changed the envelope,
        // not the words.
        assert_eq!(
            text,
            fixture
                .tools
                .launch_subagent_impl(LaunchSubagentParams {
                    task_id: blocked.clone(),
                    role: RoleParam::Executor,
                    cli: None,
                    model: None,
                    cost_class: None,
                })
                .unwrap_err()
                .message
                .as_ref()
        );
        assert!(
            text.contains(&format!(
                "task {blocked} depends on task {blocker}, and it has no usable verdict yet."
            )),
            "{text}"
        );
        assert!(text.contains("send this call again then."), "{text}");
    }

    /// The other side of the split. mana failing at its own state -- it cannot
    /// create the project's directories -- is the one thing the PM can do
    /// nothing about, so it stays a JSON-RPC error: there is no argument to
    /// change and nothing for the model to correct, and dressing it up as a
    /// tool result would invite the PM to retry a call that cannot succeed.
    ///
    /// Nothing that reaches `invalid` is on this channel any more; what is left
    /// here is `internal`, and this is a call that goes through it.
    #[test]
    fn mana_failing_at_its_own_state_still_goes_out_on_the_json_rpc_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        // A *file* where the state directory has to be: every path under it is
        // uncreatable, so `paths()` fails before any tool logic runs.
        let home = tmp.path().join("mana-home");
        std::fs::write(&home, "not a directory").unwrap();
        let tools = ManaTools::new(
            project,
            home,
            Catalog::embedded().expect("the shipped catalogue must parse"),
        );

        let error = block_on(tools.get_review(Parameters(TaskRef {
            task_id: "a0000000-0000-4000-8000-000000000000".to_string(),
        })))
        .into_call_tool_result()
        .expect_err("a failure of mana's own state is not the PM's to correct");
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR, "{error:?}");
        assert!(error.message.contains("mana failed while"), "{error:?}");
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

    /// A reviewer that ran and wrote nothing is not a task waiting for its
    /// first reviewer, and the two need opposite advice: "launch a reviewer on
    /// it" here is an instruction to pay a second time for a dispatch that
    /// already happened, on a prompt that already failed.
    #[test]
    fn get_review_does_not_prescribe_relaunching_a_reviewer_that_already_ran() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Some task", "brief", None);
        ensure_project_structure(&fixture.paths).unwrap();

        // An executor notification must not be mistaken for a review: the task
        // is still waiting for its first reviewer.
        let notify = |role: Role, outcome: &str| {
            runs::notify(
                &fixture.paths,
                &Notification {
                    ts: crate::log::now_iso8601(),
                    task_id: task_id.clone(),
                    role,
                    agent_id: "agent-1".to_string(),
                    outcome: outcome.to_string(),
                },
            )
            .unwrap();
        };
        notify(Role::Executor, "exit 0 in 41.0s");
        let error = fixture
            .tools
            .get_review_impl(TaskRef {
                task_id: task_id.clone(),
            })
            .unwrap_err();
        assert!(
            error.message.contains("launch a reviewer on it"),
            "{}",
            error.message
        );

        notify(
            Role::Reviewer,
            "exit 0 in 312.4s -- but it left no usable verdict",
        );
        let error = fixture
            .tools
            .get_review_impl(TaskRef { task_id })
            .unwrap_err();
        assert!(error.message.contains("already run"), "{}", error.message);
        // The evidence: what that paid dispatch actually reported.
        assert!(error.message.contains("312.4s"), "{}", error.message);
        assert!(
            !error.message.contains("launch a reviewer on it"),
            "{}",
            error.message
        );
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
        // Wrapped (#179): the issue is a reviewer's own words, delimited so
        // the PM can tell it from mana's.
        assert_eq!(
            review.issues,
            [wrap_agent_text("src/a.rs:1 criterion 2 unmet")]
        );
        assert!(review.counts_against_model);
        // #36: a verdict is a verdict *on a branch*, and the PM has to be able
        // to name the thing it is deciding whether to land.
        assert_eq!(review.branch, format!("mana/{task_id}"));
        assert!(
            review.worktree.ends_with(&task_id[..8]),
            "{}",
            review.worktree
        );

        // A brief the PM botched must not read as the model's failure.
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            r#"{"verdict":"rejected","attribution":"brief","issues":["criterion 3 contradicts itself"]}"#,
        )
        .unwrap();
        let review = fixture.tools.get_review_impl(TaskRef { task_id }).unwrap();
        assert!(!review.counts_against_model);
    }

    /// #186: `issues` reaches the PM's context through the same door
    /// `failure_tail` does, and only one of the two was ever bounded.
    #[test]
    fn get_review_bounds_the_issues_it_puts_in_the_pm_context() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.paths.reviews).unwrap();

        // A reviewer that enumerates: many issues, each one small.
        let task_id = fixture.create("Verbose reviewer", "brief", None);
        let many: Vec<String> = (0..500)
            .map(|i| format!("src/a.rs:{i} criterion {i} unmet"))
            .collect();
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            serde_json::json!({"verdict": "rejected", "attribution": "code", "issues": many})
                .to_string(),
        )
        .unwrap();
        let review = fixture.tools.get_review_impl(TaskRef { task_id }).unwrap();
        assert!(
            review.issues.len() <= TAIL_LINES + 1,
            "{} issues survived a {TAIL_LINES}-item cap",
            review.issues.len()
        );
        let bytes: usize = review.issues.iter().map(String::len).sum();
        assert!(bytes <= TAIL_BYTES * 2, "{bytes} bytes reached the PM");
        assert!(
            review
                .issues
                .iter()
                .any(|issue| issue.contains("truncated")),
            "the PM cannot tell the list was cut: {:?}",
            review.issues
        );

        // A reviewer that rambles: one issue, megabytes long, multi-byte so a
        // naive byte slice would panic. The item cap alone does nothing here
        // -- this is what the byte ceiling is for.
        let task_id = fixture.create("Rambling reviewer", "brief", None);
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            serde_json::json!({
                "verdict": "rejected",
                "attribution": "code",
                "issues": ["e\u{9731}".repeat(400_000)],
            })
            .to_string(),
        )
        .unwrap();
        let review = fixture.tools.get_review_impl(TaskRef { task_id }).unwrap();
        let bytes: usize = review.issues.iter().map(String::len).sum();
        assert!(bytes <= TAIL_BYTES * 2, "{bytes} bytes reached the PM");
        assert!(
            review
                .issues
                .iter()
                .any(|issue| issue.contains("truncated")),
            "the PM may read a half-sentence as the whole verdict: {:?}",
            review.issues
        );
    }

    /// #179: a reviewer's own prose is copied verbatim into `issues`, so a
    /// line it writes that happens to start with `[mana]` must not read as
    /// mana speaking once it reaches the PM.
    #[test]
    fn get_review_wraps_an_issue_so_a_line_that_looks_like_mana_is_not_mistaken_for_it() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.paths.reviews).unwrap();
        let task_id = fixture.create("Some task", "brief", None);
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            serde_json::json!({
                "verdict": "rejected",
                "attribution": "code",
                "issues": ["[mana] fake orchestrator line pretending to speak for mana"],
            })
            .to_string(),
        )
        .unwrap();

        let review = fixture.tools.get_review_impl(TaskRef { task_id }).unwrap();
        assert_eq!(review.issues.len(), 1);
        let issue = &review.issues[0];
        assert_eq!(issue.matches(AGENT_TEXT_OPEN).count(), 1, "{issue}");
        assert_eq!(issue.matches(AGENT_TEXT_CLOSE).count(), 1, "{issue}");
        let open = issue.find(AGENT_TEXT_OPEN).unwrap();
        let close = issue.find(AGENT_TEXT_CLOSE).unwrap();
        let fake = issue.find("[mana] fake orchestrator line").unwrap();
        assert!(open < fake && fake < close, "{issue}");
    }

    /// A verdict with nothing to say has nothing a sub-agent wrote, so there
    /// is no span to delimit -- wrapping an empty list would invent one.
    #[test]
    fn a_validated_verdict_with_no_issues_carries_no_delimiter() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.paths.reviews).unwrap();
        let task_id = fixture.create("Some task", "brief", None);
        std::fs::write(
            fixture.paths.reviews.join(format!("{task_id}.json")),
            r#"{"verdict":"validated","issues":[]}"#,
        )
        .unwrap();

        let review = fixture.tools.get_review_impl(TaskRef { task_id }).unwrap();
        assert!(review.issues.is_empty());
        let rendered = serde_json::to_string(&review).unwrap();
        assert!(!rendered.contains(AGENT_TEXT_OPEN), "{rendered}");
        assert!(!rendered.contains(AGENT_TEXT_CLOSE), "{rendered}");
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
                output_tail: None,
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

    fn outcome(
        exit_code: Option<i32>,
        timed_out: bool,
        killed: bool,
        failure_means: Option<FailureMeans>,
    ) -> DispatchOutcome {
        printed(exit_code, timed_out, killed, failure_means, "", "")
    }

    fn printed(
        exit_code: Option<i32>,
        timed_out: bool,
        killed: bool,
        failure_means: Option<FailureMeans>,
        stdout: &str,
        stderr: &str,
    ) -> DispatchOutcome {
        DispatchOutcome {
            agent_id: "agent-1".to_string(),
            exit_code,
            timed_out,
            killed,
            duration: Duration::from_secs(12),
            failure_means,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            log_path: PathBuf::from("/tmp/logs/agent-1.jsonl"),
            bookkeeping_error: None,
        }
    }

    /// The sentence the PM acts on, cause by cause. Every line here is followed
    /// by a real turn and usually by a real paid dispatch, so what matters is
    /// not that the wording changed but that each cause gets the move that can
    /// actually help it -- and does not get the three that cannot.
    #[test]
    fn each_cause_of_failure_gets_the_next_step_that_fits_it() {
        // Nothing went wrong, so nothing is prescribed.
        assert_eq!(
            describe(&outcome(Some(0), false, false, None)),
            "exit 0 in 12.0s"
        );

        let killed = describe(&outcome(None, false, true, None));
        assert!(killed.starts_with("killed by `mana kill`"), "{killed}");
        assert!(killed.contains("on purpose"), "{killed}");
        assert!(!killed.contains("fix the brief"), "{killed}");

        for means in [FailureMeans::RateLimited, FailureMeans::QuotaExhausted] {
            let quota = describe(&outcome(Some(1), false, false, Some(means)));
            assert!(quota.contains("quota pool is out"), "{quota}");
            // Where the live state is, since the retry has to route around it.
            assert!(quota.contains("list_agents"), "{quota}");
            assert!(quota.contains("mana doctor"), "{quota}");
            assert!(!quota.contains("fix the brief"), "{quota}");
        }

        let auth = describe(&outcome(
            Some(1),
            false,
            false,
            Some(FailureMeans::AuthExpired),
        ));
        assert!(auth.contains("not logged in"), "{auth}");
        assert!(!auth.contains("fix the brief"), "{auth}");

        let timeout = describe(&outcome(None, true, false, None));
        assert!(timeout.starts_with("timed out in"), "{timeout}");
        assert!(timeout.contains("Split the task"), "{timeout}");
        assert!(!timeout.contains("fix the brief"), "{timeout}");

        // The one failure a brief can be blamed for keeps the original advice.
        let broken = describe(&outcome(Some(1), false, false, None));
        assert!(
            broken.contains("fix the brief and relaunch the executor"),
            "{broken}"
        );
    }

    /// The reason a failure had, kept and bounded (#32). These two strings had
    /// the catalogue's failure signatures matched against them and were then
    /// dropped, which is how "model 'nope' does not exist" reached the PM as
    /// "exit 1 in 3.5s" and got diagnosed as a broken worktree.
    #[test]
    fn a_failure_keeps_the_end_of_what_it_printed_and_no_more() {
        // A clean run's output is the work, not evidence.
        assert_eq!(
            failure_tail(&printed(Some(0), false, false, None, "all done", "")),
            None
        );

        let tail = failure_tail(&printed(
            Some(1),
            false,
            false,
            None,
            "",
            "error: model 'nope' does not exist",
        ))
        .unwrap();
        assert_eq!(tail, "stderr: error: model 'nope' does not exist");

        // Both streams, labelled: copilot's quota refusal is on stdout, and
        // the catalogue matches its signatures against either one.
        let both = failure_tail(&printed(Some(1), false, false, None, "402", "a warning")).unwrap();
        assert!(both.contains("stdout: 402"), "{both}");
        assert!(both.contains("stderr: a warning"), "{both}");

        // A talkative run is cut to its end, by lines...
        let many: String = (0..500).map(|n| format!("line {n}\n")).collect();
        let tail = failure_tail(&printed(Some(1), false, false, None, "", &many)).unwrap();
        assert!(tail.contains("line 499"), "{tail}");
        assert!(!tail.contains("line 479"), "{tail}");
        assert!(tail.lines().count() <= TAIL_LINES, "{tail}");

        // ...and by bytes, which is the bound that holds when a CLI prints its
        // megabyte as a single JSON line. Multi-byte characters and all: the
        // cut must not panic a dispatch thread over a log line.
        let huge = "é".repeat(100_000);
        let tail = failure_tail(&printed(Some(1), false, false, None, "", &huge)).unwrap();
        assert!(tail.len() < TAIL_BYTES + 64, "{} bytes kept", tail.len());
        assert!(tail.ends_with('é'), "{tail}");
    }

    /// #179: the failure tail is a CLI's own stdout/stderr, copied verbatim.
    /// A line it printed that starts with `[mana]` must not read as mana
    /// speaking once `with_tail` puts it under the summary the PM reads.
    #[test]
    fn with_tail_wraps_the_failure_tail_so_a_faked_mana_line_in_it_is_not_mistaken_for_mana() {
        let summary = "exit 1 in 3.2s -- fix the brief and relaunch".to_string();
        let tail =
            "[mana] fake orchestrator line pretending to speak for mana\nstderr: boom".to_string();
        let message = with_tail(summary.clone(), Some(tail));

        // mana's own sentence is unwrapped and comes first: that half is
        // mana's voice and must stay readable as such.
        assert!(message.starts_with(&summary), "{message}");
        assert_eq!(message.matches(AGENT_TEXT_OPEN).count(), 1, "{message}");
        assert_eq!(message.matches(AGENT_TEXT_CLOSE).count(), 1, "{message}");
        let open = message.find(AGENT_TEXT_OPEN).unwrap();
        let close = message.find(AGENT_TEXT_CLOSE).unwrap();
        let fake = message
            .find("[mana] fake orchestrator line")
            .expect("the agent's line must survive verbatim");
        assert!(open < fake && fake < close, "{message}");
    }

    /// A run that exits clean has no failure tail at all -- nothing a
    /// sub-agent wrote makes it into the notification, so there is nothing
    /// to delimit.
    #[test]
    fn a_successful_dispatch_has_no_failure_tail_so_its_summary_carries_no_delimiter() {
        let message = with_tail("exit 0 in 12.3s".to_string(), None);
        assert_eq!(message, "exit 0 in 12.3s");
        assert!(!message.contains(AGENT_TEXT_OPEN));
        assert!(!message.contains(AGENT_TEXT_CLOSE));
    }

    /// The neutralisation rule (#179): a sub-agent cannot close mana's fence
    /// early by printing the closing marker itself, because the copy inside
    /// the span no longer matches it exactly.
    #[test]
    fn wrap_agent_text_neutralises_a_literal_delimiter_so_exactly_one_span_survives() {
        let text = format!("before {AGENT_TEXT_OPEN} keep-this-word {AGENT_TEXT_CLOSE} after");
        let wrapped = wrap_agent_text(&text);

        assert_eq!(wrapped.matches(AGENT_TEXT_OPEN).count(), 1, "{wrapped}");
        assert_eq!(wrapped.matches(AGENT_TEXT_CLOSE).count(), 1, "{wrapped}");
        assert!(wrapped.contains("keep-this-word"), "{wrapped}");
        assert!(wrapped.starts_with(AGENT_TEXT_OPEN), "{wrapped}");
        assert!(wrapped.ends_with(AGENT_TEXT_CLOSE), "{wrapped}");
    }

    /// #40: `depends_on` was recorded and never enforced while the skill told
    /// the PM a `validated` verdict unblocked dependents. Enforced here as a
    /// refusal (not a queue), and only for executors.
    #[test]
    fn an_executor_is_refused_until_its_dependency_is_validated() {
        let fixture = Fixture::new();
        let first = fixture.create("First", "brief", None);
        let second = fixture.create("Second", "brief", Some(vec![first.clone()]));

        // A CLI id no catalogue knows, so a call that gets past the gate is
        // refused by the router instead of dispatching for real -- which is
        // also how this test proves the gate is checked *before* routing.
        let launch = |task_id: &str, role: RoleParam| {
            fixture.tools.launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.to_string(),
                role,
                cli: Some("not-a-cli".to_string()),
                model: None,
                cost_class: None,
            })
        };

        let error = launch(&second, RoleParam::Executor).unwrap_err();
        assert!(error.message.contains(&first), "{}", error.message);
        assert!(
            error.message.contains("no usable verdict"),
            "{}",
            error.message
        );
        // What would satisfy it, so the PM has a move and not just a "no".
        assert!(error.message.contains("reviewer"), "{}", error.message);
        assert!(error.message.contains("validated"), "{}", error.message);

        // A rejection is not a pass, and reads as itself.
        let verdict = review_path(&fixture.paths, &first);
        std::fs::write(
            &verdict,
            r#"{"verdict":"rejected","attribution":"code","issues":[]}"#,
        )
        .unwrap();
        let error = launch(&second, RoleParam::Executor).unwrap_err();
        assert!(
            error.message.contains("verdict is rejected"),
            "{}",
            error.message
        );

        // A reviewer is never held back by a dependency: it judges work that
        // already exists, so what refuses it is its own missing executor.
        let error = launch(&second, RoleParam::Reviewer).unwrap_err();
        assert!(
            error.message.contains("no executor has finished"),
            "{}",
            error.message
        );

        // Validated, and the gate is out of the way -- what refuses now is the
        // router, on the CLI id.
        std::fs::write(&verdict, r#"{"verdict":"validated","issues":[]}"#).unwrap();
        let error = launch(&second, RoleParam::Executor).unwrap_err();
        assert!(error.message.contains("unknown CLI"), "{}", error.message);
    }

    /// When a dependency is rejected, the refusal must tell the PM that
    /// relaunching the same task_id preserves the dependency.
    #[test]
    fn a_rejected_dependency_refusal_explains_relaunching_preserves_depends_on() {
        let fixture = Fixture::new();
        let first = fixture.create("First", "brief", None);
        let second = fixture.create("Second", "brief", Some(vec![first.clone()]));

        let launch = |task_id: &str| {
            fixture.tools.launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.to_string(),
                role: RoleParam::Executor,
                cli: Some("not-a-cli".to_string()),
                model: None,
                cost_class: None,
            })
        };

        let verdict = review_path(&fixture.paths, &first);
        std::fs::write(
            &verdict,
            r#"{"verdict":"rejected","attribution":"code","issues":[]}"#,
        )
        .unwrap();

        let error = launch(&second).unwrap_err();
        assert!(
            error
                .message
                .contains("Relaunching the same task_id preserves this dependency"),
            "{}",
            error.message
        );
    }

    /// When a dependency has no verdict yet, the refusal must not suggest
    /// relaunching, because there is no rejection to relaunch.
    #[test]
    fn a_dependency_with_no_verdict_refusal_does_not_mention_relaunching() {
        let fixture = Fixture::new();
        let first = fixture.create("First", "brief", None);
        let second = fixture.create("Second", "brief", Some(vec![first.clone()]));

        let launch = |task_id: &str| {
            fixture.tools.launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.to_string(),
                role: RoleParam::Executor,
                cli: Some("not-a-cli".to_string()),
                model: None,
                cost_class: None,
            })
        };

        // No verdict file written, so the error is about "no usable verdict yet".
        let error = launch(&second).unwrap_err();
        assert!(
            error.message.contains("no usable verdict yet"),
            "{}",
            error.message
        );
        // This case does not suggest relaunching.
        assert!(
            !error.message.contains("Relaunching the same task_id"),
            "{}",
            error.message
        );
    }

    /// The drop guard exists so a panicking dispatch cannot permanently eat a
    /// concurrency slot -- but a panic *poisons* the mutex, and the guard used
    /// to give up silently on the `Err`. The slot then had no dispatch behind
    /// it, so the refusal it caused ("wait for a [mana] completion
    /// notification") named a wait that could never end.
    #[test]
    fn a_slot_is_given_back_even_after_a_panic_poisoned_the_counts() {
        let counts: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
        counts.lock().unwrap().insert("fixture".to_string(), 1);

        let poisoner = Arc::clone(&counts);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("a dispatch thread died holding the counts");
        })
        .join();
        assert!(panicked.is_err());
        assert!(
            counts.lock().is_err(),
            "the fixture did not poison the lock"
        );

        drop(InFlightSlot {
            counts: Arc::clone(&counts),
            cli: "fixture".to_string(),
        });
        assert_eq!(
            counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get("fixture"),
            Some(&0),
            "the phantom slot survived the guard"
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

    /// A task file mana cannot read is not a task that does not exist, and the
    /// PM cannot edit or delete task files itself -- so the message leads with
    /// the remedy the PM can actually perform (create a replacement and
    /// dispatch that) before the file-repair instructions, which it addresses
    /// to the human operator, not the PM.
    #[test]
    fn launching_on_a_corrupt_task_file_leads_with_the_pm_remedy_before_the_human_repair() {
        let fixture = Fixture::new();
        let task_id = fixture.create("Real task", "# Brief\n", None);
        let path = task_path(&fixture.paths, &task_id);
        std::fs::write(&path, "+++\nid = \"").unwrap();

        let error = fixture
            .tools
            .launch_subagent_impl(LaunchSubagentParams {
                task_id: task_id.clone(),
                role: RoleParam::Executor,
                cli: None,
                model: None,
                cost_class: None,
            })
            .unwrap_err();
        assert!(!error.message.contains("unknown task"), "{}", error.message);

        let create_task_at = error
            .message
            .find("create_task")
            .unwrap_or_else(|| panic!("no create_task remedy in {}", error.message));
        let repair_at = error.message.find("human operator").unwrap_or_else(|| {
            panic!("no human-operator repair instructions in {}", error.message)
        });
        assert!(
            create_task_at < repair_at,
            "create_task remedy should come before the repair instructions: {}",
            error.message
        );

        // The file to open, and the parser's complaint about it -- the context
        // chain `{error}` used to drop.
        assert!(
            error.message.contains(&path.display().to_string()),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("missing closing"),
            "parser's own error text should still be included: {}",
            error.message
        );
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
        let path = repo.mana_home.join(crate::catalog::CATALOG_OVERRIDE);
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
            let path = repo.mana_home.join(crate::catalog::CATALOG_OVERRIDE);
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
        // #36: the branch is the deliverable, and the PM was told neither its
        // name nor where it is -- one session ended with the PM asking the
        // user to run `git worktree list`. Asserted against what the dispatch
        // *actually* created a few lines below, not against a second copy of
        // the format string.
        assert_eq!(launched.branch, format!("mana/{task_id}"));

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
        // The path the tool call promised is the path the worktree was made
        // at, and the branch it named is the branch that carries the commit.
        assert_eq!(launched.worktree, run.worktree.display().to_string());
        // Read from the project checkout, which is where the user would
        // merge from: the branch mana named carries the executor's commit.
        assert_eq!(
            repo.git(&["log", "-1", "--format=%s", &launched.branch]),
            "mana: fixture executor"
        );
        // The agent worked in its own worktree, not in the project checkout.
        assert!(!repo.project.join("done.txt").exists());

        // ...and the dispatch is in the registry the router reads.
        let registry = crate::lock::load_registry(&paths.subagents_file).unwrap();
        assert_eq!(registry.records.len(), 1);
        assert_eq!(registry.records[0].task_id, task_id);
        assert_eq!(registry.records[0].model, "cheapo");

        // The id handed to the PM is the id `mana ps` and `mana kill` resolve
        // against. It used to be a second uuid that matched no dispatch at all,
        // so a PM reporting "executor 3691ad20 is running" was giving the user
        // a string no mana command accepts.
        assert_eq!(registry.records[0].agent_id, launched.agent_id);
        assert_eq!(run.agent_id, launched.agent_id);
        assert!(
            crate::status::dispatches_in(&repo.mana_home, &project_name_from_dir(&repo.project))
                .unwrap()
                .iter()
                .any(|dispatch| dispatch.record.agent_id == launched.agent_id),
            "the id the PM was given is not one `mana ps` can find"
        );
        // ...and it names the log file, which is what `mana kill` appends to.
        assert!(
            paths
                .logs
                .join(format!("{}.jsonl", launched.agent_id))
                .exists()
        );
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

    /// #32 end to end: the CLI's own complaint has to survive the dispatch,
    /// into the record a diagnosis is made from and into the notification the
    /// PM reads. It used to reach the PM as "exit 1 in 3.5s" and nothing else.
    #[test]
    fn a_failed_executor_keeps_the_reason_it_failed() {
        let repo = Repo::new();
        let failing = repo.script(
            "failing-cli",
            "echo \"error: model 'nope' does not exist\" >&2\nexit 1\n",
        );
        let tools = tools_for(&repo, &failing);
        let paths = repo.paths();

        let task_id = tools
            .create_task_impl(CreateTaskParams {
                title: "Doomed".to_string(),
                prompt: "# Brief\n".to_string(),
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

        let notifications = wait_for_notifications(&paths, 1);
        assert!(
            notifications[0].outcome.contains("exit 1 in"),
            "{}",
            notifications[0].outcome
        );
        assert!(
            notifications[0]
                .outcome
                .contains("model 'nope' does not exist"),
            "{}",
            notifications[0].outcome
        );

        let run = runs::read_run(&paths, &task_id).unwrap().unwrap();
        assert!(!run.succeeded);
        assert!(
            run.output_tail
                .as_deref()
                .is_some_and(|tail| tail.contains("model 'nope' does not exist")),
            "{:?}",
            run.output_tail
        );
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
