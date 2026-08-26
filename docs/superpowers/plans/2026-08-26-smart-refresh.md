# Smart Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give providers a two-mode refresh pace — Auto (interval from the worst window's quota zone, near-reset speedup) and Manual (one fixed 1–120 minute interval) — configured from a new "Providers refresh" section on the General page.

**Architecture:** `scheduler::next_interval` stays the single decision point; its `Situation` gains the mode and the worst window's `used_percent` and loses the idle field. The mode travels config → wire dict → engine field → situation; two new D-Bus methods follow the existing one-method-per-preference pattern; the preferences dialog gains a switch row and a spin row.

**Tech Stack:** Rust workspace (edition 2024), gtk4-rs 0.11 / libadwaita 1.9, zbus, toml_edit. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-26-smart-refresh-design.md`

## Global Constraints

- Branch: `feat/smart-refresh` (already created). Conventional Commits with scopes (`feat(types):`, `feat(config):`, `feat(scheduler):`, `feat(engine):`, `feat(daemon):`, `feat(ui):`, `docs:`).
- Lints are hard: `unsafe_code = "forbid"`, clippy `all` with `-D warnings`, no `anyhow` anywhere.
- Test names are descriptive sentence-style snake_case starting `a_` / `an_` / `the_`.
- Layering: `tidemark-types` reaches nothing; the UI may not depend on `tidemark-core`.
- Config is edited in place with `toml_edit`; a present-but-wrong value is **refused, never clamped**.
- Zone boundaries reuse `WARNING_AT` (70.0) / `DANGER_AT` (90.0) from `tidemark-types` — never literals.
- Absent values in wire dicts stay absent; `Preferences` is an `a{sv}` dict, new keys are the extension path.
- Full local gate: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`.
- Run `cargo test --workspace` plainly (no `dbus-run-session`; bus-dependent tests skip themselves).

---

### Task 1: Wire vocabulary — `Preferences` gains the refresh pair

**Files:**
- Modify: `crates/tidemark-types/src/wire.rs` (struct at ~453, impl at ~476, Default at ~515, round-trip test at ~1031)

**Interfaces:**
- Consumes: nothing new.
- Produces: `Preferences.refresh_mode: String`, `Preferences.refresh_minutes: u32`, consts `Preferences::REFRESH_AUTO`/`REFRESH_MANUAL`, `Preferences::valid_refresh_mode(&str) -> bool`, `Preferences::valid_refresh_minutes(u32) -> bool`. Default: `refresh_mode = "auto"`, `refresh_minutes = 5`.

- [ ] **Step 1: Update the round-trip test and add the validator test (fails to compile: fields missing)**

In `crates/tidemark-types/src/wire.rs` tests, extend the existing construction and add a test:

```rust
    #[test]
    fn preferences_survive_the_bus() {
        let original = Preferences {
            release_check: false,
            minimize_on_close: false,
            startup_mode: "daemon".into(),
            history_retention: "one-year".into(),
            proxy_mode: "socks5".into(),
            proxy_host: "127.0.0.1".into(),
            proxy_port: 1080,
            refresh_mode: "manual".into(),
            refresh_minutes: 30,
        };

        let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("encodes");
        let (decoded, _): (Preferences, _) = encoded.deserialize().expect("decodes again");
        assert_eq!(decoded, original);
    }

    #[test]
    fn only_the_two_named_refresh_modes_and_a_bounded_interval_are_known() {
        assert!(Preferences::valid_refresh_mode(Preferences::REFRESH_AUTO));
        assert!(Preferences::valid_refresh_mode(Preferences::REFRESH_MANUAL));
        assert!(!Preferences::valid_refresh_mode("sometimes"));
        assert!(!Preferences::valid_refresh_mode(""));

        assert!(Preferences::valid_refresh_minutes(1));
        assert!(Preferences::valid_refresh_minutes(120));
        assert!(!Preferences::valid_refresh_minutes(0));
        assert!(!Preferences::valid_refresh_minutes(121));
    }
```

- [ ] **Step 2: Verify it fails**

Run: `cargo test -p tidemark-types preferences`
Expected: FAIL — compile error, `no field refresh_mode` / `no function or associated item named valid_refresh_mode`.

- [ ] **Step 3: Implement**

In the `Preferences` struct, after `proxy_port`:

```rust
    /// `auto` or `manual`: whether healthy polling follows the quota zones or one fixed
    /// pace. See `CONTEXT.md` § Polling.
    pub refresh_mode: String,
    /// Minutes between polls in manual mode, 1 to 120. Ignored while the mode is `auto`.
    pub refresh_minutes: u32,
```

In `impl Preferences`, after the `PROXY_*` consts:

```rust
    pub const REFRESH_AUTO: &'static str = "auto";
    pub const REFRESH_MANUAL: &'static str = "manual";
```

After `valid_proxy_mode`:

```rust
    /// Whether this build knows the named refresh mode.
    pub fn valid_refresh_mode(value: &str) -> bool {
        matches!(value, Self::REFRESH_AUTO | Self::REFRESH_MANUAL)
    }

    /// Whether a manual interval is one the daemon will run. Refused rather than clamped:
    /// the bounds are part of the setting's meaning, and a silently different pace is the
    /// one thing a user who set it deliberately must never get.
    pub fn valid_refresh_minutes(value: u32) -> bool {
        (1..=120).contains(&value)
    }
```

In `impl Default for Preferences`, after `proxy_port: 0,`:

```rust
            refresh_mode: Self::REFRESH_AUTO.into(),
            refresh_minutes: 5,
```

- [ ] **Step 4: Verify tests pass and the crate's other constructions still compile**

Run: `cargo test -p tidemark-types`
Expected: PASS. If other files in the workspace fail to compile (`Preferences { .. }` exhaustive constructions), fix them by adding the two fields with `..Default::default()`-compatible values only where the construction already uses struct update; otherwise add `refresh_mode: Preferences::REFRESH_AUTO.into(), refresh_minutes: 5,`. Then `cargo build --workspace`.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(types): add refresh mode and minutes to preferences"
```

---

### Task 2: Config — the `[refresh]` table

**Files:**
- Modify: `crates/tidemark-core/src/config.rs`

**Interfaces:**
- Consumes: Task 1's `Preferences` fields and validators.
- Produces: `Config::set_refresh_mode(&mut self, &str) -> Result<(), ConfigError>`, `Config::set_refresh_minutes(&mut self, u32) -> Result<(), ConfigError>`; `preferences()` reads `[refresh] mode` (string, validated) and `[refresh] minutes` (integer 1..=120, refused otherwise), defaults `auto` / 5.

- [ ] **Step 1: Write the failing tests**

Extend `a_first_run_has_safe_application_preferences` with:

```rust
        assert_eq!(
            config.preferences().expect("readable").refresh_mode,
            "auto"
        );
        assert_eq!(config.preferences().expect("readable").refresh_minutes, 5);
```

Extend `application_preferences_survive_a_round_trip_without_rewriting_the_file`: add `refresh_mode: "manual".into(), refresh_minutes: 30,` to the `preferences` construction, and after `config.set_proxy(...)`:

```rust
        config.set_refresh_mode("manual").expect("refresh mode");
        config.set_refresh_minutes(30).expect("refresh minutes");
```

Add new tests:

```rust
    #[test]
    fn an_unknown_refresh_mode_is_refused() {
        let path = scratch("preferences-refresh-mode");
        std::fs::write(&path, "[refresh]\nmode = \"sometimes\"\n").expect("seeded");
        let config = Config::at(path.clone()).expect("valid TOML");

        assert!(config.preferences().is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refresh_minutes_outside_the_range_are_refused_rather_than_clamped() {
        for (index, wrong) in ["minutes = 0", "minutes = 121", "minutes = -5", "minutes = \"five\""]
            .into_iter()
            .enumerate()
        {
            let path = scratch(&format!("preferences-refresh-minutes-{index}"));
            std::fs::write(&path, format!("[refresh]\n{wrong}\n")).expect("seeded");
            let config = Config::at(path.clone()).expect("valid TOML");

            assert!(
                config.preferences().is_err(),
                "{wrong} must not silently become a pace"
            );
            let _ = std::fs::remove_file(path);
        }
    }
```

- [ ] **Step 2: Verify they fail**

Run: `cargo test -p tidemark-core config::`
Expected: FAIL — `refresh_mode` assertions fail (field reads as default only after Task 1; here `set_refresh_mode` does not exist → compile error, and the refusal tests fail once it stub-compiles).

- [ ] **Step 3: Implement**

Constants beside `PROXY_PORT_KEY`:

```rust
const REFRESH_TABLE: &str = "refresh";
const REFRESH_MODE_KEY: &str = "mode";
const REFRESH_MINUTES_KEY: &str = "minutes";
```

In `preferences()`, after the `proxy_mode` validation block and before `Ok(Preferences {`, add:

```rust
        let refresh_mode = self
            .preference_string(REFRESH_TABLE, REFRESH_MODE_KEY)?
            .unwrap_or(defaults.refresh_mode.as_str())
            .to_owned();
        if !Preferences::valid_refresh_mode(&refresh_mode) {
            return Err(self.invalid_preference(
                REFRESH_TABLE,
                REFRESH_MODE_KEY,
                format!("has unknown value {refresh_mode:?}"),
            ));
        }
        // Present-but-wrong is refused rather than clamped: a `minutes` that silently
        // becomes some other pace is the one thing a user who set it deliberately must
        // never get. Absent means the documented default, which is different.
        let refresh_minutes = match self.preference(REFRESH_TABLE, REFRESH_MINUTES_KEY)? {
            None => defaults.refresh_minutes,
            Some(item) => item
                .as_integer()
                .and_then(|minutes| u32::try_from(minutes).ok())
                .filter(|minutes| Preferences::valid_refresh_minutes(*minutes))
                .ok_or_else(|| {
                    self.invalid_preference(
                        REFRESH_TABLE,
                        REFRESH_MINUTES_KEY,
                        "must be a whole number from 1 to 120".into(),
                    )
                })?,
        };
```

and inside the `Ok(Preferences { .. })` construction, after `proxy_port:`:

```rust
            refresh_mode,
            refresh_minutes,
```

Setters after `set_proxy`:

```rust
    /// Chooses how healthy accounts are paced: by quota zone, or one fixed interval.
    pub fn set_refresh_mode(&mut self, mode: &str) -> Result<(), ConfigError> {
        if !Preferences::valid_refresh_mode(mode) {
            return Err(self.invalid_preference(
                REFRESH_TABLE,
                REFRESH_MODE_KEY,
                format!("has unknown value {mode:?}"),
            ));
        }
        self.set_preference(REFRESH_TABLE, REFRESH_MODE_KEY, value(mode))
    }

    /// Sets the fixed interval Manual mode polls at, in minutes.
    pub fn set_refresh_minutes(&mut self, minutes: u32) -> Result<(), ConfigError> {
        if !Preferences::valid_refresh_minutes(minutes) {
            return Err(self.invalid_preference(
                REFRESH_TABLE,
                REFRESH_MINUTES_KEY,
                format!("must be 1 to 120, not {minutes}"),
            ));
        }
        self.set_preference(REFRESH_TABLE, REFRESH_MINUTES_KEY, value(i64::from(minutes)))
    }
```

- [ ] **Step 4: Verify tests pass**

Run: `cargo test -p tidemark-core`
Expected: PASS (all 50+ core tests, including the untouched ones).

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(config): persist the refresh mode and interval in [refresh]"
```

---

### Task 3: Scheduler — zones and modes; idle deleted

**Files:**
- Modify: `crates/tidemarkd/src/scheduler.rs` (whole file: module doc, constants, `Situation`, `healthy`, tests)
- Modify: `crates/tidemarkd/src/engine.rs` — one construction in `reschedule` only, so the crate still compiles (Task 4 replaces it with the real wiring)

**Interfaces:**
- Consumes: `tidemark_types::{DANGER_AT, WARNING_AT, Preferences, ProviderState}`.
- Produces: `pub enum RefreshMode { Auto, Manual(Duration) }` with `RefreshMode::configured(&Preferences) -> RefreshMode`; `pub const AUTO_BLUE/AUTO_YELLOW/AUTO_RED: Duration`; `Situation { state, failures, retry_after, seconds_to_next_reset, mode, worst_used_percent }` (`worst_used_percent: Option<f64>`); `next_interval(&Situation) -> Duration` unchanged signature. `IDLE`, `IDLE_AFTER_SECS`, `Situation::seconds_since_change` are **deleted**.

- [ ] **Step 1: Rewrite the test module (fails to compile: types missing)**

Replace the helpers and the healthy/idle tests in `mod tests` with:

```rust
    fn auto_with(worst: Option<f64>, reset: Option<i64>) -> Situation {
        Situation {
            state: ProviderState::Ok,
            worst_used_percent: worst,
            seconds_to_next_reset: reset,
            mode: RefreshMode::Auto,
            ..Situation::fresh()
        }
    }

    fn manual(minutes: u64, reset: Option<i64>) -> Situation {
        Situation {
            state: ProviderState::Ok,
            seconds_to_next_reset: reset,
            mode: RefreshMode::Manual(Duration::from_secs(minutes * 60)),
            ..Situation::fresh()
        }
    }

    fn failing(state: ProviderState, failures: u32, retry_after: Option<Duration>) -> Situation {
        Situation {
            state,
            failures,
            retry_after,
            ..Situation::fresh()
        }
    }

    #[test]
    fn the_zones_pace_by_how_much_of_the_quota_is_left() {
        assert_eq!(next_interval(&auto_with(Some(0.0), Some(4 * 3600))), AUTO_BLUE);
        assert_eq!(next_interval(&auto_with(Some(69.9), Some(4 * 3600))), AUTO_BLUE);
        assert_eq!(next_interval(&auto_with(Some(70.0), Some(4 * 3600))), AUTO_YELLOW);
        assert_eq!(next_interval(&auto_with(Some(89.9), Some(4 * 3600))), AUTO_YELLOW);
        assert_eq!(next_interval(&auto_with(Some(90.0), Some(4 * 3600))), AUTO_RED);
        assert_eq!(next_interval(&auto_with(Some(99.9), Some(4 * 3600))), AUTO_RED);
    }

    #[test]
    fn an_exhausted_account_waits_like_a_blue_one() {
        // Nothing can be spent until the reset, and the reset is watched for separately.
        assert_eq!(next_interval(&auto_with(Some(100.0), Some(4 * 3600))), AUTO_BLUE);
        assert_eq!(next_interval(&auto_with(Some(105.0), Some(4 * 3600))), AUTO_BLUE);
    }

    #[test]
    fn an_account_with_no_windows_yet_is_paced_as_blue() {
        assert_eq!(next_interval(&auto_with(None, None)), AUTO_BLUE);
        assert_eq!(next_interval(&Situation::fresh()), AUTO_BLUE);
    }

    #[test]
    fn the_quarter_hour_before_a_reset_beats_every_zone() {
        assert_eq!(next_interval(&auto_with(Some(95.0), Some(14 * 60))), NEAR_RESET);
        assert_eq!(next_interval(&auto_with(Some(50.0), Some(16 * 60))), AUTO_BLUE);
    }

    #[test]
    fn an_overdue_reset_is_still_near_one() {
        // The provider said it would roll over a minute ago and we have not seen it happen
        // yet. Backing off here is how a segment boundary gets lost.
        assert_eq!(next_interval(&auto_with(Some(95.0), Some(-60))), NEAR_RESET);
    }

    #[test]
    fn manual_polls_at_exactly_the_chosen_interval() {
        assert_eq!(
            next_interval(&manual(5, Some(14 * 60))),
            Duration::from_secs(5 * 60),
            "near a reset is Auto's acceleration; Manual is the pace the user picked"
        );
        assert_eq!(next_interval(&manual(1, None)), Duration::from_secs(60));
        assert_eq!(next_interval(&manual(120, None)), Duration::from_secs(120 * 60));
    }

    #[test]
    fn the_mode_the_settings_describe_is_the_mode_the_daemon_runs() {
        let mut preferences = Preferences::default();
        assert_eq!(RefreshMode::configured(&preferences), RefreshMode::Auto);

        preferences.refresh_mode = Preferences::REFRESH_MANUAL.into();
        preferences.refresh_minutes = 30;
        assert_eq!(
            RefreshMode::configured(&preferences),
            RefreshMode::Manual(Duration::from_secs(1800))
        );
    }
```

Keep unchanged: `failures_back_off_exponentially_from_the_baseline`, `our_own_backoff_stops_at_an_hour`, `a_provider_asking_for_longer_than_our_cap_is_obeyed`, `a_provider_asking_for_a_second_does_not_turn_backoff_into_a_hot_loop`, `a_malformed_response_backs_off_rather_than_hammering_a_broken_endpoint`, `a_locked_keyring_is_asked_again_within_the_minute`, `states_only_the_user_can_clear_are_not_retried_in_a_tight_loop`. Delete: `healthy_with`, `nothing_special_is_the_baseline`, `the_quarter_hour_before_a_reset_is_watched_closely`, `an_overdue_reset_is_still_near_one` (old shape), `quota_that_stops_moving_slows_the_polling_down`, `an_idle_account_still_wakes_up_for_its_rollover`, `a_fresh_account_is_not_idle_merely_because_nothing_is_known_about_it`.

- [ ] **Step 2: Verify failure**

Run: `cargo test -p tidemarkd scheduler`
Expected: FAIL — compile errors (`RefreshMode` not found, `AUTO_BLUE` not found, `Situation` has no `mode`).

- [ ] **Step 3: Implement**

New module doc (replaces the current one):

```rust
//! When to poll next.
//!
//! Kept as one pure function over a described [`Situation`], with no clock and no I/O in
//! it, because every interesting case here is one nobody wants to reproduce live: a
//! provider asking for an hour, a window five minutes from rolling over, a keyring that
//! is still locked.
//!
//! The intervals themselves come from `CONTEXT.md` § Polling and are not this module's to
//! reinvent: in Auto the pace follows the quota zones — ten minutes in the blue, five in
//! the yellow, one in the red, ten again once the quota is exhausted — with the last
//! quarter hour before a reset watched at sixty seconds so the rollover is seen the
//! moment it happens; in Manual the user's chosen interval is the whole rule. Failures
//! back off exponentially from the baseline regardless of mode.
```

Imports and mode:

```rust
use std::time::Duration;
use tidemark_types::{DANGER_AT, Preferences, ProviderState, WARNING_AT};

/// How the daemon chooses a healthy account's next interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RefreshMode {
    /// Interval from the worst window's quota zone; the near-reset speedup applies.
    Auto,
    /// One fixed interval, and nothing else — the pace the user picked.
    Manual(Duration),
}

impl RefreshMode {
    /// The mode the settings file describes, for the daemon's construction. The file has
    /// been validated by the time this runs, so an unknown name cannot reach here.
    pub fn configured(preferences: &Preferences) -> Self {
        match preferences.refresh_mode.as_str() {
            Preferences::REFRESH_MANUAL => Self::Manual(Duration::from_secs(
                u64::from(preferences.refresh_minutes) * 60,
            )),
            _ => Self::Auto,
        }
    }
}
```

Constants — keep `BASELINE`, `NEAR_RESET`, `NEAR_RESET_HORIZON_SECS`, `MAX_BACKOFF`, `KEYRING_RETRY`, `USER_ACTION_RETRY` (trim their doc comments of idle references). **Delete `IDLE` and `IDLE_AFTER_SECS`.** Add after `BASELINE`:

```rust
/// Auto interval while the worst window is in the blue zone — under [`WARNING_AT`] used,
/// or exhausted entirely: nothing more can be spent, and the reset is watched for
/// separately by [`NEAR_RESET`].
pub const AUTO_BLUE: Duration = Duration::from_secs(10 * 60);

/// Auto interval while the worst window is between [`WARNING_AT`] and [`DANGER_AT`].
pub const AUTO_YELLOW: Duration = Duration::from_secs(5 * 60);

/// Auto interval while the worst window has passed [`DANGER_AT`] but is not exhausted.
pub const AUTO_RED: Duration = Duration::from_secs(60);
```

`Situation` — delete `seconds_since_change`, add:

```rust
    /// The refresh mode the daemon is running: zones, or the user's fixed interval.
    pub mode: RefreshMode,
    /// How much the account's most-used window had consumed, as of the last good
    /// reading. `None` before there is anything to pace by — which paces as blue.
    pub worst_used_percent: Option<f64>,
```

`Situation::fresh()` fills `mode: RefreshMode::Auto, worst_used_percent: None`.

`healthy` and the zone function (replacing the old `healthy`):

```rust
fn healthy(situation: &Situation) -> Duration {
    match situation.mode {
        RefreshMode::Auto => {
            // Near a reset beats the zone deliberately: a window that rolls over while we
            // are asleep costs a segment boundary — the one event history cannot
            // reconstruct afterwards — and the reset notification is owed the moment the
            // boundary passes, not one zone-interval later.
            if situation
                .seconds_to_next_reset
                .is_some_and(|secs| secs <= NEAR_RESET_HORIZON_SECS)
            {
                return NEAR_RESET;
            }
            zone_interval(situation.worst_used_percent)
        }
        RefreshMode::Manual(interval) => interval,
    }
}

/// The Auto interval for an account whose worst window reads this much used.
///
/// The boundaries are the same constants the bar colours and the notifications use, so
/// all three agree about when a window is worth watching.
fn zone_interval(worst_used_percent: Option<f64>) -> Duration {
    let used = worst_used_percent.unwrap_or(0.0);
    if used >= 100.0 {
        AUTO_BLUE
    } else if used >= DANGER_AT {
        AUTO_RED
    } else if used >= WARNING_AT {
        AUTO_YELLOW
    } else {
        AUTO_BLUE
    }
}
```

- [ ] **Step 4: Keep the crate compiling**

`engine.rs::reschedule` still constructs the old `Situation`. Replace its `seconds_since_change:` line with:

```rust
            mode: scheduler::RefreshMode::Auto,
            worst_used_percent: None,
```

Task 4 replaces this hard-wired pair with the engine's own field and the `worst_used` helper. `last_change_at` becomes write-only dead weight until then — harmless to `cargo test`, gone in Task 4, and Task 8's clippy gate sees only the finished state.

- [ ] **Step 5: Verify the whole daemon crate passes**

Run: `cargo test -p tidemarkd`
Expected: PASS — scheduler tests plus every engine/service test (the near-reset engine test still passes: Auto is hard-wired and near-reset wins at any usage; no engine test asserts the old 5-minute healthy baseline).

- [ ] **Step 6: Commit**

```bash
cargo fmt && git add crates/tidemarkd/src/scheduler.rs crates/tidemarkd/src/engine.rs && git commit -m "feat(scheduler): pace auto polling by quota zones"
```

---

### Task 4: Engine — the mode reaches the loop; idle plumbing removed

**Files:**
- Modify: `crates/tidemarkd/src/engine.rs`, `crates/tidemarkd/src/main.rs:301`

**Interfaces:**
- Consumes: Task 3's `RefreshMode` (incl. `configured`), `Situation { mode, worst_used_percent }`; Task 2's config setters.
- Produces: `Engine::new(accounts, history, secrets, updates, config_path, refresh: scheduler::RefreshMode, notifier)`; `Preference::RefreshMode(String)` and `Preference::RefreshMinutes(u32)`; mode change marks every account due now, minutes change does not.

- [ ] **Step 1: Write the failing engine tests**

Add to `engine.rs` tests, beside the other snapshot helpers:

```rust
    /// Two windows, so a test can put them in different zones.
    fn two_windows(five_hour: f64, weekly: f64, resets_in: i64) -> Snapshot {
        let now = Timestamp::now();
        Snapshot {
            provider: ProviderId::new("fake"),
            account: AccountId::default(),
            captured_at: now,
            windows: vec![
                Window {
                    key: WindowKey::for_length(WindowLength::from_secs(18_000).expect("nonzero")),
                    title: "5 hours".into(),
                    subtitle: None,
                    used_percent: five_hour,
                    resets_at: Some(now.saturating_add_seconds(resets_in)),
                    length: WindowLength::from_secs(18_000),
                },
                Window {
                    key: WindowKey::for_length(
                        WindowLength::from_secs(604_800).expect("nonzero"),
                    ),
                    title: "7 days".into(),
                    subtitle: None,
                    used_percent: weekly,
                    resets_at: Some(now.saturating_add_seconds(7 * 24 * 3600)),
                    length: WindowLength::from_secs(604_800),
                },
            ],
            details: Vec::new(),
        }
    }

    fn with_provider_paced(
        provider: Arc<dyn Provider>,
        refresh: scheduler::RefreshMode,
    ) -> Harness {
        let (tx, rx) = mpsc::channel(64);
        let config_path = std::env::temp_dir().join("tidemark-engine-tests-absent.toml");
        let notices = Arc::new(Recorder::default());
        Harness {
            engine: Engine::new(
                vec![Account::with_client(provider)],
                History::in_memory().expect("an in-memory database opens"),
                unlocked(),
                tx,
                config_path.clone(),
                refresh,
                Arc::clone(&notices) as Arc<dyn Notifier>,
            ),
            updates: rx,
            config_path,
            notices,
        }
    }

    #[tokio::test]
    async fn the_worst_window_sets_the_auto_pace() {
        let mut harness = with_provider(Fake::new(vec![Ok(two_windows(55.0, 95.0, 4 * 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            harness.wait_secs(),
            scheduler::AUTO_RED.as_secs(),
            "a red weekly window is news every minute beside a blue five-hour one"
        );
    }

    #[tokio::test]
    async fn manual_polls_at_the_chosen_interval_whatever_the_zone() {
        let mut harness = with_provider_paced(
            Fake::new(vec![Ok(snapshot(95.0, 4 * 3600))]),
            scheduler::RefreshMode::Manual(Duration::from_secs(15 * 60)),
        );
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            harness.wait_secs(),
            15 * 60,
            "the user picked a pace; neither the zone nor the reset changes it"
        );
    }

    #[tokio::test]
    async fn switching_the_refresh_mode_polls_every_account_now() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(50.0, 4 * 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(harness.wait_secs(), scheduler::AUTO_BLUE.as_secs());

        harness
            .engine
            .set_preference(Preference::RefreshMode("manual".into()))
            .await
            .expect("mode stored");
        assert_eq!(
            harness.wait_secs(),
            0,
            "the new pace is owed an immediate reading, not one old interval later"
        );
        let config = Config::at(harness.config_path.clone()).expect("parses");
        assert_eq!(
            config.preferences().expect("readable").refresh_mode,
            "manual"
        );
    }

    #[tokio::test]
    async fn changing_the_manual_minutes_applies_from_the_next_poll() {
        let path = std::env::temp_dir().join(format!(
            "tidemark-engine-refresh-minutes-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[refresh]\nmode = \"manual\"\nminutes = 10\n").expect("seed");
        let mut harness = harness_with_config(
            vec![Account::with_client(Fake::new(vec![
                Ok(snapshot(50.0, 4 * 3600)),
                Ok(snapshot(50.0, 4 * 3600)),
            ]))],
            path.clone(),
        );
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(harness.wait_secs(), 10 * 60);

        harness
            .engine
            .set_preference(Preference::RefreshMinutes(2))
            .await
            .expect("minutes stored");
        assert!(
            harness.wait_secs() > 0,
            "a spin control must not be able to cause a poll storm"
        );

        harness.poll_again().await;
        assert_eq!(harness.wait_secs(), 2 * 60);
        let _ = std::fs::remove_file(&path);
    }
```

Note: `harness_with_config` must be updated (Step 3) to construct Auto mode, so `changing_the_manual_minutes...` seeds the mode through the file instead — it uses `harness_with_config` whose engine reads `RefreshMode::configured` from the config it is handed:

- [ ] **Step 2: Verify failure**

Run: `cargo test -p tidemarkd engine`
Expected: FAIL — compile errors (`Engine::new` takes 6 args, `Preference` has no `RefreshMode`).

- [ ] **Step 3: Implement**

1. `Preference` enum, after `HistoryRetention`:

```rust
    RefreshMode(String),
    RefreshMinutes(u32),
```

2. `Engine` struct: add `refresh: scheduler::RefreshMode` after `config_path`. `Engine::new` gains `refresh: scheduler::RefreshMode,` between `config_path` and `notifier`, and stores it. Update **every** `Engine::new(` call site to pass the mode:
   - `crates/tidemarkd/src/main.rs:301` — pass `crate::scheduler::RefreshMode::configured(&preferences),` (computed once after `preferences` at line 132).
   - `engine.rs` tests: `Harness::new`, `Harness::configured`, `harness_with_config` → `scheduler::RefreshMode::Auto` (for `harness_with_config`, prefer `Config::at(config.clone()).and_then(|c| c.preferences()).map(|p| scheduler::RefreshMode::configured(&p)).unwrap_or(scheduler::RefreshMode::Auto)` so a seeded `[refresh]` table is honoured); the three inline constructions (`changing_antigravity_usage_source...`, `concurrent_option_and_topology...`, `current_segment_command...`) → `scheduler::RefreshMode::Auto`.

3. `set_preference` — add a `refresh_changed` flag beside `retention_changed`:

```rust
        let refresh_changed = matches!(
            &preference,
            Preference::RefreshMode(_) | Preference::RefreshMinutes(_)
        );
```

add the persistence arms beside the others:

```rust
            Preference::RefreshMode(mode) => config.set_refresh_mode(&mode),
            Preference::RefreshMinutes(minutes) => config.set_refresh_minutes(minutes),
```

and after the `prune_for_retention` block, before `Ok(preferences)`:

```rust
        if refresh_changed {
            self.refresh = scheduler::RefreshMode::configured(&preferences);
            if matches!(preference, Preference::RefreshMode(_)) {
                // A mode switch is owed an immediate reading under the new rules — the
                // `adopt_proxy` precedent. Minutes deliberately do not: a spin control
                // must not be able to cause a poll storm, so the new pace applies from
                // each account's next natural poll.
                let now = Instant::now();
                for account in &mut self.accounts {
                    account.due = now;
                }
            }
        }
```

4. `reschedule` — replace the hard-wired pair Task 3 left in the `Situation` construction with:

```rust
            mode: self.refresh,
            worst_used_percent: worst_used(&account.status.windows),
```

and add beside `soonest_reset`:

```rust
/// The most-used window the account last reported, if it reported any.
///
/// Auto paces by the worst window: a red five-hour window is news every minute even
/// beside a blue weekly one.
fn worst_used(windows: &[WindowStatus]) -> Option<f64> {
    windows
        .iter()
        .map(|window| window.used_percent)
        .reduce(f64::max)
}
```

5. Delete the idle plumbing: `CHANGE_EPSILON`, `Account::last_change_at` (field + the three constructor initialisations), `consumption_moved`, and the trailing `moved`/`last_change_at` block in `record` (keep the `match self.history.ingest(...)` and drop the `let moved = ...` line and everything after the match that references it).

- [ ] **Step 4: Verify**

Run: `cargo test -p tidemarkd`
Expected: PASS — all daemon tests, including the four new ones and the untouched backoff/keyring/near-reset tests (`an_account_polled_close_to_a_reset_comes_back_within_the_minute` still passes: Auto near-reset wins at any usage).

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(engine): schedule by the configured refresh mode"
```

---

### Task 5: D-Bus — `SetRefreshMode` and `SetRefreshMinutes`

**Files:**
- Modify: `crates/tidemarkd/src/service.rs` (interface impl after `set_history_retention` ~944; tests at the end of the tests module)

**Interfaces:**
- Consumes: Task 1 validators, Task 4 `Preference` variants.
- Produces: interface methods `SetRefreshMode(mode: s)`, `SetRefreshMinutes(minutes: u)`; both validate, then queue `Command::SetPreference`, then publish `PreferencesChanged`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn an_unknown_refresh_mode_is_refused_before_the_engine_hears_it() {
        let (daemon, _secrets, mut commands) = daemon_over(Vec::new()).await;
        let Ok(connection) = zbus::Connection::session().await else {
            eprintln!("skipped: no session bus reachable");
            return;
        };
        let emitter = SignalEmitter::new(&connection, ids::OBJECT_PATH).expect("a valid path");

        assert!(matches!(
            daemon.set_refresh_mode(&emitter, "sometimes").await,
            Err(fdo::Error::InvalidArgs(_))
        ));
        assert!(
            commands.try_recv().is_err(),
            "a refused value must not reach the engine"
        );
    }

    #[tokio::test]
    async fn a_refresh_change_reaches_the_engine_and_publishes_the_dict() {
        let (daemon, _secrets, mut commands) = daemon_over(Vec::new()).await;
        let daemon = Arc::new(daemon);
        let Ok(connection) = zbus::Connection::session().await else {
            eprintln!("skipped: no session bus reachable");
            return;
        };
        let emitter = SignalEmitter::new(&connection, ids::OBJECT_PATH).expect("a valid path");

        let changing = {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move { daemon.set_refresh_minutes(&emitter, 30).await })
        };
        let Command::SetPreference { preference, reply } =
            commands.recv().await.expect("the change reaches the engine")
        else {
            panic!("unexpected command");
        };
        assert!(matches!(preference, Preference::RefreshMinutes(30)));
        reply
            .send(Ok(Preferences::default()))
            .expect("caller waits for reply");
        changing.await.expect("task did not panic").expect("accepted");
    }
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p tidemarkd service`
Expected: FAIL — compile error, `no method set_refresh_mode`.

- [ ] **Step 3: Implement**

In `#[interface(name = "io.github.zbndev.Tidemark.Daemon1")] impl Daemon`, after `set_history_retention`:

```rust
    /// Chooses how the daemon paces healthy accounts: by quota zone, or one fixed interval.
    async fn set_refresh_mode(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        mode: &str,
    ) -> fdo::Result<()> {
        if !Preferences::valid_refresh_mode(mode) {
            return Err(fdo::Error::InvalidArgs(format!(
                "unknown refresh mode {mode:?}"
            )));
        }
        let _guard = self.preference_mutation.lock().await;
        let preferences = self
            .preference_request(Preference::RefreshMode(mode.into()))
            .await?;
        self.publish_preferences(&emitter, preferences).await
    }

    /// Sets the fixed interval Manual mode polls at, in minutes.
    async fn set_refresh_minutes(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        minutes: u32,
    ) -> fdo::Result<()> {
        if !Preferences::valid_refresh_minutes(minutes) {
            return Err(fdo::Error::InvalidArgs(format!(
                "refresh minutes must be 1 to 120, not {minutes}"
            )));
        }
        let _guard = self.preference_mutation.lock().await;
        let preferences = self
            .preference_request(Preference::RefreshMinutes(minutes))
            .await?;
        self.publish_preferences(&emitter, preferences).await
    }
```

- [ ] **Step 4: Verify**

Run: `cargo test -p tidemarkd`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(daemon): SetRefreshMode and SetRefreshMinutes on the bus"
```

---

### Task 6: UI — the "Providers refresh" section

**Files:**
- Modify: `crates/tidemark/src/bus.rs` (proxy trait after `set_history_retention` ~122), `crates/tidemark/src/preferences.rs`

**Interfaces:**
- Consumes: Task 5 D-Bus methods; Task 1 `Preferences` fields/consts.
- Produces: proxy methods `set_refresh_mode(&self, &str)`, `set_refresh_minutes(&self, u32)`; dialog rows `refresh_auto: adw::SwitchRow`, `refresh_minutes: adw::SpinRow`.

- [ ] **Step 1: Write the failing UI tests**

In `preferences.rs` tests:

```rust
    #[test]
    fn the_manual_interval_row_follows_the_auto_switch() {
        // Reads the row and not the stored preference, for the same reason the proxy
        // rows do: the switch flips before the daemon has answered, and locking the
        // row against the stored value would leave the setting unreachable mid-change.
        assert!(!manual_refresh_editable(true), "auto decides the pace");
        assert!(manual_refresh_editable(false), "manual needs a pace to read");
    }

    #[test]
    fn a_switch_state_maps_back_to_one_of_the_named_refresh_modes() {
        assert_eq!(refresh_mode_for(true), Preferences::REFRESH_AUTO);
        assert_eq!(refresh_mode_for(false), Preferences::REFRESH_MANUAL);
    }
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p tidemark preferences`
Expected: FAIL — compile error, `manual_refresh_editable` not found.

- [ ] **Step 3: Implement**

`bus.rs` proxy trait, after `set_history_retention`:

```rust
    fn set_refresh_mode(&self, mode: &str) -> zbus::Result<()>;

    fn set_refresh_minutes(&self, minutes: u32) -> zbus::Result<()>;
```

`preferences.rs`:

1. Helpers beside `proxy_rows_editable`:

```rust
/// Whether the manual interval row belongs to the mode the Auto switch shows right now —
/// including the moment between a toggle and the daemon's answer.
fn manual_refresh_editable(auto_active: bool) -> bool {
    !auto_active
}

/// The named mode a switch state commits.
fn refresh_mode_for(auto_active: bool) -> &'static str {
    if auto_active {
        Preferences::REFRESH_AUTO
    } else {
        Preferences::REFRESH_MANUAL
    }
}
```

2. `SwitchKind` gains `RefreshAuto`.

3. In `present`, after the `startup` group is added to `general`:

```rust
        let refresh_auto = adw::SwitchRow::builder()
            .title("Auto")
            .subtitle("Refresh frequency adapts to how much quota is left.")
            .build();
        let refresh_minutes = adw::SpinRow::new(
            Some(&gtk::Adjustment::new(5.0, 1.0, 120.0, 1.0, 10.0, 0.0)),
            1.0,
            0,
        );
        refresh_minutes.set_title("Manual refresh frequency");
        refresh_minutes.set_subtitle("Minutes between polls when Auto is off.");
        let refresh_group = adw::PreferencesGroup::builder().title("Providers refresh").build();
        refresh_group.add(&refresh_auto);
        refresh_group.add(&refresh_minutes);
        general.add(&refresh_group);
```

Store both rows as struct fields; wire `settings.connect_switch(&settings.refresh_auto, SwitchKind::RefreshAuto);` beside the other switch connections and add a `connect_refresh_minutes` call.

4. In `apply`, inside the `suppress` window after `minimize_on_close`:

```rust
        self.refresh_auto
            .set_active(preferences.refresh_mode == Preferences::REFRESH_AUTO);
        self.refresh_minutes
            .set_value(f64::from(preferences.refresh_minutes));
```

and after `self.suppress.set(false);`:

```rust
        self.sync_refresh_editable();
```

5. New methods:

```rust
    /// Whether the manual interval row can be typed into, from what the Auto switch
    /// shows — the row's own state, not the stored preference, for the reason the proxy
    /// rows read theirs: the switch flips before the daemon answers.
    fn sync_refresh_editable(&self) {
        self.refresh_minutes
            .set_sensitive(manual_refresh_editable(self.refresh_auto.is_active()));
    }

    /// Commits the manual interval each time the stepper settles on a value.
    ///
    /// The row is made insensitive for the round trip, which is what bounds the commit
    /// rate: a held stepper button cannot queue a poll per click.
    fn connect_refresh_minutes(self: &Rc<Self>) {
        self.refresh_minutes.connect_value_changed({
            let weak = Rc::downgrade(self);
            move |row| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                if settings.suppress.get() {
                    return;
                }
                let minutes = row.value() as u32;
                row.set_sensitive(false);
                let row = row.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) = settings.proxy.set_refresh_minutes(minutes).await {
                        let preferences = settings.preferences.borrow().clone();
                        let data = settings.data.borrow().clone();
                        settings.apply(&preferences, &data);
                        settings.toast(&error.to_string());
                    } else {
                        settings.preferences.borrow_mut().refresh_minutes = minutes;
                    }
                    settings.sync_refresh_editable();
                });
            }
        });
    }
```

6. `connect_switch` closure: after the `suppress` check, before `change_switch`:

```rust
                if matches!(kind, SwitchKind::RefreshAuto) {
                    settings.sync_refresh_editable();
                }
```

7. `change_switch`: add arms — in the proxy call match:

```rust
                SwitchKind::RefreshAuto => {
                    let mode = refresh_mode_for(enabled);
                    self.proxy.set_refresh_mode(mode).await
                }
```

and in the success match:

```rust
                    SwitchKind::RefreshAuto => {
                        preferences.refresh_mode = refresh_mode_for(enabled).to_owned();
                    }
```

- [ ] **Step 4: Verify**

Run: `cargo test -p tidemark`
Expected: PASS (chart tests and the widget-guarded ones included; headless ones skip cleanly).

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat(ui): providers refresh section in general preferences"
```

---

### Task 7: Docs — the normative record follows the decision

**Files:**
- Modify: `CONTEXT.md` § Polling (lines 377–398), `crates/tidemarkd/src/scheduler.rs` module doc (already rewritten in Task 3 — verify only), `docs/superpowers/specs/2026-08-26-smart-refresh-design.md` (one amendment)

**Interfaces:**
- Consumes: the implemented behaviour.
- Produces: updated normative text.

- [ ] **Step 1: Replace CONTEXT.md § Polling body (between `## Polling` and `## Notifications`)**

```markdown
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
```

- [ ] **Step 2: Amend the spec**

In `docs/superpowers/specs/2026-08-26-smart-refresh-design.md` § Applying a change, replace the sentence beginning "Changing either value persists it, updates the field, and marks every account due immediately" with:

```markdown
Changing the mode persists it, updates the field, and marks every account due immediately
— the `adopt_proxy` precedent: one extra request per account on a rare user action. Changing
the minutes only persists it and updates the field: the new pace applies from each
account's next natural poll, because a spin control must not be able to cause a poll storm.
```

- [ ] **Step 3: Commit**

```bash
git add CONTEXT.md docs/superpowers/specs/2026-08-26-smart-refresh-design.md && git commit -m "docs: polling follows refresh zones in CONTEXT.md"
```

---

### Task 8: Full gate

**Files:** none (verification only).

- [ ] **Step 1: Run the gate exactly as CI does**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
```

Expected: every stage exits 0. If clippy flags the new code, fix and re-run; amend the responsible commit only if it is the HEAD commit, otherwise add a `fixup!` commit and note it for squash at PR time.

- [ ] **Step 2: Report**

Summarize: commits on `feat/smart-refresh`, test counts, and any deviations from the plan (with reasons).
