use std::path::{Path, PathBuf};

pub struct ProjectPaths {
    pub root: PathBuf,
    pub tasks: PathBuf,
    pub logs: PathBuf,
    pub reviews: PathBuf,
    pub lock_file: PathBuf,
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
        lock_file: root.join("subagent-lock.yaml"),
        root,
    }
}

pub fn ensure_project_structure(paths: &ProjectPaths) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.tasks)?;
    std::fs::create_dir_all(&paths.logs)?;
    std::fs::create_dir_all(&paths.reviews)?;
    if !paths.lock_file.exists() {
        std::fs::write(&paths.lock_file, "")?;
    }
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
            paths.lock_file,
            home.join("projects/my-api/subagent-lock.yaml")
        );
    }

    #[test]
    fn ensure_project_structure_creates_dirs_and_empty_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        ensure_project_structure(&paths).unwrap();
        assert!(paths.tasks.is_dir());
        assert!(paths.logs.is_dir());
        assert!(paths.reviews.is_dir());
        assert!(paths.lock_file.is_file());
    }

    #[test]
    fn ensure_project_structure_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_project_paths(tmp.path(), "demo");
        ensure_project_structure(&paths).unwrap();
        std::fs::write(&paths.lock_file, "existing: content\n").unwrap();
        ensure_project_structure(&paths).unwrap();
        let contents = std::fs::read_to_string(&paths.lock_file).unwrap();
        assert_eq!(contents, "existing: content\n");
    }
}
