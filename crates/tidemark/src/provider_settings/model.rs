use std::collections::HashSet;

use tidemark_types::{
    AuthSelection, ExternalLogin, ProviderDefinition, ProviderOption, ProviderState, ProviderStatus,
};

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

/// Which of an account's two credentials is in force.
///
/// Only ever asked of a provider that has two — see [`ProviderDefinition::external`]. The
/// dialog needs it for one reason: the two halves of the choice are two different screens,
/// and it must draw the one the account is actually on rather than the one the user last
/// looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    /// The login Tidemark performed and holds itself.
    Tidemark,
    /// The login another program on this machine holds.
    Cli,
}

impl AuthSource {
    /// The choice value this travels as, which is also what the setting is written with.
    pub const fn as_value(self) -> &'static str {
        match self {
            Self::Tidemark => "oauth",
            Self::Cli => "cli",
        }
    }

    /// `None` for anything else, including the `auto` a setting nobody has touched holds:
    /// what an untouched setting resolves to is the daemon's answer, published separately.
    pub fn from_value(value: &str) -> Option<Self> {
        [Self::Tidemark, Self::Cli]
            .into_iter()
            .find(|candidate| candidate.as_value() == value)
    }
}

/// The credential this account is on, for a provider that has two.
///
/// Taken from the daemon's own answer rather than worked out here. The setting behind the
/// choice may be unset, and which credential an unset setting means differs per provider —
/// Antigravity's local session wins by default, Claude's and Codex's own login does. Those
/// rules live where the providers do. Falls back to the Tidemark half only when a daemon
/// too old to answer is on the other end, which is the half that has a button to press.
pub fn auth_source(definition: &ProviderDefinition, status: &ProviderStatus) -> Option<AuthSource> {
    definition.external.as_ref()?;
    Some(
        status
            .auth_source
            .as_deref()
            .and_then(AuthSource::from_value)
            .unwrap_or(AuthSource::Tidemark),
    )
}

/// The candidate row an account's published selection points at within one mode.
///
/// Modes that carry no separate candidate — Cursor App is chosen by choosing the mode —
/// claim their own availability row, so highlighting a source needs no special case.
pub fn selected_candidate<'a>(
    selection: &'a AuthSelection,
    mode_value: &'a str,
) -> Option<&'a str> {
    if selection.mode != mode_value {
        return None;
    }
    Some(selection.candidate.as_deref().unwrap_or(mode_value))
}

/// A provider's own settings that remain after its authentication capability took what is
/// the daemon's to write.
///
/// The selector option draws nothing here — the authentication tabs above choose it — and
/// an option carrying no choices has no menu to draw at all: those are identifiers, like
/// which browser was picked, that only daemon source selection may set. Sent as they
/// arrived when this build speaks with no such capability.
pub fn settings_options<'a>(
    declared: impl IntoIterator<Item = &'a ProviderOption>,
    definition: &ProviderDefinition,
) -> Vec<&'a ProviderOption> {
    declared
        .into_iter()
        .filter(|option| {
            !definition
                .browser_auth
                .as_ref()
                .is_some_and(|selector| selector.option == option.name || option.choices.is_empty())
        })
        .collect()
}

/// Concise copy for the configured-provider list, and for the heading of the half in force.
pub fn connection_text(definition: &ProviderDefinition, status: &ProviderStatus) -> String {
    let Some(external) = definition.external.as_ref() else {
        return own_login_text(status).into();
    };
    match auth_source(definition, status) {
        Some(AuthSource::Cli) => match (status.state(), status.external_present) {
            (_, Some(false)) => format!("No {} found", external.label),
            (Some(ProviderState::Pending), _) => format!("Checking for {}…", external.label),
            _ => format!("Using {}", external.label),
        },
        _ => own_login_text(status).into(),
    }
}

/// What is true of the login Tidemark holds itself.
///
/// The unknown case is not folded into "not signed in": a keyring the daemon cannot see
/// into may well hold a login, and offering to make a second one is how an account ends up
/// with two.
fn own_login_text(status: &ProviderStatus) -> &'static str {
    match status.has_credential {
        Some(true) => "Signed in through Tidemark",
        Some(false) => "Not signed in",
        None => "Cannot see the stored login",
    }
}

/// Whether the local login exists, in the two words the CLI half puts beside its name.
///
/// `None` when the daemon did not say. Absence of an answer is drawn as no answer rather
/// than as "not found": the difference is a user being told to run a login they already
/// have.
pub fn external_presence_text(status: &ProviderStatus) -> Option<&'static str> {
    match status.external_present {
        Some(true) => Some("Found"),
        Some(false) => Some("Not found"),
        None => None,
    }
}

/// The sentence that says Tidemark writes to a file it does not own, or nothing when it
/// only reads. Never hidden behind a disclosure: ADR 0001 is the most surprising thing
/// this program does, and the CLI half is where it becomes true.
pub fn write_back_text(external: &ExternalLogin) -> Option<String> {
    external.writes_back.then(|| {
        format!(
            "When this token expires, Tidemark refreshes it and writes the new one back to \
             {}, the way the CLI itself would. Nothing else in that file is touched.",
            external.location
        )
    })
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
        AccountId, AuthMode, AuthSelection, AuthSelector, CredentialKind, ExternalLogin,
        OptionChoice, ProviderDefinition, ProviderId, ProviderOption, ProviderState,
        ProviderStatus,
    };

    use super::{
        AuthSource, NotificationRow, addable, auth_source, connection_text, external_presence_text,
        merge_local_additions, notification_rows, selected_candidate, settings_options,
        write_back_text,
    };

    fn definition(provider: &str, title: &str) -> ProviderDefinition {
        ProviderDefinition {
            provider: provider.into(),
            title: title.into(),
            credential: CredentialKind::Key.as_wire().into(),
            credential_hint: "Create a key in the provider dashboard.".into(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
        }
    }

    fn oauth_definition(provider: &str, label: Option<&str>) -> ProviderDefinition {
        let external = label.map(|label| ExternalLogin {
            option: "source".into(),
            label: label.into(),
            location: "~/.somewhere/creds.json".into(),
            command: "somecli login".into(),
            writes_back: true,
        });
        let options = external
            .iter()
            .map(|external| ProviderOption {
                name: external.option.clone(),
                title: "Credential".into(),
                description: None,
                value: "auto".into(),
                choices: vec![
                    OptionChoice {
                        value: "oauth".into(),
                        title: "Tidemark login".into(),
                    },
                    OptionChoice {
                        value: "cli".into(),
                        title: external.label.clone(),
                    },
                ],
            })
            .collect();
        ProviderDefinition {
            provider: provider.into(),
            title: provider.into(),
            credential: CredentialKind::OAuth.as_wire().into(),
            credential_hint: "Sign in through a browser.".into(),
            external,
            browser_auth: None,
            options,
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

    /// The daemon's answer, as it arrives: which half is in force, and whether the local
    /// login is there.
    fn on(mut status: ProviderStatus, source: AuthSource, present: Option<bool>) -> ProviderStatus {
        status.auth_source = Some(source.as_value().to_owned());
        status.external_present = present;
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
    fn the_cli_half_says_which_local_login_is_being_read() {
        let definition = oauth_definition("antigravity", Some("agy session"));
        let cli = |state, present| {
            connection_text(
                &definition,
                &on(
                    status("antigravity", state, Some(false)),
                    AuthSource::Cli,
                    present,
                ),
            )
        };
        assert_eq!(
            cli(ProviderState::Pending, Some(true)),
            "Checking for agy session…"
        );
        assert_eq!(cli(ProviderState::Ok, Some(true)), "Using agy session");
        // Not "Not signed in": the account is on the local half and there is nothing on it.
        // The remedy is a command to run, not a button to press, and the copy must not
        // point at the wrong one.
        assert_eq!(
            cli(ProviderState::NoCredential, Some(false)),
            "No agy session found"
        );
    }

    #[test]
    fn the_tidemark_half_describes_only_the_login_tidemark_holds() {
        let definition = oauth_definition("antigravity", Some("agy session"));
        let own = |has_credential| {
            connection_text(
                &definition,
                &on(
                    status("antigravity", ProviderState::Ok, has_credential),
                    AuthSource::Tidemark,
                    Some(true),
                ),
            )
        };
        // A healthy local session is running in every one of these, and says nothing about
        // the half being drawn.
        assert_eq!(own(Some(true)), "Signed in through Tidemark");
        assert_eq!(own(Some(false)), "Not signed in");
        assert_eq!(own(None), "Cannot see the stored login");
    }

    #[test]
    fn a_provider_with_one_credential_is_never_put_on_a_half() {
        let definition = definition("zai", "Z.ai");
        let mut status = status("zai", ProviderState::Ok, Some(true));
        // Even if a daemon sent one, there is no second credential to switch to.
        status.auth_source = Some("cli".into());
        assert_eq!(auth_source(&definition, &status), None);
        assert_eq!(
            connection_text(&definition, &status),
            "Signed in through Tidemark"
        );
    }

    #[test]
    fn a_daemon_that_does_not_say_which_half_leaves_the_actionable_one_showing() {
        let definition = oauth_definition("claude", Some("Claude Code login"));
        let status = status("claude", ProviderState::Pending, Some(false));
        assert_eq!(status.auth_source, None);
        assert_eq!(
            auth_source(&definition, &status),
            Some(AuthSource::Tidemark)
        );
    }

    #[test]
    fn an_unanswered_presence_is_drawn_as_no_answer() {
        let mut status = status("claude", ProviderState::Pending, Some(false));
        assert_eq!(external_presence_text(&status), None);
        status.external_present = Some(true);
        assert_eq!(external_presence_text(&status), Some("Found"));
        status.external_present = Some(false);
        assert_eq!(external_presence_text(&status), Some("Not found"));
    }

    #[test]
    fn only_a_source_tidemark_writes_to_admits_to_being_written_to() {
        let mut external = ExternalLogin {
            option: "source".into(),
            label: "Claude Code login".into(),
            location: "~/.claude/.credentials.json".into(),
            command: "claude".into(),
            writes_back: true,
        };
        let written = write_back_text(&external).expect("a written source says so");
        assert!(written.contains("~/.claude/.credentials.json"), "{written}");
        external.writes_back = false;
        assert_eq!(write_back_text(&external), None);
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
                subtitle: None,
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
        let status = windowed(
            &[("w18000", "5 hours"), ("w604800", "Weekly")],
            &["w604800"],
        );
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

    /// One published choice, as every menu-bearing option carries it.
    fn choice(value: &str, title: &str) -> OptionChoice {
        OptionChoice {
            value: value.into(),
            title: title.into(),
        }
    }

    /// What the daemon publishes for a provider whose login is one explicit local source:
    /// the selector's own option plus the identifier settings only the daemon may write.
    fn cursor_like_definition() -> ProviderDefinition {
        ProviderDefinition {
            provider: "cursor".into(),
            title: "Cursor".into(),
            credential: CredentialKind::None.as_wire().into(),
            credential_hint: String::new(),
            external: None,
            browser_auth: Some(AuthSelector {
                option: "auth-source".into(),
                modes: vec![
                    AuthMode {
                        value: "cursor-app".into(),
                        title: "Cursor App".into(),
                    },
                    AuthMode {
                        value: "browser".into(),
                        title: "Browser".into(),
                    },
                ],
            }),
            options: vec![
                ProviderOption {
                    name: "auth-source".into(),
                    title: "Authentication source".into(),
                    description: None,
                    value: String::new(),
                    choices: vec![
                        choice("cursor-app", "Cursor App"),
                        choice("browser", "Browser"),
                    ],
                },
                ProviderOption {
                    name: "auth-browser".into(),
                    title: "Browser".into(),
                    description: None,
                    value: String::new(),
                    choices: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn the_source_selector_owns_its_settings_instead_of_the_menu_below() {
        // The selector option itself, and every choiceless identifier the daemon writes
        // through source selection, would each draw a menu of nothing — the tabs above are
        // where those are chosen.
        let definition = cursor_like_definition();
        assert!(settings_options(definition.options.iter(), &definition).is_empty());
    }

    #[test]
    fn ordinary_provider_choices_keep_their_place_under_the_tabs() {
        let mut definition = definition("zai", "Z.ai");
        definition.options = vec![ProviderOption {
            name: "model".into(),
            title: "Model".into(),
            description: None,
            value: "auto".into(),
            choices: vec![choice("auto", "Automatic")],
        }];
        let names: Vec<&str> = settings_options(definition.options.iter(), &definition)
            .iter()
            .map(|option| option.name.as_str())
            .collect();
        assert_eq!(names, ["model"]);
    }

    #[test]
    fn a_published_selection_names_the_row_it_claims_inside_its_mode() {
        let nested = AuthSelection {
            mode: "browser".into(),
            candidate: Some("zen/profile-a".into()),
        };
        assert_eq!(
            selected_candidate(&nested, "browser"),
            Some("zen/profile-a")
        );
        assert_eq!(selected_candidate(&nested, "cursor-app"), None);
    }

    #[test]
    fn a_mode_without_a_separate_candidate_claims_its_own_row() {
        let direct = AuthSelection {
            mode: "cursor-app".into(),
            candidate: None,
        };
        assert_eq!(
            selected_candidate(&direct, "cursor-app"),
            Some("cursor-app")
        );
    }
}
