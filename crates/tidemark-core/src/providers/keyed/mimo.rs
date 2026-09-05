//! Xiaomi MiMo's platform token plan and balance, read from the browser session that signs
//! in to its console.
//!
//! The session is a *pair* of cookies — `api-platform_serviceToken` and `userId` — so a jar
//! carrying only one is treated as never having signed in. Every endpoint answers the same
//! `{code, message, data}` envelope, and a code of 401 or 403 inside a 200 means the chosen
//! browser signed out. The two plan calls are supplementary: when either fails, the balance
//! is still reported, without its window and plan row. Only live cookie databases are read;
//! CodexBar's Firefox session-restore decoder and its `~/.claude-envs` local-usage fallback
//! are not ported.

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
use time::{PrimitiveDateTime, format_description};

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "mimo";

const BALANCE_URL: &str = "https://platform.xiaomimimo.com/api/v1/balance";
const PLAN_DETAIL_URL: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/detail";
const PLAN_USAGE_URL: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/usage";
const SESSION_URL: &str = "https://platform.xiaomimimo.com/";
/// The cookie the session helper gates on; the API really authenticates against this one
/// *and* [`USER_ID_COOKIE`] together.
const SESSION_COOKIE_NAMES: &[&str] = &["api-platform_serviceToken"];
const USER_ID_COOKIE: &str = "userId";
/// What inspection requires before a jar is worth proving: the whole session pair, so a
/// half pair reads as never having signed in rather than as a provider's rejection.
const SESSION_PAIR: &[&str] = &["api-platform_serviceToken", USER_ID_COOKIE];
const COOKIE_DOMAINS: &[&str] = &["platform.xiaomimimo.com", "www.platform.xiaomimimo.com"];
/// The token plan is a calendar-month pool, so its window carries the monthly length.
const MONTHLY: u64 = 2_592_000;

/// MiMo as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Xiaomi MiMo",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in platform.xiaomimimo.com browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(MiMo::new_for_account(
        account,
        &credential,
        options,
    )?))
}

/// One MiMo account, authenticated by one explicitly chosen browser profile.
pub struct MiMo {
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

impl MiMo {
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

    /// The production URL, its host swapped for the loopback server during tests.
    fn url(&self, url: &str) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            let path = url.trim_start_matches("https://platform.xiaomimimo.com");
            return format!("{base_url}{path}");
        }
        url.to_owned()
    }

    fn api_request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-timezone", "UTC")
            .header(reqwest::header::COOKIE, cookie)
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
        // The helper gates on one name; MiMo's API wants the pair, and a jar holding the
        // service token without the user id never signed in to the platform.
        if !has_cookie(&session.header, USER_ID_COOKIE) {
            return Err(ProviderError::NoCredential);
        }
        let (balance, detail, usage) = tokio::join!(
            super::request(
                PROVIDER_ID,
                &self.client,
                self.api_request(&self.url(BALANCE_URL), &session.header)?,
            ),
            super::request(
                PROVIDER_ID,
                &self.client,
                self.api_request(&self.url(PLAN_DETAIL_URL), &session.header)?,
            ),
            super::request(
                PROVIDER_ID,
                &self.client,
                self.api_request(&self.url(PLAN_USAGE_URL), &session.header)?,
            ),
        );
        parse_for_account(
            &balance?,
            detail.ok().as_deref(),
            usage.ok().as_deref(),
            Timestamp::now(),
            &self.tidemark_account,
        )
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.api_request(&self.url(BALANCE_URL), header) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        // The platform refuses a session inside a 200 envelope, so the proof must read the
        // body: a status-only check would call an expired login ready.
        match super::validate_body(&self.client, request).await {
            Err(ProviderError::Credential { status: 401 | 403 }) => {
                crate::browser::auth::Validation::Rejected
            }
            Err(_) => crate::browser::auth::Validation::Unreachable,
            Ok(body) => match parse_balance(&body) {
                Ok(_) => crate::browser::auth::Validation::Ready,
                Err(ProviderError::Credential { status: 401 | 403 }) => {
                    crate::browser::auth::Validation::Rejected
                }
                Err(_) => crate::browser::auth::Validation::Unreachable,
            },
        }
    }

    async fn inspect_sources(&self) -> Vec<AuthCandidate> {
        let browsers = session::inspect_sources_all(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            SESSION_PAIR,
            &cookie_query(),
            BALANCE_URL,
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

impl fmt::Debug for MiMo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MiMo")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for MiMo {
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

/// Whether a Cookie header carries a named pair.
fn has_cookie(header: &str, name: &str) -> bool {
    let prefix = format!("{name}=");
    header
        .split(';')
        .any(|pair| pair.trim().starts_with(&prefix))
}

/// The balance reading, the one part of the snapshot MiMo must answer for.
struct Balance {
    balance: f64,
    currency: String,
    cash: Option<f64>,
    gift: Option<f64>,
}

/// The plan call's contribution: what the subscription is called and when it renews.
struct PlanDetail {
    plan_code: Option<String>,
    period_end: Option<Timestamp>,
}

/// The usage call's contribution: this month's token pool.
struct PlanUsage {
    used: i64,
    limit: i64,
    percent: f64,
}

#[derive(Deserialize)]
struct BalanceResponse {
    code: i64,
    message: Option<String>,
    data: Option<BalanceData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BalanceData {
    balance: String,
    currency: String,
    cash_balance: Option<String>,
    gift_balance: Option<String>,
}

#[derive(Deserialize)]
struct PlanDetailResponse {
    code: i64,
    data: Option<PlanDetailData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanDetailData {
    plan_code: Option<String>,
    current_period_end: Option<String>,
}

#[derive(Deserialize)]
struct PlanUsageResponse {
    code: i64,
    data: Option<PlanUsageData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsageData {
    month_usage: Option<MonthUsage>,
}

#[derive(Deserialize)]
struct MonthUsage {
    #[serde(default)]
    items: Vec<UsageItem>,
}

#[derive(Deserialize)]
struct UsageItem {
    used: i64,
    limit: i64,
    percent: f64,
}

/// Turns the three platform responses into the monthly token-plan window and the balance
/// detail. Both plan calls are supplementary on purpose: whatever is wrong with them —
/// transport, status, or shape — costs only the window and the plan row, never the balance.
pub fn parse(
    balance_body: &str,
    plan_detail_body: Option<&str>,
    plan_usage_body: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    parse_for_account(
        balance_body,
        plan_detail_body,
        plan_usage_body,
        captured_at,
        &AccountId::default(),
    )
}

fn parse_for_account(
    balance_body: &str,
    plan_detail_body: Option<&str>,
    plan_usage_body: Option<&str>,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let balance = parse_balance(balance_body)?;
    let detail = plan_detail_body.and_then(parse_plan_detail);
    let usage = plan_usage_body.and_then(parse_plan_usage);

    let mut windows = Vec::new();
    if let Some(usage) = usage.filter(|usage| usage.limit > 0) {
        let length = WindowLength::from_secs(MONTHLY).expect("a fixed span is not zero");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: "Monthly".to_owned(),
            subtitle: Some(format!(
                "{} / {} credits",
                number_text(usage.used as f64),
                number_text(usage.limit as f64)
            )),
            used_percent: (usage.percent * 100.0).clamp(0.0, 100.0),
            resets_at: detail.as_ref().and_then(|detail| detail.period_end),
            length: Some(length),
        });
    }

    let mut details = Vec::new();
    if let Some(plan) = detail
        .as_ref()
        .and_then(|detail| detail.plan_code.as_deref())
    {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: capitalise(plan),
            }],
        });
    }
    details.push(DetailSection {
        title: DetailSection::BALANCE.to_owned(),
        rows: vec![DetailRow {
            label: "Balance".to_owned(),
            value: balance_text(&balance),
        }],
    });

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details,
    })
}

fn parse_balance(body: &str) -> Result<Balance, ProviderError> {
    let response: BalanceResponse = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not a MiMo balance body: {error}")))?;
    if response.code == 401 || response.code == 403 {
        return Err(ProviderError::Credential {
            status: response.code as u16,
        });
    }
    if response.code != 0 {
        let reason = response
            .message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(|message| format!(": {message}"))
            .unwrap_or_default();
        return Err(ProviderError::malformed(format!(
            "MiMo balance answered code {}{reason}",
            response.code
        )));
    }
    let data = response
        .data
        .ok_or_else(|| ProviderError::malformed("MiMo balance has no data payload"))?;
    let balance = amount(&data.balance, "balance")?;
    let currency = data.currency.trim().to_owned();
    if currency.is_empty() {
        return Err(ProviderError::malformed("MiMo balance has no currency"));
    }
    Ok(Balance {
        balance,
        currency,
        cash: data.cash_balance.as_deref().and_then(optional_amount),
        gift: data.gift_balance.as_deref().and_then(optional_amount),
    })
}

fn parse_plan_detail(body: &str) -> Option<PlanDetail> {
    let response: PlanDetailResponse = serde_json::from_str(body).ok()?;
    if response.code != 0 {
        return None;
    }
    let data = response.data?;
    Some(PlanDetail {
        plan_code: data
            .plan_code
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(str::to_owned),
        period_end: data
            .current_period_end
            .as_deref()
            .and_then(platform_datetime),
    })
}

fn parse_plan_usage(body: &str) -> Option<PlanUsage> {
    let response: PlanUsageResponse = serde_json::from_str(body).ok()?;
    if response.code != 0 {
        return None;
    }
    let item = response.data?.month_usage?.items.into_iter().next()?;
    Some(PlanUsage {
        used: item.used,
        limit: item.limit,
        percent: item.percent,
    })
}

/// Parses a number the platform sends as a string, because a missing field is a broken
/// reading while a missing *optional* component is only a shorter detail row.
fn amount(value: &str, field: &str) -> Result<f64, ProviderError> {
    let parsed: f64 = value
        .trim()
        .parse()
        .map_err(|_| ProviderError::malformed(format!("MiMo {field} is not a number")))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(ProviderError::malformed(format!(
            "MiMo {field} is not a number"
        )))
    }
}

fn optional_amount(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|parsed| parsed.is_finite())
}

/// "2026-05-04 23:59:59" — the platform stamps period ends in UTC without an offset.
fn platform_datetime(value: &str) -> Option<Timestamp> {
    let format =
        format_description::parse_borrowed::<2>("[year]-[month]-[day] [hour]:[minute]:[second]")
            .ok()?;
    let parsed = PrimitiveDateTime::parse(value.trim(), &format).ok()?;
    Timestamp::from_unix(parsed.assume_utc().unix_timestamp()).ok()
}

/// `$25.51 (Paid: $20.00 / Granted: $5.51)`, dropping the components the platform has not
/// filled in or that do not parse as amounts.
fn balance_text(balance: &Balance) -> String {
    let text = money(balance.balance, &balance.currency);
    match (balance.cash, balance.gift) {
        (Some(cash), Some(gift)) => format!(
            "{} (Paid: {} / Granted: {})",
            text,
            money(cash, &balance.currency),
            money(gift, &balance.currency)
        ),
        _ => text,
    }
}

/// An amount with its currency's symbol when it has one, the code when it does not.
fn money(value: f64, currency: &str) -> String {
    match currency {
        "USD" => format!("${value:.2}"),
        "CNY" => format!("¥{value:.2}"),
        other => format!("{other} {value:.2}"),
    }
}

/// Capitalises a plan code the way the platform itself writes it ("standard" → "Standard").
fn capitalise(word: &str) -> String {
    let mut characters = word.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
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
    use super::{MiMo, parse};
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

    fn gecko_home(cookies: &[(&'static str, &'static str)]) -> crate::browser::tests::TestHome {
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
        for (name, value) in cookies {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES ('.platform.xiaomimimo.com', ?1, ?2, '/', 0, 1, 0, 0, 0)",
                    (name, value),
                )
                .expect("inserts the session");
        }
        home
    }

    /// A loopback server that answers the three MiMo routes, one connection each. The
    /// requests are sent concurrently, so their order in the channel is not asserted on.
    fn three_route_server(
        balance: (u16, &'static str),
        detail: (u16, &'static str),
        usage: (u16, &'static str),
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
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
                let lower = request.to_lowercase();
                let (status, body) = if lower.starts_with("get /api/v1/balance") {
                    balance
                } else if lower.contains("/tokenplan/detail") {
                    detail
                } else {
                    usage
                };
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> MiMo {
        MiMo::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &MiMo) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    fn monthly_key() -> WindowKey {
        WindowKey::for_length(WindowLength::from_secs(2_592_000).expect("a fixed span is not zero"))
    }

    const BALANCE: &str = include_str!("../../../tests/fixtures/mimo/balance.json");
    const PLAN_DETAIL: &str = include_str!("../../../tests/fixtures/mimo/plan-detail.json");
    const PLAN_USAGE: &str = include_str!("../../../tests/fixtures/mimo/plan-usage.json");

    #[test]
    fn the_recorded_bodies_draw_the_monthly_token_plan_and_its_balance() {
        // Deriving the percentage from used/limit would round the platform's own reading.
        let snapshot = parse(
            BALANCE,
            Some(PLAN_DETAIL),
            Some(PLAN_USAGE),
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses the recorded bodies");
        let monthly = snapshot
            .windows
            .iter()
            .find(|window| window.key == monthly_key())
            .expect("monthly window");

        assert!((monthly.used_percent - 5.05).abs() < 0.001);
        assert_eq!(
            monthly.subtitle.as_deref(),
            Some("10,100,158 / 200,000,000 credits")
        );
        assert_eq!(
            monthly.resets_at,
            Some(Timestamp::from_unix(1_777_939_199).expect("2026-05-04 23:59:59 UTC"))
        );
        assert_eq!(snapshot.details[0].title, "Plan");
        assert_eq!(snapshot.details[0].rows[0].value, "Standard");
        assert_eq!(snapshot.details[1].rows[0].label, "Balance");
        assert_eq!(
            snapshot.details[1].rows[0].value,
            "$50.00 (Paid: $30.00 / Granted: $20.00)"
        );
    }

    #[test]
    fn the_plan_calls_are_optional_and_only_the_balance_remains_without_them() {
        // Failing the whole fetch over the optional calls would hide a healthy balance.
        let snapshot = parse(
            BALANCE,
            None,
            None,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");

        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, "Balance");
    }

    #[test]
    fn unparsable_balance_components_shorten_the_detail_row_rather_than_failing_it() {
        let body = r#"{"code":0,"message":"","data":{"balance":"25.51","currency":"USD","giftBalance":"","cashBalance":"unknown"}}"#;
        let snapshot = parse(
            body,
            None,
            None,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        )
        .expect("parses the body");

        assert_eq!(snapshot.details[0].rows[0].value, "$25.51");
    }

    #[test]
    fn an_envelope_refusal_inside_a_200_names_the_expired_session() {
        // Reporting this as a broken provider would hide that the chosen browser signed out.
        for body in [
            r#"{"code":401,"message":"please login"}"#,
            r#"{"code":403,"message":"forbidden"}"#,
        ] {
            let result = parse(
                body,
                None,
                None,
                Timestamp::from_unix(1_700_000_000).expect("plausible"),
            );
            assert!(
                matches!(&result, Err(ProviderError::Credential { status }) if *status == 401 || *status == 403),
                "{body}"
            );
        }
    }

    #[test]
    fn a_balance_without_a_currency_is_malformed() {
        // Accepting it would paint an amount with no unit the reader cannot compare.
        let body = r#"{"code":0,"message":"","data":{"balance":"1","currency":"  "}}"#;
        let result = parse(
            body,
            None,
            None,
            Timestamp::from_unix(1_700_000_000).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn all_three_requests_carry_both_session_cookies() {
        // A request with only the service token is answered an error envelope, not quota.
        let home = gecko_home(&[
            ("api-platform_serviceToken", "svc-token"),
            ("userId", "123"),
        ]);
        let (base_url, requests, server) =
            three_route_server((200, BALANCE), (200, PLAN_DETAIL), (200, PLAN_USAGE));
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the balance");
        let requests: Vec<String> = (0..3)
            .map(|_| {
                requests
                    .recv()
                    .expect("request captured")
                    .to_ascii_lowercase()
            })
            .collect();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "mimo");
        let paths = ["/api/v1/balance", "/tokenplan/detail", "/tokenplan/usage"];
        for path in paths {
            assert!(
                requests.iter().any(|request| request.contains(path)),
                "{path} was not requested: {requests:?}"
            );
        }
        for request in &requests {
            assert!(
                request.contains("cookie: api-platform_servicetoken=svc-token"),
                "{request}"
            );
            assert!(request.contains("userid=123"), "{request}");
            assert!(request.contains("x-timezone: utc"), "{request}");
            assert!(request.contains("accept: application/json"), "{request}");
        }
    }

    #[test]
    fn a_jar_with_the_service_token_but_without_the_user_id_never_signed_in() {
        // Treating the half-pair as a session would ask the API for an account it cannot name.
        let home = gecko_home(&[("api-platform_serviceToken", "svc-token")]);
        let result = fetch(&provider(&home, "http://127.0.0.1:1"));

        assert!(matches!(result, Err(ProviderError::NoCredential)));
    }

    #[test]
    fn an_inspection_treats_either_half_of_the_pair_as_never_signed_in() {
        // Proving the half pair would paint the source rejected when the poll itself
        // answers that there is no credential; the pair is the gate, not the proof.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        for cookies in [
            &[("api-platform_serviceToken", "svc-token")][..],
            &[("userId", "123")][..],
        ] {
            let home = gecko_home(cookies);
            let provider = provider(&home, "http://127.0.0.1:9");

            let report = runtime.block_on(provider.inspect_sources());

            assert_eq!(
                report[0].children[0].state, "missing",
                "the half pair {cookies:?} is not a credential"
            );
        }
    }

    #[test]
    fn an_inspection_proves_a_jar_that_carries_the_whole_pair() {
        // Port 9 has nothing listening: only a proven jar can report unreachable, so the
        // state says whether the pair gate let the proof run at all.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let home = gecko_home(&[
            ("api-platform_serviceToken", "svc-token"),
            ("userId", "123"),
        ]);
        let provider = provider(&home, "http://127.0.0.1:9");

        let report = runtime.block_on(provider.inspect_sources());

        assert_eq!(report[0].children[0].state, "unreachable");
    }

    #[test]
    fn an_inspection_proof_rejects_a_session_refused_inside_a_200() {
        // A status-only proof would store an expired session as ready.
        let home = gecko_home(&[
            ("api-platform_serviceToken", "svc-token"),
            ("userId", "123"),
        ]);
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
            let body = r#"{"code":401,"message":"please login"}"#;
            write!(
                stream,
                "HTTP/1.1 200 Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("writes response");
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let verdict = runtime.block_on(
            provider(&home, &format!("http://{address}"))
                .validate_header("api-platform_serviceToken=svc-token; userId=123"),
        );
        server.join().expect("server exits");

        assert!(matches!(
            verdict,
            crate::browser::auth::Validation::Rejected
        ));
    }

    #[test]
    fn failing_plan_requests_cost_only_their_window_and_plan_row() {
        let home = gecko_home(&[
            ("api-platform_serviceToken", "svc-token"),
            ("userId", "123"),
        ]);
        let (base_url, _requests, server) =
            three_route_server((200, BALANCE), (500, "boom"), (500, "boom"));
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the balance");
        server.join().expect("server exits");

        assert!(snapshot.windows.is_empty());
        assert_eq!(snapshot.details.len(), 1);
        assert_eq!(snapshot.details[0].title, "Balance");
    }
}
