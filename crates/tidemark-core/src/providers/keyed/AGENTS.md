# KEYED PROVIDERS KNOWLEDGE BASE

## OVERVIEW
Single-request catalog plus custom key/session/gateway providers; score 14 across 57 Rust files, distinct transport domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Descriptor and catalog | `mod.rs` | `Spec`, `HandSpec`, `CATALOG`, `Keyed` |
| Shared request machinery | `mod.rs` | `request`, inspected requests, validation, query redaction |
| Browser/pasted-session selection | `session.rs` | Source settings, candidate inspection, credential lookup |
| CLI refresh reference | `gemini.rs`, `grok.rs` | Vendor-file OAuth handling |
| Cursor app/browser sources | `cursor.rs` | App database, explicit source options, proof requests |
| Paginated usage | `openai-api.rs`, `poe.rs` | Cursor walks and balance/history fallbacks |
| Heterogeneous quota payloads | `chutes.rs`, `synthetic.rs`, `kilo.rs` | Classification, deduplication, pool identity |
| Self-hosted gateway options | `litellm.rs`, `sub2api.rs`, `wayfinder.rs` | URL validation and account scoping |
| Browser-emulating requests | `t3chat.rs` | Special HTTP stack; unavailable on Windows |
| External registration consumer | `crates/tidemarkd/src/registry.rs` (repo-relative) | Hand-written table and catalog consumption |

## CONVENTIONS
- Use `Spec` for one request with a pure `parse(body, timestamp, account)` function.
- Use `HandSpec` plus `impl Provider` for multi-request flows, derived auth, or value-validating options.
- `Spec` is not deprecated: the two descriptors model different fetch contracts.
- Add simple descriptors to `CATALOG`; custom descriptors register in the daemon's hand-written table.
- `OptionSchema` publishes settings metadata; export option-name constants consumed by registry/builders.
- Use shared request helpers for status mapping, retry metadata, and query-safe transport errors.
- `base_url` refuses remote plain HTTP but permits its documented loopback forms.
- `openai-api.rs` deliberately keeps the storage slug; `#[path]` exposes module `openai_api`.
- Browser-backed descriptors use external credentials and shared `session` source options where applicable.
- Each parser's units come from its upstream contract: cents, micro-dollars, tokens, and credits are not interchangeable.
- Most parsers and loopback transport suites live inline; recorded payloads also live under `../../../tests/fixtures/`.
- Provider module docs describe endpoint provenance and known fixture gaps; do not infer live coverage from LOC.

## ANTI-PATTERNS
- Do not force derived headers or invalid-value checks into `Spec::endpoint`, which cannot return an error.
- Do not replace an explicitly chosen browser/profile/session after it fails.
- Do not copy Cursor's app-owned session into Tidemark's credential store.
- Do not invent reset instants or window lengths when the payload cannot establish them.
- Do not convert exact monetary representations through `f64` merely to format them.
- Do not treat `session.rs` as a provider: it has source helpers, not its own descriptor.

## COMMANDS
```bash
cargo test -p tidemark-core --lib providers::keyed
cargo test -p tidemark-core --lib providers::keyed::cursor
```
