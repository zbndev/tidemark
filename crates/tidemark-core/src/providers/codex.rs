//! Codex subscription quota over the OAuth credentials owned by the Codex CLI.
//!
//! # The trap this provider exists to demonstrate
//!
//! `rate_limit.primary_window` and `secondary_window` are **slots, not durations**. On
//! 2026-08-19 this account's only window arrived in the slot named *primary* carrying
//! `limit_window_seconds: 604800` — a weekly window — with `secondary_window: null`;
//! earlier the same account had reported its weekly figures under *secondary*. Anything
//! keyed on the slot name splits one continuous window in two and fabricates
//! appeared/disappeared events, so every window here is keyed on its declared length. A
//! window that does not declare one has nothing left to key on and fails the response
//! rather than being filed under a slot name. See `CONTEXT.md` § Storage.
//!
//! # Three pools, one shape
//!
//! The account's own `rate_limit`, an optional `code_review_rate_limit`, and each entry of
//! `additional_rate_limits[]` all carry the same primary/secondary pair. They are different
//! *pools* rather than different lengths of one pool, so each keeps its own key prefix and
//! a weekly window in one cannot collide with a weekly window in another.

use super::{BoxFuture, Credential, Provider, ProviderError, http, length_title, title_case};
use crate::oauth;
use crate::oauth_file::{
    CredentialFile, CredentialFileError, Field, LockedCredentialFile, UpdateOutcome,
};
use crate::secrets::{self, Secrets};
use base64::Engine;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "codex";

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_SCOPE: &str = "openid profile email";

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// `offline_access` is what makes the response carry a refresh token; without it a login
/// works once and then expires with nothing to renew it.
const OAUTH_SCOPES: &str = "openid profile email offline_access";
/// Fixed, because the client is registered with exactly this redirect. See ADR 0003.
const REDIRECT_PORT: u16 = 1_455;
const REDIRECT_PATH: &str = "/auth/callback";

/// This provider's OAuth client, for the loopback flow in [`crate::oauth`].
pub fn oauth_client() -> oauth::Client {
    oauth::Client {
        authorize_url: AUTHORIZE_URL,
        token_url: REFRESH_URL,
        client_id: OAUTH_CLIENT_ID,
        client_secret: None,
        redirect_port: REDIRECT_PORT,
        redirect_path: REDIRECT_PATH,
        scopes: OAUTH_SCOPES,
        // Puts the organisation and account claims in the id token, which is where the
        // `chatgpt-account-id` this provider sends on every request comes from.
        authorize_extras: &[("id_token_add_organizations", "true")],
        // The code exchange is form-encoded even though the refresh grant beside it is
        // JSON. Two spellings at one endpoint; see [`Encoding`].
        encoding: oauth::Encoding::Form,
    }
}

/// The credential document to store after a successful login, in the shape the CLI writes.
pub fn document_from_login(
    response: &serde_json::Value,
) -> Result<serde_json::Value, ProviderError> {
    let tokens: RefreshResponse = serde_json::from_value(response.clone()).map_err(|error| {
        ProviderError::malformed(format!("the Codex login response is not readable: {error}"))
    })?;
    if tokens.access_token.trim().is_empty() {
        return Err(ProviderError::malformed(
            "the Codex login response carried a blank access token",
        ));
    }
    let mut subtree = serde_json::Map::new();
    subtree.insert("access_token".into(), tokens.access_token.into());
    if let Some(refresh_token) = nonblank(tokens.refresh_token.as_deref()) {
        subtree.insert("refresh_token".into(), refresh_token.into());
    }
    if let Some(id_token) = nonblank(tokens.id_token.as_deref()) {
        subtree.insert("id_token".into(), id_token.into());
    }
    Ok(serde_json::json!({
        TOKEN_SUBTREE: subtree,
        "last_refresh": now_rfc3339(),
    }))
}

fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// How close to its own expiry an access token is refreshed rather than spent.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// The subtree of `auth.json` the tokens live in.
const TOKEN_SUBTREE: &str = "tokens";

/// One Codex CLI account.
#[derive(Debug)]
pub struct Codex {
    client: reqwest::Client,
    credentials: CredentialFile,
    /// Where a login performed from Tidemark is kept, when the caller has somewhere to
    /// keep one.
    own: Option<Arc<dyn Secrets>>,
    usage_url: String,
    refresh_url: String,
}

impl Codex {
    /// Builds the canonical Codex account at `$CODEX_HOME/auth.json`, or `~/.codex`.
    pub fn new(own: Option<Arc<dyn Secrets>>) -> Result<Self, ProviderError> {
        let path = auth_path()?;
        let mut codex = Self::with_endpoints(
            CredentialFile::new(path.clone(), path),
            USAGE_URL.to_owned(),
            REFRESH_URL.to_owned(),
        )?;
        codex.own = own;
        Ok(codex)
    }

    fn with_endpoints(
        credentials: CredentialFile,
        usage_url: String,
        refresh_url: String,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credentials,
            own: None,
            usage_url,
            refresh_url,
        })
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let mut credentials = self.load(Timestamp::now().as_unix()).await?;
        let mut refreshed = credentials.was_refreshed;

        loop {
            let mut request = self
                .client
                .get(&self.usage_url)
                .bearer_auth(credentials.access_token.expose())
                .header(reqwest::header::ACCEPT, "application/json");
            if let Some(account_id) = credentials.account_id.as_deref() {
                request = request.header("chatgpt-account-id", account_id);
            }
            let response = request.send().await.map_err(ProviderError::Transport)?;
            let status = response.status();
            // An access token whose expiry we could not read — an opaque token, or one
            // whose claims we do not recognise — announces its own death this way and no
            // other. One refresh, one retry; a second rejection is the user's to fix.
            if !refreshed
                && matches!(
                    status,
                    reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
                )
                && credentials.refresh_token.is_some()
            {
                refreshed = true;
                credentials = self.force_refresh().await?;
                continue;
            }
            let retry_after = http::retry_after_header(&response).map(str::to_owned);
            http::check(status, retry_after.as_deref())?;
            let body = response.text().await.map_err(ProviderError::Transport)?;
            return parse(&body, Timestamp::now());
        }
    }

    /// Reads the credential, refreshing first when the token says it is spent.
    ///
    /// A Tidemark login wins over the CLI file when there is one; see the module docs.
    async fn load(&self, now: i64) -> Result<CodexCredentials, ProviderError> {
        if let Some(stored) = self.own_document().await? {
            return self.own_credentials(stored, now, false).await;
        }
        let locked = self.credentials.lock().map_err(map_file_error)?;
        let document = locked.read_json().map_err(map_file_error)?;
        let credentials = CodexCredentials::from_document(&document)?;
        if credentials.is_expired_at(now) {
            return self.refresh(&locked, credentials).await;
        }
        Ok(credentials)
    }

    /// Refreshes whatever is currently stored, regardless of what its expiry claims.
    ///
    /// This is the path an opaque access token takes: nothing local could have predicted
    /// its death, so the provider's 401 is the announcement and this is the response.
    async fn force_refresh(&self) -> Result<CodexCredentials, ProviderError> {
        if let Some(stored) = self.own_document().await? {
            return self
                .own_credentials(stored, Timestamp::now().as_unix(), true)
                .await;
        }
        let locked = self.credentials.lock().map_err(map_file_error)?;
        let document = locked.read_json().map_err(map_file_error)?;
        let credentials = CodexCredentials::from_document(&document)?;
        self.refresh(&locked, credentials).await
    }

    /// The document of a login performed from Tidemark, if there is one.
    async fn own_document(&self) -> Result<Option<serde_json::Value>, ProviderError> {
        let Some(own) = &self.own else {
            return Ok(None);
        };
        let stored = own
            .get(
                secrets::Kind::Token,
                &ProviderId::new(PROVIDER_ID),
                &AccountId::default(),
            )
            .await
            .map_err(ProviderError::from_secret_error)?;
        stored
            .map(|stored| {
                serde_json::from_str(stored.expose()).map_err(|error| {
                    ProviderError::malformed(format!("the stored Codex login is not JSON: {error}"))
                })
            })
            .transpose()
    }

    /// A Tidemark login, refreshed straight back into the Secret Service when it is spent.
    ///
    /// None of the file protocol applies and none of it is performed: nothing else writes
    /// these bytes. The new document is stored before it is used, so a rotation that
    /// succeeded at the provider is never lost to a failure on the way back.
    async fn own_credentials(
        &self,
        mut document: serde_json::Value,
        now: i64,
        force: bool,
    ) -> Result<CodexCredentials, ProviderError> {
        let credentials = CodexCredentials::from_document(&document)?;
        if !force && !credentials.is_expired_at(now) {
            return Ok(credentials);
        }
        let refresh_token = credentials
            .refresh_token
            .as_ref()
            .ok_or(ProviderError::Credential { status: 401 })?
            .expose()
            .to_owned();
        let refreshed = self.exchange_refresh(&refresh_token).await?;
        crate::oauth_file::apply_fields(
            &mut document,
            TOKEN_SUBTREE,
            &refreshed_fields(&refreshed, &refresh_token),
        )
        .map_err(map_file_error)?;
        let own = self
            .own
            .as_ref()
            .expect("only reached with a store in hand");
        own.set(
            secrets::Kind::Token,
            &ProviderId::new(PROVIDER_ID),
            &AccountId::default(),
            &Credential::new(document.to_string()),
        )
        .await
        .map_err(ProviderError::from_secret_error)?;
        let mut current = CodexCredentials::from_document(&document)?;
        current.was_refreshed = true;
        Ok(current)
    }

    /// The refresh grant itself, shared by both credential sources.
    async fn exchange_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<RefreshResponse, ProviderError> {
        let response = self
            .client
            .post(&self.refresh_url)
            .json(&serde_json::json!({
                "client_id": OAUTH_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "scope": REFRESH_SCOPE,
            }))
            .send()
            .await
            .map_err(ProviderError::Transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST
            || status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(ProviderError::Credential {
                status: status.as_u16(),
            });
        }
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(status, retry_after.as_deref())?;
        let refreshed: RefreshResponse = response.json().await.map_err(|error| {
            ProviderError::malformed(format!("Codex refresh response is not readable: {error}"))
        })?;
        if refreshed.access_token.trim().is_empty() {
            return Err(ProviderError::malformed(
                "Codex refresh response carried a blank access token",
            ));
        }
        Ok(refreshed)
    }

    async fn refresh(
        &self,
        locked: &LockedCredentialFile,
        credentials: CodexCredentials,
    ) -> Result<CodexCredentials, ProviderError> {
        let refresh_token = credentials
            .refresh_token
            .as_ref()
            .ok_or(ProviderError::Credential { status: 401 })?;
        let expected_refresh_token = refresh_token.expose().to_owned();
        locked
            .preflight_unique_fields(
                TOKEN_SUBTREE,
                &[
                    Field::Subtree("access_token"),
                    Field::Subtree("refresh_token"),
                ],
            )
            .map_err(map_file_error)?;
        // OpenAI rotates the refresh token, so this exchange is one-way. Preserve the
        // exact CLI-owned bytes before crossing it. See ADR 0001.
        locked.backup().map_err(map_file_error)?;
        let refreshed = self.exchange_refresh(&expected_refresh_token).await?;

        let outcome = locked
            .update_top_level(
                TOKEN_SUBTREE,
                ("refresh_token", &expected_refresh_token),
                &refreshed_fields(&refreshed, &expected_refresh_token),
            )
            .map_err(map_file_error)?;
        let current = locked.read_json().map_err(map_file_error)?;
        let mut current = CodexCredentials::from_document(&current)?;
        if outcome == UpdateOutcome::SourceChanged
            && current.is_expired_at(Timestamp::now().as_unix())
        {
            return Err(ProviderError::Local(
                "Codex credentials changed during refresh but are still expired".into(),
            ));
        }
        current.was_refreshed = true;
        Ok(current)
    }
}

impl Provider for Codex {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// Where the Codex CLI keeps its credentials.
fn auth_path() -> Result<PathBuf, ProviderError> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|home| Path::new(home).is_absolute())
    {
        return Ok(Path::new(&home).join("auth.json"));
    }
    let home = std::env::var_os("HOME")
        .filter(|home| Path::new(home).is_absolute())
        .ok_or_else(|| ProviderError::Local("HOME does not name an absolute directory".into()))?;
    Ok(Path::new(&home).join(".codex/auth.json"))
}

/// Turns a usage response into a snapshot.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a Codex usage response: {error}"))
    })?;

    let mut windows = Vec::new();
    pool_windows(
        envelope.rate_limit.as_ref(),
        None,
        captured_at,
        &mut windows,
    )?;
    // Not observed non-null on this account, but it is the same shape in the same
    // response, and a limit we skipped would read as a limit that does not exist.
    pool_windows(
        envelope.code_review_rate_limit.as_ref(),
        Some(&Pool {
            key: "code_review".to_owned(),
            title: "Code review".to_owned(),
        }),
        captured_at,
        &mut windows,
    )?;
    for entry in envelope.additional_rate_limits.iter().flatten() {
        let extra: AdditionalRateLimit =
            serde_json::from_value(entry.clone()).map_err(|error| {
                ProviderError::malformed(format!("an extra rate limit is not readable: {error}"))
            })?;
        let name = extra
            .limit_name
            .as_deref()
            .or(extra.metered_feature.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                // Without a name there is nothing to separate this pool from the account's
                // own, and two pools sharing one key merge two histories into one.
                ProviderError::malformed("an extra rate limit arrived with no name")
            })?;
        // Keyed on the metered feature and titled with the display name: the label is the
        // vendor's copy and can be reworded between responses, while the feature slug is
        // what the limit *is*. A key that follows the wording would split one pool's
        // history the first time marketing renamed a model.
        let key = extra
            .metered_feature
            .as_deref()
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .unwrap_or(name);
        pool_windows(
            extra.rate_limit.as_ref(),
            Some(&Pool {
                key: key.to_owned(),
                title: name.to_owned(),
            }),
            captured_at,
            &mut windows,
        )?;
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: details(&envelope),
    })
}

/// One quota pool's identity: what its windows are keyed under and called.
struct Pool {
    key: String,
    title: String,
}

/// Appends both slots of one `rate_limit` object, in the order they are reported.
fn pool_windows(
    rate_limit: Option<&serde_json::Value>,
    pool: Option<&Pool>,
    captured_at: Timestamp,
    windows: &mut Vec<Window>,
) -> Result<(), ProviderError> {
    let Some(rate_limit) = rate_limit.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    for slot in ["primary_window", "secondary_window"] {
        let Some(entry) = rate_limit.get(slot).filter(|value| !value.is_null()) else {
            continue;
        };
        let snapshot: WindowSnapshot = serde_json::from_value(entry.clone()).map_err(|error| {
            ProviderError::malformed(format!("a {slot} is not readable: {error}"))
        })?;
        windows.push(snapshot.window(pool, captured_at)?);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<serde_json::Value>,
    #[serde(default)]
    code_review_rate_limit: Option<serde_json::Value>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    credits: Option<Credits>,
    #[serde(default)]
    spend_control: Option<SpendControl>,
    #[serde(default)]
    rate_limit_reset_credits: Option<ResetCredits>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimit {
    #[serde(default)]
    limit_name: Option<String>,
    #[serde(default)]
    metered_feature: Option<String>,
    #[serde(default)]
    rate_limit: Option<serde_json::Value>,
}

/// One window as this endpoint reports it.
///
/// `limit_window_seconds` is required on purpose: it is the whole identity of the window
/// here, and a window without it could only be filed under the slot it arrived in.
#[derive(Debug, Deserialize)]
struct WindowSnapshot {
    used_percent: f64,
    limit_window_seconds: u64,
    #[serde(default)]
    reset_after_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
}

impl WindowSnapshot {
    fn window(&self, pool: Option<&Pool>, captured_at: Timestamp) -> Result<Window, ProviderError> {
        let length = WindowLength::from_secs(self.limit_window_seconds)
            .ok_or_else(|| ProviderError::malformed("a window declared a zero-second length"))?;
        let key = match pool {
            Some(pool) => WindowKey::for_pool(&pool.key, length),
            None => WindowKey::for_length(length),
        };
        let span = length_title(length);
        let title = match pool {
            Some(pool) => format!("{} · {span}", pool.title),
            None => span,
        };
        // `reset_at` is absolute and preferred. Where it is absent or absurd — a zero has
        // been seen in this family of payloads — the countdown beside it still says when
        // the window rolls over, and a window with a pace mark beats one without.
        let resets_at = self
            .reset_at
            .and_then(|seconds| Timestamp::from_unix(seconds).ok())
            .or_else(|| {
                self.reset_after_seconds
                    .filter(|seconds| *seconds >= 0)
                    .map(|seconds| captured_at.saturating_add_seconds(seconds))
            });
        Ok(Window {
            key,
            title,
            used_percent: self.used_percent.clamp(0.0, 100.0),
            resets_at,
            length: Some(length),
        })
    }
}

#[derive(Debug, Deserialize)]
struct Credits {
    #[serde(default)]
    has_credits: bool,
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    balance: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SpendControl {
    #[serde(default)]
    individual_limit: Option<IndividualLimit>,
}

#[derive(Debug, Deserialize)]
struct IndividualLimit {
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    used: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ResetCredits {
    #[serde(default)]
    available_count: i64,
    #[serde(default)]
    applicable_available_count: i64,
}

fn details(envelope: &Envelope) -> Vec<DetailSection> {
    let mut sections = Vec::new();

    if let Some(plan) = envelope.plan_type.as_deref().map(title_case) {
        sections.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Subscription".to_owned(),
                value: plan,
            }],
        });
    }

    if let Some(credits) = envelope
        .credits
        .as_ref()
        .filter(|credits| credits.has_credits || credits.unlimited)
    {
        let value = if credits.unlimited {
            "Unlimited".to_owned()
        } else {
            number_text(credits.balance.as_ref()).unwrap_or_else(|| "0".to_owned())
        };
        sections.push(DetailSection {
            title: "Credits".to_owned(),
            rows: vec![DetailRow {
                label: "Balance".to_owned(),
                value,
            }],
        });
    }

    // Credits that buy back an exhausted window. Worth a row when there are any; a row
    // saying zero is one more number to read for no decision it could change.
    if let Some(reset) = envelope
        .rate_limit_reset_credits
        .as_ref()
        .filter(|reset| reset.available_count > 0)
    {
        let mut rows = vec![DetailRow {
            label: "Available".to_owned(),
            value: reset.available_count.to_string(),
        }];
        if reset.applicable_available_count != reset.available_count {
            rows.push(DetailRow {
                label: "Usable now".to_owned(),
                value: reset.applicable_available_count.to_string(),
            });
        }
        sections.push(DetailSection {
            title: "Reset credits".to_owned(),
            rows,
        });
    }

    if let Some((used, limit)) = envelope
        .spend_control
        .as_ref()
        .and_then(|control| control.individual_limit.as_ref())
        .and_then(|limit| Some((limit.used?, limit.limit?)))
    {
        sections.push(DetailSection {
            title: "Spend".to_owned(),
            rows: vec![DetailRow {
                label: "Used".to_owned(),
                value: format!("{} of {}", trim_number(used), trim_number(limit)),
            }],
        });
    }

    sections
}

/// A balance as the provider sent it — it arrives as a string on this endpoint and as a
/// number elsewhere, and neither form is worth reformatting.
fn number_text(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) => Some(text.trim().to_owned()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn trim_number(value: f64) -> String {
    let rendered = format!("{value:.2}");
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// The tokens as `auth.json` holds them.
struct CodexCredentials {
    access_token: Credential,
    refresh_token: Option<Credential>,
    account_id: Option<String>,
    /// When the access token says it expires, where it says so at all.
    expires_at: Option<i64>,
    /// True when this value came back from a refresh this poll performed.
    was_refreshed: bool,
}

impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("access_token", &self.access_token)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("account_id", &self.account_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl CodexCredentials {
    fn from_document(document: &serde_json::Value) -> Result<Self, ProviderError> {
        let subtree = document
            .get(TOKEN_SUBTREE)
            .filter(|value| !value.is_null())
            // An `auth.json` holding only `OPENAI_API_KEY` is a working Codex login with
            // no subscription token in it, and this endpoint answers only to the latter.
            .ok_or(ProviderError::NoCredential)?;
        let raw: RawCredentials = serde_json::from_value(subtree.clone()).map_err(|error| {
            ProviderError::malformed(format!("tokens is not readable: {error}"))
        })?;
        let access_token = Credential::new(raw.access_token);
        if access_token.is_blank() {
            return Err(ProviderError::NoCredential);
        }
        let account_id = raw
            .account_id
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .or_else(|| {
                account_id_from_claims(raw.id_token.as_deref())
                    .or_else(|| account_id_from_claims(Some(access_token.expose())))
            });
        Ok(Self {
            expires_at: expiry_from_claims(access_token.expose()),
            refresh_token: raw
                .refresh_token
                .map(Credential::new)
                .filter(|token| !token.is_blank()),
            access_token,
            account_id,
            was_refreshed: false,
        })
    }

    /// True when the token says it is spent. A token that does not say is not assumed to
    /// be either: the 401 retry in [`Codex::fetch_inner`] covers it.
    fn is_expired_at(&self, now: i64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| now >= expires_at - EXPIRY_MARGIN_SECS)
    }
}

#[derive(Deserialize)]
struct RawCredentials {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

/// The fields a rotation replaces, addressed where the CLI keeps them.
///
/// `last_refresh` sits at the document root beside the subtree rather than inside it, and
/// it is the timestamp of the credential material: leaving it stale after writing new
/// tokens would be a lie about bytes we just replaced.
fn refreshed_fields<'a>(
    response: &'a RefreshResponse,
    previous_refresh_token: &'a str,
) -> Vec<(Field<'a>, serde_json::Value)> {
    let refresh_token = response
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .unwrap_or(previous_refresh_token);
    let mut fields = vec![
        (
            Field::Subtree("access_token"),
            response.access_token.clone().into(),
        ),
        (Field::Subtree("refresh_token"), refresh_token.into()),
        (Field::Root("last_refresh"), now_rfc3339().into()),
    ];
    if let Some(id_token) = response
        .id_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        fields.push((Field::Subtree("id_token"), id_token.into()));
    }
    fields
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// The `exp` claim of a JWT access token, in Unix seconds.
///
/// The signature is neither checked nor checkable here — the claim is read to decide
/// whether to spend a request, never to decide whether to trust the token. The server
/// remains the only authority on that, which is why a rejection still triggers a refresh.
fn expiry_from_claims(token: &str) -> Option<i64> {
    claims(token)?.get("exp")?.as_i64()
}

fn account_id_from_claims(token: Option<&str>) -> Option<String> {
    let claims = claims(token?)?;
    let auth = claims.get("https://api.openai.com/auth")?;
    let id = auth.get("chatgpt_account_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

fn claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn map_file_error(error: CredentialFileError) -> ProviderError {
    match error {
        CredentialFileError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
            ProviderError::NoCredential
        }
        CredentialFileError::Json(_)
        | CredentialFileError::RootNotObject
        | CredentialFileError::MissingSubtree(_) => ProviderError::malformed(error.to_string()),
        other => ProviderError::Local(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestHome {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TestHome {
        /// A copy of the real `auth.json` shape whose access token expired in 2001.
        fn expired() -> Self {
            let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tidemark-codex-test-{}-{serial}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).expect("test directory");
            let path = dir.join("auth.json");
            fs::write(
                &path,
                include_bytes!("../../tests/fixtures/codex-auth.json"),
            )
            .expect("write fixture");
            Self { dir, path }
        }

        fn provider(&self, base: &str) -> Codex {
            Codex::with_endpoints(
                CredentialFile::new(self.path.clone(), self.path.clone()),
                format!("{base}/usage"),
                format!("{base}/token"),
            )
            .expect("provider builds")
        }

        fn document(&self) -> serde_json::Value {
            serde_json::from_slice(&fs::read(&self.path).expect("auth readable"))
                .expect("auth is JSON")
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A Secret Service that is one string, held in memory. See the twin in
    /// `crate::providers::claude`.
    #[derive(Debug, Default)]
    struct FakeSecrets(std::sync::Mutex<Option<String>>);

    impl FakeSecrets {
        fn holding(document: serde_json::Value) -> Arc<Self> {
            Arc::new(Self(std::sync::Mutex::new(Some(document.to_string()))))
        }

        fn stored(&self) -> serde_json::Value {
            let held = self.0.lock().expect("no test panics holding this");
            serde_json::from_str(held.as_deref().expect("something stored")).expect("JSON")
        }
    }

    impl crate::secrets::Secrets for FakeSecrets {
        fn get<'a>(
            &'a self,
            _kind: crate::secrets::Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> BoxFuture<'a, Result<Option<Credential>, crate::secrets::SecretError>> {
            let held = self
                .0
                .lock()
                .expect("no test panics holding this")
                .clone()
                .map(Credential::new);
            Box::pin(async move { Ok(held) })
        }

        fn set<'a>(
            &'a self,
            _kind: crate::secrets::Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
            secret: &'a Credential,
        ) -> BoxFuture<'a, Result<(), crate::secrets::SecretError>> {
            *self.0.lock().expect("no test panics holding this") = Some(secret.expose().to_owned());
            Box::pin(async { Ok(()) })
        }

        fn compare_and_set<'a>(
            &'a self,
            _kind: crate::secrets::Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
            expected: &'a Credential,
            replacement: &'a Credential,
        ) -> BoxFuture<'a, Result<bool, crate::secrets::SecretError>> {
            let mut held = self.0.lock().expect("no test panics holding this");
            let matches = held.as_deref() == Some(expected.expose());
            if matches {
                *held = Some(replacement.expose().to_owned());
            }
            Box::pin(async move { Ok(matches) })
        }

        fn delete<'a>(
            &'a self,
            _kind: crate::secrets::Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> BoxFuture<'a, Result<(), crate::secrets::SecretError>> {
            *self.0.lock().expect("no test panics holding this") = None;
            Box::pin(async { Ok(()) })
        }
    }

    /// A loopback server answering a fixed script of `(status, body)` in order.
    fn local_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (requests_tx, requests_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, response_body) in responses {
                let (mut stream, _) = listener.accept().expect("request accepted");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).expect("request read");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    let Some(headers_end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
                requests_tx
                    .send(String::from_utf8(request).expect("request is text"))
                    .expect("request captured");
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .expect("response written");
            }
        });
        (format!("http://{address}"), requests_rx, handle)
    }

    const REFRESH: (u16, &str) = (
        200,
        r#"{"access_token":"new-access","refresh_token":"new-refresh",
            "id_token":"new-id","token_type":"bearer","expires_in":864000}"#,
    );
    const USAGE: (u16, &str) = (
        200,
        r#"{"plan_type":"plus","rate_limit":{"primary_window":
            {"used_percent":19,"limit_window_seconds":604800,"reset_at":1787855484}}}"#,
    );

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn an_expired_token_is_rotated_persisted_and_then_used_for_quota() {
        let home = TestHome::expired();
        let before = fs::read(&home.path).expect("fixture readable");
        let (base, requests, server) = local_server(vec![REFRESH, USAGE]);
        let provider = home.provider(&base);

        let snapshot = block_on(provider.fetch_inner()).expect("refresh and fetch succeed");

        assert_eq!(snapshot.windows[0].used_percent, 19.0);
        let refresh_request = requests.recv().expect("refresh request captured");
        assert!(
            refresh_request.starts_with("POST /token "),
            "{refresh_request}"
        );
        assert!(
            refresh_request.contains("user-agent: Tidemark/"),
            "{refresh_request}"
        );
        assert!(
            refresh_request.contains("\"grant_type\":\"refresh_token\""),
            "{refresh_request}"
        );
        assert!(
            refresh_request.contains("\"refresh_token\":\"fixture-old-refresh\""),
            "{refresh_request}"
        );
        assert!(
            refresh_request.contains(OAUTH_CLIENT_ID),
            "{refresh_request}"
        );
        let usage_request = requests.recv().expect("usage request captured");
        assert!(usage_request.starts_with("GET /usage "), "{usage_request}");
        assert!(
            usage_request.contains("authorization: Bearer new-access"),
            "{usage_request}"
        );
        assert!(
            usage_request.contains("chatgpt-account-id: 00000000-0000-4000-8000-000000000000"),
            "{usage_request}"
        );
        server.join().expect("server stopped");

        let after = home.document();
        assert_eq!(after["tokens"]["access_token"], "new-access");
        assert_eq!(after["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(after["tokens"]["id_token"], "new-id");
        assert_eq!(
            after["tokens"]["account_id"], "00000000-0000-4000-8000-000000000000",
            "an unrelated token field was rewritten"
        );
        assert_eq!(after["auth_mode"], "chatgpt");
        assert!(after["OPENAI_API_KEY"].is_null());
        assert_ne!(
            after["last_refresh"], "2026-01-02T03:04:05.000000000Z",
            "the timestamp of the credential material stayed stale"
        );

        let backup = home.path.with_file_name("auth.json.tidemark-backup");
        assert_eq!(fs::read(&backup).expect("backup readable"), before);
        assert_eq!(
            fs::metadata(backup)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn a_token_the_provider_rejects_is_refreshed_once_and_the_request_retried() {
        // The access token's own claims can be unreadable — an opaque token, or claims we
        // do not recognise. A rejection is then the only signal there is.
        let home = TestHome::expired();
        fs::write(
            &home.path,
            serde_json::to_vec_pretty(&json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "opaque-old",
                    "refresh_token": "fixture-old-refresh",
                    "account_id": "fixture-account"
                }
            }))
            .expect("serialize"),
        )
        .expect("write opaque credentials");
        let (base, requests, server) =
            local_server(vec![(401, r#"{"detail":"expired"}"#), REFRESH, USAGE]);
        let provider = home.provider(&base);

        let snapshot = block_on(provider.fetch_inner()).expect("the retry succeeds");

        assert_eq!(snapshot.windows.len(), 1);
        let first = requests.recv().expect("first usage request");
        assert!(
            first.contains("authorization: Bearer opaque-old"),
            "{first}"
        );
        let refresh = requests.recv().expect("refresh request");
        assert!(refresh.starts_with("POST /token "), "{refresh}");
        let retry = requests.recv().expect("retried usage request");
        assert!(
            retry.contains("authorization: Bearer new-access"),
            "{retry}"
        );
        server.join().expect("server stopped");
    }

    #[test]
    fn a_second_rejection_is_the_users_to_fix_rather_than_an_endless_retry() {
        let home = TestHome::expired();
        let (base, _requests, server) = local_server(vec![REFRESH, (401, "{}")]);
        let provider = home.provider(&base);

        let error = block_on(provider.fetch_inner()).expect_err("a rejection is reported");

        assert!(
            matches!(error, ProviderError::Credential { status: 401 }),
            "{error:?}"
        );
        server.join().expect("server stopped");
    }

    #[test]
    fn expiry_is_read_from_the_tokens_own_claims() {
        let document = serde_json::from_slice::<serde_json::Value>(include_bytes!(
            "../../tests/fixtures/codex-auth.json"
        ))
        .expect("fixture JSON");
        let credentials = CodexCredentials::from_document(&document).expect("credentials parse");

        assert_eq!(credentials.expires_at, Some(1_000_003_600));
        assert!(credentials.is_expired_at(1_000_003_600 - EXPIRY_MARGIN_SECS));
        assert!(!credentials.is_expired_at(1_000_003_600 - EXPIRY_MARGIN_SECS - 1));
    }

    #[test]
    fn a_token_that_does_not_say_when_it_expires_is_not_assumed_to_be_spent() {
        let document = json!({"tokens": {"access_token": "opaque", "refresh_token": "r"}});
        let credentials = CodexCredentials::from_document(&document).expect("credentials parse");

        assert_eq!(credentials.expires_at, None);
        assert!(!credentials.is_expired_at(i64::MAX / 2));
    }

    #[test]
    fn an_account_id_missing_from_the_file_is_recovered_from_the_token_claims() {
        let mut document = serde_json::from_slice::<serde_json::Value>(include_bytes!(
            "../../tests/fixtures/codex-auth.json"
        ))
        .expect("fixture JSON");
        document["tokens"]
            .as_object_mut()
            .expect("tokens object")
            .remove("account_id");

        let credentials = CodexCredentials::from_document(&document).expect("credentials parse");

        assert_eq!(
            credentials.account_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000000")
        );
    }

    #[test]
    fn an_auth_file_holding_only_an_api_key_has_no_subscription_credential() {
        // A working Codex login all the same — but this endpoint answers only to the
        // subscription token, and saying "no credential" is what the interface can act on.
        let document = json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-fixture"});

        assert!(matches!(
            CodexCredentials::from_document(&document),
            Err(ProviderError::NoCredential)
        ));
    }

    #[test]
    fn a_refresh_that_returns_no_new_refresh_token_keeps_the_one_that_worked() {
        let response = RefreshResponse {
            access_token: "new-access".to_owned(),
            refresh_token: None,
            id_token: None,
        };

        let fields = refreshed_fields(&response, "old-refresh");

        let by_name: Vec<(&str, &serde_json::Value)> = fields
            .iter()
            .map(|(field, value)| (field.name(), value))
            .collect();
        assert_eq!(by_name[1], ("refresh_token", &json!("old-refresh")));
        assert!(
            !by_name.iter().any(|(name, _)| *name == "id_token"),
            "an absent id token must not be written as one"
        );
        assert_eq!(
            fields
                .iter()
                .find(|(field, _)| matches!(field, Field::Root("last_refresh")))
                .map(|(_, value)| value.is_string()),
            Some(true)
        );
    }

    #[test]
    fn a_login_performed_here_is_used_ahead_of_the_cli_file() {
        // An access token whose `exp` claim is in 2286, so nothing wants refreshing.
        const LIVE: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjk5OTk5OTk5OTl9.";
        let home = TestHome::expired();
        let before = fs::read(&home.path).expect("fixture readable");
        let secrets = FakeSecrets::holding(json!({
            "tokens": {"access_token": LIVE, "refresh_token": "own-refresh", "account_id": "acct-own"},
            "last_refresh": "2026-08-21T00:00:00Z"
        }));
        let (base, requests, server) = local_server(vec![USAGE]);
        let mut provider = home.provider(&base);
        provider.own = Some(Arc::clone(&secrets) as Arc<dyn crate::secrets::Secrets>);

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("the stored login is enough");
        assert_eq!(snapshot.windows.len(), 1);

        let usage_request = requests.recv().expect("one request");
        assert!(
            usage_request.contains(&format!("authorization: Bearer {LIVE}")),
            "{usage_request}"
        );
        assert!(
            usage_request.contains("chatgpt-account-id: acct-own"),
            "{usage_request}"
        );
        server.join().expect("server stopped");
        assert_eq!(
            fs::read(&home.path).expect("still there"),
            before,
            "the CLI's file is not read from and not written to"
        );
    }

    #[test]
    fn an_expired_login_is_rotated_back_into_the_keyring_not_onto_disk() {
        const ROTATION: (u16, &str) = (
            200,
            r#"{"access_token":"rotated","refresh_token":"rotated-refresh"}"#,
        );
        let home = TestHome::expired();
        let before = fs::read(&home.path).expect("fixture readable");
        // No `exp` a claim reader can find, and no `id_token`: the expiry is unknown, so
        // the 401 retry is the only thing that can trigger the rotation.
        let secrets = FakeSecrets::holding(json!({
            "tokens": {"access_token": "opaque", "refresh_token": "own-refresh", "account_id": "acct-own"}
        }));
        let (base, requests, server) = local_server(vec![(401, "{}"), ROTATION, USAGE]);
        let mut provider = home.provider(&base);
        provider.own = Some(Arc::clone(&secrets) as Arc<dyn crate::secrets::Secrets>);

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("one refresh, one retry");
        assert_eq!(snapshot.windows.len(), 1);

        let _rejected = requests.recv().expect("the first usage request");
        let refresh_request = requests.recv().expect("refresh request");
        assert!(
            refresh_request.contains("\"refresh_token\":\"own-refresh\""),
            "{refresh_request}"
        );
        let retried = requests.recv().expect("the retried usage request");
        assert!(
            retried.contains("authorization: Bearer rotated"),
            "{retried}"
        );
        server.join().expect("server stopped");

        let stored = secrets.stored();
        assert_eq!(stored["tokens"]["access_token"], "rotated");
        assert_eq!(stored["tokens"]["refresh_token"], "rotated-refresh");
        assert_eq!(
            stored["tokens"]["account_id"], "acct-own",
            "a rotation replaces the tokens and nothing else"
        );
        assert!(
            stored["last_refresh"].is_string(),
            "the timestamp of the credential material is written beside it: {stored}"
        );
        assert_eq!(
            fs::read(&home.path).expect("still there"),
            before,
            "a login of our own must never write to the CLI's file"
        );
    }

    #[test]
    fn a_login_response_becomes_the_document_the_same_parser_reads() {
        let document = document_from_login(&json!({
            "access_token": "fresh", "refresh_token": "fresh-refresh",
            "id_token": "an-id-token", "token_type": "Bearer"
        }))
        .expect("a usable response");
        let credentials = CodexCredentials::from_document(&document).expect("parses");
        assert_eq!(credentials.access_token.expose(), "fresh");
        assert!(credentials.refresh_token.is_some());
        assert!(document["last_refresh"].is_string());

        let error = document_from_login(&json!({"access_token": "  "})).expect_err("blank");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
    }
}
