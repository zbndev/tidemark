# Update Availability Notification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `tidemarkd` check GitHub hourly and make the GTK client show a releases-page button when that daemon is older than the latest release.

**Architecture:** A focused daemon module performs the bounded GitHub request and strict version comparison. Transient availability is published through one D-Bus getter and one change signal; the GUI remains network-free and only renders daemon-owned state.

**Tech Stack:** Rust 2024, Tokio, reqwest with rustls, semver, serde_json, zbus 5, GTK 4/libadwaita.

**Spec:** `docs/superpowers/specs/2026-08-22-update-notification-design.md`

## Global Constraints

- Compare only with `tidemarkd`'s `CARGO_PKG_VERSION`, never the GUI version.
- Accept only canonical tags `vX.X.X`: no prereleases, metadata, leading zeroes, or arbitrary names.
- Wait 60 seconds before the first request and 3,600 seconds between later requests.
- A failed check leaves prior availability unchanged and creates no user-visible error.
- The GUI must not gain HTTP, database, or `tidemark-core` dependencies.
- Open only `https://github.com/zbndev/tidemark/releases`, never a response-provided URL.
- Do not download packages, install updates, or distinguish DEB from RPM.
- Keep Rust 1.92, GTK 4.22, and libadwaita 1.9 as the minimum versions.

---

### Task 1: Strict, bounded GitHub release checker

**Files:**
- Create: `crates/tidemarkd/src/update.rs`
- Modify: `crates/tidemarkd/src/main.rs:13-18`
- Modify: `crates/tidemarkd/Cargo.toml:14-25`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `env!("CARGO_PKG_VERSION")` and `tidemark_types::user_agent()`.
- Produces: `Checker::production() -> Result<Checker, CheckError>`.
- Produces: `Checker::check(&self) -> Result<Option<String>, CheckError>`.
- Produces: crate-visible `INITIAL_DELAY` and `INTERVAL` constants.

- [ ] **Step 1: Write failing strict-version tests**

Create `update.rs`, register it as `mod update;`, and add:

```rust
#[test]
fn a_newer_canonical_release_is_available() {
    assert_eq!(newer("v0.12.3", "0.9.9").unwrap(), Some("0.12.3".into()));
}

#[test]
fn equal_and_older_releases_are_not_available() {
    assert_eq!(newer("v1.2.3", "1.2.3").unwrap(), None);
    assert_eq!(newer("v1.2.2", "1.2.3").unwrap(), None);
}

#[test]
fn only_v_followed_by_a_canonical_three_part_version_is_accepted() {
    for tag in ["1.2.3", "v1.2", "v1.2.3.4", "v01.2.3", "v1.02.3",
                "v1.2.03", "v1.2.3-beta.2", "v1.2.3+build", "latest"] {
        assert!(newer(tag, "0.1.0").is_err(), "accepted {tag}");
    }
}
```

- [ ] **Step 2: Run the tests to prove they fail**

Run: `cargo test -p tidemarkd update::tests --no-run`

Expected: compilation fails because `newer` is not defined.

- [ ] **Step 3: Add dependencies and the minimal strict comparison**

Add direct `reqwest = { version = "0.13.4", default-features = false, features = ["rustls", "charset", "http2", "system-proxy"] }` and `semver = "1.0.27"` dependencies. Add `time`, `net`, and `io-util` to the existing Tokio features; the latter two are used only by the local HTTP fixture. Implement:

```rust
fn version(text: &str) -> Result<Version, CheckError> {
    let parts: Vec<_> = text.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0')))
    {
        return Err(CheckError::Version);
    }
    Version::parse(text).map_err(|_| CheckError::Version)
}

fn newer(tag: &str, current: &str) -> Result<Option<String>, CheckError> {
    let release = version(tag.strip_prefix('v').ok_or(CheckError::Version)?)?;
    let current = version(current)?;
    Ok((release > current).then(|| release.to_string()))
}
```

Run: `cargo test -p tidemarkd update::tests`

Expected: the three strict-version tests pass.

- [ ] **Step 4: Write failing HTTP, JSON, status, and size-limit tests**

Use a test-only `tokio::net::TcpListener` helper that serves one supplied raw HTTP response. Test these exact outcomes through `Checker::at(endpoint, "0.1.0")`:

```rust
assert_eq!(check("200 OK", br#"{"tag_name":"v0.2.0"}"#).await.unwrap(),
           Some("0.2.0".into()));
assert!(check("429 Too Many Requests", br#"{}"#).await.is_err());
assert!(check("200 OK", br#"{"#).await.is_err());
assert!(check("200 OK", br#"{"tag_name":"v0.2.0-beta.1"}"#).await.is_err());
assert!(check("200 OK", &vec![b'x'; MAX_BODY + 1]).await.is_err());
```

Run: `cargo test -p tidemarkd update::tests`

Expected: compilation fails because `Checker`, `MAX_BODY`, and bounded fetching are absent.

- [ ] **Step 5: Implement the bounded checker**

Define:

```rust
pub(crate) const INITIAL_DELAY: Duration = Duration::from_secs(60);
pub(crate) const INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_BODY: usize = 64 * 1024;
const ENDPOINT: &str = "https://api.github.com/repos/zbndev/tidemark/releases/latest";

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckError {
    #[error("could not build or send the release request")]
    Http(#[source] reqwest::Error),
    #[error("the release endpoint returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("the release response exceeded the size limit")]
    TooLarge,
    #[error("the release response was not valid JSON")]
    Json(#[source] serde_json::Error),
    #[error("the release or daemon version was not canonical X.X.X")]
    Version,
}

#[derive(Debug)]
pub(crate) struct Checker {
    client: reqwest::Client,
    endpoint: String,
    current: String,
}
```

`production()` calls `at(ENDPOINT, env!("CARGO_PKG_VERSION"))`. `at` builds a client with a 15-second timeout and default headers for Tidemark's user agent, `Accept: application/vnd.github+json`, and `X-GitHub-Api-Version: 2026-03-10`. `check` rejects non-success status, reads `response.chunk().await` until EOF while rejecting accumulation beyond `MAX_BODY`, decodes a `serde_json::Value`, extracts only string `tag_name`, and calls `newer`.

Run:

```bash
cargo test -p tidemarkd update::tests
```

Expected: all checker tests pass. The checker is intentionally dead code until Task 2 wires it into
the daemon; warnings-denied clippy therefore runs at the end of Task 2, not at this intermediate
commit.

- [ ] **Step 6: Commit Task 1**

```bash
git add crates/tidemarkd/src/update.rs crates/tidemarkd/src/main.rs \
  crates/tidemarkd/Cargo.toml Cargo.lock
git commit -m "feat: check the latest Tidemark release"
```

---

### Task 2: Publish availability through D-Bus

**Files:**
- Modify: `crates/tidemarkd/src/service.rs:54-151, 640-660, 1640-1780`
- Modify: `crates/tidemarkd/src/main.rs:107-198`

**Interfaces:**
- Consumes: `Checker`, `INITIAL_DELAY`, and `INTERVAL` from Task 1.
- Produces: `PublishedUpdate::get(&self) -> String` and `replace(&self, Option<String>) -> bool`.
- Produces: `publish_result(&PublishedUpdate, Result<Option<String>, CheckError>) -> Result<Option<String>, CheckError>`; `Some(payload)` means emit a signal.
- Produces D-Bus: `GetUpdate() -> String` and `UpdateChanged(String)`.

- [ ] **Step 1: Write a failing publication-state test**

```rust
#[tokio::test]
async fn update_publication_changes_only_when_the_value_changes() {
    let update = PublishedUpdate::default();
    assert_eq!(update.get().await, "");
    assert!(update.replace(Some("0.2.0".into())).await);
    assert!(!update.replace(Some("0.2.0".into())).await);
    assert_eq!(update.get().await, "0.2.0");
    assert!(update.replace(None).await);
    assert_eq!(update.get().await, "");
}
```

Run: `cargo test -p tidemarkd service::tests::update_publication_changes_only_when_the_value_changes`

Expected: compilation fails because `PublishedUpdate` is absent.

- [ ] **Step 2: Implement state and pass it into every `Daemon::new` call**

```rust
#[derive(Debug, Clone, Default)]
pub struct PublishedUpdate(Arc<RwLock<Option<String>>>);

impl PublishedUpdate {
    pub async fn get(&self) -> String {
        self.0.read().await.clone().unwrap_or_default()
    }
    pub async fn replace(&self, next: Option<String>) -> bool {
        let mut held = self.0.write().await;
        if *held == next { return false; }
        *held = next;
        true
    }
}
```

Add it as a `Daemon` field and constructor argument. Update the main constructor and all five service-test constructors.

Run: `cargo test -p tidemarkd service::tests::update_publication_changes_only_when_the_value_changes`

Expected: PASS.

- [ ] **Step 3: Write failing real-D-Bus assertions**

Extend `a_client_reads_the_daemon_over_a_real_session_bus`: seed `PublishedUpdate` with `0.2.0`, call `GetUpdate`, and assert the returned string. Subscribe to `UpdateChanged`, replace state with `None`, emit `""`, then assert both signal payload and a following `GetUpdate` are empty. This pins state-before-signal ordering.

Run: `dbus-run-session -- cargo test -p tidemarkd service::tests::a_client_reads_the_daemon_over_a_real_session_bus`

Expected: failure because the two D-Bus members do not exist.

- [ ] **Step 4: Add the getter and signal**

```rust
async fn get_update(&self) -> String {
    self.update.get().await
}

#[zbus(signal)]
pub async fn update_changed(
    emitter: &SignalEmitter<'_>,
    version: &str,
) -> zbus::Result<()>;
```

Run: `dbus-run-session -- cargo test -p tidemarkd service::tests::a_client_reads_the_daemon_over_a_real_session_bus`

Expected: PASS.

- [ ] **Step 5: Pin failure preservation, then spawn and stop the hourly task**

Add `publish_result` in `main.rs` and first write a test which seeds `0.2.0`, passes an `Err(CheckError::Version)`, and asserts both that the result is an error and `PublishedUpdate::get()` still returns `0.2.0`. Add success assertions showing a changed value returns `Some("0.3.0")` and a repeat returns `None`.

Run: `cargo test -p tidemarkd tests::a_failed_release_check_preserves_the_previous_update`

Expected: FAIL before `publish_result` exists, then PASS after implementing:

```rust
async fn publish_result(
    update: &PublishedUpdate,
    result: Result<Option<String>, update::CheckError>,
) -> Result<Option<String>, update::CheckError> {
    let next = result?;
    Ok(update
        .replace(next.clone())
        .await
        .then(|| next.unwrap_or_default()))
}
```

Construct the checker and shared state before serving D-Bus. After the connection exists, spawn:

```rust
tokio::time::sleep(update::INITIAL_DELAY).await;
loop {
    match publish_result(&published_update, checker.check().await).await {
        Ok(Some(version)) => {
            if let Err(error) = Daemon::update_changed(&emitter, &version).await {
                tracing::warn!(%error, "could not announce update availability");
            }
        }
        Ok(None) => {}
        Err(error) => tracing::info!(%error, "release check failed"),
    }
    tokio::time::sleep(update::INTERVAL).await;
}
```

The helper's `?` must run before `replace`. Abort the task during normal shutdown beside the signal task.

Run:

```bash
cargo fmt --all --check
dbus-run-session -- cargo test -p tidemarkd
cargo clippy -p tidemarkd --all-targets -- -D warnings
```

Expected: all pass.

- [ ] **Step 6: Commit Task 2**

```bash
git add crates/tidemarkd/src/service.rs crates/tidemarkd/src/main.rs
git commit -m "feat: publish available updates from the daemon"
```

---

### Task 3: Render the GTK update link

**Files:**
- Modify: `crates/tidemark/src/bus.rs:36-250`
- Modify: `crates/tidemark/src/update.rs`
- Modify: `crates/tidemark/src/window.rs:78-265, 510-550, 625-680`
- Modify: `crates/tidemark/examples/mock-daemon.rs:30-80`

**Interfaces:**
- Consumes D-Bus: `GetUpdate() -> String` and `UpdateChanged(String)`.
- Preserves the existing daemon-newer-than-GUI restart notice driven by the optional daemon-version
  field on `Update::Connected`.
- Produces GUI events: a new available-release `String` on `Update::Connected` and
  `Update::Available(String)`.
- Produces: `update_tooltip(&str) -> Option<String>`.

- [ ] **Step 1: Write failing tooltip/visibility tests**

Add these to the existing test module in `crates/tidemark/src/update.rs`; they extend rather than
replace the package-upgrade restart tests merged from `main`:

```rust
#[test]
fn an_empty_update_has_no_button_copy() {
    assert_eq!(update_tooltip(""), None);
}

#[test]
fn an_available_update_names_the_daemon_selected_version() {
    assert_eq!(update_tooltip("0.12.3").as_deref(),
               Some("Tidemark 0.12.3 is available"));
}
```

Run: `cargo test -p tidemark window::tests::an_empty_update_has_no_button_copy window::tests::an_available_update_names_the_daemon_selected_version`

Expected: compilation fails because `update_tooltip` is absent.

- [ ] **Step 2: Implement the pure decision**

```rust
const RELEASES_URL: &str = "https://github.com/zbndev/tidemark/releases";

pub(crate) fn update_tooltip(version: &str) -> Option<String> {
    (!version.is_empty()).then(|| format!("Tidemark {version} is available"))
}
```

Run the tests again; expect PASS.

- [ ] **Step 3: Extend the proxy and bus event loop**

Add `get_update` and `update_changed` to the proxy trait. Subscribe before `load`, poll the new
stream beside owner/provider/removal, and add `Event::Available(Option<UpdateChanged>)`. Convert its
payload to `Update::Available(args.version.to_owned())`. Do not alter the existing daemon `Version`
property subscription/reload behavior used by the restart notice.

Keep the update getter auxiliary:

```rust
let available = proxy.get_update().await.unwrap_or_else(|error| {
    tracing::info!(%error, "the daemon did not answer GetUpdate; hiding update availability");
    String::new()
});
```

Pass `available` immediately after the existing optional daemon-version field on
`Update::Connected`. A closed update signal stream ends `serve` like the existing streams.

Run: `cargo test -p tidemark bus::tests window::tests`

Expected: PASS after all enum matches compile.

- [ ] **Step 4: Build and drive the hidden header button**

Add `release: gtk::Button` to `MainWindow`; keep the existing `update_notice: RefCell<UpdateNotice>`
unchanged because it solves package-upgrade GUI restart, a separate problem. Build the button with
icon `software-update-available-symbolic` and `visible(false)`. Pack refresh first and release second
at the header end so the new control is immediately left of refresh.

```rust
fn show_update(&self, version: &str) {
    let tooltip = update_tooltip(version);
    self.release.set_tooltip_text(tooltip.as_deref());
    self.release.set_visible(tooltip.is_some());
}
```

Call it from Connected, Available, and Waiting (empty string). Connect the button through a weak `MainWindow`, create `gtk::UriLauncher::new(RELEASES_URL)`, and call `launch_future(Some(&main.window))`; only log launch errors and leave the button enabled.

Run:

```bash
cargo test -p tidemark
cargo clippy -p tidemark --all-targets -- -D warnings
```

Expected: both pass.

- [ ] **Step 5: Publish `0.2.0` from the mock daemon**

```rust
async fn get_update(&self) -> String { "0.2.0".into() }

#[zbus(signal)]
pub async fn update_changed(
    emitter: &SignalEmitter<'_>,
    version: &str,
) -> zbus::Result<()>;
```

Run: `cargo build -p tidemark --examples`

Expected: the mock daemon and GUI compile.

- [ ] **Step 6: Commit Task 3**

```bash
git add crates/tidemark/src/bus.rs crates/tidemark/src/window.rs \
  crates/tidemark/src/update.rs crates/tidemark/examples/mock-daemon.rs
git commit -m "feat: show an available-update link in the header"
```

---

### Task 4: Full and installed acceptance verification

**Files:**
- Verify: all Task 1-3 files.
- Modify only if verification finds a scoped defect.

**Interfaces:**
- Consumes the completed checker, D-Bus contract, GTK control, and mock daemon.
- Produces verified installed behavior and no additional API.

- [ ] **Step 1: Run full static and workspace checks**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
dbus-run-session -- cargo test --workspace
scripts/check-layering.sh
scripts/check-desktop-integration.sh
git diff --check
```

Expected: every command exits 0 and the GUI layering guard confirms no HTTP dependency.

- [ ] **Step 2: Verify the mock D-Bus contract**

Stop the installed daemon, run the mock daemon, and call:

```bash
busctl --user call io.github.zbndev.Tidemark.Daemon \
  /io/github/zbndev/Tidemark io.github.zbndev.Tidemark.Daemon1 GetUpdate
```

Expected: signature `s`, value `"0.2.0"`.

- [ ] **Step 3: Inspect the real GTK window**

Launch the real GUI against the mock. Verify the update icon is immediately left of quota refresh, the tooltip is `Tidemark 0.2.0 is available`, and activation targets the fixed releases page. Capture visual or accessibility evidence without adding a production debug switch.

- [ ] **Step 4: Perform installed-package acceptance**

Read `/home/herald/.codex/memories/skills/tidemark-installed-verification/SKILL.md` completely and follow it. Rebuild/install the package, restart through the packaged helper, and verify installed versions, package integrity, daemon service health, D-Bus introspection, normal status, and installed GUI startup.

- [ ] **Step 5: Review the scoped diff**

```bash
git status --short
git diff --stat main...HEAD
git diff --check main...HEAD
git log --oneline main..HEAD
```

If acceptance required a correction, commit only its scoped files with `fix: correct update availability verification finding`. Do not create an empty commit.
