//! The StepFun Step Plan, read with a pasted Oasis-Token.
//!
//! Two POSTs to `platform.stepfun.com`'s Dashboard RPC answer the card: the rate-limit
//! query carries the windows and the plan-status query the subscription's name, and a
//! failed name ask is dropped rather than failing the fetch. The token is the
//! `Oasis-Token` cookie's value, and the platform binds it to the device its JWT payload
//! names: every request replays `Oasis-Token=<token>; Oasis-Webid=<device_id>`, the
//! device id read out of the token's own payload — the second base64url segment, read
//! without checking any signature; Tidemark is the reader, not the verifier. A token
//! whose payload names no device is refused at build time, where the value can still be
//! fixed. The rate-limit answer runs one of two billing shapes: rolling five-hour and
//! weekly windows (`*_usage_left_rate`, *remaining* fractions, so what is used is their
//! complement), or the credit pool of the Token Plan (`plan_credit_rate_limit`, bucket
//! balances weighted by their sizes). Which shape a payload carries is decided by what
//! it actually contains — a live window means the rolling plan, a credit field means the
//! pool, and the `plan_family` id only breaks ties — upstream's own classification. The
//! pool names when it resets but never the length of its cycle, so its window claims no
//! length: the reset instant is drawn, no pace mark is, and the key stays on the name.
//!
//! Not ported, on purpose: the username/password login with its device registration and
//! INGRESSCOOKIE bootstrap, and the RefreshToken recovery flow — a pasted token only,
//! and a rejected one is reported as rejected rather than refreshed. Upstream's browser
//! `User-Agent` stays off, the shared client owning identity. Two upstream graces read
//! as malformed here: a rate value that is neither a number nor a numeric string
//! upstream decodes as zero, and no card may read "fully used" out of a value it could
//! not read; and a credit-family plan that names no usable credit figure upstream falls
//! through to two exhausted rolling windows.

use super::{HandSpec, Options, ProviderError, redact_query};
use crate::providers::{BoxFuture, Credential, Provider};
use base64::Engine as _;
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "stepfun";

/// The platform host both Dashboard RPCs live on.
const BASE: &str = "https://platform.stepfun.com";
const RATE_LIMIT_PATH: &str = "/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit";
const PLAN_STATUS_PATH: &str = "/api/step.openapi.devcenter.Dashboard/GetStepPlanStatus";
/// The Oasis application id the console's own web client sends.
const APP_ID: &str = "10300";

/// The rolling windows' lengths.
const FIVE_HOURS: u64 = 5 * 60 * 60;
const WEEK: u64 = 7 * 24 * 60 * 60;

/// StepFun as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "StepFun",
    credential: CredentialKind::Key,
    credential_hint: "An Oasis-Token (the Oasis-Token cookie value from platform.stepfun.com).",
    options: &[],
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    _options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(StepFun::new_for_account(
        account,
        credential.expose(),
    )?))
}

/// The token a pasted value holds: trimmed, and a pasted cookie header reduced to its
/// `Oasis-Token` value, upstream's normalizer verbatim.
fn normalize_token(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some((_before, after)) = trimmed.split_once("Oasis-Token=") {
        let end = after.find(';').unwrap_or(after.len());
        return after[..end].trim().to_owned();
    }
    trimmed.to_owned()
}

/// The `device_id` claim in a token's JWT payload, read without checking any signature
/// — Tidemark is the reader, not the verifier. A combined "access...refresh" pair names
/// the device in the refresh half, so the halves are read last-first.
fn device_id(token: &str) -> Option<String> {
    token.rsplit("...").find_map(|half| {
        let payload = half.split('.').nth(1)?.trim_end_matches('=');
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        let claims: Value = serde_json::from_slice(&bytes).ok()?;
        claims
            .get("device_id")?
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

/// One Oasis-Token, bound to the device the token's own payload names.
pub struct StepFun {
    tidemark_account: AccountId,
    client: reqwest::Client,
    /// The platform host, kept as a field so a test can point it at a loopback.
    base: String,
    token: String,
    webid: String,
}

impl StepFun {
    /// Builds the default account against the real platform.
    pub fn new(raw: &str) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), raw)
    }

    fn new_for_account(account_id: AccountId, raw: &str) -> Result<Self, ProviderError> {
        Self::with_base(account_id, BASE, raw)
    }

    #[cfg(test)]
    fn for_test(base: &str, raw: &str) -> Result<Self, ProviderError> {
        Self::with_base(AccountId::default(), base, raw)
    }

    fn with_base(account_id: AccountId, base: &str, raw: &str) -> Result<Self, ProviderError> {
        let token = normalize_token(raw);
        if token.is_empty() {
            return Err(ProviderError::Local(
                "an empty value is not an Oasis-Token; paste the Oasis-Token cookie value \
                 from platform.stepfun.com"
                    .into(),
            ));
        }
        let Some(webid) = device_id(&token) else {
            return Err(ProviderError::Local(
                "the pasted Oasis-Token carries no device_id claim in its JWT payload; copy \
                 the whole current Oasis-Token value from the platform"
                    .into(),
            ));
        };
        Ok(Self {
            tidemark_account: account_id,
            client: super::http::client()?,
            base: base.trim_end_matches('/').to_owned(),
            token,
            webid,
        })
    }

    /// One Dashboard POST: the same shape both RPCs take — an empty JSON body, the
    /// Oasis app headers, and the token with its device as cookies. The platform
    /// answers "token is embezzled" when the two disagree.
    fn post(&self, path: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(format!("{}{path}", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("oasis-appid", APP_ID)
            .header("oasis-platform", "web")
            .header("oasis-webid", &self.webid)
            .header(
                reqwest::header::COOKIE,
                format!("Oasis-Token={}; Oasis-Webid={}", self.token, self.webid),
            )
            .body("{}")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let rate_limit =
            super::request(PROVIDER_ID, &self.client, self.post(RATE_LIMIT_PATH)?).await?;
        let windows = parse_rate_limit(&rate_limit)?;

        // The plan's name is a garnish: upstream asks with `try?`, and a failed ask
        // costs the detail row, not the card.
        let details =
            match super::request(PROVIDER_ID, &self.client, self.post(PLAN_STATUS_PATH)?).await {
                Ok(body) => parse_plan_name(&body)
                    .map(|name| {
                        vec![DetailSection {
                            title: DetailSection::PLAN.to_owned(),
                            rows: vec![DetailRow {
                                label: "Plan".to_owned(),
                                value: name,
                            }],
                        }]
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };

        Ok(Snapshot {
            provider: ProviderId::new(PROVIDER_ID),
            account: self.tidemark_account.clone(),
            captured_at: Timestamp::now(),
            windows,
            details,
        })
    }
}

impl fmt::Debug for StepFun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StepFun")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for StepFun {
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

/// A number the platform may send as a JSON number or a numeric string. A value that is
/// neither reads as absent, never as zero: no card may be drawn from a figure the
/// response did not state.
fn flexible_number(raw: Option<&Value>) -> Option<f64> {
    match raw? {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

/// A whole number the platform may send as a JSON number or a numeric string.
fn flexible_int(raw: Option<&Value>) -> Option<i64> {
    match raw? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Which billing shape the rate-limit answer carries: a live rolling window means the
/// Coding Plan; no live window plus any credit field means the Token Plan's pool; a
/// brand-new plan with neither leaves the `plan_family` id as the only signal, 2 being
/// the credit family. A stale or changed family id cannot flip a plan whose payload
/// plainly carries one shape or the other.
fn is_credit_plan(response: &Map<String, Value>) -> bool {
    let live = flexible_int(response.get("five_hour_usage_reset_time"))
        .is_some_and(|time| time > 0)
        || flexible_int(response.get("weekly_usage_reset_time")).is_some_and(|time| time > 0);
    if live {
        return false;
    }
    let credit = response
        .get("plan_credit_rate_limit")
        .and_then(Value::as_object);
    if credit.is_some_and(|credit| {
        credit.contains_key("subscription_credit_left_rate")
            || credit.contains_key("topup_credit_left_rate")
            || credit
                .get("credit_buckets")
                .and_then(Value::as_array)
                .is_some_and(|buckets| !buckets.is_empty())
    }) {
        return true;
    }
    flexible_number(response.get("plan_family")).is_some_and(|family| family == 2.0)
}

/// Turns a rate-limit body into the card's windows: the credit pool as one window, or
/// the rolling plan's five-hour and weekly pair.
fn parse_rate_limit(body: &str) -> Result<Vec<Window>, ProviderError> {
    let document: Value = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a StepFun rate-limit response: {error}"))
    })?;
    let response = document.as_object().ok_or_else(|| {
        ProviderError::malformed("not a StepFun rate-limit response: the body is not an object")
    })?;

    // The envelope's own verdict: `status` 1, or a message naming the failure.
    if response.get("status").and_then(Value::as_i64) != Some(1) {
        let message = ["message", "desc"]
            .iter()
            .filter_map(|key| {
                response
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            })
            .next()
            .map(str::to_owned)
            .or_else(|| {
                response
                    .get("code")
                    .and_then(Value::as_i64)
                    .map(|code| code.to_string())
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(ProviderError::malformed(format!(
            "the StepFun rate-limit response failed: {message}"
        )));
    }

    if is_credit_plan(response) {
        return Ok(vec![credit_window(response)?]);
    }
    rolling_windows(response)
}

/// The credit pool as one window: the buckets' absolute balances weighted by their
/// sizes when every bucket carries both figures, else the subscription rate — the
/// primary allowance — or, without one, the top-up rate. The wire names when the pool
/// resets but never the length of its cycle, so the window claims no length and draws
/// no pace mark; the length a card cannot read is not one to guess at thirty days.
fn credit_window(response: &Map<String, Value>) -> Result<Window, ProviderError> {
    let credit = response
        .get("plan_credit_rate_limit")
        .and_then(Value::as_object);
    let rate = credit.and_then(total_credit_left_rate).ok_or_else(|| {
        ProviderError::malformed(
            "the StepFun response names a credit plan without a usable credit figure",
        )
    })?;
    let reset = credit
        .and_then(|credit| flexible_int(credit.get("subscription_credit_reset_time")))
        .and_then(reset_at);
    Ok(Window {
        // A pool with no stated length keys on its name — the length-derived keys are for
        // windows whose span the wire itself states.
        key: WindowKey::named("credit"),
        title: "Credit".into(),
        subtitle: None,
        used_percent: used_from_rate(rate),
        resets_at: reset,
        length: None,
    })
}

/// The pool's remaining fraction, upstream's reading: bucket balances weighted by size
/// only when all the buckets are fully described, else the subscription rate, else the
/// top-up rate. The left-rates themselves are never summed — they are independent
/// fractions of independent allowances.
fn total_credit_left_rate(credit: &Map<String, Value>) -> Option<f64> {
    if let Some(buckets) = credit.get("credit_buckets").and_then(Value::as_array)
        && !buckets.is_empty()
    {
        let balances: Vec<(f64, f64)> = buckets
            .iter()
            .filter_map(|bucket| {
                let bucket = bucket.as_object()?;
                let total = flexible_number(bucket.get("credit_total"))?;
                let residual = flexible_number(bucket.get("credit_residual"))?;
                (total > 0.0 && (0.0..=total).contains(&residual)).then_some((total, residual))
            })
            .collect();
        if balances.len() == buckets.len() {
            let total: f64 = balances.iter().map(|(total, _)| total).sum();
            let residual: f64 = balances.iter().map(|(_, residual)| residual).sum();
            return Some(residual / total);
        }
    }
    flexible_number(credit.get("subscription_credit_left_rate"))
        .or_else(|| flexible_number(credit.get("topup_credit_left_rate")))
}

/// The rolling plan's two windows. All four figures must be stated: an absent one is
/// not a zero, and a zero rate drawn from nothing would read as fully spent.
fn rolling_windows(response: &Map<String, Value>) -> Result<Vec<Window>, ProviderError> {
    let five_rate = flexible_number(response.get("five_hour_usage_left_rate"));
    let week_rate = flexible_number(response.get("weekly_usage_left_rate"));
    let five_reset = flexible_int(response.get("five_hour_usage_reset_time"));
    let week_reset = flexible_int(response.get("weekly_usage_reset_time"));
    let (Some(five_rate), Some(week_rate), Some(five_reset), Some(week_reset)) =
        (five_rate, week_rate, five_reset, week_reset)
    else {
        return Err(ProviderError::malformed(
            "the StepFun rate-limit response is missing a usage rate or reset time",
        ));
    };
    Ok(vec![
        rolling_window("5h", FIVE_HOURS, five_rate, five_reset),
        rolling_window("7-day", WEEK, week_rate, week_reset),
    ])
}

fn rolling_window(title: &str, length: u64, rate: f64, reset: i64) -> Window {
    Window {
        key: WindowKey::for_length(WindowLength::from_secs(length).expect("a fixed span")),
        title: title.to_owned(),
        subtitle: None,
        used_percent: used_from_rate(rate),
        resets_at: reset_at(reset),
        length: WindowLength::from_secs(length),
    }
}

/// The complement of a *remaining* fraction, as the card reads: 0..=100.
fn used_from_rate(rate: f64) -> f64 {
    ((1.0 - rate) * 100.0).clamp(0.0, 100.0)
}

/// The reset a "0" timestamp stands for: none. A window the platform says has no reset
/// configured is not one that ran out in 1970.
fn reset_at(reset: i64) -> Option<Timestamp> {
    (reset > 0)
        .then_some(reset)
        .and_then(|reset| Timestamp::from_unix(reset).ok())
}

/// The subscription's display name, when the plan-status answer carries one. The
/// envelope's `status` is not consulted, exactly as upstream reads it: whatever name it
/// carries is the account's.
fn parse_plan_name(body: &str) -> Option<String> {
    let document: Value = serde_json::from_str(body).ok()?;
    let name = document.get("subscription")?.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// The recorded `parses real API response format with string timestamps and integer
    /// rates` body: what the platform actually sends — string reset times, an integer
    /// rate beside a float one.
    const RATE_LIMIT: &str = include_str!("../../../tests/fixtures/stepfun/rate-limit.json");
    /// The recorded plan-status answer naming the subscription.
    const PLAN_STATUS: &str = include_str!("../../../tests/fixtures/stepfun/plan-status.json");
    /// The recorded real Mini-plan body: `plan_family` 2, the rolling fields at zero,
    /// and one credit bucket carrying absolute balances as numeric strings.
    const MINI_PLAN: &str = r#"{
        "status": 1,
        "five_hour_usage_left_rate": 0,
        "five_hour_usage_reset_time": "0",
        "weekly_usage_left_rate": 0,
        "weekly_usage_reset_time": "0",
        "plan_family": 2,
        "plan_credit_rate_limit": {
            "subscription_credit_left_rate": 0.9641096,
            "subscription_credit_reset_time": "1786288293",
            "topup_credit_left_rate": 0,
            "credit_buckets": [
                {
                    "type": 1,
                    "credit_total": "400000000",
                    "credit_residual": "385643853",
                    "expire_at": "1792416128",
                    "next_reset_at": "1786288293"
                }
            ]
        }
    }"#;
    /// The recorded `zero credit reset does not activate monthly pace` body.
    const NO_CREDIT_RESET: &str = r#"{
        "status": 1,
        "plan_family": 2,
        "plan_credit_rate_limit": {
            "subscription_credit_left_rate": 0.5,
            "subscription_credit_reset_time": "0"
        }
    }"#;
    /// The recorded `weights mixed subscription and top-up credit buckets` body.
    const MIXED_BUCKETS: &str = r#"{
        "status": 1,
        "plan_family": 2,
        "plan_credit_rate_limit": {
            "subscription_credit_left_rate": 0.8,
            "topup_credit_left_rate": 0.5,
            "credit_buckets": [
                { "credit_total": "100", "credit_residual": "80" },
                { "credit_total": "300", "credit_residual": "150" }
            ]
        }
    }"#;
    /// The recorded `falls back to the subscription rate for incomplete credit
    /// buckets` body: a bucket without a residual cannot be weighted.
    const INCOMPLETE_BUCKETS: &str = r#"{
        "status": 1,
        "plan_family": 2,
        "plan_credit_rate_limit": {
            "subscription_credit_left_rate": 0.6,
            "topup_credit_left_rate": 0.4,
            "credit_buckets": [
                { "credit_total": "100" }
            ]
        }
    }"#;
    /// The recorded `classifies exhausted zero-credit pool without a family id` body.
    const EXHAUSTED_POOL: &str = r#"{
        "status": 1,
        "five_hour_usage_left_rate": 0,
        "five_hour_usage_reset_time": "0",
        "weekly_usage_left_rate": 0,
        "weekly_usage_reset_time": "0",
        "plan_credit_rate_limit": {
            "subscription_credit_left_rate": 0,
            "subscription_credit_reset_time": "1786288293"
        }
    }"#;
    /// The recorded `classifies zero-credit pool when only top-up field is present` body.
    const TOPUP_ONLY_POOL: &str = r#"{
        "status": 1,
        "five_hour_usage_left_rate": 0,
        "five_hour_usage_reset_time": "0",
        "weekly_usage_left_rate": 0,
        "weekly_usage_reset_time": "0",
        "plan_credit_rate_limit": {
            "topup_credit_left_rate": 0
        }
    }"#;
    /// The recorded `live rolling windows win over a credit-family id` body.
    const LIVE_WINDOWS_AND_FAMILY: &str = r#"{
        "status": 1,
        "five_hour_usage_left_rate": 0.8,
        "five_hour_usage_reset_time": "1746000000",
        "weekly_usage_left_rate": 0.6,
        "weekly_usage_reset_time": "1746500000",
        "plan_family": 2,
        "plan_credit_rate_limit": { "subscription_credit_left_rate": 1, "credit_buckets": [] }
    }"#;
    /// The recorded `falls back to the credit-family id only when the payload is
    /// otherwise ambiguous` bodies: a brand-new plan with neither a live window nor a
    /// credit pool yet, under each family id.
    const CREDIT_FAMILY: &str = r#"{"status":1,"five_hour_usage_left_rate":0,"five_hour_usage_reset_time":"0","weekly_usage_left_rate":0,"weekly_usage_reset_time":"0","plan_family":2}"#;
    const WINDOW_FAMILY: &str = r#"{"status":1,"five_hour_usage_left_rate":0,"five_hour_usage_reset_time":"0","weekly_usage_left_rate":0,"weekly_usage_reset_time":"0","plan_family":1}"#;
    /// The recorded `throws on failed API status` body.
    const STATUS_ZERO: &str = r#"{
        "status": 0,
        "message": "Unauthorized",
        "five_hour_usage_left_rate": 0.75,
        "weekly_usage_left_rate": 0.5,
        "five_hour_usage_reset_time": "1746000000",
        "weekly_usage_reset_time": "1746500000"
    }"#;

    const RATE_LIMIT_REQUEST: &str =
        "POST /api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit";
    const PLAN_STATUS_REQUEST: &str =
        "POST /api/step.openapi.devcenter.Dashboard/GetStepPlanStatus";

    /// A loopback server answering the given routes in order, asserting each request
    /// opens with its expected request line and handing the raw exchange back.
    fn chained_server(
        routes: Vec<(&'static str, u16, String)>,
    ) -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for (expected, status, body) in routes {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("reads request line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    request.push_str(&line);
                }
                drop(reader);
                assert!(
                    request.starts_with(expected),
                    "expected {expected}, got: {request}"
                );
                request_tx.send(request).expect("sends request");
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("writes response");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn route(expected: &'static str, status: u16, body: &str) -> (&'static str, u16, String) {
        (expected, status, body.to_owned())
    }

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    /// A syntactically valid token whose payload names `device`, upstream's own test
    /// spelling: header, base64url payload, signature — none of it checked.
    fn jwt_with_device(device: &str) -> String {
        let payload = serde_json::json!({ "device_id": device }).to_string();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.signature")
    }

    fn fetch(provider: &StepFun) -> Result<Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    #[test]
    fn the_recorded_rate_limit_body_draws_the_two_rolling_windows() {
        let windows = parse_rate_limit(RATE_LIMIT).expect("parses the recorded body");

        assert_eq!(windows.len(), 2);
        let five = &windows[0];
        assert_eq!(
            five.key,
            WindowKey::for_length(WindowLength::from_secs(18_000).expect("a fixed span"))
        );
        assert_eq!(five.title, "5h");
        assert_eq!(
            five.length,
            Some(WindowLength::from_secs(18_000).expect("a fixed span"))
        );
        assert!(five.used_percent.abs() < 0.000_001, "{}", five.used_percent);
        assert_eq!(five.resets_at, Some(at(1_777_528_800)));

        let week = &windows[1];
        assert_eq!(week.title, "7-day");
        assert_eq!(
            week.key,
            WindowKey::for_length(WindowLength::from_secs(604_800).expect("a fixed span"))
        );
        assert!(
            (week.used_percent - 0.218_457).abs() < 0.000_001,
            "{}",
            week.used_percent
        );
        assert_eq!(week.resets_at, Some(at(1_777_899_600)));
    }

    #[test]
    fn a_credit_pool_draws_one_credit_window_at_its_stated_reset_and_no_invented_length() {
        // The wire names when the pool resets but never the length of its cycle; a
        // 30-day length assumed from the reset's presence would draw a pace mark the
        // platform never stated.
        let windows = parse_rate_limit(MINI_PLAN).expect("parses the recorded Mini-plan body");

        assert_eq!(windows.len(), 1);
        let credit = &windows[0];
        assert_eq!(credit.title, "Credit");
        assert_eq!(credit.key, WindowKey::named("credit"));
        // 385643853 of 400000000 credits left: ~3.59% used, not the ~96% the raw
        // left-rate would misread.
        assert!(
            (credit.used_percent - (1.0 - 0.964_109_632_5) * 100.0).abs() < 0.000_001,
            "{}",
            credit.used_percent
        );
        assert_eq!(credit.resets_at, Some(at(1_786_288_293)));
        assert_eq!(credit.length, None);
    }

    #[test]
    fn a_credit_pool_without_a_reset_keeps_its_window_but_not_its_pace() {
        let windows = parse_rate_limit(NO_CREDIT_RESET).expect("parses");

        assert_eq!(windows.len(), 1);
        assert!((windows[0].used_percent - 50.0).abs() < 0.000_001);
        assert_eq!(windows[0].resets_at, None);
        assert_eq!(windows[0].length, None);
    }

    #[test]
    fn mixed_credit_buckets_are_weighted_by_their_balances() {
        // (80 + 150) / (100 + 300) left: 42.5% used — the two left-rates must not be
        // summed, they are independent fractions.
        let windows = parse_rate_limit(MIXED_BUCKETS).expect("parses");

        assert_eq!(windows.len(), 1);
        assert!((windows[0].used_percent - 42.5).abs() < 0.000_001);
    }

    #[test]
    fn incomplete_credit_buckets_fall_back_to_the_subscription_rate() {
        let windows = parse_rate_limit(INCOMPLETE_BUCKETS).expect("parses");

        assert_eq!(windows.len(), 1);
        assert!((windows[0].used_percent - 40.0).abs() < 0.000_001);
    }

    #[test]
    fn an_exhausted_credit_pool_reads_as_fully_used() {
        for body in [EXHAUSTED_POOL, TOPUP_ONLY_POOL] {
            let windows = parse_rate_limit(body).expect("parses");
            assert_eq!(windows.len(), 1, "{body}");
            assert!(
                (windows[0].used_percent - 100.0).abs() < 0.000_001,
                "{body}"
            );
        }
    }

    #[test]
    fn live_rolling_windows_win_over_a_credit_family_id() {
        // A stale or changed family id must never send a windowed plan to the credit
        // renderer, which would drop the real windows.
        let windows = parse_rate_limit(LIVE_WINDOWS_AND_FAMILY).expect("parses");

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].title, "5h");
        assert!((windows[0].used_percent - 20.0).abs() < 0.000_001);
        assert_eq!(windows[1].title, "7-day");
        assert!((windows[1].used_percent - 40.0).abs() < 0.000_001);
    }

    #[test]
    fn an_ambiguous_credit_family_plan_without_a_pool_is_malformed_here() {
        // Upstream falls through to two exhausted rolling windows; a card without a
        // number it actually read is malformed here.
        let error = parse_rate_limit(CREDIT_FAMILY).expect_err("no usable credit figure");
        assert!(error.to_string().contains("credit"), "{error}");
    }

    #[test]
    fn a_window_family_plan_still_draws_its_two_exhausted_windows() {
        // The fields are present and say zero: fully used, with no reset to name.
        let windows = parse_rate_limit(WINDOW_FAMILY).expect("parses");

        assert_eq!(windows.len(), 2);
        assert!((windows[0].used_percent - 100.0).abs() < 0.000_001);
        assert_eq!(windows[0].resets_at, None);
        assert!((windows[1].used_percent - 100.0).abs() < 0.000_001);
    }

    #[test]
    fn a_failed_status_envelope_names_its_message() {
        let error = parse_rate_limit(STATUS_ZERO).expect_err("the platform refused");
        assert!(error.to_string().contains("Unauthorized"), "{error}");
    }

    #[test]
    fn a_missing_usage_field_fails_rather_than_reading_zero() {
        let error = parse_rate_limit(r#"{"status": 1}"#).expect_err("no rate was reported");
        assert!(
            error.to_string().contains("usage rate or reset time"),
            "{error}"
        );
    }

    #[test]
    fn an_oasis_token_cookie_paste_is_normalized_to_its_token() {
        assert_eq!(
            normalize_token("Oasis-Token=abc123...def456; Oasis-Webid=someid"),
            "abc123...def456"
        );
        assert_eq!(normalize_token("Oasis-Token=abc123"), "abc123");
        assert_eq!(normalize_token("  raw-token  "), "raw-token");
        assert_eq!(normalize_token("   "), "");
    }

    #[test]
    fn a_token_whose_jwt_names_a_device_builds_and_one_without_is_refused() {
        let error = (SPEC.build)(
            AccountId::default(),
            Credential::new("not a jwt at all"),
            &Options::new(),
        )
        .expect_err("no device claim to bind");
        assert!(
            matches!(error, ProviderError::Local(ref message) if message.contains("device_id")),
            "{error:?}"
        );

        let error = (SPEC.build)(
            AccountId::default(),
            Credential::new("   "),
            &Options::new(),
        )
        .expect_err("an empty paste is not a token");
        assert!(matches!(error, ProviderError::Local(_)), "{error:?}");

        assert!(
            (SPEC.build)(
                AccountId::default(),
                Credential::new(jwt_with_device("device-7")),
                &Options::new()
            )
            .is_ok(),
            "a token naming its device builds"
        );
    }

    #[test]
    fn the_webid_comes_from_the_refresh_half_of_a_token_pair() {
        // The combined "access...refresh" spelling: the device_id lives in the refresh
        // half, so it is read there first.
        let provider = StepFun::for_test(
            "http://127.0.0.1:9",
            &format!("access...{}", jwt_with_device("device-7")),
        )
        .expect("builds");

        let request = provider.post(RATE_LIMIT_PATH).expect("the request builds");
        let cookie = request
            .headers()
            .get(reqwest::header::COOKIE)
            .expect("present")
            .to_str()
            .expect("header text");
        assert!(cookie.starts_with("Oasis-Token=access..."), "{cookie}");
        assert!(cookie.ends_with("; Oasis-Webid=device-7"), "{cookie}");
    }

    #[test]
    fn the_rate_limit_request_carries_the_token_cookie_and_oasis_headers() {
        let token = jwt_with_device("device-7");
        let provider = StepFun::for_test("http://127.0.0.1:9", &token).expect("builds");

        let request = provider.post(RATE_LIMIT_PATH).expect("the request builds");
        assert_eq!(request.method(), reqwest::Method::POST);
        assert!(
            request.url().as_str().starts_with(
                "http://127.0.0.1:9/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit"
            ),
            "{}",
            request.url()
        );
        let headers = request.headers();
        assert_eq!(
            headers.get(reqwest::header::CONTENT_TYPE).expect("present"),
            "application/json"
        );
        assert_eq!(headers.get("oasis-appid").expect("present"), "10300");
        assert_eq!(headers.get("oasis-platform").expect("present"), "web");
        assert_eq!(headers.get("oasis-webid").expect("present"), "device-7");
        assert_eq!(
            headers
                .get(reqwest::header::COOKIE)
                .expect("present")
                .to_str()
                .expect("header text"),
            format!("Oasis-Token={token}; Oasis-Webid=device-7")
        );
        let body = request
            .body()
            .expect("present")
            .as_bytes()
            .expect("in memory");
        assert_eq!(body, br#"{}"#);
    }

    #[test]
    fn the_wire_requests_hit_both_endpoints_and_carry_the_cookie() {
        let (base, requests, server) = chained_server(vec![
            route(RATE_LIMIT_REQUEST, 200, RATE_LIMIT),
            route(PLAN_STATUS_REQUEST, 200, PLAN_STATUS),
        ]);
        let provider = StepFun::for_test(&base, &jwt_with_device("device-7")).expect("builds");

        let snapshot = fetch(&provider).expect("both endpoints answer");
        server.join().expect("server exits");

        let request = requests.recv().expect("rate-limit request");
        assert!(request.contains("cookie: Oasis-Token="), "{request}");
        assert!(request.contains("oasis-appid: 10300"), "{request}");
        assert!(request.contains("oasis-platform: web"), "{request}");
        assert!(request.contains("oasis-webid: device-7"), "{request}");
        assert!(!requests.recv().expect("plan-status request").is_empty());

        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, DetailSection::PLAN);
        assert_eq!(snapshot.details[0].rows[0].value, "Plus");
    }

    #[test]
    fn a_failed_plan_status_call_keeps_the_windows() {
        // Upstream asks for the name with `try?`: the card outlives a failed ask.
        let (base, _requests, server) = chained_server(vec![
            route(RATE_LIMIT_REQUEST, 200, RATE_LIMIT),
            route(PLAN_STATUS_REQUEST, 500, r#"{"error":"temporary"}"#),
        ]);
        let provider = StepFun::for_test(&base, &jwt_with_device("device-7")).expect("builds");

        let snapshot = fetch(&provider).expect("the windows survive");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 2);
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn a_rejected_token_is_a_credential_error_that_asks_no_plan_status() {
        let (base, requests, server) = chained_server(vec![route(
            RATE_LIMIT_REQUEST,
            401,
            r#"{"error":"unauthorized"}"#,
        )]);
        let provider = StepFun::for_test(&base, &jwt_with_device("device-7")).expect("builds");

        let result = fetch(&provider);
        server.join().expect("server exits");

        let error = result.expect_err("the token was rejected");
        assert!(
            matches!(error, ProviderError::Credential { status: 401 }),
            "{error}"
        );
        let request = requests.recv().expect("the one request");
        assert!(request.contains("oasis-appid: 10300"), "{request}");
        assert!(
            requests.try_recv().is_err(),
            "the plan status was never asked"
        );
    }
}
