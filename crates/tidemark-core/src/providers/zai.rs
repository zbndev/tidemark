//! Z.ai / GLM.
//!
//! The simplest of the five: one `GET`, a bearer token, no OAuth, no local CLI, no cookies.
//! It is first precisely because of that — it is the cheapest way to find out whether the
//! window model survives contact with a real API. It does: one response carried three
//! windows of three different lengths, and needed no new concepts.
//!
//! # What the payload does not tell you
//!
//! `{code, msg, success, data:{limits[], level}}`, where each limit is
//! `{type, unit, number, percentage, nextResetTime}` plus optional absolutes. Two things
//! in there are not derivable from the payload:
//!
//! 1. **`unit` is an enum with no names on the wire** — `{1: day, 3: hour, 5: minute,
//!    6: week}`, and window length is `number × unit`. Nothing in the response says so.
//! 2. **`TIME_LIMIT` with `unit=5, number=1` is the monthly MCP pool**, not a one-minute
//!    window. By the table above it computes to sixty seconds, which would put the pace
//!    mark at 100% within a minute of every reset and make a month-long quota read as
//!    permanently exhausted. There is no field that distinguishes it; it is a hardcoded
//!    special case, carried over from the reference implementation and confirmed against
//!    the live account, where that limit reports a 1000-call pool resetting in three
//!    weeks.
//!
//! 3. **`nextResetTime` is dropped entirely by a window that has just reset**, observed
//!    live: two hours after the five-hour window rolled over, and with nothing spent in the
//!    new one, the entry arrived as `{type, unit, number, percentage: 0}` and nothing else.
//!    The field comes back once the window is in use. The length is still derivable, so the
//!    window is still drawn — it just has no pace mark until then, and since the five-hour
//!    window is the one the card leads with, that is a routine state rather than an edge
//!    case. See `CONTEXT.md` § Vocabulary on Pace.
//!
//! `nextResetTime` is Unix **milliseconds**.

use super::ProviderError;
use super::keyed::{Auth, Method, OptionSchema, Spec};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "zai";

/// Path appended to the region's base URL.
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

/// Which deployment the account lives on. The two are the same API on different hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Region {
    /// `api.z.ai`.
    #[default]
    Global,
    /// `open.bigmodel.cn`.
    BigModelCn,
}

impl Region {
    /// Base URL for this region.
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Global => "https://api.z.ai",
            Self::BigModelCn => "https://open.bigmodel.cn",
        }
    }

    /// The value this region is stored as in `config.toml`.
    pub fn as_value(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::BigModelCn => "bigmodel-cn",
        }
    }

    /// The region a stored value names. An unrecognised value is the default rather than
    /// an error: a typo in `config.toml` must not take the account off the air.
    pub fn from_value(raw: Option<&str>) -> Self {
        match raw {
            Some("bigmodel-cn") => Self::BigModelCn,
            _ => Self::Global,
        }
    }
}

/// Name of the region setting under `[provider.zai]`.
pub const REGION: &str = "region";

/// Z.ai as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Z.ai",
    endpoint: |options| {
        let region = Region::from_value(options.get(REGION).map(String::as_str));
        format!("{}{QUOTA_PATH}", region.base_url())
    },
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[],
    parse,
    credential_hint: "Z.ai dashboard → API keys, on whichever region your account is on.",
    options: &[OptionSchema {
        name: REGION,
        title: "Region",
        description: Some("Two hosts for one API. Pick the one your account is on."),
        default: "global",
        choices: &[
            ("global", "Global (api.z.ai)"),
            ("bigmodel-cn", "China (open.bigmodel.cn)"),
        ],
    }],
};

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    if !envelope.success || envelope.code != 200 {
        let msg = envelope.msg.unwrap_or_else(|| "no message".to_owned());
        return Err(ProviderError::malformed(format!(
            "provider reported failure: code {} — {msg}",
            envelope.code
        )));
    }
    let data = envelope
        .data
        .ok_or_else(|| ProviderError::malformed("successful response carried no data"))?;

    // Recognise the kind *before* deserializing the entry, so that a quota type invented
    // after this was written can carry any shape it likes without breaking the ones we do
    // understand. Once a kind is recognised, a shape we cannot read is an error.
    let mut limits = Vec::new();
    for entry in data.limits {
        let Some(kind) = entry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(Kind::recognise)
        else {
            continue;
        };
        let limit: Limit = serde_json::from_value(entry).map_err(|e| {
            ProviderError::malformed(format!("{kind:?} limit entry is not readable: {e}"))
        })?;
        limits.push(Parsed::new(kind, limit));
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows: limits.iter().map(Parsed::window).collect(),
        details: details(&limits, data.level.as_deref()),
    })
}

/// The limit kinds this parser understands.
///
/// Anything else is skipped rather than refused: an unfamiliar `type` is a quota kind that
/// did not exist when this was written. A kind we *do* know that then fails to parse is a
/// different matter and fails the whole fetch — see the module docs on `providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Token allowance. The main pool on legacy plans.
    Tokens,
    /// Credit allowance. The main pool on current plans.
    Credit,
    /// MCP tool calls. A different pool, not a different length of the same one.
    Time,
}

impl Kind {
    fn recognise(raw: &str) -> Option<Self> {
        match raw {
            "TOKENS_LIMIT" => Some(Self::Tokens),
            "CREDIT_LIMIT" => Some(Self::Credit),
            "TIME_LIMIT" => Some(Self::Time),
            _ => None,
        }
    }
}

/// Seconds in one of the `unit` enum's values, and the noun to call it.
fn unit(code: i64) -> Option<(u64, &'static str)> {
    match code {
        1 => Some((86_400, "day")),
        3 => Some((3_600, "hour")),
        5 => Some((60, "minute")),
        6 => Some((604_800, "week")),
        _ => None,
    }
}

/// Thirty days. What the MCP marker actually means.
const MCP_WINDOW_SECS: u64 = 30 * 86_400;

/// One limit, with the meaning applied.
#[derive(Debug)]
struct Parsed {
    kind: Kind,
    raw: Limit,
    length: Option<WindowLength>,
    used_percent: f64,
    resets_at: Option<Timestamp>,
    title: String,
}

impl Parsed {
    fn new(kind: Kind, raw: Limit) -> Self {
        let is_mcp_marker = kind == Kind::Time && raw.unit == 5 && raw.number == 1;
        let length = if is_mcp_marker {
            WindowLength::from_secs(MCP_WINDOW_SECS)
        } else {
            unit(raw.unit)
                .filter(|_| raw.number > 0)
                .and_then(|(secs, _)| WindowLength::from_secs(secs * raw.number as u64))
        };
        let title = if is_mcp_marker {
            "MCP".to_owned()
        } else {
            match unit(raw.unit) {
                Some((_, noun)) if raw.number > 0 => {
                    let plural = if raw.number == 1 { "" } else { "s" };
                    format!("{} {noun}{plural}", raw.number)
                }
                // The provider described the window in terms we do not have a name for.
                // Better an honest placeholder than a confident wrong one.
                _ => "Quota".to_owned(),
            }
        };
        Self {
            used_percent: used_percent(&raw),
            // An absurd reset time is dropped, not fatal: the window is still real and
            // still worth drawing, it just loses its pace mark. Providers have been
            // observed reporting 1970.
            resets_at: raw
                .next_reset_time
                .and_then(|ms| Timestamp::from_unix_millis(ms).ok()),
            length,
            title,
            kind,
            raw,
        }
    }

    fn key(&self) -> WindowKey {
        match (self.kind, self.length) {
            // MCP calls draw on their own pool. Keyed by pool as well as length so that a
            // future token window of the same length cannot collide with it.
            (Kind::Time, Some(length)) => WindowKey::for_pool("mcp", length),
            (_, Some(length)) => WindowKey::for_length(length),
            // No derivable length, so no length to key on. The raw descriptors are at
            // least stable between responses, which is all a key has to be.
            (_, None) => WindowKey::named(&format!("zai-u{}n{}", self.raw.unit, self.raw.number)),
        }
    }

    fn window(&self) -> Window {
        Window {
            key: self.key(),
            title: self.title.clone(),
            used_percent: self.used_percent,
            resets_at: self.resets_at,
            length: self.length,
        }
    }

    /// The `label: value` row this limit contributes, when it reports absolutes at all.
    fn detail_row(&self) -> Option<DetailRow> {
        let usage = self.raw.usage?;
        let used = absolute_used(&self.raw)?;
        let label = match self.kind {
            Kind::Tokens => format!("{} tokens", self.title),
            Kind::Credit => format!("{} credits", self.title),
            Kind::Time => format!("{} calls", self.title),
        };
        let mut value = format!("{used} of {usage} used");
        if let Some(remaining) = self.raw.remaining {
            value.push_str(&format!(" · {remaining} left"));
        }
        Some(DetailRow { label, value })
    }
}

/// Consumption, preferring absolutes over the reported percentage.
///
/// `percentage` is an integer, so a thousand-call pool reads 0% until ten calls are spent.
/// Where the provider also sends absolutes they are strictly better, and the bar moves when
/// the quota moves.
fn used_percent(raw: &Limit) -> f64 {
    let reported = raw.percentage.clamp(0.0, 100.0);
    let Some(usage) = raw.usage.filter(|u| *u > 0) else {
        return reported;
    };
    let Some(used) = absolute_used(raw) else {
        return reported;
    };
    (used.clamp(0, usage) as f64 * 100.0 / usage as f64).clamp(0.0, 100.0)
}

/// How much of the pool is spent, in the pool's own units.
fn absolute_used(raw: &Limit) -> Option<i64> {
    match (raw.remaining, raw.current_value) {
        (Some(remaining), Some(current)) => Some((raw.usage? - remaining).max(current)),
        (Some(remaining), None) => Some(raw.usage? - remaining),
        (None, Some(current)) => Some(current),
        (None, None) => None,
    }
}

fn details(limits: &[Parsed], level: Option<&str>) -> Vec<DetailSection> {
    let mut sections = Vec::new();

    if let Some(level) = level.map(str::trim).filter(|l| !l.is_empty()) {
        sections.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Level".to_owned(),
                value: level.to_owned(),
            }],
        });
    }

    let rows: Vec<DetailRow> = limits.iter().filter_map(Parsed::detail_row).collect();
    if !rows.is_empty() {
        sections.push(DetailSection {
            title: "Quota".to_owned(),
            rows,
        });
    }

    let per_model: Vec<DetailRow> = limits
        .iter()
        .filter(|l| l.kind == Kind::Time)
        .flat_map(|l| l.raw.usage_details.iter().flatten())
        .filter_map(|detail| {
            Some(DetailRow {
                label: detail.model_code.clone()?,
                value: detail.usage?.to_string(),
            })
        })
        .collect();
    if !per_model.is_empty() {
        sections.push(DetailSection {
            title: "MCP tools".to_owned(),
            rows: per_model,
        });
    }

    sections
}

#[derive(Debug, Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<Data>,
}

#[derive(Debug, Deserialize)]
struct Data {
    #[serde(default)]
    limits: Vec<serde_json::Value>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limit {
    unit: i64,
    number: i64,
    percentage: f64,
    #[serde(default)]
    usage: Option<i64>,
    #[serde(default)]
    current_value: Option<i64>,
    #[serde(default)]
    remaining: Option<i64>,
    #[serde(default)]
    next_reset_time: Option<i64>,
    #[serde(default)]
    usage_details: Option<Vec<UsageDetail>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageDetail {
    #[serde(default)]
    model_code: Option<String>,
    #[serde(default)]
    usage: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture below is hand-authored. It reproduces the *shape* of a live response —
    /// which is a fact about the API — with invented numbers, because the values are the
    /// user's real consumption and this repository is going to be public.
    const LIVE_SHAPE: &str = r#"{
      "code": 200,
      "msg": "Operation successful",
      "data": {
        "limits": [
          {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":40,
           "remaining":960,"percentage":4,"nextResetTime":1789122642999,
           "usageDetails":[{"modelCode":"web-reader","usage":25},{"modelCode":"zread","usage":15}]},
          {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":1787164114706},
          {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":37,"nextResetTime":1787221842997}
        ],
        "level": "pro"
      },
      "success": true
    }"#;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_787_000_000).expect("plausible")
    }

    fn parsed(body: &str) -> Snapshot {
        parse(body, now()).expect("parses")
    }

    fn one_limit(fields: &str) -> Snapshot {
        parsed(&format!(
            r#"{{"code":200,"success":true,"data":{{"limits":[{fields}]}}}}"#
        ))
    }

    fn find<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|w| w.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    #[test]
    fn one_response_carries_three_windows_of_three_lengths() {
        let snapshot = parsed(LIVE_SHAPE);
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["mcp/w2592000", "w18000", "w604800"]);
        assert_eq!(snapshot.provider.as_str(), "zai");
        assert_eq!(snapshot.captured_at, now());
    }

    #[test]
    fn every_window_in_a_snapshot_has_its_own_key() {
        // Two windows sharing a key would land on the same storage row, and the second one
        // would be silently reported stale rather than stored.
        let snapshot = parsed(LIVE_SHAPE);
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        let unique = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), unique);
    }

    #[test]
    fn the_mcp_marker_is_a_month_and_not_a_minute() {
        // unit=5, number=1 computes to sixty seconds by the unit table. Taking that
        // literally would put the pace mark at 100% a minute after every reset.
        let snapshot = parsed(LIVE_SHAPE);
        let mcp = find(&snapshot, "mcp/w2592000");
        assert_eq!(mcp.length, WindowLength::from_secs(30 * 86_400));
        assert_eq!(mcp.title, "MCP");
        assert!(
            mcp.pace(now()).expect("computable") < 0.9,
            "a month-long window should not read as nearly elapsed"
        );
    }

    #[test]
    fn a_one_minute_token_window_is_still_taken_literally() {
        // The special case is the MCP *kind*, not the numbers. Nothing else gets it.
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":5,"number":1,"percentage":3}"#);
        assert_eq!(snapshot.windows[0].length, WindowLength::from_secs(60));
        assert_eq!(snapshot.windows[0].title, "1 minute");
    }

    #[test]
    fn lengths_come_from_the_unnamed_unit_enum() {
        let snapshot = parsed(LIVE_SHAPE);
        assert_eq!(
            find(&snapshot, "w18000").length,
            WindowLength::from_secs(5 * 3_600)
        );
        assert_eq!(find(&snapshot, "w18000").title, "5 hours");
        assert_eq!(find(&snapshot, "w604800").title, "1 week");
    }

    #[test]
    fn reset_times_arrive_in_milliseconds() {
        let snapshot = parsed(LIVE_SHAPE);
        let five_hour = find(&snapshot, "w18000");
        assert_eq!(
            five_hour.resets_at,
            Some(Timestamp::from_unix(1_787_164_114).expect("plausible"))
        );
    }

    #[test]
    fn an_absurd_reset_time_costs_the_pace_mark_not_the_window() {
        let snapshot = one_limit(
            r#"{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":50,"nextResetTime":0}"#,
        );
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].resets_at, None);
        assert_eq!(snapshot.windows[0].used_percent, 50.0);
    }

    #[test]
    fn a_window_that_just_reset_omits_its_reset_time_and_keeps_its_length() {
        // Observed live: two hours after the five-hour window rolled over, with nothing
        // spent in the new one, the entry carried no `nextResetTime` at all.
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0}"#);
        let window = &snapshot.windows[0];
        assert_eq!(window.length, WindowLength::from_secs(18_000));
        assert_eq!(window.key.as_str(), "w18000");
        assert_eq!(window.resets_at, None);
        assert_eq!(window.pace(now()), None, "no reset time means no pace mark");
        assert_eq!(window.is_outpacing(now()), None);
    }

    #[test]
    fn absolutes_beat_the_integer_percentage() {
        // A thousand-call pool reads 0% for its first ten calls if you trust `percentage`.
        let snapshot = one_limit(
            r#"{"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":5,
                "remaining":995,"percentage":0}"#,
        );
        assert!((snapshot.windows[0].used_percent - 0.5).abs() < 1e-9);
    }

    #[test]
    fn the_larger_of_the_two_spent_counts_wins() {
        // `usage - remaining` and `currentValue` disagree; the provider's own client takes
        // the larger, and under-reporting consumption is the dangerous direction.
        let snapshot = one_limit(
            r#"{"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":25,
                "remaining":990,"percentage":1}"#,
        );
        assert!((snapshot.windows[0].used_percent - 2.5).abs() < 1e-9);
    }

    #[test]
    fn without_absolutes_the_reported_percentage_stands() {
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":89}"#);
        assert_eq!(snapshot.windows[0].used_percent, 89.0);
    }

    #[test]
    fn an_unfamiliar_quota_kind_is_skipped_rather_than_refused() {
        let snapshot = parsed(
            r#"{"code":200,"success":true,"data":{"limits":[
                 {"type":"MYSTERY_LIMIT","shape":{"we":"have","never":"seen"}},
                 {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":10}
               ]}}"#,
        );
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key.as_str(), "w604800");
    }

    #[test]
    fn a_familiar_kind_we_cannot_read_fails_the_whole_fetch() {
        // The alternative is dropping the window, and a missing window reads as "you have
        // no such limit" — the most dangerous thing this program can say.
        let err = parse(
            r#"{"code":200,"success":true,"data":{"limits":[
                 {"type":"TOKENS_LIMIT","unit":"hour","number":5,"percentage":10}
               ]}}"#,
            now(),
        )
        .expect_err("must refuse");
        assert!(matches!(err, ProviderError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn an_unknown_unit_still_draws_a_window_just_without_a_pace_mark() {
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":9,"number":3,"percentage":42}"#);
        assert_eq!(snapshot.windows[0].length, None);
        assert_eq!(snapshot.windows[0].key.as_str(), "zai-u9n3");
        assert_eq!(snapshot.windows[0].used_percent, 42.0);
        assert_eq!(snapshot.windows[0].pace(now()), None);
    }

    #[test]
    fn a_failed_envelope_is_never_read_as_data() {
        let err = parse(
            r#"{"code":401,"msg":"invalid api key","success":false,"data":null}"#,
            now(),
        )
        .expect_err("must refuse");
        assert!(format!("{err}").contains("invalid api key"), "{err}");
    }

    #[test]
    fn a_success_flag_without_data_is_refused() {
        assert!(parse(r#"{"code":200,"success":true}"#, now()).is_err());
    }

    #[test]
    fn details_carry_the_plan_the_absolutes_and_the_per_tool_counts() {
        let snapshot = parsed(LIVE_SHAPE);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Quota", "MCP tools"]);
        assert_eq!(snapshot.details[0].rows[0].value, "pro");
        assert_eq!(snapshot.details[1].rows[0].label, "MCP calls");
        assert_eq!(
            snapshot.details[1].rows[0].value,
            "40 of 1000 used · 960 left"
        );
        assert_eq!(snapshot.details[2].rows.len(), 2);
        assert_eq!(snapshot.details[2].rows[0].label, "web-reader");
    }

    #[test]
    fn a_response_with_nothing_to_say_produces_no_empty_sections() {
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":10}"#);
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn the_card_leads_with_the_five_hour_window() {
        let dominant = parsed(LIVE_SHAPE)
            .dominant_window()
            .expect("present")
            .clone();
        assert_eq!(dominant.key.as_str(), "w18000");
    }

    #[test]
    fn the_spec_carries_the_region_to_the_endpoint() {
        use crate::providers::keyed::{Auth, Method, Options};

        let global = Options::new();
        let cn: Options = [("region".to_owned(), "bigmodel-cn".to_owned())]
            .into_iter()
            .collect();

        assert_eq!(
            (SPEC.endpoint)(&global),
            "https://api.z.ai/api/monitor/usage/quota/limit"
        );
        assert_eq!(
            (SPEC.endpoint)(&cn),
            "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
        );
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
    }

    #[test]
    fn an_unknown_region_falls_back_to_global_rather_than_refusing_to_poll() {
        use crate::providers::keyed::Options;

        let nonsense: Options = [("region".to_owned(), "atlantis".to_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            (SPEC.endpoint)(&nonsense),
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "a typo in config.toml must not take the account off the air"
        );
    }

    #[test]
    fn the_region_option_is_published_with_both_hosts() {
        let region = SPEC
            .options
            .iter()
            .find(|option| option.name == REGION)
            .expect("the region is published");
        let values: Vec<&str> = region.choices.iter().map(|(value, _)| *value).collect();
        assert_eq!(values, ["global", "bigmodel-cn"]);
        assert_eq!(region.default, "global");
    }
}
