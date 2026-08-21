//! Pure coordinate calculations for the detail dialog's burn-down chart.
//!
//! The chart has no knowledge of GTK: it maps a current window and stored observations into
//! clamped points. Keeping the schedule maths here makes a missing reset time an explicit
//! no-diagonal state rather than a rendering accident.

use tidemark_types::{HistoryPoint, WindowStatus};

/// One coordinate in a drawing area, measured from its top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    pub x: f64,
    pub y: f64,
}

/// Everything a renderer needs to show one current-segment series.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    /// Actual stored readings, oldest first.
    pub actual: Vec<Coord>,
    /// The straight line from an unused window start to its reset, when both are known.
    pub diagonal: Option<[Coord; 2]>,
    /// A single observation deserves a visible dot rather than a line with an invented end.
    pub marker: Option<Coord>,
}

/// Maps actual consumption and, when possible, even pace into the plot rectangle.
pub fn geometry(
    window: &WindowStatus,
    points: &[HistoryPoint],
    width: f64,
    height: f64,
) -> Geometry {
    let width = width.max(0.0);
    let height = height.max(0.0);
    let mut actual_points = points.to_vec();
    actual_points.sort_by_key(|point| point.captured_at);

    let schedule = window
        .resets_at
        .zip(window.length_secs)
        .map(|(reset, length)| {
            (
                reset.saturating_sub(i64::try_from(length).unwrap_or(i64::MAX)),
                reset,
            )
        });
    let domain = schedule.or_else(|| {
        actual_points
            .first()
            .zip(actual_points.last())
            .map(|(first, last)| (first.captured_at, last.captured_at))
    });

    let actual: Vec<Coord> = actual_points
        .iter()
        .map(|point| Coord {
            x: domain.map_or(width / 2.0, |(start, end)| {
                project(point.captured_at, start, end, width)
            }),
            y: consumption_y(point.used_percent, height),
        })
        .collect();
    let diagonal = schedule.map(|_| [Coord { x: 0.0, y: height }, Coord { x: width, y: 0.0 }]);
    let marker = (actual.len() == 1).then(|| actual[0]);

    Geometry {
        actual,
        diagonal,
        marker,
    }
}

fn project(value: i64, start: i64, end: i64, width: f64) -> f64 {
    if start >= end {
        return width / 2.0;
    }
    let elapsed = value.saturating_sub(start) as f64;
    let duration = end.saturating_sub(start) as f64;
    (elapsed / duration).clamp(0.0, 1.0) * width
}

fn consumption_y(used_percent: f64, height: f64) -> f64 {
    height - (used_percent / 100.0).clamp(0.0, 1.0) * height
}

#[cfg(test)]
mod tests {
    use tidemark_types::{HistoryPoint, WindowStatus};

    use super::{Coord, geometry};

    fn scheduled_window() -> WindowStatus {
        WindowStatus {
            key: "w18000".into(),
            title: "5 hours".into(),
            used_percent: 50.0,
            resets_at: Some(18_000),
            length_secs: Some(18_000),
        }
    }

    #[test]
    fn a_scheduled_segment_is_compared_to_its_even_pace() {
        let points = [
            HistoryPoint {
                captured_at: 0,
                used_percent: 0.0,
            },
            HistoryPoint {
                captured_at: 9_000,
                used_percent: 50.0,
            },
        ];

        let geometry = geometry(&scheduled_window(), &points, 100.0, 100.0);
        assert_eq!(
            geometry.diagonal,
            Some([Coord { x: 0.0, y: 100.0 }, Coord { x: 100.0, y: 0.0 }])
        );
        assert_eq!(
            geometry.actual,
            vec![Coord { x: 0.0, y: 100.0 }, Coord { x: 50.0, y: 50.0 }]
        );
    }

    #[test]
    fn an_unreported_schedule_does_not_fabricate_an_even_pace() {
        let mut window = scheduled_window();
        window.resets_at = None;
        let points = [
            HistoryPoint {
                captured_at: 10,
                used_percent: 30.0,
            },
            HistoryPoint {
                captured_at: 20,
                used_percent: 70.0,
            },
        ];

        let geometry = geometry(&window, &points, 100.0, 100.0);
        assert_eq!(geometry.diagonal, None);
        assert_eq!(
            geometry.actual,
            vec![Coord { x: 0.0, y: 70.0 }, Coord { x: 100.0, y: 30.0 }]
        );
    }

    #[test]
    fn a_single_measurement_is_a_marker_not_an_invented_line() {
        let points = [HistoryPoint {
            captured_at: 9_000,
            used_percent: 25.0,
        }];

        let geometry = geometry(&scheduled_window(), &points, 100.0, 100.0);
        assert_eq!(geometry.actual, vec![Coord { x: 50.0, y: 75.0 }]);
        assert_eq!(geometry.marker, Some(Coord { x: 50.0, y: 75.0 }));
    }

    #[test]
    fn measurements_outside_the_known_window_are_clamped_into_the_plot() {
        let points = [
            HistoryPoint {
                captured_at: -5,
                used_percent: -2.0,
            },
            HistoryPoint {
                captured_at: 20_000,
                used_percent: 110.0,
            },
        ];

        let geometry = geometry(&scheduled_window(), &points, 100.0, 100.0);
        assert_eq!(
            geometry.actual,
            vec![Coord { x: 0.0, y: 100.0 }, Coord { x: 100.0, y: 0.0 }]
        );
    }

    #[test]
    fn actual_measurements_are_chronological_even_if_the_reply_is_not() {
        let points = [
            HistoryPoint {
                captured_at: 9_000,
                used_percent: 50.0,
            },
            HistoryPoint {
                captured_at: 0,
                used_percent: 0.0,
            },
        ];

        let geometry = geometry(&scheduled_window(), &points, 100.0, 100.0);
        assert_eq!(
            geometry.actual,
            vec![Coord { x: 0.0, y: 100.0 }, Coord { x: 50.0, y: 50.0 }]
        );
    }
}
