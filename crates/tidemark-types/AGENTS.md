# SHARED CONTRACT KNOWLEDGE BASE

## OVERVIEW
Domain vocabulary and compatible IPC dictionaries; score 9, a distinct public-contract boundary.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Public API and installed identities | `src/lib.rs` | Re-exports, `ids`, product `user_agent()` |
| IPC payloads and preferences | `src/wire.rs` | `ProviderStatus`, definitions, auth selectors, history, options |
| Reading and account vocabulary | `src/snapshot.rs` | `Snapshot`, IDs, details, lead-window selection |
| Quota windows and pace | `src/window.rs` | `Window`, `WindowKey`, `WindowLength`, thresholds |
| Timestamp validation | `src/time.rs` | `Timestamp`, `AbsurdTimestamp` |
| Shared display primitives | `src/present.rs` | Duration, percentage, icon naming |

## CONVENTIONS
- `Snapshot` represents a provider reading; `ProviderStatus` also carries account state and its last good reading.
- An account id is the name the user typed and the card shows: `valid_account_id` allows letters and digits in any script, plus spaces, hyphens and underscores, with alphanumeric edges and case kept. Never transliterate it, never lowercase it, never assume ASCII.
- Wire structures derive dictionary serialization and encode as `a{sv}`, not positional D-Bus structs.
- Optional values are omitted dictionary keys; JSON uses the same shapes rather than a parallel schema.
- Extend payloads so older dictionaries with missing fields still decode.
- Unknown state strings decode as unknown, never healthy; clients decide how to display the unexplained state.
- `ProviderState::remedy` maps detailed daemon failures into client action groups without discarding wire distinctions.
- Authentication capabilities belong in `ProviderDefinition` and status fields, not client-side slug tables.
- Keep wire choice values and preference constants stable; validation helpers define the accepted vocabulary.
- `ProviderStatus::set_state` and reading replacement are separate operations: a failed poll keeps the last good snapshot.
- `zvariant` is permitted for encoding; opening a connection with `zbus` is outside this crate.

## ANTI-PATTERNS
- Do not replace missing reset times, lengths, or captures with sentinel zeroes to satisfy a fixed signature.
- Do not duplicate the wire model for a future JSON client.
- Do not rename `ids` constants casually: installed services, clients, and stored secret schemas depend on them.
- Do not change the numbered interface name in place for an incompatible protocol revision.
- Do not turn credential metadata into credential payloads; this vocabulary publishes kind/presence/selection, not secrets.
- Do not clear a good reading just to represent a new error state.

## CHECKS
- `cargo test -p tidemark-types` needs no GUI or daemon session.
- `src/wire.rs` tests serde/zvariant round trips, absent keys, older payloads, unknown states, and last-good-reading retention.
- A new optional wire field belongs in compatibility/round-trip coverage as well as its public re-export when needed.
- `scripts/check-layering.sh` distinguishes encoding dependencies from runtime I/O dependencies.
