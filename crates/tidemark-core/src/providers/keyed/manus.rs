//! Manus credits, read from the browser session that signs in to its dashboard.
//!
//! The API accepts the browser's `session_id` as a bearer token. Tidemark reads that value
//! only from the explicitly selected browser profile and never stores or logs it.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
#[cfg(test)]
use crate::browser::auth::Selection;
use crate::browser::{self, Keyring, SafeStorage};
use crate::providers::{BoxFuture, Credential, Provider};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "manus";

const CREDITS_URL: &str = "https://api.manus.im/user.v1.UserService/GetAvailableCredits";
const SESSION_URL: &str = "https://manus.im/";
const ORIGIN: &str = "https://manus.im";
const REFERER: &str = "https://manus.im/";
const SESSION_COOKIE_NAMES: &[&str] = &["session_id"];
const COOKIE_DOMAINS: &[&str] = &["manus.im", "www.manus.im"];

/// Manus as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Manus",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in manus.im browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Manus::new_for_account(
        account,
        &credential,
        options,
    )?))
}

/// One Manus account, authenticated by one explicitly chosen browser profile.
pub struct Manus {
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
}

impl Manus {
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
            base_url: Some(base_url.trim_end_matches('/').to_owned()),
        })
    }

    fn credits_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/user.v1.UserService/GetAvailableCredits");
        }
        CREDITS_URL.to_owned()
    }

    fn request(&self, url: &str, session_value: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {session_value}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::REFERER, REFERER)
            .header("Connect-Protocol-Version", "1")
            .body("{}")
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
        let body = super::request(
            PROVIDER_ID,
            &self.client,
            self.request(&self.credits_url(), &session.session_value)?,
        )
        .await?;
        parse_for_account(&body, Timestamp::now(), &self.tidemark_account)
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Some(session_value) = session_value(header) else {
            return crate::browser::auth::Validation::Rejected;
        };
        let Ok(request) = self.request(&self.credits_url(), session_value) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        // The platform can refuse a session inside a 200 envelope, so the proof must read
        // the body: a status-only check would call an expired login ready. An envelope the
        // parser cannot read at all is inconclusive, not a rejection.
        match super::validate_body(&self.client, request).await {
            Err(ProviderError::Credential { status: 401 | 403 }) => {
                crate::browser::auth::Validation::Rejected
            }
            Err(_) => crate::browser::auth::Validation::Unreachable,
            Ok(body) => match parse(&body, Timestamp::now()) {
                Ok(_) => crate::browser::auth::Validation::Ready,
                Err(_) => crate::browser::auth::Validation::Unreachable,
            },
        }
    }

    async fn inspect_sources(&self) -> Vec<AuthCandidate> {
        let browsers = session::inspect_sources(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            SESSION_URL,
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

impl fmt::Debug for Manus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manus")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Manus {
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

fn session_value(header: &str) -> Option<&str> {
    header
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("session_id="))
        .filter(|value| !value.is_empty())
}

struct Credits {
    total: f64,
    free: f64,
    periodic: f64,
    refresh: f64,
    max_refresh: f64,
    monthly: f64,
    next_refresh: Option<Timestamp>,
    refresh_interval: Option<String>,
}

/// Turns Manus's credit inventory into the stated monthly credit pool.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_for_account(body, captured_at, &AccountId::default())
}

fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not Manus credits: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("Manus credits root is not an object"))?;
    let data = ["data", "result", "response", "availableCredits"]
        .into_iter()
        .find_map(|key| object.get(key))
        .unwrap_or(&root)
        .as_object()
        .ok_or_else(|| ProviderError::malformed("Manus credits payload is not an object"))?;

    let known = [
        "totalCredits",
        "freeCredits",
        "periodicCredits",
        "addonCredits",
        "refreshCredits",
        "maxRefreshCredits",
        "proMonthlyCredits",
        "eventCredits",
    ];
    if known.iter().all(|key| !data.contains_key(*key)) {
        return Err(ProviderError::malformed(
            "Manus credits payload has no recognised credit field",
        ));
    }

    let credits = Credits {
        total: credit(data, "totalCredits")?.unwrap_or(0.0),
        free: credit(data, "freeCredits")?.unwrap_or(0.0),
        periodic: credit(data, "periodicCredits")?.unwrap_or(0.0),
        refresh: credit(data, "refreshCredits")?.unwrap_or(0.0),
        max_refresh: credit(data, "maxRefreshCredits")?.unwrap_or(0.0),
        monthly: credit(data, "proMonthlyCredits")?.unwrap_or(0.0),
        next_refresh: timestamp(data.get("nextRefreshTime"))?,
        refresh_interval: data
            .get("refreshInterval")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    };

    let mut windows = Vec::new();
    if credits.monthly > 0.0 {
        let used = (credits.monthly - credits.periodic).clamp(0.0, credits.monthly);
        windows.push(Window {
            key: WindowKey::named("credits"),
            title: "Credits".to_owned(),
            subtitle: Some(format!(
                "{} / {} used · {} left",
                number(used),
                number(credits.monthly),
                number(credits.periodic)
            )),
            used_percent: used / credits.monthly * 100.0,
            resets_at: credits.next_refresh,
            length: None,
        });
    }

    let mut rows = vec![DetailRow {
        label: "Total credits".to_owned(),
        value: number(credits.total),
    }];
    if credits.free > 0.0 {
        rows.push(DetailRow {
            label: "Free credits".to_owned(),
            value: number(credits.free),
        });
    }
    if credits.max_refresh > 0.0 {
        let label = credits
            .refresh_interval
            .as_deref()
            .map(capitalized)
            .unwrap_or_else(|| "Refresh".to_owned());
        rows.push(DetailRow {
            label: format!("{label} credits"),
            value: format!(
                "{} / {}",
                number(credits.refresh),
                number(credits.max_refresh)
            ),
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details: vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows,
        }],
    })
}

fn credit(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<f64>, ProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let value = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(number) => number.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| ProviderError::malformed(format!("Manus {field} is not a number")))?;
    if !value.is_finite() || value < 0.0 {
        return Err(ProviderError::malformed(format!(
            "Manus {field} must be a finite non-negative number"
        )));
    }
    Ok(Some(value))
}

fn timestamp(value: Option<&Value>) -> Result<Option<Timestamp>, ProviderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError::malformed("Manus nextRefreshTime is not an RFC 3339 time"))?;
    let value = OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        ProviderError::malformed(format!("invalid Manus nextRefreshTime: {error}"))
    })?;
    Timestamp::from_unix(value.unix_timestamp())
        .map(Some)
        .map_err(|error| {
            ProviderError::malformed(format!("invalid Manus nextRefreshTime: {error}"))
        })
}

fn number(value: f64) -> String {
    let digits = format!("{:.0}", value.round());
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.chars().rev().collect()
}

fn capitalized(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return "Refresh".to_owned();
    };
    first.to_uppercase().collect::<String>() + characters.as_str()
}

#[cfg(test)]
mod tests {
    use super::{Manus, parse};
    use crate::browser::SafeStorage;
    use crate::providers::{Provider, ProviderError};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use tidemark_types::{Timestamp, WindowKey};

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
                ) VALUES ('.manus.im', 'session_id', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    fn server(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
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
            request_tx.send(request).expect("sends request");
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("writes response");
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Manus {
        Manus::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Manus) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    #[test]
    fn a_recorded_credits_response_draws_the_monthly_credits_pool() {
        // Treating `periodicCredits` as consumed would invert the quota bar.
        let snapshot = parse(
            include_str!("../../../tests/fixtures/manus/credits.json"),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");
        let credits = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("credits"))
            .expect("credits window");

        assert!((credits.used_percent - 65.775).abs() < 0.001);
        assert_eq!(
            credits.resets_at,
            Some(Timestamp::from_unix(1_776_038_400).expect("plausible"))
        );
        assert_eq!(snapshot.details[0].rows[0].value, "2,869");
    }

    #[test]
    fn the_credits_request_uses_the_selected_session_as_a_bearer_token() {
        // Replacing the bearer value with the whole Cookie header makes Manus reject the request.
        let home = gecko_home();
        let (base_url, requests, server) = server(
            200,
            include_str!("../../../tests/fixtures/manus/credits.json"),
        );
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the credits");
        let request = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "manus");
        assert!(request.starts_with("post /user.v1.userservice/getavailablecredits http/1.1"));
        assert!(request.contains("authorization: bearer chosen-session"));
        assert!(request.contains("origin: https://manus.im"));
        assert!(request.contains("referer: https://manus.im/"));
        assert!(request.contains("connect-protocol-version: 1"));
        assert!(request.contains("content-type: application/json"));
        assert!(request.ends_with("{}"));
    }

    #[test]
    fn an_unauthorised_credits_response_asks_for_a_new_browser_session() {
        // Mapping 401 to a transient error would retry forever instead of showing the selected session expired.
        let home = gecko_home();
        let (base_url, _requests, server) = server(401, "{}");
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn an_inspection_proof_judges_the_envelope_and_not_only_the_status() {
        // A status-only proof would store an expired session as ready; an envelope the
        // parser cannot read at all is inconclusive rather than ready.
        for (body, expected) in [
            (
                include_str!("../../../tests/fixtures/manus/credits.json"),
                crate::browser::auth::Validation::Ready,
            ),
            (
                r#"{"error":"expired"}"#,
                crate::browser::auth::Validation::Unreachable,
            ),
        ] {
            let home = gecko_home();
            let (base_url, _requests, server) = server(200, body);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");

            let verdict = runtime
                .block_on(provider(&home, &base_url).validate_header("session_id=chosen-session"));
            server.join().expect("server exits");

            assert_eq!(verdict, expected, "{body}");
        }
    }

    #[test]
    fn a_credits_body_without_a_known_credit_field_is_malformed() {
        // Accepting a successful error envelope would paint a made-up zero quota.
        let home = gecko_home();
        let (base_url, _requests, server) = server(200, r#"{"error":"expired"}"#);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }
}
