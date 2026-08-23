# Card Reordering Design

- Status: approved
- Date: 2026-08-23
- Implements: implementation step 15

## Purpose

The grid of provider cards must be in the order the user put it in, changed by dragging a
card to where they want it, and remembered across restarts. The order the grid shows is
the order every other list of accounts uses.

All documentation, source code, code comments, tests, logs, and interface copy are written
in English.

## Goals

- Drag a card with the pointer and drop it anywhere in the grid.
- **Live reflow.** While a card is held between two others, the cards it displaced move out
  of its way and stay moved, and they move back if the pointer changes its mind — before
  the button is released, not after it.
- The order the user set is never changed by anything else. A new account is appended.
- One order, one place: the grid, the tray menu and the configured list in provider
  settings all read it.
- Persisted by the daemon, in `config.toml`, and republished to every client.

## Non-goals

- Sorting by urgency, or any other automatic order. It is deleted, not kept as a mode.
- Keyboard-driven reordering. The cards are focusable and activatable by keyboard already;
  moving one without a pointer is a separate, additive change.
- Dragging a card out of the window, onto another window, or onto anything but the grid.
- Touch-screen reordering. The gesture takes the primary button; a touch drag inside a
  scrollable view is the scroll gesture's, and taking it from there is its own decision.

## What upstream actually provides

Measured against GTK 4.22.4 and libadwaita 1.9.0 rather than assumed:

- **There is no reorder API.** `GtkFlowBox`, `GtkListView` and `GtkGridView` expose sorting
  and selection and nothing that moves a child by drag. `GtkNotebook` has
  `gtk_notebook_set_tab_reorderable()` and `AdwTabView` has `adw_tab_view_reorder_page()`,
  and both are tab-shaped and private about how they animate.
- **`GtkFlowBox` cannot do the live part at all.** `gtk_flow_box_invalidate_sort()` sorts
  its sequence and calls `gtk_widget_queue_resize()`, so a card that loses its place
  teleports to the new one. There is no position to interpolate, because the position is
  the allocation and the allocation is computed once per layout pass.
- **`GtkDragSource` / `GtkDropTarget` are the wrong controllers for this.** They carry a
  payload between widgets and draw a detached drag icon; the icon is not in the grid, so it
  cannot push anything. They are what a *cross-window* drag would need, which this is not.
- **The thing libadwaita does have is `AdwTabGrid`** — private, and an exact match for the
  problem: a two-dimensional grid whose items reorder with live animated reflow. Its
  architecture, which this design copies:

  | `AdwTabGrid` | here |
  |---|---|
  | `GtkGestureDrag` on the container | the same |
  | per-item `reorder_offset` / `end_reorder_offset`, in **index units** | the same |
  | per-item `AdwTimedAnimation`, restarted from the current value | the same |
  | the animation callback calls `gtk_widget_queue_allocate()` | the same |
  | `size_allocate` maps a fractional index to a position and calls `gtk_widget_allocate()` with a translation | the same |
  | `adw_tab_view_reorder_page()` only from `end_drag_reodering()` | `SetOrder` only on release |

  Copying the pattern rather than the type: `adw-tab-grid-private.h` refuses inclusion from
  outside libadwaita, and neither gtk4-rs nor libadwaita-rs binds it.

Restarting each animation *from its current value* is what makes "the user changed their
mind" work. A card half-way out of the way is retargeted to zero and continues from where
it is, rather than jumping to its old slot and starting again.

## Order is the `providers` array

`config.toml` already carries the ordered array `CONTEXT.md` § Storage promised card order
would live beside:

```toml
providers = ["claude", "codex", "zai"]
```

**That array is the card order.** Not a second key next to it. `providers` is already
ordered, already the user's set, already the order `GetStatus` publishes in, and
`AddProvider` already appends to it — which is exactly what "new cards go on the end" means.
A separate `card_order` array would be a second list to keep in agreement with the first,
and the first question it raises — what the order of a provider present in one and absent
from the other is — has no good answer.

The persisted identity is the provider slug, because that is the only identity this file
has: v1 configures one `default` account per provider and `config.toml` cannot express a
second one. When multi-account arrives it changes the shape of this array, and this method
with it; inventing a `(provider, account)` order now would be a wire format claiming a
precision the storage does not have.

### `Config::set_provider_order`

Permutes the existing array in place rather than writing a new one, like every other edit
this file gets. **Only the values move; the decoration stays where it was written.**

Carrying decoration along with each value was tried first and is wrong, because a TOML array
has no notion of a comment belonging to an element. Both of these are ordinary:

```toml
providers = [
    # the one I actually use
    "claude",
    "zai",       # the other one
]
```

In the first style the comment is part of `claude`'s own prefix; in the second — which is
the style this file's existing tests use — the comment after `"zai",` is part of the *next*
element's prefix. So decoration that travels with the value scrambles comments and
indentation in opposite directions depending on which style the file happens to use, and a
single-line array additionally loses its spacing, because the first element's prefix is
empty and the rest have a space: `["claude", "zai"]` becomes `[ "zai","claude"]`.

Writing the slugs into the existing slots has none of those failure modes. `toml_edit`'s
`Array::replace` keeps the slot's decoration by design, so the file comes back byte for byte
apart from the strings: a multi-line array stays multi-line and indented, an inline one stays
inline, and every comment stays on the line somebody wrote it on. The cost is stated rather
than hidden: a comment naming a provider annotates the position afterwards, not the
provider. That is the lesser evil against mangling the layout of a file a person edits, and
it is the only behaviour of the three that is predictable from the outside.

The order given must be a permutation of what is in the file. Anything else — an unknown
slug, a missing one, a duplicate — is rejected without writing, because a client that has
raced a removal should re-read rather than have the daemon guess which of the two it meant.

## D-Bus

One method and one signal.

```text
SetOrder(providers: as) -> ()
OrderChanged(providers: as)
```

`SetOrder` follows the `SetWindowNotify` path exactly: validate against the configured
accounts, then `config_request(Command::SetOrder { .. })` so the write happens on the
engine's serial queue and the caller does not return until it is persisted.

`OrderChanged` is a signal of its own rather than a burst of `ProviderChanged`, because
`ProviderChanged` carries a status and a status has no position in it. A client holding
cards needs to be told the sequence; it does not need to be told the readings again.

`GetStatus` keeps returning accounts in the published order, so the daemon reorders its own
`Published` vector as part of the same publication — a client that connects one message
later must not see the old sequence. The engine reorders its `accounts` vector too, so a
restart and a live reorder produce the same sequence.

## The grid widget

`gtk::FlowBox` is replaced by `CardGrid`, a `gtk::Widget` subclass in
`crates/tidemark/src/grid.rs`. This is the first GObject subclass in the interface crate,
and it is warranted: the drag state and the layout are one thing, because the position of
every card is a function of the drag.

The layout it computes is the layout the `FlowBox` was configured to produce, so the window
looks the same when nothing is being dragged:

- Cell width is the widest card's natural width; every cell gets it, so footers line up.
- One to three columns, the most that fit the allocated width, spacing 12.
- The block of columns is centred in the allocation. A filled row and a half-empty one sit
  under the middle of the window rather than hugging its left edge.
- The last row is left ragged.
- Rows are the tallest card's natural height at that cell width.

Four pure functions carry all of the arithmetic, so the parts that can be wrong are tested
without a display, the way `bar::geometry` is:

```rust
/// How many columns fit, between one and `max`.
fn columns(available: i32, cell: i32, spacing: i32, max: usize) -> usize;

/// Where slot `index` starts. Fractional indices interpolate, including across the end of
/// a row, so a card sliding from the end of one row to the start of the next travels there
/// instead of jumping.
fn slot(index: f64, columns: usize, cell: (f64, f64), spacing: (f64, f64)) -> (f64, f64);

/// The slot a dragged card's centre is over.
fn drop_index(centre: (f64, f64), count: usize, columns: usize, cell: (f64, f64),
              spacing: (f64, f64)) -> usize;

/// The index shift a card at `index` takes while `from` is being carried to `to`.
fn shift(index: usize, from: usize, to: usize) -> f64;

/// The card a press actually landed on, if it landed on one at all.
fn slot_at(point: (f64, f64), count: usize, columns: usize, cell: (f64, f64),
           spacing: (f64, f64)) -> Option<usize>;
```

`drop_index` is compared against the **canonical** slots — where the cards would be if
nothing were being dragged — not against where they currently are. Canonical slots do not
move, so the target index is a monotone function of pointer position and cannot oscillate
between two values while the pointer is still.

### The drag

A `GtkGestureDrag` on the grid, primary button, exclusive, default (bubble) phase.

- `drag-begin` records which slot the press landed on, and nothing else. `slot_at` and not
  `drop_index`: the latter answers "which slot is this nearest to", which is what a drag in
  progress wants and what a press must never use. Five cards in three columns leave a sixth
  slot empty, and the gutters and the centring margin are not on a card either; answering
  "the nearest one" there picks a card up off a click on the background.
- `drag-update` does nothing until `gtk_drag_check_threshold` passes. It then claims the
  sequence — which cancels the pressed card's `GtkGestureClick`, so a drag never opens the
  detail dialog — moves the dragged slot to the end of the child list so that it both paints
  and picks above its siblings, and starts following the pointer, clamped to the grid.
  Every further update recomputes `drop_index`; when it changes, every card between the
  source and the target is retargeted to ±1 and animated there over 250 ms with
  `ADW_EASE`.
- `drag-end` commits: the slot vector is permuted, every offset is retargeted to zero, and
  the released card is animated from where the pointer left it to its new slot. The order
  goes to the daemon once, here, not on every motion event.

The dragged card is *the card*, not a drag icon: it is still a child of the grid, allocated
at the pointer, which is what lets its siblings react to where it is.

**Autoscroll is part of this, not a refinement.** Six cards in a two-column window are
taller than the window, and the pointer cannot leave the viewport, so without it the last
row is unreachable. While the pointer is within 48 px of the top or bottom of the enclosing
`GtkScrolledWindow`, a tick callback advances its vertical adjustment at a rate
proportional to how far in it is, and the same distance is added to the dragged card's
position so that the card stays under the pointer.

### The card's own widget

The card's `GtkFlowBoxChild` becomes an `AdwBin` with the class `quota-slot`. It keeps the
job the flow box's child had — it is the thing the grid allocates, the thing that takes
focus, and the thing the click and key controllers are on — and loses the one that needed
working around, which is that `GtkFlowBoxChild` tints its own square allocation behind a
card with rounded corners.

The hover stays matched on the slot and applied to the card inside it, for the reason it
always was: a CSS `transform` moves what GTK picks, so a card that lifted itself out from
under the pointer would flicker. The slot's allocation does not move, so it does not.

**The carried card is opaque, and this is not cosmetic.** `.card` takes `@card_bg_color`,
which in the dark style is 8% white over whatever is behind it — right for a card lying on
the window, and wrong for one crossing its neighbours, which then read straight through it.
The dragged card takes `@popover_bg_color`, the platform's own name for a surface floating
above the content, plus a deeper shadow. Its foreground is left alone: the bar's track and
pace mark inherit the text colour, and changing it would make them shift tone for the length
of a drag.

The class that selects it goes on the **slot**, because the grid holds slots and knows
nothing about what is inside them. A rule written against the card instead matches nothing,
and the only symptom is a translucent card mid-drag — no warning, no failing test, and
nothing a screenshot of a resting grid would show. A test asserts that the stylesheet
selects the class the drag actually sets.

## What stops sorting

`model::urgency` and `model::compare` are deleted, along with the `FlowBox` sort function
they fed. Nothing replaces them: the order is the daemon's sequence, and there is no second
rule that could disagree with it.

- **The grid** holds its cards in that sequence and appends a new one.
- **The tray menu** stops sorting and lists what it is given, which is the same sequence.
  It keeps `needs_attention`, which is a threshold rather than an order.
- **Provider settings** already draws its configured list in the sequence it is handed.
  It now gets the visible one, because the window's card vector *is* the visible order.
- **The add picker** is unaffected: it lists catalog entries that have no card, and a card
  order says nothing about them.

## Failure

A rejected `SetOrder` leaves the daemon's order as it was, so the window re-reads
`GetStatus` and returns the cards to it. The cards have already moved by then — the drop
is applied locally first, because a grid that waited for a round trip before showing the
result of a drag would feel broken on a loaded machine.

## Verification

- `columns`, `slot`, `drop_index`, `slot_at` and `shift` are unit-tested, including the wrap
  between rows, the clamp past the last card, and every kind of press that is not on a card:
  a gutter, the centring margin, and the empty tail of the last row.
- `Config::set_provider_order` is tested for the permutation, for the file coming back byte
  for byte apart from the slugs in both the multi-line-with-comments and the inline shape,
  for normalising duplicates first, for an unchanged order not being a write, and for a
  non-permutation being refused with nothing written.
- `Published::reorder` is tested for the sequence and for leaving an account it was not told
  about alone; `Engine::set_order` for persisting and moving its own account vector with it.
- `Daemon::set_order` is tested for refusing a bad order before it reaches the engine, and
  for waiting on persistence before it answers.
- `model::arrangement` is tested for unnamed accounts keeping their relative position.
- The stylesheet is tested against the class the drag sets, because that mismatch has no
  other symptom than a translucent card while the pointer is down.
- Live: dragging cards in one, two and three column widths, confirming the reflow follows
  the pointer and returns, that the carried card is opaque over its neighbours, that the
  order survives a daemon restart, that `config.toml` shows it, and that the tray menu and
  the provider-settings list agree with the grid.
