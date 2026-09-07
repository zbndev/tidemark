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
