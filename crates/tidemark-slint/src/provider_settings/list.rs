//! The configured list of one tab and the catalog picker, as rows to draw.

use std::collections::HashSet;

use tidemark_types::{ProviderDefinition, ProviderStatus, provider_label};

use super::{model, multi_account_capable, opens_detail_after_add};

/// What a row of the configured list is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A provider, drawn with the state of its first account.
    Provider,
    /// A further account, under its provider.
    Account,
    /// An installed plugin nobody has configured: waiting for its first account.
    Unconfigured,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub provider: String,
    pub account: String,
    pub title: String,
    pub subtitle: String,
    pub can_add: bool,
    pub can_edit: bool,
    pub nested: bool,
    pub expanded: bool,
}

/// A provider and its accounts, in the order the list draws them.
#[derive(Debug, PartialEq)]
struct ProviderGroup<'a> {
    provider: &'a str,
    /// In configured order. The first is drawn on the provider's own row, so a provider
    /// with one account looks exactly as it always has.
    accounts: Vec<&'a ProviderStatus>,
}

/// Gathers a flat status list into providers, in the order each provider first appears.
fn group(statuses: &[ProviderStatus]) -> Vec<ProviderGroup<'_>> {
    let mut groups: Vec<ProviderGroup<'_>> = Vec::new();
    for status in statuses {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.provider == status.provider)
        {
            group.accounts.push(status);
        } else {
            groups.push(ProviderGroup {
                provider: &status.provider,
                accounts: vec![status],
            });
        }
    }
    groups
}

/// The rows of one tab: its configured providers, each followed by its further accounts
/// while expanded, then the installed definitions no account uses.
pub fn rows(
    definitions: &[ProviderDefinition],
    statuses: &[ProviderStatus],
    unconfigured: &[ProviderDefinition],
    collapsed: &HashSet<String>,
    is_waiting: &dyn Fn(&str, &str) -> bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for group in group(statuses) {
        let definition = definitions
            .iter()
            .find(|definition| definition.provider == group.provider);
        // A provider with a source selector to pick at counts as editable however it logs
        // in: which local login to read is configuration, not something polling works out.
        let can_edit = definition.is_some_and(opens_detail_after_add);
        let first = group.accounts[0];
        let nested = group.accounts.len() > 1;
        let expanded = !collapsed.contains(group.provider);
        rows.push(Row {
            kind: Kind::Provider,
            provider: first.provider.clone(),
            account: first.account.clone(),
            title: definition.map_or_else(
                || provider_label(&first.provider),
                |definition| definition.title.clone(),
            ),
            subtitle: status_text(
                definition,
                first,
                is_waiting(&first.provider, &first.account),
            ),
            can_add: definition.is_some_and(multi_account_capable),
            can_edit,
            nested,
            expanded,
        });
        if !expanded {
            continue;
        }
        for status in &group.accounts[1..] {
            rows.push(Row {
                kind: Kind::Account,
                provider: status.provider.clone(),
                account: status.account.clone(),
                // The daemon publishes the label, and derives it from the id until a
                // rename says otherwise.
                title: status
                    .account_label
                    .clone()
                    .unwrap_or_else(|| status.account.clone()),
                subtitle: status_text(
                    definition,
                    status,
                    is_waiting(&status.provider, &status.account),
                ),
                can_add: false,
                can_edit,
                nested: false,
                expanded: false,
            });
        }
    }
    // The id is the subtitle on purpose: it is what the provider's file is named by and
    // what every refusal the daemon sends will say.
    rows.extend(unconfigured.iter().map(|definition| Row {
        kind: Kind::Unconfigured,
        provider: definition.provider.clone(),
        account: String::new(),
        title: definition.title.clone(),
        subtitle: definition.provider.clone(),
        can_add: true,
        can_edit: false,
        nested: false,
        expanded: false,
    }));
    rows
}

/// The one line of state under a row's name.
fn status_text(
    definition: Option<&ProviderDefinition>,
    status: &ProviderStatus,
    waiting: bool,
) -> String {
    if waiting {
        "Waiting for your browser…".into()
    } else if let Some(definition) = definition {
        model::connection_text(definition, status)
    } else {
        status
            .message
            .clone()
            .unwrap_or_else(|| status.state.clone())
    }
}

#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, CredentialKind, ProviderId};

    use super::*;

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account))
    }

    fn definition(provider: &str, credential: CredentialKind) -> ProviderDefinition {
        ProviderDefinition {
            provider: provider.into(),
            title: provider.to_uppercase(),
            credential: credential.as_wire().into(),
            credential_hint: String::new(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
            plugin: None,
        }
    }

    fn keys(rows: &[Row]) -> Vec<(Kind, &str, &str)> {
        rows.iter()
            .map(|row| (row.kind, row.provider.as_str(), row.account.as_str()))
            .collect()
    }

    #[test]
    fn statuses_of_one_provider_are_gathered_under_it_in_first_seen_order() {
        // A status for a second account can arrive after another provider's, from a
        // locally added pending row as much as from the daemon; the provider it belongs
        // to is where it lands.
        let rows = rows(
            &[],
            &[
                status("kimi", "default"),
                status("zai", "default"),
                status("kimi", "work"),
            ],
            &[],
            &HashSet::new(),
            &|_, _| false,
        );
        assert_eq!(
            keys(&rows),
            [
                (Kind::Provider, "kimi", "default"),
                (Kind::Account, "kimi", "work"),
                (Kind::Provider, "zai", "default"),
            ]
        );
        assert!(rows[0].nested);
        assert!(!rows[2].nested);
    }

    #[test]
    fn a_collapsed_provider_hides_its_further_accounts() {
        let rows = rows(
            &[],
            &[status("kimi", "default"), status("kimi", "work")],
            &[],
            &HashSet::from(["kimi".to_owned()]),
            &|_, _| false,
        );
        assert_eq!(keys(&rows), [(Kind::Provider, "kimi", "default")]);
        assert!(rows[0].nested && !rows[0].expanded);
    }

    #[test]
    fn the_row_controls_follow_what_the_credential_can_hold() {
        let definitions = [
            definition("zai", CredentialKind::Key),
            definition("local", CredentialKind::None),
        ];
        let rows = rows(
            &definitions,
            &[status("zai", "default"), status("local", "default")],
            &[],
            &HashSet::new(),
            &|_, _| false,
        );
        assert!(rows[0].can_add && rows[0].can_edit);
        assert!(!rows[1].can_add && !rows[1].can_edit);
        assert_eq!(rows[0].title, "ZAI");
    }

    #[test]
    fn a_login_in_progress_is_what_the_row_says() {
        let rows = rows(
            &[definition("claude", CredentialKind::OAuth)],
            &[status("claude", "default")],
            &[],
            &HashSet::new(),
            &|provider, _| provider == "claude",
        );
        assert_eq!(rows[0].subtitle, "Waiting for your browser…");
    }

    #[test]
    fn unconfigured_plugins_follow_the_configured_rows_and_show_their_id() {
        let plugin = definition("com.acme.quota", CredentialKind::Key);
        let rows = rows(
            &[],
            &[status("zai", "default")],
            &[plugin],
            &HashSet::new(),
            &|_, _| false,
        );
        assert_eq!(rows[1].kind, Kind::Unconfigured);
        assert_eq!(rows[1].subtitle, "com.acme.quota");
        assert!(rows[1].can_add);
    }

    #[test]
    fn no_statuses_means_no_rows() {
        assert!(rows(&[], &[], &[], &HashSet::new(), &|_, _| false).is_empty());
    }
}
