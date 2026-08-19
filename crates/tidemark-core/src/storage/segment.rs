//! Deciding where one segment ends and the next begins.
//!
//! A segment is one instance of a window between two resets. It is the unit history is
//! grouped by, notifications deduplicate against, and the burn-down forecast is computed
//! over — so getting this wrong is not a display bug, it is the loss of every derived
//! feature at once.
//!
//! **The rule, and why it is not the obvious one.** The obvious rule is "same `resets_at`,
//! same segment". Measured against nine months of real history from the reference
//! implementation, it produces *one segment per sample*: 1363 segments over 1363 points
//! for one window, 825 over 831 for another. Two separate causes, both real:
//!
//! * Some providers compute `resets_at` as "now plus what is left", so it drifts forward
//!   by the poll interval on every single poll and no two values are ever equal.
//! * Others report it stably but recompute it with second granularity, so it jitters by
//!   tens of seconds around a fixed instant.
//!
//! Widening the comparison to an absolute tolerance fixes the second cause and not the
//! first: a value that advances by five minutes per poll leaves any fixed tolerance
//! immediately. What separates drift from a rollover is not how far `resets_at` moved, but
//! whether the move is **explained by the time that passed**. A rolling window's reset time
//! advances in step with the clock. A window that actually rolled over jumps ahead by its
//! whole length while only a poll interval elapsed.
//!
//! So: a forward jump larger than the elapsed time, beyond a tolerance that absorbs the
//! jitter, means a new segment. A drop in consumption means a new segment on its own, since
//! quota does not un-spend inside one window. Either signal is sufficient; neither is
//! required, because a provider may omit `resets_at`, and a window can roll over from zero
//! to zero without consumption moving.

use tidemark_types::Timestamp;

/// How far `resets_at` may jitter around a fixed instant before the movement is treated as
/// real. Five minutes, per `CONTEXT.md` § Storage.
pub const RESET_JITTER_TOLERANCE_SECS: i64 = 300;

/// How far consumption must fall to count as a drop. Guards against float noise in
/// percentages that arrive as ratios multiplied out.
pub const USED_DROP_EPSILON: f64 = 0.5;

/// One reading of one window, reduced to the three fields segmentation depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    /// When the reading was taken.
    pub captured_at: Timestamp,
    /// Consumption at that moment, 0..=100.
    pub used_percent: f64,
    /// When the provider said the window would roll over, if it said.
    pub resets_at: Option<Timestamp>,
}

/// Why a boundary was drawn. Kept because it is the first thing worth looking at when a
/// chart shows a segment count nobody expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// The two readings belong to the same segment.
    Continues,
    /// Consumption fell, which cannot happen within one window.
    UsageDropped,
    /// `resets_at` moved further forward than the elapsed time explains.
    ResetTimeJumped,
}

impl Boundary {
    /// Whether this boundary starts a new segment.
    pub fn starts_new_segment(self) -> bool {
        !matches!(self, Boundary::Continues)
    }
}

/// Classifies the transition between two consecutive readings of the same window.
///
/// `previous` must be the reading immediately before `next`. Out-of-order input is treated
/// as zero elapsed time rather than negative, which makes the reset test stricter rather
/// than looser — a clock that jumps backwards should not manufacture segments.
pub fn classify(previous: &Observation, next: &Observation) -> Boundary {
    if next.used_percent < previous.used_percent - USED_DROP_EPSILON {
        return Boundary::UsageDropped;
    }

    if let (Some(before), Some(after)) = (previous.resets_at, next.resets_at) {
        let elapsed = previous.captured_at.seconds_until(next.captured_at).max(0);
        let moved = before.seconds_until(after);
        if moved > elapsed + RESET_JITTER_TOLERANCE_SECS {
            return Boundary::ResetTimeJumped;
        }
    }

    Boundary::Continues
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: i64 = 305;
    const HOUR: i64 = 3600;

    fn at(offset: i64) -> Timestamp {
        Timestamp::from_unix(1_785_700_000 + offset).expect("plausible")
    }

    fn obs(captured: i64, used: f64, resets: Option<i64>) -> Observation {
        Observation {
            captured_at: at(captured),
            used_percent: used,
            resets_at: resets.map(at),
        }
    }

    #[test]
    fn a_stable_window_continues() {
        let a = obs(0, 40.0, Some(5 * HOUR));
        let b = obs(POLL, 41.0, Some(5 * HOUR));
        assert_eq!(classify(&a, &b), Boundary::Continues);
    }

    #[test]
    fn jitter_within_the_tolerance_continues() {
        // claude.json reports the same window as 807785940 and 807786000, a minute apart.
        let a = obs(0, 41.0, Some(5 * HOUR));
        let b = obs(POLL, 41.0, Some(5 * HOUR + 60));
        assert_eq!(classify(&a, &b), Boundary::Continues);
    }

    #[test]
    fn a_reset_time_that_advances_in_step_with_the_clock_continues() {
        // The rolling-window case: 825 spurious segments in the reference implementation.
        let mut previous = obs(0, 12.0, Some(7 * 24 * HOUR));
        for step in 1..200 {
            let next = obs(step * POLL, 12.0, Some(7 * 24 * HOUR + step * POLL));
            assert_eq!(
                classify(&previous, &next),
                Boundary::Continues,
                "drift at step {step} was mistaken for a reset"
            );
            previous = next;
        }
    }

    #[test]
    fn a_rollover_is_a_jump_the_elapsed_time_does_not_explain() {
        let a = obs(0, 96.0, Some(120));
        let b = obs(POLL, 96.0, Some(120 + 5 * HOUR));
        assert_eq!(classify(&a, &b), Boundary::ResetTimeJumped);
    }

    #[test]
    fn a_rolling_window_that_freezes_and_catches_up_still_continues() {
        // Observed in the corpus: a drifting window stops updating for a poll or two and
        // then makes up the difference in one step.
        let a = obs(0, 30.0, Some(2 * HOUR));
        let b = obs(3 * POLL, 30.0, Some(2 * HOUR + 3 * POLL));
        assert_eq!(classify(&a, &b), Boundary::Continues);
    }

    #[test]
    fn consumption_falling_is_a_reset_on_its_own() {
        let a = obs(0, 89.0, None);
        let b = obs(POLL, 3.0, None);
        assert_eq!(classify(&a, &b), Boundary::UsageDropped);
    }

    #[test]
    fn float_noise_is_not_a_reset() {
        let a = obs(0, 8.328_48, None);
        let b = obs(POLL, 8.328_47, None);
        assert_eq!(classify(&a, &b), Boundary::Continues);
    }

    #[test]
    fn a_window_that_rolls_over_from_zero_to_zero_is_still_caught() {
        // No drop to see, because nothing was spent. Only the reset time gives it away.
        let a = obs(0, 0.0, Some(60));
        let b = obs(POLL, 0.0, Some(60 + 7 * 24 * HOUR));
        assert_eq!(classify(&a, &b), Boundary::ResetTimeJumped);
    }

    #[test]
    fn a_missing_reset_time_leaves_only_the_consumption_signal() {
        let a = obs(0, 10.0, Some(5 * HOUR));
        let b = obs(POLL, 11.0, None);
        assert_eq!(classify(&a, &b), Boundary::Continues);
    }

    #[test]
    fn a_backwards_clock_does_not_manufacture_a_segment() {
        let a = obs(POLL, 10.0, Some(5 * HOUR));
        let b = obs(0, 10.0, Some(5 * HOUR));
        assert_eq!(classify(&a, &b), Boundary::Continues);
    }
}
