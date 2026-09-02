//! The decisions about *what* to show, kept away from the widgets that show it.
//!
//! Three of them: the order the windows appear in on a card, the order the cards appear
//! in on the grid, and what a provider is called. All pure functions over published data,
//! so all tested without a display.

use std::collections::BTreeMap;

use tidemark_types::{ProviderDefinition, ProviderStatus, Snapshot, Window, WindowLength};

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

/// Keeps one provider's accounts adjacent while preserving first-seen provider order.
pub fn provider_groups(statuses: &[ProviderStatus]) -> Vec<Vec<&ProviderStatus>> {
    let mut groups: Vec<Vec<&ProviderStatus>> = Vec::new();
    for status in statuses {
        if let Some(group) = groups.iter_mut().find(|group| {
            group
                .first()
                .is_some_and(|first| first.provider == status.provider)
        }) {
            group.push(status);
        } else {
            groups.push(vec![status]);
        }
    }
    groups
}

/// The daemon operation a completed card drag represents.
#[derive(Debug, Eq, PartialEq)]
pub enum CardReorder {
    /// Moves a whole provider group among the configured providers.
    Providers(Vec<String>),
    /// Moves one extra account within its provider, retaining the default account first.
    Accounts {
        provider: String,
        accounts: Vec<String>,
    },
}

/// Classifies a grid move without allowing an extra account to leave its provider group.
pub fn card_reorder(
    statuses: &[ProviderStatus],
    visible: &[ProviderStatus],
    from: usize,
    to: usize,
) -> Option<CardReorder> {
    if from == to {
        return None;
    }
    let source = visible.get(from)?;
    let target = visible.get(to)?;
    let groups = provider_groups(statuses);

    if source.provider == target.provider {
        if source.account == "default" || target.account == "default" {
            return None;
        }
        let mut accounts: Vec<String> = groups
            .iter()
            .find(|group| group[0].provider == source.provider)?
            .iter()
            .map(|status| status.account.clone())
            .collect();
        let source_index = accounts
            .iter()
            .position(|account| *account == source.account)?;
        let target_index = accounts
            .iter()
            .position(|account| *account == target.account)?;
        let moved = accounts.remove(source_index);
        accounts.insert(target_index, moved);
        Some(CardReorder::Accounts {
            provider: source.provider.clone(),
            accounts,
        })
    } else {
        if source.account != "default" {
            return None;
        }
        let mut providers: Vec<String> = groups
            .iter()
            .map(|group| group[0].provider.clone())
            .collect();
        let source = providers
            .iter()
            .position(|provider| *provider == source.provider)?;
        let target = providers
            .iter()
            .position(|provider| *provider == target.provider)?;
        let moved = providers.remove(source);
        providers.insert(target, moved);
        Some(CardReorder::Providers(providers))
    }
}

/// The windows of a reading, in the order the card draws them.
///
/// The dominant window first — placed there by construction, using the one rule in
/// [`Snapshot::dominant_window`], so the two cannot drift — then shortest first, windows
/// of unknown length last. A test below still pins the first element to the dominant
/// window, because two implementations of one rule is exactly how the card ends up
/// leading with a different window than the one the rest of the program calls dominant.
///
/// Nothing is added and nothing is filled in: a provider that reported one window this time
/// and three the last gets one row, and the card silently rearranges.
pub fn ordered_windows(snapshot: &Snapshot) -> Vec<Window> {
    let lead = snapshot.dominant_window().map(|window| window.key.clone());
    let mut windows = snapshot.windows.clone();
    windows.sort_by_key(|window| {
        (
            !lead.as_ref().is_some_and(|key| *key == window.key),
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
    use tidemark_types::{AccountId, ProviderId, ProviderStatus, Timestamp, WindowKey};

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

    fn keyed_window(key: &str, length: Option<u64>) -> Window {
        Window {
            key: WindowKey::named(key),
            title: key.to_owned(),
            subtitle: None,
            used_percent: 0.0,
            resets_at: None,
            length: length.and_then(WindowLength::from_secs),
        }
    }

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account))
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
    fn the_window_a_provider_leads_with_is_drawn_first_even_when_it_is_not_the_shortest() {
        // NanoGPT's weekly input pool is what its card is about; the hundred-images-a-day
        // allowance beside it is the secondary row.
        let mut card = snapshot(vec![
            keyed_window("images/w86400", Some(86_400)),
            keyed_window("input-tokens/w604800", Some(604_800)),
        ]);
        card.provider = ProviderId::new("nanogpt");

        let ordered = ordered_windows(&card);
        let keys: Vec<&str> = ordered.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["input-tokens/w604800", "images/w86400"]);
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

    #[test]
    fn statuses_of_one_provider_stay_together_in_first_seen_order() {
        let statuses = [
            status("zai", "default"),
            status("claude", "default"),
            status("zai", "work"),
        ];
        let groups = provider_groups(&statuses);

        let identities: Vec<Vec<(&str, &str)>> = groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|status| (status.provider.as_str(), status.account.as_str()))
                    .collect()
            })
            .collect();
        assert_eq!(
            identities,
            vec![
                vec![("zai", "default"), ("zai", "work")],
                vec![("claude", "default")],
            ]
        );
    }

    #[test]
    fn dragging_an_extra_account_within_its_group_only_reorders_that_group() {
        let statuses = [
            status("zai", "default"),
            status("zai", "work"),
            status("zai", "team"),
            status("claude", "default"),
        ];

        assert_eq!(
            card_reorder(&statuses, &statuses, 1, 2),
            Some(CardReorder::Accounts {
                provider: "zai".into(),
                accounts: vec!["default".into(), "team".into(), "work".into()],
            })
        );
    }

    #[test]
    fn dragging_a_main_card_across_groups_reorders_providers() {
        let statuses = [
            status("zai", "default"),
            status("zai", "work"),
            status("claude", "default"),
        ];

        assert_eq!(
            card_reorder(&statuses, &statuses, 0, 2),
            Some(CardReorder::Providers(vec!["claude".into(), "zai".into()]))
        );
    }

    #[test]
    fn dragging_an_extra_account_across_a_group_boundary_is_refused() {
        let statuses = [
            status("zai", "default"),
            status("zai", "work"),
            status("claude", "default"),
        ];

        assert_eq!(card_reorder(&statuses, &statuses, 1, 2), None);
    }
}
