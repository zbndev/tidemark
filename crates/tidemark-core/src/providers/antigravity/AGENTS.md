# ANTIGRAVITY KNOWLEDGE BASE

## OVERVIEW
Local `agy` supervision and direct OAuth quota acquisition with pool-aware parsing; score 8, distinct lifecycle domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Source selection and refresh | `mod.rs` | `Antigravity`, auto/local/direct flows |
| Local quota meaning | `mod.rs` | `logged_in`, account-aware parser, cadence table |
| Local process lifecycle | `agy.rs` | PTY spawn, socket discovery, readiness, owned/adopted state |
| Direct Cloud Code quota | `direct.rs` | Network fetch and independent payload parser |
| Login completion | `oauth.rs` | OAuth client and account login completion |
| Recorded model payload | `../../../tests/fixtures/antigravity-available-models.json` | Fixture input |

## CONVENTIONS
- `agy` and the local-source implementation compile only on Unix; direct OAuth is separate.
- Explicit CLI mode uses the local server; explicit OAuth mode uses owned login.
- Auto mode has local-first behavior; preserve its documented error/fallback distinctions.
- `GetUserStatus` establishes authentication before trusting local quota responses.
- A 200 quota response with all remaining fractions equal to one can be an unauthenticated server.
- Invert `remainingFraction` to consumption; do not display remaining as used.
- Pools of identical length need distinct `WindowKey::for_pool` identities derived from bucket IDs.
- Cadence comes from the declared window or bucket ID; unknown cadence leaves length absent.
- The local child needs a pseudoterminal to stay alive; a pipe is not equivalent.
- Recover candidate ports from the process's sockets, then probe the RPC rather than guessing ports.
- An owned server stays warm between polls; one forced relaunch bounds recovery from a wedged server.
- Reuse readiness's status body within a poll instead of fetching potentially inconsistent status twice.

## ANTI-PATTERNS
- Never signal an adopted server: another editor or daemon may own it.
- Do not describe `agy` as read-only/no-spawn: this module can spawn and tear down its own process group.
- Do not use display names for pool keys; wording changes must not split history.
- Do not add `monthly` to the fixed-seconds cadence table by inventing a month length.
- Do not infer readiness from HTTP status or a structurally valid quota body alone.

## COMMANDS
```bash
cargo test -p tidemark-core --lib providers::antigravity
```
