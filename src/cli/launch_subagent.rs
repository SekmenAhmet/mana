use crate::agents::autonomous_flag;
use crate::config::{load_config, Config};
use crate::dependencies::unmet_dependencies;
use crate::lock::{append_entry, load_lock, LockEntry};
use crate::log::{append_log, now_iso8601, LogEntry, Status};
use crate::monitor::process_watcher::watch_and_log;
use crate::monitor::pty_listener::spawn_listener;
use crate::project::{ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths};
use crate::prompts::{executor_prompt, reviewer_prompt};
use crate::pty;
use crate::task::{read_task, Role};
use std::io::Write;
use std::str::FromStr;

impl FromStr for Role {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "executor" => Ok(Role::Executor),
            "reviewer" => Ok(Role::Reviewer),
            other => anyhow::bail!("role inconnu: '{other}' (attendu: executor | reviewer)"),
        }
    }
}

fn ensure_agent_registered(config: &Config, agent_cli: &str) -> anyhow::Result<()> {
    if config.models.contains_key(agent_cli) {
        Ok(())
    } else {
        anyhow::bail!("agent '{agent_cli}' non enregistre. Lancez 'mana install' pour l'enregistrer.")
    }
}

pub fn run(agent_cli: &str, role_str: &str, task_uuid: &str, extra_params: &[String]) -> anyhow::Result<()> {
    let role: Role = role_str.parse()?;
    let home = mana_home()?;
    let cwd = std::env::current_dir()?;
    let project_name = project_name_from_dir(&cwd);
    let paths = resolve_project_paths(&home, &project_name);
    ensure_project_structure(&paths)?;

    let task_path = paths.tasks.join(format!("{task_uuid}.md"));
    if !task_path.exists() {
        anyhow::bail!("tache introuvable: {}", task_path.display());
    }
    let task = read_task(&task_path)?;

    let lock = load_lock(&paths.lock_file)?;
    let unmet = unmet_dependencies(&lock, &paths.logs, &task.frontmatter.depends_on)?;
    if !unmet.is_empty() {
        anyhow::bail!("dependances non satisfaites pour {task_uuid}: {}", unmet.join(", "));
    }

    let config = load_config(&home.join("config.yaml"))?;
    ensure_agent_registered(&config, agent_cli)?;

    let flag = autonomous_flag(agent_cli)?;

    let agent_uuid = uuid::Uuid::new_v4().to_string();
    append_entry(&paths.lock_file, &agent_uuid, LockEntry { model: agent_cli.to_string(), role: role.clone(), task_uuid: task_uuid.to_string() })?;

    let log_path = paths.logs.join(format!("{agent_uuid}.jsonl"));
    append_log(&log_path, &LogEntry { status: Status::Running, action: "started".to_string(), timestamp: now_iso8601() })?;

    let review_path = paths.reviews.join(format!("{task_uuid}.md"));
    let prompt = match role {
        Role::Executor => executor_prompt(&task, &task_path),
        Role::Reviewer => reviewer_prompt(&task, &task_path, &review_path),
    };

    let mut args = vec![flag.to_string()];
    args.extend_from_slice(extra_params);
    let mut session = pty::spawn(agent_cli, &args)?;
    session.writer.write_all(prompt.as_bytes())?;
    session.writer.write_all(b"\n")?;

    let listener_reader = session.reader;
    let listener_handle = spawn_listener(listener_reader, log_path.clone());

    let _ = listener_handle.join();
    watch_and_log(session.child, &log_path)?;

    println!("sous-agent {agent_uuid} ({role_str}) termine pour la tache {task_uuid}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Lock;

    #[test]
    fn role_parsing_accepts_known_values_and_rejects_others() {
        assert_eq!("executor".parse::<Role>().unwrap(), Role::Executor);
        assert_eq!("reviewer".parse::<Role>().unwrap(), Role::Reviewer);
        assert!("bogus".parse::<Role>().is_err());
    }

    #[test]
    fn unmet_dependencies_blocks_before_any_spawn() {
        // Reuses dependencies::unmet_dependencies directly (already covered
        // in Task 8); this test documents that launch_subagent must call it
        // before doing anything else. See run()'s early bail on non-empty
        // `unmet`.
        let lock = Lock::new();
        let tmp = tempfile::tempdir().unwrap();
        let unmet = unmet_dependencies(&lock, tmp.path(), &["missing-dep".to_string()]).unwrap();
        assert_eq!(unmet, vec!["missing-dep".to_string()]);
    }

    #[test]
    fn autonomous_flag_rejects_unsupported_cli_before_any_lock_write() {
        assert!(autonomous_flag("gemini").is_err());
    }

    #[test]
    fn ensure_agent_registered_rejects_unknown_agent() {
        let config = crate::config::Config::default();
        assert!(ensure_agent_registered(&config, "claude").is_err());
    }

    #[test]
    fn ensure_agent_registered_accepts_known_agent() {
        let mut config = crate::config::Config::default();
        config.models.insert("claude".to_string(), crate::config::AgentConfig { name: "claude".into(), version: "1.0".into(), path: "/usr/local/bin/claude".into() });
        assert!(ensure_agent_registered(&config, "claude").is_ok());
    }
}
