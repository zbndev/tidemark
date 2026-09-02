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
//!
//! # The ones whose fetch is not one request
//!
//! A [`Spec`] states that the fetch *is* one request. The providers for which that is
//! false — a paged history, a balance plus a quota — are still key-authenticated, still
//! JSON, still a pasted key, so they live in this module too, but as [`HandSpec`]s with
//! their own `impl Provider`, each request going through [`request`] so the transport
//! rules below travel with the function. So does a provider whose build must *refuse* a
//! setting a [`Spec`] can only check for emptiness: `Keyed::new` screens an empty
//! value, but a base URL that is set and invalid — scheme-less, or plain HTTP to a
//! remote host — would otherwise surface inside `Spec::endpoint`, where the only way
//! out is a panic. They register in a second table in `tidemarkd::registry`, beside
//! [`CATALOG`]; everything the settings dialog needs from them is the same shape as a
//! `Spec`'s, so the dialog does not distinguish the tables.

pub mod abacus;
pub mod aiand;
pub mod alibaba;
pub mod amp;
pub mod augment;
pub mod chutes;
pub mod clawrouter;
pub mod clinepass;
pub mod codebuff;
pub mod commandcode;
pub mod crof;
pub mod cursor;
pub mod deepgram;
pub mod deepinfra;
pub mod deepseek;
pub mod elevenlabs;
pub mod factory;
pub mod fireworks;
pub mod gemini;
pub mod grok;
pub mod groq;
pub mod ibmbob;
pub mod kilo;
pub mod kimi;
pub mod litellm;
pub mod llmproxy;
pub mod longcat;
pub mod manus;
pub mod mimo;
pub mod minimax;
pub mod mistral;
pub mod moonshot;
pub mod nanogpt;
pub mod neuralwatt;
pub mod notion;
pub mod ollama;
// The slug carries a hyphen, which a module name cannot; the file keeps the slug so the
// provider is greppable by its storage key.
#[path = "openai-api.rs"]
pub mod openai_api;
pub mod opencode;
pub mod opencodego;
pub mod openrouter;
pub mod perplexity;
pub mod poe;
pub mod qoder;
pub mod sakana;
pub mod session;
pub mod stepfun;
pub mod sub2api;
pub mod synthetic;
pub mod t3chat;
pub mod venice;
pub mod warp;
pub mod wayfinder;
pub mod xai;
pub mod zai;
pub mod zenmux;
pub mod zoommate;

use super::{BoxFuture, Credential, Provider, ProviderError, http};
use crate::debug;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{AccountId, CredentialKind, ProviderId, Snapshot, Timestamp};

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
    /// Turns a response body into a snapshot for the account being polled.
    pub parse: fn(&str, Timestamp, &AccountId) -> Result<Snapshot, ProviderError>,
    /// One sentence saying which page the key is on.
    pub credential_hint: &'static str,
    /// What the user may choose.
    pub options: &'static [OptionSchema],
}

/// Builds a pollable client from the stored key and the account's settings: what a
/// [`HandSpec`] hands the registry so the daemon can construct the provider the same way
/// it constructs a [`Keyed`].
pub type Builder = fn(AccountId, Credential, &Options) -> Result<Arc<dyn Provider>, ProviderError>;

/// Everything a hand-written key-authenticated provider publishes about itself: the parts
/// of a [`Spec`] that are not about one request, and how to build a pollable client from
/// the stored key.
///
/// For the providers whose fetch is not one request — a paged history, a balance plus a
/// quota — or whose build refuses a required option's *value*, not just its absence, so
/// the refusal cannot live in [`Spec::endpoint`]. Each keeps its own `impl Provider` in
/// a module of this one and sends every request through [`request`], so the parts that
/// are easy to forget (status mapping, `Retry-After`, the redaction of any `reqwest`
/// error) travel with the function rather than with each provider. These register in a
/// second table in `tidemarkd::registry`, not in [`CATALOG`], because a `Spec` says the
/// fetch is one request; the settings dialog sees the same fields either way, so a
/// hand-written provider needs no stanza of its own there.
#[derive(Debug)]
pub struct HandSpec {
    /// The stable slug this provider's history is filed under. Never changes once shipped.
    pub id: &'static str,
    /// What to call it in front of a person.
    pub title: &'static str,
    /// How the account is authenticated, and therefore what the credentials dialog offers.
    /// [`CredentialKind::Key`] for the providers the user pastes a key for;
    /// [`CredentialKind::None`] for the ones that need nothing — the local gateways, which
    /// answer without a credential, and the browser-session providers, which read the
    /// session a browser on this machine already holds. The registry publishes this rather
    /// than assuming a key, and builds the account to match.
    pub credential: CredentialKind,
    /// One sentence saying which page the key is on. Empty for a provider that needs no
    /// credential, which has no page to send anyone to.
    pub credential_hint: &'static str,
    /// What the user may choose. A required option is refused by `build`, named in the
    /// message, exactly as [`Keyed::new`] refuses one.
    pub options: &'static [OptionSchema],
    /// Builds a client from the stored key and the account's settings.
    pub build: Builder,
}

/// A client for one key against one [`Spec`].
pub struct Keyed {
    spec: &'static Spec,
    client: reqwest::Client,
    credential: Credential,
    url: String,
    account: AccountId,
}

impl Keyed {
    /// Builds a client. The URL is resolved once, here, because a setting that changed
    /// the host would otherwise take effect only on the next daemon restart.
    ///
    /// A required option left unset fails here, named in the message, so the card says
    /// what is missing instead of the `Unreachable` a malformed URL would produce on
    /// every poll.
    pub fn new(
        account: AccountId,
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
            account,
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
        let body = request(self.spec.id, &self.client, self.build_request()?).await?;
        (self.spec.parse)(&body, Timestamp::now(), &self.account)
    }
}

/// Sends one built request and reads its body, mapping every failure the way the keyed
/// providers have agreed to map it.
///
/// A provider whose fetch is not one request — Poe pages through a usage history,
/// OpenRouter makes two calls — keeps its own `impl Provider` and sends each of its
/// requests through here, so the parts that are easy to forget travel with the function
/// rather than with each provider: `Retry-After` is read before the body consumes the
/// headers, a non-success status goes through `http::check`, the query string is
/// stripped off any `reqwest` error before it can be rendered, and the exchange reaches
/// [`crate::debug`] when the user has asked for a raw-response log.
///
/// The slug is the provider the request belongs to. It is a parameter rather than
/// something inferred from the URL because a provider whose fetch is several requests to
/// several hosts is exactly the one whose log is hard to read without it.
pub async fn request(
    provider: &str,
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<String, ProviderError> {
    request_with_url(provider, client, request)
        .await
        .map(|(body, _)| body)
}

/// [`request`], also reporting the URL the exchange finally landed on, redirect following
/// included — which is how a provider whose expired session shows up as a bounce to a
/// sign-in page recognises the bounce. A provider that does not care where the answer
/// came from stays on [`request`].
pub async fn request_with_url(
    provider: &str,
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<(String, reqwest::Url), ProviderError> {
    request_inspected(provider, client, request, |_| Ok(())).await
}

/// [`request_with_url`] with one hook: the response is shown to `inspect` before the
/// status mapping runs, for the provider whose refusal is only recognisable from a
/// response header — T3 Chat's Vercel challenge arrives stamped as a 429 and would
/// otherwise be read as a rate limit. An `Err` from the hook is the exchange's error;
/// everything else about the exchange is [`request`]'s.
pub async fn request_inspected<F>(
    provider: &str,
    client: &reqwest::Client,
    request: reqwest::Request,
    inspect: F,
) -> Result<(String, reqwest::Url), ProviderError>
where
    F: FnOnce(&reqwest::Response) -> Result<(), ProviderError>,
{
    let sent = debug::Recorded::of(&request);
    let note = |answer| {
        if let Some(sent) = &sent {
            debug::record(debug::Exchange {
                provider,
                sent: sent.sent(),
                answer,
            });
        }
    };

    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            let error = ProviderError::Transport(redact_query(error));
            note(debug::Answer::Failed {
                error: &error.to_string(),
            });
            return Err(error);
        }
    };

    let status = response.status();
    let url = response.url().clone();
    let retry_after = http::retry_after_header(&response).map(str::to_owned);
    if let Err(error) = inspect(&response) {
        note(debug::Answer::Refused {
            status: status.as_u16(),
        });
        return Err(error);
    }
    if let Err(error) = http::check(status, retry_after.as_deref()) {
        // Refused on its status: `reqwest` has not read the body and neither have we, so
        // the line says what came back without pretending to a body it never held.
        note(debug::Answer::Refused {
            status: status.as_u16(),
        });
        return Err(error);
    }

    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            let error = ProviderError::Transport(redact_query(error));
            note(debug::Answer::Failed {
                error: &error.to_string(),
            });
            return Err(error);
        }
    };
    // Before the emptiness check, deliberately: "the provider answered nothing" is one of
    // the things a person reads this log to confirm.
    note(debug::Answer::Body {
        status: status.as_u16(),
        body: &body,
    });
    if body.trim().is_empty() {
        // An empty body is its own error rather than serde's "EOF while parsing a value":
        // the one says the provider answered nothing, the other says we read it wrong.
        return Err(ProviderError::malformed(
            "the provider answered an empty body",
        ));
    }
    Ok((body, url))
}

/// Sends a credential proof request without reading or recording its response body.
///
/// Source inspection proves only that a local session is accepted. Its body is not a quota
/// reading and can contain account data, so it must not enter the optional raw-response log.
pub async fn validate(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<(), ProviderError> {
    let response = client
        .execute(request)
        .await
        .map_err(|error| ProviderError::Transport(redact_query(error)))?;
    let retry_after = http::retry_after_header(&response).map(str::to_owned);
    http::check(response.status(), retry_after.as_deref())
}

/// Sends a credential proof request and returns its body, still without recording it.
///
/// Several providers refuse a session inside an HTTP 200 envelope, so a status-only proof
/// would call an expired login ready. The no-log rule is the one thing inherited from
/// [`validate`]: the body may carry account data and is read only to classify it.
pub async fn validate_body(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<String, ProviderError> {
    let response = client
        .execute(request)
        .await
        .map_err(|error| ProviderError::Transport(redact_query(error)))?;
    let retry_after = http::retry_after_header(&response).map(str::to_owned);
    http::check(response.status(), retry_after.as_deref())?;
    response
        .text()
        .await
        .map_err(|error| ProviderError::Transport(redact_query(error)))
}

/// Reads a required option, refusing the build with the setting's name when it is unset
/// or blank — the check [`Keyed::new`] makes generically, factored out for the
/// hand-written providers whose builds are their own.
pub fn required(options: &Options, name: &str, title: &str) -> Result<String, ProviderError> {
    options
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError::Local(format!("{title} is not set for this account")))
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
        self.account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// Every single-request key-authenticated provider this build supports, in the order they
/// are shown.
///
/// Adding a provider is a file beside this one and a line here. Nothing else in the
/// workspace names it. The multi-request providers — and the ones whose build refuses a
/// required option's value — are [`HandSpec`]s registered in a second table in
/// `tidemarkd::registry`, not here.
pub static CATALOG: &[&Spec] = &[
    &amp::SPEC,
    &chutes::SPEC,
    &clawrouter::SPEC,
    &clinepass::SPEC,
    &crof::SPEC,
    &deepseek::SPEC,
    &elevenlabs::SPEC,
    &kimi::SPEC,
    &minimax::SPEC,
    &moonshot::SPEC,
    &neuralwatt::SPEC,
    &opencodego::SPEC,
    &synthetic::SPEC,
    &venice::SPEC,
    &warp::SPEC,
    &zenmux::SPEC,
];

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

    fn parse_ok(
        _body: &str,
        captured_at: Timestamp,
        _account: &AccountId,
    ) -> Result<Snapshot, ProviderError> {
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
        let keyed = Keyed::new(
            AccountId::default(),
            &BEARER,
            Credential::new("sk-1"),
            &options(&[]),
        )
        .expect("builds");
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
        let keyed = Keyed::new(
            AccountId::default(),
            &HEADER,
            Credential::new("sk-2"),
            &options(&[]),
        )
        .expect("builds");
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
        let keyed = Keyed::new(
            AccountId::default(),
            &QUERY,
            Credential::new("a b&c"),
            &options(&[]),
        )
        .expect("builds");
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
        let keyed = Keyed::new(
            AccountId::default(),
            &POSTING,
            Credential::new("sk-3"),
            &options(&[]),
        )
        .expect("builds");
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
        let global = Keyed::new(
            AccountId::default(),
            &REGIONAL,
            Credential::new("sk-4"),
            &options(&[]),
        )
        .expect("builds");
        let cn = Keyed::new(
            AccountId::default(),
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
        // No catalogued spec carries a required option today — the self-hosted ones are
        // HandSpecs, whose builds refuse the option's *value* too — but this is the
        // mechanism's own guard: without it a future spec with no default host would say
        // "Unreachable: relative URL without a base" on every poll, with nothing pointing
        // at the settings field that fixes it.
        let error = Keyed::new(
            AccountId::default(),
            &SELF_HOSTED,
            Credential::new("sk-6"),
            &options(&[]),
        )
        .expect_err("the required option is unset");
        assert!(
            matches!(error, ProviderError::Local(ref message)
                if message == "Base URL is not set for this account"),
            "{error}"
        );

        let blank = Keyed::new(
            AccountId::default(),
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
            AccountId::default(),
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
        let keyed = Keyed::new(
            AccountId::default(),
            &BEARER,
            Credential::new("   "),
            &options(&[]),
        )
        .expect("builds");
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
            AccountId::default(),
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
        let keyed = Keyed::new(
            AccountId::default(),
            &BEARER,
            Credential::new("sk-5"),
            &options(&[]),
        )
        .expect("builds");
        assert_eq!(keyed.id(), ProviderId::new("test"));
        assert_eq!(keyed.account(), AccountId::default());
    }

    #[test]
    fn a_keyed_client_never_prints_its_credential() {
        let keyed = Keyed::new(
            AccountId::default(),
            &BEARER,
            Credential::new("sk-super-secret"),
            &options(&[]),
        )
        .expect("builds");
        let rendered = format!("{keyed:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
