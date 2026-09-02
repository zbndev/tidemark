//! ElevenLabs.
//!
//! Ported from CodexBar's Swift parser and fetcher, `Providers/ElevenLabs/
//! ElevenLabsUsageFetcher.swift`; there is no JS plugin. Never seen answering: every
//! number in the tests is a body CodexBar recorded.
//!
//! # The request
//!
//! `GET https://api.elevenlabs.io/v1/user/subscription`. The key is not a bearer
//! token: it rides the provider's own `xi-api-key` header, with
//! `Accept: application/json` beside it. CodexBar also honours an
//! `ELEVENLABS_API_URL` environment override with `/v1`-aware path joining; Tidemark
//! has no environment overrides, so the fixed host is the port and the joining rule
//! is not.
//!
//! # What the payload says
//!
//! One flat object: `character_count` and `character_limit` — required, a body
//! without either is refused — the quota's reset as `next_character_count_reset_unix`
//! in Unix seconds; two optional voice-slot pairs; `tier` and `status`; and
//! `current_overage`, which the source decodes and then reads by no one; this port
//! keeps it unread the same way.
//!
//! The character quota is a **quota window**: a percentage (count over limit,
//! clamped at both ends; a non-positive limit reads as an empty bar rather than a
//! division by zero) with a monthly reset instant and no span — the payload states
//! no length, so the window carries no pace mark and is keyed by name. The subtitle
//! carries both absolutes grouped the way the source groups them
//! (`25,000 / 100,000 characters`). The source's own summary spells the unit
//! `credits`, as its `Credits` session label does; the fields are character counts,
//! and this port names them what they are.
//!
//! The voice-slot pairs, when both halves are present and the limit is positive,
//! draw as two further windows — Voice slots and Professional voices — with their
//! absolutes plain under the bar (`2 / 10`), no reset and no length. A pair missing
//! either half draws nothing, as in the source.
//!
//! `tier` becomes the plan row: underscores to spaces, title-cased, with the status
//! appended unless it is empty or `active`; a body with no tier shows the raw status
//! instead, and one with neither shows no row.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, title_case};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "elevenlabs";

const SUBSCRIPTION_URL: &str = "https://api.elevenlabs.io/v1/user/subscription";

#[derive(Debug, Deserialize)]
struct Subscription {
    #[serde(default)]
    tier: Option<String>,
    character_count: i64,
    character_limit: i64,
    #[serde(default)]
    voice_slots_used: Option<i64>,
    #[serde(default)]
    professional_voice_slots_used: Option<i64>,
    #[serde(default)]
    voice_limit: Option<i64>,
    #[serde(default)]
    professional_voice_limit: Option<i64>,
    /// Decoded by the source and then read by no one; kept for the same check.
    #[serde(default)]
    #[allow(dead_code)]
    current_overage: Option<Overage>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    next_character_count_reset_unix: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Overage {
    #[serde(default)]
    #[allow(dead_code)]
    amount: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    currency: Option<String>,
}

/// Consumption as the source computes it: count over limit, clamped at both ends for
/// display; a non-positive limit is an empty bar rather than a division by zero.
fn percent(used: i64, limit: i64) -> f64 {
    if limit > 0 {
        (used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// A count grouped the way the source's own formatter groups it (`25,000`).
fn grouped(value: i64) -> String {
    let rendered = value.to_string();
    let bytes = rendered.as_bytes();
    let mut grouped = String::with_capacity(rendered.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    grouped
}

/// One voice-slot pair as a window, or `None` when the pair is absent or its limit is
/// not positive — the source draws nothing in both cases.
fn slot_window(key: &str, title: &str, used: Option<i64>, limit: Option<i64>) -> Option<Window> {
    let used = used?;
    let limit = limit.filter(|limit| *limit > 0)?;
    Some(Window {
        key: WindowKey::named(key),
        title: title.to_owned(),
        subtitle: Some(format!("{used} / {limit}")),
        used_percent: percent(used, limit),
        resets_at: None,
        length: None,
    })
}

/// The plan row's value: the tier title-cased with the status appended unless it is
/// empty or `active`; the raw status when no tier is stated.
fn display_tier(tier: Option<&str>, status: Option<&str>) -> Option<String> {
    match tier.map(str::trim).filter(|tier| !tier.is_empty()) {
        Some(tier) => {
            let mut value = title_case(tier);
            if let Some(status) =
                status.filter(|status| !status.is_empty() && status.to_lowercase() != "active")
            {
                value.push_str(&format!(" · {status}"));
            }
            Some(value)
        }
        None => status.map(str::to_owned),
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
    let subscription: Subscription = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    // The character quota. The payload states no span, so the window is keyed by
    // name: there is no length for WindowKey::for_length to derive one from, and the
    // reset instant still arrives without it.
    let mut windows = vec![Window {
        key: WindowKey::named("characters"),
        title: "Characters".to_owned(),
        subtitle: Some(format!(
            "{} / {} characters",
            grouped(subscription.character_count),
            grouped(subscription.character_limit)
        )),
        used_percent: percent(subscription.character_count, subscription.character_limit),
        resets_at: subscription
            .next_character_count_reset_unix
            .and_then(|seconds| Timestamp::from_unix(seconds).ok()),
        length: None,
    }];
    if let Some(window) = slot_window(
        "voice-slots",
        "Voice slots",
        subscription.voice_slots_used,
        subscription.voice_limit,
    ) {
        windows.push(window);
    }
    if let Some(window) = slot_window(
        "professional-voices",
        "Professional voices",
        subscription.professional_voice_slots_used,
        subscription.professional_voice_limit,
    ) {
        windows.push(window);
    }

    let mut details = Vec::new();
    if let Some(plan) = display_tier(subscription.tier.as_deref(), subscription.status.as_deref()) {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: plan,
            }],
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

/// ElevenLabs as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "ElevenLabs",
    endpoint: |_| SUBSCRIPTION_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Header("xi-api-key"),
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "ElevenLabs profile → API Keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use crate::providers::keyed::{Auth, Method, Options};
    use tidemark_types::{Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `ElevenLabsUsageFetcherTests.swift` — "parses
    /// subscription response into usage snapshot". CodexBar asserts 25% used,
    /// 75,000 characters remaining, the reset instant 1,738,356,858, the grouped
    /// summary of both absolutes, the tier "Creator", and exactly two extra
    /// windows.
    const CREATOR_SUBSCRIPTION: &str = r#"
        {
          "tier": "creator",
          "character_count": 25000,
          "character_limit": 100000,
          "voice_slots_used": 2,
          "voice_limit": 10,
          "professional_voice_slots_used": 1,
          "professional_voice_limit": 2,
          "current_overage": {"amount": "0", "currency": "usd"},
          "status": "active",
          "next_character_count_reset_unix": 1738356858
        }
        "#;

    /// Recorded by CodexBar, same file — "fetch usage sends xi api key header" and
    /// "fetch usage accepts versioned API base with trailing slash", which share one
    /// body. A starter quota with no reset on the wire and no voice slots at all.
    const STARTER_SUBSCRIPTION: &str = r#"
            {
              "tier": "starter",
              "character_count": 1000,
              "character_limit": 10000,
              "status": "active"
            }
            "#;

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
    fn the_creator_fixture_draws_the_character_quota_and_both_voice_windows() {
        let snapshot = parse(CREATOR_SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["characters", "voice-slots", "professional-voices"]);

        let characters = window(&snapshot, "characters");
        assert_eq!(characters.title, "Characters");
        assert_eq!(characters.used_percent, 25.0, "25,000 of 100,000");
        assert_eq!(
            characters.subtitle.as_deref(),
            Some("25,000 / 100,000 characters")
        );
        assert_eq!(
            characters.resets_at,
            Some(at(1_738_356_858)),
            "the reset instant CodexBar's own test reads"
        );
        assert_eq!(
            characters.length, None,
            "the payload states no span; a named key carries the window"
        );

        let voice = window(&snapshot, "voice-slots");
        assert_eq!(voice.title, "Voice slots");
        assert_eq!(voice.used_percent, 20.0, "2 of 10 slots");
        assert_eq!(voice.subtitle.as_deref(), Some("2 / 10"));
        assert_eq!(voice.resets_at, None);
        assert_eq!(voice.length, None);

        let professional = window(&snapshot, "professional-voices");
        assert_eq!(professional.title, "Professional voices");
        assert_eq!(professional.used_percent, 50.0, "1 of 2 voices");
        assert_eq!(professional.subtitle.as_deref(), Some("1 / 2"));
        assert_eq!(professional.resets_at, None);

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "characters",
            "no window states a length, so the card leads with the quota"
        );

        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Creator");
    }

    #[test]
    fn the_starter_fixture_draws_one_window_without_a_reset() {
        let snapshot = parse(STARTER_SUBSCRIPTION, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        let characters = window(&snapshot, "characters");
        assert_eq!(characters.used_percent, 10.0, "1,000 of 10,000");
        assert_eq!(
            characters.subtitle.as_deref(),
            Some("1,000 / 10,000 characters")
        );
        assert_eq!(characters.resets_at, None, "this body states no reset");
        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Starter");
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        // The truncated envelope the procedure names; the consumption field as a
        // string where a number belongs; and a body without the limit the source
        // declares required — none of these may draw a bar.
        let string_where_number = r#"
        { "tier": "creator", "character_count": "many", "character_limit": 100000 }
        "#;
        let no_limit = r#"
        { "tier": "creator", "character_count": 25000 }
        "#;
        for body in ["{\"partial\":", "not-json", string_where_number, no_limit] {
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
        // The recorded creator body carrying one field invented after this was
        // written. An object-shaped provider meets the unknown-kind rule here: an
        // unread field is skipped, not refused, and all three windows still draw.
        let body = CREATOR_SUBSCRIPTION.replacen(
            "\"current_overage\":",
            "\"priority_mode\": { \"kind\": \"unreleased\" }, \"current_overage\":",
            1,
        );
        let snapshot = parse(&body, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(window(&snapshot, "characters").used_percent, 25.0);
    }

    #[test]
    fn the_spec_polls_the_subscription_endpoint_with_the_xi_api_key_header() {
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.elevenlabs.io/v1/user/subscription"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Header("xi-api-key"));
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "ElevenLabs has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
