//! Kimi For Coding.
//!
//! The second API-key provider, and the first whose numbers are absolute: everything on
//! this endpoint is a count of requests, so the bar is computed from counts rather than
//! read off a percentage the provider rounded for us.
//!
//! # Wrong product, wrong endpoint
//!
//! *Kimi For Coding* is not *Moonshot / Kimi Open Platform*. The latter reports a money
//! balance and has no reset windows at all; a key from it against this host answers 401,
//! which the interface already has a state for.
//!
//! # What the payload does not tell you
//!
//! `{user, usage, limits[], parallel, ...}`, where `usage` and every `limits[].detail`
//! carry `{limit, used, remaining, resetTime}`. Three things in there are not what they
//! look like:
//!
//! 1. **The numbers are strings.** `"100"`, `"24"`, `"76"` — quoted, including the ones
//!    that are plainly counts. [`Number`] accepts either spelling rather than trusting
//!    this to stay put.
//! 2. **`used` is not always sent.** Observed live: the five-hour entry arrived as
//!    `{limit, remaining, resetTime}` with no `used` at all, while `usage` alongside it
//!    carried one. Consumption is therefore `used` when it is there and `limit -
//!    remaining` when it is not — and an entry carrying *neither* fails the response,
//!    because the alternative is drawing a full bar over a quota we cannot measure.
//! 3. **`usage` does not describe its own length.** It is the plan's request allowance and
//!    the only length descriptor in the payload belongs to the entries in `limits[]`. It
//!    is seven days: `~/.config/codexbar/history/kimi.json` records this account's
//!    `resetTime` advancing 2026-08-08 → 08-15 → 08-22, twice, at exactly 168.00 hours a
//!    step. That measurement is the whole justification for [`PLAN_WINDOW_SECS`] — nothing
//!    on the wire says it.
//!
//! # Two pools, not two lengths of one
//!
//! The plan allowance and the burst rate limit are separate quotas that happen to be
//! counted in the same currency: a request spent leaves both, but exhausting the five-hour
//! limit does not touch the weekly one. `limits[]` therefore keeps its own key prefix, so
//! a rate limit that one day arrives with a seven-day window cannot land on top of the
//! plan's own history.

use super::{BoxFuture, Credential, Provider, ProviderError, http, length_title, title_case};
use serde::{Deserialize, Deserializer, de};
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "kimi";

/// The Kimi For Coding API. Not `www.kimi.com`, which is the web console's own gateway and
/// wants a session cookie rather than a key.
const BASE_URL: &str = "https://api.kimi.com";

/// Path appended to the base URL.
const USAGES_PATH: &str = "/coding/v1/usages";

/// How long the plan allowance runs for.
///
/// Hardcoded because the payload does not say, and measured rather than assumed: see the
/// module docs. A wrong length here would not merely mislabel the window, it would put the
/// pace mark in the wrong place on the bar the user is being asked to trust.
const PLAN_WINDOW_SECS: u64 = 7 * 86_400;

/// Key prefix for the burst limits in `limits[]`, keeping them clear of the plan pool.
const RATE_POOL: &str = "rate";

/// A Kimi For Coding account.
#[derive(Debug)]
pub struct Kimi {
    client: reqwest::Client,
    credential: Credential,
    base_url: String,
}

impl Kimi {
    /// Builds a client for one key.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Self::with_base_url(credential, BASE_URL.to_owned())
    }

    fn with_base_url(credential: Credential, base_url: String) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
            base_url,
        })
    }

    /// The URL this instance polls.
    pub fn usages_url(&self) -> String {
        format!("{}{USAGES_PATH}", self.base_url.trim_end_matches('/'))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let response = self
            .client
            .get(self.usages_url())
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        let status = response.status();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(status, retry_after.as_deref())?;

        let body = response.text().await.map_err(ProviderError::Transport)?;
        parse(&body, Timestamp::now())
    }
}

impl Provider for Kimi {
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

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    // `usage` is the plan allowance, and the plan allowance is the whole reason this
    // provider is on the card. A response without one is not a Kimi usage response.
    let usage = envelope
        .usage
        .as_ref()
        .ok_or_else(|| ProviderError::malformed("response carried no usage"))?;

    let length = WindowLength::from_secs(PLAN_WINDOW_SECS).expect("a week is not zero seconds");
    let mut measured = vec![Measured::new(
        WindowKey::for_length(length),
        length_title(length),
        Some(length),
        usage,
        "the plan allowance",
    )?];

    for (index, limit) in envelope.limits.iter().flatten().enumerate() {
        measured.push(rate_limit(index, limit)?);
    }

    // Two windows under one key is not a drawing problem, it is a storage one: the second
    // loads a row whose reading is already this fresh, so `ingest` files it under `stale`
    // and drops it. Silently. Refusing here is the honest answer, and the state that would
    // cause it — two entries in `limits[]` declaring the same window — has nothing else
    // about it to tell them apart by.
    for (index, one) in measured.iter().enumerate() {
        if measured[..index]
            .iter()
            .any(|other| other.window.key == one.window.key)
        {
            return Err(ProviderError::malformed(format!(
                "two windows arrived under the key {}, and nothing distinguishes them",
                one.window.key
            )));
        }
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows: measured.iter().map(|m| m.window.clone()).collect(),
        details: details(&envelope, &measured),
    })
}

/// One entry of `limits[]`, which describes its own window and must.
///
/// The entry carries a `detail` and a `window` and nothing else — no name, no id. A window
/// we cannot derive a length for could only be keyed on its position in the array, which
/// is the trap `CONTEXT.md` § Storage exists to forbid, so the response is refused
/// instead. The card keeps its last good reading underneath the error state.
fn rate_limit(index: usize, limit: &RateLimit) -> Result<Measured, ProviderError> {
    let what = format!("rate limit {}", index + 1);
    let length = limit
        .window
        .as_ref()
        .and_then(WindowDescriptor::length)
        .ok_or_else(|| {
            ProviderError::malformed(format!(
                "{what} does not describe a window we can measure, \
                 leaving nothing but its position to key it on"
            ))
        })?;
    let detail = limit
        .detail
        .as_ref()
        .ok_or_else(|| ProviderError::malformed(format!("{what} carries no detail")))?;
    Measured::new(
        WindowKey::for_pool(RATE_POOL, length),
        length_title(length),
        Some(length),
        detail,
        &what,
    )
}

/// One window, with the counts it was computed from kept for the detail rows.
#[derive(Debug)]
struct Measured {
    window: Window,
    used: i64,
    limit: i64,
    remaining: Option<i64>,
}

impl Measured {
    fn new(
        key: WindowKey,
        title: String,
        length: Option<WindowLength>,
        detail: &Detail,
        what: &str,
    ) -> Result<Self, ProviderError> {
        let limit = detail
            .limit
            .map(Number::get)
            .filter(|limit| *limit > 0)
            .ok_or_else(|| {
                ProviderError::malformed(format!("{what} reports no usable limit to divide by"))
            })?;
        let remaining = detail.remaining.map(Number::get);
        // `used` is authoritative where it is sent, and may exceed the limit during
        // overage; `remaining` is the fallback the five-hour entry actually needs.
        let used = match (detail.used.map(Number::get), remaining) {
            (Some(used), _) => used,
            (None, Some(remaining)) => limit - remaining,
            (None, None) => {
                return Err(ProviderError::malformed(format!(
                    "{what} reports a limit but neither used nor remaining, \
                     and an unmeasured quota must not be drawn as an unused one"
                )));
            }
        };
        Ok(Self {
            window: Window {
                key,
                title,
                used_percent: used.clamp(0, limit) as f64 * 100.0 / limit as f64,
                resets_at: resets_at(detail.reset_time.as_deref(), what)?,
                length,
            },
            used,
            limit,
            remaining,
        })
    }

    /// The `label: value` row this window contributes to the absolute counts.
    fn detail_row(&self) -> DetailRow {
        let mut value = format!("{} of {} used", self.used, self.limit);
        if let Some(remaining) = self.remaining {
            value.push_str(&format!(" · {remaining} left"));
        }
        DetailRow {
            label: self.window.title.clone(),
            value,
        }
    }
}

/// When a window rolls over, from the ISO-8601 instant beside it.
///
/// An absent or empty `resetTime` is a window with no pace mark, which is a state the
/// interface draws. A *present* one we cannot read is different: `resetTime` has one
/// spelling on this endpoint, and a value that is not it means we are no longer reading
/// the payload the way it is written. That fails the response rather than quietly costing
/// the mark, matching Claude.
fn resets_at(raw: Option<&str>, what: &str) -> Result<Option<Timestamp>, ProviderError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(|parsed| Timestamp::from_unix(parsed.unix_timestamp()).ok())
        .map(Some)
        .ok_or_else(|| ProviderError::malformed(format!("{what} has an unreadable resetTime")))
}

fn details(envelope: &Envelope, measured: &[Measured]) -> Vec<DetailSection> {
    let mut sections = Vec::new();
    let user = envelope.user.as_ref();

    if let Some(level) = user
        .and_then(|user| user.membership.as_ref())
        .and_then(|membership| membership.level.as_deref())
        .and_then(|level| enum_label(level, "LEVEL_"))
    {
        sections.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Level".to_owned(),
                value: level,
            }],
        });
    }

    // The whole point of this provider: the bar says 24%, this says 24 of 100 requests.
    sections.push(DetailSection {
        title: "Requests".to_owned(),
        rows: measured.iter().map(Measured::detail_row).collect(),
    });

    let mut account = Vec::new();
    if let Some(region) = user
        .and_then(|user| user.region.as_deref())
        .and_then(|region| enum_label(region, "REGION_"))
    {
        account.push(DetailRow {
            label: "Region".to_owned(),
            value: region,
        });
    }
    if let Some(parallel) = envelope
        .parallel
        .as_ref()
        .and_then(|parallel| parallel.limit)
        .map(Number::get)
        .filter(|limit| *limit > 0)
    {
        // Not a quota — a cap on requests in flight at once. It belongs beside the account
        // rather than under a bar, because nothing about it drains.
        account.push(DetailRow {
            label: "Parallel requests".to_owned(),
            value: parallel.to_string(),
        });
    }
    if !account.is_empty() {
        sections.push(DetailSection {
            title: "Account".to_owned(),
            rows: account,
        });
    }

    sections
}

/// One of Kimi's `PREFIX_VALUE` enums as a word, or `None` when it says nothing.
fn enum_label(raw: &str, prefix: &str) -> Option<String> {
    let trimmed = raw.trim();
    let label = title_case(trimmed.strip_prefix(prefix).unwrap_or(trimmed));
    (!label.is_empty()).then_some(label)
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    usage: Option<Detail>,
    #[serde(default)]
    limits: Option<Vec<RateLimit>>,
    #[serde(default)]
    parallel: Option<Parallel>,
}

#[derive(Debug, Deserialize)]
struct User {
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    membership: Option<Membership>,
}

#[derive(Debug, Deserialize)]
struct Membership {
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Parallel {
    #[serde(default)]
    limit: Option<Number>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    #[serde(default)]
    window: Option<WindowDescriptor>,
    #[serde(default)]
    detail: Option<Detail>,
}

/// The only self-describing window in the payload: `{duration, timeUnit}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowDescriptor {
    #[serde(default)]
    duration: i64,
    #[serde(default)]
    time_unit: String,
}

impl WindowDescriptor {
    /// Seconds in each unit this endpoint has a name for. An unfamiliar one yields no
    /// length, which `rate_limit` turns into a refusal — see there for why.
    fn unit_secs(&self) -> Option<u64> {
        match self.time_unit.as_str() {
            "TIME_UNIT_SECOND" => Some(1),
            "TIME_UNIT_MINUTE" => Some(60),
            "TIME_UNIT_HOUR" => Some(3_600),
            "TIME_UNIT_DAY" => Some(86_400),
            "TIME_UNIT_WEEK" => Some(604_800),
            _ => None,
        }
    }

    fn length(&self) -> Option<WindowLength> {
        let duration = u64::try_from(self.duration).ok()?;
        WindowLength::from_secs(self.unit_secs()?.checked_mul(duration)?)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    #[serde(default)]
    limit: Option<Number>,
    #[serde(default)]
    used: Option<Number>,
    #[serde(default)]
    remaining: Option<Number>,
    #[serde(default)]
    reset_time: Option<String>,
}

/// A count, however this endpoint decides to spell it today.
///
/// Everything numeric here arrives quoted — `"100"`, not `100` — which is a fact about
/// this API rather than a convention of the format, so the bare form is accepted too. A
/// value that is neither fails its entry, because a count we cannot read is not a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Number(i64);

impl Number {
    fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(raw) => raw
                .trim()
                .parse::<i64>()
                .map(Number)
                .map_err(|_| de::Error::custom(format!("{raw:?} is not a whole number"))),
            serde_json::Value::Number(raw) => raw
                .as_i64()
                .map(Number)
                .ok_or_else(|| de::Error::custom(format!("{raw} is not a whole number"))),
            other => Err(de::Error::custom(format!(
                "expected a count, found {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a live response, with this account's real consumption replaced by
    /// invented numbers — the shape is a fact about the API, the values are the user's.
    /// Note what the five-hour entry does *not* carry: a `used`.
    const LIVE_SHAPE: &str = r#"{
      "user": {
        "userId": "abcdefghijklmnopqrst",
        "region": "REGION_OVERSEA",
        "membership": {"level": "LEVEL_INTERMEDIATE"},
        "businessId": ""
      },
      "usage": {"limit": "100", "used": "24", "remaining": "76",
                "resetTime": "2026-08-22T20:00:09Z"},
      "limits": [
        {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
         "detail": {"limit": "100", "remaining": "90",
                    "resetTime": "2026-08-20T23:00:09Z"}}
      ],
      "parallel": {"limit": "20"},
      "totalQuota": {},
      "authentication": {"method": "METHOD_API_KEY", "scope": "FEATURE_CODING"},
      "subType": "TYPE_PURCHASE",
      "domain": "DOMAIN_NEXUS"
    }"#;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_787_000_000).expect("plausible")
    }

    fn parsed(body: &str) -> Snapshot {
        parse(body, now()).expect("parses")
    }

    fn usage_only(usage: &str) -> Snapshot {
        parsed(&format!(r#"{{"usage":{usage}}}"#))
    }

    fn find<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|w| w.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    #[test]
    fn the_plan_allowance_and_the_burst_limit_are_two_windows() {
        let snapshot = parsed(LIVE_SHAPE);
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["rate/w18000", "w604800"]);
        assert_eq!(snapshot.provider.as_str(), "kimi");
        assert_eq!(snapshot.captured_at, now());
    }

    #[test]
    fn the_numbers_arrive_as_strings() {
        // Quoted counts, including the ones that are plainly counts. Reading them as
        // strings is the whole reason `Number` exists.
        let snapshot = parsed(LIVE_SHAPE);
        assert_eq!(find(&snapshot, "w604800").used_percent, 24.0);
    }

    #[test]
    fn a_count_sent_unquoted_is_read_the_same_way() {
        let snapshot = usage_only(r#"{"limit":200,"used":50}"#);
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
    }

    #[test]
    fn a_count_that_is_not_a_number_fails_its_entry() {
        let err =
            parse(r#"{"usage":{"limit":"many","used":"1"}}"#, now()).expect_err("must refuse");
        assert!(matches!(err, ProviderError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn consumption_falls_back_to_the_remainder_when_used_is_absent() {
        // Live: the five-hour entry carries `limit` and `remaining` and no `used` at all.
        let snapshot = parsed(LIVE_SHAPE);
        assert_eq!(find(&snapshot, "rate/w18000").used_percent, 10.0);
    }

    #[test]
    fn a_quota_we_cannot_measure_is_refused_rather_than_drawn_as_unused() {
        // A bar reading 0% is a claim, and the dangerous one to make wrongly.
        let err = parse(r#"{"usage":{"limit":"100"}}"#, now()).expect_err("must refuse");
        assert!(
            format!("{err}").contains("neither used nor remaining"),
            "{err}"
        );
    }

    #[test]
    fn a_limit_of_zero_never_reaches_the_division() {
        assert!(parse(r#"{"usage":{"limit":"0","used":"0"}}"#, now()).is_err());
        assert!(parse(r#"{"usage":{"used":"3"}}"#, now()).is_err());
    }

    #[test]
    fn overage_fills_the_bar_without_overflowing_it() {
        let snapshot = usage_only(r#"{"limit":"100","used":"140"}"#);
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
        // The count itself is not clamped: the detail row still says what was spent.
        assert_eq!(snapshot.details[0].rows[0].value, "140 of 100 used");
    }

    #[test]
    fn the_plan_window_is_a_week_because_the_history_says_so() {
        // Nothing on the wire declares this. The corpus records this account's resetTime
        // advancing 2026-08-08 → 08-15 → 08-22 at exactly 168.00 hours a step.
        let snapshot = parsed(LIVE_SHAPE);
        let plan = find(&snapshot, "w604800");
        assert_eq!(plan.length, WindowLength::from_secs(7 * 86_400));
        assert_eq!(plan.title, "7 days");
        assert!(
            plan.pace(now()).is_some(),
            "a length and a reset make a pace mark"
        );
    }

    #[test]
    fn a_burst_limit_takes_the_length_it_declares() {
        let rate = find(&parsed(LIVE_SHAPE), "rate/w18000").clone();
        assert_eq!(rate.length, WindowLength::from_secs(18_000));
        assert_eq!(rate.title, "5 hours");
    }

    #[test]
    fn every_unit_this_endpoint_names_is_understood() {
        for (unit, duration, secs) in [
            ("TIME_UNIT_SECOND", 90, 90),
            ("TIME_UNIT_MINUTE", 300, 18_000),
            ("TIME_UNIT_HOUR", 5, 18_000),
            ("TIME_UNIT_DAY", 7, 604_800),
            ("TIME_UNIT_WEEK", 1, 604_800),
        ] {
            let snapshot = parsed(&format!(
                r#"{{"usage":{{"limit":"1","used":"0"}},
                     "limits":[{{"window":{{"duration":{duration},"timeUnit":"{unit}"}},
                                 "detail":{{"limit":"10","used":"1"}}}}]}}"#
            ));
            assert_eq!(
                find(&snapshot, &format!("rate/w{secs}")).length,
                WindowLength::from_secs(secs),
                "{unit}"
            );
        }
    }

    #[test]
    fn a_burst_limit_that_does_not_describe_its_window_is_refused() {
        // The entry carries no name and no id, so a window with no derivable length could
        // only be keyed on where it sat in the array — the trap this project forbids.
        for entry in [
            r#"{"detail":{"limit":"10","used":"1"}}"#,
            r#"{"window":{"duration":0,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"10","used":"1"}}"#,
            r#"{"window":{"duration":2,"timeUnit":"TIME_UNIT_FORTNIGHT"},"detail":{"limit":"10","used":"1"}}"#,
        ] {
            let err = parse(
                &format!(r#"{{"usage":{{"limit":"1","used":"0"}},"limits":[{entry}]}}"#),
                now(),
            )
            .expect_err("must refuse");
            assert!(format!("{err}").contains("position"), "{err}");
        }
    }

    #[test]
    fn two_windows_of_the_same_length_are_refused_rather_than_silently_merged() {
        // The second would load a row already this fresh, be reported stale, and vanish.
        let err = parse(
            r#"{"usage":{"limit":"1","used":"0"},"limits":[
                 {"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                  "detail":{"limit":"10","used":"1"}},
                 {"window":{"duration":5,"timeUnit":"TIME_UNIT_HOUR"},
                  "detail":{"limit":"20","used":"2"}}
               ]}"#,
            now(),
        )
        .expect_err("must refuse");
        assert!(format!("{err}").contains("rate/w18000"), "{err}");
    }

    #[test]
    fn reset_times_are_iso_8601_with_or_without_a_fraction() {
        let snapshot = parsed(LIVE_SHAPE);
        assert_eq!(
            find(&snapshot, "w604800").resets_at,
            Some(Timestamp::from_unix(1_787_428_809).expect("plausible"))
        );
        let fractional =
            usage_only(r#"{"limit":"1","used":"0","resetTime":"2026-08-22T20:00:09.716839300Z"}"#);
        assert_eq!(
            fractional.windows[0].resets_at,
            Some(Timestamp::from_unix(1_787_428_809).expect("plausible"))
        );
    }

    #[test]
    fn an_absent_reset_time_costs_the_pace_mark_and_nothing_else() {
        let snapshot = usage_only(r#"{"limit":"100","used":"20","resetTime":""}"#);
        assert_eq!(snapshot.windows[0].used_percent, 20.0);
        assert_eq!(snapshot.windows[0].resets_at, None);
        assert_eq!(snapshot.windows[0].pace(now()), None);
    }

    #[test]
    fn a_reset_time_we_cannot_read_fails_the_response() {
        // Not a missing mark — a sign we are no longer reading the payload as written.
        let err = parse(
            r#"{"usage":{"limit":"100","used":"20","resetTime":"22/08/2026 20:00"}}"#,
            now(),
        )
        .expect_err("must refuse");
        assert!(format!("{err}").contains("resetTime"), "{err}");
    }

    #[test]
    fn a_response_without_the_plan_allowance_is_not_a_usage_response() {
        let err = parse(r#"{"user":{"region":"REGION_OVERSEA"}}"#, now()).expect_err("must refuse");
        assert!(format!("{err}").contains("no usage"), "{err}");
    }

    #[test]
    fn the_absolute_counts_become_rows_under_the_bars() {
        let snapshot = parsed(LIVE_SHAPE);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Requests", "Account"]);
        assert_eq!(snapshot.details[0].rows[0].value, "Intermediate");
        assert_eq!(snapshot.details[1].rows[0].label, "7 days");
        assert_eq!(
            snapshot.details[1].rows[0].value,
            "24 of 100 used · 76 left"
        );
        assert_eq!(snapshot.details[1].rows[1].label, "5 hours");
        assert_eq!(
            snapshot.details[1].rows[1].value,
            "10 of 100 used · 90 left"
        );
        assert_eq!(snapshot.details[2].rows[0].value, "Oversea");
        assert_eq!(snapshot.details[2].rows[1].value, "20");
    }

    #[test]
    fn an_account_that_says_nothing_about_itself_gets_no_empty_sections() {
        let snapshot = usage_only(r#"{"limit":"100","used":"20"}"#);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Requests"], "the counts are always worth showing");
    }

    #[test]
    fn the_card_leads_with_the_five_hour_limit() {
        let dominant = parsed(LIVE_SHAPE)
            .dominant_window()
            .expect("present")
            .clone();
        assert_eq!(dominant.key.as_str(), "rate/w18000");
    }

    #[test]
    fn a_blank_key_is_refused_before_a_request_is_spent() {
        let kimi = Kimi::new(Credential::new("  ")).expect("client builds");
        let err = block_on(kimi.fetch_inner());
        assert!(err.expect_err("must refuse").needs_user_action());
    }

    #[test]
    fn the_endpoint_is_the_coding_product_and_not_the_open_platform() {
        let kimi = Kimi::new(Credential::new("sk-1")).expect("builds");
        assert_eq!(kimi.usages_url(), "https://api.kimi.com/coding/v1/usages");
        let overridden =
            Kimi::with_base_url(Credential::new("sk-1"), "http://127.0.0.1:9/".to_owned())
                .expect("builds");
        assert_eq!(
            overridden.usages_url(),
            "http://127.0.0.1:9/coding/v1/usages"
        );
    }

    /// The one place this module needs to drive a future to completion in a test.
    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }
}
