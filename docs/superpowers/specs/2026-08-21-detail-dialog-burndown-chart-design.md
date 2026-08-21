# Detail Dialog and Burn-down Chart Design

- Status: approved
- Date: 2026-08-21
- Implements: implementation step 14

## Purpose

Clicking a provider card must answer the questions a compact card cannot: every quota
window the provider reported, provider-specific detail values, and whether consumption in
the active segment is ahead of or behind an even pace to reset. The detail view is a
standard dialog layered over the main window; background blur is explicitly out of scope.

All documentation, source code, code comments, tests, logs, and interface copy are
written in English.

## Goals

- Make every provider card an accessible, keyboard-activatable control that opens one
  detail dialog for that account.
- Show every currently reported quota window and every provider `DetailSection` from the
  last good status.
- Let the reader choose any current window to inspect in the burn-down chart. The dominant
  (shortest present) window is selected initially.
- Draw the actual stored readings for the selected window's current segment against the
  evenly paced diagonal when the provider supplied both window duration and reset time.
- Keep the GUI free of SQLite and provider/network dependencies. It obtains chart points
  from `tidemarkd` over D-Bus.
- Keep an already-open dialog current when `ProviderChanged` arrives, and close it when
  its account is removed.

## Non-goals

- Forecasting exhaustion, calibration, or forecast-based notifications.
- Browsing completed segments or combining segments into a historical chart.
- Fabricating a reset, duration, or pace line when a provider did not report one.
- Background blur or other compositor-dependent presentation effects.

## Chosen Architecture

`tidemarkd` remains the only process that reads `history.db`. A narrow D-Bus method asks
for the selected window's **current** segment and returns serializable history points; the
method is routed through the engine command queue, which owns the `History` connection.
The query returns an empty vector when that window has no stored current segment. It
returns an argument error for an unconfigured account, but a temporarily absent window is
not an error: a running provider can legitimately change the set it reports.

The GUI adds a focused `detail` module rather than growing `card.rs` or `window.rs` into
another controller. `DetailDialog` owns its `AdwDialog`, status, selected window key,
chart drawing area, and monotonically increasing request generation. An asynchronous
history reply is applied only when its generation and selected key still match, so a slow
reply cannot overwrite a more recent selection or status update.

`MainWindow` retains the dialog in its existing `DialogSlot` pattern. It opens the dialog
on card activation, forwards the affected `ProviderStatus` on live updates, and closes the
dialog before removing an account. The dialog's `closed` signal clears the slot, allowing a
fresh open and preventing stacked modal views.

## D-Bus and Storage Contract

The shared types crate gains this forward-compatible dictionary:

```rust
pub struct HistoryPoint {
    pub captured_at: i64,
    pub used_percent: f64,
}
```

`captured_at` is Unix seconds. `used_percent` is the stored value in the same 0..=100
domain the card already displays. A point does not carry its historical `resets_at`: the
chart's schedule belongs to the window currently published in `ProviderStatus`, and the
stored reset value is not needed to draw actual consumption.

The D-Bus method is:

```text
CurrentSegment(provider: &str, account: &str, window: &str) -> Vec<HistoryPoint>
```

The client proxy calls it with the selected `WindowStatus.key`. The service verifies the
account, sends `Command::CurrentSegment { provider, account, window, reply }` to the
engine, and waits for the one-shot answer. The engine validates that the account exists,
then calls `History::current_points`. This storage helper reads the current segment number
from `window_state`; when there is none it returns an empty vector, otherwise it maps the
existing oldest-first `Point` rows into history points. No client sees the database path or
the SQLite driver.

## Dialog Interface

The detail is an `AdwDialog` with the normal dimmed presentation supplied by libadwaita,
`content_width` 720, and a vertically scrollable `AdwToolbarView`. Its header has the
provider's monochrome mark, name, and Close button.

The body contains, in order:

1. A `Quota windows` group with one row per current `WindowStatus`, using the provider's
   title, formatted used percent, and reset copy when known. The selected row is an
   accessible control and drives the chart; the dominant window is initially selected.
2. A `Burn-down` group with a legend: `Actual` and, only when schedulable, `Even pace`.
   The chart is a `GtkDrawingArea` and has an explicit empty state while the request is
   pending, no points exist yet, or no schedule is available for the diagonal.
3. One `AdwPreferencesGroup` per provider `DetailSection`, containing its rows in received
   order. Empty detail sections are omitted.

The dialog must still show real points if no reset or length is available. In that case it
uses the first and latest real capture times for the horizontal plotting range and labels
the chart `Schedule unavailable`; it simply omits the diagonal. A one-point segment draws a
visible marker rather than inventing a slope. When both reset and duration are available,
the x-axis is exactly `[resets_at - length_secs, resets_at]`, the diagonal joins `(start,
0%)` to `(reset, 100%)`, and point coordinates are clamped to the plot rectangle. This
makes the comparison mathematical rather than decorative.

No rate-limit state changes are made by this dialog. Failed chart requests leave the rest
of the detail visible and replace only the chart with a concise error message.

## Card Activation and Styling

`Card` gets a `gtk::GestureClick` and keyboard activation on its `GtkFlowBoxChild`; its
callback contains only `(provider, account)`, leaving dialog ownership in `MainWindow`.
The card root adds libadwaita's `activatable` class, while the existing transform/shadow
remains attached to the flow-box child. That preserves the platform hover and pressed
states without reintroducing the square hover tint.

## Chart Model and Rendering

The pure chart module owns all coordinate calculations. Its input is a selected
`WindowStatus`, the returned `HistoryPoint` values, and the pixel plot rectangle. Its
output contains a clamped actual polyline, optional even-pace diagonal, optional marker,
and semantic state (`Loading`, `Empty`, `ScheduleUnavailable`, or `Ready`). GTK drawing is
only a consumer of this output.

The y-axis is always 0..100%, so an exhausted and a lightly used window remain comparable.
The actual series is chronological and never interpolates absent polls. The line joins
stored readings solely to make the observed progression legible; storage's deliberate gaps
from suspend remain gaps in the data model and are not synthesized as measurements.

## Error Handling and Live Updates

- An account with no snapshot has no selectable windows; the dialog explains that no quota
  reading is available yet and does not issue a history query.
- A selected window disappearing in a later status selects the new dominant window, or
  shows the no-window state when none remain.
- A dialog owned by a removed account closes immediately; removal never opens a chart query
  against a deleted account.
- Old, failed, or out-of-order history replies cannot overwrite newer chart state.
- Provider failure states preserve the last good status exactly as the card does; the
  detail displays that last good data alongside its existing state chip/copy.

## Test Strategy

Implementation follows red-green-refactor. Automated tests cover:

- `History::current_points` returning only the open segment and an empty vector for an
  unseen window;
- `HistoryPoint` D-Bus serialization and the service/engine command path;
- chart geometry for a normal scheduled segment, missing schedule, one point, timestamps
  outside the known window, and chronological actual points;
- dominant-window initial selection and selection fallback after a live status update;
- stale asynchronous chart generations being ignored;
- dialog slot lifetime and card activation without requiring a network or SQLite dependency
  in the GUI crate.

Final verification runs focused tests in each red-green cycle, then `cargo test
--workspace`, `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
`scripts/check-layering.sh`, `makepkg -sif`, and `systemctl --user restart tidemarkd`.
