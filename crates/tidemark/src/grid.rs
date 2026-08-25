//! The grid the cards sit in, and the drag that reorders them.
//!
//! # Why this is a widget of our own
//!
//! `GtkFlowBox` laid out the columns until reordering arrived, and it cannot do the part
//! that matters: while a card is being carried between two others, the cards it displaces
//! have to move out of its way *before* the button is released, and move back if the
//! pointer changes its mind. `gtk_flow_box_invalidate_sort()` sorts its sequence and queues
//! a resize, so a card that loses its place teleports to the new one. There is no position
//! to interpolate, because the position *is* the allocation.
//!
//! Nothing in GTK 4.22 or libadwaita 1.9 does this in the general case. `GtkNotebook` can
//! reorder tabs and `AdwTabView` can reorder pages, and both keep how they animate it to
//! themselves. What libadwaita does have is `AdwTabGrid` — private, and an exact match for
//! the problem — and this module is its architecture:
//!
//! - one `GtkGestureDrag` on the container, not `GtkDragSource`. A drag source draws a
//!   detached icon, and an icon that is not in the grid cannot push anything;
//! - a per-card offset in **index units**, animated by an `AdwTimedAnimation` that is
//!   restarted *from its current value*. That restart is what makes "changed their mind"
//!   work: a card half-way out of the way is retargeted to zero and carries on from where
//!   it is, rather than snapping back and starting again;
//! - the animation callback queues an allocation, and `size_allocate` turns a fractional
//!   index into a position;
//! - the order is committed once, on release. Not on every motion event.
//!
//! # Why the arithmetic is in free functions
//!
//! [`columns`], [`slot`], [`drop_index`] and [`shift`] are where this can be wrong, and none
//! of them needs a display to be wrong in. They are tested below, the way `bar::geometry`
//! is.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::subclass::prelude::*;

/// Space between cards, both ways. What the `GtkFlowBox` used.
const SPACING: i32 = 12;
/// Most columns, however wide the window gets. Three 300-pixel cards is already a wide
/// window, and a fourth column would make each card a strip.
const MAX_COLUMNS: usize = 3;
/// How long a displaced card takes to get out of the way. `AdwTabGrid`'s figure.
const REORDER_MS: u32 = 250;
/// How long a released card takes to settle into its slot.
const SETTLE_MS: u32 = 200;
/// How close to the edge of the scrolled view the pointer has to get before the view
/// follows it. Roughly a finger's width: near enough to be deliberate, far enough that it
/// engages before the pointer is jammed against the edge.
const AUTOSCROLL_EDGE: f64 = 48.0;
/// Fastest the view scrolls itself, in pixels per second, reached at the very edge.
const AUTOSCROLL_RATE: f64 = 900.0;

/// The style class the slot around a card carries.
pub const SLOT_CLASS: &str = "quota-slot";
/// Added to the *slot* of the card being carried, so the stylesheet can lift the card
/// inside it. On the slot rather than the card because the grid holds slots and knows
/// nothing about what is in them; `style.rs` matches `.quota-slot.dragging > .quota-card`.
const DRAGGING_CLASS: &str = "dragging";
/// The class the grid itself carries, which every rule in `style.rs` is scoped to. Added
/// here rather than by whoever builds the grid, so a rule cannot quietly stop matching
/// because a caller forgot it.
pub const GRID_CLASS: &str = "quota-grid";
/// Added to the *grid* for as long as any card is off its slot.
///
/// Every card, and not only the ones in motion. `.card` is 8% white over whatever is behind
/// it in the dark style, and when two cards overlap the one on top may be either of them: a
/// card the drag left standing still is painted above every card with a lower index, so a
/// traveller sliding under one showed through it however opaque the traveller itself was
/// made. The cheapest thing that is right is for no card to be translucent while any card
/// is moving. `style.rs` matches `.quota-grid.reordering > .quota-slot > .quota-card`.
const REORDERING_CLASS: &str = "reordering";

/// How many columns fit, between one and `max`.
///
/// Never zero: a window too narrow for one card gets one column and a horizontal squeeze,
/// which is what the card's own width request is for.
fn columns(available: i32, cell: i32, spacing: i32, max: usize) -> usize {
    if cell <= 0 {
        return 1;
    }
    let fitting = (available + spacing) / (cell + spacing);
    (fitting.max(1) as usize).min(max.max(1))
}

/// Where slot `index` starts, relative to the first slot.
///
/// A fractional index interpolates between the two whole slots either side of it, which
/// includes the step from the end of one row to the start of the next: a card moving across
/// that boundary travels there rather than jumping.
fn slot(index: f64, columns: usize, cell: (f64, f64), spacing: (f64, f64)) -> (f64, f64) {
    let whole = |index: f64| {
        let columns = columns.max(1) as f64;
        let row = (index / columns).floor();
        let column = index - row * columns;
        (column * (cell.0 + spacing.0), row * (cell.1 + spacing.1))
    };
    let floor = index.floor();
    let fraction = index - floor;
    if fraction == 0.0 {
        return whole(floor);
    }
    let (x0, y0) = whole(floor);
    let (x1, y1) = whole(floor + 1.0);
    (x0 + fraction * (x1 - x0), y0 + fraction * (y1 - y0))
}

/// The slot a dragged card's centre is over.
///
/// Measured against the **canonical** slots — where the cards would be if nothing were
/// being dragged — rather than against where they currently are. Canonical slots do not
/// move, so the answer is a monotone function of the pointer and cannot flicker between two
/// values while the pointer is still.
fn drop_index(
    centre: (f64, f64),
    count: usize,
    columns: usize,
    cell: (f64, f64),
    spacing: (f64, f64),
) -> usize {
    if count == 0 {
        return 0;
    }
    let columns = columns.max(1);
    let column = (centre.0 / (cell.0 + spacing.0))
        .floor()
        .clamp(0.0, (columns - 1) as f64) as usize;
    let rows = count.div_ceil(columns);
    let row = (centre.1 / (cell.1 + spacing.1))
        .floor()
        .clamp(0.0, (rows - 1) as f64) as usize;
    (row * columns + column).min(count - 1)
}

/// The card a press at this point actually landed on, if it landed on one.
///
/// [`drop_index`] answers "which slot is this nearest to", which is what a drag in progress
/// needs and what a press must not use: with five cards in three columns the sixth slot is
/// empty, and the gutters and the centring margin are not on a card either. Answering
/// "the nearest one" there would pick a card up off a click on the background.
fn slot_at(
    point: (f64, f64),
    count: usize,
    columns: usize,
    cell: (f64, f64),
    spacing: (f64, f64),
) -> Option<usize> {
    if count == 0 || cell.0 <= 0.0 || cell.1 <= 0.0 {
        return None;
    }
    let index = drop_index(point, count, columns, cell, spacing);
    let (x, y) = slot(index as f64, columns, cell, spacing);
    let inside = point.0 >= x && point.0 < x + cell.0 && point.1 >= y && point.1 < y + cell.1;
    inside.then_some(index)
}

/// The index shift a card at `index` takes while the card from `from` is carried to `to`.
///
/// Zero for the dragged card itself, which is positioned by the pointer rather than by a
/// slot, and zero for everything outside the span the move covers.
fn shift(index: usize, from: usize, to: usize) -> f64 {
    if index == from {
        return 0.0;
    }
    if from < to && index > from && index <= to {
        return -1.0;
    }
    if from > to && index >= to && index < from {
        return 1.0;
    }
    0.0
}

/// The order the cards are painted in: the ones standing still, then the ones in motion,
/// then the carried one, each group keeping its slot order.
///
/// Painting order is child order, and slot order is the wrong child order the moment a card
/// leaves its slot. A card travelling to a slot on another row crosses the rows between,
/// and every card the drag left alone with a higher index is painted after it — so the
/// travelling card slid *under* its neighbours instead of over them. Nothing that is
/// standing still should be over something that is moving.
fn paint_order(motion: &[bool], carried: Option<usize>) -> Vec<usize> {
    let carried = carried.filter(|index| *index < motion.len());
    let mut order = Vec::with_capacity(motion.len());
    for group in [false, true] {
        order.extend(
            (0..motion.len()).filter(|index| Some(*index) != carried && motion[*index] == group),
        );
    }
    order.extend(carried);
    order
}

/// How fast the scrolled view should follow a pointer this far into its edge, in pixels per
/// second. Positive scrolls down. Zero everywhere but the two edge bands.
fn autoscroll_rate(pointer: f64, height: f64) -> f64 {
    if height <= 2.0 * AUTOSCROLL_EDGE {
        return 0.0;
    }
    if pointer < AUTOSCROLL_EDGE {
        return -AUTOSCROLL_RATE * (1.0 - pointer.max(0.0) / AUTOSCROLL_EDGE);
    }
    let from_bottom = height - pointer;
    if from_bottom < AUTOSCROLL_EDGE {
        return AUTOSCROLL_RATE * (1.0 - from_bottom.max(0.0) / AUTOSCROLL_EDGE);
    }
    0.0
}

/// One card's place in the grid, and where it currently is on the way to it.
struct Slot {
    child: gtk::Widget,
    /// Index shift as drawn right now, interpolated by [`Slot::animation`].
    offset: Cell<f64>,
    /// Index shift being animated towards.
    target: Cell<f64>,
    animation: RefCell<Option<adw::TimedAnimation>>,
}

/// A drag in progress, or a released card still settling into its slot.
struct Drag {
    /// Where the card is in the slot vector. Updated when the release commits the move.
    index: usize,
    /// The card's top-left in grid coordinates when the press landed.
    origin: (f64, f64),
    /// How far the pointer has moved since, as the gesture reports it.
    pointer: (f64, f64),
    /// How far the scrolled view has moved itself since, which the card follows too.
    scrolled: f64,
    /// Provisional destination, recomputed on every motion.
    target: usize,
    /// Set once the pointer has travelled far enough to be a drag rather than a click.
    carrying: bool,
    /// While the released card animates into its slot: where it started from.
    settle: Option<(f64, f64)>,
    /// Zero to one across the settle.
    progress: f64,
    animation: Option<adw::TimedAnimation>,
    autoscroll: Option<gtk::TickCallbackId>,
}

/// What the grid hands a completed move to: the card's old index, then its new one.
type OnReorder = Rc<dyn Fn(usize, usize)>;

impl Drag {
    /// The card's current top-left in grid coordinates, given where its slot is.
    fn at(&self, resting: (f64, f64)) -> (f64, f64) {
        match self.settle {
            None => (
                self.origin.0 + self.pointer.0,
                self.origin.1 + self.pointer.1 + self.scrolled,
            ),
            Some(from) => (
                from.0 + self.progress * (resting.0 - from.0),
                from.1 + self.progress * (resting.1 - from.1),
            ),
        }
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct CardGrid {
        pub(super) slots: RefCell<Vec<Slot>>,
        pub(super) drag: RefCell<Option<Drag>>,
        /// Called once per completed move, with the indices it went between.
        pub(super) on_reorder: RefCell<Option<OnReorder>>,
        /// The layout the last allocation settled on, so the drag arithmetic and the
        /// allocation cannot come to different conclusions about where a slot is.
        pub(super) columns: Cell<usize>,
        pub(super) cell: Cell<(f64, f64)>,
    }

    // By hand because the reorder callback is a closure. What is worth printing is the
    // shape of the grid and whether a card is in the air, not the identity of a function.
    impl std::fmt::Debug for CardGrid {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("CardGrid")
                .field("cards", &self.slots.borrow().len())
                .field("columns", &self.columns.get())
                .field("cell", &self.cell.get())
                .field("dragging", &self.drag.borrow().is_some())
                .finish()
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CardGrid {
        const NAME: &'static str = "TidemarkCardGrid";
        type Type = super::CardGrid;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for CardGrid {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().add_css_class(GRID_CLASS);
            self.obj().install_gesture();
        }

        fn dispose(&self) {
            self.slots.borrow_mut().clear();
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for CardGrid {
        /// The number of columns depends on the width, so the height depends on it too.
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let count = self.slots.borrow().len();
            if count == 0 {
                return (0, 0, -1, -1);
            }
            let cell_width = self.cell_width();
            match orientation {
                // One column is the minimum, because a card is as narrow as it is going to
                // get. The natural width is however many columns there are cards for, up to
                // the maximum, which is what stops a wide window at three.
                gtk::Orientation::Horizontal => {
                    let wanted = count.min(MAX_COLUMNS) as i32;
                    (
                        cell_width,
                        wanted * cell_width + (wanted - 1) * SPACING,
                        -1,
                        -1,
                    )
                }
                _ => {
                    let available = if for_size < 0 {
                        count.min(MAX_COLUMNS) as i32 * (cell_width + SPACING) - SPACING
                    } else {
                        for_size
                    };
                    let columns = columns(available, cell_width, SPACING, MAX_COLUMNS);
                    let rows = count.div_ceil(columns) as i32;
                    let height = rows * self.natural_height(cell_width) + (rows - 1) * SPACING;
                    (height, height, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, baseline: i32) {
            let slots = self.slots.borrow();
            if slots.is_empty() {
                return;
            }
            let cell_width = self.cell_width();
            let cell_height = self.natural_height(cell_width);
            let columns = columns(width, cell_width, SPACING, MAX_COLUMNS);
            self.columns.set(columns);
            self.cell
                .set((f64::from(cell_width), f64::from(cell_height)));

            let cell = (f64::from(cell_width), f64::from(cell_height));
            let spacing = (f64::from(SPACING), f64::from(SPACING));
            let left = content_left(width, columns, cell.0);
            let drag = self.drag.borrow();

            for (index, held) in slots.iter().enumerate() {
                let resting = slot(index as f64, columns, cell, spacing);
                let (x, y) = match drag.as_ref() {
                    Some(drag)
                        if drag.index == index && (drag.carrying || drag.settle.is_some()) =>
                    {
                        drag.at(resting)
                    }
                    _ => slot(index as f64 + held.offset.get(), columns, cell, spacing),
                };
                let transform = gsk::Transform::new()
                    .translate(&graphene::Point::new((left + x) as f32, y as f32));
                held.child
                    .allocate(cell_width, cell_height, baseline, Some(transform));
            }
        }
    }
}

glib::wrapper! {
    /// A grid of cards the user can drag into the order they want.
    pub struct CardGrid(ObjectSubclass<imp::CardGrid>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for CardGrid {
    fn default() -> Self {
        Self::new()
    }
}

/// Where the block of columns starts inside an allocation of this width.
///
/// The block is centred: every card gets one column's width whatever the window is doing,
/// so a filled row and a half-empty one both sit under the middle of the window rather than
/// hugging its left edge.
fn content_left(width: i32, columns: usize, cell_width: f64) -> f64 {
    let columns = columns.max(1) as f64;
    let used = columns * cell_width + (columns - 1.0) * f64::from(SPACING);
    (f64::from(width) - used).max(0.0) / 2.0
}

impl imp::CardGrid {
    /// The width every cell gets: the widest card's **minimum** width.
    ///
    /// Minimum and not natural, and that is the whole of the rule that keeps cards the same
    /// size. A card's natural width is a function of the text it was handed — a provider
    /// that answers with a long error message, or a window title nobody sized the card for,
    /// asks for a wider card — and every cell in this grid is the same width, so under
    /// `natural` one long string from one daemon widened every card on screen. A minimum is
    /// the card's own width request and nothing the daemon said, so a card that cannot fit
    /// what it was given shortens it, which is what `card.rs` ellipsizes and wraps for.
    ///
    /// Height is still natural, below: a card that needs two lines gets two lines, and its
    /// neighbours match it so the footers line up. Width is the axis with a right answer.
    fn cell_width(&self) -> i32 {
        self.slots
            .borrow()
            .iter()
            .map(|held| held.child.measure(gtk::Orientation::Horizontal, -1).0)
            .max()
            .unwrap_or(0)
    }

    /// The tallest card's natural height at that width, which every cell gets — so cards
    /// sharing a row share a height and their footers line up.
    fn natural_height(&self, for_width: i32) -> i32 {
        self.slots
            .borrow()
            .iter()
            .map(|held| held.child.measure(gtk::Orientation::Vertical, for_width).1)
            .max()
            .unwrap_or(0)
    }
}

impl CardGrid {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Adds a card at the end. Where a new account goes, always.
    pub fn append(&self, child: &impl IsA<gtk::Widget>) {
        let child = child.as_ref().clone();
        child.set_parent(self);
        self.imp().slots.borrow_mut().push(Slot {
            child,
            offset: Cell::new(0.0),
            target: Cell::new(0.0),
            animation: RefCell::new(None),
        });
        self.queue_resize();
    }

    /// Removes one card, leaving the order of the rest alone.
    pub fn remove(&self, child: &impl IsA<gtk::Widget>) {
        self.abandon_drag();
        let child = child.as_ref();
        let mut slots = self.imp().slots.borrow_mut();
        if let Some(index) = slots.iter().position(|held| &held.child == child) {
            slots.remove(index).child.unparent();
        }
        drop(slots);
        self.queue_resize();
    }

    /// Takes every card out.
    pub fn clear(&self) {
        self.abandon_drag();
        for held in self.imp().slots.borrow_mut().drain(..) {
            held.child.unparent();
        }
        self.queue_resize();
    }

    /// Puts the cards in this order, which should be the ones the grid holds.
    ///
    /// Applying an order the grid is already in does nothing, which is the ordinary case:
    /// the daemon echoes back the order this client just asked it for.
    pub fn set_order(&self, order: &[gtk::Widget]) {
        {
            let slots = self.imp().slots.borrow();
            if slots.len() == order.len()
                && slots
                    .iter()
                    .zip(order)
                    .all(|(held, wanted)| &held.child == wanted)
            {
                return;
            }
        }
        self.abandon_drag();
        let mut slots = self.imp().slots.borrow_mut();
        let mut held: Vec<Option<Slot>> = slots.drain(..).map(Some).collect();
        for wanted in order {
            let found = held
                .iter()
                .position(|slot| slot.as_ref().is_some_and(|slot| &slot.child == wanted));
            if let Some(index) = found
                && let Some(slot) = held[index].take()
            {
                slots.push(slot);
            }
        }
        // Anything the order did not name keeps its relative place at the end rather than
        // being dropped: the sequence arrives from another process and may have raced an add.
        slots.extend(held.into_iter().flatten());
        drop(slots);
        self.reset_child_order();
        self.queue_allocate();
    }

    /// Registers what to do with a completed move: the card's old and new index.
    pub fn connect_reordered(&self, on_reorder: impl Fn(usize, usize) + 'static) {
        *self.imp().on_reorder.borrow_mut() = Some(Rc::new(on_reorder));
    }

    /// The one gesture this widget has. Bubble phase, so a card's own click gesture sees the
    /// press first; the sequence is claimed only once the pointer has travelled far enough
    /// to be a drag, and claiming it is what cancels that click.
    fn install_gesture(&self) {
        let gesture = gtk::GestureDrag::new();
        gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
        gesture.set_exclusive(true);
        gesture.connect_drag_begin({
            let grid = self.downgrade();
            move |gesture, x, y| {
                if let Some(grid) = grid.upgrade() {
                    grid.begin(gesture, x, y);
                }
            }
        });
        gesture.connect_drag_update({
            let grid = self.downgrade();
            move |gesture, dx, dy| {
                if let Some(grid) = grid.upgrade() {
                    grid.update(gesture, dx, dy);
                }
            }
        });
        gesture.connect_drag_end({
            let grid = self.downgrade();
            move |_, _, _| {
                if let Some(grid) = grid.upgrade() {
                    grid.release();
                }
            }
        });
        self.add_controller(gesture);
    }

    /// Notes which card the press landed on, and nothing else. Whether this is a drag or a
    /// click is not known yet, and deciding here would break the click.
    fn begin(&self, gesture: &gtk::GestureDrag, x: f64, y: f64) {
        self.abandon_drag();
        let imp = self.imp();
        let count = imp.slots.borrow().len();
        if count < 2 {
            // One card has nowhere to go, and no cards have nothing to move.
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        let (columns, cell) = (imp.columns.get(), imp.cell.get());
        let spacing = (f64::from(SPACING), f64::from(SPACING));
        let left = content_left(self.width(), columns, cell.0);
        // A press on a gutter, on the centring margin, or on the empty tail of the last row
        // is not a press on a card. `cell` is zero until the first allocation, which
        // `slot_at` also refuses rather than guessing an index from nothing.
        let Some(index) = slot_at((x - left, y), count, columns, cell, spacing) else {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        *imp.drag.borrow_mut() = Some(Drag {
            index,
            origin: slot(index as f64, columns, cell, spacing),
            pointer: (0.0, 0.0),
            scrolled: 0.0,
            target: index,
            carrying: false,
            settle: None,
            progress: 0.0,
            animation: None,
            autoscroll: None,
        });
    }

    /// Follows the pointer, once it has gone far enough to mean it.
    fn update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let starting = {
            let mut held = self.imp().drag.borrow_mut();
            let Some(drag) = held.as_mut() else {
                return;
            };
            if drag.settle.is_some() {
                return;
            }
            drag.pointer = (dx, dy);
            if drag.carrying {
                false
            } else if self.drag_check_threshold(0, 0, dx as i32, dy as i32) {
                drag.carrying = true;
                true
            } else {
                return;
            }
        };
        if starting {
            // Claiming cancels the pressed card's own click gesture, so a drag never opens
            // the detail dialog. The card also moves to the end of the child list, which is
            // what puts it above its siblings for both painting and picking.
            gesture.set_state(gtk::EventSequenceState::Claimed);
            // Cards are about to overlap, in both directions of z-order. Nothing on this
            // grid is translucent until the last of them is back on a slot.
            self.add_css_class(REORDERING_CLASS);
            let index = self.imp().drag.borrow().as_ref().map(|drag| drag.index);
            if let Some(index) = index {
                let slots = self.imp().slots.borrow();
                if let Some(held) = slots.get(index) {
                    let child = held.child.clone();
                    child.add_css_class(DRAGGING_CLASS);
                    drop(slots);
                    child.insert_before(self, None::<&gtk::Widget>);
                }
            }
        }
        self.follow_edge(gesture);
        self.carry();
    }

    /// Puts the carried card where the pointer is, and moves whatever it displaces.
    fn carry(&self) {
        let imp = self.imp();
        let (columns, cell) = (imp.columns.get(), imp.cell.get());
        let spacing = (f64::from(SPACING), f64::from(SPACING));
        let count = imp.slots.borrow().len();
        let shifts: Vec<(usize, f64)> = {
            let mut held = imp.drag.borrow_mut();
            let Some(drag) = held.as_mut() else {
                return;
            };
            if !drag.carrying || drag.settle.is_some() {
                return;
            }
            let (x, y) = drag.at((0.0, 0.0));
            let centre = (x + cell.0 / 2.0, y + cell.1 / 2.0);
            let target = drop_index(centre, count, columns, cell, spacing);
            if target == drag.target {
                self.queue_allocate();
                return;
            }
            drag.target = target;
            let from = drag.index;
            (0..count)
                .map(|index| (index, shift(index, from, target)))
                .collect()
        };
        for (index, wanted) in shifts {
            self.animate_offset(index, wanted);
        }
        self.raise_moving();
        self.queue_allocate();
    }

    /// Retargets one card's index offset, continuing from wherever it currently is.
    ///
    /// The restart is the point. A card half-way out of the way that is asked to come back
    /// carries on from where it is; putting it back in its old slot and animating again
    /// would make the grid twitch every time the pointer crossed a boundary.
    fn animate_offset(&self, index: usize, wanted: f64) {
        let slots = self.imp().slots.borrow();
        let Some(held) = slots.get(index) else {
            return;
        };
        if held.target.get() == wanted {
            return;
        }
        held.target.set(wanted);
        if let Some(running) = held.animation.borrow_mut().take() {
            running.pause();
        }
        let child = held.child.clone();
        let from = held.offset.get();
        drop(slots);

        let target = adw::CallbackAnimationTarget::new({
            let grid = self.downgrade();
            move |value| {
                let Some(grid) = grid.upgrade() else {
                    return;
                };
                {
                    let slots = grid.imp().slots.borrow();
                    if let Some(held) = slots.iter().find(|held| held.child == child) {
                        held.offset.set(value);
                    }
                }
                grid.queue_allocate();
            }
        });
        let animation = adw::TimedAnimation::new(self, from, wanted, REORDER_MS, target);
        animation.set_easing(adw::Easing::EaseOutCubic);
        animation.play();
        let slots = self.imp().slots.borrow();
        if let Some(held) = slots.get(index) {
            *held.animation.borrow_mut() = Some(animation);
        }
    }

    /// Commits the move and lets the released card settle into its slot.
    fn release(&self) {
        let imp = self.imp();
        let Some((from, to)) = ({
            let mut held = imp.drag.borrow_mut();
            held.as_mut().and_then(|drag| {
                if !drag.carrying || drag.settle.is_some() {
                    return None;
                }
                if let Some(autoscroll) = drag.autoscroll.take() {
                    autoscroll.remove();
                }
                Some((drag.index, drag.target))
            })
        }) else {
            self.abandon_drag();
            return;
        };

        let (columns, cell) = (imp.columns.get(), imp.cell.get());
        let spacing = (f64::from(SPACING), f64::from(SPACING));
        let landing = imp
            .drag
            .borrow()
            .as_ref()
            .expect("the drag was read above")
            .at((0.0, 0.0));

        // Committed here rather than on the way: a client that wrote a new order on every
        // motion event would spend a file write and a D-Bus round trip per pixel.
        {
            let mut slots = imp.slots.borrow_mut();
            let moved = slots.remove(from);
            slots.insert(to, moved);
            for held in slots.iter() {
                held.target.set(0.0);
                held.offset.set(0.0);
                if let Some(running) = held.animation.borrow_mut().take() {
                    running.pause();
                }
            }
        }

        let resting = slot(to as f64, columns, cell, spacing);
        if landing == resting {
            self.abandon_drag();
        } else {
            self.settle(to, landing);
        }
        if from != to
            && let Some(on_reorder) = imp.on_reorder.borrow().clone()
        {
            on_reorder(from, to);
        }
    }

    /// Animates the released card from where the pointer left it into its slot.
    fn settle(&self, index: usize, from: (f64, f64)) {
        let imp = self.imp();
        {
            let mut held = imp.drag.borrow_mut();
            let Some(drag) = held.as_mut() else {
                return;
            };
            drag.index = index;
            drag.target = index;
            drag.settle = Some(from);
            drag.progress = 0.0;
        }
        let target = adw::CallbackAnimationTarget::new({
            let grid = self.downgrade();
            move |value| {
                let Some(grid) = grid.upgrade() else {
                    return;
                };
                if let Some(drag) = grid.imp().drag.borrow_mut().as_mut() {
                    drag.progress = value;
                }
                grid.queue_allocate();
            }
        });
        let animation = adw::TimedAnimation::new(self, 0.0, 1.0, SETTLE_MS, target);
        animation.set_easing(adw::Easing::EaseOutCubic);
        animation.connect_done({
            let grid = self.downgrade();
            move |_| {
                if let Some(grid) = grid.upgrade() {
                    grid.abandon_drag();
                }
            }
        });
        animation.play();
        if let Some(drag) = imp.drag.borrow_mut().as_mut() {
            drag.animation = Some(animation);
        }
    }

    /// Ends whatever the grid was doing with a card: a finished settle, an interrupted one,
    /// or a drag whose cards have been replaced underneath it.
    fn abandon_drag(&self) {
        let held = self.imp().drag.borrow_mut().take();
        if let Some(drag) = held {
            if let Some(animation) = drag.animation {
                animation.pause();
            }
            if let Some(autoscroll) = drag.autoscroll {
                autoscroll.remove();
            }
        }
        self.remove_css_class(REORDERING_CLASS);
        for held in self.imp().slots.borrow().iter() {
            held.child.remove_css_class(DRAGGING_CLASS);
            held.target.set(0.0);
            held.offset.set(0.0);
            if let Some(running) = held.animation.borrow_mut().take() {
                running.pause();
            }
        }
        self.reset_child_order();
        self.queue_allocate();
    }

    /// Puts the child list back in slot order, so picking and painting agree with it again.
    fn reset_child_order(&self) {
        let mut previous: Option<gtk::Widget> = None;
        for held in self.imp().slots.borrow().iter() {
            held.child.insert_after(self, previous.as_ref());
            previous = Some(held.child.clone());
        }
    }

    /// Rebuilds the child list in [`paint_order`], so a card in motion is painted over the
    /// cards standing still. Once per change of destination, not once per frame: which
    /// cards are moving only changes when the pointer crosses a slot boundary.
    fn raise_moving(&self) {
        let carried = self
            .imp()
            .drag
            .borrow()
            .as_ref()
            .filter(|drag| drag.carrying || drag.settle.is_some())
            .map(|drag| drag.index);
        let slots = self.imp().slots.borrow();
        let motion: Vec<bool> = slots
            .iter()
            .map(|held| held.offset.get() != 0.0 || held.target.get() != 0.0)
            .collect();
        let mut previous: Option<gtk::Widget> = None;
        for index in paint_order(&motion, carried) {
            let child = slots[index].child.clone();
            child.insert_after(self, previous.as_ref());
            previous = Some(child);
        }
    }

    /// Starts, adjusts or stops the scroll that follows a pointer held near an edge.
    ///
    /// Without this the bottom row of a tall grid is unreachable: the pointer cannot leave
    /// the viewport, and six cards in two columns are taller than the window.
    fn follow_edge(&self, gesture: &gtk::GestureDrag) {
        let Some(scroller) = self
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
        else {
            return;
        };
        let Some((start_x, start_y)) = gesture.start_point() else {
            return;
        };
        let Some((dx, dy)) = gesture.offset() else {
            return;
        };
        let Some(pointer) = self.compute_point(
            &scroller,
            &graphene::Point::new((start_x + dx) as f32, (start_y + dy) as f32),
        ) else {
            return;
        };
        let rate = autoscroll_rate(f64::from(pointer.y()), f64::from(scroller.height()));
        let running = self
            .imp()
            .drag
            .borrow()
            .as_ref()
            .is_some_and(|drag| drag.autoscroll.is_some());
        if rate == 0.0 {
            if running
                && let Some(drag) = self.imp().drag.borrow_mut().as_mut()
                && let Some(autoscroll) = drag.autoscroll.take()
            {
                autoscroll.remove();
            }
            return;
        }
        if running {
            return;
        }
        let adjustment = scroller.vadjustment();
        let last: Cell<Option<i64>> = Cell::new(None);
        let callback = self.add_tick_callback({
            let scroller = scroller.clone();
            move |grid, clock| {
                let now = clock.frame_time();
                let elapsed = match last.replace(Some(now)) {
                    // Microseconds. The first frame of a scroll advances nothing, which is
                    // one frame of latency and no risk of a jump from a stale clock.
                    Some(previous) => (now - previous) as f64 / 1_000_000.0,
                    None => 0.0,
                };
                let Some(rate) = grid.edge_rate(&scroller) else {
                    return glib::ControlFlow::Break;
                };
                let before = adjustment.value();
                let wanted = (before + rate * elapsed)
                    .clamp(0.0, (adjustment.upper() - adjustment.page_size()).max(0.0));
                adjustment.set_value(wanted);
                if let Some(drag) = grid.imp().drag.borrow_mut().as_mut() {
                    // The card follows the view, so it stays under the pointer instead of
                    // sliding away from it while the grid moves.
                    drag.scrolled += wanted - before;
                }
                grid.carry();
                glib::ControlFlow::Continue
            }
        });
        if let Some(drag) = self.imp().drag.borrow_mut().as_mut() {
            drag.autoscroll = Some(callback);
        }
    }

    /// The scroll rate the pointer currently asks for, or `None` when there is no drag left
    /// to scroll for.
    fn edge_rate(&self, scroller: &gtk::ScrolledWindow) -> Option<f64> {
        let carried = {
            let held = self.imp().drag.borrow();
            let drag = held.as_ref()?;
            if !drag.carrying || drag.settle.is_some() {
                return None;
            }
            (
                drag.origin.0 + drag.pointer.0,
                drag.origin.1 + drag.pointer.1 + drag.scrolled,
            )
        };
        let cell = self.imp().cell.get();
        let point = self.compute_point(
            scroller,
            &graphene::Point::new(
                (carried.0 + cell.0 / 2.0) as f32,
                (carried.1 + cell.1 / 2.0) as f32,
            ),
        )?;
        Some(autoscroll_rate(
            f64::from(point.y()),
            f64::from(scroller.height()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (f64, f64) = (300.0, 200.0);
    const GAP: (f64, f64) = (12.0, 12.0);

    #[test]
    fn columns_are_the_most_that_fit_up_to_the_maximum() {
        assert_eq!(
            columns(200, 300, 12, 3),
            1,
            "a window too narrow for one card still shows one"
        );
        assert_eq!(columns(300, 300, 12, 3), 1);
        assert_eq!(columns(611, 300, 12, 3), 1, "one pixel short of two");
        assert_eq!(columns(612, 300, 12, 3), 2);
        assert_eq!(columns(936, 300, 12, 3), 3);
        assert_eq!(columns(4000, 300, 12, 3), 3, "and never a fourth");
    }

    #[test]
    fn whole_slots_lay_out_in_rows() {
        assert_eq!(slot(0.0, 3, CELL, GAP), (0.0, 0.0));
        assert_eq!(slot(2.0, 3, CELL, GAP), (624.0, 0.0));
        assert_eq!(slot(3.0, 3, CELL, GAP), (0.0, 212.0));
        assert_eq!(slot(4.0, 3, CELL, GAP), (312.0, 212.0));
    }

    #[test]
    fn a_half_step_across_the_end_of_a_row_travels_there() {
        let (x, y) = slot(2.5, 3, CELL, GAP);
        let (end_x, end_y) = slot(2.0, 3, CELL, GAP);
        let (next_x, next_y) = slot(3.0, 3, CELL, GAP);
        assert_eq!((x, y), ((end_x + next_x) / 2.0, (end_y + next_y) / 2.0));
        assert!(
            y > 0.0 && y < 212.0,
            "a card crossing rows is between them, not on one"
        );
    }

    #[test]
    fn the_drop_index_is_the_slot_the_centre_is_over() {
        // Five cards in three columns: 0, 1 and 2 on the first row, 3 and 4 on the second.
        let index = |x: f64, y: f64| drop_index((x, y), 5, 3, CELL, GAP);
        assert_eq!(index(10.0, 10.0), 0);
        assert_eq!(index(311.0, 10.0), 0, "still inside the first column");
        assert_eq!(index(313.0, 10.0), 1, "over the second");
        assert_eq!(index(700.0, 10.0), 2);
        assert_eq!(index(10.0, 300.0), 3);
        assert_eq!(index(700.0, 300.0), 4, "clamped to the last card");
    }

    #[test]
    fn the_drop_index_clamps_rather_than_running_off_the_grid() {
        assert_eq!(drop_index((-500.0, -500.0), 5, 3, CELL, GAP), 0);
        assert_eq!(drop_index((5000.0, 5000.0), 5, 3, CELL, GAP), 4);
        assert_eq!(drop_index((0.0, 0.0), 0, 3, CELL, GAP), 0);
    }

    #[test]
    fn a_press_only_picks_up_a_card_it_actually_landed_on() {
        // Five cards in three columns, so the sixth slot of the second row is empty.
        let at = |x: f64, y: f64| slot_at((x, y), 5, 3, CELL, GAP);
        assert_eq!(at(299.0, 199.0), Some(0), "its far corner");
        assert_eq!(at(299.0, 205.0), None, "one row's gutter below it");
        assert_eq!(at(305.0, 10.0), None, "the gutter between two cards");
        assert_eq!(at(10.0, 205.0), None, "the gutter between two rows");
        assert_eq!(at(700.0, 300.0), None, "the empty tail of the last row");
        assert_eq!(at(5000.0, 10.0), None, "past the last column");
        assert_eq!(at(-10.0, 10.0), None, "the centring margin to the left");
    }

    #[test]
    fn a_press_before_the_first_allocation_picks_up_nothing() {
        assert_eq!(slot_at((10.0, 10.0), 5, 1, (0.0, 0.0), GAP), None);
        assert_eq!(slot_at((10.0, 10.0), 0, 3, CELL, GAP), None);
    }

    #[test]
    fn moving_forwards_pulls_everything_between_back_one() {
        let shifts: Vec<f64> = (0..5).map(|index| shift(index, 1, 3)).collect();
        assert_eq!(shifts, [0.0, 0.0, -1.0, -1.0, 0.0]);
    }

    #[test]
    fn moving_backwards_pushes_everything_between_forward_one() {
        let shifts: Vec<f64> = (0..5).map(|index| shift(index, 3, 1)).collect();
        assert_eq!(shifts, [0.0, 1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_card_dropped_where_it_started_moves_nothing() {
        let shifts: Vec<f64> = (0..5).map(|index| shift(index, 2, 2)).collect();
        assert_eq!(shifts, [0.0; 5]);
    }

    #[test]
    fn a_card_in_motion_is_painted_over_the_cards_standing_still() {
        // Card 1 is the one being carried, and card 3 is travelling to a slot on another
        // row. Cards 4 and 5 are standing still with higher indices, which is what used to
        // put them over the top of card 3.
        let motion = [false, false, true, true, false, false];
        assert_eq!(paint_order(&motion, Some(1)), [0, 4, 5, 2, 3, 1]);
    }

    #[test]
    fn nothing_moving_leaves_the_cards_in_slot_order() {
        assert_eq!(paint_order(&[false; 4], None), [0, 1, 2, 3]);
    }

    #[test]
    fn a_carried_card_that_the_daemon_removed_underneath_the_drag_is_not_painted_twice() {
        // `slots` can be replaced while a drag is in flight, and an index that is no longer
        // in it would otherwise be appended to an order it is not a member of.
        assert_eq!(paint_order(&[false, true], Some(7)), [0, 1]);
    }

    #[test]
    fn the_stylesheet_lifts_the_widget_the_drag_actually_marks() {
        // The class goes on the slot, and the rule that makes a carried card opaque has to
        // be written against the slot too. It was written against the card once, matched
        // nothing, and the only symptom was a translucent card mid-drag — which no test
        // reaches and no warning reports.
        let selector = format!(".{SLOT_CLASS}.{DRAGGING_CLASS} >");
        assert!(
            crate::style::STYLE.contains(&selector),
            "the stylesheet has to select {selector}"
        );
    }

    #[test]
    fn the_stylesheet_makes_every_card_opaque_while_one_is_moving() {
        // Same trap, one level up: this class goes on the grid, and a rule scoped to the
        // slot instead would leave the cards a drag displaces translucent, which is a bug
        // report and not a test failure.
        let selector = format!(".{GRID_CLASS}.{REORDERING_CLASS} > .{SLOT_CLASS} >");
        assert!(
            crate::style::STYLE.contains(&selector),
            "the stylesheet has to select {selector}"
        );
    }

    #[test]
    fn the_block_of_columns_is_centred_in_the_allocation() {
        assert_eq!(content_left(1000, 3, 300.0), 38.0);
        assert_eq!(content_left(312, 1, 300.0), 6.0);
        assert_eq!(
            content_left(200, 1, 300.0),
            0.0,
            "a squeezed card starts at the edge rather than off it"
        );
    }

    #[test]
    fn the_view_only_follows_a_pointer_in_one_of_the_two_edge_bands() {
        assert_eq!(autoscroll_rate(300.0, 600.0), 0.0, "the middle is still");
        assert!(autoscroll_rate(10.0, 600.0) < 0.0, "near the top, upwards");
        assert!(autoscroll_rate(590.0, 600.0) > 0.0, "near the bottom, down");
        assert_eq!(
            autoscroll_rate(0.0, 600.0),
            -AUTOSCROLL_RATE,
            "the very edge is the fastest it goes"
        );
        assert_eq!(
            autoscroll_rate(10.0, 80.0),
            0.0,
            "a viewport with no middle never scrolls itself"
        );
    }
}
