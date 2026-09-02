//! ClinePass.
//!
//! Ported from CodexBar's `clinepass.js` plugin. Never seen answering: every number in the
//! tests is a body CodexBar recorded.
//!
//! # What the payload does not tell you
//!
//! `type` is an enum with no length on the wire — `five_hour`, `weekly`, `monthly` — and
//! the seconds behind each name are the plugin's table, not the response's. A name outside
//! that table is skipped rather than guessed at, because a window drawn at the wrong length
//! puts the pace mark in the wrong place, which is worse than not drawing it.
//!
//! `percentUsed` is clamped rather than trusted: the plugin clamps, and a bar cannot render
//! 140% of itself.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, parse_rfc3339};
use serde::Deserialize;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp, Window, WindowKey, WindowLength};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "clinepass";

const USAGE_URL: &str = "https://api.cline.bot/api/v1/users/me/plan/usage-limits";

#[derive(Debug, Deserialize)]
struct Envelope {
    success: Option<bool>,
    data: Option<Data>,
}

#[derive(Debug, Deserialize)]
struct Data {
    limits: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Limit {
    #[serde(rename = "percentUsed")]
    percent_used: f64,
    #[serde(rename = "resetsAt")]
    resets_at: Option<String>,
}

/// The window kinds this parser understands, and how long each one lasts.
fn length_of(kind: &str) -> Option<u64> {
    match kind {
        "five_hour" => Some(18_000),
        "weekly" => Some(604_800),
        "monthly" => Some(2_592_000),
        _ => None,
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
    match envelope.success {
        Some(true) => {}
        Some(false) => return Err(ProviderError::malformed("the provider reported failure")),
        None => return Err(ProviderError::malformed("no success flag")),
    }
    let data = envelope
        .data
        .ok_or_else(|| ProviderError::malformed("successful response carried no data"))?;

    let mut windows = Vec::new();
    for entry in data.limits {
        // Recognise the kind before deserializing, so a quota type invented after this was
        // written can carry any shape it likes. Once recognised, a shape we cannot read is
        // an error.
        let Some(seconds) = entry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(length_of)
        else {
            continue;
        };
        let limit: Limit = serde_json::from_value(entry)
            .map_err(|e| ProviderError::malformed(format!("limit entry is not readable: {e}")))?;
        let length = WindowLength::from_secs(seconds).expect("length_of never yields zero seconds");
        let resets_at = match limit.resets_at.as_deref() {
            Some(raw) => parse_rfc3339(raw)
                .map(Some)
                .ok_or_else(|| ProviderError::malformed(format!("unreadable reset time {raw}")))?,
            None => None,
        };
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: crate::providers::length_title(length),
            used_percent: limit.percent_used.clamp(0.0, 100.0),
            subtitle: None,
            resets_at,
            length: Some(length),
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

/// ClinePass as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "ClinePass",
    endpoint: |_| USAGE_URL.to_owned(),
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "Cline dashboard → API keys.",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar. Three windows, the five-hour one part-spent.
    const THREE_WINDOWS: &str = r#"{"success":true,"data":{"limits":[
        {"type":"five_hour","percentUsed":12.5,"resetsAt":"2026-07-16T10:20:30Z"},
        {"type":"weekly","percentUsed":34,"resetsAt":"2026-07-20T00:00:00Z"},
        {"type":"monthly","percentUsed":56.75,"resetsAt":"2026-08-01T00:00:00Z"}]}}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn every_reported_window_is_drawn_with_its_length_and_reset() {
        let snapshot = parse(THREE_WINDOWS, at(1_787_000_000)).expect("parses");
        let lengths: Vec<u64> = snapshot
            .windows
            .iter()
            .map(|w| w.length.expect("clinepass states every length").as_secs())
            .collect();
        assert_eq!(lengths, [18_000, 604_800, 2_592_000]);
        assert_eq!(snapshot.windows[0].used_percent, 12.5);
        assert!(snapshot.windows[0].resets_at.is_some());
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn a_quota_kind_invented_after_this_was_written_is_skipped_not_refused() {
        // Recorded by CodexBar: `experimental_pool` arriving beside the three known kinds.
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"five_hour","percentUsed":12.5,"resetsAt":"2026-07-16T15:00:00Z"},
            {"type":"experimental_pool","percentUsed":77,"resetsAt":"2026-07-16T15:00:00Z"},
            {"type":"weekly","percentUsed":25,"resetsAt":"2026-07-20T00:00:00Z"},
            {"type":"monthly","percentUsed":40,"resetsAt":null}]}}"#;
        let snapshot = parse(body, at(1_787_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 3, "the unknown kind is skipped");
        assert_eq!(snapshot.windows[0].used_percent, 12.5);
    }

    #[test]
    fn a_known_kind_we_cannot_read_fails_the_whole_fetch() {
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"weekly","percentUsed":"forty"}]}}"#;
        assert!(
            matches!(
                parse(body, at(1_787_000_000)),
                Err(ProviderError::Malformed(_))
            ),
            "a dropped window reads as 'you have no such limit'"
        );
    }

    #[test]
    fn a_window_with_no_reset_is_still_drawn() {
        let body = r#"{"success":true,"data":{"limits":[
            {"type":"weekly","percentUsed":40}]}}"#;
        let snapshot = parse(body, at(1_787_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert!(snapshot.windows[0].resets_at.is_none());
        assert_eq!(
            snapshot.windows[0].length.expect("derived").as_secs(),
            604_800
        );
    }

    #[test]
    fn the_spec_polls_the_documented_endpoint_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.cline.bot/api/v1/users/me/plan/usage-limits"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.id, PROVIDER_ID);
    }
}
