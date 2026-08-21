//! One account, as a card.
//!
//! The shape is the one in `CONTEXT.md` § Interface: the provider's mark, name, plan and
//! state chip along the top; the shortest present window as a large number over a bar with
//! a pace mark; the remaining windows as thin rows; and a line along the bottom saying when
//! the reading was taken.
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
use std::rc::Rc;

use gtk::prelude::*;
use tidemark_types::{ProviderStatus, Timestamp, Window, provider_label};

use crate::bar::QuotaBar;
use crate::format;
use crate::mark;
use crate::model;

/// Height of the bar under the headline number. A third thinner than the bar this card
/// started with: the height it lost is what the title row spends on a larger mark.
const DOMINANT_BAR: i32 = 9;
/// Height of the bar in a secondary window's row.
const ROW_BAR: i32 = 6;
/// Space between the columns of the title row.
const TITLE_GAP: i32 = 8;
/// Space between the mark and the name it belongs to, which is less.
const MARK_GAP: i32 = 6;
/// Vertical padding the `.quota-plan` pill draws inside itself, which the plan's allocation
/// carries and its baseline therefore sits above. Kept in step with `style::STYLE` by the
/// test below.
const PILL_PADDING: i32 = 2;

/// Narrowest a card is allowed to get. Three of these plus spacing is what "three columns"
/// costs, and it is what stops `GtkFlowBox` from packing three unreadable columns into a
/// window that only has room for two.
const MIN_WIDTH: i32 = 300;

/// The account a card opens, retained separately from the readings that update in place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CardIdentity {
    provider: String,
    account: String,
}

impl From<&ProviderStatus> for CardIdentity {
    fn from(status: &ProviderStatus) -> Self {
        Self {
            provider: status.provider.clone(),
            account: status.account.clone(),
        }
    }
}

impl CardIdentity {
    fn activate(&self, on_activate: &dyn Fn(String, String)) {
        on_activate(self.provider.clone(), self.account.clone());
    }
}

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
    pub fn new(
        status: &ProviderStatus,
        now: Timestamp,
        on_activate: Rc<dyn Fn(String, String)>,
    ) -> Self {
        let name = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        let plan = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .css_classes(["caption", "quota-plan"])
            .build();
        let chip = gtk::Label::builder()
            .halign(gtk::Align::End)
            .hexpand(true)
            .css_classes(["caption", "quota-chip"])
            .build();

        // The mark, the name and the plan stand on one line. Aligning them to the bottom of
        // the row is not enough to get there: GTK aligns allocations, and a label's
        // allocation ends at its font's descent line rather than at its baseline, so a mark
        // flush with the row's bottom hangs below the word beside it by the depth of a "y".
        // Each is lifted by the descent it does not use — see `align_to_baseline`.
        let mark = mark::image();
        mark.set_valign(gtk::Align::End);
        name.set_valign(gtk::Align::End);

        // The mark belongs to the name more tightly than the plan does, so the row is spaced
        // at the tighter of the two and the plan makes up the difference. GTK margins cannot
        // be negative, which is why it is this way round.
        let title_row = gtk::Box::builder().spacing(MARK_GAP).build();
        title_row.append(&mark);
        title_row.append(&name);
        title_row.append(&plan);
        title_row.append(&chip);
        plan.set_margin_start(TITLE_GAP - MARK_GAP);

        align_to_baseline(&name, &mark, &plan);

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
            .css_classes(["caption", "quota-footer"])
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .width_request(MIN_WIDTH)
            .css_classes(["card", "quota-card", "activatable"])
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
            .focusable(true)
            .build();
        holder.set_cursor_from_name(Some("pointer"));
        let identity = CardIdentity::from(status);
        let invoke: Rc<dyn Fn()> = Rc::new({
            let on_activate = Rc::clone(&on_activate);
            move || identity.activate(on_activate.as_ref())
        });
        let click = gtk::GestureClick::new();
        click.connect_released({
            let invoke = Rc::clone(&invoke);
            move |_, _, _, _| invoke()
        });
        holder.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let invoke = Rc::clone(&invoke);
            move |_, key, _, _| {
                if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::space)
                {
                    invoke();
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            }
        });
        holder.add_controller(keys);

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

/// Puts the mark, the name and the plan on one baseline, once there is a resolved font to
/// ask about — which is at map time, not at construction.
///
/// All three are bottom-aligned, so each one's *allocation* ends on the row's lower edge.
/// What has to end there instead is the baseline, and each of them carries a different
/// amount of nothing underneath it: the name its font's descent, the plan a smaller font's
/// descent plus the padding its pill draws, and the mark none at all. So the line is put as
/// deep as the deepest of them needs, and each is lifted by what it does not use.
///
/// GTK will not take a negative margin, which is why the line is chosen this way round
/// rather than by lifting everything to the name.
fn align_to_baseline(name: &gtk::Label, mark: &gtk::Image, plan: &gtk::Label) {
    let mark = mark.clone();
    let plan = plan.clone();
    name.connect_map(move |name| {
        let descent = |widget: &gtk::Label| {
            widget.pango_context().metrics(None, None).descent() / gtk::pango::SCALE
        };
        let under_name = descent(name);
        let under_plan = descent(&plan) + PILL_PADDING;
        let line = under_name.max(under_plan);

        name.set_margin_bottom(line - under_name);
        plan.set_margin_bottom(line - under_plan);
        mark.set_margin_bottom(line);
    });
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
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    #[test]
    fn the_pill_padding_the_baseline_maths_assumes_is_the_one_the_stylesheet_draws() {
        // `align_to_baseline` subtracts this from the plan's lift. If the stylesheet changes
        // the pill's padding and this constant does not follow, the plan drifts off the
        // name's baseline by the difference — a two-pixel bug nobody would look for here.
        assert!(
            crate::style::STYLE.contains(&format!("padding: {PILL_PADDING}px 9px")),
            "the .quota-plan padding and PILL_PADDING have to agree"
        );
    }

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

    #[test]
    fn activating_a_card_keeps_its_provider_and_account_identity() {
        let status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::new("work"));
        let identity = CardIdentity::from(&status);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&calls);
        let activate = Rc::new(move |provider: String, account: String| {
            observed.borrow_mut().push((provider, account));
        });

        identity.activate(activate.as_ref());
        assert_eq!(calls.borrow().as_slice(), [("zai".into(), "work".into())]);
    }
}
