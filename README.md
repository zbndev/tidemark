# Tidemark

Track AI provider quota limits on Linux — how much of each rate-limit window you have
burned, when it resets, and whether your current pace will get you there.

Native GTK4 + libadwaita. No web UI, no Electron, no embedded browser engine.

> **Status: early.** The daemon polls Z.ai, keeps history and publishes on D-Bus; the
> window is still empty, and the other four providers are not written yet.

## Planned for v1

Claude, Codex, Z.ai/GLM, Kimi For Coding, Antigravity. Each reports its own set of
rate-limit windows — five-hour, weekly, monthly, whatever the provider exposes — with
reset times and a pace mark showing whether the current burn rate reaches the reset.

A background daemon polls, keeps history, and sends threshold notifications; the window is
a viewer over D-Bus.

## Building

Needs Rust 1.92 or newer and the development packages for GTK 4.22+, libadwaita 1.9+ and
SQLite. The toolkit floor tracks current releases on purpose.

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
./scripts/check-layering.sh
```

The last one asserts the crate layering described in `CONTEXT.md`: the interface crate
must not reach the network or the database. Run it whenever you add a dependency.

There is a debug entry point for looking at one provider's live answer. It reads the key
from standard input rather than an argument, because arguments are visible in `ps`:

```sh
pass my/zai | cargo run -p tidemark-core --example probe
```

## Running the daemon

`tidemarkd` polls, writes history to `$XDG_DATA_HOME/tidemark/history.db`, and publishes
everything it knows on the session bus. It needs a key in the Secret Service to have
anything to poll — until then it reports `no-credential`, which is a state, not an error:

```sh
secret-tool store --label='Tidemark: zai (default)' \
    xdg:schema io.github.zbndev.Tidemark.ProviderKey provider zai account default
```

The interface is usable with `busctl` alone, which is how it is meant to be checked:

```sh
cargo run -p tidemarkd
busctl --user introspect io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark
busctl --user call io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark \
    io.github.zbndev.Tidemark.Daemon1 GetStatus
busctl --user call io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark \
    io.github.zbndev.Tidemark.Daemon1 Refresh s ""
```

As a user service, once the binary is installed at `/usr/bin/tidemarkd`:

```sh
install -Dm644 data/tidemarkd.service ~/.config/systemd/user/tidemarkd.service
systemctl --user daemon-reload
systemctl --user enable --now tidemarkd.service
```

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
