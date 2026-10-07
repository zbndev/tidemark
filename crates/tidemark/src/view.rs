//! What one card says, decided without a toolkit.
//!
//! A pure function from a [`ProviderStatus`] and the current instant to a [`CardView`]; the
//! `.slint` markup only draws what this returns.

use std::borrow::Cow;

use tidemark_types::{
    DANGER_AT, Emphasis, Field, Metric, MetricWindow, Presentation, ProviderState, ProviderStatus,
    Timestamp, WARNING_AT, Widget, WidgetKind, Window, WindowKey, WindowLength, present,
};

use crate::format;
use crate::model;

/// How loud a bar's fill is. Changes where the daemon's notifications fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Warning,
    Danger,
}

pub fn tone(used_percent: f64) -> Tone {
    if used_percent >= DANGER_AT {
        Tone::Danger
    } else if used_percent >= WARNING_AT {
        Tone::Warning
    } else {
        Tone::Normal
    }
}

/// A quota bar: its fill, and the pace mark when the provider gave enough to place one.
#[derive(Debug, Clone, PartialEq)]
pub struct Gauge {
    pub percent: f64,
    pub pace: Option<f64>,
    pub tone: Tone,
    pub blocked: bool,
}

/// One secondary row under the headline.
#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub title: String,
    pub value: String,
    pub gauge: Option<Gauge>,
    pub blocked: bool,
}

/// The headline reading and everything under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub headline: String,
    pub compact: bool,
    pub dominant_title: String,
    pub gauge: Option<Gauge>,
    pub blocked: bool,
    pub reset: Option<String>,
    pub absolutes: Option<String>,
    pub rows: Vec<RowView>,
    pub balance: Option<String>,
}

/// What occupies the body of the card. Exactly one of the three, never a mix.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Reading(Reading),
    Balance {
        whole: String,
        fraction: Option<String>,
    },
    Blank(String),
}

/// Everything one card shows.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    pub plan: Option<String>,
    pub chip: Option<format::Chip>,
    pub body: Body,
    pub footer: Option<String>,
    pub checking: bool,
    pub check_failed: bool,
}

pub fn card(status: &ProviderStatus, now: Timestamp) -> CardView {
    let checking = status.checking == Some(true);
    let check_failed = !checking
        && !matches!(
            status.state(),
            Some(ProviderState::Ok | ProviderState::Pending | ProviderState::WaitingForKeyring)
        );
    let rows = card_rows(status);
    let body = match (rows.split_first(), balance_for(status)) {
        (Some((dominant, rest)), balance) => {
            Body::Reading(reading(status, dominant, rest, balance, now))
        }
        (None, Some(balance)) => {
            let (whole, fraction) = balance_parts(balance);
            Body::Balance {
                whole: whole.to_owned(),
                fraction: fraction.map(str::to_owned),
            }
        }
        (None, None) => Body::Blank(blank_message(status)),
    };
    CardView {
        plan: status.plan().map(str::to_owned),
        chip: format::chip(status),
        body,
        footer: if checking {
            None
        } else if check_failed {
            Some("check failed".into())
        } else {
            format::footer(status, now)
        },
        checking,
        check_failed,
    }
}

fn reading(
    status: &ProviderStatus,
    dominant: &Row<'_>,
    rest: &[Row<'_>],
    balance: Option<&str>,
    now: Timestamp,
) -> Reading {
    let window = (dominant.kind == WidgetKind::Gauge)
        .then(|| gauge_percent(&dominant.metric, &dominant.widget))
        .flatten()
        .map(|percent| (percent, metric_window(&dominant.metric, percent)));
    // A window another window has consumed is drawn dimmed, with its reset line gone: the
    // reset belongs to a quota that is not being enforced right now.
    let blocked = window
        .as_ref()
        .and_then(|(_, window)| window.as_ref())
        .is_some_and(|window| blocking_key(status, window.key.as_str()).is_some());
    let gauge = window.as_ref().map(|(percent, window)| Gauge {
        percent: *percent,
        pace: window.as_ref().and_then(|window| window.pace(now)),
        tone: tone(*percent),
        blocked,
    });
    let reset = (!blocked)
        .then(|| {
            window
                .as_ref()
                .and_then(|(_, window)| window.as_ref())
                .and_then(|window| window.seconds_until_reset(now))
                .map(format::resets_in)
        })
        .flatten();

    Reading {
        headline: dominant.text().unwrap_or_default(),
        compact: dominant.widget.emphasis() == Some(Emphasis::Compact),
        dominant_title: dominant.metric.title.clone(),
        gauge,
        blocked,
        reset,
        absolutes: dominant.metric.subtitle.clone(),
        rows: rest.iter().map(|row| row_view(status, row, now)).collect(),
        balance: balance.map(str::to_owned),
    }
}

fn row_view(status: &ProviderStatus, row: &Row<'_>, now: Timestamp) -> RowView {
    let blocked = row_blocked(status, &row.metric);
    let gauge = (row.kind == WidgetKind::Gauge)
        .then(|| gauge_percent(&row.metric, &row.widget))
        .flatten()
        .map(|percent| Gauge {
            percent,
            pace: metric_window(&row.metric, percent).and_then(|window| window.pace(now)),
            tone: tone(percent),
            blocked,
        });
    RowView {
        title: row.metric.title.clone(),
        value: row.text().unwrap_or_default(),
        gauge,
        blocked,
    }
}

/// The full window that currently makes this status window unavailable.
fn blocking_key<'a>(status: &'a ProviderStatus, key: &str) -> Option<&'a str> {
    status
        .windows
        .iter()
        .find(|window| window.key == key)
        .and_then(|window| window.blocked_by.as_deref())
}

fn row_blocked(status: &ProviderStatus, metric: &Metric) -> bool {
    metric
        .window
        .as_ref()
        .is_some_and(|window| blocking_key(status, &window.key).is_some())
}

/// A drawable widget resolved against its metric. Only old-daemon rows own their data.
struct Row<'a> {
    kind: WidgetKind,
    metric: Cow<'a, Metric>,
    widget: Cow<'a, Widget>,
}

impl Row<'_> {
    fn text(&self) -> Option<String> {
        widget_text(self.kind, &self.metric, &self.widget)
    }
}

fn presentation_row<'a>(presentation: &'a Presentation, widget: &'a Widget) -> Option<Row<'a>> {
    let kind = widget.kind()?;
    let metric = presentation.metric(&widget.metric)?;
    let drawable = match kind {
        WidgetKind::Gauge => gauge_percent(metric, widget).is_some(),
        _ => widget_text(kind, metric, widget).is_some(),
    };
    drawable.then_some(())?;
    Some(Row {
        kind,
        metric: Cow::Borrowed(metric),
        widget: Cow::Borrowed(widget),
    })
}

/// Published order is authoritative. Only an absent presentation takes the rolling-upgrade path.
fn card_rows(status: &ProviderStatus) -> Vec<Row<'_>> {
    if let Some(presentation) = &status.presentation {
        return presentation
            .card
            .iter()
            .filter_map(|widget| presentation_row(presentation, widget))
            .collect();
    }
    status
        .to_snapshot()
        .map(|snapshot| model::ordered_windows(&snapshot))
        .unwrap_or_default()
        .into_iter()
        .filter(|window| window.used_percent.is_finite())
        .map(|window| {
            let metric = Metric {
                id: window.key.to_string(),
                title: window.title,
                subtitle: window.subtitle,
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
            };
            let widget = Widget::gauge(&metric.id, Field::UsedPercent);
            Row {
                kind: WidgetKind::Gauge,
                metric: Cow::Owned(metric),
                widget: Cow::Owned(widget),
            }
        })
        .collect()
}

fn widget_text(kind: WidgetKind, metric: &Metric, widget: &Widget) -> Option<String> {
    if widget.format.is_some() && widget.format().is_none() {
        return None;
    }
    match kind {
        WidgetKind::Gauge => {
            let percent = gauge_percent(metric, widget)?;
            let field = if widget.field.is_none() {
                Field::UsedPercent
            } else {
                widget.field()?
            };
            if field == Field::Text {
                return None;
            }
            if field == Field::UsedPercent {
                let metric = Metric {
                    used_percent: Some(percent),
                    ..metric.clone()
                };
                present::format_field(
                    &metric,
                    field,
                    widget.format().or(Some(tidemark_types::Format::Percent)),
                )
            } else {
                present::format_field(metric, field, widget.format())
            }
        }
        WidgetKind::Value => {
            let field = if widget.field.is_none() {
                Field::Text
            } else {
                widget.field()?
            };
            present::format_field(metric, field, widget.format())
        }
        WidgetKind::Ratio => {
            let (left, right) = (widget.left()?, widget.right()?);
            (metric.field(right)? > 0.0).then_some(())?;
            present::format_ratio(metric, left, right)
        }
        WidgetKind::Status => metric.text.clone(),
    }
}

/// Reported percentage, or a finite quotient with a positive denominator.
fn gauge_percent(metric: &Metric, widget: &Widget) -> Option<f64> {
    if let Some(used) = metric.used_percent {
        return used.is_finite().then_some(used);
    }
    let left = if widget.left.is_none() {
        Field::Value
    } else {
        widget.left()?
    };
    let right = if widget.right.is_none() {
        Field::Maximum
    } else {
        widget.right()?
    };
    let numerator = metric.field(left)?;
    let denominator = metric.field(right)?;
    let percent = numerator / denominator * 100.0;
    (denominator > 0.0 && percent.is_finite()).then_some(percent)
}

fn metric_window(metric: &Metric, used_percent: f64) -> Option<Window> {
    let window = metric.window.as_ref()?;
    Some(Window {
        key: WindowKey::named(&window.key),
        title: metric.title.clone(),
        subtitle: metric.subtitle.clone(),
        used_percent,
        resets_at: window
            .resets_at
            .and_then(|seconds| Timestamp::from_unix(seconds).ok()),
        length: window.length_secs.and_then(WindowLength::from_secs),
    })
}

/// An amount-only balance from a successful reading. Failed polls keep old details, which
/// must not hide the daemon's current explanation.
fn balance_for(status: &ProviderStatus) -> Option<&str> {
    (status.state() == Some(ProviderState::Ok))
        .then(|| status.balance())
        .flatten()
}

/// Splits a formatted balance at its decimal point so the fraction can use smaller type.
fn balance_parts(amount: &str) -> (&str, Option<&str>) {
    let Some(point) = amount.rfind('.') else {
        return (amount, None);
    };
    let (whole, fraction) = amount.split_at(point);
    let digits = &fraction[1..];
    if !whole.is_empty()
        && (1..=4).contains(&digits.len())
        && digits.bytes().all(|digit| digit.is_ascii_digit())
    {
        (whole, Some(fraction))
    } else {
        (amount, None)
    }
}

fn blank_message(status: &ProviderStatus) -> String {
    status
        .message
        .as_deref()
        .unwrap_or("No reading yet.")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId, WindowStatus};

    const CAPTURED_AT: i64 = 1_785_700_000;

    fn window(key: &str, length_secs: u64, used_percent: f64) -> WindowStatus {
        WindowStatus {
            key: key.into(),
            title: key.into(),
            subtitle: None,
            used_percent,
            resets_at: Some(CAPTURED_AT + 3_600),
            length_secs: Some(length_secs),
            blocked_by: None,
        }
    }

    fn status(windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.captured_at = Some(CAPTURED_AT);
        status.windows = windows;
        status.set_state(ProviderState::Ok, None);
        status
    }

    fn now() -> Timestamp {
        Timestamp::from_unix(CAPTURED_AT).expect("plausible")
    }

    #[test]
    fn the_card_leads_with_the_shortest_window_and_lists_the_rest() {
        let view = card(
            &status(vec![
                window("w604800", 604_800, 22.0),
                window("w18000", 18_000, 75.0),
            ]),
            now(),
        );
        let Body::Reading(reading) = view.body else {
            panic!("a card with windows is a reading");
        };
        assert_eq!(reading.dominant_title, "w18000");
        let gauge = reading.gauge.expect("a window is a gauge");
        assert_eq!(gauge.tone, Tone::Warning);
        assert_eq!(gauge.pace, Some(0.8));
        assert_eq!(reading.reset.as_deref(), Some("resets in 1 h"));
        assert_eq!(reading.rows.len(), 1);
        assert_eq!(view.chip, None, "a healthy account says nothing");
        assert_eq!(view.footer.as_deref(), Some("checked just now"));
    }

    #[test]
    fn a_blocked_window_is_dimmed_and_loses_its_reset() {
        let mut five_hour = window("w18000", 18_000, 0.0);
        five_hour.blocked_by = Some("w604800".into());
        let view = card(
            &status(vec![five_hour, window("w604800", 604_800, 100.0)]),
            now(),
        );
        let Body::Reading(reading) = view.body else {
            panic!("a card with windows is a reading");
        };
        assert!(reading.blocked);
        assert!(reading.gauge.expect("still a gauge").blocked);
        assert_eq!(reading.reset, None);
    }

    #[test]
    fn a_balance_only_account_leads_with_its_amount() {
        let mut status = status(Vec::new());
        status.details = vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows: vec![DetailRow {
                label: "Balance".to_owned(),
                value: "$454.5426".to_owned(),
            }],
        }];
        assert_eq!(
            card(&status, now()).body,
            Body::Balance {
                whole: "$454".into(),
                fraction: Some(".5426".into()),
            }
        );
    }

    #[test]
    fn an_account_with_nothing_to_show_says_what_the_daemon_said() {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        assert_eq!(
            card(&status, now()).body,
            Body::Blank("No reading yet.".into())
        );
        status.set_state(
            ProviderState::NoCredential,
            Some("No key is stored for zai.".into()),
        );
        let view = card(&status, now());
        assert_eq!(view.body, Body::Blank("No key is stored for zai.".into()));
        assert_eq!(view.chip.expect("a problem has a chip").text, "no key");
    }

    #[test]
    fn a_failed_check_replaces_the_footer_and_keeps_the_last_reading() {
        let mut status = status(vec![window("w18000", 18_000, 42.0)]);
        status.set_state(ProviderState::Unreachable, Some("request timed out".into()));
        let view = card(&status, now());
        assert_eq!(view.footer.as_deref(), Some("check failed"));
        assert!(matches!(view.body, Body::Reading(_)));
        assert!(view.check_failed);

        status.checking = Some(true);
        let checking = card(&status, now());
        assert!(checking.checking);
        assert!(!checking.check_failed, "a retry hides the previous failure");
        assert_eq!(checking.footer, None, "the spinner replaces the footer");

        status.checking = Some(false);
        status.set_state(ProviderState::Ok, None);
        let success = card(&status, now());
        assert!(!success.checking);
        assert!(!success.check_failed);
        assert_eq!(success.footer.as_deref(), Some("checked just now"));
    }

    #[test]
    fn first_check_failures_have_a_footer_but_pending_and_locked_keyrings_do_not() {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        for state in [ProviderState::Pending, ProviderState::WaitingForKeyring] {
            status.set_state(state, None);
            let view = card(&status, now());
            assert_eq!(view.footer, None);
            assert!(!view.check_failed);
        }
        for state in [
            ProviderState::Malformed,
            ProviderState::RateLimited,
            ProviderState::CredentialRejected,
            ProviderState::NoCredential,
            ProviderState::KeyringUnavailable,
            ProviderState::Unreachable,
        ] {
            status.set_state(state, Some("failure detail".into()));
            let view = card(&status, now());
            assert_eq!(view.footer.as_deref(), Some("check failed"));
            assert!(view.check_failed);
        }
    }
}
