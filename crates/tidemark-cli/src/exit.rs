//! How the process ends. `guard`'s codes *are* its output, so every code is named once
//! here and shared, and the two failures a script reacts to differently — nobody answered,
//! versus the daemon refused — never collapse into the same number.

/// Process exit codes, following sysexits where one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what it was asked.
    Ok = 0,
    /// `guard` only: less quota is left than was asked for.
    Below = 1,
    /// The arguments do not name a command this build can run (`EX_USAGE`).
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
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Usage,
            message: message.into(),
        }
    }

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

/// A stream whose reader went away — `head`, a killed panel plugin — is not the daemon's
/// fault, and there is nowhere left to print anything about it.
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self {
            exit: Exit::Unavailable,
            message: format!("cannot write the stream: {error}"),
        }
    }
}
