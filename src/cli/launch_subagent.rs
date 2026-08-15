use crate::agents::autonomous_flag;
use crate::config::{ensure_agent_registered, load_config};
use crate::dependencies::unmet_dependencies;
use crate::lock::{LockEntry, append_entry, load_lock};
use crate::log::{LogEntry, Status, append_log, now_iso8601};
use crate::monitor::process_watcher::watch_and_log;
use crate::monitor::pty_listener::spawn_listener;
use crate::project::{
    ensure_project_structure, mana_home, project_name_from_dir, resolve_project_paths,
};
use crate::prompts::{executor_prompt, reviewer_prompt};
use crate::pty::{RealSpawner, Spawner};
use crate::task::{Role, Task, read_task};
use std::io::Write;
use std::path::Path;
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

/// Builds the argv passed to the sub-agent CLI: the autonomous-mode flag
/// first, then whatever extra params the caller (the PM's `mana launch
/// --subagent ...` invocation) forwarded verbatim.
fn build_agent_args(flag: &str, extra_params: &[String]) -> Vec<String> {
    let mut args = vec![flag.to_string()];
    args.extend_from_slice(extra_params);
    args
}

/// Picks the executor or reviewer prompt for `task` depending on `role`.
/// A one-line `match`, but naming it means the role→prompt decision has its
/// own test instead of only being exercised as a side effect of `run_at`.
fn build_prompt(role: &Role, task: &Task, task_path: &Path, review_path: &Path) -> String {
    match role {
        Role::Executor => executor_prompt(task, task_path),
        Role::Reviewer => reviewer_prompt(task, task_path, review_path),
    }
}

pub fn run(
    agent_cli: &str,
    role_str: &str,
    task_uuid: &str,
    extra_params: &[String],
) -> anyhow::Result<()> {
    let home = mana_home()?;
    run_at(
        &home,
        &RealSpawner,
        agent_cli,
        role_str,
        task_uuid,
        extra_params,
    )
}

/// Does the actual work of `run`, minus resolving the real `$HOME` and
/// spawning a real PTY subprocess — both are passed in, so this is testable
/// end to end (task/lock/log files really get written, to a tempdir; the
/// "sub-agent" is a `pty::test_support::FakeSpawner`) without needing an
/// installed agent CLI.
fn run_at(
    home: &Path,
    spawner: &dyn Spawner,
    agent_cli: &str,
    role_str: &str,
    task_uuid: &str,
    extra_params: &[String],
) -> anyhow::Result<()> {
    let role: Role = role_str.parse()?;
    let cwd = std::env::current_dir()?;
    let project_name = project_name_from_dir(&cwd);
    let paths = resolve_project_paths(home, &project_name);
    ensure_project_structure(&paths)?;

    let task_path = paths.tasks.join(format!("{task_uuid}.md"));
    if !task_path.exists() {
        anyhow::bail!("tache introuvable: {}", task_path.display());
    }
    let task = read_task(&task_path)?;

    let lock = load_lock(&paths.lock_file)?;
    let unmet = unmet_dependencies(&lock, &paths.logs, &task.frontmatter.depends_on)?;
    if !unmet.is_empty() {
        anyhow::bail!(
            "dependances non satisfaites pour {task_uuid}: {}",
            unmet.join(", ")
        );
    }

    let config = load_config(&home.join("config.yaml"))?;
    ensure_agent_registered(&config, agent_cli)?;

    let flag = autonomous_flag(agent_cli)?;

    let agent_uuid = uuid::Uuid::new_v4().to_string();
    append_entry(
        &paths.lock_file,
        &agent_uuid,
        LockEntry {
            model: agent_cli.to_string(),
            role: role.clone(),
            task_uuid: task_uuid.to_string(),
        },
    )?;

    let log_path = paths.logs.join(format!("{agent_uuid}.jsonl"));
    append_log(
        &log_path,
        &LogEntry {
            status: Status::Running,
            action: "started".to_string(),
            timestamp: now_iso8601(),
        },
    )?;

    let review_path = paths.reviews.join(format!("{task_uuid}.md"));
    let prompt = build_prompt(&role, &task, &task_path, &review_path);

    let args = build_agent_args(flag, extra_params);
    let mut session = spawner.spawn(agent_cli, &args)?;
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
    use crate::config::{AgentConfig, Config, save_config};
    use crate::lock::Lock;
    use crate::pty::test_support::FakeSpawner;
    use crate::task::{Task, TaskFrontmatter, write_task};
    use std::path::PathBuf;

    fn sample_task(role: Role, depends_on: Vec<String>) -> Task {
        Task {
            frontmatter: TaskFrontmatter {
                id: "task-1".to_string(),
                title: "Titre de test".to_string(),
                role,
                depends_on,
            },
            body: "# Consigne\n\nFais le taf.\n".to_string(),
        }
    }

    /// Sets up a fake `$HOME`: project structure + one task file + a
    /// config with `claude` registered. Returns the tempdir (kept alive
    /// for the caller) and the resolved home path.
    fn fake_home_with_task(task: &Task) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project_name = project_name_from_dir(&std::env::current_dir().unwrap());
        let paths = resolve_project_paths(&home, &project_name);
        ensure_project_structure(&paths).unwrap();
        write_task(&paths.tasks.join("task-1.md"), task).unwrap();

        let mut config = Config::default();
        config.models.insert(
            "claude".to_string(),
            AgentConfig {
                name: "claude".into(),
                version: "1.0".into(),
                path: "/usr/local/bin/claude".into(),
            },
        );
        save_config(&home.join("config.yaml"), &config).unwrap();

        (tmp, home)
    }

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
    fn build_agent_args_puts_flag_first_then_extra_params() {
        let extra = vec!["--foo".to_string(), "bar".to_string()];
        assert_eq!(
            build_agent_args("--dangerously-skip-permissions", &extra),
            vec!["--dangerously-skip-permissions", "--foo", "bar"]
        );
    }

    #[test]
    fn build_agent_args_with_no_extra_params() {
        assert_eq!(build_agent_args("--flag", &[]), vec!["--flag"]);
    }

    #[test]
    fn build_prompt_selects_executor_prompt_for_executor_role() {
        let task = sample_task(Role::Executor, vec![]);
        let prompt = build_prompt(
            &Role::Executor,
            &task,
            Path::new("/tmp/tasks/task-1.md"),
            Path::new("/tmp/reviews/task-1.md"),
        );
        assert!(prompt.contains("executor"));
        assert!(prompt.contains("Fais le taf."));
    }

    #[test]
    fn build_prompt_selects_reviewer_prompt_for_reviewer_role() {
        let task = sample_task(Role::Reviewer, vec![]);
        let prompt = build_prompt(
            &Role::Reviewer,
            &task,
            Path::new("/tmp/tasks/task-1.md"),
            Path::new("/tmp/reviews/task-1.md"),
        );
        assert!(prompt.contains("reviewer"));
        assert!(prompt.contains("Verdict"));
    }

    #[test]
    fn run_at_errors_when_task_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run_at(
            tmp.path(),
            &FakeSpawner::new(vec![]),
            "claude",
            "executor",
            "does-not-exist",
            &[],
        );
        assert!(result.unwrap_err().to_string().contains("introuvable"));
    }

    #[test]
    fn run_at_errors_when_agent_not_registered() {
        let task = sample_task(Role::Executor, vec![]);
        let (_tmp, home) = fake_home_with_task(&task);
        // Overwrite with an empty config so "claude" isn't registered.
        save_config(&home.join("config.yaml"), &Config::default()).unwrap();

        let result = run_at(
            &home,
            &FakeSpawner::new(vec![]),
            "claude",
            "executor",
            "task-1",
            &[],
        );
        assert!(result.is_err());
    }

    #[test]
    fn run_at_errors_when_dependencies_unmet() {
        let task = sample_task(Role::Executor, vec!["missing-dep".to_string()]);
        let (_tmp, home) = fake_home_with_task(&task);

        let result = run_at(
            &home,
            &FakeSpawner::new(vec![]),
            "claude",
            "executor",
            "task-1",
            &[],
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("dependances non satisfaites"));
        assert!(message.contains("missing-dep"));
    }

    #[test]
    fn run_at_happy_path_writes_lock_and_log_and_sends_prompt() {
        let task = sample_task(Role::Executor, vec![]);
        let (_tmp, home) = fake_home_with_task(&task);
        let spawner = FakeSpawner::new(vec![]);

        run_at(&home, &spawner, "claude", "executor", "task-1", &[]).unwrap();

        let project_name = project_name_from_dir(&std::env::current_dir().unwrap());
        let paths = resolve_project_paths(&home, &project_name);

        let lock = load_lock(&paths.lock_file).unwrap();
        assert_eq!(lock.len(), 1);
        let entry = lock.values().next().unwrap();
        assert_eq!(entry.model, "claude");
        assert_eq!(entry.task_uuid, "task-1");

        let agent_uuid = lock.keys().next().unwrap();
        let log_path = paths.logs.join(format!("{agent_uuid}.jsonl"));
        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(log_contents.contains("\"status\":\"running\""));
        assert!(log_contents.contains("\"status\":\"done\""));

        let written = spawner.written.lock().unwrap();
        let sent = String::from_utf8_lossy(&written);
        assert!(sent.contains("Fais le taf."));
    }

    #[test]
    fn run_at_forwards_extra_params_and_autonomous_flag_to_the_spawner() {
        let task = sample_task(Role::Reviewer, vec![]);
        let (_tmp, home) = fake_home_with_task(&task);
        let spawner = FakeSpawner::new(vec![]);

        run_at(
            &home,
            &spawner,
            "claude",
            "reviewer",
            "task-1",
            &["--extra".to_string()],
        )
        .unwrap();

        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (cmd, args) = &calls[0];
        assert_eq!(cmd, "claude");
        assert!(args.contains(&"--extra".to_string()));
        assert_eq!(args[0], autonomous_flag("claude").unwrap());
    }
}
