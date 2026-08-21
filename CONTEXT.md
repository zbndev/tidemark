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
| D-Bus interface | `io.github.zbndev.Tidemark.Daemon1` |
| GUI binary | `tidemark` |
| Daemon binary | `tidemarkd` |
| systemd user unit | `tidemarkd.service` |
| Config | `$XDG_CONFIG_HOME/tidemark/config.toml` |
| History | `$XDG_DATA_HOME/tidemark/history.db` |
| Secret Service schema, API keys | `io.github.zbndev.Tidemark.ProviderKey` |
| Secret Service schema, our own logins | `io.github.zbndev.Tidemark.ProviderToken` |

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

A pace mark needs both a length and a reset time, and a provider may withhold either.
**This is a normal state, not a failure.** Z.ai drops `nextResetTime` from a window that
has just reset and has nothing spent in it — observed live — and the window it does that
to is the five-hour one, which is the window the card leads with. Two of the five
providers also report windows with no length at all. The bar must therefore have a
defined appearance with no mark on it; inventing a length or a reset time to keep the mark
on screen would put a confident wrong number in front of the user.

## Providers in v1

All five reach their data over tokens or a local server. None requires scraping browser
cookies — a deliberate scope boundary, not a coincidence.

| Provider | Path | Credential |
|---|---|---|
| Claude | OAuth token → usage API | `~/.claude/.credentials.json` (`claudeAiOauth`), or our own login |
| Codex | `GET https://chatgpt.com/backend-api/wham/usage`, Bearer | `~/.codex/auth.json` (`tokens.access_token`), or our own login |
| Z.ai / GLM | API token, Global or BigModel CN region | user-supplied key |
| Kimi | `GET https://api.kimi.com/coding/v1/usages` | user-supplied key from Kimi Code Console |
| Antigravity | local HTTPS server of the `agy` CLI, `RetrieveUserQuotaSummary` | `agy` session |

Codex reports `rate_limit.primary_window` / `secondary_window` — slots rather than lanes,
each declaring its own length — plus a `code_review_rate_limit` of the same shape and named
`additional_rate_limits[]`. The three are separate pools, not lengths of one.
Antigravity reports two model groups (Gemini, Claude+GPT) × two windows. "OpenAI" here
means the ChatGPT/Codex subscription, not the API Platform billing dashboard — the latter
has spend, not reset windows, and does not fit this model.

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

### D-Bus interface

`GetStatus` returns every account at once — state, last good reading, how the account is
authenticated, and when the next poll is due — so one round trip is enough to draw the
whole window, or a Waybar module, or a line of CLI output. `ProviderChanged` carries exactly
the same shape for one account, so a long-running client has one parser rather than two.
`Refresh(provider)` polls now, with an empty string meaning everything, and re-reads the
credential on the way. `Version` says what is on the other end.

Credentials are the daemon's, so changing them is the daemon's too: `SetKey`, `SignOut`,
`SetOption`, and the two halves of a login. Nothing there is specific to the GUI — a
`busctl` line does the same thing — which is why it is on the interface rather than inside
a dialog. **A login is two calls, because the work spans two processes.** `BeginLogin`
takes the callback port, builds the authorize URL and returns it without waiting; the client
opens that URL however its platform opens URLs; `AwaitLogin` blocks until the browser has
come back and the tokens are stored. The daemon never opens a browser itself:
it is a background service that may have started before the session had a display.

Every published structure is a dictionary (`a{sv}`), not a fixed D-Bus struct. Two reasons,
and the first is the same rule that governs the bar: **absent must stay absent.** D-Bus has
no optional field, so a fixed signature would put a `0` where a provider said nothing about
a reset time — measurably wrong, and the Z.ai five-hour window does exactly this in
practice. The second is that adding a key must not break a client compiled against the
older shape.

Failure is part of the published shape, not an absence of it: an account carries a state —
`ok`, `pending`, `no-credential`, `waiting-for-keyring`, `keyring-unavailable`,
`credential-rejected`, `rate-limited`, `unreachable`, `malformed` — and keeps its **last
good reading underneath it**. A failed poll changes the chip on the card, not the numbers;
blanking a card because one request timed out would be less honest, not more.

### Provider contract

A provider knows its own identity and can produce a snapshot. The trait says nothing about
credentials, because the five acquire them in five different ways — a key in the Secret
Service, an OAuth token refreshed out of a third-party CLI's file, a local server holding
its own session in the keyring — and any credential the trait named would fit at most two
of them. Each provider owns its HTTP client for the same reason: Antigravity's talks to a
loopback server with a self-signed certificate and needs an exception no other provider
should be handed.

Underneath the trait, every implementation splits **transport from meaning**: `fetch`
performs the request and hands the body to a pure `parse(body, captured_at)`. All the
traps live in the parsing — an unnamed unit enum, millisecond timestamps, numbers arriving
as strings, remaining-instead-of-used — and none of them need a live key to get wrong.

**A provider must never silently drop a window.** A window missing from the interface reads
as "you have no such limit", which is the most dangerous thing this program can say. An
entry of a kind we recognize but cannot parse fails the whole fetch. Only an entry of an
*unrecognized* kind is skipped, because that is a quota type that did not exist when the
parser was written rather than a failure to understand one that did.

### API floor

GTK **4.22** and libadwaita **1.9**, set as the `v4_22` and `v1_9` binding features.

The floor is *the newest we can test against*, not the oldest distribution we could
theoretically reach. Long-term-support distributions are not a design constraint here: if
a widget or an API would make the interface better, we use it, and the packaging targets
follow the code rather than the other way round. Raise this line whenever the toolkit
gains something worth having — it is a floor, not a budget.

The consequence is deliberate and accepted: distributions shipping older GTK do not get a
native package. If reach ever matters more than it does now, that is a Flatpak, not a
rewrite of the interface against an older API.

SQLite is the system library rather than a vendored copy, so the `deb` and `rpm` do not
carry a bundled copy of a library the distribution already ships — `ldd` on the daemon
shows `libsqlite3.so.0` from `/usr/lib`.

TLS is rustls, which keeps OpenSSL out of the link. It does **not** keep C out: rustls's
default provider is `aws-lc-rs`, and `aws-lc-sys` vendors C and assembly compiled by the
`cc` crate at build time. That has a packaging consequence, measured rather than predicted
— those objects cannot be built with GCC's LTO, because `rust-lld` then fails the final
link on hundreds of undefined `aws_lc_*` symbols. Any packaging of this project turns
distribution-wide LTO off; `PKGBUILD` does it with `options=(!lto)`.

## Storage

- **History** — SQLite, keyed `(provider, account, window, segment)`. A point is written
  only when `used_percent` changes, plus an hourly anchor. Points older than 90 days are
  thinned to one per 15 minutes, except that the first and last point of every segment
  always survive — without them a thinned segment loses the two things worth keeping, where
  it started and how full it got. Nothing is deleted outright.
- **Last seen is not last written** — the reading segmentation compares against is the last
  one *observed*, which is not the last one *stored*. A window can roll over without
  consumption moving, and that transition is visible only in a reading no point was written
  for. Every poll updates the last-seen state whether or not it produces a row.
- **Segment boundaries** — a new segment starts when consumption falls, or when
  `resets_at` moves **further forward than the elapsed time explains**. Either signal is
  sufficient and neither is required: a provider may omit `resets_at` entirely, and a
  window can roll over from zero to zero without consumption moving at all.

  The comparison is against elapsed time, not against a fixed tolerance, and the
  difference is not academic. Two distinct pathologies live in the nine months of prior-art
  history on this machine, and only one of them is jitter:

  * *Jitter.* `claude.json` reports the same window as `807785940` and `807786000` a minute
    apart. A ±5 minute tolerance absorbs this.
  * *Drift.* Some windows are reported as "now plus what is left", so `resets_at` advances
    by the whole poll interval — five minutes — on every single poll. No two values are
    ever equal, and no fixed tolerance ever merges them, because the movement is exactly
    the size of the tolerance and never stops.

  Measured over that corpus, exact equality gives `opencodego primary` 1363 segments from
  1363 readings and `antigravity secondary` 825 from 831. The elapsed-time rule gives 13
  and 1. The tolerance still exists, at five minutes, but it absorbs jitter *on top of* the
  elapsed time rather than standing in for it.
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
- **Third-party credential files** stay where they are, and are never *created* by us. See
  ADR 0001. Tidemark updates the token fields of a file its vendor CLI already owns; it
  does not write one into existence, so a sign-in performed here can never overwrite or
  invent somebody else's session.
- **A login performed from Tidemark** is stored under the token schema above, in the same
  document shape the vendor CLI uses — so one parser and one expiry rule serve both
  sources. It wins over the CLI's file when it is there, because it exists only because
  the user explicitly signed in here; signing out removes it and hands the account back.
- **Settings** — `config.toml` holds what is neither a secret nor a reading: Z.ai's region
  today, card order later. It is edited rather than rewritten, so comments, ordering and
  keys a newer build added all survive a change made from the interface. A file that does
  not parse is an error, never silently replaced with defaults.

## Networking

Every request sets `User-Agent: Tidemark/<version>`. This is not cosmetic:
`platform.claude.com` sits behind Cloudflare and answers an unset agent with
`403 browser_signature_banned`. Identify honestly by product name — never impersonate a
browser. We authenticate with real credentials to documented endpoints; there is no reason
to look like anything other than what we are.

## Polling

Adaptive: 5 minutes baseline, 60 seconds in the last 15 minutes before a reset, 30 minutes
when no session activity is detected. Exponential backoff on 429, capped at one hour.

**"No session activity" means consumption that has stopped moving.** The daemon cannot see
the user's terminal and does not try to; the only activity signal it has is the number the
provider reports, so an account whose `used_percent` has not moved for 30 minutes is the
idle one. The cost is bounded and stated: after a quiet spell, the first poll of a new
session can be up to 30 minutes late. Near a reset always wins over idle — an idle account
is precisely the one whose rollover would otherwise be slept through, and a rollover is the
one event history cannot reconstruct afterwards.

A provider's own `Retry-After` is obeyed when it is *longer* than our backoff and never
when it is shorter: a service failing every request while asking for one second would
otherwise turn a backoff into a hot loop. The hour cap applies to our own guess, not to an
explicit instruction from the provider.
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
  over a bar with a pace mark; remaining windows as thin rows; and one quiet line along the
  bottom saying when the reading was taken. The plan is a convention rather than a field:
  the first row of the detail section a provider titles `Plan`.
- **The mark, the name and the plan stand on one baseline**, and the mark is the largest
  thing in the row — it is what the eye finds a card by. Bottom-aligning the widgets does
  not achieve this: GTK aligns allocations, and a label's allocation ends at its font's
  descent line. Each icon is drawn standing on the floor of its own square, and the row
  lifts each part by the depth it does not use.
- **When the next poll is due is not on the card.** It is the daemon's schedule rather than
  news about the account, and on a window that updates itself it was one more number moving
  for no reason the reader has to act on.
- **The logo is the provider's own mark, monochrome**, drawn as a symbolic icon and
  recoloured by the theme — not a glyph of our invention standing in for someone else's
  product. The marks are their owners' trademarks, used to identify the service the card is
  about; they are not covered by this project's licence, and whatever ships them says so.
- **The bar is drawn, not a `GtkLevelBar`**, because of the pace mark. Its colours come
  from the CSS names `@accent_bg_color`, `@warning_bg_color` and `@error_bg_color` rather
  than from `AdwStyleManager`, so that a user who has themed their accent gets a bar in
  their colour rather than the one libadwaita would have picked. **It changes colour at 80%
  and 95% — the notification thresholds** — so the card and the notification never disagree
  about when a window became worth worrying about.
- **A card raises on hover** — two pixels and a soft shadow — rather than taking the tint
  `GtkFlowBoxChild` paints on its own square allocation, which shows as a hard edge around
  the card's rounded corners. The `:hover` is matched on the child and the card is what
  moves, so the pointer never falls off the widget it just raised. When the card becomes
  clickable, `.card.activatable` supplies the platform's own hover and active states.
- **The grid is homogeneous.** Every card gets the same allocation, so cards in a row share
  a height and their footers line up; the cost is a short card in a single-column window
  carrying the height of the tallest one. The last row is left ragged: a filler card would
  be something to click on that does nothing.
- **A window the provider did not send is not drawn.** No placeholder, no explanation. The
  window set is whatever arrived; the card rearranges silently when it changes. Needs
  hysteresis in the daemon so a single malformed response does not make a window blink.
- **Click opens a detail dialog** (`AdwDialog`, standard dimming; real blur via
  `gtk_snapshot_push_blur()` is possible and deferred) with the burn-down chart for the
  current segment.
- **Failure states** are distinguished in data but collapsed in the UI into three groups by
  what the user must do: *you fix it* / *it fixes itself* / *they broke it*. The first group
  has somewhere to go: the credentials dialog.
- **The credentials dialog** is one group per account, drawn from what the daemon
  published — a key field, a sign-in button, or neither, plus whatever settings the
  provider declares. It knows the difference between pasting a key and signing in; it does
  not know what a provider is, so adding one adds a group to it and changes no code here.
  A stored key is never shown back: the row says whether there is one, and the way to
  replace it is to type a new one.
- **Tray** — static SNI icon, spoken directly over GDBus. Left click lists providers with
  the percentage of their shortest window. `libayatana-appindicator-glib` is GPL-3 and
  cannot be linked into an MIT project.

## Packaging

`deb`, `rpm`, `PKGBUILD`. Distribution artwork policies can refuse third-party trademarks,
so a build with no provider marks stays a supported configuration: a card without one is a
state the interface already has. Targets need GTK4 ≥ 4.18, which rules out Ubuntu 24.04 as a
target — though its glibc, being the oldest, makes it a candidate build host, since glibc
is forward- but not backward-compatible.

## Non-goals

- No web UI, no Electron, no embedded browser engine. See ADR 0002 and ADR 0003.
- No browser-cookie scraping, and therefore no providers that require it.
- No API Platform spend dashboards. Different metric, different product.
- No forecast-driven notifications until there is history to calibrate them on.

## Prior art

CodexBar (MIT, Swift, macOS menu bar) is read for protocol facts: which endpoint, which
field, which fallback order. Its layout is macOS popup-shaped and is not a reference for
ours. `~/repos/CodexBar` on this machine contains an abandoned Linux-GUI fork; it is a
dead end and is not a base for this project.
