//! Wayfinder.
//!
//! Ported from CodexBar's `Wayfinder/WayfinderUsageFetcher.swift`. Never seen answering:
//! every number in the tests is a body CodexBar recorded from a gateway of its own.
//!
//! # The provider with no credential
//!
//! Wayfinder is a router that runs on this machine — `wayfinder-router serve`, listening on
//! `http://127.0.0.1:8088` by default — and it answers anyone who can reach it. There is no
//! key to paste and nobody to sign in to, so this is the first [`HandSpec`] to declare
//! [`CredentialKind::None`]: the account is its gateway URL and nothing else. The blank
//! credential the registry hands `build` is ignored, which is the whole of what "no
//! credential" means here.
//!
//! # Three requests
//!
//! `GET {base}/healthz` says whether the gateway is up and whether any route is missing its
//! key, `GET {base}/router/models` counts the configured routes and says whether it is in
//! dry-run, and `GET {base}/v1/savings?period=30d` is the month's traffic. All three are
//! required: each contributes a line, and a gateway that cannot answer one of them is not
//! reporting.
//!
//! CodexBar reads a fourth, `GET {base}/metrics`, for a Prometheus histogram it renders as
//! an average decision latency, and it is allowed to fail. It is not ported: the plan for
//! this port is three endpoints, and a routing latency in milliseconds is a profiler's
//! number rather than a usage one.
//!
//! # This card has no bars, and cannot
//!
//! A gateway has no quota. Nothing in any of the three payloads says what anything is out
//! of — the savings figure is a comparison against the most expensive route, not a share of
//! an allowance — so there is no window to draw and the card is detail rows only. It
//! renders empty and sorts last, as Moonshot's and DeepSeek's do. Inventing a limit to draw
//! a bar against would be worse.
//!
//! # What the payloads do not tell you
//!
//! **Route names are the user's own.** `by_route` is keyed by whatever the endpoints were
//! called in the Wayfinder config; nothing in the JSON says which of them is "local" and
//! which is "cloud", and the summary never claims otherwise. The routes are ordered by
//! traffic — requests descending, then name — because their order in the response is a map
//! iteration order and means nothing. CodexBar shows the busiest five; so does this.
//!
//! **Savings are only money when the gateway says they are priced.** With `priced: false`
//! the numbers are in relative units, and rendering them as dollars would invent a currency
//! the gateway disclaimed. The percentage is shown either way.
//!
//! **Everything the source requires is required here.** Swift's decoder refuses a payload
//! with a missing or wrong-typed field, and so does this: a body that cannot be read is
//! [`ProviderError::Malformed`] naming the endpoint, never a zero. Fields the source parses
//! but never shows — `realized`, `baseline`, `tokens` — are still parsed, so that a gateway
//! answering rubbish is caught rather than half-read.
//!
//! # Redirects are refused
//!
//! This client follows none. CodexBar checks that the response came back from the origin it
//! asked, because a gateway URL that redirects elsewhere would have its answer trusted; with
//! redirects switched off the same attempt is a [`ProviderError::Http`] carrying the 3xx.
//! There is no credential to leak here, but there is still a card that would otherwise show
//! a stranger's numbers.

use super::{HandSpec, OptionSchema, Options, base_url, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "wayfinder";

/// Name of the gateway-URL setting under `[provider.wayfinder]`.
pub const BASE_URL: &str = "base_url";

/// Where `wayfinder-router serve` listens unless it was told otherwise. The source's own
/// default, and the reason plain HTTP has to be allowed here at all.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8088";

/// The savings window asked for. The source's `savingsPeriod`, and the reason the rows say
/// nothing about a period: it is always this one.
pub const PERIOD: &str = "30d";

/// How many routes the summary names, busiest first. The source's `prefix(5)`.
const ROUTES_SHOWN: usize = 5;

/// Wayfinder as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Wayfinder",
    // Nothing is stored and nothing is asked for: the gateway answers anyone who can reach
    // it. See the module doc.
    credential: CredentialKind::None,
    credential_hint: "",
    options: &[OptionSchema {
        name: BASE_URL,
        title: "Gateway URL",
        description: Some(
            "Where `wayfinder-router serve` is listening. HTTPS, or HTTP on loopback.",
        ),
        default: DEFAULT_BASE_URL,
        choices: &[],
        required: false,
    }],
    build,
};

/// Builds a pollable client from the account's settings. The credential is blank and
/// unused; the gateway URL is resolved here so a value the shared reader refuses is a
/// [`ProviderError::Local`] naming the setting rather than a malformed request later.
fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Wayfinder::new(options)?))
}

/// One Wayfinder account, which is one gateway.
pub struct Wayfinder {
    client: reqwest::Client,
    base: String,
}

impl Wayfinder {
    /// Builds a client. The URL is resolved once, here, because a setting that changed the
    /// gateway would otherwise take effect only on the next daemon restart.
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            client: client()?,
            base: base_url(options, BASE_URL, DEFAULT_BASE_URL)?,
        })
    }

    /// The gateway this instance polls.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// A GET, built but not sent, so that what is asked for is testable without a server.
    pub fn request(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn get(&self, url: &str) -> Result<String, ProviderError> {
        super::request(PROVIDER_ID, &self.client, self.request(url)?).await
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let health: Health = parse(&self.get(&health_url(&self.base)).await?, "/healthz")?;
        let models: Models = parse(&self.get(&models_url(&self.base)).await?, "/router/models")?;
        let savings: Savings = parse(&self.get(&savings_url(&self.base)).await?, "/v1/savings")?;
        Ok(snapshot(&health, &models, &savings, Timestamp::now()))
    }
}

impl fmt::Debug for Wayfinder {
    /// Written by hand for the same reason every other provider's is, even though this one
    /// holds no credential: the shape of these impls should not vary with whether there is
    /// a secret to keep out of a log this week.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wayfinder")
            .field("id", &PROVIDER_ID)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Provider for Wayfinder {
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

/// A client that follows no redirect. See the module doc.
fn client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .user_agent(tidemark_types::user_agent())
        .timeout(http::REQUEST_TIMEOUT)
        .connect_timeout(http::CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(ProviderError::Client)
}

/// The health URL of a gateway.
pub fn health_url(base: &str) -> String {
    format!("{base}/healthz")
}

/// The configured-routes URL of a gateway.
pub fn models_url(base: &str) -> String {
    format!("{base}/router/models")
}

/// The savings URL of a gateway, for the one period this provider asks about.
pub fn savings_url(base: &str) -> String {
    format!("{base}/v1/savings?period={PERIOD}")
}

/// Reads one payload, or fails the whole fetch naming the endpoint that answered it.
///
/// Every field below is required exactly where the source requires it, so a missing or
/// wrong-typed one is [`ProviderError::Malformed`] rather than a silent zero.
fn parse<T: serde::de::DeserializeOwned>(body: &str, endpoint: &str) -> Result<T, ProviderError> {
    serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("{endpoint}: {error}")))
}

/// `GET /healthz`.
#[derive(Debug, Deserialize)]
struct Health {
    /// The gateway's own word for its condition: `ok`, `degraded`, or whatever else it
    /// grows. Shown as it arrives rather than mapped, so a state this build has not been
    /// taught still reaches the reader.
    status: String,
    /// Whether the gateway is refusing to leave the machine.
    offline: bool,
    /// The routes whose API key is not set. Absent and null both mean none.
    #[serde(default)]
    missing_keys: Option<Vec<String>>,
}

/// `GET /router/models`.
#[derive(Debug, Deserialize)]
struct Models {
    /// The configured routes. Only the count is shown, but each entry is still required to
    /// carry its name, so a garbled list fails rather than being counted blind.
    models: Vec<Model>,
    /// Whether the gateway is scoring prompts without calling anything.
    dry_run: bool,
}

/// One configured route.
#[derive(Debug, Deserialize)]
struct Model {
    /// The user's own name for it.
    #[allow(dead_code)]
    name: String,
}

/// `GET /v1/savings`.
#[derive(Debug, Deserialize)]
struct Savings {
    /// Whether the figures are money. When false they are in relative units and are never
    /// rendered as dollars.
    priced: bool,
    /// Requests routed in the period.
    requests: i64,
    /// What the routed traffic cost.
    #[allow(dead_code)]
    realized: f64,
    /// What it would have cost on the most expensive route.
    #[allow(dead_code)]
    baseline: f64,
    /// The difference, in whatever unit `priced` says.
    saved: f64,
    /// That difference as a percentage of `baseline`.
    saved_pct: f64,
    /// Tokens routed in the period.
    #[allow(dead_code)]
    tokens: i64,
    /// Per-route traffic, keyed by the user's own route names.
    by_route: BTreeMap<String, Route>,
}

/// One route's share of the period.
#[derive(Debug, Deserialize)]
struct Route {
    /// Requests this route took.
    requests: i64,
    /// What routing to it saved.
    #[allow(dead_code)]
    saved: f64,
    /// Tokens it carried.
    #[allow(dead_code)]
    tokens: i64,
}

/// `1 model` or `4 models`.
fn model_count_label(count: usize) -> String {
    if count == 1 {
        "1 model".to_owned()
    } else {
        format!("{count} models")
    }
}

/// What this gateway is, in one phrase: the plan line of a provider that has no plan.
///
/// Offline and dry-run come first because either makes the rest of the reading conditional;
/// a degraded gateway says how many routes are missing their key, since "degraded" alone
/// gives the reader nothing to act on.
fn status_label(health: &Health, models: &Models) -> String {
    if health.offline {
        return "Offline mode".to_owned();
    }
    if models.dry_run {
        return "Dry run".to_owned();
    }
    if health.status != "degraded" {
        return "Local gateway".to_owned();
    }
    match health.missing_keys.as_deref().unwrap_or_default() {
        [] => "Degraded".to_owned(),
        [_] => "Degraded — 1 key missing".to_owned(),
        many => format!("Degraded — {} keys missing", many.len()),
    }
}

/// The gateway's own status word, the route count, and whichever of offline and dry-run
/// apply. All of it, unlike [`status_label`], which reports only the first that does.
fn gateway_summary(health: &Health, models: &Models) -> String {
    let mut summary = format!(
        "{} · {}",
        health.status,
        model_count_label(models.models.len())
    );
    if health.offline {
        summary.push_str(" · offline");
    }
    if models.dry_run {
        summary.push_str(" · dry run");
    }
    summary
}

/// The busiest routes and what each took, or `None` before the gateway has routed anything.
///
/// Ordered by requests and then by name: a map's own order is not a signal, and neither is
/// the order the routes were configured in.
fn routed_summary(savings: &Savings) -> Option<String> {
    if savings.requests <= 0 {
        return None;
    }
    let mut routes: Vec<(&str, i64)> = savings
        .by_route
        .iter()
        .map(|(name, route)| (name.as_str(), route.requests))
        .collect();
    routes.sort_by(|(one_name, one), (other_name, other)| {
        other.cmp(one).then_with(|| one_name.cmp(other_name))
    });
    let summary = routes
        .iter()
        .take(ROUTES_SHOWN)
        .map(|(name, requests)| format!("{name}: {}", grouped(*requests)))
        .collect::<Vec<_>>()
        .join(" · ");
    (!summary.is_empty()).then_some(summary)
}

/// What routing saved, in money only where the gateway said the figures are money.
fn saved_summary(savings: &Savings) -> Option<String> {
    if savings.requests <= 0 || savings.saved <= 0.0 {
        return None;
    }
    let percent = format!("{}% vs highest-cost route", percent_text(savings.saved_pct));
    if !savings.priced {
        return Some(percent);
    }
    // A gateway routing cheap models saves fractions of a cent for weeks. `$0.00` would
    // read as "nothing saved"; the source says so explicitly instead.
    let amount = if savings.saved < 0.01 {
        "<$0.01".to_owned()
    } else {
        usd(savings.saved)
    };
    Some(format!("{amount} · {percent}"))
}

/// The card.
fn snapshot(
    health: &Health,
    models: &Models,
    savings: &Savings,
    captured_at: Timestamp,
) -> Snapshot {
    let mut rows = vec![DetailRow {
        label: "Gateway".to_owned(),
        value: gateway_summary(health, models),
    }];
    if let Some(routed) = routed_summary(savings) {
        rows.push(DetailRow {
            label: "Routed".to_owned(),
            value: routed,
        });
    }
    if let Some(saved) = saved_summary(savings) {
        rows.push(DetailRow {
            label: "Saved".to_owned(),
            value: saved,
        });
    }

    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        // A gateway has no quota. See the module doc.
        windows: Vec::new(),
        details: vec![
            DetailSection {
                title: DetailSection::PLAN.to_owned(),
                rows: vec![DetailRow {
                    label: "Status".to_owned(),
                    value: status_label(health, models),
                }],
            },
            DetailSection {
                title: "Usage".to_owned(),
                rows,
            },
        ],
    }
}

/// A percentage in the source's own spelling: no decimal where there is nothing after it.
fn percent_text(value: f64) -> String {
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// A count with its thousands grouped (`1,028`).
fn grouped(value: i64) -> String {
    let rendered = value.to_string();
    let bytes = rendered.as_bytes();
    let mut grouped = String::with_capacity(rendered.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    grouped
}

/// An amount of money in the source's own spelling: `$`, two decimals, thousands grouped.
fn usd(value: f64) -> String {
    let rendered = format!("{value:.2}");
    let (whole, fraction) = rendered
        .split_once('.')
        .unwrap_or((rendered.as_str(), "00"));
    format!(
        "${}.{fraction}",
        grouped(whole.parse::<i64>().unwrap_or_default())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar from a locally running gateway (`wayfinder-router serve`,
    /// two-tier priced config) after routing real traffic —
    /// `TestsLinux/WayfinderProviderLinuxTests.swift`, whose own test asserts a status of
    /// `ok`, two models, 14 requests, 1028 tokens, `0.005694` saved and `61.5` percent.
    const HEALTH_OK: &str = r#"{"status":"ok","models":["cloud","local"],"offline":false}"#;

    /// The same gateway with one route's key unset. Its test asserts
    /// `Degraded — 1 key missing`.
    const HEALTH_DEGRADED: &str = r#"
    {"status":"degraded","models":["cloud","local"],"offline":false,"missing_keys":["cloud"]}"#;

    const MODELS: &str = r#"
    {"models":[{"name":"local","endpoint":"http://127.0.0.1:9101/v1","model":"stand-in-small",
    "api_key_env":null,"key_ok":true},{"name":"cloud","endpoint":"http://127.0.0.1:9102/v1",
    "model":"stand-in-large","api_key_env":"RIG_CLOUD_KEY","key_ok":true}],"dry_run":false}"#;

    const SAVINGS_30D: &str = r#"
    {"period_days":30,"unit":"usd","priced":true,"requests":14,"estimated_requests":0,
    "tokens":1028,"realized":0.003558,"baseline":0.009252,"saved":0.005694,"saved_pct":61.5,
    "by_route":{"cloud":{"requests":4,"realized":0.003294,"baseline":0.003294,"saved":0.0,
    "tokens":366},"local":{"requests":10,"realized":0.000264,"baseline":0.005958,
    "saved":0.005694,"tokens":662}},"by_key":{},"price_table_version":"a3db80fd9a78"}"#;

    /// A gateway that has routed nothing yet. Its test asserts neither summary is drawn.
    const SAVINGS_ZEROS: &str = r#"
    {"period_days":30,"unit":"usd","priced":true,"requests":0,"estimated_requests":0,
    "tokens":0,"realized":0.0,"baseline":0.0,"saved":0.0,"saved_pct":0.0,"by_route":{},
    "by_key":{},"price_table_version":"a3db80fd9a78"}"#;

    /// A gateway with no price table. Its test asserts `40% vs highest-cost route`.
    const SAVINGS_UNPRICED: &str = r#"
    {"period_days":30,"unit":"relative","priced":false,"requests":5,"estimated_requests":0,
    "tokens":420,"realized":1.8,"baseline":3.0,"saved":1.2,"saved_pct":40.0,
    "by_route":{"local":{"requests":4,"realized":0.8,"baseline":2.0,"saved":1.2,"tokens":320},
    "cloud":{"requests":1,"realized":1.0,"baseline":1.0,"saved":0.0,"tokens":100}},
    "by_key":{},"price_table_version":"a3db80fd9a78"}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn read(health: &str, models: &str, savings: &str) -> Snapshot {
        snapshot(
            &parse::<Health>(health, "/healthz").expect("health reads"),
            &parse::<Models>(models, "/router/models").expect("models read"),
            &parse::<Savings>(savings, "/v1/savings").expect("savings read"),
            at(1_800_008_430),
        )
    }

    fn row<'a>(snapshot: &'a Snapshot, section: &str, label: &str) -> Option<&'a str> {
        snapshot
            .details
            .iter()
            .find(|found| found.title == section)?
            .rows
            .iter()
            .find(|found| found.label == label)
            .map(|found| found.value.as_str())
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_live_gateway_fixture_matches_codexbars_golden() {
        let snapshot = read(HEALTH_OK, MODELS, SAVINGS_30D);

        assert_eq!(row(&snapshot, "Plan", "Status"), Some("Local gateway"));
        assert_eq!(row(&snapshot, "Usage", "Gateway"), Some("ok · 2 models"));
        assert_eq!(
            row(&snapshot, "Usage", "Routed"),
            Some("local: 10 · cloud: 4")
        );
        assert_eq!(
            row(&snapshot, "Usage", "Saved"),
            Some("<$0.01 · 61.5% vs highest-cost route")
        );
        assert_eq!(snapshot.captured_at, at(1_800_008_430));
    }

    #[test]
    fn a_gateway_has_nothing_to_draw_a_bar_against() {
        // Not an oversight: see the module doc. The card is its detail rows.
        assert!(read(HEALTH_OK, MODELS, SAVINGS_30D).windows.is_empty());
    }

    #[test]
    fn a_degraded_gateway_counts_the_routes_missing_a_key() {
        assert_eq!(
            row(
                &read(HEALTH_DEGRADED, MODELS, SAVINGS_30D),
                "Plan",
                "Status"
            ),
            Some("Degraded — 1 key missing")
        );
        // Ours, not the source's: it has no fixture for a second missing key, but its
        // formatter pluralises and so does this.
        let two = r#"{"status":"degraded","offline":false,"missing_keys":["cloud","local"]}"#;
        assert_eq!(
            row(&read(two, MODELS, SAVINGS_30D), "Plan", "Status"),
            Some("Degraded — 2 keys missing")
        );
        // Degraded without saying why is still degraded, and still says so.
        let silent = r#"{"status":"degraded","offline":false}"#;
        assert_eq!(
            row(&read(silent, MODELS, SAVINGS_30D), "Plan", "Status"),
            Some("Degraded")
        );
        assert_eq!(
            row(&read(silent, MODELS, SAVINGS_30D), "Usage", "Gateway"),
            Some("degraded · 2 models"),
            "the gateway's own word for its condition is shown as it arrived"
        );
    }

    #[test]
    fn an_idle_gateway_reports_neither_routing_nor_savings() {
        let snapshot = read(HEALTH_OK, MODELS, SAVINGS_ZEROS);
        assert_eq!(row(&snapshot, "Usage", "Gateway"), Some("ok · 2 models"));
        assert_eq!(row(&snapshot, "Usage", "Routed"), None);
        assert_eq!(row(&snapshot, "Usage", "Saved"), None);
    }

    #[test]
    fn unpriced_savings_never_render_as_money() {
        let snapshot = read(HEALTH_OK, MODELS, SAVINGS_UNPRICED);
        let saved = row(&snapshot, "Usage", "Saved").expect("a saving is reported");
        assert_eq!(saved, "40% vs highest-cost route");
        assert!(
            !saved.contains('$'),
            "relative units are not a currency: {saved}"
        );
    }

    #[test]
    fn the_summary_uses_the_gateways_own_route_names() {
        // Route names are whatever the endpoints were called in the Wayfinder config;
        // nothing in the JSON marks one of them as the local tier.
        let named = r#"
        {"period_days":30,"unit":"usd","priced":true,"requests":14,"estimated_requests":0,
        "tokens":1028,"realized":0.003558,"baseline":0.009252,"saved":0.005694,
        "saved_pct":61.5,"by_route":{"groq-8b":{"requests":10,"realized":0.000264,
        "baseline":0.005958,"saved":0.005694,"tokens":662},"openai-o1":{"requests":4,
        "realized":0.003294,"baseline":0.003294,"saved":0.0,"tokens":366}},"by_key":{},
        "price_table_version":"a3db80fd9a78"}"#;
        let snapshot = read(HEALTH_OK, MODELS, named);
        let routed = row(&snapshot, "Usage", "Routed").expect("traffic is reported");

        assert_eq!(routed, "groq-8b: 10 · openai-o1: 4");
        assert!(!routed.contains("local"), "{routed}");
        assert!(!routed.contains("cloud"), "{routed}");
    }

    #[test]
    fn the_busiest_route_leads_whatever_order_the_gateway_reported() {
        // The heavier route is configured second in `/router/models` and sorts first in
        // `by_route`; neither position is a signal, and the summary derives nothing from
        // either.
        let reordered = r#"
        {"models":[{"name":"secondary-tier","endpoint":"http://127.0.0.1:9102/v1",
        "model":"stand-in-large","api_key_env":"RIG_CLOUD_KEY","key_ok":true},
        {"name":"primary-tier","endpoint":"http://127.0.0.1:9101/v1",
        "model":"stand-in-small","api_key_env":null,"key_ok":true}],"dry_run":false}"#;
        assert_eq!(
            row(&read(HEALTH_OK, reordered, SAVINGS_30D), "Usage", "Routed"),
            Some("local: 10 · cloud: 4")
        );
    }

    #[test]
    fn only_the_busiest_five_routes_are_named() {
        // The source shows five; a gateway with a route per model would otherwise write a
        // paragraph into a detail row. Ties are broken by name so the row is stable
        // between polls.
        let mut routes = Vec::new();
        for (index, name) in ["a", "b", "c", "d", "e", "f", "g"].iter().enumerate() {
            let requests = 7 - index;
            routes.push(format!(
                r#""{name}":{{"requests":{requests},"realized":0.0,"saved":0.0,"tokens":0}}"#
            ));
        }
        let many = format!(
            r#"{{"priced":true,"requests":28,"tokens":0,"realized":0.0,"baseline":0.0,
            "saved":0.0,"saved_pct":0.0,"by_route":{{{}}}}}"#,
            routes.join(",")
        );
        assert_eq!(
            row(&read(HEALTH_OK, MODELS, &many), "Usage", "Routed"),
            Some("a: 7 · b: 6 · c: 5 · d: 4 · e: 3")
        );
    }

    #[test]
    fn offline_and_dry_run_are_said_before_anything_else() {
        let offline = r#"{"status":"ok","offline":true,"missing_keys":[]}"#;
        let dry = r#"{"models":[{"name":"local"}],"dry_run":true}"#;

        let snapshot = read(offline, MODELS, SAVINGS_30D);
        assert_eq!(row(&snapshot, "Plan", "Status"), Some("Offline mode"));
        assert_eq!(
            row(&snapshot, "Usage", "Gateway"),
            Some("ok · 2 models · offline")
        );

        let snapshot = read(HEALTH_OK, dry, SAVINGS_30D);
        assert_eq!(row(&snapshot, "Plan", "Status"), Some("Dry run"));
        assert_eq!(
            row(&snapshot, "Usage", "Gateway"),
            Some("ok · 1 model · dry run"),
            "one route is one model, not one models"
        );
    }

    #[test]
    fn a_payload_that_cannot_be_read_fails_the_whole_fetch() {
        // The source's decoder refuses each of these, and a gateway answering rubbish is
        // not a gateway reporting nothing. Every failure names the endpoint that answered.
        for (body, endpoint) in [
            ("not json at all", "/healthz"),
            (r#"{"status":"ok"}"#, "/healthz"),
            (r#"{"status":"ok","offline":"no"}"#, "/healthz"),
        ] {
            let error = parse::<Health>(body, endpoint).expect_err("refused");
            assert!(
                matches!(error, ProviderError::Malformed(ref detail) if detail.contains(endpoint)),
                "{error:?}"
            );
        }
        assert!(matches!(
            parse::<Models>(r#"{"models":[{}],"dry_run":false}"#, "/router/models"),
            Err(ProviderError::Malformed(_))
        ));
        assert!(
            matches!(
                parse::<Savings>(
                    &SAVINGS_30D.replace("\"requests\":14", "\"requests\":\"14\""),
                    "/v1/savings"
                ),
                Err(ProviderError::Malformed(_))
            ),
            "a count that arrived as a string is not read as a count"
        );
    }

    #[test]
    fn fields_this_build_does_not_know_are_read_past() {
        // Everything the gateway reports beyond what is shown — `period_days`, `by_key`,
        // `price_table_version`, a route's `endpoint` — is already in the fixtures above.
        // A field added by a later gateway must be no different, or every Wayfinder card
        // would go dark on an upgrade.
        let health = r#"{"status":"ok","offline":false,"quarantined_routes":["b"],"uptime_s":9}"#;
        assert_eq!(
            row(&read(health, MODELS, SAVINGS_30D), "Plan", "Status"),
            Some("Local gateway")
        );
        // Null is how the source's optional list arrives when the gateway has nothing to
        // report, and it means the same as the field being absent.
        let null_keys = r#"{"status":"degraded","offline":false,"missing_keys":null}"#;
        assert_eq!(
            row(&read(null_keys, MODELS, SAVINGS_30D), "Plan", "Status"),
            Some("Degraded")
        );
    }

    #[test]
    fn the_gateway_url_defaults_to_loopback_and_refuses_a_plaintext_stranger() {
        let default = Wayfinder::new(&options(&[])).expect("the default is a gateway");
        assert_eq!(default.base(), DEFAULT_BASE_URL);
        assert_eq!(
            health_url(default.base()),
            "http://127.0.0.1:8088/healthz",
            "the source's own endpoint"
        );
        assert_eq!(
            models_url(default.base()),
            "http://127.0.0.1:8088/router/models"
        );
        assert_eq!(
            savings_url(default.base()),
            "http://127.0.0.1:8088/v1/savings?period=30d"
        );

        let prefixed = Wayfinder::new(&options(&[(BASE_URL, "https://wayfinder.example.com/wf/")]))
            .expect("a remote gateway over HTTPS is allowed");
        assert_eq!(
            savings_url(prefixed.base()),
            "https://wayfinder.example.com/wf/v1/savings?period=30d",
            "a path prefix survives, and the trailing slash does not"
        );

        assert!(Wayfinder::new(&options(&[(BASE_URL, "http://localhost:9090")])).is_ok());
        // Plain HTTP is allowed to this machine because that is where the gateway runs, and
        // nowhere else — including to a host that only looks like this machine.
        for refused in [
            "http://192.168.1.5:8088",
            "http://attacker.test",
            "http://user@127.0.0.1:8088",
            "ftp://127.0.0.1:8088",
        ] {
            assert!(
                matches!(
                    Wayfinder::new(&options(&[(BASE_URL, refused)])),
                    Err(ProviderError::Local(_))
                ),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn the_account_is_its_gateway_and_nothing_else() {
        assert_eq!(SPEC.credential, CredentialKind::None);
        assert!(
            SPEC.credential_hint.is_empty(),
            "there is no page to send anyone to for a credential that does not exist"
        );
        assert_eq!(SPEC.options.len(), 1);
        assert_eq!(SPEC.options[0].name, BASE_URL);
        // The blank credential the registry hands the builder is ignored, which is the
        // whole of what a keyless provider means.
        assert!((SPEC.build)(Credential::new(String::new()), &options(&[])).is_ok());
    }
}
