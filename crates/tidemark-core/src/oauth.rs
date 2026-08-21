//! Signing in to a provider, through the system browser and back over loopback.
//!
//! ADR 0002 in code: the authorize URL is opened by whatever the user's desktop uses to
//! open URLs, a temporary HTTP listener on `127.0.0.1` receives the redirect, `state` is
//! validated, exactly one request is served and the listener is dropped. No browser engine
//! is linked into Tidemark, and none is needed.
//!
//! # The port is the provider's to choose, not ours
//!
//! ADR 0002 asked for a random free port. That is achievable only where the OAuth client
//! accepts any loopback redirect. Neither of the two clients here does: an authorization
//! server matches `redirect_uri` against what the client was registered with, so Claude's
//! callback is `http://localhost:54545/callback` and Codex's is
//! `http://localhost:1455/auth/callback`, exactly, or the request is refused before the
//! user ever sees a consent screen. See ADR 0003. The rest of the ADR stands unchanged,
//! and a provider that does allow an ephemeral port gets one by setting
//! [`Client::redirect_port`] to zero.
//!
//! # Why the daemon does this and not the interface
//!
//! Two of the three steps are network I/O — the token exchange, and the refresh that
//! follows — and the GUI is not allowed any. It contributes the one thing it is uniquely
//! able to do: open a URL on the user's desktop. So the flow is split across the bus,
//! [`Login::begin`] handing out the URL and [`Login::finish`] waiting for the browser to
//! come back.
//!
//! # What is deliberately not here
//!
//! A device-code or paste-the-code fallback. Every provider Tidemark signs into runs a
//! desktop client with a loopback redirect; adding a second flow before a user has been
//! unable to use the first is inventing a requirement.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::providers::{ProviderError, http};

/// How long the listener waits for the browser before giving the port back.
///
/// Generous on purpose: this covers a user who has to find a password manager, pick
/// between two accounts and read a consent screen, and the cost of being wrong in the
/// short direction is making them start over.
pub const BROWSER_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest request line the listener will read before deciding it is not a redirect.
///
/// An authorization code and an `id_token` hint can make a callback URL genuinely long, so
/// this is not tight. It exists so that something which connects to the port and streams
/// cannot make the daemon buy memory for it.
const MAX_REQUEST: usize = 16 * 1024;

/// How the token endpoint wants the exchange spelled.
///
/// Not a detail worth abstracting away: Claude's `/v1/oauth/token` is form-encoded and
/// Codex's is JSON. Two providers, two spellings of the same grant, and getting it wrong
/// is a 400 with no useful body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// `application/x-www-form-urlencoded`.
    Form,
    /// `application/json`.
    Json,
}

/// One provider's OAuth client, as registered with its authorization server.
#[derive(Debug, Clone)]
pub struct Client {
    /// Where the user is sent to approve.
    pub authorize_url: &'static str,
    /// Where the code is exchanged.
    pub token_url: &'static str,
    /// The public client identifier. Most public clients hold no secret; some desktop
    /// clients also declare a public secret that is sent with the code exchange.
    pub client_id: &'static str,
    /// An optional public desktop-client secret required by some authorization servers.
    pub client_secret: Option<&'static str>,
    /// The loopback port the client is registered with, or zero to take any free one.
    pub redirect_port: u16,
    /// The path of the registered redirect URI.
    pub redirect_path: &'static str,
    /// Space-separated scopes.
    pub scopes: &'static str,
    /// Anything else the server insists on in the authorize URL.
    pub authorize_extras: &'static [(&'static str, &'static str)],
    /// How the token exchange is encoded.
    pub encoding: Encoding,
}

impl Client {
    fn redirect_uri(&self, port: u16) -> String {
        // `localhost` rather than `127.0.0.1`: it is the spelling both clients are
        // registered with, and the two are not interchangeable to a server doing an exact
        // string match on the redirect.
        format!("http://localhost:{port}{}", self.redirect_path)
    }
}

/// Why a login did not produce a credential.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The callback port could not be taken.
    #[error("cannot listen on 127.0.0.1:{port}: {source}")]
    Port {
        /// The port the provider's client is registered with.
        port: u16,
        /// Why the bind failed.
        #[source]
        source: std::io::Error,
    },
    /// The browser did not come back in time.
    #[error("the browser did not come back within {}s", BROWSER_TIMEOUT.as_secs())]
    TimedOut,
    /// The user said no, or the server refused.
    #[error("the provider refused the login: {0}")]
    Refused(String),
    /// The redirect did not carry what a redirect must carry.
    #[error("the login callback was not usable: {0}")]
    Callback(String),
    /// The code could not be exchanged for a token.
    #[error(transparent)]
    Exchange(#[from] ProviderError),
    /// Reading the callback off the socket failed.
    #[error("the login callback could not be read: {0}")]
    Io(#[from] std::io::Error),
}

/// A login in progress: the port is held and the URL is out with the user.
#[derive(Debug)]
pub struct Login {
    client: Client,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
    url: String,
}

impl Login {
    /// Takes the callback port and builds the URL for the user to open.
    ///
    /// The listener is bound *before* the URL exists, so a port that is already taken —
    /// the vendor's own CLI is mid-login, or a previous attempt has not let go — is
    /// reported now rather than after the user has approved something we cannot receive.
    pub async fn begin(client: Client) -> Result<Self, LoginError> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, client.redirect_port));
        let listener = TcpListener::bind(address)
            .await
            .map_err(|source| LoginError::Port {
                port: client.redirect_port,
                source,
            })?;
        let port = listener
            .local_addr()
            .map_err(|source| LoginError::Port {
                port: client.redirect_port,
                source,
            })?
            .port();

        let state = random_token();
        let verifier = random_token();
        let redirect_uri = client.redirect_uri(port);
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
            client.authorize_url,
            escape(client.client_id),
            escape(&redirect_uri),
            escape(client.scopes),
            escape(&state),
            escape(&challenge(&verifier)),
        );
        for (name, value) in client.authorize_extras {
            url.push_str(&format!("&{}={}", escape(name), escape(value)));
        }

        Ok(Self {
            client,
            listener,
            redirect_uri,
            state,
            verifier,
            url,
        })
    }

    /// The address to open in the browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the redirect and exchanges the code.
    ///
    /// Consumes the login, and gives the port back before the exchange begins: the
    /// listener is dropped as soon as the callback has been answered, so a well-known port
    /// is never held across a request to somebody else's server.
    pub async fn finish(self, client: &reqwest::Client) -> Result<serde_json::Value, LoginError> {
        let Self {
            client: oauth,
            listener,
            redirect_uri,
            state,
            verifier,
            ..
        } = self;
        let code = {
            let waiting = Waiting {
                redirect_path: oauth.redirect_path,
                listener,
                state: &state,
            };
            tokio::time::timeout(BROWSER_TIMEOUT, waiting.receive())
                .await
                .map_err(|_| LoginError::TimedOut)??
            // `waiting` owns the listener and ends here, so the socket is closed before a
            // single byte of the exchange goes out.
        };
        exchange(client, &oauth, &redirect_uri, &state, &verifier, &code).await
    }
}

/// The listener while it is waiting for the browser, and nothing else.
///
/// Separate from [`Login`] so that the socket has an owner that ends before the token
/// exchange begins. A well-known port held across a request to somebody else's server is a
/// port the vendor's own CLI cannot use for however long that request takes.
struct Waiting<'a> {
    redirect_path: &'static str,
    listener: TcpListener,
    state: &'a str,
}

impl Waiting<'_> {
    /// Serves requests until one of them is the redirect, and answers each in the browser.
    ///
    /// Not "exactly one connection": browsers ask for `/favicon.ico`, and a preflight or a
    /// stray probe on the port would otherwise consume the single request the login had.
    /// What is served exactly once is the *callback* — the first request carrying the
    /// redirect path ends the listener, whether it validated or not.
    async fn receive(&self) -> Result<String, LoginError> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let Some(target) = read_request_target(&mut stream).await? else {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    &page("Not a login callback", ""),
                )
                .await;
                continue;
            };
            let (path, query) = split_target(&target);
            if path != self.redirect_path {
                respond(
                    &mut stream,
                    "404 Not Found",
                    &page("Not a login callback", ""),
                )
                .await;
                continue;
            }

            let result = self.read_callback(query);
            let body = match &result {
                Ok(_) => page(
                    "Signed in",
                    "Tidemark has your account. You can close this tab.",
                ),
                Err(error) => page("Sign-in failed", &error.to_string()),
            };
            let status = if result.is_ok() {
                "200 OK"
            } else {
                "400 Bad Request"
            };
            respond(&mut stream, status, &body).await;
            return result;
        }
    }

    /// The authorization code out of one callback query, or why there is not one.
    fn read_callback(&self, query: &str) -> Result<String, LoginError> {
        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut description = None;
        for (name, value) in query_pairs(query) {
            match name.as_str() {
                "code" => code = Some(value),
                "state" => state = Some(value),
                "error" => error = Some(value),
                "error_description" => description = Some(value),
                _ => {}
            }
        }

        if let Some(error) = error {
            let detail = description.unwrap_or(error);
            return Err(LoginError::Refused(detail));
        }
        // Checked before the code is looked at, let alone spent. A callback carrying
        // somebody else's `state` is a request this login did not start.
        match state.as_deref() {
            Some(returned) if returned == self.state => {}
            Some(_) => {
                return Err(LoginError::Callback(
                    "the callback carried a state this login did not issue".into(),
                ));
            }
            None => {
                return Err(LoginError::Callback("the callback carried no state".into()));
            }
        }
        code.filter(|code| !code.trim().is_empty())
            .ok_or_else(|| LoginError::Callback("the callback carried no code".into()))
    }
}

/// Trades the authorization code for tokens.
async fn exchange(
    client: &reqwest::Client,
    oauth: &Client,
    redirect_uri: &str,
    state: &str,
    verifier: &str,
    code: &str,
) -> Result<serde_json::Value, LoginError> {
    let mut fields = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", oauth.client_id),
        ("code_verifier", verifier),
        // Not universal, and harmless where it is ignored: Anthropic's token endpoint
        // wants the state back alongside the code.
        ("state", state),
    ];
    if let Some(secret) = oauth.client_secret {
        fields.push(("client_secret", secret));
    }
    let request = client.post(oauth.token_url);
    let request = match oauth.encoding {
        Encoding::Form => request.form(&fields),
        Encoding::Json => request.json(
            &fields
                .iter()
                .map(|(name, value)| {
                    (
                        (*name).to_owned(),
                        serde_json::Value::String((*value).to_owned()),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        ),
    };
    let response = request
        .send()
        .await
        .map_err(|error| LoginError::Exchange(ProviderError::Transport(error)))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail = body.trim();
        let detail = if detail.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {}", truncate(detail, 400))
        };
        return Err(LoginError::Refused(detail));
    }
    response.json().await.map_err(|error| {
        LoginError::Exchange(ProviderError::malformed(format!(
            "the token response is not readable: {error}"
        )))
    })
}

/// The HTTP client a login uses. The same one every provider uses, so a login identifies
/// itself the way every other request does.
pub fn client() -> Result<reqwest::Client, ProviderError> {
    http::client()
}

/// A 256-bit random value, base64url without padding.
///
/// Used for both `state` and the PKCE verifier. `getrandom` rather than a userspace
/// generator: this is the value that stops another process on the machine from completing
/// a login it did not start.
fn random_token() -> String {
    let mut buffer = [const { std::mem::MaybeUninit::<u8>::uninit() }; 32];
    let (filled, _) = rustix::rand::getrandom(&mut buffer, rustix::rand::GetRandomFlags::empty())
        .expect("getrandom cannot fail for a 32-byte buffer with no flags");
    BASE64URL.encode(filled)
}

/// The S256 PKCE challenge for a verifier.
fn challenge(verifier: &str) -> String {
    BASE64URL.encode(Sha256::digest(verifier.as_bytes()))
}

/// Percent-encodes everything that is not unreserved, so a scope list with spaces and a
/// redirect URI with a colon survive being query parameters.
fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Undoes [`escape`], plus the `+` that some servers use for a space.
fn unescape(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `name=value` pairs of a query string, decoded.
fn query_pairs(query: &str) -> impl Iterator<Item = (String, String)> + '_ {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (unescape(name), unescape(value))
        })
}

/// Splits a request target into its path and its query.
fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    }
}

/// Reads the request line and returns its target, or `None` if this was not an HTTP
/// request we can make sense of.
async fn read_request_target(stream: &mut TcpStream) -> Result<Option<String>, std::io::Error> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        // The request line is the whole of what a redirect carries; the headers after it
        // are read only far enough to know the line has ended.
        if let Some(position) = buffer.windows(2).position(|pair| pair == b"\r\n") {
            let line = String::from_utf8_lossy(&buffer[..position]).into_owned();
            return Ok(line.split_whitespace().nth(1).map(str::to_owned));
        }
        if buffer.len() > MAX_REQUEST {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Answers one request and closes the connection.
///
/// Failures are ignored: the browser tab is a courtesy, and a login that succeeded must
/// not be reported as failed because the page could not be written to a socket the browser
/// had already given up on.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// The page the browser is left on.
///
/// Deliberately plain: it is served over `http://localhost` by a background daemon, and
/// anything that looked like the provider's own sign-in page would be teaching the user
/// the wrong lesson about what to trust.
fn page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Tidemark</title>\
         <body style=\"font-family:system-ui,sans-serif;margin:4rem auto;max-width:32rem;text-align:center\">\
         <h1 style=\"font-weight:600\">{}</h1><p>{}</p></body>",
        escape_html(title),
        escape_html(detail)
    )
}

fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        test_client(0, Encoding::Form, None)
    }

    fn test_client(
        redirect_port: u16,
        encoding: Encoding,
        client_secret: Option<&'static str>,
    ) -> Client {
        Client {
            authorize_url: "https://example.test/authorize",
            token_url: "https://example.test/token",
            client_id: "an-id",
            client_secret,
            // Zero, so the tests never fight the real ports a vendor CLI may be using.
            redirect_port,
            redirect_path: "/callback",
            scopes: "user:profile user:inference",
            authorize_extras: &[("code", "true")],
            encoding,
        }
    }

    #[test]
    fn a_provider_client_secret_is_sent_only_when_declared() {
        let body = finish_against_server(test_client(
            0,
            Encoding::Form,
            Some("desktop-public-secret"),
        ))
        .expect("exchange succeeds");
        assert!(body.contains("client_secret=desktop-public-secret"));

        let body =
            finish_against_server(test_client(0, Encoding::Form, None)).expect("exchange succeeds");
        assert!(!body.contains("client_secret="));

        let body = finish_against_server(test_client(
            0,
            Encoding::Json,
            Some("desktop-public-secret"),
        ))
        .expect("exchange succeeds");
        assert!(body.contains("\"client_secret\":\"desktop-public-secret\""));

        let body =
            finish_against_server(test_client(0, Encoding::Json, None)).expect("exchange succeeds");
        assert!(!body.contains("client_secret"));
    }

    fn finish_against_server(oauth: Client) -> Result<String, LoginError> {
        use std::io::{Read, Write};
        use std::net::TcpStream as SyncStream;

        let token_server = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let token_url: &'static str = Box::leak(
            format!(
                "http://127.0.0.1:{}/token",
                token_server.local_addr().expect("addr").port()
            )
            .into_boxed_str(),
        );
        let token_thread = std::thread::spawn(move || {
            let (mut stream, _) = token_server.accept().expect("the exchange arrives");
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).expect("request");
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let body = r#"{"access_token":"at"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("answer");
            request
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let mut oauth = oauth;
        oauth.token_url = token_url;
        let login = runtime.block_on(Login::begin(oauth))?;
        let url = login.url().to_owned();
        let port: u16 = login
            .redirect_uri
            .rsplit_once(':')
            .and_then(|(_, tail)| tail.split('/').next())
            .and_then(|port| port.parse().ok())
            .expect("the redirect names the port it bound");
        let state = url
            .split("&state=")
            .nth(1)
            .and_then(|tail| tail.split('&').next())
            .expect("the URL carries the state")
            .to_owned();
        let browser = std::thread::spawn(move || {
            let mut stream = SyncStream::connect(("127.0.0.1", port)).expect("listener");
            stream
                .write_all(
                    format!(
                        "GET /callback?code=the-code&state={state} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .expect("request");
        });
        let http = super::client().expect("an HTTP client");
        runtime.block_on(login.finish(&http))?;
        browser.join().expect("browser thread");
        Ok(token_thread.join().expect("token thread"))
    }

    #[test]
    fn a_verifier_and_its_challenge_are_the_pair_rfc_7636_describes() {
        // The example from RFC 7636 § 4.2, which is the only way to be sure the digest is
        // over the ASCII verifier and encoded base64url without padding.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn two_logins_never_share_a_state() {
        assert_ne!(random_token(), random_token());
        assert!(random_token().len() >= 43, "256 bits, base64url");
    }

    #[test]
    fn a_scope_list_survives_being_a_query_parameter() {
        assert_eq!(
            escape("user:profile user:inference"),
            "user%3Aprofile%20user%3Ainference"
        );
        assert_eq!(
            escape("http://localhost:54545/callback"),
            "http%3A%2F%2Flocalhost%3A54545%2Fcallback"
        );
    }

    #[test]
    fn a_query_round_trips_through_the_encoding_a_browser_uses() {
        let pairs: Vec<(String, String)> =
            query_pairs("code=ac%2Fb%2B1&state=xyz&error_description=not+today").collect();
        assert_eq!(
            pairs,
            vec![
                ("code".to_owned(), "ac/b+1".to_owned()),
                ("state".to_owned(), "xyz".to_owned()),
                ("error_description".to_owned(), "not today".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn the_authorize_url_carries_everything_the_server_needs() {
        let login = Login::begin(client())
            .await
            .expect("an ephemeral port binds");
        let url = login.url().to_owned();
        assert!(url.starts_with("https://example.test/authorize?"), "{url}");
        for required in [
            "response_type=code",
            "client_id=an-id",
            "code_challenge_method=S256",
            "&code=true",
        ] {
            assert!(url.contains(required), "{required} missing from {url}");
        }
        assert!(
            url.contains(&format!("redirect_uri={}", escape(&login.redirect_uri))),
            "{url}"
        );
        assert!(
            !url.contains(&login.verifier),
            "the verifier must never leave the process: {url}"
        );
    }

    /// The waiting half of a login, without binding anything: the callback rules are pure
    /// and the port is not what they are about.
    fn waiting(state: &str) -> Waiting<'_> {
        Waiting {
            redirect_path: "/callback",
            listener: futures_lite_block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
                .expect("an ephemeral port binds"),
            state,
        }
    }

    #[test]
    fn a_callback_with_the_wrong_state_is_refused_before_the_code_is_spent() {
        let error = waiting("the-state")
            .read_callback("code=abc&state=somebody-elses")
            .expect_err("a foreign state is not this login");
        assert!(matches!(error, LoginError::Callback(_)), "{error}");
    }

    #[test]
    fn a_callback_the_user_declined_says_so_rather_than_looking_broken() {
        let error = waiting("the-state")
            .read_callback("error=access_denied&error_description=The+user+said+no")
            .expect_err("declined");
        match error {
            LoginError::Refused(detail) => assert_eq!(detail, "The user said no"),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn a_callback_that_is_only_a_state_has_no_code_to_use() {
        let error = waiting("the-state")
            .read_callback("state=the-state")
            .expect_err("no code");
        assert!(matches!(error, LoginError::Callback(_)), "{error}");
    }

    #[test]
    fn the_right_state_and_a_code_is_the_one_case_that_works() {
        assert_eq!(
            waiting("the-state")
                .read_callback("code=the-code&state=the-state")
                .expect("accepted"),
            "the-code"
        );
    }

    #[test]
    fn a_target_splits_into_a_path_and_a_query() {
        assert_eq!(split_target("/callback?code=1"), ("/callback", "code=1"));
        assert_eq!(split_target("/favicon.ico"), ("/favicon.ico", ""));
    }

    /// The whole loopback half, over a real socket: a browser that asks for a favicon
    /// first, a callback that carries the code, a token endpoint that answers, and — the
    /// part a unit test of the parsing cannot reach — a port that is closed again by the
    /// time the exchange goes out.
    #[test]
    fn a_browser_coming_back_completes_the_login_and_frees_the_port() {
        use std::io::{Read, Write};
        use std::net::TcpStream as SyncStream;

        // A token endpoint of our own, so the exchange is exercised without leaving the
        // machine. Leaked because a `Client` names its URLs by `&'static str`, and a test
        // process ending is what reclaims it.
        let token_server = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let token_url: &'static str = Box::leak(
            format!(
                "http://127.0.0.1:{}/token",
                token_server.local_addr().expect("addr").port()
            )
            .into_boxed_str(),
        );
        let token_thread = std::thread::spawn(move || {
            let (mut stream, _) = token_server.accept().expect("the exchange arrives");
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).expect("request");
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let body = r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("answer");
            request
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let mut oauth = client();
        oauth.token_url = token_url;
        let login = runtime
            .block_on(Login::begin(oauth))
            .expect("an ephemeral port binds");
        let url = login.url().to_owned();
        let port: u16 = login
            .redirect_uri
            .rsplit_once(':')
            .and_then(|(_, tail)| tail.split('/').next())
            .and_then(|port| port.parse().ok())
            .expect("the redirect names the port it bound");
        let state = url
            .split("&state=")
            .nth(1)
            .and_then(|tail| tail.split('&').next())
            .expect("the URL carries the state")
            .to_owned();

        let browser = std::thread::spawn(move || {
            let get = |target: &str| {
                let mut stream =
                    SyncStream::connect(("127.0.0.1", port)).expect("the listener is up");
                stream
                    .write_all(
                        format!(
                            "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .expect("request");
                let mut answer = String::new();
                let _ = stream.read_to_string(&mut answer);
                answer
            };
            // Browsers do this, and it must not consume the one request the login has.
            let favicon = get("/favicon.ico");
            let callback = get(&format!("/callback?code=the-code&state={state}"));
            (favicon, callback)
        });

        let http = super::client().expect("an HTTP client");
        let tokens = runtime
            .block_on(login.finish(&http))
            .expect("the login completes");
        assert_eq!(tokens["access_token"], "at");

        let (favicon, callback) = browser.join().expect("the browser thread");
        assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
        assert!(callback.starts_with("HTTP/1.1 200"), "{callback}");
        assert!(callback.contains("Signed in"), "{callback}");

        let request = token_thread.join().expect("the token thread");
        assert!(request.starts_with("POST /token "), "{request}");
        assert!(
            request.contains("grant_type=authorization_code"),
            "{request}"
        );
        assert!(request.contains("code=the-code"), "{request}");
        assert!(request.contains("code_verifier="), "{request}");
        assert!(request.contains("user-agent: Tidemark/"), "{request}");

        // Nothing is listening any more: the port went back before the exchange, which is
        // what lets the vendor's own CLI run its login straight afterwards.
        assert!(
            SyncStream::connect(("127.0.0.1", port)).is_err(),
            "the callback port is still held"
        );
    }

    /// A one-shot executor, so the synchronous callback tests do not each need a runtime
    /// for a future that never yields.
    fn futures_lite_block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(future)
    }
}
