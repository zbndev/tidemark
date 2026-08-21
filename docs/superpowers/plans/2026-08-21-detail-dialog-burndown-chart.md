# Detail Dialog and Burn-down Chart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open a live, standard detail dialog from each provider card and plot the selected
window's current-segment consumption against its even pace.

**Architecture:** `tidemarkd` queries its owned SQLite history through the engine command
queue and exposes a current-segment point list over D-Bus. The GTK client adds a pure chart
geometry module and an `AdwDialog` controller that loads only the selected window's points,
rejects stale async replies, and receives live `ProviderStatus` updates from `MainWindow`.

**Tech Stack:** Rust 1.92, rusqlite, Tokio, zbus 5, GTK 4.22, libadwaita 1.9, Cairo.

**Spec:** `docs/superpowers/specs/2026-08-21-detail-dialog-burndown-chart-design.md`

## Global Constraints

- Work directly on the current `main` checkout; do not create a branch or a linked
  worktree.
- All documentation, source code, code comments, tests, logs, and interface copy are in
  English.
- The GUI crate must not depend on `tidemark-core`, SQLite, an HTTP client, or a provider.
- The daemon remains the only process that opens `history.db`.
- A missing window length or reset time is normal: never fabricate the even-pace diagonal.
- Every behaviour change is developed red-green-refactor with one focused failing test
  observed before its implementation.

## File Structure

- `crates/tidemark-types/src/wire.rs` — `HistoryPoint`, the D-Bus-safe history value.
- `crates/tidemark-types/src/lib.rs` — re-export the shared point type.
- `crates/tidemark-core/src/storage/mod.rs` — read just a window's open segment.
- `crates/tidemarkd/src/engine.rs` — serialized history command and storage-to-wire mapping.
- `crates/tidemarkd/src/service.rs` — account validation and `CurrentSegment` D-Bus method.
- `crates/tidemark/src/bus.rs` — generated client proxy method.
- `crates/tidemark/src/chart.rs` — pure chart geometry plus Cairo drawing widget.
- `crates/tidemark/src/detail.rs` — dialog state, window selection, live status updates,
  request-generation protection, and presentation.
- `crates/tidemark/src/card.rs` — accessible card activation callback and activatable style.
- `crates/tidemark/src/window.rs` — one dialog slot, card callbacks, and update/removal
  forwarding.
- `crates/tidemark/src/main.rs` — register the two new GUI modules.
- `PLAN.md` — mark step 14 done only after its installed-package verification succeeds.

---

### Task 1: Publish and Read Current-Segment Points

**Files:**

- Modify: `crates/tidemark-types/src/wire.rs`
- Modify: `crates/tidemark-types/src/lib.rs`
- Modify: `crates/tidemark-core/src/storage/mod.rs`

**Interfaces:**

- Produces `pub struct HistoryPoint { pub captured_at: i64, pub used_percent: f64 }` with
  `SerializeDict`, `DeserializeDict`, and `Type`.
- Produces `History::current_points(&self, provider: &str, account: &str, window: &WindowKey)
  -> Result<Vec<Point>, StorageError>`.

- [ ] **Step 1: Add a failing storage test for the open segment only**

  In `storage::tests`, ingest a window at 10% and 80%, roll it to a new segment at 2%, then
  add 20%. Assert that `current_points("test", "default", &key())` returns only `(2%,
  20%)` in capture order. Add a second test asserting an unseen key returns an empty vector.

- [ ] **Step 2: Run the focused storage test and observe the missing method**

  Run: `cargo test -p tidemark-core storage::tests::current_points -- --nocapture`

  Expected: compilation failure because `History::current_points` does not exist.

- [ ] **Step 3: Implement the smallest storage query**

  Add this method beside `points`:

  ```rust
  pub fn current_points(
      &self,
      provider: &str,
      account: &str,
      window: &WindowKey,
  ) -> Result<Vec<Point>, StorageError> {
      let Some(segment) = self.current_segment(provider, account, window)? else {
          return Ok(Vec::new());
      };
      self.points(provider, account, window, segment)
  }
  ```

- [ ] **Step 4: Run the focused storage tests**

  Run: `cargo test -p tidemark-core storage::tests::current_points -- --nocapture`

  Expected: both current-segment tests pass.

- [ ] **Step 5: Add the failing shared-wire round-trip test**

  In `wire::tests`, serialize and deserialize:

  ```rust
  let original = HistoryPoint { captured_at: 1_785_700_000, used_percent: 37.5 };
  ```

  Assert the decoded value equals `original`.

- [ ] **Step 6: Run the wire test and observe the missing type**

  Run: `cargo test -p tidemark-types wire::tests::history_point -- --nocapture`

  Expected: compilation failure because `HistoryPoint` is undefined.

- [ ] **Step 7: Add and export the D-Bus dictionary**

  Add to `wire.rs` beside the other published dictionaries:

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, SerializeDict, DeserializeDict, Type)]
  #[zvariant(signature = "a{sv}")]
  pub struct HistoryPoint {
      pub captured_at: i64,
      pub used_percent: f64,
  }
  ```

  Re-export it from `lib.rs` with `ProviderStatus` and `WindowStatus`.

- [ ] **Step 8: Run the affected crate tests and commit**

  Run: `cargo test -p tidemark-core -p tidemark-types`

  Commit:

  ```bash
  git add crates/tidemark-types/src/wire.rs crates/tidemark-types/src/lib.rs \
      crates/tidemark-core/src/storage/mod.rs
  git commit -m "Expose current history points"
  ```

---

### Task 2: Carry History Through the Daemon Contract

**Files:**

- Modify: `crates/tidemarkd/src/engine.rs`
- Modify: `crates/tidemarkd/src/service.rs`
- Modify: `crates/tidemark/src/bus.rs`

**Interfaces:**

- Consumes `History::current_points` and `HistoryPoint` from Task 1.
- Produces `Daemon::current_segment(provider, account, window) -> Vec<HistoryPoint>` on
  D-Bus and `DaemonProxy::current_segment` in the GUI.
- Adds `Command::CurrentSegment { provider, account, window, reply }`, where `reply` is
  `oneshot::Sender<Result<Vec<HistoryPoint>, String>>`.

- [ ] **Step 1: Add a failing engine command test**

  Extend the engine test harness with a helper that sends `Command::CurrentSegment` and
  awaits its reply. Ingest two segments through an account's fake provider, then assert
  the command returns only the open segment. Also assert an unknown account yields an
  error containing `is not configured`.

- [ ] **Step 2: Run the focused engine test and observe the missing command**

  Run: `cargo test -p tidemarkd engine::tests::current_segment -- --nocapture`

  Expected: compilation failure because `Command::CurrentSegment` is absent.

- [ ] **Step 3: Add the serialized engine query**

  Add the command variant and a private engine method that first finds the exact
  `provider/account` in `self.accounts`, then calls `self.history.current_points` with
  `WindowKey::named(window)`. Map each stored `Point` to:

  ```rust
  HistoryPoint {
      captured_at: point.captured_at.as_unix(),
      used_percent: point.used_percent,
  }
  ```

  Route the variant in `Engine::run`, sending its result through the reply without polling
  before answering it.

- [ ] **Step 4: Run the engine test to green**

  Run: `cargo test -p tidemarkd engine::tests::current_segment -- --nocapture`

  Expected: current segment and unknown-account assertions pass.

- [ ] **Step 5: Add failing service and proxy contract tests**

  Add a service test proving an unconfigured pair fails before a command is sent. Add a
  `bus.rs` compile-level test by calling `DaemonProxy::current_segment` in the mock-daemon
  integration path; it must return `Vec<HistoryPoint>`.

- [ ] **Step 6: Run the focused service test and observe the missing method**

  Run: `cargo test -p tidemarkd service::tests::current_segment -- --nocapture`

  Expected: compilation failure because the D-Bus method and proxy declaration are absent.

- [ ] **Step 7: Implement the D-Bus method and proxy declaration**

  Add to both interface traits:

  ```rust
  fn current_segment(
      &self,
      provider: &str,
      account: &str,
      window: &str,
  ) -> zbus::Result<Vec<HistoryPoint>>;
  ```

  In the service implementation, call `self.account(provider, account).await?`, send the
  command, await the reply, map channel failure to `fdo::Error::Failed`, and map the engine
  error string to the same error. Do not inspect the published window list: an absent
  window is a valid empty history query.

- [ ] **Step 8: Run daemon and client tests, then commit**

  Run: `cargo test -p tidemarkd && cargo test -p tidemark`

  Commit:

  ```bash
  git add crates/tidemarkd/src/engine.rs crates/tidemarkd/src/service.rs crates/tidemark/src/bus.rs
  git commit -m "Serve current segment history over D-Bus"
  ```

---

### Task 3: Build and Test Pure Burn-down Geometry

**Files:**

- Create: `crates/tidemark/src/chart.rs`
- Modify: `crates/tidemark/src/main.rs`

**Interfaces:**

- Consumes `HistoryPoint` and `WindowStatus` only.
- Produces `pub fn geometry(window: &WindowStatus, points: &[HistoryPoint], width: f64,
  height: f64) -> Geometry` and `pub struct Chart` with `set_loading`, `set_error`, and
  `set_data` for the later dialog.

- [ ] **Step 1: Write failing geometry tests**

  Test a 100×100 scheduled five-hour window with reset `18_000`, points at `0/0%` and
  `9_000/50%`: the diagonal is `(0,100)` to `(100,0)` and actual points share those
  coordinates. Add tests that:

  - missing `length_secs` or `resets_at` yields no diagonal but preserves actual points;
  - a one-point series yields `marker: Some` and no invented second actual point;
  - timestamps before start or after reset are clamped into the plot;
  - unordered input becomes chronological output.

- [ ] **Step 2: Run the chart test and observe the missing module**

  Run: `cargo test -p tidemark chart::tests -- --nocapture`

  Expected: compilation failure because `chart` is not declared.

- [ ] **Step 3: Implement geometry without GTK state**

  Define `Coord { x: f64, y: f64 }` and `Geometry { actual: Vec<Coord>, diagonal:
  Option<[Coord; 2]>, marker: Option<Coord>, schedule_available: bool }`. Sort a local copy
  by `captured_at`, clamp y from 0..100%, and calculate the horizontal domain from
  `reset - length..reset` only when both fields are present. Otherwise use the earliest and
  latest actual timestamp; a zero span centers the marker. Never create an actual point.

- [ ] **Step 4: Run the geometry test suite to green**

  Run: `cargo test -p tidemark chart::tests -- --nocapture`

  Expected: all deterministic geometry cases pass without a display server.

- [ ] **Step 5: Add the drawing widget after geometry is green**

  Implement `Chart` as a `gtk::DrawingArea` whose state is `Loading`, `Error(String)`, or
  `Data(WindowStatus, Vec<HistoryPoint>)`. Its draw function calls `geometry`, paints axes,
  actual line/marker, optional subdued diagonal, and appropriate text states. Use the
  widget's theme colours; add no hard-coded palette. Export `pub fn widget(&self) ->
  &gtk::DrawingArea` and queue a redraw on every setter.

- [ ] **Step 6: Register the module and run GUI tests**

  Add `mod chart;` in `main.rs` and run:

  `cargo test -p tidemark`

- [ ] **Step 7: Commit the isolated chart boundary**

  ```bash
  git add crates/tidemark/src/chart.rs crates/tidemark/src/main.rs
  git commit -m "Draw burn-down chart geometry"
  ```

---

### Task 4: Present the Live Detail Dialog from a Card

**Files:**

- Create: `crates/tidemark/src/detail.rs`
- Modify: `crates/tidemark/src/card.rs`
- Modify: `crates/tidemark/src/window.rs`
- Modify: `crates/tidemark/src/main.rs`

**Interfaces:**

- Consumes `DaemonProxy::current_segment`, `Chart`, and `ProviderStatus`.
- Produces `DetailDialog::present(parent, proxy, status, on_closed) -> Rc<DetailDialog>`,
  `DetailDialog::apply(&self, status: &ProviderStatus)`, and `DetailDialog::close(&self)`.
- Changes `Card::new` to receive a card-identity activation callback.

- [ ] **Step 1: Add failing pure selection tests**

  In `detail.rs`, test helpers must select the dominant window initially, preserve the
  selected key after a status update that still includes it, and fall back to the new
  dominant key when it disappears. A no-reading status must have no selection.

- [ ] **Step 2: Run the focused tests and observe the missing detail module**

  Run: `cargo test -p tidemark detail::tests:: -- --nocapture`

  Expected: compilation failure because `detail` is not declared.

- [ ] **Step 3: Implement selection model and dialog layout**

  Define a small pure `Selection(Option<String>)` before widget code. `DetailDialog` builds
  an `adw::Dialog` (`content_width(720)`) containing a closeable header, a scroll view,
  the `Quota windows` rows, `Chart`, and nonempty `DetailSection` groups. The window rows
  invoke `select_window(key)`. First selection and every selection change call
  `load_current_segment`; a status with no selected window leaves `Chart` in an explanatory
  empty state and makes no D-Bus call.

- [ ] **Step 4: Protect asynchronous requests and test it**

  Increment a `Cell<u64>` generation before each request. Capture the generation and key in
  `glib::spawn_future_local`; apply a successful or failed result only when both still
  match the active dialog. Unit-test the predicate with an old generation and a changed
  key, then run:

  `cargo test -p tidemark detail::tests -- --nocapture`

- [ ] **Step 5: Add failing card activation and dialog-slot tests**

  In `card.rs`, test that the activation callback receives the card's original
  `provider/account`. In `window.rs`, add to `DialogSlot` tests for a second insert after
  `closed` and a close when the open account is removed. The tests must not create a GTK
  display.

- [ ] **Step 6: Make cards activatable and wire `MainWindow` ownership**

  Add `activatable` to the card root classes and give its `FlowBoxChild` an accessible
  click/keyboard activation path. The callback calls only `MainWindow::open_detail(provider,
  account)`. Add `detail_dialog: DialogSlot<DetailDialog>` to `MainWindow`; build cards
  through one helper so initial and later cards use the same callback. `show_one` forwards
  changed status to a matching dialog. `show_removed` closes and clears a matching dialog
  before dropping its card. The dialog's `closed` signal clears the slot.

- [ ] **Step 7: Run the focused GUI suite**

  Run: `cargo test -p tidemark card::tests window::tests detail::tests -- --nocapture`

  Expected: all selection, generation, activation, and lifetime tests pass.

- [ ] **Step 8: Commit the complete UI interaction**

  ```bash
  git add crates/tidemark/src/detail.rs crates/tidemark/src/card.rs \
      crates/tidemark/src/window.rs crates/tidemark/src/main.rs
  git commit -m "Open quota details from provider cards"
  ```

---

### Task 5: Verify the Installed Step and Record It

**Files:**

- Modify: `PLAN.md`

- [ ] **Step 1: Run repository-wide automated verification**

  ```bash
  cargo fmt --check
  cargo test --workspace
  cargo clippy --workspace -- -D warnings
  scripts/check-layering.sh
  ```

  Expected: every command succeeds. If a failure is older than this step, establish it on a
  clean `main` commit, record the exact failing tests under the Step 14 log, and do not
  weaken or skip them.

- [ ] **Step 2: Build and install the working tree**

  ```bash
  makepkg -sif
  systemctl --user restart tidemarkd.service
  ```

  Expected: the installed GUI opens, a card opens the dimmed detail dialog, all windows and
  provider details appear, changing the selected window loads its current segment, and the
  installed daemon continues publishing updates.

- [ ] **Step 3: Record the outcome and commit documentation**

  Change Step 14 in `PLAN.md` from `todo` to `done`, add the exact verification result and
  any pre-existing test failure under its `Log`, then commit:

  ```bash
  git add PLAN.md docs/superpowers/specs/2026-08-21-detail-dialog-burndown-chart-design.md \
      docs/superpowers/plans/2026-08-21-detail-dialog-burndown-chart.md
  git commit -m "Document quota detail dialog"
  ```
