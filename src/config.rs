use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub models: BTreeMap<String, AgentConfig>,
}

pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let contents = std::fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&contents)?)
}

pub fn save_config(path: &Path, config: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(config)?;
    std::fs::write(path, yaml)?;
    Ok(())
}

/// Checks that `agent_cli` was registered via `mana install`. Shared by
/// every command that needs to spawn a registered agent CLI
/// (`launch`/`--subagent`), so the error message and check stay consistent.
pub fn ensure_agent_registered(config: &Config, agent_cli: &str) -> anyhow::Result<()> {
    if config.models.contains_key(agent_cli) {
        Ok(())
    } else {
        anyhow::bail!(
            "agent '{agent_cli}' non enregistre. Lancez 'mana install' pour l'enregistrer."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_config_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        let cfg = load_config(&path).unwrap();
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn ensure_agent_registered_rejects_unknown_agent() {
        let config = Config::default();
        assert!(ensure_agent_registered(&config, "claude").is_err());
    }

    #[test]
    fn ensure_agent_registered_accepts_known_agent() {
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/usr/local/bin/claude".into(),
            },
        );
        assert!(ensure_agent_registered(&config, "claude").is_ok());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.yaml");
        let mut cfg = Config::default();
        cfg.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".to_string(),
                version: "1.0.16".to_string(),
                path: "/usr/local/bin/claude".to_string(),
            },
        );
        save_config(&path, &cfg).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded, cfg);
    }
}
