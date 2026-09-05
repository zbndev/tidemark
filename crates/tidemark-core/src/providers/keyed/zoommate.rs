//! ZoomMate's browser session is exchanged for a short-lived bearer token on every poll.
//!
//! The bearer is intentionally not cached in v1: the browser's `_zm_*` SSO cookies are the
//! durable credential, and a fresh `nak` avoids making another secret lifetime persistent.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
#[cfg(test)]
use crate::browser::auth::Selection;
use crate::browser::{self, Keyring, SafeStorage};
use crate::providers::{BoxFuture, Credential, Provider};
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey, WindowLength,
};

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "zoommate";

const AI_HOST: &str = "ai.zoom.us";
const ZOOMMATE_HOST: &str = "zoommate.zoom.us";
const LOGIN_PATH: &str = "/ai-computer/api/v1/login/?continue=https://zoommate.zoom.us/";
const CREDITS_STATUS_PATH: &str = "/ai-computer/api/v1/credits/status";
const COOKIE_DOMAINS: &[&str] = &["zoom.us", "ai.zoom.us", "zoommate.zoom.us"];
const SESSION_PREFIX: &str = "_zm_";
const ORIGIN: &str = "https://zoommate.zoom.us";

/// ZoomMate as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "ZoomMate",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in zoommate.zoom.us browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(ZoomMate::new_for_account(
        account,
        &credential,
        options,
    )?))
}

/// One ZoomMate account, authenticated by its chosen browser profile.
pub struct ZoomMate {
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
    #[cfg(test)]
    base_url: Option<String>,
    #[cfg(test)]
    fallback_base_url: Option<String>,
}

impl ZoomMate {
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
            #[cfg(test)]
            base_url: None,
            #[cfg(test)]
            fallback_base_url: None,
        })
    }

    #[cfg(test)]
    fn for_test(
        home: &std::path::Path,
        storage: Arc<dyn SafeStorage>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: http::client()?,
            browser_home: Some(home.to_path_buf()),
            storage,
            source: None,
            base_url: Some(base_url.trim_end_matches('/').to_owned()),
            fallback_base_url: None,
        })
    }

    #[cfg(test)]
    fn with_selection(mut self, selection: Selection) -> Self {
        self.source = Some(session::Source::Browser(selection));
        self
    }

    #[cfg(test)]
    fn with_test_fallback(mut self, base_url: &str) -> Self {
        self.fallback_base_url = Some(base_url.trim_end_matches('/').to_owned());
        self
    }

    fn cookie_url(host: &str, path: &str) -> String {
        format!("https://{host}{path}")
    }

    fn request_url(&self, host: &str, path: &str) -> String {
        #[cfg(test)]
        if host == ZOOMMATE_HOST
            && let Some(base_url) = &self.fallback_base_url
        {
            return format!("{base_url}{path}");
        }
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}{path}");
        }
        Self::cookie_url(host, path)
    }

    async fn session(&self, url: &str) -> Result<session::Session, ProviderError> {
        let source = self.source.as_ref().ok_or(ProviderError::NoCredential)?;
        session::session_prefix(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            source,
            SESSION_PREFIX,
            &cookie_query(),
            url,
        )
        .await?
        .ok_or(ProviderError::NoCredential)
    }

    fn login_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::REFERER, ORIGIN)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    fn credits_request(
        &self,
        url: &str,
        cookie: &str,
        nak: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {nak}"))
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::REFERER, ORIGIN)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// Reads the bootstrap body without sending it to the optional raw-response log: it contains
    /// `data.nak`, a bearer credential rather than quota evidence.
    async fn login(&self, host: &str) -> Result<Login, ProviderError> {
        let cookie_url = Self::cookie_url(host, LOGIN_PATH);
        let session = self.session(&cookie_url).await?;
        let request = self.login_request(&self.request_url(host, LOGIN_PATH), &session.header)?;
        let response = self
            .client
            .execute(request)
            .await
            .map_err(|error| ProviderError::Transport(redact_query(error)))?;
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(response.status(), retry_after.as_deref())?;
        let body = response
            .text()
            .await
            .map_err(|error| ProviderError::Transport(redact_query(error)))?;
        parse_login(&body)
    }

    async fn fetch_from(&self, host: &str) -> Result<Snapshot, ProviderError> {
        let login = self.login(host).await?;
        let cookie_url = Self::cookie_url(host, CREDITS_STATUS_PATH);
        let session = self.session(&cookie_url).await?;
        let request = self.credits_request(
            &self.request_url(host, CREDITS_STATUS_PATH),
            &session.header,
            &login.nak,
        )?;
        let body = super::request(PROVIDER_ID, &self.client, request).await?;
        let mut snapshot = parse_for_account(&body, Timestamp::now(), &self.tidemark_account)?;
        if let Some(email) = login.email {
            snapshot.details.push(DetailSection {
                title: "Account".to_owned(),
                rows: vec![DetailRow {
                    label: "Email".to_owned(),
                    value: email,
                }],
            });
        }
        Ok(snapshot)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        match self.fetch_from(AI_HOST).await {
            Err(ProviderError::Transport(_)) => self.fetch_from(ZOOMMATE_HOST).await,
            result => result,
        }
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.login_request(&self.request_url(AI_HOST, LOGIN_PATH), header) else {
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
        let browsers = session::inspect_sources_prefix(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            SESSION_PREFIX,
            &cookie_query(),
            &Self::cookie_url(AI_HOST, LOGIN_PATH),
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

impl fmt::Debug for ZoomMate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZoomMate")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for ZoomMate {
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

#[derive(Debug, Deserialize)]
struct CreditsEnvelope {
    data: Option<CreditsData>,
}

#[derive(Debug, Deserialize)]
struct CreditsData {
    credit_status: Option<CreditStatus>,
}

#[derive(Debug, Deserialize)]
struct CreditStatus {
    budget_cap: Option<f64>,
    used_credit: Option<f64>,
    cycle_start_date: Option<i64>,
    cycle_end_date: Option<i64>,
    is_unlimited: Option<bool>,
}

#[derive(Deserialize)]
struct LoginEnvelope {
    data: Option<LoginData>,
}

#[derive(Deserialize)]
struct LoginData {
    nak: Option<String>,
    user_profile: Option<UserProfile>,
}

#[derive(Deserialize)]
struct UserProfile {
    email: Option<String>,
}

struct Login {
    nak: String,
    email: Option<String>,
}

/// Decodes the cookie bootstrap without exposing its bearer token to callers or diagnostics.
fn parse_login(body: &str) -> Result<Login, super::ProviderError> {
    let login: LoginEnvelope = serde_json::from_str(body).map_err(|error| {
        super::ProviderError::malformed(format!("unreadable ZoomMate login: {error}"))
    })?;
    let data = login
        .data
        .ok_or_else(|| super::ProviderError::malformed("ZoomMate login has no data"))?;
    let nak = data
        .nak
        .map(|nak| nak.trim().to_owned())
        .filter(|nak| !nak.is_empty())
        .ok_or_else(|| super::ProviderError::malformed("ZoomMate login has no nak"))?;
    Ok(Login {
        nak,
        email: data
            .user_profile
            .and_then(|profile| profile.email)
            .map(|email| email.trim().to_owned())
            .filter(|email| !email.is_empty()),
    })
}

/// Turns ZoomMate's reported credit cycle into one quota window.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, super::ProviderError> {
    parse_for_account(body, captured_at, &AccountId::default())
}

fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, super::ProviderError> {
    let envelope: CreditsEnvelope = serde_json::from_str(body).map_err(|error| {
        super::ProviderError::malformed(format!("unreadable ZoomMate credits status: {error}"))
    })?;
    let status = envelope
        .data
        .and_then(|data| data.credit_status)
        .ok_or_else(|| {
            super::ProviderError::malformed("ZoomMate credits status has no credit_status")
        })?;
    let unlimited = status.is_unlimited.unwrap_or(false);
    let budget = limited_credit(status.budget_cap, "budget_cap", unlimited)?;
    let used = limited_credit(status.used_credit, "used_credit", unlimited)?;
    let cycle_start = timestamp(status.cycle_start_date, "cycle_start_date")?;
    let cycle_end = timestamp(status.cycle_end_date, "cycle_end_date")?;
    let length = match (cycle_start, cycle_end) {
        (Some(start), Some(end)) if end > start => {
            let seconds = u64::try_from(start.seconds_until(end)).map_err(|_| {
                super::ProviderError::malformed("invalid ZoomMate credit cycle length")
            })?;
            WindowLength::from_secs(seconds)
        }
        (Some(_), Some(_)) => {
            return Err(super::ProviderError::malformed(
                "ZoomMate credit cycle ends before it starts",
            ));
        }
        _ => None,
    };
    let used_percent = if unlimited || budget <= 0.0 {
        0.0
    } else {
        (used / budget * 100.0).clamp(0.0, 100.0)
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows: vec![Window {
            key: length
                .map(WindowKey::for_length)
                .unwrap_or_else(|| WindowKey::named("credits")),
            title: "Credits".to_owned(),
            subtitle: None,
            used_percent,
            resets_at: (!unlimited && budget > 0.0).then_some(cycle_end).flatten(),
            length,
        }],
        details: Vec::new(),
    })
}

fn limited_credit(
    value: Option<f64>,
    name: &str,
    unlimited: bool,
) -> Result<f64, super::ProviderError> {
    match value {
        Some(value) if value.is_finite() => Ok(value),
        Some(_) => Err(super::ProviderError::malformed(format!(
            "invalid ZoomMate {name}"
        ))),
        None if unlimited => Ok(0.0),
        None => Err(super::ProviderError::malformed(format!(
            "ZoomMate limited credits status has no {name}"
        ))),
    }
}

fn timestamp(value: Option<i64>, name: &str) -> Result<Option<Timestamp>, super::ProviderError> {
    value
        .filter(|value| *value > 0)
        .map(|milliseconds| {
            Timestamp::from_unix(milliseconds / 1_000).map_err(|error| {
                super::ProviderError::malformed(format!("invalid ZoomMate {name}: {error}"))
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::{PROVIDER_ID, ZoomMate, parse, parse_login};
    use crate::browser::{SafeStorage, auth::Selection};
    use crate::providers::{BoxFuture, Provider};
    use crate::secrets::SecretError;
    use rusqlite::{Connection, params};
    use serde::Deserialize;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, mpsc};
    use std::thread;
    use tidemark_types::{Timestamp, WindowKey, WindowLength};

    const CREDITS: &str = include_str!("../../../tests/fixtures/zoommate/credits-status.json");
    const LOGIN: &str = include_str!("../../../tests/fixtures/zoommate/login.json");
    const COOKIE_SCOPE: &str =
        include_str!("../../../tests/fixtures/zoommate/issue-2507-cookie-scope.json");

    #[derive(Debug)]
    struct NoKeyring;

    impl SafeStorage for NoKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> BoxFuture<'_, Result<Option<String>, SecretError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[derive(Deserialize)]
    struct CookieScope {
        records: Vec<CookieRecord>,
    }

    #[derive(Deserialize)]
    struct CookieRecord {
        #[serde(rename = "sourceDomain")]
        source_domain: String,
        name: String,
        value: String,
    }

    #[test]
    fn a_credits_status_body_becomes_the_reported_cycle_window() {
        let captured_at = Timestamp::from_unix(1_700_000_000).expect("plausible capture time");

        let snapshot = parse(CREDITS, captured_at).expect("recorded credit status parses");

        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(
            window.key,
            WindowKey::for_length(WindowLength::from_secs(2_678_399).expect("nonzero cycle"))
        );
        assert_eq!(window.title, "Credits");
        assert!((window.used_percent - 5.492_102_065_6).abs() < 1e-9);
        assert_eq!(window.resets_at, Timestamp::from_unix(1_896_134_399).ok());
    }

    #[test]
    fn a_login_bootstrap_exposes_its_email_without_exposing_the_bearer() {
        let login = parse_login(LOGIN).expect("recorded login parses");

        assert_eq!(login.email.as_deref(), Some("fake.user@example.com"));
    }

    #[test]
    fn a_limited_credits_status_without_a_budget_is_malformed() {
        let mut body: serde_json::Value = serde_json::from_str(CREDITS).expect("fixture parses");
        body["data"]["credit_status"]
            .as_object_mut()
            .expect("credit status is an object")
            .remove("budget_cap");

        let result = parse(&body.to_string(), Timestamp::now());

        assert!(matches!(
            result,
            Err(crate::providers::ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn a_limited_credits_status_without_usage_is_malformed() {
        let mut body: serde_json::Value = serde_json::from_str(CREDITS).expect("fixture parses");
        body["data"]["credit_status"]
            .as_object_mut()
            .expect("credit status is an object")
            .remove("used_credit");

        let result = parse(&body.to_string(), Timestamp::now());

        assert!(matches!(
            result,
            Err(crate::providers::ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn a_credits_status_with_a_reversed_cycle_is_malformed() {
        let mut body: serde_json::Value = serde_json::from_str(CREDITS).expect("fixture parses");
        let status = body["data"]["credit_status"]
            .as_object_mut()
            .expect("credit status is an object");
        let end = status["cycle_end_date"]
            .as_i64()
            .expect("cycle end is milliseconds");
        status.insert(
            "cycle_start_date".to_owned(),
            serde_json::Value::from(end + 1_000),
        );

        let result = parse(&body.to_string(), Timestamp::now());

        assert!(matches!(
            result,
            Err(crate::providers::ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn a_selected_browser_profile_mints_a_bearer_then_reads_only_ai_scoped_cookies() {
        let home = crate::browser::tests::TestHome::new();
        let database = home.gecko(".zen/zz99.Working");
        let scope: CookieScope = serde_json::from_str(COOKIE_SCOPE).expect("scope fixture parses");
        let connection = Connection::open(database).expect("opens cookie database");
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
            .expect("creates cookie table");
        for record in scope.records {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES (?1, ?2, ?3, '/', 0, 1, 0, 0, 0)",
                    params![record.source_domain, record.name, record.value],
                )
                .expect("inserts scoped cookie");
        }
        connection
            .execute(
                "INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('.zoom.us', '_zm_ssid', 'selected-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts ZoomMate session");
        drop(connection);

        let (base, requests, server) = two_request_server();
        let provider = ZoomMate::for_test(home.path(), Arc::new(NoKeyring), &base)
            .expect("builds")
            .with_selection(Selection {
                browser: "zen".into(),
                profile: Some("zz99.Working".into()),
            });

        let snapshot = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
            .expect("fetches ZoomMate credits");
        assert_eq!(snapshot.details[0].rows[0].value, "fake.user@example.com");

        let mint = requests.recv().expect("mint request captured");
        let credits = requests.recv().expect("credits request captured");
        server.join().expect("server stops");
        for request in [&mint, &credits] {
            let headers = request.to_ascii_lowercase();
            assert!(
                headers.contains("cookie: parent=fake"),
                "parent cookie reaches ai: {request}"
            );
            assert!(
                request.contains("_zm_ssid=selected-session"),
                "session reaches ai: {request}"
            );
            assert!(
                request.contains("ai-only=fake"),
                "ai cookie reaches ai: {request}"
            );
            assert!(
                !request.contains("parent-host-only=fake"),
                "zoom.us host cookie stays home"
            );
            assert!(
                !request.contains("mate-only=fake"),
                "ZoomMate host cookie stays home"
            );
            assert!(
                !request.contains("marketing-only=fake"),
                "marketing cookie stays home"
            );
        }
        assert!(mint.starts_with("GET /ai-computer/api/v1/login/?continue="));
        assert!(credits.starts_with("GET /ai-computer/api/v1/credits/status "));
        let credits_headers = credits.to_ascii_lowercase();
        assert!(credits_headers.contains("authorization: bearer fake-minted-jwt"));
        assert!(credits_headers.contains("origin: https://zoommate.zoom.us"));
        assert!(credits_headers.contains("referer: https://zoommate.zoom.us"));
    }

    #[test]
    fn a_minted_bearer_never_reaches_the_raw_response_log() {
        let home = crate::browser::tests::TestHome::new();
        let database = home.gecko(".zen/zz99.Working");
        let connection = Connection::open(database).expect("opens cookie database");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                    isSecure INTEGER, isHttpOnly INTEGER
                );
                INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('.zoom.us', '_zm_ssid', 'selected-session', '/', 0, 1, 0, 0, 0);",
            )
            .expect("creates signed-in profile");
        drop(connection);
        let log = crate::debug::enable(home.path()).expect("enables raw response logging");
        let (base, requests, server) = two_request_server();
        let provider = ZoomMate::for_test(home.path(), Arc::new(NoKeyring), &base)
            .expect("builds")
            .with_selection(Selection {
                browser: "zen".into(),
                profile: Some("zz99.Working".into()),
            });

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
            .expect("fetches ZoomMate credits");
        let _ = requests.recv().expect("mint request captured");
        let _ = requests.recv().expect("credits request captured");
        server.join().expect("server stops");
        crate::debug::disable();

        let recorded = std::fs::read_to_string(log).expect("debug log reads");
        assert!(
            !recorded.contains("fake-minted-jwt"),
            "the login bearer must never be written to the raw-response log"
        );
    }

    #[test]
    fn a_connection_failure_on_ai_zoom_us_restarts_the_chain_on_zoommate_zoom_us() {
        let home = crate::browser::tests::TestHome::new();
        let database = home.gecko(".zen/zz99.Working");
        let connection = Connection::open(database).expect("opens cookie database");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                    isSecure INTEGER, isHttpOnly INTEGER
                );
                INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('.zoom.us', '_zm_ssid', 'selected-session', '/', 0, 1, 0, 0, 0);",
            )
            .expect("creates signed-in profile");
        drop(connection);
        let unavailable = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("reserves unused address");
            format!("http://{}", listener.local_addr().expect("address"))
        };
        let (fallback, requests, server) = two_request_server();
        let provider = ZoomMate::for_test(home.path(), Arc::new(NoKeyring), &unavailable)
            .expect("builds")
            .with_test_fallback(&fallback)
            .with_selection(Selection {
                browser: "zen".into(),
                profile: Some("zz99.Working".into()),
            });

        let snapshot = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
            .expect("fallback fetches credits");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        assert!(
            requests
                .recv()
                .expect("fallback mint")
                .starts_with("GET /ai-computer/api/v1/login/")
        );
        assert!(
            requests
                .recv()
                .expect("fallback credits")
                .starts_with("GET /ai-computer/api/v1/credits/status ")
        );
        server.join().expect("server stops");
    }

    #[test]
    fn an_ai_credential_rejection_does_not_retry_against_zoommate_zoom_us() {
        let home = crate::browser::tests::TestHome::new();
        let database = home.gecko(".zen/zz99.Working");
        let connection = Connection::open(database).expect("opens cookie database");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                    isSecure INTEGER, isHttpOnly INTEGER
                );
                INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('.zoom.us', '_zm_ssid', 'selected-session', '/', 0, 1, 0, 0, 0);",
            )
            .expect("creates signed-in profile");
        drop(connection);
        let (base, server) = one_response_server("401 Unauthorized", "{}");
        let unavailable = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("reserves unused address");
            format!("http://{}", listener.local_addr().expect("address"))
        };
        let provider = ZoomMate::for_test(home.path(), Arc::new(NoKeyring), &base)
            .expect("builds")
            .with_test_fallback(&unavailable)
            .with_selection(Selection {
                browser: "zen".into(),
                profile: Some("zz99.Working".into()),
            });

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch());
        server.join().expect("server stops");

        assert!(
            matches!(
                result,
                Err(crate::providers::ProviderError::Credential { status: 401 })
            ),
            "unexpected result: {result:?}"
        );
    }

    fn one_response_server(status: &str, body: &str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let status = status.to_owned();
        let body = body.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request accepted");
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header reads");
                if line == "\r\n" {
                    break;
                }
            }
            drop(reader);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("response writes");
        });
        (format!("http://{address}"), server)
    }

    fn two_request_server() -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for response in [LOGIN, CREDITS] {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header reads");
                    if line == "\r\n" {
                        request.push_str(&line);
                        break;
                    }
                    request.push_str(&line);
                }
                drop(reader);
                request_tx.send(request).expect("request captured");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .expect("response writes");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }
}
