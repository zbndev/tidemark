# Keyed Provider Port Design

- Status: approved
- Date: 2026-08-21
- Implements: technical debt closed ahead of the remaining implementation steps

## Purpose

Tidemark supports five providers because those are the five whose subscriptions exist on
this machine and could be checked against live responses. CodexBar supports sixty-seven.
The five existing modules were each written as a bespoke client of six hundred to two
thousand lines, which is affordable at five and impossible at thirty.

This design introduces one shared mechanism for the providers whose entire authentication
is a user-supplied API key, migrates the two existing key-based providers onto it, and
ports twenty-six more from CodexBar without a live account to check any of them against.

All documentation, source code, code comments, tests, logs, and interface copy are
written in English.

## Goals

- Add `providers::keyed`: one client type, one `Spec` per provider, transport written once.
- Migrate Kimi and Z.ai onto it. They are the only key-based providers whose live
  responses have been observed, so they are the mechanism's proof rather than its last
  customers.
- Port twenty-six API-key providers from CodexBar, each as a `Spec` plus a pure `parse`.
- Replace the hand-listed provider catalog in `tidemarkd::registry` with a table, so that
  adding a provider is one file and one line.
- Give `Window` a place to carry provider-formatted absolutes, so a fixed balance can be
  drawn as a bar with `100 / 1000 credits` written under it.
- Substitute fixture agreement with CodexBar for the live verification we cannot perform.

## Non-goals

- OAuth providers. They reuse this machinery in a later pass, on their own risk budget.
- Cookie-backed providers and providers that read another CLI's configuration file.
- Alibaba and Copilot. They carry an API-key adapter but not an API-key shape: Alibaba is
  four thousand lines with a region validator and two incompatible plan forms. They are
  decided separately.
- Marking ported providers as unverified in the interface. Decided against: a user who
  sees a wrong number opens an issue.
- Any change to how credentials are stored. These providers use the existing
  `CredentialKind::Key` path into the Secret Service unchanged.

## Chosen Architecture

### `providers::keyed`

A new module `crates/tidemark-core/src/providers/keyed.rs` holds the transport that all
one-request key-based providers share, and a description of the ways they differ:

```rust
/// How the key goes on the wire.
pub enum Auth {
    Bearer,                   // Authorization: Bearer <key>
    Header(&'static str),     // x-api-key: <key>, api-key: <key>
    Query(&'static str),      // ?key=<key>
}

/// Everything a one-request key-based provider is.
pub struct Spec {
    pub id: &'static str,
    pub title: &'static str,
    pub endpoint: fn(&Options) -> String,
    pub auth: Auth,
    pub headers: &'static [(&'static str, &'static str)],
    pub parse: fn(&str, Timestamp) -> Result<Snapshot, ProviderError>,
    pub credential_hint: &'static str,
    pub options: &'static [OptionSchema],
}

pub struct Keyed {
    spec: &'static Spec,
    client: reqwest::Client,
    credential: Credential,
    url: String,
}
```

`endpoint` is a function rather than a constant because two kinds of provider need the
host at build time: Z.ai chooses between two regional hosts, and the self-hosted
providers — Ollama, Azure OpenAI, LiteLLM, LLMProxy — take a base URL from the user. Both
resolve from the same `Options` map the daemon already threads through `Account::rebuild`.

`Keyed::fetch` is the body of today's `Zai::fetch_inner` generalised: refuse a blank
credential before spending a request, `GET` with the key applied per `Auth`, read
`Retry-After`, `http::check` the status, then hand the body to `(spec.parse)(&body,
Timestamp::now())`. The convention `providers::mod` states in prose — transport and
meaning are separate functions — becomes a property of the type: `parse` is a plain `fn`
with no client in scope and cannot perform a request.

Each provider is a file `providers/keyed/<name>.rs` containing a `pub static SPEC: Spec`,
its `parse`, the serde structures of its response, and its tests. The expected size is
one hundred to two hundred and fifty lines against the six hundred a bespoke module costs.

A provider whose fetch is not one request keeps its own `impl Provider` and reuses
`keyed::request()` for transport and `http::check` for status mapping. `Keyed` is the
common case, not a constraint: Poe pages through a usage history and OpenRouter makes two
calls, and neither is forced into the one-request shape.

### The catalog becomes a table

`tidemarkd::registry` currently names each provider three times: a `ProviderDefinition`
stanza, an arm of `account()`, and a builder function. Twenty-six more providers would be
seventy-eight hand-written stanzas whose only variation is a string.

`keyed::CATALOG: &[&'static Spec]` replaces them. `registry::catalog` maps each spec to a
`ProviderDefinition` — `credential` is always `CredentialKind::Key`, `title`,
`credential_hint` and `options` come off the spec — and `registry::account` builds a
`Keyed` for any slug the table contains. The five hand-written providers (Claude, Codex,
Antigravity, and any keyed provider with a custom fetch) stay explicitly listed and are
appended to the table's output, preserving the existing display order at the head of the
list.

The stable-order requirement is met by the table's own order, which is source order.

### Balance as a window

`Window` gains one field:

```rust
/// Absolute quantities behind the percentage, already formatted by the adapter —
/// `100 / 1000 credits`. Drawn small under the bar.
pub subtitle: Option<String>,
```

`WindowStatus` on the wire is `a{sv}`, where an absent key already means "the provider did
not say", so the field costs no migration and an older client ignores it. The GUI card
draws it under the bar in a dim, smaller style; the detail dialog shows it beside the
window it belongs to.

A provider reporting a fixed balance — a quantity consumed against a stated limit — emits
one window: `WindowKey::named("balance")` (the `named` constructor requires a reason at
the call site, and the reason is that a balance has no length to key on), `used_percent`
computed from used over limit, `length: None`, `resets_at: None`, and `subtitle` carrying
the two absolutes in the provider's own unit. No pace mark is drawn, which is correct: a
balance has no period to be ahead of or behind.

A provider reporting a balance with no limit at all — Poe's point balance, a raw credit
figure in dollars — emits no window and only a `DetailSection`. Its card renders empty and
sorts last under the existing `model::compare`. This is accepted for this pass; a card
that draws a balance without a bar is separate work.

### Extracting the contract

For each provider the source of truth in CodexBar is taken in this order:

1. The bundled JS plugin under `Sources/CodexBarCore/Resources/Plugins/<id>.js`, where one
   exists. Ten of the twenty-six have one, and the plugin states the endpoint, the auth
   header and the whole of the response handling in one compact file.
2. The Swift fetcher and usage-stats types under `Sources/CodexBarCore/Providers/<Name>/`.
3. The recorded JSON in `Tests/CodexBarTests/<Name>*Tests.swift`, which is where CodexBar
   keeps live response bodies inline. Every one of the twenty-six has at least one such
   file.

## Provider Set

Ten with a JS plugin, read first because their contracts are cheapest to read:

ClawRouter, ClinePass, Crof, OpenAI, OpenRouter, Poe, Sub2API, Synthetic, Venice, xAI.

Sixteen Swift-backed:

AiAnd, Amp, Azure OpenAI, Chutes, DeepInfra, ElevenLabs, Factory, Fireworks, Groq, IBM
Bob, LiteLLM, LLMProxy, NeuralWatt, Ollama, Warp, ZenMux.

Plus the two migrations: Kimi and Z.ai.

Poe and OpenRouter are the known custom-fetch cases — Poe pages through a usage history,
OpenRouter makes two calls — and are ported last regardless of having a plugin. Any other
provider found to need more than one request joins them there rather than distorting
`Keyed`.

Ollama, Azure OpenAI, LiteLLM and LLMProxy are self-hosted and require a `base_url`
option; the `Spec.options` schema publishes it the same way Z.ai's region is published
today, so the interface draws the control without knowing what a base URL is.

## Error Handling

The rule in `providers::mod` holds for every ported provider and is tested per provider: an
entry of a recognised kind that cannot be parsed is a `ProviderError::Malformed` for the
whole fetch, because a silently dropped window reads as "you have no such limit". An entry
of an unrecognised kind is skipped, because that is a quota type that did not exist when
this was written.

Status mapping is `http::check` unchanged: 401, 403 and 402 are `Credential`, 429 is
`RateLimited` carrying `Retry-After` when it is in seconds form, everything else is `Http`.
Where a provider signals a rejected key in a 200 body rather than in the status, its
`parse` maps that body to `ProviderError::Credential { status: 401 }`, so that the
interface asks for a new key instead of reporting an unreadable response.

## Test Strategy

Per provider, three tests at minimum:

- **Fixture agreement.** `parse` turns a response body recorded in CodexBar's own tests
  into the windows and details CodexBar's tests assert for it. This is the honest
  substitute for a live account: it does not verify the API, it verifies that we agree
  with an implementation that was verified against the API.
- **Malformed body.** A truncated or type-shifted body is a `Malformed`, not a panic and
  not an empty snapshot.
- **The unknown-kind rule.** An unrecognised entry is skipped and a recognised but
  unparseable entry fails the fetch.

Mechanism-level tests in `keyed.rs` cover each `Auth` variant putting the key in the right
place, a blank credential being refused before a request is made, and `endpoint` resolving
options.

Kimi's and Z.ai's existing tests move with them unchanged and are the migration's
acceptance criterion: the mechanism is correct when the two providers whose live responses
were observed still pass every assertion written against those responses.

`registry` tests cover the table producing one `ProviderDefinition` per spec, the
hand-written providers keeping their position at the head of the catalog, and an unknown
slug in `config.toml` still being warned about rather than failing the daemon.

## Sequencing

1. `keyed` mechanism; migrate Kimi and Z.ai onto it.
2. `Window.subtitle` through the wire, the card and the detail dialog.
3. Table-driven catalog in `registry`.
4. The JS-plugin providers that fetch in one request: ClawRouter, ClinePass, Crof,
   OpenAI, Sub2API, Synthetic, Venice, xAI.
5. The sixteen Swift-backed providers.
6. The custom-fetch remainder: Poe, OpenRouter, and anything demoted here from steps 4
   and 5 on discovering it needs more than one request.
