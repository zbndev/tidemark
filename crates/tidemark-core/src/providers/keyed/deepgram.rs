//! Deepgram.
//!
//! Ported from CodexBar's `Plugins/deepgram.js`, which is the whole contract in one file:
//! the two endpoints, the header the key rides in, every validation, and the rows. Never
//! seen answering: every number in the tests is a body CodexBar recorded.
//!
//! # Two requests, and a key that is not a bearer token
//!
//! The fetch is `GET {base}/projects` and then one
//! `GET {base}/projects/{id}/usage/breakdown` per project, so it is not a [`Spec`]. The key
//! goes in `Authorization: Token <key>` — a scheme of Deepgram's own, which [`Auth`] cannot
//! express by design: it puts the key in whole rather than deriving a header value from it.
//! Naming a project in the settings skips the first request, which is also what a key
//! scoped to one project needs, since listing projects is a Management API call it may not
//! be allowed to make.
//!
//! [`Spec`]: super::Spec
//! [`Auth`]: super::Auth
//!
//! # This card has no bars, and cannot
//!
//! Deepgram meters rather than allowances: the breakdown reports hours transcribed, tokens,
//! synthesised characters and request counts, and nothing anywhere in it says what any of
//! them is out of. There is no share to draw, so the card is detail rows only.
//!
//! # What the payload does not tell you
//!
//! **`hours` and `total_hours` are not the same hours.** The first is audio processed, the
//! second is what is billed for it — rounding and minimums make the billable figure the
//! larger one. Both are reported, on one row, because a card showing only one of them
//! invites the reader to reconcile it against an invoice it does not match.
//!
//! **The breakdown is a period, not a total.** `start` and `end` bound whatever window
//! Deepgram chose to answer with; across several projects the widest pair wins, so the row
//! covers every figure above it.
//!
//! **A number that is not a number is fatal, and a missing one is nought.** That is the
//! plugin's own rule, field by field: absent means zero, present and unreadable fails the
//! fetch. Four of the counters must be whole numbers and are refused if they are not.

use super::{HandSpec, OptionSchema, Options, base_url, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "deepgram";

/// Name of the API-URL setting under `[provider.deepgram]`.
pub const BASE_URL: &str = "base_url";

/// Name of the project setting under `[provider.deepgram]`.
pub const PROJECT_ID: &str = "project_id";

/// The host the plugin falls back to, `/v1` included: that exact string is its default.
pub const DEFAULT_BASE_URL: &str = "https://api.deepgram.com/v1";

/// One project, as the listing names it.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// The id the breakdown is fetched under.
    pub id: String,
    /// What Deepgram calls it, when it says.
    pub name: Option<String>,
}

/// Everything one breakdown reports, summed across the projects it came from.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Totals {
    /// Earliest `start` seen, as the string it arrived as: a date, not an instant.
    pub start: Option<String>,
    /// Latest `end` seen.
    pub end: Option<String>,
    /// Audio processed.
    pub hours: f64,
    /// Audio billed for, which is the larger of the two. See the module doc.
    pub total_hours: f64,
    pub agent_hours: f64,
    pub tokens_in: f64,
    pub tokens_out: f64,
    pub tts_characters: f64,
    pub requests: f64,
}

/// A value that must be a finite number when it is there at all, and nought when it is not.
///
/// `integer` refuses a fractional value, which is the plugin's rule for the four counters
/// that count things rather than measuring them.
fn optional_number(
    value: Option<&Value>,
    field: &str,
    integer: bool,
) -> Result<f64, ProviderError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(0.0);
    };
    let number = value
        .as_f64()
        .filter(|number| number.is_finite())
        .filter(|number| !integer || number.fract() == 0.0)
        .ok_or_else(|| ProviderError::malformed(format!("{field} has an invalid number")))?;
    Ok(number)
}

/// A value that must be a string when it is there at all.
fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, ProviderError> {
    match value.filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ProviderError::malformed(format!(
            "{field} must be a string"
        ))),
    }
}

/// The project listing. Pure: every validation above is reachable from a test.
pub fn parse_projects(body: &str) -> Result<Vec<Project>, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("response was not valid JSON: {e}")))?;
    let listed = root
        .as_object()
        .and_then(|root| root.get("projects"))
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::malformed("projects must be an array"))?;

    let mut projects = Vec::with_capacity(listed.len());
    for (index, entry) in listed.iter().enumerate() {
        let id = entry
            .as_object()
            .and_then(|entry| entry.get("project_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::malformed(format!("projects[{index}].project_id must be a string"))
            })?;
        let name = optional_string(
            entry.as_object().and_then(|entry| entry.get("name")),
            &format!("projects[{index}].name"),
        )?;
        projects.push(Project {
            id: id.to_owned(),
            name,
        });
    }
    Ok(projects)
}

/// One project's usage breakdown, added into `totals`. Pure.
pub fn parse_breakdown(body: &str, totals: &mut Totals) -> Result<(), ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("response was not valid JSON: {e}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| ProviderError::malformed("usage results must be an array"))?;
    let results = root
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::malformed("usage results must be an array"))?;

    // Validated and then discarded, exactly as the plugin does: nothing is read out of the
    // resolution, but a response whose shape is wrong here is wrong everywhere.
    if let Some(resolution) = root.get("resolution").filter(|value| !value.is_null()) {
        let resolution = resolution
            .as_object()
            .ok_or_else(|| ProviderError::malformed("resolution must be an object"))?;
        optional_string(resolution.get("units"), "resolution.units")?;
        optional_number(resolution.get("amount"), "resolution.amount", true)?;
    }

    let start = optional_string(root.get("start"), "start")?;
    let end = optional_string(root.get("end"), "end")?;

    for row in results {
        let row: &Map<String, Value> = row
            .as_object()
            .ok_or_else(|| ProviderError::malformed("usage result must be an object"))?;
        totals.hours += optional_number(row.get("hours"), "hours", false)?;
        totals.total_hours += optional_number(row.get("total_hours"), "total_hours", false)?;
        totals.agent_hours += optional_number(row.get("agent_hours"), "agent_hours", false)?;
        totals.tokens_in += optional_number(row.get("tokens_in"), "tokens_in", true)?;
        totals.tokens_out += optional_number(row.get("tokens_out"), "tokens_out", true)?;
        totals.tts_characters +=
            optional_number(row.get("tts_characters"), "tts_characters", true)?;
        totals.requests += optional_number(row.get("requests"), "requests", true)?;
    }

    // The widest period across the projects, compared as the strings they are: these are
    // `YYYY-MM-DD` dates, which sort correctly as text, and the plugin compares them so.
    if let Some(start) = start
        && totals.start.as_ref().is_none_or(|held| start < *held)
    {
        totals.start = Some(start);
    }
    if let Some(end) = end
        && totals.end.as_ref().is_none_or(|held| end > *held)
    {
        totals.end = Some(end);
    }
    Ok(())
}

/// A number with its thousands grouped, to `decimals` places.
fn grouped(value: f64, decimals: usize) -> String {
    let rendered = format!("{value:.decimals$}");
    let (whole, rest) = rendered.split_once('.').unwrap_or((rendered.as_str(), ""));
    let (sign, digits) = match whole.strip_prefix('-') {
        Some(digits) => ("-", digits),
        None => ("", whole),
    };
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(rendered.len() + bytes.len() / 3 + 1);
    out.push_str(sign);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }
    if !rest.is_empty() {
        out.push('.');
        out.push_str(rest);
    }
    out
}

/// A measured quantity: one decimal place, unless the value is already whole.
///
/// The plugin's own rule, and it asks about the value before rounding — 1621.974 is not
/// whole, so it prints as `1,622.0` rather than `1,622`.
fn decimal(value: f64) -> String {
    grouped(value, if value.fract() == 0.0 { 0 } else { 1 })
}

/// A counted quantity: no decimal places.
fn integer(value: f64) -> String {
    grouped(value, 0)
}

/// The rows the totals make, in the plugin's own order and wording.
pub fn snapshot(projects: &[Project], totals: &Totals, captured_at: Timestamp) -> Snapshot {
    let mut rows = vec![DetailRow {
        label: "Requests".to_owned(),
        value: integer(totals.requests),
    }];
    if totals.hours != 0.0 || totals.total_hours != 0.0 {
        // One row rather than the plugin's value-and-secondary pair, which this interface
        // has no second column for. Both figures are kept: see the module doc on why.
        rows.push(DetailRow {
            label: "Audio".to_owned(),
            value: format!(
                "{} hours · {} billable hours",
                decimal(totals.hours),
                decimal(totals.total_hours)
            ),
        });
    }
    if totals.agent_hours != 0.0 {
        rows.push(DetailRow {
            label: "Agent hours".to_owned(),
            value: decimal(totals.agent_hours),
        });
    }
    if totals.tokens_in != 0.0 || totals.tokens_out != 0.0 {
        rows.push(DetailRow {
            label: "Tokens".to_owned(),
            value: integer(totals.tokens_in + totals.tokens_out),
        });
    }
    if totals.tts_characters != 0.0 {
        rows.push(DetailRow {
            label: "TTS characters".to_owned(),
            value: integer(totals.tts_characters),
        });
    }
    if let (Some(start), Some(end)) = (&totals.start, &totals.end) {
        rows.push(DetailRow {
            label: "Period".to_owned(),
            value: format!("{start} to {end}"),
        });
    }

    // What the source shows under the provider's name. Deepgram has no plan to name, and
    // which project the figures are for is the thing a reader needs in its place.
    let who = match projects {
        [only] => format!("Project: {}", only.name.as_deref().unwrap_or(&only.id)),
        many => format!("{} projects", many.len()),
    };

    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        // Deepgram meters; nothing here is out of anything. See the module doc.
        windows: Vec::new(),
        details: vec![
            DetailSection {
                title: DetailSection::PLAN.to_owned(),
                rows: vec![DetailRow {
                    label: "Account".to_owned(),
                    value: who,
                }],
            },
            DetailSection {
                title: "Usage summary".to_owned(),
                rows,
            },
        ],
    }
}

/// Percent-encodes one path segment the way `encodeURIComponent` does, so a project id
/// with a slash or a space in it cannot walk out of its place in the path.
fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
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

/// The listing URL of a deployment base.
pub fn projects_url(base: &str) -> String {
    format!("{base}/projects")
}

/// The breakdown URL of one project.
pub fn breakdown_url(base: &str, project: &str) -> String {
    format!(
        "{base}/projects/{}/usage/breakdown",
        encode_segment(project)
    )
}

/// Deepgram as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Deepgram",
    credential: CredentialKind::Key,
    credential_hint: "Deepgram console → API keys.",
    options: &[
        OptionSchema {
            name: PROJECT_ID,
            title: "Project ID",
            description: Some(
                "Leave blank to read every project the key can list. A key scoped to one \
                 project cannot list them, and needs its id here.",
            ),
            default: "",
            choices: &[],
            required: false,
        },
        OptionSchema {
            name: BASE_URL,
            title: "API URL",
            description: Some("Only for a self-hosted deployment; HTTPS, or HTTP on loopback."),
            default: DEFAULT_BASE_URL,
            choices: &[],
            required: false,
        },
    ],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The base URL is
/// resolved here so a value the shared reader refuses is a [`ProviderError::Local`] naming
/// the setting, rather than a panic mid-fetch.
fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Deepgram::new(credential, options)?))
}

/// One Deepgram account: the key, the host, and the project it is pinned to if any.
pub struct Deepgram {
    client: reqwest::Client,
    credential: Credential,
    base: String,
    project: Option<String>,
}

impl Deepgram {
    /// Builds a client. The URL is resolved once, here, because a setting that changed the
    /// host would otherwise take effect only on the next daemon restart.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        let base = base_url(options, BASE_URL, DEFAULT_BASE_URL)?;
        let project = options
            .get(PROJECT_ID)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        Ok(Self {
            client: http::client()?,
            credential,
            base,
            project,
        })
    }

    /// The base URL this instance polls.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// A GET with the key in Deepgram's own scheme, built but not sent, so that the
    /// placement of the key is testable without a server.
    pub fn request(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Token {}", self.credential.expose()),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn get(&self, url: &str) -> Result<String, ProviderError> {
        super::request(&self.client, self.request(url)?).await
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let projects = match &self.project {
            // A key scoped to one project may not list them, so a configured id skips the
            // listing entirely rather than spending a request that would 403.
            Some(id) => vec![Project {
                id: id.clone(),
                name: None,
            }],
            None => parse_projects(&self.get(&projects_url(&self.base)).await?)?,
        };
        if projects.is_empty() {
            return Err(ProviderError::malformed(
                "no projects were returned for this API key",
            ));
        }

        let mut totals = Totals::default();
        for project in &projects {
            let body = self.get(&breakdown_url(&self.base, &project.id)).await?;
            parse_breakdown(&body, &mut totals)?;
        }
        Ok(snapshot(&projects, &totals, Timestamp::now()))
    }
}

impl fmt::Debug for Deepgram {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deepgram")
            .field("id", &PROVIDER_ID)
            .field("base", &self.base)
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

impl Provider for Deepgram {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded by CodexBar, `DeepgramProviderTests.swift` — "usage breakdown fixture
    /// matches visible detail golden". Its own test asserts `373,400` requests,
    /// `1,622.0 hours`, `1,625.2 billable hours`, `41.3` agent hours, `1,540` tokens,
    /// `9,158,866` TTS characters and the period `2025-01-16 to 2025-01-23`.
    const BREAKDOWN: &str = r#"
    {
      "start": "2025-01-16",
      "end": "2025-01-23",
      "resolution": { "units": "day", "amount": 1 },
      "results": [
        {
          "hours": 1619.7242069444444,
          "total_hours": 1621.7395791666668,
          "agent_hours": 41.33564388888889,
          "tokens_in": 1200,
          "tokens_out": 340,
          "tts_characters": 9158866,
          "requests": 373381,
          "grouping": { "start": "2025-01-16", "end": "2025-01-16", "endpoint": "listen" }
        },
        {
          "hours": 2.25,
          "total_hours": 3.5,
          "requests": 19,
          "grouping": { "start": "2025-01-17", "end": "2025-01-17", "endpoint": "speak" }
        }
      ]
    }"#;

    /// Recorded by CodexBar, same file — "project discovery aggregates every project".
    const LISTING: &str = r#"{"projects":[{"project_id":"project-a","name":"Alpha"},{"project_id":"project-b","name":"Beta"}]}"#;
    const PROJECT_A: &str = r#"{"start":"2025-01-16","end":"2025-01-23","results":[{"hours":1,"total_hours":2,"requests":3}]}"#;
    const PROJECT_B: &str = r#"{"start":"2025-01-17","end":"2025-01-24","results":[{"hours":4,"total_hours":5,"requests":6}]}"#;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn row<'a>(snapshot: &'a Snapshot, label: &str) -> &'a str {
        snapshot
            .details
            .iter()
            .flat_map(|section| section.rows.iter())
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no {label} row"))
            .value
            .as_str()
    }

    #[test]
    fn the_recorded_breakdown_makes_the_rows_the_source_shows() {
        let mut totals = Totals::default();
        parse_breakdown(BREAKDOWN, &mut totals).expect("parses");
        let pinned = [Project {
            id: "project-123".to_owned(),
            name: None,
        }];
        let snapshot = snapshot(&pinned, &totals, at(1_800_000_000));

        assert_eq!(row(&snapshot, "Requests"), "373,400");
        assert_eq!(
            row(&snapshot, "Audio"),
            "1,622.0 hours · 1,625.2 billable hours"
        );
        assert_eq!(row(&snapshot, "Agent hours"), "41.3");
        assert_eq!(row(&snapshot, "Tokens"), "1,540");
        assert_eq!(row(&snapshot, "TTS characters"), "9,158,866");
        assert_eq!(row(&snapshot, "Period"), "2025-01-16 to 2025-01-23");
        assert_eq!(row(&snapshot, "Account"), "Project: project-123");
        assert!(
            snapshot.windows.is_empty(),
            "Deepgram meters; nothing here is out of anything"
        );
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);
    }

    #[test]
    fn every_project_is_added_up_and_the_widest_period_wins() {
        let projects = parse_projects(LISTING).expect("parses");
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id, "project-a");
        assert_eq!(projects[0].name.as_deref(), Some("Alpha"));

        let mut totals = Totals::default();
        parse_breakdown(PROJECT_A, &mut totals).expect("parses");
        parse_breakdown(PROJECT_B, &mut totals).expect("parses");
        let snapshot = snapshot(&projects, &totals, at(1_800_000_000));

        assert_eq!(row(&snapshot, "Requests"), "9");
        assert_eq!(row(&snapshot, "Audio"), "5 hours · 7 billable hours");
        assert_eq!(row(&snapshot, "Period"), "2025-01-16 to 2025-01-24");
        assert_eq!(row(&snapshot, "Account"), "2 projects");
        assert!(
            snapshot
                .details
                .iter()
                .flat_map(|section| section.rows.iter())
                .all(|row| row.label != "Tokens"),
            "a counter nobody reported draws no row"
        );
    }

    #[test]
    fn a_shape_the_plugin_refuses_is_malformed() {
        assert!(matches!(
            parse_projects(r#"{"projects":{}}"#),
            Err(ProviderError::Malformed(_))
        ));
        assert!(matches!(
            parse_projects(r#"{"projects":[{"project_id":7}]}"#),
            Err(ProviderError::Malformed(_))
        ));
        assert!(matches!(
            parse_projects(r#"{"projects":[{"project_id":"a","name":7}]}"#),
            Err(ProviderError::Malformed(_))
        ));
        assert!(matches!(
            parse_projects("not-json"),
            Err(ProviderError::Malformed(_))
        ));

        for body in [
            r#"{"results":{}}"#,
            r#"{"results":["row"]}"#,
            r#"{"results":[],"resolution":[]}"#,
            r#"{"results":[],"resolution":{"amount":1.5}}"#,
            r#"{"results":[],"start":7}"#,
            // A count that counts things cannot be fractional.
            r#"{"results":[{"requests":1.5}]}"#,
            // A measurement may be fractional but must still be a number.
            r#"{"results":[{"hours":"two"}]}"#,
            "not-json",
        ] {
            let mut totals = Totals::default();
            assert!(
                matches!(
                    parse_breakdown(body, &mut totals),
                    Err(ProviderError::Malformed(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn an_absent_counter_is_nought_and_a_null_one_is_too() {
        let mut totals = Totals::default();
        parse_breakdown(
            r#"{"results":[{"hours":null,"requests":null,"tts_characters":5}],
                "resolution":null,"start":null,"end":null}"#,
            &mut totals,
        )
        .expect("absent means nought, which is the plugin's own rule");
        assert_eq!(totals.hours, 0.0);
        assert_eq!(totals.requests, 0.0);
        assert_eq!(totals.tts_characters, 5.0);
        assert_eq!(totals.start, None);
    }

    #[test]
    fn the_key_rides_in_deepgrams_own_scheme_and_never_reaches_a_rendering() {
        let client = Deepgram::new(Credential::new("dg-test"), &options(&[])).expect("builds");
        assert_eq!(client.base(), DEFAULT_BASE_URL);
        let request = client
            .request(&breakdown_url(client.base(), "project-123"))
            .expect("builds");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Token dg-test",
            "a scheme of Deepgram's own, not a bearer token"
        );
        assert_eq!(
            request.url().path(),
            "/v1/projects/project-123/usage/breakdown"
        );
        assert!(!format!("{client:?}").contains("dg-test"));
    }

    #[test]
    fn a_project_id_cannot_walk_out_of_its_place_in_the_path() {
        assert_eq!(
            breakdown_url("https://api.deepgram.com/v1", "a/../b c"),
            "https://api.deepgram.com/v1/projects/a%2F..%2Fb%20c/usage/breakdown"
        );
        assert_eq!(
            projects_url("https://api.deepgram.com/v1"),
            "https://api.deepgram.com/v1/projects"
        );
    }

    #[test]
    fn the_settings_choose_the_host_and_the_project() {
        let client = Deepgram::new(
            Credential::new("dg-test"),
            &options(&[
                (BASE_URL, "https://deepgram.test/v1/"),
                (PROJECT_ID, " project-123 "),
            ]),
        )
        .expect("builds");
        assert_eq!(client.base(), "https://deepgram.test/v1");
        assert_eq!(client.project.as_deref(), Some("project-123"));

        let refused = Deepgram::new(
            Credential::new("dg-test"),
            &options(&[(BASE_URL, "http://deepgram.test/v1")]),
        )
        .expect_err("a key over plain HTTP to a remote host is a key given away");
        assert!(matches!(refused, ProviderError::Local(_)), "{refused}");

        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.options[1].default, DEFAULT_BASE_URL);
    }

    #[tokio::test]
    async fn a_blank_credential_is_refused_before_a_request_is_spent() {
        let client = Deepgram::new(Credential::new("   "), &options(&[])).expect("builds");
        assert!(matches!(
            client.fetch().await,
            Err(ProviderError::Credential { status: 401 })
        ));
        assert_eq!(client.id().as_str(), PROVIDER_ID);
        assert_eq!(client.account(), AccountId::default());
    }
}
