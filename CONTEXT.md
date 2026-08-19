# Tidemark

A native Linux desktop app that tracks quota limits across AI providers — how much of
each rate-limit window you have burned, when it resets, and whether your current pace
will get you there.

Docs are in English because the repo is public; conversation about it happens in Russian.

## Why it exists

Menu-bar quota trackers answer "what is my number right now". A separate window can
answer the question that does not fit in a popup: **can I start a long run right now,
and on which provider?** That question is comparative (five providers side by side) and
temporal (not "how much did I spend" but "how long will it last"). Every design decision
below follows from that.

## Identity

| | |
|---|---|
| Application ID / D-Bus name | `io.github.zbndev.Tidemark` |
| Daemon bus name | `io.github.zbndev.Tidemark.Daemon` |
| Object path | `/io/github/zbndev/Tidemark` |
| GUI binary | `tidemark` |
| Daemon binary | `tidemarkd` |
| systemd user unit | `tidemarkd.service` |
| Config | `$XDG_CONFIG_HOME/tidemark/config.toml` |
| History | `$XDG_DATA_HOME/tidemark/history.db` |
| Secret Service schema | `io.github.zbndev.Tidemark.ProviderKey` |

Reverse-DNS uses `io.github.zbndev` because there is no owned domain. It is the
conventional fallback and is what desktop files, D-Bus, and Flatpak all expect to match.

## Vocabulary

- **Provider** — one AI service (Claude, Codex, Z.ai, Kimi, Antigravity).
- **Account** — one set of credentials for a provider. v1 shows exactly one per provider,
  but every key in storage carries an account id so multi-account is a UI change, not a
  migration.
- **Window** — one rate-limit period a provider reports: `{id, title, used_percent,
  resets_at, length}`. A provider returns however many it wants; the set can change
  between responses. Windows are first-class, not display strings — the pace mark and the
  burn-down forecast are computed from `length` and `resets_at`.
- **Segment** — one instance of a window between two resets. The unit that history is
  grouped by, that notifications deduplicate against, and that a forecast is computed over.
- **Snapshot** — one fetch result for a provider: a list of windows plus free-form
  `details` sections (`{title, rows: [{label, value}]}`) for anything that does not fit
  the window model, such as Kimi's absolute request counts or Codex reset credits.
- **Pace** — the fraction of the window's duration that has elapsed. Displayed as a mark
  on the bar at that position. Fill left of the mark means sustainable; fill right of it
  means the quota runs out before the reset.

## Providers in v1

All five reach their data over tokens or a local server. None requires scraping browser
cookies — a deliberate scope boundary, not a coincidence.

| Provider | Path | Credential |
|---|---|---|
| Claude | OAuth token → usage API | `~/.claude/.credentials.json` (`claudeAiOauth`) |
| Codex | `GET https://chatgpt.com/backend-api/wham/usage`, Bearer | `~/.codex/auth.json` (`tokens.access_token`) |
| Z.ai / GLM | API token, Global or BigModel CN region | user-supplied key |
| Kimi | `GET https://api.kimi.com/coding/v1/usages` | user-supplied key from Kimi Code Console |
| Antigravity | local HTTPS server of the `agy` CLI, `RetrieveUserQuotaSummary` | `agy` session |

Codex maps `rate_limit.primary_window` / `secondary_window` to the session and weekly
lanes, plus named `additional_rate_limits[]`. Antigravity reports two model groups
(Gemini, Claude+GPT) × two windows. "OpenAI" here means the ChatGPT/Codex subscription,
not the API Platform billing dashboard — the latter has spend, not reset windows, and
does not fit this model.

Providers are a plugin point: adding one is a new implementation of the provider trait
plus registration, not a refactor.

## Architecture

Daemon plus client, split over D-Bus.

- **`tidemarkd`** — systemd user unit. Owns polling, credential refresh, history, and
  notifications. Runs whether or not the window is open, because a warning that only
  arrives when you are already looking is not a warning.
- **GUI** — a thin viewer. Never performs network I/O.
- **CLI** — not in v1, but the D-Bus interface is designed so `tidemark usage --json`
  is a third consumer rather than a bolt-on. Waybar is the obvious client.

Language is Rust; GUI is GTK4 + libadwaita. Rust was chosen for packaging above all —
`deb`/`rpm`/`PKGBUILD` from a single binary is the cheap path — and for `serde`, which
turns "the provider silently changed their undocumented JSON" from a blank screen into a
named field in an error.

### Crate layout

Four crates, not three. The extra one exists to make the "GUI never performs network I/O"
rule checkable rather than aspirational.

| crate | holds | must never reach |
|---|---|---|
| `tidemark-types` | vocabulary, identity constants, D-Bus wire shapes | anything with I/O |
| `tidemark-core` | provider clients, history, secrets | GTK, GDK, libadwaita |
| `tidemarkd` | scheduler, D-Bus service, notifications | — |
| `tidemark` | the interface | `tidemark-core`, HTTP, SQLite |

The GUI depends on `tidemark-types` and D-Bus only. Folding the vocabulary into
`tidemark-core` and feature-gating the network out of it does not work: Cargo unifies
features across a workspace build, so the moment the daemon is built the GUI links the
HTTP stack too. A separate crate is the only form of this rule the build actually
enforces. `scripts/check-layering.sh` asserts it against `cargo tree`.

### API floor

GTK **4.14** and libadwaita **1.5**, set as `gtk4/v4_14` and `libadwaita/v1_5` features.
Not arbitrary: `AdwDialog`, which the detail view depends on, is libadwaita 1.5. That
floor is Ubuntu 24.04 LTS, Debian 13 and Fedora 40 — raise it only for something the
interface genuinely needs, and say so in this file when you do.

TLS is rustls, and SQLite is the system library rather than a vendored copy, both so the
`deb` and `rpm` do not carry a bundled C library that distribution policy dislikes.

## Storage

- **History** — SQLite, keyed `(provider, account, window, segment)`. A point is written
  only when `used_percent` changes, plus an hourly anchor. Points older than 90 days are
  thinned to one per 15 minutes; nothing is deleted outright.
- **Segment boundaries** — `resets_at` within ±5 minutes counts as the same segment, and
  a drop in `used_percent` independently confirms a reset. Both signals are needed: some
  providers compute `resets_at` as "now + N" so it drifts by seconds on every poll, and a
  drop alone can be a plan change rather than a reset. Segmenting on exact `resets_at`
  equality produces one segment per point and destroys the history — this is observed
  behaviour in a prior art implementation, not a hypothetical.
- **Rejected on ingest** — points with a zero or absurd timestamp. They sort wrong and
  stretch every chart by decades.
- **Window identity** — a window's key is derived from its **length**, never from the field
  name the provider used. Codex proves why: on 2026-08-19 its only window arrived as
  `rate_limit.primary_window` carrying `limit_window_seconds: 604800` — a *weekly* window in
  the slot named "primary", with `secondary_window: null`. Earlier the same account had its
  weekly figures recorded under `secondary`. Keying on the provider's positional name would
  split one continuous window into two and emit spurious appeared/disappeared events.
- **Secrets** — our own API keys go to the Secret Service (`org.freedesktop.secrets`).
  The daemon must handle "keyring still locked" as an explicit state rather than a crash;
  the unit orders itself after `graphical-session.target`.
- **Third-party credential files** stay where they are. See ADR 0001.

## Networking

Every request sets `User-Agent: Tidemark/<version>`. This is not cosmetic:
`platform.claude.com` sits behind Cloudflare and answers an unset agent with
`403 browser_signature_banned`. Identify honestly by product name — never impersonate a
browser. We authenticate with real credentials to documented endpoints; there is no reason
to look like anything other than what we are.

## Polling

Adaptive: 5 minutes baseline, 60 seconds in the last 15 minutes before a reset, 30 minutes
when no session activity is detected. Exponential backoff on 429, capped at one hour.
Antigravity likely needs a longer interval of its own because reaching it means bringing
up the `agy` local HTTPS server rather than making one request — to be measured.

Gaps from suspend stay gaps. History records observed measurements; invented points would
corrupt the forecast, which is the one thing the history exists for.

## Notifications

Thresholds at 80% and 95%, fired once per segment — the segment is the natural dedup key.
Every window notifies, not just the dominant one: you need to know both that the
five-hour window is closing and that the weekly one is. Reset notifications only fire for
windows that passed 50% in the previous segment; "your weekly quota reset" after burning
3% of it is noise. Forecast-based notifications are deliberately not in v1 — they need
calibration against history that does not exist yet.

## Interface

- **Grid of provider cards** (`GtkFlowBox`, 1–3 columns by width), sorted by urgency, with
  user-defined order persisted to config. Reordering is manual `GtkDragSource` /
  `GtkDropTarget` work — `GtkFlowBox` has no reorder API.
- **Card** — logo, name, plan, state chip; the shortest present window as a large number
  over a bar with a pace mark; remaining windows as thin rows.
- **A window the provider did not send is not drawn.** No placeholder, no explanation. The
  window set is whatever arrived; the card rearranges silently when it changes. Needs
  hysteresis in the daemon so a single malformed response does not make a window blink.
- **Click opens a detail dialog** (`AdwDialog`, standard dimming; real blur via
  `gtk_snapshot_push_blur()` is possible and deferred) with the burn-down chart for the
  current segment.
- **Failure states** are distinguished in data but collapsed in the UI into three groups by
  what the user must do: *you fix it* / *it fixes itself* / *they broke it*.
- **Tray** — static SNI icon, spoken directly over GDBus. Left click lists providers with
  the percentage of their shortest window. `libayatana-appindicator-glib` is GPL-3 and
  cannot be linked into an MIT project.

## Packaging

`deb`, `rpm`, `PKGBUILD`. Targets need GTK4 ≥ 4.18, which rules out Ubuntu 24.04 as a
target — though its glibc, being the oldest, makes it a candidate build host, since glibc
is forward- but not backward-compatible.

## Non-goals

- No web UI, no Electron, no embedded browser engine. See ADR 0002.
- No browser-cookie scraping, and therefore no providers that require it.
- No API Platform spend dashboards. Different metric, different product.
- No forecast-driven notifications until there is history to calibrate them on.

## Prior art

CodexBar (MIT, Swift, macOS menu bar) is read for protocol facts: which endpoint, which
field, which fallback order. Its layout is macOS popup-shaped and is not a reference for
ours. `~/repos/CodexBar` on this machine contains an abandoned Linux-GUI fork; it is a
dead end and is not a base for this project.
