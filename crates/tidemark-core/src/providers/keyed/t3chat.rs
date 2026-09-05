//! T3 Chat's plan meters, read from the tRPC endpoint the signed-in site itself uses.
//!
//! The site publishes no API; the card reads `getCustomerData` — the call its own
//! settings page makes — with the browser session's cookies. The answer is superjson
//! JSONL, and the customer object is found by its keys rather than its position, because
//! the lines around it are tRPC bookkeeping that has changed shape before. Two meters
//! matter: the four-hour base allowance, and the month's overage, whose reset is the
//! subscription period's end and nothing else — the billing reset timestamp tracks the
//! usage window. Vercel fronts the site with a challenge that gates on the client's TLS
//! and HTTP/2 fingerprints rather than on cookies or headers — this program's honest
//! rustls client is challenged no matter what it says, and a real browser is let through
//! with no session at all — so this one provider rides an emulating stack (`wreq`,
//! BoringSSL under a Chrome or Firefox fingerprint, whichever family the chosen session's
//! browser is) carrying the session's own cookies. The impersonation is transport-level
//! only; no web engine runs inside this process. When the edge challenges even that, it
//! is reported as a sentence.

use super::{HandSpec, Options, ProviderError, http, session};
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
use wreq_util::Emulation;

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

fn build(
    account: AccountId,
    _credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(T3Chat::new_for_account(account, options)?))
}

/// One T3 Chat account, authenticated by one explicitly chosen browser profile. The gate
/// is the whole jar: the site's session cookie has no name worth pinning.
pub struct T3Chat {
    tidemark_account: AccountId,
    client: wreq::Client,
    /// The root the browser scan is taken under, and a test fixture in every build that
    /// states one: production leaves it unset so that each platform's own browser layout
    /// decides where profiles live. A browser home is not a vendor home — Windows keeps
    /// browser profiles under `%LOCALAPPDATA%`/`%APPDATA%`, never under the user's own
    /// profile directory — so rooting the scan at one would find no browser there at all.
    browser_home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl T3Chat {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), options)
    }

    fn new_for_account(account_id: AccountId, options: &Options) -> Result<Self, ProviderError> {
        let selection = session::selection(options);
        let browser = selection
            .as_ref()
            .map_or("chrome", |selection| selection.browser.as_str());
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: Self::browser_client(browser)?,
            browser_home: None,
            storage: Arc::new(Keyring),
            selection,
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
            client: Self::browser_client("firefox")?,
            browser_home: Some(home.to_path_buf()),
            storage,
            selection: Some(Selection {
                browser: "firefox".into(),
                profile: None,
            }),
            base_url: Some(base_url.trim_end_matches('/').to_owned()),
        })
    }

    /// The browser-shaped client this provider's edge demands: the same proxy policy and
    /// timeouts as [`http::client`], but wearing a browser's TLS and HTTP/2 fingerprint
    /// instead of this program's name — see the module docs.
    fn browser_client(browser: &str) -> Result<wreq::Client, ProviderError> {
        let mut builder = wreq::Client::builder()
            .emulation(emulation_for(browser))
            .timeout(http::REQUEST_TIMEOUT)
            .connect_timeout(http::CONNECT_TIMEOUT);
        if let Some(proxy) = http::proxy() {
            let proxy = wreq::Proxy::all(proxy.url())
                .map(|proxy| proxy.no_proxy(wreq::NoProxy::from_string(http::NO_PROXY)))
                .map_err(|error| {
                    ProviderError::Emulated(format!(
                        "could not build the browser-emulating client: {error}"
                    ))
                })?;
            builder = builder.proxy(proxy);
        }
        builder.build().map_err(|error| {
            ProviderError::Emulated(format!(
                "could not build the browser-emulating client: {error}"
            ))
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

    fn data_request(&self, url: &str, cookie: &str) -> Result<wreq::Request, ProviderError> {
        self.client
            .get(url)
            .query(&[("batch", "1"), ("input", INPUT)])
            .header("trpc-accept", "application/jsonl")
            .header("x-trpc-source", "web-client")
            .header("x-trpc-batch", "true")
            .header("referer", REFERER)
            .header("origin", ORIGIN)
            .header("cookie", cookie)
            .build()
            .map_err(|error| {
                ProviderError::Emulated(format!("could not build the T3 Chat request: {error}"))
            })
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let selection = self.selection.as_ref().ok_or(ProviderError::NoCredential)?;
        let session = session::session(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            selection,
            &[],
            &cookie_query(),
            API_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let request = self.data_request(&self.url(API_URL), &session.header)?;
        let body = self.send_inspected(request).await?;
        parse_for_account(&body, Timestamp::now(), &self.tidemark_account)
    }

    /// [`super::request_inspected`] on the emulating stack: the same `Retry-After` read,
    /// the same challenge-before-status rule, the same `http::check`, the same
    /// raw-response log and empty-body refusal — against `wreq` types, because the
    /// request must wear a browser's fingerprint and `request_inspected` speaks
    /// `reqwest`.
    async fn send_inspected(&self, request: wreq::Request) -> Result<String, ProviderError> {
        let url = request.url().as_str().to_owned();
        let sent = crate::debug::enabled().then(|| crate::debug::Sent::get(&url));
        let note = |answer| {
            if let Some(sent) = &sent {
                crate::debug::record(crate::debug::Exchange {
                    provider: PROVIDER_ID,
                    sent: *sent,
                    answer,
                });
            }
        };

        let response = match self.client.execute(request).await {
            Ok(response) => response,
            Err(error) => {
                let error = ProviderError::Emulated(format!("request failed: {error}"));
                note(crate::debug::Answer::Failed {
                    error: &error.to_string(),
                });
                return Err(error);
            }
        };

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if is_vercel_challenge(&response) {
            note(crate::debug::Answer::Refused { status });
            return Err(ProviderError::Challenged(
                "T3 Chat asked for a browser check".to_owned(),
            ));
        }
        if let Err(error) = http::check(wire_status(status), retry_after.as_deref()) {
            note(crate::debug::Answer::Refused { status });
            return Err(error);
        }

        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                let error = ProviderError::Emulated(format!("request failed: {error}"));
                note(crate::debug::Answer::Failed {
                    error: &error.to_string(),
                });
                return Err(error);
            }
        };
        // Before the emptiness check, as in `request_inspected`: "the provider answered
        // nothing" is one of the things a person reads the raw-response log to confirm.
        note(crate::debug::Answer::Body {
            status,
            body: &body,
        });
        if body.trim().is_empty() {
            return Err(ProviderError::malformed(
                "the provider answered an empty body",
            ));
        }
        Ok(body)
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        use crate::browser::auth::Validation;
        let Ok(request) = self.data_request(&self.url(API_URL), header) else {
            return Validation::Unreachable;
        };
        let Ok(response) = self.client.execute(request).await else {
            return Validation::Unreachable;
        };
        if is_vercel_challenge(&response) {
            return Validation::Challenged;
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok());
        match http::check(wire_status(response.status().as_u16()), retry_after) {
            Ok(()) => Validation::Ready,
            Err(ProviderError::Credential { status: 401 | 403 }) => Validation::Rejected,
            Err(_) => Validation::Unreachable,
        }
    }

    async fn inspect_sources(&self) -> Vec<AuthCandidate> {
        session::inspect_sources(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            &[],
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

/// Whether the response is Vercel's browser challenge, which arrives stamped as a 429.
fn is_vercel_challenge(response: &wreq::Response) -> bool {
    response.status().as_u16() == 429
        && response
            .headers()
            .get("x-vercel-mitigated")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("challenge"))
}

/// The fingerprint family the emulating client wears: the chosen session's own browser
/// family, so the handshake, the headers and the cookies all name one browser. Zen is a
/// Firefox build underneath; an unrecognised name reads as Chrome, the most common shape.
fn emulation_for(browser: &str) -> Emulation {
    match browser {
        "firefox" | "zen" => Emulation::Firefox139,
        _ => Emulation::Chrome137,
    }
}

/// `http::check` speaks `reqwest`'s status type; a status off the wire is the same u16 on
/// either stack. The fallback only satisfies the type — the wire never sends it here.
fn wire_status(status: u16) -> reqwest::StatusCode {
    reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::BAD_GATEWAY)
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
    parse_for_account(body, captured_at, &AccountId::default())
}

fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(customer) = find_customer_data(&value) else {
            continue;
        };
        return snapshot_for_account(customer, captured_at, account_id);
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

fn snapshot_for_account(
    customer: &serde_json::Value,
    captured_at: Timestamp,
    account_id: &AccountId,
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
        account: account_id.clone(),
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
    use super::{T3Chat, emulation_for, is_vercel_challenge, parse};
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
            Err(ProviderError::Challenged(sentence)) if sentence == "T3 Chat asked for a browser check"
        ));
    }

    #[test]
    fn a_vercel_challenge_during_proof_is_reported_as_challenged() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(vec![(
            "GET /api/trpc/getCustomerData?batch=1&input=",
            429,
            "x-vercel-mitigated: challenge\r\n",
            "checkpoint",
        )]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let validation =
            runtime.block_on(provider(&home, &base_url).validate_header("session=t3-value"));
        server.join().expect("server exits");

        assert_eq!(validation, crate::browser::auth::Validation::Challenged);
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
                .block_on(wreq::Client::new().get(format!("{base_url}/probe")).send())
                .expect("answers");
            server.join().expect("server exits");
            is_vercel_challenge(&response)
        }

        assert!(answered(429, "x-vercel-mitigated: challenge\r\n"));
        assert!(!answered(429, "x-vercel-mitigated: rate-limit\r\n"));
        assert!(!answered(429, ""));
        assert!(!answered(500, "x-vercel-mitigated: challenge\r\n"));
    }

    #[test]
    fn the_emulated_fingerprint_follows_the_selected_browser_family() {
        use wreq_util::Emulation;

        assert!(matches!(emulation_for("chrome"), Emulation::Chrome137));
        assert!(matches!(emulation_for("chromium"), Emulation::Chrome137));
        assert!(matches!(emulation_for("firefox"), Emulation::Firefox139));
        // Zen is a Firefox build underneath; its cookies arrive on a Firefox handshake.
        assert!(matches!(emulation_for("zen"), Emulation::Firefox139));
    }
}
