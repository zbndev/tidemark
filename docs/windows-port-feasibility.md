# Windows port feasibility

This document is a measured feasibility study for porting Tidemark to Windows natively. The constraints are fixed: the GTK4 + libadwaita stack stays, the architecture does not change, a single provider workflow serves both platforms, and the parity target is pixel parity within the limits of platform font rendering. No web stack, no Electron, no webview, no second UI. What follows maps every platform seam in the workspace to a Windows verdict, so an engineer can decide on the port and then execute it without re-doing the research.

Each section below covers one seam: the Linux code as it stands today, the Windows verdict with its evidence, the guards that keep the Linux path byte-identical, and the unknowns that remain, each labeled with the probe, VM procedure, or port-time spike that resolves it. The verdicts come from five research lanes plus independent verification against local crate sources; repo citations were re-checked against this branch.

The CI-probe triage (compile-failure inventory from the windows-latest UCRT64 and MSVC jobs) sits in the measured-gap section near the top, and the risk register, go/no-go gates, open measurements, port waves, provider rules, and pixel-parity protocol follow the seam sections. This file is the seam map those sections build on.

## Measured gap (CI probes)

The CI probes ran as two `windows-latest` jobs on GitHub run 33796395826 of `github.com/zbndev/tidemark` at head `c1a446b`. Each job executed `cargo check --workspace` under a different host: UCRT64/GNU (MSYS2) and MSVC/gvsbuild (GTK4_Gvsbuild_2026.8.0_x64.zip, gvsbuild 2026.8.0, gtk4 4.22.4, libadwaita-1 1.9.2, cmake 4.4.2, clang 20.1.8). In both toolchains the build died in the dependency graph before `rustc` reached any workspace crate. The inventoried workspace seams (daemon signal handling, `update.rs` exec, `oauth_file` libc flags, browser homes, `agy` pty, engine permissions) were therefore not exercised and remain code-reading-derived expectations, not yet measured. `tidemark-types` compiled and its tests passed on Windows-GNU in the same job's hard-gate step (not shown in the `cargo check --workspace` logs below).

### UCRT64 (GNU host)

| error (verbatim, first line) | at | kind | seam id | in inventory? |
|---|---|---|---|---|
| error[E0433]: cannot find `unix` in `os` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\desktop\secret.rs:23:20 | rustc-E | other | N |
| error[E0432]: unresolved import `std::os::fd` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\desktop\secret.rs:23:10 | rustc-E | other | N |
| error[E0433]: cannot find `unix` in `os` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\file_path.rs:3:9 | rustc-E | other | N |
| error[E0432]: unresolved import `zbus::zvariant::Fd` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\desktop\secret.rs:28:5 | rustc-E | other | N |
| error: failed to run custom build command for `boring-sys2 v4.15.15` | probe-ucrt64.log:681 | build-script | other | N |
| thread 'main' (3820) panicked at C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\boring-sys2-4.15.15\build\main.rs:791:39: | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\boring-sys2-4.15.15\build\main.rs:791:39 | panic | other | N |
| D:/a/_temp/msys64/ucrt64/include\time.h:158:30: error: expected ';' after top level declarator | D:/a/_temp/msys64/ucrt64/include/time.h:158:30 | clang-diagnostic | other | N |

Counting check: UCRT64 has 4 `rustc-E` rows; `grep -Ec '^[ ]*error\[[A-Z0-9]+\]' probe-ucrt64.log` returned 4.

Rows 1-4 (`ashpd`): these are Unix-only API usages in `ashpd`, which is pulled in by `oo7` (the Secret Service backend) declared at `crates/tidemark-core/Cargo.toml:25`. `ashpd` must not be compiled on Windows; the existing `secrets::Secrets` trait is the seam that lets us cfg-isolate `oo7`/`ashpd` and add a Windows Credential Manager backend.

Row 5 (`boring-sys2` build-script failure): `boring-sys2` is pulled in by `wreq` declared at `crates/tidemark-core/Cargo.toml:49` (via `tokio-boring2`/`boring2`). The cargo-level build-script failure shows that the BoringSSL build is not free on Windows-GNU.

Row 6 (`boring-sys2` panic): the same dependency; the panic records that bindgen is the failing step (`Unable to generate bindings: ClangDiagnostic(...)`).

Row 7 (`boring-sys2` clang-diagnostic): the same dependency; the first header error is repeated for `time.h:158-160` and `stdlib.h:241,242,244,248,559,561,562` (10 ClangDiagnostic lines total). This means wreq's BoringSSL build needs either a corrected bindgen target/header environment or a Windows-specific TLS backend decision.

### MSVC (gvsbuild host)

| error (verbatim, first line) | at | kind | seam id | in inventory? |
|---|---|---|---|---|
| error[E0433]: cannot find `unix` in `os` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\desktop\secret.rs:23:20 | rustc-E | other | N |
| error[E0432]: unresolved import `std::os::fd` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\desktop\secret.rs:23:10 | rustc-E | other | N |
| error[E0433]: cannot find `unix` in `os` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\file_path.rs:3:9 | rustc-E | other | N |
| error[E0432]: unresolved import `zbus::zvariant::Fd` | C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\ashpd-0.13.13\src\desktop\secret.rs:28:5 | rustc-E | other | N |

Counting check: MSVC has 4 `rustc-E` rows; `grep -Ec '^[ ]*error\[[A-Z0-9]+\]' probe-msvc.log` returned 4.

Rows 1-4 (`ashpd`): same new finding as UCRT64. `ashpd` is pulled in by `oo7` at `crates/tidemark-core/Cargo.toml:25` and is Unix-only. Because `ashpd` failed first, the MSVC job aborted before finishing `boring-sys2`, `gtk4-sys`, `libadwaita-sys`, or any workspace crate. The log does show `Checking boring-sys2 v4.15.15` (probe-msvc.log:30) before the ashpd failure, but no `boring-sys2`-specific diagnostic was observed.

### Dependency outcomes

| dependency | UCRT64 | MSVC |
|---|---|---|
| zbus (incl. uds_windows) | compiled | not reached |
| zvariant | compiled | not reached |
| gtk4-sys | compiled | not reached |
| libadwaita-sys | compiled | not reached |
| wreq / boring-sys2 (cmake+bindgen+go) | failed: error: failed to run custom build command for `boring-sys2 v4.15.15` | started (Checking), aborted by ashpd failure; no boring-sys2 error observed |
| rusqlite + libsqlite3-sys (system SQLite) | libsqlite3-sys compiled; rusqlite wrapper not reached (Downloaded only) | not reached |
| fs4 | not reached | not reached |
| oo7 | not reached | not reached |

UCRT64: `libsqlite3-sys` compiled without a pkg-config error, so the system SQLite link succeeded on that runner. MSVC: the gvsbuild bundle listing (`rerun-artifacts/probe-msvc-log/gvsbuild-listing.txt`) contains no `sqlite3.pc`; `rusqlite` was not reached and will lack a system sqlite3 when it is.

### IPC transport

**Current code.** Client and daemon both speak D-Bus over zbus 5 (caret minimum 5.13.2 in crates/tidemarkd/Cargo.toml:22 and crates/tidemark/Cargo.toml:31; Cargo.lock actually resolves zbus 5.19.0). The daemon binds the session bus and serves `io.github.zbndev.Tidemark.Daemon1` (crates/tidemarkd/src/main.rs:184); the client opens a session connection and builds a `DaemonProxy` (crates/tidemark/src/bus.rs:237, proxy macro at crates/tidemark/src/bus.rs:38). Every published shape is an `a{sv}` dictionary carried by zvariant, so absent stays absent. The daemon is started by D-Bus activation through data/dbus-1/services into the systemd user unit.

**Windows verdict.** zbus stays. The transport becomes peer-to-peer with no broker: A1 is the built-in AF_UNIX transport (zbus 5.19.0's only Windows dependency set is `uds_windows` plus `windows-sys`), A2 is a named-pipe adapter plugged through zbus's public `Socket`/`ReadHalf`/`WriteHalf` traits as the fallback. A1 is tried first because it needs zero custom transport code. `features = ["p2p"]` is added to the zbus dependency in both crates at port time; upstream p2p tests exercise object servers, methods, properties, and signals, and the zvariant types in tidemark-types are bus-framing-independent, so p2p carries the same wire bytes. Multi-peer signal fan-out uses ordered bounded queues per connection; a lagging client is disconnected and recovers by reconnect plus full reload. The client UI spawns the daemon when the endpoint is absent (client-spawn replaces D-Bus activation). Verdict established by the seam-strategy and ultrabrain lanes, with the A1/A2 split resolved by independent verification of the zbus 5.19.0 source.

**Linux regression guards.** The session-bus path stays exactly as it is: `Connection::session()` in the client and the `Builder::session()` serve in the daemon remain the Linux implementation, and the p2p feature is additive in zbus (it changes nothing unless the connection is built with a p2p address). The `Daemon1` interface definition and the `a{sv}` shapes in tidemark-types are shared, not duplicated, so both platforms compile the same contract.

**Remaining unknowns.**
- UNVERIFIED: object server, generated proxies, and signal fan-out over zbus p2p on Windows with the real zvariant types. Resolved by the transport spike (port-time, PoC gate 2): two simultaneous clients, `GetStatus`, `ProviderChanged` fan-out, lagging-client eviction, EOF/reconnect.
- UNVERIFIED: AF_UNIX on Windows 10 1803+ quirks (socket path length, per-user ACL scoping via the socket directory, antivirus interference). Resolved by the same spike on a clean Windows VM; failure drops to A2.
- UNVERIFIED: whether the workspace compiles for a Windows target at all, and where. Resolved by the CI probe triage section of this report.

### Secrets

**Current code.** Tidemark's own API keys and login tokens live in the Secret Service (`org.freedesktop.secrets`) via oo7 with `native_crypto` (crates/tidemark-core/Cargo.toml:25), behind the `secrets::Secrets` trait with `get` / `set` / `compare_and_set` / `delete` (crates/tidemark-core/src/secrets.rs:116) and `SecretError::Locked` as an explicit state rather than a crash (crates/tidemark-core/src/secrets.rs:55-60). Tests already inject `FakeSecrets` behind the same trait.

**Windows verdict.** A Windows Credential Manager backend behind the existing `secrets::Secrets` trait; oo7 stays the Linux backend, cfg-isolated. Credentials are generic credentials targeted `io.github.zbndev.Tidemark.{ProviderKey,ProviderToken}/<provider>/<account>`, matching the existing schema split. Persistence is `CRED_PERSIST_LOCAL_MACHINE`, required; `CRED_PERSIST_ENTERPRISE` is forbidden (the keyring crate's Windows store defaults to enterprise persistence and must be configured). Operations use the raw-blob form, not the password form, so the UTF-16 halving of capacity does not apply. If and only if a stored blob exceeds the 2560-byte Credential Manager cap (`CRED_MAX_CREDENTIAL_BLOB_SIZE`), the fallback is a DPAPI-protected atomic file, never chunked credentials. `SecretError::Locked` is kept (a locked workstation is not a locked Credential Manager), and `SecretError`'s D-Bus-only variant becomes backend-neutral. Verdict established by the librarian and ultrabrain lanes; https://docs.rs/keyring documents the Windows Credential Manager backend, https://docs.rs/oo7 confirms oo7 has none.

**Measured (CI probes).** New measured seam constraint: oo7, the Linux Secret Service backend declared at crates/tidemark-core/Cargo.toml:25, transitively requires ashpd, and ashpd does not compile on Windows today (it produced all four rustc errors in both probe toolchains; measured-gap tables). The port must therefore either feature-gate oo7 off on Windows — the Credential Manager backend replaces it, exactly as the verdict above provides — or patch the ashpd chain. Consistent with the existing Credential Manager verdict: cfg-isolating oo7 is now a compile requirement for the workspace, not only a platform choice.

**Linux regression guards.** The oo7 `Store` is untouched; the trait is already the single seam and tests already run against an in-memory fake, so the Linux implementation, its locked-keyring state machine, and the reconnecting keyring wrapper in crates/tidemarkd/src/keyring.rs do not change.

**Remaining unknowns.**
- UNVERIFIED: whether real stored documents (Codex-class access+refresh+ID JWTs, Claude, Antigravity) fit the 2560-byte cap. Resolved on Linux today by measuring our own Secret Service entries after real logins plus a refresh; the report's open-measurements section carries the exact commands. This is a release blocker until measured.
- UNVERIFIED: Credential Manager behavior under concurrent sessions sharing one profile. Resolved by the port-time secrets spike together with the daemon-lifecycle mutex work.

### Daemon lifecycle

**Current code.** `tidemarkd` is a systemd user unit (data/tidemarkd.service: `Type=dbus`, `BusName=`, `Restart=on-failure`, `RestartSec=5s`, `After=`/`PartOf=graphical-session.target`), enabled or disabled with `systemctl --user` from crates/tidemarkd/src/startup.rs:40-53, with XDG `.desktop` autostart overrides for the UI at crates/tidemarkd/src/startup.rs:63. Signals arrive via `tokio::signal::unix` (crates/tidemarkd/src/main.rs:33). Upgrades restart the daemon through data/restart-user-daemon, and the UI's update flow ends in `command.exec()` self-replacement (crates/tidemark/src/update.rs:63).

**Windows verdict.** A per-user Scheduled Task (only-when-logged-on, logon trigger per startup mode) replaces the systemd unit; a machine-wide Windows Service is rejected (session-0 isolation conflicts with per-user credentials, notifications, and GUI semantics). Single instance is enforced by a cross-session named mutex acquired before opening SQLite, so two RDP sessions of the same user share one daemon. The HKCU Run key is used for the UI only. The update flow mirrors the existing try-restart: record active state, graceful stop through a control operation, stop the task, replace binaries (a running exe cannot replace itself, so a separate updater step), restart iff it was active. agy helpers and child processes are killed on close via Job Objects; no persisted PIDs. Verdict established by the ultrabrain lane.

**Linux regression guards.** The systemd unit, data/restart-user-daemon, and the `startup::Startup` trait's Linux implementation stay exactly as they are; the Windows variant is added behind the same trait, and `service.rs`'s `change_startup_mode` rollback ordering is preserved unchanged.

**Remaining unknowns.**
- UNVERIFIED: Task Scheduler's 1-minute restart floor and 255-attempt cap versus systemd's `Restart=on-failure`/`5s` parity; whether the floor is acceptable or a per-user supervisor is needed. Resolved as an open measurement in the risk register; a clean-VM scheduled-task procedure confirms the floor behavior.
- UNVERIFIED: scheduled-task registration from an installer without elevation for a per-user task. Resolved by the packaging VM procedure.

### Tray

**Current code.** The tray is a StatusNotifierItem owned by the interface process, implemented with ksni 0.3.6 (crates/tidemark/Cargo.toml:26; `impl ksni::Tray for Model` at crates/tidemark/src/tray.rs:226), with `watcher_offline` waiting for an SNI watcher to return (crates/tidemark/src/tray.rs:314) and an `async_channel` bridge from ksni's thread to the GTK one.

**Windows verdict.** A runtime swap from ksni to the tray-icon crate (https://docs.rs/tray-icon, native Win32 backend). ksni compiles on Windows, because it is pure D-Bus/SNI over zbus, but there is no StatusNotifierWatcher host on Windows for it to talk to, so this is a backend replacement, not a compile fix. The `async_channel` bridge pattern stays; tray-icon requires an event loop on the thread that creates the icon. Verdict established by the librarian and ultrabrain lanes.

**Linux regression guards.** The ksni implementation, the watcher-offline behavior, and the minimize-on-close logic keyed on a status-notifier host accepting the icon all stay as the Linux path; the tray-icon backend lives behind the same tray seam and is never compiled into the Linux build.

**Remaining unknowns.**
- UNVERIFIED: tray-icon menu semantics parity (live menu rebuilds, item enable/disable) against the ksni menu model. Resolved by the port-time tray spike alongside the lifecycle PoC (gate 5).

### Notifications

**Current code.** The daemon sends notifications with a direct `org.freedesktop.Notifications` `Notify` call over its session-bus connection, carrying the urgency hint and a server-assigned replacement ID per window so the next message about a window replaces the one on screen (crates/tidemarkd/src/notify.rs:239-284; the `showing` map of window to server ID is at crates/tidemarkd/src/notify.rs:243-247).

**Windows verdict.** Toast notifications through windows-rs (`ToastNotificationManager` / `AppNotificationManager`), preserving the replacement-ID semantics: the per-window map of outstanding notification IDs is the seam's own state and carries over, with the Windows tag replacing the freedesktop notification ID. Verdict established by the librarian lane; winrt-notification was evaluated and rejected as self-described incomplete.

**Linux regression guards.** The decision logic (thresholds, per-segment dedup, reset-always-notifies) lives above the `Notifier` trait in notify.rs and is shared; only the `Desktop` backend behind the trait is platform code, and it stays the Linux implementation untouched.

**Remaining unknowns.**
- UNVERIFIED: toast activation from an unpackaged app and whether an AppUserModelID with a Start-menu shortcut is required for toasts to display. Resolved by the packaging survey's VM procedure and the port-time notification spike.

### Paths and config

**Current code.** All storage locations derive from XDG: `$XDG_DATA_HOME/tidemark` and `$XDG_CONFIG_HOME/tidemark` with relative-value rejection, in crates/tidemark-core/src/paths.rs:28-45 (the rejection rationale is tested at crates/tidemark-core/src/paths.rs:101). `config.toml` is edited in place and holds both provider settings and application preferences.

**Windows verdict.** AppData mapping (`%APPDATA%` for config, `%LOCALAPPDATA%` or `%APPDATA%` for data, settled at port time) implemented as platform-neutral primitives in the paths layer. Linux XDG semantics stay byte-identical: the XDG mapping remains the only implementation behind the primitive on Linux, and no Linux behavior changes. Verdict established by the seam-strategy lane; known-folders resolution on Windows uses the `dirs`/`directories` family, which the librarian lane classed as low risk.

**Linux regression guards.** paths.rs is already the single place paths are derived; the port adds a Windows arm behind the same functions and leaves the XDG arms, including the relative-value rejection, exactly as they are. Existing tests for the XDG behavior keep passing unchanged.

**Remaining unknowns.**
- UNVERIFIED: roaming versus local choice for `history.db` (a database that can grow, under a roaming profile it would sync). Resolved by a port-time decision recorded in this document; default is local.

### Browser / Chromium credentials

**Current code.** Seventeen cookie-based providers read browser storage through `crate::browser`: abacus, augment, commandcode, cursor, longcat, manus, mimo, mistral, notion, ollama, opencode, perplexity, qoder, sakana, session, t3chat, zoommate. Chromium cookie values are unsealed with the os_crypt scheme, PBKDF2/AES via RustCrypto for `v10` and `v11` (crates/tidemark-core/src/browser/chromium.rs:160-163; safe-storage key lookup in crates/tidemark-core/src/browser/safe_storage.rs:5), and profile discovery assumes a Unix home layout including Flatpak `.var/app` roots (crates/tidemark-core/src/browser/mod.rs:91-168). Cookie databases are read through an owner-only temporary copy, never in place.

**Windows verdict.** A storage factory behind the browser seam so all seventeen providers stay cfg-free: the providers ask for an authentication source and the factory resolves it per platform. On Windows, Chromium `v10`/`v11` unsealing becomes DPAPI master-key decryption plus AES-GCM records, replacing the Linux PBKDF2 path while keeping the same "unseal a Chromium value" operation. Chrome App-Bound encryption (v20) is flagged as port-time investigation: if it proves unavailable to unrelated native processes, the affected providers still authenticate through the other recorded browser sources (`browser::auth::Selection`), and the report spells the consequence out per provider. Verdict established by the seam-strategy and ultrabrain lanes.

**Linux regression guards.** The catalog registration in crates/tidemark-core/src/providers/keyed/mod.rs:551 stays the single registration point, and providers never name a platform: they consume the factory's platform-neutral operations. The Linux PBKDF2/os_crypt code and the snapshot-read discipline are unchanged.

**Measured Windows result (2026-09-04).** Profile discovery and ordinary Chromium `v10` decryption are implemented. The Windows factory scans the registered browsers' standard local/roaming vendor roots without writing or treating an absent vendor as an error. A fixture generated under the running user's real DPAPI context unwraps `Local State`'s `DPAPI`-prefixed key and decrypts an AES-256-GCM cookie; malformed base64/JSON, a tampered GCM tag, and an absent profile are unavailable rather than synthesized values.

Chrome App-Bound `v20` is **WINDOWS-UNAVAILABLE** to Tidemark. The investigation spike activated the installed Google Chrome Elevation Service through its registered CLSID `{708860E0-F641-4611-8895-7D867DD3675B}` and current `IElevator2Chrome` IID `{1BF5208B-295F-4992-B5F4-3A9BB6494838}`, then passed the installed profile's genuine `APPB` payload to `DecryptData`. The service returned HRESULT `0x8004A003`, `last_error=5` (`ERROR_ACCESS_DENIED`) for the unelevated Tidemark-path caller. Chromium's interface contract binds decryption to the installed browser identity; impersonating or injecting into Chrome, running as SYSTEM, or otherwise escalating privilege is unacceptable for ordinary read-only credential discovery. A queried `v20` cookie therefore becomes the explicit `PlatformUnavailable` state, not a panic, fabricated value, or silent empty cookie.

### oauth_file

**Current code.** Tidemark refreshes OAuth credentials inside files owned by the vendor CLIs, atomically, under strict file hygiene: `O_NOFOLLOW | O_CLOEXEC`, mode `0o600`, owner-only staging, and directory-level anti-reparse checks (`O_DIRECTORY | O_NOFOLLOW`) in crates/tidemark-core/src/oauth_file.rs:48-60 and crates/tidemark-core/src/oauth_file.rs:592-649, with `std::os::unix` throughout (crates/tidemark-core/src/oauth_file.rs:6).

**Windows verdict.** Split only the low-level primitives, secure-open, file identity, private staging, lock ownership, durable replace, behind platform-neutral operations. Compare-and-set, JSON field-merge, and canonical-path refusal stay shared. Anti-reparse semantics are preserved with Windows equivalents (reparse-point checks, ACLs), not cfg'd away: no symlink tricks on Windows either, the checks translate rather than disappear. Verdict established by the seam-strategy lane.

**Linux regression guards.** The shared layers (CAS, merge, refusal) keep their existing tests; the Unix primitive implementation is the current code moved behind the operations, so the Linux behavior, including every `O_NOFOLLOW` check, is byte-identical. The existing integration test crates/tidemark-core/tests/oauth_file.rs keeps exercising the Linux path.

**Remaining unknowns.**
- UNVERIFIED: which Windows APIs compose into exact equivalents for each primitive (open-without-reparse, owner-only file creation, atomic replace, lock ownership). Resolved by the port-time oauth_file spike (PoC gate 6) against real vendor CLI files.

### agy

**Current code.** Antigravity's local fallback spawns and supervises the `agy` CLI through Unix-only machinery: `std::os::unix` `CommandExt` (crates/tidemark-core/src/providers/antigravity/agy.rs:40), `rustix::process::test_kill_process` liveness checks (crates/tidemark-core/src/providers/antigravity/agy.rs:250-251), a `rustix::pty` pseudoterminal (crates/tidemark-core/src/providers/antigravity/agy.rs:447-448), and executable-bit checks via `PermissionsExt` (crates/tidemark-core/src/providers/antigravity/agy.rs:514-516).

**Windows verdict.** Port agy last (PoC gate 8). It needs ConPTY for the terminal and Job Objects for kill-on-close supervision. At port time, first verify that `agy.exe` itself has a Windows persistent-server mode; if it does not, the local-server fallback is Windows-unavailable, which is a state the provider reports, not a crash. The Tidemark-OAuth path for Antigravity does not depend on agy and is unaffected. Verdict established by the seam-strategy and ultrabrain lanes.

**Linux regression guards.** The agy module's Unix implementation stays as the Linux path behind the provider's existing local-server seam; nothing above it (provider logic, proxy environment injection at spawn, polling) changes, and the provider trait never learns the platform.

**Remaining unknowns.**
- UNVERIFIED: whether agy ships a Windows build with persistent-server behavior at all. Resolved by a port-time check of the vendor's Windows distribution before any ConPTY work begins.

### Toolchain

**Current code.** The UI floor is GTK 4.22 / libadwaita 1.9 via gtk4-rs 0.11 and adw 0.9 (`v4_22` / `v1_9` features, crates/tidemark/Cargo.toml:22 and crates/tidemark/Cargo.toml:18). The build needs pkg-config, cmake, a C++ compiler, and libclang (BoringSSL's bindgen for wreq), and links the system SQLite.

**Windows verdict.** Two candidates, decided by the CI probe measurements rather than by argument; see the measured-gap section above. MSYS2/UCRT64 is the primary bet: its repositories ship mingw-w64-gtk4 4.22.4 (built with the win32 backend) and mingw-w64-libadwaita 1.9.3 today (https://github.com/msys2/MINGW-packages), so the GTK runtime gate measured green (gtk4-sys and libadwaita-sys compiled on the runner), and its MinGW target matches zbus's documented Windows-GNU build target. MSVC/gvsbuild (https://gtk-rs.org/gtk4-rs/stable/latest/book/installation_windows.html) is the alternative; the probe resolved its bundle at gtk4 4.22.4 / libadwaita-1 1.9.2, above the floor, leaving the un-reached `-sys` compiles and the BoringSSL-plus-bindgen build as its unverified parts. wreq's maintainer supports Windows builds (https://github.com/0x676e67/wreq), and the workspace has no openssl-sys, so the boringssl/openssl-sys symbol conflict does not apply. On tests: the bus contract tests need a session bus today (crates/tidemark/src/bus.rs:507), which a D-Bus-free contract test over a zbus in-process socket pair replaces on Windows. Whichever toolchain wins, Rust target and C import libraries must never be mixed across MinGW and MSVC.

**Measured (CI probes).** Both toolchains died in the dependency graph before rustc reached any workspace crate. MSVC: ashpd 0.13.13 fails to compile (std::os::unix, std::os::fd, zbus::zvariant::Fd; pulled in via oo7, crates/tidemark-core/Cargo.toml:25) and aborted the build first. UCRT64/GNU: boring-sys2 4.15.15's build script panics in bindgen on the MSYS2 ucrt64 headers (the wreq chain, crates/tidemark-core/Cargo.toml:49). tidemark-types compiled and passed its tests on Windows-GNU in the same job's hard-gate step. The full error tables and per-dependency outcomes are in the measured-gap section at the top. The verdict stays measurement-decided, and both toolchains remain viable-pending: neither is eliminated, because the measured blockers are dependency fixes, not toolchain verdicts.

**Linux regression guards.** The toolchain decision is additive to CI: one OS matrix entry running the same cargo commands, with Linux steps byte-identical and shell/desktop checks staying Linux-only. No Linux build file changes.

**Remaining unknowns.**
- UNVERIFIED: which toolchain compiles the workspace end to end. The probe triage (measured-gap section) recorded the distinct-error inventory — both toolchains stopped in dependencies (ashpd on MSVC, boring-sys2 on UCRT64) — so this resolves by re-running `cargo check --workspace` once those two dependency blockers are fixed.
- UNVERIFIED: MSYS2's patched font-rendering behavior (its gtk4 build re-enables hinting and disables dcomp) against the parity target. Resolved by the VM visual-QA procedure in the risk section.

### Packaging survey

**Current code.** Linux packaging is deb, rpm, and PKGBUILD, each built on its oldest target OS, with asset lists in crates/tidemark/Cargo.toml and maintainer scripts that restart the user daemon via data/restart-user-daemon (the same try-restart flow the UI update path mirrors at crates/tidemark/src/update.rs:63). It is orthogonal to a Windows port and untouched by one.

**Windows verdict.** Survey only, no implementation and no bundling decision made yet. The candidates are NSIS and MSI (cargo-wix), with the GTK runtime bundled into the installer the way GIMP and Inkscape ship GTK on Windows; winget accepts MSI and NSIS installers with silent flags, so either format satisfies winget expectations. Verdict established by the librarian lane.

**Linux regression guards.** None needed beyond scope: no Linux packaging file is touched by this survey, and the port waves keep packaging last.

**Remaining unknowns.**
- UNVERIFIED: the concrete GTK runtime file set a Windows installer must carry (DLLs, loaders, schemas, icons). Resolved when the packaging decision is made, from a working MSYS2 or gvsbuild install tree on the VM.
- UNVERIFIED: installer-driven per-user scheduled-task registration without elevation. Resolved by the packaging VM procedure.

## Risk register

| Risk | Likelihood | Impact | Mitigation | Port phase |
|---|---|---|---|---|
| R1 zbus p2p transport semantics: no name-ownership arbitration, SignalEmitter is connection-specific, multi-client fan-out and lagging-client eviction over AF_UNIX all unproven with the real zvariant types | Medium | High: the daemon/client contract depends on it | Port-time transport spike (gate 3): two simultaneous clients, GetStatus, ProviderChanged fan-out, lagging-client eviction, EOF/reconnect; ordered bounded queues per connection, lagging client disconnected, reconnect plus full reload as recovery; fall back from A1 (AF_UNIX) to A2 (named-pipe Socket adapter) if AF_UNIX misbehaves | PoC wave 2 |
| R2 Stored credential blobs exceed the 2560-byte Credential Manager cap (CRED_MAX_CREDENTIAL_BLOB_SIZE); Codex-class access+refresh+ID JWT documents are the suspect | Medium | High: release blocker for the secrets backend until measured | Measure real blob sizes on Linux today (Measurements, procedure a); raw-blob credential form only, never the password form; if any blob exceeds the cap, fall back to a DPAPI-protected atomic file, never chunked credentials | Open now, decision before secrets backend lands |
| R3 Chrome App-Bound encryption (v20) proves unavailable to unrelated native processes, blocking Chromium cookie decryption for the seventeen cookie-based providers | Medium | High for the affected providers; each still has other recorded browser sources | Port-time browser-auth spike ordered Firefox plaintext, then Chromium DPAPI, then App-Bound v20; if v20 is unreachable, affected providers authenticate through the other entries of browser::auth::Selection and the consequence is spelled out per provider | PoC wave 7 |
| R4 Pixel parity ceiling: DirectWrite versus FreeType glyph rasterization, integer-only Win32 monitor scale (125 to 175 percent DPI cannot map to a fractional GDK scale), and MSYS2's patched font rendering (hinting re-enabled, dcomp disabled) make exact cross-OS pixel equality impossible | High that some delta exists | Low to Medium: geometry and content still match | Target logical-geometry parity, not pixel equality; bundle and privately register a pinned Cantarell on both platforms to close the font-absence gap; keep semantic libadwaita colors unpinned; region-based screenshot review at 100/125/150/200 percent on the user's VM | Continuous, visual QA per wave |
| R5 Task Scheduler restart floor of 1 minute with at most 255 attempts versus systemd Restart=on-failure with RestartSec=5s: no parity without extra machinery | High: the floor is documented Task Scheduler behavior | Medium: slower crash recovery, no data risk | Decision framing in Measurements, procedure b: accept the floor or build a per-user supervisor; confirmed on the clean VM with the scheduled-task procedure | PoC wave 5 |
| R6 MSVC/gvsbuild toolchain: GTK currency of the gvsbuild bundle and the BoringSSL plus bindgen build under MSVC are both unverified | Medium | High if MSYS2 were to fail, since gvsbuild is the fallback | MSYS2/UCRT64 is the primary bet (gtk4 4.22.4 and libadwaita 1.9.3 shipped today); the CI probe's MSVC job records the pinned gvsbuild bundle's gtk4 and libadwaita-1 modversions and the distinct-error inventory, and the toolchain is chosen on measurements, not argument | CI probe, decided before PoC wave 1 |

## Go/no-go gates

| Gate | Status | Evidence |
|---|---|---|
| 1. GTK runtime, MSYS2/UCRT64 | GREEN (measured) | The UCRT64 probe compiled gtk4-sys v0.11.4 (`v4_22`) and libadwaita-sys v0.9.2 (`v1_9`) on the runner (probe-ucrt64.log:557, :629); system-deps rejects anything below the floor, so on-runner pkg-config resolved gtk4 >= 4.22 and libadwaita-1 >= 1.9. MSYS2 ships mingw-w64-gtk4 4.22.4 (built with the win32 backend) and mingw-w64-libadwaita 1.9.3 (MINGW-packages PKGBUILDs, verified 2026-09-03) |
| 2. GTK runtime, gvsbuild/MSVC | PARTIAL | The probe measured the pinned gvsbuild 2026.8.0 bundle: pkg-config resolves gtk4 4.22.4 and libadwaita-1 1.9.2 (probe-msvc.log:3-4; gtk4.pc and libadwaita-1.pc present in the bundle listing), both at or above the 4.22 / 1.9 floor. But gtk4-sys was not reached on MSVC — ashpd aborted the build first — so the -sys compile on MSVC stays pending |
| 3. zbus p2p transport spike | PENDING | Port-time PoC gate; A1 (AF_UNIX via uds_windows, built into zbus 5.19.0) tried first, A2 (named-pipe adapter through the public Socket trait) as fallback; spike scope in the IPC transport section |
| 4. Chromium decryptability (DPAPI / App-Bound v20) | PARTIAL (measured 2026-09-04) | Ordinary DPAPI + AES-256-GCM `v10` is available. The real installed Chrome Elevation Service denied its genuine `APPB` payload to the unelevated Tidemark-path caller (`0x8004A003`, `ERROR_ACCESS_DENIED`), so `v20` is an explicit Windows-unavailable state. |
| 5. Credential blob sizes vs the 2560-byte cap | PENDING | Open measurement, executable today on Linux; exact procedure in Measurements, procedure (a) |

One hard constraint stands outside the gates: the minimum supported Windows version is Windows 10 1803, the floor for AF_UNIX transport; the recommended baseline is Windows 10 22H2 or Windows 11.

## Measurements

### (a) Credential blob sizes vs the 2560-byte cap

Executable today on Linux. Tidemark files API keys under the schema `io.github.zbndev.Tidemark.ProviderKey` and Tidemark-owned OAuth tokens under `io.github.zbndev.Tidemark.ProviderToken` (crates/tidemark-types/src/lib.rs:47 and crates/tidemark-types/src/lib.rs:49, re-exported by crates/tidemark-core/src/secrets.rs:44 and crates/tidemark-core/src/secrets.rs:47; the `xdg:schema` attribute name is set at crates/tidemark-core/src/secrets.rs:49).

1. Sign into the Tidemark-owned flows for claude, codex, and antigravity, and let at least one token refresh happen so the stored document is the refreshed form.
2. List the stored token entries:

   ```
   secret-tool search xdg:schema io.github.zbndev.Tidemark.ProviderToken
   ```

3. List the stored API-key entries:

   ```
   secret-tool search xdg:schema io.github.zbndev.Tidemark.ProviderKey
   ```

4. For each entry, print the raw secret and byte-count it (substitute the provider and account attributes shown by the search output):

   ```
   secret-tool lookup xdg:schema io.github.zbndev.Tidemark.ProviderToken provider <provider> account <account> | wc -c
   ```

5. Compare each byte count against 2560. If every blob fits, gate 5 goes green and the Credential Manager backend needs no fallback. If any blob exceeds 2560, the secrets verdict's fallback applies: a DPAPI-protected atomic file, never chunked credentials.

### (b) Task-Scheduler restart floor

Not measurable without Windows; this is the decision framing, and the user's VM procedure confirms the floor behavior on a clean machine.

1. Note the documented limits: Task Scheduler's minimum restart interval is 1 minute and its restart attempt cap is 255. The systemd unit being replaced restarts on failure after 5 seconds with no attempt cap (data/tidemarkd.service).
2. Frame the decision as a tradeoff. Accepting the floor means a crashed daemon stays down for up to a minute and the 255-attempt cap is effectively unreachable in normal use. Building a per-user supervisor restores systemd-like restart latency but adds a second supervised process, which is exactly the machinery the per-user Scheduled Task plus named-mutex design exists to avoid.
3. On the clean Windows VM, register the per-user task, kill tidemarkd.exe, and observe the actual restart latency and attempt accounting.
4. Record the outcome in this document: floor accepted, or supervisor scoped.

### (c) Hosted-runner GTK init

Resolvable only during port PoC wave 1; windows-latest has no documented virtual-display contract, so CI stays honestly headless until this is observed.

1. On the windows-latest runner, build the UI crate and run a preflight binary that calls `adw::init()` under `GDK_BACKEND=win32`.
2. Observe whether initialization succeeds on the runner's window station, whether a window maps, and whether libadwaita styles apply.
3. If init fails on the hosted runner, CI guarantees compile plus headless tests only, and visual acceptance stays on the user's clean VM per the parity protocol.

## Port waves

The order follows the draft's PoC ranking: sort by risk retired per effort, so the existential question (does the unchanged GTK UI even launch) goes first and the deepest Unix-specific machinery (agy) goes last. Effort classes are S, M, L, judged from the blast radius.

| Wave | Scope | Effort | Blast radius | Linux-regression guard |
|---|---|---|---|---|
| 1 | Unchanged GTK UI builds and launches on Windows | S | crates/tidemark build configuration and one CI matrix entry; the UI crate may compile unchanged (ksni is pure D-Bus) | Linux CI steps byte-identical; shell/desktop checks stay Linux-only |
| 2 | zbus p2p transport with the real zvariant types and ProviderChanged fan-out (decides A1 vs A2) | M | New crates/tidemarkd/src/transport.rs; `features = ["p2p"]` on zbus in both Cargo.tomls; client reconnect contract in crates/tidemark/src/bus.rs | `Connection::session()` and `Builder::session()` stay the Linux implementation; p2p is additive in zbus; the Daemon1 interface and a{sv} shapes are shared |
| 3 | tidemark-core compiles for the Windows target via target-gated dependencies only | M | crates/tidemark-core/Cargo.toml target-specific dependency arms | No silent provider stubs; Linux dependency set untouched |
| 4 | One generic keyed provider end to end across all seams (secrets, paths, storage) | M | Credential Manager backend behind secrets::Secrets; AppData arm in paths.rs | oo7 backend cfg-isolated and untouched; XDG arms byte-identical; FakeSecrets tests unchanged |
| 5 | Startup and tray lifecycle together | M | Windows variant behind startup::Startup; tray-icon backend swap in crates/tidemark/src/tray.rs; daemon signal handling | systemd unit, data/restart-user-daemon, and the ksni path stay exactly as they are; Windows backend never compiled into the Linux build |
| 6 | oauth_file against real vendor CLI files | M | Platform split of the low-level primitives in crates/tidemark-core/src/oauth_file.rs only | CAS, JSON field-merge, and canonical-path refusal stay shared with their tests; the Unix primitive implementation is the current code moved, byte-identical behavior |
| 7 | Browser auth: Firefox plaintext, then Chromium DPAPI, then App-Bound v20 | L | Platform submodules under crates/tidemark-core/src/browser/ (chromium.rs, safe_storage.rs, mod.rs discovery); seventeen keyed providers stay cfg-free | Linux PBKDF2/os_crypt code and snapshot-read discipline unchanged; the catalog registration stays the single registration point |
| 8 | agy local-server fallback (ConPTY, Job Objects; first verify agy.exe has a Windows persistent-server mode) | L | Platform split inside crates/tidemark-core/src/providers/antigravity/agy.rs | The Unix implementation stays the Linux path behind the provider's local-server seam; nothing above it changes; if no Windows agy exists, the fallback reports unavailable rather than crashing |

## Provider rules (single workflow)

The single-workflow guarantee is a property of how providers are written, not of the port. These rules keep it structural: a provider that satisfies all of them works on Linux and Windows with zero extra work, because every platform difference lives in shared infrastructure the seam sections already own.

1. Register the provider in exactly one place. A single-request key-authenticated provider is a `Spec` with one line in the `CATALOG` table (crates/tidemark-core/src/providers/keyed/mod.rs:551); a multi-request provider or one whose build refuses an option value is a `HandSpec` in the `HAND_WRITTEN` table (crates/tidemarkd/src/registry.rs:142). No second registry, no Windows-only list.
2. Write no `cfg(target_os = "windows")`, or any target cfg, anywhere under `crates/tidemark-core/src/providers/**`. Platform differences live in the shared seams (secrets, paths, browser, transport), never in provider code. If a provider seems to need a cfg, the seam it needs is missing; add the seam, not the cfg.
3. Discover credentials through platform-neutral primitives only: the paths layer (`data_dir` / `config_dir`, crates/tidemark-core/src/paths.rs:29 and crates/tidemark-core/src/paths.rs:39) plus environment overrides. A provider names a logical location, such as "the vendor CLI's credentials file", and never an OS path literal like `~/.config` or `%APPDATA%`.
4. Build cookie and browser-session providers on the browser factory only: `browser::stores()` at crates/tidemark-core/src/browser/mod.rs:445 and `browser::stores_in()` at crates/tidemark-core/src/browser/mod.rs:454. The factory owns per-browser profile discovery today and will own the Windows equivalents (AppData profile roots, DPAPI unsealing) inside the browser seam, so the seventeen existing cookie providers stay cfg-free; the list stays in the Browser seam section.
5. Declare the Windows story of a vendor-CLI provider at authoring time. For claude, codex, and antigravity/agy the file must state, in a comment beside the discovery code, where the vendor's CLI writes credentials on Windows, or carry the labeled unknown "Windows discovery TODO". A silent Linux-only assumption is a defect; a labeled unknown with a resolution path is not.
6. Keep tests platform-neutral. `parse(body, captured_at)` is a pure function and its tests run everywhere; no `#[cfg(unix)]` test gates under `providers/**`. A behavior that can only be tested on one OS belongs in a seam module's own tests, not in a provider test.

## Pixel-parity protocol

The parity target is logical-geometry equality, verified by region-based screenshot review: same widget tree, same sizes in layout units, same semantic colors, compared region by region. It is explicitly not literal pixel equality, and this protocol never promises that. Two findings from the research make pixel equality impossible by construction: DirectWrite and FreeType rasterize the same glyphs differently at the pixel level, and GTK 4.22 on Windows snaps to the integer Win32 monitor scale `max(1, dpi/96)`, so a 125% or 175% display maps to a different GDK scale than Linux fractional scaling would (the parity classification table in the ultrabrain advisory, .omo/drafts/windows-port-feasibility.md). What can be equal, and what this protocol checks, is the geometry and content the workspace controls.

### Matrix

Every comparison runs the full matrix: display scales 100%, 125%, 150%, 200%, in both light and dark themes, eight conditions per region. Regions are the window's functional areas (the provider list, a provider detail pane, the chart and bar widgets, the settings dialog), not the whole window as one image, so a font-rendering difference in one label does not mask a layout regression elsewhere.

### Linux reference capture (runnable today)

On the maintainer's Linux machine, against the current build:

1. Build and run the app as usual (`cargo run -p tidemark` under the normal session).
2. Set the display scale in the desktop's display settings to 100%, then 125%, 150%, and 200% in turn, relaunching the app after each change so GTK picks up the new scale.
3. At each scale, capture each region twice: once in the light theme and once in the dark theme (toggle dark mode in the desktop appearance settings or the app's own preference). On GNOME, `gnome-screenshot --area` or the interactive screenshot UI works; any tool that crops to a region is fine.
4. Sign into at least one keyed provider and one cookie provider first, so the reference regions show populated data rather than empty states.
5. Store the captures outside the repository, named by scale, theme, and region (for example `linux-125-dark-provider-detail.png`). Screenshots are working artifacts, not deliverables; none are committed.

These references are the baseline. Re-capture them whenever the widget tree or the CSS changes, since a stale reference produces false findings.

### Windows capture procedure (after port PoC wave 1)

UNVERIFIED until port PoC wave 1 yields a launchable tidemark.exe; resolution path is the wave 1 gate in the port-waves table. On the user's Windows VM (Q2: the user runs this personally):

1. Launch the app under the Win32 backend: set `GDK_BACKEND=win32` in the session environment before starting tidemark.exe.
2. Set the display scale in Windows Settings (System, Display, Scale) to 100%, 125%, 150%, and 200% in turn, relaunching the app after each change.
3. At each scale, capture the same regions in both light and dark themes (Windows Settings, Personalization, Colors), using the Snipping Tool's region capture.
4. Compare each Windows capture against its Linux reference side by side, region by region. A finding is a difference in logical geometry: a missing widget, a different layout-unit size, a wrong semantic color, a clipped or overlapping element. A difference confined to glyph rasterization or subpixel rendering is expected and is not a finding.
5. Submit findings as entries in this document, each labeled UNVERIFIED until reproduced, with the scale, theme, region, and the resolution path: the seam section that owns the geometry (style and CSS, the bar/chart widgets, or the GTK-runtime risk row). Rasterization-only observations go in the same place labeled as expected differences, so later runs do not re-litigate them.

### Font strategy

Cantarell's absence on Windows is closable by design: bundle a pinned Cantarell build with the app and register it as a private font on both platforms, using the same registration mechanism on Linux and Windows so the comparison is fair and neither side falls back to a system font the other lacks. This is optional but recommended, since it removes the largest avoidable source of geometry drift (font metrics feed layout). What bundling does not remove is rasterization difference; that stays in the expected-differences class above. CJK parity is an explicit non-goal: bundling a Noto CJK set is a large binary cost and is excluded from this protocol unless later chosen as its own work item.
