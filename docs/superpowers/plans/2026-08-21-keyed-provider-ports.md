# Keyed Provider Ports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port twenty-six API-key providers from CodexBar into `providers::keyed`, each as one file holding a `SPEC`, a pure `parse`, and tests built from response bodies CodexBar recorded.

**Architecture:** Every provider is a file `crates/tidemark-core/src/providers/keyed/<slug>.rs` containing `pub static SPEC: Spec` and `pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError>`, plus a line in `keyed::CATALOG`. Nothing else in the workspace names a provider. Providers that need more than one request keep their own `impl Provider` and reuse `keyed`'s transport.

**Tech Stack:** Rust 2024, `serde`/`serde_json`, `reqwest`. Source material is Swift and JavaScript in `~/repos/CodexBar`.

**Spec:** `docs/superpowers/specs/2026-08-21-keyed-provider-port-design.md`

**Depends on:** `docs/superpowers/plans/2026-08-21-keyed-provider-mechanism.md` must be complete. Every task here consumes `keyed::{Auth, Method, OptionSchema, Options, Spec, CATALOG}` and `Window.subtitle` from it.

## Global Constraints

- All documentation, source code, code comments, tests, logs, and interface copy are written in English.
- Crate layering is enforced: `tidemark-core` never reaches GTK/GDK/libadwaita. `./scripts/check-layering.sh` asserts it.
- The workspace builds clean at `-D warnings`, and `cargo fmt` is clean.
- A provider must never silently drop a window. An entry of a recognised kind that cannot be parsed is a `ProviderError::Malformed` for the whole fetch; only an entry of an unrecognised kind is skipped.
- Provider slugs are storage keys and never change once shipped. The slug is CodexBar's own id, lowercased, so that a user's history could be matched up later.
- A `Credential` never reaches a log.
- **No provider here has ever been seen answering.** Every number in every test comes from a body CodexBar recorded, never from one invented to make a parser look right. If no recorded body exists for a case, the case is not tested and the task says so in its commit message.

**Verification commands** (used by every task's final step):

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
```

---

## The Porting Procedure

Every task from 2 onward runs this procedure. It is written out once here; each task
supplies the facts it needs and does not repeat the method.

**Step A — Read the contract.** Open the source files the task names, in the order it
names them. Where a JS plugin exists it is the whole contract in one file: `endpoints`,
`auth`, the request, and every validation. Where there is only Swift, the fetcher gives the
request and the usage-stats type gives the meaning. Write down, before writing any Rust:
the URL, the method, where the key goes, which fields carry consumption, which carry a
limit, which carry a reset time, and what units each is in.

**Step B — Harvest the fixtures.** Open the CodexBar test file the task names and copy the
response bodies out of it verbatim into the Rust test module as `const` string literals,
each named for the state it captures. These are the only bodies that exist; do not
normalise, prettify, or trim them.

**Step C — Write the failing tests.** Three at minimum, always:

1. *Fixture agreement.* `parse` of the recorded body produces the windows and details that
   CodexBar's own test asserts for that body — same percentages, same reset instants, same
   window lengths, same counts.
2. *Malformed body.* `parse("{\"partial\":", now)` and `parse` of a body whose consumption
   field is a string where a number belongs are both `ProviderError::Malformed`.
3. *The unknown-kind rule.* A body carrying one recognised entry and one entry of an
   invented kind yields exactly one window; a body whose recognised entry has an
   unreadable shape yields `Err(Malformed)` rather than a snapshot with a window missing.

Add a fourth when the provider has an option: the endpoint resolves the option, and an
unrecognised value falls back to the default rather than refusing to poll.

**Step D — Run them and watch them fail.** `cargo test -p tidemark-core <slug>`. The
expected failure is a compile error naming the missing `SPEC` or `parse`.

**Step E — Write the file.** `crates/tidemark-core/src/providers/keyed/<slug>.rs`:
a module doc comment naming the source it was ported from and every trap found in Step A,
serde structures for the response, `parse`, and `pub static SPEC`. Register it with a
`mod <slug>;` line and an entry in `keyed::CATALOG`, both kept alphabetical.

Window construction follows the shape the provider reports:

- *A quota window* — a percentage with a period: `WindowKey::for_length(length)`, or
  `WindowKey::for_pool(pool, length)` where one provider reports two windows of the same
  length against different quotas. Set `length` and `resets_at` when the provider gives
  them; a missing reset is not an error, it is a window without a pace mark.
- *A fixed balance* — a quantity against a stated limit: one window keyed
  `WindowKey::named("balance")` with a comment saying that a balance has no length to key
  on, `used_percent` from used over limit, `length: None`, `resets_at: None`, and
  `subtitle: Some(...)` carrying both absolutes in the provider's own unit and rounding.
- *A balance with no limit* — no window at all, only a `DetailSection`. The card renders
  empty; that is accepted and recorded in the spec.

**Step E2 — A rejected key that arrives as a 200.** Where the source shows the provider
reporting an invalid or expired key in the body rather than in the status — CodexBar's
plugins raise `authenticationExpired` from inside their parsing for several of these —
`parse` returns `ProviderError::Credential { status: 401 }`, so the interface asks for a
new key instead of reporting an unreadable response. That case gets its own test, using the
body CodexBar's own test uses. Where the source shows no such case, skip this step rather
than inventing one.

**Step F — Run them and watch them pass.** Then the full verification command, then commit
with `git commit -m "Add the <Name> provider"`.

**When the provider does not fit.** If Step A shows the provider needs more than one
request, or a credential that is not a pasted key, stop and say so rather than bending
`Keyed`. Multi-request providers move to Task 25's group. A provider that turns out to
need a cookie, a browser, or another CLI's configuration file is out of this plan's scope
entirely: report it, leave it unported, and do not invent a key-based path for it.

---

### Task 1: Make room for the ports

**Files:**
- Modify: `crates/tidemark-core/src/providers/keyed.rs` (become a directory module)
- Create: `crates/tidemark-core/src/providers/keyed/mod.rs` (the mechanism, moved)
- Modify: `crates/tidemark-core/src/providers/zai.rs` → move to `keyed/zai.rs`
- Modify: `crates/tidemark-core/src/providers/kimi.rs` → move to `keyed/kimi.rs`

**Interfaces:**
- Produces: `providers::keyed::{kimi, zai}` re-exported so that `tidemarkd::registry`,
  `crates/tidemark-core/tests/provider_to_history.rs` and `examples/probe.rs` keep
  compiling. Add to `keyed/mod.rs`:
  `pub mod kimi; pub mod zai;` and in `providers/mod.rs` keep
  `pub use keyed::{kimi, zai};` so existing paths such as `providers::zai::parse` still
  resolve.

- [ ] **Step 1: Move the files**

```bash
mkdir crates/tidemark-core/src/providers/keyed
git mv crates/tidemark-core/src/providers/keyed.rs crates/tidemark-core/src/providers/keyed/mod.rs
git mv crates/tidemark-core/src/providers/zai.rs crates/tidemark-core/src/providers/keyed/zai.rs
git mv crates/tidemark-core/src/providers/kimi.rs crates/tidemark-core/src/providers/keyed/kimi.rs
```

- [ ] **Step 2: Fix the module tree**

In `keyed/mod.rs` add `pub mod kimi;` and `pub mod zai;` above the mechanism, and change
`CATALOG` to `&[&kimi::SPEC, &zai::SPEC]` now that the modules are siblings rather than
reached through `super`. In `providers/mod.rs` remove `pub mod kimi;` and `pub mod zai;`
and add `pub use keyed::{kimi, zai};` beneath `pub mod keyed;`.

- [ ] **Step 3: Run the whole suite**

Run: `cargo test --workspace`
Expected: PASS with no test changed. This task moves files and nothing else; a failing
assertion means a path was rewritten wrongly.

- [ ] **Step 4: Full verification and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add -A crates/tidemark-core/src/providers
git commit -m "Give the keyed providers a directory"
```

---

### Task 2: ClinePass — the worked example

This task is written out in full. Tasks 3 onward supply their facts and run the porting
procedure; read this one first to see what its output looks like.

**Files:**
- Create: `crates/tidemark-core/src/providers/keyed/clinepass.rs`
- Modify: `crates/tidemark-core/src/providers/keyed/mod.rs`

**Source of truth:**
- `~/repos/CodexBar/Sources/CodexBarCore/Resources/Plugins/clinepass.js`
- Fixtures: `~/repos/CodexBar/Tests/CodexBarTests/ClinePassPluginTests.swift`, `ClinePassProviderTests.swift`

**Contract, from the plugin:**
- `GET https://api.cline.bot/api/v1/users/me/plan/usage-limits`, `Authorization: Bearer <key>`
- Envelope `{success: bool, data: {limits: [...]}}`. `success` absent or not a boolean is malformed; `success: false` is malformed.
- Each limit is `{type, percentUsed, resetsAt?}`. `type` maps to a length: `five_hour` → 18000s, `weekly` → 604800s, `monthly` → 2592000s. **An unrecognised `type` is skipped** — this is exactly the rule `providers::mod` states.
- `percentUsed` is a number and is clamped to 0..=100. A non-number for a recognised type is malformed.
- `resetsAt` is an ISO-8601 string when present; unparseable is malformed, absent is a window without a pace mark.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar. Three windows, the five-hour one part-spent.
    const THREE_WINDOWS: &str = r#"{"success":true,"data":{"limits":[
        {"type":"five_hour","percentUsed":42.5,"resetsAt":"2026-08-21T18:00:00Z"},
        {"type":"weekly","percentUsed":13.0,"resetsAt":"2026-08-25T00:00:00Z"},
        {"type":"monthly","percentUsed":4.25,"resetsAt":"2026-09-01T00:00:00Z"}]}}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn every_reported_window_is_drawn_with_its_length_and_reset() {
        let snapshot = parse(THREE_WINDOWS, at(1_787_000_000)).expect("parses");
        let lengths: Vec<u64> = snapshot
            .windows
            .iter()
            .map(|w| w.length.expect("clinepass states every length").as_secs())
            .collect();
        assert_eq!(lengths, [18_000, 604_800, 2_592_000]);
        assert_eq!(snapshot.windows[0].used_percent, 42.5);
        assert!(snapshot.windows[0].resets_at.is_some());
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn a_quota_kind_invented_after_this_was_written_is_skipped_not_refused() {
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"five_hour","percentUsed":10.0},
            {"type":"lunar_cycle","percentUsed":99.0}]}}"#;
        let snapshot = parse(body, at(1_787_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1, "the unknown kind is skipped");
        assert_eq!(snapshot.windows[0].used_percent, 10.0);
    }

    #[test]
    fn a_known_kind_we_cannot_read_fails_the_whole_fetch() {
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"five_hour","percentUsed":"lots"}]}}"#;
        assert!(
            matches!(parse(body, at(1_787_000_000)), Err(ProviderError::Malformed(_))),
            "a dropped window reads as 'you have no such limit'"
        );
    }

    #[test]
    fn a_window_with_no_reset_is_still_drawn() {
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"weekly","percentUsed":0.0}]}}"#;
        let snapshot = parse(body, at(1_787_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert!(snapshot.windows[0].resets_at.is_none());
        assert_eq!(snapshot.windows[0].length.expect("derived").as_secs(), 604_800);
    }

    #[test]
    fn a_reported_failure_is_not_an_empty_snapshot() {
        assert!(matches!(
            parse(r#"{"success":false,"data":{"limits":[]}}"#, at(1_787_000_000)),
            Err(ProviderError::Malformed(_))
        ));
        assert!(matches!(
            parse(r#"{"partial":"#, at(1_787_000_000)),
            Err(ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn consumption_is_clamped_to_the_bar_it_is_drawn_on() {
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"five_hour","percentUsed":140.0}]}}"#;
        let snapshot = parse(body, at(1_787_000_000)).expect("parses");
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
    }

    #[test]
    fn the_spec_polls_the_documented_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.cline.bot/api/v1/users/me/plan/usage-limits"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.id, PROVIDER_ID);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark-core clinepass`
Expected: FAIL to compile — the module does not exist.

- [ ] **Step 3: Write the provider**

```rust
//! ClinePass.
//!
//! Ported from CodexBar's `clinepass.js` plugin. Never seen answering: every number in the
//! tests is a body CodexBar recorded.
//!
//! # What the payload does not tell you
//!
//! `type` is an enum with no length on the wire — `five_hour`, `weekly`, `monthly` — and
//! the seconds behind each name are the plugin's table, not the response's. A name outside
//! that table is skipped rather than guessed at, because a window drawn at the wrong length
//! puts the pace mark in the wrong place, which is worse than not drawing it.
//!
//! `percentUsed` is clamped rather than trusted: the plugin clamps, and a bar cannot render
//! 140% of itself.

use super::{Auth, Method, Spec};
use crate::providers::ProviderError;
use serde::Deserialize;
use tidemark_types::{
    AccountId, ProviderId, Snapshot, Timestamp, Window, WindowKey, WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "clinepass";

const USAGE_URL: &str = "https://api.cline.bot/api/v1/users/me/plan/usage-limits";

#[derive(Debug, Deserialize)]
struct Envelope {
    success: Option<bool>,
    data: Option<Data>,
}

#[derive(Debug, Deserialize)]
struct Data {
    limits: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Limit {
    #[serde(rename = "percentUsed")]
    percent_used: f64,
    #[serde(rename = "resetsAt")]
    resets_at: Option<String>,
}

/// The window kinds this parser understands, and how long each one lasts.
fn length_of(kind: &str) -> Option<u64> {
    match kind {
        "five_hour" => Some(18_000),
        "weekly" => Some(604_800),
        "monthly" => Some(2_592_000),
        _ => None,
    }
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    match envelope.success {
        Some(true) => {}
        Some(false) => return Err(ProviderError::malformed("the provider reported failure")),
        None => return Err(ProviderError::malformed("no success flag")),
    }
    let data = envelope
        .data
        .ok_or_else(|| ProviderError::malformed("successful response carried no data"))?;

    let mut windows = Vec::new();
    for entry in data.limits {
        // Recognise the kind before deserializing, so a quota type invented after this was
        // written can carry any shape it likes. Once recognised, a shape we cannot read is
        // an error.
        let Some(seconds) = entry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(length_of)
        else {
            continue;
        };
        let limit: Limit = serde_json::from_value(entry)
            .map_err(|e| ProviderError::malformed(format!("limit entry is not readable: {e}")))?;
        let length = WindowLength::from_secs(seconds)
            .expect("length_of never yields zero seconds");
        let resets_at = match limit.resets_at.as_deref() {
            Some(raw) => Some(Timestamp::from_rfc3339(raw).map_err(|e| {
                ProviderError::malformed(format!("unreadable reset time {raw}: {e}"))
            })?),
            None => None,
        };
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: crate::providers::length_title(length),
            used_percent: limit.percent_used.clamp(0.0, 100.0),
            subtitle: None,
            resets_at,
            length: Some(length),
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

/// ClinePass as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "ClinePass",
    endpoint: |_| USAGE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "Cline dashboard → API keys.",
    options: &[],
};
```

If `Timestamp` has no `from_rfc3339`, add one to `crates/tidemark-types/src/time.rs` in
this task, with its own tests for a `Z` suffix, an offset, and an unparseable string — most
of the remaining providers state resets as ISO-8601 and will all need it.

Register it in `keyed/mod.rs`: `pub mod clinepass;` and `&clinepass::SPEC` in `CATALOG`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark-core clinepass`
Expected: PASS, 7 tests.

- [ ] **Step 5: Full verification and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemark-core/src/providers/keyed/clinepass.rs crates/tidemark-core/src/providers/keyed/mod.rs crates/tidemark-types/src/time.rs
git commit -m "Add the ClinePass provider"
```

---

## Tasks 3–8: The remaining single-request JS-plugin providers

Each runs the porting procedure with the facts below. Each ends in its own commit and its
own passing tests.

### Task 3: Crof

- Create: `crates/tidemark-core/src/providers/keyed/crof.rs`, slug `crof`
- Source: `Resources/Plugins/crof.js`; fixtures in `Tests/CodexBarTests/CrofUsageFetcherTests.swift`, `CrofProviderImplementationTests.swift`, `CrofMenuCardTests.swift`
- `GET https://crof.ai/usage_api/`, `Authorization: Bearer <key>`
- Reports a credit figure and a daily request allowance that resets at midnight
  `America/Chicago`. The daily reset is computed, not returned — carry that computation into
  `parse` and test it against a fixed `captured_at`, including the day it rolls over.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 4: Venice

- Create: `keyed/venice.rs`, slug `venice`
- Source: `Resources/Plugins/venice.js`; fixtures in `VeniceUsageFetcherTests.swift`, `VeniceSettingsReaderTests.swift`
- `GET https://api.venice.ai/api/v1/billing/balance`, bearer
- Two currencies in one response: USD and DIEM, where DIEM has an epoch allocation and USD
  does not. DIEM against its allocation is a balance window with a subtitle; USD with no
  limit is a detail row only. Test both, and test the response that carries neither.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 5: Synthetic

- Create: `keyed/synthetic.rs`, slug `synthetic`
- Source: `Resources/Plugins/synthetic.js`; fixtures in `SyntheticProviderTests.swift`, `SyntheticMenuCardTests.swift`
- `GET https://api.synthetic.new/v2/quotas`, bearer
- The plugin accepts a *list* of alternative field names for the limit (`limit`,
  `message_limit`, `request_limit`, `quota`) and for the reset (`resetAt`, `reset_at`,
  `resetsAt`). Port the alternatives as written; a body using each spelling is a separate
  test. The root may be either an array or an object with a `quotas` key — both are real.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 6: ClawRouter

- Create: `keyed/clawrouter.rs`, slug `clawrouter`
- Source: `Resources/Plugins/clawrouter.js`; fixtures in `ClawRouterUsageFetcherTests.swift`
- `GET` against `https://clawrouter.openclaw.ai` by default, with a `base_url` option
  (`CLAWROUTER_BASE_URL` in CodexBar, HTTPS-only). Bearer.
- Amounts are in micros — `budget.limitMicros` and the spent figure — so the subtitle is
  formatted from micros to dollars. The window is monthly, and its reset is derived from a
  `windowKey`. Test a micros value that would lose precision as `f64` dollars.
- The `base_url` option is a free-text `OptionSchema` with empty `choices`. Reject a
  non-HTTPS value in `endpoint` by falling back to the default host, and test that.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 7: sub2api

- Create: `keyed/sub2api.rs`, slug `sub2api`
- Source: `Resources/Plugins/sub2api.js`; fixtures in `Sub2APIUsageFetcherTests.swift`, `Sub2APIPluginGoldenTests.swift`, `Sub2APIMenuCardModelTests.swift`
- Bearer. **The base URL is required**, not optional — CodexBar has no default host. Publish
  it as a free-text option and return an endpoint that fails the fetch cleanly when unset:
  `parse` cannot help here, so `Keyed::new` must produce a URL that `reqwest` rejects, and
  the task adds a test that an unset base URL yields `ProviderError::Client` rather than a
  request to nowhere. If that reads badly, add `endpoint: fn(&Options) -> Result<String, ProviderError>`
  to `Spec` in this task and update the six existing specs — say which you chose in the commit.
- `quota` is `{limit, used, remaining, unit}` with a unit that may be absent and defaults to
  USD: a balance window with a subtitle. `subscription` adds daily and weekly limits.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 8: OpenAI

- Create: `keyed/openai.rs`, slug `openai-api` — **not** `openai`, which CodexBar uses for
  the cookie-backed dashboard provider this plan does not port
- Source: `Resources/Plugins/openai.js`; fixtures in `OpenAIAPIUsageFetcherTests.swift`, `OpenAIAPICreditBalanceTests.swift`, `OpenAIAPIMenuCardModelTests.swift`
- Bearer against `https://api.openai.com`
- **Read Step A carefully before writing anything.** The plugin paginates usage over
  31-day ranges and separately reads `/v1/dashboard/billing/credit_grants`. If both are
  needed for the card, this is a multi-request provider and belongs in Task 25; if the
  credit-grants call alone carries the balance and its expiry, it is a single GET and
  belongs here. Decide from the source, port accordingly, and say which in the commit.

---

## Tasks 9–24: The Swift-backed providers

Facts below are the endpoints and auth found in CodexBar's Swift sources. Each task reads
the fetcher and the usage-stats type, harvests fixtures from the named test file, and runs
the porting procedure.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 9: Chutes
- Slug `chutes`. `GET https://api.chutes.ai/...`, bearer, `Accept: application/json`.
- Source: `Providers/Chutes/ChutesUsageStats.swift` (1045 lines — the meaning is here), `ChutesSettingsReader.swift`, `ChutesProviderDescriptor.swift`. Fixtures: `ChutesProviderTests.swift`, `ChutesPresentationTests.swift`.
- CodexBar labels its windows "4-hour quota" and "Monthly quota"; confirm both lengths from the payload rather than the label.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 10: ZenMux
- Slug `zenmux`. `GET https://zenmux.ai/api/v1/management`, bearer.
- Source: `Providers/ZenMux/`. Fixtures: `ZenMuxProviderTests.swift`.
- Balance-shaped: expect credits with a limit.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 11: NeuralWatt
- Slug `neuralwatt`. `GET https://api.neuralwatt.com/...`, bearer.
- Source: `Providers/NeuralWatt/`. Fixtures: `NeuralWattUsageFetcherTests.swift`.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 12: AiAnd
- Slug `aiand`. `GET https://api.aiand.com/logs`, bearer.
- Source: `Providers/AiAnd/`. Fixtures: `AiAndProviderTests.swift`.
- The endpoint is a log feed, so consumption is probably aggregated in the client. If the aggregation needs a second page, this is a multi-request provider — Task 27.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 13: Fireworks
- Slug `fireworks`. `GET https://api.fireworks.ai/v1/accounts/<account>`, bearer. The account id is part of the path — publish it as a required free-text option, the same shape Task 7 settles.
- Source: `Providers/Fireworks/`. Fixtures: `FireworksUsageFetcherTests.swift`, `FireworksSettingsReaderTests.swift`.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 14: ElevenLabs
- Slug `elevenlabs`. `GET https://api.elevenlabs.io/...`. **The key is not a bearer token** — CodexBar sends no `Authorization` header, so find the header it does send (`xi-api-key`) in the fetcher and use `Auth::Header`.
- Source: `Providers/ElevenLabs/`. Fixtures: `ElevenLabsUsageFetcherTests.swift`.
- Character quota against a monthly limit: a quota window with a subtitle in characters.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 15: DeepInfra
- Slug `deepinfra`. Bearer. Two endpoints: `https://api.deepinfra.com/payment/usage?from=current` and `.../payment/checklist?compute_owed=true`.
- Source: `Providers/DeepInfra/`. Fixtures: `DeepInfraUsageFetcherTests.swift`, `DeepInfraMenuBarMetricWindowResolverTests.swift`.
- If the card needs both, this is Task 25. If `usage` alone is enough, port it here and record what the `checklist` call adds that we are leaving out.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 16: LiteLLM
- Slug `litellm`. `GET <base>/key/info`, bearer. Self-hosted: base URL is a required free-text option.
- Source: `Providers/LiteLLM/`. Fixtures: `LiteLLMUsageFetcherTests.swift`, `LiteLLMMenuCardModelTests.swift`.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 17: LLMProxy
- Slug `llmproxy`. `GET <base>/v1/quota-stats`, bearer. Self-hosted: base URL is a required free-text option.
- Source: `Providers/LLMProxy/`. Fixtures: `LLMProxyUsageFetcherTests.swift`.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 18: IBM Bob
- Slug `ibmbob`. `GET https://api.us-east.bob.ibm.com/...`, and the host is regional — CodexBar builds it from a `host` value. Publish the region as an `OptionSchema` with the choices CodexBar knows.
- **No `Authorization` header appears in the source**; find the header the fetcher sets and use `Auth::Header`. It also sets its own `User-Agent`, which `providers::http::client()` already owns — do not override it.
- Source: `Providers/IBMBob/`. Fixtures: `IBMBobUsageFetcherTests.swift`.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 19: Amp
- Slug `amp`. `GET https://ampcode.com/api/internal?userDisplayBalanceInfo`, bearer.
- Source: `Providers/Amp/`. Fixtures: `AmpUsageParserTests.swift` (the parser tests are the contract), `AmpUsageFetcherTests.swift`.
- Balance-shaped. The POST paths in the source belong to the login flow, which is out of scope.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 20: Groq
- Slug `groq`. Bearer against `https://api.groq.com/v1`.
- Source: `Providers/Groq/`. Fixtures: `GroqUsageFetcherTests.swift`, `GroqMenuCardModelTests.swift`, and **not** `GroqConsoleFetcherTests.swift` — the console path authenticates through Stytch sessions and is out of scope.
- If the API-key path in the source turns out to reach only the console, report it and leave Groq unported rather than inventing an endpoint.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 21: Ollama
- Slug `ollama`. Bearer. Self-hosted or `https://ollama.com`; the base URL is an option with `https://ollama.com` as the default.
- Source: `Providers/Ollama/OllamaUsageParser.swift` is the contract. Fixtures: `OllamaUsageParserTests.swift`, `OllamaUsageFetcherTests.swift`, `OllamaUsageFetcherRetryMappingTests.swift` — the last of those tells you how it signals rate limiting, which must map onto `ProviderError::RateLimited`.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 22: Warp
- Slug `warp`. **POST** `https://app.warp.dev/graphql/v2` with a fixed GraphQL query body and `Authorization`. This is what `Method::Post` exists for: copy the query verbatim from the Swift source into the spec's `body`.
- Source: `Providers/Warp/`. Fixtures: `WarpUsageFetcherTests.swift`.
- CodexBar also sends `x-warp-os-category`; carry it in `headers` with a comment saying it came from CodexBar and its necessity is unverified.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 23: Azure OpenAI
- Slug `azure-openai`. **POST**, and the host is the customer's own resource — every part of the URL is an option.
- Source: `Providers/AzureOpenAI/`. Fixtures: `AzureOpenAIUsageFetcherTests.swift`.
- **Read first.** If the request needs a per-call body built from the current time or a deployment list, `Method::Post` with a fixed body cannot express it and this is Task 25 work. Decide from the source.

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 24: Factory
- Slug `factory`. Bearer against `https://api.factory.ai`, paths `/api/billing/limits` and `/api/organization/subscription/usage`.
- Source: `Providers/Factory/FactoryAPIKeyUsage*.swift`. Fixtures: `FactoryAPIKeyUsageTests.swift`, `FactoryProviderImplementationTests.swift`.
- Two endpoints means Task 25 unless one suffices. The WorkOS and Safari-container paths in that directory are the browser login flow and are out of scope.

---

- [ ] **A: read the contract** — the sources named above, before writing any Rust
- [ ] **B: harvest the fixtures** — recorded bodies copied verbatim into the test module
- [ ] **C: write the failing tests** — fixture agreement, malformed body, the unknown-kind rule
- [ ] **D: run them and watch them fail** — `cargo test -p tidemark-core <slug>`
- [ ] **E: write the file** — `SPEC`, `parse`, module doc naming every trap found in A
- [ ] **F: pass, verify, commit** — the full verification command, then one commit

### Task 25: The multi-request providers

**Files:**
- Create: one file per provider under `crates/tidemark-core/src/providers/keyed/`
- Modify: `crates/tidemark-core/src/providers/keyed/mod.rs` (these register as
  hand-written `Arc<dyn Provider>` builders, not as `CATALOG` specs — extend
  `tidemarkd::registry` with a second table mapping slug to builder, keyed by
  `CredentialKind::Key`, so the credentials dialog is unchanged)

**Known members:** Poe, OpenRouter, xAI, plus anything demoted from Tasks 3–24.

Each keeps its own `impl Provider`, reuses `providers::http::{client, check,
retry_after_header}`, and splits every response body into a pure `parse_*` function so the
same three tests from the porting procedure still apply. Facts gathered so far:

- **Poe** (`Plugins/poe.js`, `PoeUsageFetcherTests.swift`): `GET https://api.poe.com/usage/current_balance`,
  then up to five pages of `/usage/points_history?limit=100&starting_after=<cursor>`, 30-day
  cutoff. Points balance with no limit: no window, details only. The history pages are
  wrapped in a `try` that swallows failures in CodexBar — keep that, and make a swallowed
  history failure visible in the details rather than silent.
- **OpenRouter** (`Plugins/openrouter.js`, `OpenRouterUsageStatsTests.swift`, `OpenRouterTestSnapshots.swift`):
  key quota and credits, with an `OPENROUTER_API_URL` override. `limit`, `limit_remaining`,
  `usage`, `usage_daily/weekly/monthly` and a `rate_limit` object; `limit_reset` is a string.
  A key with a limit is a balance window; a key without one is details only.
- **xAI** (`Plugins/xai.js`, `XAIProviderTests.swift`): needs a **team ID** as well as a key —
  publish it as a required free-text option and reject `/`, `.` and `..` as the plugin does.
  `GET https://management-api.x.ai/v1/billing/teams/<team>/prepaid/balance` returns
  `total.val` as a **string of cents, negated** — the balance is `-Number(val)/100`. A
  second POST fetches 30 days of spend for the chart. Prepaid credits have no limit: details
  only, no window. The management key is not an inference key, and the credential hint must
  say so.

Each provider is its own task, its own tests, its own commit, and stops rather than guesses
if the source does not say what a field means.

---

### Task 26: Report what did not fit

**Files:**
- Modify: `docs/superpowers/specs/2026-08-21-keyed-provider-port-design.md` (a "Ported" section at the end)
- Modify: `PLAN.md` (a dated entry, in the style of the existing ones)

- [ ] **Step 1: Write down the outcome**

For every provider in Tasks 2–25, one line: ported, moved to Task 27, or not ported and
why. A provider that turned out to need a cookie, a browser, or another CLI's config file
is a finding worth keeping — it is the answer to "why isn't X here" for the next person.

- [ ] **Step 2: Record the unverified surface**

State plainly how many providers now ship having never been seen answering, and that their
tests assert agreement with CodexBar rather than with the APIs.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-08-21-keyed-provider-port-design.md PLAN.md
git commit -m "Record what the keyed port covered"
```

---

## Self-Review Notes

- The spec's `Keyed` design assumed one GET. Tasks 7, 22 and 23 are where that assumption
  is tested; `Method::Post` and the required-option question are called out at the point
  they bite rather than pre-solved here.
- Six of the twenty-six are named in this plan as "read first, then decide" (Tasks 8, 12,
  15, 20, 23, 24). That is deliberate: their CodexBar sources mix an API-key path with a
  browser-login path, and committing them to a shape now would be guessing.
- No parser is pre-written here beyond ClinePass. A parser written from a partial read of
  Swift, into a markdown file, reviewed by nobody, and executed days later is worse than
  no parser: the procedure and the fixtures are what make the port checkable.
