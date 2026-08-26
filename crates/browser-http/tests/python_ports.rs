//! Direct ports of the reference implementation's own test files for this subsystem:
//!
//! * `tests/fetchers/test_merge_request_args.py` — the browser-only kwargs must not
//!   leak into the HTTP request dict (regression for D4Vinci/the reference implementation#247);
//! * `tests/fetchers/test_impersonate_list.py` — `_select_random_browser` and the
//!   session/request precedence of a list-valued `impersonate`;
//! * `tests/fetchers/test_proxy_rotation.py` — `cyclic_rotation`, `ProxyRotator`
//!   (creation, rotation, custom strategies, properties, thread safety) and
//!   `is_proxy_error`.
//!
//! Each Rust test names the Python class and method it comes from. The handful of
//! Python cases that assert a `TypeError` the Rust type system already prevents are
//! recorded as `#[test]`s that document the impossibility instead of being dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use browser_core::types::Proxy;
use browser_http::config::{FetcherConfig, Impersonate, RequestOverrides};
use browser_http::merge::{merge_request_args, select_random_browser, RequestContext};
use browser_http::proxy::{
    cyclic_rotation, is_proxy_error, is_proxy_error_message, ProxyError, ProxyRotator,
};
use serde_json::Value;

// =========================================================================
// test_merge_request_args.py :: TestMergeArgsSkipsBrowserParams
// =========================================================================

/// `FetcherClient()._merge_request_args(url="https://example.com", **extra)`.
fn build_args(extra: RequestOverrides) -> browser_http::merge::MergedRequest {
    merge_request_args(
        "https://example.com",
        &FetcherConfig::new(),
        &extra,
        &RequestContext::new(),
    )
}

#[test]
fn test_block_ads_excluded() {
    let args = build_args(RequestOverrides::new().extra("block_ads", Value::Bool(true)));
    assert!(!args.extra.contains_key("block_ads"));
}

#[test]
fn test_google_search_excluded() {
    let args = build_args(RequestOverrides::new().extra("google_search", Value::Bool(true)));
    assert!(!args.extra.contains_key("google_search"));
}

#[test]
fn test_extra_headers_excluded() {
    let args = build_args(
        RequestOverrides::new().extra("extra_headers", serde_json::json!({"X-Custom": "val"})),
    );
    assert!(!args.extra.contains_key("extra_headers"));
    // …and it did not sneak into the real header map either.
    assert!(!args.headers.contains_key("X-Custom"));
}

#[test]
fn test_url_present() {
    let args = build_args(RequestOverrides::new());
    assert_eq!(args.url, "https://example.com");
}

#[test]
fn test_valid_kwargs_passed_through() {
    let cookies = [("session".to_owned(), "abc".to_owned())]
        .into_iter()
        .collect();
    let args = build_args(RequestOverrides::new().cookies(cookies));
    assert_eq!(
        args.cookies
            .as_ref()
            .and_then(|c| c.get("session"))
            .map(String::as_str),
        Some("abc")
    );
}

// =========================================================================
// test_impersonate_list.py :: TestRandomBrowserSelection
// =========================================================================

#[test]
fn test_select_random_browser_with_single_string() {
    assert_eq!(
        select_random_browser(&Impersonate::single("chrome")),
        Some("chrome".to_owned())
    );
}

#[test]
fn test_select_random_browser_with_none() {
    assert_eq!(select_random_browser(&Impersonate::None), None);
}

#[test]
fn test_select_random_browser_with_list() {
    let browsers = ["chrome", "firefox", "safari"];
    let result = select_random_browser(&Impersonate::pool(browsers)).expect("a pick");
    assert!(browsers.contains(&result.as_str()));
}

#[test]
fn test_select_random_browser_with_empty_list() {
    assert_eq!(
        select_random_browser(&Impersonate::pool(Vec::<String>::new())),
        None
    );
}

#[test]
fn test_select_random_browser_with_single_item_list() {
    assert_eq!(
        select_random_browser(&Impersonate::pool(["chrome"])),
        Some("chrome".to_owned())
    );
}

// ---- TestImpersonateTypeValidation ----

#[test]
fn test_impersonate_accepts_string() {
    let session = FetcherConfig::new().with_impersonate("chrome");
    assert_eq!(session.impersonate, Impersonate::single("chrome"));
}

#[test]
fn test_impersonate_accepts_list() {
    let browsers = vec!["chrome".to_owned(), "firefox".to_owned()];
    let session = FetcherConfig::new().with_impersonate(browsers.clone());
    assert_eq!(session.impersonate, Impersonate::Pool(browsers));
}

#[test]
fn test_impersonate_accepts_none() {
    let session = FetcherConfig::new().with_impersonate(Option::<String>::None);
    assert_eq!(session.impersonate, Impersonate::None);
    assert!(!session.impersonate.is_enabled());
}

// ---- TestFetcherSessionWithImpersonateList (the offline half) ----
//
// The Python tests here drive a live httpbin; what they actually pin about the merge
// is the precedence of a request-level `impersonate` over the session's, which needs
// no network.

#[test]
fn test_session_request_level_impersonate_override() {
    let config =
        FetcherConfig::new().with_impersonate(vec!["chrome".to_owned(), "firefox".to_owned()]);
    let merged = merge_request_args(
        "https://example.com",
        &config,
        &RequestOverrides::new().impersonate("safari"),
        &RequestContext::new(),
    );
    assert_eq!(merged.impersonate.as_deref(), Some("safari"));
}

#[test]
fn test_session_request_level_impersonate_list_override() {
    let config =
        FetcherConfig::new().with_impersonate(vec!["chrome".to_owned(), "firefox".to_owned()]);
    let merged = merge_request_args(
        "https://example.com",
        &config,
        &RequestOverrides::new().impersonate(vec!["safari".to_owned(), "edge".to_owned()]),
        &RequestContext::new(),
    );
    let picked = merged.impersonate.expect("a pick");
    assert!(["safari", "edge"].contains(&picked.as_str()));
}

#[test]
fn test_session_multiple_requests_with_impersonate_list() {
    // `chrome110 | chrome120 | chrome131` is resolved per request, so a session can
    // legitimately produce a different profile on each call.
    let config = FetcherConfig::new().with_impersonate(vec![
        "chrome110".to_owned(),
        "chrome120".to_owned(),
        "chrome131".to_owned(),
    ]);
    for _ in 0..25 {
        let merged = merge_request_args(
            "https://example.com",
            &config,
            &RequestOverrides::new(),
            &RequestContext::new(),
        );
        assert!(merged.impersonate_enabled());
        let picked = merged.impersonate.expect("a pick");
        assert!(["chrome110", "chrome120", "chrome131"].contains(&picked.as_str()));
    }
}

// =========================================================================
// test_proxy_rotation.py :: TestCyclicRotationStrategy
// =========================================================================

fn urls(list: &[&str]) -> Vec<Proxy> {
    list.iter().map(|u| Proxy::Url((*u).to_owned())).collect()
}

#[test]
fn test_cyclic_rotation_cycles_through_proxies() {
    let proxies = urls(&["http://p1:8080", "http://p2:8080", "http://p3:8080"]);

    let (proxy, next_idx) = cyclic_rotation(&proxies, 0);
    assert_eq!(proxy, Proxy::Url("http://p1:8080".into()));
    assert_eq!(next_idx, 1);

    let (proxy, next_idx) = cyclic_rotation(&proxies, 1);
    assert_eq!(proxy, Proxy::Url("http://p2:8080".into()));
    assert_eq!(next_idx, 2);

    let (proxy, next_idx) = cyclic_rotation(&proxies, 2);
    assert_eq!(proxy, Proxy::Url("http://p3:8080".into()));
    assert_eq!(next_idx, 0); // Wraps around
}

#[test]
fn test_cyclic_rotation_wraps_index() {
    let proxies = urls(&["http://p1:8080", "http://p2:8080"]);
    let (proxy, next_idx) = cyclic_rotation(&proxies, 5);
    assert_eq!(proxy, Proxy::Url("http://p2:8080".into())); // 5 % 2 == 1
    assert_eq!(next_idx, 0);
}

// ---- TestProxyRotatorCreation ----

#[test]
fn test_create_with_string_proxies() {
    let proxies = urls(&["http://p1:8080", "http://p2:8080"]);
    let rotator = ProxyRotator::new(proxies.clone()).expect("rotator");
    assert_eq!(rotator.len(), 2);
    assert_eq!(rotator.proxies(), proxies);
}

#[test]
fn test_create_with_dict_proxies() {
    let proxies = vec![
        Proxy::Config {
            server: "http://p1:8080".into(),
            username: Some("user1".into()),
            password: Some("pass1".into()),
        },
        Proxy::Config {
            server: "http://p2:8080".into(),
            username: None,
            password: None,
        },
    ];
    let rotator = ProxyRotator::new(proxies.clone()).expect("rotator");
    assert_eq!(rotator.len(), 2);
    assert_eq!(rotator.proxies(), proxies);
}

#[test]
fn test_create_with_mixed_proxies() {
    let rotator = ProxyRotator::new([
        Proxy::Url("http://p1:8080".into()),
        Proxy::Config {
            server: "http://p2:8080".into(),
            username: Some("user".into()),
            password: None,
        },
    ])
    .expect("rotator");
    assert_eq!(rotator.len(), 2);
}

#[test]
fn test_empty_proxies_raises_error() {
    let error = ProxyRotator::new(Vec::<Proxy>::new()).expect_err("must reject");
    assert_eq!(error, ProxyError::Empty);
    assert_eq!(error.to_string(), "At least one proxy must be provided");
}

#[test]
fn test_dict_without_server_raises_error() {
    // A `Proxy::Config` with an empty `server` is the port's stand-in for Python's
    // "dict with no 'server' key" — the only shape the type system leaves open.
    let error = ProxyRotator::new([Proxy::Config {
        server: String::new(),
        username: Some("user".into()),
        password: Some("pass".into()),
    }])
    .expect_err("must reject");
    assert_eq!(error, ProxyError::DictMissingServer);
    assert_eq!(error.to_string(), "Proxy dict must have a 'server' key");
}

/// Python's `TypeError("Invalid proxy type: …")` and
/// `TypeError("strategy must be callable, got …")` have no Rust analogue: a proxy is a
/// `Proxy` and a strategy is a `RotationStrategy`, both checked at compile time.
/// Recorded here so the two Python cases are accounted for rather than dropped.
#[test]
fn test_invalid_proxy_type_and_non_callable_strategy_are_unrepresentable() {
    // `ProxyRotator::new([123])` and `ProxyRotator::with_strategy(…, "cyclic")` do
    // not compile; the corresponding runtime errors therefore cannot occur.
    let rotator =
        ProxyRotator::with_strategy(urls(&["http://p1:8080"]), |pool: &[Proxy], idx: usize| {
            (pool[0].clone(), idx)
        })
        .expect("rotator");
    assert_eq!(rotator.len(), 1);
}

// ---- TestProxyRotatorRotation ----

#[test]
fn test_get_proxy_cyclic_rotation() {
    let rotator = ProxyRotator::new(urls(&[
        "http://p1:8080",
        "http://p2:8080",
        "http://p3:8080",
    ]))
    .expect("rotator");

    for _cycle in 0..2 {
        assert_eq!(rotator.get_proxy(), Proxy::Url("http://p1:8080".into()));
        assert_eq!(rotator.get_proxy(), Proxy::Url("http://p2:8080".into()));
        assert_eq!(rotator.get_proxy(), Proxy::Url("http://p3:8080".into()));
    }
}

#[test]
fn test_get_proxy_single_proxy() {
    let rotator = ProxyRotator::new(urls(&["http://only:8080"])).expect("rotator");
    for _ in 0..5 {
        assert_eq!(rotator.get_proxy(), Proxy::Url("http://only:8080".into()));
    }
}

#[test]
fn test_get_proxy_with_dict_proxies() {
    let first = Proxy::Config {
        server: "http://p1:8080".into(),
        username: None,
        password: None,
    };
    let second = Proxy::Config {
        server: "http://p2:8080".into(),
        username: None,
        password: None,
    };
    let rotator = ProxyRotator::new([first.clone(), second.clone()]).expect("rotator");
    assert_eq!(rotator.get_proxy(), first);
    assert_eq!(rotator.get_proxy(), second);
    assert_eq!(rotator.get_proxy(), first);
}

// ---- TestCustomStrategies ----

#[test]
fn test_random_strategy() {
    let proxies = urls(&["http://p1:8080", "http://p2:8080", "http://p3:8080"]);
    let rotator = ProxyRotator::with_strategy(proxies.clone(), |pool: &[Proxy], idx: usize| {
        use rand::seq::IndexedRandom;
        (
            pool.choose(&mut rand::rng()).expect("non-empty").clone(),
            idx,
        )
    })
    .expect("rotator");

    for _ in 0..10 {
        assert!(proxies.contains(&rotator.get_proxy()));
    }
}

#[test]
fn test_sticky_strategy() {
    let rotator = ProxyRotator::with_strategy(
        urls(&["http://p1:8080", "http://p2:8080"]),
        |pool: &[Proxy], idx: usize| (pool[0].clone(), idx),
    )
    .expect("rotator");

    for _ in 0..5 {
        assert_eq!(rotator.get_proxy(), Proxy::Url("http://p1:8080".into()));
    }
}

#[test]
fn test_weighted_strategy() {
    let call_count = AtomicUsize::new(0);
    let rotator = ProxyRotator::with_strategy(
        urls(&["http://primary:8080", "http://backup:8080"]),
        move |pool: &[Proxy], idx: usize| {
            let n = call_count.fetch_add(1, Ordering::SeqCst) + 1;
            if n.is_multiple_of(3) {
                (pool[1].clone(), idx)
            } else {
                (pool[0].clone(), idx)
            }
        },
    )
    .expect("rotator");

    assert_eq!(
        rotator.get_proxy(),
        Proxy::Url("http://primary:8080".into())
    );
    assert_eq!(
        rotator.get_proxy(),
        Proxy::Url("http://primary:8080".into())
    );
    assert_eq!(rotator.get_proxy(), Proxy::Url("http://backup:8080".into()));
}

#[test]
fn test_lambda_strategy() {
    let rotator = ProxyRotator::with_strategy(
        urls(&["http://p1:8080", "http://p2:8080"]),
        |pool: &[Proxy], idx: usize| (pool[pool.len() - 1].clone(), idx), // Always last
    )
    .expect("rotator");

    assert_eq!(rotator.get_proxy(), Proxy::Url("http://p2:8080".into()));
    assert_eq!(rotator.get_proxy(), Proxy::Url("http://p2:8080".into()));
}

// ---- TestProxyRotatorProperties ----

#[test]
fn test_proxies_property_returns_copy() {
    let original = urls(&["http://p1:8080", "http://p2:8080"]);
    let rotator = ProxyRotator::new(original).expect("rotator");

    let mut copy = rotator.proxies();
    copy.push(Proxy::Url("http://p3:8080".into()));

    assert_eq!(rotator.len(), 2);
    assert_eq!(rotator.proxies().len(), 2);
}

#[test]
fn test_len_returns_proxy_count() {
    assert_eq!(
        ProxyRotator::from_urls(["http://p1:8080"])
            .expect("rotator")
            .len(),
        1
    );
    assert_eq!(
        ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080"])
            .expect("rotator")
            .len(),
        2
    );
    assert_eq!(
        ProxyRotator::from_urls(["a", "b", "c", "d", "e"])
            .expect("rotator")
            .len(),
        5
    );
}

#[test]
fn test_repr() {
    let rotator = ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080", "http://p3:8080"])
        .expect("rotator");
    assert_eq!(format!("{rotator:?}"), "ProxyRotator(proxies=3)");
    assert_eq!(rotator.to_string(), "ProxyRotator(proxies=3)");
}

// ---- TestProxyRotatorThreadSafety ----

#[test]
fn test_concurrent_get_proxy() {
    let proxies: Vec<Proxy> = (0..10)
        .map(|i| Proxy::Url(format!("http://p{i}:8080")))
        .collect();
    let rotator = Arc::new(ProxyRotator::new(proxies.clone()).expect("rotator"));

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let rotator = Arc::clone(&rotator);
            thread::spawn(move || (0..100).map(|_| rotator.get_proxy()).collect::<Vec<_>>())
        })
        .collect();

    let mut results = Vec::new();
    for handle in handles {
        results.extend(handle.join().expect("thread"));
    }

    assert_eq!(results.len(), 1000);
    assert!(results.iter().all(|proxy| proxies.contains(proxy)));
    // 1000 draws over 10 proxies must land exactly back on the start of the cycle;
    // this is what fails if the lock does not cover the whole strategy call.
    assert_eq!(rotator.current_index(), 0);
}

#[test]
fn test_thread_pool_concurrent_access() {
    let proxies = urls(&["http://p1:8080", "http://p2:8080", "http://p3:8080"]);
    let rotator = Arc::new(ProxyRotator::new(proxies.clone()).expect("rotator"));

    let handles: Vec<_> = (0..100)
        .map(|_| {
            let rotator = Arc::clone(&rotator);
            thread::spawn(move || rotator.get_proxy())
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();

    assert_eq!(results.len(), 100);
    assert!(results.iter().all(|proxy| proxies.contains(proxy)));
    assert_eq!(rotator.current_index(), 100 % 3);
}

// ---- TestIsProxyError ----

#[test]
fn test_proxy_errors_detected() {
    for message in [
        "net::err_proxy_connection_failed",
        "NET::ERR_PROXY_AUTH_FAILED",
        "net::err_tunnel_connection_failed",
        "Connection refused by proxy",
        "Connection reset by peer",
        "Connection timed out while connecting to proxy",
        "Failed to connect to proxy server",
        "Could not resolve proxy host",
    ] {
        assert!(is_proxy_error_message(message), "{message}");
    }
}

#[test]
fn test_non_proxy_errors_not_detected() {
    for message in [
        "Page not found",
        "404 Not Found",
        "Internal server error",
        "DNS resolution failed",
        "SSL certificate error",
        "Timeout waiting for response",
        "Invalid JSON response",
    ] {
        assert!(!is_proxy_error_message(message), "{message}");
    }
}

#[test]
fn test_case_insensitive_detection() {
    assert!(is_proxy_error_message("NET::ERR_PROXY"));
    assert!(is_proxy_error_message("Net::Err_Proxy"));
    assert!(is_proxy_error_message("CONNECTION REFUSED"));
}

#[test]
fn test_empty_error_message() {
    assert!(!is_proxy_error_message(""));
}

#[test]
fn test_custom_exception_types() {
    // `is_proxy_error` accepts anything that `Display`s, which is the Rust equivalent
    // of Python's `str(error)` over an arbitrary `Exception` subclass.
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct CustomError(String);

    assert!(is_proxy_error(&CustomError("net::err_proxy_failed".into())));
    assert!(!is_proxy_error(&CustomError("normal error".into())));
}
