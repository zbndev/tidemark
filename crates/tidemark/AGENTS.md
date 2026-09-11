# DESKTOP CLIENT KNOWLEDGE BASE

## OVERVIEW
GTK/libadwaita presentation and desktop lifecycle; score 12, a distinct IPC-client domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Application entry | `src/main.rs` | Module wiring and platform startup |
| Main coordination | `src/window.rs` | Cards, dialog slots, updates, reorder, tray |
| Daemon protocol and reconnect | `src/bus.rs` | Generated `DaemonProxy`, `Update`, name watching |
| Provider management | `src/provider_settings/` | Dialog controller, list, detail, browser auth, pure model |
| General preferences | `src/preferences.rs` | Authoritative/displayed/suppress state machines |
| Card layout and drag | `src/grid.rs` | Custom `CardGrid`, geometry and animations |
| Quota rendering | `src/card.rs`, `src/bar.rs` | Incremental card updates, pure bar geometry |
| History detail | `src/detail.rs`, `src/chart.rs` | Async history selection and pure chart geometry |
| Pure presentation | `src/model.rs`, `src/format.rs` | Ordering, catalog titles, chips, relative times |
| Release notes preview | `src/release_notes.rs`, `src/markdown.rs`, `src/update.rs` | Changelog dialog, Markdown to Pango markup, release URL |
| Theme and marks | `src/style.rs`, `src/theme.rs`, `src/mark.rs` | Semantic CSS and optional provider icons |
| Tray integration | `src/tray.rs` | Shared model with ksni / Windows backends |
| Windows lifetime and fonts | `src/daemon_job.rs`, `src/single_instance.rs`, `src/font.rs` | Safe ownership around platform APIs |
| Development data source | `examples/mock-daemon.rs` | Real zbus service publishing shared types |

## CONVENTIONS
- GTK state uses `Rc`/`Weak`, `Cell`/`RefCell`, and local GLib futures; tray threads exchange small channel messages.
- Pure geometry and formatting stay outside widget wiring for deterministic unit coverage.
- Settings keep authoritative and displayed values separate; suppress handlers during server-driven updates.
- Optimistic edits roll back on rejection; polling must preserve active edits and in-progress login pages.
- Browser-auth UI renders daemon-provided modes/candidates; activating a tab is not a complete candidate selection.
- Grid drag animates fractional slot offsets and commits child order on release.
- Card sizing is constrained independently of daemon prose; labels ellipsize or WordChar-wrap.
- Styles use libadwaita semantic classes and theme variables; hover moves the card inside its allocated slot.

## ANTI-PATTERNS
- Do not infer authentication capabilities from provider/browser names or inspect credential files in widgets.
- Do not replace `CardGrid` with FlowBox sorting or detached drag-source icons.
- Do not interpret daemon strings as markup or use color as the only status explanation.
- Do not coerce unknown server choices into known ones; keep them visible and disabled.
- Do not let stale history replies update a newer selection; preserve `RequestGeneration` checks.
- Do not close to tray unless a host accepted the icon; tray failure is nonfatal.
- Do not touch GTK from the tray thread or search PATH for daemon spawning / Windows restart.
- Do not hand-edit `src/tray_icon_rgba.rs`; regenerate pixels when the source icon changes.

## CHECKS
- `cargo test -p tidemark` covers colocated model, geometry, and state-machine tests.
- Manual GUI data: stop the real user service with `systemctl --user stop tidemarkd`, then run `cargo run -p tidemark --example mock-daemon`.
- In another terminal of the same graphical/bus session, run `cargo run -p tidemark`.
- Package asset lists in this crate's Cargo metadata mirror `PKGBUILD`; package both daemon and GUI from a prior workspace release build.
