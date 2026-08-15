use std::path::{Path, PathBuf};

pub struct ProjectPaths {
    pub root: PathBuf,
    pub tasks: PathBuf,
    pub logs: PathBuf,
    pub reviews: PathBuf,
    pub subagents_file: PathBuf,
}

pub fn mana_home() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
    Ok(home.join(".mana"))
}

pub fn project_name_from_dir(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown-project".to_string())
}

pub fn resolve_project_paths(mana_home: &Path, project_name: &str) -> ProjectPaths {
    let root = mana_home.join("projects").join(project_name);
    ProjectPaths {
        tasks: root.join("tasks"),
        logs: root.join("logs"),
        reviews: root.join("reviews"),
        subagents_file: root.join("subagents.jsonl"),
        root,
    }
}

/// Only creates the directories. `subagents.jsonl` is deliberately not
/// pre-created here: it's an append-only log (`crate::lock::append_record`
/// creates it, and its parent dir, on first write), and `load_registry`
/// already treats a missing file as an empty registry — so there is no
/// "empty but present" state worth writing to disk ahead of time.
pub fn ensure_project_structure(paths: &ProjectPaths) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.tasks)?;
    std::fs::create_dir_all(&paths.logs)?;
    std::fs::create_dir_all(&paths.reviews)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_name_from_dir_takes_basename() {
        let dir = Path::new("/Users/user/projects/my-api");
        assert_eq!(project_name_from_dir(dir), "my-api");
    }

    #[test]
    fn resolve_project_paths_builds_expected_layout() {
        let home = PathBuf::from("/tmp/fake-mana-home");
        let paths = resolve_project_paths(&home, "my-api");
        assert_eq!(paths.root, home.join("projects/my-api"));
        assert_eq!(paths.tasks, home.join("projects/my-api/tasks"));
        assert_eq!(paths.logs, home.join("projects/my-api/logs"));
        assert_eq!(paths.reviews, home.join("projects/my-api/reviews"));
        assert_eq!(
            paths.subagents_file,
            home.join("projects/my-api/subagents.jsonl")
        );
    }

    #[test]
    fn ensure_project_structure_creates_dirs_without_precreating_subagents_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        ensure_project_structure(&paths).unwrap();
        assert!(paths.tasks.is_dir());
        assert!(paths.logs.is_dir());
        assert!(paths.reviews.is_dir());
        assert!(!paths.subagents_file.exists());
    }

    #[test]
    fn ensure_project_structure_is_idempotent_and_preserves_existing_subagents_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        ensure_project_structure(&paths).unwrap();
        std::fs::write(&paths.subagents_file, "{\"agent_id\":\"a\"}\n").unwrap();
        ensure_project_structure(&paths).unwrap();
        let contents = std::fs::read_to_string(&paths.subagents_file).unwrap();
        assert_eq!(contents, "{\"agent_id\":\"a\"}\n");
    }
}
