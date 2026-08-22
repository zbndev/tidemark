//! DeepInfra.
//!
//! Ported from CodexBar's `Providers/DeepInfra/DeepInfraUsageFetcher.swift`; the
//! recorded bodies in `DeepInfraUsageFetcherTests.swift` are the contract. Never seen
//! answering: every number in the tests below is a body CodexBar recorded.
//!
//! # The two requests
//!
//! `GET https://api.deepinfra.com/payment/checklist?compute_owed=true` first, then
//! `GET https://api.deepinfra.com/payment/usage?from=current`, both
//! bearer-authenticated, in that order. The card needs both: the checklist supplies
//! the Stripe balance, the recent spend, the optional spending limit and the
//! suspension state — the whole bar — while the usage endpoint supplies this month's
//! cost as **cents** (`total_cost`), the only place a limit-less account's spend is
//! not the checklist's "recent" figure.
//!
//! # The reading
//!
//! `recent` is floored at zero and added to the Stripe balance: a negative balance is
//! prepaid credit the recent spend has not yet reached (available = −net), a positive
//! one is money owed (owed = net). The card draws two windows. The **prepaid balance**
//! is CodexBar's own bar: 0% while credit remains, 100% once the account is suspended,
//! owes money, or has nothing left, with the source's exact one-line summary as the
//! subtitle. The **spending limit**, when a positive one is set, is recent spend
//! against that limit — a fixed balance, keyed `limit` because a spending limit has no
//! length to key on. Both are resetless: DeepInfra states no reset time. An empty
//! month list falls back to the recent cost, as the source's `?? recentCost` does.
//!
//! # What ships untested
//!
//! No recorded body carries a suspend reason that is blank (the fallback spelling
//! "Suspended · …" is ported, not pinned), more than one month, or a non-null
//! `limit` the tests do not build. The `{"partial":` bodies and the string-where-a-
//! number-belongs case are constructed, as the porting procedure allows.

use super::{HandSpec, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "deepinfra";

/// Where the payment checklist lives. One host, no regional or self-hosted variant, so
/// there is no setting to resolve.
const CHECKLIST_URL: &str = "https://api.deepinfra.com/payment/checklist?compute_owed=true";

/// Where the current month's usage lives.
const USAGE_URL: &str = "https://api.deepinfra.com/payment/usage?from=current";

/// The usage endpoint reports `total_cost` in cents; the checklist's fields are USD.
const CENTS_PER_DOLLAR: f64 = 100.0;

/// DeepInfra as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "DeepInfra",
    credential: CredentialKind::Key,
    credential_hint: "deepinfra.com → Your profile → API tokens.",
    options: &[],
    build,
};

/// Builds a pollable client from the stored key. DeepInfra has nothing to configure.
fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(DeepInfra::new(credential)?))
}

/// One DeepInfra account: the key, and the two payment endpoints it unlocks.
pub struct DeepInfra {
    client: reqwest::Client,
    credential: Credential,
}

impl DeepInfra {
    /// Builds a client. One host, so the URLs are constants and there is nothing to
    /// resolve at build time.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    /// The checklist request, built but not sent, so the placement of the key is
    /// testable.
    fn checklist_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(CHECKLIST_URL)
    }

    /// The usage request, likewise.
    fn usage_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(USAGE_URL)
    }

    fn get(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
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
        let checklist_body = super::request(&self.client, self.checklist_request()?).await?;
        let usage_body = super::request(&self.client, self.usage_request()?).await?;
        combine(
            &parse_checklist(&checklist_body)?,
            &parse_usage(&usage_body)?,
            now,
        )
    }
}

impl fmt::Debug for DeepInfra {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeepInfra")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for DeepInfra {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// The payment checklist: the balance, the recent spend, the optional limit and the
/// suspension state.
#[derive(Debug, Clone, Deserialize)]
struct Checklist {
    stripe_balance: f64,
    recent: f64,
    limit: Option<f64>,
    #[serde(default)]
    suspended: bool,
    suspend_reason: Option<String>,
}

/// The current month's usage. `months` arrives oldest-first; the last is current.
#[derive(Debug, Clone, Deserialize)]
struct Usage {
    months: Vec<Month>,
    /// Decoded so a garbage value is malformed, as the source's decoder reads it;
    /// never read.
    #[allow(dead_code)]
    initial_month: Option<String>,
}

/// One month's usage.
#[derive(Debug, Clone, Deserialize)]
struct Month {
    /// Decoded for the refusal, never read.
    #[allow(dead_code)]
    period: String,
    /// In cents, unlike every checklist field.
    total_cost: f64,
}

/// Reads the checklist. Pure, and strict: the balance and the recent spend must be
/// present and numeric — the recorded `{}` refusal.
fn parse_checklist(body: &str) -> Result<Checklist, ProviderError> {
    serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a DeepInfra checklist: {e}")))
}

/// Reads the usage. Pure, likewise strict about its month list.
fn parse_usage(body: &str) -> Result<Usage, ProviderError> {
    serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a DeepInfra usage report: {e}")))
}

/// Combines both endpoints' bodies into the card. Pure, so every recorded rendering is
/// reachable from a test.
fn combine(
    checklist: &Checklist,
    usage: &Usage,
    now: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let recent_cost = checklist.recent.max(0.0);
    let current_month_cost = usage
        .months
        .last()
        .map(|month| (month.total_cost / CENTS_PER_DOLLAR).max(0.0))
        .unwrap_or(recent_cost);
    let net = checklist.stripe_balance + recent_cost;
    let available = (-net).max(0.0);
    let owed = net.max(0.0);
    let limit = checklist.limit.filter(|limit| *limit > 0.0);

    // CodexBar's own one-line summary, rebuilt verbatim: an optional suspension
    // prefix, then the net position, then this month's spend.
    let balance_text = if owed > 0.0 {
        format!("{} owed", usd(owed))
    } else {
        format!("{} available", usd(available))
    };
    let spending_text = format!("{} spent this month", usd(current_month_cost));
    let suspended_prefix = if checklist.suspended {
        match checklist
            .suspend_reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
        {
            Some(reason) => format!("Suspended: {reason} · "),
            None => "Suspended · ".to_owned(),
        }
    } else {
        String::new()
    };
    let summary = format!("{suspended_prefix}{balance_text} · {spending_text}");

    let mut windows = vec![Window {
        // A prepaid balance has no length to key on and never resets; the key names
        // what the bar measures.
        key: WindowKey::named("balance"),
        title: "Prepaid balance".to_owned(),
        subtitle: Some(summary),
        used_percent: if checklist.suspended || owed > 0.0 || available <= 0.0 {
            100.0
        } else {
            0.0
        },
        resets_at: None,
        length: None,
    }];
    if let Some(limit) = limit {
        windows.push(Window {
            // Recent spend against a stated spending limit — a second balance the
            // account reports alongside the first, keyed apart for the same reason.
            key: WindowKey::named("limit"),
            title: "Spending limit".to_owned(),
            subtitle: Some(format!(
                "{} of {} used · {} left",
                usd(recent_cost),
                usd(limit),
                usd((limit - recent_cost).max(0.0))
            )),
            used_percent: (recent_cost / limit * 100.0).clamp(0.0, 100.0),
            resets_at: None,
            length: None,
        });
    }

    let mut rows = Vec::new();
    if owed > 0.0 {
        rows.push(labeled("Owed", usd(owed)));
    } else {
        rows.push(labeled("Available", usd(available)));
    }
    rows.push(labeled("Spent this month", usd(current_month_cost)));
    if let Some(limit) = limit {
        rows.push(labeled("Spending limit", usd(limit)));
    }
    if checklist.suspended {
        let status = checklist
            .suspend_reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(|reason| format!("Suspended: {reason}"))
            .unwrap_or_else(|| "Suspended".to_owned());
        rows.push(labeled("Status", status));
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows,
        details: vec![DetailSection {
            title: "Billing".to_owned(),
            rows,
        }],
    })
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

/// The source's own rendering: dollars, two fraction digits, sign kept — an owed
/// amount is a real number here, not clamped away.
fn usd(value: f64) -> String {
    format!("${value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_700_000_000).expect("plausible")
    }

    // The recorded bodies are parameterised in DeepInfraUsageFetcherTests.swift by the
    // two builders below; they are mirrored here with the same parameters the recorded
    // assertions use.

    fn checklist(
        stripe_balance: f64,
        recent: f64,
        limit: Option<f64>,
        suspended: bool,
        suspend_reason: Option<&str>,
    ) -> String {
        let limit = limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_owned());
        let reason = suspend_reason
            .map(|reason| format!("\"{reason}\""))
            .unwrap_or_else(|| "null".to_owned());
        format!(
            r#"{{
              "stripe_balance": {stripe_balance},
              "recent": {recent},
              "limit": {limit},
              "suspended": {suspended},
              "suspend_reason": {reason}
            }}"#
        )
    }

    fn usage(total_cost_cents: f64) -> String {
        format!(
            r#"{{
              "months": [
                {{
                  "period": "2026.07",
                  "items": [],
                  "total_cost": {total_cost_cents}
                }}
              ],
              "initial_month": "2026.07"
            }}"#
        )
    }

    fn parse(checklist_body: &str, usage_body: &str) -> Result<Snapshot, ProviderError> {
        combine(
            &parse_checklist(checklist_body)?,
            &parse_usage(usage_body)?,
            now(),
        )
    }

    fn row_of<'a>(snapshot: &'a Snapshot, label: &str) -> &'a DetailRow {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no {label} row in {snapshot:?}"))
    }

    fn window_of<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|window| window.key.as_str() == key)
            .unwrap_or_else(|| panic!("no {key} window in {snapshot:?}"))
    }

    #[test]
    fn converts_monthly_cents_and_deducts_recent_usage_from_prepaid_balance() {
        let snapshot = parse(
            &checklist(-99.75, 3.94, Some(20.0), false, None),
            &usage(394.0),
        )
        .expect("parses");

        let balance = window_of(&snapshot, "balance");
        assert_eq!(balance.title, "Prepaid balance");
        assert_eq!(balance.used_percent, 0.0);
        assert_eq!(balance.length, None);
        assert_eq!(balance.resets_at, None);
        assert_eq!(
            balance.subtitle.as_deref(),
            Some("$95.81 available · $3.94 spent this month")
        );

        let limit = window_of(&snapshot, "limit");
        assert_eq!(limit.title, "Spending limit");
        assert!(
            (limit.used_percent - 3.94 / 20.0 * 100.0).abs() < 1e-9,
            "{}",
            limit.used_percent
        );
        assert_eq!(
            limit.subtitle.as_deref(),
            Some("$3.94 of $20.00 used · $16.06 left")
        );

        assert_eq!(row_of(&snapshot, "Available").value, "$95.81");
        assert_eq!(row_of(&snapshot, "Spent this month").value, "$3.94");
        assert_eq!(row_of(&snapshot, "Spending limit").value, "$20.00");
    }

    #[test]
    fn positive_stripe_balance_is_reported_as_amount_owed() {
        let snapshot = parse(
            &checklist(2.75, 7.0, Some(-1.0), false, None),
            &usage(650.0),
        )
        .expect("parses");
        assert_eq!(snapshot.windows.len(), 1, "a limit of -1 is no limit");
        let balance = window_of(&snapshot, "balance");
        assert_eq!(balance.used_percent, 100.0);
        assert_eq!(
            balance.subtitle.as_deref(),
            Some("$9.75 owed · $6.50 spent this month")
        );
        assert_eq!(row_of(&snapshot, "Owed").value, "$9.75");
        assert!(
            snapshot
                .details
                .iter()
                .flat_map(|section| section.rows.iter())
                .all(|row| row.label != "Spending limit"),
            "a limit of -1 publishes no limit row"
        );
    }

    #[test]
    fn suspended_account_is_marked_exhausted() {
        let snapshot = parse(
            &checklist(-5.0, 1.0, None, true, Some("Payment review")),
            &usage(100.0),
        )
        .expect("parses");
        let balance = window_of(&snapshot, "balance");
        assert_eq!(balance.used_percent, 100.0);
        assert!(
            balance
                .subtitle
                .as_deref()
                .unwrap_or_default()
                .starts_with("Suspended: Payment review"),
            "{:?}",
            balance.subtitle
        );
        assert_eq!(
            row_of(&snapshot, "Status").value,
            "Suspended: Payment review"
        );
    }

    #[test]
    fn the_recorded_fetch_pair_deducts_recent_usage_from_a_stripe_credit() {
        // The bodies the recorded transport test answers with: stripe -9, recent 2,
        // limit 10, usage 150 cents — whose available balance the test pins at 7.
        let snapshot = parse(
            &checklist(-9.0, 2.0, Some(10.0), false, None),
            &usage(150.0),
        )
        .expect("parses");
        assert_eq!(row_of(&snapshot, "Available").value, "$7.00");
        assert_eq!(row_of(&snapshot, "Spent this month").value, "$1.50");
    }

    #[test]
    fn a_checklist_or_usage_that_cannot_be_read_is_malformed() {
        // `{}` as the checklist is the recorded rejection; the rest are the
        // procedure's canonical bodies and a string where a number belongs.
        for (checklist_body, usage_body) in [
            ("{}", usage(100.0).as_str()),
            (
                checklist(-99.75, 3.94, Some(20.0), false, None).as_str(),
                "{}",
            ),
            ("{\"partial\":", usage(100.0).as_str()),
            (
                checklist(-99.75, 3.94, Some(20.0), false, None).as_str(),
                "{\"partial\":",
            ),
            (
                r#"{"stripe_balance":"many","recent":3.94}"#,
                usage(100.0).as_str(),
            ),
        ] {
            let error = parse(checklist_body, usage_body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{checklist_body} + {usage_body}: {error:?}"
            );
        }
    }

    #[test]
    fn fields_these_parsers_do_not_know_are_skipped() {
        // The unknown-kind rule: the recorded month already carries an `items` array
        // this parser never reads, and a field it has never met rides along too. A
        // month list with no months at all falls back to the checklist's recent cost,
        // as the source's `?? recentCost` does.
        let future = usage(394.0).replace(
            "\"initial_month\": \"2026.07\"",
            "\"initial_month\": \"2026.07\", \"future\": true",
        );
        let snapshot =
            parse(&checklist(-99.75, 3.94, Some(20.0), false, None), &future).expect("parses");
        assert_eq!(row_of(&snapshot, "Spent this month").value, "$3.94");

        let no_months = "{\"months\": [], \"initial_month\": null}";
        let snapshot =
            parse(&checklist(-99.75, 3.94, Some(20.0), false, None), no_months).expect("parses");
        assert_eq!(
            row_of(&snapshot, "Spent this month").value,
            "$3.94",
            "an empty month list falls back to the recent cost"
        );
    }

    #[test]
    fn both_requests_carry_the_recorded_paths_queries_and_bearer_key() {
        let deepinfra = DeepInfra::new(Credential::new("fixture-token")).expect("builds");
        let checklist_request = deepinfra.checklist_request().expect("builds");
        assert_eq!(checklist_request.method(), reqwest::Method::GET);
        assert_eq!(
            checklist_request.url().as_str(),
            "https://api.deepinfra.com/payment/checklist?compute_owed=true"
        );
        let usage_request = deepinfra.usage_request().expect("builds");
        assert_eq!(
            usage_request.url().as_str(),
            "https://api.deepinfra.com/payment/usage?from=current"
        );
        for request in [checklist_request, usage_request] {
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer fixture-token"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json"
            );
        }
    }

    #[test]
    fn the_spec_publishes_nothing_to_choose_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "DeepInfra");
        assert!(SPEC.options.is_empty(), "DeepInfra has nothing to choose");
        assert!(build(Credential::new("fixture-token"), &Options::new()).is_ok());
    }

    #[test]
    fn a_deepinfra_client_never_prints_its_credential() {
        let deepinfra = DeepInfra::new(Credential::new("sk-super-secret")).expect("builds");
        let rendered = format!("{deepinfra:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
