//! The Ollama Cloud example plugin against recorded endpoint fixtures.
//!
//! `render` is the one transformation: the same function `tidemarkctl plugin render`
//! and polling use. Only the free-plan shape has been observed live; the paid-plan
//! fixture is a spelled-out hypothesis (see the plugin's parser comment), and its
//! test says so.

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

fn refused(file: &str) -> tidemark_core::plugin::PluginError {
    let account = AccountId::new("default".to_string());
    let definition = schema::parse(FILE.as_bytes(), &[]).unwrap();
    provider::render(
        &definition,
        file.as_bytes(),
        &account,
        Timestamp::from_unix(1_800_000_000).expect("a reasonable test timestamp"),
    )
    .expect_err("the reading is refused, not rendered")
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
fn the_observed_free_plan_renders_without_inventing_anything() {
    let reading = render(FREE);
    // The only shape observed live (2026-09-09): no cap, so the reading carries
    // the raw usage and nothing else — no maximum, no percentage, no window with
    // an invented reset.
    let monthly = metric(&reading, "monthly-usage");
    assert_eq!(monthly.value, Some(0.0), "the raw usage is reported");
    assert!(monthly.maximum.is_none(), "no fabricated cap");
    assert!(monthly.used_percent.is_none(), "no fabricated percentage");
    assert!(monthly.window.is_none(), "no fabricated window or reset");

    // cost is the endpoint's one string-numbered field: the number() path.
    let cost = metric(&reading, "cost-4w");
    assert_eq!(cost.value, Some(0.0), "string \"0.00000\" reads as 0.0");
    assert_eq!(cost.unit.as_deref(), Some("USD"));
}

#[test]
fn the_hypothesized_paid_plan_renders_a_capped_gauge() {
    let reading = render(PAID);
    let monthly = metric(&reading, "monthly-usage");
    assert_eq!(monthly.value, Some(41.2));
    assert_eq!(monthly.maximum, Some(100.0));
    assert_eq!(
        monthly.used_percent,
        Some(41.2),
        "the percentage matches usage against the cap"
    );
    // No window: the endpoint names no reset for the monthly figure even here.
    assert!(monthly.window.is_none(), "no fabricated window or reset");

    let cost = metric(&reading, "cost-4w");
    assert_eq!(cost.value, Some(12.5), "string \"12.5\" reads as 12.5");

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
fn a_truncated_body_fails_loudly() {
    // A cut-off response (connection dropped mid-body) is not valid JSON: the
    // parse must fail rather than half-render.
    let truncated = r#"{"activity":{"cost":"0.00000""#;
    refused(truncated);
}

#[test]
fn a_shape_without_limits_fails_loudly() {
    // The parser indexes response.limits.monthly directly; a document that lost
    // the limits table must raise inside the sandbox (the documented "fail on
    // purpose" recipe), surfacing as a refused reading — never a partial card.
    let limits_less =
        r#"{"activity":{"cost":"1.0","period":{"ending_at":"2026-09-09T23:05:51Z"},"models":[]}}"#;
    refused(limits_less);
}

#[test]
fn a_model_row_without_an_id_fails_loudly() {
    // A model row that cannot be named cannot be filed: empty and absent ids
    // are recognized malformations (round-2 adversarial finding), refusing the
    // whole reading instead of publishing an unnamed `model-` metric.
    let idless = r#"{"activity":{"cost":"1.0","period":{"ending_at":"2026-09-09T23:05:51Z"},"models":[{"usage":1}]},"limits":{"monthly":{"usage":0}}}"#;
    refused(idless);
}

#[test]
fn a_limits_table_without_usage_fails_loudly() {
    // `usage` is present in every shape this endpoint has served; a monthly
    // table without it is a recognized malformation, and the direct number()
    // call must refuse the whole reading instead of silently dropping the
    // metric from the card.
    let usage_less = r#"{"activity":{"cost":"1.0","period":{"ending_at":"2026-09-09T23:05:51Z"},"models":[]},"limits":{"monthly":{}}}"#;
    refused(usage_less);
}
