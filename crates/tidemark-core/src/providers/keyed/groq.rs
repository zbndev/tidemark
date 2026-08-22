//! Groq, read through its Prometheus metrics endpoint.
//!
//! Ported from CodexBar's `Providers/Groq/GroqUsageFetcher.swift`; the recorded body in
//! `GroqUsageFetcherTests.swift` is the contract, and the rendering numbers are
//! CodexBar's own snapshot-mapping test inputs. Never seen answering: every number in
//! the tests below comes from CodexBar's recorded tests.
//!
//! # The four queries
//!
//! The API-key path issues four **scalar** Prometheus queries in parallel — `async let`
//! in the source, [`tokio::join!`] here — against
//! `{base}/metrics/prometheus/api/v1/query?query=<q>`:
//!
//! - `sum(model_project_id_status_code:requests:rate5m)` — requests per second,
//! - `sum(model_project_id:tokens_in:rate5m)` — input tokens per second,
//! - `sum(model_project_id:tokens_out:rate5m)` — output tokens per second,
//! - `sum(model_project_id:prompt_cache_hits:rate5m)` — cache hits per second.
//!
//! Each answer sums its series' last sample, bare number or numeric string; a series
//! whose sample cannot be read contributes nothing, as the source's `compactMap` drops
//! it. A body whose `status` is not `success` is the source's `apiError`, malformed
//! here. The base URL is optional — `https://api.groq.com/v1` stands in — and the
//! shared reader enforces HTTPS.
//!
//! # Rates, not quotas
//!
//! These are throughput rates over a rolling five minutes, with no limit on the wire:
//! there is nothing to draw a bar against, so this provider draws no window at all —
//! details only, the card renders empty, and that is accepted. CodexBar renders the
//! same numbers as always-empty meters; a Tidemark card says them in rows instead,
//! scaled to the source's per-minute spelling (`120 req/min`, `9000 tok/min`,
//! `180 cache/min`) with the source's own `formatDecimal` rounding, and a cache rate
//! of zero draws no row, exactly as the source suppresses its third meter.
//!
//! # What ships untested
//!
//! Only one scalar body is recorded, so the three token queries are tested through the
//! same parser with no body of their own, and a `status: "error"` body is constructed
//! in the source's own spelling. The 401/403 a rejected key earns is mapped by the
//! shared transport, so it is tested by no unit here.

use super::{HandSpec, OptionSchema, Options, base_url, redact_query};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "groq";

/// Name of the base-URL setting under `[provider.groq]`.
pub const BASE_URL: &str = "base_url";

/// The API root the metrics path appends to. The source's default, verbatim.
const DEFAULT_BASE_URL: &str = "https://api.groq.com/v1";

/// Groq as the settings dialog sees it.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Groq",
    credential_hint: "console.groq.com → API Keys. A Groq API key with metrics access.",
    options: &[OptionSchema {
        name: BASE_URL,
        title: "API URL",
        description: Some(
            "The API root, version path included. Leave unset for api.groq.com itself.",
        ),
        default: DEFAULT_BASE_URL,
        choices: &[],
        required: false,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings. The query
/// URL is resolved here, so a changed base URL takes effect on the next build.
fn build(credential: Credential, options: &Options) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Groq::new(credential, options)?))
}

/// One Groq account: the key, and the one query endpoint four questions share.
pub struct Groq {
    client: reqwest::Client,
    credential: Credential,
    query_url: String,
}

impl Groq {
    /// Builds a client. The metrics path hangs off the API root, resolved once here.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        let base = base_url(options, BASE_URL, DEFAULT_BASE_URL)?;
        Ok(Self {
            client: http::client()?,
            credential,
            query_url: format!("{base}/metrics/prometheus/api/v1/query"),
        })
    }

    /// One scalar query request, built but not sent, so the query string and the
    /// placement of the key are testable without a server.
    fn query_request(&self, query: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(&self.query_url)
            .query(&[("query", query)])
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        // The source issues these with `async let`; joined here the same way, so one
        // slow query does not become four sequential timeouts.
        let (requests, input, output, cache) = tokio::join!(
            self.scalar(REQUESTS_QUERY),
            self.scalar(INPUT_QUERY),
            self.scalar(OUTPUT_QUERY),
            self.scalar(CACHE_QUERY),
        );
        Ok(snapshot(
            &Rates {
                requests: requests?,
                input_tokens: input?,
                output_tokens: output?,
                cache_hits: cache?,
            },
            now,
        ))
    }

    /// Sends one query and reads its scalar answer.
    async fn scalar(&self, query: &str) -> Result<f64, ProviderError> {
        let body = super::request(&self.client, self.query_request(query)?).await?;
        parse_scalar(&body)
    }
}

/// The four queries, verbatim from the source.
const REQUESTS_QUERY: &str = "sum(model_project_id_status_code:requests:rate5m)";
const INPUT_QUERY: &str = "sum(model_project_id:tokens_in:rate5m)";
const OUTPUT_QUERY: &str = "sum(model_project_id:tokens_out:rate5m)";
const CACHE_QUERY: &str = "sum(model_project_id:prompt_cache_hits:rate5m)";

impl fmt::Debug for Groq {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Groq")
            .field("id", &PROVIDER_ID)
            .field("query_url", &self.query_url)
            .finish_non_exhaustive()
    }
}

impl Provider for Groq {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        AccountId::default()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// The four rates, per second, as the queries report them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rates {
    requests: f64,
    input_tokens: f64,
    output_tokens: f64,
    cache_hits: f64,
}

/// Reads one scalar query answer. Pure: the recorded body is reachable from a test.
///
/// A status other than `success` is the source's own `apiError`, malformed here. Each
/// series' last sample is read bare or as a numeric string and summed; a series whose
/// sample cannot be read contributes nothing, as the source's `compactMap` drops it.
fn parse_scalar(body: &str) -> Result<f64, ProviderError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not a Groq metrics answer: {e}")))?;
    let Value::Object(root) = root else {
        return Err(ProviderError::malformed(
            "a Groq metrics answer must be a JSON object",
        ));
    };
    if root.get("status").and_then(Value::as_str) != Some("success") {
        let reason = root
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("query failed");
        return Err(ProviderError::malformed(format!(
            "the Groq metrics query failed: {reason}"
        )));
    }
    let series = root
        .get("data")
        .and_then(|data| data.get("result"))
        .and_then(Value::as_array);
    let mut sum = 0.0;
    for entry in series.into_iter().flatten() {
        let Some(values) = entry.get("value").and_then(Value::as_array) else {
            continue;
        };
        let Some(sample) = values.last() else {
            continue;
        };
        match sample {
            Value::Number(number) => {
                if let Some(read) = number.as_f64().filter(|read| read.is_finite()) {
                    sum += read;
                }
            }
            Value::String(raw) => {
                if let Some(read) = raw.trim().parse::<f64>().ok().filter(|v| v.is_finite()) {
                    sum += read;
                }
            }
            // An object or array where a sample belongs is not a sample at all: the
            // Swift decoder refuses it for the whole body, and so does this port.
            _ => {
                return Err(ProviderError::malformed(
                    "a Groq metrics sample must be a number or a numeric string",
                ));
            }
        }
    }
    Ok(sum)
}

/// Assembles the snapshot. Pure, so the recorded rates render are reachable from a test.
///
/// No window, ever: a throughput rate is not a quota, and the shape for that is details
/// only — the card renders empty, which is accepted.
fn snapshot(rates: &Rates, now: Timestamp) -> Snapshot {
    let mut rows = vec![
        labeled(
            "Requests",
            format!("{} req/min", decimal(rates.requests * 60.0)),
        ),
        labeled(
            "Tokens",
            format!(
                "{} tok/min",
                decimal((rates.input_tokens + rates.output_tokens) * 60.0)
            ),
        ),
    ];
    if rates.cache_hits > 0.0 {
        rows.push(labeled(
            "Cache hits",
            format!("{} cache/min", decimal(rates.cache_hits * 60.0)),
        ));
    }
    Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: AccountId::default(),
        captured_at: now,
        windows: Vec::new(),
        details: vec![DetailSection {
            title: "Throughput".to_owned(),
            rows,
        }],
    }
}

/// The source's `formatDecimal`: whole from a hundred, one fraction digit from ten,
/// two below that.
fn decimal(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Credential;
    use crate::providers::keyed::Options;
    use tidemark_types::{DetailRow, DetailSection, Timestamp};

    /// Recorded by CodexBar, `GroqUsageFetcherTests.swift` — "parses prometheus scalar
    /// response". Two series, `"2.5"` and `"1.5"`, summed to 4.
    const SCALAR: &str = r#"
        {
          "status": "success",
          "data": {
            "result": [
              { "value": [1710000000, "2.5"] },
              { "value": [1710000000, "1.5"] }
            ]
          }
        }
        "#;

    /// The four queries CodexBar issues, verbatim from the fetcher.
    const REQUESTS_QUERY: &str = "sum(model_project_id_status_code:requests:rate5m)";
    const INPUT_QUERY: &str = "sum(model_project_id:tokens_in:rate5m)";
    const OUTPUT_QUERY: &str = "sum(model_project_id:tokens_out:rate5m)";
    const CACHE_QUERY: &str = "sum(model_project_id:prompt_cache_hits:rate5m)";

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    #[test]
    fn the_recorded_scalar_body_sums_its_series() {
        assert_eq!(parse_scalar(SCALAR).expect("parses"), 4.0);
    }

    #[test]
    fn the_recorded_rates_render_as_per_minute_rows() {
        // The rates are CodexBar's own snapshot-mapping test inputs (2, 100, 50, 3 per
        // second), and its assertions spell the rows this port renders: "120 req/min",
        // "9000 tok/min", "180 cache/min".
        let rates = Rates {
            requests: 2.0,
            input_tokens: 100.0,
            output_tokens: 50.0,
            cache_hits: 3.0,
        };
        let snapshot = snapshot(&rates, at(1_785_000_000));
        assert!(
            snapshot.windows.is_empty(),
            "a throughput rate is not a quota; there is no limit to draw a bar against"
        );
        assert_eq!(
            snapshot.details,
            vec![DetailSection {
                title: "Throughput".to_owned(),
                rows: vec![
                    DetailRow {
                        label: "Requests".to_owned(),
                        value: "120 req/min".to_owned(),
                    },
                    DetailRow {
                        label: "Tokens".to_owned(),
                        value: "9000 tok/min".to_owned(),
                    },
                    DetailRow {
                        label: "Cache hits".to_owned(),
                        value: "180 cache/min".to_owned(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn the_recorded_scalar_body_scales_to_the_requests_row() {
        // The body's 4 requests per second through the same renderer.
        let rates = Rates {
            requests: parse_scalar(SCALAR).expect("parses"),
            input_tokens: 0.0,
            output_tokens: 0.0,
            cache_hits: 0.0,
        };
        let snapshot = snapshot(&rates, at(1_785_000_000));
        let row = &snapshot.details[0].rows[0];
        assert_eq!(row.label, "Requests");
        assert_eq!(row.value, "240 req/min");
        // A cache rate of zero draws no row, exactly as the source suppresses its
        // tertiary window.
        assert_eq!(snapshot.details[0].rows.len(), 2);
    }

    #[test]
    fn bodies_that_cannot_be_read_are_refused() {
        // The procedure's canonical malformed bodies, a body that is not JSON at all,
        // and the shape the source refuses: a status other than success — its own
        // spelling of a failed query — and a value array carrying an object.
        for body in [
            "not-json",
            "{\"partial\":",
            r#"{"status":"error","error":"query failed"}"#,
            r#"{"status":"success","data":{"result":[{"value":[{"a":1}]}]}}"#,
        ] {
            let error = parse_scalar(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error}"
            );
        }
    }

    #[test]
    fn a_series_whose_sample_cannot_be_read_is_skipped() {
        // The source's `compactMap`: a non-numeric sample string or an empty value
        // array contributes nothing rather than failing the query.
        let body = r#"
        {
          "status": "success",
          "data": {
            "result": [
              { "value": [1710000000, "2.5"] },
              { "value": [1710000000, "not a number"] },
              { "value": [] },
              { }
            ]
          }
        }
        "#;
        assert_eq!(parse_scalar(body).expect("parses"), 2.5);
    }

    #[test]
    fn fields_this_parser_does_not_read_are_skipped() {
        // The unknown-kind rule: a Prometheus body carries `resultType` beside `result`
        // and CodexBar's own recorded body carries none of it; one more invented field
        // rides along.
        let body = SCALAR.replacen(
            "\"result\": [",
            "\"resultType\": \"vector\", \"future\": {\"kind\": \"daily\"}, \"result\": [",
            1,
        );
        assert_eq!(parse_scalar(&body).expect("parses"), 4.0);
    }

    #[test]
    fn the_four_queries_address_the_prometheus_endpoint_with_a_bearer_key() {
        let groq = Groq::new(Credential::new("gsk_test"), &Options::new()).expect("builds");
        for (query, name) in [
            (REQUESTS_QUERY, "requests"),
            (INPUT_QUERY, "input tokens"),
            (OUTPUT_QUERY, "output tokens"),
            (CACHE_QUERY, "cache hits"),
        ] {
            let request = groq.query_request(query).expect("builds");
            assert_eq!(request.method(), reqwest::Method::GET, "{name}");
            // The query string is percent-encoded on the wire; read it back decoded,
            // which is what the server does with it.
            let url = request.url();
            assert_eq!(
                url.as_str().split('?').next().expect("a path"),
                "https://api.groq.com/v1/metrics/prometheus/api/v1/query",
                "{name}"
            );
            assert_eq!(
                url.query_pairs()
                    .find(|(key, _)| key == "query")
                    .expect("present")
                    .1,
                query,
                "{name}"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer gsk_test",
                "{name}"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json",
                "{name}"
            );
        }
    }

    #[test]
    fn the_base_url_option_moves_the_queries() {
        // The procedure's fourth test: the option resolves, an unset one falls back to
        // the default host.
        let set = Groq::new(
            Credential::new("gsk_test"),
            &Options::from([(
                "base_url".to_owned(),
                "https://proxy.example.com/v1".to_owned(),
            )]),
        )
        .expect("builds");
        let request = set.query_request(REQUESTS_QUERY).expect("builds");
        assert_eq!(
            request.url().as_str().split('?').next().expect("a path"),
            "https://proxy.example.com/v1/metrics/prometheus/api/v1/query"
        );
        assert_eq!(
            request
                .url()
                .query_pairs()
                .find(|(key, _)| key == "query")
                .expect("present")
                .1,
            REQUESTS_QUERY
        );
        let error = Groq::new(
            Credential::new("gsk_test"),
            &Options::from([("base_url".to_owned(), "http://proxy.example.com".to_owned())]),
        )
        .expect_err("a key over plain HTTP to a remote host is refused");
        assert!(matches!(error, ProviderError::Local(_)), "{error}");
    }

    #[test]
    fn the_spec_publishes_one_optional_option() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Groq");
        assert_eq!(SPEC.options.len(), 1);
        assert!(!SPEC.options[0].required, "the default host stands in");
        assert!(build(Credential::new("gsk_test"), &Options::new()).is_ok());
    }

    #[test]
    fn a_groq_client_never_prints_its_credential() {
        let groq = Groq::new(Credential::new("gsk-super-secret"), &Options::new()).expect("builds");
        let rendered = format!("{groq:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
