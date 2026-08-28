//! Mistral's La Plateforme spend, wallet credits, and Vibe plan, read from the browser
//! session that signs in to the admin console.
//!
//! The session gate is any cookie whose name starts with `ory_session_` — Ory rotates the
//! suffix, so no exact name is stable. The billing usage is the required call: its payload
//! aggregates every metered category through the price table into one monthly spend and
//! token totals. The credits and Vibe calls are supplementary: when either fails, its rows
//! or window simply stay away. The Vibe call crosses to console.mistral.ai with a Cookie
//! header rebuilt from the CSRF pair and the Ory sessions alone — every other admin cookie
//! stays origin-bound. The per-day and per-model breakdowns CodexBar renders are not
//! ported; Tidemark has no cost-history surface to carry them.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use serde::Deserialize;
use std::collections::HashMap;
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
pub const PROVIDER_ID: &str = "mistral";

const USAGE_URL: &str = "https://admin.mistral.ai/api/billing/v2/usage";
const CREDITS_URL: &str = "https://admin.mistral.ai/api/billing/credits";
const VIBE_URL: &str = "https://console.mistral.ai/api-ui/trpc/billing.vibeUsage?batch=1&input=%7B%220%22%3A%7B%22json%22%3Anull%2C%22meta%22%3A%7B%22values%22%3A%5B%22undefined%22%5D%2C%22v%22%3A1%7D%7D%7D";
const SESSION_URL: &str = "https://admin.mistral.ai/";
const COOKIE_DOMAINS: &[&str] = &[
    "mistral.ai",
    "admin.mistral.ai",
    "auth.mistral.ai",
    "console.mistral.ai",
];
const SESSION_PREFIX: &str = "ory_session_";
const CSRF_COOKIE: &str = "csrftoken";
/// The Vibe plan is a calendar-month pool, so its window carries the monthly length.
const MONTHLY: u64 = 2_592_000;

/// Mistral as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Mistral",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in mistral.ai browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Mistral::new(options)?))
}

/// One Mistral account, authenticated by one explicitly chosen browser profile.
pub struct Mistral {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Mistral {
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
            let path = url
                .trim_start_matches("https://admin.mistral.ai")
                .trim_start_matches("https://console.mistral.ai");
            return format!("{base_url}{path}");
        }
        url.to_owned()
    }

    fn admin_request(
        &self,
        url: &str,
        cookie: &str,
        csrf: Option<&str>,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut builder = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::COOKIE, cookie);
        if let Some(csrf) = csrf {
            builder = builder.header("x-csrftoken", csrf);
        }
        builder
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    fn vibe_request(
        &self,
        url: &str,
        cookie: &str,
        csrf: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::COOKIE, cookie)
            .header("x-csrftoken", csrf)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let selection = self.selection.as_ref().ok_or(ProviderError::NoCredential)?;
        let session = session::session_prefix(
            self.home.as_deref(),
            self.storage.as_ref(),
            selection,
            SESSION_PREFIX,
            &cookie_query(),
            SESSION_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let csrf = cookie_value(&session.header, CSRF_COOKIE).and_then(validated_csrf);

        let (month, year) = month_year(Timestamp::now());
        let usage = parse_usage(
            &super::request(
                PROVIDER_ID,
                &self.client,
                self.admin_request(
                    &self.url(&format!("{USAGE_URL}?month={month}&year={year}")),
                    &session.header,
                    csrf.as_deref(),
                )?,
            )
            .await?,
        )?;
        // Credits and the Vibe plan are supplementary: whatever fails here costs only
        // its own rows or window, never the spend reading.
        let credits = super::request(
            PROVIDER_ID,
            &self.client,
            self.admin_request(&self.url(CREDITS_URL), &session.header, csrf.as_deref())?,
        )
        .await
        .ok()
        .and_then(|body| parse_credits(&body).ok());
        let vibe = if let Some(csrf) = csrf.as_deref() {
            super::request(
                PROVIDER_ID,
                &self.client,
                self.vibe_request(
                    &self.url(VIBE_URL),
                    &console_cookie(csrf, &session.header),
                    csrf,
                )?,
            )
            .await
            .ok()
            .and_then(|body| parse_vibe(&body).ok())
        } else {
            None
        };

        parse(usage, credits, vibe, Timestamp::now())
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let csrf = cookie_value(header, CSRF_COOKIE).and_then(validated_csrf);
        let Ok(request) = self.admin_request(USAGE_URL, header, csrf.as_deref()) else {
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
        session::inspect_sources_prefix(
            self.home.as_deref(),
            self.storage.as_ref(),
            SESSION_PREFIX,
            &cookie_query(),
            USAGE_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for Mistral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mistral")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Mistral {
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

/// A named cookie's value from a Cookie header.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    header
        .split(';')
        .map(str::trim)
        .find(|pair| pair.starts_with(&prefix))
        .and_then(|pair| pair.get(prefix.len()..))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The CSRF token as the console accepts it: present, and free of the characters that
/// could smuggle a second cookie pair or header into the request.
fn validated_csrf(token: String) -> Option<String> {
    let trimmed = token.trim();
    if trimmed.is_empty() || trimmed.contains([';', ',', '\r', '\n']) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// The Cookie header console.mistral.ai needs: the CSRF pair plus the Ory sessions from
/// the admin jar — every other admin cookie stays origin-bound.
fn console_cookie(csrf: &str, admin_header: &str) -> String {
    let mut pairs = vec![format!("{CSRF_COOKIE}={csrf}")];
    pairs.extend(
        admin_header
            .split(';')
            .map(str::trim)
            .filter(|pair| pair.starts_with(SESSION_PREFIX))
            .map(str::to_owned),
    );
    pairs.join("; ")
}

/// The UTC month and year the billing query asks about.
fn month_year(now: Timestamp) -> (u32, i32) {
    let date = OffsetDateTime::from_unix_timestamp(now.as_unix())
        .expect("a plausible timestamp has a date");
    (u8::from(date.month()) as u32, date.year())
}

/// The month's metered spend and token totals — everything Tidemark keeps from the big
/// per-day, per-model billing payload.
#[derive(Debug)]
pub struct Usage {
    total_cost: f64,
    currency_symbol: String,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
}

/// The wallet's amounts, floored at zero after the ongoing usage is deducted.
#[derive(Debug)]
pub struct Credits {
    wallet: f64,
    credit_notes: f64,
    ongoing: f64,
    available: f64,
    currency: String,
}

/// The Vibe coding plan's monthly percent.
#[derive(Debug)]
pub struct Vibe {
    usage_percentage: f64,
    resets_at: Option<Timestamp>,
}

/// Turns the three console payloads into the monthly-plan window, the spend rows, and the
/// wallet rows. The credits and Vibe contributions arrive as `None` when their calls
/// failed, and the snapshot simply goes without them.
pub fn parse(
    usage: Usage,
    credits: Option<Credits>,
    vibe: Option<Vibe>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let mut windows = Vec::new();
    if let Some(vibe) = vibe {
        let length = WindowLength::from_secs(MONTHLY).expect("a fixed span is not zero");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: "Monthly plan".to_owned(),
            subtitle: None,
            used_percent: vibe.usage_percentage,
            resets_at: vibe.resets_at,
            length: Some(length),
        });
    }

    let mut rows = vec![DetailRow {
        label: "Spend this month".to_owned(),
        value: format!("{}{:.4}", usage.currency_symbol, usage.total_cost),
    }];
    if usage.input_tokens > 0 {
        rows.push(DetailRow {
            label: "Input tokens".to_owned(),
            value: number_text(usage.input_tokens as f64),
        });
    }
    if usage.output_tokens > 0 {
        rows.push(DetailRow {
            label: "Output tokens".to_owned(),
            value: number_text(usage.output_tokens as f64),
        });
    }
    if usage.cached_tokens > 0 {
        rows.push(DetailRow {
            label: "Cached tokens".to_owned(),
            value: number_text(usage.cached_tokens as f64),
        });
    }
    let mut details = vec![DetailSection {
        title: "Usage".to_owned(),
        rows,
    }];
    if let Some(credits) = credits {
        let symbol = currency_symbol(&credits.currency);
        details.push(DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows: vec![
                DetailRow {
                    label: "Wallet".to_owned(),
                    value: format!("{symbol}{:.2}", credits.wallet),
                },
                DetailRow {
                    label: "Credit notes".to_owned(),
                    value: format!("{symbol}{:.2}", credits.credit_notes),
                },
                DetailRow {
                    label: "Ongoing usage".to_owned(),
                    value: format!("{symbol}{:.2}", credits.ongoing),
                },
                DetailRow {
                    label: "Available".to_owned(),
                    value: format!("{symbol}{:.2}", credits.available),
                },
            ],
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

/// Aggregates every metered category of the billing payload through the price table.
pub fn parse_usage(body: &str) -> Result<Usage, ProviderError> {
    let billing: Billing = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not a Mistral usage body: {error}")))?;

    let prices = price_index(&billing.prices);
    let mut total_cost = 0.0;
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    let mut cached_tokens = 0;
    let mut aggregate = |models: &HashMap<String, ModelUsage>, counts_tokens: bool| {
        for model in models.values() {
            let (cost, input, output, cached) = aggregate_model(model, &prices, counts_tokens);
            total_cost = finite_sum(total_cost, cost);
            if counts_tokens {
                input_tokens += input;
                output_tokens += output;
                cached_tokens += cached;
            }
        }
    };
    aggregate(&billing.completion.models, true);
    for category in [&billing.ocr, &billing.connectors, &billing.audio] {
        aggregate(&category.models, false);
    }
    if let Some(libraries) = &billing.libraries_api {
        if let Some(pages) = &libraries.pages {
            aggregate(&pages.models, false);
        }
        if let Some(tokens) = &libraries.tokens {
            aggregate(&tokens.models, true);
        }
    }
    if let Some(fine_tuning) = &billing.fine_tuning {
        for models in [&fine_tuning.training, &fine_tuning.storage] {
            aggregate(models, false);
        }
    }

    let currency = billing
        .currency
        .as_deref()
        .map(str::trim)
        .filter(|currency| !currency.is_empty())
        .map(str::to_uppercase)
        .unwrap_or_else(|| "XXX".to_owned());
    let currency_symbol = billing
        .currency_symbol
        .as_deref()
        .map(str::trim)
        .filter(|symbol| !symbol.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| default_currency_symbol(&currency));

    Ok(Usage {
        total_cost,
        currency_symbol,
        input_tokens,
        output_tokens,
        cached_tokens,
    })
}

pub fn parse_credits(body: &str) -> Result<Credits, ProviderError> {
    let response: CreditsResponse = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a Mistral credits body: {error}"))
    })?;
    let credit_notes = response.credit_notes_amount.unwrap_or(0.0);
    let ongoing = response.ongoing_usage_balance.unwrap_or(0.0);
    let available = (response.wallet_amount + credit_notes - ongoing).max(0.0);
    if !response.wallet_amount.is_finite()
        || !credit_notes.is_finite()
        || !ongoing.is_finite()
        || !available.is_finite()
    {
        return Err(ProviderError::malformed(
            "Mistral credits amounts are not finite",
        ));
    }
    Ok(Credits {
        wallet: response.wallet_amount,
        credit_notes,
        ongoing,
        available,
        currency: response.currency,
    })
}

pub fn parse_vibe(body: &str) -> Result<Vibe, ProviderError> {
    let responses: Vec<VibeResponse> = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not a Mistral vibe body: {error}")))?;
    let json = &responses
        .first()
        .ok_or_else(|| ProviderError::malformed("Mistral vibe response is empty"))?
        .result
        .data
        .json;
    if !json.usage_percentage.is_finite() || !(0.0..=100.0).contains(&json.usage_percentage) {
        return Err(ProviderError::malformed(
            "Mistral vibe usage percentage is out of range",
        ));
    }
    let resets_at = json
        .reset_at
        .as_deref()
        .and_then(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
        })
        .and_then(|parsed| Timestamp::from_unix(parsed.unix_timestamp()).ok());
    Ok(Vibe {
        usage_percentage: json.usage_percentage,
        resets_at,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct Billing {
    #[serde(default)]
    completion: Category,
    #[serde(default)]
    ocr: Category,
    #[serde(default)]
    connectors: Category,
    #[serde(default)]
    audio: Category,
    #[serde(default)]
    libraries_api: Option<LibrariesApi>,
    #[serde(default)]
    fine_tuning: Option<FineTuning>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    currency_symbol: Option<String>,
    #[serde(default)]
    prices: Vec<Price>,
}

#[derive(Deserialize, Default)]
struct Category {
    #[serde(default)]
    models: HashMap<String, ModelUsage>,
}

#[derive(Deserialize)]
struct LibrariesApi {
    #[serde(default)]
    pages: Option<Category>,
    #[serde(default)]
    tokens: Option<Category>,
}

#[derive(Deserialize)]
struct FineTuning {
    #[serde(default)]
    training: HashMap<String, ModelUsage>,
    #[serde(default)]
    storage: HashMap<String, ModelUsage>,
}

#[derive(Deserialize, Default)]
struct ModelUsage {
    #[serde(default)]
    input: Vec<Entry>,
    #[serde(default)]
    output: Vec<Entry>,
    #[serde(default)]
    cached: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct Entry {
    billing_metric: Option<String>,
    billing_group: Option<String>,
    #[serde(default)]
    value: i64,
    value_paid: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct Price {
    billing_metric: Option<String>,
    billing_group: Option<String>,
    price: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct CreditsResponse {
    wallet_amount: f64,
    credit_notes_amount: Option<f64>,
    ongoing_usage_balance: Option<f64>,
    currency: String,
}

#[derive(Deserialize)]
struct VibeResponse {
    result: VibeResult,
}

#[derive(Deserialize)]
struct VibeResult {
    data: VibeData,
}

#[derive(Deserialize)]
struct VibeData {
    json: VibeJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct VibeJson {
    usage_percentage: f64,
    reset_at: Option<String>,
}

/// `metric::group` → unit price, keeping only the finite ones.
fn price_index(prices: &[Price]) -> HashMap<String, f64> {
    let mut index = HashMap::new();
    for price in prices {
        if let (Some(metric), Some(group), Some(text)) =
            (&price.billing_metric, &price.billing_group, &price.price)
            && let Some(value) = text
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
        {
            index.insert(format!("{metric}::{group}"), value);
        }
    }
    index
}

/// One model's cost, and its token counts when the category meters tokens.
fn aggregate_model(
    model: &ModelUsage,
    prices: &HashMap<String, f64>,
    counts_tokens: bool,
) -> (f64, i64, i64, i64) {
    let mut cost = 0.0;
    let mut input = 0;
    let mut output = 0;
    let mut cached = 0;
    for (entries, bucket) in [
        (&model.input, &mut input),
        (&model.output, &mut output),
        (&model.cached, &mut cached),
    ] {
        for entry in entries {
            let units = entry.value_paid.unwrap_or(entry.value);
            if counts_tokens {
                *bucket += units;
            }
            cost = finite_sum(cost, entry_cost(entry, units, prices));
        }
    }
    (cost, input, output, cached)
}

fn entry_cost(entry: &Entry, units: i64, prices: &HashMap<String, f64>) -> f64 {
    let Some(metric) = &entry.billing_metric else {
        return 0.0;
    };
    let Some(group) = &entry.billing_group else {
        return 0.0;
    };
    let price = prices
        .get(&format!("{metric}::{group}"))
        .copied()
        .unwrap_or(0.0);
    finite_sum(0.0, units as f64 * price)
}

/// Adds two costs, keeping the total finite the way the console's own sums stay readable.
fn finite_sum(total: f64, addition: f64) -> f64 {
    let updated = total + addition;
    if updated.is_finite() { updated } else { total }
}

fn default_currency_symbol(currency: &str) -> String {
    match currency {
        "EUR" => "€".to_owned(),
        "USD" => "$".to_owned(),
        "GBP" => "£".to_owned(),
        other => other.to_owned(),
    }
}

fn currency_symbol(currency: &str) -> String {
    default_currency_symbol(currency.trim().to_uppercase().as_str())
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
    use super::{Mistral, parse, parse_credits, parse_usage, parse_vibe};
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
        for (name, value) in [
            ("ory_session_admin", "session-value"),
            ("csrftoken", "csrf-token"),
            ("cf_clearance", "noise"),
        ] {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES ('.mistral.ai', ?1, ?2, '/', 0, 1, 0, 0, 0)",
                    (name, value),
                )
                .expect("inserts the session");
        }
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Mistral {
        Mistral::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Mistral) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const USAGE: &str = include_str!("../../../tests/fixtures/mistral/usage.json");
    const CREDITS: &str = include_str!("../../../tests/fixtures/mistral/credits.json");
    const VIBE: &str = include_str!("../../../tests/fixtures/mistral/vibe.json");

    #[test]
    fn the_recorded_usage_aggregates_through_the_price_table() {
        // Dropping value_paid would meter refunded tokens as if they were spent.
        let usage = parse_usage(USAGE).expect("parses the recorded body");

        assert!((usage.total_cost - 0.025_362_81).abs() < 0.000_001);
        assert_eq!(usage.currency_symbol, "€");
        assert_eq!(usage.input_tokens, 11_241);
        assert_eq!(usage.output_tokens, 4_097);
        assert_eq!(usage.cached_tokens, 0);
    }

    #[test]
    fn the_recorded_credits_floor_the_available_amount() {
        let credits = parse_credits(CREDITS).expect("parses the recorded body");

        assert_eq!(credits.wallet, 12.5);
        assert_eq!(credits.credit_notes, 2.25);
        assert_eq!(credits.ongoing, 1.5);
        assert_eq!(credits.available, 13.25);
    }

    #[test]
    fn overflowing_credits_are_malformed_rather_than_an_invented_amount() {
        let result = parse_credits(
            r#"{"wallet_amount":1e308,"credit_notes_amount":1e308,"ongoing_usage_balance":0,"currency":"USD"}"#,
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn the_recorded_bodies_draw_the_monthly_plan_window_and_both_sections() {
        let usage = parse_usage(USAGE).expect("parses");
        let credits = parse_credits(CREDITS).expect("parses");
        let vibe = parse_vibe(VIBE).expect("parses");

        let snapshot = parse(
            usage,
            Some(credits),
            Some(vibe),
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("builds the snapshot");
        let monthly = snapshot
            .windows
            .iter()
            .find(|window| {
                window.key
                    == WindowKey::for_length(
                        WindowLength::from_secs(2_592_000).expect("a fixed span is not zero"),
                    )
            })
            .expect("monthly plan window");

        assert!((monthly.used_percent - 42.5).abs() < 0.001);
        assert_eq!(
            monthly.resets_at,
            Some(Timestamp::from_unix(1_782_864_000).expect("2026-07-01T00:00:00Z"))
        );
        assert_eq!(snapshot.details[0].title, "Usage");
        assert_eq!(snapshot.details[0].rows[0].label, "Spend this month");
        assert_eq!(snapshot.details[0].rows[0].value, "€0.0254");
        assert_eq!(snapshot.details[0].rows[1].value, "11,241");
        assert_eq!(snapshot.details[0].rows[2].value, "4,097");
        assert_eq!(snapshot.details[1].title, "Balance");
        assert_eq!(snapshot.details[1].rows[3].value, "$13.25");
    }

    #[test]
    fn a_vibe_percentage_out_of_range_is_malformed() {
        let result = parse_vibe(
            r#"[{"result":{"data":{"json":{"usage_percentage":142.5,"reset_at":null}}}}]"#,
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn the_three_calls_carry_their_own_cookie_scopes() {
        // The console hop must not carry the admin jar wholesale.
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[
            ("GET /api/billing/v2/usage", 200, USAGE),
            ("GET /api/billing/credits", 200, CREDITS),
            ("GET /api-ui/trpc/billing.vibeUsage", 200, VIBE),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        let requests: Vec<String> = (0..3)
            .map(|_| {
                requests
                    .recv()
                    .expect("request captured")
                    .to_ascii_lowercase()
            })
            .collect();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "mistral");
        let usage = &requests[0];
        assert!(
            usage.starts_with("get /api/billing/v2/usage?month="),
            "{usage}"
        );
        assert!(usage.contains("year="), "{usage}");
        assert!(usage.contains("x-csrftoken: csrf-token"), "{usage}");
        assert!(usage.contains("csrftoken=csrf-token"), "{usage}");
        assert!(usage.contains("ory_session_admin=session-value"), "{usage}");
        let credits = &requests[1];
        assert!(credits.contains("x-csrftoken: csrf-token"), "{credits}");
        assert!(credits.contains("csrftoken=csrf-token"), "{credits}");
        let vibe = &requests[2];
        assert!(vibe.contains("cookie: csrftoken=csrf-token"), "{vibe}");
        assert!(vibe.contains("ory_session_admin=session-value"), "{vibe}");
        assert!(!vibe.contains("noise"), "{vibe}");
        assert!(vibe.contains("x-csrftoken: csrf-token"), "{vibe}");
    }

    #[test]
    fn failing_optional_calls_cost_only_their_rows_and_window() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            ("GET /api/billing/v2/usage", 200, USAGE),
            ("GET /api/billing/credits", 500, "boom"),
            ("GET /api-ui/trpc/billing.vibeUsage", 500, "boom"),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        server.join().expect("server exits");

        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, "Usage");
    }

    #[test]
    fn an_http_level_rejection_names_the_expired_session() {
        let home = gecko_home();
        let (base_url, _requests, server) =
            chained_server(&[("GET /api/billing/v2/usage", 401, "")]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }
}
