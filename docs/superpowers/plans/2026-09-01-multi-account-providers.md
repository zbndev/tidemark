# Multi-Account Providers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let one provider carry several credentialled accounts (a work and a personal Claude, two Z.ai keys), created, named, shown, ordered and removed from the UI.

**Architecture:** The daemon's plumbing is already per-account (`AccountId` flows through
secrets, history, the wire and the tray); only *construction* is hard-coded to one account
per provider. The plan makes config name an ordered account list per provider, teaches the
registry/engine to build them, adds two D-Bus methods (`AddAccount`, `SetAccountOrder`),
and builds the settings rows and the grouped-card main window on top.

**Tech Stack:** Rust 2024 (MSRV 1.92), Tokio + zbus (daemon), GTK4 + libadwaita programmatic
UI (no `.ui`/gresource), rusqlite, oo7 Secret Service.

**Spec:** `docs/superpowers/specs/2026-09-01-multi-account-providers-design.md` — read it first; this plan argues from it.

## Global Constraints

- Work only on branch `feat/multi-account-providers`. Never commit to `main`.
- Gate after every task: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`. Run the *targeted* test during a task; run the full gate before each commit that crosses a crate boundary.
- `tidemark-types` reaches nothing (only `serde`+`zvariant`); `tidemark-core` never touches the display; `tidemark` (UI) speaks only D-Bus — no `tidemark-core`/`reqwest`/`rusqlite` deps. `check-layering.sh` enforces it.
- No `anyhow`; contextual `thiserror` enums per domain (`ConfigError`, `StorageError`, `ProviderError`, `SecretError`).
- Secrets keyed `(Kind, ProviderId, AccountId)`; never log a `Credential`.
- History tables key on `(provider, account, window, …)`; **no schema change** — account column already exists.
- `providers = [...]` config array stays a flat provider list (the `SetOrder` permutation contract). Accounts live under `[provider.<slug>] accounts = [...]`.
- `"default"` is always present and always first in a provider's account list.
- Extra OAuth accounts are Tidemark-login only (no CLI source). `None`/`External` providers stay single-account.
- `ProviderStatus` is `a{sv}`: new keys are added, absent means absent; old daemons omit them.
- Tests: built-in `#[test]`/`#[tokio::test]`, colocated `#[cfg(test)] mod tests`, descriptive `a_…`/`the_…` snake_case names, `History::in_memory()`, `FakeSecrets`, hand-rolled loopback servers, `Timestamp::from_unix`. No rstest/mockito/serial/proptest.
- Commit style: Conventional Commits with scopes (`feat(core):`, `fix(daemon):`, `feat(ui):`).

---

## Phase A — Core: config + types

### Task 1: `Config::accounts` reader

**Files:**
- Modify: `crates/tidemark-core/src/config.rs` (add reader near `providers()` ~line 460)

**Interfaces:**
- Produces: `pub fn accounts(&self, provider: &str) -> Result<Vec<String>, ConfigError>` — ordered account ids for `[provider.<slug>] accounts`, defaulting to `vec!["default"]` when the key is absent; errors on a present-but-non-array/non-string value.

- [ ] **Step 1: failing tests**

In `config.rs::tests`:

```rust
#[test]
fn a_provider_without_an_accounts_key_has_its_default_account() {
    let config = Config::from_text(r#"providers = ["zai"]"#);
    assert_eq!(config.accounts("zai").unwrap(), vec!["default".to_string()]);
}

#[test]
fn accounts_come_back_in_file_order() {
    let config = Config::from_text("[provider.claude]\naccounts = [\"default\", \"work\"]\n");
    assert_eq!(config.accounts("claude").unwrap(), vec!["default".to_string(), "work".to_string()]);
}

#[test]
fn a_non_array_accounts_key_is_refused() {
    let config = Config::from_text("[provider.zai]\naccounts = \"work\"\n");
    assert!(matches!(config.accounts("zai"), Err(ConfigError::InvalidAccounts { .. })));
}
```

(`Config::from_text` — add a test helper parsing a string into a `Config` over a temp path, mirroring `Config::at`; see existing `scratch_config` pattern in `registry.rs` tests.)

- [ ] **Step 2: run, expect FAIL** — `cargo test -p tidemark-core config::tests -- accounts`
- [ ] **Step 3: implement `accounts` + `ConfigError::InvalidAccounts`**

- [ ] **Step 1: failing tests**

In `config.rs::tests` (using the existing `scratch(name)` temp-file helper + `Config::at`):

```rust
#[test]
fn a_provider_without_an_accounts_key_has_its_default_account() {
    let path = scratch("accounts-absent");
    std::fs::write(&path, "providers = [\"zai\"]\n").expect("seed");
    let config = Config::at(path.clone()).expect("parses");
    assert_eq!(config.accounts("zai").unwrap(), vec!["default".to_string()]);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn accounts_come_back_in_file_order() {
    let path = scratch("accounts-order");
    std::fs::write(&path, "[provider.claude]\naccounts = [\"default\", \"work\"]\n").expect("seed");
    let config = Config::at(path.clone()).expect("parses");
    assert_eq!(config.accounts("claude").unwrap(), vec!["default".to_string(), "work".to_string()]);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_non_array_accounts_key_is_refused() {
    let path = scratch("accounts-invalid");
    std::fs::write(&path, "[provider.zai]\naccounts = \"work\"\n").expect("seed");
    let config = Config::at(path.clone()).expect("parses");
    assert!(matches!(config.accounts("zai"), Err(ConfigError::InvalidAccounts { .. })));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
```

Add `const ACCOUNTS_KEY: &str = "accounts";` and the `InvalidAccounts { path, provider, reason }` variant.

- [ ] **Step 4: run, expect PASS**
- [ ] **Step 5: commit** — `feat(core): read per-provider account lists`

### Task 2: `Config::set_accounts` writer + promote-on-remove

**Files:**
- Modify: `crates/tidemark-core/src/config.rs`

**Interfaces:**
- Produces: `pub fn set_accounts(&mut self, provider: &str, accounts: &[String]) -> Result<(), ConfigError>` — writes the array (staged write via existing `self.write()`); caller guarantees `"default"` is first.

- [ ] **Step 1: failing test** — round-trip: set `["default","work"]`, re-read, assert order; then assert file contains `accounts = ["default", "work"]` under `[provider.zai]`.
- [ ] **Step 2: run FAIL** — `cargo test -p tidemark-core config::tests -- set_accounts`
- [ ] **Step 3: implement** using `set_option`'s table-descend pattern but inserting an `Array` of `Value::from(each)`; call `normalize_providers(None)?` first like the other writers.
- [ ] **Step 4: run PASS**
- [ ] **Step 5: commit** — `feat(core): write per-provider account lists`

### Task 3: `ProviderStatus.account_label` wire field

**Files:**
- Modify: `crates/tidemark-types/src/wire.rs` (struct ~line 573, plus `pending`/builders)

**Interfaces:**
- Produces: `pub account_label: Option<String>` on `ProviderStatus`; defaults to `None` everywhere it is constructed. Serde: field stays absent when `None` (the `a{sv}` dict omits it — verify the existing `a_status_survives_the_bus` still passes and add one asserting the key is absent when `None`).

- [ ] **Step 1: failing tests** — (a) a status built with `account_label = Some("Work")` round-trips the bus; (b) a `None` label encodes to a dict with no `account_label` key.
- [ ] **Step 2: run FAIL** — `cargo test -p tidemark-types wire`
- [ ] **Step 3: add the field** (`Option<String>`, `#[serde(default)]`-compatible with the existing zvariant dict encoding), thread `None` through every constructor (`pending`, engine `Account::new`/`with_client`, service `account()`).
- [ ] **Step 4: run PASS** (types + a workspace build to catch construction sites)
- [ ] **Step 5: commit** — `feat(types): carry an optional account label`

---

## Phase B — Daemon: build & publish accounts

### Task 4: registry builds N accounts; constructors take the id

**Files:**
- Modify: `crates/tidemarkd/src/registry.rs` (`accounts` ~line 377, `account` ~267, `keyed_account` ~649, `hand_written_account` ~667, `claude_account`/`codex_account`/`antigravity_account` ~587-645)
- Modify: `crates/tidemark-core/src/providers/claude.rs`, `codex.rs`, `antigravity/mod.rs`, `keyed/mod.rs` — `Provider::account()` and the `Secrets` lookups take the constructed `AccountId` instead of `AccountId::default()`.

**Interfaces:**
- Consumes: `Config::accounts` (Task 1).
- Produces: `registry::account(provider, account_id, secrets, config)`; `Provider::account()` returns the constructed id; OAuth clients look up `Kind::Token` under their own id.

- [ ] **Step 1: failing tests** — in `registry.rs::tests`: a config with `[provider.zai] accounts=["default","work"]` yields two accounts with ids `default`,`work`; a Claude account built with id `work` reports `account()=="work"`.
- [ ] **Step 2: run FAIL** — `cargo test -p tidemarkd registry`
- [ ] **Step 3: implement** — thread the `AccountId` through every constructor and the `Source`/secrets plumbing; extra OAuth accounts force `Source::OAuth` (skip CLI).
- [ ] **Step 4: run PASS** + `cargo test -p tidemark-core providers`
- [ ] **Step 5: commit** — `feat(daemon): build one account per configured account id`

### Task 5: engine add/remove/promote + `set_account_order`

**Files:**
- Modify: `crates/tidemarkd/src/engine.rs` (`add_provider` ~435, `remove_provider` ~468, `set_order` ~500, `Command` enum ~79)

**Interfaces:**
- Produces: `Engine::add_account(provider, account)`, `remove_provider` promotion (removing `"default"` rewrites the survivor list so its first becomes `"default"`), `Engine::set_account_order(provider, &[String])`; new `Command::AddAccount`/`Command::SetAccountOrder` variants.

- [ ] **Step 1: failing tests** — add_account persists+publishes pending; removing `"default"` promotes `work`; set_account_order rejects a non-permutation.
- [ ] **Step 2: run FAIL** — `cargo test -p tidemarkd engine`
- [ ] **Step 3: implement**, reusing the `config_request`/`Command` + `updates.send(Publication::…)` patterns.
- [ ] **Step 4: run PASS**
- [ ] **Step 5: commit** — `feat(daemon): add, order and promote accounts in the engine`

### Task 6: D-Bus `AddAccount` / `SetAccountOrder` + publish `account_label`

**Files:**
- Modify: `crates/tidemarkd/src/service.rs` (interface ~465; `Published` ordering ~95)

**Interfaces:**
- Produces: methods `add_account(provider, account)`, `set_account_order(provider, accounts)`; `ProviderStatus.account_label` filled for non-default accounts; `Published::reorder` extended so `SetOrder` (provider slugs) keeps groups contiguous (all of a provider's accounts move together).

- [ ] **Step 1: failing tests** — `add_account` validates unknown provider / duplicate id / malformed slug → `InvalidArgs`; label published for extra, absent for default.
- [ ] **Step 2: run FAIL** — `cargo test -p tidemarkd service`
- [ ] **Step 3: implement.**
- [ ] **Step 4: run PASS** + `cargo test -p tidemarkd`
- [ ] **Step 5: commit** — `feat(daemon): expose add-account and account-order over D-Bus`

### Task 7: rename = migrate (secrets re-key + history re-key)

**Files:**
- Modify: `crates/tidemark-core/src/storage/mod.rs` (add `rekey_account(provider, old, new)` — one transaction over `window_state`, `segment`, `point`, `notice`)
- Modify: `crates/tidemarkd/src/service.rs` (`rename_account(provider, old, new)`), `crates/tidemark/src/bus.rs` proxy.

**Interfaces:**
- Produces: `History::rekey_account(&mut self, provider, old, new) -> Result<(), StorageError>` (transactional `UPDATE … SET account=?new WHERE provider=? AND account=?old` on all four tables); daemon `rename_account` copies the secret slot (get old → set new → delete old), re-keys history, rewrites the `accounts` array in place, reloads.

- [ ] **Step 1: failing tests** — history rows move to the new id and the old id has none; a mid-transaction failure leaves the old id intact (force via a bad new id / closed connection).
- [ ] **Step 2: run FAIL** — `cargo test -p tidemark-core storage -- rekey`
- [ ] **Step 3: implement** rekey + the daemon orchestration.
- [ ] **Step 4: run PASS** + `cargo test -p tidemarkd`
- [ ] **Step 5: commit** — `feat(daemon): migrate an account's credentials and history on rename`

---

## Phase C — UI proxy + settings dialog

### Task 8: bus proxy + settings list nested rows with "+" and editable label

**Files:**
- Modify: `crates/tidemark/src/bus.rs` (add `add_account`, `set_account_order`, `rename_account`)
- Modify: `crates/tidemark/src/provider_settings/list.rs` (nested rows, "+" left of pen, editable label), `provider_settings/mod.rs` (grouping in `apply`, add-account dialog, open detail after add).

**Interfaces:**
- Produces: proxy methods; `ConfiguredList` renders provider row then indented account rows; "+" opens a name dialog → `add_account` → pushes the account's detail page (reuse `open_detail`); label edit → `rename_account`.

- [ ] **Step 1:** implement proxy methods (mirror existing signatures).
- [ ] **Step 2:** nested-row rendering + "+" gated on `multi_account_capable(definition)` (Key or OAuth kind).
- [ ] **Step 3:** add-account name dialog → slugify → `add_account` → `open_detail`.
- [ ] **Step 4:** label edit wiring → `rename_account`.
- [ ] **Step 5: verify by driving the real app** (`cargo run -p tidemark`, computer-driven): add a second account to a keyed provider, see it nested, rename it.
- [ ] **Step 6: commit** — `feat(ui): create, nest and rename accounts in provider settings`

---

## Phase D — UI main window: groups, expand, backdrop, reorder

### Task 9: group cards + expand/collapse with "+N" chevron

**Files:**
- Modify: `crates/tidemark/src/window.rs` (`show_all`/`show_one`/`make_card` grouping ~352-457)
- Modify: `crates/tidemark/src/card.rs` (title-row expand button + count; account-label as card title for extra cards)

**Interfaces:**
- Produces: cards grouped by provider; main card carries an expand toggle showing "+N"; extra cards titled by `account_label`/id; in-memory expand state; auto-expand the group an account was just added to.

- [ ] **Step 1:** group statuses by provider in `show_all`/`show_one`.
- [ ] **Step 2:** expand button on main card (flat, `pan-down/up-symbolic`, "+N"), toggling insertion/removal of extra cards after the main card.
- [ ] **Step 3:** verify by driving the window (two accounts → expand shows both full cards, collapse hides them).
- [ ] **Step 4: commit** — `feat(ui): group a provider's accounts behind an expand toggle`

### Task 10: group backdrop in `CardGrid`

**Files:**
- Modify: `crates/tidemark/src/grid.rs` (`size_allocate` ~355, `snapshot`, add `group-bin` CSS)
- Modify: `crates/tidemark/src/style.rs` (backdrop fill, tighter group spacing)

**Interfaces:**
- Produces: the grid draws one darker rounded-rect region behind the cells of an expanded group, before the cards; tracks which slots belong to an expanded group; handles row-wrapping (multi-rect) and leaves drag painting above the backdrop.

- [ ] **Step 1:** pass group membership into the grid (slot → group).
- [ ] **Step 2:** paint backdrop region(s) in `snapshot()` for expanded groups only.
- [ ] **Step 3:** verify visually (backdrop under the group only; collapsed/single providers unchanged).
- [ ] **Step 4: commit** — `feat(ui): draw a shared backdrop under an expanded account group`

### Task 11: group-aware reorder

**Files:**
- Modify: `crates/tidemark/src/window.rs` (`connect_reorder` ~625, `show_order` ~678)
- Modify: `crates/tidemark/src/grid.rs` if the gesture must refuse cross-boundary drags of an extra card.

**Interfaces:**
- Consumes: `set_account_order` proxy (Task 8).
- Produces: reorder handler distinguishes provider reorder (`set_order`) from intra-group account reorder (`set_account_order`); expanded extra cards cannot cross a group boundary; collapsed groups drag as whole groups (existing behaviour).

- [ ] **Step 1:** classify `(from,to)` by the providers at those indices; same provider → `set_account_order`, else → `set_order`.
- [ ] **Step 2:** refuse cross-boundary drags of expanded extra cards.
- [ ] **Step 3:** verify by dragging in the live window (both directions, both kinds).
- [ ] **Step 4: commit** — `feat(ui): reorder providers and accounts within a group`

### Task 12: tray label prefers `account_label`

**Files:**
- Modify: `crates/tidemark/src/tray.rs` (`label` ~113)

- [ ] **Step 1: failing test** — shared provider with a label renders `Name (label)`, without → `Name (account-id)`.
- [ ] **Step 2: run FAIL** — `cargo test -p tidemark tray`
- [ ] **Step 3: implement** (prefer `account_label`, fall back to `account`).
- [ ] **Step 4: run PASS**
- [ ] **Step 5: commit** — `feat(ui): label tray rows with the account label`

---

## Final gate

- [ ] `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
- [ ] `shellcheck scripts/*.sh` unchanged-ok; no shell changes expected.
- [ ] Drive the full flow in the live app: add two accounts to one keyed provider and one OAuth provider, rename one, expand/collapse, reorder both kinds, remove the main (watch promotion), confirm tray labels.

## Execution Handoff

Plan saved to `docs/superpowers/plans/2026-09-01-multi-account-providers.md`. Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks.
2. **Inline Execution** — executing-plans, batched with checkpoints.
