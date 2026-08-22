//! Factory, read through its API-key ladder.
//!
//! Ported from CodexBar's `Providers/Factory/FactoryStatusProbe.swift` and
//! `FactoryStatusProbe+APIKey.swift`; the recorded bodies in
//! `FactoryAPIKeyUsageTests.swift`, `FactoryStatusProbeFetchTests.swift`,
//! `FactoryStatusProbeTests.swift` and `FactoryProviderImplementationTests.swift` are
//! the contract. Never seen answering: every number in the tests below is a body or a
//! test input CodexBar recorded.
//!
//! # The ladder
//!
//! A Factory API key (`fk-…`, the `FACTORY_API_KEY` CodexBar reads from the
//! environment) rides `Authorization: Bearer` against `api.factory.ai`, three rungs:
//!
//! 1. `GET /api/app/auth/me` names the organization, its tier and plan, and the user
//!    profile's `id`. A 401 there is the source's `notLoggedIn`; the shared transport
//!    maps it — and 403 — to a credential rejection, which is where the source's own
//!    preferred-error logic ends up for the key path.
//! 2. `GET /api/billing/limits` is read **softly**: any failure — transport, status, a
//!    body that will not decode — is no answer at all and the ladder falls through,
//!    exactly as the source's `catch { return nil }`, its `guard statusCode == 200`
//!    and its `try?` decode do. When it does answer with
//!    `usesTokenRateLimitsBilling` and a `limits` object, that answer is the whole
//!    card and the third rung never runs.
//! 3. `GET /api/organization/subscription/usage?useCache=true` — plus `userId` when
//!    the first rung, or the key itself, named one — reads the billing-period
//!    reading: standard and premium tokens against their allowances.
//!
//! CodexBar retries the whole ladder against `app.factory.ai` when the API host
//! fails, keeping a 401/403 from the API host preferred over the later noise. This
//! port speaks only to `api.factory.ai` — the host every key-path request in
//! CodexBar's recorded tests hits, and hits first — and stops at the first failure
//! rather than retrying it elsewhere: a credential rejection must not be buried under
//! a second host's error, and the shared transport has already refused it by then.
//!
//! # The user id out of the key
//!
//! When auth/me carries no `userProfile.id`, the source decodes the JWT payload of
//! the bearer token itself and takes `sub` as the user id — IBM Bob's precedent: the
//! sniff reads the token and answers one string, and the key itself never reaches a
//! log, a Debug, or an error. An `fk-…` key is not a JWT and simply names no user.
//!
//! # The two cards
//!
//! The token-rate-limits card draws up to six windows: the standard pool's
//! five-hour, weekly and monthly windows, and — when the core pool reports any usage
//! data at all — the same three for `core`, keyed by pool so two windows of one
//! length stay distinct. The monthly windows state no length the source passes on,
//! so they are keyed by name with none. A window resets at `secondsRemaining` from
//! now, else at `windowEnd` if that is still ahead; Factory can leave stale values
//! behind after a short window expires, and the web UI treats that state as reset —
//! so does [`effective_used_percent`]. The extra usage balance arrives in cents and
//! renders as the dollars CodexBar's own menu spells ("Extra usage balance:
//! $25.00").
//!
//! The billing-period card draws two windows — standard and premium — each a quantity
//! of tokens against a stated allowance, reset at the period's end. The source
//! prefers the API's own `usedRatio` over the local division, with the recorded
//! escalations: a ratio of zero alongside real local readings is a lie the local
//! division overrules; a ratio off the 0..1 scale is tried as a percent only when the
//! allowance is missing or beyond the unlimited threshold; an allowance past a
//! trillion tokens is unlimited and is measured against a hundred-million-token
//! reference. A billing period states no fixed span, so both windows are keyed by
//! name with no length.
//!
//! # What is fixed
//!
//! Every request carries the browser headers the source sends — `Origin`,
//! `Referer: https://app.factory.ai/`, `x-factory-client: web-app` — because this API
//! is the web app's backend; the query order `useCache` then `userId` is the recorded
//! trace's order. Period dates arrive in milliseconds and are read leniently: an
//! unreadable or implausible one is no date, not a failure, as everywhere else in
//! this workspace.
//!
//! # What ships untested
//!
//! No recorded body carries a `windowEnd` — every recorded billing window states
//! `secondsRemaining` — so the flexible date reader (number, numeric string or
//! ISO-8601, milliseconds past a trillion) and the stale-window reset rule ship
//! untested. No recorded auth body carries a `userProfile.id`, so the auth-first
//! precedence over the JWT sniff is ported from the source's `??` and never
//! exercised. The app-host retry is dropped rather than ported, as above. The 401 a
//! rejected key earns is mapped by the shared transport, so it is tested by no unit
//! here.

use super::{HandSpec, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "factory";

/// The first rung: who the key speaks for.
const AUTH_URL: &str = "https://api.factory.ai/api/app/auth/me";

/// The second rung, read softly.
const LIMITS_URL: &str = "https://api.factory.ai/api/billing/limits";

/// The third rung: the billing-period reading.
const USAGE_URL: &str = "https://api.factory.ai/api/organization/subscription/usage";

/// The five-hour window's length, the source's own `5 * 60`.
const FIVE_HOURS: u64 = 5 * 60 * 60;

/// The weekly window's length, the source's own `7 * 24 * 60`.
const WEEK: u64 = 7 * 24 * 60 * 60;

/// An allowance past this many tokens reads as unlimited, the source's own threshold.
const UNLIMITED_ABOVE: i64 = 1_000_000_000_000;

/// Factory as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Factory",
    credential: CredentialKind::Key,
    credential_hint: "app.factory.ai → Settings → API keys. The Factory API key CodexBar calls FACTORY_API_KEY.",
    options: &[],
    build,
};

/// Builds a pollable client from the stored key. Factory exposes no settings: one
/// host, one ladder.
fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Factory::new(credential)?))
}

/// One Factory account: the key, against the one host the ladder lives on.
pub struct Factory {
    client: reqwest::Client,
    credential: Credential,
}

impl Factory {
    /// Builds a client.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    /// One rung's GET, built but not sent, so the URL, the placement of the key and
    /// the browser headers are testable without a server.
    fn get(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, "https://app.factory.ai")
            .header(reqwest::header::REFERER, "https://app.factory.ai/")
            .header("x-factory-client", "web-app")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The auth/me request, the ladder's first rung.
    fn auth_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(AUTH_URL)
    }

    /// The billing/limits request, the soft second rung.
    fn limits_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(LIMITS_URL)
    }

    /// The usage request, the third rung, with the query the recorded trace spells:
    /// `useCache` always, `userId` when someone was identified.
    fn usage_request(&self, user_id: Option<&str>) -> Result<reqwest::Request, ProviderError> {
        let mut query: Vec<(&str, &str)> = vec![("useCache", "true")];
        if let Some(user_id) = user_id {
            query.push(("userId", user_id));
        }
        self.client
            .get(USAGE_URL)
            .query(&query)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, "https://app.factory.ai")
            .header(reqwest::header::REFERER, "https://app.factory.ai/")
            .header("x-factory-client", "web-app")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The soft second rung: any failure — the request, the status, the body — is no
    /// answer, and the ladder falls through to usage.
    async fn limits(&self) -> Option<BillingReading> {
        let body = super::request(&self.client, self.limits_request().ok()?)
            .await
            .ok()?;
        parse_limits(&body)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let body = super::request(&self.client, self.auth_request()?).await?;
        let auth = parse_auth(&body)?;
        // The source's own precedence: the profile's id, else the JWT inside the key.
        // The sniff is the last thing the key is used for besides the wire.
        let user_id = auth
            .user_id
            .clone()
            .or_else(|| jwt_subject(self.credential.expose()));
        if let Some(reading) = self.limits().await {
            return Ok(rate_limits_snapshot(&auth, &reading, now));
        }
        let body = super::request(&self.client, self.usage_request(user_id.as_deref())?).await?;
        let usage = parse_usage(&body)?;
        Ok(classic_snapshot(&auth, &usage, now))
    }
}

impl fmt::Debug for Factory {
    /// Written by hand: a derived impl would print the credential the first time
    /// anything traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Factory")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Factory {
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

/// Who the key speaks for, as the first rung reports it.
#[derive(Debug, Clone, PartialEq)]
struct Auth {
    organization: Option<String>,
    tier: Option<String>,
    plan: Option<String>,
    user_id: Option<String>,
}

/// The soft rung's answer, when it has one.
#[derive(Debug, Clone, PartialEq)]
struct BillingReading {
    standard: Pool,
    core: Option<Pool>,
    extra_usage_balance_cents: i64,
    overage_preference: Option<String>,
}

/// One pool of three windows.
#[derive(Debug, Clone, PartialEq)]
struct Pool {
    five_hour: BillingWindow,
    weekly: BillingWindow,
    monthly: BillingWindow,
}

impl Pool {
    /// The source's `hasUsageData`: any window carrying a percent, an end or a
    /// countdown makes the pool worth drawing.
    fn has_usage_data(&self) -> bool {
        [&self.five_hour, &self.weekly, &self.monthly]
            .into_iter()
            .any(|window| {
                window.used_percent > 0.0
                    || window.window_end.is_some()
                    || window.seconds_remaining.is_some()
            })
    }
}

/// One window of a pool, as the billing body states it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BillingWindow {
    used_percent: f64,
    window_end: Option<Timestamp>,
    seconds_remaining: Option<f64>,
}

/// When the window rolls over: the countdown from now when the body gives one,
/// else the stated end if it is still ahead. `None` is a window already gone.
fn reset_at(window: &BillingWindow, now: Timestamp) -> Option<Timestamp> {
    if let Some(seconds) = window.seconds_remaining
        && seconds > 0.0
    {
        return Some(now.saturating_add_seconds(seconds as i64));
    }
    match window.window_end {
        Some(end) if end.as_unix() > now.as_unix() => Some(end),
        _ => None,
    }
}

/// The percent to draw: the source's stale-window rule — a window whose reset has
/// passed leaving only a stated end reads as reset, 0 percent — then the stated
/// percent clamped to the card's scale.
fn effective_used_percent(window: &BillingWindow, now: Timestamp) -> f64 {
    if reset_at(window, now).is_none()
        && window.window_end.is_some()
        && window.seconds_remaining.is_none()
    {
        return 0.0;
    }
    window.used_percent.clamp(0.0, 100.0)
}

/// The third rung's reading.
#[derive(Debug, Clone, PartialEq)]
struct Usage {
    period_start: Option<Timestamp>,
    period_end: Option<Timestamp>,
    standard: TokenUsage,
    premium: TokenUsage,
}

/// One pool's tokens, defaulted to zero as the source's `?? 0` defaults them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TokenUsage {
    user_tokens: i64,
    org_total_tokens_used: i64,
    total_allowance: i64,
    used_ratio: Option<f64>,
}

impl TokenUsage {
    fn of(raw: Option<&TokenUsageBody>) -> Self {
        Self {
            user_tokens: raw.and_then(|raw| raw.user_tokens).unwrap_or(0),
            org_total_tokens_used: raw.and_then(|raw| raw.org_total_tokens_used).unwrap_or(0),
            total_allowance: raw.and_then(|raw| raw.total_allowance).unwrap_or(0),
            used_ratio: raw.and_then(|raw| raw.used_ratio),
        }
    }
}

// The bodies, as serde reads them: every field the Swift decoders mark optional is
// optional here, and a value of the wrong JSON type fails the field, as a Swift
// decoder throws.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthMeBody {
    organization: Option<OrganizationBody>,
    user_profile: Option<UserProfileBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrganizationBody {
    name: Option<String>,
    subscription: Option<SubscriptionBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionBody {
    factory_tier: Option<String>,
    orb_subscription: Option<OrbSubscriptionBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrbSubscriptionBody {
    plan: Option<PlanBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanBody {
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserProfileBody {
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingBody {
    #[serde(default)]
    uses_token_rate_limits_billing: bool,
    limits: Option<LimitsBody>,
    #[serde(default)]
    extra_usage_balance_cents: i64,
    overage_preference: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LimitsBody {
    standard: PoolBody,
    core: Option<PoolBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolBody {
    five_hour: WindowBody,
    weekly: WindowBody,
    monthly: WindowBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowBody {
    used_percent: f64,
    window_end: Option<Value>,
    seconds_remaining: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageBody {
    usage: Option<UsageDataBody>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UsageDataBody {
    start_date: Option<i64>,
    end_date: Option<i64>,
    standard: Option<TokenUsageBody>,
    premium: Option<TokenUsageBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsageBody {
    user_tokens: Option<i64>,
    org_total_tokens_used: Option<i64>,
    total_allowance: Option<i64>,
    used_ratio: Option<f64>,
}

/// Reads the first rung's body. Pure.
fn parse_auth(body: &str) -> Result<Auth, ProviderError> {
    let raw: AuthMeBody = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a Factory auth response: {e}")))?;
    Ok(Auth {
        organization: normalized(
            raw.organization
                .as_ref()
                .and_then(|org| org.name.as_deref()),
        ),
        tier: normalized(
            raw.organization
                .as_ref()
                .and_then(|org| org.subscription.as_ref())
                .and_then(|subscription| subscription.factory_tier.as_deref()),
        ),
        plan: normalized(
            raw.organization
                .as_ref()
                .and_then(|org| org.subscription.as_ref())
                .and_then(|subscription| subscription.orb_subscription.as_ref())
                .and_then(|orb| orb.plan.as_ref())
                .and_then(|plan| plan.name.as_deref()),
        ),
        user_id: normalized(
            raw.user_profile
                .as_ref()
                .and_then(|profile| profile.id.as_deref()),
        ),
    })
}

/// Reads the soft rung's body. Pure, and soft in the same breath: a body that will
/// not decode, one that declines the new billing, or one without its limits object is
/// no answer — `None` — and a window that cannot be read fails the whole read the
/// same way, because the Swift decoder's throw lands in the source's `try?`.
fn parse_limits(body: &str) -> Option<BillingReading> {
    let raw: BillingBody = serde_json::from_str(body).ok()?;
    if !raw.uses_token_rate_limits_billing {
        return None;
    }
    let raw_limits = raw.limits?;
    let standard = read_pool(&raw_limits.standard)?;
    let core = match raw_limits.core.as_ref() {
        None => None,
        Some(raw_pool) => Some(read_pool(raw_pool)?),
    };
    Some(BillingReading {
        standard,
        core,
        extra_usage_balance_cents: raw.extra_usage_balance_cents,
        overage_preference: normalized(raw.overage_preference.as_deref()),
    })
}

/// One pool of the soft body.
fn read_pool(raw: &PoolBody) -> Option<Pool> {
    Some(Pool {
        five_hour: read_window(&raw.five_hour)?,
        weekly: read_window(&raw.weekly)?,
        monthly: read_window(&raw.monthly)?,
    })
}

/// One window of the soft body: the percent as stated, the countdown as stated, and
/// the end through the flexible reader — whose failure is the pool's failure.
fn read_window(raw: &WindowBody) -> Option<BillingWindow> {
    Some(BillingWindow {
        used_percent: raw.used_percent,
        seconds_remaining: raw.seconds_remaining,
        window_end: raw
            .window_end
            .as_ref()
            .filter(|value| !value.is_null())
            .and_then(window_date),
    })
}

/// The source's `FlexibleFactoryDate`: Unix seconds as a number or a numeric string
/// (milliseconds instead past a trillion), else an ISO-8601 string. `None` is a value
/// that is none of these — which, for this soft rung, is no answer.
fn window_date(value: &Value) -> Option<Timestamp> {
    match value {
        Value::Number(number) => number.as_f64().and_then(epoch_of),
        Value::String(raw) => {
            let raw = raw.trim();
            raw.parse::<f64>()
                .ok()
                .and_then(epoch_of)
                .or_else(|| parse_rfc3339(raw))
        }
        _ => None,
    }
}

/// Seconds or milliseconds past the epoch — the trillion cutoff is the source's own —
/// as a plausible instant or nothing.
fn epoch_of(seconds: f64) -> Option<Timestamp> {
    let seconds = if seconds > 1e12 {
        seconds / 1000.0
    } else {
        seconds
    };
    Timestamp::from_unix(seconds as i64).ok()
}

/// Reads the third rung's body. Pure. The period's dates are milliseconds, read
/// leniently: an implausible one is no date, not a failure.
fn parse_usage(body: &str) -> Result<Usage, ProviderError> {
    let raw: UsageBody = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a Factory usage response: {e}")))?;
    let usage = raw.usage.unwrap_or_default();
    Ok(Usage {
        period_start: usage
            .start_date
            .and_then(|ms| Timestamp::from_unix_millis(ms).ok()),
        period_end: usage
            .end_date
            .and_then(|ms| Timestamp::from_unix_millis(ms).ok()),
        standard: TokenUsage::of(usage.standard.as_ref()),
        premium: TokenUsage::of(usage.premium.as_ref()),
    })
}

/// The subject claim inside a JWT, as the source's `parseJWT` reads it: at least two
/// dot-separated parts, the second base64url for a JSON object. Answers the `sub`
/// string and nothing else — the token never leaves this function, which is what
/// keeps the key out of every log. An `fk-…` key is not a JWT and names no user.
fn jwt_subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let payload = payload.replace('-', "+").replace('_', "/");
    let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
    let decoded = BASE64.decode(padded.as_bytes()).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    normalized(claims.get("sub")?.as_str())
}

/// The source's `factoryNormalizedString`: trimmed, and empty is nothing.
fn normalized(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// The token-rate-limits card: up to six windows — the standard pool's three, then
/// the core pool's three when it reports any usage data — plus the plan and the
/// extra usage balance. Pure, so every recorded billing body is reachable from a
/// test.
fn rate_limits_snapshot(auth: &Auth, reading: &BillingReading, now: Timestamp) -> Snapshot {
    let mut windows = vec![
        billing_window(
            WindowKey::for_length(length(FIVE_HOURS)),
            "5h",
            FIVE_HOURS,
            &reading.standard.five_hour,
            now,
        ),
        billing_window(
            WindowKey::for_length(length(WEEK)),
            "7-day",
            WEEK,
            &reading.standard.weekly,
            now,
        ),
        // A monthly window states no length the source passes on, so it is keyed by
        // name — the one spelling a month has on this card.
        billing_window(
            WindowKey::named("monthly"),
            "Monthly",
            0,
            &reading.standard.monthly,
            now,
        ),
    ];
    if let Some(core) = reading.core.as_ref().filter(|core| core.has_usage_data()) {
        windows.extend([
            billing_window(
                WindowKey::for_pool("core", length(FIVE_HOURS)),
                "Core 5h",
                FIVE_HOURS,
                &core.five_hour,
                now,
            ),
            billing_window(
                WindowKey::for_pool("core", length(WEEK)),
                "Core 7-day",
                WEEK,
                &core.weekly,
                now,
            ),
            // The core monthly window, keyed to match its pool's keyed windows.
            billing_window(
                WindowKey::named("core/monthly"),
                "Core Monthly",
                0,
                &core.monthly,
                now,
            ),
        ]);
    }
    let mut details = vec![plan_section(auth, reading.overage_preference.as_deref())];
    details.push(DetailSection {
        title: "Billing".to_owned(),
        rows: vec![labeled(
            "Extra usage balance",
            usd(reading.extra_usage_balance_cents),
        )],
    });
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows,
        details,
    }
}

/// One window of the billing card. Pure.
fn billing_window(
    key: WindowKey,
    title: &str,
    length_secs: u64,
    window: &BillingWindow,
    now: Timestamp,
) -> Window {
    Window {
        key,
        title: title.to_owned(),
        // The body states only a percentage; there are no absolutes to phrase.
        subtitle: None,
        used_percent: effective_used_percent(window, now),
        resets_at: reset_at(window, now),
        length: nonzero(length_secs),
    }
}

/// The billing-period card: two windows — standard and premium — each tokens used
/// against a stated allowance, reset at the period's end. Pure.
fn classic_snapshot(auth: &Auth, usage: &Usage, now: Timestamp) -> Snapshot {
    let windows = vec![
        pool_window("standard", "Standard", &usage.standard, usage.period_end),
        pool_window("premium", "Premium", &usage.premium, usage.period_end),
    ];
    let mut details = vec![plan_section(auth, None)];
    for (title, pool) in [("Standard", &usage.standard), ("Premium", &usage.premium)] {
        details.push(DetailSection {
            title: title.to_owned(),
            rows: vec![
                labeled("Used", number_text(pool.user_tokens)),
                labeled(
                    "Organization total",
                    number_text(pool.org_total_tokens_used),
                ),
                labeled("Allowance", number_text(pool.total_allowance)),
            ],
        });
    }
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows,
        details,
    }
}

/// One pool's billing-period window. A billing period states no fixed span — its
/// length is whatever the dates happen to delimit — so it is keyed by name with no
/// length and no pace mark.
fn pool_window(key: &str, title: &str, pool: &TokenUsage, resets_at: Option<Timestamp>) -> Window {
    Window {
        key: WindowKey::named(key),
        title: title.to_owned(),
        subtitle: (pool.total_allowance > 0).then(|| {
            format!(
                "{} / {} tokens",
                number_text(pool.user_tokens),
                number_text(pool.total_allowance)
            )
        }),
        used_percent: usage_percent(pool.user_tokens, pool.total_allowance, pool.used_ratio),
        resets_at,
        length: None,
    }
}

/// The percent behind every billing-period window, the source's
/// `calculateUsagePercent` with its recorded escalations: the API's ratio when it is
/// usable, the local division otherwise, an unlimited allowance measured against a
/// hundred-million-token reference, and no allowance at all worth zero.
fn usage_percent(used: i64, allowance: i64, api_ratio: Option<f64>) -> f64 {
    if let Some(ratio) = api_ratio
        && !ratio_contradicted_by_local_reading(ratio, used, allowance)
        && let Some(percent) = percent_from_ratio(ratio, allowance)
    {
        return percent;
    }
    if allowance > UNLIMITED_ABOVE {
        return ((used as f64 / 100_000_000.0) * 100.0).min(100.0);
    }
    if allowance <= 0 {
        return 0.0;
    }
    ((used as f64 / allowance as f64) * 100.0).min(100.0)
}

/// A ratio of zero alongside real local readings — tokens used against a stated,
/// finite allowance — is the recorded contradiction the local division overrules.
fn ratio_contradicted_by_local_reading(ratio: f64, used: i64, allowance: i64) -> bool {
    ratio == 0.0 && used > 0 && allowance > 0 && allowance <= UNLIMITED_ABOVE
}

/// The source's `percentFromAPIRatio`: a ratio on the 0..1 scale is a fraction of
/// the card; one off that scale is tried as an already-percent only when the
/// allowance cannot arbitrate — missing, or beyond the unlimited threshold.
fn percent_from_ratio(ratio: f64, allowance: i64) -> Option<f64> {
    if !ratio.is_finite() {
        return None;
    }
    if (-0.001..=1.001).contains(&ratio) {
        return Some((ratio * 100.0).clamp(0.0, 100.0));
    }
    let allowance_is_reliable = allowance > 0 && allowance <= UNLIMITED_ABOVE;
    if !allowance_is_reliable && (-0.1..=100.1).contains(&ratio) {
        return Some(ratio.clamp(0.0, 100.0));
    }
    None
}

/// The plan section: the source's login method — `Factory <Tier>` joined with the
/// plan unless the plan already says Factory, plus the overage fallback when the new
/// billing names one — and the organization.
fn plan_section(auth: &Auth, overage: Option<&str>) -> DetailSection {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tier) = auth.tier.as_deref().filter(|tier| !tier.is_empty()) {
        parts.push(format!("Factory {}", capitalized(tier)));
    }
    if let Some(plan) = auth
        .plan
        .as_deref()
        .filter(|plan| !plan.is_empty() && !plan.to_lowercase().contains("factory"))
    {
        parts.push(plan.to_owned());
    }
    if let Some(overage) = overage.filter(|overage| !overage.is_empty()) {
        parts.push(format!("Fallback: {overage}"));
    }
    let mut rows = Vec::new();
    if !parts.is_empty() {
        rows.push(labeled("Plan", parts.join(" - ")));
    }
    if let Some(organization) = auth.organization.as_deref() {
        rows.push(labeled("Organization", organization));
    }
    DetailSection {
        title: DetailSection::PLAN.to_owned(),
        rows,
    }
}

/// The source's `tier.capitalized`: each whitespace-separated word with its first
/// character upper and the rest lower.
fn capitalized(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A window length, for the two constants this card keys on.
fn length(secs: u64) -> WindowLength {
    nonzero(secs).expect("a nonzero constant")
}

/// A window length, or none for the monthly spellings that state no span.
fn nonzero(secs: u64) -> Option<WindowLength> {
    WindowLength::from_secs(secs)
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

/// The extra usage balance in dollars, as the menu text records it: cents over a
/// hundred, two fraction digits.
fn usd(cents: i64) -> String {
    format!("${:.2}", cents as f64 / 100.0)
}

/// Whole counts with thousands separators, the card's token counts being always
/// whole.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn window_of<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|window| window.key.as_str() == key)
            .unwrap_or_else(|| panic!("no {key} window in {snapshot:?}"))
    }

    fn row_of<'a>(snapshot: &'a Snapshot, label: &str) -> &'a DetailRow {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no {label} row in {snapshot:?}"))
    }

    fn secs(length: Option<WindowLength>) -> Option<u64> {
        length.map(WindowLength::as_secs)
    }

    /// The auth/me body every ladder test serves, verbatim from
    /// FactoryAPIKeyUsageTests.swift and FactoryStatusProbeFetchTests.swift.
    const AUTH_ME: &str = r#"{
          "organization": {
            "id": "org_1",
            "name": "Acme",
            "subscription": {
              "factoryTier": "team",
              "orbSubscription": {
                "plan": { "name": "Team", "id": "plan_1" },
                "status": "active"
              }
            }
          }
        }"#;

    /// The auth/me body of "uses bearer subject when auth profile omits user id": a
    /// userProfile that carries no id, so the user comes from the token instead.
    const AUTH_ME_WITHOUT_USER_ID: &str = r#"{
          "organization": {
            "id": "org_1",
            "name": "Acme",
            "subscription": {
              "factoryTier": "team",
              "orbSubscription": {
                "plan": { "name": "Team", "id": "plan_1" },
                "status": "active"
              }
            }
          },
          "userProfile": {
            "email": "user@example.com",
            "role": "member"
          }
        }"#;

    /// FactoryAPIKeyUsageTests.swift, "fetch with api key uses bearer billing limits
    /// path": no core pool, a zero extra-usage balance.
    const LIMITS_WITHOUT_CORE: &str = r#"{
          "usesTokenRateLimitsBilling": true,
          "limits": {
            "standard": {
              "fiveHour": { "usedPercent": 12, "secondsRemaining": 3600 },
              "weekly": { "usedPercent": 34, "secondsRemaining": 86400 },
              "monthly": { "usedPercent": 56, "secondsRemaining": 604800 }
            }
          },
          "extraUsageBalanceCents": 0,
          "extraUsageAllowed": false,
          "tokenRateLimitsRolloutEligible": true
        }"#;

    /// FactoryStatusProbeFetchTests.swift, "uses token rate limits billing when core
    /// pool is absent": the same standard pool, a 2500-cent balance.
    const LIMITS_WITH_BALANCE: &str = r#"{
          "usesTokenRateLimitsBilling": true,
          "extraUsageBalanceCents": 2500,
          "overagePreference": null,
          "extraUsageAllowed": false,
          "tokenRateLimitsRolloutEligible": true,
          "limits": {
            "standard": {
              "fiveHour": { "usedPercent": 12, "secondsRemaining": 3600 },
              "weekly": { "usedPercent": 34, "secondsRemaining": 7200 },
              "monthly": { "usedPercent": 56, "secondsRemaining": 10800 }
            }
          }
        }"#;

    /// FactoryStatusProbeFetchTests.swift, "uses token rate limits billing when
    /// enabled": the core pool beside the standard one, an overage preference.
    const LIMITS_WITH_CORE: &str = r#"{
          "usesTokenRateLimitsBilling": true,
          "extraUsageBalanceCents": 2500,
          "overagePreference": "core",
          "extraUsageAllowed": true,
          "tokenRateLimitsRolloutEligible": true,
          "limits": {
            "standard": {
              "fiveHour": { "usedPercent": 12, "secondsRemaining": 3600 },
              "weekly": { "usedPercent": 34, "secondsRemaining": 7200 },
              "monthly": { "usedPercent": 56, "secondsRemaining": 10800 }
            },
            "core": {
              "fiveHour": { "usedPercent": 7, "secondsRemaining": 1800 },
              "weekly": { "usedPercent": 8, "secondsRemaining": 2800 },
              "monthly": { "usedPercent": 9, "secondsRemaining": 3800 }
            }
          }
        }"#;

    /// FactoryStatusProbeFetchTests.swift, the cookie-ladder test that reads both
    /// pools with a billing period attached.
    const USAGE_WITH_PERIOD: &str = r#"{
          "usage": {
            "startDate": 1700000000000,
            "endDate": 1700003600000,
            "standard": {
              "userTokens": 100,
              "orgTotalTokensUsed": 250,
              "totalAllowance": 1000,
              "usedRatio": 0.10
            },
            "premium": {
              "userTokens": 10,
              "orgTotalTokensUsed": 20,
              "totalAllowance": 100,
              "usedRatio": 0.10
            }
          },
          "userId": "user-1"
        }"#;

    /// FactoryStatusProbeFetchTests.swift, "falls back to legacy usage when billing
    /// limits request fails": no period, both pools.
    const USAGE_WITHOUT_PERIOD: &str = r#"{
          "usage": {
            "standard": {
              "userTokens": 100,
              "totalAllowance": 1000,
              "usedRatio": 0.10
            },
            "premium": {
              "userTokens": 20,
              "totalAllowance": 100,
              "usedRatio": 0.20
            }
          },
          "userId": "user-1"
        }"#;

    /// The JWT FactoryStatusProbeFetchTests builds with makeJWT(["sub": "user_jwt"]):
    /// its payload segment is the base64url of {"sub":"user_jwt"}, the signature empty.
    /// The header segment is respelled "header" — Swift's JSONSerialization writes the
    /// recorded header's keys in no order worth pinning, and the sniff never reads it.
    const RECORDED_JWT: &str = "header.eyJzdWIiOiJ1c2VyX2p3dCJ9.";

    #[test]
    fn the_recorded_auth_body_reads_the_organization_and_its_plan() {
        let auth = parse_auth(AUTH_ME).expect("parses");
        assert_eq!(auth.organization.as_deref(), Some("Acme"));
        assert_eq!(auth.tier.as_deref(), Some("team"));
        assert_eq!(auth.plan.as_deref(), Some("Team"));
        assert_eq!(
            auth.user_id, None,
            "the recorded body carries no userProfile"
        );

        let without_id = parse_auth(AUTH_ME_WITHOUT_USER_ID).expect("parses");
        assert_eq!(
            without_id.user_id, None,
            "a profile without an id names no user"
        );
        assert_eq!(without_id.organization.as_deref(), Some("Acme"));
    }

    #[test]
    fn the_recorded_jwt_names_the_user_and_a_plain_key_names_none() {
        assert_eq!(jwt_subject(RECORDED_JWT).as_deref(), Some("user_jwt"));
        // "fk-test-key" is the recorded key of the API-key ladder test; an fk- key is
        // not a JWT, so it names no user.
        assert_eq!(jwt_subject("fk-test-key"), None);
        // A payload whose sub is not a string names no user, as the source's
        // `as? String` refuses it.
        assert_eq!(jwt_subject("header.eyJzdWIiOjV9."), None);
    }

    #[test]
    fn the_recorded_api_key_limits_body_draws_three_windows() {
        let auth = parse_auth(AUTH_ME).expect("parses");
        let reading = parse_limits(LIMITS_WITHOUT_CORE).expect("reads");
        let snapshot = rate_limits_snapshot(&auth, &reading, at(1_785_000_000));
        assert_eq!(snapshot.windows.len(), 3, "no core pool, no core windows");
        let five_hour = window_of(&snapshot, "w18000");
        assert_eq!(five_hour.title, "5h");
        assert_eq!(secs(five_hour.length), Some(18_000));
        assert_eq!(five_hour.used_percent, 12.0);
        assert_eq!(
            five_hour.resets_at.map(Timestamp::as_unix),
            Some(1_785_003_600),
            "the recorded 3600 seconds remaining"
        );
        assert_eq!(
            five_hour.subtitle, None,
            "the body states only a percentage"
        );
        let weekly = window_of(&snapshot, "w604800");
        assert_eq!(weekly.title, "7-day");
        assert_eq!(secs(weekly.length), Some(604_800));
        assert_eq!(weekly.used_percent, 34.0);
        assert_eq!(
            weekly.resets_at.map(Timestamp::as_unix),
            Some(1_785_086_400)
        );
        // The monthly window states no length CodexBar passes on, so it is keyed by
        // name with none.
        let monthly = window_of(&snapshot, "monthly");
        assert_eq!(monthly.title, "Monthly");
        assert_eq!(secs(monthly.length), None);
        assert_eq!(monthly.used_percent, 56.0);
        assert_eq!(
            monthly.resets_at.map(Timestamp::as_unix),
            Some(1_785_604_800)
        );
        // The recorded body's balance is zero cents.
        assert_eq!(row_of(&snapshot, "Extra usage balance").value, "$0.00");
        assert_eq!(row_of(&snapshot, "Plan").value, "Factory Team - Team");
        assert_eq!(row_of(&snapshot, "Organization").value, "Acme");
    }

    #[test]
    fn the_recorded_balance_only_limits_body_renders_the_recorded_dollars() {
        let auth = parse_auth(AUTH_ME).expect("parses");
        let reading = parse_limits(LIMITS_WITH_BALANCE).expect("reads");
        assert!(reading.core.is_none());
        let snapshot = rate_limits_snapshot(&auth, &reading, at(1_785_000_000));
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(window_of(&snapshot, "w18000").used_percent, 12.0);
        assert_eq!(window_of(&snapshot, "w604800").used_percent, 34.0);
        assert_eq!(window_of(&snapshot, "monthly").used_percent, 56.0);
        // 2500 cents is the 25 the probe test asserts; "$25.00" is the menu text
        // FactoryProviderImplementationTests records for it.
        assert_eq!(row_of(&snapshot, "Extra usage balance").value, "$25.00");
        assert_eq!(row_of(&snapshot, "Plan").value, "Factory Team - Team");
    }

    #[test]
    fn the_recorded_core_pool_body_draws_six_windows() {
        let auth = parse_auth(AUTH_ME).expect("parses");
        let reading = parse_limits(LIMITS_WITH_CORE).expect("reads");
        assert!(reading.core.is_some());
        let snapshot = rate_limits_snapshot(&auth, &reading, at(1_785_000_000));
        assert_eq!(snapshot.windows.len(), 6);
        let core_five_hour = window_of(&snapshot, "core/w18000");
        assert_eq!(core_five_hour.title, "Core 5h");
        assert_eq!(secs(core_five_hour.length), Some(18_000));
        assert_eq!(core_five_hour.used_percent, 7.0);
        assert_eq!(
            core_five_hour.resets_at.map(Timestamp::as_unix),
            Some(1_785_001_800),
            "the recorded 1800 seconds remaining"
        );
        let core_weekly = window_of(&snapshot, "core/w604800");
        assert_eq!(core_weekly.title, "Core 7-day");
        assert_eq!(core_weekly.used_percent, 8.0);
        assert_eq!(
            core_weekly.resets_at.map(Timestamp::as_unix),
            Some(1_785_002_800)
        );
        let core_monthly = window_of(&snapshot, "core/monthly");
        assert_eq!(core_monthly.title, "Core Monthly");
        assert_eq!(secs(core_monthly.length), None);
        assert_eq!(core_monthly.used_percent, 9.0);
        assert_eq!(
            core_monthly.resets_at.map(Timestamp::as_unix),
            Some(1_785_003_800)
        );
        // The standard pool keeps its own three windows beside the core's.
        assert_eq!(window_of(&snapshot, "w18000").used_percent, 12.0);
        assert_eq!(window_of(&snapshot, "w604800").used_percent, 34.0);
        assert_eq!(window_of(&snapshot, "monthly").used_percent, 56.0);
        assert_eq!(row_of(&snapshot, "Extra usage balance").value, "$25.00");
        // The recorded login method for this body, as its own test spells it.
        assert_eq!(
            row_of(&snapshot, "Plan").value,
            "Factory Team - Team - Fallback: core"
        );
    }

    #[test]
    fn the_recorded_usage_body_draws_two_period_windows() {
        let auth = parse_auth(AUTH_ME).expect("parses");
        let usage = parse_usage(USAGE_WITH_PERIOD).expect("parses");
        let snapshot = classic_snapshot(&auth, &usage, at(1_785_000_000));
        assert_eq!(snapshot.windows.len(), 2);
        let standard = window_of(&snapshot, "standard");
        assert_eq!(standard.title, "Standard");
        assert_eq!(standard.used_percent, 10.0, "the recorded usedRatio 0.10");
        assert_eq!(
            secs(standard.length),
            None,
            "a billing period states no span"
        );
        assert_eq!(
            standard.resets_at.map(Timestamp::as_unix),
            Some(1_700_003_600),
            "the recorded endDate 1700003600000 in milliseconds"
        );
        assert_eq!(standard.subtitle.as_deref(), Some("100 / 1,000 tokens"));
        let premium = window_of(&snapshot, "premium");
        assert_eq!(premium.title, "Premium");
        assert_eq!(premium.used_percent, 10.0);
        assert_eq!(
            premium.resets_at.map(Timestamp::as_unix),
            Some(1_700_003_600)
        );
        assert_eq!(premium.subtitle.as_deref(), Some("10 / 100 tokens"));
        // The token rows behind the bars.
        assert_eq!(row_of(&snapshot, "Used").value, "100");
        assert_eq!(row_of(&snapshot, "Organization total").value, "250");
        assert_eq!(row_of(&snapshot, "Allowance").value, "1,000");
        assert_eq!(row_of(&snapshot, "Plan").value, "Factory Team - Team");
        assert_eq!(row_of(&snapshot, "Organization").value, "Acme");
    }

    #[test]
    fn the_recorded_usage_body_without_period_reads_both_pools() {
        let auth = parse_auth(AUTH_ME).expect("parses");
        let usage = parse_usage(USAGE_WITHOUT_PERIOD).expect("parses");
        let snapshot = classic_snapshot(&auth, &usage, at(1_785_000_000));
        let standard = window_of(&snapshot, "standard");
        assert_eq!(standard.used_percent, 10.0);
        assert_eq!(standard.resets_at, None, "no endDate, no reset");
        assert_eq!(standard.subtitle.as_deref(), Some("100 / 1,000 tokens"));
        let premium = window_of(&snapshot, "premium");
        assert_eq!(premium.used_percent, 20.0, "the recorded usedRatio 0.20");
        assert_eq!(premium.subtitle.as_deref(), Some("20 / 100 tokens"));
    }

    #[test]
    fn the_recorded_percent_rules_stand() {
        // Every row is an input CodexBar's own FactoryStatusSnapshotTests records,
        // with the percent its test asserts.
        for (used, allowance, ratio, expected) in [
            (50, 100, None, 50.0), // "maps usage snapshot windows and login method"
            (25, 50, None, 50.0),  // the same test's premium pool
            (50_000_000, 100_000_000, None, 50.0), // "falls back to calculation when API ratio missing"
            (50_000_000, 100_000_000, Some(1.5), 50.0), // "falls back when API ratio is invalid"
            (100_000_000, 100_000_000, Some(1.0005), 100.0), // "clamps slightly out of range ratios"
            (0, 0, Some(10.0), 10.0), // "uses percent scale ratio when allowance missing"
            (50_000_000, 2_000_000_000_000, None, 50.0), // "treats large allowances as unlimited"
        ] {
            assert_eq!(usage_percent(used, allowance, ratio), expected);
        }
        // "prefers API used ratio when allowance missing": 36.155…%, asserted between
        // 36 and 37 there.
        let preferred = usage_percent(72_311_737, 0, Some(0.361_558_68));
        assert!((36.0..37.0).contains(&preferred), "{preferred}");
        // "falls back to calculation when API ratio is zero but usage and allowance
        // are present": 29.13…%, asserted between 29 and 30 there.
        let zero_ratio = usage_percent(5_826_293, 20_000_000, Some(0.0));
        assert!((29.0..30.0).contains(&zero_ratio), "{zero_ratio}");
    }

    #[test]
    fn bodies_that_cannot_be_read_are_refused() {
        // The procedure's canonical partial bodies, plus a recognised field whose
        // value cannot be read — constructed error paths, as the procedure allows.
        for error in [
            parse_usage("{\"partial\":").expect_err("usage partial"),
            parse_usage(r#"{"usage":{"standard":{"userTokens":"many"}}}"#)
                .expect_err("a string where a token count belongs"),
            parse_usage(r#"{"usage":{"startDate":1700000000.5}}"#)
                .expect_err("a fraction where a whole millisecond belongs"),
            parse_auth("{\"partial\":").expect_err("auth partial"),
            parse_auth(r#"{"organization":42}"#).expect_err("a number where an object belongs"),
        ] {
            assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
        }
    }

    #[test]
    fn a_billing_body_that_cannot_be_read_is_softly_skipped() {
        // The second rung reads softly: a partial body, a body that declines the new
        // billing, a body without its limits object, and a window missing its percent
        // are all no answer — the ladder falls through to usage, as the source's
        // `try?` and its guards do.
        assert_eq!(parse_limits("{\"partial\":"), None);
        assert_eq!(
            parse_limits(r#"{"usesTokenRateLimitsBilling":false}"#),
            None
        );
        assert_eq!(parse_limits(r#"{"usesTokenRateLimitsBilling":true}"#), None);
        assert_eq!(
            parse_limits(
                r#"{"usesTokenRateLimitsBilling":true,"limits":{"standard":{"fiveHour":{"secondsRemaining":10}}}}"#
            ),
            None
        );
    }

    #[test]
    fn fields_these_parsers_do_not_read_are_skipped() {
        // The unknown-kind rule: the recorded bodies already carry `status`, `id`,
        // `source`, `extraUsageAllowed` and `tokenRateLimitsRolloutEligible` — none of
        // them read — and one more invented field rides along the same way.
        let usage = parse_usage(&USAGE_WITH_PERIOD.replacen(
            "{\n          \"usage\"",
            "{\n          \"future\": {\"kind\": \"daily\"},\n          \"usage\"",
            1,
        ))
        .expect("parses");
        assert_eq!(usage.standard.user_tokens, 100);
        let reading = parse_limits(&LIMITS_WITHOUT_CORE.replacen(
            "\"usesTokenRateLimitsBilling\": true,",
            "\"usesTokenRateLimitsBilling\": true, \"future\": {\"kind\": \"daily\"},",
            1,
        ))
        .expect("reads");
        assert_eq!(reading.standard.five_hour.used_percent, 12.0);
        let auth = parse_auth(&AUTH_ME.replacen(
            "{\n          \"organization\"",
            "{\n          \"future\": {\"kind\": \"daily\"},\n          \"organization\"",
            1,
        ))
        .expect("parses");
        assert_eq!(auth.organization.as_deref(), Some("Acme"));
    }

    #[test]
    fn the_three_requests_address_the_recorded_ladder_with_a_bearer_key() {
        let factory = Factory::new(Credential::new("fk-test-key")).expect("builds");
        let auth = factory.auth_request().expect("builds");
        assert_eq!(auth.method(), reqwest::Method::GET);
        assert_eq!(
            auth.url().as_str(),
            "https://api.factory.ai/api/app/auth/me"
        );
        let limits = factory.limits_request().expect("builds");
        assert_eq!(
            limits.url().as_str(),
            "https://api.factory.ai/api/billing/limits"
        );
        let named = factory.usage_request(Some("user_jwt")).expect("builds");
        assert_eq!(
            named.url().as_str(),
            "https://api.factory.ai/api/organization/subscription/usage?useCache=true&userId=user_jwt",
            "the recorded trace's query, in its order"
        );
        let anonymous = factory.usage_request(None).expect("builds");
        assert_eq!(
            anonymous.url().as_str(),
            "https://api.factory.ai/api/organization/subscription/usage?useCache=true"
        );
        for request in [auth, limits, named] {
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer fk-test-key",
                "the recorded spelling of the key's placement"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .expect("present"),
                "application/json"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ORIGIN)
                    .expect("present"),
                "https://app.factory.ai"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::REFERER)
                    .expect("present"),
                "https://app.factory.ai/"
            );
            assert_eq!(
                request.headers().get("x-factory-client").expect("present"),
                "web-app"
            );
        }
    }

    #[test]
    fn the_spec_publishes_no_options_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Factory");
        assert!(SPEC.options.is_empty());
        assert!(build(Credential::new("fk-test-key"), &Options::new()).is_ok());
    }

    #[test]
    fn a_factory_client_never_prints_its_credential() {
        let factory = Factory::new(Credential::new("fk-super-secret")).expect("builds");
        let rendered = format!("{factory:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
