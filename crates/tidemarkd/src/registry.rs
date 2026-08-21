//! Which accounts this build watches, and how each of them is signed in to.
//!
//! Registration is a spec in `keyed::CATALOG`, for every key-authenticated provider. Only
//! the three OAuth providers — Antigravity, Claude and Codex — are registered by hand here,
//! because each of them acquires its credential its own way. Nothing else in the daemon
//! names a key-authenticated provider.
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
use tidemark_core::providers::{Provider, ProviderError, antigravity, claude, codex, keyed};
use tidemark_core::secrets::Secrets;
use tidemark_types::{
    AccountId, CredentialKind, OptionChoice, ProviderDefinition, ProviderId, ProviderOption,
};

use crate::engine::Account;

/// Name of Antigravity's usage-source setting under `[provider.antigravity]`.
pub const ANTIGRAVITY_SOURCE: &str = "source";

/// Every provider this build can configure, in stable display order.
///
/// The three OAuth providers come first, written out by hand because each of them
/// acquires its credential its own way. Every key-authenticated provider follows, one
/// entry per spec in `keyed::CATALOG` — so adding one is a file beside `keyed.rs` and a
/// line in that table, not a new stanza here.
pub fn catalog(config: &Config) -> Vec<ProviderDefinition> {
    let mut definitions = vec![
        ProviderDefinition {
            provider: antigravity::PROVIDER_ID.into(),
            title: "Antigravity".into(),
            credential: CredentialKind::OAuth.as_wire().into(),
            credential_hint:
                "Sign in with Google through Tidemark, or use an existing agy session.".into(),
            external_fallback: Some("agy session".into()),
            options: options(antigravity::PROVIDER_ID, config),
        },
        ProviderDefinition {
            provider: "claude".into(),
            title: "Claude".into(),
            credential: CredentialKind::OAuth.as_wire().into(),
            credential_hint: "Sign in through Tidemark or use Claude Code's login.".into(),
            external_fallback: Some("Claude Code login".into()),
            options: options("claude", config),
        },
        ProviderDefinition {
            provider: codex::PROVIDER_ID.into(),
            title: "Codex".into(),
            credential: CredentialKind::OAuth.as_wire().into(),
            credential_hint: "Sign in through Tidemark or use the Codex CLI's login.".into(),
            external_fallback: Some("Codex CLI login".into()),
            options: options(codex::PROVIDER_ID, config),
        },
    ];
    definitions.extend(keyed::CATALOG.iter().map(|spec| ProviderDefinition {
        provider: spec.id.to_owned(),
        title: spec.title.to_owned(),
        credential: CredentialKind::Key.as_wire().to_owned(),
        credential_hint: spec.credential_hint.to_owned(),
        external_fallback: None,
        options: options(spec.id, config),
    }));
    definitions
}

/// Builds one configured account, or returns `None` for a slug this build does not support.
pub fn account(
    provider: &str,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Option<Account>, ProviderError> {
    let account = match provider {
        antigravity::PROVIDER_ID => Some(antigravity_account(secrets, config)?),
        "claude" => Some(claude_account(secrets)?),
        codex::PROVIDER_ID => Some(codex_account(secrets)?),
        other => keyed::CATALOG
            .iter()
            .find(|spec| spec.id == other)
            .map(|spec| keyed_account(spec)),
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
/// what is on disk rather than what was on disk when the daemon started.
pub fn options(provider: &str, config: &Config) -> Vec<ProviderOption> {
    if provider == antigravity::PROVIDER_ID {
        return vec![antigravity_source_option(config)];
    }
    let Some(spec) = keyed::CATALOG.iter().find(|spec| spec.id == provider) else {
        return Vec::new();
    };
    spec.options
        .iter()
        .map(|schema| ProviderOption {
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
                .map(|(value, label)| OptionChoice {
                    value: (*value).to_owned(),
                    title: (*label).to_owned(),
                })
                .collect(),
        })
        .collect()
}

/// Antigravity's two quota sources, and letting the user say which one this account reads.
///
/// Published as a choice rather than decided here because neither source is right
/// everywhere: `agy` is the vendor's own live session but has to be installed and logged
/// in, while the login works on a machine with no `agy` and only for an account Google
/// entitles to the Cloud Code quota RPCs.
fn antigravity_source_option(config: &Config) -> ProviderOption {
    let choices = vec![
        OptionChoice {
            value: antigravity::AUTO_SOURCE.to_owned(),
            title: "Automatic".to_owned(),
        },
        OptionChoice {
            value: antigravity::OAUTH_SOURCE.to_owned(),
            title: "Google sign-in".to_owned(),
        },
        OptionChoice {
            value: antigravity::CLI_SOURCE.to_owned(),
            title: "Local agy session".to_owned(),
        },
    ];
    ProviderOption {
        name: ANTIGRAVITY_SOURCE.to_owned(),
        title: "Usage source".to_owned(),
        description: Some(
            "Automatic reads the local agy session and falls back to the Google sign-in."
                .to_owned(),
        ),
        value: source_value(config).0.to_owned(),
        choices,
    }
}

/// The stored usage source, and the client's own enum for it.
fn source_value(config: &Config) -> (&'static str, antigravity::Source) {
    match config.option(antigravity::PROVIDER_ID, ANTIGRAVITY_SOURCE) {
        Some(antigravity::OAUTH_SOURCE) => (antigravity::OAUTH_SOURCE, antigravity::Source::OAuth),
        Some(antigravity::CLI_SOURCE) => (antigravity::CLI_SOURCE, antigravity::Source::Cli),
        _ => (antigravity::AUTO_SOURCE, antigravity::Source::Auto),
    }
}

/// The OAuth client to run a login against, for a provider that has one.
pub fn oauth_client(provider: &str) -> Option<oauth::Client> {
    match provider {
        antigravity::PROVIDER_ID => Some(antigravity::oauth::client()),
        "claude" => Some(claude::oauth_client()),
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
        "claude" => claude::document_from_login(response, now_ms),
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
        source_value(config).1,
    )?))
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |options| {
            let source = antigravity::Source::from_value(
                options.get(ANTIGRAVITY_SOURCE).map(String::as_str),
            );
            Ok(Arc::new(antigravity::Antigravity::new(
                Some(Arc::clone(&secrets)),
                source,
            )?) as Arc<dyn Provider>)
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint("Sign in with Google through Tidemark, or use an existing agy session."))
}

fn claude_account(secrets: &Arc<dyn Secrets>) -> Result<Account, ProviderError> {
    Ok(
        Account::with_client(Arc::new(claude::Claude::new(Some(Arc::clone(secrets)))?))
            .with_credential(CredentialKind::OAuth)
            .with_hint(
                "Uses Claude Code's own login when there is one. Sign in here to give Tidemark an account of its own.",
            ),
    )
}

fn codex_account(secrets: &Arc<dyn Secrets>) -> Result<Account, ProviderError> {
    Ok(
        Account::with_client(Arc::new(codex::Codex::new(Some(Arc::clone(secrets)))?))
            .with_credential(CredentialKind::OAuth)
            .with_hint(
                "Uses the Codex CLI's own login when there is one. Sign in here to give Tidemark an account of its own.",
            ),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tidemark_core::providers::{BoxFuture, Credential, zai};
    use tidemark_core::secrets::{Kind, SecretError};

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
    fn antigravity_publishes_its_three_usage_sources() {
        let config = empty_config();
        let published = options(antigravity::PROVIDER_ID, &config);
        let source = published
            .iter()
            .find(|option| option.name == ANTIGRAVITY_SOURCE)
            .expect("the usage source is published");
        let values: Vec<&str> = source
            .choices
            .iter()
            .map(|choice| choice.value.as_str())
            .collect();
        assert_eq!(values, ["auto", "oauth", "cli"]);
        assert_eq!(source.value, "auto", "auto is what an unset file means");
    }

    #[test]
    fn a_configured_usage_source_is_what_the_option_reports() {
        let path = scratch_config(
            "antigravity-source",
            "providers = [\"antigravity\"]\n\n[provider.antigravity]\nsource = \"cli\"\n",
        );
        let config = Config::at(path).expect("config reads");
        let published = options(antigravity::PROVIDER_ID, &config);
        let source = published
            .iter()
            .find(|option| option.name == ANTIGRAVITY_SOURCE)
            .expect("the usage source is published");
        assert_eq!(source.value, "cli");
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
        assert_eq!(definitions.len(), 5);
        assert_eq!(definitions[0].provider, "antigravity");
        assert_eq!(definitions[0].credential, CredentialKind::OAuth.as_wire());
        assert_eq!(
            definitions[0].external_fallback.as_deref(),
            Some("agy session")
        );
        assert_eq!(
            definitions[0].credential_hint,
            "Sign in with Google through Tidemark, or use an existing agy session."
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
    fn the_oauth_providers_keep_the_head_of_the_catalog() {
        let published = catalog(&empty_config());
        let slugs: Vec<&str> = published
            .iter()
            .map(|definition| definition.provider.as_str())
            .collect();
        assert_eq!(&slugs[..3], &["antigravity", "claude", "codex"]);
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
