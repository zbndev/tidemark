//! Chutes.
//!
//! Ported from CodexBar's Swift parser and fetcher, `Providers/Chutes/
//! ChutesUsageStats.swift`; there is no JS plugin. Never seen answering: every number
//! in the tests is a body CodexBar recorded.
//!
//! # One endpoint of a two-step fetch
//!
//! CodexBar polls `/users/me/subscription_usage` first and, when that body is missing
//! either window, falls back to `/users/me/quotas` and then to one request per chute
//! under `/users/me/quota_usage/<id>`. [`Spec`] spends one request, so this port polls
//! the primary endpoint and stops there: a body carrying only the monthly window draws
//! only the monthly window. The fallback ladder is CodexBar's robustness against a
//! sparser API, not a second meaning, and is not ported.
//!
//! # The two windows
//!
//! A subscription body carries a **rolling** window (the payload states
//! `window_minutes: 240`, four hours) and a **monthly** window (the payload states no
//! length; the parser's own default is 43,200 minutes, thirty days — the same length
//! the recorded quota entry states for itself). Both lengths come from the payload and
//! its defaults, not from the interface labels. Each window's consumption is a percent
//! read directly when a percent field is present, else derived from used over limit
//! with the source's cross-fill (a missing limit is `used + remaining`, a missing used
//! is `limit - remaining`); the absolutes ride under the bar as `40/100 requests`,
//! whole amounts plain and fractional ones trimmed to two decimals, with the unit
//! (default `credits`) as a suffix.
//!
//! A percent of exactly 1 is one percent, not a fraction: only a value *below* 1 in
//! magnitude is scaled by 100 (`0.5` reads as 50), as the source normalises, and every
//! percent clamps to 0..=100. Numbers may arrive quoted, carrying `,`, `$` and `%`, and
//! are read through the noise. Key matching ignores case and separators —
//! `rolling_window`, `rollingWindow` and `ROLLINGWINDOW` all match.
//!
//! # The quota walk
//!
//! When no named rolling or monthly slot is present, the payload is walked for
//! quota-shaped objects under the container keys (`quotas`, `usage`, …) and then
//! through the whole body, and the first entry classifying as rolling (label or unit
//! naming it, or 240 minutes) becomes the rolling window, the first classifying as
//! monthly (naming it, or at least 28 days) the monthly window. The rest draw as extra
//! windows. Objects the named slots already read are not drawn twice, and structurally
//! identical objects collapse to one — the source deduplicates the same way, so two
//! byte-identical quota entries are one window, not a collision.
//!
//! Two windows stating the same length are two quotas, not one window reported twice:
//! on a collision each is keyed by its pool — the named windows by `rolling`/
//! `monthly`, a walked entry by the key it descended under — and a duplicate the pools
//! cannot separate (two label-less entries of one span inside one section) is refused.
//!
//! # Where this port is stricter than the source
//!
//! The source silently drops a quota-shaped entry whose consumption cannot be derived;
//! this port refuses the whole fetch for one, per the workspace rule that a recognised
//! entry is never a silent absence. An entry no key family recognises as a quota is
//! skipped, as in the source. An object with no quota data at all is an empty reading,
//! not an error — the source's own test says so — but a body that is not an object or
//! an array is refused here.
//!
//! A subscription the payload calls inactive draws a `No active subscription` row in
//! the plan section, the source's own sentence; an active or unknown state draws no
//! row, also as in the source.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, length_title, parse_rfc3339};
use serde_json::{Map, Value};
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "chutes";

const SUBSCRIPTION_USAGE_URL: &str = "https://api.chutes.ai/users/me/subscription_usage";

/// The rolling window's default length in minutes, as the source states it.
const ROLLING_MINUTES: i64 = 4 * 60;
/// The monthly window's default length in minutes: thirty days.
const MONTHLY_MINUTES: i64 = 30 * 24 * 60;
/// A window of at least 28 days reads as monthly, whatever it calls itself.
const MONTHLY_MINIMUM_MINUTES: i64 = 28 * 24 * 60;

/// The named rolling slots, in the source's order. Matching ignores case and separators.
const ROLLING_KEYS: &[&str] = &[
    "rolling",
    "rolling_window",
    "rolling_4h",
    "four_hour",
    "four_hour_usage",
    "window_4h",
];
/// The named monthly slots, in the source's order.
const MONTHLY_KEYS: &[&str] = &[
    "monthly",
    "monthly_usage",
    "subscription",
    "subscription_usage",
    "billing_period",
];
/// The subscription block's possible homes.
const SUBSCRIPTION_KEYS: &[&str] = &[
    "subscription",
    "subscription_usage",
    "current_subscription",
    "plan",
];
/// Where the quota walk looks first, in the source's order.
const CONTAINER_KEYS: &[&str] = &[
    "quotas",
    "quota",
    "quota_usage",
    "limits",
    "usage",
    "entries",
    "subscription_usage",
];
const LABEL_KEYS: &[&str] = &[
    "label",
    "name",
    "title",
    "type",
    "quota_type",
    "period",
    "window",
    "window_name",
    "chute_id",
];
const LIMIT_KEYS: &[&str] = &[
    "limit",
    "cap",
    "max",
    "maximum",
    "quota",
    "quota_limit",
    "monthly_cap",
    "monthly_limit",
    "request_limit",
    "token_limit",
    "hard_limit",
    "total",
];
const USED_KEYS: &[&str] = &[
    "used",
    "usage",
    "used_amount",
    "consumed",
    "consumed_amount",
    "current",
    "current_usage",
    "requests",
    "request_count",
    "tokens",
    "token_usage",
    "monthly_usage",
];
const REMAINING_KEYS: &[&str] = &[
    "remaining",
    "available",
    "balance",
    "left",
    "remaining_amount",
    "available_amount",
];
const PERCENT_USED_KEYS: &[&str] = &[
    "percent_used",
    "usage_percent",
    "used_percent",
    "utilization",
    "utilization_percent",
];
const PERCENT_REMAINING_KEYS: &[&str] = &["percent_remaining", "remaining_percent"];
const RESET_KEYS: &[&str] = &[
    "reset_at",
    "resets_at",
    "reset_time",
    "next_reset_at",
    "renews_at",
    "renewal_at",
    "period_end",
    "current_period_end",
    "expires_at",
    "window_end",
    "end_time",
];
const UNIT_KEYS: &[&str] = &["unit", "units", "currency", "quota_unit"];
const ACTIVE_KEYS: &[&str] = &[
    "active",
    "is_active",
    "subscription_active",
    "has_subscription",
];
const STATUS_KEYS: &[&str] = &["status", "state", "subscription_status"];
const PLAN_KEYS: &[&str] = &[
    "plan_name",
    "plan",
    "tier",
    "subscription_plan",
    "subscription_tier",
];

/// One quota as the payload states it, before it becomes a window.
#[derive(Debug, Clone, PartialEq)]
struct QuotaWindow {
    label: Option<String>,
    limit: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
    percent: Option<f64>,
    minutes: Option<i64>,
    resets_at: Option<Timestamp>,
    unit: String,
}

/// Which named slot a walked entry classifies into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Rolling,
    Monthly,
}

/// One window-to-be: the quota, the pool its key falls back to when its length is
/// contested, and the title it draws under.
struct Draft {
    quota: QuotaWindow,
    pool: String,
    title: String,
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let json: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    // An array root is a quotas list with the wrapper omitted, as the source reads it.
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
    let root_map = root.as_object().expect("just built as one");
    let data_root = value_by_keys(root_map, &["data", "result"])
        .and_then(Value::as_object)
        .unwrap_or(root_map);
    let subscription = dictionary_by_keys(root_map, data_root, SUBSCRIPTION_KEYS);

    // The named slots first, each with its default label and length.
    let rolling_dict = dictionary_by_keys(root_map, data_root, ROLLING_KEYS);
    let monthly_dict = dictionary_by_keys(root_map, data_root, MONTHLY_KEYS);
    let explicit_rolling = rolling_dict
        .map(|dict| parse_quota(dict, Some("4-hour quota"), Some(ROLLING_MINUTES)))
        .transpose()?
        .flatten();
    let explicit_monthly = monthly_dict
        .map(|dict| parse_quota(dict, Some("Monthly quota"), Some(MONTHLY_MINUTES)))
        .transpose()?
        .flatten();

    // The walk, for everything the named slots did not carry. An object a named slot
    // already read is not drawn twice — compared by contents, since the walk holds
    // clones — and the source's dedup collapses byte-identical objects the same way.
    let mut walked: Vec<(QuotaWindow, String, Option<Kind>)> = Vec::new();
    for item in quota_walk(root_map, data_root) {
        let dict = item
            .value
            .as_object()
            .expect("the walk collected an object");
        if rolling_dict.is_some_and(|named| named == dict)
            || monthly_dict.is_some_and(|named| named == dict)
        {
            continue;
        }
        let Some(quota) = parse_quota(dict, None, None)? else {
            continue;
        };
        let kind = kind(&quota);
        walked.push((quota, item.section, kind));
    }

    // A named slot, when present, wins its kind outright; only a missing slot takes
    // the first walked entry of its kind.
    let rolling_from_walk = if explicit_rolling.is_some() {
        None
    } else {
        walked
            .iter()
            .position(|(_, _, kind)| *kind == Some(Kind::Rolling))
    };
    let monthly_from_walk = if explicit_monthly.is_some() {
        None
    } else {
        walked
            .iter()
            .position(|(_, _, kind)| *kind == Some(Kind::Monthly))
    };

    // Drafts in draw order: the rolling window, the monthly window, then every walked
    // entry neither of those claimed.
    let mut drafts: Vec<Draft> = Vec::new();
    if let Some(quota) = explicit_rolling {
        let title = quota.label.clone().expect("the default label applied");
        drafts.push(Draft {
            quota,
            pool: "rolling".to_owned(),
            title,
        });
    } else if let Some(index) = rolling_from_walk {
        let (quota, _, _) = &walked[index];
        drafts.push(Draft {
            quota: quota.clone(),
            pool: "rolling".to_owned(),
            title: quota
                .label
                .clone()
                .unwrap_or_else(|| "4-hour quota".to_owned()),
        });
    }
    if let Some(quota) = explicit_monthly {
        let title = quota.label.clone().expect("the default label applied");
        drafts.push(Draft {
            quota,
            pool: "monthly".to_owned(),
            title,
        });
    } else if let Some(index) = monthly_from_walk {
        let (quota, _, _) = &walked[index];
        drafts.push(Draft {
            quota: quota.clone(),
            pool: "monthly".to_owned(),
            title: quota
                .label
                .clone()
                .unwrap_or_else(|| "Monthly quota".to_owned()),
        });
    }
    let classified = [rolling_from_walk, monthly_from_walk];
    for (index, (quota, section, _)) in walked.iter().enumerate() {
        if classified.contains(&Some(index)) {
            continue;
        }
        let title = quota
            .label
            .clone()
            .or_else(|| length_of(quota).map(length_title));
        let Some(title) = title else {
            return Err(ProviderError::malformed(
                "a quota entry states no label and no length to key its window on",
            ));
        };
        drafts.push(Draft {
            quota: quota.clone(),
            pool: section.clone(),
            title,
        });
    }

    let windows = key_windows(drafts)?;

    // The plan section: the plan's name, the state the source calls out, and the
    // subscription's renewal date.
    let state = subscription_state(root_map, data_root, subscription);
    let mut plan_rows = Vec::new();
    if let Some(plan) = first_string(root_map, PLAN_KEYS)
        .or_else(|| first_string(data_root, PLAN_KEYS))
        .or_else(|| subscription.and_then(|map| first_string(map, PLAN_KEYS)))
    {
        plan_rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: plan,
        });
    }
    if state == State::Inactive {
        plan_rows.push(DetailRow {
            label: "Subscription".to_owned(),
            value: "No active subscription".to_owned(),
        });
    }
    if let Some(at) = first_date(root_map, RESET_KEYS)
        .or_else(|| first_date(data_root, RESET_KEYS))
        .or_else(|| subscription.and_then(|map| first_date(map, RESET_KEYS)))
    {
        plan_rows.push(DetailRow {
            label: "Renews".to_owned(),
            value: day_of(at),
        });
    }
    let details = if plan_rows.is_empty() {
        Vec::new()
    } else {
        vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: plan_rows,
        }]
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
}

/// Settles the windows' keys: a length keys the window, a lengthless one its label.
/// Two windows claiming one key are re-keyed by their pool, so the same span reported
/// by a named slot and by a walked entry (or by two sections) draws both windows; a
/// duplicate the pools cannot separate is refused, because the ingest would file the
/// second under the first's key and drop it silently.
fn key_windows(drafts: Vec<Draft>) -> Result<Vec<Window>, ProviderError> {
    let mut keyed: Vec<(Draft, String)> = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let key = match length_of(&draft.quota) {
            Some(length) => format!("w{}", length.as_secs()),
            None => draft
                .quota
                .label
                .as_deref()
                .expect("a lengthless draft was checked for a label")
                .to_lowercase(),
        };
        keyed.push((draft, key));
    }

    let mut windows = Vec::with_capacity(keyed.len());
    for (index, (draft, key)) in keyed.iter().enumerate() {
        let contested = keyed[..index]
            .iter()
            .chain(keyed[index + 1..].iter())
            .any(|(_, other)| other == key);
        let length = length_of(&draft.quota);
        let key = if contested {
            match length {
                Some(length) => WindowKey::for_pool(&draft.pool, length).as_str().to_owned(),
                None => {
                    return Err(ProviderError::malformed(format!(
                        "two windows arrived under the key {key}"
                    )));
                }
            }
        } else {
            key.clone()
        };
        windows.push(Window {
            key: WindowKey::named(&key),
            title: draft.title.clone(),
            subtitle: usage_description(&draft.quota),
            used_percent: draft
                .quota
                .percent
                .expect("a draft was checked for a percent")
                .clamp(0.0, 100.0),
            resets_at: draft.quota.resets_at,
            length,
        });
    }
    for (index, one) in windows.iter().enumerate() {
        if windows[..index].iter().any(|other| other.key == one.key) {
            return Err(ProviderError::malformed(format!(
                "two windows arrived under the key {}",
                one.key
            )));
        }
    }
    Ok(windows)
}

/// Reads one quota payload into a [`QuotaWindow`], or `Ok(None)` when no key family
/// recognises it as a quota at all. A quota-shaped payload whose consumption cannot be
/// derived is an error, not a silent absence.
fn parse_quota(
    payload: &Map<String, Value>,
    default_label: Option<&str>,
    default_minutes: Option<i64>,
) -> Result<Option<QuotaWindow>, ProviderError> {
    let label = first_string(payload, LABEL_KEYS).or_else(|| default_label.map(str::to_owned));
    let limit = first_number(payload, LIMIT_KEYS);
    let used = first_number(payload, USED_KEYS);
    let remaining = first_number(payload, REMAINING_KEYS);
    let mut percent = normalized_percent(first_number(payload, PERCENT_USED_KEYS)).or_else(|| {
        normalized_percent(first_number(payload, PERCENT_REMAINING_KEYS))
            .map(|remaining| 100.0 - remaining)
    });

    // The cross-fill the source performs, in its order: only these two fills feed the
    // percentage, and each sees the fills before it.
    let mut filled_limit = limit;
    let mut filled_used = used;
    if filled_limit.is_none() && used.is_some() && remaining.is_some() {
        filled_limit = used
            .zip(remaining)
            .map(|(used, remaining)| used + remaining);
    }
    if filled_used.is_none() && filled_limit.is_some() && remaining.is_some() {
        filled_used = filled_limit
            .zip(remaining)
            .map(|(limit, remaining)| limit - remaining);
    }
    if percent.is_none()
        && let (Some(limit), Some(used)) = (filled_limit, filled_used)
        && limit > 0.0
    {
        percent = Some(used / limit * 100.0);
    }

    let Some(percent) = percent else {
        if is_quota(payload) {
            return Err(ProviderError::malformed(
                "a quota this parser recognises states no readable consumption",
            ));
        }
        return Ok(None);
    };

    Ok(Some(QuotaWindow {
        label,
        limit,
        used,
        remaining,
        percent: Some(percent),
        minutes: window_minutes(payload).or(default_minutes),
        resets_at: first_date(payload, RESET_KEYS),
        unit: first_string(payload, UNIT_KEYS).unwrap_or_else(|| "credits".to_owned()),
    }))
}

/// True when any quota key family finds a number — the source's own `isQuotaPayload`.
fn is_quota(payload: &Map<String, Value>) -> bool {
    [
        LIMIT_KEYS,
        USED_KEYS,
        REMAINING_KEYS,
        PERCENT_USED_KEYS,
        PERCENT_REMAINING_KEYS,
    ]
    .iter()
    .any(|keys| first_number(payload, keys).is_some())
}

/// Which named slot a walked entry classifies into: by its label and unit naming it,
/// or by its length landing on the rolling default / above the monthly minimum.
fn kind(quota: &QuotaWindow) -> Option<Kind> {
    let label = [quota.label.as_deref(), Some(quota.unit.as_str())]
        .iter()
        .flatten()
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if ["rolling", "4h", "4 h", "4-hour", "four hour", "four-hour"]
        .iter()
        .any(|marker| label.contains(marker))
        || quota.minutes == Some(ROLLING_MINUTES)
    {
        return Some(Kind::Rolling);
    }
    if ["month", "billing", "subscription"]
        .iter()
        .any(|marker| label.contains(marker))
        || quota
            .minutes
            .is_some_and(|minutes| minutes >= MONTHLY_MINIMUM_MINUTES)
    {
        return Some(Kind::Monthly);
    }
    None
}

/// The window's length, when its minutes are present and positive.
fn length_of(quota: &QuotaWindow) -> Option<WindowLength> {
    quota
        .minutes
        .filter(|minutes| *minutes > 0)
        .and_then(|minutes| WindowLength::from_secs((minutes * 60) as u64))
}

/// The absolutes under the bar, in the source's spelling: `40/100 requests`. Whole
/// amounts render plain, fractional ones with their trailing zeros trimmed; the unit
/// (defaulting to credits) rides as a suffix.
fn usage_description(quota: &QuotaWindow) -> Option<String> {
    let limit = quota.limit.filter(|limit| *limit > 0.0)?;
    let used = quota.used.or_else(|| {
        quota
            .remaining
            .map(|remaining| (limit - remaining).max(0.0))
    })?;
    let unit = quota.unit.trim();
    let suffix = if unit.is_empty() {
        String::new()
    } else {
        format!(" {unit}")
    };
    Some(format!("{}/{}{}", amount(used), amount(limit), suffix))
}

/// One amount as the source formats it: whole numbers plain, anything else to two
/// decimals with trailing zeros trimmed.
fn amount(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.0001 {
        return format!("{}", rounded as i64);
    }
    let mut text = format!("{value:.2}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// A percent as the source normalises it: a value below 1 in magnitude is a fraction
/// and is scaled by 100; everything clamps to 0..=100.
fn normalized_percent(value: Option<f64>) -> Option<f64> {
    value.map(|value| {
        let percent = if value.abs() < 1.0 {
            value * 100.0
        } else {
            value
        };
        percent.clamp(0.0, 100.0)
    })
}

/// The window length in minutes, from whichever spelling states it: minutes, hours,
/// days, seconds, or a `240min`-shaped string.
fn window_minutes(payload: &Map<String, Value>) -> Option<i64> {
    const MINUTE_KEYS: &[&str] = &["window_minutes", "period_minutes", "duration_minutes"];
    const HOUR_KEYS: &[&str] = &["window_hours", "period_hours", "duration_hours"];
    const DAY_KEYS: &[&str] = &["window_days", "period_days", "duration_days"];
    const SECOND_KEYS: &[&str] = &["window_seconds", "period_seconds", "duration_seconds"];
    const TEXT_KEYS: &[&str] = &["window", "period", "interval", "duration"];

    if let Some(minutes) = first_number(payload, MINUTE_KEYS) {
        return Some(minutes.round() as i64);
    }
    if let Some(hours) = first_number(payload, HOUR_KEYS) {
        return Some((hours * 60.0).round() as i64);
    }
    if let Some(days) = first_number(payload, DAY_KEYS) {
        return Some((days * 1440.0).round() as i64);
    }
    if let Some(seconds) = first_number(payload, SECOND_KEYS) {
        return Some((seconds / 60.0).round() as i64);
    }
    let text = first_string(payload, TEXT_KEYS)?;
    let compact: String = text
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let split = compact
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(compact.len());
    let (number, unit) = compact.split_at(split);
    let value: f64 = number.parse().ok()?;
    if value <= 0.0 {
        return None;
    }
    let minutes = if unit.starts_with("min") || unit == "m" {
        value
    } else if unit.starts_with("hour") || unit.starts_with("hr") || unit == "h" {
        value * 60.0
    } else if unit.starts_with("day") || unit == "d" {
        value * 1440.0
    } else if unit.starts_with("month") || unit == "mo" {
        value * 43_200.0
    } else {
        return None;
    };
    Some(minutes.round() as i64)
}

/// One walked object, with the object key it descended under.
struct Walked {
    value: Value,
    section: String,
}

/// Gathers every quota-shaped object the payload carries, as the source gathers them:
/// the first container key present on the root, then under `data`, then the `data`
/// object and the root themselves, swept recursively. Duplicates collapse to their
/// first occurrence.
fn quota_walk(root_map: &Map<String, Value>, data_root: &Map<String, Value>) -> Vec<Walked> {
    let mut collected: Vec<Walked> = Vec::new();
    for map in [root_map, data_root] {
        if let Some((key, candidate)) = key_and_value_by_keys(map, CONTAINER_KEYS) {
            collect(candidate, key, &mut collected);
        }
    }
    let data = Value::Object(data_root.clone());
    collect(&data, "data", &mut collected);
    let root = Value::Object(root_map.clone());
    collect(&root, "root", &mut collected);

    let mut unique: Vec<Walked> = Vec::new();
    for entry in collected {
        if !unique.iter().any(|other| other.value == entry.value) {
            unique.push(entry);
        }
    }
    unique
}

/// Descends one candidate: an array descends into its items; an object collects itself
/// when quota-shaped and still descends into its values in sorted key order — the
/// source does the same, so a quota nested inside a quota is found too.
fn collect(value: &Value, section: &str, out: &mut Vec<Walked>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect(item, section, out)),
        Value::Object(map) => {
            if is_quota(map) {
                out.push(Walked {
                    value: value.clone(),
                    section: section.to_owned(),
                });
            }
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for key in keys {
                collect(&map[key], key, out);
            }
        }
        _ => {}
    }
}

/// The normalized spelling both sides of a lookup are reduced to: lowercase,
/// alphanumerics only, so `rolling_window` and `rollingWindow` are one key.
fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// The first value any of `keys` matches, key-list priority first, as the source looks.
fn value_by_keys<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    key_and_value_by_keys(map, keys).map(|(_, value)| value)
}

/// [`value_by_keys`], also naming the object key that matched — the section a walked
/// entry keys its pool by.
fn key_and_value_by_keys<'a>(
    map: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<(&'a str, &'a Value)> {
    keys.iter().find_map(|key| {
        let normalized = normalized_key(key);
        map.iter()
            .find(|(candidate, _)| normalized_key(candidate) == normalized)
            .map(|(matched, value)| (matched.as_str(), value))
    })
}

/// The first dictionary any of `keys` matches on the root, then under `data`.
fn dictionary_by_keys<'a>(
    root: &'a Map<String, Value>,
    data_root: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Map<String, Value>> {
    value_by_keys(root, keys)
        .or_else(|| value_by_keys(data_root, keys))
        .and_then(Value::as_object)
}

/// The first string any of `keys` matches: trimmed, non-empty; a number reads as its
/// own spelling.
fn first_string(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    value_by_keys(map, keys).and_then(|value| match value {
        Value::String(raw) => {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Number(_) => {
            let rendered = value.to_string();
            (!rendered.is_empty()).then_some(rendered)
        }
        _ => None,
    })
}

/// A number, however the payload spells it: bare, quoted, or quoted with `,`, `$` and
/// `%` mixed in.
fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(raw) => raw.as_f64().filter(|n| n.is_finite()),
        Value::String(raw) => {
            let text: String = raw
                .trim()
                .chars()
                .filter(|c| !matches!(c, ',' | '$' | '%'))
                .collect();
            if text.is_empty() {
                return None;
            }
            text.parse::<f64>().ok().filter(|n| n.is_finite())
        }
        _ => None,
    }
}

fn first_number(map: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    value_by_keys(map, keys).and_then(number_value)
}

/// A boolean however the payload states it: bare, as a number, or as the source's own
/// set of words.
fn first_bool(map: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    value_by_keys(map, keys).and_then(|value| match value {
        Value::Bool(raw) => Some(*raw),
        Value::Number(raw) => raw.as_f64().map(|n| n != 0.0),
        Value::String(raw) => match raw.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "active" => Some(true),
            "false" | "0" | "no" | "inactive" | "none" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

/// A reset instant: an epoch in seconds or milliseconds, or an RFC-3339 string — the
/// number may itself arrive quoted. An unreadable date is discarded, as in the source,
/// so the window draws without its pace mark rather than failing the fetch.
fn first_date(map: &Map<String, Value>, keys: &[&str]) -> Option<Timestamp> {
    value_by_keys(map, keys).and_then(date_value)
}

fn date_value(value: &Value) -> Option<Timestamp> {
    match value {
        Value::Number(raw) => raw.as_f64().and_then(epoch),
        Value::String(raw) => {
            let text = raw.trim();
            if text.is_empty() {
                return None;
            }
            text.parse::<f64>()
                .ok()
                .and_then(epoch)
                .or_else(|| parse_rfc3339(text))
        }
        _ => None,
    }
}

/// An epoch the source would accept: positive, and in milliseconds when implausibly
/// large for seconds.
fn epoch(value: f64) -> Option<Timestamp> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let seconds = if value > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    Timestamp::from_unix(seconds as i64).ok()
}

/// The subscription's state, from its active flag or its status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Active,
    Inactive,
    Unknown,
}

fn subscription_state(
    root: &Map<String, Value>,
    data_root: &Map<String, Value>,
    subscription: Option<&Map<String, Value>>,
) -> State {
    if let Some(active) = first_bool(root, ACTIVE_KEYS)
        .or_else(|| first_bool(data_root, ACTIVE_KEYS))
        .or_else(|| subscription.and_then(|map| first_bool(map, ACTIVE_KEYS)))
    {
        return if active {
            State::Active
        } else {
            State::Inactive
        };
    }
    let status = first_string(root, STATUS_KEYS)
        .or_else(|| first_string(data_root, STATUS_KEYS))
        .or_else(|| subscription.and_then(|map| first_string(map, STATUS_KEYS)))
        .map(|status| status.to_lowercase());
    let Some(status) = status else {
        return State::Unknown;
    };
    if status.contains("active") && !status.contains("inactive") {
        return State::Active;
    }
    if ["free", "inactive", "cancel", "none", "expired"]
        .iter()
        .any(|marker| status.contains(marker))
    {
        return State::Inactive;
    }
    State::Unknown
}

/// The `YYYY-MM-DD` a whole-second timestamp falls on, for the renewal row.
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

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        _ => "not an object or array",
    }
}

/// Chutes as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Chutes",
    endpoint: |_| SUBSCRIPTION_USAGE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "chutes.ai account settings → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use tidemark_types::{Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `ChutesProviderTests.swift` — "fetch usage maps active
    /// subscription monthly and rolling windows". CodexBar asserts the rolling window at
    /// 40% with 240 minutes, the reset instants, both reset descriptions, the renewal
    /// date, and that exactly one request is spent on this body.
    const ACTIVE_SUBSCRIPTION: &str = r#"
            {
              "subscription": {
                "active": true,
                "plan_name": "Pro",
                "current_period_end": "2026-07-01T00:00:00Z"
              },
              "monthly": {
                "used": 250,
                "limit": 1000,
                "resets_at": "2026-07-01T00:00:00Z",
                "unit": "credits"
              },
              "rolling_window": {
                "requests": 40,
                "limit": 100,
                "window_minutes": 240,
                "reset_at": "2026-06-13T18:00:00Z",
                "unit": "requests"
              }
            }
            "#;

    /// Recorded by CodexBar, same file — "no active subscription falls back to quotas
    /// endpoint". This is the `subscription_usage` body of that multi-request test; on
    /// this endpoint it carries no quota data at all and an inactive subscription.
    const NO_ACTIVE_SUBSCRIPTION: &str = r#"
                {
                  "subscription": {
                    "active": false,
                    "status": "free"
                  }
                }
                "#;

    /// Recorded by CodexBar, same file — "partial subscription usage fills missing
    /// rolling window from quotas". The `subscription_usage` body: the monthly window
    /// alone, stating no reset of its own.
    const PARTIAL_MONTHLY: &str = r#"
                {
                  "subscription": {
                    "active": true,
                    "plan_name": "Pro",
                    "current_period_end": "2026-07-01T00:00:00Z"
                  },
                  "monthly": {
                    "used": 250,
                    "limit": 1000,
                    "unit": "credits"
                  }
                }
                "#;

    /// Recorded by CodexBar, same file — "missing usage fields returns no data snapshot
    /// without decode failure". An object with no quota data is an empty reading, not an
    /// error.
    const MISSING_USAGE_FIELDS: &str =
        r#"{"subscription":{"active":true},"unexpected":{"nested":true}}"#;

    /// Recorded by CodexBar, same file — "identical usage values keep distinct quota
    /// windows". Two quota entries of the same consumption, told apart only by their
    /// window lengths: 240 minutes and 43,200.
    const DISTINCT_QUOTA_WINDOWS: &str = r#"
        {
          "quotas": [
            {
              "used": 0,
              "limit": 100,
              "window_minutes": 240
            },
            {
              "used": 0,
              "limit": 100,
              "window_minutes": 43200
            }
          ]
        }
        "#;

    /// Recorded by CodexBar, same file — "exact percent value of one stays one percent".
    /// A used percent of 1 is one percent, not a fraction; a remaining percent of 1 reads
    /// as 99 used.
    const PERCENT_USED_ONE: &str = r#"
        {
          "rolling_window": {
            "usage_percent": 1
          }
        }
        "#;
    const PERCENT_REMAINING_ONE: &str = r#"
        {
          "rolling_window": {
            "percent_remaining": 1
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

    fn row<'a>(snapshot: &'a Snapshot, in_section: &str, label: &str) -> &'a DetailRow {
        let found = snapshot
            .details
            .iter()
            .find(|section| section.title == in_section)
            .unwrap_or_else(|| panic!("no section {in_section} in {:?}", snapshot.details));
        found
            .rows
            .iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no row {label} in {in_section}"))
    }

    #[test]
    fn the_active_subscription_fixture_draws_both_windows() {
        let snapshot = parse(ACTIVE_SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["w14400", "w2592000"]);

        let rolling = window(&snapshot, "w14400");
        assert_eq!(rolling.title, "4-hour quota");
        assert_eq!(rolling.used_percent, 40.0, "40 requests of a 100 limit");
        assert_eq!(rolling.subtitle.as_deref(), Some("40/100 requests"));
        assert_eq!(
            rolling.resets_at,
            Some(at(1_781_373_600)),
            "2026-06-13T18:00:00Z, the reset CodexBar's own test reads"
        );
        assert_eq!(
            rolling.length.expect("window_minutes 240").as_secs(),
            14_400
        );

        let monthly = window(&snapshot, "w2592000");
        assert_eq!(monthly.title, "Monthly quota");
        assert_eq!(monthly.used_percent, 25.0, "250 credits of a 1000 limit");
        assert_eq!(monthly.subtitle.as_deref(), Some("250/1000 credits"));
        assert_eq!(monthly.resets_at, Some(at(1_782_864_000)));
        assert_eq!(
            monthly
                .length
                .expect("the stated monthly default")
                .as_secs(),
            2_592_000,
            "no window_minutes on the wire, so the parser's 30-day default applies"
        );

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "w14400",
            "the card leads with the four-hour window, as CodexBar's primary does"
        );

        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Pro");
        assert_eq!(row(&snapshot, "Plan", "Renews").value, "2026-07-01");
    }

    #[test]
    fn a_subscription_without_quota_fields_is_an_empty_reading() {
        let snapshot = parse(NO_ACTIVE_SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "no quota data on the wire, and none is invented"
        );
        assert_eq!(
            row(&snapshot, "Plan", "Subscription").value,
            "No active subscription",
            "CodexBar's own sentence for this state"
        );
    }

    #[test]
    fn the_partial_fixture_draws_the_monthly_window_alone() {
        // CodexBar's fetcher answers the missing rolling window with a second request to
        // /users/me/quotas; this port polls one endpoint, so the monthly window is the
        // whole reading and the card carries the one bar.
        let snapshot = parse(PARTIAL_MONTHLY, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        let monthly = window(&snapshot, "w2592000");
        assert_eq!(monthly.used_percent, 25.0);
        assert_eq!(monthly.subtitle.as_deref(), Some("250/1000 credits"));
        assert_eq!(monthly.resets_at, None, "this window states no reset");
        assert_eq!(row(&snapshot, "Plan", "Renews").value, "2026-07-01");
    }

    #[test]
    fn a_body_with_no_quota_data_is_not_a_failure() {
        let snapshot = parse(MISSING_USAGE_FIELDS, at(1_800_000_000)).expect("parses");
        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn the_quota_list_fixture_classifies_its_entries_by_length() {
        let snapshot = parse(DISTINCT_QUOTA_WINDOWS, at(1_800_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["w14400", "w2592000"]);
        assert_eq!(window(&snapshot, "w14400").used_percent, 0.0);
        assert_eq!(
            window(&snapshot, "w14400").subtitle.as_deref(),
            Some("0/100 credits")
        );
        assert_eq!(
            window(&snapshot, "w14400").title,
            "4-hour quota",
            "the entry classifies as the rolling window, so it takes the source's own label"
        );
        assert_eq!(window(&snapshot, "w2592000").used_percent, 0.0);
        assert_eq!(
            window(&snapshot, "w2592000").subtitle.as_deref(),
            Some("0/100 credits")
        );
    }

    #[test]
    fn a_percent_of_one_stays_one_and_a_remaining_reads_as_used() {
        let used = parse(PERCENT_USED_ONE, at(1_800_000_000)).expect("parses");
        assert_eq!(used.windows.len(), 1);
        let rolling = window(&used, "w14400");
        assert_eq!(rolling.used_percent, 1.0, "1 is a percent, not a fraction");
        assert_eq!(
            rolling.subtitle, None,
            "no absolutes ride a percent-only window"
        );
        assert_eq!(
            rolling.length.expect("the rolling default").as_secs(),
            14_400
        );

        let remaining = parse(PERCENT_REMAINING_ONE, at(1_800_000_000)).expect("parses");
        assert_eq!(window(&remaining, "w14400").used_percent, 99.0);
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names; a body that is not an object or an
        // array at all; and the recorded rolling window with its consumption a string
        // where a number belongs — a quota we cannot measure must not be drawn as one
        // we did.
        let string_where_number = r#"
        { "rolling_window": { "requests": "many", "limit": 100 } }
        "#;
        for body in ["{\"partial\":", "5", string_where_number] {
            let error = parse(body, at(1_800_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn an_entry_of_an_unrecognised_kind_is_skipped_and_an_unreadable_one_refused() {
        let unknown_kind = r#"
        { "quotas": [
            { "used": 0, "limit": 100, "window_minutes": 240 },
            { "kind": "mystery", "note": true }
        ] }
        "#;
        let snapshot = parse(unknown_kind, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key.as_str(), "w14400");

        let unreadable = r#"
        { "quotas": [ { "name": "Odd", "used": 7 } ] }
        "#;
        assert!(
            matches!(
                parse(unreadable, at(1_800_000_000)),
                Err(ProviderError::Malformed(_))
            ),
            "used without a limit or a percentage is not a readable quota"
        );
    }

    #[test]
    fn byte_identical_quota_entries_collapse_to_one_window() {
        // The recorded 240-minute entry standing twice: the source deduplicates the
        // walked objects, so two byte-identical entries are one window, not a collision.
        let body = r#"
        { "quotas": [
            { "used": 0, "limit": 100, "window_minutes": 240 },
            { "used": 0, "limit": 100, "window_minutes": 240 }
        ] }
        "#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key.as_str(), "w14400");
    }

    #[test]
    fn the_same_length_from_two_sections_draws_both_windows_keyed_by_pool() {
        // Spliced, not recorded: the recorded active-subscription body beside the
        // recorded 240-minute quota entry. Both shapes are individually real, so a body
        // carrying both is plausible — and there, the rolling window and the quota entry
        // state the same 240-minute length against different quotas.
        let body = r#"
            {
              "subscription": {
                "active": true,
                "plan_name": "Pro",
                "current_period_end": "2026-07-01T00:00:00Z"
              },
              "monthly": {
                "used": 250,
                "limit": 1000,
                "resets_at": "2026-07-01T00:00:00Z",
                "unit": "credits"
              },
              "rolling_window": {
                "requests": 40,
                "limit": 100,
                "window_minutes": 240,
                "reset_at": "2026-06-13T18:00:00Z",
                "unit": "requests"
              },
              "quotas": [
                { "used": 0, "limit": 100, "window_minutes": 240 }
              ]
            }
            "#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            ["rolling/w14400", "w2592000", "quotas/w14400"],
            "only the contested four-hour span takes a pool; the month keeps its length key"
        );
        assert_eq!(window(&snapshot, "rolling/w14400").used_percent, 40.0);
        assert_eq!(window(&snapshot, "quotas/w14400").used_percent, 0.0);
    }

    #[test]
    fn the_same_length_twice_in_one_section_is_still_refused() {
        // The recorded quota entries' shape at one shared, unclassifiable length: two
        // label-less five-hour entries inside one `quotas` list leave only position to
        // tell them apart, and no pool separates them.
        let body = r#"
        { "quotas": [
            { "used": 0, "limit": 100, "window_minutes": 300 },
            { "used": 10, "limit": 100, "window_minutes": 300 }
        ] }
        "#;
        let error = parse(body, at(1_800_000_000)).expect_err("contested within one section");
        assert!(
            matches!(error, ProviderError::Malformed(_)),
            "{error} for {body}"
        );
    }

    #[test]
    fn the_spec_polls_the_subscription_usage_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.chutes.ai/users/me/subscription_usage"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "Chutes has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
