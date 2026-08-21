//! Warp.
//!
//! Ported from CodexBar's Swift `Providers/Warp/WarpUsageFetcher.swift`; there is no
//! separate parser file, and the fetcher's tests are the contract. Never seen answering:
//! every number in the tests is a body CodexBar recorded.
//!
//! # The request
//!
//! One GraphQL POST to `app.warp.dev/graphql/v2?op=GetRequestLimitInfo` with the
//! `GetRequestLimitInfo` query copied verbatim from the source and a bearer key — the
//! shape [`Method::Post`] exists for. CodexBar sends the running macOS version in the
//! body's `osContext` and the `x-warp-os-version` header; no recorded body pins a
//! version, so this port states a constant one and says so here. `User-Agent:
//! Warp/1.0` is required: the source documents the edge limiter answering 429
//! "Rate exceeded." to any other agent (that rejection lives in the status, so the
//! shared transport maps it, and `parse` never sees it). `x-warp-os-category` came
//! from CodexBar too; its necessity is unverified.
//!
//! # The reading
//!
//! `requestLimitInfo` is a fixed balance — `requestsUsedSinceLastRefresh` of
//! `requestLimit`, reset by `nextRefreshTime` — drawn as one lengthless window keyed
//! `credits` with `used/limit credits` under the bar. Counts arrive as numbers or as
//! strings and read either way; `isUnlimited` may be null (then false). An unlimited
//! account draws a zero bar with "Unlimited" and no reset — that branch has no
//! recorded body, so it ships untested.
//!
//! `bonusGrants` on the user and `grants` inside every workspace's `bonusGrantsInfo`
//! aggregate into one add-on window: total granted, total remaining, and the earliest
//! expiry that still has credits, whose remaining rides under the bar ("10 credits
//! expires on 2026-03-01"). No grants, no window.
//!
//! # The rejections that arrive as a 200
//!
//! GraphQL cannot answer 401, and Warp says it two ways in a successful body: an
//! `errors` array carrying "Unauthorized", and a user object whose `__typename` is
//! `AuthError`. Both map to `Credential`, so the interface asks for a new key; any
//! other `errors` message is `Malformed` carrying it. A body whose user object is
//! present but carries no `requestLimitInfo` at all is `Malformed`, as in the source.

use super::{Auth, Method, Spec};
use crate::providers::{ProviderError, parse_rfc3339};
use serde::Deserialize;
use serde_json::Value;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp, Window, WindowKey};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "warp";

/// The endpoint, operation query string included.
const USAGE_URL: &str = "https://app.warp.dev/graphql/v2?op=GetRequestLimitInfo";

/// The request body: the `GetRequestLimitInfo` query exactly as the Swift source
/// spells it (newlines escaped as the wire carries them), with the request context the
/// source builds. The OS version is pinned — CodexBar sends the running one, and no
/// recorded body pins a value.
const USAGE_BODY: &str = concat!(
    "{\"query\":\"query GetRequestLimitInfo($requestContext: RequestContext!) {\\n",
    "  user(requestContext: $requestContext) {\\n",
    "    __typename\\n",
    "    ... on UserOutput {\\n",
    "      user {\\n",
    "        requestLimitInfo {\\n",
    "          isUnlimited\\n",
    "          nextRefreshTime\\n",
    "          requestLimit\\n",
    "          requestsUsedSinceLastRefresh\\n",
    "        }\\n",
    "        bonusGrants {\\n",
    "          requestCreditsGranted\\n",
    "          requestCreditsRemaining\\n",
    "          expiration\\n",
    "        }\\n",
    "        workspaces {\\n",
    "          bonusGrantsInfo {\\n",
    "            grants {\\n",
    "              requestCreditsGranted\\n",
    "              requestCreditsRemaining\\n",
    "              expiration\\n",
    "            }\\n",
    "          }\\n",
    "        }\\n",
    "      }\\n",
    "    }\\n",
    "  }\\n",
    "}\",\"variables\":{\"requestContext\":{\"clientContext\":{},",
    "\"osContext\":{\"category\":\"macOS\",\"name\":\"macOS\",\"version\":\"14.0\"}}},",
    "\"operationName\":\"GetRequestLimitInfo\"}"
);

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Option<DataRoot>,
}

#[derive(Debug, Deserialize)]
struct DataRoot {
    #[serde(default)]
    user: Option<UserShell>,
}

#[derive(Debug, Deserialize)]
struct UserShell {
    #[serde(rename = "__typename", default)]
    typename: Option<String>,
    #[serde(default)]
    user: Option<InnerUser>,
}

#[derive(Debug, Deserialize)]
struct InnerUser {
    #[serde(default, rename = "requestLimitInfo")]
    request_limit_info: Option<RequestLimitInfo>,
    #[serde(default, rename = "bonusGrants")]
    bonus_grants: Option<Vec<Grant>>,
    #[serde(default)]
    workspaces: Option<Vec<Workspace>>,
}

#[derive(Debug, Deserialize)]
struct RequestLimitInfo {
    #[serde(default, rename = "isUnlimited")]
    is_unlimited: Option<Value>,
    #[serde(default, rename = "requestLimit")]
    request_limit: Option<Value>,
    #[serde(default, rename = "requestsUsedSinceLastRefresh")]
    requests_used: Option<Value>,
    #[serde(default, rename = "nextRefreshTime")]
    next_refresh_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Grant {
    #[serde(default, rename = "requestCreditsGranted")]
    granted: Option<Value>,
    #[serde(default, rename = "requestCreditsRemaining")]
    remaining: Option<Value>,
    #[serde(default)]
    expiration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Workspace {
    #[serde(default, rename = "bonusGrantsInfo")]
    bonus_grants_info: Option<BonusGrantsInfo>,
}

#[derive(Debug, Deserialize)]
struct BonusGrantsInfo {
    #[serde(default)]
    grants: Option<Vec<Grant>>,
}

/// The aggregated add-on credits, as the source aggregates them.
#[derive(Debug, Default)]
struct Bonus {
    remaining: i64,
    total: i64,
    /// The earliest expiry that still has credits.
    next_expiration: Option<Timestamp>,
    /// What remains against that expiry, summed over grants sharing it.
    next_expiration_remaining: i64,
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    let Some(json) = root.as_object() else {
        return Err(ProviderError::malformed("root JSON is not an object"));
    };

    if let Some(errors) = json.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let messages: Vec<String> = errors.iter().filter_map(error_message).collect();
        // The rejection that arrives as a 200: the interface asks for a new key.
        if messages
            .iter()
            .any(|message| message.to_lowercase().contains("unauthorized"))
        {
            return Err(ProviderError::Credential { status: 401 });
        }
        let summary = if messages.is_empty() {
            "GraphQL request failed.".to_owned()
        } else {
            messages
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        };
        return Err(ProviderError::malformed(summary));
    }

    let envelope: Envelope = serde_json::from_value(root.clone())
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;
    let user = envelope
        .data
        .and_then(|data| data.user)
        .ok_or_else(|| ProviderError::malformed("missing data.user in response"))?;

    let inner = user.user.ok_or_else(|| match user.typename.as_deref() {
        // The second way a rejected key arrives as a 200.
        Some(name) if !name.trim().is_empty() && name != "UserOutput" => {
            ProviderError::Credential { status: 401 }
        }
        _ => ProviderError::malformed("unable to extract requestLimitInfo from response"),
    })?;
    let limit_info = inner.request_limit_info.as_ref().ok_or_else(|| {
        match user.typename.as_deref() {
            // The second way a rejected key arrives as a 200.
            Some(name) if !name.trim().is_empty() && name != "UserOutput" => {
                ProviderError::Credential { status: 401 }
            }
            _ => ProviderError::malformed("unable to extract requestLimitInfo from response"),
        }
    })?;

    let unlimited = bool_value(limit_info.is_unlimited.as_ref());
    let limit = int_value(limit_info.request_limit.as_ref());
    let used = int_value(limit_info.requests_used.as_ref());
    let resets_at = limit_info
        .next_refresh_time
        .as_deref()
        .and_then(parse_rfc3339);

    let mut windows = vec![Window {
        // A balance has no length to key on: it drains rather than rolling over.
        key: WindowKey::named("credits"),
        title: "Credits".to_owned(),
        subtitle: Some(if unlimited {
            "Unlimited".to_owned()
        } else {
            format!("{used}/{limit} credits")
        }),
        used_percent: if unlimited || limit <= 0 {
            0.0
        } else {
            (used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0)
        },
        // An unlimited account has no ceiling to reset against.
        resets_at: if unlimited { None } else { resets_at },
        length: None,
    }];

    let bonus = aggregate_bonus(&inner);
    if bonus.total > 0 || bonus.remaining > 0 {
        let used_percent = if bonus.total > 0 {
            ((bonus.total - bonus.remaining) as f64 / bonus.total as f64 * 100.0).clamp(0.0, 100.0)
        } else if bonus.remaining > 0 {
            0.0
        } else {
            100.0
        };
        let subtitle = bonus
            .next_expiration
            .filter(|_| bonus.next_expiration_remaining > 0)
            .map(|at| {
                format!(
                    "{} credits expires on {}",
                    bonus.next_expiration_remaining,
                    day_of(at)
                )
            });
        windows.push(Window {
            key: WindowKey::named("addon"),
            title: "Add-on credits".to_owned(),
            subtitle,
            used_percent,
            resets_at: None,
            length: None,
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: Vec::new(),
    })
}

/// Sums the user-level and workspace-level grants, and finds the earliest expiry that
/// still has credits — summing every grant that shares that instant, as the source does.
fn aggregate_bonus(user: &InnerUser) -> Bonus {
    let mut grants: Vec<(i64, i64, Option<Timestamp>)> = Vec::new();
    for grant in user.bonus_grants.iter().flatten() {
        grants.push((
            int_value(grant.granted.as_ref()),
            int_value(grant.remaining.as_ref()),
            grant.expiration.as_deref().and_then(parse_rfc3339),
        ));
    }
    for workspace in user.workspaces.iter().flatten() {
        for grant in workspace
            .bonus_grants_info
            .iter()
            .flat_map(|info| info.grants.iter().flatten())
        {
            grants.push((
                int_value(grant.granted.as_ref()),
                int_value(grant.remaining.as_ref()),
                grant.expiration.as_deref().and_then(parse_rfc3339),
            ));
        }
    }

    let mut bonus = Bonus {
        remaining: grants.iter().map(|(_, remaining, _)| remaining).sum(),
        total: grants.iter().map(|(granted, _, _)| granted).sum(),
        ..Bonus::default()
    };
    let earliest = grants
        .iter()
        .filter(|(_, remaining, expiration)| *remaining > 0 && expiration.is_some())
        .min_by_key(|(_, _, expiration)| expiration.expect("just checked present"));
    if let Some((_, _, Some(at))) = earliest {
        bonus.next_expiration = Some(*at);
        bonus.next_expiration_remaining = grants
            .iter()
            .filter(|(_, remaining, expiration)| {
                *remaining > 0 && expiration.is_some_and(|other| other == *at)
            })
            .map(|(_, remaining, _)| remaining)
            .sum();
    }
    bonus
}

/// One GraphQL error's message, trimmed, whatever shape the entry takes.
fn error_message(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(raw) => raw,
        Value::Object(map) => map.get("message")?.as_str()?,
        _ => return None,
    };
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// A count however the wire spells it: a number, or a string holding one. Anything
/// else reads as zero, as the source's own `intValue` reads it.
fn int_value(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().map_or(0.0, f64::round) as i64,
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// A flag however the wire spells it: a boolean, a number, or one of the source's own
/// words. Anything else reads as false.
fn bool_value(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => false,
        },
        _ => false,
    }
}

/// The `YYYY-MM-DD` a whole-second timestamp falls on, for the expiry line.
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

/// Warp as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Warp",
    endpoint: |_| USAGE_URL.to_owned(),
    method: Method::Post {
        body: USAGE_BODY,
        content_type: "application/json",
    },
    auth: Auth::Bearer,
    headers: &[
        ("Accept", "application/json"),
        // The source documents the edge limiter refusing other agents with 429.
        ("User-Agent", "Warp/1.0"),
        ("x-warp-client-id", "warp-app"),
        // Came from CodexBar; its necessity is unverified.
        ("x-warp-os-category", "macOS"),
        ("x-warp-os-name", "macOS"),
        // Pinned: CodexBar sends the running OS version, and no recorded body pins one.
        ("x-warp-os-version", "14.0"),
    ],
    parse,
    credential_hint: "Warp settings → API key (docs.warp.dev/reference/cli/api-keys).",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use tidemark_types::{Snapshot, Timestamp, Window};

    /// Recorded by CodexBar, `WarpUsageFetcherTests.swift` — "parses snapshot and
    /// aggregates bonus credits". CodexBar asserts limit 1500, used 5, refresh
    /// 2026-02-28T19:16:33.462988Z, bonus total 35 across user and workspace grants,
    /// remaining 15, and 10 remaining against the earliest expiry (2026-03-01T10:00:00Z).
    const AGGREGATED_BONUS: &str = r#"
        {
          "data": {
            "user": {
              "__typename": "UserOutput",
              "user": {
                "requestLimitInfo": {
                  "isUnlimited": false,
                  "nextRefreshTime": "2026-02-28T19:16:33.462988Z",
                  "requestLimit": 1500,
                  "requestsUsedSinceLastRefresh": 5
                },
                "bonusGrants": [
                  {
                    "requestCreditsGranted": 20,
                    "requestCreditsRemaining": 10,
                    "expiration": "2026-03-01T10:00:00Z"
                  }
                ],
                "workspaces": [
                  {
                    "bonusGrantsInfo": {
                      "grants": [
                        {
                          "requestCreditsGranted": "15",
                          "requestCreditsRemaining": "5",
                          "expiration": "2026-03-15T10:00:00Z"
                        }
                      ]
                    }
                  }
                ]
              }
            }
          }
        }
        "#;

    /// Recorded by CodexBar, same file — "null unlimited and string numerics parse
    /// safely". `isUnlimited` null and both counts quoted as strings still read.
    const STRING_NUMERICS: &str = r#"
        {
          "data": {
            "user": {
              "__typename": "UserOutput",
              "user": {
                "requestLimitInfo": {
                  "isUnlimited": null,
                  "nextRefreshTime": "2026-02-28T19:16:33Z",
                  "requestLimit": "1500",
                  "requestsUsedSinceLastRefresh": "5"
                }
              }
            }
          }
        }
        "#;

    /// Recorded by CodexBar, same file — "graph QL errors throw API error". The
    /// rejection arrives in the errors array of a 200 body.
    const UNAUTHORIZED: &str = r#"
        {
          "errors": [
            { "message": "Unauthorized" }
          ]
        }
        "#;

    /// Recorded by CodexBar, same file — "unexpected typename returns parse error".
    /// An auth failure reported as the user object's typename, again inside a 200.
    const AUTH_ERROR_TYPENAME: &str = r#"
        {
          "data": {
            "user": {
              "__typename": "AuthError"
            }
          }
        }
        "#;

    /// Recorded by CodexBar, same file — "missing request limit info returns parse
    /// error". The user object with nothing in it.
    const MISSING_LIMIT_INFO: &str = r#"
        {
          "data": {
            "user": {
              "__typename": "UserOutput",
              "user": {}
            }
          }
        }
        "#;

    /// Recorded by CodexBar, same file — "invalid root returns parse error".
    const ROOT_ARRAY: &str = r#"[{ "data": {} }]"#;

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

    #[test]
    fn the_bonus_fixture_draws_the_credit_window_and_the_add_on_window() {
        let snapshot = parse(AGGREGATED_BONUS, at(1_785_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["credits", "addon"]);

        let credits = window(&snapshot, "credits");
        assert_eq!(credits.title, "Credits");
        assert!(
            (credits.used_percent - 5.0 / 1500.0 * 100.0).abs() < 1e-9,
            "5 used of 1500, CodexBar's own figures"
        );
        assert_eq!(credits.subtitle.as_deref(), Some("5/1500 credits"));
        assert_eq!(
            credits.resets_at,
            Some(at(1_772_306_193)),
            "2026-02-28T19:16:33Z, the refresh CodexBar's own test reads"
        );
        assert_eq!(credits.length, None, "the wire states no window length");

        let addon = window(&snapshot, "addon");
        assert_eq!(addon.title, "Add-on credits");
        assert_eq!(
            addon.used_percent,
            20.0 / 35.0 * 100.0,
            "20 of 35 granted credits spent across user and workspace grants"
        );
        assert_eq!(
            addon.subtitle.as_deref(),
            Some("10 credits expires on 2026-03-01"),
            "the earliest-expiring batch, with its own remaining"
        );
        assert_eq!(addon.resets_at, None, "a balance drains, it does not reset");
        assert_eq!(addon.length, None);

        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "credits",
            "both windows are lengthless, so the card keeps CodexBar's order"
        );
    }

    #[test]
    fn quoted_counts_and_a_null_unlimited_flag_read_the_same() {
        let snapshot = parse(STRING_NUMERICS, at(1_785_000_000)).expect("parses");
        assert_eq!(
            snapshot.windows.len(),
            1,
            "no grants on the wire, no add-on"
        );
        let credits = window(&snapshot, "credits");
        assert!(
            (credits.used_percent - 5.0 / 1500.0 * 100.0).abs() < 1e-9,
            "the string counts read as numbers"
        );
        assert_eq!(
            credits.resets_at,
            Some(at(1_772_306_193)),
            "2026-02-28T19:16:33Z without fractional seconds"
        );
    }

    #[test]
    fn a_rejected_key_in_a_200_body_asks_for_a_new_key() {
        for body in [UNAUTHORIZED, AUTH_ERROR_TYPENAME] {
            let error = parse(body, at(1_785_000_000)).expect_err("rejected");
            assert!(
                matches!(error, ProviderError::Credential { status: 401 }),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        for body in [
            "{\"partial\":",
            "not json",
            ROOT_ARRAY,
            MISSING_LIMIT_INFO,
            "{\"data\":{\"user\":\"not an object\"}}",
        ] {
            let error = parse(body, at(1_785_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_field_of_an_unrecognised_kind_is_skipped_and_other_errors_are_malformed() {
        let with_unknown = AGGREGATED_BONUS.replacen(
            "{\n          \"data\": {",
            "{\n          \"experiment\": {\"arm\": \"b\"},\n          \"data\": {",
            1,
        );
        let snapshot = parse(&with_unknown, at(1_785_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            window(&snapshot, "credits").subtitle.as_deref(),
            Some("5/1500 credits")
        );

        let other_error = "{\n  \"errors\": [\n    { \"message\": \"boom\" }\n  ]\n}";
        let error = parse(other_error, at(1_785_000_000)).expect_err("a GraphQL failure");
        assert!(
            matches!(error, ProviderError::Malformed(ref message) if message.contains("boom")),
            "{error}"
        );
    }

    #[test]
    fn the_spec_posts_the_recorded_graphql_query_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Warp");
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://app.warp.dev/graphql/v2?op=GetRequestLimitInfo"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        let Method::Post { body, content_type } = SPEC.method else {
            panic!("Warp's usage endpoint is a GraphQL POST");
        };
        assert_eq!(content_type, "application/json");
        assert!(
            body.contains("query GetRequestLimitInfo($requestContext: RequestContext!) {"),
            "the query is copied verbatim from the Swift source: {body}"
        );
        assert!(body.contains("requestLimitInfo {"));
        assert!(body.contains("requestsUsedSinceLastRefresh"));
        assert!(body.contains("bonusGrantsInfo {"));
        assert!(body.contains("\"operationName\":\"GetRequestLimitInfo\""));
        assert!(
            SPEC.headers.contains(&("x-warp-os-category", "macOS")),
            "came from CodexBar; its necessity is unverified"
        );
        assert!(
            SPEC.headers.contains(&("User-Agent", "Warp/1.0")),
            "the source documents the edge limiter refusing other user agents"
        );
        assert!(SPEC.options.is_empty(), "Warp has nothing to choose");
    }
}
