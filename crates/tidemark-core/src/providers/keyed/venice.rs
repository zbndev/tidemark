//! Venice.
//!
//! Ported from CodexBar's `venice.js` plugin. Never seen answering: every number in the
//! tests is a body CodexBar recorded.
//!
//! # One endpoint, two currencies
//!
//! `GET /api/v1/billing/balance` answers with two balances at once — `balances.diem` and
//! `balances.usd` — and one of them is the limit being drawn down, decided by
//! `consumptionCurrency`. The DIEM balance against `diemEpochAllocation` is a balance
//! window: used over limit, both absolutes under the bar. The USD balance has no
//! allocation to divide by, so it is a detail row and never a window — a bar over no
//! limit is the one drawing this port must not make. When the account consumes in USD
//! the epoch allocation is dormant (the plugin shows the USD balance alone for that
//! body), so the window is drawn only while the consumption currency is not USD.
//!
//! # What the payload does not tell you
//!
//! Every amount may arrive quoted — `"90.50"` — including the allocation, and the plugin
//! trims before parsing. An empty string counts as absent; a value that is neither a
//! number nor a numeric string fails the whole fetch, because a balance we cannot read
//! must not be drawn as one we did.
//!
//! `canConsume` must be a boolean and `balances` must be an object; the plugin refuses
//! anything else, and so does this port (an array at the top level is one of the bodies
//! CodexBar recorded as malformed).
//!
//! Two states draw a full bar rather than an empty card: `canConsume: false` — the key
//! cannot spend at all, whatever the balances say — and no balance to draw on. Both
//! carry the plugin's own sentence under the bar, so the emptiness reads as the state it
//! is rather than as "no limit known". Percentages clamp at both ends: a DIEM balance
//! above its allocation reads 0% used, not a negative fill.

use super::{Auth, Method, Spec};
use crate::providers::ProviderError;
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "venice";

const BALANCE_URL: &str = "https://api.venice.ai/api/v1/billing/balance";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    can_consume: bool,
    #[serde(default)]
    consumption_currency: Option<String>,
    balances: Balances,
    #[serde(default)]
    diem_epoch_allocation: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Balances {
    #[serde(default)]
    diem: Option<serde_json::Value>,
    #[serde(default)]
    usd: Option<serde_json::Value>,
}

/// One amount, however this endpoint spells it: a number, or the same number quoted.
/// Null and the empty string read as absent — the plugin's `optionalNumber`, which also
/// refuses anything else, as this does by failing the fetch.
fn amount(value: Option<&serde_json::Value>, field: &str) -> Result<Option<f64>, ProviderError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(raw)) => raw
            .as_f64()
            .map(Some)
            .ok_or_else(|| ProviderError::malformed(format!("{field} must be numeric"))),
        Some(serde_json::Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let parsed: f64 = trimmed
                .parse()
                .map_err(|_| ProviderError::malformed(format!("{field} must be numeric")))?;
            if !parsed.is_finite() {
                return Err(ProviderError::malformed(format!("{field} must be numeric")));
            }
            Ok(Some(parsed))
        }
        Some(other) => Err(ProviderError::malformed(format!(
            "{field} must be numeric, not {}",
            type_name(other)
        ))),
    }
}

fn type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
        _ => "not a number",
    }
}

/// The balance window, or one of the two exhausted states, as one window shape.
fn balance_window(used_percent: f64, subtitle: String) -> Window {
    // A balance has no length to key on: it does not roll over, it drains.
    Window {
        key: WindowKey::named("balance"),
        title: "Balance".to_owned(),
        subtitle: Some(subtitle),
        used_percent,
        resets_at: None,
        length: None,
    }
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

    let diem = amount(envelope.balances.diem.as_ref(), "balances.diem")?;
    let usd = amount(envelope.balances.usd.as_ref(), "balances.usd")?;
    let allocation = amount(
        envelope.diem_epoch_allocation.as_ref(),
        "diemEpochAllocation",
    )?;
    // The plugin compares the uppercased currency; an absent one never selects USD.
    let consuming_usd = envelope
        .consumption_currency
        .as_deref()
        .is_some_and(|currency| currency.to_uppercase() == "USD");

    let mut windows = Vec::new();
    let mut rows = Vec::new();

    // The epoch window: DIEM remaining against its allocation, drawn only when that
    // allocation is the limit in force — a USD-consuming account is drawing USD down.
    let epoch = match (diem, allocation) {
        (Some(diem), Some(allocation)) if !consuming_usd && allocation > 0.0 => {
            Some((diem, allocation))
        }
        _ => None,
    };

    let mut diem_is_the_window = false;
    if !envelope.can_consume {
        windows.push(balance_window(
            100.0,
            "Balance unavailable for API calls".to_owned(),
        ));
    } else if let Some((diem, allocation)) = epoch {
        diem_is_the_window = true;
        windows.push(balance_window(
            ((allocation - diem) / allocation * 100.0).clamp(0.0, 100.0),
            format!("DIEM {diem:.2} / {allocation:.2} epoch allocation"),
        ));
    } else if !usd.is_some_and(|usd| usd > 0.0) && !diem.is_some_and(|diem| diem > 0.0) {
        windows.push(balance_window(
            100.0,
            "No Venice API balance available".to_owned(),
        ));
    }

    // Every balance that is not behind the window is a row: DIEM first, then USD, the
    // order the payload itself carries them in.
    if let Some(diem) = diem.filter(|_| !diem_is_the_window) {
        rows.push(DetailRow {
            label: "DIEM balance".to_owned(),
            value: format!("DIEM {diem:.2}"),
        });
    }
    if let Some(usd) = usd {
        rows.push(DetailRow {
            label: "USD balance".to_owned(),
            value: format!("${usd:.2} USD"),
        });
    }

    let details = if rows.is_empty() {
        Vec::new()
    } else {
        vec![DetailSection {
            title: "Balances".to_owned(),
            rows,
        }]
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows,
        details,
    })
}

/// Venice as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Venice",
    endpoint: |_| BALANCE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "Venice dashboard → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;

    /// Recorded by CodexBar, `VeniceUsageFetcherTests.swift` — "DIEM balance fixture
    /// matches the production golden". CodexBar asserts usedPercent 9.5 and the
    /// allocation subtitle for this body.
    const DIEM_EPOCH: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": 90.50, "usd": null },
          "diemEpochAllocation": 100.0
        }
        "#;

    /// Recorded by CodexBar, same file — "string-encoded balances and allocation match
    /// the production golden". Same numbers, quoted.
    const STRING_ENCODED: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": "90.50", "usd": "25.75" },
          "diemEpochAllocation": "100.0"
        }
        "#;

    /// Recorded by CodexBar, same file — "bundled credits prefer DIEM allocation when
    /// both balances are present". CodexBar asserts usedPercent 50.
    const BUNDLED_CREDITS: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "BUNDLED_CREDITS",
          "balances": { "diem": 50.0, "usd": 10.0 },
          "diemEpochAllocation": 100.0
        }
        "#;

    /// Recorded by CodexBar, same file — "USD currency wins when both balances are
    /// present". CodexBar asserts usedPercent 0 and the USD subtitle: consuming in USD
    /// means the DIEM epoch allocation is not the limit being drawn down.
    const USD_WINS: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "USD",
          "balances": { "diem": 50.0, "usd": 12.34 },
          "diemEpochAllocation": 100.0
        }
        "#;

    /// Recorded by CodexBar, same file — "non-consumable fixture is exhausted".
    const NON_CONSUMABLE: &str = r#"
        {
          "canConsume": false,
          "consumptionCurrency": "USD",
          "balances": { "diem": null, "usd": 100.0 },
          "diemEpochAllocation": null
        }
        "#;

    /// Recorded by CodexBar, same file — "DIEM allocation progress matches the
    /// production golden". CodexBar asserts usedPercent 25.
    const DIEM_PROGRESS: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": 75.0, "usd": null },
          "diemEpochAllocation": 100.0
        }
        "#;

    /// Recorded by CodexBar, same file — "DIEM without allocation matches the
    /// production golden". A DIEM balance with no allocation to divide by.
    const DIEM_NO_ALLOCATION: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": 50.0, "usd": null },
          "diemEpochAllocation": null
        }
        "#;

    /// Recorded by CodexBar, same file — "USD-only balance matches the production
    /// golden".
    const USD_ONLY: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "USD",
          "balances": { "diem": null, "usd": 15.50 },
          "diemEpochAllocation": null
        }
        "#;

    /// Recorded by CodexBar, same file — "zero balances match the exhausted golden".
    const ZERO_BALANCES: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "USD",
          "balances": { "diem": 0.0, "usd": 0.0 },
          "diemEpochAllocation": null
        }
        "#;

    /// Recorded by CodexBar, same file — "null balances match the exhausted golden".
    /// The response that carries neither currency.
    const NULL_BALANCES: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": null,
          "balances": { "diem": null, "usd": null },
          "diemEpochAllocation": null
        }
        "#;

    /// Recorded by CodexBar, same file — "used percentage clamps to zero". CodexBar
    /// asserts usedPercent 0 for a DIEM balance above its allocation.
    const OVER_ALLOCATION: &str = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": 150.0, "usd": null },
          "diemEpochAllocation": 100.0
        }
        "#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_diem_epoch_fixture_draws_the_allocation_window() {
        let snapshot = parse(DIEM_EPOCH, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        assert_eq!(snapshot.captured_at, at(1_800_000_000));
        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(window.key.as_str(), "balance");
        assert_eq!(window.used_percent, 9.5);
        assert_eq!(
            window.subtitle.as_deref(),
            Some("DIEM 90.50 / 100.00 epoch allocation")
        );
        assert_eq!(window.length, None);
        assert_eq!(window.resets_at, None);
    }

    #[test]
    fn string_encoded_amounts_read_the_same_way() {
        let snapshot = parse(STRING_ENCODED, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows[0].used_percent, 9.5);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("DIEM 90.50 / 100.00 epoch allocation")
        );
    }

    #[test]
    fn bundled_credits_draw_the_diem_window_and_file_the_usd_row() {
        let snapshot = parse(BUNDLED_CREDITS, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 50.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("DIEM 50.00 / 100.00 epoch allocation")
        );
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].rows.len(), 1);
        assert_eq!(snapshot.details[0].rows[0].label, "USD balance");
        assert_eq!(snapshot.details[0].rows[0].value, "$10.00 USD");
    }

    #[test]
    fn a_usd_account_draws_no_window_and_files_both_balances_as_rows() {
        // Consuming in USD, the plugin shows the USD balance alone at 0%. A balance
        // with no limit is not a window here: both balances become rows.
        let snapshot = parse(USD_WINS, at(1_800_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "the DIEM allocation is dormant while consuming in USD"
        );
        let rows = &snapshot.details[0].rows;
        assert_eq!(rows[0].label, "DIEM balance");
        assert_eq!(rows[0].value, "DIEM 50.00");
        assert_eq!(rows[1].label, "USD balance");
        assert_eq!(rows[1].value, "$12.34 USD");
    }

    #[test]
    fn a_non_consumable_account_is_exhausted_not_unused() {
        let snapshot = parse(NON_CONSUMABLE, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("Balance unavailable for API calls")
        );
        assert_eq!(snapshot.details[0].rows[0].value, "$100.00 USD");
    }

    #[test]
    fn the_progress_fixture_reads_25_percent() {
        let snapshot = parse(DIEM_PROGRESS, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
    }

    #[test]
    fn diem_without_an_allocation_is_a_row_not_a_window() {
        let snapshot = parse(DIEM_NO_ALLOCATION, at(1_800_000_000)).expect("parses");
        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.details[0].rows[0].label, "DIEM balance");
        assert_eq!(snapshot.details[0].rows[0].value, "DIEM 50.00");
    }

    #[test]
    fn a_usd_only_balance_is_a_row() {
        let snapshot = parse(USD_ONLY, at(1_800_000_000)).expect("parses");
        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.details[0].rows[0].value, "$15.50 USD");
    }

    #[test]
    fn zero_balances_are_exhausted() {
        let snapshot = parse(ZERO_BALANCES, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("No Venice API balance available")
        );
    }

    #[test]
    fn the_response_that_carries_neither_balance_is_exhausted() {
        let snapshot = parse(NULL_BALANCES, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("No Venice API balance available")
        );
        assert!(
            snapshot.details.is_empty(),
            "there is no quantity to file under anything"
        );
    }

    #[test]
    fn a_diem_balance_above_its_allocation_clamps_to_zero_used() {
        let snapshot = parse(OVER_ALLOCATION, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows[0].used_percent, 0.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("DIEM 150.00 / 100.00 epoch allocation")
        );
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names, the two malformed bodies CodexBar
        // recorded, an amount a JSON string that is not numeric cannot be, and a body
        // with no canConsume flag at all — the plugin refuses each of these shapes.
        let string_where_number = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": "many", "usd": null },
          "diemEpochAllocation": 100.0
        }
        "#;
        for body in [
            "{\"partial\":",
            "[{ \"canConsume\": true }]",
            "{ invalid json }",
            string_where_number,
            r#"{"consumptionCurrency":"DIEM","balances":{"diem":1}}"#,
        ] {
            let error = parse(body, at(1_800_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_field_of_a_kind_this_parser_does_not_read_is_skipped() {
        // The recorded DIEM body with one field invented after this was written. An
        // object-shaped provider meets the unknown-kind rule here: an unread field is
        // skipped, not refused, and the allocation window still draws.
        let body = r#"
        {
          "canConsume": true,
          "consumptionCurrency": "DIEM",
          "balances": { "diem": 90.50, "usd": null },
          "diemEpochAllocation": 100.0,
          "epochSummary": { "spent": 9.5, "window": "epoch" }
        }
        "#;
        let snapshot = parse(body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 9.5);
    }

    #[test]
    fn the_spec_polls_the_billing_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.venice.ai/api/v1/billing/balance"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "Venice has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
