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
/// Cloud Code hosts, production first.
///
/// `daily-` is a staging host and stays only as a second chance for a login that
/// production refused. The order is load-bearing beyond the login: the direct quota fetch
/// reads `API_ENDPOINTS[0]` outright, so a staging host in front would have production
/// quota read from it every poll.
pub(super) const API_ENDPOINTS: &[&str] = &[
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.googleapis.com",
];
/// How many times `loadCodeAssist` is re-asked for a project after onboarding.
const PROJECT_POLLS: usize = 5;

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

    let project = resolve_project(client, endpoint, access_token, &loaded, retry_delay).await;
    document_from_login(response, project.as_deref(), now_ms)
}

/// The Cloud AI Companion project this account uses, when it has one.
///
/// `None` is an ordinary answer rather than a failure. A tier declaring
/// `userDefinedCloudaicompanionProject` is telling us the server will not mint a project —
/// the user names their own — and for such an account `onboardUser` answers `done` with an
/// empty `cloudaicompanionProject` however many times it is asked. Refusing the login there
/// would deny an account its tokens over a field only the direct quota call ever wants.
async fn resolve_project(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    loaded: &serde_json::Value,
    retry_delay: Duration,
) -> Option<String> {
    if let Some(project_id) = project_id(loaded) {
        return Some(project_id.to_owned());
    }
    // No tier to onboard means nothing to ask for. The literal `free-tier` that used to
    // stand here is the one tier the server names in `ineligibleTiers` for this client.
    let tier_id = onboard_tier(loaded)?;

    // Asked once: the answer is final on the first call, so a retry is a second identical
    // question. A refusal — 403 for an account not eligible for the tier — is not fatal
    // either, because onboarding is an attempt to help rather than a precondition.
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
    .await;
    if let Ok(onboarded) = &onboarded
        && let Some(project_id) = onboarded.get("response").and_then(project_id)
    {
        return Some(project_id.to_owned());
    }

    // A tier whose project is the user's to name has no project coming, and the server has
    // said so in the same breath as the tier. Polling it is ten seconds of asking a question
    // already answered, spent after the browser is done and while the user is watching.
    if user_defined_project(loaded, tier_id) {
        return None;
    }

    // Where onboarding does provision a project asynchronously, it is `loadCodeAssist` that
    // starts naming it — the onboarding call itself has already given its final answer.
    for _ in 0..PROJECT_POLLS {
        tokio::time::sleep(retry_delay).await;
        let reloaded = post_json(
            client,
            endpoint,
            "v1internal:loadCodeAssist",
            access_token,
            &serde_json::json!({ "metadata": client_metadata() }),
        )
        .await;
        if let Ok(reloaded) = &reloaded
            && let Some(project_id) = project_id(reloaded)
        {
            return Some(project_id.to_owned());
        }
    }
    None
}

/// Whether the tier being onboarded expects the user to name their own project.
fn user_defined_project(loaded: &serde_json::Value, tier_id: &str) -> bool {
    loaded
        .get("allowedTiers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|tier| tier.get("id").and_then(serde_json::Value::as_str) == Some(tier_id))
        .and_then(|tier| {
            tier.get("userDefinedCloudaicompanionProject")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
}

/// The tier to onboard into: the one the server marks default, else the first it allows.
fn onboard_tier(loaded: &serde_json::Value) -> Option<&str> {
    let tiers = loaded
        .get("allowedTiers")
        .and_then(serde_json::Value::as_array)?;
    fn id(tier: &serde_json::Value) -> Option<&str> {
        tier.get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(nonblank)
    }
    tiers
        .iter()
        .find(|tier| {
            tier.get("isDefault").and_then(serde_json::Value::as_bool) == Some(true)
                && id(tier).is_some()
        })
        .or_else(|| tiers.iter().find(|tier| id(tier).is_some()))
        .and_then(id)
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
    project_id: Option<&str>,
    now_ms: i64,
) -> Result<serde_json::Value, ProviderError> {
    let tokens = parse_login_response(response)?;
    let access_token = nonblank(&tokens.access_token).ok_or_else(|| {
        ProviderError::malformed("the Antigravity login response carried a blank access token")
    })?;
    let refresh_token = nonblank(&tokens.refresh_token).ok_or_else(|| {
        ProviderError::malformed("the Antigravity login response carried a blank refresh token")
    })?;
    if tokens.expires_in <= 0 {
        return Err(ProviderError::malformed(
            "the Antigravity login response carried a non-positive expiry",
        ));
    }
    let mut document = serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_at": now_ms.saturating_add(tokens.expires_in.saturating_mul(1_000)),
    });
    // Absent rather than blank: a key holding an empty string would have to be defended
    // against everywhere it is read, and there is nothing to say.
    if let Some(project_id) = project_id.and_then(nonblank) {
        document["project_id"] = serde_json::Value::String(project_id.to_owned());
    }
    Ok(document)
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
    fn the_production_host_is_tried_before_the_daily_one() {
        // `daily-` is a staging host. It is kept as a second chance rather than removed,
        // but production quota must not be read from it while production answers — and
        // `API_ENDPOINTS[0]` is also the host the direct quota fetch uses outright.
        assert_eq!(API_ENDPOINTS[0], "https://cloudcode-pa.googleapis.com");
        assert!(
            API_ENDPOINTS
                .iter()
                .any(|endpoint| endpoint.contains("daily-"))
        );
        assert!(!API_ENDPOINTS[0].contains("daily-"));
    }

    #[test]
    fn login_document_keeps_tokens_expiry_and_project() {
        let document = document_from_login(
            &serde_json::json!({
                "access_token": "a",
                "refresh_token": "r",
                "expires_in": 3_600
            }),
            Some("project-1"),
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
    fn onboarding_that_provisions_a_project_returns_it_without_polling() {
        let load = r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true}]}"#;
        let done = r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"project-2"}}}"#;
        let (base, requests, server) = local_server(vec![(200, load), (200, done)]);
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
        let onboard = requests.recv().expect("onboard captured");
        assert!(
            onboard.starts_with("POST /v1internal:onboardUser "),
            "{onboard}"
        );
        assert!(onboard.contains(r#""tierId":"standard-tier""#), "{onboard}");
        assert!(
            requests.try_recv().is_err(),
            "one onboarding request is enough"
        );
        server.join().expect("server stopped");
    }

    #[test]
    fn polling_that_never_yields_a_project_still_yields_a_credential() {
        // Onboarding says `done` with an empty project object and the polls that follow
        // never name one either — so the login completes without a project rather than
        // failing, which is what the user saw as "unparseable response".
        let load = r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true}]}"#;
        let onboard = r#"{"done":true,"response":{"cloudaicompanionProject":{}}}"#;
        let mut responses = vec![(200, load), (200, onboard)];
        responses.extend(std::iter::repeat_n((200, load), 5));
        let (base, requests, server) = local_server(responses);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("a login without a project is still a login");

        assert_eq!(document["access_token"], "owned-access");
        assert_eq!(document["refresh_token"], "owned-refresh");
        assert!(
            document.get("project_id").is_none(),
            "no project was discovered, so none is stored: {document}"
        );
        assert_eq!(requests.into_iter().count(), 7);
        server.join().expect("server stopped");
    }

    #[test]
    fn a_user_defined_project_tier_is_not_polled_for_a_project_it_will_never_mint() {
        // `userDefinedCloudaicompanionProject` is the server saying the user names the
        // project. Polling it five times at two seconds apart is ten seconds of waiting for
        // an answer it has already given.
        let load = r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true,"userDefinedCloudaicompanionProject":true}]}"#;
        let onboard = r#"{"done":true,"response":{"cloudaicompanionProject":{}}}"#;
        // Enough responses queued for the polling this must not do, so a poll would be
        // served and counted rather than failing to connect and counting as nothing.
        let mut responses = vec![(200, load), (200, onboard)];
        responses.extend(std::iter::repeat_n((200, load), PROJECT_POLLS));
        let (base, requests, _server) = local_server(responses);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("a login without a project is still a login");

        assert!(document.get("project_id").is_none(), "{document}");
        // Drained rather than iterated: the server still waits on the requests it was given
        // responses for, so the channel does not close while it is parked in `accept`.
        let mut served = 0;
        while requests.try_recv().is_ok() {
            served += 1;
        }
        assert_eq!(
            served, 2,
            "one load and one onboarding, and no polling after them"
        );
    }

    #[test]
    fn onboarding_is_asked_once_and_the_project_is_then_polled_from_load() {
        // `onboardUser` is final on the first answer, so re-asking it is five wasted calls.
        // The project can still appear, and when it does it appears in `loadCodeAssist`.
        let load = r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true}]}"#;
        let onboard = r#"{"done":true,"response":{"cloudaicompanionProject":{}}}"#;
        let settled = r#"{"cloudaicompanionProject":{"id":"project-9"}}"#;
        let (base, requests, server) =
            local_server(vec![(200, load), (200, onboard), (200, settled)]);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("the settled project is picked up");

        assert_eq!(document["project_id"], "project-9");
        let first = requests.recv().expect("load captured");
        assert!(
            first.starts_with("POST /v1internal:loadCodeAssist "),
            "{first}"
        );
        let second = requests.recv().expect("onboard captured");
        assert!(
            second.starts_with("POST /v1internal:onboardUser "),
            "{second}"
        );
        assert!(second.contains(r#""tierId":"standard-tier""#), "{second}");
        let third = requests.recv().expect("re-load captured");
        assert!(
            third.starts_with("POST /v1internal:loadCodeAssist "),
            "{third}"
        );
        assert!(requests.try_recv().is_err(), "polling stops at the project");
        server.join().expect("server stopped");
    }

    #[test]
    fn a_refused_onboarding_does_not_fail_the_login() {
        // `free-tier` answers 403 FREE_TIER_USER_NOT_ELIGIBLE for an account Google has
        // moved to Antigravity. Onboarding is an attempt to help, not a precondition.
        let load = r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#;
        let refused = r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#;
        let settled = r#"{"cloudaicompanionProject":{"id":"project-3"}}"#;
        let (base, _requests, server) =
            local_server(vec![(200, load), (403, refused), (200, settled)]);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("a refused onboarding is not a refused login");

        assert_eq!(document["project_id"], "project-3");
        server.join().expect("server stopped");
    }

    #[test]
    fn no_allowed_tier_means_no_onboarding_request_at_all() {
        // The old code fell back to the literal `free-tier` here — the one tier the server
        // has already said this client may not have. Onboarding nothing is the honest move.
        let load = r#"{"allowedTiers":[]}"#;
        let (base, requests, server) = local_server(vec![(200, load)]);
        let client = crate::providers::http::client().expect("client");
        let document = block_on(complete_login_at(
            &client,
            &[base],
            &token_response(),
            1_787_270_400_000,
            Duration::ZERO,
        ))
        .expect("a login with no tier to onboard is still a login");

        assert!(document.get("project_id").is_none(), "{document}");
        for request in requests {
            assert!(
                !request.contains("onboardUser"),
                "no tier was offered, so nothing may be onboarded: {request}"
            );
        }
        server.join().expect("server stopped");
    }
}
