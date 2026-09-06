//! Abacus compute points, read from the browser session that signs in to its dashboard.
//!
//! Every Abacus endpoint answers the same envelope — `{"success": true, "result": {…}}` —
//! and an error envelope that names the session means the chosen browser signed out. The
//! billing call is supplementary: when it fails, the points window is still drawn, without
//! its reset and plan. A browser session is selected explicitly, never substituted from
//! another profile.

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
pub const PROVIDER_ID: &str = "abacus";

const COMPUTE_POINTS_URL: &str = "https://apps.abacus.ai/api/_getOrganizationComputePoints";
const BILLING_INFO_URL: &str = "https://apps.abacus.ai/api/_getBillingInfo";
const SESSION_URL: &str = "https://apps.abacus.ai/";
/// The cookie names Abacus has carried its session in, matched exactly: a substring rule
/// here would accept cookies like `csrftoken` that merely contain a session word.
const SESSION_COOKIE_NAMES: &[&str] = &[
    "sessionid",
    "session_id",
    "session_token",
    "auth_token",
    "access_token",
];
const COOKIE_DOMAINS: &[&str] = &["abacus.ai", "apps.abacus.ai"];
/// Words in an error envelope that mean the session was rejected, so a body-level
/// refusal is reported as a credential problem rather than a broken provider.
const SESSION_REJECTS: &[&str] = &[
    "expired",
    "session",
    "login",
    "authenticate",
    "unauthorized",
    "unauthenticated",
    "forbidden",
];

/// Abacus as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Abacus",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in abacus.ai browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Abacus::new_for_account(
        account,
        &credential,
        options,
    )?))
}

/// One Abacus account, authenticated by one explicitly chosen browser profile.
pub struct Abacus {
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

impl Abacus {
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

    fn compute_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/api/_getOrganizationComputePoints");
        }
        COMPUTE_POINTS_URL.to_owned()
    }

    fn billing_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/api/_getBillingInfo");
        }
        BILLING_INFO_URL.to_owned()
    }

    fn compute_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::COOKIE, cookie)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    fn billing_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::COOKIE, cookie)
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
        let (points, billing) = tokio::join!(
            super::request(
                PROVIDER_ID,
                &self.client,
                self.compute_request(&self.compute_url(), &session.header)?,
            ),
            super::request(
                PROVIDER_ID,
                &self.client,
                self.billing_request(&self.billing_url(), &session.header)?,
            ),
        );
        parse_for_account(
            &points?,
            billing.ok().as_deref(),
            Timestamp::now(),
            &self.tidemark_account,
        )
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.compute_request(&self.compute_url(), header) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        // Abacus refuses a session inside a 200 envelope, so the proof must read the body:
        // a status-only check would call an expired login ready.
        match super::validate_body(&self.client, request).await {
            Err(ProviderError::Credential { status: 401 | 403 }) => {
                crate::browser::auth::Validation::Rejected
            }
            Err(_) => crate::browser::auth::Validation::Unreachable,
            Ok(body) => match envelope(&body, "compute points") {
                Ok(_) => crate::browser::auth::Validation::Ready,
                Err(ProviderError::Credential { status: 401 | 403 }) => {
                    crate::browser::auth::Validation::Rejected
                }
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
            COMPUTE_POINTS_URL,
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

impl fmt::Debug for Abacus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Abacus")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Abacus {
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

/// Unwraps the `{success, result}` envelope every Abacus endpoint answers in, mapping a
/// session-naming error to a credential refusal and everything else to a malformed body.
fn envelope(body: &str, endpoint: &str) -> Result<serde_json::Map<String, Value>, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not Abacus {endpoint}: {error}")))?;
    let object = root.as_object().ok_or_else(|| {
        ProviderError::malformed(format!("Abacus {endpoint} root is not an object"))
    })?;
    if object.get("success").and_then(Value::as_bool) == Some(true)
        && let Some(result) = object.get("result").and_then(Value::as_object)
    {
        return Ok(result.clone());
    }
    let message = object
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_lowercase();
    if SESSION_REJECTS.iter().any(|word| message.contains(word)) {
        return Err(ProviderError::Credential { status: 401 });
    }
    Err(ProviderError::malformed(format!(
        "Abacus {endpoint} answered an error: {message}"
    )))
}

/// Turns Abacus's compute-points and billing responses into the points balance window.
///
/// The billing body is supplementary on purpose: whatever is wrong with it — transport,
/// status, or shape — costs only the reset and the plan row, never the points reading.
pub fn parse(
    compute_points: &str,
    billing_info: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    parse_for_account(
        compute_points,
        billing_info,
        captured_at,
        &AccountId::default(),
    )
}

fn parse_for_account(
    compute_points: &str,
    billing_info: Option<&str>,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let points = envelope(compute_points, "compute points")?;
    let total = number(&points, "totalComputePoints")?;
    let left = number(&points, "computePointsLeft")?;
    let used = total - left;

    let billing = billing_info.and_then(|body| envelope(body, "billing info").ok());
    let resets_at = billing
        .as_ref()
        .and_then(|billing| billing.get("nextBillingDate"))
        .and_then(Value::as_str)
        .and_then(billing_date);
    let tier = billing
        .as_ref()
        .and_then(|billing| billing.get("currentTier"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tier| !tier.is_empty())
        .map(str::to_owned);

    let windows = vec![Window {
        key: WindowKey::named("points"),
        title: "Compute points".to_owned(),
        subtitle: Some(format!(
            "{} / {} points",
            number_text(used),
            number_text(total)
        )),
        used_percent: if total > 0.0 {
            (used / total * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        },
        resets_at,
        length: None,
    }];

    let mut details = Vec::new();
    if let Some(tier) = tier {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: tier,
            }],
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details,
    })
}

fn number(object: &serde_json::Map<String, Value>, field: &str) -> Result<f64, ProviderError> {
    object
        .get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| ProviderError::malformed(format!("Abacus {field} is not a number")))
}

fn billing_date(value: &str) -> Option<Timestamp> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    Timestamp::from_unix(parsed.unix_timestamp()).ok()
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
    use super::{Abacus, parse};
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
                ) VALUES ('.abacus.ai', 'sessionid', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the two Abacus routes, one connection each. The
    /// requests are sent concurrently, so their order in the channel is not asserted on.
    fn two_route_server(
        compute: (u16, &'static str),
        billing: (u16, &'static str),
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
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
                let is_compute = request.starts_with("GET /api/_getOrganizationComputePoints");
                request_tx.send(request).expect("sends request");
                let (status, body) = if is_compute { compute } else { billing };
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("writes response");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Abacus {
        Abacus::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Abacus) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const POINTS: &str = include_str!("../../../tests/fixtures/abacus/compute-points.json");
    const BILLING: &str = include_str!("../../../tests/fixtures/abacus/billing-info.json");

    /// A loopback server answering one request, for the inspection proofs.
    fn one_route_server(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request accepted");
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("reads request line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("writes response");
        });
        (format!("http://{address}"), server)
    }

    fn proof(
        home: &crate::browser::tests::TestHome,
        base_url: &str,
    ) -> crate::browser::auth::Validation {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(provider(home, base_url).validate_header("sessionid=chosen-session"))
    }

    #[test]
    fn an_inspection_proof_accepts_a_jar_the_envelope_confirms() {
        let (base_url, server) = one_route_server(200, POINTS);

        assert!(matches!(
            proof(&gecko_home(), &base_url),
            crate::browser::auth::Validation::Ready
        ));
        server.join().expect("server exits");
    }

    #[test]
    fn an_inspection_proof_rejects_a_session_refused_inside_a_200() {
        // A status-only proof would store an expired session as ready.
        let (base_url, server) =
            one_route_server(200, r#"{"success":false,"error":"session expired"}"#);

        assert!(matches!(
            proof(&gecko_home(), &base_url),
            crate::browser::auth::Validation::Rejected
        ));
        server.join().expect("server exits");
    }

    #[test]
    fn an_inspection_proof_is_unreachable_on_an_unreadable_envelope() {
        // An answer that is neither quota nor a refusal says nothing about the session.
        let (base_url, server) = one_route_server(200, r#"{"success":false,"error":"boom"}"#);

        assert!(matches!(
            proof(&gecko_home(), &base_url),
            crate::browser::auth::Validation::Unreachable
        ));
        server.join().expect("server exits");
    }

    #[test]
    fn a_recorded_compute_points_response_draws_the_points_window_with_its_billing() {
        // Treating points-left as used would invert the quota bar.
        let snapshot = parse(
            POINTS,
            Some(BILLING),
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses the recorded bodies");
        let points = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("points"))
            .expect("points window");

        assert!((points.used_percent - 25.0).abs() < 0.01);
        assert_eq!(
            points.resets_at,
            Some(Timestamp::from_unix(1_700_000_000).expect("plausible"))
        );
        assert_eq!(points.subtitle.as_deref(), Some("250 / 1,000 points"));
        assert_eq!(snapshot.details[0].rows[0].value, "Pro");
    }

    #[test]
    fn billing_info_is_supplementary_and_costs_only_its_reset_and_plan() {
        // Failing the whole fetch over the optional call would hide a healthy points reading.
        let snapshot = parse(
            POINTS,
            None,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");
        let points = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("points"))
            .expect("points window");

        assert_eq!(points.resets_at, None);
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn both_requests_carry_the_chosen_browsers_session_cookie() {
        // Either request without the session cookie answers an error envelope instead of quota.
        let home = gecko_home();
        let (base_url, requests, server) = two_route_server((200, POINTS), (200, BILLING));
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the points");
        let requests: Vec<String> = (0..2)
            .map(|_| {
                requests
                    .recv()
                    .expect("request captured")
                    .to_ascii_lowercase()
            })
            .collect();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "abacus");
        let compute = requests
            .iter()
            .find(|request| request.starts_with("get /api/_getorganizationcomputepoints"))
            .expect("compute points request");
        let billing = requests
            .iter()
            .find(|request| request.starts_with("post /api/_getbillinginfo"))
            .expect("billing info request");
        for request in [compute.as_str(), billing.as_str()] {
            assert!(
                request.contains("cookie: sessionid=chosen-session"),
                "{request}"
            );
            assert!(request.contains("accept: application/json"), "{request}");
        }
        assert!(billing.contains("content-type: application/json"));
        assert!(billing.ends_with("{}"));
    }

    #[test]
    fn an_unauthorised_compute_points_response_asks_for_a_new_browser_session() {
        // Mapping 401 to a transient error would retry forever instead of showing the selected session expired.
        let home = gecko_home();
        let (base_url, _requests, server) = two_route_server((401, "{}"), (401, "{}"));
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn an_error_envelope_that_names_the_session_is_a_credential_refusal() {
        // Reporting this as a broken provider would hide that the chosen browser signed out.
        let home = gecko_home();
        let (base_url, _requests, server) = two_route_server(
            (200, r#"{"success":false,"error":"session expired"}"#),
            (200, BILLING),
        );
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn a_compute_points_result_without_the_credit_fields_is_malformed() {
        // Accepting an empty result would paint a made-up zero quota.
        let home = gecko_home();
        let (base_url, _requests, server) =
            two_route_server((200, r#"{"success":true,"result":{}}"#), (200, BILLING));
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_failing_billing_request_costs_only_its_detail_rows() {
        let home = gecko_home();
        let (base_url, _requests, server) = two_route_server((200, POINTS), (500, "boom"));
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the points");
        server.join().expect("server exits");

        let points = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("points"))
            .expect("points window");
        assert!((points.used_percent - 25.0).abs() < 0.01);
        assert_eq!(points.resets_at, None);
        assert!(snapshot.details.is_empty());
    }
}
