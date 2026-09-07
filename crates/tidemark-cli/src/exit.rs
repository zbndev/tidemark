//! How the process ends. `guard`'s codes *are* its output, so every code is named once
//! here and shared, and the two failures a script reacts to differently — nobody answered,
//! versus the daemon refused — never collapse into the same number.

/// Process exit codes, following sysexits where one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what it was asked.
    Ok = 0,
    /// `guard` only: less quota is left than was asked for.
    #[expect(
        dead_code,
        reason = "guard is the only caller, and it lands in a later commit"
    )]
    Below = 1,
    /// The arguments do not name a command this build can run (`EX_USAGE`).
    #[expect(
        dead_code,
        reason = "for arguments this program rejects itself; clap's own errors exit 2"
    )]
    Usage = 64,
    /// The daemon could not be reached, or there is no reading to judge (`EX_UNAVAILABLE`).
    Unavailable = 69,
    /// The daemon answered with an error (`EX_SOFTWARE`).
    Daemon = 70,
}

impl Exit {
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Why a command stopped, in the form the process exits with.
#[derive(Debug)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    #[expect(
        dead_code,
        reason = "the first self-rejected argument is in a later commit"
    )]
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Usage,
            message: message.into(),
        }
    }

    #[expect(
        dead_code,
        reason = "the first missing-reading path is in a later commit"
    )]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Unavailable,
            message: message.into(),
        }
    }
}

/// An error *reply* is the daemon refusing something; anything else is the daemon not being
/// there. A script retries one and gives up on the other.
impl From<zbus::Error> for Failure {
    fn from(error: zbus::Error) -> Self {
        let exit = match &error {
            zbus::Error::MethodError(..) | zbus::Error::FDO(_) => Exit::Daemon,
            _ => Exit::Unavailable,
        };
        Self {
            exit,
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self {
            exit: Exit::Daemon,
            message: format!("cannot serialize the daemon's answer: {error}"),
        }
    }
}
