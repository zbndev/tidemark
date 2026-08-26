//! When to poll next.
//!
//! Kept as one pure function over a described [`Situation`], with no clock and no I/O in
//! it, because every interesting case here is one nobody wants to reproduce live: a
//! provider asking for an hour, a window five minutes from rolling over, a keyring that
//! is still locked.
//!
//! The intervals themselves come from `CONTEXT.md` § Polling and are not this module's to
//! reinvent: in Auto the pace follows the quota zones — ten minutes in the blue, five in
//! the yellow, one in the red, ten again once the quota is exhausted — with the last
//! quarter hour before a reset watched at sixty seconds so the rollover is seen the
//! moment it happens; in Manual the user's chosen interval is the whole rule. Failures
//! back off exponentially from the baseline regardless of mode.

use std::time::Duration;
use tidemark_types::{DANGER_AT, Preferences, ProviderState, WARNING_AT};

/// How the daemon chooses a healthy account's next interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RefreshMode {
    /// Interval from the worst window's quota zone; the near-reset speedup applies.
    Auto,
    /// One fixed interval, and nothing else — the pace the user picked.
    Manual(Duration),
}

impl RefreshMode {
    /// The mode the settings file describes, for the daemon's construction. The file has
    /// been validated by the time this runs, so an unknown name cannot reach here.
    pub fn configured(preferences: &Preferences) -> Self {
        match preferences.refresh_mode.as_str() {
            Preferences::REFRESH_MANUAL => Self::Manual(Duration::from_secs(
                u64::from(preferences.refresh_minutes) * 60,
            )),
            _ => Self::Auto,
        }
    }
}

/// Interval when nothing special is happening: the seed exponential backoff doubles from,
/// and the pace a manual interval defaults to.
pub const BASELINE: Duration = Duration::from_secs(5 * 60);

/// Interval while a window is about to roll over. The rollover is the one event history
/// cannot reconstruct afterwards — it is the boundary every segment is measured from — so
/// this is where the polling budget goes.
pub const NEAR_RESET: Duration = Duration::from_secs(60);

/// How close to a reset counts as near it.
pub const NEAR_RESET_HORIZON_SECS: i64 = 15 * 60;

/// Auto interval while the worst window is in the blue zone — under [`WARNING_AT`] used,
/// or exhausted entirely: nothing more can be spent, and the reset is watched for
/// separately by [`NEAR_RESET`].
pub const AUTO_BLUE: Duration = Duration::from_secs(10 * 60);

/// Auto interval while the worst window is between [`WARNING_AT`] and [`DANGER_AT`].
pub const AUTO_YELLOW: Duration = Duration::from_secs(5 * 60);

/// Auto interval while the worst window has passed [`DANGER_AT`] but is not exhausted.
pub const AUTO_RED: Duration = Duration::from_secs(60);

/// Ceiling on backoff we invent ourselves. A provider that explicitly asks for longer is
/// still obeyed: this caps our guessing, not their instruction.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// How often to re-ask a locked keyring. Frequent because it costs one local D-Bus call
/// and never touches the network, and because the answer changes the moment the user logs
/// in — which is the whole reason the daemon starts before the keyring is unlocked.
pub const KEYRING_RETRY: Duration = Duration::from_secs(60);

/// How often to re-check a state only the user can clear — no key stored, key rejected, no
/// Secret Service at all. Slow on purpose: the interface has a `Refresh` method, so the
/// moment the user fixes it they get an immediate poll rather than waiting this out.
pub const USER_ACTION_RETRY: Duration = Duration::from_secs(30 * 60);

/// Everything the interval depends on, so that choosing one needs no clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Situation {
    /// What the last attempt left the account in.
    pub state: ProviderState,
    /// How many attempts in a row have failed. Zero after any success.
    pub failures: u32,
    /// What the provider asked us to wait, when it asked.
    pub retry_after: Option<Duration>,
    /// Seconds until the soonest reset among the windows we last saw. Negative when a
    /// reset is overdue, which is still near it. `None` when no window said.
    pub seconds_to_next_reset: Option<i64>,
    /// The refresh mode the daemon is running: zones, or the user's fixed interval.
    pub mode: RefreshMode,
    /// How much the account's most-used window had consumed, as of the last good
    /// reading. `None` before there is anything to pace by — which paces as blue.
    pub worst_used_percent: Option<f64>,
}

impl Situation {
    /// A freshly configured account: nothing tried, nothing known.
    #[cfg(test)]
    pub fn fresh() -> Self {
        Self {
            state: ProviderState::Pending,
            failures: 0,
            retry_after: None,
            seconds_to_next_reset: None,
            mode: RefreshMode::Auto,
            worst_used_percent: None,
        }
    }
}

/// How long to wait before the next poll of this account.
pub fn next_interval(situation: &Situation) -> Duration {
    match situation.state {
        ProviderState::WaitingForKeyring => KEYRING_RETRY,
        ProviderState::NoCredential
        | ProviderState::KeyringUnavailable
        | ProviderState::CredentialRejected => USER_ACTION_RETRY,
        ProviderState::RateLimited | ProviderState::Unreachable | ProviderState::Malformed => {
            backoff(situation.failures, situation.retry_after)
        }
        ProviderState::Ok | ProviderState::Pending => healthy(situation),
    }
}

fn healthy(situation: &Situation) -> Duration {
    match situation.mode {
        RefreshMode::Auto => {
            // Near a reset beats the zone deliberately: a window that rolls over while we
            // are asleep costs a segment boundary — the one event history cannot
            // reconstruct afterwards — and the reset notification is owed the moment the
            // boundary passes, not one zone-interval later.
            if situation
                .seconds_to_next_reset
                .is_some_and(|secs| secs <= NEAR_RESET_HORIZON_SECS)
            {
                return NEAR_RESET;
            }
            zone_interval(situation.worst_used_percent)
        }
        RefreshMode::Manual(interval) => interval,
    }
}

/// The Auto interval for an account whose worst window reads this much used.
///
/// The boundaries are the same constants the bar colours and the notifications use, so
/// all three agree about when a window is worth watching.
fn zone_interval(worst_used_percent: Option<f64>) -> Duration {
    let used = worst_used_percent.unwrap_or(0.0);
    if used >= 100.0 {
        AUTO_BLUE
    } else if used >= DANGER_AT {
        AUTO_RED
    } else if used >= WARNING_AT {
        AUTO_YELLOW
    } else {
        AUTO_BLUE
    }
}

/// Exponential from the baseline, doubling per consecutive failure.
///
/// The provider's own `Retry-After` is honoured when it is longer than our guess, and
/// never shortens it: a service under load that asks for one second while failing every
/// request would otherwise turn a backoff into a hot loop.
fn backoff(failures: u32, retry_after: Option<Duration>) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    let ours = BASELINE.saturating_mul(1u32 << doublings).min(MAX_BACKOFF);
    match retry_after {
        Some(theirs) if theirs > ours => theirs,
        _ => ours,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_with(worst: Option<f64>, reset: Option<i64>) -> Situation {
        Situation {
            state: ProviderState::Ok,
            worst_used_percent: worst,
            seconds_to_next_reset: reset,
            mode: RefreshMode::Auto,
            ..Situation::fresh()
        }
    }

    fn manual(minutes: u64, reset: Option<i64>) -> Situation {
        Situation {
            state: ProviderState::Ok,
            seconds_to_next_reset: reset,
            mode: RefreshMode::Manual(Duration::from_secs(minutes * 60)),
            ..Situation::fresh()
        }
    }

    fn failing(state: ProviderState, failures: u32, retry_after: Option<Duration>) -> Situation {
        Situation {
            state,
            failures,
            retry_after,
            ..Situation::fresh()
        }
    }

    #[test]
    fn the_zones_pace_by_how_much_of_the_quota_is_left() {
        assert_eq!(
            next_interval(&auto_with(Some(0.0), Some(4 * 3600))),
            AUTO_BLUE
        );
        assert_eq!(
            next_interval(&auto_with(Some(69.9), Some(4 * 3600))),
            AUTO_BLUE
        );
        assert_eq!(
            next_interval(&auto_with(Some(70.0), Some(4 * 3600))),
            AUTO_YELLOW
        );
        assert_eq!(
            next_interval(&auto_with(Some(89.9), Some(4 * 3600))),
            AUTO_YELLOW
        );
        assert_eq!(
            next_interval(&auto_with(Some(90.0), Some(4 * 3600))),
            AUTO_RED
        );
        assert_eq!(
            next_interval(&auto_with(Some(99.9), Some(4 * 3600))),
            AUTO_RED
        );
    }

    #[test]
    fn an_exhausted_account_waits_like_a_blue_one() {
        // Nothing can be spent until the reset, and the reset is watched for separately.
        assert_eq!(
            next_interval(&auto_with(Some(100.0), Some(4 * 3600))),
            AUTO_BLUE
        );
        assert_eq!(
            next_interval(&auto_with(Some(105.0), Some(4 * 3600))),
            AUTO_BLUE
        );
    }

    #[test]
    fn an_account_with_no_windows_yet_is_paced_as_blue() {
        assert_eq!(next_interval(&auto_with(None, None)), AUTO_BLUE);
        assert_eq!(next_interval(&Situation::fresh()), AUTO_BLUE);
    }

    #[test]
    fn the_quarter_hour_before_a_reset_beats_every_zone() {
        assert_eq!(
            next_interval(&auto_with(Some(95.0), Some(14 * 60))),
            NEAR_RESET
        );
        assert_eq!(
            next_interval(&auto_with(Some(50.0), Some(16 * 60))),
            AUTO_BLUE
        );
    }

    #[test]
    fn an_overdue_reset_is_still_near_one() {
        // The provider said it would roll over a minute ago and we have not seen it happen
        // yet. Backing off here is how a segment boundary gets lost.
        assert_eq!(next_interval(&auto_with(Some(95.0), Some(-60))), NEAR_RESET);
    }

    #[test]
    fn manual_polls_at_exactly_the_chosen_interval() {
        assert_eq!(
            next_interval(&manual(5, Some(14 * 60))),
            Duration::from_secs(5 * 60),
            "near a reset is Auto's acceleration; Manual is the pace the user picked"
        );
        assert_eq!(next_interval(&manual(1, None)), Duration::from_secs(60));
        assert_eq!(
            next_interval(&manual(120, None)),
            Duration::from_secs(120 * 60)
        );
    }

    #[test]
    fn the_mode_the_settings_describe_is_the_mode_the_daemon_runs() {
        let mut preferences = Preferences::default();
        assert_eq!(RefreshMode::configured(&preferences), RefreshMode::Auto);

        preferences.refresh_mode = Preferences::REFRESH_MANUAL.into();
        preferences.refresh_minutes = 30;
        assert_eq!(
            RefreshMode::configured(&preferences),
            RefreshMode::Manual(Duration::from_secs(1800))
        );
    }

    #[test]
    fn failures_back_off_exponentially_from_the_baseline() {
        let at = |n| next_interval(&failing(ProviderState::Unreachable, n, None));
        assert_eq!(at(1), BASELINE);
        assert_eq!(at(2), BASELINE * 2);
        assert_eq!(at(3), BASELINE * 4);
    }

    #[test]
    fn our_own_backoff_stops_at_an_hour() {
        for failures in 5..40 {
            assert_eq!(
                next_interval(&failing(ProviderState::RateLimited, failures, None)),
                MAX_BACKOFF,
                "{failures} failures"
            );
        }
    }

    #[test]
    fn a_provider_asking_for_longer_than_our_cap_is_obeyed() {
        assert_eq!(
            next_interval(&failing(
                ProviderState::RateLimited,
                1,
                Some(Duration::from_secs(7200))
            )),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn a_provider_asking_for_a_second_does_not_turn_backoff_into_a_hot_loop() {
        assert_eq!(
            next_interval(&failing(
                ProviderState::RateLimited,
                3,
                Some(Duration::from_secs(1))
            )),
            BASELINE * 4
        );
    }

    #[test]
    fn a_malformed_response_backs_off_rather_than_hammering_a_broken_endpoint() {
        // Nothing we do next poll will parse any better. Only a new release fixes this.
        assert_eq!(
            next_interval(&failing(ProviderState::Malformed, 4, None)),
            BASELINE * 8
        );
    }

    #[test]
    fn a_locked_keyring_is_asked_again_within_the_minute() {
        assert_eq!(
            next_interval(&failing(ProviderState::WaitingForKeyring, 9, None)),
            KEYRING_RETRY,
            "a locked keyring is not a failure to back off from; the user is about to log in"
        );
    }

    #[test]
    fn states_only_the_user_can_clear_are_not_retried_in_a_tight_loop() {
        for state in [
            ProviderState::NoCredential,
            ProviderState::CredentialRejected,
            ProviderState::KeyringUnavailable,
        ] {
            assert_eq!(next_interval(&failing(state, 1, None)), USER_ACTION_RETRY);
        }
    }
}
