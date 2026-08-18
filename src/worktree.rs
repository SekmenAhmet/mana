//! Git worktree isolation for write-role sub-agents (spec §9).
//!
//! One worktree + one branch per task, created from the project repo and
//! parked under `~/.mana/worktrees/`. Executors run in parallel on the same
//! project, so they cannot share a checkout: without this, two agents editing
//! the same file are a race, and "what did this agent change" has no answer.
//! The branch is the deliverable — the reviewer reads `base_ref..HEAD` and
//! the PM merges it — so cleanup removes the worktree and keeps the branch.
//!
//! Everything here shells out to `git` (argv arrays, never a shell) rather
//! than linking a git library: worktrees are a porcelain feature, the user's
//! own git is the one whose behavior the project is configured for, and a
//! failure carries git's own stderr, which is what makes a broken dispatch
//! debuggable.

use crate::project::project_name_from_dir;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Identity written into the worktree's own config so the executor's
/// `git commit` succeeds on a machine with no global git identity (CI, a
/// fresh container, a user who only ever commits through a GUI). Not a real
/// mailbox: it must be syntactically valid and obviously non-human, since it
/// ends up in the authorship of every agent commit.
const COMMIT_USER_NAME: &str = "mana";
const COMMIT_USER_EMAIL: &str = "mana@localhost";

/// How much of the task id names the worktree directory. Windows still caps
/// paths at 260 chars by default, and the budget is spent by
/// `~/.mana/worktrees/<project>/` plus whatever the checkout itself nests —
/// a full 36-char UUID in the middle of that is the difference between a
/// working dispatch and a checkout that fails halfway. 8 hex chars of a v4
/// UUID collide at ~65k tasks per project (birthday bound), and a collision
/// is not silent: `create` recreates the directory from scratch.
const TASK_DIR_CHARS: usize = 8;

/// Where a task's worktree lives, what branch it is on, and the commit it
/// started from. `base_ref` is captured at creation and never recomputed:
/// the reviewer diffs `base_ref..HEAD`, and the project's own HEAD moves
/// while the executor works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
}

/// Gate for write-role dispatch. Isolation *is* the worktree, so there is no
/// degraded mode to fall back to on a non-git project — mana refuses and says
/// what to do about it (spec §9). Read-only roles never call this.
pub fn ensure_git_repo(project_root: &Path) -> anyhow::Result<()> {
    let probe = git_output(project_root, &["rev-parse", "--is-inside-work-tree"])?;
    if !probe.status.success() {
        anyhow::bail!(
            "{} is not a git repository. mana runs write roles in an isolated \
             git worktree, so the project needs a repo first: run `git init` \
             there, make an initial commit, then dispatch again.",
            project_root.display()
        );
    }
    // `--is-inside-work-tree` answers "false" (exit 0) in a bare repo: there
    // is nothing to branch a checkout off of, and a bare repo's `core.bare`
    // would leak into every worktree once the extension below is on.
    if probe_stdout(&probe) != "true" {
        anyhow::bail!(
            "{} is a bare git repository. mana needs a normal checkout to \
             create task worktrees from.",
            project_root.display()
        );
    }
    Ok(())
}

/// Creates (or recreates) the isolated worktree for `task_id`.
///
/// Recreation is deliberate: a retried task must start from `base_ref`, not
/// on top of the previous attempt, and leftovers from a crashed run must not
/// turn the next dispatch into a git error the user has to clean up by hand.
pub fn create(
    project_root: &Path,
    mana_home: &Path,
    task_id: &str,
) -> anyhow::Result<WorktreeInfo> {
    validate_task_id(task_id)?;
    ensure_git_repo(project_root)?;

    let head = git_output(project_root, &["rev-parse", "--verify", "HEAD"])?;
    if !head.status.success() {
        anyhow::bail!(
            "{} has no commits yet, so there is nothing to branch a task \
             worktree from. Make an initial commit, then dispatch again. \
             (git: {})",
            project_root.display(),
            String::from_utf8_lossy(&head.stderr).trim()
        );
    }
    let base_ref = probe_stdout(&head).to_string();

    let path = worktree_path(mana_home, project_root, task_id);
    let branch = branch_name(task_id);

    // Before anything else: a worktree still registered at this path pins the
    // branch (git refuses to force-update a branch checked out in a worktree)
    // and blocks `worktree add` (the directory "already exists").
    remove_at(project_root, &path)?;
    if let Some(parent) = path.parent() {
        crate::project::create_dir_all(parent)?;
    }

    enable_worktree_config(project_root)?;

    // Force, because a retry of the same task reuses the branch name and must
    // start from base again — reviewing a diff that still contains the failed
    // attempt is worse than useless. Nothing is destroyed: the old tip stays
    // reachable through the branch's reflog until gc expires it.
    git(project_root, &["branch", "--force", &branch, &base_ref])?;
    git(
        project_root,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            path.as_os_str(),
            OsStr::new(&branch),
        ],
    )?;

    configure_commit_identity(&path)?;

    Ok(WorktreeInfo {
        path,
        branch,
        base_ref,
    })
}

/// The branch a task's work lands on. Public because `mcp` reports it to the
/// PM at dispatch time -- before `create` has run, and therefore before there
/// is a `WorktreeInfo` to read it off (#36). Deterministic from the task id
/// for exactly that reason: a retry rebuilds the same branch from a fresh
/// base, so the name mana promised at dispatch is still the name afterwards.
pub fn branch_name(task_id: &str) -> String {
    format!("mana/{task_id}")
}

/// Where every task worktree for one project lives.
///
/// Additive helper (task 4.3): `mana doctor` lists this directory looking for
/// leftovers, and the layout has to be stated in exactly one place or the
/// two will drift the first time either changes.
pub fn worktrees_dir(mana_home: &Path, project_name: &str) -> PathBuf {
    mana_home.join("worktrees").join(project_name)
}

/// The directory name a task's worktree takes. Public for the same reason
/// (task 4.3): doctor has to map a directory back to the dispatch that made
/// it, and truncating the id by hand at the call site would silently stop
/// matching the day `TASK_DIR_CHARS` changes.
pub fn task_dir_name(task_id: &str) -> String {
    task_id.chars().take(TASK_DIR_CHARS).collect()
}

/// Where a task's worktree is, or will be once `create` runs. Public for the
/// same reason `branch_name` is: `mcp` tells the PM where its dispatch will
/// work before the directory exists (#36), and a second builder for this path
/// would be a second answer to "where is my work".
pub fn worktree_path(mana_home: &Path, project_root: &Path, task_id: &str) -> PathBuf {
    worktrees_dir(mana_home, &project_name_from_dir(project_root)).join(task_dir_name(task_id))
}

/// Task ids reach this module from task files and, since the MCP server
/// landed, straight from the PM over MCP — untrusted enough that a `..` or a
/// separator would otherwise build a path outside the worktree root and a
/// branch name outside the `mana/` namespace.
///
/// Visible to the crate because `dispatch_reviewer` needs the same rule for
/// the same reason and creates no worktree to get it from `create` (#187),
/// and a second copy of the character set would be a second answer to "what
/// is a task id".
pub(crate) fn validate_task_id(task_id: &str) -> anyhow::Result<()> {
    let acceptable = !task_id.is_empty()
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !acceptable {
        anyhow::bail!("invalid task id {task_id:?}: expected only letters, digits, '-' or '_'");
    }
    Ok(())
}

/// Best-effort teardown: `git worktree remove`, then the filesystem, then
/// `git worktree prune` for the `.git/worktrees/<id>` admin entry.
///
/// The removal itself is allowed to fail, and does in exactly the cases that
/// matter — git validates the worktree before removing it and gives up with
/// "validation failed ... .git does not exist" the moment someone deleted
/// part of the directory. The two remaining steps finish the job, so only
/// they are fatal.
///
/// Addressed by path, never by `WorktreeInfo`: `create` calls it on a path it
/// has not built a `WorktreeInfo` for yet, and `mana doctor --prune` finds
/// leftovers by walking `worktrees_dir` and knows nothing but a path. The
/// branch never enters into it — it carries the executor's commits, which the
/// reviewer and then the PM still need, so teardown must not touch it.
pub fn remove_at(project_root: &Path, path: &Path) -> anyhow::Result<()> {
    // Doubled `--force` is git's documented way to remove a *locked*
    // worktree; a single one only covers a dirty one.
    let _ = git(
        project_root,
        &[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            OsStr::new("--force"),
            path.as_os_str(),
        ],
    );
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    // Runs unconditionally: prune is what drops the admin entry of a worktree
    // whose directory is gone, which is the state the two steps above leave
    // behind whenever the remove failed.
    git(project_root, &["worktree", "prune"])?;
    Ok(())
}

/// Turns on `extensions.worktreeConfig` so per-worktree config exists at all,
/// and moves `core.worktree` out of the shared config if it is there.
///
/// That move is not optional. Without the extension, git applies a shared
/// `core.worktree` to the main worktree only; with it, the exception is gone
/// and every linked worktree inherits it — verified against git 2.50: the
/// worktree's own toplevel becomes the project checkout, so the "isolated"
/// executor would be editing the shared tree. Repos hit by this are exactly
/// the ones a user is likely to dispatch on: submodule checkouts set
/// `core.worktree`. git-worktree(1) "CONFIGURATION FILE" prescribes this move.
fn enable_worktree_config(project_root: &Path) -> anyhow::Result<()> {
    let existing = git_output(
        project_root,
        &["config", "--local", "--get", "extensions.worktreeConfig"],
    )?;
    if probe_stdout(&existing) != "true"
        && let Err(lost) = git(
            project_root,
            &["config", "--local", "extensions.worktreeConfig", "true"],
        )
    {
        // `git config` takes `.git/config.lock` and does not retry, so in a
        // project's first parallel wave every dispatch but one loses this
        // write -- and used to fail the dispatch over a lock file (#201).
        // Losing the write is not losing the outcome: the value this call
        // wanted is the value the winner wrote. Only a repo that still does
        // not have the extension is a real failure, and then the original
        // git error is the one worth reporting.
        let settled = git_output(
            project_root,
            &["config", "--local", "--get", "extensions.worktreeConfig"],
        )?;
        if probe_stdout(&settled) != "true" {
            return Err(lost);
        }
    }

    let shared_worktree = git_output(
        project_root,
        &["config", "--local", "--get", "core.worktree"],
    )?;
    if shared_worktree.status.success() {
        let value = probe_stdout(&shared_worktree).to_string();
        // Writing with `--worktree` from the project root targets the *main*
        // worktree's config, which is where the value belonged all along.
        git(
            project_root,
            &["config", "--worktree", "core.worktree", &value],
        )?;
        git(
            project_root,
            &["config", "--local", "--unset", "core.worktree"],
        )?;
    }
    Ok(())
}

/// Gives the worktree its own commit identity.
///
/// The executor CLI runs `git commit` itself, inside this directory, on a
/// machine that may have no identity anywhere — git then either invents one
/// from username@hostname or refuses outright, and a dispatch dies on a
/// configuration problem that has nothing to do with the task. Scoped with
/// `--worktree` (i.e. `.git/worktrees/<id>/config.worktree`) rather than
/// `--local`, because `--local` is the *project's* config: mana would be
/// rewriting the identity the user commits under, and the setting would
/// outlive the worktree. This file dies with the worktree.
fn configure_commit_identity(worktree: &Path) -> anyhow::Result<()> {
    git(
        worktree,
        &["config", "--worktree", "user.name", COMMIT_USER_NAME],
    )?;
    git(
        worktree,
        &["config", "--worktree", "user.email", COMMIT_USER_EMAIL],
    )?;
    // A user with `commit.gpgsign = true` globally would otherwise have every
    // agent commit try to sign as mana with a key that isn't theirs — at best
    // an error, at worst a passphrase prompt that hangs a headless executor
    // until its timeout.
    git(
        worktree,
        &["config", "--worktree", "commit.gpgsign", "false"],
    )?;
    Ok(())
}

fn probe_stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap_or("").trim()
}

/// Runs git and hands back the raw outcome; only a git that cannot be
/// launched at all is an error. For probes whose non-zero exit is an answer
/// rather than a failure (`config --get` on an unset key, `rev-parse` in a
/// non-repo).
fn git_output<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> anyhow::Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        // Windows caps paths at 260 chars unless git is told otherwise, and
        // mana's own checkouts are the deep ones. Unknown config keys are
        // ignored by git elsewhere, so this needs no platform branch.
        .arg("-c")
        .arg("core.longpaths=true")
        .args(args)
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot run `git {}` in {}: {e}",
                render_args(args),
                cwd.display()
            )
        })
}

/// Same, for calls whose failure is a failure. git's stderr goes into the
/// error: a swallowed git error surfaces later as a dispatch that "just
/// didn't work", with nothing to debug from.
fn git<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> anyhow::Result<String> {
    let output = git_output(cwd, args)?;
    if !output.status.success() {
        anyhow::bail!(
            "`git {}` failed in {} ({}): {}",
            render_args(args),
            cwd.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(probe_stdout(&output).to_string())
}

fn render_args<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_ID: &str = "3f2a1b6c-9d4e-4a7b-8c1d-2e5f0a9b8c7d";

    /// A throwaway project repo plus a throwaway `~/.mana`, wired so that no
    /// git identity is reachable from the fixture: `HOME` (and `USERPROFILE`,
    /// its Windows counterpart) point at an empty directory,
    /// `GIT_CONFIG_GLOBAL` at a file that does not exist, and
    /// `GIT_CONFIG_NOSYSTEM` cuts off the system config. Every fixture git
    /// call therefore has exactly one possible source of identity — the repo —
    /// which is what makes the commit test a proof instead of a coincidence.
    struct Fixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
        mana_home: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let project = tmp.path().join("project");
            let mana_home = tmp.path().join("mana-home");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&project).unwrap();
            let fixture = Fixture {
                _tmp: tmp,
                home,
                project,
                mana_home,
            };
            // Pinned default branch: git otherwise picks one from config (or
            // warns), and these tests read branch state.
            fixture.git_ok(
                &fixture.project.clone(),
                &["-c", "init.defaultBranch=main", "init"],
            );
            // Windows git defaults to core.autocrlf=true, which renormalizes
            // the LF fixtures on checkout; a later `add -A` in the worktree
            // then stages a phantom README change and the base..HEAD diff
            // grows an entry no test wrote. Pin it off for determinism.
            fixture.git_ok(
                &fixture.project.clone(),
                &["config", "core.autocrlf", "false"],
            );
            fixture
        }

        fn cmd(&self, cwd: &Path, args: &[&str]) -> Command {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(cwd)
                .args(args)
                .env("HOME", &self.home)
                .env("USERPROFILE", &self.home)
                .env("GIT_CONFIG_GLOBAL", self.home.join("absent-gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env_remove("GIT_AUTHOR_NAME")
                .env_remove("GIT_AUTHOR_EMAIL")
                .env_remove("GIT_COMMITTER_NAME")
                .env_remove("GIT_COMMITTER_EMAIL");
            cmd
        }

        fn run(&self, cwd: &Path, args: &[&str]) -> Output {
            self.cmd(cwd, args).output().unwrap()
        }

        fn git_ok(&self, cwd: &Path, args: &[&str]) -> String {
            let output = self.run(cwd, args);
            assert!(
                output.status.success(),
                "git {args:?} failed in {}: {}",
                cwd.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        /// Commits everything in the project checkout and returns the new sha.
        /// The identity comes from the environment, never from
        /// `git config user.*`: the isolation assertions below require the
        /// project repo to hold no identity of its own.
        fn seed_commit(&self, file: &str, contents: &str) -> String {
            std::fs::write(self.project.join(file), contents).unwrap();
            self.git_ok(&self.project, &["add", "-A"]);
            let output = self
                .cmd(&self.project, &["commit", "-m", file])
                .env("GIT_AUTHOR_NAME", "seed")
                .env("GIT_AUTHOR_EMAIL", "seed@example.invalid")
                .env("GIT_COMMITTER_NAME", "seed")
                .env("GIT_COMMITTER_EMAIL", "seed@example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "seed commit failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            self.git_ok(&self.project, &["rev-parse", "HEAD"])
        }

        fn worktree_admin_entries(&self) -> usize {
            let admin = self.project.join(".git").join("worktrees");
            match std::fs::read_dir(&admin) {
                Ok(entries) => entries.count(),
                Err(_) => 0,
            }
        }

        fn registered_worktrees(&self) -> usize {
            self.git_ok(&self.project, &["worktree", "list", "--porcelain"])
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count()
        }
    }

    #[test]
    fn worktree_path_keeps_only_the_first_eight_task_id_chars() {
        let project = Path::new("/home/u/code/my-api");
        let path = worktree_path(Path::new("/home/u/.mana"), project, TASK_ID);
        // The project component is `project_name_from_dir`'s, fingerprint
        // included (#33) -- spelled out here rather than hard-coded, because
        // what this test pins is the *task* component's width.
        assert_eq!(
            path,
            Path::new("/home/u/.mana/worktrees")
                .join(project_name_from_dir(project))
                .join("3f2a1b6c")
        );
    }

    #[test]
    fn create_rejects_task_ids_that_would_escape_the_worktree_root() {
        let fixture = Fixture::new();
        for hostile in ["../../etc", "a/b", "", "id with spaces"] {
            let error = match create(&fixture.project, &fixture.mana_home, hostile) {
                Ok(info) => panic!("task id {hostile:?} should have been rejected, got {info:?}"),
                Err(error) => error.to_string(),
            };
            // Specifically rejected as an id — not incidentally failing later
            // on some git call that happened to choke on it.
            assert!(error.contains("invalid task id"), "got: {error}");
        }
    }

    #[test]
    fn ensure_git_repo_refuses_a_directory_that_is_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let error = ensure_git_repo(tmp.path()).unwrap_err().to_string();
        assert!(
            error.contains("not a git repository") && error.contains("git init"),
            "refusal should tell the user to init a repo, got: {error}"
        );
    }

    #[test]
    fn create_refuses_a_write_role_dispatch_on_a_non_git_project() {
        let tmp = tempfile::tempdir().unwrap();
        let mana_home = tmp.path().join("mana-home");
        let plain_dir = tmp.path().join("plain");
        std::fs::create_dir_all(&plain_dir).unwrap();
        let error = create(&plain_dir, &mana_home, TASK_ID)
            .unwrap_err()
            .to_string();
        assert!(error.contains("git init"), "got: {error}");
    }

    #[test]
    fn create_refuses_a_repo_with_no_commits_yet() {
        let fixture = Fixture::new();
        let error = create(&fixture.project, &fixture.mana_home, TASK_ID)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no commits yet"), "got: {error}");
    }

    #[test]
    fn commit_inside_the_worktree_uses_manas_identity_and_lands_in_the_base_diff() {
        let fixture = Fixture::new();
        let base = fixture.seed_commit("README.md", "base\n");

        let info = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();
        assert_eq!(info.branch, format!("mana/{TASK_ID}"));
        assert_eq!(info.base_ref, base);
        assert_eq!(
            info.path,
            fixture
                .mana_home
                .join("worktrees")
                .join(project_name_from_dir(&fixture.project))
                .join("3f2a1b6c")
        );

        // Control: same environment, project checkout, no worktree config.
        // `user.useConfigOnly` stops git from inventing an identity out of
        // username@hostname (it does, silently, as of 2.50) — without this the
        // commit below would succeed even if mana had configured nothing.
        let uncommittable = fixture.run(
            &fixture.project,
            &[
                "-c",
                "user.useConfigOnly=true",
                "commit",
                "--allow-empty",
                "-m",
                "control",
            ],
        );
        assert!(
            !uncommittable.status.success(),
            "the fixture is supposed to have no reachable identity, but a \
             commit in the project checkout succeeded"
        );

        std::fs::write(info.path.join("feature.txt"), "work\n").unwrap();
        fixture.git_ok(&info.path, &["add", "-A"]);
        fixture.git_ok(
            &info.path,
            &[
                "-c",
                "user.useConfigOnly=true",
                "commit",
                "-m",
                "mana: executor commit",
            ],
        );

        assert_eq!(
            fixture.git_ok(&info.path, &["log", "-1", "--format=%an|%ae|%cn|%ce"]),
            format!(
                "{COMMIT_USER_NAME}|{COMMIT_USER_EMAIL}|{COMMIT_USER_NAME}|{COMMIT_USER_EMAIL}"
            )
        );

        let changed = fixture.git_ok(
            &info.path,
            &["diff", "--name-only", &format!("{}..HEAD", info.base_ref)],
        );
        assert_eq!(changed, "feature.txt");

        // The project's own identity is untouched: mana wrote into the
        // worktree's config, not the repo's.
        assert!(
            !fixture
                .run(&fixture.project, &["config", "--get", "user.name"])
                .status
                .success(),
            "mana leaked an identity into the project repo"
        );
    }

    #[test]
    fn remove_at_removes_the_worktree_and_its_admin_entry_but_keeps_the_branch() {
        let fixture = Fixture::new();
        fixture.seed_commit("README.md", "base\n");
        let info = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();

        std::fs::write(info.path.join("feature.txt"), "work\n").unwrap();
        fixture.git_ok(&info.path, &["add", "-A"]);
        fixture.git_ok(&info.path, &["commit", "-m", "mana: executor commit"]);
        let tip = fixture.git_ok(&info.path, &["rev-parse", "HEAD"]);

        remove_at(&fixture.project, &info.path).unwrap();

        assert!(!info.path.exists(), "worktree directory survived cleanup");
        assert_eq!(
            fixture.worktree_admin_entries(),
            0,
            "stale .git/worktrees entry left behind"
        );
        assert_eq!(
            fixture.registered_worktrees(),
            1,
            "git still lists a task worktree besides the project checkout"
        );
        // The branch is the work product — cleanup must not touch it.
        assert_eq!(
            fixture.git_ok(&fixture.project, &["rev-parse", &info.branch]),
            tip
        );
    }

    #[test]
    fn remove_at_survives_a_worktree_directory_deleted_by_hand() {
        let fixture = Fixture::new();
        fixture.seed_commit("README.md", "base\n");
        let info = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();

        // Half-deleted, the state git refuses to remove: the directory is
        // still there, its `.git` pointer is not.
        std::fs::remove_file(info.path.join(".git")).unwrap();

        remove_at(&fixture.project, &info.path).unwrap();
        assert!(!info.path.exists());
        assert_eq!(fixture.worktree_admin_entries(), 0);
        assert_eq!(fixture.registered_worktrees(), 1);
    }

    #[test]
    fn create_recreates_over_a_half_deleted_worktree() {
        let fixture = Fixture::new();
        fixture.seed_commit("README.md", "base\n");
        let first = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();
        std::fs::remove_file(first.path.join(".git")).unwrap();
        std::fs::write(first.path.join("leftover.txt"), "junk\n").unwrap();

        let second = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();

        assert_eq!(second.path, first.path);
        assert!(
            !second.path.join("leftover.txt").exists(),
            "recreated worktree still holds the previous run's leftovers"
        );
        assert_eq!(fixture.registered_worktrees(), 2);
        // Still a real, usable worktree — not just a directory.
        assert_eq!(
            fixture.git_ok(&second.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            second.branch
        );
    }

    #[test]
    fn create_resets_an_existing_task_branch_to_the_current_base() {
        let fixture = Fixture::new();
        fixture.seed_commit("README.md", "base\n");
        let first = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();
        std::fs::write(first.path.join("attempt.txt"), "first try\n").unwrap();
        fixture.git_ok(&first.path, &["add", "-A"]);
        fixture.git_ok(&first.path, &["commit", "-m", "mana: first attempt"]);
        let abandoned = fixture.git_ok(&first.path, &["rev-parse", "HEAD"]);

        // The project moves on while the task is retried.
        let new_base = fixture.seed_commit("other.md", "meanwhile\n");
        let second = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();

        assert_eq!(second.base_ref, new_base);
        assert_eq!(
            fixture.git_ok(&second.path, &["rev-parse", "HEAD"]),
            new_base,
            "retry did not start from the current base"
        );
        assert!(!second.path.join("attempt.txt").exists());
        // Discarded, not destroyed: the abandoned tip is still an object in
        // the repo (and in the branch's reflog) until gc expires it.
        assert_eq!(
            fixture.git_ok(&fixture.project, &["cat-file", "-t", &abandoned]),
            "commit"
        );
    }

    #[test]
    fn create_moves_core_worktree_out_of_the_shared_config() {
        let fixture = Fixture::new();
        fixture.seed_commit("README.md", "base\n");
        // What a submodule checkout looks like: `core.worktree` in the config
        // every worktree shares. Left there, enabling extensions.worktreeConfig
        // points the task worktree back at the project checkout.
        let project = fixture.project.to_string_lossy().to_string();
        fixture.git_ok(&fixture.project, &["config", "core.worktree", &project]);

        let info = create(&fixture.project, &fixture.mana_home, TASK_ID).unwrap();

        let toplevel = fixture.git_ok(&info.path, &["rev-parse", "--show-toplevel"]);
        assert_eq!(
            std::fs::canonicalize(&toplevel).unwrap(),
            std::fs::canonicalize(&info.path).unwrap(),
            "task worktree resolves to the project checkout — isolation lost"
        );
        assert!(
            !fixture
                .run(
                    &fixture.project,
                    &["config", "--local", "--get", "core.worktree"]
                )
                .status
                .success(),
            "core.worktree still in the shared config"
        );
        // Moved, not dropped: the project checkout keeps its own setting.
        assert_eq!(
            std::fs::canonicalize(
                fixture.git_ok(&fixture.project, &["rev-parse", "--show-toplevel"])
            )
            .unwrap(),
            std::fs::canonicalize(&fixture.project).unwrap()
        );
    }

    /// #201: every dispatch in a parallel wave runs `enable_worktree_config`,
    /// and `git config --local` takes `.git/config.lock` without retrying. The
    /// losers used to bail, so a project's first parallel wave lost half its
    /// dispatches to a git lock file. Losing the write is not losing the
    /// outcome: the value the loser wanted is the value the winner wrote.
    ///
    /// Ten rounds because one is a coin toss and the failure is what is being
    /// pinned; the measured rate before the fix was 25/25 at this concurrency.
    #[test]
    fn a_lost_race_to_set_the_worktree_extension_is_not_a_failed_dispatch() {
        use std::sync::{Arc, Barrier};

        for round in 0..10 {
            let fixture = Fixture::new();
            let project = Arc::new(fixture.project.clone());
            let gate = Arc::new(Barrier::new(2));

            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let project = Arc::clone(&project);
                    let gate = Arc::clone(&gate);
                    std::thread::spawn(move || {
                        gate.wait();
                        enable_worktree_config(&project)
                    })
                })
                .collect();

            for handle in handles {
                let outcome = handle.join().unwrap();
                assert!(
                    outcome.is_ok(),
                    "round {round}: a concurrent dispatch lost the config write and failed: {:#}",
                    outcome.unwrap_err()
                );
            }
        }
    }
}
