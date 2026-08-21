# Keyed Provider Mechanism Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the shared `providers::keyed` mechanism, migrate Kimi and Z.ai onto it, give `Window` a place to carry absolutes, and make the daemon's provider catalog a table — so that adding a provider afterwards is one file and one line.

**Architecture:** One `Keyed` client type reads a `&'static Spec` describing endpoint, HTTP method, auth placement and a pure `parse` function. `Keyed::fetch` performs the request and hands the body to `parse`, which is a plain `fn` with no client in scope and therefore cannot make a request of its own. `tidemarkd::registry` iterates `keyed::CATALOG` to produce both the published `ProviderDefinition` list and the built `Account`s, appending the hand-written OAuth providers.

**Tech Stack:** Rust 2024, `reqwest`, `serde`/`serde_json`, `zvariant` for D-Bus wire shapes, GTK4/libadwaita in the GUI crate only.

**Spec:** `docs/superpowers/specs/2026-08-21-keyed-provider-port-design.md`

## Global Constraints

- All documentation, source code, code comments, tests, logs, and interface copy are written in English.
- Crate layering is enforced, not aspirational: `tidemark-types` reaches nothing with I/O; `tidemark-core` never reaches GTK/GDK/libadwaita; `tidemark` never reaches `tidemark-core`, HTTP or SQLite. `./scripts/check-layering.sh` asserts it.
- The workspace builds clean at `-D warnings`, and `cargo fmt` is clean.
- A provider must never silently drop a window. An entry of a recognised kind that cannot be parsed is a `ProviderError::Malformed` for the whole fetch; only an entry of an unrecognised kind is skipped.
- Every outbound request carries the `Tidemark/<version>` user agent, built only through `providers::http::client()`.
- A `Credential` never reaches a log. `Debug` on any type holding one is written by hand or delegates to `Credential`'s redacting impl.
- Provider slugs are storage keys and never change once shipped.

**Verification commands** (used by every task's final step):

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
```

---

### Task 1: The `keyed` mechanism

**Files:**
- Create: `crates/tidemark-core/src/providers/keyed.rs`
- Modify: `crates/tidemark-core/src/providers/mod.rs` (add `pub mod keyed;` beside the existing `pub mod http;`)

**Interfaces:**
- Consumes: `providers::http::{client, check, retry_after_header}`, `providers::{Credential, Provider, ProviderError, BoxFuture}`, `tidemark_types::{Snapshot, Timestamp, ProviderId, AccountId}`.
- Produces, relied on by every later task and by the ports plan:
  - `pub enum Auth { Bearer, Header(&'static str), Query(&'static str) }`
  - `pub enum Method { Get, Post { body: &'static str, content_type: &'static str } }`
  - `pub struct OptionSchema { pub name: &'static str, pub title: &'static str, pub description: Option<&'static str>, pub default: &'static str, pub choices: &'static [(&'static str, &'static str)] }` — `choices` empty means free text.
  - `pub type Options = std::collections::BTreeMap<String, String>;`
  - `pub struct Spec { pub id: &'static str, pub title: &'static str, pub endpoint: fn(&Options) -> String, pub method: Method, pub auth: Auth, pub headers: &'static [(&'static str, &'static str)], pub parse: fn(&str, Timestamp) -> Result<Snapshot, ProviderError>, pub credential_hint: &'static str, pub options: &'static [OptionSchema] }`
  - `pub struct Keyed` with `pub fn new(spec: &'static Spec, credential: Credential, options: &Options) -> Result<Keyed, ProviderError>`, `pub fn url(&self) -> &str`, `pub fn build_request(&self) -> Result<reqwest::Request, ProviderError>`, and `impl Provider for Keyed`.
  - `pub static CATALOG: &[&'static Spec]` — empty in this task, filled by Tasks 2 and 3.

- [ ] **Step 1: Write the failing tests**

Create `crates/tidemark-core/src/providers/keyed.rs` containing only the test module below plus `use super::*;`. The tests describe the whole mechanism before any of it exists.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn snapshot_of(id: &str, captured_at: Timestamp) -> Snapshot {
        Snapshot {
            provider: ProviderId::new(id),
            account: AccountId::default(),
            captured_at,
            windows: Vec::new(),
            details: Vec::new(),
        }
    }

    fn parse_ok(_body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
        Ok(snapshot_of("test", captured_at))
    }

    static BEARER: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |_| "https://example.invalid/usage".to_owned(),
        method: Method::Get,
        auth: Auth::Bearer,
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[],
    };

    static HEADER: Spec = Spec {
        auth: Auth::Header("x-api-key"),
        ..BEARER_TEMPLATE
    };

    static QUERY: Spec = Spec {
        auth: Auth::Query("key"),
        ..BEARER_TEMPLATE
    };

    static POSTING: Spec = Spec {
        method: Method::Post {
            body: "{\"query\":\"usage\"}",
            content_type: "application/json",
        },
        ..BEARER_TEMPLATE
    };

    static REGIONAL: Spec = Spec {
        endpoint: |options| match options.get("region").map(String::as_str) {
            Some("cn") => "https://cn.example.invalid/usage".to_owned(),
            _ => "https://example.invalid/usage".to_owned(),
        },
        ..BEARER_TEMPLATE
    };

    #[test]
    fn a_bearer_key_goes_in_the_authorization_header() {
        let keyed = Keyed::new(&BEARER, Credential::new("sk-1"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer sk-1"
        );
        assert_eq!(request.headers().get("Accept").expect("present"), "application/json");
        assert_eq!(request.method(), reqwest::Method::GET);
    }

    #[test]
    fn a_header_key_goes_in_the_header_the_spec_names() {
        let keyed = Keyed::new(&HEADER, Credential::new("sk-2"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(request.headers().get("x-api-key").expect("present"), "sk-2");
        assert!(
            request.headers().get(reqwest::header::AUTHORIZATION).is_none(),
            "a header-auth provider must not also be sent a bearer token"
        );
    }

    #[test]
    fn a_query_key_goes_in_the_query_string_and_is_escaped() {
        let keyed = Keyed::new(&QUERY, Credential::new("a b&c"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(request.url().query_pairs().count(), 1);
        assert_eq!(
            request
                .url()
                .query_pairs()
                .find(|(name, _)| name == "key")
                .expect("present")
                .1,
            "a b&c"
        );
    }

    #[test]
    fn a_posting_spec_sends_its_body_and_content_type() {
        let keyed = Keyed::new(&POSTING, Credential::new("sk-3"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.headers().get(reqwest::header::CONTENT_TYPE).expect("present"),
            "application/json"
        );
        let body = request.body().expect("present").as_bytes().expect("in memory");
        assert_eq!(body, b"{\"query\":\"usage\"}");
    }

    #[test]
    fn the_endpoint_reads_the_accounts_options() {
        let global = Keyed::new(&REGIONAL, Credential::new("sk-4"), &options(&[])).expect("builds");
        let cn = Keyed::new(
            &REGIONAL,
            Credential::new("sk-4"),
            &options(&[("region", "cn")]),
        )
        .expect("builds");
        assert_eq!(global.url(), "https://example.invalid/usage");
        assert_eq!(cn.url(), "https://cn.example.invalid/usage");
    }

    #[tokio::test]
    async fn a_blank_credential_is_refused_before_a_request_is_spent() {
        let keyed = Keyed::new(&BEARER, Credential::new("   "), &options(&[])).expect("builds");
        assert!(matches!(
            keyed.fetch().await,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn the_client_reports_the_specs_identity() {
        let keyed = Keyed::new(&BEARER, Credential::new("sk-5"), &options(&[])).expect("builds");
        assert_eq!(keyed.id(), ProviderId::new("test"));
        assert_eq!(keyed.account(), AccountId::default());
    }

    #[test]
    fn a_keyed_client_never_prints_its_credential() {
        let keyed = Keyed::new(&BEARER, Credential::new("sk-super-secret"), &options(&[]))
            .expect("builds");
        let rendered = format!("{keyed:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
```

Note: `BEARER_TEMPLATE` above is the same value as `BEARER`; declare it once as
`const BEARER_TEMPLATE: Spec = Spec { /* the BEARER fields */ };` and define
`static BEARER: Spec = BEARER_TEMPLATE;` so the `..` struct-update syntax has a `const` to
draw from. `Spec` must therefore contain no field that forbids `const` construction.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark-core keyed`
Expected: FAIL to compile — `cannot find type Spec in this scope`, and the same for `Keyed`, `Auth`, `Method`, `Options`.

- [ ] **Step 3: Write the mechanism**

Above the test module in the same file:

```rust
//! The transport every key-authenticated provider shares, and the description of how
//! they differ.
//!
//! # Why a table and not a module each
//!
//! The first five providers were written one module apiece, because each of the five
//! acquires its credential differently and no two of them agreed on enough to share. The
//! providers in this module agree on almost everything: the user pastes a key, the key
//! goes on one request, and the response is JSON. What is left that differs is an
//! endpoint, where the key sits, and what the body means — which is exactly the fields of
//! [`Spec`].
//!
//! # The rule this module makes structural
//!
//! `providers::mod` states that transport and meaning are separate functions. Here that
//! stops being a convention: [`Spec::parse`] is a plain `fn` with no client, no
//! credential and no `async` in its signature. A parser physically cannot make a request,
//! so every trap in a response is reachable from a test that needs no network.

use super::{BoxFuture, Credential, Provider, ProviderError, http};
use std::collections::BTreeMap;
use std::fmt;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp};

/// The settings of one account, as `config.toml` holds them.
pub type Options = BTreeMap<String, String>;

/// Where the key goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// A header the provider names itself — `x-api-key`, `api-key`.
    Header(&'static str),
    /// A query parameter. Rare, and always worse: it reaches access logs.
    Query(&'static str),
}

/// How the request is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The common case.
    Get,
    /// A fixed body, for the providers whose usage endpoint is a GraphQL or analytics
    /// POST that takes no per-request input.
    Post {
        /// Sent verbatim.
        body: &'static str,
        /// `Content-Type` for the body.
        content_type: &'static str,
    },
}

/// One setting a provider lets the user choose, published so that the interface can draw
/// the control without knowing what the setting means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionSchema {
    /// Key under `[provider.<slug>]` in `config.toml`.
    pub name: &'static str,
    /// What to call it on screen.
    pub title: &'static str,
    /// A sentence under the control, when it needs one.
    pub description: Option<&'static str>,
    /// What it is when the user has not chosen.
    pub default: &'static str,
    /// `(value, label)` pairs. Empty means free text, such as a base URL.
    pub choices: &'static [(&'static str, &'static str)],
}

/// Everything a key-authenticated provider is.
#[derive(Debug)]
pub struct Spec {
    /// The stable slug this provider's history is filed under. Never changes once shipped.
    pub id: &'static str,
    /// What to call it in front of a person.
    pub title: &'static str,
    /// The URL to poll, given the account's settings.
    pub endpoint: fn(&Options) -> String,
    /// How the request is made.
    pub method: Method,
    /// Where the key goes.
    pub auth: Auth,
    /// Headers beyond auth and the shared user agent.
    pub headers: &'static [(&'static str, &'static str)],
    /// Turns a response body into a snapshot. Pure by construction.
    pub parse: fn(&str, Timestamp) -> Result<Snapshot, ProviderError>,
    /// One sentence saying which page the key is on.
    pub credential_hint: &'static str,
    /// What the user may choose.
    pub options: &'static [OptionSchema],
}

/// A client for one key against one [`Spec`].
pub struct Keyed {
    spec: &'static Spec,
    client: reqwest::Client,
    credential: Credential,
    url: String,
}

impl Keyed {
    /// Builds a client. The URL is resolved once, here, because a setting that changed
    /// the host would otherwise take effect only on the next daemon restart.
    pub fn new(
        spec: &'static Spec,
        credential: Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            spec,
            client: http::client()?,
            credential,
            url: (spec.endpoint)(options),
        })
    }

    /// The URL this instance polls.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The request this instance would send, built but not sent, so that the placement of
    /// the key is testable without a server.
    pub fn build_request(&self) -> Result<reqwest::Request, ProviderError> {
        let mut builder = match self.spec.method {
            Method::Get => self.client.get(&self.url),
            Method::Post { body, content_type } => self
                .client
                .post(&self.url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body),
        };
        for (name, value) in self.spec.headers {
            builder = builder.header(*name, *value);
        }
        builder = match self.spec.auth {
            Auth::Bearer => builder.bearer_auth(self.credential.expose()),
            Auth::Header(name) => builder.header(name, self.credential.expose()),
            Auth::Query(name) => builder.query(&[(name, self.credential.expose())]),
        };
        builder.build().map_err(ProviderError::Client)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let response = self
            .client
            .execute(self.build_request()?)
            .await
            .map_err(ProviderError::Transport)?;

        let status = response.status();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(status, retry_after.as_deref())?;

        let body = response.text().await.map_err(ProviderError::Transport)?;
        (self.spec.parse)(&body, Timestamp::now())
    }
}

impl fmt::Debug for Keyed {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyed")
            .field("id", &self.spec.id)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Provider for Keyed {
    fn id(&self) -> ProviderId {
        ProviderId::new(self.spec.id)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// Every key-authenticated provider this build supports, in the order they are shown.
///
/// Adding a provider is a file beside this one and a line here. Nothing else in the
/// workspace names it.
pub static CATALOG: &[&Spec] = &[];
```

Add `pub mod keyed;` to `crates/tidemark-core/src/providers/mod.rs` beside `pub mod http;`, keeping the list alphabetical.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark-core keyed`
Expected: PASS, 8 tests.

- [ ] **Step 5: Full verification**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
Expected: all clean. If clippy objects to `CATALOG` being an empty static, leave it — Task 2 fills it, and a `#[allow]` added now would outlive its reason.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark-core/src/providers/keyed.rs crates/tidemark-core/src/providers/mod.rs
git commit -m "Add the keyed provider mechanism"
```

---

### Task 2: Migrate Z.ai onto `keyed`

Z.ai goes first because its live responses have been observed and its tests were written against them. The migration is correct when every one of those assertions still passes.

**Files:**
- Modify: `crates/tidemark-core/src/providers/zai.rs` (remove the `Zai` struct, its `new`, `quota_url`, `fetch_inner` and `impl Provider`; add `SPEC` and `endpoint`)
- Modify: `crates/tidemark-core/src/providers/keyed.rs` (add `&zai::SPEC` to `CATALOG`)
- Modify: `crates/tidemarkd/src/registry.rs` (delete `zai_account`, its catalog stanza and its `account()` arm — Task 6 replaces the mechanism, but Z.ai must keep working in between)
- Modify: `crates/tidemark-core/examples/probe.rs` (it constructs `zai::Zai::new`)

**Interfaces:**
- Consumes: `keyed::{Auth, Method, Options, OptionSchema, Spec}` from Task 1.
- Produces: `zai::SPEC: Spec`, `zai::REGION` (the option name, `"region"`), `zai::parse` unchanged in signature and behaviour.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `crates/tidemark-core/src/providers/zai.rs`:

```rust
#[test]
fn the_spec_carries_the_region_to_the_endpoint() {
    use crate::providers::keyed::{Auth, Method, Options};

    let global = Options::new();
    let cn: Options = [("region".to_owned(), "bigmodel-cn".to_owned())]
        .into_iter()
        .collect();

    assert_eq!(
        (SPEC.endpoint)(&global),
        "https://api.z.ai/api/monitor/usage/quota/limit"
    );
    assert_eq!(
        (SPEC.endpoint)(&cn),
        "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
    );
    assert_eq!(SPEC.id, PROVIDER_ID);
    assert_eq!(SPEC.auth, Auth::Bearer);
    assert_eq!(SPEC.method, Method::Get);
}

#[test]
fn an_unknown_region_falls_back_to_global_rather_than_refusing_to_poll() {
    use crate::providers::keyed::Options;

    let nonsense: Options = [("region".to_owned(), "atlantis".to_owned())]
        .into_iter()
        .collect();
    assert_eq!(
        (SPEC.endpoint)(&nonsense),
        "https://api.z.ai/api/monitor/usage/quota/limit",
        "a typo in config.toml must not take the account off the air"
    );
}

#[test]
fn the_region_option_is_published_with_both_hosts() {
    let region = SPEC
        .options
        .iter()
        .find(|option| option.name == REGION)
        .expect("the region is published");
    let values: Vec<&str> = region.choices.iter().map(|(value, _)| *value).collect();
    assert_eq!(values, ["global", "bigmodel-cn"]);
    assert_eq!(region.default, "global");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p tidemark-core zai`
Expected: FAIL to compile — `cannot find value SPEC in this scope`.

- [ ] **Step 3: Replace the bespoke client with a spec**

In `crates/tidemark-core/src/providers/zai.rs`, delete `pub struct Zai`, `impl Zai`, and `impl Provider for Zai`. Keep `PROVIDER_ID`, `QUOTA_PATH`, `Region`, `parse` and everything below it untouched. Add:

```rust
/// Name of the region setting under `[provider.zai]`.
pub const REGION: &str = "region";

impl Region {
    /// The value this region is stored as in `config.toml`.
    pub fn as_value(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::BigModelCn => "bigmodel-cn",
        }
    }

    /// The region a stored value names. An unrecognised value is the default rather than
    /// an error: a typo in `config.toml` must not take the account off the air.
    pub fn from_value(raw: Option<&str>) -> Self {
        match raw {
            Some("bigmodel-cn") => Self::BigModelCn,
            _ => Self::Global,
        }
    }
}

/// Z.ai as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Z.ai",
    endpoint: |options| {
        let region = Region::from_value(options.get(REGION).map(String::as_str));
        format!("{}{QUOTA_PATH}", region.base_url())
    },
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[],
    parse,
    credential_hint: "Z.ai dashboard → API keys, on whichever region your account is on.",
    options: &[OptionSchema {
        name: REGION,
        title: "Region",
        description: Some("Two hosts for one API. Pick the one your account is on."),
        default: "global",
        choices: &[
            ("global", "Global (api.z.ai)"),
            ("bigmodel-cn", "China (open.bigmodel.cn)"),
        ],
    }],
};
```

Change the module's imports to `use super::keyed::{Auth, Method, OptionSchema, Spec};` alongside what `parse` already needs, and drop the now-unused `BoxFuture`, `Credential`, `Provider`, `http` imports.

Add `&zai::SPEC` to `keyed::CATALOG`:

```rust
pub static CATALOG: &[&Spec] = &[&super::zai::SPEC];
```

In `crates/tidemarkd/src/registry.rs`, replace the body of `zai_account` so the account is built through `Keyed` rather than `Zai`:

```rust
fn zai_account() -> Account {
    Account::new(
        ProviderId::new(zai::PROVIDER_ID),
        AccountId::default(),
        Box::new(|credential, options| {
            // The URL is resolved at build time, which is why storing a key or changing
            // the region drops the client: both change which host this account talks to.
            Ok(Arc::new(keyed::Keyed::new(&zai::SPEC, credential, options)?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint(zai::SPEC.credential_hint)
}
```

In `crates/tidemark-core/examples/probe.rs`, replace the `zai::Zai::new(...)` construction with `keyed::Keyed::new(&zai::SPEC, Credential::new(key.trim()), &options)`, building `options` from the existing region argument with `[(zai::REGION.to_owned(), region.as_value().to_owned())]`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark-core zai && cargo test --workspace`
Expected: PASS. Every pre-existing Z.ai parse assertion — the unit table, the millisecond
timestamps, the just-reset window with no `nextResetTime`, the monthly MCP special case —
must pass untouched. If any of them needed editing to go green, the migration changed
behaviour and must be reworked rather than the test relaxed.

- [ ] **Step 5: Full verification**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
Expected: all clean.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark-core/src/providers/zai.rs crates/tidemark-core/src/providers/keyed.rs crates/tidemarkd/src/registry.rs crates/tidemark-core/examples/probe.rs
git commit -m "Move Z.ai onto the keyed mechanism"
```

---

### Task 3: Migrate Kimi onto `keyed`

**Files:**
- Modify: `crates/tidemark-core/src/providers/kimi.rs` (remove `Kimi`, `new`, `with_base_url`, `fetch_inner`, `impl Provider`; add `SPEC`)
- Modify: `crates/tidemark-core/src/providers/keyed.rs` (add `&kimi::SPEC` to `CATALOG`)
- Modify: `crates/tidemarkd/src/registry.rs` (`kimi_account` builds a `Keyed`)

**Interfaces:**
- Consumes: `keyed::{Auth, Method, Spec}` from Task 1.
- Produces: `kimi::SPEC: Spec`; `kimi::parse` unchanged.

Kimi has no settings, so `options` is `&[]` and `endpoint` ignores its argument. The
existing `with_base_url` constructor exists only so a test could point the client at an
unreachable host; that test is replaced below, because `Keyed` covers the same ground in
Task 1's `a_blank_credential_is_refused_before_a_request_is_spent`.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `crates/tidemark-core/src/providers/kimi.rs`:

```rust
#[test]
fn the_spec_polls_the_coding_api_with_a_bearer_key() {
    use crate::providers::keyed::{Auth, Method, Options};

    assert_eq!(
        (SPEC.endpoint)(&Options::new()),
        "https://api.kimi.com/coding/v1/usages",
        "not www.kimi.com, which wants a session cookie rather than a key"
    );
    assert_eq!(SPEC.id, PROVIDER_ID);
    assert_eq!(SPEC.auth, Auth::Bearer);
    assert_eq!(SPEC.method, Method::Get);
    assert!(SPEC.options.is_empty(), "Kimi has nothing to choose");
}
```

Delete the existing test that constructs `Kimi::with_base_url(Credential::new("sk-1"), "http://127.0.0.1:9/")` — its subject no longer exists, and Task 1 covers the behaviour it asserted.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p tidemark-core kimi`
Expected: FAIL to compile — `cannot find value SPEC in this scope`.

- [ ] **Step 3: Replace the bespoke client with a spec**

In `crates/tidemark-core/src/providers/kimi.rs`, delete `pub struct Kimi`, `impl Kimi`, and
`impl Provider for Kimi`. Keep `PROVIDER_ID`, `BASE_URL`, `USAGES_PATH`, `parse` and
everything below. Add:

```rust
/// Kimi For Coding as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Kimi",
    endpoint: |_| format!("{BASE_URL}{USAGES_PATH}"),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[],
    parse,
    credential_hint:
        "Kimi Code Console → API keys. This is Kimi For Coding, not the Open Platform.",
    options: &[],
};
```

Add `&kimi::SPEC` to `CATALOG`, before `&zai::SPEC` to keep the catalog alphabetical:

```rust
pub static CATALOG: &[&Spec] = &[&super::kimi::SPEC, &super::zai::SPEC];
```

In `registry.rs`, rewrite `kimi_account`:

```rust
fn kimi_account() -> Account {
    Account::new(
        ProviderId::new(kimi::PROVIDER_ID),
        AccountId::default(),
        Box::new(|credential, options| {
            Ok(Arc::new(keyed::Keyed::new(&kimi::SPEC, credential, options)?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint(kimi::SPEC.credential_hint)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark-core kimi && cargo test --workspace`
Expected: PASS. Kimi's numbers-as-strings assertions and its absolute request counts must
pass untouched.

- [ ] **Step 5: Full verification**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
Expected: all clean.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark-core/src/providers/kimi.rs crates/tidemark-core/src/providers/keyed.rs crates/tidemarkd/src/registry.rs
git commit -m "Move Kimi onto the keyed mechanism"
```

---

### Task 4: `Window.subtitle` through the type and the wire

**Files:**
- Modify: `crates/tidemark-types/src/window.rs` (the `Window` struct and its constructors in tests)
- Modify: `crates/tidemark-types/src/wire.rs` (`WindowStatus`, `from_window`, `to_window`)
- Modify: every construction of `Window` in the workspace: `crates/tidemark-core/src/providers/zai.rs`, `kimi.rs`, `claude.rs`, `codex.rs`, `antigravity/mod.rs`, plus test helpers in `crates/tidemark-types/src/snapshot.rs`, `crates/tidemark/src/model.rs`, `crates/tidemark/src/detail.rs`, `crates/tidemark-core/src/storage`

**Interfaces:**
- Produces: `Window.subtitle: Option<String>` and `WindowStatus.subtitle: Option<String>`, relied on by Task 5 and by every balance-shaped provider in the ports plan.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/tidemark-types/src/wire.rs`:

```rust
#[test]
fn absolutes_survive_the_round_trip_to_the_wire_and_back() {
    let mut source = window(Some(1_785_704_500));
    source.subtitle = Some("100 / 1000 credits".to_owned());
    let published = WindowStatus::from_window(&source);
    assert_eq!(published.subtitle.as_deref(), Some("100 / 1000 credits"));
    assert_eq!(published.to_window().subtitle, source.subtitle);
}

#[test]
fn a_window_with_no_absolutes_publishes_no_subtitle_key() {
    let published = WindowStatus::from_window(&window(None));
    assert!(
        published.subtitle.is_none(),
        "an absent key is how a{{sv}} says the provider did not tell us"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark-types wire`
Expected: FAIL to compile — `no field subtitle on type Window`.

- [ ] **Step 3: Add the field**

In `crates/tidemark-types/src/window.rs`, add to `Window`, after `title`:

```rust
    /// The absolute quantities behind [`Window::used_percent`], already formatted by the
    /// adapter — `100 / 1000 credits`, `4.2M / 10M tokens`.
    ///
    /// Presentation the provider owns rather than a pair of numbers, because the unit is
    /// the provider's and so is the rounding: a credit balance in dollars and a token
    /// allowance in millions are not the same kind of quantity, and a shared formatter
    /// would have to pick a house style for both. The interface draws it small under the
    /// bar and never parses it.
    ///
    /// `None` where the provider reported only a percentage, which is the common case.
    pub subtitle: Option<String>,
```

In `crates/tidemark-types/src/wire.rs`, add the matching field to `WindowStatus` after
`title`, carry it in `from_window` with `window.subtitle.clone()`, and in `to_window` with
`self.subtitle.clone()`.

Then fix every `Window { .. }` construction in the workspace by adding `subtitle: None`.
The compiler enumerates them; work through the list until `cargo build --workspace` is
clean. No existing provider sets it in this task.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS, including the two new tests.

- [ ] **Step 5: Full verification**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
Expected: all clean.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark-types crates/tidemark-core crates/tidemark
git commit -m "Carry provider absolutes on a window"
```

---

### Task 5: Draw the absolutes

**Files:**
- Modify: `crates/tidemark/src/card.rs` (a dim label under the dominant window's bar)
- Modify: `crates/tidemark/src/detail.rs` (the same value for whichever window is selected)

**Interfaces:**
- Consumes: `WindowStatus::to_window().subtitle` from Task 4.

The card already has a `reset` label under the bar. The subtitle is a second line, placed
below it, hidden when absent so that a provider that reports only a percentage keeps the
card's current height.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/tidemark/src/card.rs`:

```rust
#[test]
fn absolutes_are_shown_under_the_bar_when_the_provider_reported_them() {
    let mut dominant = window(Some(18_000), 42.0);
    dominant.subtitle = Some("420 / 1000 credits".to_owned());
    let card = Card::new();
    card.set(&status_with(vec![dominant]));
    assert_eq!(card.absolutes_label().label(), "420 / 1000 credits");
    assert!(card.absolutes_label().is_visible());
}

#[test]
fn the_absolutes_line_disappears_rather_than_showing_an_empty_row() {
    let card = Card::new();
    card.set(&status_with(vec![window(Some(18_000), 42.0)]));
    assert!(
        !card.absolutes_label().is_visible(),
        "a provider that reports only a percentage must not grow a blank line"
    );
}
```

Use the module's existing test helpers for building a `ProviderStatus`; if the module has
no `window`/`status_with` helper yet, write them beside the tests in the same shape the
detail module's tests already use.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark card`
Expected: FAIL to compile — `no method named absolutes_label`.

- [ ] **Step 3: Add the label**

In `card.rs`, build a label beside the existing `reset` one:

```rust
        let absolutes = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["caption", "dim-label"])
            .build();
```

Append it to the same container the `reset` label is in, immediately after it. Store it on
the struct as `absolutes: gtk::Label`, expose `pub(crate) fn absolutes_label(&self) -> &gtk::Label`
for the tests, and set it wherever the dominant window is applied:

```rust
        match dominant.subtitle.as_deref() {
            Some(text) => {
                self.absolutes.set_label(text);
                self.absolutes.set_visible(true);
            }
            None => {
                self.absolutes.set_label("");
                self.absolutes.set_visible(false);
            }
        }
```

Apply the same treatment in `detail.rs` for the selected window, next to where its reset
time is shown, following that module's existing pattern for optional rows.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark`
Expected: PASS.

- [ ] **Step 5: Look at it**

Run the GUI under `xvfb-run` against `examples/mock-daemon.rs` with one account whose
dominant window carries a subtitle, screenshot the window, and confirm the line sits under
the bar without changing the card's width or crowding the reset time. Add the subtitle to
one of the mock daemon's invented accounts so the case stays reachable later.

- [ ] **Step 6: Full verification and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemark/src/card.rs crates/tidemark/src/detail.rs crates/tidemarkd/examples/mock-daemon.rs
git commit -m "Draw provider absolutes under the quota bar"
```

---

### Task 6: The catalog becomes a table

**Files:**
- Modify: `crates/tidemarkd/src/registry.rs` (the `catalog`, `account` and `options` functions; delete `kimi_account`, `zai_account`, `ZAI_REGION`, `ZAI_GLOBAL`, `ZAI_BIGMODEL_CN`)

**Interfaces:**
- Consumes: `keyed::{CATALOG, Keyed, Spec, OptionSchema}` from Tasks 1–3.
- Produces: no new public surface. `registry::catalog` and `registry::account` keep their
  signatures; only their bodies stop naming key-authenticated providers one at a time.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/tidemarkd/src/registry.rs`:

```rust
#[test]
fn every_keyed_spec_reaches_the_published_catalog() {
    let config = Config::default();
    let published = catalog(&config);
    for spec in keyed::CATALOG {
        let entry = published
            .iter()
            .find(|definition| definition.provider == spec.id)
            .unwrap_or_else(|| panic!("{} is in the catalog but not published", spec.id));
        assert_eq!(entry.title, spec.title);
        assert_eq!(entry.credential, CredentialKind::Key.as_wire());
        assert_eq!(entry.credential_hint, spec.credential_hint);
        assert_eq!(entry.options.len(), spec.options.len());
    }
}

#[test]
fn the_oauth_providers_keep_the_head_of_the_catalog() {
    let published = catalog(&Config::default());
    let slugs: Vec<&str> = published
        .iter()
        .map(|definition| definition.provider.as_str())
        .collect();
    assert_eq!(&slugs[..3], &["antigravity", "claude", "codex"]);
}

#[test]
fn a_keyed_spec_builds_a_configured_account() {
    let secrets: Arc<dyn Secrets> = Arc::new(NoSecrets);
    let built = account("zai", &secrets, &Config::default()).expect("no error");
    assert!(built.is_some(), "a slug in keyed::CATALOG must build");
}

#[test]
fn a_slug_no_build_supports_is_still_not_an_account() {
    let secrets: Arc<dyn Secrets> = Arc::new(NoSecrets);
    assert!(
        account("nonesuch", &secrets, &Config::default())
            .expect("no error")
            .is_none(),
        "an unknown slug is warned about, not turned into an account"
    );
}

#[test]
fn a_published_option_carries_the_users_current_value() {
    let mut config = Config::default();
    config.set_option("zai", "region", "bigmodel-cn");
    let published = catalog(&config);
    let zai = published
        .iter()
        .find(|definition| definition.provider == "zai")
        .expect("published");
    let region = zai
        .options
        .iter()
        .find(|option| option.name == "region")
        .expect("published");
    assert_eq!(region.value, "bigmodel-cn");
    assert_eq!(region.choices.len(), 2);
}
```

If `Config` has no `set_option` helper, use whatever the existing `options` tests in this
module already use to seed a value, and keep the assertion.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemarkd registry`
Expected: FAIL — `every_keyed_spec_reaches_the_published_catalog` passes only by accident
today; `a_published_option_carries_the_users_current_value` fails because the region's
choices are built by hand rather than from the spec, and the head-of-catalog test fails
once the hand-written Kimi and Z.ai stanzas are removed in Step 3 before the table replaces
them.

- [ ] **Step 3: Replace the stanzas with the table**

Rewrite `catalog` so the three OAuth providers stay written out and the keyed ones come
from the table:

```rust
pub fn catalog(config: &Config) -> Vec<ProviderDefinition> {
    let mut definitions = vec![
        // ... the existing antigravity, claude and codex stanzas, unchanged ...
    ];
    definitions.extend(keyed::CATALOG.iter().map(|spec| ProviderDefinition {
        provider: spec.id.to_owned(),
        title: spec.title.to_owned(),
        credential: CredentialKind::Key.as_wire().to_owned(),
        credential_hint: spec.credential_hint.to_owned(),
        external_fallback: None,
        options: options(spec.id, config),
    }));
    definitions
}
```

Rewrite `options` so a keyed provider's schema comes from its spec rather than from a
`match` on the slug, keeping the existing Antigravity branch. Antigravity's option is
currently built by a helper reached from `options`; keep that helper under whatever name it
already has and call it below — the rewrite is about the keyed half only:

```rust
fn options(provider: &str, config: &Config) -> Vec<ProviderOption> {
    if provider == antigravity::PROVIDER_ID {
        // The existing helper, unchanged and still named whatever it is named today.
        return vec![antigravity_source_option(config)];
    }
    let Some(spec) = keyed::CATALOG.iter().find(|spec| spec.id == provider) else {
        return Vec::new();
    };
    spec.options
        .iter()
        .map(|schema| ProviderOption {
            name: schema.name.to_owned(),
            title: schema.title.to_owned(),
            description: schema.description.map(str::to_owned),
            value: config
                .option(provider, schema.name)
                .unwrap_or(schema.default)
                .to_owned(),
            choices: schema
                .choices
                .iter()
                .map(|(value, label)| OptionChoice {
                    value: (*value).to_owned(),
                    label: (*label).to_owned(),
                })
                .collect(),
        })
        .collect()
}
```

A free-text option — an empty `choices` — publishes an empty `choices` vector. `ProviderOption`
documents `choices` as never empty; update that doc comment to say that an empty list means
free text, since a base URL has no menu to offer.

Rewrite the keyed half of `account`:

```rust
    let account = match provider {
        antigravity::PROVIDER_ID => Some(antigravity_account(secrets, config)?),
        "claude" => Some(claude_account(secrets)?),
        codex::PROVIDER_ID => Some(codex_account(secrets)?),
        other => keyed::CATALOG
            .iter()
            .find(|spec| spec.id == other)
            .map(|spec| keyed_account(spec)),
    };
```

and add the one builder that replaces `kimi_account` and `zai_account`:

```rust
/// Every key-authenticated account is built the same way: the engine hands over the stored
/// key and the account's settings, and the spec says what to do with them.
fn keyed_account(spec: &'static keyed::Spec) -> Account {
    Account::new(
        ProviderId::new(spec.id),
        AccountId::default(),
        Box::new(move |credential, options| {
            // The URL is resolved at build time, which is why storing a key or changing a
            // setting drops the client: either may change which host this account talks to.
            Ok(Arc::new(keyed::Keyed::new(spec, credential, options)?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint(spec.credential_hint)
}
```

Delete `kimi_account`, `zai_account`, `ZAI_REGION`, `ZAI_GLOBAL`, `ZAI_BIGMODEL_CN` and the
`region` helper. Update the module doc comment: registration is now "a spec in
`keyed::CATALOG`", and only OAuth providers are registered by hand.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemarkd && cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify against a running daemon**

Stop the installed unit, rebuild with `makepkg -sif`, start it, and confirm over the bus
that the catalog is unchanged for a client:

```bash
busctl --user call dev.tidemark.Daemon1 /dev/tidemark/Daemon1 dev.tidemark.Daemon1 ListProviders
```

Expected: five providers, Antigravity, Claude and Codex first, then Kimi and Z.ai, with
Z.ai still carrying its two region choices and the current value from `config.toml`.

- [ ] **Step 6: Full verification and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemarkd/src/registry.rs crates/tidemark-types/src/wire.rs
git commit -m "Build the provider catalog from the keyed table"
```

---

## Done When

- `cargo test --workspace` passes, and Kimi's and Z.ai's parse assertions are the same
  assertions as before the migration.
- `providers/zai.rs` and `providers/kimi.rs` contain a `SPEC` and a `parse` and no HTTP.
- `registry.rs` names no key-authenticated provider anywhere.
- A card whose dominant window carries absolutes shows them under the bar; one that does
  not is unchanged in height.
- Adding a provider is demonstrably one new file plus one line in `keyed::CATALOG`, which
  the ports plan then does twenty-six times.
