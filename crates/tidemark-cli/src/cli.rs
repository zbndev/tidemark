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
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
        name: String,
        value: String,
    },
    /// Notifications for one window of one account.
    Notify {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
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
    /// Print a shell completion script on stdout.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
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
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
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
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
        /// Read the key from this file instead of stdin.
        #[arg(long)]
        key_file: Option<std::path::PathBuf>,
    },
    /// Store a browser session header. Read like a key.
    SetSession {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
        /// Read the session from this file instead of stdin.
        #[arg(long)]
        key_file: Option<std::path::PathBuf>,
    },
    /// Remove whatever credential Tidemark holds for an account.
    SignOut {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// Print the authorize URL, then wait for the browser to come back.
    Login {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// Abandon a login that is waiting.
    CancelLogin {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// The dynamic local authentication sources the daemon can see, without credentials.
    Sources {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// Record which authentication source this account uses.
    Select {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
        /// `oauth` or `cli`, or a dynamic mode value from `auth sources`.
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
    Rm {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// Rewrite the order the cards go in. Must name every configured provider.
    Order { providers: Vec<String> },
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Add one more account to a provider the config already has.
    Add { provider: String, account: String },
    /// Remove one account. The same call as `provider rm`.
    Rm {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// Rename an account, carrying its credential and history to the new id.
    Rename {
        provider: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
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

    #[test]
    fn provider_and_account_mutations_default_to_the_default_account() {
        let provider = Cli::try_parse_from(["tidemarkctl", "provider", "rm", "codex"])
            .expect("provider removal defaults the account");
        assert!(matches!(
            provider.command,
            Command::Provider {
                command: ProviderCommand::Rm {
                    provider,
                    account
                }
            } if provider == "codex" && account == "default"
        ));

        let remove = Cli::try_parse_from(["tidemarkctl", "account", "rm", "codex"])
            .expect("account removal defaults the account");
        assert!(matches!(
            remove.command,
            Command::Account {
                command: AccountCommand::Rm {
                    provider,
                    account
                }
            } if provider == "codex" && account == "default"
        ));

        let rename = Cli::try_parse_from(["tidemarkctl", "account", "rename", "codex", "personal"])
            .expect("account rename defaults the old account");
        assert!(matches!(
            rename.command,
            Command::Account {
                command: AccountCommand::Rename {
                    provider,
                    account,
                    new
                }
            } if provider == "codex" && account == "default" && new == "personal"
        ));
    }

    #[test]
    fn authentication_commands_default_to_the_default_account() {
        for verb in ["sign-out", "login", "cancel-login", "sources"] {
            let parsed = Cli::try_parse_from(["tidemarkctl", "auth", verb, "codex"])
                .unwrap_or_else(|error| panic!("{verb} should default the account: {error}"));
            let account = match parsed.command {
                Command::Auth {
                    command:
                        AuthCommand::SignOut { account, .. }
                        | AuthCommand::Login { account, .. }
                        | AuthCommand::CancelLogin { account, .. }
                        | AuthCommand::Sources { account, .. },
                } => account,
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(account, "default", "{verb}");
        }

        let key = Cli::try_parse_from(["tidemarkctl", "auth", "set-key", "zai"])
            .expect("set-key defaults the account");
        assert!(matches!(
            key.command,
            Command::Auth {
                command: AuthCommand::SetKey { account, .. }
            } if account == "default"
        ));

        let session = Cli::try_parse_from(["tidemarkctl", "auth", "set-session", "t3chat"])
            .expect("set-session defaults the account");
        assert!(matches!(
            session.command,
            Command::Auth {
                command: AuthCommand::SetSession { account, .. }
            } if account == "default"
        ));

        let select =
            Cli::try_parse_from(["tidemarkctl", "auth", "select", "codex", "--mode", "oauth"])
                .expect("select defaults the account");
        assert!(matches!(
            select.command,
            Command::Auth {
                command: AuthCommand::Select { account, .. }
            } if account == "default"
        ));
    }

    #[test]
    fn option_notification_and_history_default_to_the_default_account() {
        let option = Cli::try_parse_from(["tidemarkctl", "option", "zai", "region", "china"])
            .expect("option defaults the account");
        assert!(matches!(
            option.command,
            Command::Option { account, .. } if account == "default"
        ));

        let notify = Cli::try_parse_from(["tidemarkctl", "notify", "codex", "w604800", "on"])
            .expect("notify defaults the account");
        assert!(matches!(
            notify.command,
            Command::Notify { account, .. } if account == "default"
        ));

        let history =
            Cli::try_parse_from(["tidemarkctl", "history", "segment", "codex", "w604800"])
                .expect("history defaults the account");
        assert!(matches!(
            history.command,
            Command::History {
                command: HistoryCommand::Segment { account, .. }
            } if account == "default"
        ));
    }

    #[test]
    fn an_explicit_account_overrides_the_default() {
        let parsed =
            Cli::try_parse_from(["tidemarkctl", "auth", "login", "codex", "--account", "work"])
                .expect("an account override is accepted");
        assert!(matches!(
            parsed.command,
            Command::Auth {
                command: AuthCommand::Login { account, .. }
            } if account == "work"
        ));
    }

    #[test]
    fn aggregate_filters_still_include_every_account_by_default() {
        let usage = Cli::parse_from(["tidemarkctl", "usage", "--provider", "codex"]);
        assert!(matches!(
            usage.command,
            Command::Usage(Usage { account: None, .. })
        ));

        let guard = Cli::parse_from([
            "tidemarkctl",
            "guard",
            "--min-remaining",
            "20",
            "--provider",
            "codex",
        ]);
        assert!(matches!(
            guard.command,
            Command::Guard(Guard { account: None, .. })
        ));
    }
}
