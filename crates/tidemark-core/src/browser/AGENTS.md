# BROWSER KNOWLEDGE BASE

## OVERVIEW
Browser profile discovery, private cookie snapshots, decryption, and candidate validation; score 11, distinct credential-read domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Browser/platform roots | `mod.rs` | `BROWSERS`, profile scan, private `storage` module |
| Request cookie scoping | `mod.rs` | `Query`, `Cookie`, `header_for` |
| Database ownership boundary | `mod.rs` | Private `Snapshot`, `copy_private_database` |
| Candidate validation | `auth.rs` | `Selection`, `Validation`, `CandidateCredential`, inspectors |
| Chromium encrypted values | `chromium.rs` | Unix cookie formats and Windows decryption |
| Gecko cookies | `gecko.rs` | Read path for Firefox-family databases |
| Platform decryption secret | `safe_storage.rs` | Re-exported `SafeStorage`/`Keyring`, Windows key handling |
| Shared fixture home | `mod.rs` test module | Crate-visible helpers used by provider tests |
| Windows integration cases | `../../tests/browser_windows.rs` | Target-specific coverage |

## CONVENTIONS
- Discovery stats directories; it does not open every cookie database on each scan.
- Browser table order, root order, and sorted profile names make discovery deterministic.
- Chromium checks `Network/Cookies` before legacy `Cookies`; Gecko uses `cookies.sqlite`.
- Read a private database copy with existing WAL/SHM sidecars, not the vendor database in place.
- Unix snapshot directories use owner-only mode through `DirBuilderExt`; no Unix chmod FFI is needed.
- Windows copying opens the source read-only with sharing compatible with a running browser.
- Snapshot cleanup runs on drop and on partial-copy failure.
- `Snapshot` is internal, not public API; callers use the store/query interfaces.
- `safe_storage` stays private while its public trait/backend are re-exported.
- Request host/path/security filtering matters: a cookie in a jar may not belong on the proof request.

## ANTI-PATTERNS
- Do not create SQLite sidecars in a browser's directory, even for an intended read-only operation.
- Do not copy only the main database: recent sessions may exist only in WAL.
- Never log cookie values or the candidate header while validating it.
- Do not turn App-Bound v20 into a panic or fabricated credential; report its supported error state.
- Do not treat a locked decryption keyring as missing credentials requiring a new login.

## COMMANDS
```bash
cargo test -p tidemark-core --lib browser
```
