//! Google OAuth and Cloud Code Assist project provisioning for Antigravity.

use serde::Deserialize;
use std::time::Duration;

use crate::oauth::{Client, Encoding};
use crate::providers::{ProviderError, http};

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(super) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
const REDIRECT_PORT: u16 = 51_121;
const REDIRECT_PATH: &str = "/oauth-callback";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
pub(super) const API_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const ONBOARD_ATTEMPTS: usize = 5;

/// The registered Google desktop client used by the system-browser login flow.
pub fn client() -> Client {
    Client {
        authorize_url: AUTHORIZE_URL,
        token_url: TOKEN_URL,
        client_id: CLIENT_ID,
        client_secret: Some(CLIENT_SECRET),
        redirect_port: REDIRECT_PORT,
        redirect_path: REDIRECT_PATH,
        scopes: SCOPES,
        authorize_extras: &[
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("include_granted_scopes", "true"),
        ],
        encoding: Encoding::Form,
    }
}

/// Discovers or provisions the Cloud AI Companion project belonging to a login response.
pub async fn complete_login(
    response: &serde_json::Value,
    now_ms: i64,
) -> Result<serde_json::Value, ProviderError> {
    let client = http::client()?;
    let endpoints: Vec<String> = API_ENDPOINTS
        .iter()
        .map(|endpoint| (*endpoint).to_owned())
        .collect();
    complete_login_at(
        &client,
        &endpoints,
        response,
        now_ms,
        Duration::from_secs(2),
    )
    .await
}

async fn complete_login_at(
    client: &reqwest::Client,
    endpoints: &[String],
    response: &serde_json::Value,
    now_ms: i64,
    retry_delay: Duration,
) -> Result<serde_json::Value, ProviderError> {
    let tokens = parse_login_response(response)?;
    let access_token = nonblank(&tokens.access_token).ok_or_else(|| {
        ProviderError::malformed("the Antigravity login response carried a blank access token")
    })?;

    let mut loaded = None;
    let mut last_error = None;
    for endpoint in endpoints {
        match post_json(
            client,
            endpoint,
            "v1internal:loadCodeAssist",
            access_token,
            &serde_json::json!({ "metadata": client_metadata() }),
        )
        .await
        {
            Ok(response) => {
                loaded = Some((endpoint, response));
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let Some((endpoint, loaded)) = loaded else {
        return Err(last_error.unwrap_or_else(|| {
            ProviderError::malformed("Antigravity has no Cloud Code Assist endpoint")
        }));
    };

    if let Some(project_id) = project_id(&loaded) {
        return document_from_login(response, project_id, now_ms);
    }
    let tier_id = loaded
        .get("allowedTiers")
        .and_then(serde_json::Value::as_array)
        .and_then(|tiers| {
            tiers.iter().find(|tier| {
                tier.get("isDefault").and_then(serde_json::Value::as_bool) == Some(true)
            })
        })
        .and_then(|tier| tier.get("id"))
        .and_then(serde_json::Value::as_str)
        .and_then(nonblank)
        .unwrap_or("free-tier");

    for attempt in 0..ONBOARD_ATTEMPTS {
        let onboarded = post_json(
            client,
            endpoint,
            "v1internal:onboardUser",
            access_token,
            &serde_json::json!({
                "tierId": tier_id,
                "metadata": client_metadata(),
            }),
        )
        .await?;
        if onboarded.get("done").and_then(serde_json::Value::as_bool) == Some(true) {
            let project_id = onboarded
                .get("response")
                .and_then(project_id)
                .ok_or_else(|| {
                    ProviderError::malformed(
                        "Antigravity provisioning completed without a project id",
                    )
                })?;
            return document_from_login(response, project_id, now_ms);
        }
        if attempt + 1 < ONBOARD_ATTEMPTS {
            tokio::time::sleep(retry_delay).await;
        }
    }
    Err(ProviderError::malformed(
        "Antigravity project provisioning did not complete after five attempts",
    ))
}

fn client_metadata() -> serde_json::Value {
    serde_json::json!({
        "ideType": "ANTIGRAVITY",
        "platform": "PLATFORM_UNSPECIFIED",
        "pluginType": "GEMINI",
    })
}

async fn post_json(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    access_token: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, ProviderError> {
    let url = format!("{}/{}", endpoint.trim_end_matches('/'), method);
    let response = client
        .post(url)
        .bearer_auth(access_token)
        .json(body)
        .send()
        .await
        .map_err(ProviderError::Transport)?;
    let status = response.status();
    let retry_after = http::retry_after_header(&response).map(str::to_owned);
    http::check(status, retry_after.as_deref())?;
    response.json().await.map_err(|error| {
        ProviderError::malformed(format!(
            "the Antigravity {method} response is not readable: {error}"
        ))
    })
}

fn project_id(response: &serde_json::Value) -> Option<&str> {
    let project = response.get("cloudaicompanionProject")?;
    project
        .as_str()
        .or_else(|| project.get("id").and_then(serde_json::Value::as_str))
        .and_then(nonblank)
}

fn document_from_login(
    response: &serde_json::Value,
    project_id: &str,
    now_ms: i64,
) -> Result<serde_json::Value, ProviderError> {
    let tokens = parse_login_response(response)?;
    let access_token = nonblank(&tokens.access_token).ok_or_else(|| {
        ProviderError::malformed("the Antigravity login response carried a blank access token")
    })?;
    let refresh_token = nonblank(&tokens.refresh_token).ok_or_else(|| {
        ProviderError::malformed("the Antigravity login response carried a blank refresh token")
    })?;
    let project_id = nonblank(project_id)
        .ok_or_else(|| ProviderError::malformed("Antigravity returned a blank project id"))?;
    if tokens.expires_in <= 0 {
        return Err(ProviderError::malformed(
            "the Antigravity login response carried a non-positive expiry",
        ));
    }
    Ok(serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_at": now_ms.saturating_add(tokens.expires_in.saturating_mul(1_000)),
        "project_id": project_id,
    }))
}

fn parse_login_response(response: &serde_json::Value) -> Result<LoginResponse, ProviderError> {
    serde_json::from_value(response.clone()).map_err(|error| {
        ProviderError::malformed(format!(
            "the Antigravity login response is not readable: {error}"
        ))
    })
}

fn nonblank(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[derive(Deserialize)]
struct LoginResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn local_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (requests_tx, requests_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, response_body) in responses {
                let (mut stream, _) = listener.accept().expect("request accepted");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).expect("request read");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    let Some(headers_end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
                requests_tx
                    .send(String::from_utf8(request).expect("request is text"))
                    .expect("request captured");
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .expect("response written");
            }
        });
        (format!("http://{address}"), requests_rx, handle)
    }

    fn token_response() -> serde_json::Value {
        serde_json::json!({
            "access_token": "owned-access",
            "refresh_token": "owned-refresh",
            "expires_in": 3_600
        })
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn antigravity_uses_the_registered_google_callback_and_offline_scopes() {
        let client = client();
        assert_eq!(client.redirect_port, 51_121);
        assert_eq!(client.redirect_path, "/oauth-callback");
        assert!(client.scopes.contains("cloud-platform"));
        assert!(client.scopes.contains("userinfo.email"));
        assert!(
            client
                .authorize_extras
                .contains(&("access_type", "offline"))
        );
        assert!(client.authorize_extras.contains(&("prompt", "consent")));
        assert!(
            client
                .authorize_extras
                .contains(&("include_granted_scopes", "true"))
        );
        assert!(client.client_secret.is_some());
    }

    #[test]
    fn login_document_keeps_tokens_expiry_and_project() {
        let document = document_from_login(
            &serde_json::json!({
                "access_token": "a",
                "refresh_token": "r",
                "expires_in": 3_600
            }),
            "project-1",
            1_787_270_400_000,
        )
        .expect("valid");
        assert_eq!(document["access_token"], "a");
        assert_eq!(document["refresh_token"], "r");
        assert_eq!(document["project_id"], "project-1");
        assert_eq!(document["expires_at"], 1_787_274_000_000_i64);
    }

    #[test]
    fn an_existing_cloud_companion_project_skips_onboarding() {
        let load = r#"{"cloudaicompanionProject":{"id":"project-1"}}"#;
        let (base, requests, server) = local_server(vec![(200, load)]);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("project discovered");

        assert_eq!(document["project_id"], "project-1");
        let request = requests.recv().expect("load request captured");
        assert!(
            request.starts_with("POST /v1internal:loadCodeAssist "),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer owned-access"),
            "{request}"
        );
        assert!(request.contains(r#""ideType":"ANTIGRAVITY""#), "{request}");
        assert!(
            request.contains(r#""platform":"PLATFORM_UNSPECIFIED""#),
            "{request}"
        );
        assert!(request.contains(r#""pluginType":"GEMINI""#), "{request}");
        assert!(request.contains("user-agent: Tidemark/"), "{request}");
        assert!(requests.try_recv().is_err());
        server.join().expect("server stopped");
    }

    #[test]
    fn onboarding_is_bounded_and_returns_its_project() {
        let load = r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#;
        let pending = r#"{"done":false}"#;
        let done = r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"project-2"}}}"#;
        let (base, requests, server) = local_server(vec![(200, load), (200, pending), (200, done)]);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("onboarding completes");

        assert_eq!(document["project_id"], "project-2");
        let _load = requests.recv().expect("load captured");
        for _ in 0..2 {
            let request = requests.recv().expect("onboard captured");
            assert!(
                request.starts_with("POST /v1internal:onboardUser "),
                "{request}"
            );
            assert!(request.contains(r#""tierId":"free-tier""#), "{request}");
        }
        server.join().expect("server stopped");
    }

    #[test]
    fn incomplete_onboarding_stops_after_five_attempts() {
        let load = r#"{"allowedTiers":[]}"#;
        let pending = r#"{"done":false}"#;
        let mut responses = vec![(200, load)];
        responses.extend(std::iter::repeat_n((200, pending), 5));
        let (base, requests, server) = local_server(responses);
        let client = crate::providers::http::client().expect("client");
        let error = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect_err("provisioning must be bounded");

        assert!(
            matches!(error, ProviderError::Malformed(message) if message.contains("provision"))
        );
        assert_eq!(requests.into_iter().count(), 6);
        server.join().expect("server stopped");
    }
}
