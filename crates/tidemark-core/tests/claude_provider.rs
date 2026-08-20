use tidemark_core::providers::{ProviderError, claude};
use tidemark_types::{Timestamp, WindowLength};

const LIVE_SHAPE: &str = r#"{
  "five_hour": {"utilization": 99, "resets_at": "2026-08-20T21:50:00Z"},
  "seven_day": {"utilization": 98, "resets_at": "2026-08-21T09:00:00Z"},
  "limits": [
    {
      "kind": "session", "group": "session", "percent": 26,
      "severity": "normal", "resets_at": "2026-08-20T21:50:00.414253+00:00",
      "scope": null, "is_active": false
    },
    {
      "kind": "weekly_all", "group": "weekly", "percent": 70,
      "severity": "warning", "resets_at": "2026-08-21T09:00:00.414272+00:00",
      "scope": null, "is_active": true
    }
  ],
  "spend": {
    "used": {"amount_minor": 1250, "currency": "USD", "exponent": 2},
    "limit": {"amount_minor": 5000, "currency": "USD", "exponent": 2},
    "percent": 25, "severity": "normal", "enabled": true,
    "disabled_reason": null, "cap": null, "balance": null, "auto_reload": null,
    "disclaimer": "invented fixture", "can_purchase_credits": true, "can_toggle": true
  },
  "extra_usage": {
    "is_enabled": true, "monthly_limit": 5000, "used_credits": 1250,
    "utilization": 25, "currency": "USD", "decimal_places": 2,
    "disabled_reason": null, "user_disabled": false, "spend_limit_reached": false,
    "credits_ever_enabled": true, "daily": null, "weekly": null
  }
}"#;

fn captured_at() -> Timestamp {
    Timestamp::from_unix(1_787_250_000).expect("plausible")
}

#[test]
fn limits_array_is_authoritative_over_legacy_top_level_windows() {
    let snapshot = claude::parse(LIVE_SHAPE, captured_at()).expect("live shape parses");

    assert_eq!(snapshot.provider.as_str(), "claude");
    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.windows[0].used_percent, 26.0);
    assert_eq!(snapshot.windows[0].length, WindowLength::from_secs(18_000));
    assert_eq!(snapshot.windows[0].key.as_str(), "w18000");
    assert_eq!(snapshot.windows[1].used_percent, 70.0);
    assert_eq!(snapshot.windows[1].length, WindowLength::from_secs(604_800));
    assert_eq!(snapshot.windows[1].key.as_str(), "w604800");
}

#[test]
fn an_understood_limit_with_a_broken_percent_fails_the_whole_snapshot() {
    let body = r#"{
      "limits": [
        {"kind":"session","group":"session","percent":"nearly full",
         "severity":"normal","resets_at":"2026-08-20T21:50:00Z",
         "scope":null,"is_active":true}
      ]
    }"#;

    assert!(matches!(
        claude::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn an_unknown_limit_kind_does_not_hide_the_known_windows() {
    let body = r#"{
      "limits": [
        {"kind":"future_daily_pool","group":"daily","percent":"new shape"},
        {"kind":"session","group":"session","percent":17,
         "severity":"normal","resets_at":"2026-08-20T21:50:00Z",
         "scope":null,"is_active":true}
      ]
    }"#;

    let snapshot = claude::parse(body, captured_at()).expect("known limit survives");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].used_percent, 17.0);
}

#[test]
fn an_unknown_kind_is_skipped_even_when_it_reuses_a_known_group() {
    let body = r#"{
      "limits": [
        {"kind":"future_weekly_pool","group":"weekly","percent":88,
         "severity":"normal","resets_at":"2026-08-21T09:00:00Z",
         "scope":null,"is_active":true},
        {"kind":"session","group":"session","percent":17,
         "severity":"normal","resets_at":"2026-08-20T21:50:00Z",
         "scope":null,"is_active":true}
      ]
    }"#;

    let snapshot = claude::parse(body, captured_at()).expect("known limit survives");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].key.as_str(), "w18000");
}

#[test]
fn a_present_but_malformed_reset_fails_the_understood_limit() {
    let body = r#"{
      "limits": [
        {"kind":"session","group":"session","percent":17,
         "severity":"normal","resets_at":"some time tomorrow",
         "scope":null,"is_active":true}
      ]
    }"#;

    assert!(matches!(
        claude::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn spend_and_extra_usage_are_kept_as_details_not_fake_windows() {
    let snapshot = claude::parse(LIVE_SHAPE, captured_at()).expect("live shape parses");
    assert_eq!(snapshot.windows.len(), 2);
    let usage = snapshot
        .details
        .iter()
        .find(|section| section.title == "Extra usage")
        .expect("extra usage details");
    assert_eq!(usage.rows[0].label, "Used");
    assert_eq!(usage.rows[0].value, "$12.50 of $50.00");
}

#[test]
fn scoped_weekly_limits_get_distinct_stable_keys() {
    let body = r#"{
      "limits": [
        {"kind":"weekly_all","group":"weekly","percent":20,
         "severity":"normal","resets_at":"2026-08-21T09:00:00Z",
         "scope":null,"is_active":true},
        {"kind":"weekly_scoped","group":"weekly","percent":40,
         "severity":"normal","resets_at":"2026-08-21T09:00:00Z",
         "scope":{"model":{"id":"model-fable","display_name":"Fable"}},"is_active":false}
      ]
    }"#;

    let snapshot = claude::parse(body, captured_at()).expect("scoped limits parse");
    assert_eq!(snapshot.windows[0].key.as_str(), "w604800");
    assert_eq!(snapshot.windows[1].key.as_str(), "model-fable/w604800");
    assert_eq!(snapshot.windows[1].title, "Fable · 7 days");
}

#[test]
fn a_known_kind_remains_known_when_group_is_omitted() {
    let body = r#"{
      "limits": [
        {"kind":"session","percent":11,"severity":"normal",
         "resets_at":"2026-08-20T21:50:00Z","scope":null,"is_active":true}
      ]
    }"#;

    let snapshot = claude::parse(body, captured_at()).expect("kind is self-describing");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].key.as_str(), "w18000");
}

#[test]
fn a_known_kind_with_a_conflicting_group_is_malformed() {
    let body = r#"{
      "limits": [
        {"kind":"session","group":"weekly","percent":11,"severity":"normal",
         "resets_at":"2026-08-20T21:50:00Z","scope":null,"is_active":true}
      ]
    }"#;

    assert!(matches!(
        claude::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn a_known_kind_with_a_non_string_group_is_malformed() {
    let body = r#"{
      "limits": [
        {"kind":"session","group":42,"percent":11,"severity":"normal",
         "resets_at":"2026-08-20T21:50:00Z","scope":null,"is_active":true}
      ]
    }"#;

    assert!(matches!(
        claude::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn spend_is_kept_when_the_legacy_extra_usage_object_is_absent() {
    let body = r#"{
      "limits": [],
      "spend": {
        "used":{"amount_minor":1250,"currency":"USD","exponent":2},
        "limit":{"amount_minor":5000,"currency":"USD","exponent":2},
        "percent":25,"severity":"normal","enabled":true,"disabled_reason":null,
        "cap":null,"balance":null,"auto_reload":null,"disclaimer":"fixture",
        "can_purchase_credits":true,"can_toggle":true
      }
    }"#;

    let snapshot = claude::parse(body, captured_at()).expect("spend parses");
    assert_eq!(snapshot.details[0].title, "Extra usage");
    assert_eq!(snapshot.details[0].rows[0].value, "$12.50 of $50.00");
}
