# Tidemark

Track AI provider quota limits on Linux — how much of each rate-limit window you have
burned, when it resets, and whether your current pace will get you there.

Native GTK4 + libadwaita. No web UI, no Electron, no embedded browser engine.

> **Status: design complete, no code yet.** This repository currently holds the design
> record. Nothing here builds.

## Planned for v1

Claude, Codex, Z.ai/GLM, Kimi For Coding, Antigravity. Each reports its own set of
rate-limit windows — five-hour, weekly, monthly, whatever the provider exposes — with
reset times and a pace mark showing whether the current burn rate reaches the reset.

A background daemon polls, keeps history, and sends threshold notifications; the window is
a viewer over D-Bus.

## Design record

- [`CONTEXT.md`](CONTEXT.md) — vocabulary, providers, architecture, storage, interface
- [`docs/adr/0001`](docs/adr/0001-refresh-and-write-third-party-credentials.md) — refreshing
  OAuth tokens and writing them back to CLI credential files
- [`docs/adr/0002`](docs/adr/0002-no-embedded-webview.md) — OAuth through the system browser

## Prior art

Provider protocol details — endpoints, field names, fallback order — are taken from
[CodexBar](https://github.com/steipete/CodexBar) (MIT). Its interface is a macOS menu-bar
popup and is not a model for this one.

## License

MIT
