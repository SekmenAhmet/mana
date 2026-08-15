use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
// `deny_unknown_fields` is load-bearing, not decoration: a differently
// shaped foreign config (e.g. per-CLI top-level tables, seen in the wild
// from an older prototype) must fail to parse instead of silently
// deserializing into an empty `Config` — see
// `load_config_rejects_foreign_schema_toml_instead_of_reading_it_as_empty`.
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub models: BTreeMap<String, AgentConfig>,
}

/// Canonical location of mana's config file under `~/.mana`. TOML is what
/// the original managent spec used and what v2 restores as the only format
/// mana ever writes going forward. Centralized here so every call site
/// resolves the filename the same way instead of each concatenating
/// `home.join("config.yaml")` by hand — that duplication is how v1's
/// prompt/code path mismatch happened.
pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// v1's config file lived next to where the TOML file now lives. Only
/// `load_config`'s one-time migration should ever read this path — nothing
/// else in mana should know YAML config exists.
fn legacy_yaml_path(toml_path: &Path) -> PathBuf {
    toml_path.with_extension("yaml")
}

/// The one line printed when `load_config` migrates a legacy YAML config in
/// place. Pulled out of `load_config` so the wording has its own test
/// instead of only being exercised as a side effect of a println!.
fn migration_message(legacy: &Path, canonical: &Path) -> String {
    format!("migrated {} -> {}", legacy.display(), canonical.display())
}

pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    if path.exists() {
        let contents = std::fs::read_to_string(path)?;
        return toml::from_str(&contents).map_err(|err| {
            // A config.toml that exists but won't parse is either
            // hand-edit damage or (as seen on the dev machine this
            // migration shipped from) a leftover from an older,
            // differently-shaped prototype config. Either way, falling
            // back to an empty config would silently hide the user's
            // registrations; point at the fix instead of guessing at one.
            anyhow::anyhow!(
                "failed to parse {}: {err}\nrun 'mana install' to regenerate it",
                path.display()
            )
        });
    }

    // One-time migration: v1 (mana's previous incarnation) wrote
    // config.yaml. If no TOML config exists yet but a YAML sibling does,
    // parse it once, persist it as TOML, and leave the YAML file exactly
    // where it was — deleting user data is not this migration's call, and
    // an untouched YAML file is a free rollback path if the migration is
    // ever wrong.
    let legacy = legacy_yaml_path(path);
    if legacy.exists() {
        let contents = std::fs::read_to_string(&legacy)?;
        let config: Config = serde_yaml::from_str(&contents)
            .map_err(|err| anyhow::anyhow!("failed to parse legacy {}: {err}", legacy.display()))?;
        save_config(path, &config)?;
        println!("{}", migration_message(&legacy, path));
        return Ok(config);
    }

    Ok(Config::default())
}

pub fn save_config(path: &Path, config: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Pretty-printed: config.toml is meant to be hand-readable/editable,
    // same expectation the old config.yaml carried.
    let toml = toml::to_string_pretty(config)?;
    std::fs::write(path, toml)?;
    Ok(())
}

/// Checks that `agent_cli` was registered via `mana install`. Shared by
/// every command that needs to spawn a registered agent CLI
/// (`launch`/`--subagent`), so the error message and check stay consistent.
pub fn ensure_agent_registered(config: &Config, agent_cli: &str) -> anyhow::Result<()> {
    if config.models.contains_key(agent_cli) {
        Ok(())
    } else {
        anyhow::bail!("agent '{agent_cli}' not registered. Run 'mana install' to register it.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".to_string(),
                version: "1.0.16".to_string(),
                path: "/usr/local/bin/claude".to_string(),
            },
        );
        config
    }

    #[test]
    fn load_config_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = config_path(tmp.path());
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
        let path = tmp.path().join("nested/config.toml");
        let cfg = sample_config();
        save_config(&path, &cfg).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn save_config_writes_toml_not_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = config_path(tmp.path());
        save_config(&path, &sample_config()).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        // TOML tables read as `[models.claude]`; YAML would have rendered
        // `models:` with indented keys instead. This is a format check,
        // not a schema check — the round-trip test above covers schema.
        assert!(contents.contains("[models.claude]"));
        assert!(!contents.contains("models:\n"));
    }

    #[test]
    fn migration_message_names_both_paths() {
        let legacy = Path::new("/home/x/.mana/config.yaml");
        let canonical = Path::new("/home/x/.mana/config.toml");
        let message = migration_message(legacy, canonical);
        assert!(message.contains("config.yaml"));
        assert!(message.contains("config.toml"));
    }

    #[test]
    fn load_config_migrates_legacy_yaml_and_preserves_it() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        let yaml_path = legacy_yaml_path(&toml_path);
        let cfg = sample_config();
        let yaml_text = serde_yaml::to_string(&cfg).unwrap();
        std::fs::write(&yaml_path, &yaml_text).unwrap();

        let loaded = load_config(&toml_path).unwrap();
        assert_eq!(loaded, cfg);

        // The migration wrote config.toml...
        assert!(toml_path.exists());
        let migrated = load_config(&toml_path).unwrap();
        assert_eq!(migrated, cfg);

        // ...and left config.yaml exactly as it was (cheap rollback).
        let yaml_after = std::fs::read_to_string(&yaml_path).unwrap();
        assert_eq!(yaml_after, yaml_text);
    }

    #[test]
    fn load_config_skips_migration_when_toml_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        let yaml_path = legacy_yaml_path(&toml_path);

        let toml_cfg = sample_config();
        save_config(&toml_path, &toml_cfg).unwrap();

        // A YAML sibling with *different* content must be ignored entirely
        // once config.toml is present.
        let mut yaml_cfg = Config::default();
        yaml_cfg.models.insert(
            "agy".to_string(),
            AgentConfig {
                name: "agy".to_string(),
                version: "9.9.9".to_string(),
                path: "/usr/local/bin/agy".to_string(),
            },
        );
        std::fs::write(&yaml_path, serde_yaml::to_string(&yaml_cfg).unwrap()).unwrap();

        let loaded = load_config(&toml_path).unwrap();
        assert_eq!(loaded, toml_cfg);
        assert!(!loaded.models.contains_key("agy"));
    }

    #[test]
    fn load_config_errors_loudly_on_corrupt_legacy_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        let yaml_path = legacy_yaml_path(&toml_path);
        std::fs::write(&yaml_path, "models: [this is not a map").unwrap();

        let result = load_config(&toml_path);
        assert!(result.is_err());
        // A failed migration must not leave a bogus/empty config.toml behind.
        assert!(!toml_path.exists());
    }

    #[test]
    fn load_config_errors_loudly_on_corrupt_toml_and_suggests_install() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        std::fs::write(&toml_path, "this is not [ valid toml").unwrap();

        let result = load_config(&toml_path);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("mana install"));
    }

    #[test]
    fn load_config_rejects_foreign_schema_toml_instead_of_reading_it_as_empty() {
        // Shaped after a real file found at ~/.mana/config.toml on the dev
        // machine this migration shipped from: an older, unrelated
        // prototype used per-CLI top-level tables instead of nesting under
        // `models.<id>`. Without `deny_unknown_fields`, this parses
        // "successfully" into an empty `Config` (every key here is unknown
        // to our struct and silently dropped) — which looks exactly like a
        // fresh install and would hide the user's real registrations
        // instead of erroring loudly.
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        std::fs::write(
            &toml_path,
            r#"
[copilot]
version = "GitHub Copilot CLI 1.0.78."
path = "/Users/x/.local/bin/copilot"

[claude]
version = "2.1.231 (Claude Code)"
path = "/opt/homebrew/bin/claude"
"#,
        )
        .unwrap();

        let result = load_config(&toml_path);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("mana install"));
    }
}
