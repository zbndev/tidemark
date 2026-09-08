# COMMAND-LINE CLIENT KNOWLEDGE BASE

## OVERVIEW
`tidemarkctl`: the third consumer of the daemon's interface, after the window and `busctl`. It prints what the daemon published — no provider I/O, no database, no display, no second async runtime — and its `json`, `waybar` and exit-code shapes are a published contract that panels and scripts parse.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Argument grammar, subcommand names | `src/cli.rs` | Grouped by entity (`provider`, `account`, `auth`, `config`, `history`), not flattened verbs; `Switch` is `on`/`off` |
| Dispatch and process exit | `src/main.rs` | One `async_io::block_on`; every body lives in the library beside it |
| Exit codes | `src/exit.rs` | `Exit`, `Failure`, and the `zbus::Error` split between "refused" and "not there" |
| A daemon method's command body | `src/commands/{provider,account,auth,config}.rs` | Each takes the proxy; a fake daemon can be passed in |
| Output shapes | `src/format/{text,json,waybar}.rs`, `mod.rs` | `payload` peels the `a{sv}` envelope; `select` and `dominant` are shared |
| The event stream | `src/watch/mod.rs`, `watch/mirror.rs` | NDJSON events, reconnect, and the client-side mirror Waybar re-renders from |
| `guard`'s verdict | `src/guard.rs` | `Selection`, `Verdict`, and why only `ok` accounts are judged |
| Secret input | `src/secret.rs` | stdin or `--key-file`, one trailing newline trimmed |
| Provider spelling | `src/titles.rs` | Catalog title first, `provider_label` as the fallback |
| The one connection | `src/connect.rs` | Session bus, D-Bus activation, no `systemctl` inspection |

## CONVENTIONS
- The library holds every command body and the binary is parsing plus an exit code, so `tests/` drives the real bodies against `tests/fake/` — a served daemon, not the machine's.
- Command functions take `&DaemonProxy`; nothing but `connect::daemon` builds one, and no environment variable redirects the bus name.
- `text` is for a person and may be reworded. `json` is `{"accounts": [...]}`; `waybar` is `{text, tooltip, class, percentage}` with `class` always an array (zone `ok`/`warning`/`danger` at `WARNING_AT`/`DANGER_AT`, then `stale`).
- Percentages, spans and window titles come from `tidemark_types::present` and the daemon's own text, so a number here cannot drift from the same number on a card.
- Every published value passes through `format::payload`: `SerializeDict` renders `{"signature", "value"}` under `serde_json`, and events and snapshots must peel identically.
- `watch` opens with a `snapshot`, prints `waiting` when the daemon leaves the bus, and re-reads a fresh `snapshot` after every reconnect; lines are flushed one at a time because a pipe is block-buffered.
- Account order is the daemon's published order — the user's — and is never re-sorted here.
- Commands targeting one existing account expose `--account` with `default` as the
  default. `usage`, `guard` and `watch` keep omission as an all-account filter; `account
  add` and `account order` keep explicit ids because those ids are the operation's value.
- `auth select --mode oauth|cli` chooses the static source of a dual-source provider;
  dynamic browser/profile choices still use the mode and candidate ids from `auth sources`.
- `zbus` without `p2p`: the CLI is a session-bus client. Windows compiles the crate but packages nothing.

## ANTI-PATTERNS
- Do not add an argv slot for a secret; `/proc/<pid>/cmdline` is readable by every process of the same user. stdin or `--key-file` only.
- Do not format a percentage, a duration or an icon name outside `tidemark_types::present`.
- Do not invent a value the daemon did not send: no `null`, no zero, no "0%" placeholder where a provider withheld a reading. An absent key stays absent.
- Do not rename or remove a key in `json`/`waybar` or renumber an exit code — add keys instead. A panel widget parses these.
- Do not let `guard` judge a non-`ok` account's last-good reading: exit `69`, never a "safe" answer about quota nobody can spend.
- Do not add provider I/O, storage, a display dependency, or `tokio`; `scripts/check-layering.sh` forbids `tidemark-core`, `reqwest`, `hyper`, `rusqlite`, `libsqlite3-sys`, `gtk4`, `gtk4-sys`, `libadwaita` and `tokio`.
- Do not re-derive which window matters: `format::dominant` calls `Snapshot::dominant_window`, the same rule the card and the tray use.

## CHECKS
- `cargo test -p tidemark-cli` covers the grammar, formats, `guard` verdicts and the mirror; the command integration suites serve a fake daemon on the session bus, while completion generation needs none.
- Exit codes are observable: `0` ok, `1` below threshold, `64` an argument this build cannot act on, `69` unreachable or nothing to judge, `70` the daemon refused, `2` from clap's own parse failure.
- `tidemarkctl completions <shell>` is generated from the same `clap` command; `tests/completions.rs` pins that the shipped shells generate.
