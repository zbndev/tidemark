//! One account, as a card.
//!
//! The shape is the one in `CONTEXT.md` § Interface: the provider's mark, name, plan and
//! state chip along the top; the shortest present window as a large number over a bar with
//! a pace mark; the remaining windows as thin rows; and a line along the bottom saying when
//! the reading was taken.
//!
//! Three rules are structural here rather than remembered.
//!
//! **The last good reading stays on screen.** A card is never blanked because a poll
//! failed; the numbers keep standing and the chip changes. The only card with no numbers on
//! it is one that has never had any.
//!
//! **A window the provider did not send is not drawn.** The rows are rebuilt from whatever
//! arrived, so a provider that stops reporting a window loses a row without explanation,
//! and one that starts reporting a new one gains it the same way.
//!
//! **Nothing a daemon says changes the size of a card.** Every string on a card arrives from
//! another process — a provider's name, a plan, a window title, an error message — and every
//! label that shows one either ellipsizes or wraps at any character, so a string too long
//! for the space shortens itself. It has to: `grid::CardGrid` gives every cell the widest
//! card's *minimum* width, so a label that answered "as wide as my text" would be answering
//! for every card on screen.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use tidemark_types::{DetailSection, ProviderState, ProviderStatus, Timestamp, Window};

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

/// How wide a card is. Not a floor: the grid gives every cell exactly this, so three of
/// these plus spacing is what "three columns" costs, and no card can widen its neighbours.
///
/// That is only true while **every line on a card can shorten itself** — the rule the
/// `ellipsize`, `wrap_mode` and `width_chars` calls below exist for. A label that can do
/// neither reports the width of its text as its minimum, `grid::CardGrid::cell_width` takes
/// the widest minimum on screen, and one provider's long error message becomes every card's
/// width. That is the bug this constant used to lose to.
const MIN_WIDTH: i32 = 300;
/// Most lines of a daemon message a card will lay out before ellipsizing it. Three is two
/// more than any live message needs; what it stops is a paragraph deciding how tall every
/// card in the row is. The whole of it stays readable in the provider's settings pane, which
/// has the width and the scroller for it.
const BLANK_LINES: i32 = 3;

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

/// The native account-group control shown on a provider's main card.
pub(crate) struct CardExpansion {
    pub(crate) extra_accounts: usize,
    pub(crate) expanded: bool,
    pub(crate) on_toggled: Rc<dyn Fn(bool)>,
}

/// A provider card.
#[derive(Debug)]
pub struct Card {
    slot: gtk::Overlay,
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
    absolutes: gtk::Label,
    rows: gtk::Box,
    footer: gtk::Label,
    shown: RefCell<Shown>,
}

impl Card {
    /// Builds an empty card and fills it with `status`.
    pub fn new(
        status: &ProviderStatus,
        now: Timestamp,
        title: String,
        on_activate: Rc<dyn Fn(String, String)>,
        expansion: Option<CardExpansion>,
    ) -> Self {
        // All three ellipsize. None of them is expected to: a provider's name, its plan and
        // a state chip all fit the row this build ships. They ellipsize because the three
        // strings come from the *daemon* — a catalog entry, a plan the provider named, and,
        // for a state this build has never heard of, that state verbatim — and a label that
        // cannot shorten itself sets the width of every card in the grid. See `MIN_WIDTH`.
        //
        // Where the row does run short, GTK gives each label its minimum and shares what is
        // left smallest-need-first, so the name is the last thing to lose characters.
        let name = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let plan = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "quota-plan"])
            .build();
        let chip = gtk::Label::builder()
            .halign(gtk::Align::End)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
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

        let expansion = expansion.map(|expansion| {
            let content = gtk::Label::builder()
                .label(format!("+{}", expansion.extra_accounts))
                .build();
            let toggle = gtk::ToggleButton::builder()
                .active(expansion.expanded)
                .child(&content)
                .css_classes(["flat", "circular", "quota-account-toggle"])
                .tooltip_text("Show other accounts")
                .build();
            toggle.connect_toggled(move |toggle| (expansion.on_toggled)(toggle.is_active()));
            toggle
        });
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

        // The absolute quantities behind the percentage, exactly as the provider phrased
        // them. Presentation the provider owns: it is set, never parsed and never
        // reformatted. A provider that reports only a percentage leaves it hidden, so the
        // card keeps the height it has always had.
        //
        // One line, ellipsized: the provider decides how long this string is, and neither
        // the card's width nor its height is the provider's to decide.
        let absolutes = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();

        let reading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        reading.append(&headline_row);
        reading.append(bar.widget());
        reading.append(&reset);
        reading.append(&absolutes);

        // Shown in place of the reading, never alongside it: an account that has never
        // answered has nothing to put a number on.
        //
        // The one place on a card where daemon prose is printed whole, and therefore the
        // one that has to be told, twice, that it may not resize the card. `WordChar` is
        // what breaks the strings these messages are actually made of — a D-Bus error name,
        // a URL, a token with no space in it anywhere. Plain word wrapping cannot break any
        // of those, so it asks for the width of the longest one instead, and asking is
        // enough: a minimum width is a minimum for every card in the grid. `lines` with an
        // ellipsis bounds the other axis; see `BLANK_LINES`.
        let blank = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .lines(BLANK_LINES)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        let rows = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        // Pinned to the bottom so that cards sharing a row line their footers up: the grid
        // gives every card the height of the tallest one, and without this the extra space
        // would fall in a different place on every card.
        let footer = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .vexpand(true)
            .css_classes(["caption", "quota-footer"])
            .build();

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        body.append(&title_row);
        body.append(&reading);
        body.append(&blank);
        body.append(&rows);
        body.append(&footer);
        let root = gtk::Overlay::builder()
            .child(&body)
            .width_request(MIN_WIDTH)
            .css_classes(["card", "quota-card", "activatable"])
            .build();
        root.set_overflow(gtk::Overflow::Visible);

        // The card owns the widget the grid allocates rather than letting the grid wrap it,
        // so that a reorder moves slots around a vector instead of taking widgets out of
        // the tree and putting them back: re-parenting a widget that a disposed wrapper
        // still holds is a `Gtk-CRITICAL`, and it costs the focus and the pointer besides.
        //
        // A plain `AdwBin` and not a `GtkFlowBoxChild`, which tinted its own square
        // allocation behind a card with rounded corners and had to be told not to. What it
        // is still for is the hover: `style.rs` matches `:hover` here and moves the card
        // inside, because a CSS transform moves what GTK picks and a card that lifted
        // itself out from under the pointer would flicker.
        let slot = gtk::Overlay::builder()
            .child(&root)
            .focusable(true)
            .css_classes([crate::grid::SLOT_CLASS])
            .build();
        slot.set_overflow(gtk::Overflow::Visible);
        if let Some(expansion) = expansion {
            expansion.set_halign(gtk::Align::End);
            expansion.set_valign(gtk::Align::Start);
            slot.add_overlay(&expansion);
        }
        slot.set_cursor_from_name(Some("pointer"));
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
        slot.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let invoke = Rc::clone(&invoke);
            move |_, key, _, _| {
                if matches!(
                    key,
                    gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::space
                ) {
                    invoke();
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            }
        });
        slot.add_controller(keys);

        let card = Self {
            slot,
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
            absolutes,
            rows,
            footer,
            shown: RefCell::new(Shown {
                status: status.clone(),
                secondary: Vec::new(),
            }),
        };
        card.set_title(&title);
        card.apply(status, now);
        card
    }

    /// The widget the grid allocates. Not the card itself: see the slot above.
    pub fn widget(&self) -> &gtk::Widget {
        self.slot.upcast_ref()
    }

    /// The status this card is showing.
    pub fn status(&self) -> ProviderStatus {
        self.shown.borrow().status.clone()
    }

    /// Replaces the title resolved by the window for this account.
    pub fn set_title(&self, title: &str) {
        self.name.set_label(title);
    }

    /// Shows a new status for the same account.
    pub fn apply(&self, status: &ProviderStatus, now: Timestamp) {
        mark::set(&self.mark, &status.provider);

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

        self.set_absolutes(absolutes_for(status).as_deref());

        let balance = balance_for(status);
        let secondary = match (windows.split_first(), balance) {
            (Some((dominant, rest)), _) => {
                self.reading.set_visible(true);
                self.blank.set_visible(false);
                self.bar.widget().set_visible(true);
                self.dominant_title.set_label(&dominant.title);
                self.rebuild_rows(rest)
            }
            (None, Some(balance)) => {
                self.reading.set_visible(true);
                self.blank.set_visible(false);
                self.headline.set_label(balance);
                self.dominant_title.set_label(DetailSection::BALANCE);
                self.bar.widget().set_visible(false);
                self.reset.set_visible(false);
                self.set_absolutes(None);
                self.rebuild_rows(&[])
            }
            (None, None) => {
                self.reading.set_visible(false);
                self.blank.set_visible(true);
                let message = blank_message(status);
                // The card shows the first `BLANK_LINES` of it; the tooltip is where the
                // rest of a long one is, so that truncating it costs nothing on the way to
                // the settings pane.
                self.blank.set_tooltip_text(Some(&message));
                self.blank.set_label(&message);
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

    /// Shows the dominant window's absolutes, or removes the line when there are none.
    ///
    /// The label is emptied as well as hidden so that a card whose provider stops sending
    /// absolutes cannot flash the previous account's numbers if the line is shown again.
    fn set_absolutes(&self, subtitle: Option<&str>) {
        match subtitle {
            Some(text) => {
                self.absolutes.set_label(text);
                self.absolutes.set_visible(true);
            }
            None => {
                self.absolutes.set_label("");
                self.absolutes.set_visible(false);
            }
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

/// The absolutes the card leads with: the dominant window's subtitle, when there is one.
///
/// A decision about the data rather than about the drawing — which window's absolutes
/// belong on the card — and therefore a free function, testable without a windowing
/// system. [`Card::set_absolutes`] only renders what this decides.
fn absolutes_for(status: &ProviderStatus) -> Option<String> {
    let snapshot = status.to_snapshot()?;
    model::ordered_windows(&snapshot)
        .first()
        .and_then(|window| window.subtitle.clone())
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

/// An amount-only balance from a successful reading.
///
/// Failed polls leave the last good details in place. Those details must not hide the
/// daemon's current explanation, so only an `Ok` state may promote its balance to the card.
fn balance_for(status: &ProviderStatus) -> Option<&str> {
    (status.state() == Some(ProviderState::Ok))
        .then(|| status.balance())
        .flatten()
}

/// What to say on a card that has no quota window or amount-only balance.
fn blank_message(status: &ProviderStatus) -> String {
    status
        .message
        .as_deref()
        .unwrap_or("No reading yet.")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use tidemark_types::{
        AccountId, DetailRow, DetailSection, ProviderId, ProviderState, WindowStatus,
    };

    /// When the readings below were taken. Fixed so that nothing here depends on the wall
    /// clock.
    const CAPTURED_AT: i64 = 1_785_700_000;

    fn window(length_secs: Option<u64>, used_percent: f64) -> WindowStatus {
        WindowStatus {
            key: format!("w{length_secs:?}"),
            title: format!("{length_secs:?}"),
            subtitle: None,
            used_percent,
            resets_at: Some(CAPTURED_AT + 3_600),
            length_secs,
        }
    }

    fn status_with(windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.captured_at = Some(CAPTURED_AT);
        status.windows = windows;
        status
    }

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
    fn a_balance_only_reading_shows_its_amount_instead_of_a_placeholder() {
        let mut status = status_with(Vec::new());
        status.details = vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows: vec![DetailRow {
                label: "Balance".to_owned(),
                value: "$1.93".to_owned(),
            }],
        }];
        status.set_state(ProviderState::Ok, None);

        assert_eq!(balance_for(&status), Some("$1.93"));
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

    #[test]
    fn the_absolutes_come_from_the_dominant_window_and_not_from_whichever_arrived_first() {
        let mut weekly = window(Some(604_800), 22.0);
        weekly.subtitle = Some("220 / 1000 weekly prompts".to_owned());
        let mut five_hour = window(Some(18_000), 42.0);
        five_hour.subtitle = Some("420 / 1000 prompts".to_owned());
        let status = status_with(vec![weekly, five_hour]);

        assert_eq!(
            absolutes_for(&status).as_deref(),
            Some("420 / 1000 prompts"),
            "the card leads with the shortest window, so it must lead with its absolutes"
        );
    }

    #[test]
    fn a_reading_without_absolutes_decides_on_none_rather_than_an_empty_line() {
        let status = status_with(vec![window(Some(18_000), 42.0)]);
        assert_eq!(
            absolutes_for(&status),
            None,
            "a provider that reports only a percentage must not grow a blank line"
        );

        let mut pending = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        pending.captured_at = None;
        assert_eq!(
            absolutes_for(&pending),
            None,
            "no reading means no absolutes"
        );
    }
}
