//! Kilo.
//!
//! Ported from CodexBar's `Kilo/KiloUsageFetcher.swift`. Never seen answering: every number
//! in the tests is a body CodexBar recorded.
//!
//! # Two requests
//!
//! The reading is one tRPC batch — `user.getCreditBlocks`, `kiloPass.getState` and
//! `user.getAutoTopUpPaymentMethod` in a single GET, their inputs spelled out in a query
//! parameter because that is how a tRPC batch is addressed. Beside it, `GET
//! /api/profile` names the account and its organizations; it is allowed to fail, because a
//! name on a card is not worth losing the quota over.
//!
//! The `~/.local/share/kilo/auth.json` CLI session and the `auto` source mode that falls
//! back to it are out of scope: the key comes from the Secret Service like every other key
//! in this module. Kilo's own organization picker is a list fetched at runtime, which a
//! static [`OptionSchema`] cannot offer, so the organization is entered as an id — blank
//! meaning the personal account, which is what the header's absence means on the wire.
//!
//! # What the payload does not tell you
//!
//! **Three procedures answer in one array, and position is identity.** An entry carries no
//! name: which procedure answered is which slot it came back in. A sparse object keyed
//! `"0"`, `"2"` has been recorded too, and routes the same way — so a `planName` sitting in
//! slot two is the auto-top-up procedure's field and not the plan's, and is not read as one.
//!
//! **One of the three is allowed to fail.** An error in the auto-top-up slot leaves the row
//! off and keeps the reading; an error in either of the other two fails the fetch. A tRPC
//! error saying `UNAUTHORIZED` is a rejected key reported under HTTP 200, so it asks for a
//! new key rather than reporting an unreadable response.
//!
//! **Money arrives in three scales.** `_mUsd` is millionths of a dollar, `Cents` is
//! hundredths, and a bare key is dollars. Reading one as another is off by four orders of
//! magnitude, so the suffix is what decides, never the value.
//!
//! **The credit fields have been seen under several names, within two levels.** The source
//! searches a bounded set of spellings at a bounded depth rather than pinning one shape,
//! because the recorded bodies genuinely differ — `creditBlocks` with `amount_mUsd` in one,
//! `blocks` with `usedCredits` in another, bare `creditsUsed` at the top in a third. That
//! search is ported as written, with its key lists and its depth of two. A key found with a
//! value of the wrong type is not that field and the search goes on; inside a `creditBlocks`
//! block, where the shape *is* known, an unreadable amount fails the fetch.
//!
//! **An empty account is drawn full, not left blank.** A zero total is a real state — the
//! source keeps it visible at a hundred per cent rather than showing an empty card, so that
//! an account with no credit reads as one. Nothing reported at all is still no window.
//!
//! **The Kilo Pass bar is drawn against the base allowance, and the bonus is named
//! separately.** The percentage divides by base plus bonus, because that is what may be
//! spent; the subtitle names them apart, because that is what was bought.

use super::{HandSpec, OptionSchema, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "kilo";

/// Name of the organization setting under `[provider.kilo]`.
pub const ORGANIZATION: &str = "organization";

/// The tRPC endpoint the batch is addressed to.
const TRPC_URL: &str = "https://app.kilo.ai/api/trpc";

/// The REST profile, which names the account and its organizations.
const PROFILE_URL: &str = "https://api.kilo.ai/api/profile";

/// The header that scopes the batch to an organization. Absent means the personal account.
const ORGANIZATION_HEADER: &str = "X-KILOCODE-ORGANIZATIONID";

/// The three procedures, in the order their answers come back in. Position is identity: see
/// the module doc.
const PROCEDURES: [&str; 3] = [
    "user.getCreditBlocks",
    "kiloPass.getState",
    "user.getAutoTopUpPaymentMethod",
];

/// The one procedure whose failure is survivable.
const OPTIONAL: usize = 2;

/// What the batch reports, once the three slots have been read.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Reading {
    pub credits_used: Option<f64>,
    pub credits_total: Option<f64>,
    pub credits_remaining: Option<f64>,
    pub pass_used: Option<f64>,
    pub pass_total: Option<f64>,
    pub pass_bonus: Option<f64>,
    pub pass_resets_at: Option<Timestamp>,
    pub plan: Option<String>,
    pub auto_topup: Option<bool>,
    /// How the top-up is paid, or what it is worth when the method is not named.
    pub auto_topup_method: Option<String>,
}

/// What the profile reports.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Profile {
    pub email: Option<String>,
    pub organizations: Vec<Organization>,
}

/// One organization the key can see.
#[derive(Debug, Clone, PartialEq)]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
}

// -- the bounded search the source performs, ported with its own limits --------------

/// Every object within two levels of `payload`, breadth first and including the objects
/// inside arrays. The source's own `dictionaryContexts`, depth and all.
fn contexts(payload: &Value) -> Vec<&Map<String, Value>> {
    let Some(root) = payload.as_object() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([(root, 0usize)]);
    while let Some((current, depth)) = queue.pop_front() {
        found.push(current);
        if depth >= 2 {
            continue;
        }
        for value in current.values() {
            match value {
                Value::Object(nested) => queue.push_back((nested, depth + 1)),
                Value::Array(items) => {
                    for nested in items.iter().filter_map(Value::as_object) {
                        queue.push_back((nested, depth + 1));
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// A number, whether it arrived as one or as a string holding one. A value of any other
/// type is not this field: see the module doc on why the search goes on rather than failing.
fn as_number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// The first of these keys, in the first of these objects, that carries a number.
fn first_number(where_: &[&Map<String, Value>], keys: &[&str]) -> Option<f64> {
    where_
        .iter()
        .find_map(|map| keys.iter().find_map(|key| as_number(map.get(*key))))
}

/// The first of these keys that carries a non-empty string.
fn first_text(where_: &[&Map<String, Value>], keys: &[&str]) -> Option<String> {
    where_.iter().find_map(|map| {
        keys.iter().find_map(|key| {
            map.get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        })
    })
}

/// The first of these keys that carries something that means true or false.
fn first_flag(where_: &[&Map<String, Value>], keys: &[&str]) -> Option<bool> {
    where_
        .iter()
        .find_map(|map| keys.iter().find_map(|key| as_flag(map.get(*key))))
}

/// A boolean, whether it arrived as one or as a word for one.
fn as_flag(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_f64().map(|value| value != 0.0),
        Value::String(text) => match text.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "enabled" | "on" => Some(true),
            "false" | "0" | "no" | "disabled" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// The first of these keys that carries an array.
fn first_array<'a>(where_: &[&'a Map<String, Value>], keys: &[&str]) -> Option<&'a Vec<Value>> {
    where_.iter().find_map(|map| {
        keys.iter()
            .find_map(|key| map.get(*key).and_then(Value::as_array))
    })
}

/// The first of these keys that carries a readable instant.
fn first_instant(where_: &[&Map<String, Value>], keys: &[&str]) -> Option<Timestamp> {
    where_
        .iter()
        .find_map(|map| keys.iter().find_map(|key| as_instant(map.get(*key))))
}

/// An instant, as RFC-3339 or as an epoch in milliseconds above the year 2286.
fn as_instant(value: Option<&Value>) -> Option<Timestamp> {
    match value? {
        Value::String(raw) => {
            let trimmed = raw.trim();
            parse_rfc3339(trimmed).or_else(|| trimmed.parse::<f64>().ok().and_then(epoch))
        }
        Value::Number(raw) => raw.as_f64().and_then(epoch),
        _ => None,
    }
}

fn epoch(value: f64) -> Option<Timestamp> {
    if !value.is_finite() {
        return None;
    }
    if value.abs() > 10_000_000_000.0 {
        return Timestamp::from_unix_millis(value as i64).ok();
    }
    Timestamp::from_unix(value as i64).ok()
}

/// An amount of money, read at whichever scale its key names. See the module doc: the
/// suffix decides, never the value.
fn money(
    where_: &[&Map<String, Value>],
    cents: &[&str],
    micro_usd: &[&str],
    dollars: &[&str],
) -> Option<f64> {
    if let Some(value) = first_number(where_, cents) {
        return Some(value / 100.0);
    }
    if let Some(value) = first_number(where_, micro_usd) {
        return Some(value / 1_000_000.0);
    }
    first_number(where_, dollars)
}

/// A value at a path of keys, where every step is an object.
fn at_path(where_: &[&Map<String, Value>], path: &[&str]) -> Option<String> {
    for map in where_ {
        let mut cursor = Value::Object((*map).clone());
        let mut ok = true;
        for key in path {
            match cursor.get(*key) {
                Some(next) => cursor = next.clone(),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok
            && let Some(text) = cursor.as_str()
            && !text.trim().is_empty()
        {
            return Some(text.trim().to_owned());
        }
    }
    None
}

// -- reading the batch ----------------------------------------------------------------

/// The three answers, routed by the slot they came back in.
fn slots(root: &Value) -> Result<[Option<&Map<String, Value>>; 3], ProviderError> {
    let mut found: [Option<&Map<String, Value>>; 3] = [None, None, None];
    if let Some(entries) = root.as_array() {
        for (index, entry) in entries.iter().take(PROCEDURES.len()).enumerate() {
            found[index] = entry.as_object();
        }
        return Ok(found);
    }
    if let Some(map) = root.as_object() {
        // A single-procedure answer, unbatched.
        if map.contains_key("result") || map.contains_key("error") {
            found[0] = Some(map);
            return Ok(found);
        }
        // A sparse object keyed by slot number.
        let mut any = false;
        for (key, value) in map {
            let Ok(index) = key.parse::<usize>() else {
                continue;
            };
            if index < PROCEDURES.len()
                && let Some(entry) = value.as_object()
            {
                found[index] = Some(entry);
                any = true;
            }
        }
        if any {
            return Ok(found);
        }
    }
    Err(ProviderError::malformed("unexpected tRPC batch shape"))
}

/// The error a slot reports, if it reports one.
fn slot_error(entry: &Map<String, Value>) -> Option<ProviderError> {
    let error = entry.get("error")?.as_object()?;
    let wrapped: Vec<&Map<String, Value>> = std::iter::once(error)
        .chain(error.get("json").and_then(Value::as_object))
        .collect();
    let code = at_path(&wrapped, &["data", "code"]).or_else(|| first_text(&wrapped, &["code"]));
    let message = first_text(&wrapped, &["message"]);
    let combined = [code, message]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();

    // A rejected key reported under HTTP 200. See the module doc.
    if combined.contains("unauthorized") || combined.contains("forbidden") {
        return Some(ProviderError::Credential { status: 401 });
    }
    if combined.contains("not_found") || combined.contains("not found") {
        return Some(ProviderError::malformed(
            "the tRPC batch path or a procedure name is no longer there",
        ));
    }
    Some(ProviderError::malformed(
        "the provider reported a tRPC error",
    ))
}

/// What a slot actually returned, unwrapped from tRPC's envelope.
fn slot_payload(entry: &Map<String, Value>) -> Option<&Value> {
    let result = entry.get("result")?.as_object()?;
    if let Some(data) = result.get("data").and_then(Value::as_object) {
        return match data.get("json") {
            Some(Value::Null) => None,
            Some(json) => Some(json),
            None => result.get("data"),
        };
    }
    match result.get("json") {
        Some(Value::Null) | None => None,
        json => json,
    }
}

/// The credit figures, as the three separately-optional numbers the payload states them as.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Credits {
    used: Option<f64>,
    total: Option<f64>,
    remaining: Option<f64>,
}

/// The credit figures, in the order of preference the source reads them in.
fn credit_fields(payload: Option<&Value>) -> Result<Credits, ProviderError> {
    let Some(payload) = payload else {
        return Ok(Credits::default());
    };
    let where_ = contexts(payload);

    // The known shape: a list of blocks, each stating what it was worth and what is left.
    // Here the shape *is* known, so an unreadable amount fails the fetch.
    if let Some(blocks) = first_array(&where_, &["creditBlocks"]) {
        let (mut total, mut remaining) = (0.0f64, 0.0f64);
        let (mut saw_total, mut saw_remaining) = (false, false);
        for block in blocks.iter().filter_map(Value::as_object) {
            for (key, sum, saw) in [
                ("amount_mUsd", &mut total, &mut saw_total),
                ("balance_mUsd", &mut remaining, &mut saw_remaining),
            ] {
                match block.get(key) {
                    None | Some(Value::Null) => {}
                    Some(value) => {
                        let micro_usd = as_number(Some(value)).ok_or_else(|| {
                            ProviderError::malformed(format!(
                                "a credit block's {key} is not a number"
                            ))
                        })?;
                        *sum += micro_usd / 1_000_000.0;
                        *saw = true;
                    }
                }
            }
        }
        if saw_total || saw_remaining {
            let total = saw_total.then(|| total.max(0.0));
            let remaining = saw_remaining.then(|| remaining.max(0.0));
            let used = match (total, remaining) {
                (Some(total), Some(remaining)) => Some((total - remaining).max(0.0)),
                _ => None,
            };
            return Ok(Credits {
                used,
                total,
                remaining,
            });
        }
    }

    // The other recorded spellings, searched at the source's own depth.
    let blocks = first_array(&where_, &["blocks"])
        .cloned()
        .unwrap_or_default();
    let inside: Vec<&Map<String, Value>> = blocks.iter().filter_map(Value::as_object).collect();

    let mut used = first_number(
        &inside,
        &["used", "usedCredits", "consumed", "spent", "creditsUsed"],
    );
    let mut total = first_number(&inside, &["total", "totalCredits", "creditsTotal", "limit"]);
    let mut remaining = first_number(
        &inside,
        &["remaining", "remainingCredits", "creditsRemaining"],
    );

    used = used.or_else(|| {
        first_number(
            &where_,
            &["used", "usedCredits", "creditsUsed", "consumed", "spent"],
        )
    });
    total = total
        .or_else(|| first_number(&where_, &["total", "totalCredits", "creditsTotal", "limit"]));
    remaining = remaining.or_else(|| {
        first_number(
            &where_,
            &["remaining", "remainingCredits", "creditsRemaining"],
        )
    });

    if total.is_none()
        && let (Some(used), Some(remaining)) = (used, remaining)
    {
        total = Some(used + remaining);
    }

    if used.is_none() && total.is_none() && remaining.is_none() {
        // An account that has never bought anything answers with no blocks and a balance of
        // nothing. Kept visible as an exhausted state rather than as "no data": see the
        // module doc.
        if let Some(balance) = first_number(&where_, &["totalBalance_mUsd"]) {
            let balance = if balance == 0.0 {
                0.0
            } else {
                (balance / 1_000_000.0).max(0.0)
            };
            return Ok(Credits {
                used: Some(0.0),
                total: Some(balance),
                remaining: Some(balance),
            });
        }
    }

    Ok(Credits {
        used,
        total,
        remaining,
    })
}

/// The subscription object, where the payload carries one under that name or is one.
fn subscription(payload: &Value) -> Option<&Map<String, Value>> {
    let map = payload.as_object()?;
    match map.get("subscription") {
        Some(Value::Object(subscription)) => return Some(subscription),
        Some(Value::Null) => return None,
        _ => {}
    }
    let looks_like = [
        "currentPeriodUsageUsd",
        "currentPeriodBaseCreditsUsd",
        "currentPeriodBonusCreditsUsd",
        "tier",
    ]
    .iter()
    .any(|key| map.contains_key(*key));
    looks_like.then_some(map)
}

/// The Kilo Pass figures: used, total, bonus, reset.
fn pass_fields(
    payload: Option<&Value>,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<Timestamp>) {
    let Some(payload) = payload else {
        return (None, None, None, None);
    };

    if let Some(subscription) = subscription(payload) {
        let one = [subscription];
        let used = as_number(subscription.get("currentPeriodUsageUsd")).map(|value| value.max(0.0));
        let base =
            as_number(subscription.get("currentPeriodBaseCreditsUsd")).map(|value| value.max(0.0));
        let bonus = as_number(subscription.get("currentPeriodBonusCreditsUsd"))
            .unwrap_or(0.0)
            .max(0.0);
        let total = base.map(|base| base + bonus);
        let resets_at = first_instant(
            &one,
            &["nextBillingAt", "nextRenewalAt", "renewsAt", "renewAt"],
        );
        return (used, total, (bonus > 0.0).then_some(bonus), resets_at);
    }

    let where_ = contexts(payload);
    if where_.is_empty() {
        return (None, None, None, None);
    }
    let mut total = money(
        &where_,
        &[
            "amountCents",
            "totalCents",
            "planAmountCents",
            "monthlyAmountCents",
            "limitCents",
            "includedCents",
            "valueCents",
        ],
        &[
            "amount_mUsd",
            "total_mUsd",
            "planAmount_mUsd",
            "limit_mUsd",
            "included_mUsd",
            "value_mUsd",
        ],
        &[
            "amount",
            "total",
            "limit",
            "included",
            "value",
            "creditsTotal",
            "totalCredits",
            "planAmount",
        ],
    );
    let mut used = money(
        &where_,
        &[
            "usedCents",
            "spentCents",
            "consumedCents",
            "usedAmountCents",
            "consumedAmountCents",
        ],
        &[
            "used_mUsd",
            "spent_mUsd",
            "consumed_mUsd",
            "usedAmount_mUsd",
        ],
        &[
            "used",
            "spent",
            "consumed",
            "usage",
            "creditsUsed",
            "usedAmount",
            "consumedAmount",
        ],
    );
    let remaining = money(
        &where_,
        &[
            "remainingCents",
            "remainingAmountCents",
            "availableCents",
            "leftCents",
            "balanceCents",
        ],
        &[
            "remaining_mUsd",
            "available_mUsd",
            "left_mUsd",
            "balance_mUsd",
        ],
        &[
            "remaining",
            "available",
            "left",
            "balance",
            "creditsRemaining",
            "remainingAmount",
            "availableAmount",
        ],
    );
    let bonus = money(
        &where_,
        &[
            "bonusCents",
            "bonusAmountCents",
            "includedBonusCents",
            "bonusRemainingCents",
        ],
        &["bonus_mUsd", "bonusAmount_mUsd"],
        &["bonus", "bonusAmount", "bonusCredits", "includedBonus"],
    );
    let resets_at = first_instant(
        &where_,
        &[
            "resetAt",
            "resetsAt",
            "nextResetAt",
            "renewAt",
            "renewsAt",
            "nextRenewalAt",
            "currentPeriodEnd",
            "periodEndsAt",
            "expiresAt",
            "expiryAt",
        ],
    );

    if total.is_none()
        && let (Some(used), Some(remaining)) = (used, remaining)
    {
        total = Some(used + remaining);
    }
    if used.is_none()
        && let (Some(total), Some(remaining)) = (total, remaining)
    {
        used = Some((total - remaining).max(0.0));
    }
    (used, total, bonus, resets_at)
}

/// What Kilo calls a tier, in the words its own interface uses.
fn plan_for_tier(tier: &str) -> String {
    match tier {
        "tier_19" => "Starter".to_owned(),
        "tier_49" => "Pro".to_owned(),
        "tier_199" => "Expert".to_owned(),
        other => other.to_owned(),
    }
}

/// The plan's name, from the slot that owns it.
fn plan_name(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    if let Some(subscription) = subscription(payload) {
        return Some(match first_text(&[subscription], &["tier"]) {
            Some(tier) => plan_for_tier(&tier),
            // A subscription with no tier is still a subscription.
            None => "Kilo Pass".to_owned(),
        });
    }
    let where_ = contexts(payload);
    first_text(
        &where_,
        &[
            "planName",
            "tier",
            "tierName",
            "passName",
            "subscriptionName",
        ],
    )
    .or_else(|| at_path(&where_, &["plan", "name"]))
    .or_else(|| at_path(&where_, &["subscription", "plan", "name"]))
    .or_else(|| at_path(&where_, &["subscription", "name"]))
    .or_else(|| at_path(&where_, &["pass", "name"]))
    .or_else(|| at_path(&where_, &["state", "name"]))
    .or_else(|| first_text(&where_, &["state"]))
    .or_else(|| first_text(&where_, &["name"]).filter(|name| name.to_lowercase().contains("pass")))
}

/// A dollar amount as the source labels a top-up: whole dollars where it is whole.
fn amount_label(amount: f64) -> String {
    if amount.trunc() == amount {
        format!("${amount:.0}")
    } else {
        format!("${amount:.2}")
    }
}

/// Whether the account tops itself up, and how it pays when it does.
fn auto_topup(credits: Option<&Value>, topup: Option<&Value>) -> (Option<bool>, Option<String>) {
    let empty = Value::Object(Map::new());
    let credit_where = contexts(credits.unwrap_or(&empty));
    let topup_where = contexts(topup.unwrap_or(&empty));

    let enabled = first_flag(&topup_where, &["enabled", "isEnabled", "active"])
        .or_else(|| {
            match first_text(&topup_where, &["status"])?
                .to_lowercase()
                .as_str()
            {
                "enabled" | "active" | "on" => Some(true),
                "disabled" | "inactive" | "off" | "none" => Some(false),
                _ => None,
            }
        })
        .or_else(|| first_flag(&credit_where, &["autoTopUpEnabled"]));

    let method = first_text(
        &topup_where,
        &["paymentMethod", "paymentMethodType", "method", "cardBrand"],
    )
    .or_else(|| {
        // No card named, but an amount is as good an answer to "how much" as a brand is to
        // "how" — the source falls back to it, and so does this.
        money(
            &topup_where,
            &["amountCents"],
            &[],
            &["amount", "topUpAmount", "amountUsd"],
        )
        .filter(|amount| *amount > 0.0)
        .map(amount_label)
    });

    (enabled, method)
}

/// The whole batch as one reading. Pure: every trap above is reachable from a test.
pub fn parse_batch(body: &str) -> Result<Reading, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    let slots = slots(&root)?;

    let mut payloads: [Option<&Value>; 3] = [None, None, None];
    for (index, entry) in slots.iter().enumerate() {
        let Some(entry) = entry else { continue };
        if let Some(error) = slot_error(entry) {
            if index == OPTIONAL {
                // Survivable: the row goes missing, the reading does not.
                continue;
            }
            return Err(error);
        }
        payloads[index] = slot_payload(entry);
    }

    let credits = credit_fields(payloads[0])?;
    let (pass_used, pass_total, pass_bonus, pass_resets_at) = pass_fields(payloads[1]);
    let (auto_topup_enabled, auto_topup_method) = auto_topup(payloads[0], payloads[2]);

    Ok(Reading {
        credits_used: credits.used,
        credits_total: credits.total,
        credits_remaining: credits.remaining,
        pass_used,
        pass_total,
        pass_bonus,
        pass_resets_at,
        plan: plan_name(payloads[1]),
        auto_topup: auto_topup_enabled,
        auto_topup_method,
    })
}

/// The REST profile, and the tRPC-wrapped listing that answers the same question. Pure.
pub fn parse_profile(body: &str) -> Result<Profile, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    // The tRPC batch shape: one entry whose data is the list itself.
    if let Some(listed) = root
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("result"))
        .and_then(|result| result.get("data"))
        .and_then(|data| data.get("json").or(Some(data)))
        .and_then(Value::as_array)
    {
        return Ok(Profile {
            email: None,
            organizations: organizations(listed),
        });
    }

    let map = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("the profile response is not an object"))?;
    let email = map
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            map.get("user")
                .and_then(Value::as_object)
                .and_then(|user| user.get("email"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);
    let listed = map
        .get("organizations")
        .and_then(Value::as_array)
        .map(|listed| organizations(listed))
        .unwrap_or_default();
    Ok(Profile {
        email,
        organizations: listed,
    })
}

/// The organizations in a list, skipping any entry with no id: an organization that cannot
/// be addressed is not one the settings could name.
fn organizations(listed: &[Value]) -> Vec<Organization> {
    listed
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.trim();
            if id.is_empty() {
                return None;
            }
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            let role = entry
                .get("role")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|role| !role.is_empty())
                .map(str::to_owned);
            Some(Organization {
                id: id.to_owned(),
                name: name.to_owned(),
                role,
            })
        })
        .collect()
}

/// A credit count in the source's own rounding: whole where it is whole, two decimals
/// otherwise.
fn count(value: f64) -> String {
    if value.trunc() == value {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// A dollar amount to the cent, never below nothing.
fn dollars(value: f64) -> String {
    format!("{:.2}", value.max(0.0))
}

/// Both bodies as one reading. Pure, so the whole shape is reachable without a server.
pub fn snapshot(
    reading: &Reading,
    profile: Option<&Profile>,
    organization: Option<&str>,
    captured_at: Timestamp,
) -> Snapshot {
    let mut windows = Vec::new();

    let total = reading
        .credits_total
        .map(|total| total.max(0.0))
        .or_else(|| match (reading.credits_used, reading.credits_remaining) {
            (Some(used), Some(remaining)) => Some((used + remaining).max(0.0)),
            _ => None,
        });
    let used = reading.credits_used.map(|used| used.max(0.0)).or_else(|| {
        match (total, reading.credits_remaining) {
            (Some(total), Some(remaining)) => Some((total - remaining).max(0.0)),
            _ => None,
        }
    });

    if let Some(total) = total {
        let used = used.unwrap_or(0.0);
        windows.push(Window {
            // A credit balance has no length to key on: it does not roll over, it drains.
            key: WindowKey::named("credits"),
            title: "Credits".to_owned(),
            subtitle: Some(format!("{}/{} credits", count(used), count(total))),
            // A zero total is a real state, kept visible rather than blank. See the module
            // doc.
            used_percent: if total > 0.0 {
                (used / total * 100.0).clamp(0.0, 100.0)
            } else {
                100.0
            },
            resets_at: None,
            length: None,
        });
    }

    let pass_total = reading.pass_total.map(|total| total.max(0.0));
    if let Some(total) = pass_total {
        let used = reading.pass_used.unwrap_or(0.0).max(0.0);
        let bonus = reading.pass_bonus.unwrap_or(0.0).max(0.0);
        let base = (total - bonus).max(0.0);
        let mut subtitle = format!("${} / ${}", dollars(used), dollars(base));
        if bonus > 0.0 {
            subtitle.push_str(&format!(" (+ ${} bonus)", dollars(bonus)));
        }
        windows.push(Window {
            // A pass period has a reset but no stated length, so it is keyed by name.
            key: WindowKey::named("kilo-pass"),
            title: "Kilo Pass".to_owned(),
            subtitle: Some(subtitle),
            used_percent: if total > 0.0 {
                (used / total * 100.0).clamp(0.0, 100.0)
            } else {
                100.0
            },
            resets_at: reading.pass_resets_at,
            length: None,
        });
    }

    let mut rows = Vec::new();
    if let Some(plan) = &reading.plan {
        rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: plan.clone(),
        });
    }
    // The organization the reading is scoped to, named where the profile could name it.
    if let Some(id) = organization.map(str::trim).filter(|id| !id.is_empty()) {
        let named = profile
            .and_then(|profile| profile.organizations.iter().find(|org| org.id == id))
            .map(|org| org.name.clone())
            .unwrap_or_else(|| id.to_owned());
        rows.push(DetailRow {
            label: "Organization".to_owned(),
            value: named,
        });
    }
    if let Some(email) = profile.and_then(|profile| profile.email.clone()) {
        rows.push(DetailRow {
            label: "Account".to_owned(),
            value: email,
        });
    }
    match (reading.auto_topup, &reading.auto_topup_method) {
        (Some(true), Some(method)) => rows.push(DetailRow {
            label: "Auto top-up".to_owned(),
            value: crate::providers::title_case(method),
        }),
        (Some(true), None) => rows.push(DetailRow {
            label: "Auto top-up".to_owned(),
            value: "Enabled".to_owned(),
        }),
        (Some(false), _) => rows.push(DetailRow {
            label: "Auto top-up".to_owned(),
            value: "Off".to_owned(),
        }),
        (None, _) => {}
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

/// The batch URL: the three procedure names joined into the path, and an input per slot in
/// the query, which is how a tRPC batch is addressed.
pub fn batch_url() -> String {
    let procedures = PROCEDURES.join(",");
    let input = (0..PROCEDURES.len())
        .map(|index| format!("\"{index}\":{{\"json\":null}}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{TRPC_URL}/{procedures}?batch=1&input={}",
        encode_query(&format!("{{{input}}}"))
    )
}

/// Percent-encodes one query value: everything outside the unreserved set is escaped, so
/// the braces and quotes of the input object survive into the query string.
fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Kilo as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Kilo",
    credential: CredentialKind::Key,
    credential_hint: "Kilo dashboard → Settings → API keys.",
    options: &[OptionSchema {
        name: ORGANIZATION,
        title: "Organization ID",
        description: Some("Leave blank for the personal account."),
        default: "",
        choices: &[],
        required: false,
    }],
    build,
};

fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Kilo::new(credential, options)?))
}

/// One Kilo account: the key, and the organization it is scoped to if any.
pub struct Kilo {
    client: reqwest::Client,
    credential: Credential,
    organization: Option<String>,
}

impl Kilo {
    /// Builds a client.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
            organization: options
                .get(ORGANIZATION)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
        })
    }

    /// The organization this instance reads, or `None` for the personal account.
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    /// The batch request, built but not sent, so that the placement of the key and the
    /// scope header are testable without a server.
    pub fn batch_request(&self) -> Result<reqwest::Request, ProviderError> {
        let mut builder = self
            .client
            .get(batch_url())
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(organization) = &self.organization {
            builder = builder.header(ORGANIZATION_HEADER, organization);
        }
        builder
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The profile request, built but not sent.
    pub fn profile_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(PROFILE_URL)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The optional second request. A failure of any kind leaves the account's name off the
    /// card: the quota is the point of the poll.
    async fn profile(&self) -> Option<Profile> {
        let request = self.profile_request().ok()?;
        let body = super::request(&self.client, request).await.ok()?;
        parse_profile(&body).ok()
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let reading = parse_batch(&super::request(&self.client, self.batch_request()?).await?)?;
        let profile = self.profile().await;
        Ok(snapshot(
            &reading,
            profile.as_ref(),
            self.organization.as_deref(),
            Timestamp::now(),
        ))
    }
}

impl fmt::Debug for Kilo {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kilo")
            .field("id", &PROVIDER_ID)
            .field("organization", &self.organization)
            .finish_non_exhaustive()
    }
}

impl Provider for Kilo {
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

    /// Recorded by CodexBar, `KiloUsageFetcherTests.swift` — "maps business fields and
    /// identity". Its own test asserts 25% and a plan of "Kilo Pass Pro".
    const BLOCKS: &str = r#"
    [
      { "result": { "data": { "json": { "blocks": [
        { "usedCredits": 25, "totalCredits": 100, "remainingCredits": 75 } ] } } } },
      { "result": { "data": { "json": { "plan": { "name": "Kilo Pass Pro" } } } } },
      { "result": { "data": { "json": { "enabled": true, "paymentMethod": "visa" } } } }
    ]"#;

    /// Recorded by CodexBar, same file — "maps kilo pass window from subscription state".
    /// Its own test asserts 0% on both bars, `$0.00 / $19.00 (+ $9.50 bonus)`, and
    /// "Starter · Auto top-up: off".
    const SUBSCRIPTION: &str = r#"
    [
      { "result": { "data": {
        "creditBlocks": [ { "id": "cb-1", "effective_date": "2026-02-01T00:00:00Z",
          "expiry_date": null, "balance_mUsd": 19000000, "amount_mUsd": 19000000,
          "is_free": false } ],
        "totalBalance_mUsd": 19000000, "autoTopUpEnabled": false } } },
      { "result": { "data": { "subscription": {
        "tier": "tier_19", "currentPeriodUsageUsd": 0,
        "currentPeriodBaseCreditsUsd": 19.0, "currentPeriodBonusCreditsUsd": 9.5,
        "nextBillingAt": "2026-03-28T04:00:00.000Z" } } } },
      { "result": { "data": { "enabled": false, "amountCents": 5000,
        "paymentMethod": null } } }
    ]"#;

    /// Recorded by CodexBar, same file — "fallback pass fields use micro dollar scale".
    /// Its own test asserts `$3.50 / $19.00 (+ $9.50 bonus)` and "Starter".
    const MICRO_DOLLARS: &str = r#"
    [
      { "result": { "data": { "json": { "blocks": [
        { "usedCredits": 0, "totalCredits": 19, "remainingCredits": 19 } ] } } } },
      { "result": { "data": { "json": { "planName": "Starter", "amount_mUsd": 28500000,
        "used_mUsd": 3500000, "bonus_mUsd": 9500000,
        "nextRenewalAt": "2026-03-28T04:00:00.000Z" } } } },
      { "result": { "data": { "json": { "enabled": false, "paymentMethod": null } } } }
    ]"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn row<'a>(snapshot: &'a Snapshot, label: &str) -> Option<&'a str> {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .map(|row| row.value.as_str())
    }

    fn read(body: &str) -> Snapshot {
        let reading = parse_batch(body).expect("parses");
        snapshot(&reading, None, None, at(1_800_000_000))
    }

    #[test]
    fn the_blocks_fixture_draws_the_credit_bar_and_names_the_plan() {
        let snapshot = read(BLOCKS);
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].title, "Credits");
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("25/100 credits")
        );
        assert_eq!(row(&snapshot, "Plan"), Some("Kilo Pass Pro"));
        assert_eq!(row(&snapshot, "Auto top-up"), Some("Visa"));
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn the_subscription_fixture_draws_both_bars_and_names_the_tier() {
        let snapshot = read(SUBSCRIPTION);
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].used_percent, 0.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("0/19 credits"),
            "the blocks were worth $19 and none of it is spent"
        );
        assert_eq!(snapshot.windows[1].title, "Kilo Pass");
        assert_eq!(snapshot.windows[1].used_percent, 0.0);
        assert_eq!(
            snapshot.windows[1].subtitle.as_deref(),
            Some("$0.00 / $19.00 (+ $9.50 bonus)"),
            "the bar divides by base plus bonus; the subtitle names them apart"
        );
        assert!(
            snapshot.windows[1].resets_at.is_some(),
            "nextBillingAt is when the pass period turns over"
        );
        assert_eq!(row(&snapshot, "Plan"), Some("Starter"));
        assert_eq!(row(&snapshot, "Auto top-up"), Some("Off"));
    }

    #[test]
    fn money_is_read_at_the_scale_its_key_names() {
        let snapshot = read(MICRO_DOLLARS);
        assert_eq!(
            snapshot.windows[1].subtitle.as_deref(),
            Some("$3.50 / $19.00 (+ $9.50 bonus)"),
            "28_500_000 mUsd is $28.50, not twenty-eight million"
        );
        assert_eq!(row(&snapshot, "Plan"), Some("Starter"));
    }

    #[test]
    fn a_tier_is_named_in_kilos_own_words_and_an_untiered_pass_is_still_a_pass() {
        // CodexBar's "maps known tier names and defaults to kilo pass".
        let pro = r#"[
          { "result": { "data": { "creditBlocks": [], "totalBalance_mUsd": 0,
            "autoTopUpEnabled": false } } },
          { "result": { "data": { "subscription": { "tier": "tier_49" } } } },
          { "result": { "data": { "enabled": false, "paymentMethod": null } } } ]"#;
        assert_eq!(row(&read(pro), "Plan"), Some("Pro"));

        let untiered = r#"[
          { "result": { "data": { "creditBlocks": [], "totalBalance_mUsd": 0,
            "autoTopUpEnabled": false } } },
          { "result": { "data": { "subscription": { "currentPeriodUsageUsd": 1.0,
            "currentPeriodBaseCreditsUsd": 19.0 } } } },
          { "result": { "data": { "enabled": false, "paymentMethod": null } } } ]"#;
        assert_eq!(row(&read(untiered), "Plan"), Some("Kilo Pass"));
    }

    #[test]
    fn an_amount_stands_in_for_a_card_nobody_named() {
        // CodexBar's "uses auto top up amount when enabled without payment method".
        let body = r#"[
          { "result": { "data": { "creditBlocks": [], "totalBalance_mUsd": 0,
            "autoTopUpEnabled": true } } },
          { "result": { "data": { "subscription": null } } },
          { "result": { "data": { "enabled": true, "amountCents": 5000,
            "paymentMethod": null } } } ]"#;
        assert_eq!(row(&read(body), "Auto top-up"), Some("$50"));
    }

    #[test]
    fn nothing_reported_anywhere_draws_nothing_and_says_nothing() {
        // CodexBar's "treats empty and null business fields as no data success".
        let body = r#"[
          { "result": { "data": { "json": { "blocks": [] } } } },
          { "result": { "data": { "json": { "plan": { "name": null } } } } },
          { "result": { "data": { "json": { "enabled": null, "paymentMethod": null } } } } ]"#;
        let snapshot = read(body);
        assert!(snapshot.windows.is_empty());
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn a_sparse_answer_routes_by_slot_and_not_by_the_names_inside_it() {
        // CodexBar's "keeps sparse indexed object routing by procedure index": the
        // `planName` in slot two belongs to the auto-top-up procedure, not to the plan.
        let body = r#"{
          "0": { "result": { "data": { "json": { "creditsUsed": 10,
            "creditsRemaining": 90 } } } },
          "2": { "result": { "data": { "json": { "planName": "wrong-route",
            "enabled": true, "method": "visa" } } } } }"#;
        let snapshot = read(body);
        assert_eq!(snapshot.windows[0].used_percent, 10.0);
        assert_eq!(row(&snapshot, "Auto top-up"), Some("Visa"));
        assert_eq!(
            row(&snapshot, "Plan"),
            None,
            "slot two is not the plan's slot"
        );
    }

    #[test]
    fn a_total_nobody_stated_is_arrived_at_from_the_other_end() {
        // CodexBar's "uses top level credits used fallback": 40 spent and 60 left is 40%.
        let body = r#"[ { "result": { "data": { "json": { "creditsUsed": 40,
          "creditsRemaining": 60 } } } } ]"#;
        let snapshot = read(body);
        assert_eq!(snapshot.windows[0].used_percent, 40.0);
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("40/100 credits")
        );
    }

    #[test]
    fn an_empty_account_is_drawn_full_rather_than_left_blank() {
        // CodexBar's "keeps zero total visible when activity exists".
        let counted = r#"[
          { "result": { "data": { "json": { "creditsUsed": 0, "creditsRemaining": 0 } } } },
          { "result": { "data": { "json": { "planName": "Kilo Pass Pro" } } } },
          { "result": { "data": { "json": { "enabled": true,
            "paymentMethod": "visa" } } } } ]"#;
        let snapshot = read(counted);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(snapshot.windows[0].subtitle.as_deref(), Some("0/0 credits"));

        // "treats zero balance without credit blocks as visible zero total".
        let balance = r#"[
          { "result": { "data": { "creditBlocks": [], "totalBalance_mUsd": 0,
            "isFirstPurchase": true, "autoTopUpEnabled": false } } },
          { "result": { "data": { "subscription": null } } },
          { "result": { "data": { "enabled": false, "amountCents": 5000,
            "paymentMethod": null } } } ]"#;
        let snapshot = read(balance);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        assert_eq!(snapshot.windows[0].subtitle.as_deref(), Some("0/0 credits"));
        assert_eq!(row(&snapshot, "Auto top-up"), Some("Off"));
    }

    #[test]
    fn the_optional_slot_may_fail_and_the_others_may_not() {
        // CodexBar's "degrades optional auto top up TRPC error".
        let optional = r#"[
          { "result": { "data": { "json": { "creditsUsed": 10,
            "creditsRemaining": 90 } } } },
          { "result": { "data": { "json": { "planName": "Starter" } } } },
          { "error": { "json": { "message": "Internal server error",
            "data": { "code": "INTERNAL_SERVER_ERROR" } } } } ]"#;
        let snapshot = read(optional);
        assert_eq!(snapshot.windows[0].used_percent, 10.0);
        assert_eq!(row(&snapshot, "Plan"), Some("Starter"));
        assert_eq!(row(&snapshot, "Auto top-up"), None);

        // "keeps required procedure TRPC error fatal", and it is a rejected key.
        let required = r#"[
          { "result": { "data": { "json": { "creditsUsed": 10,
            "creditsRemaining": 90 } } } },
          { "error": { "json": { "message": "Unauthorized",
            "data": { "code": "UNAUTHORIZED" } } } } ]"#;
        assert!(matches!(
            parse_batch(required),
            Err(ProviderError::Credential { status: 401 })
        ));

        // "maps unauthorized TRPC error" on its own.
        let alone = r#"[ { "error": { "json": { "message": "Unauthorized",
          "data": { "code": "UNAUTHORIZED" } } } } ]"#;
        assert!(matches!(
            parse_batch(alone),
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn a_body_we_cannot_read_is_malformed() {
        for body in [
            // CodexBar's "maps invalid JSON to parse error".
            "not-json",
            r#""a string""#,
            r#"{"unexpected":true}"#,
        ] {
            assert!(
                matches!(parse_batch(body), Err(ProviderError::Malformed(_))),
                "{body}"
            );
        }
        // Inside a credit block the shape is known, so an unreadable amount is fatal.
        let block = r#"[ { "result": { "data": { "creditBlocks": [
          { "amount_mUsd": "nineteen", "balance_mUsd": 19000000 } ] } } } ]"#;
        assert!(matches!(
            parse_batch(block),
            Err(ProviderError::Malformed(_))
        ));
        // An error that is neither a rejected key nor a missing endpoint.
        let other = r#"[ { "error": { "json": { "message": "Internal server error",
          "data": { "code": "INTERNAL_SERVER_ERROR" } } } } ]"#;
        assert!(matches!(
            parse_batch(other),
            Err(ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn the_profile_names_the_account_and_its_organizations() {
        // CodexBar's "parseOrganizations decodes profile REST shape".
        let rest = r#"{ "user": { "email": "test@example.com" },
          "organizations": [ { "id": "org_42", "name": "Gamma" } ] }"#;
        let profile = parse_profile(rest).expect("parses");
        assert_eq!(profile.email.as_deref(), Some("test@example.com"));
        assert_eq!(profile.organizations.len(), 1);
        assert_eq!(profile.organizations[0].id, "org_42");
        assert_eq!(profile.organizations[0].role, None);

        // "decodes tRPC array shape".
        let trpc = r#"[ { "result": { "data": { "json": [
          { "id": "org_1", "name": "Alpha", "role": "owner" },
          { "id": "org_2", "name": "Beta", "role": "member" } ] } } } ]"#;
        let profile = parse_profile(trpc).expect("parses");
        assert_eq!(profile.organizations.len(), 2);
        assert_eq!(profile.organizations[0].name, "Alpha");
        assert_eq!(profile.organizations[0].role.as_deref(), Some("owner"));

        // "returns empty for no orgs".
        let none = r#"{ "user": { "email": "x@y" }, "organizations": [] }"#;
        assert!(
            parse_profile(none)
                .expect("parses")
                .organizations
                .is_empty()
        );
        assert!(parse_profile("not-json").is_err());
    }

    #[test]
    fn a_named_organization_is_shown_by_its_name_and_an_unknown_one_by_its_id() {
        let reading = parse_batch(BLOCKS).expect("parses");
        let profile = parse_profile(
            r#"{ "user": { "email": "test@example.com" },
              "organizations": [ { "id": "org_42", "name": "Gamma" } ] }"#,
        )
        .expect("parses");
        let named = snapshot(&reading, Some(&profile), Some("org_42"), at(1_800_000_000));
        assert_eq!(row(&named, "Organization"), Some("Gamma"));
        assert_eq!(row(&named, "Account"), Some("test@example.com"));

        let unknown = snapshot(&reading, Some(&profile), Some("org_99"), at(1_800_000_000));
        assert_eq!(row(&unknown, "Organization"), Some("org_99"));

        let personal = snapshot(&reading, Some(&profile), Some("  "), at(1_800_000_000));
        assert_eq!(row(&personal, "Organization"), None);
    }

    #[test]
    fn the_batch_is_addressed_the_way_a_trpc_batch_is() {
        let url = batch_url();
        assert!(
            url.starts_with(
                "https://app.kilo.ai/api/trpc/user.getCreditBlocks,kiloPass.getState,\
                 user.getAutoTopUpPaymentMethod?batch=1&input="
            ),
            "{url}"
        );
        let input = url.split("input=").nth(1).expect("an input");
        let decoded = percent_decode(input);
        let parsed: Value = serde_json::from_str(&decoded).expect("the input is JSON");
        for slot in ["0", "1", "2"] {
            assert!(
                parsed[slot]["json"].is_null(),
                "every procedure takes a null input"
            );
        }
    }

    /// Only for the test above: the inverse of [`encode_query`].
    fn percent_decode(raw: &str) -> String {
        let bytes = raw.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' && index + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).expect("ascii");
                out.push(u8::from_str_radix(hex, 16).expect("hex"));
                index += 3;
            } else {
                out.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn the_scope_header_is_sent_only_for_an_organization() {
        let personal = Kilo::new(Credential::new("kilo-test"), &options(&[])).expect("builds");
        let request = personal.batch_request().expect("builds");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer kilo-test"
        );
        assert!(request.headers().get(ORGANIZATION_HEADER).is_none());
        assert_eq!(personal.organization(), None);

        let scoped = Kilo::new(
            Credential::new("kilo-test"),
            &options(&[(ORGANIZATION, " org_42 ")]),
        )
        .expect("builds");
        assert_eq!(scoped.organization(), Some("org_42"));
        let request = scoped.batch_request().expect("builds");
        assert_eq!(
            request.headers().get(ORGANIZATION_HEADER).expect("present"),
            "org_42"
        );
        assert_eq!(
            scoped.profile_request().expect("builds").url().as_str(),
            "https://api.kilo.ai/api/profile"
        );
        assert!(!format!("{scoped:?}").contains("kilo-test"));
    }

    #[tokio::test]
    async fn a_blank_credential_is_refused_before_a_request_is_spent() {
        let client = Kilo::new(Credential::new("   "), &options(&[])).expect("builds");
        assert!(matches!(
            client.fetch().await,
            Err(ProviderError::Credential { status: 401 })
        ));
        assert_eq!(client.id().as_str(), PROVIDER_ID);
        assert_eq!(client.account(), AccountId::default());
        assert_eq!(SPEC.id, PROVIDER_ID);
    }
}
