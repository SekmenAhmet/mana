use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Default budget for `capture_version_output`: long enough for a real CLI
/// to answer `--version`, short enough that a hung/unresponsive binary
/// doesn't block `mana doctor`/`mana install` indefinitely.
pub const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawns `path --version`, waits up to `timeout` for it to exit, and
/// returns the first line of its stdout, trimmed. Shared by `doctor`
/// (checking a registered agent's current version) and `install`
/// (resolving a newly selected agent's version) — both need the same
/// "don't hang forever on an unresponsive CLI" behavior, so it lives in one
/// place instead of two. First-line-only because some CLIs (e.g. GitHub
/// Copilot CLI) print an extra nag line after the version
/// ("Run 'copilot update' to check for updates.") that would otherwise get
/// stored verbatim as part of the "version".
pub fn capture_version_output(path: &Path, timeout: Duration) -> anyhow::Result<String> {
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_status) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    stdout.read_to_string(&mut output)?;
                }
                let first_line = output.lines().next().unwrap_or("").trim().to_string();
                return Ok(first_line);
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

    #[cfg(unix)]
    #[test]
    fn capture_version_output_reads_stdout_on_normal_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_version_script(tmp.path(), "agent.sh", "3.1.4");
        assert_eq!(
            capture_version_output(&script, VERSION_CHECK_TIMEOUT).unwrap(),
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
            capture_version_output(&script, VERSION_CHECK_TIMEOUT).unwrap(),
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
        let result = capture_version_output(&script, Duration::from_millis(200));
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
            VERSION_CHECK_TIMEOUT,
        );
        assert!(result.is_err());
    }
}
