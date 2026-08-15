use crate::config::{AgentConfig, Config, config_path, load_config, save_config};
use crate::project::mana_home;
use crate::subprocess::{VERSION_CHECK_TIMEOUT, capture_version_output};

#[derive(Debug, Clone, PartialEq)]
pub enum DoctorIssue {
    MissingBinary {
        agent: String,
        path: String,
    },
    OutdatedVersion {
        agent: String,
        registered: String,
        current: String,
    },
}

pub fn diagnose(config: &Config) -> Vec<DoctorIssue> {
    let mut issues = Vec::new();
    for (name, agent) in &config.models {
        if !std::path::Path::new(&agent.path).exists() {
            issues.push(DoctorIssue::MissingBinary {
                agent: name.clone(),
                path: agent.path.clone(),
            });
            continue;
        }
        if let Ok(current) = current_version(agent)
            && current != agent.version
        {
            issues.push(DoctorIssue::OutdatedVersion {
                agent: name.clone(),
                registered: agent.version.clone(),
                current,
            });
        }
    }
    issues
}

fn current_version(agent: &AgentConfig) -> anyhow::Result<String> {
    capture_version_output(
        std::path::Path::new(&agent.path),
        &agent.version_args,
        VERSION_CHECK_TIMEOUT,
    )
}

/// Applies the safe, automatic half of doctor.md's "Actions proposees":
/// an outdated-version entry just gets its metadata refreshed in place —
/// mana already knows the correct value (`current`), so there's nothing to
/// ask the user. Returns whether the config was actually changed.
fn apply_fix(config: &mut Config, issue: &DoctorIssue) -> bool {
    match issue {
        DoctorIssue::OutdatedVersion { agent, current, .. } => match config.models.get_mut(agent) {
            Some(entry) => {
                entry.version = current.clone();
                true
            }
            None => false,
        },
        DoctorIssue::MissingBinary { .. } => false,
    }
}

/// The other half of "Actions proposees": a missing binary has no safe
/// automatic fix (mana doesn't know where the right one is), so doctor
/// suggests the repair command instead of guessing.
fn repair_suggestion(issue: &DoctorIssue) -> Option<String> {
    match issue {
        DoctorIssue::MissingBinary { agent, .. } => Some(format!(
            "run 'mana install' to re-register '{agent}', or 'mana uninstall {agent}' to remove it from the configuration"
        )),
        DoctorIssue::OutdatedVersion { .. } => None,
    }
}

pub fn run() -> anyhow::Result<()> {
    let home = mana_home()?;
    run_at(&config_path(&home))
}

fn run_at(config_path: &std::path::Path) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;
    let issues = diagnose(&config);
    if issues.is_empty() {
        println!(
            "mana doctor: everything is in order ({} agent(s) registered)",
            config.models.len()
        );
        return Ok(());
    }

    let mut config_changed = false;
    for issue in &issues {
        match issue {
            DoctorIssue::OutdatedVersion {
                agent,
                registered,
                current,
            } => {
                apply_fix(&mut config, issue);
                config_changed = true;
                println!("[{agent}] version updated: {registered} -> {current}");
            }
            DoctorIssue::MissingBinary { agent, path } => {
                println!("[{agent}] binary not found: {path}");
                if let Some(suggestion) = repair_suggestion(issue) {
                    println!("  -> {suggestion}");
                }
            }
        }
    }

    if config_changed {
        save_config(config_path, &config)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_flags_missing_binary() {
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/nonexistent/path/claude".into(),
                version_args: vec!["--version".into()],
            },
        );
        let issues = diagnose(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0],
            DoctorIssue::MissingBinary {
                agent: "claude".to_string(),
                path: "/nonexistent/path/claude".to_string(),
            }
        );
    }

    #[test]
    fn diagnose_reports_no_issues_for_empty_config() {
        let config = Config::default();
        assert!(diagnose(&config).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn diagnose_flags_outdated_version() {
        use crate::subprocess::write_version_script;

        let tmp = tempfile::tempdir().unwrap();
        let path = write_version_script(tmp.path(), "agent.sh", "2.0.0")
            .to_string_lossy()
            .to_string();

        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0.0".into(),
                path,
                version_args: vec!["--version".into()],
            },
        );
        let issues = diagnose(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0],
            DoctorIssue::OutdatedVersion {
                agent: "claude".to_string(),
                registered: "1.0.0".to_string(),
                current: "2.0.0".to_string(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn diagnose_reports_no_issue_when_version_matches() {
        use crate::subprocess::write_version_script;

        let tmp = tempfile::tempdir().unwrap();
        let path = write_version_script(tmp.path(), "agent.sh", "1.0.0")
            .to_string_lossy()
            .to_string();

        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0.0".into(),
                path,
                version_args: vec!["--version".into()],
            },
        );
        assert!(diagnose(&config).is_empty());
    }

    #[test]
    fn apply_fix_updates_outdated_version_in_place() {
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0.0".into(),
                path: "/bin/claude".into(),
                version_args: vec!["--version".into()],
            },
        );
        let issue = DoctorIssue::OutdatedVersion {
            agent: "claude".to_string(),
            registered: "1.0.0".to_string(),
            current: "2.0.0".to_string(),
        };
        assert!(apply_fix(&mut config, &issue));
        assert_eq!(config.models.get("claude").unwrap().version, "2.0.0");
    }

    #[test]
    fn apply_fix_is_a_noop_for_missing_binary() {
        let mut config = Config::default();
        let issue = DoctorIssue::MissingBinary {
            agent: "claude".to_string(),
            path: "/nonexistent".to_string(),
        };
        assert!(!apply_fix(&mut config, &issue));
    }

    #[test]
    fn repair_suggestion_offers_install_or_uninstall_for_missing_binary() {
        let issue = DoctorIssue::MissingBinary {
            agent: "claude".to_string(),
            path: "/nonexistent".to_string(),
        };
        let suggestion = repair_suggestion(&issue).unwrap();
        assert!(suggestion.contains("mana install"));
        assert!(suggestion.contains("mana uninstall claude"));
    }

    #[test]
    fn repair_suggestion_is_none_for_outdated_version() {
        let issue = DoctorIssue::OutdatedVersion {
            agent: "claude".to_string(),
            registered: "1.0.0".to_string(),
            current: "2.0.0".to_string(),
        };
        assert!(repair_suggestion(&issue).is_none());
    }

    #[test]
    fn run_at_reports_ok_for_empty_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = config_path(tmp.path());
        crate::config::save_config(&config_path, &Config::default()).unwrap();
        assert!(run_at(&config_path).is_ok());
    }

    #[test]
    fn run_at_reports_issues_for_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = config_path(tmp.path());
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/nonexistent/path/claude".into(),
                version_args: vec!["--version".into()],
            },
        );
        crate::config::save_config(&config_path, &config).unwrap();
        assert!(run_at(&config_path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn run_at_persists_the_refreshed_version_for_outdated_agents() {
        use crate::subprocess::write_version_script;

        let tmp = tempfile::tempdir().unwrap();
        let path = write_version_script(tmp.path(), "agent.sh", "2.0.0")
            .to_string_lossy()
            .to_string();

        let config_path = config_path(tmp.path());
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0.0".into(),
                path,
                version_args: vec!["--version".into()],
            },
        );
        save_config(&config_path, &config).unwrap();

        run_at(&config_path).unwrap();

        let reloaded = load_config(&config_path).unwrap();
        assert_eq!(reloaded.models.get("claude").unwrap().version, "2.0.0");
    }

    #[test]
    fn run_at_leaves_config_untouched_when_only_missing_binaries_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = config_path(tmp.path());
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/nonexistent/path/claude".into(),
                version_args: vec!["--version".into()],
            },
        );
        save_config(&config_path, &config).unwrap();

        run_at(&config_path).unwrap();

        let reloaded = load_config(&config_path).unwrap();
        assert_eq!(reloaded.models.get("claude").unwrap().version, "1.0");
    }
}
