//! "May I start a long run right now?", answered with an exit code.
//!
//! Only an `ok` account's reading is judged. A last-good number sitting behind a rejected
//! credential or a rate limit would answer "safe" about quota that cannot be spent, so
//! those exit 69 — unavailable — and a script can tell "no answer" from "no quota".

use tidemark_types::{ProviderState, ProviderStatus};

use crate::exit::Exit;
use crate::format;

/// Which window the verdict is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The window a card leads with.
    Dominant,
    /// One window by its stable key.
    Named(String),
    /// Every window of every selected account; the fullest one decides.
    Any,
}

/// What the guard concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Safe {
        remaining: f64,
    },
    Below {
        remaining: f64,
    },
    /// Nothing trustworthy to judge, with the reason for stderr.
    Unavailable(String),
}

impl Verdict {
    pub const fn exit(&self) -> Exit {
        match self {
            Self::Safe { .. } => Exit::Ok,
            Self::Below { .. } => Exit::Below,
            Self::Unavailable(_) => Exit::Unavailable,
        }
    }
}

/// The fullest selected window against the threshold. Every selected account must be `ok`
/// and must have the window asked for; anything else is unavailable rather than a guess.
pub fn decide(statuses: &[&ProviderStatus], selection: &Selection, min_remaining: u8) -> Verdict {
    if statuses.is_empty() {
        return Verdict::Unavailable("no configured account matches".to_owned());
    }

    let mut fullest: Option<f64> = None;
    for status in statuses {
        if ProviderState::from_wire(&status.state) != Some(ProviderState::Ok) {
            return Verdict::Unavailable(format!(
                "{} · {} is {}",
                status.provider, status.account, status.state
            ));
        }
        let used = match selection {
            Selection::Dominant => format::dominant(status).map(|window| window.used_percent),
            Selection::Named(key) => status
                .windows
                .iter()
                .find(|window| &window.key == key)
                .map(|window| window.used_percent),
            Selection::Any => status
                .windows
                .iter()
                .map(|window| window.used_percent)
                .fold(None, |worst: Option<f64>, used| {
                    Some(worst.map_or(used, |worst| worst.max(used)))
                }),
        };
        let Some(used) = used else {
            return Verdict::Unavailable(match selection {
                Selection::Named(key) => format!(
                    "{} · {} reports no window {key}",
                    status.provider, status.account
                ),
                _ => format!("{} · {} has no reading", status.provider, status.account),
            });
        };
        fullest = Some(fullest.map_or(used, |worst: f64| worst.max(used)));
    }

    let remaining = 100.0 - fullest.unwrap_or(100.0);
    if remaining >= f64::from(min_remaining) {
        Verdict::Safe { remaining }
    } else {
        Verdict::Below { remaining }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, WindowStatus};

    fn window(key: &str, used_percent: f64) -> WindowStatus {
        WindowStatus {
            key: key.to_owned(),
            title: key.to_owned(),
            subtitle: None,
            used_percent,
            resets_at: None,
            length_secs: Some(18_000),
            blocked_by: None,
        }
    }

    fn status(state: ProviderState, windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status =
            ProviderStatus::pending(&ProviderId::new("claude".to_owned()), &AccountId::default());
        status.state = state.as_wire().to_owned();
        status.captured_at = Some(1_785_704_400);
        status.windows = windows;
        status
    }

    #[test]
    fn enough_left_is_safe() {
        let status = status(ProviderState::Ok, vec![window("w18000", 40.0)]);
        let verdict = decide(&[&status], &Selection::Dominant, 20);
        assert_eq!(verdict, Verdict::Safe { remaining: 60.0 });
        assert_eq!(verdict.exit(), Exit::Ok);
    }

    #[test]
    fn too_little_left_is_below() {
        let status = status(ProviderState::Ok, vec![window("w18000", 85.0)]);
        let verdict = decide(&[&status], &Selection::Dominant, 20);
        assert_eq!(verdict, Verdict::Below { remaining: 15.0 });
        assert_eq!(verdict.exit(), Exit::Below);
    }

    #[test]
    fn a_named_window_that_is_not_there_is_unavailable_not_safe() {
        let status = status(ProviderState::Ok, vec![window("w18000", 5.0)]);
        let verdict = decide(&[&status], &Selection::Named("w604800".to_owned()), 20);
        assert_eq!(verdict.exit(), Exit::Unavailable);
    }

    #[test]
    fn a_stale_reading_is_never_judged() {
        let status = status(
            ProviderState::CredentialRejected,
            vec![window("w18000", 1.0)],
        );
        let verdict = decide(&[&status], &Selection::Dominant, 20);
        assert_eq!(verdict.exit(), Exit::Unavailable);
    }

    #[test]
    fn any_window_judges_the_fullest_one() {
        let status = status(
            ProviderState::Ok,
            vec![window("w18000", 10.0), window("w604800", 95.0)],
        );
        assert_eq!(decide(&[&status], &Selection::Any, 20).exit(), Exit::Below);
        assert_eq!(
            decide(&[&status], &Selection::Dominant, 20).exit(),
            Exit::Ok
        );
    }

    #[test]
    fn selecting_no_account_is_unavailable() {
        assert_eq!(
            decide(&[], &Selection::Dominant, 20).exit(),
            Exit::Unavailable
        );
    }
}
