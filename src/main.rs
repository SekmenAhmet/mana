mod catalog;
mod cli;
mod config;
mod dispatch;
mod lock;
mod log;
mod mcp;
mod pm;
mod project;
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
        Command::Launch { agent } => cli::launch_pm::run(&agent)?,
        Command::Doctor => cli::doctor::run()?,
        Command::Upgrade => cli::upgrade::run()?,
        Command::Dev { command } => cli::dev::run(&command)?,
        Command::McpServer { project_root } => mcp::serve(&project_root)?,
    }
    Ok(())
}
