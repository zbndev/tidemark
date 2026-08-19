# ADR 0001 — Refresh OAuth tokens ourselves and write them back to CLI credential files

- Status: accepted
- Date: 2026-08-19

## Context

Claude and Codex quota data is only available with a subscription OAuth token. A plain API
key does not expose plan limits at all — it exposes token billing, which is a different
metric. Those tokens live in files owned by other programs: `~/.claude/.credentials.json`
(Claude Code) and `~/.codex/auth.json` (Codex CLI).

Access tokens are short-lived. When this decision was made, the local Claude access token
had already been expired for 10.5 hours while its refresh token had 27 days left. That
measurement is the whole argument: a strictly read-only daemon would have shown an error
on its very first poll, and would show one most mornings.

Four options were considered.

- **Read-only, never refresh.** Safest in principle, non-functional in practice, per above.
- **Refresh ourselves and write back to the owning file.**
- **Refresh ourselves and keep the new token private.** Rejected outright. Refresh tokens
  are typically rotated on use, so keeping the rotation would silently break the user's
  working Claude Code or Codex login. Breaking someone's editor to draw a chart is not a
  trade worth making.
- **Delegate: run the vendor CLI and let it repair its own file.** This is what CodexBar
  does for Claude, wrapped in a five-minute persistent cooldown, in-flight deduplication,
  an 8-second timeout and several gates.

## Decision

Refresh tokens ourselves and write them back to the canonical credential file, for both
Claude and Codex, using one code path.

Three safeguards are copied from CodexBar's Codex implementation, which already does
exactly this:

1. **Source gate.** Writing is permitted only to the canonical path. CodexBar models this
   as `canPersistRefresh`, true only for `codexHome` and false for legacy paths and for
   third-party copies such as OpenCode's. We do the same: never write to a file we merely
   discovered.
2. **Field-scoped merge.** Read the existing JSON, replace only the token subtree, preserve
   everything else. Neither file belongs to us and both carry unrelated data — Claude's,
   for instance, also holds MCP OAuth entries.
3. **Atomic private publish.** Write to a staged temp file opened `O_WRONLY|O_CREAT|O_EXCL`
   at mode `0600`, `fchmod(0600)` regardless of umask, `fsync`, then `rename(2)` over the
   target. The credential is never world-readable even momentarily, and a crash mid-write
   leaves the original intact rather than a truncated file.

Additionally, ours takes an advisory file lock so we do not race the vendor CLI for the
same file.

Delegation was rejected because its justification does not transfer. CodexBar delegates
for Claude specifically to avoid macOS Keychain prompts — most of the files in its Claude
OAuth directory exist to manage that. On Linux, Claude Code stores the token as plaintext
JSON and there is no Keychain, so the cost of delegation (spawning a large Node process
from a background daemon on every expiry, plus reimplementing the cooldown machinery)
buys nothing.

## Consequences

- The daemon writes to files owned by other programs. This is the sharpest edge in the
  project and must be covered by tests against the real file shapes.
- If a provider rotates refresh tokens and we crash between receiving a new one and
  publishing it, the user is logged out of their editor. `rename(2)` narrows this window to
  effectively nothing, but it does not close it.
- Third-party files are never relocated, never reformatted, and never written to when
  discovered outside their canonical path.
