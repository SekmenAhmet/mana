use crate::agents::KNOWN_CLIS;
use crate::config::{AgentConfig, load_config, save_config};
use crate::project::mana_home;
use crate::subprocess::{VERSION_CHECK_TIMEOUT, capture_version_output};
use dialoguer::MultiSelect;

/// Resolves an installed CLI's absolute path and version. Kept separate from
/// the interactive selector below so it's testable against a real,
/// always-present binary.
pub fn resolve_agent(name: &str) -> anyhow::Result<AgentConfig> {
    let path = which::which(name)
        .map_err(|_| anyhow::anyhow!("binaire '{name}' introuvable dans le PATH"))?;
    let version = capture_version_output(&path, VERSION_CHECK_TIMEOUT)?;
    Ok(AgentConfig {
        name: name.to_string(),
        version,
        path: path.to_string_lossy().to_string(),
    })
}

pub fn run() -> anyhow::Result<()> {
    let selection = MultiSelect::new()
        .with_prompt("Select Agent config (space: select, enter: save)")
        .items(KNOWN_CLIS)
        .interact()?;
    let selected_names: Vec<&str> = selection.into_iter().map(|idx| KNOWN_CLIS[idx]).collect();

    let home = mana_home()?;
    install_selected(&selected_names, &home.join("config.yaml"))
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
