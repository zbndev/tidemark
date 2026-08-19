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

## Verification

Run end to end against the live account on 2026-08-19, after this ADR was written.

**The refresh token rotates.** `POST https://platform.claude.com/v1/oauth/token` with
`grant_type=refresh_token` returned a *different* refresh token from the one sent. Option
(c) above — keeping a refreshed token private — would therefore have silently destroyed the
user's Claude Code login, exactly as predicted. It is not merely inadvisable; it is
destructive.

The response is richer than the reference implementation models: alongside `access_token`,
`refresh_token`, `expires_in` (28800 s, 8 hours) and `token_type` it carries `account`,
`organization`, `scope`, `token_uuid`, and **`refresh_token_expires_in`**. That last field
must be written to `refreshTokenExpiresAt`; the first implementation of this experiment
left the stored value stale.

**Requests must identify themselves.** The endpoint sits behind Cloudflare, which answered
an unset user agent with `403 error_code 1010 browser_signature_banned` and the advice not
to retry. Setting `User-Agent: Tidemark/<version>` produced `200` on the first attempt.
Identify honestly by product name; do not impersonate a browser. The reference
implementation does the same, reserving browser-like agents for dashboard scraping we do
not do.

**The acceptance criterion held.** After writing the rotated token back through the staged
`0600` / `fsync` / `rename(2)` path, `~/.claude/.credentials.json` kept its seven unrelated
`mcpOAuth` entries and its mode, the new access token authenticated against
`https://api.anthropic.com/api/oauth/usage`, and `claude -p` still answered with exit 0 —
and did not overwrite what we had written.

## Consequences

- The daemon writes to files owned by other programs. This is the sharpest edge in the
  project and must be covered by tests against the real file shapes.
- Anthropic **does** rotate refresh tokens, so a crash between receiving a new one and
  publishing it logs the user out of their editor, and restoring a backup does not help —
  the backed-up token is already dead. `rename(2)` narrows this window to effectively
  nothing but does not close it. Back up before the exchange anyway; it is free.
- Third-party files are never relocated, never reformatted, and never written to when
  discovered outside their canonical path.
