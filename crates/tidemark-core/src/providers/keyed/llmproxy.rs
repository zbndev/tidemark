//! LLM Proxy.
//!
//! Ported from CodexBar's Swift parser and fetcher, `Providers/LLMProxy/
//! LLMProxyUsageFetcher.swift`; there is no JS plugin. Never seen answering: every
//! number in the tests is a body CodexBar recorded.
//!
//! # A self-hosted proxy with no home
//!
//! LLM Proxy is software other people run; there is no default host, so the base URL
//! is a *required* free-text option and the account's `build` refuses to build without
//! one, naming the setting. The shared reader enforces the whole
//! rule — HTTPS, with plain HTTP standing for loopback only, which is how a local
//! instance is reached — and a value it refuses is refused at build time, as a
//! [`ProviderError::Local`] the card can state, rather than inside an endpoint
//! closure where it would panic. `/v1/quota-stats` is appended as the fetcher appends
//! it: a base already ending in `/v1` is left alone.
//!
//! # The meaning
//!
//! `{providers: {name: stats}, summary?}` — a proxy speaking for several upstream
//! providers at once. The card's one quota bar is the **tightest** of every
//! provider's quota groups: used is `100 - min(remaining_percent)`, clamped; the
//! reset is the earliest `reset_time` still in the future, elapsed ones dropped so a
//! stale past reset cannot win — the source's own rule, and the reason `parse` is
//! honest only about a clock the caller supplies. No span arrives on the wire, so
//! the window has no length and no pace mark and is keyed by name.
//!
//! The providers themselves, sorted by request count (name ascending on a tie) and
//! cut to three, draw as informational windows — a zero bar carrying
//! `120 req · 6,000 tok · $12.50` — exactly as the source draws them under its
//! primary. Totals come from `summary` when it states them and from the providers'
//! sums otherwise; the cost falls back to the providers' sum only when positive.
//!
//! `quota_groups` may be a list or a keyed map, and a map is sorted by remaining
//! ascending before it is flattened, as the source sorts it. The whole field is
//! best-effort in the source — a shape neither reading parses becomes nothing, so
//! the card shows no quota bar rather than failing; this port keeps that leniency
//! and confines its refusal to the strict fields (`total_requests` and its kin,
//! which the source type-checks).

use super::{HandSpec, OptionSchema, Options, base_url, redact_query, required};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "llmproxy";

/// Name of the base-URL setting under `[provider.llmproxy]`.
pub const BASE_URL: &str = "base_url";

/// How many of the providers draw as informational windows, as the source cuts them.
const TOP_PROVIDERS: usize = 3;

#[derive(Debug, Deserialize)]
struct Envelope {
    providers: BTreeMap<String, ProviderStats>,
    #[serde(default)]
    summary: Option<Summary>,
}

#[derive(Debug, Deserialize)]
struct ProviderStats {
    #[serde(default, rename = "credential_count")]
    credential_count: Option<i64>,
    #[serde(default, rename = "active_count")]
    active_count: Option<i64>,
    /// Counted by the source's snapshot and then drawn by nobody; kept unread the
    /// same way.
    #[serde(default, rename = "exhausted_count")]
    #[allow(dead_code)]
    exhausted_count: Option<i64>,
    #[serde(default, rename = "total_requests")]
    total_requests: Option<i64>,
    #[serde(default)]
    tokens: Option<Tokens>,
    #[serde(default, rename = "approx_cost")]
    approx_cost: Option<f64>,
    /// A list or a keyed map, read best-effort; see the module docs.
    #[serde(default, rename = "quota_groups")]
    quota_groups: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Tokens {
    #[serde(default, rename = "input_cached")]
    input_cached: Option<i64>,
    #[serde(default, rename = "input_uncached")]
    input_uncached: Option<i64>,
    #[serde(default)]
    output: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct QuotaGroup {
    #[serde(default, rename = "remaining_percent")]
    remaining_percent: Option<f64>,
    #[serde(default, rename = "reset_time")]
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Summary {
    #[serde(default, rename = "total_requests")]
    total_requests: Option<i64>,
    #[serde(default, rename = "approx_cost")]
    approx_cost: Option<f64>,
    #[serde(default, rename = "total_tokens")]
    total_tokens: Option<i64>,
}

/// One provider's line under the quota bar.
struct ProviderSummary {
    name: String,
    requests: i64,
    tokens: i64,
    approx_cost: Option<f64>,
}

impl ProviderSummary {
    /// The source's own sentence: request and token counts grouped, the cost last.
    fn subtitle(&self) -> String {
        let mut pieces = vec![
            format!("{} req", grouped(self.requests)),
            format!("{} tok", grouped(self.tokens)),
        ];
        if let Some(cost) = self.approx_cost {
            pieces.push(usd(cost));
        }
        pieces.join(" · ")
    }
}

/// The quota groups of one stats block, as the source reads them: a list in its own
/// order, or a keyed map sorted by remaining ascending — and nothing at all when the
/// shape parses as neither.
fn quota_groups(value: Option<&serde_json::Value>) -> Vec<QuotaGroup> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Ok(list) = serde_json::from_value::<Vec<QuotaGroup>>(value.clone()) {
        return list;
    }
    let Ok(map) = serde_json::from_value::<BTreeMap<String, QuotaGroup>>(value.clone()) else {
        return Vec::new();
    };
    let mut groups: Vec<QuotaGroup> = map.into_values().collect();
    groups.sort_by(|one, other| {
        let one = one.remaining_percent.unwrap_or(f64::INFINITY);
        let other = other.remaining_percent.unwrap_or(f64::INFINITY);
        one.partial_cmp(&other).unwrap_or(std::cmp::Ordering::Equal)
    });
    groups
}

/// A count grouped the way the source's own formatter groups it (`7,000`).
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

/// An amount of money in the source's own spelling: `$`, two decimals, and the
/// thousands grouped (`$1,000.00`).
fn usd(value: f64) -> String {
    let rendered = format!("{value:.2}");
    let (int_part, rest) = rendered.split_once('.').unwrap_or((rendered.as_str(), ""));
    let bytes = int_part.as_bytes();
    let mut grouped = String::with_capacity(int_part.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    if !rest.is_empty() {
        grouped.push('.');
        grouped.push_str(rest);
    }
    format!("${grouped}")
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a
/// test, including the reset filter's dependence on the clock the caller supplies.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let data: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    // The providers, sorted by request count with the name breaking ties, as the
    // source sorts them before cutting to three.
    let mut summaries: Vec<ProviderSummary> = data
        .providers
        .iter()
        .map(|(name, stats)| ProviderSummary {
            name: name.clone(),
            requests: stats.total_requests.unwrap_or(0),
            tokens: token_total(stats.tokens.as_ref()),
            approx_cost: stats.approx_cost,
        })
        .collect();
    summaries.sort_by(|one, other| {
        other
            .requests
            .cmp(&one.requests)
            .then_with(|| one.name.cmp(&other.name))
    });

    // Totals: the summary block wins, the providers' sums answer for it.
    let summary = data.summary.as_ref();
    let requests = summary
        .and_then(|s| s.total_requests)
        .unwrap_or_else(|| summaries.iter().map(|summary| summary.requests).sum());
    let tokens = summary
        .and_then(|s| s.total_tokens)
        .unwrap_or_else(|| summaries.iter().map(|summary| summary.tokens).sum());
    let cost = summary.and_then(|s| s.approx_cost).or_else(|| {
        let sum: f64 = summaries
            .iter()
            .filter_map(|summary| summary.approx_cost)
            .sum();
        (sum > 0.0).then_some(sum)
    });

    // The tightest of every provider's quota groups, and the earliest reset still in
    // the future — elapsed ones are dropped so a stale past reset cannot win.
    let groups: Vec<QuotaGroup> = data
        .providers
        .values()
        .flat_map(|stats| quota_groups(stats.quota_groups.as_ref()))
        .collect();
    let minimum_remaining = groups
        .iter()
        .filter_map(|group| group.remaining_percent)
        .fold(None::<f64>, |tightest, remaining| {
            Some(match tightest {
                Some(tightest) if tightest <= remaining => tightest,
                _ => remaining,
            })
        });
    let next_reset = groups
        .iter()
        .filter_map(|group| group.reset_time.as_deref())
        .filter_map(parse_rfc3339)
        .filter(|at| *at > captured_at)
        .min();

    let mut windows = Vec::new();
    if let Some(remaining) = minimum_remaining {
        // No span arrives on the wire, so the window is keyed by name: there is no
        // length for WindowKey::for_length to derive one from.
        windows.push(Window {
            key: WindowKey::named("quota"),
            title: "Quota".to_owned(),
            subtitle: None,
            used_percent: (100.0 - remaining).clamp(0.0, 100.0),
            resets_at: next_reset,
            length: None,
        });
    }
    for summary in summaries.iter().take(TOP_PROVIDERS) {
        // Informational bars, as the source draws them: no quota to fill against,
        // the summary text riding underneath.
        windows.push(Window {
            key: WindowKey::named(&summary.name),
            title: summary.name.clone(),
            subtitle: Some(summary.subtitle()),
            used_percent: 0.0,
            resets_at: None,
            length: None,
        });
    }
    for (index, one) in windows.iter().enumerate() {
        if windows[..index].iter().any(|other| other.key == one.key) {
            return Err(ProviderError::malformed(format!(
                "two windows arrived under the key {}",
                one.key
            )));
        }
    }

    // The identity line the source always draws: how many keys still answer.
    let credential_count: i64 = data
        .providers
        .values()
        .map(|stats| stats.credential_count.unwrap_or(0))
        .sum();
    let active_count: i64 = data
        .providers
        .values()
        .map(|stats| stats.active_count.unwrap_or(0))
        .sum();
    let mut rows = vec![
        DetailRow {
            label: "Requests".to_owned(),
            value: grouped(requests),
        },
        DetailRow {
            label: "Tokens".to_owned(),
            value: grouped(tokens),
        },
    ];
    if let Some(cost) = cost {
        rows.push(DetailRow {
            label: "Approx. spend".to_owned(),
            value: usd(cost),
        });
    }
    rows.push(DetailRow {
        label: "Credentials".to_owned(),
        value: format!("{active_count}/{credential_count} active keys"),
    });

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: vec![DetailSection {
            title: "Usage summary".to_owned(),
            rows,
        }],
    })
}

/// Tokens in, tokens out, cached or not — all three halves, as the source adds them.
fn token_total(tokens: Option<&Tokens>) -> i64 {
    tokens
        .map(|tokens| {
            tokens.input_cached.unwrap_or(0)
                + tokens.input_uncached.unwrap_or(0)
                + tokens.output.unwrap_or(0)
        })
        .unwrap_or(0)
}

/// LLM Proxy as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "LLM Proxy",
    credential: CredentialKind::Key,
    credential_hint: "LLM Proxy admin console → API keys.",
    options: &[OptionSchema {
        name: BASE_URL,
        title: "Base URL",
        description: Some("Host of the LLM Proxy instance to poll; HTTPS, or HTTP on loopback."),
        default: "",
        choices: &[],
        required: true,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The base
/// URL is required — LLM Proxy has no default host — and resolved here, so a changed
/// one takes effect on the next build and a value the shared reader refuses is a
/// [`ProviderError::Local`] naming the setting, not a panic mid-fetch.
fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(LLMProxy::new(credential, options)?))
}

/// One LLM Proxy deployment: the key, and the `/v1/quota-stats` URL it is polled at.
pub struct LLMProxy {
    client: reqwest::Client,
    credential: Credential,
    url: String,
}

impl LLMProxy {
    /// Builds a client. The URL is resolved once, here, because a setting that changed
    /// the host would otherwise take effect only on the next daemon restart.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        // `required` proved a value exists and `base_url` the HTTPS-or-loopback rule on
        // it, the same two checks a catalogued spec's build makes; both name the setting
        // when they refuse.
        let raw = required(options, BASE_URL, "Base URL")?;
        let base = base_url(&Options::from([(BASE_URL.to_owned(), raw)]), BASE_URL, "")?;
        Ok(Self {
            client: http::client()?,
            credential,
            url: quota_stats_url(&base),
        })
    }

    /// The `/v1/quota-stats` URL this instance polls.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The request this instance would send, built but not sent, so that the placement
    /// of the key is testable without a server.
    fn quota_stats_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(&self.url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let body = super::request(PROVIDER_ID, &self.client, self.quota_stats_request()?).await?;
        parse(&body, Timestamp::now())
    }
}

impl fmt::Debug for LLMProxy {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LLMProxy")
            .field("id", &PROVIDER_ID)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Provider for LLMProxy {
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

/// The `/v1/quota-stats` URL of a deployment base, appended as the fetcher appends it:
/// `/v1` unless the base already ends in it, then `/quota-stats`. Pure, so the
/// recorded URL spellings stay reachable from a test.
fn quota_stats_url(base: &str) -> String {
    let mut base = base.to_owned();
    if !base.ends_with("/v1") {
        base.push_str("/v1");
    }
    base.push_str("/quota-stats");
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{DetailRow, Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `LLMProxyUsageFetcherTests.swift` — "parses quota stats
    /// summary". CodexBar asserts two providers, four credentials of which three
    /// active, 160 requests, 7,000 tokens, $15.5 approximate cost, 42% minimum
    /// remaining, the primary bar at 58%, and the grouped request and token totals.
    /// One provider carries `quota_groups` as a keyed map, the other as a list.
    const QUOTA_STATS: &str = r#"
        {
          "providers": {
            "openai": {
              "credential_count": 3,
              "active_count": 2,
              "exhausted_count": 1,
              "total_requests": 120,
              "tokens": {
                "input_cached": 1000,
                "input_uncached": 2000,
                "output": 3000
              },
              "approx_cost": 12.5,
              "quota_groups": {
                "default": {
                  "remaining_percent": 42,
                  "reset_time": "2026-05-18T12:00:00Z"
                }
              }
            },
            "anthropic": {
              "credential_count": 1,
              "active_count": 1,
              "exhausted_count": 0,
              "total_requests": 40,
              "tokens": {
                "input_cached": 0,
                "input_uncached": 500,
                "output": 500
              },
              "approx_cost": 3.0,
              "quota_groups": [
                { "remaining_percent": 80 }
              ]
            }
          },
          "summary": {
            "total_requests": 160,
            "total_tokens": 7000,
            "approx_cost": 15.5
          }
        }
        "#;

    /// Recorded by CodexBar, same file — "parses fractional second quota reset
    /// times". A reset time carrying milliseconds, and nothing else on the wire.
    const FRACTIONAL_RESET: &str = r#"
        {
          "providers": {
            "openai": {
              "quota_groups": [
                {
                  "remaining_percent": 42,
                  "reset_time": "2026-05-18T12:00:00.123Z"
                }
              ]
            }
          }
        }
        "#;

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
    fn the_summary_fixture_draws_the_tightest_quota_and_the_top_providers() {
        // CodexBar reads this body at one second past the epoch, before any recorded
        // reset; this port reads it at the earliest instant its clock accepts. The
        // reset filter drops elapsed resets, so the clock the test supplies must sit
        // before 2026-05-18 for the reset to survive.
        let snapshot = parse(QUOTA_STATS, at(1_600_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["quota", "openai", "anthropic"]);

        let quota = window(&snapshot, "quota");
        assert_eq!(
            quota.used_percent, 58.0,
            "100 - 42% remaining, the tightest of both providers' groups"
        );
        assert_eq!(quota.subtitle, None);
        assert_eq!(
            quota.resets_at,
            Some(at(1_779_105_600)),
            "2026-05-18T12:00:00Z, the only reset still in the future"
        );
        assert_eq!(quota.length, None, "the wire states no span");

        let openai = window(&snapshot, "openai");
        assert_eq!(openai.title, "openai");
        assert_eq!(
            openai.used_percent, 0.0,
            "an informational bar, as in the source"
        );
        assert_eq!(
            openai.subtitle.as_deref(),
            Some("120 req · 6,000 tok · $12.50"),
            "1000 + 2000 + 3000 tokens, grouped as CodexBar groups them"
        );
        assert_eq!(openai.resets_at, None);

        let anthropic = window(&snapshot, "anthropic");
        assert_eq!(
            anthropic.subtitle.as_deref(),
            Some("40 req · 1,000 tok · $3.00")
        );

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "quota",
            "no window states a length, so the card leads with the quota"
        );

        assert_eq!(row(&snapshot, "Usage summary", "Requests").value, "160");
        assert_eq!(row(&snapshot, "Usage summary", "Tokens").value, "7,000");
        assert_eq!(
            row(&snapshot, "Usage summary", "Approx. spend").value,
            "$15.50"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "Credentials").value,
            "3/4 active keys",
            "the identity line CodexBar draws for this body"
        );
    }

    #[test]
    fn the_fractional_fixture_reads_its_reset() {
        let snapshot = parse(FRACTIONAL_RESET, at(1_600_000_000)).expect("parses");
        let quota = window(&snapshot, "quota");
        assert_eq!(quota.used_percent, 58.0);
        assert_eq!(
            quota.resets_at,
            Some(at(1_779_105_600)),
            "2026-05-18T12:00:00.123Z to the whole second"
        );
    }

    #[test]
    fn an_elapsed_reset_is_dropped_but_the_window_still_draws() {
        // The same recorded fractional body, read after its reset has passed: the
        // source's filter exists so a stale reset cannot win, and the bar must not
        // fail with it.
        let snapshot = parse(FRACTIONAL_RESET, at(1_800_000_000)).expect("parses");
        let quota = window(&snapshot, "quota");
        assert_eq!(quota.used_percent, 58.0);
        assert_eq!(quota.resets_at, None, "the only reset is elapsed");
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names; a body without the providers
        // object the source declares required; and the request count as a string
        // where a number belongs — a strict field the source type-checks.
        let string_where_number = r#"
        { "providers": { "openai": { "total_requests": "many" } } }
        "#;
        let no_providers = r#"{"summary": {"total_requests": 160}}"#;
        for body in [
            "{\"partial\":",
            "not-json",
            string_where_number,
            no_providers,
        ] {
            let error = parse(body, at(1_600_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_field_of_a_kind_this_parser_does_not_read_is_skipped() {
        // The recorded summary body carrying one provider field invented after this
        // was written. An object-shaped provider meets the unknown-kind rule here:
        // an unread field is skipped, not refused, and every window still draws.
        let body = QUOTA_STATS.replacen(
            "\"total_requests\": 120,",
            "\"latency_p99_ms\": 240, \"total_requests\": 120,",
            1,
        );
        let snapshot = parse(&body, at(1_600_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(window(&snapshot, "quota").used_percent, 58.0);
    }

    #[test]
    fn an_unreadable_quota_groups_shape_draws_no_bar_rather_than_failing() {
        // No numbers here to invent: the source reads quota_groups best-effort, and
        // a shape neither of its two readings parses becomes nothing.
        let body = r#"
        { "providers": { "openai": { "quota_groups": "banana" } } }
        "#;
        let snapshot = parse(body, at(1_600_000_000)).expect("parses");
        assert!(
            snapshot.windows.iter().all(|w| w.key.as_str() != "quota"),
            "no quota reading, and none is invented"
        );
    }

    fn options(base: &str) -> Options {
        Options::from([(BASE_URL.to_owned(), base.to_owned())])
    }

    fn built(base: &str) -> Result<LLMProxy, ProviderError> {
        LLMProxy::new(Credential::new("sk-test"), &options(base))
    }

    #[test]
    fn the_required_base_url_resolves_the_v1_quota_stats_path() {
        // CodexBar's own URL test: a bare host and one already carrying /v1 reach
        // the same path.
        assert_eq!(
            built("https://proxy.example.com").expect("builds").url(),
            "https://proxy.example.com/v1/quota-stats"
        );
        assert_eq!(
            built("https://proxy.example.com/v1").expect("builds").url(),
            "https://proxy.example.com/v1/quota-stats"
        );
        assert_eq!(
            built("http://127.0.0.1:8080").expect("builds").url(),
            "http://127.0.0.1:8080/v1/quota-stats",
            "loopback HTTP is how a self-hosted LLM Proxy is reached"
        );
    }

    #[test]
    fn a_base_url_the_shared_reader_refuses_fails_the_build_naming_the_setting() {
        // A set-but-invalid value — plain HTTP to a remote host, or the likelier
        // scheme-less typo — must refuse the account's build as a `Local` the card can
        // state. The daemon's factory calls this same `build`, so proving it here is
        // proving the daemon cannot panic on these inputs.
        for bad in ["http://remote.host", "myproxy.example.com"] {
            let Err(error) = build(Credential::new("sk-test"), &options(bad)) else {
                panic!("{bad} must refuse the build, not panic");
            };
            assert!(
                matches!(error, ProviderError::Local(ref message)
                    if message.contains("base_url") && message.contains("https://")),
                "{error} for {bad}"
            );
            assert!(
                built(bad).is_err(),
                "the constructor and the builder refuse the same values"
            );
        }
    }

    #[test]
    fn an_unset_base_url_names_itself_rather_than_malforming_the_url() {
        // Without this refusal the user would see "Unreachable: relative URL without a
        // base" on every poll, with nothing pointing at the settings field that fixes it.
        let Err(error) = build(Credential::new("sk-test"), &Options::new()) else {
            panic!("the required option is unset, so the build must refuse")
        };
        assert!(
            matches!(error, ProviderError::Local(ref message)
                if message == "Base URL is not set for this account"),
            "{error}"
        );
        let Err(blank) = build(Credential::new("sk-test"), &options("  ")) else {
            panic!("a blank value is an unset value, so the build must refuse")
        };
        assert!(
            matches!(blank, ProviderError::Local(ref message) if message.contains("Base URL")),
            "{blank}"
        );
    }

    #[test]
    fn the_request_polls_with_a_bearer_key() {
        let llmproxy = built("https://proxy.example.com").expect("builds");
        let request = llmproxy.quota_stats_request().expect("builds");
        assert_eq!(request.method(), reqwest::Method::GET);
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
            "application/json",
            "the recorded request carries this header"
        );
    }

    #[test]
    fn the_spec_publishes_one_required_option() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "LLM Proxy");
        assert_eq!(SPEC.options.len(), 1);
        let option = &SPEC.options[0];
        assert_eq!(option.name, "base_url");
        assert!(
            option.required,
            "no default host exists; the build refuses without one"
        );
        assert!(option.choices.is_empty(), "free text");
    }

    #[test]
    fn an_llm_proxy_client_never_prints_its_credential() {
        let llmproxy = built("https://proxy.example.com").expect("builds");
        let rendered = format!("{llmproxy:?}");
        assert!(!rendered.contains("sk-test"), "{rendered}");
    }
}
