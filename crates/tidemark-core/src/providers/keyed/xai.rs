//! xAI.
//!
//! Ported from CodexBar's `Plugins/xai.js`; `XAIProviderTests.swift` holds the recorded
//! bodies. Never seen answering: every number in the tests below is a body CodexBar
//! recorded.
//!
//! # The credential is two things
//!
//! A **management key** — bearer on every request — and a **team ID**, a free-text
//! required option, validated as the plugin validates it: no `/`, and neither `.` nor
//! `..`, so the team cannot smuggle path structure into the URL (what the slash check
//! misses, the percent-encoding renders inert). The management key is *not* an
//! inference API key, and the hint says so, because pasting the wrong one is the
//! obvious mistake.
//!
//! # The two requests
//!
//! `GET /v1/billing/teams/<team>/prepaid/balance` returns `total.val` as a **string of
//! cents, negated**: the balance is `-Number(val)/100`, so `"-1000"` is $10.00 and
//! `"2500"` is −$25.00. A string only — a number where the string belongs is malformed,
//! as is anything that is not the plugin's cent shape `/^-?\d+(\.\d+)?$/`. This request
//! is the point of the fetch and can fail it; its statuses map through the shared
//! transport (401/403 to a rejected credential, 429 to rate-limited, anything else to
//! an HTTP error), which is exactly the classification the recorded tests pin.
//!
//! `POST /v1/billing/teams/<team>/usage` fetches thirty days of spend for the
//! "Last 30 days" row — ported because the card shows that number. The per-day
//! breakdown CodexBar draws as a chart is dropped: a Tidemark card has no chart. The
//! POST is optional, as in the source: every failure except an authentication one is
//! swallowed — but the row says "Unavailable right now", where CodexBar silently shows
//! $0.00 over an empty history. `limitReached: true` marks the row partial, as the
//! source marks its chart partial.
//!
//! # No window
//!
//! Prepaid credits have no limit: there is nothing to draw a bar against, so the card
//! renders the billing rows and nothing else. That is accepted, and recorded here.

use super::{HandSpec, OptionSchema, Options, redact_query, required};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http, parse_rfc3339};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp,
};
use time::OffsetDateTime;

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "xai";

/// Name of the team-ID setting under `[provider.xai]`.
pub const TEAM: &str = "team_id";

/// Where the billing API lives. One host, no regional or self-hosted variant.
const BASE_URL: &str = "https://management-api.x.ai/v1/billing/teams";

/// xAI as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "xAI",
    credential: CredentialKind::Key,
    credential_hint: "xAI Console → Settings → Management Keys. A management key is not an inference API key.",
    options: &[OptionSchema {
        name: TEAM,
        title: "Team ID",
        description: Some(
            "The team the Management key belongs to, from the console's billing page.",
        ),
        default: "",
        choices: &[],
        required: true,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The team ID
/// is read and validated here, so a missing or invalid one is named on the card rather
/// than reaching the wire as a malformed URL.
fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Xai::new_for_account(
        account, credential, options,
    )?))
}

/// One xAI account: the management key, the team it bills, and the two endpoints that
/// come from both.
pub struct Xai {
    tidemark_account: AccountId,
    client: reqwest::Client,
    credential: Credential,
    balance_url: String,
    usage_url: String,
}

impl Xai {
    /// Builds a client. The team ID is part of both paths, so it is resolved once, here.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), credential, options)
    }

    fn new_for_account(
        account_id: AccountId,
        credential: Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        let team = required(options, TEAM, "Team ID")?;
        if team.contains('/') || team == "." || team == ".." {
            return Err(ProviderError::Local(format!(
                "Team ID {team:?} is not valid — it may not contain '/', or be '.' or '..'"
            )));
        }
        let root = format!("{BASE_URL}/{}", encode_team(&team));
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: http::client()?,
            credential,
            balance_url: format!("{root}/prepaid/balance"),
            usage_url: format!("{root}/usage"),
        })
    }

    /// The balance URL this instance polls.
    pub fn balance_url(&self) -> &str {
        &self.balance_url
    }

    /// The usage URL this instance posts to.
    pub fn usage_url(&self) -> &str {
        &self.usage_url
    }

    /// The balance request, built but not sent, so the placement of the key is testable.
    fn balance_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(&self.balance_url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    /// The usage request for the thirty days ending at `now`, with the fixed analytics
    /// body the source posts.
    fn usage_request(&self, now: Timestamp) -> Result<reqwest::Request, ProviderError> {
        self.client
            .post(&self.usage_url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(usage_body(now))
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let balance = parse_balance(
            &super::request(PROVIDER_ID, &self.client, self.balance_request()?).await?,
        )?;
        // The history is optional, as in the source: every failure but an
        // authentication one is swallowed into the unavailable row. A rejected
        // credential on this request is the same rejection as on the balance, and the
        // interface should ask for a new key, not draw a balance over a dead one.
        let history = match self.history(now).await {
            Ok(history) => history,
            Err(ProviderError::Credential { status }) => {
                return Err(ProviderError::Credential { status });
            }
            Err(_) => History::Unavailable,
        };
        Ok(snapshot_for_account(
            balance,
            history,
            now,
            &self.tidemark_account,
        ))
    }

    async fn history(&self, now: Timestamp) -> Result<History, ProviderError> {
        let body = super::request(PROVIDER_ID, &self.client, self.usage_request(now)?).await?;
        parse_usage(&body).map(History::Present)
    }
}

impl fmt::Debug for Xai {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Xai")
            .field("id", &PROVIDER_ID)
            .field("balance_url", &self.balance_url)
            .finish_non_exhaustive()
    }
}

impl Provider for Xai {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        self.tidemark_account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// What the optional usage request produced.
#[derive(Debug, Clone, PartialEq)]
enum History {
    /// The daily spend the analytics POST returned.
    Present(UsageHistory),
    /// The request failed or its body was unreadable; the row says so.
    Unavailable,
}

/// Thirty days of spend, summed per UTC day.
#[derive(Debug, Clone, PartialEq)]
struct UsageHistory {
    days: Vec<DaySpend>,
    /// The analytics query hit its row limit; the totals are partial, not wrong.
    limit_reached: bool,
}

/// One day's spend.
#[derive(Debug, Clone, PartialEq)]
struct DaySpend {
    /// `YYYY-MM-DD`, the bucket the source's day grouping produces.
    day: String,
    usd: f64,
}

/// Reads the balance body. Pure: every recorded ledger amount is reachable from a test.
fn parse_balance(body: &str) -> Result<f64, ProviderError> {
    #[derive(Deserialize)]
    struct BalanceBody {
        total: Total,
    }
    #[derive(Deserialize)]
    struct Total {
        val: String,
    }
    let body: BalanceBody = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an xAI balance response: {e}")))?;
    let raw = body.total.val.trim();
    if !cent_amount(raw) {
        return Err(ProviderError::malformed(
            "the xAI balance total.val is not a cent amount",
        ));
    }
    // A number too long for f64 parses to infinity, which is as unreadable as "n/a": the
    // shape held but the value cannot be read, and that is malformed, not a panic.
    let cents: f64 = raw
        .parse::<f64>()
        .ok()
        .filter(|cents| cents.is_finite())
        .ok_or_else(|| {
            ProviderError::malformed("the xAI balance total.val is not a finite amount")
        })?;
    // The sign is the ledger's, and the balance is the negation of it: -1000 cents is
    // $10.00 of credit.
    Ok(-cents / 100.0)
}

/// The source's cent shape, `/^-?\d+(\.\d+)?$/` verbatim: an optional minus, digits, an
/// optional fraction with at least one digit.
fn cent_amount(raw: &str) -> bool {
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// Reads the usage body. Pure, and whole: a point of a recognised shape whose value is
/// not a number fails the history, which the fetch then swallows into the unavailable
/// row — never into a day that quietly under-reports.
fn parse_usage(body: &str) -> Result<UsageHistory, ProviderError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UsageBody {
        #[serde(default)]
        time_series: Vec<Series>,
        #[serde(default)]
        limit_reached: bool,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Series {
        #[serde(default)]
        data_points: Vec<Point>,
    }
    #[derive(Deserialize)]
    struct Point {
        timestamp: String,
        #[serde(default)]
        values: Vec<f64>,
    }
    let usage: UsageBody = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not an xAI usage history: {e}")))?;
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    for series in &usage.time_series {
        for point in &series.data_points {
            let at = parse_rfc3339(point.timestamp.trim()).ok_or_else(|| {
                ProviderError::malformed("an xAI usage point has an unreadable timestamp")
            })?;
            // An absent or empty values array reads as zero spend, as the source reads it.
            *totals.entry(utc_day(at)).or_default() += point.values.first().copied().unwrap_or(0.0);
        }
    }
    Ok(UsageHistory {
        days: totals
            .into_iter()
            .map(|(day, usd)| DaySpend { day, usd })
            .collect(),
        limit_reached: usage.limit_reached,
    })
}

/// The fixed analytics request the source posts, with the window computed from the
/// clock: 29 days back from now, to UTC midnight, through now — thirty days of buckets.
fn usage_body(now: Timestamp) -> String {
    let end = utc_date(now);
    // `Timestamp` only holds 2020..2100, so the subtraction cannot leave the range.
    let start = (end - time::Duration::days(29)).replace_time(time::Time::MIDNIGHT);
    format!(
        concat!(
            r#"{{"analyticsRequest":{{"timeRange":{{"startTime":"{}","endTime":"{}","#,
            r#""timezone":"Etc/GMT"}},"timeUnit":"TIME_UNIT_DAY","#,
            r#""values":[{{"name":"usd","aggregation":"AGGREGATION_SUM"}}],"#,
            r#""groupBy":[],"filters":[]}}}}"#
        ),
        stamp(start),
        stamp(end)
    )
}

/// Percent-encodes the team ID as the source's `encodeURIComponent` does, so a value
/// the validation let through cannot alter the URL's structure.
fn encode_team(team: &str) -> String {
    let mut encoded = String::with_capacity(team.len());
    for byte in team.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => encoded.push(byte as char),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Assembles the snapshot. Pure, so the recorded renderings are reachable from a test.
///
/// No window, ever: prepaid credits have no limit to draw a bar against, and the shape
/// for that is details only — the card renders the rows and nothing else, which is
/// accepted.
#[cfg(test)]
fn snapshot(balance: f64, history: History, captured_at: Timestamp) -> Snapshot {
    snapshot_for_account(balance, history, captured_at, &AccountId::default())
}

fn snapshot_for_account(
    balance: f64,
    history: History,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Snapshot {
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account_id.clone(),
        captured_at,
        windows: Vec::new(),
        details: vec![billing_details(balance, &history)],
    }
}

/// The billing rows: the balance, and the thirty-day spend the history managed.
fn billing_details(balance: f64, history: &History) -> DetailSection {
    let mut rows = vec![labeled("Prepaid balance", usd(balance))];
    match history {
        History::Present(usage) => {
            let spend: f64 = usage.days.iter().map(|day| day.usd).sum();
            let label = if usage.limit_reached {
                "Last 30 days (partial)"
            } else {
                "Last 30 days"
            };
            rows.push(labeled(label, usd(spend)));
        }
        // Where the source silently sums an empty history to $0.00, this says what
        // happened: a row that reads $0.00 over a failed request is a wrong number
        // wearing the right format.
        History::Unavailable => rows.push(labeled("Last 30 days", "Unavailable right now")),
    }
    DetailSection {
        title: "Billing summary".to_owned(),
        rows,
    }
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

/// The source's currency rendering: dollars, two fraction digits, sign kept — an
/// overdrawn ledger is −$25.00, not $0.00.
fn usd(value: f64) -> String {
    format!("${value:.2}")
}

/// The UTC calendar day of an instant, `YYYY-MM-DD`.
fn utc_day(at: Timestamp) -> String {
    let date = utc_date(at);
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The source's timestamp spelling: `YYYY-MM-DD HH:MM:SS`, zero-filled.
fn stamp(at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// The UTC reading of an instant. `Timestamp` only holds 2020..2100, which
/// `from_unix_timestamp` always accepts.
fn utc_date(at: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(at.as_unix()).expect("a plausible timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_800_000_000).expect("plausible")
    }

    fn options(team: &str) -> Options {
        [(TEAM.to_owned(), team.to_owned())].into_iter().collect()
    }

    // Recorded bodies, verbatim from XAIProviderTests.swift.
    const BALANCE: &str = r#"{"total":{"val":"-1000"}}"#;
    const BALANCE_OVERDRAWN: &str = r#"{"total":{"val":"2500"}}"#;
    const BALANCE_ZERO: &str = r#"{"total":{"val":"0"}}"#;
    const BALANCE_NEGATIVE_CENTS: &str = r#"{"total":{"val":"-333"}}"#;
    const BALANCE_MALFORMED: &str = r#"{"total":{"val":"n/a"}}"#;
    const USAGE: &str = r#"{
      "timeSeries": [
        {"dataPoints": [
          {"timestamp":"2027-01-13T00:00:00Z","values":[0.75973725]},
          {"timestamp":"2027-01-14T00:00:00Z","values":[0.5]},
          {"timestamp":"2027-01-15T00:00:00Z","values":[0]}
        ]},
        {"dataPoints": [
          {"timestamp":"2027-01-13T00:00:00Z","values":[0.5]},
          {"timestamp":"2027-01-14T00:00:00Z","values":[0]},
          {"timestamp":"2027-01-15T00:00:00Z","values":[0]}
        ]}
      ],
      "limitReached": false
    }"#;

    #[test]
    fn the_recorded_ledger_amounts_negate_and_rescale() {
        // `total.val` is a string of cents, negated: the balance is -val/100.
        assert_eq!(parse_balance(BALANCE).expect("parses"), 10.0);
        assert_eq!(parse_balance(BALANCE_OVERDRAWN).expect("parses"), -25.0);
        assert_eq!(parse_balance(BALANCE_ZERO).expect("parses"), 0.0);
        assert_eq!(parse_balance(BALANCE_NEGATIVE_CENTS).expect("parses"), 3.33);
    }

    #[test]
    fn a_balance_that_is_not_a_string_of_cents_is_malformed() {
        // `"n/a"` is the recorded malformed fixture; a number where the string belongs,
        // and the procedure's canonical partial body, fail the same way.
        for body in [
            BALANCE_MALFORMED,
            r#"{"total":{"val":-1000}}"#,
            "{\"partial\":",
        ] {
            let error = parse_balance(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error:?}"
            );
        }
        let named = parse_balance(BALANCE_MALFORMED).expect_err("names the field");
        assert!(format!("{named}").contains("total.val"), "{named}");
    }

    #[test]
    fn a_cent_amount_too_large_for_f64_is_malformed_not_a_panic() {
        // Not a recorded body: the recorded balance with `total.val` replaced by 400
        // ones, which passes the cent shape but overflows f64 to infinity. A recognised
        // field whose value cannot be read is malformed, never a panic on server input.
        let overflowing = BALANCE.replace("-1000", &"1".repeat(400));
        let error = parse_balance(&overflowing).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn the_recorded_usage_history_sums_by_day() {
        let history = parse_usage(USAGE).expect("parses");
        assert!(!history.limit_reached);
        let days: Vec<&DaySpend> = history.days.iter().collect();
        let expected: [(&str, f64); 3] = [
            ("2027-01-13", 1.25973725),
            ("2027-01-14", 0.5),
            ("2027-01-15", 0.0),
        ];
        assert_eq!(days.len(), expected.len());
        for (day, (name, usd)) in days.iter().zip(&expected) {
            assert_eq!(day.day, *name);
            assert!(
                (day.usd - usd).abs() < 1e-9,
                "{}: {} vs {usd}",
                day.day,
                day.usd
            );
        }
    }

    #[test]
    fn usage_fields_this_parser_does_not_know_are_skipped() {
        // The unknown-kind rule: an unfamiliar field rides along without breaking the
        // recognised ones.
        let history = parse_usage(&USAGE.replace(
            "\"limitReached\": false",
            "\"limitReached\": false, \"future\": true",
        ))
        .expect("parses");
        assert_eq!(history.days.len(), 3);
    }

    #[test]
    fn a_usage_point_whose_value_is_not_a_number_is_malformed() {
        let error = parse_usage(
            r#"{"timeSeries":[{"dataPoints":[
                 {"timestamp":"2027-01-13T00:00:00Z","values":["0.5"]}]}]}"#,
        )
        .expect_err("must refuse");
        assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn the_recorded_balance_and_history_render_two_rows() {
        let snapshot = snapshot(
            parse_balance(BALANCE).expect("parses"),
            History::Present(parse_usage(USAGE).expect("parses")),
            now(),
        );
        assert!(snapshot.windows.is_empty(), "prepaid credits have no limit");
        assert_eq!(
            snapshot.details,
            vec![DetailSection {
                title: "Billing summary".to_owned(),
                rows: vec![
                    DetailRow {
                        label: "Prepaid balance".to_owned(),
                        value: "$10.00".to_owned(),
                    },
                    DetailRow {
                        label: "Last 30 days".to_owned(),
                        value: "$1.76".to_owned(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn a_limit_reached_history_marks_the_spend_partial() {
        let partial = USAGE.replace("\"limitReached\": false", "\"limitReached\": true");
        let snapshot = snapshot(
            parse_balance(BALANCE).expect("parses"),
            History::Present(parse_usage(&partial).expect("parses")),
            now(),
        );
        let row = &snapshot.details[0].rows[1];
        assert_eq!(row.label, "Last 30 days (partial)");
        assert_eq!(row.value, "$1.76");
    }

    #[test]
    fn an_unavailable_history_preserves_the_recorded_balance() {
        let snapshot = snapshot(
            parse_balance(BALANCE).expect("parses"),
            History::Unavailable,
            now(),
        );
        assert_eq!(snapshot.details[0].rows[0].label, "Prepaid balance");
        assert_eq!(snapshot.details[0].rows[0].value, "$10.00");
        assert_eq!(snapshot.details[0].rows[1].label, "Last 30 days");
        assert_eq!(snapshot.details[0].rows[1].value, "Unavailable right now");
    }

    #[test]
    fn the_team_id_resolves_into_the_paths_and_is_validated() {
        let xai = Xai::new(
            Credential::new("fixture-management-key"),
            &options("team-1234"),
        )
        .expect("builds");
        assert_eq!(
            xai.balance_url(),
            "https://management-api.x.ai/v1/billing/teams/team-1234/prepaid/balance"
        );
        assert_eq!(
            xai.usage_url(),
            "https://management-api.x.ai/v1/billing/teams/team-1234/usage"
        );

        let unset = Xai::new(Credential::new("key"), &Options::new())
            .expect_err("the required option is unset");
        assert!(format!("{unset}").contains("Team ID"), "{unset}");

        // `team/../other` is the recorded invalid value; `.` and `..` are the plugin's
        // other two refusals.
        for invalid in ["team/../other", ".", ".."] {
            let error = Xai::new(Credential::new("key"), &options(invalid))
                .expect_err("an invalid team id is refused");
            assert!(
                matches!(error, ProviderError::Local(_)),
                "{invalid}: {error:?}"
            );
        }
    }

    #[test]
    fn the_team_id_is_encoded_as_one_path_segment() {
        let spaced = Xai::new(Credential::new("key"), &options("team 1234")).expect("builds");
        assert_eq!(
            spaced.balance_url(),
            "https://management-api.x.ai/v1/billing/teams/team%201234/prepaid/balance"
        );
    }

    #[test]
    fn the_requests_match_the_recorded_golden() {
        let xai = Xai::new(
            Credential::new("fixture-management-key"),
            &options("team-1234"),
        )
        .expect("builds");

        let balance = xai.balance_request().expect("builds");
        assert_eq!(balance.method(), reqwest::Method::GET);
        assert_eq!(
            balance.url().as_str(),
            "https://management-api.x.ai/v1/billing/teams/team-1234/prepaid/balance"
        );
        assert_eq!(
            balance
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer fixture-management-key"
        );
        assert_eq!(
            balance
                .headers()
                .get(reqwest::header::ACCEPT)
                .expect("present"),
            "application/json"
        );

        // The recorded golden pins the clock at 1_800_000_000 (2027-01-15T08:00:00Z)
        // and the window at 29 days back to UTC midnight.
        let usage = xai.usage_request(now()).expect("builds");
        assert_eq!(usage.method(), reqwest::Method::POST);
        assert_eq!(
            usage.url().as_str(),
            "https://management-api.x.ai/v1/billing/teams/team-1234/usage"
        );
        assert_eq!(
            usage
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .expect("present"),
            "application/json"
        );
        let body = usage
            .body()
            .expect("present")
            .as_bytes()
            .expect("in memory");
        let payload: serde_json::Value = serde_json::from_slice(body).expect("valid JSON");
        let time_range = &payload["analyticsRequest"]["timeRange"];
        assert_eq!(time_range["startTime"], "2026-12-17 00:00:00");
        assert_eq!(time_range["endTime"], "2027-01-15 08:00:00");
        assert_eq!(time_range["timezone"], "Etc/GMT");
        assert_eq!(payload["analyticsRequest"]["timeUnit"], "TIME_UNIT_DAY");
    }

    #[test]
    fn the_spec_publishes_a_required_team_and_says_what_the_key_is() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "xAI");
        assert!(
            SPEC.credential_hint.contains("inference"),
            "the hint must say a management key is not an inference key"
        );
        assert_eq!(SPEC.options.len(), 1);
        assert!(SPEC.options[0].required);
        assert!(
            build(
                AccountId::default(),
                Credential::new("key"),
                &options("team-1234")
            )
            .is_ok()
        );
        assert!(
            build(
                AccountId::default(),
                Credential::new("key"),
                &Options::new()
            )
            .is_err()
        );
    }

    #[test]
    fn an_xai_client_never_prints_its_credential() {
        let xai =
            Xai::new(Credential::new("sk-super-secret"), &options("team-1234")).expect("builds");
        let rendered = format!("{xai:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
