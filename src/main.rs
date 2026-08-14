mod agents;
mod cli;
mod config;
mod log;
mod project;
mod task;
mod lock;
mod dependencies;
mod review;
mod prompts;
mod pty;
mod monitor;
mod tui;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Install => cli::install::run()?,
        Command::Uninstall { cli } => cli::uninstall::run(&cli)?,
        Command::Launch(args) => {
            if let Some(subagent_cli) = &args.subagent {
                let role = args.role.as_deref().ok_or_else(|| anyhow::anyhow!("--role est requis avec --subagent"))?;
                let assign = args.assign.as_deref().ok_or_else(|| anyhow::anyhow!("--assign est requis avec --subagent"))?;
                cli::launch_subagent::run(subagent_cli, role, assign, &args.params)?;
            } else if let Some(agent) = &args.agent {
                println!("mana launch {agent} (PM): not yet implemented"); // replaced in Task 21
            } else {
                anyhow::bail!("usage: mana launch <agent> | mana launch --subagent <cli> --role <role> --assign <task-uuid>");
            }
        }
        Command::Doctor => cli::doctor::run()?,
        Command::Upgrade => println!("mana upgrade: pas encore disponible"),
    }
    Ok(())
}
