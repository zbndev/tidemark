//! Ollama Cloud usage, read from the browser session that signs in to ollama.com.
//!
//! The site meters its cloud plans on the settings page itself: there is no JSON API
//! behind it, so the numbers are scraped out of server-rendered HTML. The parser is
//! deliberately shallow — labels, a `data-time` attribute, a percent — rather than a
//! document model, because the page is HTML and can move; when it moves, the fetch fails
//! loudly as a malformed page instead of quietly inventing numbers. An expired session
//! shows up as a bounce to a sign-in landing — recognised by where the request finally
//! lands, or by the sign-in form served on a 200 — and is reported as a rejected session,
//! not a broken provider.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey, WindowLength,
};
use time::OffsetDateTime;

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "ollama";

const SETTINGS_URL: &str = "https://ollama.com/settings";
const SESSION_URL: &str = "https://ollama.com/";
const SITE_HOST: &str = "ollama.com";
const COOKIE_DOMAINS: &[&str] = &["ollama.com", "www.ollama.com"];
/// Every session spelling the site has served: its own, Next.js's, and WorkOS's.
const SESSION_COOKIE_NAMES: &[&str] = &[
    "__Secure-session",
    "session",
    "ollama_session",
    "__Host-ollama_session",
    "wos-session",
    "__Secure-next-auth.session-token",
    "next-auth.session-token",
];
const SESSION: u64 = 5 * 60 * 60;
const HOURLY: u64 = 3_600;
const WEEKLY: u64 = 7 * 24 * 60 * 60;

/// Ollama as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Ollama",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in ollama.com browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    _credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Ollama::new_for_account(account, options)?))
}

/// One Ollama account, authenticated by one explicitly chosen browser profile.
pub struct Ollama {
    tidemark_account: AccountId,
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Ollama {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), options)
    }

    fn new_for_account(account_id: AccountId, options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: account_id.clone(),
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
            tidemark_account: AccountId::default(),
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
            return format!("{base_url}{}", url.trim_start_matches("https://ollama.com"));
        }
        url.to_owned()
    }

    /// The host the settings page is served from, which is also the host whose `/signin`
    /// path means the session was bounced — the loopback server during tests.
    fn site_host(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return reqwest::Url::parse(base_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| SITE_HOST.to_owned());
        }
        SITE_HOST.to_owned()
    }

    fn settings_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
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
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            SESSION_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let request = self.settings_request(&self.url(SETTINGS_URL), &session.header)?;
        let (body, landed_on) = super::request_with_url(PROVIDER_ID, &self.client, request).await?;
        // A 200 that is really the sign-in landing: WorkOS bounces an expired session
        // there, and the page answers like any page would.
        if is_signin_redirect(&landed_on, &self.site_host()) {
            return Err(ProviderError::Credential { status: 401 });
        }
        parse_for_account(&body, Timestamp::now(), &self.tidemark_account)
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.settings_request(SETTINGS_URL, header) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        // The proof is where the request lands, not only that it succeeds: an expired
        // session is bounced to the sign-in page, which answers 200 like a settings page.
        let Ok(response) = self.client.execute(request).await else {
            return crate::browser::auth::Validation::Unreachable;
        };
        let landed_on = response.url().clone();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        match http::check(response.status(), retry_after.as_deref()) {
            Ok(()) if is_signin_redirect(&landed_on, &self.site_host()) => {
                crate::browser::auth::Validation::Rejected
            }
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
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            SETTINGS_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for Ollama {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ollama")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Ollama {
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

/// Whether a landing URL is the sign-in page an expired session is bounced to: the site's
/// own `/signin`, WorkOS's hosted sign-in subdomain, or its authorization flow.
fn is_signin_redirect(url: &reqwest::Url, site_host: &str) -> bool {
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let path = url.path().to_ascii_lowercase();
    if host == site_host || host == format!("www.{site_host}") {
        return path == "/signin";
    }
    if host == "signin.ollama.com" {
        return true;
    }
    host.ends_with(".workos.com") && path.starts_with("/user_management/authorize")
}

/// The page's meters as a snapshot: the session (or hourly) window, the weekly window,
/// and the plan and account rows the page happens to carry.
pub fn parse(html: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_for_account(html, captured_at, &AccountId::default())
}

fn parse_for_account(
    html: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    if looks_signed_out(html) {
        // The page the site serves a signed-out visitor: the session is gone, not broken.
        return Err(ProviderError::Credential { status: 401 });
    }
    let primary = usage_block(html, "Session usage")
        .or_else(|| usage_block(html, "Hourly usage"))
        .ok_or_else(|| {
            ProviderError::malformed("the Ollama settings page rendered no usage numbers")
        })?;
    let weekly = usage_block(html, "Weekly usage");

    let mut windows = vec![primary.window()];
    if let Some(weekly) = weekly {
        windows.push(weekly.window());
    }
    let mut details = Vec::new();
    if let Some(plan) = plan_name(html) {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: plan.to_owned(),
            }],
        });
    }
    if let Some(email) = account_email(html) {
        details.push(DetailSection {
            title: "Account".to_owned(),
            rows: vec![DetailRow {
                label: "Email".to_owned(),
                value: email.to_owned(),
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

/// Every usage label the page renders, in page order; a block runs until the next one.
const USAGE_LABELS: [&str; 3] = ["Session usage", "Hourly usage", "Weekly usage"];

/// One labelled meter: a percent, maybe a reset instant, and the span it meters.
#[derive(Debug)]
struct UsageBlock {
    title: &'static str,
    length_secs: u64,
    used_percent: f64,
    resets_at: Option<Timestamp>,
}

impl UsageBlock {
    fn window(&self) -> Window {
        let length = WindowLength::from_secs(self.length_secs).expect("a fixed span is not zero");
        Window {
            key: WindowKey::for_length(length),
            title: self.title.to_owned(),
            subtitle: None,
            used_percent: self.used_percent.clamp(0.0, 100.0),
            resets_at: self.resets_at,
            length: Some(length),
        }
    }
}

/// The block after one usage label — bounded by the next usage label, or four thousand
/// characters, a span the page's own blocks never come near — with its percent required:
/// a label whose block carries no number is a page that moved, not a meter at zero.
fn usage_block(html: &str, label: &str) -> Option<UsageBlock> {
    let tail = html.split_once(label)?.1;
    let bound = USAGE_LABELS
        .iter()
        .filter(|other| **other != label)
        .filter_map(|other| tail.find(*other))
        .min()
        .unwrap_or(tail.len());
    let window = first_characters(&tail[..bound], 4000);
    let used_percent = parse_percent(window)?;
    Some(UsageBlock {
        title: match label {
            "Session usage" => "Session",
            "Hourly usage" => "Hourly",
            _ => "Weekly",
        },
        length_secs: match label {
            "Session usage" => SESSION,
            "Hourly usage" => HOURLY,
            _ => WEEKLY,
        },
        used_percent,
        resets_at: parse_data_time(window),
    })
}

/// The first `limit` characters of the page, so a block whose closing label never comes
/// still cannot swallow the rest of the page.
fn first_characters(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(index, _)| &text[..index])
}

/// The meter's percent: the first `N% used` (any capitalisation), else the first
/// `width: N%` inline style — the two spellings the page has served.
fn parse_percent(text: &str) -> Option<f64> {
    text.match_indices('%')
        .find_map(|(at, _)| percent_around(text, at))
        .or_else(|| width_percent(text))
}

/// `N% used` around one `%`, with any capitalisation of "used".
fn percent_around(text: &str, at: usize) -> Option<f64> {
    let after = text[at + 1..].trim_start();
    if !after
        .get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("used"))
    {
        return None;
    }
    number_ending(text[..at].trim_end())
}

/// The fallback meter: the first `width: N%` inline style, any capitalisation.
fn width_percent(text: &str) -> Option<f64> {
    let lower = text.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find("width:") {
        let after = lower[search + found + "width:".len()..].trim_start();
        search = lower.len() - after.len();
        let end = after
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(after.len());
        let (run, rest) = after.split_at(end);
        if rest.starts_with('%')
            && let Some(value) = number_ending(run)
        {
            return Some(value);
        }
    }
    None
}

/// The number a percent reads: digits, or digits with a fraction, and nothing else in
/// the run — a run shaped like anything else is not a number the page meant.
fn number_ending(before: &str) -> Option<f64> {
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
        .last()
        .map_or(before.len(), |(index, _)| index);
    let run = &before[start..];
    let (whole, fraction) = run.split_once('.').unwrap_or((run, ""));
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (!fraction.is_empty() && !digits(fraction)) {
        return None;
    }
    run.parse().ok()
}

/// The block's reset instant: the first `data-time` attribute, an RFC 3339 timestamp.
fn parse_data_time(text: &str) -> Option<Timestamp> {
    let value = text.split_once("data-time=\"")?.1;
    let value = &value[..value.find('"')?];
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .and_then(|parsed| Timestamp::from_unix(parsed.unix_timestamp()).ok())
}

/// The plan badge beside the "Cloud Usage" heading, when the page carries one.
fn plan_name(html: &str) -> Option<&str> {
    let after = html.split_once("Cloud Usage")?.1;
    let after = after.trim_start().strip_prefix("</span>")?.trim_start();
    let after = after.strip_prefix("<span")?;
    let inner = &after[after.find('>')? + 1..];
    let end = inner.find('<')?;
    let text = inner[..end].trim();
    (!text.is_empty()).then_some(text)
}

/// The header's account email, when the page carries one.
fn account_email(html: &str) -> Option<&str> {
    let after = html.split_once("id=\"header-email\"")?.1;
    let inner = &after[after.find('>')? + 1..];
    let end = inner.find('<')?;
    let text = inner[..end].trim();
    text.contains('@').then_some(text)
}

/// Whether the page is the one the site serves a signed-out visitor: a sign-in heading
/// over a real auth form. A passing mention of signing in is not a sign-in page.
fn looks_signed_out(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let contains_any = |markers: &[&str]| markers.iter().any(|marker| lower.contains(marker));

    let sign_in_heading = lower.contains("sign in to ollama") || lower.contains("log in to ollama");
    let auth_route = lower.contains("/api/auth/signin") || lower.contains("/auth/signin");
    let login_route = contains_any(&[
        "action=\"/login\"",
        "action='/login'",
        "href=\"/login\"",
        "href='/login'",
        "action=\"/signin\"",
        "action='/signin'",
        "href=\"/signin\"",
        "href='/signin'",
    ]);
    let password_field = contains_any(&[
        "type=\"password\"",
        "type='password'",
        "name=\"password\"",
        "name='password'",
    ]);
    let email_field = contains_any(&[
        "type=\"email\"",
        "type='email'",
        "name=\"email\"",
        "name='email'",
    ]);
    let auth_form = lower.contains("<form");
    let auth_endpoint = auth_route || login_route;

    (sign_in_heading && auth_form && (email_field || password_field || auth_endpoint))
        || (auth_form && auth_endpoint)
        || (auth_form && password_field && email_field)
}

#[cfg(test)]
mod tests {
    use super::{Ollama, is_signin_redirect, parse};
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
                ) VALUES ('.ollama.com', ?1, ?2, '/', 0, 1, 0, 0, 0)",
                ("session", "session-value"),
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the given routes in order — request line, status,
    /// extra headers, body — asserting each request opens with its expected request line.
    /// Pass only routes that will actually be hit.
    #[allow(clippy::type_complexity)]
    fn chained_server(
        routes: &'static [(
            &'static str,
            u16,
            &'static [(&'static str, &'static str)],
            &'static str,
        )],
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
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n",
                    body.len()
                )
                .expect("writes response");
                for (name, value) in headers.iter() {
                    write!(stream, "{name}: {value}\r\n").expect("writes header");
                }
                write!(stream, "Connection: close\r\n\r\n{body}").expect("writes response");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Ollama {
        Ollama::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Ollama) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const SETTINGS: &str = include_str!("../../../tests/fixtures/ollama/settings.html");
    const CAPTURED_AT: i64 = 1_700_000_000;

    fn session_key() -> WindowKey {
        WindowKey::for_length(WindowLength::from_secs(5 * 60 * 60).expect("a span is not zero"))
    }

    fn weekly_key() -> WindowKey {
        WindowKey::for_length(
            WindowLength::from_secs(7 * 24 * 60 * 60).expect("a span is not zero"),
        )
    }

    #[test]
    fn the_recorded_settings_page_draws_the_session_and_weekly_windows() {
        let snapshot = parse(
            SETTINGS,
            Timestamp::from_unix(CAPTURED_AT).expect("plausible"),
        )
        .expect("parses the recorded page");

        let session = &snapshot.windows[0];
        assert_eq!(session.title, "Session");
        assert_eq!(session.key, session_key());
        assert!((session.used_percent - 0.1).abs() < 0.000_001);
        assert_eq!(
            session.resets_at,
            Some(Timestamp::from_unix(1_769_796_000).expect("2026-01-30T18:00:00Z"))
        );
        let weekly = &snapshot.windows[1];
        assert_eq!(weekly.title, "Weekly");
        assert_eq!(weekly.key, weekly_key());
        assert!((weekly.used_percent - 0.7).abs() < 0.000_001);
        assert_eq!(
            weekly.resets_at,
            Some(Timestamp::from_unix(1_769_990_400).expect("2026-02-02T00:00:00Z"))
        );
        assert_eq!(snapshot.details[0].title, "Plan");
        assert_eq!(snapshot.details[0].rows[0].value, "free");
        assert_eq!(snapshot.details[1].title, "Account");
        assert_eq!(snapshot.details[1].rows[0].value, "user@example.com");
    }

    #[test]
    fn an_hourly_block_is_keyed_by_the_hour_it_names() {
        let html = "<div><span>Hourly usage</span><span>2.5% used</span>\
                    <div class=\"local-time\" data-time=\"2026-01-30T18:00:00Z\">Resets</div>\
                    <span>Weekly usage</span><span>4.2% used</span></div>";

        let snapshot =
            parse(html, Timestamp::from_unix(CAPTURED_AT).expect("plausible")).expect("parses");

        let hourly = &snapshot.windows[0];
        assert_eq!(hourly.title, "Hourly");
        assert_eq!(
            hourly.key,
            WindowKey::for_length(WindowLength::from_secs(3_600).expect("a span is not zero"))
        );
        assert!((hourly.used_percent - 2.5).abs() < 0.000_001);
        assert!((snapshot.windows[1].used_percent - 4.2).abs() < 0.000_001);
    }

    #[test]
    fn a_capitalised_used_still_parses() {
        let html = "<div><span>Session usage</span><span>1.2% Used</span></div>\
                    <div><span>Weekly usage</span><span>3.4% USED</span></div>";

        let snapshot =
            parse(html, Timestamp::from_unix(CAPTURED_AT).expect("plausible")).expect("parses");

        assert!((snapshot.windows[0].used_percent - 1.2).abs() < 0.000_001);
        assert!((snapshot.windows[1].used_percent - 3.4).abs() < 0.000_001);
    }

    #[test]
    fn a_width_style_percent_stands_in_when_no_used_text_is_rendered() {
        let html = "<div><span>Session usage</span>\
                    <div class=\"progress\" style=\"width: 42%\"></div>\
                    <span>Weekly usage</span><span>7% used</span></div>";

        let snapshot =
            parse(html, Timestamp::from_unix(CAPTURED_AT).expect("plausible")).expect("parses");

        assert!((snapshot.windows[0].used_percent - 42.0).abs() < 0.000_001);
        assert!((snapshot.windows[1].used_percent - 7.0).abs() < 0.000_001);
    }

    #[test]
    fn a_reset_beyond_the_next_label_binds_to_its_own_block() {
        // The filler pushes the weekly reset far down the session block's tail; it must
        // still belong to the weekly meter, whose window the session block never enters.
        let filler = "<span class=\"grid-cell\"></span>".repeat(40);
        let html = format!(
            "<div><span>Session usage</span><span>0.1% used</span>\
             <span>Weekly usage</span><span>0.7% used</span>{filler}\
             <div class=\"local-time\" data-time=\"2026-02-02T00:00:00Z\">Resets</div></div>"
        );

        let snapshot =
            parse(&html, Timestamp::from_unix(CAPTURED_AT).expect("plausible")).expect("parses");

        assert_eq!(snapshot.windows[0].resets_at, None);
        assert_eq!(
            snapshot.windows[1].resets_at,
            Some(Timestamp::from_unix(1_769_990_400).expect("2026-02-02T00:00:00Z"))
        );
    }

    #[test]
    fn a_signed_out_page_is_a_rejected_session_not_a_broken_provider() {
        let html = "<html><body><h1>Sign in to Ollama</h1>\
                    <form action=\"/auth/signin\" method=\"post\">\
                    <input type=\"email\" name=\"email\" />\
                    <input type=\"password\" name=\"password\" /></form></body></html>";

        let result = parse(html, Timestamp::from_unix(CAPTURED_AT).expect("plausible"));

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn a_generic_sign_in_mention_is_not_a_signed_out_page() {
        let html = "<html><body><h2>Usage Dashboard</h2>\
                    <p>If you have an account, you can sign in from the homepage.</p>\
                    <div>No usage rows rendered.</div></body></html>";

        let result = parse(html, Timestamp::from_unix(CAPTURED_AT).expect("plausible"));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_page_that_renders_no_numbers_is_malformed() {
        let result = parse(
            "<html><body>No usage here. login status unknown.</body></html>",
            Timestamp::from_unix(CAPTURED_AT).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn the_signin_landings_are_recognised_and_the_settings_page_is_not() {
        let url = |text: &str| reqwest::Url::parse(text).expect("parses");

        assert!(is_signin_redirect(
            &url("https://ollama.com/signin"),
            "ollama.com"
        ));
        assert!(is_signin_redirect(
            &url("https://www.ollama.com/signin"),
            "ollama.com"
        ));
        assert!(is_signin_redirect(
            &url("https://signin.ollama.com/anything"),
            "ollama.com"
        ));
        assert!(is_signin_redirect(
            &url("https://auth.workos.com/user_management/authorize?client_id=x"),
            "ollama.com"
        ));
        assert!(!is_signin_redirect(
            &url("https://ollama.com/settings"),
            "ollama.com"
        ));
        assert!(!is_signin_redirect(
            &url("https://ollama.com/signin/other"),
            "ollama.com"
        ));
        assert!(!is_signin_redirect(
            &url("https://example.com/signin"),
            "ollama.com"
        ));
    }

    #[test]
    fn the_settings_page_is_fetched_with_the_session_cookie() {
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[("GET /settings", 200, &[], SETTINGS)]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        let request = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "ollama");
        assert_eq!(snapshot.windows.len(), 2);
        assert!(
            request.contains("cookie: session=session-value"),
            "{request}"
        );
        assert!(request.contains("accept: text/html"), "{request}");
    }

    #[test]
    fn a_redirect_that_lands_on_signin_rejects_the_session() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            ("GET /settings", 302, &[("Location", "/signin")], ""),
            ("GET /signin", 200, &[], "<html>sign in</html>"),
        ]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn an_http_rejection_names_the_expired_session() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[("GET /settings", 401, &[], "")]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }
}
