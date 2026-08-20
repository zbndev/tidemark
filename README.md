# Tidemark

Track AI provider quota limits on Linux — how much of each rate-limit window you have
burned, when it resets, and whether your current pace will get you there.

Native GTK4 + libadwaita. No web UI, no Electron, no embedded browser engine.

> **Status: early.** The daemon polls Z.ai, keeps history and publishes on D-Bus, and the
> window shows it: a grid of provider cards that updates on its own. The other four
> providers are not written yet.

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

## Installing on Arch

`PKGBUILD` in this directory builds a release package **from the working tree**, so it can
be reinstalled after any change rather than only at a release:

```sh
makepkg -sif    # -f: the version does not change between builds, so force the rebuild
systemctl --user enable --now tidemarkd.service
```

It installs `tidemark`, `tidemarkd`, the systemd user unit and the provider marks. `options=(!lto)` is
required, not preferred: rustls's default provider vendors C and assembly through
`aws-lc-sys`, and objects built with GCC's LTO cannot be linked by `rust-lld`.

## Running the daemon

`tidemarkd` polls, writes history to `$XDG_DATA_HOME/tidemark/history.db`, and publishes
everything it knows on the session bus. It needs a key in the Secret Service to have
anything to poll — until then it reports `no-credential`, which is a state, not an error:

```sh
secret-tool store --label='Tidemark: zai (default)' \
    xdg:schema io.github.zbndev.Tidemark.ProviderKey provider zai account default
```

The same command stores the Kimi For Coding key with `kimi` in place of `zai`. Claude and
Codex hold no key here at all: they read the credential files their own CLIs own.
Antigravity holds none either — it asks the local server the `agy` CLI runs, which keeps
its own session in the keyring. Tidemark starts that server if it is not already up, reuses
one that is, and stops the one it started when it exits.

The interface is usable with `busctl` alone, which is how it is meant to be checked:

```sh
cargo run -p tidemarkd   # or: systemctl --user start tidemarkd.service
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

## The window

`tidemark` is a viewer: it shows what `tidemarkd` publishes and never talks to a provider
itself. Start the daemon first, or leave the window open — it waits for the daemon to
appear on the bus, and picks up again by itself when the daemon is restarted.

```sh
tidemark                 # or: cargo run -p tidemark
```

Each account is a card: the provider's own mark and name, the shortest window it reported
as a large number over a bar, the
remaining windows as thin rows, and a pace mark on the bar showing how much of the window
has elapsed — fill to the left of the mark finishes before the window resets, fill to the right
does not. A window the provider did not report is not drawn, and a bar with no mark means
the provider gave no reset time; neither is an error.

The provider marks are symbolic icons installed into `hicolor`, which is how they take the
theme's colour instead of the colours in the files. Running uninstalled, point GTK at the
source tree — the layout under `data/icons` is the installed one:

```sh
XDG_DATA_DIRS="$PWD/data:$XDG_DATA_DIRS" cargo run -p tidemark
```

They are their owners' trademarks and are not covered by this project's licence; see
[`docs/TRADEMARKS.md`](docs/TRADEMARKS.md), which also says what to drop to build without
them. A card with no mark is a state the interface already has.

Looking at more than one card without configuring more than one account:

```sh
systemctl --user stop tidemarkd
cargo run -p tidemark --example mock-daemon
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

MIT — except the provider marks under `data/icons`, which are their owners' trademarks.
See [`docs/TRADEMARKS.md`](docs/TRADEMARKS.md).
