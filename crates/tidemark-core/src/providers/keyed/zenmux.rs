//! ZenMux.
//!
//! Ported from CodexBar's Swift fetcher, `Providers/ZenMux/ZenMuxUsageFetcher.swift`;
//! there is no JS plugin. Never seen answering: every number in the tests is a body
//! CodexBar recorded.
//!
//! # The Management API, and what one request buys
//!
//! The key is a *Management API* key — ZenMux's error copy insists standard inference
//! keys are not supported. CodexBar makes its second request, `payg/balance`, only when
//! the interface wants the pay-as-you-go balance beside the quotas; [`Spec`] spends one
//! request, so this port polls `subscription/detail` alone and the PAYG balance is not
//! ported. A rejected key arrives as HTTP 401/403, which the shared transport maps;
//! an envelope whose `success` is false is refused here as unreadable, exactly as the
//! source's `parseFailed`.
//!
//! # A fraction named a percentage
//!
//! The field is called `usage_percentage` but carries a **fraction**: 0.0715 reads as
//! 7.15%, and every value is scaled by 100 and clamped before it draws. The two
//! windows are named `quota_5_hour` and `quota_7_day`, and neither states a length —
//! the five hours and the seven days are the names, so the windows take those lengths
//! as constants, as the source does. The absolutes are flows (`57.20 / 800 flows`,
//! with spaces around the slash as the source spells it): whole amounts render with no
//! decimals, fractional ones keep two. `resets_at` may carry fractional seconds, which
//! a whole-second timestamp floors. The payload's third quota (`quota_monthly`) states
//! no consumption and is read by nobody, in the source or here.
//!
//! The plan section carries the tier as CodexBar's identity does (`Ultra plan`), the
//! account status *only when it is not healthy* (`Ultra plan · Monitored` there, a
//! Status row here), and the plan's expiry as a date.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, parse_rfc3339, title_case};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "zenmux";

const SUBSCRIPTION_DETAIL_URL: &str = "https://zenmux.ai/api/v1/management/subscription/detail";

/// The five-hour window's length, which the wire names but never states.
const FIVE_HOURS: u64 = 5 * 3_600;
/// The weekly window's length, likewise.
const SEVEN_DAYS: u64 = 7 * 86_400;

#[derive(Debug, Deserialize)]
struct Envelope {
    success: bool,
    data: DataPayload,
}

#[derive(Debug, Deserialize)]
struct DataPayload {
    plan: Plan,
    #[serde(rename = "account_status")]
    account_status: String,
    #[serde(rename = "quota_5_hour")]
    quota_5_hour: Quota,
    #[serde(rename = "quota_7_day")]
    quota_7_day: Quota,
}

#[derive(Debug, Deserialize)]
struct Plan {
    tier: String,
    #[serde(default, rename = "expires_at")]
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Quota {
    /// A fraction despite the name: 0.0715 is 7.15%.
    #[serde(rename = "usage_percentage")]
    usage_percentage: f64,
    #[serde(default, rename = "resets_at")]
    resets_at: Option<String>,
    #[serde(rename = "max_flows")]
    max_flows: f64,
    #[serde(rename = "used_flows")]
    used_flows: f64,
    /// Required on the wire and then read by no one, as in the source.
    #[serde(rename = "remaining_flows")]
    #[allow(dead_code)]
    remaining_flows: f64,
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    if !envelope.success {
        return Err(ProviderError::malformed(
            "the subscription response reported failure",
        ));
    }
    let data = envelope.data;

    let windows = vec![
        quota_window(&data.quota_5_hour, "5-hour quota", FIVE_HOURS),
        quota_window(&data.quota_7_day, "Weekly quota", SEVEN_DAYS),
    ];

    let tier = data.plan.tier.trim();
    let status = data.account_status.trim();
    let mut plan_rows = Vec::new();
    if !tier.is_empty() {
        plan_rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: format!("{} plan", title_case(tier)),
        });
    }
    // A healthy status is the absence of news; anything else is worth a row, as the
    // source's identity carries it beside the plan.
    if !status.is_empty() && !status.eq_ignore_ascii_case("healthy") {
        plan_rows.push(DetailRow {
            label: "Status".to_owned(),
            value: title_case(status),
        });
    }
    if let Some(at) = data.plan.expires_at.as_deref().and_then(parse_rfc3339) {
        plan_rows.push(DetailRow {
            label: "Expires".to_owned(),
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

/// One quota as a window: the fraction scaled and clamped, the flows under the bar,
/// and the length the quota's own name states.
fn quota_window(quota: &Quota, title: &str, secs: u64) -> Window {
    let length = WindowLength::from_secs(secs).expect("a fixed span is not zero");
    Window {
        key: WindowKey::for_length(length),
        title: title.to_owned(),
        subtitle: Some(format!(
            "{} / {} flows",
            amount(quota.used_flows),
            amount(quota.max_flows)
        )),
        used_percent: (quota.usage_percentage * 100.0).clamp(0.0, 100.0),
        // The source discards a date it cannot read, so the window draws without its
        // pace mark rather than failing the fetch.
        resets_at: quota.resets_at.as_deref().and_then(parse_rfc3339),
        length: Some(length),
    }
}

/// A flow count as the source formats it: whole amounts with no decimals, fractional
/// ones with two.
fn amount(value: f64) -> String {
    if value.round() == value {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
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

/// ZenMux as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "ZenMux",
    endpoint: |_| SUBSCRIPTION_DETAIL_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "ZenMux platform → Management API key (an inference key will not do).",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use tidemark_types::{Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `ZenMuxProviderTests.swift` — "subscription and balance map
    /// to quota windows and USD PAYG". CodexBar asserts primary 7.15% at 300 minutes
    /// with "57.20 / 800 flows", secondary 6.73% at 10,080 minutes with
    /// "416.11 / 6182 flows", the "Ultra plan" identity, and the expiry. The body also
    /// carries `quota_monthly` and the per-flow USD fields, which CodexBar reads for
    /// neither window.
    const SUBSCRIPTION: &str = r#"
    {
      "success": true,
      "data": {
        "plan": {
          "tier": "ultra",
          "amount_usd": 200,
          "interval": "month",
          "expires_at": "2026-04-12T08:26:56.000Z"
        },
        "currency": "usd",
        "base_usd_per_flow": 0.03283,
        "effective_usd_per_flow": 0.03283,
        "account_status": "healthy",
        "quota_5_hour": {
          "usage_percentage": 0.0715,
          "resets_at": "2026-03-24T08:35:09.000Z",
          "max_flows": 800,
          "used_flows": 57.2,
          "remaining_flows": 742.8,
          "used_value_usd": 1.88,
          "max_value_usd": 26.27
        },
        "quota_7_day": {
          "usage_percentage": 0.0673,
          "resets_at": "2026-03-26T02:15:05.000Z",
          "max_flows": 6182,
          "used_flows": 416.11,
          "remaining_flows": 5765.89,
          "used_value_usd": 13.66,
          "max_value_usd": 202.99
        },
        "quota_monthly": {
          "max_flows": 34560,
          "max_value_usd": 1134.33
        }
      }
    }
    "#;

    /// Recorded by CodexBar, same file — "unhealthy account status is included in
    /// identity": the fixture above with `account_status` replaced by "monitored".
    /// CodexBar asserts the identity reads "Ultra plan · Monitored".
    const MONITORED: &str = r#"
    {
      "success": true,
      "data": {
        "plan": {
          "tier": "ultra",
          "amount_usd": 200,
          "interval": "month",
          "expires_at": "2026-04-12T08:26:56.000Z"
        },
        "currency": "usd",
        "base_usd_per_flow": 0.03283,
        "effective_usd_per_flow": 0.03283,
        "account_status": "monitored",
        "quota_5_hour": {
          "usage_percentage": 0.0715,
          "resets_at": "2026-03-24T08:35:09.000Z",
          "max_flows": 800,
          "used_flows": 57.2,
          "remaining_flows": 742.8,
          "used_value_usd": 1.88,
          "max_value_usd": 26.27
        },
        "quota_7_day": {
          "usage_percentage": 0.0673,
          "resets_at": "2026-03-26T02:15:05.000Z",
          "max_flows": 6182,
          "used_flows": 416.11,
          "remaining_flows": 5765.89,
          "used_value_usd": 13.66,
          "max_value_usd": 202.99
        },
        "quota_monthly": {
          "max_flows": 34560,
          "max_value_usd": 1134.33
        }
      }
    }
    "#;

    /// Recorded by CodexBar, same file — "malformed subscription payload fails parsing":
    /// a plan block with no tier in it.
    const PLANLESS: &str = r#"{"success":true,"data":{"plan":{}}}"#;

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
    fn the_subscription_fixture_draws_both_quota_windows() {
        let snapshot = parse(SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["w18000", "w604800"]);

        let five_hours = window(&snapshot, "w18000");
        assert_eq!(five_hours.title, "5-hour quota");
        assert!(
            (five_hours.used_percent - 7.15).abs() < 1e-9,
            "usage_percentage 0.0715 is a fraction, scaled by 100"
        );
        assert_eq!(
            five_hours.subtitle.as_deref(),
            Some("57.20 / 800 flows"),
            "fractional amounts keep their decimals, whole ones do not"
        );
        assert_eq!(
            five_hours.resets_at,
            Some(at(1_774_341_309)),
            "2026-03-24T08:35:09Z, the reset CodexBar's own test reads"
        );
        assert_eq!(
            five_hours.length.expect("the five-hour default").as_secs(),
            18_000
        );

        let weekly = window(&snapshot, "w604800");
        assert_eq!(weekly.title, "Weekly quota");
        assert!((weekly.used_percent - 6.73).abs() < 1e-9);
        assert_eq!(weekly.subtitle.as_deref(), Some("416.11 / 6182 flows"));
        assert_eq!(weekly.resets_at, Some(at(1_774_491_305)));
        assert_eq!(
            weekly.length.expect("the weekly default").as_secs(),
            604_800
        );

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "w18000",
            "the card leads with the five-hour window, as CodexBar's primary does"
        );

        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Ultra plan");
        assert_eq!(row(&snapshot, "Plan", "Expires").value, "2026-04-12");
        assert!(
            snapshot
                .details
                .iter()
                .flat_map(|section| &section.rows)
                .all(|row| row.label != "Status"),
            "a healthy status draws no row, as in CodexBar's identity"
        );
    }

    #[test]
    fn an_unhealthy_status_becomes_a_row() {
        let snapshot = parse(MONITORED, at(1_800_000_000)).expect("parses");
        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Ultra plan");
        assert_eq!(
            row(&snapshot, "Plan", "Status").value,
            "Monitored",
            "CodexBar shows this beside the plan as \"Ultra plan · Monitored\""
        );
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names; the recorded plan-less body; the
        // recorded shape with a consumption field a string where a number belongs; and a
        // body whose envelope reports failure.
        let string_where_number = r#"
        { "success": true, "data": { "plan": { "tier": "ultra" },
           "account_status": "healthy",
           "quota_5_hour": { "usage_percentage": 0.0715, "max_flows": 800,
             "used_flows": "many", "remaining_flows": 742.8 },
           "quota_7_day": { "usage_percentage": 0.0673, "max_flows": 6182,
             "used_flows": 416.11, "remaining_flows": 5765.89 } } }
        "#;
        let reported_failure = r#"{ "success": false, "data": { "plan": { "tier": "ultra" } } }"#;
        for body in [
            "{\"partial\":",
            PLANLESS,
            string_where_number,
            reported_failure,
        ] {
            let error = parse(body, at(1_800_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_field_of_a_kind_this_parser_does_not_read_is_skipped() {
        // The recorded body already carries `quota_monthly`, `amount_usd` and the
        // per-flow USD fields, none of which CodexBar reads for a window. One more
        // invented field is skipped the same way.
        let body = SUBSCRIPTION.replacen(
            "\"currency\": \"usd\",",
            "\"alerts\": {\"kind\": \"quota\"},\n        \"currency\": \"usd\",",
            1,
        );
        let snapshot = parse(&body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert!((window(&snapshot, "w18000").used_percent - 7.15).abs() < 1e-9);
    }

    #[test]
    fn the_spec_polls_the_subscription_detail_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://zenmux.ai/api/v1/management/subscription/detail"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "ZenMux has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
