//! The Alibaba Coding Plan quota, read with a DashScope API key.
//!
//! One POST to the Model Studio gateway's `queryCodingPlanInstanceInfoV2` RPC answers with
//! the plan's three allowance windows — five hours, a week, a billing month — as used and
//! total request figures with their own reset instants, plus the plan's display name. The
//! same RPC lives on two gateways: the international one
//! (`modelstudio.console.alibabacloud.com`) and the China-mainland one
//! (`bailian.console.aliyun.com`), and an account lives on one or the other. A first POST
//! to the international host is therefore retried whole against the China-mainland one
//! when the failure says "region", not "account": the host unreachable, the key rejected,
//! an HTTP 404, or a well-formed envelope with no quota windows in it. A console sign-in
//! envelope, a gateway error message and unparseable JSON are terminal — the other region
//! would answer the same thing. The key travels in both shapes the gateway accepts,
//! `Authorization: Bearer` and `X-DashScope-API-Key`, and the request carries the `Origin`
//! and `Referer` the console itself would send, as upstream's client sends them; the
//! browser `User-Agent` upstream also sends stays off, the shared client owning identity.
//!
//! The envelope is not trusted to keep its shape. The China console double-stringifies its
//! payloads — `successResponse.body` holding a whole JSON document as a string — so every
//! string that itself parses as JSON is expanded in place before the keys are read, and
//! the keys are then searched wherever they hide: `codingPlanQuotaInfo` by name, the
//! per-window figures by their `per5Hour*`/`perWeek*`/`perBillMonth*` spellings (snake_case
//! aliases included), the plan name across the plan instances and the envelope both. When
//! several instances are listed, the active one — a `VALID`/`ACTIVE` status, an `isActive`
//! flag, or an end time still ahead — owns the quota and the name; an expired instance
//! must not lend its figures to the card.
//!
//! Not ported, on purpose: upstream's cookie/OneConsole mode with its `SEC_TOKEN`
//! bootstrap — this port is key mode only — and a pinned region, the two gateways being
//! tried in a fixed order instead of chosen. Two upstream graces are also not ported. A
//! five-hour reset that is not in the future upstream shifts itself forward by the
//! window's length; a Tidemark card never invents a reset instant, so an overdue reset is
//! drawn as the overdue instant it is. And a plan that is visibly active but reports no
//! figures upstream keeps as a non-quantitative card; here a card without a number is
//! malformed, the same call `Grok` makes.

use super::{HandSpec, Options, ProviderError, redact_query};
use crate::providers::{BoxFuture, Credential, Provider};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};
use time::{
    Date, OffsetDateTime, PrimitiveDateTime, format_description,
    format_description::well_known::Rfc3339,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "alibaba";

/// The gateway RPC both regions serve.
const ACTION: &str = "zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2";
const PRODUCT: &str = "broadscope-bailian";
const API: &str = "queryCodingPlanInstanceInfoV2";
/// The international gateway, tried first.
const INTL_BASE: &str = "https://modelstudio.console.alibabacloud.com";
/// The China-mainland gateway a region-shaped failure is retried against.
const CN_BASE: &str = "https://bailian.console.aliyun.com";

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
    credential: CredentialKind::Key,
    credential_hint: "A DashScope API key (Model Studio console).",
    options: &[],
    build,
};

fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    if credential.expose().trim().is_empty() {
        return Err(ProviderError::Local(
            "an empty key is not a DashScope API key; paste one from the Model Studio console"
                .into(),
        ));
    }
    Ok(Arc::new(Alibaba::new(credential.expose())?))
}

/// One of the two gateways the same RPC lives on, with everything about the request the
/// region changes: the query's region id, the commodity in the body, the console headers.
/// No user-facing choice — the gateways are tried in a fixed order, international first.
#[derive(Debug, Clone, Copy)]
enum Region {
    International,
    ChinaMainland,
}

impl Region {
    /// The `currentRegionId` the RPC expects, upstream's region ids verbatim.
    fn region_id(self) -> &'static str {
        match self {
            Self::International => "ap-southeast-1",
            Self::ChinaMainland => "cn-beijing",
        }
    }

    /// The console host the `Origin` names.
    fn gateway(self) -> &'static str {
        match self {
            Self::International => "https://modelstudio.console.alibabacloud.com",
            Self::ChinaMainland => "https://bailian.console.aliyun.com",
        }
    }

    /// The console page the `Referer` names, upstream's dashboard URL verbatim.
    fn referer(self) -> &'static str {
        match self {
            Self::International => {
                "https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=coding-plan#/efm/coding_plan"
            }
            Self::ChinaMainland => {
                "https://bailian.console.aliyun.com/cn-beijing/?tab=model#/efm/coding_plan"
            }
        }
    }

    /// Which plan catalogue the region sells.
    fn commodity_code(self) -> &'static str {
        match self {
            Self::International => "sfm_codingplan_public_intl",
            Self::ChinaMainland => "sfm_codingplan_public_cn",
        }
    }

    /// The fixed body the RPC takes: which commodity to look up, nothing else.
    fn body(self) -> String {
        serde_json::json!({
            "queryCodingPlanInstanceInfoRequest": { "commodityCode": self.commodity_code() },
        })
        .to_string()
    }
}

/// One DashScope key against the Coding Plan RPC.
pub struct Alibaba {
    client: reqwest::Client,
    key: String,
    /// The two gateways, kept as fields so a test can point each at a loopback.
    intl_base: String,
    cn_base: String,
}

impl Alibaba {
    /// Builds the account against the real gateways.
    pub fn new(key: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            client: super::http::client()?,
            key: key.trim().to_owned(),
            intl_base: INTL_BASE.to_owned(),
            cn_base: CN_BASE.to_owned(),
        })
    }

    #[cfg(test)]
    fn for_test(intl_base: &str, cn_base: &str, key: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            client: super::http::client()?,
            key: key.to_owned(),
            intl_base: intl_base.trim_end_matches('/').to_owned(),
            cn_base: cn_base.trim_end_matches('/').to_owned(),
        })
    }

    /// The region's POST: the RPC's fixed path and query, both key headers, and the
    /// console's own `Origin` and `Referer` for the region.
    fn post(&self, region: Region) -> Result<reqwest::Request, ProviderError> {
        let base = match region {
            Region::International => &self.intl_base,
            Region::ChinaMainland => &self.cn_base,
        };
        self.client
            .post(format!(
                "{base}/data/api.json?action={ACTION}&product={PRODUCT}&api={API}&currentRegionId={}",
                region.region_id()
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(&self.key)
            .header("X-DashScope-API-Key", &self.key)
            .header(reqwest::header::ORIGIN, region.gateway())
            .header(reqwest::header::REFERER, region.referer())
            .body(region.body())
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let mut last = None;
        for region in [Region::International, Region::ChinaMainland] {
            let error = match self.attempt(region).await {
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

    /// One region's whole exchange: request, status mapping, envelope parse.
    async fn attempt(&self, region: Region) -> Result<Snapshot, Attempt> {
        let request = self.post(region).map_err(Attempt::Exchange)?;
        let body = super::request(PROVIDER_ID, &self.client, request)
            .await
            .map_err(Attempt::Exchange)?;
        parse_quota(&body, Timestamp::now())
    }
}

impl fmt::Debug for Alibaba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Alibaba")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Alibaba {
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

/// One attempt's failure, in the gateway's own vocabulary — what the failover reads
/// before the error becomes the card's [`ProviderError`].
enum Attempt {
    /// The exchange itself failed: transport, a rejected key, an HTTP status.
    Exchange(ProviderError),
    /// The gateway rejected the key: a numeric 401/403 envelope, or one whose message
    /// names the key or an unauthorised call.
    Rejected {
        /// The status the envelope named, where it named one.
        status: u16,
    },
    /// The gateway answered a console sign-in: key mode cannot read this account here,
    /// and the other region would say the same.
    KeyUnavailable,
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
            Self::KeyUnavailable => ProviderError::malformed(
                "the Alibaba gateway answered a console sign-in: Coding Plan quota is not \
                 available through an API key for this account or region",
            ),
            Self::Gateway(message) => ProviderError::malformed(format!(
                "the Alibaba gateway reported an error: {message}"
            )),
            Self::NoQuota => {
                ProviderError::malformed("the Alibaba Coding Plan response named no quota windows")
            }
            Self::PlanWithoutFigures(plan) => ProviderError::malformed(format!(
                "the Alibaba Coding Plan reports {plan} without any quota figures"
            )),
            Self::Malformed(detail) => ProviderError::malformed(detail),
        }
    }
}

/// Whether the other region is worth asking: the failure says something about *where*,
/// not about the account. A rejected key is retried there, exactly as upstream retries
/// its invalid-credentials error; an unparseable body or a console sign-in envelope is
/// not, and neither is a gateway error message.
fn region_shaped(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport(_)
        | ProviderError::Credential { .. }
        | ProviderError::Http { status: 404, .. } => true,
        ProviderError::Malformed(message) => message.contains("named no quota windows"),
        _ => false,
    }
}

/// Turns a response body into the card: up to three windows and the plan's name.
///
/// Pure — the body and the clock decide everything — so every recorded envelope is
/// reachable from a test.
fn parse_quota(body: &str, now: Timestamp) -> Result<Snapshot, Attempt> {
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
            return Err(Attempt::KeyUnavailable);
        }
    }
    if let Some(message) = find_text(&payload, &["message", "msg", "statusMessage"]) {
        let lowered = message.to_lowercase();
        if lowered.contains("log in") || lowered.contains("login") {
            return Err(Attempt::KeyUnavailable);
        }
        if lowered.contains("console session")
            || lowered.contains("api key mode may be unavailable")
        {
            return Err(Attempt::KeyUnavailable);
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
        account: AccountId::default(),
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// The recorded intl-host body of `AlibabaCodingPlanUsageParsingTests::parses quota
    /// payload`: three windows, one instance, a zero status code.
    const INTL: &str = include_str!("../../../tests/fixtures/alibaba/intl.json");
    /// The recorded wrapped body of `parses wrapped JSON string payload`: the China
    /// console's double-stringified envelope, five-hour figures only.
    const CN: &str = include_str!("../../../tests/fixtures/alibaba/cn.json");
    /// A recorded console sign-in envelope: what key mode cannot read past.
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

    const INTL_REQUEST: &str = "POST /data/api.json?action=zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2&product=broadscope-bailian&api=queryCodingPlanInstanceInfoV2&currentRegionId=ap-southeast-1";
    const CN_REQUEST: &str = "POST /data/api.json?action=zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2&product=broadscope-bailian&api=queryCodingPlanInstanceInfoV2&currentRegionId=cn-beijing";

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
    fn a_body_with_no_quota_and_no_active_signal_is_a_region_shaped_malformed() {
        // The shape the other region may still answer with figures in, which is why the
        // fetch retries it there.
        let error = parse(NO_QUOTA, at(1_700_000_000)).expect_err("no window can be drawn");

        assert!(
            error.to_string().contains("named no quota windows"),
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
    fn a_console_login_envelope_refuses_an_api_key_read() {
        let error =
            parse(NEED_LOGIN, at(1_700_000_000)).expect_err("the gateway wants a console session");

        assert!(error.to_string().contains("API key"), "{error}");
        assert!(!region_shaped(&error), "{error}");
    }

    #[test]
    fn a_numeric_status_gate_maps_to_its_error_kind() {
        let rejected = parse(
            r#"{"statusCode":401,"message":"unauthorized"}"#,
            at(1_700_000_000),
        )
        .expect_err("the key was rejected");
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
    fn an_empty_key_is_refused_at_build() {
        let error = (SPEC.build)(Credential::new("   "), &Options::new())
            .expect_err("an empty key is not a credential");
        assert!(
            matches!(error, ProviderError::Local(ref message) if message.contains("key")),
            "{error:?}"
        );

        assert!(
            (SPEC.build)(Credential::new("cpk-live"), &Options::new()).is_ok(),
            "a pasted key builds"
        );
    }

    #[test]
    fn the_international_request_carries_both_key_headers_and_the_region() {
        let provider = Alibaba::for_test("http://127.0.0.1:9", "http://127.0.0.1:9", "cpk-test")
            .expect("builds");
        let request = provider.post(Region::International).expect("builds");

        assert_eq!(request.method(), reqwest::Method::POST);
        let url = request.url().as_str();
        assert!(
            url.starts_with("http://127.0.0.1:9/data/api.json?"),
            "{url}"
        );
        assert!(
            url.contains(
                "action=zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2"
            ),
            "{url}"
        );
        assert!(url.contains("product=broadscope-bailian"), "{url}");
        assert!(url.contains("api=queryCodingPlanInstanceInfoV2"), "{url}");
        assert!(url.contains("currentRegionId=ap-southeast-1"), "{url}");

        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer cpk-test"
        );
        assert_eq!(
            request
                .headers()
                .get("x-dashscope-api-key")
                .expect("present"),
            "cpk-test"
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .expect("present"),
            "application/json"
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ORIGIN)
                .expect("present"),
            "https://modelstudio.console.alibabacloud.com"
        );
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::REFERER)
                .expect("present"),
            "https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=coding-plan#/efm/coding_plan"
        );

        let body = request
            .body()
            .expect("present")
            .as_bytes()
            .expect("in memory");
        assert_eq!(
            body,
            br#"{"queryCodingPlanInstanceInfoRequest":{"commodityCode":"sfm_codingplan_public_intl"}}"#
        );
    }

    #[test]
    fn a_dead_international_host_sends_the_whole_post_to_the_china_host() {
        // A bound-then-dropped listener: the port refuses, which is the transport failure
        // the failover exists for.
        let doomed = TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead = doomed.local_addr().expect("address");
        drop(doomed);

        let (base, requests, server) = chained_server(vec![route(CN_REQUEST, 200, INTL)]);
        let provider =
            Alibaba::for_test(&format!("http://{dead}"), &base, "cpk-live").expect("builds");

        let snapshot = fetch(&provider).expect("the china host answers");
        server.join().expect("server exits");

        let request = requests.recv().expect("china request");
        assert!(
            request.contains("authorization: Bearer cpk-live"),
            "{request}"
        );
        assert!(
            request.contains("x-dashscope-api-key: cpk-live"),
            "{request}"
        );
        assert!(
            request.contains("origin: https://bailian.console.aliyun.com"),
            "{request}"
        );
        assert!(
            request.contains(
                "referer: https://bailian.console.aliyun.com/cn-beijing/?tab=model#/efm/coding_plan"
            ),
            "{request}"
        );
        assert_eq!(snapshot.windows.len(), 3);
    }

    #[test]
    fn a_rejected_key_on_the_international_host_is_tried_against_the_china_host() {
        let (intl, intl_requests, intl_server) = chained_server(vec![route(
            INTL_REQUEST,
            401,
            r#"{"message":"unauthorized"}"#,
        )]);
        let (cn, cn_requests, cn_server) = chained_server(vec![route(CN_REQUEST, 200, INTL)]);
        let provider = Alibaba::for_test(&intl, &cn, "cpk-live").expect("builds");

        let snapshot = fetch(&provider).expect("the second region answers");
        intl_server.join().expect("intl server exits");
        cn_server.join().expect("cn server exits");

        assert!(
            intl_requests
                .recv()
                .expect("intl request")
                .contains("currentRegionId=ap-southeast-1")
        );
        assert!(
            cn_requests
                .recv()
                .expect("cn request")
                .contains("currentRegionId=cn-beijing")
        );
        assert_eq!(snapshot.windows.len(), 3);
    }

    #[test]
    fn a_console_login_envelope_is_not_retried_on_the_other_region() {
        let (intl, intl_requests, intl_server) =
            chained_server(vec![route(INTL_REQUEST, 200, NEED_LOGIN)]);
        let provider = Alibaba::for_test(&intl, "http://127.0.0.1:9", "cpk-live").expect("builds");

        let result = fetch(&provider);
        intl_server.join().expect("server exits");

        let error = result.expect_err("key mode cannot read this account");
        assert!(error.to_string().contains("API key"), "{error}");
        assert!(
            intl_requests
                .recv()
                .expect("the one request")
                .contains("currentRegionId=ap-southeast-1")
        );
        assert!(
            intl_requests.try_recv().is_err(),
            "the other region was never asked"
        );
    }

    #[test]
    fn a_missing_quota_envelope_on_both_hosts_ends_malformed() {
        let (intl, intl_requests, intl_server) =
            chained_server(vec![route(INTL_REQUEST, 200, NO_QUOTA)]);
        let (cn, cn_requests, cn_server) = chained_server(vec![route(CN_REQUEST, 200, NO_QUOTA)]);
        let provider = Alibaba::for_test(&intl, &cn, "cpk-live").expect("builds");

        let result = fetch(&provider);
        intl_server.join().expect("intl server exits");
        cn_server.join().expect("cn server exits");

        let error = result.expect_err("neither region named figures");
        assert!(
            error.to_string().contains("named no quota windows"),
            "{error}"
        );
        assert!(!intl_requests.recv().expect("intl request").is_empty());
        assert!(!cn_requests.recv().expect("cn request").is_empty());
    }
}
