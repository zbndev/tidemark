//! T3 Chat's plan meters, read from the tRPC endpoint the signed-in site itself uses.
//!
//! The site publishes no API; the card reads `getCustomerData` — the call its own
//! settings page makes — with the browser session's cookies. The answer is superjson
//! JSONL, and the customer object is found by its keys rather than its position, because
//! the lines around it are tRPC bookkeeping that has changed shape before. Two meters
//! matter: the four-hour base allowance, and the month's overage, whose reset is the
//! subscription period's end and nothing else — the billing reset timestamp tracks the
//! usage window. Vercel fronts the site with a browser challenge; when it asks, that is
//! reported as a sentence, because this client identifies itself as Tidemark and will not
//! pretend to be a browser to get past it.

use super::{HandSpec, Options, ProviderError, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use serde::Deserialize;
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
pub const PROVIDER_ID: &str = "t3chat";

const API_URL: &str = "https://t3.chat/api/trpc/getCustomerData";
const REFERER: &str = "https://t3.chat/settings/customization";
const ORIGIN: &str = "https://t3.chat";
const COOKIE_DOMAINS: &[&str] = &["t3.chat", "www.t3.chat"];
const FOUR_HOUR: u64 = 4 * 60 * 60;
/// The tRPC batch input the settings page itself sends: one procedure call, its
/// `sessionId` argument explicitly `undefined`.
const INPUT: &str =
    r#"{"0":{"json":{"sessionId":null},"meta":{"values":{"sessionId":["undefined"]}}}}"#;

/// T3 Chat as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "T3 Chat",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in t3.chat browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(T3Chat::new(options)?))
}

/// One T3 Chat account, authenticated by one explicitly chosen browser profile. The gate
/// is the whole jar: the site's session cookie has no name worth pinning.
pub struct T3Chat {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl T3Chat {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            client: super::http::client()?,
            home: std::env::var_os("HOME").map(PathBuf::from),
            storage: Arc::new(Keyring),
            selection: session::selection(options),
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
            client: super::http::client()?,
            home: Some(home.to_path_buf()),
            storage,
            selection: Some(Selection {
                browser: "firefox".into(),
                profile: None,
            }),
            base_url: Some(base_url.trim_end_matches('/').to_owned()),
        })
    }

    /// The production URL, its host swapped for the loopback server during tests.
    fn url(&self, url: &str) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}{}", url.trim_start_matches("https://t3.chat"));
        }
        url.to_owned()
    }

    fn data_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .query(&[("batch", "1"), ("input", INPUT)])
            .header("trpc-accept", "application/jsonl")
            .header("x-trpc-source", "web-client")
            .header("x-trpc-batch", "true")
            .header(reqwest::header::REFERER, REFERER)
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::COOKIE, cookie)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let selection = self.selection.as_ref().ok_or(ProviderError::NoCredential)?;
        let session = session::session(
            self.home.as_deref(),
            self.storage.as_ref(),
            selection,
            &[],
            &cookie_query(),
            API_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let request = self.data_request(&self.url(API_URL), &session.header)?;
        let (body, _) = super::request_inspected(PROVIDER_ID, &self.client, request, |response| {
            if is_vercel_challenge(response) {
                return Err(ProviderError::Local(
                    "T3 Chat asked for a browser check".to_owned(),
                ));
            }
            Ok(())
        })
        .await?;
        parse(&body, Timestamp::now())
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.data_request(API_URL, header) else {
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
        session::inspect_sources(
            self.home.as_deref(),
            self.storage.as_ref(),
            &cookie_query(),
            API_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for T3Chat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("T3Chat")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for T3Chat {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
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

/// Whether the response is Vercel's browser challenge, which arrives stamped as a 429.
fn is_vercel_challenge(response: &reqwest::Response) -> bool {
    response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        && response
            .headers()
            .get("x-vercel-mitigated")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("challenge"))
}

/// The subscription fields the meters need.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    #[serde(default)]
    product_name: Option<String>,
    #[serde(default)]
    current_period_end: Option<f64>,
}

/// The customer object inside the JSONL, as the site defines it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomerData {
    #[serde(default)]
    sub_tier: Option<String>,
    #[serde(default)]
    subscription: Option<Subscription>,
    #[serde(default)]
    usage_band: Option<String>,
    #[serde(default)]
    usage_four_hour_percentage: Option<f64>,
    #[serde(default)]
    usage_month_percentage: Option<f64>,
    #[serde(default)]
    usage_period_percentage: Option<f64>,
    #[serde(default)]
    usage_four_hour_next_reset_at: Option<f64>,
    #[serde(default)]
    usage_window_next_reset_at: Option<f64>,
}

/// The JSONL body as a snapshot: the four-hour base window, the month's overage window,
/// and the plan row. A meter the response did not state stays away rather than reading
/// zero; a response that states none of them is not an answer.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(customer) = find_customer_data(&value) else {
            continue;
        };
        return snapshot(customer, captured_at);
    }
    Err(ProviderError::malformed(
        "the T3 Chat response named no customer data",
    ))
}

/// The object the meters live in, found by its keys wherever the batch bookkeeping put
/// it this time.
fn find_customer_data(value: &serde_json::Value) -> Option<&serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            let names = |name: &str| map.contains_key(name);
            if names("usageFourHourPercentage")
                || names("usageMonthPercentage")
                || (names("subscription") && names("usageBand"))
            {
                return Some(value);
            }
            map.values().find_map(find_customer_data)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_customer_data),
        _ => None,
    }
}

fn snapshot(
    customer: &serde_json::Value,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let data: CustomerData = serde_json::from_value(customer.clone()).map_err(|error| {
        ProviderError::malformed(format!("the T3 Chat customer data did not decode: {error}"))
    })?;

    let mut windows = Vec::new();
    if let Some(used_percent) = percent(data.usage_four_hour_percentage) {
        let length = WindowLength::from_secs(FOUR_HOUR).expect("a fixed span is not zero");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: "4-hour".to_owned(),
            subtitle: data.usage_band.as_deref().map(crate::providers::title_case),
            used_percent,
            resets_at: instant(
                data.usage_four_hour_next_reset_at
                    .or(data.usage_window_next_reset_at),
            ),
            length: Some(length),
        });
    }
    if let Some(used_percent) =
        percent(data.usage_month_percentage.or(data.usage_period_percentage))
    {
        windows.push(Window {
            // The billing period is a calendar span the subscription names only by its
            // end instant — there is no stated length to key this window on.
            key: WindowKey::named("period"),
            title: "Monthly".to_owned(),
            subtitle: None,
            used_percent,
            // The billing reset timestamp tracks the usage window, not the billing
            // period; only the subscription's own period end is this window's reset.
            resets_at: data
                .subscription
                .as_ref()
                .and_then(|subscription| instant(subscription.current_period_end)),
            length: None,
        });
    }
    if windows.is_empty() {
        return Err(ProviderError::malformed(
            "the T3 Chat response named no usage windows",
        ));
    }

    let plan = data
        .subscription
        .as_ref()
        .and_then(|subscription| subscription.product_name.as_deref())
        .or(data.sub_tier.as_deref())
        .map(crate::providers::title_case)
        .filter(|plan| !plan.is_empty());
    let mut details = Vec::new();
    if let Some(plan) = plan {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: plan,
            }],
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
}

/// A stated percentage, clamped to what a bar can draw. `None` when unstated.
fn percent(raw: Option<f64>) -> Option<f64> {
    raw.map(|percent| percent.clamp(0.0, 100.0))
}

/// A reset the API states in JavaScript epoch milliseconds — with some subscription
/// fields having been seen in seconds, which the magnitudes never overlap with.
fn instant(raw: Option<f64>) -> Option<Timestamp> {
    let raw = raw.filter(|raw| *raw > 0.0)?;
    let seconds = if raw > 10_000_000_000.0 {
        raw / 1000.0
    } else {
        raw
    };
    Timestamp::from_unix(seconds as i64).ok()
}

#[cfg(test)]
mod tests {
    use super::{T3Chat, is_vercel_challenge, parse};
    use crate::browser::SafeStorage;
    use crate::providers::{Provider, ProviderError};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Write};
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
                ) VALUES ('.t3.chat', ?1, ?2, '/', 0, 1, 0, 0, 0)",
                ("session", "t3-value"),
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the given routes in order, asserting each request
    /// opens with its expected request line. Pass only routes that will actually be hit.
    /// The headers field is written verbatim between the status line and Content-Length.
    fn chained_server(
        routes: Vec<(&'static str, u16, &'static str, &'static str)>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for (expected, status, headers, body) in routes {
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> T3Chat {
        T3Chat::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &T3Chat) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const RECORDED: &str = include_str!("../../../tests/fixtures/t3chat/customer-data.jsonl");
    const CAPTURED_AT: i64 = 1_700_000_000;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    /// One JSONL line carrying only the customer object.
    fn customer_line(json: &str) -> String {
        format!("{{\"json\":[2,0,[[{json}]]]}}\n")
    }

    #[test]
    fn the_recorded_response_draws_both_windows_and_the_plan() {
        let snapshot = parse(RECORDED, at(CAPTURED_AT)).expect("parses the response");

        let base = &snapshot.windows[0];
        assert_eq!(base.title, "4-hour");
        assert_eq!(
            base.key,
            WindowKey::for_length(WindowLength::from_secs(4 * 60 * 60).expect("non-zero"))
        );
        assert_eq!(base.subtitle.as_deref(), Some("Max"));
        assert!((base.used_percent - 12.5).abs() < 0.000_001);
        assert_eq!(base.resets_at, Some(at(1_779_366_216)));
        let monthly = &snapshot.windows[1];
        assert_eq!(monthly.title, "Monthly");
        assert_eq!(monthly.key, WindowKey::named("period"));
        assert_eq!(monthly.length, None);
        assert!((monthly.used_percent - 34.25).abs() < 0.000_001);
        assert_eq!(monthly.resets_at, Some(at(1_780_763_009)));
        assert_eq!(snapshot.details[0].title, "Plan");
        assert_eq!(snapshot.details[0].rows[0].value, "Pro");
    }

    #[test]
    fn a_free_account_reads_the_period_percentage_for_the_month() {
        let body = customer_line(
            r#"{"subTier":"free","usageFourHourPercentage":5,"usagePeriodPercentage":65}"#,
        );

        let snapshot = parse(&body, at(CAPTURED_AT)).expect("parses the response");

        assert!((snapshot.windows[0].used_percent - 5.0).abs() < 0.000_001);
        assert!((snapshot.windows[1].used_percent - 65.0).abs() < 0.000_001);
        assert_eq!(snapshot.windows[1].resets_at, None);
        assert_eq!(snapshot.details[0].rows[0].value, "Free");
    }

    #[test]
    fn the_monthly_reset_ignores_the_billing_reset() {
        // billingNextResetAt tracks the usage window; showing it as the month's reset
        // would promise the overage clears hours or weeks before the period ends.
        let body =
            customer_line(r#"{"usageMonthPercentage":20,"billingNextResetAt":1779366216920}"#);

        let snapshot = parse(&body, at(CAPTURED_AT)).expect("parses the response");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].title, "Monthly");
        assert_eq!(snapshot.windows[0].resets_at, None);
    }

    #[test]
    fn an_over_full_percentage_clamps_to_the_bar() {
        let body = customer_line(r#"{"usageFourHourPercentage":250}"#);

        let snapshot = parse(&body, at(CAPTURED_AT)).expect("parses the response");

        assert_eq!(snapshot.windows[0].used_percent, 100.0);
    }

    #[test]
    fn a_response_without_customer_data_is_malformed() {
        let body = "{\"json\":{\"0\":[[0],[null,0,0]]}}\n";

        let result = parse(body, at(CAPTURED_AT));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn an_unreadable_percentage_is_malformed() {
        let body = customer_line(r#"{"usageFourHourPercentage":"12.5"}"#);

        let result = parse(&body, at(CAPTURED_AT));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn the_data_request_carries_the_trpc_headers_and_the_whole_jar() {
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(vec![(
            "GET /api/trpc/getCustomerData?batch=1&input=%7B%220%22",
            200,
            "",
            RECORDED,
        )]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        let request = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "t3chat");
        assert_eq!(snapshot.windows.len(), 2);
        assert!(request.contains("cookie: session=t3-value"), "{request}");
        assert!(
            request.contains("trpc-accept: application/jsonl"),
            "{request}"
        );
        assert!(request.contains("x-trpc-source: web-client"), "{request}");
        assert!(request.contains("x-trpc-batch: true"), "{request}");
        assert!(
            request.contains("referer: https://t3.chat/settings/customization"),
            "{request}"
        );
        assert!(request.contains("origin: https://t3.chat\r\n"), "{request}");
    }

    #[test]
    fn a_vercel_challenge_is_a_browser_check_not_a_rate_limit() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(vec![(
            "GET /api/trpc/getCustomerData?batch=1&input=",
            429,
            "x-vercel-mitigated: challenge\r\n",
            "checkpoint",
        )]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Local(sentence)) if sentence == "T3 Chat asked for a browser check"
        ));
    }

    #[test]
    fn a_plain_rate_limit_stays_a_rate_limit() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(vec![(
            "GET /api/trpc/getCustomerData?batch=1&input=",
            429,
            "",
            "slow down",
        )]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::RateLimited { retry_after: None })
        ));
    }

    #[test]
    fn an_http_rejection_names_the_expired_session() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(vec![(
            "GET /api/trpc/getCustomerData?batch=1&input=",
            401,
            "",
            "unauthorized",
        )]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn the_challenge_is_recognised_by_status_and_header_together() {
        // A response carrying the mitigation header on any other status is not a
        // challenge, and must not be reported as one.
        fn answered(status: u16, headers: &'static str) -> bool {
            let (base_url, _requests, server) =
                chained_server(vec![("GET /probe", status, headers, "")]);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let response = runtime
                .block_on(
                    reqwest::Client::new()
                        .get(format!("{base_url}/probe"))
                        .send(),
                )
                .expect("answers");
            server.join().expect("server exits");
            is_vercel_challenge(&response)
        }

        assert!(answered(429, "x-vercel-mitigated: challenge\r\n"));
        assert!(!answered(429, "x-vercel-mitigated: rate-limit\r\n"));
        assert!(!answered(429, ""));
        assert!(!answered(500, "x-vercel-mitigated: challenge\r\n"));
    }
}
