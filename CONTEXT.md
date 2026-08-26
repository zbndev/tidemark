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
- **Provider definition** — one entry in the catalog compiled into this build: its stable
  slug, display metadata, authentication kind, and declared settings. The catalog says
  what can be added; it does not say what the daemon currently polls.
- **Configured account** — a catalog provider the user has added. v1 creates its single
  `default` account and persists the provider slug in `config.toml`.
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
| Antigravity | Cloud Code Assist `fetchAvailableModels`; optional local `agy` fallback | Tidemark Google OAuth, or an existing `agy` session |

Three of them have **two** credentials rather than one, and which of the two an account
uses is the user's choice, not a rule hidden in the daemon. It is stored as
`[provider.<slug>] source = "oauth" | "cli"`, published with the provider so a client can
draw it without knowing what either credential is, and drawn in the authentication group
as a two-part control — Tidemark's own login on the left, the local program's on the right.
**A pinned credential that is not there is `no-credential`, never a quiet fall back to the
other one**: falling back would show quota for an account the user did not choose. An
account whose `source` has never been set behaves exactly as it always did — the daemon
publishes which credential that resolves to, so the control still shows the truth.

Codex reports `rate_limit.primary_window` / `secondary_window` — slots rather than lanes,
each declaring its own length — plus a `code_review_rate_limit` of the same shape and named
`additional_rate_limits[]`. The three are separate pools, not lengths of one. One extra
pool is dropped rather than drawn: `metered_feature: "base_model_inference"`, shown by
OpenAI as *gpt-reserve*, whose weekly window resets at `captured_at + 604800` on every
poll and so measures nothing.
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

The compiled provider catalog and configured accounts are separate D-Bus concepts.
`ListProviders` returns every provider definition supported by this build. `GetStatus`
returns only configured accounts — state, last good reading, how the account is
authenticated, and when the next poll is due — so one round trip is enough to draw the
whole window, or a Waybar module, or a line of CLI output. `AddProvider(provider)` creates
and persists the provider's `default` account; `RemoveProvider(provider, account)` removes
one. `ProviderChanged` carries the same status shape as `GetStatus` for an account that was
added or updated, while `ProviderRemoved(provider, account)` tells long-running clients to
drop its settings row and card. `SetOrder(providers)` rewrites the order the cards go in and
`OrderChanged(providers)` announces it — a signal of its own rather than a burst of
`ProviderChanged`, because a status carries no position: a client holding cards needs the
sequence, not the readings again. `GetStatus` answers in that order from the moment it is
emitted. `Refresh(provider)` polls now, with an empty string meaning everything, re-reads
the credential on the way, and looks again for the vendor login a provider can read instead
of ours — a refresh is what a user reaches for straight after running `claude` in a
terminal. `Version` says what is on the other end.

Credentials are the daemon's, so changing them is the daemon's too: `SetKey`, `SignOut`,
`SetOption`, `SetWindowNotify`, and the two halves of a login. Nothing there is specific to the GUI — a
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

### Local authentication sources

Some providers hold no key at all — their credential is a session this machine
already has. Cursor is the first: it reads cursor.com through either the Cursor
App's local state or one browser's cookie store. Which of them answers is a
recorded choice, not a scan: the daemon discovers every candidate, proves each
with a real request, and records the pick (`auth-source`, plus `auth-browser`
and `auth-profile` where they apply) beside the account's other options — the
same way an OAuth-or-CLI choice is stored. The window sees titles and readiness
states over D-Bus, never cookie values, tokens, or database paths.

Three boundaries make this acceptable where blanket cookie-scraping would not be:

- **The daemon owns all of it.** Browser storage is touched by tidemarkd alone;
  the GTK process renders published states and never learns how a credential is
  stored.
- **Snapshot reads.** A browser's cookie database is read through an owner-only
  temporary copy; no browser directory is opened in place or written to.
- **One recorded source, used exclusively.** A failing source fails the account,
  and Tidemark reports the gap rather than quietly switching to another browser
  or the App — quota on screen must belong to the account the user chose.

Browser slugs and profile identifiers are stable config keys for the same reason
provider slugs are — not renamable once shipped.

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
  it started and how full it got. The default retention is `forever`, so nothing is deleted
  outright unless the user chooses six months or one year in Preferences. A shorter policy
  deletes older points immediately and during daily maintenance. Clear History deletes
  points, segments, notification deduplication and last-observed state, then compacts the
  database; provider configuration and credentials are separate and survive.
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
- **Settings** — `config.toml` holds what is neither a secret nor a reading: Z.ai's region,
  the ordered `providers` array and the per-window notification opt-in (`[notify.<slug>]`,
  a `windows` array of window keys). **That `providers` array is the card order.** Not a
  second key beside it: it is already ordered, already the user's set, already the order
  `GetStatus` publishes in, and adding a provider already appends to it — which is exactly
  what "a new card goes on the end" means. A separate `card_order` would be a second list to
  keep in agreement with the first, and its first question — where a provider named in one
  and absent from the other goes — has no good answer. Reordering **moves the values and
  leaves the decoration where it was written**: a TOML array has no notion of a comment
  belonging to an element, since in `"claude", # mine` the comment is part of the *next*
  element's prefix and in the style above it part of its own, so carrying decoration along
  would scramble comments and indentation in opposite directions depending on which style
  the file uses. What comes back is the file byte for byte apart from the slugs. A missing
  file or missing
  array has the same meaning as `providers = []`: a fresh installation has no configured
  accounts. It is edited rather than rewritten, so comments, ordering and keys a newer
  build added all survive a change made from the interface. A file that does not parse is
  an error, never silently replaced with defaults. Removing a provider deletes its
  provider-specific settings and both kinds of Tidemark-owned credential, then removes its
  account and card; quota history and vendor-owned credential files or `agy` sessions are
  retained.
- **Application preferences use the same file, not GSettings.** `[general]` carries
  `minimize_on_close` (default `true`) and `startup`, whose named modes are `app` (the
  default: app and tray), `daemon` (daemon only), and `off`; `[updates] check` defaults to
  `true`; `[history] retention` is `forever`, `six-months` or `one-year`; `[proxy]` carries
  `mode`, `host` and `port`, written as one edit because they are one setting. A present value
  with the wrong type or an unknown named choice is an error, not a reason to guess.
  Mutations travel over D-Bus and share the engine's serial configuration queue, so a
  provider edit and a global preference cannot overwrite each other.
- **The startup mode names one coherent desired state in `config.toml` and applies both
  platform integrations.** Hiding the desktop client writes the standard per-user XDG
  autostart override (`Hidden=true`); exposing it removes that override and reveals the
  packaged `/etc/xdg/autostart` entry again. Daemon-only startup uses `systemctl --user
  enable tidemarkd.service`; the other modes disable that unit without stopping the current
  daemon, because launching the app already D-Bus-activates it. If either platform
  integration or persistence fails, the previous platform mode is restored.

## Networking

Every request sets `User-Agent: Tidemark/<version>`. This is not cosmetic:
`platform.claude.com` sits behind Cloudflare and answers an unset agent with
`403 browser_signature_banned`. Identify honestly by product name — never impersonate a
browser. We authenticate with real credentials to documented endpoints; there is no reason
to look like anything other than what we are.

**One proxy, set once, applied everywhere.** `[proxy]` in `config.toml` names a mode
(`off`, `http`, `https`, `socks5`), a host and a port; `tidemark-core`'s `providers::http`
holds it for the process, and every client this program builds starts from that one
builder. `socks5` becomes a `socks5h://` URL, so names are resolved at the proxy — for
anyone whose own resolver is the thing in the way, resolving here and handing the proxy an
address would defeat the point.

- **A subprocess is not an exception.** Antigravity's `agy` does its own HTTP, so the proxy
  is handed to it as `HTTP(S)_PROXY`/`ALL_PROXY`/`NO_PROXY` in both spellings, at spawn
  time, per command. **Not** on the systemd unit: an environment variable there takes
  effect on a restart, and a restart takes every card off the screen until the first poll
  comes back.
- **A change is adopted without restarting anything.** `SetProxy` takes all three values as
  one call — half a proxy applied is a proxy nothing can be reached through — validates the
  URL before the file is written, then drops every provider client. A `reqwest::Client`
  holds its proxy for life and its pool holds sockets opened through the old one, so the
  clients are rebuilt from stored keys and settings on the next poll. **No status is
  touched**: each card keeps its last good reading while the new client is built under it.
  Dropping Antigravity's client shuts `agy` down, and the next poll spawns it again with
  the new environment.
- **Loopback is never proxied.** `localhost,127.0.0.0/8,::1` is excluded on our clients and
  passed as `NO_PROXY` to children, and Antigravity's loopback client is built with
  `no_proxy()` outright. Asking a proxy to reach `agy` on this machine asks it to connect
  to itself.
- **Off means "as before", not "direct".** With no mode set, `reqwest` reads the process
  environment and a child inherits it, which is what this program did before the setting
  existed.

## Polling

Adaptive, in two modes kept in `[refresh]` in `config.toml` and switched from the
preferences dialog.

**Auto** (the default) paces each account by its worst window: ten minutes under 70% used,
five minutes from 70%, one minute from 90%, and ten again once the quota is exhausted —
nothing more can be spent, and the reset itself is watched for separately. The zone
boundaries are the same 70/90 constants the bar colours and the notifications use, so all
three agree about when a window is worth watching. Within fifteen minutes of a reset —
including an overdue one — the interval drops to sixty seconds, because a rollover is the
one event history cannot reconstruct afterwards, and the reset notification is owed the
moment the boundary passes rather than one zone-interval later.

**Manual** polls healthy accounts at exactly the chosen interval, one to a hundred and
twenty minutes. No near-reset acceleration: the user picked a pace and gets it, at the
stated cost that a reset notification can arrive one interval late.

Changing the mode polls every account immediately; changing the manual interval applies
from each account's next poll. There is deliberately no idle slowdown in either mode:
half an hour of quiet is enough to spend most of a cheap subscription's quota with no
notification at all.

Failures back off exponentially in both modes — five minutes, doubling, capped at an hour.
A provider's own `Retry-After` is obeyed when it is *longer* than our backoff and never
when it is shorter: a service failing every request while asking for one second would
otherwise turn a backoff into a hot loop. The hour cap applies to our own guess, not to an
explicit instruction from the provider. A locked keyring is re-asked every minute; states
only the user can clear wait half an hour.
Antigravity likely needs a longer interval of its own because reaching it means bringing
up the `agy` local HTTPS server rather than making one request — to be measured.

Gaps from suspend stay gaps. History records observed measurements; invented points would
corrupt the forecast, which is the one thing the history exists for.

## Notifications

Thresholds at 70% and 90%, fired once per segment — the segment is the natural dedup key,
and the rows recording what has gone out live in the history database, so a daemon restart
does not warn anybody a second time. Any window can notify, not just the dominant one: you
need to know both that the five-hour window is closing and that the weekly one is.

**Opted into per window.** Five providers report fifteen windows between them, and a
warning about all of them is a warning about none, so a freshly added provider is silent
until a switch on its settings page is turned on. The switch covers both the thresholds and
the reset for that window.

**A reset always notifies**, however little of the previous segment was spent. The earlier
rule here was "only above 50%, because *your weekly quota reset* after burning 3% of it is
noise" — and that reasoning holds only for resets that arrive on schedule. Providers reset
quota outside their own schedule, and an unscheduled reset is exactly the news a person
acts on. The opt-in is what keeps the volume down; a consumption threshold on top of it
would only hide the interesting case.

The two thresholds are the same constants the bar changes colour at, defined once in
`tidemark-types` rather than in each process, and the phrasing of a percentage or a span is
shared the same way: the card and the notification must never disagree.

Forecast-based notifications are deliberately not in v1 — they need calibration against
history that does not exist yet.

## Interface

- **Grid of provider cards**, one to three columns by width, **in the order the user put
  them in**. Nothing else ever changes that order: there is no urgency sort underneath it,
  a new account goes on the end, and the sequence is persisted by the daemon and
  republished to every client. The grid the tray menu lists and the grid the settings
  dialog lists are this one.
- **The grid is a widget of ours, not a `GtkFlowBox`.** Reordering has to be *live* — the
  cards a held card displaces move out of its way before the button is released, and move
  back if the pointer changes its mind — and `gtk_flow_box_invalidate_sort()` sorts a
  sequence and queues a resize, so a card that loses its place teleports. There is nothing
  to interpolate, because the position *is* the allocation. Nothing in GTK 4.22 or
  libadwaita 1.9 does this generally; the one upstream implementation of exactly this
  behaviour is libadwaita's private `AdwTabGrid`, and `grid.rs` is its architecture: a
  `GtkGestureDrag` on the container, a per-card offset in **index units** animated by an
  `AdwTimedAnimation` restarted from its current value, an animation callback that queues an
  allocation, and a `size_allocate` that turns a fractional index into a position.
  `GtkDragSource` / `GtkDropTarget` are the wrong controllers for it: they carry a payload
  and draw a detached icon, and an icon that is not in the grid cannot push anything.
- **The order is committed on release, not on the way.** A file write and a D-Bus round trip
  per pixel is not a design. The drop is applied locally first and sent afterwards, because
  a grid that waited for the daemon before showing where the card landed would feel broken;
  a refusal — the configured set moved while the card was in the air — puts the cards back
  to what the daemon actually has.
- **Empty state** — when there are no configured providers, the main window says
  `Welcome to Tidemark` and `Add a provider to start tracking your quota.` The providers
  button remains available while the daemon is connected.
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
- **Provider settings carry the notification switches** — one row per window the account
  currently reports, drawn from the last reading rather than from a fixed list, because the
  window set is whatever arrived. An account nobody has polled yet has no switches to
  offer and the group is not drawn at all.
- **The bar is drawn, not a `GtkLevelBar`**, because of the pace mark. Its colours come
  from the CSS names `@accent_bg_color`, `@warning_bg_color` and `@error_bg_color` rather
  than from `AdwStyleManager`, so that a user who has themed their accent gets a bar in
  their colour rather than the one libadwaita would have picked. **It changes colour at 70%
  and 90% — the notification thresholds** — so the card and the notification never disagree
  about when a window became worth worrying about.
- **A card raises on hover** — two pixels and a soft shadow. The `:hover` is matched on the
  slot around the card and the card is what moves, because a CSS transform moves what GTK
  picks and a card that lifted itself out from under the pointer would flicker. That slot
  is an `AdwBin`; it was a `GtkFlowBoxChild`, which also tinted its own square allocation
  behind a card with rounded corners and had to be told not to. `.card.activatable`
  supplies the platform's own hover and active states.
- **A card being carried is opaque.** `.card` takes `@card_bg_color`, which in the dark
  style is 8% white over whatever is behind it — right for a card lying on the window, and
  wrong for one crossing its neighbours, which then read straight through it. The dragged
  card takes `@popover_bg_color`, the platform's own name for a surface floating above the
  content, and a deeper shadow. Its foreground is deliberately left alone: the bar's track
  and pace mark inherit the text colour, and changing it would make them shift tone for the
  length of a drag.
- **The grid is homogeneous.** Every card gets the same allocation, so cards in a row share
  a height and their footers line up; the cost is a short card in a single-column window
  carrying the height of the tallest one. The last row is left ragged: a filler card would
  be something to click on that does nothing.
- **A card's width is the card's, not the daemon's.** The cell is the widest card's
  *minimum* width — its own width request — and never its natural width, because a natural
  width is the width of whatever text arrived: a provider that answered with an error
  message longer than a card, or a window title nobody sized a card for, used to set the
  width of every card on screen and collapse three columns into one. So every label that
  shows a string from the daemon ellipsizes, and the one that shows prose whole wraps at any
  character and stops after three lines — a D-Bus error name and a URL have no space in them
  to wrap at, and a label that cannot shorten itself asks for the width of its text as its
  *minimum*. The rest of a long message is in the provider's settings pane, and in the
  tooltip.
- **A window the provider did not send is not drawn.** No placeholder, no explanation. The
  window set is whatever arrived; the card rearranges silently when it changes. Needs
  hysteresis in the daemon so a single malformed response does not make a window blink.
- **Click opens a detail dialog** (`AdwDialog`, standard dimming; real blur via
  `gtk_snapshot_push_blur()` is possible and deferred) with the burn-down chart for the
  current segment.
- **Failure states** are distinguished in data but collapsed in the UI into three groups by
  what the user must do: *you fix it* / *it fixes itself* / *they broke it*. The first group
  has somewhere to go: the provider's settings detail page.
- **Provider settings** opens on a list of configured accounts. Its add button pushes a
  searchable picker containing catalog entries that have not been configured; choosing
  one adds it and opens that provider's detail page. Edit reaches the same detail page,
  which is drawn from the daemon's authentication and settings declarations. A stored key
  is never shown back: the row says whether there is one, and replacement requires typing
  a new one. Removal is destructive and confirmed: it deletes Tidemark-owned credentials,
  provider settings and the current card, but keeps quota history. Closing the preferences
  dialog cancels pending OAuth work, and the dialog can then be opened again.
- **A provider with two credentials names both of them.** Its authentication group leads
  with a two-part control — Tidemark's own login, and the login the vendor's program keeps
  on this machine — and the rows below it are the half that is selected. The Tidemark half
  is the sign-in button and what it is signed in as. The local half is the three things
  somebody with a problem needs, in that order: whether the login is there and where
  Tidemark looked for it, what to run if it is not, and — where it is true — that Tidemark
  refreshes that credential in place and writes the rotated token back into a file it does
  not own. That last sentence is ADR 0001, and it is stated in the open next to the choice
  rather than left to be discovered: a program that edits another program's credentials
  says so where the decision is made.
- **The primary menu is the platform's, and so are its dialogs.** The header's rightmost
  button opens the menu every GNOME application keeps there. Preferences and About
  Tidemark are separate sections; provider and credential management stays on its existing
  header button because accounts are managed, not application preferences. Preferences is
  an `AdwPreferencesDialog` with General, Network and Data pages and standard rows. The
  release-check switch lives on Network beside the proxy rather than on a page of its own:
  it is one switch, and what it switches is whether this program reaches the network at all
  on its own behalf. About
  is an `AdwAboutDialog` with properties set and no layout of ours: the icon over the name,
  the version pill, Details, Report an Issue and
  Legal are what the platform draws from `application-icon`, `version`, `website`,
  `issue-url`, `copyright` and `license-type`. The warranty sentence and the licence link
  are GTK's own text for `MIT_X11` rather than a second copy of a legal notice to keep in
  agreement — and the issue link goes to `/issues/new/choose`, because the repository has
  templates and a report that skips them has to be asked for the version and the desktop
  all over again.
- **The three proxy rows are one form, and the mode row opens the other two.** Choosing
  `SOCKS5` before typing where the proxy is, is how the group gets filled in, so the
  choice alone is not sent - the daemon would refuse half a proxy and be right to - and the
  host and the port become editable off the *row's* selection rather than off what is
  stored. Deriving them from storage is a lock: the stored mode is still `off` at exactly
  the moment the two rows that would change it have to be typed into. Nothing reaches the
  daemon until all three are complete, and only a deliberate submit of an incomplete one is
  marked wrong.
- **The one page that is ours is Troubleshooting**, and it is there so those questions are
  answered before they are asked: the client's version, the version the daemon on the other
  end reported, the GTK and libadwaita the process actually loaded, the desktop and session
  type, and whether a status-notifier host took the icon — which is the difference between
  a close button that hides the window and one that ends the program. Runtime values, not
  the compiled floor: the floor is what we built against and no evidence at all about the
  machine the bug is on.
- **Tray** — a static StatusNotifierItem, owned by the interface process rather than by the
  daemon. Left click shows the window; the menu lists the configured accounts with the
  percentage of their shortest window, in the order the grid uses, and ends with Open,
  Refresh and Quit. The icon takes the `NeedsAttention` status at the same threshold the
  bar changes colour at and the notification fires at, so the panel cannot become a third
  opinion about when a window got worrying.
- **Closing the window hides it by default; the tray is what the program minimises to.**
  `minimize_on_close = true` keeps the interface process alive, readings keep arriving, and
  Quit in the tray menu is the way out. Switching it off makes close end the interface
  process. Both choices remain conditional on the icon actually being accepted: when no
  status-notifier host answers, close always ends the process because hiding a window with
  nothing left to bring it back is worse than ignoring a preference.
- **The `app` startup mode uses that same tray condition.** `tidemark --background` builds
  the window without showing it and stays only after a StatusNotifier host accepts the
  icon. On a desktop without one it exits cleanly instead of leaving an invisible process
  behind.
- **Release checks are optional twice.** `[updates] check = false` stops the hourly GitHub
  request at runtime and clears any published update notice. The daemon's `update-check`
  Cargo feature is enabled by default for upstream builds; a distribution can build with
  `--no-default-features`, in which case Preferences shows the switch off and unavailable.
- **`libayatana-appindicator-glib` is GPL-3** and cannot be linked into an MIT project. The
  protocol is spoken through `ksni`, which is Unlicense — public domain, so compatible —
  and which is built on the same zbus the interface already reaches the daemon over.

## Packaging

`deb`, `rpm`, `PKGBUILD`. Distribution artwork policies can refuse third-party trademarks,
so a build with no provider marks stays a supported configuration: a card without one is a
state the interface already has.

The GTK 4.22 / libadwaita 1.9 floor above is GNOME 50, which became the default in exactly
two places: **Fedora 44** and **Ubuntu 26.04 LTS**. So the `rpm` targets Fedora 44+ and the
`deb` targets Ubuntu 26.04+. Nothing older qualifies — Debian's trixie is at GTK 4.18 — and
the `ubuntu-26.04` runner reports 4.22.4 and 1.9.1, which is where that is checked rather
than assumed.

glibc is forward- but not backward-compatible, so a build host must be no newer than the
oldest target. That is settled by construction rather than by choosing a host: each format
is built on the oldest release of its own target — the `deb` on the `ubuntu-26.04` runner,
the `rpm` in a `fedora:44` container, because GitHub hosts no Fedora runner. There is no
cross-distribution glibc question left to get wrong.

Building each format on its own target is a correctness requirement, not tidiness.
Measured 2026-08-22: `cargo-generate-rpm`'s `auto-req` scans the payload *transitively*
rather than reading `DT_NEEDED`, so an `rpm` built on Arch asked for `libgstreamer`,
`libcups`, `libkrb5` and `libxml2.so.16` — none of which either binary links, and some of
which Fedora numbers differently. And neither packaging tool treats a missing dependency
helper as an error: without `dpkg-shlibdeps`, `depends = "$auto"` resolves to *nothing* and
`cargo-deb` emits a warning a log scrolls past, yielding a package that installs with no
GTK present and then fails to start. `scripts/check-package-deps.sh` turns that warning
into a failed build, and both package jobs run it.

An upgrade restarts the user's daemon: both formats' maintainer scripts call
`data/restart-user-daemon`, which uses `runuser` plus the user's runtime directory to reach
each real user manager rather than talking to root's. Fedora's `.host` machine transport
cannot reach the user manager in the systemd container even with `systemd-machined`
running, so it is deliberately not part of this path. `scripts/test-package-upgrade.sh`
proves the restart against real systemd user managers and real `dpkg` / `rpm`
transactions; it is run by hand rather than in CI.

## Non-goals

- No web UI, no Electron, no embedded browser engine. See ADR 0002 and ADR 0003.
- No silent source selection. A local browser or app session is read only from
  the source the user picked and the daemon validated; nothing scans or falls
  back on its own.
- No API Platform spend dashboards. Different metric, different product.
- No forecast-driven notifications until there is history to calibrate them on.

## Prior art

CodexBar (MIT, Swift, macOS menu bar) is read for protocol facts: which endpoint, which
field, which fallback order. Its layout is macOS popup-shaped and is not a reference for
ours. `~/repos/CodexBar` on this machine contains an abandoned Linux-GUI fork; it is a
dead end and is not a base for this project.
