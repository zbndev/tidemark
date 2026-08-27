//! Augment usage units, read from the browser session that signs in to its dashboard.
//!
//! The credits call carries the quota; the subscription call only adds the plan name,
//! the account email and the billing-cycle reset, so when it fails the window is still
//! drawn without them. A browser session is selected explicitly, never substituted from
//! another profile.

use super::{HandSpec, Options, ProviderError, http, redact_query, session};
use crate::browser::{self, Keyring, SafeStorage, auth::Selection};
use crate::providers::{BoxFuture, Credential, Provider};
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tidemark_types::{
    AccountId, AuthCandidate, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot,
    Timestamp, Window, WindowKey,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "augment";

const CREDITS_URL: &str = "https://app.augmentcode.com/api/credits";
const SUBSCRIPTION_URL: &str = "https://app.augmentcode.com/api/subscription";
const SESSION_URL: &str = "https://app.augmentcode.com/";
const SESSION_COOKIE_NAMES: &[&str] = &[
    "session",
    "_session",
    "web_rpc_proxy_session",
    "__Secure-next-auth.session-token",
    "next-auth.session-token",
    "__Secure-authjs.session-token",
    "authjs.session-token",
];
const COOKIE_DOMAINS: &[&str] = &["augmentcode.com", "app.augmentcode.com"];

/// Augment as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Augment",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in augmentcode.com browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Augment::new(options)?))
}

/// One Augment account, authenticated by one explicitly chosen browser profile.
pub struct Augment {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Augment {
    pub fn new(options: &Options) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            home: std::env::var_os("HOME").map(PathBuf::from),
            storage: Arc::new(Keyring),
            selection: session::selection(options),
            #[cfg(test)]
            base_url: None,
        })
    }

    #[cfg(test)]
    fn for_test(
        home: &Path,
        storage: Arc<dyn SafeStorage>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: http::client()?,
            home: Some(home.to_path_buf()),
            storage,
            selection: Some(Selection {
                browser: "firefox".into(),
                profile: None,
            }),
            base_url: Some(base_url.trim_end_matches('/').to_owned()),
        })
    }

    fn credits_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/api/credits");
        }
        CREDITS_URL.to_owned()
    }

    fn subscription_url(&self) -> String {
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return format!("{base_url}/api/subscription");
        }
        SUBSCRIPTION_URL.to_owned()
    }

    fn request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::COOKIE, cookie)
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        let selection = self.selection.as_ref().ok_or(ProviderError::NoCredential)?;
        let session = session::session(
            self.home.as_deref(),
            self.storage.as_ref(),
            selection,
            SESSION_COOKIE_NAMES,
            &cookie_query(),
            SESSION_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let (credits, subscription) = tokio::join!(
            super::request(
                PROVIDER_ID,
                &self.client,
                self.request(&self.credits_url(), &session.header)?,
            ),
            super::request(
                PROVIDER_ID,
                &self.client,
                self.request(&self.subscription_url(), &session.header)?,
            ),
        );
        parse(&credits?, subscription.ok().as_deref(), Timestamp::now())
    }

    async fn validate_header(&self, header: &str) -> crate::browser::auth::Validation {
        let Ok(request) = self.request(CREDITS_URL, header) else {
            return crate::browser::auth::Validation::Unreachable;
        };
        match super::validate(&self.client, request).await {
            Ok(()) => crate::browser::auth::Validation::Ready,
            Err(ProviderError::Credential { status: 401 | 403 }) => {
                crate::browser::auth::Validation::Rejected
            }
            Err(_) => crate::browser::auth::Validation::Unreachable,
        }
    }

    async fn inspect_sources(&self) -> Vec<AuthCandidate> {
        session::inspect_sources(
            self.home.as_deref(),
            self.storage.as_ref(),
            &cookie_query(),
            CREDITS_URL,
            |credential| async move { self.validate_header(credential.header()).await },
        )
        .await
    }
}

impl fmt::Debug for Augment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Augment")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Augment {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }

    fn inspect_auth_sources(&self) -> BoxFuture<'_, Result<Vec<AuthCandidate>, ProviderError>> {
        Box::pin(async { Ok(self.inspect_sources().await) })
    }
}

fn cookie_query() -> browser::Query {
    browser::Query::new(COOKIE_DOMAINS.iter().copied(), Vec::<String>::new())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credits {
    usage_units_remaining: Option<f64>,
    usage_units_consumed_this_billing_cycle: Option<f64>,
    usage_units_available: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    plan_name: Option<String>,
    billing_period_end: Option<String>,
    email: Option<String>,
}

/// Turns Augment's credits and subscription responses into the usage-units window.
///
/// The subscription body is supplementary on purpose: whatever is wrong with it costs
/// only the reset and the plan rows, never the quota reading. The limit is
/// `usageUnitsAvailable` when it is positive, and the remaining-plus-consumed sum
/// otherwise — a plan that reports no separate availability still draws its window.
pub fn parse(
    credits_body: &str,
    subscription_body: Option<&str>,
    captured_at: Timestamp,
) -> Result<Snapshot, ProviderError> {
    let credits: Credits = serde_json::from_str(credits_body)
        .map_err(|error| ProviderError::malformed(format!("not Augment credits: {error}")))?;
    let limit = match credits
        .usage_units_available
        .filter(|available| *available > 0.0)
    {
        Some(available) => available,
        None => match (
            credits.usage_units_remaining,
            credits.usage_units_consumed_this_billing_cycle,
        ) {
            (Some(remaining), Some(consumed)) => remaining + consumed,
            _ => {
                return Err(ProviderError::malformed(
                    "Augment usage-units limit cannot be read",
                ));
            }
        },
    };
    let used = match (
        credits.usage_units_consumed_this_billing_cycle,
        credits.usage_units_remaining,
    ) {
        (Some(consumed), _) => consumed,
        (None, Some(remaining)) => limit - remaining,
        (None, None) => {
            return Err(ProviderError::malformed(
                "Augment usage-units usage cannot be read",
            ));
        }
    };

    let subscription =
        subscription_body.and_then(|body| serde_json::from_str::<Subscription>(body).ok());
    let resets_at = subscription
        .as_ref()
        .and_then(|subscription| subscription.billing_period_end.as_deref())
        .and_then(billing_date);

    let windows = vec![Window {
        key: WindowKey::named("units"),
        title: "Usage units".to_owned(),
        subtitle: Some(format!(
            "{} / {} units",
            number_text(used),
            number_text(limit)
        )),
        used_percent: if limit > 0.0 {
            (used / limit * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        },
        resets_at,
        length: None,
    }];

    let mut rows = Vec::new();
    if let Some(plan) = subscription
        .as_ref()
        .and_then(|subscription| subscription.plan_name.as_deref())
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
    {
        rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: plan.to_owned(),
        });
    }
    if let Some(email) = subscription
        .as_ref()
        .and_then(|subscription| subscription.email.as_deref())
        .map(str::trim)
        .filter(|email| !email.is_empty())
    {
        rows.push(DetailRow {
            label: "Email".to_owned(),
            value: email.to_owned(),
        });
    }
    let details = if rows.is_empty() {
        Vec::new()
    } else {
        vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows,
        }]
    };

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details,
    })
}

fn billing_date(value: &str) -> Option<Timestamp> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    Timestamp::from_unix(parsed.unix_timestamp()).ok()
}

fn number_text(value: f64) -> String {
    let digits = format!("{:.0}", value.round());
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::{Augment, parse};
    use crate::browser::SafeStorage;
    use crate::providers::{Provider, ProviderError};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use tidemark_types::{Timestamp, WindowKey};

    #[derive(Debug)]
    struct NoKeyring;

    impl SafeStorage for NoKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn gecko_home() -> crate::browser::tests::TestHome {
        let home = crate::browser::tests::TestHome::new();
        let connection = Connection::open(home.gecko(".mozilla/firefox/Default")).expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                    isSecure INTEGER, isHttpOnly INTEGER
                );",
            )
            .expect("creates");
        connection
            .execute(
                "INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('.augmentcode.com', 'session', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    /// A loopback server that answers the two Augment routes, one connection each. The
    /// requests are sent concurrently, so their order in the channel is not asserted on.
    fn two_route_server(
        credits: (u16, &'static str),
        subscription: (u16, &'static str),
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
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
                let is_credits = request.starts_with("GET /api/credits");
                request_tx.send(request).expect("sends request");
                let (status, body) = if is_credits { credits } else { subscription };
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

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Augment {
        Augment::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Augment) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    const CREDITS: &str = include_str!("../../../tests/fixtures/augment/credits.json");
    const SUBSCRIPTION: &str = include_str!("../../../tests/fixtures/augment/subscription.json");

    #[test]
    fn a_recorded_credits_response_draws_the_usage_units_window_with_its_subscription() {
        // Reading remaining as used would invert the quota bar.
        let snapshot = parse(
            CREDITS,
            Some(SUBSCRIPTION),
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded bodies");
        let units = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("units"))
            .expect("usage units window");

        assert!((units.used_percent - 10.0).abs() < 0.01);
        assert_eq!(
            units.resets_at,
            Some(Timestamp::from_unix(1_788_220_800).expect("plausible"))
        );
        assert_eq!(units.subtitle.as_deref(), Some("10 / 100 units"));
        assert_eq!(snapshot.details[0].rows[0].value, "Developer Pro");
        assert_eq!(snapshot.details[0].rows[1].value, "dev@example.com");
    }

    #[test]
    fn a_missing_available_field_falls_back_to_remaining_plus_consumed() {
        // Without the fallback a plan that reports no separate availability would draw nothing.
        let body = r#"{
          "usageUnitsRemaining": 15,
          "usageUnitsConsumedThisBillingCycle": 10
        }"#;
        let snapshot = parse(
            body,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the fallback body");
        let units = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("units"))
            .expect("usage units window");

        assert!((units.used_percent - 40.0).abs() < 0.01);
        assert_eq!(units.subtitle.as_deref(), Some("10 / 25 units"));
    }

    #[test]
    fn subscription_info_is_supplementary_and_costs_only_its_reset_and_plan() {
        // Failing the whole fetch over the optional call would hide a healthy quota reading.
        let snapshot = parse(
            CREDITS,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");
        let units = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("units"))
            .expect("usage units window");

        assert!((units.used_percent - 10.0).abs() < 0.01);
        assert_eq!(units.resets_at, None);
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn a_credits_body_whose_limit_cannot_be_read_is_malformed() {
        // Accepting a status-only body would paint a made-up zero quota.
        let result = parse(
            r#"{"usageBalanceStatus":"ok"}"#,
            None,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        );

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }

    #[test]
    fn both_requests_carry_the_chosen_browsers_session_cookie() {
        // Either request without the session cookie answers 401 instead of quota.
        let home = gecko_home();
        let (base_url, requests, server) = two_route_server((200, CREDITS), (200, SUBSCRIPTION));
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the credits");
        let requests: Vec<String> = (0..2)
            .map(|_| {
                requests
                    .recv()
                    .expect("request captured")
                    .to_ascii_lowercase()
            })
            .collect();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "augment");
        let credits = requests
            .iter()
            .find(|request| request.starts_with("get /api/credits"))
            .expect("credits request");
        let subscription = requests
            .iter()
            .find(|request| request.starts_with("get /api/subscription"))
            .expect("subscription request");
        for request in [credits.as_str(), subscription.as_str()] {
            assert!(
                request.contains("cookie: session=chosen-session"),
                "{request}"
            );
            assert!(request.contains("accept: application/json"), "{request}");
        }
    }

    #[test]
    fn an_unauthorised_credits_response_asks_for_a_new_browser_session() {
        // Mapping 401 to a transient error would retry forever instead of showing the selected session expired.
        let home = gecko_home();
        let (base_url, _requests, server) = two_route_server((401, "{}"), (401, "{}"));
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }
}
