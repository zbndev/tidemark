//! Replays real history through the segmenter.
//!
//! **Why this test reads a directory instead of a fixture.** The corpus is nine months of
//! one person's actual usage of five paid services. It is the best possible test input and
//! the worst possible thing to commit to a repository that becomes public. So the test
//! looks for the corpus, runs against it when it is there, and reports that it skipped
//! when it is not. Nothing derived from it is checked in, including the segment counts —
//! the assertions below are properties that any healthy segmentation satisfies, not
//! numbers copied out of one person's history.
//!
//! Point it somewhere else with `TIDEMARK_CORPUS_DIR`.
//!
//! The corpus is written by the reference implementation and carries its conventions:
//! timestamps are seconds since 2001-01-01, and windows are identified by the slot they
//! arrived in. Replaying it means honouring both, which is why this file is the one place
//! allowed to build a [`WindowKey`] from a slot name.

use std::collections::BTreeMap;
use std::path::PathBuf;
use tidemark_core::storage::History;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp, Window, WindowKey};

/// Seconds between the Unix epoch and 2001-01-01.
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

/// Below this a window has too little history for an average to mean anything.
const MINIMUM_POINTS_FOR_RATIO: usize = 20;

/// A healthy segmentation averages at least this many readings per segment. The failure
/// this guards against produced roughly one segment per reading.
const MINIMUM_POINTS_PER_SEGMENT: usize = 10;

struct Reading {
    captured_at: f64,
    used_percent: f64,
    resets_at: Option<f64>,
}

struct Replay {
    points_offered: usize,
    points_rejected: usize,
    segments: i64,
}

fn corpus_dir() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("TIDEMARK_CORPUS_DIR") {
        let path = PathBuf::from(explicit);
        return path.is_dir().then_some(path);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".config/codexbar/history");
    path.is_dir().then_some(path)
}

/// `{ windowId: { segments: [ { points: [ ... ] } ] } }`, flattened and sorted by time.
fn read_file(path: &std::path::Path) -> BTreeMap<String, Vec<Reading>> {
    let text = std::fs::read_to_string(path).expect("corpus file is readable");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("corpus file is JSON");
    let mut windows = BTreeMap::new();

    for (window_id, window) in parsed.as_object().expect("corpus file is an object") {
        let mut readings: Vec<Reading> = window
            .get("segments")
            .and_then(|s| s.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|segment| segment.get("points")?.as_array())
            .flatten()
            .filter_map(|point| {
                Some(Reading {
                    captured_at: point.get("capturedAt")?.as_f64()?,
                    used_percent: point.get("usedPercent")?.as_f64()?,
                    resets_at: point.get("resetsAt").and_then(serde_json::Value::as_f64),
                })
            })
            .collect();
        readings.sort_by(|a, b| a.captured_at.total_cmp(&b.captured_at));
        windows.insert(window_id.clone(), readings);
    }
    windows
}

fn to_unix(apple_seconds: f64) -> i64 {
    apple_seconds as i64 + APPLE_EPOCH_OFFSET
}

fn replay(provider: &str, window_id: &str, readings: &[Reading]) -> Replay {
    let mut history = History::in_memory().expect("in-memory database");
    // The one sanctioned use of `named`: this is historical data whose only identity is
    // the slot it was recorded under. Live adapters key on window length instead.
    let key = WindowKey::named(window_id);
    let mut rejected = 0;

    for reading in readings {
        let Ok(captured_at) = Timestamp::from_unix(to_unix(reading.captured_at)) else {
            rejected += 1;
            continue;
        };
        let snapshot = Snapshot {
            provider: ProviderId::new(provider),
            account: AccountId::default(),
            captured_at,
            windows: vec![Window {
                key: key.clone(),
                title: window_id.to_owned(),
                used_percent: reading.used_percent,
                resets_at: reading
                    .resets_at
                    .and_then(|r| Timestamp::from_unix(to_unix(r)).ok()),
                length: None,
            }],
            details: Vec::new(),
        };
        history.ingest(&snapshot).expect("ingested");
    }

    Replay {
        points_offered: readings.len(),
        points_rejected: rejected,
        segments: history
            .segment_count(provider, "default", &key)
            .expect("counted"),
    }
}

#[test]
fn real_history_does_not_shatter_into_one_segment_per_reading() {
    let Some(dir) = corpus_dir() else {
        eprintln!(
            "skipped: no corpus. Set TIDEMARK_CORPUS_DIR to a directory of \
             reference-implementation history files to run this."
        );
        return;
    };

    let mut windows_checked = 0;
    let mut absurd_timestamps = 0;

    for entry in std::fs::read_dir(&dir).expect("corpus directory is readable") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let provider = path
            .file_stem()
            .expect("file has a stem")
            .to_string_lossy()
            .into_owned();

        for (window_id, readings) in read_file(&path) {
            if readings.is_empty() {
                continue;
            }
            let result = replay(&provider, &window_id, &readings);
            absurd_timestamps += result.points_rejected;
            windows_checked += 1;
            // Printed rather than asserted: the numbers are a property of one person's
            // usage, so they belong in the run output, not in the repository.
            eprintln!(
                "  {provider}/{window_id}: {} readings -> {} segments",
                result.points_offered, result.segments
            );

            let stored = result.points_offered - result.points_rejected;
            if stored == 0 {
                // codex/primary is exactly this: a single reading stamped 1970. Refusing
                // it is the point, and a window with nothing left has no history.
                assert_eq!(
                    result.segments, 0,
                    "{provider}/{window_id} stored a segment out of nothing but rejected \
                     readings"
                );
                continue;
            }
            assert!(
                result.segments >= 1,
                "{provider}/{window_id} produced no segments from {stored} readings"
            );

            if stored >= MINIMUM_POINTS_FOR_RATIO {
                let per_segment = stored as f64 / result.segments as f64;
                assert!(
                    per_segment >= MINIMUM_POINTS_PER_SEGMENT as f64,
                    "{provider}/{window_id} averaged {per_segment:.1} readings per segment \
                     ({} segments from {stored} readings); segmentation is shattering",
                    result.segments
                );
            }
        }
    }

    assert!(
        windows_checked > 0,
        "corpus at {} contained no windows",
        dir.display()
    );
    assert!(
        absurd_timestamps > 0,
        "the corpus is known to contain at least one 1970 timestamp; if none was rejected, \
         either the corpus changed or the ingest guard stopped working"
    );
    eprintln!(
        "replayed {windows_checked} windows from {}; rejected {absurd_timestamps} absurd \
         timestamps",
        dir.display()
    );
}

#[test]
fn replaying_the_same_history_twice_gives_the_same_answer() {
    let Some(dir) = corpus_dir() else {
        eprintln!("skipped: no corpus.");
        return;
    };

    for entry in std::fs::read_dir(&dir).expect("corpus directory is readable") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let provider = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned();
        for (window_id, readings) in read_file(&path) {
            if readings.is_empty() {
                continue;
            }
            assert_eq!(
                replay(&provider, &window_id, &readings).segments,
                replay(&provider, &window_id, &readings).segments,
                "{provider}/{window_id} segmented differently on a second pass"
            );
        }
    }
}
