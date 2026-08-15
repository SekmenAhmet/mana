use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    pub version: String,
    pub path: String,
    /// How `version` was obtained, copied from the catalogue's
    /// `[cli].version_args` at install time. Stored rather than looked up
    /// again so `mana doctor` re-runs the *same* probe without needing to
    /// know which CLI it is holding — and so a config entry says out loud how
    /// its version was measured. No `serde(default)`: an entry written before
    /// this field existed cannot be re-probed correctly, and
    /// `deny_unknown_fields` plus `load_config`'s error already point at the
    /// fix (`mana install`).
    pub version_args: Vec<String>,
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

/// v1's config file lived next to where the TOML file now lives. mana no
/// longer reads it — this path exists only so a leftover one can be pointed
/// at by name instead of being silently ignored.
fn legacy_yaml_path(toml_path: &Path) -> PathBuf {
    toml_path.with_extension("yaml")
}

/// What `load_config` says when the only config present is v1's YAML file.
/// Pulled out so the wording has its own test instead of only being
/// exercised as a side effect of an error path.
fn legacy_config_message(legacy: &Path) -> String {
    format!(
        "{} is v1's YAML config, which mana no longer reads (config is TOML now). \
         Run 'mana install' to register your CLIs again, then delete it.",
        legacy.display()
    )
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

    // v1 (mana's previous incarnation) wrote config.yaml, and phase 0
    // migrated it in place. That migration is gone with `serde_yaml`, which
    // is archived upstream and was the only reason a YAML parser was still
    // linked in. Nothing is lost: a config entry is a name, a version and a
    // `$PATH` lookup, all of which `mana install` re-derives in seconds. What
    // would be lost is the user's registrations *silently* — a leftover YAML
    // file next to no TOML file looks exactly like a fresh install — so say
    // it out loud instead of returning an empty config.
    let legacy = legacy_yaml_path(path);
    if legacy.exists() {
        anyhow::bail!("{}", legacy_config_message(&legacy));
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
                version_args: vec!["--version".into()],
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
    fn legacy_config_message_names_the_file_and_the_fix() {
        let message = legacy_config_message(Path::new("/home/x/.mana/config.yaml"));
        assert!(message.contains("config.yaml"));
        assert!(message.contains("mana install"));
    }

    /// The failure mode this guards is silence: a leftover config.yaml with no
    /// config.toml beside it must not read as "nothing registered yet".
    #[test]
    fn load_config_refuses_to_ignore_a_leftover_yaml_config() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        let yaml_path = legacy_yaml_path(&toml_path);
        std::fs::write(&yaml_path, "models:\n  claude:\n    name: claude\n").unwrap();

        let message = load_config(&toml_path).unwrap_err().to_string();
        assert!(message.contains("mana install"), "{message}");
        // ...and it stays a read: nothing was written in the user's name.
        assert!(!toml_path.exists());
    }

    #[test]
    fn load_config_ignores_a_leftover_yaml_config_once_toml_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = config_path(tmp.path());
        let toml_cfg = sample_config();
        save_config(&toml_path, &toml_cfg).unwrap();
        std::fs::write(legacy_yaml_path(&toml_path), "models:\n  agy: {}\n").unwrap();

        let loaded = load_config(&toml_path).unwrap();
        assert_eq!(loaded, toml_cfg);
        assert!(!loaded.models.contains_key("agy"));
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
