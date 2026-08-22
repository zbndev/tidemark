//! ai&.
//!
//! Ported from CodexBar's `Providers/AiAnd/AiAndUsageFetcher.swift`; the recorded
//! bodies in `AiAndProviderTests.swift` — sanitized from a live `/logs` response on
//! 2026-07-17 — are the contract. Never seen answering since: every number in the tests
//! below is one of those bodies.
//!
//! # The one request, paged
//!
//! `GET https://api.aiand.com/logs?range=30days&limit=100`, bearer-authenticated, then
//! up to [`MAX_PAGES`] more pages following the `after`/`after_id` cursor pair while
//! `has_more` holds. The two cursors are one step: the docs warn `after` alone is
//! unsafe, so a page that claims more rows without naming **both** stops the walk and
//! marks the sum partial, exactly as the source does — and so does hitting the page
//! cap. The partial flag is never silent here: it names the row
//! "Last 30 days (partial)", or says "Partial right now" when nothing priced landed.
//!
//! ai& exposes no balance or quota API, so the sum is the only usage signal: **no
//! window, ever** — details only, the card renders empty, which is accepted.
//!
//! # The money is exact
//!
//! Costs arrive as decimal strings and CodexBar sums them with `Decimal`, asserting
//! 0.1 + 0.1 + 0.1 is exactly 0.3. This port sums the same strings as exact integers
//! at a billionth ([`decimal_nanos`]), which carries that exactness. Rows are
//! newest-first, the first priced row's currency decides the display currency, and
//! only rows in that currency are summed. A row without a cost (the recorded failed
//! request), or without a currency, is skipped; a cost that is neither null nor a
//! decimal string fails the fetch — a fraction longer than nine decimal places
//! included, which the i128-nanos scale cannot hold. That is the one place this port
//! is stricter than the source, whose guard silently skips such a row and
//! under-reports the spend.
//!
//! # Statuses
//!
//! The source has its own words for 401 (rejected key), 402 (out of credits) and 429;
//! this port sends every request through the shared keyed transport, which maps
//! 401/402/403 to a rejected credential and 429 to rate-limited. The words differ, the
//! actions are the same.

use super::{HandSpec, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "aiand";

/// Where the request log lives. One host, no regional or self-hosted variant, so there
/// is no setting to resolve.
const LOGS_URL: &str = "https://api.aiand.com/logs";

/// Rows per page. The source's own limit.
const PAGE_LIMIT: usize = 100;

/// Pages fetched at most, per poll: log rows are per-request, so ten pages covers the
/// newest thousand requests of the window. The source's own cap.
const MAX_PAGES: usize = 10;

/// ai& as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "ai&",
    credential_hint: "console.aiand.com → API keys.",
    options: &[],
    build,
};

/// Builds a pollable client from the stored key. ai& has nothing to configure.
fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(AiAnd::new(credential)?))
}

/// One ai& account: the key, and the paged request log it unlocks.
pub struct AiAnd {
    client: reqwest::Client,
    credential: Credential,
}

impl AiAnd {
    /// Builds a client. One host, so the URL is a constant and there is nothing to
    /// resolve at build time.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    /// One log page, built but not sent, so the query and the placement of the key are
    /// testable. The cursors are percent-encoded by hand because a cursor timestamp
    /// carries a `+00` offset: the form serializer a generic query builder uses would
    /// leave a bare `+`, which a server reads as a space.
    fn logs_request(
        &self,
        after: Option<&str>,
        after_id: Option<&str>,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut url = format!("{LOGS_URL}?range=30days&limit={PAGE_LIMIT}");
        if let Some(after) = after {
            url.push_str(&format!("&after={}", encode_query(after)));
        }
        if let Some(after_id) = after_id {
            url.push_str(&format!("&after_id={}", encode_query(after_id)));
        }
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
        let (rows, complete) = self.pages().await?;
        let spend = summarise(&rows)?;
        Ok(snapshot(spend.as_ref(), complete, now))
    }

    /// Walks the pages, keeping every row that lands. Transport, status and shape
    /// failures fail the fetch; only a lost cursor pair or the page cap ends the walk
    /// early, marked partial.
    async fn pages(&self) -> Result<(Vec<LogRow>, bool), ProviderError> {
        let mut rows = Vec::new();
        let mut after: Option<String> = None;
        let mut after_id: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let body = super::request(
                &self.client,
                self.logs_request(after.as_deref(), after_id.as_deref())?,
            )
            .await?;
            let page = parse_logs_page(&body)?;
            let next = step(&page);
            rows.extend(page.rows);
            match next {
                Step::Done => return Ok((rows, true)),
                Step::Advance(next_after, next_after_id) => {
                    after = Some(next_after);
                    after_id = Some(next_after_id);
                }
                // The server reports more rows but did not return both cursors; the
                // docs warn `after` alone is unsafe, so stop and mark the sum partial.
                Step::Stop => return Ok((rows, false)),
            }
        }
        Ok((rows, false))
    }
}

impl fmt::Debug for AiAnd {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiAnd")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for AiAnd {
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

/// One parsed log page.
#[derive(Debug, Clone, Deserialize)]
struct LogsPage {
    #[serde(rename = "data")]
    rows: Vec<LogRow>,
    has_more: Option<bool>,
    next_after: Option<String>,
    next_after_id: Option<String>,
}

/// One log row, reduced to the two fields the sum reads. Every other field the
/// recorded bodies carry — `id`, `model`, `api_key`, `status_code`, the token counts —
/// rides along unread.
#[derive(Debug, Clone, Deserialize)]
struct LogRow {
    cost: Option<String>,
    currency: Option<String>,
}

/// What one page says about the next. Pure, so the recorded cursor bodies are reachable
/// from a test.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// No more rows: the walk covered the whole window.
    Done,
    /// Follow this cursor pair.
    Advance(String, String),
    /// More rows exist but the page named no usable cursor pair; stop, partial.
    Stop,
}

/// Reads one log page. Pure, and strict at the edges the source is strict at: the page
/// must carry its `data` array (the recorded `{"object":"list"}` refusal), and a cost
/// or currency that is present but not a string is malformed.
fn parse_logs_page(body: &str) -> Result<LogsPage, ProviderError> {
    serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an ai& logs page: {e}")))
}

/// The pagination step: no `has_more` reads as false, and both cursors must be named
/// for the walk to continue.
fn step(page: &LogsPage) -> Step {
    if !page.has_more.unwrap_or(false) {
        return Step::Done;
    }
    match (page.next_after.clone(), page.next_after_id.clone()) {
        (Some(after), Some(after_id)) => Step::Advance(after, after_id),
        _ => Step::Stop,
    }
}

/// The window's spend: an exact total and the currency it is stated in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spend {
    /// Total cost in billionths of the currency's unit — the exact-decimal stand-in
    /// for the source's `Decimal`.
    nanos: i128,
    /// Uppercased ISO code, as the source uppercases it (`jpy` → `JPY`).
    currency: String,
}

/// Sums the priced rows: newest-first, the first priced row's currency decides, only
/// rows in that currency count, and rows without a cost or a currency are skipped.
/// Pure, and exact.
fn summarise(rows: &[LogRow]) -> Result<Option<Spend>, ProviderError> {
    let mut currency: Option<String> = None;
    let mut total: i128 = 0;
    for row in rows {
        let Some(raw) = row
            .cost
            .as_deref()
            .map(str::trim)
            .filter(|cost| !cost.is_empty())
        else {
            continue;
        };
        let Some(code) = row
            .currency
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(str::to_lowercase)
        else {
            continue;
        };
        let cost = decimal_nanos(raw)
            .ok_or_else(|| ProviderError::malformed("an ai& cost is not a decimal amount"))?;
        if currency.is_none() {
            currency = Some(code.clone());
        }
        if code == currency.as_deref().unwrap_or_default() {
            total += cost;
        }
    }
    Ok(currency.map(|currency| Spend {
        nanos: total,
        currency: currency.to_uppercase(),
    }))
}

/// One decimal money string as an exact integer of billionths: an optional sign,
/// digits, an optional fraction. `None` for anything else — including a fraction
/// longer than nine places, which this scale cannot hold exactly.
fn decimal_nanos(raw: &str) -> Option<i128> {
    let unsigned = raw
        .strip_prefix('-')
        .or_else(|| raw.strip_prefix('+'))
        .unwrap_or(raw);
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let fraction = fraction.unwrap_or("");
    if fraction.len() > 9 {
        return None;
    }
    let mut nanos: i128 = whole.parse().ok()?;
    nanos *= 10_i128.pow(9);
    if !fraction.is_empty() {
        let padded = format!("{fraction:0<9}");
        nanos += padded.parse::<i128>().ok()?;
    }
    (raw.starts_with('-')).then_some(-nanos).or(Some(nanos))
}

/// Assembles the snapshot. Pure, so every recorded rendering is reachable from a test.
///
/// No window, ever: ai& is prepaid with no quota to draw a bar against, and the shape
/// for that is details only — the card renders empty when nothing priced landed, which
/// is accepted.
fn snapshot(spend: Option<&Spend>, complete: bool, now: Timestamp) -> Snapshot {
    let rows = match spend {
        Some(spend) => vec![DetailRow {
            label: if complete {
                "Last 30 days".to_owned()
            } else {
                "Last 30 days (partial)".to_owned()
            },
            value: money(spend),
        }],
        // Where the source shows nothing at all over an incomplete window, this says
        // what happened: silence would read as a confirmed zero.
        None if !complete => vec![DetailRow {
            label: "Last 30 days".to_owned(),
            value: "Partial right now".to_owned(),
        }],
        None => Vec::new(),
    };
    let details = if rows.is_empty() {
        Vec::new()
    } else {
        vec![DetailSection {
            title: "Spend".to_owned(),
            rows,
        }]
    };
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows: Vec::new(),
        details,
    }
}

/// One spend in its own currency: a symbol for the four this card knows, the code
/// spelled out otherwise, and zero fraction digits for the yen the recorded body
/// carries — every other currency shows two.
fn money(spend: &Spend) -> String {
    let digits = if spend.currency == "JPY" { 0 } else { 2 };
    let divisor = 10_u128.pow(9 - digits);
    let sign = if spend.nanos < 0 { "-" } else { "" };
    let nanos = spend.nanos.unsigned_abs();
    let minor = (nanos + divisor / 2) / divisor;
    let unit = 10_u128.pow(digits);
    let whole = minor / unit;
    let fraction = minor % unit;
    let amount = match digits {
        0 => format!("{whole}"),
        _ => format!("{whole}.{fraction:02}"),
    };
    match spend.currency.as_str() {
        "USD" => format!("{sign}${amount}"),
        "JPY" => format!("{sign}¥{amount}"),
        "EUR" => format!("{sign}€{amount}"),
        "GBP" => format!("{sign}£{amount}"),
        code => format!("{sign}{amount} {code}"),
    }
}

/// Percent-encodes one query value the way the source's explicit `+` escaping does:
/// everything outside the URL-safe set is escaped, a `+` included, so a cursor
/// timestamp's `+00` offset survives the round trip. The colon of a timestamp stays
/// bare, as the recorded golden URL keeps it.
fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_800_000_000).expect("plausible")
    }

    // Recorded bodies, verbatim from AiAndProviderTests.swift: FINAL_PAGE and
    // FIRST_PAGE are sanitized from a live `/logs` response (2026-07-17).

    #[test]
    fn a_quoted_total_is_read_at_nanos_scale() {
        // "7.02344000" + "1.10000000", from the recorded final page; the nanos scale
        // is this port's exact-decimal stand-in for CodexBar's `Decimal`.
        assert_eq!(decimal_nanos("7.02344000").expect("parses"), 7_023_440_000);
        assert_eq!(decimal_nanos("1.10000000").expect("parses"), 1_100_000_000);
        assert_eq!(decimal_nanos("0.10000000").expect("parses"), 100_000_000);
    }

    #[test]
    fn the_recorded_final_page_sums_priced_rows_and_skips_the_null_cost_row() {
        let page = parse_logs_page(FINAL_PAGE).expect("parses");
        assert_eq!(page.rows.len(), 3);
        let spend = summarise(&page.rows).expect("sums").expect("priced");
        assert_eq!(spend.currency, "JPY");
        assert_eq!(spend.nanos, 8_123_440_000);
    }

    #[test]
    fn both_recorded_pages_sum_across_the_cursor() {
        let first = parse_logs_page(FIRST_PAGE).expect("parses");
        let final_page = parse_logs_page(FINAL_PAGE).expect("parses");
        let mut rows = first.rows.clone();
        rows.extend(final_page.rows);
        let spend = summarise(&rows).expect("sums").expect("priced");
        assert_eq!(spend.currency, "JPY");
        assert_eq!(spend.nanos, 20_623_440_000);
    }

    #[test]
    fn the_recorded_pages_step_done_advance_and_stop() {
        // The pure pagination step: a page with no more rows is done, one with both
        // cursors advances, one that claims more rows but names no cursor stops and
        // marks the sum partial.
        assert!(matches!(
            step(&parse_logs_page(FINAL_PAGE).expect("parses")),
            Step::Done
        ));
        assert!(matches!(
            step(&parse_logs_page(MISSING_CURSOR_PAGE).expect("parses")),
            Step::Stop
        ));
        match step(&parse_logs_page(FIRST_PAGE).expect("parses")) {
            Step::Advance(after, after_id) => {
                assert_eq!(after, "2026-07-17 10:24:30.094374+00");
                assert_eq!(after_id, "912bf992-0000-4000-8000-000000000002");
            }
            other => panic!("the recorded first page advances, got {other:?}"),
        }
    }

    #[test]
    fn ten_recorded_first_pages_sum_to_the_recorded_cap_total() {
        // The recorded cap test: the transport answers the first page ten times
        // (maxPages), the sum is 125.0, and it is partial.
        let first = parse_logs_page(FIRST_PAGE).expect("parses");
        let rows: Vec<LogRow> = (0..MAX_PAGES)
            .flat_map(|_| first.rows.iter().cloned())
            .collect();
        assert_eq!(rows.len(), 20);
        let spend = summarise(&rows).expect("sums").expect("priced");
        assert_eq!(spend.nanos, 125_000_000_000);
        let snapshot = snapshot(Some(&spend), false, now());
        assert!(snapshot.windows.is_empty(), "ai& is prepaid with no quota");
        assert_eq!(
            row_of(&snapshot, "Last 30 days (partial)").value,
            "¥125",
            "the partial flag must be visible in the details, not silent"
        );
    }

    #[test]
    fn the_recorded_final_page_renders_a_complete_spend_row() {
        let page = parse_logs_page(FINAL_PAGE).expect("parses");
        let spend = summarise(&page.rows).expect("sums").expect("priced");
        let snapshot = snapshot(Some(&spend), true, now());
        assert!(snapshot.windows.is_empty());
        assert_eq!(row_of(&snapshot, "Last 30 days").value, "¥8");
        assert!(
            snapshot
                .details
                .iter()
                .flat_map(|section| section.rows.iter())
                .all(|row| !row.label.contains("partial"))
        );
    }

    #[test]
    fn mixed_currencies_keep_the_newest_rows_currency_and_skip_the_rest() {
        let page = parse_logs_page(MIXED_CURRENCY).expect("parses");
        let spend = summarise(&page.rows).expect("sums").expect("priced");
        assert_eq!(spend.currency, "JPY");
        assert_eq!(spend.nanos, 9_500_000_000);
    }

    #[test]
    fn rows_without_a_currency_are_skipped_and_alone_yield_no_spend() {
        let page = parse_logs_page(MISSING_CURRENCY).expect("parses");
        assert_eq!(summarise(&page.rows).expect("sums"), None);
    }

    #[test]
    fn the_recorded_empty_window_reports_no_spend_at_all() {
        let page = parse_logs_page(EMPTY_PAGE).expect("parses");
        assert_eq!(summarise(&page.rows).expect("sums"), None);
        let snapshot = snapshot(None, true, now());
        assert!(snapshot.windows.is_empty());
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn an_incomplete_window_with_no_priced_rows_still_says_it_is_partial() {
        // No recorded body combines the two, but the cap and the missing cursor can
        // both strand an empty or currency-less window; CodexBar then shows no cost at
        // all, and this port says why instead.
        let snapshot = snapshot(None, false, now());
        assert_eq!(row_of(&snapshot, "Last 30 days").value, "Partial right now");
    }

    #[test]
    fn decimal_money_strings_sum_exactly() {
        // The recorded exactness test: 0.1 + 0.1 + 0.1 must be exactly 0.3.
        let page = parse_logs_page(DECIMAL).expect("parses");
        let spend = summarise(&page.rows).expect("sums").expect("priced");
        assert_eq!(spend.nanos, 300_000_000);
    }

    #[test]
    fn unloggable_bodies_are_malformed() {
        // `{"object":"list"}` is the recorded malformed payload; the others are the
        // porting procedure's canonical bodies for this field. A cost that is neither
        // null nor a decimal string fails the fetch rather than skipping — the one
        // place this port is stricter than CodexBar, whose guard silently skips such a
        // row and under-reports the spend.
        for body in [
            r#"{"object":"list"}"#,
            "{\"partial\":",
            r#"{"data":[{"cost":7,"currency":"jpy"}],"has_more":false}"#,
            r#"{"data":[{"cost":"many","currency":"jpy"}],"has_more":false}"#,
        ] {
            match parse_logs_page(body) {
                Ok(page) => {
                    let error = summarise(&page.rows).expect_err("must refuse");
                    assert!(
                        matches!(error, ProviderError::Malformed(_)),
                        "{body}: {error:?}"
                    );
                }
                Err(error) => {
                    assert!(
                        matches!(error, ProviderError::Malformed(_)),
                        "{body}: {error:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_logs_requests_match_the_recorded_golden_urls() {
        let aiand = AiAnd::new(Credential::new("fixture-key")).expect("builds");
        let first = aiand.logs_request(None, None).expect("builds");
        assert_eq!(first.method(), reqwest::Method::GET);
        assert_eq!(
            first.url().as_str(),
            "https://api.aiand.com/logs?range=30days&limit=100"
        );
        let second = aiand
            .logs_request(
                Some("2026-07-17 10:24:30.094374+00"),
                Some("912bf992-0000-4000-8000-000000000002"),
            )
            .expect("builds");
        assert_eq!(
            second.url().as_str(),
            "https://api.aiand.com/logs?range=30days&limit=100&after=2026-07-17%2010:24:30.094374%2B00&after_id=912bf992-0000-4000-8000-000000000002"
        );
        for request in [first, second] {
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer fixture-key"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json"
            );
            assert!(
                !request.url().as_str().contains("fixture-key"),
                "the key is only ever a header"
            );
        }
    }

    #[test]
    fn the_page_cap_is_ten_and_the_page_limit_is_a_hundred() {
        assert_eq!(MAX_PAGES, 10);
        assert_eq!(PAGE_LIMIT, 100);
    }

    #[test]
    fn the_spec_publishes_nothing_to_choose_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "ai&");
        assert!(SPEC.options.is_empty(), "ai& has nothing to choose");
        assert!(build(Credential::new("fixture-key"), &Options::new()).is_ok());
    }

    #[test]
    fn an_aiand_client_never_prints_its_credential() {
        let aiand = AiAnd::new(Credential::new("sk-super-secret")).expect("builds");
        let rendered = format!("{aiand:?}");
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

    // Recorded bodies, verbatim from AiAndProviderTests.swift; sanitized from a live
    // `/logs` response (2026-07-17).
    const FINAL_PAGE: &str = r#"
    {
      "data": [
        {
          "id": "cdd2b25d-0000-4000-8000-000000000001",
          "model": "zai-org/glm-5.2",
          "api_key": "masked",
          "status_code": 200,
          "ttft_ms": 1449,
          "latency_ms": 3163,
          "input_tokens": 170569,
          "output_tokens": 248,
          "cached_tokens": 170240,
          "cost": "7.02344000",
          "currency": "jpy",
          "created_at": "2026-07-17 10:24:30.094374+00"
        },
        {
          "id": "cdd2b25d-0000-4000-8000-000000000002",
          "model": "zai-org/glm-5.2",
          "api_key": "masked",
          "status_code": 200,
          "ttft_ms": 512,
          "latency_ms": 1201,
          "input_tokens": 1200,
          "output_tokens": 90,
          "cached_tokens": 0,
          "cost": "1.10000000",
          "currency": "jpy",
          "created_at": "2026-07-17 10:20:00.000000+00"
        },
        {
          "id": "cdd2b25d-0000-4000-8000-000000000003",
          "model": "zai-org/glm-5.2",
          "api_key": "masked",
          "status_code": 500,
          "ttft_ms": 0,
          "latency_ms": 42,
          "input_tokens": 0,
          "output_tokens": 0,
          "cached_tokens": null,
          "cost": null,
          "currency": "jpy",
          "created_at": "2026-07-17 10:15:00.000000+00"
        }
      ],
      "has_more": false,
      "next_after": null,
      "next_after_id": null
    }
    "#;
    const FIRST_PAGE: &str = r#"
    {
      "data": [
        {
          "id": "912bf992-0000-4000-8000-000000000001",
          "model": "zai-org/glm-5.2",
          "api_key": "masked",
          "status_code": 200,
          "ttft_ms": 800,
          "latency_ms": 2400,
          "input_tokens": 52000,
          "output_tokens": 700,
          "cached_tokens": 0,
          "cost": "12.00000000",
          "currency": "jpy",
          "created_at": "2026-07-17 10:24:30.094374+00"
        },
        {
          "id": "912bf992-0000-4000-8000-000000000002",
          "model": "zai-org/glm-5.2",
          "api_key": "masked",
          "status_code": 200,
          "ttft_ms": 300,
          "latency_ms": 900,
          "input_tokens": 2100,
          "output_tokens": 55,
          "cached_tokens": 0,
          "cost": "0.50000000",
          "currency": "jpy",
          "created_at": "2026-07-17 10:24:30.094374+00"
        }
      ],
      "has_more": true,
      "next_after": "2026-07-17 10:24:30.094374+00",
      "next_after_id": "912bf992-0000-4000-8000-000000000002"
    }
    "#;
    const MIXED_CURRENCY: &str = r#"
    {
      "data": [
        {
          "id": "aaaa0000-0000-4000-8000-000000000001",
          "cost": "9.50000000",
          "currency": "jpy",
          "created_at": "2026-07-17 10:24:30.094374+00"
        },
        {
          "id": "aaaa0000-0000-4000-8000-000000000002",
          "cost": "1.25000000",
          "currency": "usd",
          "created_at": "2026-07-17 10:20:00.000000+00"
        }
      ],
      "has_more": false,
      "next_after": null,
      "next_after_id": null
    }
    "#;
    const MISSING_CURRENCY: &str = r#"
    {
      "data": [
        {
          "id": "aaaa0000-0000-4000-8000-000000000003",
          "cost": "4.20000000",
          "currency": null,
          "created_at": "2026-07-17 10:24:30.094374+00"
        },
        {
          "id": "aaaa0000-0000-4000-8000-000000000004",
          "cost": "1.00000000",
          "currency": "  ",
          "created_at": "2026-07-17 10:20:00.000000+00"
        }
      ],
      "has_more": false,
      "next_after": null,
      "next_after_id": null
    }
    "#;
    const DECIMAL: &str = r#"
    {
      "data": [
        {"id": "bbbb0000-0000-4000-8000-000000000001", "cost": "0.10000000", "currency": "jpy"},
        {"id": "bbbb0000-0000-4000-8000-000000000002", "cost": "0.10000000", "currency": "jpy"},
        {"id": "bbbb0000-0000-4000-8000-000000000003", "cost": "0.10000000", "currency": "jpy"}
      ],
      "has_more": false,
      "next_after": null,
      "next_after_id": null
    }
    "#;
    const MISSING_CURSOR_PAGE: &str = r#"
            {
              "data": [{"cost": "2.50000000", "currency": "jpy"}],
              "has_more": true,
              "next_after": null,
              "next_after_id": null
            }
            "#;
    const EMPTY_PAGE: &str = r#"{"data": [], "has_more": false}"#;
}
