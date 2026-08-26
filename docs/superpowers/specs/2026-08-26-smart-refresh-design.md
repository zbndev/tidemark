# Providers refresh: Auto zones and a manual interval

Date: 2026-08-26
Branch: `feat/smart-refresh`
Status: approved by the owner on 2026-08-26

## Problem

Polling today is one fixed ladder — 5 minutes baseline, 60 seconds near a reset, 30 minutes
when consumption stops moving — that knows nothing about how much quota is left. A provider
at 95% is polled as rarely as one at 5%, and an idle account can burn half its quota for
thirty minutes before the daemon notices. The owner wants the interval to follow the state
of the quota, with an opt-out for a fixed pace.

## Decision

Two modes, one switch, kept by the daemon in `config.toml` and edited from the General page
of the preferences dialog:

- **Auto** (default) — the interval comes from the worst window the account reports.
- **Manual** — one fixed interval in minutes, 1 to 120, default 5.

### Auto intervals

The zone boundaries are the shared constants `WARNING_AT` (70) and `DANGER_AT` from
`tidemark-types` — the colour the bar shows and the rate the daemon polls at are the same
opinion, so they can never disagree.

| Worst window usage | Interval |
| --- | --- |
| < 70% (blue) | 10 minutes |
| 70% to < 90% (yellow) | 5 minutes |
| 90% to < 100% (red) | 1 minute |
| >= 100% (exhausted) | 10 minutes |

**Worst window decides.** An account's interval is the minimum over its windows' zone
intervals: a provider whose five-hour window is red is polled every minute even if its
weekly window is blue. Scheduling stays per account, as the engine already works.

**Near-reset survives in Auto only.** Within 15 minutes of the soonest reset — including an
overdue one — the interval drops to 60 seconds. This is the mechanism that makes the reset
notification immediate: the first reading after the rollover opens a new segment and fires
`Kind::Reset` within a minute of the boundary, not one zone-interval later.

**Idle is removed.** The 30-minute slowdown for accounts whose consumption stopped moving
is deleted entirely, in both modes. Rationale from the owner: half an hour is enough to
spend most of a cheap subscription's quota with no notification at all. The
`seconds_since_change` situation field, the `IDLE`/`IDLE_AFTER_SECS` constants and the
engine's `last_change_at` plumbing exist only to feed it and go with it.

### Manual behaviour

Healthy accounts are polled at exactly the chosen interval. No near-reset speedup, no idle:
the user picked a pace and gets it. The cost is stated: a reset notification can arrive up
to one interval late.

### What does not change

- Error states keep the exponential backoff from a 5-minute seed, capped at an hour, a
  provider's longer `Retry-After` still obeyed — in both modes. A first failure waits 5
  minutes even under Manual 1: a falling endpoint is not hammered.
- `KEYRING_RETRY` (60 s) and `USER_ACTION_RETRY` (30 min) are untouched.
- A healthy account with no windows yet polls at the blue interval; a fresh account's
  first poll is immediate, as it is today.
- The published `next_poll_at` keeps describing what the scheduler actually decided.

## Configuration

A new `[refresh]` table in `config.toml`, edited in place like every other preference:

```toml
[refresh]
mode = "auto"    # or "manual"
minutes = 5      # 1..=120, read only in manual mode
```

An unknown mode, a non-integer `minutes`, or a `minutes` outside 1..=120 is refused with a
`ConfigError::InvalidPreference` — refused, never clamped, matching the proxy port rule.

## Wire surface

`Preferences` (the `a{sv}` dict) gains `refresh_mode: String` and `refresh_minutes: u32`,
with `REFRESH_AUTO`/`REFRESH_MANUAL` constants and `valid_refresh_mode`. Defaults: `auto`,
5. This is the documented extension path for the dict; clients that do not know the keys
are unaffected. Two D-Bus methods follow the existing one-method-per-preference pattern:

- `SetRefreshMode(mode: s)`
- `SetRefreshMinutes(minutes: u)`

Both validate before queueing a `Command::SetPreference` and publish `PreferencesChanged`
with the complete dict afterwards, exactly as `SetStartupMode` does.

## Applying a change

The engine holds the parsed mode in a field, set at construction from `Config::preferences()`
and replaced on `SetPreference`. Changing either value persists it, updates the field, and
marks every account due immediately — the `adopt_proxy` precedent: one extra request per
account on a rare user action, in exchange for the new pace being observable at once rather
than one old interval later. `reschedule` then computes the worst `used_percent` across the
account's published windows and passes it, with the mode, into the `Situation`.

`scheduler::Situation` gains `mode: RefreshMode` (`Auto | Manual(Duration)`, daemon-side
enum) and `worst_used_percent: Option<f64>`, and loses `seconds_since_change`.
`next_interval`'s healthy branch becomes: Auto → near-reset check, then the zone interval;
Manual → the chosen interval. Everything else in the function is unchanged.

## Interface

On the General page of the preferences dialog, after the Startup group, a new
"Providers refresh" group:

- An `AdwSwitchRow` titled **Auto**, subtitle *"Refresh frequency adapts to how much quota
  is left."* — deliberately without detail; the zones are the daemon's business.
- An `AdwSpinRow` titled **Manual refresh frequency**, subtitle *"Minutes between polls
  when Auto is off."*, adjustment 1..=120 step 1, with the standard +/- stepper. The row is
  insensitive while Auto is active, and its value is not read by the daemon in that state.

Both rows commit through the daemon proxy with the dialog's existing suppress/toast/rollback
pattern.

## Documentation

`CONTEXT.md` § Polling is rewritten to the new design — the normative record must follow
the owner's decision — and the `scheduler.rs` module doc, which quotes it, is updated to
match. No new ADR: this is a behaviour change recorded in `CONTEXT.md`, not a binding
cross-cutting decision.

## Tests

Colocated, sentence-style names, no new harnesses:

- **scheduler:** each zone boundary (69.9 → 10 min, 70 → 5 min, 89.9 → 5 min, 90 → 1 min,
  99.9 → 1 min, 100 → 10 min, 105 → 10 min); near-reset beats every zone in Auto; Manual
  ignores near-reset; Manual's own interval; no windows → blue; backoff, keyring and
  user-action paths unchanged; the idle tests are deleted with the feature.
- **config:** defaults read as auto/5; a round trip writes `[refresh]` without touching the
  rest of the file; unknown mode and out-of-range or non-integer minutes are refused.
- **wire:** `Preferences` with the new keys survives a zvariant round trip.
- **engine:** switching the preference marks accounts due and the next interval reflects
  the new mode; worst-window selection feeds the situation.
- **service:** `SetRefreshMode` with an unknown value answers `InvalidArgs`; a valid one
  queues the command and publishes the dict.
- **UI:** mode-to-row mapping and the rule that keeps the Manual row insensitive under
  Auto.
