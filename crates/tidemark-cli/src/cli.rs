//! The argument grammar, grouped by the entity a subcommand acts on rather than flattened
//! into thirty verbs: a person reading `--help` should find `provider add` under
//! `provider`, and a plugin author should be able to guess the next one.

use clap::{Parser, Subcommand};

/// The command-line client for the Tidemark daemon.
#[derive(Debug, Parser)]
#[command(name = "tidemarkctl", version, about, arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// This client's version, and the daemon's.
    Version,
    /// What every configured account currently reports.
    Usage(Usage),
}

#[derive(Debug, clap::Args)]
pub struct Usage {
    /// Only this provider slug.
    #[arg(long)]
    pub provider: Option<String>,
    /// Only this account of that provider.
    #[arg(long, requires = "provider")]
    pub account: Option<String>,
    /// How to print it.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

/// The output shapes. `json` and `waybar` are contracts other programs parse; `text` is
/// for a person and may be reworded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Text,
    Json,
    Waybar,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_grammar_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_takes_no_arguments() {
        let cli = Cli::parse_from(["tidemarkctl", "version"]);
        assert!(matches!(cli.command, Command::Version));
    }
}
