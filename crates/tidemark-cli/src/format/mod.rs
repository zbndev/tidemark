//! Turning what the daemon published into what a caller asked for.

pub mod json;
pub mod text;

use tidemark_types::{ProviderStatus, Window};

/// The accounts a `--provider` / `--account` pair names, in the daemon's published order.
///
/// The order is the user's, kept by the daemon and never re-sorted here: a CLI that
/// ordered by urgency would disagree with the grid about which card comes first.
pub fn select<'a>(
    statuses: &'a [ProviderStatus],
    provider: Option<&str>,
    account: Option<&str>,
) -> Vec<&'a ProviderStatus> {
    statuses
        .iter()
        .filter(|status| provider.is_none_or(|slug| status.provider == slug))
        .filter(|status| account.is_none_or(|id| status.account == id))
        .collect()
}

/// The window a card would lead with, cloned out of the rebuilt snapshot so it outlives it.
///
/// `Snapshot::dominant_window` is the shared rule — shortest window unless the provider
/// names a lead one — and reusing it is what keeps the CLI, the card and the tray naming
/// the same window.
#[expect(
    dead_code,
    reason = "waybar and guard are its callers, in later commits"
)]
pub fn dominant(status: &ProviderStatus) -> Option<Window> {
    let snapshot = status.to_snapshot()?;
    snapshot.dominant_window().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(
            &ProviderId::new(provider.to_owned()),
            &AccountId::new(account.to_owned()),
        )
    }

    #[test]
    fn no_filter_keeps_every_account_in_published_order() {
        let statuses = vec![status("codex", "default"), status("claude", "work")];
        let selected = select(&statuses, None, None);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].provider, "codex");
    }

    #[test]
    fn a_provider_filter_keeps_all_of_its_accounts() {
        let statuses = vec![
            status("claude", "default"),
            status("claude", "work"),
            status("codex", "default"),
        ];
        assert_eq!(select(&statuses, Some("claude"), None).len(), 2);
        assert_eq!(select(&statuses, Some("claude"), Some("work")).len(), 1);
    }

    #[test]
    fn an_account_that_is_not_there_selects_nothing() {
        let statuses = vec![status("claude", "default")];
        assert!(select(&statuses, Some("claude"), Some("nope")).is_empty());
    }
}
