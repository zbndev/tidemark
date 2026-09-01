//! Neuralwatt.
//!
//! Ported from CodexBar's Swift fetcher, `Providers/NeuralWatt/NeuralWattUsageFetcher.
//! swift`; there is no JS plugin. Never seen answering: every number in the tests is a
//! body CodexBar recorded.
//!
//! # Three readings of one body
//!
//! `GET /v1/quota` answers with a prepaid **credit balance** in USD, an energy
//! **subscription** in kWh, and a per-**key allowance** in USD, all at once.
//!
//! The balance is a fixed quantity against a stated limit, drawn as one balance window
//! (`$19.66 / $52.34`): `credits_used_usd` over `total_credits_usd`, with the source's
//! cross-fill (a missing total is remaining + used; a missing used is total −
//! remaining) and its validity rules — a negative or zero total reads as absent, a
//! negative remaining likewise. A remaining of zero with no usable total is a row, not
//! a window: there is nothing to divide by. CodexBar carries the remaining as a cost
//! line and computes the same percentage its own test asserts; this port draws that
//! percentage as the balance window the window model prescribes.
//!
//! The subscription is kWh used of kWh included, resetting at the period end. The
//! billing period is one continuing quota whose calendar length wobbles (28–31 days),
//! so its length keys the pace mark but never its identity — the window is keyed by
//! name, the lesson [`WindowKey`]'s own docs teach, and a body without a period start
//! simply carries no length. A non-renewing subscription keeps the window's reset and
//! draws no `Renews` row, exactly as the source keeps the reset but drops the renewal.
//!
//! The key allowance (`Key Monthly`, from its `period` word) states no length and no
//! reset on the wire — the source passes `windowMinutes: nil` deliberately — so it is
//! keyed by name too, and a `blocked: true` allowance reads as a full bar with no
//! absolutes under it.
//!
//! # What refuses the fetch
//!
//! A body with no balance object, a balance none of whose three fields reads, and a
//! period date that is not ISO-8601 all fail the whole fetch, as the source's decoder
//! refuses them. Everything else — `snapshot_at`, the lifetime usage, the limits, the
//! request and token counts — is deserialized for the shape check and read by no one,
//! exactly as in the source. A rejected key arrives as HTTP 401/403, which the shared
//! transport maps; the body carries no rejection of its own.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, parse_rfc3339, title_case};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "neuralwatt";

const QUOTA_URL: &str = "https://api.neuralwatt.com/v1/quota";

#[derive(Debug, Deserialize)]
struct Envelope {
    /// Deserialized for the shape check, then read by no one, as in the source.
    #[serde(default, rename = "snapshot_at")]
    #[allow(dead_code)]
    snapshot_at: Option<String>,
    balance: Balance,
    #[serde(default)]
    usage: Option<Usage>,
    /// Deserialized for the shape check, then read by no one, as in the source.
    #[serde(default)]
    #[allow(dead_code)]
    limits: Option<Limits>,
    /// Documented as always-present: an object when active, `null` otherwise.
    #[serde(default)]
    subscription: Option<Subscription>,
    #[serde(default)]
    key: Option<Key>,
}

#[derive(Debug, Deserialize)]
struct Balance {
    #[serde(default, rename = "credits_remaining_usd")]
    credits_remaining_usd: Option<f64>,
    #[serde(default, rename = "total_credits_usd")]
    total_credits_usd: Option<f64>,
    #[serde(default, rename = "credits_used_usd")]
    credits_used_usd: Option<f64>,
    #[serde(default, rename = "accounting_method")]
    accounting_method: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    /// Deserialized for the shape check, then read by no one, as in the source.
    #[serde(default)]
    #[allow(dead_code)]
    lifetime: Option<Period>,
    #[serde(default, rename = "current_month")]
    current_month: Option<Period>,
}

#[derive(Debug, Deserialize)]
struct Period {
    #[serde(default, rename = "cost_usd")]
    cost_usd: Option<f64>,
    /// Deserialized for the shape check, then read by no one, as in the source.
    #[serde(default)]
    #[allow(dead_code)]
    requests: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    tokens: Option<i64>,
    #[serde(default, rename = "energy_kwh")]
    energy_kwh: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Limits {
    /// Deserialized for the shape check, then read by no one, as in the source.
    #[serde(default, rename = "overage_limit_usd")]
    #[allow(dead_code)]
    overage_limit_usd: Option<f64>,
    #[serde(default, rename = "rate_limit_tier")]
    #[allow(dead_code)]
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Subscription {
    #[serde(default)]
    plan: Option<String>,
    #[serde(default, rename = "current_period_start")]
    current_period_start: Option<String>,
    #[serde(default, rename = "current_period_end")]
    current_period_end: Option<String>,
    #[serde(default, rename = "auto_renew")]
    auto_renew: Option<bool>,
    #[serde(default, rename = "kwh_included")]
    kwh_included: Option<f64>,
    #[serde(default, rename = "kwh_used")]
    kwh_used: Option<f64>,
    #[serde(default, rename = "kwh_remaining")]
    kwh_remaining: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Key {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    allowance: Option<Allowance>,
}

#[derive(Debug, Deserialize)]
struct Allowance {
    #[serde(default, rename = "limit_usd")]
    limit_usd: Option<f64>,
    #[serde(default)]
    period: Option<String>,
    #[serde(default, rename = "spent_usd")]
    spent_usd: Option<f64>,
    #[serde(default)]
    blocked: Option<bool>,
}

/// A value the source would accept: finite and not negative.
fn valid_non_negative(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value >= 0.0)
}

/// A value the source would accept: finite and positive.
fn valid_positive(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value > 0.0)
}

/// The total behind the balance: stated, or the remaining plus the used.
fn effective_total(balance: &Balance) -> Option<f64> {
    if let Some(total) = valid_positive(balance.total_credits_usd) {
        return Some(total);
    }
    let total = valid_non_negative(balance.credits_remaining_usd)?
        + valid_non_negative(balance.credits_used_usd)?;
    (total > 0.0).then_some(total)
}

/// The used behind the balance: stated, or the total minus the remaining.
fn effective_used(balance: &Balance) -> Option<f64> {
    if let Some(used) = valid_non_negative(balance.credits_used_usd) {
        return Some(used);
    }
    let total = valid_positive(balance.total_credits_usd)?;
    let remaining = valid_non_negative(balance.credits_remaining_usd)?;
    Some((total - remaining).max(0.0))
}

/// The remaining behind the balance: stated, or the total minus the used.
fn effective_remaining(balance: &Balance) -> Option<f64> {
    valid_non_negative(balance.credits_remaining_usd)
        .or_else(|| Some((effective_total(balance)? - effective_used(balance)?).max(0.0)))
}

/// The subscription's kWh ceiling: included, or the used plus the remaining.
fn subscription_total(subscription: &Subscription) -> Option<f64> {
    if let Some(included) = valid_positive(subscription.kwh_included) {
        return Some(included);
    }
    let total = valid_non_negative(subscription.kwh_used)?
        + valid_non_negative(subscription.kwh_remaining)?;
    (total > 0.0).then_some(total)
}

/// The subscription's kWh used: stated, or the ceiling minus the remaining.
fn subscription_used(subscription: &Subscription) -> Option<f64> {
    if let Some(used) = valid_non_negative(subscription.kwh_used) {
        return Some(used);
    }
    let total = subscription_total(subscription)?;
    let remaining = valid_non_negative(subscription.kwh_remaining)?;
    Some((total - remaining).max(0.0))
}

/// An amount of energy as the source formats it: whole with no decimals, fractional
/// with two (`13.90 / 20 kWh`).
fn kwh(value: f64) -> String {
    if value.round() == value {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// The `YYYY-MM-DD` a whole-second timestamp falls on, for the renewal row.
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

    // The balance gate the source keeps: at least one of the three credit fields must
    // read, or the fetch refuses rather than draw a balance it never saw.
    if valid_non_negative(envelope.balance.credits_remaining_usd).is_none()
        && valid_non_negative(envelope.balance.credits_used_usd).is_none()
        && valid_positive(envelope.balance.total_credits_usd).is_none()
    {
        return Err(ProviderError::malformed(
            "the balance object carries no readable credit fields",
        ));
    }

    let mut windows = Vec::new();
    let mut balance_rows = Vec::new();
    let mut plan_rows = Vec::new();

    // The plan section first, because the plan names the account whether or not any
    // window drew: the plan when the subscription states one, else the accounting
    // method, as the source's identity reads.
    if let Some(plan) = envelope
        .subscription
        .as_ref()
        .and_then(|subscription| subscription.plan.as_deref())
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
    {
        plan_rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: format!("{} plan", title_case(plan)),
        });
    } else if let Some(method) = envelope
        .balance
        .accounting_method
        .as_deref()
        .map(str::trim)
        .filter(|method| !method.is_empty())
    {
        plan_rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: title_case(method),
        });
    }

    if let Some(subscription) = &envelope.subscription {
        // A period date that is present but unreadable fails the fetch, as the source's
        // decoder refuses it.
        let start = dated(subscription.current_period_start.as_deref())?;
        let end = dated(subscription.current_period_end.as_deref())?;
        if let (Some(total), Some(used)) = (
            subscription_total(subscription),
            subscription_used(subscription),
        ) {
            // The billing period is one continuing quota whose calendar length wobbles
            // (28-31 days), so the length keys the pace mark but never the identity.
            let length = match (start, end) {
                (Some(start), Some(end)) if end > start => {
                    WindowLength::from_secs((end.as_unix() - start.as_unix()) as u64)
                }
                _ => None,
            };
            windows.push(Window {
                key: WindowKey::named("subscription"),
                title: "Subscription".to_owned(),
                subtitle: Some(format!("{} / {} kWh", kwh(used), kwh(total))),
                used_percent: (used / total * 100.0).clamp(0.0, 100.0),
                resets_at: end,
                length,
            });
        }
        // The renewal row: a non-renewing subscription keeps the window's reset above
        // but draws no renewal, as the source keeps the one and drops the other.
        if subscription.auto_renew != Some(false)
            && let Some(end) = end
        {
            plan_rows.push(DetailRow {
                label: "Renews".to_owned(),
                value: day_of(end),
            });
        }
    }

    // The prepaid balance: a window while a total exists to divide by, a row otherwise.
    // A balance has no length to key on: it does not roll over, it drains.
    match (
        effective_total(&envelope.balance),
        effective_used(&envelope.balance),
    ) {
        (Some(total), Some(used)) => windows.push(Window {
            key: WindowKey::named("balance"),
            title: "Prepaid balance".to_owned(),
            subtitle: Some(format!("${used:.2} / ${total:.2}")),
            used_percent: (used / total * 100.0).clamp(0.0, 100.0),
            resets_at: None,
            length: None,
        }),
        _ => {
            if let Some(remaining) = effective_remaining(&envelope.balance) {
                balance_rows.push(DetailRow {
                    label: "Balance".to_owned(),
                    value: format!("${remaining:.2}"),
                });
            }
        }
    }

    // The key allowance: a full bar when blocked, spent over limit otherwise, and no
    // length or reset anywhere on the wire.
    if let Some(allowance) = envelope.key.as_ref().and_then(|key| key.allowance.as_ref()) {
        let percent = if allowance.blocked == Some(true) {
            Some(100.0)
        } else {
            match (allowance.spent_usd, allowance.limit_usd) {
                (Some(spent), Some(limit)) if limit > 0.0 => {
                    Some((spent / limit * 100.0).clamp(0.0, 100.0))
                }
                _ => None,
            }
        };
        if let Some(percent) = percent {
            let period = allowance.period.as_deref().unwrap_or("allowance");
            windows.push(Window {
                key: WindowKey::named("key-allowance"),
                title: format!("Key {}", title_case(period)),
                subtitle: None,
                used_percent: percent,
                resets_at: None,
                length: None,
            });
        }
    }

    let mut details = Vec::new();
    if !plan_rows.is_empty() {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: plan_rows,
        });
    }
    if let Some(month) = envelope
        .usage
        .as_ref()
        .and_then(|usage| usage.current_month.as_ref())
    {
        let mut rows = Vec::new();
        if let Some(cost) = month.cost_usd {
            rows.push(DetailRow {
                label: "This month cost".to_owned(),
                value: format!("${cost:.2}"),
            });
        }
        if let Some(energy) = month.energy_kwh {
            rows.push(DetailRow {
                label: "This month energy".to_owned(),
                value: format!("{} kWh", kwh(energy)),
            });
        }
        if !rows.is_empty() {
            details.push(DetailSection {
                title: "Usage summary".to_owned(),
                rows,
            });
        }
    }
    if !balance_rows.is_empty() {
        details.push(DetailSection {
            title: "Prepaid balance".to_owned(),
            rows: balance_rows,
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows,
        details,
    })
}

/// A period date: present-but-unreadable fails the fetch, as the source's decoder does.
fn dated(raw: Option<&str>) -> Result<Option<Timestamp>, ProviderError> {
    match raw {
        None => Ok(None),
        Some(raw) => parse_rfc3339(raw).map(Some).ok_or_else(|| {
            ProviderError::malformed(format!("a period date is not ISO-8601: {raw}"))
        }),
    }
}

/// Neuralwatt as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Neuralwatt",
    endpoint: |_| QUOTA_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "Neuralwatt portal → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use tidemark_types::{Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `NeuralWattUsageFetcherTests.swift` — "parses quota
    /// response into usage snapshot". CodexBar asserts the credit percent 19.6626/52.34,
    /// the key allowance at 25%, the subscription percent 13.9023/20 with
    /// "13.90 / 20 kWh", the period end as the reset and the renewal, the prepaid
    /// balance 32.6774, the "Standard plan" identity, and the "Key Monthly" allowance.
    const FULL_QUOTA: &str = r#"
        {
          "snapshot_at": "2026-04-16T18:30:00Z",
          "balance": {
            "credits_remaining_usd": 32.6774,
            "total_credits_usd": 52.34,
            "credits_used_usd": 19.6626,
            "accounting_method": "energy"
          },
          "usage": {
            "lifetime": {
              "cost_usd": 243.9145,
              "requests": 37801,
              "tokens": 1235477176,
              "energy_kwh": 15.6009
            },
            "current_month": {
              "cost_usd": 160.1463,
              "requests": 23902,
              "tokens": 1116658995,
              "energy_kwh": 9.7278
            }
          },
          "limits": {
            "overage_limit_usd": null,
            "rate_limit_tier": "standard"
          },
          "subscription": {
            "plan": "standard",
            "status": "active",
            "billing_interval": "month",
            "current_period_start": "2026-04-11T05:05:25Z",
            "current_period_end": "2026-05-11T05:05:25Z",
            "auto_renew": true,
            "kwh_included": 20.0,
            "kwh_used": 13.9023,
            "kwh_remaining": 6.0977,
            "in_overage": false
          },
          "key": {
            "name": "my-production-key",
            "allowance": {
              "limit_usd": 50.0,
              "period": "monthly",
              "spent_usd": 12.5,
              "remaining_usd": 37.5,
              "blocked": false
            }
          }
        }
        "#;

    /// Recorded by CodexBar, same file — "parses response with null subscription using
    /// accounting method". CodexBar asserts creditUsedPercent 10, no primary window, and
    /// the "Energy" identity.
    const NULL_SUBSCRIPTION: &str = r#"
        {
          "snapshot_at": "2026-04-16T18:30:00Z",
          "balance": {
            "credits_remaining_usd": 4.5,
            "total_credits_usd": 5.0,
            "credits_used_usd": 0.5,
            "accounting_method": "energy"
          },
          "usage": {
            "lifetime": {"cost_usd": 0.5, "requests": 10, "tokens": 1000, "energy_kwh": 0.01},
            "current_month": {"cost_usd": 0.5, "requests": 10, "tokens": 1000, "energy_kwh": 0.01}
          },
          "limits": {"overage_limit_usd": null, "rate_limit_tier": "free"},
          "subscription": null,
          "key": {"name": "trial", "allowance": null}
        }
        "#;

    /// Recorded by CodexBar, same file — "parses response with missing credits used
    /// derived from remaining". CodexBar asserts the derived used credits of 70.
    const DERIVED_USED: &str = r#"
        {
          "balance": {
            "credits_remaining_usd": 30.0,
            "total_credits_usd": 100.0,
            "accounting_method": "energy"
          },
          "usage": {"lifetime": {}, "current_month": {}},
          "limits": {},
          "subscription": null,
          "key": {"name": "x", "allowance": null}
        }
        "#;

    /// Recorded by CodexBar, same file — "keeps known zero prepaid balance separate
    /// from subscription quota". A remaining of zero with no usable total: no window,
    /// only the balance row.
    const ZERO_BALANCE: &str = r#"
        {
          "balance": {
            "credits_remaining_usd": 0.0,
            "total_credits_usd": 0.0,
            "accounting_method": "energy"
          },
          "usage": {"lifetime": {}, "current_month": {}},
          "limits": {},
          "subscription": null,
          "key": {"name": "x", "allowance": null}
        }
        "#;

    /// Recorded by CodexBar, same file — "zero prepaid balance does not exhaust active
    /// subscription". CodexBar asserts the subscription at 25% with "2.50 / 10 kWh" and
    /// the "Pro Energy plan" identity.
    const ZERO_BALANCE_WITH_SUBSCRIPTION: &str = r#"
        {
          "balance": {
            "credits_remaining_usd": 0.0,
            "total_credits_usd": 0.0,
            "accounting_method": "energy"
          },
          "usage": {"lifetime": {}, "current_month": {}},
          "limits": {},
          "subscription": {
            "plan": "pro_energy",
            "status": "active",
            "current_period_start": "2026-04-01T00:00:00Z",
            "current_period_end": "2026-05-01T00:00:00Z",
            "kwh_included": 10.0,
            "kwh_used": 2.5,
            "kwh_remaining": 7.5
          },
          "key": {"name": "subscriber", "allowance": null}
        }
        "#;

    /// Recorded by CodexBar, same file — "non renewing subscription keeps period end
    /// without renewal date". The window keeps its reset; the renewal row does not draw.
    const NON_RENEWING: &str = r#"
        {
          "balance": {"credits_remaining_usd": 1.0},
          "subscription": {
            "plan": "standard",
            "status": "active",
            "current_period_end": "2026-05-01T00:00:00Z",
            "auto_renew": false,
            "kwh_included": 10.0,
            "kwh_used": 4.0,
            "kwh_remaining": 6.0
          },
          "key": {"name": "subscriber", "allowance": null}
        }
        "#;

    /// Recorded by CodexBar, same file — "blocked key allowance is exhausted without
    /// numeric limit". CodexBar asserts the allowance window at 100%.
    const BLOCKED_ALLOWANCE: &str = r#"
        {
          "balance": {"credits_remaining_usd": 3.0},
          "subscription": null,
          "key": {"name": "blocked", "allowance": {"blocked": true, "period": "monthly"}}
        }
        "#;

    /// Recorded by CodexBar, same file — "parses fractional subscription dates".
    /// CodexBar asserts the period end parses and the credits read 20%.
    const FRACTIONAL_DATES: &str = r#"
        {
          "balance": {
            "credits_remaining_usd": 8.0,
            "total_credits_usd": 10.0,
            "credits_used_usd": 2.0,
            "accounting_method": "energy"
          },
          "usage": {"lifetime": {}, "current_month": {}},
          "limits": {},
          "subscription": {
            "plan": "standard",
            "status": "active",
            "current_period_start": "2026-04-11T05:05:25.123Z",
            "current_period_end": "2026-05-11T05:05:25.456Z"
          },
          "key": {"name": "x", "allowance": null}
        }
        "#;

    /// Recorded by CodexBar, same file — "rejects malformed successful response without
    /// balance".
    const NO_BALANCE: &str = r#"{"error":"temporarily unavailable"}"#;

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
    fn the_quota_fixture_draws_the_subscription_balance_and_allowance() {
        let snapshot = parse(FULL_QUOTA, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["subscription", "balance", "key-allowance"]);

        let subscription = window(&snapshot, "subscription");
        assert_eq!(subscription.title, "Subscription");
        assert!(
            (subscription.used_percent - 13.9023 / 20.0 * 100.0).abs() < 1e-9,
            "13.9023 kWh of the 20 included"
        );
        assert_eq!(subscription.subtitle.as_deref(), Some("13.90 / 20 kWh"));
        assert_eq!(
            subscription.resets_at,
            Some(at(1_778_475_925)),
            "2026-05-11T05:05:25Z, the reset CodexBar's own test reads"
        );
        assert_eq!(
            subscription.length.expect("the stated period").as_secs(),
            2_592_000,
            "2026-04-11 to 2026-05-11 is thirty days"
        );

        let balance = window(&snapshot, "balance");
        assert_eq!(balance.title, "Prepaid balance");
        assert!((balance.used_percent - 19.6626 / 52.34 * 100.0).abs() < 1e-9);
        assert_eq!(balance.subtitle.as_deref(), Some("$19.66 / $52.34"));
        assert_eq!(balance.length, None, "a balance has no length to key on");
        assert_eq!(balance.resets_at, None);

        let allowance = window(&snapshot, "key-allowance");
        assert_eq!(allowance.title, "Key Monthly");
        assert_eq!(allowance.used_percent, 25.0, "12.50 spent of a 50 limit");
        assert_eq!(
            allowance.subtitle, None,
            "the allowance states no absolutes CodexBar would show"
        );
        assert_eq!(allowance.length, None, "the wire states no length");

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "subscription",
            "the card leads with the subscription, as CodexBar's primary does"
        );

        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Standard plan");
        assert_eq!(row(&snapshot, "Plan", "Renews").value, "2026-05-11");
        assert_eq!(
            row(&snapshot, "Usage summary", "This month cost").value,
            "$160.15"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "This month energy").value,
            "9.73 kWh"
        );
    }

    #[test]
    fn a_null_subscription_reads_the_accounting_method() {
        let snapshot = parse(NULL_SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["balance"]);
        assert!((window(&snapshot, "balance").used_percent - 10.0).abs() < 1e-9);
        assert_eq!(
            row(&snapshot, "Plan", "Plan").value,
            "Energy",
            "no plan on the wire, so the accounting method names the account"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "This month cost").value,
            "$0.50"
        );
        assert_eq!(
            row(&snapshot, "Usage summary", "This month energy").value,
            "0.01 kWh"
        );
    }

    #[test]
    fn a_missing_used_credit_derives_from_total_and_remaining() {
        let snapshot = parse(DERIVED_USED, at(1_800_000_000)).expect("parses");
        let balance = window(&snapshot, "balance");
        assert_eq!(balance.used_percent, 70.0, "100 total minus 30 remaining");
        assert_eq!(balance.subtitle.as_deref(), Some("$70.00 / $100.00"));
    }

    #[test]
    fn a_zero_balance_with_no_total_is_a_row_not_a_window() {
        let snapshot = parse(ZERO_BALANCE, at(1_800_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "a remaining of zero with no total: nothing to divide by"
        );
        assert_eq!(row(&snapshot, "Prepaid balance", "Balance").value, "$0.00");
    }

    #[test]
    fn a_zero_balance_does_not_exhaust_the_subscription() {
        let snapshot = parse(ZERO_BALANCE_WITH_SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        let subscription = window(&snapshot, "subscription");
        assert_eq!(subscription.used_percent, 25.0);
        assert_eq!(subscription.subtitle.as_deref(), Some("2.50 / 10 kWh"));
        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Pro Energy plan");
        assert_eq!(row(&snapshot, "Prepaid balance", "Balance").value, "$0.00");
    }

    #[test]
    fn a_non_renewing_subscription_keeps_its_reset_but_not_a_renews_row() {
        let snapshot = parse(NON_RENEWING, at(1_800_000_000)).expect("parses");
        let subscription = window(&snapshot, "subscription");
        assert_eq!(subscription.resets_at, Some(at(1_777_593_600)));
        assert_eq!(
            subscription.length, None,
            "no period start on the wire, so no length derives"
        );
        assert!(
            snapshot
                .details
                .iter()
                .flat_map(|section| &section.rows)
                .all(|row| row.label != "Renews"),
            "auto_renew false: the window resets but nothing renews"
        );
    }

    #[test]
    fn a_blocked_allowance_is_exhausted_without_a_limit() {
        let snapshot = parse(BLOCKED_ALLOWANCE, at(1_800_000_000)).expect("parses");
        let allowance = window(&snapshot, "key-allowance");
        assert_eq!(allowance.used_percent, 100.0);
        assert_eq!(allowance.subtitle, None);
        assert_eq!(allowance.title, "Key Monthly");
    }

    #[test]
    fn fractional_dates_parse_and_a_derived_balance_reads_twenty_percent() {
        let snapshot = parse(FRACTIONAL_DATES, at(1_800_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            ["balance"],
            "the subscription states no kWh fields, so no window derives for it"
        );
        assert_eq!(window(&snapshot, "balance").used_percent, 20.0);
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names; the recorded balance-less body; a
        // balance none of whose fields reads; the recorded shape with a credit a string
        // where a number belongs; and a period end that is not a date — the source's
        // decoder refuses each of these shapes.
        let string_where_number = r#"
        { "balance": { "credits_remaining_usd": 30.0, "total_credits_usd": 100.0,
                      "credits_used_usd": "many" } }
        "#;
        let fields_without_numbers = r#"
        { "balance": { "accounting_method": "energy" } }
        "#;
        let bad_date = r#"
        { "balance": { "credits_remaining_usd": 1.0 },
          "subscription": { "current_period_end": "next Tuesday", "kwh_included": 10.0 } }
        "#;
        for body in [
            "{\"partial\":",
            NO_BALANCE,
            fields_without_numbers,
            string_where_number,
            bad_date,
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
        // The recorded null-subscription body with one field invented after this was
        // written: an unread field is skipped, not refused, and the balance still draws.
        let body = NULL_SUBSCRIPTION.replacen(
            "\"limits\":",
            "\"alerts\": {\"kind\": \"quota\"},\n          \"limits\":",
            1,
        );
        let snapshot = parse(&body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert!((window(&snapshot, "balance").used_percent - 10.0).abs() < 1e-9);
    }

    #[test]
    fn the_spec_polls_the_quota_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.neuralwatt.com/v1/quota"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "Neuralwatt has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
