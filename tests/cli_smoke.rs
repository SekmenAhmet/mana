use assert_cmd::Command;

#[test]
fn help_lists_all_subcommands() {
    let mut cmd = Command::cargo_bin("mana").unwrap();
    let output = cmd.arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["install", "uninstall", "launch", "doctor", "upgrade"] {
        assert!(stdout.contains(expected), "missing subcommand: {expected}");
    }
}
