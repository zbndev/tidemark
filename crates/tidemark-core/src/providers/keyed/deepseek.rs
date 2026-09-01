//! DeepSeek.
//!
//! Ported from CodexBar's `DeepSeek/DeepSeekUsageFetcher.swift`, the API-key path only:
//! one GET against the balance endpoint. Never seen answering: every number in the tests is
//! a body CodexBar recorded.
//!
//! # What is not ported, and why
//!
//! Cost, token counts and the daily breakdown come from DeepSeek's *platform* endpoints,
//! which take a browser session token rather than an API key — a different credential
//! acquired a different way, and out of this plan's scope. What a key can read is the
//! balance, and that is all this reads.
//!
//! # The card reading
//!
//! A balance has no limit behind it: DeepSeek reports what is *left* — total, granted,
//! topped up — and never what was spent, so there is no denominator and no share to
//! compute. Inventing one would be inventing a quota. The snapshot therefore carries no
//! window. It files the selected amount under [`DetailSection::BALANCE`], which lets the
//! card print the money without a percentage or bar.
//!
//! # What the payload does not tell you
//!
//! **The amounts are strings.** `"50.00"`, not `50.00`. A value that is not a number in a
//! string fails the whole fetch rather than being read as zero, which would report an empty
//! account to someone who has money in theirs.
//!
//! **There is a row per currency, and the useful one is not always the first.** The source
//! prefers a funded USD row, then any funded row, then an empty USD row, then whatever came
//! first — because the API has been seen returning a zeroed USD row beside a funded CNY
//! one, and taking the first would report an account with money in it as empty.
//!
//! **`is_available` is not "the account works".** It says whether the balance may be spent
//! on API calls; a funded account can report `false`, and an empty `balance_infos` is
//! reported as unavailable with nothing in it.

use super::{Auth, Method, Spec};
use crate::providers::ProviderError;
use serde::Deserialize;
use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "deepseek";

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

#[derive(Debug, Deserialize)]
struct Envelope {
    is_available: bool,
    balance_infos: Vec<Info>,
}

#[derive(Debug, Deserialize)]
struct Info {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

/// One currency's row, with its amounts read out of the strings they arrive in.
#[derive(Debug)]
struct Balance {
    currency: String,
    total: f64,
    granted: f64,
    topped_up: f64,
}

impl Balance {
    /// The symbol this currency is written with. Only the two the source knows.
    fn symbol(&self) -> &'static str {
        if self.currency == "CNY" { "¥" } else { "$" }
    }

    /// An amount in this currency, to the cent, the way the source formats it.
    fn amount(&self, value: f64) -> String {
        format!("{}{value:.2}", self.symbol())
    }
}

/// An amount that arrived as a string. Anything unreadable fails the fetch — see the module
/// doc on why this is not read as zero.
fn amount(raw: &str, field: &str) -> Result<f64, ProviderError> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| ProviderError::malformed(format!("{field} is not a number: {raw:?}")))
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_for_account(body, captured_at, &AccountId::default())
}

pub fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    let mut balances = Vec::with_capacity(envelope.balance_infos.len());
    for info in envelope.balance_infos {
        balances.push(Balance {
            total: amount(&info.total_balance, "total_balance")?,
            granted: amount(&info.granted_balance, "granted_balance")?,
            topped_up: amount(&info.topped_up_balance, "topped_up_balance")?,
            currency: info.currency,
        });
    }

    // The source's own order of preference, and its reason: a zeroed USD row has been seen
    // arriving beside a funded CNY one, and taking the first would report an account with
    // money in it as empty.
    let chosen = balances
        .iter()
        .find(|b| b.currency == "USD" && b.total > 0.0)
        .or_else(|| balances.iter().find(|b| b.total > 0.0))
        .or_else(|| balances.iter().find(|b| b.currency == "USD"))
        .or_else(|| balances.first());

    let rows = match chosen {
        // No row at all is an account with nothing in it, not an unreadable response: the
        // source reports exactly that, in dollars, with the sentence that says what to do.
        None => vec![
            DetailRow {
                label: "Balance".to_owned(),
                value: "$0.00".to_owned(),
            },
            DetailRow {
                label: "Status".to_owned(),
                value: "Add credits at platform.deepseek.com".to_owned(),
            },
        ],
        Some(balance) if balance.total <= 0.0 => vec![
            DetailRow {
                label: "Balance".to_owned(),
                value: balance.amount(0.0),
            },
            DetailRow {
                label: "Status".to_owned(),
                value: "Add credits at platform.deepseek.com".to_owned(),
            },
        ],
        Some(balance) if !envelope.is_available => vec![
            DetailRow {
                label: "Balance".to_owned(),
                value: balance.amount(balance.total),
            },
            DetailRow {
                label: "Status".to_owned(),
                value: "Unavailable for API calls".to_owned(),
            },
        ],
        Some(balance) => vec![
            DetailRow {
                label: "Balance".to_owned(),
                value: balance.amount(balance.total),
            },
            DetailRow {
                label: "Paid".to_owned(),
                value: balance.amount(balance.topped_up),
            },
            DetailRow {
                label: "Granted".to_owned(),
                value: balance.amount(balance.granted),
            },
        ],
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows: Vec::new(),
        details: vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows,
        }],
    })
}

/// DeepSeek as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "DeepSeek",
    endpoint: |_| BALANCE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "DeepSeek platform → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar, `DeepSeekUsageFetcherTests.swift` — "parses USD balance
    /// response". Its own test asserts 50.00 total, 10.00 granted, 40.00 topped up.
    const USD: &str = r#"{
      "is_available": true,
      "balance_infos": [
        {
          "currency": "USD",
          "total_balance": "50.00",
          "granted_balance": "10.00",
          "topped_up_balance": "40.00"
        }
      ]
    }"#;

    /// Recorded by CodexBar, same file — "prefers positive CNY balance over empty USD
    /// balance". Its own test asserts the CNY row wins and the card says `¥100.00`.
    const EMPTY_USD_BESIDE_FUNDED_CNY: &str = r#"{
      "is_available": true,
      "balance_infos": [
        {
          "currency": "USD",
          "total_balance": "0.00",
          "granted_balance": "0.00",
          "topped_up_balance": "0.00"
        },
        {
          "currency": "CNY",
          "total_balance": "100.00",
          "granted_balance": "0.00",
          "topped_up_balance": "100.00"
        }
      ]
    }"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn rows(snapshot: &Snapshot) -> Vec<(String, String)> {
        snapshot.details[0]
            .rows
            .iter()
            .map(|row| (row.label.clone(), row.value.clone()))
            .collect()
    }

    #[test]
    fn a_funded_balance_is_an_amount_only_reading() {
        let snapshot = parse(USD, at(1_800_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "a remaining balance says nothing about a percentage"
        );
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, "Balance");
        assert_eq!(
            rows(&snapshot),
            [
                ("Balance".to_owned(), "$50.00".to_owned()),
                ("Paid".to_owned(), "$40.00".to_owned()),
                ("Granted".to_owned(), "$10.00".to_owned()),
            ]
        );
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn a_funded_row_wins_over_an_empty_one_whatever_the_currency() {
        let snapshot = parse(EMPTY_USD_BESIDE_FUNDED_CNY, at(1_800_000_000)).expect("parses");
        assert_eq!(
            rows(&snapshot),
            [
                ("Balance".to_owned(), "¥100.00".to_owned()),
                ("Paid".to_owned(), "¥100.00".to_owned()),
                ("Granted".to_owned(), "¥0.00".to_owned()),
            ],
            "an empty USD row must not hide money in another currency"
        );

        // "prefers USD when both currencies present": both funded, USD wins.
        let both = r#"{"is_available":true,"balance_infos":[
            {"currency":"CNY","total_balance":"100.00","granted_balance":"0.00",
             "topped_up_balance":"100.00"},
            {"currency":"USD","total_balance":"20.00","granted_balance":"5.00",
             "topped_up_balance":"15.00"}]}"#;
        let snapshot = parse(both, at(1_800_000_000)).expect("parses");
        assert_eq!(rows(&snapshot)[0].1, "$20.00");
    }

    #[test]
    fn an_empty_account_says_where_to_add_credit() {
        // CodexBar's "zero balance prompts top up even when unavailable".
        let zeroed = r#"{"is_available":false,"balance_infos":[
            {"currency":"USD","total_balance":"0.00","granted_balance":"0.00",
             "topped_up_balance":"0.00"}]}"#;
        let snapshot = parse(zeroed, at(1_800_000_000)).expect("parses");
        assert_eq!(
            rows(&snapshot),
            [
                ("Balance".to_owned(), "$0.00".to_owned()),
                (
                    "Status".to_owned(),
                    "Add credits at platform.deepseek.com".to_owned()
                ),
            ]
        );
        assert!(snapshot.windows.is_empty());

        // "empty balance_infos returns unavailable snapshot": no row at all is an account
        // with nothing in it, not an unreadable response.
        let none = r#"{"is_available":true,"balance_infos":[]}"#;
        let snapshot = parse(none, at(1_800_000_000)).expect("parses");
        assert_eq!(rows(&snapshot)[0].1, "$0.00");
        assert_eq!(rows(&snapshot)[1].0, "Status");
        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn a_funded_balance_that_cannot_be_spent_says_so() {
        let body = r#"{"is_available":false,"balance_infos":[
            {"currency":"USD","total_balance":"5.00","granted_balance":"0.00",
             "topped_up_balance":"5.00"}]}"#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(
            rows(&snapshot),
            [
                ("Balance".to_owned(), "$5.00".to_owned()),
                ("Status".to_owned(), "Unavailable for API calls".to_owned()),
            ]
        );
        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn a_body_we_cannot_read_is_malformed() {
        for body in [
            // CodexBar's "throws on malformed balance string".
            r#"{"is_available":true,"balance_infos":[{"currency":"USD",
                "total_balance":"not-a-number","granted_balance":"0.00",
                "topped_up_balance":"0.00"}]}"#,
            // "throws on invalid JSON root".
            r#"[{ "is_available": true }]"#,
            r#"{"partial":"#,
            // An amount sent as a number where the API sends a string.
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":50.0,
                "granted_balance":"0.00","topped_up_balance":"0.00"}]}"#,
            // No availability flag at all.
            r#"{"balance_infos":[]}"#,
        ] {
            assert!(
                matches!(
                    parse(body, at(1_800_000_000)),
                    Err(ProviderError::Malformed(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn a_field_invented_after_this_was_written_is_ignored() {
        let body = r#"{"is_available":true,"trace":"x","balance_infos":[{"currency":"USD",
            "total_balance":"1.50","granted_balance":"0.00","topped_up_balance":"1.50",
            "frozen_balance":"9.00"}]}"#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(rows(&snapshot)[0].1, "$1.50");
    }

    #[test]
    fn the_spec_polls_the_documented_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::Options;
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.deepseek.com/user/balance"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.id, PROVIDER_ID);
    }
}
