//! Alibaba Coding Plan quota, read from an explicitly selected browser session.
//!
//! Alibaba exposes the quota through a OneConsole RPC, not through DashScope's inference
//! API. A session fetch therefore has three steps: read `SEC_TOKEN` from the region's
//! dashboard (falling back to `/tool/user/info.json` and then the cookie itself), send the
//! console's form-encoded RPC with the same cookie jar, and parse the returned plan
//! instance. International and China-mainland accounts use different dashboards and RPC
//! gateways, so a region-shaped failure is retried against the other pair.
//!
//! Browser profiles are explicit credentials. On Unix the shared browser reader finds and
//! decrypts matching cookies from every supported browser; on Windows the paste-session
//! mode remains the reliable path for Chromium's App-Bound cookies. The copied header is
//! normalized by `keyed::session` before it is sent. Tidemark never stores cookies imported
//! from a browser profile.
//!
//! The RPC form contains `SEC_TOKEN`, so its request and bootstrap responses deliberately do
//! not pass through the raw-response recorder: recording that body would persist a live
//! console credential. The final quota body is parsed in memory only.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage};
use crate::providers::{BoxFuture, Credential, Provider};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tidemark_types::{
    AccountId, AuthCandidate, AuthCandidateState, CredentialKind, DetailRow, DetailSection,
    ProviderId, Snapshot, Timestamp, Window, WindowKey, WindowLength,
};
use time::{
    Date, OffsetDateTime, PrimitiveDateTime, format_description,
    format_description::well_known::Rfc3339,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "alibaba";

const ACTION: &str = "zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2";
const RPC_PRODUCT: &str = "sfm_bailian";
const SESSION_COOKIE: &str = "login_aliyunid_ticket";
const ACCOUNT_COOKIES: &[&str] = &["login_aliyunid_pk", "login_current_pk", "login_aliyunid"];
const COOKIE_DOMAINS: &[&str] = &[
    "bailian-singapore-cs.alibabacloud.com",
    "bailian-cs.console.aliyun.com",
    "bailian-beijing-cs.aliyuncs.com",
    "modelstudio.console.alibabacloud.com",
    "bailian.console.aliyun.com",
    "free.aliyun.com",
    "account.aliyun.com",
    "signin.aliyun.com",
    "passport.alibabacloud.com",
    "console.alibabacloud.com",
    "console.aliyun.com",
    "alibabacloud.com",
    "aliyun.com",
];

/// The three windows' lengths: five hours, a week, and upstream's 30-day spelling of a
/// billing month.
const FIVE_HOURS: u64 = 5 * 60 * 60;
const WEEK: u64 = 7 * 24 * 60 * 60;
const MONTH: u64 = 30 * 24 * 60 * 60;

/// The keys of the plan-instance list, camelCase and the snake_case alias.
const INSTANCE_KEYS: &[&str] = &["codingPlanInstanceInfos", "coding_plan_instance_infos"];
/// The keys of the quota block, and of each figure it may carry.
const QUOTA_CONTAINER_KEYS: &[&str] = &["codingPlanQuotaInfo", "coding_plan_quota_info"];
const QUOTA_FIGURE_KEYS: &[&str] = &[
    "per5HourUsedQuota",
    "per5HourTotalQuota",
    "perWeekUsedQuota",
    "perWeekTotalQuota",
    "perBillMonthUsedQuota",
    "perBillMonthTotalQuota",
];

/// Alibaba Coding Plan as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Alibaba Coding Plan",
    credential: CredentialKind::External,
    credential_hint: "Linux reads a signed-in supported browser; Windows accepts a Google Chrome Cookie header.",
    options: session::OPTIONS,
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Alibaba::new_for_account(
        account,
        &credential,
        options,
    )?))
}

/// One of the two console deployments the same plan RPC lives on.
#[derive(Debug, Clone, Copy)]
enum Region {
    International,
    ChinaMainland,
}

impl Region {
    fn region_id(self) -> &'static str {
        match self {
            Self::International => "ap-southeast-1",
            Self::ChinaMainland => "cn-beijing",
        }
    }

    fn origin(self) -> &'static str {
        match self {
            Self::International => "https://modelstudio.console.alibabacloud.com",
            Self::ChinaMainland => "https://bailian.console.aliyun.com",
        }
    }

    fn dashboard(self) -> &'static str {
        match self {
            Self::International => {
                "https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=coding-plan#/efm/coding_plan"
            }
            Self::ChinaMainland => {
                "https://bailian.console.aliyun.com/cn-beijing/?tab=model#/efm/coding_plan"
            }
        }
    }

    fn rpc_base(self) -> &'static str {
        match self {
            Self::International => "https://bailian-singapore-cs.alibabacloud.com",
            Self::ChinaMainland => "https://bailian-cs.console.aliyun.com",
        }
    }

    fn rpc_action(self) -> &'static str {
        match self {
            Self::International => "IntlBroadScopeAspnGateway",
            Self::ChinaMainland => "BroadScopeAspnGateway",
        }
    }

    fn console_domain(self) -> &'static str {
        match self {
            Self::International => "modelstudio.console.alibabacloud.com",
            Self::ChinaMainland => "bailian.console.aliyun.com",
        }
    }

    fn console_site(self) -> &'static str {
        match self {
            Self::International => "MODELSTUDIO_ALIBABACLOUD",
            Self::ChinaMainland => "BAILIAN_ALIYUN",
        }
    }

    fn commodity_code(self) -> &'static str {
        match self {
            Self::International => "sfm_codingplan_public_intl",
            Self::ChinaMainland => "sfm_codingplan_public_cn",
        }
    }
}

/// One Alibaba account, authenticated by one explicitly chosen browser profile or paste.
pub struct Alibaba {
    tidemark_account: AccountId,
    client: reqwest::Client,
    browser_home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    source: Option<session::Source>,
    #[cfg(test)]
    bases: Option<(String, String)>,
}

impl Alibaba {
    /// Builds the default account against the real console gateways.
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
            tidemark_account: account_id,
            client: http::client()?,
            browser_home: None,
            storage: Arc::new(Keyring),
            source: session::source(credential, options),
            #[cfg(test)]
            bases: None,
        })
    }

    #[cfg(test)]
    fn for_test(intl_base: &str, cn_base: &str, cookie: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: http::client()?,
            browser_home: None,
            storage: Arc::new(Keyring),
            source: Some(session::Source::Pasted(cookie.to_owned())),
            bases: Some((
                intl_base.trim_end_matches('/').to_owned(),
                cn_base.trim_end_matches('/').to_owned(),
            )),
        })
    }

    #[cfg(test)]
    fn for_browser_test(
        home: &Path,
        storage: Arc<dyn SafeStorage>,
        intl_base: &str,
        cn_base: &str,
    ) -> Result<Self, ProviderError> {
        use crate::browser::auth::Selection;
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: http::client()?,
            browser_home: Some(home.to_path_buf()),
            storage,
            source: Some(session::Source::Browser(Selection {
                browser: "firefox".into(),
                profile: None,
            })),
            bases: Some((
                intl_base.trim_end_matches('/').to_owned(),
                cn_base.trim_end_matches('/').to_owned(),
            )),
        })
    }

    fn base(&self, _region: Region) -> Option<&str> {
        #[cfg(test)]
        if let Some((international, china)) = &self.bases {
            return Some(match _region {
                Region::International => international,
                Region::ChinaMainland => china,
            });
        }
        None
    }

    fn dashboard_url(&self, region: Region) -> String {
        self.base(region)
            .map(|base| format!("{base}/dashboard"))
            .unwrap_or_else(|| region.dashboard().to_owned())
    }

    fn rpc_base(&self, region: Region) -> String {
        self.base(region)
            .map(str::to_owned)
            .unwrap_or_else(|| region.rpc_base().to_owned())
    }

    fn user_info_url(&self, region: Region) -> String {
        format!(
            "{}/tool/user/info.json",
            self.base(region).unwrap_or_else(|| region.origin())
        )
    }

    fn rpc_url(&self, region: Region) -> String {
        format!(
            "{}/data/api.json?action={}&product={RPC_PRODUCT}&api={ACTION}&_v=undefined",
            self.rpc_base(region),
            region.rpc_action()
        )
    }

    fn stores(&self) -> Vec<browser::Store> {
        self.browser_home
            .as_deref()
            .map(browser::stores_in)
            .unwrap_or_else(browser::stores)
    }

    async fn cookie_header(&self) -> Result<String, ProviderError> {
        let source = self.source.as_ref().ok_or(ProviderError::NoCredential)?;
        match source {
            session::Source::Pasted(_) => {
                let parsed = session::session(
                    self.browser_home.as_deref(),
                    self.storage.as_ref(),
                    source,
                    &[SESSION_COOKIE],
                    &cookie_query(),
                    Region::International.dashboard(),
                )
                .await?
                .ok_or(ProviderError::NoCredential)?;
                if header_is_authenticated(&parsed.header) {
                    Ok(parsed.header)
                } else {
                    Err(ProviderError::NoCredential)
                }
            }
            session::Source::Browser(selection) => {
                let now = Timestamp::now();
                let mut keyring_locked = false;
                for store in self.stores().into_iter().filter(|store| {
                    store.browser.slug == selection.browser
                        && selection
                            .profile
                            .as_ref()
                            .is_none_or(|profile| profile == &store.profile)
                }) {
                    let cookies = match store.cookies(&cookie_query(), self.storage.as_ref()).await
                    {
                        Ok(cookies) => cookies,
                        Err(browser::CookieError::KeyringLocked) => {
                            keyring_locked = true;
                            continue;
                        }
                        Err(_) => continue,
                    };
                    let live: Vec<_> = cookies
                        .into_iter()
                        .filter(|cookie| cookie.is_live(now))
                        .collect();
                    if cookies_are_authenticated(&live) {
                        return Ok(jar_header(&live));
                    }
                }
                if keyring_locked {
                    Err(ProviderError::KeyringLocked)
                } else {
                    Err(ProviderError::NoCredential)
                }
            }
        }
    }

    fn dashboard_request(
        &self,
        region: Region,
        cookie: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(self.dashboard_url(region))
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::COOKIE, cookie)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    fn user_info_request(
        &self,
        region: Region,
        cookie: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(self.user_info_url(region))
            .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, region.origin())
            .header(reqwest::header::REFERER, region.dashboard())
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn sec_token(&self, region: Region, cookie: &str) -> Result<String, Attempt> {
        let mut last_error = None;
        match self.dashboard_request(region, cookie) {
            Ok(request) => match super::validate_body(&self.client, request).await {
                Ok(body) => {
                    if let Some(token) = token_from_html(&body) {
                        return Ok(token);
                    }
                }
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error),
        }

        match self.user_info_request(region, cookie) {
            Ok(request) => match super::validate_body(&self.client, request).await {
                Ok(body) => {
                    if let Ok(document) = serde_json::from_str::<Value>(&body)
                        && let Some(token) =
                            find_text(&expand(document), &["secToken", "sec_token", "SEC_TOKEN"])
                    {
                        return Ok(token);
                    }
                }
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error),
        }

        if let Some(token) = cookie_value(cookie, "sec_token") {
            return Ok(token);
        }
        match last_error {
            Some(ProviderError::Credential { .. }) | None => Err(Attempt::LoginRequired),
            Some(error) => Err(Attempt::Exchange(error)),
        }
    }

    fn quota_request(
        &self,
        region: Region,
        cookie: &str,
        sec_token: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        static TRACE: AtomicU64 = AtomicU64::new(0);
        let mut cornerstone = Map::from_iter([
            (
                "feTraceId".into(),
                Value::String(format!(
                    "tidemark-{}-{}",
                    std::process::id(),
                    TRACE.fetch_add(1, Ordering::Relaxed)
                )),
            ),
            ("feURL".into(), Value::String(region.dashboard().into())),
            ("protocol".into(), Value::String("V2".into())),
            ("console".into(), Value::String("ONECONSOLE".into())),
            ("productCode".into(), Value::String("p_efm".into())),
            (
                "domain".into(),
                Value::String(region.console_domain().into()),
            ),
            (
                "consoleSite".into(),
                Value::String(region.console_site().into()),
            ),
            ("userNickName".into(), Value::String(String::new())),
            ("userPrincipalName".into(), Value::String(String::new())),
            ("xsp-lang".into(), Value::String("en-US".into())),
        ]);
        if let Some(cna) = cookie_value(cookie, "cna") {
            cornerstone.insert("X-Anonymous-Id".into(), Value::String(cna));
        }
        let params = serde_json::json!({
            "Api": ACTION,
            "V": "1.0",
            "Data": {
                "queryCodingPlanInstanceInfoRequest": {
                    "commodityCode": region.commodity_code(),
                    "onlyLatestOne": true,
                }
            },
            "region": region.region_id(),
            "cornerstoneParam": cornerstone,
        })
        .to_string();

        let mut request = self
            .client
            .post(self.rpc_url(region))
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, region.origin())
            .header(reqwest::header::REFERER, region.dashboard())
            .header("X-Request-With", "XMLHttpRequest");
        if let Some(csrf) =
            cookie_value(cookie, "login_aliyunid_csrf").or_else(|| cookie_value(cookie, "csrf"))
        {
            request = request
                .header("x-csrf-token", &csrf)
                .header("x-xsrf-token", csrf);
        }
        request
            .form(&[
                ("params", params.as_str()),
                ("region", region.region_id()),
                ("sec_token", sec_token),
            ])
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn attempt(&self, region: Region, cookie: &str) -> Result<Snapshot, Attempt> {
        let sec_token = self.sec_token(region, cookie).await?;
        let request = self
            .quota_request(region, cookie, &sec_token)
            .map_err(Attempt::Exchange)?;
        let body = super::validate_body(&self.client, request)
            .await
            .map_err(Attempt::Exchange)?;
        parse_quota_for_account(&body, Timestamp::now(), &self.tidemark_account)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let cookie = self.cookie_header().await?;
        let mut last = None;
        for region in [Region::International, Region::ChinaMainland] {
            let error = match self.attempt(region, &cookie).await {
                Ok(snapshot) => return Ok(snapshot),
                Err(attempt) => attempt.into_provider_error(),
            };
            if !region_shaped(&error) {
                return Err(error);
            }
            last = Some(error);
        }
        Err(last.expect("the second attempt ran"))
    }

    async fn validate_header(&self, cookie: &str) -> crate::browser::auth::Validation {
        use crate::browser::auth::Validation;
        let mut rejected = false;
        for region in [Region::International, Region::ChinaMainland] {
            match self.attempt(region, cookie).await {
                Ok(_) | Err(Attempt::NoQuota | Attempt::PlanWithoutFigures(_)) => {
                    return Validation::Ready;
                }
                Err(Attempt::Rejected { .. } | Attempt::LoginRequired) => rejected = true,
                Err(_) => {}
            }
        }
        if rejected {
            Validation::Rejected
        } else {
            Validation::Unreachable
        }
    }

    async fn validate_pasted_header(&self, header: &str) -> crate::browser::auth::Validation {
        use crate::browser::auth::Validation;
        let source = session::Source::Pasted(header.to_owned());
        match session::session(
            self.browser_home.as_deref(),
            self.storage.as_ref(),
            &source,
            &[SESSION_COOKIE],
            &cookie_query(),
            Region::International.dashboard(),
        )
        .await
        {
            Ok(Some(session)) if header_is_authenticated(&session.header) => {
                self.validate_header(&session.header).await
            }
            Ok(_) => Validation::Rejected,
            Err(_) => Validation::Unreachable,
        }
    }

    async fn inspect_sources(&self) -> Vec<AuthCandidate> {
        let now = Timestamp::now();
        let mut browsers: Vec<(browser::Browser, Vec<AuthCandidate>)> = Vec::new();
        for store in self.stores() {
            let state = match store.cookies(&cookie_query(), self.storage.as_ref()).await {
                Ok(cookies) => {
                    let live: Vec<_> = cookies
                        .into_iter()
                        .filter(|cookie| cookie.is_live(now))
                        .collect();
                    if !cookies_are_authenticated(&live) {
                        AuthCandidateState::Missing
                    } else {
                        validation_state(self.validate_header(&jar_header(&live)).await)
                    }
                }
                Err(browser::CookieError::KeyringLocked) => AuthCandidateState::WaitingForKeyring,
                Err(_) => AuthCandidateState::Unreachable,
            };
            let child = AuthCandidate {
                id: crate::browser::auth::Selection {
                    browser: store.browser.slug.into(),
                    profile: Some(store.profile.clone()),
                }
                .candidate_id(),
                title: store.profile,
                subtitle: None,
                state: state.as_wire().into(),
                children: Vec::new(),
            };
            match browsers.last_mut() {
                Some((browser, children)) if browser.slug == store.browser.slug => {
                    children.push(child);
                }
                _ => browsers.push((store.browser, vec![child])),
            }
        }
        let browser_sources = browsers
            .into_iter()
            .map(|(browser, children)| AuthCandidate {
                id: browser.slug.into(),
                title: browser.title.into(),
                subtitle: None,
                state: aggregate_state(&children).as_wire().into(),
                children,
            })
            .collect();
        session::modes(
            browser_sources,
            self.source.as_ref().and_then(session::Source::pasted),
            |header| async move { self.validate_pasted_header(&header).await },
        )
        .await
    }
}

impl fmt::Debug for Alibaba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Alibaba")
            .field("id", &PROVIDER_ID)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Provider for Alibaba {
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

fn cookies_are_authenticated(cookies: &[browser::Cookie]) -> bool {
    let names: BTreeSet<_> = cookies.iter().map(|cookie| cookie.name.as_str()).collect();
    names.contains(SESSION_COOKIE)
        && ACCOUNT_COOKIES
            .iter()
            .any(|account| names.contains(account))
}

fn header_is_authenticated(header: &str) -> bool {
    let names: BTreeSet<_> = cookie_pairs(header).map(|(name, _)| name).collect();
    names.contains(SESSION_COOKIE)
        && ACCOUNT_COOKIES
            .iter()
            .any(|account| names.contains(account))
}

fn jar_header(cookies: &[browser::Cookie]) -> String {
    let mut jar = BTreeMap::new();
    for cookie in cookies {
        jar.insert(cookie.name.as_str(), cookie.value.as_str());
    }
    jar.into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn cookie_pairs(header: &str) -> impl Iterator<Item = (&str, &str)> {
    header.split(';').filter_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        Some((name.trim(), value.trim()))
    })
}

fn cookie_value(header: &str, wanted: &str) -> Option<String> {
    cookie_pairs(header)
        .find(|(name, value)| *name == wanted && !value.is_empty())
        .map(|(_, value)| value.to_owned())
}

fn token_from_html(body: &str) -> Option<String> {
    for key in ["SEC_TOKEN", "secToken", "sec_token"] {
        let mut remaining = body;
        while let Some(position) = remaining.find(key) {
            let after = &remaining[position + key.len()..];
            let Some(colon) = after.find(':').filter(|colon| *colon <= 8) else {
                remaining = after;
                continue;
            };
            let value = after[colon + 1..].trim_start();
            if let Some(value) = value.strip_prefix("\\\"")
                && let Some(end) = value.find("\\\"")
                && end > 0
            {
                return Some(value[..end].to_owned());
            }
            if let Some(quote) = value
                .chars()
                .next()
                .filter(|quote| matches!(quote, '"' | '\''))
            {
                let value = &value[quote.len_utf8()..];
                if let Some(end) = value.find(quote)
                    && end > 0
                {
                    return Some(value[..end].to_owned());
                }
            }
            remaining = after;
        }
    }
    None
}

fn validation_state(validation: crate::browser::auth::Validation) -> AuthCandidateState {
    match validation {
        crate::browser::auth::Validation::Ready => AuthCandidateState::Ready,
        crate::browser::auth::Validation::Rejected => AuthCandidateState::Rejected,
        crate::browser::auth::Validation::Unreachable => AuthCandidateState::Unreachable,
        crate::browser::auth::Validation::Challenged => AuthCandidateState::Challenged,
    }
}

fn aggregate_state(children: &[AuthCandidate]) -> AuthCandidateState {
    let states = children.iter().filter_map(AuthCandidate::state);
    for candidate in [
        AuthCandidateState::Ready,
        AuthCandidateState::WaitingForKeyring,
        AuthCandidateState::Challenged,
        AuthCandidateState::Unreachable,
        AuthCandidateState::Rejected,
    ] {
        if states.clone().any(|state| state == candidate) {
            return candidate;
        }
    }
    AuthCandidateState::Missing
}
/// One region attempt's failure, classified before it becomes a card error.
enum Attempt {
    /// The HTTP exchange itself failed.
    Exchange(ProviderError),
    /// The gateway rejected the browser session: a numeric 401/403 envelope, or one whose
    /// message names an unauthorised call.
    Rejected {
        /// The status the envelope named, where it named one.
        status: u16,
    },
    /// The gateway answered a console sign-in, so this region did not accept the session.
    LoginRequired,
    /// The gateway named an API error of its own.
    Gateway(String),
    /// A recognised envelope with no quota windows in it — the shape the other region may
    /// still answer with figures in.
    NoQuota,
    /// A plan that is visibly active but names no figures: terminal here, where upstream
    /// keeps it as a card without numbers.
    PlanWithoutFigures(String),
    /// A body whose JSON means nothing to this parser.
    Malformed(String),
}

impl Attempt {
    fn into_provider_error(self) -> ProviderError {
        match self {
            Self::Exchange(error) => error,
            Self::Rejected { status } => ProviderError::Credential { status },
            Self::LoginRequired => ProviderError::Credential { status: 401 },
            Self::Gateway(message) => ProviderError::malformed(format!(
                "the Alibaba gateway reported an error: {message}"
            )),
            Self::NoQuota => ProviderError::malformed(
                "the Alibaba account has no active Coding Plan with quota windows",
            ),
            Self::PlanWithoutFigures(plan) => ProviderError::malformed(format!(
                "the Alibaba Coding Plan reports {plan} without any quota figures"
            )),
            Self::Malformed(detail) => ProviderError::malformed(detail),
        }
    }
}

/// Whether the other region is worth asking: transport, authentication, a missing endpoint,
/// or an authenticated account with no plan can all differ between the international and
/// China-mainland console deployments.
fn region_shaped(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport(_)
        | ProviderError::Credential { .. }
        | ProviderError::Http { status: 404, .. } => true,
        ProviderError::Malformed(message) => message.contains("no active Coding Plan"),
        _ => false,
    }
}

/// Turns a response body into the card: up to three windows and the plan's name.
///
/// Pure — the body and the clock decide everything — so every recorded envelope is
/// reachable from a test.
#[cfg(test)]
fn parse_quota(body: &str, now: Timestamp) -> Result<Snapshot, Attempt> {
    parse_quota_for_account(body, now, &AccountId::default())
}

fn parse_quota_for_account(
    body: &str,
    now: Timestamp,
    account_id: &AccountId,
) -> Result<Snapshot, Attempt> {
    let document: Value = serde_json::from_str(body).map_err(|error| {
        Attempt::Malformed(format!("not an Alibaba Coding Plan response: {error}"))
    })?;
    let payload = expand(document);

    // The gateway's own error envelope: a numeric status that is neither the `0` nor the
    // `200` a success carries.
    if let Some(status) = find_int(&payload, &["statusCode", "status_code", "code"])
        && status != 0
        && status != 200
    {
        let message = find_text(&payload, &["statusMessage", "status_msg", "message", "msg"])
            .unwrap_or_else(|| format!("status code {status}"));
        let lowered = message.to_lowercase();
        if status == 401 || status == 403 {
            return Err(Attempt::Rejected {
                status: status as u16,
            });
        }
        if lowered.contains("api key") || lowered.contains("unauthorized") {
            return Err(Attempt::Rejected { status: 401 });
        }
        return Err(Attempt::Gateway(message));
    }

    // A console sign-in envelope: key mode cannot read this account, here or anywhere.
    if let Some(code) = find_text(&payload, &["code", "status", "statusCode"]) {
        let lowered = code.to_lowercase();
        if lowered.contains("needlogin") || lowered.contains("login") {
            return Err(Attempt::LoginRequired);
        }
    }
    if let Some(message) = find_text(&payload, &["message", "msg", "statusMessage"]) {
        let lowered = message.to_lowercase();
        if lowered.contains("log in") || lowered.contains("login") {
            return Err(Attempt::LoginRequired);
        }
        if lowered.contains("console session")
            || lowered.contains("api key mode may be unavailable")
        {
            return Err(Attempt::LoginRequired);
        }
    }

    let instances = find_array(&payload, INSTANCE_KEYS);
    let selected = select_active_instance(&payload, now);
    let listed = instances
        .map(|list| list.iter().filter(|entry| entry.is_object()).count())
        .unwrap_or(0);
    // Several instances listed: the selected one owns the figures, and an expired
    // neighbour must not lend its own.
    let scoped = listed > 1 && selected.is_some_and(|info| active_score(info, now) > 0);

    let quota = if scoped {
        selected.and_then(find_quota)
    } else {
        selected
            .and_then(find_quota)
            .or_else(|| find_quota(&payload))
    };
    let Some(quota) = quota else {
        return Err(missing_figures(&payload, selected, now));
    };

    let five = Quota {
        used: any_int(quota, &["per5HourUsedQuota", "perFiveHourUsedQuota"]),
        total: any_int(quota, &["per5HourTotalQuota", "perFiveHourTotalQuota"]),
        reset: any_date(
            quota,
            &[
                "per5HourQuotaNextRefreshTime",
                "perFiveHourQuotaNextRefreshTime",
            ],
        ),
    };
    let week = Quota {
        used: any_int(quota, &["perWeekUsedQuota"]),
        total: any_int(quota, &["perWeekTotalQuota"]),
        reset: any_date(quota, &["perWeekQuotaNextRefreshTime"]),
    };
    let month = Quota {
        used: any_int(quota, &["perBillMonthUsedQuota", "perMonthUsedQuota"]),
        total: any_int(quota, &["perBillMonthTotalQuota", "perMonthTotalQuota"]),
        reset: any_date(
            quota,
            &[
                "perBillMonthQuotaNextRefreshTime",
                "perMonthQuotaNextRefreshTime",
            ],
        ),
    };
    if five.total.is_none() && week.total.is_none() && month.total.is_none() {
        return Err(missing_figures(&payload, selected, now));
    }

    let plan_name = selected
        .and_then(find_plan_name)
        .or_else(|| find_plan_name(&payload));

    let mut windows = Vec::new();
    for (title, seconds, figures) in [
        ("5h", FIVE_HOURS, five),
        ("7-day", WEEK, week),
        ("Monthly", MONTH, month),
    ] {
        if let Some(window) = figures.window(title, seconds) {
            windows.push(window);
        }
    }

    let mut details = Vec::new();
    if let Some(plan) = plan_name {
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
        captured_at: now,
        windows,
        details,
    })
}

/// The failure a body without figures produces: upstream's plan-without-numbers grace
/// names the plan it kept, the plain shape names nothing — and only the plain shape is
/// worth asking the other region about.
fn missing_figures(payload: &Value, selected: Option<&Value>, now: Timestamp) -> Attempt {
    match visible_active_plan(payload, selected, now) {
        Some(plan) => Attempt::PlanWithoutFigures(plan),
        None => Attempt::NoQuota,
    }
}

/// One window's used/total pair and its own reset, as the quota block reports it.
struct Quota {
    used: Option<i64>,
    total: Option<i64>,
    reset: Option<Timestamp>,
}

impl Quota {
    /// The card's window, when both figures are present and the total is a real one —
    /// upstream draws nothing from a partial pair either.
    fn window(&self, title: &str, seconds: u64) -> Option<Window> {
        let used = self.used?;
        let total = self.total?;
        if total <= 0 {
            return None;
        }
        let length = WindowLength::from_secs(seconds).expect("a fixed span");
        Some(Window {
            key: WindowKey::for_length(length),
            title: title.to_owned(),
            subtitle: Some(format!("{used} / {total} used")),
            used_percent: (used as f64).clamp(0.0, total as f64) / total as f64 * 100.0,
            resets_at: self.reset,
            length: Some(length),
        })
    }
}

/// Expands every string that itself parses as JSON, recursively: the China console
/// double-stringifies its payloads, and a key locked inside a quoted envelope is a key
/// the search would never see.
fn expand(value: Value) -> Value {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if (trimmed.starts_with('{') || trimmed.starts_with('['))
                && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
            {
                return expand(parsed);
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(expand).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, expand(value)))
                .collect(),
        ),
        other => other,
    }
}

/// The first value any of the keys names, wherever the envelope buried it. Every key is
/// honoured across the whole tree before the next one is, so a caller's priority order
/// survives the nesting.
fn find_in_tree<T>(
    value: &Value,
    keys: &[&str],
    coerce: &impl Fn(Option<&Value>) -> Option<T>,
) -> Option<T> {
    if let Some(map) = value.as_object() {
        for key in keys {
            if let Some(found) = coerce(map.get(*key)) {
                return Some(found);
            }
        }
        for nested in map.values() {
            if let Some(found) = find_in_tree(nested, keys, coerce) {
                return Some(found);
            }
        }
        return None;
    }
    if let Some(array) = value.as_array() {
        for nested in array {
            if let Some(found) = find_in_tree(nested, keys, coerce) {
                return Some(found);
            }
        }
    }
    None
}

fn find_int(value: &Value, keys: &[&str]) -> Option<i64> {
    find_in_tree(value, keys, &scalar_int)
}

fn find_text(value: &Value, keys: &[&str]) -> Option<String> {
    find_in_tree(value, keys, &scalar_text)
}

/// The first array any of the keys names, wherever the envelope buried it.
fn find_array<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    if let Some(map) = value.as_object() {
        for key in keys {
            if let Some(found) = map.get(*key).and_then(Value::as_array) {
                return Some(found);
            }
        }
        for nested in map.values() {
            if let Some(found) = find_array(nested, keys) {
                return Some(found);
            }
        }
        return None;
    }
    if let Some(array) = value.as_array() {
        for nested in array {
            if let Some(found) = find_array(nested, keys) {
                return Some(found);
            }
        }
    }
    None
}

/// The first object any of the keys names, wherever the envelope buried it.
fn find_dict<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    if let Some(map) = value.as_object() {
        for key in keys {
            if let Some(found) = map.get(*key).and_then(Value::as_object) {
                return Some(found);
            }
        }
        for nested in map.values() {
            if let Some(found) = find_dict(nested, keys) {
                return Some(found);
            }
        }
        return None;
    }
    if let Some(array) = value.as_array() {
        for nested in array {
            if let Some(found) = find_dict(nested, keys) {
                return Some(found);
            }
        }
    }
    None
}

/// The first object anywhere in the envelope that carries any of the keys at all — the
/// quota block when the gateway never named its container.
fn find_dict_with_any_key<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    if let Some(map) = value.as_object() {
        if keys.iter().any(|key| map.contains_key(*key)) {
            return Some(map);
        }
        for nested in map.values() {
            if let Some(found) = find_dict_with_any_key(nested, keys) {
                return Some(found);
            }
        }
        return None;
    }
    if let Some(array) = value.as_array() {
        for nested in array {
            if let Some(found) = find_dict_with_any_key(nested, keys) {
                return Some(found);
            }
        }
    }
    None
}

/// Reads the keys off one object in priority order — no descent, the tree walkers above
/// already found this object.
fn any_text(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| scalar_text(map.get(*key)))
}

fn any_int(map: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| scalar_int(map.get(*key)))
}

fn any_date(map: &Map<String, Value>, keys: &[&str]) -> Option<Timestamp> {
    keys.iter().find_map(|key| one_console_date(map.get(*key)))
}

fn any_bool(map: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| scalar_bool(map.get(*key)))
}

/// `OneConsoleJSON.string`: trimmed, and empty is nothing.
fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(raw) => {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        _ => None,
    }
}

/// `OneConsoleJSON.int`: whole numbers, truncated decimals, decimal strings.
fn scalar_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64)),
        Value::String(raw) => raw.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Upstream's `parseBool` spellings: JSON booleans, numbers, the words both consoles
/// write.
fn scalar_bool(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_f64().map(|value| value != 0.0),
        Value::String(raw) => match raw.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "active" | "valid" => Some(true),
            "false" | "0" | "no" | "inactive" | "invalid" | "expired" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `OneConsoleJSON.date`: epoch seconds or milliseconds, RFC 3339, and the bare
/// date-and-time spellings the consoles write — read as UTC, where upstream reads them
/// in the device's zone, an instant a card cannot depend on.
fn one_console_date(value: Option<&Value>) -> Option<Timestamp> {
    match value? {
        Value::Number(number) => {
            let seconds = number.as_f64().filter(|seconds| *seconds > 0.0)?;
            if seconds >= 1_000_000_000_000.0 {
                Timestamp::from_unix_millis(seconds as i64).ok()
            } else {
                Timestamp::from_unix(seconds as i64).ok()
            }
        }
        Value::String(raw) => date_text(raw.trim()),
        _ => None,
    }
}

fn date_text(raw: &str) -> Option<Timestamp> {
    if let Ok(moment) = OffsetDateTime::parse(raw, &Rfc3339) {
        return Timestamp::from_unix(moment.unix_timestamp()).ok();
    }
    let day_format = format_description::parse_borrowed::<1>("[year]-[month]-[day]").ok()?;
    if let Ok(day) = Date::parse(raw, &day_format) {
        return Timestamp::from_unix(day.midnight().assume_utc().unix_timestamp()).ok();
    }
    // The consoles' bare date-and-time spellings, upstream's formats in their order —
    // read here as UTC, where upstream reads them in the device's zone.
    for spelling in [
        "[year]-[month]-[day] [hour]:[minute]",
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    ] {
        let format = format_description::parse_borrowed::<1>(spelling).ok()?;
        if let Ok(moment) = PrimitiveDateTime::parse(raw, &format) {
            return Timestamp::from_unix(moment.assume_utc().unix_timestamp()).ok();
        }
    }
    None
}

/// The quota block: the named container when the envelope carries one, else whichever
/// nested object holds the per-window figures themselves.
fn find_quota(payload: &Value) -> Option<&Map<String, Value>> {
    find_dict(payload, QUOTA_CONTAINER_KEYS)
        .or_else(|| find_dict_with_any_key(payload, QUOTA_FIGURE_KEYS))
}

/// The instance whose figures this card reports: the one with the strongest active
/// signal when any signal is positive, the first listed otherwise. `None` when the
/// envelope lists no instances at all.
fn select_active_instance(payload: &Value, now: Timestamp) -> Option<&Value> {
    let instances = find_array(payload, INSTANCE_KEYS)?;
    let mut first = None;
    let mut best = None;
    let mut best_score = i32::MIN;
    for info in instances {
        if !info.is_object() {
            continue;
        }
        first = first.or(Some(info));
        let score = active_score(info, now);
        if score > best_score {
            best = Some(info);
            best_score = score;
        }
    }
    if best_score > 0 { best } else { first }
}

/// Upstream's active signal: a named status, else an explicit flag, else an end time
/// still ahead. An unrecognised status counts for nothing and falls through.
fn active_score(info: &Value, now: Timestamp) -> i32 {
    let Some(map) = info.as_object() else {
        return 0;
    };
    if let Some(status) = any_text(map, &["status", "instanceStatus"]) {
        match status.to_uppercase().as_str() {
            "VALID" | "ACTIVE" => return 3,
            "EXPIRED" | "INVALID" | "INACTIVE" | "DISABLED" | "TERMINATED" | "STOPPED" => {
                return -1;
            }
            _ => {}
        }
    }
    if let Some(flag) = any_bool(map, &["isActive", "active"]) {
        return if flag { 3 } else { -1 };
    }
    if let Some(expiry) = any_date(
        map,
        &["endTime", "periodEndTime", "expireTime", "expirationTime"],
    ) && expiry > now
    {
        return 1;
    }
    0
}

/// The plan's own name, upstream's candidate order: what the instance calls its plan,
/// then its instance or package name, then the same hunt across the whole envelope.
fn find_plan_name(payload: &Value) -> Option<String> {
    if let Some(infos) = find_array(payload, INSTANCE_KEYS) {
        for info in infos {
            let Some(map) = info.as_object() else {
                continue;
            };
            for keys in [
                &["planName", "plan_name"][..],
                &["instanceName", "instance_name"][..],
                &["packageName", "package_name"][..],
            ] {
                if let Some(name) = any_text(map, keys) {
                    return Some(name);
                }
            }
        }
    }
    find_text(
        payload,
        &["planName", "plan_name", "packageName", "package_name"],
    )
}

/// The plan name the fallback may still carry: an active-looking plan — by the envelope's
/// own status when it lists instances, by either signal otherwise — that names no
/// figures. The name is what the error tells the user; the figures it cannot invent.
fn visible_active_plan(
    payload: &Value,
    selected: Option<&Value>,
    now: Timestamp,
) -> Option<String> {
    let source = selected.unwrap_or(payload);
    let positive = if contains_instances(payload) {
        active_score(source, now) > 0
    } else {
        active_score(source, now) > 0 || active_score(payload, now) > 0
    };
    if !positive {
        return None;
    }
    find_plan_name(source).or_else(|| find_plan_name(payload))
}

/// Whether the envelope lists plan instances at all, an empty or malformed list not
/// counting — the gate upstream's fallback signal is read behind.
fn contains_instances(payload: &Value) -> bool {
    find_array(payload, INSTANCE_KEYS).is_some_and(|infos| infos.iter().any(Value::is_object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// The recorded intl-host body of `AlibabaCodingPlanUsageParsingTests::parses quota
    /// payload`: three windows, one instance, a zero status code.
    const INTL: &str = include_str!("../../../tests/fixtures/alibaba/intl.json");
    /// The recorded wrapped body of `parses wrapped JSON string payload`: the China
    /// console's double-stringified envelope, five-hour figures only.
    const CN: &str = include_str!("../../../tests/fixtures/alibaba/cn.json");
    #[derive(Debug)]
    struct NoKeyring;

    impl SafeStorage for NoKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> BoxFuture<'_, Result<Option<String>, SecretError>> {
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
        for (domain, name, value) in [
            (".aliyun.com", SESSION_COOKIE, "browser-session"),
            (".alibabacloud.com", "login_aliyunid_pk", "browser-account"),
            (".alibabacloud.com", "cna", "anonymous-id"),
        ] {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES (?1, ?2, ?3, '/', 0, 1, 0, 0, 0)",
                    (domain, name, value),
                )
                .expect("inserts a cookie");
        }
        home
    }
    /// A recorded console sign-in envelope: what a browser session cannot read past.
    const NEED_LOGIN: &str = r#"{
          "code": "ConsoleNeedLogin",
          "message": "You need to log in.",
          "requestId": "abc",
          "successResponse": false
        }"#;
    /// A recorded envelope with an instance but no figures anywhere.
    const NO_QUOTA: &str = r#"{
          "data": {
            "codingPlanInstanceInfos": [
              { "planName": "Alibaba Coding Plan Pro" }
            ]
          },
          "status_code": 0
        }"#;
    const COOKIE_HEADER: &str =
        "login_aliyunid_ticket=session; login_aliyunid_pk=account; csrf=csrf-value";
    const TOKEN_HTML: &str = r#"<script>window.bootstrap={"SEC_TOKEN":"sec-token"}</script>"#;
    const DASHBOARD_REQUEST: &str = "GET /dashboard";
    const USER_INFO_REQUEST: &str = "GET /tool/user/info.json";
    const INTL_REQUEST: &str = "POST /data/api.json?action=IntlBroadScopeAspnGateway&product=sfm_bailian&api=zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2&_v=undefined";
    const CN_REQUEST: &str = "POST /data/api.json?action=BroadScopeAspnGateway&product=sfm_bailian&api=zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2&_v=undefined";

    /// A loopback server answering the given routes in order, asserting each request
    /// opens with its expected request line and handing the raw exchange back.
    fn chained_server(
        routes: Vec<(&'static str, u16, String)>,
    ) -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = mpsc::channel();
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
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = value.trim().parse().expect("numeric content length");
                    }
                    request.push_str(&line);
                }
                if content_length > 0 {
                    let mut body = vec![0; content_length];
                    reader.read_exact(&mut body).expect("reads request body");
                    request.push_str("\r\n");
                    request.push_str(std::str::from_utf8(&body).expect("UTF-8 request body"));
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

    fn route(expected: &'static str, status: u16, body: &str) -> (&'static str, u16, String) {
        (expected, status, body.to_owned())
    }

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn fetch(provider: &Alibaba) -> Result<Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    /// The pure parse as the fetch sees its outcome: the attempt error already converted
    /// to the error the card reports.
    fn parse(body: &str, now: Timestamp) -> Result<Snapshot, ProviderError> {
        parse_quota(body, now).map_err(Attempt::into_provider_error)
    }

    #[test]
    fn the_recorded_intl_body_draws_three_windows_and_the_plan() {
        let snapshot = parse(INTL, at(1_700_000_000)).expect("parses the intl response");

        assert_eq!(snapshot.details.len(), 1);
        let plan = &snapshot.details[0];
        assert_eq!(plan.title, DetailSection::PLAN);
        assert_eq!(plan.rows[0].label, "Plan");
        assert_eq!(plan.rows[0].value, "Alibaba Coding Plan Pro");

        assert_eq!(snapshot.windows.len(), 3);
        let five = &snapshot.windows[0];
        assert_eq!(
            five.key,
            WindowKey::for_length(WindowLength::from_secs(18_000).expect("a fixed span"))
        );
        assert_eq!(five.title, "5h");
        assert_eq!(
            five.length,
            Some(WindowLength::from_secs(18_000).expect("a fixed span"))
        );
        assert!((five.used_percent - 5.2).abs() < 0.000_001);
        assert_eq!(five.subtitle.as_deref(), Some("52 / 1000 used"));
        assert_eq!(five.resets_at, Some(at(1_700_000_300)));

        let week = &snapshot.windows[1];
        assert_eq!(week.title, "7-day");
        assert!((week.used_percent - 16.0).abs() < 0.000_001);
        assert_eq!(week.subtitle.as_deref(), Some("800 / 5000 used"));
        assert_eq!(week.resets_at, Some(at(1_700_100_000)));

        let month = &snapshot.windows[2];
        assert_eq!(month.title, "Monthly");
        assert!((month.used_percent - 6.0).abs() < 0.000_001);
        assert_eq!(month.resets_at, Some(at(1_701_000_000)));
    }

    #[test]
    fn the_recorded_cn_body_opens_the_double_stringified_envelope() {
        let snapshot = parse(CN, at(1_700_000_000)).expect("parses the cn response");

        assert_eq!(snapshot.windows.len(), 1);
        let five = &snapshot.windows[0];
        assert!(five.used_percent.abs() < 0.000_001);
        assert_eq!(five.subtitle.as_deref(), Some("0 / 1000 used"));
        assert_eq!(five.resets_at, Some(at(1_700_000_300)));
        assert_eq!(snapshot.details[0].rows[0].value, "Coding Plan Lite");
    }

    #[test]
    fn the_active_instance_wins_when_one_of_several_is_expired() {
        // The recorded `multi instance quota payload uses selected active instance plan
        // name` body: an expired instance still carrying figures must not be read.
        let snapshot = parse(
            r#"{
          "data": {
            "codingPlanInstanceInfos": [
              {
                "planName": "Expired Starter",
                "status": "EXPIRED",
                "endTime": "2025-04-01 17:00",
                "codingPlanQuotaInfo": {
                  "per5HourUsedQuota": 7,
                  "per5HourTotalQuota": 100,
                  "per5HourQuotaNextRefreshTime": 1700000100000
                }
              },
              {
                "planName": "Active Pro",
                "status": "VALID",
                "codingPlanQuotaInfo": {
                  "per5HourUsedQuota": 52,
                  "per5HourTotalQuota": 1000,
                  "per5HourQuotaNextRefreshTime": 1700000300000
                }
              }
            ]
          },
          "status_code": 0
        }"#,
            at(1_700_000_000),
        )
        .expect("parses");

        assert_eq!(snapshot.windows.len(), 1);
        assert!((snapshot.windows[0].used_percent - 5.2).abs() < 0.000_001);
        assert_eq!(snapshot.windows[0].resets_at, Some(at(1_700_000_300)));
        assert_eq!(snapshot.details[0].rows[0].value, "Active Pro");
    }

    #[test]
    fn an_active_instance_without_figures_does_not_borrow_another_instances() {
        // The recorded `active instance without quota does not borrow quota from another
        // instance` body: scoping to the selected instance is what keeps the expired one's
        // figures off the card — and leaves nothing to draw.
        let error = parse(
            r#"{
          "data": {
            "codingPlanInstanceInfos": [
              {
                "planName": "Expired Starter",
                "status": "EXPIRED",
                "endTime": "2025-04-01 17:00",
                "codingPlanQuotaInfo": {
                  "per5HourUsedQuota": 7,
                  "per5HourTotalQuota": 100,
                  "per5HourQuotaNextRefreshTime": 1700000100000
                }
              },
              {
                "planName": "Active Pro",
                "status": "VALID"
              }
            ]
          },
          "status_code": 0
        }"#,
            at(1_700_000_000),
        )
        .expect_err("the selected instance names no figures");

        let rendered = error.to_string();
        assert!(rendered.contains("Active Pro"), "{rendered}");
        assert!(rendered.contains("without any quota figures"), "{rendered}");
        assert!(!region_shaped(&error), "{error}");
    }

    #[test]
    fn a_body_with_no_quota_and_no_active_signal_names_the_missing_plan() {
        // The shape the other region may still answer with figures in, which is why the
        // fetch retries it there.
        let error = parse(NO_QUOTA, at(1_700_000_000)).expect_err("no window can be drawn");

        assert!(
            error.to_string().contains("no active Coding Plan"),
            "{error}"
        );
        assert!(region_shaped(&error), "{error}");
    }

    #[test]
    fn a_visible_active_plan_without_figures_is_malformed_here() {
        // Upstream keeps this shape as a plan card without numbers; a Tidemark card cannot
        // be drawn unnumbered.
        let error = parse(
            r#"{
          "data": {
            "codingPlanInstanceInfos": [
              {
                "planName": "Coding Plan Lite",
                "status": "VALID",
                "planUsage": "0%",
                "endTime": "2026-04-01 17:00"
              }
            ]
          },
          "status_code": 0
        }"#,
            at(1_700_000_000),
        )
        .expect_err("no figures");

        assert!(
            error.to_string().contains("without any quota figures"),
            "{error}"
        );
        assert!(!region_shaped(&error), "{error}");
    }

    #[test]
    fn a_console_login_envelope_rejects_the_browser_session() {
        let error =
            parse(NEED_LOGIN, at(1_700_000_000)).expect_err("the gateway wants a console session");

        assert!(matches!(error, ProviderError::Credential { status: 401 }));
        assert!(region_shaped(&error), "{error}");
    }

    #[test]
    fn a_numeric_status_gate_maps_to_its_error_kind() {
        let rejected = parse(
            r#"{"statusCode":401,"message":"unauthorized"}"#,
            at(1_700_000_000),
        )
        .expect_err("the session was rejected");
        assert!(
            matches!(rejected, ProviderError::Credential { status: 401 }),
            "{rejected}"
        );
        assert!(region_shaped(&rejected));

        let gateway = parse(
            r#"{"code":500001,"message":"InternalError."}"#,
            at(1_700_000_000),
        )
        .expect_err("the gateway failed");
        assert!(gateway.to_string().contains("InternalError."), "{gateway}");
        assert!(!region_shaped(&gateway), "{gateway}");
    }

    #[test]
    fn the_spec_is_cookie_only_and_builds_without_a_stored_secret() {
        assert_eq!(SPEC.credential, CredentialKind::External);
        assert_eq!(
            SPEC.options
                .iter()
                .map(|option| option.name)
                .collect::<Vec<_>>(),
            ["auth-browser", "auth-profile"]
        );
        assert!(
            (SPEC.build)(
                AccountId::default(),
                Credential::new(String::new()),
                &Options::new()
            )
            .is_ok(),
            "a browser-session provider is built before a source is selected"
        );
    }

    #[test]
    fn a_signed_in_firefox_profile_is_offered_without_exposing_its_cookie() {
        let home = gecko_home();
        let (base, requests, server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(INTL_REQUEST, 200, INTL),
        ]);
        let provider = Alibaba::for_browser_test(
            home.path(),
            Arc::new(NoKeyring),
            &base,
            "http://127.0.0.1:9",
        )
        .expect("builds");

        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.inspect_auth_sources())
            .expect("inspection succeeds");
        server.join().expect("server exits");

        let browsers = report
            .iter()
            .find(|candidate| candidate.id == session::BROWSER_SOURCE)
            .expect("browser mode");
        assert_eq!(browsers.state(), Some(AuthCandidateState::Ready));
        let firefox = browsers
            .children
            .iter()
            .find(|candidate| candidate.id == "firefox")
            .expect("Firefox");
        assert_eq!(firefox.state(), Some(AuthCandidateState::Ready));
        let quota = requests.into_iter().last().expect("quota request");
        assert!(quota.contains("login_aliyunid_ticket=browser-session"));
        assert!(quota.contains("login_aliyunid_pk=browser-account"));
    }

    #[test]
    fn the_international_sec_token_fallback_uses_the_model_studio_gateway() {
        let provider = Alibaba::new(&Options::new()).expect("builds");
        assert_eq!(
            provider.user_info_url(Region::International),
            "https://modelstudio.console.alibabacloud.com/tool/user/info.json"
        );
    }

    #[test]
    fn pasted_cookie_header_lines_are_normalized_before_validation() {
        let pasted = format!("Cookie: {COOKIE_HEADER}");
        let (base, requests, server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(INTL_REQUEST, 200, INTL),
        ]);
        let mut provider = Alibaba::for_test(&base, "http://127.0.0.1:9", &pasted).expect("builds");
        let empty_home = crate::browser::tests::TestHome::new();
        provider.browser_home = Some(empty_home.path().to_path_buf());

        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.inspect_auth_sources())
            .expect("inspection succeeds");
        server.join().expect("server exits");

        let paste = report
            .iter()
            .find(|candidate| candidate.id == session::PASTE_SOURCE)
            .expect("paste mode");
        assert_eq!(paste.state(), Some(AuthCandidateState::Ready));
        let dashboard = requests.recv().expect("dashboard request");
        assert!(
            dashboard.contains(&format!("cookie: {COOKIE_HEADER}\r\n")),
            "{dashboard}"
        );
        assert!(!dashboard.contains("cookie: Cookie:"), "{dashboard}");
    }
    #[test]
    fn the_quota_request_uses_the_browser_session_instead_of_api_key_headers() {
        let cookie = "login_aliyunid_ticket=session; login_aliyunid_pk=account; csrf=csrf-value";
        let provider =
            Alibaba::for_test("http://127.0.0.1:9", "http://127.0.0.1:9", cookie).expect("builds");
        let request = provider
            .quota_request(Region::International, cookie, "secret-token")
            .expect("builds");

        assert!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
        assert!(request.headers().get("x-dashscope-api-key").is_none());
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::COOKIE)
                .expect("cookie header"),
            cookie
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .expect("form content type"),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            request
                .headers()
                .get("x-request-with")
                .expect("OneConsole request marker"),
            "XMLHttpRequest"
        );
        assert_eq!(
            request.headers().get("x-csrf-token").expect("CSRF token"),
            "csrf-value"
        );
        let body = std::str::from_utf8(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("in-memory form"),
        )
        .expect("UTF-8 form");
        assert!(body.contains("sec_token=secret-token"), "{body}");
        assert!(body.contains("region=ap-southeast-1"), "{body}");
        assert!(body.contains("onlyLatestOne%22%3Atrue"), "{body}");
        assert!(body.contains("cornerstoneParam"), "{body}");
    }

    #[test]
    fn the_full_cookie_flow_bootstraps_sec_token_and_parses_quota() {
        let (base, requests, server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(INTL_REQUEST, 200, INTL),
        ]);
        let provider =
            Alibaba::for_test(&base, "http://127.0.0.1:9", COOKIE_HEADER).expect("builds");

        let snapshot = fetch(&provider).expect("the authenticated console answers quota");
        server.join().expect("server exits");

        let dashboard = requests.recv().expect("dashboard request");
        assert!(
            dashboard.contains(&format!("cookie: {COOKIE_HEADER}")),
            "{dashboard}"
        );
        let quota = requests.recv().expect("quota request");
        assert!(quota.starts_with(INTL_REQUEST), "{quota}");
        assert!(
            quota.contains(&format!("cookie: {COOKIE_HEADER}")),
            "{quota}"
        );
        assert!(quota.contains("sec_token=sec-token"), "{quota}");
        assert!(quota.contains("onlyLatestOne%22%3Atrue"), "{quota}");
        assert!(!quota.contains("authorization:"), "{quota}");
        assert_eq!(snapshot.windows.len(), 3);
    }

    #[test]
    fn user_info_supplies_sec_token_when_the_dashboard_does_not_embed_it() {
        let (base, requests, server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, "<html></html>"),
            route(USER_INFO_REQUEST, 200, r#"{"secToken":"from-user-info"}"#),
            route(INTL_REQUEST, 200, INTL),
        ]);
        let provider =
            Alibaba::for_test(&base, "http://127.0.0.1:9", COOKIE_HEADER).expect("builds");

        let snapshot = fetch(&provider).expect("the user-info token authenticates quota");
        server.join().expect("server exits");

        assert!(
            requests
                .recv()
                .expect("dashboard")
                .starts_with(DASHBOARD_REQUEST)
        );
        assert!(
            requests
                .recv()
                .expect("user info")
                .starts_with(USER_INFO_REQUEST)
        );
        let quota = requests.recv().expect("quota");
        assert!(quota.contains("sec_token=from-user-info"), "{quota}");
        assert_eq!(snapshot.windows.len(), 3);
    }

    #[test]
    fn a_dead_international_console_retries_the_cookie_flow_in_china() {
        let doomed = TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead = doomed.local_addr().expect("address");
        drop(doomed);

        let (base, requests, server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(CN_REQUEST, 200, INTL),
        ]);
        let provider =
            Alibaba::for_test(&format!("http://{dead}"), &base, COOKIE_HEADER).expect("builds");

        let snapshot = fetch(&provider).expect("the China console answers");
        server.join().expect("server exits");

        assert!(
            requests
                .recv()
                .expect("dashboard")
                .starts_with(DASHBOARD_REQUEST)
        );
        let quota = requests.recv().expect("quota");
        assert!(quota.starts_with(CN_REQUEST), "{quota}");
        assert!(quota.contains("region=cn-beijing"), "{quota}");
        assert_eq!(snapshot.windows.len(), 3);
    }

    #[test]
    fn console_login_envelopes_reject_the_browser_session() {
        let routes = || {
            vec![
                route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
                route(INTL_REQUEST, 200, NEED_LOGIN),
            ]
        };
        let (intl, intl_requests, intl_server) = chained_server(routes());
        let (cn, cn_requests, cn_server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(CN_REQUEST, 200, NEED_LOGIN),
        ]);
        let provider = Alibaba::for_test(&intl, &cn, COOKIE_HEADER).expect("builds");

        let error = fetch(&provider).expect_err("neither console accepts the session");
        intl_server.join().expect("intl server exits");
        cn_server.join().expect("cn server exits");

        assert!(matches!(error, ProviderError::Credential { status: 401 }));
        assert_eq!(intl_requests.into_iter().count(), 2);
        assert_eq!(cn_requests.into_iter().count(), 2);
    }

    #[test]
    fn an_authenticated_account_without_a_plan_says_so_after_both_regions() {
        let (intl, intl_requests, intl_server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(INTL_REQUEST, 200, NO_QUOTA),
        ]);
        let (cn, cn_requests, cn_server) = chained_server(vec![
            route(DASHBOARD_REQUEST, 200, TOKEN_HTML),
            route(CN_REQUEST, 200, NO_QUOTA),
        ]);
        let provider = Alibaba::for_test(&intl, &cn, COOKIE_HEADER).expect("builds");

        let error = fetch(&provider).expect_err("neither account view has a plan");
        intl_server.join().expect("intl server exits");
        cn_server.join().expect("cn server exits");

        assert!(
            error.to_string().contains("no active Coding Plan"),
            "{error}"
        );
        assert_eq!(intl_requests.into_iter().count(), 2);
        assert_eq!(cn_requests.into_iter().count(), 2);
    }
}
