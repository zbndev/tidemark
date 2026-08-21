//! Poe.
//!
//! Ported from CodexBar's `Plugins/poe.js`; the recorded bodies in
//! `PoeUsageFetcherTests.swift` are the contract. Never seen answering: every number in
//! the tests below is a body CodexBar recorded.
//!
//! # The two requests
//!
//! `GET https://api.poe.com/usage/current_balance` first: `current_point_balance`, sent
//! bare (`1500`) or quoted (`"2500"`) — both spellings are recorded. A points balance
//! has no limit to measure against, so it is a detail row and nothing else: no window,
//! the card renders empty, and that is accepted.
//!
//! Then up to five pages of `GET /usage/points_history?limit=100`, following each page's
//! own cursor and keeping entries newer than thirty days. The plugin wraps the entire
//! history in a `try` that swallows every failure — a page that answers 500, a page that
//! is not JSON, a row whose points are not a number — and keeps the rows that already
//! landed. That is kept here, because a broken history must not cost the balance. But
//! the swallow is made visible — one row saying the history is unavailable, or
//! incomplete when some of it landed — where CodexBar said nothing at all.
//!
//! # What ships untested
//!
//! No recorded body carries a history row; the only recorded page is an empty one. The
//! row parser's date spellings and points handling, the cutoff, and every aggregate fed
//! by them — today, the last 7 and 30 days, top model, usage mix, recent activity — are
//! ported from the plugin and tested by nothing here. The error-path bodies in the
//! malformed tests are constructed, as the porting procedure allows; no number in a
//! passing assertion is invented.

use super::{HandSpec, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp};
use time::OffsetDateTime;

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "poe";

/// Where the balance lives. Poe has one host and no regional or self-hosted variant, so
/// there is no setting to resolve.
const BALANCE_URL: &str = "https://api.poe.com/usage/current_balance";

/// Where the history pages live.
const HISTORY_URL: &str = "https://api.poe.com/usage/points_history";

/// Pages fetched at most, per fetch. The plugin's cap, carried over: a cursor that never
/// runs out must not turn one poll into an unbounded walk.
const HISTORY_PAGES: usize = 5;

/// How far back the history is kept. The plugin's cutoff, carried over.
const HISTORY_WINDOW_SECS: i64 = 30 * 86_400;

/// Poe as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Poe",
    credential_hint: "poe.com → Settings → API keys.",
    options: &[],
    build,
};

/// Builds a pollable client from the stored key. Poe has nothing to configure.
fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Poe::new(credential)?))
}

/// One Poe account: the key, and the two endpoints it unlocks.
pub struct Poe {
    client: reqwest::Client,
    credential: Credential,
}

impl Poe {
    /// Builds a client. Poe has one host, so the URLs are constants and there is nothing
    /// to resolve at build time.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    /// The balance request, built but not sent, so the placement of the key is testable.
    fn balance_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(BALANCE_URL)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// One history page, `starting_after` the previous page's cursor when there is one.
    fn history_request(&self, cursor: Option<&str>) -> Result<reqwest::Request, ProviderError> {
        let mut request = self.client.get(HISTORY_URL).query(&[("limit", "100")]);
        if let Some(cursor) = cursor {
            request = request.query(&[("starting_after", cursor)]);
        }
        request
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
        let body = super::request(&self.client, self.balance_request()?).await?;
        let balance = parse_balance(&body)?;
        let history = self.history(now).await;
        Ok(snapshot(balance, &history, now))
    }

    /// Walks the history pages, keeping what lands inside the cutoff. Any failure —
    /// transport, status, shape — is the plugin's swallowed `catch`: the entries already
    /// collected survive and the attempt is marked failed, which [`snapshot`] turns into
    /// one visible row rather than silence.
    async fn history(&self, now: Timestamp) -> History {
        let cutoff = now.saturating_add_seconds(-HISTORY_WINDOW_SECS);
        let mut entries = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..HISTORY_PAGES {
            match self
                .fetch_page(cursor.as_deref(), cutoff, &mut entries)
                .await
            {
                Ok(Some(next)) => cursor = Some(next),
                Ok(None) => return History::complete(entries),
                Err(()) => return History::failed(entries),
            }
        }
        History::complete(entries)
    }

    /// One page: sends it, parses it, keeps the entries inside the cutoff, and says
    /// whether another page follows. `Err(())` is the swallowed failure; the reason is
    /// not kept, exactly as the plugin's bare `catch {}` keeps nothing.
    async fn fetch_page(
        &self,
        cursor: Option<&str>,
        cutoff: Timestamp,
        entries: &mut Vec<HistoryEntry>,
    ) -> Result<Option<String>, ()> {
        let request = self.history_request(cursor).map_err(|_| ())?;
        let body = super::request(&self.client, request)
            .await
            .map_err(|_| ())?;
        let page = parse_history_page(&body).map_err(|_| ())?;
        let last_at = page.entries.last().map(|entry| entry.at);
        entries.extend(page.entries.into_iter().filter(|entry| entry.at >= cutoff));
        // The pages run newest first: once a page's last row predates the cutoff, no
        // later page can hold anything worth keeping.
        if page.cursor.is_none() || last_at.is_some_and(|at| at < cutoff) {
            return Ok(None);
        }
        Ok(page.cursor)
    }
}

impl fmt::Debug for Poe {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Poe")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Poe {
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

/// The history attempt's outcome: the entries inside the cutoff, and whether the attempt
/// failed partway — CodexBar's `try` keeps the rows that already landed, and so does
/// this, but the failure is marked so the details can say so.
#[derive(Debug, Clone, PartialEq)]
struct History {
    entries: Vec<HistoryEntry>,
    failed: bool,
}

impl History {
    fn complete(entries: Vec<HistoryEntry>) -> Self {
        Self {
            entries,
            failed: false,
        }
    }

    fn failed(entries: Vec<HistoryEntry>) -> Self {
        Self {
            entries,
            failed: true,
        }
    }
}

/// One history row, reduced to what the aggregates need.
#[derive(Debug, Clone, PartialEq)]
struct HistoryEntry {
    at: Timestamp,
    points: f64,
    cost_usd: Option<f64>,
    model: String,
    usage_type: String,
}

/// One parsed page.
#[derive(Debug, Clone, PartialEq)]
struct HistoryPage {
    entries: Vec<HistoryEntry>,
    cursor: Option<String>,
}

/// Reads the balance body. Pure: the recorded spellings are reachable from a test.
///
/// `Ok(None)` for an absent balance is the recorded `{}` case, not an error. A balance
/// that is present but not a number fails the fetch, as the plugin's `optionalNumber`
/// throws outside its `try`.
fn parse_balance(body: &str) -> Result<Option<f64>, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a Poe balance response: {e}")))?;
    let Value::Object(root) = root else {
        return Err(ProviderError::malformed(
            "the Poe balance response must be a JSON object",
        ));
    };
    match root.get("current_point_balance") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => numeric(value, "current_point_balance").map(Some),
    }
}

/// Reads one history page. Pure, and the whole page fails together: a row of a
/// recognised kind whose points are not a number is the plugin's thrown (and swallowed)
/// error, not a row to skip.
fn parse_history_page(body: &str) -> Result<HistoryPage, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a Poe history page: {e}")))?;
    let Value::Object(root) = root else {
        return Err(ProviderError::malformed(
            "a Poe history page must be a JSON object",
        ));
    };

    // The three array spellings the plugin knows, in its order. A page carrying none of
    // them says nothing: an unfamiliar root key is a shape that did not exist when the
    // plugin was written, not a failure to read one that did.
    let rows = ["data", "items", "results"]
        .iter()
        .find_map(|name| root.get(*name).and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let mut entries = Vec::new();
    for row in rows {
        let Value::Object(row) = row else {
            continue;
        };
        // A row with no readable date is skipped, exactly as the source skips it; the
        // cutoff itself is applied by the fetch, which owns the clock.
        let Some(at) = row_time(row) else {
            continue;
        };
        let points = optional_number(row, &["cost_points", "points", "point_cost"], "points")?
            .unwrap_or(0.0)
            .max(0.0);
        let cost_usd = optional_number(row, &["cost_usd", "usd"], "cost_usd")?;
        entries.push(HistoryEntry {
            at,
            points,
            cost_usd,
            model: word(row, "bot_name").unwrap_or("unknown").to_owned(),
            usage_type: word(row, "usage_type").unwrap_or("unknown").to_owned(),
        });
    }

    // The next page: the cursor the page names, or — when it says it has more and its
    // last row carries a query id — that id. An empty string is no cursor, as the
    // plugin's falsy check reads it.
    let cursor = word(&root, "next_cursor").map(str::to_owned).or_else(|| {
        (root.get("has_more").and_then(Value::as_bool) == Some(true))
            .then(|| {
                rows.last()
                    .and_then(Value::as_object)
                    .and_then(|row| word(row, "query_id"))
                    .map(str::to_owned)
            })
            .flatten()
    });

    Ok(HistoryPage { entries, cursor })
}

/// One number, bare or quoted, as this endpoint sends both spellings (recorded: `1500`
/// and `"2500"`). Anything else fails the field it names.
fn numeric(value: &Value, field: &str) -> Result<f64, ProviderError> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.trim().parse::<f64>().ok(),
        _ => None,
    };
    number
        .filter(|number| number.is_finite())
        .ok_or_else(|| ProviderError::malformed(format!("Poe {field} must be numeric")))
}

/// One optional number out of the given spellings, in order: the first present, non-null
/// one wins; an absent or null field is `None`, and a present value that is not a number
/// fails the page.
fn optional_number(
    row: &serde_json::Map<String, Value>,
    names: &[&str],
    field: &str,
) -> Result<Option<f64>, ProviderError> {
    for name in names {
        match row.get(*name) {
            None | Some(Value::Null) => continue,
            Some(value) => return numeric(value, field).map(Some),
        }
    }
    Ok(None)
}

/// A non-empty trimmed string field, when the row carries one.
fn word<'a>(row: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a str> {
    row.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// When a row happened, from whichever of its three date spellings is present. The
/// plugin accepts epoch seconds, milliseconds or microseconds, or a numeric or ISO
/// string; a value that is none of those is no date, and the row is skipped.
fn row_time(row: &serde_json::Map<String, Value>) -> Option<Timestamp> {
    let value = ["creation_time", "timestamp", "created_at"]
        .iter()
        .find_map(|name| row.get(*name).filter(|value| !value.is_null()))?;
    match value {
        Value::Number(number) => number.as_f64().and_then(epoch),
        Value::String(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            raw.parse::<f64>()
                .ok()
                .and_then(epoch)
                .or_else(|| parse_rfc3339(raw))
        }
        _ => None,
    }
}

/// The three epoch spellings: above 10^14 microseconds, above 10^12 milliseconds, below
/// that seconds. The thresholds are the plugin's.
fn epoch(value: f64) -> Option<Timestamp> {
    let millis = if value > 1e14 {
        value / 1_000.0
    } else if value > 1e12 {
        value
    } else {
        value * 1_000.0
    };
    Timestamp::from_unix_millis(millis as i64).ok()
}

/// Assembles the snapshot. Pure, so the recorded balance and degraded-history renderings
/// are reachable from a test.
///
/// No window, ever: a points balance has no limit to draw a bar against, and the task's
/// shape for that is details only — the card renders empty, which is accepted.
fn snapshot(balance: Option<f64>, history: &History, now: Timestamp) -> Snapshot {
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows: Vec::new(),
        details: points_details(balance, history, now),
    }
}

/// The one section Poe ever produces, built from whatever the two requests managed.
fn points_details(balance: Option<f64>, history: &History, now: Timestamp) -> Vec<DetailSection> {
    let mut rows = Vec::new();
    if let Some(balance) = balance {
        rows.push(DetailRow {
            label: "Current balance".to_owned(),
            value: format!("{} points", compact(balance)),
        });
    }
    if history.failed {
        let state = if history.entries.is_empty() {
            "Unavailable"
        } else {
            "Incomplete"
        };
        rows.push(DetailRow {
            label: "History".to_owned(),
            value: format!("{state} right now"),
        });
    }
    if !history.entries.is_empty() {
        rows.extend(summaries(&history.entries, now));
        rows.extend(activity(&history.entries));
    }
    if rows.is_empty() {
        return Vec::new();
    }
    vec![DetailSection {
        title: "Points".to_owned(),
        rows,
    }]
}

/// Totals over a set of entries or days.
#[derive(Debug, Default)]
struct Summary {
    points: f64,
    requests: usize,
    cost: f64,
    has_cost: bool,
}

impl Summary {
    fn row(self, label: &str) -> DetailRow {
        let mut value = format!("{} points", compact(self.points));
        value.push_str(&format!(" · {} requests", self.requests));
        if self.has_cost {
            value.push_str(&format!(" · ${:.2}", self.cost));
        }
        DetailRow {
            label: label.to_owned(),
            value,
        }
    }

    fn add_entry(&mut self, entry: &HistoryEntry) {
        self.points += entry.points;
        self.requests += 1;
        if let Some(cost) = entry.cost_usd.map(|cost| cost.max(0.0)) {
            self.cost += cost;
            self.has_cost = true;
        }
    }

    fn add_day(&mut self, day: &Day) {
        self.points += day.points;
        self.requests += day.requests;
        if day.has_cost {
            self.cost += day.cost;
            self.has_cost = true;
        }
    }
}

/// One day's totals.
#[derive(Debug, Default)]
struct Day {
    points: f64,
    requests: usize,
    cost: f64,
    has_cost: bool,
}

/// The summary rows — today, the last seven days, the last thirty, the heaviest model,
/// and the two heaviest usage types — as the plugin computes them: today over entries,
/// the spans over the last N day buckets, ranks by points with the name breaking ties.
fn summaries(entries: &[HistoryEntry], now: Timestamp) -> Vec<DetailRow> {
    let mut daily: BTreeMap<String, Day> = BTreeMap::new();
    let mut models: BTreeMap<String, f64> = BTreeMap::new();
    let mut types: BTreeMap<String, f64> = BTreeMap::new();
    for entry in entries {
        let day = daily.entry(utc_day(entry.at)).or_default();
        day.points += entry.points;
        day.requests += 1;
        if let Some(cost) = entry.cost_usd.map(|cost| cost.max(0.0)) {
            day.cost += cost;
            day.has_cost = true;
        }
        *models.entry(entry.model.clone()).or_default() += entry.points;
        *types.entry(entry.usage_type.clone()).or_default() += entry.points;
    }

    let mut rows = Vec::new();
    let today = utc_day(now);
    let mut todays = Summary::default();
    for entry in entries.iter().filter(|entry| utc_day(entry.at) == today) {
        todays.add_entry(entry);
    }
    rows.push(todays.row("Today"));
    for (label, count) in [("Last 7 days", 7), ("Last 30 days", 30)] {
        let mut summary = Summary::default();
        for day in daily.values().rev().take(count) {
            summary.add_day(day);
        }
        rows.push(summary.row(label));
    }

    let ranked_models = ranked(&models);
    if let Some((model, points)) = ranked_models.first() {
        rows.push(DetailRow {
            label: "Top model".to_owned(),
            value: format!("{model} · {} points", compact(**points)),
        });
    }
    let mix = ranked(&types)
        .iter()
        .take(2)
        .map(|(name, points)| format!("{name}: {} points", compact(**points)))
        .collect::<Vec<_>>()
        .join(" · ");
    if !mix.is_empty() {
        rows.push(DetailRow {
            label: "Usage mix".to_owned(),
            value: mix,
        });
    }
    rows
}

/// Names ranked by total points, heaviest first, the name breaking ties.
fn ranked(totals: &BTreeMap<String, f64>) -> Vec<(&String, &f64)> {
    let mut ranked: Vec<(&String, &f64)> = totals.iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
    ranked
}

/// The three most recent entries, newest first, with the plugin's `MM-DD HH:mm` stamps.
fn activity(entries: &[HistoryEntry]) -> Vec<DetailRow> {
    let mut recent: Vec<&HistoryEntry> = entries.iter().collect();
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.at));
    recent
        .into_iter()
        .take(3)
        .enumerate()
        .map(|(index, entry)| {
            let when = month_day_hour(entry.at);
            DetailRow {
                label: if index == 0 {
                    "Recent activity".to_owned()
                } else {
                    when.clone()
                },
                value: format!(
                    "{when} · {} · {} points",
                    entry.model,
                    compact(entry.points)
                ),
            }
        })
        .collect()
}

/// CodexBar's `compact`: one fraction digit below a thousand, none above, thousands
/// separators — the recorded bodies render 1500 as `1,500`.
fn compact(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let value = value.abs();
    let (whole, tenth) = if value >= 1000.0 {
        (value.round() as i64, 0)
    } else {
        let tenths = (value * 10.0).round() as i64;
        (tenths / 10, tenths % 10)
    };
    let mut text = format!("{sign}{}", grouped(whole));
    if tenth > 0 {
        text.push_str(&format!(".{tenth}"));
    }
    text
}

/// Thousands separators, the way the source's number formatter groups.
fn grouped(whole: i64) -> String {
    let digits = whole.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The UTC calendar day of an instant, `YYYY-MM-DD`, which is how the aggregates bucket.
fn utc_day(at: Timestamp) -> String {
    let date = utc_date(at);
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The source's row stamp, `MM-DD HH:mm` in UTC.
fn month_day_hour(at: Timestamp) -> String {
    let date = utc_date(at);
    format!(
        "{:02}-{:02} {:02}:{:02}",
        u8::from(date.month()),
        date.day(),
        date.hour(),
        date.minute()
    )
}

/// The UTC reading of an instant. `Timestamp` only holds 2020..2100, which
/// `from_unix_timestamp` always accepts.
fn utc_date(at: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(at.as_unix()).expect("a plausible timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_787_000_000).expect("plausible")
    }

    // Recorded bodies, verbatim from `PoeUsageFetcherTests.swift`: the balance bodies are
    // `#"{"current_point_balance": \#(raw)}"#` for raw = 1500 and "\"2500\"", and the
    // history transport always answers `{"data":[],"next_cursor":null}`.

    #[test]
    fn a_balance_arrives_bare_or_quoted() {
        assert_eq!(
            parse_balance(r#"{"current_point_balance": 1500}"#).expect("parses"),
            Some(1500.0)
        );
        assert_eq!(
            parse_balance(r#"{"current_point_balance": "2500"}"#).expect("parses"),
            Some(2500.0)
        );
    }

    #[test]
    fn an_absent_balance_is_not_an_error() {
        assert_eq!(parse_balance("{}").expect("parses"), None);
    }

    #[test]
    fn a_balance_that_cannot_be_read_is_malformed() {
        // `not-json` is the recorded malformed fixture; the other two are the porting
        // procedure's canonical malformed bodies for this field.
        for body in [
            "not-json",
            "{\"partial\":",
            r#"{"current_point_balance":"many"}"#,
        ] {
            let error = parse_balance(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn the_recorded_empty_history_page_yields_nothing() {
        let page = parse_history_page(r#"{"data":[],"next_cursor":null}"#).expect("parses");
        assert!(page.entries.is_empty());
        assert_eq!(page.cursor, None);
    }

    #[test]
    fn a_history_page_without_a_known_array_is_an_empty_page() {
        // The plugin reads `data`, `items` or `results` and nothing else: an unfamiliar
        // root key is a page that says nothing, not a refusal. The cursor is still read,
        // whatever the array is called.
        let page = parse_history_page(r#"{"entries":[],"next_cursor":"x"}"#).expect("parses");
        assert!(page.entries.is_empty());
        assert_eq!(page.cursor, Some("x".to_owned()));
    }

    #[test]
    fn a_history_row_whose_points_are_not_a_number_fails_the_page() {
        // A constructed error-path body, as the procedure allows: the row is a recognised
        // kind with an unreadable shape, which the plugin turns into a thrown (and
        // swallowed) failure of the whole history.
        let error =
            parse_history_page(r#"{"data":[{"creation_time":1787000000,"cost_points":"many"}]}"#)
                .expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn a_swallowed_history_failure_keeps_the_recorded_balance_and_says_so() {
        // The recorded degraded case: balance 1500, the history answering HTTP 500.
        // CodexBar renders the balance row alone; this port adds the visible marker, so
        // the swallow costs a row rather than silence.
        let degraded = snapshot(Some(1500.0), &History::failed(Vec::new()), now());
        assert!(degraded.windows.is_empty());
        assert_eq!(
            degraded.details,
            vec![DetailSection {
                title: "Points".to_owned(),
                rows: vec![
                    DetailRow {
                        label: "Current balance".to_owned(),
                        value: "1,500 points".to_owned(),
                    },
                    DetailRow {
                        label: "History".to_owned(),
                        value: "Unavailable right now".to_owned(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn an_empty_history_and_the_recorded_balance_render_one_row() {
        let recorded = snapshot(Some(1500.0), &History::complete(Vec::new()), now());
        assert_eq!(
            recorded.details,
            vec![DetailSection {
                title: "Points".to_owned(),
                rows: vec![DetailRow {
                    label: "Current balance".to_owned(),
                    value: "1,500 points".to_owned(),
                }],
            }]
        );
    }

    #[test]
    fn an_absent_balance_and_an_empty_history_render_nothing() {
        let empty = snapshot(None, &History::complete(Vec::new()), now());
        assert!(empty.windows.is_empty());
        assert!(empty.details.is_empty());
    }

    #[test]
    fn the_balance_request_carries_a_bearer_key() {
        let poe = Poe::new(Credential::new("poe-key")).expect("builds");
        let request = poe.balance_request().expect("builds");
        assert_eq!(
            request.url().as_str(),
            "https://api.poe.com/usage/current_balance"
        );
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer poe-key"
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
    fn the_history_request_pages_a_hundred_rows_at_a_time() {
        let poe = Poe::new(Credential::new("poe-key")).expect("builds");
        let first = poe.history_request(None).expect("builds");
        assert_eq!(
            first.url().as_str(),
            "https://api.poe.com/usage/points_history?limit=100"
        );
        let next = poe.history_request(Some("cursor-token")).expect("builds");
        assert_eq!(
            next.url().as_str(),
            "https://api.poe.com/usage/points_history?limit=100&starting_after=cursor-token"
        );
        assert_eq!(
            next.headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer poe-key"
        );
    }

    #[test]
    fn the_spec_publishes_the_hint_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Poe");
        assert!(SPEC.options.is_empty(), "Poe has nothing to choose");
        assert!(build(Credential::new("poe-key"), &Options::new()).is_ok());
    }

    #[test]
    fn a_poe_client_never_prints_its_credential() {
        let poe = Poe::new(Credential::new("sk-super-secret")).expect("builds");
        let rendered = format!("{poe:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
