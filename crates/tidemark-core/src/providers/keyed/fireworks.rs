//! Fireworks.
//!
//! Ported from CodexBar's `Providers/Fireworks/FireworksUsageFetcher.swift`; the
//! recorded bodies in `FireworksUsageFetcherTests.swift` are the contract. Never seen
//! answering: every number in the tests below is a body CodexBar recorded.
//!
//! # The account id is part of the path
//!
//! `GET https://api.fireworks.ai/v1/accounts/<account>/billing/summary`, so the
//! account slug is a **required free-text option**, validated against the source's own
//! alphabet — ASCII letters, digits, `.`, `_`, `-` — before it can reach a URL: a slug
//! with any other character is refused as a configuration error, never allowed to
//! widen the path, inject a query, or crash on URL construction.
//!
//! # The window rolls, and is computed per poll
//!
//! The endpoint takes `startTime`/`endTime` as ISO-8601 stamps of *now minus thirty
//! days* and *now*. There is no fixed window to resolve at build time, which is why
//! this provider is hand-written: [`Fireworks::summary_request`] computes both stamps
//! from [`Timestamp::now`] on every fetch, so each poll sums the thirty days ending at
//! that poll.
//!
//! # The money and the card
//!
//! Rated line items arrive as Google-style money — `units` as a **string**, `nanos` as
//! an integer — summed as `units + nanos/1e9`. The first rated row's currency decides
//! the display currency and only rows in it are summed; a row without a readable rated
//! cost (no `totalCost`, a `units` that does not parse, no `nanos`, no currency) is
//! skipped, as the source's guard skips it. Fireworks exposes no credit-balance API,
//! so spend is the only usage signal: **no window, ever** — details only, the card
//! renders empty when nothing is rated, which is accepted.
//!
//! # What ships untested
//!
//! No recorded body carries a first currency other than USD (the mixed fixture's EUR
//! row is the skipped one), so the rendering of any other currency is unpinned. The
//! unrated-row shapes in the skip test are constructed around the recorded fetch
//! body's rated row; the procedure allows it, and no number in a passing assertion
//! comes from them.

use super::{HandSpec, OptionSchema, Options, redact_query, required};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "fireworks";

/// Name of the account-id setting under `[provider.fireworks]`.
pub const ACCOUNT: &str = "account_id";

/// Where the billing API lives, before the account's own path segment.
const BASE_URL: &str = "https://api.fireworks.ai/v1/accounts";

/// How far back the summary window reaches. The source's own lookback.
const LOOKBACK_SECS: i64 = 30 * 86_400;

/// Fireworks as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Fireworks",
    credential: CredentialKind::Key,
    credential_hint: "app.fireworks.ai → Account Settings → API keys. The key alone is not enough: the account id from the accounts URL is a second, required setting.",
    options: &[OptionSchema {
        name: ACCOUNT,
        title: "Account ID",
        description: Some(
            "The account slug from app.fireworks.ai/accounts/<slug> — it is part of the billing URL itself.",
        ),
        default: "",
        choices: &[],
        required: true,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The
/// account id is read and validated here, so a missing or invalid one is named on the
/// card rather than reaching the wire as a malformed URL.
fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Fireworks::new_for_account(
        account, credential, options,
    )?))
}

/// One Fireworks account: the key, and the billing summary it unlocks when the account
/// id says whose.
pub struct Fireworks {
    tidemark_account: AccountId,
    client: reqwest::Client,
    credential: Credential,
    account: String,
}

impl Fireworks {
    /// Builds a client. The account id is part of the path, so it is resolved once,
    /// here, against the source's slug alphabet.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), credential, options)
    }

    fn new_for_account(
        account_id: AccountId,
        credential: Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        let account = required(options, ACCOUNT, "Account ID")?;
        if !account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        {
            return Err(ProviderError::Local(format!(
                "Account ID {account:?} is not valid — it may only contain letters, digits, '.', '_' and '-'"
            )));
        }
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: http::client()?,
            credential,
            account,
        })
    }

    /// The billing-summary request for the thirty days ending at `now`, built but not
    /// sent, so the placement of the key is testable. The window is computed here, per
    /// poll, from the clock — see the module doc.
    fn summary_request(&self, now: Timestamp) -> Result<reqwest::Request, ProviderError> {
        let url = summary_url(&self.account, now.as_unix() - LOOKBACK_SECS, now.as_unix());
        self.client
            .get(url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let body = super::request(PROVIDER_ID, &self.client, self.summary_request(now)?).await?;
        Ok(snapshot_for_account(
            parse_summary(&body)?.as_ref(),
            now,
            &self.tidemark_account,
        ))
    }
}

impl fmt::Debug for Fireworks {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fireworks")
            .field("id", &PROVIDER_ID)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl Provider for Fireworks {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        self.tidemark_account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// The billing-summary URL for one account over an explicit window, pure so the
/// recorded golden — pinned at the epoch and one day later — is reachable from a test.
fn summary_url(account: &str, start_unix: i64, end_unix: i64) -> String {
    format!(
        "{BASE_URL}/{account}/billing/summary?startTime={}&endTime={}",
        iso_stamp(start_unix),
        iso_stamp(end_unix)
    )
}

/// The source's ISO-8601 spelling of a window edge: internet date-time, UTC.
fn iso_stamp(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .expect("a plausible window edge")
        .format(&Rfc3339)
        .expect("RFC-3339 always formats")
}

/// The window's spend: an exact-enough total and the currency it is stated in.
#[derive(Debug, Clone, PartialEq)]
struct Spend {
    total: f64,
    currency: String,
}

/// One billing summary, as the wire states it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryBody {
    #[serde(default)]
    line_items: Vec<LineItem>,
    /// Decoded so a garbage bucket list is malformed, as the source's decoder reads
    /// it; the per-day buckets feed a chart this card does not draw.
    #[serde(default)]
    #[allow(dead_code)]
    usage_buckets: Vec<UsageBucket>,
}

/// One rated line. Every field but `total_cost` is decoded for that same refusal and
/// never read.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LineItem {
    #[allow(dead_code)]
    category: Option<String>,
    #[allow(dead_code)]
    grouping_key: Option<String>,
    #[allow(dead_code)]
    grouping_value: Option<String>,
    #[allow(dead_code)]
    quantity: Option<f64>,
    #[allow(dead_code)]
    series: Option<String>,
    total_cost: Option<Money>,
    #[allow(dead_code)]
    unit_amount: Option<Money>,
}

/// Google-style money: `units` as a string, `nanos` as an integer.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Money {
    currency_code: Option<String>,
    nanos: Option<i64>,
    units: Option<String>,
}

/// One per-day bucket of the chart this card does not draw.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageBucket {
    #[allow(dead_code)]
    bucket_start_time: Option<String>,
    #[allow(dead_code)]
    line_items: Option<Vec<LineItem>>,
}

/// Reads the billing summary. Pure: the first rated row's currency decides, only rows
/// in it are summed, and a row without a readable rated cost is skipped — the source's
/// own guard, kept.
fn parse_summary(body: &str) -> Result<Option<Spend>, ProviderError> {
    let summary: SummaryBody = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a Fireworks summary: {e}")))?;
    let mut currency: Option<String> = None;
    let mut total = 0.0;
    for item in &summary.line_items {
        let Some(cost) = item.total_cost.as_ref() else {
            continue;
        };
        let Some(units) = cost
            .units
            .as_deref()
            .and_then(|units| units.parse::<f64>().ok())
        else {
            continue;
        };
        let Some(nanos) = cost.nanos else {
            continue;
        };
        let Some(code) = cost
            .currency_code
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
        else {
            continue;
        };
        if currency.is_none() {
            currency = Some(code.to_owned());
        }
        if code == currency.as_deref().unwrap_or_default() {
            total += units + nanos as f64 / 1_000_000_000.0;
        }
    }
    Ok(currency.map(|currency| Spend { total, currency }))
}

/// Assembles the snapshot. Pure, so the recorded renderings are reachable from a test.
///
/// No window, ever: Fireworks is prepaid with no quota to draw a bar against, and the
/// shape for that is details only — the card renders empty when nothing is rated,
/// which is accepted.
#[cfg(test)]
fn snapshot(spend: Option<&Spend>, now: Timestamp) -> Snapshot {
    snapshot_for_account(spend, now, &AccountId::default())
}

fn snapshot_for_account(spend: Option<&Spend>, now: Timestamp, account_id: &AccountId) -> Snapshot {
    let details = spend
        .map(|spend| {
            vec![DetailSection {
                title: "Spend".to_owned(),
                rows: vec![DetailRow {
                    label: "Last 30 days".to_owned(),
                    value: money(spend.total, &spend.currency),
                }],
            }]
        })
        .unwrap_or_default();
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at: now,
        windows: Vec::new(),
        details,
    }
}

/// One spend in its own currency. Only the USD of the recorded bodies is pinned; the
/// yen of the sibling providers keeps zero fraction digits here too, every other
/// currency shows two, and an unknown code is spelled out rather than guessed a
/// symbol.
fn money(total: f64, currency: &str) -> String {
    let value = if currency == "JPY" {
        format!("{:.0}", total.max(0.0))
    } else {
        format!("{:.2}", total.max(0.0))
    };
    match currency {
        "USD" => format!("${value}"),
        "JPY" => format!("¥{value}"),
        "EUR" => format!("€{value}"),
        "GBP" => format!("£{value}"),
        code => format!("{value} {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_800_000_000).expect("plausible")
    }

    #[test]
    fn the_recorded_line_items_sum_units_and_nanos() {
        let spend = parse_summary(LINE_ITEMS).expect("parses").expect("rated");
        assert!(
            (spend.total - 1.525548296).abs() <= 0.000_000_001,
            "{}",
            spend.total
        );
        assert_eq!(spend.currency, "USD");
        let snapshot = snapshot(Some(&spend), now());
        assert!(
            snapshot.windows.is_empty(),
            "Fireworks is prepaid with no quota"
        );
        assert_eq!(row_of(&snapshot, "Last 30 days").value, "$1.53");
    }

    #[test]
    fn only_rows_in_the_first_rated_currency_are_summed() {
        let spend = parse_summary(MIXED_CURRENCY)
            .expect("parses")
            .expect("rated");
        assert_eq!(spend.currency, "USD");
        assert!(
            (spend.total - 1.35).abs() <= 0.000_000_001,
            "{}",
            spend.total
        );
    }

    #[test]
    fn a_row_without_a_readable_cost_is_skipped() {
        // The rated row is the recorded fetch body's (nanos 500000000, units "0"); the
        // two unrated rows are constructed shapes for the source's own guard: no
        // totalCost at all, or a units string that does not parse. Each is a row that
        // carries no rated cost, not a failed page.
        let spend = parse_summary(UNRATED_ROWS).expect("parses").expect("rated");
        assert_eq!(spend.currency, "USD");
        assert!(
            (spend.total - 0.5).abs() <= 0.000_000_001,
            "{}",
            spend.total
        );
    }

    #[test]
    fn empty_line_items_report_no_spend() {
        assert!(parse_summary(EMPTY).expect("parses").is_none());
        let snapshot = snapshot(None, now());
        assert!(snapshot.windows.is_empty());
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn unsummary_like_bodies_are_malformed() {
        // `[{ "lineItems": [] }]` is the recorded invalid root; a string where the
        // integer nanos belong fails the Swift decoder the same way; `{"partial":` is
        // the procedure's canonical body.
        for body in [
            r#"[{ "lineItems": [] }]"#,
            "{\"partial\":",
            r#"{"lineItems":[{"totalCost":{"currencyCode":"USD","nanos":"492256016","units":"0"}}]}"#,
            r#"{"lineItems":[{"totalCost":{"currencyCode":"USD","nanos":492256016,"units":0}}]}"#,
        ] {
            let error = parse_summary(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn fields_these_parsers_do_not_know_are_skipped() {
        // The unknown-kind rule: the recorded body's many unread fields — category,
        // grouping, quantity, series, unitAmount, usageBuckets — ride along, and a
        // field this parser has never met does too.
        let future = LINE_ITEMS.replace(
            "\"usageBuckets\": []",
            "\"usageBuckets\": [], \"future\": {\"whatever\": \"it says\"}",
        );
        let spend = parse_summary(&future).expect("parses").expect("rated");
        assert!(
            (spend.total - 1.525548296).abs() <= 0.000_000_001,
            "{}",
            spend.total
        );
    }

    #[test]
    fn malformed_account_slugs_fail_with_a_config_error_instead_of_misrouting() {
        // The recorded bad slugs: reserved or invalid URL characters must surface as a
        // config error, never widen the path, inject a query, or crash.
        for bad in [
            "sp ace",
            "has/slash",
            "has?query",
            "has#fragment",
            "percent%2F",
            "col\u{00e9}on",
        ] {
            let error = Fireworks::new(Credential::new("fw-key"), &options(bad))
                .expect_err("an invalid slug is refused");
            assert!(matches!(error, ProviderError::Local(_)), "{bad}: {error:?}");
        }
        // The recorded good slugs still produce the exact billing-summary path.
        for good in ["x0mh0x", "acct-1_x.d"] {
            let fireworks =
                Fireworks::new(Credential::new("fw-key"), &options(good)).expect("builds");
            let request = fireworks.summary_request(now()).expect("builds");
            assert!(
                request.url().as_str().starts_with(&format!(
                    "https://api.fireworks.ai/v1/accounts/{good}/billing/summary?"
                )),
                "{}",
                request.url()
            );
        }
    }

    #[test]
    fn the_summary_url_carries_the_account_slug_and_the_iso_window() {
        // The recorded golden, pinned at the epoch and one day later.
        let url = summary_url("x0mh0x", 0, 86_400);
        assert_eq!(
            url,
            "https://api.fireworks.ai/v1/accounts/x0mh0x/billing/summary?startTime=1970-01-01T00:00:00Z&endTime=1970-01-02T00:00:00Z"
        );
    }

    #[test]
    fn the_summary_request_is_a_bearer_get_over_a_rolling_thirty_day_window() {
        // The window is computed per poll from the clock — the reason this provider is
        // hand-written — so the test pins shape, not a frozen pair of timestamps.
        let fireworks =
            Fireworks::new(Credential::new("fw-test-key"), &options("x0mh0x")).expect("builds");
        let request = fireworks.summary_request(now()).expect("builds");
        assert_eq!(request.method(), reqwest::Method::GET);
        let url = request.url().as_str();
        assert!(
            url.starts_with("https://api.fireworks.ai/v1/accounts/x0mh0x/billing/summary?"),
            "{url}"
        );
        assert!(url.contains("startTime="), "{url}");
        assert!(url.contains("endTime="), "{url}");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer fw-test-key"
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ACCEPT)
                .expect("present"),
            "application/json"
        );
    }

    #[test]
    fn the_account_id_option_is_required_and_named_when_missing() {
        let unset = Fireworks::new(Credential::new("fw-key"), &Options::new())
            .expect_err("the required option is unset");
        assert!(format!("{unset}").contains("Account ID"), "{unset}");
    }

    #[test]
    fn the_spec_publishes_the_account_id_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Fireworks");
        assert_eq!(SPEC.options.len(), 1);
        assert!(SPEC.options[0].required);
        assert!(
            build(
                AccountId::default(),
                Credential::new("fw-key"),
                &options("x0mh0x")
            )
            .is_ok(),
            "a slug and a key build a client"
        );
        assert!(
            build(
                AccountId::default(),
                Credential::new("fw-key"),
                &Options::new()
            )
            .is_err()
        );
    }

    #[test]
    fn a_fireworks_client_never_prints_its_credential() {
        let fireworks =
            Fireworks::new(Credential::new("fw-super-secret"), &options("x0mh0x")).expect("builds");
        let rendered = format!("{fireworks:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }

    fn row_of<'a>(snapshot: &'a Snapshot, label: &str) -> &'a DetailRow {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no {label} row in {snapshot:?}"))
    }

    fn options(account: &str) -> Options {
        [(ACCOUNT.to_owned(), account.to_owned())]
            .into_iter()
            .collect()
    }

    // Recorded bodies, verbatim from FireworksUsageFetcherTests.swift.
    const LINE_ITEMS: &str = r#"{
          "lineItems": [
            {
              "category": "LLM input tokens (cached)",
              "groupingKey": "model_bucket",
              "groupingValue": "DeepSeek V4 Flash",
              "quantity": 17580572,
              "series": "SERVERLESS",
              "totalCost": { "currencyCode": "USD", "nanos": 492256016, "units": "0" },
              "unitAmount": { "currencyCode": "USD", "nanos": 28, "units": "0" }
            },
            {
              "category": "LLM output tokens",
              "groupingKey": "model_bucket",
              "groupingValue": "DeepSeek V4 Flash",
              "quantity": 118901,
              "series": "SERVERLESS",
              "totalCost": { "currencyCode": "USD", "nanos": 33292280, "units": "1" },
              "unitAmount": { "currencyCode": "USD", "nanos": 280, "units": "0" }
            }
          ],
          "usageBuckets": []
        }"#;
    const MIXED_CURRENCY: &str = r#"{
          "lineItems": [
            {
              "category": "LLM input tokens (cached)",
              "totalCost": { "currencyCode": "USD", "nanos": 100000000, "units": "1" }
            },
            {
              "category": "LLM output tokens",
              "totalCost": { "currencyCode": "EUR", "nanos": 900000000, "units": "9" }
            },
            {
              "category": "LLM input tokens (uncached)",
              "totalCost": { "currencyCode": "USD", "nanos": 250000000, "units": "0" }
            }
          ],
          "usageBuckets": []
        }"#;
    const UNRATED_ROWS: &str = r#"{
              "lineItems": [
                {
                  "category": "LLM input tokens (cached)",
                  "totalCost": { "currencyCode": "USD", "nanos": 500000000, "units": "0" }
                },
                {
                  "category": "LLM output tokens"
                },
                {
                  "category": "LLM input tokens (uncached)",
                  "totalCost": { "currencyCode": "USD", "nanos": 500000000, "units": "later" }
                }
              ],
              "usageBuckets": []
            }"#;
    const EMPTY: &str = r#"{ "lineItems": [], "usageBuckets": [] }"#;
}
