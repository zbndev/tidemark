//! NanoGPT subscription quota and prepaid-balance readings.
//!
//! # The two requests
//!
//! NanoGPT documents `GET /api/subscription/v1/usage` for the subscription's daily and
//! monthly usage units, and `POST /api/check-balance` for the account's USD and NANO
//! balances. Both accept the same `x-api-key` header. They are independent and run
//! concurrently; both are required because a subscription can coexist with pay-as-you-go
//! credit and neither response describes the other.
//!
//! # The reading
//!
//! Subscription `percentUsed` values are fractions in `[0, 1]`, not percentages. Daily is
//! a real 24-hour window and is keyed by that length. Monthly reports its reset but not its
//! start, so its length stays absent rather than manufacturing a billing-period duration.
//! An active or grace subscription publishes both windows.
//!
//! A prepaid balance has no denominator. USD is therefore the first row of
//! [`DetailSection::BALANCE`], which lets the card show the amount without inventing a bar;
//! NANO and the deposit address remain detail rows. The successful fixtures below are the
//! examples published in NanoGPT's API reference, not synthesized account data.

use super::{HandSpec, Options, ProviderError, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, http};
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

pub const PROVIDER_ID: &str = "nanogpt";

const SUBSCRIPTION_URL: &str = "https://nano-gpt.com/api/subscription/v1/usage";
const BALANCE_URL: &str = "https://nano-gpt.com/api/check-balance";

pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "NanoGPT",
    credential: CredentialKind::Key,
    credential_hint: "nano-gpt.com/settings → API keys.",
    options: &[],
    build,
};

fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(NanoGpt::new(credential)?))
}

pub struct NanoGpt {
    client: reqwest::Client,
    credential: Credential,
}

impl NanoGpt {
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    fn subscription_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(SUBSCRIPTION_URL)
            .header("x-api-key", self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    fn balance_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(BALANCE_URL)
            .header("x-api-key", self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let subscription_request = self.subscription_request()?;
        let balance_request = self.balance_request()?;
        let (subscription, balance) = tokio::join!(
            super::request(&self.client, subscription_request),
            super::request(&self.client, balance_request),
        );
        parse(&subscription?, &balance?, Timestamp::now())
    }
}

impl fmt::Debug for NanoGpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NanoGpt")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for NanoGpt {
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

#[derive(Debug, Deserialize)]
struct Subscription {
    active: bool,
    state: SubscriptionState,
    limits: Limits,
    daily: Quota,
    monthly: Quota,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SubscriptionState {
    Active,
    Grace,
    Inactive,
}

#[derive(Debug, Deserialize)]
struct Limits {
    daily: f64,
    monthly: f64,
}

#[derive(Debug, Deserialize)]
struct Quota {
    used: f64,
    remaining: f64,
    #[serde(rename = "percentUsed")]
    percent_used: f64,
    #[serde(rename = "resetAt")]
    reset_at: i64,
}

#[derive(Debug, Deserialize)]
struct Balance {
    usd_balance: String,
    nano_balance: String,
    #[serde(rename = "nanoDepositAddress")]
    nano_deposit_address: String,
}

fn amount(raw: &str, field: &str) -> Result<f64, ProviderError> {
    let value = raw
        .parse::<f64>()
        .map_err(|_| ProviderError::malformed(format!("{field} is not a number")))?;
    if !value.is_finite() {
        return Err(ProviderError::malformed(format!(
            "{field} is not a finite number"
        )));
    }
    Ok(value)
}

fn reset(raw: i64, field: &str) -> Result<Timestamp, ProviderError> {
    Timestamp::from_unix_millis(raw)
        .map_err(|_| ProviderError::malformed(format!("{field} is not a plausible timestamp")))
}

fn window(
    key: WindowKey,
    title: &str,
    length: Option<WindowLength>,
    quota: &Quota,
    limit: f64,
) -> Result<Window, ProviderError> {
    if !quota.used.is_finite()
        || !quota.remaining.is_finite()
        || !quota.percent_used.is_finite()
        || !limit.is_finite()
    {
        return Err(ProviderError::malformed(format!(
            "{title} quota contains a non-finite number"
        )));
    }
    Ok(Window {
        key,
        title: title.to_owned(),
        subtitle: Some(format!("{} / {} units", quota.used, limit)),
        used_percent: (quota.percent_used * 100.0).clamp(0.0, 100.0),
        resets_at: Some(reset(quota.reset_at, title)?),
        length,
    })
}

fn parse(
    subscription_body: &str,
    balance_body: &str,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let subscription: Subscription = serde_json::from_str(subscription_body)
        .map_err(|e| ProviderError::malformed(format!("unreadable subscription usage: {e}")))?;
    let balance: Balance = serde_json::from_str(balance_body)
        .map_err(|e| ProviderError::malformed(format!("unreadable balance: {e}")))?;
    let usd = amount(&balance.usd_balance, "usd_balance")?;
    amount(&balance.nano_balance, "nano_balance")?;

    let mut windows = Vec::with_capacity(2);
    if subscription.active || subscription.state == SubscriptionState::Grace {
        let day = WindowLength::from_secs(86_400).expect("one day is nonzero");
        windows.push(window(
            WindowKey::for_length(day),
            "Daily",
            Some(day),
            &subscription.daily,
            subscription.limits.daily,
        )?);
        windows.push(window(
            WindowKey::named("monthly"),
            "Monthly",
            None,
            &subscription.monthly,
            subscription.limits.monthly,
        )?);
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows: vec![
                DetailRow {
                    label: "Balance".to_owned(),
                    value: format!("${usd:.2}"),
                },
                DetailRow {
                    label: "Nano balance".to_owned(),
                    value: format!("{} NANO", balance.nano_balance),
                },
                DetailRow {
                    label: "Deposit address".to_owned(),
                    value: balance.nano_deposit_address,
                },
            ],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::{NanoGpt, Options, SPEC, parse};
    use crate::providers::Credential;
    use tidemark_types::{CredentialKind, DetailSection, Timestamp};

    const SUBSCRIPTION: &str = r#"{
      "active": true,
      "limits": { "daily": 5000, "monthly": 60000 },
      "enforceDailyLimit": true,
      "daily": {
        "used": 5,
        "remaining": 4995,
        "percentUsed": 0.001,
        "resetAt": 1738540800000
      },
      "monthly": {
        "used": 45,
        "remaining": 59955,
        "percentUsed": 0.00075,
        "resetAt": 1739404800000
      },
      "period": {
        "currentPeriodEnd": "2025-02-13T23:59:59.000Z"
      },
      "state": "active",
      "graceUntil": null
    }"#;

    const BALANCE: &str = r#"{
      "usd_balance": "129.46956147",
      "nano_balance": "26.71801147",
      "nanoDepositAddress": "nano_1gx385nnj7rw67hsksa3pyxwnfr48zu13t35ncjmtnqb9zdebtjhh7ahks34"
    }"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_documented_responses_show_subscription_quotas_and_balance() {
        let snapshot = parse(SUBSCRIPTION, BALANCE, at(1_738_000_000)).expect("parses");

        assert_eq!(snapshot.provider.as_str(), "nanogpt");
        assert_eq!(snapshot.windows.len(), 2);

        let daily = &snapshot.windows[0];
        assert_eq!(daily.key.as_str(), "w86400");
        assert_eq!(daily.title, "Daily");
        assert_eq!(daily.used_percent, 0.1);
        assert_eq!(daily.subtitle.as_deref(), Some("5 / 5000 units"));
        assert_eq!(daily.length.expect("known").as_secs(), 86_400);
        assert_eq!(daily.resets_at.expect("reported").as_unix(), 1_738_540_800);

        let monthly = &snapshot.windows[1];
        assert_eq!(monthly.key.as_str(), "monthly");
        assert_eq!(monthly.title, "Monthly");
        assert_eq!(monthly.used_percent, 0.075);
        assert_eq!(monthly.subtitle.as_deref(), Some("45 / 60000 units"));
        assert!(monthly.length.is_none());
        assert_eq!(
            monthly.resets_at.expect("reported").as_unix(),
            1_739_404_800
        );

        let balance = snapshot
            .details
            .iter()
            .find(|section| section.title == DetailSection::BALANCE)
            .expect("balance section");
        assert_eq!(balance.rows[0].label, "Balance");
        assert_eq!(balance.rows[0].value, "$129.47");
        assert_eq!(balance.rows[1].label, "Nano balance");
        assert_eq!(balance.rows[1].value, "26.71801147 NANO");
        assert_eq!(balance.rows[2].label, "Deposit address");
        assert_eq!(
            balance.rows[2].value,
            "nano_1gx385nnj7rw67hsksa3pyxwnfr48zu13t35ncjmtnqb9zdebtjhh7ahks34"
        );
    }

    #[test]
    fn a_recognized_quota_with_a_malformed_remaining_value_fails_the_snapshot() {
        let malformed = SUBSCRIPTION.replacen("\"remaining\": 4995", "\"remaining\": \"many\"", 1);

        assert!(matches!(
            parse(&malformed, BALANCE, at(1_738_000_000)),
            Err(super::ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn a_subscription_in_grace_keeps_its_reported_windows() {
        let grace = SUBSCRIPTION
            .replacen("\"active\": true", "\"active\": false", 1)
            .replacen("\"state\": \"active\"", "\"state\": \"grace\"", 1);

        let snapshot = parse(&grace, BALANCE, at(1_738_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
    }

    #[test]
    fn both_documented_requests_use_the_same_header_key() {
        let provider = NanoGpt::new(Credential::new("not-a-real-key")).expect("builds");
        let subscription = provider.subscription_request().expect("builds");
        let balance = provider.balance_request().expect("builds");

        assert_eq!(subscription.method(), reqwest::Method::GET);
        assert_eq!(
            subscription.url().as_str(),
            "https://nano-gpt.com/api/subscription/v1/usage"
        );
        assert_eq!(balance.method(), reqwest::Method::POST);
        assert_eq!(
            balance.url().as_str(),
            "https://nano-gpt.com/api/check-balance"
        );
        assert!(balance.body().is_none());
        for request in [subscription, balance] {
            assert_eq!(
                request.headers().get("x-api-key").expect("present"),
                "not-a-real-key"
            );
        }
    }

    #[test]
    fn the_spec_builds_a_key_authenticated_nanogpt_provider() {
        assert_eq!(SPEC.id, "nanogpt");
        assert_eq!(SPEC.title, "NanoGPT");
        assert_eq!(SPEC.credential, CredentialKind::Key);
        assert!(SPEC.options.is_empty());

        let provider =
            (SPEC.build)(Credential::new("not-a-real-key"), &Options::new()).expect("builds");
        assert_eq!(provider.id().as_str(), "nanogpt");
    }

    #[test]
    fn a_nanogpt_client_never_prints_its_credential() {
        let provider = NanoGpt::new(Credential::new("do-not-print-this")).expect("builds");
        let debug = format!("{provider:?}");

        assert!(debug.contains("nanogpt"));
        assert!(!debug.contains("do-not-print-this"));
    }
}
