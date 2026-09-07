//! Turning what the daemon published into what a caller asked for.

pub mod json;
pub mod text;
pub mod waybar;

use serde_json::Value;
use tidemark_types::{ProviderStatus, Window};

/// The values without their D-Bus envelope.
///
/// `tidemark-types` derives `SerializeDict`, which exists to encode `a{sv}`: every value
/// carries its signature, so under `serde_json` `"provider"` arrives as
/// `{"signature": "s", "value": "claude"}` and `details` nests three of those. A plugin
/// wants the payload, and the alternative — a second serialization in `tidemark-types` —
/// would mean two models of one wire shape and two sets of key names to keep in step.
/// Stripping is safe because no published structure has `signature` and `value` as its
/// only two fields; the envelope is the only thing that shape means.
///
/// It lives here rather than in [`json`] because `watch`'s event lines publish the same
/// wire types and need the same peeling: one rule, one place.
pub fn payload(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            if map.len() == 2
                && map.contains_key("signature")
                && let Some(inner) = map.remove("value")
            {
                return payload(inner);
            }
            Value::Object(map.into_iter().map(|(key, v)| (key, payload(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(payload).collect()),
        other => other,
    }
}

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
