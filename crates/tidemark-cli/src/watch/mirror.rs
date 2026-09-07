//! The client-side copy of what the daemon published.
//!
//! `watch --format waybar` has to re-render one card from *every* account after each
//! signal, so the stream keeps a mirror. The rules are the daemon's own: a change to a
//! known `(provider, account)` replaces it where it stands, an unknown one goes on the end,
//! and an order announcement is a permutation of provider slugs rather than a new set — a
//! status carries no position, so the sequence has to be applied separately.

use tidemark_types::ProviderStatus;

/// One announced change.
///
/// The size difference between the variants is deliberate: a change carries a whole status
/// because that is what the signal delivers, and the value is constructed once per signal
/// and consumed immediately, so boxing it would buy an allocation and nothing else.
// The tests below are the only callers until `watch` lands, so the expectation holds for
// the binary and would be unfulfilled — an error under `-D warnings` — in a test build.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the watch command builds these from signals, in the next commit"
    )
)]
#[expect(
    clippy::large_enum_variant,
    reason = "one status per signal, consumed at once; see the doc comment"
)]
#[derive(Debug)]
pub enum Change {
    Upsert(ProviderStatus),
    Remove { provider: String, account: String },
    Order(Vec<String>),
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the watch command is the caller, in the next commit"
    )
)]
pub fn apply(statuses: &mut Vec<ProviderStatus>, change: Change) {
    match change {
        Change::Upsert(status) => {
            match statuses
                .iter_mut()
                .find(|held| held.provider == status.provider && held.account == status.account)
            {
                Some(held) => *held = status,
                None => statuses.push(status),
            }
        }
        Change::Remove { provider, account } => {
            statuses.retain(|held| !(held.provider == provider && held.account == account));
        }
        // A stable sort, so accounts keep the order the daemon gave them inside their
        // provider. A provider the announcement does not name sorts last rather than
        // disappearing.
        Change::Order(providers) => statuses.sort_by_key(|status| {
            providers
                .iter()
                .position(|slug| *slug == status.provider)
                .unwrap_or(usize::MAX)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(
            &ProviderId::new(provider.to_owned()),
            &AccountId::new(account.to_owned()),
        )
    }

    #[test]
    fn a_change_replaces_the_account_where_it_stands() {
        let mut statuses = vec![status("claude", "default"), status("codex", "default")];
        let mut changed = status("claude", "default");
        changed.state = ProviderState::Ok.as_wire().to_owned();

        apply(&mut statuses, Change::Upsert(changed));

        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].provider, "claude");
        assert_eq!(statuses[0].state, "ok");
    }

    #[test]
    fn an_account_nobody_has_seen_goes_on_the_end() {
        let mut statuses = vec![status("claude", "default")];
        apply(&mut statuses, Change::Upsert(status("claude", "work")));
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[1].account, "work");
    }

    #[test]
    fn a_removal_drops_exactly_one_account() {
        let mut statuses = vec![status("claude", "default"), status("claude", "work")];
        apply(
            &mut statuses,
            Change::Remove {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
            },
        );
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].account, "work");
    }

    #[test]
    fn an_order_permutes_providers_and_keeps_accounts_together() {
        let mut statuses = vec![
            status("claude", "default"),
            status("codex", "default"),
            status("claude", "work"),
        ];
        apply(
            &mut statuses,
            Change::Order(vec!["codex".to_owned(), "claude".to_owned()]),
        );
        assert_eq!(statuses[0].provider, "codex");
        assert_eq!(statuses[1].account, "default");
        assert_eq!(statuses[2].account, "work");
    }
}
