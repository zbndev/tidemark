//! The decisions about *what* to show, kept away from the widgets that show it.
//!
//! Two of them: the order the windows appear in on a card, and the order the cards appear
//! in on the grid. Both are pure functions over published statuses, so both are tested
//! without a display.

use tidemark_types::{ProviderStatus, Snapshot, Window, WindowLength};

/// The windows of a reading, in the order the card draws them.
///
/// Shortest first, windows of unknown length last — the same rule as
/// [`Snapshot::dominant_window`], which is why the first element of this list *is* the
/// dominant window. A test below pins them together, because two implementations of one
/// rule is exactly how the card ends up leading with a different window than the one the
/// rest of the program calls dominant.
///
/// Nothing is added and nothing is filled in: a provider that reported one window this time
/// and three the last gets one row, and the card silently rearranges.
pub fn ordered_windows(snapshot: &Snapshot) -> Vec<Window> {
    let mut windows = snapshot.windows.clone();
    windows.sort_by_key(|window| {
        (
            window.length.is_none(),
            window.length.map(WindowLength::as_secs),
        )
    });
    windows
}

/// How much attention an account is asking for, as the fraction of its dominant window that
/// is gone. `None` for an account with no reading at all.
///
/// This is only half of what `CONTEXT.md` § Interface eventually wants — the other half is
/// the user's own order, which Step 15 persists to config and which will replace this as
/// the outer sort. Until then the grid sorts itself, and an account that has never answered
/// sits at the end rather than at the top, since a card with no numbers on it is not urgent,
/// it is just empty.
pub fn urgency(status: &ProviderStatus) -> Option<f64> {
    status
        .to_snapshot()?
        .dominant_window()
        .map(|window| window.used_percent)
}

/// Orders two cards for the grid: most consumed first, accounts with no reading last, ties
/// broken by slug so that the grid does not shuffle between updates.
pub fn compare(left: &ProviderStatus, right: &ProviderStatus) -> std::cmp::Ordering {
    let key = |status: &ProviderStatus| urgency(status).unwrap_or(-1.0);
    key(right)
        .partial_cmp(&key(left))
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| left.provider.cmp(&right.provider))
        .then_with(|| left.account.cmp(&right.account))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, Timestamp, WindowKey};

    fn window(length: Option<u64>, used: f64) -> Window {
        Window {
            key: WindowKey::named(&format!("w{length:?}")),
            title: format!("{length:?}"),
            used_percent: used,
            resets_at: None,
            length: length.and_then(WindowLength::from_secs),
        }
    }

    fn snapshot(windows: Vec<Window>) -> Snapshot {
        Snapshot {
            provider: ProviderId::new("zai"),
            account: AccountId::default(),
            captured_at: Timestamp::from_unix(1_785_700_000).expect("plausible"),
            windows,
            details: Vec::new(),
        }
    }

    fn status(provider: &str, windows: Vec<Window>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new(provider), &AccountId::default());
        status.set_reading(&snapshot(windows));
        status
    }

    #[test]
    fn the_card_leads_with_the_window_the_rest_of_the_program_calls_dominant() {
        let snapshot = snapshot(vec![
            window(Some(2_592_000), 1.0),
            window(Some(18_000), 2.0),
            window(None, 3.0),
            window(Some(604_800), 4.0),
        ]);
        let ordered = ordered_windows(&snapshot);
        assert_eq!(
            Some(&ordered[0]),
            snapshot.dominant_window(),
            "two rules for one thing have drifted apart"
        );
        let lengths: Vec<Option<u64>> = ordered
            .iter()
            .map(|w| w.length.map(WindowLength::as_secs))
            .collect();
        assert_eq!(
            lengths,
            [Some(18_000), Some(604_800), Some(2_592_000), None]
        );
    }

    #[test]
    fn a_provider_that_reported_one_window_gets_one_row() {
        assert_eq!(
            ordered_windows(&snapshot(vec![window(Some(18_000), 0.0)])).len(),
            1
        );
        assert!(ordered_windows(&snapshot(Vec::new())).is_empty());
    }

    #[test]
    fn the_fullest_account_is_the_first_card() {
        let mut cards = [
            status("kimi", vec![window(Some(18_000), 10.0)]),
            status("zai", vec![window(Some(18_000), 90.0)]),
        ];
        cards.sort_by(compare);
        assert_eq!(cards[0].provider, "zai");
    }

    #[test]
    fn an_account_with_nothing_to_show_sorts_last_rather_than_first() {
        let waiting = ProviderStatus::pending(
            &ProviderId::new("aaa-first-alphabetically"),
            &AccountId::default(),
        );
        let mut cards = [waiting, status("zai", vec![window(Some(18_000), 0.0)])];
        cards.sort_by(compare);
        assert_eq!(
            cards[0].provider, "zai",
            "0% of a real reading outranks no reading at all"
        );
    }

    #[test]
    fn equally_urgent_accounts_keep_a_stable_order() {
        let mut cards = [
            status("zai", vec![window(Some(18_000), 50.0)]),
            status("codex", vec![window(Some(18_000), 50.0)]),
        ];
        cards.sort_by(compare);
        let first = cards[0].provider.clone();
        cards.sort_by(compare);
        assert_eq!(cards[0].provider, first);
        assert_eq!(first, "codex");
    }
}
