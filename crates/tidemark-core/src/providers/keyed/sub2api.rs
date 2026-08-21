//! sub2api.
//!
//! Ported from CodexBar's `sub2api.js` plugin (mirrored by its Swift fetcher, whose
//! tests recorded the fixtures). Never seen answering: every number in the tests is a
//! body CodexBar recorded.
//!
//! # A self-hosted relay with no home
//!
//! sub2api is software other people run; there is no default host, so the base URL is
//! a *required* free-text option and [`Keyed::new`](super::Keyed::new) refuses to build
//! without one, naming the setting. The shared reader enforces HTTPS — plain HTTP
//! stands for loopback only, which is how a local instance is reached. `/v1/usage` is
//! appended as the plugin appends it (a host already ending in `/v1` or `/v1/usage` is
//! left alone), with the `days=30` window the recorded request carries. CodexBar also
//! sends the machine's timezone for the server to bucket `usage.today` by; a daemon
//! has no user timezone to speak of, so this port states UTC.
//!
//! # Two ways to be limited
//!
//! A **quota-limited key** carries `quota: {limit, used, remaining, unit}` — a fixed
//! balance against a stated limit, drawn as one window keyed `balance` with both
//! absolutes under the bar. The unit is the root's, the quota's, or USD, in that
//! order; a quota whose limit is not positive draws no window (there is nothing to
//! divide by) and the totals carry the reading.
//!
//! A **subscription** carries daily, weekly and monthly usage *and* limits in USD:
//! three windows of 1, 7 and 30 days. Nothing on the wire says when they reset — the
//! subscription's expiry is a different date — so none of the three carries a pace
//! mark. A subscription and a quota never coexist in the plugin's reading: the
//! subscription wins.
//!
//! `rate_limits` are extra windows named by their span (`5h`, `1d`, `7d`): the three
//! known names map to lengths and titles, an unfamiliar name keeps its own string for
//! a title and gets no length, and `pct`'s rule that a limit of zero or less reads as
//! a full bar is preserved. A `reset_at` that is present but unreadable fails the
//! fetch, as it does in the plugin.
//!
//! A subscription and a `rate_limits` array can name the *same* span — the weekly
//! subscription window beside a `7d` rate limit is two quotas, not one window
//! reported twice — so a span both sections report draws both windows, keyed by pool
//! (`subscription/w604800` beside `rate/w604800`), and a span only one section
//! reports keeps the plain length key. A duplicate the two pools cannot separate —
//! the same span twice inside `rate_limits` — is still refused: one section naming
//! one span twice is a reading this port cannot tell apart.
//!
//! # The rejection that arrives as a 200
//!
//! A revoked or unassigned key answers this endpoint with HTTP 200 and
//! `"isValid": false`. That body is refused as [`ProviderError::Credential`] — the
//! interface asks for a new key instead of reporting an unreadable response.
//!
//! Money is grouped the way the plugin groups it (`$1,296.23 / $2,800.00`), spent
//! amounts keep their cents, and the request/token totals are whole integers or the
//! fetch fails. Fields the plugin only type-checks (`mode`, `status`, `remaining`,
//! each rate limit's `remaining`) are deserialized for the same check and then read
//! by no one, exactly as in the plugin.

use super::{Auth, Method, OptionSchema, Spec, base_url};
use crate::providers::{ProviderError, length_title, parse_rfc3339};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "sub2api";

/// The `rate_limits` names this parser knows a length for; anything else keeps its own
/// name and carries no length.
const KNOWN_RATE_WINDOWS: &[(&str, u64, &str)] = &[
    ("5h", 18_000, "5 hour limit"),
    ("1d", 86_400, "Daily limit"),
    ("7d", 604_800, "7 day limit"),
];

/// Key prefix for the subscription block's windows, when `rate_limits` names the same
/// span: one key per section, so both windows draw. See [`WindowKey::for_pool`].
const SUBSCRIPTION_POOL: &str = "subscription";
/// Key prefix for the `rate_limits` windows under the same circumstances.
const RATE_POOL: &str = "rate";

#[derive(Debug, Deserialize)]
struct Envelope {
    /// The four fields below are type-checked only, as the plugin type-checks them.
    #[serde(default)]
    #[allow(dead_code)]
    mode: Option<String>,
    #[serde(default, rename = "isValid")]
    is_valid: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<String>,
    #[serde(default, rename = "planName")]
    plan_name: Option<String>,
    /// Validated as a number, then read by no one, as in the plugin.
    #[serde(default)]
    #[allow(dead_code)]
    remaining: Option<f64>,
    #[serde(default)]
    balance: Option<f64>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    quota: Option<Quota>,
    #[serde(default)]
    subscription: Option<Subscription>,
    #[serde(default)]
    rate_limits: Option<Vec<RateLimit>>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Quota {
    limit: f64,
    used: f64,
    /// Required on the wire and then read by no one, as in the plugin.
    #[allow(dead_code)]
    remaining: f64,
    #[serde(default)]
    unit: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Subscription {
    #[serde(default)]
    daily_usage_usd: Option<f64>,
    #[serde(default)]
    weekly_usage_usd: Option<f64>,
    #[serde(default)]
    monthly_usage_usd: Option<f64>,
    #[serde(default)]
    daily_limit_usd: Option<f64>,
    #[serde(default)]
    weekly_limit_usd: Option<f64>,
    #[serde(default)]
    monthly_limit_usd: Option<f64>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    window: String,
    limit: f64,
    used: f64,
    /// Validated as a number, then read by no one, as in the plugin.
    #[allow(dead_code)]
    remaining: f64,
    #[serde(default)]
    reset_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    today: Option<Totals>,
    #[serde(default)]
    total: Option<Totals>,
}

#[derive(Debug, Deserialize)]
struct Totals {
    #[serde(default)]
    requests: Option<f64>,
    #[serde(default, rename = "total_tokens")]
    total_tokens: Option<f64>,
    #[serde(default, rename = "actual_cost")]
    actual_cost: Option<f64>,
}

/// The plugin's `ctx.pct`: a percentage clamped at both ends, and a limit of zero or
/// less reads as a full bar rather than a division by zero.
fn pct(used: f64, limit: f64) -> f64 {
    if limit > 0.0 {
        (used / limit * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    }
}

/// An amount of money in the provider's own spelling: USD with a `$` and grouping,
/// anything else as a bare two-decimal figure with its unit.
fn money(value: f64, unit: &str) -> String {
    if unit.eq_ignore_ascii_case("USD") {
        format!("${}", grouped(value, 2))
    } else {
        format!("{value:.2} {unit}")
    }
}

/// A number with thousands separators and a fixed number of decimals, the way the
/// plugin's formatter groups (`1,296.23`, `12,000`, `4`).
fn grouped(value: f64, decimals: usize) -> String {
    let rendered = format!("{value:.decimals$}");
    let (int_part, rest) = rendered.split_once('.').unwrap_or((rendered.as_str(), ""));
    let bytes = int_part.as_bytes();
    let mut grouped = String::with_capacity(int_part.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    if !rest.is_empty() {
        grouped.push('.');
        grouped.push_str(rest);
    }
    grouped
}

/// A whole count, grouped.
fn count(value: i64) -> String {
    let rendered = value.to_string();
    let bytes = rendered.as_bytes();
    let mut grouped = String::with_capacity(rendered.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    grouped
}

/// The `YYYY-MM-DD` a whole-second timestamp falls on, for the expiry row.
fn day_of(at: Timestamp) -> String {
    let date = time::OffsetDateTime::from_unix_timestamp(at.as_unix())
        .expect("a plausible timestamp converts")
        .date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let data: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    // The rejection that arrives as a 200: the interface asks for a new key, not for
    // patience with an unreadable body.
    if data.is_valid == Some(false) {
        return Err(ProviderError::Credential { status: 401 });
    }

    let unit = data
        .unit
        .clone()
        .or_else(|| data.quota.as_ref().and_then(|quota| quota.unit.clone()))
        .unwrap_or_else(|| "USD".to_owned());

    // The subscription block and `rate_limits` are kept apart until their keys are
    // settled, because a span both sections report needs a key per section.
    let mut windows = Vec::new();
    let mut subscription_windows = Vec::new();
    if let Some(subscription) = &data.subscription {
        // The subscription block is authoritative; the `daily_usage` series beside it
        // is read by nobody, and no window is recomputed from it.
        let spans: [(Option<f64>, Option<f64>, u64); 3] = [
            (
                subscription.daily_usage_usd,
                subscription.daily_limit_usd,
                86_400,
            ),
            (
                subscription.weekly_usage_usd,
                subscription.weekly_limit_usd,
                604_800,
            ),
            (
                subscription.monthly_usage_usd,
                subscription.monthly_limit_usd,
                2_592_000,
            ),
        ];
        for (usage, limit, secs) in spans {
            let Some(limit) = limit.filter(|limit| *limit > 0.0) else {
                continue;
            };
            let length = WindowLength::from_secs(secs).expect("a fixed span is not zero");
            subscription_windows.push(Window {
                key: WindowKey::for_length(length),
                title: length_title(length),
                subtitle: Some(format!(
                    "{} / {}",
                    money(usage.unwrap_or(0.0), "USD"),
                    money(limit, "USD")
                )),
                used_percent: pct(usage.unwrap_or(0.0), limit),
                // The wire states no per-window reset; the subscription expiry is a
                // different date, and it rides the rows below.
                resets_at: None,
                length: Some(length),
            });
        }
    } else if let Some(quota) = &data.quota
        && quota.limit > 0.0
    {
        // A balance has no length to key on: it does not roll over, it drains. It
        // cannot contest a span with anything, so it needs no pool.
        windows.push(Window {
            key: WindowKey::named("balance"),
            title: "Balance".to_owned(),
            subtitle: Some(format!(
                "{} / {}",
                money(quota.used, &unit),
                money(quota.limit, &unit)
            )),
            used_percent: pct(quota.used, quota.limit),
            resets_at: None,
            length: None,
        });
    }

    let mut rate_windows = Vec::new();
    for rate in data.rate_limits.iter().flatten() {
        let known = KNOWN_RATE_WINDOWS
            .iter()
            .find(|(name, ..)| *name == rate.window.to_lowercase());
        let length = known.and_then(|(_, secs, _)| WindowLength::from_secs(*secs));
        let key = length.map_or_else(
            || WindowKey::named(&rate.window.to_lowercase()),
            WindowKey::for_length,
        );
        let title = known.map_or_else(
            || format!("{} limit", rate.window),
            |(_, _, title)| (*title).to_owned(),
        );
        let resets_at = match rate.reset_at.as_deref() {
            Some(raw) => parse_rfc3339(raw).map(Some).ok_or_else(|| {
                ProviderError::malformed("rate_limits has an unreadable reset_at")
            })?,
            None => None,
        };
        rate_windows.push(Window {
            key,
            title,
            subtitle: Some(format!(
                "{} / {}",
                money(rate.used, "USD"),
                money(rate.limit, "USD")
            )),
            used_percent: pct(rate.used, rate.limit),
            resets_at,
            length,
        });
    }

    // A span both sections name is keyed by pool on both sides, so the subscription's
    // weekly window and a `7d` rate limit both draw; a span only one section reports
    // keeps the plain length key, recorded bodies unchanged.
    for window in &mut subscription_windows {
        if let Some(length) = window.length
            && rate_windows
                .iter()
                .any(|other| other.length == Some(length))
        {
            window.key = WindowKey::for_pool(SUBSCRIPTION_POOL, length);
        }
    }
    for window in &mut rate_windows {
        if let Some(length) = window.length
            && subscription_windows
                .iter()
                .any(|other| other.length == Some(length))
        {
            window.key = WindowKey::for_pool(RATE_POOL, length);
        }
    }
    windows.append(&mut subscription_windows);
    windows.append(&mut rate_windows);

    // Two windows under one key is a storage hazard: the ingest files the second as
    // stale and drops it silently. The pools above separate the sections; a duplicate
    // that survives them — the same span twice inside `rate_limits`, say — is refused.
    for (index, one) in windows.iter().enumerate() {
        if windows[..index].iter().any(|other| other.key == one.key) {
            return Err(ProviderError::malformed(format!(
                "two windows arrived under the key {}",
                one.key
            )));
        }
    }

    let mut summary = Vec::new();
    if let Some(balance) = data.balance {
        summary.push(DetailRow {
            label: "Balance".to_owned(),
            value: money(balance, &unit),
        });
    }
    for (label, totals) in [
        (
            "Today",
            data.usage.as_ref().and_then(|usage| usage.today.as_ref()),
        ),
        (
            "All time",
            data.usage.as_ref().and_then(|usage| usage.total.as_ref()),
        ),
    ] {
        let Some(totals) = totals else {
            continue;
        };
        // Counts are whole integers, or the fetch fails: a token total of 1200.5 is
        // not a reading, it is a miscount.
        let requests = whole(totals.requests, "requests")?;
        let tokens = whole(totals.total_tokens, "total_tokens")?;
        summary.push(DetailRow {
            label: format!("{label} requests"),
            value: count(requests),
        });
        summary.push(DetailRow {
            label: format!("{label} tokens"),
            value: format!(
                "{} · {}",
                count(tokens),
                money(totals.actual_cost.unwrap_or(0.0), "USD")
            ),
        });
    }

    let mut details = Vec::new();
    let mut plan_rows = Vec::new();
    if let Some(plan) = &data.plan_name {
        plan_rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: plan.clone(),
        });
    }
    let expiry = data
        .subscription
        .as_ref()
        .and_then(|subscription| subscription.expires_at.as_deref())
        .or(data.expires_at.as_deref());
    if let Some(raw) = expiry {
        let at = parse_rfc3339(raw)
            .ok_or_else(|| ProviderError::malformed("expires_at is not a valid date"))?;
        plan_rows.push(DetailRow {
            label: "Expires".to_owned(),
            value: day_of(at),
        });
    }
    if !plan_rows.is_empty() {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: plan_rows,
        });
    }
    if !summary.is_empty() {
        details.push(DetailSection {
            title: "Usage summary".to_owned(),
            rows: summary,
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
}

/// A count the plugin insists is an integer.
fn whole(value: Option<f64>, field: &str) -> Result<i64, ProviderError> {
    let value = value.unwrap_or(0.0);
    if value.fract() == 0.0 {
        Ok(value as i64)
    } else {
        Err(ProviderError::malformed(format!(
            "{field} counts must be integers"
        )))
    }
}

/// sub2api as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "sub2api",
    endpoint: |options| {
        let mut base =
            base_url(options, "base_url", "").expect("a required option was checked at build time");
        if !(base.ends_with("/v1") || base.ends_with("/v1/usage")) {
            base.push_str("/v1");
        }
        if !base.ends_with("/usage") {
            base.push_str("/usage");
        }
        // The 30-day window is the request CodexBar recorded. CodexBar also sends the
        // machine's timezone for server-side bucketing; a daemon has none to state.
        format!("{base}?days=30&timezone=UTC")
    },
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "sub2api group page → API keys.",
    options: &[OptionSchema {
        name: "base_url",
        title: "Base URL",
        description: Some("Host of the sub2api instance to poll; HTTPS, or HTTP on loopback."),
        default: "",
        choices: &[],
        required: true,
    }],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use crate::providers::keyed::Options;
    use tidemark_types::{DetailRow, DetailSection, Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `Sub2APIPluginGoldenTests.swift` and
    /// `Sub2APIUsageFetcherTests.swift` — "quota limited fixture matches the production
    /// golden". CodexBar asserts the 25% quota window, two extra rate windows with the
    /// 5h one at 300 minutes, the grouped token counts, and the expiry.
    const QUOTA_LIMITED: &str = r#"
        {
          "mode": "quota_limited",
          "isValid": true,
          "status": "active",
          "remaining": 75,
          "unit": "USD",
          "quota": { "limit": 100, "used": 25, "remaining": 75, "unit": "USD" },
          "rate_limits": [
            { "window": "5h", "limit": 20, "used": 5, "remaining": 15,
              "reset_at": "2026-07-11T12:30:00Z" },
            { "window": "7d", "limit": 200, "used": 40, "remaining": 160 }
          ],
          "expires_at": "2026-08-01T00:00:00Z",
          "usage": {
            "today": { "requests": 4, "total_tokens": 1200, "actual_cost": 1.25 },
            "total": { "requests": 40, "total_tokens": 12000, "actual_cost": 25 }
          }
        }
        "#;

    /// Recorded by CodexBar, `Sub2APIPluginGoldenTests.swift` — "subscription windows
    /// remain authoritative and grouped". CodexBar asserts 100%, 229.20/700, and
    /// 1296.23/2800 with the grouped subtitles. The `daily_usage` series beside them
    /// is read by nobody — the windows come from the subscription block alone.
    const SUBSCRIPTION: &str = r#"
        {
          "mode": "unrestricted",
          "planName": "Claude Team",
          "subscription": {
            "daily_usage_usd": 120.23,
            "weekly_usage_usd": 229.20,
            "monthly_usage_usd": 1296.23,
            "daily_limit_usd": 120,
            "weekly_limit_usd": 700,
            "monthly_limit_usd": 2800,
            "expires_at": "2026-08-15T00:00:00.123Z"
          },
          "daily_usage": [{ "date": "2026-07-05", "actual_cost": 229.20 }]
        }
        "#;

    /// Recorded by CodexBar, `Sub2APIUsageFetcherTests.swift` — "does not reinterpret
    /// subscription windows as local calendar periods". Usage above both daily and
    /// weekly limits clamps to full bars.
    const OVER_LIMIT: &str = r#"
        {
          "mode": "unrestricted",
          "subscription": {
            "daily_usage_usd": 99,
            "weekly_usage_usd": 99,
            "monthly_usage_usd": 30,
            "daily_limit_usd": 10,
            "weekly_limit_usd": 40,
            "monthly_limit_usd": 100
          },
          "daily_usage": [
            { "date": "2026-07-05", "actual_cost": 50 },
            { "date": "2026-07-06", "actual_cost": 4 },
            { "date": "2026-07-08", "actual_cost": 2 }
          ]
        }
        "#;

    /// Recorded by CodexBar, `Sub2APIUsageFetcherTests.swift` — "parses unrestricted
    /// wallet balance". No window at all; the balance is a row.
    const WALLET: &str = r#"
        {
          "mode": "unrestricted",
          "isValid": true,
          "planName": "Wallet plan",
          "remaining": 42.5,
          "unit": "USD",
          "balance": 42.5
        }
        "#;

    /// Recorded by CodexBar, `Sub2APIMenuCardModelTests.swift` — "subscription amounts
    /// share the percentage row". Whole-dollar amounts against a four-figure limit.
    const WHOLE_DOLLARS: &str = r#"
        {
          "mode": "unrestricted",
          "subscription": {
            "daily_usage_usd": 12,
            "weekly_usage_usd": 70,
            "monthly_usage_usd": 280,
            "daily_limit_usd": 120,
            "weekly_limit_usd": 700,
            "monthly_limit_usd": 2800
          }
        }
        "#;

    /// Recorded by CodexBar, `Sub2APIMenuCardModelTests.swift` — "extra window amount
    /// renders as detail instead of reset". A rate window with no quota and no
    /// subscription, and no reset on the wire.
    const RATE_ONLY: &str = r#"
        {
          "mode": "quota_limited",
          "rate_limits": [
            {
              "window": "7d",
              "limit": 200,
              "used": 40,
              "remaining": 160
            }
          ]
        }
        "#;

    /// Recorded by CodexBar, `Sub2APIUsageFetcherTests.swift` and
    /// `Sub2APIPluginGoldenTests.swift` — "fetch rejects invalid key in successful
    /// response": the rejection arrives in the body of a 200.
    const REJECTED_KEY: &str = r#"{"mode":"unrestricted","isValid":false}"#;

    /// Spliced, not recorded: the subscription body above carrying the `rate_limits`
    /// array of the quota body above. Each shape is individually real, so a body with
    /// both is plausible — and there, the subscription's weekly span and the `7d` rate
    /// limit are two quotas under one span. Every number is one a fixture recorded.
    const SUBSCRIPTION_WITH_RATES: &str = r#"
        {
          "mode": "unrestricted",
          "planName": "Claude Team",
          "subscription": {
            "daily_usage_usd": 120.23,
            "weekly_usage_usd": 229.20,
            "monthly_usage_usd": 1296.23,
            "daily_limit_usd": 120,
            "weekly_limit_usd": 700,
            "monthly_limit_usd": 2800,
            "expires_at": "2026-08-15T00:00:00.123Z"
          },
          "rate_limits": [
            { "window": "5h", "limit": 20, "used": 5, "remaining": 15,
              "reset_at": "2026-07-11T12:30:00Z" },
            { "window": "7d", "limit": 200, "used": 40, "remaining": 160 }
          ],
          "daily_usage": [{ "date": "2026-07-05", "actual_cost": 229.20 }]
        }
        "#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn window<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|w| w.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    fn section<'a>(snapshot: &'a Snapshot, title: &str) -> &'a DetailSection {
        snapshot
            .details
            .iter()
            .find(|section| section.title == title)
            .unwrap_or_else(|| panic!("no section {title} in {:?}", snapshot.details))
    }

    fn row<'a>(snapshot: &'a Snapshot, in_section: &str, label: &str) -> &'a DetailRow {
        let found = section(snapshot, in_section);
        found
            .rows
            .iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no row {label} in {in_section}"))
    }

    #[test]
    fn the_quota_fixture_draws_the_balance_window_and_both_rate_windows() {
        let snapshot = parse(QUOTA_LIMITED, at(1_785_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["balance", "w18000", "w604800"]);

        let balance = window(&snapshot, "balance");
        assert_eq!(balance.used_percent, 25.0, "used 25 of limit 100");
        assert_eq!(balance.subtitle.as_deref(), Some("$25.00 / $100.00"));
        assert_eq!(balance.length, None, "a balance has no length to key on");
        assert_eq!(balance.resets_at, None);

        let five_hours = window(&snapshot, "w18000");
        assert_eq!(five_hours.title, "5 hour limit");
        assert_eq!(five_hours.used_percent, 25.0, "used 5 of limit 20");
        assert_eq!(five_hours.subtitle.as_deref(), Some("$5.00 / $20.00"));
        assert_eq!(
            five_hours.resets_at,
            Some(at(1_783_773_000)),
            "2026-07-11T12:30:00Z, the reset CodexBar's own test reads"
        );
        assert_eq!(
            five_hours.length.expect("5h maps to minutes").as_secs(),
            18_000
        );

        let weekly = window(&snapshot, "w604800");
        assert_eq!(weekly.title, "7 day limit");
        assert_eq!(weekly.used_percent, 20.0, "used 40 of limit 200");
        assert_eq!(weekly.subtitle.as_deref(), Some("$40.00 / $200.00"));
        assert_eq!(
            weekly.resets_at, None,
            "this entry states no reset, and none is invented"
        );

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "w18000",
            "the card leads with the five-hour limit"
        );
    }

    #[test]
    fn the_quota_fixture_carries_the_summary_rows_and_the_expiry() {
        let snapshot = parse(QUOTA_LIMITED, at(1_785_000_000)).expect("parses");
        assert_eq!(
            row(&snapshot, "Usage summary", "Today requests").value,
            "4",
            "the numbers are grouped the way CodexBar groups them"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "Today tokens").value,
            "1,200 · $1.25"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "All time requests").value,
            "40"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "All time tokens").value,
            "12,000 · $25.00"
        );
        assert!(
            section(&snapshot, "Usage summary")
                .rows
                .iter()
                .all(|row| row.label != "Balance"),
            "no balance key on the wire, no balance row"
        );
        assert_eq!(row(&snapshot, "Plan", "Expires").value, "2026-08-01");
    }

    #[test]
    fn the_subscription_fixture_draws_three_windows_from_the_block_alone() {
        let snapshot = parse(SUBSCRIPTION, at(1_720_440_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["w86400", "w604800", "w2592000"]);

        let daily = window(&snapshot, "w86400");
        assert_eq!(daily.title, "1 day");
        assert_eq!(
            daily.used_percent, 100.0,
            "120.23 spent of a 120 limit clamps to a full bar"
        );
        assert_eq!(daily.subtitle.as_deref(), Some("$120.23 / $120.00"));
        assert_eq!(daily.resets_at, None, "the wire states no per-window reset");

        let weekly = window(&snapshot, "w604800");
        assert_eq!(weekly.used_percent, 229.20 / 700.0 * 100.0);
        assert_eq!(weekly.subtitle.as_deref(), Some("$229.20 / $700.00"));

        let monthly = window(&snapshot, "w2592000");
        assert_eq!(monthly.used_percent, 1296.23 / 2800.0 * 100.0);
        assert_eq!(
            monthly.subtitle.as_deref(),
            Some("$1,296.23 / $2,800.00"),
            "four-figure amounts are grouped, as CodexBar's own golden spells them"
        );

        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Claude Team");
        assert_eq!(row(&snapshot, "Plan", "Expires").value, "2026-08-15");
    }

    #[test]
    fn usage_above_two_limits_fills_both_bars() {
        let snapshot = parse(OVER_LIMIT, at(1_720_440_000)).expect("parses");
        assert_eq!(window(&snapshot, "w86400").used_percent, 100.0);
        assert_eq!(window(&snapshot, "w604800").used_percent, 100.0);
        assert_eq!(window(&snapshot, "w2592000").used_percent, 30.0);
        assert_eq!(
            window(&snapshot, "w86400").subtitle.as_deref(),
            Some("$99.00 / $10.00")
        );
    }

    #[test]
    fn a_wallet_balance_is_a_row_not_a_window() {
        let snapshot = parse(WALLET, at(1_785_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "no quota and no subscription: nothing to divide by"
        );
        assert_eq!(row(&snapshot, "Usage summary", "Balance").value, "$42.50");
        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Wallet plan");
    }

    #[test]
    fn whole_dollar_amounts_read_plainly() {
        let snapshot = parse(WHOLE_DOLLARS, at(1_720_440_000)).expect("parses");
        assert_eq!(
            window(&snapshot, "w86400").subtitle.as_deref(),
            Some("$12.00 / $120.00")
        );
        assert_eq!(
            window(&snapshot, "w604800").subtitle.as_deref(),
            Some("$70.00 / $700.00")
        );
        assert_eq!(
            window(&snapshot, "w2592000").subtitle.as_deref(),
            Some("$280.00 / $2,800.00")
        );
    }

    #[test]
    fn a_rate_window_alone_still_draws() {
        let snapshot = parse(RATE_ONLY, at(1_720_440_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        let weekly = window(&snapshot, "w604800");
        assert_eq!(weekly.title, "7 day limit");
        assert_eq!(weekly.used_percent, 20.0);
        assert_eq!(weekly.subtitle.as_deref(), Some("$40.00 / $200.00"));
    }

    #[test]
    fn a_span_both_sections_report_draws_both_windows_keyed_by_pool() {
        let snapshot = parse(SUBSCRIPTION_WITH_RATES, at(1_720_440_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "w86400",
                "subscription/w604800",
                "w2592000",
                "w18000",
                "rate/w604800",
            ],
            "only the contested weekly span takes a pool; the rest keep length keys"
        );

        let subscribed = window(&snapshot, "subscription/w604800");
        assert_eq!(subscribed.title, "7 days");
        assert_eq!(subscribed.used_percent, 229.20 / 700.0 * 100.0);
        assert_eq!(subscribed.subtitle.as_deref(), Some("$229.20 / $700.00"));

        let rated = window(&snapshot, "rate/w604800");
        assert_eq!(rated.title, "7 day limit");
        assert_eq!(rated.used_percent, 20.0, "used 40 of limit 200");
        assert_eq!(rated.subtitle.as_deref(), Some("$40.00 / $200.00"));

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "w18000",
            "the card still leads with the five-hour limit beside the pooled windows"
        );
    }

    #[test]
    fn the_same_span_twice_inside_rate_limits_is_still_refused() {
        // The recorded `7d` entry standing twice: one section naming one span twice is
        // a reading this port cannot tell apart, so no pool separates it.
        let body = r#"
        { "rate_limits": [
            { "window": "7d", "limit": 200, "used": 40, "remaining": 160 },
            { "window": "7d", "limit": 200, "used": 40, "remaining": 160 }
        ] }
        "#;
        let error = parse(body, at(1_785_000_000)).expect_err("contested within one section");
        assert!(
            matches!(error, ProviderError::Malformed(_)),
            "{error} for {body}"
        );
    }

    #[test]
    fn a_rejected_key_in_a_200_body_asks_for_a_new_key() {
        // The body CodexBar's own test uses for the rejected key, arriving with a
        // successful status: the interface must ask for a new key, not report an
        // unreadable response.
        let error = parse(REJECTED_KEY, at(1_785_000_000)).expect_err("rejected");
        assert!(
            matches!(error, ProviderError::Credential { status: 401 }),
            "{error}"
        );
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // `{"quota":{"limit":"many"}}` is the malformed body CodexBar recorded; the
        // truncated envelope is the procedure's own; a non-integer request count and
        // an unreadable expiry date fail the plugin's own checks.
        let fractional_count = r#"
        { "usage": { "today": { "requests": 4.5, "total_tokens": 10, "actual_cost": 0 } } }
        "#;
        let bad_date = r#"{ "expires_at": "next Tuesday" }"#;
        for body in [
            "not-json",
            "{\"partial\":",
            r#"{"quota":{"limit":"many"}}"#,
            fractional_count,
            bad_date,
        ] {
            let error = parse(body, at(1_785_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_field_of_a_kind_this_parser_does_not_read_is_skipped() {
        // The recorded subscription body already carries `daily_usage`, a series the
        // plugin never reads; the windows come from the subscription block. One more
        // invented field is skipped the same way.
        let body = SUBSCRIPTION.replacen(
            "\"daily_usage\":",
            "\"alerts\": {\"kind\": \"daily\"}, \"daily_usage\":",
            1,
        );
        let snapshot = parse(&body, at(1_720_440_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(
            window(&snapshot, "w604800").used_percent,
            229.20 / 700.0 * 100.0
        );
    }

    fn endpoint(base: &str) -> String {
        (SPEC.endpoint)(&Options::from([("base_url".to_owned(), base.to_owned())]))
    }

    #[test]
    fn the_required_base_url_resolves_the_v1_usage_path() {
        // CodexBar's own request test: a bare host, one already carrying /v1, and one
        // already complete all reach the same path.
        assert_eq!(
            endpoint("https://api.example.com"),
            "https://api.example.com/v1/usage?days=30&timezone=UTC"
        );
        assert_eq!(
            endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/usage?days=30&timezone=UTC"
        );
        assert_eq!(
            endpoint("https://api.example.com/v1/usage"),
            "https://api.example.com/v1/usage?days=30&timezone=UTC"
        );
        assert_eq!(
            endpoint("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1/usage?days=30&timezone=UTC",
            "loopback HTTP is how a self-hosted sub2api is reached"
        );
    }

    #[test]
    fn the_spec_polls_with_a_bearer_key_and_requires_the_base_url() {
        use crate::providers::keyed::{Auth, Method};
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "sub2api");
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.options.len(), 1);
        let option = &SPEC.options[0];
        assert_eq!(option.name, "base_url");
        assert!(
            option.required,
            "no default host exists; the mechanism refuses to build without one"
        );
        assert!(option.choices.is_empty(), "free text");
    }
}
