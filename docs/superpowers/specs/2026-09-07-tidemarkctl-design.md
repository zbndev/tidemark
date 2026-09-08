# tidemarkctl — the command-line consumer of the daemon

**Date:** 2026-09-07
**Status:** implemented; UX revised 2026-09-08
**Branch:** `feat/cli`

## Why

`tidemarkd` publishes everything it knows on the session bus, and the interface was shaped
for a third consumer from the start: `tidemark-types/src/lib.rs`, `wire.rs` and
`tidemarkd/src/service.rs` all assumed a CLI and a Waybar module while their dictionaries
were designed. `tidemarkctl` is that consumer; scripts and panel plugins no longer need to
generate a D-Bus proxy.

Two things follow from shipping the CLI:

- **Panel plugins stop being our work.** CodexBar has no Linux GUI, and Linux got Waybar,
  three Plasma widgets, a GNOME extension, a Cinnamon applet, Noctalia, SketchyBar and a
  Stream Deck integration anyway — every one of them built on its bundled CLI. The
  D-Bus interface alone did not produce that, because a panel author will not generate a
  proxy to draw one number.
- **Scripts and CI get a pre-flight gate.** `tidemarkctl guard --min-remaining 20 --window
  weekly` answers "may I start a long run" with an exit code, which is the only form a
  shell can act on.

The user's stated goal is stronger than read-only: everything the GUI can do must be
reachable from `tidemarkctl`, because plugins for Noctalia and Caelestia are planned and
they must be able to add a provider, store a key and sign in without the window.

## What it is

One new binary, `tidemarkctl`, and one new library crate holding the contract it shares
with the window.

### Crates

| crate | holds | depends on |
|---|---|---|
| `tidemark-ipc` | the single `#[zbus::proxy] trait Daemon` | `tidemark-types`, `zbus` |
| `tidemark-cli` | `tidemarkctl`: argument grammar, formatters, command bodies | `tidemark-ipc`, `tidemark-types`, `zbus`, `clap`, `clap_complete`, `serde_json`, `futures-lite` |

The proxy **moves** out of `crates/tidemark/src/bus.rs` (lines 38-186 as of `2c19a91`)
rather than being copied. It already covers the whole interface — 33 methods, the
`Version` property and seven signals — so a copy would be a second definition of the
contract, drifting silently and failing at runtime on a user's machine instead of at
build time on ours. This is the same argument that put the wire vocabulary in its own
crate: a rule the build enforces beats a rule a hurry can skip.

What stays in `crates/tidemark/src/bus.rs`: `Update`, `watch` (GLib-local), `serve`, the
`cfg(windows)` `reconnect` module and their tests. The GUI imports
`tidemark_ipc::DaemonProxy` and is otherwise untouched.

No tokio. The GUI reaches the daemon on zbus 5's async-io backend, which drives its own
connection thread, and the CLI blocks on futures with `futures-lite`. `scripts/check-layering.sh`
gains two `forbid` entries — `tidemark-ipc` and `tidemark-cli` must not depend on
`tidemark-core`, `reqwest`, `hyper`, `rusqlite`, `libsqlite3-sys`, `gtk4`, `gtk4-sys`,
`libadwaita` **or `tokio`** — so a second runtime cannot arrive unnoticed in a tool whose
whole value is starting fast.

### Command surface

Grouped by entity, because a flat list of thirty verbs is not a `--help` anybody reads.
`P` is a provider slug, `A` an account id.

```
usage    [--provider P] [--account A] [--format text|json|waybar]
guard    --min-remaining N [--window KEY | --any] [--provider P] [--account A]
watch    [--provider P] [--format json|waybar]
refresh  [P]
provider catalog | list | add P | rm P [--account A] | order P...
account  add P A | rm P [--account A] | rename P NEW [--account A] | order P A...
auth     set-key P [--account A] [--key-file PATH]
         set-session P [--account A] [--key-file PATH]
         sign-out P [--account A] | login P [--account A]
         cancel-login P [--account A] | sources P [--account A]
         select P [--account A] --mode M [--candidate ID]
option   P NAME VALUE [--account A]
notify   P WINDOW on|off [--account A]
config   show | proxy MODE [HOST PORT] | refresh auto|manual [--minutes N]
         retention R | theme T | startup M | release-check on|off | minimize-on-close on|off
history  segment P WINDOW [--account A] | clear
data
update
version
completions bash|zsh|fish
```

On commands that target one existing account, `--account` defaults to `default`. It stays
required where it is the value being created (`account add`) or part of a complete
permutation (`account order`). `usage`, `guard` and `watch` are collection commands:
omitting their account filter continues to select every matching account for panel and
script integrations.

Adding Claude, Codex or Antigravity stores the explicit `oauth` source before the daemon's
first credential probe. `provider add codex` therefore creates a `no-credential` account
until `auth login codex` succeeds; it never silently reads `~/.codex/auth.json`.
`auth select codex --mode cli` opts into that file. Existing configurations with no stored
source retain the legacy `Auto` behavior and are not migrated.

`RequestActivate` is deliberately absent: it is the second-instance path that asks a
running window to come forward, and a CLI has no window to raise.

`provider catalog` is `ListProviders` — every provider this build knows how to configure —
and `provider list` is the configured subset from `GetStatus`. The daemon keeps those as
two concepts and so does the CLI.

### Output formats

- **`text`** — for a person. Rendered through `tidemark_types::present::{percent, duration}`,
  so a number the CLI prints and the same number on a card can never disagree. A value the
  provider withheld prints as withheld; the CLI never substitutes a zero for a missing
  reset time, which is the rule the bar and the notifications already follow.
- **`json`** — an object, `{"accounts": [...]}`, not a bare array, so a key can be added
  later without breaking a plugin that parsed the old shape. Inside, the serde form of
  `ProviderStatus` as it already exists. **Absent stays absent:** a window with no
  `resets_at` has no such key, and no `null` stands in for it.
- **`waybar`** — `{"text", "tooltip", "class", "percentage"}`.
  - `text`: the worst window's percentage across the selected accounts.
  - `percentage`: the same number as an integer, for Waybar's own formatting.
  - `class`: **always an array**, so the shape does not change when a second class
    applies. First element is the zone — `ok`, `warning`, `danger` — from the same
    `WARNING_AT` / `DANGER_AT` constants the bar recolours at and the notifications fire
    at. `stale` is appended whenever the account's state is not `ok`, so a rate-limited
    account at 95% is `["danger", "stale"]` rather than losing its zone.
  - `tooltip`: one line per account with its dominant window and reset, Pango-escaped.
  - Class names are a public contract — users write CSS against them — and are documented
    in `README.md` and `CONTEXT.md`, not left to be discovered from the source.

### watch

NDJSON on stdout, one line per event, flushed per line.

```json
{"event":"snapshot","accounts":[…]}
{"event":"changed","account":{…}}
{"event":"removed","provider":"claude","account":"default"}
{"event":"order","providers":["claude","codex"]}
{"event":"preferences","preferences":{…}}
{"event":"data","data":{…}}
{"event":"update","version":"0.5.0"}
{"event":"waiting"}
```

The first line is always a `snapshot`, and a fresh `snapshot` follows every reconnect —
whatever happened while the daemon was away was announced to nobody, so re-reading is the
only honest recovery. `waiting` is emitted when the daemon leaves the bus, which is what
makes a plugin able to dim rather than keep showing numbers from before an upgrade.
Reconnect waits for the bus name's owner exactly as `bus.rs::serve` does; there is no
polling. `--format waybar` prints one Waybar object per event instead, which is what a
`"return-type": "json"` custom module consumes continuously.

`SIGINT` and `SIGTERM` exit 0.

### Exit codes

`guard`, whose codes are its whole output:

| code | meaning |
|---|---|
| 0 | at or above the threshold — safe to start |
| 1 | below the threshold |
| 64 | invalid arguments (`EX_USAGE`) |
| 69 | the window is unavailable or the account has no reading (`EX_UNAVAILABLE`) |

Every other command: `0` success, `64` invalid arguments, `69` the daemon could not be
reached, `70` the daemon returned an error (its message goes to stderr). The daemon is
reached through D-Bus activation; the CLI never inspects `systemctl` to decide whether it
is running.

### Secrets

`auth set-key` and `auth set-session` accept a value from **stdin** (one trailing newline
trimmed) or from `--key-file PATH`. Never from argv, under any flag: `/proc/<pid>/cmdline`
is readable by every process of the same user, and a key typed once ends up in shell
history.

stdin is a file descriptor, not a terminal, so this does not force a console on anybody:
a plugin spawns the process with a pipe and writes the key into it — `Process { stdinEnabled: true }`
in Quickshell, `QProcess::write`, `subprocess.run(input=…)`. `--key-file` exists for
frameworks that can hand over a path but not a pipe; the path is in argv, the secret is
not.

Nothing is ever printed back. `auth sources` shows candidate titles and readiness states,
which is all the daemon publishes — cookie values, tokens and database paths never cross
the bus.

`auth login` prints the authorize URL on the first line of stdout and blocks until the
callback completes, so a plugin reads the line, opens it however its platform opens URLs,
and shows a spinner until the exit code arrives. The CLI never launches a browser itself,
matching the daemon's own reason for splitting a login in two: the process that holds the
credential may have started before there was a display.

## Testing

- **Pure functions.** The three formatters over `Vec<ProviderStatus>` fixtures, including
  windows with no length and no reset time — the case that proves nothing is invented — and
  a golden JSON fixture pinning the published shape, because that shape is what plugins
  parse. The `guard` decision (threshold, window selection, `--any`, unavailability) and
  its mapping to exit codes.
- **Grammar.** `clap` parse tests for each group, including that a key cannot be passed on
  argv.
- **Integration.** Command bodies take `&DaemonProxy<'_>`, so a test stands up a fake
  daemon on a private session bus — the pattern `tidemarkd/src/service.rs` tests already
  use — and drives real commands against it. No environment hook exists to redirect the
  bus name; testability comes from the parameter, not from a back door.
- **Smoke, by hand.** `tidemarkctl usage --format text`, `--format json` and
  `--format waybar` against the live daemon on the development machine, plus `watch` in one
  terminal while `refresh` runs in another.

## Packaging and documentation

The third binary is added in four places: `[package.metadata.deb] assets` and
`[package.metadata.generate-rpm] assets` in `crates/tidemark/Cargo.toml`, `PKGBUILD`, and
`nix/package.nix`'s `postInstall`. `completions` output is installed for bash, zsh and fish
by the same four.

Documentation: a CLI section in `README.md` with a copy-pasteable Waybar module, a
`CONTEXT.md` subsection under Architecture covering the format contract, the class names
and the exit codes, and `AGENTS.md` for both new crates.

## Non-goals

- **No Windows build of the CLI.** There the daemon serves zbus p2p over AF_UNIX and the
  endpoint discovery lives in `bus.rs`'s `cfg(windows)` `reconnect` module; reusing it is
  separate work. The NSIS installer does not gain the binary, and the docs say so. The
  Windows build must keep working exactly as it does — the proxy move is the only change
  that touches its code path.
- **No HTTP server.** `codexbar serve` exists because a client may be unable to read a
  child process's stdout; `watch` covers the plugins we know we are writing, and a
  listening socket brings an authorization question this project currently does not have.
- **No cost or spend output.** There is no cost-history surface in Tidemark to print.
- **No localization.** The CLI is English, like the rest of the repository's text.
