//! Codex `wham/usage`, parsed.
//!
//! `LIVE_SHAPE` is the body the real endpoint returned on 2026-08-20, with the identifiers
//! replaced: one window, in the slot called *primary*, describing itself as seven days.
//! That single fact is what most of these tests are about.

use tidemark_core::providers::{ProviderError, codex};
use tidemark_types::{DetailSection, Timestamp, WindowLength};

const LIVE_SHAPE: &str = r#"{
  "user_id": "user-fixture",
  "account_id": "00000000-0000-4000-8000-000000000000",
  "email": "fixture@example.invalid",
  "plan_type": "plus",
  "rate_limit": {
    "allowed": true,
    "limit_reached": false,
    "primary_window": {
      "used_percent": 19,
      "limit_window_seconds": 604800,
      "reset_after_seconds": 599960,
      "reset_at": 1787855484
    },
    "secondary_window": null
  },
  "code_review_rate_limit": null,
  "additional_rate_limits": null,
  "credits": {
    "has_credits": false,
    "unlimited": false,
    "overage_limit_reached": false,
    "balance": "0",
    "approx_local_messages": [0, 0],
    "approx_cloud_messages": [0, 0]
  },
  "spend_control": {"reached": false, "individual_limit": null},
  "rate_limit_reached_type": null,
  "promo": null,
  "rate_limit_reset_credits": {"available_count": 0, "applicable_available_count": 0}
}"#;

fn captured_at() -> Timestamp {
    Timestamp::from_unix(1_787_255_524).expect("plausible")
}

#[test]
fn the_only_window_is_keyed_by_its_length_and_not_by_the_slot_it_arrived_in() {
    let snapshot = codex::parse(LIVE_SHAPE, captured_at()).expect("live shape parses");

    assert_eq!(snapshot.provider.as_str(), "codex");
    assert_eq!(snapshot.windows.len(), 1);
    let weekly = &snapshot.windows[0];
    assert_eq!(weekly.key.as_str(), "w604800");
    assert_eq!(weekly.title, "7 days");
    assert_eq!(weekly.used_percent, 19.0);
    assert_eq!(weekly.length, WindowLength::from_secs(604_800));
    assert_eq!(
        weekly.resets_at.map(Timestamp::as_unix),
        Some(1_787_855_484)
    );
}

#[test]
fn the_same_weekly_window_keeps_its_key_when_it_moves_to_the_secondary_slot() {
    // Measured: this account reported its weekly figures under `secondary` before
    // 2026-08-19 and under `primary` after it. One continuous window, one key.
    let moved = r#"{
      "plan_type": "plus",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": null,
        "secondary_window": {
          "used_percent": 19,
          "limit_window_seconds": 604800,
          "reset_after_seconds": 599960,
          "reset_at": 1787855484
        }
      }
    }"#;

    let snapshot = codex::parse(moved, captured_at()).expect("the moved window parses");

    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].key.as_str(), "w604800");
    assert_eq!(snapshot.windows[0].used_percent, 19.0);
}

#[test]
fn a_five_hour_window_beside_the_weekly_one_is_the_one_the_card_leads_with() {
    let body = r#"{
      "plan_type": "pro",
      "rate_limit": {
        "primary_window": {"used_percent": 4, "limit_window_seconds": 18000,
                           "reset_after_seconds": 1200, "reset_at": 1787256724},
        "secondary_window": {"used_percent": 61, "limit_window_seconds": 604800,
                             "reset_after_seconds": 599960, "reset_at": 1787855484}
      }
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("both windows parse");

    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.windows[0].title, "5 hours");
    assert_eq!(snapshot.windows[1].title, "7 days");
    let dominant = snapshot.dominant_window().expect("a window is present");
    assert_eq!(dominant.key.as_str(), "w18000");
}

#[test]
fn a_window_that_does_not_say_how_long_it_is_fails_rather_than_being_keyed_by_its_slot() {
    // There is nothing else to key it on, and keying on "primary" is the trap this
    // provider exists to demonstrate. Refusing the response is the honest answer.
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": 19, "reset_at": 1787855484}}
    }"#;

    assert!(matches!(
        codex::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn an_unreadable_window_fails_the_whole_snapshot() {
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": "most of it",
                                        "limit_window_seconds": 604800}}
    }"#;

    assert!(matches!(
        codex::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn a_response_carrying_no_windows_at_all_is_a_snapshot_with_no_windows() {
    let body = r#"{"plan_type": "free", "rate_limit": null}"#;

    let snapshot = codex::parse(body, captured_at()).expect("an empty rate limit is not a failure");

    assert!(snapshot.windows.is_empty());
    assert_eq!(snapshot.details[0].title, DetailSection::PLAN);
}

#[test]
fn a_reset_time_is_derived_from_the_countdown_when_the_absolute_one_is_absurd() {
    // A zero `reset_at` has been seen in this family of payloads. The countdown beside it
    // still says when the window rolls over, and a window with a pace mark beats one
    // without.
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": 19, "limit_window_seconds": 604800,
                                        "reset_after_seconds": 600, "reset_at": 0}}
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("the countdown carries it");

    assert_eq!(
        snapshot.windows[0].resets_at.map(Timestamp::as_unix),
        Some(captured_at().as_unix() + 600)
    );
}

#[test]
fn a_window_with_neither_reset_field_is_still_drawn() {
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": 19, "limit_window_seconds": 604800}}
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("a window without a reset is a window");

    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].resets_at, None);
}

#[test]
fn additional_rate_limits_become_windows_named_after_their_own_pool() {
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": 19, "limit_window_seconds": 604800,
                                        "reset_at": 1787855484}},
      "additional_rate_limits": [
        {"limit_name": "GPT-5.3-Codex-Spark", "metered_feature": "codex_spark",
         "rate_limit": {
           "primary_window": {"used_percent": 40, "limit_window_seconds": 18000,
                              "reset_at": 1787256724},
           "secondary_window": {"used_percent": 12, "limit_window_seconds": 604800,
                                "reset_at": 1787855484}}}
      ]
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("the extra pool parses");

    let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(
        keys,
        ["w604800", "codex_spark/w18000", "codex_spark/w604800"],
        "the extra pool's weekly window must not collide with the account's own"
    );
    assert_eq!(snapshot.windows[1].title, "GPT-5.3-Codex-Spark · 5 hours");
    assert_eq!(snapshot.windows[2].title, "GPT-5.3-Codex-Spark · 7 days");
}

#[test]
fn the_gpt_reserve_pool_is_dropped_instead_of_drawn_as_a_second_week() {
    // The body the live endpoint returned on 2026-08-22, identifiers replaced. The
    // reserve's reset is `captured_at + 604800` to the second, so its seven days restart
    // on every poll while the account's own weekly window counts down beside it.
    let body = r#"{
      "plan_type": "plus",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": {"used_percent": 40, "limit_window_seconds": 604800,
                           "reset_after_seconds": 560287, "reset_at": 1787815811},
        "secondary_window": null
      },
      "additional_rate_limits": [
        {"limit_name": "gpt-reserve", "metered_feature": "base_model_inference",
         "rate_limit": {
           "allowed": true,
           "limit_reached": false,
           "primary_window": {"used_percent": 0, "limit_window_seconds": 604800,
                              "reset_after_seconds": 604800, "reset_at": 1787860324},
           "secondary_window": null}}
      ]
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("the live body parses");

    let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(
        keys,
        ["w604800"],
        "the account's own weekly window is the only quota in this body"
    );
}

#[test]
fn the_reserve_is_known_by_its_feature_slug_and_not_by_the_name_it_is_shown_under() {
    // Same entry with the display name reworded: the slug still identifies it. And a
    // pool that merely calls itself gpt-reserve while metering something else is a real
    // limit, drawn like any other.
    let reworded = r#"{
      "rate_limit": {"primary_window": {"used_percent": 40, "limit_window_seconds": 604800}},
      "additional_rate_limits": [
        {"limit_name": "GPT Reserve", "metered_feature": "base_model_inference",
         "rate_limit": {"primary_window": {"used_percent": 0, "limit_window_seconds": 604800}}}
      ]
    }"#;
    let snapshot = codex::parse(reworded, captured_at()).expect("parses");
    assert_eq!(snapshot.windows.len(), 1);

    let borrowed_name = r#"{
      "rate_limit": {"primary_window": {"used_percent": 40, "limit_window_seconds": 604800}},
      "additional_rate_limits": [
        {"limit_name": "gpt-reserve", "metered_feature": "some_new_pool",
         "rate_limit": {"primary_window": {"used_percent": 7, "limit_window_seconds": 18000}}}
      ]
    }"#;
    let snapshot = codex::parse(borrowed_name, captured_at()).expect("parses");
    let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys, ["w604800", "some_new_pool/w18000"]);
}

#[test]
fn an_extra_pool_with_no_name_at_all_fails_rather_than_colliding_with_the_main_one() {
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": 19, "limit_window_seconds": 604800}},
      "additional_rate_limits": [
        {"rate_limit": {"primary_window": {"used_percent": 40, "limit_window_seconds": 604800}}}
      ]
    }"#;

    assert!(matches!(
        codex::parse(body, captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}

#[test]
fn code_review_has_its_own_pool_rather_than_overwriting_the_account_window() {
    let body = r#"{
      "rate_limit": {"primary_window": {"used_percent": 19, "limit_window_seconds": 604800}},
      "code_review_rate_limit": {
        "primary_window": {"used_percent": 3, "limit_window_seconds": 604800}}
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("both pools parse");

    let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys, ["w604800", "code_review/w604800"]);
    assert_eq!(snapshot.windows[1].title, "Code review · 7 days");
}

#[test]
fn the_plan_is_filed_where_the_card_looks_for_it_and_spelled_for_a_person() {
    let snapshot = codex::parse(LIVE_SHAPE, captured_at()).expect("live shape parses");

    assert_eq!(snapshot.details[0].title, DetailSection::PLAN);
    assert_eq!(snapshot.details[0].rows[0].value, "Plus");
}

#[test]
fn a_multi_word_plan_slug_is_spelled_as_words() {
    let body = r#"{"plan_type": "free_workspace", "rate_limit": null}"#;

    let snapshot = codex::parse(body, captured_at()).expect("plan parses");

    assert_eq!(snapshot.details[0].rows[0].value, "Free Workspace");
}

#[test]
fn an_exhausted_pool_reports_no_credits_and_no_reset_credits() {
    let snapshot = codex::parse(LIVE_SHAPE, captured_at()).expect("live shape parses");

    let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        [DetailSection::PLAN],
        "empty balances are noise, not a section"
    );
}

#[test]
fn credits_and_reset_credits_become_details_rather_than_invented_windows() {
    let body = r#"{
      "plan_type": "pro",
      "rate_limit": {"primary_window": {"used_percent": 100, "limit_window_seconds": 604800}},
      "credits": {"has_credits": true, "unlimited": false, "balance": "12.5"},
      "rate_limit_reset_credits": {"available_count": 2, "applicable_available_count": 1}
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("details parse");

    assert_eq!(snapshot.windows.len(), 1, "no balance became a window");
    let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, [DetailSection::PLAN, "Credits", "Reset credits"]);
    assert_eq!(snapshot.details[1].rows[0].value, "12.5");
    assert_eq!(snapshot.details[2].rows[0].value, "2");
}

#[test]
fn an_unlimited_balance_says_so_instead_of_printing_a_number() {
    let body = r#"{
      "rate_limit": null,
      "credits": {"has_credits": true, "unlimited": true, "balance": "0"}
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("details parse");

    assert_eq!(snapshot.details[0].title, "Credits");
    assert_eq!(snapshot.details[0].rows[0].value, "Unlimited");
}

#[test]
fn a_spend_control_limit_is_kept_as_a_detail() {
    let body = r#"{
      "rate_limit": null,
      "spend_control": {"reached": false,
        "individual_limit": {"limit": 100, "used": 42.5, "resets_at": 1787855484}}
    }"#;

    let snapshot = codex::parse(body, captured_at()).expect("details parse");

    assert_eq!(snapshot.details[0].title, "Spend");
    assert_eq!(snapshot.details[0].rows[0].value, "42.5 of 100");
}

#[test]
fn a_body_that_is_not_a_usage_response_at_all_is_malformed() {
    assert!(matches!(
        codex::parse("<html>signed out</html>", captured_at()),
        Err(ProviderError::Malformed { .. })
    ));
}
