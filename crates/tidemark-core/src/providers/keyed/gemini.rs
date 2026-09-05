//! Gemini's Code Assist quota, read from the Gemini CLI's own Google login.
//!
//! The credential is the CLI's `~/.gemini/oauth_creds.json`, read in place and refreshed
//! back into it, per ADR 0001 — never created here, never replaced wholesale. The CLI has
//! no headless way to renew the login; the refresh grant itself does not rotate the
//! refresh token, so the write-back replaces only the access token, the expiry instant
//! and the id token, and every other byte of the document stays the CLI's. The CLI does
//! not honor the advisory lock — it replaces the file atomically — so the write-back is
//! conditional: the update itself rereads the document as it publishes, and a file
//! changed meanwhile is left alone, that poll spending the freshly obtained access token
//! without persisting it. Tidemark runs no vendor program: an expired token with nothing to refresh it is
//! `NoCredential`, pointing at the sign-in.
//!
//! The quota is a per-model-family fraction, not a number of requests: `buckets[]` names
//! each model and the share of its daily allowance left, and the card draws the tiers —
//! Pro, Flash, Flash-Lite — at the lowest fraction any of their models reported, which is
//! how the CLI's own status line reads the same body. A model outside the three families
//! draws no window anyone could size; like upstream, the card skips it rather than
//! inventing a limit. A bucket that names a known family but states no fraction fails the
//! fetch: quietly dropping it would hide a tier the card promises to draw.
//!
//! Two hops stand between the token and the quota: `loadCodeAssist` names the account's
//! tier and — usually — its Cloud Code project, and when it names none, the resource
//! manager is asked and the CLI's own `gen-lang-client` project picked. Both hops fail
//! soft, exactly as upstream treats them: the quota call goes out with an empty project
//! body when neither answers, and the quota endpoint has the final word. Not ported is
//! the recovery path that spawns a Node runtime to re-extract the OAuth client out of an
//! installed CLI: Tidemark carries the CLI's public client constants instead, overridable
//! through `GEMINI_OAUTH_CLIENT_ID` / `GEMINI_OAUTH_CLIENT_SECRET`.

use super::{HandSpec, Options, ProviderError, redact_query};
use crate::oauth_file::{CredentialFile, CredentialFileError};
use crate::providers::{BoxFuture, Credential, Provider};
use base64::Engine;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "gemini";

const QUOTA_BASE: &str = "https://cloudcode-pa.googleapis.com";
const PROJECTS_BASE: &str = "https://cloudresourcemanager.googleapis.com";
const TOKEN_BASE: &str = "https://oauth2.googleapis.com";
/// The body `loadCodeAssist` carries: it says this call speaks for the CLI, which is what
/// makes a consumer account's tier answer at all.
const LOAD_BODY: &str = r#"{"metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}}"#;
/// The settings value that says the CLI's login is the personal Google OAuth one — the
/// only kind this provider can read. Any other selection is the CLI's own business.
const OAUTH_PERSONAL: &str = "oauth-personal";
/// The Gemini CLI's installed-app OAuth client, the constants its own `oauth2.ts` ships in
/// every installed copy. Google's OAuth documentation is explicit that an installed app's
/// client secret "is obviously not treated as a secret". `GEMINI_OAUTH_CLIENT_ID` and
/// `GEMINI_OAUTH_CLIENT_SECRET` override them for a private or custom build.
const CLI_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const CLI_CLIENT_SECRET: &str = "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";

/// The tiers draw one window each. Upstream sizes them at a day: the fraction is the
/// account's daily allowance, and the response's `resetTime` says exactly when it rolls.
const DAY_SECS: u64 = 86_400;

/// Gemini as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Gemini",
    credential: CredentialKind::External,
    credential_hint: "Read the Gemini CLI's own login (`gemini` → sign in).",
    options: &[],
    build,
};

fn build(
    account: AccountId,
    _credential: Credential,
    _options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Gemini::new_for_account(account)?))
}

/// One Gemini CLI login on this machine.
pub struct Gemini {
    tidemark_account: AccountId,
    client: reqwest::Client,
    credentials: CredentialFile,
    /// The home the credentials and the CLI's settings live under, when `$HOME` says.
    home: Option<PathBuf>,
    /// The three upstream hosts, kept apart so a test can point them all at one loopback.
    quota_base: String,
    projects_base: String,
    token_base: String,
    client_id: String,
    client_secret: String,
}

impl Gemini {
    /// Builds the canonical account at `~/.gemini/oauth_creds.json`.
    pub fn new() -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default())
    }

    fn new_for_account(account_id: AccountId) -> Result<Self, ProviderError> {
        let path = cli_credentials_path().ok_or_else(|| {
            ProviderError::Local("HOME does not name an absolute directory".into())
        })?;
        let home = path.parent().and_then(Path::parent).map(Path::to_path_buf);
        let (client_id, client_secret) = client_credentials(
            std::env::var("GEMINI_OAUTH_CLIENT_ID").ok(),
            std::env::var("GEMINI_OAUTH_CLIENT_SECRET").ok(),
        );
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: super::http::client()?,
            credentials: CredentialFile::new(path.clone(), path),
            home,
            quota_base: QUOTA_BASE.to_owned(),
            projects_base: PROJECTS_BASE.to_owned(),
            token_base: TOKEN_BASE.to_owned(),
            client_id,
            client_secret,
        })
    }

    #[cfg(test)]
    fn for_test(home: &Path, base: &str) -> Result<Self, ProviderError> {
        let base = base.trim_end_matches('/').to_owned();
        let credentials = home.join(".gemini/oauth_creds.json");
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: super::http::client()?,
            credentials: CredentialFile::new(credentials.clone(), credentials),
            home: Some(home.to_path_buf()),
            quota_base: base.clone(),
            projects_base: base.clone(),
            token_base: base,
            client_id: "test-client-id".to_owned(),
            client_secret: "test-client-secret".to_owned(),
        })
    }

    fn load_url(&self) -> String {
        format!("{}/v1internal:loadCodeAssist", self.quota_base)
    }

    fn quota_url(&self) -> String {
        format!("{}/v1internal:retrieveUserQuota", self.quota_base)
    }

    fn projects_url(&self) -> String {
        format!("{}/v1/projects", self.projects_base)
    }

    fn token_url(&self) -> String {
        format!("{}/token", self.token_base)
    }

    /// The CLI's chosen authentication, when its settings name one. A settings file that
    /// is missing or silent leaves `None` — upstream treats that as unknown and tries the
    /// OAuth credentials anyway.
    fn selected_auth_type(&self) -> Option<String> {
        let bytes = std::fs::read(self.home.as_ref()?.join(".gemini/settings.json")).ok()?;
        let settings: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        settings
            .get("security")?
            .get("auth")?
            .get("selectedType")?
            .as_str()
            .map(str::to_owned)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if let Some(selected) = self.selected_auth_type()
            && selected != OAUTH_PERSONAL
        {
            return Err(ProviderError::Local(format!(
                "the Gemini CLI is set to `{selected}` authentication; \
                 run `gemini` and sign in with your Google account"
            )));
        }

        let now_ms = Timestamp::now().as_unix().saturating_mul(1000);
        let locked = self.credentials.lock().map_err(file_error)?;
        let document = locked.read_json().map_err(file_error)?;
        let stored = Stored::from_document(&document)?;

        let (access_token, id_token) = if stored.needs_refresh(now_ms) {
            let refresh_token = stored
                .refresh_token
                .as_deref()
                .ok_or(ProviderError::NoCredential)?;
            let refreshed = self.exchange_refresh(refresh_token).await?;
            // The CLI does not honor this lock: it replaces the file atomically while the
            // grant runs. Write back only when the document the update rereads is still
            // the one the exchange was based on — the comparison and the publish read the
            // same bytes, so a newer login or a fresher rotation keeps its tokens, and
            // this poll simply spends the access token it obtained.
            locked
                .update_root_fields_if_unchanged(
                    |document| {
                        Stored::from_document(document).is_ok_and(|current| {
                            current.refresh_token == stored.refresh_token
                                && current.expiry_date == stored.expiry_date
                        })
                    },
                    &refreshed.fields(now_ms),
                )
                .map_err(file_error)?;
            (
                refreshed.access_token,
                refreshed.id_token.or(stored.id_token.clone()),
            )
        } else {
            let access_token = stored
                .access_token
                .clone()
                .expect("a live token is why no refresh is due");
            (access_token, stored.id_token.clone())
        };
        drop(locked);

        let email = id_token
            .as_deref()
            .and_then(claims)
            .and_then(|value| {
                value
                    .get("email")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|email| !email.is_empty());

        // Both lookups are advisory upstream: a refusal there costs the tier row and the
        // accurate project, never the card.
        let code_assist = self.code_assist(&access_token).await.unwrap_or_default();
        let project = match code_assist.cloudaicompanion_project.clone() {
            Some(project) => Some(project),
            None => self.discover_project(&access_token).await,
        };

        let request = self.quota_request(&access_token, project.as_deref())?;
        let (body, _) = super::request_inspected(PROVIDER_ID, &self.client, request, |response| {
            // A dead login is the sign-in's business, not an HTTP error.
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(ProviderError::NoCredential);
            }
            Ok(())
        })
        .await?;
        let mut snapshot =
            parse_quota_for_account(&body, Timestamp::now(), &self.tidemark_account)?;

        if let Some(email) = email {
            snapshot.details.push(DetailSection {
                title: "Account".to_owned(),
                rows: vec![DetailRow {
                    label: "Email".to_owned(),
                    value: email,
                }],
            });
        }
        if let Some(tier) = code_assist.display_tier() {
            snapshot.details.push(DetailSection {
                title: DetailSection::PLAN.to_owned(),
                rows: vec![DetailRow {
                    label: "Tier".to_owned(),
                    value: crate::providers::title_case(&tier),
                }],
            });
        }
        Ok(snapshot)
    }

    /// The refresh grant. Google answers `400`/`401`/`403` to a spent or revoked refresh
    /// token — the sign-in's business — and anything else is transport.
    async fn exchange_refresh(&self, refresh_token: &str) -> Result<Refreshed, ProviderError> {
        let response = self
            .client
            .post(self.token_url())
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(ProviderError::Transport)?;
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(ProviderError::Credential {
                status: status.as_u16(),
            });
        }
        let retry_after = super::http::retry_after_header(&response).map(str::to_owned);
        super::http::check(status, retry_after.as_deref())?;
        let refreshed: Refreshed = response.json().await.map_err(|error| {
            ProviderError::malformed(format!(
                "the Gemini refresh response is not readable: {error}"
            ))
        })?;
        if refreshed.access_token.trim().is_empty() {
            return Err(ProviderError::malformed(
                "the Gemini refresh response carried a blank access token",
            ));
        }
        Ok(refreshed)
    }

    async fn code_assist(&self, access_token: &str) -> Result<CodeAssist, ProviderError> {
        let request = self
            .client
            .post(self.load_url())
            .bearer_auth(access_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(LOAD_BODY.to_owned())
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))?;
        let body = super::request(PROVIDER_ID, &self.client, request).await?;
        serde_json::from_str(&body).map_err(|error| {
            ProviderError::malformed(format!("the Code Assist response is not readable: {error}"))
        })
    }

    /// Asks the resource manager which of the account's projects serves the CLI. Advisory
    /// in full: a refusal here leaves the quota call with an empty project body.
    async fn discover_project(&self, access_token: &str) -> Option<String> {
        let request = self
            .client
            .get(self.projects_url())
            .bearer_auth(access_token)
            .build()
            .ok()?;
        let body = super::request(PROVIDER_ID, &self.client, request)
            .await
            .ok()?;
        let projects: Projects = serde_json::from_str(&body).ok()?;
        pick_project(&projects.projects)
    }

    fn quota_request(
        &self,
        access_token: &str,
        project: Option<&str>,
    ) -> Result<reqwest::Request, ProviderError> {
        let body = match project {
            Some(project) => serde_json::json!({ "project": project }).to_string(),
            None => "{}".to_owned(),
        };
        self.client
            .post(self.quota_url())
            .bearer_auth(access_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }
}

impl fmt::Debug for Gemini {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gemini")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Gemini {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        self.tidemark_account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// Where the Gemini CLI keeps its Google login: `~/.gemini/oauth_creds.json`, or `None`
/// when `$HOME` does not name an absolute directory.
///
/// Free-standing rather than a method so that a caller can ask whether the CLI's login
/// exists on this machine without building the provider.
pub fn cli_credentials_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| Path::new(home).is_absolute())?;
    Some(credentials_path(Path::new(&home)))
}

fn credentials_path(home: &Path) -> PathBuf {
    home.join(".gemini/oauth_creds.json")
}

/// The OAuth client the refresh grant is presented with: the environment's pair when both
/// are set, the CLI's public constants otherwise.
fn client_credentials(env_id: Option<String>, env_secret: Option<String>) -> (String, String) {
    let id = nonblank(env_id).unwrap_or_else(|| CLI_CLIENT_ID.to_owned());
    let secret = nonblank(env_secret).unwrap_or_else(|| CLI_CLIENT_SECRET.to_owned());
    (id, secret)
}

fn nonblank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// One model family's daily allowance, as the card names it.
///
/// The matching is upstream's, on the lower-cased model id: `flash-lite` must be tested
/// before `flash`, or it would file as Flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Pro,
    Flash,
    FlashLite,
}

impl Tier {
    /// Card order: the families as upstream lists them, Pro first.
    const ALL: [Self; 3] = [Self::Pro, Self::Flash, Self::FlashLite];

    fn of(model_id: &str) -> Option<Self> {
        let id = model_id.to_ascii_lowercase();
        if id.contains("flash-lite") {
            Some(Self::FlashLite)
        } else if id.contains("flash") {
            Some(Self::Flash)
        } else if id.contains("pro") {
            Some(Self::Pro)
        } else {
            None
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Pro => "pro",
            Self::Flash => "flash",
            Self::FlashLite => "flash-lite",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Pro => "Pro",
            Self::Flash => "Flash",
            Self::FlashLite => "Flash-Lite",
        }
    }
}

/// Turns a quota response into a snapshot: one window per tier, drawn at the lowest
/// fraction any of the tier's models reported.
pub fn parse_quota(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_quota_for_account(body, captured_at, &AccountId::default())
}

fn parse_quota_for_account(
    body: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let response: QuotaResponse = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a Gemini quota response: {error}"))
    })?;
    let buckets = response
        .buckets
        .filter(|buckets| !buckets.is_empty())
        .ok_or_else(|| ProviderError::malformed("the Gemini quota response named no buckets"))?;

    // The lowest fraction per tier wins — a model family's buckets are the same allowance
    // seen through different token counts, and the tightest one is the truth.
    let mut lowest = [None; 3];
    for bucket in buckets {
        let Some(model_id) = bucket.model_id else {
            continue;
        };
        let Some(tier) = Tier::of(&model_id) else {
            continue;
        };
        // A family bucket that names no fraction is a recognised tier refusing to say how
        // much of its allowance is left — fail rather than drop the tier from the card.
        let fraction = bucket.remaining_fraction.ok_or_else(|| {
            ProviderError::malformed(format!(
                "the Gemini quota response names {model_id} without a remaining fraction"
            ))
        })?;
        let index = Tier::ALL
            .iter()
            .position(|known| *known == tier)
            .expect("every tier is in ALL");
        let reset = bucket.reset_time.as_deref().and_then(instant);
        match lowest[index] {
            Some((seen, _)) if fraction >= seen => {}
            _ => lowest[index] = Some((fraction, reset)),
        }
    }

    let length = WindowLength::from_secs(DAY_SECS).expect("a day is not zero");
    let span = crate::providers::length_title(length);
    let mut windows = Vec::new();
    for (index, tier) in Tier::ALL.into_iter().enumerate() {
        let Some((fraction, reset)) = lowest[index] else {
            continue;
        };
        windows.push(Window {
            key: WindowKey::for_pool(tier.slug(), length),
            title: format!("{} · {span}", tier.title()),
            subtitle: None,
            used_percent: ((1.0 - fraction) * 100.0).clamp(0.0, 100.0),
            resets_at: reset,
            length: Some(length),
        });
    }
    if windows.is_empty() {
        return Err(ProviderError::malformed(
            "the Gemini quota response named no model tiers we could read",
        ));
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaResponse {
    #[serde(default)]
    buckets: Option<Vec<QuotaBucket>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaBucket {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    remaining_fraction: Option<f64>,
    #[serde(default)]
    reset_time: Option<String>,
}

/// What `loadCodeAssist` says about the account: a tier to name on the card and, usually,
/// the Cloud Code project the quota is metered against.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeAssist {
    #[serde(default)]
    current_tier: Option<TierInfo>,
    #[serde(default)]
    paid_tier: Option<TierInfo>,
    #[serde(default)]
    cloudaicompanion_project: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TierInfo {
    #[serde(default)]
    name: Option<String>,
}

impl CodeAssist {
    /// What the account's plan is called, the paid tier's name when there is one.
    fn display_tier(&self) -> Option<String> {
        self.paid_tier
            .as_ref()
            .and_then(|tier| tier.name.clone())
            .or_else(|| {
                self.current_tier
                    .as_ref()
                    .and_then(|tier| tier.name.clone())
            })
            .filter(|name| !name.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct Projects {
    #[serde(default)]
    projects: Vec<ProjectRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRef {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

impl ProjectRef {
    fn is_gemini_project(&self) -> bool {
        let Some(id) = self.project_id.as_deref() else {
            return false;
        };
        id.starts_with("gen-lang-client")
            || self
                .labels
                .as_ref()
                .is_some_and(|labels| labels.contains_key("generative-language"))
    }
}

/// The resource manager's pick, in list order: the CLI's own auto-created project, or one
/// labelled as serving the Generative Language API.
fn pick_project(projects: &[ProjectRef]) -> Option<String> {
    projects
        .iter()
        .find(|project| project.is_gemini_project())
        .and_then(|project| project.project_id.clone())
}

/// The credential document as `oauth_creds.json` holds it: flat, with the expiry instant
/// in milliseconds where the CLI wrote it.
#[derive(Debug, Default, Deserialize)]
struct Stored {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expiry_date: Option<i64>,
}

impl Stored {
    fn from_document(document: &serde_json::Value) -> Result<Self, ProviderError> {
        let raw: Self = serde_json::from_value(document.clone()).map_err(|error| {
            ProviderError::malformed(format!(
                "the Gemini CLI credentials are not readable: {error}"
            ))
        })?;
        Ok(Self {
            access_token: nonblank(raw.access_token),
            refresh_token: nonblank(raw.refresh_token),
            id_token: nonblank(raw.id_token),
            expiry_date: raw.expiry_date,
        })
    }

    /// The upstream rule: an absent access token, or one whose own expiry instant is
    /// past, is refreshed before it is spent. A token that does not say when it expires
    /// is spent as it stands, and the quota endpoint's 401 is the announcement.
    fn needs_refresh(&self, now_ms: i64) -> bool {
        self.access_token.is_none() || self.expiry_date.is_some_and(|expiry| expiry < now_ms)
    }
}

#[derive(Debug, Deserialize)]
struct Refreshed {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
}

impl Refreshed {
    /// The root fields a rotation replaces. This grant never returns a refresh token, so
    /// the one in the file stays the one in the file; the id token is written only when
    /// the response carried a new one.
    fn fields(&self, now_ms: i64) -> Vec<(&'static str, serde_json::Value)> {
        let mut fields = vec![(
            "access_token",
            serde_json::Value::from(self.access_token.clone()),
        )];
        if let Some(expires_in) = self.expires_in {
            fields.push((
                "expiry_date",
                serde_json::Value::from(now_ms.saturating_add(expires_in.saturating_mul(1000))),
            ));
        }
        if let Some(id_token) = self
            .id_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            fields.push(("id_token", serde_json::Value::from(id_token)));
        }
        fields
    }
}

/// The payload claims of a JWT, signature unchecked — the token is read to label the
/// card, never to decide whether to trust it; Google's API remains the only judge.
fn claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// A reset instant as the quota response states it, RFC 3339.
fn instant(raw: &str) -> Option<Timestamp> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(|time| Timestamp::from_unix(time.unix_timestamp()).ok())
}

fn file_error(error: CredentialFileError) -> ProviderError {
    match error {
        CredentialFileError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
            ProviderError::NoCredential
        }
        CredentialFileError::Json(_) | CredentialFileError::RootNotObject => {
            ProviderError::malformed(error.to_string())
        }
        other => ProviderError::Local(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Read, Write};
    #[cfg(unix)]
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(unix)]
    use std::sync::mpsc;
    use tidemark_types::{Timestamp, WindowKey, WindowLength};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// The recorded `retrieveUserQuota` body of `GeminiAPITestHelpers.sampleQuotaResponse`,
    /// one bucket per tier.
    const QUOTA: &str = include_str!("../../../tests/fixtures/gemini/quota.json");
    /// The recorded `loadCodeAssistStandardTierResponse` — a tier, no project of its own.
    #[cfg(unix)]
    const LOAD: &str = include_str!("../../../tests/fixtures/gemini/load.json");

    struct TestHome {
        dir: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tidemark-gemini-test-{}-{serial}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join(".gemini")).expect("test directory");
            Self { dir }
        }

        fn path(&self) -> &Path {
            &self.dir
        }

        fn write_credentials(&self, document: serde_json::Value) {
            fs::write(
                self.path().join(".gemini/oauth_creds.json"),
                document.to_string(),
            )
            .expect("write credentials");
        }

        fn write_settings(&self, selected_type: &str) {
            fs::write(
                self.path().join(".gemini/settings.json"),
                json!({"security": {"auth": {"selectedType": selected_type}}}).to_string(),
            )
            .expect("write settings");
        }

        #[cfg(unix)]
        fn document(&self) -> serde_json::Value {
            serde_json::from_str(
                &fs::read_to_string(self.path().join(".gemini/oauth_creds.json"))
                    .expect("credentials readable"),
            )
            .expect("credentials are JSON")
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// The payload of a recorded id token, in the shape `GeminiAPITestHelpers.makeIDToken`
    /// builds: header, base64url payload carrying the email, no signature to speak of.
    fn id_token(email: &str) -> String {
        let payload = json!({ "email": email }).to_string();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.sig")
    }

    /// Credentials whose access token expired in 2001, so the first poll must refresh.
    /// `unrelated` stands for every field the CLI owns that we do not.
    #[cfg(unix)]
    fn expired_credentials() -> serde_json::Value {
        json!({
            "access_token": "old-access",
            "refresh_token": "fixture-refresh",
            "id_token": id_token("user@example.com"),
            "expiry_date": 1_000_000_000_000_i64,
            "unrelated": "kept",
        })
    }

    /// Credentials whose access token lives into next century: no refresh is due.
    fn live_credentials() -> serde_json::Value {
        json!({
            "access_token": "file-access",
            "id_token": id_token("user@example.com"),
            "expiry_date": 4_000_000_000_000_i64,
        })
    }

    /// A loopback server answering the given routes in order, asserting each request
    /// opens with its expected request line. Bodies are runtime strings because the
    /// refresh response carries a built id token.
    #[cfg(unix)]
    fn chained_server(
        routes: Vec<(&'static str, u16, &'static str, String)>,
    ) -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for (expected, status, headers, body) in routes {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("reads request line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line
                        .to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                    {
                        content_length = value;
                    }
                    request.push_str(&line);
                }
                if content_length > 0 {
                    let mut body_bytes = vec![0_u8; content_length];
                    reader
                        .read_exact(&mut body_bytes)
                        .expect("reads request body");
                    request.push_str(&String::from_utf8_lossy(&body_bytes));
                }
                drop(reader);
                assert!(
                    request.starts_with(expected),
                    "expected {expected}, got: {request}"
                );
                request_tx.send(request).expect("sends request");
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("writes response");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    #[cfg(unix)]
    fn route(
        expected: &'static str,
        status: u16,
        body: &str,
    ) -> (&'static str, u16, &'static str, String) {
        (expected, status, "", body.to_owned())
    }

    fn day() -> WindowLength {
        WindowLength::from_secs(86_400).expect("a day is not zero")
    }

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn fetch(provider: &Gemini) -> Result<Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    #[test]
    fn the_recorded_quota_draws_one_window_per_tier_at_the_stated_remainders() {
        let snapshot = parse_quota(QUOTA, at(1_767_225_600)).expect("parses the response");

        assert_eq!(snapshot.windows.len(), 3);
        let pro = &snapshot.windows[0];
        assert_eq!(pro.key, WindowKey::for_pool("pro", day()));
        assert_eq!(pro.title, "Pro · 1 day");
        assert_eq!(pro.length, Some(day()));
        assert!((pro.used_percent - 40.0).abs() < 0.000_001);
        assert_eq!(pro.resets_at, Some(at(1_735_689_600)));
        let flash = &snapshot.windows[1];
        assert_eq!(flash.key, WindowKey::for_pool("flash", day()));
        assert!((flash.used_percent - 10.0).abs() < 0.000_001);
        let flash_lite = &snapshot.windows[2];
        assert_eq!(flash_lite.key, WindowKey::for_pool("flash-lite", day()));
        assert!((flash_lite.used_percent - 20.0).abs() < 0.000_001);
        assert!(
            snapshot.details.is_empty(),
            "the quota body names no account"
        );
    }

    #[test]
    fn the_lowest_fraction_of_a_model_reported_twice_is_the_one_drawn() {
        // `GeminiAPITestHelpers.sampleFlashQuotaResponse`: the same model twice, the
        // fractions 0.9 and 0.4 — the input-token bucket is the lower one.
        let body = r#"{"buckets":[
            {"modelId":"gemini-2.5-flash","remainingFraction":0.9,"resetTime":"2025-01-01T00:00:00Z"},
            {"modelId":"gemini-2.5-flash","remainingFraction":0.4,"resetTime":"2025-01-01T00:00:00Z"}]}"#;

        let snapshot = parse_quota(body, at(1_700_000_000)).expect("parses the response");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key, WindowKey::for_pool("flash", day()));
        assert!((snapshot.windows[0].used_percent - 60.0).abs() < 0.000_001);
    }

    #[test]
    fn a_model_outside_the_tiers_is_skipped() {
        // A model the three families cannot classify draws no window anyone could size;
        // the flash bucket beside it still does.
        let body = r#"{"buckets":[
            {"modelId":"gemini-exp-1206","remainingFraction":0.5,"resetTime":"2025-01-01T00:00:00Z"},
            {"modelId":"gemini-2.5-flash","remainingFraction":0.25,"resetTime":"2025-01-01T00:00:00Z"}]}"#;

        let snapshot = parse_quota(body, at(1_700_000_000)).expect("parses the response");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key, WindowKey::for_pool("flash", day()));
        assert!((snapshot.windows[0].used_percent - 75.0).abs() < 0.000_001);
    }

    #[test]
    fn a_known_tier_bucket_without_a_fraction_fails_rather_than_hiding_the_tier() {
        // Dropping the fraction-less pro bucket would draw flash alone and read as the
        // whole account, hiding the tier the response itself named.
        let body = r#"{"buckets":[
            {"modelId":"gemini-2.5-pro","resetTime":"2025-01-01T00:00:00Z"},
            {"modelId":"gemini-2.5-flash","remainingFraction":0.25,"resetTime":"2025-01-01T00:00:00Z"}]}"#;

        let result = parse_quota(body, at(1_700_000_000));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_quota_body_without_usable_buckets_is_malformed() {
        for body in [
            "not json",
            "{}",
            r#"{"buckets": []}"#,
            r#"{"buckets": [{"modelId":"mystery-model","remainingFraction":0.5}]}"#,
        ] {
            let result = parse_quota(body, at(1_700_000_000));
            assert!(
                matches!(result, Err(ProviderError::Malformed(_))),
                "{body} must not parse into an answer"
            );
        }
    }

    #[test]
    fn the_credentials_file_is_the_gemini_directory_of_the_given_home() {
        let home = TestHome::new();
        home.write_credentials(live_credentials());

        assert_eq!(
            credentials_path(home.path()),
            home.path().join(".gemini/oauth_creds.json")
        );
        assert!(credentials_path(home.path()).exists());
    }

    // The unix loopback-server + advisory-lock fixtures (blocking reads that
    // would-block on Windows sockets, mandatory file locks) are unix-only;
    // a_quota_request... HANGS on Windows without this gate.
    #[cfg(unix)]
    #[test]
    fn an_expired_token_is_refreshed_merged_back_into_the_file_and_used() {
        let home = TestHome::new();
        home.write_credentials(expired_credentials());
        home.write_settings("oauth-personal");
        let refreshed = r#"{"access_token":"new-access","expires_in":3600,"id_token":""#.to_owned()
            + &id_token("user@example.com")
            + r#""}"#;
        let (base, requests, server) = chained_server(vec![
            route("POST /token", 200, &refreshed),
            route("POST /v1internal:loadCodeAssist", 200, LOAD),
            route("GET /v1/projects", 200, r#"{"projects": []}"#),
            route("POST /v1internal:retrieveUserQuota", 200, QUOTA),
        ]);
        let provider = Gemini::for_test(home.path(), &base).expect("builds");

        let snapshot = fetch(&provider).expect("the chain succeeds");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 3);
        let token_request = requests.recv().expect("token request").to_ascii_lowercase();
        assert!(token_request.starts_with("post /token "), "{token_request}");
        assert!(
            token_request.contains("content-type: application/x-www-form-urlencoded"),
            "{token_request}"
        );
        assert!(
            token_request.contains("grant_type=refresh_token"),
            "{token_request}"
        );
        assert!(
            token_request.contains("refresh_token=fixture-refresh"),
            "{token_request}"
        );
        assert!(
            token_request.contains("client_id=test-client-id"),
            "{token_request}"
        );
        assert!(
            token_request.contains("client_secret=test-client-secret"),
            "{token_request}"
        );
        let load_request = requests.recv().expect("load request");
        assert!(
            load_request.starts_with("POST /v1internal:loadCodeAssist "),
            "{load_request}"
        );
        assert!(
            load_request.contains("authorization: Bearer new-access"),
            "{load_request}"
        );
        assert!(
            load_request.contains(r#""metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}"#),
            "{load_request}"
        );
        let projects_request = requests.recv().expect("projects request");
        assert!(
            projects_request.starts_with("GET /v1/projects "),
            "{projects_request}"
        );
        let quota_request = requests.recv().expect("quota request");
        assert!(
            quota_request.starts_with("POST /v1internal:retrieveUserQuota "),
            "{quota_request}"
        );
        assert!(
            quota_request.contains("authorization: Bearer new-access"),
            "{quota_request}"
        );
        assert!(
            quota_request.trim_end().ends_with("{}"),
            "no project anywhere, so an empty body: {quota_request}"
        );

        let after = home.document();
        assert_eq!(after["access_token"], "new-access");
        assert_eq!(
            after["refresh_token"], "fixture-refresh",
            "this grant does not rotate the refresh token"
        );
        assert_eq!(after["id_token"], id_token("user@example.com"));
        assert_eq!(
            after["unrelated"], "kept",
            "a field the CLI owns must survive the merge"
        );
        assert!(
            after["expiry_date"].as_i64().expect("a new expiry") > 1_000_000_000_000,
            "the expiry instant moves to the fresh token's"
        );

        assert_eq!(snapshot.details.len(), 2);
        assert_eq!(snapshot.details[0].title, "Account");
        assert_eq!(snapshot.details[0].rows[0].label, "Email");
        assert_eq!(snapshot.details[0].rows[0].value, "user@example.com");
        assert_eq!(snapshot.details[1].title, "Plan");
        assert_eq!(snapshot.details[1].rows[0].label, "Tier");
        assert_eq!(snapshot.details[1].rows[0].value, "Standard");
    }

    // The unix loopback-server + advisory-lock fixtures (blocking reads that
    // would-block on Windows sockets, mandatory file locks) are unix-only;
    // a_quota_request... HANGS on Windows without this gate.
    #[cfg(unix)]
    #[test]
    fn the_quota_request_carries_the_project_code_assist_named() {
        let home = TestHome::new();
        home.write_credentials(live_credentials());
        let load_body = r#"{"currentTier":{"id":"free-tier","name":"free"},"cloudaicompanionProject":"managed-project-123"}"#;
        let (base, requests, server) = chained_server(vec![
            route("POST /v1internal:loadCodeAssist", 200, load_body),
            route("POST /v1internal:retrieveUserQuota", 200, QUOTA),
        ]);
        let provider = Gemini::for_test(home.path(), &base).expect("builds");

        let snapshot = fetch(&provider).expect("the chain succeeds");
        server.join().expect("server exits");

        let _load = requests.recv().expect("load request");
        let quota_request = requests.recv().expect("quota request");
        assert!(
            quota_request.contains(r#""project":"managed-project-123""#),
            "{quota_request}"
        );
        assert!(
            requests.try_recv().is_err(),
            "the resource manager is not asked when code assist names a project"
        );
        assert_eq!(snapshot.details[0].rows[0].value, "user@example.com");
        assert_eq!(snapshot.details[1].rows[0].value, "Free");
    }

    #[test]
    fn a_project_comes_from_the_resource_manager_when_code_assist_names_none() {
        let listed: Projects = serde_json::from_str(
            r#"{"projects":[
                {"projectId":"unrelated"},
                {"projectId":"gen-lang-client-123"},
                {"projectId":"labeled-only","labels":{"generative-language":"enabled"}}]}"#,
        )
        .expect("fixture JSON");
        assert_eq!(
            pick_project(&listed.projects).as_deref(),
            Some("gen-lang-client-123"),
            "the CLI's own auto-created project wins in list order"
        );

        let labeled: Projects = serde_json::from_str(
            r#"{"projects":[{"projectId":"labeled-only","labels":{"generative-language":"enabled"}}]}"#,
        )
        .expect("fixture JSON");
        assert_eq!(
            pick_project(&labeled.projects).as_deref(),
            Some("labeled-only")
        );

        let empty: Projects = serde_json::from_str(r#"{"projects": []}"#).expect("fixture JSON");
        assert_eq!(pick_project(&empty.projects), None);
    }

    // The unix loopback-server + advisory-lock fixtures (blocking reads that
    // would-block on Windows sockets, mandatory file locks) are unix-only;
    // a_quota_request... HANGS on Windows without this gate.
    #[cfg(unix)]
    #[test]
    fn a_quota_request_rejected_as_unauthorized_is_no_credential() {
        let home = TestHome::new();
        home.write_credentials(live_credentials());
        let (base, _requests, server) = chained_server(vec![
            route("POST /v1internal:loadCodeAssist", 200, LOAD),
            route("GET /v1/projects", 200, r#"{"projects": []}"#),
            route("POST /v1internal:retrieveUserQuota", 401, "{}"),
        ]);
        let provider = Gemini::for_test(home.path(), &base).expect("builds");

        let result = fetch(&provider);
        server.join().expect("server exits");

        assert!(
            matches!(result, Err(ProviderError::NoCredential)),
            "{result:?}"
        );
    }

    // The unix loopback-server + advisory-lock fixtures (blocking reads that
    // would-block on Windows sockets, mandatory file locks) are unix-only;
    // a_quota_request... HANGS on Windows without this gate.
    #[cfg(unix)]
    #[test]
    fn a_credential_file_replaced_during_the_exchange_is_never_overlaid() {
        // The CLI replaces the file atomically and honors no advisory lock. A write-back
        // from an exchange that started against the old document would overlay a newer
        // login with older tokens; the compare-and-skip must leave it alone while the
        // poll still spends its own freshly obtained access token.
        let home = TestHome::new();
        home.write_credentials(expired_credentials());
        home.write_settings("oauth-personal");
        let refreshed = r#"{"access_token":"new-access","expires_in":3600,"id_token":""#.to_owned()
            + &id_token("user@example.com")
            + r#""}"#;
        let cli_document = json!({
            "access_token": "cli-access",
            "refresh_token": "cli-refresh",
            "id_token": id_token("cli@example.com"),
            "expiry_date": 4_000_000_000_000_i64,
            "unrelated": "cli-kept",
        })
        .to_string();

        let credentials = home.path().join(".gemini/oauth_creds.json");
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            for (index, (expected, status, body)) in [
                ("POST /token", 200, refreshed),
                ("POST /v1internal:loadCodeAssist", 200, LOAD.to_owned()),
                ("GET /v1/projects", 200, r#"{"projects": []}"#.to_owned()),
                ("POST /v1internal:retrieveUserQuota", 200, QUOTA.to_owned()),
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("reads request line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    request.push_str(&line);
                }
                assert!(
                    request.starts_with(expected),
                    "expected {expected}: {request}"
                );
                if index == 0 {
                    // The CLI's atomic replacement lands while the grant is in flight.
                    fs::write(&credentials, &cli_document).expect("the CLI replaces the file");
                }
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("writes response");
            }
        });
        let provider = Gemini::for_test(home.path(), &format!("http://{address}")).expect("builds");

        let snapshot = fetch(&provider).expect("the poll succeeds on its own fresh token");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 3);
        let after = home.document();
        assert_eq!(
            after["refresh_token"], "cli-refresh",
            "the newer login's refresh token survives"
        );
        assert_eq!(
            after["access_token"], "cli-access",
            "the older exchange must not clobber the CLI's newer token"
        );
        assert_eq!(after["unrelated"], "cli-kept");
    }

    // The unix loopback-server + advisory-lock fixtures (blocking reads that
    // would-block on Windows sockets, mandatory file locks) are unix-only;
    // a_quota_request... HANGS on Windows without this gate.
    #[cfg(unix)]
    #[test]
    fn a_spent_token_with_nothing_to_refresh_it_is_no_credential_without_a_request() {
        let home = TestHome::new();
        home.write_credentials(json!({
            "access_token": "spent",
            "expiry_date": 1_000_000_000_000_i64,
        }));
        // Port 9 has nothing listening: were a request attempted, the error could not be
        // `NoCredential`.
        let provider = Gemini::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

        let error = fetch(&provider).expect_err("nothing to refresh with");

        assert!(matches!(error, ProviderError::NoCredential), "{error:?}");
    }

    #[test]
    fn a_home_without_a_credentials_file_has_no_credential() {
        let home = TestHome::new();
        let provider = Gemini::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

        let error = fetch(&provider).expect_err("the CLI has never signed in here");

        assert!(matches!(error, ProviderError::NoCredential), "{error:?}");
    }

    #[test]
    fn another_selected_auth_type_points_at_the_cli_sign_in() {
        let home = TestHome::new();
        home.write_credentials(live_credentials());
        home.write_settings("gemini-api-key");
        let provider = Gemini::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

        let error = fetch(&provider).expect_err("API-key mode is not this card's to read");

        match error {
            ProviderError::Local(message) => {
                assert!(message.contains("gemini"), "{message}");
                assert!(message.contains("sign in"), "{message}");
            }
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[test]
    fn the_client_credentials_fall_back_to_the_public_constants_the_cli_ships() {
        let (id, secret) = client_credentials(None, None);
        assert_eq!(
            id,
            "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
        );
        assert_eq!(secret, "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");

        let (id, _) = client_credentials(Some("  env-id  ".into()), None);
        assert_eq!(id, "env-id", "an override replaces the constant whole");
        let (id, _) = client_credentials(Some("   ".into()), Some("env-secret".into()));
        assert_eq!(
            id, "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com",
            "a blank override is no override"
        );
        let (_, secret) = client_credentials(None, Some("env-secret".into()));
        assert_eq!(secret, "env-secret");
    }
}
