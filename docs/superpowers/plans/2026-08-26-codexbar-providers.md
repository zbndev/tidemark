# CodexBar Provider Wave Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the 19 CodexBar providers that Tidemark's existing mechanisms already cover — 15 browser-cookie providers, 2 CLI-credential-file providers (Gemini, Grok), and 2 API-key providers (Alibaba Coding Plan, StepFun) — on one branch, one PR.

**Architecture:** Every cookie provider is a `keyed::HandSpec` with `CredentialKind::External` that reads its session with the existing `crate::browser` module, exactly the way `cursor.rs` does, minus the Cursor-App source. A new shared helper (`keyed/session.rs`) owns the plumbing all 19 would otherwise copy: the two auth options, the chosen-store lookup, the KeyringLocked state, and candidate inspection. Gemini and Grok read vendor credential files the way `codex.rs` reads `~/.codex/auth.json`. Alibaba and StepFun are ordinary key-authenticated `HandSpec`s.

**Tech Stack:** Rust workspace (edition 2024), reqwest + serde_json in `tidemark-core`, no new dependencies.

## Global Constraints

- **One branch, one PR:** `feat/codexbar-providers` off `main`. Every task commits to it. PR body lists the provider table from this plan.
- **Reference repo:** CodexBar at commit `cf79d13` (2026-08-25). Re-clone when needed:
  `git clone https://github.com/steipete/CodexBar /tmp/codexbar && git -C /tmp/codexbar checkout cf79d13`
  All upstream paths below are relative to `/tmp/codexbar`. Where a task says "port the parse from <file>", that file is the normative source for field names, shapes, and edge cases.
- **Fixtures are recorded bodies only.** Nobody here has accounts on these services. Every response body in tests comes from CodexBar's own recorded payloads (their tests and `Tests/CodexBarTests/Fixtures/Providers/`). This is established precedent: `venice.rs` says "every number in the tests is a body CodexBar recorded". Never invent a JSON field or a number. If a body shape is ambiguous, port the upstream parser's exact `CodingKeys`.
- **Networking:** every request goes through `crate::providers::http` (`http::client()`, `http::check`, `super::validate` in `keyed/mod.rs`) so status mapping, `Retry-After`, and the `Tidemark/<version>` identification travel with the transport. Send `Origin`/`Referer` only where the endpoint demands it (Cursor's Bot endpoint is the precedent). **Never** add a browser `User-Agent`, `Editor-Version`, or similar impersonation headers. A provider that turns out to need browser spoofing fails honestly instead.
- **Slugs are permanent storage keys** (config, Secret Service, history, D-Bus). The 19, fixed:

| slug | title | family |
| --- | --- | --- |
| `abacus` | Abacus | cookie |
| `alibaba` | Alibaba | API key (Coding Plan) |
| `augment` | Augment | cookie |
| `commandcode` | CommandCode | cookie |
| `gemini` | Gemini | CLI file |
| `grok` | Grok | CLI file |
| `longcat` | LongCat | cookie |
| `manus` | Manus | cookie |
| `mimo` | MiMo | cookie |
| `mistral` | Mistral | cookie |
| `notion` | Notion | cookie |
| `ollama` | Ollama | cookie (HTML) |
| `opencode` | OpenCode | cookie |
| `perplexity` | Perplexity | cookie |
| `qoder` | Qoder | cookie |
| `sakana` | Sakana | cookie (HTML) |
| `stepfun` | StepFun | API key |
| `t3chat` | T3 Chat | cookie |
| `zoommate` | ZoomMate | cookie |

- **Registration:** each provider adds `pub mod <slug>;` to `crates/tidemark-core/src/providers/keyed/mod.rs` (alphabetical) and `&<slug>::SPEC,` to `HAND_WRITTEN` in `crates/tidemarkd/src/registry.rs` (alphabetical). The complete final `HAND_WRITTEN` order after all tasks:
  `abacus, aiand, alibaba, augment, codebuff, commandcode, cursor, deepgram, deepinfra, factory, fireworks, gemini, groq, grok, ibmbob, kilo, litellm, llmproxy, longcat, manus, mimo, mistral, nanogpt, notion, ollama, openai_api, opencode, openrouter, perplexity, poe, qoder, sakana, stepfun, sub2api, t3chat, wayfinder, xai, zoommate`
- **Window rules:** keys derive from window *length*, never from the source field name; a pool that has no length (a draining balance) keys on its name (`WindowKey::named("balance")` is the precedent). A recognised but malformed window fails the whole fetch (`ProviderError::malformed`). Percentages already expressed as percent are never multiplied by 100.
- **Per-provider housekeeping** (every provider task ends with these five, then commits):
  1. `provider_label` arm in `crates/tidemark-types/src/snapshot.rs` only where default capitalisation reads badly — needed for: `commandcode`→`CommandCode`, `longcat`→`LongCat`, `mimo`→`MiMo`, `opencode`→`OpenCode`, `stepfun`→`StepFun`, `t3chat`→`T3 Chat`, `zoommate`→`ZoomMate`. The rest read fine.
  2. README provider blurb in the style of the Cursor section (`README.md` around line 103).
  3. `docs/TRADEMARKS.md` row `| `tidemark-<slug>-symbolic.svg` | <owner> |` — verify the owner name; known: Perplexity AI, Inc. / Manus AI / Mistral AI SAS / Notion Labs, Inc. / Augment Code, Inc. / Abacus AI, Inc. / Alibaba Cloud (Alibaba Group) / Meituan (LongCat) / Xiaomi (MiMo) / Ollama, Inc. / OpenCode / Sakana AI / Zoom Video Communications, Inc. (ZoomMate) / Google LLC (Gemini) / xAI Corp. (Grok) / StepFun (阶跃星辰) / Qoder (Alibaba Group). T3 Chat's owner must be looked up at execution time.
  4. Icon `data/icons/hicolor/symbolic/apps/tidemark-<slug>-symbolic.svg` — filled outlines only, no `stroke` attributes (`scripts/check-desktop-integration.sh` rejects them). Packaging needs no edit: the deb/RPM/PKGBUILD asset lines glob `tidemark-*-symbolic.svg`.
  5. `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p tidemark-core` before the commit.
- **Errors:** `ProviderError` variants only, no `anyhow`. Never log a cookie header, a `Credential`, or a JWT. Keyring-locked is a state (`ProviderError::KeyringLocked`), not a crash.
- **Test style:** sentence snake_case starting `a_`/`an_`/`the_`; HTTP tests use the loopback `one_request_server` pattern (`providers/antigravity/direct.rs:538`) with a `base_url` override seam like `Cursor::with_test_base`; browser reads use `crate::browser::tests::TestHome` with a fake `SafeStorage` (see `browser/auth.rs` tests and `cursor.rs:1414`). No new test dependencies.

---

### Task 1: The shared browser-session helper

**Files:**
- Create: `crates/tidemark-core/src/providers/keyed/session.rs`
- Modify: `crates/tidemark-core/src/providers/keyed/mod.rs` (add `pub mod session;` in the alphabetical module list — it lands between `poe` and `sub2api`)
- Modify: `crates/tidemark-core/src/providers/keyed/cursor.rs` (migrate its browser branch onto the helper)
- Test: `crates/tidemark-core/src/providers/keyed/session.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::browser::{self, Query, SafeStorage, Keyring}`, `crate::browser::auth::{self, Selection, CandidateCredential, Validation}`, `super::{OptionSchema, Options, ProviderError}`.
- Produces (every later cookie task uses exactly these):
  - `pub const AUTH_BROWSER: &str = "auth-browser";` / `pub const AUTH_PROFILE: &str = "auth-profile";`
  - `pub static OPTIONS: &[OptionSchema]` — the two options, matching cursor's browser pair.
  - `pub fn selection(options: &Options) -> Option<Selection>`
  - `pub fn store_selection(options: &mut Options, selection: &Selection)`
  - `pub struct Session { header, session_name, session_value }` — `Debug` manual, values redacted.
  - `pub async fn session(home: Option<&Path>, storage: &dyn SafeStorage, selection: &Selection, session_names: &[&str], query: &Query, url: &str) -> Result<Option<Session>, ProviderError>`
  - `pub async fn inspect_sources<F, Fut>(home: Option<&Path>, storage: &dyn SafeStorage, query: &Query, probe_url: &str, validate: F) -> Vec<AuthCandidate>` (thin wrapper over `browser::auth::inspect`; `stores()` from `browser::stores_in`).

- [ ] **Step 1: Create the branch**

```bash
git checkout main && git pull && git checkout -b feat/codexbar-providers
```

- [ ] **Step 2: Write the failing tests**

In `session.rs`, model the tests on `browser/auth.rs` tests and `cursor.rs:1414-1470`: a `TestHome` with gecko profiles carrying named cookies, a `NoKeyring` fake, and asserts that (a) `session` returns the header only from the store the `Selection` names, profile-scoped; (b) a jar with none of `session_names` yields `Ok(None)`; (c) an empty `session_names` slice means "any live cookie on the query domains" (the whole-jar providers — Qoder, LongCat, T3Chat, Sakana depend on this); (d) a locked keyring surfaces `ProviderError::KeyringLocked` only when no other store answered; (e) `selection`/`store_selection` round-trip, and `AUTH_PROFILE` absent means "the store's default profile wins the tie".

```rust
#[test]
fn the_session_comes_from_exactly_the_selected_browser_and_profile() {
    let home = crate::browser::tests::TestHome::new();
    gecko_profile(&home, ".mozilla/firefox/aa", "tok", 0);
    gecko_profile(&home, ".zen/bb", "other", 0);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let session = runtime.block_on(session(
        Some(home.path()), &NoKeyring,
        &Selection { browser: "firefox".into(), profile: Some("aa".into()) },
        &["session"], &Query::new(["example.com"].into_iter(), Vec::<String>::new()),
        "https://example.com/api",
    )).unwrap().unwrap();
    assert_eq!(session.session_name, "session");
}
```

(Write the other four tests in the same shape.)

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p tidemark-core keyed::session`
Expected: compile error — module does not exist yet.

- [ ] **Step 4: Implement the helper**

Port the logic verbatim from `cursor.rs:325-370` (`browser_session_header`) and `cursor.rs:394-430` (`inspect_sources`), generalised:

```rust
//! The plumbing every browser-session provider shares: the two options its chosen
//! source is stored under, the reading of that one store, and the inspection of all
//! of them. Cursor keeps its own copy of the shape because it also has the Cursor App;
//! every provider added after it has a browser only, and reads it through here.

pub struct Session {
    /// The full Cookie header for `url`, the same one a browser would send.
    pub header: String,
    /// Which of `session_names` was found — the gate that made this jar worth reading.
    pub session_name: String,
    /// That cookie's value, for the providers whose API wants it as a bearer.
    pub session_value: String,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("header", &"<redacted>")
            .field("session_name", &self.session_name)
            .field("session_value", &"<redacted>")
            .finish()
    }
}
```

`session()` iterates `browser::stores_in(home)` filtered by `store.browser.slug == selection.browser && selection.profile.is_none_or(|p| p == &store.profile)`, reads `store.cookies(query, storage)`, tracks `KeyringLocked` across stores without failing early, filters `is_live(now)`, gates on `session_names` (empty slice = any live cookie), and builds `header_for(&live, url)`. Returns `Ok(None)` when no store passed the gate, `Err(ProviderError::KeyringLocked)` when the only failures were locked keyrings. `inspect_sources` calls `browser::auth::inspect(stores_in(home), query, probe_url, storage, validate)` with the caller's closure.

- [ ] **Step 5: Migrate cursor's browser branch**

Replace `cursor.rs` `browser_session_header` body with a call to `session::session(...)`, passing cursor's `SESSION_COOKIE_NAMES` and `USAGE_SUMMARY_URL`, and its browser-candidates block with `session::inspect_sources(...)`. The Cursor-App candidate stays hand-written in cursor.rs. No cursor test may change.

- [ ] **Step 6: Run the full core suite**

Run: `cargo test -p tidemark-core`
Expected: PASS including all existing cursor tests.

- [ ] **Step 7: Commit**

```bash
git add crates/tidemark-core/src/providers/keyed/
git commit -m "feat(core): shared browser-session plumbing for cookie providers"
```

---

### Task 2: Perplexity — the template provider

**Files:**
- Create: `crates/tidemark-core/src/providers/keyed/perplexity.rs`
- Create: `crates/tidemark-core/tests/fixtures/perplexity/credits.json`
- Modify: `crates/tidemark-core/src/providers/keyed/mod.rs`, `crates/tidemarkd/src/registry.rs`, `crates/tidemark-types/src/snapshot.rs` (no label arm needed), `README.md`, `docs/TRADEMARKS.md`, `data/icons/hicolor/symbolic/apps/tidemark-perplexity-symbolic.svg`

**Interfaces:**
- Consumes: `super::session::{self, OPTIONS, Session}`, `super::{HandSpec, ProviderError, http}`, `crate::browser::{self, Query}`.
- Produces: `pub const PROVIDER_ID: &str = "perplexity";`, `pub static SPEC: HandSpec`, `pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError>` (pure), `struct Perplexity { ... }` with `#[cfg(test)] base_url`.

- [ ] **Step 1: Record the fixture**

Copy the recorded body from `/tmp/codexbar/Tests/CodexBarTests/PerplexityUsageFetcherTests.swift:16-28` (the `parses full response with recurring and promotional credits` payload), substituting the file's `Self.renewalTs`/`Self.futureTs` constants with their literal values from the top of that test file, into `fixtures/perplexity/credits.json`:

```json
{
  "balance_cents": 7250,
  "renewal_date_ts": 1770000000,
  "current_period_purchased_cents": 0,
  "credit_grants": [
    { "type": "recurring", "amount_cents": 10000, "expires_at_ts": 1771000000 },
    { "type": "promotional", "amount_cents": 20000, "expires_at_ts": 1771000000 }
  ],
  "total_usage_cents": 2750
}
```

(Use the upstream constants' real values; the shape and the non-timestamp numbers above are the recorded ones.)

- [ ] **Step 2: Write the failing tests**

Port the assertions of upstream's four parsing tests (`parses full response...`, `waterfall attribution...`, and the two zero-balance bodies at lines 40-100) into `perplexity.rs::tests`, loading `credits.json` with `include_str!`:

```rust
#[test]
fn a_full_response_draws_the_recurring_window_and_attributes_usage_by_the_waterfall() {
    let at = Timestamp::from_unix(1_768_000_000);
    let snapshot = parse(include_str!("../../../tests/fixtures/perplexity/credits.json"), at).unwrap();
    let recurring = snapshot.windows.iter().find(|w| w.key == WindowKey::named("recurring")).unwrap();
    assert!((recurring.used_percent - 27.5).abs() < 0.01); // 2750 of 10000 cents
    assert_eq!(recurring.resets_at, Some(Timestamp::from_unix(1_770_000_000)));
}
```

Plus transport tests with `one_request_server`: the request carries the `Cookie` header from the chosen store, `Origin: https://www.perplexity.ai`, `Referer: https://www.perplexity.ai/account/usage`; a 401 maps to `ProviderError::Credential`; a body that is not the credits object is `Malformed`. Names in house style, e.g. `the_credits_request_carries_the_chosen_browsers_session_cookie`.

- [ ] **Step 3: Verify they fail**

Run: `cargo test -p tidemark-core perplexity`
Expected: FAIL (module missing).

- [ ] **Step 4: Implement**

Constants and shape:

```rust
pub const PROVIDER_ID: &str = "perplexity";

const CREDITS_URL: &str = "https://www.perplexity.ai/rest/billing/credits?version=2.18&source=default";
const ORIGIN: &str = "https://www.perplexity.ai";
const REFERER: &str = "https://www.perplexity.ai/account/usage";

/// The cookie names Perplexity has carried its session in. Chunked `name.0`/`name.1`
/// variants are not reassembled in this port: they are rare, and a jar that only has
/// chunks reads as Missing rather than as a broken provider.
const SESSION_COOKIE_NAMES: &[&str] = &[
    "__Secure-next-auth.session-token",
    "__Secure-authjs.session-token",
    "authjs.session-token",
    "next-auth.session-token",
];
const COOKIE_DOMAINS: &[&str] = &["perplexity.ai", "www.perplexity.ai"];

fn cookie_query() -> browser::Query {
    browser::Query::new(COOKIE_DOMAINS.iter().copied(), Vec::<String>::new())
}

pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Perplexity",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in perplexity.ai browser session.",
    options: session::OPTIONS,
    build,
};
```

`struct Perplexity { client, home, storage, selection: Option<Selection>, #[cfg(test)] base_url }` mirroring `Cursor` (cursor.rs:215-262): `Perplexity::new(options)` uses `http::client()`, `$HOME`, `Keyring`, `session::selection(options)`. `fetch` = `session::session(...)?.ok_or(ProviderError::NoCredential)?` then one `GET` with `.header(COOKIE, session.header).header(ORIGIN, ...).header(REFERER, ...)`, `http::check`, then `parse`.

`parse` (pure, upstream `PerplexityModels.swift` + `PerplexityUsageSnapshot.swift`): serde struct with `#[serde(rename)]` to `balance_cents`, `renewal_date_ts`, `current_period_purchased_cents`, `credit_grants[].{type, amount_cents, expires_at_ts}`, `total_usage_cents` — all `f64` cents, missing `expires_at_ts` allowed. Windows: one `recurring` window per billing cycle (limit = recurring grant cents, used = waterfall-attributed recurring used, reset = `renewal_date_ts`); a `promotional` window only when a promotional grant exists (limit = its cents, used = attributed promo). Waterfall: `recurring_used = min(total_usage, recurring_total)`, spill into purchased, then promo — port the exact function from `PerplexityUsageSnapshot.swift`. Detail rows: `Balance` (cents→dollars), `Total usage`. A window whose numbers cannot be read fails the fetch (`ProviderError::malformed`), per the workspace contract.

- [ ] **Step 5: Register + housekeeping**

`pub mod perplexity;` in keyed/mod.rs (between `poe`... follow the alphabetical module list); `&perplexity::SPEC,` in `HAND_WRITTEN` between `openrouter` and `poe`. No `provider_label` arm. README blurb, TRADEMARKS row (`Perplexity AI, Inc.`), symbolic icon.

- [ ] **Step 6: Run and commit**

Run: `cargo test -p tidemark-core && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

```bash
git add -A && git commit -m "feat(provider): Perplexity from the browser session"
```

---

### Task 3: Manus

**Files:** Create `keyed/manus.rs`, `tests/fixtures/manus/credits.json`; modify registrations + docs per Global Constraints.

- [ ] **Step 1 — fixture:** from `/tmp/codexbar/Tests/CodexBarTests/ManusProviderTests.swift` (the `GetAvailableCredits` body: `totalCredits`, `periodicCredits`, `refreshCredits`, `maxRefreshCredits`, `proMonthlyCredits`, `nextRefreshTime`).
- [ ] **Step 2 — failing tests:** parse test over the fixture (windows: the credits pool as a balance window, `WindowKey::named("credits")`, used = `totalCredits - remaining` where upstream defines remaining — port from `ManusUsageFetcher.swift`; reset = `nextRefreshTime`); transport test: `POST https://api.manus.im/user.v1.UserService/GetAvailableCredits`, body `{}`, headers `Authorization: Bearer <session_value>` (**note: bearer built from the cookie value — this is why `Session::session_value` exists**), `Origin: https://manus.im`, `Referer: https://manus.im/`, `Connect-Protocol-Version: 1`, `Content-Type: application/json`.
- [ ] **Step 3 — implement:** domains `manus.im`, `www.manus.im`; `SESSION_COOKIE_NAMES = ["session_id"]`. `SPEC`: title "Manus", `CredentialKind::External`, hint "Choose a signed-in manus.im browser session."
- [ ] **Step 4 — register + housekeeping + gate + commit:** `git commit -m "feat(provider): Manus from the browser session"`.

---

### Task 4: Qoder

**Files:** Create `keyed/qoder.rs`, `tests/fixtures/qoder/usage.json`; registrations + docs.

- **Domains (exact match, both stores):** `qoder.com`, `www.qoder.com`, `qoder.com.cn`, `www.qoder.com.cn`. **No name gate** — whole jar (`session_names: &[]`).
- **Fetch:** `GET https://qoder.com/api/v2/me/usages/big_model_credits`; on 401/403 retry `https://qoder.com.cn/api/v2/me/usages/big_model_credits`. Headers: `Accept: application/json, text/plain, */*`, `Origin: <site>`, `Referer: <site>/account/usage`, `X-Requested-With: XMLHttpRequest`, `Bx-V: 2.5.35`.
- **Parse:** `quotaSummary` under `totalQuota`/`sharedQuota` with used/limit/remaining, camel or snake keys — port the exact acceptance from `/tmp/codexbar/Sources/CodexBarCore/Providers/Qoder/QoderUsageFetcher.swift` and the recorded bodies in `Tests/CodexBarTests/QoderUsageFetcherTests.swift`.
- **Tests:** fixture parse (window keys stable, e.g. `WindowKey::named("total")`/`shared` per length rules), site-failover transport test (first server answers 401, second answers the body), cookie-forwarding test. Commit: `feat(provider): Qoder from the browser session`.

---

### Task 5: Abacus

**Files:** Create `keyed/abacus.rs`, `tests/fixtures/abacus/{compute-points.json,billing-info.json}`; registrations + docs.

- **Domains:** `abacus.ai`, `apps.abacus.ai`. **Names:** `sessionid`, `session_id`, `session_token`, `auth_token`, `access_token` (exact-match only; CodexBar's substring heuristic is not ported).
- **Fetch (2, concurrent is fine):** `GET https://apps.abacus.ai/api/_getOrganizationComputePoints` and `POST https://apps.abacus.ai/api/_getBillingInfo` body `{}`, both `Cookie` + `Accept: application/json`.
- **Parse:** `totalComputePoints`/`computePointsLeft` → one balance window (used = total − left); `nextBillingDate`, `currentTier` → detail rows. Bodies from `Tests/CodexBarTests/AbacusProviderTests.swift`; acceptance from `Sources/CodexBarCore/Providers/Abacus/AbacusUsageFetcher.swift` (lines ~28-205).
- **Tests:** parse over both fixtures; transport: two routes on the loopback server, both receiving the cookie. Commit: `feat(provider): Abacus from the browser session`.

---

### Task 6: Augment

**Files:** Create `keyed/augment.rs`, `tests/fixtures/augment/{credits.json,subscription.json}`; registrations + docs.

- **Domains:** `augmentcode.com`, `app.augmentcode.com`. **Names:** `session`, `_session`, `web_rpc_proxy_session`, `__Secure-next-auth.session-token`, `next-auth.session-token`, `__Secure-authjs.session-token`, `authjs.session-token`.
- **Fetch:** `GET https://app.augmentcode.com/api/credits` (required) and `GET https://app.augmentcode.com/api/subscription` (optional — a failure there costs only its detail rows, the Cursor supplementary-request rule).
- **Parse:** credits → one window (`usageUnitsAvailable` limit, `usageUnitsConsumedThisBillingCycle` used, subtitle plan-units); subscription → `billingPeriodEnd` reset when present, `planName`/`email` details. Bodies from `Tests/CodexBarTests/AugmentStatusProbeTests.swift`; acceptance from `Sources/CodexBarCore/Providers/Augment/AugmentStatusProbe.swift:505-577`.
- **Tests:** parse over fixtures incl. a missing-subscription case (window still drawn); transport with the credits route 401 → `Credential`. Commit: `feat(provider): Augment from the browser session`.

---

### Task 7: CommandCode

**Files:** Create `keyed/commandcode.rs`; copy fixtures directly from `/tmp/codexbar/Tests/CodexBarTests/Fixtures/Providers/CommandCode/window-limits-root.json` and `window-limits-nested.json` into `tests/fixtures/commandcode/`; registrations + docs.

- **Domains:** `commandcode.ai`, `www.commandcode.ai`. **Names (better-auth):** `__Secure-commandcode_prod_.session_token`, `commandcode_prod_.session_token`, `__Host-commandcode_prod_.session_token`, `__Host-better-auth.session_token`, `__Secure-better-auth.session_token`, `better-auth.session_token`.
- **Fetch:** `GET https://api.commandcode.ai/internal/billing/credits` and `GET https://api.commandcode.ai/internal/billing/subscriptions`; headers `Origin: https://commandcode.ai`, `Referer: https://commandcode.ai/`, `Accept: application/json`.
- **Parse:** `credits.monthlyCredits`/`purchasedCredits` and `windowLimits.fiveHour`/`weekly` (both fixture spellings — root and nested — must parse: that is what the two upstream fixtures exist for); `subscriptions.data[].planId/status/currentPeriodEnd`. Windows: 5-hour and weekly and monthly, keyed by length (`WindowKey` from seconds 5*3600 / 7*86400 / month), used/limit from each window limit object. Acceptance: `Sources/CodexBarCore/Providers/CommandCode/CommandCodeUsageFetcher.swift:12-157`.
- **Tests:** parse both fixture spellings asserting the same window keys; transport 401 → `Credential`. Commit: `feat(provider): CommandCode from the browser session`.

---

### Task 8: Notion

**Files:** Create `keyed/notion.rs`; copy `/tmp/codexbar/Tests/CodexBarTests/Fixtures/Providers/Notion/get-spaces.json` and `get-credit-rate-limit-status.json` into `tests/fixtures/notion/`; registrations + docs.

- **Domains:** `notion.com`, `www.notion.com`, `notion.so`, `www.notion.so`, `app.notion.com`. **Name:** `token_v2` (required — without it the API 401s).
- **Fetch:** `POST https://app.notion.com/api/v3/getSpaces` body `{}` → pick the configured-or-first `spaceId`; then `POST https://app.notion.com/api/v3/getCreditRateLimitStatus` body `{"spaceId":"..."}`. Both `Cookie` + `Content-Type: application/json`.
- **Parse:** the rolling 6-hour window and the monthly billing window from the rate-limit status — port field names and the 6h/month key derivation from `Sources/CodexBarCore/Providers/Notion/NotionUsageFetcher.swift:396-428` and its parser; workspace name → detail row.
- **Tests:** parse both fixtures (spaces then status, chained through the spaceId); transport: second request receives the spaceId from the first response. Commit: `feat(provider): Notion AI from the browser session`.

---

### Task 9: MiMo

**Files:** Create `keyed/mimo.rs`, `tests/fixtures/mimo/{balance.json,plan-detail.json,plan-usage.json}`; registrations + docs.

- **Domains:** `platform.xiaomimimo.com`, `www.platform.xiaomimimo.com`. **Names (both required):** `api-platform_serviceToken`, `userId` — the session gate is "jar contains both".
- **Fetch (3 GET, concurrent):** `https://platform.xiaomimimo.com/api/v1/balance` (required), `/api/v1/tokenPlan/detail`, `/api/v1/tokenPlan/usage`; headers `Cookie`, `x-timeZone: UTC`, `Accept: application/json`.
- **Parse:** balance + plan detail/usage → windows per plan pool; port from `Sources/CodexBarCore/Providers/MiMo/MiMoUsageFetcher.swift:88-181` and `Tests/CodexBarTests/MiMoProviderTests.swift` bodies.
- **Not ported (say so in the module doc):** CodexBar's Firefox session-restore LZ4 decoder and the `~/.claude-envs` local-usage fallback — v1 reads live cookie databases only.
- **Tests:** parse over the three fixtures; transport: all three routes get the cookie; a jar missing `userId` is `Ok(None)` → Missing. Commit: `feat(provider): MiMo from the browser session`.

---

### Task 10: LongCat

**Files:** Create `keyed/longcat.rs`, `tests/fixtures/longcat/{user-current.json,token-packs.json,token-usage.json}`; registrations + docs.

- **Domains:** `longcat.chat`, `www.longcat.chat`. **No name gate** (whole jar).
- **Fetch:** `GET https://longcat.chat/api/v1/user-current` (required); `POST https://longcat.chat/api/pay/quota/metering/token-packs/summary` body `{}`; `GET https://longcat.chat/api/lc-platform/v1/tokenUsage` (only when no ACTIVE `currentLot`); `GET https://longcat.chat/api/lc-platform/v1/pending-fuel-packages` (optional). Headers: `Origin: https://longcat.chat`, `Referer: https://longcat.chat/platform/usage`.
- **Parse — the Meituan envelope:** HTTP 200 with an embedded `code: 401|403` means the session was rejected: map to `ProviderError::Credential`. Quota windows from the packs/usage bodies; port from `Sources/CodexBarCore/Providers/LongCat/LongCatUsageFetcher.swift` (bodies in `Tests/CodexBarTests/LongCatProviderTests.swift`).
- **Tests:** envelope-rejection test (200-with-code-401 body → `Credential`), parse windows, transport chain. Commit: `feat(provider): LongCat from the browser session`.

---

### Task 11: Mistral

**Files:** Create `keyed/mistral.rs`, `tests/fixtures/mistral/{usage.json,credits.json,vibe.json}`; registrations + docs.

- **Domains (exact):** `mistral.ai`, `admin.mistral.ai`, `auth.mistral.ai`, `console.mistral.ai`. **Session gate:** any cookie whose name *starts with* `ory_session_` (the one prefix rule this wave needs — add a `starts_with` match alongside exact names in `session::session`, or pre-filter in the provider via the query; choose the former, one test covers it). **CSRF:** the `csrftoken` cookie's value goes out as `X-CSRFTOKEN` / `X-CSRFToken`.
- **Fetch:** `GET https://admin.mistral.ai/api/billing/v2/usage?month=<m>&year=<y>` (required, `Cookie` + `X-CSRFTOKEN`); `GET https://admin.mistral.ai/api/billing/credits` (optional); `GET https://console.mistral.ai/api-ui/trpc/billing.vibeUsage?batch=1&input=<encoded>` (optional; the cookie header is rebuilt as `csrftoken=<v>; ory_session_*=...` for the cross-subdomain hop — `console` cookies forwarded to `admin` and back, port the rebuild from `Sources/CodexBarCore/Providers/Mistral/MistralUsageFetcher.swift:67-185`).
- **Parse:** the big per-model/day usage payload aggregates to spend windows; credits → wallet details; vibeUsage → the monthly plan percent. Bodies from `Tests/CodexBarTests/MistralUsageParserTests.swift` and `MistralVibeUsageTests.swift`.
- **Tests:** ory-prefix gate, CSRF header presence, cross-subdomain rebuilt header, parse of the three fixtures. Commit: `feat(provider): Mistral from the browser session`.

---

### Task 12: Ollama

**Files:** Create `keyed/ollama.rs`, `tests/fixtures/ollama/settings.html` (recorded HTML from `Tests/CodexBarTests/OllamaUsageParserTests.swift`); registrations + docs.

- **Domains:** `ollama.com`, `www.ollama.com`. **Names:** `__Secure-session`, `session`, `ollama_session`, `__Host-ollama_session`, `wos-session`, `__Secure-next-auth.session-token`, `next-auth.session-token`.
- **Fetch:** `GET https://ollama.com/settings` with `Cookie` + `Accept: text/html,...`. A redirect that lands on `/signin` means the session was rejected: detect via the final response URL and map to `ProviderError::Credential`.
- **Parse (HTML):** port `Sources/CodexBarCore/Providers/Ollama/OllamaUsageParser.swift` — the server-rendered usage blocks → windows; a page that renders no numbers is `Malformed`, not an empty card. The module doc must say the page is HTML and can move.
- **Tests:** parse the recorded HTML asserting window values; transport: a 302 to `/signin` → `Credential`; a numbers-less page → `Malformed`. Commit: `feat(provider): Ollama from the browser session`.

---

### Task 13: Sakana

**Files:** Create `keyed/sakana.rs`, `tests/fixtures/sakana/billing.html` (recorded HTML from `Tests/CodexBarTests/SakanaUsageFetcherTests.swift`); registrations + docs.

- **Domains:** `console.sakana.ai`. **No name gate** (whole jar).
- **Fetch:** `GET https://console.sakana.ai/billing` (required) and `GET https://console.sakana.ai/billing?tab=payAsYouGo` (best-effort); `Cookie` + `Accept: text/html,...`.
- **Parse (HTML):** regexes for `X% used`, `Resets on <date>` (UTC), plan name, `Credit balance`, `Usage Total` — port `Sources/CodexBarCore/Providers/Sakana/SakanaUsageFetcher.swift::parseBillingHTML/parsePayAsYouGoHTML` including the React-hydration-comment handling. Window: the plan percent with reset; PAYG balance → detail row.
- **Tests:** parse the recorded HTML; hydration-comment body still parses; a page without the markers → `Malformed`. Commit: `feat(provider): Sakana from the browser session`.

---

### Task 14: OpenCode

**Files:** Create `keyed/opencode.rs`, `tests/fixtures/opencode/{workspaces.txt,subscription.txt}` (recorded server-fn bodies from `Tests/CodexBarTests/OpenCodeUsageParserTests.swift`; copy `Fixtures/Providers/OpenCode/billing-pay-as-you-go.txt` too); registrations + docs.

- **Domains:** `opencode.ai`, `app.opencode.ai`. **Names:** `auth`, `__Host-auth`.
- **Fetch (server functions):** `GET https://opencode.ai/_server?id=<id>&args=<json>` (POST fallback with JSON body and `X-Server-Id`/`X-Server-Instance: server-fn:<uuid>` headers). Three function ids — **copy the exact hash strings from `/tmp/codexbar/Sources/CodexBarCore/Providers/OpenCode/OpenCodeUsageFetcher.swift:29-34`, never from this plan**: workspaces → `wrk_…` ids, subscription → 5h + weekly percent/`resetInSec`, billing → PAYG monthly (same parser our `opencodego.rs` already has for the Zen balance — reuse it if the shapes match, else port `OpenCodeZenBillingParser`). A 401/403 or a null/500 subscription falls through to billing.
- **Module doc must say:** the ids are build hashes that rotate; a wrong id today is `Unreachable`-shaped breakage we accept.
- **Tests:** parse workspace→subscription chain from fixtures; fallthrough to billing on 500; transport with both GET and POST spellings. Commit: `feat(provider): OpenCode from the browser session`.

---

### Task 15: T3 Chat

**Files:** Create `keyed/t3chat.rs`, `tests/fixtures/t3chat/customer-data.jsonl`; registrations + docs. Label arm `t3chat` → `T3 Chat`.

- **Domains:** `t3.chat`, `www.t3.chat`. **No name gate.**
- **Fetch:** `GET https://t3.chat/api/trpc/getCustomerData?batch=1&input=<urlencoded {"0":{"json":{"sessionId":null},"meta":{"values":{"sessionId":["undefined"]}}}}>`; headers `trpc-accept: application/jsonl`, `x-trpc-source: web-client`, `x-trpc-batch: true`, `Referer: https://t3.chat/settings/customization`, `Origin: https://t3.chat`.
- **Parse:** JSONL lines → usage percentages. **Challenge:** a 429 carrying `x-vercel-mitigated: challenge` maps to `ProviderError::Local("T3 Chat asked for a browser check")` — surfaced, not retried, not spoofed around.
- **Tests:** parse the recorded JSONL; transport: 429-with-mitigated → the Local sentence; 200 chain → windows. Commit: `feat(provider): T3 Chat from the browser session`.

---

### Task 16: ZoomMate

**Files:** Create `keyed/zoommate.rs`, `tests/fixtures/zoommate/{login.json,credits-status.json}` (recorded from `Tests/CodexBarTests/ZoomMateUsageFetcherTests.swift`; copy `Fixtures/ZoomMate/issue-2507-cookie-scope.json` for the RFC 6265 narrowing test); registrations + docs.

- **Domains:** `zoom.us`, `ai.zoom.us`, `zoommate.zoom.us`. **Names:** the `_zm_*` SSO cookies — gate on "jar contains a cookie whose name starts with `_zm_`" (the Task 11 prefix rule).
- **Fetch (mint then read):** `GET https://ai.zoom.us/ai-computer/api/v1/login/?continue=https://zoommate.zoom.us/` with the cookie header → `data.nak` (a bearer JWT; **never logged**) and `data.user_profile.email`; then `GET https://ai.zoom.us/ai-computer/api/v1/credits/status` with `Authorization: Bearer <nak>`, a host-scoped `Cookie`, `Origin/Referer: https://zoommate.zoom.us`. Host failover to `zoommate.zoom.us` on connection failure. Mint fresh every poll in v1 (no token cache — say so in the doc).
- **Parse:** credits status → windows; email → detail row. Port from `Sources/CodexBarCore/Providers/ZoomMate/ZoomMateUsageFetcher.swift:14-207`.
- **Tests:** mint-then-read chain on the loopback server; the cookie-scope fixture asserts the header sent to `ai.zoom.us` carries only RFC 6265-matching cookies (this is what `browser::header_for` already does — the test pins it). Commit: `feat(provider): ZoomMate from the browser session`.

---

### Task 17: Gemini

**Files:**
- Create: `crates/tidemark-core/src/providers/keyed/gemini.rs`, `tests/fixtures/gemini/{quota.json,load.json}`
- Modify: registrations; `crates/tidemarkd/src/registry.rs` — an `external_present` arm (the `antigravity::agy::is_available` pattern at registry.rs:458) reporting `~/.gemini/oauth_creds.json` presence; README; TRADEMARKS (`Google LLC`); icon.

**Interfaces:**
- Consumes: `super::{HandSpec, ProviderError, http}`, `crate::oauth_file` (ADR-0001 field-merge write-back), `crate::providers::oauth` refresh types.
- Produces: `pub const PROVIDER_ID: &str = "gemini";`, `pub fn cli_credentials_path() -> Option<PathBuf>` (`$HOME/.gemini/oauth_creds.json`), `pub static SPEC: HandSpec` (`CredentialKind::External`, hint "Read the Gemini CLI's own login (`gemini` → sign in)."), `pub fn parse_quota(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError>`.

- [ ] **Step 1 — failing tests:** (a) `cli_credentials_path` under a `TestHome`; (b) `parse_quota` over the recorded `retrieveUserQuota` body from `Tests/CodexBarTests/GeminiStatusProbeAPITests.swift` — `buckets[].{modelId, remainingFraction, resetTime}` grouped into Pro/Flash/Flash-Lite windows (one window per tier, `used_percent = (1 - remainingFraction) * 100`, reset from `resetTime`); (c) refresh: a loopback token server answering `oauth2.googleapis.com`-shaped form data returns a new access token and the file is field-merged (unknown JSON keys preserved — the `oauth_file.rs` tests already prove the merge; this test proves Gemini calls it); (d) an `oauth_creds.json` with no refresh token and an expired access token → `ProviderError::NoCredential` with the settings hint pointing at `gemini`.
- [ ] **Step 2 — implement:** read `~/.gemini/oauth_creds.json` (`access_token`, `refresh_token`, `expiry_date` ms); refresh via `POST https://oauth2.googleapis.com/token` (`grant_type=refresh_token`, client id/secret from `GEMINI_OAUTH_CLIENT_ID`/`GEMINI_OAUTH_CLIENT_SECRET` env, else the Gemini CLI's public constants — **copy the exact values from `/tmp/codexbar/Sources/CodexBarCore/Providers/Gemini/GeminiOAuthConfig.swift` or the CLI's `oauth2.js` regexes at cf79d13**), write back merged. Fetch: `POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` (body `{"metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}}`, Bearer) → tier/project; `GET https://cloudresourcemanager.googleapis.com/v1/projects` only when load gave no project (pick `gen-lang-client*`); `POST .../v1internal:retrieveUserQuota` `{"project":"<id>"}`. A `settings.json` whose `security.auth.selectedType` is not `oauth-personal` → `ProviderError::Local` naming the CLI sign-in.
- [ ] **Step 3 — register + housekeeping + gate + commit:** `git commit -m "feat(provider): Gemini from the Gemini CLI login"`.

---

### Task 18: Grok

**Files:** Create `keyed/grok.rs`, `tests/fixtures/grok/{billing.json,settings.json}`; registrations; `external_present` arm for `~/.grok/auth.json` ( honour `$GROK_HOME`); docs.

- **Credential file:** `~/.grok/auth.json` — a JSON map keyed by scope URL; prefer the entry whose scope starts `https://auth.x.ai::`, fall back to `https://accounts.x.ai/sign-in`; fields `key` (access token), `refresh_token`, `expires_at`, `email`, `team_id`. **No refresh in v1** (upstream does not refresh either — module doc says so): an expired token is `ProviderError::NoCredential` pointing at `grok login`.
- **Fetch (2):** `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` and `GET https://cli-chat-proxy.grok.com/v1/settings`, both `Authorization: Bearer <key>` + `x-xai-token-auth: xai-grok-cli`. A pasted key starting `xai-` is refused at build (`ProviderError::Local` naming the account type) — that is the API key, a different provider (`xai`, already shipped).
- **Parse:** billing `format=credits` body → windows; settings `subscription_tier_display` → subtitle/detail. Bodies from `Tests/CodexBarTests/GrokCreditsProxyFetcherTests.swift` and `GrokCLISettingsFetcherTests.swift`.
- **Tests:** scope-selection test (both scopes present → auth.x.ai wins), expired-token test, parse tests, transport tests. Commit: `feat(provider): Grok from the grok CLI login`.

---

### Task 19: Alibaba Coding Plan

**Files:** Create `keyed/alibaba.rs`, `tests/fixtures/alibaba/{intl.json,cn.json}`; registrations; docs. Label arm `alibaba` → `Alibaba` (fine by default — skip the arm).

- **Credential:** `CredentialKind::Key`, hint "A DashScope API key (Model Studio console)." Sent as `Authorization: Bearer` **and** `X-DashScope-API-Key`.
- **Fetch (region failover):** `POST https://modelstudio.console.alibabacloud.com/data/api.json?action=zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2&product=broadscope-bailian&api=queryCodingPlanInstanceInfoV2&currentRegionId=intl` — on transport failure or a region-shaped error, retry the whole POST against `https://bailian.console.aliyun.com/...currentRegionId=cn`. `Content-Type: application/json`, body `{}`.
- **Parse:** `per5Hour*/perWeek*/perBillMonth*` quota keys → three windows (keys by length: 5h, 7d, month); port the exact key acceptance from `Sources/CodexBarCore/Providers/Alibaba/AlibabaCodingPlanUsageFetcher.swift` and bodies from `Tests/CodexBarTests/AlibabaCodingPlanProviderTests.swift`.
- **Not ported (module doc):** the cookie/OneConsole mode and its SEC_TOKEN bootstrap — key mode only in this wave.
- **Tests:** parse both regional fixtures; failover transport test (first host unreachable — point the base-url seam at two loopback servers); key-refusal of empty key at build. Commit: `feat(provider): Alibaba Coding Plan by API key`.

---

### Task 20: StepFun

**Files:** Create `keyed/stepfun.rs`, `tests/fixtures/stepfun/{rate-limit.json,plan-status.json}`; registrations; docs. Label arm `stepfun` → `StepFun`.

- **Credential:** `CredentialKind::Key`; the pasted value is the Oasis-Token (strip an `Oasis-Token=` prefix if pasted with it). At build, decode the JWT payload (second base64url segment — no signature check, we are the reader) and take `device_id`; a token without the claim → `ProviderError::Local` naming it.
- **Fetch (2 POST):** `POST https://platform.stepfun.com/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit` body `{}` and `.../GetStepPlanStatus`; cookies `Oasis-Token=<token>; Oasis-Webid=<device_id>`; headers `oasis-appid: 10300`, `oasis-platform: web`, `Content-Type: application/json`.
- **Parse:** `five_hour_usage_left_rate`, `weekly_usage_left_rate`, their reset timestamps, `plan_credit_rate_limit` buckets → 5h and weekly windows (these are *remaining* rates — `used_percent = (1 - rate) * 100`); `GetStepPlanStatus` → plan-name detail. Bodies from `Tests/CodexBarTests/StepFunUsageFetcherTests.swift`.
- **Not ported (module doc):** the username/password login and token-refresh flows — pasted token only, as decided.
- **Tests:** JWT decode test (fixed token fixture with a known `device_id`), parse tests, transport tests asserting both cookies and headers. Commit: `feat(provider): StepFun by Oasis token`.

---

### Task 21: Final gate and PR

- [ ] **Step 1 — the full local gate:**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo test --workspace && ./scripts/check-layering.sh && ./scripts/check-desktop-integration.sh
```

Expected: all green. `check-desktop-integration.sh` also validates the 19 new SVGs (no `stroke`).

- [ ] **Step 2 — consistency sweep:** `HAND_WRITTEN` order matches the Global Constraints list exactly; `pub mod` list alphabetical; every provider has README blurb + TRADEMARKS row + icon; `registry.rs` has an `external_present` arm only for gemini/grok; no `provider_label` arms beyond the seven listed.

- [ ] **Step 3 — push and open the PR:**

```bash
git push -u origin feat/codexbar-providers
```

PR title: `feat: 19 CodexBar providers on existing mechanisms`. PR body: the Global Constraints provider table, one line per provider naming its credential source, and the known-fragility notes (Ollama/Sakana HTML, OpenCode server-fn ids, T3 Chat challenge, ZoomMate Cloudflare, chunked Perplexity cookies not reassembled).

---

## Self-review notes

- Coverage: 19 providers = 15 cookie (Tasks 2–16) + 2 CLI-file (17–18) + 2 key (19–20); Zed, QwenCloud, Alibaba Token Plan excluded per decision; manual cookie paste out of scope; no vendor-CLI subprocess appears in any task (none of the 19 needs one — the CLI executions in CodexBar were List-2 territory).
- The template (Task 2) carries the full code shape; Tasks 3–16 repeat their own constants, endpoints, cookie names, fixture paths, and parse sources rather than saying "similar to Task 2".
- Upstream file paths were verified against the clone at `cf79d13`; where an upstream file name could not be cited with certainty (MiMo fixture names, Grok settings tests), the task names the provider directory and the normative parser file, which is stable.
