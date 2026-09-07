# PROVIDERS KNOWLEDGE BASE

## OVERVIEW
Provider trait, error semantics, CLI/OAuth clients, and shared HTTP policy; score 11, distinct integration domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Polling contract | `mod.rs` | `Provider`, `BoxFuture`, `Credential`, `ProviderError` |
| Credential-source semantics | `mod.rs` | `Source::{Auto,OAuth,Cli}` and stored spellings |
| HTTP/proxy policy | `http.rs` | Client builder, process-wide proxy, retry headers |
| Claude OAuth/CLI files | `claude.rs` | Login document, refresh, usage parsing |
| Codex OAuth/CLI files | `codex.rs` | Source selection, refresh, account enrichment |
| Local-server/direct Google quota | `antigravity/` | Own lifecycle and payload guidance |
| Key and browser-session providers | `keyed/` | Catalog and custom builders |
| Transport integration coverage | `../../tests/claude_provider.rs`, `../../tests/codex_provider.rs` | Loopback provider tests |

## CONVENTIONS
- `Provider` is object-safe through boxed `Send` futures; the daemon holds heterogeneous clients.
- Keep transport separate from pure body-to-snapshot parsing with an explicit capture timestamp.
- Preserve the selected account when building snapshots; the trait's default account is not every account.
- `inspect_auth_sources` returns credential-free candidates; providers without choices use its empty default.
- `Source::OAuth` selects owned login only; `Cli` selects vendor credentials only.
- `Auto` is provider-specific: Claude/Codex prefer owned login, Antigravity checks its local source first.
- Unknown stored source spellings currently map to `Auto`, not a configuration error.
- Shared HTTP identifies as `Tidemark/<version>` and uses 30-second request/10-second connect ceilings.
- Proxy configuration is process-wide; child commands receive proxy variables without mutating daemon environment.
- Loopback bypass is fixed; SOCKS5 mode uses `socks5h` for proxy-side DNS.
- `kimi` and `zai` are compatibility re-exports of modules under `keyed/`.

## ANTI-PATTERNS
- Never change a shipped provider slug: history and credential identities use it.
- Never silently drop an unreadable recognized window; fail the fetch as `Malformed`.
- Skip only genuinely unrecognized quota kinds, not known windows with inconvenient payloads.
- Do not map HTTP 429 to reauthentication; preserve retry/rate-limit semantics.
- Do not derive secret-printing `Debug` implementations for credential-bearing clients.
- Do not generalize credential acquisition into the thin trait: source ownership differs by provider.

## COMMANDS
```bash
cargo test -p tidemark-core --lib providers
cargo test -p tidemark-core --test claude_provider
cargo test -p tidemark-core --test codex_provider
```
