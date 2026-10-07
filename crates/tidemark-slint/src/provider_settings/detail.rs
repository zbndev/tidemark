//! One account's page: what its authentication, settings and notifications groups show.
//!
//! Built fresh from the daemon's status every time, with two things layered over it that
//! the daemon does not know yet: a browser login in progress, and the controls a click has
//! moved before its write has landed. A refused write takes its layer away, which puts the
//! control back where the daemon says it is.

use std::collections::HashMap;

use tidemark_types::{
    CredentialKind, ExternalLogin, ProviderDefinition, ProviderOption, ProviderStatus, Remedy,
};

use super::DEFAULT_ACCOUNT;
use super::browser_auth::{BrowserAuth, Line};
use super::model::{self, AuthSource, NotificationRow};
use crate::format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Key,
    /// A login Tidemark performs, and maybe a local one to read instead.
    Login,
    /// A credential this client can only describe.
    Described,
    /// One explicitly chosen local source.
    LocalSource,
}

/// The CLI half, for a provider whose credential can come from another program here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLogin {
    pub label: String,
    pub location: String,
    /// Whether the daemon found it; `None` when it did not say.
    pub present: Option<bool>,
    pub presence_text: String,
    /// What to run to create one; empty when there is no single command to name.
    pub command: String,
    /// That Tidemark writes to a file it does not own; empty when it only reads.
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionView {
    pub name: String,
    pub title: String,
    pub description: String,
    pub choices: Vec<String>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub title: String,
    pub subtitle: String,
    pub can_rename: bool,
    /// `None` for a real credential-free service, which exposes no authentication controls.
    pub auth: Option<Auth>,
    pub description: String,
    pub stored: bool,
    pub choices: Vec<String>,
    pub choice: usize,
    pub tidemark_half: bool,
    pub sign_in_subtitle: String,
    pub waiting: bool,
    pub login_url: String,
    pub local: Option<LocalLogin>,
    pub connection: String,
    pub modes: Vec<String>,
    pub mode: usize,
    pub pasting: bool,
    pub lines: Vec<Line>,
    pub options: Vec<OptionView>,
    pub notifications: Vec<NotificationRow>,
}

/// What this page holds that the daemon has not published.
#[derive(Debug, Default)]
pub struct Local<'a> {
    /// The login address while a browser login is in progress.
    pub login_url: Option<&'a str>,
    /// Controls a click has moved ahead of the write, by [`override_key`].
    pub overrides: Option<&'a HashMap<String, String>>,
    pub browser: Option<&'a BrowserAuth>,
}

pub const SOURCE: &str = "source";

/// The key a moved control is remembered under until its write lands or is refused.
pub fn override_key(kind: &str, name: &str) -> String {
    format!("{kind}:{name}")
}

pub fn view(definition: &ProviderDefinition, status: &ProviderStatus, local: &Local) -> Detail {
    let held = |key: &str| local.overrides.and_then(|overrides| overrides.get(key));
    let stored = status.has_credential == Some(true);
    let waiting = local.login_url.is_some();
    let auth = auth(definition);

    let sources = source_choices(definition);
    let source = model::auth_source(definition, status).map(|published| {
        held(SOURCE)
            .and_then(|value| AuthSource::from_value(value))
            .unwrap_or(published)
    });
    let choices = sources
        .as_ref()
        .map(|sources| sources.iter().map(|(_, title)| title.clone()).collect())
        .unwrap_or_default();
    let choice = sources
        .as_ref()
        .and_then(|sources| sources.iter().position(|(value, _)| Some(*value) == source))
        .unwrap_or(0);

    let options = settings_options(definition, status)
        .into_iter()
        .map(|option| {
            let value = held(&override_key("option", &option.name)).unwrap_or(&option.value);
            OptionView {
                name: option.name.clone(),
                title: option.title.clone(),
                description: option.description.clone().unwrap_or_default(),
                choices: option
                    .choices
                    .iter()
                    .map(|choice| choice.title.clone())
                    .collect(),
                selected: option
                    .choices
                    .iter()
                    .position(|choice| choice.value == *value)
                    .unwrap_or(0),
            }
        })
        .collect();

    let notifications = model::notification_rows(status)
        .into_iter()
        .map(|mut row| {
            if let Some(held) = held(&override_key("notify", &row.key)) {
                row.enabled = held == "true";
            }
            row
        })
        .collect();

    Detail {
        title: definition.title.clone(),
        // A page a second account lands on must say which account it is.
        subtitle: if status.account == DEFAULT_ACCOUNT {
            String::new()
        } else {
            status
                .account_label
                .clone()
                .unwrap_or_else(|| status.account.clone())
        },
        // The default account is the provider's structural first one; the daemon will not
        // move it, so the pen stays off its page.
        can_rename: status.account != DEFAULT_ACCOUNT,
        auth,
        description: describe(definition, status),
        stored,
        choices,
        choice,
        tidemark_half: source.is_none_or(|source| source == AuthSource::Tidemark),
        sign_in_subtitle: if waiting {
            "Waiting for your browser…".into()
        } else {
            model::connection_text(definition, status)
        },
        waiting,
        login_url: local.login_url.unwrap_or_default().to_owned(),
        local: definition
            .external
            .as_ref()
            .map(|external| local_login(external, status)),
        connection: model::connection_text(definition, status),
        modes: local.browser.map(BrowserAuth::titles).unwrap_or_default(),
        mode: local.browser.map_or(0, BrowserAuth::mode_index),
        pasting: local.browser.is_some_and(BrowserAuth::pasting),
        lines: local.browser.map(BrowserAuth::lines).unwrap_or_default(),
        options,
        notifications,
    }
}

fn auth(definition: &ProviderDefinition) -> Option<Auth> {
    if definition.browser_auth.is_some() {
        return Some(Auth::LocalSource);
    }
    match definition.credential_kind() {
        Some(CredentialKind::Key) => Some(Auth::Key),
        Some(CredentialKind::OAuth) => Some(Auth::Login),
        Some(CredentialKind::None) => None,
        Some(CredentialKind::External) | None => Some(Auth::Described),
    }
}

/// The pill's two halves, in the provider's own words — `Claude Code login`, not a slug
/// this crate would have to be taught. A choice this build has no screen for is refused
/// outright: half a pill is worse than none, because the half that is drawn looks like
/// the whole choice.
pub fn source_choices(definition: &ProviderDefinition) -> Option<Vec<(AuthSource, String)>> {
    let external = definition.external.as_ref()?;
    let option = definition
        .options
        .iter()
        .find(|option| option.name == external.option)?;
    let toggles: Vec<(AuthSource, String)> = option
        .choices
        .iter()
        .filter_map(|choice| Some((AuthSource::from_value(&choice.value)?, choice.title.clone())))
        .collect();
    (toggles.len() == option.choices.len() && toggles.len() >= 2).then_some(toggles)
}

/// The provider's own settings — every one except the credential choice, which the pill
/// draws, and the local-source identifiers, which the source tabs own. Left in the list
/// either would appear twice.
pub fn settings_options<'a>(
    definition: &'a ProviderDefinition,
    status: &'a ProviderStatus,
) -> Vec<&'a ProviderOption> {
    let declared = if status.options.is_empty() {
        &definition.options
    } else {
        &status.options
    };
    let auth_option = definition.auth_option();
    model::settings_options(
        declared
            .iter()
            .filter(|option| Some(option.name.as_str()) != auth_option),
        definition,
    )
}

/// Where the local login is, whether it is there, what to run if it is not, and — for the
/// providers this is true of — that Tidemark writes the refreshed token back into a file
/// it does not own. ADR 0001, stated where the choice is made rather than behind a
/// disclosure.
fn local_login(external: &ExternalLogin, status: &ProviderStatus) -> LocalLogin {
    LocalLogin {
        label: external.label.clone(),
        location: external.location.clone(),
        present: status.external_present,
        presence_text: model::external_presence_text(status)
            .unwrap_or_default()
            .to_owned(),
        command: external.command.clone(),
        note: model::write_back_text(external).unwrap_or_default(),
    }
}

/// The sentence under the group's heading: what is wrong, and where the credential comes
/// from.
///
/// The second half is dropped for a provider whose two credentials are drawn as a pill:
/// its hint is the pill spelled out in prose directly above the pill.
fn describe(definition: &ProviderDefinition, status: &ProviderStatus) -> String {
    let hint = if definition.external.is_some() {
        ""
    } else {
        definition.credential_hint.as_str()
    };
    match format::chip(status) {
        Some(chip)
            if status
                .state()
                .is_none_or(|state| state.remedy() != Remedy::Nothing) =>
        {
            let detail = status.message.clone().unwrap_or(chip.text);
            if hint.is_empty() {
                detail
            } else {
                format!("{detail} — {hint}")
            }
        }
        _ => hint.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, OptionChoice, ProviderId, ProviderState, WindowStatus};

    use super::*;

    fn definition(kind: CredentialKind) -> ProviderDefinition {
        ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: kind.as_wire().into(),
            credential_hint: "Z.ai dashboard → API keys.".into(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
            plugin: None,
        }
    }

    fn status() -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default())
    }

    fn choice(value: &str, title: &str) -> OptionChoice {
        OptionChoice {
            value: value.into(),
            title: title.into(),
        }
    }

    fn claude() -> ProviderDefinition {
        let mut definition = definition(CredentialKind::OAuth);
        definition.provider = "claude".into();
        definition.external = Some(ExternalLogin {
            option: "source".into(),
            label: "Claude Code login".into(),
            location: "~/.claude/.credentials.json".into(),
            command: "claude".into(),
            writes_back: true,
        });
        definition.options = vec![ProviderOption {
            name: "source".into(),
            title: "Credential".into(),
            description: None,
            value: "auto".into(),
            choices: vec![
                choice("oauth", "Tidemark login"),
                choice("cli", "Claude Code login"),
            ],
        }];
        definition
    }

    #[test]
    fn a_healthy_account_is_described_by_where_its_credential_comes_from() {
        let definition = definition(CredentialKind::Key);
        let mut healthy = status();
        healthy.set_state(ProviderState::Ok, None);
        assert_eq!(describe(&definition, &healthy), definition.credential_hint);
    }

    #[test]
    fn an_account_the_user_must_fix_leads_with_what_is_wrong() {
        let definition = definition(CredentialKind::Key);
        let mut rejected = status();
        rejected.set_state(
            ProviderState::CredentialRejected,
            Some("the credential was rejected (HTTP 401)".into()),
        );
        assert_eq!(
            describe(&definition, &rejected),
            "the credential was rejected (HTTP 401) — Z.ai dashboard → API keys."
        );
    }

    #[test]
    fn an_account_that_is_merely_waiting_does_not_shout_about_it() {
        let definition = definition(CredentialKind::Key);
        assert_eq!(describe(&definition, &status()), definition.credential_hint);
    }

    #[test]
    fn a_state_this_build_does_not_know_is_still_reported() {
        let definition = definition(CredentialKind::OAuth);
        let mut unknown = status();
        unknown.state = "quota-frozen".into();
        assert!(describe(&definition, &unknown).starts_with("quota-frozen"));
    }

    #[test]
    fn a_credential_free_provider_shows_no_authentication() {
        let view = view(
            &definition(CredentialKind::None),
            &status(),
            &Local::default(),
        );
        assert_eq!(view.auth, None);
    }

    #[test]
    fn the_pill_is_drawn_from_the_providers_own_words_and_the_daemons_answer() {
        let definition = claude();
        let mut status = status();
        status.auth_source = Some("cli".into());
        let view = view(&definition, &status, &Local::default());
        assert_eq!(view.choices, ["Tidemark login", "Claude Code login"]);
        assert_eq!(view.choice, 1);
        assert!(!view.tidemark_half);
        // The pill says which credential; the heading's sentence does not say it again.
        assert_eq!(view.description, "");
        assert!(
            view.local
                .is_some_and(|local| local.note.contains("~/.claude/.credentials.json"))
        );
    }

    #[test]
    fn a_choice_this_build_has_no_screen_for_draws_no_pill() {
        let mut definition = claude();
        definition.options[0]
            .choices
            .push(choice("token", "Pasted token"));
        assert_eq!(source_choices(&definition), None);
    }

    #[test]
    fn a_moved_control_shows_where_it_was_moved_until_the_layer_is_taken_away() {
        let mut definition = claude();
        definition.options.push(ProviderOption {
            name: "region".into(),
            title: "Region".into(),
            description: None,
            value: "global".into(),
            choices: vec![choice("global", "Global"), choice("cn", "China")],
        });
        let mut status = status();
        status.windows = vec![WindowStatus {
            key: "w18000".into(),
            title: "5 hours".into(),
            subtitle: None,
            used_percent: 0.0,
            resets_at: None,
            length_secs: None,
            blocked_by: None,
        }];
        let overrides = HashMap::from([
            (SOURCE.to_owned(), "cli".to_owned()),
            (override_key("option", "region"), "cn".to_owned()),
            (override_key("notify", "w18000"), "true".to_owned()),
        ]);
        let moved = view(
            &definition,
            &status,
            &Local {
                overrides: Some(&overrides),
                ..Local::default()
            },
        );
        assert_eq!(moved.choice, 1);
        // The credential choice is the pill's, so it is not a menu as well.
        assert_eq!(moved.options.len(), 1);
        assert_eq!(moved.options[0].selected, 1);
        assert!(moved.notifications[0].enabled);

        let settled = view(&definition, &status, &Local::default());
        assert_eq!(settled.choice, 0);
        assert_eq!(settled.options[0].selected, 0);
        assert!(!settled.notifications[0].enabled);
    }

    #[test]
    fn a_login_in_progress_says_so_and_carries_its_address() {
        let view = view(
            &claude(),
            &status(),
            &Local {
                login_url: Some("https://claude.ai/oauth"),
                ..Local::default()
            },
        );
        assert!(view.waiting);
        assert_eq!(view.sign_in_subtitle, "Waiting for your browser…");
        assert_eq!(view.login_url, "https://claude.ai/oauth");
    }

    #[test]
    fn only_a_further_account_can_be_renamed_and_names_itself() {
        let mut work = status();
        work.account = "work".into();
        work.account_label = Some("Work".into());
        let view = view(&definition(CredentialKind::Key), &work, &Local::default());
        assert!(view.can_rename);
        assert_eq!(view.subtitle, "Work");
        let default = super::view(
            &definition(CredentialKind::Key),
            &status(),
            &Local::default(),
        );
        assert!(!default.can_rename);
        assert_eq!(default.subtitle, "");
    }
}
