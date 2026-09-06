//! Notion AI credits, read from the browser session that signs in to the app.
//!
//! The wire is a two-call chain: `getSpaces` names the account's workspaces, and
//! `getCreditRateLimitStatus` answers the rolling and billing-period windows for the one
//! workspace the account points at — the configured space id when there is one, the first
//! workspace otherwise. The rolling reset arrives as seconds from now, so it is anchored
//! to the reading's own clock. A browser session is selected explicitly, never
//! substituted from another profile.

use super::{HandSpec, OptionSchema, Options, ProviderError, http, redact_query, session};
#[cfg(test)]
use crate::browser::auth::Selection;
use crate::browser::{self, Keyring, SafeStorage};
use crate::providers::{BoxFuture, Credential, Provider};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey, WindowLength,
};

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "notion";

/// The setting under `[provider.notion]` that names the workspace to report on.
const WORKSPACE: &str = "workspace";

const SPACES_URL: &str = "https://app.notion.com/api/v3/getSpaces";
const STATUS_URL: &str = "https://app.notion.com/api/v3/getCreditRateLimitStatus";
const SESSION_URL: &str = "https://app.notion.com/";
/// Without `token_v2` the API answers 401 for every call, so it alone gates the jar.
const SESSION_COOKIE_NAMES: &[&str] = &["token_v2"];
/// The app moved to `app.notion.com`; the `notion.so` domains stay for sessions that
/// predate the move.
const COOKIE_DOMAINS: &[&str] = &[
    "app.notion.com",
    "www.notion.com",
    "notion.com",
    "www.notion.so",
    "notion.so",
];
/// The billing period's length when the wire names none: the calendar-month sentinel the
/// workspace's `periodEndMs` resolves against.
const MONTH_SECS: u64 = 2_592_000;

/// Notion as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Notion",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in notion.com browser session.",
    options: OPTIONS,
    build,
};

/// The browser pair every session provider publishes, plus the workspace picker.
static OPTIONS: &[OptionSchema] = &[
    OptionSchema {
        name: session::AUTH_BROWSER,
        title: "Browser",
        description: None,
        default: "",
        choices: &[],
        required: false,
    },
    OptionSchema {
        name: session::AUTH_PROFILE,
        title: "Browser profile",
        description: None,
        default: "",
        choices: &[],
        required: false,
    },
    OptionSchema {
        name: WORKSPACE,
        title: "Workspace",
        description: Some("The workspace's space id; blank uses the first workspace."),
        default: "",
        choices: &[],
        required: false,
    },
];

fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Notion::new_for_account(
        account,
        &credential,
        options,
    )?))
}

/// One Notion account, authenticated by one explicitly chosen browser profile.
pub struct Notion {
    tidemark_account: AccountId,
    client: reqwest::Client,
    /// The root the browser scan is taken under, and a test fixture in every build that
    /// states one: production leaves it unset so that each platform's own browser layout
    /// decides where profiles live. A browser home is not a vendor home — Windows keeps
    /// browser profiles under `%LOCALAPPDATA%`/`%APPDATA%`, never under the user's own
    /// profile directory — so rooting the scan at one would find no browser there at all.
    browser_home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    source: Option<session::Source>,
    workspace: Option<String>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Notion {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(
            AccountId::default(),
            &Credential::new(String::new()),
            options,
        )
    }

    fn new_for_account(
        account_id: AccountId,
        credential: &Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: http::client()?,
            browser_home: None,
            storage: Arc::new(Keyring),
            source: session::source(credential, options),
            workspace: options
                .get(WORKSPACE)
                .map(String::as_str)
                .map(str::trim)
                .filter(|workspace| !workspace.is_empty())
                .map(str::to_owned),
            #[cfg(test)]
            base_url: None,
        })
    }

    #[cfg(test)]
    fn for_test(
        home: &Path,
        storage: Arc<dyn SafeStorage>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: http::client()?,
            browser_home: Some(home.to_path_buf()),
            storage,
            source: Some(session::Source::Browser(Selection {
                browser: "firefox".into(),
                profile: None,
            })),
            workspace: None,
            base_url: Some(base_url.trim_end_matches('/').to_owned()),
        })
    }

    fn spaces_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/api/v3/getSpaces");
        }
        SPACES_URL.to_owned()
    }

    fn status_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/api/v3/getCreditRateLimitStatus");
        }
        STATUS_URL.to_owned()
    }

    fn request(
        &self,
        url: &str,
        cookie: &str,
        body: String,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::COOKIE, cookie)
            .body(body)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let source = self.source.as_ref().ok_or(ProviderError::NoCredential)?;
        let session = session::session(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            source,
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            SESSION_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let spaces = super::request(
            PROVIDER_ID,
            &self.client,
            self.request(&self.spaces_url(), &session.header, "{}".to_owned())?,
        )
        .await?;
        let space = workspace_id(&spaces, self.workspace.as_deref())?;
        let body = serde_json::json!({ "spaceId": space }).to_string();
        let status = super::request(
            PROVIDER_ID,
            &self.client,
            self.request(&self.status_url(), &session.header, body)?,
        )
        .await?;
        parse_for_account(
            &spaces,
            &status,
            self.workspace.as_deref(),
            Timestamp::now(),
            &self.tidemark_account,
        )
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.request(SPACES_URL, header, "{}".to_owned()) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        match super::validate(&self.client, request).await {
            Ok(()) => crate::browser::auth::Validation::Ready,
            Err(ProviderError::Credential { status: 401 | 403 }) => {
                crate::browser::auth::Validation::Rejected
            }
            Err(_) => crate::browser::auth::Validation::Unreachable,
        }
    }

    async fn inspect_sources(&self) -> Vec<AuthCandidate> {
        let browsers = session::inspect_sources(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            SPACES_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await;
        session::modes(
            browsers,
            self.source.as_ref().and_then(session::Source::pasted),
            |header| async move { self.validate_header(&header).await },
        )
        .await
    }
}

impl fmt::Debug for Notion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notion")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Notion {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        self.tidemark_account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }

    fn inspect_auth_sources(&self) -> BoxFuture<'_, Result<Vec<AuthCandidate>, ProviderError>> {
        Box::pin(async { Ok(self.inspect_sources().await) })
    }
}

fn cookie_query() -> browser::Query {
    browser::Query::new(COOKIE_DOMAINS.iter().copied(), Vec::<String>::new())
}

struct Account {
    email: Option<String>,
    workspaces: Vec<Workspace>,
}

struct Workspace {
    id: String,
    name: Option<String>,
    tier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollingWindow {
    window: Option<String>,
    used: Option<f64>,
    limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingWindow {
    used: Option<f64>,
    limit: Option<f64>,
    period_end_ms: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    status: Option<String>,
    window: Option<RollingWindow>,
    resets_in_seconds: Option<f64>,
    billing_period_window: Option<BillingWindow>,
}

/// Turns the `getSpaces` and `getCreditRateLimitStatus` bodies into the rolling and
/// billing-period windows.
///
/// A window object that is present but states no readable usage pair is a recognised
/// window failing to describe itself and fails the fetch: skipping it would paint the
/// remaining window as the whole truth, and drawing it at zero would paint headroom the
/// wire never stated.
pub fn parse(
    spaces_body: &str,
    status_body: &str,
    preferred: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    parse_for_account(
        spaces_body,
        status_body,
        preferred,
        captured_at,
        &AccountId::default(),
    )
}

fn parse_for_account(
    spaces_body: &str,
    status_body: &str,
    preferred: Option<&str>,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let account = parse_spaces(spaces_body)?;
    let workspace = resolve_workspace(&account, preferred)?;
    let status: Status = serde_json::from_str(status_body).map_err(|error| {
        ProviderError::malformed(format!("not a Notion rate-limit status: {error}"))
    })?;
    if status
        .status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("not_applicable"))
    {
        return Err(ProviderError::Local(format!(
            "Notion AI credits are not available for workspace {}",
            workspace.name.as_deref().unwrap_or(&workspace.id)
        )));
    }
    if status.window.is_none() && status.billing_period_window.is_none() {
        return Err(ProviderError::malformed(
            "Notion rate-limit status returned no usage windows",
        ));
    }

    let mut windows = Vec::new();
    if let Some(rolling) = &status.window {
        let (used, limit) = measurable(rolling.used, rolling.limit).ok_or_else(|| {
            ProviderError::malformed("Notion rolling window states no readable usage pair")
        })?;
        // A rolling window the wire calls a month has no honest length: it is the billing
        // period, so it is drawn name-keyed rather than mislabeled "30 days" — and never
        // keyed where it would collide with the billing window below.
        let (key, title, length) = match rolling.window.as_deref().and_then(token_secs) {
            Some(secs) if secs != MONTH_SECS => {
                let length = WindowLength::from_secs(secs).expect("a provider span is not zero");
                (
                    WindowKey::for_length(length),
                    span_title(secs),
                    Some(length),
                )
            }
            _ => (WindowKey::named("rolling"), "Rolling".to_owned(), None),
        };
        windows.push(Window {
            key,
            title,
            subtitle: Some(format!(
                "{} / {} credits",
                credit_text(used),
                credit_text(limit)
            )),
            used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
            resets_at: status
                .resets_in_seconds
                .filter(|seconds| *seconds >= 0.0)
                .and_then(|seconds| {
                    Timestamp::from_unix(captured_at.as_unix() + seconds as i64).ok()
                }),
            length,
        });
    }
    if let Some(billing) = &status.billing_period_window {
        let (used, limit) = measurable(billing.used, billing.limit).ok_or_else(|| {
            ProviderError::malformed("Notion billing window states no readable usage pair")
        })?;
        let length = WindowLength::from_secs(MONTH_SECS).expect("a fixed span is not zero");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: "Monthly".to_owned(),
            subtitle: Some(format!(
                "{} / {} credits",
                credit_text(used),
                credit_text(limit)
            )),
            used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
            resets_at: billing
                .period_end_ms
                .filter(|milliseconds| *milliseconds > 0.0)
                .and_then(|milliseconds| Timestamp::from_unix(milliseconds as i64 / 1000).ok()),
            length: Some(length),
        });
    }

    let mut rows = Vec::new();
    if let Some(name) = workspace.name.as_deref() {
        rows.push(DetailRow {
            label: "Workspace".to_owned(),
            value: name.to_owned(),
        });
    }
    if let Some(tier) = workspace
        .tier
        .as_deref()
        .map(str::trim)
        .filter(|tier| !tier.is_empty())
    {
        rows.push(DetailRow {
            label: "Tier".to_owned(),
            value: tier.to_owned(),
        });
    }
    if let Some(email) = account
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
    {
        rows.push(DetailRow {
            label: "Email".to_owned(),
            value: email.to_owned(),
        });
    }
    let details = if rows.is_empty() {
        Vec::new()
    } else {
        vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows,
        }]
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details,
    })
}

/// A usable usage pair: both numbers on the wire, the limit positive.
fn measurable(used: Option<f64>, limit: Option<f64>) -> Option<(f64, f64)> {
    Some((used?, limit?)).filter(|(_, limit)| *limit > 0.0)
}

/// The space id the status call should ask about.
fn workspace_id(spaces_body: &str, preferred: Option<&str>) -> Result<String, ProviderError> {
    let account = parse_spaces(spaces_body)?;
    Ok(resolve_workspace(&account, preferred)?.id.clone())
}

fn parse_spaces(body: &str) -> Result<Account, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not a getSpaces body: {error}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("getSpaces response is not a JSON object"))?;
    let Some(user_id) = resolve_user_id(root) else {
        return Err(ProviderError::malformed(
            "getSpaces response did not identify a single user",
        ));
    };
    let container = &root[&user_id];

    let mut email = None;
    if let Some(users) = container.get("notion_user").and_then(Value::as_object) {
        let record = users
            .get(&user_id)
            .and_then(|raw| unwrap_record(raw).cloned())
            .or_else(|| users.values().find_map(|raw| unwrap_record(raw).cloned()));
        email = record
            .as_ref()
            .and_then(|record| record.get("email"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }

    let mut workspaces = Vec::new();
    if let Some(spaces) = container.get("space").and_then(Value::as_object) {
        for (key, raw) in spaces {
            let Some(record) = unwrap_record(raw) else {
                continue;
            };
            workspaces.push(Workspace {
                id: record
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    .to_owned(),
                name: record
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                tier: record
                    .get("subscription_tier")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }

    Ok(Account { email, workspaces })
}

/// Picks the key whose own `notion_user` record identifies it, rather than trusting key
/// order; a single-key payload without the self-identifying id is still unambiguous.
fn resolve_user_id(root: &Map<String, Value>) -> Option<String> {
    let identified: Vec<&String> = root
        .keys()
        .filter(|key| {
            root.get(*key)
                .and_then(Value::as_object)
                .and_then(|container| container.get("notion_user"))
                .and_then(Value::as_object)
                .and_then(|users| users.get(*key))
                .and_then(unwrap_record)
                .and_then(|record| record.get("id"))
                .and_then(Value::as_str)
                .is_some_and(|id| id == key.as_str())
        })
        .collect();
    if identified.len() == 1 {
        return Some(identified[0].clone());
    }
    if identified.is_empty() && root.len() == 1 {
        return root.keys().next().cloned();
    }
    None
}

/// Records arrive as `{"value": {…}}` or, on newer responses, `{"value": {"value": {…}}}`.
fn unwrap_record(raw: &Value) -> Option<&Map<String, Value>> {
    let outer = raw.as_object()?;
    let Some(value) = outer.get("value").and_then(Value::as_object) else {
        return Some(outer);
    };
    Some(
        value
            .get("value")
            .and_then(Value::as_object)
            .unwrap_or(value),
    )
}

/// The configured workspace when its id matches, the first workspace otherwise — ids
/// compared the way Notion writes them, ignoring case and dashes.
fn resolve_workspace<'a>(
    account: &'a Account,
    preferred: Option<&str>,
) -> Result<&'a Workspace, ProviderError> {
    if let Some(preferred) = preferred
        .map(normalize_id)
        .filter(|preferred| !preferred.is_empty())
        && let Some(found) = account
            .workspaces
            .iter()
            .find(|workspace| normalize_id(&workspace.id) == preferred)
    {
        return Ok(found);
    }
    account.workspaces.first().ok_or_else(|| {
        ProviderError::malformed("getSpaces response returned no workspace for this account")
    })
}

/// Lowercases and strips the dashes a UUID carries, so a configured id matches however
/// the response spelled it.
fn normalize_id(raw: &str) -> String {
    raw.trim().to_lowercase().replace('-', "")
}

/// Reads a rolling-window length token such as `6h`.
fn token_secs(token: &str) -> Option<u64> {
    let token = token.trim().to_lowercase();
    let unit = token.chars().next_back()?;
    let value: u64 = token[..token.len() - 1].trim().parse().ok()?;
    if value == 0 {
        return None;
    }
    match unit {
        'm' => Some(value),
        'h' => value.checked_mul(3_600),
        'd' => value.checked_mul(86_400),
        'w' => value.checked_mul(604_800),
        _ => None,
    }
}

fn span_title(secs: u64) -> String {
    let (size, unit) = if secs.is_multiple_of(86_400) {
        (secs / 86_400, "day")
    } else if secs.is_multiple_of(3_600) {
        (secs / 3_600, "hour")
    } else {
        (secs / 60, "minute")
    };
    if size == 1 {
        format!("1 {unit}")
    } else {
        format!("{size} {unit}s")
    }
}

fn credit_text(value: f64) -> String {
    let rounded = format!("{value:.2}");
    let trimmed = rounded.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{Notion, parse};
    use crate::browser::SafeStorage;
    use crate::providers::{Provider, ProviderError};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use tidemark_types::{Timestamp, WindowKey, WindowLength};

    #[derive(Debug)]
    struct NoKeyring;

    impl SafeStorage for NoKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn gecko_home() -> crate::browser::tests::TestHome {
        let home = crate::browser::tests::TestHome::new();
        let connection = Connection::open(home.gecko(".mozilla/firefox/Default")).expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                    isSecure INTEGER, isHttpOnly INTEGER
                );",
            )
            .expect("creates");
        connection
            .execute(
                "INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('app.notion.com', 'token_v2', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the Notion routes in order: `getSpaces` first,
    /// then `getCreditRateLimitStatus`, because the second needs the first's answer. A
    /// rejected first answer stops the chain, so a test that expects a refusal passes
    /// one route only — the server never waits for a connection that will not come.
    fn chained_server(
        routes: &'static [(&'static str, u16, &'static str)],
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for (path, status_code, body) in routes {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("reads request line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("content-length: ") {
                        content_length = value.trim().parse().expect("content length");
                    }
                    request.push_str(&line);
                }
                let mut request_body = String::new();
                (&mut reader)
                    .take(content_length)
                    .read_to_string(&mut request_body)
                    .expect("reads request body");
                request.push_str(&request_body);
                drop(reader);
                assert!(
                    request.starts_with(&format!("POST /api/v3/{path}")),
                    "routes are asked in order: {request}"
                );
                request_tx.send(request).expect("sends request");
                write!(
                    stream,
                    "HTTP/1.1 {status_code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("writes response");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Notion {
        Notion::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Notion) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const SPACES: &str = include_str!("../../../tests/fixtures/notion/get-spaces.json");
    const STATUS: &str =
        include_str!("../../../tests/fixtures/notion/get-credit-rate-limit-status.json");

    fn key_of(secs: u64) -> WindowKey {
        WindowKey::for_length(WindowLength::from_secs(secs).expect("a fixed span"))
    }

    #[test]
    fn the_recorded_bodies_draw_the_rolling_and_monthly_windows() {
        // Sorting the workspace keys any other way would report Acme's allowance as
        // Personal's; the first workspace in sorted order is the one asked about.
        let at = Timestamp::from_unix(1_740_000_000).expect("plausible");
        let snapshot = parse(SPACES, STATUS, None, at).expect("parses the recorded bodies");
        let rolling = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(6 * 3_600))
            .expect("rolling window");
        let monthly = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(2_592_000))
            .expect("monthly window");

        assert!((rolling.used_percent - 42.5).abs() < 0.01);
        assert_eq!(rolling.title, "6 hours");
        assert_eq!(
            rolling.resets_at,
            Some(Timestamp::from_unix(1_740_012_600).expect("plausible"))
        );
        assert_eq!(rolling.subtitle.as_deref(), Some("42.5 / 100 credits"));
        assert!((monthly.used_percent - 18.0).abs() < 0.01);
        assert_eq!(
            monthly.resets_at,
            Some(Timestamp::from_unix(1_788_000_000).expect("plausible"))
        );
        assert_eq!(snapshot.details[0].rows[0].value, "Acme");
        assert_eq!(snapshot.details[0].rows[1].value, "business");
        assert_eq!(snapshot.details[0].rows[2].value, "person@example.com");
    }

    #[test]
    fn a_configured_workspace_wins_over_the_first_one() {
        let snapshot = parse(
            SPACES,
            STATUS,
            Some("66666666-7777-8888-9999-aaaaaaaaaaaa"),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded bodies");

        assert_eq!(snapshot.details[0].rows[0].value, "Personal");
        assert_eq!(snapshot.details[0].rows[1].value, "free");
    }

    #[test]
    fn a_present_window_without_a_readable_pair_fails_rather_than_hiding() {
        // Skipping the unmeasurable rolling window would paint the billing window as the
        // whole truth; drawing it at zero would paint headroom the wire never stated.
        let body = r#"{
          "status": "within_limit",
          "window": { "window": "6h", "used": 42.5 },
          "billingPeriodWindow": { "used": 18.0, "limit": 100, "periodEndMs": 1788000000000 }
        }"#;
        let result = parse(
            SPACES,
            body,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_status_that_reports_no_windows_is_malformed() {
        // Every field being optional means an error envelope decodes cleanly; accepting it
        // would report 0% used on a workspace that may be at its cap.
        let result = parse(
            SPACES,
            r#"{"error":"unauthorised"}"#,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_not_applicable_status_names_its_workspace() {
        let result = parse(
            SPACES,
            r#"{"status":"not_applicable"}"#,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Local(_))));
    }

    #[test]
    fn the_status_request_receives_the_space_id_from_the_first_response() {
        // Asking about the wrong space would report another workspace's allowance.
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[
            ("getSpaces", 200, SPACES),
            ("getCreditRateLimitStatus", 200, STATUS),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the status");
        let spaces_request = requests
            .recv()
            .expect("spaces request")
            .to_ascii_lowercase();
        let status_request = requests
            .recv()
            .expect("status request")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "notion");
        assert!(spaces_request.starts_with("post /api/v3/getspaces"));
        assert!(spaces_request.ends_with("{}"));
        assert!(
            status_request.starts_with("post /api/v3/getcreditratelimitstatus"),
            "{status_request}"
        );
        assert!(
            status_request.ends_with(
                r#"{"spaceid":"11111111-2222-3333-4444-555555555555"}"#
                    .to_lowercase()
                    .as_str()
            ),
            "{status_request}"
        );
        for request in [spaces_request.as_str(), status_request.as_str()] {
            assert!(
                request.contains("cookie: token_v2=chosen-session"),
                "{request}"
            );
            assert!(
                request.contains("content-type: application/json"),
                "{request}"
            );
        }
    }

    #[test]
    fn an_unauthorised_spaces_response_asks_for_a_new_browser_session() {
        // Mapping 401 to a transient error would retry forever instead of showing the selected session expired.
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[("getSpaces", 401, "{}")]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }
}
