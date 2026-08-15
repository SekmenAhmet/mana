use crate::task::Task;
use std::path::Path;

/// Orphaned by the v2 launch flow (mana v2, task 2.3): the PM's instructions
/// are now `assets/roles/pm/SKILL.md` installed on disk plus the MCP tool
/// schemas, so nothing calls this. Kept compiling rather than deleted because
/// the whole of `prompts.rs` goes in the 4.5 kill sweep, together with the
/// shell-out protocol it describes -- which design §10 lists as dead.
#[allow(dead_code)]
pub fn pm_prompt(project_name: &str) -> String {
    format!(
        "You are the Main Agent (Project Manager) orchestrated by mana for the project '{project_name}'.\n\
        You never write code yourself. Your role: talk with the user to understand what they need, \
        break the work down into tasks, and delegate each task to a sub-agent via the shell command \
        `mana launch --subagent <cli> --role <executor|reviewer> --assign <task-uuid>`.\n\n\
        To create a task, write a file `.mana/projects/{project_name}/tasks/<uuid>.md` with a YAML \
        frontmatter (id, title, role, depends-on optional) followed by the prompt intended for the sub-agent.\n\n\
        Launch an executor first for each code task, then a reviewer once the executor is done, \
        assigning it the same task. Read the review file written to `.mana/projects/{project_name}/reviews/<uuid>.md` \
        to decide whether the task is complete or needs to be relaunched with corrections.\n\n\
        Start by asking the user questions to properly understand what they want to build."
    )
}

pub fn executor_prompt(task: &Task, task_path: &Path) -> String {
    format!(
        "You are an executor sub-agent orchestrated by mana. Your task is described in {} (id: {}, title: {}).\n\n\
        Do exactly what's asked: write the code, add tests where relevant, verify it builds/passes. \
        When you're done, just stop — don't wait for any confirmation, nobody will respond.\n\n\
        --- Task content ---\n{}",
        task_path.display(),
        task.frontmatter.id,
        task.frontmatter.title,
        task.body,
    )
}

pub fn reviewer_prompt(task: &Task, task_path: &Path, review_path: &Path) -> String {
    format!(
        "You are a reviewer sub-agent orchestrated by mana. Review the work done for the task described in {} \
        (id: {}, title: {}), comparing it against the code changes produced (check the git diff if relevant).\n\n\
        Write your verdict in {}:\n\
        - If everything is correct, write ONLY the line `## Verdict: \u{2705} Validated` — no summary, no extra prose.\n\
        - If you find issues, write `## Verdict: \u{274c} Rejected` followed by a `### Issues found` section \
        with a numbered list, one concrete issue per line.\n\n\
        --- Task content ---\n{}",
        task_path.display(),
        task.frontmatter.id,
        task.frontmatter.title,
        review_path.display(),
        task.body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Role, TaskFrontmatter};
    use std::path::PathBuf;

    fn sample_task() -> Task {
        Task {
            frontmatter: TaskFrontmatter {
                id: "uuid-1".to_string(),
                title: "Test title".to_string(),
                role: Role::Executor,
                depends_on: vec![],
            },
            body: "# Description\n\nDo X.\n".to_string(),
        }
    }

    #[test]
    fn pm_prompt_mentions_project_and_launch_command() {
        let prompt = pm_prompt("my-api");
        assert!(prompt.contains("my-api"));
        assert!(prompt.contains("mana launch --subagent"));
        assert!(prompt.contains("never write code"));
    }

    #[test]
    fn executor_prompt_includes_task_body_and_path() {
        let task = sample_task();
        let path = PathBuf::from("/tmp/tasks/uuid-1.md");
        let prompt = executor_prompt(&task, &path);
        assert!(prompt.contains("Do X."));
        assert!(prompt.contains("uuid-1.md"));
        assert!(prompt.contains("Test title"));
    }

    #[test]
    fn reviewer_prompt_explains_minimal_validated_format() {
        let task = sample_task();
        let task_path = PathBuf::from("/tmp/tasks/uuid-1.md");
        let review_path = PathBuf::from("/tmp/reviews/uuid-1.md");
        let prompt = reviewer_prompt(&task, &task_path, &review_path);
        assert!(prompt.contains("ONLY the line"));
        assert!(prompt.contains("Issues found"));
        assert!(prompt.contains("uuid-1.md"));
    }
}
