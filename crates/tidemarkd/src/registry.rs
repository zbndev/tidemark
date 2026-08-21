//! Which accounts this build watches, and how each of them is signed in to.
//!
//! Registration is the whole of "adding a provider": one entry here, and the client itself
//! in `tidemark-core`. Nothing else in the daemon names a provider.
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
use tidemark_core::providers::{Provider, ProviderError, antigravity, claude, codex, kimi, zai};
use tidemark_core::secrets::Secrets;
use tidemark_types::{
    AccountId, CredentialKind, OptionChoice, ProviderDefinition, ProviderId, ProviderOption,
};

use crate::engine::Account;

/// Name of Z.ai's region setting under `[provider.zai]`.
pub const ZAI_REGION: &str = "region";
const ZAI_GLOBAL: &str = "global";
const ZAI_BIGMODEL_CN: &str = "bigmodel-cn";

/// Every provider this build can configure, in stable display order.
pub fn catalog(config: &Config) -> Vec<ProviderDefinition> {
    vec![
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
        ProviderDefinition {
            provider: kimi::PROVIDER_ID.into(),
            title: "Kimi".into(),
            credential: CredentialKind::Key.as_wire().into(),
            credential_hint:
                "Kimi Code Console → API keys. This is Kimi For Coding, not the Open Platform."
                    .into(),
            external_fallback: None,
            options: options(kimi::PROVIDER_ID, config),
        },
        ProviderDefinition {
            provider: zai::PROVIDER_ID.into(),
            title: "Z.ai".into(),
            credential: CredentialKind::Key.as_wire().into(),
            credential_hint: "Z.ai dashboard → API keys, on whichever region your account is on."
                .into(),
            external_fallback: None,
            options: options(zai::PROVIDER_ID, config),
        },
    ]
}

/// Builds one configured account, or returns `None` for a slug this build does not support.
pub fn account(
    provider: &str,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Option<Account>, ProviderError> {
    let account = match provider {
        antigravity::PROVIDER_ID => Some(antigravity_account(secrets)?),
        "claude" => Some(claude_account(secrets)?),
        codex::PROVIDER_ID => Some(codex_account(secrets)?),
        kimi::PROVIDER_ID => Some(kimi_account()),
        zai::PROVIDER_ID => Some(zai_account()),
        _ => None,
    };
    Ok(account.map(|account| account.with_options(options(provider, config))))
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

/// The settings one provider exposes, filled in from the user's file.
///
/// Called again whenever the file changes, so a provider's published options are always
/// what is on disk rather than what was on disk when the daemon started.
pub fn options(provider: &str, config: &Config) -> Vec<ProviderOption> {
    if provider != zai::PROVIDER_ID {
        return Vec::new();
    }
    let choices = vec![
        OptionChoice {
            value: ZAI_GLOBAL.to_owned(),
            title: "Global (api.z.ai)".to_owned(),
        },
        OptionChoice {
            value: ZAI_BIGMODEL_CN.to_owned(),
            title: "China (open.bigmodel.cn)".to_owned(),
        },
    ];
    vec![ProviderOption {
        name: ZAI_REGION.to_owned(),
        title: "Region".to_owned(),
        description: Some(
            "The same API on two hosts. A key issued for one is rejected by the other.".to_owned(),
        ),
        value: region(config).0.to_owned(),
        choices,
    }]
}

/// The stored region, and the client's own enum for it.
///
/// An unrecognised value falls back to Global rather than failing the account: the file is
/// hand-editable, and a typo in it should cost a wrong host rather than a card that will
/// not start.
fn region(config: &Config) -> (&'static str, zai::Region) {
    match config.option(zai::PROVIDER_ID, ZAI_REGION) {
        Some(ZAI_BIGMODEL_CN) => (ZAI_BIGMODEL_CN, zai::Region::BigModelCn),
        _ => (ZAI_GLOBAL, zai::Region::Global),
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

fn antigravity_account(secrets: &Arc<dyn Secrets>) -> Result<Account, ProviderError> {
    Ok(
        Account::with_client(Arc::new(antigravity::Antigravity::new(Some(Arc::clone(
            secrets,
        )))?))
        .with_credential(CredentialKind::OAuth)
        .with_hint("Sign in with Google through Tidemark, or use an existing agy session."),
    )
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

fn kimi_account() -> Account {
    Account::new(
        ProviderId::new(kimi::PROVIDER_ID),
        AccountId::default(),
        Box::new(|credential, _options| {
            Ok(Arc::new(kimi::Kimi::new(credential)?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint("Kimi Code Console → API keys. This is Kimi For Coding, not the Open Platform.")
}

fn zai_account() -> Account {
    Account::new(
        ProviderId::new(zai::PROVIDER_ID),
        AccountId::default(),
        Box::new(|credential, options| {
            // The region is read at build time, which is why storing a key or changing the
            // region drops the client: both change which host this account talks to.
            let region = match options.get(ZAI_REGION).map(String::as_str) {
                Some(ZAI_BIGMODEL_CN) => zai::Region::BigModelCn,
                _ => zai::Region::Global,
            };
            Ok(Arc::new(zai::Zai::new(credential, region)?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint("Z.ai dashboard → API keys, on whichever region your account is on.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_core::providers::{BoxFuture, Credential};
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
        assert_eq!(region(&empty_config()).1, zai::Region::Global);

        let path = std::env::temp_dir().join(format!(
            "tidemark-registry-region-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[provider.zai]\nregion = \"bigmodel-cn\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert_eq!(region(&config).1, zai::Region::BigModelCn);
        assert_eq!(options(zai::PROVIDER_ID, &config)[0].value, "bigmodel-cn");

        std::fs::write(&path, "[provider.zai]\nregion = \"mars\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert_eq!(
            region(&config).1,
            zai::Region::Global,
            "a typo in a hand-edited file costs the wrong host, not a dead card"
        );
        let _ = std::fs::remove_file(&path);
    }
}
