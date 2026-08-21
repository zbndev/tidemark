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
//! There is still no list of accounts in configuration, and deliberately so — an account
//! exists as far as the daemon is concerned and reports `no-credential` until something
//! supplies one. What `config.toml` now holds is the settings *of* those accounts, which
//! is a different question from which of them exist.

use std::sync::Arc;

use tidemark_core::config::Config;
use tidemark_core::oauth;
use tidemark_core::providers::{Provider, ProviderError, antigravity, claude, codex, kimi, zai};
use tidemark_core::secrets::Secrets;
use tidemark_types::{AccountId, CredentialKind, OptionChoice, ProviderId, ProviderOption};

use crate::engine::Account;

/// Name of Z.ai's region setting under `[provider.zai]`.
pub const ZAI_REGION: &str = "region";
const ZAI_GLOBAL: &str = "global";
const ZAI_BIGMODEL_CN: &str = "bigmodel-cn";

/// Every account the daemon polls.
pub fn accounts(
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Vec<Account>, ProviderError> {
    Ok(vec![
        antigravity_account()?,
        claude_account(secrets)?,
        codex_account(secrets)?,
        kimi_account(),
        zai_account(),
    ]
    .into_iter()
    .map(|account| {
        let options = options(account.provider().as_str(), config);
        account.with_options(options)
    })
    .collect())
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
pub fn login_document(
    provider: &str,
    response: &serde_json::Value,
    now_ms: i64,
) -> Result<serde_json::Value, ProviderError> {
    match provider {
        "claude" => claude::document_from_login(response, now_ms),
        codex::PROVIDER_ID => codex::document_from_login(response),
        _ => Err(ProviderError::Local(format!(
            "{provider} does not sign in through Tidemark"
        ))),
    }
}

fn antigravity_account() -> Result<Account, ProviderError> {
    // Its credential is neither ours to hold nor a file we read: the `agy` CLI keeps a
    // session in the system keyring and answers on loopback. What this build owns is the
    // process, which is why the client exists from registration and starts nothing until
    // the first poll. There is nothing here for a credentials dialog to do — signing in
    // happens in `agy`, and Tidemark reports what it finds.
    Ok(
        Account::with_client(Arc::new(antigravity::Antigravity::new()?))
            .with_credential(CredentialKind::External)
            .with_hint("Sign in with the Antigravity IDE or the agy CLI; Tidemark reads the session they keep."),
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

    fn registered() -> Vec<Account> {
        let secrets: Arc<dyn Secrets> = Arc::new(NoSecrets);
        accounts(&secrets, &empty_config()).expect("registry builds")
    }

    #[test]
    fn every_registered_account_is_published_before_it_is_polled() {
        let accounts = registered();
        assert_eq!(accounts.len(), 5);
        let providers: Vec<&str> = accounts
            .iter()
            .map(|account| account.status().provider.as_str())
            .collect();
        assert_eq!(
            providers,
            [
                antigravity::PROVIDER_ID,
                "claude",
                codex::PROVIDER_ID,
                kimi::PROVIDER_ID,
                zai::PROVIDER_ID
            ]
        );
        assert!(accounts.iter().all(|account| {
            account.status().account == "default" && account.status().captured_at.is_none()
        }));
    }

    #[test]
    fn every_account_says_what_its_credentials_dialog_should_offer() {
        for account in registered() {
            let status = account.status();
            assert!(
                status.credential_kind().is_some(),
                "{} published no credential kind",
                status.provider
            );
            assert!(
                status.credential_hint.is_some(),
                "{} left the user with nowhere to go",
                status.provider
            );
        }
    }

    #[test]
    fn only_the_provider_with_a_choice_to_make_publishes_one() {
        for account in registered() {
            let status = account.status();
            let expected = usize::from(status.provider == zai::PROVIDER_ID);
            assert_eq!(status.options.len(), expected, "{}", status.provider);
        }
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
