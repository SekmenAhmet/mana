use crate::agents::KNOWN_CLIS;
use crate::config::{AgentConfig, load_config, save_config};
use crate::project::mana_home;
use dialoguer::MultiSelect;
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves an installed CLI's absolute path and version. Kept separate from
/// the interactive selector below so it's testable against a real,
/// always-present binary.
pub fn resolve_agent(name: &str) -> anyhow::Result<AgentConfig> {
    let path = which::which(name)
        .map_err(|_| anyhow::anyhow!("binaire '{name}' introuvable dans le PATH"))?;
    let version = run_version_check(&path, VERSION_CHECK_TIMEOUT)?;
    Ok(AgentConfig {
        name: name.to_string(),
        version,
        path: path.to_string_lossy().to_string(),
    })
}

fn run_version_check(path: &std::path::Path, timeout: Duration) -> anyhow::Result<String> {
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
                        path.display(),
                        timeout
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn run_version_check_times_out_on_unresponsive_binary() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("slow-agent.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let start = std::time::Instant::now();
        let result = run_version_check(&script, std::time::Duration::from_millis(200));
        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "should time out near the 200ms bound, took {:?}",
            start.elapsed()
        );
    }
}
