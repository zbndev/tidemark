//! OpenAI, read through the Admin API.
//!
//! Ported from CodexBar's `Plugins/openai.js` and
//! `Providers/OpenAI/OpenAIAPIUsageFetcher.swift`; the recorded bodies in
//! `OpenAIAPIUsageFetcherTests.swift` and `OpenAIAPICreditBalanceTests.swift` are the
//! contract. Never seen answering: every number in the tests below is a body CodexBar
//! recorded.
//!
//! # The slug
//!
//! `openai-api`, not `openai`: CodexBar's own CLI alias for this key-based provider,
//! which leaves the bare vendor name free for the dashboard provider this port will
//! never carry (it needs cookies and a browser).
//!
//! # The card comes from two paginated endpoints
//!
//! `GET /v1/organization/costs?group_by=line_item` and
//! `GET /v1/organization/usage/completions?group_by=model`, each over a window of
//! [`HISTORY_DAYS`] days split into 31-day ranges (`limit` is capped at 31 days of
//! buckets), each range paged with a `page` cursor while `has_more` holds. The paging
//! rules are the fetcher's own: a page that says it has more must name the next cursor
//! (`next_page`, trimmed — an empty one is missing, which is malformed), a cursor that
//! repeats is a loop, which is malformed, and no range may exceed 100 pages. A project
//! filter (`project_ids`) rides every request when the option is set. Costs arrive as
//! `amount.value`, bare or quoted — the recorded bodies carry both spellings — and a
//! non-finite string where an amount belongs is the recorded malformed case.
//!
//! The Admin usage endpoints report spend with **no limit**, so this path draws no
//! window at all: details only, the card renders empty, which is accepted.
//!
//! # The legacy balance fallback
//!
//! `GET /v1/dashboard/billing/credit_grants` is a fallback CodexBar keeps "for unscoped
//! keys and Admin API outages": `total_granted`/`total_used`/`total_available`, plus the
//! next grant expiry. Alone it reads grant-less post-paid orgs as 100% exhausted, which
//! is why the plugin gates it behind `OPENAI_ALLOW_BALANCE_FALLBACK` — a setting it
//! exposes in its own settings list, so the gate is carried here as the
//! `balance_fallback` option, default off, matching the plugin's "only the exact value
//! `1` enables it" reading. CodexBar's app derives that flag from whether the key is an
//! admin key and whether a project is scoped; Tidemark sees one pasted key and cannot
//! tell, so the user decides. When the fallback itself fails too, the source's error
//! precedence is kept: a credential rejection on the usage path surfaces the balance
//! error, anything else surfaces the usage error.
//!
//! # What is fixed
//!
//! History is 30 days, the plugin's own default. CodexBar feeds `OPENAI_HISTORY_DAYS`
//! from a global app preference that has no counterpart here, so the default stands.
//! The per-day spend chart CodexBar draws is dropped: a Tidemark card has no chart.
//!
//! # What ships untested
//!
//! No recorded body exercises the 100-page cap, a quoted token count (the plugin accepts
//! numeric strings where the Swift decoder would not; the plugin is the contract and
//! that acceptance is ported), or a credit body with no grants object. The error-path
//! `{"partial":` bodies are constructed, as the porting procedure allows; no number in a
//! passing assertion is invented.

use super::{HandSpec, OptionSchema, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
};
use time::OffsetDateTime;

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "openai-api";

/// Name of the project-filter setting under `[provider.openai-api]`.
pub const PROJECT: &str = "project_id";

/// Name of the legacy-balance-fallback setting under `[provider.openai-api]`.
pub const BALANCE_FALLBACK: &str = "balance_fallback";

/// Where the organization's costs live. One host, no regional variant.
const COSTS_URL: &str = "https://api.openai.com/v1/organization/costs";

/// Where the organization's completions usage lives.
const COMPLETIONS_URL: &str = "https://api.openai.com/v1/organization/usage/completions";

/// The legacy billing endpoint the fallback reads.
const CREDIT_GRANTS_URL: &str = "https://api.openai.com/v1/dashboard/billing/credit_grants";

/// How far back the card looks. The plugin's own default; see the module doc.
const HISTORY_DAYS: i64 = 30;

/// Days of buckets one range may ask for. The source's own cap.
const BUCKET_LIMIT: i64 = 31;

/// Pages one range may fetch, per endpoint. The source's own cap.
const MAX_PAGES: usize = 100;

/// OpenAI as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "OpenAI",
    credential_hint: "platform.openai.com → Settings → API keys. An organization Admin key reads organization usage; project and service-account keys do not.",
    options: &[
        OptionSchema {
            name: PROJECT,
            title: "Project ID",
            description: Some(
                "Limits both endpoints to one project's usage, as the Admin API's project filter does. Leave unset for the whole organization.",
            ),
            default: "",
            choices: &[],
            required: false,
        },
        OptionSchema {
            name: BALANCE_FALLBACK,
            title: "Balance fallback",
            description: Some(
                "When the Admin usage endpoints fail, read the legacy credit-grants balance instead. A post-paid organization without grants reads as fully exhausted there.",
            ),
            default: "0",
            choices: &[("0", "Off"), ("1", "On")],
            required: false,
        },
    ],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The project
/// filter is resolved here so a changed one takes effect on the next build.
fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(OpenAiApi::new(credential, options)?))
}

/// One OpenAI account: the Admin key, the project it is scoped to, and the three
/// endpoints the card can come from.
pub struct OpenAiApi {
    client: reqwest::Client,
    credential: Credential,
    /// Trimmed and quote-stripped, exactly as CodexBar's `cleaned` reads it; `None` for
    /// the whole organization.
    project: Option<String>,
    /// Whether a failed usage fetch falls back to the legacy balance endpoint.
    fallback: bool,
}

impl OpenAiApi {
    /// Builds a client. The project and the fallback gate are settings, resolved once
    /// here.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
            project: cleaned(options.get(PROJECT)),
            // The plugin enables the fallback only for the exact value "1"; anything
            // else — unset, "0", a typo — is off, so an unrecognised value falls back
            // to the default rather than refusing to poll.
            fallback: options.get(BALANCE_FALLBACK).map(String::as_str) == Some("1"),
        })
    }

    /// One Admin-API page request, built but not sent, so the query and the placement of
    /// the key are testable without a server.
    fn page_request(
        &self,
        url: &str,
        group_by: &str,
        range: DayRange,
        page: Option<&str>,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut query: Vec<(&str, String)> = vec![
            ("start_time", range.start.to_string()),
            ("end_time", range.end.to_string()),
            ("bucket_width", "1d".to_owned()),
            ("limit", range.limit.to_string()),
            ("group_by", group_by.to_owned()),
        ];
        if let Some(project) = &self.project {
            query.push(("project_ids", project.clone()));
        }
        if let Some(page) = page {
            query.push(("page", page.to_owned()));
        }
        self.client
            .get(url)
            .query(&query)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The legacy balance request, likewise.
    fn credit_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(CREDIT_GRANTS_URL)
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
        match self.usage(now).await {
            Ok(snapshot) => Ok(snapshot),
            Err(usage_error) => {
                if !self.fallback {
                    return Err(usage_error);
                }
                match self.balance(now).await {
                    Ok(snapshot) => Ok(snapshot),
                    Err(balance_error) => {
                        // The source surfaces the balance error only when the usage
                        // path was a credential rejection; otherwise the usage error
                        // is the truer one.
                        if matches!(usage_error, ProviderError::Credential { .. }) {
                            Err(balance_error)
                        } else {
                            Err(usage_error)
                        }
                    }
                }
            }
        }
    }

    /// The legacy balance path, whose failure is a balance error for the precedence
    /// above to weigh — never a silent return to the usage error.
    async fn balance(&self, now: Timestamp) -> Result<Snapshot, ProviderError> {
        let body = super::request(&self.client, self.credit_request()?).await?;
        let grants = parse_credit_grants(&body, now)?;
        Ok(balance_snapshot(&grants, now))
    }

    /// Walks both endpoints over the whole history window.
    async fn usage(&self, now: Timestamp) -> Result<Snapshot, ProviderError> {
        let ranges = daily_ranges(now, HISTORY_DAYS);
        let costs = self
            .pages(COSTS_URL, "line_item", &ranges, parse_costs_page)
            .await?;
        let completions = self
            .pages(COMPLETIONS_URL, "model", &ranges, parse_completions_page)
            .await?;
        snapshot(&costs, &completions, now)
    }

    /// Every page of one endpoint: each 31-day range in turn, each following its cursor
    /// until it runs out, repeating or losing one being malformed, and no range allowed
    /// past [`MAX_PAGES`] pages.
    async fn pages<T, P>(
        &self,
        url: &str,
        group_by: &str,
        ranges: &[DayRange],
        parse: P,
    ) -> Result<Vec<T>, ProviderError>
    where
        P: Fn(&str) -> Result<Page<T>, ProviderError>,
    {
        let mut buckets = Vec::new();
        for range in ranges {
            let mut page: Option<String> = None;
            let mut seen: Vec<String> = Vec::new();
            for _ in 0..MAX_PAGES {
                let body = super::request(
                    &self.client,
                    self.page_request(url, group_by, *range, page.as_deref())?,
                )
                .await?;
                let parsed = parse(&body)?;
                buckets.extend(parsed.data);
                page = match next_cursor(parsed.has_more, parsed.next_page.as_deref(), &mut seen)? {
                    Some(cursor) => Some(cursor),
                    None => break,
                };
            }
            if page.is_some() {
                return Err(ProviderError::malformed(format!(
                    "the OpenAI pagination exceeded {MAX_PAGES} pages"
                )));
            }
        }
        Ok(buckets)
    }
}

impl fmt::Debug for OpenAiApi {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiApi")
            .field("id", &PROVIDER_ID)
            .field("project", &self.project)
            .field("fallback", &self.fallback)
            .finish_non_exhaustive()
    }
}

impl Provider for OpenAiApi {
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

/// One 31-day slice of the history window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DayRange {
    /// Inclusive start, epoch seconds.
    start: i64,
    /// Exclusive end, epoch seconds.
    end: i64,
    /// Days of buckets asked for; capped at [`BUCKET_LIMIT`].
    limit: i64,
}

/// One page of an Admin API response, either endpoint.
#[derive(Debug, Clone, Deserialize)]
struct Page<T> {
    data: Vec<T>,
    has_more: bool,
    next_page: Option<String>,
}

/// One day of organization costs.
#[derive(Debug, Clone, Deserialize)]
struct CostBucket {
    start_time: i64,
    /// Decoded only so a bucket without it is malformed, as the source's decoder
    /// reads it; the card never needs the end of a day it already has the start of.
    #[allow(dead_code)]
    end_time: i64,
    results: Vec<CostResult>,
}

/// One cost line: an amount and the line it belongs to.
#[derive(Debug, Clone, Deserialize)]
struct CostResult {
    amount: Option<Amount>,
    line_item: Option<String>,
}

/// The money of a cost line. `value` is read flexibly — bare or quoted — because the
/// recorded bodies carry both spellings; `currency` is not used.
#[derive(Debug, Clone, Deserialize)]
struct Amount {
    value: Value,
}

/// One day of completions usage.
#[derive(Debug, Clone, Deserialize)]
struct UsageBucket {
    start_time: i64,
    /// As the costs bucket's: decoded for the refusal, never read.
    #[allow(dead_code)]
    end_time: i64,
    results: Vec<UsageResult>,
}

/// One per-model usage line. Every count is optional and flexible, as the plugin reads
/// them.
#[derive(Debug, Clone, Deserialize)]
struct UsageResult {
    input_tokens: Option<Value>,
    input_cached_tokens: Option<Value>,
    input_audio_tokens: Option<Value>,
    output_tokens: Option<Value>,
    output_audio_tokens: Option<Value>,
    num_model_requests: Option<Value>,
    model: Option<String>,
}

/// The legacy credit-grants balance.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CreditGrants {
    total_granted: f64,
    total_used: f64,
    total_available: f64,
    /// The soonest grant expiry still in the future, when the body names one.
    next_expiry: Option<Timestamp>,
}

/// One day, reduced to what the card shows.
#[derive(Debug, Clone, PartialEq)]
struct DaySummary {
    /// `YYYY-MM-DD`, the UTC day `start_time` falls in.
    day: String,
    cost_usd: f64,
    requests: i64,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    /// `(name, usd)` pairs, heaviest first, the name breaking ties.
    line_items: Vec<(String, f64)>,
    /// `(name, tokens, requests)` triples, most tokens first, the name breaking ties.
    models: Vec<(String, i64, i64)>,
}

/// Reads one costs page. Pure, and whole: the page must carry its `data` array and its
/// `has_more` flag, and every amount must be readable — the recorded cases refuse a page
/// without `data` and refuse `"NaN"`-shaped amounts.
fn parse_costs_page(body: &str) -> Result<Page<CostBucket>, ProviderError> {
    let page: Page<CostBucket> = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an OpenAI costs page: {e}")))?;
    // Read every amount now, so an unreadable one fails the fetch instead of hiding
    // until the accumulator meets it.
    for bucket in &page.data {
        for result in &bucket.results {
            amount_of(result.amount.as_ref().map(|amount| &amount.value))?;
        }
    }
    Ok(page)
}

/// Reads one completions page. Pure, and whole for the same reason: a token count that
/// cannot be read is malformed, not zero.
fn parse_completions_page(body: &str) -> Result<Page<UsageBucket>, ProviderError> {
    let page: Page<UsageBucket> = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an OpenAI completions page: {e}")))?;
    for bucket in &page.data {
        for result in &bucket.results {
            for value in [
                &result.input_tokens,
                &result.input_cached_tokens,
                &result.input_audio_tokens,
                &result.output_tokens,
                &result.output_audio_tokens,
                &result.num_model_requests,
            ]
            .into_iter()
            .flatten()
            {
                count_of(value)?;
            }
        }
    }
    Ok(page)
}

/// The pagination step, pure so the recorded cursor bodies are reachable from a test:
/// a page with no more rows ends the range; a page with more must name a non-empty
/// cursor, one never served before.
fn next_cursor(
    has_more: bool,
    next_page: Option<&str>,
    seen: &mut Vec<String>,
) -> Result<Option<String>, ProviderError> {
    if !has_more {
        return Ok(None);
    }
    let cursor = next_page
        .map(str::trim)
        .filter(|cursor| !cursor.is_empty())
        .ok_or_else(|| {
            ProviderError::malformed("the OpenAI pagination cursor is missing from the page")
        })?;
    if seen.iter().any(|served| served == cursor) {
        return Err(ProviderError::malformed(
            "the OpenAI pagination cursor repeated, which is a loop",
        ));
    }
    seen.push(cursor.to_owned());
    Ok(Some(cursor.to_owned()))
}

/// Reads the legacy credit-grants body. Pure. The three totals must be present and
/// readable (bare or quoted, as the plugin's `finite` accepts); the grants list is
/// optional, and only expiries still in the future count.
fn parse_credit_grants(body: &str, now: Timestamp) -> Result<CreditGrants, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an OpenAI credit balance: {e}")))?;
    let Value::Object(root) = root else {
        return Err(ProviderError::malformed(
            "the OpenAI credit balance must be a JSON object",
        ));
    };
    let total = |name: &str| -> Result<f64, ProviderError> {
        let value = root
            .get(name)
            .ok_or_else(|| ProviderError::malformed(format!("OpenAI {name} must be numeric")))?;
        flexible_number(value, name)
    };
    let grants = root
        .get("grants")
        .and_then(|grants| grants.get("data"))
        .and_then(Value::as_array);
    let next_expiry = grants
        .and_then(|rows| {
            rows.iter()
                .filter_map(|row| row.get("expires_at"))
                .filter_map(|value| flexible_number(value, "expires_at").ok())
                .filter(|at| (*at as i64) > now.as_unix())
                .fold(None::<f64>, |soonest, at| match soonest {
                    Some(earlier) => Some(earlier.min(at)),
                    None => Some(at),
                })
        })
        .and_then(|at| Timestamp::from_unix(at as i64).ok());
    Ok(CreditGrants {
        total_granted: total("total_granted")?,
        total_used: total("total_used")?,
        total_available: total("total_available")?,
        next_expiry,
    })
}

/// One flexible amount, as both the plugin and the Swift decoder read it: absent or null
/// is no amount, a number or a numeric string is the amount, an empty string is no
/// amount, and anything else — including a non-finite string, the recorded case — fails
/// the field it names.
fn amount_of(value: Option<&Value>) -> Result<Option<f64>, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match flexible_number(value, "cost amount") {
            Ok(amount) => Ok(Some(amount)),
            // An empty string reads as no amount, as the source's optional check does.
            Err(error) => match value {
                Value::String(raw) if raw.trim().is_empty() => Ok(None),
                _ => Err(error),
            },
        },
    }
}

/// One flexible count, as the plugin's `integer` reads it: absent, null or empty is
/// zero, a number or numeric string must be finite and whole, and anything else fails
/// the field.
fn count_of(value: &Value) -> Result<i64, ProviderError> {
    let number = match value {
        Value::Null => return Ok(0),
        other => flexible_number(other, "token count")?,
    };
    if number.fract() != 0.0 {
        return Err(ProviderError::malformed(
            "OpenAI token count must be an integer",
        ));
    }
    Ok(number as i64)
}

/// The number both spellings share: a JSON number, or a string holding one. Non-finite
/// results are unreadable — the recorded `"1e309"` and `"NaN"` cases.
fn flexible_number(value: &Value, field: &str) -> Result<f64, ProviderError> {
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed
        .filter(|number| number.is_finite())
        .ok_or_else(|| ProviderError::malformed(format!("OpenAI {field} must be numeric")))
}

/// The 31-day slices of a `days`-day window ending tomorrow UTC, as the source's
/// `dailyRanges` computes them: the first range starts `days - 1` days before today's
/// UTC midnight, and no range asks for more than [`BUCKET_LIMIT`] days of buckets.
fn daily_ranges(now: Timestamp, days: i64) -> Vec<DayRange> {
    let days = days.clamp(1, 365);
    let today = utc_date(now)
        .replace_time(time::Time::MIDNIGHT)
        .unix_timestamp();
    let mut cursor = today - (days - 1) * 86_400;
    let mut remaining = days;
    let mut ranges = Vec::new();
    while remaining > 0 {
        let chunk = BUCKET_LIMIT.min(remaining);
        ranges.push(DayRange {
            start: cursor,
            end: cursor + chunk * 86_400,
            limit: chunk,
        });
        cursor += chunk * 86_400;
        remaining -= chunk;
    }
    ranges
}

/// Combines both endpoints' buckets into the card. Pure, so every recorded rendering is
/// reachable from a test.
///
/// No window, ever: the Admin usage endpoints report spend with no limit to draw a bar
/// against, and the shape for that is details only — the card renders empty, which is
/// accepted.
fn snapshot(
    costs: &[CostBucket],
    completions: &[UsageBucket],
    now: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let days = summarise(costs, completions, now)?;
    let totals = days.iter().fold(
        (0.0_f64, 0_i64, 0_i64, 0_i64, 0_i64),
        |(cost, requests, input, cached, output), day| {
            (
                cost + day.cost_usd,
                requests + day.requests,
                input + day.input_tokens,
                cached + day.cached_input_tokens,
                output + day.output_tokens,
            )
        },
    );
    let models = ranked_models(&days);
    let lines = ranked_lines(&days);

    let summary = vec![
        labeled(
            "Spend",
            format!("{} · Last {HISTORY_DAYS} days", usd(totals.0)),
        ),
        labeled("Requests", number_text(totals.1)),
        labeled(
            "Tokens",
            format!(
                "{} · {} input · {} output",
                number_text(totals.2 + totals.4),
                number_text(totals.2),
                number_text(totals.4)
            ),
        ),
        labeled("Cached input", number_text(totals.3)),
    ];
    let mut details = vec![DetailSection {
        title: "Usage summary".to_owned(),
        rows: summary,
    }];
    if !models.is_empty() {
        details.push(DetailSection {
            title: "Models".to_owned(),
            rows: models
                .iter()
                .take(24)
                .map(|(name, tokens, requests)| {
                    labeled(
                        name,
                        format!("{} tokens · {} requests", number_text(*tokens), requests),
                    )
                })
                .collect(),
        });
    }
    if !lines.is_empty() {
        details.push(DetailSection {
            title: "Line items".to_owned(),
            rows: lines
                .iter()
                .take(24)
                .map(|(name, cost)| labeled(name, usd(*cost)))
                .collect(),
        });
    }
    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows: Vec::new(),
        details,
    })
}

/// Folds both endpoints' buckets into one summary per day, oldest first, keeping only
/// days that have started. Costs land first, completions second, exactly as the source
/// accumulates them onto the same keyed day.
fn summarise(
    costs: &[CostBucket],
    completions: &[UsageBucket],
    now: Timestamp,
) -> Result<Vec<DaySummary>, ProviderError> {
    #[derive(Default)]
    struct Day {
        cost_usd: f64,
        requests: i64,
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
        total_tokens: i64,
        lines: BTreeMap<String, f64>,
        models: BTreeMap<String, (i64, i64)>,
    }
    let mut days: BTreeMap<i64, Day> = BTreeMap::new();

    for bucket in costs {
        let day = days.entry(bucket.start_time).or_default();
        for result in &bucket.results {
            let amount =
                amount_of(result.amount.as_ref().map(|amount| &amount.value))?.unwrap_or(0.0);
            day.cost_usd += amount;
            let line = display_name(result.line_item.as_deref(), "API");
            *day.lines.entry(line).or_default() += amount;
        }
    }
    for bucket in completions {
        let day = days.entry(bucket.start_time).or_default();
        for result in &bucket.results {
            let input = count_of(result.input_tokens.as_ref().unwrap_or(&Value::Null))?;
            let cached = count_of(result.input_cached_tokens.as_ref().unwrap_or(&Value::Null))?;
            let audio_input = count_of(result.input_audio_tokens.as_ref().unwrap_or(&Value::Null))?;
            let output = count_of(result.output_tokens.as_ref().unwrap_or(&Value::Null))?;
            let audio_output =
                count_of(result.output_audio_tokens.as_ref().unwrap_or(&Value::Null))?;
            let requests = count_of(result.num_model_requests.as_ref().unwrap_or(&Value::Null))?;
            let total = input + audio_input + output + audio_output;
            day.requests += requests;
            day.input_tokens += input + audio_input;
            day.cached_input_tokens += cached;
            day.output_tokens += output + audio_output;
            day.total_tokens += total;
            let model = display_name(result.model.as_deref(), "Responses and Chat Completions");
            let entry = day.models.entry(model).or_default();
            entry.0 += total;
            entry.1 += requests;
        }
    }

    days.into_iter()
        .filter(|(start, _)| *start <= now.as_unix())
        .map(|(start, day)| {
            Ok(DaySummary {
                day: day_key(start)?,
                cost_usd: day.cost_usd,
                requests: day.requests,
                input_tokens: day.input_tokens,
                cached_input_tokens: day.cached_input_tokens,
                output_tokens: day.output_tokens,
                total_tokens: day.total_tokens,
                line_items: ranked(&day.lines, |a, b| b.total_cmp(a)),
                models: day
                    .models
                    .into_iter()
                    .map(|(name, (tokens, requests))| (name, tokens, requests))
                    .collect(),
            })
        })
        .collect()
}

/// The credit-grants fallback as a card: one fixed balance — spend against the grant —
/// keyed `balance` because a grant budget has no length to key on, resetless or reset at
/// the next grant's expiry.
fn balance_snapshot(grants: &CreditGrants, now: Timestamp) -> Snapshot {
    // The plugin's own degenerate cases: no grant to measure against means 0% when
    // something is still available and 100% when nothing is.
    let used_percent = if grants.total_granted > 0.0 {
        (grants.total_used / grants.total_granted * 100.0).clamp(0.0, 100.0)
    } else if grants.total_available > 0.0 {
        0.0
    } else {
        100.0
    };
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows: vec![Window {
            key: WindowKey::named("balance"),
            title: "API credits".to_owned(),
            subtitle: Some(format!(
                "{} of {} used · {} left",
                usd(grants.total_used),
                usd(grants.total_granted),
                usd(grants.total_available)
            )),
            used_percent,
            resets_at: grants.next_expiry,
            length: None,
        }],
        details: vec![DetailSection {
            title: "API credits".to_owned(),
            rows: vec![
                labeled("Available", usd(grants.total_available)),
                labeled("Used", usd(grants.total_used)),
                labeled("Granted", usd(grants.total_granted)),
            ],
        }],
    }
}

/// All days' models, ranked by tokens, the name breaking ties.
fn ranked_models(days: &[DaySummary]) -> Vec<(String, i64, i64)> {
    let mut totals: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for day in days {
        for (name, tokens, requests) in &day.models {
            let entry = totals.entry(name.clone()).or_default();
            entry.0 += tokens;
            entry.1 += requests;
        }
    }
    let mut ranked: Vec<_> = totals
        .into_iter()
        .map(|(name, (t, r))| (name, t, r))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// All days' line items, ranked by cost, the name breaking ties.
fn ranked_lines(days: &[DaySummary]) -> Vec<(String, f64)> {
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    for day in days {
        for (name, cost) in &day.line_items {
            *totals.entry(name.clone()).or_default() += cost;
        }
    }
    ranked(&totals, |a, b| b.total_cmp(a))
}

/// A map's pairs under a value ranking, the name breaking ties.
fn ranked<V>(
    totals: &BTreeMap<String, V>,
    by_value: impl Fn(&V, &V) -> std::cmp::Ordering,
) -> Vec<(String, V)>
where
    V: Copy,
{
    let mut pairs: Vec<(&String, &V)> = totals.iter().collect();
    pairs.sort_by(|a, b| by_value(a.1, b.1).then_with(|| a.0.cmp(b.0)));
    pairs
        .into_iter()
        .map(|(name, value)| (name.clone(), *value))
        .collect()
}

/// The source's `name()`: a trimmed non-empty string or the fallback.
fn display_name(raw: Option<&str>, fallback: &str) -> String {
    raw.map(str::trim)
        .filter(|raw| !raw.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

/// The source's `cleaned()`: trimmed, one pair of surrounding quotes stripped, trimmed
/// again; empty is no value.
fn cleaned(raw: Option<&String>) -> Option<String> {
    let mut value = raw?.trim();
    if (value.starts_with('"') && value.ends_with('"') && value.len() > 1)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() > 1)
    {
        value = value[1..value.len() - 1].trim();
    }
    (!value.is_empty()).then(|| value.to_owned())
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

/// The source's `usd()`: dollars, two fraction digits, negatives clamped away.
fn usd(value: f64) -> String {
    format!("${:.2}", value.max(0.0))
}

/// The source's `numberText()`: whole counts with thousands separators — the card's
/// token and request counts are always whole.
fn number_text(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{sign}{grouped}")
}

/// The UTC calendar day of an epoch-second bucket start, `YYYY-MM-DD`. A start outside
/// the readable range is malformed: a day this card cannot name is a day it cannot
/// show.
fn day_key(start: i64) -> Result<String, ProviderError> {
    let at = OffsetDateTime::from_unix_timestamp(start)
        .map_err(|_| ProviderError::malformed("an OpenAI bucket start is not a plausible time"))?;
    Ok(format!(
        "{:04}-{:02}-{:02}",
        at.year(),
        u8::from(at.month()),
        at.day()
    ))
}

/// The UTC reading of an instant.
fn utc_date(at: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(at.as_unix()).expect("a plausible timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_700_179_200).expect("plausible")
    }

    // Recorded bodies, verbatim from OpenAIAPIUsageFetcherTests.swift.
    const COSTS: &str = r#"{
          "object": "page",
          "data": [
            {
              "object": "bucket",
              "start_time": 1700000000,
              "end_time": 1700086400,
              "results": [
                {
                  "object": "organization.costs.result",
                  "amount": { "value": 12.50, "currency": "usd" },
                  "line_item": "Text tokens"
                },
                {
                  "object": "organization.costs.result",
                  "amount": { "value": "2.25", "currency": "usd" },
                  "line_item": "Web search tool calls"
                }
              ]
            },
            {
              "object": "bucket",
              "start_time": 1700086400,
              "end_time": 1700172800,
              "results": [
                {
                  "object": "organization.costs.result",
                  "amount": { "value": 4.00, "currency": "usd" },
                  "line_item": "Text tokens"
                }
              ]
            }
          ],
          "has_more": false,
          "next_page": null
        }"#;
    const COMPLETIONS: &str = r#"{
          "object": "page",
          "data": [
            {
              "object": "bucket",
              "start_time": 1700000000,
              "end_time": 1700086400,
              "results": [
                {
                  "object": "organization.usage.completions.result",
                  "input_tokens": 1000,
                  "input_cached_tokens": 250,
                  "output_tokens": 500,
                  "num_model_requests": 7,
                  "model": "gpt-5.2"
                },
                {
                  "object": "organization.usage.completions.result",
                  "input_tokens": 300,
                  "output_tokens": 200,
                  "num_model_requests": 3,
                  "model": "gpt-5.2-codex"
                }
              ]
            },
            {
              "object": "bucket",
              "start_time": 1700086400,
              "end_time": 1700172800,
              "results": [
                {
                  "object": "organization.usage.completions.result",
                  "input_tokens": 200,
                  "output_tokens": 100,
                  "num_model_requests": 2,
                  "model": "gpt-5.2"
                }
              ]
            }
          ],
          "has_more": false,
          "next_page": null
        }"#;
    const COSTS_PAGE_1: &str = r#"{
      "object": "page",
      "data": [
        {
          "object": "bucket",
          "start_time": 1700000000,
          "end_time": 1700086400,
          "results": [
            {
              "object": "organization.costs.result",
              "amount": { "value": 1.25, "currency": "usd" },
              "line_item": "Text tokens"
            }
          ]
        }
      ],
      "has_more": true,
      "next_page": "costs_page_2"
    }"#;
    const COSTS_PAGE_2: &str = r#"{
      "object": "page",
      "data": [
        {
          "object": "bucket",
          "start_time": 1700000000,
          "end_time": 1700086400,
          "results": [
            {
              "object": "organization.costs.result",
              "amount": { "value": 2.75, "currency": "usd" },
              "line_item": "Web search tool calls"
            }
          ]
        }
      ],
      "has_more": false,
      "next_page": null
    }"#;
    const COMPLETIONS_PAGE_1: &str = r#"{
      "object": "page",
      "data": [
        {
          "object": "bucket",
          "start_time": 1700000000,
          "end_time": 1700086400,
          "results": [
            {
              "object": "organization.usage.completions.result",
              "input_tokens": 10,
              "output_tokens": 5,
              "num_model_requests": 1,
              "model": "gpt-5.2"
            }
          ]
        }
      ],
      "has_more": true,
      "next_page": "completions_page_2"
    }"#;
    const COMPLETIONS_PAGE_2: &str = r#"{
      "object": "page",
      "data": [
        {
          "object": "bucket",
          "start_time": 1700000000,
          "end_time": 1700086400,
          "results": [
            {
              "object": "organization.usage.completions.result",
              "input_tokens": 20,
              "output_tokens": 10,
              "num_model_requests": 2,
              "model": "gpt-5.2"
            }
          ]
        }
      ],
      "has_more": false,
      "next_page": null
    }"#;
    const EMPTY_PAGE: &str = r#"{"object":"page","data":[],"has_more":false,"next_page":null}"#;
    const COSTS_WITHOUT_DATA: &str = r#"{"object":"page","has_more":false,"next_page":null}"#;
    const COMPLETIONS_WITHOUT_STATE: &str = r#"{"object":"page","data":[],"next_page":null}"#;
    const REPEATING_CURSOR: &str = r#"{
          "object": "page",
          "data": [],
          "has_more": true,
          "next_page": "same_page"
        }"#;
    const MISSING_CURSOR: &str = r#"{
          "object": "page",
          "data": [],
          "has_more": true,
          "next_page": null
        }"#;
    const CREDIT_GRANTS: &str = r#"{
          "object": "credit_summary",
          "total_granted": 25.5,
          "total_used": 7.25,
          "total_available": 18.25,
          "grants": {
            "object": "list",
            "data": [
              {
                "grant_amount": 10.0,
                "used_amount": 1.0,
                "effective_at": 1690000000,
                "expires_at": 1800000000
              }
            ]
          }
        }"#;

    fn row_of<'a>(snapshot: &'a Snapshot, label: &str) -> &'a DetailRow {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no {label} row in {snapshot:?}"))
    }

    fn options(project: Option<&str>, fallback: Option<&str>) -> Options {
        [
            project.map(|value| (PROJECT.to_owned(), value.to_owned())),
            fallback.map(|value| (BALANCE_FALLBACK.to_owned(), value.to_owned())),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    #[test]
    fn the_recorded_bodies_summarise_into_daily_and_window_totals() {
        let costs = parse_costs_page(COSTS).expect("parses").data;
        let completions = parse_completions_page(COMPLETIONS).expect("parses").data;
        let days = summarise(&costs, &completions, now()).expect("summarises");
        assert_eq!(days.len(), 2);
        assert!((days[0].cost_usd - 14.75).abs() < 1e-9);
        assert_eq!(days[0].requests, 10);
        assert_eq!(days[0].total_tokens, 2000);
        assert_eq!(days[0].cached_input_tokens, 250);
        assert_eq!(
            days[0].line_items.first().map(|(name, _)| name.as_str()),
            Some("Text tokens")
        );
        let totals: f64 = days.iter().map(|day| day.cost_usd).sum();
        assert!((totals - 18.75).abs() < 1e-9);
        assert_eq!(days.iter().map(|day| day.requests).sum::<i64>(), 12);
        assert_eq!(days.iter().map(|day| day.total_tokens).sum::<i64>(), 2300);
    }

    #[test]
    fn the_recorded_bodies_render_three_sections_and_no_window() {
        let costs = parse_costs_page(COSTS).expect("parses").data;
        let completions = parse_completions_page(COMPLETIONS).expect("parses").data;
        let snapshot = snapshot(&costs, &completions, now()).expect("assembles");
        assert!(
            snapshot.windows.is_empty(),
            "the Admin usage endpoints report spend with no limit"
        );
        assert_eq!(row_of(&snapshot, "Spend").value, "$18.75 · Last 30 days");
        assert_eq!(row_of(&snapshot, "Requests").value, "12");
        assert_eq!(
            row_of(&snapshot, "Tokens").value,
            "2,300 · 1,500 input · 800 output"
        );
        assert_eq!(row_of(&snapshot, "Cached input").value, "250");
        assert_eq!(
            row_of(&snapshot, "gpt-5.2").value,
            "1,800 tokens · 9 requests"
        );
        assert_eq!(row_of(&snapshot, "Text tokens").value, "$16.50");
        assert_eq!(row_of(&snapshot, "Web search tool calls").value, "$2.25");
    }

    #[test]
    fn nonfinite_cost_strings_are_malformed() {
        // The recorded rejection set, in the recorded body shape.
        for value in ["NaN", "Infinity", "-Infinity", "1e309", "-1e309"] {
            let costs = format!(
                r#"{{
          "data": [{{
            "start_time": 1700000000,
            "end_time": 1700086400,
            "results": [{{ "amount": {{ "value": "{value}", "currency": "usd" }} }}]
          }}],
          "has_more": false,
          "next_page": null
        }}"#
            );
            let error = parse_costs_page(&costs).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{value}: {error:?}"
            );
        }
    }

    #[test]
    fn pages_missing_their_shape_are_malformed() {
        // The recorded bodies: costs without its data array, completions without its
        // pagination state, plus the procedure's canonical partial body.
        for error in [
            parse_costs_page(COSTS_WITHOUT_DATA).expect_err("data is missing"),
            parse_completions_page(COMPLETIONS_WITHOUT_STATE).expect_err("has_more is missing"),
            parse_costs_page("{\"partial\":").expect_err("partial"),
            parse_completions_page("{\"partial\":").expect_err("partial"),
        ] {
            assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
        }
        let named = parse_costs_page(COSTS_WITHOUT_DATA).expect_err("names the field");
        assert!(format!("{named}").contains("data"), "{named}");
    }

    #[test]
    fn a_token_count_that_is_not_a_number_is_malformed() {
        // A constructed error-path body, as the procedure allows: a recognised field
        // whose value cannot be read.
        let error = parse_completions_page(
            r#"{"data":[{"start_time":1700000000,"end_time":1700086400,"results":[{"input_tokens":"many"}]}],"has_more":false,"next_page":null}"#,
        )
        .expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn fields_these_parsers_do_not_know_are_skipped() {
        // The unknown-kind rule: an unfamiliar field rides along without breaking the
        // recognised ones — the recorded bodies already carry `object` fields this
        // parser never reads.
        let costs = parse_costs_page(&COSTS.replace(
            "\"has_more\": false",
            "\"has_more\": false, \"future\": {\"whatever\": \"it says\"}",
        ))
        .expect("parses");
        assert_eq!(costs.data.len(), 2);
        let completions = parse_completions_page(&COMPLETIONS.replace(
            "\"next_page\": null",
            "\"next_page\": null, \"future\": true",
        ))
        .expect("parses");
        assert_eq!(completions.data.len(), 2);
    }

    #[test]
    fn the_recorded_pages_follow_stop_and_repeat_their_cursors() {
        let first = parse_costs_page(COSTS_PAGE_1).expect("parses");
        assert!(first.has_more);
        assert_eq!(first.next_page.as_deref(), Some("costs_page_2"));
        let mut seen = Vec::new();
        assert_eq!(
            next_cursor(first.has_more, first.next_page.as_deref(), &mut seen).expect("advances"),
            Some("costs_page_2".to_owned())
        );
        let second = parse_costs_page(COSTS_PAGE_2).expect("parses");
        assert!(!second.has_more);
        assert_eq!(
            next_cursor(second.has_more, second.next_page.as_deref(), &mut seen).expect("stops"),
            None
        );

        // The recorded pagination script walks the completions endpoint the same way,
        // under its own cursor.
        let first = parse_completions_page(COMPLETIONS_PAGE_1).expect("parses");
        assert_eq!(first.next_page.as_deref(), Some("completions_page_2"));
        let mut seen = Vec::new();
        assert_eq!(
            next_cursor(first.has_more, first.next_page.as_deref(), &mut seen).expect("advances"),
            Some("completions_page_2".to_owned())
        );
        let second = parse_completions_page(COMPLETIONS_PAGE_2).expect("parses");
        assert_eq!(
            next_cursor(second.has_more, second.next_page.as_deref(), &mut seen).expect("stops"),
            None
        );

        let repeating = parse_costs_page(REPEATING_CURSOR).expect("parses");
        let mut looped = Vec::new();
        next_cursor(
            repeating.has_more,
            repeating.next_page.as_deref(),
            &mut looped,
        )
        .expect("serves");
        let error = next_cursor(
            repeating.has_more,
            repeating.next_page.as_deref(),
            &mut looped,
        )
        .expect_err("must refuse a repeated cursor");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
        assert!(format!("{error}").contains("repeated"), "{error}");

        let missing = parse_completions_page(MISSING_CURSOR).expect("parses");
        let error = next_cursor(
            missing.has_more,
            missing.next_page.as_deref(),
            &mut Vec::new(),
        )
        .expect_err("must refuse a missing cursor");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
        assert!(format!("{error}").contains("missing"), "{error}");
    }

    #[test]
    fn ninety_days_of_history_is_three_ranges_per_endpoint() {
        // The recorded pagination test: 90 days at the recorded now (UTC midnight of
        // 2023-11-15) is 6 requests whose limits are 31, 31, 28 — twice.
        let ranges = daily_ranges(now(), 90);
        let limits: Vec<i64> = ranges.iter().map(|range| range.limit).collect();
        assert_eq!(limits, [31, 31, 28]);
        assert_eq!(ranges[0].start, 1_692_489_600);
        assert_eq!(ranges[2].end, 1_700_265_600);
        // The window this provider actually polls is the plugin's default: 30 days,
        // one range.
        let polled = daily_ranges(now(), HISTORY_DAYS);
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].limit, 30);
    }

    #[test]
    fn the_page_requests_carry_the_recorded_query_and_the_bearer_key() {
        let api = OpenAiApi::new(
            Credential::new("sk-test"),
            &options(Some(" proj_abc "), None),
        )
        .expect("builds");
        let range = DayRange {
            start: 1_700_179_200,
            end: 1_700_265_600,
            limit: 1,
        };
        let costs = api
            .page_request(COSTS_URL, "line_item", range, None)
            .expect("builds");
        assert_eq!(costs.method(), reqwest::Method::GET);
        assert_eq!(
            costs.url().as_str(),
            "https://api.openai.com/v1/organization/costs?start_time=1700179200&end_time=1700265600&bucket_width=1d&limit=1&group_by=line_item&project_ids=proj_abc"
        );
        let completions = api
            .page_request(COMPLETIONS_URL, "model", range, None)
            .expect("builds");
        assert_eq!(
            completions.url().as_str(),
            "https://api.openai.com/v1/organization/usage/completions?start_time=1700179200&end_time=1700265600&bucket_width=1d&limit=1&group_by=model&project_ids=proj_abc"
        );
        for request in [costs, completions] {
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer sk-test"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json"
            );
        }
        let paged = api
            .page_request(COSTS_URL, "line_item", range, Some("costs_page_2"))
            .expect("builds");
        assert!(
            paged.url().as_str().ends_with("&page=costs_page_2"),
            "{}",
            paged.url()
        );

        let unscoped =
            OpenAiApi::new(Credential::new("sk-test"), &options(None, None)).expect("builds");
        let request = unscoped
            .page_request(COSTS_URL, "line_item", range, None)
            .expect("builds");
        assert!(
            !request.url().as_str().contains("project_ids"),
            "{}",
            request.url()
        );
    }

    #[test]
    fn the_project_setting_is_trimmed_and_unquoted() {
        // " proj_abc " is the recorded spelling of the filter's value; the quote
        // stripping is the source's own `cleaned`, ported with it.
        let api = OpenAiApi::new(
            Credential::new("sk-test"),
            &options(Some(" 'proj_abc' "), None),
        )
        .expect("builds");
        assert_eq!(api.project.as_deref(), Some("proj_abc"));
    }

    #[test]
    fn the_recorded_credit_balance_reads_its_totals_and_next_expiry() {
        let grants = parse_credit_grants(
            CREDIT_GRANTS,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses");
        assert_eq!(grants.total_granted, 25.5);
        assert_eq!(grants.total_used, 7.25);
        assert_eq!(grants.total_available, 18.25);
        assert_eq!(
            grants.next_expiry.map(Timestamp::as_unix),
            Some(1_800_000_000)
        );
    }

    #[test]
    fn the_credit_balance_renders_one_window_and_three_rows() {
        let now = Timestamp::from_unix(1_700_000_000).expect("plausible");
        let grants = parse_credit_grants(CREDIT_GRANTS, now).expect("parses");
        let snapshot = balance_snapshot(&grants, now);
        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(window.key.as_str(), "balance");
        assert_eq!(window.title, "API credits");
        assert!(
            (window.used_percent - 7.25 / 25.5 * 100.0).abs() < 1e-9,
            "{}",
            window.used_percent
        );
        assert_eq!(window.length, None);
        assert_eq!(
            window.resets_at.map(Timestamp::as_unix),
            Some(1_800_000_000)
        );
        assert_eq!(
            window.subtitle.as_deref(),
            Some("$7.25 of $25.50 used · $18.25 left")
        );
        assert_eq!(row_of(&snapshot, "Available").value, "$18.25");
        assert_eq!(row_of(&snapshot, "Used").value, "$7.25");
        assert_eq!(row_of(&snapshot, "Granted").value, "$25.50");
    }

    #[test]
    fn a_credit_balance_that_cannot_be_read_is_malformed() {
        // A constructed error-path body — the plugin's `finite` throws on "many" — plus
        // the procedure's canonical partial body.
        for body in [
            r#"{"total_granted":"many","total_used":7.25,"total_available":18.25}"#,
            "{\"partial\":",
        ] {
            let error = parse_credit_grants(body, now()).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn the_balance_fallback_option_is_off_unless_it_says_exactly_on() {
        // The plugin enables the fallback only for the exact value "1"; the procedure's
        // fourth test — an unrecognised value falls back to the default.
        for (setting, expected) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("maybe"), false),
        ] {
            let api = OpenAiApi::new(Credential::new("sk-test"), &options(None, setting))
                .expect("builds");
            assert_eq!(api.fallback, expected, "{setting:?}");
        }
    }

    #[test]
    fn the_credit_request_targets_the_legacy_billing_endpoint() {
        let api = OpenAiApi::new(Credential::new("sk-test"), &options(None, None)).expect("builds");
        let request = api.credit_request().expect("builds");
        assert_eq!(
            request.url().as_str(),
            "https://api.openai.com/v1/dashboard/billing/credit_grants"
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer sk-test"
        );
    }

    #[test]
    fn the_recorded_empty_page_carries_no_buckets_and_no_cursor() {
        let page = parse_costs_page(EMPTY_PAGE).expect("parses");
        assert!(page.data.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.next_page, None);
    }

    #[test]
    fn the_spec_publishes_two_options_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "OpenAI");
        assert_eq!(SPEC.options.len(), 2);
        assert!(SPEC.options.iter().all(|option| !option.required));
        assert!(build(Credential::new("sk-test"), &Options::new()).is_ok());
    }

    #[test]
    fn an_openai_client_never_prints_its_credential() {
        let api =
            OpenAiApi::new(Credential::new("sk-super-secret"), &Options::new()).expect("builds");
        let rendered = format!("{api:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
