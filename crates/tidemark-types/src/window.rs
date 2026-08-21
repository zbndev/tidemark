//! Rate-limit windows: the unit everything else in Tidemark is expressed in.

use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Consumption at which a window stops being unremarkable.
///
/// One pair of numbers, in the shared crate, because two processes act on them: the daemon
/// fires a notification here, and the interface changes the colour of the bar here. The
/// card and the notification that interrupts you must never disagree about when a window
/// became worth worrying about, and a comment in each crate saying "keep these in step" is
/// not a mechanism. See `CONTEXT.md` § Notifications.
pub const WARNING_AT: f64 = 80.0;

/// Consumption at which a window is nearly gone. See [`WARNING_AT`].
pub const DANGER_AT: f64 = 95.0;

/// How long a window lasts, in whole seconds. Never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowLength(u64);

impl WindowLength {
    /// Builds a length, refusing zero. A zero-length window divides by zero in every
    /// pace calculation downstream.
    pub fn from_secs(seconds: u64) -> Option<Self> {
        (seconds > 0).then_some(Self(seconds))
    }

    /// Length in seconds.
    pub fn as_secs(self) -> u64 {
        self.0
    }

    /// Length as a [`Duration`].
    pub fn as_duration(self) -> Duration {
        Duration::from_secs(self.0)
    }
}

/// The stable identity of a window across responses, restarts and provider redesigns.
///
/// **The rule this type exists to enforce.** A key is derived from what the window *is* —
/// its length, and the pool it draws from — never from where it appeared in the response.
/// Codex is the proof: the same weekly window has been observed arriving as
/// `rate_limit.primary_window` on one day and `secondary_window` on another. Keying on the
/// slot name splits one continuous window in two and fabricates appeared/disappeared
/// events; keying on the length does not.
///
/// Build keys with [`WindowKey::for_length`] or [`WindowKey::for_pool`]. [`WindowKey::named`]
/// exists for the handful of windows a provider does not describe usefully, and every use
/// of it should carry a comment saying why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowKey(String);

impl WindowKey {
    /// The key for a window of this length, where the provider has only one such pool.
    pub fn for_length(length: WindowLength) -> Self {
        Self(format!("w{}", length.as_secs()))
    }

    /// The key for a window of this length drawing on a named pool.
    ///
    /// Needed wherever one provider reports several windows of the *same* length against
    /// different quotas — Antigravity reports a seven-day window for Gemini models and
    /// another seven-day window for third-party models.
    pub fn for_pool(pool: &str, length: WindowLength) -> Self {
        Self(format!("{pool}/w{}", length.as_secs()))
    }

    /// A key for a window whose length the provider does not describe, or describes
    /// wrongly. Use sparingly and say why at the call site.
    pub fn named(name: &str) -> Self {
        Self(name.to_owned())
    }

    /// The key as stored and sent over D-Bus.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WindowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One rate-limit period as a provider currently reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Stable identity. See [`WindowKey`].
    pub key: WindowKey,
    /// What to call it in the interface, in the provider's own terms.
    pub title: String,
    /// The absolute quantities behind [`Window::used_percent`], already formatted by the
    /// adapter — `100 / 1000 credits`, `4.2M / 10M tokens`.
    ///
    /// Presentation the provider owns rather than a pair of numbers, because the unit is
    /// the provider's and so is the rounding: a credit balance in dollars and a token
    /// allowance in millions are not the same kind of quantity, and a shared formatter
    /// would have to pick a house style for both. The interface draws it small under the
    /// bar and never parses it.
    ///
    /// `None` where the provider reported only a percentage, which is the common case.
    pub subtitle: Option<String>,
    /// Consumption, 0..=100.
    pub used_percent: f64,
    /// When the window rolls over, if the provider says.
    pub resets_at: Option<Timestamp>,
    /// How long the window lasts, if the provider says or it can be derived.
    pub length: Option<WindowLength>,
}

impl Window {
    /// Seconds remaining before the reset, or `None` if the provider did not say when.
    /// Negative values are clamped to zero: a reset that is overdue has not happened yet.
    pub fn seconds_until_reset(&self, now: Timestamp) -> Option<i64> {
        self.resets_at.map(|r| now.seconds_until(r).max(0))
    }

    /// The fraction of the window that has elapsed, 0.0..=1.0.
    ///
    /// This is the pace mark: the position on the bar that consumption is compared
    /// against. Fill to the left of it is sustainable; fill to the right of it means the
    /// quota runs out before the reset. Requires both a length and a reset time.
    pub fn pace(&self, now: Timestamp) -> Option<f64> {
        let length = self.length?.as_secs() as f64;
        let remaining = self.seconds_until_reset(now)? as f64;
        Some(((length - remaining) / length).clamp(0.0, 1.0))
    }

    /// True when consumption is running ahead of the clock, so the window will be
    /// exhausted before it resets. `None` whenever the pace mark cannot be computed.
    pub fn is_outpacing(&self, now: Timestamp) -> Option<bool> {
        self.pace(now).map(|pace| self.used_percent / 100.0 > pace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(length: u64, resets_in: i64, used: f64) -> (Window, Timestamp) {
        let now = Timestamp::from_unix(1_785_700_000).expect("plausible");
        let w = Window {
            key: WindowKey::for_length(WindowLength::from_secs(length).expect("nonzero")),
            title: "test".into(),
            subtitle: None,
            used_percent: used,
            resets_at: Some(now.saturating_add_seconds(resets_in)),
            length: WindowLength::from_secs(length),
        };
        (w, now)
    }

    #[test]
    fn a_zero_length_window_cannot_be_built() {
        assert!(WindowLength::from_secs(0).is_none());
    }

    #[test]
    fn pace_is_the_elapsed_fraction() {
        let (w, now) = window(3600, 900, 0.0);
        assert!((w.pace(now).expect("computable") - 0.75).abs() < 1e-9);
    }

    #[test]
    fn an_overdue_reset_reads_as_a_full_window_not_a_negative_one() {
        let (w, now) = window(3600, -600, 0.0);
        assert_eq!(w.seconds_until_reset(now), Some(0));
        assert_eq!(w.pace(now), Some(1.0));
    }

    #[test]
    fn outpacing_compares_consumption_against_the_clock() {
        let (ahead, now) = window(3600, 1800, 80.0);
        assert_eq!(ahead.is_outpacing(now), Some(true));
        let (behind, now) = window(3600, 1800, 20.0);
        assert_eq!(behind.is_outpacing(now), Some(false));
    }

    #[test]
    fn without_a_length_there_is_no_pace_mark() {
        let (mut w, now) = window(3600, 1800, 50.0);
        w.length = None;
        assert_eq!(w.pace(now), None);
        assert_eq!(w.is_outpacing(now), None);
    }

    #[test]
    fn keys_distinguish_pools_of_the_same_length() {
        let week = WindowLength::from_secs(604_800).expect("nonzero");
        assert_ne!(
            WindowKey::for_pool("gemini", week),
            WindowKey::for_pool("third-party", week)
        );
        assert_eq!(WindowKey::for_length(week).as_str(), "w604800");
    }
}
