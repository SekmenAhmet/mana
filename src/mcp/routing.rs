//! Which (CLI × model) a dispatch actually runs on.
//!
//! The PM says how hard a task is (a cost class); this module says which
//! concrete pair pays for it. Design §8 is the whole specification: cheapest
//! quota first, no synthesized quality score, and the choice derived from what
//! mana already wrote down -- `subagents.jsonl`, `logs/*.jsonl`, `reviews/*.json`.
//! There is no router state file, because a second store of the same facts is
//! a second thing that can be wrong.
//!
//! Determinism is a requirement, not a nicety: the PM cannot debug a dispatch
//! it cannot predict, and a test cannot assert on a tie broken by hash order.
//! Every ordering below ends in an alphabetical tiebreak for that reason.

use crate::catalog::{CliEntry, CostClass, FailureMeans, PoolScope};
use crate::dispatch::failure_wire_name;
use crate::lock::load_registry;
use crate::log;
use crate::project::ProjectPaths;
use crate::review::{self, Attribution, Decision, Verdict};
use crate::task::Role;
use anyhow::{Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
// Through the SDK rather than as a direct dependency: `schemars` is rmcp's
// schema vocabulary, and pinning our own copy of it would make a version bump
// there produce two incompatible `JsonSchema` traits.
use rmcp::schemars;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

/// Applied when a failure signature matched but declared no
/// `cooldown_minutes`, and when the log names a CLI the catalogue no longer
/// does (a local override edited between the run and now). An hour is the
/// period the only measured entry uses; guessing shorter would put a dead pool
/// back in rotation, guessing longer would idle quota that recovered.
const DEFAULT_COOLDOWN_MINUTES: i64 = 60;

/// The failure meanings a cooldown answers. `auth_expired` is deliberately not
/// one of them: waiting does not log a user back in, and resting the pool
/// would only hide the credential problem behind an unexplained delay.
const COOLING_MEANS: [FailureMeans; 2] = [FailureMeans::QuotaExhausted, FailureMeans::RateLimited];

/// What this (CLI x model) pair has actually done here so far.
//
// Doc comments on this type reach the PM through `list_agents`'s output
// schema, so maintainer notes go in `//` comments like this one. Counted and
// never scored (design §8): with dozens of cells and few tasks a score cannot
// stay calibrated, but "4 of 6" lets the PM discount a small sample itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct Stats {
    pub dispatched: u32,
    pub validated: u32,
    /// Rejections blamed on the implementation. A rejection blamed on the
    /// brief is not counted here -- it was not this model's failure.
    //
    // Design §8, the agy-cwd incident: a bad brief must not condemn a good
    // model, which is the whole reason the verdict carries an attribution.
    pub rejected_for_code: u32,
    pub quota_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_duration_ms: Option<u64>,
}

/// Everything the router reads off disk, gathered once per tool call.
///
/// Held by value rather than cached on the server: an MCP call is rare and the
/// files are small, whereas a cache would have to be invalidated by the
/// background dispatch threads that write those very files.
#[derive(Debug, Default)]
pub struct Observations {
    stats: BTreeMap<(String, String), Stats>,
    cooldowns: BTreeMap<(String, String), DateTime<Utc>>,
}

impl Observations {
    /// Joins the dispatch registry with the per-agent logs and the reviewers'
    /// verdicts. `now` is a parameter so cooldown expiry is testable without
    /// sleeping.
    pub fn gather(paths: &ProjectPaths, entries: &[CliEntry], now: DateTime<Utc>) -> Result<Self> {
        let registry = load_registry(&paths.subagents_file)?;

        // `log::counters` already owns dispatched/quota_failures/duration;
        // duplicating that join here would be a second implementation of the
        // same arithmetic, free to disagree with the one the TUI will read.
        let mut stats: BTreeMap<(String, String), Stats> = log::counters(&paths.logs, &registry)?
            .into_iter()
            .map(|(key, counts)| {
                (
                    key,
                    Stats {
                        dispatched: counts.dispatched,
                        quota_failures: counts.quota_failures,
                        avg_duration_ms: counts.avg_duration_ms,
                        ..Stats::default()
                    },
                )
            })
            .collect();

        // One verdict file per task, read once however many agents worked on
        // it. `None` covers both "no reviewer ran yet" and "the verdict is
        // unusable" -- neither is a routing signal, and neither is an error
        // here: `get_review` is where a malformed verdict has to be loud.
        let mut verdicts: BTreeMap<String, Option<Verdict>> = BTreeMap::new();
        let mut cooldowns: BTreeMap<(String, String), DateTime<Utc>> = BTreeMap::new();

        for record in &registry.records {
            let key = (record.cli.clone(), record.model.clone());

            // Credited to the executor only: the reviewer's own (CLI × model)
            // did not write the code the verdict is about.
            if record.role == Role::Executor {
                let verdict = verdicts.entry(record.task_id.clone()).or_insert_with(|| {
                    review::read_verdict(&paths.reviews.join(format!("{}.json", record.task_id)))
                        .ok()
                });
                if let Some(verdict) = verdict {
                    let stats = stats.entry(key.clone()).or_default();
                    match (verdict.verdict, verdict.attribution) {
                        (Decision::Validated, _) => stats.validated += 1,
                        (Decision::Rejected, Some(Attribution::Code)) => {
                            stats.rejected_for_code += 1
                        }
                        // A rejection blamed on the brief is recorded nowhere:
                        // it is the PM's failure, not the model's.
                        (Decision::Rejected, _) => {}
                    }
                }
            }

            let log_path = paths.logs.join(format!("{}.jsonl", record.agent_id));
            let Some(exit) = log::read_last_exit(&log_path)? else {
                continue;
            };
            // `failure_means` is only ever written for a failure, and every
            // name in it came from `failure_wire_name` -- so a name this build
            // cannot map is a record from another mana (or a catalogue edited
            // since), not a clean exit. It rests the pool for the default hour:
            // the wrong guess costs a wait, whereas letting it through costs a
            // real paid dispatch into a quota the log says is gone. Only
            // `auth_expired` is deliberately let through, because waiting never
            // logs anyone back in.
            let Some(wire) = exit.failure_means.as_deref() else {
                continue;
            };
            if wire == failure_wire_name(FailureMeans::AuthExpired) {
                continue;
            }
            let means = cooling_means(wire);

            // A cooldown mana cannot date is still a cooldown. Dropping it is
            // the failure mode that spends money, so the fallback walks to the
            // next timestamp mana wrote itself -- the dispatch's own start,
            // minutes before the failure at worst -- and only then to `now`.
            // `now` is the one that cannot expire while the bad line is still
            // the last exit; the PM can still name that pair explicitly, which
            // bypasses cooldowns by design.
            let failed_at = utc(&exit.timestamp)
                .or_else(|| utc(&record.started_at))
                .unwrap_or(now);

            let entry = entries.iter().find(|entry| entry.cli.id == record.cli);
            let minutes = match (entry, means) {
                (Some(entry), Some(means)) => cooldown_minutes(entry, means),
                _ => DEFAULT_COOLDOWN_MINUTES,
            };
            let until = failed_at + TimeDelta::minutes(minutes);
            if until <= now {
                continue;
            }
            for model in entry.map_or_else(
                || vec![record.model.clone()],
                |entry| cooled_models(entry, &record.model),
            ) {
                let slot = cooldowns
                    .entry((record.cli.clone(), model))
                    .or_insert(until);
                // The longest live cooldown wins: two failures on the same pool
                // must not shorten each other.
                *slot = (*slot).max(until);
            }
        }

        Ok(Observations { stats, cooldowns })
    }

    /// A pair mana has never dispatched reads as all-zeros rather than as
    /// absent -- "never tried" is a legitimate routing state, and every
    /// comparison below would otherwise need its own `Option` branch.
    pub fn stats(&self, cli: &str, model: &str) -> Stats {
        self.stats
            .get(&(cli.to_string(), model.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    pub fn cooldown_until(&self, cli: &str, model: &str) -> Option<DateTime<Utc>> {
        self.cooldowns
            .get(&(cli.to_string(), model.to_string()))
            .copied()
    }
}

/// One routable pair. Deliberately not carrying a cost class: the class is how
/// a candidate was *found*, and a model discovered at runtime has none at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub cli: String,
    pub model: String,
}

/// What the PM asked for, after the tool layer has parsed it. All three are
/// optional because the PM is expected to name none of them and let mana route.
#[derive(Debug, Default, Clone, Copy)]
pub struct Request<'a> {
    pub cli: Option<&'a str>,
    pub model: Option<&'a str>,
    pub cost_class: Option<CostClass>,
}

/// The one entry point: turns a PM request into the pair that will be spawned.
///
/// Every failure is worded for the PM, not for a maintainer, because the PM is
/// the only caller and its next move is to call again with different
/// arguments -- so each message names what *would* have worked.
pub fn resolve(
    entries: &[CliEntry],
    installed: &BTreeSet<String>,
    observations: &Observations,
    request: &Request<'_>,
) -> Result<Candidate> {
    // An explicitly named model wins outright, cooldown included: the PM asked
    // for that pair on purpose, and silently routing elsewhere would make the
    // parameter a suggestion.
    if let Some(model) = request.model {
        return resolve_named(entries, installed, request.cli, model);
    }

    let wanted = request.cost_class.unwrap_or(CostClass::Cheap);
    let pool = installed_entries(entries, installed, request.cli)?;
    let candidates: Vec<Candidate> = pool
        .iter()
        .flat_map(|entry| {
            entry
                .models
                .static_models
                .iter()
                .filter(|model| model.cost_class == wanted)
                .map(|model| Candidate {
                    cli: entry.cli.id.clone(),
                    model: model.id.clone(),
                })
        })
        .collect();

    if candidates.is_empty() {
        bail!(
            "no installed CLI declares a {} model. Available: {}",
            wanted.word(),
            render_classes(&pool)
        );
    }

    let mut available: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| {
            observations
                .cooldown_until(&candidate.cli, &candidate.model)
                .is_none()
        })
        .cloned()
        .collect();
    if available.is_empty() {
        // No automatic escalation to a pricier class: that would spend
        // expensive quota on a decision the PM never made. It gets told what is
        // resting and until when, and can ask for a higher class itself.
        bail!(
            "every {} model is resting on a quota cooldown: {}. Retry later, or \
             ask for a higher cost_class.",
            wanted.word(),
            candidates
                .iter()
                .map(|candidate| {
                    let until = observations
                        .cooldown_until(&candidate.cli, &candidate.model)
                        .map(|until| until.to_rfc3339())
                        .unwrap_or_default();
                    format!("{}/{} until {until}", candidate.cli, candidate.model)
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // The ordering, and the whole reason this function is not a `.first()`:
    // most proven first, then least burnt, then the catalogue's own editorial
    // preference, then alphabetical so two equal candidates always resolve the
    // same way.
    //
    // The weight sits third rather than first because a maintainer's belief
    // must yield to what this project has actually seen. It sits above the
    // alphabetical tiebreak because on a fresh project the two counters are
    // equal for everything, and a CLI's *name* deciding which agent does the
    // work is an accident -- one that put every new project's default cheap
    // dispatch on the least suitable of the four (#145). Alphabetical is not
    // gone, it is what still separates two entries the catalogue rates the
    // same, which is the only question it was ever a good answer to.
    available.sort_by_key(|candidate| {
        let stats = observations.stats(&candidate.cli, &candidate.model);
        (
            Reverse(stats.validated),
            stats.quota_failures,
            Reverse(routing_weight(&pool, &candidate.cli)),
            candidate.cli.clone(),
            candidate.model.clone(),
        )
    });
    Ok(available.remove(0))
}

/// The editorial weight of the entry a candidate came from. Every candidate
/// was built out of `pool` a few lines above, so the lookup cannot miss; if it
/// somehow did, ranking that candidate last is the safe answer -- an
/// unattributable CLI is the last one to hand an unattended dispatch to, and
/// aborting the PM's turn over a tiebreak would be worse than any of them.
fn routing_weight(pool: &[&CliEntry], cli: &str) -> u8 {
    pool.iter()
        .find(|entry| entry.cli.id == cli)
        .map_or(0, |entry| entry.subagent.routing_weight)
}

/// Resolution when the PM named a model. The CLI is optional because the PM
/// skill documents `{task_id, role, model | cost_class}` and never mentions a
/// CLI -- so a bare model id has to find its own owner.
fn resolve_named(
    entries: &[CliEntry],
    installed: &BTreeSet<String>,
    cli: Option<&str>,
    model: &str,
) -> Result<Candidate> {
    if let Some(cli) = cli {
        let entry = find_entry(entries, cli)?;
        validate_model(entry, model)?;
        require_installed(entry, installed)?;
        return Ok(Candidate {
            cli: entry.cli.id.clone(),
            model: model.to_string(),
        });
    }

    let owners: Vec<&CliEntry> = entries
        .iter()
        .filter(|entry| {
            entry
                .models
                .static_models
                .iter()
                .any(|static_model| static_model.id == model)
        })
        .collect();
    match owners.as_slice() {
        [entry] => {
            require_installed(entry, installed)?;
            Ok(Candidate {
                cli: entry.cli.id.clone(),
                model: model.to_string(),
            })
        }
        [] => bail!(
            "unknown model {model:?}. Known models: {}{}",
            render_models(entries),
            render_discovery_note(entries)
        ),
        many => bail!(
            "model {model:?} is declared by {} -- add \"cli\" to say which one.",
            many.iter()
                .map(|entry| entry.cli.id.as_str())
                .collect::<Vec<_>>()
                .join(" and ")
        ),
    }
}

/// The installed entries a class-based resolution may draw from, narrowed to
/// one CLI when the PM named one.
fn installed_entries<'a>(
    entries: &'a [CliEntry],
    installed: &BTreeSet<String>,
    cli: Option<&str>,
) -> Result<Vec<&'a CliEntry>> {
    if let Some(cli) = cli {
        let entry = find_entry(entries, cli)?;
        require_installed(entry, installed)?;
        return Ok(vec![entry]);
    }
    let pool: Vec<&CliEntry> = entries
        .iter()
        .filter(|entry| installed.contains(&entry.cli.id))
        .collect();
    if pool.is_empty() {
        bail!(
            "no catalogued CLI is installed. mana looked on PATH for: {}",
            entries
                .iter()
                .map(|entry| format!("{} ({})", entry.cli.id, entry.cli.bin()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(pool)
}

fn find_entry<'a>(entries: &'a [CliEntry], cli: &str) -> Result<&'a CliEntry> {
    entries
        .iter()
        .find(|entry| entry.cli.id == cli)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown CLI {cli:?} -- the catalogue knows: {}",
                entries
                    .iter()
                    .map(|entry| entry.cli.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn require_installed(entry: &CliEntry, installed: &BTreeSet<String>) -> Result<()> {
    if !installed.contains(&entry.cli.id) {
        bail!(
            "{} is not installed -- mana found no {:?} on PATH. Install it: {}",
            entry.cli.id,
            entry.cli.bin(),
            entry.install.url
        );
    }
    Ok(())
}

/// Rejects a hallucinated model id before it can burn a dispatch (design §5:
/// "a hallucinated model id cannot fail a dispatch").
///
/// The escape hatch is for CLIs whose catalogue entry declares runtime
/// discovery and ships no curated list: mana has nothing to check against
/// until discovery is implemented, and refusing everything would make such a
/// CLI undispatchable. The CLI itself still rejects a bad id.
pub(crate) fn validate_model(entry: &CliEntry, model: &str) -> Result<()> {
    let statics = &entry.models.static_models;
    if statics.iter().any(|static_model| static_model.id == model) {
        return Ok(());
    }
    if statics.is_empty() && !entry.models.discovery_args.is_empty() {
        return Ok(());
    }
    bail!(
        "unknown model {model:?} for {} -- valid ids are: {}",
        entry.cli.id,
        if statics.is_empty() {
            "none (this catalogue entry declares no models)".to_string()
        } else {
            statics
                .iter()
                .map(|static_model| static_model.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
}

/// How long a pool rests after this failure meaning, from the first signature
/// that declares it. Signatures are ordered and the first match wins at
/// dispatch time (design §8), so the first one that mentions the meaning is
/// the one that produced this record.
fn cooldown_minutes(entry: &CliEntry, means: FailureMeans) -> i64 {
    entry
        .failure
        .iter()
        .find(|failure| failure.means == means)
        .and_then(|failure| failure.cooldown_minutes)
        .map_or(DEFAULT_COOLDOWN_MINUTES, i64::from)
}

/// Which models a cooldown reaches. `pool_scope = "global"` means the quota is
/// shared, so one model hitting the wall takes its pool-mates with it;
/// `per-model` (and an unknown model, which has no declared pool) cools only
/// the pair that failed.
fn cooled_models(entry: &CliEntry, model: &str) -> Vec<String> {
    let Some(failed) = entry
        .models
        .static_models
        .iter()
        .find(|static_model| static_model.id == model)
    else {
        return vec![model.to_string()];
    };
    let scope = entry
        .quota
        .pools
        .iter()
        .find(|pool| pool.id == failed.pool)
        .map(|pool| pool.pool_scope);
    match scope {
        Some(PoolScope::Global) => entry
            .models
            .static_models
            .iter()
            .filter(|static_model| static_model.pool == failed.pool)
            .map(|static_model| static_model.id.clone())
            .collect(),
        _ => vec![model.to_string()],
    }
}

/// One of mana's own timestamps, if it is one. `None` is "mana wrote this and
/// cannot read it back", which is a fact the caller has to decide about rather
/// than a value to default.
fn utc(stamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn cooling_means(wire: &str) -> Option<FailureMeans> {
    // Matched through the dispatcher's own wire names so the log format has
    // exactly one spelling, defined in one place.
    COOLING_MEANS
        .into_iter()
        .find(|means| failure_wire_name(*means) == wire)
}

fn render_classes(entries: &[&CliEntry]) -> String {
    let listed: Vec<String> = [CostClass::Cheap, CostClass::Mid, CostClass::Expensive]
        .into_iter()
        .filter_map(|class| {
            let models: Vec<String> = entries
                .iter()
                .flat_map(|entry| {
                    entry
                        .models
                        .static_models
                        .iter()
                        .filter(move |model| model.cost_class == class)
                        .map(move |model| format!("{}/{}", entry.cli.id, model.id))
                })
                .collect();
            (!models.is_empty()).then(|| format!("{} ({})", class.word(), models.join(", ")))
        })
        .collect();
    if listed.is_empty() {
        "no installed CLI declares any model at all".to_string()
    } else {
        listed.join("; ")
    }
}

fn render_models(entries: &[CliEntry]) -> String {
    let listed: Vec<String> = entries
        .iter()
        .filter(|entry| !entry.models.static_models.is_empty())
        .map(|entry| {
            format!(
                "{}: {}",
                entry.cli.id,
                entry
                    .models
                    .static_models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    if listed.is_empty() {
        "none".to_string()
    } else {
        listed.join("; ")
    }
}

/// Names the CLIs whose models mana cannot enumerate, so "unknown model" does
/// not read as "that model does not exist" when it might well be one of theirs.
fn render_discovery_note(entries: &[CliEntry]) -> String {
    let discovering: Vec<&str> = entries
        .iter()
        .filter(|entry| {
            entry.models.static_models.is_empty() && !entry.models.discovery_args.is_empty()
        })
        .map(|entry| entry.cli.id.as_str())
        .collect();
    if discovering.is_empty() {
        String::new()
    } else {
        format!(
            ". Models of {} are discovered at runtime and not listed here -- \
             name one together with \"cli\".",
            discovering.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::parse_entry;
    use crate::lock::{SubagentRecord, append_record};
    use crate::log::{ExitEntry, Status, append_log};
    use crate::project::resolve_project_paths;
    use crate::task::Role;

    /// A catalogue entry for a CLI that does not exist, built through the real
    /// parser so a schema change breaks these tests instead of silently
    /// leaving them testing a shape nobody ships.
    ///
    /// `models` is the `[models]` table body -- the one thing most tests here
    /// disagree about; `failures` appends `[[failure]]` tables, which have to
    /// come after every other table header.
    fn entry(id: &str, models: &str, failures: &str) -> CliEntry {
        parse_entry(&format!(
            r#"
schema = 1
notes = "routing fixture"

[cli]
id = "{id}"
name = "{id} CLI"
bin = "{id}-bin"
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
{models}

[skills]
dirs = []

[install]
url = "https://example.invalid/{id}"
{failures}
"#
        ))
        .expect("routing fixture must parse")
    }

    /// Two cheap models, one mid, all in one globally-scoped pool.
    fn shared_pool_entry(id: &str) -> CliEntry {
        entry(
            id,
            r#"discovery_args = []

[[models.static]]
id = "mini"
cost_class = "cheap"
pool = "main"

[[models.static]]
id = "nano"
cost_class = "cheap"
pool = "main"

[[models.static]]
id = "midi"
cost_class = "mid"
pool = "main"

[[quota.pools]]
id = "main"
kind = "requests"
period = "monthly"
pool_scope = "global""#,
            "",
        )
    }

    /// Same models, but each burns its own quota.
    fn per_model_entry(id: &str, failures: &str) -> CliEntry {
        entry(
            id,
            r#"discovery_args = []

[[models.static]]
id = "mini"
cost_class = "cheap"
pool = "main"

[[models.static]]
id = "nano"
cost_class = "cheap"
pool = "main"

[[quota.pools]]
id = "main"
kind = "requests"
period = "monthly"
pool_scope = "per-model""#,
            failures,
        )
    }

    /// The same entry with an editorial weight, set after parsing rather than
    /// written into the fixture's TOML: `entry` builds one string every test
    /// here shares, and threading a weight through it would make a dozen tests
    /// that do not care about ordering declare one. The default and the TOML
    /// spelling are covered where they belong, in `catalog`'s own tests.
    fn weighted(id: &str, weight: u8) -> CliEntry {
        let mut entry = shared_pool_entry(id);
        entry.subagent.routing_weight = weight;
        entry
    }

    fn installed(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn request<'a>(class: CostClass) -> Request<'a> {
        Request {
            cost_class: Some(class),
            ..Request::default()
        }
    }

    /// Observations built by hand, for the ordering tests that have no reason
    /// to go through the filesystem.
    fn synthetic(stats: &[(&str, &str, Stats)]) -> Observations {
        Observations {
            stats: stats
                .iter()
                .map(|(cli, model, counts)| ((cli.to_string(), model.to_string()), counts.clone()))
                .collect(),
            cooldowns: BTreeMap::new(),
        }
    }

    fn counts(validated: u32, quota_failures: u32) -> Stats {
        Stats {
            dispatched: validated + quota_failures,
            validated,
            quota_failures,
            ..Stats::default()
        }
    }

    /// The alphabetical rule, demoted by #145 from second tiebreak to last and
    /// not deleted: it is still the only thing that separates two candidates
    /// the catalogue rates identically, and it is still what makes a tie
    /// resolve the same way on every machine.
    #[test]
    fn with_no_history_equally_weighted_candidates_resolve_alphabetically() {
        // Neither fixture declares a routing_weight, so both carry the default.
        let entries = vec![shared_pool_entry("bravo"), shared_pool_entry("alpha")];
        let chosen = resolve(
            &entries,
            &installed(&["alpha", "bravo"]),
            &Observations::default(),
            &request(CostClass::Cheap),
        )
        .unwrap();
        // Alphabetical on (cli, model): "alpha" before "bravo", "mini" before
        // "nano" -- and the catalogue order (bravo first) does not decide it.
        assert_eq!(chosen.cli, "alpha");
        assert_eq!(chosen.model, "mini");
    }

    /// #145: with no history the first two keys are equal for every candidate,
    /// so before the catalogue could say anything the CLI's *name* decided
    /// which agent did the work -- which is how seeding agy's models (#31)
    /// silently moved every fresh project's default cheap dispatch onto the
    /// least suitable of the four.
    #[test]
    fn a_heavier_entry_wins_over_an_alphabetically_earlier_lighter_one() {
        let entries = vec![weighted("alpha", 10), weighted("bravo", 90)];
        let chosen = resolve(
            &entries,
            &installed(&["alpha", "bravo"]),
            &Observations::default(),
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(chosen.cli, "bravo");

        // ...and the weight is a belief, so it yields to evidence: one
        // validation on the lighter entry takes the dispatch straight back.
        let chosen = resolve(
            &entries,
            &installed(&["alpha", "bravo"]),
            &synthetic(&[("alpha", "mini", counts(1, 0))]),
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(
            (chosen.cli.as_str(), chosen.model.as_str()),
            ("alpha", "mini")
        );
    }

    #[test]
    fn cost_class_defaults_to_cheap() {
        let entries = vec![shared_pool_entry("alpha")];
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &Request::default(),
        )
        .unwrap();
        assert_eq!(chosen.model, "mini");
    }

    #[test]
    fn a_class_only_ever_yields_models_of_that_class() {
        let entries = vec![shared_pool_entry("alpha")];
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &request(CostClass::Mid),
        )
        .unwrap();
        assert_eq!(chosen.model, "midi");
    }

    #[test]
    fn more_validations_win_over_alphabetical_order() {
        let entries = vec![shared_pool_entry("alpha")];
        let observations = synthetic(&[("alpha", "nano", counts(3, 0))]);
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &observations,
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(chosen.model, "nano");
    }

    #[test]
    fn on_equal_validations_the_fewer_quota_failures_win() {
        let entries = vec![shared_pool_entry("alpha")];
        let observations = synthetic(&[
            ("alpha", "mini", counts(2, 4)),
            ("alpha", "nano", counts(2, 1)),
        ]);
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &observations,
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(chosen.model, "nano");
    }

    #[test]
    fn a_fully_tied_field_still_resolves_the_same_way_every_time() {
        let entries = vec![shared_pool_entry("bravo"), shared_pool_entry("alpha")];
        let observations = synthetic(&[
            ("alpha", "mini", counts(2, 1)),
            ("alpha", "nano", counts(2, 1)),
            ("bravo", "mini", counts(2, 1)),
            ("bravo", "nano", counts(2, 1)),
        ]);
        for _ in 0..8 {
            let chosen = resolve(
                &entries,
                &installed(&["alpha", "bravo"]),
                &observations,
                &request(CostClass::Cheap),
            )
            .unwrap();
            assert_eq!(
                (chosen.cli.as_str(), chosen.model.as_str()),
                ("alpha", "mini")
            );
        }
    }

    #[test]
    fn a_model_on_cooldown_is_skipped_however_good_its_record() {
        let entries = vec![shared_pool_entry("alpha")];
        let mut observations = synthetic(&[("alpha", "nano", counts(9, 0))]);
        observations.cooldowns.insert(
            ("alpha".to_string(), "nano".to_string()),
            Utc::now() + TimeDelta::minutes(30),
        );
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &observations,
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(chosen.model, "mini");
    }

    #[test]
    fn every_candidate_resting_is_an_error_that_names_the_wait() {
        let entries = vec![per_model_entry("alpha", "")];
        let until = Utc::now() + TimeDelta::minutes(30);
        let mut observations = Observations::default();
        for model in ["mini", "nano"] {
            observations
                .cooldowns
                .insert(("alpha".to_string(), model.to_string()), until);
        }
        let error = resolve(
            &entries,
            &installed(&["alpha"]),
            &observations,
            &request(CostClass::Cheap),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cooldown"), "{error}");
        assert!(error.contains("alpha/mini"), "{error}");
        assert!(error.contains(&until.to_rfc3339()), "{error}");
        // The PM's way out is named, because mana will not escalate by itself.
        assert!(error.contains("cost_class"), "{error}");
    }

    #[test]
    fn an_uninstalled_cli_is_never_a_candidate() {
        let entries = vec![shared_pool_entry("alpha"), shared_pool_entry("bravo")];
        let chosen = resolve(
            &entries,
            &installed(&["bravo"]),
            &Observations::default(),
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(chosen.cli, "bravo");
    }

    #[test]
    fn nothing_installed_is_an_error_naming_the_binaries_that_were_looked_for() {
        let entries = vec![shared_pool_entry("alpha")];
        let error = resolve(
            &entries,
            &installed(&[]),
            &Observations::default(),
            &request(CostClass::Cheap),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("alpha-bin"), "{error}");
    }

    #[test]
    fn a_class_no_installed_cli_declares_lists_the_ones_that_exist() {
        let entries = vec![shared_pool_entry("alpha")];
        let error = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &request(CostClass::Expensive),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("expensive"), "{error}");
        assert!(error.contains("cheap (alpha/mini, alpha/nano)"), "{error}");
        assert!(error.contains("mid (alpha/midi)"), "{error}");
    }

    #[test]
    fn an_explicit_pair_wins_over_every_counter_and_over_a_cooldown() {
        let entries = vec![shared_pool_entry("alpha")];
        let mut observations = synthetic(&[("alpha", "mini", counts(9, 0))]);
        observations.cooldowns.insert(
            ("alpha".to_string(), "midi".to_string()),
            Utc::now() + TimeDelta::minutes(30),
        );
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &observations,
            &Request {
                cli: Some("alpha"),
                model: Some("midi"),
                cost_class: Some(CostClass::Cheap),
            },
        )
        .unwrap();
        assert_eq!(
            (chosen.cli.as_str(), chosen.model.as_str()),
            ("alpha", "midi")
        );
    }

    #[test]
    fn a_bare_model_id_finds_its_own_cli() {
        // What the PM skill actually teaches: `{task_id, role, model}`, no CLI.
        let entries = vec![shared_pool_entry("alpha")];
        let chosen = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &Request {
                model: Some("midi"),
                ..Request::default()
            },
        )
        .unwrap();
        assert_eq!(
            (chosen.cli.as_str(), chosen.model.as_str()),
            ("alpha", "midi")
        );
    }

    #[test]
    fn a_model_two_clis_declare_asks_which_one() {
        let entries = vec![shared_pool_entry("alpha"), shared_pool_entry("bravo")];
        let error = resolve(
            &entries,
            &installed(&["alpha", "bravo"]),
            &Observations::default(),
            &Request {
                model: Some("mini"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("alpha and bravo"), "{error}");
        assert!(error.contains("cli"), "{error}");
    }

    #[test]
    fn an_unknown_model_lists_the_valid_ids() {
        let entries = vec![shared_pool_entry("alpha")];
        let error = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &Request {
                cli: Some("alpha"),
                model: Some("gpt-9-turbo"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("gpt-9-turbo"), "{error}");
        assert!(error.contains("mini, nano, midi"), "{error}");
    }

    #[test]
    fn an_unknown_model_with_no_cli_lists_every_cli_s_ids() {
        let entries = vec![shared_pool_entry("alpha")];
        let error = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &Request {
                model: Some("gpt-9-turbo"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("alpha: mini, nano, midi"), "{error}");
    }

    #[test]
    fn a_cli_whose_models_are_discovered_at_runtime_accepts_any_id() {
        // The shipped agy entry's shape: runtime discovery, no curated list.
        // Refusing every id would make that CLI undispatchable.
        let entries = vec![entry(
            "discoverer",
            "discovery_args = [\"models\"]\nline_regex = '^(\\S+)\\t'",
            "",
        )];
        let chosen = resolve(
            &entries,
            &installed(&["discoverer"]),
            &Observations::default(),
            &Request {
                cli: Some("discoverer"),
                model: Some("whatever-it-offers"),
                ..Request::default()
            },
        )
        .unwrap();
        assert_eq!(chosen.model, "whatever-it-offers");
        // ...and its existence is mentioned when a bare model id misses.
        let error = resolve(
            &entries,
            &installed(&["discoverer"]),
            &Observations::default(),
            &Request {
                model: Some("whatever-it-offers"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("discovered at runtime"), "{error}");
    }

    #[test]
    fn an_unknown_cli_lists_the_catalogue_ids() {
        let entries = vec![shared_pool_entry("alpha")];
        let error = resolve(
            &entries,
            &installed(&["alpha"]),
            &Observations::default(),
            &Request {
                cli: Some("clode"),
                model: Some("mini"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("clode"), "{error}");
        assert!(error.contains("alpha"), "{error}");
    }

    #[test]
    fn an_explicit_pair_on_an_uninstalled_cli_points_at_the_install_url() {
        let entries = vec![shared_pool_entry("alpha")];
        let error = resolve(
            &entries,
            &installed(&[]),
            &Observations::default(),
            &Request {
                cli: Some("alpha"),
                model: Some("mini"),
                ..Request::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("https://example.invalid/alpha"), "{error}");
    }

    /// A project directory with a registry, logs and reviews -- everything
    /// `Observations::gather` reads.
    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: ProjectPaths,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let paths = resolve_project_paths(tmp.path(), "demo");
            crate::project::ensure_project_structure(&paths).unwrap();
            Fixture { _tmp: tmp, paths }
        }

        fn dispatch(&self, agent_id: &str, cli: &str, model: &str, task_id: &str, role: Role) {
            append_record(
                &self.paths.subagents_file,
                &SubagentRecord {
                    agent_id: agent_id.to_string(),
                    cli: cli.to_string(),
                    model: model.to_string(),
                    role,
                    task_id: task_id.to_string(),
                    pid: Some(1),
                    started_at: "2026-08-15T10:00:00Z".to_string(),
                },
            )
            .unwrap();
        }

        fn exit(&self, agent_id: &str, at: DateTime<Utc>, failure_means: Option<&str>) {
            self.exit_stamped(agent_id, &at.to_rfc3339(), failure_means);
        }

        /// The same, with the timestamp written verbatim -- the only way to
        /// exercise a record mana cannot date.
        fn exit_stamped(&self, agent_id: &str, timestamp: &str, failure_means: Option<&str>) {
            append_log(
                &self.paths.logs.join(format!("{agent_id}.jsonl")),
                &ExitEntry {
                    status: Status::Done,
                    action: "exited".to_string(),
                    timestamp: timestamp.to_string(),
                    exit_code: Some(i32::from(failure_means.is_some())),
                    duration_ms: Some(1000),
                    failure_means: failure_means.map(str::to_string),
                },
            )
            .unwrap();
        }

        fn verdict(&self, task_id: &str, json: &str) {
            std::fs::write(self.paths.reviews.join(format!("{task_id}.json")), json).unwrap();
        }
    }

    #[test]
    fn gather_counts_validations_against_the_executor_that_earned_them() {
        let fixture = Fixture::new();
        let entries = vec![shared_pool_entry("alpha")];
        let now = Utc::now();

        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now, None);
        fixture.verdict("task-1", r#"{"verdict":"validated","issues":[]}"#);

        fixture.dispatch("a2", "alpha", "nano", "task-2", Role::Executor);
        fixture.exit("a2", now, None);
        fixture.verdict(
            "task-2",
            r#"{"verdict":"rejected","attribution":"code","issues":["nope"]}"#,
        );

        fixture.dispatch("a3", "alpha", "nano", "task-3", Role::Executor);
        fixture.exit("a3", now, None);
        // A brief the PM botched: recorded nowhere (design §8).
        fixture.verdict(
            "task-3",
            r#"{"verdict":"rejected","attribution":"brief","issues":["unclear"]}"#,
        );

        // The reviewer that read task-1 must not be credited with its verdict.
        fixture.dispatch("a4", "alpha", "midi", "task-1", Role::Reviewer);
        fixture.exit("a4", now, None);

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        assert_eq!(observations.stats("alpha", "mini").validated, 1);
        assert_eq!(observations.stats("alpha", "mini").dispatched, 1);
        assert_eq!(observations.stats("alpha", "nano").validated, 0);
        assert_eq!(observations.stats("alpha", "nano").rejected_for_code, 1);
        assert_eq!(observations.stats("alpha", "midi").validated, 0);
        assert_eq!(observations.stats("alpha", "midi").dispatched, 1);
        // Never dispatched reads as zeros, not as absent.
        assert_eq!(observations.stats("alpha", "ghost"), Stats::default());
    }

    #[test]
    fn a_recent_quota_failure_cools_the_whole_shared_pool() {
        let fixture = Fixture::new();
        let entries = vec![shared_pool_entry("alpha")];
        let now = Utc::now();
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now - TimeDelta::minutes(5), Some("quota_exhausted"));

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        // pool_scope = global: the sibling models rest too.
        for model in ["mini", "nano", "midi"] {
            assert!(
                observations.cooldown_until("alpha", model).is_some(),
                "{model} should be resting"
            );
        }
        assert_eq!(observations.stats("alpha", "mini").quota_failures, 1);

        // 60 minutes by default, so it expires between +5 and +60 of the run.
        let until = observations.cooldown_until("alpha", "mini").unwrap();
        assert_eq!(until, now - TimeDelta::minutes(5) + TimeDelta::minutes(60));
    }

    #[test]
    fn a_per_model_pool_cools_only_the_model_that_failed() {
        let fixture = Fixture::new();
        let entries = vec![per_model_entry("alpha", "")];
        let now = Utc::now();
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now - TimeDelta::minutes(5), Some("rate_limited"));

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        assert!(observations.cooldown_until("alpha", "mini").is_some());
        assert!(observations.cooldown_until("alpha", "nano").is_none());
    }

    #[test]
    fn a_cooldown_declared_by_the_catalogue_beats_the_default() {
        let entries = vec![per_model_entry(
            "alpha",
            "[[failure]]\nmeans = \"rate_limited\"\nstdout_regex = \"x\"\ncooldown_minutes = 5\n",
        )];
        let fixture = Fixture::new();
        let now = Utc::now();
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now - TimeDelta::minutes(10), Some("rate_limited"));

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        // The catalogue said 5 minutes and the failure is 10 minutes old, so
        // the pool is already back in rotation -- the 60-minute default would
        // still be resting it.
        assert!(observations.cooldown_until("alpha", "mini").is_none());
    }

    #[test]
    fn an_expired_cooldown_is_not_reported() {
        let fixture = Fixture::new();
        let entries = vec![per_model_entry("alpha", "")];
        let now = Utc::now();
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now - TimeDelta::minutes(61), Some("quota_exhausted"));

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        assert!(observations.cooldown_until("alpha", "mini").is_none());
        // The failure itself is still counted -- only the rest period expired.
        assert_eq!(observations.stats("alpha", "mini").quota_failures, 1);
    }

    #[test]
    fn an_expired_credential_is_not_a_cooldown() {
        let fixture = Fixture::new();
        let entries = vec![per_model_entry("alpha", "")];
        let now = Utc::now();
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now, Some("auth_expired"));

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        assert!(
            observations.cooldown_until("alpha", "mini").is_none(),
            "waiting does not log anyone back in"
        );
    }

    /// A cooldown mana cannot date is still a cooldown: dropping it is what
    /// costs money, because the next dispatch pays to rediscover the wall.
    #[test]
    fn an_unparseable_exit_timestamp_does_not_clear_the_cooldown() {
        let fixture = Fixture::new();
        let entries = vec![per_model_entry("alpha", "")];
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit_stamped("a1", "yesterday afternoon", Some("quota_exhausted"));

        // Half an hour after the dispatch started -- the stand-in mana falls
        // back to, and well inside the default hour.
        let now = utc("2026-08-15T10:30:00Z").unwrap();
        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        assert_eq!(
            observations.cooldown_until("alpha", "mini"),
            Some(utc("2026-08-15T11:00:00Z").unwrap()),
            "an undated quota failure must rest the pool from the dispatch's own start"
        );
    }

    /// A wire name from a mana that knew more failure meanings than this one.
    /// Unrecognised is not "fine": the record only exists because a dispatch
    /// failed, so it rests the pool for the default hour.
    #[test]
    fn a_failure_meaning_this_build_cannot_map_still_rests_the_pool() {
        let fixture = Fixture::new();
        let entries = vec![per_model_entry("alpha", "")];
        let now = Utc::now();
        fixture.dispatch("a1", "alpha", "mini", "task-1", Role::Executor);
        fixture.exit("a1", now - TimeDelta::minutes(5), Some("region_blocked"));

        let observations = Observations::gather(&fixture.paths, &entries, now).unwrap();
        assert_eq!(
            observations.cooldown_until("alpha", "mini"),
            Some(now - TimeDelta::minutes(5) + TimeDelta::minutes(DEFAULT_COOLDOWN_MINUTES))
        );
    }

    #[test]
    fn an_empty_project_observes_nothing() {
        let fixture = Fixture::new();
        let observations = Observations::gather(&fixture.paths, &[], Utc::now()).unwrap();
        assert_eq!(observations.stats("alpha", "mini"), Stats::default());
        assert!(observations.cooldown_until("alpha", "mini").is_none());
    }

    #[test]
    fn the_shipped_catalogue_routes_a_cheap_task_to_a_declared_model() {
        // Guards the real data: an entry whose models lost their cost classes
        // would make every cheapest-first dispatch fail in front of a user.
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        let entries = catalog.entries();
        let installed: BTreeSet<String> =
            entries.iter().map(|entry| entry.cli.id.clone()).collect();
        let chosen = resolve(
            entries,
            &installed,
            &Observations::default(),
            &request(CostClass::Cheap),
        )
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.cli.id == chosen.cli)
            .unwrap();
        assert!(
            entry
                .models
                .static_models
                .iter()
                .any(|model| model.id == chosen.model && model.cost_class == CostClass::Cheap)
        );
    }

    /// The concrete regression #145 reports. Seeding agy's `[[models.static]]`
    /// for #31 was right, but it also gave a fresh project a cheap candidate
    /// whose CLI id sorts before "claude" -- and with no history to separate
    /// them, that alone decided. mana's own choice then landed on the entry
    /// with one concurrency slot the PM's turn may already hold, a print mode
    /// that denies reads, and a brief that must spell out absolute paths.
    ///
    /// Asserted against the shipped files rather than a fixture on purpose:
    /// the defect was in the data as much as in the sort, so an edit that
    /// drops claude's weight below agy's has to fail here.
    #[test]
    fn the_shipped_catalogue_sends_a_no_history_cheap_task_to_claude() {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        let entries = catalog.entries();
        let installed: BTreeSet<String> =
            entries.iter().map(|entry| entry.cli.id.clone()).collect();
        let chosen = resolve(
            entries,
            &installed,
            &Observations::default(),
            &request(CostClass::Cheap),
        )
        .unwrap();
        assert_eq!(chosen.cli, "claude", "a fresh project's default dispatch");
        assert_ne!(chosen.cli, "agy");
    }
}
