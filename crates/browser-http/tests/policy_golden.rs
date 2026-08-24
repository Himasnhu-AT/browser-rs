//! Differential tests against the real the Python reference (`the reference implementation`).
//!
//! `testdata/policy_golden.jsonl` is generated (outside the repo) by
//! `/Users/himanshu/.claude/jobs/a62e4b8e/tmp/gen/gen_policy_golden.py`, run against the
//! installed `the reference implementation`. It drives the reference implementation directly:
//! `_ConfigurationLogic._merge_request_args`, `_headers_job`, `_select_random_browser`,
//! the `_make_request` retry/proxy resolution, `ProxyRotator`, `cyclic_rotation`,
//! `is_proxy_error` and `construct_proxy_dict`.
//!
//! The three non-deterministic Python inputs are pinned in the generator
//! (`generate_headers`, `__default_useragent__`, `random.choice`) and the same values
//! are injected here through [`merge_request_args_seeded`] and a zero-valued RNG, so
//! every row is an exact, reproducible comparison.
//!
//! Rows carrying a `divergence` object are the cases where the port deliberately or
//! unavoidably differs; the reason is in the row's `note` and the expected Rust value
//! in `divergence.rust`. No row is ever skipped.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::TryRng;
use browser_core::types::{FollowRedirects, Proxy, RedirectMode};
use browser_fingerprint::headers::Headers;
use browser_fingerprint::useragent::default_ua;
use browser_http::config::{
    resolve_selector_config, ClientCert, Cookies, FetcherConfig, HeaderDict, HttpVersion,
    Impersonate, ProxySpec, Query, RequestOverrides, SelectorConfig,
};
use browser_http::merge::{merge_request_args_seeded, MergedRequest, RequestContext};
use browser_http::proxy::{
    construct_proxy_dict, construct_proxy_dict_from_str, cyclic_rotation, is_proxy_error_message,
    proxy_key, ProxyRotator,
};
use browser_http::retry::{proxy_for_attempt, retry_warning, RetryDecision, RetryPolicy};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// A deterministic RNG: every draw is zero, so `slice.choose` always yields the
// first element — the same pinning the generator applies to Python's `choice`.
// ---------------------------------------------------------------------------

struct ZeroRng;

impl TryRng for ZeroRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(0)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(0)
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        dst.fill(0);
        Ok(())
    }
}

/// The header set the generator pinned `browser-rs.engines.static.generate_headers` to.
fn pinned_generated_headers() -> Headers {
    let mut headers = Headers::new();
    headers.push("User-Agent", "Generated/1.0");
    headers.push("Accept", "text/html");
    headers.push("Accept-Language", "en-US;q=1.0");
    headers.push("sec-ch-ua-mobile", "?0");
    headers.push("Referer", "https://generated.example/");
    headers
}

/// The value the generator pinned `__default_useragent__` to.
const PINNED_FALLBACK_UA: &str = "Fallback/1.0";

// ---------------------------------------------------------------------------
// Corpus loading
// ---------------------------------------------------------------------------

fn corpus() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/policy_golden.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("golden row is not valid JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// Numeric-tolerant structural comparison. Python emits `30` where Rust emits
// `30.0` for a `Duration`, and that is not a difference worth failing on.
// ---------------------------------------------------------------------------

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => (a - b).abs() < 1e-9,
            _ => a == b,
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| values_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| values_equal(a, b)))
        }
        _ => left == right,
    }
}

/// Collects the failures of one kind so a run reports every mismatch, not the first.
#[derive(Default)]
struct Report {
    total: usize,
    failures: Vec<String>,
}

impl Report {
    fn check(&mut self, id: &str, expected: &Value, actual: &Value) {
        self.total += 1;
        if !values_equal(expected, actual) {
            self.failures.push(format!(
                "[{id}]\n  expected: {expected}\n  actual:   {actual}"
            ));
        }
    }

    fn finish(self, kind: &str) {
        assert!(
            self.failures.is_empty(),
            "{}/{} `{kind}` rows failed:\n{}",
            self.failures.len(),
            self.total,
            self.failures.join("\n")
        );
        assert!(self.total > 0, "no `{kind}` rows in the corpus");
        eprintln!("{kind}: {}/{} rows match", self.total, self.total);
    }
}

// ---------------------------------------------------------------------------
// JSON -> typed inputs
// ---------------------------------------------------------------------------

fn as_str(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected a string, got {value}"))
        .to_owned()
}

fn header_dict(value: &Value) -> HeaderDict {
    let mut out = HeaderDict::new();
    if let Some(map) = value.as_object() {
        for (name, value) in map {
            // A non-string value is not representable in a String->String map; the
            // row carries the `divergence` that spells out what Rust produces.
            let rendered = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            out.insert(name.clone(), rendered);
        }
    }
    out
}

fn cookies(value: &Value) -> Cookies {
    let mut out = Cookies::new();
    if let Some(map) = value.as_object() {
        for (name, value) in map {
            out.insert(name.clone(), as_str(value));
        }
    }
    out
}

fn query(value: &Value) -> Query {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), as_str(value)))
                .collect()
        })
        .unwrap_or_default()
}

fn pair(value: &Value) -> (String, String) {
    let items = value
        .as_array()
        .unwrap_or_else(|| panic!("expected a two-element array, got {value}"));
    (as_str(&items[0]), as_str(&items[1]))
}

fn proxy_spec(value: &Value) -> ProxySpec {
    let mut spec = ProxySpec::default();
    if let Some(map) = value.as_object() {
        for (scheme, url) in map {
            let url = Some(as_str(url));
            match scheme.as_str() {
                "all" => spec.all = url,
                "http" => spec.http = url,
                "https" => spec.https = url,
                "ws" => spec.ws = url,
                "wss" => spec.wss = url,
                other => panic!("unmodelled proxies scheme `{other}`"),
            }
        }
    }
    spec
}

fn proxy(value: &Value) -> Option<Proxy> {
    match value {
        Value::Null => None,
        Value::Object(_) => Some(proxy_config(value)),
        other => Some(serde_json::from_value(other.clone()).expect("proxy shape")),
    }
}

/// A dictionary proxy, including the `server`-less one Python only rejects at
/// runtime: an empty `server` is this port's stand-in for the missing key.
fn proxy_config(value: &Value) -> Proxy {
    Proxy::Config {
        server: value
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        username: value
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_owned),
        password: value
            .get("password")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn client_cert(value: &Value) -> Option<ClientCert> {
    match value {
        Value::Null => None,
        Value::String(path) => Some(ClientCert::File(PathBuf::from(path))),
        Value::Array(items) => Some(ClientCert::Pair {
            cert: PathBuf::from(as_str(&items[0])),
            key: PathBuf::from(as_str(&items[1])),
        }),
        other => panic!("unmodelled cert shape {other}"),
    }
}

fn duration(value: &Value) -> Option<Duration> {
    match value {
        Value::Null => None,
        Value::Number(seconds) => Some(Duration::from_secs_f64(
            seconds.as_f64().expect("finite seconds"),
        )),
        other => panic!("unmodelled duration {other}"),
    }
}

/// `true` for a `(connect, read)` timeout pair, which has no `Duration` shape.
fn is_timeout_pair(value: &Value) -> bool {
    value.is_array()
}

fn impersonate(value: &Value) -> Impersonate {
    match value {
        Value::Null => Impersonate::None,
        Value::String(profile) => Impersonate::single(profile.clone()),
        Value::Array(items) => Impersonate::pool(items.iter().map(as_str)),
        other => panic!("unmodelled impersonate {other}"),
    }
}

fn follow_redirects(value: &Value) -> FollowRedirects {
    match value {
        Value::Bool(flag) => FollowRedirects::Bool(*flag),
        Value::String(mode) => FollowRedirects::Mode(match mode.as_str() {
            "safe" => RedirectMode::Safe,
            "all" => RedirectMode::All,
            "obeycode" => RedirectMode::ObeyCode,
            "firstonly" => RedirectMode::FirstOnly,
            other => panic!("unmodelled redirect mode `{other}`"),
        }),
        other => panic!("unmodelled follow_redirects {other}"),
    }
}

fn selector_config(value: &Value) -> SelectorConfig {
    let mut config = SelectorConfig::new();
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            config.insert(key.clone(), value.clone());
        }
    }
    config
}

fn http_version(code: u64) -> HttpVersion {
    match code {
        0 => HttpVersion::None,
        1 => HttpVersion::Http1_0,
        2 => HttpVersion::Http1_1,
        3 => HttpVersion::Http2,
        4 => HttpVersion::Http2Tls,
        5 => HttpVersion::Http2PriorKnowledge,
        30 => HttpVersion::Http3,
        31 => HttpVersion::Http3Only,
        other => panic!("unmodelled curl http version {other}"),
    }
}

/// `FetcherClient(**session)`, including Python's `kwargs.get(k) or default`
/// falsy-collapse on the five keys that have one.
fn build_config(session: &Value) -> FetcherConfig {
    let mut config = FetcherConfig::default();
    let map = session.as_object().cloned().unwrap_or_default();
    for (key, value) in &map {
        config = match key.as_str() {
            "impersonate" => config.with_impersonate(impersonate(value)),
            "stealthy_headers" => config.with_stealthy_headers(value.as_bool().unwrap_or(false)),
            "http3" => config.with_http3(value.as_bool().unwrap_or(false)),
            "proxies" => config.with_proxies(proxy_spec(value)),
            "proxy" => config.with_proxy(proxy(value)),
            "proxy_auth" => config.with_proxy_auth(match value {
                Value::Null => None,
                other => Some(pair(other)),
            }),
            "timeout" => config.with_timeout(duration(value)),
            "headers" => config.with_headers(header_dict(value)),
            "retries" => config.with_retries(match value {
                Value::Null => None,
                other => Some(other.as_i64().expect("retries is an integer") as i32),
            }),
            "retry_delay" => {
                config.with_retry_delay(duration(value).unwrap_or(Duration::from_secs(0)))
            }
            "follow_redirects" => config.with_follow_redirects(follow_redirects(value)),
            "max_redirects" => {
                config.with_max_redirects(value.as_i64().expect("max_redirects") as i32)
            }
            "verify" => config.with_verify(value.as_bool().unwrap_or(false)),
            "cert" => config.with_cert(client_cert(value)),
            "selector_config" => config.with_selector_config(selector_config(value)),
            other => panic!("unmodelled session key `{other}`"),
        };
    }
    config
}

/// `client._merge_request_args(url=…, **request)`.
///
/// `stealth` lands in the [`RequestContext`] because that is how `_make_request`
/// injects it; every key the Python skip set does not name is routed to the
/// pass-through bag, exactly as `for k, v in method_kwargs.items()` does.
fn build_overrides(request: &Value) -> (RequestOverrides, RequestContext) {
    let mut overrides = RequestOverrides::new();
    let mut context = RequestContext::new();
    let map = request.as_object().cloned().unwrap_or_default();
    for (key, value) in &map {
        match key.as_str() {
            "impersonate" => overrides = overrides.impersonate(impersonate(value)),
            "http3" => overrides = overrides.http3(value.as_bool().unwrap_or(false)),
            "stealth" => context = context.with_stealth(value.as_bool().unwrap_or(false)),
            "headers" => overrides = overrides.headers(header_dict(value)),
            "proxies" => overrides = overrides.proxies(proxy_spec(value)),
            "proxy" => overrides = overrides.proxy(proxy(value)),
            "proxy_auth" => {
                overrides = overrides.proxy_auth(match value {
                    Value::Null => None,
                    other => Some(pair(other)),
                });
            }
            // A (connect, read) pair is unrepresentable; the row carries a `divergence`.
            "timeout" if is_timeout_pair(value) => {}
            "timeout" => overrides = overrides.timeout(duration(value)),
            // `null` on these three has no Rust representation (they are a plain
            // `bool` / `i32` / `FollowRedirects`); the row carries a `divergence`.
            "follow_redirects" if value.is_null() => {}
            "follow_redirects" => overrides = overrides.follow_redirects(follow_redirects(value)),
            "max_redirects" if value.is_null() => {}
            "max_redirects" => {
                overrides = overrides.max_redirects(value.as_i64().expect("max_redirects") as i32);
            }
            "verify" if value.is_null() => {}
            "verify" => overrides = overrides.verify(value.as_bool().expect("verify is a bool")),
            "cert" => overrides = overrides.cert(client_cert(value)),
            "retries" => {
                overrides = overrides.retries(match value {
                    Value::Null => None,
                    other => Some(other.as_i64().expect("retries") as i32),
                });
            }
            "retry_delay" => {
                overrides =
                    overrides.retry_delay(duration(value).unwrap_or(Duration::from_secs(0)));
            }
            "selector_config" => overrides = overrides.selector_config(selector_config(value)),
            "http_version" if !value.is_null() => {
                overrides =
                    overrides.http_version(http_version(value.as_u64().expect("curl code")));
            }
            "params" if !value.is_null() => overrides = overrides.params(query(value)),
            "cookies" if !value.is_null() => overrides = overrides.cookies(cookies(value)),
            "auth" if !value.is_null() => {
                let (user, password) = pair(value);
                overrides = overrides.auth(user, password);
            }
            "json" if !value.is_null() => overrides = overrides.json(value.clone()),
            "data" if !value.is_null() => overrides = overrides.form(query(value)),
            // Everything else — including an explicit `null` on a pass-through key,
            // which the merge must drop.
            other => overrides = overrides.extra(other.to_owned(), value.clone()),
        }
    }
    (overrides, context)
}

// ---------------------------------------------------------------------------
// MergedRequest -> the Python kwargs dict
// ---------------------------------------------------------------------------

fn proxy_to_json(proxy: Option<&Proxy>) -> Value {
    proxy.map_or(Value::Null, |proxy| {
        serde_json::to_value(proxy).expect("proxy serialises")
    })
}

fn merged_to_json(merged: &MergedRequest) -> Value {
    let mut out = Map::new();
    // The pass-through bag goes in FIRST and the typed fields overwrite it, because
    // that is the precedence a driver sees: it reads `merged.allow_redirects`, not
    // `merged.extra["allow_redirects"]`. Python builds one flat dict where the
    // pass-through loop runs last and wins, so a key that collides with a named
    // output key must show up here as a mismatch rather than be papered over.
    for (key, value) in &merged.extra {
        out.insert(key.clone(), value.clone());
    }
    out.insert("url".to_owned(), json!(merged.url));
    out.insert(
        "headers".to_owned(),
        Value::Array(
            merged
                .headers
                .iter()
                .map(|(name, value)| json!([name, value]))
                .collect(),
        ),
    );
    let mut proxies = Map::new();
    for (scheme, url) in merged.proxies.entries() {
        proxies.insert(scheme.to_owned(), json!(url));
    }
    out.insert("proxies".to_owned(), Value::Object(proxies));
    // Python keeps the proxy in its original shape; the port additionally renders a
    // URL for the transport in `MergedRequest::proxy`, checked separately.
    out.insert(
        "proxy".to_owned(),
        proxy_to_json(merged.proxy_source.as_ref()),
    );
    out.insert(
        "proxy_auth".to_owned(),
        merged
            .proxy_auth
            .as_ref()
            .map_or(Value::Null, |(user, password)| json!([user, password])),
    );
    out.insert(
        "timeout".to_owned(),
        merged
            .timeout
            .map_or(Value::Null, |timeout| json!(timeout.as_secs_f64())),
    );
    out.insert(
        "allow_redirects".to_owned(),
        serde_json::to_value(merged.allow_redirects).expect("redirect policy serialises"),
    );
    out.insert("max_redirects".to_owned(), json!(merged.max_redirects));
    out.insert("verify".to_owned(), json!(merged.verify));
    out.insert(
        "cert".to_owned(),
        merged.cert.as_ref().map_or(Value::Null, |cert| match cert {
            ClientCert::File(path) => json!(path.to_string_lossy()),
            ClientCert::Pair { cert, key } => {
                json!([cert.to_string_lossy(), key.to_string_lossy()])
            }
        }),
    );
    out.insert("impersonate".to_owned(), json!(merged.impersonate));

    // The typed pass-throughs go back under their Python names.
    if let Some(params) = &merged.params {
        out.insert("params".to_owned(), pairs_to_object(params));
    }
    if let Some(jar) = &merged.cookies {
        out.insert(
            "cookies".to_owned(),
            Value::Object(
                jar.iter()
                    .map(|(name, value)| (name.clone(), json!(value)))
                    .collect(),
            ),
        );
    }
    if let Some((user, password)) = &merged.auth {
        out.insert("auth".to_owned(), json!([user, password]));
    }
    match &merged.body {
        Some(browser_http::config::Body::Form(pairs)) => {
            out.insert("data".to_owned(), pairs_to_object(pairs));
        }
        Some(browser_http::config::Body::Json(value)) => {
            out.insert("json".to_owned(), value.clone());
        }
        Some(browser_http::config::Body::Text(text)) => {
            out.insert("data".to_owned(), json!(text));
        }
        Some(browser_http::config::Body::Raw(bytes)) => {
            out.insert("data".to_owned(), json!(bytes.as_ref()));
        }
        None => {}
    }
    if let Some(version) = merged.http_version {
        out.insert("http_version".to_owned(), json!(version.curl_code()));
    }
    Value::Object(out)
}

fn pairs_to_object(pairs: &[(String, String)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect(),
    )
}

/// Apply a row's declared divergence to the Python expectation.
fn apply_divergence(row: &Value, expected: &mut Value) {
    if let Some(divergence) = row.get("divergence") {
        let field = as_str(&divergence["field"]);
        let replacement = divergence["rust"].clone();
        if field == "expected" {
            *expected = replacement;
        } else {
            expected
                .as_object_mut()
                .expect("divergent field on a non-object expectation")
                .insert(field, replacement);
        }
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn zero_rng_always_picks_the_first_element() {
    use rand::seq::IndexedRandom;
    let items = ["a", "b", "c", "d", "e", "f", "g"];
    for _ in 0..64 {
        assert_eq!(items.choose(&mut ZeroRng), Some(&"a"));
    }
}

#[test]
fn merge_request_args_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "merge" {
            continue;
        }
        let id = as_str(&row["id"]);
        let config = build_config(&row["session"]);
        let (overrides, context) = build_overrides(&row["request"]);
        let merged = merge_request_args_seeded(
            &as_str(&row["url"]),
            &config,
            &overrides,
            &context,
            &mut ZeroRng,
            pinned_generated_headers,
            PINNED_FALLBACK_UA,
        );
        let mut expected = row["expected"].clone();
        apply_divergence(&row, &mut expected);
        report.check(&id, &expected, &merged_to_json(&merged));
    }
    report.finish("merge");
}

#[test]
fn retry_and_selector_resolution_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "retry_resolution" {
            continue;
        }
        let id = as_str(&row["id"]);
        let config = build_config(&row["session"]);
        let (overrides, _) = build_overrides(&row["request"]);
        let policy = RetryPolicy::resolve(&config, &overrides);
        let resolved = resolve_selector_config(&config, &overrides);
        let actual = json!({
            "expected_attempts": policy.attempts,
            "expected_delay": policy.delay.as_secs_f64(),
            "expected_selector_config": Value::Object(
                resolved
                    .as_map()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
        });
        let expected = json!({
            "expected_attempts": row["expected_attempts"],
            "expected_delay": row["expected_delay"],
            "expected_selector_config": row["expected_selector_config"],
        });
        report.check(&id, &expected, &actual);
    }
    report.finish("retry_resolution");
}

/// The `for attempt in range(max_retries)` loop body: when Python sleeps and
/// retries versus when it re-raises.
#[test]
fn retry_decision_sequence_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "retry_sequence" {
            continue;
        }
        let id = as_str(&row["id"]);
        let (overrides, _) = build_overrides(&row["request"]);
        let delay = duration(&row["delay"]).expect("a delay");
        let policy = RetryPolicy::resolve(
            &FetcherConfig::default().with_retry_delay(delay),
            &overrides,
        );
        let decisions: Vec<Value> = (0..policy.attempts.min(8))
            .map(|attempt| match policy.decide(attempt) {
                RetryDecision::Retry { delay } => json!({
                    "attempt": attempt,
                    "retry": true,
                    "delay": delay.as_secs_f64(),
                }),
                RetryDecision::GiveUp => json!({
                    "attempt": attempt,
                    "retry": false,
                    "delay": row["delay"],
                }),
            })
            .collect();
        report.check(
            &id,
            &json!({"attempts": row["expected_attempts"], "decisions": row["expected"]}),
            &json!({"attempts": policy.attempts, "decisions": decisions}),
        );
    }
    report.finish("retry_sequence");
}

#[test]
fn every_curl_impersonation_alias_is_known() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "impersonate_alias" {
            continue;
        }
        let alias = as_str(&row["alias"]);
        let profile = Impersonate::single(alias.clone());
        report.check(
            &alias,
            &json!({"unknown": Vec::<String>::new(), "selected": alias, "enabled": true}),
            &json!({
                "unknown": profile.unknown_profiles(),
                "selected": browser_http::merge::select_random_browser(&profile),
                "enabled": profile.is_enabled(),
            }),
        );
    }
    report.finish("impersonate_alias");
}

#[test]
fn select_random_browser_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "select_random_browser" {
            continue;
        }
        let id = as_str(&row["id"]);
        let selected = browser_http::merge::select_random_browser_with(
            &impersonate(&row["input"]),
            &mut ZeroRng,
        );
        report.check(&id, &row["expected"], &json!(selected));
    }
    report.finish("select_random_browser");
}

/// `__default_useragent__` is a fresh browserforge draw taken at Python import
/// time, so it is not byte-reproducible on either side. What *is* reproducible is
/// its shape: one of the three browser families `generate_headers(browser_mode=False)`
/// is configured with, at or above that family's declared minimum version.
#[test]
fn default_user_agent_shape_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "default_useragent" {
            continue;
        }
        let declared = row["declared_minimums"]
            .as_object()
            .expect("declared minimums");
        let observed = as_str(&row["observed"]);
        // The generator's own sample must satisfy the rule it states.
        let (family, version) = ua_family(&observed);
        assert!(
            declared
                .get(&family)
                .and_then(Value::as_u64)
                .is_some_and(|minimum| version >= minimum),
            "the Python sample `{observed}` does not satisfy its own declared minimums"
        );

        let ours = default_ua();
        assert!(
            ours.starts_with("Mozilla/5.0 "),
            "default_ua() is not a browser UA: {ours}"
        );
        let (family, version) = ua_family(ours);
        report.check(
            "default_useragent",
            &json!({"family_is_declared": true, "at_least_minimum": true}),
            &json!({
                "family_is_declared": declared.contains_key(&family),
                "at_least_minimum": declared
                    .get(&family)
                    .and_then(Value::as_u64)
                    .is_some_and(|minimum| version >= minimum),
            }),
        );
    }
    report.finish("default_useragent");
}

/// The browser family and major version of a `User-Agent`. `Edg/` wins over the
/// `Chrome/` token Edge also carries.
fn ua_family(user_agent: &str) -> (String, u64) {
    for family in ["Edg", "Firefox", "Chrome"] {
        let needle = format!("{family}/");
        if let Some(rest) = user_agent.split(&needle).nth(1) {
            let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(version) = major.parse() {
                return (family.to_owned(), version);
            }
        }
    }
    panic!("no browser family token in `{user_agent}`")
}

#[test]
fn proxy_rotation_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "rotation" {
            continue;
        }
        let id = as_str(&row["id"]);
        let proxies: Vec<Proxy> = row["proxies"]
            .as_array()
            .expect("proxies array")
            .iter()
            .map(|value| proxy(value).expect("a non-null proxy"))
            .collect();
        let rotator = ProxyRotator::new(proxies).expect("rotator builds");
        let draws = row["draws"].as_u64().expect("draws") as usize;
        let sequence: Vec<Value> = (0..draws)
            .map(|_| proxy_to_json(Some(&rotator.get_proxy())))
            .collect();
        report.check(
            &id,
            &json!({
                "sequence": row["expected"],
                "len": row["expected_len"],
                "repr": row["expected_repr"],
                "final_index": row["final_index"],
            }),
            &json!({
                "sequence": sequence,
                "len": rotator.len(),
                "repr": format!("{rotator}"),
                "final_index": rotator.current_index(),
            }),
        );
    }
    report.finish("rotation");
}

#[test]
fn cyclic_rotation_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "cyclic_rotation" {
            continue;
        }
        let proxies: Vec<Proxy> = row["proxies"]
            .as_array()
            .expect("proxies array")
            .iter()
            .map(|value| proxy(value).expect("a non-null proxy"))
            .collect();
        let index = row["index"].as_u64().expect("index") as usize;
        let (chosen, next) = cyclic_rotation(&proxies, index);
        report.check(
            &format!("cyclic-{}-{index}", proxies.len()),
            &json!({"proxy": row["expected_proxy"], "next": row["expected_next"]}),
            &json!({"proxy": proxy_to_json(Some(&chosen)), "next": next}),
        );
    }
    report.finish("cyclic_rotation");
}

#[test]
fn rotator_construction_errors_match_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "rotator_error" {
            continue;
        }
        let id = as_str(&row["id"]);
        let proxies: Vec<Proxy> = row["proxies"]
            .as_array()
            .expect("proxies array")
            .iter()
            .map(|value| proxy(value).expect("a non-null proxy"))
            .collect();
        let actual = match ProxyRotator::new(proxies) {
            Ok(_) => Value::Null,
            Err(error) => json!({ "message": error.to_string() }),
        };
        let expected = match &row["expected_error"] {
            Value::Null => Value::Null,
            error => json!({ "message": error["message"] }),
        };
        report.check(&id, &expected, &actual);
    }
    report.finish("rotator_error");
}

#[test]
fn proxy_key_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "proxy_key" {
            continue;
        }
        let value = proxy(&row["input"]).expect("a non-null proxy");
        report.check(
            &format!("{}", row["input"]),
            &row["expected"],
            &json!(proxy_key(&value)),
        );
    }
    report.finish("proxy_key");
}

#[test]
fn is_proxy_error_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "is_proxy_error" {
            continue;
        }
        let message = as_str(&row["message"]);
        report.check(
            &format!("{message:?}"),
            &row["expected"],
            &json!(is_proxy_error_message(&message)),
        );
    }
    report.finish("is_proxy_error");
}

#[test]
fn construct_proxy_dict_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "construct_proxy_dict" {
            continue;
        }
        let id = format!("{}", row["input"]);
        let result = match &row["input"] {
            Value::String(text) => construct_proxy_dict_from_str(text),
            // A dict without `server` has no `Proxy` representation, so the
            // empty-server `Proxy::Config` stands in for it.
            object => construct_proxy_dict(&proxy_config(object)),
        };
        let actual = match result {
            Ok(dict) => json!({
                "ok": {"server": dict.server, "username": dict.username, "password": dict.password},
                "error": Value::Null,
            }),
            Err(error) => json!({"ok": Value::Null, "error": error.to_string()}),
        };
        let expected = match row.get("divergence") {
            // The row states, with its `note`, what the port produces instead.
            Some(divergence) => json!({"ok": divergence["ok"], "error": divergence["error"]}),
            None => json!({
                "ok": row["expected"],
                "error": match &row["expected_error"] {
                    Value::Null => Value::Null,
                    error => error["message"].clone(),
                },
            }),
        };
        report.check(&id, &expected, &actual);
    }
    report.finish("construct_proxy_dict");
}

/// The `_make_request` loop as a whole: `proxy_for_attempt` then `merge_request_args`,
/// once per attempt, with the rotator advancing between them.
#[test]
fn request_cycle_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "request_cycle" {
            continue;
        }
        let id = as_str(&row["id"]);
        let session = &row["session"];
        let mut config = FetcherConfig::default();
        if let Some(urls) = session.get("proxy_rotator_urls").and_then(Value::as_array) {
            let proxies: Vec<Proxy> = urls
                .iter()
                .map(|value| proxy(value).expect("a non-null proxy"))
                .collect();
            config = config
                .with_proxy_rotator(Some(Arc::new(ProxyRotator::new(proxies).expect("rotator"))));
        }
        if let Some(value) = session.get("proxy") {
            config = config.with_proxy(proxy(value));
        }
        let (overrides, context) = build_overrides(&row["request"]);
        let attempts = row["attempts"].as_u64().expect("attempts") as usize;
        let merged: Vec<Value> = (0..attempts)
            .map(|_| {
                let chosen = proxy_for_attempt(&config, &overrides);
                let context = context.clone().with_proxy(chosen);
                merged_to_json(&merge_request_args_seeded(
                    "https://example.com",
                    &config,
                    &overrides,
                    &context,
                    &mut ZeroRng,
                    pinned_generated_headers,
                    PINNED_FALLBACK_UA,
                ))
            })
            .collect();
        report.check(&id, &row["expected"], &json!(merged));
    }
    report.finish("request_cycle");
}

/// `BaseFetcher.parser_keywords` and the five parser defaults this crate models.
#[test]
fn parser_keywords_match_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "parser_keywords" {
            continue;
        }
        let defaults = browser_http::config::SelectorConfig::parser_defaults();
        report.check(
            "parser_keywords",
            &json!({
                "keywords": row["expected"],
                "defaults": row["expected_defaults"],
            }),
            &json!({
                "keywords": browser_http::config::PARSER_KEYWORDS,
                "defaults": Value::Object(
                    defaults
                        .as_map()
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
            }),
        );
    }
    report.finish("parser_keywords");
}

/// What `curl_cffi.requests.utils` actually sets `CURLOPT_FOLLOWLOCATION` to.
#[test]
fn follow_redirects_curl_codes_match_curl_cffi() {
    use browser_http::config::FollowRedirectsExt;
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "follow_redirects_code" {
            continue;
        }
        let id = format!("{}", row["input"]);
        let policy = follow_redirects(&row["input"]);
        let expected = match row.get("divergence") {
            Some(divergence) => divergence["rust"].clone(),
            None => row["expected"].clone(),
        };
        report.check(&id, &expected, &json!(policy.curl_code()));
    }
    report.finish("follow_redirects_code");
}

#[test]
fn http_version_curl_codes_match_curl_cffi() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "http_version_code" {
            continue;
        }
        let name = as_str(&row["name"]);
        let code = row["expected"].as_u64().expect("curl code");
        report.check(
            &name,
            &row["expected"],
            &json!(http_version(code).curl_code()),
        );
    }
    report.finish("http_version_code");
}

#[test]
fn rotator_conflict_validation_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "config_validation" {
            continue;
        }
        let id = as_str(&row["id"]);
        let rotator = Arc::new(ProxyRotator::from_urls(["http://p:1"]).expect("rotator"));
        let config = FetcherConfig::default()
            .with_proxy(proxy(&row["has_proxy"]))
            .with_proxies(proxy_spec(&row["has_proxies"]))
            .with_proxy_rotator(Some(rotator));
        let actual = match config.validate() {
            Ok(()) => Value::Null,
            Err(error) => json!({ "message": error.to_string() }),
        };
        let expected = match &row["expected_error"] {
            Value::Null => Value::Null,
            error => json!({ "message": error["message"] }),
        };
        report.check(&id, &expected, &actual);
    }
    report.finish("config_validation");
}

#[test]
fn per_attempt_proxy_matches_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "attempt_proxy" {
            continue;
        }
        let id = as_str(&row["id"]);
        let session = &row["session"];
        let mut config = FetcherConfig::default();
        if let Some(urls) = session.get("proxy_rotator_urls").and_then(Value::as_array) {
            let proxies: Vec<Proxy> = urls
                .iter()
                .map(|value| proxy(value).expect("a non-null proxy"))
                .collect();
            config = config
                .with_proxy_rotator(Some(Arc::new(ProxyRotator::new(proxies).expect("rotator"))));
        }
        if let Some(value) = session.get("proxy") {
            config = config.with_proxy(proxy(value));
        }
        let mut overrides = RequestOverrides::new();
        if row["static_proxy_present"].as_bool() == Some(true) {
            overrides = overrides.proxy(proxy(&row["static_proxy"]).or_else(|| {
                // `proxy=""` is present but falsy: it must still disable the rotator.
                row["static_proxy"]
                    .as_str()
                    .map(|text| Proxy::Url(text.to_owned()))
            }));
        }
        let attempts = row["attempts"].as_u64().expect("attempts") as usize;
        let sequence: Vec<Value> = (0..attempts)
            .map(|_| proxy_to_json(proxy_for_attempt(&config, &overrides).as_ref()))
            .collect();
        report.check(&id, &row["expected"], &json!(sequence));
    }
    report.finish("attempt_proxy");
}

#[test]
fn retry_log_lines_match_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "retry_log" {
            continue;
        }
        let id = as_str(&row["id"]);
        let attempt = row["attempt"].as_u64().expect("attempt") as u32;
        let error = as_str(&row["error"]);
        let selected = proxy(&row["proxy"]);
        let delay = duration(&row["delay"]).expect("a delay");
        let mut expected = row["expected"].clone();
        apply_divergence(&row, &mut expected);
        report.check(
            &id,
            &expected,
            &json!(retry_warning(
                attempt,
                error.as_str(),
                selected.as_ref(),
                delay
            )),
        );
        // The proxy-error classification the message depends on.
        assert_eq!(
            is_proxy_error_message(&error),
            row["is_proxy_error"].as_bool().expect("is_proxy_error"),
            "proxy-error classification for `{error}`"
        );
    }
    report.finish("retry_log");
}

#[test]
fn failure_log_lines_match_python() {
    let mut report = Report::default();
    for row in corpus() {
        if row["kind"] != "failure_log" {
            continue;
        }
        let attempts = row["attempts"].as_u64().expect("attempts") as u32;
        let error = as_str(&row["error"]);
        report.check(
            &error,
            &row["expected"],
            &json!(browser_http::retry::failure_message(
                attempts,
                error.as_str()
            )),
        );
    }
    report.finish("failure_log");
}

// ---------------------------------------------------------------------------
// The pinned expectations of `tests/fetchers/test_merge_request_args.py`,
// `test_impersonate_list.py` and `test_proxy_rotation.py` are ported verbatim in
// `tests/python_ports.rs`; only the invariant that file does not assert lives here.
// ---------------------------------------------------------------------------

/// `ProxyRotator.get_proxy` holds the lock across the **strategy call**, not just
/// the cursor write.
///
/// Python's `with self._lock: proxy, self._current_index = self._strategy(...)`
/// makes the read-decide-write one atomic step, so a strategy may safely derive its
/// answer from the index it was handed. A strategy that asserts
/// `index == calls-so-far` fails under contention if only the store is guarded.
#[test]
fn the_rotator_lock_covers_the_whole_strategy_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    let seen = Arc::new(AtomicUsize::new(0));
    let checker = Arc::clone(&seen);
    let strategy = move |proxies: &[Proxy], index: usize| {
        assert_eq!(index, checker.load(Ordering::SeqCst) % proxies.len());
        checker.fetch_add(1, Ordering::SeqCst);
        cyclic_rotation(proxies, index)
    };
    let proxies: Vec<Proxy> = (0..4)
        .map(|i| Proxy::Url(format!("http://p{i}:8080")))
        .collect();
    let rotator = Arc::new(ProxyRotator::with_strategy(proxies, strategy).expect("rotator builds"));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let rotator = Arc::clone(&rotator);
            thread::spawn(move || {
                for _ in 0..50 {
                    let _ = rotator.get_proxy();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread joins");
    }
    assert_eq!(seen.load(Ordering::SeqCst), 400);
    assert_eq!(rotator.current_index(), 0);
}
