//! Synthetic.
//!
//! Ported from CodexBar's `synthetic.js` plugin. Never seen answering: every number in
//! the tests is a body CodexBar recorded.
//!
//! # A payload with no fixed spelling
//!
//! This endpoint describes its quotas with a *list* of alternative field names for
//! everything: the limit may be `limit`, `max`, `quota`, `message_limit`, …; the reset
//! may be `reset_at`, `renewsAt`, `nextTickAt`, …; every number may arrive quoted; a
//! percent at or below 1 is a fraction and is scaled by 100; a date may be ISO-8601 or
//! an epoch in seconds or milliseconds; a credit amount may carry `$` and thousands
//! separators. All of that is ported as written — the key lists are the plugin's own,
//! in its order, first match wins.
//!
//! The root may be an array (a `quotas` list with the wrapper omitted) or an object.
//! Three *named slots* take precedence when present — `rollingFiveHourLimit`,
//! `weeklyTokenLimit`, `search.hourly`, on the root or under `data` — and only they
//! are read. Otherwise the first of the candidate keys (`quotas`, `quota`, `limits`,
//! `usage`, `entries`, `subscription`, `data`, …) that yields anything is walked
//! recursively, descending into non-quota objects in sorted key order. None of the
//! named slots states a length on the wire — the rolling window's "five hours" is its
//! name, not a measurement — so the slot windows are keyed by name and carry no
//! length; the generic entries are keyed by their own label, or by their length when
//! the entry states one (`window_minutes` and its spellings).
//!
//! # What is a window and what is a row
//!
//! Each quota entry becomes one window, its percentage read directly when a percent
//! field is present (`100 - percentRemaining` when only that is) or derived from
//! limit/used/remaining with the cross-fill the plugin performs (any one of the three
//! is computed from the other two; `used` then `limit` win when explicit). Derived
//! windows carry both absolutes under the bar; percent-only windows carry none.
//!
//! `maxCredits` is a quantity against a stated limit — the weekly credit allowance —
//! so it is a balance window (`$0.70 / $36.00`), with the first entry carrying it
//! deciding. The regeneration rates the payload reports (`tickPercent`,
//! `nextRegenCredits`) have no analogue in the window model and become rows, so a fact
//! CodexBar renders as "Full in ~25 regens" is not dropped on the floor here.
//!
//! # Where this port is stricter than the plugin
//!
//! The plugin silently drops a quota-shaped entry whose percentage cannot be derived;
//! this port refuses the whole fetch for one, per the workspace rule that a recognised
//! entry that cannot be parsed is never a silent absence. An entry no key list
//! recognises as a quota is still skipped, as in the plugin.
//!
//! An unreadable date string is *not* a refusal here: the plugin catches and discards
//! it, so the window draws with no pace mark. Sub-second resets floor to the second,
//! because a whole-second timestamp cannot carry them.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, length_title, parse_rfc3339};
use serde_json::Value;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "synthetic";

const QUOTAS_URL: &str = "https://api.synthetic.new/v2/quotas";

/// The label keys, in the plugin's order.
const LABEL_KEYS: &[&str] = &["name", "label", "type", "period", "scope", "title", "id"];
/// The percent-used keys, in the plugin's order.
const PERCENT_USED_KEYS: &[&str] = &[
    "percentUsed",
    "usedPercent",
    "usagePercent",
    "usage_percent",
    "used_percent",
    "percent_used",
    "percent",
];
/// The percent-remaining keys, in the plugin's order.
const PERCENT_REMAINING_KEYS: &[&str] = &[
    "percentRemaining",
    "remainingPercent",
    "remaining_percent",
    "percent_remaining",
];
/// The limit keys, in the plugin's order.
const LIMIT_KEYS: &[&str] = &[
    "limit",
    "messageLimit",
    "message_limit",
    "messages",
    "maxRequests",
    "max_requests",
    "requestLimit",
    "request_limit",
    "quota",
    "max",
    "total",
    "capacity",
    "allowance",
];
/// The used keys, in the plugin's order.
const USED_KEYS: &[&str] = &[
    "used",
    "usage",
    "usedMessages",
    "used_messages",
    "messagesUsed",
    "messages_used",
    "requests",
    "requestCount",
    "request_count",
    "consumed",
    "spent",
];
/// The remaining keys, in the plugin's order.
const REMAINING_KEYS: &[&str] = &["remaining", "left", "available", "balance"];
/// The reset keys, in the plugin's order.
const RESET_KEYS: &[&str] = &[
    "resetAt",
    "reset_at",
    "resetsAt",
    "resets_at",
    "renewAt",
    "renew_at",
    "renewsAt",
    "renews_at",
    "nextTickAt",
    "next_tick_at",
    "nextRegenAt",
    "next_regen_at",
    "periodEnd",
    "period_end",
    "expiresAt",
    "expires_at",
    "endAt",
    "end_at",
];
/// The plan keys, in the plugin's order.
const PLAN_KEYS: &[&str] = &[
    "plan",
    "planName",
    "plan_name",
    "subscription",
    "subscriptionPlan",
    "tier",
    "package",
    "packageName",
];

/// The named slots, in the order the plugin reads them: (root path, key, label).
const SLOTS: &[(&[&str], &str, &str)] = &[
    (
        &["rollingFiveHourLimit"],
        "rolling-five-hour",
        "Rolling five-hour limit",
    ),
    (&["weeklyTokenLimit"], "weekly-token", "Weekly token limit"),
    (&["search", "hourly"], "search-hourly", "Search hourly"),
];

/// The candidate keys the generic walk tries, on the root and then under `data`.
const CONTAINER_KEYS: &[&str] = &[
    "quotas",
    "quota",
    "limits",
    "usage",
    "entries",
    "subscription",
    "data",
];

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let json: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    // An array root is a quotas list with the wrapper omitted.
    let wrapped;
    let root = match json {
        Value::Object(map) => Value::Object(map),
        Value::Array(entries) => {
            wrapped = serde_json::json!({ "quotas": Value::Array(entries) });
            wrapped
        }
        other => {
            return Err(ProviderError::malformed(format!(
                "expected an object or array, found {}",
                json_kind(&other)
            )));
        }
    };
    let data = root.get("data").filter(|data| data.is_object());

    // The named slots, when any of them is present, are the whole reading.
    let mut parsed: Vec<Quota> = Vec::new();
    let mut any_slot = false;
    for (path, key, label) in SLOTS {
        let candidate = value_at(&root, path)
            .filter(|entry| is_quota(entry))
            .or_else(|| {
                data.and_then(|data| value_at(data, path))
                    .filter(|entry| is_quota(entry))
            });
        if let Some(entry) = candidate {
            any_slot = true;
            let mut quota = quota(entry).map_err(ProviderError::Malformed)?;
            quota.key = (*key).to_owned();
            quota.title = (*label).to_owned();
            parsed.push(quota);
        }
    }

    if !any_slot {
        for entry in generic_entries(&root, data) {
            let quota = quota(&entry).map_err(ProviderError::Malformed)?;
            // The entry's own label is its only stable identity unless it states a
            // length; a window with neither has nothing honest to be keyed on.
            if let Some(label) = first_string(&entry, LABEL_KEYS) {
                parsed.push(Quota {
                    key: label.to_lowercase(),
                    title: label,
                    ..quota
                });
            } else if let Some(length) = quota.window.length {
                parsed.push(Quota {
                    key: format!("w{}", length.as_secs()),
                    title: length_title(length),
                    ..quota
                });
            } else {
                return Err(ProviderError::malformed(
                    "a quota entry states no label and no length to key its window on",
                ));
            }
        }
    }
    if parsed.is_empty() {
        return Err(ProviderError::malformed(
            "the response carries no quota data at all",
        ));
    }

    // Two windows under one key is a storage hazard, not a drawing one: the ingest would
    // file the second under stale and drop it silently. Refusing is the honest answer.
    for (index, one) in parsed.iter().enumerate() {
        if parsed[..index].iter().any(|other| other.key == one.key) {
            return Err(ProviderError::malformed(format!(
                "two windows arrived under the key {}",
                one.key
            )));
        }
    }

    let mut windows: Vec<Window> = parsed
        .iter()
        .map(|quota| Window {
            key: WindowKey::named(&quota.key),
            title: quota.title.clone(),
            ..quota.window.clone()
        })
        .collect();
    let mut regen_rows = Vec::new();
    for quota in &parsed {
        if let Some(tick) = quota.tick_percent {
            regen_rows.push(DetailRow {
                label: quota.title.clone(),
                value: format!("+{} per tick", percent_text(tick)),
            });
        }
    }

    // `maxCredits` is a fixed balance: the first entry carrying it decides.
    if let Some(carrier) = parsed.iter().find(|quota| quota.cost.is_some()) {
        let cost = carrier.cost.as_ref().expect("the find checked for one");
        if let Some(regen) = cost.regen {
            regen_rows.push(DetailRow {
                label: "Weekly credits".to_owned(),
                value: format!("+${regen:.2} per regen"),
            });
        }
        // A balance has no length to key on: it does not roll over, it drains.
        windows.push(Window {
            key: WindowKey::named("balance"),
            title: "Weekly credits".to_owned(),
            subtitle: Some(format!("${:.2} / ${:.2}", cost.used, cost.limit)),
            used_percent: (cost.used / cost.limit * 100.0).clamp(0.0, 100.0),
            resets_at: carrier.window.resets_at,
            length: None,
        });
    }

    let mut details = Vec::new();
    if let Some(plan) = first_string(&root, PLAN_KEYS)
        .or_else(|| data.and_then(|data| first_string(data, PLAN_KEYS)))
    {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: plan,
            }],
        });
    }
    if !regen_rows.is_empty() {
        details.push(DetailSection {
            title: "Regeneration".to_owned(),
            rows: regen_rows,
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

/// One parsed quota: its window, and the rates and balances that ride beside it.
#[derive(Debug)]
struct Quota {
    key: String,
    title: String,
    window: Window,
    /// `tickPercent`, scaled — the share of the window restored per tick.
    tick_percent: Option<f64>,
    /// `maxCredits` and friends: the weekly credit allowance.
    cost: Option<Cost>,
}

/// The credit allowance beside a quota.
#[derive(Debug)]
struct Cost {
    used: f64,
    limit: f64,
    /// `nextRegenCredits`.
    regen: Option<f64>,
}

/// Reads one quota entry into a [`Quota`] with a placeholder key and title.
fn quota(entry: &Value) -> Result<Quota, String> {
    // A percentage read directly, when any percent field is present.
    let direct = normalized(first_number(entry, PERCENT_USED_KEYS)).or_else(|| {
        normalized(first_number(entry, PERCENT_REMAINING_KEYS)).map(|remaining| 100.0 - remaining)
    });

    let (used_percent, absolutes) = match direct {
        Some(percent) => (percent.clamp(0.0, 100.0), None),
        None => {
            let mut limit = first_number(entry, LIMIT_KEYS);
            let mut used = first_number(entry, USED_KEYS);
            let remaining = first_number(entry, REMAINING_KEYS);
            // The cross-fill the plugin performs: any one of the three is computed
            // from the other two. Only `limit` and `used` feed the percentage, so
            // only those two fills are computed here.
            if limit.is_none() && used.is_some() && remaining.is_some() {
                limit = used
                    .zip(remaining)
                    .map(|(used, remaining)| used + remaining);
            }
            if used.is_none() && limit.is_some() && remaining.is_some() {
                used = limit
                    .zip(remaining)
                    .map(|(limit, remaining)| limit - remaining);
            }
            match (limit, used) {
                (Some(limit), Some(used)) if limit > 0.0 => (
                    (used / limit * 100.0).clamp(0.0, 100.0),
                    Some(format!("{used:.0} of {limit:.0}")),
                ),
                _ => {
                    return Err(format!(
                        "a quota entry ({}) states no percentage and no limit to divide by",
                        first_string(entry, LABEL_KEYS).unwrap_or_else(|| "unnamed".to_owned())
                    ));
                }
            }
        }
    };

    let minutes = window_minutes(entry);
    let length = minutes
        .filter(|minutes| *minutes > 0.0)
        .and_then(|minutes| WindowLength::from_secs((minutes * 60.0).round() as u64));
    let resets_at = first_date(entry, RESET_KEYS);

    let tick_percent = normalized(first_number(
        entry,
        &[
            "tickPercent",
            "tick_percent",
            "nextTickPercent",
            "next_tick_percent",
        ],
    ));

    let cost = first_currency(entry, &["maxCredits", "max_credits"]).map(|limit| {
        let remaining = first_currency(entry, &["remainingCredits", "remaining_credits"]);
        let explicit = first_currency(entry, &["usedCredits", "used_credits"]);
        let used = explicit
            .or_else(|| remaining.map(|remaining| (limit - remaining).max(0.0)))
            .unwrap_or(used_percent / 100.0 * limit);
        Cost {
            used,
            limit,
            regen: first_currency(entry, &["nextRegenCredits", "next_regen_credits"]),
        }
    });

    Ok(Quota {
        key: String::new(),
        title: String::new(),
        window: Window {
            key: WindowKey::named(""),
            title: String::new(),
            subtitle: absolutes,
            used_percent,
            resets_at,
            length,
        },
        tick_percent,
        cost,
    })
}

/// True when any of the quota key families finds a number — the plugin's `isQuota`.
fn is_quota(entry: &Value) -> bool {
    [
        LIMIT_KEYS,
        USED_KEYS,
        REMAINING_KEYS,
        PERCENT_USED_KEYS,
        PERCENT_REMAINING_KEYS,
    ]
    .iter()
    .any(|keys| first_number(entry, keys).is_some())
}

/// The first candidate container that yields entries, walked recursively as the plugin
/// walks it: quota-shaped objects collect themselves, everything else descends in
/// sorted key order.
fn generic_entries(root: &Value, data: Option<&Value>) -> Vec<Value> {
    let mut candidates: Vec<&Value> = Vec::new();
    for key in CONTAINER_KEYS {
        if let Some(candidate) = root.get(*key) {
            candidates.push(candidate);
        }
    }
    if let Some(data) = data {
        for key in &CONTAINER_KEYS[..CONTAINER_KEYS.len() - 1] {
            if let Some(candidate) = data.get(*key) {
                candidates.push(candidate);
            }
        }
    }
    for candidate in candidates {
        let mut collected = Vec::new();
        collect(candidate, &mut collected);
        if !collected.is_empty() {
            return collected;
        }
    }
    Vec::new()
}

fn collect(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
        Value::Object(_) if is_quota(value) => out.push(value.clone()),
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for key in keys {
                collect(&map[key], out);
            }
        }
        _ => {}
    }
}

/// Follows a path of keys into an object, as `root.search.hourly` does in the plugin.
fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |node, key| node.get(*key))
        .filter(|node| node.is_object())
}

fn first_string(entry: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        entry.get(*key).and_then(|value| match value {
            Value::String(raw) => {
                let trimmed = raw.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            }
            _ => None,
        })
    })
}

fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(raw) => raw.as_f64().filter(|n| n.is_finite()),
        Value::String(raw) => raw
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .filter(|_| !raw.trim().is_empty()),
        _ => None,
    }
}

fn first_number(entry: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| entry.get(*key).and_then(number_value))
}

/// A percent at or below 1 is a fraction: the plugin scales it by 100.
fn normalized(percent: Option<f64>) -> Option<f64> {
    percent.map(|percent| {
        if percent <= 1.0 {
            percent * 100.0
        } else {
            percent
        }
    })
}

/// An amount of money, which may carry `$` and thousands separators.
fn first_currency(entry: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        entry.get(*key).and_then(|value| match value {
            Value::String(raw) => raw
                .trim()
                .replace(['$', ','], "")
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite()),
            other => number_value(other),
        })
    })
}

/// A reset instant: ISO-8601, or an epoch past either threshold. The plugin discards a
/// date it cannot read rather than failing, so the window keeps drawing without one.
fn first_date(entry: &Value, keys: &[&str]) -> Option<Timestamp> {
    for key in keys {
        let Some(value) = entry.get(*key) else {
            continue;
        };
        if let Some(number) = number_value(value) {
            let unix = if number > 1_000_000_000_000.0 {
                (number / 1000.0).floor()
            } else if number > 1_000_000_000.0 {
                number.floor()
            } else {
                continue;
            };
            if let Ok(at) = Timestamp::from_unix(unix as i64) {
                return Some(at);
            }
        }
        if let Value::String(raw) = value
            && let Some(at) = parse_rfc3339(raw)
        {
            return Some(at);
        }
    }
    None
}

/// The window length in minutes, from whichever spelling states it.
fn window_minutes(entry: &Value) -> Option<f64> {
    if let Some(minutes) = first_number(
        entry,
        &[
            "windowMinutes",
            "window_minutes",
            "periodMinutes",
            "period_minutes",
        ],
    ) {
        return Some(minutes.round());
    }
    if let Some(hours) = first_number(
        entry,
        &["windowHours", "window_hours", "periodHours", "period_hours"],
    ) {
        return Some((hours * 60.0).round());
    }
    if let Some(days) = first_number(
        entry,
        &["windowDays", "window_days", "periodDays", "period_days"],
    ) {
        return Some((days * 1440.0).round());
    }
    if let Some(seconds) = first_number(
        entry,
        &[
            "windowSeconds",
            "window_seconds",
            "periodSeconds",
            "period_seconds",
        ],
    ) {
        return Some((seconds / 60.0).round());
    }
    let text = first_string(
        entry,
        &[
            "window",
            "windowLabel",
            "window_label",
            "period",
            "periodLabel",
            "period_label",
        ],
    )?;
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let split = cleaned
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(cleaned.len());
    let (number, unit) = cleaned.split_at(split);
    let multiplier = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 1.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60.0,
        "d" | "day" | "days" => 1440.0,
        _ => return None,
    };
    number
        .parse::<f64>()
        .ok()
        .map(|number| (number * multiplier).round())
}

/// A percent for a row: whole numbers plain, anything else to two decimals.
fn percent_text(percent: f64) -> String {
    if percent.fract() == 0.0 {
        format!("{percent:.0}%")
    } else {
        format!("{percent:.2}%")
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        _ => "not an object or array",
    }
}

/// Synthetic as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Synthetic",
    endpoint: |_| QUOTAS_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "Synthetic dashboard → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use tidemark_types::{Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `SyntheticProviderTests.swift` — "generic quota fixture
    /// matches the production golden". CodexBar asserts primary 25% with the 2025-01-01
    /// reset, secondary 75% with windowMinutes 1440, and the Starter plan.
    const GENERIC_QUOTAS: &str = r#"
        {
          "plan": "Starter",
          "quotas": [
            { "name": "Monthly", "limit": 1000, "used": 250, "reset_at": "2025-01-01T00:00:00Z" },
            { "name": "Daily", "max": 200, "remaining": 50, "window_minutes": 1440 }
          ]
        }
        "#;

    /// Recorded by CodexBar, same file — "missing rolling lane keeps weekly and search
    /// slots". CodexBar asserts primary nil, secondary 2%, tertiary 0.8%, and the
    /// $36.00 / $35.30 cost. Note the spellings: the limit rides `max`, the weekly
    /// percentage rides `percentRemaining`, the search reset rides `renewsAt`.
    const MISSING_ROLLING: &str = r#"
        {
          "weeklyTokenLimit": {
            "nextRegenAt": "2026-04-17T05:19:30.000Z",
            "percentRemaining": 98.0,
            "maxCredits": "$36.00",
            "remainingCredits": "$35.30",
            "nextRegenCredits": "$0.72"
          },
          "search": {
            "hourly": {
              "limit": 250,
              "requests": 2,
              "renewsAt": "2026-04-17T04:30:01.494Z"
            }
          }
        }
        "#;

    /// Recorded by CodexBar, `ProviderPluginParityTests.swift` — "Synthetic fixture
    /// matches the cut-over golden". All three named slots at once. CodexBar asserts
    /// 20% / 1.9411527777777735% / 0.8%, the nextTickAt and nextRegenAt resets, the
    /// tick percent 5, and the cost used 0.7000000000000028 of 36.
    const CUT_OVER: &str = r#"
        {
          "plan": "Starter",
          "weeklyTokenLimit": {
            "nextRegenAt": "2026-04-17T05:19:30.000Z",
            "percentRemaining": 98.05884722222223,
            "maxCredits": "$36.00",
            "remainingCredits": "$35.30",
            "nextRegenCredits": "$0.72"
          },
          "rollingFiveHourLimit": {
            "nextTickAt": "2026-04-17T03:44:11.000Z",
            "tickPercent": 0.05,
            "remaining": 600,
            "max": 750,
            "limited": false
          },
          "search": {
            "hourly": {
              "limit": 250,
              "requests": 2,
              "renewsAt": "2026-04-17T04:30:01.494Z"
            }
          }
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

    #[test]
    fn the_generic_quotas_fixture_draws_one_window_per_entry() {
        let snapshot = parse(GENERIC_QUOTAS, at(1_775_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        assert_eq!(snapshot.windows.len(), 2);

        let monthly = window(&snapshot, "monthly");
        assert_eq!(monthly.title, "Monthly");
        assert_eq!(monthly.used_percent, 25.0);
        assert_eq!(monthly.subtitle.as_deref(), Some("250 of 1000"));
        assert_eq!(
            monthly.resets_at,
            Some(at(1_735_689_600)),
            "2025-01-01T00:00:00Z, the instant CodexBar's own test asserts"
        );
        assert_eq!(monthly.length, None, "the entry states no window length");

        let daily = window(&snapshot, "daily");
        assert_eq!(daily.used_percent, 75.0, "max 200 with 50 remaining");
        assert_eq!(daily.subtitle.as_deref(), Some("150 of 200"));
        assert_eq!(daily.length.expect("window_minutes 1440").as_secs(), 86_400);
        assert_eq!(daily.resets_at, None);
    }

    #[test]
    fn the_missing_rolling_fixture_keeps_weekly_and_search() {
        let snapshot = parse(MISSING_ROLLING, at(1_775_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["weekly-token", "search-hourly", "balance"]);

        let weekly = window(&snapshot, "weekly-token");
        assert_eq!(weekly.used_percent, 2.0, "100 - percentRemaining 98");
        assert_eq!(weekly.resets_at, Some(at(1_776_403_170)));
        assert_eq!(
            weekly.subtitle, None,
            "no absolutes ride a percent-only entry"
        );
        assert_eq!(weekly.length, None);

        let search = window(&snapshot, "search-hourly");
        assert_eq!(search.used_percent, 0.8, "requests 2 of limit 250");

        let balance = window(&snapshot, "balance");
        assert_eq!(balance.used_percent, (36.0 - 35.3) / 36.0 * 100.0);
        assert_eq!(balance.subtitle.as_deref(), Some("$0.70 / $36.00"));
        assert_eq!(balance.resets_at, Some(at(1_776_403_170)));
        assert_eq!(balance.length, None, "a balance has no length to key on");
    }

    #[test]
    fn the_cut_over_fixture_draws_all_three_slots_and_the_cost() {
        let snapshot = parse(CUT_OVER, at(1_775_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "rolling-five-hour",
                "weekly-token",
                "search-hourly",
                "balance"
            ]
        );

        let rolling = window(&snapshot, "rolling-five-hour");
        assert_eq!(rolling.title, "Rolling five-hour limit");
        assert_eq!(rolling.used_percent, 20.0, "750 max with 600 remaining");
        assert_eq!(rolling.subtitle.as_deref(), Some("150 of 750"));
        assert_eq!(rolling.resets_at, Some(at(1_776_397_451)), "nextTickAt");
        assert_eq!(
            rolling.length, None,
            "the wire states no length for the slot"
        );

        let weekly = window(&snapshot, "weekly-token");
        assert_eq!(
            weekly.used_percent,
            100.0 - 98.058847222222_23,
            "the exact double CodexBar's own test asserts"
        );

        let search = window(&snapshot, "search-hourly");
        assert_eq!(search.used_percent, 0.8);
        assert_eq!(
            search.resets_at,
            Some(at(1_776_400_201)),
            "renewsAt; the .494 fraction cannot survive a whole-second timestamp"
        );

        let balance = window(&snapshot, "balance");
        assert_eq!(balance.title, "Weekly credits");
        assert_eq!(balance.used_percent, (36.0 - 35.3) / 36.0 * 100.0);
        assert_eq!(balance.subtitle.as_deref(), Some("$0.70 / $36.00"));
    }

    #[test]
    fn the_plan_and_the_regen_rates_become_detail_rows() {
        let snapshot = parse(CUT_OVER, at(1_775_000_000)).expect("parses");
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Regeneration"]);
        assert_eq!(snapshot.details[0].rows[0].label, "Plan");
        assert_eq!(snapshot.details[0].rows[0].value, "Starter");
        let regen = &snapshot.details[1].rows;
        assert_eq!(regen[0].label, "Rolling five-hour limit");
        assert_eq!(
            regen[0].value, "+5% per tick",
            "tickPercent 0.05 is a fraction"
        );
        assert_eq!(regen[1].label, "Weekly credits");
        assert_eq!(regen[1].value, "+$0.72 per regen");
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names; a body with nothing quota-shaped
        // in it at all (the plugin's own "Missing quota data" refusal); and a quota
        // entry whose consumption is a string where a number belongs.
        let string_where_number = r#"
        { "quotas": [ { "name": "Monthly", "limit": 1000, "used": "many" } ] }
        "#;
        for body in [
            "{\"partial\":",
            r#"{ "plan": "Starter" }"#,
            string_where_number,
        ] {
            let error = parse(body, at(1_775_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn an_entry_of_an_unrecognised_kind_is_skipped_and_an_unreadable_one_refused() {
        // One recorded-shape entry beside one entry no quota key list recognises: the
        // second is skipped, exactly one window draws. The same entry with a recognised
        // key whose value no arithmetic can turn into a percentage is refused — a quota
        // we cannot measure must not be drawn as one we did.
        let unknown_kind = r#"
        { "quotas": [
            { "name": "Monthly", "limit": 1000, "used": 250 },
            { "kind": "mystery", "note": true, "windowMinutes": "soon" }
        ] }
        "#;
        let snapshot = parse(unknown_kind, at(1_775_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key.as_str(), "monthly");

        let unreadable = r#"
        { "quotas": [ { "name": "Odd", "used": 7 } ] }
        "#;
        assert!(
            matches!(
                parse(unreadable, at(1_775_000_000)),
                Err(ProviderError::Malformed(_))
            ),
            "used without a limit or a percentage is not a readable quota"
        );
    }

    #[test]
    fn the_spec_polls_the_quota_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.synthetic.new/v2/quotas"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "Synthetic has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
