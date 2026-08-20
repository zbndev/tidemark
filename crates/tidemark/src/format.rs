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

use tidemark_types::{ProviderState, ProviderStatus, Remedy, Timestamp};

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

/// Consumption as the big number on the card.
///
/// Rounds, but never across the ends: a window with something spent in it never reads `0%`,
/// and one with anything left never reads `100%`. Those two are the readings a person acts
/// on, and rounding is not a good enough reason to get either wrong.
pub fn percent(used_percent: f64) -> String {
    let used = used_percent.clamp(0.0, 100.0);
    let rounded = used.round();
    if rounded <= 0.0 && used > 0.0 {
        "<1%".to_owned()
    } else if rounded >= 100.0 && used < 100.0 {
        ">99%".to_owned()
    } else {
        format!("{rounded:.0}%")
    }
}

/// A span of time, at the coarsest unit that still says something useful.
pub fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if minutes == 0 {
        "under a minute".to_owned()
    } else if hours == 0 {
        format!("{minutes} min")
    } else if days == 0 {
        match minutes % 60 {
            0 => format!("{hours} h"),
            rest => format!("{hours} h {rest} min"),
        }
    } else {
        match hours % 24 {
            0 => plural(days, "day"),
            rest => format!("{} {rest} h", plural(days, "day")),
        }
    }
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

fn plural(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
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
    fn rounding_never_reports_an_untouched_window_or_an_exhausted_one_by_mistake() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(
            percent(0.2),
            "<1%",
            "something was spent; do not print zero"
        );
        assert_eq!(
            percent(99.7),
            ">99%",
            "there is quota left; do not print 100"
        );
        assert_eq!(percent(100.0), "100%");
        assert_eq!(percent(42.4), "42%");
        assert_eq!(percent(42.5), "43%");
    }

    #[test]
    fn a_percentage_outside_the_range_is_clamped_rather_than_printed() {
        assert_eq!(percent(140.0), "100%");
        assert_eq!(percent(-3.0), "0%");
    }

    #[test]
    fn durations_stop_at_the_unit_that_still_means_something() {
        assert_eq!(duration(30), "under a minute");
        assert_eq!(duration(90), "1 min");
        assert_eq!(duration(3600), "1 h");
        assert_eq!(duration(3600 + 12 * 60), "1 h 12 min");
        assert_eq!(duration(48 * 3600), "2 days");
        assert_eq!(duration(50 * 3600), "2 days 2 h");
        assert_eq!(duration(-5), "under a minute");
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
