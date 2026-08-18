use assert_cmd::Command;

#[test]
fn help_lists_all_subcommands() {
    let mut cmd = Command::cargo_bin("mana").unwrap();
    let output = cmd.arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["launch", "ps", "kill", "doctor", "upgrade"] {
        assert!(stdout.contains(expected), "missing subcommand: {expected}");
    }
}

#[test]
fn version_flag_prints_cargo_package_version() {
    let mut cmd = Command::cargo_bin("mana").unwrap();
    let output = cmd.arg("--version").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), format!("mana {}", env!("CARGO_PKG_VERSION")));
}

/// `mana ps` is a listing, and a listing that exits non-zero is one no script
/// can pipe. It stays 0 even with nothing to list.
#[test]
fn ps_on_an_unknown_project_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("never-dispatched-here");
    std::fs::create_dir_all(&project).unwrap();

    let output = Command::cargo_bin("mana")
        .unwrap()
        .args(["ps", "--project"])
        .arg(&project)
        // MANA_HOME rather than a HOME/USERPROFILE redirect: dirs 6 resolves
        // the Windows home through the Known Folder API and ignores the env,
        // so only mana's own override is hermetic on every platform.
        .env("MANA_HOME", home.path().join(".mana"))
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no dispatches recorded"), "{stdout}");
}

#[test]
fn doctor_exits_zero_on_a_home_where_nothing_has_happened() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("mana")
        .unwrap()
        .arg("doctor")
        // MANA_HOME rather than a HOME/USERPROFILE redirect: dirs 6 resolves
        // the Windows home through the Known Folder API and ignores the env,
        // so only mana's own override is hermetic on every platform.
        .env("MANA_HOME", home.path().join(".mana"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Still a real report, not an early return: the catalogue section runs
    // against a home where nothing has ever happened.
    assert!(stdout.contains("catalogue --"), "{stdout}");
}

/// Issue #167: `status::dispatches_in` prints the registry's skipped-line
/// warnings, and `src/status.rs` promises one is said "exactly once per
/// command" -- which is what `mana ps` does. `mana doctor` derived project
/// state twice and said it twice.
#[test]
fn doctor_warns_about_a_corrupt_registry_line_exactly_once() {
    let home = tempfile::tempdir().unwrap();
    let mana_home = home.path().join(".mana");
    let project = home.path().join("my-api");
    std::fs::create_dir_all(&project).unwrap();

    // Where the state lives is mana's own answer, not this test's: the project
    // name fingerprints the absolute path (#33), and rebuilding that rule here
    // would be a second copy of it. `mana ps` prints the registry path.
    let listing = Command::cargo_bin("mana")
        .unwrap()
        .args(["ps", "--project"])
        .arg(&project)
        .env("MANA_HOME", &mana_home)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    let registry = std::path::PathBuf::from(
        listing
            .rsplit_once('(')
            .expect("`mana ps` names the registry file")
            .1
            .trim()
            .trim_end_matches(')'),
    );
    let state_dir = registry.parent().unwrap();
    std::fs::create_dir_all(state_dir).unwrap();
    std::fs::write(&registry, "{ not a record }\n").unwrap();
    // The worktrees directory has to exist, or doctor's worktree section
    // returns before the second derivation this test is about.
    std::fs::create_dir_all(
        mana_home
            .join("worktrees")
            .join(state_dir.file_name().unwrap()),
    )
    .unwrap();

    let output = Command::cargo_bin("mana")
        .unwrap()
        .args(["doctor", "--project"])
        .arg(&project)
        .env("MANA_HOME", &mana_home)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("warning:").count(),
        1,
        "one corrupt line, one warning: {stderr}"
    );
}

/// The other half of the exit-code contract: `mana kill` fails loudly on an id
/// it cannot find, rather than exiting 0 having done nothing.
#[test]
fn kill_with_an_unknown_id_fails() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("never-dispatched-here");
    std::fs::create_dir_all(&project).unwrap();

    let output = Command::cargo_bin("mana")
        .unwrap()
        .args(["kill", "does-not-exist", "--project"])
        .arg(&project)
        // MANA_HOME rather than a HOME/USERPROFILE redirect: dirs 6 resolves
        // the Windows home through the Known Folder API and ignores the env,
        // so only mana's own override is hermetic on every platform.
        .env("MANA_HOME", home.path().join(".mana"))
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no dispatch matching"), "{stderr}");
}
