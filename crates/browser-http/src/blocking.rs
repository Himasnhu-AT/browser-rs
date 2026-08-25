//! The synchronous `Fetcher` / `FetcherSession` façade over a lazily created
//! runtime, which **fails loudly rather than deadlocking inside one**.
//!
//! `ARCHITECTURE.md` §5.2. Python's blocking `Fetcher` works with no event loop
//! at all, so a library consumer who never touches `async` should not have to
//! build a runtime by hand; this module builds one on first use and keeps it for
//! the process.
//!
//! The one rule that makes it safe: [`runtime`] refuses to hand out the runtime
//! when a `tokio` runtime is already running on the calling thread.
//! `Runtime::block_on` from inside a runtime worker panics or deadlocks
//! depending on the flavour, and a hang is far worse than an error, so the
//! façade returns [`FetchError::BlockingInsideRuntime`] and says what to use
//! instead.
//!
//! Everything here is a *thin* wrapper. No logic is duplicated: every call ends
//! in the same [`crate::session`] driver the async API uses.
//!
//! # Examples
//!
//! ```
//! use std::sync::Arc;
//! use browser_http::backend::mock::{MockBackend, MockReply};
//! use browser_http::blocking::FetcherSession;
//!
//! let backend = Arc::new(MockBackend::always(
//!     MockReply::new(200)
//!         .header("Content-Type", "text/html; charset=utf-8")
//!         .body("<h1>hello</h1>"),
//! ));
//! let session = FetcherSession::from_builder(FetcherSession::builder().backend(backend))?;
//! session.open()?;
//!
//! let page = session.get("https://example.test/").send()?;
//! assert_eq!(page.status, 200);
//! assert_eq!(page.css("h1::text")?.texts(), ["hello"]);
//!
//! session.close()?;
//! # Ok::<(), browser_http::error::FetchError>(())
//! ```

use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

use crate::backend::Method;
use crate::config::{FetcherConfig, RequestOverrides};
use crate::error::{FetchError, Result};
use crate::response::Response;
use crate::session::{AsyncFetcher, AsyncFetcherClient, AsyncFetcherSession, SessionBuilder};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The process-wide runtime the blocking façade drives.
///
/// Built on first use, multi-threaded, with timers and I/O enabled.
///
/// # Errors
///
/// * [`FetchError::BlockingInsideRuntime`] when a `tokio` runtime is already
///   running on this thread — the blocking API cannot be used from an async
///   context, and saying so beats deadlocking.
/// * [`FetchError::RuntimeStart`] when the runtime cannot be built at all.
///
/// # Examples
///
/// ```
/// # use browser_http::blocking::runtime;
/// // Outside a runtime it is available…
/// assert!(runtime().is_ok());
///
/// // …and inside one it refuses rather than deadlocking.
/// let rt = tokio::runtime::Builder::new_current_thread().build()?;
/// rt.block_on(async {
///     assert!(matches!(
///         runtime(),
///         Err(browser_http::error::FetchError::BlockingInsideRuntime)
///     ));
/// });
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn runtime() -> Result<&'static Runtime> {
    if Handle::try_current().is_ok() {
        return Err(FetchError::BlockingInsideRuntime);
    }
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let built = Builder::new_multi_thread()
        .enable_all()
        .thread_name("browser-rs-blocking")
        .build()
        .map_err(|err| FetchError::RuntimeStart(err.to_string()))?;
    Ok(RUNTIME.get_or_init(|| built))
}

/// Run one future to completion on the façade's runtime.
///
/// # Errors
///
/// As [`runtime`], plus whatever the future itself returns.
pub fn block_on<F: std::future::Future<Output = Result<T>>, T>(future: F) -> Result<T> {
    runtime()?.block_on(future)
}

/// The synchronous mirror of [`AsyncFetcherSession`].
///
/// Holds the async session and drives it on the shared runtime. Cloning is
/// cheap and shares the connection pool and cookie jar, exactly as the async
/// one does.
#[derive(Clone, Debug)]
pub struct FetcherSession(AsyncFetcherSession);

impl FetcherSession {
    /// Start building one. The builder is shared with the async API, so every
    /// knob is the same; finish it with [`FetcherSession::from_builder`].
    #[must_use]
    pub fn builder() -> SessionBuilder {
        AsyncFetcherSession::builder()
    }

    /// Finish a [`SessionBuilder`] as a blocking session.
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build`].
    pub fn from_builder(builder: SessionBuilder) -> Result<Self> {
        Ok(Self(builder.build()?))
    }

    /// A session with Python's defaults.
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build`].
    pub fn new() -> Result<Self> {
        Ok(Self(AsyncFetcherSession::new()?))
    }

    /// Wrap an existing async session.
    #[must_use]
    pub fn from_async(session: AsyncFetcherSession) -> Self {
        Self(session)
    }

    /// The async session underneath.
    #[must_use]
    pub fn inner(&self) -> &AsyncFetcherSession {
        &self.0
    }

    /// The session defaults.
    #[must_use]
    pub fn config(&self) -> &FetcherConfig {
        self.0.config()
    }

    /// Whether the session is open.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.0.is_alive()
    }

    /// `__enter__`.
    ///
    /// # Errors
    ///
    /// As [`AsyncFetcherSession::open`], plus [`runtime`]'s.
    pub fn open(&self) -> Result<()> {
        block_on(self.0.open())
    }

    /// `__exit__`.
    ///
    /// # Errors
    ///
    /// As [`AsyncFetcherSession::close`], plus [`runtime`]'s.
    pub fn close(&self) -> Result<()> {
        block_on(self.0.close())
    }

    /// `session.get(url, **kwargs)`.
    #[must_use]
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.get(url))
    }

    /// `session.post(url, **kwargs)`.
    #[must_use]
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.post(url))
    }

    /// `session.put(url, **kwargs)`.
    #[must_use]
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.put(url))
    }

    /// `session.delete(url, **kwargs)`.
    #[must_use]
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.delete(url))
    }

    /// Any of the four methods.
    #[must_use]
    pub fn request(&self, method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.request(method, url))
    }

    /// Send an already-assembled set of overrides.
    ///
    /// # Errors
    ///
    /// As [`RequestBuilder::send`].
    pub fn send(
        &self,
        method: Method,
        url: &str,
        overrides: &RequestOverrides,
    ) -> Result<Response> {
        block_on(self.0.send(method, url, overrides))
    }
}

/// The synchronous mirror of [`AsyncFetcherClient`] — one-shot, no cookie
/// persistence.
#[derive(Clone, Debug)]
pub struct FetcherClient(AsyncFetcherClient);

impl FetcherClient {
    /// Start building one; finish it with [`FetcherClient::from_builder`].
    #[must_use]
    pub fn builder() -> SessionBuilder {
        AsyncFetcherClient::builder()
    }

    /// Finish a [`SessionBuilder`] as a blocking one-shot client.
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build_client`].
    pub fn from_builder(builder: SessionBuilder) -> Result<Self> {
        Ok(Self(builder.build_client()?))
    }

    /// A client with Python's defaults.
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build_client`].
    pub fn new() -> Result<Self> {
        Ok(Self(AsyncFetcherClient::new()?))
    }

    /// Wrap an existing async client.
    #[must_use]
    pub fn from_async(client: AsyncFetcherClient) -> Self {
        Self(client)
    }

    /// `Fetcher.get(url, **kwargs)`.
    #[must_use]
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.get(url))
    }

    /// `Fetcher.post(url, **kwargs)`.
    #[must_use]
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.post(url))
    }

    /// `Fetcher.put(url, **kwargs)`.
    #[must_use]
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.put(url))
    }

    /// `Fetcher.delete(url, **kwargs)`.
    #[must_use]
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.delete(url))
    }

    /// Any of the four methods.
    #[must_use]
    pub fn request(&self, method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(self.0.request(method, url))
    }
}

/// `Fetcher` — the blocking one-shot namespace, Python's classmethods.
///
/// ```no_run
/// use browser_http::blocking::Fetcher;
///
/// let page = Fetcher::get("https://example.com").send()?;
/// let titles = page.css("h3 a::text")?.texts();
/// # let _ = titles;
/// # Ok::<(), browser_http::error::FetchError>(())
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Fetcher;

impl Fetcher {
    /// `Fetcher.get(url, **kwargs)`.
    #[must_use]
    pub fn get(url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(AsyncFetcher::get(url))
    }

    /// `Fetcher.post(url, **kwargs)`.
    #[must_use]
    pub fn post(url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(AsyncFetcher::post(url))
    }

    /// `Fetcher.put(url, **kwargs)`.
    #[must_use]
    pub fn put(url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(AsyncFetcher::put(url))
    }

    /// `Fetcher.delete(url, **kwargs)`.
    #[must_use]
    pub fn delete(url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(AsyncFetcher::delete(url))
    }

    /// Any of the four methods.
    #[must_use]
    pub fn request(method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder(AsyncFetcher::request(method, url))
    }
}

/// The blocking request builder.
///
/// [`std::ops::Deref`] would lose the fluent `self`-by-value chain, so the
/// setters are forwarded explicitly. Anything not forwarded is reachable through
/// [`RequestBuilder::map`].
#[derive(Debug)]
pub struct RequestBuilder(crate::session::RequestBuilder);

impl RequestBuilder {
    /// Apply any async-builder method that is not forwarded here.
    ///
    /// ```
    /// use browser_http::blocking::Fetcher;
    ///
    /// let builder = Fetcher::get("https://example.com")
    ///     .map(|b| b.impersonate(browser_http::config::Impersonate::single("firefox")));
    /// assert_eq!(builder.url(), "https://example.com");
    /// ```
    #[must_use]
    pub fn map(
        self,
        f: impl FnOnce(crate::session::RequestBuilder) -> crate::session::RequestBuilder,
    ) -> Self {
        Self(f(self.0))
    }

    /// The async builder underneath.
    #[must_use]
    pub fn into_inner(self) -> crate::session::RequestBuilder {
        self.0
    }

    /// The overrides collected so far.
    #[must_use]
    pub fn overrides(&self) -> &RequestOverrides {
        self.0.overrides()
    }

    /// The method this request will use.
    #[must_use]
    pub fn method(&self) -> Method {
        self.0.method()
    }

    /// The URL this request will fetch.
    #[must_use]
    pub fn url(&self) -> &str {
        self.0.url()
    }

    /// Replace the overrides wholesale.
    #[must_use]
    pub fn with_overrides(self, overrides: RequestOverrides) -> Self {
        Self(self.0.with_overrides(overrides))
    }

    /// Perform the request, blocking until it finishes.
    ///
    /// # Errors
    ///
    /// As [`crate::session::RequestBuilder::send`], plus
    /// [`FetchError::BlockingInsideRuntime`] when called from inside a `tokio`
    /// runtime.
    pub fn send(self) -> Result<Response> {
        block_on(self.0.send())
    }
}

/// Forward the fluent setters one for one, so the blocking API is the async one
/// with `.await` removed.
macro_rules! forward_setters {
    ($( $name:ident ( $($arg:ident : $ty:ty),* ) => $doc:literal );+ $(;)?) => {
        impl RequestBuilder {
            $(
                #[doc = $doc]
                #[must_use]
                pub fn $name(self, $($arg: $ty),*) -> Self {
                    Self(self.0.$name($($arg),*))
                }
            )+
        }
    };
}

forward_setters! {
    impersonate(value: crate::config::Impersonate) => "`impersonate=`.";
    http3(value: bool) => "`http3=`.";
    stealthy_headers(value: bool) => "`stealthy_headers=`.";
    proxies(value: crate::config::ProxySpec) => "`proxies=`.";
    proxy(value: Option<browser_core::types::Proxy>) => "`proxy=`.";
    proxy_url(value: String) => "`proxy=\"http://…\"`.";
    proxy_auth(value: Option<(String, String)>) => "`proxy_auth=`.";
    timeout(value: Option<std::time::Duration>) => "`timeout=`; `None` means no timeout.";
    headers(value: crate::config::HeaderDict) => "`headers=`.";
    retries(value: Option<i32>) => "`retries=`.";
    retry_delay(value: std::time::Duration) => "`retry_delay=`.";
    follow_redirects(value: browser_core::types::FollowRedirects) => "`follow_redirects=`.";
    max_redirects(value: i32) => "`max_redirects=`; `-1` is unlimited.";
    verify(value: bool) => "`verify=`.";
    cert(value: Option<crate::config::ClientCert>) => "`cert=`.";
    selector_config(value: crate::config::SelectorConfig) => "`selector_config=`.";
    http_version(value: crate::config::HttpVersion) => "`http_version=`.";
    params(value: crate::config::Query) => "`params=`.";
    cookies(value: crate::config::Cookies) => "`cookies=`.";
    body(value: crate::config::Body) => "`data=` / `json=`.";
    json(value: serde_json::Value) => "`json=`.";
}

impl RequestBuilder {
    /// Add one header.
    #[must_use]
    pub fn header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        Self(self.0.header(name, value))
    }

    /// `auth=(username, password)`.
    #[must_use]
    pub fn auth(self, username: impl Into<String>, password: impl Into<String>) -> Self {
        Self(self.0.auth(username, password))
    }

    /// `data={…}` — a url-encoded form body.
    #[must_use]
    pub fn form<I, K, V>(self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(self.0.form(pairs))
    }

    /// One pass-through option.
    #[must_use]
    pub fn extra(self, key: impl Into<String>, value: serde_json::Value) -> Self {
        Self(self.0.extra(key, value))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::backend::mock::{MockBackend, MockReply};
    use crate::backend::HttpBackend;
    use crate::error::TransportError;
    use crate::redirect_safe::HostResolver;
    use async_trait::async_trait;

    struct PublicResolver;

    #[async_trait]
    impl HostResolver for PublicResolver {
        async fn resolve(
            &self,
            _host: &str,
        ) -> std::result::Result<Vec<std::net::IpAddr>, TransportError> {
            Ok(vec!["93.184.216.34".parse().expect("ip")])
        }
    }

    fn session_with(backend: Arc<dyn HttpBackend>) -> FetcherSession {
        FetcherSession::from_builder(
            FetcherSession::builder()
                .backend(backend)
                .resolver(Arc::new(PublicResolver))
                .with_config(|config| config.with_retry_delay(Duration::ZERO)),
        )
        .expect("builds")
    }

    #[test]
    fn a_blocking_request_returns_a_parsed_response() {
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .body("<h1>hello</h1>"),
        ));
        let session = session_with(backend);
        session.open().expect("open");

        let page = session.get("https://a.test/").send().expect("send");
        assert_eq!(page.status, 200);
        assert_eq!(page.css("h1::text").expect("css").texts(), ["hello"]);
        assert!(session.is_alive());

        session.close().expect("close");
        assert!(!session.is_alive());
    }

    #[test]
    fn the_blocking_setters_reach_the_same_overrides() {
        let builder = Fetcher::get("https://a.test/")
            .timeout(None)
            .retries(Some(2))
            .header("X-Test", "1")
            .max_redirects(-1);
        assert_eq!(
            builder.overrides().timeout,
            browser_core::types::Override::Set(None)
        );
        assert_eq!(builder.overrides().max_redirects.or_default(30), -1);
        assert_eq!(builder.method(), Method::Get);
        assert_eq!(builder.url(), "https://a.test/");
        // `map` reaches anything not forwarded.
        let mapped = Fetcher::post("https://a.test/").map(|b| b.verify(false));
        assert_eq!(mapped.method(), Method::Post);
        assert!(!mapped.overrides().verify.or_default(true));
    }

    #[test]
    fn the_facade_refuses_to_run_inside_a_runtime_instead_of_deadlocking() {
        let backend: Arc<dyn HttpBackend> = Arc::new(MockBackend::always(MockReply::new(200)));
        let session = session_with(backend);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            assert!(matches!(runtime(), Err(FetchError::BlockingInsideRuntime)));
            let err = session
                .get("https://a.test/")
                .send()
                .expect_err("must refuse");
            assert!(matches!(err, FetchError::BlockingInsideRuntime));
            assert_eq!(
                err.to_string(),
                "blocking API called from inside a tokio runtime; use the async API instead"
            );
        });
    }

    #[test]
    fn a_one_shot_client_does_not_persist_cookies() {
        let backend: Arc<dyn HttpBackend> = Arc::new(MockBackend::always(
            MockReply::new(200).header("Set-Cookie", "sid=abc"),
        ));
        let client = FetcherClient::from_builder(
            FetcherClient::builder()
                .backend(Arc::clone(&backend))
                .resolver(Arc::new(PublicResolver)),
        )
        .expect("client");
        let first = client.get("https://a.test/").send().expect("send");
        assert_eq!(first.cookies["sid"], "abc");
        client.get("https://a.test/").send().expect("send");
        assert!(client.get("https://a.test/").url().starts_with("https://"));
    }

    #[test]
    fn the_runtime_is_built_once_and_shared() {
        let first = runtime().expect("runtime");
        let second = runtime().expect("runtime");
        assert!(std::ptr::eq(first, second));
    }
}
