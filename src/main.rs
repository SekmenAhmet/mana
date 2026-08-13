mod cli;
mod project;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Install => println!("mana install: not yet implemented"),
        Command::Uninstall { cli } => println!("mana uninstall {cli}: not yet implemented"),
        Command::Launch(_args) => println!("mana launch: not yet implemented"),
        Command::Doctor => println!("mana doctor: not yet implemented"),
        Command::Upgrade => println!("mana upgrade: pas encore disponible"),
    }
    Ok(())
}
