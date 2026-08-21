//! Claude subscription quota, over one of two credentials.
//!
//! # Two sources, and which one wins
//!
//! Normally the account is Claude Code's: `~/.claude/.credentials.json`, read in place and
//! refreshed back into it under the CLI's own lock protocol, per ADR 0001. That file is
//! never created here and never replaced wholesale — Tidemark only ever updates the token
//! fields of a file the CLI already owns.
//!
//! The other source is a login the user performed **from Tidemark**, whose tokens live in
//! the Secret Service under [`crate::secrets::Kind::Token`]. It is checked first, and the
//! order is deliberate: it exists only because the user explicitly signed in here, so it
//! is the more recent statement of intent, and signing out removes it and hands the
//! account straight back to the CLI file. The stored document has the *same shape* as the
//! CLI's, which is why one parser reads both.

use super::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use crate::oauth;
use crate::oauth_file::{
    CredentialFile, CredentialFileError, Field, LockedCredentialFile, UpdateOutcome,
};
use crate::secrets::{self, Secrets};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "claude";

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA_HEADER: &str = "oauth-2025-04-20";

const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// The scopes the usage endpoint answers to. `user:inference` is what makes the token a
/// subscription token rather than a profile one, and without it `/api/oauth/usage` has
/// nothing to report.
const OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";
/// Fixed, because the client is registered with exactly this redirect. See ADR 0003.
const REDIRECT_PORT: u16 = 54_545;
const REDIRECT_PATH: &str = "/callback";

/// The subtree both the CLI file and a Tidemark login are stored under.
const TOKEN_SUBTREE: &str = "claudeAiOauth";

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
        // Anthropic's authorize page uses this to decide it is talking to a client that
        // will exchange a code rather than one expecting a token in the fragment.
        authorize_extras: &[("code", "true")],
        encoding: oauth::Encoding::Form,
    }
}

/// The credential document to store after a successful login.
///
/// Deliberately the *same shape the CLI writes*, so that everything downstream — the
/// parser, the expiry rule, the plan line — is one implementation rather than two. What is
/// not carried over is anything the token response did not say: an absent
/// `subscriptionType` stays absent, and the card simply shows no plan.
pub fn document_from_login(
    response: &serde_json::Value,
    now_ms: i64,
) -> Result<serde_json::Value, ProviderError> {
    let tokens: RefreshResponse = serde_json::from_value(response.clone()).map_err(|error| {
        ProviderError::malformed(format!(
            "the Claude login response is not readable: {error}"
        ))
    })?;
    if tokens.access_token.trim().is_empty() || tokens.refresh_token.trim().is_empty() {
        return Err(ProviderError::malformed(
            "the Claude login response carried a blank token",
        ));
    }
    let subscription = response
        .get("subscription_type")
        .or_else(|| response.get("subscriptionType"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let mut subtree = serde_json::Map::new();
    subtree.insert("accessToken".into(), tokens.access_token.into());
    subtree.insert("refreshToken".into(), tokens.refresh_token.into());
    subtree.insert(
        "expiresAt".into(),
        now_ms
            .saturating_add(tokens.expires_in.saturating_mul(1_000))
            .into(),
    );
    if tokens.refresh_token_expires_in > 0 {
        subtree.insert(
            "refreshTokenExpiresAt".into(),
            now_ms
                .saturating_add(tokens.refresh_token_expires_in.saturating_mul(1_000))
                .into(),
        );
    }
    if let Some(subscription) = subscription {
        subtree.insert("subscriptionType".into(), subscription.into());
    }
    Ok(serde_json::json!({ TOKEN_SUBTREE: subtree }))
}

#[derive(Debug)]
/// One Claude Code account.
pub struct Claude {
    client: reqwest::Client,
    credentials: CredentialFile,
    /// Where a login performed from Tidemark is kept, when the caller has somewhere to
    /// keep one. `None` in tests that only exercise the CLI file.
    own: Option<Arc<dyn Secrets>>,
    usage_url: String,
    refresh_url: String,
}

impl Claude {
    /// Builds the canonical Claude Code account at `~/.claude/.credentials.json`.
    pub fn new(own: Option<Arc<dyn Secrets>>) -> Result<Self, ProviderError> {
        let home = std::env::var_os("HOME")
            .filter(|home| Path::new(home).is_absolute())
            .ok_or_else(|| {
                ProviderError::Local("HOME does not name an absolute directory".into())
            })?;
        let path = Path::new(&home).join(".claude/.credentials.json");
        let write_lock = Path::new(&home).join(".claude/.storage-write.lock");
        let mut claude = Self::with_endpoints(
            CredentialFile::new(path.clone(), path).coordinated_by(write_lock),
            USAGE_URL.to_owned(),
            REFRESH_URL.to_owned(),
        )?;
        claude.own = own;
        Ok(claude)
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
        let (access_token, plan) = self.credential().await?;

        let response = self
            .client
            .get(&self.usage_url)
            .bearer_auth(access_token.expose())
            .header("anthropic-beta", BETA_HEADER)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(ProviderError::Transport)?;
        let status = response.status();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(status, retry_after.as_deref())?;
        let body = response.text().await.map_err(ProviderError::Transport)?;
        let mut snapshot = parse(&body, Timestamp::now())?;
        if let Some(plan) = plan.filter(|plan| !plan.trim().is_empty()) {
            snapshot.details.insert(
                0,
                DetailSection {
                    title: DetailSection::PLAN.to_owned(),
                    rows: vec![DetailRow {
                        label: "Subscription".to_owned(),
                        value: plan,
                    }],
                },
            );
        }
        Ok(snapshot)
    }

    /// The access token to spend, and the plan to put on the card.
    ///
    /// A Tidemark login wins over the CLI file when there is one — see the module docs.
    /// Neither source is consulted for the other's failures: a keyring that is locked
    /// reports itself as locked rather than silently falling through to a file that may
    /// hold a different account.
    async fn credential(&self) -> Result<(Credential, Option<String>), ProviderError> {
        if let Some(own) = &self.own {
            let provider = ProviderId::new(PROVIDER_ID);
            let account = AccountId::default();
            let stored = own
                .get(secrets::Kind::Token, &provider, &account)
                .await
                .map_err(ProviderError::from_secret_error)?;
            if let Some(stored) = stored {
                return self.own_login_credential(own.as_ref(), stored).await;
            }
        }
        self.cli_file_credential().await
    }

    /// The CLI's file, refreshed in place if the token in it is spent.
    async fn cli_file_credential(&self) -> Result<(Credential, Option<String>), ProviderError> {
        let locked = self.credentials.lock().map_err(map_file_error)?;
        let document = locked.read_json().map_err(map_file_error)?;
        let mut credentials = ClaudeCredentials::from_document(&document)?;
        let now_ms = now_millis();
        if credentials.is_expired_at(now_ms) {
            credentials = self.refresh(&locked, credentials, now_ms).await?;
        }
        Ok((
            credentials.access_token.clone(),
            credentials.subscription_type().map(str::to_owned),
        ))
    }

    /// Tidemark's own login, refreshed straight back into the Secret Service.
    ///
    /// None of the file protocol applies here and none of it is performed: there is no
    /// vendor process racing us for these bytes, nothing else reads them, and the backup
    /// the file path takes before an irreversible rotation exists to protect a credential
    /// this one *is* the only copy of. What replaces it is the ordering — the new document
    /// is stored before it is used, so a refresh that succeeds at the provider and then
    /// fails locally has still recorded the token that rotation just made the live one.
    async fn own_login_credential(
        &self,
        own: &dyn Secrets,
        stored: Credential,
    ) -> Result<(Credential, Option<String>), ProviderError> {
        let document: serde_json::Value =
            serde_json::from_str(stored.expose()).map_err(|error| {
                ProviderError::malformed(format!("the stored Claude login is not JSON: {error}"))
            })?;
        let credentials = ClaudeCredentials::from_document(&document)?;
        let now_ms = now_millis();
        if !credentials.is_expired_at(now_ms) {
            return Ok((
                credentials.access_token.clone(),
                credentials.subscription_type().map(str::to_owned),
            ));
        }

        let refresh_token = credentials
            .refresh_token
            .as_ref()
            .ok_or(ProviderError::Credential { status: 401 })?;
        if credentials
            .refresh_token_expires_at
            .is_some_and(|expires_at| now_ms >= expires_at)
        {
            // The one-time token is past its own expiry, so there is nothing left to
            // exchange. The user signs in again; that is the `credential-rejected` state.
            return Err(ProviderError::Credential { status: 401 });
        }
        let refreshed = self.exchange_refresh(refresh_token.expose()).await?;
        let mut document = document;
        let subtree = document
            .get_mut(TOKEN_SUBTREE)
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| ProviderError::malformed("the stored Claude login lost its subtree"))?;
        for (field, value) in refreshed_fields(refreshed, now_ms) {
            subtree.insert(field.name().to_owned(), value);
        }
        own.set(
            secrets::Kind::Token,
            &ProviderId::new(PROVIDER_ID),
            &AccountId::default(),
            &Credential::new(document.to_string()),
        )
        .await
        .map_err(ProviderError::from_secret_error)?;

        let credentials = ClaudeCredentials::from_document(&document)?;
        Ok((
            credentials.access_token.clone(),
            credentials.subscription_type().map(str::to_owned),
        ))
    }

    /// The refresh grant itself, shared by both credential sources.
    async fn exchange_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<RefreshResponse, ProviderError> {
        let response = self
            .client
            .post(&self.refresh_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", OAUTH_CLIENT_ID),
            ])
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
            ProviderError::malformed(format!("Claude refresh response is not readable: {error}"))
        })?;
        if refreshed.access_token.trim().is_empty() || refreshed.refresh_token.trim().is_empty() {
            return Err(ProviderError::malformed(
                "Claude refresh response carried a blank token",
            ));
        }
        if refreshed.expires_in <= 0 || refreshed.refresh_token_expires_in <= 0 {
            return Err(ProviderError::malformed(
                "Claude refresh response carried a non-positive expiry",
            ));
        }
        Ok(refreshed)
    }

    async fn refresh(
        &self,
        locked: &LockedCredentialFile,
        credentials: ClaudeCredentials,
        now_ms: i64,
    ) -> Result<ClaudeCredentials, ProviderError> {
        let refresh_token = credentials
            .refresh_token
            .as_ref()
            .ok_or(ProviderError::Credential { status: 401 })?;
        if credentials
            .refresh_token_expires_at
            .is_some_and(|expires_at| now_ms >= expires_at)
        {
            return Err(ProviderError::Credential { status: 401 });
        }
        let expected_refresh_token = refresh_token.expose().to_owned();
        locked
            .preflight_unique_fields(
                TOKEN_SUBTREE,
                &[
                    Field::Subtree("accessToken"),
                    Field::Subtree("refreshToken"),
                    Field::Subtree("expiresAt"),
                    Field::Subtree("refreshTokenExpiresAt"),
                ],
            )
            .map_err(map_file_error)?;
        // A successful refresh rotates the one-time token. Preserve the exact CLI-owned
        // bytes before crossing that irreversible boundary.
        locked.backup().map_err(map_file_error)?;
        let refreshed = self.exchange_refresh(&expected_refresh_token).await?;
        let updates = refreshed_fields(refreshed, now_ms);
        let outcome = locked
            .update_top_level(
                TOKEN_SUBTREE,
                ("refreshToken", &expected_refresh_token),
                &updates,
            )
            .map_err(map_file_error)?;
        let current = locked.read_json().map_err(map_file_error)?;
        let current = ClaudeCredentials::from_document(&current)?;
        if outcome == UpdateOutcome::SourceChanged && current.is_expired_at(now_millis()) {
            return Err(ProviderError::Local(
                "Claude credentials changed during refresh but are still expired".into(),
            ));
        }
        Ok(current)
    }
}

impl Provider for Claude {
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

/// Turns a usage response into a snapshot.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a Claude usage response: {error}"))
    })?;
    let entries = envelope
        .limits
        .ok_or_else(|| ProviderError::malformed("Claude usage response carried no limits array"))?;

    let mut windows = Vec::new();
    for entry in entries {
        let Some(kind) = Kind::recognise(&entry)? else {
            continue;
        };
        let limit: Limit = serde_json::from_value(entry).map_err(|error| {
            ProviderError::malformed(format!("{kind:?} limit entry is not readable: {error}"))
        })?;
        windows.push(kind.window(limit)?);
    }

    let mut details = extra_usage_details(envelope.extra_usage.as_ref());
    if details.is_empty() {
        details = spend_details(envelope.spend.as_ref());
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    limits: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    spend: Option<Spend>,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Session,
    Weekly,
}

impl Kind {
    fn recognise(entry: &serde_json::Value) -> Result<Option<Self>, ProviderError> {
        let kind = match entry.get("kind").and_then(serde_json::Value::as_str) {
            Some("session") => Self::Session,
            Some("weekly_all" | "weekly_scoped") => Self::Weekly,
            _ => return Ok(None),
        };
        match entry.get("group") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(group)) => {
                let expected = match kind {
                    Self::Session => "session",
                    Self::Weekly => "weekly",
                };
                if group != expected {
                    return Err(ProviderError::malformed(format!(
                        "{kind:?} limit has conflicting group {group:?}"
                    )));
                }
            }
            Some(_) => {
                return Err(ProviderError::malformed(format!(
                    "{kind:?} limit has a non-string group"
                )));
            }
        }
        Ok(Some(kind))
    }

    fn length(self) -> WindowLength {
        let seconds = match self {
            Self::Session => 5 * 3_600,
            Self::Weekly => 7 * 86_400,
        };
        WindowLength::from_secs(seconds).expect("both provider windows are non-zero")
    }

    fn window(self, limit: Limit) -> Result<Window, ProviderError> {
        let length = self.length();
        let scoped_pool = limit.scope.as_ref().and_then(|scope| {
            scope
                .model
                .as_ref()
                .and_then(|model| model.id.as_deref().or(model.display_name.as_deref()))
        });
        let key = scoped_pool.map_or_else(
            || WindowKey::for_length(length),
            |pool| WindowKey::for_pool(pool, length),
        );
        let title = match (self, limit.scope.as_ref().and_then(Scope::model_name)) {
            (Self::Session, _) => "5 hours".to_owned(),
            (Self::Weekly, Some(model)) => format!("{model} · 7 days"),
            (Self::Weekly, None) => "7 days".to_owned(),
        };
        let resets_at = match limit.resets_at.as_deref() {
            None => None,
            Some(raw) => Some(parse_rfc3339(raw).ok_or_else(|| {
                ProviderError::malformed(format!("{self:?} limit has invalid resets_at"))
            })?),
        };
        Ok(Window {
            key,
            title,
            subtitle: None,
            used_percent: limit.percent.clamp(0.0, 100.0),
            resets_at,
            length: Some(length),
        })
    }
}

#[derive(Debug, Deserialize)]
struct Limit {
    percent: f64,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Option<Scope>,
}

#[derive(Debug, Deserialize)]
struct Scope {
    #[serde(default)]
    model: Option<Model>,
}

impl Scope {
    fn model_name(&self) -> Option<&str> {
        self.model.as_ref()?.display_name.as_deref()
    }
}

#[derive(Debug, Deserialize)]
struct Model {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtraUsage {
    #[serde(default)]
    is_enabled: bool,
    #[serde(default)]
    monthly_limit: Option<f64>,
    #[serde(default)]
    used_credits: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    decimal_places: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct Spend {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    used: Option<Money>,
    #[serde(default)]
    limit: Option<Money>,
}

#[derive(Debug, Deserialize)]
struct Money {
    amount_minor: i64,
    currency: String,
    exponent: u32,
}

fn extra_usage_details(extra: Option<&ExtraUsage>) -> Vec<DetailSection> {
    let Some(extra) = extra.filter(|extra| extra.is_enabled) else {
        return Vec::new();
    };
    let (Some(used), Some(limit)) = (extra.used_credits, extra.monthly_limit) else {
        return Vec::new();
    };
    let decimals = extra.decimal_places.unwrap_or(2).min(6);
    let divisor = 10_f64.powi(decimals as i32);
    let currency = extra.currency.as_deref().unwrap_or("");
    vec![DetailSection {
        title: "Extra usage".to_owned(),
        rows: vec![DetailRow {
            label: "Used".to_owned(),
            value: format!(
                "{} of {}",
                format_money(used / divisor, currency, decimals),
                format_money(limit / divisor, currency, decimals)
            ),
        }],
    }]
}

fn format_money(amount: f64, currency: &str, decimals: usize) -> String {
    match currency {
        "USD" => format!("${amount:.decimals$}"),
        "" => format!("{amount:.decimals$}"),
        other => format!("{amount:.decimals$} {other}"),
    }
}

fn spend_details(spend: Option<&Spend>) -> Vec<DetailSection> {
    let Some(spend) = spend.filter(|spend| spend.enabled) else {
        return Vec::new();
    };
    let (Some(used), Some(limit)) = (&spend.used, &spend.limit) else {
        return Vec::new();
    };
    let decimals = used.exponent.min(6) as usize;
    let divisor = 10_f64.powi(used.exponent.min(18) as i32);
    let limit_divisor = 10_f64.powi(limit.exponent.min(18) as i32);
    vec![DetailSection {
        title: "Extra usage".to_owned(),
        rows: vec![DetailRow {
            label: "Used".to_owned(),
            value: format!(
                "{} of {}",
                format_money(used.amount_minor as f64 / divisor, &used.currency, decimals),
                format_money(
                    limit.amount_minor as f64 / limit_divisor,
                    &limit.currency,
                    limit.exponent.min(6) as usize
                )
            ),
        }],
    }]
}

struct ClaudeCredentials {
    access_token: Credential,
    refresh_token: Option<Credential>,
    expires_at: Option<i64>,
    refresh_token_expires_at: Option<i64>,
    subscription_type: Option<String>,
}

impl std::fmt::Debug for ClaudeCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCredentials")
            .field("access_token", &self.access_token)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("expires_at", &self.expires_at)
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .field("subscription_type", &self.subscription_type)
            .finish()
    }
}

impl ClaudeCredentials {
    fn from_document(document: &serde_json::Value) -> Result<Self, ProviderError> {
        let subtree = document
            .get(TOKEN_SUBTREE)
            .cloned()
            .ok_or_else(|| ProviderError::malformed("missing claudeAiOauth"))?;
        let raw: RawCredentials = serde_json::from_value(subtree.clone()).map_err(|error| {
            ProviderError::malformed(format!("claudeAiOauth is not readable: {error}"))
        })?;
        let access_token = Credential::new(raw.access_token);
        if access_token.is_blank() {
            return Err(ProviderError::malformed(
                "claudeAiOauth has no access token",
            ));
        }
        let refresh_token = raw
            .refresh_token
            .map(Credential::new)
            .filter(|token| !token.is_blank());
        Ok(Self {
            access_token,
            refresh_token,
            expires_at: raw.expires_at,
            refresh_token_expires_at: raw.refresh_token_expires_at,
            subscription_type: raw.subscription_type,
        })
    }

    fn is_expired_at(&self, now_ms: i64) -> bool {
        self.expires_at
            .is_none_or(|expires_at| now_ms >= expires_at)
    }

    fn subscription_type(&self) -> Option<&str> {
        self.subscription_type.as_deref()
    }
}

fn refreshed_fields(
    response: RefreshResponse,
    now_ms: i64,
) -> [(Field<'static>, serde_json::Value); 4] {
    [
        (Field::Subtree("accessToken"), response.access_token.into()),
        (
            Field::Subtree("refreshToken"),
            response.refresh_token.into(),
        ),
        (
            Field::Subtree("expiresAt"),
            now_ms
                .saturating_add(response.expires_in.saturating_mul(1_000))
                .into(),
        ),
        (
            Field::Subtree("refreshTokenExpiresAt"),
            now_ms
                .saturating_add(response.refresh_token_expires_in.saturating_mul(1_000))
                .into(),
        ),
    ]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCredentials {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    refresh_token_expires_at: Option<i64>,
    #[serde(default)]
    subscription_type: Option<String>,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    refresh_token_expires_in: i64,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
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
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    struct TestCredentials {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TestCredentials {
        fn expired() -> Self {
            // Unique per call, not merely per process: more than one test in this module
            // wants an expired credential file, and they run on the same process.
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("tidemark-claude-test-{}-{nth}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).expect("test directory");
            let path = dir.join(".credentials.json");
            let mut document: serde_json::Value = serde_json::from_slice(include_bytes!(
                "../../tests/fixtures/claude-credentials.json"
            ))
            .expect("fixture JSON");
            document["claudeAiOauth"]["expiresAt"] = json!(1_i64);
            fs::write(
                &path,
                serde_json::to_vec_pretty(&document).expect("serialize fixture"),
            )
            .expect("write fixture");
            Self { dir, path }
        }
    }

    impl Drop for TestCredentials {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A Secret Service that is one string, held in memory.
    ///
    /// The real one is exercised in `crate::secrets`; what these tests need from it is the
    /// two things that decide the provider's behaviour — whether a Tidemark login exists,
    /// and what it says after a refresh has been written back.
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

    fn local_server(
        responses: Vec<&'static str>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (requests_tx, requests_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response_body in responses {
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .expect("response written");
            }
        });
        (format!("http://{address}"), requests_rx, handle)
    }

    #[test]
    fn a_refresh_rotation_produces_both_token_and_expiry_field_updates() {
        let document = json!({
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 1_787_000_000_000_i64,
                "refreshTokenExpiresAt": 1_789_000_000_000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "pro",
                "rateLimitTier": "default_claude_ai"
            },
            "mcpOAuth": {"unrelated": {"accessToken": "mcp-token"}}
        });
        let credentials = ClaudeCredentials::from_document(&document).expect("credentials parse");
        let response: RefreshResponse = serde_json::from_value(json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 28_800,
            "refresh_token_expires_in": 2_419_200,
            "token_type": "bearer",
            "account": {"uuid": "ignored"},
            "organization": {"uuid": "ignored"},
            "scope": "user:inference user:profile",
            "token_uuid": "ignored"
        }))
        .expect("refresh shape parses");

        let fields = refreshed_fields(response, 1_787_100_000_000);
        let fields: serde_json::Map<String, serde_json::Value> = fields
            .into_iter()
            .map(|(field, value)| (field.name().to_owned(), value))
            .collect();
        assert_eq!(fields["accessToken"], "new-access");
        assert_eq!(fields["refreshToken"], "new-refresh");
        assert_eq!(fields["expiresAt"], 1_787_128_800_000_i64);
        assert_eq!(fields["refreshTokenExpiresAt"], 1_789_519_200_000_i64);
        assert_eq!(credentials.subscription_type(), Some("pro"));
    }

    #[test]
    fn expiry_is_read_as_milliseconds_not_seconds() {
        let document = json!({"claudeAiOauth": {
            "accessToken": "access", "refreshToken": "refresh",
            "expiresAt": 1_787_200_000_000_i64
        }});
        let credentials = ClaudeCredentials::from_document(&document).expect("credentials parse");

        assert!(!credentials.is_expired_at(1_787_199_999_999));
        assert!(credentials.is_expired_at(1_787_200_000_000));
    }

    #[test]
    fn an_expired_token_is_rotated_persisted_and_then_used_for_quota() {
        const REFRESH: &str = r#"{
          "access_token":"new-access","refresh_token":"new-refresh",
          "expires_in":28800,"refresh_token_expires_in":2419200,"token_type":"bearer",
          "account":{"uuid":"fixture"},"organization":{"uuid":"fixture"},
          "scope":"user:inference user:profile","token_uuid":"fixture"
        }"#;
        const USAGE: &str = r#"{
          "limits":[
            {"kind":"session","group":"session","percent":31,"severity":"normal",
             "resets_at":"2026-08-20T21:50:00Z","scope":null,"is_active":true}
          ],
          "spend":null,
          "extra_usage":{"is_enabled":false,"monthly_limit":null,"used_credits":null,
            "utilization":null,"currency":null,"decimal_places":null,"disabled_reason":null,
            "user_disabled":false,"spend_limit_reached":false,"credits_ever_enabled":false,
            "daily":null,"weekly":null}
        }"#;
        let credentials = TestCredentials::expired();
        let before_bytes = fs::read(&credentials.path).expect("fixture readable");
        let before: serde_json::Value =
            serde_json::from_slice(&before_bytes).expect("fixture JSON");
        let (base, requests, server) = local_server(vec![REFRESH, USAGE]);
        let provider = Claude::with_endpoints(
            CredentialFile::new(credentials.path.clone(), credentials.path.clone())
                .coordinated_by(credentials.dir.join(".storage-write.lock")),
            format!("{base}/usage"),
            format!("{base}/token"),
        )
        .expect("provider builds");

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("refresh and fetch succeed");

        assert_eq!(snapshot.windows[0].used_percent, 31.0);
        assert_eq!(snapshot.details[0].title, DetailSection::PLAN);
        assert_eq!(snapshot.details[0].rows[0].value, "pro");
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
            refresh_request.contains("grant_type=refresh_token"),
            "{refresh_request}"
        );
        assert!(
            refresh_request.contains("refresh_token=fixture-old-refresh"),
            "{refresh_request}"
        );
        assert!(
            refresh_request.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
            "{refresh_request}"
        );
        let usage_request = requests.recv().expect("usage request captured");
        assert!(usage_request.starts_with("GET /usage "), "{usage_request}");
        assert!(
            usage_request.contains("authorization: Bearer new-access"),
            "{usage_request}"
        );
        assert!(
            usage_request.contains("anthropic-beta: oauth-2025-04-20"),
            "{usage_request}"
        );
        server.join().expect("server stopped");

        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(&credentials.path).expect("credentials readable"))
                .expect("credentials JSON");
        assert_eq!(after["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(after["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert!(
            after["claudeAiOauth"]["expiresAt"]
                .as_i64()
                .expect("access expiry")
                > 1
        );
        assert!(
            after["claudeAiOauth"]["refreshTokenExpiresAt"]
                .as_i64()
                .expect("refresh expiry")
                > after["claudeAiOauth"]["expiresAt"]
                    .as_i64()
                    .expect("access expiry")
        );
        assert_eq!(after["mcpOAuth"], before["mcpOAuth"]);
        let backup = credentials
            .path
            .with_file_name(".credentials.json.tidemark-backup");
        assert_eq!(fs::read(&backup).expect("backup readable"), before_bytes);
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
    fn a_login_performed_here_is_used_ahead_of_the_cli_file() {
        const USAGE: &str = r#"{
          "limits":[
            {"kind":"session","group":"session","percent":12,"severity":"normal",
             "resets_at":"2026-08-20T21:50:00Z","scope":null,"is_active":true}
          ],
          "spend":null,"extra_usage":null
        }"#;
        // The CLI file on disk is expired and would demand a refresh; the Tidemark login
        // is live. Exactly one request must leave, and it must carry the login's token.
        let credentials = TestCredentials::expired();
        let secrets = FakeSecrets::holding(json!({"claudeAiOauth": {
            "accessToken": "from-the-tidemark-login",
            "refreshToken": "own-refresh",
            "expiresAt": 4_102_444_800_000_i64,
            "subscriptionType": "max"
        }}));
        let (base, requests, server) = local_server(vec![USAGE]);
        let mut provider = Claude::with_endpoints(
            CredentialFile::new(credentials.path.clone(), credentials.path.clone()),
            format!("{base}/usage"),
            format!("{base}/token"),
        )
        .expect("provider builds");
        provider.own = Some(Arc::clone(&secrets) as Arc<dyn crate::secrets::Secrets>);

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("the stored login is enough");

        assert_eq!(snapshot.windows[0].used_percent, 12.0);
        assert_eq!(snapshot.details[0].rows[0].value, "max");
        let usage_request = requests.recv().expect("one request");
        assert!(
            usage_request.contains("authorization: Bearer from-the-tidemark-login"),
            "{usage_request}"
        );
        server.join().expect("server stopped");
        assert!(
            requests.try_recv().is_err(),
            "the CLI file must not have been touched"
        );
    }

    #[test]
    fn an_expired_login_is_rotated_back_into_the_keyring_not_onto_disk() {
        const REFRESH: &str = r#"{
          "access_token":"rotated","refresh_token":"rotated-refresh",
          "expires_in":28800,"refresh_token_expires_in":2419200,"token_type":"bearer"
        }"#;
        const USAGE: &str = r#"{
          "limits":[
            {"kind":"session","group":"session","percent":7,"severity":"normal",
             "resets_at":"2026-08-20T21:50:00Z","scope":null,"is_active":true}
          ],
          "spend":null,"extra_usage":null
        }"#;
        let credentials = TestCredentials::expired();
        let before = fs::read(&credentials.path).expect("fixture readable");
        let secrets = FakeSecrets::holding(json!({"claudeAiOauth": {
            "accessToken": "spent",
            "refreshToken": "own-refresh",
            "expiresAt": 1_i64,
            "subscriptionType": "max"
        }}));
        let (base, requests, server) = local_server(vec![REFRESH, USAGE]);
        let mut provider = Claude::with_endpoints(
            CredentialFile::new(credentials.path.clone(), credentials.path.clone()),
            format!("{base}/usage"),
            format!("{base}/token"),
        )
        .expect("provider builds");
        provider.own = Some(Arc::clone(&secrets) as Arc<dyn crate::secrets::Secrets>);

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("refresh and fetch succeed");
        assert_eq!(snapshot.windows[0].used_percent, 7.0);

        let refresh_request = requests.recv().expect("refresh request");
        assert!(
            refresh_request.contains("refresh_token=own-refresh"),
            "{refresh_request}"
        );
        let usage_request = requests.recv().expect("usage request");
        assert!(
            usage_request.contains("authorization: Bearer rotated"),
            "{usage_request}"
        );
        server.join().expect("server stopped");

        let stored = secrets.stored();
        assert_eq!(stored["claudeAiOauth"]["accessToken"], "rotated");
        assert_eq!(stored["claudeAiOauth"]["refreshToken"], "rotated-refresh");
        assert_eq!(
            stored["claudeAiOauth"]["subscriptionType"], "max",
            "a rotation replaces the tokens and nothing else"
        );
        assert_eq!(
            fs::read(&credentials.path).expect("still there"),
            before,
            "a login of our own must never write to the CLI's file"
        );
    }

    #[test]
    fn a_login_response_becomes_the_document_the_same_parser_reads() {
        let response = json!({
            "access_token": "fresh", "refresh_token": "fresh-refresh",
            "expires_in": 28_800, "refresh_token_expires_in": 2_419_200,
            "token_type": "bearer", "scope": "user:inference user:profile"
        });
        let document =
            document_from_login(&response, 1_787_100_000_000).expect("a usable response");
        let credentials = ClaudeCredentials::from_document(&document).expect("parses");
        assert_eq!(credentials.access_token.expose(), "fresh");
        assert!(!credentials.is_expired_at(1_787_100_000_000));
        assert!(credentials.is_expired_at(1_787_128_800_000));
        assert_eq!(
            credentials.subscription_type(),
            None,
            "a plan the response did not name is absent, not invented"
        );
    }

    #[test]
    fn a_login_response_with_no_token_in_it_is_refused_rather_than_stored() {
        let error = document_from_login(&json!({"access_token": "", "refresh_token": "r", "expires_in": 1, "refresh_token_expires_in": 1}), 0)
            .expect_err("blank");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
        let error =
            document_from_login(&json!({"token_type": "bearer"}), 0).expect_err("no tokens");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
    }
}
