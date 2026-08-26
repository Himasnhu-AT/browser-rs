//! [`AsyncFetcherSession`] (pooled connections, cookie jar) and the one-shot
//! [`AsyncFetcher`].
//!
//! This is `_make_request` — the loop `http-fetch.md` §3.4 specifies — wired to
//! unit 11's pure policy types. Nothing here re-derives a value: [`crate::merge`]
//! resolves the arguments, [`crate::retry`] decides the attempts and the proxy,
//! [`crate::redirect_safe`] walks the chain, [`crate::backend`] performs the
//! exchange and [`crate::response`] builds the answer.
//!
//! ```text
//! for attempt in 0..policy.attempts {
//!     proxy   = proxy_for_attempt(config, overrides)      // rotates per attempt
//!     merged  = merge_request_args(url, config, overrides, context)
//!     request = WireRequest::from_merged(method, url, merged)
//!     match follow_redirects(backend, resolver, jar, request, redirect_policy) {
//!         Ok(outcome)                 => return Response::from_wire(..)
//!         Err(e) if e.is_retryable()  => { log; sleep(policy.delay); }
//!         Err(e)                      => return Err(e)
//!     }
//! }
//! ```
//!
//! # Sessions versus one-shot fetchers
//!
//! Python gives `Fetcher.get(...)` a brand-new `curl_cffi` session per call and
//! closes it in a `finally`, because `curl_cffi` caches impersonation state on
//! the handle. The observable consequences are **no cookie persistence** and
//! **no connection reuse** between one-shot calls.
//!
//! Here the impersonation profile is chosen per request by the backend, so the
//! handle-caching problem does not exist and `ARCHITECTURE.md` §6.5 sanctions
//! sharing one pooled client. The port therefore keeps the *observable* half —
//! [`AsyncFetcher`] gives every request a fresh, empty [`CookieJar`], so nothing
//! leaks between calls — and drops the *unobservable* half, reusing connections.
//! [`AsyncFetcherSession`] keeps a jar for its whole life, as Python's does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use indexmap::IndexMap;
use parking_lot::RwLock;
use serde_json::Value;
use url::Url;

use browser_core::types::Proxy;

use crate::backend::{HttpBackend, Method, WireRequest, WireResponse};
use crate::config::{
    resolve_selector_config, Body, ClientCert, Cookies, FetcherConfig, HeaderDict, HttpVersion,
    Impersonate, ProxySpec, Query, RequestOverrides, SelectorConfig,
};
use crate::error::{FetchError, Result};
use crate::merge::{merge_request_args, RequestContext};
use crate::redirect_safe::{
    follow_redirects, HopObserver, HostResolver, RedirectPolicy, SystemResolver,
};
use crate::response::{DocumentHook, Response, ResponseFactory, META_PROXY_KEY};
use crate::retry::{log_failure, log_retry, proxy_for_attempt, RetryPolicy};

// ---------------------------------------------------------------------------
// Cookie jar
// ---------------------------------------------------------------------------

/// One stored cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredCookie {
    name: String,
    value: String,
    /// The domain, never with a leading dot.
    domain: String,
    /// `true` when the cookie had no `Domain=` attribute, i.e. it belongs to
    /// exactly one host.
    host_only: bool,
    path: String,
    secure: bool,
    expires: Option<cookie::time::OffsetDateTime>,
}

impl StoredCookie {
    fn matches(&self, url: &Url, now: cookie::time::OffsetDateTime) -> bool {
        if self.is_expired(now) {
            return false;
        }
        if self.secure && url.scheme() != "https" {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        if !domain_matches(host, &self.domain, self.host_only) {
            return false;
        }
        path_matches(url.path(), &self.path)
    }

    fn is_expired(&self, now: cookie::time::OffsetDateTime) -> bool {
        self.expires.is_some_and(|expires| expires <= now)
    }
}

/// RFC 6265 §5.1.3, minus the public-suffix check: a `Domain=` cookie matches
/// the domain itself and any subdomain of it.
fn domain_matches(host: &str, domain: &str, host_only: bool) -> bool {
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    if host_only {
        return host == domain;
    }
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// RFC 6265 §5.1.4: a prefix match that respects segment boundaries.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if cookie_path == "/" || request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

/// RFC 6265 §5.1.4's default path: the request path up to its last `/`.
fn default_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return "/".to_owned();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
    }
}

/// A cookie jar shared by every request of one session.
///
/// `curl_cffi` builds `Cookies` from `Set-Cookie` only, and so does this. The
/// implementation is RFC 6265's matching rules minus the public-suffix check —
/// which is a *transport* concern in curl, not a reference implementation one, and which
/// `browser-storage`'s `registrable_domain` could supply if a caller needs it.
///
/// # Examples
///
/// ```
/// use browser_http::session::CookieJar;
/// use url::Url;
///
/// let url = Url::parse("https://shop.test/catalogue")?;
/// let jar = CookieJar::new();
/// jar.store(&url, "sid=abc; Path=/; HttpOnly");
/// jar.store(&url, "theme=dark");
///
/// assert_eq!(jar.header_for(&url).as_deref(), Some("sid=abc; theme=dark"));
/// assert_eq!(jar.matching(&url)["sid"], "abc");
///
/// // A different host sees nothing.
/// let other = Url::parse("https://other.test/")?;
/// assert!(jar.header_for(&other).is_none());
/// # Ok::<(), url::ParseError>(())
/// ```
#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: RwLock<Vec<StoredCookie>>,
}

impl CookieJar {
    /// An empty jar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store one `Set-Cookie` value, as seen from `url`.
    ///
    /// An unparsable value is dropped with a `debug` line, never an error —
    /// `curl_cffi` ignores malformed cookies too.
    pub fn store(&self, url: &Url, set_cookie: &str) {
        let Ok(parsed) = cookie::Cookie::parse(set_cookie.to_owned()) else {
            tracing::debug!(value = set_cookie, "ignoring an unparsable Set-Cookie");
            return;
        };
        let Some(host) = url.host_str() else { return };

        let host_only = parsed.domain().is_none();
        let domain = parsed
            .domain()
            .map_or_else(|| host.to_ascii_lowercase(), str::to_ascii_lowercase);
        let path = parsed
            .path()
            .filter(|path| path.starts_with('/'))
            .map_or_else(|| default_path(url), str::to_owned);
        let expires = match parsed.max_age() {
            // `Max-Age` wins over `Expires`, per RFC 6265 §5.3.
            Some(max_age) => Some(cookie::time::OffsetDateTime::now_utc() + max_age),
            None => parsed.expires().and_then(|e| e.datetime()),
        };

        let entry = StoredCookie {
            name: parsed.name().to_owned(),
            value: parsed.value().to_owned(),
            domain,
            host_only,
            path,
            secure: parsed.secure().unwrap_or(false),
            expires,
        };

        let mut cookies = self.cookies.write();
        // RFC 6265: (name, domain, path) is the identity.
        if let Some(slot) = cookies
            .iter_mut()
            .find(|c| c.name == entry.name && c.domain == entry.domain && c.path == entry.path)
        {
            *slot = entry;
        } else {
            cookies.push(entry);
        }
        // A `Max-Age=0` / past `Expires` deletes rather than stores.
        let now = cookie::time::OffsetDateTime::now_utc();
        cookies.retain(|cookie| !cookie.is_expired(now));
    }

    /// Store every line of [`crate::backend::WireResponse::set_cookies`].
    ///
    /// This is what the driver uses. The *folded* header cannot be used for the
    /// job: repeats are joined on `", "` to match Python's
    /// `dict(response.headers)`, and a cookie's `Expires` attribute carries a
    /// comma of its own, so the join is not reversible.
    pub fn store_all(&self, url: &Url, set_cookies: &[String]) {
        for line in set_cookies {
            if !line.trim().is_empty() {
                self.store(url, line);
            }
        }
    }

    /// Store every `Set-Cookie` in a response header map.
    ///
    /// A convenience for callers holding only a [`HeaderDict`]; a value carrying
    /// several `\n`-separated lines is split. Prefer [`CookieJar::store_all`],
    /// which cannot be confused by a comma inside an `Expires`.
    pub fn store_response(&self, url: &Url, headers: &HeaderDict) {
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("set-cookie") {
                for line in value.split('\n') {
                    if !line.trim().is_empty() {
                        self.store(url, line);
                    }
                }
            }
        }
    }

    /// Every cookie that applies to `url`, in insertion order.
    #[must_use]
    pub fn matching(&self, url: &Url) -> IndexMap<String, String> {
        let now = cookie::time::OffsetDateTime::now_utc();
        let mut out = IndexMap::new();
        for cookie in self.cookies.read().iter() {
            if cookie.matches(url, now) {
                out.insert(cookie.name.clone(), cookie.value.clone());
            }
        }
        out
    }

    /// The `Cookie:` header value for `url`, or `None` when nothing applies.
    #[must_use]
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let pairs = self.matching(url);
        if pairs.is_empty() {
            return None;
        }
        Some(
            pairs
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Forget everything.
    pub fn clear(&self) {
        self.cookies.write().clear();
    }

    /// How many cookies are held, expired ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cookies.read().len()
    }

    /// Whether the jar is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Applies and harvests a [`CookieJar`] around every redirect hop.
struct JarObserver {
    jar: Arc<CookieJar>,
    /// The `cookies=` the caller passed for this request, held in a jar of their
    /// own so they neither pollute a session's jar nor escape the cookie rules.
    ///
    /// `curl_cffi` loads `cookies=` into curl's cookie engine with the request
    /// host and `Path=/` (`Cookies.get_cookies_for_curl`, plus
    /// `CURLOPT_COOKIELIST "ALL"` first so they never persist), which is exactly
    /// what this reproduces: they follow a same-host redirect — a different port
    /// included, because cookies ignore ports — and never reach another host.
    overlay: Arc<CookieJar>,
    /// A `Cookie:` header the **caller** wrote by hand, if there was one.
    ///
    /// libcurl lets a custom `Cookie:` header override the cookie engine
    /// outright, and so does this: measured (`tools/corpus/u12`,
    /// `echo-cookie-header-and-param` and `session-cookie-header-and-jar`) a
    /// hand-written header beats both a session cookie and a per-request
    /// `cookies=`, and it survives a redirect hop. Held here rather than sniffed
    /// off the request, because by hop 2 the observer's own header would be
    /// indistinguishable from the caller's.
    caller_cookie: Option<String>,
}

impl HopObserver for JarObserver {
    fn before_send(&self, request: &mut WireRequest) {
        // A hand-written `Cookie:` wins outright — the jars are not consulted at
        // all, exactly as libcurl behaves when `CURLOPT_HTTPHEADER` carries one.
        if self.caller_cookie.is_some() {
            request.cookies = None;
            return;
        }
        let url = request.effective_url();
        // Otherwise the observer owns cookie rendering outright: every hop starts
        // from the jars, so nothing a previous hop sent can survive into this one.
        let mut merged = self.jar.matching(&url);
        // A per-request `cookies=` wins over the jar, key by key.
        for (name, value) in self.overlay.matching(&url) {
            merged.insert(name, value);
        }
        request.cookies = None;
        let existing: Vec<String> = request
            .headers
            .keys()
            .filter(|key| key.eq_ignore_ascii_case("cookie"))
            .cloned()
            .collect();
        for key in existing {
            request.headers.shift_remove(&key);
        }
        if merged.is_empty() {
            return;
        }
        let header = merged
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        request.headers.insert("Cookie".to_owned(), header);
    }

    fn after_receive(&self, request: &WireRequest, response: &WireResponse) {
        let url = Url::parse(&response.url).unwrap_or_else(|_| request.effective_url());
        self.jar.store_all(&url, &response.set_cookies);
    }
}

// ---------------------------------------------------------------------------
// The shared engine state
// ---------------------------------------------------------------------------

/// What a session or a one-shot client needs to perform a request.
struct EngineState {
    config: FetcherConfig,
    backend: Arc<dyn HttpBackend>,
    resolver: Arc<dyn HostResolver>,
    document_hook: Option<DocumentHook>,
    /// The session's jar. Always present so [`AsyncFetcherSession::cookies`]
    /// can hand one back without an `Option`; whether requests actually *use*
    /// it is `persist_cookies`.
    jar: Arc<CookieJar>,
    /// `true` for a session (one jar for its whole life), `false` for the
    /// one-shot fetchers, which get a fresh empty jar per request.
    persist_cookies: bool,
    alive: AtomicBool,
    auto_open: bool,
}

impl std::fmt::Debug for EngineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineState")
            .field("backend", &self.backend.name())
            .field("pooled_cookies", &self.persist_cookies)
            .field("alive", &self.alive.load(Ordering::Acquire))
            .finish()
    }
}

impl EngineState {
    async fn execute(
        &self,
        method: Method,
        url: &str,
        overrides: &RequestOverrides,
    ) -> Result<Response> {
        if !self.alive.load(Ordering::Acquire) {
            if !self.auto_open {
                return Err(FetchError::NoSession);
            }
            tracing::debug!("opening the session lazily on first use");
            self.alive.store(true, Ordering::Release);
        }

        // `_make_request` resolves these once, outside the attempt loop.
        let selector_config = resolve_selector_config(&self.config, overrides);
        let policy = RetryPolicy::resolve(&self.config, overrides);
        let parsed = Url::parse(url).map_err(|source| FetchError::InvalidUrl {
            url: url.to_owned(),
            source,
        })?;
        let jar = if self.persist_cookies {
            Arc::clone(&self.jar)
        } else {
            // Python opens a brand-new session per one-shot request, which is
            // what makes `Fetcher.get` cookie-isolated. A fresh jar reproduces
            // exactly that, without discarding the connection pool.
            Arc::new(CookieJar::new())
        };
        let overlay = Arc::new(CookieJar::new());
        if let Some(explicit) = overrides.cookies.as_ref().into_option() {
            for (name, value) in explicit {
                overlay.store(&parsed, &format!("{name}={value}; Path=/"));
            }
        }
        let factory = ResponseFactory::with_selector_config(selector_config)
            .with_document_hook(self.document_hook.clone());

        let mut attempt: u32 = 0;
        loop {
            // Rotates on every attempt, so a failing proxy is swapped out before
            // the retry. A per-request `proxy=` pins it and never touches the
            // rotator.
            let proxy = proxy_for_attempt(&self.config, overrides);
            let context = RequestContext::new().with_proxy(proxy.clone());
            let merged = merge_request_args(url, &self.config, overrides, &context);
            let redirect_policy = RedirectPolicy::new(merged.allow_redirects, merged.max_redirects);
            let observer = JarObserver {
                jar: Arc::clone(&jar),
                overlay: Arc::clone(&overlay),
                caller_cookie: crate::redirect_safe::header_value(&merged.headers, "cookie")
                    .map(str::to_owned),
            };
            let request = WireRequest::from_merged(method, parsed.clone(), &merged);

            // `timeout` is curl's `CURLOPT_TIMEOUT_MS`, which covers the **whole
            // transfer, redirects included** — not one hop. The backend also
            // applies it per hop, so a slow single response still trips; this
            // deadline is what stops a thirty-hop chain taking thirty timeouts.
            // `0` means "no timeout" to libcurl, and so it does here.
            let deadline = merged.timeout.filter(|budget| !budget.is_zero());
            let attempt_result = match deadline {
                Some(budget) => match tokio::time::timeout(
                    budget,
                    follow_redirects(
                        self.backend.as_ref(),
                        self.resolver.as_ref(),
                        &observer,
                        request,
                        redirect_policy,
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(FetchError::Transport(
                        crate::error::TransportError::timeout(format!(
                            "Operation timed out after {} milliseconds",
                            budget.as_millis()
                        )),
                    )),
                },
                None => {
                    follow_redirects(
                        self.backend.as_ref(),
                        self.resolver.as_ref(),
                        &observer,
                        request,
                        redirect_policy,
                    )
                    .await
                }
            };

            match attempt_result {
                Ok(outcome) => {
                    let mut meta = IndexMap::new();
                    meta.insert(META_PROXY_KEY.to_owned(), proxy_to_meta(proxy.as_ref()));
                    // `Response.cookies` is `dict(response.cookies)`, and
                    // `curl_cffi` builds *that* from the final reply's
                    // `Set-Cookie` lines alone — not from the jar. So a cookie
                    // set on a redirect hop does not appear, an expired or
                    // `Secure`-over-plain-HTTP cookie does, and no domain/path
                    // matching is applied. The jar is a separate thing and keeps
                    // its own, correct rules.
                    let cookies =
                        crate::response::cookies_from_set_cookie(&outcome.response.set_cookies);
                    return Ok(factory.from_http_request(
                        outcome.response,
                        outcome.method,
                        outcome.history,
                        cookies,
                        meta,
                    ));
                }
                // Only a transport failure retries — an HTTP 4xx/5xx never
                // becomes an error at all, and a refused redirect is terminal.
                Err(err) if err.is_retryable() && policy.should_retry(attempt) => {
                    log_retry(attempt, &err, proxy.as_ref(), policy.delay);
                    if !policy.delay.is_zero() {
                        tokio::time::sleep(policy.delay).await;
                    }
                }
                Err(err) => {
                    if err.is_retryable() {
                        log_failure(policy.attempts, &err);
                    }
                    return Err(err);
                }
            }
            attempt += 1;
        }
    }
}

/// `meta={"proxy": proxy}` — a URL proxy becomes a string, a dictionary proxy
/// keeps its shape, and no proxy is JSON `null` (Python's `None`).
fn proxy_to_meta(proxy: Option<&Proxy>) -> Value {
    proxy.map_or(Value::Null, |proxy| {
        serde_json::to_value(proxy).unwrap_or(Value::Null)
    })
}

// ---------------------------------------------------------------------------
// The default backend
// ---------------------------------------------------------------------------

#[cfg(feature = "impersonate")]
static DEFAULT_BACKEND: OnceLock<Arc<crate::backend::wreq::WreqBackend>> = OnceLock::new();

/// The process-wide default transport.
///
/// A single pooled client, shared by every session and one-shot fetcher that
/// does not name its own. Cookie isolation does **not** come from the client
/// (this crate manages cookies itself), so sharing it is safe.
///
/// # Errors
///
/// [`FetchError::NoBackend`] when the crate was built without the
/// `impersonate` feature.
#[allow(clippy::unnecessary_wraps)]
pub fn default_backend() -> Result<Arc<dyn HttpBackend>> {
    #[cfg(feature = "impersonate")]
    {
        Ok(Arc::clone(
            DEFAULT_BACKEND.get_or_init(|| Arc::new(crate::backend::wreq::WreqBackend::new())),
        ) as Arc<dyn HttpBackend>)
    }
    #[cfg(not(feature = "impersonate"))]
    {
        Err(FetchError::NoBackend)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Build an [`AsyncFetcherSession`] or an [`AsyncFetcherClient`].
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use browser_http::backend::mock::{MockBackend, MockReply};
/// use browser_http::config::{FetcherConfig, Impersonate};
/// use browser_http::session::AsyncFetcherSession;
///
/// let backend = Arc::new(MockBackend::always(MockReply::new(200).body("<p>hi</p>")));
/// let session = AsyncFetcherSession::builder()
///     .config(
///         FetcherConfig::default()
///             .with_impersonate(Impersonate::single("chrome"))
///             .with_retries(Some(3))
///             .with_retry_delay(Duration::from_millis(0)),
///     )
///     .backend(backend)
///     .build()?;
/// assert!(!session.is_alive(), "a built session is not open yet");
/// # Ok::<(), browser_http::error::FetchError>(())
/// ```
pub struct SessionBuilder {
    config: FetcherConfig,
    backend: Option<Arc<dyn HttpBackend>>,
    resolver: Option<Arc<dyn HostResolver>>,
    document_hook: Option<DocumentHook>,
    auto_open: bool,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            config: FetcherConfig::default(),
            backend: None,
            resolver: None,
            document_hook: None,
            auto_open: true,
        }
    }
}

impl std::fmt::Debug for SessionBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionBuilder")
            .field("config", &self.config)
            .field("backend", &self.backend.as_ref().map(|b| b.name()))
            .field("auto_open", &self.auto_open)
            .finish()
    }
}

impl SessionBuilder {
    /// Start from the Python defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole session configuration.
    #[must_use]
    pub fn config(mut self, config: FetcherConfig) -> Self {
        self.config = config;
        self
    }

    /// Mutate the configuration in place.
    #[must_use]
    pub fn with_config(mut self, f: impl FnOnce(FetcherConfig) -> FetcherConfig) -> Self {
        self.config = f(self.config);
        self
    }

    /// Use a specific transport instead of [`default_backend`].
    #[must_use]
    pub fn backend(mut self, backend: Arc<dyn HttpBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Use a specific host resolver for the `"safe"` redirect guard.
    #[must_use]
    pub fn resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Attach a [`DocumentHook`], e.g. to give every response adaptive storage.
    #[must_use]
    pub fn document_hook(mut self, hook: DocumentHook) -> Self {
        self.document_hook = Some(hook);
        self
    }

    /// Whether a request on a session that is not open should open it.
    ///
    /// `true` (the default) matches the spider's "opened by `start`, or lazily
    /// on first use". `false` reproduces Python exactly: a request without an
    /// active session raises `RuntimeError("No active session available.")`,
    /// which is [`FetchError::NoSession`].
    #[must_use]
    pub fn auto_open(mut self, auto_open: bool) -> Self {
        self.auto_open = auto_open;
        self
    }

    fn state(self, pooled_cookies: bool) -> Result<Arc<EngineState>> {
        let config = self.config.normalized();
        config.validate()?;
        let backend = match self.backend {
            Some(backend) => backend,
            None => default_backend()?,
        };
        let resolver = self
            .resolver
            .unwrap_or_else(|| Arc::new(SystemResolver) as Arc<dyn HostResolver>);
        Ok(Arc::new(EngineState {
            config,
            backend,
            resolver,
            document_hook: self.document_hook,
            jar: Arc::new(CookieJar::new()),
            persist_cookies: pooled_cookies,
            alive: AtomicBool::new(false),
            auto_open: self.auto_open,
        }))
    }

    /// Finish as a pooled session with a cookie jar.
    ///
    /// # Errors
    ///
    /// [`FetchError::Config`] for the proxy-rotator conflict, or
    /// [`FetchError::NoBackend`] when no transport is available.
    pub fn build(self) -> Result<AsyncFetcherSession> {
        Ok(AsyncFetcherSession(self.state(true)?))
    }

    /// Finish as a one-shot client: a fresh cookie jar per request.
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build`].
    pub fn build_client(self) -> Result<AsyncFetcherClient> {
        let state = self.state(false)?;
        // A one-shot client has no `__enter__`, so it is always usable.
        state.alive.store(true, Ordering::Release);
        Ok(AsyncFetcherClient(state))
    }
}

// ---------------------------------------------------------------------------
// AsyncFetcherSession
// ---------------------------------------------------------------------------

/// A pooled session: one connection pool, one cookie jar, one set of defaults.
///
/// Python's docs claim a session is "10 times faster than creating a single
/// session for each request".
///
/// Cloning is cheap and shares everything — a clone is the same session, which
/// is what lets a crawl hand it to many tasks.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use browser_http::backend::mock::{MockBackend, MockReply};
/// use browser_http::session::AsyncFetcherSession;
///
/// # let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
/// # runtime.block_on(async {
/// let backend = Arc::new(MockBackend::always(
///     MockReply::new(200)
///         .header("Content-Type", "text/html; charset=utf-8")
///         .body("<h1>hello</h1>"),
/// ));
/// let session = AsyncFetcherSession::builder().backend(backend).build()?;
/// session.open().await?;
///
/// let page = session.get("https://example.test/").send().await?;
/// assert_eq!(page.status, 200);
/// assert_eq!(page.css("h1::text")?.texts(), ["hello"]);
///
/// session.close().await?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct AsyncFetcherSession(Arc<EngineState>);

impl AsyncFetcherSession {
    /// Start building one.
    #[must_use]
    pub fn builder() -> SessionBuilder {
        SessionBuilder::new()
    }

    /// A session with Python's defaults over [`default_backend`].
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build`].
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    /// The session defaults.
    #[must_use]
    pub fn config(&self) -> &FetcherConfig {
        &self.0.config
    }

    /// The cookie jar, shared by every request of this session.
    #[must_use]
    pub fn cookies(&self) -> &Arc<CookieJar> {
        &self.0.jar
    }

    /// Whether the session is open — Python's `_is_alive`.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::Acquire)
    }

    /// Open the session — `__aenter__`.
    ///
    /// # Errors
    ///
    /// [`FetchError::SessionAlreadyActive`] when it is already open, with
    /// Python's message.
    pub async fn open(&self) -> Result<()> {
        if self
            .0
            .alive
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FetchError::SessionAlreadyActive(
                "This FetcherSession instance already has an active asynchronous session.",
            ));
        }
        Ok(())
    }

    /// Close the session — `__aexit__`. The cookie jar is emptied and pooled
    /// connections are released.
    ///
    /// # Errors
    ///
    /// [`FetchError::InvalidSessionExit`] when it was not open, with Python's
    /// `"Cannot exit invalid session"`.
    pub async fn close(&self) -> Result<()> {
        if self
            .0
            .alive
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FetchError::InvalidSessionExit);
        }
        self.0.jar.clear();
        self.0.backend.close().await;
        Ok(())
    }

    /// `session.get(url, **kwargs)`.
    #[must_use]
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Get, url)
    }

    /// `session.post(url, **kwargs)`.
    #[must_use]
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Post, url)
    }

    /// `session.put(url, **kwargs)`.
    #[must_use]
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Put, url)
    }

    /// `session.delete(url, **kwargs)`.
    #[must_use]
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Delete, url)
    }

    /// Any of the four methods.
    #[must_use]
    pub fn request(&self, method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder::new(Arc::clone(&self.0), method, url.into())
    }

    /// Send an already-assembled set of overrides.
    ///
    /// # Errors
    ///
    /// As [`RequestBuilder::send`].
    pub async fn send(
        &self,
        method: Method,
        url: &str,
        overrides: &RequestOverrides,
    ) -> Result<Response> {
        self.0.execute(method, url, overrides).await
    }
}

// ---------------------------------------------------------------------------
// AsyncFetcherClient / AsyncFetcher
// ---------------------------------------------------------------------------

/// The one-shot client behind [`AsyncFetcher`] — Python's `AsyncFetcherClient`.
///
/// Every request gets a fresh, empty [`CookieJar`], so nothing persists between
/// calls; the connection pool *is* shared, which Python could not do.
#[derive(Clone, Debug)]
pub struct AsyncFetcherClient(Arc<EngineState>);

impl AsyncFetcherClient {
    /// Start building one.
    #[must_use]
    pub fn builder() -> SessionBuilder {
        SessionBuilder::new()
    }

    /// A client with Python's defaults over [`default_backend`].
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build_client`].
    pub fn new() -> Result<Self> {
        Self::builder().build_client()
    }

    /// The client defaults.
    #[must_use]
    pub fn config(&self) -> &FetcherConfig {
        &self.0.config
    }

    /// `Fetcher.get(url, **kwargs)`.
    #[must_use]
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Get, url)
    }

    /// `Fetcher.post(url, **kwargs)`.
    #[must_use]
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Post, url)
    }

    /// `Fetcher.put(url, **kwargs)`.
    #[must_use]
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Put, url)
    }

    /// `Fetcher.delete(url, **kwargs)`.
    #[must_use]
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Delete, url)
    }

    /// Any of the four methods.
    #[must_use]
    pub fn request(&self, method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder::new(Arc::clone(&self.0), method, url.into())
    }
}

static GLOBAL_CLIENT: OnceLock<AsyncFetcherClient> = OnceLock::new();

/// `AsyncFetcher` — the module-level singleton Python exposes as classmethods.
///
/// Constructed with **all** defaults: `impersonate="chrome"`,
/// `stealthy_headers=True`, `timeout=30`, `retries=3`, `retry_delay=1`,
/// `follow_redirects="safe"`, `max_redirects=30`, `verify=True`.
///
/// # Examples
///
/// ```no_run
/// use browser_http::session::AsyncFetcher;
///
/// # async fn demo() -> Result<(), browser_http::error::FetchError> {
/// let page = AsyncFetcher::get("https://example.com")
///     .timeout(None)                  // an explicit "no timeout"
///     .header("x-api-key", "…")
///     .retries(Some(5))
///     .send()
///     .await?;
/// let titles = page.css("h3 a::text")?.texts();
/// # let _ = titles;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct AsyncFetcher;

impl AsyncFetcher {
    /// The process-wide one-shot client.
    ///
    /// # Errors
    ///
    /// [`FetchError::NoBackend`] when there is no transport to use.
    pub fn client() -> Result<&'static AsyncFetcherClient> {
        if let Some(client) = GLOBAL_CLIENT.get() {
            return Ok(client);
        }
        let client = AsyncFetcherClient::new()?;
        Ok(GLOBAL_CLIENT.get_or_init(|| client))
    }

    /// `AsyncFetcher.get(url, **kwargs)`.
    #[must_use]
    pub fn get(url: impl Into<String>) -> RequestBuilder {
        Self::request(Method::Get, url)
    }

    /// `AsyncFetcher.post(url, **kwargs)`.
    #[must_use]
    pub fn post(url: impl Into<String>) -> RequestBuilder {
        Self::request(Method::Post, url)
    }

    /// `AsyncFetcher.put(url, **kwargs)`.
    #[must_use]
    pub fn put(url: impl Into<String>) -> RequestBuilder {
        Self::request(Method::Put, url)
    }

    /// `AsyncFetcher.delete(url, **kwargs)`.
    #[must_use]
    pub fn delete(url: impl Into<String>) -> RequestBuilder {
        Self::request(Method::Delete, url)
    }

    /// Any of the four methods.
    ///
    /// The backend is resolved lazily, at [`RequestBuilder::send`], so that a
    /// build without the `impersonate` feature fails with
    /// [`FetchError::NoBackend`] at send time rather than panicking here.
    #[must_use]
    pub fn request(method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder::deferred(method, url.into())
    }
}

// ---------------------------------------------------------------------------
// RequestBuilder
// ---------------------------------------------------------------------------

/// The fluent per-request API — Python's `**kwargs`, with presence preserved.
///
/// Calling a setter **at all** is the "the key is present" signal, so
/// `.timeout(None)` really does mean "no timeout" and overrides a session's 30 s.
/// That is `_get_param` reproduced in the type system (`ARCHITECTURE.md` §5.4).
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use browser_core::types::Override;
/// use browser_http::session::AsyncFetcher;
///
/// let builder = AsyncFetcher::get("https://example.com").timeout(None);
/// assert_eq!(builder.overrides().timeout, Override::Set(None));
///
/// // Not calling it leaves the session default in charge.
/// assert!(AsyncFetcher::get("https://example.com").overrides().timeout.is_unset());
/// ```
pub struct RequestBuilder {
    state: Option<Arc<EngineState>>,
    method: Method,
    url: String,
    overrides: RequestOverrides,
}

impl std::fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestBuilder")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("overrides", &self.overrides)
            .finish()
    }
}

impl RequestBuilder {
    fn new(state: Arc<EngineState>, method: Method, url: String) -> Self {
        Self {
            state: Some(state),
            method,
            url,
            overrides: RequestOverrides::new(),
        }
    }

    fn deferred(method: Method, url: String) -> Self {
        Self {
            state: None,
            method,
            url,
            overrides: RequestOverrides::new(),
        }
    }

    /// The overrides collected so far.
    #[must_use]
    pub fn overrides(&self) -> &RequestOverrides {
        &self.overrides
    }

    /// Replace the overrides wholesale — the escape hatch for callers that
    /// already hold a [`RequestOverrides`], such as the spider's session
    /// adapter.
    #[must_use]
    pub fn with_overrides(mut self, overrides: RequestOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    /// The method this request will use.
    #[must_use]
    pub const fn method(&self) -> Method {
        self.method
    }

    /// The URL this request will fetch.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Perform the request.
    ///
    /// # Errors
    ///
    /// [`FetchError::Transport`] when every attempt failed,
    /// [`FetchError::UnsafeRedirect`] / [`FetchError::TooManyRedirects`] from
    /// the redirect guard, [`FetchError::InvalidUrl`] for an unparsable URL,
    /// [`FetchError::NoSession`] on a closed strict session, and
    /// [`FetchError::NoBackend`] when there is no transport.
    ///
    /// An HTTP 4xx or 5xx is **not** an error: it comes back as a [`Response`]
    /// carrying that status, exactly as in Python.
    pub async fn send(self) -> Result<Response> {
        let state = match self.state {
            Some(state) => state,
            None => Arc::clone(&AsyncFetcher::client()?.0),
        };
        state.execute(self.method, &self.url, &self.overrides).await
    }
}

/// Generate the delegating setters, so every knob of [`RequestOverrides`] is
/// reachable from the fluent API without 300 lines of copy-paste.
macro_rules! request_setters {
    ($( $name:ident ( $($arg:ident : $ty:ty),* ) => $doc:literal );+ $(;)?) => {
        impl RequestBuilder {
            $(
                #[doc = $doc]
                #[must_use]
                pub fn $name(mut self, $($arg: $ty),*) -> Self {
                    self.overrides = self.overrides.$name($($arg),*);
                    self
                }
            )+
        }
    };
}

request_setters! {
    impersonate(value: Impersonate) => "`impersonate=` — a profile or a pool sampled per request.";
    http3(value: bool) => "`http3=` — force HTTP/3 only.";
    stealthy_headers(value: bool) => "`stealthy_headers=` — the kwarg the fetcher methods pop as `stealth`.";
    proxies(value: ProxySpec) => "`proxies=` — the per-scheme proxy table.";
    proxy(value: Option<Proxy>) => "`proxy=` — pins the proxy and disables the rotator for this request.";
    proxy_url(value: String) => "`proxy=\"http://…\"` as a URL string.";
    proxy_auth(value: Option<(String, String)>) => "`proxy_auth=` — basic auth for the proxy.";
    timeout(value: Option<Duration>) => "`timeout=`; `None` is an explicit \"no timeout\".";
    headers(value: HeaderDict) => "`headers=` — replaces the per-request header map.";
    retries(value: Option<i32>) => "`retries=` — total attempts; `0`, `None` and negatives all mean one.";
    retry_delay(value: Duration) => "`retry_delay=` — a fixed pause, no backoff and no jitter.";
    max_redirects(value: i32) => "`max_redirects=`; `-1` is unlimited.";
    verify(value: bool) => "`verify=` — TLS peer verification.";
    cert(value: Option<ClientCert>) => "`cert=` — a client certificate.";
    selector_config(value: SelectorConfig) => "`selector_config=`; an empty map falls back to the session's.";
    http_version(value: HttpVersion) => "`http_version=` — overwritten when `http3=true`.";
    params(value: Query) => "`params=` — query-string pairs appended to the URL.";
    cookies(value: Cookies) => "`cookies=` — cookies for this request, winning over the jar.";
    body(value: Body) => "`data=` / `json=` — the request body.";
}

impl RequestBuilder {
    /// Add one header, keeping the ones already set.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.overrides = self.overrides.header(name, value);
        self
    }

    /// `follow_redirects=` — takes the `bool` or the four named modes.
    #[must_use]
    pub fn follow_redirects(mut self, value: browser_core::types::FollowRedirects) -> Self {
        self.overrides = self.overrides.follow_redirects(value);
        self
    }

    /// `auth=(username, password)` — HTTP basic auth.
    #[must_use]
    pub fn auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.overrides = self.overrides.auth(username, password);
        self
    }

    /// `json=` — a JSON body.
    #[must_use]
    pub fn json(mut self, value: Value) -> Self {
        self.overrides = self.overrides.json(value);
        self
    }

    /// `data={…}` — a url-encoded form body, order preserved.
    #[must_use]
    pub fn form<I, K, V>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.overrides = self.overrides.form(pairs);
        self
    }

    /// One pass-through option, forwarded verbatim to the backend.
    ///
    /// `Value::Null` is dropped and anything in
    /// [`crate::merge::SKIP_KEYS`] is ignored, exactly as in Python.
    #[must_use]
    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.overrides = self.overrides.extra(key, value);
        self
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use browser_core::types::{FollowRedirects, RedirectMode};

    use super::*;
    use crate::backend::mock::{MockBackend, MockReply};
    use crate::config::Impersonate;
    use crate::error::TransportError;
    use crate::proxy::ProxyRotator;

    /// Everything resolves to one public address, so `"safe"` never fires
    /// unless a test wants it to.
    struct PublicResolver;

    #[async_trait]
    impl HostResolver for PublicResolver {
        async fn resolve(&self, _host: &str) -> std::result::Result<Vec<IpAddr>, TransportError> {
            Ok(vec!["93.184.216.34".parse().expect("ip")])
        }
    }

    struct PrivateResolver;

    #[async_trait]
    impl HostResolver for PrivateResolver {
        async fn resolve(&self, _host: &str) -> std::result::Result<Vec<IpAddr>, TransportError> {
            Ok(vec!["10.0.0.1".parse().expect("ip")])
        }
    }

    fn session_with(backend: Arc<dyn HttpBackend>, config: FetcherConfig) -> AsyncFetcherSession {
        AsyncFetcherSession::builder()
            .config(config)
            .backend(backend)
            .resolver(Arc::new(PublicResolver))
            .build()
            .expect("builds")
    }

    fn html_reply(body: &'static str) -> MockReply {
        MockReply::new(200)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(body)
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("url")
    }

    // --- the cookie jar --------------------------------------------------

    #[test]
    fn the_jar_matches_on_domain_path_and_secure() {
        let jar = CookieJar::new();
        let root = url("https://shop.test/");
        jar.store(&root, "sid=abc; Path=/");
        jar.store(&root, "deep=1; Path=/admin");
        jar.store(&root, "tls=1; Secure");
        jar.store(&root, "wide=1; Domain=shop.test");

        assert_eq!(
            jar.header_for(&root).as_deref(),
            Some("sid=abc; tls=1; wide=1")
        );
        assert!(jar
            .header_for(&url("https://shop.test/admin/x"))
            .expect("cookies")
            .contains("deep=1"));
        // `Path=/admin` must not match `/administrator`.
        assert!(!jar
            .header_for(&url("https://shop.test/administrator"))
            .expect("cookies")
            .contains("deep=1"));
        // A `Secure` cookie never goes out over plain HTTP.
        let insecure = jar.header_for(&url("http://shop.test/")).expect("cookies");
        assert!(!insecure.contains("tls=1"));
        // A `Domain=` cookie reaches subdomains; a host-only one does not.
        let sub = jar
            .header_for(&url("https://api.shop.test/"))
            .expect("cookies");
        assert!(sub.contains("wide=1"));
        assert!(!sub.contains("sid=abc"));
        // …and never a sibling domain that merely ends the same way.
        assert!(jar.header_for(&url("https://evilshop.test/")).is_none());
    }

    #[test]
    fn the_jar_replaces_and_expires() {
        let jar = CookieJar::new();
        let root = url("https://shop.test/");
        jar.store(&root, "sid=one");
        jar.store(&root, "sid=two");
        assert_eq!(jar.matching(&root)["sid"], "two");
        assert_eq!(jar.len(), 1);

        jar.store(&root, "sid=three; Max-Age=0");
        assert!(jar.matching(&root).is_empty());
        assert!(jar.is_empty());

        jar.store(&root, "junk");
        jar.store(&root, "keep=1");
        assert_eq!(jar.matching(&root).len(), 1);
        jar.clear();
        assert!(jar.is_empty());
    }

    #[test]
    fn the_default_path_is_the_requests_directory() {
        assert_eq!(default_path(&url("https://a.test/")), "/");
        assert_eq!(default_path(&url("https://a.test/x")), "/");
        assert_eq!(default_path(&url("https://a.test/a/b")), "/a");
        assert_eq!(default_path(&url("https://a.test/a/b/")), "/a/b");
    }

    // --- the driver ------------------------------------------------------

    #[tokio::test]
    async fn a_request_becomes_a_parsed_response() {
        let backend = Arc::new(MockBackend::new([(
            "https://shop.test/",
            html_reply("<h1>hello</h1><a href='/p/1'>one</a>"),
        )]));
        let session = session_with(backend, FetcherConfig::default());
        session.open().await.expect("open");

        let page = session
            .get("https://shop.test/")
            .send()
            .await
            .expect("send");
        assert_eq!(page.status, 200);
        assert_eq!(page.reason, "OK");
        assert_eq!(page.method, Method::Get);
        assert_eq!(page.encoding, "utf-8");
        assert_eq!(page.css("h1::text").expect("css").texts(), ["hello"]);
        assert_eq!(page.urljoin("/p/2"), "https://shop.test/p/2");
        assert!(page.history.is_empty());
        assert_eq!(page.proxy(), None);
        session.close().await.expect("close");
    }

    #[tokio::test]
    async fn a_four_hundred_is_a_response_not_an_error_and_never_retries() {
        let backend = Arc::new(MockBackend::always(MockReply::new(503).body("nope")));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default()
                .with_retries(Some(3))
                .with_retry_delay(Duration::ZERO),
        );
        let page = session.get("https://a.test/").send().await.expect("send");
        assert_eq!(page.status, 503);
        assert!(!page.ok());
        assert_eq!(backend.call_count(), 1, "a 5xx is a success, not a retry");
    }

    #[tokio::test]
    async fn only_transport_failures_retry_and_the_delay_is_fixed() {
        let backend = Arc::new(MockBackend::sequence([
            Err(TransportError::connect("connection reset")),
            Err(TransportError::connect("connection reset")),
            Ok(html_reply("<p>third time</p>")),
        ]));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default()
                .with_retries(Some(3))
                .with_retry_delay(Duration::ZERO),
        );
        let page = session.get("https://a.test/").send().await.expect("send");
        assert_eq!(page.css("p::text").expect("css").texts(), ["third time"]);
        assert_eq!(backend.call_count(), 3);
    }

    #[tokio::test]
    async fn retries_is_an_attempt_count_not_extra_attempts() {
        for (retries, expected) in [
            (Some(3), 3),
            (Some(1), 1),
            (Some(0), 1),
            (Some(-1), 1),
            (None, 1),
        ] {
            let backend = Arc::new(MockBackend::always_failing(TransportError::connect("boom")));
            let session = session_with(
                Arc::clone(&backend) as Arc<dyn HttpBackend>,
                FetcherConfig::default()
                    .with_retries(retries)
                    .with_retry_delay(Duration::ZERO),
            );
            let err = session
                .get("https://a.test/")
                .send()
                .await
                .expect_err("always fails");
            assert!(matches!(err, FetchError::Transport(_)));
            assert_eq!(backend.call_count(), expected, "retries={retries:?}");
        }
    }

    #[tokio::test]
    async fn a_per_request_retries_zero_overrides_a_session_default_of_three() {
        let backend = Arc::new(MockBackend::always_failing(TransportError::connect("boom")));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default()
                .with_retries(Some(3))
                .with_retry_delay(Duration::ZERO),
        );
        session
            .get("https://a.test/")
            .retries(Some(0))
            .send()
            .await
            .expect_err("fails");
        assert_eq!(backend.call_count(), 1);
    }

    #[tokio::test]
    async fn the_rotator_advances_on_every_attempt() {
        let backend = Arc::new(MockBackend::always_failing(TransportError::connect(
            "Failed to connect to proxy",
        )));
        let rotator = Arc::new(
            ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080"]).expect("proxies"),
        );
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default()
                .with_retries(Some(2))
                .with_retry_delay(Duration::ZERO)
                .with_proxy_rotator(Some(rotator)),
        );
        session
            .get("https://a.test/")
            .send()
            .await
            .expect_err("fails");

        let used: Vec<Option<String>> = backend
            .sent()
            .iter()
            .map(|request| request.proxy.as_ref().map(crate::proxy::proxy_url))
            .collect();
        assert_eq!(
            used,
            [
                Some("http://p1:8080".to_owned()),
                Some("http://p2:8080".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn a_per_request_proxy_pins_every_attempt_and_lands_in_meta() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default().with_retry_delay(Duration::ZERO),
        );
        let page = session
            .get("https://a.test/")
            .proxy_url("http://pinned:1".to_owned())
            .send()
            .await
            .expect("send");
        assert_eq!(
            page.proxy().and_then(Value::as_str),
            Some("http://pinned:1")
        );
        assert_eq!(
            backend.sent()[0]
                .proxy
                .as_ref()
                .map(crate::proxy::proxy_url),
            Some("http://pinned:1".to_owned())
        );
    }

    #[tokio::test]
    async fn a_session_proxy_reaches_the_wire() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default().with_proxy_url("http://session:9"),
        );
        session.get("https://a.test/").send().await.expect("send");
        assert_eq!(
            backend.sent()[0]
                .proxy
                .as_ref()
                .map(crate::proxy::proxy_url),
            Some("http://session:9".to_owned())
        );
    }

    #[tokio::test]
    async fn presence_beats_none_for_timeout() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );

        session.get("https://a.test/").send().await.expect("send");
        assert_eq!(backend.sent()[0].timeout, Some(Duration::from_secs(30)));

        session
            .get("https://a.test/")
            .timeout(None)
            .send()
            .await
            .expect("send");
        assert_eq!(backend.sent()[1].timeout, None, "an explicit None wins");
    }

    #[tokio::test]
    async fn the_impersonation_profile_is_sampled_per_request() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default().with_impersonate(Impersonate::pool(["chrome", "firefox"])),
        );
        for _ in 0..8 {
            session.get("https://a.test/").send().await.expect("send");
        }
        for request in backend.sent() {
            let profile = request.impersonate.expect("a profile per request");
            assert!(
                ["chrome", "firefox"].contains(&profile.as_str()),
                "{profile}"
            );
        }
    }

    #[tokio::test]
    async fn the_referer_and_the_users_headers_reach_the_wire() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );
        let page = session
            .get("https://a.test/")
            .header("X-Test", "1")
            .send()
            .await
            .expect("send");
        let sent = &backend.sent()[0].headers;
        assert_eq!(
            crate::redirect_safe::header_value(sent, "referer"),
            Some(crate::headers_job::GOOGLE_REFERER)
        );
        assert_eq!(
            crate::redirect_safe::header_value(sent, "x-test"),
            Some("1")
        );
        // …and the response reports what was actually sent.
        assert_eq!(
            crate::redirect_safe::header_value(&page.request_headers, "x-test"),
            Some("1")
        );
    }

    /// A backend that takes its time, so the whole-transfer deadline can be
    /// exercised without a network.
    struct SlowBackend(Duration);

    #[async_trait]
    impl HttpBackend for SlowBackend {
        async fn execute(
            &self,
            request: WireRequest,
        ) -> std::result::Result<WireResponse, TransportError> {
            tokio::time::sleep(self.0).await;
            Ok(WireResponse::new(
                200,
                request.effective_url().to_string(),
                bytes::Bytes::from_static(b"<p>slow</p>"),
            ))
        }
    }

    #[tokio::test]
    async fn the_timeout_covers_the_whole_transfer_and_zero_means_none() {
        let backend: Arc<dyn HttpBackend> = Arc::new(SlowBackend(Duration::from_millis(200)));
        let session = session_with(
            Arc::clone(&backend),
            FetcherConfig::default()
                .with_retries(Some(1))
                .with_retry_delay(Duration::ZERO),
        );

        let err = session
            .get("https://a.test/")
            .timeout(Some(Duration::from_millis(20)))
            .send()
            .await
            .expect_err("too slow");
        match err {
            FetchError::Transport(TransportError::Timeout(message)) => {
                assert!(message.contains("timed out"), "{message}");
            }
            other => panic!("wrong error: {other}"),
        }

        // `timeout=None` really is "no timeout", not "the default".
        let page = session
            .get("https://a.test/")
            .timeout(None)
            .send()
            .await
            .expect("no deadline");
        assert_eq!(page.status, 200);

        // libcurl reads `0` as "no timeout" too, so a zero must not abort.
        let page = session
            .get("https://a.test/")
            .timeout(Some(Duration::ZERO))
            .send()
            .await
            .expect("zero means none");
        assert_eq!(page.css("p::text").expect("css").texts(), ["slow"]);
    }

    #[tokio::test]
    async fn a_timeout_is_a_transport_error_and_is_retried() {
        let backend: Arc<dyn HttpBackend> = Arc::new(SlowBackend(Duration::from_millis(200)));
        let session = session_with(
            Arc::clone(&backend),
            FetcherConfig::default()
                .with_retries(Some(3))
                .with_retry_delay(Duration::ZERO)
                .with_timeout(Some(Duration::from_millis(10))),
        );
        let err = session
            .get("https://a.test/")
            .send()
            .await
            .expect_err("always too slow");
        assert!(err.is_retryable());
        assert!(matches!(
            err,
            FetchError::Transport(TransportError::Timeout(_))
        ));
    }

    // --- redirects through the driver ------------------------------------

    #[tokio::test]
    async fn the_history_reaches_the_response() {
        let backend = Arc::new(MockBackend::new([
            (
                "https://a.test/one",
                MockReply::new(301).header("Location", "/two"),
            ),
            ("https://a.test/two", html_reply("<p>done</p>")),
        ]));
        let session = session_with(backend, FetcherConfig::default());
        let page = session
            .get("https://a.test/one")
            .send()
            .await
            .expect("send");
        assert_eq!(page.url, "https://a.test/two");
        assert_eq!(page.history.len(), 1);
        assert_eq!(page.history[0].status, 301);
        assert_eq!(page.css("p::text").expect("css").texts(), ["done"]);
    }

    #[tokio::test]
    async fn the_safe_guard_is_on_by_default_and_is_not_retried() {
        let backend = Arc::new(MockBackend::new([
            (
                "https://a.test/",
                MockReply::new(302).header("Location", "http://169.254.169.254/latest/"),
            ),
            ("http://169.254.169.254/latest/", MockReply::new(200)),
        ]));
        let session = AsyncFetcherSession::builder()
            .config(
                FetcherConfig::default()
                    .with_retries(Some(3))
                    .with_retry_delay(Duration::ZERO),
            )
            .backend(Arc::clone(&backend) as Arc<dyn HttpBackend>)
            .resolver(Arc::new(PrivateResolver))
            .build()
            .expect("builds");

        let err = session
            .get("https://a.test/")
            .send()
            .await
            .expect_err("blocked");
        assert!(matches!(err, FetchError::UnsafeRedirect { .. }));
        // A guard refusal is terminal — it is not a `CurlError`.
        assert_eq!(backend.call_count(), 1);
    }

    #[tokio::test]
    async fn max_redirects_minus_one_is_unlimited() {
        // Fifty hops would blow a budget of 30; `-1` must sail through.
        let mut table: Vec<(String, MockReply)> = (0..50)
            .map(|i| {
                (
                    format!("https://a.test/{i}"),
                    MockReply::new(302).header("Location", format!("/{}", i + 1)),
                )
            })
            .collect();
        table.push(("https://a.test/50".to_owned(), html_reply("<p>end</p>")));
        let backend = Arc::new(MockBackend::new(table));
        let session = session_with(backend, FetcherConfig::default());

        let err = session
            .get("https://a.test/0")
            .send()
            .await
            .expect_err("budget");
        assert!(matches!(err, FetchError::TooManyRedirects { max: 30, .. }));

        let page = session
            .get("https://a.test/0")
            .max_redirects(-1)
            .send()
            .await
            .expect("unlimited");
        assert_eq!(page.history.len(), 50);
        assert_eq!(page.css("p::text").expect("css").texts(), ["end"]);
    }

    // --- cookies through the driver --------------------------------------

    #[tokio::test]
    async fn a_session_persists_cookies_and_a_one_shot_does_not() {
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200).header("Set-Cookie", "sid=abc; Path=/"),
        ));

        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );
        let first = session.get("https://a.test/").send().await.expect("send");
        assert_eq!(first.cookies["sid"], "abc");
        session.get("https://a.test/x").send().await.expect("send");
        assert_eq!(
            crate::redirect_safe::header_value(&backend.sent()[1].headers, "cookie"),
            Some("sid=abc"),
            "the session replays its jar"
        );

        let client = AsyncFetcherSession::builder()
            .backend(Arc::clone(&backend) as Arc<dyn HttpBackend>)
            .resolver(Arc::new(PublicResolver))
            .build_client()
            .expect("client");
        client.get("https://a.test/").send().await.expect("send");
        client.get("https://a.test/x").send().await.expect("send");
        assert_eq!(
            crate::redirect_safe::header_value(&backend.sent()[3].headers, "cookie"),
            None,
            "a one-shot fetcher starts from an empty jar every time"
        );
    }

    #[tokio::test]
    async fn a_per_request_cookie_wins_over_the_jar() {
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200).header("Set-Cookie", "sid=jar"),
        ));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );
        session.get("https://a.test/").send().await.expect("send");

        let mut explicit = Cookies::new();
        explicit.insert("sid".to_owned(), "explicit".to_owned());
        session
            .get("https://a.test/")
            .cookies(explicit)
            .send()
            .await
            .expect("send");
        assert_eq!(
            crate::redirect_safe::header_value(&backend.sent()[1].headers, "cookie"),
            Some("sid=explicit")
        );
    }

    #[tokio::test]
    async fn cookies_do_not_cross_an_origin_boundary() {
        let backend = Arc::new(MockBackend::new([
            (
                "https://a.test/",
                MockReply::new(302)
                    .header("Set-Cookie", "sid=secret")
                    .header("Location", "https://b.test/"),
            ),
            ("https://b.test/", MockReply::new(200)),
        ]));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default()
                .with_follow_redirects(FollowRedirects::Mode(RedirectMode::All)),
        );
        session.get("https://a.test/").send().await.expect("send");
        assert_eq!(
            crate::redirect_safe::header_value(&backend.sent()[1].headers, "cookie"),
            None
        );
    }

    #[tokio::test]
    async fn per_request_cookies_survive_a_same_origin_redirect() {
        let backend = Arc::new(MockBackend::new([
            (
                "https://a.test/one",
                MockReply::new(302).header("Location", "/two"),
            ),
            ("https://a.test/two", MockReply::new(200)),
        ]));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );

        let mut explicit = Cookies::new();
        explicit.insert("sid".to_owned(), "explicit".to_owned());
        session
            .get("https://a.test/one")
            .cookies(explicit)
            .send()
            .await
            .expect("send");

        let sent = backend.sent();
        assert_eq!(
            crate::redirect_safe::header_value(&sent[0].headers, "cookie"),
            Some("sid=explicit")
        );
        assert_eq!(
            crate::redirect_safe::header_value(&sent[1].headers, "cookie"),
            Some("sid=explicit"),
            "a same-origin hop keeps the caller's cookies"
        );
    }

    #[tokio::test]
    async fn follow_redirects_false_returns_the_three_hundred() {
        let backend = Arc::new(MockBackend::new([(
            "https://a.test/",
            MockReply::new(302).header("Location", "https://b.test/"),
        )]));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );
        let page = session
            .get("https://a.test/")
            .follow_redirects(FollowRedirects::Bool(false))
            .send()
            .await
            .expect("send");
        assert_eq!(page.status, 302);
        assert!(page.is_redirect());
        assert_eq!(page.header("location"), Some("https://b.test/"));
        assert!(page.history.is_empty());
        assert_eq!(backend.call_count(), 1);
    }

    #[tokio::test]
    async fn stealth_off_and_impersonation_off_falls_back_to_one_static_user_agent() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default()
                .with_impersonate(Impersonate::None)
                .with_stealthy_headers(false),
        );
        session.get("https://a.test/").send().await.expect("send");
        let sent = &backend.sent()[0].headers;
        assert_eq!(
            crate::redirect_safe::header_value(sent, "user-agent"),
            Some(browser_fingerprint::useragent::default_ua())
        );
        // No stealth means no Google referer either.
        assert_eq!(crate::redirect_safe::header_value(sent, "referer"), None);
    }

    #[tokio::test]
    async fn a_per_request_selector_config_beats_the_sessions() {
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200).body("<p class='price'>9</p>"),
        ));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default().with_selector_config(
                SelectorConfig::default().with_adaptive_domain("session.test"),
            ),
        );

        let default = session.get("https://a.test/").send().await.expect("send");
        assert_eq!(default.selector_url(), "session.test");

        let overridden = session
            .get("https://a.test/")
            .selector_config(SelectorConfig::default().with_adaptive_domain("from-request.test"))
            .send()
            .await
            .expect("send");
        assert_eq!(overridden.selector_url(), "from-request.test");
        // The fetched URL is untouched either way, and so is the parse base.
        assert_eq!(overridden.url, "https://a.test/");
        assert_eq!(overridden.urljoin("/x"), "https://a.test/x");

        // An *empty* per-request map falls back to the session's, as in Python.
        let empty = session
            .get("https://a.test/")
            .selector_config(SelectorConfig::new())
            .send()
            .await
            .expect("send");
        assert_eq!(empty.selector_url(), "session.test");
    }

    #[tokio::test]
    async fn every_field_unit_15_narrows_into_a_fetched_response_is_reachable() {
        // `browser-spider` depends on this crate, not the other way round, so
        // `impl From<Response> for FetchedResponse` has to live there. This test
        // pins that every field it reads is public here.
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .body("<p class='price'>$9.99</p>"),
        ));
        let session = session_with(backend, FetcherConfig::default());
        let page = session
            .get("https://shop.test/catalogue")
            .send()
            .await
            .expect("send");

        let _status: u16 = page.status;
        let _reason: &str = &page.reason;
        let _url: &str = &page.url;
        let _body: &bytes::Bytes = page.body();
        let _encoding: &str = &page.encoding;
        let _headers: &HeaderDict = &page.headers;
        let _request_headers: &HeaderDict = &page.request_headers;
        let _cookies: &IndexMap<String, String> = &page.cookies;
        let history: Vec<String> = page.history.iter().map(|hop| hop.url.clone()).collect();
        let _meta: &IndexMap<String, Value> = &page.meta;
        assert!(history.is_empty());
        assert_eq!(page.to_string(), "<200 https://shop.test/catalogue>");
        assert_eq!(page.css("p::text").expect("css").texts(), ["$9.99"]);
    }

    // --- lifecycle -------------------------------------------------------

    #[tokio::test]
    async fn the_session_lifecycle_matches_pythons_messages() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(backend, FetcherConfig::default());

        assert!(!session.is_alive());
        session.open().await.expect("open");
        assert!(session.is_alive());

        let err = session.open().await.expect_err("double enter");
        assert_eq!(
            err.to_string(),
            "This FetcherSession instance already has an active asynchronous session."
        );

        session.close().await.expect("close");
        assert!(!session.is_alive());
        assert_eq!(
            session.close().await.expect_err("double exit").to_string(),
            "Cannot exit invalid session"
        );
        // …and the same object may be reused sequentially.
        session.open().await.expect("re-enter");
    }

    #[tokio::test]
    async fn a_strict_session_refuses_requests_until_it_is_opened() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = AsyncFetcherSession::builder()
            .backend(Arc::clone(&backend) as Arc<dyn HttpBackend>)
            .resolver(Arc::new(PublicResolver))
            .auto_open(false)
            .build()
            .expect("builds");

        let err = session
            .get("https://a.test/")
            .send()
            .await
            .expect_err("closed");
        assert_eq!(err.to_string(), "No active session available.");
        assert_eq!(backend.call_count(), 0);

        session.open().await.expect("open");
        session.get("https://a.test/").send().await.expect("send");
        session.close().await.expect("close");
        assert!(session.get("https://a.test/").send().await.is_err());
    }

    #[tokio::test]
    async fn closing_empties_the_jar() {
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200).header("Set-Cookie", "sid=abc"),
        ));
        let session = session_with(backend, FetcherConfig::default());
        session.get("https://a.test/").send().await.expect("send");
        assert!(!session.cookies().is_empty());
        session.close().await.expect("close");
        assert!(session.cookies().is_empty());
    }

    #[tokio::test]
    async fn a_rotator_and_a_static_proxy_are_refused_at_build_time() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let rotator = Arc::new(ProxyRotator::from_urls(["http://p1:1"]).expect("proxies"));
        let err = AsyncFetcherSession::builder()
            .config(
                FetcherConfig::default()
                    .with_proxy_rotator(Some(rotator))
                    .with_proxy_url("http://static:1"),
            )
            .backend(backend)
            .build()
            .expect_err("conflict");
        assert_eq!(
            err.to_string(),
            "Cannot use 'proxy_rotator' together with 'proxy' or 'proxies'. \
             Use either a static proxy or proxy rotation, not both."
        );
    }

    #[tokio::test]
    async fn a_bad_url_is_reported_before_any_i_o() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );
        let err = session.get("not a url").send().await.expect_err("bad url");
        assert!(matches!(err, FetchError::InvalidUrl { .. }));
        assert_eq!(backend.call_count(), 0);
    }

    #[tokio::test]
    async fn a_session_is_shared_by_its_clones_and_crosses_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AsyncFetcherSession>();
        assert_send_sync::<AsyncFetcherClient>();
        assert_send_sync::<Response>();

        let counter = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(MockBackend::always(html_reply("<p>x</p>")));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );

        let mut handles = Vec::new();
        for _ in 0..8 {
            let session = session.clone();
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                let page = session.get("https://a.test/").send().await.expect("send");
                assert_eq!(page.css("p::text").expect("css").texts(), ["x"]);
                counter.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for handle in handles {
            handle.await.expect("no panic");
        }
        assert_eq!(counter.load(Ordering::Relaxed), 8);
        assert_eq!(backend.call_count(), 8);
    }

    #[tokio::test]
    async fn the_body_and_the_query_reach_the_backend() {
        let backend = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(
            Arc::clone(&backend) as Arc<dyn HttpBackend>,
            FetcherConfig::default(),
        );
        session
            .post("https://a.test/submit")
            .params(vec![("page".to_owned(), "2".to_owned())])
            .json(serde_json::json!({"a": 1}))
            .send()
            .await
            .expect("send");

        let sent = &backend.sent()[0];
        assert_eq!(sent.method, Method::Post);
        assert_eq!(
            sent.effective_url().as_str(),
            "https://a.test/submit?page=2"
        );
        assert!(matches!(sent.body, Some(Body::Json(_))));
    }

    #[test]
    fn the_builder_records_presence_not_value() {
        let builder = AsyncFetcher::get("https://a.test/").timeout(None);
        assert_eq!(
            builder.overrides().timeout,
            browser_core::types::Override::Set(None)
        );
        assert!(AsyncFetcher::get("https://a.test/")
            .overrides()
            .timeout
            .is_unset());
        assert_eq!(AsyncFetcher::post("https://a.test/").method(), Method::Post);
        assert_eq!(AsyncFetcher::put("https://a.test/").method(), Method::Put);
        assert_eq!(
            AsyncFetcher::delete("https://a.test/").method(),
            Method::Delete
        );
        assert_eq!(
            AsyncFetcher::get("https://a.test/x").url(),
            "https://a.test/x"
        );
    }

    #[test]
    fn a_proxy_becomes_json_in_meta() {
        assert_eq!(proxy_to_meta(None), Value::Null);
        assert_eq!(
            proxy_to_meta(Some(&Proxy::Url("http://p:1".to_owned()))),
            Value::String("http://p:1".to_owned())
        );
        let dict = Proxy::Config {
            server: "http://p:1".to_owned(),
            username: Some("u".to_owned()),
            password: None,
        };
        assert_eq!(proxy_to_meta(Some(&dict))["server"], "http://p:1");
    }
}
