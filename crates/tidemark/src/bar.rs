//! The quota bar, and the pace mark on it.
//!
//! `GtkLevelBar` can draw the fill and nothing else, so this is a `GtkDrawingArea`. What it
//! draws is a track, a fill, and — when there is one — a single mark at the fraction of the
//! window that has already elapsed. Fill to the left of the mark is spending that finishes
//! before the window rolls over; fill to the right of it is spending that does not.
//!
//! **The markless bar is the normal case, not the fallback.** Z.ai omits `nextResetTime`
//! from its five-hour window whenever the window has just reset and nothing has been spent
//! in it, and that five-hour window is the one the card leads with. So the geometry is
//! written as "a bar, optionally with a mark": there is no length to invent and no reset
//! time to guess when the provider says nothing, and inventing either would put a confident
//! wrong claim on the screen.
//!
//! Colour comes from the theme rather than from a palette of ours, and it comes through
//! CSS rather than through libadwaita's API: the fill is whatever `@accent_bg_color`,
//! `@warning_bg_color` and `@error_bg_color` currently resolve to, which is what a user's
//! own stylesheet overrides when they change their accent — asking `AdwStyleManager` gives
//! libadwaita's idea of the accent instead, and on a machine with a themed accent the bar
//! would be the one blue thing in a grey window. The track and the mark take the inherited
//! text colour from the widget above. Everything is read at draw time, so a theme change is
//! picked up by the next frame without anything having to listen for it.

use std::cell::Cell;
use std::rc::Rc;

use gtk::cairo;
use gtk::gdk::RGBA;
use gtk::prelude::*;

/// Consumption at which the bar stops being plain accent-coloured.
///
/// The same numbers as the notification thresholds in `CONTEXT.md` § Notifications, and
/// deliberately so: the colour on the card and the notification that interrupts you should
/// never disagree about when a window became worth worrying about.
pub const WARNING_AT: f64 = 80.0;
/// Consumption at which the bar turns to the error colour. See [`WARNING_AT`].
pub const DANGER_AT: f64 = 95.0;

/// Style class every bar carries; `style.rs` gives it its fill colour.
const CLASS_BAR: &str = "quota-bar";
/// Added at [`WARNING_AT`], and again with [`CLASS_DANGER`] at [`DANGER_AT`].
const CLASS_WARNING: &str = "quota-warning";
/// Added at [`DANGER_AT`].
const CLASS_DANGER: &str = "quota-danger";

/// How loud the fill is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Below [`WARNING_AT`].
    Normal,
    /// At or above [`WARNING_AT`].
    Warning,
    /// At or above [`DANGER_AT`].
    Danger,
}

/// The tone for a reading.
pub fn tone(used_percent: f64) -> Tone {
    if used_percent >= DANGER_AT {
        Tone::Danger
    } else if used_percent >= WARNING_AT {
        Tone::Warning
    } else {
        Tone::Normal
    }
}

/// Where the ink goes, in widget coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// Width of the fill, or `None` when nothing has been spent — a zero-width rounded
    /// rectangle is not a smaller bar, it is a rendering artefact.
    pub fill_width: Option<f64>,
    /// Where the pace mark sits, or `None` when the provider gave nothing to compute it
    /// from. See the module docs: this is the common case, not the exception.
    pub mark_x: Option<f64>,
}

/// Lays out one bar.
///
/// A fill is never narrower than the bar is tall, so that 0.3% of a window is a visible dot
/// rather than a sliver too thin for the rounded ends to render. The mark is kept a pixel
/// inside each end for the same reason: a mark at exactly `0` or exactly `width` is drawn
/// half outside the widget and reads as no mark at all.
pub fn geometry(width: f64, height: f64, used_percent: f64, pace: Option<f64>) -> Geometry {
    let width = width.max(0.0);
    let fraction = (used_percent / 100.0).clamp(0.0, 1.0);
    let fill_width = (fraction > 0.0).then(|| (fraction * width).max(height.min(width)).min(width));
    let mark_x = pace.map(|pace| (pace.clamp(0.0, 1.0) * width).clamp(1.0, (width - 1.0).max(1.0)));
    Geometry { fill_width, mark_x }
}

/// What the widget is currently showing.
#[derive(Debug, Clone, Copy, Default)]
struct Reading {
    used_percent: f64,
    pace: Option<f64>,
}

/// A quota bar. Cheap to make: the card builds one for the dominant window and one per
/// remaining window, and updates them in place for the life of the card.
#[derive(Debug, Clone)]
pub struct QuotaBar {
    area: gtk::DrawingArea,
    reading: Rc<Cell<Reading>>,
}

impl QuotaBar {
    /// A bar `height` pixels tall, filling whatever width it is given.
    pub fn new(height: i32) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(height)
            .hexpand(true)
            .css_classes([CLASS_BAR])
            .build();
        let reading = Rc::new(Cell::new(Reading::default()));

        area.set_draw_func({
            let reading = Rc::clone(&reading);
            move |area, context, width, height| {
                draw(
                    area,
                    context,
                    f64::from(width),
                    f64::from(height),
                    reading.get(),
                );
            }
        });

        Self { area, reading }
    }

    /// The widget to pack.
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Shows a reading. `pace` is [`tidemark_types::Window::pace`] — `None` whenever the
    /// provider did not give a reset time or a length, which draws a bar with no mark.
    pub fn set(&self, used_percent: f64, pace: Option<f64>) {
        self.reading.set(Reading { used_percent, pace });

        // The tone is a CSS class rather than a colour chosen here, so that the three
        // colours a bar can be are the three the rest of the desktop already uses.
        self.area.remove_css_class(CLASS_WARNING);
        self.area.remove_css_class(CLASS_DANGER);
        match tone(used_percent) {
            Tone::Normal => {}
            Tone::Warning => self.area.add_css_class(CLASS_WARNING),
            Tone::Danger => self.area.add_css_class(CLASS_DANGER),
        }

        self.area.queue_draw();
    }
}

fn draw(
    area: &gtk::DrawingArea,
    context: &cairo::Context,
    width: f64,
    height: f64,
    reading: Reading,
) {
    let geometry = geometry(width, height, reading.used_percent, reading.pace);
    // The fill colour arrives as the widget's own `color`, put there by the stylesheet; the
    // neutral one is inherited from the card, which is where the ordinary text colour is.
    let ink = area.color();
    let neutral = area.parent().map_or(ink, |parent| parent.color());
    let radius = height / 2.0;

    rounded_rect(context, 0.0, width, height, radius);
    set_source(context, neutral, 0.15);
    fill(context);

    if let Some(fill_width) = geometry.fill_width {
        rounded_rect(context, 0.0, fill_width, height, radius);
        set_source(context, ink, 1.0);
        fill(context);
    }

    if let Some(mark_x) = geometry.mark_x {
        // Drawn in the text colour rather than one of its own: it has to stay legible both
        // against the fill and against the empty track, and the text colour is the one
        // colour a theme guarantees contrasts with everything else it chose.
        context.rectangle(mark_x - 1.0, 0.0, 2.0, height);
        set_source(context, neutral, 0.85);
        fill(context);
    }
}

fn rounded_rect(context: &cairo::Context, x: f64, width: f64, height: f64, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let (left, right) = (x + radius, x + width - radius);
    let (top, bottom) = (radius, height - radius);
    context.new_sub_path();
    context.arc(right, top, radius, -FRAC_PI_2, 0.0);
    context.arc(right, bottom, radius, 0.0, FRAC_PI_2);
    context.arc(left, bottom, radius, FRAC_PI_2, PI);
    context.arc(left, top, radius, PI, 3.0 * FRAC_PI_2);
    context.close_path();
}

fn set_source(context: &cairo::Context, colour: RGBA, alpha: f64) {
    context.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        f64::from(colour.alpha()) * alpha,
    );
}

/// Filling a path can only fail if the context is already in an error state, which for a
/// context GTK just handed us means the frame is lost either way.
fn fill(context: &cairo::Context) {
    if let Err(error) = context.fill() {
        tracing::debug!(%error, "dropping a bar frame");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: f64 = 200.0;
    const HEIGHT: f64 = 12.0;

    fn at(used: f64, pace: Option<f64>) -> Geometry {
        geometry(WIDTH, HEIGHT, used, pace)
    }

    #[test]
    fn an_untouched_window_draws_no_fill_at_all() {
        assert_eq!(at(0.0, None).fill_width, None);
    }

    #[test]
    fn a_barely_touched_window_still_shows_something() {
        // 0.3% of 200px is half a pixel. Rounding it away would draw the same bar as a
        // window nothing has been spent in, which is the one thing it must not look like.
        assert_eq!(at(0.3, None).fill_width, Some(HEIGHT));
    }

    #[test]
    fn a_full_window_fills_the_bar_and_no_more() {
        assert_eq!(at(100.0, None).fill_width, Some(WIDTH));
        assert_eq!(at(140.0, None).fill_width, Some(WIDTH));
    }

    #[test]
    fn the_fill_is_proportional_in_between() {
        assert_eq!(at(50.0, None).fill_width, Some(100.0));
    }

    #[test]
    fn a_window_the_provider_said_nothing_about_gets_no_mark() {
        // The normal state of the leading card, not an edge case: see the module docs.
        assert_eq!(at(42.0, None).mark_x, None);
    }

    #[test]
    fn the_mark_sits_at_the_elapsed_fraction() {
        assert_eq!(at(42.0, Some(0.25)).mark_x, Some(50.0));
    }

    #[test]
    fn a_mark_at_either_end_stays_inside_the_widget() {
        assert_eq!(at(0.0, Some(0.0)).mark_x, Some(1.0));
        assert_eq!(at(100.0, Some(1.0)).mark_x, Some(WIDTH - 1.0));
    }

    #[test]
    fn a_bar_with_no_width_yet_does_not_produce_nonsense() {
        // Widgets are drawn once before they have been allocated a size.
        let geometry = geometry(0.0, 0.0, 50.0, Some(0.5));
        assert_eq!(geometry.fill_width, Some(0.0));
        assert_eq!(geometry.mark_x, Some(1.0));
    }

    #[test]
    fn the_bar_changes_colour_where_the_notifications_fire() {
        assert_eq!(tone(79.9), Tone::Normal);
        assert_eq!(tone(WARNING_AT), Tone::Warning);
        assert_eq!(tone(94.9), Tone::Warning);
        assert_eq!(tone(DANGER_AT), Tone::Danger);
    }
}
