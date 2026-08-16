//! Process plumbing that belongs to no single caller: the short "run a CLI and
//! read what it printed" probe `doctor` and `install` share, plus the two
//! platform details the sub-agent spawner (`crate::spawn`) and the PM drivers
//! (`crate::pm::child`) need identically -- putting a child in its own process
//! group, and waiting on a reader thread without waiting forever.
//!
//! Only the parts that carry no policy live here. What a timeout or a shutdown
//! then *does* to that group stays with whoever owns the deadline.

use anyhow::Context;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a wait loop asks whether it can stop. Small enough that a
/// sub-second bound is still honoured, large enough not to spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Default budget for `capture_version_output`: long enough for a real CLI
/// to answer `--version`, short enough that a hung/unresponsive binary
/// doesn't block `mana doctor` indefinitely.
pub const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawns `path` with `version_args`, waits up to `timeout` for it to exit,
/// and returns the first line of its stdout, trimmed. Shared by `doctor`
/// (checking a registered agent's current version) and `install`
/// (resolving a newly selected agent's version) — both need the same
/// "don't hang forever on an unresponsive CLI" behavior, so it lives in one
/// place instead of two. First-line-only because some CLIs (e.g. GitHub
/// Copilot CLI) print an extra nag line after the version
/// ("Run 'copilot update' to check for updates.") that would otherwise get
/// stored verbatim as part of the "version".
///
/// `version_args` is a parameter and not a hardcoded `--version` because
/// which flag prints a version is per-CLI knowledge, and per-CLI knowledge
/// lives in `catalog/*.toml` ([cli].version_args) — never here.
pub fn capture_version_output(
    path: &Path,
    version_args: &[String],
    timeout: Duration,
) -> anyhow::Result<String> {
    let output = capture_output(path, version_args, timeout, Capture::Stdout)?;
    Ok(output.lines().next().unwrap_or("").trim().to_string())
}

/// Which of a probe's streams to keep.
///
/// `Both` is not a convenience: two of the four shipped CLIs print their model
/// list on **stderr** (`agy models` sends everything there; `opencode models`
/// splits the list across both), so a discovery that read stdout alone would
/// report "no models" for one and half a list for the other. Which stream a
/// CLI answers on is per-CLI knowledge that the catalogue schema has no field
/// for, and reading both needs no such field: it is stream-agnostic, not a
/// branch on a CLI's name.
pub enum Capture {
    Stdout,
    Both,
}

/// The same spawn-and-wait, keeping every line instead of the first.
///
/// Additive helper (task 4.3): `mana doctor` runs a catalogue entry's
/// `[models].discovery_args` and reads a *list* of model ids off the output,
/// which is the same "run a CLI briefly, do not hang on it" problem
/// `capture_version_output` already solved -- so that function now delegates
/// here rather than the two keeping separate copies of the timeout loop.
///
/// Both streams are drained only after the process has exited, which is the
/// shape this function already had: a child that writes more than a pipe
/// buffer (64 KiB) without exiting blocks, and is then killed by the timeout
/// below. That is a loud failure and not a hang, which is all a probe needs --
/// the streaming case belongs to `crate::spawn`, which has the reader threads.
pub fn capture_output(
    path: &Path,
    args: &[String],
    timeout: Duration,
    capture: Capture,
) -> anyhow::Result<String> {
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(match capture {
            Capture::Stdout => Stdio::null(),
            Capture::Both => Stdio::piped(),
        })
        // Names the binary it could not start (#117): a bare
        // "No such file or directory" from a version probe is a report
        // nobody can act on.
        .spawn()
        .with_context(|| format!("spawning {}", path.display()))?;

    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_status) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    stdout.read_to_string(&mut output)?;
                }
                if let Some(mut stderr) = child.stderr.take() {
                    stderr.read_to_string(&mut output)?;
                }
                return Ok(output);
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow::anyhow!(
                        "subprocess timeout for '{}': exceeded {:?}",
                        path.display(),
                        timeout
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Puts a child in a process group of its own, so a signal aimed at the group
/// on either side cannot cross into the other.
///
/// One copy for two callers because it is one call for one reason: without it
/// the child shares mana's group, so killing that group would kill mana and a
/// Ctrl-C meant for the TUI would reach the child. What each caller then kills
/// -- and when -- is policy and stays with the caller.
#[cfg(unix)]
pub fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // 0 = "become the leader of your own group", so the child's pgid equals
    // its pid -- which is what makes a later `killpg` reach its descendants.
    command.process_group(0);
}

#[cfg(windows)]
pub fn set_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // The child and its descendants form a console group of their own. This is
    // the whole of what Windows gives for free: see each caller's `kill_group`
    // for what a kill actually reaches, which is where the two diverge.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Waits for a reader thread to finish, but never past `deadline`.
///
/// Unbounded joining would hand mana's own timing to whatever still holds the
/// write end of the pipe -- a grandchild that survived the kill, say -- so the
/// process that ignored a deadline would decide how long mana waits on it. A
/// detached thread keeps appending to a bounded buffer nobody reads, which
/// costs memory that is already capped and no correctness.
pub fn join_or_detach(handle: JoinHandle<()>, deadline: Instant) {
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Writes an executable shell script at `dir/name` with `body` as its
/// contents, shebang included.
///
/// The one place the write-then-chmod ritual lives. Five modules had their own
/// copy of it and a sixth open-coded it inline, which is six chances to forget
/// the `0o755` and get a "Permission denied" that reads like a bug in the code
/// under test rather than in its fixture (#66).
#[cfg(all(test, unix))] // uses unix permission bits; every caller is unix-gated
pub(crate) fn write_executable(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join(name);
    std::fs::write(&script, body).unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

/// A fake CLI that prints `printed_version` and exits — the fixture every
/// version-reporting test needs, without depending on a real binary being
/// installed.
#[cfg(all(test, unix))]
pub(crate) fn write_version_script(
    dir: &Path,
    name: &str,
    printed_version: &str,
) -> std::path::PathBuf {
    write_executable(dir, name, &format!("#!/bin/sh\necho {printed_version}\n"))
}

#[cfg(all(test, unix))] // every test here execs unix shell fixtures
mod tests {
    use super::*;

    /// What every real catalogue entry declares today. Spelled out per call
    /// rather than defaulted in the function: the point of the parameter is
    /// that no CLI's flag is baked into this module.
    fn version_flag() -> Vec<String> {
        vec!["--version".to_string()]
    }

    /// The whole reason `version_args` is a parameter: a CLI whose version
    /// lives behind something other than `--version` must still be probeable.
    #[cfg(unix)]
    #[test]
    fn capture_version_output_passes_the_catalogue_args_to_the_binary() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("echo-args.sh");
        std::fs::write(&script, "#!/bin/sh\necho \"got:$*\"\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let args = vec!["version".to_string(), "--short".to_string()];
        assert_eq!(
            capture_version_output(&script, &args, VERSION_CHECK_TIMEOUT).unwrap(),
            "got:version --short"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_version_output_reads_stdout_on_normal_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_version_script(tmp.path(), "agent.sh", "3.1.4");
        assert_eq!(
            capture_version_output(&script, &version_flag(), VERSION_CHECK_TIMEOUT).unwrap(),
            "3.1.4"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_version_output_drops_extra_lines_after_the_version() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("copilot-like.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'GitHub Copilot CLI 1.0.80.'\necho \"Run 'copilot update' to check for updates.\"\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        assert_eq!(
            capture_version_output(&script, &version_flag(), VERSION_CHECK_TIMEOUT).unwrap(),
            "GitHub Copilot CLI 1.0.80."
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_version_output_times_out_on_unresponsive_binary() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("slow-agent.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let start = Instant::now();
        let result = capture_version_output(&script, &version_flag(), Duration::from_millis(200));
        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should time out near the 200ms bound, took {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_version_output_errors_on_missing_binary() {
        let result = capture_version_output(
            Path::new("/nonexistent/path/to/nowhere"),
            &version_flag(),
            VERSION_CHECK_TIMEOUT,
        );
        assert!(result.is_err());
    }
}
