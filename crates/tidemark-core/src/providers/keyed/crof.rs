//! Crof.
//!
//! Ported from CodexBar's `crof.js` plugin. Never seen answering: every number in the
//! tests is a body CodexBar recorded.
//!
//! # What the payload does not tell you
//!
//! `credits` is a remaining balance in dollars, not a share of anything: the plugin this
//! ports draws it as a binary bar — 0% while funded, 100% once it hits zero — and puts
//! the amount itself under the bar, floored to the cent: `$9.04` for 9.0441, `$9.99`
//! for 9.9999, never the fourth decimal. A negative balance is clamped to zero before
//! either is computed. The bar is drawn rather than filed under a detail row because the
//! depletion it reports is real information; there being no limit to divide by is why
//! the window is keyed by name and carries no length.
//!
//! `usable_requests` is *remaining*, not used: consumption is
//! `100 - floor(remaining / requests_plan * 100)`, floored before the subtraction, so
//! 998 of 1000 reads 1%. Either counter being null — either one, not both — means there
//! is no request window at all and only the balance is drawn. The count under the bar
//! shows `usable_requests` itself, unclamped by the plan, the way the plugin formats it:
//! whole numbers plain, anything else to two decimals.
//!
//! `usage` is the most detailed thing in the payload — per-model token counts — and the
//! plugin never reads it. Neither does this port: an unread field is skipped, not
//! refused.
//!
//! # The reset that is not on the wire
//!
//! The request allowance resets at midnight `America/Chicago`, and that reset is
//! computed by the source, never returned. This port does not compute it: the workspace
//! carries no timezone database, and a hard-coded Chicago offset would put the pace mark
//! in the wrong place for half the year. The window is drawn with its length and no
//! reset — a window with no pace mark is honest, and a wrong one is not.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, length_title};
use serde::Deserialize;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp, Window, WindowKey, WindowLength};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "crof";

/// The public usage endpoint, trailing slash and all: that exact URL is the request
/// CodexBar recorded.
const USAGE_URL: &str = "https://crof.ai/usage_api/";

/// How long the request allowance runs. The plugin's `windowMinutes: 1440`; nothing on
/// the wire says how long the window lasts.
const DAY_SECS: u64 = 86_400;

#[derive(Debug, Deserialize)]
struct Envelope {
    credits: f64,
    #[serde(default)]
    requests_plan: Option<f64>,
    #[serde(default)]
    usable_requests: Option<f64>,
}

/// The balance in dollars, floored to the cent — the plugin's own formatting.
fn dollars(credits: f64) -> String {
    format!("${:.2}", (credits * 100.0).floor() / 100.0)
}

/// A request count in the plugin's own rounding: whole numbers plain, anything else to
/// two decimals.
fn count(requests: f64) -> String {
    if requests.fract() == 0.0 {
        format!("{requests:.0}")
    } else {
        format!("{requests:.2}")
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
    if !envelope.credits.is_finite() {
        return Err(ProviderError::malformed("credits must be a number"));
    }

    let mut windows = Vec::new();
    if let (Some(plan), Some(usable)) = (envelope.requests_plan, envelope.usable_requests) {
        // Remaining is clamped to the plan before dividing and floored after: 998 of
        // 1000 is 99% remaining, 1% used — not 99.8%, and not 2%.
        let clamped = plan.min(usable).max(0.0);
        let remaining_percent = if plan > 0.0 {
            (clamped / plan * 100.0).floor().clamp(0.0, 100.0)
        } else {
            0.0
        };
        let length = WindowLength::from_secs(DAY_SECS).expect("a day is not zero seconds");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: length_title(length),
            subtitle: Some(format!("{} requests left", count(usable.max(0.0)))),
            used_percent: 100.0 - remaining_percent,
            // Midnight America/Chicago, computed by the source and by nobody here: see
            // the module doc. A window with no pace mark is honest, and a wrong one is not.
            resets_at: None,
            length: Some(length),
        });
    }

    // The balance bar is binary — funded reads 0%, depleted reads 100% — because there is
    // no limit to divide by; that is the plugin's own drawing, not a measured share. The
    // key is a name for the same reason: a balance has no length to key on.
    let funded = envelope.credits > 0.0;
    let credits = if funded { envelope.credits } else { 0.0 };
    windows.push(Window {
        key: WindowKey::named("balance"),
        title: "Credits".to_owned(),
        subtitle: Some(dollars(credits)),
        used_percent: if funded { 0.0 } else { 100.0 },
        resets_at: None,
        length: None,
    });

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

/// Crof as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Crof",
    endpoint: |_| USAGE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "Crof dashboard → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar, `CrofUsageFetcherTests.swift` — "credits-only fixture
    /// matches the production golden". A funded balance, no request plan, and a
    /// per-model usage block the plugin never reads.
    const CREDITS_ONLY: &str = r#"{
          "credits":9.0441,
          "requests_plan":null,
          "usable_requests":null,
          "usage":{
            "deepseek-v4-flash":{
              "cached_tokens":0,
              "input_tokens":23,
              "output_tokens":132,
              "total_tokens":155
            }
          }
        }"#;

    /// Recorded by CodexBar, same file — "request-quota fixture matches the production
    /// golden". CodexBar's own test asserts usedPercent 1, windowMinutes 1440 and
    /// "998 requests left" for this body.
    const REQUEST_QUOTA: &str = r#"{"credits":10.0,"requests_plan":1000,"usable_requests":998}"#;

    /// Recorded by CodexBar, same file — "credit balance formatting and depletion match
    /// the production goldens".
    const FUNDED_TO_THE_EDGE: &str =
        r#"{"credits":9.9999,"requests_plan":null,"usable_requests":null}"#;
    const DEPLETED: &str = r#"{"credits":0,"requests_plan":null,"usable_requests":null}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_credits_only_fixture_draws_one_funded_balance() {
        // 1_800_000_000 is the `now` CodexBar's harness passes for this fixture.
        let snapshot = parse(CREDITS_ONLY, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        assert_eq!(snapshot.captured_at, at(1_800_000_000));
        assert_eq!(snapshot.windows.len(), 1);
        let balance = &snapshot.windows[0];
        assert_eq!(balance.key.as_str(), "balance");
        assert_eq!(balance.title, "Credits");
        assert_eq!(balance.used_percent, 0.0);
        assert_eq!(balance.subtitle.as_deref(), Some("$9.04"));
        assert_eq!(balance.length, None);
        assert_eq!(balance.resets_at, None);
    }

    #[test]
    fn a_field_of_a_kind_this_parser_does_not_read_is_skipped() {
        // The recorded body carries `usage` — per-model token counts, the most detailed
        // thing in the payload — and the plugin this ports never reads it. An object-shaped
        // provider meets the unknown-kind rule here: an unread field is skipped, not
        // refused, so the balance still draws.
        let snapshot = parse(CREDITS_ONLY, at(1_800_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1, "only the balance is drawn");
    }

    #[test]
    fn the_quota_fixture_draws_the_daily_window_over_the_balance() {
        // 1_777_800_000 is the `now` CodexBar's own test passes for this body.
        let snapshot = parse(REQUEST_QUOTA, at(1_777_800_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        let daily = &snapshot.windows[0];
        assert_eq!(daily.key.as_str(), "w86400");
        assert_eq!(daily.title, "1 day");
        assert_eq!(daily.used_percent, 1.0);
        assert_eq!(
            daily.length.expect("1440 minutes on the wire").as_secs(),
            86_400
        );
        assert_eq!(
            daily.resets_at, None,
            "the reset is computed, and this port does not compute it"
        );
        assert_eq!(daily.subtitle.as_deref(), Some("998 requests left"));
        let balance = &snapshot.windows[1];
        assert_eq!(balance.key.as_str(), "balance");
        assert_eq!(balance.used_percent, 0.0);
        assert_eq!(balance.subtitle.as_deref(), Some("$10.00"));
        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "w86400",
            "the card leads with the daily quota"
        );
    }

    #[test]
    fn the_balance_floors_to_the_cent_and_depletion_fills_the_bar() {
        let funded = parse(FUNDED_TO_THE_EDGE, at(1_800_000_000)).expect("parses");
        assert_eq!(funded.windows[0].subtitle.as_deref(), Some("$9.99"));
        assert_eq!(funded.windows[0].used_percent, 0.0);
        let depleted = parse(DEPLETED, at(1_800_000_000)).expect("parses");
        assert_eq!(depleted.windows[0].subtitle.as_deref(), Some("$0.00"));
        assert_eq!(depleted.windows[0].used_percent, 100.0);
    }

    #[test]
    fn a_body_we_cannot_read_is_refused_wholesale() {
        // The truncated envelope the procedure names, then a count a JSON string cannot
        // be: the plugin's own refusals (credits not a number, requests_plan not a
        // number) must land as Malformed, never as a window drawn from nothing.
        for body in [
            r#"{"partial":"#,
            r#"{"credits":"many","requests_plan":null,"usable_requests":null}"#,
            r#"{"credits":10.0,"requests_plan":"many","usable_requests":998}"#,
        ] {
            let error = parse(body, at(1_800_000_000))
                .expect_err("a count that is not a number fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn the_spec_polls_the_public_usage_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://crof.ai/usage_api/",
            "trailing slash and all, the URL CodexBar recorded"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert!(SPEC.options.is_empty(), "Crof has nothing to choose");
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
    }
}
