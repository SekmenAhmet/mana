pub const KNOWN_CLIS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "antigravity",
    "copilot",
    "opencode",
];

/// The flag that makes a given agent CLI skip interactive permission
/// prompts, required for sub-agents (executor/reviewer) which run
/// unattended. v1 only knows this for `claude`; any other CLI is rejected
/// with a clear error rather than risking a sub-agent hanging forever.
pub fn autonomous_flag(cli_name: &str) -> anyhow::Result<&'static str> {
    match cli_name {
        "claude" => Ok("--dangerously-skip-permissions"),
        other => anyhow::bail!(
            "sub-agent role not supported for '{other}' yet (only 'claude' is supported in v1)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_has_known_autonomous_flag() {
        assert_eq!(
            autonomous_flag("claude").unwrap(),
            "--dangerously-skip-permissions"
        );
    }

    #[test]
    fn unknown_cli_errors_clearly() {
        let err = autonomous_flag("codex").unwrap_err();
        assert!(err.to_string().contains("codex"));
        assert!(err.to_string().contains("not supported"));
    }
}
