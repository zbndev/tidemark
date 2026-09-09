# PROJECT KNOWLEDGE BASE

**Generated:** 2026-09-07T19:52:14+03:00
**Commit:** 90d4531
**Branch:** main

## OVERVIEW
Tidemark tracks AI-provider quota windows and pace. Six Rust crates separate daemon state, provider I/O, shared vocabulary, the generated D-Bus proxy, GTK presentation, and `tidemarkctl`; Rust edition 2024, MSRV 1.92, GTK 4.22, libadwaita 1.9.

## STRUCTURE
```text
tidemark/
|-- crates/
|   |-- tidemark/        # GTK GUI; consumes IPC, never core
|   |-- tidemark-cli/    # tidemarkctl; D-Bus client, no core/display/Tokio
|   |-- tidemarkd/       # Polling, history, secrets and IPC ownership
|   |-- tidemark-core/   # External I/O and provider implementations
|   |-- tidemark-ipc/    # Single generated D-Bus client proxy
|   `-- tidemark-types/  # Shared domain and wire vocabulary
|-- data/               # Desktop assets, user service, packaging payloads
|-- scripts/            # Layering, integration, packaging and release checks
|-- nix/                # Package and NixOS module
|-- docs/adr/           # Ownership and integration decisions
|-- CONTEXT.md          # Architecture
`-- PLAN.md             # Implementation log
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Add a provider | `crates/tidemark-core/src/providers/`, `crates/tidemarkd/src/registry.rs` | Core implementation plus daemon registration; simple catalog and hand-written descriptors coexist |
| Change IPC vocabulary | `crates/tidemark-types/src/wire.rs` | Keep daemon and GUI compatible |
| Change the D-Bus proxy | `crates/tidemark-ipc/src/lib.rs` | Keep the one generated client contract synchronized with the daemon interface |
| Change the CLI | `crates/tidemark-cli/` | Preserve JSON/Waybar shapes, exit codes, secret input and layering |
| Polling or mutation ordering | `crates/tidemarkd/src/engine.rs`, `service.rs` | Owned state and published mirror |
| GUI or daemon reconnect | `crates/tidemark/src/window.rs`, `bus.rs` | Presentation and IPC client |
| Credentials or browser sources | `crates/tidemark-core/src/oauth_file.rs`, `secrets.rs`, `browser/` | Ownership and explicit source selection matter |
| Charts or notice identity | `crates/tidemark-core/src/storage/` | Segmentation, retention and migrations |
| Installation | `README.md`, `PKGBUILD`, `data/packaging/`, `nix/` | deb/rpm metadata also lives in GUI Cargo manifest |
| Ownership rationale | `CONTEXT.md`, `docs/adr/` | Normative design and binding decisions; dated superpowers plans/specs are historical, not current-code proof |
| Release and CI | `scripts/release.sh`, `.github/workflows/` | Release helper commits, tags and pushes |

## CODE MAP
Declarations/import shapes are reported by subtree analysis; exact numeric reference counts are unmeasured.

| Symbol | Type | Location | Refs | Role |
|--------|------|----------|------|------|
| `Engine` / `Command` / `Publication` | Runtime types | `crates/tidemarkd/src/engine.rs` | Unmeasured | State, serialized mutations, publication |
| `Daemon` / `Published` | Service types | `crates/tidemarkd/src/service.rs` | Unmeasured | D-Bus interface and shared mirror |
| `catalog` / `account` | Functions | `crates/tidemarkd/src/registry.rs` | Unmeasured | Definitions and configured clients |
| `Provider` / `ProviderError` | Trait / error | `crates/tidemark-core/src/providers/mod.rs` | Unmeasured | Provider contract |
| `Spec` / `HandSpec` / `CATALOG` | Descriptors / catalog | `crates/tidemark-core/src/providers/keyed/mod.rs` | Unmeasured | Simple and custom provider definitions |
| `History` | Storage type | `crates/tidemark-core/src/storage/mod.rs` | Unmeasured | Persisted usage history |
| `Config` / `CredentialFile` | Configuration / credential types | `crates/tidemark-core/src/{config,oauth_file}.rs` | Unmeasured | Preferences and vendor credential updates |
| `ProviderStatus` / `Preferences` | Wire types | `crates/tidemark-types/src/wire.rs` | Unmeasured | Cross-process dictionary contract |
| `ids` | Module | `crates/tidemark-types/src/lib.rs` | Unmeasured | Installed identity and schema constants |
| `MainWindow` / `CardGrid` | GUI types | `crates/tidemark/src/{window,grid}.rs` | Unmeasured | Coordination and reorderable layout |
| `DaemonProxy` / `Update` | Client / update types | `crates/tidemark/src/bus.rs` | Unmeasured | IPC client and reconnect stream |

**Poll to pixel:** `tidemarkd::main::run` loads `Config`, `History`, and `Keyring`; `registry::accounts` supplies accounts. `Engine::poll_due` lazily constructs providers and fetches concurrently; `Engine::apply` calls `History::ingest(&Snapshot)` and `ProviderStatus::set_reading`. The publisher updates the mirror with `Published::upsert`, then emits `Daemon::provider_changed`.
On the GUI side, `bus::watch` drives `DaemonProxy` on `glib::spawn_future_local`; signals become `Update::Changed`. `MainWindow::handle` calls `show_all`/`show_one`; `Card::apply` uses `ProviderStatus::to_snapshot`; `QuotaBar::set` draws value and pace on a Cairo-backed `gtk::DrawingArea`.

**D-Bus contract:** name `io.github.zbndev.Tidemark.Daemon`, path `/io/github/zbndev/Tidemark`, interface `io.github.zbndev.Tidemark.Daemon1`; method `GetStatus`, signal `ProviderChanged`. App ID: `io.github.zbndev.Tidemark`. Activation uses `data/dbus-1/services/` and the systemd user unit `tidemarkd.service`. Published `a{sv}` dictionaries are extensible; absent values stay absent.

## CONVENTIONS
- Layering is enforced by `scripts/check-layering.sh`: types have no runtime I/O; IPC has no policy; CLI has no core/HTTP/SQLite/GTK/runtime; core has no GTK/GDK/adwaita; GUI has no core/HTTP/SQLite. Types may use zvariant, not zbus.
- Cargo sets `unsafe_code = "deny"`, not `forbid`; audited Windows exceptions exist. Clippy warns on `all`, `todo` and `dbg_macro`.
- Missing provider values stay missing. Window identity is its length, not the vendor field name; slugs are persistent storage keys.
- Config stores preferences, not secrets; preserve TOML decoration and reject present-but-invalid values. Provider/account array order determines UI order.
- Shared proxy policy covers child processes and bypasses loopback; the off setting inherits environment behavior.
- Errors use contextual `thiserror` enums, not `anyhow`. Daemon async runs on manually built multi-thread Tokio; GUI futures stay GLib-local. `async_channel` bridges only the ksni tray thread.
- Cargo only; crates declare their own dependencies, without `[workspace.dependencies]`. Change committed `Cargo.lock` through Cargo, never by hand.
- Provider fetch separates transport from pure parsing; a recognized malformed window fails the fetch, while genuinely unknown kinds may be skipped.
- Tests use built-in `#[test]`/`#[tokio::test]`, mostly colocated; integration tests cover process-global behavior and cross-seam flows. Use recorded provider fixtures, deterministic timestamps, `History::in_memory()`, and `FakeSecrets`, not the real keyring.

## ANTI-PATTERNS
- Do not fabricate quota rows, reset timestamps or pace when a provider omits them.
- Do not silently substitute a browser profile/account after a selected source fails, modify live browser databases, or expose credentials in diagnostics.
- Do not create vendor-owned credential files that do not already exist; preserve their ownership boundary.
- Do not configure cargo-deb system-scope `systemd-units` for the user daemon or restart it from RPM postun. Package greeting failure must not fail installation.
- Never run `scripts/release.sh` as a validator: it changes versions and performs commit/tag/push. The core live probe reads a real key and calls Z.ai; exclude it from routine validation.
- Never ship Windows system DLLs; the NSIS installer is per-user and unelevated.
- No embedded webview or JS engine. Never rename shipped provider slugs, fabricate history points, or hide windows.
- Root `src/` is ignored scratch, not a Cargo target; `pkg/`, `.worktrees/`, `target/`, and `*.pkg.tar.*` are build output, not editable source.

## UNIQUE STYLES
- Outbound application identity is `Tidemark/<version>`, not browser/executable impersonation; T3 Chat separately uses a browser-emulating transport stack.
- Linux uses session D-Bus and systemd user services; Windows uses per-user zbus p2p AF_UNIX, Task Scheduler/HKCU Run, jobs and native tray/toasts.
- Linux tray integration uses ksni, not libayatana-appindicator-glib. Bundled Rubik and icons are runtime assets.
- Provider SVG marks use filled outlines rather than strokes; desktop integration checks enforce this.
- UI construction is programmatic, styled through `style::STYLE`; state uses `Rc<RefCell<_>>`/`Cell`/`Weak`, with `CardGrid` the single custom GObject subclass. Card and notification thresholds share 70% / 90%.
- Simple API-key providers register alphabetically in `keyed::CATALOG`; unusual auth/multi-request clients use `HandSpec` and daemon `HAND_WRITTEN`, not forced `Keyed` implementations. Provider additions also update README and `docs/TRADEMARKS.md`.

## COMMANDS
```bash
cargo run -p tidemarkd
cargo run -p tidemark
tidemarkctl usage --format json
# Full local gate (plain workspace tests):
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
scripts/check-desktop-integration.sh
cargo build --release --locked --workspace
# After building both binaries:
cargo deb --no-build -p tidemark
cargo generate-rpm -p crates/tidemark
# Packaging/release helper tests:
scripts/test-release.sh
scripts/test-restart-user-daemon.sh
scripts/test-nix-flake.sh
# Public session-bus probes:
busctl --user introspect io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark
busctl --user call io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark io.github.zbndev.Tidemark.Daemon1 GetStatus
```

## NOTES
- Commands above are documented entry points, not a claim they passed during knowledge-base generation.
- Secret Service tests skip without a session bus; use plain `cargo test --workspace` for the local gate. Linux validation does not cover Windows-only branches; T3 Chat is Unix-only and Windows Antigravity local agy remains gated.
- Build prerequisites (Debian/Ubuntu): `libgtk-4-dev libadwaita-1-dev libsqlite3-dev pkg-config cmake g++ libclang-dev`; Fedora: `gtk4-devel libadwaita-devel sqlite-devel pkgconf-pkg-config cmake gcc-c++ clang-devel`.
- T3 Chat's BoringSSL client needs CMake, a C++ compiler, and libclang; `bindgen` generates its bindings at build time. The repository has no `build.rs`, no Blueprint, and no gresource files to precompile.
- Release only with `scripts/release.sh X.X.X` from clean, up-to-date `main`. It bumps workspace/dependency versions, lockfile, AppStream release entry, and PKGBUILD; commits, tags, and pushes without running tests. AppStream release prose is human work; tag push starts release CI.
- SQLite is system-linked; TLS uses rustls. Arch packaging disables makepkg LTO for aws-lc-sys.
- CI uses ubuntu-26.04 and Windows MSYS2 UCRT64 with `stable-x86_64-pc-windows-gnu`, not MSVC. Windows packaging documents pinned SHA-256 archives and PE import-closure staging.
- Nix exports packages, apps, a NixOS module and a dev shell for x86_64-linux/aarch64-linux.
- `scripts/test-package-upgrade.sh [workdir]` needs Docker/systemd; `scripts/check-tag-version.sh <tag>` validates release version alignment.
- More specific AGENTS.md files cover each crate and core's providers, keyed providers, Antigravity, browser and storage domains; keep implementation details there.

## USER PREFERENCES (binding)
- Never perform manual/UI verification (clicking through the app, screenshots, driving the installed app) — the user does all manual checks. State what to verify by hand and stop.

## BUILDING & TESTING ON THIS WINDOWS MACHINE (binding recipe)
The default Rust host toolchain here is MSVC and plain `cargo` in Git Bash FAILS (`link.exe` resolves to GNU coreutils' `link`, build scripts die). The project targets `stable-x86_64-pc-windows-gnu` with the MSYS2 UCRT64 GTK runtime. Always run builds/tests through MSYS2 bash with the toolchain and paths pinned:

```bash
C:/msys64/usr/bin/bash.exe -lc 'set -euo pipefail; \
  export PATH=/c/Users/zaebo/.cargo/bin:/ucrt64/bin:$PATH; \
  export RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu; \
  export PKG_CONFIG_PATH=/ucrt64/lib/pkgconfig:/ucrt64/share/pkgconfig; \
  cd /c/Users/zaebo/tidemark; \
  cargo build --release -p tidemark -p tidemarkd'
```
(adjust the final cargo command as needed). Without `/ucrt64/bin` on PATH linking fails (`cannot find -lglib-2.0`); without `RUSTUP_TOOLCHAIN` it builds MSVC. Replacing installed binaries: installed app lives in `$LOCALAPPDATA/Programs/tidemark`; `taskkill //IM tidemark.exe //IM tidemarkd.exe //F` first, back up, then copy from `target/release`. Known unrelated local test failures: `tidemark-core secrets::windows_store` tests fail while the real app/daemon is running (they touch the real Windows credential store), the `tidemarkd lifecycle::tests::the_singleton_is_exclusive_within_this_session` test fails while the daemon is running (singleton lock held), and `providers::keyed`/`providers::codex`/`oauth` live-transport tests fail behind a system proxy (HTTP 503) — neither is a regression signal; verify a suspicious failure with `git stash` before believing it.
