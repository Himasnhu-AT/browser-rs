//! [`merge_request_args`]: the skip set, the `follow_redirects` →
//! `allow_redirects` rename and [`select_random_browser`]. Zero I/O.
//!
//! Port of `_ConfigurationLogic._merge_request_args` (`reference/engines/static.py`).
//! The Python function returns the `**kwargs` dictionary handed straight to
//! `curl_cffi`'s `Session.request()`; here it returns the typed [`MergedRequest`]
//! that [`crate::backend`] consumes.
//!
//! Four behaviours are load bearing:
//!
//! * **presence, not `None`** — the twelve named keys resolve through
//!   [`browser_core::types::Override`], so an explicit `None` beats the session
//!   default (`timeout=None` really means "no timeout");
//! * **the rename** — `follow_redirects` becomes `allow_redirects` on the way out;
//! * **the skip set** — [`SKIP_KEYS`] is subtracted from the pass-through bag, which
//!   is what makes the browser-only `extra_headers` / `google_search` / `block_ads`
//!   arguments silently ignored by an HTTP session (the reference implementation issue #247);
//! * **`None` pass-throughs are dropped** — a named key can be `None`, a
//!   pass-through key cannot, so `Value::Null` entries never reach the wire.

use std::time::Duration;

use indexmap::IndexMap;
use rand::seq::IndexedRandom;
use rand::Rng;
use browser_core::types::{FollowRedirects, Proxy};
use serde_json::Value;

use crate::config::{
    Body, ClientCert, Cookies, FetcherConfig, HeaderDict, HttpVersion, Impersonate, ProxySpec,
    Query, RequestOverrides,
};
use crate::headers_job::headers_job_with;
use crate::proxy::proxy_url;
use browser_fingerprint::headers::{generate_headers, BrowserMode, Headers};
use browser_fingerprint::useragent::default_ua;

/// The fifteen keys the merge consumes itself, plus the three browser-session keys
/// an HTTP session must ignore.
///
/// The first twelve are read into named fields, `retries` / `retry_delay` /
/// `selector_config` are consumed by the retry loop and the response factory, and
/// the last three exist only so that code written for a browser session
/// (`extra_headers`, `google_search`, `block_ads`) can be passed to
/// the `extra` bag of [`RequestOverrides`] without ending up on the wire.
pub const SKIP_KEYS: [&str; 18] = [
    // The twelve keys with a named home in `MergedRequest`.
    "impersonate",
    "http3",
    "stealth",
    "headers",
    "proxies",
    "proxy",
    "proxy_auth",
    "timeout",
    "follow_redirects",
    "max_redirects",
    "verify",
    "cert",
    // Consumed elsewhere in the engine, never sent.
    "retries",
    "retry_delay",
    "selector_config",
    // Browser session params (ignored by HTTP sessions).
    "extra_headers",
    "google_search",
    "block_ads",
];

/// The three browser-only keys inside [`SKIP_KEYS`].
pub const BROWSER_ONLY_SKIP_KEYS: [&str; 3] = ["extra_headers", "google_search", "block_ads"];

/// The name `follow_redirects` is renamed to on the way out — and the one
/// pass-through key that collides with a named output field.
///
/// It is deliberately **not** in [`SKIP_KEYS`] (Python does not skip it either):
/// a caller who spells curl's own keyword wins over the renamed
/// `follow_redirects`, because Python's pass-through loop writes into the same
/// dictionary after the rename has run.
pub const ALLOW_REDIRECTS_KEY: &str = "allow_redirects";

/// `true` when `key` must not be forwarded to the transport.
#[must_use]
pub fn is_skipped_key(key: &str) -> bool {
    SKIP_KEYS.contains(&key)
}

/// The values `_make_request` injects into the merge on every attempt.
///
/// Python passes them as keyword arguments (`stealth=…, proxy=…`), which is why
/// they take precedence over anything in [`RequestOverrides`]: the engine has
/// already resolved them.
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    /// The `stealthy_headers` value the fetcher method popped off its kwargs.
    /// `None` falls back to the `stealthy_headers` override and then to the
    /// session default.
    pub stealth: Option<bool>,
    /// The proxy chosen **for this attempt** — see
    /// [`crate::retry::proxy_for_attempt`], which applies the rotator.
    ///
    /// `_make_request` always injects one, so an engine should always set this.
    /// Leaving it `None` makes the merge fall back to plain `_get_param` semantics
    /// on the `proxy` key (per-request value if mentioned, otherwise the session
    /// default), which is what a *bare* `_merge_request_args(url=…)` call does in
    /// Python — and never advances a rotator.
    pub proxy: Option<Proxy>,
}

impl RequestContext {
    /// An empty context: session stealth, no proxy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the resolved stealth flag.
    #[must_use]
    pub fn with_stealth(mut self, stealth: bool) -> Self {
        self.stealth = Some(stealth);
        self
    }

    /// Set the proxy for this attempt.
    #[must_use]
    pub fn with_proxy(mut self, proxy: Option<Proxy>) -> Self {
        self.proxy = proxy;
        self
    }
}

/// The fully resolved arguments for one HTTP attempt.
///
/// Field names follow the Python dictionary keys, including
/// [`MergedRequest::allow_redirects`] (renamed from `follow_redirects`).
#[derive(Clone, Debug, PartialEq)]
pub struct MergedRequest {
    /// Target URL.
    pub url: String,
    /// Final header map, from [`crate::headers_job::headers_job`].
    pub headers: HeaderDict,
    /// Per-scheme proxy table.
    pub proxies: ProxySpec,
    /// The proxy for this attempt, rendered as a URL for the transport.
    pub proxy: Option<String>,
    /// The same proxy in its original shape — for `Response.meta["proxy"]` and for
    /// retry logging.
    pub proxy_source: Option<Proxy>,
    /// Proxy basic-auth credentials.
    pub proxy_auth: Option<(String, String)>,
    /// Request timeout; `None` means no timeout.
    pub timeout: Option<Duration>,
    /// The redirect policy — Python's `allow_redirects`, renamed from
    /// `follow_redirects` right here.
    pub allow_redirects: FollowRedirects,
    /// Redirect ceiling; `-1` means unlimited.
    pub max_redirects: i32,
    /// TLS peer verification.
    pub verify: bool,
    /// Client certificate.
    pub cert: Option<ClientCert>,
    /// The selected impersonation profile, or `None` when impersonation is off.
    /// A pool has already been sampled at this point.
    pub impersonate: Option<String>,
    /// Forced HTTP version. Set by `http3=true` (to
    /// [`HttpVersion::Http3Only`]), which **overwrites** any caller-supplied value.
    pub http_version: Option<HttpVersion>,
    /// Query-string parameters.
    pub params: Option<Query>,
    /// Request cookies.
    pub cookies: Option<Cookies>,
    /// HTTP basic auth.
    pub auth: Option<(String, String)>,
    /// Request body.
    pub body: Option<Body>,
    /// Pass-through curl options, minus [`SKIP_KEYS`] and minus `Value::Null`.
    pub extra: IndexMap<String, Value>,
}

impl MergedRequest {
    /// `bool(impersonate)` after selection — the empty string is falsy, as in
    /// Python.
    #[must_use]
    pub fn impersonate_enabled(&self) -> bool {
        self.impersonate
            .as_ref()
            .is_some_and(|profile| !profile.is_empty())
    }
}

/// Resolve `impersonate` for one request (`_select_random_browser`).
///
/// A pool is sampled uniformly **per request**, so successive requests on one
/// session rotate fingerprints. An empty pool disables impersonation; a single
/// profile and `None` are returned unchanged.
///
/// # Examples
///
/// ```
/// use browser_http::config::Impersonate;
/// use browser_http::merge::select_random_browser;
///
/// assert_eq!(select_random_browser(&Impersonate::single("chrome")), Some("chrome".to_owned()));
/// assert_eq!(select_random_browser(&Impersonate::None), None);
/// assert_eq!(select_random_browser(&Impersonate::pool(Vec::<String>::new())), None);
/// assert_eq!(select_random_browser(&Impersonate::pool(["chrome"])), Some("chrome".to_owned()));
///
/// let picked = select_random_browser(&Impersonate::pool(["chrome", "firefox"])).unwrap();
/// assert!(["chrome", "firefox"].contains(&picked.as_str()));
/// ```
#[must_use]
pub fn select_random_browser(impersonate: &Impersonate) -> Option<String> {
    select_random_browser_with(impersonate, &mut rand::rng())
}

/// [`select_random_browser`] with an injectable RNG, for deterministic tests.
#[must_use]
pub fn select_random_browser_with<R: Rng + ?Sized>(
    impersonate: &Impersonate,
    rng: &mut R,
) -> Option<String> {
    match impersonate {
        Impersonate::None => None,
        Impersonate::Single(profile) => Some(profile.clone()),
        Impersonate::Pool(profiles) => profiles.choose(rng).cloned(),
    }
}

/// Merge session defaults with per-request overrides into the arguments for one
/// attempt.
///
/// # Examples
///
/// ```
/// use browser_core::types::{FollowRedirects, RedirectMode};
/// use browser_http::config::{FetcherConfig, Impersonate, RequestOverrides};
/// use browser_http::merge::{merge_request_args, RequestContext};
/// use serde_json::Value;
///
/// let config = FetcherConfig::default().with_impersonate(Impersonate::single("chrome"));
/// let overrides = RequestOverrides::new()
///     .follow_redirects(FollowRedirects::Bool(true))
///     .extra("block_ads", Value::Bool(true));       // browser-only: dropped
///
/// let merged = merge_request_args(
///     "https://example.com",
///     &config,
///     &overrides,
///     &RequestContext::new(),
/// );
///
/// assert_eq!(merged.url, "https://example.com");
/// assert_eq!(merged.allow_redirects, FollowRedirects::Bool(true));
/// assert_eq!(merged.impersonate.as_deref(), Some("chrome"));
/// assert!(!merged.extra.contains_key("block_ads"));
/// ```
#[must_use]
pub fn merge_request_args(
    url: &str,
    config: &FetcherConfig,
    overrides: &RequestOverrides,
    context: &RequestContext,
) -> MergedRequest {
    merge_request_args_with(url, config, overrides, context, &mut rand::rng())
}

/// [`merge_request_args`] with an injectable RNG, so a pooled `impersonate` can be
/// pinned in tests.
#[must_use]
pub fn merge_request_args_with<R: Rng + ?Sized>(
    url: &str,
    config: &FetcherConfig,
    overrides: &RequestOverrides,
    context: &RequestContext,
    rng: &mut R,
) -> MergedRequest {
    merge_request_args_seeded(
        url,
        config,
        overrides,
        context,
        rng,
        || generate_headers(BrowserMode::Http),
        default_ua(),
    )
}

/// [`merge_request_args`] with **both** non-deterministic inputs injected: the RNG that
/// samples a pooled `impersonate`, and the header generator plus fallback user agent
/// that [`crate::headers_job::headers_job_with`] takes.
///
/// This is the seam the differential tests drive, so a golden corpus captured from the
/// Python reference can be replayed without depending on a random browserforge sample.
#[must_use]
pub fn merge_request_args_seeded<R, F>(
    url: &str,
    config: &FetcherConfig,
    overrides: &RequestOverrides,
    context: &RequestContext,
    rng: &mut R,
    header_generator: F,
    fallback_user_agent: &str,
) -> MergedRequest
where
    R: Rng + ?Sized,
    F: FnOnce() -> Headers,
{
    let impersonate_setting = overrides
        .impersonate
        .as_ref()
        .into_option()
        .unwrap_or(&config.impersonate);
    let impersonate = select_random_browser_with(impersonate_setting, rng);
    // `bool(impersonate)`: `None` and the empty string are both falsy.
    let impersonate_enabled = impersonate
        .as_ref()
        .is_some_and(|profile| !profile.is_empty());

    let http3_enabled = overrides.http3.or_default(config.http3);
    let stealth = context.stealth.unwrap_or_else(|| {
        overrides
            .stealthy_headers
            .or_default(config.stealthy_headers)
    });

    let request_headers = overrides
        .headers
        .as_ref()
        .into_option()
        .unwrap_or(&config.headers);

    let mut merged = MergedRequest {
        url: url.to_owned(),
        headers: headers_job_with(
            url,
            &config.headers,
            request_headers,
            stealth,
            impersonate_enabled,
            header_generator,
            fallback_user_agent,
        ),
        proxies: overrides
            .proxies
            .as_ref()
            .into_option()
            .cloned()
            .unwrap_or_else(|| config.proxies.clone()),
        // `_make_request` always injects `proxy=`, so the context wins outright —
        // it is the value the rotator (or the static/session fallback) produced.
        // When nothing was injected we redo the *rotator-free* half of that
        // resolution (`static_proxy or self._default_proxy`), which keeps a bare
        // `merge_request_args` call honest without ever advancing a rotator.
        proxy: None,
        proxy_source: None,
        proxy_auth: overrides
            .proxy_auth
            .as_ref()
            .into_option()
            .cloned()
            .unwrap_or_else(|| config.proxy_auth.clone()),
        timeout: overrides.timeout.or_default(config.timeout),
        // The rename: `follow_redirects` in, `allow_redirects` out.
        allow_redirects: overrides
            .follow_redirects
            .or_default(config.follow_redirects),
        max_redirects: overrides.max_redirects.or_default(config.max_redirects),
        verify: overrides.verify.or_default(config.verify),
        cert: overrides
            .cert
            .as_ref()
            .into_option()
            .cloned()
            .unwrap_or_else(|| config.cert.clone()),
        impersonate,
        http_version: overrides.http_version.into_option(),
        params: overrides.params.as_ref().into_option().cloned(),
        cookies: overrides.cookies.as_ref().into_option().cloned(),
        auth: overrides.auth.as_ref().into_option().cloned(),
        body: overrides.body.as_ref().into_option().cloned(),
        extra: IndexMap::new(),
    };

    let proxy = context
        .proxy
        .clone()
        .or_else(|| merge_proxy_param(config, overrides));
    merged.proxy = proxy.as_ref().map(proxy_url);
    merged.proxy_source = proxy;

    // The pass-through loop: skip the keys handled above, drop explicit nulls.
    for (key, value) in &overrides.extra {
        if !is_skipped_key(key) && !value.is_null() {
            merged.extra.insert(key.clone(), value.clone());
        }
    }

    // `allow_redirects` is the *output* name of `follow_redirects` and is not in
    // the skip set, so in Python the pass-through loop runs last and overwrites the
    // value the rename produced (`final_args["allow_redirects"] = v`). Reproduce
    // that here, where the transport reads the named field rather than the bag.
    // A value that is not a legal redirect policy stays in the bag for the
    // transport to reject, as it would in Python.
    if let Some(value) = merged.extra.get(ALLOW_REDIRECTS_KEY) {
        if let Ok(policy) = serde_json::from_value::<FollowRedirects>(value.clone()) {
            merged.allow_redirects = policy;
            merged.extra.shift_remove(ALLOW_REDIRECTS_KEY);
        }
    }

    if http3_enabled {
        // Python writes `final_args["http_version"]` here, clobbering whatever the
        // pass-through loop put there a moment earlier.
        merged.extra.shift_remove("http_version");
        merged.http_version = Some(HttpVersion::Http3Only);
        if impersonate_enabled {
            tracing::warn!(
                "The argument `http3` might cause errors if used with `impersonate` argument, \
                 try switching it off if you encounter any curl errors."
            );
        }
    }

    merged
}

/// `_get_param(method_kwargs, "proxy", self._default_proxy)` — pure **presence**
/// semantics, used by the merge itself.
///
/// An explicit `proxy=None` therefore wins over the session default, and an explicit
/// `proxy=""` is forwarded verbatim: Python's `or None` collapse only ever ran on the
/// *session* value, inside `__init__`.
fn merge_proxy_param(config: &FetcherConfig, overrides: &RequestOverrides) -> Option<Proxy> {
    match overrides.proxy.as_ref() {
        browser_core::types::Override::Set(proxy) => proxy.clone(),
        browser_core::types::Override::Unset => config.proxy.clone(),
    }
}

/// `static_proxy or self._default_proxy` — the **truthiness** fallback
/// `_make_request` performs before it calls the merge. Used by
/// [`crate::retry::proxy_for_attempt`], where a falsy per-request proxy really does
/// fall back to the session's.
pub(crate) fn static_proxy(config: &FetcherConfig, overrides: &RequestOverrides) -> Option<Proxy> {
    overrides
        .proxy
        .as_ref()
        .into_option()
        .and_then(Option::as_ref)
        .filter(|proxy| crate::config::proxy_is_truthy(proxy))
        .or_else(|| {
            config
                .proxy
                .as_ref()
                .filter(|proxy| crate::config::proxy_is_truthy(proxy))
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use rand::rngs::ThreadRng;
    use browser_core::types::{Override, RedirectMode};
    use serde_json::json;

    use super::*;
    use crate::config::{ProxySpec, SelectorConfig};

    fn rng() -> ThreadRng {
        // The RNG only ever picks from an `impersonate` pool; every assertion
        // below is written to hold for any draw, so the thread RNG is enough and
        // no seeded generator (an extra `rand` feature) is needed.
        rand::rng()
    }

    fn merge(config: &FetcherConfig, overrides: &RequestOverrides) -> MergedRequest {
        merge_request_args_with(
            "https://example.com",
            config,
            overrides,
            &RequestContext::new(),
            &mut rng(),
        )
    }

    // --- _select_random_browser ------------------------------------------

    #[test]
    fn select_random_browser_matches_test_impersonate_list() {
        assert_eq!(
            select_random_browser(&Impersonate::single("chrome")),
            Some("chrome".to_owned())
        );
        assert_eq!(select_random_browser(&Impersonate::None), None);
        assert_eq!(select_random_browser(&Impersonate::Pool(Vec::new())), None);
        assert_eq!(
            select_random_browser(&Impersonate::pool(["chrome"])),
            Some("chrome".to_owned())
        );
        let pool = ["chrome", "firefox", "safari260"];
        for _ in 0..32 {
            let picked = select_random_browser(&Impersonate::pool(pool)).unwrap();
            assert!(pool.contains(&picked.as_str()));
        }
    }

    #[test]
    fn a_pool_is_stored_unchanged_and_sampled_per_request() {
        let config = FetcherConfig::default()
            .with_impersonate(Impersonate::pool(["chrome150", "firefox147"]));
        assert_eq!(
            config.impersonate,
            Impersonate::Pool(vec!["chrome150".to_owned(), "firefox147".to_owned()]),
            "the session stores the pool, it does not select eagerly"
        );
        let picked = merge(&config, &RequestOverrides::new())
            .impersonate
            .unwrap();
        assert!(["chrome150", "firefox147"].contains(&picked.as_str()));
    }

    #[test]
    fn a_per_request_impersonate_overrides_the_session() {
        let config = FetcherConfig::default().with_impersonate(Impersonate::single("chrome"));
        let merged = merge(
            &config,
            &RequestOverrides::new().impersonate(Impersonate::single("firefox147")),
        );
        assert_eq!(merged.impersonate.as_deref(), Some("firefox147"));
        assert!(merged.impersonate_enabled());

        let disabled = merge(
            &config,
            &RequestOverrides::new().impersonate(Impersonate::None),
        );
        assert_eq!(disabled.impersonate, None);
        assert!(!disabled.impersonate_enabled());
    }

    #[test]
    fn an_empty_profile_string_is_falsy_like_python() {
        let config = FetcherConfig::default().with_impersonate(Impersonate::single(""));
        let merged = merge(&config, &RequestOverrides::new());
        assert_eq!(merged.impersonate.as_deref(), Some(""));
        assert!(!merged.impersonate_enabled());
        // …so the generated header set is added.
        assert!(merged.headers.len() > 1);
    }

    // --- the named keys ---------------------------------------------------

    #[test]
    fn the_url_is_always_present() {
        let merged = merge(&FetcherConfig::default(), &RequestOverrides::new());
        assert_eq!(merged.url, "https://example.com");
    }

    #[test]
    fn follow_redirects_is_renamed_to_allow_redirects() {
        let merged = merge(&FetcherConfig::default(), &RequestOverrides::new());
        assert_eq!(
            merged.allow_redirects,
            FollowRedirects::Mode(RedirectMode::Safe)
        );
        let merged = merge(
            &FetcherConfig::default(),
            &RequestOverrides::new().follow_redirects(FollowRedirects::Bool(false)),
        );
        assert_eq!(merged.allow_redirects, FollowRedirects::Bool(false));
        assert!(!merged.extra.contains_key("follow_redirects"));
        assert!(!merged.extra.contains_key("allow_redirects"));
    }

    #[test]
    fn session_defaults_flow_through_untouched() {
        let config = FetcherConfig::default()
            .with_timeout(Some(Duration::from_secs(5)))
            .with_max_redirects(-1)
            .with_verify(false)
            .with_proxies(ProxySpec::all("http://spec:1"))
            .with_proxy_auth(Some(("u".to_owned(), "p".to_owned())))
            .with_cert(Some(ClientCert::File("/tmp/cert.pem".into())));
        let merged = merge(&config, &RequestOverrides::new());
        assert_eq!(merged.timeout, Some(Duration::from_secs(5)));
        assert_eq!(merged.max_redirects, -1);
        assert!(!merged.verify);
        assert_eq!(merged.proxies, ProxySpec::all("http://spec:1"));
        assert_eq!(merged.proxy_auth, Some(("u".to_owned(), "p".to_owned())));
        assert_eq!(merged.cert, Some(ClientCert::File("/tmp/cert.pem".into())));
    }

    #[test]
    fn an_explicit_none_beats_the_session_default() {
        let config = FetcherConfig::default().with_timeout(Some(Duration::from_secs(30)));
        let merged = merge(&config, &RequestOverrides::new().timeout(None));
        assert_eq!(merged.timeout, None);

        let config = FetcherConfig::default().with_cert(Some(ClientCert::Pair {
            cert: "/c".into(),
            key: "/k".into(),
        }));
        let merged = merge(&config, &RequestOverrides::new().cert(None));
        assert_eq!(merged.cert, None);
    }

    #[test]
    fn the_injected_proxy_wins_and_is_rendered_as_a_url() {
        let config = FetcherConfig::default().with_proxy_url("http://session:1");
        // Nothing injected: the session proxy is still what Python would have
        // resolved and injected.
        let bare = merge(&config, &RequestOverrides::new());
        assert_eq!(bare.proxy.as_deref(), Some("http://session:1"));
        assert_eq!(
            bare.proxy_source,
            Some(Proxy::Url("http://session:1".into()))
        );

        // A per-request proxy beats the session one.
        let pinned = merge(
            &config,
            &RequestOverrides::new().proxy_url("http://request:2"),
        );
        assert_eq!(pinned.proxy.as_deref(), Some("http://request:2"));

        // Inside the merge, `proxy` resolves through `_get_param`, which keys on
        // *presence*: an explicit `proxy=None` beats the session default, and an
        // explicit `proxy=""` is forwarded verbatim. (The truthiness fallback
        // `static_proxy or self._default_proxy` lives one level up, in
        // `retry::proxy_for_attempt`.)
        let explicit_none = merge(&config, &RequestOverrides::new().proxy(None));
        assert_eq!(explicit_none.proxy, None);
        assert_eq!(explicit_none.proxy_source, None);

        let explicit_empty = merge(&config, &RequestOverrides::new().proxy_url(""));
        assert_eq!(explicit_empty.proxy.as_deref(), Some(""));

        // A per-request dict proxy keeps its shape in `proxy_source`.
        let dict = merge(
            &config,
            &RequestOverrides::new().proxy(Some(Proxy::Config {
                server: "http://p:8080".into(),
                username: Some("u".into()),
                password: Some("pw".into()),
            })),
        );
        assert_eq!(dict.proxy.as_deref(), Some("http://u:pw@p:8080"));
        assert!(matches!(dict.proxy_source, Some(Proxy::Config { .. })));

        assert_eq!(
            merge(&FetcherConfig::default(), &RequestOverrides::new()).proxy,
            None
        );

        let context = RequestContext::new().with_proxy(Some(Proxy::Config {
            server: "http://rotated:8080".into(),
            username: Some("u".into()),
            password: Some("p".into()),
        }));
        let merged = merge_request_args_with(
            "https://example.com",
            &config,
            &RequestOverrides::new(),
            &context,
            &mut rng(),
        );
        assert_eq!(merged.proxy.as_deref(), Some("http://u:p@rotated:8080"));
        assert_eq!(merged.proxy_source, context.proxy);
    }

    // --- headers ----------------------------------------------------------

    #[test]
    fn headers_come_from_the_header_job_and_merge_session_with_request() {
        let mut session_headers = HeaderDict::new();
        session_headers.insert("X-Session".to_owned(), "1".to_owned());
        let config = FetcherConfig::default()
            .with_headers(session_headers)
            .with_impersonate(Impersonate::single("chrome"));

        let merged = merge(&config, &RequestOverrides::new().header("X-Request", "2"));
        assert_eq!(
            merged.headers.get("X-Session").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            merged.headers.get("X-Request").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            merged.headers.get("referer").map(String::as_str),
            Some("https://www.google.com/")
        );
        assert!(!merged.extra.contains_key("headers"));
    }

    #[test]
    fn stealth_resolution_prefers_the_context_then_the_override() {
        let config = FetcherConfig::default()
            .with_stealthy_headers(true)
            .with_impersonate(Impersonate::single("chrome"));

        // Session default: stealth on -> a referer appears.
        assert!(merge(&config, &RequestOverrides::new())
            .headers
            .contains_key("referer"));

        // Per-request override turns it off.
        let off = merge(&config, &RequestOverrides::new().stealthy_headers(false));
        assert!(off.headers.is_empty());

        // The injected context beats both.
        let forced = merge_request_args_with(
            "https://example.com",
            &config,
            &RequestOverrides::new().stealthy_headers(false),
            &RequestContext::new().with_stealth(true),
            &mut rng(),
        );
        assert!(forced.headers.contains_key("referer"));
    }

    // --- the pass-through bag --------------------------------------------

    #[test]
    fn browser_only_keys_never_reach_the_wire() {
        let overrides = RequestOverrides::new()
            .extra("block_ads", json!(true))
            .extra("google_search", json!(true))
            .extra("extra_headers", json!({"X-Extra": "1"}));
        let merged = merge(&FetcherConfig::default(), &overrides);
        for key in BROWSER_ONLY_SKIP_KEYS {
            assert!(!merged.extra.contains_key(key), "{key} leaked");
        }
        assert!(merged.extra.is_empty());
    }

    #[test]
    fn every_skip_key_is_filtered_out_of_the_bag() {
        let mut overrides = RequestOverrides::new();
        for key in SKIP_KEYS {
            overrides = overrides.extra(key, json!("leaked"));
        }
        overrides = overrides.extra("files", json!({"f": "x"}));
        let merged = merge(&FetcherConfig::default(), &overrides);
        assert_eq!(merged.extra.len(), 1);
        assert_eq!(merged.extra.get("files"), Some(&json!({"f": "x"})));
    }

    #[test]
    fn pass_through_values_survive_but_nulls_are_dropped() {
        let overrides = RequestOverrides::new()
            .extra("referer", json!("https://ref.test/"))
            .extra("accept_encoding", json!("gzip"))
            .extra("stream", json!(true))
            .extra("interface", Value::Null);
        let merged = merge(&FetcherConfig::default(), &overrides);
        assert_eq!(merged.extra.len(), 3);
        assert_eq!(
            merged.extra.get("referer"),
            Some(&json!("https://ref.test/"))
        );
        assert!(!merged.extra.contains_key("interface"));
        // Insertion order is preserved for reproducible requests.
        assert_eq!(
            merged.extra.keys().collect::<Vec<_>>(),
            vec!["referer", "accept_encoding", "stream"]
        );
    }

    #[test]
    fn cookies_params_auth_and_bodies_pass_through_verbatim() {
        let mut cookies = Cookies::new();
        cookies.insert("session".to_owned(), "abc".to_owned());
        let overrides = RequestOverrides::new()
            .cookies(cookies.clone())
            .params(vec![("q".to_owned(), "rust".to_owned())])
            .auth("user", "pass")
            .json(json!({"a": 1}));
        let merged = merge(&FetcherConfig::default(), &overrides);
        assert_eq!(merged.cookies, Some(cookies));
        assert_eq!(
            merged.params,
            Some(vec![("q".to_owned(), "rust".to_owned())])
        );
        assert_eq!(merged.auth, Some(("user".to_owned(), "pass".to_owned())));
        assert_eq!(merged.body, Some(Body::Json(json!({"a": 1}))));
    }

    #[test]
    fn selector_config_and_retry_knobs_never_reach_the_request() {
        let overrides = RequestOverrides::new()
            .selector_config(SelectorConfig::new().with_adaptive_domain("x.test"))
            .retries(Some(7))
            .retry_delay(Duration::from_secs(9));
        let merged = merge(&FetcherConfig::default(), &overrides);
        assert!(merged.extra.is_empty());
    }

    // --- http3 ------------------------------------------------------------

    #[test]
    fn http3_forces_v3only_and_overwrites_a_supplied_http_version() {
        let config = FetcherConfig::default().with_http3(true);
        let merged = merge(
            &config,
            &RequestOverrides::new().http_version(HttpVersion::Http2),
        );
        assert_eq!(merged.http_version, Some(HttpVersion::Http3Only));
        assert_eq!(merged.http_version.unwrap().curl_code(), 31);

        // Even when smuggled in through the pass-through bag.
        let merged = merge(
            &config,
            &RequestOverrides::new().extra("http_version", json!(3)),
        );
        assert_eq!(merged.http_version, Some(HttpVersion::Http3Only));
        assert!(!merged.extra.contains_key("http_version"));
    }

    #[test]
    fn without_http3_a_supplied_http_version_survives() {
        let merged = merge(
            &FetcherConfig::default(),
            &RequestOverrides::new().http_version(HttpVersion::Http2Tls),
        );
        assert_eq!(merged.http_version, Some(HttpVersion::Http2Tls));

        let merged = merge(&FetcherConfig::default(), &RequestOverrides::new());
        assert_eq!(merged.http_version, None);
    }

    #[test]
    fn per_request_http3_beats_the_session() {
        let config = FetcherConfig::default().with_http3(false);
        let merged = merge(&config, &RequestOverrides::new().http3(true));
        assert_eq!(merged.http_version, Some(HttpVersion::Http3Only));

        let config = FetcherConfig::default().with_http3(true);
        let merged = merge(&config, &RequestOverrides::new().http3(false));
        assert_eq!(merged.http_version, None);
    }

    #[test]
    fn overrides_are_presence_based_not_none_based() {
        // A sanity check on the primitive the whole merge is built from.
        let unset: Override<Option<Duration>> = Override::Unset;
        assert_eq!(
            unset.or_default(Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            Override::Set(None).or_default(Some(Duration::from_secs(30))),
            None
        );
    }
}
