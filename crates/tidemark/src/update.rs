use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt;
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

/// Replaces this process with the same command, avoiding a race with GTK's single instance.
pub fn restart() -> io::Error {
    let Some(mut command) = restart_command(std::env::args_os()) else {
        return io::Error::new(io::ErrorKind::NotFound, "the program name is unavailable");
    };
    command.exec()
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
