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
//! stops being a convention: [`Spec::parse`] is a plain `fn` whose only inputs are the
//! body and the clock — no client, no credential, no status, no headers in scope — so
//! every trap in a response is reachable from a test that needs no network, and the
//! accidental path to a request from inside a parser does not exist.

pub mod clinepass;
pub mod kimi;
pub mod zai;

use super::{BoxFuture, Credential, Provider, ProviderError, http};
use std::collections::BTreeMap;
use std::fmt;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp};

/// The settings of one account, as `config.toml` holds them.
pub type Options = BTreeMap<String, String>;

/// Where the key goes on the wire.
///
/// A key-derived header — `Basic base64(key:)`, `token <key>` — is not expressible: the
/// key goes in whole, as the provider's own header or bearer token. A provider needing
/// more than that keeps its own `impl Provider` and sends through [`request`], rather
/// than smuggling a derived value into the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// A header the provider names itself — `x-api-key`, `api-key`.
    Header(&'static str),
    /// A query parameter. Rare, and always worse: the key rides the URL into the
    /// provider's access logs. [`redact_query`] keeps it out of ours — no error this
    /// module lets escape carries a query string.
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
    /// True when the account cannot poll without a value — a base URL with no default
    /// host, an account id that is part of the path. [`Keyed::new`] refuses to build with
    /// one unset, saying which setting is missing, rather than letting the endpoint
    /// produce a malformed URL the user would only ever see as `Unreachable`.
    pub required: bool,
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
    ///
    /// A required option left unset fails here, named in the message, so the card says
    /// what is missing instead of the `Unreachable` a malformed URL would produce on
    /// every poll.
    pub fn new(
        spec: &'static Spec,
        credential: Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        for schema in spec.options {
            if schema.required
                && options
                    .get(schema.name)
                    .is_none_or(|value| value.trim().is_empty())
            {
                return Err(ProviderError::Local(format!(
                    "{} is not set for this account",
                    schema.title
                )));
            }
        }
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
        builder
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let body = request(&self.client, self.build_request()?).await?;
        (self.spec.parse)(&body, Timestamp::now())
    }
}

/// Sends one built request and reads its body, mapping every failure the way the keyed
/// providers have agreed to map it.
///
/// A provider whose fetch is not one request — Poe pages through a usage history,
/// OpenRouter makes two calls — keeps its own `impl Provider` and sends each of its
/// requests through here, so the parts that are easy to forget travel with the function
/// rather than with each provider: `Retry-After` is read before the body consumes the
/// headers, a non-success status goes through `http::check`, and the query string is
/// stripped off any `reqwest` error before it can be rendered.
pub async fn request(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<String, ProviderError> {
    let response = client
        .execute(request)
        .await
        .map_err(|error| ProviderError::Transport(redact_query(error)))?;

    let status = response.status();
    let retry_after = http::retry_after_header(&response).map(str::to_owned);
    http::check(status, retry_after.as_deref())?;

    let body = response
        .text()
        .await
        .map_err(|error| ProviderError::Transport(redact_query(error)))?;
    if body.trim().is_empty() {
        // An empty body is its own error rather than serde's "EOF while parsing a value":
        // the one says the provider answered nothing, the other says we read it wrong.
        return Err(ProviderError::malformed(
            "the provider answered an empty body",
        ));
    }
    Ok(body)
}

/// Reads a free-text base-URL option the way every self-hosted provider needs it read:
/// the account's value when one is set, the spec's default otherwise, with any trailing
/// slash trimmed so the endpoint can append its path.
///
/// Refused unless it speaks HTTPS — a key sent over plain HTTP to a remote host is a key
/// given away — except to a loopback host, which is how a self-hosted Ollama is reached.
/// A provider with a friendlier policy, such as falling back to its default host on a bad
/// value, keeps that policy in its own endpoint and calls this for the mechanics.
pub fn base_url(options: &Options, name: &str, default: &str) -> Result<String, ProviderError> {
    let raw = options
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default)
        .trim_end_matches('/');
    if raw.is_empty() {
        return Err(ProviderError::Local(format!(
            "{name} is not set for this account"
        )));
    }
    if let Some(host) = raw.strip_prefix("http://") {
        let host = host.split('/').next().unwrap_or_default();
        let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
        if host != "localhost" && host != "127.0.0.1" {
            return Err(ProviderError::Local(format!(
                "{name} must be an https:// URL; a key over plain HTTP to {host} would be given away"
            )));
        }
    } else if !raw.starts_with("https://") {
        return Err(ProviderError::Local(format!(
            "{name} must be an https:// URL"
        )));
    }
    Ok(raw.to_owned())
}

/// Strips the query string — where [`Auth::Query`] carries the credential — off every
/// `reqwest::Error` before it can be rendered.
///
/// Both `Display` and `Debug` on a `reqwest::Error` print the whole request URL, query
/// included, and these errors reach two places a secret must not: the daemon's log and the
/// status message published over D-Bus. Applied to every error leaving this module, so no
/// auth variant added later can reintroduce the leak. The host and path stay, because they
/// are what makes a transport failure diagnosable.
fn redact_query(mut error: reqwest::Error) -> reqwest::Error {
    if let Some(url) = error.url_mut() {
        url.set_query(None);
    }
    error
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
pub static CATALOG: &[&Spec] = &[&clinepass::SPEC, &kimi::SPEC, &zai::SPEC];

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

    static SELF_HOSTED: Spec = Spec {
        id: "test",
        title: "Test",
        endpoint: |options| {
            format!(
                "{}/usage",
                base_url(options, "base_url", "https://cloud.example.invalid")
                    .expect("a required option was checked at build time")
            )
        },
        method: Method::Get,
        auth: Auth::Bearer,
        headers: &[("Accept", "application/json")],
        parse: parse_ok,
        credential_hint: "Test console.",
        options: &[OptionSchema {
            name: "base_url",
            title: "Base URL",
            description: None,
            default: "",
            choices: &[],
            required: true,
        }],
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

    #[test]
    fn an_unset_required_option_names_itself_rather_than_malforming_the_url() {
        // Sub2API, LiteLLM and LLMProxy have no default host: without this check the user
        // would see "Unreachable: relative URL without a base" on every poll, with nothing
        // pointing at the settings field that fixes it.
        let error = Keyed::new(&SELF_HOSTED, Credential::new("sk-6"), &options(&[]))
            .expect_err("the required option is unset");
        assert!(
            matches!(error, ProviderError::Local(ref message)
                if message == "Base URL is not set for this account"),
            "{error}"
        );

        let blank = Keyed::new(
            &SELF_HOSTED,
            Credential::new("sk-6"),
            &options(&[("base_url", "  ")]),
        )
        .expect_err("a blank value is an unset value");
        assert!(
            matches!(blank, ProviderError::Local(ref message) if message.contains("Base URL")),
            "{blank}"
        );

        let set = Keyed::new(
            &SELF_HOSTED,
            Credential::new("sk-6"),
            &options(&[("base_url", "https://self.example.invalid/")]),
        )
        .expect("builds with the option set");
        assert_eq!(set.url(), "https://self.example.invalid/usage");
    }

    #[test]
    fn a_base_url_trims_its_slash_and_falls_back_to_its_default() {
        let empty = Options::new();
        assert_eq!(
            base_url(&empty, "base_url", "https://cloud.example.invalid").expect("default"),
            "https://cloud.example.invalid"
        );
        let trailing = options(&[("base_url", "https://self.example.invalid//")]);
        assert_eq!(
            base_url(&trailing, "base_url", "https://cloud.example.invalid").expect("trims"),
            "https://self.example.invalid"
        );
    }

    #[test]
    fn a_base_url_refuses_plain_http_except_to_loopback() {
        let remote = options(&[("base_url", "http://self.example.invalid:8080")]);
        let error = base_url(&remote, "base_url", "https://cloud.example.invalid")
            .expect_err("a key over plain HTTP to a remote host is a key given away");
        assert!(
            matches!(error, ProviderError::Local(ref message) if message.contains("https://")),
            "{error}"
        );

        let words = options(&[("base_url", "self.example.invalid")]);
        assert!(base_url(&words, "base_url", "https://cloud.example.invalid").is_err());

        for loopback in ["http://localhost:11434", "http://127.0.0.1:11434"] {
            let set = options(&[("base_url", loopback)]);
            assert_eq!(
                base_url(&set, "base_url", "https://cloud.example.invalid")
                    .expect("loopback is how a self-hosted Ollama is reached"),
                loopback
            );
        }
    }

    #[tokio::test]
    async fn a_blank_credential_is_refused_before_a_request_is_spent() {
        let keyed = Keyed::new(&BEARER, Credential::new("   "), &options(&[])).expect("builds");
        assert!(matches!(
            keyed.fetch().await,
            Err(ProviderError::Credential { status: 401 })
        ));
    }

    #[tokio::test]
    async fn a_query_key_never_reaches_an_error_message() {
        // Port 9 (discard) has nothing listening on a development machine, so the request
        // fails at transport — the exact shape whose `reqwest::Error` prints the whole URL,
        // query string and key included. Whatever renders the error, Display or Debug, the
        // key must not be in it; the host must be, or the failure stops being diagnosable.
        static QUERY_LEAK: Spec = Spec {
            id: "test",
            title: "Test",
            endpoint: |_| "http://127.0.0.1:9/usage".to_owned(),
            method: Method::Get,
            auth: Auth::Query("key"),
            headers: &[],
            parse: parse_ok,
            credential_hint: "Test console.",
            options: &[],
        };
        let keyed = Keyed::new(
            &QUERY_LEAK,
            Credential::new("sk-query-secret"),
            &options(&[]),
        )
        .expect("builds");
        let error = keyed.fetch().await.expect_err("nothing listens on port 9");
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("sk-query-secret"), "{rendered}");
            assert!(!rendered.contains("key="), "{rendered}");
            assert!(rendered.contains("http://127.0.0.1:9/usage"), "{rendered}");
        }
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
