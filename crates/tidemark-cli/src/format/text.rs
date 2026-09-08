//! Usage as a person reads it.
//!
//! Percentages and spans come from `tidemark_types::present`, which is why a number here
//! and the same number on a card cannot drift apart. Everything else on the line is the
//! provider's own text, printed as it arrived.

use tidemark_types::present::{duration, percent};
use tidemark_types::{ProviderStatus, Timestamp, WindowStatus};

use crate::titles::Titles;

/// Every selected account, one block each, in the order the daemon published them.
pub fn render(statuses: &[&ProviderStatus], titles: &Titles, now: Timestamp) -> String {
    let mut out = String::new();
    for status in statuses {
        let label = status
            .account_label
            .as_deref()
            .unwrap_or(status.account.as_str());
        out.push_str(&format!(
            "{} · {}  [{}]\n",
            titles.name(&status.provider),
            label,
            status.state
        ));
        if let Some(plan) = status.plan() {
            out.push_str(&format!("  plan {plan}\n"));
        }
        if let Some(balance) = status.balance() {
            out.push_str(&format!("  balance {balance}\n"));
        }
        if let Some(message) = &status.message {
            out.push_str(&format!("  {message}\n"));
        }
        for window in &status.windows {
            out.push_str(&line(window, now));
        }
        if let Some(captured) = status.captured_at {
            out.push_str(&format!(
                "  read {} ago\n",
                duration(now.as_unix() - captured)
            ));
        }
        out.push('\n');
    }
    out
}

/// One window. The reset span and the pace note appear only when the provider gave enough
/// to compute them; a withheld value is drawn as withheld, never as zero.
fn line(status: &WindowStatus, now: Timestamp) -> String {
    let window = status.to_window();
    let mut line = format!("  {:<20} {:>5}", status.title, percent(window.used_percent));
    if let Some(seconds) = window.seconds_until_reset(now) {
        line.push_str(&format!("  resets in {}", duration(seconds)));
    }
    match window.is_outpacing(now) {
        Some(true) => line.push_str("  outpacing"),
        Some(false) => line.push_str("  on pace"),
        None => {}
    }
    if let Some(subtitle) = &status.subtitle {
        line.push_str(&format!("  ({subtitle})"));
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderDefinition, ProviderId, ProviderState};

    fn status(state: ProviderState, windows: Vec<WindowStatus>) -> ProviderStatus {
        let mut status =
            ProviderStatus::pending(&ProviderId::new("claude".to_owned()), &AccountId::default());
        status.state = state.as_wire().to_owned();
        status.captured_at = Some(1_785_704_400);
        status.windows = windows;
        status
    }

    fn window(resets_at: Option<i64>, used_percent: f64) -> WindowStatus {
        WindowStatus {
            key: "w18000".to_owned(),
            title: "Session".to_owned(),
            subtitle: Some("72 / 100 prompts".to_owned()),
            used_percent,
            resets_at,
            length_secs: Some(18_000),
        }
    }

    /// 1785704400 + 3600.
    const NOW: i64 = 1_785_708_000;

    fn now() -> Timestamp {
        Timestamp::from_unix(NOW).expect("plausible")
    }

    #[test]
    fn a_window_without_a_reset_time_says_nothing_about_one() {
        let status = status(ProviderState::Ok, vec![window(None, 72.0)]);
        let out = render(&[&status], &Titles::default(), now());
        assert!(out.contains("72%"), "{out}");
        assert!(!out.contains("resets in"), "{out}");
        assert!(!out.contains("pace"), "{out}");
    }

    /// Five hours long with one to go, so four fifths of it has elapsed and 72% consumed
    /// is *behind* that pace: `Window::is_outpacing` answers `Some(false)`, which this
    /// renderer words as `on pace`.
    #[test]
    fn a_reset_time_brings_the_span_and_the_pace() {
        let status = status(ProviderState::Ok, vec![window(Some(NOW + 3_600), 72.0)]);
        let out = render(&[&status], &Titles::default(), now());
        assert!(out.contains("resets in 1 h"), "{out}");
        assert!(out.contains("on pace"), "{out}");
    }

    /// The same window with the same hour left, consumed past the elapsed fraction.
    #[test]
    fn a_window_ahead_of_its_elapsed_fraction_is_outpacing() {
        let status = status(ProviderState::Ok, vec![window(Some(NOW + 3_600), 95.0)]);
        let out = render(&[&status], &Titles::default(), now());
        assert!(out.contains("outpacing"), "{out}");
    }

    #[test]
    fn a_failed_poll_keeps_the_last_reading_under_its_state() {
        let mut status = status(ProviderState::Unreachable, vec![window(None, 72.0)]);
        status.message = Some("connection timed out".to_owned());
        let out = render(&[&status], &Titles::default(), now());
        assert!(out.contains("unreachable"), "{out}");
        assert!(out.contains("connection timed out"), "{out}");
        assert!(out.contains("72%"), "{out}");
    }

    #[test]
    fn a_barely_touched_window_never_reads_as_untouched() {
        let status = status(ProviderState::Ok, vec![window(None, 0.2)]);
        assert!(render(&[&status], &Titles::default(), now()).contains("<1%"));
    }

    /// The catalog's own spelling, not the capitalised slug: a person reading this and a
    /// person reading the card must see the same provider name. `provider_label` would say
    /// "Claude" here, so a title the label cannot produce is what proves which one won.
    #[test]
    fn a_provider_is_named_the_way_the_catalog_spells_it() {
        let status = status(ProviderState::Ok, vec![window(None, 10.0)]);
        let titles = Titles::index(&[ProviderDefinition {
            provider: "claude".to_owned(),
            title: "Claude Code".to_owned(),
            credential: "oauth".to_owned(),
            credential_hint: String::new(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
        }]);
        let out = render(&[&status], &titles, now());
        assert!(out.starts_with("Claude Code · default"), "{out}");
    }
}
