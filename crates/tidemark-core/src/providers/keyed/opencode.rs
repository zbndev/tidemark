//! OpenCode's workspace quotas, read through the browser session that signs in to
//! opencode.ai.
//!
//! The site serves its numbers through TanStack Start server functions: hashed endpoints
//! under `/_server` that answer either JSON or a seroval-seeded JavaScript assignment.
//! The three function ids pinned below — workspaces, subscription, billing — are build
//! hashes that rotate when the site is redeployed; a stale id answers as an unknown
//! route, which surfaces as ordinary breakage we accept rather than a protocol we chase.
//! Each call is spelled GET with the arguments in the query, falling back to POST with
//! them in the body, the same two spellings the site's own client uses.
//!
//! A subscribed workspace reports a five-hour and a weekly window off the subscription
//! function, in either answer dialect. A pay-as-you-go workspace has no subscription
//! object — the function answers null or fails outright — and its spend lives in the
//! billing payload instead: monthly usage and a prepaid balance, fixed-point integers
//! scaled by 1e8 against a whole-dollar monthly limit. The fallthrough from a
//! subscription-shaped failure to billing keeps both account kinds on one card; a
//! refused session or a broken network fails the fetch, because billing would fail the
//! same way.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider, length_title};
use serde_json::{Map, Value};
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
pub const PROVIDER_ID: &str = "opencode";

const SERVER_URL: &str = "https://opencode.ai/_server";
const SESSION_URL: &str = "https://opencode.ai/";
const SITE_ORIGIN: &str = "https://opencode.ai";
const COOKIE_DOMAINS: &[&str] = &["opencode.ai", "app.opencode.ai"];
const SESSION_COOKIE_NAMES: &[&str] = &["auth", "__Host-auth"];
/// The workspaces server function's build hash.
const WORKSPACES_ID: &str = "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
/// The subscription server function's build hash.
const SUBSCRIPTION_ID: &str = "7abeebee372f304e050aaaf92be863f4a86490e382f8c79db68fd94040d691b4";
/// The billing server function's build hash — the same one the Zen reader's key reaches.
const BILLING_ID: &str = "c83b78a614689c38ebee981f9b39a8b377716db85c1fd7dbab604adc02d3313d";
/// The pay-as-you-go month the billing payload meters.
const MONTHLY: u64 = 30 * 86_400;
/// The billing payload's dollar fields are fixed-point integers at this scale; the
/// monthly limit is already whole dollars.
const USD_SCALE: f64 = 100_000_000.0;

/// OpenCode as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "OpenCode",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in opencode.ai browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(OpenCode::new(options)?))
}

/// One OpenCode account, authenticated by one explicitly chosen browser profile.
pub struct OpenCode {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl OpenCode {
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
                url.trim_start_matches("https://opencode.ai")
            );
        }
        url.to_owned()
    }

    fn request(&self, call: &ServerCall, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        let mut builder = if call.post {
            self.client
                .post(self.url(SERVER_URL))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(call.args.clone().unwrap_or_else(|| "[]".to_owned()))
        } else {
            let mut builder = self
                .client
                .get(self.url(SERVER_URL))
                .query(&[("id", call.server_id)]);
            if let Some(args) = &call.args {
                builder = builder.query(&[("args", args)]);
            }
            builder
        };
        builder = builder
            .header(
                reqwest::header::ACCEPT,
                "text/javascript, application/json;q=0.9, */*;q=0.8",
            )
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, SITE_ORIGIN)
            .header(reqwest::header::REFERER, &call.referer)
            .header("X-Server-Id", call.server_id)
            .header("X-Server-Instance", instance_id());
        builder
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn server_text(&self, call: &ServerCall, cookie: &str) -> Result<String, ProviderError> {
        super::request(PROVIDER_ID, &self.client, self.request(call, cookie)?).await
    }

    /// The account's first workspace: the ids out of the workspaces function, with the
    /// POST spelling tried when the GET one answers a shell.
    async fn workspace_id(&self, cookie: &str) -> Result<String, ProviderError> {
        let text = self.server_text(&ServerCall::workspaces(), cookie).await?;
        if looks_signed_out(&text) {
            return Err(ProviderError::Credential { status: 401 });
        }
        if let Some(id) = seeded_workspace(&text).or_else(|| json_workspace(&text)) {
            return Ok(id);
        }
        let fallback = self
            .server_text(&ServerCall::workspaces_post(), cookie)
            .await?;
        if looks_signed_out(&fallback) {
            return Err(ProviderError::Credential { status: 401 });
        }
        seeded_workspace(&fallback)
            .or_else(|| json_workspace(&fallback))
            .ok_or_else(|| ProviderError::malformed("the workspaces answer named no workspace"))
    }

    /// The subscription's two windows, or the failure that says this workspace bills
    /// another way.
    async fn subscription(
        &self,
        workspace: &str,
        cookie: &str,
        now: Timestamp,
    ) -> Result<Snapshot, ProviderError> {
        let text = self
            .server_text(&ServerCall::subscription(workspace), cookie)
            .await?;
        if looks_signed_out(&text) {
            return Err(ProviderError::Credential { status: 401 });
        }
        // A null answer is a workspace without a subscription: the POST spelling answers
        // those with a 500, so it is not worth trying.
        if !is_null_payload(&text) {
            if let Some(snapshot) = parse_subscription(&text, now) {
                return Ok(snapshot);
            }
            // The GET spelling sometimes answers a shell; the POST carries the data.
            let fallback = self
                .server_text(&ServerCall::subscription_post(workspace), cookie)
                .await?;
            if looks_signed_out(&fallback) {
                return Err(ProviderError::Credential { status: 401 });
            }
            if !is_null_payload(&fallback)
                && let Some(snapshot) = parse_subscription(&fallback, now)
            {
                return Ok(snapshot);
            }
        }
        Err(ProviderError::malformed(
            "the subscription answer carried no usage windows",
        ))
    }

    /// The billing payload's month, or `None` when it does not pay-as-you-go this
    /// workspace. A refused session is propagated: it would refuse every other call too.
    async fn payg(
        &self,
        workspace: &str,
        cookie: &str,
        now: Timestamp,
    ) -> Result<Option<Snapshot>, ProviderError> {
        let text = self
            .server_text(&ServerCall::billing(workspace), cookie)
            .await?;
        if looks_signed_out(&text) {
            return Err(ProviderError::Credential { status: 401 });
        }
        let Some(billing) = parse_billing(&text) else {
            return Ok(None);
        };
        if billing.has_subscription {
            // Still subscribed: the subscription failure stands, and its windows are the
            // honest reading.
            return Ok(None);
        }
        Ok(Some(payg_snapshot(billing, now)))
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
        let now = Timestamp::now();
        let workspace = self.workspace_id(&session.header).await?;
        match self.subscription(&workspace, &session.header, now).await {
            Ok(snapshot) => Ok(snapshot),
            Err(error) if can_fall_back(&error) => {
                match self.payg(&workspace, &session.header, now).await {
                    Ok(Some(snapshot)) => Ok(snapshot),
                    Ok(None) => Err(error),
                    Err(refused @ ProviderError::Credential { .. }) => Err(refused),
                    Err(_) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.request(&ServerCall::workspaces(), header) else {
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
            SERVER_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for OpenCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCode")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for OpenCode {
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

/// One server-function call: the hashed id it hangs from, its arguments as the JSON text
/// the query or the body carries, and the page the site believes it is serving.
struct ServerCall {
    server_id: &'static str,
    args: Option<String>,
    post: bool,
    referer: String,
}

impl ServerCall {
    fn workspaces() -> Self {
        Self {
            server_id: WORKSPACES_ID,
            args: None,
            post: false,
            referer: format!("{SITE_ORIGIN}/"),
        }
    }

    fn workspaces_post() -> Self {
        Self {
            server_id: WORKSPACES_ID,
            args: Some("[]".to_owned()),
            post: true,
            referer: format!("{SITE_ORIGIN}/"),
        }
    }

    fn subscription(workspace: &str) -> Self {
        Self {
            server_id: SUBSCRIPTION_ID,
            args: Some(format!("[\"{workspace}\"]")),
            post: false,
            referer: format!("{SITE_ORIGIN}/workspace/{workspace}/billing"),
        }
    }

    fn subscription_post(workspace: &str) -> Self {
        Self {
            post: true,
            ..Self::subscription(workspace)
        }
    }

    fn billing(workspace: &str) -> Self {
        Self {
            server_id: BILLING_ID,
            args: Some(format!("[\"{workspace}\"]")),
            post: false,
            referer: format!("{SITE_ORIGIN}/workspace/{workspace}"),
        }
    }
}

/// A throwaway instance label for the server-function headers: uuid-shaped, from the
/// clock, the process and a per-process counter. The site only ties a call's parts
/// together with it; nothing needs it to be a real random uuid.
fn instance_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is set")
        .as_nanos() as u64;
    let serial = CALLS.fetch_add(1, Ordering::Relaxed);
    let mut state = nanos ^ u64::from(std::process::id()) ^ serial.rotate_left(32);
    let mut hex = [b'0'; 32];
    for digit in &mut hex {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *digit = b"0123456789abcdef"[usize::try_from(state & 0xF).expect("a nibble fits")];
    }
    hex[12] = b'4';
    hex[16] = b"89ab"[usize::try_from(state & 3).expect("a nibble fits")];
    let text = std::str::from_utf8(&hex).expect("hex digits are ascii");
    format!(
        "server-fn:{}-{}-{}-{}-{}",
        &text[..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..]
    )
}

/// Whether a subscription failure is worth answering from the billing payload: only a
/// subscription-shaped one — a refused session or a broken network would fail the same
/// way there.
fn can_fall_back(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Malformed(_) | ProviderError::Http { .. }
    )
}

/// The body the site serves when it wants the visitor to sign in again.
fn looks_signed_out(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "login",
        "sign in",
        "auth/authorize",
        "not associated with an account",
        "actor of type \"public\"",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// A server function that resolved to null: the literal, or the seeded assignment the
/// dialect answers with — `…["server-fn:<uuid>"]=[],null)`.
fn is_null_payload(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return true;
    }
    // …["server-fn:<uuid>"]=[],null) — read from the right: `)`, `null`, `,`, the `]`
    // and `[` of the emptied list, the seeding `=`, and the key's own `]`.
    let Some(after_close) = trimmed.strip_suffix(')') else {
        return false;
    };
    let Some(after_null) = after_close.strip_suffix("null") else {
        return false;
    };
    let Some(after_comma) = after_null.strip_suffix(',') else {
        return false;
    };
    let Some(after_list) = after_comma.strip_suffix(']') else {
        return false;
    };
    let Some(after_open) = after_list.strip_suffix('[') else {
        return false;
    };
    let Some(after_assign) = after_open.strip_suffix('=') else {
        return false;
    };
    after_assign.trim_end().ends_with(']')
}

/// The first workspace id out of the seeded script: `id:"wrk_…"`.
fn seeded_workspace(text: &str) -> Option<String> {
    let mut search = 0;
    while let Some(found) = text[search..].find("id") {
        let at = search + found;
        search = at + 2;
        let rest = text[at + 2..].trim_start();
        let Some(value) = rest.strip_prefix(':').map(str::trim_start) else {
            continue;
        };
        let Some(value) = value.strip_prefix("\"wrk_") else {
            continue;
        };
        let Some(end) = value.find('"') else {
            continue;
        };
        if end > 0 {
            return Some(format!("wrk_{}", &value[..end]));
        }
    }
    None
}

/// The first workspace id out of a JSON answer, wherever the id sits in it.
fn json_workspace(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    first_wrk(&value)
}

fn first_wrk(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if text.starts_with("wrk_") => Some(text.clone()),
        Value::Array(items) => items.iter().find_map(first_wrk),
        Value::Object(map) => map.values().find_map(first_wrk),
        _ => None,
    }
}

/// One number out of a seeded script's window object: the `field` inside the object that
/// opens after `label`, before its closing brace.
fn seeded_number(text: &str, label: &str, field: &str) -> Option<f64> {
    let after = text.split_once(label)?.1;
    let window = &after[..after.find('}')?];
    let rest = &window[window.find(field)? + field.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The subscription answer as a snapshot: JSON first — the same shapes and rules the
/// Zen reader already pins — then the seeded script's numbers. `None` when neither
/// dialect carries the two windows.
fn parse_subscription(text: &str, now: Timestamp) -> Option<Snapshot> {
    if let Ok(mut snapshot) = super::opencodego::parse(text, now) {
        snapshot.provider = ProviderId::new(PROVIDER_ID);
        return Some(snapshot);
    }
    let rolling = seeded_number(text, "rollingUsage", "usagePercent")?;
    let weekly = seeded_number(text, "weeklyUsage", "usagePercent")?;
    let windows = vec![
        window(
            rolling,
            seeded_number(text, "rollingUsage", "resetInSec"),
            5 * 3_600,
            now,
        ),
        window(
            weekly,
            seeded_number(text, "weeklyUsage", "resetInSec"),
            7 * 86_400,
            now,
        ),
    ];
    Some(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows,
        details: Vec::new(),
    })
}

/// One quota window, percent clamped to the bar and a reset counted down from the poll.
fn window(used_percent: f64, reset_in: Option<f64>, seconds: u64, now: Timestamp) -> Window {
    let length = WindowLength::from_secs(seconds).expect("a fixed span is not zero");
    Window {
        key: WindowKey::for_length(length),
        title: length_title(length),
        subtitle: None,
        used_percent: used_percent.clamp(0.0, 100.0),
        resets_at: reset_in.map(|countdown| now.saturating_add_seconds(countdown.max(0.0) as i64)),
        length: Some(length),
    }
}

/// The billing payload's month: spend, limit and prepaid balance in USD.
#[derive(Debug)]
struct Billing {
    monthly_usage: f64,
    monthly_limit: Option<f64>,
    balance: Option<f64>,
    has_subscription: bool,
}

/// The billing answer in either dialect: JSON first, then the seeded script. `None`
/// when the answer is not a customer payload at all.
fn parse_billing(text: &str) -> Option<Billing> {
    json_billing(text).or_else(|| seeded_billing(text))
}

fn json_billing(text: &str) -> Option<Billing> {
    let value: Value = serde_json::from_str(text).ok()?;
    let customer = customer_in(&value)?;
    let raw_usage = customer.get("monthlyUsage").and_then(json_number)?;
    Some(Billing {
        monthly_usage: raw_usage / USD_SCALE,
        monthly_limit: customer.get("monthlyLimit").and_then(json_number),
        balance: customer
            .get("balance")
            .and_then(json_number)
            .map(|balance| balance / USD_SCALE),
        has_subscription: customer
            .get("subscription")
            .is_some_and(|subscription| !subscription.is_null()),
    })
}

/// The object that is a customer: a non-empty `customerID`, wherever it sits.
fn customer_in(value: &Value) -> Option<&Map<String, Value>> {
    match value {
        Value::Object(map) => {
            if map
                .get("customerID")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
            {
                return Some(map);
            }
            map.values().find_map(customer_in)
        }
        Value::Array(items) => items.iter().find_map(customer_in),
        _ => None,
    }
}

fn json_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|number| number.is_finite())
}

fn seeded_billing(text: &str) -> Option<Billing> {
    let customer = field_value(text, "customerID")?.trim().trim_matches('"');
    if customer.is_empty() {
        return None;
    }
    let raw_usage = field_value(text, "monthlyUsage")?.parse::<f64>().ok()?;
    Some(Billing {
        monthly_usage: raw_usage / USD_SCALE,
        monthly_limit: field_value(text, "monthlyLimit").and_then(|v| v.parse::<f64>().ok()),
        balance: field_value(text, "balance")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|balance| balance / USD_SCALE),
        has_subscription: matches!(field_value(text, "subscription"), Some(value) if value != "null"),
    })
}

/// The raw value of one field in the seeded script or its JSON twin — `"field":value` or
/// `field:value`, a `$R[n]=` seeding prefix skipped — up to the separator that ends it.
fn field_value<'a>(text: &'a str, field: &str) -> Option<&'a str> {
    for spelling in [format!("\"{field}\":"), format!("{field}:")] {
        if let Some(found) = text.find(&spelling) {
            let value = &text[found + spelling.len()..];
            let value = value.trim_start();
            // The seeded script writes `$R[n]=` before a shared value.
            let value = match value.strip_prefix("$R[") {
                Some(seeded)
                    if seeded
                        .find(']')
                        .is_some_and(|close| seeded[close + 1..].trim_start().starts_with('=')) =>
                {
                    let close = seeded.find(']').expect("just checked");
                    seeded[close + 1..].trim_start().trim_start_matches('=')
                }
                _ => value,
            };
            let end = value.find([',', '}', ')']).unwrap_or(value.len());
            return Some(value[..end].trim());
        }
    }
    None
}

/// The pay-as-you-go month as a snapshot: a window when a limit is configured, and the
/// balance rows the payload carries.
fn payg_snapshot(billing: Billing, captured_at: Timestamp) -> Snapshot {
    let mut windows = Vec::new();
    if let Some(limit) = billing.monthly_limit.filter(|limit| *limit > 0.0) {
        windows.push(window(
            billing.monthly_usage / limit * 100.0,
            None,
            MONTHLY,
            captured_at,
        ));
    }
    let mut rows = vec![DetailRow {
        label: "Spend this month".to_owned(),
        value: format!("${:.2}", billing.monthly_usage),
    }];
    if let Some(limit) = billing.monthly_limit {
        rows.push(DetailRow {
            label: "Monthly limit".to_owned(),
            value: format!("${limit:.2}"),
        });
    }
    if let Some(balance) = billing.balance {
        rows.push(DetailRow {
            label: "Balance".to_owned(),
            value: format!("${balance:.2}"),
        });
    }
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenCode, is_null_payload, parse_billing, parse_subscription, seeded_workspace};
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
                ) VALUES ('.opencode.ai', ?1, ?2, '/', 0, 1, 0, 0, 0)",
                ("auth", "session-value"),
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> OpenCode {
        OpenCode::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &OpenCode) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const WORKSPACES: &str = include_str!("../../../tests/fixtures/opencode/workspaces.txt");
    const SUBSCRIPTION: &str = include_str!("../../../tests/fixtures/opencode/subscription.txt");
    const BILLING: &str =
        include_str!("../../../tests/fixtures/opencode/billing-pay-as-you-go.txt");
    /// The request lines the three server functions open with, keyed by the id prefixes
    /// that make each one unique.
    const GET_WORKSPACES: &str = "GET /_server?id=def39";
    const GET_SUBSCRIPTION: &str = "GET /_server?id=7abee";
    const GET_BILLING: &str = "GET /_server?id=c83b7";
    const WORKSPACE: &str = "wrk_01K6AR1ZET89H8NB691FQ2C2VB";

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_recorded_workspaces_answer_names_its_workspace() {
        assert_eq!(
            seeded_workspace(WORKSPACES).as_deref(),
            Some("wrk_01K6AR1ZET89H8NB691FQ2C2VB")
        );
    }

    #[test]
    fn the_recorded_subscription_answer_draws_both_windows() {
        let now = at(1_800_000_000);
        let snapshot = parse_subscription(SUBSCRIPTION, now).expect("parses the script");

        assert_eq!(snapshot.provider.as_str(), "opencode");
        let rolling = &snapshot.windows[0];
        assert_eq!(rolling.title, "5 hours");
        assert_eq!(
            rolling.key,
            WindowKey::for_length(WindowLength::from_secs(18_000).expect("a span is not zero"))
        );
        assert_eq!(rolling.used_percent, 17.0);
        assert_eq!(rolling.resets_at, Some(now.saturating_add_seconds(5_944)));
        let weekly = &snapshot.windows[1];
        assert_eq!(weekly.title, "7 days");
        assert_eq!(weekly.used_percent, 75.0);
        assert_eq!(weekly.resets_at, Some(now.saturating_add_seconds(278_201)));
    }

    #[test]
    fn the_recorded_billing_answer_scales_its_dollars() {
        let billing = parse_billing(BILLING).expect("parses the customer payload");

        assert_eq!(billing.monthly_usage, 15.0);
        assert_eq!(billing.monthly_limit, Some(20.0));
        assert_eq!(billing.balance, Some(12.5));
        assert!(
            !billing.has_subscription,
            "subscription:null is not a subscription"
        );
    }

    #[test]
    fn a_seeded_null_and_a_plain_null_are_both_null_payloads() {
        assert!(is_null_payload(
            ";0x1;((self.$R=self.$R||{})[\"server-fn:00000000-0000-4000-8000-000000000000\"]=[],null)"
        ));
        assert!(is_null_payload(" null "));
        assert!(!is_null_payload(SUBSCRIPTION));
    }

    #[test]
    fn the_chain_walks_workspaces_then_subscription_over_get() {
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[
            (GET_WORKSPACES, 200, WORKSPACES),
            (GET_SUBSCRIPTION, 200, SUBSCRIPTION),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        let first = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        let second = requests.recv().expect("request captured");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 2);
        assert!(
            first.starts_with("get /_server?id=def3997"),
            "{first}: the workspaces call is not the GET spelling"
        );
        assert!(first.contains("cookie: auth=session-value"), "{first}");
        assert!(first.contains("x-server-id"), "{first}");
        assert!(
            second.contains(&format!("args=%5B%22{WORKSPACE}%22%5D")),
            "{second}"
        );
    }

    #[test]
    fn an_answerless_get_falls_back_to_the_post_spelling() {
        let home = gecko_home();
        let (base_url, requests, server) = chained_server(&[
            (GET_WORKSPACES, 200, "[]"),
            ("POST /_server", 200, WORKSPACES),
            (GET_SUBSCRIPTION, 200, SUBSCRIPTION),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the usage");
        let _ = requests.recv().expect("the workspaces GET");
        let post = requests
            .recv()
            .expect("the workspaces POST")
            .to_ascii_lowercase();
        let _ = requests.recv().expect("the subscription GET");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 2);
        // The POST spelling carries the id in a header and the arguments in the body.
        assert!(post.contains("x-server-id"), "{post}");
        assert!(post.contains("content-type: application/json"), "{post}");
    }

    #[test]
    fn a_null_subscription_falls_through_to_the_billing_month() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            (GET_WORKSPACES, 200, WORKSPACES),
            (
                GET_SUBSCRIPTION,
                200,
                ";0x1;((self.$R=self.$R||{})[\"server-fn:00000000-0000-4000-8000-000000000000\"]=[],null)",
            ),
            (GET_BILLING, 200, BILLING),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the billing month");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 1);
        let monthly = &snapshot.windows[0];
        assert_eq!(monthly.title, "30 days");
        assert_eq!(monthly.used_percent, 75.0);
        assert_eq!(snapshot.details[0].title, "Balance");
        assert_eq!(snapshot.details[0].rows[0].value, "$15.00");
        assert_eq!(snapshot.details[0].rows[2].value, "$12.50");
    }

    #[test]
    fn a_failing_subscription_falls_through_to_the_billing_month() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[
            (GET_WORKSPACES, 200, WORKSPACES),
            (GET_SUBSCRIPTION, 500, "boom"),
            (GET_BILLING, 200, BILLING),
        ]);
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the billing month");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 75.0);
    }

    #[test]
    fn a_refused_session_is_not_answered_from_billing() {
        let home = gecko_home();
        let (base_url, _requests, server) = chained_server(&[(GET_WORKSPACES, 401, "")]);
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }
}
