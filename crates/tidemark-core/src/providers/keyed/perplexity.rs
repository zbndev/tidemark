//! Perplexity credits, read from the browser session that signs in to its dashboard.
//!
//! The recorded responses and waterfall order come from CodexBar's Perplexity provider. A
//! browser session is selected explicitly, never substituted from another profile.

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

#[cfg(test)]
use std::path::Path;

/// The stable slug this provider's history is filed under.
pub const PROVIDER_ID: &str = "perplexity";

const CREDITS_URL: &str =
    "https://www.perplexity.ai/rest/billing/credits?version=2.18&source=default";
const ORIGIN: &str = "https://www.perplexity.ai";
const REFERER: &str = "https://www.perplexity.ai/account/usage";
const SESSION_COOKIE_NAMES: &[&str] = &[
    "__Secure-next-auth.session-token",
    "__Secure-authjs.session-token",
    "authjs.session-token",
    "next-auth.session-token",
];
const COOKIE_DOMAINS: &[&str] = &["perplexity.ai", "www.perplexity.ai"];

/// Perplexity as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Perplexity",
    credential: CredentialKind::External,
    credential_hint: "Choose a signed-in perplexity.ai browser session.",
    options: session::OPTIONS,
    build,
};

fn build(_credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Perplexity::new(options)?))
}

/// One Perplexity account, authenticated by one explicitly chosen browser profile.
pub struct Perplexity {
    client: reqwest::Client,
    home: Option<PathBuf>,
    storage: Arc<dyn SafeStorage>,
    selection: Option<Selection>,
    #[cfg(test)]
    base_url: Option<String>,
}

impl Perplexity {
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
            return format!("{base_url}/rest/billing/credits?version=2.18&source=default");
        }
        CREDITS_URL.to_owned()
    }

    fn request(&self, url: &str, cookie: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::REFERER, REFERER)
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
            CREDITS_URL,
        )
        .await?
        .ok_or(ProviderError::NoCredential)?;
        let body = super::request(
            PROVIDER_ID,
            &self.client,
            self.request(&self.credits_url(), &session.header)?,
        )
        .await?;
        parse(&body, Timestamp::now())
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

impl fmt::Debug for Perplexity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Perplexity")
            .field("id", &PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl Provider for Perplexity {
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
struct Credits {
    balance_cents: f64,
    renewal_date_ts: f64,
    current_period_purchased_cents: f64,
    credit_grants: Vec<Grant>,
    total_usage_cents: f64,
}

#[derive(Debug, Deserialize)]
struct Grant {
    #[serde(rename = "type")]
    kind: String,
    amount_cents: f64,
    #[serde(default)]
    expires_at_ts: Option<f64>,
}

/// Turns Perplexity's credits response into its billing-cycle and promotional windows.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    let credits: Credits = serde_json::from_str(body)
        .map_err(|error| ProviderError::malformed(format!("not Perplexity credits: {error}")))?;
    let cents = [
        ("balance_cents", credits.balance_cents),
        ("renewal_date_ts", credits.renewal_date_ts),
        (
            "current_period_purchased_cents",
            credits.current_period_purchased_cents,
        ),
        ("total_usage_cents", credits.total_usage_cents),
    ];
    for (field, value) in cents {
        if !value.is_finite() {
            return Err(ProviderError::malformed(format!("{field} must be finite")));
        }
    }
    for grant in &credits.credit_grants {
        if !grant.amount_cents.is_finite()
            || grant
                .expires_at_ts
                .is_some_and(|expires| !expires.is_finite())
        {
            return Err(ProviderError::malformed(
                "credit grant amounts must be finite",
            ));
        }
    }

    let recurring_total = positive_sum(
        credits
            .credit_grants
            .iter()
            .filter(|grant| grant.kind == "recurring")
            .map(|grant| grant.amount_cents),
    );
    let promotional_total = positive_sum(
        credits
            .credit_grants
            .iter()
            .filter(|grant| {
                grant.kind == "promotional"
                    && grant
                        .expires_at_ts
                        .is_none_or(|expires| expires > captured_at.as_unix() as f64)
            })
            .map(|grant| grant.amount_cents),
    );
    let purchased_total = positive_sum(
        credits
            .credit_grants
            .iter()
            .filter(|grant| grant.kind == "purchased")
            .map(|grant| grant.amount_cents),
    )
    .max(credits.current_period_purchased_cents.max(0.0));

    let mut remaining = credits.total_usage_cents.max(0.0);
    let recurring_used = remaining.min(recurring_total);
    remaining -= recurring_used;
    let purchased_used = remaining.min(purchased_total);
    remaining -= purchased_used;
    let promotional_used = remaining.min(promotional_total);

    let renewal = Timestamp::from_unix(credits.renewal_date_ts as i64)
        .map_err(|error| ProviderError::malformed(error.to_string()))?;
    let mut windows = Vec::new();
    if recurring_total > 0.0 {
        windows.push(Window {
            key: WindowKey::named("recurring"),
            title: "Recurring credits".to_owned(),
            subtitle: Some(format!(
                "{recurring_used:.0} / {recurring_total:.0} credits"
            )),
            used_percent: percent(recurring_used, recurring_total),
            resets_at: Some(renewal),
            length: None,
        });
    }
    if promotional_total > 0.0 {
        windows.push(Window {
            key: WindowKey::named("promotional"),
            title: "Promotional credits".to_owned(),
            subtitle: Some(format!(
                "{promotional_used:.0} / {promotional_total:.0} credits"
            )),
            used_percent: percent(promotional_used, promotional_total),
            resets_at: None,
            length: None,
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at,
        windows,
        details: vec![DetailSection {
            title: DetailSection::BALANCE.to_owned(),
            rows: vec![
                DetailRow {
                    label: "Balance".to_owned(),
                    value: format!("${:.2}", credits.balance_cents / 100.0),
                },
                DetailRow {
                    label: "Total usage".to_owned(),
                    value: format!("${:.2}", credits.total_usage_cents / 100.0),
                },
            ],
        }],
    })
}

fn positive_sum(values: impl Iterator<Item = f64>) -> f64 {
    values.sum::<f64>().max(0.0)
}

fn percent(used: f64, total: f64) -> f64 {
    (used / total * 100.0).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::{Perplexity, parse};
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
                ) VALUES ('.perplexity.ai', '__Secure-next-auth.session-token', 'chosen-session', '/', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("inserts the session");
        home
    }

    fn server(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
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
            request_tx.send(request).expect("sends request");
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("writes response");
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn provider(home: &crate::browser::tests::TestHome, base_url: &str) -> Perplexity {
        Perplexity::for_test(home.path(), Arc::new(NoKeyring), base_url).expect("builds")
    }

    fn fetch(provider: &Perplexity) -> Result<tidemark_types::Snapshot, ProviderError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(provider.fetch())
    }

    #[test]
    fn a_full_response_draws_the_recurring_window_and_attributes_usage_by_the_waterfall() {
        // Dividing cents by dollars or skipping the recurring rung would make the bar disagree with the dashboard.
        let at = Timestamp::from_unix(1_740_000_000).expect("plausible");
        let snapshot = parse(
            include_str!("../../../tests/fixtures/perplexity/credits.json"),
            at,
        )
        .expect("parses the recorded body");
        let recurring = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("recurring"))
            .expect("recurring window");
        let promotional = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("promotional"))
            .expect("promotional window");

        assert!((recurring.used_percent - 27.5).abs() < 0.01);
        assert_eq!(
            recurring.resets_at,
            Some(Timestamp::from_unix(1_743_000_000).expect("plausible"))
        );
        assert_eq!(promotional.used_percent, 0.0);
    }

    #[test]
    fn usage_spills_from_recurring_to_purchased_then_promotional_credits() {
        // Reordering the waterfall would attribute paid usage to promotional credit first.
        let body = r#"{
          "balance_cents": 0,
          "renewal_date_ts": 1743000000,
          "current_period_purchased_cents": 3000,
          "credit_grants": [
            { "type": "recurring", "amount_cents": 5000, "expires_at_ts": 1750000000 },
            { "type": "promotional", "amount_cents": 4000, "expires_at_ts": 1750000000 }
          ],
          "total_usage_cents": 9000
        }"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");
        let recurring = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("recurring"))
            .expect("recurring window");
        let promotional = snapshot
            .windows
            .iter()
            .find(|window| window.key == WindowKey::named("promotional"))
            .expect("promotional window");

        assert_eq!(recurring.used_percent, 100.0);
        assert_eq!(promotional.used_percent, 25.0);
    }

    #[test]
    fn an_expired_promotional_grant_is_not_drawn_as_available_credit() {
        // Keeping expired grant credit would show a pool Perplexity no longer lets the account spend.
        let body = r#"{
          "balance_cents": 0,
          "renewal_date_ts": 1743000000,
          "current_period_purchased_cents": 0,
          "credit_grants": [
            { "type": "recurring", "amount_cents": 10000, "expires_at_ts": 1750000000 },
            { "type": "promotional", "amount_cents": 5000, "expires_at_ts": 1700000000 }
          ],
          "total_usage_cents": 1000
        }"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");

        assert!(
            snapshot
                .windows
                .iter()
                .all(|window| window.key != WindowKey::named("promotional"))
        );
    }

    #[test]
    fn an_empty_credit_grants_array_draws_no_invented_windows() {
        // Manufacturing a zero-sized quota would make the UI promise a limit the service did not report.
        let body = r#"{
          "balance_cents": 0,
          "renewal_date_ts": 1743000000,
          "current_period_purchased_cents": 0,
          "credit_grants": [],
          "total_usage_cents": 0
        }"#;
        let snapshot = parse(
            body,
            Timestamp::from_unix(1_740_000_000).expect("plausible"),
        )
        .expect("parses the recorded body");

        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn the_credits_request_carries_the_chosen_browsers_session_cookie() {
        // Dropping any of these browser headers makes Perplexity reject a real dashboard request.
        let home = gecko_home();
        let (base_url, requests, server) = server(
            200,
            include_str!("../../../tests/fixtures/perplexity/credits.json"),
        );
        let snapshot = fetch(&provider(&home, &base_url)).expect("fetches the credits");
        let request = requests
            .recv()
            .expect("request captured")
            .to_ascii_lowercase();
        server.join().expect("server exits");

        assert_eq!(snapshot.provider.as_str(), "perplexity");
        assert!(
            request.starts_with("get /rest/billing/credits?version=2.18&source=default http/1.1")
        );
        assert!(request.contains("cookie: __secure-next-auth.session-token=chosen-session"));
        assert!(request.contains("origin: https://www.perplexity.ai"));
        assert!(request.contains("referer: https://www.perplexity.ai/account/usage"));
    }

    #[test]
    fn an_unauthorised_credits_response_asks_for_a_new_browser_session() {
        // Mapping 401 to a transient error would retry forever instead of showing the selected session expired.
        let home = gecko_home();
        let (base_url, _requests, server) = server(401, "{}");
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(
            result,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn a_credits_body_that_is_not_the_recorded_shape_is_malformed() {
        // Accepting an unrelated successful JSON object would paint made-up empty quota.
        let home = gecko_home();
        let (base_url, _requests, server) = server(200, "{}");
        let result = fetch(&provider(&home, &base_url));
        server.join().expect("server exits");

        assert!(matches!(result, Err(ProviderError::Malformed(_))));
    }
}
