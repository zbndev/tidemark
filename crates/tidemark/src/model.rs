//! The decisions about *what* to show, kept away from the widgets that show it.
//!
//! Three of them: the order the windows appear in on a card, the order the cards appear
//! in on the grid, and what a provider is called. All pure functions over published data,
//! so all tested without a display.

use std::collections::BTreeMap;

use tidemark_types::{ProviderDefinition, Snapshot, Window, WindowLength};

/// The catalog's spelling of each provider's name, by slug.
pub type Titles = BTreeMap<String, String>;

/// Indexes the published definitions by slug, for the name lookups below.
pub fn titles(definitions: &[ProviderDefinition]) -> Titles {
    definitions
        .iter()
        .map(|definition| (definition.provider.clone(), definition.title.clone()))
        .collect()
}

/// What to call a provider on screen.
///
/// The catalog's title when this client has it — "ClinePass", not the capitalised slug
/// "Clinepass" — and [`tidemark_types::provider_label`]'s capitalisation when it does not,
/// because a daemon newer than this client may publish a provider this build has never
/// heard of and its card is still worth drawing with something close to its name.
pub fn name(titles: &Titles, slug: &str) -> String {
    titles
        .get(slug)
        .cloned()
        .unwrap_or_else(|| tidemark_types::provider_label(slug))
}

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

/// The positions `slugs` take when arranged into `order`.
///
/// The user's order is the only order there is — nothing sorts the grid by urgency or by
/// anything else, because an order that rearranged itself would not be the user's. So this
/// is the whole of the rule: what the daemon published, applied to what the window holds.
///
/// Anything `order` does not name keeps its relative place at the end. That is a real case
/// rather than defensiveness: the sequence arrives from another process and may have been
/// sent before an account this client already knows about was added.
pub fn arrangement(slugs: &[String], order: &[String]) -> Vec<usize> {
    let mut positions: Vec<usize> = (0..slugs.len()).collect();
    positions.sort_by_key(|index| {
        order
            .iter()
            .position(|slug| *slug == slugs[*index])
            .unwrap_or(order.len())
    });
    positions
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, Timestamp, WindowKey};

    fn window(length: Option<u64>, used: f64) -> Window {
        Window {
            key: WindowKey::named(&format!("w{length:?}")),
            title: format!("{length:?}"),
            subtitle: None,
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

    #[test]
    fn a_provider_is_called_what_the_catalog_calls_it() {
        let definitions = [ProviderDefinition {
            provider: "clinepass".to_owned(),
            title: "ClinePass".to_owned(),
            credential: "key".to_owned(),
            credential_hint: "ClinePass console.".to_owned(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
        }];
        let titles = titles(&definitions);
        assert_eq!(name(&titles, "clinepass"), "ClinePass");
        assert_eq!(
            name(&titles, "mistral"),
            "Mistral",
            "a daemon newer than this client still gets its cards named"
        );
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
    fn the_published_order_is_the_order_the_cards_take() {
        let held = ["kimi".to_owned(), "zai".to_owned(), "claude".to_owned()];
        let order = ["claude".to_owned(), "kimi".to_owned(), "zai".to_owned()];
        assert_eq!(arrangement(&held, &order), [2, 0, 1]);
    }

    #[test]
    fn an_account_the_order_does_not_name_keeps_its_place_at_the_end() {
        let held = ["kimi".to_owned(), "zai".to_owned(), "claude".to_owned()];
        let order = ["claude".to_owned()];
        assert_eq!(
            arrangement(&held, &order),
            [2, 0, 1],
            "the unnamed accounts stay in the order they were already in"
        );
    }

    #[test]
    fn an_empty_order_moves_nothing() {
        let held = ["kimi".to_owned(), "zai".to_owned()];
        assert_eq!(arrangement(&held, &[]), [0, 1]);
        assert!(arrangement(&[], &["zai".to_owned()]).is_empty());
    }
}
