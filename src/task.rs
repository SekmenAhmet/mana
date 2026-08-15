use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Executor,
    Reviewer,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TaskFrontmatter {
    pub id: String,
    pub title: String,
    pub role: Role,
    #[serde(rename = "depends-on", default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub frontmatter: TaskFrontmatter,
    pub body: String,
}

pub fn parse_task_file(contents: &str) -> anyhow::Result<Task> {
    let contents = contents.trim_start();
    if !contents.starts_with("---") {
        anyhow::bail!("task file missing frontmatter delimiter");
    }
    let rest = &contents[3..];
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("task file missing closing frontmatter delimiter"))?;
    let yaml_block = &rest[..end];
    let after = &rest[end + 4..];
    let body = after.trim_start_matches('\n').to_string();
    let frontmatter: TaskFrontmatter = serde_yaml::from_str(yaml_block)?;
    Ok(Task { frontmatter, body })
}

#[allow(dead_code)]
pub fn render_task_file(task: &Task) -> String {
    let yaml = serde_yaml::to_string(&task.frontmatter).unwrap_or_default();
    format!("---\n{yaml}---\n\n{}", task.body)
}

pub fn read_task(path: &Path) -> anyhow::Result<Task> {
    let contents = std::fs::read_to_string(path)?;
    parse_task_file(&contents)
}

#[allow(dead_code)]
pub fn write_task(path: &Path, task: &Task) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, render_task_file(task))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"---
id: f9e8d7c6-b5a4-3210-fedc-ba9876543210
title: Implement the authentication endpoint
role: executor
depends-on:
  - a1b2c3d4-e5f6-7890-abcd-ef1234567890
---

# Implement the POST /auth/login endpoint

Prompt content.
"#;

    #[test]
    fn parses_frontmatter_and_body() {
        let task = parse_task_file(EXAMPLE).unwrap();
        assert_eq!(task.frontmatter.id, "f9e8d7c6-b5a4-3210-fedc-ba9876543210");
        assert_eq!(task.frontmatter.role, Role::Executor);
        assert_eq!(
            task.frontmatter.depends_on,
            vec!["a1b2c3d4-e5f6-7890-abcd-ef1234567890"]
        );
        assert!(task.body.starts_with("# Implement"));
    }

    #[test]
    fn parses_task_with_no_dependencies() {
        let contents = "---\nid: abc\ntitle: Test\nrole: reviewer\n---\n\nBody text\n";
        let task = parse_task_file(contents).unwrap();
        assert!(task.frontmatter.depends_on.is_empty());
        assert_eq!(task.frontmatter.role, Role::Reviewer);
    }

    #[test]
    fn missing_frontmatter_errors() {
        let result = parse_task_file("# no frontmatter here\n");
        assert!(result.is_err());
    }

    #[test]
    fn render_then_parse_roundtrips() {
        let task = Task {
            frontmatter: TaskFrontmatter {
                id: "uuid-1".to_string(),
                title: "Title".to_string(),
                role: Role::Executor,
                depends_on: vec!["uuid-0".to_string()],
            },
            body: "# Body\n\nDetail.\n".to_string(),
        };
        let rendered = render_task_file(&task);
        let parsed = parse_task_file(&rendered).unwrap();
        assert_eq!(parsed, task);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tasks/uuid-1.md");
        let task = Task {
            frontmatter: TaskFrontmatter {
                id: "uuid-1".to_string(),
                title: "Title".to_string(),
                role: Role::Reviewer,
                depends_on: vec![],
            },
            body: "Body\n".to_string(),
        };
        write_task(&path, &task).unwrap();
        let read_back = read_task(&path).unwrap();
        assert_eq!(read_back, task);
    }
}
