//! OpenRouter.
//!
//! Ported from CodexBar's `Plugins/openrouter.js`; the recorded bodies in
//! `OpenRouterUsageStatsTests.swift` are the contract. Never seen answering: every
//! number in the tests below is a body CodexBar recorded.
//!
//! # The two requests
//!
//! `GET {base}/credits` — the account's money: `data.total_credits` and
//! `data.total_usage`, plain numbers, where a string is the recorded malformed case.
//! This request is the point of the fetch, and it alone can fail it.
//!
//! `GET {base}/key` — the key's own spend limit: `limit`, `limit_remaining`, `usage`,
//! `usage_daily/weekly/monthly`, the `limit_reset` window name (a string), and a
//! `rate_limit` object. In CodexBar this one is optional and every failure is degraded
//! to a row rather than thrown; that is kept, and the reason reaches the row. The
//! source's one-second cap on the request is not: it belongs to a menu-bar app that
//! cannot hang a refresh, while this is a background poll that can afford the shared
//! 30-second ceiling.
//!
//! # The reading
//!
//! A key with a limit is a fixed balance — spend against a stated budget — drawn as one
//! lengthless window keyed `balance`, because a budget has no length to key on and
//! never resets. Spend is the server-reported `limit_remaining` first, clamped to the
//! limit, so a negative remaining reads as an exhausted quota; then the usage matching
//! the declared reset window; then cumulative `usage`. A key without a limit is a row
//! saying so and no window.
//!
//! # The base URL
//!
//! `OPENROUTER_API_URL` in the source, `base_url` here, read through
//! [`keyed::base_url`] with the source's own default — `https://openrouter.ai/api/v1`,
//! version path included, because the recorded override carries the path too and the
//! endpoints append `/credits` and `/key` directly. The source's `X-Title` and
//! `HTTP-Referer` headers are dropped: they attribute a client to the provider's
//! leaderboard, and this port names itself through the shared user agent.

use super::{HandSpec, OptionSchema, Options, base_url, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "openrouter";

/// Name of the base-URL setting under `[provider.openrouter]`.
pub const BASE_URL: &str = "base_url";

/// The API root the two paths append to. The source's default, verbatim: the version
/// path is part of it, and an override without it would miss the API by one segment.
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "OpenRouter",
    credential_hint: "openrouter.ai/settings/keys → Create Key.",
    options: &[OptionSchema {
        name: BASE_URL,
        title: "API URL",
        description: Some(
            "The API root, version path included. Leave unset for openrouter.ai itself.",
        ),
        default: DEFAULT_BASE_URL,
        choices: &[],
        required: false,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The URLs
/// are resolved here, so a changed base URL takes effect on the next build.
fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(OpenRouter::new(credential, options)?))
}

/// One OpenRouter account: the key, and the two endpoints it unlocks.
pub struct OpenRouter {
    client: reqwest::Client,
    credential: Credential,
    credits_url: String,
    key_url: String,
}

impl OpenRouter {
    /// Builds a client. The base URL is resolved once, here, because a setting that
    /// changed the host would otherwise take effect only on the next daemon restart.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        let base = base_url(options, BASE_URL, DEFAULT_BASE_URL)?;
        Ok(Self {
            client: http::client()?,
            credential,
            credits_url: format!("{base}/credits"),
            key_url: format!("{base}/key"),
        })
    }

    /// The credits URL this instance polls.
    pub fn credits_url(&self) -> &str {
        &self.credits_url
    }

    /// The key URL this instance polls.
    pub fn key_url(&self) -> &str {
        &self.key_url
    }

    fn get(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The credits request, built but not sent, so the placement of the key is testable.
    fn credits_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(&self.credits_url)
    }

    /// The key request, likewise.
    fn key_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(&self.key_url)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let credits = parse_credits(&super::request(&self.client, self.credits_request()?).await?)?;
        let key = self.key_state().await;
        Ok(snapshot(&credits, &key, now))
    }

    /// The optional `/key` request. It never fails the fetch: a failure — transport,
    /// status or shape — degrades the API key section to one row saying why, exactly as
    /// the source's `try` degrades it, because the credits already in hand are the
    /// point of the poll.
    async fn key_state(&self) -> KeyState {
        let Ok(request) = self.key_request() else {
            return KeyState::Degraded("request failed".to_owned());
        };
        match super::request(&self.client, request).await {
            Ok(body) => match parse_key(&body) {
                Ok(Some(data)) => KeyState::Data(data),
                Ok(None) => KeyState::Degraded("no key data in the response".to_owned()),
                Err(_) => KeyState::Degraded("invalid response".to_owned()),
            },
            Err(error) => KeyState::Degraded(reason(error)),
        }
    }
}

impl fmt::Debug for OpenRouter {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenRouter")
            .field("id", &PROVIDER_ID)
            .field("credits_url", &self.credits_url)
            .finish_non_exhaustive()
    }
}

impl Provider for OpenRouter {
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

/// Why the optional key request did not produce key data, in the short form the row
/// carries. The source keeps its reason in a secondary line; the row here folds it in
/// after a middle dot.
fn reason(error: ProviderError) -> String {
    match error {
        ProviderError::Http { status } | ProviderError::Credential { status } => {
            format!("HTTP {status}")
        }
        ProviderError::RateLimited { .. } => "HTTP 429".to_owned(),
        ProviderError::Transport(_) => "request failed".to_owned(),
        other => {
            debug_assert!(
                matches!(other, ProviderError::Malformed(_)),
                "only statuses, transport faults and unreadable bodies degrade"
            );
            "invalid response".to_owned()
        }
    }
}

/// What the optional key request produced.
#[derive(Debug, Clone, PartialEq)]
enum KeyState {
    /// The quota fields, however few.
    Data(KeyData),
    /// The request failed or its body was not key data; the reason reaches the row.
    Degraded(String),
}

/// The account's money.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Credits {
    total_credits: f64,
    total_usage: f64,
}

impl Credits {
    /// What is left: total added minus total spent, floored at zero as the source floors
    /// it.
    fn balance(self) -> f64 {
        (self.total_credits - self.total_usage).max(0.0)
    }
}

/// Reads the credits body. Pure, and strict: this request is the point of the fetch, so
/// an unreadable body fails it — the recorded cases are a string where `total_credits`
/// belongs and a body that is not JSON at all.
fn parse_credits(body: &str) -> Result<Credits, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|_| ProviderError::malformed("the OpenRouter response was not valid JSON"))?;
    let data = root
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::malformed("the OpenRouter credits data must be an object"))?;
    let read = |name: &str| -> Result<f64, ProviderError> {
        data.get(name)
            .and_then(Value::as_number)
            .and_then(serde_json::Number::as_f64)
            .filter(|number| number.is_finite())
            .ok_or_else(|| ProviderError::malformed(format!("{name} must be a finite number")))
    };
    Ok(Credits {
        total_credits: read("total_credits")?,
        total_usage: read("total_usage")?,
    })
}

/// The key's own spend limit, every field optional.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
struct KeyData {
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    limit_remaining: Option<f64>,
    #[serde(default)]
    usage: Option<f64>,
    #[serde(default)]
    usage_daily: Option<f64>,
    #[serde(default)]
    usage_weekly: Option<f64>,
    #[serde(default)]
    usage_monthly: Option<f64>,
    /// The reset window's name — `daily`, `weekly`, `monthly` — as a string on the wire.
    #[serde(default)]
    limit_reset: Option<String>,
    #[serde(default)]
    rate_limit: Option<RateLimit>,
}

/// The requests-per-interval pair the key reports.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct RateLimit {
    requests: i64,
    interval: String,
}

/// Reads the key body. Pure.
///
/// `Ok(None)` — no `data` object — is the source's "the response was unavailable", not
/// an error; a `data` object with a field that fails validation is its "invalid". Both
/// degrade the fetch rather than fail it, which is why this parser reports them
/// separately instead of refusing.
fn parse_key(body: &str) -> Result<Option<KeyData>, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an OpenRouter key response: {e}")))?;
    let Some(data) = root.get("data").filter(|data| data.is_object()) else {
        return Ok(None);
    };
    serde_json::from_value(data.clone()).map(Some).map_err(|e| {
        ProviderError::malformed(format!("the OpenRouter key data is not readable: {e}"))
    })
}

/// The measured budget behind the window and its rows.
#[derive(Debug, Clone, Copy)]
struct Budget {
    limit: f64,
    used: f64,
}

impl Budget {
    /// The measured budget: a positive limit and a computable spend. A key with neither —
    /// no limit at all, or a limit with no spend any field describes — has rows but no
    /// bar, exactly as the source suppresses its primary meter.
    fn of(data: &KeyData) -> Option<Self> {
        let limit = data.limit.filter(|limit| *limit > 0.0)?;
        let used = used_for_quota(data, limit)?.filter(|used| *used >= 0.0 && used.is_finite())?;
        Some(Self { limit, used })
    }

    /// The spend limit as a window: a fixed balance, used over limit. Lengthless and
    /// resetless — the budget does not roll — so `named("balance")`, because a balance
    /// has no length to key on.
    fn window(&self) -> Window {
        Window {
            key: WindowKey::named("balance"),
            title: "API key budget".to_owned(),
            subtitle: Some(format!(
                "{} of {} used · {} left",
                usd(self.used),
                usd(self.limit),
                usd((self.limit - self.used).max(0.0))
            )),
            used_percent: (self.used / self.limit * 100.0).clamp(0.0, 100.0),
            resets_at: None,
            length: None,
        }
    }
}

/// How much of the budget is spent, by the source's own precedence: the server-reported
/// remaining first, clamped to the limit so a negative remaining reads as exhausted;
/// then the usage matching the declared reset window; then cumulative usage.
fn used_for_quota(data: &KeyData, limit: f64) -> Option<Option<f64>> {
    if let Some(remaining) = data.limit_remaining {
        return Some(Some(limit - remaining.clamp(0.0, limit)));
    }
    let window_usage = match data.limit_reset.as_deref() {
        Some("daily") => data.usage_daily,
        Some("weekly") => data.usage_weekly,
        Some("monthly") => data.usage_monthly,
        _ => None,
    };
    Some(window_usage.or(data.usage))
}

/// Assembles the snapshot. Pure, so every recorded key body is reachable from a test.
fn snapshot(credits: &Credits, key: &KeyState, captured_at: Timestamp) -> Snapshot {
    let budget = match key {
        KeyState::Data(data) => Budget::of(data),
        KeyState::Degraded(_) => None,
    };
    let mut windows = Vec::new();
    if let Some(budget) = &budget {
        windows.push(budget.window());
    }
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: vec![credits_details(credits), key_details(key, budget.as_ref())],
    }
}

/// The account's money, in the source's own three rows.
fn credits_details(credits: &Credits) -> DetailSection {
    DetailSection {
        title: "Credits".to_owned(),
        rows: vec![
            labeled("Remaining", usd(credits.balance())),
            labeled("Used", usd(credits.total_usage)),
            labeled("Total added", usd(credits.total_credits)),
        ],
    }
}

/// The key's own limits, or the one row that says they are missing.
fn key_details(key: &KeyState, budget: Option<&Budget>) -> DetailSection {
    let rows = match key {
        KeyState::Data(data) => key_rows(data, budget),
        KeyState::Degraded(reason) => vec![labeled(
            "API key budget",
            format!("Unavailable right now · {reason}"),
        )],
    };
    DetailSection {
        title: "API key".to_owned(),
        rows,
    }
}

fn key_rows(data: &KeyData, budget: Option<&Budget>) -> Vec<DetailRow> {
    let mut rows = Vec::new();
    if let Some(limit) = data.limit.filter(|limit| *limit > 0.0) {
        rows.push(labeled("API key budget", usd(limit)));
        if let Some(budget) = budget {
            rows.push(labeled(
                "API key remaining",
                usd((budget.limit - budget.used).max(0.0)),
            ));
        }
        if let Some(usage) = data.usage {
            rows.push(labeled("API key used", usd(usage)));
        }
    } else {
        rows.push(labeled("API key budget", "No limit configured"));
    }
    if let Some(reset) = data
        .limit_reset
        .as_deref()
        .map(str::trim)
        .filter(|reset| !reset.is_empty())
    {
        rows.push(labeled("Reset window", reset));
    }
    for (label, value) in [
        ("Today", data.usage_daily),
        ("This week", data.usage_weekly),
        ("This month", data.usage_monthly),
    ] {
        if let Some(value) = value {
            rows.push(labeled(label, usd(value)));
        }
    }
    if let Some(rate) = &data.rate_limit {
        rows.push(labeled(
            "Rate limit",
            format!("{} requests / {}", rate.requests, rate.interval),
        ));
    }
    rows
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

/// The source's currency rendering: dollars, two fraction digits, negatives clamped to
/// zero — a negative figure is a refund in the pipeline, not money to show.
fn usd(value: f64) -> String {
    format!("${:.2}", value.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_787_000_000).expect("plausible")
    }

    // Recorded bodies, verbatim from OpenRouterUsageStatsTests.swift.
    const CREDITS: &str = r#"{"data":{"total_credits":100,"total_usage":40}}"#;
    const CREDITS_MALFORMED: &str = r#"{"data":{"total_credits":"many","total_usage":40}}"#;
    const KEY_LIMIT_USAGE: &str = r#"{"data":{"limit":20,"usage":5}}"#;
    const KEY_EMPTY: &str = r#"{"data":{}}"#;
    const KEY_NO_DATA: &str = "{}";
    const KEY_MALFORMED: &str = r#"{"data":{"limit":"twenty"}}"#;
    const KEY_REMAINING: &str = r#"{"data":{
          "limit":500,
          "limit_remaining":454.542594979,
          "limit_reset":"monthly",
          "usage":433.286754736,
          "usage_daily":3.404645509,
          "usage_weekly":3.404645509,
          "usage_monthly":45.457405021
        }}"#;
    const KEY_MONTHLY_NO_REMAINING: &str = r#"{"data":{
          "limit":500,
          "limit_reset":"monthly",
          "usage":433.286754736,
          "usage_monthly":45.457405021
        }}"#;
    const KEY_MONTHLY_ONLY: &str = r#"{"data":{
          "limit":500,
          "limit_reset":"monthly",
          "usage_monthly":45.457405021
        }}"#;
    const KEY_NEGATIVE_REMAINING: &str = r#"{"data":{
          "limit":500,
          "limit_remaining":-5,
          "limit_reset":"monthly",
          "usage":433.286754736,
          "usage_monthly":45.457405021
        }}"#;
    const KEY_PERIODS: &str = r#"{"data":{
          "limit":20,
          "usage":0.5,
          "usage_daily":0.12,
          "usage_weekly":0.74,
          "usage_monthly":4.56,
          "rate_limit":{"requests":120,"interval":"10s"}
        }}"#;

    fn row_of<'a>(snapshot: &'a Snapshot, label: &str) -> &'a DetailRow {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no {label} row in {snapshot:?}"))
    }

    fn key_data(body: &str) -> KeyData {
        parse_key(body).expect("parses").expect("key data")
    }

    fn snapshot_with(key: &KeyState) -> Snapshot {
        snapshot(&parse_credits(CREDITS).expect("parses"), key, now())
    }

    #[test]
    fn the_recorded_credits_body_reads_its_two_numbers() {
        let credits = parse_credits(CREDITS).expect("parses");
        assert_eq!(credits.total_credits, 100.0);
        assert_eq!(credits.total_usage, 40.0);
        assert_eq!(credits.balance(), 60.0);
    }

    #[test]
    fn credits_that_are_not_numbers_fail_the_fetch() {
        // `total_credits` as a string is the recorded malformed case; `not-json` is the
        // recorded invalid-JSON case; `{"partial":` is the procedure's canonical body.
        for body in [CREDITS_MALFORMED, "not-json", "{\"partial\":"] {
            let error = parse_credits(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error:?}"
            );
        }
        let named = parse_credits(CREDITS_MALFORMED).expect_err("names the field");
        assert!(format!("{named}").contains("total_credits"), "{named}");
    }

    #[test]
    fn the_recorded_quota_fixture_reads_limit_and_usage() {
        let data = key_data(KEY_LIMIT_USAGE);
        assert_eq!(data.limit, Some(20.0));
        assert_eq!(data.usage, Some(5.0));
        assert_eq!(data.limit_remaining, None);
    }

    #[test]
    fn a_key_body_without_data_is_no_key_data_not_an_error() {
        assert_eq!(parse_key(KEY_NO_DATA).expect("parses"), None);
    }

    #[test]
    fn a_key_field_that_is_not_a_number_is_malformed() {
        for body in [KEY_MALFORMED, "{\"partial\":"] {
            assert!(
                matches!(parse_key(body), Err(ProviderError::Malformed(_))),
                "{body}"
            );
        }
    }

    #[test]
    fn fields_this_parser_does_not_know_are_skipped() {
        // The unknown-kind rule: an unfamiliar field rides along without breaking the
        // recognised ones, because that is a field that did not exist when this was
        // written, not a failure to read one that did.
        let data = key_data(r#"{"data":{"limit":20,"usage":5,"future":{"whatever":"it says"}}}"#);
        assert_eq!(data.limit, Some(20.0));
        assert_eq!(data.usage, Some(5.0));
    }

    #[test]
    fn a_key_with_a_limit_is_one_balance_window() {
        let snapshot = snapshot_with(&KeyState::Data(key_data(KEY_LIMIT_USAGE)));
        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(window.key.as_str(), "balance");
        assert_eq!(window.title, "API key budget");
        assert_eq!(window.used_percent, 25.0);
        assert_eq!(window.length, None);
        assert_eq!(window.resets_at, None);
        assert_eq!(
            window.subtitle.as_deref(),
            Some("$5.00 of $20.00 used · $15.00 left")
        );
    }

    #[test]
    fn the_credits_become_three_rows() {
        let snapshot = snapshot_with(&KeyState::Data(key_data(KEY_LIMIT_USAGE)));
        assert_eq!(row_of(&snapshot, "Remaining").value, "$60.00");
        assert_eq!(row_of(&snapshot, "Used").value, "$40.00");
        assert_eq!(row_of(&snapshot, "Total added").value, "$100.00");
        assert_eq!(row_of(&snapshot, "API key budget").value, "$20.00");
        assert_eq!(row_of(&snapshot, "API key remaining").value, "$15.00");
        assert_eq!(row_of(&snapshot, "API key used").value, "$5.00");
    }

    #[test]
    fn a_key_without_a_limit_has_no_window_and_says_so() {
        let snapshot = snapshot_with(&KeyState::Data(key_data(KEY_EMPTY)));
        assert!(snapshot.windows.is_empty());
        assert_eq!(
            row_of(&snapshot, "API key budget").value,
            "No limit configured"
        );
    }

    #[test]
    fn the_server_reported_remaining_drives_the_window() {
        let snapshot = snapshot_with(&KeyState::Data(key_data(KEY_REMAINING)));
        let window = &snapshot.windows[0];
        assert!(
            (window.used_percent - 9.0914810042).abs() < 1e-9,
            "{}",
            window.used_percent
        );
        assert_eq!(row_of(&snapshot, "API key remaining").value, "$454.54");
        assert_eq!(row_of(&snapshot, "Reset window").value, "monthly");
    }

    #[test]
    fn a_missing_remaining_falls_back_to_the_reset_window_usage() {
        for body in [KEY_MONTHLY_NO_REMAINING, KEY_MONTHLY_ONLY] {
            let snapshot = snapshot_with(&KeyState::Data(key_data(body)));
            assert_eq!(snapshot.windows.len(), 1, "{body}");
            assert!(
                (snapshot.windows[0].used_percent - 9.0914810042).abs() < 1e-9,
                "{body}: {}",
                snapshot.windows[0].used_percent
            );
            assert_eq!(row_of(&snapshot, "API key remaining").value, "$454.54");
        }
    }

    #[test]
    fn a_negative_remaining_is_an_exhausted_quota() {
        let snapshot = snapshot_with(&KeyState::Data(key_data(KEY_NEGATIVE_REMAINING)));
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(row_of(&snapshot, "API key remaining").value, "$0.00");
    }

    #[test]
    fn the_periods_and_rate_limit_become_rows() {
        let snapshot = snapshot_with(&KeyState::Data(key_data(KEY_PERIODS)));
        assert_eq!(row_of(&snapshot, "Today").value, "$0.12");
        assert_eq!(row_of(&snapshot, "This week").value, "$0.74");
        assert_eq!(row_of(&snapshot, "This month").value, "$4.56");
        assert_eq!(row_of(&snapshot, "Rate limit").value, "120 requests / 10s");
        assert_eq!(row_of(&snapshot, "API key remaining").value, "$19.50");
    }

    #[test]
    fn a_degraded_key_request_leaves_the_credits_and_says_why() {
        let snapshot = snapshot_with(&KeyState::Degraded("HTTP 500".to_owned()));
        assert!(snapshot.windows.is_empty());
        assert_eq!(row_of(&snapshot, "Remaining").value, "$60.00");
        assert_eq!(
            row_of(&snapshot, "API key budget").value,
            "Unavailable right now · HTTP 500"
        );
    }

    #[test]
    fn the_base_url_defaults_to_the_api_root_and_follows_the_setting() {
        let unset = Options::new();
        let provider = OpenRouter::new(Credential::new("sk-or"), &unset).expect("builds");
        assert_eq!(
            provider.credits_url(),
            "https://openrouter.ai/api/v1/credits"
        );
        assert_eq!(provider.key_url(), "https://openrouter.ai/api/v1/key");

        // The recorded override, verbatim from the request-recording test.
        let override_url: Options = [(
            BASE_URL.to_owned(),
            "https://openrouter.test/api/v1".to_owned(),
        )]
        .into_iter()
        .collect();
        let provider = OpenRouter::new(Credential::new("sk-or"), &override_url).expect("builds");
        assert_eq!(
            provider.credits_url(),
            "https://openrouter.test/api/v1/credits"
        );

        let trailing_slash: Options = [(
            BASE_URL.to_owned(),
            "https://openrouter.test/api/v1/".to_owned(),
        )]
        .into_iter()
        .collect();
        let provider = OpenRouter::new(Credential::new("sk-or"), &trailing_slash).expect("builds");
        assert_eq!(
            provider.credits_url(),
            "https://openrouter.test/api/v1/credits",
            "a trailing slash is trimmed, not doubled up"
        );

        let plain_http: Options = [(BASE_URL.to_owned(), "http://openrouter.test".to_owned())]
            .into_iter()
            .collect();
        assert!(
            OpenRouter::new(Credential::new("sk-or"), &plain_http).is_err(),
            "a key over plain HTTP is a key given away"
        );
    }

    #[test]
    fn both_requests_carry_the_bearer_key() {
        let provider =
            OpenRouter::new(Credential::new("sk-or-v1"), &Options::new()).expect("builds");
        for request in [
            provider.credits_request().expect("builds"),
            provider.key_request().expect("builds"),
        ] {
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer sk-or-v1"
            );
        }
    }

    #[test]
    fn the_spec_publishes_the_option_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "OpenRouter");
        assert_eq!(SPEC.options.len(), 1);
        assert!(build(Credential::new("sk-or"), &Options::new()).is_ok());
    }

    #[test]
    fn an_openrouter_client_never_prints_its_credential() {
        let provider =
            OpenRouter::new(Credential::new("sk-super-secret"), &Options::new()).expect("builds");
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
