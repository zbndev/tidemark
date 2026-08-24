//! Which accounts this build watches, and how each of them is signed in to.
//!
//! Registration is a spec in `keyed::CATALOG`, for every single-request
//! key-authenticated provider. The three OAuth providers — Antigravity, Claude and Codex
//! — are registered by hand here, because each of them acquires its credential its own
//! way; so are the hand-written key providers below, whose fetch is more than one
//! request or whose build refuses a required option's value, neither of which a
//! `keyed::Spec` can say. Nothing else in the daemon names a key-authenticated
//! provider.
//!
//! An entry says three things beyond how to build a client. **How the account is
//! authenticated** decides what the credentials dialog offers — a key field, a sign-in
//! button, or nothing at all. **Where the credential comes from** is one sentence for the
//! user, because "paste your API key" is not useful advice without saying which page it is
//! on. And **what the provider lets the user choose** is published as a schema rather than
//! as knowledge the interface has to carry: Z.ai's two regions are two hosts for one API,
//! and a client that had to know what a region *is* could not draw the control.
//!
//! The compiled catalog is separate from the configured accounts. A catalog entry tells
//! clients what this build supports; an account exists only after its slug appears in
//! `config.toml`.

use std::sync::Arc;

use tidemark_core::config::Config;
use tidemark_core::oauth;
use tidemark_core::providers::keyed::{
    self, aiand, codebuff, deepgram, deepinfra, factory, fireworks, groq, ibmbob, kilo, litellm,
    llmproxy, nanogpt, openai_api, openrouter, poe, sub2api, wayfinder, xai,
};
use tidemark_core::providers::{
    AUTO_SOURCE, CLI_SOURCE, Credential, OAUTH_SOURCE, Provider, ProviderError, Source,
    antigravity, claude, codex,
};
use tidemark_core::secrets::Secrets;
use tidemark_types::{
    AccountId, CredentialKind, ExternalLogin, OptionChoice, ProviderDefinition, ProviderId,
    ProviderOption, ProviderStatus,
};

use crate::engine::Account;

/// Name of the setting, under `[provider.<slug>]`, that says which of an OAuth provider's
/// two credentials this account reads. One key serves all three: it was Antigravity's
/// alone when Antigravity was the only provider with two credentials, so a
/// `[provider.antigravity] source = "…"` written then keeps working untouched.
pub const AUTH_SOURCE: &str = "source";

/// One of the three OAuth providers: everything about it that varies, named. The same
/// entry feeds the catalog, the settings schema and the account builders, and bare
/// strings in a row are one transposition away from telling the user Tidemark writes back
/// to a file it only reads.
struct OAuthEntry {
    /// Provider slug.
    slug: &'static str,
    /// Display title.
    title: &'static str,
    /// One sentence on where the credential comes from.
    credential_hint: &'static str,
    /// What the local login is called, in the words its own program uses.
    external_label: &'static str,
    /// Where the local login lives, for a person: a path, or a sentence about a process.
    external_location: &'static str,
    /// What to run to create one.
    external_command: &'static str,
    /// Whether Tidemark refreshes this credential in place and writes the rotated token
    /// back where it found it (ADR 0001), or only ever reads it.
    writes_back: bool,
}

/// The three OAuth providers, written out because each acquires its credential its own
/// way; everything that varies between them is in this table, and everything else is
/// decided where the entries are used.
static OAUTH: &[OAuthEntry] = &[
    OAuthEntry {
        slug: antigravity::PROVIDER_ID,
        title: "Antigravity",
        credential_hint: "Sign in with Google through Tidemark, or read a signed-in agy session.",
        external_label: "agy session",
        external_location: "a signed-in agy server on this machine",
        external_command: "agy",
        writes_back: false,
    },
    OAuthEntry {
        slug: claude::PROVIDER_ID,
        title: "Claude",
        credential_hint: "Sign in through Tidemark, or read Claude Code's own login.",
        external_label: "Claude Code login",
        external_location: "~/.claude/.credentials.json",
        external_command: "claude",
        writes_back: true,
    },
    OAuthEntry {
        slug: codex::PROVIDER_ID,
        title: "Codex",
        credential_hint: "Sign in through Tidemark, or read the Codex CLI's own login.",
        external_label: "Codex CLI login",
        external_location: "~/.codex/auth.json",
        external_command: "codex login",
        writes_back: true,
    },
];

/// The table entry for an OAuth provider, or `None` for a provider with one credential.
fn oauth_entry(provider: &str) -> Option<&'static OAuthEntry> {
    OAUTH.iter().find(|entry| entry.slug == provider)
}

/// The hand-written key-authenticated providers: those whose fetch is more than one
/// request, so a `keyed::Spec` cannot describe them — ai& pages a request log,
/// Codebuff posts for credits and then reads a subscription it can do without,
/// Deepgram lists projects and then reads a usage breakdown for each (and puts its key in
/// a scheme of its own, `Authorization: Token`, which `keyed::Auth` cannot express),
/// DeepInfra reads a checklist and a month's usage, Factory walks an
/// auth/billing/usage ladder, Fireworks reads a rolling billing
/// window, Groq reads four Prometheus rate queries, IBM Bob reads a profile then
/// per-team regional budgets, Kilo reads a tRPC batch and then a profile, LiteLLM walks a
/// two-request management ladder, NanoGPT reads subscription quotas and a prepaid balance,
/// OpenAI pages two Admin API endpoints, OpenRouter reads credits and key quota, Poe pages
/// through a usage history, xAI reads a prepaid balance and a spend
/// history — and those whose single request hangs from a required base URL with no
/// default host, where the shared reader's refusal of a bad value must happen at
/// build time rather than inside an endpoint closure: LLM Proxy and sub2api — and
/// Wayfinder, a router on this machine that answers without a credential at all and reads
/// health, routes and savings in three requests. Each
/// entry is the provider's own [`keyed::HandSpec`], which carries everything a
/// `Spec` carries except the single endpoint, and says how to build a client from
/// the stored key and the account's settings. Each entry says how it is authenticated:
/// the same pasted key as the catalog's, `CredentialKind::Key`, for all of them so far,
/// so the credentials dialog is unchanged — but a provider that answers without a
/// credential says `CredentialKind::None` here and is published, and built, with no key
/// field at all.
static HAND_WRITTEN: &[&keyed::HandSpec] = &[
    &aiand::SPEC,
    &codebuff::SPEC,
    &deepgram::SPEC,
    &deepinfra::SPEC,
    &factory::SPEC,
    &fireworks::SPEC,
    &groq::SPEC,
    &ibmbob::SPEC,
    &kilo::SPEC,
    &litellm::SPEC,
    &llmproxy::SPEC,
    &nanogpt::SPEC,
    &openai_api::SPEC,
    &openrouter::SPEC,
    &poe::SPEC,
    &sub2api::SPEC,
    &wayfinder::SPEC,
    &xai::SPEC,
];

/// The catalog's own spelling of a provider's name, for the places the daemon speaks to a
/// person outside the settings dialog: notification text, and anything else that has only a
/// slug in hand.
///
/// Static, like the catalog itself, so a notification never depends on the settings file.
/// `None` for a slug this build knows nothing about; the caller falls back to capitalising
/// the slug, which is all an unknown slug can honestly be called.
pub fn title(provider: &str) -> Option<&'static str> {
    oauth_entry(provider)
        .map(|entry| entry.title)
        .or_else(|| {
            keyed::CATALOG
                .iter()
                .find(|spec| spec.id == provider)
                .map(|spec| spec.title)
        })
        .or_else(|| {
            HAND_WRITTEN
                .iter()
                .find(|spec| spec.id == provider)
                .map(|spec| spec.title)
        })
}

/// Every provider this build can configure, in stable display order.
///
/// The three OAuth providers come first, written out because each of them
/// acquires its credential its own way. Every single-request key-authenticated provider
/// follows, one entry per spec in `keyed::CATALOG` — so adding one is a file beside
/// `keyed.rs` and a line in that table, not a new stanza here. The hand-written
/// key-authenticated providers come last, from the table above, in the same shape.
pub fn catalog(config: &Config) -> Vec<ProviderDefinition> {
    let mut definitions: Vec<ProviderDefinition> = OAUTH
        .iter()
        .map(|entry| ProviderDefinition {
            provider: entry.slug.to_owned(),
            title: entry.title.to_owned(),
            credential: CredentialKind::OAuth.as_wire().to_owned(),
            credential_hint: entry.credential_hint.to_owned(),
            external: Some(ExternalLogin {
                option: AUTH_SOURCE.to_owned(),
                label: entry.external_label.to_owned(),
                location: entry.external_location.to_owned(),
                command: entry.external_command.to_owned(),
                writes_back: entry.writes_back,
            }),
            options: options(entry.slug, config),
        })
        .collect();
    definitions.extend(keyed::CATALOG.iter().map(|spec| ProviderDefinition {
        provider: spec.id.to_owned(),
        title: spec.title.to_owned(),
        credential: CredentialKind::Key.as_wire().to_owned(),
        credential_hint: spec.credential_hint.to_owned(),
        external: None,
        options: options(spec.id, config),
    }));
    definitions.extend(
        HAND_WRITTEN
            .iter()
            .map(|spec| hand_written_definition(spec, config)),
    );
    definitions
}

/// One hand-written provider as the settings dialog sees it.
///
/// Written apart from [`catalog`] so that the mapping can be checked against a spec of a
/// test's own — above all the credential kind, the one field of the table that is not the
/// same for every entry in it.
fn hand_written_definition(spec: &keyed::HandSpec, config: &Config) -> ProviderDefinition {
    ProviderDefinition {
        provider: spec.id.to_owned(),
        title: spec.title.to_owned(),
        credential: spec.credential.as_wire().to_owned(),
        credential_hint: spec.credential_hint.to_owned(),
        external: None,
        options: options(spec.id, config),
    }
}

/// Builds one configured account, or returns `None` for a slug this build does not support.
pub fn account(
    provider: &str,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Option<Account>, ProviderError> {
    let account = match provider {
        antigravity::PROVIDER_ID => Some(antigravity_account(secrets, config)?),
        claude::PROVIDER_ID => Some(claude_account(secrets, config)?),
        codex::PROVIDER_ID => Some(codex_account(secrets, config)?),
        other => keyed::CATALOG
            .iter()
            .find(|spec| spec.id == other)
            .map(|spec| keyed_account(spec))
            .or_else(|| {
                HAND_WRITTEN
                    .iter()
                    .find(|spec| spec.id == other)
                    .map(|spec| hand_written_account(spec))
            }),
    };
    Ok(account.map(|account| {
        account
            .with_options(options(provider, config))
            .with_notify(notify(provider, config))
    }))
}

/// Every configured account the daemon polls, in the order of `config.toml`.
pub fn accounts(
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Vec<Account>, ProviderError> {
    let providers = config
        .providers()
        .map_err(|error| ProviderError::Local(error.to_string()))?;
    let mut accounts = Vec::with_capacity(providers.len());
    for provider in providers {
        match account(&provider, secrets, config)? {
            Some(account) => accounts.push(account),
            None => tracing::warn!(provider, "configured provider is unsupported by this build"),
        }
    }
    Ok(accounts)
}

/// Which of a provider's windows the user asked to be notified about.
///
/// A list the file holds in a shape this build cannot read is reported and treated as
/// empty. Refusing to start over it would take the whole daemon down for a typo in an
/// opt-in list, and repairing it silently would decide on the user's behalf which windows
/// they meant.
pub fn notify(provider: &str, config: &Config) -> Vec<String> {
    match config.notify_windows(provider) {
        Ok(windows) => windows,
        Err(error) => {
            tracing::warn!(provider, %error, "ignoring an unreadable notification opt-in");
            Vec::new()
        }
    }
}

/// The settings one provider exposes, filled in from the user's file.
///
/// Called again whenever the file changes, so a provider's published options are always
/// what is on disk rather than what was on disk when the daemon started. The schema comes
/// from `keyed::CATALOG`, or from the hand-written table for the providers that are not a
/// `Spec`; either way the row is the same shape.
pub fn options(provider: &str, config: &Config) -> Vec<ProviderOption> {
    if let Some(entry) = oauth_entry(provider) {
        return vec![auth_source_option(entry, config)];
    }
    keyed::CATALOG
        .iter()
        .find(|spec| spec.id == provider)
        .map(|spec| spec.options)
        .or_else(|| {
            HAND_WRITTEN
                .iter()
                .find(|spec| spec.id == provider)
                .map(|spec| spec.options)
        })
        .map(|schemas| {
            schemas
                .iter()
                .map(|schema| published_option(provider, schema, config))
                .collect()
        })
        .unwrap_or_default()
}

/// One published setting: the provider's schema for it, filled in with the user's value.
fn published_option(
    provider: &str,
    schema: &keyed::OptionSchema,
    config: &Config,
) -> ProviderOption {
    ProviderOption {
        name: schema.name.to_owned(),
        title: schema.title.to_owned(),
        description: schema.description.map(str::to_owned),
        value: config
            .option(provider, schema.name)
            .unwrap_or(schema.default)
            .to_owned(),
        choices: schema
            .choices
            .iter()
            .map(|(value, title)| OptionChoice {
                value: (*value).to_owned(),
                title: (*title).to_owned(),
            })
            .collect(),
    }
}

/// The choice between an OAuth provider's two credentials: the login Tidemark performed
/// itself, and the one the vendor's own program already holds on this machine.
///
/// Exactly two choices, in a fixed order. `auto` is deliberately not among them: it
/// survives as what an untouched `config.toml` means — which is why the value below may
/// legitimately be a string the choices do not contain — but the user can only ever write
/// one of the two concrete values. A control that offered "automatic" would let them
/// re-ask for the silent picking this row exists to replace.
///
/// No description: the dialog draws this row itself, in the authentication group, with
/// its own explanation, and a sentence here would be shown twice.
fn auth_source_option(entry: &OAuthEntry, config: &Config) -> ProviderOption {
    ProviderOption {
        name: AUTH_SOURCE.to_owned(),
        title: "Credential".to_owned(),
        description: None,
        value: config
            .option(entry.slug, AUTH_SOURCE)
            .unwrap_or(AUTO_SOURCE)
            .to_owned(),
        choices: vec![
            OptionChoice {
                value: OAUTH_SOURCE.to_owned(),
                title: "Tidemark login".to_owned(),
            },
            OptionChoice {
                value: CLI_SOURCE.to_owned(),
                title: entry.external_label.to_owned(),
            },
        ],
    }
}

/// Which of a provider's two credentials its account reads, from the stored setting.
/// Anything unrecognised — including the unset default — is [`Source::Auto`]: the Tidemark login when there
/// is one, the vendor program's otherwise — the behaviour these accounts have always had.
fn source_value(provider: &str, config: &Config) -> Source {
    Source::from_value(config.option(provider, AUTH_SOURCE))
}

/// Whether the local login a provider can read instead of a Tidemark login exists on this
/// machine — `None` for a provider that has no such login at all.
///
/// This proves existence, not usability: the file existing is not the same as it holding
/// a usable credential, and an installed `agy` is not the same as a signed-in one. The
/// poll state says the rest. The answer exists so the dialog can offer the choice before
/// anything has been polled.
pub fn external_present(provider: &str) -> Option<bool> {
    match provider {
        claude::PROVIDER_ID => {
            Some(claude::cli_credentials_path().is_some_and(|path| path.exists()))
        }
        codex::PROVIDER_ID => Some(codex::cli_credentials_path().is_some_and(|path| path.exists())),
        antigravity::PROVIDER_ID => Some(antigravity::agy::is_available()),
        _ => None,
    }
}

/// Which credential the next poll will use, resolving an unset setting the way the
/// provider itself resolves [`Source::Auto`]. `None` for a provider with one credential.
///
/// The stored value is read off the published options rather than the file: the engine
/// has already refreshed them, and re-reading `config.toml` here could disagree with them
/// for the length of a reload. The `Auto` branches mirror the clients' own control flow
/// rather than approximating it: Claude and Codex reach the vendor file only when the
/// Secret Service answered that nothing is stored — a held token wins, and a locked
/// keyring errors out before the file is ever opened — while Antigravity tries the local
/// server first whenever `agy` is installed.
pub fn auth_source(provider: &str, status: &ProviderStatus) -> Option<String> {
    oauth_entry(provider)?;
    let stored = status
        .options
        .iter()
        .find(|option| option.name == AUTH_SOURCE)
        .map(|option| option.value.as_str());
    let resolved = match Source::from_value(stored) {
        Source::OAuth => OAUTH_SOURCE,
        Source::Cli => CLI_SOURCE,
        Source::Auto => match provider {
            claude::PROVIDER_ID | codex::PROVIDER_ID if status.has_credential == Some(false) => {
                CLI_SOURCE
            }
            antigravity::PROVIDER_ID if status.external_present == Some(true) => CLI_SOURCE,
            _ => OAUTH_SOURCE,
        },
    };
    Some(resolved.to_owned())
}

/// The OAuth client to run a login against, for a provider that has one.
pub fn oauth_client(provider: &str) -> Option<oauth::Client> {
    match provider {
        antigravity::PROVIDER_ID => Some(antigravity::oauth::client()),
        claude::PROVIDER_ID => Some(claude::oauth_client()),
        codex::PROVIDER_ID => Some(codex::oauth_client()),
        _ => None,
    }
}

/// The credential document to store after a login, in the provider's own shape.
///
/// Built by the provider rather than here, because the shape is the provider's: it is the
/// same document its parser reads out of the vendor CLI's file, which is what lets one
/// implementation serve both sources.
pub async fn login_document(
    provider: &str,
    response: &serde_json::Value,
    now_ms: i64,
) -> Result<serde_json::Value, ProviderError> {
    match provider {
        antigravity::PROVIDER_ID => antigravity::oauth::complete_login(response, now_ms).await,
        claude::PROVIDER_ID => claude::document_from_login(response, now_ms),
        codex::PROVIDER_ID => codex::document_from_login(response),
        _ => Err(ProviderError::Local(format!(
            "{provider} does not sign in through Tidemark"
        ))),
    }
}

fn antigravity_account(
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Account, ProviderError> {
    Ok(Account::with_client(Arc::new(antigravity::Antigravity::new(
        Some(Arc::clone(secrets)),
        source_value(antigravity::PROVIDER_ID, config),
    )?))
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |options| {
            let source = Source::from_value(options.get(AUTH_SOURCE).map(String::as_str));
            Ok(Arc::new(antigravity::Antigravity::new(
                Some(Arc::clone(&secrets)),
                source,
            )?) as Arc<dyn Provider>)
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint("Sign in with Google through Tidemark, or read a signed-in agy session."))
}

fn claude_account(secrets: &Arc<dyn Secrets>, config: &Config) -> Result<Account, ProviderError> {
    Ok(Account::with_client(Arc::new(claude::Claude::new(
        Some(Arc::clone(secrets)),
        source_value(claude::PROVIDER_ID, config),
    )?))
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |options| {
            let source = Source::from_value(options.get(AUTH_SOURCE).map(String::as_str));
            Ok(
                Arc::new(claude::Claude::new(Some(Arc::clone(&secrets)), source)?)
                    as Arc<dyn Provider>,
            )
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint("Sign in through Tidemark, or read Claude Code's own login."))
}

fn codex_account(secrets: &Arc<dyn Secrets>, config: &Config) -> Result<Account, ProviderError> {
    Ok(Account::with_client(Arc::new(codex::Codex::new(
        Some(Arc::clone(secrets)),
        source_value(codex::PROVIDER_ID, config),
    )?))
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |options| {
            let source = Source::from_value(options.get(AUTH_SOURCE).map(String::as_str));
            Ok(
                Arc::new(codex::Codex::new(Some(Arc::clone(&secrets)), source)?)
                    as Arc<dyn Provider>,
            )
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint("Sign in through Tidemark, or read the Codex CLI's own login."))
}

/// Every key-authenticated account is built the same way: the engine hands over the stored
/// key and the account's settings, and the spec says what to do with them.
fn keyed_account(spec: &'static keyed::Spec) -> Account {
    Account::new(
        ProviderId::new(spec.id),
        AccountId::default(),
        Box::new(move |credential, options| {
            // The URL is resolved at build time, which is why storing a key or changing a
            // setting drops the client: either may change which host this account talks to.
            Ok(Arc::new(keyed::Keyed::new(spec, credential, options)?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint(spec.credential_hint)
}

/// The hand-written key-authenticated accounts, built the same way as the catalogued ones:
/// the engine hands over the stored key and the account's settings, and the provider's own
/// builder says what to do with them. It, too, resolves its URLs at build time and refuses
/// a required option that is unset, naming it.
fn hand_written_account(spec: &'static keyed::HandSpec) -> Account {
    if spec.credential == CredentialKind::None {
        // Nothing is stored and nothing is asked for, so there is no key to hand the
        // builder — it is given a blank one and ignores it. The settings are the whole of
        // what this account is, which is why it is built from them alone.
        return Account::keyless(
            ProviderId::new(spec.id),
            AccountId::default(),
            Box::new(move |options| (spec.build)(Credential::new(String::new()), options)),
        );
    }
    Account::new(
        ProviderId::new(spec.id),
        AccountId::default(),
        Box::new(move |credential, options| (spec.build)(credential, options)),
    )
    .with_credential(spec.credential)
    .with_hint(spec.credential_hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tidemark_core::providers::{BoxFuture, Credential, zai};
    use tidemark_core::secrets::{Kind, SecretError};

    // The tests below name `zai` where a concrete slug is unavoidable: a config→option
    // binding needs a provider that has an option, and it is the only one that does. The
    // production path above names no key-authenticated provider — that is the point of the
    // table — and these tests are not a reason to reintroduce a name.

    #[derive(Debug)]
    struct NoSecrets;

    impl Secrets for NoSecrets {
        fn get<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
            Box::pin(async { Ok(None) })
        }

        fn set<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
            _secret: &'a Credential,
        ) -> BoxFuture<'a, Result<(), SecretError>> {
            Box::pin(async { Ok(()) })
        }

        fn compare_and_set<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
            _expected: &'a Credential,
            _replacement: &'a Credential,
        ) -> BoxFuture<'a, Result<bool, SecretError>> {
            Box::pin(async { Ok(false) })
        }

        fn delete<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> BoxFuture<'a, Result<(), SecretError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn empty_config() -> Config {
        Config::at(
            std::env::temp_dir()
                .join(format!("tidemark-registry-{}", std::process::id()))
                .join("absent.toml"),
        )
        .expect("a missing file is an empty config")
    }

    fn secrets() -> Arc<dyn Secrets> {
        Arc::new(NoSecrets)
    }

    fn scratch_config(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tidemark-registry-{name}-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("seeds config");
        path
    }

    #[test]
    fn every_oauth_provider_publishes_its_two_credentials_as_the_choice() {
        let published = catalog(&empty_config());
        for entry in OAUTH {
            let definition = published
                .iter()
                .find(|definition| definition.provider == entry.slug)
                .unwrap_or_else(|| panic!("{} is in the table but not published", entry.slug));
            let external = definition
                .external
                .as_ref()
                .expect("an OAuth provider has two credentials to name");
            assert_eq!(external.option, AUTH_SOURCE);
            assert_eq!(external.label, entry.external_label);
            assert_eq!(external.location, entry.external_location);
            assert_eq!(external.command, entry.external_command);
            assert_eq!(external.writes_back, entry.writes_back);
            let option = definition
                .options
                .iter()
                .find(|option| option.name == AUTH_SOURCE)
                .expect("the credential choice is published");
            assert_eq!(option.title, "Credential");
            assert_eq!(option.description, None);
            let choices: Vec<(&str, &str)> = option
                .choices
                .iter()
                .map(|choice| (choice.value.as_str(), choice.title.as_str()))
                .collect();
            assert_eq!(
                choices,
                [
                    (OAUTH_SOURCE, "Tidemark login"),
                    (CLI_SOURCE, entry.external_label)
                ],
                "auto is the unset default, never a choice, for {}",
                entry.slug
            );
        }
    }

    #[test]
    fn a_provider_with_one_credential_publishes_no_external_login() {
        // The absent field is the whole signal a client dispatches on: no external login
        // means no credential choice to draw.
        for definition in catalog(&empty_config()) {
            assert_eq!(
                definition.external.is_some(),
                oauth_entry(&definition.provider).is_some(),
                "{} must publish an external login exactly when it has two credentials",
                definition.provider
            );
        }
    }

    #[test]
    fn the_credential_choice_reports_the_stored_value_verbatim() {
        for slug in [
            antigravity::PROVIDER_ID,
            claude::PROVIDER_ID,
            codex::PROVIDER_ID,
        ] {
            let published = options(slug, &empty_config());
            let source = published
                .iter()
                .find(|option| option.name == AUTH_SOURCE)
                .expect("the credential choice is published");
            assert_eq!(
                source.value, AUTO_SOURCE,
                "auto is what an unset file means"
            );

            let path = scratch_config(
                &format!("{slug}-source"),
                &format!("providers = [\"{slug}\"]\n\n[provider.{slug}]\nsource = \"cli\"\n"),
            );
            let config = Config::at(path.clone()).expect("config reads");
            let published = options(slug, &config);
            let source = published
                .iter()
                .find(|option| option.name == AUTH_SOURCE)
                .expect("the credential choice is published");
            assert_eq!(source.value, CLI_SOURCE);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn the_oauth_accounts_rebuild_their_client_when_the_choice_changes() {
        // Without a Rebuild the engine cannot drop the client on `set_option`, and a
        // change of credential would take effect only on the next daemon restart.
        let config = empty_config();
        for slug in [
            antigravity::PROVIDER_ID,
            claude::PROVIDER_ID,
            codex::PROVIDER_ID,
        ] {
            let account = account(slug, &secrets(), &config)
                .expect("no error")
                .expect("an OAuth provider builds an account");
            assert!(
                account.rebuildable(),
                "{slug} must take a source change without a restart"
            );
        }
    }

    /// A status the way the engine hands one to `auth_source`: the published options
    /// carrying the stored value — `None` meaning an unset file, which publishes `auto` —
    /// and the two probe answers still to be filled in.
    fn probed_status(provider: &str, stored: Option<&str>) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new(provider), &AccountId::default());
        status.options = vec![ProviderOption {
            name: AUTH_SOURCE.to_owned(),
            title: "Credential".to_owned(),
            description: None,
            value: stored.unwrap_or(AUTO_SOURCE).to_owned(),
            choices: Vec::new(),
        }];
        status
    }

    #[test]
    fn claude_says_which_credential_the_next_poll_will_use() {
        for stored in [OAUTH_SOURCE, CLI_SOURCE] {
            // A stored choice wins over whatever the probe found: it is read first.
            let mut status = probed_status(claude::PROVIDER_ID, Some(stored));
            status.has_credential = Some(false);
            assert_eq!(
                auth_source(claude::PROVIDER_ID, &status).as_deref(),
                Some(stored)
            );
        }
        for (has_credential, expected) in [
            (Some(true), OAUTH_SOURCE),
            (Some(false), CLI_SOURCE),
            (None, OAUTH_SOURCE),
        ] {
            let mut status = probed_status(claude::PROVIDER_ID, None);
            status.has_credential = has_credential;
            assert_eq!(
                auth_source(claude::PROVIDER_ID, &status).as_deref(),
                Some(expected),
                "auto reaches the vendor file only on Ok(None), not on a locked keyring"
            );
        }
    }

    #[test]
    fn codex_says_which_credential_the_next_poll_will_use() {
        for stored in [OAUTH_SOURCE, CLI_SOURCE] {
            let mut status = probed_status(codex::PROVIDER_ID, Some(stored));
            status.has_credential = Some(false);
            assert_eq!(
                auth_source(codex::PROVIDER_ID, &status).as_deref(),
                Some(stored)
            );
        }
        for (has_credential, expected) in [
            (Some(true), OAUTH_SOURCE),
            (Some(false), CLI_SOURCE),
            (None, OAUTH_SOURCE),
        ] {
            let mut status = probed_status(codex::PROVIDER_ID, None);
            status.has_credential = has_credential;
            assert_eq!(
                auth_source(codex::PROVIDER_ID, &status).as_deref(),
                Some(expected),
                "auto reaches the vendor file only on Ok(None), not on a locked keyring"
            );
        }
    }

    #[test]
    fn antigravity_says_which_credential_the_next_poll_will_use() {
        for stored in [OAUTH_SOURCE, CLI_SOURCE] {
            let mut status = probed_status(antigravity::PROVIDER_ID, Some(stored));
            status.external_present = Some(true);
            assert_eq!(
                auth_source(antigravity::PROVIDER_ID, &status).as_deref(),
                Some(stored)
            );
        }
        for (external_present, expected) in [
            (Some(true), CLI_SOURCE),
            (Some(false), OAUTH_SOURCE),
            (None, OAUTH_SOURCE),
        ] {
            let mut status = probed_status(antigravity::PROVIDER_ID, None);
            status.external_present = external_present;
            assert_eq!(
                auth_source(antigravity::PROVIDER_ID, &status).as_deref(),
                Some(expected),
                "auto tries the local server first whenever agy is installed"
            );
        }
    }

    #[test]
    fn a_provider_with_one_credential_says_nothing_about_a_source() {
        assert_eq!(
            auth_source("zai", &probed_status("zai", None)),
            None,
            "there is no second credential for the next poll to use"
        );
    }

    // Two specs of the tests' own, so the mapping can be checked without waiting for a
    // provider of each kind to exist in the table.
    static KEYLESS_SPEC: keyed::HandSpec = keyed::HandSpec {
        id: "test-keyless",
        title: "Test Keyless",
        credential: CredentialKind::None,
        credential_hint: "",
        options: &[],
        build: |_, _| Err(ProviderError::Local("not built in a test".into())),
    };

    static KEY_SPEC: keyed::HandSpec = keyed::HandSpec {
        id: "test-keyed",
        title: "Test Keyed",
        credential: CredentialKind::Key,
        credential_hint: "Test console \u{2192} API keys.",
        options: &[],
        build: |_, _| Err(ProviderError::Local("not built in a test".into())),
    };

    #[test]
    fn a_provider_with_no_credential_is_published_without_a_hint() {
        // Nothing is stored and nothing is asked for, so there is no page to send anyone
        // to: the definition carries "none" and an empty hint, which is what tells the
        // settings dialog to draw no credential row at all.
        let published = hand_written_definition(&KEYLESS_SPEC, &empty_config());
        assert_eq!(published.credential, "none");
        assert_eq!(published.credential_kind(), Some(CredentialKind::None));
        assert!(published.credential_hint.is_empty());
        assert_eq!(published.external, None);
    }

    #[test]
    fn a_key_provider_is_published_exactly_as_before() {
        // The kind travelling from the spec rather than being assumed must not have moved
        // the pasted-key providers, which are every other entry in the table.
        let published = hand_written_definition(&KEY_SPEC, &empty_config());
        assert_eq!(published.credential, "key");
        assert_eq!(published.credential_kind(), Some(CredentialKind::Key));
        assert_eq!(published.credential_hint, KEY_SPEC.credential_hint);
    }

    #[test]
    fn an_account_with_no_credential_is_built_from_its_settings_alone() {
        // `Account::new` would ask the keyring for a key that was never stored and report
        // `NoCredential` forever, so a keyless account is built without a factory at all —
        // and without a hint, there being nowhere to send anyone for a credential.
        let account = hand_written_account(&KEYLESS_SPEC);
        assert_eq!(
            account.status().credential.as_deref(),
            Some(CredentialKind::None.as_wire())
        );
        assert_eq!(account.status().credential_hint, None);
    }

    #[test]
    fn the_catalog_exists_even_when_no_account_is_configured() {
        let config = empty_config();
        assert!(
            accounts(&secrets(), &config)
                .expect("accounts build")
                .is_empty()
        );
        let definitions = catalog(&config);
        assert_eq!(definitions.len(), 38);
        assert_eq!(definitions[0].provider, "antigravity");
        assert_eq!(definitions[0].credential, CredentialKind::OAuth.as_wire());
        assert_eq!(
            definitions[0]
                .external
                .as_ref()
                .map(|external| external.label.as_str()),
            Some("agy session")
        );
        assert_eq!(
            definitions[0].credential_hint,
            "Sign in with Google through Tidemark, or read a signed-in agy session."
        );
        assert!(
            definitions
                .iter()
                .all(|definition| !definition.title.is_empty())
        );
    }

    #[test]
    fn antigravity_exposes_its_registered_google_oauth_client() {
        let client = oauth_client(antigravity::PROVIDER_ID).expect("OAuth client");
        assert_eq!(client.redirect_port, 51_121);
        assert_eq!(client.redirect_path, "/oauth-callback");
        assert!(client.client_secret.is_some());
    }

    #[tokio::test]
    async fn existing_oauth_document_builders_survive_async_completion() {
        let document = login_document(
            codex::PROVIDER_ID,
            &serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh"
            }),
            1_787_270_400_000,
        )
        .await
        .expect("Codex document");

        assert_eq!(document["tokens"]["access_token"], "access");
        assert_eq!(document["tokens"]["refresh_token"], "refresh");
    }

    #[test]
    fn only_configured_known_providers_become_accounts_in_file_order() {
        let path = scratch_config(
            "configured",
            "providers = [\"zai\", \"future\", \"claude\"]\n",
        );
        let config = Config::at(path.clone()).expect("parses");
        let accounts = accounts(&secrets(), &config).expect("known accounts build");
        let slugs: Vec<&str> = accounts
            .iter()
            .map(|account| account.provider().as_str())
            .collect();
        assert_eq!(slugs, ["zai", "claude"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_configured_providers_are_reported_as_local_errors() {
        let path = scratch_config("invalid-providers", "providers = \"claude\"\n");
        let config = Config::at(path.clone()).expect("parses");
        let error = accounts(&secrets(), &config).expect_err("providers are invalid");
        assert!(
            matches!(error, ProviderError::Local(message) if message.contains("providers must be an array of strings"))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_region_defaults_to_global_and_follows_the_file_when_it_says_otherwise() {
        assert_eq!(
            options(zai::PROVIDER_ID, &empty_config())[0].value,
            "global"
        );

        let path = std::env::temp_dir().join(format!(
            "tidemark-registry-region-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[provider.zai]\nregion = \"bigmodel-cn\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert_eq!(options(zai::PROVIDER_ID, &config)[0].value, "bigmodel-cn");

        // A published option shows what is on disk verbatim; the spec's own `endpoint`
        // is what keeps an unrecognised value from reaching the wrong host, so an
        // unrecognised value on disk is not silently rewritten here.
        std::fs::write(&path, "[provider.zai]\nregion = \"mars\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert_eq!(options(zai::PROVIDER_ID, &config)[0].value, "mars");
        assert_eq!(
            (zai::SPEC.endpoint)(&BTreeMap::from([("region".to_owned(), "mars".to_owned())])),
            (zai::SPEC.endpoint)(&BTreeMap::new()),
            "a typo in a hand-edited file costs the wrong host at request time, not a dead card"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_keyed_spec_reaches_the_published_catalog() {
        let config = empty_config();
        let published = catalog(&config);
        for spec in keyed::CATALOG {
            let entry = published
                .iter()
                .find(|definition| definition.provider == spec.id)
                .unwrap_or_else(|| panic!("{} is in the catalog but not published", spec.id));
            assert_eq!(entry.title, spec.title);
            assert_eq!(entry.credential, CredentialKind::Key.as_wire());
            assert_eq!(entry.credential_hint, spec.credential_hint);
            assert_eq!(entry.options.len(), spec.options.len());
        }
    }

    #[test]
    fn every_hand_written_spec_reaches_the_published_catalog() {
        // The second table is hand-maintained, so each of its entries is checked for the
        // same agreement the catalog gets as a whole: same title, the credential the spec
        // itself declares, same hint, same options — and it must build an account at all.
        let config = empty_config();
        let published = catalog(&config);
        for spec in HAND_WRITTEN {
            let entry = published
                .iter()
                .find(|definition| definition.provider == spec.id)
                .unwrap_or_else(|| panic!("{} is in the table but not published", spec.id));
            assert_eq!(entry.title, spec.title);
            assert_eq!(entry.credential, spec.credential.as_wire());
            assert_eq!(entry.credential_hint, spec.credential_hint);
            assert_eq!(entry.options.len(), spec.options.len());
            assert!(
                account(spec.id, &secrets(), &config)
                    .expect("no error")
                    .is_some(),
                "{} must build an account",
                spec.id
            );
        }
    }

    #[test]
    fn the_oauth_providers_keep_the_head_of_the_catalog() {
        let published = catalog(&empty_config());
        let slugs: Vec<&str> = published
            .iter()
            .map(|definition| definition.provider.as_str())
            .collect();
        assert_eq!(&slugs[..3], &["antigravity", "claude", "codex"]);
    }

    #[test]
    fn every_published_slug_is_unique() {
        // A duplicate id in `keyed::CATALOG` — or in the hand-written table, or between
        // the two — would publish two definitions with the same slug: two settings rows,
        // while `account()`'s find silently uses the first; an id colliding with an OAuth
        // slug is worse, because the hand-written stanza and the spec then shadow each
        // other. At two entries neither can happen by accident; across the tables it can,
        // so the invariant is asserted rather than trusted.
        let published = catalog(&empty_config());
        let mut slugs: Vec<&str> = published
            .iter()
            .map(|definition| definition.provider.as_str())
            .collect();
        slugs.sort_unstable();
        let count = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "every slug must name one provider");
    }

    #[test]
    fn the_title_lookup_agrees_with_the_published_catalog() {
        // Notifications name providers through `title()`; the settings dialog through
        // `catalog()`. If the two disagreed, a provider's card and its notification would
        // spell its name differently on the same desktop.
        for definition in catalog(&empty_config()) {
            assert_eq!(
                title(&definition.provider),
                Some(definition.title.as_str()),
                "{} must have one spelling everywhere",
                definition.provider
            );
        }
        assert_eq!(title("nonesuch"), None);
    }

    #[test]
    fn a_keyed_spec_builds_a_configured_account() {
        let built = account("zai", &secrets(), &empty_config()).expect("no error");
        assert!(built.is_some(), "a slug in keyed::CATALOG must build");
    }

    #[test]
    fn a_slug_no_build_supports_is_still_not_an_account() {
        assert!(
            account("nonesuch", &secrets(), &empty_config())
                .expect("no error")
                .is_none(),
            "an unknown slug is warned about, not turned into an account"
        );
    }

    #[test]
    fn a_published_option_carries_the_users_current_value() {
        let path = scratch_config("zai-region", "[provider.zai]\nregion = \"bigmodel-cn\"\n");
        let config = Config::at(path.clone()).expect("parses");
        let published = catalog(&config);
        let zai = published
            .iter()
            .find(|definition| definition.provider == "zai")
            .expect("published");
        let region = zai
            .options
            .iter()
            .find(|option| option.name == "region")
            .expect("published");
        assert_eq!(region.value, "bigmodel-cn");
        assert_eq!(region.choices.len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
