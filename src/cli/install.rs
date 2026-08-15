use crate::agents::KNOWN_CLIS;
use crate::config::{AgentConfig, Config, load_config, save_config};
use crate::project::mana_home;
use crate::subprocess::{VERSION_CHECK_TIMEOUT, capture_version_output};
use dialoguer::MultiSelect;

/// Pre-checks agents already registered in `config`, one entry per
/// `KNOWN_CLIS`, so the selector mirrors current state instead of always
/// starting blank — matches `mana install.md`'s mockup, which shows some
/// entries already checked.
fn default_selections(config: &Config) -> Vec<bool> {
    KNOWN_CLIS
        .iter()
        .map(|name| config.models.contains_key(*name))
        .collect()
}

/// Resolves an installed CLI's absolute path and version. Kept separate from
/// the interactive selector below so it's testable against a real,
/// always-present binary.
pub fn resolve_agent(name: &str) -> anyhow::Result<AgentConfig> {
    let path =
        which::which(name).map_err(|_| anyhow::anyhow!("binary '{name}' not found in PATH"))?;
    let version = capture_version_output(&path, VERSION_CHECK_TIMEOUT)?;
    Ok(AgentConfig {
        name: name.to_string(),
        version,
        path: path.to_string_lossy().to_string(),
    })
}

pub fn run() -> anyhow::Result<()> {
    let home = mana_home()?;
    let config_path = home.join("config.yaml");
    let config = load_config(&config_path)?;
    let defaults = default_selections(&config);

    let selection = MultiSelect::new()
        .with_prompt("Select Agent config (space: select, enter: save)")
        .items(KNOWN_CLIS)
        .defaults(&defaults)
        .interact()?;
    let selected_names: Vec<&str> = selection.into_iter().map(|idx| KNOWN_CLIS[idx]).collect();

    install_selected(&selected_names, &config_path)
}

/// Resolves each selected CLI name to an `AgentConfig` and persists the
/// resulting config. Kept separate from `run` so the resolve+save loop is
/// testable without going through the interactive `MultiSelect` prompt.
fn install_selected(names: &[&str], config_path: &std::path::Path) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;

    for &name in names {
        match resolve_agent(name) {
            Ok(agent) => {
                println!("{name}: {} ({})", agent.version, agent.path);
                config.models.insert(name.to_string(), agent);
            }
            Err(err) => eprintln!("{name}: {err}"),
        }
    }

    save_config(config_path, &config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_selections_marks_registered_agents_true() {
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/usr/local/bin/claude".into(),
            },
        );
        let defaults = default_selections(&config);
        assert_eq!(defaults.len(), KNOWN_CLIS.len());
        let claude_idx = KNOWN_CLIS.iter().position(|&c| c == "claude").unwrap();
        assert!(defaults[claude_idx]);
        for (idx, &is_default) in defaults.iter().enumerate() {
            if idx != claude_idx {
                assert!(
                    !is_default,
                    "unexpected pre-checked entry: {}",
                    KNOWN_CLIS[idx]
                );
            }
        }
    }

    #[test]
    fn default_selections_all_false_for_empty_config() {
        let defaults = default_selections(&Config::default());
        assert_eq!(defaults, vec![false; KNOWN_CLIS.len()]);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_agent_finds_a_real_binary() {
        let agent = resolve_agent("cat").unwrap();
        assert_eq!(agent.name, "cat");
        assert!(agent.path.ends_with("cat"));
    }

    #[test]
    fn resolve_agent_errors_on_unknown_binary() {
        let result = resolve_agent("this-binary-does-not-exist-anywhere");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_selected_persists_resolved_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");

        install_selected(&["cat"], &config_path).unwrap();

        let config = load_config(&config_path).unwrap();
        let agent = config.models.get("cat").expect("cat should be resolved");
        assert!(agent.path.ends_with("cat"));
    }

    #[test]
    fn install_selected_skips_unresolvable_names_but_still_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");

        install_selected(&["this-binary-does-not-exist-anywhere"], &config_path).unwrap();

        let config = load_config(&config_path).unwrap();
        assert!(config.models.is_empty());
    }
}
