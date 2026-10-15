//! Direct Cloud Code quota for a Tidemark-owned OAuth session.
//!
//! This asks Cloud Code the same `retrieveUserQuotaSummary` question the local `agy` server
//! answers, with Tidemark's own login, and hands the body to the parser the local source
//! uses. The two sources differ in how they authenticate and in nothing the card shows.
//!
//! The summary is written per quota group, which is the unit Google limits: Claude and
//! GPT-OSS draw on one shared bucket. The per-model `fetchAvailableModels` this replaced
//! repeated that bucket once per model with no group identity to merge on, which is how
//! one shared limit came to be drawn as an Anthropic bar and an OpenAI bar.
//!
//! Unlike the per-model call, this one needs the project: asked without it, the server
//! answers `403 SUBSCRIPTION_REQUIRED` even for an account it would otherwise serve.

use tidemark_types::{AccountId, Snapshot, Timestamp};

use crate::providers::ProviderError;

use super::PROVIDER_ID;

/// Fetches the quota summary using the OAuth access token and the account's project.
pub async fn fetch(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    project_id: Option<&str>,
    account: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let url = format!(
        "{}/v1internal:retrieveUserQuotaSummary",
        endpoint.trim_end_matches('/')
    );
    let payload = match project_id {
        Some(project_id) => serde_json::json!({ "project": project_id }),
        // Not `{"project": ""}`: a blank project is a value the server would validate,
        // while an absent one is the question we mean to ask.
        None => serde_json::json!({}),
    };
    let response = client
        .post(&url)
        .bearer_auth(access_token)
        .header(
            reqwest::header::USER_AGENT,
            super::oauth::cloud_code_user_agent(),
        )
        .json(&payload)
        .send()
        .await
        .map_err(ProviderError::Transport)?;
    let sent_body = payload.to_string();
    let body = crate::providers::read_body(
        PROVIDER_ID,
        crate::debug::Sent {
            method: "POST",
            url: &url,
            body: Some(&sent_body),
        },
        response,
        &[access_token],
    )
    .await?;
    // No user status to go with it: that is a local-server RPC, and an OAuth token is
    // its own proof that somebody is logged in.
    super::parse_for_account(&body, None, Timestamp::now(), account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    const RECORDED: &str = include_str!("../../../tests/fixtures/antigravity-quota-summary.json");

    /// Captures exactly one request and returns one successful JSON response.
    fn one_request_server(
        response_body: &'static str,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request accepted");
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header read");
                if line == "\r\n" {
                    request.push_str(&line);
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().expect("content length");
                }
                request.push_str(&line);
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("body read");
            request.push_str(&String::from_utf8(body).expect("request body is text"));
            drop(reader);
            request_tx.send(request).expect("request captured");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .expect("response written");
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn direct_quota_requests_announce_hub_compatibility_and_tidemark() {
        let (base, requests, server) = one_request_server(
            r#"{"groups":[{"displayName":"Claude and GPT models","buckets":[{"bucketId":"3p-weekly","window":"weekly","remainingFraction":0.5}]}]}"#,
        );
        let client = crate::providers::http::client().expect("client");
        let account = AccountId::new("second");
        let snapshot = block_on(fetch(&client, &base, "token", Some("project-1"), &account))
            .expect("quota fetched");
        assert_eq!(snapshot.account, account);
        assert_eq!(snapshot.windows[0].used_percent, 50.0);

        let request = requests.recv().expect("request captured");
        server.join().expect("server stopped");
        let agent = request
            .lines()
            .find(|line| line.starts_with("user-agent:"))
            .expect("product identified");
        assert!(agent.starts_with("user-agent: Tidemark/"), "{agent}");
        assert!(
            agent.contains("(compatible; antigravity/hub/2.9.1 "),
            "Cloud Code otherwise rejects this client: {agent}"
        );
    }

    #[test]
    fn a_login_without_a_project_asks_without_one() {
        let (base, requests, server) = one_request_server(RECORDED);
        let client = crate::providers::http::client().expect("client");
        block_on(fetch(&client, &base, "token", None, &AccountId::default()))
            .expect("quota fetched");

        let request = requests.recv().expect("request captured");
        assert!(
            request.ends_with("\r\n\r\n{}"),
            "an absent project sends an empty body: {request}"
        );
        server.join().expect("server stopped");
    }

    #[test]
    fn direct_fetch_sends_the_project_and_owned_bearer_token() {
        let (base, requests, server) = one_request_server(RECORDED);
        let client = crate::providers::http::client().expect("client");
        let snapshot = block_on(fetch(
            &client,
            &base,
            "owned-access",
            Some("project-1"),
            &AccountId::default(),
        ))
        .expect("quota fetch succeeds");

        assert_eq!(snapshot.provider.as_str(), "antigravity");
        let request = requests.recv().expect("request captured");
        assert!(
            request.starts_with("POST /v1internal:retrieveUserQuotaSummary "),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer owned-access"),
            "{request}"
        );
        assert!(request.contains(r#""project":"project-1""#), "{request}");
        server.join().expect("server stopped");
    }

    #[test]
    fn claude_and_gpt_share_one_window_keyed_like_the_local_source() {
        // The bug this module was rewritten for: one shared limit must be one bar, under
        // the key the local `agy` source files it under, so switching sources keeps history.
        let (base, _requests, server) = one_request_server(RECORDED);
        let client = crate::providers::http::client().expect("client");
        let snapshot = block_on(fetch(
            &client,
            &base,
            "token",
            Some("project-1"),
            &AccountId::default(),
        ))
        .expect("recorded summary parses");
        server.join().expect("server stopped");

        let mut windows: Vec<(&str, &str)> = snapshot
            .windows
            .iter()
            .map(|window| (window.key.as_str(), window.title.as_str()))
            .collect();
        windows.sort_unstable();
        assert_eq!(
            windows,
            [
                ("3p/w604800", "Claude and GPT models · 7 days"),
                ("gemini/w604800", "Gemini Models · 7 days"),
            ]
        );
        let models = snapshot
            .details
            .iter()
            .find(|section| section.title == "Models")
            .expect("which models each bar covers");
        assert!(
            models
                .rows
                .iter()
                .any(|row| row.label == "Claude and GPT models"
                    && row.value == "Claude Opus, Claude Sonnet, GPT-OSS")
        );
    }
}
