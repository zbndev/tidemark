//! MiniMax.
//!
//! Ported from CodexBar's `MiniMax/MiniMaxUsageFetcher.swift` and `MiniMaxAPIRegion.swift`,
//! the Coding Plan **API-token** path only: one GET against the token-plan remains endpoint
//! with an `sk-cp-` key. Never seen answering: every number in the tests is a body CodexBar
//! recorded.
//!
//! # What is not ported, and why
//!
//! Most of that five-thousand-line module is the other three ways in — a browser cookie, a
//! `localStorage` import, and an HTML scrape of the coding-plan page — together with the
//! automatic fallback between them. None of those is a pasted key, and they are out of this
//! plan's scope. Two smaller ladders go with them: the retry that repeats a rejected
//! request against the other region's host, and the older
//! `/v1/api/openplatform/coding_plan/remains` endpoint tried when the token-plan one fails.
//! Both are second requests, and a [`Spec`] is one request; the region is a setting the user
//! chooses instead of something guessed at by spending a rejected request to find out.
//!
//! The pre-digested `data.services` shape the source tries first is not ported either: no
//! recorded body uses it, and a parser written against a shape nobody has seen is a guess.
//!
//! # What the payload does not tell you
//!
//! **The counts are backwards.** `current_interval_usage_count` is what is *left*, not what
//! is spent — the source says so in a comment beside it, and reading it the other way would
//! draw a full bar for an empty quota.
//!
//! **A lane reports a share or a count, never both.** When
//! `current_interval_remaining_percent` is there the counts are zeroes and mean nothing;
//! the share is what is real, and consumption is a hundred minus it.
//!
//! **The window bounds move, and the length is bucketed on purpose.** An interval arrives
//! four to six hours wide and is a five-hour window either way; twenty-three to twenty-five
//! hours is a day. The source buckets it, and so does this — a window keyed on the exact
//! span would split one continuous quota across two keys every time the bounds shifted an
//! hour, which is exactly what [`WindowKey`] exists to prevent.
//!
//! **A boost is a bigger allowance, not more consumption.** `weekly_boost_permille: 1500`
//! means the weekly allowance is one and a half times the standard one. It does not change
//! the share already spent, so it is said under the bar rather than folded into it. The
//! field is spelled both `_permill` and `_permille` in recorded bodies.
//!
//! **Status 3 means "not in your subscription".** A lane the schema has but the plan does
//! not comes back with zero counts, a hundred per cent remaining and status 3; drawing it
//! would put an untouched bar on the card for a service the account cannot use. The one
//! exception is the weekly text-generation lane, where the same shape means unlimited, and
//! the source draws it — so this does too, as a window at nought with no reset.
//!
//! # The failure that arrives as a success
//!
//! A rejected key comes back inside `base_resp` with status 1004 and a message about
//! logging in again, under HTTP 200. That is a credential error rather than an unreadable
//! response, so the interface asks for a new key.

use super::{Auth, Method, OptionSchema, Spec};
use crate::providers::{ProviderError, length_title, title_case};
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "minimax";

/// Path appended to the region's API host.
const REMAINS_PATH: &str = "/v1/token_plan/remains";

/// Name of the region setting under `[provider.minimax]`.
pub const REGION: &str = "region";

/// The status a lane carries when the schema has it and the subscription does not.
const STATUS_NOT_SUBSCRIBED: i64 = 3;

/// Which deployment the account lives on. The two are the same API on different hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Region {
    /// `api.minimax.io`.
    #[default]
    Global,
    /// `api.minimaxi.com`.
    ChinaMainland,
}

impl Region {
    /// API host for this region. Not the same host as the console the key is copied from.
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Global => "https://api.minimax.io",
            Self::ChinaMainland => "https://api.minimaxi.com",
        }
    }

    /// The value this region is stored as in `config.toml`. The source's own spellings.
    pub fn as_value(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::ChinaMainland => "cn",
        }
    }

    /// The region a stored value names. An unrecognised value is the default rather than
    /// an error: a typo in `config.toml` must not take the account off the air.
    pub fn from_value(raw: Option<&str>) -> Self {
        match raw {
            Some("cn") => Self::ChinaMainland,
            _ => Self::Global,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    base_resp: Option<BaseResp>,
    #[serde(default)]
    current_subscribe_title: Option<String>,
    #[serde(default)]
    plan_name: Option<String>,
    #[serde(default)]
    combo_title: Option<String>,
    #[serde(default)]
    current_plan_title: Option<String>,
    #[serde(default)]
    current_combo_card: Option<ComboCard>,
    #[serde(default)]
    points_balance: Option<f64>,
    #[serde(default)]
    model_remains: Vec<Lane>,
}

#[derive(Debug, Deserialize)]
struct BaseResp {
    #[serde(default)]
    status_code: Option<i64>,
    #[serde(default)]
    status_msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ComboCard {
    #[serde(default)]
    title: Option<String>,
}

/// One model's two quota lanes, as the response spells them out side by side.
#[derive(Debug, Deserialize)]
struct Lane {
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    current_interval_total_count: Option<i64>,
    #[serde(default)]
    current_interval_usage_count: Option<i64>,
    #[serde(default)]
    current_interval_remaining_percent: Option<f64>,
    #[serde(default)]
    current_interval_status: Option<i64>,
    #[serde(default)]
    start_time: Option<i64>,
    #[serde(default)]
    end_time: Option<i64>,
    #[serde(default)]
    remains_time: Option<i64>,
    #[serde(default, alias = "interval_boost_permille")]
    interval_boost_permill: Option<i64>,
    #[serde(default)]
    current_weekly_total_count: Option<i64>,
    #[serde(default)]
    current_weekly_usage_count: Option<i64>,
    #[serde(default)]
    current_weekly_remaining_percent: Option<f64>,
    #[serde(default)]
    current_weekly_status: Option<i64>,
    #[serde(default)]
    weekly_start_time: Option<i64>,
    #[serde(default)]
    weekly_end_time: Option<i64>,
    #[serde(default)]
    weekly_remains_time: Option<i64>,
    #[serde(default, alias = "weekly_boost_permille")]
    weekly_boost_permill: Option<i64>,
}

/// The parts of one lane the window is built from, so the interval and the weekly halves
/// go through the same code rather than through two copies of it.
struct Quota<'a> {
    /// What the model is called after mapping, for the pool key and the title.
    service: &'a str,
    total: Option<i64>,
    /// **Remaining**, not spent. See the module doc.
    remaining: Option<i64>,
    remaining_percent: Option<f64>,
    status: Option<i64>,
    start: Option<i64>,
    end: Option<i64>,
    remains: Option<i64>,
    boost_permille: Option<i64>,
    /// True for the weekly half, which has its own length bucket and its own unlimited case.
    weekly: bool,
}

/// MiniMax's own bucketing of a model name into the service the card names.
///
/// `general` and `video` are the Token Plan's own buckets and keep their names; the older
/// per-model names are mapped to what the service is.
fn service_of(model: &str) -> String {
    let lower = model.trim().to_lowercase();
    if lower == "general" || lower == "video" {
        return lower;
    }
    if is_text_generation(&lower) {
        return "text generation".to_owned();
    }
    if lower.contains("speech") {
        return "text to speech".to_owned();
    }
    if lower.contains("hailuo") && lower.contains("fast") {
        return "image to video".to_owned();
    }
    if lower.contains("hailuo") {
        return "text to video".to_owned();
    }
    if lower.starts_with("image-") {
        return "image generation".to_owned();
    }
    if lower.contains("music") {
        return "music generation".to_owned();
    }
    lower
}

/// Whether a model name is one of the text-generation ones. Only these have a weekly lane.
fn is_text_generation(lower: &str) -> bool {
    lower == "general" || lower.contains("minimax-m") || lower.starts_with("m2.")
}

/// An epoch value in whichever unit it arrived in — milliseconds above the year 33658,
/// seconds above 2001, and nothing below that, which is the source's own test.
fn instant(raw: Option<i64>) -> Option<Timestamp> {
    let raw = raw?;
    if raw > 1_000_000_000_000 {
        return Timestamp::from_unix_millis(raw).ok();
    }
    if raw > 1_000_000_000 {
        return Timestamp::from_unix(raw).ok();
    }
    None
}

/// The window length a span of this many seconds means.
///
/// Bucketed rather than taken literally: the bounds move between polls, and a key derived
/// from the exact span would split one continuous quota in two. See the module doc.
fn bucket(span: i64, weekly: bool) -> Option<WindowLength> {
    let span = u64::try_from(span).ok()?;
    let canonical = if weekly && (518_400..=691_200).contains(&span) {
        604_800
    } else if !weekly && (14_400..=21_600).contains(&span) {
        18_000
    } else if !weekly && (82_800..=90_000).contains(&span) {
        86_400
    } else {
        span
    };
    WindowLength::from_secs(canonical)
}

/// The allowance a boosted percentage lane is drawn against, as a share of the standard
/// one: `1500` per mille is 150%.
fn boost_percent(permille: Option<i64>) -> Option<i64> {
    let permille = permille?;
    (permille > 0 && permille != 1_000).then_some((permille + 5) / 10)
}

impl Quota<'_> {
    /// A lane the schema has and the subscription does not. See the module doc.
    fn is_placeholder(&self) -> bool {
        self.status == Some(STATUS_NOT_SUBSCRIBED)
            && self.total.unwrap_or(0) == 0
            && self.remaining.unwrap_or(0) == 0
            && self
                .remaining_percent
                .is_some_and(|percent| percent >= 100.0)
    }

    /// The same shape as a placeholder, but on the weekly text-generation lane it means
    /// there is no ceiling rather than no entitlement — the source's own exception.
    fn is_unlimited(&self) -> bool {
        self.weekly
            && self.status == Some(STATUS_NOT_SUBSCRIBED)
            && matches!(self.service, "general" | "text generation")
            && self
                .remaining_percent
                .is_some_and(|percent| percent >= 100.0)
    }
}

/// One quota lane as a window, `None` where the lane is not the account's to use.
fn window_of(quota: &Quota<'_>, captured_at: Timestamp) -> Result<Option<Window>, ProviderError> {
    let unlimited = quota.is_unlimited();
    if !unlimited && quota.is_placeholder() {
        return Ok(None);
    }

    let start = instant(quota.start);
    let end = instant(quota.end);
    let length = match (start, end) {
        (Some(start), Some(end)) => bucket(end.as_unix() - start.as_unix(), quota.weekly),
        _ => None,
    };

    // The end of the interval when it is still ahead, otherwise a countdown from the poll.
    // `remains_time` arrives in milliseconds when it is large enough to be unambiguous.
    let resets_at = match end {
        Some(end) if end.as_unix() > captured_at.as_unix() => Some(end),
        _ => match quota.remains.filter(|remains| *remains > 0) {
            Some(remains) if remains > 1_000_000 => {
                Some(captured_at.saturating_add_seconds(remains / 1_000))
            }
            Some(remains) => Some(captured_at.saturating_add_seconds(remains)),
            None => None,
        },
    };

    let (used_percent, subtitle, resets_at) = if unlimited {
        (0.0, Some("Unlimited".to_owned()), None)
    } else if let Some(remaining_percent) = quota.remaining_percent {
        if !remaining_percent.is_finite() {
            return Err(ProviderError::malformed(
                "a remaining share must be a number",
            ));
        }
        let boosted = boost_percent(quota.boost_permille)
            .map(|percent| format!("Allowance boosted to {percent}%"));
        (
            (100.0 - remaining_percent).clamp(0.0, 100.0),
            boosted,
            resets_at,
        )
    } else {
        // A count lane. `remaining` is what is left; consumption is the difference.
        let (Some(total), Some(remaining)) = (quota.total, quota.remaining) else {
            return Err(ProviderError::malformed(format!(
                "the {} lane reported neither a share nor a count",
                quota.service
            )));
        };
        if total <= 0 {
            return Err(ProviderError::malformed(format!(
                "the {} lane reported a count out of nothing",
                quota.service
            )));
        }
        let used = (total - remaining).max(0);
        (
            (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0),
            Some(format!("{used} / {total} prompts")),
            resets_at,
        )
    };

    let length = length.ok_or_else(|| {
        ProviderError::malformed(format!(
            "the {} lane reported no window to draw it in",
            quota.service
        ))
    })?;

    Ok(Some(Window {
        key: WindowKey::for_length(length),
        title: length_title(length),
        subtitle,
        used_percent,
        resets_at,
        length: Some(length),
    }))
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_for_account(body, captured_at, &AccountId::default())
}

pub fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    if let Some(base) = &envelope.base_resp
        && let Some(status) = base.status_code
        && status != 0
    {
        let message = base
            .status_msg
            .clone()
            .unwrap_or_else(|| format!("status_code {status}"));
        let lower = message.to_lowercase();
        // A rejected key arrives here rather than in the HTTP status. See the module doc.
        if status == 1004
            || lower.contains("cookie")
            || lower.contains("log in")
            || lower.contains("login")
        {
            return Err(ProviderError::Credential { status: 401 });
        }
        return Err(ProviderError::malformed(format!(
            "the provider reported {message}"
        )));
    }

    if envelope.model_remains.is_empty() {
        return Err(ProviderError::malformed("no quota lanes in the response"));
    }

    let mut drafts: Vec<(String, Window)> = Vec::new();
    for lane in &envelope.model_remains {
        // A lane with no model name is still a quota lane: the source skips it when
        // building its per-service list but still draws its bar from the same numbers, and
        // dropping the window here would report an account with a limit as having none.
        let service = lane.model_name.as_deref().map(service_of);
        let named = service.clone().unwrap_or_default();

        let interval = Quota {
            service: &named,
            total: lane.current_interval_total_count,
            remaining: lane.current_interval_usage_count,
            remaining_percent: lane.current_interval_remaining_percent,
            status: lane.current_interval_status,
            start: lane.start_time,
            end: lane.end_time,
            remains: lane.remains_time,
            boost_permille: lane.interval_boost_permill,
            weekly: false,
        };
        if let Some(window) = window_of(&interval, captured_at)? {
            drafts.push((named.clone(), window));
        }

        // Only the text-generation lanes have a weekly quota; for everything else the
        // weekly fields are zeroes that would draw an untouched bar.
        if service.as_deref().is_some_and(is_text_generation) {
            let weekly = Quota {
                service: &named,
                total: lane.current_weekly_total_count,
                remaining: lane.current_weekly_usage_count,
                remaining_percent: lane.current_weekly_remaining_percent,
                status: lane.current_weekly_status,
                start: lane.weekly_start_time,
                end: lane.weekly_end_time,
                remains: lane.weekly_remains_time,
                boost_permille: lane.weekly_boost_permill,
                weekly: true,
            };
            if let Some(window) = window_of(&weekly, captured_at)? {
                drafts.push((named.clone(), window));
            }
        }
    }

    // Two services reporting the same length would otherwise file under one key. Only then
    // does the pool go in, and only then is the service worth saying in the title.
    let contested: Vec<bool> = drafts
        .iter()
        .map(|(_, window)| {
            drafts
                .iter()
                .filter(|(_, other)| other.key == window.key)
                .count()
                > 1
        })
        .collect();
    let mut windows = Vec::with_capacity(drafts.len());
    for ((service, mut window), contested) in drafts.into_iter().zip(contested) {
        if contested && !service.is_empty() {
            let length = window.length.expect("every window here has a length");
            window.key = WindowKey::for_pool(&service, length);
            window.title = format!("{} · {}", title_case(&service), window.title);
        }
        windows.push(window);
    }

    let plan = [
        envelope.current_subscribe_title,
        envelope.plan_name,
        envelope.combo_title,
        envelope.current_plan_title,
        envelope.current_combo_card.and_then(|card| card.title),
    ]
    .into_iter()
    .flatten()
    .map(|title| title.trim().to_owned())
    .find(|title| !title.is_empty());

    let mut rows = Vec::new();
    if let Some(plan) = plan {
        rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: plan,
        });
    }
    if let Some(points) = envelope.points_balance.filter(|value| value.is_finite()) {
        rows.push(DetailRow {
            label: "Points".to_owned(),
            value: format!("{points:.0}"),
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows,
        details: if rows.is_empty() {
            Vec::new()
        } else {
            vec![DetailSection {
                title: DetailSection::PLAN.to_owned(),
                rows,
            }]
        },
    })
}

/// MiniMax as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "MiniMax",
    endpoint: |options| {
        let region = Region::from_value(options.get(REGION).map(String::as_str));
        format!("{}{REMAINS_PATH}", region.base_url())
    },
    method: Method::Get,
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "MiniMax platform → Coding Plan → API key (sk-cp-…).",
    options: &[OptionSchema {
        name: REGION,
        title: "Region",
        description: Some(
            "The same API on two hosts. A key issued for one is rejected by the other.",
        ),
        default: "global",
        choices: &[
            ("global", "Global (api.minimax.io)"),
            ("cn", "China mainland (api.minimaxi.com)"),
        ],
        required: false,
    }],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::keyed::Options;

    /// Recorded by CodexBar, `MiniMaxCurrentTokenPlanResponseTests.swift` — "parses token
    /// plan boosted weekly lane with permille spelling". Its own test asserts two lanes,
    /// `["5 hours", "Weekly"]`, at 0% and 30%, and that the video lane draws nothing.
    const BOOSTED_WEEKLY: &str = r#"
    {
      "model_remains": [
        {
          "start_time": 1782043200000,
          "end_time": 1782057600000,
          "remains_time": 7003536,
          "current_interval_total_count": 0,
          "current_interval_usage_count": 0,
          "model_name": "general",
          "current_weekly_total_count": 0,
          "current_weekly_usage_count": 0,
          "weekly_start_time": 1781452800000,
          "weekly_end_time": 1782057600000,
          "weekly_remains_time": 7003536,
          "current_interval_status": 1,
          "current_interval_remaining_percent": 100,
          "current_weekly_status": 1,
          "current_weekly_remaining_percent": 70,
          "weekly_boost_permille": 1500
        },
        {
          "start_time": 1781971200000,
          "end_time": 1782057600000,
          "remains_time": 7003536,
          "current_interval_total_count": 0,
          "current_interval_usage_count": 0,
          "model_name": "video",
          "current_weekly_total_count": 0,
          "current_weekly_usage_count": 0,
          "weekly_start_time": 1781452800000,
          "weekly_end_time": 1782057600000,
          "weekly_remains_time": 7003536,
          "current_interval_status": 3,
          "current_interval_remaining_percent": 100,
          "current_weekly_status": 3,
          "current_weekly_remaining_percent": 100
        }
      ],
      "base_resp": { "status_code": 0, "status_msg": "success" }
    }"#;

    /// Recorded by CodexBar, same file — the percent-based body its web test asserts a
    /// primary of 4% for, from 96% remaining.
    const PERCENT_BASED: &str = r#"
    {
      "model_remains": [
        {
          "start_time": 1780279200000,
          "end_time": 1780297200000,
          "remains_time": 16659830,
          "current_interval_total_count": 0,
          "current_interval_usage_count": 0,
          "model_name": "general",
          "current_weekly_total_count": 0,
          "current_weekly_usage_count": 0,
          "weekly_start_time": 1780243200000,
          "weekly_end_time": 1780848000000,
          "weekly_remains_time": 567459830,
          "current_interval_status": 1,
          "current_interval_remaining_percent": 96,
          "current_weekly_status": 1,
          "current_weekly_remaining_percent": 99
        }
      ],
      "base_resp": { "status_code": 0, "status_msg": "success" }
    }"#;

    /// Recorded by CodexBar, `MiniMaxAPITokenFetchTests.swift` — the body the china host
    /// answers the API token with. A count lane, no model name, plan "Max".
    const COUNTED: &str = r#"
    {
      "base_resp": { "status_code": 0 },
      "current_subscribe_title": "Max",
      "model_remains": [
        {
          "current_interval_total_count": 1000,
          "current_interval_usage_count": 250,
          "start_time": 1700000000000,
          "end_time": 1700018000000,
          "remains_time": 240000
        }
      ]
    }"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_boosted_fixture_draws_the_two_lanes_the_source_draws() {
        let snapshot = parse(BOOSTED_WEEKLY, at(1_782_050_596)).expect("parses");
        assert_eq!(
            snapshot.windows.len(),
            2,
            "the video lane is not in this subscription and draws nothing"
        );
        assert_eq!(snapshot.windows[0].title, "5 hours");
        assert_eq!(snapshot.windows[0].used_percent, 0.0);
        assert_eq!(snapshot.windows[0].subtitle, None);
        assert_eq!(snapshot.windows[1].title, "7 days");
        assert_eq!(snapshot.windows[1].used_percent, 30.0);
        assert_eq!(
            snapshot.windows[1].subtitle.as_deref(),
            Some("Allowance boosted to 150%"),
            "a boost is a bigger allowance, not more consumption"
        );
        assert_eq!(
            snapshot.windows[0].resets_at.expect("the interval ends"),
            at(1_782_057_600)
        );
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn a_percentage_lane_is_a_hundred_minus_what_is_left() {
        let snapshot = parse(PERCENT_BASED, at(1_780_282_340)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].used_percent, 4.0);
        assert_eq!(snapshot.windows[1].used_percent, 1.0);
        assert_eq!(
            snapshot.windows[0].length.expect("bucketed").as_secs(),
            18_000
        );
        assert_eq!(
            snapshot.windows[1].length.expect("bucketed").as_secs(),
            604_800
        );
    }

    #[test]
    fn a_count_lane_reads_the_remaining_count_as_remaining() {
        let snapshot = parse(COUNTED, at(1_700_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(
            snapshot.windows[0].used_percent, 75.0,
            "250 left of 1000 is 750 spent, not 250"
        );
        assert_eq!(
            snapshot.windows[0].subtitle.as_deref(),
            Some("750 / 1000 prompts")
        );
        assert_eq!(snapshot.details[0].rows[0].value, "Max");
    }

    #[test]
    fn a_span_that_moved_by_an_hour_is_still_the_same_window() {
        // The recorded interval is four hours wide and the plan's is five; both key the
        // same, or one continuous quota would split across two keys.
        let four_hours = parse(BOOSTED_WEEKLY, at(1_782_050_596)).expect("parses");
        let five_hours = parse(PERCENT_BASED, at(1_780_282_340)).expect("parses");
        assert_eq!(four_hours.windows[0].key, five_hours.windows[0].key);
        assert_eq!(
            four_hours.windows[0].length.expect("bucketed").as_secs(),
            18_000
        );
    }

    #[test]
    fn an_unlimited_weekly_lane_is_drawn_and_an_unsubscribed_one_is_not() {
        let unlimited = r#"{"base_resp":{"status_code":0},"model_remains":[{
            "model_name":"general","start_time":1780279200000,"end_time":1780297200000,
            "current_interval_total_count":0,"current_interval_usage_count":0,
            "current_interval_status":1,"current_interval_remaining_percent":50,
            "weekly_start_time":1780243200000,"weekly_end_time":1780848000000,
            "current_weekly_total_count":0,"current_weekly_usage_count":0,
            "current_weekly_status":3,"current_weekly_remaining_percent":100}]}"#;
        let snapshot = parse(unlimited, at(1_780_282_340)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[1].used_percent, 0.0);
        assert_eq!(snapshot.windows[1].subtitle.as_deref(), Some("Unlimited"));
        assert!(
            snapshot.windows[1].resets_at.is_none(),
            "nothing that does not run out has a reset"
        );
    }

    #[test]
    fn two_services_of_the_same_length_are_told_apart_by_their_pool() {
        let body = r#"{"base_resp":{"status_code":0},"model_remains":[
            {"model_name":"speech-hd","start_time":1780279200000,"end_time":1780297200000,
             "current_interval_total_count":100,"current_interval_usage_count":40,
             "current_interval_status":1},
            {"model_name":"image-01","start_time":1780279200000,"end_time":1780297200000,
             "current_interval_total_count":50,"current_interval_usage_count":50,
             "current_interval_status":1}]}"#;
        let snapshot = parse(body, at(1_780_282_340)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert_ne!(snapshot.windows[0].key, snapshot.windows[1].key);
        assert_eq!(snapshot.windows[0].title, "Text To Speech · 5 hours");
        assert_eq!(snapshot.windows[1].title, "Image Generation · 5 hours");
        assert_eq!(snapshot.windows[0].used_percent, 60.0);
        assert_eq!(snapshot.windows[1].used_percent, 0.0);
    }

    #[test]
    fn a_key_the_provider_rejects_inside_a_body_asks_for_a_new_key() {
        // Recorded by CodexBar, `MiniMaxAPITokenFetchTests.swift`.
        let body = r#"{"base_resp":{"status_code":1004,"status_msg":"invalid api key"}}"#;
        assert!(matches!(
            parse(body, at(1_780_282_340)),
            Err(ProviderError::Credential { status: 401 })
        ));

        // Recorded by CodexBar, `MiniMaxCurrentTokenPlanResponseTests.swift`: the same
        // rejection worded as a session that ran out.
        let cookie = r#"{"base_resp":{"status_code":1004,
            "status_msg":"cookie is missing, log in again"}}"#;
        assert!(matches!(
            parse(cookie, at(1_780_282_340)),
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn a_body_we_cannot_read_is_malformed() {
        for body in [
            r#"{"partial":"#,
            // No lanes at all: CodexBar's "Missing coding plan data."
            r#"{"base_resp":{"status_code":0},"model_remains":[]}"#,
            // A lane with a share where a number belongs.
            r#"{"model_remains":[{"model_name":"general","start_time":1780279200000,
                "end_time":1780297200000,"current_interval_remaining_percent":"half"}]}"#,
            // A lane with neither a share nor a count.
            r#"{"model_remains":[{"model_name":"general","start_time":1780279200000,
                "end_time":1780297200000,"current_interval_status":1}]}"#,
            // A lane with no window to draw it in.
            r#"{"model_remains":[{"model_name":"general","current_interval_total_count":10,
                "current_interval_usage_count":5,"current_interval_status":1}]}"#,
            // A failure that is not a rejected key.
            r#"{"base_resp":{"status_code":2013,"status_msg":"internal error"}}"#,
        ] {
            assert!(
                matches!(
                    parse(body, at(1_780_282_340)),
                    Err(ProviderError::Malformed(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn the_region_chooses_the_host_and_an_unknown_value_falls_back() {
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://api.minimax.io/v1/token_plan/remains"
        );
        assert_eq!(
            (SPEC.endpoint)(&options(&[(REGION, "cn")])),
            "https://api.minimaxi.com/v1/token_plan/remains"
        );
        assert_eq!(
            (SPEC.endpoint)(&options(&[(REGION, "mars")])),
            "https://api.minimax.io/v1/token_plan/remains",
            "a typo in the settings file must not take the account off the air"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(SPEC.method, Method::Get);
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.options[0].default, Region::default().as_value());
    }
}
