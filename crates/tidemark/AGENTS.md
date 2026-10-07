# DESKTOP CLIENT KNOWLEDGE BASE

## OVERVIEW
Slint presentation and desktop lifecycle, an IPC client of `tidemarkd`. Markup in `ui/`, compiled by `build.rs`; Rust in `src/` owns state and talks D-Bus.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Application entry | `src/main.rs` | Renderer choice, single instance, `--background` |
| Single instance (Linux) | `src/application.rs` | Session-bus name and `org.freedesktop.Application` |
| Main coordination | `src/window.rs` | Card model, updates, reorder, card menu, dialog slots |
| Daemon protocol and reconnect | `src/bus.rs` | Generated `DaemonProxy`, `Update`, name watching, Windows p2p |
| Provider management | `src/provider_settings/` | Dialog controller, list, detail, browser auth, pure model |
| Alerts | `src/alert.rs`, `ui/alert.slint` | Questions awaited as futures |
| Card content | `src/view.rs`, `ui/card.slint` | Pure status-to-card decisions; markup only draws |
| Grid, drag and window | `ui/app.slint` | Slot geometry, drag, settle, header bar |
| Widgets and palette | `ui/adw.slint`, `ui/theme.slint` | libadwaita metrics and colours |
| Pure presentation | `src/model.rs`, `src/format.rs` | Ordering, catalog titles, chips, relative times |
| Marks and desktop style | `src/marks.rs`, `src/portal.rs`, `src/registry.rs` | Provider SVG lookup; XDG portal / Windows registry |
| Windows lifetime | `src/daemon_job.rs`, `src/single_instance.rs`, `src/frame.rs`, `src/file_log.rs` | Audited `unsafe` islands |
| Development data source | `examples/mock-daemon.rs` | Real zbus service publishing shared types |

## CONVENTIONS
- State uses `Rc`/`Weak`, `Cell`/`RefCell`; futures run on Slint's event loop (`window::spawn`). zbus interface methods run on zbus's thread and reach the window only through `upgrade_in_event_loop`.
- Persistent `VecModel`s are kept in step row by row (`sync`), so unchanged rows keep their animation state.
- Decisions are pure Rust with colocated tests; `.slint` files draw what Rust hands them.
- Optimistic edits roll back on refusal; polling must preserve active edits and in-progress login pages.
- FemtoVG renders rounded clips and translucent items with children through an off-grid layer, which blurs text: no `clip` with a radius, and fade with `transparentize` rather than `opacity`.

## ANTI-PATTERNS
- Do not infer authentication capabilities from provider/browser names or inspect credential files in the client.
- Do not interpret daemon strings as markup or use colour as the only status explanation.
- Do not coerce unknown server choices into known ones; keep them visible and disabled.
- Do not search PATH for daemon spawning / Windows restart.

## CHECKS
- `cargo test -p tidemark` covers colocated model, view and state-machine tests.
- Manual GUI data: `systemctl --user stop tidemarkd`, then `cargo run -p tidemark --example mock-daemon`, and `cargo run -p tidemark` in the same session.
- Package asset lists in this crate's Cargo metadata mirror `PKGBUILD`; package both daemon and GUI from a prior workspace release build.
