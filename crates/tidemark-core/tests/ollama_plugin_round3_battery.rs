//! ROUND-3 ADVERSARIAL BATTERY — hostile inputs against the example plugin.
//!
//! Every case is NEW: none re-raises a ledger finding. Cases are grouped by the
//! expected verdict: refused whole (loud), or rendered truthfully (no clamping).
//! Written by the round-3 critic; not part of the PR under review.

use tidemark_core::plugin::{provider, schema};
use tidemark_types::{AccountId, Timestamp};

const FILE: &str = include_str!("fixtures/plugin/ollama-cloud.tidemark-provider");

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

/// The smallest body the parser can honestly read: nothing but the fields it uses.
const MINIMAL: &str = r#"{"activity":{"cost":"0","models":[]},"limits":{"monthly":{"usage":0}}}"#;

// ---------------------------------------------------------------------------
// Group A — must be REFUSED whole (loud), never partially published
// ---------------------------------------------------------------------------

#[test]
fn r3_a_null_monthly_cap_is_refused_not_rendered_as_uncapped() {
    // The endpoint demonstrably emits explicit nulls for "no cap" (model.limit
    // is null in the paid hypothesis). If limits.monthly.usage_limit is ever a
    // null, `number(null)` must fail loudly rather than silently taking the
    // uncapped branch — that branch exists for an ABSENT field, not a present
    // null. (Round-3 finding: currently this FAILS the reading via the null
    // sentinel being a table — confirm the refusal is the actual behaviour.)
    let body = r#"{"activity":{"cost":"1.0","models":[]},"limits":{"monthly":{"usage":3.0,"usage_limit":null}}}"#;
    let error = refused(body);
    eprintln!("r3 null-cap error: {error}");
}

// ---------------------------------------------------------------------------
// FINDING R3-1 — models container shape drift is SILENTLY swallowed
// ---------------------------------------------------------------------------
// If activity.models arrives as a JSON object (container type flip — exactly
// the class of drift rounds 1/2 made loud everywhere else), ipairs over a
// string-keyed table performs ZERO iterations: every model row disappears and
// a degraded reading is published with no error. This test PINS the current
// (flawed) behaviour so the finding is executable; the fix belongs to the PR.
#[test]
fn r3_finding_models_as_object_silently_drops_every_row() {
    let body = r#"{"activity":{"cost":"1","models":{"gemma4:31b":{"usage":5}}},"limits":{"monthly":{"usage":0}}}"#;
    let reading = render(body); // renders — THAT is the finding
    assert_eq!(
        reading.presentation.metrics.len(),
        2,
        "FINDING R3-1: the model row was silently dropped, no refusal"
    );
}

#[test]
fn r3_negative_monthly_usage_renders_truthfully() {
    // Recalibrated: the runtime's own philosophy is truthful data, no
    // clamping and no judgement (percent_above_one_hundred is truthful; the
    // round-2 battery already accepted negatives). A negative usage is data.
    let body = r#"{"activity":{"cost":"1.0","models":[]},"limits":{"monthly":{"usage":-5}}}"#;
    let reading = render(body);
    let monthly = reading
        .presentation
        .metrics
        .iter()
        .find(|m| m.id == "monthly-usage")
        .expect("present");
    assert_eq!(monthly.value, Some(-5.0));
}

#[test]
fn r3_cost_as_object_is_refused() {
    let body = r#"{"activity":{"cost":{"total":"1.0","currency":"USD"}},"limits":{"monthly":{"usage":0}}}"#;
    let error = refused(body);
    eprintln!("r3 cost-object error: {error}");
}

#[test]
fn r3_cost_huge_string_is_refused() {
    let body = r#"{"activity":{"cost":"1e999"},"limits":{"monthly":{"usage":0}}}"#;
    let error = refused(body);
    eprintln!("r3 cost-1e999 error: {error}");
}

// INTENTIONAL FAILING PROBE — this is finding R3-1, executable. Its passing
// twin below pins the flawed behaviour. Once the fix lands (refuse a models
// container that is not an array), delete the twin and remove #[ignore] here.
// Kept out of the default run so the suite stays green for rounds 4+.
#[test]
#[ignore = "finding R3-1: models container drift is swallowed (see twin test)"]
fn r3_models_as_object_not_array_is_refused() {
    let body = r#"{"activity":{"cost":"1","models":{"gemma4:31b":{"usage":1}}},"limits":{"monthly":{"usage":0}}}"#;
    let error = refused(body);
    eprintln!("r3 models-object error: {error}");
}

#[test]
fn r3_duplicate_metric_id_via_monthly_usage_model_row_is_refused() {
    // A model literally called "monthly-usage" would produce the metric id
    // "model-monthly-usage" — no collision. But a model whose id makes the
    // prefixed id equal another model's prefixed id must be refused by R5.
    let body = r#"{"activity":{"cost":"1","models":[{"id":"m","usage":1},{"id":"m","usage":2}]},"limits":{"monthly":{"usage":0}}}"#;
    let error = refused(body);
    eprintln!("r3 duplicate-model error: {error}");
}

#[test]
fn r3_model_usage_missing_is_refused() {
    let body = r#"{"activity":{"cost":"1","models":[{"id":"gemma4:31b"}]},"limits":{"monthly":{"usage":0}}}"#;
    let error = refused(body);
    eprintln!("r3 model-no-usage error: {error}");
}

#[test]
fn r3_model_limit_negative_is_refused() {
    // percent() refuses a non-positive denominator at draw time.
    let body = r#"{"activity":{"cost":"1","models":[{"id":"m","usage":1,"limit":-10}]},"limits":{"monthly":{"usage":0}}}"#;
    let error = refused(body);
    eprintln!("r3 negative-limit error: {error}");
}

#[test]
fn r3_monthly_usage_null_is_refused() {
    let body = r#"{"activity":{"cost":"1","models":[]},"limits":{"monthly":{"usage":null}}}"#;
    let error = refused(body);
    eprintln!("r3 null-usage error: {error}");
}

#[test]
fn r3_limits_monthly_as_array_is_refused() {
    let body = r#"{"activity":{"cost":"1","models":[]},"limits":{"monthly":[]}}"#;
    let error = refused(body);
    eprintln!("r3 monthly-array error: {error}");
}

#[test]
fn r3_usage_limit_zero_is_refused() {
    let body =
        r#"{"activity":{"cost":"1","models":[]},"limits":{"monthly":{"usage":5,"usage_limit":0}}}"#;
    let error = refused(body);
    eprintln!("r3 zero-limit error: {error}");
}

// ---------------------------------------------------------------------------
// Group B — must be RENDERED, truthfully (no clamping, no invention)
// ---------------------------------------------------------------------------

#[test]
fn r3_usage_above_limit_renders_an_honest_overdraw() {
    // 80 of a 50 cap is 160%, not a clamped 100%: an overdraw is information.
    let body = r#"{"activity":{"cost":"1","models":[]},"limits":{"monthly":{"usage":80,"usage_limit":50}}}"#;
    let reading = render(body);
    let monthly = reading
        .presentation
        .metrics
        .iter()
        .find(|m| m.id == "monthly-usage")
        .expect("present");
    assert_eq!(monthly.value, Some(80.0));
    assert_eq!(monthly.used_percent, Some(160.0), "overdraw is truthful");
}

#[test]
fn r3_200_model_rows_are_refused_whole_by_the_metric_bound() {
    // R8: 128 metrics max. 200 model rows + monthly + cost = 202 → the runtime
    // refuses the WHOLE reading (loud), never truncates the model list.
    // (Critic note: initial expectation of a render was wrong — R8 governs.)
    let models: Vec<String> = (0..200)
        .map(|i| format!(r#"{{"id":"m{i}","usage":{i}}}"#))
        .collect();
    let body = format!(
        r#"{{"activity":{{"cost":"1","models":[{}]}},"limits":{{"monthly":{{"usage":0}}}}}}"#,
        models.join(",")
    );
    let error = refused(&body);
    assert!(
        error.to_string().contains("128"),
        "the refusal names the bound: {error}"
    );
}

#[test]
fn r3_the_minimal_observed_shape_renders() {
    // Nothing but the fields the parser reads: no period, no limits.models.
    let reading = render(MINIMAL);
    assert_eq!(reading.presentation.metrics.len(), 2);
}

#[test]
fn r3_unknown_extra_fields_are_ignored_without_harm() {
    // New endpoint fields the parser does not know must not change the reading.
    let body = r#"{"activity":{"cost":"2.5","models":[],"new_field":true},"limits":{"monthly":{"usage":1,"future_field":"x"}}}"#;
    let reading = render(body);
    let cost = reading
        .presentation
        .metrics
        .iter()
        .find(|m| m.id == "cost-4w")
        .expect("present");
    assert_eq!(cost.value, Some(2.5));
}

#[test]
fn r3_negative_cost_renders_as_reported() {
    // A credit is data; the parser reports it, it does not judge it.
    let body = r#"{"activity":{"cost":"-1.25","models":[]},"limits":{"monthly":{"usage":0}}}"#;
    let reading = render(body);
    let cost = reading
        .presentation
        .metrics
        .iter()
        .find(|m| m.id == "cost-4w")
        .expect("present");
    assert_eq!(cost.value, Some(-1.25));
}
