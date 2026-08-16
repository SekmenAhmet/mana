use crate::catalog::{Catalog, CliEntry};
use crate::config::{AgentConfig, Config, config_path, load_config, save_config};
use crate::project::mana_home;
use crate::subprocess::{VERSION_CHECK_TIMEOUT, capture_version_output};
use anyhow::Context;
use dialoguer::MultiSelect;

/// The user's escape valve (design §7): a local override may add a CLI mana
/// does not ship an entry for, and `mana install` has to offer it like any
/// other. Same filename `mana dev` reads it from.
const CATALOG_OVERRIDE: &str = "catalog.local.toml";

/// Pre-checks agents already registered in `config`, one entry per catalogue
/// entry, so the selector mirrors current state instead of always starting
/// blank — matches `mana install.md`'s mockup, which shows some entries
/// already checked.
fn default_selections(config: &Config, entries: &[CliEntry]) -> Vec<bool> {
    entries
        .iter()
        .map(|entry| config.models.contains_key(&entry.cli.id))
        .collect()
}

/// Resolves an installed CLI's absolute path and version from its catalogue
/// entry. Kept separate from the interactive selector below so it's testable
/// against a real, always-present binary.
///
/// Note what is read from the entry and not guessed: the binary name (which
/// is frequently not the id — Antigravity's id is `agy`, and per-OS
/// `bin_overrides` can change it again) and the flags that print a version.
pub fn resolve_agent(entry: &CliEntry) -> anyhow::Result<AgentConfig> {
    let path = entry
        .cli
        .resolve()
        .with_context(|| format!("install {} first: {}", entry.cli.name, entry.install.url))?;
    let version = capture_version_output(&path, &entry.cli.version_args, VERSION_CHECK_TIMEOUT)?;
    Ok(AgentConfig {
        name: entry.cli.id.clone(),
        version,
        path: path.to_string_lossy().to_string(),
        version_args: entry.cli.version_args.clone(),
    })
}

pub fn run() -> anyhow::Result<()> {
    let home = mana_home()?;
    let catalog = Catalog::load(Some(&home.join(CATALOG_OVERRIDE)))?;
    let entries = catalog.entries();
    let config_path = config_path(&home);
    let config = load_config(&config_path)?;
    let defaults = default_selections(&config, entries);

    let selection = MultiSelect::new()
        .with_prompt("Select Agent config (space: select, enter: save)")
        .items(&catalog.ids())
        .defaults(&defaults)
        .interact()?;
    let selected: Vec<&CliEntry> = selection.into_iter().map(|idx| &entries[idx]).collect();

    install_selected(&selected, &config_path)
}

/// Resolves each selected catalogue entry to an `AgentConfig` and persists
/// the resulting config. Kept separate from `run` so the resolve+save loop is
/// testable without going through the interactive `MultiSelect` prompt.
fn install_selected(entries: &[&CliEntry], config_path: &std::path::Path) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;

    for entry in entries {
        let id = &entry.cli.id;
        match resolve_agent(entry) {
            Ok(agent) => {
                println!("{id}: {} ({})", agent.version, agent.path);
                config.models.insert(id.clone(), agent);
            }
            Err(err) => eprintln!("{id}: {err}"),
        }
    }

    save_config(config_path, &config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::embedded().unwrap()
    }

    /// Rewrites an embedded entry's binary name so a test can point it at
    /// something that really exists on the machine. Everything else about the
    /// entry stays as shipped, which is the point: the code path under test
    /// must read the binary from the entry rather than from the id.
    fn entry_with_bin(catalog: &Catalog, id: &str, bin: &str) -> CliEntry {
        let mut entry = catalog.get(id).unwrap().clone();
        entry.cli.bin = bin.to_string();
        entry.cli.bin_overrides.clear();
        entry
    }

    #[test]
    fn default_selections_marks_registered_agents_true() {
        let catalog = catalog();
        let entries = catalog.entries();
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/usr/local/bin/claude".into(),
                version_args: vec!["--version".into()],
            },
        );
        let defaults = default_selections(&config, entries);
        assert_eq!(defaults.len(), entries.len());
        let claude_idx = entries.iter().position(|e| e.cli.id == "claude").unwrap();
        assert!(defaults[claude_idx]);
        for (idx, &is_default) in defaults.iter().enumerate() {
            if idx != claude_idx {
                assert!(
                    !is_default,
                    "unexpected pre-checked entry: {}",
                    entries[idx].cli.id
                );
            }
        }
    }

    #[test]
    fn default_selections_all_false_for_empty_config() {
        let catalog = catalog();
        let defaults = default_selections(&Config::default(), catalog.entries());
        assert_eq!(defaults, vec![false; catalog.entries().len()]);
    }

    /// The list the selector offers is the catalogue's, not a hardcoded one.
    /// A CLI mana has no entry for cannot be driven — registering it would
    /// put a name in `list_agents` that every dispatch then fails on.
    #[test]
    fn the_offered_list_is_exactly_the_catalogue() {
        let catalog = catalog();
        assert_eq!(catalog.ids(), ["claude", "agy", "copilot", "opencode"]);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_agent_finds_a_real_binary_named_by_the_entry() {
        let catalog = catalog();
        // id "claude", binary "cat": if resolution used the id this fails.
        let entry = entry_with_bin(&catalog, "claude", "cat");
        let agent = resolve_agent(&entry).unwrap();
        assert_eq!(agent.name, "claude");
        assert!(agent.path.ends_with("cat"));
        assert_eq!(agent.version_args, entry.cli.version_args);
    }

    #[test]
    fn resolve_agent_errors_on_unknown_binary_and_names_the_install_url() {
        let catalog = catalog();
        let entry = entry_with_bin(&catalog, "claude", "this-binary-does-not-exist-anywhere");
        // `{:#}` rather than `to_string()`: the binary name now lives in the
        // source error `CliMeta::resolve` returns, and the install URL in the
        // context wrapped around it. `main` returns `anyhow::Result`, which
        // Rust prints with `Debug` -- so the whole chain is what a user
        // actually reads, and the whole chain is what this asserts on.
        let message = format!("{:#}", resolve_agent(&entry).unwrap_err());
        assert!(
            message.contains("this-binary-does-not-exist-anywhere"),
            "{message}"
        );
        assert!(message.contains(&entry.install.url), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn install_selected_persists_resolved_agent_under_its_catalogue_id() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = config_path(tmp.path());
        let catalog = catalog();
        let entry = entry_with_bin(&catalog, "claude", "cat");

        install_selected(&[&entry], &config_path).unwrap();

        let config = load_config(&config_path).unwrap();
        let agent = config.models.get("claude").expect("claude should resolve");
        assert!(agent.path.ends_with("cat"));
        assert_eq!(agent.version_args, ["--version"]);
    }

    #[test]
    fn install_selected_skips_unresolvable_entries_but_still_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = config_path(tmp.path());
        let catalog = catalog();
        let entry = entry_with_bin(&catalog, "claude", "this-binary-does-not-exist-anywhere");

        install_selected(&[&entry], &config_path).unwrap();

        let config = load_config(&config_path).unwrap();
        assert!(config.models.is_empty());
    }
}
