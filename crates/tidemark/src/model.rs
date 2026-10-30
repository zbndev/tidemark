//! The decisions about *what* to show, kept away from the widgets that show it.
//!
//! Three of them: the order the windows appear in on a card, the order the cards appear
//! in on the grid, and what a provider is called. All pure functions over published data,
//! so all tested without a display.

use std::collections::BTreeMap;

use tidemark_types::{ProviderDefinition, ProviderStatus};

/// The order a card draws a reading's windows in. Defined in the shared crate, so the
/// daemon's adapter and this renderer cannot drift apart.
pub use tidemark_types::ordered_windows;

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

/// The account order a drag between two extra accounts' visible slots makes: the
/// provider and its accounts, the first still first. `None` for any other pair.
pub fn account_order(
    statuses: &[ProviderStatus],
    visible: &[ProviderStatus],
    from: usize,
    to: usize,
) -> Option<(String, Vec<String>)> {
    if from == to {
        return None;
    }
    let source = visible.get(from)?;
    let target = visible.get(to)?;
    if source.provider != target.provider
        || source.account == "default"
        || target.account == "default"
    {
        return None;
    }
    let mut accounts: Vec<String> = provider_groups(statuses)
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
    Some((source.provider.clone(), accounts))
}

/// Where a carried provider group lands.
#[derive(Debug, Eq, PartialEq)]
pub struct GroupDrop {
    /// The group's index among the provider groups, before and after the move.
    pub from: usize,
    pub to: usize,
    /// The visible slot its first card then rests in.
    pub slot: usize,
}

/// Where a provider group lands when the card carrying it, from visible slot `grabbed`, is
/// over visible slot `hovered`. A group is never split, so it lands at the boundary between
/// the others nearest the pointer; of two equally near, the one nearer where it was.
///
/// `spans` are the visible slots each group takes, in order. `None` when `grabbed` lies
/// past them all.
pub fn group_drop(spans: &[usize], grabbed: usize, hovered: usize) -> Option<GroupDrop> {
    let mut end = 0;
    let from = spans.iter().position(|span| {
        end += span;
        grabbed < end
    })?;
    // Where each of the other groups ends: the boundaries the carried one can land on.
    let mut boundary = 0;
    let boundaries = spans
        .iter()
        .enumerate()
        .filter(|&(index, _)| index != from)
        .map(|(_, span)| {
            boundary += span;
            boundary
        });
    let (to, slot) = std::iter::once(0)
        .chain(boundaries)
        .enumerate()
        .min_by_key(|&(to, slot)| (slot.abs_diff(hovered), to.abs_diff(from)))?;
    Some(GroupDrop { from, to, slot })
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
    use tidemark_types::{
        AccountId, ProviderId, ProviderStatus, Snapshot, Timestamp, Window, WindowKey, WindowLength,
    };

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
            plugin: None,
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
            account_order(&statuses, &statuses, 1, 2),
            Some((
                "zai".into(),
                vec!["default".into(), "team".into(), "work".into()]
            ))
        );
    }

    #[test]
    fn an_extra_account_neither_leaves_its_group_nor_displaces_the_first() {
        let statuses = [
            status("zai", "default"),
            status("zai", "work"),
            status("claude", "default"),
        ];

        assert_eq!(account_order(&statuses, &statuses, 1, 2), None);
        assert_eq!(account_order(&statuses, &statuses, 1, 0), None);
    }

    // A, then B with three accounts shown, C, D with two, and E.
    const SPANS: [usize; 5] = [1, 3, 1, 2, 1];

    #[test]
    fn a_carried_group_lands_on_the_boundary_nearest_the_pointer() {
        assert_eq!(
            group_drop(&SPANS, 0, 2),
            Some(GroupDrop {
                from: 0,
                to: 1,
                slot: 3
            }),
            "over B's middle card A is nearer B's end than its start"
        );
        assert_eq!(
            group_drop(&SPANS, 0, 1),
            Some(GroupDrop {
                from: 0,
                to: 0,
                slot: 0
            })
        );
    }

    #[test]
    fn a_group_is_never_split_and_a_tie_keeps_it_where_it_was() {
        assert_eq!(
            group_drop(&SPANS, 4, 5),
            Some(GroupDrop {
                from: 2,
                to: 2,
                slot: 4
            }),
            "over D's first card, C is as near D's end as its start"
        );
        assert_eq!(
            group_drop(&SPANS, 4, 6),
            Some(GroupDrop {
                from: 2,
                to: 3,
                slot: 6
            })
        );
    }

    #[test]
    fn any_shown_account_carries_its_whole_group() {
        assert_eq!(
            group_drop(&SPANS, 3, 0),
            Some(GroupDrop {
                from: 1,
                to: 0,
                slot: 0
            })
        );
        assert_eq!(group_drop(&SPANS, 8, 0), None);
    }
}
