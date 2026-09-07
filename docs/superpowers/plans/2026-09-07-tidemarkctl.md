# tidemarkctl Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `tidemarkctl`, a command-line client that can do everything the GTK window can, so panel plugins and scripts consume the daemon without generating a D-Bus proxy of their own.

**Architecture:** The `#[zbus::proxy] trait Daemon` already covers the entire interface and
lives in the GUI crate; it moves into a new `tidemark-ipc` crate so the window and the CLI
share one definition of the contract. A second new crate, `tidemark-cli`, builds the
`tidemarkctl` binary on top of it: clap grammar grouped by entity, three output formats
(`text`, `json`, `waybar`), a `guard` verdict with stable exit codes, and a `watch` stream
that prints NDJSON as the daemon's signals arrive. No provider I/O, no GTK, no Tokio.

**Tech Stack:** Rust 2024 (MSRV 1.92), zbus 5.13.2 on its async-io backend, clap 4 derive,
clap_complete, serde_json, async-io for `block_on` and the retry timer.

**Spec:** `docs/superpowers/specs/2026-09-07-tidemarkctl-design.md` — read it first; this plan argues from it.

## Global Constraints

- Work only on branch `feat/cli`. Never commit to `main`.
- Gate before every commit: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`. Run the targeted test during a task.
- `tidemark-ipc` and `tidemark-cli` must not depend on `tidemark-core`, `reqwest`, `hyper`, `rusqlite`, `libsqlite3-sys`, `gtk4`, `gtk4-sys`, `libadwaita` **or `tokio`**. `scripts/check-layering.sh` enforces it.
- Pin `zbus = "5.13.2"`, the version `tidemark` and `tidemarkd` already use. Add every dependency with `cargo add` — the committed `Cargo.lock` changes through Cargo, never by hand.
- No `anyhow`. The CLI's error type is one struct, `exit::Failure { exit: Exit, message: String }`.
- Exit codes: `0` success, `1` guard below threshold, `64` invalid arguments, `69` daemon unreachable or reading unavailable, `70` the daemon returned an error.
- Percentages and spans go through `tidemark_types::present::{percent, duration}`. Never format a percentage or a duration by hand: the card, the notification and the CLI must not disagree.
- Absent stays absent. A window without `resets_at` prints nothing about a reset and serializes without the key. Never substitute `0` or `null`.
- Secrets arrive on stdin or via `--key-file PATH`. **Never** as an argv value, under any flag.
- Windows: `tidemarkctl` must keep compiling there (CI builds `--workspace`) but is not packaged and not supported. Do not add it to any NSIS or Windows staging list, and do not touch `crates/tidemark/src/bus.rs`'s `cfg(windows)` `reconnect` module.
- Tests: built-in `#[test]`/`#[tokio::test]`-free — this crate has no Tokio, so async tests use `async_io::block_on` inside a plain `#[test]`. Colocated `#[cfg(test)] mod tests`, descriptive `a_…`/`the_…` snake_case names. Tests needing a session bus print `skipped: no session bus reachable` and return, the way `crates/tidemarkd/src/service.rs` tests do.
- Commit style: Conventional Commits with scopes (`feat(cli):`, `refactor(ipc):`, `docs:`).

---

## Phase A — one definition of the contract

### Task 1: `tidemark-ipc`, the shared proxy

**Files:**
- Create: `crates/tidemark-ipc/Cargo.toml`
- Create: `crates/tidemark-ipc/src/lib.rs`
- Create: `crates/tidemark-ipc/AGENTS.md`
- Modify: `Cargo.toml` (workspace members)
- Modify: `crates/tidemark/Cargo.toml` (add the dependency)
- Modify: `crates/tidemark/src/bus.rs:38-186` (cut the trait, re-export instead)
- Modify: `scripts/check-layering.sh`

**Interfaces:**
- Produces: `tidemark_ipc::DaemonProxy` (generated) with the same method, property and
signal set the GUI has today, plus one message struct per signal
(`ProviderChanged`, `ProviderRemoved`, `OrderChanged`, `UpdateChanged`,
`PreferencesChanged`, `DataChanged`, `ActivateRequested`). `#[zbus::proxy]` *consumes* the
trait it is written on, so there is no `tidemark_ipc::Daemon` to re-export. Every later
task consumes `DaemonProxy`.
- Produces: `crate::bus::DaemonProxy` in the GUI keeps resolving, via `pub use`, so `detail.rs`, `preferences.rs`, `window.rs`, `provider_settings/mod.rs` and `provider_settings/detail.rs` are not edited at all.

- [x] **Step 1: Create the crate manifest**

`crates/tidemark-ipc/Cargo.toml`. Copy the `[package]`/`[lints]` shape from
`crates/tidemark-types/Cargo.toml` so the workspace inheritance matches:

```toml
[package]
name = "tidemark-ipc"
description = "The D-Bus contract between tidemarkd and its clients"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[lints]
workspace = true

[dependencies]
tidemark-types = { path = "../tidemark-types" }
zbus = { version = "5.13.2", features = ["p2p"] }
```

`p2p` is kept because the Windows GUI builds its connection that way and this crate is
what it builds the proxy from.

- [x] **Step 2: Add the crate to the workspace**

In the root `Cargo.toml`, `members` becomes:

```toml
members = [
    "crates/tidemark-types",
    "crates/tidemark-ipc",
    "crates/tidemark-core",
    "crates/tidemarkd",
    "crates/tidemark",
]

`crates/tidemark-cli` is **not** listed here: a member that does not exist yet fails every
`cargo` command in the workspace. Task 2 adds the line when it creates the crate.

- [x] **Step 3: Move the proxy**

`crates/tidemark-ipc/src/lib.rs` starts with this module documentation, then contains
lines 38-186 of `crates/tidemark/src/bus.rs` **verbatim** — the whole `#[zbus::proxy]`
attribute and `pub trait Daemon`, unchanged, including every doc comment:

```rust
//! The D-Bus contract between `tidemarkd` and everything that reads it.
//!
//! One definition, shared: the GTK window and `tidemarkctl` generate their proxy from this
//! trait, so a method that changed shape is a compile error here rather than a runtime
//! error on somebody's machine. That is the same argument that keeps the wire vocabulary in
//! `tidemark-types` — a rule the build enforces beats a rule a hurry can skip.
//!
//! Nothing here opens a connection or decides a policy. Reconnection, retry and the
//! Windows peer-to-peer transport belong to the client that needs them.

use tidemark_types::{
    AuthCandidate, AuthSelection, DataInfo, HistoryPoint, Preferences, ProviderDefinition,
    ProviderStatus,
};
```

The trait body references `tidemark_types::HistoryPoint` by full path today
(`current_segment`'s return type); with the import above, shorten it to `Vec<HistoryPoint>`.
Nothing else changes.

- [x] **Step 4: Point the GUI at it**

In `crates/tidemark/Cargo.toml`, beside the other path dependency:

```toml
tidemark-ipc = { path = "../tidemark-ipc" }
```

In `crates/tidemark/src/bus.rs`, replace lines 38-186 (the attribute and the trait) with a
re-export of what the macro generated — the proxy and the signal message types `Event`
matches on:

```rust
/// The contract lives in `tidemark-ipc`, shared with `tidemarkctl`. Re-exported here so
/// this module stays the one place the rest of the interface asks for the daemon.
///
/// `#[zbus::proxy]` consumes the trait it is written on and emits the proxy plus one
/// message type per signal, so those names come from there too: [`Event`] below matches on
/// them.
pub use tidemark_ipc::{
    ActivateRequested, DaemonProxy, DataChanged, OrderChanged, PreferencesChanged, ProviderChanged,
    ProviderRemoved, UpdateChanged,
};
```

Keep the `use tidemark_types::{…}` list at the top of `bus.rs`: `Update` and `load` still
name those types. Remove from it only what the compiler then reports as unused.

- [x] **Step 5: Build the GUI and run its tests**

Run: `cargo test -p tidemark`
Expected: PASS, including the five `bus.rs` reconnect tests that build a `DaemonProxy`
through `DaemonProxy::builder(…)`. If a `cfg(windows)` block fails to resolve
`DaemonProxy`, check that the `pub use` is outside every `cfg`.

- [x] **Step 6: Write the contract's own test**

At the end of `crates/tidemark-ipc/src/lib.rs`. This proves the moved proxy still names the
right interface and path, and still decodes the published dictionaries:

```rust
#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, ProviderId, ProviderStatus, ids};

    /// A daemon that answers two of the real methods.
    struct FakeDaemon;

    #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
    impl FakeDaemon {
        async fn get_status(&self) -> Vec<ProviderStatus> {
            vec![ProviderStatus::pending(
                &ProviderId::new("zai".to_owned()),
                &AccountId::default(),
            )]
        }

        #[zbus(property(emits_changed_signal = "false"))]
        async fn version(&self) -> String {
            "0.0.0-test".to_owned()
        }
    }

    #[test]
    fn the_proxy_reads_a_daemon_serving_the_interface() {
        async_io::block_on(async {
            let Ok(connection) = zbus::Connection::session().await else {
                eprintln!("skipped: no session bus reachable");
                return;
            };
            let name = format!("io.github.zbndev.TidemarkIpcTest{}", std::process::id());
            let server = zbus::connection::Builder::session()
                .expect("session builder")
                .name(name.as_str())
                .expect("unique test name")
                .serve_at(ids::OBJECT_PATH, FakeDaemon)
                .expect("serves the object")
                .build()
                .await
                .expect("test service starts");

            let proxy = super::DaemonProxy::builder(&connection)
                .destination(name.as_str())
                .expect("valid destination")
                .build()
                .await
                .expect("proxy builds");

            assert_eq!(proxy.version().await.expect("version"), "0.0.0-test");
            let statuses = proxy.get_status().await.expect("status");
            assert_eq!(statuses.len(), 1);
            assert_eq!(statuses[0].provider, "zai");
            assert_eq!(statuses[0].state, "pending");

            drop(server);
        });
    }
}
```

Add the dev-dependency this needs: `cargo add -p tidemark-ipc --dev async-io`.

- [x] **Step 7: Run it**

Run: `cargo test -p tidemark-ipc`
Expected: PASS (or the skip line on a machine with no session bus).

- [x] **Step 8: Teach the layering check about the new crate**

In `scripts/check-layering.sh`, after the `tidemark-types` block, insert:

```bash
# The contract's client half: it may open a connection, and nothing else. tokio is on the
# list because zbus can be built on either reactor, and a CLI whose value is starting fast
# must not acquire a second runtime by accident.
forbid tidemark-ipc 'the contract carries no implementation' \
    tidemark-core reqwest hyper rusqlite libsqlite3-sys gtk4 gtk4-sys libadwaita tokio
```

Also extend the header comment block at the top of the file, which currently describes
four crates, with:

```bash
#   tidemark-ipc    the generated D-Bus proxy, shared by every client.
#   tidemark-cli    tidemarkctl. Speaks D-Bus and prints; no runtime, no display.
```

- [x] **Step 9: Run the full gate**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
Expected: PASS, `layering ok`.

- [x] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/tidemark-ipc crates/tidemark/Cargo.toml crates/tidemark/src/bus.rs scripts/check-layering.sh
git commit -m "refactor(ipc): share the daemon proxy between the window and a CLI"
```

---

## Phase B — reading

### Task 2: the binary, and `version`

**Files:**
- Create: `crates/tidemark-cli/Cargo.toml`
- Create: `crates/tidemark-cli/src/main.rs`
- Create: `crates/tidemark-cli/src/cli.rs`
- Create: `crates/tidemark-cli/src/connect.rs`
- Create: `crates/tidemark-cli/src/exit.rs`
- Modify: `scripts/check-layering.sh`

**Interfaces:**
- Produces: `exit::Exit` (`Ok`, `Below`, `Usage`, `Unavailable`, `Daemon`) with `fn code(self) -> u8`, and `exit::Failure { exit, message }` with `From<zbus::Error>`.
- Produces: `connect::daemon() -> zbus::Result<DaemonProxy<'static>>`.
- Produces: `cli::Cli` with `cli::Command`, extended by every later task.

- [x] **Step 1: Create the crate**

Add `"crates/tidemark-cli"` to the root `Cargo.toml`'s `members` — Task 1 deferred it, so
it is this task's line to write — and create `crates/tidemark-cli/Cargo.toml`:

```toml
[package]
name = "tidemark-cli"
description = "Tidemark command-line client"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[lints]
workspace = true

[[bin]]
name = "tidemarkctl"
path = "src/main.rs"

[dependencies]
tidemark-ipc = { path = "../tidemark-ipc" }
tidemark-types = { path = "../tidemark-types" }
zbus = "5.13.2"
```

Then add the rest with Cargo, so the lockfile is Cargo's work:

```bash
cargo add -p tidemark-cli clap --features derive
cargo add -p tidemark-cli serde_json async-io
cargo add -p tidemark-cli serde --features derive
```

Note the absent `p2p`: the CLI only ever talks to a session bus.

- [x] **Step 2: Write the exit codes**

`crates/tidemark-cli/src/exit.rs`:

```rust
//! How the process ends. `guard`'s codes *are* its output, so every code is named once
//! here and shared, and the two failures a script reacts to differently — nobody answered,
//! versus the daemon refused — never collapse into the same number.

/// Process exit codes, following sysexits where one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what it was asked.
    Ok = 0,
    /// `guard` only: less quota is left than was asked for.
    Below = 1,
    /// The arguments do not name a command this build can run (`EX_USAGE`).
    Usage = 64,
    /// The daemon could not be reached, or there is no reading to judge (`EX_UNAVAILABLE`).
    Unavailable = 69,
    /// The daemon answered with an error (`EX_SOFTWARE`).
    Daemon = 70,
}

impl Exit {
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Why a command stopped, in the form the process exits with.
#[derive(Debug)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Usage,
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Unavailable,
            message: message.into(),
        }
    }
}

/// An error *reply* is the daemon refusing something; anything else is the daemon not being
/// there. A script retries one and gives up on the other.
impl From<zbus::Error> for Failure {
    fn from(error: zbus::Error) -> Self {
        let exit = match &error {
            zbus::Error::MethodError(..) | zbus::Error::FDO(_) => Exit::Daemon,
            _ => Exit::Unavailable,
        };
        Self {
            exit,
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self {
            exit: Exit::Daemon,
            message: format!("cannot serialize the daemon's answer: {error}"),
        }
    }
}
```

Until a later task constructs them, `Exit::Below`, `Exit::Usage`, `Failure::usage` and
`Failure::unavailable` are dead code, and the gate denies warnings. Each carries
`#[expect(dead_code, reason = "…")]`, not `allow`: `expect` warns once the item *is* used,
so the task that reaches for it is told to delete the marker.

- [x] **Step 3: Write the connection**

`crates/tidemark-cli/src/connect.rs`:

```rust
//! The one connection this program makes.
//!
//! Nothing here inspects `systemctl`: the daemon is D-Bus-activated, so asking for it is
//! how it starts. A machine with no session bus fails as unreachable, which is the truth.

use tidemark_ipc::DaemonProxy;

pub async fn daemon() -> zbus::Result<DaemonProxy<'static>> {
    let connection = zbus::Connection::session().await?;
    DaemonProxy::new(&connection).await
}
```

- [x] **Step 4: Write the failing grammar test**

`crates/tidemark-cli/src/cli.rs`:

```rust
//! The argument grammar, grouped by the entity a subcommand acts on rather than flattened
//! into thirty verbs: a person reading `--help` should find `provider add` under
//! `provider`, and a plugin author should be able to guess the next one.

use clap::{Parser, Subcommand};

/// The command-line client for the Tidemark daemon.
#[derive(Debug, Parser)]
#[command(name = "tidemarkctl", version, about, arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// This client's version, and the daemon's.
    Version,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_takes_no_arguments() {
        let cli = Cli::parse_from(["tidemarkctl", "version"]);
        assert!(matches!(cli.command, Command::Version));
    }
}
```

`Cli::command()` needs `use clap::CommandFactory;` inside the test module.
`debug_assert()` is clap's own grammar validator — it catches a `requires` naming a
non-existent argument, which is the mistake this grammar will make most often as it grows.

- [x] **Step 5: Run it and watch it fail**

Run: `cargo test -p tidemark-cli`
Expected: FAIL — `main.rs` does not exist yet, so the crate does not build.

- [x] **Step 6: Write main**

`crates/tidemark-cli/src/main.rs`:

```rust
//! `tidemarkctl`: the third consumer of the daemon's interface, after the window and
//! `busctl`.
//!
//! It performs no provider I/O — every number it prints came off the bus — and it holds no
//! runtime: zbus's async-io backend drives its own connection thread, so one `block_on` at
//! the top is the whole of this program's concurrency.

mod cli;
mod connect;
mod exit;

use std::process::ExitCode;

use clap::Parser;

use crate::exit::{Exit, Failure};

fn main() -> ExitCode {
    let parsed = cli::Cli::parse();
    match async_io::block_on(run(parsed)) {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(failure) => {
            eprintln!("{}", failure.message);
            ExitCode::from(failure.exit.code())
        }
    }
}

async fn run(cli: cli::Cli) -> Result<Exit, Failure> {
    match cli.command {
        cli::Command::Version => {
            let proxy = connect::daemon().await?;
            println!("tidemarkctl {}", env!("CARGO_PKG_VERSION"));
            println!("tidemarkd {}", proxy.version().await?);
            Ok(Exit::Ok)
        }
    }
}
```

- [x] **Step 7: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [x] **Step 8: Smoke it against the live daemon**

Run: `cargo run -p tidemark-cli -- version`
Expected: two lines, the second being the running daemon's version.

Run: `env DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent cargo run -p tidemark-cli -- version; echo $?`
Expected: a message on stderr and `69`.

- [x] **Step 9: Add the CLI to the layering check**

In `scripts/check-layering.sh`, after the `tidemark-ipc` block:

```bash
forbid tidemark-cli 'the CLI prints what the daemon publishes and nothing else' \
    tidemark-core reqwest hyper rusqlite libsqlite3-sys gtk4 gtk4-sys libadwaita tokio
```

Run: `./scripts/check-layering.sh`
Expected: `layering ok`.

- [x] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/tidemark-cli scripts/check-layering.sh
git commit -m "feat(cli): tidemarkctl skeleton and version"
```

### Task 3: `usage --format text`

**Files:**
- Create: `crates/tidemark-cli/src/format/mod.rs`
- Create: `crates/tidemark-cli/src/format/text.rs`
- Modify: `crates/tidemark-cli/src/cli.rs`
- Modify: `crates/tidemark-cli/src/main.rs`

**Interfaces:**
- Produces: `format::select<'a>(&'a [ProviderStatus], Option<&str>, Option<&str>) -> Vec<&'a ProviderStatus>` — the daemon's published order, filtered.
- Produces: `format::dominant(&ProviderStatus) -> Option<Window>` — the window a card leads with, owned.
- Produces: `format::text::render(&[&ProviderStatus], Timestamp) -> String`.
- Produces: `cli::Usage { provider, account, format }` and `cli::Format` (variants added per format task).

- [x] **Step 1: Write the failing tests**

`crates/tidemark-cli/src/format/text.rs`:

```rust
//! Usage as a person reads it.
//!
//! Percentages and spans come from `tidemark_types::present`, which is why a number here
//! and the same number on a card cannot drift apart. Everything else on the line is the
//! provider's own text, printed as it arrived.

use tidemark_types::present::{duration, percent};
use tidemark_types::{ProviderStatus, Timestamp, WindowStatus, provider_label};

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    fn status(state: ProviderState, windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(
            &ProviderId::new("claude".to_owned()),
            &AccountId::default(),
        );
        status.state = state.as_wire().to_owned();
        status.captured_at = Some(1_785_704_400);
        status.windows = windows;
        status
    }

    fn window(resets_at: Option<i64>, used_percent: f64) -> WindowStatus {
        WindowStatus {
            key: "w18000".to_owned(),
            title: "Session".to_owned(),
            subtitle: Some("72 / 100 prompts".to_owned()),
            used_percent,
            resets_at,
            length_secs: Some(18_000),
        }
    }

    /// 1785704400 + 3600.
    const NOW: i64 = 1_785_708_000;

    fn now() -> Timestamp {
        Timestamp::from_unix(NOW).expect("plausible")
    }

    #[test]
    fn a_window_without_a_reset_time_says_nothing_about_one() {
        let status = status(ProviderState::Ok, vec![window(None, 72.0)]);
        let out = render(&[&status], now());
        assert!(out.contains("72%"), "{out}");
        assert!(!out.contains("resets in"), "{out}");
        assert!(!out.contains("pace"), "{out}");
    }

    #[test]
    fn a_reset_time_brings_the_span_and_the_pace() {
        let status = status(ProviderState::Ok, vec![window(Some(NOW + 3_600), 72.0)]);
        let out = render(&[&status], now());
        assert!(out.contains("resets in 1 h"), "{out}");
        assert!(out.contains("outpacing"), "{out}");
    }

    #[test]
    fn a_failed_poll_keeps_the_last_reading_under_its_state() {
        let mut status = status(ProviderState::Unreachable, vec![window(None, 72.0)]);
        status.message = Some("connection timed out".to_owned());
        let out = render(&[&status], now());
        assert!(out.contains("unreachable"), "{out}");
        assert!(out.contains("connection timed out"), "{out}");
        assert!(out.contains("72%"), "{out}");
    }

    #[test]
    fn a_barely_touched_window_never_reads_as_untouched() {
        let status = status(ProviderState::Ok, vec![window(None, 0.2)]);
        assert!(render(&[&status], now()).contains("<1%"));
    }
}
```

The pace assertion is real arithmetic, not a guess: the window is five hours long and
resets in one, so four fifths of it have elapsed, and 72% consumed is behind that — the
window is *not* outpacing. **Resolved when the test ran:** the assertion on `outpacing`
failed against `on pace  (72 / 100 prompts)`, confirming `is_outpacing() == Some(false)`,
so the test now asserts `on pace` and a second one — same window, 95% consumed — covers
the `Some(true)` wording.

- [x] **Step 2: Run and watch it fail**

Run: `cargo test -p tidemark-cli format::text`
Expected: FAIL — `render` is not defined.

- [x] **Step 3: Write the renderer**

Append to `crates/tidemark-cli/src/format/text.rs`, above the test module:

```rust
/// Every selected account, one block each, in the order the daemon published them.
pub fn render(statuses: &[&ProviderStatus], now: Timestamp) -> String {
    let mut out = String::new();
    for status in statuses {
        let label = status
            .account_label
            .as_deref()
            .unwrap_or(status.account.as_str());
        out.push_str(&format!(
            "{} · {}  [{}]\n",
            provider_label(&status.provider),
            label,
            status.state
        ));
        if let Some(plan) = status.plan() {
            out.push_str(&format!("  plan {plan}\n"));
        }
        if let Some(balance) = status.balance() {
            out.push_str(&format!("  balance {balance}\n"));
        }
        if let Some(message) = &status.message {
            out.push_str(&format!("  {message}\n"));
        }
        for window in &status.windows {
            out.push_str(&line(window, now));
        }
        if let Some(captured) = status.captured_at {
            out.push_str(&format!(
                "  read {} ago\n",
                duration(now.as_unix() - captured)
            ));
        }
        out.push('\n');
    }
    out
}

/// One window. The reset span and the pace note appear only when the provider gave enough
/// to compute them; a withheld value is drawn as withheld, never as zero.
fn line(status: &WindowStatus, now: Timestamp) -> String {
    let window = status.to_window();
    let mut line = format!("  {:<20} {:>5}", status.title, percent(window.used_percent));
    if let Some(seconds) = window.seconds_until_reset(now) {
        line.push_str(&format!("  resets in {}", duration(seconds)));
    }
    match window.is_outpacing(now) {
        Some(true) => line.push_str("  outpacing"),
        Some(false) => line.push_str("  on pace"),
        None => {}
    }
    if let Some(subtitle) = &status.subtitle {
        line.push_str(&format!("  ({subtitle})"));
    }
    line.push('\n');
    line
}
```

- [x] **Step 4: Write the selection helpers**

`format::dominant` has no caller until the waybar and guard tasks, so it carries
`#[expect(dead_code, reason = …)]`; the task that first calls it deletes the marker.

`crates/tidemark-cli/src/format/mod.rs`:

```rust
//! Turning what the daemon published into what a caller asked for.

pub mod text;

use tidemark_types::{ProviderStatus, Window};

/// The accounts a `--provider` / `--account` pair names, in the daemon's published order.
///
/// The order is the user's, kept by the daemon and never re-sorted here: a CLI that
/// ordered by urgency would disagree with the grid about which card comes first.
pub fn select<'a>(
    statuses: &'a [ProviderStatus],
    provider: Option<&str>,
    account: Option<&str>,
) -> Vec<&'a ProviderStatus> {
    statuses
        .iter()
        .filter(|status| provider.is_none_or(|slug| status.provider == slug))
        .filter(|status| account.is_none_or(|id| status.account == id))
        .collect()
}

/// The window a card would lead with, cloned out of the rebuilt snapshot so it outlives it.
///
/// `Snapshot::dominant_window` is the shared rule — shortest window unless the provider
/// names a lead one — and reusing it is what keeps the CLI, the card and the tray naming
/// the same window.
pub fn dominant(status: &ProviderStatus) -> Option<Window> {
    let snapshot = status.to_snapshot()?;
    snapshot.dominant_window().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(
            &ProviderId::new(provider.to_owned()),
            &AccountId::new(account.to_owned()),
        )
    }

    #[test]
    fn no_filter_keeps_every_account_in_published_order() {
        let statuses = vec![status("codex", "default"), status("claude", "work")];
        let selected = select(&statuses, None, None);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].provider, "codex");
    }

    #[test]
    fn a_provider_filter_keeps_all_of_its_accounts() {
        let statuses = vec![
            status("claude", "default"),
            status("claude", "work"),
            status("codex", "default"),
        ];
        assert_eq!(select(&statuses, Some("claude"), None).len(), 2);
        assert_eq!(select(&statuses, Some("claude"), Some("work")).len(), 1);
    }

    #[test]
    fn an_account_that_is_not_there_selects_nothing() {
        let statuses = vec![status("claude", "default")];
        assert!(select(&statuses, Some("claude"), Some("nope")).is_empty());
    }
}
```

`AccountId::new` and `ProviderId::new` both take anything that converts into a `String`, so
`AccountId::new("work")` is fine and the `to_owned()` above is optional — `tidemarkd`'s own
tests call them both ways. `is_none_or` is stable in Rust 1.92 and reads better here than
`map_or(true, …)`, which clippy rejects.

- [x] **Step 5: Extend the grammar and wire the command**

In `cli.rs`, add to `Command`:

```rust
    /// What every configured account currently reports.
    Usage(Usage),
```

and, after it:

```rust
#[derive(Debug, clap::Args)]
pub struct Usage {
    /// Only this provider slug.
    #[arg(long)]
    pub provider: Option<String>,
    /// Only this account of that provider.
    #[arg(long, requires = "provider")]
    pub account: Option<String>,
    /// How to print it.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

/// The output shapes. `json` and `waybar` are contracts other programs parse; `text` is
/// for a person and may be reworded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Text,
}
```

In `main.rs`, add `mod format;` and the arm:

```rust
        cli::Command::Usage(args) => {
            let proxy = connect::daemon().await?;
            let statuses = proxy.get_status().await?;
            let selected = format::select(&statuses, args.provider.as_deref(), args.account.as_deref());
            match args.format {
                cli::Format::Text => print!(
                    "{}",
                    format::text::render(&selected, tidemark_types::Timestamp::now())
                ),
            }
            Ok(Exit::Ok)
        }
```

- [x] **Step 6: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [x] **Step 7: Smoke it**

Run: `cargo run -p tidemark-cli -- usage`
Expected: one block per configured account, matching what the window shows.

Run: `cargo run -p tidemark-cli -- usage --provider claude`
Expected: only Claude's accounts.

Run: `cargo run -p tidemark-cli -- usage --account work`
Expected: exit 2 from clap, complaining that `--account` requires `--provider`. clap's own
usage error is code 2, not 64; that is clap's contract and is left alone — `Exit::Usage` is
for arguments this program rejects itself.

- [x] **Step 8: Commit**

```bash
git add crates/tidemark-cli
git commit -m "feat(cli): usage in text form"
```

### Task 4: `usage --format json`

**Files:**
- Create: `crates/tidemark-cli/src/format/json.rs`
- Modify: `crates/tidemark-cli/src/format/mod.rs`, `src/cli.rs`, `src/main.rs`

**Interfaces:**
- Produces: `format::json::render(&[&ProviderStatus]) -> Result<String, serde_json::Error>` emitting `{"accounts": [...]}`.

- [x] **Step 1: Write the failing tests**

`crates/tidemark-cli/src/format/json.rs`:

```rust
//! Usage as another program parses it.
//!
//! An object with one key rather than a bare array, so a later build can add a second key
//! without breaking a plugin that already reads this one — the same reason every published
//! D-Bus structure is a dictionary. The account objects are `ProviderStatus` as serde
//! renders it, which means **absent stays absent**: a provider that withheld a reset time
//! produces no key at all, and no `null` invites a plugin to render it as zero.

use serde::Serialize;
use tidemark_types::ProviderStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState, WindowStatus};

    #[test]
    fn an_account_with_no_reading_publishes_no_reading_keys() {
        let status = ProviderStatus::pending(
            &ProviderId::new("zai".to_owned()),
            &AccountId::default(),
        );
        let text = render(&[&status]).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let account = &value["accounts"][0];
        assert_eq!(account["provider"], "zai");
        assert_eq!(account["state"], "pending");
        assert!(account.get("captured_at").is_none(), "{text}");
        assert!(account.get("message").is_none(), "{text}");
    }

    #[test]
    fn a_window_publishes_the_keys_a_plugin_parses() {
        let mut status = ProviderStatus::pending(
            &ProviderId::new("claude".to_owned()),
            &AccountId::default(),
        );
        status.state = ProviderState::Ok.as_wire().to_owned();
        status.captured_at = Some(1_785_704_400);
        status.windows = vec![WindowStatus {
            key: "w18000".to_owned(),
            title: "Session".to_owned(),
            subtitle: None,
            used_percent: 72.0,
            resets_at: Some(1_785_708_000),
            length_secs: Some(18_000),
        }];
        let text = render(&[&status]).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let window = &value["accounts"][0]["windows"][0];
        assert_eq!(window["key"], "w18000");
        assert_eq!(window["used_percent"], 72.0);
        assert_eq!(window["resets_at"], 1_785_708_000_i64);
        assert_eq!(window["length_secs"], 18_000);
        assert!(window.get("subtitle").is_none(), "{text}");
    }
}
```

These two tests **pin a published contract**, which is their whole point.

**What running them found, and the one deviation in this task:** the key *spellings* are
the field names, as expected — but `SerializeDict` exists to encode `a{sv}`, so under
`serde_json` every value arrives inside its D-Bus envelope:
`"provider": {"signature": "s", "value": "claude"}`, and `details` nests three levels of
them. Publishing that would make a plugin unwrap every scalar. The plan's instruction —
record the shape, never `rename` `tidemark-types` — still holds for spellings, and adding a
second `Serialize` to the wire types is impossible anyway (one `impl` per type) while
mirroring `ProviderStatus` in the CLI would be the duplicated wire model
`crates/tidemark-types/AGENTS.md` forbids. So `format::json::payload` peels the envelope
recursively after `serde_json::to_value`: one wire model, one set of key names, and no
`{"signature","value"}` in what a plugin reads. Absent still stays absent — the derive
omits `None` before the peeling ever sees it. A third test,
`details_arrive_as_plain_objects_all_the_way_down`, pins the deepest published structure.

- [x] **Step 2: Run and watch it fail**

Run: `cargo test -p tidemark-cli format::json`
Expected: FAIL — `render` is not defined.

- [x] **Step 3: Write the renderer**

Above the test module:

```rust
/// The published document.
#[derive(Debug, Serialize)]
struct Document<'a> {
    accounts: &'a [&'a ProviderStatus],
}

pub fn render(statuses: &[&ProviderStatus]) -> Result<String, serde_json::Error> {
    let document = serde_json::to_value(Document { accounts: statuses })?;
    serde_json::to_string_pretty(&payload(document))
}
```

- [x] **Step 4: Wire it up**

`format/mod.rs` gains `pub mod json;`. `cli::Format` gains `Json`. `main.rs`'s match gains:

```rust
                cli::Format::Json => println!("{}", format::json::render(&selected)?),
```

- [x] **Step 5: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [x] **Step 6: Smoke it**

Run: `cargo run -p tidemark-cli -- usage --format json | jq '.accounts[0] | {provider, state, windows}'`
Expected: real values; no `null` where the provider said nothing.

- [x] **Step 7: Commit**

```bash
git add crates/tidemark-cli
git commit -m "feat(cli): usage as json"
```

### Task 5: `usage --format waybar`

**Files:**
- Create: `crates/tidemark-cli/src/format/waybar.rs`
- Modify: `crates/tidemark-cli/src/format/mod.rs`, `src/cli.rs`, `src/main.rs`

**Interfaces:**
- Produces: `format::waybar::render(&[&ProviderStatus], Timestamp) -> Result<String, serde_json::Error>` emitting `{"text","tooltip","class","percentage"}` with `class` always an array.

- [x] **Step 1: Write the failing tests**

`crates/tidemark-cli/src/format/waybar.rs`:

```rust
//! Usage as a Waybar custom module consumes it.
//!
//! The number is the worst dominant window across the selected accounts — the same window
//! a card leads with, so the panel and the grid never disagree about which limit matters.
//! `class` is always an array: a rate-limited account at 95% is `["danger", "stale"]`, and
//! a shape that changed with the situation would make somebody's CSS conditional.

use serde::Serialize;
use tidemark_types::present::{duration, percent};
use tidemark_types::{
    DANGER_AT, ProviderState, ProviderStatus, Timestamp, WARNING_AT, provider_label,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, WindowStatus};

    const NOW: i64 = 1_785_708_000;

    fn now() -> Timestamp {
        Timestamp::from_unix(NOW).expect("plausible")
    }

    fn status(provider: &str, state: ProviderState, used_percent: f64) -> ProviderStatus {
        let mut status = ProviderStatus::pending(
            &ProviderId::new(provider.to_owned()),
            &AccountId::default(),
        );
        status.state = state.as_wire().to_owned();
        status.captured_at = Some(NOW - 60);
        status.windows = vec![WindowStatus {
            key: "w18000".to_owned(),
            title: "Session".to_owned(),
            subtitle: None,
            used_percent,
            resets_at: Some(NOW + 3_600),
            length_secs: Some(18_000),
        }];
        status
    }

    fn card(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid json")
    }

    #[test]
    fn the_worst_account_sets_the_number() {
        let low = status("claude", ProviderState::Ok, 12.0);
        let high = status("codex", ProviderState::Ok, 91.0);
        let card = card(&render(&[&low, &high], now()).expect("serializes"));
        assert_eq!(card["text"], "91%");
        assert_eq!(card["percentage"], 91);
        assert_eq!(card["class"][0], "danger");
    }

    #[test]
    fn a_stale_account_keeps_its_zone() {
        let status = status("claude", ProviderState::RateLimited, 95.0);
        let card = card(&render(&[&status], now()).expect("serializes"));
        assert_eq!(card["class"][0], "danger");
        assert_eq!(card["class"][1], "stale");
    }

    #[test]
    fn nothing_to_report_hides_the_module() {
        let pending = ProviderStatus::pending(
            &ProviderId::new("zai".to_owned()),
            &AccountId::default(),
        );
        let card = card(&render(&[&pending], now()).expect("serializes"));
        assert_eq!(card["text"], "");
        assert_eq!(card["percentage"], 0);
        assert_eq!(card["class"][0], "stale");
    }

    #[test]
    fn a_tooltip_escapes_the_provider_own_text() {
        let mut status = status("claude", ProviderState::Ok, 50.0);
        status.windows[0].title = "Session & overage".to_owned();
        let card = card(&render(&[&status], now()).expect("serializes"));
        let tooltip = card["tooltip"].as_str().expect("a string");
        assert!(tooltip.contains("&amp;"), "{tooltip}");
        assert!(!tooltip.contains("Session & overage"), "{tooltip}");
    }
}
```

- [x] **Step 2: Run and watch it fail**

Run: `cargo test -p tidemark-cli format::waybar`
Expected: FAIL — `render` is not defined.

- [x] **Step 3: Write the renderer**

Above the test module:

```rust
/// What a Waybar custom module reads.
#[derive(Debug, Serialize)]
struct Card {
    text: String,
    tooltip: String,
    class: Vec<String>,
    percentage: u8,
}

pub fn render(
    statuses: &[&ProviderStatus],
    now: Timestamp,
) -> Result<String, serde_json::Error> {
    let worst = statuses
        .iter()
        .filter_map(|status| super::dominant(status))
        .map(|window| window.used_percent)
        .fold(None, |worst: Option<f64>, used| {
            Some(worst.map_or(used, |worst| worst.max(used)))
        });
    let stale = statuses
        .iter()
        .any(|status| ProviderState::from_wire(&status.state) != Some(ProviderState::Ok));

    let card = match worst {
        Some(used) => Card {
            text: percent(used),
            tooltip: tooltip(statuses, now),
            class: classes(Some(used), stale),
            percentage: used.clamp(0.0, 100.0).round() as u8,
        },
        // An empty text hides the module, which is the honest rendering of "no reading
        // yet": a zero would be a claim about quota nobody has measured.
        None => Card {
            text: String::new(),
            tooltip: "Tidemark has no reading yet".to_owned(),
            class: classes(None, true),
            percentage: 0,
        },
    };
    serde_json::to_string(&card)
}

/// The zone first, so a stylesheet keyed on `.danger` keeps working when a second class
/// applies. The boundaries are the shared constants the bar recolours at.
fn classes(used: Option<f64>, stale: bool) -> Vec<String> {
    let mut classes = Vec::new();
    if let Some(used) = used {
        classes.push(
            if used >= DANGER_AT {
                "danger"
            } else if used >= WARNING_AT {
                "warning"
            } else {
                "ok"
            }
            .to_owned(),
        );
    }
    if stale {
        classes.push("stale".to_owned());
    }
    classes
}

fn tooltip(statuses: &[&ProviderStatus], now: Timestamp) -> String {
    statuses
        .iter()
        .map(|status| {
            let mut line = format!(
                "{} · {}",
                provider_label(&status.provider),
                status
                    .account_label
                    .as_deref()
                    .unwrap_or(status.account.as_str())
            );
            if let Some(window) = super::dominant(status) {
                line.push_str(&format!(
                    " — {} {}",
                    window.title,
                    percent(window.used_percent)
                ));
                if let Some(seconds) = window.seconds_until_reset(now) {
                    line.push_str(&format!(", resets in {}", duration(seconds)));
                }
            }
            if ProviderState::from_wire(&status.state) != Some(ProviderState::Ok) {
                line.push_str(&format!(" [{}]", status.state));
            }
            escape(&line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waybar renders a tooltip as Pango markup, and a provider's own window title is text we
/// did not write. Escaping the five predefined entities is the whole of it.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\'' => escaped.push_str("&apos;"),
            '"' => escaped.push_str("&quot;"),
            other => escaped.push(other),
        }
    }
    escaped
}
```

- [x] **Step 4: Wire it up**

`format::dominant` now has callers, so its `#[expect(dead_code, …)]` from Task 3 is
deleted here — `expect` reports an unfulfilled expectation, which is what makes the marker
self-removing.

`format/mod.rs` gains `pub mod waybar;`. `cli::Format` gains `Waybar`. `main.rs`'s match
gains:

```rust
                cli::Format::Waybar => println!(
                    "{}",
                    format::waybar::render(&selected, tidemark_types::Timestamp::now())?
                ),
```

- [x] **Step 5: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [x] **Step 6: Smoke it**

Run: `cargo run -p tidemark-cli -- usage --format waybar | jq .`
Expected: four keys, `class` an array, `percentage` an integer matching `text`.

- [x] **Step 7: Commit**

```bash
git add crates/tidemark-cli
git commit -m "feat(cli): usage as a waybar card"
```

### Task 6: `guard`

**Files:**
- Create: `crates/tidemark-cli/src/guard.rs`
- Modify: `crates/tidemark-cli/src/cli.rs`, `src/main.rs`

**Interfaces:**
- Produces: `guard::Selection { Dominant, Named(String), Any }`, `guard::Verdict { Safe { remaining }, Below { remaining }, Unavailable(String) }` with `fn exit(&self) -> Exit`, and `guard::decide(&[&ProviderStatus], &Selection, u8) -> Verdict`.

- [x] **Step 1: Write the failing tests**

`crates/tidemark-cli/src/guard.rs`:

```rust
//! "May I start a long run right now?", answered with an exit code.
//!
//! Only an `ok` account's reading is judged. A last-good number sitting behind a rejected
//! credential or a rate limit would answer "safe" about quota that cannot be spent, so
//! those exit 69 — unavailable — and a script can tell "no answer" from "no quota".

use tidemark_types::{ProviderState, ProviderStatus};

use crate::exit::Exit;
use crate::format;

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, WindowStatus};

    fn window(key: &str, used_percent: f64) -> WindowStatus {
        WindowStatus {
            key: key.to_owned(),
            title: key.to_owned(),
            subtitle: None,
            used_percent,
            resets_at: None,
            length_secs: Some(18_000),
        }
    }

    fn status(state: ProviderState, windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(
            &ProviderId::new("claude".to_owned()),
            &AccountId::default(),
        );
        status.state = state.as_wire().to_owned();
        status.captured_at = Some(1_785_704_400);
        status.windows = windows;
        status
    }

    #[test]
    fn enough_left_is_safe() {
        let status = status(ProviderState::Ok, vec![window("w18000", 40.0)]);
        let verdict = decide(&[&status], &Selection::Dominant, 20);
        assert_eq!(verdict, Verdict::Safe { remaining: 60.0 });
        assert_eq!(verdict.exit(), Exit::Ok);
    }

    #[test]
    fn too_little_left_is_below() {
        let status = status(ProviderState::Ok, vec![window("w18000", 85.0)]);
        let verdict = decide(&[&status], &Selection::Dominant, 20);
        assert_eq!(verdict, Verdict::Below { remaining: 15.0 });
        assert_eq!(verdict.exit(), Exit::Below);
    }

    #[test]
    fn a_named_window_that_is_not_there_is_unavailable_not_safe() {
        let status = status(ProviderState::Ok, vec![window("w18000", 5.0)]);
        let verdict = decide(&[&status], &Selection::Named("w604800".to_owned()), 20);
        assert_eq!(verdict.exit(), Exit::Unavailable);
    }

    #[test]
    fn a_stale_reading_is_never_judged() {
        let status = status(ProviderState::CredentialRejected, vec![window("w18000", 1.0)]);
        let verdict = decide(&[&status], &Selection::Dominant, 20);
        assert_eq!(verdict.exit(), Exit::Unavailable);
    }

    #[test]
    fn any_window_judges_the_fullest_one() {
        let status = status(
            ProviderState::Ok,
            vec![window("w18000", 10.0), window("w604800", 95.0)],
        );
        assert_eq!(
            decide(&[&status], &Selection::Any, 20).exit(),
            Exit::Below
        );
        assert_eq!(
            decide(&[&status], &Selection::Dominant, 20).exit(),
            Exit::Ok
        );
    }

    #[test]
    fn selecting_no_account_is_unavailable() {
        assert_eq!(decide(&[], &Selection::Dominant, 20).exit(), Exit::Unavailable);
    }
}
```

- [x] **Step 2: Run and watch it fail**

Run: `cargo test -p tidemark-cli guard`
Expected: FAIL — `decide`, `Selection` and `Verdict` are not defined.

- [x] **Step 3: Write the decision**

`Exit::Below` now has a constructor, so its Task 2 `#[expect(dead_code, …)]` is deleted
here. `Exit::Usage`, `Failure::usage` and `Failure::unavailable` keep theirs: `guard`
rejects nothing itself, and clap's own usage errors exit 2.

Above the test module:

```rust
/// Which window the verdict is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The window a card leads with.
    Dominant,
    /// One window by its stable key.
    Named(String),
    /// Every window of every selected account; the fullest one decides.
    Any,
}

/// What the guard concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Safe { remaining: f64 },
    Below { remaining: f64 },
    /// Nothing trustworthy to judge, with the reason for stderr.
    Unavailable(String),
}

impl Verdict {
    pub const fn exit(&self) -> Exit {
        match self {
            Self::Safe { .. } => Exit::Ok,
            Self::Below { .. } => Exit::Below,
            Self::Unavailable(_) => Exit::Unavailable,
        }
    }
}

/// The fullest selected window against the threshold. Every selected account must be `ok`
/// and must have the window asked for; anything else is unavailable rather than a guess.
pub fn decide(statuses: &[&ProviderStatus], selection: &Selection, min_remaining: u8) -> Verdict {
    if statuses.is_empty() {
        return Verdict::Unavailable("no configured account matches".to_owned());
    }

    let mut fullest: Option<f64> = None;
    for status in statuses {
        if ProviderState::from_wire(&status.state) != Some(ProviderState::Ok) {
            return Verdict::Unavailable(format!(
                "{} · {} is {}",
                status.provider, status.account, status.state
            ));
        }
        let used = match selection {
            Selection::Dominant => format::dominant(status).map(|window| window.used_percent),
            Selection::Named(key) => status
                .windows
                .iter()
                .find(|window| &window.key == key)
                .map(|window| window.used_percent),
            Selection::Any => status
                .windows
                .iter()
                .map(|window| window.used_percent)
                .fold(None, |worst: Option<f64>, used| {
                    Some(worst.map_or(used, |worst| worst.max(used)))
                }),
        };
        let Some(used) = used else {
            return Verdict::Unavailable(match selection {
                Selection::Named(key) => format!(
                    "{} · {} reports no window {key}",
                    status.provider, status.account
                ),
                _ => format!("{} · {} has no reading", status.provider, status.account),
            });
        };
        fullest = Some(fullest.map_or(used, |worst: f64| worst.max(used)));
    }

    let remaining = 100.0 - fullest.unwrap_or(100.0);
    if remaining >= f64::from(min_remaining) {
        Verdict::Safe { remaining }
    } else {
        Verdict::Below { remaining }
    }
}
```

- [x] **Step 4: Extend the grammar**

In `cli.rs`, add to `Command`:

```rust
    /// Exit 0 when enough quota is left to start something, 1 when there is not.
    Guard(Guard),
```

and:

```rust
#[derive(Debug, clap::Args)]
pub struct Guard {
    /// Fail when less than this percentage of the window is left.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub min_remaining: u8,
    /// Judge this window key instead of the one a card leads with.
    #[arg(long, conflicts_with = "any")]
    pub window: Option<String>,
    /// Judge every window, not just the leading one.
    #[arg(long)]
    pub any: bool,
    /// Only this provider slug.
    #[arg(long)]
    pub provider: Option<String>,
    /// Only this account of that provider.
    #[arg(long, requires = "provider")]
    pub account: Option<String>,
}
```

- [x] **Step 5: Wire the command**

`main.rs` gains `mod guard;` and:

```rust
        cli::Command::Guard(args) => {
            let proxy = connect::daemon().await?;
            let statuses = proxy.get_status().await?;
            let selected =
                format::select(&statuses, args.provider.as_deref(), args.account.as_deref());
            let selection = match (args.any, args.window) {
                (true, _) => guard::Selection::Any,
                (false, Some(key)) => guard::Selection::Named(key),
                (false, None) => guard::Selection::Dominant,
            };
            let verdict = guard::decide(&selected, &selection, args.min_remaining);
            match &verdict {
                guard::Verdict::Safe { remaining } | guard::Verdict::Below { remaining } => {
                    println!("{} left", tidemark_types::present::percent(*remaining));
                }
                guard::Verdict::Unavailable(reason) => eprintln!("{reason}"),
            }
            Ok(verdict.exit())
        }
```

- [x] **Step 6: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [x] **Step 7: Smoke it**

```bash
cargo run -p tidemark-cli -- guard --min-remaining 1 --provider claude; echo $?
cargo run -p tidemark-cli -- guard --min-remaining 100 --provider claude; echo $?
cargo run -p tidemark-cli -- guard --min-remaining 20 --window nope --provider claude; echo $?
cargo run -p tidemark-cli -- guard --min-remaining 900; echo $?
```
Expected: `0`, `1`, `69`, and clap's own `2` for the out-of-range threshold.

- [x] **Step 8: Commit**

```bash
git add crates/tidemark-cli
git commit -m "feat(cli): guard a long run behind an exit code"
```

---

## Phase C — streaming

### Task 7: the event lines and the client-side mirror

**Files:**
- Create: `crates/tidemark-cli/src/watch/mod.rs`
- Create: `crates/tidemark-cli/src/watch/mirror.rs`

**Interfaces:**
- Produces: `watch::Event<'a>` serializing as `{"event":"<kind>", …}`, and `watch::line(&Event) -> Result<String, serde_json::Error>`.
- Produces: `watch::mirror::Change { Upsert(ProviderStatus), Remove { provider, account }, Order(Vec<String>) }` and `watch::mirror::apply(&mut Vec<ProviderStatus>, Change)`.

- [ ] **Step 1: Write the failing mirror tests**

`crates/tidemark-cli/src/watch/mirror.rs`:

```rust
//! The client-side copy of what the daemon published.
//!
//! `watch --format waybar` has to re-render one card from *every* account after each
//! signal, so the stream keeps a mirror. The rules are the daemon's own: a change to a
//! known `(provider, account)` replaces it where it stands, an unknown one goes on the end,
//! and an order announcement is a permutation of provider slugs rather than a new set — a
//! status carries no position, so the sequence has to be applied separately.

use tidemark_types::ProviderStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(
            &ProviderId::new(provider.to_owned()),
            &AccountId::new(account.to_owned()),
        )
    }

    #[test]
    fn a_change_replaces_the_account_where_it_stands() {
        let mut statuses = vec![status("claude", "default"), status("codex", "default")];
        let mut changed = status("claude", "default");
        changed.state = ProviderState::Ok.as_wire().to_owned();

        apply(&mut statuses, Change::Upsert(changed));

        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].provider, "claude");
        assert_eq!(statuses[0].state, "ok");
    }

    #[test]
    fn an_account_nobody_has_seen_goes_on_the_end() {
        let mut statuses = vec![status("claude", "default")];
        apply(&mut statuses, Change::Upsert(status("claude", "work")));
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[1].account, "work");
    }

    #[test]
    fn a_removal_drops_exactly_one_account() {
        let mut statuses = vec![status("claude", "default"), status("claude", "work")];
        apply(
            &mut statuses,
            Change::Remove {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
            },
        );
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].account, "work");
    }

    #[test]
    fn an_order_permutes_providers_and_keeps_accounts_together() {
        let mut statuses = vec![
            status("claude", "default"),
            status("codex", "default"),
            status("claude", "work"),
        ];
        apply(
            &mut statuses,
            Change::Order(vec!["codex".to_owned(), "claude".to_owned()]),
        );
        assert_eq!(statuses[0].provider, "codex");
        assert_eq!(statuses[1].account, "default");
        assert_eq!(statuses[2].account, "work");
    }
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tidemark-cli watch::mirror`
Expected: FAIL — `apply` and `Change` are not defined.

- [ ] **Step 3: Write the mirror**

Above the test module:

```rust
/// One announced change.
#[derive(Debug)]
pub enum Change {
    Upsert(ProviderStatus),
    Remove { provider: String, account: String },
    Order(Vec<String>),
}

pub fn apply(statuses: &mut Vec<ProviderStatus>, change: Change) {
    match change {
        Change::Upsert(status) => {
            match statuses
                .iter_mut()
                .find(|held| held.provider == status.provider && held.account == status.account)
            {
                Some(held) => *held = status,
                None => statuses.push(status),
            }
        }
        Change::Remove { provider, account } => {
            statuses.retain(|held| !(held.provider == provider && held.account == account));
        }
        // A stable sort, so accounts keep the order the daemon gave them inside their
        // provider. A provider the announcement does not name sorts last rather than
        // disappearing.
        Change::Order(providers) => statuses.sort_by_key(|status| {
            providers
                .iter()
                .position(|slug| *slug == status.provider)
                .unwrap_or(usize::MAX)
        }),
    }
}
```

- [ ] **Step 4: Write the failing event-line tests**

`crates/tidemark-cli/src/watch/mod.rs`:

```rust
//! The daemon's signals as a line-oriented stream.
//!
//! One JSON object per line, flushed as it is written, so a plugin can read a pipe instead
//! of polling a snapshot every few seconds. The first line of a connection is always a
//! snapshot, and so is the first line after a reconnect: whatever the daemon published
//! while nothing was listening was announced to nobody, and re-reading is the only honest
//! recovery.

pub mod mirror;

use serde::Serialize;
use tidemark_types::{DataInfo, Preferences, ProviderStatus};

/// One line of the stream.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event<'a> {
    Snapshot { accounts: &'a [ProviderStatus] },
    Changed { account: &'a ProviderStatus },
    Removed { provider: &'a str, account: &'a str },
    Order { providers: &'a [String] },
    Preferences { preferences: &'a Preferences },
    Data { data: &'a DataInfo },
    Update { version: &'a str },
    Waiting,
}

pub fn line(event: &Event<'_>) -> Result<String, serde_json::Error> {
    serde_json::to_string(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    #[test]
    fn a_snapshot_names_itself_and_carries_every_account() {
        let accounts = vec![ProviderStatus::pending(
            &ProviderId::new("zai".to_owned()),
            &AccountId::default(),
        )];
        let text = line(&Event::Snapshot {
            accounts: &accounts,
        })
        .expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["event"], "snapshot");
        assert_eq!(value["accounts"][0]["provider"], "zai");
        assert!(!text.contains('\n'), "one event is one line: {text}");
    }

    #[test]
    fn a_removal_names_the_account_that_went_away() {
        let text = line(&Event::Removed {
            provider: "claude",
            account: "work",
        })
        .expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["event"], "removed");
        assert_eq!(value["provider"], "claude");
        assert_eq!(value["account"], "work");
    }

    #[test]
    fn waiting_is_a_line_of_its_own() {
        let text = line(&Event::Waiting).expect("serializes");
        assert_eq!(text, r#"{"event":"waiting"}"#);
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p tidemark-cli watch`
Expected: PASS. `main.rs` needs `mod watch;` for the module to be compiled at all.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark-cli
git commit -m "feat(cli): the watch stream's events and mirror"
```

### Task 8: `watch`, the stream and the reconnect

**Files:**
- Modify: `crates/tidemark-cli/src/watch/mod.rs`
- Modify: `crates/tidemark-cli/src/cli.rs`, `src/main.rs`
- Create: `crates/tidemark-cli/tests/fake/mod.rs`
- Create: `crates/tidemark-cli/tests/watching_a_fake_daemon.rs`

**Interfaces:**
- Consumes: `watch::Event`, `watch::line`, `watch::mirror::{apply, Change}`, `format::waybar::render`, `connect::daemon`.
- Produces: `watch::pump(&DaemonProxy<'_>, Sink, &mut impl Write) -> zbus::Result<()>` — one connection's worth of streaming.
- Produces: `watch::run(Sink) -> Result<Exit, Failure>` — the connect/retry loop, never returning under normal operation.
- Produces: `tests::fake::FakeDaemon` and `tests::fake::serve(name) -> (zbus::Connection, DaemonProxy<'static>)`, reused by Tasks 9-11.

- [ ] **Step 1: Write the pump and the retry loop**

Append to `crates/tidemark-cli/src/watch/mod.rs`. The structure mirrors
`crates/tidemark/src/bus.rs:249-357` (`serve`) — read it first — minus the GLib half and
minus the `cfg(windows)` transport, which stays the window's business:

```rust
use std::io::Write;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use tidemark_ipc::DaemonProxy;
use tidemark_types::Timestamp;
use zbus::export::futures_core::Stream;

use crate::exit::{Exit, Failure};
use crate::{connect, format};

/// How long to wait before trying the bus again. Only reached when the *bus* is
/// unreachable; a daemon that is merely not running is waited for by name.
const RETRY: Duration = Duration::from_secs(5);

/// What the stream prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// One JSON event per line.
    Events,
    /// One Waybar card per change, re-rendered from the mirror.
    Waybar,
}

/// Streams until the process is killed. Each connection's events are printed, and the loop
/// re-reads everything after a gap.
pub async fn run(sink: Sink, provider: Option<String>) -> Result<Exit, Failure> {
    let mut out = std::io::stdout();
    loop {
        match connect::daemon().await {
            Ok(proxy) => {
                if let Err(error) = pump(&proxy, sink, provider.as_deref(), &mut out).await {
                    eprintln!("{error}");
                }
            }
            Err(error) => eprintln!("{error}"),
        }
        emit(&mut out, &Event::Waiting)?;
        async_io::Timer::after(RETRY).await;
    }
}

/// One connection's worth of streaming. Returns when a stream ends, which on the session
/// bus means the connection is finished with.
pub async fn pump(
    proxy: &DaemonProxy<'_>,
    sink: Sink,
    provider: Option<&str>,
    out: &mut impl Write,
) -> zbus::Result<()> {
    // Subscribed before the first GetStatus, so a poll finishing between the two arrives as
    // a signal rather than being missed by both.
    let mut owner = pin!(proxy.inner().receive_owner_changed().await?);
    let mut changes = pin!(proxy.receive_provider_changed().await?);
    let mut removals = pin!(proxy.receive_provider_removed().await?);
    let mut orders = pin!(proxy.receive_order_changed().await?);
    let mut updates = pin!(proxy.receive_update_changed().await?);
    let mut preferences = pin!(proxy.receive_preferences_changed().await?);
    let mut data = pin!(proxy.receive_data_changed().await?);

    let mut statuses = proxy.get_status().await?;
    publish(out, sink, provider, &statuses, &Event::Snapshot { accounts: &statuses })?;

    loop {
        let event = std::future::poll_fn(|context| {
            if let Poll::Ready(owner) = owner.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Owner(owner));
            }
            if let Poll::Ready(change) = changes.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Changed(change));
            }
            if let Poll::Ready(removal) = removals.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Removed(removal));
            }
            if let Poll::Ready(order) = orders.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Order(order));
            }
            if let Poll::Ready(update) = updates.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Update(update));
            }
            if let Poll::Ready(changed) = preferences.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Preferences(changed));
            }
            if let Poll::Ready(changed) = data.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Data(changed));
            }
            Poll::Pending
        })
        .await;

        match event {
            // The daemon appeared, or a newer one replaced it. Re-read: what it published
            // while nothing was listening was announced to nobody.
            Signal::Owner(Some(Some(_))) => {
                statuses = proxy.get_status().await?;
                publish(
                    out,
                    sink,
                    provider,
                    &statuses,
                    &Event::Snapshot { accounts: &statuses },
                )?;
            }
            Signal::Owner(Some(None)) => publish(out, sink, provider, &statuses, &Event::Waiting)?,
            Signal::Changed(Some(signal)) => {
                let status = signal.args()?.status;
                publish(out, sink, provider, &statuses, &Event::Changed { account: &status })?;
                mirror::apply(&mut statuses, mirror::Change::Upsert(status));
                republish(out, sink, provider, &statuses)?;
            }
            Signal::Removed(Some(signal)) => {
                let args = signal.args()?;
                publish(
                    out,
                    sink,
                    provider,
                    &statuses,
                    &Event::Removed {
                        provider: args.provider,
                        account: args.account,
                    },
                )?;
                mirror::apply(
                    &mut statuses,
                    mirror::Change::Remove {
                        provider: args.provider.to_owned(),
                        account: args.account.to_owned(),
                    },
                );
                republish(out, sink, provider, &statuses)?;
            }
            Signal::Order(Some(signal)) => {
                let providers = signal.args()?.providers;
                publish(
                    out,
                    sink,
                    provider,
                    &statuses,
                    &Event::Order {
                        providers: &providers,
                    },
                )?;
                mirror::apply(&mut statuses, mirror::Change::Order(providers));
                republish(out, sink, provider, &statuses)?;
            }
            Signal::Update(Some(signal)) => publish(
                out,
                sink,
                provider,
                &statuses,
                &Event::Update {
                    version: signal.args()?.version,
                },
            )?,
            Signal::Preferences(Some(signal)) => {
                let args = signal.args()?;
                publish(
                    out,
                    sink,
                    provider,
                    &statuses,
                    &Event::Preferences {
                        preferences: &args.preferences,
                    },
                )?;
            }
            Signal::Data(Some(signal)) => {
                let args = signal.args()?;
                publish(out, sink, provider, &statuses, &Event::Data { data: &args.data })?;
            }
            // Any stream ending means this connection is done.
            Signal::Owner(None)
            | Signal::Changed(None)
            | Signal::Removed(None)
            | Signal::Order(None)
            | Signal::Update(None)
            | Signal::Preferences(None)
            | Signal::Data(None) => return Ok(()),
        }
    }
}

/// Which stream produced something.
enum Signal {
    Owner(Option<Option<zbus::names::OwnedUniqueName>>),
    Changed(Option<ProviderChanged>),
    Removed(Option<ProviderRemoved>),
    Order(Option<OrderChanged>),
    Update(Option<UpdateChanged>),
    Preferences(Option<PreferencesChanged>),
    Data(Option<DataChanged>),
}

/// In event mode, one line per event; in Waybar mode, events are silent and only the card
/// is printed, from `republish`.
fn publish(
    out: &mut impl Write,
    sink: Sink,
    provider: Option<&str>,
    statuses: &[ProviderStatus],
    event: &Event<'_>,
) -> Result<(), std::io::Error> {
    match sink {
        Sink::Events => emit(out, event),
        Sink::Waybar => match event {
            Event::Snapshot { .. } | Event::Waiting => card(out, provider, statuses),
            _ => Ok(()),
        },
    }
}

/// After a change has been applied to the mirror, a Waybar module wants the whole card
/// again: its text is the worst window across every account, not the one that changed.
fn republish(
    out: &mut impl Write,
    sink: Sink,
    provider: Option<&str>,
    statuses: &[ProviderStatus],
) -> Result<(), std::io::Error> {
    match sink {
        Sink::Events => Ok(()),
        Sink::Waybar => card(out, provider, statuses),
    }
}

fn card(
    out: &mut impl Write,
    provider: Option<&str>,
    statuses: &[ProviderStatus],
) -> Result<(), std::io::Error> {
    let selected = format::select(statuses, provider, None);
    let rendered = format::waybar::render(&selected, Timestamp::now())
        .map_err(std::io::Error::other)?;
    writeln!(out, "{rendered}")?;
    out.flush()
}

/// Written and flushed per line: stdout is block-buffered when it is a pipe, and a plugin
/// reading a pipe must not wait for a buffer to fill.
fn emit(out: &mut impl Write, event: &Event<'_>) -> Result<(), std::io::Error> {
    writeln!(out, "{}", line(event).map_err(std::io::Error::other)?)?;
    out.flush()
}
```

The `Signal` variants name the generated signal-argument types (`ProviderChanged`,
`ProviderRemoved`, …). Import them from `tidemark_ipc`: the `#[zbus::proxy]` macro
generates one type per signal beside the proxy, named after the signal in CamelCase. They
are public where the trait is, so `use tidemark_ipc::{DataChanged, OrderChanged,
PreferencesChanged, ProviderChanged, ProviderRemoved, UpdateChanged};` is the import. If a
name does not resolve, read the compiler's suggestion rather than guessing — the macro's
naming is the contract here, not ours.

`emit` returning `std::io::Error` and `run` returning `Failure` need
`impl From<std::io::Error> for Failure` in `exit.rs`:

```rust
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self {
            exit: Exit::Unavailable,
            message: format!("cannot write the stream: {error}"),
        }
    }
}
```

- [ ] **Step 2: Extend the grammar and wire the command**

`cli.rs` gains:

```rust
    /// Print the daemon's changes as they arrive, one JSON object per line.
    Watch(Watch),
```

```rust
#[derive(Debug, clap::Args)]
pub struct Watch {
    /// Only this provider slug.
    #[arg(long)]
    pub provider: Option<String>,
    /// `json` prints one event per line; `waybar` prints a card after every change.
    #[arg(long, value_enum, default_value_t = StreamFormat::Json)]
    pub format: StreamFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StreamFormat {
    Json,
    Waybar,
}
```

`main.rs` gains:

```rust
        cli::Command::Watch(args) => {
            let sink = match args.format {
                cli::StreamFormat::Json => watch::Sink::Events,
                cli::StreamFormat::Waybar => watch::Sink::Waybar,
            };
            watch::run(sink, args.provider).await
        }
```

- [ ] **Step 3: Write the fake daemon the integration tests share**

`crates/tidemark-cli/tests/fake/mod.rs`:

```rust
//! A daemon that answers, and records what it was asked.
//!
//! The commands take a proxy, so a test drives the real command bodies against this
//! instead of the machine's own daemon. There is deliberately no environment variable that
//! redirects the bus name: testability comes from the parameter, not from a back door a
//! user could trip over.

use std::sync::{Arc, Mutex};

use tidemark_ipc::DaemonProxy;
use tidemark_types::{
    AccountId, ProviderDefinition, ProviderId, ProviderState, ProviderStatus, ids,
};

#[derive(Debug, Default, Clone)]
pub struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    pub fn record(&self, call: impl Into<String>) {
        self.0.lock().expect("not poisoned").push(call.into());
    }

    pub fn recorded(&self) -> Vec<String> {
        self.0.lock().expect("not poisoned").clone()
    }
}

pub struct FakeDaemon {
    pub calls: Calls,
    pub statuses: Vec<ProviderStatus>,
}

impl FakeDaemon {
    pub fn with_one_account() -> Self {
        let mut status = ProviderStatus::pending(
            &ProviderId::new("claude".to_owned()),
            &AccountId::default(),
        );
        status.state = ProviderState::Ok.as_wire().to_owned();
        Self {
            calls: Calls::default(),
            statuses: vec![status],
        }
    }
}

#[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
impl FakeDaemon {
    async fn get_status(&self) -> Vec<ProviderStatus> {
        self.calls.record("GetStatus");
        self.statuses.clone()
    }

    async fn list_providers(&self) -> Vec<ProviderDefinition> {
        self.calls.record("ListProviders");
        Vec::new()
    }

    async fn add_provider(&self, provider: &str) {
        self.calls.record(format!("AddProvider({provider})"));
    }

    async fn remove_provider(&self, provider: &str, account: &str) {
        self.calls
            .record(format!("RemoveProvider({provider},{account})"));
    }

    async fn add_account(&self, provider: &str, account: &str) {
        self.calls.record(format!("AddAccount({provider},{account})"));
    }

    async fn rename_account(&self, provider: &str, account: &str, new: &str) {
        self.calls
            .record(format!("RenameAccount({provider},{account},{new})"));
    }

    async fn set_order(&self, providers: Vec<String>) {
        self.calls.record(format!("SetOrder({})", providers.join(",")));
    }

    async fn set_account_order(&self, provider: &str, accounts: Vec<String>) {
        self.calls.record(format!(
            "SetAccountOrder({provider},{})",
            accounts.join(",")
        ));
    }

    async fn refresh(&self, provider: &str) {
        self.calls.record(format!("Refresh({provider})"));
    }

    async fn set_key(&self, provider: &str, account: &str, key: &str) {
        // The key's *length* is recorded, never the key: a test that printed a secret
        // would teach the next person that printing secrets is fine.
        self.calls.record(format!(
            "SetKey({provider},{account},{} chars)",
            key.chars().count()
        ));
    }

    async fn sign_out(&self, provider: &str, account: &str) {
        self.calls.record(format!("SignOut({provider},{account})"));
    }

    async fn set_option(&self, provider: &str, account: &str, name: &str, value: &str) {
        self.calls
            .record(format!("SetOption({provider},{account},{name},{value})"));
    }

    async fn set_window_notify(&self, provider: &str, account: &str, window: &str, enabled: bool) {
        self.calls.record(format!(
            "SetWindowNotify({provider},{account},{window},{enabled})"
        ));
    }

    async fn set_proxy(&self, mode: &str, host: &str, port: u16) {
        self.calls.record(format!("SetProxy({mode},{host},{port})"));
    }

    #[zbus(signal)]
    async fn provider_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        status: ProviderStatus,
    ) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "false"))]
    async fn version(&self) -> String {
        "0.0.0-test".to_owned()
    }
}

/// Serves one fake daemon under a unique name and returns a proxy pointed at it.
pub async fn serve(daemon: FakeDaemon) -> Option<(zbus::Connection, DaemonProxy<'static>, Calls)> {
    let calls = daemon.calls.clone();
    let client = zbus::Connection::session().await.ok()?;
    let name = format!(
        "io.github.zbndev.TidemarkCliTest{}x{}",
        std::process::id(),
        calls.recorded().len()
    );
    let server = zbus::connection::Builder::session()
        .expect("session builder")
        .name(name.as_str())
        .expect("unique test name")
        .serve_at(ids::OBJECT_PATH, daemon)
        .expect("serves the object")
        .build()
        .await
        .expect("test service starts");
    let proxy = DaemonProxy::builder(&client)
        .destination(name)
        .expect("valid destination")
        .build()
        .await
        .expect("proxy builds");
    Some((server, proxy, calls))
}
```

A unique bus name per test matters: `cargo test` runs tests in parallel threads on one
session bus, and two services under the same name would make the second one fail.
Include the test's own name in the string if collisions still happen.

- [ ] **Step 4: Write the failing integration test**

`crates/tidemark-cli/tests/watching_a_fake_daemon.rs`:

```rust
mod fake;

use tidemark_types::{AccountId, ProviderId, ProviderState, ProviderStatus};

#[test]
fn the_stream_opens_with_a_snapshot_and_follows_every_change() {
    async_io::block_on(async {
        let Some((server, proxy, _calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let mut out = Vec::new();
        // One connection's worth of streaming, ended by dropping the service below.
        let pumping = async {
            let _ = tidemark_cli::watch::pump(
                &proxy,
                tidemark_cli::watch::Sink::Events,
                None,
                &mut out,
            )
            .await;
            out
        };

        let emitting = async {
            let mut changed = ProviderStatus::pending(
                &ProviderId::new("claude".to_owned()),
                &AccountId::default(),
            );
            changed.state = ProviderState::RateLimited.as_wire().to_owned();
            // Give the pump a moment to subscribe before the signal goes out.
            async_io::Timer::after(std::time::Duration::from_millis(200)).await;
            fake::emit_change(&server, changed).await;
            async_io::Timer::after(std::time::Duration::from_millis(200)).await;
            drop(server);
        };

        let (printed, ()) = futures_lite::future::zip(pumping, emitting).await;
        let text = String::from_utf8(printed).expect("utf-8");
        let mut lines = text.lines();
        let snapshot: serde_json::Value =
            serde_json::from_str(lines.next().expect("a snapshot line")).expect("json");
        assert_eq!(snapshot["event"], "snapshot");
        assert_eq!(snapshot["accounts"][0]["provider"], "claude");

        let change: serde_json::Value =
            serde_json::from_str(lines.next().expect("a change line")).expect("json");
        assert_eq!(change["event"], "changed");
        assert_eq!(change["account"]["state"], "rate-limited");
    });
}
```

This needs three things the crate does not have yet:

1. A library target, so an integration test can call `watch::pump`. Add to
   `crates/tidemark-cli/Cargo.toml`:

   ```toml
   [lib]
   name = "tidemark_cli"
   path = "src/lib.rs"
   ```

   Create `src/lib.rs` holding the module declarations and doc comment currently at the top
   of `main.rs` (`pub mod cli; pub mod connect; pub mod exit; pub mod format; pub mod guard;
   pub mod watch;`), and reduce `main.rs` to `use tidemark_cli::…;` plus `fn main` and `run`.
2. `fake::emit_change(&server, status)`, which emits the signal from the served object:

   ```rust
   pub async fn emit_change(server: &zbus::Connection, status: ProviderStatus) {
       let emitter = zbus::object_server::SignalEmitter::new(server, ids::OBJECT_PATH)
           .expect("valid emitter");
       FakeDaemon::provider_changed(&emitter, status)
           .await
           .expect("signal goes out");
   }
   ```
3. `cargo add -p tidemark-cli --dev futures-lite` for `zip`.

- [ ] **Step 5: Run it**

Run: `cargo test -p tidemark-cli --test watching_a_fake_daemon`
Expected: PASS, or the skip line with no session bus.

- [ ] **Step 6: Smoke it against the live daemon**

In one terminal: `cargo run -p tidemark-cli -- watch`
In another: `cargo run -p tidemark-cli -- refresh claude`
Expected: a `snapshot` line at once, then a `changed` line per poll. Then
`systemctl --user restart tidemarkd` and expect a `waiting` line followed by a fresh
`snapshot`.

Also: `cargo run -p tidemark-cli -- watch --format waybar | head -3`
Expected: one card per change, each a complete JSON object on its own line. `head` closing
the pipe must not hang the process.

- [ ] **Step 7: Full gate and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemark-cli Cargo.lock
git commit -m "feat(cli): watch the daemon as a stream"
```

---

## Phase D — everything the window can do

### Task 9: providers, accounts and `refresh`

**Files:**
- Create: `crates/tidemark-cli/src/commands/mod.rs`
- Create: `crates/tidemark-cli/src/commands/provider.rs`
- Create: `crates/tidemark-cli/src/commands/account.rs`
- Create: `crates/tidemark-cli/tests/managing_a_fake_daemon.rs`
- Modify: `crates/tidemark-cli/src/lib.rs`, `src/cli.rs`, `src/main.rs`

**Interfaces:**
- Produces: `commands::provider::run(&DaemonProxy<'_>, cli::ProviderCommand) -> Result<Exit, Failure>` and `commands::account::run(&DaemonProxy<'_>, cli::AccountCommand) -> Result<Exit, Failure>`.

- [ ] **Step 1: Extend the grammar**

In `cli.rs`, add to `Command`:

```rust
    /// The provider catalog, the configured set, and what is in it.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// The accounts one provider carries.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Poll now: one provider, or everything.
    Refresh {
        /// A provider slug. Omitted, every configured account is polled.
        provider: Option<String>,
    },
```

and the two subcommand enums:

```rust
#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Every provider this build knows how to configure.
    Catalog,
    /// Every configured account, with its state.
    List,
    /// Configure a provider, creating its default account.
    Add { provider: String },
    /// Remove one configured account, its credentials and its card.
    Rm { provider: String, account: String },
    /// Rewrite the order the cards go in. Must name every configured provider.
    Order { providers: Vec<String> },
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Add one more account to a provider the config already has.
    Add { provider: String, account: String },
    /// Remove one account. The same call as `provider rm`.
    Rm { provider: String, account: String },
    /// Rename an account, carrying its credential and history to the new id.
    Rename {
        provider: String,
        account: String,
        new: String,
    },
    /// Rewrite one provider's account order.
    Order {
        provider: String,
        accounts: Vec<String>,
    },
}
```

- [ ] **Step 2: Write the failing integration test**

`crates/tidemark-cli/tests/managing_a_fake_daemon.rs`:

```rust
mod fake;

use tidemark_cli::cli::{AccountCommand, ProviderCommand};
use tidemark_cli::commands;

#[test]
fn adding_and_ordering_reach_the_daemon_with_the_arguments_given() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::provider::run(
            &proxy,
            ProviderCommand::Add {
                provider: "codex".to_owned(),
            },
        )
        .await
        .expect("add succeeds");

        commands::provider::run(
            &proxy,
            ProviderCommand::Order {
                providers: vec!["codex".to_owned(), "claude".to_owned()],
            },
        )
        .await
        .expect("order succeeds");

        commands::account::run(
            &proxy,
            AccountCommand::Rename {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
                new: "work".to_owned(),
            },
        )
        .await
        .expect("rename succeeds");

        assert_eq!(
            calls.recorded(),
            vec![
                "AddProvider(codex)".to_owned(),
                "SetOrder(codex,claude)".to_owned(),
                "RenameAccount(claude,default,work)".to_owned(),
            ]
        );
        drop(server);
    });
}

#[test]
fn an_order_with_no_providers_is_refused_before_the_daemon_hears_it() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let failure = commands::provider::run(
            &proxy,
            ProviderCommand::Order {
                providers: Vec::new(),
            },
        )
        .await
        .expect_err("an empty order is not a permutation");

        assert_eq!(failure.exit, tidemark_cli::exit::Exit::Usage);
        assert!(calls.recorded().is_empty(), "{:?}", calls.recorded());
        drop(server);
    });
}
```

- [ ] **Step 3: Run and watch it fail**

Run: `cargo test -p tidemark-cli --test managing_a_fake_daemon`
Expected: FAIL — `commands` does not exist.

- [ ] **Step 4: Write the commands**

`crates/tidemark-cli/src/commands/mod.rs`:

```rust
//! One module per entity the daemon owns. Every function takes the proxy rather than
//! building one, which is what makes them testable against a fake daemon.

pub mod account;
pub mod provider;
```

`crates/tidemark-cli/src/commands/provider.rs`:

```rust
use tidemark_ipc::DaemonProxy;
use tidemark_types::provider_label;

use crate::cli::ProviderCommand;
use crate::exit::{Exit, Failure};

pub async fn run(
    proxy: &DaemonProxy<'_>,
    command: ProviderCommand,
) -> Result<Exit, Failure> {
    match command {
        ProviderCommand::Catalog => {
            for definition in proxy.list_providers().await? {
                println!(
                    "{:<20} {:<24} {}",
                    definition.provider, definition.title, definition.credential
                );
            }
        }
        ProviderCommand::List => {
            for status in proxy.get_status().await? {
                println!(
                    "{:<20} {:<12} {:<20} {}",
                    status.provider,
                    status.account,
                    provider_label(&status.provider),
                    status.state
                );
            }
        }
        ProviderCommand::Add { provider } => proxy.add_provider(&provider).await?,
        ProviderCommand::Rm { provider, account } => {
            proxy.remove_provider(&provider, &account).await?
        }
        // The daemon takes an order as a permutation of the configured set and refuses a
        // partial one; refusing an empty list here keeps a mistyped shell glob from
        // becoming a D-Bus error the user has to interpret.
        ProviderCommand::Order { providers } => {
            if providers.is_empty() {
                return Err(Failure::usage(
                    "provider order needs the whole configured set, in the order you want",
                ));
            }
            proxy.set_order(&providers).await?
        }
    }
    Ok(Exit::Ok)
}
```

`crates/tidemark-cli/src/commands/account.rs`:

```rust
use tidemark_ipc::DaemonProxy;

use crate::cli::AccountCommand;
use crate::exit::{Exit, Failure};

pub async fn run(proxy: &DaemonProxy<'_>, command: AccountCommand) -> Result<Exit, Failure> {
    match command {
        AccountCommand::Add { provider, account } => {
            proxy.add_account(&provider, &account).await?
        }
        AccountCommand::Rm { provider, account } => {
            proxy.remove_provider(&provider, &account).await?
        }
        AccountCommand::Rename {
            provider,
            account,
            new,
        } => proxy.rename_account(&provider, &account, &new).await?,
        AccountCommand::Order { provider, accounts } => {
            if accounts.is_empty() {
                return Err(Failure::usage(
                    "account order needs every account of that provider, in the order you want",
                ));
            }
            proxy.set_account_order(&provider, accounts).await?
        }
    }
    Ok(Exit::Ok)
}
```

`main.rs` gains the three arms:

```rust
        cli::Command::Provider { command } => {
            let proxy = connect::daemon().await?;
            commands::provider::run(&proxy, command).await
        }
        cli::Command::Account { command } => {
            let proxy = connect::daemon().await?;
            commands::account::run(&proxy, command).await
        }
        cli::Command::Refresh { provider } => {
            let proxy = connect::daemon().await?;
            proxy.refresh(provider.as_deref().unwrap_or("")).await?;
            Ok(Exit::Ok)
        }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [ ] **Step 6: Smoke it**

```bash
cargo run -p tidemark-cli -- provider catalog | head -5
cargo run -p tidemark-cli -- provider list
cargo run -p tidemark-cli -- refresh
```
Expected: the catalog, the configured set, and a poll that shows up in `watch`.

Then verify a real mutation round trip on a provider you do not mind touching:

```bash
cargo run -p tidemark-cli -- provider add wayfinder
cargo run -p tidemark-cli -- provider list | grep wayfinder
cargo run -p tidemark-cli -- provider rm wayfinder default
```
Expected: the card appears in the running window and then goes away. Wayfinder needs no
credential, which is why it is the safe one to test with.

- [ ] **Step 7: Commit**

```bash
git add crates/tidemark-cli Cargo.lock
git commit -m "feat(cli): manage providers and accounts"
```

### Task 10: credentials

**Files:**
- Create: `crates/tidemark-cli/src/secret.rs`
- Create: `crates/tidemark-cli/src/commands/auth.rs`
- Create: `crates/tidemark-cli/tests/storing_a_secret.rs`
- Modify: `crates/tidemark-cli/src/lib.rs`, `src/cli.rs`, `src/main.rs`, `src/commands/mod.rs`

**Interfaces:**
- Produces: `secret::Source { Stdin, File(PathBuf) }` and `secret::read(Source) -> Result<String, Failure>`.
- Produces: `commands::auth::run(&DaemonProxy<'_>, cli::AuthCommand) -> Result<Exit, Failure>`.

- [ ] **Step 1: Write the failing secret-input tests**

`crates/tidemark-cli/src/secret.rs`:

```rust
//! Where a secret is allowed to come from.
//!
//! stdin or a file, never an argv value: `/proc/<pid>/cmdline` is readable by every process
//! of the same user, and a key typed once ends up in shell history. stdin is a file
//! descriptor rather than a terminal, so a plugin with its own UI writes into a pipe and
//! never opens a console.

use std::io::Read;
use std::path::PathBuf;

use crate::exit::Failure;

/// One trailing newline is trimmed — `printf` users and `echo` users should get the same
/// key — and nothing else is touched: a key with meaningful interior characters is the
/// provider's business, not ours.
pub fn read(source: Source) -> Result<String, Failure> {
    let raw = match source {
        Source::Stdin => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|error| Failure::usage(format!("cannot read stdin: {error}")))?;
            buffer
        }
        Source::File(path) => std::fs::read_to_string(&path).map_err(|error| {
            Failure::usage(format!("cannot read {}: {error}", path.display()))
        })?,
    };
    let trimmed = raw
        .strip_suffix('\n')
        .map(|value| value.strip_suffix('\r').unwrap_or(value))
        .unwrap_or(&raw);
    if trimmed.is_empty() {
        return Err(Failure::usage(
            "no value on stdin: pipe the key in, or pass --key-file",
        ));
    }
    Ok(trimmed.to_owned())
}

#[derive(Debug)]
pub enum Source {
    Stdin,
    File(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tidemarkctl-secret-{}-{}",
            std::process::id(),
            contents.len()
        ));
        std::fs::write(&path, contents).expect("writes");
        path
    }

    #[test]
    fn one_trailing_newline_is_trimmed() {
        let path = file("sk-abc\n");
        assert_eq!(read(Source::File(path.clone())).expect("reads"), "sk-abc");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_second_trailing_newline_belongs_to_the_value() {
        let path = file("sk-abcd\n\n");
        assert_eq!(read(Source::File(path.clone())).expect("reads"), "sk-abcd\n");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn nothing_at_all_is_a_usage_error_not_a_cleared_key() {
        let path = file("\n");
        let failure = read(Source::File(path.clone())).expect_err("refuses");
        assert_eq!(failure.exit, crate::exit::Exit::Usage);
        std::fs::remove_file(path).ok();
    }
}
```

An empty value is refused locally because `SetKey("")` is a request to store nothing, and
the daemon already refuses it — failing here gives the user the sentence that says what to
do instead of a D-Bus error.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tidemark-cli secret`
Expected: FAIL — the module is not declared yet. Add `pub mod secret;` to `lib.rs`, then
the tests compile and pass.

- [ ] **Step 3: Extend the grammar**

`cli.rs` gains:

```rust
    /// Credentials: keys, pasted sessions, logins and local sources.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
```

```rust
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store an API key. The key is read from stdin, or from --key-file.
    SetKey {
        provider: String,
        account: String,
        /// Read the key from this file instead of stdin.
        #[arg(long)]
        key_file: Option<std::path::PathBuf>,
    },
    /// Store a browser session header. Read like a key.
    SetSession {
        provider: String,
        account: String,
        #[arg(long)]
        key_file: Option<std::path::PathBuf>,
    },
    /// Remove whatever credential Tidemark holds for an account.
    SignOut { provider: String, account: String },
    /// Print the authorize URL, then wait for the browser to come back.
    Login { provider: String, account: String },
    /// Abandon a login that is waiting.
    CancelLogin { provider: String, account: String },
    /// The local authentication sources the daemon can see, without their credentials.
    Sources { provider: String, account: String },
    /// Record which local source this account uses.
    Select {
        provider: String,
        account: String,
        /// The mode value from `auth sources`.
        #[arg(long)]
        mode: String,
        /// The candidate id, for a mode that offers a choice.
        #[arg(long)]
        candidate: Option<String>,
    },
}
```

- [ ] **Step 4: Write the commands**

`crates/tidemark-cli/src/commands/auth.rs`:

```rust
use std::io::Write;

use tidemark_ipc::DaemonProxy;
use tidemark_types::{AuthCandidate, AuthSelection};

use crate::cli::AuthCommand;
use crate::exit::{Exit, Failure};
use crate::secret;

pub async fn run(proxy: &DaemonProxy<'_>, command: AuthCommand) -> Result<Exit, Failure> {
    match command {
        AuthCommand::SetKey {
            provider,
            account,
            key_file,
        } => {
            let key = secret::read(source(key_file))?;
            proxy.set_key(&provider, &account, &key).await?;
        }
        AuthCommand::SetSession {
            provider,
            account,
            key_file,
        } => {
            let session = secret::read(source(key_file))?;
            proxy.set_session(&provider, &account, &session).await?;
        }
        AuthCommand::SignOut { provider, account } => {
            proxy.sign_out(&provider, &account).await?
        }
        // The URL first and flushed, because the caller — a person or a plugin — cannot act
        // on it until it is on the pipe, and `AwaitLogin` then blocks for as long as the
        // browser takes. The daemon never opens a browser and neither does this: it may be
        // running on a machine with no display.
        AuthCommand::Login { provider, account } => {
            let url = proxy.begin_login(&provider, &account).await?;
            println!("{url}");
            std::io::stdout().flush()?;
            proxy.await_login(&provider, &account).await?;
        }
        AuthCommand::CancelLogin { provider, account } => {
            proxy.cancel_login(&provider, &account).await?
        }
        AuthCommand::Sources { provider, account } => {
            for candidate in proxy.get_auth_sources(&provider, &account).await? {
                print_candidate(&candidate, 0);
            }
        }
        AuthCommand::Select {
            provider,
            account,
            mode,
            candidate,
        } => {
            proxy
                .select_auth_source(&provider, &account, AuthSelection { mode, candidate })
                .await?
        }
    }
    Ok(Exit::Ok)
}

fn source(key_file: Option<std::path::PathBuf>) -> secret::Source {
    match key_file {
        Some(path) => secret::Source::File(path),
        None => secret::Source::Stdin,
    }
}

/// Titles and readiness, never a cookie value, a token or a database path — the daemon does
/// not publish those, and this prints exactly what it publishes.
fn print_candidate(candidate: &AuthCandidate, depth: usize) {
    let indent = "  ".repeat(depth);
    let subtitle = candidate.subtitle.as_deref().unwrap_or("");
    println!(
        "{indent}{:<28} {:<20} {}",
        candidate.id, candidate.state, subtitle
    );
    for child in &candidate.children {
        print_candidate(child, depth + 1);
    }
}
```

`main.rs` gains:

```rust
        cli::Command::Auth { command } => {
            let proxy = connect::daemon().await?;
            commands::auth::run(&proxy, command).await
        }
```

`commands/mod.rs` gains `pub mod auth;`.

- [ ] **Step 5: Write the failing integration test**

`crates/tidemark-cli/tests/storing_a_secret.rs`:

```rust
mod fake;

use tidemark_cli::cli::AuthCommand;
use tidemark_cli::commands;

#[test]
fn a_key_from_a_file_reaches_the_daemon_and_is_never_printed() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let path = std::env::temp_dir().join(format!("tidemarkctl-key-{}", std::process::id()));
        std::fs::write(&path, "sk-secret-value\n").expect("writes the key file");

        commands::auth::run(
            &proxy,
            AuthCommand::SetKey {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
                key_file: Some(path.clone()),
            },
        )
        .await
        .expect("stores the key");

        let recorded = calls.recorded();
        assert_eq!(recorded, vec!["SetKey(claude,default,15 chars)".to_owned()]);
        assert!(
            !recorded.iter().any(|call| call.contains("sk-secret")),
            "a secret must never reach a log or a test name: {recorded:?}"
        );

        std::fs::remove_file(path).ok();
        drop(server);
    });
}
```

`set_key` on the fake needs to be in `fake/mod.rs` from Task 8 — it is. Extend the fake
with `set_session`, `begin_login`, `await_login`, `cancel_login`, `get_auth_sources` and
`select_auth_source` only if a test drives them; do not add unused methods.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [ ] **Step 7: Smoke it**

```bash
printf '%s' 'sk-not-a-real-key' | cargo run -p tidemark-cli -- auth set-key zai default
cargo run -p tidemark-cli -- usage --provider zai
cargo run -p tidemark-cli -- auth sign-out zai default
```
Expected: the card moves to `credential-rejected` (a wrong key is a real answer), then back
to `no-credential`. Do this on a provider you are not signed into, or restore your key
afterwards.

Confirm the prohibition holds: `cargo run -p tidemark-cli -- auth set-key zai default sk-x`
Expected: clap rejects the extra argument — there is no positional slot for a key.

- [ ] **Step 8: Commit**

```bash
git add crates/tidemark-cli
git commit -m "feat(cli): store credentials without putting them on a command line"
```

### Task 11: options, notifications, preferences, history

**Files:**
- Create: `crates/tidemark-cli/src/commands/config.rs`
- Modify: `crates/tidemark-cli/src/cli.rs`, `src/main.rs`, `src/commands/mod.rs`
- Modify: `crates/tidemark-cli/tests/managing_a_fake_daemon.rs`

**Interfaces:**
- Produces: `commands::config::run(&DaemonProxy<'_>, cli::ConfigCommand) -> Result<Exit, Failure>`.

- [ ] **Step 1: Extend the grammar**

`cli.rs` gains four commands:

```rust
    /// One of a provider's own settings.
    Option {
        provider: String,
        account: String,
        name: String,
        value: String,
    },
    /// Notifications for one window of one account.
    Notify {
        provider: String,
        account: String,
        window: String,
        #[arg(value_parser = switch)]
        enabled: bool,
    },
    /// Application preferences the daemon keeps in config.toml.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Stored history.
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Paths and storage facts.
    Data,
    /// A newer published release, if the daemon knows of one.
    Update,
```

```rust
/// `on` and `off` rather than `true` and `false`: the settings pages call these switches.
fn switch(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(format!("expected `on` or `off`, not `{other}`")),
    }
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Every preference, as the daemon holds it.
    Show,
    /// The one proxy every request and every child process goes through.
    Proxy {
        /// off, http, https or socks5.
        mode: String,
        /// Required by every mode but `off`.
        host: Option<String>,
        /// Required by every mode but `off`.
        port: Option<u16>,
    },
    /// How healthy accounts are paced.
    Refresh {
        /// auto or manual.
        mode: String,
        /// Minutes between polls in manual mode, 1 to 120.
        #[arg(long)]
        minutes: Option<u32>,
    },
    /// forever, six-months or one-year.
    Retention { retention: String },
    /// system, light or dark.
    Theme { theme: String },
    /// app, daemon or off.
    Startup { mode: String },
    /// Whether the daemon may ask GitHub for the latest release.
    ReleaseCheck {
        #[arg(value_parser = switch)]
        enabled: bool,
    },
    /// Whether the window's close button hides it.
    MinimizeOnClose {
        #[arg(value_parser = switch)]
        enabled: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// The stored points of one window's current segment, oldest first.
    Segment {
        provider: String,
        account: String,
        window: String,
    },
    /// Delete every stored point, segment and notification record.
    Clear,
}
```

- [ ] **Step 2: Write the failing test**

Append to `crates/tidemark-cli/tests/managing_a_fake_daemon.rs`:

```rust
#[test]
fn a_proxy_is_sent_as_one_setting_and_a_half_of_one_is_refused() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let failure = commands::config::run(
            &proxy,
            tidemark_cli::cli::ConfigCommand::Proxy {
                mode: "socks5".to_owned(),
                host: Some("127.0.0.1".to_owned()),
                port: None,
            },
        )
        .await
        .expect_err("half a proxy is not a proxy");
        assert_eq!(failure.exit, tidemark_cli::exit::Exit::Usage);
        assert!(calls.recorded().is_empty(), "{:?}", calls.recorded());

        commands::config::run(
            &proxy,
            tidemark_cli::cli::ConfigCommand::Proxy {
                mode: "socks5".to_owned(),
                host: Some("127.0.0.1".to_owned()),
                port: Some(1080),
            },
        )
        .await
        .expect("a whole proxy is accepted");
        assert_eq!(
            calls.recorded(),
            vec!["SetProxy(socks5,127.0.0.1,1080)".to_owned()]
        );

        commands::config::run(
            &proxy,
            tidemark_cli::cli::ConfigCommand::Proxy {
                mode: "off".to_owned(),
                host: None,
                port: None,
            },
        )
        .await
        .expect("off needs neither");

        drop(server);
    });
}
```

- [ ] **Step 3: Run and watch it fail**

Run: `cargo test -p tidemark-cli --test managing_a_fake_daemon`
Expected: FAIL — `commands::config` does not exist.

- [ ] **Step 4: Write the command**

`crates/tidemark-cli/src/commands/config.rs`:

```rust
use tidemark_ipc::DaemonProxy;
use tidemark_types::Preferences;

use crate::cli::ConfigCommand;
use crate::exit::{Exit, Failure};

pub async fn run(proxy: &DaemonProxy<'_>, command: ConfigCommand) -> Result<Exit, Failure> {
    match command {
        ConfigCommand::Show => show(&proxy.get_preferences().await?),
        // The daemon takes the three proxy values as one call because half a proxy is a
        // proxy nothing can be reached through. Completing them here means the user gets a
        // sentence instead of a refusal.
        ConfigCommand::Proxy { mode, host, port } => {
            if mode == Preferences::PROXY_OFF {
                proxy.set_proxy(&mode, "", 0).await?;
            } else {
                let (Some(host), Some(port)) = (host, port) else {
                    return Err(Failure::usage(format!(
                        "the `{mode}` proxy mode needs a host and a port"
                    )));
                };
                proxy.set_proxy(&mode, &host, port).await?;
            }
        }
        ConfigCommand::Refresh { mode, minutes } => {
            if let Some(minutes) = minutes {
                proxy.set_refresh_minutes(minutes).await?;
            }
            proxy.set_refresh_mode(&mode).await?;
        }
        ConfigCommand::Retention { retention } => {
            proxy.set_history_retention(&retention).await?
        }
        ConfigCommand::Theme { theme } => proxy.set_theme(&theme).await?,
        ConfigCommand::Startup { mode } => proxy.set_startup_mode(&mode).await?,
        ConfigCommand::ReleaseCheck { enabled } => proxy.set_release_check(enabled).await?,
        ConfigCommand::MinimizeOnClose { enabled } => {
            proxy.set_minimize_on_close(enabled).await?
        }
    }
    Ok(Exit::Ok)
}

/// Named choices are printed as the daemon stores them, so what comes out of `show` is
/// what goes back into the corresponding subcommand.
fn show(preferences: &Preferences) {
    println!("release-check       {}", preferences.release_check);
    println!("minimize-on-close   {}", preferences.minimize_on_close);
    println!(
        "theme               {}",
        preferences.theme.as_deref().unwrap_or(Preferences::THEME_SYSTEM)
    );
    println!("startup             {}", preferences.startup_mode);
    println!("retention           {}", preferences.history_retention);
    println!("refresh             {}", preferences.refresh_mode);
    println!("refresh-minutes     {}", preferences.refresh_minutes);
    println!("proxy               {}", preferences.proxy_mode);
    println!("proxy-host          {}", preferences.proxy_host);
    println!("proxy-port          {}", preferences.proxy_port);
}
```

Note the ordering in `Refresh`: the interval is sent before the mode, so switching to
`manual` with a new interval polls at the interval the user asked for rather than at the
old one. `set_refresh_mode` polls every account immediately — see `CONTEXT.md` § Polling.

- [ ] **Step 5: Wire the remaining arms in `main.rs`**

```rust
        cli::Command::Option {
            provider,
            account,
            name,
            value,
        } => {
            let proxy = connect::daemon().await?;
            proxy.set_option(&provider, &account, &name, &value).await?;
            Ok(Exit::Ok)
        }
        cli::Command::Notify {
            provider,
            account,
            window,
            enabled,
        } => {
            let proxy = connect::daemon().await?;
            proxy
                .set_window_notify(&provider, &account, &window, enabled)
                .await?;
            Ok(Exit::Ok)
        }
        cli::Command::Config { command } => {
            let proxy = connect::daemon().await?;
            commands::config::run(&proxy, command).await
        }
        cli::Command::History { command } => {
            let proxy = connect::daemon().await?;
            match command {
                cli::HistoryCommand::Segment {
                    provider,
                    account,
                    window,
                } => {
                    for point in proxy.current_segment(&provider, &account, &window).await? {
                        println!(
                            "{} {}",
                            point.captured_at,
                            tidemark_types::present::percent(point.used_percent)
                        );
                    }
                }
                cli::HistoryCommand::Clear => proxy.clear_history().await?,
            }
            Ok(Exit::Ok)
        }
        cli::Command::Data => {
            let proxy = connect::daemon().await?;
            let data = proxy.get_data_info().await?;
            println!("config              {}", data.config_path);
            println!("history             {}", data.history_path);
            println!("history-bytes       {}", data.history_bytes);
            println!("key-schema          {}", data.key_schema);
            println!("token-schema        {}", data.token_schema);
            println!("release-check       {}", data.release_check_available);
            Ok(Exit::Ok)
        }
        cli::Command::Update => {
            let proxy = connect::daemon().await?;
            let version = proxy.get_update().await?;
            if version.is_empty() {
                println!("no newer release is known");
            } else {
                println!("{version}");
            }
            Ok(Exit::Ok)
        }
```

`commands/mod.rs` gains `pub mod config;`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p tidemark-cli`
Expected: PASS.

- [ ] **Step 7: Smoke it**

```bash
cargo run -p tidemark-cli -- config show
cargo run -p tidemark-cli -- data
cargo run -p tidemark-cli -- update
cargo run -p tidemark-cli -- config refresh manual --minutes 15
cargo run -p tidemark-cli -- config show | grep refresh
cargo run -p tidemark-cli -- config refresh auto
cargo run -p tidemark-cli -- history segment claude default w18000 | tail -3
```
Expected: `config show` reflects each change, the running Preferences dialog updates live
(it listens to `PreferencesChanged`), and the segment prints timestamps with percentages.
Put `refresh` back to whatever it was before.

- [ ] **Step 8: Full gate and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemark-cli
git commit -m "feat(cli): options, notifications, preferences and history"
```

---

## Phase E — shipping it

### Task 12: completions and packaging

**Files:**
- Modify: `crates/tidemark-cli/Cargo.toml`, `src/cli.rs`, `src/main.rs`
- Modify: `crates/tidemark/Cargo.toml` (deb and rpm asset lists)
- Modify: `PKGBUILD`
- Modify: `nix/package.nix`

**Interfaces:**
- Produces: `tidemarkctl completions <bash|zsh|fish>` on stdout.

- [ ] **Step 1: Add the completion generator**

`cargo add -p tidemark-cli clap_complete`

`cli.rs` gains:

```rust
    /// Print a shell completion script on stdout.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
```

`main.rs` gains an arm that needs no daemon at all:

```rust
        cli::Command::Completions { shell } => {
            let mut command = <cli::Cli as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut command, "tidemarkctl", &mut std::io::stdout());
            Ok(Exit::Ok)
        }
```

- [ ] **Step 2: Verify each shell**

```bash
cargo run -p tidemark-cli -- completions bash | head -5
cargo run -p tidemark-cli -- completions zsh | head -5
cargo run -p tidemark-cli -- completions fish | head -5
```
Expected: a script per shell, each mentioning `tidemarkctl`.

Load one for real: `source <(cargo run -q -p tidemark-cli -- completions bash)` then type
`tidemarkctl prov<TAB>`.
Expected: it completes to `provider`.

- [ ] **Step 3: Add the binary to the Debian package**

In `crates/tidemark/Cargo.toml`, in `[package.metadata.deb] assets`, after the `tidemarkd`
line:

```toml
    ["target/release/tidemarkctl", "usr/bin/", "755"],
```

- [ ] **Step 4: Add it to the RPM**

In the same file, in `[package.metadata.generate-rpm] assets`:

```toml
    { source = "target/release/tidemarkctl", dest = "/usr/bin/tidemarkctl", mode = "755" },
```

- [ ] **Step 5: Add it to the Arch package**

In `PKGBUILD`, after the `tidemarkd` install line:

```bash
    install -Dm755 "$bin/tidemarkctl" "$pkgdir/usr/bin/tidemarkctl"
```

- [ ] **Step 6: Add it to the Nix package**

In `nix/package.nix`, in `postInstall`, after the `tidemarkd` line:

```nix
    install -Dm755 target/*/release/tidemarkctl -t "$out/bin"
```

`cargoBuildFlags` is already `--workspace --bins`, so the binary is built; nothing else in
the derivation changes. Do **not** wrap it with `wrapGAppsHook4`: it loads no GTK.

- [ ] **Step 7: Prove the packages carry it**

```bash
cargo build --release --locked --workspace
cargo deb --no-build -p tidemark
dpkg --contents target/debian/*.deb | grep tidemarkctl
```
Expected: `usr/bin/tidemarkctl` in the listing.

Then the dependency scan that guards these lists:
Run: `./scripts/check-package-deps.sh`
Expected: PASS. A third binary must not add a shared-library dependency the metadata does
not declare; `tidemarkctl` links no GTK, so the set should be unchanged.

- [ ] **Step 8: Confirm the Windows build still builds**

Run: `cargo check --workspace --target x86_64-pc-windows-gnu` if the target is installed;
otherwise rely on CI and say so in the commit body.
Expected: `tidemark-cli` compiles. It is not added to any Windows packaging list — check
`data/packaging` and the NSIS inputs and leave them alone.

- [ ] **Step 9: Commit**

```bash
git add crates/tidemark-cli crates/tidemark/Cargo.toml PKGBUILD nix/package.nix Cargo.lock
git commit -m "feat(cli): completions, and ship tidemarkctl in every Linux package"
```

### Task 13: the documentation the contract needs

**Files:**
- Create: `crates/tidemark-cli/AGENTS.md`
- Modify: `README.md`
- Modify: `CONTEXT.md`
- Modify: `AGENTS.md`
- Modify: `crates/tidemark-ipc/AGENTS.md` (created in Task 1)

- [ ] **Step 1: Write the README section**

In `README.md`, after the "Getting started" section, add:

````markdown
## From the command line

`tidemarkctl` talks to the same daemon the window does, so anything the interface can do is
scriptable — and a panel widget needs no D-Bus code of its own.

```bash
tidemarkctl usage                      # every account, for a person
tidemarkctl usage --format json        # {"accounts": [...]}, for a program
tidemarkctl guard --min-remaining 20 --window w604800 || echo "not this week"
tidemarkctl watch                      # one JSON object per change, until you stop it
```

`guard` exits `0` when the quota is there, `1` when it is not, and `69` when there is no
trustworthy reading to judge — a rejected credential never answers "safe".

Secrets are read from stdin, never from the command line:

```bash
printf '%s' "$ZAI_API_KEY" | tidemarkctl auth set-key zai default
```

A Waybar module is two files. In `~/.config/waybar/config.jsonc`:

```jsonc
"custom/tidemark": {
    "exec": "tidemarkctl watch --format waybar",
    "return-type": "json",
    "on-click": "tidemark"
}
```

and in your stylesheet, the classes it sets — `ok`, `warning`, `danger` at the same 70% and
90% the app's own bar changes colour at, plus `stale` when the reading is not fresh:

```css
#custom-tidemark.warning { color: @warning_color; }
#custom-tidemark.danger  { color: @error_color; }
#custom-tidemark.stale   { opacity: 0.5; }
```

`tidemarkctl completions zsh > ~/.zfunc/_tidemarkctl` installs completions;
`tidemarkctl --help` lists the rest — `provider`, `account`, `auth`, `notify`, `config`,
`history`.
````

- [ ] **Step 2: Update the architecture record**

In `CONTEXT.md` § Architecture, the crate-layout paragraph currently opens with "Four
crates, not three." Replace that sentence and add the two rows to its table. The new
opening:

```markdown
Six crates, and two of them exist to make a rule checkable rather than aspirational: the
GUI never performs network I/O, and there is exactly one definition of the D-Bus contract.
```

Table rows to add, in dependency order:

```markdown
| `tidemark-ipc` | the generated D-Bus proxy | providers, storage, the display |
| `tidemark-cli` | `tidemarkctl` | `tidemark-core`, GTK, a runtime |
```

Then, immediately after the "### D-Bus interface" subsection, add "### Command line":

```markdown
### Command line

`tidemarkctl` is the third client, and the reason the interface was shaped as it was.
It generates its proxy from `tidemark-ipc`, the same crate the window uses, so a method
that changes shape breaks the build rather than a user's panel.

Three output formats, and two of them are contracts. `text` is for a person and may be
reworded. `json` is an object with an `accounts` key — not a bare array, so a key can be
added later — carrying `ProviderStatus` as serde renders it, absent values still absent.
`waybar` is `{text, tooltip, class, percentage}`, where `class` is **always an array** so
its shape does not change when a second class applies: the zone (`ok`, `warning`, `danger`,
at the same 70/90 the bar uses) and `stale` when the account's state is not `ok`.

`watch` streams NDJSON: a `snapshot` first, then one line per signal, a `waiting` line when
the daemon leaves the bus, and a fresh `snapshot` after every reconnect — what the daemon
published while nothing was listening was announced to nobody.

Exit codes are the point of `guard`: `0` safe, `1` below the threshold, `64` bad arguments,
`69` nothing trustworthy to judge, `70` the daemon refused. Only an `ok` account's reading
is judged; a last-good number behind a rejected credential would otherwise say "safe"
about quota nobody can spend.

Secrets are read from stdin or `--key-file`, never from argv: `/proc/<pid>/cmdline` is
readable by every process of the same user. stdin is a file descriptor rather than a
terminal, so a plugin with its own UI writes into a pipe without opening a console.

Windows is out of scope for the CLI: there the daemon serves zbus p2p over AF_UNIX, and the
endpoint discovery lives in the window's own reconnect module. The binary still compiles
there; it is packaged only on Linux.
```

- [ ] **Step 3: Write the crate AGENTS files**

`crates/tidemark-ipc/AGENTS.md` and `crates/tidemark-cli/AGENTS.md`, following the shape of
`crates/tidemark-types/AGENTS.md`: an OVERVIEW line, a WHERE TO LOOK table, CONVENTIONS and
ANTI-PATTERNS. The anti-patterns that matter, stated plainly:

- `tidemark-ipc`: never add policy — no retry, no reconnect, no transport choice. Never let
  a second proxy definition exist anywhere in the workspace.
- `tidemark-cli`: never accept a secret in argv; never format a percentage or a span
  outside `tidemark_types::present`; never invent a value the daemon did not send; the
  `json` and `waybar` shapes and the exit codes are a published contract — add keys, never
  rename or remove them.

- [ ] **Step 4: Update the root knowledge base**

In the root `AGENTS.md`: add `tidemark-ipc` and `tidemark-cli` to the STRUCTURE block, one
row each to WHERE TO LOOK (`Change the CLI` → `crates/tidemark-cli/`; `Change the D-Bus
proxy` → `crates/tidemark-ipc/src/lib.rs`), and `tidemarkctl usage --format json` to
COMMANDS.

- [ ] **Step 5: Verify the documentation against the binary**

Run every command block in the new README section verbatim against the live daemon, and the
Waybar snippet in a real Waybar if one is running.
Expected: each works as written. A documented command that does not run is worse than an
undocumented one.

- [ ] **Step 6: Full gate and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add README.md CONTEXT.md AGENTS.md crates/tidemark-cli/AGENTS.md crates/tidemark-ipc/AGENTS.md
git commit -m "docs: tidemarkctl, and the contract it publishes"
```

---

## Done when

- `tidemarkctl usage`, `--format json` and `--format waybar` print the same numbers the
  window shows, with nothing invented where a provider withheld a value.
- `guard` distinguishes safe, below-threshold and unjudgeable with three different exit
  codes.
- `watch` opens with a snapshot, follows every signal, survives `systemctl --user restart
  tidemarkd`, and drives a Waybar module.
- Every method on the interface except `RequestActivate` is reachable, including adding a
  provider, storing a key from a pipe, signing in, and changing preferences.
- No secret can be passed on a command line.
- `deb`, `rpm`, `PKGBUILD` and the Nix package carry `tidemarkctl`; the Windows packaging
  does not, and the Windows build still compiles.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` and `./scripts/check-layering.sh` all pass.
