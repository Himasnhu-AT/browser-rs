//! Unit 12's differential corpus: the HTTP driver replayed against the same
//! local fixture Python's `the reference implementation` was recorded against.
//!
//! `tools/corpus/u12/generate.py` starts two loopback fixture servers, drives
//! every matrix row through the real `Fetcher` / `FetcherSession`, and records
//! what it saw into `testdata/u12_http.jsonl`. This test starts the *same*
//! fixture — the Rust server in `u12_support` interprets the very JSON table the
//! Python one does — replays the same rows through `browser_http`, and
//! compares field by field.
//!
//! No Python and no public network at `cargo test` time.
//!
//! ## The house rule
//!
//! A row the port cannot reproduce is never deleted and never skipped: it is
//! listed in [`DIVERGENCES`] with a written reason, and the test **fails if it
//! ever starts matching**. Raising the pass rate by shrinking the corpus is not
//! available.

#![cfg(feature = "impersonate")]

mod u12_support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use browser_core::types::{FollowRedirects, RedirectMode};
use browser_http::backend::Method;
use browser_http::config::{Body, FetcherConfig, Impersonate, RequestOverrides};
use browser_http::error::FetchError;
use browser_http::response::Response;
use browser_http::session::{AsyncFetcherClient, AsyncFetcherSession, SessionBuilder};
use serde_json::{json, Value};

use u12_support::{encode_hex, start_pair, Fixture};

// ---------------------------------------------------------------------------
// Declared divergences
// ---------------------------------------------------------------------------

/// `(row id, observation index, field, reason)`.
///
/// Every entry is asserted to still differ. Nothing here is a skip: if the port
/// ever reproduces one of these the test fails and the entry must be removed.
const DIVERGENCES: &[(&str, usize, &str, &str)] = &[
    (
        "charset-unknown-label",
        0,
        "outcome",
        "Python raises LookupError('unknown encoding') out of Selector.__init__ when \
         Content-Type names a charset the codec registry does not know; the port warns \
         and decodes as UTF-8 instead (docs/DIVERGENCES.md, `decode_body`). Recorded as \
         a Python failure, reproduced as a Rust success.",
    ),
    (
        "redirect-toftp-safe",
        0,
        "error_class",
        "curl checks the SSRF rule before the protocol allow-list, so an `ftp://` \
         Location on a private host reports `ssrf`; the port refuses every non-HTTP(S) \
         redirect target first and reports `protocol`. Both refuse the hop.",
    ),
    (
        "redirect-toftp-true",
        0,
        "error_class",
        "curl_cffi's build enables the FTP protocol in redirects, so `follow_redirects=True` \
         *dials* ftp://127.0.0.1:9 and fails to connect; the port refuses any non-HTTP(S) \
         redirect target in every mode (curl's own CURLOPT_REDIR_PROTOCOLS default).",
    ),
    (
        "redirect-close-safe",
        0,
        "outcome",
        "curl only runs the `safe` private-address check when a redirect needs a NEW \
         connection, so a same-origin hop over a `Connection: close` reply is checked \
         and refused while the identical hop over a kept-alive connection is not. The \
         port exempts same-origin hops outright — it does not control the pool and \
         cannot see connection reuse — so it follows this one.",
    ),
    (
        "status-299",
        0,
        "reason",
        "hyper keeps a non-canonical reason phrase in `hyper::ext::ReasonPhrase`, a type \
         this crate cannot name without taking a direct hyper dependency (wreq does not \
         re-export it, and the workspace pins the dependency set). `Response::reason` \
         therefore reports the canonical phrase, falling back to `StatusText`, so a \
         server that sends `299 Totally Fine` is read as `Unknown Status Code`. Every \
         status with a canonical phrase — which is every status the corpus exercises \
         besides these two — matches exactly.",
    ),
    (
        "status-222",
        0,
        "reason",
        "As `status-299`: an EMPTY reason phrase cannot be distinguished from a missing \
         one either, and reads as StatusText's `Unknown Status Code`.",
    ),
    (
        "encoding-gzip",
        0,
        "headers",
        "wreq's transparent decoder removes `Content-Encoding` and `Content-Length` once \
         it has decoded the body, on the grounds that neither still describes what \
         `body()` holds; curl leaves both in place. The decoded bytes, the length and \
         everything else match. Re-adding them would mean turning wreq's decompression \
         off and decoding here, which needs dependencies this crate does not have.",
    ),
    (
        "encoding-deflate",
        0,
        "headers",
        "As `encoding-gzip`: `Content-Encoding: deflate` and `Content-Length` are dropped \
         by wreq's decoder.",
    ),
    (
        "encoding-br",
        0,
        "headers",
        "As `encoding-gzip`: `Content-Encoding: br` and `Content-Length` are dropped by \
         wreq's decoder. The brotli body itself decodes byte for byte.",
    ),
    (
        "encoding-gzip-chunked",
        0,
        "headers",
        "As `encoding-gzip`: `Content-Encoding` is dropped by wreq's decoder. \
         `Transfer-Encoding: chunked` survives on both sides.",
    ),
    (
        "encoding-deflate-raw",
        0,
        "outcome",
        "A `Content-Encoding: deflate` body sent as a RAW deflate stream (no zlib header) \
         is accepted by libcurl, which sniffs the two forms; wreq decodes `deflate` \
         strictly as zlib and errors. RFC 9110 says `deflate` means the zlib format, so \
         the fixture is the non-conforming party — but Python accepts it and the port \
         does not, and that is a difference worth recording rather than hiding.",
    ),
    (
        "echo-cookies",
        0,
        "body_hex",
        "The `Cookie:` header carries the same pairs in a different order: libcurl emits \
         its cookie list in reverse insertion order (a list-prepend artifact) while the \
         jar here emits insertion order. RFC 6265 §5.4 makes the order non-semantic, and \
         mimicking a linked-list artifact is not worth pinning; the `echo` check compares \
         the pair SET and passes.",
    ),
    (
        "echo-cookies",
        0,
        "text",
        "As `echo-cookies.body_hex`: the decoded body differs only in `Cookie:` ordering.",
    ),
    (
        "session-cookie-multi",
        2,
        "body_hex",
        "As `echo-cookies.body_hex`, for three jar cookies replayed to \
         `/cookie/echo-path`: curl sends `h=8; g=7; f=6`, the port sends `f=6; g=7; h=8`.",
    ),
    (
        "session-cookie-multi",
        2,
        "text",
        "As `session-cookie-multi[2].body_hex`.",
    ),
];

/// `(row id, observation index, field, the JSON-string value the port may report
/// instead, reason)`.
///
/// Not a divergence and not a skip: a value the *host* decides, where the corpus
/// still asserts everything that is about the port. Both sides must still agree
/// on the outcome; only this one field may take either shape.
const UNPINNABLE: &[(&str, usize, &str, &str, &str)] = &[
    (
        "redirect-tolinklocal-true",
        0,
        "error_class",
        "connect",
        "Dialling 169.254.169.254 with the guard off is decided by the host's routing \
         table, not by the port: a machine with a link-local route blackholes the SYN \
         and the 3 s budget expires (`timeout`, what the corpus recorded), a machine \
         without one gets an immediate ENETUNREACH (`connect`). Both are the same \
         refusal to reach a link-local address; the important half — that \
         `follow_redirects=\"safe\"` reports `ssrf` for the identical hop — is pinned \
         exactly by `redirect-tolinklocal-safe`.",
    ),
    (
        "redirect-toprivate-true",
        0,
        "error_class",
        "connect",
        "As `redirect-tolinklocal-true`, for 10.1.2.3.",
    ),
];

fn unpinnable(id: &str, index: usize, field: &str) -> Option<&'static str> {
    UNPINNABLE
        .iter()
        .find(|(row, at, name, _, _)| *row == id && *at == index && *name == field)
        .map(|(_, _, _, alternative, _)| *alternative)
}

fn divergence(id: &str, index: usize, field: &str) -> Option<&'static str> {
    DIVERGENCES
        .iter()
        .find(|(row, at, name, _)| *row == id && *at == index && *name == field)
        .map(|(_, _, _, reason)| *reason)
}

// ---------------------------------------------------------------------------
// Corpus plumbing
// ---------------------------------------------------------------------------

fn testdata(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

fn placeholderise(text: &str, port_a: u16, port_b: u16) -> String {
    text.replace(&format!("http://127.0.0.1:{port_a}"), "{A}")
        .replace(&format!("http://127.0.0.1:{port_b}"), "{B}")
        .replace(&format!("127.0.0.1:{port_a}"), "{HOSTA}")
        .replace(&format!("127.0.0.1:{port_b}"), "{HOSTB}")
}

/// Map a [`FetchError`] onto the same small vocabulary `generate.py` maps
/// libcurl's error text onto.
fn error_class(error: &FetchError) -> &'static str {
    match error {
        FetchError::UnsafeRedirect { .. } => "ssrf",
        FetchError::UnsupportedRedirectScheme { .. } => "protocol",
        FetchError::TooManyRedirects { .. } => "too_many_redirects",
        FetchError::Transport(transport) => {
            let message = transport.message().to_lowercase();
            if matches!(transport, browser_http::error::TransportError::Timeout(_))
                || message.contains("timed out")
                || message.contains("timeout")
            {
                "timeout"
            } else {
                "connect"
            }
        }
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Turning corpus argument JSON into a RequestOverrides
// ---------------------------------------------------------------------------

fn follow_from(value: &Value) -> Option<FollowRedirects> {
    match value {
        Value::Bool(flag) => Some(FollowRedirects::Bool(*flag)),
        Value::String(name) => Some(FollowRedirects::Mode(match name.as_str() {
            "safe" => RedirectMode::Safe,
            "all" => RedirectMode::All,
            "obeycode" => RedirectMode::ObeyCode,
            "firstonly" => RedirectMode::FirstOnly,
            other => panic!("unknown follow_redirects {other:?}"),
        })),
        _ => None,
    }
}

fn overrides_from(args: &Value) -> RequestOverrides {
    let mut overrides = RequestOverrides::new();
    let Some(map) = args.as_object() else {
        return overrides;
    };
    for (key, value) in map {
        match key.as_str() {
            "follow_redirects" => {
                if let Some(mode) = follow_from(value) {
                    overrides = overrides.follow_redirects(mode);
                }
            }
            "max_redirects" => {
                overrides = overrides.max_redirects(value.as_i64().unwrap_or(30) as i32);
            }
            "timeout" => {
                overrides = overrides.timeout(value.as_f64().map(Duration::from_secs_f64));
            }
            "retries" => {
                overrides = overrides.retries(value.as_i64().map(|count| count as i32));
            }
            "retry_delay" => {
                overrides =
                    overrides.retry_delay(Duration::from_secs_f64(value.as_f64().unwrap_or(0.0)));
            }
            "headers" => {
                let mut headers = browser_http::config::HeaderDict::new();
                for (name, header) in value.as_object().into_iter().flatten() {
                    headers.insert(name.clone(), header.as_str().unwrap_or_default().to_owned());
                }
                overrides = overrides.headers(headers);
            }
            "params" => {
                let params = value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(name, item)| {
                        (name.clone(), item.as_str().unwrap_or_default().to_owned())
                    })
                    .collect();
                overrides = overrides.params(params);
            }
            "cookies" => {
                let mut cookies = browser_http::config::Cookies::new();
                for (name, item) in value.as_object().into_iter().flatten() {
                    cookies.insert(name.clone(), item.as_str().unwrap_or_default().to_owned());
                }
                overrides = overrides.cookies(cookies);
            }
            "auth" => {
                if let Some(pair) = value.as_array() {
                    overrides = overrides.auth(
                        pair[0].as_str().unwrap_or_default(),
                        pair[1].as_str().unwrap_or_default(),
                    );
                }
            }
            "data" => {
                overrides =
                    overrides.body(Body::Text(value.as_str().unwrap_or_default().to_owned()));
            }
            "json" => {
                overrides = overrides.body(Body::Json(value.clone()));
            }
            "impersonate" => {
                overrides = overrides.impersonate(match value.as_str() {
                    Some(profile) => Impersonate::single(profile),
                    None => Impersonate::None,
                });
            }
            "stealthy_headers" => {
                overrides = overrides.stealthy_headers(value.as_bool().unwrap_or(true));
            }
            other => panic!("corpus argument {other:?} has no mapping"),
        }
    }
    overrides
}

fn config_from(defaults: &Value) -> FetcherConfig {
    let mut config = FetcherConfig::new();
    let Some(map) = defaults.as_object() else {
        return config;
    };
    for (key, value) in map {
        match key.as_str() {
            "__oneshot__" => {}
            "follow_redirects" => {
                if let Some(mode) = follow_from(value) {
                    config.follow_redirects = mode;
                }
            }
            "max_redirects" => config.max_redirects = value.as_i64().unwrap_or(30) as i32,
            "timeout" => {
                config.timeout = value.as_f64().map(Duration::from_secs_f64);
            }
            "retries" => config.retries = value.as_i64().map(|count| count as i32),
            "retry_delay" => {
                config.retry_delay = Duration::from_secs_f64(value.as_f64().unwrap_or(0.0));
            }
            "headers" => {
                for (name, header) in value.as_object().into_iter().flatten() {
                    config
                        .headers
                        .insert(name.clone(), header.as_str().unwrap_or_default().to_owned());
                }
            }
            other => panic!("corpus session default {other:?} has no mapping"),
        }
    }
    config
}

fn method_from(name: &str) -> Method {
    match name {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        other => panic!("unknown method {other}"),
    }
}

// ---------------------------------------------------------------------------
// Observing a Rust response the way generate.py observes a Python one
// ---------------------------------------------------------------------------

const CSS: [&str; 5] = [
    "title::text",
    "p.t::text",
    "#main h1::text",
    "li[data-k]::attr(data-k)",
    "a::attr(href)",
];
const XPATH: [&str; 3] = ["//p[@class='t']/text()", "//li/@data-k", "//a/@href"];

fn observe(response: &Response, port_a: u16, port_b: u16) -> Value {
    let mut headers = serde_json::Map::new();
    for (name, value) in &response.headers {
        headers.insert(
            name.to_lowercase(),
            Value::String(placeholderise(value, port_a, port_b)),
        );
    }
    let mut cookies = serde_json::Map::new();
    for (name, value) in &response.cookies {
        cookies.insert(name.clone(), Value::String(value.clone()));
    }
    let history: Vec<Value> = response
        .history
        .iter()
        .map(|hop| json!([hop.status, placeholderise(&hop.url, port_a, port_b)]))
        .collect();

    let mut css = serde_json::Map::new();
    for selector in CSS {
        let found = response.css(selector).map_or_else(
            |err| json!({ "error": err.to_string() }),
            |nodes| {
                Value::Array(
                    nodes
                        .getall()
                        .into_iter()
                        .map(|text| Value::String(placeholderise(&text, port_a, port_b)))
                        .collect(),
                )
            },
        );
        css.insert(selector.to_owned(), found);
    }
    let mut xpath = serde_json::Map::new();
    for expression in XPATH {
        let found = response.xpath(expression).map_or_else(
            |err| json!({ "error": err.to_string() }),
            |nodes| {
                Value::Array(
                    nodes
                        .getall()
                        .into_iter()
                        .map(|text| Value::String(placeholderise(&text, port_a, port_b)))
                        .collect(),
                )
            },
        );
        xpath.insert(expression.to_owned(), found);
    }

    let mut record = json!({
        "ok": true,
        "status": response.status,
        "reason": response.reason,
        "url": placeholderise(&response.url, port_a, port_b),
        "encoding": response.encoding,
        "headers": Value::Object(headers),
        "cookies": Value::Object(cookies),
        "history": Value::Array(history),
        "body_hex": encode_hex(response.body()),
        "body_len": response.body().len(),
        "text": response.body_text().into_owned(),
        "css": Value::Object(css),
        "xpath": Value::Object(xpath),
    });
    if response
        .header("content-type")
        .is_some_and(|value| value.starts_with("application/json"))
    {
        if let Ok(echo) = serde_json::from_slice::<Value>(response.body()) {
            record["echo"] = echo;
        }
    }
    record
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    checks: usize,
    passed: usize,
    failures: Vec<String>,
    unexpected_passes: Vec<String>,
}

impl Report {
    fn check(&mut self, id: &str, index: usize, field: &str, expected: &Value, actual: &Value) {
        self.checks += 1;
        let same = if field == "outcome" {
            expected["ok"] == actual["ok"]
        } else {
            expected == actual
                || unpinnable(id, index, field).is_some_and(|alternative| *actual == alternative)
        };
        match (divergence(id, index, field), same) {
            (None, true) => self.passed += 1,
            (None, false) => self.failures.push(format!(
                "{id}[{index}].{field}\n     python: {expected}\n       rust: {actual}"
            )),
            (Some(_), false) => self.passed += 1,
            (Some(reason), true) => self.unexpected_passes.push(format!(
                "{id}[{index}].{field} now MATCHES python — remove the declared \
                 divergence:\n     {reason}"
            )),
        }
    }
}

/// Header keys whose value the corpus cannot pin because the transport, not the
/// caller, chooses it.
fn comparable_headers(value: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (name, item) in value.as_object().into_iter().flatten() {
        out.insert(name.clone(), item.clone());
    }
    Value::Object(out)
}

/// The echo document, restricted to what the corpus can pin: the request line,
/// the body, and only those headers the case itself set (plus the two the
/// redirect rules are about).
fn comparable_echo(echo: &Value, args: &Value) -> Value {
    let mut named: BTreeSet<String> = ["cookie", "authorization", "content-type", "content-length"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for name in args
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, _)| name.to_lowercase())
    {
        named.insert(name);
    }
    let mut headers = BTreeMap::new();
    for (name, value) in echo
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if !named.contains(name) {
            continue;
        }
        if name == "cookie" {
            // curl's jar and this crate's order cookies differently; the pair
            // set is what is actually specified.
            let mut pairs: Vec<&str> = value
                .as_str()
                .unwrap_or_default()
                .split(';')
                .map(str::trim)
                .filter(|pair| !pair.is_empty())
                .collect();
            pairs.sort_unstable();
            headers.insert(name.clone(), json!(pairs));
        } else {
            headers.insert(name.clone(), value.clone());
        }
    }
    json!({
        "method": echo.get("method").cloned().unwrap_or(Value::Null),
        "path": echo.get("path").cloned().unwrap_or(Value::Null),
        "query": echo.get("query").cloned().unwrap_or(Value::Null),
        "body": echo.get("body").cloned().unwrap_or(Value::Null),
        "headers": Value::Object(headers.into_iter().collect()),
    })
}

fn compare(
    report: &mut Report,
    id: &str,
    index: usize,
    args: &Value,
    python: &Value,
    rust: &Value,
) {
    let python_ok = python["ok"].as_bool().unwrap_or(false);
    let rust_ok = rust["ok"].as_bool().unwrap_or(false);
    // Carry the error text into the comparison so a mismatched outcome prints
    // *why* each side failed rather than just `true != false`.
    report.check(
        id,
        index,
        "outcome",
        &json!({ "ok": python_ok, "why": python.get("python_error").cloned().unwrap_or(Value::Null) }),
        &json!({ "ok": rust_ok, "why": rust.get("rust_error").cloned().unwrap_or(Value::Null) }),
    );
    if !python_ok || !rust_ok {
        if !python_ok && !rust_ok {
            report.check(
                id,
                index,
                "error_class",
                &python["error_class"],
                &rust["error_class"],
            );
        }
        return;
    }

    for field in [
        "status", "reason", "url", "encoding", "cookies", "history", "body_hex", "text", "css",
        "xpath",
    ] {
        report.check(id, index, field, &python[field], &rust[field]);
    }
    report.check(
        id,
        index,
        "headers",
        &comparable_headers(&python["headers"]),
        &comparable_headers(&rust["headers"]),
    );
    if python.get("echo").is_some() || rust.get("echo").is_some() {
        let empty = Value::Null;
        report.check(
            id,
            index,
            "echo",
            &comparable_echo(python.get("echo").unwrap_or(&empty), args),
            &comparable_echo(rust.get("echo").unwrap_or(&empty), args),
        );
    }
}

// ---------------------------------------------------------------------------
// Driving one row
// ---------------------------------------------------------------------------

fn base(server: &str, port_a: u16, port_b: u16) -> String {
    format!(
        "http://127.0.0.1:{}",
        if server == "A" { port_a } else { port_b }
    )
}

async fn run_single(
    row: &Value,
    fixtures: &(Fixture, Fixture),
    port_a: u16,
    port_b: u16,
) -> Vec<Value> {
    if row["reset"].as_bool().unwrap_or(false) {
        fixtures.0.reset();
        fixtures.1.reset();
    }
    let url = base(row["server"].as_str().unwrap_or("A"), port_a, port_b)
        + row["path"].as_str().unwrap_or_default();
    let mut overrides = overrides_from(&row["args"]);
    if !row["args"].get("retries").is_some() {
        overrides = overrides.retries(Some(0));
    }
    if !row["args"].get("retry_delay").is_some() {
        overrides = overrides.retry_delay(Duration::ZERO);
    }
    let method = method_from(row["method"].as_str().unwrap_or("GET"));
    let client = AsyncFetcherClient::new().expect("client");
    match client
        .request(method, url)
        .with_overrides(overrides)
        .send()
        .await
    {
        Ok(response) => vec![observe(&response, port_a, port_b)],
        Err(err) => vec![json!({
            "ok": false,
            "error_class": error_class(&err),
            "rust_error": err.to_string(),
        })],
    }
}

async fn run_session(
    row: &Value,
    _fixtures: &(Fixture, Fixture),
    port_a: u16,
    port_b: u16,
) -> Vec<Value> {
    let defaults = row.get("defaults").cloned().unwrap_or(json!({}));
    let oneshot = defaults
        .get("__oneshot__")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut config = config_from(&defaults);
    if defaults.get("retries").is_none() {
        config.retries = Some(0);
    }
    if defaults.get("retry_delay").is_none() {
        config.retry_delay = Duration::ZERO;
    }

    let mut results = Vec::new();
    if oneshot {
        for step in row["steps"].as_array().into_iter().flatten() {
            let url = base(
                step.get("server").and_then(Value::as_str).unwrap_or("A"),
                port_a,
                port_b,
            ) + step["path"].as_str().unwrap_or_default();
            let mut overrides = overrides_from(&step["args"]);
            overrides = overrides.retries(Some(0)).retry_delay(Duration::ZERO);
            let client = AsyncFetcherClient::new().expect("client");
            let method = method_from(step["method"].as_str().unwrap_or("GET"));
            results.push(
                match client
                    .request(method, url)
                    .with_overrides(overrides)
                    .send()
                    .await
                {
                    Ok(response) => observe(&response, port_a, port_b),
                    Err(err) => json!({
                        "ok": false,
                        "error_class": error_class(&err),
                        "rust_error": err.to_string(),
                    }),
                },
            );
        }
        return results;
    }

    let session: AsyncFetcherSession = SessionBuilder::new()
        .config(config)
        .build()
        .expect("session");
    session.open().await.expect("open");
    for step in row["steps"].as_array().into_iter().flatten() {
        let url = base(
            step.get("server").and_then(Value::as_str).unwrap_or("A"),
            port_a,
            port_b,
        ) + step["path"].as_str().unwrap_or_default();
        let overrides = overrides_from(&step["args"]);
        let method = method_from(step["method"].as_str().unwrap_or("GET"));
        results.push(match session.send(method, &url, &overrides).await {
            Ok(response) => observe(&response, port_a, port_b),
            Err(err) => json!({
                "ok": false,
                "error_class": error_class(&err),
                "rust_error": err.to_string(),
            }),
        });
    }
    session.close().await.expect("close");
    results
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn u12_http_matches_python() {
    let table: Value = serde_json::from_str(&testdata("u12_endpoints.json")).expect("endpoints");
    let rows: Vec<Value> = testdata("u12_http.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("corpus row"))
        .collect();
    assert!(
        rows.len() >= 120,
        "the corpus must keep at least 120 endpoint x argument combinations, found {}",
        rows.len()
    );

    let fixtures = start_pair(table);
    let (port_a, port_b) = (fixtures.0.port, fixtures.1.port);

    let mut report = Report::default();
    for row in &rows {
        let id = row["id"].as_str().unwrap_or("?");
        let observed = if row["kind"] == json!("single") {
            run_single(row, &fixtures, port_a, port_b).await
        } else {
            run_session(row, &fixtures, port_a, port_b).await
        };
        let expected = row["python"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            expected.len(),
            observed.len(),
            "{id}: python produced {} observations, rust produced {}",
            expected.len(),
            observed.len()
        );
        let args = row.get("args").cloned().unwrap_or(json!({}));
        for (index, (python, rust)) in expected.iter().zip(observed.iter()).enumerate() {
            let step_args = row
                .get("steps")
                .and_then(Value::as_array)
                .and_then(|steps| steps.get(index))
                .and_then(|step| step.get("args"))
                .cloned()
                .unwrap_or_else(|| args.clone());
            compare(&mut report, id, index, &step_args, python, rust);
        }
    }
    fixtures.0.stop();
    fixtures.1.stop();

    let rate = report.passed as f64 * 100.0 / report.checks as f64;
    println!(
        "u12 differential: {}/{} field checks match python ({rate:.2}%), \
         {} declared divergences over {} corpus rows",
        report.passed,
        report.checks,
        DIVERGENCES.len(),
        rows.len()
    );

    let mut problems = String::new();
    if !report.unexpected_passes.is_empty() {
        problems.push_str(&format!(
            "\n{} declared divergence(s) started matching python:\n  - {}\n",
            report.unexpected_passes.len(),
            report.unexpected_passes.join("\n  - ")
        ));
    }
    if !report.failures.is_empty() {
        problems.push_str(&format!(
            "\n{} field(s) differ from python:\n  - {}\n",
            report.failures.len(),
            report.failures.join("\n  - ")
        ));
    }
    assert!(problems.is_empty(), "{problems}");
}

/// The fixture the Rust tests serve is the one Python was recorded against.
///
/// A guard against the two servers drifting: every path the corpus exercises
/// must exist in the shared table.
#[test]
fn every_corpus_path_exists_in_the_shared_endpoint_table() {
    let table: Value = serde_json::from_str(&testdata("u12_endpoints.json")).expect("endpoints");
    for line in testdata("u12_http.jsonl").lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).expect("row");
        let mut paths = Vec::new();
        if let Some(path) = row["path"].as_str() {
            if !path.is_empty() {
                paths.push(path.to_owned());
            }
        }
        for step in row["steps"].as_array().into_iter().flatten() {
            paths.push(step["path"].as_str().unwrap_or_default().to_owned());
        }
        for path in paths {
            assert!(
                table.get(&path).is_some(),
                "{}: no fixture endpoint for {path}",
                row["id"]
            );
        }
    }
}

/// `Arc<dyn ...>` is the shape the driver hands the backend around in; keeping
/// this here means the corpus test compiles against the same public surface the
/// façade does.
#[test]
fn the_public_surface_the_corpus_uses_is_object_safe() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Response>();
    assert_send_sync::<Arc<AsyncFetcherSession>>();
}

// ---------------------------------------------------------------------------
// Named assertions for the three behaviours the corpus proves in aggregate
// ---------------------------------------------------------------------------

fn fixtures() -> (Fixture, Fixture) {
    start_pair(serde_json::from_str(&testdata("u12_endpoints.json")).expect("endpoints"))
}

async fn get(url: &str, overrides: RequestOverrides) -> Result<Response, FetchError> {
    AsyncFetcherClient::new()
        .expect("client")
        .get(url)
        .with_overrides(overrides.retries(Some(0)).retry_delay(Duration::ZERO))
        .send()
        .await
}

/// The `follow_redirects="safe"` decision table, against a live socket.
///
/// The aggregate corpus covers each of these as one row among 188; this states
/// the table in one place, in the order the guard applies it.
#[tokio::test(flavor = "current_thread")]
async fn the_safe_guard_decision_table_holds_against_a_live_fixture() {
    let (a, b) = fixtures();
    let base = format!("http://127.0.0.1:{}", a.port);
    let safe =
        || RequestOverrides::new().follow_redirects(FollowRedirects::Mode(RedirectMode::Safe));

    // A hop that stays on the same origin is followed — the fixture is itself on
    // loopback, and Python follows this too. Anything else would make
    // `follow_redirects="safe"`, the default, unusable against a private host.
    let page = get(&format!("{base}/redirect/chain3"), safe())
        .await
        .expect("a same-origin chain is followed under `safe`");
    assert_eq!(page.status, 200);
    assert_eq!(page.history.len(), 3);
    assert_eq!(page.css("#main h1::text").expect("css").getall(), ["Hello"]);

    // Every hop that changes origin onto a private address is refused, and the
    // refusal names the range.
    for (path, needle) in [
        ("/redirect/toprivate", "private (10.0.0.0/8)"),
        ("/redirect/tolinklocal", "link-local (169.254.0.0/16)"),
        ("/redirect/toloopbackport", "loopback (127.0.0.0/8)"),
        ("/redirect/tov6loopback", "loopback (::1)"),
        // A different *port* on the same host is a different origin, which is
        // exactly the distinction curl draws too.
        ("/redirect/cross", "loopback (127.0.0.0/8)"),
    ] {
        let err = get(
            &format!("{base}{path}"),
            safe().timeout(Some(Duration::from_secs(3))),
        )
        .await
        .expect_err(path);
        match &err {
            FetchError::UnsafeRedirect { reason, .. } => {
                assert!(reason.contains(needle), "{path}: {reason}");
            }
            other => panic!("{path}: expected an SSRF refusal, got {other}"),
        }
        assert_eq!(error_class(&err), "ssrf");
    }

    // A non-HTTP(S) target is refused before the address is even looked at, in
    // every mode — curl's own `CURLOPT_REDIR_PROTOCOLS` default.
    for follow in [
        FollowRedirects::Mode(RedirectMode::Safe),
        FollowRedirects::Bool(true),
    ] {
        for path in ["/redirect/tofile", "/redirect/toftp"] {
            let err = get(
                &format!("{base}{path}"),
                RequestOverrides::new()
                    .follow_redirects(follow)
                    .timeout(Some(Duration::from_secs(3))),
            )
            .await
            .expect_err(path);
            assert!(
                matches!(err, FetchError::UnsupportedRedirectScheme { .. }),
                "{path} under {follow:?}: {err}"
            );
        }
    }

    // The budget is enforced before the guard ever runs, and an unlimited budget
    // still terminates on a loop only because the budget is what stops it.
    let err = get(
        &format!("{base}/redirect/loop"),
        RequestOverrides::new()
            .follow_redirects(FollowRedirects::Bool(true))
            .max_redirects(4),
    )
    .await
    .expect_err("a loop exhausts the budget");
    assert!(
        matches!(err, FetchError::TooManyRedirects { max: 4, .. }),
        "{err}"
    );

    // `max_redirects=0` refuses the very first hop.
    let err = get(
        &format!("{base}/redirect/rel302"),
        RequestOverrides::new()
            .follow_redirects(FollowRedirects::Bool(true))
            .max_redirects(0),
    )
    .await
    .expect_err("zero hops");
    assert!(
        matches!(err, FetchError::TooManyRedirects { max: 0, .. }),
        "{err}"
    );

    a.stop();
    b.stop();
}

/// A session keeps its cookies; a one-shot fetcher does not.
#[tokio::test(flavor = "current_thread")]
async fn a_session_keeps_cookies_across_requests_and_a_one_shot_does_not() {
    let (a, b) = fixtures();
    let base = format!("http://127.0.0.1:{}", a.port);

    let mut config = FetcherConfig::new();
    config.retries = Some(0);
    config.retry_delay = Duration::ZERO;
    let session = SessionBuilder::new()
        .config(config)
        .build()
        .expect("session");
    session.open().await.expect("open");

    let first = session
        .send(
            Method::Get,
            &format!("{base}/cookie/set-then-echo"),
            &RequestOverrides::new(),
        )
        .await
        .expect("set");
    // `Response.cookies` is the reply's own `Set-Cookie` lines, which is what
    // `dict(curl_cffi.Response.cookies)` reports.
    assert_eq!(first.cookies["sess"], "abc123");

    let echoed = session
        .send(
            Method::Get,
            &format!("{base}/end/echo"),
            &RequestOverrides::new(),
        )
        .await
        .expect("echo");
    let body: Value = serde_json::from_slice(echoed.body()).expect("json");
    assert_eq!(body["headers"]["cookie"], json!("sess=abc123"));
    // …and the reply that carries no `Set-Cookie` reports no cookies, even
    // though the jar still holds one.
    assert!(echoed.cookies.is_empty());
    assert_eq!(session.cookies().len(), 1);
    session.close().await.expect("close");

    // A one-shot fetcher gets a fresh jar per request, so nothing persists.
    let _ = get(
        &format!("{base}/cookie/set-then-echo"),
        RequestOverrides::new(),
    )
    .await
    .expect("set");
    let echoed = get(&format!("{base}/end/echo"), RequestOverrides::new())
        .await
        .expect("echo");
    let body: Value = serde_json::from_slice(echoed.body()).expect("json");
    assert!(body["headers"].get("cookie").is_none(), "{body}");

    a.stop();
    b.stop();
}

/// The blocking façade reaches the same fixture and produces the same `Response`.
#[cfg(feature = "blocking")]
#[test]
fn the_blocking_facade_drives_the_same_fixture() {
    let (a, b) = fixtures();
    let base = format!("http://127.0.0.1:{}", a.port);

    let mut config = FetcherConfig::new();
    config.retries = Some(0);
    config.retry_delay = Duration::ZERO;
    let session = browser_http::blocking::FetcherSession::from_builder(
        SessionBuilder::new().config(config),
    )
    .expect("session");
    session.open().expect("open");

    let page = session
        .get(format!("{base}/redirect/chain3"))
        .send()
        .expect("chain");
    assert_eq!(page.status, 200);
    assert_eq!(page.history.len(), 3);
    assert_eq!(
        page.css("p.t::text").expect("css").getall(),
        ["alpha", "beta"]
    );
    session.close().expect("close");

    a.stop();
    b.stop();
}
