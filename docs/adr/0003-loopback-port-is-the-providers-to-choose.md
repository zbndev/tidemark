# ADR 0003 — The loopback callback port belongs to the provider, not to us

- Status: accepted
- Date: 2026-08-21
- Amends: ADR 0002

## Context

ADR 0002 decided that OAuth happens in the system browser with a loopback callback, and
listed four properties the listener must have: **a random free port**, exactly one request,
a validated `state`, and immediate shutdown.

Three of those survived contact with the implementation. The random port did not.

Tidemark signs in with the desktop clients the providers already publish — Claude Code's
`9d1c250a-…` and the Codex CLI's `app_EMoamEEZ73f0CkXaXp7hrann`. An authorization server
matches the `redirect_uri` of a request against what the client was registered with, and
both of these are registered with one fixed loopback address:

| Provider | Registered redirect URI |
|---|---|
| Claude | `http://localhost:54545/callback` |
| Codex | `http://localhost:1455/auth/callback` |

Sending anything else is refused before a consent screen is ever drawn. The choice is not
between a random port and a fixed one; it is between a fixed port and no login.

Not every provider is like this. Google registers *installed application* clients, for
which the loopback redirect may use any port — which is why CodexBar's Antigravity login
picks an ephemeral one and works.

## Decision

The callback port is a property of the provider's OAuth client, declared alongside its
authorize URL and its scopes. `Client::redirect_port` is the port the client is registered
with, or **zero to take any free port** where the provider allows it.

The rest of ADR 0002 stands unchanged and is unaffected: the system browser opens the URL,
no browser engine is linked in, `state` is validated before the code is looked at, and the
listener is dropped as soon as the callback has been answered.

## Consequences

- **A busy port is a real failure mode.** The vendor's own CLI runs its login on the same
  port, so `claude auth login` and a Tidemark sign-in cannot be in progress at once. The
  bind happens before the URL is built, so this is reported immediately and by name rather
  than after the user has approved something we cannot receive.
- **A login holds a well-known port for as long as it waits.** Bounded by the five-minute
  browser timeout, and released early by `CancelLogin`.
- The port is not a secret and was never doing security work. `state` and PKCE are; both
  are unchanged.
- A future provider that registers a wildcard loopback redirect gets an ephemeral port by
  setting the port to zero, with no other change.
