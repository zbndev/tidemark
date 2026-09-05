//! Grok's credit usage, read from the grok CLI's own login.
//!
//! The credential is the CLI's `~/.grok/auth.json` (or `$GROK_HOME/auth.json`), a JSON
//! map keyed by OIDC scope. The entry whose scope starts `https://auth.x.ai::` is the
//! SuperGrok login and wins; the legacy `https://accounts.x.ai/sign-in` scope (and any
//! other `/sign-in` scope) stands in when the OIDC record is missing — or stale, an
//! entry without a usable `key` never being allowed to shadow a healthy one. The file is
//! read in place and never written, per ADR 0001.
//!
//! There is no refresh: upstream does not renew the login from a session either, so an
//! `expires_at` that has passed is [`ProviderError::NoCredential`] pointing at
//! `grok login`, and so is a file that names no usable entry at all. A pasted `xai-`
//! token is refused at build — that is an x.ai API key, the `xai` provider's credential,
//! not a Grok login.
//!
//! The quota is one credit figure: `GET /v1/billing?format=credits` reports the share of
//! the included allowance spent (`creditUsagePercent`, or an on-demand `used` against its
//! `cap`), and `GET /v1/settings` names the billed tier (`subscription_tier_display`) the
//! way the plan writes it — "SuperGrok Heavy", not a coded `SUPERGROK_HEAVY`. The tier
//! hop is advisory, exactly as upstream treats it: a refusal there costs the Plan row,
//! never the card. Both calls carry the same two headers the CLI itself sends — the
//! bearer token and `x-xai-token-auth: xai-grok-cli`.
//!
//! Two upstream graces are not ported. The billing surface may answer a reset instant
//! with no usage figure at all; upstream carries that shape as an unknown and reports
//! "usage unavailable", while a Tidemark card cannot be drawn without a number, so the
//! fetch fails malformed instead. And when neither the billing body nor the settings
//! names a tier, upstream falls back to the OIDC login method ("SuperGrok" for any
//! `oidc` sign-in); the Plan row here says nothing rather than guessing at a billed plan.
//!
//! The reset instant is drawn as a mark on a window of no stated length: the period the
//! body describes (weekly in every recorded shape) is real, but upstream declines to
//! infer a cadence from time-to-reset alone, and so do we — inventing a length would
//! file the window under a key the provider never claimed.

use super::{HandSpec, Options, ProviderError, redact_query};
use crate::providers::{BoxFuture, Credential, Provider};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "grok";

/// The billing host the CLI itself talks to; the settings read is the same host, one
/// path over. Kept as a field so a test can point it at a loopback.
const PROXY_BASE: &str = "https://cli-chat-proxy.grok.com";
/// Names this call as the CLI's own, which is what makes the proxy answer a CLI login.
const TOKEN_AUTH: &str = "xai-grok-cli";
/// The top-level OIDC scope a `grok login` writes for SuperGrok subscribers.
const OIDC_SCOPE_PREFIX: &str = "https://auth.x.ai::";
/// The scope of the older session-style login.
const LEGACY_SCOPE: &str = "https://accounts.x.ai/sign-in";

/// Grok as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Grok",
    credential: CredentialKind::External,
    credential_hint: "Read the grok CLI's own login (`grok login`).",
    options: &[],
    build,
};

fn build(
    account: AccountId,
    credential: Credential,
    _options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    if credential
        .expose()
        .trim()
        .to_ascii_lowercase()
        .starts_with("xai-")
    {
        return Err(ProviderError::Local(
            "an `xai-` key is an x.ai API key, not a Grok login; add the xai provider for it"
                .into(),
        ));
    }
    Ok(Arc::new(Grok::new_for_account(account)?))
}

/// One grok CLI login on this machine.
pub struct Grok {
    tidemark_account: AccountId,
    client: reqwest::Client,
    login: PathBuf,
    /// The CLI proxy host, kept as a field so a test can point it at a loopback.
    proxy_base: String,
}

impl Grok {
    /// Builds the account at the CLI's own login path.
    pub fn new() -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default())
    }

    fn new_for_account(account_id: AccountId) -> Result<Self, ProviderError> {
        let login = cli_credentials_path().ok_or_else(|| {
            ProviderError::Local("neither GROK_HOME nor HOME names a directory".into())
        })?;
        Ok(Self {
            tidemark_account: account_id.clone(),
            client: super::http::client()?,
            login,
            proxy_base: PROXY_BASE.to_owned(),
        })
    }

    #[cfg(test)]
    fn for_test(home: &Path, base: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: AccountId::default(),
            client: super::http::client()?,
            login: credentials_path(home),
            proxy_base: base.trim_end_matches('/').to_owned(),
        })
    }

    fn billing_url(&self) -> String {
        format!("{}/v1/billing?format=credits", self.proxy_base)
    }

    fn settings_url(&self) -> String {
        format!("{}/v1/settings", self.proxy_base)
    }

    /// Both calls carry the same two headers the CLI itself sends: the login's bearer
    /// token and the marker that says this is the CLI speaking.
    fn get(&self, url: &str, token: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .bearer_auth(token)
            .header("x-xai-token-auth", TOKEN_AUTH)
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let bytes = std::fs::read(&self.login).map_err(login_error)?;
        let document: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            ProviderError::malformed(format!("the Grok login file is not readable: {error}"))
        })?;
        let entry = select_entry(&document)?;
        if let Some(expires_at) = entry.expires_at.as_deref().and_then(instant)
            && Timestamp::now() >= expires_at
        {
            return Err(ProviderError::NoCredential);
        }
        let token = entry.key.as_deref().unwrap_or_default();

        let request = self.get(&self.billing_url(), token)?;
        let (body, _) = super::request_inspected(PROVIDER_ID, &self.client, request, |response| {
            // A dead login is the sign-in's business, not an HTTP error.
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(ProviderError::NoCredential);
            }
            Ok(())
        })
        .await?;
        let billing = parse_billing_for_account(&body, Timestamp::now(), &self.tidemark_account)?;

        // Advisory, as upstream treats it: a refusal here costs the Plan row only.
        let settings_tier = self.settings_tier(token).await;
        let tier = billing.tier.or(settings_tier);

        let mut snapshot = billing.snapshot;
        let mut account = Vec::new();
        if let Some(email) = nonblank(entry.email) {
            account.push(DetailRow {
                label: "Email".to_owned(),
                value: email,
            });
        }
        if let Some(team) = nonblank(entry.team_id) {
            account.push(DetailRow {
                label: "Team".to_owned(),
                value: team,
            });
        }
        if !account.is_empty() {
            snapshot.details.push(DetailSection {
                title: "Account".to_owned(),
                rows: account,
            });
        }
        if let Some(tier) = tier {
            snapshot.details.push(DetailSection {
                title: DetailSection::PLAN.to_owned(),
                rows: vec![DetailRow {
                    label: "Tier".to_owned(),
                    value: tier,
                }],
            });
        }
        Ok(snapshot)
    }

    /// The tier the settings envelope names, when it names one readably.
    async fn settings_tier(&self, token: &str) -> Option<String> {
        let request = self.get(&self.settings_url(), token).ok()?;
        let body = super::request(PROVIDER_ID, &self.client, request)
            .await
            .ok()?;
        parse_settings(&body)
    }
}

impl fmt::Debug for Grok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grok")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Grok {
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

/// Where the grok CLI keeps its login: `$GROK_HOME/auth.json` when `$GROK_HOME` names a
/// directory, `~/.grok/auth.json` when `$HOME` names an absolute one, `None` otherwise.
///
/// Free-standing rather than a method so that a caller can ask whether the CLI's login
/// exists on this machine without building the provider.
pub fn cli_credentials_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("GROK_HOME") {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            return Some(credentials_path(&home));
        }
    }
    let home = crate::paths::home()?;
    Some(credentials_path(&home))
}

fn credentials_path(home: &Path) -> PathBuf {
    home.join(".grok/auth.json")
}

/// One scope's entry in the login map, in the fields this card reads. Everything else the
/// CLI writes — refresh token, names, OIDC bookkeeping — stays unread and unwritten.
#[derive(Debug, Default, Deserialize)]
struct Entry {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

/// The login the CLI means: the SuperGrok OIDC entry when it carries a usable key, else
/// the legacy session entry. An entry without a key — a stale or partial record — is
/// never allowed to shadow a healthy one, and a map with neither is no credential.
fn select_entry(document: &serde_json::Value) -> Result<Entry, ProviderError> {
    let scopes = document
        .as_object()
        .ok_or_else(|| ProviderError::malformed("the Grok login file is not a map of scopes"))?;
    let mut oidc = None;
    let mut legacy = None;
    for (scope, value) in scopes {
        let Ok(entry) = serde_json::from_value::<Entry>(value.clone()) else {
            continue;
        };
        if entry
            .key
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            continue;
        }
        if scope.starts_with(OIDC_SCOPE_PREFIX) {
            oidc = Some(entry);
        } else if scope == LEGACY_SCOPE || scope.contains("/sign-in") {
            legacy = Some(entry);
        }
    }
    oidc.or(legacy).ok_or(ProviderError::NoCredential)
}

/// What one billing exchange produced: the card's window and the tier the body itself
/// named, if any — the settings read only fills in when the body did not.
#[derive(Debug)]
pub struct Billing {
    pub snapshot: Snapshot,
    pub tier: Option<String>,
}

/// Turns a billing body into the one Credits window: the included-allowance percent when
/// the body states it, else the on-demand use against its cap. A body with neither is
/// malformed — upstream carries it as an unknown, but a card cannot be drawn unnumbered.
pub fn parse_billing(body: &str, captured_at: Timestamp) -> Result<Billing, ProviderError> {
    parse_billing_for_account(body, captured_at, &AccountId::default())
}

fn parse_billing_for_account(
    body: &str,
    captured_at: Timestamp,
    account_id: &AccountId,
) -> Result<Billing, ProviderError> {
    let response: BillingResponse = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("not a Grok billing response: {error}"))
    })?;
    let config = response
        .config
        .ok_or_else(|| ProviderError::malformed("the Grok billing response named no config"))?;

    let resets_at = config
        .current_period
        .and_then(|period| period.end)
        .or(config.billing_period_end)
        .as_deref()
        .and_then(instant);

    let used_percent = if let Some(percent) = config
        .credit_usage_percent
        .filter(|percent| percent.is_finite())
    {
        Some(percent.clamp(0.0, 100.0))
    } else {
        match (
            config.on_demand_cap.and_then(|amount| amount.val),
            config.on_demand_used.and_then(|amount| amount.val),
        ) {
            (Some(cap), Some(used)) if cap > 0.0 => Some((used / cap * 100.0).clamp(0.0, 100.0)),
            _ => None,
        }
    }
    .ok_or_else(|| ProviderError::malformed("the Grok billing response named no usage figure"))?;

    let tier = plan_display(
        config
            .subscription_tier
            .as_deref()
            .or(response.subscription_tier.as_deref()),
    );

    Ok(Billing {
        snapshot: Snapshot {
            provider: ProviderId::new(PROVIDER_ID),
            account: account_id.clone(),
            captured_at,
            windows: vec![Window {
                key: WindowKey::named("credits"),
                title: "Credits".to_owned(),
                subtitle: None,
                used_percent,
                resets_at,
                length: None,
            }],
            details: Vec::new(),
        },
        tier,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingResponse {
    #[serde(default)]
    config: Option<CreditsConfig>,
    #[serde(default)]
    subscription_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditsConfig {
    #[serde(default)]
    credit_usage_percent: Option<f64>,
    #[serde(default)]
    current_period: Option<CurrentPeriod>,
    #[serde(default)]
    billing_period_end: Option<String>,
    #[serde(default)]
    on_demand_cap: Option<CreditsAmount>,
    #[serde(default)]
    on_demand_used: Option<CreditsAmount>,
    #[serde(default)]
    subscription_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CurrentPeriod {
    #[serde(default)]
    end: Option<String>,
}

/// The proxy reports amounts as `{ "val": <number> }`; a fractional value decodes like a
/// whole one, so an unusual cap/used shape cannot fail an otherwise valid response.
#[derive(Debug, Deserialize)]
struct CreditsAmount {
    #[serde(default)]
    val: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SettingsResponse {
    #[serde(default)]
    subscription_tier_display: Option<String>,
}

/// The tier the settings envelope names, written the way the plan writes it. A body that
/// says nothing readable names no tier — never an error on an advisory read.
pub fn parse_settings(body: &str) -> Option<String> {
    let response: SettingsResponse = serde_json::from_str(body).ok()?;
    plan_display(response.subscription_tier_display.as_deref())
}

/// The consumer plan labels, upstream's mapping: a coded `SUPERGROK_HEAVY` and a bare
/// `heavy` alike are the plan whose name people know; anything else is shown as sent.
fn plan_display(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let compact: String = trimmed
        .to_lowercase()
        .chars()
        .filter(|letter| letter.is_alphabetic())
        .collect();
    match compact.as_str() {
        "supergrokheavy" | "heavy" => Some("SuperGrok Heavy".to_owned()),
        "supergrok" => Some("SuperGrok".to_owned()),
        _ => Some(trimmed.to_owned()),
    }
}

/// A reset instant as the billing body states it, RFC 3339 with optional fraction.
fn instant(raw: &str) -> Option<Timestamp> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(|time| Timestamp::from_unix(time.unix_timestamp()).ok())
}

/// A login file the machine has is `NoCredential` when it is simply not there; anything
/// else about the read is local trouble, not the sign-in's.
fn login_error(error: std::io::Error) -> ProviderError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProviderError::NoCredential
    } else {
        ProviderError::Local(error.to_string())
    }
}

fn nonblank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;
    use serde_json::json;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use tidemark_types::{Timestamp, WindowKey};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// The recorded `/v1/billing?format=credits` body of
    /// `GrokCreditsProxyFetcherTests`: a percent, a weekly period, an on-demand pair.
    const BILLING: &str = include_str!("../../../tests/fixtures/grok/billing.json");
    /// The recorded `/v1/settings` body shape of `GrokPlanTests`.
    const SETTINGS: &str = include_str!("../../../tests/fixtures/grok/settings.json");

    struct TestHome {
        dir: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tidemark-grok-test-{}-{serial}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join(".grok")).expect("test directory");
            Self { dir }
        }

        fn path(&self) -> &Path {
            &self.dir
        }

        fn write_login(&self, document: serde_json::Value) {
            fs::write(self.path().join(".grok/auth.json"), document.to_string())
                .expect("write login");
        }

        fn document(&self) -> serde_json::Value {
            serde_json::from_str(
                &fs::read_to_string(self.path().join(".grok/auth.json")).expect("login readable"),
            )
            .expect("login is JSON")
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A signed-in OIDC entry in the shape `GrokAuthTests` records: the expiry far enough
    /// out that the token spends without a refresh, the email and team the probe labels
    /// the card with.
    fn live_login() -> serde_json::Value {
        json!({
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                "key": "fresh-token",
                "auth_mode": "oidc",
                "email": "user@example.com",
                "team_id": "team-uuid",
                "expires_at": "2099-01-01T00:00:00Z",
            }
        })
    }

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

    fn fetch(provider: &Grok) -> Result<Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    #[test]
    fn the_recorded_billing_draws_the_credits_window_at_the_stated_percent() {
        let billing = parse_billing(BILLING, at(1_786_579_200)).expect("parses the response");

        assert_eq!(billing.tier, None);
        let windows = &billing.snapshot.windows;
        assert_eq!(windows.len(), 1);
        let credits = &windows[0];
        assert_eq!(credits.key, WindowKey::named("credits"));
        assert_eq!(credits.title, "Credits");
        assert_eq!(credits.length, None);
        assert!((credits.used_percent - 12.5).abs() < 0.000_001);
        assert_eq!(credits.resets_at, Some(at(1_786_579_200)));
    }

    #[test]
    fn an_on_demand_cap_and_usage_become_the_percent() {
        let billing = parse_billing(
            r#"{
                "config": {
                    "onDemandCap": { "val": 1000.0 },
                    "onDemandUsed": { "val": 250.5 }
                }
            }"#,
            at(1_786_579_200),
        )
        .expect("parses the response");

        assert_eq!(billing.snapshot.windows[0].resets_at, None);
        assert!((billing.snapshot.windows[0].used_percent - 25.05).abs() < 0.000_001);
    }

    #[test]
    fn an_out_of_range_percent_is_clamped_into_the_card() {
        let over = parse_billing(
            r#"{
                "config": {
                    "creditUsagePercent": 104.2,
                    "billingPeriodEnd": "2026-08-13T00:00:00Z"
                }
            }"#,
            at(1_786_579_200),
        )
        .expect("parses the response");
        let under = parse_billing(
            r#"{ "config": { "creditUsagePercent": -3.5 } }"#,
            at(1_786_579_200),
        )
        .expect("parses the response");

        assert!((over.snapshot.windows[0].used_percent - 100.0).abs() < 0.000_001);
        assert_eq!(over.snapshot.windows[0].resets_at, Some(at(1_786_579_200)));
        assert!((under.snapshot.windows[0].used_percent).abs() < 0.000_001);
    }

    #[test]
    fn the_config_tier_is_preferred_over_the_envelope_tier() {
        let billing = parse_billing(
            r#"{
                "config": {
                    "creditUsagePercent": 8,
                    "billingPeriodEnd": "2026-08-13T00:00:00Z",
                    "subscriptionTier": "SuperGrok Heavy"
                },
                "subscriptionTier": "SuperGrok"
            }"#,
            at(1_786_579_200),
        )
        .expect("parses the response");

        assert_eq!(billing.tier.as_deref(), Some("SuperGrok Heavy"));
        assert!((billing.snapshot.windows[0].used_percent - 8.0).abs() < 0.000_001);
    }

    #[test]
    fn a_coded_tier_is_named_the_way_the_plan_writes_it() {
        let billing = parse_billing(
            r#"{
                "config": {
                    "subscriptionTier": "SUPERGROK_HEAVY",
                    "creditUsagePercent": 42,
                    "currentPeriod": { "end": "2026-08-13T00:00:00Z" }
                }
            }"#,
            at(1_786_579_200),
        )
        .expect("parses the response");

        assert_eq!(billing.tier.as_deref(), Some("SuperGrok Heavy"));
        assert!((billing.snapshot.windows[0].used_percent - 42.0).abs() < 0.000_001);
        assert_eq!(
            billing.snapshot.windows[0].resets_at,
            Some(at(1_786_579_200))
        );
    }

    #[test]
    fn a_response_without_a_usable_usage_figure_is_malformed() {
        for body in [
            r#"{"config":{}}"#,
            // A tier alone draws nothing: the cap is zero, so no on-demand figure either.
            r#"{"config":{ "onDemandCap": { "val": 0 } },"subscriptionTier":"supergrok_heavy"}"#,
            // Upstream keeps a period without usage as an unknown; a card cannot.
            r#"{"config":{ "currentPeriod": { "end": "2026-08-13T00:00:00Z" } }}"#,
        ] {
            let error = parse_billing(body, at(1_786_579_200)).expect_err("nothing to draw");
            assert!(
                matches!(error, ProviderError::Malformed { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn the_settings_tier_display_names_the_plan_upstreams_way() {
        assert_eq!(parse_settings(SETTINGS).as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(
            parse_settings(r#"{"subscription_tier_display":"supergrok"}"#).as_deref(),
            Some("SuperGrok")
        );
        assert_eq!(
            parse_settings(r#"{"subscription_tier_display":"  HEAVY  "}"#).as_deref(),
            Some("SuperGrok Heavy")
        );
        assert_eq!(
            parse_settings(r#"{"subscription_tier_display":"Custom Team"}"#).as_deref(),
            Some("Custom Team")
        );
        assert_eq!(
            parse_settings(r#"{"subscription_tier_display":"   "}"#),
            None
        );
        assert_eq!(parse_settings(r#"{}"#), None);
        assert_eq!(parse_settings("not-json"), None);
    }

    #[test]
    fn the_credentials_file_is_the_grok_directory_of_the_given_home() {
        assert_eq!(
            credentials_path(Path::new("/home/herald")),
            PathBuf::from("/home/herald/.grok/auth.json")
        );
    }

    #[test]
    fn the_full_fetch_reads_the_login_draws_the_window_and_names_the_plan() {
        let home = TestHome::new();
        home.write_login(live_login());
        let (base, requests, server) = chained_server(vec![
            route("GET /v1/billing?format=credits", 200, BILLING),
            route("GET /v1/settings", 200, SETTINGS),
        ]);
        let provider = Grok::for_test(home.path(), &base).expect("builds");

        let snapshot = fetch(&provider).expect("fetches");
        server.join().expect("server exits");

        let billing_request = requests.recv().expect("billing request");
        assert!(billing_request.contains("authorization: Bearer fresh-token"));
        assert!(billing_request.contains("x-xai-token-auth: xai-grok-cli"));
        assert!(billing_request.contains("accept: application/json"));
        assert!(
            requests
                .recv()
                .expect("settings request")
                .starts_with("GET /v1/settings")
        );

        assert_eq!(snapshot.windows.len(), 1);
        assert!((snapshot.windows[0].used_percent - 12.5).abs() < 0.000_001);

        assert_eq!(snapshot.details.len(), 2);
        let account = &snapshot.details[0];
        assert_eq!(account.title, "Account");
        assert_eq!(account.rows[0].label, "Email");
        assert_eq!(account.rows[0].value, "user@example.com");
        assert_eq!(account.rows[1].label, "Team");
        assert_eq!(account.rows[1].value, "team-uuid");
        let plan = &snapshot.details[1];
        assert_eq!(plan.title, DetailSection::PLAN);
        // The billing fixture names no tier; the settings read is what names the plan.
        assert_eq!(plan.rows[0].label, "Tier");
        assert_eq!(plan.rows[0].value, "SuperGrok Heavy");

        assert_eq!(
            home.document()["https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828"]["key"],
            "fresh-token",
            "the login file is never written"
        );
    }

    #[test]
    fn the_oidc_scope_entry_wins_over_the_legacy_sign_in() {
        let home = TestHome::new();
        home.write_login(json!({
            "https://accounts.x.ai/sign-in": {
                "key": "legacy-should-not-win",
                "auth_mode": "session"
            },
            "https://auth.x.ai::client-id": {
                "key": "oidc-wins",
                "auth_mode": "oidc",
                "email": "preferred@example.com"
            }
        }));
        let (base, requests, server) = chained_server(vec![
            route(
                "GET /v1/billing?format=credits",
                200,
                r#"{"config":{"creditUsagePercent":1}}"#,
            ),
            route("GET /v1/settings", 200, r#"{}"#),
        ]);
        let provider = Grok::for_test(home.path(), &base).expect("builds");

        let snapshot = fetch(&provider).expect("fetches");
        server.join().expect("server exits");

        assert!(
            requests
                .recv()
                .expect("billing request")
                .contains("authorization: Bearer oidc-wins")
        );
        let account = &snapshot.details[0];
        assert_eq!(account.rows[0].value, "preferred@example.com");
    }

    #[test]
    fn a_stale_oidc_record_does_not_shadow_a_healthy_legacy_session() {
        let home = TestHome::new();
        home.write_login(json!({
            "https://auth.x.ai::stale-client": {
                "auth_mode": "oidc",
                "email": "stale@example.com"
            },
            "https://accounts.x.ai/sign-in": {
                "key": "healthy-legacy-token",
                "auth_mode": "session",
                "email": "healthy@example.com"
            }
        }));
        let (base, requests, server) = chained_server(vec![
            route(
                "GET /v1/billing?format=credits",
                200,
                r#"{"config":{"creditUsagePercent":1}}"#,
            ),
            route("GET /v1/settings", 200, r#"{}"#),
        ]);
        let provider = Grok::for_test(home.path(), &base).expect("builds");

        fetch(&provider).expect("fetches");
        server.join().expect("server exits");

        assert!(
            requests
                .recv()
                .expect("billing request")
                .contains("authorization: Bearer healthy-legacy-token")
        );
    }

    #[test]
    fn an_expired_login_is_no_credential_before_any_request() {
        let home = TestHome::new();
        home.write_login(json!({
            "https://auth.x.ai::client": {
                "key": "stale-token",
                "expires_at": "2020-01-01T00:00:00Z"
            }
        }));
        // Port 9 has nothing listening: were a request attempted, the error could not be
        // `NoCredential`.
        let provider = Grok::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

        let error = fetch(&provider).expect_err("the login has expired");

        assert!(matches!(error, ProviderError::NoCredential), "{error:?}");
    }

    #[test]
    fn a_login_that_names_no_usable_entry_is_no_credential() {
        let home = TestHome::new();
        home.write_login(json!({
            "https://auth.x.ai::abc": { "auth_mode": "oidc" }
        }));
        let provider = Grok::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

        let error = fetch(&provider).expect_err("no entry carries a key");

        assert!(matches!(error, ProviderError::NoCredential), "{error:?}");
    }

    #[test]
    fn a_billing_call_rejected_as_unauthorized_is_no_credential() {
        let home = TestHome::new();
        home.write_login(live_login());
        let (base, _requests, server) = chained_server(vec![route(
            "GET /v1/billing?format=credits",
            401,
            "unauthorized",
        )]);
        let provider = Grok::for_test(home.path(), &base).expect("builds");

        let result = fetch(&provider);
        server.join().expect("server exits");

        assert!(
            matches!(result, Err(ProviderError::NoCredential)),
            "{result:?}"
        );
    }

    #[test]
    fn a_settings_refusal_costs_only_the_tier() {
        let home = TestHome::new();
        home.write_login(live_login());
        let (base, _requests, server) = chained_server(vec![
            route("GET /v1/billing?format=credits", 200, BILLING),
            route("GET /v1/settings", 500, "nope"),
        ]);
        let provider = Grok::for_test(home.path(), &base).expect("builds");

        let snapshot = fetch(&provider).expect("fetches");
        server.join().expect("server exits");

        assert_eq!(snapshot.windows.len(), 1);
        assert!((snapshot.windows[0].used_percent - 12.5).abs() < 0.000_001);
        assert!(
            snapshot
                .details
                .iter()
                .all(|section| section.title != DetailSection::PLAN),
            "no tier was learned, so no Plan row: {:?}",
            snapshot.details
        );
    }

    #[test]
    fn a_home_without_a_login_file_is_no_credential() {
        let home = TestHome::new();
        let provider = Grok::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

        let error = fetch(&provider).expect_err("the CLI has never signed in here");

        assert!(matches!(error, ProviderError::NoCredential), "{error:?}");
    }

    #[test]
    fn an_unreadable_login_file_is_malformed() {
        for document in ["not-json", "[]"] {
            let home = TestHome::new();
            fs::write(home.path().join(".grok/auth.json"), document).expect("write login");
            let provider = Grok::for_test(home.path(), "http://127.0.0.1:9").expect("builds");

            let error = fetch(&provider).expect_err("nothing readable in the file");

            assert!(
                matches!(error, ProviderError::Malformed { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_pasted_xai_key_is_refused_at_build() {
        let error = (SPEC.build)(
            AccountId::default(),
            Credential::new("xai-abc123"),
            &Options::new(),
        )
        .expect_err("that key belongs to another provider");

        match error {
            ProviderError::Local(message) => {
                assert!(message.contains("x.ai"), "{message}");
                assert!(message.contains("xai"), "{message}");
            }
            other => panic!("expected Local, got {other:?}"),
        }
    }
}
