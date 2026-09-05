//! Qoder credits, read from the browser session that signs in to its dashboard.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, AuthCandidateState, CredentialKind, ProviderId, Snapshot, Timestamp,
    Window, WindowKey,
};

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "qoder";

const INTERNATIONAL_URL: &str = "https://qoder.com/api/v2/me/usages/big_model_credits";
const CHINA_URL: &str = "https://qoder.com.cn/api/v2/me/usages/big_model_credits";
const INTERNATIONAL_ORIGIN: &str = "https://qoder.com";
const CHINA_ORIGIN: &str = "https://qoder.com.cn";
const COOKIE_DOMAINS: &[&str] = &[
    "qoder.com",
    "www.qoder.com",
    "qoder.com.cn",
    "www.qoder.com.cn",
];

/// Qoder as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Qoder",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in qoder.com or qoder.com.cn browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    _credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Qoder::new_for_account(account, options)?))
}

/// One Qoder account, authenticated by one explicitly chosen browser profile.
pub struct Qoder {
    tidemark_account: AccountId,
    client: reqwest::Client,
    /// The root the browser scan is taken under, and a test fixture in every build that
    /// states one: production leaves it unset so that each platform's own browser layout
    /// decides where profiles live. A browser home is not a vendor home — Windows keeps
    /// browser profiles under `%LOCALAPPDATA%`/`%APPDATA%`, never under the user's own
    /// profile directory — so rooting the scan at one would find no browser there at all.
    browser_home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    urls: Option<(String, String)>,
}

impl Qoder {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), options)
    }

    fn new_for_account(account_id: AccountId, options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: http::client()?,
            browser_home: None,
            storage: Arc::new(Keyring),
            selection: session::selection(options),
            #[cfg(test)]
            urls: None,
        })
    }

    #[cfg(test)]
    fn for_test(
        home: &std::path::Path,
        storage: Arc<dyn SafeStorage>,
        international: &str,
        china: &str,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: http::client()?,
            browser_home: Some(home.to_path_buf()),
            storage,
            selection: Some(Selection {
                browser: "firefox".into(),
                profile: None,
            }),
            urls: Some((
                format!(
                    "{}/api/v2/me/usages/big_model_credits",
                    international.trim_end_matches('/')
                ),
                format!(
                    "{}/api/v2/me/usages/big_model_credits",
                    china.trim_end_matches('/')
                ),
            )),
        })
    }

    fn url(&self, china: bool) -> String {
        #[cfg(test)]
        if let Some((international, china_url)) = &self.urls {
            return if china {
                china_url.clone()
            } else {
                international.clone()
            };
        }
        if china {
            CHINA_URL.into()
        } else {
            INTERNATIONAL_URL.into()
        }
    }

    fn request(
        &self,
        url: &str,
        cookie: &str,
        origin: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, origin)
            .header(reqwest::header::REFERER, format!("{origin}/account/usage"))
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Bx-V", "2.5.35")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn session(&self, url: &str) -> Result<String, ProviderError> {
        let selection = self.selection.as_ref().ok_or(ProviderError::NoCredential)?;
        session::session(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            selection,
            &[],
            &cookie_query(),
            url,
        )
        .await?
        .map(|session| session.header)
        .ok_or(ProviderError::NoCredential)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let body = match self.session(INTERNATIONAL_URL).await {
            Ok(cookie) => match self.fetch_site(false, &cookie).await {
                Ok(body) => body,
                Err(ProviderError::Credential { status: 401 | 403 }) => self.fetch_china().await?,
                Err(error) => return Err(error),
            },
            Err(ProviderError::NoCredential) => self.fetch_china().await?,
            Err(error) => return Err(error),
        };
        parse_for_account(&body, Timestamp::now(), &self.tidemark_account)
    }

    async fn fetch_china(&self) -> Result<String, ProviderError> {
        let cookie = self.session(CHINA_URL).await?;
        self.fetch_site(true, &cookie).await
    }

    async fn fetch_site(&self, china: bool, cookie: &str) -> Result<String, ProviderError> {
        let (origin, url) = if china {
            (CHINA_ORIGIN, self.url(true))
        } else {
            (INTERNATIONAL_ORIGIN, self.url(false))
        };
        super::request(
            PROVIDER_ID,
            &self.client,
            self.request(&url, cookie, origin)?,
        )
        .await
    }

    async fn validate_header(&self, header: &str, china: bool) -> crate::browser::auth::Validation {
        let (origin, url) = if china {
            (CHINA_ORIGIN, self.url(true))
        } else {
            (INTERNATIONAL_ORIGIN, self.url(false))
        };
        let Ok(request) = self.request(&url, header, origin) else {
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
        let international = session::inspect_sources(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            &[],
            &cookie_query(),
            INTERNATIONAL_URL,
            |credential| async move { self.validate_header(credential.header(), false).await },
        )
        .await;
        let china = session::inspect_sources(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            &[],
            &cookie_query(),
            CHINA_URL,
            |credential| async move { self.validate_header(credential.header(), true).await },
        )
        .await;
        merge_sources(international, china)
    }
}

fn merge_sources(
    mut primary: Vec<AuthCandidate>,
    secondary: Vec<AuthCandidate>,
) -> Vec<AuthCandidate> {
    for browser in secondary {
        let Some(existing) = primary
            .iter_mut()
            .find(|candidate| candidate.id == browser.id)
        else {
            primary.push(browser);
            continue;
        };
        for child in browser.children {
            match existing
                .children
                .iter_mut()
                .find(|candidate| candidate.id == child.id)
            {
                Some(current) if source_rank(child.state()) > source_rank(current.state()) => {
                    current.state = child.state;
                }
                Some(_) => {}
                None => existing.children.push(child),
            }
        }
        existing.state = aggregate_source_state(&existing.children)
            .as_wire()
            .to_owned();
    }
    primary
}

fn source_rank(state: Option<AuthCandidateState>) -> u8 {
    match state {
        Some(AuthCandidateState::Ready) => 6,
        Some(AuthCandidateState::WaitingForKeyring) => 5,
        Some(AuthCandidateState::Challenged) => 4,
        Some(AuthCandidateState::Unreachable) => 3,
        Some(AuthCandidateState::Rejected) => 2,
        Some(AuthCandidateState::Missing) | None => 1,
    }
}

fn aggregate_source_state(children: &[AuthCandidate]) -> AuthCandidateState {
    children
        .iter()
        .filter_map(AuthCandidate::state)
        .max_by_key(|state| source_rank(Some(*state)))
        .unwrap_or(AuthCandidateState::Missing)
}

impl fmt::Debug for Qoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Qoder")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Qoder {
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

/// Turns Qoder's recorded dashboard response into distinct total and shared credit pools.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_for_account(body, captured_at, &AccountId::default())
}

fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not Qoder usage: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("Qoder usage root is not an object"))?;
    let resets_at = timestamp(field(object, "nextResetAt", "next_reset_at"))?;
    let total = quota(object, "totalQuota", "total_quota")?
        .ok_or_else(|| ProviderError::malformed("Qoder usage has no totalQuota.quotaSummary"))?;
    let shared = quota(object, "sharedQuota", "shared_quota")?;

    let mut windows = vec![window("total", "Total credits", total, resets_at)];
    if let Some(shared) = shared {
        windows.push(window("shared", "Shared credits", shared, resets_at));
    }
    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

#[derive(Clone, Copy)]
struct Quota {
    used: f64,
    limit: f64,
    remaining: f64,
    percentage: f64,
}

fn quota(
    object: &serde_json::Map<String, Value>,
    camel: &str,
    snake: &str,
) -> Result<Option<Quota>, ProviderError> {
    let Some(container) = field(object, camel, snake) else {
        return Ok(None);
    };
    let container = container
        .as_object()
        .ok_or_else(|| ProviderError::malformed(format!("Qoder {camel} is not an object")))?;
    let summary = field(container, "quotaSummary", "quota_summary")
        .ok_or_else(|| ProviderError::malformed(format!("Qoder {camel} has no quotaSummary")))?
        .as_object()
        .ok_or_else(|| {
            ProviderError::malformed(format!("Qoder {camel} quotaSummary is not an object"))
        })?;
    let used = number(summary, "usedValue", "used_value")?;
    let limit = number(summary, "limitValue", "limit_value")?;
    let remaining = optional_number(summary, "remainingValue", "remaining_value")?
        .unwrap_or_else(|| (limit - used).max(0.0));
    if used < 0.0 || limit < 0.0 || remaining < 0.0 {
        return Err(ProviderError::malformed(
            "Qoder quota values must be nonnegative",
        ));
    }
    let percentage = optional_number(summary, "usagePercentage", "usage_percentage")?
        .unwrap_or_else(|| {
            if limit == 0.0 {
                100.0
            } else {
                used / limit * 100.0
            }
        });
    if !percentage.is_finite() {
        return Err(ProviderError::malformed(
            "Qoder usagePercentage must be finite",
        ));
    }
    if limit == 0.0 && (used != 0.0 || remaining != 0.0) {
        return Err(ProviderError::malformed(
            "Qoder zero total quota must have zero usage and remaining",
        ));
    }
    Ok(Some(Quota {
        used,
        limit,
        remaining,
        percentage,
    }))
}

fn field<'a>(
    object: &'a serde_json::Map<String, Value>,
    camel: &str,
    snake: &str,
) -> Option<&'a Value> {
    object.get(camel).or_else(|| object.get(snake))
}

fn number(
    object: &serde_json::Map<String, Value>,
    camel: &str,
    snake: &str,
) -> Result<f64, ProviderError> {
    optional_number(object, camel, snake)?
        .ok_or_else(|| ProviderError::malformed(format!("Qoder quotaSummary has no {camel}")))
}

fn optional_number(
    object: &serde_json::Map<String, Value>,
    camel: &str,
    snake: &str,
) -> Result<Option<f64>, ProviderError> {
    let Some(value) = field(object, camel, snake) else {
        return Ok(None);
    };
    let value = value
        .as_f64()
        .ok_or_else(|| ProviderError::malformed(format!("Qoder {camel} is not a number")))?;
    if !value.is_finite() {
        return Err(ProviderError::malformed(format!(
            "Qoder {camel} must be finite"
        )));
    }
    Ok(Some(value))
}

fn timestamp(value: Option<&Value>) -> Result<Option<Timestamp>, ProviderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let seconds = match value {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => {
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .ok()
                .map(|value| value.unix_timestamp() as f64)
        }
        _ => None,
    }
    .ok_or_else(|| ProviderError::malformed("Qoder nextResetAt is not a timestamp"))?;
    let seconds = if seconds > 10_000_000_000.0 {
        seconds / 1000.0
    } else {
        seconds
    };
    Timestamp::from_unix(seconds as i64)
        .map(Some)
        .map_err(|error| ProviderError::malformed(format!("invalid Qoder nextResetAt: {error}")))
}

fn window(key: &str, title: &str, quota: Quota, resets_at: Option<Timestamp>) -> Window {
    Window {
        key: WindowKey::named(key),
        title: title.to_owned(),
        subtitle: Some(format!(
            "{} / {} used · {} left",
            number_text(quota.used),
            number_text(quota.limit),
            number_text(quota.remaining),
        )),
        used_percent: quota.percentage,
        resets_at,
        length: None,
    }
}

fn number_text(value: f64) -> String {
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

#[cfg(test)]
mod tests {
    use super::{Qoder, parse};
    use crate::browser::SafeStorage;
    use crate::providers::{Provider, ProviderError};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use tidemark_types::{AuthCandidateState, Timestamp, WindowKey};

    #[test]
    fn a_recorded_usage_response_draws_total_and_shared_credit_pools() {
        let snapshot = parse(
            include_str!("../../../tests/fixtures/qoder/usage.json"),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");

        let total = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("total"))
            .expect("total window");
        let shared = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("shared"))
            .expect("shared window");

        assert_eq!(total.used_percent, 100.0);
        assert_eq!(shared.used_percent, 20.0);
        assert_eq!(total.length, None);
        assert_eq!(shared.length, None);
        assert_eq!(
            total.resets_at,
            Some(Timestamp::from_unix(1_725_148_800).expect("plausible"))
        );
    }

    #[test]
    fn a_recorded_snake_case_usage_response_draws_the_total_credit_pool() {
        let snapshot = parse(
            include_str!("../../../tests/fixtures/qoder/usage-snake.json"),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key, WindowKey::named("total"));
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
        assert_eq!(
            snapshot.windows[0].resets_at,
            Some(Timestamp::from_unix(1_725_148_800).expect("plausible"))
        );
    }

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
        for (domain, name, value) in [
            (".qoder.com", "sid", "selected-session"),
            (".qoder.com", "locale", "en"),
            (".qoder.com.cn", "cn-session", "fallback-session"),
        ] {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES (?1, ?2, ?3, '/', 0, 1, 0, 0, 0)",
                    (domain, name, value),
                )
                .expect("inserts a cookie");
        }
        home
    }

    fn gecko_home_with_only_a_china_cookie() -> crate::browser::tests::TestHome {
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
                ) VALUES ('.qoder.com.cn', 'cn-session', 'china-only-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts a cookie");
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
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("reads request line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
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

    fn provider(home: &crate::browser::tests::TestHome, international: &str, china: &str) -> Qoder {
        Qoder::for_test(home.path(), Arc::new(NoKeyring), international, china).expect("builds")
    }

    fn fetch(provider: &Qoder) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    #[test]
    fn a_credential_rejection_on_qoder_com_retries_qoder_com_cn() {
        let home = gecko_home();
        let (international, first_requests, first_server) = server(401, "{}");
        let (china, second_requests, second_server) = server(
            200,
            include_str!("../../../tests/fixtures/qoder/usage.json"),
        );

        let snapshot = fetch(&provider(&home, &international, &china)).expect("falls back");
        let first = first_requests
            .recv()
            .expect("first request captured")
            .to_ascii_lowercase();
        let second = second_requests
            .recv()
            .expect("second request captured")
            .to_ascii_lowercase();
        first_server.join().expect("first server exits");
        second_server.join().expect("second server exits");

        assert_eq!(snapshot.provider.as_str(), "qoder");
        assert!(first.starts_with("get /api/v2/me/usages/big_model_credits http/1.1"));
        assert!(first.contains("origin: https://qoder.com"));
        assert!(second.contains("origin: https://qoder.com.cn"));
        assert!(second.contains("referer: https://qoder.com.cn/account/usage"));
    }

    #[test]
    fn a_forbidden_response_on_qoder_com_retries_qoder_com_cn() {
        let home = gecko_home();
        let (international, _first_requests, first_server) = server(403, "{}");
        let (china, second_requests, second_server) = server(
            200,
            include_str!("../../../tests/fixtures/qoder/usage.json"),
        );

        fetch(&provider(&home, &international, &china)).expect("falls back");
        let second = second_requests
            .recv()
            .expect("second request captured")
            .to_ascii_lowercase();
        first_server.join().expect("first server exits");
        second_server.join().expect("second server exits");

        assert!(second.contains("cookie: cn-session=fallback-session"));
    }

    #[test]
    fn a_china_only_selected_jar_reaches_the_china_usage_endpoint() {
        let home = gecko_home_with_only_a_china_cookie();
        let (china, requests, server) = server(
            200,
            include_str!("../../../tests/fixtures/qoder/usage.json"),
        );

        let snapshot =
            fetch(&provider(&home, "http://127.0.0.1:9", &china)).expect("uses the China session");
        let request = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "qoder");
        assert!(request.contains("cookie: cn-session=china-only-session"));
        assert!(request.contains("origin: https://qoder.com.cn"));
    }

    #[test]
    fn a_china_only_browser_session_is_a_ready_auth_source() {
        let home = gecko_home_with_only_a_china_cookie();
        let (china, _requests, server) = server(
            200,
            include_str!("../../../tests/fixtures/qoder/usage.json"),
        );
        let provider = provider(&home, "http://127.0.0.1:9", &china);

        let sources = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.inspect_auth_sources())
            .expect("inspects sources");
        server.join().expect("server exits");

        assert_eq!(sources[0].children[0].id, "firefox/Default");
        assert_eq!(
            sources[0].children[0].state,
            AuthCandidateState::Ready.as_wire()
        );
    }

    #[test]
    fn the_usage_request_forwards_the_selected_browsers_whole_cookie_jar() {
        let home = gecko_home();
        let (international, requests, server) = server(
            200,
            include_str!("../../../tests/fixtures/qoder/usage.json"),
        );
        let snapshot =
            fetch(&provider(&home, &international, &international)).expect("fetches usage");
        let request = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "qoder");
        assert!(request.contains("cookie: sid=selected-session; locale=en"));
        assert!(request.contains("accept: application/json, text/plain, */*"));
        assert!(request.contains("x-requested-with: xmlhttprequest"));
        assert!(request.contains("bx-v: 2.5.35"));
    }
}
