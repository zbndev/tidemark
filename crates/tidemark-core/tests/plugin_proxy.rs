//! A plugin gets no say in the proxy, and no say in certificate validation.
//!
//! A binary of its own, and one test function in it, for the reason `proxy.rs` gives: the
//! proxy is process-wide, so a test that sets it sets it for everything running beside it.
//! The assertions are `proxy.rs`'s, made again through a plugin account — the point being
//! that the plugin path reuses the one client policy rather than building a client of its
//! own that could disagree with it.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use tidemark_core::plugin::provider::{Endpoint, PluginProvider};
use tidemark_core::plugin::schema;
use tidemark_core::providers::http::{self, Proxy};
use tidemark_core::providers::{Credential, Provider};
use tidemark_types::{AccountId, Preferences};

const KEY: &str = "sk-test-not-a-real-key";
const RESPONSE: &str = include_str!("fixtures/plugin/acme-response.json");
const FILE: &str = include_str!("fixtures/plugin/acme.tidemark-provider");

/// A server that answers everything with the fixture and reports the request lines it saw.
fn recording_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port is available");
    let port = listener.local_addr().expect("bound").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || answer(stream, &recorded));
        }
    });
    (port, seen)
}

fn answer(stream: TcpStream, seen: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(&stream);
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
            break;
        }
    }
    seen.lock()
        .expect("not poisoned")
        .push(request.trim().to_owned());
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        RESPONSE.len()
    );
    let mut stream: &TcpStream = &stream;
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(RESPONSE.as_bytes());
    let _ = stream.flush();
}

/// One plugin account pointed at `url`, insecure transport acknowledged the way an account
/// owner would have to acknowledge it.
fn plugin_at(url: &str) -> PluginProvider {
    PluginProvider::new(
        Arc::new(schema::parse(FILE.as_bytes(), &[]).expect("the fixture parses")),
        AccountId::default(),
        Endpoint {
            url: url.to_owned(),
            allow_insecure_http: true,
        },
        Credential::new(KEY),
    )
    .expect("the account builds")
}

#[tokio::test]
async fn the_proxy_policy_is_the_applications_own_and_a_plugin_cannot_opt_out_of_it() {
    let (proxied, through_proxy) = recording_server();
    http::set_proxy(
        Proxy::new(Preferences::PROXY_HTTP, "127.0.0.1", proxied).expect("a usable proxy"),
    );

    // Loopback first: the client that polls a plugin account is built after the proxy was
    // set, and it must still reach a local endpoint directly.
    let (direct, locally) = recording_server();
    let loopback = plugin_at(&format!("http://127.0.0.1:{direct}/usage"));
    loopback.fetch().await.expect("a local endpoint is polled");
    assert_eq!(
        locally.lock().expect("not poisoned").as_slice(),
        ["GET /usage HTTP/1.1"],
        "loopback must be reached directly, in origin form"
    );
    assert!(
        through_proxy.lock().expect("not poisoned").is_empty(),
        "loopback bypasses the shared proxy"
    );

    // And a host that is not this machine goes through it. `quota.invalid` resolves
    // nowhere, so an absolute request line arriving at the proxy at all is the proof.
    let remote = plugin_at("http://quota.invalid/usage");
    let _ = remote.fetch().await;
    assert_eq!(
        through_proxy.lock().expect("not poisoned").as_slice(),
        ["GET http://quota.invalid/usage HTTP/1.1"],
        "a remote host goes through the configured proxy"
    );

    http::set_proxy(Proxy::new(Preferences::PROXY_OFF, "", 0).expect("off is not a failure"));
    assert_eq!(http::proxy(), None);
}
