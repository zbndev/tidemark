//! Moonshot AI.
//!
//! Ported from CodexBar's `Moonshot/MoonshotUsageFetcher.swift` and `MoonshotRegion.swift`.
//! Never seen answering: every number in the tests is a body CodexBar recorded.
//!
//! # This card is empty on purpose
//!
//! The balance endpoint is the whole of what a Moonshot key can read, and a balance has no
//! limit behind it: there is nothing to divide by, so there is no share to draw a bar
//! against. The design's rule for that shape is a detail section and no window — see
//! `docs/superpowers/specs/2026-08-21-keyed-provider-port-design.md`. Inventing a limit to
//! fill a bar would be drawing a number the provider never reported.
//!
//! # What the payload does not tell you
//!
//! `available_balance` is not `voucher_balance + cash_balance`: vouchers are spent first
//! and expire, cash is what remains of money paid, and the available figure is the source's
//! own arithmetic over both. All three are reported, because which one matters depends on
//! whether the account is running on credits or on cash.
//!
//! `cash_balance` goes **negative**: an account that has spent past its paid balance owes
//! the difference, and the source calls that a deficit rather than a balance of minus
//! forty-two cents. A negative available balance is not a shape the source has recorded, so
//! it is reported as it arrives.
//!
//! # The failure that arrives as a success
//!
//! A rejected key comes back inside the body — `{"code":401,"scode":"unauthorized",
//! "status":false}` — not in the HTTP status, which is the fixture CodexBar records. That
//! is a credential error rather than an unreadable response, so the interface asks for a new
//! key instead of reporting that the provider broke. Any other non-zero `code` is
//! malformed, carrying the provider's own `code` and `scode` so the message says what it
//! actually said.
//!
//! # The two hosts
//!
//! `api.moonshot.ai` and `api.moonshot.cn` are the same API, and a key issued for one is
//! rejected by the other — the same shape as Z.ai's two regions, published as a choice for
//! the same reason.

use super::{Auth, Method, OptionSchema, Spec};
use crate::providers::ProviderError;
use serde::Deserialize;
use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "moonshot";

/// Path appended to the region's base URL.
const BALANCE_PATH: &str = "/v1/users/me/balance";

/// Name of the region setting under `[provider.moonshot]`.
pub const REGION: &str = "region";

/// Which deployment the account lives on. The two are the same API on different hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Region {
    /// `api.moonshot.ai`.
    #[default]
    International,
    /// `api.moonshot.cn`.
    China,
}

impl Region {
    /// Base URL for this region.
    pub fn base_url(self) -> &'static str {
        match self {
            Self::International => "https://api.moonshot.ai",
            Self::China => "https://api.moonshot.cn",
        }
    }

    /// The value this region is stored as in `config.toml`.
    pub fn as_value(self) -> &'static str {
        match self {
            Self::International => "international",
            Self::China => "china",
        }
    }

    /// The region a stored value names. An unrecognised value is the default rather than
    /// an error: a typo in `config.toml` must not take the account off the air.
    pub fn from_value(raw: Option<&str>) -> Self {
        match raw {
            Some("china") => Self::China,
            _ => Self::International,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    code: i64,
    data: Balance,
    scode: String,
    status: bool,
}

#[derive(Debug, Deserialize)]
struct Balance {
    available_balance: f64,
    voucher_balance: f64,
    cash_balance: f64,
}

/// A dollar amount in the source's own formatting: two decimals, always.
fn dollars(amount: f64) -> String {
    format!("${amount:.2}")
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    if envelope.code != 0 || !envelope.status {
        // A rejected key is reported in the body, not in the status. See the module doc.
        if envelope.code == 401 {
            return Err(ProviderError::Credential { status: 401 });
        }
        return Err(ProviderError::malformed(format!(
            "the provider reported code {}, scode {}",
            envelope.code, envelope.scode
        )));
    }

    let balance = envelope.data;
    for amount in [
        balance.available_balance,
        balance.voucher_balance,
        balance.cash_balance,
    ] {
        if !amount.is_finite() {
            return Err(ProviderError::malformed("a balance must be a number"));
        }
    }

    let mut rows = vec![DetailRow {
        label: "Balance".to_owned(),
        value: dollars(balance.available_balance),
    }];
    rows.push(DetailRow {
        label: "Vouchers".to_owned(),
        value: dollars(balance.voucher_balance),
    });
    rows.push(if balance.cash_balance < 0.0 {
        // The source's own wording: an account past its paid balance owes the difference
        // rather than holding minus forty-two cents of it.
        DetailRow {
            label: "Cash".to_owned(),
            value: format!("{} in deficit", dollars(-balance.cash_balance)),
        }
    } else {
        DetailRow {
            label: "Cash".to_owned(),
            value: dollars(balance.cash_balance),
        }
    });

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        // No limit was reported, so there is no share to draw. See the module doc.
        windows: Vec::new(),
        details: vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows,
        }],
    })
}

/// Moonshot as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Moonshot",
    endpoint: |options| {
        let region = Region::from_value(options.get(REGION).map(String::as_str));
        format!("{}{BALANCE_PATH}", region.base_url())
    },
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse,
    credential_hint: "Moonshot console → API keys, on whichever region your account is on.",
    options: &[OptionSchema {
        name: REGION,
        title: "Region",
        description: Some(
            "The same API on two hosts. A key issued for one is rejected by the other.",
        ),
        default: "international",
        choices: &[
            ("international", "International (api.moonshot.ai)"),
            ("china", "China (api.moonshot.cn)"),
        ],
        required: false,
    }],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::keyed::Options;

    /// Recorded by CodexBar, `MoonshotUsageFetcherTests.swift` — "parses documented
    /// response". Its own test asserts 49.58, 50.00 and 12.34 for this body, and
    /// `Balance: $49.58` on the card.
    const DOCUMENTED: &str = r#"{
      "code": 0,
      "data": {
        "available_balance": 49.58,
        "voucher_balance": 50.00,
        "cash_balance": 12.34
      },
      "scode": "0x0",
      "status": true
    }"#;

    /// Recorded by CodexBar, same file — "negative cash balance is surfaced as deficit".
    const IN_DEFICIT: &str = r#"{
      "code": 0,
      "data": {
        "available_balance": 49.58,
        "voucher_balance": 50.00,
        "cash_balance": -0.42
      },
      "scode": "0x0",
      "status": true
    }"#;

    /// Recorded by CodexBar, same file — "api code failure returns api error".
    const UNAUTHORIZED: &str = r#"{
      "code": 401,
      "data": {
        "available_balance": 0,
        "voucher_balance": 0,
        "cash_balance": 0
      },
      "scode": "unauthorized",
      "status": false
    }"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_balance_with_no_limit_draws_no_bar_and_says_what_it_is() {
        let snapshot = parse(DOCUMENTED, at(1_800_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "nothing said what the balance is out of; a bar would be invented"
        );
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, DetailSection::PLAN);
        let rows: Vec<(&str, &str)> = snapshot.details[0]
            .rows
            .iter()
            .map(|row| (row.label.as_str(), row.value.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Balance", "$49.58"),
                ("Vouchers", "$50.00"),
                ("Cash", "$12.34"),
            ]
        );
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn an_overspent_account_owes_the_difference_rather_than_holding_a_negative_balance() {
        let snapshot = parse(IN_DEFICIT, at(1_800_000_000)).expect("parses");
        let cash = snapshot.details[0]
            .rows
            .iter()
            .find(|row| row.label == "Cash")
            .expect("the cash row is always drawn");
        assert_eq!(cash.value, "$0.42 in deficit");
    }

    #[test]
    fn a_key_the_provider_rejects_inside_a_body_asks_for_a_new_key() {
        assert!(
            matches!(
                parse(UNAUTHORIZED, at(1_800_000_000)),
                Err(ProviderError::Credential { status: 401 })
            ),
            "the rejection arrives in the payload, not in the HTTP status"
        );
    }

    #[test]
    fn any_other_reported_failure_carries_the_providers_own_words() {
        let body = r#"{"code":42,"data":{"available_balance":0,"voucher_balance":0,
            "cash_balance":0},"scode":"quota_frozen","status":false}"#;
        match parse(body, at(1_800_000_000)) {
            Err(ProviderError::Malformed(message)) => {
                assert!(message.contains("42"), "{message}");
                assert!(message.contains("quota_frozen"), "{message}");
            }
            other => panic!("expected a malformed body, got {other:?}"),
        }
    }

    #[test]
    fn a_body_we_cannot_read_is_malformed() {
        for body in [
            // CodexBar's "invalid root returns parse error".
            r#"[{ "available_balance": 1 }]"#,
            r#"{"partial":"#,
            // A balance where a number belongs.
            r#"{"code":0,"data":{"available_balance":"49.58","voucher_balance":50,
                "cash_balance":12.34},"scode":"0x0","status":true}"#,
            // The envelope without its data.
            r#"{"code":0,"scode":"0x0","status":true}"#,
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
        let body = r#"{"code":0,"data":{"available_balance":1.5,"voucher_balance":0,
            "cash_balance":1.5,"crypto_balance":9},"scode":"0x0","status":true,"trace":"x"}"#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.details[0].rows[0].value, "$1.50");
    }

    #[test]
    fn the_region_chooses_the_host_and_an_unknown_value_falls_back() {
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.moonshot.ai/v1/users/me/balance"
        );
        assert_eq!(
            (SPEC.endpoint)(&options(&[(REGION, "china")])),
            "https://api.moonshot.cn/v1/users/me/balance"
        );
        assert_eq!(
            (SPEC.endpoint)(&options(&[(REGION, "mars")])),
            "https://api.moonshot.ai/v1/users/me/balance",
            "a typo in the settings file must not take the account off the air"
        );
        assert_eq!(Region::from_value(Some("china")).as_value(), "china");
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.options[0].default, Region::default().as_value());
    }
}
