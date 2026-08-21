use std::collections::HashSet;

use tidemark_types::{ProviderDefinition, ProviderState, ProviderStatus};

/// Catalog entries which have not already been configured and match the query.
pub fn addable<'a>(
    definitions: &'a [ProviderDefinition],
    statuses: &[ProviderStatus],
    query: &str,
) -> Vec<&'a ProviderDefinition> {
    let query = query.trim().to_lowercase();
    definitions
        .iter()
        .filter(|definition| {
            !statuses
                .iter()
                .any(|status| status.provider == definition.provider)
        })
        .filter(|definition| {
            definition.provider.to_lowercase().contains(&query)
                || definition.title.to_lowercase().contains(&query)
        })
        .collect()
}

/// Concise copy for the configured-provider list.
pub fn connection_text(definition: &ProviderDefinition, status: &ProviderStatus) -> String {
    if status.has_credential == Some(true) {
        return "Signed in through Tidemark".into();
    }

    match (status.state(), definition.external_fallback.as_deref()) {
        (Some(ProviderState::Pending), Some(fallback)) => {
            format!("Checking for {fallback}…")
        }
        (Some(ProviderState::Ok), Some(fallback)) => format!("Using {fallback}"),
        _ => "Not signed in".into(),
    }
}

/// One line of the notifications group: a window the account reports, and whether the user
/// asked to hear about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationRow {
    /// The window key, which is what the daemon is told to switch.
    pub key: String,
    /// The window's own title, in the provider's terms.
    pub title: String,
    /// Whether notifications for it are on.
    pub enabled: bool,
}

/// The switches to draw for one account, in the order the provider reported its windows.
///
/// Driven by the windows rather than by the opt-in list: what the interface can offer is
/// what the account currently reports, and an account nobody has polled yet reports nothing
/// — a normal state on the first seconds of a daemon's life, and one the group hides for.
pub fn notification_rows(status: &ProviderStatus) -> Vec<NotificationRow> {
    status
        .windows
        .iter()
        .map(|window| NotificationRow {
            key: window.key.clone(),
            title: window.title.clone(),
            enabled: status.notify.iter().any(|key| key == &window.key),
        })
        .collect()
}

/// Keeps successful local additions visible until the daemon publishes their first status.
pub fn merge_local_additions(
    incoming: &[ProviderStatus],
    current: &[ProviderStatus],
    local_additions: &HashSet<String>,
) -> Vec<ProviderStatus> {
    let mut merged = incoming.to_vec();
    for status in current {
        let still_local = local_additions.contains(&status.provider)
            && !incoming
                .iter()
                .any(|incoming| incoming.provider == status.provider);
        let already_present = merged
            .iter()
            .any(|merged| merged.provider == status.provider && merged.account == status.account);
        if still_local && !already_present {
            merged.push(status.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tidemark_types::{
        AccountId, CredentialKind, ProviderDefinition, ProviderId, ProviderState, ProviderStatus,
    };

    use super::{NotificationRow, addable, connection_text, merge_local_additions, notification_rows};

    fn definition(provider: &str, title: &str) -> ProviderDefinition {
        ProviderDefinition {
            provider: provider.into(),
            title: title.into(),
            credential: CredentialKind::Key.as_wire().into(),
            credential_hint: "Create a key in the provider dashboard.".into(),
            external_fallback: None,
            options: Vec::new(),
        }
    }

    fn oauth_definition(provider: &str, fallback: Option<&str>) -> ProviderDefinition {
        ProviderDefinition {
            provider: provider.into(),
            title: provider.into(),
            credential: CredentialKind::OAuth.as_wire().into(),
            credential_hint: "Sign in through a browser.".into(),
            external_fallback: fallback.map(str::to_owned),
            options: Vec::new(),
        }
    }

    fn status(
        provider: &str,
        state: ProviderState,
        has_credential: Option<bool>,
    ) -> ProviderStatus {
        let mut status =
            ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new("default"));
        status.set_state(state, None);
        status.has_credential = has_credential;
        status
    }

    #[test]
    fn search_is_case_insensitive_and_excludes_added_providers() {
        let definitions = vec![definition("claude", "Claude"), definition("codex", "Codex")];
        let statuses = vec![status("claude", ProviderState::Ok, Some(false))];
        let matches = addable(&definitions, &statuses, "CODE");
        assert_eq!(
            matches
                .iter()
                .map(|item| item.provider.as_str())
                .collect::<Vec<_>>(),
            ["codex"]
        );
        assert!(addable(&definitions, &statuses, "claude").is_empty());
    }

    #[test]
    fn oauth_fallback_copy_never_claims_an_unverified_session() {
        let definition = oauth_definition("antigravity", Some("agy session"));
        assert_eq!(
            connection_text(
                &definition,
                &status("antigravity", ProviderState::Pending, Some(false))
            ),
            "Checking for agy session…"
        );
        assert_eq!(
            connection_text(
                &definition,
                &status("antigravity", ProviderState::Ok, Some(false))
            ),
            "Using agy session"
        );
        assert_eq!(
            connection_text(
                &definition,
                &status("antigravity", ProviderState::NoCredential, Some(false))
            ),
            "Not signed in"
        );
        assert_eq!(
            connection_text(
                &definition,
                &status("antigravity", ProviderState::Ok, Some(true))
            ),
            "Signed in through Tidemark"
        );
    }

    #[test]
    fn an_unacknowledged_local_add_survives_an_unrelated_status_update() {
        let current = vec![status("codex", ProviderState::Pending, Some(false))];
        let incoming = vec![status("claude", ProviderState::Ok, Some(false))];
        let local = HashSet::from(["codex".to_owned()]);

        let merged = merge_local_additions(&incoming, &current, &local);

        assert_eq!(
            merged
                .iter()
                .map(|status| status.provider.as_str())
                .collect::<Vec<_>>(),
            ["claude", "codex"]
        );
    }

    fn windowed(keys: &[(&str, &str)], notify: &[&str]) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("claude"), &AccountId::default());
        status.windows = keys
            .iter()
            .map(|(key, title)| tidemark_types::WindowStatus {
                key: (*key).into(),
                title: (*title).into(),
                used_percent: 0.0,
                resets_at: None,
                length_secs: None,
            })
            .collect();
        status.notify = notify.iter().map(|key| (*key).to_string()).collect();
        status
    }

    #[test]
    fn a_switch_is_offered_for_every_window_the_account_reports() {
        let status = windowed(&[("w18000", "5 hours"), ("w604800", "Weekly")], &["w604800"]);
        assert_eq!(
            notification_rows(&status),
            vec![
                NotificationRow {
                    key: "w18000".into(),
                    title: "5 hours".into(),
                    enabled: false
                },
                NotificationRow {
                    key: "w604800".into(),
                    title: "Weekly".into(),
                    enabled: true
                },
            ]
        );
    }

    /// The window set is whatever arrived. An opt-in for a window the provider has stopped
    /// reporting stays in the settings file — it is not ours to delete — but there is
    /// nothing to draw a switch against.
    #[test]
    fn an_opt_in_for_a_window_nobody_reports_draws_no_row() {
        let status = windowed(&[("w18000", "5 hours")], &["w18000", "w604800"]);
        assert_eq!(notification_rows(&status).len(), 1);
    }

    #[test]
    fn an_account_that_has_never_been_polled_offers_nothing_to_switch() {
        let status = ProviderStatus::pending(&ProviderId::new("claude"), &AccountId::default());
        assert!(notification_rows(&status).is_empty());
    }
}
