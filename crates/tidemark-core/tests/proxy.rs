//! What "the proxy is applied" has to mean: the bytes leave through it.
//!
//! An integration test rather than a unit test because the proxy is process-wide, and this
//! file is a process of its own — a `#[test]` in the library would set it for every other
//! test running beside it. One test function for the same reason: two would race over the
//! one value they both set.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use tidemark_core::providers::http::{self, Proxy};
use tidemark_types::Preferences;

/// A server that answers one request with `200 OK` and reports the request line it was
/// sent.
///
/// Enough of an HTTP proxy to prove a client used one: a direct request carries the origin
/// form (`GET /summary`), and only a proxied request carries the absolute form with the
/// scheme and the host in it.
fn one_shot_server() -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port is available");
    let port = listener.local_addr().expect("bound").port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the client connects");
        let mut reader = BufReader::new(&stream);
        let mut request = String::new();
        reader.read_line(&mut request).expect("a request line");
        // Headers to the blank line, so the client is not answered mid-request.
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                break;
            }
        }
        let mut stream: &TcpStream = &stream;
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let _ = stream.flush();
        request.trim().to_owned()
    });
    (port, handle)
}

/// A port nothing is listening on, for a proxy that must never be dialled.
fn dead_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a port to abandon");
    let port = listener.local_addr().expect("bound").port();
    drop(listener);
    port
}

#[tokio::test]
async fn requests_leave_through_the_proxy_and_loopback_never_does() {
    let (proxied, proxy_server) = one_shot_server();
    http::set_proxy(
        Proxy::new(Preferences::PROXY_HTTP, "127.0.0.1", proxied).expect("a usable proxy"),
    );
    let client = http::client().expect("a client builds with a proxy in force");

    // `quota.invalid` resolves nowhere, so reaching `200` at all is the first half of the
    // proof; the absolute request line is the second.
    let response = client
        .get("http://quota.invalid/summary")
        .send()
        .await
        .expect("the proxy answers");
    assert!(response.status().is_success());
    assert_eq!(
        proxy_server.join().expect("the proxy thread finishes"),
        "GET http://quota.invalid/summary HTTP/1.1"
    );

    // Now with a proxy that cannot be dialled at all: a loopback request must still work,
    // which is only true if it never went near it.
    http::set_proxy(
        Proxy::new(Preferences::PROXY_HTTP, "127.0.0.1", dead_port()).expect("a usable proxy"),
    );
    let (direct, local_server) = one_shot_server();
    let client = http::client().expect("client builds");
    let response = client
        .get(format!("http://127.0.0.1:{direct}/status"))
        .send()
        .await
        .expect("loopback is reached without the proxy");
    assert!(response.status().is_success());
    assert_eq!(
        local_server.join().expect("the server thread finishes"),
        "GET /status HTTP/1.1",
        "loopback must be reached directly, in origin form"
    );

    // And off again puts the process back to reading its own environment.
    http::set_proxy(Proxy::new(Preferences::PROXY_OFF, "", 0).expect("off is not a failure"));
    assert_eq!(http::proxy(), None);
}
