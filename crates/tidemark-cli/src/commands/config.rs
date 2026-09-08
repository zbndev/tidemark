//! Application preferences, as the daemon holds them.
//!
//! Every named choice is a string the daemon validates, not an enum this client duplicates:
//! a newer daemon may accept a value this build has never heard of, and refusing it here
//! would make the client the thing that needs upgrading.

use tidemark_ipc::DaemonProxy;
use tidemark_types::Preferences;

use crate::cli::ConfigCommand;
use crate::exit::{Exit, Failure};

pub async fn run(proxy: &DaemonProxy<'_>, command: ConfigCommand) -> Result<Exit, Failure> {
    match command {
        ConfigCommand::Show => print!("{}", show(&proxy.get_preferences().await?)),
        // The daemon takes the three proxy values as one call because half a proxy is a
        // proxy nothing can be reached through. Completing them here means the user gets a
        // sentence instead of a refusal.
        ConfigCommand::Proxy { mode, host, port } => {
            if mode == Preferences::PROXY_OFF {
                proxy.set_proxy(&mode, "", 0).await?;
            } else {
                let (Some(host), Some(port)) = (host, port) else {
                    return Err(Failure::usage(format!(
                        "the `{mode}` proxy mode needs a host and a port"
                    )));
                };
                proxy.set_proxy(&mode, &host, port).await?;
            }
        }
        // The interval first: `SetRefreshMode` polls every account immediately, so the
        // other order would poll once at the pace the user is leaving behind.
        ConfigCommand::Refresh { mode, minutes } => {
            if let Some(minutes) = minutes {
                proxy.set_refresh_minutes(minutes).await?;
            }
            proxy.set_refresh_mode(&mode).await?;
        }
        ConfigCommand::Retention { retention } => proxy.set_history_retention(&retention).await?,
        ConfigCommand::Theme { theme } => proxy.set_theme(&theme).await?,
        ConfigCommand::Startup { mode } => proxy.set_startup_mode(&mode).await?,
        ConfigCommand::ReleaseCheck { enabled } => proxy.set_release_check(enabled.into()).await?,
        ConfigCommand::MinimizeOnClose { enabled } => {
            proxy.set_minimize_on_close(enabled.into()).await?
        }
    }
    Ok(Exit::Ok)
}

/// Named choices are printed as the daemon stores them, so what comes out of `show` is
/// what goes back into the corresponding subcommand. Switches print `on` and `off` for the
/// same reason: that is what `config release-check` takes.
fn show(preferences: &Preferences) -> String {
    let mut out = String::new();
    let mut row = |name: &str, value: &str| out.push_str(&format!("{name:<20}{value}\n"));
    row("release-check", switch(preferences.release_check));
    row("minimize-on-close", switch(preferences.minimize_on_close));
    row(
        "theme",
        preferences
            .theme
            .as_deref()
            .unwrap_or(Preferences::THEME_SYSTEM),
    );
    row("startup", &preferences.startup_mode);
    row("retention", &preferences.history_retention);
    row("refresh", &preferences.refresh_mode);
    row("refresh-minutes", &preferences.refresh_minutes.to_string());
    row("proxy", &preferences.proxy_mode);
    row("proxy-host", &preferences.proxy_host);
    row("proxy-port", &preferences.proxy_port.to_string());
    out
}

const fn switch(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `config show` and the subcommands must speak the same words, or the output cannot be
    /// fed back in. `on`/`off` is what `value_parser = switch` takes in `cli.rs`.
    #[test]
    fn a_switch_prints_the_word_the_subcommand_accepts() {
        let preferences = Preferences {
            release_check: false,
            ..Preferences::default()
        };
        let out = show(&preferences);
        assert!(out.contains("release-check       off"), "{out}");
        assert!(out.contains("minimize-on-close   on"), "{out}");
    }

    /// A daemon too old to publish a theme sends the field absent, which means system —
    /// not "unknown", and not an empty column.
    #[test]
    fn an_absent_theme_reads_as_system() {
        let preferences = Preferences {
            theme: None,
            ..Preferences::default()
        };
        assert!(show(&preferences).contains("theme               system"));
    }
}
