use crate::config::{Config, load_config, save_config};
use crate::project::mana_home;

pub fn remove_agent(config: &mut Config, name: &str) -> bool {
    config.models.remove(name).is_some()
}

pub fn run(name: &str) -> anyhow::Result<()> {
    let home = mana_home()?;
    run_at(&home.join("config.yaml"), name)
}

fn run_at(config_path: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;
    if remove_agent(&mut config, name) {
        save_config(config_path, &config)?;
        println!("{name}: retire de la configuration mana");
    } else {
        println!("{name}: n'etait pas enregistre");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    #[test]
    fn remove_agent_removes_existing_entry() {
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/bin/claude".into(),
            },
        );
        assert!(remove_agent(&mut config, "claude"));
        assert!(config.models.is_empty());
    }

    #[test]
    fn remove_agent_on_missing_entry_returns_false() {
        let mut config = Config::default();
        assert!(!remove_agent(&mut config, "claude"));
    }

    #[test]
    fn run_at_removes_existing_agent_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/bin/claude".into(),
            },
        );
        save_config(&config_path, &config).unwrap();

        run_at(&config_path, "claude").unwrap();

        let reloaded = load_config(&config_path).unwrap();
        assert!(!reloaded.models.contains_key("claude"));
    }

    #[test]
    fn run_at_on_unknown_agent_leaves_config_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        save_config(&config_path, &Config::default()).unwrap();

        run_at(&config_path, "claude").unwrap();

        let reloaded = load_config(&config_path).unwrap();
        assert!(reloaded.models.is_empty());
    }
}
