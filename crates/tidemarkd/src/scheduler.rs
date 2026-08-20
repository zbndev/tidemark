//! When to poll next.
//!
//! Kept as one pure function over a described [`Situation`], with no clock and no I/O in
//! it, because every interesting case here is one nobody wants to reproduce live: a
//! provider asking for an hour, a window five minutes from rolling over, a keyring that is
//! still locked, a machine that has been idle since lunch.
//!
//! The intervals themselves come from `CONTEXT.md` § Polling and are not this module's to
//! reinvent: five minutes baseline, sixty seconds in the last quarter hour before a reset,
//! half an hour when nothing is being spent, exponential backoff capped at an hour.

use std::time::Duration;
use tidemark_types::ProviderState;

/// Interval when nothing special is happening.
pub const BASELINE: Duration = Duration::from_secs(5 * 60);

/// Interval while a window is about to roll over. The rollover is the one event history
/// cannot reconstruct afterwards — it is the boundary every segment is measured from — so
/// this is where the polling budget goes.
pub const NEAR_RESET: Duration = Duration::from_secs(60);

/// How close to a reset counts as near it.
pub const NEAR_RESET_HORIZON_SECS: i64 = 15 * 60;

/// Interval when no quota is being spent.
pub const IDLE: Duration = Duration::from_secs(30 * 60);

/// How long consumption must sit still before the account is treated as idle.
///
/// **What "no session activity" means here.** The daemon cannot see the user's terminal,
/// and `CONTEXT.md` deliberately does not say it can. The only activity signal it actually
/// has is the number the provider reports: quota that stops moving means nobody is
/// spending it. The cost of that definition is bounded and known — after a quiet spell,
/// the first poll of a new session can be up to [`IDLE`] late.
pub const IDLE_AFTER_SECS: i64 = 30 * 60;

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
    /// Seconds since consumption last moved. `None` before there is anything to compare.
    pub seconds_since_change: Option<i64>,
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
            seconds_since_change: None,
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
    // Near a reset beats idle deliberately: a window that rolls over while we are asleep
    // costs a segment boundary, and an idle account is exactly the one whose rollover
    // would otherwise be missed entirely.
    if situation
        .seconds_to_next_reset
        .is_some_and(|secs| secs <= NEAR_RESET_HORIZON_SECS)
    {
        return NEAR_RESET;
    }
    if situation
        .seconds_since_change
        .is_some_and(|secs| secs >= IDLE_AFTER_SECS)
    {
        return IDLE;
    }
    BASELINE
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

    fn healthy_with(reset: Option<i64>, since_change: Option<i64>) -> Situation {
        Situation {
            state: ProviderState::Ok,
            seconds_to_next_reset: reset,
            seconds_since_change: since_change,
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
    fn nothing_special_is_the_baseline() {
        assert_eq!(
            next_interval(&healthy_with(Some(4 * 3600), Some(60))),
            BASELINE
        );
        assert_eq!(next_interval(&Situation::fresh()), BASELINE);
    }

    #[test]
    fn the_quarter_hour_before_a_reset_is_watched_closely() {
        assert_eq!(
            next_interval(&healthy_with(Some(14 * 60), None)),
            NEAR_RESET
        );
        assert_eq!(next_interval(&healthy_with(Some(16 * 60), None)), BASELINE);
    }

    #[test]
    fn an_overdue_reset_is_still_near_one() {
        // The provider said it would roll over a minute ago and we have not seen it happen
        // yet. Backing off to five minutes here is how a segment boundary gets lost.
        assert_eq!(
            next_interval(&healthy_with(Some(-60), Some(10 * 3600))),
            NEAR_RESET
        );
    }

    #[test]
    fn quota_that_stops_moving_slows_the_polling_down() {
        assert_eq!(
            next_interval(&healthy_with(None, Some(IDLE_AFTER_SECS))),
            IDLE
        );
        assert_eq!(
            next_interval(&healthy_with(None, Some(IDLE_AFTER_SECS - 1))),
            BASELINE
        );
    }

    #[test]
    fn an_idle_account_still_wakes_up_for_its_rollover() {
        assert_eq!(
            next_interval(&healthy_with(Some(5 * 60), Some(12 * 3600))),
            NEAR_RESET
        );
    }

    #[test]
    fn a_fresh_account_is_not_idle_merely_because_nothing_is_known_about_it() {
        assert_eq!(next_interval(&healthy_with(None, None)), BASELINE);
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
