# Browser-Cookie Auth Source Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Let Cursor and future browser-cookie providers use one explicit, daemon-validated local authentication source rather than silently selecting the first one found.

**Architecture:** Add optional, secret-free browser-auth metadata to the shared wire vocabulary. Core owns candidate discovery and provider-specific validation; the daemon exposes inspect/select operations and atomically persists choices. The GTK provider page renders generic Cursor App/Browser tabs exclusively from daemon data.

**Tech Stack:** Rust 2024, GTK4/libadwaita 1.9, zbus, Tokio, reqwest, rusqlite, toml_edit, Secret Service through oo7.

**Spec:** docs/superpowers/specs/2026-08-26-browser-cookie-auth-source-selection-design.md

## Global Constraints

- tidemark is display-only and uses D-Bus only; it must not depend on tidemark-core, reqwest, rusqlite, or browser-storage code.
- tidemarkd alone owns browser discovery, network validation, config writes, and provider rebuilds.
- Browser databases are read only through private owner-only snapshots; never write to browser directories.
- Cookies and Cursor App tokens are credentials: never place them in config, D-Bus types, logs, errors, toasts, or Debug output.
- Browser/provider slugs and profile identifiers are stable selected-source identifiers; do not rename them after shipping.
- A selected source is exclusive; a failed source must not fall back to another browser or Cursor App.
- Existing OAuth/CLI and API-key flows remain behaviorally unchanged.
- Preserve absent fields as absent in published a{sv} dictionaries.
- Tests use built-in test support, temporary homes, fake keyring storage, and real loopback HTTP servers; no new test frameworks.
- Final acceptance includes the full CI gate and installed daemon/GUI/D-Bus validation.

---

### Task 1: Define the generic, secret-free browser-auth wire contract

**Files:**
- Modify: crates/tidemark-types/src/wire.rs
- Modify: crates/tidemark-types/src/lib.rs
- Test: crates/tidemark-types/src/wire.rs

**Interfaces:**
- Produces: AuthSelector, AuthMode, AuthCandidate, AuthCandidateState, and AuthSelection public wire types.
- Produces: optional ProviderDefinition.browser_auth and ProviderStatus.auth_selection fields.
- Consumes: existing ProviderDefinition, ProviderStatus, SerializeDict, DeserializeDict, and zvariant::Type conventions.
- Used by: core discovery, daemon D-Bus methods, and GTK provider settings.

- [ ] **Step 1: Write the failing wire tests**

Add a round-trip test for a definition containing a browser-auth selector and a nested browser/profile candidate, plus a status test proving an older-style dictionary that omits the new fields still decodes with None.

~~~rust
let selector = AuthSelector {
    option: "auth-source".into(),
    modes: vec![
        AuthMode { value: "cursor-app".into(), title: "Cursor App".into() },
        AuthMode { value: "browser".into(), title: "Browser".into() },
    ],
};
assert_eq!(decoded.browser_auth.as_ref(), Some(&selector));
assert_eq!(decoded.auth_selection, None);
~~~

- [ ] **Step 2: Run the focused test and verify it fails for missing types/fields**

Run: cargo test -p tidemark-types browser_auth -- --nocapture
Expected: FAIL because AuthSelector/AuthCandidate/AuthSelection and the optional fields do not exist.

- [ ] **Step 3: Implement the minimal extensible wire vocabulary**

Add the five structs/enums as a{sv}-compatible serializable types. Candidate state must distinguish ready, missing, rejected, waiting-for-keyring, and unreachable. Add optional browser_auth to ProviderDefinition and optional auth_selection to ProviderStatus; initialize both as None in ProviderStatus::pending. Re-export the new types from lib.rs.

~~~rust
pub struct AuthSelection {
    pub mode: String,
    pub candidate: Option<String>,
}

pub struct AuthCandidate {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub state: String,
    pub children: Vec<AuthCandidate>,
}
~~~

- [ ] **Step 4: Re-run focused and crate tests**

Run: cargo test -p tidemark-types
Expected: PASS, including legacy decoding and nested candidate round trips.

- [ ] **Step 5: Commit the standalone vocabulary change**

~~~bash
git add crates/tidemark-types/src/lib.rs crates/tidemark-types/src/wire.rs
git commit -m "feat(types): describe browser auth sources"
~~~

### Task 2: Build reusable browser-auth discovery and selection primitives

**Files:**
- Create: crates/tidemark-core/src/browser/auth.rs
- Modify: crates/tidemark-core/src/browser/mod.rs
- Test: crates/tidemark-core/src/browser/auth.rs

**Interfaces:**
- Consumes: browser::Store, browser::Query, Cookie, SafeStorage, Timestamp, and the Task 1 wire types.
- Produces: browser::auth::inspect and browser::auth::Selection, returning only AuthCandidate metadata plus an internal credential handle used within core.
- Used by: Cursor's validator in Task 3 and future browser-cookie provider adapters.

- [ ] **Step 1: Write failing temporary-home tests for browser candidates**

Create Gecko fixture stores for two browsers and two profiles. Assert that an expired cookie is Missing, an accepted validator result is Ready, a validator credential error is Rejected, and two valid profiles stay nested under their browser in stable scan order.

~~~rust
let report = inspect(&home, &query, storage.as_ref(), validate).await;
assert_eq!(report[0].children[0].state, AuthCandidateState::Ready.as_wire());
assert!(report[1].children[0].is_insensitive_candidate());
~~~

- [ ] **Step 2: Run the focused test and verify it fails**

Run: cargo test -p tidemark-core browser::auth::tests -- --nocapture
Expected: FAIL because browser::auth and inspect do not exist.

- [ ] **Step 3: Implement discovery without leaking cookies**

Add an internal candidate/header pair whose Debug implementation does not reveal the header. Read each Store via the current snapshot API, filter live cookies, preserve BROWSERS/profile ordering, and map CookieError::KeyringLocked to the dedicated neutral state. Let the async callback convert each candidate header to Ready, Rejected, or Unreachable; return wire candidates only after dropping headers.

~~~rust
pub async fn inspect<F, Fut>(
    stores: Vec<Store>,
    query: &Query,
    storage: &dyn SafeStorage,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(CandidateCredential) -> Fut,
    Fut: Future<Output = CandidateState>;
~~~

- [ ] **Step 4: Run focused and browser module tests**

Run: cargo test -p tidemark-core browser:: -- --nocapture
Expected: PASS; existing snapshot, domain-match, Chromium, and Gecko tests stay green.

- [ ] **Step 5: Commit the reusable discovery layer**

~~~bash
git add crates/tidemark-core/src/browser/mod.rs crates/tidemark-core/src/browser/auth.rs
git commit -m "feat(browser): inspect cookie auth candidates"
~~~

### Task 3: Make Cursor resolve one explicit source and validate it

**Files:**
- Modify: crates/tidemark-core/src/providers/keyed/cursor.rs
- Modify: crates/tidemark-core/src/providers/keyed/mod.rs
- Test: crates/tidemark-core/src/providers/keyed/cursor.rs

**Interfaces:**
- Consumes: browser::auth primitives from Task 2 and OptionSchema/Options from keyed::mod.
- Produces: Cursor auth-source option schema, inspect_sources(options), and a provider that resolves only its selected source.
- Used by: registry/daemon source inspection and normal Cursor polling.

- [ ] **Step 1: Replace historical fallback tests with failing explicit-selection tests**

Change the tests that currently expect an expired browser to fall through to another browser or Cursor App. Add tests for selecting Cursor App, a selected browser parent, and a selected nested profile. Each must prove requests contain only that candidate's cookie and that a rejection does not try another source.

~~~rust
let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring))?
    .with_options(cursor_options("browser", Some("zen"), Some("zz99.Working")));
let error = runtime.block_on(provider.fetch_inner()).expect_err("selected session rejected");
assert!(matches!(error, ProviderError::Credential { .. }));
assert_eq!(summary_requests(&requests), 1);
~~~

- [ ] **Step 2: Run the focused Cursor tests and verify they fail**

Run: cargo test -p tidemark-core providers::keyed::cursor::tests -- --nocapture
Expected: FAIL because the provider still scans every source and HandSpec exposes no auth options.

- [ ] **Step 3: Implement Cursor source parsing, inspection, and validation**

Declare stable auth-source, auth-browser, and auth-profile option names and accepted mode values. Parse options into a selected internal source with clear malformed/absent behavior. Reuse the current Cursor request builder for a one-request validator, classify 401/403 as Rejected, and use the generic helper for browsers. Read only the chosen browser/profile or Cursor App in normal fetch; never append fallback headers.

~~~rust
enum Source {
    CursorApp,
    Browser { slug: String, profile: Option<String> },
}

pub async fn inspect_sources(&self) -> Result<Vec<AuthCandidate>, ProviderError>;
~~~

- [ ] **Step 4: Run all Cursor/provider tests**

Run: cargo test -p tidemark-core providers::keyed::cursor -- --nocapture
Expected: PASS, including WAL snapshot, redaction, selected-source, and malformed-option coverage.

- [ ] **Step 5: Commit Cursor's explicit source behavior**

~~~bash
git add crates/tidemark-core/src/providers/keyed/cursor.rs crates/tidemark-core/src/providers/keyed/mod.rs
git commit -m "feat(cursor): select an explicit local auth source"
~~~

### Task 4: Persist and expose source selection through daemon config and D-Bus

**Files:**
- Modify: crates/tidemark-core/src/config.rs
- Modify: crates/tidemarkd/src/registry.rs
- Modify: crates/tidemarkd/src/engine.rs
- Modify: crates/tidemarkd/src/service.rs
- Modify: crates/tidemark/src/bus.rs
- Test: crates/tidemark-core/src/config.rs
- Test: crates/tidemarkd/src/registry.rs
- Test: crates/tidemarkd/src/engine.rs
- Test: crates/tidemarkd/src/service.rs

**Interfaces:**
- Consumes: Task 1 wire types and Task 3 Cursor inspection/selection APIs.
- Produces: Config::set_auth_selection, Engine inspect/select commands, and Daemon/GetAuthSources/SelectAuthSource proxy methods.
- Used by: Task 5 GTK detail page.

- [x] **Step 1: Write failing config, engine, registry, and service tests**

Test that Cursor App selection removes auth-browser/auth-profile, a browser-parent selection removes only auth-profile, and nested selection retains all fields. Test that unknown/unready candidate selection leaves config unchanged, drops/rebuilds the client only after validation, and publishes an immediate due status. Under dbus-run-session, exercise the two new D-Bus methods and ensure the returned report contains no cookie value.

~~~rust
config.set_auth_selection("cursor", &AuthSelection {
    mode: "cursor-app".into(),
    candidate: None,
})?;
assert_eq!(config.option("cursor", "auth-browser"), None);
assert_eq!(config.option("cursor", "auth-profile"), None);
~~~

- [x] **Step 2: Run focused daemon/config tests and verify they fail**

Run: cargo test -p tidemark-core config::tests && cargo test -p tidemarkd auth_source -- --nocapture
Expected: FAIL because the atomic config operation and source D-Bus methods do not exist.

- [x] **Step 3: Implement serialized selection lifecycle**

Add Config::set_auth_selection using one staged write that preserves TOML comments and removes stale keys. Let registry publish Cursor's browser-auth selector in ProviderDefinition and resolve its current AuthSelection from config without defaulting to another source. Add engine commands for inspect/select; select must validate inside the account mutation sequence, write config only after success, refresh status/options, clear the rebuildable client, reset scheduling failure state, and set due to now. Service validates account existence, delegates through the engine queue, and exposes generic GetAuthSources/SelectAuthSource. Extend the GUI proxy only with those D-Bus signatures.

~~~rust
async fn select_auth_source(
    &self,
    provider: &str,
    account: &str,
    selection: AuthSelection,
) -> fdo::Result<()>;
~~~

- [x] **Step 4: Re-run focused tests under a session bus**

Run: dbus-run-session -- cargo test -p tidemarkd auth_source -- --nocapture
Expected: PASS; config is unchanged on a rejected source and the valid selection publishes a changed provider status.

- [x] **Step 5: Commit daemon-owned source selection**

~~~bash
git add crates/tidemark-core/src/config.rs crates/tidemarkd/src/registry.rs crates/tidemarkd/src/engine.rs crates/tidemarkd/src/service.rs crates/tidemark/src/bus.rs
git commit -m "feat(daemon): manage browser auth sources"
~~~

### Task 5: Render the generic source tabs in provider settings

**Files:**
- Create: crates/tidemark/src/provider_settings/browser_auth.rs
- Modify: crates/tidemark/src/provider_settings/mod.rs
- Modify: crates/tidemark/src/provider_settings/model.rs
- Modify: crates/tidemark/src/provider_settings/detail.rs
- Modify: crates/tidemark/src/provider_settings/list.rs
- Test: crates/tidemark/src/provider_settings/browser_auth.rs
- Test: crates/tidemark/src/provider_settings/model.rs
- Test: crates/tidemark/src/provider_settings/mod.rs

**Interfaces:**
- Consumes: ProviderDefinition.browser_auth, ProviderStatus.auth_selection, AuthCandidate reports, and DaemonProxy GetAuthSources/SelectAuthSource.
- Produces: reusable browser-auth tab widgets and data-driven configured-list edit eligibility.
- Must not consume: Cursor core code, cookies, filesystem, HTTP, or SQLite.

- [x] **Step 1: Write failing model/widget tests**

Add pure model tests that mark Ready candidates selectable and Missing/Rejected candidates insensitive, retain WaitingForKeyring/Unreachable as neutral recheckable states, and show nested profiles only when a browser has more than one ready profile. Add a detail navigation test showing a keyless provider with browser_auth opens after add and exposes its pencil.

~~~rust
assert!(candidate_selectable(AuthCandidateState::Ready));
assert!(!candidate_selectable(AuthCandidateState::Rejected));
assert!(shows_profile_children(&two_ready_profiles));
assert!(!shows_profile_children(&one_ready_profile));
~~~

- [x] **Step 2: Run focused GUI tests and verify they fail**

Run: cargo test -p tidemark provider_settings::browser_auth -- --nocapture
Expected: FAIL because browser_auth UI/model helpers do not exist and keyless browser-auth providers are not editable.

- [x] **Step 3: Implement generic Authentication tabs and report refresh**

Add a BrowserAuthRows component owned by ProviderDetail. Build the full-width ToggleGroup from daemon-published mode titles, with Cursor App and Browser halves rather than provider-slug branches. On detail construction and Check again, call GetAuthSources through glib::spawn_future_local, render loading/neutral/red/green accessibility states, and retain the prior authoritative selection on an error. SelectAuthSource handles row activation; after success it asks the daemon to publish the new status. Use profile titles as nested ActionRows only for browsers that need them.

Update opens_detail_after_add and configured-row edit visibility to use the generic capability rather than credential kind/options alone.

~~~rust
fn open_browser_auth(self: &Rc<Self>) {
    glib::spawn_future_local(async move {
        let report = detail.proxy.get_auth_sources(&provider, &account).await?;
        detail.browser_auth.borrow().as_ref().expect("built").apply(report);
    });
}
~~~

- [x] **Step 4: Run provider-settings tests and an isolated GTK smoke test**

Run: cargo test -p tidemark provider_settings -- --nocapture
Expected: PASS; existing OAuth/CLI rows retain their current tests and the new Browser Auth page handles loading, retry, selection rollback, and reopen.

- [x] **Step 5: Commit the generic HIG page**

~~~bash
git add crates/tidemark/src/provider_settings/browser_auth.rs crates/tidemark/src/provider_settings/mod.rs crates/tidemark/src/provider_settings/model.rs crates/tidemark/src/provider_settings/detail.rs crates/tidemark/src/provider_settings/list.rs
git commit -m "feat(settings): choose browser auth sources"
~~~

### Task 6: Align normative and user-facing documentation

**Files:**
- Modify: CONTEXT.md
- Modify: README.md
- Modify: docs/TRADEMARKS.md only if a new visible mark is actually added

**Interfaces:**
- Consumes: the final behavior from Tasks 1-5.
- Produces: an accurate architecture contract and provider setup explanation.

- [ ] **Step 1: Write documentation assertions as review criteria**

Record the exact statements that must be true: browser-cookie auth is a bounded daemon-only mechanism; reads use snapshots; source selection is explicit; a failed source does not silently fall back; Cursor supports Cursor App and selected browsers.

- [ ] **Step 2: Verify current documentation fails those criteria**

Run: rg -n "No browser-cookie scraping|Cursor reads the session|first" CONTEXT.md README.md
Expected: FIND the deferred blanket exclusion and the old implicit-selection description.

- [ ] **Step 3: Update only the affected documentation**

Replace the deferred browser-cookie exclusion in CONTEXT.md with the bounded mechanism and its privacy/ownership rules. Update the Cursor README entry to describe explicit source selection, browser/profile behavior, and the local Cursor App choice. Do not change unrelated provider guidance.

- [ ] **Step 4: Review rendered Markdown and terminology**

Run: git diff --check && rg -n "browser-cookie|Cursor App|silent|fallback" CONTEXT.md README.md
Expected: PASS with coherent, user-facing language and no whitespace errors.

- [ ] **Step 5: Commit documentation alignment**

~~~bash
git add CONTEXT.md README.md
git commit -m "docs: explain explicit browser auth selection"
~~~

### Task 7: Run full verification and installed acceptance

**Files:**
- Verify: all files changed by Tasks 1-6
- Verify: scripts/check-layering.sh
- Verify: scripts/check-desktop-integration.sh
- Verify: scripts/test-restart-user-daemon.sh

**Interfaces:**
- Consumes: completed feature commits.
- Produces: evidence that the source selector works across unit, D-Bus, GUI, package, and installed daemon boundaries.

- [ ] **Step 1: Run formatting and static checks**

Run: cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
Expected: PASS with no warnings.

- [ ] **Step 2: Run the workspace test gate exactly as CI does**

Run: dbus-run-session -- cargo test --workspace
Expected: PASS, including new type/core/daemon/GTK tests.

- [ ] **Step 3: Run architecture, desktop, restart, and shell checks**

Run: scripts/check-layering.sh && scripts/check-desktop-integration.sh && scripts/test-restart-user-daemon.sh && shellcheck scripts/*.sh data/restart-user-daemon data/packaging/deb/postinst data/packaging/rpm/post-install.sh
Expected: PASS.

- [ ] **Step 4: Verify the installed application**

Use the tidemark-installed-verification skill. Rebuild/install the scoped package, start the installed daemon, inspect its D-Bus surface, and exercise the installed provider page in an isolated display. Prove Cursor opens directly into settings, shows both tabs, accepts a ready source, leaves red candidates inactive, and does not move the user's cursor.

- [ ] **Step 5: Capture final evidence and request review**

Run: git status --short && git log --oneline -7
Expected: only the scoped commits and any explicitly pre-existing user changes. Report the exact command results, installed D-Bus evidence, and commit list before proposing integration.

