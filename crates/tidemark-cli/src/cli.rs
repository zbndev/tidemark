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
    /// Exit 0 when enough quota is left to start something, 1 when there is not.
    Guard(Guard),
    /// Print the daemon's changes as they arrive, one JSON object per line.
    Watch(Watch),
    /// The provider catalog, the configured set, and what is in it.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// The accounts one provider carries.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Credentials: keys, pasted sessions, logins and local sources.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Poll now: one provider, or everything.
    Refresh {
        /// A provider slug. Omitted, every configured account is polled.
        provider: Option<String>,
    },
    /// One of a provider's own settings.
    Option {
        provider: String,
        account: String,
        name: String,
        value: String,
    },
    /// Notifications for one window of one account.
    Notify {
        provider: String,
        account: String,
        window: String,
        enabled: Switch,
    },
    /// Application preferences the daemon keeps in config.toml.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Stored history.
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Paths and storage facts.
    Data,
    /// A newer published release, if the daemon knows of one.
    Update,
}

/// `on` and `off` rather than `true` and `false`: the settings pages call these switches,
/// and `config show` prints the same two words, so its output feeds straight back in.
///
/// A `ValueEnum` rather than a parser onto `bool`: clap derives a flag from a `bool`
/// positional and refuses to take a value for it, and this way `--help` lists the two
/// words instead of leaving them to prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Switch {
    On,
    Off,
}

impl From<Switch> for bool {
    fn from(switch: Switch) -> Self {
        matches!(switch, Switch::On)
    }
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Every preference, as the daemon holds it.
    Show,
    /// The one proxy every request and every child process goes through.
    Proxy {
        /// off, http, https or socks5.
        mode: String,
        /// Required by every mode but `off`.
        host: Option<String>,
        /// Required by every mode but `off`.
        port: Option<u16>,
    },
    /// How healthy accounts are paced.
    Refresh {
        /// auto or manual.
        mode: String,
        /// Minutes between polls in manual mode, 1 to 120.
        #[arg(long)]
        minutes: Option<u32>,
    },
    /// forever, six-months or one-year.
    Retention { retention: String },
    /// system, light or dark.
    Theme { theme: String },
    /// app, daemon or off.
    Startup { mode: String },
    /// Whether the daemon may ask GitHub for the latest release.
    ReleaseCheck { enabled: Switch },
    /// Whether the window's close button hides it.
    MinimizeOnClose { enabled: Switch },
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// The stored points of one window's current segment, oldest first.
    Segment {
        provider: String,
        account: String,
        window: String,
    },
    /// Delete every stored point, segment and notification record.
    Clear,
}

/// A secret is never a positional argument: `/proc/<pid>/cmdline` is readable by every
/// process of the same user, and a key typed once lands in shell history. There is
/// deliberately no slot to put one in — see `secret.rs`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store an API key. The key is read from stdin, or from --key-file.
    SetKey {
        provider: String,
        account: String,
        /// Read the key from this file instead of stdin.
        #[arg(long)]
        key_file: Option<std::path::PathBuf>,
    },
    /// Store a browser session header. Read like a key.
    SetSession {
        provider: String,
        account: String,
        /// Read the session from this file instead of stdin.
        #[arg(long)]
        key_file: Option<std::path::PathBuf>,
    },
    /// Remove whatever credential Tidemark holds for an account.
    SignOut { provider: String, account: String },
    /// Print the authorize URL, then wait for the browser to come back.
    Login { provider: String, account: String },
    /// Abandon a login that is waiting.
    CancelLogin { provider: String, account: String },
    /// The local authentication sources the daemon can see, without their credentials.
    Sources { provider: String, account: String },
    /// Record which local source this account uses.
    Select {
        provider: String,
        account: String,
        /// The mode value from `auth sources`.
        #[arg(long)]
        mode: String,
        /// The candidate id, for a mode that offers a choice.
        #[arg(long)]
        candidate: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Every provider this build knows how to configure.
    Catalog,
    /// Every configured account, with its state.
    List,
    /// Configure a provider, creating its default account.
    Add { provider: String },
    /// Remove one configured account, its credentials and its card.
    Rm { provider: String, account: String },
    /// Rewrite the order the cards go in. Must name every configured provider.
    Order { providers: Vec<String> },
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Add one more account to a provider the config already has.
    Add { provider: String, account: String },
    /// Remove one account. The same call as `provider rm`.
    Rm { provider: String, account: String },
    /// Rename an account, carrying its credential and history to the new id.
    Rename {
        provider: String,
        account: String,
        new: String,
    },
    /// Rewrite one provider's account order.
    Order {
        provider: String,
        accounts: Vec<String>,
    },
}

#[derive(Debug, clap::Args)]
pub struct Watch {
    /// Only this provider slug.
    #[arg(long)]
    pub provider: Option<String>,
    /// `json` prints one event per line; `waybar` prints a card after every change.
    #[arg(long, value_enum, default_value_t = StreamFormat::Json)]
    pub format: StreamFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StreamFormat {
    Json,
    Waybar,
}

#[derive(Debug, clap::Args)]
pub struct Guard {
    /// Fail when less than this percentage of the window is left.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub min_remaining: u8,
    /// Judge this window key instead of the one a card leads with.
    #[arg(long, conflicts_with = "any")]
    pub window: Option<String>,
    /// Judge every window, not just the leading one.
    #[arg(long)]
    pub any: bool,
    /// Only this provider slug.
    #[arg(long)]
    pub provider: Option<String>,
    /// Only this account of that provider.
    #[arg(long, requires = "provider")]
    pub account: Option<String>,
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
