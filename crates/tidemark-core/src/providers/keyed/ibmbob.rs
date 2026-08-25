//! IBM Bob.
//!
//! Ported from CodexBar's `Providers/IBMBob/IBMBobUsageFetcher.swift`; the recorded
//! bodies in `IBMBobUsageFetcherTests.swift` are the contract. Never seen answering:
//! every number in the tests below is a body CodexBar recorded.
//!
//! # The ladder
//!
//! `GET https://api.us-east.bob.ibm.com/admin/v1/profile` first. Each instance in it
//! names a `region_domain`, and each team budget is read from **that** regional host —
//! `GET https://<api.><region>/admin/v1/teams/<team>/users/<user>` with the instance
//! and team also stated as `x-instance-id` and `x-team-id` headers. CodexBar's own
//! test asserts three requests for a two-team profile, and so does this port's ladder.
//!
//! A regional domain is trusted only when the host it names is `bob.ibm.com` or a
//! subdomain of it — no userinfo, port, path, query or fragment, all of which
//! CodexBar's own argument list pins as bypasses. The check runs **before** the team
//! request is built, so a hostile profile cannot send the key anywhere but IBM's own
//! hosts; a refusal there is malformed, the same reading the source's
//! `untrustedRegion` gives it.
//!
//! # The derived Authorization
//!
//! The key is sent as `Apikey <key>`, or as `Bearer <key>` when it *is* a JWT — three
//! dot-separated parts whose middle is base64url JSON object. That sniff is why this
//! provider is hand-written: the keyed mechanism's auth spellings carry a key whole,
//! and this one derives the header from the key's shape. The sniff answers a bool and
//! nothing else; neither the key nor the token reaches a log, a Debug, or an error.
//!
//! CodexBar also sends `User-Agent: CodexBar`; this port does not, because the shared
//! client owns the product's name.
//!
//! # The reading
//!
//! Bobcoins. The profile's teams each carry a fallback `budget_limit`; the team body's
//! own `budget_limit` wins when present, and a negative one reads as none. Usage is
//! the team body's, floored at zero. The card is one aggregate window — the teams'
//! used summed over the teams' limits, but only when *every* team states a limit, as
//! the source sums it — a fixed balance keyed `balance`, because the wire states no
//! budget duration. Its reset is the soonest of the instances' `refresh_at` values,
//! which arrive as ISO strings or Unix seconds. Without a limit there is no window,
//! only the per-team rows. A profile that reads no team at all is the source's
//! `noSubscription`, refused here as malformed.
//!
//! # What ships untested
//!
//! No recorded body exercises a negative `budget_limit` (constructed only), a team
//! body whose usage is negative (the floor is ported from the source's `max(0, ·)`),
//! or a `refresh_at` that is neither seconds nor an ISO string — the Swift decoder
//! refuses it and so does this port, but with a constructed body. The 401/403 a
//! rejected key earns is mapped by the shared transport, so it is tested by no unit
//! here.

use super::{HandSpec, Options, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "ibmbob";

/// Where the profile lives, and the default regional host.
const PROFILE_HOST: &str = "https://api.us-east.bob.ibm.com";

/// The only domain family a regional host may name.
const TRUSTED_SUFFIX: &str = ".bob.ibm.com";

/// IBM Bob as the settings dialog sees it. One host, no options.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "IBM Bob",
    credential: CredentialKind::Key,
    credential_hint: "bob.ibm.com → Settings → API keys. A Bob API key or an IBM IAM JWT.",
    options: &[],
    build,
};

/// Builds a pollable client from the stored key. Nothing to resolve: the profile host
/// is fixed and the regional hosts come from the response.
fn build(credential: Credential, _options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(IBMBob::new(credential)?))
}

/// One IBM Bob account: the key, and the profile host it unlocks.
pub struct IBMBob {
    client: reqwest::Client,
    credential: Credential,
}

impl IBMBob {
    /// Builds a client.
    pub fn new(credential: Credential) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            credential,
        })
    }

    /// The profile request, built but not sent, so the placement of the key and the
    /// derived Authorization are testable without a server.
    fn profile_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(format!("{PROFILE_HOST}/admin/v1/profile"), None, None)
    }

    /// One team-budget request against a validated regional host, likewise.
    fn team_request(
        &self,
        host: &str,
        instance_id: &str,
        team_id: &str,
        user_id: &str,
    ) -> Result<reqwest::Request, ProviderError> {
        self.get(
            format!("{host}/admin/v1/teams/{team_id}/users/{user_id}"),
            Some(instance_id),
            Some(team_id),
        )
    }

    /// One Bob GET, with the auth the key's own shape dictates.
    fn get(
        &self,
        url: String,
        instance_id: Option<&str>,
        team_id: Option<&str>,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut builder = self
            .client
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                // The derived header: Apikey for a plain key, Bearer for a JWT. The
                // sniff sees the key and returns a bool; the header carries it whole.
                if is_jwt(self.credential.expose()) {
                    format!("Bearer {}", self.credential.expose())
                } else {
                    format!("Apikey {}", self.credential.expose())
                },
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(instance_id) = instance_id {
            builder = builder.header("x-instance-id", instance_id);
        }
        if let Some(team_id) = team_id {
            builder = builder.header("x-team-id", team_id);
        }
        builder
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let body = super::request(PROVIDER_ID, &self.client, self.profile_request()?).await?;
        let profile = parse_profile(&body)?;
        let mut teams = Vec::new();
        for instance in &profile.instances {
            let Some(user_id) = instance
                .user_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let host = regional_host(instance.region_domain.as_deref())?;
            for team in &instance.teams {
                if team.id.trim().is_empty() {
                    continue;
                }
                let request = self.team_request(&host, &instance.instance_id, &team.id, user_id)?;
                let body = super::request(PROVIDER_ID, &self.client, request).await?;
                let budget = parse_team_budget(&body)?;
                teams.push(TeamUsage::of(instance, team, &budget));
            }
        }
        if teams.is_empty() {
            return Err(ProviderError::malformed(
                "the IBM Bob profile carried no teams for this key",
            ));
        }
        Ok(snapshot(&teams, now))
    }
}

impl fmt::Debug for IBMBob {
    /// Written by hand: a derived impl would print the credential — key or token — the
    /// first time anything traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IBMBob")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for IBMBob {
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

/// The profile body's shape, as the fetch walks it.
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    instances: Vec<Instance>,
}

/// One instance of the account.
#[derive(Debug, Clone, PartialEq)]
struct Instance {
    instance_id: String,
    instance_name: Option<String>,
    legacy_name: Option<String>,
    user_id: Option<String>,
    plan_name: Option<String>,
    refresh_at: Option<Timestamp>,
    region_domain: Option<String>,
    teams: Vec<ProfileTeam>,
}

impl Instance {
    /// The instance's display name: `instance_name`, else the legacy `name`.
    fn display_name(&self) -> Option<&str> {
        self.instance_name
            .as_deref()
            .or(self.legacy_name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }
}

/// One team as the profile names it.
#[derive(Debug, Clone, PartialEq)]
struct ProfileTeam {
    id: String,
    name: Option<String>,
    budget_limit: Option<f64>,
}

/// One team-budget body.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TeamBudget {
    usage: f64,
    budget_limit: Option<f64>,
}

/// One team's reading, the ladder's unit of work.
#[derive(Debug, Clone, PartialEq)]
struct TeamUsage {
    instance_name: String,
    team_name: String,
    plan_name: Option<String>,
    used_bobcoins: f64,
    limit_bobcoins: Option<f64>,
    resets_at: Option<Timestamp>,
}

impl TeamUsage {
    /// Combines one profile instance, one profile team and the team's budget body into
    /// the reading, with the source's own precedence: the body's limit wins, a
    /// negative limit is none, usage is floored, and the names fall back to the ids.
    fn of(instance: &Instance, team: &ProfileTeam, budget: &TeamBudget) -> Self {
        Self {
            instance_name: instance
                .display_name()
                .unwrap_or(&instance.instance_id)
                .to_owned(),
            team_name: team
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&team.id)
                .to_owned(),
            plan_name: instance
                .plan_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            used_bobcoins: budget.usage.max(0.0),
            limit_bobcoins: budget
                .budget_limit
                .or(team.budget_limit)
                .filter(|limit| *limit >= 0.0),
            resets_at: instance.refresh_at,
        }
    }
}

/// Reads the profile body. Pure, and strict where the Swift decoder is: `instances`
/// must be an array, every instance must carry its `instance_id` and `teams`, and a
/// `refresh_at` that is neither seconds nor an ISO string fails the whole body.
fn parse_profile(body: &str) -> Result<Profile, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an IBM Bob profile: {e}")))?;
    let instances = root
        .get("instances")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::malformed("the IBM Bob profile must carry instances"))?;
    let mut parsed = Vec::with_capacity(instances.len());
    for instance in instances {
        let Value::Object(instance) = instance else {
            return Err(ProviderError::malformed(
                "an IBM Bob instance must be an object",
            ));
        };
        parsed.push(Instance {
            instance_id: non_empty(instance.get("instance_id")).ok_or_else(|| {
                ProviderError::malformed("an IBM Bob instance must carry instance_id")
            })?,
            instance_name: non_empty(instance.get("instance_name")),
            legacy_name: non_empty(instance.get("name")),
            user_id: non_empty(instance.get("user_id")),
            plan_name: non_empty(instance.get("plan_name")),
            refresh_at: refresh_at(instance.get("refresh_at"))?,
            region_domain: non_empty(instance.get("region_domain")),
            teams: teams_of(instance)?,
        });
    }
    Ok(Profile { instances: parsed })
}

/// The profile's `teams` array, decoded as strictly as the Swift decoder decodes it.
fn teams_of(instance: &serde_json::Map<String, Value>) -> Result<Vec<ProfileTeam>, ProviderError> {
    let teams = instance
        .get("teams")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::malformed("an IBM Bob instance must carry teams"))?;
    teams
        .iter()
        .map(|team| {
            let Value::Object(team) = team else {
                return Err(ProviderError::malformed(
                    "an IBM Bob team must be an object",
                ));
            };
            Ok(ProfileTeam {
                id: team
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                name: non_empty(team.get("name")),
                budget_limit: number(team.get("budget_limit"), "budget_limit")?,
            })
        })
        .collect()
}

/// Reads one team-budget body. Pure; `usage` must be present and numeric.
fn parse_team_budget(body: &str) -> Result<TeamBudget, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an IBM Bob team budget: {e}")))?;
    let Value::Object(root) = root else {
        return Err(ProviderError::malformed(
            "an IBM Bob team budget must be a JSON object",
        ));
    };
    Ok(TeamBudget {
        usage: number(root.get("usage"), "usage")?
            .ok_or_else(|| ProviderError::malformed("an IBM Bob team budget must carry usage"))?,
        budget_limit: number(root.get("budget_limit"), "budget_limit")?,
    })
}

/// The regional host a `region_domain` names, once it has passed the trust gate.
/// `None` — or a blank domain — is the profile's own host. Pure, so every recorded
/// bypass is reachable from a test.
fn regional_host(domain: Option<&str>) -> Result<String, ProviderError> {
    let Some(domain) = domain.map(str::trim).filter(|domain| !domain.is_empty()) else {
        return Ok(PROFILE_HOST.to_owned());
    };
    // The host is lowercased before it is trusted, as the source lowercases it for
    // every comparison; the `api.` prefix is added only when absent.
    let host = if domain.to_lowercase().starts_with("api.") {
        domain.to_lowercase()
    } else {
        format!("api.{}", domain.to_lowercase())
    };
    let trusted = !host.is_empty()
        && !host.contains(['@', ':', '/', '?', '#'])
        && (host == TRUSTED_SUFFIX[1..] || host.ends_with(TRUSTED_SUFFIX));
    if !trusted {
        return Err(ProviderError::malformed(format!(
            "an untrusted regional host appeared in the profile: {host}"
        )));
    }
    Ok(format!("https://{host}"))
}

/// Whether a token is a JWT: three dot-separated parts whose middle is base64url for a
/// JSON object. Answers a bool and nothing else — the token itself never leaves this
/// function, which is what keeps the derived Authorization from leaking through a log.
fn is_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let payload = parts[1].replace('-', "+").replace('_', "/");
    let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
    BASE64
        .decode(padded.as_bytes())
        .ok()
        .and_then(|decoded| serde_json::from_slice::<Value>(&decoded).ok())
        .is_some_and(|value| value.is_object())
}

/// A `refresh_at` as the source reads it: Unix seconds, or an ISO-8601 string; absent
/// or null is no date; anything else fails the body.
fn refresh_at(value: Option<&Value>) -> Result<Option<Timestamp>, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let seconds = number.as_f64().filter(|seconds| seconds.is_finite());
            Ok(seconds
                .filter(|seconds| *seconds > 0.0)
                .and_then(|seconds| Timestamp::from_unix(seconds as i64).ok()))
        }
        Some(Value::String(raw)) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            Ok(parse_rfc3339(raw))
        }
        Some(_) => Err(ProviderError::malformed(
            "IBM Bob refresh_at must be Unix seconds or an ISO-8601 string",
        )),
    }
}

/// An optional number, strict: present but not a number fails the field.
fn number(value: Option<&Value>, field: &str) -> Result<Option<f64>, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| ProviderError::malformed(format!("IBM Bob {field} must be numeric"))),
        Some(_) => Err(ProviderError::malformed(format!(
            "IBM Bob {field} must be numeric"
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

/// Assembles the snapshot. Pure, so the recorded profile renders are reachable from a
/// test.
///
/// One aggregate window — the teams' used summed over the teams' limits, and only when
/// every team states a limit, as the source sums it. A balance has no length to key
/// on: the wire states no budget duration. The reset is the soonest of the instances'
/// refresh times.
fn snapshot(teams: &[TeamUsage], now: Timestamp) -> Snapshot {
    let used: f64 = teams.iter().map(|team| team.used_bobcoins).sum();
    let limits: Vec<f64> = teams
        .iter()
        .filter_map(|team| team.limit_bobcoins)
        .collect();
    let limit = (limits.len() == teams.len() && !limits.is_empty()).then(|| limits.iter().sum());
    let resets_at = teams
        .iter()
        .filter_map(|team| team.resets_at)
        .reduce(|soonest, at| soonest.min(at));
    let mut windows = Vec::new();
    if let Some(limit) = limit.filter(|limit| *limit > 0.0) {
        windows.push(Window {
            key: WindowKey::named("balance"),
            title: "Bobcoins".to_owned(),
            subtitle: Some(format!("{} / {} Bobcoins", bobcoins(used), bobcoins(limit))),
            used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
            resets_at,
            length: None,
        });
    }
    // No window when a team states no limit: nothing to divide by, and a bar at 0%
    // (CodexBar's rendering of this case) would read as "nothing spent". The rows
    // below carry the reading instead.
    let rows = teams
        .iter()
        .map(|team| {
            let label = if team.team_name == team.instance_name {
                team.team_name.clone()
            } else {
                format!("{} · {}", team.instance_name, team.team_name)
            };
            let value = match team.limit_bobcoins.filter(|limit| *limit >= 0.0) {
                Some(limit) => format!(
                    "{} / {} Bobcoins",
                    bobcoins(team.used_bobcoins),
                    bobcoins(limit)
                ),
                None => format!("{} Bobcoins used", bobcoins(team.used_bobcoins)),
            };
            let value = match &team.plan_name {
                Some(plan) => format!("{value} · {plan}"),
                None => value,
            };
            DetailRow { label, value }
        })
        .collect();
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows,
        details: vec![DetailSection {
            title: "Bobcoin usage".to_owned(),
            rows,
        }],
    }
}

/// The source's `bobcoins`: whole values plain, fractional ones to two digits.
fn bobcoins(value: f64) -> String {
    if value.round() == value {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Credential;
    use crate::providers::keyed::Options;
    use tidemark_types::{DetailRow, Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `IBMBobUsageFetcherTests.swift` — "fetches profile and
    /// regional team budgets". CodexBar asserts the 35 Bobcoin total against the 200
    /// limit (17.5%), three requests, the `api.eu-de.bob.ibm.com` regional host for the
    /// second instance, `Apikey` authorization, and the `x-instance-id`/`x-team-id`
    /// headers.
    const PROFILE: &str = r#"
        {
          "instances": [
            {
              "instance_id": "instance-one",
              "name": "Personal",
              "user_id": "user-one",
              "plan_name": "Pro+",
              "refresh_at": "2026-09-01T00:00:00Z",
              "region_domain": "us-east.bob.ibm.com",
              "teams": [{"id": "team-one", "name": "Solo", "budget_limit": 40}]
            },
            {
              "instance_id": "instance-two",
              "name": "Work",
              "user_id": "user-two",
              "plan_name": "Enterprise",
              "refresh_at": "2026-09-05T00:00:00.000Z",
              "region_domain": "api.eu-de.bob.ibm.com",
              "teams": [{"id": "team-two", "name": "Platform", "budget_limit": 160}]
            }
          ]
        }
        "#;

    /// The team-budget bodies the same test serves, verbatim: neither carries its own
    /// `budget_limit`, so each falls back to the profile's.
    const TEAM_ONE_BUDGET: &str = r#"{"usage":10}"#;
    const TEAM_TWO_BUDGET: &str = r#"{"usage":25}"#;

    /// Recorded by CodexBar, `IBMBobUsageFetcherTests.swift` — "uses bearer
    /// authorization for JWT credentials".
    const SINGLE_TEAM_PROFILE: &str = r#"
        {
          "instances": [{
            "instance_id": "instance-one",
            "user_id": "user-one",
            "teams": [{"id": "team-one", "budget_limit": 40}]
          }]
        }
        "#;

    /// Recorded by CodexBar, `IBMBobUsageFetcherTests.swift` — "decodes live profile
    /// names unix resets and team budget". CodexBar asserts the "Personal" name read
    /// from `instance_name`, the 12.5/80 budget, and the 1788220800 reset.
    const LIVE_PROFILE: &str = r#"
        {
          "instances": [{
            "instance_id": "instance-one",
            "instance_name": "Personal",
            "user_id": "user-one",
            "plan_name": "Pro+",
            "refresh_at": 1788220800,
            "region_domain": "us-east.bob.ibm.com",
            "teams": [{"id": "team-one", "name": "Solo", "budget_limit": 40, "usage": 10}]
          }]
        }
        "#;

    /// The team-budget body the live-profile test serves, verbatim.
    const LIVE_TEAM_BUDGET: &str = r#"{"usage":12.5,"budget_limit":80}"#;

    /// The JWT CodexBar's own test uses, verbatim: its payload is `{"sub":"user"}`.
    const RECORDED_JWT: &str = "header.eyJzdWIiOiJ1c2VyIn0.signature";

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
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
    fn the_recorded_profile_and_team_bodies_sum_into_one_window() {
        let profile = parse_profile(PROFILE).expect("parses");
        assert_eq!(profile.instances.len(), 2);
        let one = parse_team_budget(TEAM_ONE_BUDGET).expect("parses");
        let two = parse_team_budget(TEAM_TWO_BUDGET).expect("parses");

        let teams = ladder(&profile, [one, two]);
        let snapshot = snapshot(&teams, at(1_785_000_000));
        assert_eq!(snapshot.windows.len(), 1);
        let budget = window(&snapshot, "balance");
        assert_eq!(budget.title, "Bobcoins");
        assert_eq!(budget.used_percent, 17.5, "35 of 200, as CodexBar asserts");
        assert_eq!(budget.subtitle.as_deref(), Some("35 / 200 Bobcoins"));
        assert_eq!(
            budget.resets_at.map(Timestamp::as_unix),
            Some(1_788_220_800),
            "the sooner of 2026-09-01 and 2026-09-05"
        );
        assert_eq!(budget.length, None, "the wire states no budget duration");
    }

    #[test]
    fn the_recorded_profile_renders_one_row_per_team() {
        let profile = parse_profile(PROFILE).expect("parses");
        let teams = ladder(
            &profile,
            [
                parse_team_budget(TEAM_ONE_BUDGET).expect("parses"),
                parse_team_budget(TEAM_TWO_BUDGET).expect("parses"),
            ],
        );
        let snapshot = snapshot(&teams, at(1_785_000_000));
        assert_eq!(
            row(&snapshot, "Bobcoin usage", "Personal · Solo").value,
            "10 / 40 Bobcoins · Pro+",
            "the team body names no budget_limit, so the profile's 40 stands"
        );
        assert_eq!(
            row(&snapshot, "Bobcoin usage", "Work · Platform").value,
            "25 / 160 Bobcoins · Enterprise"
        );
    }

    #[test]
    fn the_live_profile_reads_names_unix_resets_and_its_own_budget() {
        let profile = parse_profile(LIVE_PROFILE).expect("parses");
        assert_eq!(
            profile.instances[0].display_name(),
            Some("Personal"),
            "instance_name is preferred over the id"
        );
        let teams = ladder(
            &profile,
            [parse_team_budget(LIVE_TEAM_BUDGET).expect("parses")],
        );
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].instance_name, "Personal");
        assert_eq!(teams[0].used_bobcoins, 12.5);
        assert_eq!(teams[0].limit_bobcoins, Some(80.0));
        assert_eq!(
            teams[0].resets_at.map(Timestamp::as_unix),
            Some(1_788_220_800)
        );

        let snapshot = snapshot(&teams, at(1_785_000_000));
        let budget = window(&snapshot, "balance");
        assert_eq!(budget.used_percent, 15.625, "12.5 of 80");
        assert_eq!(budget.subtitle.as_deref(), Some("12.50 / 80 Bobcoins"));
        assert_eq!(
            row(&snapshot, "Bobcoin usage", "Personal · Solo").value,
            "12.50 / 80 Bobcoins · Pro+"
        );
    }

    #[test]
    fn the_requests_address_the_profile_and_the_regional_hosts() {
        let bob = IBMBob::new(Credential::new("fixture-key")).expect("builds");
        let profile_request = bob.profile_request().expect("builds");
        assert_eq!(
            profile_request.url().as_str(),
            "https://api.us-east.bob.ibm.com/admin/v1/profile"
        );
        assert_eq!(profile_request.method(), reqwest::Method::GET);
        assert_eq!(
            profile_request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Apikey fixture-key",
            "a plain key spells Apikey, as CodexBar asserts for this fixture key"
        );
        assert!(
            profile_request
                .headers()
                .get(reqwest::header::USER_AGENT)
                .is_none(),
            "the shared client owns the User-Agent"
        );

        let us = bob
            .team_request(
                "https://api.us-east.bob.ibm.com",
                "instance-one",
                "team-one",
                "user-one",
            )
            .expect("builds");
        assert_eq!(
            us.url().as_str(),
            "https://api.us-east.bob.ibm.com/admin/v1/teams/team-one/users/user-one"
        );
        assert_eq!(
            us.headers().get("x-instance-id").expect("present"),
            "instance-one"
        );
        assert_eq!(us.headers().get("x-team-id").expect("present"), "team-one");
        let eu = bob
            .team_request(
                "https://api.eu-de.bob.ibm.com",
                "instance-two",
                "team-two",
                "user-two",
            )
            .expect("builds");
        assert_eq!(
            eu.url().host_str(),
            Some("api.eu-de.bob.ibm.com"),
            "the profile's region_domain is already api-prefixed and stays so"
        );
        for request in [profile_request, us, eu] {
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Apikey fixture-key"
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
    fn a_jwt_key_is_sent_as_a_bearer_token() {
        let bob = IBMBob::new(Credential::new(RECORDED_JWT)).expect("builds");
        let request = bob.profile_request().expect("builds");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            format!("Bearer {RECORDED_JWT}").as_str(),
            "the recorded JWT token, as CodexBar asserts"
        );
    }

    #[test]
    fn the_jwt_sniff_reads_only_the_token_shape() {
        assert!(is_jwt(RECORDED_JWT));
        assert!(!is_jwt("fixture-key"), "no dots at all");
        assert!(!is_jwt("a.b"), "two parts");
        assert!(!is_jwt("x.y.z"), "a middle that is not base64 JSON");
        assert!(!is_jwt("one.two.three.four"), "four parts");
        assert!(!is_jwt("a..c"), "an empty payload is no JSON object");
    }

    #[test]
    fn the_sniff_never_renders_the_key_or_the_token() {
        // The derived Debug prints neither: the sniff returns a bool and the client
        // holds the credential in the redacting wrapper.
        let bob = IBMBob::new(Credential::new(RECORDED_JWT)).expect("builds");
        let rendered = format!("{bob:?}");
        assert!(!rendered.contains(RECORDED_JWT), "{rendered}");
        assert!(!rendered.contains("fixture-key"), "{rendered}");
        assert!(
            is_jwt(RECORDED_JWT),
            "the sniff's own result is the only thing it says"
        );
    }

    #[test]
    fn the_regional_host_is_prefixed_defaulted_and_gated() {
        // The recorded defaults and spellings: an api-prefixed domain stays, a bare one
        // gains the prefix, and a missing domain falls back to the profile host.
        assert_eq!(
            regional_host(Some("api.eu-de.bob.ibm.com")).expect("trusted"),
            "https://api.eu-de.bob.ibm.com"
        );
        assert_eq!(
            regional_host(Some("us-east.bob.ibm.com")).expect("trusted"),
            "https://api.us-east.bob.ibm.com"
        );
        assert_eq!(
            regional_host(None).expect("trusted"),
            "https://api.us-east.bob.ibm.com"
        );
        assert_eq!(
            regional_host(Some("  ")).expect("trusted"),
            "https://api.us-east.bob.ibm.com",
            "a blank domain is a missing domain"
        );
    }

    #[test]
    fn the_regional_host_refuses_every_recorded_bypass() {
        // CodexBar's own argument list, verbatim: userinfo, ports, paths, queries and
        // fragments are all refused before any credential is spent on the host.
        for domain in [
            "evil.example",
            "evil.example/x.bob.ibm.com",
            "bob.ibm.com.evil.example",
            "x@evil.example",
            "evil.example/path/.bob.ibm.com",
            "evil.example?next=.bob.ibm.com",
            "evil.example#.bob.ibm.com",
            "evil.example@us-east.bob.ibm.com",
            "us-east.bob.ibm.com:443",
        ] {
            let error = regional_host(Some(domain)).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(ref message)
                    if message.contains("untrusted regional host")),
                "{domain}: {error}"
            );
        }
    }

    #[test]
    fn bodies_that_cannot_be_read_are_refused() {
        // The procedure's canonical malformed bodies plus the shapes the Swift decoder
        // refuses: a profile without its instances array, a team budget without its
        // usage, and strings where the numbers belong.
        for body in [
            "not-json",
            "{\"partial\":",
            r#"{"instances": {}}"#,
            r#"{"teams": []}"#,
        ] {
            let error = parse_profile(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error}"
            );
        }
        // An empty profile decodes; refusing it as the source's `noSubscription` is the
        // fetch ladder's job, which no unit here can reach without a server.
        let empty = parse_profile(r#"{"instances": []}"#).expect("decodes");
        assert!(empty.instances.is_empty());

        for body in [
            "{\"partial\":",
            r#"{"budget_limit":80}"#,
            r#"{"usage":"many"}"#,
        ] {
            let error = parse_team_budget(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error}"
            );
        }
    }

    #[test]
    fn an_instance_without_a_user_or_a_team_without_an_id_is_skipped() {
        // The recorded single-team profile beside a constructed instance that carries
        // no user_id: the ladder reads one team, not zero and not an error.
        let spliced = SINGLE_TEAM_PROFILE.replacen(
            "\"instances\": [{",
            "\"instances\": [{\"instance_id\": \"none\", \"teams\": [{\"id\": \"t\"}]}, {",
            1,
        );
        let profile = parse_profile(&spliced).expect("parses");
        assert_eq!(profile.instances.len(), 2);
        let teams = ladder(
            &profile,
            [parse_team_budget(r#"{"usage":4}"#).expect("parses")],
        );
        assert_eq!(teams.len(), 1, "only the instance with a user_id is read");
    }

    #[test]
    fn fields_this_parser_does_not_read_are_skipped() {
        // The unknown-kind rule: the recorded bodies already carry a `usage` field on
        // the profile's team entries (read only as the fallback limit's neighbour) and
        // one more invented field rides along.
        let spliced = LIVE_PROFILE.replacen(
            "\"instances\": [{",
            "\"future\": {\"kind\": \"daily\"}, \"instances\": [{",
            1,
        );
        let profile = parse_profile(&spliced).expect("parses");
        assert_eq!(profile.instances.len(), 1);
        let budget =
            parse_team_budget(r#"{"usage":12.5,"budget_limit":80,"future":true}"#).expect("parses");
        assert_eq!(budget.usage, 12.5);
    }

    #[test]
    fn the_spec_offers_no_options_and_builds_a_client() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "IBM Bob");
        assert!(SPEC.options.is_empty(), "one host, nothing to choose");
        assert!(build(Credential::new("fixture-key"), &Options::new()).is_ok());
    }

    /// The ladder's pure half: pairs the profile's instances with their team budgets
    /// in reading order, the way the fetch walks them.
    fn ladder(profile: &Profile, budgets: impl IntoIterator<Item = TeamBudget>) -> Vec<TeamUsage> {
        let mut budgets = budgets.into_iter();
        let mut teams = Vec::new();
        for instance in &profile.instances {
            if instance.user_id.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            for team in &instance.teams {
                if team.id.is_empty() {
                    continue;
                }
                let budget = budgets.next().expect("a budget per team");
                teams.push(TeamUsage::of(instance, team, &budget));
            }
        }
        teams
    }
}
