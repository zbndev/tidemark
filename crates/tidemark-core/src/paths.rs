//! Where Tidemark keeps its files.
//!
//! The XDG base directory rules, with the part everyone skips: a relative
//! `XDG_DATA_HOME` is **ignored**, not joined onto the current directory. The
//! specification requires it, and a daemon that honoured a stray `XDG_DATA_HOME=.` would
//! write its history wherever it happened to be started from — a different database per
//! working directory, which looks exactly like data loss.

use std::path::{Path, PathBuf};

/// Directory name Tidemark owns under each base directory.
pub const APP_DIR: &str = "tidemark";

/// File name of the history database, per `CONTEXT.md` § Identity.
pub const HISTORY_FILE: &str = "history.db";

/// File name of the settings the user owns, per `CONTEXT.md` § Identity.
pub const CONFIG_FILE: &str = "config.toml";

/// Why a path could not be resolved.
#[derive(Debug, thiserror::Error)]
#[error("neither {variable} nor HOME names an absolute directory")]
pub struct NoBaseDirectory {
    /// The XDG variable that was consulted first.
    pub variable: &'static str,
}

/// `$XDG_DATA_HOME/tidemark`, or `$HOME/.local/share/tidemark`.
pub fn data_dir() -> Result<PathBuf, NoBaseDirectory> {
    resolve(
        "XDG_DATA_HOME",
        std::env::var_os("XDG_DATA_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
        ".local/share",
    )
}

/// `$XDG_CONFIG_HOME/tidemark`, or `$HOME/.config/tidemark`.
pub fn config_dir() -> Result<PathBuf, NoBaseDirectory> {
    resolve(
        "XDG_CONFIG_HOME",
        std::env::var_os("XDG_CONFIG_HOME")
            .as_deref()
            .map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
        ".config",
    )
}

/// Full path of the history database.
pub fn history_path() -> Result<PathBuf, NoBaseDirectory> {
    Ok(data_dir()?.join(HISTORY_FILE))
}

/// Full path of the settings file.
pub fn config_path() -> Result<PathBuf, NoBaseDirectory> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// The resolution rule, with the environment passed in so it is testable without mutating
/// the process — `std::env::set_var` is `unsafe` in this edition, and for good reason: the
/// test suite is threaded.
fn resolve(
    variable: &'static str,
    xdg: Option<&Path>,
    home: Option<&Path>,
    home_relative: &str,
) -> Result<PathBuf, NoBaseDirectory> {
    if let Some(base) = xdg.filter(|p| p.is_absolute()) {
        return Ok(base.join(APP_DIR));
    }
    home.filter(|p| p.is_absolute())
        .map(|home| home.join(home_relative).join(APP_DIR))
        .ok_or(NoBaseDirectory { variable })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(xdg: Option<&str>, home: Option<&str>) -> Result<PathBuf, NoBaseDirectory> {
        resolve(
            "XDG_DATA_HOME",
            xdg.map(Path::new),
            home.map(Path::new),
            ".local/share",
        )
    }

    #[test]
    fn the_xdg_variable_wins_when_it_is_absolute() {
        assert_eq!(
            data(Some("/srv/state"), Some("/home/u")).expect("resolvable"),
            PathBuf::from("/srv/state/tidemark")
        );
    }

    #[test]
    fn a_relative_xdg_variable_is_ignored_rather_than_joined() {
        // The whole reason this function takes its environment as arguments: honouring
        // `XDG_DATA_HOME=.` would give one history database per working directory.
        assert_eq!(
            data(Some("relative/path"), Some("/home/u")).expect("falls back to HOME"),
            PathBuf::from("/home/u/.local/share/tidemark")
        );
    }

    #[test]
    fn without_the_variable_the_default_location_is_used() {
        assert_eq!(
            data(None, Some("/home/u")).expect("resolvable"),
            PathBuf::from("/home/u/.local/share/tidemark")
        );
    }

    #[test]
    fn with_nothing_to_go_on_the_daemon_is_told_rather_than_guessing() {
        let err = data(None, None).expect_err("nowhere to write");
        assert!(err.to_string().contains("XDG_DATA_HOME"), "{err}");
    }

    #[test]
    fn the_settings_and_the_history_live_under_different_bases() {
        // Two XDG variables, and the one a step confuses is the one that moves a file the
        // user hand-edits out from under them.
        let config = resolve(
            "XDG_CONFIG_HOME",
            None,
            Some(Path::new("/home/u")),
            ".config",
        )
        .expect("resolvable")
        .join(CONFIG_FILE);
        assert_eq!(
            config,
            PathBuf::from("/home/u/.config/tidemark/config.toml")
        );
    }

    #[test]
    fn the_history_lives_in_the_data_directory_under_the_documented_name() {
        let path = data(Some("/srv/state"), None)
            .expect("resolvable")
            .join(HISTORY_FILE);
        assert_eq!(path, PathBuf::from("/srv/state/tidemark/history.db"));
    }
}
