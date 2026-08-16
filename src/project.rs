use anyhow::Context;
use std::path::{Path, PathBuf};

pub struct ProjectPaths {
    pub root: PathBuf,
    pub tasks: PathBuf,
    pub logs: PathBuf,
    pub reviews: PathBuf,
    pub subagents_file: PathBuf,
}

pub fn mana_home() -> anyhow::Result<PathBuf> {
    // The explicit override comes first: dirs 6 resolves the Windows home
    // through the Known Folder API and ignores USERPROFILE/HOME, so an env
    // redirect of the whole profile no longer moves ~/.mana there. MANA_HOME
    // relocates exactly the directory mana owns — which is also the only
    // hermetic way the CLI tests can point a spawned mana at a fixture home
    // on every platform.
    if let Some(dir) = std::env::var_os("MANA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
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
    // Named one by one rather than through a loop: this is the first thing
    // every command does, so a failure here is the first thing a user sees,
    // and "Permission denied" with no path is the report they cannot act on
    // (#117). The three differ only in which directory could not be made.
    for dir in [&paths.tasks, &paths.logs, &paths.reviews] {
        create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(())
}

// Everything mana keeps under `~/.mana` is private by nature: task files hold
// the full prompt a user typed, logs hold dispatch metadata and failure
// signatures, run records hold a sub-agent's output. `std::fs`'s creating
// helpers take their mode from the process umask instead — 0o022 on most
// distros, which is world-readable — so mana states the mode rather than
// inheriting it. Every create under the state dir goes through the three
// functions below; that is the whole enforcement, and adding a fourth
// `std::fs::write` call site under `~/.mana` is what would break it.
//
// The mode travels *into* `mkdir`/`open` rather than being chmod-ed after, so
// the path never exists with a wider mode for another process to catch. The
// umask still applies on top, but it can only clear bits: the result is always
// a subset of owner-only, never more.
//
// Non-unix is a deliberate no-op — Windows ACLs already inherit from the
// user's profile directory, and there is no mode to set.
//
// ponytail: only fresh creations are covered; a file an older mana left
// world-readable keeps its mode. Re-chmod on open if that ever matters.

/// `std::fs::create_dir_all`, with every directory it creates owner-only.
pub fn create_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    // `recursive` also makes an already-existing directory an `Ok`, and makes
    // the mode apply to every component created on the way down -- `~/.mana`
    // itself is created by the first `projects/<name>/tasks` call.
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// `std::fs::write`, with the file owner-only if this call creates it.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;
    private_options()
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(contents.as_ref())
}

/// `OpenOptions::new().create(true).append(true).open(path)`, with the file
/// owner-only if this call creates it.
pub fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    private_options().append(true).open(path)
}

fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
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

    /// The bound the whole state directory rests on: nothing mana creates
    /// under `~/.mana` is readable by another account, whatever the umask the
    /// process inherited. Asserts the intermediate components too — the
    /// `projects/` directory is created on the way to `tasks/` and would
    /// otherwise be the umask's to decide.
    #[cfg(unix)]
    #[test]
    fn everything_created_under_the_state_dir_is_owner_only() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home/.mana");
        let paths = resolve_project_paths(&home, "demo");
        ensure_project_structure(&paths).unwrap();
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("projects")), 0o700);
        assert_eq!(mode(&paths.tasks), 0o700);
        assert_eq!(mode(&paths.logs), 0o700);
        assert_eq!(mode(&paths.reviews), 0o700);

        let task = paths.tasks.join("task-1.md");
        write(&task, "the full prompt").unwrap();
        assert_eq!(mode(&task), 0o600);

        let log = paths.logs.join("agent-1.jsonl");
        writeln!(open_append(&log).unwrap(), "{{}}").unwrap();
        assert_eq!(mode(&log), 0o600);
    }
}
