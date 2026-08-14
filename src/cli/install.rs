use crate::agents::KNOWN_CLIS;
use crate::config::{load_config, save_config, AgentConfig};
use crate::project::mana_home;
use dialoguer::MultiSelect;

/// Resolves an installed CLI's absolute path and version. Kept separate from
/// the interactive selector below so it's testable against a real,
/// always-present binary.
pub fn resolve_agent(name: &str) -> anyhow::Result<AgentConfig> {
    let path = which::which(name).map_err(|_| anyhow::anyhow!("binaire '{name}' introuvable dans le PATH"))?;
    let output = std::process::Command::new(&path).arg("--version").output()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(AgentConfig { name: name.to_string(), version, path: path.to_string_lossy().to_string() })
}

pub fn run() -> anyhow::Result<()> {
    let selection = MultiSelect::new()
        .with_prompt("Select Agent config (space: select, enter: save)")
        .items(KNOWN_CLIS)
        .interact()?;

    let home = mana_home()?;
    let config_path = home.join("config.yaml");
    let mut config = load_config(&config_path)?;

    for idx in selection {
        let name = KNOWN_CLIS[idx];
        match resolve_agent(name) {
            Ok(agent) => {
                println!("{name}: {} ({})", agent.version, agent.path);
                config.models.insert(name.to_string(), agent);
            }
            Err(err) => eprintln!("{name}: {err}"),
        }
    }

    save_config(&config_path, &config)?;
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
}
