//! Codebuff.
//!
//! Ported from CodexBar's `Codebuff/CodebuffUsageFetcher.swift`. Never seen answering:
//! every number in the tests is a body CodexBar recorded.
//!
//! # Two requests, one of which may fail without failing the fetch
//!
//! `POST /api/v1/usage` carries the credit balance and is the point of the poll.
//! `GET /api/user/subscription` carries the plan and the weekly window, and the source
//! races it against a two-second grace: whatever it does — hang, refuse, answer nonsense —
//! the credits already in hand are published. That is ported as written, because a card
//! that blanked because the *second* endpoint was slow would be worse than one missing its
//! plan name.
//!
//! The CLI's credentials file is not read. The key comes from the Secret Service like every
//! other key in this module.
//!
//! # What the payload does not tell you
//!
//! **The usage POST needs a body, and the body needs a name in it.** `fingerprintId` is
//! required or the endpoint refuses; the value is a client identifier, so this sends its
//! own rather than borrowing CodexBar's.
//!
//! **The credit total is not always stated.** `quota` or `limit` when it is; otherwise
//! `usage + remainingBalance`, which is the same number arrived at from the other end.
//!
//! **A credit balance with no total is drawn full, on purpose.** The source's own comment
//! says why: a payload that reports spending and no allowance is a broken configuration,
//! and a healthy-looking empty bar would hide it. So would no bar at all. Nothing reported
//! anywhere is still no window.
//!
//! **The numbers may be strings, and the plan name may be a number.** Both are read as what
//! they are. A tier arriving as an integer too large for one is rendered from its own
//! digits rather than through a float, which is the source's own care.
//!
//! **A credit balance has no period.** `next_quota_reset` says when it refills but nothing
//! says how long the cycle is, so the window carries a reset and no length: a pace mark
//! needs both, and a guessed one is worse than none.

use super::{HandSpec, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "codebuff";

/// The host both endpoints live on.
const BASE_URL: &str = "https://www.codebuff.com";

/// Sent as the body of the usage POST. The endpoint refuses a request without it; the value
/// is a client identifier, and this one is ours.
const FINGERPRINT: &str = r#"{"fingerprintId":"tidemark-usage"}"#;

/// How long the weekly window runs. Not on the wire: the source's `7 * 24 * 60` minutes.
const WEEK_SECS: u64 = 604_800;

/// What the usage POST reports.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    /// Credits spent, where the payload says.
    pub used: Option<f64>,
    /// The allowance, where the payload states one outright.
    pub total: Option<f64>,
    /// Credits left.
    pub remaining: Option<f64>,
    /// When the allowance refills.
    pub next_quota_reset: Option<Timestamp>,
    /// Whether the account buys more credits by itself when it runs out.
    pub auto_topup: Option<bool>,
}

/// What the subscription GET reports.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Subscription {
    pub status: Option<String>,
    /// What the plan is called. May arrive as a number; see the module doc.
    pub tier: Option<String>,
    pub billing_period_end: Option<Timestamp>,
    pub weekly_used: Option<f64>,
    pub weekly_limit: Option<f64>,
    pub weekly_resets_at: Option<Timestamp>,
    pub email: Option<String>,
}

/// A number, whether it arrived as one or as a string holding one.
///
/// `None` for an absent or null field; `Err` for one that is there and is neither — the
/// source ignores it, but a field it cannot read is a response we do not understand.
fn number(map: &Map<String, Value>, key: &str) -> Result<Option<f64>, ProviderError> {
    let Some(value) = map.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    parsed
        .filter(|number: &f64| number.is_finite())
        .map(Some)
        .ok_or_else(|| ProviderError::malformed(format!("{key} is not a number")))
}

/// The first of these keys that carries a number.
fn first_number(map: &Map<String, Value>, keys: &[&str]) -> Result<Option<f64>, ProviderError> {
    for key in keys {
        if let Some(value) = number(map, key)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// A non-empty string, or a number rendered from its own digits.
///
/// The digits matter: a tier of `9223372036854775808` has been recorded, and rendering it
/// through a float would print something that is not what arrived.
fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key)? {
        Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// An instant in any of the spellings the source accepts: RFC-3339, or an epoch in
/// milliseconds above the year 2286 and seconds below it.
///
/// A field that is there and unreadable fails the fetch rather than quietly becoming a
/// window with no reset.
fn instant(map: &Map<String, Value>, key: &str) -> Result<Option<Timestamp>, ProviderError> {
    let Some(value) = map.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let parsed = match value {
        Value::String(raw) => {
            let trimmed = raw.trim();
            parse_rfc3339(trimmed).or_else(|| trimmed.parse::<f64>().ok().and_then(epoch))
        }
        Value::Number(raw) => raw.as_f64().and_then(epoch),
        _ => None,
    };
    parsed
        .map(Some)
        .ok_or_else(|| ProviderError::malformed(format!("{key} is not a readable time")))
}

/// An epoch value in whichever unit it arrived in.
fn epoch(value: f64) -> Option<Timestamp> {
    if !value.is_finite() {
        return None;
    }
    if value > 10_000_000_000.0 {
        return Timestamp::from_unix_millis(value as i64).ok();
    }
    Timestamp::from_unix(value as i64).ok()
}

/// The object at `key`, if there is one.
fn child<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    map.get(key).and_then(Value::as_object)
}

/// The usage POST's body. Pure: every trap above is reachable from a test.
pub fn parse_usage(body: &str) -> Result<Usage, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("the usage response is not an object"))?;

    let auto_topup = ["autoTopupEnabled", "auto_topup_enabled"]
        .iter()
        .find_map(|key| root.get(*key).and_then(Value::as_bool));

    Ok(Usage {
        used: first_number(root, &["usage", "used"])?,
        total: first_number(root, &["quota", "limit"])?,
        remaining: first_number(root, &["remainingBalance", "remaining"])?,
        next_quota_reset: instant(root, "next_quota_reset")?,
        auto_topup,
    })
}

/// The subscription GET's body. Pure.
pub fn parse_subscription(body: &str) -> Result<Subscription, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("the subscription response is not an object"))?;
    let subscription = child(root, "subscription");
    let rate_limit = child(root, "rateLimit");
    let empty = Map::new();
    let sub = subscription.unwrap_or(&empty);
    let rate = rate_limit.unwrap_or(&empty);

    let tier = text(sub, "displayName")
        .or_else(|| text(root, "displayName"))
        .or_else(|| text(sub, "tier"))
        .or_else(|| text(root, "tier"))
        .or_else(|| text(sub, "scheduledTier"));
    let email = root
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            child(root, "user")
                .and_then(|user| user.get("email"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);

    let billing_period_end = match instant(sub, "billingPeriodEnd")? {
        Some(at) => Some(at),
        None => instant(sub, "currentPeriodEnd")?,
    };

    Ok(Subscription {
        status: sub.get("status").and_then(Value::as_str).map(str::to_owned),
        tier,
        billing_period_end,
        weekly_used: first_number(rate, &["weeklyUsed", "used"])?,
        weekly_limit: first_number(rate, &["weeklyLimit", "limit"])?,
        weekly_resets_at: instant(rate, "weeklyResetsAt")?,
        email,
    })
}

/// A credit count in the source's own rounding: grouped, and to one decimal below a
/// thousand where there is a fraction to show.
fn credits(value: f64) -> String {
    let decimals = if value >= 1_000.0 || value.fract() == 0.0 {
        0
    } else {
        1
    };
    let rendered = format!("{value:.decimals$}");
    let (whole, rest) = rendered.split_once('.').unwrap_or((rendered.as_str(), ""));
    let bytes = whole.as_bytes();
    let mut out = String::with_capacity(rendered.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }
    if !rest.is_empty() {
        out.push('.');
        out.push_str(rest);
    }
    out
}

/// The `YYYY-MM-DD` a whole-second timestamp falls on, for the billing row.
fn day_of(at: Timestamp) -> String {
    let date = time::OffsetDateTime::from_unix_timestamp(at.as_unix())
        .expect("a plausible timestamp converts")
        .date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The plan name in the source's own casing: a word it shouted or spelled lower is title
/// cased, a name already spelled deliberately is left alone.
fn plan_title(tier: &str) -> String {
    crate::providers::title_case(tier)
}

/// Both bodies as one reading. Pure, so the whole shape is reachable without a server.
pub fn snapshot(
    usage: &Usage,
    subscription: Option<&Subscription>,
    captured_at: Timestamp,
) -> Snapshot {
    let mut windows = Vec::new();

    // The allowance where it is stated, otherwise arrived at from the other end.
    let total =
        usage
            .total
            .map(|total| total.max(0.0))
            .or_else(|| match (usage.used, usage.remaining) {
                (Some(used), Some(remaining)) => Some((used + remaining).max(0.0)),
                _ => None,
            });
    let used = usage
        .used
        .map(|used| used.max(0.0))
        .or_else(|| match (total, usage.remaining) {
            (Some(total), Some(remaining)) => Some((total - remaining).max(0.0)),
            _ => None,
        });

    match total {
        Some(total) if total > 0.0 => {
            let used = used.unwrap_or(0.0);
            windows.push(Window {
                // A credit balance has no length to key on. See the module doc.
                key: WindowKey::named("credits"),
                title: "Credits".to_owned(),
                subtitle: Some(format!("{} / {} credits", credits(used), credits(total))),
                used_percent: (used / total * 100.0).clamp(0.0, 100.0),
                resets_at: usage.next_quota_reset,
                length: None,
            });
        }
        // Spending reported against no allowance: drawn full so the broken configuration is
        // visible, which is the source's own choice and its own comment.
        _ if usage.used.is_some() || usage.remaining.is_some() => {
            let known = usage
                .remaining
                .map(|remaining| {
                    format!("{} credits left, no allowance reported", credits(remaining))
                })
                .or_else(|| {
                    usage.used.map(|used| {
                        format!("{} credits spent, no allowance reported", credits(used))
                    })
                });
            windows.push(Window {
                key: WindowKey::named("credits"),
                title: "Credits".to_owned(),
                subtitle: known,
                used_percent: 100.0,
                resets_at: usage.next_quota_reset,
                length: None,
            });
        }
        // Nothing anywhere: no window rather than an invented one.
        _ => {}
    }

    let mut rows = Vec::new();
    if let Some(subscription) = subscription {
        if let Some(limit) = subscription.weekly_limit.filter(|limit| *limit > 0.0) {
            let used = subscription.weekly_used.unwrap_or(0.0).max(0.0);
            let length = WindowLength::from_secs(WEEK_SECS).expect("a week is not zero seconds");
            windows.push(Window {
                key: WindowKey::for_length(length),
                title: crate::providers::length_title(length),
                subtitle: Some(format!("{} / {} credits", credits(used), credits(limit))),
                used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
                resets_at: subscription.weekly_resets_at,
                length: Some(length),
            });
        }

        if let Some(tier) = &subscription.tier {
            rows.push(DetailRow {
                label: "Plan".to_owned(),
                value: plan_title(tier),
            });
        }
        if let Some(status) = &subscription.status {
            rows.push(DetailRow {
                label: "Status".to_owned(),
                value: crate::providers::title_case(status),
            });
        }
        if let Some(end) = subscription.billing_period_end {
            rows.push(DetailRow {
                label: "Billing period ends".to_owned(),
                value: day_of(end),
            });
        }
        if let Some(email) = &subscription.email {
            rows.push(DetailRow {
                label: "Account".to_owned(),
                value: email.clone(),
            });
        }
    }
    if let Some(remaining) = usage.remaining {
        rows.push(DetailRow {
            label: "Credits left".to_owned(),
            value: credits(remaining),
        });
    }
    // Said only when it is on, as the source says it only then: "auto top-up: off" reads as
    // a setting worth acting on, and it is not.
    if usage.auto_topup == Some(true) {
        rows.push(DetailRow {
            label: "Auto top-up".to_owned(),
            value: "On".to_owned(),
        });
    }

    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: if rows.is_empty() {
            Vec::new()
        } else {
            vec![DetailSection {
                title: DetailSection::PLAN.to_owned(),
                rows,
            }]
        },
    }
}

/// The usage endpoint.
pub fn usage_url() -> String {
    format!("{BASE_URL}/api/v1/usage")
}

/// The subscription endpoint.
pub fn subscription_url() -> String {
    format!("{BASE_URL}/api/user/subscription")
}

/// Codebuff as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Codebuff",
    credential: CredentialKind::Key,
    credential_hint: "codebuff.com → Settings → API keys.",
    options: &[],
    build,
};

fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    let _ = options;
    Ok(Arc::new(Codebuff::new(credential)?))
}

/// One Codebuff account: the key, and the two endpoints it is polled at.
pub struct Codebuff {
    client: reqwest::Client,
    credential: Credential,
}

impl Codebuff {
    /// Builds a client.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    /// The usage POST, built but not sent, so that the placement of the key and the body
    /// the endpoint insists on are testable without a server.
    pub fn usage_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(usage_url())
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(FINGERPRINT)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The subscription GET, built but not sent.
    pub fn subscription_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(subscription_url())
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The optional second request. It never fails the fetch: a failure of any kind —
    /// transport, status or shape — leaves the plan and the weekly window off the card,
    /// because the credits already in hand are the point of the poll.
    async fn subscription(&self) -> Option<Subscription> {
        let request = self.subscription_request().ok()?;
        let body = super::request(PROVIDER_ID, &self.client, request)
            .await
            .ok()?;
        parse_subscription(&body).ok()
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let usage =
            parse_usage(&super::request(PROVIDER_ID, &self.client, self.usage_request()?).await?)?;
        let subscription = self.subscription().await;
        Ok(snapshot(&usage, subscription.as_ref(), Timestamp::now()))
    }
}

impl fmt::Debug for Codebuff {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Codebuff")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Codebuff {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar, `CodebuffUsageFetcherTests.swift` — "usage payload parses
    /// numeric credit fields".
    const USAGE: &str = r#"
    {
      "usage": 1250,
      "quota": 5000,
      "remainingBalance": 3750,
      "autoTopupEnabled": true,
      "next_quota_reset": "2026-05-01T00:00:00Z"
    }"#;

    /// Recorded by CodexBar, same file — "subscription payload parses tier and weekly
    /// window".
    const SUBSCRIPTION: &str = r#"
    {
      "hasSubscription": true,
      "subscription": {
        "status": "active",
        "tier": "pro",
        "billingPeriodEnd": "2026-05-15T00:00:00Z"
      },
      "rateLimit": {
        "weeklyUsed": 2100,
        "weeklyLimit": 7000,
        "weeklyResetsAt": "2026-05-08T00:00:00Z"
      },
      "email": "user@example.com"
    }"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn row<'a>(snapshot: &'a Snapshot, label: &str) -> Option<&'a str> {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .map(|row| row.value.as_str())
    }

    #[test]
    fn the_recorded_pair_draws_the_credits_and_the_week() {
        let usage = parse_usage(USAGE).expect("parses");
        assert_eq!(usage.used, Some(1250.0));
        assert_eq!(usage.total, Some(5000.0));
        assert_eq!(usage.remaining, Some(3750.0));
        assert_eq!(usage.auto_topup, Some(true));
        let subscription = parse_subscription(SUBSCRIPTION).expect("parses");
        assert_eq!(subscription.tier.as_deref(), Some("pro"));
        assert_eq!(subscription.status.as_deref(), Some("active"));

        let snapshot = snapshot(&usage, Some(&subscription), at(1_800_000_000));
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].title, "Credits");
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("1,250 / 5,000 credits")
        );
        assert!(
            snapshot.windows[0].length.is_none(),
            "nothing said how long a credit cycle is; a pace mark needs a length"
        );
        assert!(snapshot.windows[0].resets_at.is_some());
        assert_eq!(snapshot.windows[1].title, "7 days");
        assert_eq!(snapshot.windows[1].used_percent, 30.0);
        assert_eq!(
            snapshot.windows[1].length.expect("a week").as_secs(),
            604_800
        );
        assert_eq!(row(&snapshot, "Plan"), Some("Pro"));
        assert_eq!(row(&snapshot, "Status"), Some("Active"));
        assert_eq!(row(&snapshot, "Billing period ends"), Some("2026-05-15"));
        assert_eq!(row(&snapshot, "Account"), Some("user@example.com"));
        assert_eq!(row(&snapshot, "Credits left"), Some("3,750"));
        assert_eq!(row(&snapshot, "Auto top-up"), Some("On"));
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn a_total_nobody_stated_is_arrived_at_from_the_other_end() {
        // CodexBar's "snapshot infers total from used plus remaining": 40 of 100 is 40%.
        let usage = parse_usage(r#"{"usage":40,"remainingBalance":60}"#).expect("parses");
        let snapshot = snapshot(&usage, None, at(1_800_000_000));
        assert_eq!(snapshot.windows[0].used_percent, 40.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("40 / 100 credits")
        );
    }

    #[test]
    fn credits_with_no_allowance_are_drawn_full_and_nothing_at_all_draws_nothing() {
        // CodexBar's "snapshot surfaces exhausted state when quota is missing".
        for body in [r#"{"usage":42}"#, r#"{"remainingBalance":17}"#] {
            let usage = parse_usage(body).expect("parses");
            let snapshot = snapshot(&usage, None, at(1_800_000_000));
            assert_eq!(snapshot.windows.len(), 1, "{body}");
            assert_eq!(
                snapshot.windows[0].used_percent, 100.0,
                "a healthy-looking bar would hide the broken configuration"
            );
        }

        // "snapshot hides credit window when no credit fields are present".
        let empty = parse_usage("{}").expect("an empty object is a readable response");
        assert_eq!(empty, Usage::default());
        assert!(snapshot(&empty, None, at(1_800_000_000)).windows.is_empty());
    }

    #[test]
    fn a_number_sent_as_a_string_is_still_a_number() {
        // CodexBar's "usage payload accepts string-encoded numbers".
        let usage = parse_usage(r#"{ "usage": "12", "quota": "100", "remainingBalance": "88" }"#)
            .expect("parses");
        assert_eq!(usage.used, Some(12.0));
        assert_eq!(usage.total, Some(100.0));
        assert_eq!(usage.remaining, Some(88.0));
    }

    #[test]
    fn a_plan_name_that_arrives_as_a_number_keeps_its_own_digits() {
        // CodexBar's "prefers display name over numeric tier".
        let named =
            parse_subscription(r#"{ "subscription": { "tier": 2, "displayName": "Pro" } }"#)
                .expect("parses");
        assert_eq!(named.tier.as_deref(), Some("Pro"));

        // "falls back to numeric scheduled tier".
        let scheduled =
            parse_subscription(r#"{ "subscription": { "scheduledTier": 3 } }"#).expect("parses");
        assert_eq!(scheduled.tier.as_deref(), Some("3"));

        // "formats oversized numeric tier without trapping": too large for an i64, and it
        // must still come out as the digits that arrived.
        let huge =
            parse_subscription(r#"{ "subscription": { "scheduledTier": 9223372036854775808 } }"#)
                .expect("parses");
        assert_eq!(huge.tier.as_deref(), Some("9223372036854775808"));
    }

    #[test]
    fn a_subscription_with_no_rate_limit_draws_no_weekly_window() {
        // CodexBar's "subscription payload tolerates missing rate limit".
        let subscription =
            parse_subscription(r#"{ "subscription": { "status": "trialing", "tier": "free" } }"#)
                .expect("parses");
        assert_eq!(subscription.weekly_limit, None);
        let usage = parse_usage(r#"{"usage":1,"quota":10}"#).expect("parses");
        let snapshot = snapshot(&usage, Some(&subscription), at(1_800_000_000));
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(row(&snapshot, "Status"), Some("Trialing"));
    }

    #[test]
    fn a_body_we_cannot_read_is_malformed() {
        for body in ["not-json", "[]", r#"{"usage":true}"#, r#"{"quota":{}}"#] {
            assert!(
                matches!(parse_usage(body), Err(ProviderError::Malformed(_))),
                "{body}"
            );
        }
        assert!(matches!(
            parse_usage(r#"{"next_quota_reset":"soon"}"#),
            Err(ProviderError::Malformed(_))
        ));
        for body in ["not-json", "[]", r#"{"rateLimit":{"weeklyLimit":"lots"}}"#] {
            assert!(
                matches!(parse_subscription(body), Err(ProviderError::Malformed(_))),
                "{body}"
            );
        }
    }

    #[test]
    fn the_key_and_the_body_the_endpoint_insists_on_are_both_on_the_request() {
        let client = Codebuff::new(Credential::new("cb-test")).expect("builds");
        let usage = client.usage_request().expect("builds");
        assert_eq!(usage.method(), reqwest::Method::POST);
        assert_eq!(
            usage.url().as_str(),
            "https://www.codebuff.com/api/v1/usage"
        );
        assert_eq!(
            usage
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer cb-test"
        );
        let body = usage
            .body()
            .expect("present")
            .as_bytes()
            .expect("in memory");
        let sent: serde_json::Value = serde_json::from_slice(body).expect("valid JSON");
        assert!(
            sent.get("fingerprintId").is_some(),
            "the endpoint refuses a request without one"
        );

        let subscription = client.subscription_request().expect("builds");
        assert_eq!(subscription.method(), reqwest::Method::GET);
        assert_eq!(
            subscription.url().as_str(),
            "https://www.codebuff.com/api/user/subscription"
        );
        assert!(!format!("{client:?}").contains("cb-test"));
    }

    #[tokio::test]
    async fn a_blank_credential_is_refused_before_a_request_is_spent() {
        let client = Codebuff::new(Credential::new("   ")).expect("builds");
        assert!(matches!(
            client.fetch().await,
            Err(ProviderError::Credential { status: 401 })
        ));
        assert_eq!(client.id().as_str(), PROVIDER_ID);
        assert_eq!(client.account(), AccountId::default());
        assert_eq!(SPEC.id, PROVIDER_ID);
    }
}
