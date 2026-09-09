//! One plugin account against a local server: the transport boundary, end to end.
//!
//! The unit tests prove the parser and the sandbox. This file proves the parts only a socket
//! can: that the declared header carries the key and nothing else does, that a redirect does
//! not move it, that an oversized body is refused before it is parsed, and that a failed poll
//! leaves the last good reading in place.
//!
//! The proxy half of the boundary is deliberately not here. It is process-wide state, and a
//! test that set it would set it for every test running beside it in this binary — see
//! `plugin_proxy.rs`, which is a binary of its own for that reason, exactly as `proxy.rs`
//! already is for the built-in providers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use tidemark_core::plugin::provider::{Endpoint, PluginProvider};
use tidemark_core::plugin::{Definition, Reading, limits, provider, schema};
use tidemark_core::providers::{Credential, Provider, ProviderError};
use tidemark_core::storage::History;
use tidemark_types::{
    AccountId, Presentation, ProviderState, ProviderStatus, Timestamp, WindowKey,
};

/// A test key. Long enough that the debug recorder's own needle rule applies to it, and
/// obviously not a real one.
const KEY: &str = "sk-test-not-a-real-key";
/// The fixture body, so a test that only needs a reading does not read a file.
const RESPONSE: &str = include_str!("fixtures/plugin/acme-response.json");
/// The fixture definition: the example the author guide ships, unchanged.
const FILE: &str = include_str!("fixtures/plugin/acme.tidemark-provider");

/// A whole plugin file whose parser reports what it can see, for the credential assertion.
///
/// Written out rather than patched into [`FILE`]: a test that edits a TOML literal string
/// block by substring is a test that breaks when the fixture is reformatted.
const REFLECTING_FILE: &str = r#"
format_version = 1

[provider]
id = "com.acme.reflect"
name = "Reflect"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "X-Acme-Key"
api_key_prefix = ""

[parser]
language = "lua54"
source = '''
function parse(response, context)
    local metrics = {}
    for index, key in ipairs(sorted_keys(response)) do
        metrics[index] = { id = "k" .. index, title = key, text = tostring(response[key]) }
    end
    return { metrics = metrics, card = {}, details = {} }
end
'''
"#;

// ---------------------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------------------

/// One request as the server saw it on the wire.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    body: String,
    /// Header names lowercased, values verbatim: what matters here is which name carried
    /// what, and HTTP field names are case-insensitive.
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(seen, _)| seen == name)
            .map(|(_, value)| value.clone())
    }
}

/// What the server answers with next.
#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn json(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.to_owned(),
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            headers: vec![("Location".to_owned(), location.to_owned())],
            body: String::new(),
        }
    }

    fn rate_limited(retry_after: u64) -> Self {
        Self {
            status: 429,
            headers: vec![("Retry-After".to_owned(), retry_after.to_string())],
            body: String::new(),
        }
    }
}

/// A server that records every request and answers with a scripted [`Reply`].
///
/// Raw sockets rather than a dev-dependency, the way `proxy.rs` already does it: what these
/// tests read is the bytes a request was made of, and a framework that normalised them
/// would be standing between the assertion and the thing asserted.
struct Recording {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    reply: Arc<Mutex<Reply>>,
}

impl Recording {
    async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port is available");
        let address = listener.local_addr().expect("bound");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new(reply));
        let served = (Arc::clone(&seen), Arc::clone(&reply));
        // Detached: the listener lives as long as the test process, which is shorter than
        // any of these tests needs it to be. Nothing joins it, so nothing can hang on it.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let (seen, reply) = (Arc::clone(&served.0), Arc::clone(&served.1));
                std::thread::spawn(move || answer(stream, &seen, &reply));
            }
        });
        Self {
            address,
            seen,
            reply,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("not poisoned").clone()
    }

    fn last_request(&self) -> Seen {
        self.requests().pop().expect("the client made a request")
    }

    fn answer_next(&self, reply: Reply) {
        *self.reply.lock().expect("not poisoned") = reply;
    }
}

/// Reads one request off `stream`, records it, and writes the scripted reply.
fn answer(stream: TcpStream, seen: &Mutex<Vec<Seen>>, reply: &Mutex<Reply>) {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 && reader.read_exact(&mut body).is_err() {
        body.clear();
    }

    seen.lock().expect("not poisoned").push(Seen {
        method,
        body: String::from_utf8_lossy(&body).into_owned(),
        headers,
    });

    let reply = reply.lock().expect("not poisoned").clone();
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream: &TcpStream = &stream;
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(reply.body.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------------------
// The accounts
// ---------------------------------------------------------------------------------------

/// The fixture definition with its request declaration rewritten.
///
/// The declaration is three scalar TOML fields, so substituting them is substituting whole
/// lines — unlike the Lua block, which is why [`REFLECTING_FILE`] is written out instead.
fn declaring(method: &str, header: &str, prefix: &str) -> Arc<Definition> {
    let file = FILE
        .replace("method = \"GET\"", &format!("method = \"{method}\""))
        .replace(
            "api_key_header = \"X-Acme-Key\"",
            &format!("api_key_header = \"{header}\""),
        )
        .replace(
            "api_key_prefix = \"\"",
            &format!("api_key_prefix = \"{prefix}\""),
        );
    Arc::new(schema::parse(file.as_bytes(), &[]).expect("the fixture parses"))
}

/// The fixture definition as shipped.
fn definition() -> Arc<Definition> {
    Arc::new(schema::parse(FILE.as_bytes(), &[]).expect("the fixture parses"))
}

/// One plugin account over `server`, with `method`, `header` and `prefix` declared.
async fn plugin_over(
    server: &Recording,
    method: &str,
    header: &str,
    prefix: &str,
) -> PluginProvider {
    account(declaring(method, header, prefix), &server.url("/usage"))
}

/// One plugin account over `server` with a parser of its own.
async fn plugin_over_source(server: &Recording, file: &str) -> PluginProvider {
    account(
        Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses")),
        &server.url("/usage"),
    )
}

/// The one constructor the three above share.
///
/// `allow_insecure_http` is true because every local server here is `http://127.0.0.1`, and
/// that acknowledgement is the account owner's — which is the point of it being a field on
/// the endpoint rather than something the definition could ask for.
fn account(definition: Arc<Definition>, url: &str) -> PluginProvider {
    PluginProvider::new(
        definition,
        AccountId::default(),
        Endpoint {
            url: url.to_owned(),
            allow_insecure_http: true,
        },
        Credential::new(KEY),
    )
    .expect("the account builds")
}

/// The pure reading of one body at a stated time, for the tests about what a reading does
/// once it exists rather than about how it arrived.
fn reading(definition: &Definition, body: &str, at: i64) -> Reading {
    provider::render(
        definition,
        body.as_bytes(),
        &AccountId::default(),
        Timestamp::from_unix(at).expect("plausible"),
    )
    .expect("the fixture renders")
}

/// The presentation of one body, so a test can fill a `ProviderStatus` the way the daemon
/// does.
fn plugin_presentation(definition: &Definition, body: &str) -> Presentation {
    reading(definition, body, 1_787_000_000).presentation
}

/// The window the fixture declares, which is also the segment identity in history.
fn month() -> WindowKey {
    WindowKey::named("month")
}

// ---------------------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_get_sends_the_declared_header_and_nothing_that_was_not_declared() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
    provider.fetch().await.expect("polls");

    let seen = server.last_request();
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.header("x-acme-key").as_deref(), Some(KEY));
    assert_eq!(seen.header("accept").as_deref(), Some("application/json"));
    assert_eq!(
        seen.header("user-agent"),
        Some(tidemark_types::user_agent())
    );
    assert!(seen.header("cookie").is_none());
    assert!(
        seen.header("authorization").is_none(),
        "only the declared header carries the key"
    );
}

/// A plugin's `Snapshot` carries the history window but cannot carry the semantic card
/// order. Going through the provider trait must therefore keep the companion presentation:
/// rebuilding one from the snapshot would silently drop the value widget below.
#[tokio::test]
async fn a_plugin_poll_keeps_the_card_layout_declared_by_its_parser() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;

    let reading = provider.fetch_reading().await.expect("polls");

    assert_eq!(reading.presentation.card.len(), 2);
    assert_eq!(reading.presentation.card[0].kind, "gauge");
    assert_eq!(reading.presentation.card[1].kind, "value");
    assert_eq!(reading.presentation.card[1].metric, "month-requests");
}

#[tokio::test]
async fn an_empty_post_sends_no_body_and_no_content_type_a_plugin_chose() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "POST", "Authorization", "Bearer ").await;
    provider.fetch().await.expect("polls");

    let seen = server.last_request();
    assert_eq!(seen.method, "POST");
    assert!(seen.body.is_empty(), "a plugin has no way to send a body");
    assert_eq!(
        seen.header("content-length").as_deref().unwrap_or("0"),
        "0",
        "an empty body is either declared as zero-length or not declared at all"
    );
    assert!(
        seen.header("transfer-encoding").is_none(),
        "and it is never chunked, which would be a body being streamed"
    );
    assert_eq!(
        seen.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(
        seen.header("content-type").is_none(),
        "a body a plugin cannot send has no type it could choose"
    );
}

#[tokio::test]
async fn a_redirect_to_another_origin_is_not_followed_and_the_key_does_not_move() {
    let elsewhere = Recording::start(Reply::json(RESPONSE)).await;
    let server = Recording::start(Reply::redirect(302, &elsewhere.url("/usage"))).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;

    let error = provider.fetch().await.expect_err("a 302 is not a reading");
    assert!(
        matches!(error, ProviderError::Http { status: 302 }),
        "{error:?}"
    );
    assert!(
        elsewhere.requests().is_empty(),
        "the secret header must not follow a redirect to another origin"
    );
}

#[tokio::test]
async fn a_body_larger_than_the_limit_is_refused_without_being_parsed() {
    let oversized = format!("{{\"pad\":\"{}\"}}", "x".repeat(limits::RESPONSE_BYTES));
    let server = Recording::start(Reply::json(&oversized)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;

    let error = provider
        .fetch()
        .await
        .expect_err("the body is over the bound");
    assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
    assert!(
        error.to_string().contains("response body"),
        "the message names the bound that was hit: {error}"
    );
}

#[tokio::test]
async fn each_status_maps_onto_the_state_the_engine_already_knows() {
    for (reply, expected) in [
        (Reply::status(401), "credential"),
        (Reply::status(403), "credential"),
        (Reply::rate_limited(90), "rate-limited"),
        (Reply::status(500), "http"),
        (Reply::json("{ not json"), "malformed"),
    ] {
        let server = Recording::start(reply).await;
        let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
        let error = provider
            .fetch()
            .await
            .expect_err("none of these is a reading");
        let actual = match &error {
            ProviderError::Credential { .. } => "credential",
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(
                    *retry_after,
                    Some(90),
                    "the wait the provider asked for is kept"
                );
                "rate-limited"
            }
            ProviderError::Http { .. } => "http",
            ProviderError::Malformed(_) => "malformed",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, expected, "{error:?}");
    }
}

#[tokio::test]
async fn the_lua_input_never_contains_the_credential() {
    // The parser here reports every top-level key it can see, so if the credential or a
    // request header had been handed to Lua it would appear in the metric titles.
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over_source(&server, REFLECTING_FILE).await;
    let snapshot = provider.fetch().await.expect("polls");
    let rendered = format!("{snapshot:?}");
    assert!(!rendered.contains(KEY), "the key reached the sandbox");
    assert!(!rendered.to_lowercase().contains("x-acme-key"));
    assert!(
        !format!("{provider:?}").contains(KEY),
        "and it is not in the provider's own debug form either"
    );
}

#[tokio::test]
async fn a_malformed_response_leaves_the_previous_reading_in_place() {
    // Two polls of one account, the way `provider_to_history.rs` drives a built-in provider:
    // the first is a reading, the second is nonsense, and the card must still show the first.
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let mut history = History::in_memory().expect("in-memory history");
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;

    let first = provider.fetch().await.expect("a reading");
    history.ingest(&first).expect("ingests");
    let mut status = ProviderStatus::pending(&first.provider, &first.account);
    status.set_reading(&first, plugin_presentation(&definition(), RESPONSE));

    server.answer_next(Reply::json("{ not json"));
    let error = provider.fetch().await.expect_err("nonsense");
    status.set_state(ProviderState::Malformed, Some(error.to_string()));

    assert_eq!(
        status.windows.len(),
        1,
        "the last good windows survive the failure"
    );
    assert!(status.presentation.is_some(), "and so does its layout");
    assert_eq!(
        history
            .current_points(
                first.provider.as_str(),
                first.account.as_str(),
                &first.windows[0].key
            )
            .expect("readable")
            .len(),
        1,
        "and no history point was fabricated from the failure"
    );
}

#[tokio::test]
async fn the_published_wire_form_carries_no_credential() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
    let snapshot = provider.fetch().await.expect("a reading");
    let mut status = ProviderStatus::pending(&snapshot.provider, &snapshot.account);
    status.set_reading(&snapshot, plugin_presentation(&definition(), RESPONSE));

    let encoded = zvariant::to_bytes(
        zvariant::serialized::Context::new_dbus(zvariant::LE, 0),
        &status,
    )
    .expect("the published shape encodes");
    let haystack = encoded.bytes();
    assert!(
        !haystack
            .windows(KEY.len())
            .any(|window| window == KEY.as_bytes()),
        "no credential may appear anywhere in what the daemon publishes"
    );
}

// ---------------------------------------------------------------------------------------
// The window a plugin declares, in the models the daemon already has
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_window_accumulates_history_across_polls_like_any_other() {
    // Two readings with different consumption, one segment, two points — and the segment
    // identity is the plugin's own window key, so a renamed title does not split it.
    //
    // Rendered at stated times rather than fetched twice: `History::ingest` refuses an
    // observation no newer than the last one, and two polls of a local server land in the
    // same second. The transport is already proved above; what is proved here is the seam.
    let definition = definition();
    let mut history = History::in_memory().expect("in-memory history");
    let start = 1_787_000_000;

    let first = reading(&definition, RESPONSE, start);
    history.ingest(&first.snapshot).expect("ingests");

    // The consumption moves, not just the leftovers: `used_percent` is what a segment is
    // tracked by, and a reading that repeats it is deliberately not stored twice.
    let later = RESPONSE
        .replace("\"total\": \"12.5\"", "\"total\": \"31.0\"")
        .replace("\"remaining\": 37.5", "\"remaining\": 19.0");
    let second = reading(&definition, &later, start + 300);
    let report = history.ingest(&second.snapshot).expect("ingests");

    assert!(report.stale.is_empty(), "{report:?}");
    assert_eq!(
        report.segments_opened().count(),
        0,
        "rising consumption is not a rollover: {report:?}"
    );
    assert_eq!(
        history
            .segment_count("com.acme.quota", "default", &month())
            .expect("counts"),
        1,
        "one window in steady use is one segment"
    );
    let points = history
        .current_points("com.acme.quota", "default", &month())
        .expect("reads");
    assert_eq!(points.len(), 2);
    assert!(
        points[0].used_percent < points[1].used_percent,
        "both readings were stored, in order: {points:?}"
    );
}

#[tokio::test]
async fn a_plugin_window_can_be_opted_into_notifications_by_its_key() {
    // `Engine::set_window_notify` accepts a window name only when the account's published
    // status carries a window under that key, so what has to hold here is that the key the
    // plugin declared is the key that reaches `ProviderStatus.windows` — not a title, not a
    // length-derived key it never chose.
    let definition = definition();
    let reading = reading(&definition, RESPONSE, 1_787_000_000);
    let mut status = ProviderStatus::pending(&reading.snapshot.provider, &reading.snapshot.account);
    status.set_reading(&reading.snapshot, reading.presentation.clone());

    assert_eq!(
        status
            .windows
            .iter()
            .map(|window| window.key.as_str())
            .collect::<Vec<_>>(),
        vec!["month"],
        "the declared key is what a notification preference is filed under"
    );
    assert_eq!(status.windows[0].title, "Monthly cost");
    assert!(
        status.windows[0].resets_at.is_some(),
        "and the reset the plugin parsed came with it"
    );
    // A metric that declared no window contributes none: the fixture's `month-requests` is
    // on the card and nowhere in this list.
    assert!(
        reading
            .presentation
            .metrics
            .iter()
            .any(|metric| metric.id == "month-requests"),
        "the fixture does carry the second metric"
    );
}

#[test]
fn the_same_fixture_produces_the_same_typed_result_on_every_platform() {
    // The whole reason `pairs` and `math.random` are gone: two platforms iterating one JSON
    // object in two orders would publish two card orders. So this asserts the exact ids, the
    // exact order and the exact numbers, and a platform that disagrees fails here rather than
    // on somebody's machine.
    let reading = reading(&definition(), RESPONSE, 1_788_870_896);

    let ids: Vec<&str> = reading
        .presentation
        .metrics
        .iter()
        .map(|metric| metric.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["month-cost", "month-requests", "model-sonnet-5"],
        "the model with a null cap is left out, and the rest keep the order the parser built"
    );

    let card: Vec<(&str, &str)> = reading
        .presentation
        .card
        .iter()
        .map(|widget| (widget.kind.as_str(), widget.metric.as_str()))
        .collect();
    assert_eq!(card, [("gauge", "month-cost"), ("value", "month-requests")]);
    let details: Vec<(&str, &str)> = reading.presentation.details[0]
        .items
        .iter()
        .map(|widget| (widget.kind.as_str(), widget.metric.as_str()))
        .collect();
    assert_eq!(
        details,
        [("ratio", "month-cost"), ("ratio", "model-sonnet-5")],
        "the detail section has an order of its own, and it is the parser's"
    );

    // The exact numbers, including the ones that came in as a string and as a subtraction.
    let cost = reading.presentation.metric("month-cost").expect("present");
    assert_eq!(cost.value, Some(12.5), "a numeric string is a number");
    assert_eq!(cost.maximum, Some(50.0));
    assert_eq!(cost.used_percent, Some(25.0));
    let requests = reading
        .presentation
        .metric("month-requests")
        .expect("present");
    assert_eq!(requests.value, Some(3760.0), "limit less remaining");
    assert_eq!(requests.used_percent, Some(75.2));

    assert_eq!(reading.snapshot.windows.len(), 1);
    assert_eq!(reading.snapshot.windows[0].key.as_str(), "month");
    assert_eq!(
        reading.snapshot.windows[0].resets_at.map(|at| at.as_unix()),
        Some(1_790_812_800),
        "the RFC 3339 reset is one instant on every platform"
    );
}
