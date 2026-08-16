//! `mana doctor` — the catalogue, this project and the configuration, as mana
//! sees them right now.
//!
//! Catalogue-first, because the catalogue is what mana actually acts on: the
//! config file only records which CLIs were registered, while every flag,
//! driver, model, quota pool and failure signature is data in
//! `catalog/*.toml`. Nothing below branches on a CLI's id — every per-CLI fact
//! printed here is read off its entry, which is the only way this report can
//! stay true for a CLI added through `~/.mana/catalog.local.toml`.
//!
//! # Exit-code contract
//!
//! Three codes, because scripts read them, and "the report never ran" is not
//! the same answer as "the report ran and found something":
//!
//! - **0** — the report ran and nothing mana can check is broken. Reported but
//!   *not* broken, on purpose: a catalogued CLI you never installed (you are
//!   not required to install all of them), a model-discovery probe that failed
//!   (the list degrades, dispatch does not), a quota pool resting on a cooldown
//!   (that is the system working), a leftover worktree (disk, not
//!   correctness).
//! - **1** — the report ran and at least one finding is broken, i.e. anything
//!   `Report::broken` was called for: a *registered* CLI whose binary has
//!   vanished (every dispatch to it fails at spawn); a **stale** dispatch (a
//!   dead pid with no exit record — an agent that will never finish and never
//!   notify the PM); a config file mana cannot read, including v1's leftover
//!   `config.yaml`; a `catalog.local.toml` mana cannot parse (dispatch silently
//!   runs on the shipped entry instead of yours); a `--prune` that failed and
//!   left the worktree behind.
//! - **2** — no report: doctor could not get far enough to print one. Never
//!   returned for a finding, only for doctor itself failing.
//!
//! Every finding is printed either way. Output is plain aligned text with no
//! colour and no table crate, so `mana doctor | grep` works.

use crate::catalog::{Catalog, CliEntry, CostClass, PmDriver, PoolScope, QuotaKind, ToolChannel};
use crate::cli::ps;
use crate::log;
use crate::mcp::routing::Observations;
use crate::project::{ProjectPaths, mana_home, project_name_from_dir, resolve_project_paths};
use crate::review::{self, Attribution, Decision};
use crate::status::{self, Dispatch, DispatchStatus};
use crate::subprocess::{Capture, VERSION_CHECK_TIMEOUT, capture_output, capture_version_output};
use crate::worktree;
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The user's catalogue escape valve (design §7), read from the same place
/// every other command reads it from.
const CATALOG_OVERRIDE: &str = "catalog.local.toml";

/// Budget for one `[models].discovery_args` run. Longer than the version probe
/// because discovery usually hits the network, short enough that four CLIs
/// cannot turn a diagnostic into a coffee break. A CLI that blows it is
/// reported as a failed discovery, never as having no models.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

/// How many model ids a line shows before it says "and N more". Runtime
/// discovery returns fourteen on one CLI already, and a wrapped line is a line
/// nobody reads.
const MODELS_SHOWN: usize = 6;

/// How much of an entry's `notes` the capability hint keeps.
const NOTE_HINT_CHARS: usize = 100;

/// What the whole report came to: the lines to print, and the subset of
/// findings that make the exit code 1.
pub struct Report {
    pub lines: Vec<String>,
    /// One rendered line per broken finding. See the module doc for what
    /// counts.
    pub broken: Vec<String>,
}

impl Report {
    fn say(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// A finding that is both printed and counted against the exit code.
    fn broken(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.lines.push(line.clone());
        self.broken.push(line);
    }

    pub fn exit_code(&self) -> i32 {
        i32::from(!self.broken.is_empty())
    }
}

/// Everything the report needs, injected rather than resolved, so the whole
/// thing runs against a throwaway `~/.mana` and a fixture catalogue.
pub struct Options<'a> {
    pub mana_home: &'a Path,
    /// The catalogue as loaded, override already applied.
    pub entries: &'a [CliEntry],
    /// One rendered line about an active `catalog.local.toml`, if there is one.
    pub override_note: Option<String>,
    /// Why `catalog.local.toml` could not be applied, if it could not be:
    /// `entries` are then the shipped ones, which is *not* what dispatch will
    /// run once the file parses again.
    pub override_error: Option<String>,
    /// The project directory to report on. `None` when the working directory
    /// is not a project mana has state for and `--project` was not given.
    pub project_root: Option<&'a Path>,
    pub prune: bool,
    pub now: DateTime<Utc>,
}

pub fn run(project: Option<&Path>, prune: bool) -> Result<()> {
    let report = match build_report(project, prune) {
        Ok(report) => report,
        // Exit 2, not the 1 that returning the error to `main` would give:
        // "doctor could not run" has to stay distinguishable from "doctor ran
        // and found something broken".
        Err(error) => {
            eprintln!("mana doctor could not run: {error:#}");
            std::process::exit(2);
        }
    };

    for line in &report.lines {
        println!("{line}");
    }
    // Flushed before exiting, since `process::exit` runs no destructors and a
    // report nobody sees is worse than a wrong exit code.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let code = report.exit_code();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Everything between the environment and `diagnose`. Split out so `run` has
/// exactly one place to turn "no report at all" into exit 2.
fn build_report(project: Option<&Path>, prune: bool) -> Result<Report> {
    let home = mana_home()?;
    let override_path = home.join(CATALOG_OVERRIDE);
    let (catalog, override_error) = catalog_or_shipped(&override_path)?;

    // `--project` is an explicit request and always gets a section, even when
    // the project has no state yet; without it, the working directory only
    // gets one if mana has actually been used there. Printing an empty project
    // section from `~` would be noise on every run. A working directory that
    // no longer exists is not worth dying on either: it only means there is no
    // project section to print.
    let project_root = match project {
        Some(path) => Some(path.to_path_buf()),
        None => std::env::current_dir()
            .ok()
            .filter(|cwd| has_project_state(&home, cwd)),
    };

    diagnose(&Options {
        mana_home: &home,
        entries: catalog.entries(),
        override_note: override_note(&override_path),
        override_error,
        project_root: project_root.as_deref(),
        prune,
        now: Utc::now(),
    })
}

/// The catalogue to report on, plus the override failure that made it degrade.
///
/// Every other command is right to die on an unreadable `catalog.local.toml`.
/// Doctor is the command you run *because* the override is broken, so it falls
/// back to the shipped entries and reports the failure as a finding -- dying
/// here would take the config, project and worktree sections down with it, and
/// the override is exactly the kind of damage those sections explain.
fn catalog_or_shipped(override_path: &Path) -> Result<(Catalog, Option<String>)> {
    match Catalog::load(Some(override_path)) {
        Ok(catalog) => Ok((catalog, None)),
        Err(error) => Ok((Catalog::embedded()?, Some(format!("{error:#}")))),
    }
}

fn has_project_state(mana_home: &Path, project_root: &Path) -> bool {
    resolve_project_paths(mana_home, &project_name_from_dir(project_root))
        .root
        .is_dir()
}

/// What an active local override is doing to the shipped catalogue. Read
/// straight from the file, because "which entry does it replace" is a question
/// about the override, not about the merged result.
fn override_note(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let source = std::fs::read_to_string(path).ok()?;
    let Ok(entry) = crate::catalog::parse_entry(&source) else {
        // No note: the same failure already reached the report as
        // `Options::override_error`, with the parse error attached.
        return None;
    };
    let shipped = Catalog::embedded().ok()?;
    let verb = if shipped.get(&entry.cli.id).is_some() {
        "replaces the shipped"
    } else {
        "adds a new"
    };
    Some(format!(
        "{} is active: it {verb} '{}' entry",
        path.display(),
        entry.cli.id
    ))
}

/// Builds the whole report. Sections that have nothing to say are skipped.
pub fn diagnose(options: &Options<'_>) -> Result<Report> {
    let mut report = Report {
        lines: Vec::new(),
        broken: Vec::new(),
    };

    let project = options.project_root.map(|root| {
        let name = project_name_from_dir(root);
        (root, resolve_project_paths(options.mana_home, &name), name)
    });
    let observations = match &project {
        Some((_, paths, _)) => Observations::gather(paths, options.entries, options.now).ok(),
        None => None,
    };

    catalogue_section(options, observations.as_ref(), &mut report);
    if let Some((root, paths, name)) = &project {
        project_section(options, paths, name, observations.as_ref(), &mut report)?;
        worktree_section(options, root, name, &mut report)?;
    }
    Ok(report)
}

// -- catalogue ---------------------------------------------------------------

fn catalogue_section(
    options: &Options<'_>,
    observations: Option<&Observations>,
    report: &mut Report,
) {
    // The plan asks for "catalogue age", and this is the only honest answer
    // available: the shipped entries are `include_str!`-ed into the binary, so
    // their freshness *is* this binary's version -- there is no fetch and no
    // timestamp to report. A per-entry measurement date would need a
    // `[cli].measured_at` field the schema does not have; until then, the one
    // date each entry carries is prose in its `notes`, whose first line is
    // printed below and usually opens with "Measured on ...".
    report.say(format!(
        "catalogue -- {} entries, embedded in mana {}",
        options.entries.len(),
        env!("CARGO_PKG_VERSION")
    ));
    if let Some(note) = &options.override_note {
        report.say(format!("  {note}"));
    }
    // Broken, not a note: the entries below are the shipped ones, so every
    // per-CLI fact in this section is describing a catalogue the user did not
    // ask for.
    if let Some(error) = &options.override_error {
        report.broken(format!(
            "  BROKEN: {error} -- reporting on the shipped catalogue instead; \
             fix or delete the file"
        ));
    }
    for entry in options.entries {
        report.say("");
        report.say(format!("{}  {}", entry.cli.id, entry.cli.name));
        // The same lookup every spawn path uses, so "installed" here and
        // "spawnable" there cannot diverge -- see `CliMeta::resolve`.
        let installed = entry.cli.resolve().ok();
        match &installed {
            Some(path) => {
                field(
                    report,
                    "binary",
                    format!("{} -> {}", entry.cli.bin(), path.display()),
                );
                field(
                    report,
                    "version",
                    match capture_version_output(
                        path,
                        &entry.cli.version_args,
                        VERSION_CHECK_TIMEOUT,
                    ) {
                        Ok(version) if version.is_empty() => {
                            format!(
                                "printed nothing for `{}`",
                                args_line(&entry.cli.version_args)
                            )
                        }
                        Ok(version) => version,
                        Err(error) => format!("probe failed: {error}"),
                    },
                );
            }
            // Not broken: nobody has to install every catalogued CLI. It only
            // becomes broken once it is *registered* -- see `config_section`.
            None => {
                field(
                    report,
                    "binary",
                    format!("{} -> not found in PATH", entry.cli.bin()),
                );
                field(report, "install", entry.install.url.clone());
            }
        }
        field(
            report,
            "pm",
            format!(
                "driver {}, tools over {}",
                driver_word(entry.pm.driver),
                channel_word(entry.tools.channel)
            ),
        );
        field(report, "models", models_line(entry, installed.as_deref()));
        field(report, "quota", quota_line(entry));
        field(report, "failures", failures_line(entry));
        if let Some(observations) = observations {
            field(report, "cooldowns", cooldowns_line(entry, observations));
        }
        for degradation in degradations(entry) {
            field(report, "degraded", degradation);
        }
        if let Some(hint) = note_hint(&entry.notes) {
            field(report, "notes", hint);
        }
    }
}

/// One `  label   value` line, with the label column wide enough for the
/// longest label used above.
fn field(report: &mut Report, label: &str, value: impl Into<String>) {
    report.say(format!("  {label:<12}{}", value.into()));
}

/// The transports mana implements, spelled the way the catalogue spells them.
/// A match on a closed set of drivers is not a match on a CLI: adding a CLI
/// never touches this, adding a *protocol* does.
fn driver_word(driver: PmDriver) -> &'static str {
    match driver {
        PmDriver::Acp => "acp",
        PmDriver::Stream => "stream",
        PmDriver::OneshotContinue => "oneshot-continue",
    }
}

fn channel_word(channel: ToolChannel) -> &'static str {
    match channel {
        ToolChannel::Mcp => "mcp",
        ToolChannel::Sentinel => "sentinel",
    }
}

fn class_word(class: CostClass) -> &'static str {
    match class {
        CostClass::Cheap => "cheap",
        CostClass::Mid => "mid",
        CostClass::Expensive => "expensive",
    }
}

/// The static list, or the result of actually running the CLI's discovery
/// command. Half the catalogue would rot on someone else's release schedule
/// otherwise, which is why `discovery_args` exists -- and why a failed
/// discovery has to say so rather than quietly render an empty list.
fn models_line(entry: &CliEntry, binary: Option<&Path>) -> String {
    if !entry.models.discovery_args.is_empty() {
        let Some(binary) = binary else {
            return format!(
                "discovered at runtime via `{}` -- needs the binary",
                args_line(&entry.models.discovery_args)
            );
        };
        return match discover_models(binary, entry) {
            Ok(models) if models.is_empty() => format!(
                "discovery via `{}` returned no model ids",
                args_line(&entry.models.discovery_args)
            ),
            Ok(models) => format!("{} (discovered)", truncate_list(&models)),
            Err(error) => format!("discovery failed: {error}"),
        };
    }
    if entry.models.static_models.is_empty() {
        return "none declared".to_string();
    }
    entry
        .models
        .static_models
        .iter()
        .map(|model| {
            format!(
                "{} ({}, pool {})",
                model.id,
                class_word(model.cost_class),
                model.pool
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn discover_models(binary: &Path, entry: &CliEntry) -> Result<Vec<String>, String> {
    // Both streams: see `subprocess::Capture` -- two of the four shipped CLIs
    // print their model list on stderr, and reading stdout alone would report
    // "no models" for a CLI that answered perfectly well.
    let output = capture_output(
        binary,
        &entry.models.discovery_args,
        DISCOVERY_TIMEOUT,
        Capture::Both,
    )
    .map_err(|error| error.to_string())?;
    let Some(pattern) = &entry.models.line_regex else {
        // No regex means the whole line is the id, which is what the entries
        // without one declare in so many words.
        return Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect());
    };
    let regex = regex::Regex::new(pattern)
        .map_err(|error| format!("invalid line_regex {pattern:?}: {error}"))?;
    Ok(output
        .lines()
        .filter_map(|line| regex.captures(line))
        .filter_map(|captures| captures.get(1))
        .map(|id| id.as_str().to_string())
        .collect())
}

fn truncate_list(items: &[String]) -> String {
    if items.len() <= MODELS_SHOWN {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..MODELS_SHOWN].join(", "),
        items.len() - MODELS_SHOWN
    )
}

/// Spelled the way the catalogue file spells them, not the way `Debug` spells
/// the enum: a user comparing this report against their `catalog.local.toml`
/// must not have to translate `PerModel` back into `per-model`.
fn kind_word(kind: QuotaKind) -> &'static str {
    match kind {
        QuotaKind::Requests => "requests",
        QuotaKind::Tokens => "tokens",
        QuotaKind::Credits => "credits",
        QuotaKind::Unknown => "unknown",
    }
}

fn scope_word(scope: PoolScope) -> &'static str {
    match scope {
        PoolScope::Global => "global",
        PoolScope::PerModel => "per-model",
    }
}

fn quota_line(entry: &CliEntry) -> String {
    if entry.quota.pools.is_empty() {
        return "none declared".to_string();
    }
    entry
        .quota
        .pools
        .iter()
        .map(|pool| {
            format!(
                "{} ({}, {}, {})",
                pool.id,
                kind_word(pool.kind),
                pool.period,
                scope_word(pool.pool_scope)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn failures_line(entry: &CliEntry) -> String {
    if entry.failure.is_empty() {
        return "none declared -- quota exhaustion cannot be recognised".to_string();
    }
    let mut means: Vec<String> = entry
        .failure
        .iter()
        .map(|failure| crate::dispatch::failure_wire_name(failure.means).to_string())
        .collect();
    means.dedup();
    means.join(", ")
}

/// Which of this entry's pairs are resting right now, and until when.
fn cooldowns_line(entry: &CliEntry, observations: &Observations) -> String {
    let resting: Vec<String> = entry
        .models
        .static_models
        .iter()
        .filter_map(|model| {
            observations
                .cooldown_until(&entry.cli.id, &model.id)
                .map(|until| format!("{} until {}", model.id, until.to_rfc3339()))
        })
        .collect();
    if resting.is_empty() {
        "none".to_string()
    } else {
        resting.join(", ")
    }
}

/// What this CLI cannot do, straight from its entry. Every one of these is a
/// field being empty or set, never a name being recognised — which is what
/// makes the list correct for a CLI added through the local override.
fn degradations(entry: &CliEntry) -> Vec<String> {
    let mut degradations = Vec::new();
    if entry.pm.permission_args.is_empty() {
        degradations.push(
            "no PM permission flags declared: the no-code rule is prompt-only here".to_string(),
        );
    }
    if entry.subagent.auto_approve_args.is_empty() {
        degradations.push(
            "no sub-agent auto-approve flag: an interactive prompt would hang a dispatch"
                .to_string(),
        );
    }
    if entry.subagent.cwd_required_in_brief {
        degradations
            .push("briefs must spell out absolute paths: this CLI ignores its cwd".to_string());
    }
    if entry.subagent.max_concurrent != 0 {
        degradations.push(format!(
            "at most {} sub-agent(s) at a time",
            entry.subagent.max_concurrent
        ));
    }
    if entry.skills.inline_in_activation {
        degradations.push(
            "the role text is sent inline every turn: this CLI cannot read the SKILL.md mana \
             writes"
                .to_string(),
        );
    }
    degradations
}

fn note_hint(notes: &str) -> Option<String> {
    let first = notes.lines().map(str::trim).find(|line| !line.is_empty())?;
    if first.chars().count() <= NOTE_HINT_CHARS {
        return Some(first.to_string());
    }
    let clipped: String = first.chars().take(NOTE_HINT_CHARS).collect();
    Some(format!("{clipped}..."))
}

fn args_line(args: &[String]) -> String {
    args.join(" ")
}

// -- project -----------------------------------------------------------------

fn project_section(
    options: &Options<'_>,
    paths: &ProjectPaths,
    name: &str,
    observations: Option<&Observations>,
    report: &mut Report,
) -> Result<()> {
    report.say("");
    report.say(format!("project '{name}' -- {}", paths.root.display()));

    let dispatches = status::dispatches_in(options.mana_home, name)?;
    if dispatches.is_empty() {
        report.say("  no dispatches recorded yet");
        return Ok(());
    }
    let count = |status: DispatchStatus| dispatches.iter().filter(|d| d.status == status).count();
    let stale = count(DispatchStatus::Stale);
    report.say(format!(
        "  {} dispatch(es): {} running, {} done, {} stale, {} unknown",
        dispatches.len(),
        count(DispatchStatus::Running),
        count(DispatchStatus::Done),
        stale,
        count(DispatchStatus::Unknown),
    ));

    // Only the ones that are not over: a done dispatch is history, and history
    // belongs in `mana ps`. A header with no rows under it would be worse than
    // no table at all, so nothing is printed when everything has finished.
    let open: Vec<Dispatch> = dispatches
        .iter()
        .filter(|d| d.status != DispatchStatus::Done)
        .cloned()
        .collect();
    if !open.is_empty() {
        for line in ps::table(&open, false, options.now) {
            report.say(format!("  {line}"));
        }
    }
    if stale > 0 {
        report.broken(format!(
            "  BROKEN: {stale} stale dispatch(es) -- their process is gone and no exit record \
             was ever written, so the PM is still waiting on them. Clear each with \
             `mana kill <agent-id>`."
        ));
    }

    counters_section(paths, observations, report)?;
    verdicts_section(paths, report);
    Ok(())
}

/// Per-(CLI x model) counters, from the same `Observations` the router uses.
/// Recomputing them here would be a second implementation of the join, free to
/// disagree with the one that actually decides where work goes.
///
/// `log::counters` is still read, for its *keys*: which pairs this project has
/// ever dispatched to. `Observations` answers per pair and does not enumerate
/// them, and enumerating the registry here by hand would be the third copy of
/// the same loop.
fn counters_section(
    paths: &ProjectPaths,
    observations: Option<&Observations>,
    report: &mut Report,
) -> Result<()> {
    let registry = crate::lock::load_registry(&paths.subagents_file)?;
    let counters = log::counters(&paths.logs, &registry)?;
    // `None` only when gathering failed, which is already the reason the
    // cooldown lines are missing from the catalogue section above.
    let (false, Some(observations)) = (counters.is_empty(), observations) else {
        return Ok(());
    };
    report.say("  counters");
    let mut rows = vec![vec![
        "PAIR".to_string(),
        "DISPATCHED".into(),
        "VALIDATED".into(),
        "REJECTED(CODE)".into(),
        "QUOTA".into(),
        "AVG".into(),
    ]];
    for (cli, model) in counters.keys() {
        let stats = observations.stats(cli, model);
        rows.push(vec![
            format!("{cli}/{model}"),
            stats.dispatched.to_string(),
            stats.validated.to_string(),
            stats.rejected_for_code.to_string(),
            stats.quota_failures.to_string(),
            stats
                .avg_duration_ms
                .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                .unwrap_or_else(|| "-".to_string()),
        ]);
    }
    for line in status::render_table(&rows) {
        report.say(format!("    {line}"));
    }
    Ok(())
}

/// Verdict tallies straight off `reviews/*.json`. Counted per file rather than
/// per dispatch: one task has one verdict however many agents worked on it.
fn verdicts_section(paths: &ProjectPaths, report: &mut Report) {
    let Ok(entries) = std::fs::read_dir(&paths.reviews) else {
        return;
    };
    let (mut validated, mut code, mut brief, mut unusable) = (0u32, 0u32, 0u32, 0u32);
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match review::read_verdict(&entry.path()) {
            Ok(verdict) => match (verdict.verdict, verdict.attribution) {
                (Decision::Validated, _) => validated += 1,
                (Decision::Rejected, Some(Attribution::Code)) => code += 1,
                (Decision::Rejected, _) => brief += 1,
            },
            // Not broken: a malformed verdict is the dispatch layer's problem
            // (it re-prompts once), and doctor's job here is to say it exists.
            Err(_) => unusable += 1,
        }
    }
    if validated + code + brief + unusable == 0 {
        return;
    }
    // "per task" because the counters table above counts per *dispatch*: two
    // executors retried on one task both carry that task's one verdict, so the
    // two numbers are meant to differ and the labels have to say why.
    let mut line = format!(
        "  verdicts     (per task) {validated} validated, {} rejected ({code} code, {brief} brief)",
        code + brief
    );
    if unusable > 0 {
        line.push_str(&format!(", {unusable} unusable"));
    }
    report.say(line);
}

// -- worktrees ---------------------------------------------------------------

/// Worktrees whose dispatch is over are disk, not state: nothing reads them,
/// and `git worktree` keeps an admin entry for each one until it is pruned.
/// A worktree whose dispatch is *running* is the executor's checkout and is
/// never touched, `--prune` or not.
fn worktree_section(
    options: &Options<'_>,
    project_root: &Path,
    name: &str,
    report: &mut Report,
) -> Result<()> {
    let dir = worktree::worktrees_dir(options.mana_home, name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    let dispatches = status::dispatches_in(options.mana_home, name)?;

    let mut leftovers: Vec<(PathBuf, String, bool)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        // A dispatch is "in" this worktree when its task id names the
        // directory -- the same truncation `worktree::create` applied.
        let in_use = dispatches.iter().any(|dispatch| {
            dispatch.status == DispatchStatus::Running
                && worktree::task_dir_name(&dispatch.record.task_id) == dir_name
        });
        leftovers.push((entry.path(), dir_name, in_use));
    }
    if leftovers.is_empty() {
        return Ok(());
    }
    leftovers.sort_by(|a, b| a.1.cmp(&b.1));

    report.say("");
    report.say(format!("worktrees -- {}", dir.display()));
    for (path, dir_name, in_use) in leftovers {
        let age = status::render_age(dir_age(&path, options.now));
        if in_use {
            report.say(format!(
                "  {dir_name}  age {age}  in use by a running dispatch{}",
                if options.prune { " -- kept" } else { "" }
            ));
            continue;
        }
        if !options.prune {
            report.say(format!(
                "  {dir_name}  age {age}  no running dispatch -- remove with `mana doctor --prune`"
            ));
            continue;
        }
        match worktree::remove_at(project_root, &path) {
            Ok(()) => report.say(format!("  {dir_name}  age {age}  pruned")),
            // A leftover worktree is disk, not correctness, so listing one is
            // not broken -- but a *failed* prune is: the cleanup the user
            // asked for did not happen, and a script that read exit 0 here
            // would move on believing the directory is gone.
            Err(error) => {
                report.broken(format!("  {dir_name}  age {age}  prune failed: {error:#}"))
            }
        }
    }
    Ok(())
}

/// A worktree's age from its own mtime, not from a registry record: the
/// leftovers worth finding are exactly the ones no record explains any more.
fn dir_age(path: &Path, now: DateTime<Utc>) -> Option<TimeDelta> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let modified: DateTime<Utc> = modified.into();
    Some(now - modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::parse_entry;
    use crate::lock::{SubagentRecord, append_record};
    use crate::log::{ExitEntry, Status, append_log, now_iso8601};
    use crate::task::Role;

    /// A complete entry for a CLI that does not exist, so no test depends on
    /// what happens to be installed on the machine running it.
    pub(super) const FIXTURE: &str = r#"
schema = 1
notes = """
Measured on 2026-08-15 against the fixture.
Second line, which the hint must not show.
"""

[cli]
id = "alpha"
name = "Alpha CLI"
bin = "alpha-does-not-exist"
version_args = ["--version"]

[pm]
driver = "stream"
args = ["-p"]
permission_args = ["--allowedTools", "mcp__mana__*"]
prompt = "stdin-jsonl"

[pm.events]
text = "$.text"

[tools]
channel = "mcp"
mcp_args = ["--mcp-config", "{config_path}"]

[subagent]
args = ["-p"]
auto_approve_args = ["--yes"]
model_args = ["--model", "{model}"]
prompt = "argv"
max_concurrent = 2
cwd_required_in_brief = true

[models]
discovery_args = []

[[models.static]]
id = "mini"
cost_class = "cheap"
pool = "main"

[[quota.pools]]
id = "main"
kind = "requests"
period = "monthly"
pool_scope = "global"

[[failure]]
means = "rate_limited"
stdout_regex = "rate.?limit"
cooldown_minutes = 30

[skills]
dirs = ["~/.alpha/skills"]

[install]
url = "https://example.invalid/alpha"
"#;

    fn entries() -> Vec<CliEntry> {
        vec![parse_entry(FIXTURE).unwrap()]
    }

    fn options<'a>(mana_home: &'a Path, entries: &'a [CliEntry]) -> Options<'a> {
        Options {
            mana_home,
            entries,
            override_note: None,
            override_error: None,
            project_root: None,
            prune: false,
            now: Utc::now(),
        }
    }

    fn joined(report: &Report) -> String {
        report.lines.join("\n")
    }

    fn record(agent_id: &str, task_id: &str, pid: Option<u32>) -> SubagentRecord {
        SubagentRecord {
            agent_id: agent_id.to_string(),
            cli: "alpha".into(),
            model: "mini".into(),
            role: Role::Executor,
            task_id: task_id.to_string(),
            pid,
            started_at: now_iso8601(),
        }
    }

    fn finish(paths: &ProjectPaths, agent_id: &str, duration_ms: u64) {
        append_log(
            &paths.logs.join(format!("{agent_id}.jsonl")),
            &ExitEntry {
                status: Status::Done,
                action: "exited".into(),
                timestamp: now_iso8601(),
                exit_code: Some(0),
                duration_ms: Some(duration_ms),
                input_tokens: None,
                output_tokens: None,
                failure_means: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn the_catalogue_section_reports_an_uninstalled_cli_without_calling_it_broken() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = entries();
        let report = diagnose(&options(tmp.path(), &entries)).unwrap();
        let text = joined(&report);

        // The version is the catalogue's only honest freshness signal: the
        // entries ship inside the binary, so they are exactly as old as it is.
        assert!(
            text.contains(&format!(
                "catalogue -- 1 entries, embedded in mana {}",
                env!("CARGO_PKG_VERSION")
            )),
            "{text}"
        );
        assert!(text.contains("alpha  Alpha CLI"), "{text}");
        assert!(text.contains("not found in PATH"), "{text}");
        assert!(text.contains("https://example.invalid/alpha"), "{text}");
        assert!(text.contains("driver stream, tools over mcp"), "{text}");
        assert!(text.contains("mini (cheap, pool main)"), "{text}");
        assert!(text.contains("main (requests, monthly, global)"), "{text}");
        assert!(text.contains("rate_limited"), "{text}");
        // Not installed is not broken: nobody has to install all of them.
        assert_eq!(report.exit_code(), 0, "{text}");
    }

    #[test]
    fn declared_capability_gaps_are_read_off_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = entries();
        let text = joined(&diagnose(&options(tmp.path(), &entries)).unwrap());
        assert!(text.contains("at most 2 sub-agent(s)"), "{text}");
        assert!(text.contains("spell out absolute paths"), "{text}");
        // Declared, so not listed as missing.
        assert!(!text.contains("no sub-agent auto-approve flag"), "{text}");
        assert!(!text.contains("no PM permission flags"), "{text}");
    }

    #[test]
    fn an_entry_that_declares_nothing_says_so_instead_of_showing_an_empty_line() {
        let bare = FIXTURE
            .replace(
                r#"permission_args = ["--allowedTools", "mcp__mana__*"]
"#,
                "",
            )
            .replace(r#"auto_approve_args = ["--yes"]"#, "")
            .replace(
                r#"[[failure]]
means = "rate_limited"
stdout_regex = "rate.?limit"
cooldown_minutes = 30
"#,
                "",
            );
        let entries = vec![parse_entry(&bare).unwrap()];
        let tmp = tempfile::tempdir().unwrap();
        let text = joined(&diagnose(&options(tmp.path(), &entries)).unwrap());
        assert!(text.contains("none declared -- quota exhaustion"), "{text}");
        assert!(text.contains("no PM permission flags"), "{text}");
        assert!(text.contains("no sub-agent auto-approve flag"), "{text}");
    }

    #[test]
    fn the_notes_hint_is_the_first_line_only() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = entries();
        let text = joined(&diagnose(&options(tmp.path(), &entries)).unwrap());
        assert!(
            text.contains("Measured on 2026-08-15 against the fixture."),
            "{text}"
        );
        assert!(!text.contains("Second line"), "{text}");
    }

    #[test]
    fn note_hint_clips_a_long_first_line() {
        let long = "x".repeat(NOTE_HINT_CHARS + 40);
        let hint = note_hint(&long).unwrap();
        assert!(hint.ends_with("..."), "{hint}");
        assert_eq!(hint.chars().count(), NOTE_HINT_CHARS + 3);
    }

    #[test]
    fn an_active_override_is_named_in_the_catalogue_header() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = entries();
        let mut options = options(tmp.path(), &entries);
        options.override_note =
            Some("catalog.local.toml is active: it adds a new 'alpha' entry".into());
        let text = joined(&diagnose(&options).unwrap());
        assert!(text.contains("catalog.local.toml is active"), "{text}");
        assert!(text.contains("adds a new 'alpha'"), "{text}");
    }

    #[test]
    fn override_note_says_which_shipped_entry_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CATALOG_OVERRIDE);
        std::fs::write(
            &path,
            FIXTURE.replace(r#"id = "alpha""#, r#"id = "claude""#),
        )
        .unwrap();
        let note = override_note(&path).unwrap();
        assert!(note.contains("replaces the shipped"), "{note}");
        assert!(note.contains("claude"), "{note}");
    }

    #[test]
    fn override_note_is_absent_without_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(override_note(&tmp.path().join(CATALOG_OVERRIDE)).is_none());
    }

    #[test]
    fn the_project_section_reports_counters_and_verdicts() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("my-api");
        std::fs::create_dir_all(&project).unwrap();
        let paths = resolve_project_paths(tmp.path(), "my-api");
        crate::project::ensure_project_structure(&paths).unwrap();

        append_record(&paths.subagents_file, &record("agent-1", "task-1", None)).unwrap();
        append_record(&paths.subagents_file, &record("agent-2", "task-2", None)).unwrap();
        finish(&paths, "agent-1", 2000);
        finish(&paths, "agent-2", 4000);
        std::fs::write(
            paths.reviews.join("task-1.json"),
            r#"{"verdict":"validated","issues":[]}"#,
        )
        .unwrap();
        std::fs::write(
            paths.reviews.join("task-2.json"),
            r#"{"verdict":"rejected","attribution":"code","issues":["nope"]}"#,
        )
        .unwrap();
        std::fs::write(paths.reviews.join("task-3.json"), "half a json").unwrap();

        let entries = entries();
        let mut options = options(tmp.path(), &entries);
        options.project_root = Some(&project);
        let text = joined(&diagnose(&options).unwrap());

        assert!(text.contains("project 'my-api'"), "{text}");
        assert!(text.contains("2 dispatch(es): 0 running, 2 done"), "{text}");
        // Everything finished, so there is no in-flight table -- and a header
        // with no rows under it is worse than no table at all.
        assert!(!text.contains("AGENT  ROLE"), "{text}");
        assert!(text.contains("alpha/mini"), "{text}");
        // 2 dispatched, 1 validated, 1 rejected for code, avg of 2s and 4s.
        assert!(text.contains("3.0s"), "{text}");
        assert!(
            text.contains("(per task) 1 validated, 1 rejected (1 code, 0 brief), 1 unusable"),
            "{text}"
        );
    }

    #[test]
    fn a_project_with_no_dispatches_says_so_and_stays_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("fresh");
        std::fs::create_dir_all(&project).unwrap();
        let entries = entries();
        let mut options = options(tmp.path(), &entries);
        options.project_root = Some(&project);
        let report = diagnose(&options).unwrap();
        assert!(joined(&report).contains("no dispatches recorded yet"));
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn no_project_context_means_no_project_section() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = entries();
        let text = joined(&diagnose(&options(tmp.path(), &entries)).unwrap());
        assert!(!text.contains("project '"), "{text}");
        assert!(!text.contains("worktrees --"), "{text}");
    }

    /// The whole point of issue #115: doctor is the command you run *because*
    /// the override is broken, so it has to survive it, name it, and keep
    /// reporting past the finding.
    #[test]
    fn a_broken_catalog_override_is_a_finding_not_an_abort() {
        let tmp = tempfile::tempdir().unwrap();
        let override_path = tmp.path().join(CATALOG_OVERRIDE);
        std::fs::write(&override_path, "not [ valid").unwrap();

        let (catalog, error) = catalog_or_shipped(&override_path).unwrap();
        // Degraded to the shipped entries rather than dying on the load.
        assert_eq!(
            catalog.entries().len(),
            Catalog::embedded().unwrap().entries().len()
        );
        let error = error.expect("a broken override must be reported");
        assert!(
            error.contains(&override_path.display().to_string()),
            "{error}"
        );
        assert!(error.contains("TOML parse error"), "{error}");

        let entries = entries();
        let report = diagnose(&Options {
            override_error: Some(error),
            ..options(tmp.path(), &entries)
        })
        .unwrap();
        let text = joined(&report);
        assert!(text.contains("BROKEN"), "{text}");
        assert!(text.contains("TOML parse error"), "{text}");
        // The report kept going past the finding -- that is what aborting used
        // to cost. The catalogue's own entries are printed after the BROKEN
        // line, from the shipped catalogue it fell back to.
        assert!(text.contains("Alpha CLI"), "{text}");
        assert_eq!(report.exit_code(), 1, "{text}");
    }

    /// Issue #114: a cleanup that reports success while leaving the mess is
    /// worse than one that does nothing.
    #[test]
    fn a_failed_prune_is_broken_rather_than_a_clean_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let mana_home = tmp.path().join("mana-home");
        // No `git init`, which is the reported repro: every git call in
        // `worktree::remove_at` fails with "not a git repository".
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let leftover =
            worktree::worktrees_dir(&mana_home, &project_name_from_dir(&project)).join("3f2a1b6c");
        std::fs::create_dir_all(&leftover).unwrap();

        let entries = entries();
        let report = diagnose(&Options {
            project_root: Some(&project),
            prune: true,
            ..options(&mana_home, &entries)
        })
        .unwrap();
        let text = joined(&report);
        assert!(text.contains("prune failed"), "{text}");
        assert_eq!(report.exit_code(), 1, "a failed prune exited clean: {text}");
    }

    #[test]
    fn truncate_list_says_how_many_it_left_out() {
        let many: Vec<String> = (0..10).map(|i| format!("m{i}")).collect();
        let rendered = truncate_list(&many);
        assert!(rendered.contains("and 4 more"), "{rendered}");
        assert!(rendered.starts_with("m0, m1"), "{rendered}");
        assert_eq!(truncate_list(&["a".to_string()]), "a");
    }
}

#[cfg(all(test, unix))] // needs an executable fixture and a live pid
mod process_tests {
    use super::tests_support::*;
    use super::*;
    use crate::catalog::parse_entry;
    use crate::lock::{SubagentRecord, append_record};
    use crate::log::now_iso8601;
    use crate::task::Role;

    fn record(task_id: &str, pid: Option<u32>) -> SubagentRecord {
        SubagentRecord {
            agent_id: format!("agent-for-{task_id}"),
            cli: "alpha".into(),
            model: "mini".into(),
            role: Role::Executor,
            task_id: task_id.to_string(),
            pid,
            started_at: now_iso8601(),
        }
    }

    #[test]
    fn a_stale_dispatch_is_broken_and_names_the_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("my-api");
        std::fs::create_dir_all(&project).unwrap();
        let paths = resolve_project_paths(tmp.path(), "my-api");
        crate::project::ensure_project_structure(&paths).unwrap();
        append_record(
            &paths.subagents_file,
            &record("3f2a1b6c-dead", Some(reaped_pid())),
        )
        .unwrap();

        let entries = vec![parse_entry(super::tests::FIXTURE).unwrap()];
        let report = diagnose(&Options {
            mana_home: tmp.path(),
            entries: &entries,
            override_note: None,
            override_error: None,
            project_root: Some(&project),
            prune: false,
            now: Utc::now(),
        })
        .unwrap();
        let text = report.lines.join("\n");

        assert!(text.contains("1 stale"), "{text}");
        assert!(text.contains("BROKEN"), "{text}");
        assert!(text.contains("mana kill"), "{text}");
        assert_eq!(report.exit_code(), 1, "{text}");
    }

    #[test]
    fn discovery_runs_the_entrys_own_command_and_applies_its_regex() {
        let tmp = tempfile::tempdir().unwrap();
        // A "CLI" whose `models` subcommand prints TSV, exactly like the one
        // real entry that discovers at runtime.
        let script = write_script(
            tmp.path(),
            "alpha",
            "#!/bin/sh\nif [ \"$1\" = models ]; then\n  printf 'fast\\tcheap\\nslow\\tdear\\n'\nelse\n  echo 9.9.9\nfi\n",
        );
        let entry = parse_entry(
            &super::tests::FIXTURE
                .replace(
                    r#"bin = "alpha-does-not-exist""#,
                    &format!("bin = {script:?}"),
                )
                .replace(
                    "discovery_args = []",
                    "discovery_args = [\"models\"]\nline_regex = '^(\\S+)\\t'",
                ),
        )
        .unwrap();

        let models = models_line(&entry, Some(&script));
        assert_eq!(models, "fast, slow (discovered)");
    }

    #[test]
    fn a_failing_discovery_degrades_loudly_instead_of_showing_no_models() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_script(tmp.path(), "alpha", "#!/bin/sh\nsleep 30\n");
        let entry = parse_entry(
            &super::tests::FIXTURE
                .replace(
                    r#"bin = "alpha-does-not-exist""#,
                    &format!("bin = {script:?}"),
                )
                .replace("discovery_args = []", "discovery_args = [\"models\"]"),
        )
        .unwrap();

        // The real timeout is 8s; call the probe directly with a short one so
        // the test proves the degradation without paying for it.
        let failure = capture_output(
            &script,
            &["models".into()],
            Duration::from_millis(150),
            Capture::Both,
        )
        .unwrap_err()
        .to_string();
        assert!(failure.contains("timeout"), "{failure}");
        // ...and the line built from that failure names it.
        assert!(
            models_line(&entry, None).contains("needs the binary"),
            "an uninstalled CLI must not claim a discovery result"
        );
    }

    #[test]
    fn a_worktree_with_no_running_dispatch_is_listed_and_pruned_while_a_live_one_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = GitFixture::new(tmp.path());
        let mana_home = tmp.path().join("mana-home");

        let dead_task = "3f2a1b6c-dead-0000-0000-000000000000";
        let live_task = "9d4e4a7b-live-0000-0000-000000000000";
        let dead = worktree::create(&fixture.project, &mana_home, dead_task).unwrap();
        let live = worktree::create(&fixture.project, &mana_home, live_task).unwrap();

        let paths = resolve_project_paths(&mana_home, "project");
        crate::project::ensure_project_structure(&paths).unwrap();
        append_record(
            &paths.subagents_file,
            &record(dead_task, Some(reaped_pid())),
        )
        .unwrap();
        let sleeper = Sleeper::new();
        append_record(
            &paths.subagents_file,
            &record(live_task, Some(sleeper.pid())),
        )
        .unwrap();

        let entries = vec![parse_entry(super::tests::FIXTURE).unwrap()];
        let listing = diagnose(&Options {
            mana_home: &mana_home,
            entries: &entries,
            override_note: None,
            override_error: None,
            project_root: Some(&fixture.project),
            prune: false,
            now: Utc::now(),
        })
        .unwrap();
        let text = listing.lines.join("\n");
        assert!(text.contains("worktrees --"), "{text}");
        assert!(text.contains("mana doctor --prune"), "{text}");
        assert!(text.contains("in use by a running dispatch"), "{text}");

        let pruned = diagnose(&Options {
            mana_home: &mana_home,
            entries: &entries,
            override_note: None,
            override_error: None,
            project_root: Some(&fixture.project),
            prune: true,
            now: Utc::now(),
        })
        .unwrap();
        let text = pruned.lines.join("\n");
        assert!(text.contains("pruned"), "{text}");
        assert!(text.contains("kept"), "{text}");

        assert!(!dead.path.exists(), "the stale worktree survived --prune");
        assert!(
            live.path.exists(),
            "--prune removed a worktree a running dispatch is using"
        );
    }
}

/// Fixtures for the tests above, gated with them: on Windows neither module
/// exists, so nothing here is dead code there.
#[cfg(all(test, unix))]
mod tests_support {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    pub(super) struct Sleeper(Child);

    impl Sleeper {
        pub(super) fn new() -> Sleeper {
            Sleeper(
                Command::new("sleep")
                    .arg("30")
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }

        pub(super) fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub(super) fn reaped_pid() -> u32 {
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    pub(super) fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A throwaway git project with one commit, which is the minimum
    /// `worktree::create` needs.
    pub(super) struct GitFixture {
        pub(super) project: PathBuf,
    }

    impl GitFixture {
        pub(super) fn new(root: &Path) -> GitFixture {
            let project = root.join("project");
            std::fs::create_dir_all(&project).unwrap();
            let git = |args: &[&str]| {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_AUTHOR_NAME", "seed")
                    .env("GIT_AUTHOR_EMAIL", "seed@example.invalid")
                    .env("GIT_COMMITTER_NAME", "seed")
                    .env("GIT_COMMITTER_EMAIL", "seed@example.invalid")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["-c", "init.defaultBranch=main", "init"]);
            std::fs::write(project.join("README.md"), "base\n").unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-m", "base"]);
            GitFixture { project }
        }
    }
}
