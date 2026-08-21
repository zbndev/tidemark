//! Pure coordinate calculations for the detail dialog's burn-down chart.
//!
//! The chart has no knowledge of GTK: it maps a current window and stored observations into
//! clamped points. Keeping the schedule maths here makes a missing reset time an explicit
//! no-diagonal state rather than a rendering accident.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::cairo;
use gtk::prelude::*;
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

#[derive(Debug, Clone)]
struct Data {
    window: WindowStatus,
    points: Vec<HistoryPoint>,
}

/// A GTK drawing surface for current-segment consumption.
///
/// Its geometry lives above, where unit tests can exercise it without a display. This small
/// widget only selects the visible state and turns that geometry into Cairo paths.
#[derive(Debug, Clone)]
pub struct Chart {
    stack: gtk::Stack,
    area: gtk::DrawingArea,
    message: gtk::Label,
    data: Rc<RefCell<Option<Data>>>,
}

impl Chart {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(220)
            .hexpand(true)
            .css_classes(["quota-chart"])
            .build();
        let message = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .margin_top(36)
            .margin_bottom(36)
            .margin_start(24)
            .margin_end(24)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&area, Some("chart"));
        stack.add_named(&message, Some("message"));
        let data = Rc::new(RefCell::new(None));
        area.set_draw_func({
            let data = Rc::clone(&data);
            move |area, context, width, height| {
                if let Some(data) = data.borrow().as_ref() {
                    draw(area, context, f64::from(width), f64::from(height), data);
                }
            }
        });

        let chart = Self {
            stack,
            area,
            message,
            data,
        };
        chart.set_loading();
        chart
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    pub fn set_loading(&self) {
        self.data.borrow_mut().take();
        self.message.set_label("Loading current segment…");
        self.stack.set_visible_child_name("message");
    }

    pub fn set_empty(&self, message: &str) {
        self.data.borrow_mut().take();
        self.message.set_label(message);
        self.stack.set_visible_child_name("message");
    }

    pub fn set_error(&self, message: &str) {
        self.data.borrow_mut().take();
        self.message.set_label(message);
        self.stack.set_visible_child_name("message");
    }

    pub fn set_data(&self, window: WindowStatus, points: Vec<HistoryPoint>) {
        if points.is_empty() {
            self.set_empty("No stored readings in this segment yet.");
            return;
        }
        *self.data.borrow_mut() = Some(Data { window, points });
        self.stack.set_visible_child_name("chart");
        self.area.queue_draw();
    }
}

fn draw(area: &gtk::DrawingArea, context: &cairo::Context, width: f64, height: f64, data: &Data) {
    let geometry = geometry(&data.window, &data.points, width, height);
    let ink = area.color();

    if let Some([start, end]) = geometry.diagonal {
        context.set_dash(&[5.0, 5.0], 0.0);
        context.set_line_width(1.5);
        context.move_to(start.x, start.y);
        context.line_to(end.x, end.y);
        set_source(context, ink, 0.38);
        stroke(context);
        context.set_dash(&[], 0.0);
    }

    if let Some((first, rest)) = geometry.actual.split_first() {
        context.set_line_width(2.5);
        context.move_to(first.x, first.y);
        for point in rest {
            context.line_to(point.x, point.y);
        }
        set_source(context, ink, 1.0);
        stroke(context);
    }

    if let Some(point) = geometry.marker {
        context.arc(point.x, point.y, 4.0, 0.0, std::f64::consts::TAU);
        set_source(context, ink, 1.0);
        fill(context);
    }
}

fn set_source(context: &cairo::Context, colour: gtk::gdk::RGBA, alpha: f64) {
    context.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        f64::from(colour.alpha()) * alpha,
    );
}

fn stroke(context: &cairo::Context) {
    if let Err(error) = context.stroke() {
        tracing::debug!(%error, "dropping a burn-down chart frame");
    }
}

fn fill(context: &cairo::Context) {
    if let Err(error) = context.fill() {
        tracing::debug!(%error, "dropping a burn-down chart frame");
    }
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
            subtitle: None,
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
