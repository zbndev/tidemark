# ADR 0002 — OAuth through the system browser, never an embedded webview

- Status: accepted
- Date: 2026-08-19

## Context

Three of the five v1 providers authenticate over OAuth: Claude, Codex, and Antigravity.
The other two are API-key only — Z.ai/GLM is token-based with no cookie path at all, and
Kimi offers an API key, CLI credential reuse, and cookie import, but no OAuth. So "OAuth
for every provider" is not achievable, and for those two the interface is a key field.

For the three that do use OAuth, the login flow has to happen somewhere. The two real
options are an embedded WebKitGTK view inside the app, or the system browser with a
loopback callback.

An embedded view is tempting because the user never leaves the window, and there is
precedent: the abandoned Linux fork of CodexBar on this machine linked `webkitgtk-6.0`
for exactly this.

## Decision

Open the authorize URL in the system browser via `xdg-open`, bind a temporary HTTP
listener on `127.0.0.1`, and receive the redirect there. No browser engine is linked into
Tidemark.

The founding constraint of this project is "no web" — nothing Electron-shaped. An embedded
WebKitGTK view is literally a browser engine inside the application; it is not Electron by
letter but it is by spirit, and adopting it quietly would be a violation by technicality.
It also costs a large dependency with a steady stream of CVEs, in a package we intend to
ship to `deb`, `rpm`, and the AUR.

The loopback flow is also the proven path: `claude` and `codex` authenticate this way
themselves.

## Consequences

- Login briefly leaves the app. Acceptable; it happens once per provider.
- The one thing an embedded view would genuinely enable — harvesting session cookies from
  a provider's web dashboard — stays impossible. That is consistent with the decision to
  support no cookie-authenticated providers at all.
- Providers that offer no OAuth get an API key field, and the UI must present that as a
  normal path rather than a degraded one.
- The loopback listener must bind to a random free port, accept exactly one request,
  validate the `state` parameter, and shut down immediately afterwards.
