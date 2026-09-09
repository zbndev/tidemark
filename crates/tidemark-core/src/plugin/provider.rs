//! One plugin account, as something the engine can poll.
//!
//! The transport is Tidemark's and is the same for every plugin: one request to the URL the
//! *account owner* typed, the key in the header the file declared, redirects off, the body
//! bounded, then a pure transformation. A plugin cannot choose a host, cannot add a header,
//! cannot follow a redirect to another origin and cannot see the credential — which is what
//! makes importing a file from a stranger a reasonable thing to do.

use super::{Definition, Method, PluginError, Reading, limits, lua, output};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use std::sync::Arc;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp};

/// Where one account sends its request. Supplied by the account owner, never by the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The absolute URL the key is sent to.
    pub url: String,
    /// Whether the owner acknowledged that this URL puts the key on the network in clear.
    /// Only ever true for `http://`, and the plugin cannot ask for it or remember it.
    pub allow_insecure_http: bool,
}

/// One plugin account.
pub struct PluginProvider {
    definition: Arc<Definition>,
    account: AccountId,
    client: reqwest::Client,
    url: reqwest::Url,
    credential: Credential,
}

impl std::fmt::Debug for PluginProvider {
    /// By hand: the credential must never reach a log, and `Definition` carries a whole file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginProvider")
            .field("provider", &self.definition.id)
            .field("account", &self.account)
            .field("url", &self.url.as_str())
            .finish_non_exhaustive()
    }
}

impl PluginProvider {
    /// Builds a client, refusing every endpoint the format does not allow.
    pub fn new(
        definition: Arc<Definition>,
        account: AccountId,
        endpoint: Endpoint,
        credential: Credential,
    ) -> Result<Self, ProviderError> {
        if credential.is_blank() {
            return Err(ProviderError::NoCredential);
        }
        let url = reqwest::Url::parse(&endpoint.url)
            .map_err(|error| ProviderError::Local(format!("the endpoint is not a URL: {error}")))?;
        match url.scheme() {
            "https" => {}
            "http" if endpoint.allow_insecure_http => {}
            "http" => {
                return Err(ProviderError::Local(
                    "this endpoint is plain http, which puts the API key on the network in \
                     clear; confirm insecure transport for this account to use it"
                        .to_owned(),
                ));
            }
            other => {
                return Err(ProviderError::Local(format!(
                    "{other} endpoints are not supported; use https"
                )));
            }
        }
        if url.host().is_none() {
            return Err(ProviderError::Local("the endpoint has no host".to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ProviderError::Local(
                "an endpoint may not carry credentials in its URL".to_owned(),
            ));
        }
        if url.fragment().is_some() {
            return Err(ProviderError::Local(
                "an endpoint may not carry a fragment".to_owned(),
            ));
        }

        // Redirects off: a 302 would move the secret header to whatever origin answered.
        let client = http::builder()
            .map_err(ProviderError::Client)?
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ProviderError::Client)?;

        Ok(Self {
            definition,
            account,
            client,
            url,
            credential,
        })
    }

    /// The one request, built. Separate from [`Provider::fetch`] so its headers are testable
    /// without a server: the header the key goes in is the whole security boundary.
    ///
    /// `User-Agent` and `Accept` are *inserted* after the declared header rather than added
    /// before it, and inserting replaces every value already under that name. A file may
    /// legitimately name either as its key header — neither moves the request or the
    /// connection, so neither is on the forbidden list — and `reqwest`'s `header` builder
    /// appends, which would otherwise send Tidemark's identity out with the account's key
    /// beside it. Setting them last means the plugin can name them and lose, rather than
    /// name them and leak. It is also why they are set here at all instead of being left to
    /// the client's defaults, which `reqwest` merges in only at execution time, where no
    /// test can see them.
    pub(crate) fn build_request(&self) -> Result<reqwest::Request, ProviderError> {
        let method = match self.definition.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut builder = self.client.request(method, self.url.clone()).header(
            &self.definition.api_key_header,
            format!(
                "{}{}",
                self.definition.api_key_prefix,
                self.credential.expose()
            ),
        );
        if self.definition.method == Method::Post {
            builder = builder.body(reqwest::Body::from(Vec::new()));
        }
        let mut request = builder.build().map_err(|error| {
            ProviderError::Local(format!("the request could not be built: {error}"))
        })?;
        let identity = reqwest::header::HeaderValue::from_str(&tidemark_types::user_agent())
            .map_err(|error| {
                ProviderError::Local(format!("the user agent is not a header: {error}"))
            })?;
        let headers = request.headers_mut();
        headers.insert(reqwest::header::USER_AGENT, identity);
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        Ok(request)
    }
}

impl Provider for PluginProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new(self.definition.id.clone())
    }

    fn account(&self) -> AccountId {
        self.account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(async move { poll(self).await.map(|reading| reading.snapshot) })
    }

    fn fetch_reading(&self) -> BoxFuture<'_, Result<Reading, ProviderError>> {
        Box::pin(poll(self))
    }
}

/// One reading, and its layout, kept together for the caller that needs both.
///
/// [`Provider::fetch`] can only return a `Snapshot`, so the daemon calls this instead when it
/// wants the presentation too — see `registry::plugin_account`.
pub async fn poll(provider: &PluginProvider) -> Result<Reading, ProviderError> {
    let request = provider.build_request()?;
    let body = crate::providers::keyed::request(&provider.definition.id, &provider.client, request)
        .await?;
    render(
        &provider.definition,
        body.as_bytes(),
        &provider.account,
        Timestamp::now(),
    )
    .map_err(provider_error)
}

/// The pure path: a body, a definition and a timestamp, with no key and no network.
///
/// This is what `tidemarkctl plugin render` runs, and it is the documented authoring loop —
/// so it is the same function polling uses, not a second implementation that could disagree.
pub fn render(
    definition: &Definition,
    response: &[u8],
    account: &AccountId,
    captured_at: Timestamp,
) -> Result<Reading, PluginError> {
    if response.len() > limits::RESPONSE_BYTES {
        return Err(PluginError::TooLarge {
            what: "the response body",
            found: response.len(),
            limit: limits::RESPONSE_BYTES,
        });
    }
    let document: serde_json::Value =
        serde_json::from_slice(response).map_err(|error| PluginError::Unreadable {
            path_hint: "the response".to_owned(),
            reason: error.to_string(),
        })?;
    let executed = lua::run(&definition.lua_source, &document, captured_at.as_unix())?;
    output::validate(
        &executed,
        &ProviderId::new(definition.id.clone()),
        account,
        captured_at,
    )
}

/// A plugin failure as the state machine the engine already has.
///
/// Every plugin failure is `Malformed`: the response arrived and did not mean what the
/// account's definition says it means. That is the state whose remedy is `TheyBrokeIt`, which
/// is the truth — either the endpoint changed or the file is wrong, and neither is something
/// the user fixes by waiting or by pasting a new key.
pub fn provider_error(error: PluginError) -> ProviderError {
    ProviderError::Malformed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::schema;

    const FILE: &str = r#"
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "X-Acme-Key"
api_key_prefix = ""

[parser]
language = "lua54"
source = '''
function parse(response, context)
    return {
        metrics = {
            {
                id = "cost",
                title = "Cost",
                value = number(response.used),
                maximum = number(response.limit),
                used_percent = percent(number(response.used), number(response.limit)),
                unit = "USD",
                window = { key = "monthly", length_seconds = 2592000 },
            },
        },
        card = { gauge("cost", { field = "used_percent" }) },
        details = {},
    }
end
'''
"#;

    fn definition() -> Arc<Definition> {
        Arc::new(schema::parse(FILE.as_bytes(), &[]).expect("the fixture parses"))
    }

    fn at() -> Timestamp {
        Timestamp::from_unix(1_788_870_896).expect("plausible")
    }

    #[test]
    fn a_fixture_renders_without_a_key_or_a_network() {
        let reading = render(
            &definition(),
            br#"{"used": "12.5", "limit": 50}"#,
            &AccountId::default(),
            at(),
        )
        .expect("the pure path needs nothing but the file and the body");
        assert_eq!(
            reading.presentation.metric("cost").unwrap().value,
            Some(12.5)
        );
        assert_eq!(reading.snapshot.windows[0].used_percent, 25.0);
    }

    #[test]
    fn a_body_that_is_not_json_fails_at_the_json_stage() {
        let error = render(
            &definition(),
            b"<html>nope</html>",
            &AccountId::default(),
            at(),
        )
        .expect_err("this is not a JSON document");
        assert!(
            matches!(error, PluginError::Unreadable { .. }),
            "a body that is not JSON fails before Lua ever sees it: {error:?}"
        );
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_is_parsed() {
        let huge = vec![b' '; limits::RESPONSE_BYTES + 1];
        assert!(matches!(
            render(&definition(), &huge, &AccountId::default(), at()),
            Err(PluginError::TooLarge {
                what: "the response body",
                ..
            })
        ));
    }

    #[test]
    fn an_endpoint_must_be_an_absolute_url_without_credentials_or_a_fragment() {
        for url in [
            "not a url",
            "/relative/path",
            "ftp://example.test/usage",
            "https://user:pass@example.test/usage",
            "https://example.test/usage#frag",
            "https://example.test/usage?key=abc#frag",
        ] {
            let endpoint = Endpoint {
                url: url.to_owned(),
                allow_insecure_http: false,
            };
            assert!(
                PluginProvider::new(
                    definition(),
                    AccountId::default(),
                    endpoint,
                    Credential::new("k")
                )
                .is_err(),
                "{url} must be refused"
            );
        }
        let good = Endpoint {
            url: "https://example.test/usage?window=month".into(),
            allow_insecure_http: false,
        };
        assert!(
            PluginProvider::new(
                definition(),
                AccountId::default(),
                good,
                Credential::new("k")
            )
            .is_ok()
        );
    }

    #[test]
    fn plain_http_needs_the_accounts_acknowledgement() {
        let unacknowledged = Endpoint {
            url: "http://metrics.corp.test/usage".into(),
            allow_insecure_http: false,
        };
        let error = PluginProvider::new(
            definition(),
            AccountId::default(),
            unacknowledged,
            Credential::new("k"),
        )
        .expect_err("a key on the wire in clear needs saying so");
        assert!(error.to_string().contains("http"), "{error}");

        let acknowledged = Endpoint {
            url: "http://metrics.corp.test/usage".into(),
            allow_insecure_http: true,
        };
        assert!(
            PluginProvider::new(
                definition(),
                AccountId::default(),
                acknowledged,
                Credential::new("k")
            )
            .is_ok()
        );
    }

    #[test]
    fn an_account_with_no_key_cannot_be_built_into_a_polling_client() {
        let endpoint = Endpoint {
            url: "https://example.test/usage".into(),
            allow_insecure_http: false,
        };
        assert!(
            PluginProvider::new(
                definition(),
                AccountId::default(),
                endpoint,
                Credential::new("   ")
            )
            .is_err()
        );
    }

    #[test]
    fn the_built_request_carries_the_declared_header_and_tidemarks_own_identity() {
        let endpoint = Endpoint {
            url: "https://example.test/usage".into(),
            allow_insecure_http: false,
        };
        let provider = PluginProvider::new(
            definition(),
            AccountId::default(),
            endpoint,
            Credential::new("sk-test"),
        )
        .expect("builds");
        let request = provider.build_request().expect("builds a request");
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.headers()["x-acme-key"], "sk-test");
        assert_eq!(request.headers()["accept"], "application/json");
        assert_eq!(
            request.headers()["user-agent"],
            tidemark_types::user_agent()
        );
        assert!(request.body().is_none());
    }

    #[test]
    fn a_post_declaration_sends_an_empty_body() {
        let file = FILE.replace("method = \"GET\"", "method = \"POST\"");
        let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
        let endpoint = Endpoint {
            url: "https://example.test/usage".into(),
            allow_insecure_http: false,
        };
        let provider = PluginProvider::new(
            definition,
            AccountId::default(),
            endpoint,
            Credential::new("k"),
        )
        .expect("builds");
        let request = provider.build_request().expect("builds");
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .unwrap_or_default(),
            b"",
            "a plugin has no way to send a body"
        );
    }

    #[test]
    fn a_prefix_is_placed_in_front_of_the_key_verbatim() {
        let file = FILE
            .replace(
                "api_key_header = \"X-Acme-Key\"",
                "api_key_header = \"Authorization\"",
            )
            .replace("api_key_prefix = \"\"", "api_key_prefix = \"Bearer \"");
        let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
        let endpoint = Endpoint {
            url: "https://example.test/usage".into(),
            allow_insecure_http: false,
        };
        let provider = PluginProvider::new(
            definition,
            AccountId::default(),
            endpoint,
            Credential::new("sk-1"),
        )
        .expect("builds");
        assert_eq!(
            provider.build_request().expect("builds").headers()["authorization"],
            "Bearer sk-1"
        );
    }

    #[test]
    fn the_provider_reports_the_plugin_id_and_account_it_polls() {
        let endpoint = Endpoint {
            url: "https://example.test/usage".into(),
            allow_insecure_http: false,
        };
        let provider = PluginProvider::new(
            definition(),
            AccountId::new("work"),
            endpoint,
            Credential::new("k"),
        )
        .expect("builds");
        assert_eq!(provider.id().as_str(), "com.acme.quota");
        assert_eq!(provider.account().as_str(), "work");
    }

    #[test]
    fn a_plugin_failure_is_reported_as_a_malformed_reading_and_names_no_secret() {
        let file = FILE.replace(
            "return {",
            "error(\"boom \" .. tostring(response.used)) return {",
        );
        let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
        let error = render(
            &definition,
            br#"{"used": 1, "limit": 2}"#,
            &AccountId::default(),
            at(),
        )
        .expect_err("the plugin raised");
        let rendered = error.to_string();
        assert!(rendered.contains("boom"));
        assert!(
            !rendered.contains("sk-"),
            "no credential can appear in a diagnostic: {rendered}"
        );
    }

    #[test]
    fn a_plugin_error_maps_onto_the_provider_failure_the_engine_already_handles() {
        assert!(matches!(
            provider_error(PluginError::Output {
                reason: "no".into()
            }),
            ProviderError::Malformed(_)
        ));
        assert!(matches!(
            provider_error(PluginError::LuaExhausted {
                what: "instruction"
            }),
            ProviderError::Malformed(_)
        ));
    }

    /// A file may declare `User-Agent` or `Accept` as its key header — the forbidden list is
    /// about headers that move the request or the connection, and these do neither. What
    /// must not happen is Tidemark's own identity going out carrying the key.
    #[test]
    fn a_declared_key_header_cannot_displace_or_join_tidemarks_own_identity() {
        for header in ["User-Agent", "Accept", "user-agent", "accept"] {
            let file = FILE.replace(
                "api_key_header = \"X-Acme-Key\"",
                &format!("api_key_header = \"{header}\""),
            );
            let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
            let endpoint = Endpoint {
                url: "https://example.test/usage".into(),
                allow_insecure_http: false,
            };
            let provider = PluginProvider::new(
                definition,
                AccountId::default(),
                endpoint,
                Credential::new("sk-secret"),
            )
            .expect("builds");
            let request = provider.build_request().expect("builds");
            let name = header.to_ascii_lowercase();
            let sent: Vec<&str> = request
                .headers()
                .get_all(name.as_str())
                .iter()
                .map(|value| value.to_str().expect("ascii"))
                .collect();
            assert!(
                !sent.iter().any(|value| value.contains("sk-secret")),
                "{header} went out carrying the key: {sent:?}"
            );
            assert_eq!(
                sent.len(),
                1,
                "{header} must carry exactly Tidemark's value: {sent:?}"
            );
        }
        assert_eq!(
            {
                let definition = definition();
                let endpoint = Endpoint {
                    url: "https://example.test/usage".into(),
                    allow_insecure_http: false,
                };
                let provider = PluginProvider::new(
                    definition,
                    AccountId::default(),
                    endpoint,
                    Credential::new("sk-secret"),
                )
                .expect("builds");
                provider.build_request().expect("builds").headers()["x-acme-key"]
                    .to_str()
                    .expect("ascii")
                    .to_owned()
            },
            "sk-secret",
            "and an ordinary declared header still carries it"
        );
    }
}
