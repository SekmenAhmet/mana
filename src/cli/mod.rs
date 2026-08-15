use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod dev;
pub mod doctor;
pub mod install;
pub mod kill;
pub mod launch_pm;
pub mod ps;
pub mod uninstall;
pub mod upgrade;

#[derive(Parser)]
#[command(name = "mana", about = "AI coding-agent orchestrator", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Register a CLI agent
    Install,
    /// Remove a CLI agent
    Uninstall { cli: String },
    // One argument and no roles: sub-agents are never launched from a shell.
    // The PM dispatches them through mana's own tool channel
    // (`launch_subagent`, design §5), which is what lets mana pick the CLI and
    // model, own the worktree and observe the run. v1's
    // `mana launch --subagent <cli> --role <role> --assign <uuid>` was the
    // shell-out protocol that made all three impossible; it is gone, and this
    // comment is not a doc comment so the rationale stays out of `--help`.
    /// Launch a CLI agent in PM mode
    Launch {
        /// CLI agent to launch in PM mode (e.g. claude)
        agent: String,
    },
    /// List the sub-agents mana has dispatched
    Ps {
        /// Report on every project under ~/.mana/projects, not just this one
        #[arg(long)]
        all: bool,
        /// Project directory to report on (default: the working directory)
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },
    /// Kill a running sub-agent, by its agent id or an unambiguous prefix
    Kill {
        /// Agent id from `mana ps`, or any unambiguous prefix of one
        agent_id: String,
        /// Search every project under ~/.mana/projects, not just this one
        #[arg(long)]
        all: bool,
        /// Project directory to search (default: the working directory)
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },
    /// Diagnose the catalogue, this project and the configuration
    Doctor {
        /// Project directory to report on (default: the working directory)
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
        /// Remove worktrees left behind by dispatches that are no longer running
        #[arg(long)]
        prune: bool,
    },
    /// Update mana
    Upgrade,
    /// Developer scaffolding for the v2 pipeline. Hidden because its surface
    /// is unstable and it exists to drive by hand what the PM will drive over
    /// MCP once phase 2 lands -- not something to document to users.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: dev::DevCommand,
    },
    /// mana's orchestration tools, spoken over MCP on stdin/stdout.
    ///
    /// Hidden because no human runs it: mana registers this exact invocation
    /// with the PM's CLI via the catalogue's `mcp_args` template, pointing at
    /// `current_exe()`. Documenting it would invite users to wire it up by
    /// hand against a surface that is an internal contract (design §5).
    #[command(hide = true, name = "mcp-server")]
    McpServer {
        /// Absolute path to the project the PM is orchestrating. Required
        /// rather than inferred from the working directory: the PM CLI spawns
        /// this process from wherever it happens to be, and which project a
        /// task belongs to is not negotiable at that point.
        #[arg(long, value_name = "PATH")]
        project_root: PathBuf,
    },
}

/// clap's generated `help` subcommand only understands `mana help <cmd>`.
/// `mana help.md` also documents `mana <cmd> help` as valid — this rewrites
/// a trailing `help` token to the front so both forms parse identically.
/// `args` includes the program name at index 0, same shape as
/// `std::env::args().collect()`.
pub fn normalize_help_invocation(args: Vec<String>) -> Vec<String> {
    if args.len() < 3 {
        return args;
    }
    let (program, rest) = args.split_first().expect("checked len >= 3 above");
    if rest.last().map(String::as_str) == Some("help") {
        let mut rewritten = vec![program.clone(), "help".to_string()];
        rewritten.extend(rest[..rest.len() - 1].iter().cloned());
        rewritten
    } else {
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_help_invocation_rewrites_trailing_help() {
        let args = vec!["mana".to_string(), "doctor".to_string(), "help".to_string()];
        assert_eq!(
            normalize_help_invocation(args),
            vec!["mana".to_string(), "help".to_string(), "doctor".to_string()]
        );
    }

    #[test]
    fn normalize_help_invocation_leaves_leading_help_alone() {
        let args = vec!["mana".to_string(), "help".to_string(), "doctor".to_string()];
        assert_eq!(normalize_help_invocation(args.clone()), args);
    }

    #[test]
    fn normalize_help_invocation_leaves_bare_help_alone() {
        let args = vec!["mana".to_string(), "help".to_string()];
        assert_eq!(normalize_help_invocation(args.clone()), args);
    }

    #[test]
    fn normalize_help_invocation_leaves_non_help_invocations_alone() {
        let args = vec!["mana".to_string(), "doctor".to_string()];
        assert_eq!(normalize_help_invocation(args.clone()), args);
    }

    #[test]
    fn normalize_help_invocation_preserves_args_before_trailing_help() {
        let args = vec![
            "mana".to_string(),
            "uninstall".to_string(),
            "claude".to_string(),
            "help".to_string(),
        ];
        assert_eq!(
            normalize_help_invocation(args),
            vec![
                "mana".to_string(),
                "help".to_string(),
                "uninstall".to_string(),
                "claude".to_string(),
            ]
        );
    }
}
