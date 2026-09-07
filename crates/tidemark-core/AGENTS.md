# CORE KNOWLEDGE BASE

## OVERVIEW
External I/O library: provider clients, configuration, credentials, and history; score 12, distinct crate boundary.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Public module surface | `src/lib.rs` | Consumed by the daemon, not the GUI |
| Provider implementations | `src/providers/` | Separate provider guidance below |
| Browser credential discovery | `src/browser/` | Read-only vendor data boundary |
| History and segmentation | `src/storage/` | SQLite ingest and migrations |
| Preferences/accounts | `src/config.rs` | Comment-preserving `toml_edit` mutations |
| Application paths | `src/paths.rs` | Absolute XDG/HOME or Windows local-data roots |
| Interactive login | `src/oauth.rs` | Loopback callback, state, PKCE, token exchange |
| Vendor credential updates | `src/oauth_file.rs` | Field-preserving conditional publication |
| Owned credentials | `src/secrets.rs`, `src/secrets/` | Key/token/session slots and platform backends |
| Raw response diagnostics | `src/debug.rs` | Opt-in NDJSON with rotation |
| End-to-end core contracts | `tests/` | OAuth files, providers, proxies, history replay |
| Live Z.ai probe | `examples/probe.rs` | Reads a key from stdin; performs network I/O |

## CONVENTIONS
- `Config` preserves comments, formatting, ordering, and unrelated TOML; absence is first run.
- Application path resolution ignores relative environment roots rather than using the current directory.
- Windows application config and history both use local app data, not roaming app data.
- `Kind::{Key,Token,Session}` separates credentials by schema, provider, and account.
- Vendor JSON refreshes preserve unrelated fields and require canonical paths and conditional updates.
- Exchange tokens before the short credential mutation lock; publish only if the source still matches.
- Platform implementations use `cfg(unix)`/`cfg(windows)`; a Linux pass does not verify Windows code.
- Debug exchanges retain response bodies verbatim; query strings and request headers are excluded.

## ANTI-PATTERNS
- Do not add a GUI dependency on this crate; `src/lib.rs` explicitly forbids that link.
- Do not prompt an unattended daemon to unlock Secret Service; return the locked state.
- Do not replace vendor OAuth JSON wholesale or bypass its lock/canonical-path checks.
- Do not send token-exchange or session-bootstrap secrets through ordinary response logging.
- Do not remove `serde_json`'s `float_roundtrip`: recorded numeric fixture fidelity depends on it.

## COMMANDS
```bash
cargo check -p tidemark-core
cargo test -p tidemark-core
cargo test -p tidemark-core --test oauth_file
cargo test -p tidemark-core --test proxy
cargo clippy -p tidemark-core --all-targets -- -D warnings
```

## NOTES
- Secret Service integration coverage requires a reachable session bus; Windows suites require Windows.
- `Cargo.toml` keeps T3 Chat's browser-emulating HTTP dependencies Unix-only.
