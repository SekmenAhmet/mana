use anyhow::Context;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

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
    let mut child = std::process::Command::new(path)
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
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Writes an executable shell script at `dir/name` that prints
/// `printed_version` and exits. Test-only fixture shared by every module
/// that needs a fake "CLI" binary to check version-reporting behavior
/// against, without depending on a specific real binary being installed.
#[cfg(all(test, unix))] // uses unix permission bits; every caller is unix-gated
pub(crate) fn write_version_script(
    dir: &Path,
    name: &str,
    printed_version: &str,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join(name);
    std::fs::write(&script, format!("#!/bin/sh\necho {printed_version}\n")).unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
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
