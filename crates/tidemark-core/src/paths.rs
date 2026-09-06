//! Where Tidemark keeps its files.
//!
//! The XDG base directory rules, with the part everyone skips: a relative
//! `XDG_DATA_HOME` is **ignored**, not joined onto the current directory. The
//! specification requires it, and a daemon that honoured a stray `XDG_DATA_HOME=.` would
//! write its history wherever it happened to be started from — a different database per
//! working directory, which looks exactly like data loss.
//!
//! The OS-specific half lives in the private `platform` module: Unix keeps the
//! XDG rule below, Windows maps everything onto the local-only
//! `%LOCALAPPDATA%\tidemark` root.

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

/// `$XDG_DATA_HOME/tidemark`, or `$HOME/.local/share/tidemark`. On Windows,
/// `%LOCALAPPDATA%\tidemark`.
pub fn data_dir() -> Result<PathBuf, NoBaseDirectory> {
    platform::data_dir()
}

/// `$XDG_CONFIG_HOME/tidemark`, or `$HOME/.config/tidemark`. On Windows,
/// `%LOCALAPPDATA%\tidemark` too — a single local root, never the roaming
/// `%APPDATA%`, matching Credential Manager's local-only stance.
pub fn config_dir() -> Result<PathBuf, NoBaseDirectory> {
    platform::config_dir()
}

/// Full path of the history database.
pub fn history_path() -> Result<PathBuf, NoBaseDirectory> {
    Ok(data_dir()?.join(HISTORY_FILE))
}

/// Full path of the settings file.
pub fn config_path() -> Result<PathBuf, NoBaseDirectory> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// The user's home directory for third-party vendor files: `$HOME` when it names an
/// absolute directory, else on Windows `%USERPROFILE%`. Native Windows processes —
/// Explorer, autostart, the UI-spawned daemon — never carry `HOME`, while every vendor
/// CLI keeps its login under the profile; without the fallback the daemon cannot start
/// there at all. Absolute-only, like every rule in this module; `None` when nothing
/// usable names one.
pub fn home() -> Option<PathBuf> {
    home_in(
        std::env::var_os("HOME").map(PathBuf::from),
        fallback_profile(),
    )
}

/// The second place [`home`] can come from: `%USERPROFILE%` on Windows, where the
/// vendor CLIs keep their logins. Unix has none — HOME-only there.
#[cfg(windows)]
fn fallback_profile() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// [`home`] without the process environment, so tests never mutate it — the same
/// reason `resolve` takes its inputs as arguments.
#[cfg(not(windows))]
fn fallback_profile() -> Option<PathBuf> {
    None
}

fn home_in(home: Option<PathBuf>, fallback: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(home) = home.filter(|home| home.is_absolute()) {
        return Some(home);
    }
    used_fallback(fallback)
}

/// The fallback counts only where one exists (Windows `%USERPROFILE%`).
#[cfg(windows)]
fn used_fallback(fallback: Option<PathBuf>) -> Option<PathBuf> {
    fallback.filter(|fallback| fallback.is_absolute())
}

/// Unix keeps the HOME-only rule: the fallback never counts there.
#[cfg(not(windows))]
fn used_fallback(_fallback: Option<PathBuf>) -> Option<PathBuf> {
    None
}

/// The resolution rule, with the environment passed in so it is testable without mutating
/// the process — `std::env::set_var` is `unsafe` in this edition, and for good reason: the
/// test suite is threaded. The Windows arm does not consult XDG variables, so outside
/// Unix this rule compiles for the tests alone.
#[cfg_attr(not(unix), allow(dead_code))]
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

/// The OS-specific half of path resolution — plain cfg-selected functions, no
/// trait objects: the seam is one call deep and both arms are final. Exactly one
/// of the two modules below is compiled per target.
#[cfg(unix)]
mod platform {
    use super::{NoBaseDirectory, Path, PathBuf, resolve};

    pub(super) fn data_dir() -> Result<PathBuf, NoBaseDirectory> {
        resolve(
            "XDG_DATA_HOME",
            std::env::var_os("XDG_DATA_HOME").as_deref().map(Path::new),
            std::env::var_os("HOME").as_deref().map(Path::new),
            ".local/share",
        )
    }

    pub(super) fn config_dir() -> Result<PathBuf, NoBaseDirectory> {
        resolve(
            "XDG_CONFIG_HOME",
            std::env::var_os("XDG_CONFIG_HOME")
                .as_deref()
                .map(Path::new),
            std::env::var_os("HOME").as_deref().map(Path::new),
            ".config",
        )
    }
}

#[cfg(windows)]
mod platform {
    use super::{APP_DIR, NoBaseDirectory, Path, PathBuf};

    pub(super) fn data_dir() -> Result<PathBuf, NoBaseDirectory> {
        resolve(std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new))
    }

    // One root for both directories: Credential Manager secrets are
    // per-machine local, so nothing may drift to the roaming %APPDATA% and
    // split the app's state across a roaming profile.
    pub(super) fn config_dir() -> Result<PathBuf, NoBaseDirectory> {
        resolve(std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new))
    }

    /// `%LOCALAPPDATA%\tidemark`, mirroring the HOME-missing error shape of
    /// the Unix rule when the variable is unset (or not absolute). The
    /// environment is a parameter for the same testability reason as the XDG
    /// rule — no process-global mutation from tests.
    fn resolve(localappdata: Option<&Path>) -> Result<PathBuf, NoBaseDirectory> {
        localappdata
            .filter(|base| base.is_absolute())
            .map(|base| base.join(APP_DIR))
            .ok_or(NoBaseDirectory {
                variable: "LOCALAPPDATA",
            })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn data_and_config_share_one_local_root() {
            let base = Path::new(r"C:\Users\alice\AppData\Local");
            assert_eq!(resolve(Some(base)).expect("resolvable"), base.join(APP_DIR));
        }

        #[test]
        fn an_unset_localappdata_is_reported_not_guessed() {
            let error = resolve(None).expect_err("nowhere to write");
            assert!(error.to_string().contains("LOCALAPPDATA"), "{error}");
        }

        #[test]
        fn an_empty_or_relative_localappdata_is_rejected() {
            // The same absolute-only rule as the XDG variables: never join a
            // stray value onto the working directory.
            assert!(resolve(Some(Path::new(""))).is_err());
            assert!(resolve(Some(Path::new(r"AppData\Local"))).is_err());
            assert!(resolve(Some(Path::new("."))).is_err());
        }

        #[test]
        fn odd_but_absolute_values_pass_through_unchanged() {
            let base = Path::new(r"C:\Users\ünïcode ör\AppData\Local");
            assert_eq!(resolve(Some(base)).expect("resolvable"), base.join(APP_DIR));
        }

        #[test]
        fn the_public_helpers_read_localappdata_from_the_environment() {
            let expected = rendered(resolve(
                std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new),
            ));
            assert_eq!(rendered(data_dir()), expected);
            assert_eq!(rendered(config_dir()), expected);
        }

        /// `NoBaseDirectory` deliberately has no `PartialEq`; compare through
        /// the rendered form instead.
        fn rendered(res: Result<PathBuf, NoBaseDirectory>) -> Result<PathBuf, String> {
            res.map_err(|error| error.to_string())
        }
    }
}

// The XDG-variable resolution these tests pin is the unix arm's contract; the
// windows arm maps LOCALAPPDATA instead (see the windows tests module above).
#[cfg(all(test, unix))]
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

    #[test]
    fn a_relative_home_is_rejected_too_rather_than_joined() {
        // Characterization (split guard): the HOME fallback gets the same
        // absolute-only treatment as the XDG variable — a relative HOME must not
        // relocate the database into whatever directory the daemon started in.
        let err = data(None, Some("relative/home")).expect_err("nowhere safe to write");
        assert!(err.to_string().contains("HOME"), "{err}");
    }

    #[test]
    fn a_relative_xdg_variable_with_no_home_is_an_error_not_a_guess() {
        // Characterization (split guard): the ignored relative variable cannot
        // silently become the base when there is no HOME to fall back to.
        let err = data(Some("."), None).expect_err("nowhere to write");
        assert!(err.to_string().contains("XDG_DATA_HOME"), "{err}");
    }

    #[test]
    fn the_error_names_both_variables_it_considered() {
        // Characterization (split guard): pin the user-facing error text verbatim
        // — it reaches the daemon's startup diagnostics, and the split must not
        // reword it.
        assert_eq!(
            data(None, None).expect_err("nowhere to write").to_string(),
            "neither XDG_DATA_HOME nor HOME names an absolute directory"
        );
    }

    #[test]
    fn data_dir_reads_the_documented_variables_from_the_environment() {
        // Characterization (split guard) of the env-reading wiring — the arm the
        // platform split moves. Reads (never mutates) the process environment and
        // checks the public helper against the rule applied to the same inputs, so
        // a swapped variable name or suffix changes the result wherever the two
        // variables disagree.
        let expected = resolve(
            "XDG_DATA_HOME",
            std::env::var_os("XDG_DATA_HOME").as_deref().map(Path::new),
            std::env::var_os("HOME").as_deref().map(Path::new),
            ".local/share",
        );
        assert_eq!(rendered(data_dir()), rendered(expected));
    }

    #[test]
    fn config_dir_reads_the_documented_variables_from_the_environment() {
        // Characterization (split guard), same as the data_dir one above.
        let expected = resolve(
            "XDG_CONFIG_HOME",
            std::env::var_os("XDG_CONFIG_HOME")
                .as_deref()
                .map(Path::new),
            std::env::var_os("HOME").as_deref().map(Path::new),
            ".config",
        );
        assert_eq!(rendered(config_dir()), rendered(expected));
    }

    /// `NoBaseDirectory` deliberately has no `PartialEq`; compare through the
    /// rendered form instead so both `Ok` paths and error texts are pinned.
    fn rendered(res: Result<PathBuf, NoBaseDirectory>) -> Result<PathBuf, String> {
        res.map_err(|error| error.to_string())
    }
}

/// [`home`](super::home) without the process environment: `temp_dir` is absolute on
/// every platform, so these cases pin the rule without mutating anything global.
#[cfg(test)]
mod home_tests {
    use super::{PathBuf, home_in};

    fn dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn an_absolute_home_wins_over_any_fallback() {
        let home = dir("tidemark-home");
        assert_eq!(
            home_in(Some(home.clone()), Some(dir("tidemark-profile"))),
            Some(home)
        );
    }

    #[test]
    fn a_relative_home_is_rejected_not_joined() {
        // Same absolute-only discipline as the XDG rule: a relative HOME must
        // never relocate a vendor login into the daemon's working directory.
        assert_eq!(home_in(Some(PathBuf::from("relative/home")), None), None);
    }

    #[test]
    fn the_fallback_counts_only_on_windows() {
        let profile = dir("tidemark-profile");
        assert_eq!(
            home_in(None, Some(profile.clone())),
            if cfg!(windows) { Some(profile) } else { None }
        );
    }

    #[test]
    fn a_relative_fallback_is_no_home_at_all() {
        assert_eq!(home_in(None, Some(PathBuf::from("relative/profile"))), None);
    }
}
