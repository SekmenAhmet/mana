use crate::config::{AgentConfig, Config, load_config};
use crate::project::mana_home;
use crate::subprocess::{VERSION_CHECK_TIMEOUT, capture_version_output};

#[derive(Debug, Clone, PartialEq)]
pub struct DoctorIssue {
    pub agent: String,
    pub problem: String,
}

pub fn diagnose(config: &Config) -> Vec<DoctorIssue> {
    let mut issues = Vec::new();
    for (name, agent) in &config.models {
        if !std::path::Path::new(&agent.path).exists() {
            issues.push(DoctorIssue {
                agent: name.clone(),
                problem: format!("binaire introuvable: {}", agent.path),
            });
            continue;
        }
        if let Ok(current) = current_version(agent)
            && current != agent.version
        {
            issues.push(DoctorIssue {
                agent: name.clone(),
                problem: format!(
                    "version enregistree {} != version actuelle {current}",
                    agent.version
                ),
            });
        }
    }
    issues
}

fn current_version(agent: &AgentConfig) -> anyhow::Result<String> {
    capture_version_output(std::path::Path::new(&agent.path), VERSION_CHECK_TIMEOUT)
}

pub fn run() -> anyhow::Result<()> {
    let home = mana_home()?;
    run_at(&home.join("config.yaml"))
}

fn run_at(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let issues = diagnose(&config);
    if issues.is_empty() {
        println!(
            "mana doctor: tout est en ordre ({} agent(s) enregistre(s))",
            config.models.len()
        );
    } else {
        for issue in issues {
            println!("[{}] {}", issue.agent, issue.problem);
        }
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
            },
        );
        let issues = diagnose(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].agent, "claude");
        assert!(issues[0].problem.contains("introuvable"));
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
            },
        );
        let issues = diagnose(&config);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].problem.contains("1.0.0"));
        assert!(issues[0].problem.contains("2.0.0"));
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
            },
        );
        assert!(diagnose(&config).is_empty());
    }

    #[test]
    fn run_at_reports_ok_for_empty_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        crate::config::save_config(&config_path, &Config::default()).unwrap();
        assert!(run_at(&config_path).is_ok());
    }

    #[test]
    fn run_at_reports_issues_for_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/nonexistent/path/claude".into(),
            },
        );
        crate::config::save_config(&config_path, &config).unwrap();
        assert!(run_at(&config_path).is_ok());
    }
}
