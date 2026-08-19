//! The seam between a provider and the history: what Step 3 produces is what Step 2 eats.
//!
//! Each half is covered on its own — the parser against fixtures, the segmenter against
//! nine months of real readings. This file exists for the failure neither of those can
//! catch: a snapshot that parses correctly and stores wrongly, because the window keys the
//! provider mints do not behave the way the storage layer assumes.

use tidemark_core::providers::zai;
use tidemark_core::storage::History;
use tidemark_types::{Timestamp, WindowKey, WindowLength};

/// The five-hour window in use, with a reset time.
fn in_use(percent: u32, reset_ms: i64) -> String {
    format!(
        r#"{{"code":200,"success":true,"data":{{"limits":[
             {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":{percent},
              "nextResetTime":{reset_ms}}}
           ]}}}}"#
    )
}

/// The same window just after it rolled over, which drops the reset time. Observed live.
const JUST_RESET: &str = r#"{"code":200,"success":true,"data":{"limits":[
     {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0}
   ]}}"#;

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_unix(seconds).expect("plausible")
}

fn five_hour() -> WindowKey {
    WindowKey::for_length(WindowLength::from_secs(18_000).expect("nonzero"))
}

#[test]
fn a_parsed_snapshot_stores_and_segments_the_way_the_storage_layer_expects() {
    let mut history = History::in_memory().expect("opens");
    let start = 1_787_000_000;
    let reset = (start + 9_000) * 1000;

    for (step, percent) in [10, 40, 70].into_iter().enumerate() {
        let body = in_use(percent, reset);
        let snapshot =
            zai::parse(&body, at(start + step as i64 * 300)).expect("the fixture parses");
        let report = history.ingest(&snapshot).expect("ingests");
        assert!(report.stale.is_empty(), "{report:?}");
        assert_eq!(report.windows.len(), 1);
    }

    assert_eq!(
        history
            .segment_count("zai", "default", &five_hour())
            .expect("counts"),
        1,
        "one window in steady use is one segment"
    );
    assert_eq!(
        history
            .points("zai", "default", &five_hour(), 1)
            .expect("reads")
            .len(),
        3
    );
}

#[test]
fn the_rollover_that_drops_the_reset_time_still_opens_a_segment() {
    // The reset time vanishing is not itself a boundary — a provider that stops reporting
    // one must not shatter the history. Consumption falling to zero is, and it is the only
    // signal left when `resets_at` is gone.
    let mut history = History::in_memory().expect("opens");
    let start = 1_787_000_000;

    let before = zai::parse(&in_use(88, (start + 300) * 1000), at(start)).expect("parses");
    history.ingest(&before).expect("ingests");

    let after = zai::parse(JUST_RESET, at(start + 600)).expect("parses");
    let report = history.ingest(&after).expect("ingests");

    assert_eq!(report.segments_opened().count(), 1, "{report:?}");
    assert_eq!(
        history
            .current_segment("zai", "default", &five_hour())
            .expect("reads"),
        Some(2)
    );
}

#[test]
fn coming_back_from_a_reset_continues_the_new_segment_rather_than_starting_another() {
    // The reverse transition: no reset time, then one. Nothing about that is a rollover,
    // and treating it as one would open a segment on every window's first real use.
    let mut history = History::in_memory().expect("opens");
    let start = 1_787_000_000;

    history
        .ingest(&zai::parse(JUST_RESET, at(start)).expect("parses"))
        .expect("ingests");
    let report = history
        .ingest(&zai::parse(&in_use(3, (start + 18_000) * 1000), at(start + 300)).expect("parses"))
        .expect("ingests");

    assert_eq!(report.segments_opened().count(), 0, "{report:?}");
    assert_eq!(
        history
            .segment_count("zai", "default", &five_hour())
            .expect("counts"),
        1
    );
}

#[test]
fn three_windows_of_one_response_are_three_independent_histories() {
    let body = r#"{"code":200,"success":true,"data":{"limits":[
         {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":0,
          "remaining":1000,"percentage":0,"nextResetTime":1789122642999},
         {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":1787164114706},
         {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":37,"nextResetTime":1787221842997}
       ]}}"#;
    let mut history = History::in_memory().expect("opens");
    let report = history
        .ingest(&zai::parse(body, at(1_787_000_000)).expect("parses"))
        .expect("ingests");

    assert_eq!(report.windows.len(), 3);
    assert!(
        report.stale.is_empty(),
        "a window reported stale here means two of them collided on one key: {report:?}"
    );
    assert_eq!(history.point_count().expect("counts"), 3);
}
