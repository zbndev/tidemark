//! ClawRouter.
//!
//! Ported from CodexBar's `clawrouter.js` plugin. Never seen answering: every number in
//! the tests is a body CodexBar recorded.
//!
//! # Micros, a window key, and no calendar
//!
//! Every amount on this endpoint is an integer count of micro-dollars, and the money
//! never passes through an `f64` dollar on its way to the screen: dollars and cents are
//! split off the integer (spent and actual cost at six decimals, the limit at two),
//! because a dollar amount like 24.994 is not exactly representable and a subtitle that
//! reads `$24.993999` would be a lie told by a formatter. The window's percentage alone
//! divides, and it divides micros by micros.
//!
//! The budget window is monthly, and the month is *named*, not measured:
//! `budget.windowKey` ends in `YYYY-MM`, and the reset is the first instant of the next
//! month — December rolling into January is handled, and a `windowKey` that names no
//! month yields a window with no pace mark rather than a guessed one. A month is not a
//! fixed span of seconds; the 30-day length this port uses is the same convention the
//! ClinePass port pinned its monthly window to, and the pace mark it yields is that
//! approximate.
//!
//! An unmetered policy (`budget.configured: false`, no limit on the wire) draws no
//! window — there is nothing to divide by — and the actual cost rides the rows.
//!
//! # The base URL, read the friendly way
//!
//! The host comes from the account's `base_url` option or the public service, with a
//! trailing slash trimmed and `/v1` appended when it is not already there. Where the
//! shared reader would refuse a value — plain HTTP to a remote host, bare words — this
//! provider's own policy falls back to the default host rather than failing the card,
//! because a typo in one setting should not read as a provider outage. Loopback HTTP
//! stands: that is how a self-hosted router is reached.

use super::{Auth, Method, OptionSchema, Spec, base_url};
use crate::providers::{ProviderError, length_title};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "clawrouter";

/// The public service, and the fallback when the account's value is unusable.
const DEFAULT_HOST: &str = "https://clawrouter.openclaw.ai";

/// A month, the same convention the ClinePass port pinned its monthly window to. See
/// the module doc: a calendar month is not a fixed span, so the pace mark computed from
/// this length is approximate.
const MONTH_SECS: u64 = 30 * 86_400;

#[derive(Debug, Deserialize)]
struct Envelope {
    budget: Budget,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Budget {
    configured: bool,
    ledger: String,
    window_key: Option<String>,
    limit_micros: Option<i64>,
    spent_micros: Option<i64>,
    remaining_micros: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    summary: Summary,
    providers: Vec<Provider>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    request_count: i64,
    success_count: i64,
    error_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    actual_cost_micros: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Provider {
    provider: String,
    request_count: i64,
    /// Validation only, exactly as the plugin reads them: a routed provider whose
    /// counts are not integers fails the fetch, even though no row ever shows them.
    #[allow(dead_code)]
    success_count: i64,
    #[allow(dead_code)]
    error_count: i64,
    total_tokens: i64,
    actual_cost_micros: i64,
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    let mut windows = Vec::new();
    let mut usage_rows = vec![
        DetailRow {
            label: "Requests".to_owned(),
            value: format!(
                "{} · {} succeeded · {} failed",
                envelope.usage.summary.request_count,
                envelope.usage.summary.success_count,
                envelope.usage.summary.error_count
            ),
        },
        DetailRow {
            label: "Tokens".to_owned(),
            value: format!(
                "{} · {} input · {} output",
                envelope.usage.summary.total_tokens,
                envelope.usage.summary.input_tokens,
                envelope.usage.summary.output_tokens
            ),
        },
        DetailRow {
            label: "Actual cost".to_owned(),
            value: dollars6(envelope.usage.summary.actual_cost_micros),
        },
        DetailRow {
            label: "Budget ledger".to_owned(),
            value: envelope.budget.ledger.clone(),
        },
    ];

    // A budget with both absolutes is the monthly window; the reset comes from the
    // window key, not the wire.
    if let (Some(spent), Some(limit)) = (envelope.budget.spent_micros, envelope.budget.limit_micros)
    {
        let length = WindowLength::from_secs(MONTH_SECS).expect("a month is not zero seconds");
        let resets_at = monthly_reset(envelope.budget.window_key.as_deref())?;
        let mut value = format!("{} / {}", dollars6(spent), dollars2(limit));
        if let Some(remaining) = envelope.budget.remaining_micros {
            value.push_str(&format!(" · {} remaining", dollars6(remaining)));
        }
        usage_rows.push(DetailRow {
            label: "Monthly budget".to_owned(),
            value,
        });
        if limit > 0 {
            // The percentage divides micros by micros; no dollar value is involved.
            let percent = (spent as f64 / limit as f64 * 100.0).clamp(0.0, 100.0);
            windows.push(Window {
                key: WindowKey::for_length(length),
                title: length_title(length),
                subtitle: Some(format!("{} / {}", dollars6(spent), dollars2(limit))),
                used_percent: percent,
                resets_at,
                length: Some(length),
            });
        }
    }

    let mut details = vec![DetailSection {
        title: "Usage".to_owned(),
        rows: usage_rows,
    }];

    // Routed providers, sorted the way the plugin sorts them: cost, then requests,
    // then name. The plugin caps the rows at twenty; so does this port.
    let mut routed: Vec<&Provider> = envelope.usage.providers.iter().collect();
    routed.sort_by(|a, b| {
        b.actual_cost_micros
            .cmp(&a.actual_cost_micros)
            .then(b.request_count.cmp(&a.request_count))
            .then(a.provider.cmp(&b.provider))
    });
    if !routed.is_empty() {
        details.push(DetailSection {
            title: "Routed providers".to_owned(),
            rows: routed
                .iter()
                .take(20)
                .map(|item| DetailRow {
                    label: if item.provider.trim().is_empty() {
                        "Unknown".to_owned()
                    } else {
                        item.provider.trim().to_owned()
                    },
                    value: format!(
                        "{} requests · {} · {} tokens",
                        item.request_count,
                        dollars6(item.actual_cost_micros),
                        item.total_tokens
                    ),
                })
                .collect(),
        });
    }

    details.push(DetailSection {
        title: DetailSection::PLAN.to_owned(),
        rows: vec![DetailRow {
            label: "Budget".to_owned(),
            value: if envelope.budget.configured {
                "Managed monthly budget".to_owned()
            } else {
                "Unmetered".to_owned()
            },
        }],
    });

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
}

/// The first instant of the month after the one `windowKey` names, or `None` when the
/// key names no month. A key that names an impossible month is refused: the plugin's
/// own date parser rejects it, and a guessed reset is worse than none.
///
/// The key is read byte-wise throughout, as a shape check should be: a multibyte key
/// fails the digit check below the way ASCII garbage does — the plugin's regex simply
/// does not match it — rather than panicking on a char boundary the slicing never
/// looked for.
fn monthly_reset(window_key: Option<&str>) -> Result<Option<Timestamp>, ProviderError> {
    let Some(key) = window_key else {
        return Ok(None);
    };
    let bytes = key.as_bytes();
    if bytes.len() < 7 {
        return Ok(None);
    }
    let tail = &bytes[bytes.len() - 7..];
    let (year, month) = tail.split_at(4);
    let month = &month[1..];
    if !year.iter().all(|b| b.is_ascii_digit()) || !month.iter().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    // Slices of nothing but ASCII digits are valid UTF-8, so these reads cannot fail.
    let year: i32 = std::str::from_utf8(year)
        .expect("every byte is an ASCII digit")
        .parse()
        .map_err(|_| ProviderError::malformed("windowKey names no readable year"))?;
    let month: u32 = std::str::from_utf8(month)
        .expect("every byte is an ASCII digit")
        .parse()
        .map_err(|_| ProviderError::malformed("windowKey names no readable month"))?;
    if !(1..=12).contains(&month) {
        return Err(ProviderError::malformed(format!(
            "windowKey names the impossible month {month}"
        )));
    }
    let (year, month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let month = u8::try_from(month).expect("1..=12");
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).expect("1..=12"), 1)
            .map_err(|_| ProviderError::malformed("windowKey names no readable month"))?;
    let first = time::OffsetDateTime::new_utc(date, time::Time::MIDNIGHT);
    Timestamp::from_unix(first.unix_timestamp())
        .map(Some)
        .map_err(|_| ProviderError::malformed("the derived reset is not a plausible instant"))
}

/// Micro-dollars at six decimals, exactly: split off the integer, never through `f64`.
fn dollars6(micros: i64) -> String {
    format!(
        "${}.{:06}",
        micros / 1_000_000,
        micros.rem_euclid(1_000_000)
    )
}

/// Micro-dollars at two decimals, rounded half up from the exact integer.
fn dollars2(micros: i64) -> String {
    let cents = micros.div_euclid(10_000)
        + if micros.rem_euclid(10_000) >= 5_000 {
            1
        } else {
            0
        };
    format!("${}.{:02}", cents / 100, cents.rem_euclid(100))
}

/// ClawRouter as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "ClawRouter",
    endpoint: |options| {
        let mut base =
            base_url(options, "base_url", DEFAULT_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_owned());
        if !base.ends_with("/v1") {
            base.push_str("/v1");
        }
        format!("{base}/usage")
    },
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "ClawRouter policy page → API keys.",
    options: &[OptionSchema {
        name: "base_url",
        title: "Base URL",
        description: Some(
            "Host of a self-hosted ClawRouter; HTTPS only. The default is the public service.",
        ),
        default: DEFAULT_HOST,
        choices: &[],
        required: false,
    }],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use crate::providers::keyed::Options;
    use tidemark_types::{DetailRow, DetailSection, Snapshot, Timestamp};

    /// Recorded by CodexBar, `ClawRouterUsageFetcherTests.swift` — "monthly budget
    /// fixture matches the production golden". CodexBar asserts 0.024% used, the
    /// 2026-08-01 reset (the windowKey says 2026-07), the $0.006 / $25 cost, and the
    /// routed-provider order openai, anthropic.
    const BUDGETED: &str = r#"
    {
      "policyId": "openclaw-smoke",
      "budget": {
        "configured": true,
        "ledger": "durable_object",
        "windowKey": "openclaw/openclaw-smoke/2026-07",
        "limitMicros": 25000000,
        "spentMicros": 6000,
        "remainingMicros": 24994000
      },
      "usage": {
        "ledger": "ready",
        "summary": {
          "requestCount": 6,
          "successCount": 5,
          "errorCount": 1,
          "inputTokens": 50000,
          "outputTokens": 4191,
          "totalTokens": 54191,
          "actualCostMicros": 6000
        },
        "providers": [
          {
            "provider": "anthropic",
            "requestCount": 2,
            "successCount": 2,
            "errorCount": 0,
            "totalTokens": 12191,
            "actualCostMicros": 2000
          },
          {
            "provider": "openai",
            "requestCount": 4,
            "successCount": 3,
            "errorCount": 1,
            "totalTokens": 42000,
            "actualCostMicros": 4000
          }
        ],
        "events": []
      }
    }
    "#;

    /// Recorded by CodexBar, same file — "unmetered fixture keeps arbitrary providers
    /// and spend". No budget, so no window: only rows. CodexBar asserts the $1.25
    /// actual cost and the routed-provider order replicate, tavily.
    const UNMETERED: &str = r#"
    {
      "policyId": "any-provider-policy",
      "budget": {
        "configured": false,
        "ledger": "unmetered",
        "windowKey": null,
        "limitMicros": null,
        "spentMicros": null,
        "remainingMicros": null
      },
      "usage": {
        "ledger": "ready",
        "summary": {
          "requestCount": 3,
          "successCount": 3,
          "errorCount": 0,
          "inputTokens": 0,
          "outputTokens": 0,
          "totalTokens": 0,
          "actualCostMicros": 1250000
        },
        "providers": [
          {
            "provider": "tavily",
            "requestCount": 2,
            "successCount": 2,
            "errorCount": 0,
            "totalTokens": 0,
            "actualCostMicros": 250000
          },
          {
            "provider": "replicate",
            "requestCount": 1,
            "successCount": 1,
            "errorCount": 0,
            "totalTokens": 0,
            "actualCostMicros": 1000000
          }
        ],
        "events": []
      }
    }
    "#;

    /// CodexBar's harness polls this fixture at unix 1, which a plausible-range
    /// timestamp refuses; the instant feeds nothing here but `captured_at`, so the
    /// tests use one inside the fixture's own month.
    const FIXTURE_NOW: i64 = 1_785_000_000;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn section<'a>(snapshot: &'a Snapshot, title: &str) -> &'a DetailSection {
        snapshot
            .details
            .iter()
            .find(|section| section.title == title)
            .unwrap_or_else(|| panic!("no section {title} in {:?}", snapshot.details))
    }

    fn rows<'a>(snapshot: &'a Snapshot, title: &str) -> &'a [DetailRow] {
        &section(snapshot, title).rows
    }

    #[test]
    fn the_budgeted_fixture_draws_the_monthly_window() {
        let snapshot = parse(BUDGETED, at(FIXTURE_NOW)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(window.key.as_str(), "w2592000");
        assert_eq!(window.title, "30 days");
        assert_eq!(window.used_percent, 0.024, "6000 of 25000000 micros");
        assert_eq!(
            window.resets_at,
            Some(at(1_785_542_400)),
            "the windowKey's 2026-07 rolls over on 2026-08-01T00:00:00Z"
        );
        assert_eq!(window.length.expect("monthly").as_secs(), 2_592_000);
        assert_eq!(
            window.subtitle.as_deref(),
            Some("$0.006000 / $25.00"),
            "spent at the micros it was recorded in, limit at cents"
        );
    }

    #[test]
    fn the_budgeted_fixture_carries_every_recorded_row() {
        let snapshot = parse(BUDGETED, at(FIXTURE_NOW)).expect("parses");
        let usage = rows(&snapshot, "Usage");
        assert_eq!(usage[0].label, "Requests");
        assert_eq!(usage[0].value, "6 · 5 succeeded · 1 failed");
        assert_eq!(usage[1].label, "Tokens");
        assert_eq!(usage[1].value, "54191 · 50000 input · 4191 output");
        assert_eq!(usage[2].label, "Actual cost");
        assert_eq!(usage[2].value, "$0.006000");
        assert_eq!(usage[3].label, "Budget ledger");
        assert_eq!(usage[3].value, "durable_object");
        assert_eq!(usage[4].label, "Monthly budget");
        assert_eq!(
            usage[4].value, "$0.006000 / $25.00 · $24.994000 remaining",
            "24994000 micros is not a dyadic dollar amount; the strings stay exact \
             because money is formatted from integer micros, never through an f64 dollar"
        );

        let routed = rows(&snapshot, "Routed providers");
        let labels: Vec<&str> = routed.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            ["openai", "anthropic"],
            "sorted by cost, descending"
        );
        assert_eq!(routed[0].value, "4 requests · $0.004000 · 42000 tokens");
        assert_eq!(routed[1].value, "2 requests · $0.002000 · 12191 tokens");

        assert_eq!(rows(&snapshot, "Plan")[0].label, "Budget");
        assert_eq!(rows(&snapshot, "Plan")[0].value, "Managed monthly budget");
    }

    #[test]
    fn the_unmetered_fixture_draws_no_window_and_keeps_the_rows() {
        let snapshot = parse(UNMETERED, at(FIXTURE_NOW)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "no limit on the wire, so there is nothing to divide by"
        );
        let labels: Vec<&str> = rows(&snapshot, "Usage")
            .iter()
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["Requests", "Tokens", "Actual cost", "Budget ledger"],
            "no Monthly budget row without a budget"
        );
        assert_eq!(rows(&snapshot, "Usage")[2].value, "$1.250000");
        let routed: Vec<&str> = rows(&snapshot, "Routed providers")
            .iter()
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(routed, ["replicate", "tavily"]);
        assert_eq!(rows(&snapshot, "Plan")[0].value, "Unmetered");
    }

    #[test]
    fn zero_spend_with_no_budget_costs_nothing_and_draws_nothing() {
        // CodexBar's own zero-spend variant of the unmetered fixture: the same body
        // with actualCostMicros replaced by 0. Nothing is invented to fill the card.
        let body = UNMETERED.replace("1250000", "0");
        let snapshot = parse(&body, at(FIXTURE_NOW)).expect("parses");
        assert!(snapshot.windows.is_empty());
        assert_eq!(rows(&snapshot, "Usage")[2].value, "$0.000000");
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // "not-json" and `{"budget":{}}` are the two malformed bodies CodexBar
        // recorded. The truncated envelope is the procedure's own. A micros figure
        // quoted, and one with a fraction, fail the plugin's integer check.
        let string_where_number = r#"
        { "budget": { "configured": true, "ledger": "durable_object",
                      "limitMicros": 25000000, "spentMicros": "6000" },
          "usage": { "summary": { "requestCount": 6, "successCount": 5, "errorCount": 1,
                                  "inputTokens": 1, "outputTokens": 1, "totalTokens": 2,
                                  "actualCostMicros": 6000 },
                     "providers": [] } }
        "#;
        let fractional = r#"
        { "budget": { "configured": true, "ledger": "durable_object" },
          "usage": { "summary": { "requestCount": 6, "successCount": 5, "errorCount": 1,
                                  "inputTokens": 1, "outputTokens": 1, "totalTokens": 2,
                                  "actualCostMicros": 1.5 },
                     "providers": [] } }
        "#;
        for body in [
            "not-json",
            "{\"budget\":{}}",
            "{\"partial\":",
            string_where_number,
            fractional,
        ] {
            let error =
                parse(body, at(FIXTURE_NOW)).expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_field_of_a_kind_this_parser_does_not_read_is_skipped() {
        // The recorded body already carries unread fields (`events`, `policyId`,
        // `usage.ledger`); one more field invented after this was written is skipped
        // the same way, and the monthly window is unaffected.
        let body = BUDGETED.replacen(
            "\"events\": []",
            "\"events\": [], \"alerts\": {\"budget\": true, \"kind\": \"monthly\"}",
            1,
        );
        let snapshot = parse(&body, at(FIXTURE_NOW)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 0.024);
    }

    #[test]
    fn a_multibyte_window_key_costs_the_pace_mark_not_the_fetch() {
        // Not a recorded body: it tests the panic-safety of the string handling, not
        // provider semantics. The recorded key replaced by a multibyte one whose last
        // seven bytes are not `YYYY-MM`, the window draws with no reset — exactly what
        // ASCII garbage yields — instead of panicking on a char boundary.
        let body = BUDGETED.replace("openclaw/openclaw-smoke/2026-07", "öaaaaaa");
        let snapshot = parse(&body, at(FIXTURE_NOW)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1, "the budget itself still stands");
        assert_eq!(
            snapshot.windows[0].resets_at, None,
            "a key that names no month yields no pace mark, never a panic"
        );
    }

    fn endpoint(pairs: &[(&str, &str)]) -> String {
        let options: Options = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        (SPEC.endpoint)(&options)
    }

    #[test]
    fn the_base_url_option_resolves_the_v1_usage_path() {
        // CodexBar's own request test: a bare host and one already carrying /v1
        // reach the same usage path.
        assert_eq!(
            endpoint(&[]),
            "https://clawrouter.openclaw.ai/v1/usage",
            "unset, the public host"
        );
        assert_eq!(
            endpoint(&[("base_url", "https://router.example.com")]),
            "https://router.example.com/v1/usage"
        );
        assert_eq!(
            endpoint(&[("base_url", "https://router.example.com/v1")]),
            "https://router.example.com/v1/usage",
            "/v1 is not appended twice"
        );
        assert_eq!(
            endpoint(&[("base_url", "https://router.example.com/v1/")]),
            "https://router.example.com/v1/usage",
            "a trailing slash is trimmed first"
        );
    }

    #[test]
    fn a_bad_base_url_falls_back_to_the_default_host() {
        // This provider's friendlier policy: a value the shared reader refuses — plain
        // HTTP to a remote host, or bare words — costs the default host, not a dead
        // card. Loopback HTTP is how a self-hosted router is reached, and stands.
        assert_eq!(
            endpoint(&[("base_url", "http://router.example.com")]),
            "https://clawrouter.openclaw.ai/v1/usage"
        );
        assert_eq!(
            endpoint(&[("base_url", "router.example.com")]),
            "https://clawrouter.openclaw.ai/v1/usage"
        );
        assert_eq!(
            endpoint(&[("base_url", "http://127.0.0.1:8080")]),
            "http://127.0.0.1:8080/v1/usage"
        );
    }

    #[test]
    fn the_spec_polls_with_a_bearer_key_and_publishes_the_base_url() {
        use crate::providers::keyed::{Auth, Method};
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "ClawRouter");
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.options.len(), 1);
        let option = &SPEC.options[0];
        assert_eq!(option.name, "base_url");
        assert!(!option.required, "the public host is the default");
        assert!(option.choices.is_empty(), "free text");
    }
}
