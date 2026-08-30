//! The one place an outbound HTTP client is built, and the proxy it is built against.
//!
//! Everything that leaves this process through this builder identifies itself as
//! `Tidemark/<version>`, and that is load-bearing rather than polite:
//! `platform.claude.com` sits behind Cloudflare and answers a request with no user agent
//! with `403 browser_signature_banned`. See `CONTEXT.md` § Networking. The one client
//! that deliberately does not start here is T3 Chat's: its edge admits only
//! browser-shaped clients, so it rides an emulating stack instead — see
//! `keyed/t3chat.rs`.
//!
//! # Why the proxy is process-wide rather than a parameter
//!
//! There are forty-odd provider clients and every one of them would forward the same
//! value, unchanged, from the same source. A proxy is not a property of a provider: it is
//! a property of this process's network, exactly like the user agent and the two timeouts
//! above, and those are read here rather than passed in for the same reason. It is written
//! from one place — the daemon, at startup and on a preference change, both of which are
//! already serialized behind the engine's configuration queue — and read wherever a client
//! or a child process is built.

use super::ProviderError;
use reqwest::header::RETRY_AFTER;
use reqwest::{Client, ClientBuilder, StatusCode};
use std::sync::RwLock;
use std::time::Duration;
use tidemark_types::Preferences;

/// Ceiling on a whole request. Codex is the slowest of the five at 2.7 s measured, so this
/// is generous by an order of magnitude; it exists to stop a hung socket from wedging the
/// poll loop, not to enforce a latency budget.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on establishing the connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a proxy is never used for.
///
/// Two providers answer on this machine — Antigravity's `agy` language server on loopback,
/// and a gateway a user runs themselves — and asking a proxy to reach `127.0.0.1` asks it
/// to connect to *itself*. The list is not a preference: it is the one exclusion that is
/// wrong to omit, and the same value is handed to child processes as `NO_PROXY`.
pub const NO_PROXY: &str = "localhost,127.0.0.0/8,::1";

/// The proxy every request and every child process goes through.
///
/// Holds the URL rather than a [`reqwest::Proxy`], which is neither `Clone` nor comparable,
/// and which a child process cannot be handed at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    url: String,
}

impl Proxy {
    /// The proxy these three settings describe, or `None` when the mode is `off`.
    ///
    /// A mode with no host or no port is an error rather than a silent `None`: the user
    /// asked for a proxy, and polling without one would look like it had worked. This is
    /// the form the daemon validates a change in, before it is written.
    pub fn new(mode: &str, host: &str, port: u16) -> Result<Option<Self>, String> {
        let scheme = match mode {
            Preferences::PROXY_OFF => return Ok(None),
            Preferences::PROXY_HTTP => "http",
            Preferences::PROXY_HTTPS => "https",
            // `socks5h`, not `socks5`: the host name is resolved *at the proxy*. Anyone
            // whose own resolver is the thing in the way would otherwise have the name
            // looked up here and the proxy handed an address it cannot reach — which is
            // also why `ALL_PROXY=socks5h://…` is the spelling the CLIs document.
            Preferences::PROXY_SOCKS5 => "socks5h",
            other => return Err(format!("unknown proxy mode {other:?}")),
        };
        let host = host.trim();
        if host.is_empty() {
            return Err("a proxy needs a host to reach".into());
        }
        if port == 0 {
            return Err("a proxy needs a port to reach".into());
        }
        // A bare IPv6 address is not a URL authority until it is bracketed.
        let authority = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let url = format!("{scheme}://{authority}:{port}");
        // Parsed once, here, so a host that cannot be a proxy URL is refused where the
        // user set it rather than failing every client build for the rest of the session.
        reqwest::Proxy::all(&url)
            .map_err(|error| format!("{url} cannot be used as a proxy: {error}"))?;
        Ok(Some(Self { url }))
    }

    /// The proxy the stored preferences describe, for the daemon's own startup.
    pub fn configured(preferences: &Preferences) -> Result<Option<Self>, String> {
        Self::new(
            &preferences.proxy_mode,
            &preferences.proxy_host,
            preferences.proxy_port,
        )
    }

    /// The proxy URL, in the form both `reqwest` and the `*_proxy` variables take.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// What a child process needs in its environment to use the same proxy.
    ///
    /// Both spellings of each name, because which one a program reads depends on what it
    /// was written in — Go and `curl` accept either, Node's fetch wants the uppercase
    /// form — and this is set per command rather than on the daemon's own environment.
    /// **That is the whole point:** an environment variable on the unit only takes effect
    /// on a restart, and a restart takes every provider off the screen for a few seconds.
    pub fn child_env(&self) -> [(&'static str, &str); 8] {
        [
            ("HTTP_PROXY", &self.url),
            ("http_proxy", &self.url),
            ("HTTPS_PROXY", &self.url),
            ("https_proxy", &self.url),
            ("ALL_PROXY", &self.url),
            ("all_proxy", &self.url),
            ("NO_PROXY", NO_PROXY),
            ("no_proxy", NO_PROXY),
        ]
    }

    fn to_reqwest(&self) -> Result<reqwest::Proxy, reqwest::Error> {
        Ok(reqwest::Proxy::all(&self.url)?.no_proxy(reqwest::NoProxy::from_string(NO_PROXY)))
    }
}

/// The configured proxy, or `None` for "whatever this process's environment says".
///
/// Written by the daemon and read by every client build; a `RwLock` rather than a channel
/// because there is one writer, no writer that blocks, and nothing to wake up.
static ACTIVE: RwLock<Option<Proxy>> = RwLock::new(None);

/// Points every client and child process built from now on at this proxy.
///
/// `None` restores the default: `reqwest` reads the process environment, and a child
/// inherits it — which is what this program did before the setting existed, and which is
/// why turning the setting off does not mean "no proxy".
pub fn set_proxy(proxy: Option<Proxy>) {
    *ACTIVE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = proxy;
}

/// The proxy in force, for a client build or a child process.
pub fn proxy() -> Option<Proxy> {
    ACTIVE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The builder every outbound client in this program starts from: the user agent, both
/// timeouts, and the proxy in force.
///
/// Public, and erroring as `reqwest` rather than as a provider, because the release check
/// is not a provider and still has to go through the same proxy. The one client that
/// deliberately does not start here is Antigravity's loopback client, which talks to this
/// machine.
pub fn builder() -> Result<ClientBuilder, reqwest::Error> {
    let builder = Client::builder()
        .user_agent(tidemark_types::user_agent())
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT);
    match proxy() {
        // Naming a proxy also stops `reqwest` reading the environment for one, which is
        // what makes the setting authoritative rather than one of two opinions.
        Some(proxy) => Ok(builder.proxy(proxy.to_reqwest()?)),
        None => Ok(builder),
    }
}

/// Builds the client every provider talks through.
///
/// Providers own their client rather than sharing one, because Antigravity's is not
/// interchangeable: it talks to a loopback server with a self-signed certificate and needs
/// an exception no other provider should be given.
pub fn client() -> Result<Client, ProviderError> {
    builder()
        .and_then(ClientBuilder::build)
        .map_err(ProviderError::Client)
}

/// Turns an unsuccessful status into the error variant the caller can act on.
///
/// `retry_after` is the `Retry-After` header when the provider sent one in seconds form.
/// The HTTP-date form is not parsed: no provider in v1 has been observed sending it, and
/// guessing wrong about the clock is worse than falling back to our own backoff.
pub fn check(status: StatusCode, retry_after: Option<&str>) -> Result<(), ProviderError> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::PAYMENT_REQUIRED => {
            ProviderError::Credential {
                status: status.as_u16(),
            }
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
            retry_after: retry_after.and_then(|value| value.trim().parse::<u64>().ok()),
        },
        other => ProviderError::Http {
            status: other.as_u16(),
        },
    })
}

/// Reads `Retry-After` off a response, for handing to [`check`].
pub fn retry_after_header(response: &reqwest::Response) -> Option<&str> {
    response.headers().get(RETRY_AFTER)?.to_str().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_announces_the_product() {
        // Not asserted through the builder — reqwest does not hand the header back — so
        // this pins the value the builder is given instead.
        assert!(tidemark_types::user_agent().starts_with("Tidemark/"));
        assert!(client().is_ok());
    }

    #[test]
    fn success_is_not_an_error() {
        assert!(check(StatusCode::OK, None).is_ok());
        assert!(check(StatusCode::NO_CONTENT, None).is_ok());
    }

    #[test]
    fn a_rejected_key_is_told_apart_from_a_broken_server() {
        assert!(matches!(
            check(StatusCode::UNAUTHORIZED, None),
            Err(ProviderError::Credential { status: 401 })
        ));
        assert!(matches!(
            check(StatusCode::FORBIDDEN, None),
            Err(ProviderError::Credential { status: 403 })
        ));
        assert!(matches!(
            check(StatusCode::BAD_GATEWAY, None),
            Err(ProviderError::Http { status: 502 })
        ));
    }

    #[test]
    fn the_providers_own_backoff_is_preferred_when_it_gives_one() {
        assert!(matches!(
            check(StatusCode::TOO_MANY_REQUESTS, Some(" 120 ")),
            Err(ProviderError::RateLimited {
                retry_after: Some(120)
            })
        ));
    }

    #[test]
    fn an_http_date_retry_after_falls_back_to_our_own_backoff() {
        assert!(matches!(
            check(
                StatusCode::TOO_MANY_REQUESTS,
                Some("Wed, 21 Oct 2026 07:28:00 GMT")
            ),
            Err(ProviderError::RateLimited { retry_after: None })
        ));
    }

    fn preferences(mode: &str, host: &str, port: u16) -> Preferences {
        Preferences {
            proxy_mode: mode.into(),
            proxy_host: host.into(),
            proxy_port: port,
            ..Preferences::default()
        }
    }

    #[test]
    fn each_mode_becomes_the_url_scheme_its_tools_document() {
        let url = |mode| {
            Proxy::configured(&preferences(mode, "proxy.example", 3128))
                .expect("usable")
                .expect("configured")
                .url()
                .to_owned()
        };
        assert_eq!(url(Preferences::PROXY_HTTP), "http://proxy.example:3128");
        assert_eq!(url(Preferences::PROXY_HTTPS), "https://proxy.example:3128");
        // Remote name resolution, deliberately: see `Proxy::configured`.
        assert_eq!(
            url(Preferences::PROXY_SOCKS5),
            "socks5h://proxy.example:3128"
        );
    }

    #[test]
    fn off_is_no_proxy_of_ours_whatever_the_host_and_port_say() {
        assert_eq!(
            Proxy::configured(&preferences(Preferences::PROXY_OFF, "proxy.example", 3128))
                .expect("readable"),
            None
        );
    }

    #[test]
    fn a_mode_without_somewhere_to_reach_is_refused_rather_than_ignored() {
        assert!(Proxy::configured(&preferences(Preferences::PROXY_HTTP, "", 3128)).is_err());
        assert!(
            Proxy::configured(&preferences(Preferences::PROXY_HTTP, "proxy.example", 0)).is_err()
        );
        assert!(Proxy::configured(&preferences("gopher", "proxy.example", 3128)).is_err());
    }

    #[test]
    fn a_bare_ipv6_address_is_bracketed_into_a_usable_authority() {
        assert_eq!(
            Proxy::configured(&preferences(Preferences::PROXY_SOCKS5, "::1", 1080))
                .expect("usable")
                .expect("configured")
                .url(),
            "socks5h://[::1]:1080"
        );
    }

    #[test]
    fn a_child_process_is_given_both_spellings_and_the_loopback_exclusion() {
        let proxy = Proxy::configured(&preferences(Preferences::PROXY_HTTP, "proxy.example", 3128))
            .expect("usable")
            .expect("configured");
        let env = proxy.child_env();

        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            assert_eq!(
                env.iter()
                    .find(|(candidate, _)| *candidate == name)
                    .map(|(_, value)| *value),
                Some(proxy.url()),
                "{name}"
            );
        }
        for name in ["NO_PROXY", "no_proxy"] {
            assert_eq!(
                env.iter()
                    .find(|(candidate, _)| *candidate == name)
                    .map(|(_, value)| *value),
                Some(NO_PROXY),
                "{name}"
            );
        }
    }

    #[test]
    fn a_socks_proxy_builds_a_client_at_all() {
        // The `socks` feature is what makes this true; without it `reqwest` accepts the
        // URL and then refuses every connection through it at runtime.
        let proxy = Proxy::configured(&preferences(Preferences::PROXY_SOCKS5, "127.0.0.1", 1080))
            .expect("usable")
            .expect("configured");
        assert!(
            Client::builder()
                .proxy(proxy.to_reqwest().expect("proxy built"))
                .build()
                .is_ok()
        );
    }
}
