# Provider Management and Antigravity OAuth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the static five-provider settings sheet with a scalable add/edit/remove workflow and let Antigravity users sign in and fetch quota without installing `agy`.

**Architecture:** `tidemarkd` publishes a compiled provider catalog separately from the configured accounts stored in `config.toml`. Topology mutations run through the engine command queue and are announced over D-Bus; the GTK client renders a searchable catalog and one detail page per configured provider. Antigravity prefers a Tidemark-owned Google OAuth credential for direct Cloud Code Assist quota requests and falls back to the existing local `agy` transport only when no owned token exists.

**Tech Stack:** Rust 1.92, Tokio, zbus 5, reqwest 0.13 with rustls, serde/serde_json, toml_edit, GTK 4.22, libadwaita 1.9, Secret Service through oo7, SQLite through rusqlite.

**Spec:** `docs/superpowers/specs/2026-08-21-provider-management-antigravity-oauth-design.md`

## Global Constraints

- All documentation, source code, code comments, tests, logs, and interface copy are written in English.
- The GUI crate must not depend on `tidemark-core`, HTTP, SQLite, or Secret Service; it communicates only through `tidemark-types` and D-Bus.
- A fresh installation has no configured providers.
- Removing a provider deletes both possible Tidemark-owned credential slots and its provider-specific settings, but never deletes quota history or changes vendor-owned files and `agy` sessions.
- Multiple accounts remain out of scope; every added provider uses account id `default`.
- OAuth uses the system browser and loopback callback, never an embedded browser or browser-cookie scraping.
- Antigravity HTTP requests identify the product as `Tidemark/<version>`; do not copy randomized browser or Antigravity executable user agents from prior art.
- Every behavior change follows red-green-refactor: add one focused test, observe the expected failure, implement the minimum behavior, and rerun the focused and affected suites.
- Preserve user-owned work already committed in `d076092`; do not rewrite or squash that historical checkpoint.

## File Structure

- `crates/tidemark-core/src/config.rs` — ordered configured-provider persistence and provider option editing.
- `crates/tidemark-types/src/wire.rs` — forward-compatible D-Bus dictionaries for catalog definitions and account status.
- `crates/tidemarkd/src/registry.rs` — compiled catalog metadata and construction of one configured account by slug.
- `crates/tidemarkd/src/engine.rs` — serialized add/remove/reload/poll commands and topology publications.
- `crates/tidemarkd/src/service.rs` — D-Bus methods, credential deletion, login ownership, and catalog/status publication.
- `crates/tidemarkd/src/main.rs` — routes changed and removed publications to shared state and D-Bus signals.
- `crates/tidemark/src/bus.rs` — client proxy plus catalog, status, changed, and removed event streams.
- `crates/tidemark/src/window.rs` — card insertion/removal, welcome state, catalog storage, and dialog lifetime slot.
- `crates/tidemark/src/provider_settings/mod.rs` — provider settings dialog controller and navigation.
- `crates/tidemark/src/provider_settings/model.rs` — pure search/filter/connection-copy logic.
- `crates/tidemark/src/provider_settings/list.rs` — configured-provider list, add picker, edit, and removal confirmation.
- `crates/tidemark/src/provider_settings/detail.rs` — one provider's authentication and declared option controls.
- `crates/tidemark/src/mark.rs` — reusable provider mark widget at card and detail-page sizes.
- `crates/tidemark-core/src/oauth.rs` — provider-neutral loopback OAuth with optional client secret.
- `crates/tidemark-core/src/providers/antigravity/direct.rs` — direct Cloud Code Assist quota transport and pure payload parser.
- `crates/tidemark-core/src/providers/antigravity/oauth.rs` — Google login metadata, project discovery/onboarding, credential document, and token refresh.
- `crates/tidemark-core/src/providers/antigravity/mod.rs` — credential-source selection and the existing `agy` fallback.
- `crates/tidemark-core/src/providers/antigravity/agy.rs` — local CLI availability probe and existing supervised transport.
- `CONTEXT.md`, `README.md`, and `docs/adr/0003-loopback-port-is-the-providers-to-choose.md` — public contract and Antigravity callback documentation.

---

### Task 1: Persist the Ordered Configured-Provider Set

**Files:**
- Modify: `crates/tidemark-core/src/config.rs:21-253`

**Interfaces:**
- Consumes: the existing `Config::at`, `Config::set_option`, and staged `Config::write` implementation.
- Produces:
  - `pub fn providers(&self) -> Result<Vec<String>, ConfigError>`
  - `pub fn add_provider(&mut self, provider: &str) -> Result<bool, ConfigError>`
  - `pub fn remove_provider(&mut self, provider: &str) -> Result<bool, ConfigError>`
  - `ConfigError::InvalidProviders { path: PathBuf, reason: String }`

- [ ] **Step 1: Add failing tests for empty, ordered, and deduplicated provider reads**

Add these tests to the existing `config.rs` test module, using its `scratch()` helper:

```rust
#[test]
fn a_first_run_has_no_configured_providers() {
    let config = Config::at(scratch("providers-absent")).expect("missing is valid");
    assert_eq!(config.providers().expect("readable"), Vec::<String>::new());
}

#[test]
fn configured_providers_keep_their_order_and_first_duplicate() {
    let path = scratch("providers-order");
    std::fs::write(&path, "providers = [\"claude\", \"zai\", \"claude\"]\n")
        .expect("seed");
    let config = Config::at(path.clone()).expect("parses");
    assert_eq!(config.providers().expect("readable"), ["claude", "zai"]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_non_array_provider_list_is_refused_not_replaced() {
    let path = scratch("providers-wrong-type");
    std::fs::write(&path, "providers = \"claude\"\n").expect("seed");
    let config = Config::at(path.clone()).expect("valid TOML");
    assert!(matches!(
        config.providers(),
        Err(ConfigError::InvalidProviders { .. })
    ));
    assert_eq!(std::fs::read_to_string(&path).expect("still there"), "providers = \"claude\"\n");
    let _ = std::fs::remove_file(path);
}
```

- [ ] **Step 2: Run the focused tests and observe the missing API**

Run: `cargo test -p tidemark-core config::tests::configured_providers -- --nocapture`

Expected: compilation fails because `Config::providers` and `ConfigError::InvalidProviders` do not exist.

- [ ] **Step 3: Implement strict provider-list reads**

Add `const PROVIDERS_KEY: &str = "providers";`. Read only a TOML array of strings, deduplicate with `BTreeSet` while retaining first-seen order, and return `InvalidProviders` for a wrong top-level type or a non-string array element:

```rust
pub fn providers(&self) -> Result<Vec<String>, ConfigError> {
    let Some(item) = self.document.get(PROVIDERS_KEY) else {
        return Ok(Vec::new());
    };
    let array = item.as_array().ok_or_else(|| ConfigError::InvalidProviders {
        path: self.path.clone(),
        reason: "providers must be an array of strings".to_owned(),
    })?;
    let mut seen = std::collections::BTreeSet::new();
    let mut providers = Vec::new();
    for item in array.iter() {
        let slug = item.as_str().ok_or_else(|| ConfigError::InvalidProviders {
            path: self.path.clone(),
            reason: "every providers entry must be a string".to_owned(),
        })?;
        if seen.insert(slug.to_owned()) {
            providers.push(slug.to_owned());
        }
    }
    Ok(providers)
}
```

- [ ] **Step 4: Add failing mutation tests**

```rust
#[test]
fn adding_is_idempotent_and_removing_drops_only_that_provider_table() {
    let path = scratch("provider-mutations");
    std::fs::write(
        &path,
        "# owned by the user\nproviders = [\"claude\"]\n\n[provider.claude]\nfuture = \"gone with claude\"\n\n[unrelated]\nfuture = \"kept\"\n",
    )
    .expect("seed");
    let mut config = Config::at(path.clone()).expect("parses");
    assert!(config.add_provider("zai").expect("added"));
    assert!(!config.add_provider("zai").expect("duplicate is a no-op"));
    assert!(config.remove_provider("claude").expect("removed"));
    assert!(!config.remove_provider("claude").expect("missing is a no-op"));

    let reread = Config::at(path.clone()).expect("parses again");
    assert_eq!(reread.providers().expect("readable"), ["zai"]);
    let text = std::fs::read_to_string(&path).expect("written");
    assert!(text.contains("# owned by the user"));
    assert!(text.contains("[unrelated]"));
    assert!(!text.contains("[provider.claude]"));
    let _ = std::fs::remove_file(path);
}
```

- [ ] **Step 5: Run the mutation test and observe the missing methods**

Run: `cargo test -p tidemark-core config::tests::adding_is_idempotent -- --nocapture`

Expected: compilation fails because `add_provider` and `remove_provider` do not exist.

- [ ] **Step 6: Implement atomic add and remove mutations**

Use `toml_edit::Array` for the root list. `add_provider` appends only after `providers()` validates the existing value. `remove_provider` rebuilds the array without the slug, removes `[provider.<slug>]`, and calls the existing staged `write()` only when something changed. Do not remove the root `[provider]` table when unrelated provider tables remain.

- [ ] **Step 7: Run the complete config suite**

Run: `cargo test -p tidemark-core config::tests -- --nocapture`

Expected: every config test passes, including preservation of comments and unrelated keys.

- [ ] **Step 8: Commit the config boundary**

```bash
git add crates/tidemark-core/src/config.rs
git commit -m "Store the configured provider set"
```

---

### Task 2: Publish Provider Catalog Definitions as Wire Dictionaries

**Files:**
- Modify: `crates/tidemark-types/src/wire.rs:133-203`
- Modify: `crates/tidemark-types/src/lib.rs:16-23`

**Interfaces:**
- Consumes: `CredentialKind`, `ProviderOption`, zvariant dictionary serialization.
- Produces:
  - `pub struct ProviderDefinition`
  - `ProviderDefinition::credential_kind(&self) -> Option<CredentialKind>`

- [ ] **Step 1: Write the failing wire round-trip test**

```rust
#[test]
fn a_provider_definition_survives_the_bus() {
    let original = ProviderDefinition {
        provider: "antigravity".into(),
        title: "Antigravity".into(),
        credential: CredentialKind::OAuth.as_wire().into(),
        credential_hint: "Sign in with Google.".into(),
        external_fallback: Some("agy session".into()),
        options: Vec::new(),
    };
    let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("encodes");
    let (decoded, _): (ProviderDefinition, _) = encoded.deserialize().expect("decodes");
    assert_eq!(decoded, original);
    assert_eq!(decoded.credential_kind(), Some(CredentialKind::OAuth));
}
```

- [ ] **Step 2: Run the test and observe the missing type**

Run: `cargo test -p tidemark-types wire::tests::a_provider_definition -- --nocapture`

Expected: compilation fails because `ProviderDefinition` does not exist.

- [ ] **Step 3: Implement and export the dictionary**

Add immediately after `ProviderOption`:

```rust
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct ProviderDefinition {
    pub provider: String,
    pub title: String,
    pub credential: String,
    pub credential_hint: String,
    pub external_fallback: Option<String>,
    pub options: Vec<ProviderOption>,
}

impl ProviderDefinition {
    pub fn credential_kind(&self) -> Option<CredentialKind> {
        CredentialKind::from_wire(&self.credential)
    }
}
```

Re-export it from `tidemark-types/src/lib.rs` beside `ProviderOption`.

- [ ] **Step 4: Run the wire crate tests**

Run: `cargo test -p tidemark-types`

Expected: all wire and domain tests pass.

- [ ] **Step 5: Commit the catalog wire shape**

```bash
git add crates/tidemark-types/src/wire.rs crates/tidemark-types/src/lib.rs
git commit -m "Define the provider catalog wire shape"
```

---

### Task 3: Split the Compiled Catalog from Configured Accounts

**Files:**
- Modify: `crates/tidemarkd/src/registry.rs:1-310`
- Modify: `crates/tidemarkd/src/main.rs:77-96`

**Interfaces:**
- Consumes: `Config::providers`, `ProviderDefinition`, and existing provider constructors.
- Produces:
  - `pub fn catalog(config: &Config) -> Vec<ProviderDefinition>`
  - `pub fn account(provider: &str, secrets: &Arc<dyn Secrets>, config: &Config) -> Result<Option<Account>, ProviderError>`
  - `pub fn accounts(secrets: &Arc<dyn Secrets>, config: &Config) -> Result<Vec<Account>, ProviderError>` restricted to configured slugs.

- [ ] **Step 1: Replace the static-account tests with failing catalog/configuration tests**

```rust
#[test]
fn the_catalog_exists_even_when_no_account_is_configured() {
    let config = empty_config();
    assert!(accounts(&secrets(), &config).expect("accounts build").is_empty());
    let definitions = catalog(&config);
    assert_eq!(definitions.len(), 5);
    assert_eq!(definitions[0].provider, "antigravity");
    assert!(definitions.iter().all(|definition| !definition.title.is_empty()));
}

#[test]
fn only_configured_known_providers_become_accounts_in_file_order() {
    let path = scratch_config("configured", "providers = [\"zai\", \"future\", \"claude\"]\n");
    let config = Config::at(path.clone()).expect("parses");
    let accounts = accounts(&secrets(), &config).expect("known accounts build");
    let slugs: Vec<&str> = accounts.iter().map(|account| account.provider().as_str()).collect();
    assert_eq!(slugs, ["zai", "claude"]);
    let _ = std::fs::remove_file(path);
}
```

Add `secrets()` and `scratch_config()` as concrete test helpers using the existing `NoSecrets` fake and process-id-scoped temp paths.

- [ ] **Step 2: Run the registry tests and observe that all five accounts are still created**

Run: `cargo test -p tidemarkd registry::tests -- --nocapture`

Expected: the empty-config assertion fails with five accounts.

- [ ] **Step 3: Implement the catalog table and single-account constructor**

Build `catalog()` in the stable display order Antigravity, Claude, Codex, Kimi, Z.ai. Preserve Antigravity as `CredentialKind::External` in this intermediate commit; Task 10 changes it to OAuth only when direct OAuth is functional. Publish exact fallback labels for Claude and Codex now:

```rust
ProviderDefinition {
    provider: "claude".into(),
    title: "Claude".into(),
    credential: CredentialKind::OAuth.as_wire().into(),
    credential_hint: "Sign in through Tidemark or use Claude Code's login.".into(),
    external_fallback: Some("Claude Code login".into()),
    options: options("claude", config),
}
```

`account()` matches the slug and invokes the existing provider-specific constructor. `accounts()` iterates `config.providers()`, logs unsupported slugs, and preserves configured order.

- [ ] **Step 4: Update daemon startup to accept zero accounts**

Keep `Engine::new` unchanged in this task. Pass only `registry::accounts(&secrets, &config)` into it and retain the startup log's `accounts = accounts.len()`. The existing engine already sleeps for one baseline interval when the vector is empty.

- [ ] **Step 5: Run registry and daemon tests**

Run: `cargo test -p tidemarkd registry::tests -- --nocapture && cargo test -p tidemarkd engine::tests::every_account_is_announced -- --nocapture`

Expected: registry tests pass and an empty engine announces nothing without spinning or panicking.

- [ ] **Step 6: Commit the registry split**

```bash
git add crates/tidemarkd/src/registry.rs crates/tidemarkd/src/main.rs
git commit -m "Separate provider catalog from configured accounts"
```

---

### Task 4: Add and Remove Accounts Inside the Running Engine

**Files:**
- Modify: `crates/tidemarkd/src/engine.rs:38-575`
- Modify: `crates/tidemarkd/src/main.rs:96-158`

**Interfaces:**
- Consumes: `registry::account`, `Config::add_provider`, `Config::remove_provider`, the existing scheduler and credential probe.
- Produces:

```rust
pub enum Publication {
    Changed(ProviderStatus),
    Removed { provider: String, account: String },
}

pub enum Command {
    Refresh(Option<String>),
    Reload { provider: Option<String> },
    AddProvider {
        provider: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    RemoveProvider {
        provider: String,
        account: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Shutdown,
}
```

- [ ] **Step 1: Write a failing engine test for a runtime add**

Extend the existing engine harness so it accepts a real `config_path`, then add:

```rust
#[tokio::test]
async fn adding_a_provider_persists_announces_and_makes_it_due_now() {
    let mut harness = Harness::empty("runtime-add").await;
    harness.engine.add_provider("kimi").await.expect("added");

    assert_eq!(harness.engine.accounts().len(), 1);
    assert_eq!(harness.engine.accounts()[0].provider().as_str(), "kimi");
    let publication = harness.updates.recv().await.expect("announced");
    assert!(matches!(publication, Publication::Changed(status) if status.provider == "kimi"));
    let config = Config::at(harness.config_path.clone()).expect("parses");
    assert_eq!(config.providers().expect("readable"), ["kimi"]);
}
```

- [ ] **Step 2: Run the add test and observe the missing topology API**

Run: `cargo test -p tidemarkd engine::tests::adding_a_provider -- --nocapture`

Expected: compilation fails because `Publication`, `Harness::empty`, and `Engine::add_provider` are missing.

- [ ] **Step 3: Change the update channel to typed publications**

Change `Engine.updates` and its constructor argument from `Sender<ProviderStatus>` to `Sender<Publication>`. Wrap every current status send as `Publication::Changed(status)`. Update the publisher task in `main.rs` to match both variants; temporarily remove the status on `Removed` without emitting a D-Bus signal until Task 5 adds that signal.

- [ ] **Step 4: Implement `Engine::add_provider`**

The method loads `Config::at(self.config_path.clone())`, returns idempotent success when the account already exists, constructs the account before writing, persists the slug, pushes the account, probes its credential, sets `due = Instant::now()`, and sends its pending status as `Publication::Changed`. Convert `ConfigError` and `ProviderError` to their English `to_string()` messages.

- [ ] **Step 5: Rerun the add test**

Run: `cargo test -p tidemarkd engine::tests::adding_a_provider -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Write the failing runtime-remove test**

```rust
#[tokio::test]
async fn removing_a_provider_stops_polling_and_keeps_history() {
    let mut harness = Harness::configured("runtime-remove", &["kimi"]).await;
    let before = harness.engine.history.point_count().expect("count");
    harness.engine.remove_provider("kimi", "default").await.expect("removed");

    assert!(harness.engine.accounts().is_empty());
    assert_eq!(harness.engine.history.point_count().expect("count"), before);
    assert!(matches!(
        harness.updates.recv().await,
        Some(Publication::Removed { provider, account })
            if provider == "kimi" && account == "default"
    ));
    let config = Config::at(harness.config_path.clone()).expect("parses");
    assert!(config.providers().expect("readable").is_empty());
}

#[tokio::test]
async fn reload_keeps_a_self_loading_provider_client_alive() {
    let mut harness = with_provider(Fake::new(vec![Ok(snapshot(7.0, 3_600))]));
    harness.engine.reload(None).await;
    harness.engine.poll_due(Instant::now()).await;

    let published = harness.published();
    assert_eq!(published.last().and_then(ProviderStatus::state), Some(ProviderState::Ok));
}
```

Use the existing public `History::point_count` method for this assertion; do not add any history-deletion API.

- [ ] **Step 7: Run the remove test and observe the missing method**

Run: `cargo test -p tidemarkd engine::tests::removing_a_provider -- --nocapture`

Expected: the removal test does not compile because `Engine::remove_provider` is missing, and the reload regression would fail because the current code drops a self-loading client that has no factory.

- [ ] **Step 8: Implement removal and command replies**

`remove_provider` verifies the exact provider/account pair, writes config first, removes the account only after a successful write, drops the client by dropping the `Account`, and sends `Publication::Removed`. Add `Command` match arms in `Engine::run` that call the topology method and send the result through the one-shot reply. Remove `Clone`, `PartialEq`, and `Eq` derives from `Command`; adjust existing command tests to `match` received variants instead of comparing whole values. In `reload`, clear `account.client` only when `account.factory.is_some()`; self-loading clients such as Claude, Codex, and Antigravity read their own Secret Service or vendor credential source on each fetch and must remain constructible after sign-in, sign-out, and option reloads.

- [ ] **Step 9: Run the full engine suite**

Run: `cargo test -p tidemarkd engine::tests -- --nocapture`

Expected: all scheduler, polling, reload, add, remove, and history-preservation tests pass.

- [ ] **Step 10: Commit dynamic topology**

```bash
git add crates/tidemarkd/src/engine.rs crates/tidemarkd/src/main.rs
git commit -m "Change provider topology at runtime"
```

---

### Task 5: Expose Catalog and Topology Mutations over D-Bus

**Files:**
- Modify: `crates/tidemarkd/src/service.rs:34-810`
- Modify: `crates/tidemarkd/src/main.rs:96-158`

**Interfaces:**
- Consumes: `ProviderDefinition`, Task 4 topology commands and publications, both Secret Service kinds.
- Produces:
  - `ListProviders() -> Vec<ProviderDefinition>`
  - `AddProvider(provider: &str) -> fdo::Result<()>`
  - `RemoveProvider(provider: &str, account: &str) -> fdo::Result<()>`
  - `ProviderRemoved(provider: &str, account: &str)` signal
  - `Published::remove(provider, account) -> Option<ProviderStatus>`

- [ ] **Step 1: Add failing shared-state and catalog tests**

```rust
#[tokio::test]
async fn removing_a_published_account_does_not_reorder_the_rest() {
    let published = Published::default();
    published.upsert(status("zai")).await;
    published.upsert(status("kimi")).await;
    assert!(published.remove("zai", "default").await.is_some());
    assert_eq!(published.all().await[0].provider, "kimi");
}

#[tokio::test]
async fn the_daemon_lists_the_catalog_even_with_no_statuses() {
    let definition = ProviderDefinition {
        provider: "zai".into(),
        title: "Z.ai".into(),
        credential: "key".into(),
        credential_hint: "Z.ai dashboard → API keys.".into(),
        external_fallback: None,
        options: Vec::new(),
    };
    let (daemon, _secrets, _commands) = daemon_over_catalog(Vec::new(), vec![definition.clone()]).await;
    assert_eq!(daemon.list_providers().await, vec![definition]);
}
```

- [ ] **Step 2: Run the tests and observe the missing methods**

Run: `cargo test -p tidemarkd service::tests -- --nocapture`

Expected: compilation fails because `Published::remove`, catalog storage, and `list_providers` do not exist.

- [ ] **Step 3: Store the catalog on `Daemon` and publish it**

Add `catalog: Vec<ProviderDefinition>` to `Daemon`, pass `registry::catalog(&config)` from `main.rs`, and return a clone from `list_providers`. Update every `Daemon::new` test call with an explicit catalog vector.

- [ ] **Step 4: Write failing add/remove method tests with a command responder**

Use a spawned responder that receives one topology command and answers its one-shot channel:

```rust
#[tokio::test]
async fn adding_waits_for_the_engine_result() {
    let (daemon, _secrets, mut commands) = daemon_over_catalog(Vec::new(), catalog()).await;
    let responder = tokio::spawn(async move {
        match commands.recv().await.expect("command") {
            Command::AddProvider { provider, reply } => {
                assert_eq!(provider, "zai");
                reply.send(Ok(())).expect("caller waits");
            }
            command => panic!("unexpected command: {command:?}"),
        }
    });
    daemon.add_provider("zai").await.expect("added");
    responder.await.expect("responder finished");
}
```

For removal, seed both `Kind::Key` and `Kind::Token` for `zai/default`, answer `Command::RemoveProvider`, and assert `FakeSecrets::held()` is empty. Add a `FailingDeleteSecrets` fake returning `SecretError::Locked` and assert no engine command is sent.

- [ ] **Step 5: Run the topology service tests and observe missing D-Bus methods**

Run: `cargo test -p tidemarkd service::tests -- --nocapture`

Expected: compilation fails because `add_provider` and `remove_provider` do not exist.

- [ ] **Step 6: Implement D-Bus add/remove and idempotent login cancellation**

Add a private helper:

```rust
async fn topology_request(
    &self,
    make: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Command,
) -> fdo::Result<()> {
    let (reply, answer) = oneshot::channel();
    self.commands.send(make(reply)).await
        .map_err(|_| fdo::Error::Failed("the poll loop has stopped".into()))?;
    answer.await
        .map_err(|_| fdo::Error::Failed("the poll loop dropped the request".into()))?
        .map_err(fdo::Error::Failed)
}
```

`remove_provider` first validates the configured account, removes and aborts any pending login, deletes `Kind::Key`, then `Kind::Token`, and only then sends `Command::RemoveProvider`. Missing secret entries remain successful through the `Secrets` contract. Do not call history.

- [ ] **Step 7: Emit and integration-test `ProviderRemoved`**

Add the signal declaration beside `provider_changed`. In `main.rs`, handle `Publication::Removed` by calling `Published::remove` before emitting the signal. Extend the real-session-bus test to subscribe to `ProviderRemoved`, emit it, deserialize `(String, String)`, and assert `("zai", "default")`.

- [ ] **Step 8: Run daemon tests**

Run: `cargo test -p tidemarkd -- --nocapture`

Expected: all D-Bus, credential, engine, registry, and scheduler tests pass.

- [ ] **Step 9: Commit the D-Bus topology contract**

```bash
git add crates/tidemarkd/src/service.rs crates/tidemarkd/src/main.rs
git commit -m "Expose provider management over D-Bus"
```

---

### Task 6: Teach the GUI Bus and Main Window About Catalog and Removal

**Files:**
- Modify: `crates/tidemark/src/bus.rs:34-185`
- Modify: `crates/tidemark/src/window.rs:20-300`

**Interfaces:**
- Consumes: Task 5 D-Bus methods/signals and `ProviderDefinition`.
- Produces:

```rust
pub enum Update {
    Connected(DaemonProxy<'static>, Vec<ProviderDefinition>, Vec<ProviderStatus>),
    Changed(ProviderStatus),
    Removed { provider: String, account: String },
    Waiting(String),
}
```

- [ ] **Step 1: Extend the generated client proxy**

Add `list_providers`, `add_provider`, `remove_provider`, and `provider_removed` to the `Daemon` proxy trait with the exact signatures from Task 5.

- [ ] **Step 2: Add a failing pure removal test to `window.rs`**

Extract identity matching before touching widgets:

```rust
fn account_index(statuses: &[ProviderStatus], provider: &str, account: &str) -> Option<usize> {
    statuses.iter().position(|status| status.provider == provider && status.account == account)
}

#[test]
fn removal_matches_the_full_provider_account_identity() {
    let statuses = vec![status("zai", "first"), status("zai", "default")];
    assert_eq!(account_index(&statuses, "zai", "default"), Some(1));
    assert_eq!(account_index(&statuses, "kimi", "default"), None);
}
```

Use a local test helper that constructs `ProviderStatus::pending` from both slugs.

- [ ] **Step 3: Run the window test and observe the missing helper**

Run: `cargo test -p tidemark window::tests::removal_matches -- --nocapture`

Expected: compilation fails because the helper and test module do not exist.

- [ ] **Step 4: Load catalog and multiplex the removal signal**

In `bus::load`, call `list_providers` and `get_status`; send `Connected` only when both succeed. Add a third pinned stream for `receive_provider_removed` and a matching `Event::Removed`. Deserialize its `provider` and `account` arguments into `Update::Removed`.

- [ ] **Step 5: Store definitions and remove cards in place**

Add `definitions: RefCell<Vec<ProviderDefinition>>` to `MainWindow`. On `Connected`, replace it and call `show_all`. On `Removed`, locate the `Card` by its current `status`, remove its widget from `FlowBox`, remove the `Rc<Card>` from `cards`, and update the open settings dialog.

When the last card is removed, show:

```rust
self.show_message(
    "view-grid-symbolic",
    "Welcome to Tidemark",
    "Add a provider to start tracking your quota.",
);
```

Use the same copy when `show_all` receives an empty vector. Keep the providers button sensitive whenever the daemon is connected.

- [ ] **Step 6: Run GUI unit tests and the layering guard**

Run: `cargo test -p tidemark && scripts/check-layering.sh`

Expected: GUI tests pass and the script prints `layering ok`.

- [ ] **Step 7: Commit client-side topology updates**

```bash
git add crates/tidemark/src/bus.rs crates/tidemark/src/window.rs
git commit -m "Update cards when provider topology changes"
```

---

### Task 7: Replace the Credentials Sheet with Navigable Provider Settings

**Files:**
- Delete: `crates/tidemark/src/credentials.rs`
- Create: `crates/tidemark/src/provider_settings/mod.rs`
- Create: `crates/tidemark/src/provider_settings/model.rs`
- Create: `crates/tidemark/src/provider_settings/list.rs`
- Create: `crates/tidemark/src/provider_settings/detail.rs`
- Modify: `crates/tidemark/src/main.rs:1-18`
- Modify: `crates/tidemark/src/window.rs:20-280`
- Modify: `crates/tidemark/src/mark.rs:20-90`

**Interfaces:**
- Consumes: `DaemonProxy`, `ProviderDefinition`, `ProviderStatus`, provider mark naming, all existing key/OAuth/option D-Bus methods.
- Produces:
  - `ProviderSettings::present(parent, proxy, definitions, statuses, on_closed) -> Rc<ProviderSettings>`
  - `ProviderSettings::apply(&self, definitions, statuses)`
  - pure `model::addable` and `model::connection_text`
  - reusable `mark::image_at(pixel_size: i32)`

- [ ] **Step 1: Write failing pure model tests**

```rust
#[test]
fn search_is_case_insensitive_and_excludes_added_providers() {
    let definitions = vec![definition("claude", "Claude"), definition("codex", "Codex")];
    let statuses = vec![status("claude", ProviderState::Ok, Some(false))];
    let matches = addable(&definitions, &statuses, "CODE");
    assert_eq!(matches.iter().map(|item| item.provider.as_str()).collect::<Vec<_>>(), ["codex"]);
    assert!(addable(&definitions, &statuses, "claude").is_empty());
}

#[test]
fn oauth_fallback_copy_never_claims_an_unverified_session() {
    let definition = oauth_definition("antigravity", Some("agy session"));
    assert_eq!(connection_text(&definition, &status("antigravity", ProviderState::Pending, Some(false))), "Checking for agy session…");
    assert_eq!(connection_text(&definition, &status("antigravity", ProviderState::Ok, Some(false))), "Using agy session");
    assert_eq!(connection_text(&definition, &status("antigravity", ProviderState::NoCredential, Some(false))), "Not signed in");
    assert_eq!(connection_text(&definition, &status("antigravity", ProviderState::Ok, Some(true))), "Signed in through Tidemark");
}
```

Define the `definition`, `oauth_definition`, and `status` helpers entirely inside `model.rs`'s test module.

- [ ] **Step 2: Run the model tests and observe the missing module**

Run: `cargo test -p tidemark provider_settings::model::tests -- --nocapture`

Expected: compilation fails because `provider_settings` and its model do not exist.

- [ ] **Step 3: Implement the pure model and mark sizing**

`addable` lowercases the trimmed query with `to_lowercase`, excludes every definition whose slug appears in statuses, and matches title or slug. `connection_text` implements the four approved strings exactly. Add:

```rust
pub fn image_at(pixel_size: i32) -> gtk::Image {
    gtk::Image::builder()
        .pixel_size(pixel_size)
        .valign(gtk::Align::Center)
        .visible(false)
        .build()
}

pub fn image() -> gtk::Image {
    image_at(SIZE)
}
```

- [ ] **Step 4: Move the existing single-account controls into `detail.rs`**

Refactor the current `Credentials::build_key_rows`, `build_sign_in_row`, `build_option_row`, `sign_in`, `set_waiting`, `caption`, and D-Bus error formatting around one `ProviderDefinition` plus one `ProviderStatus`. Keep the secret field empty after save. Preserve Copy link, Cancel, toast text, and the rule that a provider-specific option change does not overwrite text typed into the credential field.

The detail page's header content is a centered vertical box containing `mark::image_at(72)` and an `adw::WindowTitle` with the definition title. Put authentication and options in separate `adw::PreferencesGroup`s.

- [ ] **Step 5: Build the configured list and searchable picker in `list.rs`**

For each configured status, create an `adw::ActionRow` with mark, title, `model::connection_text`, an edit button using `document-edit-symbolic`, and a remove button using `user-trash-symbolic` plus the `destructive-action` CSS class. The `+` header button pushes a `gtk::SearchEntry` and scrollable list of `model::addable` definitions.

Selecting a picker row calls `proxy.add_provider(slug)`. On success, keep the provider in the dialog's local configured set, rebuild both lists, and push its detail page. On failure, leave the picker visible and show the daemon's one-sentence error as a toast.

Removal uses:

```rust
let confirmation = adw::AlertDialog::builder()
    .heading(format!("Remove {}?", definition.title))
    .body("This removes the provider and its saved credentials. Quota history will be kept.")
    .build();
confirmation.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
confirmation.set_default_response(Some("cancel"));
confirmation.set_close_response("cancel");
confirmation.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
if confirmation.choose_future(Some(&dialog)).await == "remove" {
    if let Err(error) = proxy.remove_provider(&provider, &account).await {
        dialog.add_toast(adw::Toast::new(&reason(&error)));
    }
}
```

Do not close the dialog after removal.

- [ ] **Step 6: Implement the dialog controller and empty list copy**

`ProviderSettings` owns the `adw::PreferencesDialog`, proxy, definitions, statuses, and current rows. Its main page displays `No providers added` / `Use + to add a provider.` when statuses are empty. `apply` updates existing status summaries, adds newly configured rows, and removes deleted rows without rebuilding a credential entry currently being edited. Step 8 adds the pending-login tracker after its lifecycle test is red.

- [ ] **Step 7: Write the failing dialog-slot regression test**

Add this test to `window.rs` before defining `DialogSlot`:

```rust
#[test]
fn a_closed_dialog_slot_can_be_filled_again() {
    let slot = DialogSlot::default();
    assert!(slot.insert_if_empty(Rc::new(1)));
    assert!(!slot.insert_if_empty(Rc::new(2)));
    slot.clear();
    assert!(slot.insert_if_empty(Rc::new(3)));
}

#[test]
fn pending_logins_are_taken_once_for_cancellation() {
    let pending = PendingLogins::default();
    pending.insert("antigravity", "default");
    assert_eq!(pending.take_all(), vec![("antigravity".into(), "default".into())]);
    assert!(pending.take_all().is_empty());
}
```

Run: `cargo test -p tidemark window::tests::a_closed_dialog_slot -- --nocapture`

Expected: compilation fails because `DialogSlot` and `PendingLogins` do not exist.

- [ ] **Step 8: Implement the slot, wire `AdwDialog::closed`, and cancel logins**

Add the generic slot in `window.rs`:

```rust
#[derive(Debug, Default)]
struct DialogSlot<T>(RefCell<Option<Rc<T>>>);

impl<T> DialogSlot<T> {
    fn insert_if_empty(&self, value: Rc<T>) -> bool {
        let mut held = self.0.borrow_mut();
        if held.is_some() { return false; }
        *held = Some(value);
        true
    }

    fn clear(&self) {
        self.0.borrow_mut().take();
    }
}

#[derive(Debug, Default)]
struct PendingLogins(RefCell<HashSet<(String, String)>>);

impl PendingLogins {
    fn insert(&self, provider: &str, account: &str) {
        self.0.borrow_mut().insert((provider.into(), account.into()));
    }

    fn take_all(&self) -> Vec<(String, String)> {
        self.0.borrow_mut().drain().collect()
    }
}
```

Replace `MainWindow.credentials` with `DialogSlot<ProviderSettings>`. When presenting, install an `AdwDialog::closed` callback that:

1. drains `PendingLogins::take_all()` and asynchronously calls the idempotent D-Bus `cancel_login` method for every returned identity;
2. clears the main window's slot through a `Weak<MainWindow>`;
3. lets the final `Rc` drop.

Clicking while the slot is occupied does nothing; clicking after `closed` builds and presents a new dialog. Do not inspect `GtkWidget.visible` anywhere.

Run: `cargo test -p tidemark window::tests::a_closed_dialog_slot -- --nocapture`

Expected: PASS, and the production path uses the same tested slot.

- [ ] **Step 9: Test dialog model, GUI crate, and layering**

Run: `cargo test -p tidemark && scripts/check-layering.sh`

Expected: model/lifecycle tests pass and `layering ok` confirms the GUI still has no network or core dependency.

- [ ] **Step 10: Commit the scalable settings UI**

```bash
git add crates/tidemark/src/main.rs crates/tidemark/src/window.rs crates/tidemark/src/mark.rs crates/tidemark/src/provider_settings crates/tidemark/src/credentials.rs
git commit -m "Build scalable provider settings navigation"
```

---

### Task 8: Support OAuth Clients That Require a Public Client Secret

**Files:**
- Modify: `crates/tidemark-core/src/oauth.rs:54-390`
- Modify: `crates/tidemark-core/src/providers/claude.rs:54-70`
- Modify: `crates/tidemark-core/src/providers/codex.rs:54-70`

**Interfaces:**
- Consumes: existing PKCE/state/callback flow and form/JSON token encodings.
- Produces: a new `pub client_secret: Option<&'static str>` field on `oauth::Client` and an exchange that includes `client_secret` only when present.

- [ ] **Step 1: Add a failing token-exchange test**

Extend the existing local OAuth test server and test client:

```rust
#[test]
fn a_provider_client_secret_is_sent_only_when_declared() {
    let secret = test_client(0, Encoding::Form, Some("desktop-public-secret"));
    let body = finish_against_server(secret).expect("exchange succeeds");
    assert!(body.contains("client_secret=desktop-public-secret"));

    let public = test_client(0, Encoding::Form, None);
    let body = finish_against_server(public).expect("exchange succeeds");
    assert!(!body.contains("client_secret="));
}
```

Make the existing `test_client` helper accept the exact third argument rather than creating another constructor.

- [ ] **Step 2: Run the focused OAuth test and observe the missing field**

Run: `cargo test -p tidemark-core oauth::tests::a_provider_client_secret -- --nocapture`

Expected: compilation fails because `Client::client_secret` and the new helper argument do not exist.

- [ ] **Step 3: Add the optional field and build exchange fields dynamically**

Add `pub client_secret: Option<&'static str>` after `client_id`. Replace the fixed exchange array with a `Vec<(&str, &str)>`, push `("client_secret", secret)` only for `Some(secret)`, and feed that vector to either form or JSON encoding. Set `client_secret: None` in Claude, Codex, and all OAuth tests not exercising the field.

- [ ] **Step 4: Run all OAuth and existing provider refresh tests**

Run: `cargo test -p tidemark-core oauth::tests -- --nocapture && cargo test -p tidemark-core providers::claude::tests -- --nocapture && cargo test -p tidemark-core providers::codex::tests -- --nocapture`

Expected: every loopback, state, exchange, Claude, and Codex test passes unchanged.

- [ ] **Step 5: Commit the OAuth primitive**

```bash
git add crates/tidemark-core/src/oauth.rs crates/tidemark-core/src/providers/claude.rs crates/tidemark-core/src/providers/codex.rs
git commit -m "Support OAuth desktop client secrets"
```

---

### Task 9: Parse Direct Antigravity Quota Without `agy`

**Files:**
- Create: `crates/tidemark-core/src/providers/antigravity/direct.rs`
- Create: `crates/tidemark-core/tests/fixtures/antigravity-available-models.json`
- Modify: `crates/tidemark-core/src/providers/antigravity/mod.rs:39-55`

**Interfaces:**
- Consumes: `Snapshot`, `Window`, `WindowKey::for_pool`, `WindowLength`, `Timestamp`, `ProviderError`.
- Produces:
  - `direct::parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError>`
  - `pub async fn direct::fetch(client: &reqwest::Client, endpoint: &str, access_token: &str, project_id: &str) -> Result<Snapshot, ProviderError>`

- [ ] **Step 1: Add a representative direct-API fixture**

Create `antigravity-available-models.json` with this exact body. It gives the parser four logical counters while exercising shared counters, daily/weekly variants, and a missing fraction that means exhausted:

```json
{
  "models": {
    "gemini-3-pro-high": {
      "displayName": "Gemini 3 Pro",
      "modelProvider": "MODEL_PROVIDER_GOOGLE",
      "weeklyQuotaInfo": {
        "remainingFraction": 0.75,
        "resetTime": "2026-08-28T00:00:00Z",
        "windowId": "weekly",
        "windowLabel": "Weekly"
      }
    },
    "gemini-3-pro-low": {
      "displayName": "Gemini 3 Pro",
      "modelProvider": "MODEL_PROVIDER_GOOGLE",
      "weeklyQuotaInfo": {
        "remainingFraction": 0.50,
        "resetTime": "2026-08-28T00:00:00Z",
        "windowId": "weekly",
        "windowLabel": "Weekly"
      }
    },
    "gemini-3-flash": {
      "displayName": "Gemini 3 Flash",
      "modelProvider": "MODEL_PROVIDER_GOOGLE",
      "dailyQuotaInfo": {
        "remainingFraction": 0.90,
        "resetTime": "2026-08-22T00:00:00Z",
        "windowId": "daily",
        "windowLabel": "Daily"
      }
    },
    "claude-sonnet": {
      "displayName": "Claude Sonnet",
      "modelProvider": "MODEL_PROVIDER_ANTHROPIC",
      "weeklyQuotaInfo": {
        "remainingFraction": 0.20,
        "resetTime": "2026-08-28T00:00:00Z",
        "windowId": "weekly",
        "windowLabel": "Weekly"
      }
    },
    "gpt-oss": {
      "displayName": "GPT OSS",
      "modelProvider": "MODEL_PROVIDER_OPENAI",
      "quotaInfo": {
        "resetTime": "2026-08-28T00:00:00Z",
        "windowId": "weekly",
        "windowLabel": "Weekly"
      }
    }
  }
}
```

- [ ] **Step 2: Write failing parser tests in `direct.rs`**

```rust
#[test]
fn shared_models_become_one_counter_and_exhausted_is_not_masked() {
    let now = Timestamp::from_unix(1_787_270_400).expect("2026-08-21");
    let snapshot = parse(include_str!("../../../tests/fixtures/antigravity-available-models.json"), now)
        .expect("fixture parses");
    assert_eq!(snapshot.provider.as_str(), "antigravity");
    assert_eq!(snapshot.account.as_str(), "default");
    assert_eq!(snapshot.windows.len(), 4);
    assert!(snapshot.windows.iter().any(|window| {
        window.key.as_str().contains("openai") && window.used_percent == 100.0
    }));
}

#[test]
fn one_healthy_model_cannot_hide_an_exhausted_shared_counter() {
    let body = r#"{"models":{"claude-a":{"modelProvider":"MODEL_PROVIDER_ANTHROPIC","quotaInfo":{"remainingFraction":0.8,"resetTime":"2026-08-28T00:00:00Z"}},"claude-b":{"modelProvider":"MODEL_PROVIDER_ANTHROPIC","quotaInfo":{"remainingFraction":0.0,"resetTime":"2026-08-28T00:00:00Z"}}}}"#;
    let snapshot = parse(body, Timestamp::from_unix(1_787_270_400).expect("plausible"))
        .expect("parses");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].used_percent, 100.0);
}
```

- [ ] **Step 3: Run the parser tests and observe the missing module**

Run: `cargo test -p tidemark-core providers::antigravity::direct::tests -- --nocapture`

Expected: compilation fails because `direct` does not exist.

- [ ] **Step 4: Implement tolerant shape decoding and strict known-entry validation**

Deserialize `models: BTreeMap<String, ModelInfo>`. Normalize `quotaInfo`, `quotaInfos`, daily/weekly spellings, and by-window/by-tier maps into a flat vector. Derive counter names from `modelProvider`/`apiProvider` (`google`, `anthropic`, `openai`), classify explicit daily/weekly ids before using reset distance as a fallback, and deduplicate by `(counter, tier, window id)` using the minimum remaining fraction and earliest valid reset. Treat missing fraction plus a present reset as exhausted; reject non-finite fractions and invalid present timestamps. Build stable keys with `WindowKey::for_pool(&pool, length)` when length is known and `WindowKey::named(&format!("{pool}/{window_id}"))` only when the API gives no duration.

- [ ] **Step 5: Rerun parser tests and add malformed-known-entry coverage**

Run: `cargo test -p tidemark-core providers::antigravity::direct::tests -- --nocapture`

Expected: all direct parser tests pass, including malformed reset and duplicate-key rejection.

- [ ] **Step 6: Add the direct request function and HTTP capture test**

`fetch` POSTs `{ "project": project_id }` to `{endpoint}/v1internal:fetchAvailableModels` with bearer authorization, JSON content type, and the shared `Tidemark/<version>` client. Copy the fixed-script `local_server` helper from `crates/tidemark-core/src/providers/codex.rs` into this test module, then add:

```rust
#[test]
fn direct_fetch_sends_the_project_and_owned_bearer_token() {
    let fixture = include_str!("../../../tests/fixtures/antigravity-available-models.json");
    let fixture: &'static str = Box::leak(fixture.to_owned().into_boxed_str());
    let (base, requests, server) = local_server(vec![(200, fixture)]);
    let client = crate::providers::http::client().expect("client");
    let snapshot = block_on(fetch(&client, &base, "owned-access", "project-1"))
        .expect("quota fetch succeeds");

    assert_eq!(snapshot.provider.as_str(), "antigravity");
    let request = requests.recv().expect("request captured");
    assert!(request.starts_with("POST /v1internal:fetchAvailableModels "), "{request}");
    assert!(request.contains("authorization: Bearer owned-access"), "{request}");
    assert!(request.contains("user-agent: Tidemark/"), "{request}");
    assert!(request.contains(r#"{\"project\":\"project-1\"}"#), "{request}");
    server.join().expect("server stopped");
}
```

Add the same `block_on` helper used by the Codex provider tests.

- [ ] **Step 7: Run Antigravity local and direct tests**

Run: `cargo test -p tidemark-core providers::antigravity -- --nocapture`

Expected: both existing `agy` payload tests and new direct payload tests pass.

- [ ] **Step 8: Commit direct quota parsing**

```bash
git add crates/tidemark-core/src/providers/antigravity/direct.rs crates/tidemark-core/src/providers/antigravity/mod.rs crates/tidemark-core/tests/fixtures/antigravity-available-models.json
git commit -m "Parse Antigravity quota directly"
```

---

### Task 10: Add Antigravity OAuth, Project Discovery, Refresh, and `agy` Fallback

**Files:**
- Create: `crates/tidemark-core/src/providers/antigravity/oauth.rs`
- Modify: `crates/tidemark-core/src/providers/antigravity/mod.rs:1-130`
- Modify: `crates/tidemark-core/src/providers/antigravity/agy.rs:270-300`
- Modify: `crates/tidemarkd/src/registry.rs:95-185`
- Modify: `crates/tidemarkd/src/service.rs:246-300`

**Interfaces:**
- Consumes: optional-secret OAuth client, Secret Service token documents, direct quota fetcher, existing `Agy` supervisor.
- Produces:
  - `antigravity::oauth::client() -> crate::oauth::Client`
  - `pub async fn antigravity::oauth::complete_login(response: &serde_json::Value, now_ms: i64) -> Result<serde_json::Value, ProviderError>`
  - `Antigravity::new(own: Option<Arc<dyn Secrets>>) -> Result<Self, ProviderError>`
  - `agy::is_available() -> bool`
  - `pub async fn registry::login_document(provider: &str, response: &serde_json::Value, now_ms: i64) -> Result<serde_json::Value, ProviderError>` for Antigravity project discovery.

- [ ] **Step 1: Write failing OAuth metadata and document tests**

```rust
#[test]
fn antigravity_uses_the_registered_google_callback_and_offline_scopes() {
    let client = oauth::client();
    assert_eq!(client.redirect_port, 51_121);
    assert_eq!(client.redirect_path, "/oauth-callback");
    assert!(client.scopes.contains("cloud-platform"));
    assert!(client.scopes.contains("userinfo.email"));
    assert!(client.authorize_extras.contains(&("access_type", "offline")));
    assert!(client.client_secret.is_some());
}

#[test]
fn login_document_keeps_tokens_expiry_and_project() {
    let document = document_from_login(
        &serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":3600}),
        "project-1",
        1_787_270_400_000,
    ).expect("valid");
    assert_eq!(document["access_token"], "a");
    assert_eq!(document["refresh_token"], "r");
    assert_eq!(document["project_id"], "project-1");
    assert_eq!(document["expires_at"], 1_787_274_000_000_i64);
}
```

- [ ] **Step 2: Run the OAuth tests and observe the missing module**

Run: `cargo test -p tidemark-core providers::antigravity::oauth::tests -- --nocapture`

Expected: compilation fails because the Antigravity OAuth module does not exist.

- [ ] **Step 3: Implement exact Google OAuth metadata**

Use the Antigravity desktop client id and public client secret already recorded in the approved protocol references. Set:

```rust
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
const REDIRECT_PORT: u16 = 51_121;
const REDIRECT_PATH: &str = "/oauth-callback";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const API_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
```

Use `Encoding::Form`, `client_secret: Some(CLIENT_SECRET)`, and authorize extras `access_type=offline`, `prompt=consent`, and `include_granted_scopes=true`.

- [ ] **Step 4: Write failing project-discovery tests against a local server**

Copy the fixed-script `local_server` helper used by the Codex provider tests and add an internal seam with this exact signature:

```rust
async fn complete_login_at(
    client: &reqwest::Client,
    endpoints: &[String],
    response: &serde_json::Value,
    now_ms: i64,
    retry_delay: Duration,
) -> Result<serde_json::Value, ProviderError>
```

Then write these three tests; `token_response()` returns access `owned-access`, refresh `owned-refresh`, and a one-hour expiry:

```rust
#[test]
fn an_existing_cloud_companion_project_skips_onboarding() {
    let load = r#"{"cloudaicompanionProject":{"id":"project-1"}}"#;
    let (base, requests, server) = local_server(vec![(200, load)]);
    let client = crate::providers::http::client().expect("client");
    let document = block_on(complete_login_at(
        &client,
        &[base],
        &token_response(),
        1_787_270_400_000,
        Duration::ZERO,
    )).expect("project discovered");

    assert_eq!(document["project_id"], "project-1");
    let request = requests.recv().expect("load request captured");
    assert!(request.starts_with("POST /v1internal:loadCodeAssist "), "{request}");
    assert!(request.contains("authorization: Bearer owned-access"), "{request}");
    assert!(request.contains(r#"\"ideType\":\"ANTIGRAVITY\""#), "{request}");
    assert!(request.contains("user-agent: Tidemark/"), "{request}");
    assert!(requests.try_recv().is_err());
    server.join().expect("server stopped");
}

#[test]
fn onboarding_is_bounded_and_returns_its_project() {
    let load = r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#;
    let pending = r#"{"done":false}"#;
    let done = r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"project-2"}}}"#;
    let (base, requests, server) = local_server(vec![(200, load), (200, pending), (200, done)]);
    let client = crate::providers::http::client().expect("client");
    let document = block_on(complete_login_at(
        &client,
        &[base],
        &token_response(),
        1_787_270_400_000,
        Duration::ZERO,
    )).expect("onboarding completes");

    assert_eq!(document["project_id"], "project-2");
    let _load = requests.recv().expect("load captured");
    for _ in 0..2 {
        let request = requests.recv().expect("onboard captured");
        assert!(request.starts_with("POST /v1internal:onboardUser "), "{request}");
        assert!(request.contains(r#"\"tierId\":\"free-tier\""#), "{request}");
    }
    server.join().expect("server stopped");
}

#[test]
fn incomplete_onboarding_stops_after_five_attempts() {
    let load = r#"{"allowedTiers":[]}"#;
    let pending = r#"{"done":false}"#;
    let mut responses = vec![(200, load)];
    responses.extend(std::iter::repeat_n((200, pending), 5));
    let (base, requests, server) = local_server(responses);
    let client = crate::providers::http::client().expect("client");
    let error = block_on(complete_login_at(
        &client,
        &[base],
        &token_response(),
        1_787_270_400_000,
        Duration::ZERO,
    )).expect_err("provisioning must be bounded");

    assert!(matches!(error, ProviderError::Malformed(message) if message.contains("provision")));
    assert_eq!(requests.into_iter().count(), 6);
    server.join().expect("server stopped");
}
```

- [ ] **Step 5: Implement project discovery and login completion**

`complete_login` builds the shared HTTP client, converts `API_ENDPOINTS` to owned strings, and calls `complete_login_at` with a two-second retry delay. Try the endpoints in declared order for `v1internal:loadCodeAssist`. Prefer an existing project id. Otherwise select the default allowed tier, falling back to `free-tier`, POST `v1internal:onboardUser`, and retry at most five times. Store the returned project with the token fields.

- [ ] **Step 6: Write failing refresh-rotation tests**

First copy the in-memory `FakeSecrets` implementation from the Claude provider tests. Seed it with this expired document:

```rust
serde_json::json!({
    "access_token": "old",
    "refresh_token": "old-refresh",
    "expires_at": 1_787_270_399_000_i64,
    "project_id": "project-1"
})
```

Use the local server with a token response `{"access_token":"new","refresh_token":"rotated","expires_in":3600}` followed by the direct quota fixture. Build the provider with the test constructor introduced in Step 7, call `fetch_inner`, and assert:

```rust
let refresh_request = requests.recv().expect("refresh captured");
assert!(refresh_request.starts_with("POST /token "), "{refresh_request}");
assert!(refresh_request.contains("refresh_token=old-refresh"), "{refresh_request}");
let quota_request = requests.recv().expect("quota captured");
assert!(quota_request.contains("authorization: Bearer new"), "{quota_request}");
let stored = secrets.document().expect("rotated document");
assert_eq!(stored["refresh_token"], "rotated");
assert_eq!(stored["project_id"], "project-1");
```

Add the same test with a token response that omits `refresh_token` and assert `stored["refresh_token"] == "old-refresh"`.

- [ ] **Step 7: Implement credential loading, refresh, and direct fetch**

Use this narrow private seam around the existing local transport:

```rust
trait LocalQuota: std::fmt::Debug + Send + Sync {
    fn available(&self) -> bool;
    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>>;
}
```

Implement it for a small `AgyQuota` wrapper without changing `Agy` supervision or parsing. `Antigravity` owns the standard HTTP client, an optional `Arc<dyn Secrets>`, direct and token endpoint strings, and `Box<dyn LocalQuota>`. Keep `Antigravity::new(own)` as the production constructor and add this private test-only constructor:

```rust
#[cfg(test)]
fn with_endpoints_and_local(
    own: Option<Arc<dyn Secrets>>,
    direct_endpoint: String,
    token_endpoint: String,
    local: Box<dyn LocalQuota>,
) -> Result<Self, ProviderError>
```

Its fetch order is exact:

```rust
match self.own_token().await? {
    Some(credentials) => self.fetch_direct(credentials).await,
    None if agy::is_available() => self.fetch_from_agy().await,
    None => Err(ProviderError::NoCredential),
}
```

An owned token that fails refresh or direct authorization returns that error and never reaches `agy`. Refresh at least five minutes before expiry, POST form fields `client_id`, `client_secret`, `refresh_token`, and `grant_type=refresh_token`, write the rotated document before quota fetch, and preserve the project id.

- [ ] **Step 8: Add fallback tests**

Add `FakeLocal { available: bool, calls: Arc<AtomicUsize>, result: Mutex<Option<Result<Snapshot, ProviderError>>> }` implementing `LocalQuota`. Use the local HTTP server as the direct transport and assert these exact outcomes:

```rust
assert_eq!(local_calls.load(Ordering::SeqCst), 0); // stored OAuth used direct once
assert_eq!(direct_requests.into_iter().count(), 1);

assert_eq!(local_calls.load(Ordering::SeqCst), 1); // no stored token used valid local

assert!(matches!(error, ProviderError::NoCredential)); // neither source available
assert_eq!(local_calls.load(Ordering::SeqCst), 0);

assert!(matches!(error, ProviderError::Credential { status: 401 }));
assert_eq!(local_calls.load(Ordering::SeqCst), 0); // rejected owned OAuth never fell back
```

Name the four tests `stored_oauth_wins_over_local`, `local_is_used_only_without_an_owned_token`, `missing_owned_and_local_credentials_is_no_credential`, and `rejected_owned_oauth_does_not_fall_back`.

Expose `agy::is_available()` as a pure wrapper around the existing binary resolution. Keep process supervision and local RPC parsing unchanged.

- [ ] **Step 9: Register Antigravity as OAuth with fallback**

Change its catalog definition to `CredentialKind::OAuth`, `external_fallback: Some("agy session")`, and hint `Sign in with Google through Tidemark, or use an existing agy session.` Construct it with `Some(Arc::clone(secrets))`. Add it to `registry::oauth_client` and make `registry::login_document` async so Antigravity can finish project discovery before Secret Service storage; Claude and Codex still return their existing documents immediately.

In `service::begin_login`, await the async registry completion inside the spawned login task before calling `secrets.set`.

- [ ] **Step 10: Run focused OAuth/provider/daemon tests**

Run: `cargo test -p tidemark-core providers::antigravity -- --nocapture && cargo test -p tidemark-core oauth::tests -- --nocapture && cargo test -p tidemarkd registry::tests -- --nocapture && cargo test -p tidemarkd service::tests -- --nocapture`

Expected: direct OAuth, refresh, project discovery, `agy` fallback, registry metadata, and login service tests pass.

- [ ] **Step 11: Commit Antigravity OAuth**

```bash
git add crates/tidemark-core/src/providers/antigravity crates/tidemarkd/src/registry.rs crates/tidemarkd/src/service.rs
git commit -m "Authenticate Antigravity without agy"
```

---

### Task 11: Update the Public Contract and Run Final Verification

**Files:**
- Modify: `CONTEXT.md:64-80,145-180,330-345`
- Modify: `README.md:60-76`
- Modify: `docs/adr/0003-loopback-port-is-the-providers-to-choose.md`

**Interfaces:**
- Consumes: all completed behavior from Tasks 1-10.
- Produces: English documentation matching the shipped config, UI, D-Bus, and Antigravity credential order.

- [ ] **Step 1: Update documentation assertions**

Document all of these exact facts:

- fresh installs have `providers = []` semantically, even before a config file exists;
- the provider catalog and configured accounts are separate D-Bus concepts;
- `ListProviders`, `AddProvider`, `RemoveProvider`, and `ProviderRemoved` are public interface members;
- settings use list → searchable picker → provider detail navigation;
- removal deletes Tidemark credentials/settings/cards but retains history;
- Antigravity prefers Tidemark Google OAuth and directly calls Cloud Code Assist, with `agy` as an optional fallback;
- Antigravity owns fixed callback port `51121` and path `/oauth-callback` under ADR 0003's provider-owned-port rule;
- the main empty-state copy is `Welcome to Tidemark` / `Add a provider to start tracking your quota.`

- [ ] **Step 2: Scan all changed text for the English-only rule and diff errors**

Run:

```bash
rg -n '[А-Яа-яЁё]' CONTEXT.md README.md docs crates
git diff --check
```

Expected: `rg` returns no matches and `git diff --check` exits successfully.

- [ ] **Step 3: Format and lint every target**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: both commands exit 0 with no warnings.

- [ ] **Step 4: Run the full automated suite with loopback access available**

Run:

```bash
cargo test --workspace
scripts/check-layering.sh
```

Expected: every test passes and the layering script prints `layering ok`. If the execution sandbox denies loopback binds with `Operation not permitted`, rerun the same test command with the environment's approved local-network permission; do not weaken or skip the tests.

- [ ] **Step 5: Perform the GUI lifecycle smoke check without touching existing user data**

Run the daemon and GUI against task-specific temporary XDG directories and an isolated session bus that has a disposable Secret Service. Confirm this sequence:

1. Main window shows `Welcome to Tidemark` and the add-provider instruction.
2. Open settings, close it, and open it again.
3. Add a provider from the searchable picker and reach its detail page.
4. Return to the list, use edit, and reach the same detail page.
5. Remove the provider, accept the destructive confirmation, and see its card disappear.
6. Re-add it and confirm its prior history is still available.

Do not perform steps 5-6 against the developer's normal session bus or normal XDG directories because removal intentionally deletes Tidemark-owned credentials.

- [ ] **Step 6: Review the final diff against every spec section**

Check Configuration, D-Bus Contract, Dynamic Daemon State, all three Settings Interface subsections, Antigravity OAuth and Direct Quota Fetching, Dialog Lifecycle Fix, Error Handling, and Test Strategy. Confirm each has corresponding code and a test; remove any behavior not justified by the spec.

- [ ] **Step 7: Commit documentation and verification-ready state**

```bash
git add CONTEXT.md README.md docs/adr/0003-loopback-port-is-the-providers-to-choose.md
git commit -m "Document managed providers and Antigravity OAuth"
```
