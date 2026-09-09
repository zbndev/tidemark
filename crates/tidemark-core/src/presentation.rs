//! Every built-in provider's reading, in the semantic presentation the card renders.
//!
//! One adapter, in core, so the plugin path and the built-in path reach the GTK card
//! through the same shape and there is no second renderer to keep in step. It is a pure
//! function of a [`Snapshot`]: the windows become gauges in the order the card has always
//! drawn them, and the detail sections become ordered status text. Nothing is invented —
//! a window with no reset time produces a metric with no reset time.

use tidemark_types::{
    DetailSection, Emphasis, Field, Metric, MetricWindow, Presentation, PresentedSection, Snapshot,
    Timestamp, Widget, Window, WindowLength, ordered_windows,
};

/// The presentation of one built-in reading.
pub fn from_snapshot(snapshot: &Snapshot) -> Presentation {
    let mut metrics = Vec::new();
    let mut card = Vec::new();

    for (index, window) in ordered_windows(snapshot).iter().enumerate() {
        metrics.push(window_metric(window));
        let emphasis = if index == 0 {
            Emphasis::Normal
        } else {
            Emphasis::Compact
        };
        card.push(
            Widget::gauge(window.key.as_str(), Field::UsedPercent)
                .formatted(tidemark_types::Format::Percent)
                .emphasized(emphasis),
        );
    }

    let mut details = Vec::with_capacity(snapshot.details.len());
    for (section_index, section) in snapshot.details.iter().enumerate() {
        let (section, rows) = detail_section(section_index, section);
        metrics.extend(rows);
        details.push(section);
    }

    Presentation {
        metrics,
        card,
        details,
    }
}

/// One window as a metric. `id` is the window key, so the metric's identity and the
/// history's identity are the same string rather than two strings that agree today.
fn window_metric(window: &Window) -> Metric {
    Metric {
        id: window.key.to_string(),
        title: window.title.clone(),
        subtitle: window.subtitle.clone(),
        value: None,
        maximum: None,
        remaining: None,
        used_percent: Some(window.used_percent),
        text: None,
        unit: None,
        window: Some(MetricWindow {
            key: window.key.to_string(),
            resets_at: window.resets_at.map(Timestamp::as_unix),
            length_secs: window.length.map(WindowLength::as_secs),
        }),
    }
}

/// One detail section, with a metric per row.
///
/// The ids are positional — `detail/<section>/<row>` — because a row's label is the
/// provider's prose and two sections may legitimately use the same one, and a duplicate id
/// is the one thing the validated model refuses.
fn detail_section(
    section_index: usize,
    section: &DetailSection,
) -> (PresentedSection, Vec<Metric>) {
    let mut metrics = Vec::with_capacity(section.rows.len());
    let mut items = Vec::with_capacity(section.rows.len());
    for (row_index, row) in section.rows.iter().enumerate() {
        let id = format!("detail/{section_index}/{row_index}");
        metrics.push(Metric {
            id: id.clone(),
            title: row.label.clone(),
            subtitle: None,
            value: None,
            maximum: None,
            remaining: None,
            used_percent: None,
            text: Some(row.value.clone()),
            unit: None,
            window: None,
        });
        items.push(Widget::status(&id));
    }
    (
        PresentedSection {
            title: section.title.clone(),
            items,
        },
        metrics,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{
        AccountId, DetailRow, DetailSection, Emphasis, Field, ProviderId, Snapshot, Timestamp,
        Window, WindowKey, WindowLength,
    };

    fn at() -> Timestamp {
        Timestamp::from_unix(1_788_870_896).expect("plausible")
    }

    fn window(key: &str, length: u64, used: f64, subtitle: Option<&str>) -> Window {
        Window {
            key: WindowKey::named(key),
            title: format!("{key} title"),
            subtitle: subtitle.map(str::to_owned),
            used_percent: used,
            resets_at: Some(at().saturating_add_seconds(600)),
            length: WindowLength::from_secs(length),
        }
    }

    fn snapshot(windows: Vec<Window>, details: Vec<DetailSection>) -> Snapshot {
        Snapshot {
            provider: ProviderId::new("zai"),
            account: AccountId::default(),
            captured_at: at(),
            windows,
            details,
        }
    }

    #[test]
    fn each_window_becomes_a_gauge_in_card_order() {
        let p = from_snapshot(&snapshot(
            vec![
                window("w604800", 604_800, 10.0, None),
                window("w18000", 18_000, 42.0, Some("100 / 1000 credits")),
            ],
            Vec::new(),
        ));
        let metrics: Vec<&str> = p.card.iter().map(|w| w.metric.as_str()).collect();
        assert_eq!(
            metrics,
            ["w18000", "w604800"],
            "shortest leads, as the card always did"
        );
        assert_eq!(p.card[0].kind, "gauge");
        assert_eq!(p.card[0].field(), Some(Field::UsedPercent));
        assert_eq!(p.card[0].emphasis(), Some(Emphasis::Normal));
        assert_eq!(p.card[1].emphasis(), Some(Emphasis::Compact));
    }

    #[test]
    fn a_windows_identity_and_metadata_survive_the_mapping() {
        let p = from_snapshot(&snapshot(
            vec![window("w18000", 18_000, 42.0, Some("1 / 2"))],
            vec![],
        ));
        let metric = p.metric("w18000").expect("present");
        assert_eq!(metric.used_percent, Some(42.0));
        assert_eq!(metric.subtitle.as_deref(), Some("1 / 2"));
        let w = metric
            .window
            .as_ref()
            .expect("a window metric carries its window");
        assert_eq!(w.key, "w18000");
        assert_eq!(w.length_secs, Some(18_000));
        assert_eq!(
            w.resets_at,
            Some(at().saturating_add_seconds(600).as_unix())
        );
    }

    #[test]
    fn a_window_without_a_reset_or_length_carries_neither() {
        let mut bare = window("monthly", 60, 5.0, None);
        bare.resets_at = None;
        bare.length = None;
        let p = from_snapshot(&snapshot(vec![bare], vec![]));
        let w = p
            .metric("monthly")
            .expect("present")
            .window
            .clone()
            .expect("windowed");
        assert_eq!(
            w.resets_at, None,
            "nothing is invented for a reset the provider never sent"
        );
        assert_eq!(w.length_secs, None);
    }

    #[test]
    fn detail_rows_become_status_widgets_in_their_original_order() {
        let p = from_snapshot(&snapshot(
            vec![],
            vec![DetailSection {
                title: "Plan".into(),
                rows: vec![
                    DetailRow {
                        label: "Level".into(),
                        value: "pro".into(),
                    },
                    DetailRow {
                        label: "Seats".into(),
                        value: "3".into(),
                    },
                ],
            }],
        ));
        let section = &p.details[0];
        assert_eq!(section.title, "Plan");
        let labels: Vec<&str> = section
            .items
            .iter()
            .map(|item| p.metric(&item.metric).expect("present").title.as_str())
            .collect();
        assert_eq!(labels, ["Level", "Seats"]);
        assert_eq!(section.items[0].kind, "status");
        let first = p.metric(&section.items[0].metric).expect("present");
        assert_eq!(first.text.as_deref(), Some("pro"));
        assert!(
            first.window.is_none(),
            "a detail row is not a window and never enters history"
        );
    }

    #[test]
    fn detail_metric_ids_do_not_collide_with_window_ids_or_each_other() {
        let p = from_snapshot(&snapshot(
            vec![window("w18000", 18_000, 1.0, None)],
            vec![
                DetailSection {
                    title: "Plan".into(),
                    rows: vec![DetailRow {
                        label: "w18000".into(),
                        value: "a".into(),
                    }],
                },
                DetailSection {
                    title: "Other".into(),
                    rows: vec![DetailRow {
                        label: "w18000".into(),
                        value: "b".into(),
                    }],
                },
            ],
        ));
        let mut ids: Vec<&str> = p.metrics.iter().map(|m| m.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "every metric id in one reading is unique");
    }

    #[test]
    fn a_reading_with_nothing_in_it_presents_nothing_rather_than_a_placeholder() {
        let p = from_snapshot(&snapshot(vec![], vec![]));
        assert!(p.metrics.is_empty() && p.card.is_empty() && p.details.is_empty());
    }
}
