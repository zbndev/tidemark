# ADR 0002 — OAuth through the system browser, never an embedded webview

- Status: accepted
- Date: 2026-08-19

## Context

Providers offer different authentication paths: OAuth, API keys, local credentials and
existing browser sessions. The interface exposes only the paths each provider supports.

For providers that use OAuth, the login flow has to happen somewhere. The two real
options are an embedded browser engine inside the app, or the system browser with a
loopback callback.

## Decision

Open the authorize URL in the system browser through the platform URL launcher, bind a temporary HTTP
listener on `127.0.0.1`, and receive the redirect there. No browser engine is linked into
Tidemark.

The founding constraint of this project is "no web" — nothing Electron-shaped. An embedded
browser view is literally a browser engine inside the application; it is not Electron by
letter but it is by spirit, and adopting it quietly would be a violation by technicality.
It also costs a large dependency with a steady stream of CVEs, in a package we intend to
ship to `deb`, `rpm`, and the AUR.

The loopback flow is also the proven path: `claude` and `codex` authenticate this way
themselves.

## Consequences

- Login briefly leaves the app. Acceptable; it happens once per provider.
- Tidemark does not host a web dashboard to harvest login cookies. Browser authentication
  reads an explicitly selected existing profile through the separate browser-source flow.
- Providers that offer no OAuth get an API key field, and the UI must present that as a
  normal path rather than a degraded one.
- A loopback listener binds only to localhost at the port the provider accepts (ADR 0003),
  validates the callback and `state` parameter, and shuts down when the flow finishes,
  fails or is cancelled.
