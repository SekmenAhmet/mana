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
