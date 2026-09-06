use std::ffi::OsStr;
use std::io;
use std::process::Command;

use semver::{Error, Version};

pub(crate) const RELEASES_URL: &str = "https://github.com/zbndev/tidemark/releases";

pub(crate) fn update_tooltip(version: &str) -> Option<String> {
    (!version.is_empty()).then(|| format!("Tidemark {version} is available"))
}

/// Remembers which daemon release this client has already offered to restart into.
#[derive(Debug)]
pub struct UpdateNotice {
    client: Version,
    offered: Option<Version>,
}

impl UpdateNotice {
    pub fn new(client: &str) -> Self {
        Self {
            client: Version::parse(client).expect("Cargo always supplies a SemVer package version"),
            offered: None,
        }
    }

    /// Returns true once for each successively newer daemon release.
    pub fn consider(&mut self, daemon: &str) -> Result<bool, Error> {
        let daemon = Version::parse(daemon)?;
        let newer_than_client = daemon > self.client;
        let newer_than_offer = self
            .offered
            .as_ref()
            .is_none_or(|offered| daemon > *offered);
        if newer_than_client && newer_than_offer {
            self.offered = Some(daemon);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

fn restart_command<I, S>(args: I) -> Option<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut args = args.into_iter();
    let program = args.next()?;
    let mut command = Command::new(program);
    command.args(args);
    Some(command)
}

/// Replaces this process with the same command, avoiding a race with GTK's single
/// instance. Returns only when the restart did not happen; the caller turns the error
/// into the "could not restart" dialog.
pub fn restart() -> io::Error {
    let Some(command) = restart_command(std::env::args_os()) else {
        return io::Error::new(io::ErrorKind::NotFound, "the program name is unavailable");
    };
    restart_process(command)
}

/// Unix can swap the program in place: the exec keeps the process identity, so GTK's
/// single-instance lock never sees two holders, and argv[0] is resolved exactly as it
/// was for this invocation.
#[cfg(unix)]
fn restart_process(mut command: Command) -> io::Error {
    use std::os::unix::process::CommandExt;

    command.exec()
}

/// Windows has no exec, so the successor is spawned first and this process exits only
/// once it exists: there is never a moment without a process, the brief overlap of the
/// two instances is what the platform offers instead. The successor's executable is
/// [`restart_sibling`]'s resolution of the original program argument — never a PATH
/// search.
#[cfg(windows)]
fn restart_process(command: Command) -> io::Error {
    match restart_sibling(&command) {
        Ok(mut sibling) => match sibling.spawn() {
            Ok(_child) => std::process::exit(0),
            Err(error) => error,
        },
        Err(error) => error,
    }
}

/// Resolves the next instance's executable the way the Unix arm's exec() does — from
/// the original program argument — but anchored to this process's own directory on
/// disk rather than a PATH search. A bare or relative argv[0] keeps its name and is
/// looked up next to `current_exe()`, mirroring the Unix arm's reuse of the original
/// program; an absolute argv[0] names where this instance happened to be started from
/// rather than where the updater just wrote, so the successor is this process's own
/// file instead. (When no program argument exists at all, [`restart`] has already
/// answered with the same NotFound error on every platform.)
#[cfg(windows)]
fn restart_sibling(command: &Command) -> io::Result<Command> {
    let exe = std::env::current_exe()?;
    let original = std::path::Path::new(command.get_program());
    let program = match exe.parent() {
        Some(dir) if !original.is_absolute() => dir.join(original),
        _ => exe,
    };
    let mut sibling = Command::new(program);
    sibling.args(command.get_args());
    Ok(sibling)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{UpdateNotice, restart_command, update_tooltip};

    #[test]
    fn an_empty_update_has_no_button_copy() {
        assert_eq!(update_tooltip(""), None);
    }

    #[test]
    fn an_available_update_names_the_daemon_selected_version() {
        assert_eq!(
            update_tooltip("0.12.3").as_deref(),
            Some("Tidemark 0.12.3 is available")
        );
    }

    #[test]
    fn a_newer_daemon_is_offered_even_when_lexical_order_would_disagree() {
        let mut notice = UpdateNotice::new("0.9.0");

        assert!(notice.consider("0.10.0").unwrap());
    }

    #[test]
    fn the_same_daemon_release_is_only_offered_once() {
        let mut notice = UpdateNotice::new("0.1.0");

        assert!(notice.consider("0.2.0").unwrap());
        assert!(!notice.consider("0.2.0").unwrap());
        assert!(notice.consider("0.3.0").unwrap());
    }

    #[test]
    fn equal_older_and_invalid_daemon_versions_are_not_updates() {
        let mut notice = UpdateNotice::new("1.2.3");

        assert!(!notice.consider("1.2.3").unwrap());
        assert!(!notice.consider("1.2.2").unwrap());
        assert!(notice.consider("development build").is_err());
    }

    #[test]
    fn restart_reuses_the_original_program_and_arguments() {
        let command = restart_command(["tidemark", "--background"]).unwrap();

        assert_eq!(command.get_program(), OsStr::new("tidemark"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("--background")]
        );
    }
}

/// The Windows restart resolves its successor from the original program argument,
/// anchored to this executable's own directory; this pins that resolution without
/// spawning anything.
#[cfg(all(test, windows))]
mod restart_tests {
    use std::ffi::OsStr;

    use super::{restart_command, restart_sibling};

    #[test]
    fn a_bare_program_name_is_anchored_to_this_executables_directory() {
        let original = restart_command(["tidemark", "--background"]).unwrap();
        let sibling = restart_sibling(&original).unwrap();

        let expected = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("tidemark");
        assert_eq!(sibling.get_program(), expected);
        assert_eq!(
            sibling.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("--background")]
        );
    }

    #[test]
    fn an_absolute_program_name_falls_back_to_this_executable() {
        let original = restart_command([r"C:\elsewhere\tidemark.exe", "--background"]).unwrap();
        let sibling = restart_sibling(&original).unwrap();

        assert_eq!(sibling.get_program(), std::env::current_exe().unwrap());
        assert_eq!(
            sibling.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("--background")]
        );
    }
}
