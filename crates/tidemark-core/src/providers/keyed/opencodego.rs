//! OpenCode Go.
//!
//! Ported from CodexBar's `OpenCodeGo/OpenCodeGoUsageFetcher.swift`, `fetchAPIUsage` and
//! the JSON half of `parseSubscription`. Never seen answering: every number in the tests is
//! a body CodexBar recorded.
//!
//! # What is not ported, and why
//!
//! CodexBar reaches this provider three ways: the Zen API key, a browser cookie against the
//! workspace page, and a local SQLite history. Only the key is here. The other two carry the
//! two fallbacks that make `parseSubscription` five hundred lines — a regular-expression
//! sweep over a seroval hydration script, and a recursive hunt for *any* object anywhere in
//! the document that has a percentage and a countdown in it, matched to a window by
//! substrings of the path it was found at. Neither belongs on this path: the endpoint
//! answers JSON, and a window located by guessing at a path is a window drawn at a length
//! nothing said it had. The Zen balance is a second request behind a cookie and is out of
//! scope with them.
//!
//! # What the payload does not tell you
//!
//! **The lengths are not on the wire.** Five hours, seven days and thirty days are the
//! source's own table for the three windows, keyed off their names.
//!
//! **A reset is a countdown, not an instant.** `resetInSec` is seconds from the moment of
//! the poll, so the absolute time this files depends on when the request went out.
//! `resetAt` is the alternative and is read as an instant. Where both are absent the window
//! is drawn without a pace mark, which is the honest shape; where `resetAt` is present but
//! unreadable it is dropped rather than refused, because CodexBar pins that tolerance in a
//! test of its own — the field has been seen carrying `1e309`.
//!
//! **A percentage may be a fraction.** A *stated* percent of `0.9` means 90%, not 0.9%:
//! the source scales anything in `0..=1` by a hundred. A percent *computed* from `used` and
//! `limit` is already out of a hundred and must not be scaled again, which is why the two
//! paths are kept apart below.
//!
//! # Where this port is stricter than its source
//!
//! CodexBar drops an unreadable monthly window silently and keeps the rest. Tidemark's rule
//! is that a provider never silently drops a window: a recognised window that cannot be read
//! fails the whole fetch, because a card missing a bar reads as "you have no such limit".
//! An absent window is still absent, not an error — only the rolling one is required, which
//! is the source's own condition for having a reading at all.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, length_title, parse_rfc3339};
use serde_json::{Map, Value};
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "opencodego";

/// The Zen API's usage endpoint — the whole of the key-authenticated contract.
const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// The three windows, in the order they are drawn: the key spellings the source accepts,
/// what to key and title the window by, and how long the source says it lasts.
const WINDOWS: &[(&[&str], u64)] = &[
    (
        &[
            "rollingUsage",
            "rolling",
            "rolling_usage",
            "rollingWindow",
            "rolling_window",
        ],
        5 * 3_600,
    ),
    (
        &[
            "weeklyUsage",
            "weekly",
            "weekly_usage",
            "weeklyWindow",
            "weekly_window",
        ],
        7 * 86_400,
    ),
    (
        &[
            "monthlyUsage",
            "monthly",
            "monthly_usage",
            "monthlyWindow",
            "monthly_window",
        ],
        30 * 86_400,
    ),
];

/// Envelope keys the windows have been seen sitting one level down inside.
const ENVELOPE_KEYS: &[&str] = &["data", "result", "usage", "billing", "payload"];

/// Spellings of a stated share. First one present wins, and the value is a fraction when it
/// is not greater than one — see the module doc.
const PERCENT_KEYS: &[&str] = &[
    "usagePercent",
    "usedPercent",
    "percentUsed",
    "percent",
    "usage_percent",
    "used_percent",
    "utilization",
    "utilizationPercent",
    "utilization_percent",
    "usage",
];

/// Spellings of the consumed half of a share.
const USED_KEYS: &[&str] = &["used", "usage", "consumed", "count", "usedTokens"];

/// Spellings of the allowance a share is out of.
const LIMIT_KEYS: &[&str] = &["limit", "total", "quota", "max", "cap", "tokenLimit"];

/// Spellings of a reset stated as a countdown in seconds.
const RESET_IN_KEYS: &[&str] = &[
    "resetInSec",
    "resetInSeconds",
    "resetSeconds",
    "reset_sec",
    "reset_in_sec",
    "resetsInSec",
    "resetsInSeconds",
    "resetIn",
    "resetSec",
];

/// Spellings of a reset stated as an instant.
const RESET_AT_KEYS: &[&str] = &[
    "resetAt",
    "resetsAt",
    "reset_at",
    "resets_at",
    "nextReset",
    "next_reset",
    "renewAt",
    "renew_at",
];

/// Spellings of the day the subscription itself renews.
const RENEW_AT_KEYS: &[&str] = &["renewAt", "renew_at"];

/// The first of these keys the object carries, whatever its type.
fn first_value<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| map.get(*key))
}

/// The first of these keys whose value is an object.
fn first_object<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(Value::as_object))
}

/// A number, whether it arrived as one or as a string holding one. Non-finite is no number.
fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// The first of these keys that holds a number.
fn first_number(map: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| number(map.get(*key)))
}

/// An instant, in any of the spellings the source accepts: epoch milliseconds above the
/// year 33658, epoch seconds above 2001, otherwise RFC-3339. An implausible instant is no
/// instant — see [`Timestamp::from_unix`].
fn instant(value: Option<&Value>) -> Option<Timestamp> {
    let value = value?;
    if let Some(seconds) = number(Some(value)) {
        if seconds > 1_000_000_000_000.0 {
            return Timestamp::from_unix_millis(seconds as i64).ok();
        }
        if seconds > 1_000_000_000.0 {
            return Timestamp::from_unix(seconds as i64).ok();
        }
        return None;
    }
    parse_rfc3339(value.as_str()?)
}

/// The object carrying the three windows, found by the presence of a rolling window.
///
/// `usage` is descended into before the object itself is considered, the way the source
/// does it: a document with windows at both levels means the inner ones.
fn locate(map: &Map<String, Value>) -> Option<&Map<String, Value>> {
    if let Some(inner) = map.get("usage").and_then(Value::as_object)
        && let Some(found) = locate(inner)
    {
        return Some(found);
    }
    first_object(map, WINDOWS[0].0).map(|_| map)
}

/// One window's share and reset, or `Malformed` when the object is there and unreadable.
fn window_of(
    map: &Map<String, Value>,
    seconds: u64,
    captured_at: Timestamp,
) -> Result<Window, ProviderError> {
    // A stated share and a computed one are scaled differently, so which one this is has to
    // survive the lookup. See the module doc.
    let used_percent = match first_number(map, PERCENT_KEYS) {
        Some(stated) if (0.0..=1.0).contains(&stated) => stated * 100.0,
        Some(stated) => stated,
        None => {
            let used = first_number(map, USED_KEYS);
            let limit = first_number(map, LIMIT_KEYS).filter(|limit| *limit > 0.0);
            match (used, limit) {
                (Some(used), Some(limit)) => used / limit * 100.0,
                _ => {
                    return Err(ProviderError::malformed(
                        "a usage window carried neither a share nor a used-over-limit pair",
                    ));
                }
            }
        }
    };

    let length = WindowLength::from_secs(seconds).expect("the table holds no zero lengths");
    let resets_at = match first_number(map, RESET_IN_KEYS) {
        // A countdown, not an instant: the absolute time depends on when the poll went out.
        Some(countdown) => Some(captured_at.saturating_add_seconds(countdown.max(0.0) as i64)),
        None => instant(first_value(map, RESET_AT_KEYS)),
    };

    Ok(Window {
        key: WindowKey::for_length(length),
        title: length_title(length),
        subtitle: None,
        used_percent: used_percent.clamp(0.0, 100.0),
        resets_at,
        length: Some(length),
    })
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("the usage response is not an object"))?;

    let usage = locate(root)
        .or_else(|| {
            ENVELOPE_KEYS
                .iter()
                .filter_map(|key| root.get(*key).and_then(Value::as_object))
                .find_map(locate)
        })
        .ok_or_else(|| ProviderError::malformed("no rolling usage window in the response"))?;

    let mut windows = Vec::with_capacity(WINDOWS.len());
    for (keys, seconds) in WINDOWS {
        // An absent window is absent, not an error. A present one that cannot be read fails
        // the fetch: see the module doc on where this port is stricter than its source.
        let Some(entry) = first_object(usage, keys) else {
            continue;
        };
        windows.push(window_of(entry, *seconds, captured_at)?);
    }

    // The renewal day is the subscription's, not a window's: the source draws it as a bar
    // at nought per cent, which would read here as an untouched quota. A row says the same
    // thing without claiming there is an allowance behind it. The inner object's own value
    // wins over the envelope's, the way the source resolves it.
    let renews_at = instant(first_value(usage, RENEW_AT_KEYS))
        .or_else(|| instant(first_value(root, RENEW_AT_KEYS)));
    let details = match renews_at {
        Some(at) => vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Renews".to_owned(),
                value: day_of(at),
            }],
        }],
        None => Vec::new(),
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
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

/// OpenCode Go as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "OpenCode Go",
    endpoint: |_| USAGE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "opencode.ai → Zen → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape CodexBar records for the three-window case, as JSON rather than as the
    /// seroval script its own fixture is written in — the API answers the same fields.
    /// `OpenCodeGoUsageParserTests.swift`, "parses subscription usage from seroval
    /// response": 17%/5944s, 75%/278201s, 91%/880201s, and a `status` nobody reads.
    const THREE_WINDOWS: &str = r#"{"rollingUsage":{"status":"ok","resetInSec":5944,"usagePercent":17},
        "weeklyUsage":{"status":"ok","resetInSec":278201,"usagePercent":75},
        "monthlyUsage":{"status":"ok","resetInSec":880201,"usagePercent":91}}"#;

    /// Recorded by CodexBar, same file — "parses rolling only usage from JSON response".
    const ROLLING_ONLY: &str = r#"{"usage":{"rollingUsage":{"usagePercent":25,"resetInSec":600}}}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_three_window_fixture_draws_the_lengths_the_source_names() {
        let now = at(1_800_000_000);
        let snapshot = parse(THREE_WINDOWS, now).expect("parses");
        let lengths: Vec<u64> = snapshot
            .windows
            .iter()
            .map(|w| {
                w.length
                    .expect("every length comes from the table")
                    .as_secs()
            })
            .collect();
        assert_eq!(lengths, [18_000, 604_800, 2_592_000]);
        let percents: Vec<f64> = snapshot.windows.iter().map(|w| w.used_percent).collect();
        assert_eq!(percents, [17.0, 75.0, 91.0]);
        assert_eq!(
            snapshot.windows[0]
                .resets_at
                .expect("a countdown was given"),
            now.saturating_add_seconds(5_944),
            "resetInSec counts from the poll, not from an epoch"
        );
        assert_eq!(
            snapshot.windows[2]
                .resets_at
                .expect("a countdown was given"),
            now.saturating_add_seconds(880_201)
        );
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        assert!(snapshot.details.is_empty(), "nothing said when it renews");
    }

    #[test]
    fn a_rolling_only_response_draws_one_window_and_no_others() {
        let snapshot = parse(ROLLING_ONLY, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1, "absent is absent, not an error");
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
        assert_eq!(
            snapshot.windows[0].length.expect("derived").as_secs(),
            18_000
        );
    }

    #[test]
    fn a_response_with_no_rolling_window_is_malformed() {
        // CodexBar's "parse subscription throws when required fields are missing": a
        // monthly window on its own is not a reading.
        let body = r#"{"monthlyUsage":{"usagePercent":50,"resetInSec":123}}"#;
        assert!(matches!(
            parse(body, at(1_800_000_000)),
            Err(ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn a_window_we_cannot_read_fails_the_whole_fetch() {
        let body = r#"{"rollingUsage":{"usagePercent":10,"resetInSec":600},
            "monthlyUsage":{"usagePercent":"ninety","resetInSec":86400}}"#;
        assert!(
            matches!(
                parse(body, at(1_800_000_000)),
                Err(ProviderError::Malformed(_))
            ),
            "the source drops this window silently; a missing bar reads as 'no such limit'"
        );
        assert!(matches!(
            parse("{\"partial\":", at(1_800_000_000)),
            Err(ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn a_stated_share_below_one_is_a_fraction_and_a_computed_one_is_not() {
        // CodexBar's "parses subscription from JSON with reset at and ratio percentages":
        // 0.25 means 25%, 0.9 means 90%.
        let ratios = r#"{"usage":{"rollingUsage":{"usagePercent":0.25,"resetInSec":3600},
            "weeklyUsage":{"usagePercent":75,"resetInSec":7200},
            "monthlyUsage":{"usagePercent":0.9,"resetInSec":86400}}}"#;
        let snapshot = parse(ratios, at(1_800_000_000)).expect("parses");
        let percents: Vec<f64> = snapshot.windows.iter().map(|w| w.used_percent).collect();
        assert_eq!(percents, [25.0, 75.0, 90.0]);

        // "computes usage percent from totals and treats monthly as optional": 25 of 100 is
        // 25%, and 50 of 200 is 25% — neither is multiplied by a hundred a second time.
        let totals = r#"{"rollingUsage":{"used":25,"limit":100,"resetInSec":600},
            "weeklyUsage":{"used":50,"limit":200,"resetInSec":3600}}"#;
        let snapshot = parse(totals, at(1_800_000_000)).expect("parses");
        let percents: Vec<f64> = snapshot.windows.iter().map(|w| w.used_percent).collect();
        assert_eq!(percents, [25.0, 25.0]);
    }

    #[test]
    fn an_impossible_share_is_clamped_to_the_bar() {
        // CodexBar's "clamps invalid percentages".
        let body = r#"{"rollingUsage":{"usagePercent":150,"resetInSec":60},
            "weeklyUsage":{"usagePercent":-10,"resetInSec":120}}"#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(snapshot.windows[1].used_percent, 0.0);
    }

    #[test]
    fn a_reset_instant_outside_the_integer_range_leaves_the_window_without_a_pace_mark() {
        // CodexBar's "ignores reset timestamps outside integer range", both arguments.
        for absurd in ["1e309", "1e308"] {
            let body = format!(
                r#"{{"rollingUsage":{{"usagePercent":17,"resetAt":"{absurd}"}},
                    "weeklyUsage":{{"usagePercent":75,"resetInSec":7200}}}}"#
            );
            let snapshot = parse(&body, at(1_800_000_000)).expect("parses");
            assert_eq!(snapshot.windows[0].used_percent, 17.0);
            assert!(
                snapshot.windows[0].resets_at.is_none(),
                "{absurd} is not a date; the source drops it rather than refusing the body"
            );
            assert_eq!(snapshot.windows[1].used_percent, 75.0);
        }
    }

    #[test]
    fn a_reset_stated_as_an_instant_is_read_as_one() {
        let body = r#"{"rollingUsage":{"usagePercent":10,"resetAt":"2027-01-15T10:20:30Z"}}"#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(
            snapshot.windows[0].resets_at.expect("stated"),
            at(1_800_008_430)
        );
    }

    #[test]
    fn the_renewal_day_is_a_row_and_the_inner_value_wins() {
        // CodexBar's "child renewAt overrides parent renewAt".
        let body = r#"{"renewAt":"2027-01-01T00:00:00Z","usage":{"renewAt":"2027-02-15T00:00:00Z",
            "rollingUsage":{"usagePercent":10,"resetInSec":600}}}"#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, DetailSection::PLAN);
        assert_eq!(snapshot.details[0].rows[0].label, "Renews");
        assert_eq!(snapshot.details[0].rows[0].value, "2027-02-15");

        // "top level renewAt is preserved for nested usage object".
        let inherited = r#"{"renew_at":"2027-01-01T00:00:00Z","usage":{
            "rollingUsage":{"usagePercent":10,"resetInSec":600}}}"#;
        let snapshot = parse(inherited, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.details[0].rows[0].value, "2027-01-01");
    }

    #[test]
    fn the_spec_polls_the_documented_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::Options;
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://opencode.ai/zen/go/v1/usage"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.id, PROVIDER_ID);
    }
}
