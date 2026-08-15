mod agents;
mod catalog;
mod cli;
mod config;
mod dependencies;
mod dispatch;
mod lock;
mod log;
mod mcp;
mod monitor;
mod pm;
mod project;
mod prompts;
mod pty;
mod review;
mod sentinel;
mod spawn;
mod subprocess;
mod task;
mod tui;
mod worktree;

use clap::Parser;
use cli::{Cli, Command, normalize_help_invocation};

fn main() -> anyhow::Result<()> {
    let args = normalize_help_invocation(std::env::args().collect());
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Install => cli::install::run()?,
        Command::Uninstall { cli } => cli::uninstall::run(&cli)?,
        Command::Launch(args) => {
            if let Some(subagent_cli) = &args.subagent {
                let role = args
                    .role
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--role is required with --subagent"))?;
                let assign = args
                    .assign
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--assign is required with --subagent"))?;
                cli::launch_subagent::run(subagent_cli, role, assign, &args.params)?;
            } else if let Some(agent) = &args.agent {
                cli::launch_pm::run(agent)?;
            } else {
                anyhow::bail!(
                    "usage: mana launch <agent> | mana launch --subagent <cli> --role <role> --assign <task-uuid>"
                );
            }
        }
        Command::Doctor => cli::doctor::run()?,
        Command::Upgrade => cli::upgrade::run()?,
        Command::Dev { command } => cli::dev::run(&command)?,
        Command::McpServer { project_root } => mcp::serve(&project_root)?,
    }
    Ok(())
}
