//! Sakana AI's console quotas, scraped from the billing pages the signed-in browser sees.
//!
//! The console meters its plans on server-rendered HTML — there is no JSON API behind it
//! — so both the subscription windows and the pay-as-you-go tab are read with
//! deliberately shallow label scans rather than a document model: the page is HTML and
//! can move, and when it does the fetch fails loudly as a malformed page instead of
//! quietly inventing numbers. Every reset the page prints is UTC — the console
//! localises it only in the viewer's own browser, which this reader never runs. The
//! pay-as-you-go tab is a second, best-effort page: when it fails or carries nothing,
//! its rows stay away and the quota windows are unaffected.

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
use time::PrimitiveDateTime;

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "sakana";

const BILLING_URL: &str = "https://console.sakana.ai/billing";
const PAYG_URL: &str = "https://console.sakana.ai/billing?tab=payAsYouGo";
const SESSION_URL: &str = "https://console.sakana.ai/";
const SITE_HOST: &str = "console.sakana.ai";
const COOKIE_DOMAINS: &[&str] = &["console.sakana.ai"];
const FIVE_HOUR: u64 = 5 * 60 * 60;
const WEEKLY: u64 = 7 * 24 * 60 * 60;

/// Sakana as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Sakana",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in console.sakana.ai browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Sakana::new(options)?))
}

/// One Sakana account, authenticated by one explicitly chosen browser profile. The gate
/// is the whole jar: the console's session cookie has no name worth pinning.
pub struct Sakana {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Sakana {
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
            return format!(
                "{base_url}{}",
                url.trim_start_matches("https://console.sakana.ai")
            );
        }
        url.to_owned()
    }

    /// The host the console lives on — the loopback server during tests.
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

    fn page_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
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
        let request = self.page_request(&self.url(BILLING_URL), &session.header)?;
        let (body, landed_on) = super::request_with_url(PROVIDER_ID, &self.client, request).await?;
        // A landing anywhere but the console is the sign-in bounce an expired session
        // gets; the sign-in page itself answers 200 like a billing page would.
        if !is_console(&landed_on, &self.site_host()) {
            return Err(ProviderError::Credential { status: 401 });
        }
        // The pay-as-you-go tab is best-effort: whatever goes wrong with it costs only
        // its own rows, never the quota windows.
        let payg = super::request(
            PROVIDER_ID,
            &self.client,
            self.page_request(&self.url(PAYG_URL), &session.header)?,
        )
        .await
        .ok();
        parse(&body, payg.as_deref(), Timestamp::now())
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.page_request(BILLING_URL, header) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        // The proof is where the request lands, not only that it succeeds: an expired
        // session is bounced off the console, and the landing answers 200.
        let Ok(response) = self.client.execute(request).await else {
            return crate::browser::auth::Validation::Unreachable;
        };
        let landed_on = response.url().clone();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        match http::check(response.status(), retry_after.as_deref()) {
            Ok(()) if !is_console(&landed_on, &self.site_host()) => {
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
            &cookie_query(),
            BILLING_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for Sakana {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sakana")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Sakana {
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

/// Whether the exchange stayed on the console: a landing anywhere else is the sign-in
/// bounce an expired session gets.
fn is_console(url: &reqwest::Url, site_host: &str) -> bool {
    url.host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case(site_host))
}

/// The billing page's quota windows and plan title.
#[derive(Debug)]
struct Billing {
    plan_label: Option<String>,
    five_hour: Option<Quota>,
    weekly: Option<Quota>,
}

/// One quota window.
#[derive(Debug)]
struct Quota {
    used_percent: f64,
    resets_at: Option<Timestamp>,
}

impl Quota {
    fn window(&self, title: &str, length_secs: u64) -> Window {
        let length = WindowLength::from_secs(length_secs).expect("a fixed span is not zero");
        Window {
            key: WindowKey::for_length(length),
            title: title.to_owned(),
            subtitle: None,
            used_percent: self.used_percent,
            resets_at: self.resets_at,
            length: Some(length),
        }
    }
}

/// The pay-as-you-go tab's balance, its rolling usage total, and the date range that
/// total covers.
#[derive(Debug)]
struct Payg {
    credit_balance: f64,
    usage_total: Option<f64>,
    period_label: Option<String>,
}

/// The two billing pages as a snapshot: the five-hour and weekly windows, the plan row,
/// and — when the pay-as-you-go page read — the balance rows.
pub fn parse(
    html: &str,
    payg_html: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let billing = parse_billing(html)?;
    let payg = payg_html.and_then(parse_payg);

    let mut windows = Vec::new();
    if let Some(quota) = billing.five_hour {
        windows.push(quota.window("5-hour", FIVE_HOUR));
    }
    if let Some(quota) = billing.weekly {
        windows.push(quota.window("Weekly", WEEKLY));
    }
    let mut details = Vec::new();
    if let Some(plan) = billing.plan_label {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: plan,
            }],
        });
    }
    if let Some(payg) = payg {
        let mut rows = vec![DetailRow {
            label: "Balance".to_owned(),
            value: format!("${:.2}", payg.credit_balance),
        }];
        if let Some(total) = payg.usage_total {
            rows.push(DetailRow {
                label: "Usage".to_owned(),
                value: format!("${total:.2}"),
            });
        }
        if let Some(period) = payg.period_label {
            rows.push(DetailRow {
                label: "Period".to_owned(),
                value: period,
            });
        }
        details.push(DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows,
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

fn parse_billing(html: &str) -> Result<Billing, ProviderError> {
    let five_hour = parse_window(html, "5-hour")?;
    let weekly = parse_window(html, "Weekly")?;
    if five_hour.is_none() && weekly.is_none() {
        return Err(ProviderError::malformed(
            "the Sakana billing page rendered no usage windows",
        ));
    }
    Ok(Billing {
        plan_label: plan_label(html),
        five_hour,
        weekly,
    })
}

/// One quota window. A window whose label paragraph exists but whose percent does not is
/// a page that moved; a window whose label is absent entirely is a plan without that
/// meter, and stays away.
fn parse_window(html: &str, label: &str) -> Result<Option<Quota>, ProviderError> {
    let Some(body) = window_body(html, label) else {
        return Ok(None);
    };
    let used_percent = used_percent(body).filter(|percent| (0.0..=100.0).contains(percent));
    let Some(used_percent) = used_percent else {
        return Err(ProviderError::malformed(format!(
            "the Sakana {label} window has no readable percentage"
        )));
    };
    Ok(Some(Quota {
        used_percent,
        resets_at: reset_time(body),
    }))
}

/// A window's body: from its label paragraph to the next quota label or titled card,
/// whichever the page serves first.
fn window_body<'a>(html: &'a str, label: &str) -> Option<&'a str> {
    let (_, body_start) = labelled_element(html, 0, "p", label)?;
    let end = ["5-hour", "Weekly"]
        .iter()
        .filter_map(|other| labelled_element(html, body_start, "p", other).map(|(start, _)| start))
        .min()
        .or_else(|| card_start(html, body_start))
        .unwrap_or(html.len());
    let body = html[body_start..end].trim();
    (!body.is_empty()).then_some(body)
}

/// The next card the page opens — a quota window never runs past one.
fn card_start(html: &str, from: usize) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut search = from;
    while let Some((start, end)) = open_tag(&lower, search, "div") {
        search = start + 1;
        let tag = &lower[start..end];
        if [
            "data-slot=\"card\"",
            "data-slot='card'",
            "data-slot=\"card-title\"",
            "data-slot='card-title'",
        ]
        .iter()
        .any(|slot| tag.contains(slot))
        {
            return Some(start);
        }
    }
    None
}

/// The window's percent: the first paragraph whose whole text is `N% used`.
fn used_percent(body: &str) -> Option<f64> {
    let mut search = 0;
    while let Some(((start, _), (content_start, content_end))) = paragraph(body, search) {
        search = start + 1;
        let text = body[content_start..content_end].trim();
        let lowered = text.to_ascii_lowercase();
        let Some(number) = lowered.strip_suffix("% used") else {
            continue;
        };
        if let Some(percent) = plain_number(number) {
            return Some(percent);
        }
    }
    None
}

/// The window's reset instant: the paragraph that opens "Resets on", printed in UTC.
fn reset_time(body: &str) -> Option<Timestamp> {
    let mut search = 0;
    while let Some(((start, _), (content_start, content_end))) = paragraph(body, search) {
        search = start + 1;
        let text = body[content_start..content_end].trim();
        if !text.to_ascii_lowercase().starts_with("resets on ") {
            continue;
        }
        return parse_reset(&text["Resets on ".len()..]);
    }
    None
}

/// The reset the console prints — "June 23, 2026 at 2:53 PM" — read as UTC: the page
/// always renders UTC, and the browser's local correction never runs here.
fn parse_reset(value: &str) -> Option<Timestamp> {
    let format = time::format_description::parse_borrowed::<2>(
        "[month repr:long] [day], [year] at [hour repr:12 padding:none]:[minute] [period]",
    )
    .ok()?;
    let stamp = PrimitiveDateTime::parse(value.trim(), &format)
        .ok()?
        .assume_utc();
    Timestamp::from_unix(stamp.unix_timestamp()).ok()
}

/// The plan card's title: the plan's name, and its price when the card shows one.
fn plan_label(html: &str) -> Option<String> {
    let card_end = card_title_end(html)?;
    let name_start = card_end + html[card_end..].find("<span>")?;
    let (text_start, text_end) = element_text(html, name_start)?;
    let name = html[text_start..text_end].trim();
    if name.is_empty() {
        return None;
    }
    // The price span follows the name span's close with nothing between but whitespace.
    let tail = html[text_end..]
        .trim_start()
        .strip_prefix("</span>")?
        .trim_start();
    if !tail.starts_with("<span") {
        return Some(name.to_owned());
    }
    let price_start = html.len() - tail.len();
    let Some((price_text_start, price_text_end)) = element_text(html, price_start) else {
        return Some(name.to_owned());
    };
    let price = html[price_text_start..price_text_end].trim();
    if price.is_empty() {
        return Some(name.to_owned());
    }
    Some(format!("{name} {price}"))
}

/// The end of the first plan card title's opening tag.
fn card_title_end(html: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut search = 0;
    while let Some((start, end)) = open_tag(&lower, search, "div") {
        search = start + 1;
        if lower[start..end].contains("data-slot=\"card-title\"") {
            return Some(end);
        }
    }
    None
}

/// The tab's prepaid credit: the first tabular paragraph within a short reach of the
/// "Credit balance" heading.
fn credit_balance(html: &str) -> Option<f64> {
    let (_, heading_end) = labelled_element(html, 0, "h2", "Credit balance")?;
    let mut search = heading_end;
    while let Some(((start, open_end), (content_start, content_end))) = paragraph(html, search) {
        search = start + 1;
        if start > heading_end + 900 {
            return None;
        }
        if !html[start..open_end]
            .to_ascii_lowercase()
            .contains("tabular-nums")
        {
            continue;
        }
        return amount(html[content_start..content_end].trim());
    }
    None
}

/// The tab's rolling usage total: the span right after the "Usage" heading, printed as
/// "Total: $N" with React's hydration comments in between.
fn usage_total(html: &str) -> Option<f64> {
    let (_, heading_end) = labelled_element(html, 0, "h2", "Usage")?;
    let tail = html[heading_end..].trim_start();
    if !tail.starts_with("<span") {
        return None;
    }
    let span_start = heading_end + (html[heading_end..].len() - tail.len());
    let (text_start, _) = element_text(html, span_start)?;
    // The span's text runs to its own close, past the hydration comments inside it.
    let close = span_start + html[span_start..].find("</span>")?;
    let text = strip_comments(&html[text_start..close]);
    let after_total = text
        .to_ascii_lowercase()
        .strip_prefix("total:")?
        .trim()
        .to_owned();
    amount(&after_total)
}

/// The date-range picker's label: the button's whole text, hydration comments stripped.
fn period_label(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let marker = "aria-label=\"usage date range\"";
    let found = lower.find(marker)?;
    let open_end = found + marker.len() + html[found + marker.len()..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</button>")?;
    let text = strip_comments(&html[open_end..close]);
    (!text.is_empty()).then_some(text)
}

/// The pay-as-you-go tab, or `None` when its balance is not on the page.
fn parse_payg(html: &str) -> Option<Payg> {
    Some(Payg {
        credit_balance: credit_balance(html)?,
        usage_total: usage_total(html),
        period_label: period_label(html),
    })
}

/// React's `<!-- -->` hydration boundary comments, stripped, with the whitespace around
/// what remains collapsed to single spaces.
fn strip_comments(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(found) = rest.find("<!--") {
        stripped.push_str(&rest[..found]);
        rest = rest[found + 4..]
            .split_once("-->")
            .map_or("", |(_, tail)| tail);
    }
    stripped.push_str(rest);
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The offsets of the next opening tag named `name` at or after `from`: its `<` and the
/// position just past its `>`. The tag name is matched exactly, so a `<p` hunt never
/// stops at a `<path`.
fn open_tag(html: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut search = from;
    let opening = format!("<{name}");
    while let Some(found) = html[search..].find(&opening) {
        let start = search + found;
        search = start + opening.len();
        match html[search..].chars().next() {
            Some(' ' | '\t' | '\r' | '\n' | '>' | '/') => {}
            _ => continue,
        }
        let end = html[search..].find('>').map(|found| search + found + 1)?;
        return Some((start, end));
    }
    None
}

/// The next `<p>` element at or after `from`: its opening tag's byte range and its text
/// content's byte range.
fn paragraph(html: &str, from: usize) -> Option<((usize, usize), (usize, usize))> {
    let (start, open_end) = open_tag(html, from, "p")?;
    let content_end = open_end + html[open_end..].find('<')?;
    Some(((start, open_end), (open_end, content_end)))
}

/// The next element named `tag` at or after `from` whose whole text is `label` (any
/// capitalisation): the offsets of its `<` and of the end of its closing tag.
fn labelled_element(html: &str, from: usize, tag: &str, label: &str) -> Option<(usize, usize)> {
    let mut search = from;
    let closing = format!("</{tag}>");
    while let Some((start, open_end)) = open_tag(html, search, tag) {
        search = start + 1;
        let content_end = open_end + html[open_end..].find('<')?;
        if !html[open_end..content_end]
            .trim()
            .eq_ignore_ascii_case(label)
        {
            continue;
        }
        let tail = html[content_end..].trim_start();
        if !tail.starts_with(&closing) {
            continue;
        }
        let element_end = content_end + (html[content_end..].len() - tail.len()) + closing.len();
        return Some((start, element_end));
    }
    None
}

/// The text of the element whose opening `<` starts at `tag_start`: where its text
/// begins and where it ends.
fn element_text(html: &str, tag_start: usize) -> Option<(usize, usize)> {
    let open_end = tag_start + html[tag_start..].find('>')? + 1;
    let content_end = open_end + html[open_end..].find('<')?;
    Some((open_end, content_end))
}

/// A plain number: digits, or digits with a fraction — nothing else.
fn plain_number(text: &str) -> Option<f64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (!fraction.is_empty() && !digits(fraction)) {
        return None;
    }
    text.parse().ok()
}

/// A dollar amount as the console prints it: an optional `$`, then digits with optional
/// thousands commas and an optional fraction.
fn amount(text: &str) -> Option<f64> {
    let digits = text.strip_prefix('$').unwrap_or(text);
    let whole = digits.split_once('.').map_or(digits, |(whole, _)| whole);
    let whole_ok = !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit() || b == b',');
    let fraction_ok = match digits.split_once('.') {
        Some((_, fraction)) => !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit()),
        None => true,
    };
    if !whole_ok || !fraction_ok {
        return None;
    }
    let cleaned: String = digits.chars().filter(|c| *c != ',').collect();
    cleaned.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Sakana, is_console, parse};
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
                ) VALUES ('.console.sakana.ai', ?1, ?2, '/', 0, 1, 0, 0, 0)",
                ("sakana_session", "jar-value"),
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the given routes in order, asserting each request
    /// opens with its expected request line. Pass only routes that will actually be hit.
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
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("writes response");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Sakana {
        Sakana::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Sakana) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const BILLING: &str = include_str!("../../../tests/fixtures/sakana/billing.html");
    const PAYG: &str = include_str!("../../../tests/fixtures/sakana/pay-as-you-go.html");
    const CAPTURED_AT: i64 = 1_700_000_000;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_recorded_pages_draw_both_windows_the_plan_and_the_balance() {
        let snapshot = parse(BILLING, Some(PAYG), at(CAPTURED_AT)).expect("parses the pages");

        let five_hour = &snapshot.windows[0];
        assert_eq!(five_hour.title, "5-hour");
        assert_eq!(
            five_hour.key,
            WindowKey::for_length(
                WindowLength::from_secs(5 * 60 * 60).expect("a span is not zero")
            )
        );
        assert!((five_hour.used_percent - 92.0).abs() < 0.000_001);
        assert_eq!(five_hour.resets_at, Some(at(1_782_226_380)));
        let weekly = &snapshot.windows[1];
        assert_eq!(weekly.title, "Weekly");
        assert_eq!(
            weekly.key,
            WindowKey::for_length(
                WindowLength::from_secs(7 * 24 * 60 * 60).expect("a span is not zero")
            )
        );
        assert!((weekly.used_percent - 32.0).abs() < 0.000_001);
        assert_eq!(weekly.resets_at, Some(at(1_782_691_200)));
        assert_eq!(snapshot.details[0].title, "Plan");
        assert_eq!(snapshot.details[0].rows[0].value, "Standard $20/mo");
        assert_eq!(snapshot.details[1].title, "Balance");
        assert_eq!(snapshot.details[1].rows[0].value, "$12.34");
        assert_eq!(snapshot.details[1].rows[1].value, "$5.67");
        assert_eq!(
            snapshot.details[1].rows[2].value,
            "Jun 02, 2026 - Jul 01, 2026"
        );
    }

    #[test]
    fn an_out_of_range_percentage_is_malformed() {
        let html = BILLING.replace("92% used", "101% used");

        let result = parse(&html, None, at(CAPTURED_AT));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_page_without_windows_is_malformed() {
        let result = parse("<main>Billing</main>", None, at(CAPTURED_AT));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn a_window_without_a_percent_line_is_malformed() {
        let html = BILLING.replacen(
            "<p class=\"text-muted-foreground text-sm\">92% used</p>",
            "",
            1,
        );

        let result = parse(&html, None, at(CAPTURED_AT));

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn an_unparsable_reset_leaves_the_window_without_a_reset() {
        let html = BILLING.replace("June 23, 2026 at 2:53 PM", "soon-ish");

        let snapshot = parse(&html, None, at(CAPTURED_AT)).expect("parses");

        assert!((snapshot.windows[0].used_percent - 92.0).abs() < 0.000_001);
        assert_eq!(snapshot.windows[0].resets_at, None);
    }

    #[test]
    fn a_payg_page_without_a_usage_total_still_carries_the_balance() {
        let payg = PAYG.replacen(
            "<span class=\"text-muted-foreground text-sm\">Total<!-- -->: <!-- -->$5.67</span>",
            "",
            1,
        );

        let snapshot = parse(BILLING, Some(&payg), at(CAPTURED_AT)).expect("parses");

        let balance = &snapshot.details[1];
        assert_eq!(balance.rows[0].value, "$12.34");
        assert!(balance.rows.iter().all(|row| row.label != "Usage"));
    }

    #[test]
    fn the_billing_page_alone_has_no_balance_section() {
        let snapshot = parse(BILLING, None, at(CAPTURED_AT)).expect("parses");

        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, "Plan");
    }

    #[test]
    fn a_bounce_off_the_console_is_a_rejected_session() {
        let url = |text: &str| reqwest::Url::parse(text).expect("parses");

        assert!(is_console(
            &url("https://console.sakana.ai/billing"),
            "console.sakana.ai"
        ));
        assert!(!is_console(
            &url("https://auth.sakana.ai/login"),
            "console.sakana.ai"
        ));
    }

    #[test]
    fn both_pages_are_fetched_with_the_whole_jar() {
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[
            ("GET /billing", 200, BILLING),
            ("GET /billing?tab=payAsYouGo", 200, PAYG),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        let first = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        let second = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "sakana");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.details.len(), 2);
        assert!(
            first.contains("cookie: sakana_session=jar-value"),
            "{first}"
        );
        assert!(first.contains("accept: text/html"), "{first}");
        assert!(
            second.contains("cookie: sakana_session=jar-value"),
            "{second}"
        );
    }

    #[test]
    fn a_failing_payg_page_costs_only_its_rows() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            ("GET /billing", 200, BILLING),
            ("GET /billing?tab=payAsYouGo", 500, "boom"),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, "Plan");
    }

    #[test]
    fn an_http_rejection_names_the_expired_session() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[("GET /billing", 401, "")]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }
}
