//! The one place an outbound HTTP client is built.
//!
//! Everything that leaves this process identifies itself as `Tidemark/<version>`, and that
//! is load-bearing rather than polite: `platform.claude.com` sits behind Cloudflare and
//! answers a request with no user agent with `403 browser_signature_banned`. See
//! `CONTEXT.md` § Networking.

use super::ProviderError;
use reqwest::header::RETRY_AFTER;
use reqwest::{Client, StatusCode};
use std::time::Duration;

/// Ceiling on a whole request. Codex is the slowest of the five at 2.7 s measured, so this
/// is generous by an order of magnitude; it exists to stop a hung socket from wedging the
/// poll loop, not to enforce a latency budget.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on establishing the connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the client every provider talks through.
///
/// Providers own their client rather than sharing one, because Antigravity's is not
/// interchangeable: it talks to a loopback server with a self-signed certificate and needs
/// an exception no other provider should be given.
pub fn client() -> Result<Client, ProviderError> {
    Client::builder()
        .user_agent(tidemark_types::user_agent())
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
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
}
