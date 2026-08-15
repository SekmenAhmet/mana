use clap::{Args, Parser, Subcommand};

pub mod doctor;
pub mod install;
pub mod launch_pm;
pub mod launch_subagent;
pub mod uninstall;
pub mod upgrade;

#[derive(Parser)]
#[command(name = "mana", about = "Orchestrateur d'agents IA de coding", version)]
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
