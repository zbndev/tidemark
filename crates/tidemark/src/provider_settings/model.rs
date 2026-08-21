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

    use super::{addable, connection_text, merge_local_additions};

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
}
