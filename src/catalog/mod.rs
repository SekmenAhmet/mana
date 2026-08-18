//! The CLI catalogue: everything that differs between agent CLIs, as data.
//!
//! The rule this module exists to enforce is that no CLI-specific knowledge
//! lives in Rust. Spawn flags, auto-approve flags, model ids, failure
//! signatures and skill directories all rot on someone else's release schedule
//! -- a hardcoded list already went stale within months -- so they belong in
//! `catalog/*.toml`, where updating them is a data change and not a refactor.
//! Nothing below may branch on which CLI it is holding.

mod embedded;
mod schema;
mod template;

// Re-exported as globs so callers write `catalog::Subagent` and
// `catalog::substitute`: the split into submodules is an organisation detail,
// not something the rest of mana should have to track.
pub use schema::*;
pub use template::*;

use anyhow::{Context, Result, bail};
use std::path::Path;

/// The only schema this build understands. The catalogue carries a version --
/// unlike mana's internal contracts -- because outside humans edit these files
/// and their editor may be older or newer than the binary reading them.
/// The user's catalogue escape valve (design §7): a local override may replace
/// a shipped entry or add a CLI mana ships no entry for.
///
/// Declared here rather than in each command because four of them read it, and
/// four spellings of one filename is four chances for a command to look in a
/// place the others do not -- the failure would be a silently ignored override,
/// which is the hardest kind to notice.
pub const CATALOG_OVERRIDE: &str = "catalog.local.toml";

pub const SCHEMA_VERSION: u32 = 1;

/// The catalogue as mana sees it: the embedded entries, with the user's local
/// override applied on top.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    entries: Vec<CliEntry>,
}

impl Catalog {
    /// Parses the embedded entries and applies `local_override` if that file
    /// exists. A missing override is normal; an unparseable one is an error,
    /// because silently ignoring it would leave the user staring at the old
    /// behaviour with no clue their edit was thrown away.
    ///
    /// The override replaces a whole entry by id, or adds a new CLI if the id
    /// is unknown. No field-level merge: a half-overridden entry is the kind
    /// of state nobody can reason about while a CLI is broken on a Tuesday.
    pub fn load(local_override: Option<&Path>) -> Result<Self> {
        let mut catalog = Self::embedded()?;
        let Some(path) = local_override else {
            return Ok(catalog);
        };
        if !path.exists() {
            return Ok(catalog);
        }
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("reading catalogue override {}", path.display()))?;
        let entry = parse_entry(&source)
            .with_context(|| format!("invalid catalogue override {}", path.display()))?;
        catalog.upsert(entry);
        Ok(catalog)
    }

    /// The entries baked into the binary, with no override applied. A failure
    /// here is a build defect, which is why a unit test calls this: a
    /// malformed catalogue must fail `cargo test`, never a user's dispatch.
    pub fn embedded() -> Result<Self> {
        Self::from_sources(embedded::FILES.iter().map(|f| (f.path, f.source)))
    }

    fn from_sources<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self> {
        let mut entries: Vec<CliEntry> = Vec::new();
        for (label, source) in sources {
            let entry = parse_entry(source)
                .with_context(|| format!("catalogue file {label} is invalid"))?;
            if entries.iter().any(|e| e.cli.id == entry.cli.id) {
                bail!("catalogue file {label} redeclares id '{}'", entry.cli.id);
            }
            entries.push(entry);
        }
        Ok(Self { entries })
    }

    fn upsert(&mut self, entry: CliEntry) {
        match self.entries.iter().position(|e| e.cli.id == entry.cli.id) {
            Some(index) => self.entries[index] = entry,
            None => self.entries.push(entry),
        }
    }

    pub fn get(&self, id: &str) -> Option<&CliEntry> {
        self.entries.iter().find(|e| e.cli.id == id)
    }

    pub fn entries(&self) -> &[CliEntry] {
        &self.entries
    }

    pub fn ids(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.cli.id.as_str()).collect()
    }
}

/// Parses one catalogue file and checks everything the type system cannot.
pub fn parse_entry(source: &str) -> Result<CliEntry> {
    let entry: CliEntry = toml::from_str(source)?;
    validate(&entry)?;
    Ok(entry)
}

/// The checks that turn a syntactically valid file into a usable entry. They
/// run at load time so the embedded catalogue's unit test is a real gate:
/// every one of these would otherwise surface as a broken dispatch weeks later.
fn validate(entry: &CliEntry) -> Result<()> {
    if entry.schema != SCHEMA_VERSION {
        bail!(
            "catalogue schema {} is not supported by this build (expected {SCHEMA_VERSION}); \
             upgrade mana or fix the file",
            entry.schema
        );
    }
    for (field, args) in argv_templates(entry) {
        validate_placeholders(args, &field)?;
    }

    let id = &entry.cli.id;
    // ACP carries prose, tool calls, permissions and the end of a turn as
    // typed notifications, so both of these describe something the driver will
    // never look at. Rejecting them rather than ignoring them keeps design §4's
    // "non-ACP drivers only" a rule the file has to obey, not a comment: a
    // maintainer who copies claude's entry to add an ACP CLI finds out at
    // `cargo test`, not from a chat pane that stayed empty.
    if entry.pm.driver == PmDriver::Acp {
        if entry.pm.events.is_some() {
            bail!(
                "{id}: [pm.events] must be absent for the acp driver, which reads typed \
                 notifications rather than JSONPaths over a proprietary stream (design §4)"
            );
        }
        if entry.pm.prompt.is_some() {
            bail!(
                "{id}: [pm].prompt must be absent for the acp driver, which sends turns as \
                 typed content blocks rather than over argv or stdin"
            );
        }
    } else if entry.pm.prompt.is_none() {
        bail!(
            "{id}: [pm].prompt is required for the {:?} driver",
            entry.pm.driver
        );
    }

    // A oneshot driver spawns a fresh process per turn, so it has no use for
    // `args` and is unusable without both of these.
    if entry.pm.driver == PmDriver::OneshotContinue {
        for (field, args) in [
            ("first_args", &entry.pm.first_args),
            ("continue_args", &entry.pm.continue_args),
        ] {
            if args.as_ref().is_none_or(|a| a.is_empty()) {
                bail!("{id}: [pm].driver is oneshot-continue, which needs a non-empty {field}");
            }
        }
    }

    // A signature with nothing to match on would sit in the ordered list doing
    // nothing, and the CLI would look like it had no known failure modes.
    for (index, failure) in entry.failure.iter().enumerate() {
        let matches_on = failure.exit_codes.is_some()
            || failure.stderr_regex.is_some()
            || failure.stdout_regex.is_some();
        if !matches_on {
            bail!(
                "{id}: [[failure]] #{} has no exit_codes, stderr_regex or stdout_regex, so it can never match",
                index + 1
            );
        }
    }

    // Cooldowns are applied to a pool, so a model pointing at a pool that does
    // not exist would be exhaustion-proof by accident.
    for model in &entry.models.static_models {
        if !entry.quota.pools.iter().any(|p| p.id == model.pool) {
            bail!(
                "{id}: model '{}' is in pool '{}', which no [[quota.pools]] declares",
                model.id,
                model.pool
            );
        }
    }
    Ok(())
}

/// Every argv template in an entry, labelled for error messages. Kept in one
/// place so a new templated field cannot be added without deciding whether it
/// takes placeholders.
fn argv_templates(entry: &CliEntry) -> Vec<(String, &[String])> {
    let id = &entry.cli.id;
    let mut templates: Vec<(String, &[String])> = vec![
        (format!("{id}: [cli].version_args"), &entry.cli.version_args),
        (format!("{id}: [pm].args"), &entry.pm.args),
        (
            format!("{id}: [pm].permission_args"),
            &entry.pm.permission_args,
        ),
        (format!("{id}: [pm].resume_args"), &entry.pm.resume_args),
        (format!("{id}: [tools].mcp_args"), &entry.tools.mcp_args),
        (format!("{id}: [subagent].args"), &entry.subagent.args),
        (
            format!("{id}: [subagent].auto_approve_args"),
            &entry.subagent.auto_approve_args,
        ),
        (
            format!("{id}: [subagent].model_args"),
            &entry.subagent.model_args,
        ),
        (
            format!("{id}: [models].discovery_args"),
            &entry.models.discovery_args,
        ),
    ];
    if let Some(args) = &entry.pm.first_args {
        templates.push((format!("{id}: [pm].first_args"), args));
    }
    if let Some(args) = &entry.pm.continue_args {
        templates.push((format!("{id}: [pm].continue_args"), args));
    }
    templates
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete, valid entry for a CLI that does not exist. Tests mutate a
    /// copy of it rather than depending on the shipped files, so a real
    /// catalogue edit cannot quietly break unrelated tests.
    const FIXTURE: &str = r#"
schema = 1
notes = "fixture"

[cli]
id = "alpha"
name = "Alpha CLI"
bin = "alpha"
version_args = ["--version"]

[pm]
driver = "stream"
args = ["-p"]
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
max_concurrent = 0
cwd_required_in_brief = false

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

    fn fixture_with(from: &str, to: &str) -> String {
        assert!(FIXTURE.contains(from), "fixture has no {from:?} to replace");
        FIXTURE.replace(from, to)
    }

    fn write_override(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.local.toml");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    /// The design document's reference entry is what someone building an
    /// override copies, and an override replaces a whole entry -- there is no
    /// field-by-field merge -- so a field the reference forgets is a field the
    /// copy silently loses.
    ///
    /// Asserts on the field names inside the section rather than parsing the
    /// fenced TOML out of the Markdown: the fields belonging to a driver the
    /// reference entry is not (`oneshot-continue`) are shown commented out, as
    /// `line_regex` already was, and a TOML parse would not see them at all.
    #[test]
    fn the_design_documents_reference_entry_names_every_field_the_schema_accepts() {
        const DESIGN: &str =
            include_str!("../../docs/superpowers/specs/2026-08-15-mana-v2-design.md");
        let section = DESIGN
            .split_once("### Schema (`schema = 1`)")
            .expect("the reference entry section")
            .1
            .split_once("\n### ")
            .expect("the reference entry section ends at the next heading")
            .0;
        for field in [
            "permission_args",
            "resume_args",
            "first_args",
            "continue_args",
            "routing_weight",
            "inline_in_activation",
            "exit_codes",
            "stderr_regex",
        ] {
            assert!(
                section.contains(field),
                "{field} is missing from the design document's reference entry"
            );
        }
    }

    /// The build-time gate. If a shipped catalogue file is malformed this test
    /// fails, so the bad data never reaches a release.
    #[test]
    fn every_embedded_file_parses_and_validates() {
        let catalog = Catalog::embedded().unwrap();
        assert_eq!(catalog.ids(), vec!["claude", "agy", "copilot", "opencode"]);

        // `notes` is the only top-level key, so it has to be written before the
        // first [table] header. Losing it means the file was reordered and the
        // provenance of every flag went with it.
        for entry in catalog.entries() {
            assert!(!entry.notes.is_empty(), "{} lost its notes", entry.cli.id);
        }

        let claude = catalog.get("claude").unwrap();
        assert_eq!(claude.pm.driver, PmDriver::Stream);
        assert_eq!(claude.tools.channel, ToolChannel::Mcp);
        // No catalogued CLI restricts its PM any more: claude's allowlist was
        // dropped on 2026-08-18 so the PM could reach the issues and the
        // branches the loop needs at either end. The rule this pins is not
        // "the PM is restricted" but the one `mana doctor` reads -- an
        // auto-approve flag must never be filed under `permission_args`,
        // because an entry with a non-empty one is reported as enforcing
        // something.
        assert!(claude.pm.permission_args.is_empty());
        assert_eq!(claude.subagent.max_concurrent, 0);
        // `mana launch claude --continue`, measured on this machine twice
        // (see the entry's notes). One flag, appended to the argv above.
        assert_eq!(claude.pm.resume_args, ["--continue"]);
        // Project-local first, so the PM role stops appearing in the global
        // skill list of every other project the user opens. A relative entry
        // is resolved against the project root by `cli::launch_pm`.
        assert_eq!(claude.skills.dirs[0], ".claude/skills");
        assert!(claude.skills.dirs.iter().any(|dir| dir.starts_with('~')));
        assert_eq!(
            claude.pm.events.as_ref().unwrap().usage.as_deref(),
            Some("$.usage")
        );

        // #145: mana's own choice on a project with no history is data, not
        // an accident of where "agy" and "claude" fall in the alphabet.
        assert!(
            claude.subagent.routing_weight > catalog.get("agy").unwrap().subagent.routing_weight,
            "claude must outrank agy for a dispatch nobody chose"
        );

        let agy = catalog.get("agy").unwrap();
        assert_eq!(agy.pm.driver, PmDriver::OneshotContinue);
        assert_eq!(agy.tools.channel, ToolChannel::Sentinel);
        assert_eq!(agy.subagent.max_concurrent, 1);
        assert!(agy.subagent.cwd_required_in_brief);
        assert_eq!(agy.models.discovery_args, vec!["models"]);
        // Deliberately absent: no allowlist-shaped flag was ever measured on
        // this CLI, and inventing one would fail the launch at startup.
        assert!(agy.pm.permission_args.is_empty());
        assert_eq!(agy.quota.pools[0].pool_scope, PoolScope::PerModel);
        // Deliberately empty: agy has never shown an observable quota signal,
        // and a signature borrowed from another CLI would trigger false
        // cooldowns (a copilot-measured one was removed on 2026-08-15).
        assert!(agy.failure.is_empty());

        // The two ACP entries are what makes the PM role a role rather than a
        // claude feature (milestone 3), and the pair is deliberately uneven:
        // one attaches mana through the protocol, the other through argv,
        // because that is what each CLI was measured doing.
        for id in ["copilot", "opencode"] {
            let entry = catalog.get(id).unwrap();
            assert_eq!(entry.pm.driver, PmDriver::Acp, "{id}");
            // Both are non-ACP fields; the validator rejects them here, and
            // this is the assertion that the shipped files stay on that side.
            assert!(entry.pm.events.is_none(), "{id}");
            assert!(entry.pm.prompt.is_none(), "{id}");
            // ACP has no allowlist flag: the permission flow is the
            // enforcement, and inventing a flag would fail the launch.
            assert!(entry.pm.permission_args.is_empty(), "{id}");
            assert_eq!(entry.tools.channel, ToolChannel::Mcp, "{id}");
        }
        // opencode honours session/new's mcpServers, so it needs no flags...
        assert!(catalog.get("opencode").unwrap().tools.mcp_args.is_empty());
        // ...while copilot rejects stdio MCP servers offered that way and gets
        // the config file over argv instead. One line of data, no code branch.
        assert_eq!(
            catalog.get("copilot").unwrap().tools.mcp_args,
            ["--additional-mcp-config", "@{config_path}"]
        );
    }

    #[test]
    fn load_without_override_returns_the_embedded_catalogue() {
        let catalog = Catalog::load(None).unwrap();
        assert_eq!(catalog.ids(), Catalog::embedded().unwrap().ids());
    }

    #[test]
    fn missing_override_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::load(Some(&dir.path().join("absent.toml"))).unwrap();
        assert_eq!(catalog.entries().len(), embedded::FILES.len());
    }

    #[test]
    fn override_replaces_the_whole_entry() {
        let (_dir, path) = write_override(&fixture_with(r#"id = "alpha""#, r#"id = "claude""#));
        let catalog = Catalog::load(Some(&path)).unwrap();

        assert_eq!(catalog.entries().len(), embedded::FILES.len());
        let claude = catalog.get("claude").unwrap();
        assert_eq!(claude.cli.name, "Alpha CLI");
        // The embedded entry's model list is gone rather than merged: wholesale
        // replacement is the point of the escape valve.
        assert_eq!(claude.models.static_models.len(), 1);
        assert_eq!(claude.models.static_models[0].id, "mini");
    }

    #[test]
    fn override_can_add_a_new_cli() {
        let (_dir, path) = write_override(FIXTURE);
        let catalog = Catalog::load(Some(&path)).unwrap();
        assert_eq!(catalog.entries().len(), embedded::FILES.len() + 1);
        assert_eq!(catalog.get("alpha").unwrap().cli.bin, "alpha");
    }

    #[test]
    fn invalid_override_fails_loudly_and_names_the_file() {
        let (_dir, path) = write_override("schema = 1\nthis is not toml");
        let err = Catalog::load(Some(&path)).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("catalog.local.toml"), "{rendered}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse_entry(&fixture_with(
            r#"bin = "alpha""#,
            "bin = \"alpha\"\nbim = \"typo\"",
        ))
        .unwrap_err();
        assert!(err.to_string().contains("bim"), "{err}");
    }

    #[test]
    fn unknown_enum_value_is_rejected() {
        let err = parse_entry(&fixture_with(
            r#"driver = "stream""#,
            r#"driver = "telepathy""#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("telepathy"), "{err}");
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let err = parse_entry(&fixture_with("schema = 1", "schema = 99")).unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let err = Catalog::from_sources([("a.toml", FIXTURE), ("b.toml", FIXTURE)]).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("redeclares"), "{rendered}");
        assert!(rendered.contains("b.toml"), "{rendered}");
    }

    #[test]
    fn unknown_placeholder_in_a_template_is_caught_at_load() {
        let err = parse_entry(&fixture_with(
            r#"model_args = ["--model", "{model}"]"#,
            r#"model_args = ["--model", "{modle}"]"#,
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("modle"), "{rendered}");
        assert!(rendered.contains("[subagent].model_args"), "{rendered}");
    }

    /// Every templated field is checked, including the one added last: the
    /// list in `argv_templates` is easy to forget to extend, and a field
    /// missing from it silently stops being validated.
    #[test]
    fn unknown_placeholder_in_permission_args_is_caught_at_load() {
        let err = parse_entry(&fixture_with(
            r#"prompt = "stdin-jsonl""#,
            "prompt = \"stdin-jsonl\"\npermission_args = [\"--allow\", \"{tolls}\"]",
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("tolls"), "{rendered}");
        assert!(rendered.contains("[pm].permission_args"), "{rendered}");
    }

    #[test]
    fn oneshot_driver_without_turn_args_is_rejected() {
        let err = parse_entry(&fixture_with(
            r#"driver = "stream""#,
            r#"driver = "oneshot-continue""#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("first_args"), "{err}");
    }

    /// Design §4 says `[pm.events]` is for non-ACP drivers only. Left as a
    /// comment it would be copied away the first time somebody added an ACP
    /// CLI by editing claude's entry; as a check it fails `cargo test`.
    #[test]
    fn an_acp_entry_may_not_carry_the_non_acp_fields() {
        let acp = fixture_with(r#"driver = "stream""#, r#"driver = "acp""#);
        let err = parse_entry(&acp).unwrap_err();
        assert!(err.to_string().contains("[pm.events]"), "{err}");

        let err = parse_entry(&acp.replace("[pm.events]\ntext = \"$.text\"\n", "")).unwrap_err();
        assert!(err.to_string().contains("[pm].prompt"), "{err}");
    }

    #[test]
    fn an_acp_entry_without_either_of_them_is_valid() {
        let entry = parse_entry(
            &fixture_with(r#"driver = "stream""#, r#"driver = "acp""#)
                .replace("[pm.events]\ntext = \"$.text\"\n", "")
                .replace("prompt = \"stdin-jsonl\"\n", ""),
        )
        .unwrap();
        assert_eq!(entry.pm.driver, PmDriver::Acp);
    }

    /// The other half of the same rule: every driver that reads a stream needs
    /// to be told how a turn gets *out*, and a missing `prompt` used to be
    /// impossible rather than merely wrong.
    #[test]
    fn a_non_acp_entry_without_a_prompt_mode_is_rejected() {
        let err = parse_entry(&fixture_with("prompt = \"stdin-jsonl\"\n", "")).unwrap_err();
        assert!(err.to_string().contains("[pm].prompt"), "{err}");
    }

    #[test]
    fn failure_signature_with_nothing_to_match_on_is_rejected() {
        let err = parse_entry(&fixture_with(r#"stdout_regex = "rate.?limit""#, "")).unwrap_err();
        assert!(err.to_string().contains("never match"), "{err}");
    }

    #[test]
    fn model_in_an_undeclared_pool_is_rejected() {
        let err = parse_entry(&fixture_with(r#"pool = "main""#, r#"pool = "typo""#)).unwrap_err();
        assert!(err.to_string().contains("typo"), "{err}");
    }

    #[test]
    fn per_os_bin_override_wins_on_the_matching_platform() {
        // An inline table keeps the replacement on one line: a `[cli.bin_overrides]`
        // header here would swallow every [cli] key written after it.
        let entry = parse_entry(&fixture_with(
            r#"bin = "alpha""#,
            &format!(
                "bin = \"alpha\"\nbin_overrides = {{ {} = \"alpha.exe\" }}",
                std::env::consts::OS
            ),
        ))
        .unwrap();
        assert_eq!(entry.cli.bin(), "alpha.exe");
        assert_eq!(entry.cli.bin, "alpha");
    }

    #[test]
    fn bin_falls_back_when_no_override_matches_this_os() {
        let entry = parse_entry(FIXTURE).unwrap();
        assert_eq!(entry.cli.bin(), "alpha");
    }

    /// The tempting non-fix for the Windows spawn bug is `bin_overrides =
    /// { windows = "claude.cmd" }`. It works for whoever installed from npm and
    /// breaks whoever installed natively (`claude.exe`), because which of the
    /// two is on PATH is a property of the install and not of the OS. The real
    /// fix is `CliMeta::resolve`; this keeps the data side from quietly
    /// re-acquiring an opinion that contradicts it.
    #[test]
    fn no_entry_papers_over_path_resolution_with_a_file_extension() {
        for entry in Catalog::embedded().unwrap().entries() {
            for (os, bin) in &entry.cli.bin_overrides {
                assert!(
                    !bin.contains('.'),
                    "{}: [cli.bin_overrides].{os} is {bin:?} -- a file extension here pins one \
                     install method; PATH resolution belongs in CliMeta::resolve",
                    entry.cli.id
                );
            }
        }
    }

    /// The lookup every spawn path and every "is it installed?" answer shares.
    /// A binary that is really there resolves to an absolute path, which is
    /// what makes the Windows case work for both install methods: `which`
    /// consults PATHEXT, `Command::new` does not.
    #[test]
    fn resolve_returns_an_absolute_path_for_a_binary_that_exists() {
        // Present on every supported host, and not something mana ships.
        let real = if cfg!(windows) { "cmd" } else { "sh" };
        let entry = parse_entry(&fixture_with(
            r#"bin = "alpha""#,
            &format!("bin = {real:?}"),
        ))
        .unwrap();
        let path = entry.cli.resolve().unwrap();
        assert!(path.is_absolute(), "{}", path.display());
        assert!(path.exists(), "{}", path.display());
    }

    #[test]
    fn resolve_names_the_binary_it_could_not_find() {
        let err = parse_entry(FIXTURE).unwrap().cli.resolve().unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("alpha"), "{rendered}");
        assert!(rendered.contains("PATH"), "{rendered}");
    }

    /// #145 added `routing_weight`, and the default is the whole of what keeps
    /// that addition safe: a `catalog.local.toml` written before the field
    /// existed declares none, and must land mid-order rather than be demoted
    /// under every shipped entry for having said nothing.
    #[test]
    fn an_entry_that_declares_no_routing_weight_lands_mid_scale() {
        assert_eq!(parse_entry(FIXTURE).unwrap().subagent.routing_weight, 50);
        let declared = parse_entry(&fixture_with(
            "cwd_required_in_brief = false",
            "cwd_required_in_brief = false\nrouting_weight = 90",
        ))
        .unwrap();
        assert_eq!(declared.subagent.routing_weight, 90);
    }

    /// Routing is "cheapest first, escalate on failure", which only works if
    /// the ordinal survives parsing.
    #[test]
    fn cost_classes_order_cheapest_first() {
        assert!(CostClass::Cheap < CostClass::Mid);
        assert!(CostClass::Mid < CostClass::Expensive);
    }
}
