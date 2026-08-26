//! [`FetcherConfig`] (session defaults) and [`RequestOverrides`] (per-request
//! values), built on [`Override`] so "not mentioned" stays distinguishable from an
//! explicit `None`.
//!
//! This is the Rust shape of `_ConfigurationLogic.__init__` in
//! `reference/engines/static.py` plus the `RequestsSession` / `GetRequestParams` /
//! `DataRequestParams` TypedDicts in `reference/engines/_browsers/_types.py`.
//!
//! # Why `Override<T>` and not `Option<T>`
//!
//! Python resolves per-request values with
//!
//! ```python
//! kwargs[key] if key in kwargs else default
//! ```
//!
//! which keys on **presence**, not on `None`. `Fetcher.get(url, timeout=None)` really
//! does mean "no timeout", overriding the session's 30 s, and `session.get(url,
//! retries=0)` really does mean one attempt. A bare `Option<Duration>` cannot say
//! that, so nullable knobs are typed `Override<Option<T>>`: `Unset` is "not
//! mentioned", `Set(None)` is "explicitly nothing".
//!
//! # Falsy normalisation
//!
//! The Python constructor writes `kwargs.get("proxy") or None`, not
//! `kwargs.get("proxy")`, for `proxies`, `proxy`, `proxy_auth`, `headers`, `cert` and
//! `selector_config`. An empty string or empty dict therefore collapses to the empty
//! default. [`FetcherConfig::normalized`] reproduces that; the builder methods call
//! it for you. `timeout`, `retries`, `retry_delay`, `follow_redirects`,
//! `max_redirects`, `verify`, `http3` and `impersonate` are stored **as given**, so
//! `timeout: None`, `retries: Some(0)` and `verify: false` all survive.
//!
//! Two of those six are *vacuous* in the port and are deliberately not normalised:
//! `proxy_auth` is a two-string tuple, which is truthy in Python even when both
//! strings are empty (only the empty tuple is falsy, and that shape is not
//! representable here); the empty `ProxySpec` / `HeaderDict` / `SelectorConfig` /
//! `Option<ClientCert>` already *are* the empty default, so `or None` is a no-op on
//! them.
//!
//! # Presence vs truthiness for `proxy`
//!
//! `proxy` is the one key resolved **twice**, with different rules on each side, and
//! the two must not be conflated:
//!
//! * [`crate::merge::merge_request_args`] uses `_get_param` — pure presence. An
//!   explicit `proxy=None` beats the session default and an explicit `proxy=""` is
//!   forwarded verbatim.
//! * [`crate::retry::proxy_for_attempt`] reproduces `_make_request`'s
//!   `static_proxy or self._default_proxy` — truthiness — after an *identity* test
//!   (`static_proxy is None`) that decides whether the rotator runs at all.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use browser_fingerprint::impersonate::ImpersonateProfile;
use bytes::Bytes;
use indexmap::IndexMap;
use serde_json::Value;
use thiserror::Error;

use crate::proxy::ProxyRotator;

/// Re-exported from `browser-core` because [`FetcherConfig`],
/// [`RequestOverrides`] and [`crate::merge::MergedRequest`] all carry them in
/// **public** fields. Without this a downstream crate — unit 12's driver
/// included — cannot name the type of a field it can read.
pub use browser_core::types::{FollowRedirects, Override, Proxy, RedirectMode};

/// An ordered, case-preserving header map.
///
/// Deliberately **not** [`http::HeaderMap`]: Python's headers are a plain `dict`, so
/// key case is preserved (`"User-Agent"` and `"user-agent"` are two different keys
/// that can coexist) and insertion order is the wire order — which is exactly what a
/// fingerprinting-sensitive client needs. `IndexMap::insert` keeps an existing key's
/// position while replacing its value, matching `{**a, **b}`.
pub type HeaderDict = IndexMap<String, String>;

/// The session default impersonation profile (`impersonate="chrome"`).
pub const DEFAULT_IMPERSONATE: &str = "chrome";
/// The session default timeout (`timeout=30`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The session default attempt count (`retries=3`).
pub const DEFAULT_RETRIES: i32 = 3;
/// The session default pause between attempts (`retry_delay=1`).
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// The session default redirect ceiling (`max_redirects=30`); `-1` means unlimited.
pub const DEFAULT_MAX_REDIRECTS: i32 = 30;

/// The seven keys `BaseFetcher.configure()` accepts (`parser_keywords`).
pub const PARSER_KEYWORDS: [&str; 7] = [
    "huge_tree",
    "adaptive",
    "storage",
    "keep_cdata",
    "storage_args",
    "keep_comments",
    "adaptive_domain",
];

// ---------------------------------------------------------------------------
// Impersonation
// ---------------------------------------------------------------------------

/// `ImpersonateType = BrowserTypeLiteral | List[BrowserTypeLiteral] | None`.
///
/// A [`Impersonate::Pool`] is resolved **per request** — see
/// [`crate::merge::select_random_browser`] — so successive requests on one session
/// rotate fingerprints. The profile string is forwarded verbatim; the reference implementation never
/// resolves aliases itself (`browser_fingerprint::impersonate` can, if you want to
/// validate first).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Impersonate {
    /// Impersonation disabled (`impersonate=None`).
    None,
    /// One profile, e.g. `"chrome"` or `"firefox147"`.
    Single(String),
    /// A pool; one element is drawn uniformly per request. An **empty** pool
    /// disables impersonation, exactly like `impersonate=[]` in Python.
    Pool(Vec<String>),
}

impl Default for Impersonate {
    fn default() -> Self {
        Self::Single(DEFAULT_IMPERSONATE.to_owned())
    }
}

impl Impersonate {
    /// One profile by name.
    pub fn single(profile: impl Into<String>) -> Self {
        Self::Single(profile.into())
    }

    /// A pool of profiles.
    pub fn pool<I, S>(profiles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Pool(profiles.into_iter().map(Into::into).collect())
    }

    /// Profile names that neither `curl-impersonate` nor `wreq` would recognise.
    ///
    /// Purely advisory: the reference implementation forwards the string verbatim and lets the
    /// transport complain, so this never rejects anything — it exists so a CLI or a
    /// session builder can warn early. Canonical names, current aliases and the
    /// deprecated aliases all count as known.
    ///
    /// # Examples
    ///
    /// ```
    /// use browser_http::config::Impersonate;
    ///
    /// assert!(Impersonate::single("chrome").unknown_profiles().is_empty());
    /// assert_eq!(
    ///     Impersonate::pool(["chrome", "netscape"]).unknown_profiles(),
    ///     vec!["netscape"]
    /// );
    /// ```
    #[must_use]
    pub fn unknown_profiles(&self) -> Vec<&str> {
        // `parse` already resolves the current and the deprecated aliases.
        let known = |profile: &str| ImpersonateProfile::parse(profile).is_ok();
        match self {
            Self::None => Vec::new(),
            Self::Single(profile) => {
                if profile.is_empty() || known(profile.as_str()) {
                    Vec::new()
                } else {
                    vec![profile.as_str()]
                }
            }
            Self::Pool(profiles) => profiles
                .iter()
                .map(String::as_str)
                .filter(|profile| !profile.is_empty() && !known(profile))
                .collect(),
        }
    }

    /// `true` when this configuration can ever yield a profile.
    ///
    /// Mirrors Python's `bool(impersonate)` *after* selection: `None`, an empty
    /// pool and the empty string are all falsy.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        match self {
            Self::None => false,
            Self::Single(profile) => !profile.is_empty(),
            Self::Pool(profiles) => !profiles.is_empty(),
        }
    }
}

impl From<&str> for Impersonate {
    fn from(value: &str) -> Self {
        Self::Single(value.to_owned())
    }
}

impl From<String> for Impersonate {
    fn from(value: String) -> Self {
        Self::Single(value)
    }
}

impl From<Option<String>> for Impersonate {
    fn from(value: Option<String>) -> Self {
        value.map_or(Self::None, Self::Single)
    }
}

impl From<Vec<String>> for Impersonate {
    fn from(value: Vec<String>) -> Self {
        Self::Pool(value)
    }
}

// ---------------------------------------------------------------------------
// Redirects, HTTP version, proxies, certificates
// ---------------------------------------------------------------------------

/// The `CURLFOLLOW_*` codes behind [`FollowRedirects`].
///
/// [`FollowRedirects`] lives in `browser-core` (it is shared with the browser
/// engines), so the HTTP-specific mapping is an extension trait here.
pub trait FollowRedirectsExt {
    /// libcurl's `CURLFOLLOW_*` value: `0` none, `1` all, `2` obeycode,
    /// `3` firstonly, `4` safe.
    fn curl_code(self) -> u32;
    /// `false` only for `FollowRedirects::Bool(false)`.
    fn follows(self) -> bool;
}

impl FollowRedirectsExt for FollowRedirects {
    fn curl_code(self) -> u32 {
        match self {
            Self::Bool(false) => 0,
            Self::Bool(true) | Self::Mode(RedirectMode::All) => 1,
            Self::Mode(RedirectMode::ObeyCode) => 2,
            Self::Mode(RedirectMode::FirstOnly) => 3,
            Self::Mode(RedirectMode::Safe) => 4,
        }
    }

    fn follows(self) -> bool {
        !matches!(self, Self::Bool(false))
    }
}

/// The session default redirect policy (`follow_redirects="safe"`).
///
/// Note this differs from [`FollowRedirects::default`], which is `true`: the HTTP
/// session's own default is the SSRF-guarding `"safe"` mode.
#[must_use]
pub fn default_follow_redirects() -> FollowRedirects {
    FollowRedirects::Mode(RedirectMode::Safe)
}

/// curl's `CurlHttpVersion` enum, needed because `http3=true` forces `V3Only`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HttpVersion {
    /// `NONE` (0) — let the transport decide.
    #[default]
    None,
    /// `V1_0` (1).
    Http1_0,
    /// `V1_1` (2).
    Http1_1,
    /// `V2_0` (3).
    Http2,
    /// `V2TLS` (4).
    Http2Tls,
    /// `V2_PRIOR_KNOWLEDGE` (5).
    Http2PriorKnowledge,
    /// `V3` (30) — try HTTP/3, fall back.
    Http3,
    /// `V3ONLY` (31) — what `http3=true` selects.
    Http3Only,
}

impl HttpVersion {
    /// The numeric value curl expects.
    #[must_use]
    pub const fn curl_code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Http1_0 => 1,
            Self::Http1_1 => 2,
            Self::Http2 => 3,
            Self::Http2Tls => 4,
            Self::Http2PriorKnowledge => 5,
            Self::Http3 => 30,
            Self::Http3Only => 31,
        }
    }
}

/// curl's `ProxySpec`: a per-scheme proxy table.
///
/// Empty by default (`proxies or {}`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxySpec {
    /// `"all"` — used for every scheme.
    pub all: Option<String>,
    /// `"http"`.
    pub http: Option<String>,
    /// `"https"`.
    pub https: Option<String>,
    /// `"ws"`.
    pub ws: Option<String>,
    /// `"wss"`.
    pub wss: Option<String>,
}

impl ProxySpec {
    /// One proxy for every scheme (`{"all": url}`).
    pub fn all(url: impl Into<String>) -> Self {
        Self {
            all: Some(url.into()),
            ..Self::default()
        }
    }

    /// `true` when no scheme has a proxy — Python's falsy `{}`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.all.is_none()
            && self.http.is_none()
            && self.https.is_none()
            && self.ws.is_none()
            && self.wss.is_none()
    }

    /// The populated entries as `(scheme, url)` pairs, in curl's key order.
    #[must_use]
    pub fn entries(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        for (key, value) in [
            ("all", &self.all),
            ("http", &self.http),
            ("https", &self.https),
            ("ws", &self.ws),
            ("wss", &self.wss),
        ] {
            if let Some(url) = value {
                out.push((key, url.as_str()));
            }
        }
        out
    }
}

/// Python's `bool(proxy)` for a `ProxyType`.
///
/// A proxy reaches the merge as either a string or a dict, and the reference implementation leans on
/// truthiness in two places (`kwargs.get("proxy") or None` and
/// `static_proxy or self._default_proxy`). The empty string and the empty dict are
/// falsy; a dict with any key — including `{"server": ""}` — is truthy, which is why
/// an empty `server` still counts here.
///
/// # Examples
///
/// ```
/// use browser_core::types::Proxy;
/// use browser_http::config::proxy_is_truthy;
///
/// assert!(proxy_is_truthy(&Proxy::Url("http://p:1".to_owned())));
/// assert!(!proxy_is_truthy(&Proxy::Url(String::new())));
/// assert!(proxy_is_truthy(&Proxy::Config {
///     server: String::new(),
///     username: None,
///     password: None,
/// }));
/// ```
#[must_use]
pub fn proxy_is_truthy(proxy: &Proxy) -> bool {
    match proxy {
        Proxy::Url(url) => !url.is_empty(),
        // A `Proxy::Config` always carries at least the `server` key, so the Python
        // dict it models is never the empty dict.
        Proxy::Config { .. } => true,
    }
}

/// A client certificate: either one bundled file, or a certificate and its key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ClientCert {
    /// `cert="/path/to/bundle.pem"`.
    File(PathBuf),
    /// `cert=("/path/cert.pem", "/path/key.pem")`.
    Pair {
        /// The certificate file.
        cert: PathBuf,
        /// The private-key file.
        key: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Bodies and query parameters
// ---------------------------------------------------------------------------

/// Query-string parameters (`params=`), ordered and allowing repeated keys.
pub type Query = Vec<(String, String)>;

/// Request cookies (`cookies=`).
pub type Cookies = IndexMap<String, String>;

/// A request body: `data=`, `json=` or a raw payload.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// `data={"a": "b"}` — url-encoded form pairs, order preserved.
    Form(Vec<(String, String)>),
    /// `json={...}` — serialised as JSON with `Content-Type: application/json`.
    Json(Value),
    /// `data="..."` — a raw text body.
    Text(String),
    /// `data=b"..."` — raw bytes.
    Raw(Bytes),
}

// ---------------------------------------------------------------------------
// Selector configuration
// ---------------------------------------------------------------------------

/// The `selector_config` dictionary forwarded to the parser when the response is
/// built.
///
/// Kept as an ordered string→JSON map rather than a struct on purpose: Python
/// merges these dictionaries key by key (`{**class_defaults, **per_call}`) and
/// forwards unknown keys straight to the `Selector` constructor, and a struct would
/// silently drop the keys a future parser version adds. The typed accessors below
/// cover the seven [`PARSER_KEYWORDS`].
///
/// # Examples
///
/// ```
/// use browser_http::config::SelectorConfig;
///
/// let defaults = SelectorConfig::parser_defaults();
/// assert!(defaults.huge_tree());
/// assert!(!defaults.adaptive());
///
/// let per_call = SelectorConfig::new().with_adaptive_domain("from-request.test");
/// let merged = defaults.merged_with(&per_call);
/// assert_eq!(merged.adaptive_domain(), Some("from-request.test"));
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectorConfig(IndexMap<String, Value>);

impl SelectorConfig {
    /// An empty configuration (Python's falsy `{}`).
    #[must_use]
    pub fn new() -> Self {
        Self(IndexMap::new())
    }

    /// `BaseFetcher._generate_parser_arguments()`: the class-level defaults
    /// `huge_tree=True, keep_comments=False, keep_cdata=False, adaptive=False,
    /// adaptive_domain=""`.
    ///
    /// `storage` and `storage_args` are omitted — they are `None` in Python and the
    /// storage backend is chosen by `browser-storage`, not here.
    #[must_use]
    pub fn parser_defaults() -> Self {
        let mut map = IndexMap::new();
        map.insert("huge_tree".to_owned(), Value::Bool(true));
        map.insert("keep_comments".to_owned(), Value::Bool(false));
        map.insert("keep_cdata".to_owned(), Value::Bool(false));
        map.insert("adaptive".to_owned(), Value::Bool(false));
        map.insert("adaptive_domain".to_owned(), Value::String(String::new()));
        Self(map)
    }

    /// `true` when no key is set (Python's falsy `{}`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of configured keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Read a raw key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Write a raw key, returning the previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.0.insert(key.into(), value)
    }

    /// Builder form of [`SelectorConfig::insert`].
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: Value) -> Self {
        self.0.insert(key.into(), value);
        self
    }

    /// Set `adaptive_domain`, the one key that **replaces the response URL** on the
    /// parsed document.
    #[must_use]
    pub fn with_adaptive_domain(self, domain: impl Into<String>) -> Self {
        self.with("adaptive_domain", Value::String(domain.into()))
    }

    /// `huge_tree`, defaulting to `true`.
    #[must_use]
    pub fn huge_tree(&self) -> bool {
        self.bool_or("huge_tree", true)
    }

    /// `adaptive`, defaulting to `false`.
    #[must_use]
    pub fn adaptive(&self) -> bool {
        self.bool_or("adaptive", false)
    }

    /// `keep_comments`, defaulting to `false`.
    #[must_use]
    pub fn keep_comments(&self) -> bool {
        self.bool_or("keep_comments", false)
    }

    /// `keep_cdata`, defaulting to `false`.
    #[must_use]
    pub fn keep_cdata(&self) -> bool {
        self.bool_or("keep_cdata", false)
    }

    /// `adaptive_domain`, `None` when absent or empty — Python treats `""` as
    /// "unset" (`adaptive_domain or url`).
    #[must_use]
    pub fn adaptive_domain(&self) -> Option<&str> {
        match self.0.get("adaptive_domain") {
            Some(Value::String(domain)) if !domain.is_empty() => Some(domain),
            _ => None,
        }
    }

    /// `{**self, **other}`: `other` wins key by key, `self` fixes the order of the
    /// keys they share.
    #[must_use]
    pub fn merged_with(&self, other: &Self) -> Self {
        let mut merged = self.0.clone();
        for (key, value) in &other.0 {
            merged.insert(key.clone(), value.clone());
        }
        Self(merged)
    }

    /// Borrow the underlying map, e.g. to feed a parser builder.
    #[must_use]
    pub fn as_map(&self) -> &IndexMap<String, Value> {
        &self.0
    }

    fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.0.get(key) {
            Some(Value::Bool(value)) => *value,
            _ => default,
        }
    }
}

impl From<IndexMap<String, Value>> for SelectorConfig {
    fn from(value: IndexMap<String, Value>) -> Self {
        Self(value)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Configuration errors raised before any I/O happens.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// A rotator was combined with a static proxy.
    ///
    /// Python raises this from `_ConfigurationLogic.__init__`, i.e. only when the
    /// engine is built on `__enter__`; `http-fetch.md` §2.3 explicitly allows a port
    /// to hoist it into the constructor, which [`FetcherConfig::validate`] does.
    #[error("Cannot use 'proxy_rotator' together with 'proxy' or 'proxies'. Use either a static proxy or proxy rotation, not both.")]
    ProxyRotatorConflict,
}

// ---------------------------------------------------------------------------
// FetcherConfig
// ---------------------------------------------------------------------------

/// Session-level defaults — the `FetcherSession(...)` keyword arguments.
///
/// Every field is public; the `with_*` methods exist for fluent construction and
/// apply Python's falsy normalisation as they go.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use browser_http::config::{FetcherConfig, Impersonate};
///
/// let cfg = FetcherConfig::default()
///     .with_impersonate(Impersonate::pool(["chrome", "firefox"]))
///     .with_timeout(None)
///     .with_proxy_url("http://user:pass@host:8080");
///
/// assert!(cfg.timeout.is_none());
/// assert!(cfg.validate().is_ok());
/// ```
#[derive(Clone, Debug)]
pub struct FetcherConfig {
    /// TLS/HTTP2 fingerprint profile. Default `"chrome"`.
    pub impersonate: Impersonate,
    /// Force HTTP/3 only. Default `false`.
    pub http3: bool,
    /// Generate realistic headers plus a Google referer. Default `true`.
    pub stealthy_headers: bool,
    /// Per-scheme proxy table. Default empty.
    pub proxies: ProxySpec,
    /// A single proxy, either a URL or Playwright's three-field dictionary
    /// (Python's `ProxyType = Union[str, Dict[str, str]]`). Default `None`.
    pub proxy: Option<Proxy>,
    /// Basic auth for the proxy. Default `None`.
    pub proxy_auth: Option<(String, String)>,
    /// Request timeout. `None` means "no timeout" and is a legal, distinct value.
    /// Default `Some(30s)`.
    pub timeout: Option<Duration>,
    /// Headers merged into every request. Default empty.
    pub headers: HeaderDict,
    /// Total attempt count — **not** extra attempts. `None`, `0` and negative all
    /// collapse to one attempt (see [`crate::retry::attempts_from_retries`]).
    /// Default `Some(3)`.
    pub retries: Option<i32>,
    /// Fixed pause between attempts; no backoff, no jitter. Default 1 s.
    pub retry_delay: Duration,
    /// Redirect policy. Default [`RedirectMode::Safe`].
    pub follow_redirects: FollowRedirects,
    /// Redirect ceiling; `-1` means unlimited. Default 30.
    pub max_redirects: i32,
    /// TLS peer verification. Default `true`.
    pub verify: bool,
    /// Client certificate. Default `None`.
    pub cert: Option<ClientCert>,
    /// Extra arguments for the parser that builds the response document.
    pub selector_config: SelectorConfig,
    /// Rotates the proxy per **attempt**. Mutually exclusive with `proxy`/`proxies`.
    pub proxy_rotator: Option<Arc<ProxyRotator>>,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        Self {
            impersonate: Impersonate::default(),
            http3: false,
            stealthy_headers: true,
            proxies: ProxySpec::default(),
            proxy: None,
            proxy_auth: None,
            timeout: Some(DEFAULT_TIMEOUT),
            headers: HeaderDict::new(),
            retries: Some(DEFAULT_RETRIES),
            retry_delay: DEFAULT_RETRY_DELAY,
            follow_redirects: default_follow_redirects(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            verify: true,
            cert: None,
            selector_config: SelectorConfig::new(),
            proxy_rotator: None,
        }
    }
}

impl FetcherConfig {
    /// The Python defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply Python's `x or default` normalisation: an empty proxy string, empty
    /// proxy table, empty header map or empty credential pair collapses to `None` /
    /// empty.
    ///
    /// Idempotent, and called by every `with_*` setter and by
    /// [`FetcherConfig::validate`].
    #[must_use]
    pub fn normalized(mut self) -> Self {
        // `self._default_proxy = kwargs.get("proxy") or None`: only a *falsy* proxy
        // collapses, i.e. the empty string or the empty dict. `proxy_auth` gets the
        // same `or None`, but a two-string tuple is never falsy in Python — even
        // `("", "")` — so it is deliberately left alone.
        if self
            .proxy
            .as_ref()
            .is_some_and(|proxy| !proxy_is_truthy(proxy))
        {
            self.proxy = None;
        }
        self
    }

    /// Check the one invariant Python enforces (`http-fetch.md` §2.3).
    ///
    /// # Errors
    ///
    /// [`ConfigError::ProxyRotatorConflict`] when a rotator is combined with a
    /// static `proxy` or a non-empty `proxies` table.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.proxy_rotator.is_some()
            && (self.proxy.as_ref().is_some_and(proxy_is_truthy) || !self.proxies.is_empty())
        {
            return Err(ConfigError::ProxyRotatorConflict);
        }
        Ok(())
    }

    /// [`FetcherConfig::normalized`] followed by [`FetcherConfig::validate`].
    ///
    /// # Errors
    ///
    /// As [`FetcherConfig::validate`].
    pub fn build(self) -> Result<Self, ConfigError> {
        let config = self.normalized();
        config.validate()?;
        Ok(config)
    }

    /// Set the impersonation profile (or pool).
    #[must_use]
    pub fn with_impersonate(mut self, impersonate: impl Into<Impersonate>) -> Self {
        self.impersonate = impersonate.into();
        self
    }

    /// Force HTTP/3 only.
    #[must_use]
    pub fn with_http3(mut self, http3: bool) -> Self {
        self.http3 = http3;
        self
    }

    /// Toggle stealthy header synthesis.
    #[must_use]
    pub fn with_stealthy_headers(mut self, stealthy: bool) -> Self {
        self.stealthy_headers = stealthy;
        self
    }

    /// Set the per-scheme proxy table.
    #[must_use]
    pub fn with_proxies(mut self, proxies: ProxySpec) -> Self {
        self.proxies = proxies;
        self
    }

    /// Set the static proxy; a falsy value (empty URL, empty `server`) collapses to
    /// `None`, exactly as Python's `kwargs.get("proxy") or None` does.
    #[must_use]
    pub fn with_proxy(mut self, proxy: Option<Proxy>) -> Self {
        self.proxy = proxy;
        self.normalized()
    }

    /// Set the static proxy from a URL string.
    #[must_use]
    pub fn with_proxy_url(self, proxy: impl Into<String>) -> Self {
        self.with_proxy(Some(Proxy::Url(proxy.into())))
    }

    /// Set proxy basic-auth credentials.
    #[must_use]
    pub fn with_proxy_auth(mut self, auth: Option<(String, String)>) -> Self {
        self.proxy_auth = auth;
        self.normalized()
    }

    /// Set the timeout. `None` means "no timeout".
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Replace the session headers.
    #[must_use]
    pub fn with_headers(mut self, headers: HeaderDict) -> Self {
        self.headers = headers;
        self
    }

    /// Add one session header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Set the attempt count.
    #[must_use]
    pub fn with_retries(mut self, retries: Option<i32>) -> Self {
        self.retries = retries;
        self
    }

    /// Set the pause between attempts.
    #[must_use]
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    /// Set the redirect policy.
    #[must_use]
    pub fn with_follow_redirects(mut self, follow: FollowRedirects) -> Self {
        self.follow_redirects = follow;
        self
    }

    /// Set the redirect ceiling (`-1` = unlimited).
    #[must_use]
    pub fn with_max_redirects(mut self, max_redirects: i32) -> Self {
        self.max_redirects = max_redirects;
        self
    }

    /// Toggle TLS verification.
    #[must_use]
    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify = verify;
        self
    }

    /// Set the client certificate.
    #[must_use]
    pub fn with_cert(mut self, cert: Option<ClientCert>) -> Self {
        self.cert = cert;
        self
    }

    /// Set the parser configuration.
    #[must_use]
    pub fn with_selector_config(mut self, selector_config: SelectorConfig) -> Self {
        self.selector_config = selector_config;
        self
    }

    /// Attach a proxy rotator. Validate afterwards — a rotator plus a static proxy
    /// is an error.
    #[must_use]
    pub fn with_proxy_rotator(mut self, rotator: Option<Arc<ProxyRotator>>) -> Self {
        self.proxy_rotator = rotator;
        self
    }
}

// ---------------------------------------------------------------------------
// RequestOverrides
// ---------------------------------------------------------------------------

/// Per-request keyword arguments.
///
/// Everything defaults to [`Override::Unset`], i.e. "fall back to the session".
/// `Override::Set(None)` is the explicit `None` Python lets you pass.
///
/// # Examples
///
/// ```
/// use browser_core::types::Override;
/// use browser_http::config::RequestOverrides;
///
/// // `Fetcher.get(url, timeout=None)` — no timeout, overriding the session's 30 s.
/// let overrides = RequestOverrides::new().timeout(None);
/// assert_eq!(overrides.timeout, Override::Set(None));
///
/// // Not mentioning it keeps the session default.
/// assert!(RequestOverrides::new().timeout.is_unset());
/// ```
#[derive(Clone, Debug, Default)]
pub struct RequestOverrides {
    /// `impersonate=`.
    pub impersonate: Override<Impersonate>,
    /// `http3=`.
    pub http3: Override<bool>,
    /// `stealthy_headers=` — renamed to `stealth` on the way into the engine.
    pub stealthy_headers: Override<bool>,
    /// `proxies=`.
    pub proxies: Override<ProxySpec>,
    /// `proxy=`. Consumed by [`crate::retry::proxy_for_attempt`]: a per-request
    /// proxy **disables the rotator** for that request. Like the session default it
    /// accepts either a URL or a Playwright-style dictionary.
    pub proxy: Override<Option<Proxy>>,
    /// `proxy_auth=`.
    pub proxy_auth: Override<Option<(String, String)>>,
    /// `timeout=`; `Set(None)` means "no timeout".
    pub timeout: Override<Option<Duration>>,
    /// `headers=`.
    pub headers: Override<HeaderDict>,
    /// `retries=`; `Set(None)`, `Set(Some(0))` and negatives all mean one attempt.
    pub retries: Override<Option<i32>>,
    /// `retry_delay=`.
    pub retry_delay: Override<Duration>,
    /// `follow_redirects=` — becomes `allow_redirects` in the merged request.
    pub follow_redirects: Override<FollowRedirects>,
    /// `max_redirects=`.
    pub max_redirects: Override<i32>,
    /// `verify=`.
    pub verify: Override<bool>,
    /// `cert=`.
    pub cert: Override<Option<ClientCert>>,
    /// `selector_config=`; an **empty** map falls back to the session's, matching
    /// Python's `... or self.selector_config`.
    pub selector_config: Override<SelectorConfig>,
    /// `http_version=`. A pass-through curl option in Python; typed here because
    /// `http3=true` overwrites it (see [`crate::merge::merge_request_args`]).
    pub http_version: Override<HttpVersion>,
    /// `params=` — the query string.
    pub params: Override<Query>,
    /// `cookies=`.
    pub cookies: Override<Cookies>,
    /// `auth=` — HTTP basic auth only.
    pub auth: Override<(String, String)>,
    /// `data=` / `json=`.
    pub body: Override<Body>,
    /// Every other curl keyword the reference implementation forwards verbatim (`files`, `multipart`,
    /// `referer`, `accept_encoding`, `ja3`, `akamai`, `interface`, `stream`, …).
    ///
    /// `Value::Null` entries are dropped by the merge, exactly like Python's
    /// `if v is not None`, and any key in [`crate::merge::SKIP_KEYS`] is ignored.
    pub extra: IndexMap<String, Value>,
}

impl RequestOverrides {
    /// No overrides at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `impersonate=`.
    #[must_use]
    pub fn impersonate(mut self, impersonate: impl Into<Impersonate>) -> Self {
        self.impersonate = Override::Set(impersonate.into());
        self
    }

    /// `http3=`.
    #[must_use]
    pub fn http3(mut self, http3: bool) -> Self {
        self.http3 = Override::Set(http3);
        self
    }

    /// `stealthy_headers=`.
    #[must_use]
    pub fn stealthy_headers(mut self, stealthy: bool) -> Self {
        self.stealthy_headers = Override::Set(stealthy);
        self
    }

    /// `proxies=`.
    #[must_use]
    pub fn proxies(mut self, proxies: ProxySpec) -> Self {
        self.proxies = Override::Set(proxies);
        self
    }

    /// `proxy=`. `Set(None)` is an explicit "no proxy for this request" and beats
    /// the session default; leaving it `Unset` inherits it.
    #[must_use]
    pub fn proxy(mut self, proxy: Option<Proxy>) -> Self {
        self.proxy = Override::Set(proxy);
        self
    }

    /// `proxy=` from a URL string.
    #[must_use]
    pub fn proxy_url(self, proxy: impl Into<String>) -> Self {
        self.proxy(Some(Proxy::Url(proxy.into())))
    }

    /// `proxy_auth=`.
    #[must_use]
    pub fn proxy_auth(mut self, auth: Option<(String, String)>) -> Self {
        self.proxy_auth = Override::Set(auth);
        self
    }

    /// `timeout=`; pass `None` for "no timeout".
    #[must_use]
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = Override::Set(timeout);
        self
    }

    /// `headers=`.
    #[must_use]
    pub fn headers(mut self, headers: HeaderDict) -> Self {
        self.headers = Override::Set(headers);
        self
    }

    /// Add one request header, starting from the overrides already set.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let mut headers = match self.headers {
            Override::Set(headers) => headers,
            Override::Unset => HeaderDict::new(),
        };
        headers.insert(name.into(), value.into());
        self.headers = Override::Set(headers);
        self
    }

    /// `retries=`.
    #[must_use]
    pub fn retries(mut self, retries: Option<i32>) -> Self {
        self.retries = Override::Set(retries);
        self
    }

    /// `retry_delay=`.
    #[must_use]
    pub fn retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = Override::Set(delay);
        self
    }

    /// `follow_redirects=`.
    #[must_use]
    pub fn follow_redirects(mut self, follow: FollowRedirects) -> Self {
        self.follow_redirects = Override::Set(follow);
        self
    }

    /// `max_redirects=`.
    #[must_use]
    pub fn max_redirects(mut self, max_redirects: i32) -> Self {
        self.max_redirects = Override::Set(max_redirects);
        self
    }

    /// `verify=`.
    #[must_use]
    pub fn verify(mut self, verify: bool) -> Self {
        self.verify = Override::Set(verify);
        self
    }

    /// `cert=`.
    #[must_use]
    pub fn cert(mut self, cert: Option<ClientCert>) -> Self {
        self.cert = Override::Set(cert);
        self
    }

    /// `selector_config=`.
    #[must_use]
    pub fn selector_config(mut self, selector_config: SelectorConfig) -> Self {
        self.selector_config = Override::Set(selector_config);
        self
    }

    /// `http_version=`.
    #[must_use]
    pub fn http_version(mut self, version: HttpVersion) -> Self {
        self.http_version = Override::Set(version);
        self
    }

    /// `params=`.
    #[must_use]
    pub fn params(mut self, params: Query) -> Self {
        self.params = Override::Set(params);
        self
    }

    /// `cookies=`.
    #[must_use]
    pub fn cookies(mut self, cookies: Cookies) -> Self {
        self.cookies = Override::Set(cookies);
        self
    }

    /// `auth=`.
    #[must_use]
    pub fn auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Override::Set((username.into(), password.into()));
        self
    }

    /// `data=` / `json=`.
    #[must_use]
    pub fn body(mut self, body: Body) -> Self {
        self.body = Override::Set(body);
        self
    }

    /// `json=` shorthand.
    #[must_use]
    pub fn json(self, value: Value) -> Self {
        self.body(Body::Json(value))
    }

    /// `data={...}` shorthand.
    #[must_use]
    pub fn form<I, K, V>(self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.body(Body::Form(
            pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        ))
    }

    /// Add a pass-through curl keyword.
    #[must_use]
    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

/// Resolve the parser configuration for one request.
///
/// Python: `_get_param(kwargs, "selector_config", self.selector_config) or
/// self.selector_config` — a *falsy* (empty) per-request map falls back to the
/// session's, which is why this is not a plain [`Override::or_default`].
///
/// # Examples
///
/// ```
/// use browser_http::config::{
///     resolve_selector_config, FetcherConfig, RequestOverrides, SelectorConfig,
/// };
///
/// let session = FetcherConfig::default()
///     .with_selector_config(SelectorConfig::new().with_adaptive_domain("session.test"));
///
/// // An empty per-request map falls back to the session's.
/// let empty = RequestOverrides::new().selector_config(SelectorConfig::new());
/// assert_eq!(resolve_selector_config(&session, &empty).adaptive_domain(), Some("session.test"));
///
/// // A non-empty one replaces it wholesale.
/// let given = RequestOverrides::new()
///     .selector_config(SelectorConfig::new().with_adaptive_domain("from-request.test"));
/// assert_eq!(
///     resolve_selector_config(&session, &given).adaptive_domain(),
///     Some("from-request.test")
/// );
/// ```
#[must_use]
pub fn resolve_selector_config(
    config: &FetcherConfig,
    overrides: &RequestOverrides,
) -> SelectorConfig {
    match overrides.selector_config.as_ref() {
        Override::Set(selector_config) if !selector_config.is_empty() => selector_config.clone(),
        _ => config.selector_config.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let cfg = FetcherConfig::default();
        assert_eq!(cfg.impersonate, Impersonate::Single("chrome".to_owned()));
        assert!(!cfg.http3);
        assert!(cfg.stealthy_headers);
        assert!(cfg.proxies.is_empty());
        assert_eq!(cfg.proxy, None);
        assert_eq!(cfg.proxy_auth, None);
        assert_eq!(cfg.timeout, Some(Duration::from_secs(30)));
        assert!(cfg.headers.is_empty());
        assert_eq!(cfg.retries, Some(3));
        assert_eq!(cfg.retry_delay, Duration::from_secs(1));
        assert_eq!(
            cfg.follow_redirects,
            FollowRedirects::Mode(RedirectMode::Safe)
        );
        assert_eq!(cfg.max_redirects, 30);
        assert!(cfg.verify);
        assert!(cfg.cert.is_none());
        assert!(cfg.selector_config.is_empty());
        assert!(cfg.proxy_rotator.is_none());
    }

    #[test]
    fn falsy_values_normalize_but_meaningful_zeros_survive() {
        let cfg = FetcherConfig::default()
            .with_proxy_url("")
            .with_proxy_auth(Some((String::new(), String::new())))
            .with_timeout(None)
            .with_retries(Some(0))
            .with_verify(false)
            .with_max_redirects(-1);
        assert_eq!(cfg.proxy, None);
        // `("", "")` is a *truthy* two-element tuple in Python, so
        // `kwargs.get("proxy_auth") or None` keeps it. Only the empty tuple is falsy
        // there, and that shape is not representable here.
        assert_eq!(cfg.proxy_auth, Some((String::new(), String::new())));
        assert_eq!(cfg.timeout, None);
        assert_eq!(cfg.retries, Some(0));
        assert!(!cfg.verify);
        assert_eq!(cfg.max_redirects, -1);
    }

    #[test]
    fn rotator_conflicts_with_static_proxies() {
        let rotator = Arc::new(ProxyRotator::from_urls(["http://p1"]).unwrap());

        let with_proxy = FetcherConfig::default()
            .with_proxy_url("http://static")
            .with_proxy_rotator(Some(Arc::clone(&rotator)));
        assert_eq!(
            with_proxy.validate(),
            Err(ConfigError::ProxyRotatorConflict)
        );

        let with_proxies = FetcherConfig::default()
            .with_proxies(ProxySpec::all("http://static"))
            .with_proxy_rotator(Some(Arc::clone(&rotator)));
        assert!(with_proxies.build().is_err());

        let alone = FetcherConfig::default().with_proxy_rotator(Some(rotator));
        assert!(alone.validate().is_ok());
    }

    #[test]
    fn rotator_conflict_message_matches_python() {
        assert_eq!(
            ConfigError::ProxyRotatorConflict.to_string(),
            "Cannot use 'proxy_rotator' together with 'proxy' or 'proxies'. \
             Use either a static proxy or proxy rotation, not both."
        );
    }

    #[test]
    fn impersonate_truthiness() {
        assert!(Impersonate::default().is_enabled());
        assert!(!Impersonate::None.is_enabled());
        assert!(!Impersonate::Single(String::new()).is_enabled());
        assert!(!Impersonate::Pool(Vec::new()).is_enabled());
        assert!(Impersonate::pool(["chrome"]).is_enabled());
        assert_eq!(
            Impersonate::from("edge"),
            Impersonate::Single("edge".into())
        );
        assert!(Impersonate::single("chrome150")
            .unknown_profiles()
            .is_empty());
        assert!(Impersonate::single("safari15_3")
            .unknown_profiles()
            .is_empty());
        assert!(Impersonate::None.unknown_profiles().is_empty());
        assert!(Impersonate::single("").unknown_profiles().is_empty());
        assert_eq!(
            Impersonate::pool(["chrome", "netscape4"]).unknown_profiles(),
            vec!["netscape4"]
        );
        assert_eq!(Impersonate::from(None::<String>), Impersonate::None);
    }

    #[test]
    fn follow_redirects_curl_codes() {
        assert_eq!(FollowRedirects::Bool(false).curl_code(), 0);
        assert_eq!(FollowRedirects::Bool(true).curl_code(), 1);
        assert_eq!(FollowRedirects::Mode(RedirectMode::All).curl_code(), 1);
        assert_eq!(FollowRedirects::Mode(RedirectMode::ObeyCode).curl_code(), 2);
        assert_eq!(
            FollowRedirects::Mode(RedirectMode::FirstOnly).curl_code(),
            3
        );
        assert_eq!(FollowRedirects::Mode(RedirectMode::Safe).curl_code(), 4);
        assert!(!FollowRedirects::Bool(false).follows());
        assert!(default_follow_redirects().follows());
    }

    #[test]
    fn http_version_codes_match_curl() {
        assert_eq!(HttpVersion::None.curl_code(), 0);
        assert_eq!(HttpVersion::Http1_0.curl_code(), 1);
        assert_eq!(HttpVersion::Http1_1.curl_code(), 2);
        assert_eq!(HttpVersion::Http2.curl_code(), 3);
        assert_eq!(HttpVersion::Http2Tls.curl_code(), 4);
        assert_eq!(HttpVersion::Http2PriorKnowledge.curl_code(), 5);
        assert_eq!(HttpVersion::Http3.curl_code(), 30);
        assert_eq!(HttpVersion::Http3Only.curl_code(), 31);
    }

    #[test]
    fn proxy_spec_entries_are_ordered() {
        let spec = ProxySpec {
            all: None,
            http: Some("http://h".to_owned()),
            https: Some("http://s".to_owned()),
            ws: None,
            wss: None,
        };
        assert!(!spec.is_empty());
        assert_eq!(
            spec.entries(),
            vec![("http", "http://h"), ("https", "http://s")]
        );
        assert!(ProxySpec::default().is_empty());
        assert_eq!(ProxySpec::all("u").entries(), vec![("all", "u")]);
    }

    #[test]
    fn selector_config_merges_key_by_key() {
        let defaults = SelectorConfig::parser_defaults();
        assert!(defaults.huge_tree());
        assert!(!defaults.adaptive());
        assert!(!defaults.keep_comments());
        assert!(!defaults.keep_cdata());
        assert_eq!(defaults.adaptive_domain(), None);
        assert_eq!(defaults.len(), 5);

        let per_call = SelectorConfig::new()
            .with("adaptive", Value::Bool(true))
            .with_adaptive_domain("configured.test");
        let merged = defaults.merged_with(&per_call);
        assert!(merged.adaptive());
        assert!(merged.huge_tree());
        assert_eq!(merged.adaptive_domain(), Some("configured.test"));
        // Key order follows the left-hand map for shared keys.
        assert_eq!(
            merged.as_map().keys().collect::<Vec<_>>(),
            vec![
                "huge_tree",
                "keep_comments",
                "keep_cdata",
                "adaptive",
                "adaptive_domain"
            ]
        );
        // Unknown keys survive so a future parser argument is not swallowed.
        let exotic = SelectorConfig::new().with("future_knob", Value::from(7));
        assert_eq!(
            defaults.merged_with(&exotic).get("future_knob"),
            Some(&Value::from(7))
        );
    }

    #[test]
    fn selector_config_resolution_prefers_a_non_empty_request_map() {
        let session = FetcherConfig::default()
            .with_selector_config(SelectorConfig::new().with_adaptive_domain("session.test"));

        assert_eq!(
            resolve_selector_config(&session, &RequestOverrides::new()).adaptive_domain(),
            Some("session.test")
        );
        assert_eq!(
            resolve_selector_config(
                &session,
                &RequestOverrides::new().selector_config(SelectorConfig::new())
            )
            .adaptive_domain(),
            Some("session.test")
        );
        assert_eq!(
            resolve_selector_config(
                &session,
                &RequestOverrides::new()
                    .selector_config(SelectorConfig::new().with_adaptive_domain("request.test"))
            )
            .adaptive_domain(),
            Some("request.test")
        );
    }

    #[test]
    fn overrides_default_to_unset_and_record_explicit_none() {
        let untouched = RequestOverrides::new();
        assert!(untouched.timeout.is_unset());
        assert!(untouched.retries.is_unset());
        assert!(untouched.extra.is_empty());

        let explicit = RequestOverrides::new().timeout(None).retries(None);
        assert_eq!(explicit.timeout, Override::Set(None));
        assert_eq!(explicit.retries, Override::Set(None));
        assert_eq!(
            explicit.timeout.or_default(Some(DEFAULT_TIMEOUT)),
            None,
            "an explicit None must beat the session default"
        );
    }

    #[test]
    fn override_builders_cover_the_request_surface() {
        let overrides = RequestOverrides::new()
            .header("X-A", "1")
            .header("X-B", "2")
            .params(vec![("q".to_owned(), "rust".to_owned())])
            .auth("user", "pass")
            .json(Value::from(true))
            .extra("stream", Value::Bool(true))
            .http_version(HttpVersion::Http2);

        let headers = overrides.headers.clone().into_option().unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("X-A").map(String::as_str), Some("1"));
        assert_eq!(
            overrides.auth,
            Override::Set(("user".into(), "pass".into()))
        );
        assert_eq!(overrides.body, Override::Set(Body::Json(Value::from(true))));
        assert_eq!(overrides.extra.get("stream"), Some(&Value::Bool(true)));
        assert_eq!(overrides.http_version, Override::Set(HttpVersion::Http2));
    }

    #[test]
    fn form_shorthand_preserves_order() {
        let overrides = RequestOverrides::new().form([("b", "2"), ("a", "1")]);
        assert_eq!(
            overrides.body,
            Override::Set(Body::Form(vec![
                ("b".to_owned(), "2".to_owned()),
                ("a".to_owned(), "1".to_owned()),
            ]))
        );
    }
}
