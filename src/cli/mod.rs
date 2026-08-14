use clap::{Args, Parser, Subcommand};

pub mod doctor;
pub mod install;
pub mod launch_pm;
pub mod launch_subagent;
pub mod uninstall;
pub mod upgrade;

#[derive(Parser)]
#[command(name = "mana", about = "Orchestrateur d'agents IA de coding")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Enregistrer un agent CLI
    Install,
    /// Retirer un agent CLI
    Uninstall { cli: String },
    /// Lancer un agent en mode PM, ou un sous-agent avec --subagent
    Launch(LaunchArgs),
    /// Diagnostiquer la configuration
    Doctor,
    /// Mettre a jour mana
    Upgrade,
}

#[derive(Args)]
pub struct LaunchArgs {
    /// CLI agent a lancer en mode PM (ex: claude). Absent si --subagent est utilise.
    pub agent: Option<String>,

    #[arg(long)]
    pub subagent: Option<String>,

    #[arg(long)]
    pub role: Option<String>,

    #[arg(long)]
    pub assign: Option<String>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub params: Vec<String>,
}
