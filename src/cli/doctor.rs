use crate::config::{AgentConfig, Config, load_config};
use crate::project::mana_home;
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

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
    current_version_with_timeout(agent, VERSION_CHECK_TIMEOUT)
}

fn current_version_with_timeout(agent: &AgentConfig, timeout: Duration) -> anyhow::Result<String> {
    let path = &agent.path;
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_status) => {
                // Process has exited, read stdout
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    stdout.read_to_string(&mut output)?;
                }
                return Ok(output.trim().to_string());
            }
            None => {
                // Process still running
                if start.elapsed() > timeout {
                    // Timeout exceeded
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow::anyhow!(
                        "subprocess timeout for '{}': exceeded {:?}",
                        path,
                        timeout
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
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
    fn write_version_script(dir: &std::path::Path, name: &str, printed_version: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join(name);
        std::fs::write(&script, format!("#!/bin/sh\necho {printed_version}\n")).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn diagnose_flags_outdated_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_version_script(tmp.path(), "agent.sh", "2.0.0");

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
        let tmp = tempfile::tempdir().unwrap();
        let path = write_version_script(tmp.path(), "agent.sh", "1.0.0");

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

    #[cfg(unix)]
    #[test]
    fn current_version_reads_stdout_on_normal_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_version_script(tmp.path(), "agent.sh", "3.1.4");
        let agent = AgentConfig {
            name: "agent".into(),
            version: "irrelevant".into(),
            path,
        };
        assert_eq!(current_version(&agent).unwrap(), "3.1.4");
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

    #[cfg(unix)]
    #[test]
    fn current_version_times_out_on_unresponsive_binary() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("slow-agent.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let agent = AgentConfig {
            name: "slow".to_string(),
            version: "1.0".to_string(),
            path: script.to_string_lossy().to_string(),
        };

        let start = std::time::Instant::now();
        let result = current_version_with_timeout(&agent, std::time::Duration::from_millis(200));
        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "should time out near the 200ms bound, took {:?}",
            start.elapsed()
        );
    }
}
