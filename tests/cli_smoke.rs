use assert_cmd::Command;

#[test]
fn help_lists_all_subcommands() {
    let mut cmd = Command::cargo_bin("mana").unwrap();
    let output = cmd.arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "install",
        "uninstall",
        "launch",
        "ps",
        "kill",
        "doctor",
        "upgrade",
    ] {
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

/// `mana doctor`'s exit-code contract, end to end: a registered CLI whose
/// binary is gone is broken, and broken is exit 1 — the whole report still
/// prints. Asserted through the real binary because the contract is what a
/// script observes, not what the report struct says internally.
#[test]
fn doctor_exits_one_when_a_registered_binary_is_gone() {
    let home = tempfile::tempdir().unwrap();
    let mana = home.path().join(".mana");
    std::fs::create_dir_all(&mana).unwrap();
    std::fs::write(
        mana.join("config.toml"),
        "[models.ghost]\nname = \"ghost\"\nversion = \"1.0\"\n\
         path = \"/nonexistent/path/ghost\"\nversion_args = [\"--version\"]\n",
    )
    .unwrap();

    let output = Command::cargo_bin("mana")
        .unwrap()
        .arg("doctor")
        // MANA_HOME rather than a HOME/USERPROFILE redirect: dirs 6 resolves
        // the Windows home through the Known Folder API and ignores the env,
        // so only mana's own override is hermetic on every platform.
        .env("MANA_HOME", home.path().join(".mana"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("BINARY MISSING"), "{stdout}");
    // The rest of the diagnosis is not lost to one broken finding.
    assert!(stdout.contains("catalogue --"), "{stdout}");
}

#[test]
fn doctor_exits_zero_on_a_home_with_nothing_registered() {
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
    assert!(stdout.contains("nothing registered"), "{stdout}");
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
