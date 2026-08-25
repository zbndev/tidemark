//! Direct Cloud Code Assist quota parsing for a Tidemark-owned OAuth session.
//!
//! This payload is deliberately kept separate from the local `agy` RPC payload. The
//! direct endpoint describes quota once per model even when several models draw from the
//! same backend counter, and releases have used several container spellings for those
//! entries. Normalization happens before validation and deduplication so every known
//! spelling has the same strict meaning.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp, Window, WindowKey, WindowLength};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::providers::{ProviderError, title_case};

use super::PROVIDER_ID;

const DAY_SECONDS: u64 = 86_400;
const WEEK_SECONDS: u64 = 7 * DAY_SECONDS;

/// Fetches direct quota using the OAuth access token and the discovered project, if any.
pub async fn fetch(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    project_id: Option<&str>,
) -> Result<Snapshot, ProviderError> {
    let url = format!(
        "{}/v1internal:fetchAvailableModels",
        endpoint.trim_end_matches('/')
    );
    let payload = match project_id {
        Some(project_id) => serde_json::json!({ "project": project_id }),
        // Not `{"project": ""}`: an account Google gave no Cloud AI Companion project
        // asks about itself, and a blank one is a value the server would validate.
        None => serde_json::json!({}),
    };
    let response = client
        .post(&url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await
        .map_err(ProviderError::Transport)?;
    let sent_body = payload.to_string();
    let body = crate::providers::read_body(
        PROVIDER_ID,
        crate::debug::Sent {
            method: "POST",
            url: &url,
            body: Some(&sent_body),
        },
        response,
    )
    .await?;
    parse(&body, Timestamp::now())
}

/// Turns a direct `fetchAvailableModels` response into one window per shared counter.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let response: AvailableModels = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a direct Antigravity quota response: {error}"))
    })?;

    let mut parsed = Vec::new();
    for (model_id, model) in response.models {
        for flat in model.normalized() {
            parsed.push(ParsedQuota::new(&model_id, flat)?);
        }
    }

    infer_missing_windows(&mut parsed, captured_at);

    let mut counters: BTreeMap<CounterIdentity, MergedQuota> = BTreeMap::new();
    for quota in parsed {
        let descriptor = quota
            .descriptor
            .expect("every normalized quota receives a window descriptor");
        let identity = CounterIdentity {
            counter: quota.counter,
            tier: quota.tier,
            window_id: descriptor.id.clone(),
        };
        match counters.get_mut(&identity) {
            Some(existing) => {
                existing.remaining_fraction =
                    existing.remaining_fraction.min(quota.remaining_fraction);
                existing.resets_at = earliest(existing.resets_at, quota.resets_at);
            }
            None => {
                counters.insert(
                    identity,
                    MergedQuota {
                        descriptor,
                        remaining_fraction: quota.remaining_fraction,
                        resets_at: quota.resets_at,
                    },
                );
            }
        }
    }

    if counters.is_empty() {
        return Err(ProviderError::malformed(
            "the direct Antigravity response described no quota windows",
        ));
    }

    let mut keys = BTreeSet::new();
    let mut windows = Vec::with_capacity(counters.len());
    for (identity, quota) in counters {
        let pool = if identity.tier == "default" {
            identity.counter.clone()
        } else {
            format!("{}/{}", identity.counter, identity.tier)
        };
        let key = match quota.descriptor.length {
            Some(length) => WindowKey::for_pool(&pool, length),
            // The API supplied an identity but no duration. Keeping that identity is
            // safer than guessing a duration from display copy or array position.
            None => WindowKey::named(&format!("{pool}/{}", identity.window_id)),
        };
        if !keys.insert(key.as_str().to_owned()) {
            return Err(ProviderError::malformed(format!(
                "two direct Antigravity counters arrived under the key {key}"
            )));
        }

        let counter = counter_title(&identity.counter);
        let tier = (identity.tier != "default").then(|| title_case(&identity.tier));
        let title = match tier {
            Some(tier) => format!("{counter} · {tier} · {}", quota.descriptor.label),
            None => format!("{counter} · {}", quota.descriptor.label),
        };
        windows.push(Window {
            key,
            title,
            subtitle: None,
            used_percent: (1.0 - quota.remaining_fraction.clamp(0.0, 1.0)) * 100.0,
            resets_at: quota.resets_at,
            length: quota.descriptor.length,
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

fn earliest(one: Option<Timestamp>, other: Option<Timestamp>) -> Option<Timestamp> {
    match (one, other) {
        (Some(one), Some(other)) => Some(one.min(other)),
        (one, other) => one.or(other),
    }
}

fn infer_missing_windows(quotas: &mut [ParsedQuota], captured_at: Timestamp) {
    let mut resets_by_counter: BTreeMap<(String, String), BTreeSet<Timestamp>> = BTreeMap::new();
    for quota in quotas.iter().filter(|quota| quota.descriptor.is_none()) {
        let resets = resets_by_counter
            .entry((quota.counter.clone(), quota.tier.clone()))
            .or_default();
        if let Some(reset) = quota.resets_at {
            resets.insert(reset);
        }
    }

    for quota in quotas {
        if quota.descriptor.is_some() {
            continue;
        }
        let resets = &resets_by_counter[&(quota.counter.clone(), quota.tier.clone())];
        let latest_distinct = (resets.len() > 1).then(|| resets.last().copied()).flatten();
        let weekly = latest_distinct.is_some_and(|latest| quota.resets_at == Some(latest))
            || quota
                .resets_at
                .is_some_and(|reset| captured_at.seconds_until(reset) > DAY_SECONDS as i64);
        quota.descriptor = Some(if weekly {
            WindowDescriptor::weekly()
        } else {
            WindowDescriptor::daily()
        });
    }
}

fn counter_title(counter: &str) -> String {
    match counter {
        "google" => "Google".to_owned(),
        "anthropic" => "Anthropic".to_owned(),
        "openai" => "OpenAI".to_owned(),
        other => title_case(other),
    }
}

fn counter_name(model_provider: Option<&str>, api_provider: Option<&str>) -> Option<&'static str> {
    model_provider
        .and_then(recognized_counter)
        .or_else(|| api_provider.and_then(recognized_counter))
}

fn recognized_counter(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "MODEL_PROVIDER_GOOGLE" | "API_PROVIDER_GOOGLE_GEMINI" => Some("google"),
        "MODEL_PROVIDER_ANTHROPIC" | "API_PROVIDER_ANTHROPIC_VERTEX" => Some("anthropic"),
        "MODEL_PROVIDER_OPENAI" | "API_PROVIDER_OPENAI_VERTEX" => Some("openai"),
        _ => None,
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_reset(raw: &Present<String>, what: &str) -> Result<Option<Timestamp>, ProviderError> {
    let Present::Value(raw) = raw else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(|parsed| Timestamp::from_unix(parsed.unix_timestamp()).ok())
        .map(Some)
        .ok_or_else(|| ProviderError::malformed(format!("{what} has an unreadable resetTime")))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvailableModels {
    models: BTreeMap<String, ModelInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    #[serde(default)]
    model_provider: Option<String>,
    #[serde(default)]
    api_provider: Option<String>,
    #[serde(default)]
    quota_info: Option<QuotaEntries>,
    #[serde(default)]
    quota_infos: Option<QuotaEntries>,
    #[serde(default)]
    daily_quota_info: Option<QuotaEntries>,
    #[serde(default)]
    daily_quota_infos: Option<QuotaEntries>,
    #[serde(default)]
    weekly_quota_info: Option<QuotaEntries>,
    #[serde(default)]
    weekly_quota_infos: Option<QuotaEntries>,
    #[serde(default)]
    quota_info_by_tier: Option<BTreeMap<String, QuotaEntries>>,
    #[serde(default)]
    quota_infos_by_tier: Option<BTreeMap<String, QuotaEntries>>,
    #[serde(default)]
    quota_info_by_window: Option<BTreeMap<String, QuotaEntries>>,
    #[serde(default)]
    quota_infos_by_window: Option<BTreeMap<String, QuotaEntries>>,
}

impl ModelInfo {
    fn normalized(&self) -> Vec<FlatQuota> {
        let mut quotas = Vec::new();
        self.add(&mut quotas, self.quota_info.as_ref(), None, None);
        self.add(&mut quotas, self.quota_infos.as_ref(), None, None);
        self.add(
            &mut quotas,
            self.daily_quota_info.as_ref(),
            None,
            Some(WindowDescriptor::daily()),
        );
        self.add(
            &mut quotas,
            self.daily_quota_infos.as_ref(),
            None,
            Some(WindowDescriptor::daily()),
        );
        self.add(
            &mut quotas,
            self.weekly_quota_info.as_ref(),
            None,
            Some(WindowDescriptor::weekly()),
        );
        self.add(
            &mut quotas,
            self.weekly_quota_infos.as_ref(),
            None,
            Some(WindowDescriptor::weekly()),
        );

        for map in [
            self.quota_info_by_tier.as_ref(),
            self.quota_infos_by_tier.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            for (tier, entries) in map {
                self.add(&mut quotas, Some(entries), Some(tier), None);
            }
        }
        for map in [
            self.quota_info_by_window.as_ref(),
            self.quota_infos_by_window.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            for (window_id, entries) in map {
                self.add(
                    &mut quotas,
                    Some(entries),
                    None,
                    WindowDescriptor::explicit(Some(window_id), None),
                );
            }
        }
        quotas
    }

    fn add(
        &self,
        output: &mut Vec<FlatQuota>,
        entries: Option<&QuotaEntries>,
        tier: Option<&str>,
        container_window: Option<WindowDescriptor>,
    ) {
        let Some(entries) = entries else {
            return;
        };
        for quota in entries.iter() {
            output.push(FlatQuota {
                quota: quota.clone(),
                model_provider: quota
                    .model_provider
                    .clone()
                    .or_else(|| self.model_provider.clone()),
                api_provider: quota
                    .api_provider
                    .clone()
                    .or_else(|| self.api_provider.clone()),
                tier: tier.map(str::to_owned).or_else(|| quota.tier.clone()),
                container_window: container_window.clone(),
            });
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum QuotaEntries {
    One(QuotaInfo),
    Many(Vec<QuotaInfo>),
}

impl QuotaEntries {
    fn iter(&self) -> Box<dyn Iterator<Item = &QuotaInfo> + '_> {
        match self {
            Self::One(quota) => Box::new(std::iter::once(quota)),
            Self::Many(quotas) => Box::new(quotas.iter()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaInfo {
    #[serde(default)]
    remaining_fraction: Present<f64>,
    #[serde(default)]
    reset_time: Present<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    window_label: Option<String>,
    #[serde(default)]
    api_provider: Option<String>,
    #[serde(default)]
    model_provider: Option<String>,
}

#[derive(Debug, Clone, Default)]
enum Present<T> {
    #[default]
    Missing,
    Value(T),
}

impl<'de, T> Deserialize<'de> for Present<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Value)
    }
}

#[derive(Debug, Clone)]
struct FlatQuota {
    quota: QuotaInfo,
    model_provider: Option<String>,
    api_provider: Option<String>,
    tier: Option<String>,
    container_window: Option<WindowDescriptor>,
}

#[derive(Debug)]
struct ParsedQuota {
    counter: String,
    tier: String,
    remaining_fraction: f64,
    resets_at: Option<Timestamp>,
    descriptor: Option<WindowDescriptor>,
}

impl ParsedQuota {
    fn new(model_id: &str, flat: FlatQuota) -> Result<Self, ProviderError> {
        let what = format!("model {model_id} quota entry");
        let resets_at = parse_reset(&flat.quota.reset_time, &what)?;
        let remaining_fraction = match flat.quota.remaining_fraction {
            Present::Value(fraction) if fraction.is_finite() => fraction,
            Present::Value(_) => {
                return Err(ProviderError::malformed(format!(
                    "{what} has a non-finite remainingFraction"
                )));
            }
            Present::Missing if resets_at.is_some() => 0.0,
            Present::Missing => {
                return Err(ProviderError::malformed(format!(
                    "{what} has neither remainingFraction nor resetTime"
                )));
            }
        };
        let descriptor = WindowDescriptor::explicit(
            flat.quota.window_id.as_deref(),
            flat.quota.window_label.as_deref(),
        )
        .or(flat.container_window);
        let counter = counter_name(flat.model_provider.as_deref(), flat.api_provider.as_deref())
            .ok_or_else(|| {
                ProviderError::malformed(format!(
                    "{what} has neither a recognized modelProvider nor apiProvider"
                ))
            })?;
        Ok(Self {
            counter: counter.to_owned(),
            tier: nonempty(flat.tier.as_deref())
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| "default".to_owned()),
            remaining_fraction,
            resets_at,
            descriptor,
        })
    }
}

#[derive(Debug, Clone)]
struct WindowDescriptor {
    id: String,
    label: String,
    length: Option<WindowLength>,
}

impl WindowDescriptor {
    fn daily() -> Self {
        Self {
            id: "daily".to_owned(),
            label: "Daily".to_owned(),
            length: WindowLength::from_secs(DAY_SECONDS),
        }
    }

    fn weekly() -> Self {
        Self {
            id: "weekly".to_owned(),
            label: "Weekly".to_owned(),
            length: WindowLength::from_secs(WEEK_SECONDS),
        }
    }

    fn explicit(id: Option<&str>, label: Option<&str>) -> Option<Self> {
        let id = nonempty(id);
        let label = nonempty(label);
        let source = format!("{} {}", id.unwrap_or_default(), label.unwrap_or_default())
            .to_ascii_lowercase();
        if source.contains("week")
            || source.contains("7d")
            || source.contains("7 day")
            || source.contains("7-day")
            || source.contains("7_day")
        {
            return Some(Self::weekly());
        }
        if source.contains("daily") || source.contains("day") || source.contains("24h") {
            return Some(Self::daily());
        }
        let id = id.or(label)?.to_ascii_lowercase();
        Some(Self {
            label: label.map(str::to_owned).unwrap_or_else(|| title_case(&id)),
            id,
            length: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CounterIdentity {
    counter: String,
    tier: String,
    window_id: String,
}

#[derive(Debug)]
struct MergedQuota {
    descriptor: WindowDescriptor,
    remaining_fraction: f64,
    resets_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use tidemark_types::Timestamp;

    /// Captures exactly one request and returns one successful JSON response.
    fn one_request_server(
        response_body: &'static str,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request accepted");
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header read");
                if line == "\r\n" {
                    request.push_str(&line);
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().expect("content length");
                }
                request.push_str(&line);
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("body read");
            request.push_str(&String::from_utf8(body).expect("request body is text"));
            drop(reader);
            request_tx.send(request).expect("request captured");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .expect("response written");
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn a_login_without_a_project_asks_for_models_without_one() {
        // The field is omitted rather than sent blank: `{"project":""}` is a value the
        // server may validate, while an absent project is the question we mean to ask.
        let (base, requests, server) = one_request_server(
            r#"{"models":{"m":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-08-28T00:00:00Z","windowId":"weekly"}}}}"#,
        );
        let client = crate::providers::http::client().expect("client");
        block_on(fetch(&client, &base, "token", None)).expect("quota fetched");

        let request = requests.recv().expect("request captured");
        assert!(
            request.contains("{}"),
            "an absent project sends an empty body: {request}"
        );
        assert!(!request.contains("\"project\""), "{request}");
        server.join().expect("server stopped");
    }

    #[test]
    fn shared_models_become_one_counter_and_exhausted_is_not_masked() {
        let now = Timestamp::from_unix(1_787_270_400).expect("2026-08-21");
        let snapshot = parse(
            include_str!("../../../tests/fixtures/antigravity-available-models.json"),
            now,
        )
        .expect("fixture parses");
        assert_eq!(snapshot.provider.as_str(), "antigravity");
        assert_eq!(snapshot.account.as_str(), "default");
        assert_eq!(snapshot.windows.len(), 4);
        assert!(snapshot.windows.iter().any(|window| {
            window.key.as_str().contains("openai") && window.used_percent == 100.0
        }));
    }

    #[test]
    fn one_healthy_model_cannot_hide_an_exhausted_shared_counter() {
        let body = r#"{"models":{"claude-a":{"modelProvider":"MODEL_PROVIDER_ANTHROPIC","quotaInfo":{"remainingFraction":0.8,"resetTime":"2026-08-28T00:00:00Z"}},"claude-b":{"modelProvider":"MODEL_PROVIDER_ANTHROPIC","quotaInfo":{"remainingFraction":0.0,"resetTime":"2026-08-28T00:00:00Z"}}}}"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
    }

    #[test]
    fn observed_singular_plural_and_map_containers_are_all_normalized() {
        let body = r#"{
            "models": {
                "all-shapes": {
                    "modelProvider": "MODEL_PROVIDER_GOOGLE",
                    "quotaInfo": {"tier":"single","remainingFraction":0.9,"windowId":"daily"},
                    "quotaInfos": [{"tier":"plural","remainingFraction":0.8,"windowId":"weekly"}],
                    "dailyQuotaInfo": {"tier":"daily-single","remainingFraction":0.7},
                    "dailyQuotaInfos": [{"tier":"daily-plural","remainingFraction":0.6}],
                    "weeklyQuotaInfo": {"tier":"weekly-single","remainingFraction":0.5},
                    "weeklyQuotaInfos": [{"tier":"weekly-plural","remainingFraction":0.4}],
                    "quotaInfoByTier": {
                        "tier-map": {"remainingFraction":0.3,"resetTime":"2026-08-22T00:00:00Z"}
                    },
                    "quotaInfoByWindow": {
                        "WINDOW_7_DAY": {"tier":"window-map","remainingFraction":0.2}
                    },
                    "quotaInfosByWindow": {
                        "WINDOW_DAILY": [{"tier":"windows-map","remainingFraction":0.1}]
                    }
                }
            }
        }"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("all observed containers parse");

        assert_eq!(snapshot.windows.len(), 9);
    }

    #[test]
    fn an_explicit_weekly_id_wins_even_when_reset_is_less_than_a_day_away() {
        let body = r#"{"models":{"gemini":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-08-21T12:00:00Z","windowId":"WINDOW_WEEKLY"}}}}"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("explicit window parses");

        assert_eq!(
            snapshot.windows[0].length.map(|length| length.as_secs()),
            Some(7 * 86_400)
        );
        assert_eq!(snapshot.windows[0].key.as_str(), "google/w604800");
    }

    #[test]
    fn an_unlabelled_far_reset_is_inferred_as_weekly() {
        let body = r#"{"models":{"gemini":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-08-27T00:00:00Z"}}}}"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("reset distance is usable");

        assert_eq!(snapshot.windows[0].key.as_str(), "google/w604800");
    }

    #[test]
    fn an_unlabelled_fraction_without_a_reset_uses_the_daily_fallback() {
        let body = r#"{"models":{"gemini":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfo":{"remainingFraction":0.25}}}}"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("fraction-only quota parses");

        assert_eq!(snapshot.windows[0].key.as_str(), "google/w86400");
        assert_eq!(snapshot.windows[0].used_percent, 75.0);
        assert_eq!(snapshot.windows[0].resets_at, None);
    }

    #[test]
    fn provider_identity_uses_recognized_api_when_model_provider_is_unspecified_or_unknown() {
        let body = r#"{"models":{"claude":{"modelProvider":"MODEL_PROVIDER_UNSPECIFIED","apiProvider":"API_PROVIDER_ANTHROPIC_VERTEX","quotaInfo":{"remainingFraction":0.5,"windowId":"weekly"}},"gpt":{"modelProvider":"MODEL_PROVIDER_VENDOR","apiProvider":"API_PROVIDER_OPENAI_VERTEX","quotaInfo":{"remainingFraction":0.4,"windowId":"weekly"}}}}"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("recognized API providers parse");

        let keys: Vec<_> = snapshot
            .windows
            .iter()
            .map(|window| window.key.as_str())
            .collect();
        assert_eq!(keys, ["anthropic/w604800", "openai/w604800"]);
    }

    #[test]
    fn provider_identity_rejects_missing_provider_instead_of_merging_models() {
        let body = r#"{"models":{"one":{"weeklyQuotaInfo":{"remainingFraction":0.8}},"two":{"weeklyQuotaInfo":{"remainingFraction":0.1}}}}"#;
        let error = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect_err("missing provider identity must fail");

        assert!(matches!(
            error,
            crate::providers::ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn provider_identity_rejects_unknown_provider_instead_of_creating_arbitrary_counters() {
        let body = r#"{"models":{"one":{"modelProvider":"MODEL_PROVIDER_VENDOR_A","weeklyQuotaInfo":{"remainingFraction":0.8}},"two":{"apiProvider":"API_PROVIDER_VENDOR_B","weeklyQuotaInfo":{"remainingFraction":0.1}}}}"#;
        let error = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect_err("unknown provider identity must fail");

        assert!(matches!(
            error,
            crate::providers::ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn duplicate_counters_keep_the_earliest_reset_and_lowest_fraction() {
        let body = r#"{"models":{"a":{"modelProvider":"MODEL_PROVIDER_ANTHROPIC","weeklyQuotaInfo":{"remainingFraction":0.8,"resetTime":"2026-08-28T00:00:00Z"}},"b":{"modelProvider":"MODEL_PROVIDER_ANTHROPIC","weeklyQuotaInfo":{"remainingFraction":0.7,"resetTime":"2026-08-27T00:00:00Z"}}}}"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect("duplicates merge");

        assert_eq!(snapshot.windows.len(), 1);
        assert!((snapshot.windows[0].used_percent - 30.0).abs() < 1e-9);
        assert_eq!(
            snapshot.windows[0].resets_at.map(Timestamp::as_unix),
            Some(1_787_788_800)
        );
    }

    #[test]
    fn a_present_unreadable_reset_rejects_the_known_entry() {
        let body = r#"{"models":{"gemini":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfo":{"remainingFraction":0.5,"resetTime":"tomorrow"}}}}"#;
        let error = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect_err("bad reset must fail");

        assert!(matches!(
            error,
            crate::providers::ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn a_non_finite_fraction_rejects_the_known_entry() {
        let body = r#"{"models":{"gemini":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfo":{"remainingFraction":1e400,"resetTime":"2026-08-28T00:00:00Z"}}}}"#;
        let error = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect_err("non-finite fraction must fail");

        assert!(matches!(
            error,
            crate::providers::ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn a_known_entry_with_neither_fraction_nor_reset_is_rejected() {
        let body = r#"{"models":{"gemini":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfo":{"windowId":"daily"}}}}"#;
        let error = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect_err("unmeasured entry must fail");

        assert!(matches!(
            error,
            crate::providers::ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn distinct_semantic_counters_that_build_the_same_window_key_are_rejected() {
        let body = r#"{"models":{"gemini":{"modelProvider":"MODEL_PROVIDER_GOOGLE","quotaInfos":[{"tier":"pro","remainingFraction":0.5,"windowId":"weekly"},{"remainingFraction":0.4,"windowId":"pro/w604800"}]}}}"#;
        let error = parse(
            body,
            Timestamp::from_unix(1_787_270_400).expect("plausible"),
        )
        .expect_err("duplicate storage key must fail");

        assert!(matches!(
            error,
            crate::providers::ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn direct_fetch_sends_the_project_and_owned_bearer_token() {
        let fixture = include_str!("../../../tests/fixtures/antigravity-available-models.json");
        let (base, requests, server) = one_request_server(fixture);
        let client = crate::providers::http::client().expect("client");
        let snapshot = block_on(fetch(&client, &base, "owned-access", Some("project-1")))
            .expect("quota fetch succeeds");

        assert_eq!(snapshot.provider.as_str(), "antigravity");
        let request = requests.recv().expect("request captured");
        assert!(
            request.starts_with("POST /v1internal:fetchAvailableModels "),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer owned-access"),
            "{request}"
        );
        assert!(
            request.contains("content-type: application/json"),
            "{request}"
        );
        assert!(request.contains("user-agent: Tidemark/"), "{request}");
        assert!(request.contains(r#"{"project":"project-1"}"#), "{request}");
        server.join().expect("server stopped");
    }
}
