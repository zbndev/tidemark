//! The transport every key-authenticated provider shares, and the description of how
//! they differ.
//!
//! # Why a table and not a module each
//!
//! The first five providers were written one module apiece, because each of the five
//! acquires its credential differently and no two of them agreed on enough to share. The
//! providers in this module agree on almost everything: the user pastes a key, the key
//! goes on one request, and the response is JSON. What is left that differs is an
//! endpoint, where the key sits, and what the body means — which is exactly the fields of
//! [`Spec`].
//!
//! # The rule this module makes structural
//!
//! `providers::mod` states that transport and meaning are separate functions. Here that
//! stops being a convention: [`Spec::parse`] is a plain `fn` with no client, no
//! credential and no `async` in its signature. A parser physically cannot make a request,
//! so every trap in a response is reachable from a test that needs no network.

use super::{BoxFuture, Credential, Provider, ProviderError, http};
use std::collections::BTreeMap;
use std::fmt;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp};

/// The settings of one account, as `config.toml` holds them.
pub type Options = BTreeMap<String, String>;

/// Where the key goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// A header the provider names itself — `x-api-key`, `api-key`.
    Header(&'static str),
    /// A query parameter. Rare, and always worse: it reaches access logs.
    Query(&'static str),
}

/// How the request is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The common case.
    Get,
    /// A fixed body, for the providers whose usage endpoint is a GraphQL or analytics
    /// POST that takes no per-request input.
    Post {
        /// Sent verbatim.
        body: &'static str,
        /// `Content-Type` for the body.
        content_type: &'static str,
    },
}

/// One setting a provider lets the user choose, published so that the interface can draw
/// the control without knowing what the setting means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionSchema {
    /// Key under `[provider.<slug>]` in `config.toml`.
    pub name: &'static str,
    /// What to call it on screen.
    pub title: &'static str,
    /// A sentence under the control, when it needs one.
    pub description: Option<&'static str>,
    /// What it is when the user has not chosen.
    pub default: &'static str,
    /// `(value, label)` pairs. Empty means free text, such as a base URL.
    pub choices: &'static [(&'static str, &'static str)],
}

/// Everything a key-authenticated provider is.
#[derive(Debug)]
pub struct Spec {
    /// The stable slug this provider's history is filed under. Never changes once shipped.
    pub id: &'static str,
    /// What to call it in front of a person.
    pub title: &'static str,
    /// The URL to poll, given the account's settings.
    pub endpoint: fn(&Options) -> String,
    /// How the request is made.
    pub method: Method,
    /// Where the key goes.
    pub auth: Auth,
    /// Headers beyond auth and the shared user agent.
    pub headers: &'static [(&'static str, &'static str)],
    /// Turns a response body into a snapshot. Pure by construction.
    pub parse: fn(&str, Timestamp) -> Result<Snapshot, ProviderError>,
    /// One sentence saying which page the key is on.
    pub credential_hint: &'static str,
    /// What the user may choose.
    pub options: &'static [OptionSchema],
}

/// A client for one key against one [`Spec`].
pub struct Keyed {
    spec: &'static Spec,
    client: reqwest::Client,
    credential: Credential,
    url: String,
}

impl Keyed {
    /// Builds a client. The URL is resolved once, here, because a setting that changed
    /// the host would otherwise take effect only on the next daemon restart.
    pub fn new(
        spec: &'static Spec,
        credential: Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            spec,
            client: http::client()?,
            credential,
            url: (spec.endpoint)(options),
        })
    }

    /// The URL this instance polls.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The request this instance would send, built but not sent, so that the placement of
    /// the key is testable without a server.
    pub fn build_request(&self) -> Result<reqwest::Request, ProviderError> {
        let mut builder = match self.spec.method {
            Method::Get => self.client.get(&self.url),
            Method::Post { body, content_type } => self
                .client
                .post(&self.url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body),
        };
        for (name, value) in self.spec.headers {
            builder = builder.header(*name, *value);
        }
        builder = match self.spec.auth {
            Auth::Bearer => builder.bearer_auth(self.credential.expose()),
            Auth::Header(name) => builder.header(name, self.credential.expose()),
            Auth::Query(name) => builder.query(&[(name, self.credential.expose())]),
        };
        builder.build().map_err(ProviderError::Client)
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let response = self
            .client
            .execute(self.build_request()?)
            .await
            .map_err(ProviderError::Transport)?;

        let status = response.status();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(status, retry_after.as_deref())?;

        let body = response.text().await.map_err(ProviderError::Transport)?;
        (self.spec.parse)(&body, Timestamp::now())
    }
}

impl fmt::Debug for Keyed {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyed")
            .field("id", &self.spec.id)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Provider for Keyed {
    fn id(&self) -> ProviderId {
        ProviderId::new(self.spec.id)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// Every key-authenticated provider this build supports, in the order they are shown.
///
/// Adding a provider is a file beside this one and a line here. Nothing else in the
/// workspace names it.
pub static CATALOG: &[&Spec] = &[&super::kimi::SPEC, &super::zai::SPEC];

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn snapshot_of(id: &str, captured_at: Timestamp) -> Snapshot {
        Snapshot {
            provider: ProviderId::new(id),
            account: AccountId::default(),
            captured_at,
            windows: Vec::new(),
            details: Vec::new(),
        }
    }

    fn parse_ok(_body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
        Ok(snapshot_of("test", captured_at))
    }

    static BEARER: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |_| "https://example.invalid/usage".to_owned(),
        method: Method::Get,
        auth: Auth::Bearer,
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[],
    };

    static HEADER: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |_| "https://example.invalid/usage".to_owned(),
        method: Method::Get,
        auth: Auth::Header("x-api-key"),
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[],
    };

    static QUERY: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |_| "https://example.invalid/usage".to_owned(),
        method: Method::Get,
        auth: Auth::Query("key"),
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[],
    };

    static POSTING: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |_| "https://example.invalid/usage".to_owned(),
        method: Method::Post {
            body: "{\"query\":\"usage\"}",
            content_type: "application/json",
        },
        auth: Auth::Bearer,
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[],
    };

    static REGIONAL: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |options| match options.get("region").map(String::as_str) {
            Some("cn") => "https://cn.example.invalid/usage".to_owned(),
            _ => "https://example.invalid/usage".to_owned(),
        },
        method: Method::Get,
        auth: Auth::Bearer,
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[],
    };

    #[test]
    fn a_bearer_key_goes_in_the_authorization_header() {
        let keyed = Keyed::new(&BEARER, Credential::new("sk-1"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .expect("present"),
            "Bearer sk-1"
        );
        assert_eq!(
            request.headers().get("Accept").expect("present"),
            "application/json"
        );
        assert_eq!(request.method(), reqwest::Method::GET);
    }

    #[test]
    fn a_header_key_goes_in_the_header_the_spec_names() {
        let keyed = Keyed::new(&HEADER, Credential::new("sk-2"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(request.headers().get("x-api-key").expect("present"), "sk-2");
        assert!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none(),
            "a header-auth provider must not also be sent a bearer token"
        );
    }

    #[test]
    fn a_query_key_goes_in_the_query_string_and_is_escaped() {
        let keyed = Keyed::new(&QUERY, Credential::new("a b&c"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(request.url().query_pairs().count(), 1);
        assert_eq!(
            request
                .url()
                .query_pairs()
                .find(|(name, _)| name == "key")
                .expect("present")
                .1,
            "a b&c"
        );
    }

    #[test]
    fn a_posting_spec_sends_its_body_and_content_type() {
        let keyed = Keyed::new(&POSTING, Credential::new("sk-3"), &options(&[])).expect("builds");
        let request = keyed.build_request().expect("builds");
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .expect("present"),
            "application/json"
        );
        let body = request
            .body()
            .expect("present")
            .as_bytes()
            .expect("in memory");
        assert_eq!(body, b"{\"query\":\"usage\"}");
    }

    #[test]
    fn the_endpoint_reads_the_accounts_options() {
        let global = Keyed::new(&REGIONAL, Credential::new("sk-4"), &options(&[])).expect("builds");
        let cn = Keyed::new(
            &REGIONAL,
            Credential::new("sk-4"),
            &options(&[("region", "cn")]),
        )
        .expect("builds");
        assert_eq!(global.url(), "https://example.invalid/usage");
        assert_eq!(cn.url(), "https://cn.example.invalid/usage");
    }

    #[tokio::test]
    async fn a_blank_credential_is_refused_before_a_request_is_spent() {
        let keyed = Keyed::new(&BEARER, Credential::new("   "), &options(&[])).expect("builds");
        assert!(matches!(
            keyed.fetch().await,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[test]
    fn the_client_reports_the_specs_identity() {
        let keyed = Keyed::new(&BEARER, Credential::new("sk-5"), &options(&[])).expect("builds");
        assert_eq!(keyed.id(), ProviderId::new("test"));
        assert_eq!(keyed.account(), AccountId::default());
    }

    #[test]
    fn a_keyed_client_never_prints_its_credential() {
        let keyed =
            Keyed::new(&BEARER, Credential::new("sk-super-secret"), &options(&[])).expect("builds");
        let rendered = format!("{keyed:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
