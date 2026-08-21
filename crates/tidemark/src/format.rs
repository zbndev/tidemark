//! Turning the published numbers into the words on the card.
//!
//! Everything here is a pure function of a [`ProviderStatus`] and the current instant, so
//! the phrasing is testable without a display — which matters more than it sounds, because
//! the interesting cases are the ones nobody wants to reproduce by waiting: a reset that is
//! already overdue, a reading taken three days ago, a quota that is not quite zero.
//!
//! One rule runs through all of it, the same one that governs the wire format: **a number
//! the daemon did not send is not shown.** Every function that depends on an absent field
//! returns `None` rather than a plausible-looking placeholder.

use tidemark_types::present::{duration, plural};
use tidemark_types::{ProviderState, ProviderStatus, Remedy, Timestamp};

pub use tidemark_types::present::percent;

/// How much emphasis a chip gets. Maps to the libadwaita style classes, which is why there
/// are three of them rather than one per [`ProviderState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Nothing is wrong; the chip is only saying what is happening.
    Neutral,
    /// Worth noticing, but it resolves on its own.
    Attention,
    /// Needs the user, or a new release.
    Danger,
}

impl Tone {
    /// The libadwaita style class that colours a label this way.
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Neutral => "dim-label",
            Self::Attention => "warning",
            Self::Danger => "error",
        }
    }

    /// Every class this enum can apply, so a widget can drop the previous one.
    pub const ALL_CLASSES: [&'static str; 3] = ["dim-label", "warning", "error"];
}

/// The short label shown next to the provider's name when something is going on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// What it says.
    pub text: String,
    /// How loudly.
    pub tone: Tone,
}

/// The chip for a status, or `None` when there is nothing to say.
///
/// A healthy account gets no chip at all. "ok" written on five cards is five words that
/// never change, and it would make the one card that *does* say something harder to spot,
/// not easier.
pub fn chip(status: &ProviderStatus) -> Option<Chip> {
    let Some(state) = status.state() else {
        // A state string this build does not know. Showing it verbatim is the honest
        // option: it is a problem, and the daemon is the one that can name it.
        return Some(Chip {
            text: status.state.clone(),
            tone: Tone::Danger,
        });
    };

    let text = match state {
        ProviderState::Ok => return None,
        ProviderState::Pending => "checking…",
        ProviderState::NoCredential => "no key",
        ProviderState::WaitingForKeyring => "keyring locked",
        ProviderState::KeyringUnavailable => "no keyring",
        ProviderState::CredentialRejected => "key rejected",
        ProviderState::RateLimited => "rate limited",
        ProviderState::Unreachable => "offline",
        ProviderState::Malformed => "unreadable",
    };

    let tone = match state.remedy() {
        Remedy::Nothing => Tone::Neutral,
        Remedy::ItFixesItself => Tone::Attention,
        Remedy::YouFixIt | Remedy::TheyBrokeIt => Tone::Danger,
    };

    Some(Chip {
        text: text.to_owned(),
        tone,
    })
}

/// When the window rolls over, phrased for the line under the bar.
pub fn resets_in(seconds: i64) -> String {
    if seconds <= 0 {
        "resetting now".to_owned()
    } else {
        format!("resets in {}", duration(seconds))
    }
}

/// How long ago something happened.
pub fn ago(seconds: i64) -> String {
    if seconds < 45 {
        return "just now".to_owned();
    }
    let minutes = (seconds + 30) / 60;
    if minutes < 60 {
        return format!("{} ago", plural(minutes.max(1), "minute"));
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{} ago", plural(hours, "hour"));
    }
    format!("{} ago", plural(hours / 24, "day"))
}

/// The line along the bottom of the card: when the reading was taken, and when the daemon
/// intends to take the next one.
///
/// When the next poll is due is deliberately not here. It is the daemon's schedule, not
/// news about the account, and on a card that updates itself the countdown was one more
/// number moving for no reason the reader has to act on.
///
/// `None` while there is nothing true to say — an account that has never been polled, which
/// is what a status looks like between the daemon starting and its first attempt.
pub fn footer(status: &ProviderStatus, now: Timestamp) -> Option<String> {
    status
        .captured_at
        .map(|at| format!("checked {}", ago(now.as_unix() - at)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId};

    fn status() -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default())
    }

    #[test]
    fn a_healthy_account_says_nothing_at_all() {
        let mut status = status();
        status.set_state(ProviderState::Ok, None);
        assert_eq!(chip(&status), None);
    }

    #[test]
    fn the_three_groups_are_three_tones() {
        let mut status = status();
        status.set_state(ProviderState::NoCredential, None);
        assert_eq!(chip(&status).expect("a chip").tone, Tone::Danger);
        status.set_state(ProviderState::RateLimited, None);
        assert_eq!(chip(&status).expect("a chip").tone, Tone::Attention);
        status.set_state(ProviderState::Pending, None);
        assert_eq!(chip(&status).expect("a chip").tone, Tone::Neutral);
    }

    #[test]
    fn a_state_from_a_newer_daemon_is_shown_rather_than_swallowed() {
        let mut status = status();
        status.state = "quota-frozen".into();
        let chip = chip(&status).expect("an unknown state is still a problem");
        assert_eq!(chip.text, "quota-frozen");
        assert_eq!(chip.tone, Tone::Danger);
    }

    #[test]
    fn an_overdue_reset_is_happening_rather_than_negative() {
        assert_eq!(resets_in(0), "resetting now");
        assert_eq!(resets_in(-600), "resetting now");
        assert_eq!(resets_in(900), "resets in 15 min");
    }

    #[test]
    fn recent_readings_read_as_recent() {
        assert_eq!(ago(3), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(600), "10 minutes ago");
        assert_eq!(ago(7200), "2 hours ago");
        assert_eq!(ago(3 * 86_400), "3 days ago");
    }

    #[test]
    fn the_footer_reports_only_what_the_daemon_actually_said() {
        let now = Timestamp::from_unix(1_785_700_000).expect("plausible");
        let mut status = status();
        assert_eq!(footer(&status, now), None, "nothing known, nothing claimed");

        // A scheduled poll is not a reading, and the footer is about the reading.
        status.next_poll_at = Some(now.as_unix() + 120);
        assert_eq!(footer(&status, now), None, "a schedule is not news");

        status.captured_at = Some(now.as_unix() - 600);
        assert_eq!(
            footer(&status, now).as_deref(),
            Some("checked 10 minutes ago")
        );
    }

    #[test]
    fn the_plan_line_comes_from_the_section_the_adapters_agree_on() {
        // Not a formatting rule of ours, but the card depends on it, so it is asserted
        // where the card would break.
        let mut status = status();
        status.details = vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Level".into(),
                value: "pro".into(),
            }],
        }];
        assert_eq!(status.plan(), Some("pro"));
    }
}
