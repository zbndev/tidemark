//! Cursor plan usage, read from cursor.com with the session cookie a browser holds.
//!
//! # Why the credential is a browser session
//!
//! Cursor publishes no usage API and issues no API key for one. Every reader of this data —
//! the dashboard, and CodexBar, which this port follows — authenticates with the
//! `WorkosCursorSessionToken` cookie a browser session holds. CodexBar on Linux makes the
//! user copy that header out of the browser's network panel by hand; Tidemark instead reads
//! the cookie itself, out of every browser on the machine, on every poll — which is also
//! what keeps the reading alive when the session rolls over. The reading machinery is
//! [`crate::browser`]; this module only names the domains and the cookie names.
//!
//! Which cookies count, and in what order they are tried, is CodexBar's ladder: a strict
//! pass that requires one of the known session-cookie names to be present, then a fallback
//! pass that takes any cookies on Cursor's domains at all, because the session name has
//! changed before and the API is the arbiter of what still works.
//!
//! # The four requests
//!
//! Only the first is the reading:
//!
//! - `GET /api/usage-summary` — the plan window and its lanes. **Required.**
//! - `GET /api/auth/me` — email, name, and the `sub` the legacy endpoint is keyed by.
//! - `GET /api/usage?user=<sub>` — the request quota of a legacy, request-based plan.
//! - `POST /api/dashboard/get-sand-usage-status` — the weekly Grok Bot allowance. Cursor
//!   calls the feature "sand" internally; the dashboard calls it Grok Bot. It is a
//!   cross-site POST, so it needs `Origin: https://cursor.com` or Cursor rejects it.
//!
//! The three supplementary requests are asked of accounts that have nothing to answer with
//! — an account on a modern plan has no request quota, one without a Bot allowance has no
//! Bot window — so a request that *fails* leaves its section absent rather than failing the
//! poll. A supplementary request that *answers* and cannot be read is a different thing:
//! `/api/usage` and the Bot endpoint each describe a window, and a window we cannot read is
//! the malformed case this workspace fails on rather than silently drops. Identity answers
//! no window, so an unreadable one costs only the detail rows.
//!
//! # What the numbers mean
//!
//! Every `used`/`limit`/`remaining` in the summary is **cents**: `7384` is $73.84. Every
//! `*PercentUsed` is already a percentage even when it is below 1.0 — `0.36` means 0.36% of
//! the plan, which the dashboard rounds to 0% — so nothing here multiplies a percent by a
//! hundred.
//!
//! The headline percentage takes the first of these Cursor reports, which is the ladder
//! CodexBar arrived at against live Pro, Team and Enterprise accounts: `plan.totalPercentUsed`,
//! then the mean of the two lane percentages, then either lane alone, then `plan`'s own
//! cents ratio, then `individualUsage.overall` (the personal cap an Enterprise seat gets
//! instead of a `plan` block), then `teamUsage.pooled` (the shared pool, when the account
//! reports no individual usage at all). An account that reports none of them is at 0%,
//! which is what Cursor's own dashboard shows it as.
//!
//! # Windows
//!
//! A billing cycle is a month, and a month is not a fixed number of seconds, so the keys
//! here name their pool rather than deriving from a length that would change between a
//! 30-day and a 31-day cycle and split the history in two. The length is still published
//! when Cursor states both ends of the cycle, because that is what draws the pace mark.
//!
//! A legacy request-based plan replaces the lane bars rather than joining them: its
//! percentages come from the token-based pricing that plan is not on, so showing them
//! beside a request quota would be showing a person two unrelated readings of one account.

use super::{HandSpec, Options, ProviderError, redact_query};
use crate::browser::{self, Keyring, SafeStorage};
use crate::providers::{BoxFuture, Credential, Provider, http, parse_rfc3339, title_case};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

pub const PROVIDER_ID: &str = "cursor";

const USAGE_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
const AUTH_ME_URL: &str = "https://cursor.com/api/auth/me";
const REQUEST_USAGE_URL: &str = "https://cursor.com/api/usage";
const SAND_USAGE_URL: &str = "https://cursor.com/api/dashboard/get-sand-usage-status";

/// Cursor's standalone desktop app keeps its sign-in token in this VS Code-style state store.
const STATE_DATABASE: &str = ".config/Cursor/User/globalStorage/state.vscdb";

/// The key Cursor uses for the raw JWT in [`STATE_DATABASE`].
const ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";

/// What the dashboard POST must be sent from. Cursor refuses the Bot endpoint without it.
const ORIGIN: &str = "https://cursor.com";

/// The cookie names Cursor has carried its session in, newest scheme first. WorkOS is the
/// current one; the Auth.js (`next-auth`/`authjs`) names are what older sessions were filed
/// under. Presence of any one of them is what makes a browser's cookies worth trying.
const SESSION_COOKIE_NAMES: &[&str] = &[
    "WorkosCursorSessionToken",
    "__Secure-next-auth.session-token",
    "next-auth.session-token",
    "wos-session",
    "__Secure-wos-session",
    "authjs.session-token",
    "__Secure-authjs.session-token",
];

/// The hosts a Cursor session can live on, including the authenticator the sign-in flow
/// goes through.
const COOKIE_DOMAINS: &[&str] = &[
    "cursor.com",
    "www.cursor.com",
    "cursor.sh",
    "authenticator.cursor.sh",
];

/// The query every store is read with: everything on Cursor's domains, no name filter —
/// the session names are a gate on the result, not the query, because the header that goes
/// on the wire carries the domain's other cookies too.
fn cookie_query() -> browser::Query {
    browser::Query::new(COOKIE_DOMAINS.iter().copied(), Vec::<String>::new())
}

pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Cursor",
    // Nothing is asked of the user: the credential is the session a browser on this
    // machine already holds, so the settings dialog draws no credential row at all.
    credential: CredentialKind::None,
    credential_hint: "",
    options: &[],
    build,
};

fn build(_credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Cursor::new()?))
}

pub struct Cursor {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Cursor {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            home: std::env::var_os("HOME").map(PathBuf::from),
            storage: Arc::new(Keyring),
            #[cfg(test)]
            base_url: None,
        })
    }

    /// A client reading browsers under a stated home directory with a stated keyring — the
    /// seam the tests pull on, so no test ever touches the real browser it runs beside.
    #[cfg(test)]
    fn for_test(
        home: &std::path::Path,
        storage: Arc<dyn SafeStorage>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            home: Some(home.to_path_buf()),
            storage,
            base_url: None,
        })
    }

    #[cfg(test)]
    fn with_test_base(mut self, base_url: &str) -> Self {
        self.base_url = Some(base_url.trim_end_matches('/').to_owned());
        self
    }

    fn url(&self, production: &'static str) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            let path = production
                .strip_prefix("https://cursor.com")
                .expect("Cursor endpoints share the Cursor origin");
            return format!("{base_url}{path}");
        }
        production.to_owned()
    }

    fn get(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The legacy request-quota call. The account id rides the query string because that is
    /// where Cursor takes it; [`redact_query`] keeps it out of any error we render.
    fn request_usage_request(
        &self,
        user: &str,
        cookie: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(self.url(REQUEST_USAGE_URL))
            .query(&[("user", user)])
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    fn sand_usage_request(&self, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(self.url(SAND_USAGE_URL))
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, ORIGIN)
            .body("{}")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// Every `Cookie:` header a browser on this machine could send to cursor.com.
    ///
    /// Two passes over every profile of every browser, CodexBar's order: first the stores
    /// holding one of the known session-cookie names, then — because the name has changed
    /// before — any store holding cookies on Cursor's domains at all. The headers remain in
    /// that order, but are not trusted on the name alone: [`Self::fetch_inner`] gives each
    /// one the required summary request, then only moves to the next after an HTTP
    /// credential rejection.
    ///
    /// A locked keyring is not an empty answer: a Chromium store's cookies are sealed with
    /// a Secret Service password, so nothing can be said about them until it unlocks, and
    /// the caller gets [`ProviderError::KeyringLocked`] — the waiting state, not a failure.
    async fn session_headers(&self) -> Result<Vec<String>, ProviderError> {
        let stores = match &self.home {
            Some(home) => browser::stores_in(home),
            None => Vec::new(),
        };
        let standalone = self.home.as_deref().and_then(standalone_session_header);
        let now = Timestamp::now();
        let mut keyring_locked = false;
        let mut headers = Vec::new();
        for strict in [true, false] {
            for store in &stores {
                let cookies = match store.cookies(&cookie_query(), self.storage.as_ref()).await {
                    Ok(cookies) => cookies,
                    Err(browser::CookieError::KeyringLocked) => {
                        keyring_locked = true;
                        continue;
                    }
                    // A database that does not open is a browser we cannot ask, not a
                    // session that is not there; the next store may still hold one.
                    Err(_) => continue,
                };
                let live: Vec<_> = cookies
                    .into_iter()
                    .filter(|cookie| cookie.is_live(now))
                    .collect();
                if live.is_empty() {
                    continue;
                }
                let has_session = live
                    .iter()
                    .any(|cookie| SESSION_COOKIE_NAMES.contains(&cookie.name.as_str()));
                if strict && !has_session {
                    continue;
                }
                let header = browser::header(&live);
                if !headers.contains(&header) {
                    headers.push(header);
                }
            }
        }
        if let Some(header) = standalone
            && !headers.contains(&header)
        {
            headers.push(header);
        }
        if headers.is_empty() && keyring_locked {
            return Err(ProviderError::KeyringLocked);
        }
        Ok(headers)
    }

    #[cfg(test)]
    async fn session_header(&self) -> Result<Option<String>, ProviderError> {
        Ok(self.session_headers().await?.into_iter().next())
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let headers = self.session_headers().await?;
        if headers.is_empty() {
            return Err(ProviderError::NoCredential);
        }
        let mut rejected = None;
        for cookie in headers {
            match self.fetch_with_cookie(&cookie).await {
                Err(error @ ProviderError::Credential { .. }) => rejected = Some(error),
                outcome => return outcome,
            }
        }
        // `headers` was non-empty; reaching here means each candidate was explicitly
        // rejected by Cursor, not that no browser had a session. The `None` branch is
        // defensive against a future non-rejection retry rule changing above.
        match rejected {
            Some(error) => Err(error),
            None => Err(ProviderError::NoCredential),
        }
    }

    async fn fetch_with_cookie(&self, cookie: &str) -> Result<Snapshot, ProviderError> {
        let summary_request = self.get(&self.url(USAGE_SUMMARY_URL), cookie)?;
        let summary = super::request(PROVIDER_ID, &self.client, summary_request).await?;
        let identity_request = self.get(&self.url(AUTH_ME_URL), cookie)?;
        let sand_request = self.sand_usage_request(cookie)?;
        let (identity, sand) = tokio::join!(
            super::request(PROVIDER_ID, &self.client, identity_request),
            super::request(PROVIDER_ID, &self.client, sand_request),
        );
        // Both are supplementary: a failure here is an account with nothing to say, not a
        // poll that went wrong. See the module note.
        let identity = identity.ok();
        let sand = sand.ok();

        // The legacy quota is keyed by the account id, so it can only be asked for after
        // identity answered. Parsed here as well as in `parse` so that `parse` stays a pure
        // function of the bodies.
        let subject = identity
            .as_deref()
            .and_then(|body| serde_json::from_str::<UserInfo>(body).ok())
            .and_then(|user| user.sub)
            .filter(|sub| !sub.trim().is_empty());
        let legacy = match subject {
            Some(subject) => {
                let request = self.request_usage_request(&subject, cookie)?;
                super::request(PROVIDER_ID, &self.client, request)
                    .await
                    .ok()
            }
            None => None,
        };

        parse(
            &summary,
            identity.as_deref(),
            legacy.as_deref(),
            sand.as_deref(),
            Timestamp::now(),
        )
    }
}

/// Reads Cursor standalone's session JWT and reconstructs the cookie its dashboard accepts.
///
/// The desktop app owns the database and may be writing it in WAL mode, so its files are
/// copied into an owner-only temporary directory before SQLite opens them. A missing or changed
/// store is just an unavailable credential source; browser sessions remain candidates.
fn standalone_session_header(home: &Path) -> Option<String> {
    let state = StateSnapshot::of(&home.join(STATE_DATABASE)).ok()?;
    let connection = rusqlite::Connection::open_with_flags(
        state.database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let token: String = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [ACCESS_TOKEN_KEY],
            |row| row.get(0),
        )
        .optional()
        .ok()??;
    standalone_cookie(&token)
}

/// Rebuilds the WorkOS session value Cursor's web dashboard derives from its access token.
fn standalone_cookie(token: &str) -> Option<String> {
    let claims = token.split('.').nth(1)?;
    let claims = BASE64_URL.decode(claims).ok()?;
    let subject = serde_json::from_slice::<StandaloneClaims>(&claims)
        .ok()?
        .sub;
    let user = subject.split_once('|')?.1;
    if user.trim().is_empty() {
        return None;
    }
    Some(format!("WorkosCursorSessionToken={user}%3A%3A{token}"))
}

/// The only JWT claim needed to reconstruct the dashboard cookie.
#[derive(Deserialize)]
struct StandaloneClaims {
    sub: String,
}

/// An owner-only temporary copy of Cursor standalone's SQLite database.
#[derive(Debug)]
struct StateSnapshot {
    directory: PathBuf,
}

impl StateSnapshot {
    fn of(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;

        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "tidemark-cursor-state-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let snapshot = Self { directory };
        std::fs::copy(path, snapshot.database())?;
        for sidecar in ["-wal", "-shm"] {
            let source = with_suffix(path, sidecar);
            if source.is_file() {
                std::fs::copy(source, with_suffix(&snapshot.database(), sidecar))?;
            }
        }
        Ok(snapshot)
    }

    fn database(&self) -> PathBuf {
        self.directory.join("state.vscdb")
    }
}

impl Drop for StateSnapshot {
    fn drop(&mut self) {
        // Best effort: the private copy has mode 0700, and a failed cleanup cannot be fixed.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Cursor {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    billing_cycle_start: Option<String>,
    billing_cycle_end: Option<String>,
    membership_type: Option<String>,
    individual_usage: Option<IndividualUsage>,
    team_usage: Option<TeamUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndividualUsage {
    plan: Option<PlanUsage>,
    on_demand: Option<Amounts>,
    /// The personal cap an Enterprise or Team seat gets *instead of* a `plan` block.
    overall: Option<Amounts>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeamUsage {
    on_demand: Option<Amounts>,
    /// The shared pool a whole team draws on.
    pooled: Option<Amounts>,
}

/// Cents used against cents allowed. `limit` absent means the budget is uncapped.
#[derive(Debug, Deserialize)]
struct Amounts {
    used: Option<f64>,
    limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    used: Option<f64>,
    limit: Option<f64>,
    /// Cursor's own models, the lane the dashboard calls "Cursor". Already a percentage.
    auto_percent_used: Option<f64>,
    /// Named third-party models. Already a percentage.
    api_percent_used: Option<f64>,
    /// The headline. Already a percentage.
    total_percent_used: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    email: Option<String>,
    name: Option<String>,
    /// The account id `/api/usage` is keyed by, in its `auth0|…` spelling.
    sub: Option<String>,
}

/// `GET /api/usage?user=…`: the request counter of a legacy, request-based plan.
#[derive(Debug, Deserialize)]
struct RequestUsage {
    /// Every legacy plan reports its counter under the model that plan was sold with.
    #[serde(rename = "gpt-4")]
    gpt4: Option<ModelUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelUsage {
    num_requests: Option<f64>,
    num_requests_total: Option<f64>,
    /// Present only on a request-based plan. Its presence is what identifies one.
    max_request_usage: Option<f64>,
}

/// `POST /api/dashboard/get-sand-usage-status`: the weekly Grok Bot allowance.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandUsage {
    current_period_start: Option<String>,
    next_reset_timestamp_utc: Option<String>,
    usage_percent: Option<f64>,
    /// False for an account with no included Bot allowance, which therefore has no window.
    has_non_zero_included_limit: Option<bool>,
}

/// A percentage Cursor states, refused when it is not a number and clamped to the range a
/// bar can be drawn in.
fn percent(value: Option<f64>, field: &str) -> Result<Option<f64>, ProviderError> {
    match value {
        None => Ok(None),
        Some(value) if value.is_finite() => Ok(Some(value.clamp(0.0, 100.0))),
        Some(_) => Err(ProviderError::malformed(format!(
            "{field} is not a finite number"
        ))),
    }
}

/// A quantity Cursor states — cents, or a request count — refused when it is not a number.
fn number(value: Option<f64>, field: &str) -> Result<Option<f64>, ProviderError> {
    match value {
        None => Ok(None),
        Some(value) if value.is_finite() => Ok(Some(value)),
        Some(_) => Err(ProviderError::malformed(format!(
            "{field} is not a finite number"
        ))),
    }
}

/// The percentage a used/allowed pair states, or `None` when there is no allowance to
/// divide by — a limit of zero is Cursor saying "no cap here", not "everything is spent".
fn ratio(used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    let limit = limit.filter(|limit| *limit > 0.0)?;
    Some((used.unwrap_or(0.0) / limit * 100.0).clamp(0.0, 100.0))
}

/// An instant Cursor states, refused when it is stated and unreadable: a reset time this
/// build cannot parse is not a reset time it may quietly drop.
fn instant(raw: Option<&str>, field: &str) -> Result<Option<Timestamp>, ProviderError> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => Ok(None),
        Some(raw) => parse_rfc3339(raw).map(Some).ok_or_else(|| {
            ProviderError::malformed(format!("{field} is not a readable timestamp"))
        }),
    }
}

/// How long a period lasts, from the two ends of it Cursor names.
fn length_between(start: Option<Timestamp>, end: Option<Timestamp>) -> Option<WindowLength> {
    let (start, end) = (start?, end?);
    u64::try_from(start.seconds_until(end))
        .ok()
        .and_then(WindowLength::from_secs)
}

/// Cursor's own word for a plan, in the spelling it puts in front of a person: `pro_plus`
/// is Pro+, `express` is Start. A level this build has not seen keeps Cursor's own word,
/// re-cased.
fn membership(raw: &str) -> String {
    let named = match raw.trim().to_lowercase().as_str() {
        "enterprise" => "Enterprise".to_owned(),
        "express" => "Start".to_owned(),
        "free" => "Free".to_owned(),
        "free_trial" => "Pro Trial".to_owned(),
        "hobby" => "Hobby".to_owned(),
        "pro" | "pro_student" => "Pro".to_owned(),
        "pro_plus" => "Pro+".to_owned(),
        "team" => "Team".to_owned(),
        "ultra" => "Ultra".to_owned(),
        _ => title_case(raw),
    };
    format!("Cursor {named}")
}

/// A budget in the dollars Cursor holds it in cents: `$4.50 / $10.00`, or the spend alone
/// when the budget is uncapped, which is what an absent limit means.
fn money(used: f64, limit: Option<f64>) -> String {
    match limit.filter(|limit| *limit > 0.0) {
        Some(limit) => format!("${:.2} / ${:.2}", used / 100.0, limit / 100.0),
        None => format!("${:.2}", used / 100.0),
    }
}

fn parse(
    summary_body: &str,
    identity_body: Option<&str>,
    legacy_body: Option<&str>,
    sand_body: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let summary: Summary = serde_json::from_str(summary_body)
        .map_err(|e| ProviderError::malformed(format!("unreadable usage summary: {e}")))?;
    // Identity describes no window, so an unreadable one costs detail rows and nothing else.
    let identity = identity_body.and_then(|body| serde_json::from_str::<UserInfo>(body).ok());
    let legacy = legacy_body
        .map(|body| {
            serde_json::from_str::<RequestUsage>(body)
                .map_err(|e| ProviderError::malformed(format!("unreadable request usage: {e}")))
        })
        .transpose()?;
    let sand = sand_body
        .map(|body| {
            serde_json::from_str::<SandUsage>(body)
                .map_err(|e| ProviderError::malformed(format!("unreadable Grok Bot usage: {e}")))
        })
        .transpose()?;

    let cycle_start = instant(summary.billing_cycle_start.as_deref(), "billingCycleStart")?;
    let cycle_end = instant(summary.billing_cycle_end.as_deref(), "billingCycleEnd")?;
    let cycle_length = length_between(cycle_start, cycle_end);

    let individual = summary.individual_usage.as_ref();
    let plan = individual.and_then(|usage| usage.plan.as_ref());
    let overall = individual.and_then(|usage| usage.overall.as_ref());
    let pooled = summary
        .team_usage
        .as_ref()
        .and_then(|usage| usage.pooled.as_ref());

    let auto = percent(
        plan.and_then(|plan| plan.auto_percent_used),
        "autoPercentUsed",
    )?;
    let api = percent(
        plan.and_then(|plan| plan.api_percent_used),
        "apiPercentUsed",
    )?;
    let total = percent(
        plan.and_then(|plan| plan.total_percent_used),
        "totalPercentUsed",
    )?;

    let plan_used = number(plan.and_then(|plan| plan.used), "plan.used")?;
    let plan_limit = number(plan.and_then(|plan| plan.limit), "plan.limit")?;
    let overall_used = number(overall.and_then(|overall| overall.used), "overall.used")?;
    let overall_limit = number(overall.and_then(|overall| overall.limit), "overall.limit")?;
    let pooled_used = number(pooled.and_then(|pooled| pooled.used), "pooled.used")?;
    let pooled_limit = number(pooled.and_then(|pooled| pooled.limit), "pooled.limit")?;

    // The ladder in the module note, in order.
    let headline = total
        .or_else(|| match (auto, api) {
            (Some(auto), Some(api)) => Some(((auto + api) / 2.0).clamp(0.0, 100.0)),
            _ => None,
        })
        .or(api)
        .or(auto)
        .or_else(|| ratio(plan_used, plan_limit))
        .or_else(|| match (overall_used, overall_limit) {
            (Some(used), limit) => ratio(Some(used), limit),
            _ => None,
        })
        .or_else(|| match (pooled_used, pooled_limit) {
            (Some(used), limit) => ratio(Some(used), limit),
            _ => None,
        })
        .unwrap_or(0.0);

    // The absolutes under the headline bar come from whichever block the account is
    // actually metered against, so they never contradict the percentage above them.
    let (usd_used, usd_limit) = if plan_used.unwrap_or(0.0) > 0.0 || plan_limit.unwrap_or(0.0) > 0.0
    {
        (plan_used.unwrap_or(0.0), plan_limit)
    } else if let (Some(used), Some(limit)) = (overall_used, overall_limit) {
        (used, Some(limit))
    } else if let (Some(used), Some(limit)) = (pooled_used, pooled_limit) {
        (used, Some(limit))
    } else {
        (0.0, None)
    };

    // A request-based plan is identified by the presence of a request ceiling, and reads
    // only when the counter beside it is there too.
    let requests = match legacy.and_then(|legacy| legacy.gpt4) {
        Some(model) => {
            let limit = number(model.max_request_usage, "maxRequestUsage")?;
            let used = number(
                model.num_requests_total.or(model.num_requests),
                "numRequests",
            )?;
            match (used, limit.filter(|limit| *limit > 0.0)) {
                (Some(used), Some(limit)) => Some((used, limit)),
                _ => None,
            }
        }
        None => None,
    };

    let mut windows = Vec::new();
    if let Some((used, limit)) = requests {
        // A legacy plan meters requests, not dollars, so this is a different pool from the
        // one the lanes below describe and carries a key of its own. Named rather than
        // keyed by length: the billing month it resets with is not a fixed span.
        windows.push(Window {
            key: WindowKey::named("requests"),
            title: "Requests".to_owned(),
            subtitle: Some(format!("{used:.0} / {limit:.0} requests")),
            used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
            resets_at: cycle_end,
            length: cycle_length,
        });
    } else {
        windows.push(Window {
            // Named for the same reason as above: one plan pool, on a month that is not a
            // fixed number of seconds.
            key: WindowKey::named("plan"),
            title: "Total".to_owned(),
            subtitle: usd_limit
                .filter(|limit| *limit > 0.0)
                .map(|limit| money(usd_used, Some(limit))),
            used_percent: headline,
            resets_at: cycle_end,
            length: cycle_length,
        });
        // The two lanes the headline is composed of. Cursor states them as percentages
        // only, so there is nothing to print under the bars.
        for (key, title, value) in [("auto", "Cursor", auto), ("api", "Third Party", api)] {
            if let Some(used_percent) = value {
                windows.push(Window {
                    key: WindowKey::named(key),
                    title: title.to_owned(),
                    subtitle: None,
                    used_percent,
                    resets_at: cycle_end,
                    length: cycle_length,
                });
            }
        }
        // The Bot allowance is weekly and separate from the billing cycle. Its own pool,
        // its own period, and absent entirely for an account with no included limit.
        if let Some(sand) = sand.filter(|sand| sand.has_non_zero_included_limit == Some(true))
            && let Some(used_percent) = percent(sand.usage_percent, "usagePercent")?
        {
            let start = instant(sand.current_period_start.as_deref(), "currentPeriodStart")?;
            let resets_at = instant(
                sand.next_reset_timestamp_utc.as_deref(),
                "nextResetTimestampUtc",
            )?;
            windows.push(Window {
                key: WindowKey::named("grok-bot"),
                title: "Grok Bot".to_owned(),
                subtitle: None,
                used_percent,
                resets_at,
                length: length_between(start, resets_at),
            });
        }
    }

    let mut details = Vec::new();
    if let Some(level) = summary
        .membership_type
        .as_deref()
        .map(str::trim)
        .filter(|level| !level.is_empty())
    {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: membership(level),
            }],
        });
    }

    // On-demand spend is money past the plan, not a quota that drains: it belongs beside
    // the account rather than under a bar.
    let mut usage = Vec::new();
    if let Some(on_demand) = individual.and_then(|usage| usage.on_demand.as_ref()) {
        let used = number(on_demand.used, "onDemand.used")?.unwrap_or(0.0);
        let limit = number(on_demand.limit, "onDemand.limit")?;
        if used > 0.0 || limit.is_some_and(|limit| limit > 0.0) {
            usage.push(DetailRow {
                label: "On-demand".to_owned(),
                value: money(used, limit),
            });
        }
    }
    if let Some(on_demand) = summary
        .team_usage
        .as_ref()
        .and_then(|usage| usage.on_demand.as_ref())
    {
        let used = number(on_demand.used, "teamUsage.onDemand.used")?.unwrap_or(0.0);
        let limit = number(on_demand.limit, "teamUsage.onDemand.limit")?;
        if used > 0.0 || limit.is_some_and(|limit| limit > 0.0) {
            usage.push(DetailRow {
                label: "Team on-demand".to_owned(),
                value: money(used, limit),
            });
        }
    }
    if !usage.is_empty() {
        details.push(DetailSection {
            title: "Usage".to_owned(),
            rows: usage,
        });
    }

    let mut account = Vec::new();
    if let Some(identity) = identity.as_ref() {
        for (label, value) in [("Email", &identity.email), ("Name", &identity.name)] {
            if let Some(value) = value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                account.push(DetailRow {
                    label: label.to_owned(),
                    value: value.to_owned(),
                });
            }
        }
    }
    if !account.is_empty() {
        details.push(DetailSection {
            title: "Account".to_owned(),
            rows: account,
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

#[cfg(test)]
mod tests {
    use super::{Cursor, Options, SPEC, parse};
    use crate::providers::{Credential, ProviderError};
    use tidemark_types::{CredentialKind, DetailSection, Timestamp};

    /// A live Pro account's `GET /api/usage-summary`, as CodexBar's own regression test
    /// records it: fractional lane percentages that are percentages, not fractions.
    const PRO: &str = r#"{
      "billingCycleStart": "2026-03-18T20:45:42.000Z",
      "billingCycleEnd": "2026-04-18T20:45:42.000Z",
      "membershipType": "pro",
      "limitType": "user",
      "isUnlimited": false,
      "autoModelSelectedDisplayMessage": "You've used 1% of your included total usage",
      "namedModelSelectedDisplayMessage": "You've used 1% of your included API usage",
      "individualUsage": {
        "plan": {
          "enabled": true,
          "used": 86,
          "limit": 2000,
          "remaining": 1914,
          "breakdown": { "included": 86, "bonus": 0, "total": 86 },
          "autoPercentUsed": 0.36,
          "apiPercentUsed": 0.7111111111111111,
          "totalPercentUsed": 0.441025641025641
        },
        "onDemand": { "enabled": false, "used": 0, "limit": null, "remaining": null }
      },
      "teamUsage": { "onDemand": null }
    }"#;

    /// A live Enterprise account, sanitized: no `plan` block at all, a personal cap under
    /// `individualUsage.overall`, and the shared pool beside it.
    const ENTERPRISE: &str = r#"{
      "billingCycleStart": "2026-04-01T00:00:00.000Z",
      "billingCycleEnd": "2026-05-01T00:00:00.000Z",
      "membershipType": "enterprise",
      "limitType": "team",
      "isUnlimited": false,
      "individualUsage": {
        "overall": { "enabled": true, "used": 7384, "limit": 10000, "remaining": 2616 }
      },
      "teamUsage": {
        "onDemand": { "enabled": true, "used": 0, "limit": null, "remaining": null },
        "pooled": {
          "enabled": true,
          "used": 12725135,
          "limit": 28122000,
          "remaining": 15396865
        }
      }
    }"#;

    /// A Pro account with both on-demand budgets reported.
    const BUDGETS: &str = r#"{
      "billingCycleStart": "2025-01-01T00:00:00.000Z",
      "billingCycleEnd": "2025-02-01T00:00:00.000Z",
      "membershipType": "pro",
      "individualUsage": {
        "plan": {
          "enabled": true,
          "used": 1500,
          "limit": 5000,
          "remaining": 3500,
          "totalPercentUsed": 30.0
        },
        "onDemand": { "enabled": true, "used": 500, "limit": 10000, "remaining": 9500 }
      },
      "teamUsage": {
        "onDemand": { "enabled": true, "used": 2000, "limit": 50000, "remaining": 48000 }
      }
    }"#;

    const IDENTITY: &str = r#"{"email":"user@example.com","email_verified":true,"name":"Test User","sub":"auth0|user_test"}"#;

    /// `GET /api/usage?user=…` for a legacy request-based plan.
    const LEGACY: &str =
        r#"{"gpt-4":{"numRequests":200,"numRequestsTotal":240,"maxRequestUsage":500}}"#;

    /// `POST /api/dashboard/get-sand-usage-status` for an account with a Bot allowance.
    const SAND: &str = r#"{
      "currentPeriodStart": "2026-08-17T07:57:50.647Z",
      "nextResetTimestampUtc": "2026-08-24T07:57:50.647Z",
      "usagePercent": 100,
      "hasAvailableUsage": true,
      "hasNonZeroIncludedLimit": true
    }"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn a_pro_account_reads_as_a_total_and_the_two_lanes_it_is_made_of() {
        let snapshot = parse(PRO, Some(IDENTITY), None, None, at(1_774_000_000)).expect("parses");

        assert_eq!(snapshot.provider.as_str(), "cursor");
        assert_eq!(snapshot.windows.len(), 3);

        let total = &snapshot.windows[0];
        assert_eq!(total.key.as_str(), "plan");
        assert_eq!(total.title, "Total");
        assert_eq!(total.used_percent, 0.441_025_641_025_641);
        assert_eq!(total.subtitle.as_deref(), Some("$0.86 / $20.00"));
        // 2026-03-18T20:45:42Z .. 2026-04-18T20:45:42Z is thirty-one days.
        assert_eq!(total.length.expect("both ends stated").as_secs(), 2_678_400);
        assert_eq!(total.resets_at.expect("stated").as_unix(), 1_776_545_142);

        let auto = &snapshot.windows[1];
        assert_eq!(auto.key.as_str(), "auto");
        assert_eq!(auto.title, "Cursor");
        assert_eq!(
            auto.used_percent, 0.36,
            "a fractional percent is a percent, not a fraction"
        );
        assert_eq!(auto.subtitle, None);

        let api = &snapshot.windows[2];
        assert_eq!(api.key.as_str(), "api");
        assert_eq!(api.title, "Third Party");
        assert_eq!(api.used_percent, 0.711_111_111_111_111_1);

        let plan = &snapshot.details[0];
        assert_eq!(plan.title, DetailSection::PLAN);
        assert_eq!(plan.rows[0].label, "Plan");
        assert_eq!(plan.rows[0].value, "Cursor Pro");

        // On-demand is off and unspent, so there is no usage section to show.
        let account = snapshot
            .details
            .iter()
            .find(|section| section.title == "Account")
            .expect("identity answered");
        assert_eq!(account.rows[0].value, "user@example.com");
        assert_eq!(account.rows[1].value, "Test User");
        assert!(
            !snapshot
                .details
                .iter()
                .any(|section| section.title == "Usage")
        );
    }

    #[test]
    fn an_enterprise_seat_is_metered_by_its_personal_cap_not_the_team_pool() {
        let snapshot = parse(ENTERPRISE, None, None, None, at(1_775_000_000)).expect("parses");

        // $73.84 of $100.00, which is what Cursor's own dashboard shows the seat as.
        let total = &snapshot.windows[0];
        assert!((total.used_percent - 73.84).abs() < 1e-9);
        assert_eq!(total.subtitle.as_deref(), Some("$73.84 / $100.00"));
        assert_eq!(
            snapshot.windows.len(),
            1,
            "an account with no plan block states no lane percentages"
        );
        assert_eq!(snapshot.details[0].rows[0].value, "Cursor Enterprise");
    }

    #[test]
    fn a_seat_with_no_individual_usage_falls_back_to_the_shared_pool() {
        let pooled_only = ENTERPRISE.replacen(
            r#""overall": { "enabled": true, "used": 7384, "limit": 10000, "remaining": 2616 }"#,
            "",
            1,
        );

        let snapshot = parse(&pooled_only, None, None, None, at(1_775_000_000)).expect("parses");

        let total = &snapshot.windows[0];
        // 12,725,135 of 28,122,000 cents.
        assert!((total.used_percent - 45.249_751_084_560_13).abs() < 1e-9);
        assert_eq!(total.subtitle.as_deref(), Some("$127251.35 / $281220.00"));
    }

    #[test]
    fn a_legacy_plan_shows_its_request_quota_instead_of_lanes_it_is_not_metered_by() {
        let snapshot = parse(
            PRO,
            Some(IDENTITY),
            Some(LEGACY),
            Some(SAND),
            at(1_774_000_000),
        )
        .expect("parses");

        assert_eq!(
            snapshot.windows.len(),
            1,
            "the token-based lanes and the Bot allowance do not belong beside a request quota"
        );
        let requests = &snapshot.windows[0];
        assert_eq!(requests.key.as_str(), "requests");
        assert_eq!(requests.title, "Requests");
        assert_eq!(requests.subtitle.as_deref(), Some("240 / 500 requests"));
        assert_eq!(requests.used_percent, 48.0);
        assert_eq!(requests.resets_at.expect("stated").as_unix(), 1_776_545_142);
    }

    #[test]
    fn a_modern_plan_answering_the_legacy_endpoint_keeps_its_lanes() {
        // Cursor answers `/api/usage` for every account; only a request-based plan states a
        // ceiling in it.
        let no_ceiling = r#"{"gpt-4":{"numRequests":0},"startOfMonth":"2026-03-18"}"#;

        let snapshot = parse(PRO, None, Some(no_ceiling), None, at(1_774_000_000)).expect("parses");

        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(snapshot.windows[0].key.as_str(), "plan");
    }

    #[test]
    fn the_bot_allowance_is_a_weekly_window_of_its_own() {
        let snapshot = parse(PRO, None, None, Some(SAND), at(1_787_000_000)).expect("parses");

        let bot = snapshot.windows.last().expect("a fourth window");
        assert_eq!(bot.key.as_str(), "grok-bot");
        assert_eq!(bot.title, "Grok Bot");
        assert_eq!(bot.used_percent, 100.0);
        assert_eq!(bot.length.expect("both ends stated").as_secs(), 604_800);
        assert_eq!(bot.resets_at.expect("stated").as_unix(), 1_787_558_270);
    }

    #[test]
    fn an_account_without_an_included_bot_allowance_has_no_bot_window() {
        let none = SAND.replacen(
            r#""hasNonZeroIncludedLimit": true"#,
            r#""hasNonZeroIncludedLimit": false"#,
            1,
        );

        let snapshot = parse(PRO, None, None, Some(&none), at(1_787_000_000)).expect("parses");

        assert_eq!(snapshot.windows.len(), 3);
    }

    #[test]
    fn both_on_demand_budgets_are_reported_in_the_dollars_they_are_held_in() {
        let snapshot = parse(BUDGETS, None, None, None, at(1_736_000_000)).expect("parses");

        assert_eq!(snapshot.windows[0].used_percent, 30.0);
        let usage = snapshot
            .details
            .iter()
            .find(|section| section.title == "Usage")
            .expect("both budgets are reported");
        assert_eq!(usage.rows[0].label, "On-demand");
        assert_eq!(usage.rows[0].value, "$5.00 / $100.00");
        assert_eq!(usage.rows[1].label, "Team on-demand");
        assert_eq!(usage.rows[1].value, "$20.00 / $500.00");
    }

    #[test]
    fn an_account_cursor_reports_no_usage_for_reads_as_untouched_rather_than_failing() {
        let snapshot = parse(
            r#"{"membershipType":"hobby","individualUsage":{}}"#,
            None,
            None,
            None,
            at(1_774_000_000),
        )
        .expect("parses");

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 0.0);
        assert_eq!(snapshot.windows[0].subtitle, None);
        assert_eq!(snapshot.windows[0].length, None);
        assert_eq!(snapshot.details[0].rows[0].value, "Cursor Hobby");
    }

    #[test]
    fn a_window_source_that_answers_something_unreadable_fails_the_whole_fetch() {
        let malformed_bot = SAND.replacen(r#""usagePercent": 100"#, r#""usagePercent": "all""#, 1);
        assert!(matches!(
            parse(PRO, None, None, Some(&malformed_bot), at(1_787_000_000)),
            Err(ProviderError::Malformed { .. })
        ));

        let malformed_requests =
            LEGACY.replacen(r#""maxRequestUsage":500"#, r#""maxRequestUsage":"lots""#, 1);
        assert!(matches!(
            parse(
                PRO,
                None,
                Some(&malformed_requests),
                None,
                at(1_774_000_000)
            ),
            Err(ProviderError::Malformed { .. })
        ));

        let malformed_reset = PRO.replacen(
            r#""billingCycleEnd": "2026-04-18T20:45:42.000Z""#,
            r#""billingCycleEnd": "in a month""#,
            1,
        );
        assert!(matches!(
            parse(&malformed_reset, None, None, None, at(1_774_000_000)),
            Err(ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn an_unreadable_identity_costs_the_account_rows_and_nothing_else() {
        let snapshot = parse(PRO, Some("not json"), None, None, at(1_774_000_000)).expect("parses");

        assert_eq!(snapshot.windows.len(), 3);
        assert!(
            !snapshot
                .details
                .iter()
                .any(|section| section.title == "Account")
        );
    }

    #[test]
    fn every_request_carries_the_session_and_the_dashboard_post_carries_its_origin() {
        let provider = Cursor::new().expect("builds");
        let cookie = "WorkosCursorSessionToken=abc";

        let summary = provider
            .get(super::USAGE_SUMMARY_URL, cookie)
            .expect("builds");
        assert_eq!(summary.method(), reqwest::Method::GET);
        assert_eq!(
            summary.url().as_str(),
            "https://cursor.com/api/usage-summary"
        );

        let legacy = provider
            .request_usage_request("auth0|user_test", cookie)
            .expect("builds");
        assert_eq!(
            legacy.url().as_str(),
            "https://cursor.com/api/usage?user=auth0%7Cuser_test"
        );

        let sand = provider.sand_usage_request(cookie).expect("builds");
        assert_eq!(sand.method(), reqwest::Method::POST);
        assert_eq!(
            sand.url().as_str(),
            "https://cursor.com/api/dashboard/get-sand-usage-status"
        );
        assert_eq!(
            sand.headers().get("origin").expect("present"),
            "https://cursor.com"
        );
        assert_eq!(
            sand.body().and_then(reqwest::Body::as_bytes),
            Some(b"{}".as_slice()),
            "the dashboard endpoint takes an empty JSON object"
        );

        for request in [summary, legacy, sand] {
            assert_eq!(request.headers().get("cookie").expect("present"), cookie);
        }
    }

    #[test]
    fn the_spec_builds_a_cursor_provider_that_needs_no_credential() {
        assert_eq!(SPEC.id, "cursor");
        assert_eq!(SPEC.title, "Cursor");
        assert_eq!(SPEC.credential, CredentialKind::None);
        assert!(SPEC.options.is_empty());

        let provider =
            (SPEC.build)(Credential::new(String::new()), &Options::new()).expect("builds");
        assert_eq!(provider.id().as_str(), "cursor");
    }

    // The session-resolution tests below run against a fixture home directory and never
    // touch a real browser. The fixture database is Gecko (plain text) so the test needs
    // no keyring at all; the Chromium decryption path has its own tests in `browser`.

    use crate::browser::SafeStorage;
    use crate::secrets::SecretError;
    use std::sync::Arc;

    /// A keyring that answers nothing — enough for Gecko stores, which never ask.
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

    /// A keyring that is locked, for the waiting-state test.
    #[derive(Debug)]
    struct LockedKeyring;

    impl SafeStorage for LockedKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
            Box::pin(async { Err(SecretError::Locked) })
        }
    }

    /// A throwaway home with one Gecko profile holding the given cookies.
    fn gecko_home(cookies: &[(&str, &str, &str, i64)]) -> crate::browser::tests::TestHome {
        use rusqlite::Connection;
        let home = crate::browser::tests::TestHome::new();
        let path = home.gecko(".zen/k26qcf29.Default (release)");
        let connection = Connection::open(&path).expect("opens");
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
            .expect("creates the table");
        for (host, name, value, expiry) in cookies {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES (?1, ?2, ?3, '/', ?4, 1, 0, 0, 0)",
                    (host, name, value, expiry),
                )
                .expect("inserts");
        }
        home
    }

    /// A throwaway Cursor standalone state store holding its access token.
    fn cursor_home(access_token: &str) -> crate::browser::tests::TestHome {
        let home = crate::browser::tests::TestHome::new();
        cursor_state(&home, access_token);
        home
    }

    fn cursor_state(home: &crate::browser::tests::TestHome, access_token: &str) {
        use rusqlite::Connection;

        let path = home
            .path()
            .join(".config/Cursor/User/globalStorage/state.vscdb");
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("creates");
        let connection = Connection::open(path).expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
            )
            .expect("creates the table");
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', ?1)",
                [access_token],
            )
            .expect("inserts the access token");
    }

    fn cursor_state_in_wal(
        home: &crate::browser::tests::TestHome,
        access_token: &str,
    ) -> rusqlite::Connection {
        let path = home
            .path()
            .join(".config/Cursor/User/globalStorage/state.vscdb");
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("creates");
        let connection = rusqlite::Connection::open(path).expect("opens");
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
            )
            .expect("sets WAL mode and creates the table");
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', ?1)",
                [access_token],
            )
            .expect("inserts the access token");
        connection
    }

    fn header_of(provider: &Cursor) -> Option<String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.session_header())
            .expect("no keyring state")
    }

    fn session_server() -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header read");
                    if line == "\r\n" {
                        request.push_str(&line);
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().expect("content length");
                    }
                    request.push_str(&line);
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).expect("body read");
                request.push_str(&String::from_utf8(body).expect("request body is text"));
                drop(reader);

                let (status, response) =
                    if request.contains("WorkosCursorSessionToken=expired-session") {
                        ("401 Unauthorized", "{}")
                    } else if request.starts_with("GET /api/usage-summary ") {
                        ("200 OK", PRO)
                    } else {
                        ("200 OK", "{}")
                    };
                request_tx.send(request).expect("request captured");
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .expect("response written");
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    #[test]
    fn a_later_browser_session_is_used_after_an_earlier_one_is_rejected() {
        let home = gecko_home(&[(
            ".cursor.com",
            "WorkosCursorSessionToken",
            "expired-session",
            0,
        )]);
        let other = home.gecko(".zen/zz99.Working");
        {
            use rusqlite::Connection;

            let connection = Connection::open(&other).expect("opens");
            connection
                .execute_batch(
                    "CREATE TABLE moz_cookies (
                        id INTEGER PRIMARY KEY,
                        baseDomain TEXT,
                        originAttributes TEXT NOT NULL DEFAULT '',
                        name TEXT, value TEXT, host TEXT, path TEXT,
                        expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                        isSecure INTEGER, isHttpOnly INTEGER
                    );
                    INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES (
                        '.cursor.com', 'WorkosCursorSessionToken', 'working-session', '/', 0,
                        1, 0, 0, 0
                    );",
                )
                .expect("creates the working session");
        }
        let (base, requests, server) = session_server();
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring))
            .expect("builds")
            .with_test_base(&base);

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("the working session wins");
        assert_eq!(snapshot.provider.as_str(), "cursor");

        let requests: Vec<_> = (0..4)
            .map(|_| requests.recv().expect("request captured"))
            .collect();
        server.join().expect("server stopped");
        let summaries: Vec<_> = requests
            .iter()
            .filter(|request| request.starts_with("GET /api/usage-summary "))
            .collect();
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].contains("WorkosCursorSessionToken=expired-session"));
        assert!(summaries[1].contains("WorkosCursorSessionToken=working-session"));
    }

    #[test]
    fn a_signed_in_browser_supplies_the_session_header() {
        let home = gecko_home(&[
            (".cursor.com", "WorkosCursorSessionToken", "the-session", 0),
            (".cursor.com", "_ga", "analytics", 0),
            (".google.com", "NID", "not-ours", 0),
        ]);
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        let header = header_of(&provider).expect("a session");
        // The whole domain's cookies go on the wire, not just the session one.
        assert!(header.contains("WorkosCursorSessionToken=the-session"));
        assert!(header.contains("_ga=analytics"));
        assert!(
            !header.contains("NID"),
            "other sites' cookies are never read"
        );
    }

    #[test]
    fn a_signed_in_cursor_desktop_app_supplies_the_session_header() {
        let home =
            cursor_home("eyJhbGciOiJub25lIn0.eyJzdWIiOiJhdXRoMHx1c2VyX2N1cnNvciJ9.signature");
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        assert_eq!(
            header_of(&provider).as_deref(),
            Some(
                "WorkosCursorSessionToken=user_cursor%3A%3AeyJhbGciOiJub25lIn0.eyJzdWIiOiJhdXRoMHx1c2VyX2N1cnNvciJ9.signature"
            )
        );
    }

    #[test]
    fn a_cursor_desktop_session_is_tried_after_browser_sessions_are_rejected() {
        let home = gecko_home(&[(
            ".cursor.com",
            "WorkosCursorSessionToken",
            "expired-session",
            0,
        )]);
        cursor_state(
            &home,
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhdXRoMHx1c2VyX2N1cnNvciJ9.signature",
        );
        let (base, requests, server) = session_server();
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring))
            .expect("builds")
            .with_test_base(&base);

        let snapshot = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(provider.fetch_inner())
            .expect("the standalone session wins after browser rejection");
        assert_eq!(snapshot.provider.as_str(), "cursor");

        let requests: Vec<_> = (0..4)
            .map(|_| requests.recv().expect("request captured"))
            .collect();
        server.join().expect("server stopped");
        let summaries: Vec<_> = requests
            .iter()
            .filter(|request| request.starts_with("GET /api/usage-summary "))
            .collect();
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].contains("WorkosCursorSessionToken=expired-session"));
        assert!(summaries[1].contains(
            "WorkosCursorSessionToken=user_cursor%3A%3AeyJhbGciOiJub25lIn0.eyJzdWIiOiJhdXRoMHx1c2VyX2N1cnNvciJ9.signature"
        ));
    }

    #[test]
    fn a_cursor_desktop_session_committed_only_to_wal_is_found() {
        let home = crate::browser::tests::TestHome::new();
        let writer = cursor_state_in_wal(
            &home,
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhdXRoMHx1c2VyX2N1cnNvciJ9.signature",
        );
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        assert!(header_of(&provider).is_some());
        drop(writer);
    }

    #[test]
    fn a_known_session_name_wins_over_a_store_that_only_has_domain_cookies() {
        let home = gecko_home(&[(".cursor.com", "_ga", "analytics", 0)]);
        // A second profile, later in the scan order, that holds the named session.
        let other = home.gecko(".zen/zz99.Other");
        {
            use rusqlite::Connection;
            let connection = Connection::open(&other).expect("opens");
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
                .expect("creates the table");
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES ('.cursor.com', 'WorkosCursorSessionToken', 'the-session', '/', 0, 1, 0, 0, 0)",
                    [],
                )
                .expect("inserts");
        }
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        let header = header_of(&provider).expect("a session");
        assert!(header.contains("WorkosCursorSessionToken=the-session"));
        assert!(
            !header.contains("analytics"),
            "the strict pass runs over every store before the fallback pass"
        );
    }

    #[test]
    fn a_store_without_a_known_name_is_still_tried_in_the_fallback_pass() {
        let home = gecko_home(&[(".cursor.com", "_ga", "analytics", 0)]);
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        assert_eq!(header_of(&provider).as_deref(), Some("_ga=analytics"));
    }

    #[test]
    fn a_dead_session_cookie_is_not_a_session() {
        let home = gecko_home(&[(
            ".cursor.com",
            "WorkosCursorSessionToken",
            "expired-session",
            1_600_000_000, // 2020
        )]);
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        assert_eq!(header_of(&provider), None);
    }

    #[test]
    fn a_machine_with_no_cursor_session_is_a_missing_credential_not_an_error() {
        let home = crate::browser::tests::TestHome::new();
        let provider = Cursor::for_test(home.path(), Arc::new(NoKeyring)).expect("builds");

        assert_eq!(header_of(&provider), None);
    }

    #[test]
    fn a_locked_keyring_is_a_state_to_wait_out_not_a_missing_session() {
        let home = crate::browser::tests::TestHome::new();
        // A Chromium profile's cookies are sealed with the password this keyring holds.
        home.profile("chromium/Default", "Cookies");
        let provider = Cursor::for_test(home.path(), Arc::new(LockedKeyring)).expect("builds");

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.session_header());
        assert!(matches!(result, Err(ProviderError::KeyringLocked)));
    }
}
