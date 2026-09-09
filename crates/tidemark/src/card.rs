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

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use tidemark_types::{
    DetailSection, Emphasis, Field, Metric, MetricWindow, Presentation, ProviderState,
    ProviderStatus, Timestamp, Widget, WidgetKind, Window, WindowKey, WindowLength, present,
};

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
    /// The secondary gauges, in published order (text rows have no bar).
    secondary: Vec<QuotaBar>,
}

/// The native account-group control shown on a provider's main card.
pub(crate) struct CardExpansion {
    pub(crate) extra_accounts: usize,
    pub(crate) expanded: bool,
    pub(crate) on_toggled: Rc<dyn Fn(bool)>,
}

/// The provider context and account name shown at the top of a quota card.
#[derive(Debug)]
pub(crate) struct CardTitle {
    heading: String,
    caption: Option<String>,
}

impl CardTitle {
    pub(crate) fn main(heading: String) -> Self {
        Self {
            heading,
            caption: None,
        }
    }

    pub(crate) fn child(provider: &str, account: &str) -> Self {
        Self {
            heading: account.into(),
            caption: Some(provider.into()),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct AccountToggleState {
    label: String,
    tooltip: &'static str,
}

fn account_toggle_state(extra_accounts: usize, expanded: bool) -> AccountToggleState {
    if expanded {
        AccountToggleState {
            label: "−".into(),
            tooltip: "Hide other accounts",
        }
    } else {
        AccountToggleState {
            label: format!("+{extra_accounts}"),
            tooltip: "Show other accounts",
        }
    }
}

/// The full window that currently makes this status window unavailable.
fn blocking_key<'a>(status: &'a ProviderStatus, key: &str) -> Option<&'a str> {
    status
        .windows
        .iter()
        .find(|window| window.key == key)
        .and_then(|window| window.blocked_by.as_deref())
}

/// Whether a card row's window is unavailable because another window consumed it.
///
/// A row without a window — a plugin's free-form text or a plain value — is never
/// blocked: it does not describe a quota that could stop being enforced.
fn row_blocked(status: &ProviderStatus, metric: &Metric) -> bool {
    metric
        .window
        .as_ref()
        .is_some_and(|window| blocking_key(status, &window.key).is_some())
}

/// A provider card.
#[derive(Debug)]
pub struct Card {
    slot: gtk::Overlay,
    mark: gtk::Image,
    name: gtk::Label,
    caption: gtk::Label,
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
    balance: gtk::Label,
    balance_only: gtk::Box,
    balance_whole: gtk::Label,
    balance_fraction: gtk::Label,
    footer: gtk::Label,
    shown: RefCell<Shown>,
}

/// A drawable widget resolved against its metric. Only old-daemon rows own their data.
pub(crate) struct Row<'a> {
    pub(crate) kind: WidgetKind,
    pub(crate) metric: Cow<'a, Metric>,
    pub(crate) widget: Cow<'a, Widget>,
}

impl Row<'_> {
    pub(crate) fn text(&self) -> Option<String> {
        widget_text(self.kind, &self.metric, &self.widget)
    }
}

pub(crate) fn presentation_row<'a>(
    presentation: &'a Presentation,
    widget: &'a Widget,
) -> Option<Row<'a>> {
    let kind = widget.kind()?;
    let metric = presentation.metric(&widget.metric)?;
    // A gauge is its bar first and its label second. Losing the label — a field it cannot
    // put a number on, or a format token only a newer daemon knows — must not cost the
    // percentage the producer actually reported, or a rolling upgrade degrades harder than
    // the extensible `a{sv}` contract implies. Every other kind *is* its text.
    let drawable = match kind {
        WidgetKind::Gauge => gauge_percent(metric, widget).is_some(),
        _ => widget_text(kind, metric, widget).is_some(),
    };
    drawable.then_some(())?;
    Some(Row {
        kind,
        metric: Cow::Borrowed(metric),
        widget: Cow::Borrowed(widget),
    })
}

/// Published order is authoritative. Only an absent presentation takes the rolling-upgrade path.
pub(crate) fn card_rows(status: &ProviderStatus) -> Vec<Row<'_>> {
    if let Some(presentation) = &status.presentation {
        return presentation
            .card
            .iter()
            .filter_map(|widget| presentation_row(presentation, widget))
            .collect();
    }
    status
        .to_snapshot()
        .map(|snapshot| model::ordered_windows(&snapshot))
        .unwrap_or_default()
        .into_iter()
        .filter(|window| window.used_percent.is_finite())
        .map(|window| {
            let metric = Metric {
                id: window.key.to_string(),
                title: window.title,
                subtitle: window.subtitle,
                value: None,
                maximum: None,
                remaining: None,
                used_percent: Some(window.used_percent),
                text: None,
                unit: None,
                window: Some(MetricWindow {
                    key: window.key.to_string(),
                    resets_at: window.resets_at.map(Timestamp::as_unix),
                    length_secs: window.length.map(WindowLength::as_secs),
                }),
            };
            let widget = Widget::gauge(&metric.id, Field::UsedPercent);
            Row {
                kind: WidgetKind::Gauge,
                metric: Cow::Owned(metric),
                widget: Cow::Owned(widget),
            }
        })
        .collect()
}

fn widget_text(kind: WidgetKind, metric: &Metric, widget: &Widget) -> Option<String> {
    // Unknown explicit selectors cannot silently choose another field or format.
    if widget.format.is_some() && widget.format().is_none() {
        return None;
    }
    match kind {
        WidgetKind::Gauge => {
            let percent = gauge_percent(metric, widget)?;
            let field = if widget.field.is_none() {
                Field::UsedPercent
            } else {
                widget.field()?
            };
            if field == Field::Text {
                return None;
            }
            if field == Field::UsedPercent {
                let metric = Metric {
                    used_percent: Some(percent),
                    ..metric.clone()
                };
                present::format_field(
                    &metric,
                    field,
                    widget.format().or(Some(tidemark_types::Format::Percent)),
                )
            } else {
                present::format_field(metric, field, widget.format())
            }
        }
        WidgetKind::Value => {
            let field = if widget.field.is_none() {
                Field::Text
            } else {
                widget.field()?
            };
            present::format_field(metric, field, widget.format())
        }
        WidgetKind::Ratio => {
            let (left, right) = (widget.left()?, widget.right()?);
            (metric.field(right)? > 0.0).then_some(())?;
            present::format_ratio(metric, left, right)
        }
        WidgetKind::Status => metric.text.clone(),
    }
}

/// Reported percentage, or a finite quotient with a positive denominator.
pub(crate) fn gauge_percent(metric: &Metric, widget: &Widget) -> Option<f64> {
    if let Some(used) = metric.used_percent {
        return used.is_finite().then_some(used);
    }
    let left = if widget.left.is_none() {
        Field::Value
    } else {
        widget.left()?
    };
    let right = if widget.right.is_none() {
        Field::Maximum
    } else {
        widget.right()?
    };
    let numerator = metric.field(left)?;
    let denominator = metric.field(right)?;
    let percent = numerator / denominator * 100.0;
    (denominator > 0.0 && percent.is_finite()).then_some(percent)
}

fn metric_window(metric: &Metric, used_percent: f64) -> Option<Window> {
    let window = metric.window.as_ref()?;
    Some(Window {
        key: WindowKey::named(&window.key),
        title: metric.title.clone(),
        subtitle: metric.subtitle.clone(),
        used_percent,
        resets_at: window
            .resets_at
            .and_then(|seconds| Timestamp::from_unix(seconds).ok()),
        length: window.length_secs.and_then(WindowLength::from_secs),
    })
}

impl Card {
    /// Builds an empty card and fills it with `status`.
    pub fn new(
        status: &ProviderStatus,
        now: Timestamp,
        title: CardTitle,
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
        let caption = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
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
        let title_stack = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .build();
        title_stack.append(&caption);
        title_stack.append(&name);
        title_row.append(&mark);
        title_row.append(&title_stack);
        title_row.append(&plan);
        title_row.append(&chip);
        plan.set_margin_start(TITLE_GAP - MARK_GAP);

        let expansion = expansion.map(|expansion| {
            let state = account_toggle_state(expansion.extra_accounts, expansion.expanded);
            let content = gtk::Label::builder().label(&state.label).build();
            let toggle = gtk::ToggleButton::builder()
                .active(expansion.expanded)
                .child(&content)
                .css_classes(["flat", "circular", "quota-account-toggle"])
                .tooltip_text(state.tooltip)
                .build();
            toggle.connect_toggled(move |toggle| {
                let state = account_toggle_state(expansion.extra_accounts, toggle.is_active());
                content.set_label(&state.label);
                toggle.set_tooltip_text(Some(state.tooltip));
                (expansion.on_toggled)(toggle.is_active());
            });
            toggle
        });
        align_to_baseline(&name, &mark, &plan);

        let headline = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
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

        // The wallet amount under the quota rows. Money is not a rate, so it gets no bar
        // and no percentage — just the provider's phrasing of the amount, bold and between
        // the headline and the secondary rows in size. See `DetailSection::BALANCE` for the
        // convention this rides on.
        let balance = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["quota-balance"])
            .build();

        // A balance with no quota windows is the card's primary reading. Keep it separate
        // from the ordinary headline row: the amount sits vertically centred at the left,
        // with its fraction on the same baseline in quieter type and its meaning below.
        let balance_whole = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Baseline)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["numeric", "quota-balance-whole"])
            .build();
        let balance_fraction = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Baseline)
            .css_classes(["numeric", "quota-balance-fraction"])
            .build();
        let balance_amount = gtk::Box::builder()
            .spacing(0)
            .valign(gtk::Align::Baseline)
            .build();
        balance_amount.append(&balance_whole);
        balance_amount.append(&balance_fraction);
        let balance_caption = gtk::Label::builder()
            .label(DetailSection::BALANCE)
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        let balance_only = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .build();
        balance_only.append(&balance_amount);
        balance_only.append(&balance_caption);
        balance_only.set_visible(false);

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
        body.append(&balance_only);
        body.append(&rows);
        body.append(&balance);
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
            caption,
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
            balance,
            balance_only,
            balance_whole,
            balance_fraction,
            footer,
            shown: RefCell::new(Shown {
                status: status.clone(),
                secondary: Vec::new(),
            }),
        };
        card.set_title(title);
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
    pub fn set_title(&self, title: CardTitle) {
        self.name.set_label(&title.heading);
        self.caption
            .set_label(title.caption.as_deref().unwrap_or_default());
        self.caption.set_visible(title.caption.is_some());
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

        let rows = card_rows(status);

        self.set_absolutes(absolutes_for(status).as_deref());

        let balance = balance_for(status);
        let secondary = match (rows.split_first(), balance) {
            (Some((dominant, rest)), _) => {
                self.reading.set_visible(true);
                self.blank.set_visible(false);
                self.set_balance_only(None);
                self.footer.set_vexpand(true);
                self.bar
                    .widget()
                    .set_visible(dominant.kind == WidgetKind::Gauge);
                self.dominant_title.set_label(&dominant.metric.title);
                self.headline
                    .set_label(&dominant.text().unwrap_or_default());
                self.headline.set_css_classes(
                    if dominant.widget.emphasis() == Some(Emphasis::Compact) {
                        &["heading"]
                    } else {
                        &["title-1"]
                    },
                );
                self.set_balance_line(balance);
                self.rebuild_rows(rest, status)
            }
            (None, Some(balance)) => {
                self.reading.set_visible(false);
                self.blank.set_visible(false);
                self.set_balance_only(Some(balance));
                self.footer.set_vexpand(false);
                self.set_absolutes(None);
                self.set_balance_line(None);
                self.rebuild_rows(&[], status)
            }
            (None, None) => {
                self.reading.set_visible(false);
                self.blank.set_visible(true);
                self.set_balance_only(None);
                self.footer.set_vexpand(true);
                self.set_balance_line(None);
                let message = blank_message(status);
                // The card shows the first `BLANK_LINES` of it; the tooltip is where the
                // rest of a long one is, so that truncating it costs nothing on the way to
                // the settings pane.
                self.blank.set_tooltip_text(Some(&message));
                self.blank.set_label(&message);
                self.rebuild_rows(&[], status)
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

        let rows = card_rows(&shown.status);
        let Some((dominant, rest)) = rows.split_first() else {
            return;
        };

        let window = if dominant.kind == WidgetKind::Gauge {
            gauge_percent(&dominant.metric, &dominant.widget).and_then(|percent| {
                let window = metric_window(&dominant.metric, percent);
                self.bar
                    .set(percent, window.as_ref().and_then(|window| window.pace(now)));
                window
            })
        } else {
            None
        };
        // A window another window has consumed is drawn dimmed, with its reset line gone:
        // the reset belongs to a quota that is not being enforced right now.
        let blocked = window
            .as_ref()
            .is_some_and(|window| blocking_key(&shown.status, window.key.as_str()).is_some());
        self.headline.set_opacity(if blocked { 0.5 } else { 1.0 });
        self.bar.set_blocked(blocked);
        if blocked {
            self.reset.set_visible(false);
        } else {
            match window.and_then(|window| window.seconds_until_reset(now)) {
                Some(seconds) => {
                    self.reset.set_label(&format::resets_in(seconds));
                    self.reset.set_visible(true);
                }
                // No reset time is the ordinary case for the window this card leads with. The
                // line is removed rather than filled with a guess.
                None => self.reset.set_visible(false),
            }
        }

        for (bar, row) in shown
            .secondary
            .iter()
            .zip(rest.iter().filter(|row| row.kind == WidgetKind::Gauge))
        {
            if let Some(percent) = gauge_percent(&row.metric, &row.widget) {
                let pace = metric_window(&row.metric, percent).and_then(|window| window.pace(now));
                bar.set(percent, pace);
            }
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

    /// Shows the wallet amount under the quota rows, or removes the line.
    ///
    /// Only a reading with windows draws it: a balance-only card already shows the amount
    /// as its headline, and this line must not repeat it. Emptied as well as hidden, for
    /// the same reason `set_absolutes` empties.
    fn set_balance_line(&self, amount: Option<&str>) {
        match amount {
            Some(text) => {
                self.balance.set_label(text);
                self.balance.set_visible(true);
            }
            None => {
                self.balance.set_label("");
                self.balance.set_visible(false);
            }
        }
    }

    /// Shows the primary amount for a card that has no quota windows.
    fn set_balance_only(&self, amount: Option<&str>) {
        match amount {
            Some(amount) => {
                let (whole, fraction) = balance_parts(amount);
                self.balance_whole.set_label(whole);
                match fraction {
                    Some(fraction) => {
                        self.balance_fraction.set_label(fraction);
                        self.balance_fraction.set_visible(true);
                    }
                    None => {
                        self.balance_fraction.set_label("");
                        self.balance_fraction.set_visible(false);
                    }
                }
                self.balance_only.set_visible(true);
            }
            None => {
                self.balance_whole.set_label("");
                self.balance_fraction.set_label("");
                self.balance_fraction.set_visible(false);
                self.balance_only.set_visible(false);
            }
        }
    }

    /// Replaces the thin rows, returning their bars in the same order.
    fn rebuild_rows(&self, rows: &[Row<'_>], status: &ProviderStatus) -> Vec<QuotaBar> {
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        self.rows.set_visible(!rows.is_empty());

        rows.iter()
            .filter_map(|resolved| {
                let title = gtk::Label::builder()
                    .label(&resolved.metric.title)
                    .halign(gtk::Align::Start)
                    .width_chars(12)
                    .max_width_chars(12)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["dim-label", "caption"])
                    .build();
                let blocked = row_blocked(status, &resolved.metric);
                let opacity = if blocked { 0.5 } else { 1.0 };
                let value = gtk::Label::builder()
                    .label(resolved.text().unwrap_or_default())
                    .halign(gtk::Align::End)
                    .hexpand(true)
                    .width_chars(5)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .xalign(1.0)
                    .css_classes(["dim-label", "caption", "numeric"])
                    .build();
                value.set_opacity(opacity);

                let row = gtk::Box::builder().spacing(8).build();
                row.append(&title);
                let bar = (resolved.kind == WidgetKind::Gauge).then(|| {
                    let bar = QuotaBar::new(ROW_BAR);
                    bar.widget().set_valign(gtk::Align::Center);
                    bar.set_blocked(blocked);
                    value.set_hexpand(false);
                    row.append(bar.widget());
                    bar
                });
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
    card_rows(status)
        .first()
        .and_then(|row| row.metric.subtitle.clone())
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

/// Splits a formatted balance at its decimal point so the fraction can use smaller type.
///
/// Only the currency shape the adapters publish is split. Arbitrary provider prose stays
/// intact rather than shrinking everything after its last full stop.
fn balance_parts(amount: &str) -> (&str, Option<&str>) {
    let Some(point) = amount.rfind('.') else {
        return (amount, None);
    };
    let (whole, fraction) = amount.split_at(point);
    let digits = &fraction[1..];
    if !whole.is_empty()
        && (1..=4).contains(&digits.len())
        && digits.bytes().all(|digit| digit.is_ascii_digit())
    {
        (whole, Some(fraction))
    } else {
        (amount, None)
    }
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
            blocked_by: None,
        }
    }

    fn status_with(windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.captured_at = Some(CAPTURED_AT);
        status.windows = windows;
        status
    }
    #[test]
    fn a_blocked_five_hour_window_exposes_its_full_parent() {
        let mut five_hour = window(Some(18_000), 0.0);
        five_hour.key = "w18000".into();
        five_hour.blocked_by = Some("w604800".into());
        let status = status_with(vec![five_hour]);

        assert_eq!(blocking_key(&status, "w18000"), Some("w604800"));
    }

    fn numeric_metric(id: &str) -> tidemark_types::Metric {
        tidemark_types::Metric {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            value: Some(12.5),
            maximum: Some(50.0),
            remaining: Some(37.5),
            used_percent: Some(25.0),
            text: Some("active".into()),
            unit: Some("USD".into()),
            window: None,
        }
    }

    fn presented(
        card: Vec<tidemark_types::Widget>,
        metrics: Vec<tidemark_types::Metric>,
    ) -> ProviderStatus {
        let mut status = status_with(Vec::new());
        status.presentation = Some(tidemark_types::Presentation {
            metrics,
            card,
            details: Vec::new(),
        });
        status
    }

    #[test]
    fn the_card_draws_the_widgets_in_the_published_order() {
        use tidemark_types::{Field, Widget, WidgetKind};
        let status = presented(
            vec![
                Widget::gauge("a", Field::UsedPercent),
                Widget::value("b", Field::Remaining),
                Widget::ratio("a", Field::Value, Field::Maximum),
            ],
            vec![numeric_metric("a"), numeric_metric("b")],
        );
        assert_eq!(
            card_rows(&status)
                .iter()
                .map(|row| row.kind)
                .collect::<Vec<_>>(),
            [WidgetKind::Gauge, WidgetKind::Value, WidgetKind::Ratio]
        );
    }

    #[test]
    fn a_widget_naming_a_metric_that_is_not_there_is_not_drawn() {
        use tidemark_types::{Field, Widget};
        let status = presented(
            vec![Widget::gauge("ghost", Field::UsedPercent)],
            vec![numeric_metric("a")],
        );
        assert!(card_rows(&status).is_empty());
    }

    #[test]
    fn a_card_from_an_older_daemon_still_draws_its_windows() {
        let mut status = status_with(vec![window(Some(18_000), 42.0)]);
        status.presentation = None;
        assert_eq!(card_rows(&status).len(), 1);
    }

    #[test]
    fn an_empty_presentation_does_not_resurrect_legacy_windows() {
        let mut status = presented(Vec::new(), Vec::new());
        status.windows = vec![window(Some(18_000), 42.0)];
        assert!(card_rows(&status).is_empty());
    }

    #[test]
    fn a_gauge_spells_its_selected_field_while_its_bar_uses_the_percentage() {
        use tidemark_types::{Field, Format, Widget};
        let status = presented(
            vec![Widget::gauge("a", Field::Remaining).formatted(Format::Currency)],
            vec![numeric_metric("a")],
        );
        let rows = card_rows(&status);
        assert_eq!(rows[0].text().as_deref(), Some("37.50 USD"));
        assert_eq!(gauge_percent(&rows[0].metric, &rows[0].widget), Some(25.0));
    }

    #[test]
    fn a_gauge_keeps_its_bar_when_its_label_cannot_be_spelled() {
        use tidemark_types::{Field, Widget};
        // A newer daemon's format token, and a field a gauge cannot put a number on. Both
        // cost the number; neither costs the percentage the producer actually reported.
        let mut unreadable_format = Widget::gauge("a", Field::UsedPercent);
        unreadable_format.format = Some("bushels".into());
        let status = presented(
            vec![unreadable_format, Widget::gauge("a", Field::Text)],
            vec![numeric_metric("a")],
        );
        let rows = card_rows(&status);
        assert_eq!(
            rows.len(),
            2,
            "an unspellable label does not remove the bar"
        );
        for row in &rows {
            assert_eq!(row.text(), None);
            assert_eq!(gauge_percent(&row.metric, &row.widget), Some(25.0));
        }
    }

    #[test]
    fn undrawable_values_ratios_and_gauges_stay_absent() {
        use tidemark_types::{Field, Widget};
        let mut metric = numeric_metric("a");
        metric.used_percent = None;
        metric.maximum = Some(0.0);
        metric.remaining = None;
        metric.text = None;
        let status = presented(
            vec![
                Widget::gauge("a", Field::UsedPercent),
                Widget::value("a", Field::Remaining),
                Widget::ratio("a", Field::Value, Field::Maximum),
                Widget::status("a"),
            ],
            vec![metric],
        );
        assert!(card_rows(&status).is_empty());
    }

    #[test]
    fn a_gauge_derives_a_finite_percentage_and_only_uses_reported_window_pace() {
        use tidemark_types::{Field, MetricWindow, Widget};
        let mut metric = numeric_metric("a");
        metric.used_percent = None;
        let widget = Widget::gauge("a", Field::UsedPercent);
        assert_eq!(gauge_percent(&metric, &widget), Some(25.0));
        assert!(metric_window(&metric, 25.0).is_none());
        metric.window = Some(MetricWindow {
            key: "a".into(),
            resets_at: Some(CAPTURED_AT + 3600),
            length_secs: Some(18_000),
        });
        let now = Timestamp::from_unix(CAPTURED_AT).unwrap();
        assert_eq!(metric_window(&metric, 25.0).unwrap().pace(now), Some(0.8));
        metric.value = Some(f64::MAX);
        metric.maximum = Some(f64::MIN_POSITIVE);
        assert_eq!(gauge_percent(&metric, &widget), None);
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
    fn a_balance_amount_separates_its_fraction_for_smaller_type() {
        assert_eq!(balance_parts("$454.5426"), ("$454", Some(".5426")));
        assert_eq!(balance_parts("$60"), ("$60", None));
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
    fn a_child_card_title_keeps_the_provider_and_account_identity_without_card_chrome() {
        let title = CardTitle::child("Claude", "Work");

        assert_eq!(title.caption.as_deref(), Some("Claude"));
        assert_eq!(title.heading, "Work");
        assert!(
            !crate::style::STYLE.contains(".quota-child-card"),
            "nested identity belongs in the provider settings list, not card chrome"
        );
    }

    #[test]
    fn expanding_an_account_toggle_changes_its_words_without_becoming_transparent() {
        let collapsed = account_toggle_state(2, false);
        let expanded = account_toggle_state(2, true);

        assert_eq!(collapsed.label, "+2");
        assert_eq!(collapsed.tooltip, "Show other accounts");
        assert_eq!(expanded.label, "−");
        assert_eq!(expanded.tooltip, "Hide other accounts");
        assert!(
            crate::style::STYLE.contains(
                ".quota-account-toggle:checked {\n    background-color: @accent_bg_color;"
            ),
            "the active account toggle must remain opaque"
        );
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
