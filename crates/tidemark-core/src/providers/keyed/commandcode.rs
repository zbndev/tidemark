//! CommandCode rate windows, read from the browser session that signs in to its dashboard.
//!
//! The credits call carries the 5-hour and weekly windows and the monthly credit balance;
//! the subscriptions call adds the plan and the billing-cycle reset, so when it fails the
//! windows are still drawn without them. The monthly grant's total is not on the wire —
//! the credits endpoint reports only what remains — so it comes from the public pricing
//! catalogue below, keyed by the subscription's `planId`. A browser session is selected
//! explicitly, never substituted from another profile.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey, WindowLength,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "commandcode";

const CREDITS_URL: &str = "https://api.commandcode.ai/internal/billing/credits";
const SUBSCRIPTIONS_URL: &str = "https://api.commandcode.ai/internal/billing/subscriptions";
const SESSION_URL: &str = "https://api.commandcode.ai/";
const ORIGIN: &str = "https://commandcode.ai";
const REFERER: &str = "https://commandcode.ai/";
/// The cookie names CommandCode's better-auth backend has carried its session in.
const SESSION_COOKIE_NAMES: &[&str] = &[
    "__Secure-commandcode_prod_.session_token",
    "commandcode_prod_.session_token",
    "__Host-commandcode_prod_.session_token",
    "__Host-better-auth.session_token",
    "__Secure-better-auth.session_token",
    "better-auth.session_token",
];
const COOKIE_DOMAINS: &[&str] = &["commandcode.ai", "www.commandcode.ai"];
/// The monthly credit allowance (USD) each plan publishes on its pricing page, keyed by
/// the `planId` the subscriptions endpoint answers with.
static PLANS: &[(&str, &str, f64)] = &[
    ("individual-go", "Go", 10.0),
    ("individual-goat", "GOAT", 70.0),
    ("individual-pro", "Pro", 30.0),
    ("individual-pro-v1", "Pro", 80.0),
    ("individual-max", "Max", 150.0),
    ("individual-ultra", "Ultra", 300.0),
];

/// CommandCode as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "CommandCode",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in commandcode.ai browser session.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    _credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(CommandCode::new_for_account(account, options)?))
}

/// One CommandCode account, authenticated by one explicitly chosen browser profile.
pub struct CommandCode {
    tidemark_account: AccountId,
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl CommandCode {
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

    fn credits_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/internal/billing/credits");
        }
        CREDITS_URL.to_owned()
    }

    fn subscriptions_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/internal/billing/subscriptions");
        }
        SUBSCRIPTIONS_URL.to_owned()
    }

    fn request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::REFERER, REFERER)
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
        let (credits, subscriptions) = tokio::join!(
            super::request(
                PROVIDER_ID,
                &self.client,
                self.request(&self.credits_url(), &session.header)?,
            ),
            super::request(
                PROVIDER_ID,
                &self.client,
                self.request(&self.subscriptions_url(), &session.header)?,
            ),
        );
        parse_for_account(
            &credits?,
            subscriptions.ok().as_deref(),
            Timestamp::now(),
            &self.tidemark_account,
        )
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.request(CREDITS_URL, header) else {
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
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            CREDITS_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for CommandCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandCode")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for CommandCode {
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

/// One rolling window limit: a cap that must be positive to count, usage that defaults
/// to zero, and a reset that may be epoch milliseconds, epoch seconds, or an ISO 8601
/// string — the wire has been observed carrying all three.
struct Limit {
    cap: f64,
    used: f64,
    reset: Option<Timestamp>,
}

struct Subscription {
    plan_id: String,
    status: String,
    period_end: Option<Timestamp>,
}

/// Turns CommandCode's credits and subscriptions responses into its rate windows.
///
/// `windowLimits` sits at the response root or inside `credits` — both spellings parse,
/// the root one winning when both are present, exactly as upstream accepts them. The
/// subscriptions body is supplementary: whatever is wrong with it costs only the plan row
/// and the monthly window's total, never the rolling windows.
pub fn parse(
    credits_body: &str,
    subscriptions_body: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    parse_for_account(
        credits_body,
        subscriptions_body,
        captured_at,
        &AccountId::default(),
    )
}

fn parse_for_account(
    credits_body: &str,
    subscriptions_body: Option<&str>,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let root: Value = serde_json::from_str(credits_body)
        .map_err(|error| ProviderError::malformed(format!("not CommandCode credits: {error}")))?;
    let credits = root
        .get("credits")
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::malformed("CommandCode credits has no credits object"))?;
    let monthly = number(credits.get("monthlyCredits"))
        .ok_or_else(|| ProviderError::malformed("CommandCode monthlyCredits is missing"))?;
    let purchased = number(credits.get("purchasedCredits")).unwrap_or(0.0);
    let limits = root
        .get("windowLimits")
        .and_then(Value::as_object)
        .or_else(|| credits.get("windowLimits").and_then(Value::as_object));
    let five_hour = limit(
        limits.and_then(|limits| limits.get("fiveHour")),
        "five-hour",
    )?;
    let weekly = limit(limits.and_then(|limits| limits.get("weekly")), "weekly")?;

    let subscription = subscriptions_body
        .and_then(|body| subscription(body).ok())
        .flatten();
    let plan = subscription
        .as_ref()
        .and_then(|subscription| catalog_plan(&subscription.plan_id));

    let mut windows = Vec::new();
    for (limit, secs, title) in [
        (five_hour, 5 * 3_600, "5 hours"),
        (weekly, 7 * 86_400, "Weekly"),
    ] {
        let Some(limit) = limit else {
            continue;
        };
        let length = WindowLength::from_secs(secs).expect("a fixed span is not zero");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: title.to_owned(),
            subtitle: Some(format!(
                "{} / {} credits",
                credit_text(limit.used),
                credit_text(limit.cap)
            )),
            used_percent: (limit.used / limit.cap * 100.0).clamp(0.0, 100.0),
            resets_at: limit.reset,
            length: Some(length),
        });
    }
    // The monthly grant has a total only through the pricing catalogue: without a plan
    // there is no limit to draw a percentage against, and none is invented.
    if let Some(total) = plan.map(|(_, _, total)| total).filter(|total| *total > 0.0) {
        let used = (total - monthly).clamp(0.0, total);
        let length = WindowLength::from_secs(2_592_000).expect("a fixed span is not zero");
        windows.push(Window {
            key: WindowKey::for_length(length),
            title: "Monthly".to_owned(),
            subtitle: Some(format!(
                "{} / {} credits",
                credit_text(used),
                credit_text(total)
            )),
            used_percent: (used / total * 100.0).clamp(0.0, 100.0),
            resets_at: subscription.as_ref().and_then(|sub| sub.period_end),
            length: Some(length),
        });
    }

    let mut details = Vec::new();
    if let Some(subscription) = &subscription {
        let name = plan
            .map(|(_, name, _)| name.to_owned())
            .unwrap_or_else(|| subscription.plan_id.clone());
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![
                DetailRow {
                    label: "Plan".to_owned(),
                    value: name,
                },
                DetailRow {
                    label: "Status".to_owned(),
                    value: subscription.status.clone(),
                },
            ],
        });
    }
    let mut rows = vec![DetailRow {
        label: "Monthly credits".to_owned(),
        value: format!("${:.2}", monthly),
    }];
    if purchased > 0.0 {
        rows.push(DetailRow {
            label: "Purchased credits".to_owned(),
            value: format!("${purchased:.2}"),
        });
    }
    details.push(DetailSection {
        title: DetailSection::BALANCE.to_owned(),
        rows,
    });

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows,
        details,
    })
}

/// The catalogue entry for a `planId`, matched case-insensitively.
fn catalog_plan(plan_id: &str) -> Option<(&'static str, &'static str, f64)> {
    PLANS
        .iter()
        .find(|(id, _, _)| id.eq_ignore_ascii_case(plan_id))
        .map(|(id, name, total)| (*id, *name, *total))
}

/// Reads a window limit object. An absent window is simply not offered; a present one —
/// even a `null`, a string, or an array — is a recognised window, and one whose shape,
/// cap, or usage cannot be read fails the fetch rather than hiding or drawing at zero;
/// the reset stays optional, because "no reset named" is a state the wire genuinely
/// carries.
fn limit(value: Option<&Value>, title: &str) -> Result<Option<Limit>, ProviderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Err(ProviderError::malformed(format!(
            "CommandCode {title} window is not an object"
        )));
    };
    let cap = number(object.get("cap"))
        .filter(|cap| *cap > 0.0)
        .ok_or_else(|| {
            ProviderError::malformed(format!("CommandCode {title} window has no readable cap"))
        })?;
    let used = number(object.get("used")).ok_or_else(|| {
        ProviderError::malformed(format!("CommandCode {title} window has no readable usage"))
    })?;
    Ok(Some(Limit {
        cap,
        used,
        reset: object.get("resetAt").and_then(reset_time),
    }))
}

/// Reads a subscriptions body: `success` with explicit `data`, `null` meaning the free
/// tier, and a `planId`-carrying object otherwise.
fn subscription(body: &str) -> Result<Option<Subscription>, ProviderError> {
    let root: Value = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not CommandCode subscriptions: {error}"))
    })?;
    let object = root.as_object().ok_or_else(|| {
        ProviderError::malformed("CommandCode subscriptions root is not an object")
    })?;
    if object.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(ProviderError::malformed(
            "CommandCode subscriptions response is not successful",
        ));
    }
    let Some(data) = object.get("data") else {
        return Err(ProviderError::malformed(
            "CommandCode subscriptions response has no data",
        ));
    };
    if data.is_null() {
        return Ok(None);
    }
    let data = data.as_object().ok_or_else(|| {
        ProviderError::malformed("CommandCode subscriptions data is not an object")
    })?;
    let plan_id = data
        .get("planId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan_id| !plan_id.is_empty())
        .ok_or_else(|| ProviderError::malformed("CommandCode subscriptions data has no planId"))?;
    Ok(Some(Subscription {
        plan_id: plan_id.to_owned(),
        status: data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        period_end: data.get("currentPeriodEnd").and_then(reset_time),
    }))
}

/// Reads a number the way CommandCode's wire has been observed carrying one: as a JSON
/// number, or as a numeric string.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

/// Reads a reset instant: epoch milliseconds, epoch seconds, or an ISO 8601 string.
fn reset_time(value: &Value) -> Option<Timestamp> {
    if let Some(number) = number(Some(value)).filter(|number| *number > 0.0) {
        let seconds = if number > 10_000_000_000.0 {
            number / 1000.0
        } else {
            number
        };
        return Timestamp::from_unix(seconds as i64).ok();
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let parsed = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    Timestamp::from_unix(parsed.unix_timestamp()).ok()
}

/// Formats a credit amount with at most two decimals and no trailing zeros: `0.75`,
/// `1.5`, `3`.
fn credit_text(value: f64) -> String {
    let rounded = format!("{value:.2}");
    let trimmed = rounded.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{CommandCode, parse};
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
                ) VALUES ('.commandcode.ai', 'better-auth.session_token', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the two CommandCode routes, one connection each.
    /// The requests are sent concurrently, so their order in the channel is not asserted on.
    fn two_route_server(
        credits: (u16, &'static str),
        subscriptions: (u16, &'static str),
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
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("reads request line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    request.push_str(&line);
                }
                drop(reader);
                let is_credits = request.starts_with("GET /internal/billing/credits");
                request_tx.send(request).expect("sends request");
                let (status, body) = if is_credits { credits } else { subscriptions };
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> CommandCode {
        CommandCode::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &CommandCode) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const ROOT: &str = include_str!("../../../tests/fixtures/commandcode/window-limits-root.json");
    const NESTED: &str =
        include_str!("../../../tests/fixtures/commandcode/window-limits-nested.json");
    const SUBSCRIPTIONS: &str =
        include_str!("../../../tests/fixtures/commandcode/subscriptions.json");

    fn key_of(secs: u64) -> WindowKey {
        WindowKey::for_length(WindowLength::from_secs(secs).expect("a fixed span"))
    }

    #[test]
    fn window_limits_at_the_response_root_draw_the_rolling_windows() {
        // Keying the windows by their slot names instead of their lengths would split one
        // continuous window in two when the wire moves them between spellings.
        let snapshot = parse(
            ROOT,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");
        let five_hour = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(5 * 3_600))
            .expect("5-hour window");
        let weekly = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(7 * 86_400))
            .expect("weekly window");

        assert!((five_hour.used_percent - 25.0).abs() < 0.01);
        assert_eq!(
            five_hour.resets_at,
            Some(Timestamp::from_unix(1_780_000_000).expect("plausible"))
        );
        assert_eq!(five_hour.subtitle.as_deref(), Some("0.75 / 3 credits"));
        assert!((weekly.used_percent - 10.0).abs() < 0.01);
        assert_eq!(
            weekly.resets_at,
            Some(Timestamp::from_unix(1_780_100_000).expect("plausible"))
        );
        // Without a subscription there is no plan total to draw a monthly window against.
        assert!(
            snapshot
                .windows
                .iter()
                .all(|window| window.key != key_of(2_592_000))
        );
    }

    #[test]
    fn window_limits_nested_inside_credits_draw_the_same_windows() {
        // The nested spelling also carries its numbers as strings and its reset as epoch
        // seconds; both must parse identically to the root spelling.
        let snapshot = parse(
            NESTED,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");
        let five_hour = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(5 * 3_600))
            .expect("5-hour window");
        let weekly = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(7 * 86_400))
            .expect("weekly window");

        assert!((five_hour.used_percent - 25.0).abs() < 0.01);
        assert_eq!(
            five_hour.resets_at,
            Some(Timestamp::from_unix(1_780_200_000).expect("plausible"))
        );
        assert!((weekly.used_percent - 20.0).abs() < 0.01);
    }

    #[test]
    fn an_active_subscription_catalogues_the_monthly_grant() {
        // The grant's total lives on the pricing page, not the wire; without the catalogue
        // the monthly window would have no limit to draw against.
        let snapshot = parse(
            ROOT,
            Some(SUBSCRIPTIONS),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded bodies");
        let monthly = snapshot
            .windows
            .iter()
            .find(|window| window.key == key_of(2_592_000))
            .expect("monthly window");

        assert!((monthly.used_percent - 15.0).abs() < 0.01);
        assert_eq!(
            monthly.resets_at,
            Some(Timestamp::from_unix(1_780_730_930).expect("plausible"))
        );
        assert_eq!(
            snapshot.details[0].rows[0].value, "Go",
            "the catalogue spells the plan name"
        );
        assert_eq!(snapshot.details[0].rows[1].value, "active");
        assert_eq!(snapshot.details[1].rows[0].value, "$8.50");
    }

    #[test]
    fn a_null_subscription_data_is_the_free_tier_and_draws_no_monthly_window() {
        let snapshot = parse(
            ROOT,
            Some(r#"{"success":true,"data":null}"#),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the free-tier body");

        assert!(
            snapshot
                .windows
                .iter()
                .all(|window| window.key != key_of(2_592_000))
        );
        assert_eq!(
            snapshot.details[0].title,
            tidemark_types::DetailSection::BALANCE
        );
    }

    #[test]
    fn a_present_window_without_a_cap_or_usage_fails_rather_than_hiding_or_zeroing() {
        // Skipping a recognised window would hide it; defaulting its usage to zero would
        // paint headroom the wire never stated.
        for body in [
            r#"{"credits":{"monthlyCredits":10,"windowLimits":{"fiveHour":{}}}}"#,
            r#"{"credits":{"monthlyCredits":10,"windowLimits":{"fiveHour":{"cap":3}}}}"#,
            r#"{"credits":{"monthlyCredits":10,"windowLimits":{"weekly":{"used":0.7}}}}"#,
            r#"{"credits":{"monthlyCredits":10,"windowLimits":{"fiveHour":null}}}"#,
            r#"{"credits":{"monthlyCredits":10,"windowLimits":{"fiveHour":"soon"}}}"#,
            r#"{"credits":{"monthlyCredits":10,"windowLimits":{"weekly":[]}}}"#,
        ] {
            let result = parse(
                body,
                None,
                Timestamp::from_unix(1_740_000_000).expect("plausible"),
            );
            assert!(
                matches!(result, Err(ProviderError::Malformed(_))),
                "{body} must fail"
            );
        }
    }

    #[test]
    fn a_credits_body_without_monthly_credits_is_malformed() {
        // Accepting a credits-less body would paint a made-up empty quota.
        let result = parse(
            r#"{"credits":{"purchasedCredits":0}}"#,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn both_requests_carry_the_chosen_browsers_session_cookie() {
        // Either request without the session cookie answers 401 instead of quota.
        let home = gecko_home();
        let (base_url, requests, server) = two_route_server((200, ROOT), (200, SUBSCRIPTIONS));
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the credits");
        let requests: Vec<String> = (0..2)
            .map(|_| {
                requests
                    .recv()
                    .expect("request captured")
                    .to_ascii_lowercase()
            })
            .collect();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "commandcode");
        let credits = requests
            .iter()
            .find(|request| request.starts_with("get /internal/billing/credits"))
            .expect("credits request");
        let subscriptions = requests
            .iter()
            .find(|request| request.starts_with("get /internal/billing/subscriptions"))
            .expect("subscriptions request");
        for request in [credits.as_str(), subscriptions.as_str()] {
            assert!(
                request.contains("cookie: better-auth.session_token=chosen-session"),
                "{request}"
            );
            assert!(request.contains("accept: application/json"), "{request}");
            assert!(
                request.contains("origin: https://commandcode.ai"),
                "{request}"
            );
            assert!(
                request.contains("referer: https://commandcode.ai/"),
                "{request}"
            );
        }
    }

    #[test]
    fn an_unauthorised_credits_response_asks_for_a_new_browser_session() {
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
}
