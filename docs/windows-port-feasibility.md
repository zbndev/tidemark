# Windows port feasibility

This document is a measured feasibility study for porting Tidemark to Windows natively. The constraints are fixed: the GTK4 + libadwaita stack stays, the architecture does not change, a single provider workflow serves both platforms, and the parity target is pixel parity within the limits of platform font rendering. No web stack, no Electron, no webview, no second UI. What follows maps every platform seam in the workspace to a Windows verdict, so an engineer can decide on the port and then execute it without re-doing the research.

Each section below covers one seam: the Linux code as it stands today, the Windows verdict with its evidence, the guards that keep the Linux path byte-identical, and the unknowns that remain, each labeled with the probe, VM procedure, or port-time spike that resolves it. The verdicts come from five research lanes plus independent verification against local crate sources; repo citations were re-checked against this branch.

The CI-probe triage (compile-failure inventory from the windows-latest UCRT64 and MSVC jobs) is inserted where marked, and the risk register, go/no-go gates, and recommended port waves follow in later commits. This file is the seam map those sections build on.

<!-- measured-gap section lands here (todo 3) -->

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

**Remaining unknowns.**
- UNVERIFIED: DPAPI decryption of current Chrome/Edge/Brave/Vivaldi cookie stores on Windows, including App-Bound v20 reachability. Resolved by the port-time browser-auth spike (PoC gate 7), ordered Firefox plaintext first, then Chromium DPAPI, then App-Bound.
- UNVERIFIED: Windows browser profile discovery layout per vendor. Resolved inside the same spike.

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

**Windows verdict.** Two candidates, decided by the CI probe measurements rather than by argument; see the measured-gap section above. MSYS2/UCRT64 is the primary bet: its repositories ship mingw-w64-gtk4 4.22.4 (built with the win32 backend) and mingw-w64-libadwaita 1.9.3 today (https://github.com/msys2/MINGW-packages), so the GTK runtime gate is green on paper, and its MinGW target matches zbus's documented Windows-GNU build target. MSVC/gvsbuild (https://gtk-rs.org/gtk4-rs/stable/latest/book/installation_windows.html) is the alternative; its GTK currency and the BoringSSL-plus-bindgen build are the unverified parts. wreq's maintainer supports Windows builds (https://github.com/0x676e67/wreq), and the workspace has no openssl-sys, so the boringssl/openssl-sys symbol conflict does not apply. On tests: the bus contract tests need a session bus today (crates/tidemark/src/bus.rs:507), which a D-Bus-free contract test over a zbus in-process socket pair replaces on Windows. Whichever toolchain wins, Rust target and C import libraries must never be mixed across MinGW and MSVC.

**Linux regression guards.** The toolchain decision is additive to CI: one OS matrix entry running the same cargo commands, with Linux steps byte-identical and shell/desktop checks staying Linux-only. No Linux build file changes.

**Remaining unknowns.**
- UNVERIFIED: which toolchain compiles the workspace, and the distinct-error inventory per toolchain. Resolved by the CI probe triage (measured-gap section).
- UNVERIFIED: MSYS2's patched font-rendering behavior (its gtk4 build re-enables hinting and disables dcomp) against the parity target. Resolved by the VM visual-QA procedure in the risk section.

### Packaging survey

**Current code.** Linux packaging is deb, rpm, and PKGBUILD, each built on its oldest target OS, with asset lists in crates/tidemark/Cargo.toml and maintainer scripts that restart the user daemon via data/restart-user-daemon (the same try-restart flow the UI update path mirrors at crates/tidemark/src/update.rs:63). It is orthogonal to a Windows port and untouched by one.

**Windows verdict.** Survey only, no implementation and no bundling decision made yet. The candidates are NSIS and MSI (cargo-wix), with the GTK runtime bundled into the installer the way GIMP and Inkscape ship GTK on Windows; winget accepts MSI and NSIS installers with silent flags, so either format satisfies winget expectations. Verdict established by the librarian lane.

**Linux regression guards.** None needed beyond scope: no Linux packaging file is touched by this survey, and the port waves keep packaging last.

**Remaining unknowns.**
- UNVERIFIED: the concrete GTK runtime file set a Windows installer must carry (DLLs, loaders, schemas, icons). Resolved when the packaging decision is made, from a working MSYS2 or gvsbuild install tree on the VM.
- UNVERIFIED: installer-driven per-user scheduled-task registration without elevation. Resolved by the packaging VM procedure.
