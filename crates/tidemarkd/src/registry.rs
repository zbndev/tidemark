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
    self, abacus, aiand, alibaba, augment, codebuff, commandcode, cursor, deepgram, deepinfra,
    factory, fireworks, gemini, grok, groq, ibmbob, kilo, litellm, llmproxy, longcat, manus, mimo,
    mistral, nanogpt, notion, ollama, openai_api, opencode, openrouter, perplexity, poe, qoder,
    sakana, stepfun, sub2api, wayfinder, xai, zai, zoommate,
};
// t3chat is Unix-only for now: its HTTP stack wreq depends unconditionally on boring2
// (BoringSSL), which does not build on Windows yet — reversible once boring-sys2 does.
#[cfg(not(target_os = "windows"))]
use tidemark_core::providers::keyed::t3chat;
use tidemark_core::providers::{
    AUTO_SOURCE, CLI_SOURCE, OAUTH_SOURCE, Provider, ProviderError, Source, antigravity, claude,
    codex,
};
use tidemark_core::secrets::Secrets;
use tidemark_types::{
    AccountId, AuthMode, AuthSelection, AuthSelector, CredentialKind, ExternalLogin, OptionChoice,
    ProviderDefinition, ProviderId, ProviderOption, ProviderStatus,
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

// Todo 19 remains gated on G2; Windows must not register the local agy source before it passes.
#[cfg(target_os = "windows")]
const ANTIGRAVITY_LOCAL_SOURCE_AVAILABLE: bool = false;
// Todo 19 remains gated on G2; non-Windows builds retain the existing local agy registration.
#[cfg(not(target_os = "windows"))]
const ANTIGRAVITY_LOCAL_SOURCE_AVAILABLE: bool = true;

/// Whether this build registers the vendor program's local credential source.
fn local_source_available(provider: &str) -> bool {
    provider != antigravity::PROVIDER_ID || ANTIGRAVITY_LOCAL_SOURCE_AVAILABLE
}

fn credential_hint(entry: &OAuthEntry) -> &'static str {
    if local_source_available(entry.slug) {
        entry.credential_hint
    } else {
        "Sign in with Google through Tidemark."
    }
}

/// The hand-written key-authenticated providers: those whose fetch is more than one
/// request, so a `keyed::Spec` cannot describe them — ai& pages a request log,
/// Alibaba Coding Plan retries its one quota POST across the international and
/// China-mainland gateways,
/// Codebuff posts for credits and then reads a subscription it can do without,
/// Cursor reads a usage summary and then the identity, legacy request quota and weekly Bot
/// allowance an account may or may not have, signing every request with the session cookie
/// a browser on this machine already holds rather than with anything the user is asked for,
/// Deepgram lists projects and then reads a usage breakdown for each (and puts its key in
/// a scheme of its own, `Authorization: Token`, which `keyed::Auth` cannot express),
/// DeepInfra reads a checklist and a month's usage, Factory walks an
/// auth/billing/usage ladder, Fireworks reads a rolling billing
/// window, Groq reads four Prometheus rate queries, IBM Bob reads a profile then
/// per-team regional budgets, Kilo reads a tRPC batch and then a profile, LiteLLM walks a
/// two-request management ladder, Manus reads its browser session's credit inventory, NanoGPT reads subscription quotas and a prepaid balance,
/// OpenAI pages two Admin API endpoints, OpenRouter reads credits and key quota, Poe pages
/// through a usage history, StepFun asks a rate-limit RPC and then a plan-status one,
/// deriving the device id its cookie pair needs from the token's own JWT payload at
/// build time, xAI reads a prepaid balance and a spend
/// history, and Z.ai follows its quota meters with the wallet
/// the same key reads, gating its MCP window on the money there is left — and those whose single request hangs from a required base URL with no
/// default host, where the shared reader's refusal of a bad value must happen at
/// build time rather than inside an endpoint closure: LLM Proxy and sub2api — and
/// Wayfinder, a router on this machine that answers without a credential at all and reads
/// health, routes and savings in three requests. Each
/// entry is the provider's own [`keyed::HandSpec`], which carries everything a
/// `Spec` carries except the single endpoint, and says how to build a client from
/// the stored key and the account's settings. An ordinary entry says it uses the same
/// pasted key as the catalog's, `CredentialKind::Key`; a browser-session provider says
/// `CredentialKind::External`; and a provider that answers without a credential says
/// `CredentialKind::None` and is published, and built, with no key field at all.
static HAND_WRITTEN: &[&keyed::HandSpec] = &[
    &abacus::SPEC,
    &aiand::SPEC,
    &alibaba::SPEC,
    &augment::SPEC,
    &codebuff::SPEC,
    &commandcode::SPEC,
    &cursor::SPEC,
    &deepgram::SPEC,
    &deepinfra::SPEC,
    &factory::SPEC,
    &fireworks::SPEC,
    &gemini::SPEC,
    &grok::SPEC,
    &groq::SPEC,
    &ibmbob::SPEC,
    &kilo::SPEC,
    &litellm::SPEC,
    &llmproxy::SPEC,
    &longcat::SPEC,
    &manus::SPEC,
    &mimo::SPEC,
    &mistral::SPEC,
    &nanogpt::SPEC,
    &notion::SPEC,
    &ollama::SPEC,
    &openai_api::SPEC,
    &opencode::SPEC,
    &openrouter::SPEC,
    &perplexity::SPEC,
    &poe::SPEC,
    &qoder::SPEC,
    &sakana::SPEC,
    &stepfun::SPEC,
    &sub2api::SPEC,
    // t3chat is Unix-only for now: wreq depends unconditionally on boring2 (BoringSSL),
    // which does not build on Windows yet — reversible once boring-sys2 does.
    #[cfg(not(target_os = "windows"))]
    &t3chat::SPEC,
    &wayfinder::SPEC,
    &xai::SPEC,
    &zai::SPEC,
    &zoommate::SPEC,
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
            credential_hint: credential_hint(entry).to_owned(),
            external: local_source_available(entry.slug).then(|| ExternalLogin {
                option: AUTH_SOURCE.to_owned(),
                label: entry.external_label.to_owned(),
                location: entry.external_location.to_owned(),
                command: entry.external_command.to_owned(),
                writes_back: entry.writes_back,
            }),
            browser_auth: None,
            options: options(entry.slug, config),
        })
        .collect();
    definitions.extend(keyed::CATALOG.iter().map(|spec| ProviderDefinition {
        provider: spec.id.to_owned(),
        title: spec.title.to_owned(),
        credential: CredentialKind::Key.as_wire().to_owned(),
        credential_hint: spec.credential_hint.to_owned(),
        external: None,
        browser_auth: None,
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
        browser_auth: browser_auth(spec.id),
        options: options(spec.id, config),
    }
}

/// Builds one configured account, or returns `None` for a slug this build does not support.
pub fn account(
    provider: &str,
    account: &AccountId,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Option<Account>, ProviderError> {
    let account = match provider {
        antigravity::PROVIDER_ID => Some(antigravity_account(account, secrets, config)?),
        claude::PROVIDER_ID => Some(claude_account(account, secrets, config)?),
        codex::PROVIDER_ID => Some(codex_account(account, secrets, config)?),
        other => keyed::CATALOG
            .iter()
            .find(|spec| spec.id == other)
            .map(|spec| keyed_account(spec, account))
            .or_else(|| {
                HAND_WRITTEN
                    .iter()
                    .find(|spec| spec.id == other)
                    .map(|spec| hand_written_account(spec, account))
            }),
    };
    Ok(account.map(|account| {
        account
            .with_options(options(provider, config))
            .with_auth_selection(browser_auth_selection(provider, config))
            .with_notify(notify(provider, config))
    }))
}

/// The local browser-auth capability one hand-written provider declares.
///
/// This is daemon metadata rather than a GUI branch: a later browser-cookie provider adds
/// its selector here and gets the same wire contract, engine lifecycle and GTK rendering.
fn browser_auth(provider: &str) -> Option<AuthSelector> {
    match provider {
        cursor::PROVIDER_ID => Some(AuthSelector {
            option: cursor::AUTH_SOURCE.into(),
            modes: vec![
                AuthMode {
                    value: cursor::CURSOR_APP_SOURCE.into(),
                    title: "Cursor App".into(),
                },
                AuthMode {
                    value: cursor::BROWSER_SOURCE.into(),
                    title: "Browser".into(),
                },
                AuthMode {
                    value: keyed::session::PASTE_SOURCE.into(),
                    title: "Paste session".into(),
                },
            ],
        }),
        abacus::PROVIDER_ID
        | augment::PROVIDER_ID
        | commandcode::PROVIDER_ID
        | longcat::PROVIDER_ID
        | manus::PROVIDER_ID
        | mimo::PROVIDER_ID
        | mistral::PROVIDER_ID
        | notion::PROVIDER_ID
        | ollama::PROVIDER_ID
        | opencode::PROVIDER_ID
        | perplexity::PROVIDER_ID
        | qoder::PROVIDER_ID
        | sakana::PROVIDER_ID
        | zoommate::PROVIDER_ID => Some(AuthSelector {
            option: cursor::AUTH_SOURCE.into(),
            modes: vec![
                AuthMode {
                    value: cursor::BROWSER_SOURCE.into(),
                    title: "Browser".into(),
                },
                AuthMode {
                    value: keyed::session::PASTE_SOURCE.into(),
                    title: "Paste session".into(),
                },
            ],
        }),
        // Split from the arm above only because a cfg attribute cannot sit on one
        // alternative of an or-pattern; same boring2 reason as the use gate above.
        #[cfg(not(target_os = "windows"))]
        t3chat::PROVIDER_ID => Some(AuthSelector {
            option: cursor::AUTH_SOURCE.into(),
            modes: vec![
                AuthMode {
                    value: cursor::BROWSER_SOURCE.into(),
                    title: "Browser".into(),
                },
                AuthMode {
                    value: keyed::session::PASTE_SOURCE.into(),
                    title: "Paste session".into(),
                },
            ],
        }),
        _ => None,
    }
}

/// Whether a provider offers a pasted session as one of its authentication modes.
///
/// The engine asks before it reads the session slot at all, so that an account of a
/// provider that has no paste mode never queries the keyring for one.
pub(crate) fn has_pasted_session_auth(provider: &str) -> bool {
    browser_auth(provider).is_some_and(|selector| {
        selector
            .modes
            .iter()
            .any(|mode| mode.value == keyed::session::PASTE_SOURCE)
    })
}

/// The durable config source an account is already constrained to, if it is complete.
///
/// An incomplete or hand-edited value deliberately remains absent: treating it as another
/// source would restore the old silent fallback this selector is designed to remove.
pub(crate) fn browser_auth_selection(provider: &str, config: &Config) -> Option<AuthSelection> {
    browser_auth(provider)?;
    match config.option(provider, cursor::AUTH_SOURCE) {
        Some(cursor::CURSOR_APP_SOURCE) => Some(AuthSelection {
            mode: cursor::CURSOR_APP_SOURCE.into(),
            candidate: None,
        }),
        // The paste mode names no candidate: the stored header is the whole selection,
        // and the settings deliberately hold nothing that could identify it.
        Some(keyed::session::PASTE_SOURCE) => Some(AuthSelection {
            mode: keyed::session::PASTE_SOURCE.into(),
            candidate: None,
        }),
        Some(cursor::BROWSER_SOURCE) => {
            let browser = config.option(provider, cursor::AUTH_BROWSER)?;
            let candidate = match config.option(provider, cursor::AUTH_PROFILE) {
                Some(profile) => format!("{browser}/{profile}"),
                None => browser.to_owned(),
            };
            Some(AuthSelection {
                mode: cursor::BROWSER_SOURCE.into(),
                candidate: Some(candidate),
            })
        }
        _ => None,
    }
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
        for account_id in config
            .accounts(&provider)
            .map_err(|error| ProviderError::Local(error.to_string()))?
        {
            let account_id = AccountId::new(account_id);
            match account(&provider, &account_id, secrets, config)? {
                Some(account) => accounts.push(account),
                None => tracing::warn!(
                    provider,
                    account = %account_id,
                    "configured provider is unsupported by this build"
                ),
            }
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
/// Both choices are published in a fixed order when the local source is available; a
/// platform without it publishes only OAuth. `auto` is deliberately not among them: it
/// survives as what an untouched `config.toml` means where both sources exist — which is
/// why the value below may legitimately be a string the choices do not contain — but the
/// user can only ever write a concrete value. A control that offered "automatic" would
/// let them re-ask for the silent picking this row exists to replace.
///
/// No description: the dialog draws this row itself, in the authentication group, with
/// its own explanation, and a sentence here would be shown twice.
fn auth_source_option(entry: &OAuthEntry, config: &Config) -> ProviderOption {
    let available = local_source_available(entry.slug);
    let value = if available {
        config
            .option(entry.slug, AUTH_SOURCE)
            .unwrap_or(AUTO_SOURCE)
            .to_owned()
    } else {
        OAUTH_SOURCE.to_owned()
    };
    let mut choices = vec![OptionChoice {
        value: OAUTH_SOURCE.to_owned(),
        title: "Tidemark login".to_owned(),
    }];
    if available {
        choices.push(OptionChoice {
            value: CLI_SOURCE.to_owned(),
            title: entry.external_label.to_owned(),
        });
    }
    ProviderOption {
        name: AUTH_SOURCE.to_owned(),
        title: "Credential".to_owned(),
        description: None,
        value,
        choices,
    }
}

/// Which of a provider's two credentials its account reads, from the stored setting.
/// Anything unrecognised — including the unset default — is [`Source::Auto`]: the Tidemark login when there
/// is one, the vendor program's otherwise — the behaviour these accounts have always had.
fn source_value(provider: &str, config: &Config) -> Source {
    Source::from_value(config.option(provider, AUTH_SOURCE))
}

/// Extra configured accounts have no vendor CLI file, so they always use Tidemark's login.
pub(crate) fn source_for_account(provider: &str, account: &AccountId, config: &Config) -> Source {
    if account.as_str() == "default" {
        supported_source(provider, source_value(provider, config))
    } else {
        Source::OAuth
    }
}

fn supported_source(provider: &str, source: Source) -> Source {
    if local_source_available(provider) {
        source
    } else {
        Source::OAuth
    }
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
        antigravity::PROVIDER_ID => Some(antigravity_external_present()),
        gemini::PROVIDER_ID => {
            Some(gemini::cli_credentials_path().is_some_and(|path| path.exists()))
        }
        grok::PROVIDER_ID => Some(grok::cli_credentials_path().is_some_and(|path| path.exists())),
        _ => None,
    }
}

/// Which credential the next poll will use, from the source selected for this account.
///
/// The account carries the constructor's resolved source. `Source::Auto` still follows the
/// provider's historical runtime rule using the probe answers, while explicit `Cli` and
/// `OAuth` sources are authoritative — especially for non-default accounts, which never
/// have a vendor CLI file.
pub fn auth_source(provider: &str, source: Source, status: &ProviderStatus) -> Option<String> {
    oauth_entry(provider)?;
    let resolved = match supported_source(provider, source) {
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

// Todo 19 remains gated on G2; only non-Windows builds may inspect or start the agy supervisor.
#[cfg(not(target_os = "windows"))]
fn antigravity_external_present() -> bool {
    antigravity::agy::is_available()
}

// Todo 19 remains gated on G2; Windows publishes the local agy source as unavailable.
#[cfg(target_os = "windows")]
fn antigravity_external_present() -> bool {
    false
}

// Todo 19 remains gated on G2; non-Windows account construction retains the agy-capable provider.
#[cfg(not(target_os = "windows"))]
fn build_antigravity(
    account: AccountId,
    secrets: Arc<dyn Secrets>,
    source: Source,
) -> Result<antigravity::Antigravity, ProviderError> {
    antigravity::Antigravity::new(account, Some(secrets), source)
}

// Todo 19 remains gated on G2; Windows constructs the same provider through its OAuth-only path.
#[cfg(target_os = "windows")]
fn build_antigravity(
    account: AccountId,
    secrets: Arc<dyn Secrets>,
    _source: Source,
) -> Result<antigravity::Antigravity, ProviderError> {
    antigravity::Antigravity::oauth(account, secrets)
}

fn antigravity_account(
    account: &AccountId,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Account, ProviderError> {
    let source = source_for_account(antigravity::PROVIDER_ID, account, config);
    let account_id = account.clone();
    Ok(Account::with_client(Arc::new(build_antigravity(
        account_id.clone(),
        Arc::clone(secrets),
        source,
    )?))
    .with_source(source)
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |account, _credential, options| {
            let source = if account.as_str() == "default" {
                supported_source(
                    antigravity::PROVIDER_ID,
                    Source::from_value(options.get(AUTH_SOURCE).map(String::as_str)),
                )
            } else {
                Source::OAuth
            };
            Ok(Arc::new(build_antigravity(
                account.clone(),
                Arc::clone(&secrets),
                source,
            )?) as Arc<dyn Provider>)
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint(credential_hint(
        oauth_entry(antigravity::PROVIDER_ID).expect("Antigravity is registered"),
    )))
}

fn claude_account(
    account: &AccountId,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Account, ProviderError> {
    let source = source_for_account(claude::PROVIDER_ID, account, config);
    let account_id = account.clone();
    Ok(Account::with_client(Arc::new(claude::Claude::new(
        account_id.clone(),
        Some(Arc::clone(secrets)),
        source,
    )?))
    .with_source(source)
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |account, _credential, options| {
            let source = if account.as_str() == "default" {
                Source::from_value(options.get(AUTH_SOURCE).map(String::as_str))
            } else {
                Source::OAuth
            };
            Ok(Arc::new(claude::Claude::new(
                account.clone(),
                Some(Arc::clone(&secrets)),
                source,
            )?) as Arc<dyn Provider>)
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint("Sign in through Tidemark, or read Claude Code's own login."))
}

fn codex_account(
    account: &AccountId,
    secrets: &Arc<dyn Secrets>,
    config: &Config,
) -> Result<Account, ProviderError> {
    let source = source_for_account(codex::PROVIDER_ID, account, config);
    let account_id = account.clone();
    Ok(Account::with_client(Arc::new(codex::Codex::new(
        account_id.clone(),
        Some(Arc::clone(secrets)),
        source,
    )?))
    .with_source(source)
    .with_rebuild({
        let secrets = Arc::clone(secrets);
        Box::new(move |account, _credential, options| {
            let source = if account.as_str() == "default" {
                Source::from_value(options.get(AUTH_SOURCE).map(String::as_str))
            } else {
                Source::OAuth
            };
            Ok(Arc::new(codex::Codex::new(
                account.clone(),
                Some(Arc::clone(&secrets)),
                source,
            )?) as Arc<dyn Provider>)
        })
    })
    .with_credential(CredentialKind::OAuth)
    .with_hint("Sign in through Tidemark, or read the Codex CLI's own login."))
}

/// Every key-authenticated account is built the same way: the engine hands over the stored
/// key and the account's settings, and the spec says what to do with them.
fn keyed_account(spec: &'static keyed::Spec, account: &AccountId) -> Account {
    let account_id = account.clone();
    Account::new(
        ProviderId::new(spec.id),
        account_id.clone(),
        Box::new(move |account, credential, options| {
            // The URL is resolved at build time, which is why storing a key or changing a
            // setting drops the client: either may change which host this account talks to.
            Ok(Arc::new(keyed::Keyed::new(
                account.clone(),
                spec,
                credential,
                options,
            )?) as Arc<dyn Provider>)
        }),
    )
    .with_credential(CredentialKind::Key)
    .with_hint(spec.credential_hint)
}

/// The hand-written key-authenticated accounts, built the same way as the catalogued ones:
/// the engine hands over the stored key and the account's settings, and the provider's own
/// builder says what to do with them. It, too, resolves its URLs at build time and refuses
/// a required option that is unset, naming it.
fn hand_written_account(spec: &'static keyed::HandSpec, account: &AccountId) -> Account {
    if matches!(
        spec.credential,
        CredentialKind::None | CredentialKind::External
    ) {
        // Neither local gateways nor external local sessions have a Tidemark-owned secret.
        // Both rebuild from settings alone; their published kinds still distinguish the two
        // contracts for clients. A credential-free service also keeps its hint absent —
        // there is genuinely nothing to say about where a nonexistent secret comes from.
        let account = Account::keyless(
            ProviderId::new(spec.id),
            account.clone(),
            Box::new(move |account_id, credential, options| {
                (spec.build)(account_id.clone(), credential, options)
            }),
        )
        .with_credential(spec.credential);
        if spec.credential_hint.is_empty() {
            return account;
        }
        return account.with_hint(spec.credential_hint);
    }
    Account::new(
        ProviderId::new(spec.id),
        account.clone(),
        Box::new(move |account_id, credential, options| {
            (spec.build)(account_id.clone(), credential, options)
        }),
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
    fn a_configured_provider_builds_one_account_per_account_id() {
        let path = scratch_config(
            "zai-accounts",
            "providers = [\"zai\"]\n\n[provider.zai]\naccounts = [\"default\", \"work\"]\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        let accounts = accounts(&secrets(), &config).expect("accounts build");

        assert_eq!(
            accounts
                .iter()
                .map(|account| account.account().as_str().to_owned())
                .collect::<Vec<_>>(),
            ["default", "work"]
        );
        assert!(
            accounts
                .iter()
                .all(|account| account.provider().as_str() == "zai")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_claude_account_reports_its_constructed_account_id() {
        let config = empty_config();
        let account = account(
            claude::PROVIDER_ID,
            &AccountId::new("work"),
            &secrets(),
            &config,
        )
        .expect("builds")
        .expect("Claude account builds");

        assert_eq!(account.account().as_str(), "work");
    }

    #[test]
    fn an_extra_account_forces_oauth_even_when_cli_is_configured() {
        let path = scratch_config(
            "claude-extra-source",
            "providers = [\"claude\"]\n\n[provider.claude]\nsource = \"cli\"\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        assert_eq!(
            source_for_account(claude::PROVIDER_ID, &AccountId::new("work"), &config),
            Source::OAuth
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn every_oauth_provider_publishes_its_available_credentials() {
        let published = catalog(&empty_config());
        for entry in OAUTH {
            let definition = published
                .iter()
                .find(|definition| definition.provider == entry.slug)
                .unwrap_or_else(|| panic!("{} is in the table but not published", entry.slug));
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
            if local_source_available(entry.slug) {
                let external = definition
                    .external
                    .as_ref()
                    .expect("an available local credential is named");
                assert_eq!(external.option, AUTH_SOURCE);
                assert_eq!(external.label, entry.external_label);
                assert_eq!(external.location, entry.external_location);
                assert_eq!(external.command, entry.external_command);
                assert_eq!(external.writes_back, entry.writes_back);
                assert_eq!(
                    choices,
                    [
                        (OAUTH_SOURCE, "Tidemark login"),
                        (CLI_SOURCE, entry.external_label)
                    ],
                    "auto is the unset default, never a choice, for {}",
                    entry.slug
                );
            } else {
                assert_eq!(definition.external, None);
                assert_eq!(choices, [(OAUTH_SOURCE, "Tidemark login")]);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_publishes_antigravity_oauth_and_local_agy() {
        let config = empty_config();
        let definition = catalog(&config)
            .into_iter()
            .find(|definition| definition.provider == antigravity::PROVIDER_ID)
            .expect("Antigravity remains in the catalog");
        let external = definition
            .external
            .as_ref()
            .expect("the local agy source remains published");
        let source = definition
            .options
            .iter()
            .find(|option| option.name == AUTH_SOURCE)
            .expect("the credential selector remains published");

        assert_eq!(definition.credential_kind(), Some(CredentialKind::OAuth));
        assert_eq!(
            definition.credential_hint,
            "Sign in with Google through Tidemark, or read a signed-in agy session."
        );
        assert_eq!(external.option, AUTH_SOURCE);
        assert_eq!(external.label, "agy session");
        assert_eq!(external.command, "agy");
        assert!(!external.writes_back);
        assert_eq!(source.value, AUTO_SOURCE);
        assert_eq!(
            source
                .choices
                .iter()
                .map(|choice| (choice.value.as_str(), choice.title.as_str()))
                .collect::<Vec<_>>(),
            [
                (OAUTH_SOURCE, "Tidemark login"),
                (CLI_SOURCE, "agy session")
            ]
        );
        let built = account(
            antigravity::PROVIDER_ID,
            &AccountId::default(),
            &secrets(),
            &config,
        )
        .expect("account construction succeeds")
        .expect("Antigravity account remains registered");
        assert_eq!(
            built.status().credential_kind(),
            Some(CredentialKind::OAuth)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_publishes_antigravity_as_oauth_only() {
        let path = scratch_config(
            "windows-antigravity-source",
            "providers = [\"antigravity\"]\n\n[provider.antigravity]\nsource = \"cli\"\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        let definition = catalog(&config)
            .into_iter()
            .find(|definition| definition.provider == antigravity::PROVIDER_ID)
            .expect("Antigravity remains in the catalog");
        let source = definition
            .options
            .iter()
            .find(|option| option.name == AUTH_SOURCE)
            .expect("the OAuth source remains published");

        println!(
            "published Antigravity Windows capabilities: credential={}, external={}, choices={:?}",
            definition.credential,
            definition.external.is_some(),
            source
                .choices
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(definition.credential_kind(), Some(CredentialKind::OAuth));
        assert_eq!(definition.external, None);
        assert_eq!(
            definition.credential_hint,
            "Sign in with Google through Tidemark."
        );
        assert_eq!(source.value, OAUTH_SOURCE);
        assert_eq!(source.choices.len(), 1);
        assert_eq!(source.choices[0].value, OAUTH_SOURCE);
        assert_eq!(
            source_for_account(antigravity::PROVIDER_ID, &AccountId::default(), &config),
            Source::OAuth
        );
        assert_eq!(external_present(antigravity::PROVIDER_ID), Some(false));
        assert!(oauth_client(antigravity::PROVIDER_ID).is_some());
        let built = account(
            antigravity::PROVIDER_ID,
            &AccountId::default(),
            &secrets(),
            &config,
        )
        .expect("account construction succeeds")
        .expect("Antigravity account remains registered");
        assert_eq!(
            built.status().credential_kind(),
            Some(CredentialKind::OAuth)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_provider_publishes_external_login_exactly_when_available() {
        // The absent field is the whole signal a client dispatches on: no external login
        // means no credential choice to draw.
        for definition in catalog(&empty_config()) {
            assert_eq!(
                definition.external.is_some(),
                oauth_entry(&definition.provider)
                    .is_some_and(|entry| local_source_available(entry.slug)),
                "{} must publish an external login exactly when it is available",
                definition.provider
            );
        }
    }

    #[test]
    fn cursor_publishes_its_browser_auth_capability_and_stored_selection() {
        let definition = catalog(&empty_config())
            .into_iter()
            .find(|definition| definition.provider == cursor::PROVIDER_ID)
            .expect("Cursor is in the catalog");
        // The session belongs to another application: D-Bus clients must see external,
        // not none, so their authentication semantics stay honest.
        assert_eq!(definition.credential_kind(), Some(CredentialKind::External));
        let selector = definition
            .browser_auth
            .expect("Cursor has local source selection");
        assert_eq!(selector.option, cursor::AUTH_SOURCE);
        assert_eq!(
            selector
                .modes
                .iter()
                .map(|mode| (mode.value.as_str(), mode.title.as_str()))
                .collect::<Vec<_>>(),
            [
                (cursor::CURSOR_APP_SOURCE, "Cursor App"),
                (cursor::BROWSER_SOURCE, "Browser"),
                (keyed::session::PASTE_SOURCE, "Paste session"),
            ]
        );

        let path = scratch_config(
            "cursor-browser-selection",
            "providers = [\"cursor\"]\n\n[provider.cursor]\nauth-source = \"browser\"\nauth-browser = \"zen\"\nauth-profile = \"work\"\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        let account = account(
            cursor::PROVIDER_ID,
            &AccountId::default(),
            &secrets(),
            &config,
        )
        .expect("builds")
        .expect("Cursor account builds");
        assert_eq!(
            account.status().auth_selection,
            Some(tidemark_types::AuthSelection {
                mode: cursor::BROWSER_SOURCE.into(),
                candidate: Some("zen/work".into()),
            })
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn qoder_publishes_browser_auth_and_restores_its_selected_profile() {
        let definition = catalog(&empty_config())
            .into_iter()
            .find(|definition| definition.provider == qoder::PROVIDER_ID)
            .expect("Qoder is in the catalog");
        let selector = definition
            .browser_auth
            .expect("Qoder has local source selection");
        assert_eq!(selector.option, cursor::AUTH_SOURCE);
        assert_eq!(
            selector
                .modes
                .iter()
                .map(|mode| (mode.value.as_str(), mode.title.as_str()))
                .collect::<Vec<_>>(),
            [
                (cursor::BROWSER_SOURCE, "Browser"),
                (keyed::session::PASTE_SOURCE, "Paste session"),
            ]
        );

        let path = scratch_config(
            "qoder-browser-selection",
            "providers = [\"qoder\"]\n\n[provider.qoder]\nauth-source = \"browser\"\nauth-browser = \"firefox\"\nauth-profile = \"Default\"\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        let account = account(
            qoder::PROVIDER_ID,
            &AccountId::default(),
            &secrets(),
            &config,
        )
        .expect("builds")
        .expect("Qoder account builds");
        assert_eq!(
            account.status().auth_selection,
            Some(tidemark_types::AuthSelection {
                mode: cursor::BROWSER_SOURCE.into(),
                candidate: Some("firefox/Default".into()),
            })
        );
        let _ = std::fs::remove_file(path);
    }

    // t3chat is Unix-only for now: wreq depends unconditionally on boring2 (BoringSSL),
    // which does not build on Windows yet — reversible once boring-sys2 does.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn t3chat_publishes_browser_auth_and_restores_its_selected_profile() {
        // Without the selector, the settings dialog cannot write a Firefox choice and the
        // provider necessarily reports NoCredential despite a signed-in browser profile.
        let definition = catalog(&empty_config())
            .into_iter()
            .find(|definition| definition.provider == t3chat::PROVIDER_ID)
            .expect("T3 Chat is in the catalog");
        let selector = definition
            .browser_auth
            .expect("T3 Chat has local source selection");
        assert_eq!(selector.option, cursor::AUTH_SOURCE);
        assert_eq!(
            selector
                .modes
                .iter()
                .map(|mode| (mode.value.as_str(), mode.title.as_str()))
                .collect::<Vec<_>>(),
            [
                (cursor::BROWSER_SOURCE, "Browser"),
                (keyed::session::PASTE_SOURCE, "Paste session"),
            ]
        );

        let path = scratch_config(
            "t3chat-browser-selection",
            "providers = [\"t3chat\"]\n\n[provider.t3chat]\nauth-source = \"browser\"\nauth-browser = \"firefox\"\nauth-profile = \"Default\"\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        let account = account(
            t3chat::PROVIDER_ID,
            &AccountId::default(),
            &secrets(),
            &config,
        )
        .expect("builds")
        .expect("T3 Chat account builds");
        assert_eq!(
            account.status().auth_selection,
            Some(tidemark_types::AuthSelection {
                mode: cursor::BROWSER_SOURCE.into(),
                candidate: Some("firefox/Default".into()),
            })
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn every_browser_session_provider_publishes_a_browser_selector() {
        // A HandSpec with session::OPTIONS cannot work until the selector exposes those
        // options to the settings client, so adding a provider without this registration
        // must be caught here rather than by a person seeing NoCredential.
        for provider in [
            abacus::PROVIDER_ID,
            augment::PROVIDER_ID,
            commandcode::PROVIDER_ID,
            longcat::PROVIDER_ID,
            manus::PROVIDER_ID,
            mimo::PROVIDER_ID,
            mistral::PROVIDER_ID,
            notion::PROVIDER_ID,
            ollama::PROVIDER_ID,
            opencode::PROVIDER_ID,
            perplexity::PROVIDER_ID,
            qoder::PROVIDER_ID,
            sakana::PROVIDER_ID,
            // t3chat is Unix-only for now: wreq depends unconditionally on boring2
            // (BoringSSL), which does not build on Windows yet — reversible once
            // boring-sys2 does.
            #[cfg(not(target_os = "windows"))]
            t3chat::PROVIDER_ID,
            zoommate::PROVIDER_ID,
        ] {
            let definition = catalog(&empty_config())
                .into_iter()
                .find(|definition| definition.provider == provider)
                .expect("browser-session provider is in the catalog");
            let selector = definition
                .browser_auth
                .expect("browser-session provider has local source selection");
            assert_eq!(selector.option, cursor::AUTH_SOURCE, "{provider}");
            assert_eq!(
                selector
                    .modes
                    .iter()
                    .map(|mode| (mode.value.as_str(), mode.title.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (cursor::BROWSER_SOURCE, "Browser"),
                    (keyed::session::PASTE_SOURCE, "Paste session"),
                ],
                "{provider}"
            );
        }
    }

    #[test]
    fn zoommate_publishes_browser_auth_and_restores_its_selected_profile() {
        let definition = catalog(&empty_config())
            .into_iter()
            .find(|definition| definition.provider == zoommate::PROVIDER_ID)
            .expect("ZoomMate is in the catalog");
        let selector = definition
            .browser_auth
            .expect("ZoomMate has local source selection");
        assert_eq!(selector.option, cursor::AUTH_SOURCE);
        assert_eq!(
            selector
                .modes
                .iter()
                .map(|mode| (mode.value.as_str(), mode.title.as_str()))
                .collect::<Vec<_>>(),
            [
                (cursor::BROWSER_SOURCE, "Browser"),
                (keyed::session::PASTE_SOURCE, "Paste session"),
            ]
        );

        let path = scratch_config(
            "zoommate-browser-selection",
            "providers = [\"zoommate\"]\n\n[provider.zoommate]\nauth-source = \"browser\"\nauth-browser = \"firefox\"\nauth-profile = \"Default\"\n",
        );
        let config = Config::at(path.clone()).expect("config reads");
        let account = account(
            zoommate::PROVIDER_ID,
            &AccountId::default(),
            &secrets(),
            &config,
        )
        .expect("builds")
        .expect("ZoomMate account builds");
        assert_eq!(
            account.status().auth_selection,
            Some(tidemark_types::AuthSelection {
                mode: cursor::BROWSER_SOURCE.into(),
                candidate: Some("firefox/Default".into()),
            })
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_credential_choice_reports_a_supported_value() {
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
                source.value,
                supported_source(slug, Source::Auto).as_value(),
                "an unset file resolves only to a source this platform supports"
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
            assert_eq!(source.value, supported_source(slug, Source::Cli).as_value());
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
            let account = account(slug, &AccountId::default(), &secrets(), &config)
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

    fn auth_source_from_status(provider: &str, status: &ProviderStatus) -> Option<String> {
        let source = status
            .options
            .iter()
            .find(|option| option.name == AUTH_SOURCE)
            .map(|option| option.value.as_str());
        auth_source(provider, Source::from_value(source), status)
    }

    #[test]
    fn an_explicit_oauth_source_overrides_probe_answers() {
        let mut status = probed_status(claude::PROVIDER_ID, Some(CLI_SOURCE));
        status.has_credential = Some(false);
        assert_eq!(
            auth_source(claude::PROVIDER_ID, Source::OAuth, &status).as_deref(),
            Some(OAUTH_SOURCE)
        );
    }

    #[test]
    fn claude_says_which_credential_the_next_poll_will_use() {
        for stored in [OAUTH_SOURCE, CLI_SOURCE] {
            // A stored choice wins over whatever the probe found: it is read first.
            let mut status = probed_status(claude::PROVIDER_ID, Some(stored));
            status.has_credential = Some(false);
            assert_eq!(
                auth_source_from_status(claude::PROVIDER_ID, &status).as_deref(),
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
                auth_source_from_status(claude::PROVIDER_ID, &status).as_deref(),
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
                auth_source_from_status(codex::PROVIDER_ID, &status).as_deref(),
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
                auth_source_from_status(codex::PROVIDER_ID, &status).as_deref(),
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
                auth_source_from_status(antigravity::PROVIDER_ID, &status).as_deref(),
                Some(
                    supported_source(antigravity::PROVIDER_ID, Source::from_value(Some(stored)))
                        .as_value()
                )
            );
        }
        for external_present in [Some(true), Some(false), None] {
            let mut status = probed_status(antigravity::PROVIDER_ID, None);
            status.external_present = external_present;
            let expected = if local_source_available(antigravity::PROVIDER_ID)
                && external_present == Some(true)
            {
                CLI_SOURCE
            } else {
                OAUTH_SOURCE
            };
            assert_eq!(
                auth_source_from_status(antigravity::PROVIDER_ID, &status).as_deref(),
                Some(expected),
                "auto uses agy only on a platform where that source is registered"
            );
        }
    }

    #[test]
    fn a_provider_with_one_credential_says_nothing_about_a_source() {
        assert_eq!(
            auth_source_from_status("zai", &probed_status("zai", None)),
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
        build: |_, _, _| Err(ProviderError::Local("not built in a test".into())),
    };

    static KEY_SPEC: keyed::HandSpec = keyed::HandSpec {
        id: "test-keyed",
        title: "Test Keyed",
        credential: CredentialKind::Key,
        credential_hint: "Test console \u{2192} API keys.",
        options: &[],
        build: |_, _, _| Err(ProviderError::Local("not built in a test".into())),
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
        let account = hand_written_account(&KEYLESS_SPEC, &AccountId::default());
        assert_eq!(
            account.status().credential.as_deref(),
            Some(CredentialKind::None.as_wire())
        );
        assert_eq!(account.status().credential_hint, None);
    }

    #[test]
    // The Unix catalog is 58 providers; Windows excludes t3chat (its HTTP stack needs
    // boring2, which does not build there — see the SPEC table above), so the count and
    // the register-what-Linux-registers premise are unix-build facts, not defects.
    #[cfg(not(target_os = "windows"))]
    fn the_catalog_exists_even_when_no_account_is_configured() {
        let config = empty_config();
        assert!(
            accounts(&secrets(), &config)
                .expect("accounts build")
                .is_empty()
        );
        let definitions = catalog(&config);
        assert_eq!(definitions.len(), 58);
        assert_eq!(definitions[0].provider, "antigravity");
        assert_eq!(definitions[0].credential, CredentialKind::OAuth.as_wire());
        assert_eq!(
            definitions[0]
                .external
                .as_ref()
                .map(|external| external.label.as_str()),
            local_source_available(antigravity::PROVIDER_ID).then_some("agy session")
        );
        assert_eq!(
            definitions[0].credential_hint,
            credential_hint(oauth_entry(antigravity::PROVIDER_ID).expect("registered"))
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

        // A published option shows what is on disk verbatim; the provider's own URL
        // resolution is what keeps an unrecognised value from reaching the wrong host, so
        // an unrecognised value on disk is not silently rewritten here.
        std::fs::write(&path, "[provider.zai]\nregion = \"mars\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert_eq!(options(zai::PROVIDER_ID, &config)[0].value, "mars");
        let wrong_host = zai::Zai::new(
            Credential::new("key"),
            &BTreeMap::from([("region".to_owned(), "mars".to_owned())]),
        )
        .expect("builds");
        let no_region = zai::Zai::new(Credential::new("key"), &BTreeMap::new()).expect("builds");
        assert_eq!(
            (wrong_host.quota_url(), wrong_host.balance_url()),
            (no_region.quota_url(), no_region.balance_url()),
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
                account(spec.id, &AccountId::default(), &secrets(), &config)
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
        let built =
            account("zai", &AccountId::default(), &secrets(), &empty_config()).expect("no error");
        assert!(built.is_some(), "a slug in keyed::CATALOG must build");
    }

    #[test]
    fn a_slug_no_build_supports_is_still_not_an_account() {
        assert!(
            account(
                "nonesuch",
                &AccountId::default(),
                &secrets(),
                &empty_config()
            )
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
