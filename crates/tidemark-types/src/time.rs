//! Instants, expressed the way the wire and the database want them.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time, as whole seconds since the Unix epoch.
///
/// The type exists to make one of the storage rules structural rather than remembered:
/// a `Timestamp` cannot hold an absurd value, because [`Timestamp::from_unix`] refuses to
/// build one. Providers have been observed reporting `1970-01-01`, and a single such point
/// stretches every chart by decades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

/// Why a timestamp was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsurdTimestamp {
    /// The value that was offered.
    pub seconds: i64,
}

impl std::fmt::Display for AbsurdTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not a plausible instant (expected {}..{})",
            self.seconds,
            Timestamp::EARLIEST,
            Timestamp::LATEST
        )
    }
}

impl std::error::Error for AbsurdTimestamp {}

impl Timestamp {
    /// 2020-01-01. Nothing this project observes predates it.
    pub const EARLIEST: i64 = 1_577_836_800;
    /// 2100-01-01. Anything beyond is a unit mix-up, not a date.
    pub const LATEST: i64 = 4_102_444_800;

    /// Builds a timestamp from Unix seconds, refusing values outside the plausible range.
    pub fn from_unix(seconds: i64) -> Result<Self, AbsurdTimestamp> {
        if (Self::EARLIEST..Self::LATEST).contains(&seconds) {
            Ok(Self(seconds))
        } else {
            Err(AbsurdTimestamp { seconds })
        }
    }

    /// Builds a timestamp from Unix milliseconds. Z.ai reports `nextResetTime` this way.
    pub fn from_unix_millis(millis: i64) -> Result<Self, AbsurdTimestamp> {
        Self::from_unix(millis.div_euclid(1000))
    }

    /// The current instant.
    ///
    /// Falls back to [`Timestamp::EARLIEST`] if the system clock is set before 1970, which
    /// is not a state worth propagating a `Result` through every call site for.
    pub fn now() -> Self {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(Self::EARLIEST);
        Self(seconds.clamp(Self::EARLIEST, Self::LATEST - 1))
    }

    /// Unix seconds.
    pub fn as_unix(self) -> i64 {
        self.0
    }

    /// Seconds from `self` to `later`; negative if `later` is in the past.
    pub fn seconds_until(self, later: Self) -> i64 {
        later.0 - self.0
    }

    /// `self` moved forward by `seconds`, clamped into the plausible range.
    pub fn saturating_add_seconds(self, seconds: i64) -> Self {
        Self(
            self.0
                .saturating_add(seconds)
                .clamp(Self::EARLIEST, Self::LATEST - 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_epoch_that_a_provider_actually_sent() {
        // Observed in the reference implementation's history: an Apple-epoch zero that
        // round-trips to 1970-01-01T00:00:01Z.
        assert!(Timestamp::from_unix(1).is_err());
        assert!(Timestamp::from_unix(0).is_err());
        assert!(Timestamp::from_unix(-978_307_199).is_err());
    }

    #[test]
    fn accepts_a_plausible_instant() {
        let t = Timestamp::from_unix(1_785_700_585).expect("plausible");
        assert_eq!(t.as_unix(), 1_785_700_585);
    }

    #[test]
    fn milliseconds_are_divided_not_truncated_towards_zero() {
        let t = Timestamp::from_unix_millis(1_785_700_585_999).expect("plausible");
        assert_eq!(t.as_unix(), 1_785_700_585);
    }

    #[test]
    fn seconds_out_of_range_are_named_in_the_error() {
        let err = Timestamp::from_unix(1).expect_err("absurd");
        assert_eq!(err.seconds, 1);
        assert!(err.to_string().contains('1'));
    }
}
