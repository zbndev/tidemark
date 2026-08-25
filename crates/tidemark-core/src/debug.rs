//! The raw-response log: every provider exchange, written down verbatim, when the user
//! asks for one.
//!
//! Off unless `[debug] raw_responses = true` in `config.toml`, and read only when the
//! daemon starts. A reading that is wrong is wrong somewhere between the bytes on the
//! wire and the number on the card, and by the time the card is wrong the bytes are gone:
//! this is the only place they still exist. NDJSON, one line per request, so a body can
//! go to `jq` or into a bug report exactly as it arrived — the body is stored as a
//! *string*, never re-encoded, because a malformed response is the interesting case and
//! re-encoding it would repair the evidence.
//!
//! # What is deliberately not written
//!
//! Request headers, ever: the credential rides in one. The query string of a URL, for the
//! same reason — a few providers take the key there. And the OAuth token endpoints do not
//! come through here at all, because their response body *is* the credential.
//!
//! # Why the sink is process-wide rather than a parameter
//!
//! For the reason [`crate::providers::http`] gives about the proxy, which this deliberately
//! mirrors: whether this process writes a debug log is a property of the process, it is
//! written once at startup, and forty-odd provider clients would otherwise forward the
//! same value, unchanged, from the same source.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Directory the log lives in, under the data directory.
pub const DEBUG_DIR: &str = "debug";

/// File name of the raw-response log.
pub const RESPONSE_LOG: &str = "responses.ndjson";

/// Size at which the log rolls over to `responses.ndjson.1`.
///
/// One previous file is kept, so the ceiling on disk is twice this. At forty accounts on
/// a five-minute interval that is days of history, which is the horizon a "sometimes it
/// reads 100%" report needs — long enough to catch the next occurrence, short enough that
/// a switch left on does not quietly fill a home directory.
const ROTATE_AT: u64 = 16 * 1024 * 1024;

/// The open log, or `None` for "the user did not ask for one".
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// What was sent. Headers are absent by construction — see the module docs.
#[derive(Debug, Clone, Copy)]
pub struct Sent<'a> {
    /// HTTP method.
    pub method: &'a str,
    /// URL, whose query string this module strips before writing.
    pub url: &'a str,
    /// The request body, for the providers whose quota call is a POST. Without it the
    /// JSON-RPC to `agy` is unreadable.
    pub body: Option<&'a str>,
}

impl<'a> Sent<'a> {
    /// A GET, which is what all but a handful of quota endpoints are.
    pub const fn get(url: &'a str) -> Self {
        Self {
            method: "GET",
            url,
            body: None,
        }
    }
}

/// A built request's own description of itself, taken before the request is consumed so
/// that the answer can be written down beside it.
#[derive(Debug, Clone)]
pub struct Recorded {
    method: String,
    url: String,
    body: Option<String>,
}

impl Recorded {
    /// Describes a request, or nothing at all when the log is off and no one would read
    /// it — which is the state this costs anything in.
    pub fn of(request: &reqwest::Request) -> Option<Self> {
        if !enabled() {
            return None;
        }
        Some(Self {
            method: request.method().as_str().to_owned(),
            url: request.url().as_str().to_owned(),
            body: request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        })
    }

    /// What to hand [`Exchange`].
    pub fn sent(&self) -> Sent<'_> {
        Sent {
            method: &self.method,
            url: &self.url,
            body: self.body.as_deref(),
        }
    }
}

/// What came back.
#[derive(Debug, Clone, Copy)]
pub enum Answer<'a> {
    /// A response whose body was read, whatever that body turns out to mean.
    Body {
        /// The status that carried it.
        status: u16,
        /// The body, verbatim.
        body: &'a str,
    },
    /// A response refused on its status before the body was read.
    Refused {
        /// The status that refused it.
        status: u16,
    },
    /// No response at all: DNS, connection, TLS, timeout.
    Failed {
        /// The failure, already rendered — and already redacted, where the caller
        /// redacts.
        error: &'a str,
    },
}

/// One provider request and whatever came back from it.
#[derive(Debug, Clone, Copy)]
pub struct Exchange<'a> {
    /// Slug of the provider that made the request, so a line is greppable by the card it
    /// belongs to rather than only by host.
    pub provider: &'a str,
    /// What was sent.
    pub sent: Sent<'a>,
    /// What came back.
    pub answer: Answer<'a>,
}

/// Starts writing every exchange to `<data_dir>/debug/responses.ndjson`, and says where.
///
/// Appends to an existing file: the whole point is to still hold yesterday's evidence
/// after a restart.
pub fn enable(data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = data_dir.join(DEBUG_DIR).join(RESPONSE_LOG);
    let sink = Sink::open(path.clone())?;
    *lock() = Some(sink);
    Ok(path)
}

/// Stops writing and closes the log.
pub fn disable() {
    *lock() = None;
}

/// Whether an exchange handed to [`record`] would be written.
///
/// For a caller deciding whether to pay for a string it would otherwise not build.
pub fn enabled() -> bool {
    lock().is_some()
}

/// Writes one exchange, or does nothing at all when the log is off.
///
/// A write that fails closes the log and says so once. The alternative — reporting per
/// poll — turns a full disk into a second problem, and this is a debugging aid: it must
/// never be able to take polling down with it.
pub fn record(exchange: Exchange<'_>) {
    let mut guard = lock();
    let Some(sink) = guard.as_mut() else {
        return;
    };
    if let Err(error) = sink.write(&line(&exchange)) {
        *guard = None;
        tracing::error!(%error, "the raw-response log could not be written; it is now off");
    }
}

/// The open file and how much has gone into it.
#[derive(Debug)]
struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

impl Sink {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            written,
        })
    }

    fn write(&mut self, line: &str) -> std::io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        // Flushed by `File` on every write; nothing is buffered here on purpose, because
        // the run this log exists to explain is the one that ends in a crash.
        self.written = self
            .written
            .saturating_add(line.len() as u64)
            .saturating_add(1);
        if self.written >= ROTATE_AT {
            *self = Self::rolled(&self.path)?;
        }
        Ok(())
    }

    /// The log renamed aside and a fresh one opened in its place.
    fn rolled(path: &Path) -> std::io::Result<Self> {
        std::fs::rename(path, previous(path))?;
        Self::open(path.to_owned())
    }
}

/// Where a rolled-over log goes: `responses.ndjson.1`, beside the live one.
fn previous(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// One NDJSON line. Pure, so the shape is testable without a file.
fn line(exchange: &Exchange<'_>) -> String {
    let mut entry = serde_json::Map::new();
    entry.insert("at".to_owned(), now().into());
    entry.insert("provider".to_owned(), exchange.provider.into());
    entry.insert("method".to_owned(), exchange.sent.method.into());
    entry.insert("url".to_owned(), redact(exchange.sent.url).into());
    entry.insert(
        "request_body".to_owned(),
        match exchange.sent.body {
            Some(body) => body.into(),
            None => serde_json::Value::Null,
        },
    );
    match exchange.answer {
        Answer::Body { status, body } => {
            entry.insert("status".to_owned(), status.into());
            entry.insert("body".to_owned(), body.into());
        }
        Answer::Refused { status } => {
            entry.insert("status".to_owned(), status.into());
            entry.insert("body".to_owned(), serde_json::Value::Null);
        }
        Answer::Failed { error } => {
            entry.insert("error".to_owned(), error.into());
        }
    }
    serde_json::Value::Object(entry).to_string()
}

/// The URL without its query string, which is where a few providers carry the key.
fn redact(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => url.to_owned(),
    }
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn lock() -> MutexGuard<'static, Option<Sink>> {
    SINK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange<'a>(url: &'a str, answer: Answer<'a>) -> Exchange<'a> {
        Exchange {
            provider: "opencodego",
            sent: Sent {
                method: "GET",
                url,
                body: None,
            },
            answer,
        }
    }

    fn parsed(line: &str) -> serde_json::Value {
        serde_json::from_str(line).expect("every written line is one JSON object")
    }

    #[test]
    fn a_body_is_written_as_a_string_rather_than_re_encoded() {
        // The malformed response is the one worth having, and a re-encode would repair
        // it on the way to disk.
        let raw = "{\"used\": 100,}";
        let entry = parsed(&line(&exchange(
            "https://opencode.ai/api/usage",
            Answer::Body {
                status: 200,
                body: raw,
            },
        )));
        assert_eq!(entry["body"], serde_json::Value::String(raw.to_owned()));
        assert_eq!(entry["status"], 200);
        assert_eq!(entry["provider"], "opencodego");
        assert_eq!(entry["request_body"], serde_json::Value::Null);
    }

    #[test]
    fn a_key_carried_in_the_query_string_never_reaches_the_log() {
        let entry = parsed(&line(&exchange(
            "https://api.example/usage?api_key=sk-secret&x=1",
            Answer::Body {
                status: 200,
                body: "{}",
            },
        )));
        assert_eq!(entry["url"], "https://api.example/usage?<redacted>");
        assert!(!entry.to_string().contains("sk-secret"), "{entry}");
    }

    #[test]
    fn a_refusal_and_a_transport_failure_are_told_apart() {
        let refused = parsed(&line(&exchange(
            "https://api.example/usage",
            Answer::Refused { status: 429 },
        )));
        assert_eq!(refused["status"], 429);
        assert_eq!(refused["body"], serde_json::Value::Null);
        assert_eq!(refused.get("error"), None);

        let failed = parsed(&line(&exchange(
            "https://api.example/usage",
            Answer::Failed {
                error: "connection refused",
            },
        )));
        assert_eq!(failed["error"], "connection refused");
        assert_eq!(failed.get("status"), None);
    }

    /// Answers exactly one request with one JSON body, then closes.
    fn one_request_server(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request accepted");
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header read");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            drop(reader);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("response written");
        });
        (format!("http://{address}"), server)
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(future)
    }

    /// A temporary directory that removes itself, named so two suites can run at once.
    #[derive(Debug)]
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SERIAL: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "tidemark-debug-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Exercises the file directly rather than through the process-wide sink: while the
    /// sink is on, every other provider test in this binary writes to it too, so line
    /// counts are only deterministic here.
    #[test]
    fn the_log_appends_a_line_at_a_time_and_rolls_over_at_the_cap() {
        let dir = TestDir::new();
        let path = dir.0.join(DEBUG_DIR).join(RESPONSE_LOG);
        let mut sink = Sink::open(path.clone()).expect("a writable log");

        for index in 0..3 {
            sink.write(&format!("{{\"n\":{index}}}")).expect("written");
        }
        let written = std::fs::read_to_string(&path).expect("the log exists");
        assert_eq!(written.lines().count(), 3, "{written}");
        assert_eq!(written.lines().last(), Some("{\"n\":2}"));

        // One oversized line takes the file past the cap, which must move it aside and
        // start a fresh one rather than letting it grow without bound.
        let huge = "x".repeat(usize::try_from(ROTATE_AT).expect("the cap fits a usize"));
        sink.write(&huge).expect("written");
        assert_eq!(
            std::fs::read_to_string(&path).expect("a fresh log"),
            "",
            "the live log starts empty after a roll-over"
        );
        assert_eq!(
            std::fs::read_to_string(previous(&path))
                .expect("the rolled-aside log")
                .lines()
                .count(),
            4
        );

        // Reopening appends rather than truncating: the evidence from before a restart is
        // the reason this file exists.
        let mut reopened = Sink::open(path.clone()).expect("reopened");
        reopened.write("{\"after\":\"restart\"}").expect("written");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the log exists"),
            "{\"after\":\"restart\"}\n"
        );
    }

    /// The sink reached the way a provider reaches it: through the transport every keyed
    /// provider shares. Asserts only on its own line, because the sink is process-wide and
    /// the rest of this binary's tests keep polling while it is on.
    #[test]
    fn an_exchange_reaches_the_log_through_the_transport_every_provider_shares() {
        let dir = TestDir::new();
        let served = "{\"limits\":[{\"used\":7}]}";
        let (url, server) = one_request_server(served);

        let path = enable(&dir.0).expect("a writable log");
        assert!(enabled());
        let fetched = block_on(async {
            let client = reqwest::Client::new();
            let request = client
                .get(format!("{url}/usage"))
                .query(&[("api_key", "sk-secret")])
                .build()
                .expect("a built request");
            crate::providers::keyed::request("opencodego", &client, request).await
        });
        disable();
        assert!(!enabled());

        server.join().expect("the server thread finished");
        assert_eq!(fetched.expect("the server answered"), served);

        let written = std::fs::read_to_string(&path).expect("the log exists");
        let entry = written
            .lines()
            .map(parsed)
            .find(|entry| entry["provider"] == "opencodego")
            .expect("the exchange this test made");
        assert_eq!(entry["method"], "GET");
        assert_eq!(entry["status"], 200);
        assert_eq!(entry["body"], served);
        assert_eq!(entry["url"], format!("{url}/usage?<redacted>"));
        assert!(!written.contains("sk-secret"), "{written}");
    }
}
