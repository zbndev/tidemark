//! Usage as a Waybar custom module consumes it.
//!
//! The number is the worst dominant window across the selected accounts — the same window
//! a card leads with, so the panel and the grid never disagree about which limit matters.
//! `class` is always an array: a rate-limited account at 95% is `["danger", "stale"]`, and
//! a shape that changed with the situation would make somebody's CSS conditional.

use serde::Serialize;
use tidemark_types::present::{duration, percent};
use tidemark_types::{
    DANGER_AT, ProviderState, ProviderStatus, Timestamp, WARNING_AT, provider_label,
};

/// What a Waybar custom module reads.
#[derive(Debug, Serialize)]
struct Card {
    text: String,
    tooltip: String,
    class: Vec<String>,
    percentage: u8,
}

pub fn render(statuses: &[&ProviderStatus], now: Timestamp) -> Result<String, serde_json::Error> {
    let worst = statuses
        .iter()
        .filter_map(|status| super::dominant(status))
        .map(|window| window.used_percent)
        .fold(None, |worst: Option<f64>, used| {
            Some(worst.map_or(used, |worst| worst.max(used)))
        });
    let stale = statuses
        .iter()
        .any(|status| ProviderState::from_wire(&status.state) != Some(ProviderState::Ok));

    let card = match worst {
        Some(used) => Card {
            text: percent(used),
            tooltip: tooltip(statuses, now),
            class: classes(Some(used), stale),
            percentage: used.clamp(0.0, 100.0).round() as u8,
        },
        // An empty text hides the module, which is the honest rendering of "no reading
        // yet": a zero would be a claim about quota nobody has measured.
        None => Card {
            text: String::new(),
            tooltip: "Tidemark has no reading yet".to_owned(),
            class: classes(None, true),
            percentage: 0,
        },
    };
    serde_json::to_string(&card)
}

/// The zone first, so a stylesheet keyed on `.danger` keeps working when a second class
/// applies. The boundaries are the shared constants the bar recolours at.
fn classes(used: Option<f64>, stale: bool) -> Vec<String> {
    let mut classes = Vec::new();
    if let Some(used) = used {
        classes.push(
            if used >= DANGER_AT {
                "danger"
            } else if used >= WARNING_AT {
                "warning"
            } else {
                "ok"
            }
            .to_owned(),
        );
    }
    if stale {
        classes.push("stale".to_owned());
    }
    classes
}

fn tooltip(statuses: &[&ProviderStatus], now: Timestamp) -> String {
    statuses
        .iter()
        .map(|status| {
            let mut line = format!(
                "{} · {}",
                provider_label(&status.provider),
                status
                    .account_label
                    .as_deref()
                    .unwrap_or(status.account.as_str())
            );
            if let Some(window) = super::dominant(status) {
                line.push_str(&format!(
                    " — {} {}",
                    window.title,
                    percent(window.used_percent)
                ));
                if let Some(seconds) = window.seconds_until_reset(now) {
                    line.push_str(&format!(", resets in {}", duration(seconds)));
                }
            }
            if ProviderState::from_wire(&status.state) != Some(ProviderState::Ok) {
                line.push_str(&format!(" [{}]", status.state));
            }
            escape(&line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waybar renders a tooltip as Pango markup, and a provider's own window title is text we
/// did not write. Escaping the five predefined entities is the whole of it.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\'' => escaped.push_str("&apos;"),
            '"' => escaped.push_str("&quot;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, WindowStatus};

    const NOW: i64 = 1_785_708_000;

    fn now() -> Timestamp {
        Timestamp::from_unix(NOW).expect("plausible")
    }

    fn status(provider: &str, state: ProviderState, used_percent: f64) -> ProviderStatus {
        let mut status =
            ProviderStatus::pending(&ProviderId::new(provider.to_owned()), &AccountId::default());
        status.state = state.as_wire().to_owned();
        status.captured_at = Some(NOW - 60);
        status.windows = vec![WindowStatus {
            key: "w18000".to_owned(),
            title: "Session".to_owned(),
            subtitle: None,
            used_percent,
            resets_at: Some(NOW + 3_600),
            length_secs: Some(18_000),
        }];
        status
    }

    fn card(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid json")
    }

    #[test]
    fn the_worst_account_sets_the_number() {
        let low = status("claude", ProviderState::Ok, 12.0);
        let high = status("codex", ProviderState::Ok, 91.0);
        let card = card(&render(&[&low, &high], now()).expect("serializes"));
        assert_eq!(card["text"], "91%");
        assert_eq!(card["percentage"], 91);
        assert_eq!(card["class"][0], "danger");
    }

    #[test]
    fn a_stale_account_keeps_its_zone() {
        let status = status("claude", ProviderState::RateLimited, 95.0);
        let card = card(&render(&[&status], now()).expect("serializes"));
        assert_eq!(card["class"][0], "danger");
        assert_eq!(card["class"][1], "stale");
    }

    #[test]
    fn nothing_to_report_hides_the_module() {
        let pending =
            ProviderStatus::pending(&ProviderId::new("zai".to_owned()), &AccountId::default());
        let card = card(&render(&[&pending], now()).expect("serializes"));
        assert_eq!(card["text"], "");
        assert_eq!(card["percentage"], 0);
        assert_eq!(card["class"][0], "stale");
    }

    #[test]
    fn a_tooltip_escapes_the_provider_own_text() {
        let mut status = status("claude", ProviderState::Ok, 50.0);
        status.windows[0].title = "Session & overage".to_owned();
        let card = card(&render(&[&status], now()).expect("serializes"));
        let tooltip = card["tooltip"].as_str().expect("a string");
        assert!(tooltip.contains("&amp;"), "{tooltip}");
        assert!(!tooltip.contains("Session & overage"), "{tooltip}");
    }
}
