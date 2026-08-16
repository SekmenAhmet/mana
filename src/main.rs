mod catalog;
mod cli;
mod dispatch;
mod lock;
mod log;
mod mcp;
mod pm;
mod project;
mod review;
mod sentinel;
mod spawn;
mod status;
mod subprocess;
mod task;
mod tui;
mod worktree;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        // The one subcommand that wraps a child process, and therefore the one
        // whose exit code is not mana's to invent: a PM that exited 7 has to
        // reach the shell as 7, not as anyhow's 1 (#95). Every other arm's
        // success is mana's own, so `Ok(())` is the whole answer.
        Command::Launch { agent, resume } => {
            let code = cli::launch_pm::run(agent.as_deref(), resume)?;
            if code != 0 {
                std::process::exit(i32::from(code));
            }
        }
        Command::Ps { all, project } => cli::ps::run(all, project.as_deref())?,
        Command::Kill {
            agent_id,
            all,
            project,
        } => cli::kill::run(&agent_id, all, project.as_deref())?,
        Command::Doctor { project, prune } => cli::doctor::run(project.as_deref(), prune)?,
        Command::Upgrade => cli::upgrade::run()?,
        Command::Dev { command } => cli::dev::run(&command)?,
        Command::McpServer { project_root } => mcp::serve(&project_root)?,
    }
    Ok(())
}
