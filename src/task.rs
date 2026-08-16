//! Task files: a TOML frontmatter block fenced by `+++`, then the PM's brief
//! as free markdown.
//!
//! `+++`/TOML rather than `---`/YAML because mana's every other file --
//! config, catalogue, `Cargo.toml` -- is TOML, and `serde_yaml` is archived
//! upstream. The `+++` fence is the convention Hugo/Zola already established
//! for exactly this (TOML where the markdown world defaults to YAML), so a
//! reader who has seen frontmatter before does not have to be told.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Opens and closes the frontmatter block.
const FENCE: &str = "+++";

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
    if !contents.starts_with(FENCE) {
        // A `---` opener is not a generic syntax error, it is the previous
        // format: name it, because "missing frontmatter delimiter" on a file
        // that visibly *has* one sends the reader looking for a typo that
        // isn't there. mana never wrote a released YAML task file, so there is
        // nothing to migrate -- rewriting the two fences and the block between
        // them is the whole fix.
        if contents.starts_with("---") {
            anyhow::bail!(
                "task file uses the old '---' YAML frontmatter; mana reads TOML frontmatter \
                 fenced by '+++' now. Rewrite the block (e.g. `title: X` -> `title = \"X\"`) \
                 or delete the file and have the PM recreate the task."
            );
        }
        anyhow::bail!("task file missing frontmatter delimiter (expected a leading '{FENCE}')");
    }
    let rest = &contents[FENCE.len()..];
    let end = rest
        .find(&format!("\n{FENCE}"))
        .ok_or_else(|| anyhow::anyhow!("task file missing closing '{FENCE}' delimiter"))?;
    let toml_block = &rest[..end];
    let after = &rest[end + 1 + FENCE.len()..];
    let body = after.trim_start_matches('\n').to_string();
    let frontmatter: TaskFrontmatter = toml::from_str(toml_block)?;
    Ok(Task { frontmatter, body })
}

pub fn render_task_file(task: &Task) -> anyhow::Result<String> {
    let frontmatter = toml::to_string(&task.frontmatter)?;
    Ok(format!("{FENCE}\n{frontmatter}{FENCE}\n\n{}", task.body))
}

pub fn read_task(path: &Path) -> anyhow::Result<Task> {
    let contents = std::fs::read_to_string(path)?;
    parse_task_file(&contents)
}

pub fn write_task(path: &Path, task: &Task) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::project::create_dir_all(parent)?;
    }
    crate::project::write(path, render_task_file(task)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"+++
id = "f9e8d7c6-b5a4-3210-fedc-ba9876543210"
title = "Implement the authentication endpoint"
role = "executor"
depends-on = ["a1b2c3d4-e5f6-7890-abcd-ef1234567890"]
+++

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
        let contents =
            "+++\nid = \"abc\"\ntitle = \"Test\"\nrole = \"reviewer\"\n+++\n\nBody text\n";
        let task = parse_task_file(contents).unwrap();
        assert!(task.frontmatter.depends_on.is_empty());
        assert_eq!(task.frontmatter.role, Role::Reviewer);
        assert_eq!(task.body, "Body text\n");
    }

    #[test]
    fn missing_frontmatter_errors() {
        let result = parse_task_file("# no frontmatter here\n");
        assert!(result.is_err());
    }

    /// A pre-v2 task file must not read as "syntax error": the fence changed,
    /// and the message has to say so or the reader hunts for a typo.
    #[test]
    fn legacy_yaml_frontmatter_names_the_format_change() {
        let legacy = "---\nid: abc\ntitle: Test\nrole: reviewer\n---\n\nBody\n";
        let message = parse_task_file(legacy).unwrap_err().to_string();
        assert!(message.contains("YAML"), "{message}");
        assert!(message.contains("+++"), "{message}");
    }

    #[test]
    fn render_writes_toml_frontmatter_between_plus_fences() {
        let task = Task {
            frontmatter: TaskFrontmatter {
                id: "uuid-1".to_string(),
                title: "Title".to_string(),
                role: Role::Executor,
                depends_on: vec![],
            },
            body: "Body\n".to_string(),
        };
        let rendered = render_task_file(&task).unwrap();
        assert!(rendered.starts_with("+++\n"), "{rendered}");
        assert!(rendered.contains("id = \"uuid-1\""), "{rendered}");
        // Not YAML: `id: uuid-1` would parse as neither TOML nor a fence.
        assert!(!rendered.contains("id: "), "{rendered}");
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
        let rendered = render_task_file(&task).unwrap();
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
