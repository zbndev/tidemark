//! NanoGPT subscription quota and prepaid-balance readings.
//!
//! # The two requests
//!
//! NanoGPT serves `GET /api/subscription/v1/usage` for metered allowances and
//! `POST /api/check-balance` for the account's USD and NANO balances. Both accept the same
//! `x-api-key` header. They are independent and run concurrently; both are required
//! because metered allowances coexist with pay-as-you-go credit and neither response
//! describes the other.
//!
//! # What the usage endpoint really returns
//!
//! Not what its reference documents. The published example is a fixed `daily`/`monthly`
//! pair; a live account instead returns one object per metered pool, each named for its
//! own period — `dailyImages`, `dailyInputTokens`, `weeklyInputTokens` — with the pools
//! the account has no allowance for present but `null`, and a parallel `limits` object
//! keyed by those same names. Requiring the documented fields rejected every real
//! response with `missing field \`daily\``, so the reading is driven by what the body
//! names rather than by fields this module hopes to find.
//!
//! # The reading
//!
//! `percentUsed` values are fractions in `[0, 1]`, not percentages. The leading word of a
//! metric name states the period, which is the only place a length appears: `daily` is a
//! real 24-hour window, `weekly` a seven-day one, and `monthly` a billing month whose
//! length is not fixed, so it is named rather than keyed by a duration and draws no pace
//! mark. Because two pools can share a period, keys carry the pool as well as the length —
//! `images/w86400` and `input-tokens/w86400` are two windows, not one. A period this build
//! has never seen is skipped; a pool whose numbers cannot be read fails the whole fetch. A
//! stated limit becomes the absolutes under the bar, and a pool with no stated limit keeps
//! its percentage and prints nothing it would have to invent.
//!
//! A prepaid balance has no denominator. USD is therefore the first row of
//! [`DetailSection::BALANCE`], which lets the card show the amount without inventing a bar;
//! NANO and the deposit address remain detail rows. The fixtures below are recorded
//! responses, not synthesized account data.

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

/// One metric object: what a period has consumed, as every NanoGPT usage field reports it.
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

/// The period a metric name's leading word states, that word, and the pool the rest of the
/// name identifies.
///
/// The live API does not report the `daily`/`monthly` pair its reference documents. It
/// reports one object per metered thing — `dailyImages`, `weeklyInputTokens`,
/// `dailyInputTokens` — and the leading word is the only place the period ever appears.
/// Reading the length out of that word is what keeps [`WindowKey`] derived from length and
/// pool: `dailyImages` and `dailyInputTokens` become two day-long windows drawing on
/// different pools instead of one silently replacing the other. A leading word this build
/// has never seen states a period of unknown length and is skipped rather than guessed at.
fn period(name: &str) -> Option<(&'static str, Option<WindowLength>, &str)> {
    let length = |seconds| Some(WindowLength::from_secs(seconds).expect("a period is nonzero"));
    for (word, length) in [
        ("daily", length(86_400)),
        ("weekly", length(604_800)),
        // A billing month is not a fixed number of seconds, so there is no length to key
        // on and no pace mark to draw. See the `WindowKey::named` call below.
        ("monthly", None),
    ] {
        if let Some(rest) = name.strip_prefix(word)
            && (rest.is_empty() || rest.starts_with(|first: char| first.is_ascii_uppercase()))
        {
            return Some((word, length, rest));
        }
    }
    None
}

/// `InputTokens` → `input tokens`: the provider's own metric name, in prose.
fn spaced(rest: &str) -> String {
    let mut text = String::with_capacity(rest.len() + 2);
    for character in rest.chars() {
        if character.is_ascii_uppercase() && !text.is_empty() {
            text.push(' ');
        }
        text.extend(character.to_lowercase());
    }
    text
}

/// What the numbers under the bar are counted in. NanoGPT meters tokens, images and — in
/// the documented subscription shape — nothing more specific than usage units.
fn unit(pool: &str) -> &str {
    pool.rsplit(' ')
        .next()
        .filter(|word| !word.is_empty())
        .unwrap_or("units")
}

/// The period word capitalised, and the pool after it: `Weekly input tokens`.
fn title(word: &str, pool: &str) -> String {
    let mut title = String::with_capacity(word.len() + pool.len() + 1);
    let mut characters = word.chars();
    if let Some(first) = characters.next() {
        title.extend(first.to_uppercase());
        title.push_str(characters.as_str());
    }
    if !pool.is_empty() {
        title.push(' ');
        title.push_str(pool);
    }
    title
}

/// Whole counts with thousands separators, keeping one fraction digit only where there is
/// one: `93,176`, `60,000,000`, `1.5`.
fn compact(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let tenths = (value.abs() * 10.0).round() as i128;
    let mut text = format!("{sign}{}", grouped(tenths / 10));
    if tenths % 10 > 0 {
        text.push('.');
        text.push_str(&(tenths % 10).to_string());
    }
    text
}

fn grouped(whole: i128) -> String {
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

fn window(
    key: WindowKey,
    title: String,
    unit: &str,
    length: Option<WindowLength>,
    quota: &Quota,
    limit: Option<f64>,
) -> Result<Window, ProviderError> {
    if !quota.used.is_finite()
        || !quota.remaining.is_finite()
        || !quota.percent_used.is_finite()
        || limit.is_some_and(|limit| !limit.is_finite())
    {
        return Err(ProviderError::malformed(format!(
            "{title} quota contains a non-finite number"
        )));
    }
    let resets_at = reset(quota.reset_at, &title)?;
    Ok(Window {
        key,
        // Without a limit there is no denominator to print, and inventing one would be a
        // lie the card draws in small type. The percentage still draws the bar.
        subtitle: limit.map(|limit| format!("{} / {} {unit}", compact(quota.used), compact(limit))),
        used_percent: (quota.percent_used * 100.0).clamp(0.0, 100.0),
        resets_at: Some(resets_at),
        length,
        title,
    })
}

fn parse(
    subscription_body: &str,
    balance_body: &str,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let usage: serde_json::Map<String, serde_json::Value> = serde_json::from_str(subscription_body)
        .map_err(|e| ProviderError::malformed(format!("unreadable subscription usage: {e}")))?;
    let balance: Balance = serde_json::from_str(balance_body)
        .map_err(|e| ProviderError::malformed(format!("unreadable balance: {e}")))?;
    let usd = amount(&balance.usd_balance, "usd_balance")?;
    amount(&balance.nano_balance, "nano_balance")?;

    let limits = usage.get("limits").and_then(serde_json::Value::as_object);
    let mut windows = Vec::new();
    for (name, reported) in &usage {
        // A metric the account has no allowance for arrives as `null`. That is an absent
        // window, not a broken one: nothing is reported, so nothing is published.
        if name == "limits" || reported.is_null() {
            continue;
        }
        let Some((word, length, rest)) = period(name) else {
            continue;
        };
        let quota = Quota::deserialize(reported)
            .map_err(|e| ProviderError::malformed(format!("unreadable {name} usage: {e}")))?;
        let limit = match limits.and_then(|limits| limits.get(name)) {
            None | Some(serde_json::Value::Null) => None,
            Some(limit) => Some(limit.as_f64().ok_or_else(|| {
                ProviderError::malformed(format!("{name} limit is not a number"))
            })?),
        };
        let pool = spaced(rest);
        let key = match length {
            Some(length) if pool.is_empty() => WindowKey::for_length(length),
            Some(length) => WindowKey::for_pool(&pool.replace(' ', "-"), length),
            // Named, not keyed by length, because a billing month does not have one. The
            // name is still built from the period and the pool rather than from the field
            // the provider happened to send them in.
            None if pool.is_empty() => WindowKey::named(word),
            None => WindowKey::named(&format!("{word}-{}", pool.replace(' ', "-"))),
        };
        windows.push(window(
            key,
            title(word, &pool),
            unit(&pool),
            length,
            &quota,
            limit,
        )?);
    }
    // Shortest window first, the order the card reads best in. `serde_json` hands back a
    // sorted map, so this is a presentation order rather than a tie-break for randomness.
    windows.sort_by(|left, right| {
        let length = |window: &Window| window.length.map_or(u64::MAX, WindowLength::as_secs);
        length(left)
            .cmp(&length(right))
            .then_with(|| left.title.cmp(&right.title))
    });

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

    /// A recorded `GET /api/subscription/v1/usage` response. This is the shape the live API
    /// actually returns: one object per metered pool, named for its period, with the pools
    /// the account has no allowance for reported as `null`. The `daily`/`monthly` pair in
    /// NanoGPT's reference does not appear.
    const LIVE: &str = r#"{"active":true,"provider":"balance","providerStatus":null,"providerStatusRaw":null,"stripeSubscriptionId":null,"cancellationReason":null,"canceledAt":null,"endedAt":null,"cancelAt":null,"cancelAtPeriodEnd":false,"limits":{"weeklyInputTokens":60000000,"dailyInputTokens":null,"dailyImages":100},"allowOverage":false,"period":{"currentPeriodEnd":"2026-09-19T22:57:50.820Z"},"dailyImages":{"used":0,"remaining":100,"percentUsed":0,"resetAt":1787616000000},"dailyInputTokens":null,"weeklyInputTokens":{"used":93176,"remaining":59906824,"percentUsed":0.0015529333333333334,"resetAt":1788134400000},"state":"active","graceUntil":null}"#;

    /// The example published in NanoGPT's API reference. No account has been observed
    /// returning it, but the periods it names are read the same way as the live ones.
    const DOCUMENTED: &str = r#"{
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
    fn the_live_response_publishes_a_window_for_every_pool_it_meters() {
        let snapshot = parse(LIVE, BALANCE, at(1_787_600_000)).expect("parses");

        assert_eq!(snapshot.provider.as_str(), "nanogpt");
        // Three pools are named; `dailyInputTokens` is null, so two are reported.
        assert_eq!(snapshot.windows.len(), 2);

        let images = &snapshot.windows[0];
        assert_eq!(images.key.as_str(), "images/w86400");
        assert_eq!(images.title, "Daily images");
        assert_eq!(images.used_percent, 0.0);
        assert_eq!(images.subtitle.as_deref(), Some("0 / 100 images"));
        assert_eq!(images.length.expect("known").as_secs(), 86_400);
        assert_eq!(images.resets_at.expect("reported").as_unix(), 1_787_616_000);

        let tokens = &snapshot.windows[1];
        assert_eq!(tokens.key.as_str(), "input-tokens/w604800");
        assert_eq!(tokens.title, "Weekly input tokens");
        assert!((tokens.used_percent - 0.155_293_333_333_333_34).abs() < 1e-12);
        assert_eq!(
            tokens.subtitle.as_deref(),
            Some("93,176 / 60,000,000 tokens")
        );
        assert_eq!(tokens.length.expect("known").as_secs(), 604_800);
        assert_eq!(tokens.resets_at.expect("reported").as_unix(), 1_788_134_400);

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
    fn two_pools_of_the_same_length_do_not_share_one_key() {
        // The recorded body with the null `dailyInputTokens` metric filled in: its limit
        // stays null, so this is also the case where no denominator is stated.
        let both_daily = LIVE.replacen(
            r#""dailyInputTokens":null,"weeklyInputTokens""#,
            r#""dailyInputTokens":{"used":10,"remaining":0,"percentUsed":1,"resetAt":1787616000000},"weeklyInputTokens""#,
            1,
        );

        let snapshot = parse(&both_daily, BALANCE, at(1_787_600_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "images/w86400",
                "input-tokens/w86400",
                "input-tokens/w604800"
            ]
        );

        let tokens = &snapshot.windows[1];
        assert_eq!(tokens.title, "Daily input tokens");
        assert_eq!(tokens.used_percent, 100.0);
        assert_eq!(tokens.subtitle, None, "no limit is stated for this pool");
    }

    #[test]
    fn a_response_that_meters_nothing_still_reports_the_balance() {
        let nothing = LIVE
            .replacen(
                r#""dailyImages":{"used":0,"remaining":100,"percentUsed":0,"resetAt":1787616000000}"#,
                r#""dailyImages":null"#,
                1,
            )
            .replacen(
                r#""weeklyInputTokens":{"used":93176,"remaining":59906824,"percentUsed":0.0015529333333333334,"resetAt":1788134400000}"#,
                r#""weeklyInputTokens":null"#,
                1,
            );

        let snapshot = parse(&nothing, BALANCE, at(1_787_600_000)).expect("parses");
        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.details[0].rows[0].value, "$129.47");
    }

    #[test]
    fn a_recognized_pool_with_a_malformed_number_fails_the_snapshot() {
        let malformed = LIVE.replacen(r#""used":93176"#, r#""used":"many""#, 1);

        assert!(matches!(
            parse(&malformed, BALANCE, at(1_787_600_000)),
            Err(super::ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn a_limit_that_is_not_a_number_fails_rather_than_being_dropped() {
        let malformed = LIVE.replacen(
            r#""weeklyInputTokens":60000000"#,
            r#""weeklyInputTokens":"lots""#,
            1,
        );

        assert!(matches!(
            parse(&malformed, BALANCE, at(1_787_600_000)),
            Err(super::ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn a_period_this_build_does_not_know_is_skipped_rather_than_guessed_at() {
        let unknown = LIVE.replacen(
            r#""dailyImages":{"used":0"#,
            r#""fortnightlyImages":{"used":0"#,
            1,
        );

        let snapshot = parse(&unknown, BALANCE, at(1_787_600_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["input-tokens/w604800"]);
    }

    #[test]
    fn the_documented_shape_reads_as_a_day_and_a_billing_month() {
        let snapshot = parse(DOCUMENTED, BALANCE, at(1_738_000_000)).expect("parses");

        assert_eq!(snapshot.windows.len(), 2);

        let daily = &snapshot.windows[0];
        assert_eq!(daily.key.as_str(), "w86400");
        assert_eq!(daily.title, "Daily");
        assert_eq!(daily.used_percent, 0.1);
        assert_eq!(daily.subtitle.as_deref(), Some("5 / 5,000 units"));
        assert_eq!(daily.length.expect("known").as_secs(), 86_400);
        assert_eq!(daily.resets_at.expect("reported").as_unix(), 1_738_540_800);

        // A billing month has no fixed length, so it is named rather than keyed by one and
        // it carries no pace mark.
        let monthly = &snapshot.windows[1];
        assert_eq!(monthly.key.as_str(), "monthly");
        assert_eq!(monthly.title, "Monthly");
        assert_eq!(monthly.used_percent, 0.075);
        assert_eq!(monthly.subtitle.as_deref(), Some("45 / 60,000 units"));
        assert!(monthly.length.is_none());
        assert_eq!(
            monthly.resets_at.expect("reported").as_unix(),
            1_739_404_800
        );
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
