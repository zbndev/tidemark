//! Discovery of a newer published Tidemark release.

use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use semver::Version;

pub(crate) const INITIAL_DELAY: Duration = Duration::from_secs(60);
pub(crate) const INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_BODY: usize = 64 * 1024;
const ENDPOINT: &str = "https://api.github.com/repos/zbndev/tidemark/releases/latest";

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckError {
    #[error("could not build or send the release request")]
    Http(#[source] reqwest::Error),
    #[error("the release endpoint returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("the release response exceeded the size limit")]
    TooLarge,
    #[error("the release response was not valid JSON")]
    Json(#[source] serde_json::Error),
    #[error("the release or daemon version was not canonical X.X.X")]
    Version,
}

#[derive(Debug)]
pub(crate) struct Checker {
    client: reqwest::Client,
    endpoint: String,
    current: String,
}

impl Checker {
    pub(crate) fn production() -> Result<Self, CheckError> {
        Self::at(ENDPOINT, env!("CARGO_PKG_VERSION"))
    }

    fn at(endpoint: impl Into<String>, current: impl Into<String>) -> Result<Self, CheckError> {
        let current = current.into();
        version(&current)?;

        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2026-03-10"),
        );
        // The shared builder, so this goes through the user's proxy like everything else;
        // the timeout is tightened over its default because nothing waits on this answer.
        let client = tidemark_core::providers::http::builder()
            .map_err(CheckError::Http)?
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .map_err(CheckError::Http)?;

        Ok(Self {
            client,
            endpoint: endpoint.into(),
            current,
        })
    }

    pub(crate) async fn check(&self) -> Result<Option<String>, CheckError> {
        let mut response = self
            .client
            .get(&self.endpoint)
            .send()
            .await
            .map_err(CheckError::Http)?;
        if !response.status().is_success() {
            return Err(CheckError::Status(response.status()));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(CheckError::Http)? {
            if body.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(CheckError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let response: serde_json::Value =
            serde_json::from_slice(&body).map_err(CheckError::Json)?;
        let tag = response
            .get("tag_name")
            .and_then(serde_json::Value::as_str)
            .ok_or(CheckError::Version)?;
        newer(tag, &self.current)
    }
}

fn version(text: &str) -> Result<Version, CheckError> {
    let parts: Vec<_> = text.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return Err(CheckError::Version);
    }
    Version::parse(text).map_err(|_| CheckError::Version)
}

fn newer(tag: &str, current: &str) -> Result<Option<String>, CheckError> {
    let release = version(tag.strip_prefix('v').ok_or(CheckError::Version)?)?;
    let current = version(current)?;
    Ok((release > current).then(|| release.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_once(status: &str, body: &[u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the test can bind loopback");
        let address = listener.local_addr().expect("the listener has an address");
        let status = status.to_owned();
        let body = body.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("the checker connects");
            let mut request = [0_u8; 4096];
            let read = stream
                .read(&mut request)
                .await
                .expect("the request can be read");
            assert!(
                request[..read].starts_with(b"GET /latest HTTP/1.1\r\n"),
                "the checker requested the configured endpoint"
            );
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(head.as_bytes())
                .await
                .expect("the response headers can be written");
            stream
                .write_all(&body)
                .await
                .expect("the response body can be written");
        });
        format!("http://{address}/latest")
    }

    async fn check_response(status: &str, body: &[u8]) -> Result<Option<String>, CheckError> {
        let endpoint = serve_once(status, body).await;
        Checker::at(endpoint, "0.1.0").unwrap().check().await
    }

    #[test]
    fn a_newer_canonical_release_is_available() {
        assert_eq!(newer("v0.12.3", "0.9.9").unwrap(), Some("0.12.3".into()));
    }

    #[test]
    fn equal_and_older_releases_are_not_available() {
        assert_eq!(newer("v1.2.3", "1.2.3").unwrap(), None);
        assert_eq!(newer("v1.2.2", "1.2.3").unwrap(), None);
    }

    #[test]
    fn only_v_followed_by_a_canonical_three_part_version_is_accepted() {
        for tag in [
            "1.2.3",
            "v1.2",
            "v1.2.3.4",
            "v01.2.3",
            "v1.02.3",
            "v1.2.03",
            "v1.2.3-beta.2",
            "v1.2.3+build",
            "latest",
        ] {
            assert!(newer(tag, "0.1.0").is_err(), "accepted {tag}");
        }
    }

    #[tokio::test]
    async fn the_latest_tag_is_read_from_a_small_success_response() {
        assert_eq!(
            check_response("200 OK", br#"{"tag_name":"v0.2.0"}"#)
                .await
                .unwrap(),
            Some("0.2.0".into())
        );
    }

    #[tokio::test]
    async fn a_non_success_status_fails_the_check() {
        assert!(
            check_response("429 Too Many Requests", br#"{}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn malformed_json_fails_the_check() {
        assert!(check_response("200 OK", br#"{"#).await.is_err());
    }

    #[tokio::test]
    async fn a_noncanonical_release_tag_fails_the_check() {
        assert!(
            check_response("200 OK", br#"{"tag_name":"v0.2.0-beta.1"}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_response_over_the_limit_fails_the_check() {
        let body = vec![b'x'; MAX_BODY + 1];
        assert!(check_response("200 OK", &body).await.is_err());
    }
}
