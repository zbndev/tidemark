//! Antigravity, through the local server the `agy` CLI brings up.
//!
//! The hardest of the five, and not because of the payload. Quota here is not an endpoint
//! on the internet — it is an RPC on a loopback HTTPS server with a self-signed certificate
//! that only exists while a CLI process is alive. [`agy`] is that half of the work; this
//! module is the meaning.
//!
//! # What the payload says, and what it does not
//!
//! `{response: {description, groups: [{displayName, description, buckets: [...]}]}}`, two
//! groups deep — *Gemini Models* and *Claude and GPT models* — each with its own weekly
//! bucket. Three things are worth stating before the code says them:
//!
//! 1. **It reports what is left, not what is spent.** `remainingFraction: 1` is an untouched
//!    quota, not a full one. Every other provider in this workspace reports consumption; this
//!    one has to be inverted, and getting the direction wrong would draw a card that is
//!    exactly as wrong as it is possible to be.
//! 2. **Two pools of the same length.** Both groups run weekly, so the length alone cannot
//!    key them and `WindowKey::for_pool` exists for precisely this case. The pool is taken
//!    from the `bucketId` — `gemini-weekly`, `3p-weekly` — because the group's `displayName`
//!    is display copy that can be reworded, and a key that follows the wording splits one
//!    pool's history the first time somebody edits a label.
//! 3. **The window's length arrived late.** The live payload names it (`window: "weekly"`);
//!    the shape recorded before it did not. So the length is read from the declaration when
//!    there is one and from the cadence in the bucket's own id when there is not, and the
//!    seven days it comes to is a measurement either way: this account's `gemini-weekly`
//!    reset advanced by exactly 168.00 hours twice in
//!    `~/.config/codexbar/history/antigravity.json`.
//!
//! # Readiness is not a status code
//!
//! An unauthenticated server answers this RPC `200` with a structurally valid body whose
//! buckets all read `remainingFraction: 1` — the same thing a genuinely unused quota looks
//! like. Storing it would write a fully-unused week into history for an account nobody is
//! logged into. Every fetch therefore passes [`agy::USER_STATUS_PATH`] first, and this
//! module's [`logged_in`] is the predicate that gate is built on.

pub mod agy;

use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{BoxFuture, Provider, ProviderError, length_title, title_case};
use agy::Agy;

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "antigravity";

/// How a bucket can spell the period it runs over, in either the `window` field or the
/// tail of its own `bucketId`.
///
/// Deliberately short. A cadence that is not here yields a window with no length, which
/// costs the pace mark and nothing else; a cadence guessed wrongly puts the mark in the
/// wrong place on a bar the user is being asked to trust. **`monthly` is absent on
/// purpose** — a month is not a number of seconds, and the only way to put one in this
/// table is to invent one.
const CADENCES: &[(&str, u64)] = &[
    ("weekly", 7 * 86_400),
    ("week", 7 * 86_400),
    ("daily", 86_400),
    ("day", 86_400),
    ("5h", 5 * 3_600),
    ("5-hour", 5 * 3_600),
    ("five_hour", 5 * 3_600),
    ("fivehour", 5 * 3_600),
];

/// An Antigravity account, seen through the local server.
#[derive(Debug)]
pub struct Antigravity {
    agy: Agy,
}

impl Antigravity {
    /// Builds the provider. No process is started until the first fetch.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self { agy: Agy::new()? })
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let ready = self.agy.ready().await?;
        let quota = self.agy.rpc(ready.port, agy::QUOTA_SUMMARY_PATH).await?;
        parse(&quota, &ready.status_body, Timestamp::now())
    }
}

impl Provider for Antigravity {
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

/// Whether the server has an account behind it, from a `GetUserStatus` body.
///
/// The readiness gate, and the reason a quota response is never trusted on its own. `Err`
/// carries the server's own sentence about it — while logged out it says so in words —
/// which is worth more in a log than any wording of ours.
pub fn logged_in(body: &str) -> Result<(), String> {
    let response: UserStatusResponse = serde_json::from_str(body)
        .map_err(|error| format!("the server's answer is not a user status: {error}"))?;
    let account = response.user_status.as_ref().is_some_and(|status| {
        nonempty(status.email.as_deref()).is_some() || status.plan_status.is_some()
    });
    if account {
        return Ok(());
    }
    Err(nonempty(response.message.as_deref())
        .unwrap_or("the server reports nobody logged in")
        .to_owned())
}

/// Turns a quota summary and the user status beside it into a snapshot.
///
/// Pure, and both bodies are arguments for the same reason: every trap in here — the
/// inverted fraction, the two pools of one length, the cadence that is sometimes declared
/// and sometimes only implied by an id — is reachable from a test without a running server.
pub fn parse(quota: &str, status: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let envelope: QuotaEnvelope = serde_json::from_str(quota)
        .map_err(|error| format!("not a quota summary: {error}"))
        .map_err(ProviderError::malformed)?;
    let payload = envelope.payload().ok_or_else(|| {
        ProviderError::malformed("the quota summary carried no groups of any kind")
    })?;

    let mut windows = Vec::new();
    let mut models = Vec::new();
    for (index, group) in payload.groups.iter().flatten().enumerate() {
        let title = nonempty(group.display_name.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Group {}", index + 1));
        for bucket in group.buckets.iter().flatten() {
            if let Some(window) = bucket.window(&title)? {
                windows.push(window);
            }
        }
        if let Some(covers) = nonempty(group.description.as_deref()) {
            models.push(DetailRow {
                label: title,
                value: covers
                    .trim_start_matches("Models within this group: ")
                    .to_owned(),
            });
        }
    }

    if windows.is_empty() {
        return Err(ProviderError::malformed(
            "the quota summary described no window this account is subject to",
        ));
    }
    // Two windows under one key is a storage failure rather than a drawing one: the second
    // loads a row already this fresh, is filed as stale, and disappears. See Kimi, where
    // the same check was written for the same reason.
    for (index, one) in windows.iter().enumerate() {
        if windows[..index].iter().any(|other| other.key == one.key) {
            return Err(ProviderError::malformed(format!(
                "two windows arrived under the key {}, and nothing distinguishes them",
                one.key
            )));
        }
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: details(status, models),
    })
}

/// The sections under the bars: who the account is on, and which models each bar covers.
///
/// A user status that will not parse costs the sections and nothing else. It has already
/// done its real job by the time this runs — it is what let the quota be believed — and
/// failing a good quota reading over the copy beneath it would be the wrong trade.
fn details(status: &str, models: Vec<DetailRow>) -> Vec<DetailSection> {
    let status: UserStatusResponse = serde_json::from_str(status).unwrap_or_default();
    let plan_status = status
        .user_status
        .as_ref()
        .and_then(|status| status.plan_status.as_ref());
    let mut sections = Vec::new();

    if let Some(plan) = plan_status
        .and_then(|status| status.plan_info.as_ref())
        .and_then(PlanInfo::preferred_name)
    {
        sections.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            // The provider's own display copy, so unlike a plan *slug* it is not re-cased.
            rows: vec![DetailRow {
                label: "Plan".to_owned(),
                value: plan.to_owned(),
            }],
        });
    }

    if !models.is_empty() {
        // The one thing the bars cannot say for themselves: a card with two weekly bars on
        // it is unreadable until you know which models are behind which.
        sections.push(DetailSection {
            title: "Models".to_owned(),
            rows: models,
        });
    }

    let credits: Vec<DetailRow> = [
        (
            "Prompt credits",
            plan_status.and_then(|s| s.available_prompt_credits),
        ),
        (
            "Flow credits",
            plan_status.and_then(|s| s.available_flow_credits),
        ),
    ]
    .into_iter()
    .filter_map(|(label, value)| {
        // Not a quota: credits are bought and spent rather than reset, so they belong
        // beside the account instead of under a bar.
        value.map(|value| DetailRow {
            label: label.to_owned(),
            value: value.to_string(),
        })
    })
    .collect();
    if !credits.is_empty() {
        sections.push(DetailSection {
            title: "Credits".to_owned(),
            rows: credits,
        });
    }

    sections
}

/// The pool a bucket draws on and how long its window runs, from its id and its own
/// declaration.
///
/// The id names both — `gemini-weekly` is the Gemini pool's weekly window — and the
/// declaration, where there is one, is the more authoritative half of that. What is left
/// after the cadence is stripped is the pool: `gemini`, `3p`. An id whose tail is not a
/// cadence we know is used whole, which keeps a pool we have never seen on one key rather
/// than splitting it on a guess.
fn pool_and_length(bucket_id: &str, declared: Option<&str>) -> (String, Option<WindowLength>) {
    let id = bucket_id.trim().to_ascii_lowercase();
    let (pool, from_id) = match id.rsplit_once(['-', '_']) {
        Some((head, tail)) if !head.is_empty() => match cadence(tail) {
            Some(seconds) => (head.to_owned(), Some(seconds)),
            None => (id.clone(), None),
        },
        _ => (id.clone(), None),
    };
    let seconds = declared.and_then(cadence).or(from_id);
    (pool, seconds.and_then(WindowLength::from_secs))
}

/// Seconds in a cadence this provider has been seen to name, or `None` for one it has not.
fn cadence(name: &str) -> Option<u64> {
    let name = name.trim().to_ascii_lowercase();
    CADENCES
        .iter()
        .find(|(spelling, _)| *spelling == name)
        .map(|(_, seconds)| *seconds)
}

/// The first of these strings that is not blank.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// When a window rolls over, from the ISO-8601 instant beside it.
///
/// Absent is a normal state — a window with no pace mark, which the interface draws.
/// Present and unreadable is not: `resetTime` has one spelling here, and a value that is
/// not it means we are no longer reading the payload the way it is written. Claude and Kimi
/// draw the same line in the same place.
fn resets_at(raw: Option<&str>, what: &str) -> Result<Option<Timestamp>, ProviderError> {
    let Some(raw) = nonempty(raw) else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(|parsed| Timestamp::from_unix(parsed.unix_timestamp()).ok())
        .map(Some)
        .ok_or_else(|| ProviderError::malformed(format!("{what} has an unreadable resetTime")))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaEnvelope {
    #[serde(default)]
    response: Option<Payload>,
    #[serde(default)]
    summary: Option<Payload>,
    #[serde(default)]
    groups: Option<Vec<Group>>,
}

impl QuotaEnvelope {
    /// The summary, wherever this build of the CLI decided to put it.
    fn payload(self) -> Option<Payload> {
        self.response.or(self.summary).or_else(|| {
            self.groups.map(|groups| Payload {
                groups: Some(groups),
            })
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    #[serde(default)]
    groups: Option<Vec<Group>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    buckets: Option<Vec<Bucket>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    #[serde(default)]
    bucket_id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    /// The cadence, where this build of the server sends one. `"weekly"`, live.
    #[serde(default)]
    window: Option<String>,
    /// The provider's own statement that this bucket is not in force.
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    remaining_fraction: Option<f64>,
    /// The older spelling, where the fraction sits one level down.
    #[serde(default)]
    remaining: Option<Remaining>,
    #[serde(default)]
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Remaining {
    #[serde(default)]
    remaining_fraction: Option<f64>,
}

impl Bucket {
    /// This bucket as a window, or `None` when the provider says it does not apply.
    fn window(&self, group: &str) -> Result<Option<Window>, ProviderError> {
        let id = nonempty(self.bucket_id.as_deref()).ok_or_else(|| {
            // Without an id there is nothing to key the window on but where it sat in the
            // array, which is the trap `CONTEXT.md` § Storage exists to forbid.
            ProviderError::malformed(format!(
                "a bucket of {group} arrived with no bucketId, \
                 leaving nothing but its position to key it on"
            ))
        })?;
        // Skipping is normally forbidden — a window missing from the card reads as "you
        // have no such limit". This is the one case where that reading is the true one:
        // the provider is not withholding the bucket, it is saying the bucket does not
        // apply to this account.
        if self.disabled == Some(true) {
            return Ok(None);
        }
        let what = format!("{group} bucket {id}");
        let fraction = self
            .remaining_fraction
            .or_else(|| self.remaining.as_ref().and_then(|r| r.remaining_fraction))
            .filter(|fraction| fraction.is_finite())
            .ok_or_else(|| {
                // A bar drawn over a quota nobody measured is the dangerous direction, and
                // here it would read as untouched — the exact value an unauthenticated
                // server hands out.
                ProviderError::malformed(format!(
                    "{what} reports no remaining fraction, and an unmeasured quota \
                     must not be drawn as an unused one"
                ))
            })?;
        let (pool, length) = pool_and_length(id, self.window.as_deref());
        // Remaining, not used. Inverted here, once, deliberately.
        let used_percent = (1.0 - fraction.clamp(0.0, 1.0)) * 100.0;
        let span = match length {
            Some(length) => length_title(length),
            None => nonempty(self.display_name.as_deref())
                .map(str::to_owned)
                .unwrap_or_else(|| title_case(id)),
        };
        Ok(Some(Window {
            key: match length {
                Some(length) => WindowKey::for_pool(&pool, length),
                // The one place this provider needs a bare name. It is safe where Codex's
                // slot names were not: `bucketId` is the bucket's own identifier and names
                // its own cadence, so unlike `primary_window` it cannot come to mean a
                // different window between two responses.
                None => WindowKey::named(id),
            },
            title: format!("{group} · {span}"),
            used_percent,
            resets_at: resets_at(self.reset_time.as_deref(), &what)?,
            length,
        }))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserStatusResponse {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    user_status: Option<UserStatus>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserStatus {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    plan_status: Option<PlanStatus>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanStatus {
    #[serde(default)]
    plan_info: Option<PlanInfo>,
    #[serde(default)]
    available_prompt_credits: Option<i64>,
    #[serde(default)]
    available_flow_credits: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanInfo {
    #[serde(default)]
    plan_display_name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    product_name: Option<String>,
    #[serde(default)]
    plan_name: Option<String>,
    #[serde(default)]
    plan_short_name: Option<String>,
}

impl PlanInfo {
    /// The most presentable of the five spellings this object has been seen to carry.
    fn preferred_name(&self) -> Option<&str> {
        [
            self.plan_display_name.as_deref(),
            self.display_name.as_deref(),
            self.product_name.as_deref(),
            self.plan_name.as_deref(),
            self.plan_short_name.as_deref(),
        ]
        .into_iter()
        .find_map(nonempty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body the live server returned on 2026-08-20, verbatim in shape. Both groups run
    /// weekly, both declare the cadence, and both were untouched — which is exactly the
    /// value an unauthenticated server also produces, and the reason for the gate.
    const LIVE_QUOTA: &str = r#"{
      "response": {
        "groups": [
          {
            "displayName": "Gemini Models",
            "description": "Models within this group: Gemini Flash, Gemini Pro",
            "buckets": [
              {"bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining",
               "window": "weekly", "remainingFraction": 0.62,
               "resetTime": "2026-08-27T20:38:22Z"}
            ]
          },
          {
            "displayName": "Claude and GPT models",
            "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS",
            "buckets": [
              {"bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining",
               "window": "weekly", "remainingFraction": 1,
               "resetTime": "2026-08-27T20:38:22Z"}
            ]
          }
        ],
        "description": "Within each group, models share a weekly limit."
      }
    }"#;

    /// The shape recorded before `window` existed: the fraction one level down, no cadence
    /// declared anywhere but the bucket id, and a five-hour bucket beside the weekly one.
    const OLDER_QUOTA: &str = r#"{
      "response": {
        "groups": [
          {
            "displayName": "Gemini Models",
            "buckets": [
              {"bucketId": "gemini-weekly", "displayName": "Weekly Limit",
               "remaining": {"remainingFraction": 0.82},
               "resetTime": "2026-08-27T08:45:39Z"},
              {"bucketId": "gemini-5h", "displayName": "Five Hour Limit",
               "remaining": {"remainingFraction": 0.91},
               "resetTime": "2026-08-20T23:39:34Z"}
            ]
          }
        ]
      }
    }"#;

    const LIVE_STATUS: &str = r#"{
      "userStatus": {
        "name": "A Person",
        "email": "person@example.invalid",
        "planStatus": {
          "planInfo": {"teamsTier": "TEAMS_TIER_PRO", "planName": "Pro",
                       "monthlyPromptCredits": 50000},
          "availablePromptCredits": 500,
          "availableFlowCredits": 100
        }
      }
    }"#;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_787_000_000).expect("plausible")
    }

    fn parsed(quota: &str) -> Snapshot {
        parse(quota, LIVE_STATUS, now()).expect("parses")
    }

    fn find<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|window| window.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    fn one_bucket(bucket: &str) -> Result<Snapshot, ProviderError> {
        parse(
            &format!(r#"{{"response":{{"groups":[{{"displayName":"G","buckets":[{bucket}]}}]}}}}"#),
            LIVE_STATUS,
            now(),
        )
    }

    #[test]
    fn both_model_groups_arrive_as_their_own_weekly_window() {
        let snapshot = parsed(LIVE_QUOTA);
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["3p/w604800", "gemini/w604800"]);
        assert_eq!(snapshot.provider.as_str(), "antigravity");
        assert_eq!(snapshot.captured_at, now());
    }

    #[test]
    fn the_fraction_reported_is_what_is_left_and_the_bar_wants_what_is_spent() {
        // The single most consequential line in this module: 0.62 remaining is 38% used.
        let snapshot = parsed(LIVE_QUOTA);
        assert!((find(&snapshot, "gemini/w604800").used_percent - 38.0).abs() < 1e-9);
        assert_eq!(find(&snapshot, "3p/w604800").used_percent, 0.0);
    }

    #[test]
    fn two_pools_of_one_length_stay_two_histories() {
        // Both are weekly, so the length alone would put them on one key and lose one.
        let snapshot = parsed(LIVE_QUOTA);
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            find(&snapshot, "gemini/w604800").length,
            WindowLength::from_secs(604_800)
        );
        assert_eq!(
            find(&snapshot, "3p/w604800").title,
            "Claude and GPT models · 7 days"
        );
    }

    #[test]
    fn a_pace_mark_needs_the_length_and_the_reset_and_here_it_has_both() {
        let gemini = find(&parsed(LIVE_QUOTA), "gemini/w604800").clone();
        assert_eq!(
            gemini.resets_at,
            Some(Timestamp::from_unix(1_787_863_102).expect("plausible"))
        );
        assert!(gemini.pace(now()).is_some());
    }

    #[test]
    fn the_cadence_is_read_from_the_bucket_id_when_the_payload_does_not_declare_it() {
        // The shape recorded before `window` existed still keys and paces correctly.
        let snapshot = parse(OLDER_QUOTA, LIVE_STATUS, now()).expect("parses");
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["gemini/w18000", "gemini/w604800"]);
        assert_eq!(
            find(&snapshot, "gemini/w18000").title,
            "Gemini Models · 5 hours"
        );
    }

    #[test]
    fn the_nested_spelling_of_the_fraction_is_read_the_same_way() {
        let snapshot = parse(OLDER_QUOTA, LIVE_STATUS, now()).expect("parses");
        assert!((find(&snapshot, "gemini/w604800").used_percent - 18.0).abs() < 1e-9);
    }

    #[test]
    fn a_declared_cadence_wins_over_the_one_in_the_id() {
        let snapshot =
            one_bucket(r#"{"bucketId":"gemini-weekly","window":"daily","remainingFraction":0.5}"#)
                .expect("parses");
        assert_eq!(snapshot.windows[0].key.as_str(), "gemini/w86400");
    }

    #[test]
    fn a_cadence_nobody_has_seen_costs_the_pace_mark_and_keeps_the_window() {
        // Monthly is the live case: no fixed number of seconds, so no length is invented.
        let snapshot = one_bucket(
            r#"{"bucketId":"gemini-monthly","displayName":"Monthly Limit",
                "window":"monthly","remainingFraction":0.25}"#,
        )
        .expect("parses");
        let window = &snapshot.windows[0];
        assert_eq!(window.key.as_str(), "gemini-monthly");
        assert_eq!(window.length, None);
        assert_eq!(window.title, "G · Monthly Limit");
        assert_eq!(window.used_percent, 75.0);
        assert_eq!(window.pace(now()), None);
    }

    #[test]
    fn a_bucket_with_no_id_is_refused_rather_than_keyed_on_its_position() {
        let error = one_bucket(r#"{"displayName":"Weekly","remainingFraction":0.5}"#)
            .expect_err("must refuse");
        assert!(format!("{error}").contains("position"), "{error}");
    }

    #[test]
    fn a_quota_we_cannot_measure_is_refused_rather_than_drawn_as_unused() {
        for bucket in [
            r#"{"bucketId":"gemini-weekly","displayName":"Weekly"}"#,
            r#"{"bucketId":"gemini-weekly","remaining":{}}"#,
        ] {
            let error = one_bucket(bucket).expect_err("must refuse");
            assert!(format!("{error}").contains("unused one"), "{error}");
        }
    }

    #[test]
    fn a_bucket_the_provider_says_is_not_in_force_is_not_drawn_as_a_limit() {
        let snapshot = parse(
            r#"{"response":{"groups":[{"displayName":"G","buckets":[
                 {"bucketId":"gemini-weekly","remainingFraction":0.5},
                 {"bucketId":"gemini-5h","disabled":true,"remainingFraction":0.5}
               ]}]}}"#,
            LIVE_STATUS,
            now(),
        )
        .expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["gemini/w604800"]);
    }

    #[test]
    fn a_summary_with_nothing_in_it_is_refused() {
        for body in [
            r#"{}"#,
            r#"{"response":{"groups":[]}}"#,
            r#"{"response":{"groups":[{"displayName":"G","buckets":[]}]}}"#,
        ] {
            assert!(parse(body, LIVE_STATUS, now()).is_err(), "{body}");
        }
    }

    #[test]
    fn two_buckets_that_cannot_be_told_apart_are_refused_rather_than_silently_merged() {
        let error = parse(
            r#"{"response":{"groups":[{"displayName":"G","buckets":[
                 {"bucketId":"gemini-weekly","remainingFraction":0.5},
                 {"bucketId":"gemini-week","remainingFraction":0.9}
               ]}]}}"#,
            LIVE_STATUS,
            now(),
        )
        .expect_err("must refuse");
        assert!(format!("{error}").contains("gemini/w604800"), "{error}");
    }

    #[test]
    fn a_reset_time_we_cannot_read_fails_the_response() {
        let error = one_bucket(
            r#"{"bucketId":"gemini-weekly","remainingFraction":0.5,"resetTime":"27/08/2026"}"#,
        )
        .expect_err("must refuse");
        assert!(format!("{error}").contains("resetTime"), "{error}");
    }

    #[test]
    fn an_absent_reset_time_costs_the_pace_mark_and_nothing_else() {
        let snapshot =
            one_bucket(r#"{"bucketId":"gemini-weekly","remainingFraction":0.5,"resetTime":""}"#)
                .expect("parses");
        assert_eq!(snapshot.windows[0].resets_at, None);
        assert_eq!(snapshot.windows[0].used_percent, 50.0);
    }

    #[test]
    fn the_summary_is_found_wherever_the_server_puts_it() {
        let inner = r#""groups":[{"displayName":"G","buckets":[
            {"bucketId":"gemini-weekly","remainingFraction":0.5}]}]"#;
        for body in [
            format!(r#"{{"response":{{{inner}}}}}"#),
            format!(r#"{{"summary":{{{inner}}}}}"#),
            format!(r#"{{{inner}}}"#),
        ] {
            assert_eq!(parsed(&body).windows.len(), 1, "{body}");
        }
    }

    #[test]
    fn the_sections_say_who_the_account_is_and_what_each_bar_covers() {
        let snapshot = parsed(LIVE_QUOTA);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Models", "Credits"]);
        assert_eq!(snapshot.details[0].rows[0].value, "Pro");
        assert_eq!(snapshot.details[1].rows[0].label, "Gemini Models");
        assert_eq!(
            snapshot.details[1].rows[0].value, "Gemini Flash, Gemini Pro",
            "the sentence the provider wraps the list in is not news twice"
        );
        assert_eq!(snapshot.details[2].rows[0].value, "500");
        assert_eq!(snapshot.details[2].rows[1].value, "100");
    }

    #[test]
    fn a_user_status_we_cannot_read_costs_the_sections_and_not_the_reading() {
        // It has already done its real job by then: it is what let the quota be believed.
        let snapshot = parse(LIVE_QUOTA, "not json at all", now()).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert!(snapshot.details.iter().all(|s| s.title == "Models"));
    }

    #[test]
    fn readiness_is_the_presence_of_an_account_and_not_a_status_code() {
        assert!(logged_in(LIVE_STATUS).is_ok());
        assert!(logged_in(r#"{"userStatus":{"planStatus":{"planInfo":{}}}}"#).is_ok());
    }

    #[test]
    fn the_gate_is_the_only_thing_that_tells_a_pre_auth_answer_from_a_real_one() {
        // E2's finding, and it cost two attempts to see: before authentication finishes the
        // server answers this RPC 200 with a body that parses perfectly and reads as an
        // untouched quota. Nothing in the quota summary distinguishes it, and `parse` is
        // right not to try — there is nothing there to look at.
        const PRE_AUTH: &str = r#"{"response":{"groups":[{"displayName":"Gemini Models",
            "buckets":[{"bucketId":"gemini-weekly","window":"weekly","remainingFraction":1}]}]}}"#;
        let snapshot = parsed(PRE_AUTH);
        assert_eq!(snapshot.windows[0].used_percent, 0.0);
        assert!(
            logged_in(r#"{"message":"You are not logged into Antigravity."}"#).is_err(),
            "the separation lives here and nowhere else"
        );
    }

    #[test]
    fn a_server_with_nobody_logged_in_says_so_and_is_believed() {
        // The whole reason the gate exists: this same server answers the quota RPC 200
        // with every bucket reading fully unused.
        let error = logged_in(r#"{"message":"You are not logged into Antigravity."}"#)
            .expect_err("must refuse");
        assert_eq!(error, "You are not logged into Antigravity.");
        assert!(logged_in(r#"{"userStatus":{"email":"  "}}"#).is_err());
        assert!(logged_in("<html>").is_err());
    }

    #[test]
    fn the_pool_is_the_bucket_id_with_its_cadence_taken_off() {
        assert_eq!(pool_and_length("gemini-weekly", None).0, "gemini");
        assert_eq!(pool_and_length("3p-weekly", Some("weekly")).0, "3p");
        assert_eq!(pool_and_length("gemini_5h", None).0, "gemini");
        // Nothing recognisable to strip: kept whole rather than split on a guess.
        assert_eq!(pool_and_length("gemini-session", None).0, "gemini-session");
        assert_eq!(pool_and_length("weekly", None).0, "weekly");
    }
}
