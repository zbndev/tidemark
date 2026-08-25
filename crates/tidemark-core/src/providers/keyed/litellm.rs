//! LiteLLM.
//!
//! Ported from CodexBar's `Providers/LiteLLM/LiteLLMUsageFetcher.swift`; the recorded
//! bodies in `LiteLLMUsageFetcherTests.swift` and `LiteLLMMenuCardModelTests.swift` are
//! the contract. Never seen answering: every number in the tests below is a body
//! CodexBar recorded.
//!
//! # The two-request ladder
//!
//! LiteLLM is software other people run, so the base URL is a required free-text
//! option read through [`keyed::base_url`](super::base_url) — HTTPS, or plain HTTP on
//! loopback. A base whose path ends in `/v1` — the inference path — is trimmed back to
//! the management root, exactly as the source's `managementBaseURL` does; `/key/info`,
//! `/user/info` and `/team/info` append to that root.
//!
//! `GET /key/info` first: it names the user or team the key belongs to — `user_id`
//! preferred over `team_id` — plus the key's own name, spend and expiry. It never
//! feeds a budget: every window on the card comes from the second body, and a
//! `/key/info` that names neither a user nor a team is the source's `missingUserID`,
//! malformed here. CodexBar's own tests assert the ladder is always two requests.
//!
//! The second request is `/user/info?user_id=<id>` for a user key, or
//! `/team/info?team_id=<id>` for a team-only key. A response naming a *different* id
//! than `/key/info` did is refused, as the source refuses it.
//!
//! # The reading
//!
//! A user key reads a personal budget — `max_budget` against `spend`, reset at
//! `budget_reset_at` — and, when `/key/info` also named a team, that team's budget out
//! of the `teams` array: the entry whose `team_id` matches; the others are present on
//! the wire and read by nobody. A team-only key reads one budget from `team_info`.
//!
//! A budget is a quantity against a stated limit. The personal one states no duration,
//! so it is keyed `personal` with no length — a budget with no stated span has nothing
//! to key on. A team budget stating a `budget_duration` of whole days (`7d`, `30d`)
//! keys on that length and earns a pace mark; any other spelling reads as no length
//! and falls back to the `team` key, rather than guessed at. A section with no budget,
//! or a limit that is not positive, draws no window and keeps its spend in the details
//! — the source does the same, demoting a budgetless spend to a cost line with no
//! meter.
//!
//! Dates are read leniently, as the source reads them: an unreadable `expires` or
//! `budget_reset_at` is no date, not a failure.
//!
//! # What ships untested
//!
//! No recorded body carries a personal `budget_reset_at` (both recorded user bodies
//! send `null`); the menu-card body's team and personal resets are exercised instead,
//! through the same parser. No recorded body carries a `budget_duration` this port
//! cannot read (`1mo` and friends) or a spend or budget of the wrong JSON type — the
//! error-path bodies for those are constructed, as the porting procedure allows, and
//! no number in a passing assertion is invented. The 401 a rejected key earns is
//! mapped by the shared transport, not by a parser, so it is tested by no unit here.

use super::{HandSpec, OptionSchema, Options, base_url, redact_query, required};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "litellm";

/// Name of the base-URL setting under `[provider.litellm]`.
pub const BASE_URL: &str = "base_url";

/// LiteLLM as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "LiteLLM",
    credential: CredentialKind::Key,
    credential_hint: "A virtual key from your LiteLLM deployment's admin console.",
    options: &[OptionSchema {
        name: BASE_URL,
        title: "Base URL",
        description: Some(
            "Host of the LiteLLM deployment to poll; HTTPS, or HTTP on loopback. A path ending in /v1 is trimmed to the management root.",
        ),
        default: "",
        choices: &[],
        required: true,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The base
/// URL is required — LiteLLM has no default host — and resolved here, so a changed one
/// takes effect on the next build.
fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(LiteLLM::new(credential, options)?))
}

/// One LiteLLM deployment: the key, and the management root its two endpoints hang
/// from.
pub struct LiteLLM {
    client: reqwest::Client,
    credential: Credential,
    management: String,
}

impl LiteLLM {
    /// Builds a client. The `/v1` trimming happens once, here, so every request of a
    /// fetch agrees on the root.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        let raw = required(options, BASE_URL, "Base URL")?;
        // `required` proved a value exists; the shared reader then enforces the
        // HTTPS-or-loopback rule on it.
        let base = base_url(&Options::from([(BASE_URL.to_owned(), raw)]), BASE_URL, "")?;
        Ok(Self {
            client: http::client()?,
            credential,
            management: management_root(&base),
        })
    }

    /// The `/key/info` URL this instance polls.
    pub fn key_info_url(&self) -> String {
        format!("{}/key/info", self.management)
    }

    /// The `/user/info` URL for one user.
    pub fn user_info_url(&self, user_id: &str) -> String {
        format!("{}/user/info?user_id={}", self.management, user_id)
    }

    /// The `/team/info` URL for one team.
    pub fn team_info_url(&self, team_id: &str) -> String {
        format!("{}/team/info?team_id={}", self.management, team_id)
    }

    /// The `/key/info` request, built but not sent, so the placement of the key is
    /// testable without a server.
    fn key_info_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(self.key_info_url())
    }

    /// The `/user/info` request, likewise.
    fn user_info_request(&self, user_id: &str) -> Result<reqwest::Request, ProviderError> {
        self.get(self.user_info_url(user_id))
    }

    /// The `/team/info` request, likewise.
    fn team_info_request(&self, team_id: &str) -> Result<reqwest::Request, ProviderError> {
        self.get(self.team_info_url(team_id))
    }

    /// One management GET, authenticated the one way LiteLLM accepts.
    fn get(&self, url: String) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let body = super::request(PROVIDER_ID, &self.client, self.key_info_request()?).await?;
        let key = parse_key_info(&body)?;
        let reading = if key.user_id.is_some() {
            let id = key.user_id.as_deref().expect("checked above");
            let body =
                super::request(PROVIDER_ID, &self.client, self.user_info_request(id)?).await?;
            parse_user_info(&body, &key)?
        } else if key.team_id.is_some() {
            let id = key.team_id.as_deref().expect("checked above");
            let body =
                super::request(PROVIDER_ID, &self.client, self.team_info_request(id)?).await?;
            Reading::Team(parse_team_info(&body, &key)?)
        } else {
            // Unreachable — `parse_key_info` refuses a body naming neither — but the
            // ladder states it, so the fetch does too.
            return Err(ProviderError::malformed(
                "the LiteLLM key info did not include a user_id or team_id",
            ));
        };
        Ok(snapshot(&reading, &key, now))
    }
}

impl fmt::Debug for LiteLLM {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiteLLM")
            .field("id", &PROVIDER_ID)
            .field("management", &self.management)
            .finish_non_exhaustive()
    }
}

impl Provider for LiteLLM {
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

/// The management root of a deployment base: the `/v1` inference segment, and only
/// that, is trimmed. Pure, so the recorded URL spellings are reachable from a test.
fn management_root(base: &str) -> String {
    let path = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base);
    match path.rsplit_once('/') {
        Some((head, "v1")) if !head.is_empty() => {
            format!("{}://{}", scheme_of(base), head)
        }
        _ => base.trim_end_matches('/').to_owned(),
    }
}

/// The scheme a base states, for re-joining a trimmed path onto its host.
fn scheme_of(base: &str) -> &str {
    if base.starts_with("http://") {
        "http"
    } else {
        "https"
    }
}

/// What `/key/info` said about the key. Every budget comes from the second body; these
/// are the identity and the labels.
#[derive(Debug, Clone, PartialEq)]
struct KeyInfo {
    user_id: Option<String>,
    team_id: Option<String>,
    key_name: Option<String>,
    #[allow(dead_code)]
    spend_usd: f64,
    expires_at: Option<Timestamp>,
}

/// One budget as the card draws it.
#[derive(Debug, Clone, PartialEq)]
struct Budget {
    spend: f64,
    limit: Option<f64>,
    resets_at: Option<Timestamp>,
    /// `budget_duration` as recorded — `7d`, `30d` — when the wire states one.
    duration: Option<String>,
}

/// What the second request said. A user key reads a personal budget and maybe a team's;
/// a team-only key reads one team budget.
#[derive(Debug, PartialEq)]
enum Reading {
    User {
        email: Option<String>,
        personal: Budget,
        team: Option<TeamBudget>,
    },
    Team(TeamBudget),
}

/// A team's budget, with the alias the wire names it by.
#[derive(Debug, Clone, PartialEq)]
struct TeamBudget {
    alias: Option<String>,
    budget: Budget,
}

/// Reads the `/key/info` body. Pure, and whole: the `info` object must exist, its
/// numbers must be numbers, and it must name a user or a team — the source's
/// `missingUserID` for the last.
fn parse_key_info(body: &str) -> Result<KeyInfo, ProviderError> {
    let root = object(body, "the LiteLLM key info")?;
    let info = member_object(&root, "info", "the LiteLLM key info")?;
    let user_id = non_empty(info.get("user_id"));
    let team_id = non_empty(info.get("team_id"));
    if user_id.is_none() && team_id.is_none() {
        return Err(ProviderError::malformed(
            "the LiteLLM key info did not include a user_id or team_id",
        ));
    }
    Ok(KeyInfo {
        user_id,
        team_id,
        key_name: non_empty(info.get("key_name")),
        spend_usd: opt_number(info.get("spend"), "spend")?.unwrap_or(0.0),
        expires_at: lenient_date(info.get("expires")),
    })
}

/// Reads the `/user/info` body against the key's identity. Pure, and whole: a response
/// naming a different user than `/key/info` did is refused, as are numbers that are not
/// numbers.
fn parse_user_info(body: &str, key: &KeyInfo) -> Result<Reading, ProviderError> {
    let root = object(body, "the LiteLLM user info")?;
    let info = member_object(&root, "user_info", "the LiteLLM user info")?;
    let expected = key
        .user_id
        .as_deref()
        .ok_or_else(|| ProviderError::malformed("/user/info requested without a user_id"))?;
    let response_id = non_empty(info.get("user_id")).or_else(|| non_empty(root.get("user_id")));
    if response_id.as_deref().is_some_and(|id| id != expected) {
        return Err(ProviderError::malformed(
            "the LiteLLM user_id did not match /key/info",
        ));
    }
    let metadata = info.get("metadata").and_then(Value::as_object);
    let email = ["user_email", "user_alias"]
        .iter()
        .find_map(|name| non_empty(info.get(*name)))
        .or_else(|| metadata.and_then(|metadata| non_empty(metadata.get("preferred_username"))));
    let personal = Budget {
        spend: opt_number(info.get("spend"), "spend")?.unwrap_or(0.0),
        limit: opt_number(info.get("max_budget"), "max_budget")?,
        resets_at: lenient_date(info.get("budget_reset_at")),
        duration: None,
    };
    let team = match (&key.team_id, root.get("teams")) {
        (Some(expected), Some(Value::Array(teams))) => {
            let mut matched = None;
            for entry in teams {
                let entry = entry.as_object().ok_or_else(|| {
                    ProviderError::malformed("a LiteLLM teams entry must be an object")
                })?;
                // Every entry is decoded strictly, as the Swift decoder decodes the
                // whole array; the one that matches is then the reading.
                let budget = Budget {
                    spend: opt_number(entry.get("spend"), "spend")?.unwrap_or(0.0),
                    limit: opt_number(entry.get("max_budget"), "max_budget")?,
                    resets_at: lenient_date(entry.get("budget_reset_at")),
                    duration: non_empty(entry.get("budget_duration")),
                };
                if non_empty(entry.get("team_id")).as_deref() == Some(expected) && matched.is_none()
                {
                    matched = Some(TeamBudget {
                        alias: non_empty(entry.get("team_alias")),
                        budget,
                    });
                }
            }
            matched
        }
        _ => None,
    };
    Ok(Reading::User {
        email,
        personal,
        team,
    })
}

/// Reads the `/team/info` body against the key's identity. Pure, and whole for the same
/// reasons.
fn parse_team_info(body: &str, key: &KeyInfo) -> Result<TeamBudget, ProviderError> {
    let root = object(body, "the LiteLLM team info")?;
    let info = member_object(&root, "team_info", "the LiteLLM team info")?;
    let expected = key
        .team_id
        .as_deref()
        .ok_or_else(|| ProviderError::malformed("/team/info requested without a team_id"))?;
    let response_id = non_empty(info.get("team_id")).or_else(|| non_empty(root.get("team_id")));
    if response_id.as_deref().is_some_and(|id| id != expected) {
        return Err(ProviderError::malformed(
            "the LiteLLM team_id did not match /key/info",
        ));
    }
    Ok(TeamBudget {
        alias: non_empty(info.get("team_alias")),
        budget: Budget {
            spend: opt_number(info.get("spend"), "spend")?.unwrap_or(0.0),
            limit: opt_number(info.get("max_budget"), "max_budget")?,
            resets_at: lenient_date(info.get("budget_reset_at")),
            duration: non_empty(info.get("budget_duration")),
        },
    })
}

/// Assembles the snapshot. Pure, so every recorded budget spelling is reachable from a
/// test.
///
/// A budget with a positive limit is a window; one without is a row, and the spend
/// survives either way. The personal window keys `personal` — it states no duration, so
/// there is no length to key on — and a team window keys on its `budget_duration` when
/// that is whole days, else `team`.
fn snapshot(reading: &Reading, key: &KeyInfo, now: Timestamp) -> Snapshot {
    let mut windows = Vec::new();
    let mut rows = Vec::new();
    match reading {
        Reading::User { personal, .. } => {
            if let Some(window) = budget_window("Personal budget", personal, None) {
                windows.push(window);
                rows.push(labeled(
                    "Personal spend",
                    against(personal.spend, personal.limit),
                ));
            } else {
                rows.push(labeled("Personal spend", usd(personal.spend)));
            }
        }
        Reading::Team(_) => rows.push(labeled(
            "Personal spend",
            "No personal budget on a team key".to_owned(),
        )),
    }
    if let Some(team) = team_of(reading) {
        if let Some(window) = budget_window("Team budget", &team.budget, team.alias.as_deref()) {
            windows.push(window);
        }
        rows.push(labeled("Team spend", team_line(team)));
    }
    if let Reading::User { email, .. } = reading
        && let Some(email) = email
    {
        rows.push(labeled("Account", email.clone()));
    }

    let mut details = vec![DetailSection {
        title: "Budgets".to_owned(),
        rows,
    }];
    let mut key_rows = Vec::new();
    if let Some(name) = &key.key_name {
        key_rows.push(labeled("Name", name.clone()));
    }
    if let Some(expires) = key.expires_at {
        key_rows.push(labeled("Expires", day_of(expires)));
    }
    if !key_rows.is_empty() {
        details.push(DetailSection {
            title: "API key".to_owned(),
            rows: key_rows,
        });
    }
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows,
        details,
    }
}

/// The team half of a reading, whichever body it came from.
fn team_of(reading: &Reading) -> Option<&TeamBudget> {
    match reading {
        Reading::User { team, .. } => team.as_ref(),
        Reading::Team(team) => Some(team),
    }
}

/// One budget as a window, when its limit is positive. A limit that is absent or not
/// positive is no window — there is nothing to divide by — and the caller keeps the
/// spend as a row.
fn budget_window(title: &str, budget: &Budget, alias: Option<&str>) -> Option<Window> {
    let limit = budget.limit.filter(|limit| *limit > 0.0)?;
    let length = budget
        .duration
        .as_deref()
        .and_then(duration_secs)
        .and_then(WindowLength::from_secs);
    let key = match length {
        // A team budget stating whole days keys on its span; the span is what the
        // window is.
        Some(length) => WindowKey::for_length(length),
        // No stated duration: nothing to key on but the section itself.
        None => WindowKey::named(if alias.is_some() { "team" } else { "personal" }),
    };
    let subtitle = match alias {
        Some(alias) => format!("Team {alias}: {} / {}", usd(budget.spend), usd(limit)),
        None => format!("{} / {}", usd(budget.spend), usd(limit)),
    };
    Some(Window {
        key,
        title: title.to_owned(),
        subtitle: Some(subtitle),
        used_percent: (budget.spend / limit * 100.0).clamp(0.0, 100.0),
        resets_at: budget.resets_at,
        length,
    })
}

/// A `budget_duration` of whole days, the only spelling this port reads. `7d` is seven
/// days; anything else — `1mo`, `3600s`, the empty string — is no length, not a guess.
fn duration_secs(raw: &str) -> Option<u64> {
    let days = raw.strip_suffix('d')?.trim().parse::<u64>().ok()?;
    days.checked_mul(86_400)
}

/// The team's details line: the alias when the wire states one, both absolutes when
/// there is a limit to divide by.
fn team_line(team: &TeamBudget) -> String {
    let spend = against(team.budget.spend, team.budget.limit);
    match &team.alias {
        Some(alias) => format!("{alias} · {spend}"),
        None => spend,
    }
}

/// Spend against a stated limit, the source's own spelling.
fn against(spend: f64, limit: Option<f64>) -> String {
    match limit.filter(|limit| *limit > 0.0) {
        Some(limit) => format!("{} / {}", usd(spend), usd(limit)),
        None => usd(spend),
    }
}

/// A JSON object root, or the fetch fails as the Swift decoder fails.
fn object(body: &str, what: &str) -> Result<serde_json::Map<String, Value>, ProviderError> {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(root)) => Ok(root),
        _ => Err(ProviderError::malformed(format!(
            "{what} must be a JSON object"
        ))),
    }
}

/// One member that must itself be an object.
fn member_object<'a>(
    root: &'a serde_json::Map<String, Value>,
    name: &str,
    what: &str,
) -> Result<&'a serde_json::Map<String, Value>, ProviderError> {
    root.get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::malformed(format!("{what} must carry an {name} object")))
}

/// An optional number, strict: present but not a number fails the field, as the Swift
/// decoder fails the whole response.
fn opt_number(value: Option<&Value>, field: &str) -> Result<Option<f64>, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| ProviderError::malformed(format!("LiteLLM {field} must be numeric"))),
        Some(_) => Err(ProviderError::malformed(format!(
            "LiteLLM {field} must be numeric"
        ))),
    }
}

/// A trimmed non-empty string field, when the body carries one.
fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A date read leniently, as the source reads it: not RFC-3339 is no date, not a
/// failure.
fn lenient_date(value: Option<&Value>) -> Option<Timestamp> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(parse_rfc3339)
}

/// The `YYYY-MM-DD` an instant falls on, for the expiry row.
fn day_of(at: Timestamp) -> String {
    let date = time::OffsetDateTime::from_unix_timestamp(at.as_unix())
        .expect("a plausible timestamp converts")
        .date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The source's `usdString`: dollars, two fraction digits, en-US grouping — the
/// recorded bodies render 1000 as `$1,000.00`.
fn usd(value: f64) -> String {
    format!("${}", grouped(value, 2))
}

/// A number with thousands separators and a fixed number of decimals.
fn grouped(value: f64, decimals: usize) -> String {
    let rendered = format!("{value:.decimals$}");
    let (int_part, rest) = rendered.split_once('.').unwrap_or((rendered.as_str(), ""));
    let bytes = int_part.as_bytes();
    let mut grouped = String::with_capacity(int_part.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    if !rest.is_empty() {
        grouped.push('.');
        grouped.push_str(rest);
    }
    grouped
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Credential;
    use crate::providers::keyed::Options;
    use tidemark_types::{DetailRow, Timestamp, Window};

    /// Recorded by CodexBar, `LiteLLMUsageFetcherTests.swift` — "parses user usage with
    /// personal and team budgets". CodexBar asserts the 70.78% personal window with the
    /// "$212.35 / $300.00" description, the 21.53% team window with "Team ai: $215.32 /
    /// $1,000.00", the email, and the key name.
    const USER_INFO: &str = r#"
        {
          "user_id": "user-123",
          "user_info": {
            "user_id": "user-123",
            "user_alias": "litellm-user@example.com",
            "max_budget": 300.0,
            "spend": 212.3537162499998,
            "user_email": "litellm-user@example.com",
            "budget_reset_at": null,
            "teams": ["team-456"],
            "metadata": {
              "source": "keycloak",
              "preferred_username": "litellm-user@example.com",
              "budget": 300,
              "flags": {
                "keycloak": true
              }
            }
          },
          "keys": [
            {
              "key_name": "sk-...OTHER",
              "user_id": "user-123",
              "team_id": "team-other"
            },
            {
              "key_name": "sk-...IAAw",
              "spend": 212.3537162499998,
              "expires": "2026-09-11T00:12:55.950000+00:00",
              "user_id": "user-123",
              "team_id": "team-456"
            }
          ],
          "teams": [
            {
              "team_alias": "unrelated",
              "team_id": "team-other",
              "max_budget": 5.0,
              "spend": 4.0
            },
            {
              "team_alias": "ai",
              "team_id": "team-456",
              "max_budget": 1000.0,
              "spend": 215.3245658499998,
              "budget_duration": "7d",
              "budget_reset_at": "2026-06-15T00:00:00Z"
            }
          ]
        }
        "#;

    /// Recorded by CodexBar, `LiteLLMUsageFetcherTests.swift` — "preserves personal
    /// spend when no budget is configured". No window at all; the spend survives.
    const USER_NO_BUDGET: &str = r#"
        {
          "user_id": "user-123",
          "user_info": {
            "user_id": "user-123",
            "max_budget": null,
            "spend": 12.5
          }
        }
        "#;

    /// Recorded by CodexBar, `LiteLLMUsageFetcherTests.swift` — "parses key info identity
    /// for user lookup".
    const KEY_INFO_USER: &str = r#"
        {
          "key": "sk-redacted",
          "info": {
            "key_name": "sk-...IAAw",
            "spend": 212.3537162499998,
            "expires": "2026-09-11T00:12:55.950000+00:00",
            "user_id": "user-123",
            "team_id": "team-456",
            "max_budget": null
          }
        }
        "#;

    /// Recorded by CodexBar, `LiteLLMUsageFetcherTests.swift` — "parses team-only key
    /// info without user identity".
    const KEY_INFO_TEAM_ONLY: &str = r#"
        {
          "info": {
            "key_name": "team-service-key",
            "spend": 25.0,
            "team_id": "team-456"
          }
        }
        "#;

    /// Recorded by CodexBar, `LiteLLMUsageFetcherTests.swift` — "fetches team usage for
    /// team-only virtual keys": the `/team/info` body of the team-only ladder. CodexBar
    /// asserts the 25% team window and the "Team budget" period.
    const TEAM_INFO: &str = r#"
                {
                  "team_id": "team-456",
                  "team_info": {
                    "team_id": "team-456",
                    "team_alias": "platform",
                    "max_budget": 100,
                    "spend": 25,
                    "budget_duration": "30d",
                    "budget_reset_at": "2026-07-01T00:00:00Z"
                  }
                }
                "#;

    /// Recorded by CodexBar, `LiteLLMMenuCardModelTests.swift` — "litellm budget rows
    /// show spend detail with reset time". CodexBar asserts the "Personal budget" and
    /// "Team budget" titles, "$403.99 / $900.00" and "Team Platform: $70.00 /
    /// $1,000.00", and both resets.
    const MENU_USER_INFO: &str = r#"
        {
          "user_id": "user-123",
          "user_info": {
            "user_id": "user-123",
            "max_budget": 900.0,
            "spend": 403.99,
            "budget_reset_at": "1970-01-07T00:00:00Z"
          },
          "teams": [
            {
              "team_alias": "Platform",
              "team_id": "team-123",
              "max_budget": 1000.0,
              "spend": 70.0,
              "budget_duration": "30d",
              "budget_reset_at": "1970-01-07T00:00:00Z"
            }
          ]
        }
        "#;

    /// Recorded by CodexBar, `LiteLLMUsageFetcherTests.swift` — "fetch surfaces rejected
    /// virtual key": the body a rejected key's 401 carries.
    const REJECTED_KEY_BODY: &str = r#"{"detail":"Unauthorized"}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn key_info() -> KeyInfo {
        parse_key_info(KEY_INFO_USER).expect("parses")
    }

    fn window<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|w| w.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    fn row<'a>(snapshot: &'a Snapshot, in_section: &str, label: &str) -> &'a DetailRow {
        let found = snapshot
            .details
            .iter()
            .find(|section| section.title == in_section)
            .unwrap_or_else(|| panic!("no section {in_section} in {:?}", snapshot.details));
        found
            .rows
            .iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no row {label} in {in_section}"))
    }

    #[test]
    fn the_recorded_key_info_names_the_user_the_team_and_the_key() {
        let parsed = key_info();
        assert_eq!(parsed.user_id.as_deref(), Some("user-123"));
        assert_eq!(parsed.team_id.as_deref(), Some("team-456"));
        assert_eq!(parsed.key_name.as_deref(), Some("sk-...IAAw"));
        assert!((parsed.spend_usd - 212.3537162499998).abs() < 1e-9);
        assert_eq!(
            parsed.expires_at.map(Timestamp::as_unix),
            Some(1_789_085_575)
        );
    }

    #[test]
    fn the_recorded_team_only_key_info_names_no_user() {
        let parsed = parse_key_info(KEY_INFO_TEAM_ONLY).expect("parses");
        assert_eq!(parsed.user_id, None);
        assert_eq!(parsed.team_id.as_deref(), Some("team-456"));
        assert_eq!(parsed.key_name.as_deref(), Some("team-service-key"));
    }

    #[test]
    fn the_recorded_user_body_draws_the_personal_and_team_windows() {
        let snapshot = user_snapshot(USER_INFO);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["personal", "w604800"]);

        let personal = window(&snapshot, "personal");
        assert_eq!(personal.title, "Personal budget");
        assert!(
            (personal.used_percent - 70.78457208333327).abs() < 1e-6,
            "{}",
            personal.used_percent
        );
        assert_eq!(personal.subtitle.as_deref(), Some("$212.35 / $300.00"));
        assert_eq!(
            personal.resets_at, None,
            "budget_reset_at is null on the wire"
        );
        assert_eq!(
            personal.length, None,
            "a personal budget states no duration"
        );

        let team = window(&snapshot, "w604800");
        assert_eq!(team.title, "Team budget");
        assert!((team.used_percent - 21.53245658499998).abs() < 1e-6);
        assert_eq!(
            team.subtitle.as_deref(),
            Some("Team ai: $215.32 / $1,000.00")
        );
        assert_eq!(
            team.resets_at.map(Timestamp::as_unix),
            Some(1_781_481_600),
            "2026-06-15T00:00:00Z, the reset CodexBar's own test reads"
        );
        assert_eq!(team.length.expect("7d maps to a length").as_secs(), 604_800);
    }

    #[test]
    fn the_recorded_user_body_carries_the_details() {
        let snapshot = user_snapshot(USER_INFO);
        assert_eq!(
            row(&snapshot, "Budgets", "Personal spend").value,
            "$212.35 / $300.00"
        );
        assert_eq!(
            row(&snapshot, "Budgets", "Team spend").value,
            "ai · $215.32 / $1,000.00"
        );
        assert_eq!(
            row(&snapshot, "Budgets", "Account").value,
            "litellm-user@example.com"
        );
        assert_eq!(row(&snapshot, "API key", "Name").value, "sk-...IAAw");
        assert_eq!(row(&snapshot, "API key", "Expires").value, "2026-09-11");
    }

    #[test]
    fn the_menu_card_body_reads_both_resets_and_the_thirty_day_duration() {
        // CodexBar's own harness for this body constructs the key by hand — user-123,
        // team-123, no key name — so this test does too; the body is the recorded one.
        let key = KeyInfo {
            user_id: Some("user-123".to_owned()),
            team_id: Some("team-123".to_owned()),
            key_name: None,
            spend_usd: 403.99,
            expires_at: None,
        };
        let snapshot = snapshot(
            &parse_user_info(MENU_USER_INFO, &key).expect("parses"),
            &key,
            at(1_785_000_000),
        );
        let personal = window(&snapshot, "personal");
        assert!((personal.used_percent - 403.99 / 900.0 * 100.0).abs() < 1e-9);
        assert_eq!(personal.subtitle.as_deref(), Some("$403.99 / $900.00"));
        // CodexBar recorded this body against a 1970 test clock, so its resets are
        // 1970-01-07 — outside the range a Tidemark `Timestamp` can hold, and the
        // lenient date reader treats them as no date. The 2026 resets above are the
        // ones that exercise the reading.
        assert_eq!(personal.resets_at, None);
        let team = window(&snapshot, "w2592000");
        assert!((team.used_percent - 7.0).abs() < 1e-9);
        assert_eq!(
            team.subtitle.as_deref(),
            Some("Team Platform: $70.00 / $1,000.00")
        );
        assert_eq!(team.resets_at, None);
        assert_eq!(
            team.length.expect("30d maps to a length").as_secs(),
            2_592_000
        );
    }

    #[test]
    fn the_recorded_team_body_draws_one_team_window_and_no_personal_one() {
        let key = parse_key_info(KEY_INFO_TEAM_ONLY).expect("parses");
        let snapshot = snapshot(
            &Reading::Team(parse_team_info(TEAM_INFO, &key).expect("parses")),
            &key,
            at(1_785_000_000),
        );
        assert_eq!(snapshot.windows.len(), 1);
        let team = window(&snapshot, "w2592000");
        assert_eq!(team.title, "Team budget");
        assert_eq!(team.used_percent, 25.0);
        assert_eq!(
            team.subtitle.as_deref(),
            Some("Team platform: $25.00 / $100.00")
        );
        assert_eq!(
            team.resets_at.map(Timestamp::as_unix),
            Some(1_782_864_000),
            "2026-07-01T00:00:00Z"
        );
        assert!(
            row(&snapshot, "Budgets", "Personal spend")
                .value
                .contains("No personal budget"),
            "a team-only key has no personal section"
        );
        assert_eq!(
            row(&snapshot, "Budgets", "Team spend").value,
            "platform · $25.00 / $100.00"
        );
        assert_eq!(row(&snapshot, "API key", "Name").value, "team-service-key");
    }

    #[test]
    fn a_personal_spend_without_a_budget_is_a_row_not_a_window() {
        let key = key_info();
        let snapshot = snapshot(
            &parse_user_info(USER_NO_BUDGET, &key).expect("parses"),
            &key,
            at(1_785_000_000),
        );
        assert!(
            snapshot.windows.is_empty(),
            "no budget to divide by, and none is invented"
        );
        assert_eq!(row(&snapshot, "Budgets", "Personal spend").value, "$12.50");
    }

    #[test]
    fn an_identity_that_does_not_match_the_key_info_is_refused() {
        // Constructed error-path bodies, as the procedure allows, over the recorded
        // shapes: the source's own mismatch checks.
        let key = key_info();
        let mismatched =
            USER_INFO.replace("\"user_id\": \"user-123\"", "\"user_id\": \"user-999\"");
        let error = parse_user_info(&mismatched, &key).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");

        let team_key = parse_key_info(KEY_INFO_TEAM_ONLY).expect("parses");
        let mismatched = TEAM_INFO.replace("\"team-456\"", "\"team-999\"");
        let error = parse_team_info(&mismatched, &team_key).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
    }

    #[test]
    fn key_info_that_names_neither_a_user_nor_a_team_is_refused() {
        // The source's `missingUserID`, over the recorded 401 body (which carries no
        // info at all) and a spend-only info object.
        for body in [
            REJECTED_KEY_BODY,
            r#"{"info": {"spend": 1}}"#,
            "{\"partial\":",
        ] {
            let error = parse_key_info(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_number_that_arrives_as_a_string_is_refused() {
        // Constructed error-path bodies, as the procedure allows: the Swift decoder
        // refuses a string where a Double belongs, and so does this port.
        let key = key_info();
        let bad_spend = USER_INFO.replace("\"spend\": 212.3537162499998", "\"spend\": \"many\"");
        let error = parse_user_info(&bad_spend, &key).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");

        let bad_team = USER_INFO.replace("\"max_budget\": 5.0", "\"max_budget\": \"five\"");
        let error = parse_user_info(&bad_team, &key).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
    }

    #[test]
    fn a_user_body_without_its_user_info_is_refused() {
        let error =
            parse_user_info(r#"{"user_id": "user-123"}"#, &key_info()).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error}");
    }

    #[test]
    fn fields_and_entries_this_parser_does_not_read_are_skipped() {
        // The unknown-kind rule: the recorded body already carries a root `keys` array
        // and a `metadata.flags` object this parser never reads, and its `teams` array
        // holds a team the key does not belong to. One more invented root field is
        // skipped the same way.
        let body = USER_INFO.replacen(
            "\"teams\": [",
            "\"future\": {\"kind\": \"daily\"}, \"teams\": [",
            1,
        );
        let key = key_info();
        let snapshot = snapshot(
            &parse_user_info(&body, &key).expect("parses"),
            &key,
            at(1_785_000_000),
        );
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            window(&snapshot, "w604800").subtitle.as_deref(),
            Some("Team ai: $215.32 / $1,000.00"),
            "the unrelated team is present on the wire and read by nobody"
        );
    }

    #[test]
    fn the_management_root_accepts_root_or_v1_base_urls() {
        // CodexBar's own URL test: a root, a versioned, and a nested versioned base.
        assert_eq!(
            LiteLLM::new(
                Credential::new("sk-test"),
                &options("https://litellm.example.com")
            )
            .expect("builds")
            .key_info_url(),
            "https://litellm.example.com/key/info"
        );
        assert_eq!(
            LiteLLM::new(
                Credential::new("sk-test"),
                &options("https://litellm.example.com/v1")
            )
            .expect("builds")
            .key_info_url(),
            "https://litellm.example.com/key/info"
        );
        let nested = LiteLLM::new(
            Credential::new("sk-test"),
            &options("https://gateway.example.com/litellm/v1/"),
        )
        .expect("builds");
        assert_eq!(
            nested.user_info_url("user-123"),
            "https://gateway.example.com/litellm/user/info?user_id=user-123"
        );
        assert_eq!(
            nested.team_info_url("team-456"),
            "https://gateway.example.com/litellm/team/info?team_id=team-456"
        );
    }

    #[test]
    fn the_requests_carry_a_bearer_key() {
        let litellm = LiteLLM::new(
            Credential::new("sk-test"),
            &options("https://litellm.example.com"),
        )
        .expect("builds");
        for request in [
            litellm.key_info_request().expect("builds"),
            litellm.user_info_request("user-123").expect("builds"),
            litellm.team_info_request("team-456").expect("builds"),
        ] {
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer sk-test"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json"
            );
        }
    }

    #[test]
    fn the_base_url_is_required_and_enforced() {
        // The procedure's fourth test: the option resolves, and a missing one names
        // itself rather than producing a malformed URL.
        let error = build(Credential::new("sk-test"), &Options::new())
            .expect_err("the required option is unset");
        assert!(
            matches!(error, ProviderError::Local(ref message)
                if message == "Base URL is not set for this account"),
            "{error}"
        );
        let remote = options("http://litellm.lan:4000");
        let error = build(Credential::new("sk-test"), &remote)
            .expect_err("a key over plain HTTP to a remote host is refused");
        assert!(matches!(error, ProviderError::Local(_)), "{error}");
        assert!(
            build(
                Credential::new("sk-test"),
                &options("http://127.0.0.1:4000")
            )
            .is_ok(),
            "loopback HTTP is how a self-hosted LiteLLM is reached"
        );
    }

    #[test]
    fn the_spec_publishes_one_required_option() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "LiteLLM");
        assert_eq!(SPEC.options.len(), 1);
        assert!(SPEC.options[0].required);
        assert!(SPEC.options[0].choices.is_empty(), "free text");
    }

    #[test]
    fn a_litellm_client_never_prints_its_credential() {
        let litellm = LiteLLM::new(
            Credential::new("sk-super-secret"),
            &options("https://litellm.example.com"),
        )
        .expect("builds");
        let rendered = format!("{litellm:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }

    fn options(base: &str) -> Options {
        Options::from([(BASE_URL.to_owned(), base.to_owned())])
    }

    fn user_snapshot(body: &str) -> Snapshot {
        let key = key_info();
        snapshot(
            &parse_user_info(body, &key).expect("parses"),
            &key,
            at(1_785_000_000),
        )
    }
}
