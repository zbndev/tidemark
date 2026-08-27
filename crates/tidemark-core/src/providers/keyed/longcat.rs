//! LongCat's console token quota and fuel packs, read from the browser session that signs
//! in to longcat.chat.
//!
//! Every endpoint answers a Meituan envelope, and a `code` of 401 or 403 inside an HTTP 200
//! means the chosen browser signed out. The quota prefers the pack lot currently being
//! drained (`currentLot`) and falls back to the legacy whole-account usage when that lot is
//! missing, expired, or empty; the fuel-pack call is supplementary, so its failure never
//! hides the quota. The whole jar travels with each request — no single cookie name carries
//! the session.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use serde_json::{Map, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey,
};
use time::{
    OffsetDateTime, PrimitiveDateTime, format_description, format_description::well_known::Rfc3339,
};

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "longcat";

const USER_CURRENT_URL: &str = "https://longcat.chat/api/v1/user-current";
const TOKEN_PACKS_URL: &str = "https://longcat.chat/api/pay/quota/metering/token-packs/summary";
const TOKEN_USAGE_URL: &str = "https://longcat.chat/api/lc-platform/v1/tokenUsage";
const PENDING_FUEL_URL: &str = "https://longcat.chat/api/lc-platform/v1/pending-fuel-packages";
const SESSION_URL: &str = "https://longcat.chat/";
const COOKIE_DOMAINS: &[&str] = &["longcat.chat", "www.longcat.chat"];

/// LongCat as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "LongCat",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in longcat.chat browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(LongCat::new(options)?))
}

/// One LongCat account, authenticated by one explicitly chosen browser profile.
pub struct LongCat {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl LongCat {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
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
            client: http::client()?,
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
            let path = url.trim_start_matches("https://longcat.chat");
            return format!("{base_url}{path}");
        }
        url.to_owned()
    }

    fn get(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.request(self.client.get(url), cookie)
    }

    fn post(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.request(
            self.client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body("{}"),
            cookie,
        )
    }

    fn request(
        &self,
        builder: reqwest::RequestBuilder,
        cookie: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        builder
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ORIGIN, "https://longcat.chat")
            .header(
                reqwest::header::REFERER,
                "https://longcat.chat/platform/usage",
            )
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
            SESSION_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;

        let account = data_object(
            &super::request(
                PROVIDER_ID,
                &self.client,
                self.get(&self.url(USER_CURRENT_URL), &session.header)?,
            )
            .await?,
            "user current",
        )?;
        // The pack summary is best-effort: whatever fails here only costs the lot and
        // sends the quota back to the legacy whole-account usage.
        let packs = super::request(
            PROVIDER_ID,
            &self.client,
            self.post(&self.url(TOKEN_PACKS_URL), &session.header)?,
        )
        .await
        .ok()
        .and_then(|body| data_object(&body, "token packs").ok());
        let usage = if active_lot(packs.as_ref()).is_none() {
            let data = data_object(
                &super::request(
                    PROVIDER_ID,
                    &self.client,
                    self.get(&self.url(TOKEN_USAGE_URL), &session.header)?,
                )
                .await?,
                "token usage",
            )?;
            // `data.usage` is the canonical aggregate; the payload itself is accepted
            // only when it carries the aggregate directly.
            let canonical = data
                .get("usage")
                .and_then(Value::as_object)
                .unwrap_or(&data);
            if canonical.get("totalToken").and_then(number).is_none() {
                return Err(ProviderError::malformed(
                    "LongCat token usage is missing totalToken",
                ));
            }
            Some(canonical.clone())
        } else {
            None
        };
        let fuel = super::request(
            PROVIDER_ID,
            &self.client,
            self.get(&self.url(PENDING_FUEL_URL), &session.header)?,
        )
        .await
        .ok()
        .and_then(|body| data_object(&body, "pending fuel").ok());

        parse(
            Some(&account),
            packs.as_ref(),
            usage.as_ref(),
            fuel.as_ref(),
            Timestamp::now(),
        )
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.get(USER_CURRENT_URL, header) else {
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
            USER_CURRENT_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for LongCat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LongCat")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for LongCat {
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

/// Unwraps the Meituan envelope every LongCat endpoint answers in: success is a `code` of
/// 0 (200 on some surfaces), and 401 or 403 inside an HTTP 200 means the session was
/// rejected. The payload rides under `data`, which some surfaces omit.
fn envelope(body: &str, endpoint: &str) -> Result<Value, ProviderError> {
    let root: Value = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a LongCat {endpoint} body: {error}"))
    })?;
    let object = root.as_object().ok_or_else(|| {
        ProviderError::malformed(format!("LongCat {endpoint} root is not an object"))
    })?;
    if let Some(code) = object
        .get("code")
        .and_then(Value::as_i64)
        .filter(|code| *code != 0 && *code != 200)
    {
        if code == 401 || code == 403 {
            return Err(ProviderError::Credential {
                status: code as u16,
            });
        }
        let message = object
            .get("message")
            .or_else(|| object.get("msg"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("code {code}"));
        return Err(ProviderError::malformed(format!(
            "LongCat {endpoint} answered: {message}"
        )));
    }
    Ok(object
        .get("data")
        .cloned()
        .unwrap_or_else(|| Value::Object(object.clone())))
}

fn data_object(body: &str, endpoint: &str) -> Result<Map<String, Value>, ProviderError> {
    envelope(body, endpoint)?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            ProviderError::malformed(format!("LongCat {endpoint} data was not an object"))
        })
}

/// The pack lot currently being drained: ACTIVE status with a positive size. A missing,
/// null, expired, or empty lot falls back to the legacy whole-account quota.
fn active_lot(summary: Option<&Map<String, Value>>) -> Option<&Map<String, Value>> {
    let lot = summary?.get("currentLot")?.as_object()?;
    if lot.get("status")?.as_str()?.to_uppercase() != "ACTIVE" {
        return None;
    }
    number(lot.get("totalToken")?).filter(|total| *total > 0.0)?;
    Some(lot)
}

/// A number the console sends as a JSON number or a numeric string.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

/// The account's display name, preferring `name` and falling back to `nickName`.
fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A fuel package's expiry: epoch milliseconds or seconds, or a stamped date.
fn datetime(value: &Value) -> Option<Timestamp> {
    if let Some(number) = number(value).filter(|number| *number > 1_000_000_000.0) {
        let seconds = if number > 1_000_000_000_000.0 {
            number / 1000.0
        } else {
            number
        };
        return Timestamp::from_unix(seconds as i64).ok();
    }
    let text = value.as_str()?.trim();
    if let Ok(parsed) = OffsetDateTime::parse(text, &Rfc3339) {
        return Timestamp::from_unix(parsed.unix_timestamp()).ok();
    }
    naive_datetime(text)
}

/// "2026-05-04 23:59:59", taken as UTC — the console stamps expiries without an offset.
fn naive_datetime(value: &str) -> Option<Timestamp> {
    let format =
        format_description::parse_borrowed::<2>("[year]-[month]-[day] [hour]:[minute]:[second]")
            .ok()?;
    let parsed = PrimitiveDateTime::parse(value, &format).ok()?;
    Timestamp::from_unix(parsed.assume_utc().unix_timestamp()).ok()
}

/// Turns the four console payloads into the token-quota and fuel-pack windows.
///
/// The quota prefers the active lot's `totalToken`/`consumedToken` and falls back to the
/// legacy usage's `totalToken` with `usedToken` (or `totalToken` − `availableToken`).
pub fn parse(
    account: Option<&Map<String, Value>>,
    token_pack_summary: Option<&Map<String, Value>>,
    token_usage: Option<&Map<String, Value>>,
    pending_fuel: Option<&Map<String, Value>>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let quota = if let Some(lot) = active_lot(token_pack_summary) {
        lot.get("totalToken")
            .and_then(number)
            .filter(|total| *total > 0.0)
            .map(|total| {
                let used = lot.get("consumedToken").and_then(number).unwrap_or(0.0);
                (total, used)
            })
    } else {
        token_usage.and_then(|usage| {
            usage
                .get("totalToken")
                .and_then(number)
                .filter(|total| *total > 0.0)
                .map(|total| {
                    let used = match usage.get("usedToken").and_then(number) {
                        Some(used) => used.max(0.0),
                        None => usage
                            .get("availableToken")
                            .and_then(number)
                            .map(|remaining| (total - remaining).max(0.0))
                            .unwrap_or(0.0),
                    };
                    (total, used)
                })
        })
    };
    let mut windows = Vec::new();
    if let Some((total, used)) = quota {
        windows.push(Window {
            key: WindowKey::named("tokens"),
            title: "Tokens".to_owned(),
            subtitle: Some(format!(
                "{} / {} tokens",
                number_text(used),
                number_text(total)
            )),
            used_percent: if total > 0.0 {
                (used / total * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            },
            resets_at: None,
            length: None,
        });
    }

    if let Some(fuel) = pending_fuel
        && let Some(total) = fuel
            .get("totalQuota")
            .and_then(number)
            .filter(|total| *total > 0.0)
    {
        let mut remaining = 0.0;
        let mut saw_remaining = false;
        let mut nearest_expiry = None;
        for package in fuel
            .get("list")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if let Some(value) = package.get("availableToken").and_then(number) {
                remaining += value;
                saw_remaining = true;
            }
            if let Some(expiry) = package.get("expireTime").and_then(datetime) {
                nearest_expiry = Some(match nearest_expiry {
                    Some(current) if current <= expiry => current,
                    _ => expiry,
                });
            }
        }
        let remaining = if saw_remaining { remaining } else { total };
        let used = (total - remaining).max(0.0);
        windows.push(Window {
            key: WindowKey::named("fuel"),
            title: "Fuel pack".to_owned(),
            subtitle: Some(format!(
                "{} / {} tokens",
                number_text(used),
                number_text(total)
            )),
            used_percent: (used / total * 100.0).clamp(0.0, 100.0),
            resets_at: nearest_expiry,
            length: None,
        });
    }

    let mut details = Vec::new();
    if let Some(name) = account.and_then(|account| {
        account
            .get("name")
            .and_then(text)
            .or_else(|| account.get("nickName").and_then(text))
    }) {
        details.push(DetailSection {
            title: "Account".to_owned(),
            rows: vec![DetailRow {
                label: "Name".to_owned(),
                value: name,
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
    use super::{LongCat, parse};
    use crate::browser::SafeStorage;
    use crate::providers::{Provider, ProviderError};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use serde_json::Value;
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
                ) VALUES ('.longcat.chat', 'session', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the given routes in order, asserting each request
    /// opens with its expected request line — which pins the conditional skip of the
    /// legacy usage call. Pass only routes that will actually be hit.
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
            for (expected, status, body) in routes {
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
                    request.starts_with(expected),
                    "expected {expected}, got: {request}"
                );
                request_tx.send(request).expect("sends request");
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> LongCat {
        LongCat::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &LongCat) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    fn object(body: &str) -> serde_json::Map<String, Value> {
        serde_json::from_str::<Value>(body)
            .expect("parses")
            .as_object()
            .expect("object")
            .clone()
    }

    const USER_CURRENT: &str = include_str!("../../../tests/fixtures/longcat/user-current.json");
    const TOKEN_PACKS: &str = include_str!("../../../tests/fixtures/longcat/token-packs.json");
    const TOKEN_USAGE: &str = include_str!("../../../tests/fixtures/longcat/token-usage.json");
    const FUEL: &str = r#"{"code":0,"message":"","data":{"totalQuota":1000,"list":[{"availableToken":600,"expireTime":1750000000000},{"availableToken":150,"expireTime":1760000000000}]}}"#;

    #[test]
    fn an_active_lot_drives_the_quota_and_skips_the_legacy_usage_call() {
        // Asking for the legacy usage anyway would double-count a lot already being drained.
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[
            ("GET /api/v1/user-current", 200, USER_CURRENT),
            (
                "POST /api/pay/quota/metering/token-packs/summary",
                200,
                TOKEN_PACKS,
            ),
            ("GET /api/lc-platform/v1/pending-fuel-packages", 200, FUEL),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the quota");
        let requests: Vec<String> = (0..3)
            .map(|_| {
                requests
                    .recv()
                    .expect("request captured")
                    .to_ascii_lowercase()
            })
            .collect();
        server.join().expect("server exits");

        let tokens = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("tokens"))
            .expect("tokens window");
        assert!((tokens.used_percent - 2.425_152).abs() < 0.001);
        assert_eq!(
            tokens.subtitle.as_deref(),
            Some("1,212,576 / 50,000,000 tokens")
        );
        let fuel = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("fuel"))
            .expect("fuel window");
        assert!((fuel.used_percent - 25.0).abs() < 0.001);
        assert_eq!(
            fuel.resets_at,
            Some(Timestamp::from_unix(1_750_000_000).expect("plausible"))
        );
        assert_eq!(snapshot.details[0].rows[0].value, "LongCat User");

        let summary = &requests[1];
        assert!(summary.starts_with("post /api/pay/quota/metering/token-packs/summary"));
        assert!(
            summary.contains("content-type: application/json"),
            "{summary}"
        );
        assert!(summary.ends_with("{}"));
        for request in &requests {
            assert!(
                request.contains("cookie: session=chosen-session"),
                "{request}"
            );
            assert!(
                request.contains("origin: https://longcat.chat"),
                "{request}"
            );
            assert!(
                request.contains("referer: https://longcat.chat/platform/usage"),
                "{request}"
            );
        }
    }

    #[test]
    fn a_lot_that_is_not_being_drained_falls_back_to_the_legacy_usage() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            ("GET /api/v1/user-current", 200, USER_CURRENT),
            (
                "POST /api/pay/quota/metering/token-packs/summary",
                200,
                r#"{"code":0,"message":"","data":{"currentLot":null}}"#,
            ),
            ("GET /api/lc-platform/v1/tokenUsage", 200, TOKEN_USAGE),
            (
                "GET /api/lc-platform/v1/pending-fuel-packages",
                200,
                r#"{"code":0,"message":"","data":{"totalQuota":0,"list":[]}}"#,
            ),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the quota");
        server.join().expect("server exits");

        let tokens = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("tokens"))
            .expect("tokens window");
        assert!((tokens.used_percent - 24.0).abs() < 0.001);
        assert_eq!(tokens.subtitle.as_deref(), Some("120,000 / 500,000 tokens"));
        assert!(
            !snapshot
                .windows
                .iter()
                .any(|window| window.key == WindowKey::named("fuel"))
        );
    }

    #[test]
    fn an_envelope_refusal_inside_a_200_names_the_expired_session() {
        // Reporting this as a broken provider would hide that the chosen browser signed out.
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[(
            "GET /api/v1/user-current",
            200,
            r#"{"code":401,"message":"unauthorized"}"#,
        )]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn an_http_level_rejection_also_names_the_expired_session() {
        let home = gecko_home();
        let (base_url, _requests, server) =
            chained_server(&[("GET /api/v1/user-current", 403, "")]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 403 })
        ));
    }

    #[test]
    fn a_legacy_usage_body_without_total_token_is_malformed() {
        // Accepting it would draw a quota whose size the console never stated.
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            ("GET /api/v1/user-current", 200, USER_CURRENT),
            (
                "POST /api/pay/quota/metering/token-packs/summary",
                200,
                r#"{"code":0,"message":"","data":{"currentLot":null}}"#,
            ),
            (
                "GET /api/lc-platform/v1/tokenUsage",
                200,
                r#"{"code":0,"message":"","data":{"usage":{"usedToken":120000}}}"#,
            ),
        ]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn an_expired_lot_falls_back_and_the_nickname_names_the_account() {
        let expired = object(
            r#"{"currentLot":{"totalToken":50000000,"consumedToken":1,"status":"EXPIRED"}}"#,
        );
        let usage = object(r#"{"totalToken":500000,"usedToken":120000}"#);
        let account = object(r#"{"nickName":"Leo"}"#);

        let snapshot = parse(
            Some(&account),
            Some(&expired),
            Some(&usage),
            None,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses");

        let tokens = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("tokens"))
            .expect("tokens window");
        assert!((tokens.used_percent - 24.0).abs() < 0.001);
        assert_eq!(snapshot.details[0].rows[0].value, "Leo");
    }

    #[test]
    fn a_failing_fuel_call_costs_only_its_own_window() {
        let usage = object(r#"{"totalToken":500000,"usedToken":120000}"#);

        let snapshot = parse(
            None,
            None,
            Some(&usage),
            None,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key, WindowKey::named("tokens"));
    }
}
