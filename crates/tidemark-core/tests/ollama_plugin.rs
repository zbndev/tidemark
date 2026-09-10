//! The Ollama Cloud example plugin against recorded endpoint fixtures.
//!
//! `render` is the one transformation: the same function `tidemarkctl plugin render`
//! and polling use. These tests keep the example honest against both plan shapes
//! `ollama.com/api/usage` serves — a free plan whose `limits.monthly` carries no
//! absolute cap, and a paid plan carrying `usage_limit`.

use tidemark_core::plugin::{provider, schema};
use tidemark_types::{AccountId, Timestamp};

const FILE: &str = include_str!("fixtures/plugin/ollama-cloud.tidemark-provider");
const FREE: &str = include_str!("fixtures/plugin/ollama-free-response.json");
const PAID: &str = include_str!("fixtures/plugin/ollama-paid-response.json");

fn render(file: &str) -> tidemark_core::plugin::Reading {
    let account = AccountId::new("default".to_string());
    let definition = schema::parse(FILE.as_bytes(), &[]).unwrap();
    provider::render(
        &definition,
        file.as_bytes(),
        &account,
        Timestamp::from_unix(1_800_000_000).expect("a reasonable test timestamp"),
    )
    .unwrap()
}

fn metric<'a>(reading: &'a tidemark_core::plugin::Reading, id: &str) -> &'a tidemark_types::Metric {
    reading
        .presentation
        .metrics
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("metric {id} is present"))
}

#[test]
fn the_free_plan_renders_usage_without_a_window() {
    let reading = render(FREE);
    // The endpoint reports no cap on the free plan: the reading must carry the
    // usage value and must not invent a limit, a percentage or a window.
    let monthly = metric(&reading, "monthly-usage");
    assert_eq!(monthly.value, Some(0.0), "the raw usage is reported");
    assert!(monthly.maximum.is_none(), "no fabricated cap");
    assert!(monthly.used_percent.is_none(), "no fabricated percentage");
    assert!(monthly.window.is_none(), "no fabricated window");
}

#[test]
fn the_paid_plan_renders_a_windowed_gauge() {
    let reading = render(PAID);
    let monthly = metric(&reading, "monthly-usage");
    assert_eq!(monthly.value, Some(41.2));
    assert_eq!(monthly.maximum, Some(100.0));
    assert_eq!(
        monthly.used_percent,
        Some(41.2),
        "the percentage matches usage against the cap"
    );
    let window = monthly.window.as_ref().expect("the monthly window");
    assert_eq!(window.key, "month");

    // Per-model rows: a capped model carries its ratio fields, and a model with
    // a null limit is still listed with its raw usage and nothing invented.
    let gemma = metric(&reading, "model-gemma4:31b");
    assert_eq!(gemma.value, Some(320.0));
    assert_eq!(gemma.maximum, Some(1000.0));
    let oss = metric(&reading, "model-gpt-oss:120b");
    assert_eq!(oss.value, Some(74.0));
    assert!(oss.maximum.is_none(), "null limit stays uncapped");
}

#[test]
fn a_rejected_credential_body_fails_loudly() {
    // The endpoint answers 401 with a JSON error body; polling never hands that
    // to the parser (it is a transport failure), but render must refuse the same
    // body just the same — nothing may be read out of an error document.
    let rejected = include_str!("fixtures/plugin/ollama-rejected-response.json");
    let account = AccountId::new("default".to_string());
    let definition = schema::parse(FILE.as_bytes(), &[]).unwrap();
    let outcome = provider::render(
        &definition,
        rejected.as_bytes(),
        &account,
        Timestamp::from_unix(1_800_000_000).expect("a reasonable test timestamp"),
    );
    assert!(outcome.is_err(), "an error document renders nothing");
}

#[test]
fn a_truncated_body_fails_loudly() {
    // A cut-off response (connection dropped mid-body) is not valid JSON: the
    // parse must fail rather than half-render.
    let truncated = r#"{"activity":{"cost":"0.00000""#;
    let account = AccountId::new("default".to_string());
    let definition = schema::parse(FILE.as_bytes(), &[]).unwrap();
    let outcome = provider::render(
        &definition,
        truncated.as_bytes(),
        &account,
        Timestamp::from_unix(1_800_000_000).expect("a reasonable test timestamp"),
    );
    assert!(outcome.is_err(), "a truncated body renders nothing");
}

#[test]
fn a_shape_without_limits_fails_loudly() {
    // The parser indexes response.limits.monthly directly; a document that lost
    // the limits table must raise inside the sandbox (the documented "fail on
    // purpose" recipe), surfacing as a refused reading — never a partial card.
    let limits_less =
        r#"{"activity":{"cost":"1.0","period":{"ending_at":"2026-09-09T23:05:51Z"},"models":[]}}"#;
    let account = AccountId::new("default".to_string());
    let definition = schema::parse(FILE.as_bytes(), &[]).unwrap();
    let outcome = provider::render(
        &definition,
        limits_less.as_bytes(),
        &account,
        Timestamp::from_unix(1_800_000_000).expect("a reasonable test timestamp"),
    );
    assert!(
        outcome.is_err(),
        "a document without limits renders nothing"
    );
}
