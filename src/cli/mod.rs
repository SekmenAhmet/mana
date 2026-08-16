use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod dev;
pub mod doctor;
pub mod kill;
pub mod launch_pm;
pub mod ps;
pub mod upgrade;

#[derive(Parser)]
#[command(name = "mana", about = "AI coding-agent orchestrator", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    // One argument and no roles: sub-agents are never launched from a shell.
    // The PM dispatches them through mana's own tool channel
    // (`launch_subagent`, design §5), which is what lets mana pick the CLI and
    // model, own the worktree and observe the run. v1's
    // `mana launch --subagent <cli> --role <role> --assign <uuid>` was the
    // shell-out protocol that made all three impossible; it is gone, and this
    // comment is not a doc comment so the rationale stays out of `--help`.
    /// Launch a CLI agent in PM mode
    Launch {
        /// CLI agent to launch in PM mode (e.g. claude). Optional only with
        /// --continue, which falls back to the last CLI launched in this
        /// project.
        agent: Option<String>,
        /// Resume this project's previous PM conversation instead of starting
        /// a fresh one
        #[arg(long = "continue", short = 'c')]
        resume: bool,
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
    /// Diagnose the catalogue and this project
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
    /// is unstable and it drives by hand what the PM now drives over MCP --
    /// not something to document to users.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(args: &[&str]) -> (Option<String>, bool) {
        match Cli::parse_from(args).command {
            Command::Launch { agent, resume } => (agent, resume),
            _ => panic!("not a launch"),
        }
    }

    /// The four shapes of `mana launch`, pinned: the CLI is optional *only*
    /// because `-c` can supply it from this project's state, and both spellings
    /// of the flag mean the same thing.
    #[test]
    fn launch_takes_an_optional_cli_and_a_continue_flag() {
        assert_eq!(
            launch(&["mana", "launch", "claude"]),
            (Some("claude".into()), false)
        );
        assert_eq!(
            launch(&["mana", "launch", "claude", "-c"]),
            (Some("claude".into()), true)
        );
        assert_eq!(
            launch(&["mana", "launch", "--continue", "claude"]),
            (Some("claude".into()), true)
        );
        assert_eq!(launch(&["mana", "launch", "-c"]), (None, true));
        assert_eq!(launch(&["mana", "launch"]), (None, false));
    }
}
