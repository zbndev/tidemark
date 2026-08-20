//! One account, as a card.
//!
//! The shape is the one in `CONTEXT.md` § Interface: the provider's mark, name, plan and
//! state chip along the top; the shortest present window as a large number over a bar with
//! a pace mark; the remaining windows as thin rows; and a line along the bottom saying when
//! the reading was taken and when the next one is due.
//!
//! Two rules are structural here rather than remembered.
//!
//! **The last good reading stays on screen.** A card is never blanked because a poll
//! failed; the numbers keep standing and the chip changes. The only card with no numbers on
//! it is one that has never had any.
//!
//! **A window the provider did not send is not drawn.** The rows are rebuilt from whatever
//! arrived, so a provider that stops reporting a window loses a row without explanation,
//! and one that starts reporting a new one gains it the same way.

use std::cell::RefCell;

use gtk::prelude::*;
use tidemark_types::{ProviderStatus, Timestamp, Window, provider_label};

use crate::bar::QuotaBar;
use crate::format;
use crate::mark;
use crate::model;

/// Height of the bar under the headline number.
const DOMINANT_BAR: i32 = 14;
/// Height of the bar in a secondary window's row.
const ROW_BAR: i32 = 6;
/// Narrowest a card is allowed to get. Three of these plus spacing is what "three columns"
/// costs, and it is what stops `GtkFlowBox` from packing three unreadable columns into a
/// window that only has room for two.
const MIN_WIDTH: i32 = 300;

/// What the card is currently showing, kept so that the clock-dependent parts can be
/// redrawn without another D-Bus round trip.
#[derive(Debug)]
struct Shown {
    status: ProviderStatus,
    /// The bars of the secondary rows, in the order [`model::ordered_windows`] put them.
    secondary: Vec<QuotaBar>,
}

/// A provider card.
#[derive(Debug)]
pub struct Card {
    holder: gtk::FlowBoxChild,
    mark: gtk::Image,
    name: gtk::Label,
    plan: gtk::Label,
    chip: gtk::Label,
    reading: gtk::Box,
    blank: gtk::Label,
    headline: gtk::Label,
    dominant_title: gtk::Label,
    bar: QuotaBar,
    reset: gtk::Label,
    rows: gtk::Box,
    footer: gtk::Label,
    shown: RefCell<Shown>,
}

impl Card {
    /// Builds an empty card and fills it with `status`.
    pub fn new(status: &ProviderStatus, now: Timestamp) -> Self {
        let name = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        let plan = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption"])
            .build();
        let chip = gtk::Label::builder()
            .halign(gtk::Align::End)
            .hexpand(true)
            .css_classes(["caption", "quota-chip"])
            .build();

        // The mark and the name are one thing and are spaced as one; the plan and the chip
        // are separate columns of the row.
        let mark = mark::image();
        let named = gtk::Box::builder().spacing(6).build();
        named.append(&mark);
        named.append(&name);

        let title_row = gtk::Box::builder().spacing(8).build();
        title_row.append(&named);
        title_row.append(&plan);
        title_row.append(&chip);

        let headline = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["title-1"])
            .build();
        let dominant_title = gtk::Label::builder()
            .halign(gtk::Align::End)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let headline_row = gtk::Box::builder()
            .spacing(8)
            .valign(gtk::Align::Baseline)
            .build();
        headline_row.append(&headline);
        headline_row.append(&dominant_title);

        let bar = QuotaBar::new(DOMINANT_BAR);
        let reset = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption"])
            .build();

        let reading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        reading.append(&headline_row);
        reading.append(bar.widget());
        reading.append(&reset);

        // Shown in place of the reading, never alongside it: an account that has never
        // answered has nothing to put a number on.
        let blank = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        let rows = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        // Pinned to the bottom so that cards sharing a row line their footers up: GtkFlowBox
        // stretches every child to the height of the tallest one in the row, and without
        // this the extra space would fall in a different place on every card.
        let footer = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .vexpand(true)
            .css_classes(["dim-label", "caption"])
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .width_request(MIN_WIDTH)
            .css_classes(["card", "quota-card"])
            .build();
        root.append(&title_row);
        root.append(&reading);
        root.append(&blank);
        root.append(&rows);
        root.append(&footer);

        // The card owns its `GtkFlowBoxChild` rather than letting the grid wrap it on
        // insertion, so that the grid can reorder by sorting rather than by taking widgets
        // out and putting them back: re-parenting a widget that a disposed wrapper still
        // holds is a `Gtk-CRITICAL`, and it costs the focus and the pointer besides.
        let holder = gtk::FlowBoxChild::builder()
            .child(&root)
            .focusable(false)
            .build();

        let card = Self {
            holder,
            mark,
            name,
            plan,
            chip,
            reading,
            blank,
            headline,
            dominant_title,
            bar,
            reset,
            rows,
            footer,
            shown: RefCell::new(Shown {
                status: status.clone(),
                secondary: Vec::new(),
            }),
        };
        card.apply(status, now);
        card
    }

    /// The widget to put in the grid.
    pub fn widget(&self) -> &gtk::FlowBoxChild {
        &self.holder
    }

    /// The status this card is showing.
    pub fn status(&self) -> ProviderStatus {
        self.shown.borrow().status.clone()
    }

    /// Shows a new status for the same account.
    pub fn apply(&self, status: &ProviderStatus, now: Timestamp) {
        mark::set(&self.mark, &status.provider);
        self.name.set_label(&provider_label(&status.provider));

        match status.plan() {
            Some(plan) => {
                self.plan.set_label(plan);
                self.plan.set_visible(true);
            }
            None => self.plan.set_visible(false),
        }

        for class in format::Tone::ALL_CLASSES {
            self.chip.remove_css_class(class);
        }
        match format::chip(status) {
            Some(chip) => {
                self.chip.set_label(&chip.text);
                self.chip.add_css_class(chip.tone.css_class());
                self.chip.set_visible(true);
            }
            None => self.chip.set_visible(false),
        }

        let windows = status
            .to_snapshot()
            .map(|snapshot| model::ordered_windows(&snapshot))
            .unwrap_or_default();

        let secondary = match windows.split_first() {
            Some((dominant, rest)) => {
                self.reading.set_visible(true);
                self.blank.set_visible(false);
                self.dominant_title.set_label(&dominant.title);
                self.rebuild_rows(rest)
            }
            None => {
                self.reading.set_visible(false);
                self.blank.set_visible(true);
                self.blank.set_label(&blank_message(status));
                self.rebuild_rows(&[])
            }
        };

        *self.shown.borrow_mut() = Shown {
            status: status.clone(),
            secondary,
        };
        self.retime(now);
    }

    /// Redraws everything that depends on the current time rather than on a new reading:
    /// the pace marks, which move as the window elapses, and the two relative timestamps.
    pub fn retime(&self, now: Timestamp) {
        let shown = self.shown.borrow();

        match format::footer(&shown.status, now) {
            Some(line) => {
                self.footer.set_label(&line);
                self.footer.set_visible(true);
            }
            None => self.footer.set_visible(false),
        }

        let Some(snapshot) = shown.status.to_snapshot() else {
            return;
        };
        let windows = model::ordered_windows(&snapshot);
        let Some((dominant, rest)) = windows.split_first() else {
            return;
        };

        self.headline
            .set_label(&format::percent(dominant.used_percent));
        self.bar.set(dominant.used_percent, dominant.pace(now));
        match dominant.seconds_until_reset(now) {
            Some(seconds) => {
                self.reset.set_label(&format::resets_in(seconds));
                self.reset.set_visible(true);
            }
            // No reset time is the ordinary case for the window this card leads with. The
            // line is removed rather than filled with a guess.
            None => self.reset.set_visible(false),
        }

        for (bar, window) in shown.secondary.iter().zip(rest) {
            bar.set(window.used_percent, window.pace(now));
        }
    }

    /// Replaces the thin rows, returning their bars in the same order.
    fn rebuild_rows(&self, windows: &[Window]) -> Vec<QuotaBar> {
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        self.rows.set_visible(!windows.is_empty());

        windows
            .iter()
            .map(|window| {
                let title = gtk::Label::builder()
                    .label(&window.title)
                    .halign(gtk::Align::Start)
                    .width_chars(12)
                    .max_width_chars(12)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["dim-label", "caption"])
                    .build();
                let bar = QuotaBar::new(ROW_BAR);
                bar.widget().set_valign(gtk::Align::Center);
                let value = gtk::Label::builder()
                    .label(format::percent(window.used_percent))
                    .halign(gtk::Align::End)
                    .width_chars(5)
                    .xalign(1.0)
                    .css_classes(["dim-label", "caption", "numeric"])
                    .build();

                let row = gtk::Box::builder().spacing(8).build();
                row.append(&title);
                row.append(bar.widget());
                row.append(&value);
                self.rows.append(&row);
                bar
            })
            .collect()
    }
}

/// What to say on a card that has no numbers to show.
///
/// The daemon's own explanation when it gave one — it knows why, and it is more specific
/// than anything this side could infer from the state alone.
fn blank_message(status: &ProviderStatus) -> String {
    match status.message.as_deref() {
        Some(message) => message.to_owned(),
        None => "No reading yet.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    #[test]
    fn a_card_with_nothing_to_show_says_what_the_daemon_said() {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        assert_eq!(blank_message(&status), "No reading yet.");

        status.set_state(
            ProviderState::NoCredential,
            Some("No key is stored for zai.".into()),
        );
        assert_eq!(blank_message(&status), "No key is stored for zai.");
    }
}
