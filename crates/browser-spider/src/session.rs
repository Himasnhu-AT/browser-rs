//! [`SessionManager`] over an `IndexMap`, with the insertion-order default fallback and
//! lazy sessions behind double-checked locking.
//!
//! A crawl routes each [`Request`] to a session by its string id (`sid`), so one spider can
//! mix an HTTP session, a stealth browser session and a proxied session in a single run.
//! Python duck-types the three session classes and branches on `isinstance`; the port
//! replaces that with the [`CrawlSession`] trait, which is the seam every fetcher
//! implements.
//!
//! # The `Response` seam
//!
//! `browser-http`'s `Response` (work unit 12) does not exist yet, and this crate must
//! stay testable without a network anyway, so [`CrawlSession::fetch`] returns
//! [`FetchedResponse`] — the narrow set of members the crawl core actually reads
//! (spider-core spec §3.2): status, body, encoding, headers, cookies, url, reason,
//! `request_headers`, `meta` and the originating `request`. When unit 12 lands it converts
//! its richer `Response` into this on the way out (and, for the dev-mode cache, back in).
//!
//! # Pinned invariant
//!
//! [`SessionManager::fetch`] must **not** mutate `request.session_kwargs`. Python had a bug
//! here: it popped `method` out of the request's own kwargs dict, so a retried copy of a
//! POST silently became a GET. The port takes `&Request`, which makes the bug
//! unrepresentable, and [`SessionManager::fetch`] is regression-tested for it anyway.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use indexmap::IndexMap;
use browser_http::response::Response as HttpResponse;
use serde_json::Value;

use crate::error::{Result, SpiderError};
use crate::request::{Item, Request, SessionKwargs};

/// The response members the crawl core reads.
///
/// Deliberately narrow — see the module docs. `body` is `Bytes` so a cache replay or a
/// browser capture can hand over a buffer without copying it.
#[derive(Clone, Debug, Default)]
pub struct FetchedResponse {
    /// HTTP status code. Drives `is_blocked`, the status counters and AutoThrottle's
    /// "was this request OK" test.
    pub status: u16,
    /// HTTP reason phrase.
    pub reason: String,
    /// The final URL, after redirects.
    pub url: String,
    /// The raw body. Its length is what `response_bytes` counts.
    pub body: Bytes,
    /// The body's character encoding, used when decoding `robots.txt`.
    pub encoding: String,
    /// Response headers, in wire order.
    pub headers: IndexMap<String, String>,
    /// The headers that were actually sent.
    pub request_headers: IndexMap<String, String>,
    /// Cookies. A JSON object for the HTTP engines and an array for the browser engines,
    /// which is exactly the shape the dev-mode cache serialises.
    pub cookies: Value,
    /// Redirect history, oldest first.
    pub history: Vec<String>,
    /// Merged metadata: the request's `meta` with the response's own on top.
    pub meta: Item,
    /// The request this response answers. Set by [`SessionManager::fetch`] and by a cache
    /// replay.
    pub request: Option<Request>,
}

impl FetchedResponse {
    /// A bare response with a status and body, for tests and cache replays.
    pub fn new(status: u16, url: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            url: url.into(),
            body: body.into(),
            ..Self::default()
        }
    }

    /// Python's two lines at the end of `SessionManager.fetch`:
    ///
    /// ```text
    /// response.request = request
    /// response.meta = {**request.meta, **response.meta}
    /// ```
    ///
    /// The response's own `meta` wins on a key collision. The cache-replay path in the
    /// engine does the same thing, which is why this is a method and not inlined.
    pub fn attach_request(&mut self, request: &Request) {
        let mut merged = request.meta.clone();
        for (key, value) in std::mem::take(&mut self.meta) {
            merged.insert(key, value);
        }
        self.meta = merged;
        self.request = Some(request.clone());
    }
}

impl std::fmt::Display for FetchedResponse {
    /// Python's `Response.__str__`: `<200 https://example.com/>`. Log lines use it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} {}>", self.status, self.url)
    }
}

// ---------------------------------------------------------------------------
// The unit-12 seam: `browser_http::response::Response` <-> `FetchedResponse`
// ---------------------------------------------------------------------------

impl From<&HttpResponse> for FetchedResponse {
    /// Narrow one of unit 12's (or unit 14's) responses down to what the crawl core reads.
    ///
    /// Everything the engine consults survives; the two things that do not are the parsed
    /// [`browser_parser::document::Document`] — a crawl re-parses through
    /// [`FetchedResponse::response_builder`] or [`browser_parser::document::Document`]
    /// directly — and the per-hop headers of the redirect history, since Python's
    /// `Response.history` is a list of URLs by the time the engine sees it.
    ///
    /// `request` is left `None`: [`SessionManager::fetch`] attaches it, exactly as Python
    /// does after the session returns.
    fn from(response: &HttpResponse) -> Self {
        Self {
            status: response.status,
            reason: response.reason.clone(),
            url: response.url.clone(),
            body: response.body().clone(),
            encoding: response.encoding.clone(),
            headers: response.headers.clone(),
            request_headers: response.request_headers.clone(),
            // The HTTP engines produce an object; the browser engines produce an array.
            // Both shapes reach the dev-mode cache unchanged — see `cache::CacheDocument`.
            cookies: Value::Object(
                response
                    .cookies
                    .iter()
                    .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                    .collect(),
            ),
            history: response.history.iter().map(|hop| hop.url.clone()).collect(),
            meta: response
                .meta
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            request: None,
        }
    }
}

impl From<HttpResponse> for FetchedResponse {
    fn from(response: HttpResponse) -> Self {
        Self::from(&response)
    }
}

impl FetchedResponse {
    /// Widen back into one of unit 12's responses, ready to be parsed.
    ///
    /// This is the dev-mode cache replay: a [`crate::cache::ResponseCacheManager`] hit never
    /// had a live exchange, and a spider callback still expects the same `Response` a live
    /// fetch would have produced. The builder is returned rather than a finished response so
    /// the caller can attach its `selector_config` and its adaptive-storage
    /// `document_hook` before the (lazy) parse is configured:
    ///
    /// ```
    /// use browser_spider::session::FetchedResponse;
    ///
    /// let cached = FetchedResponse {
    ///     encoding: "utf-8".to_owned(),
    ///     ..FetchedResponse::new(200, "https://shop.test/", "<p class=price>$9.99</p>")
    /// };
    /// let response = cached.response_builder().build();
    ///
    /// assert_eq!(response.status, 200);
    /// assert_eq!(response.css("p.price::text")?.texts(), ["$9.99"]);
    /// # Ok::<(), browser_http::error::FetchError>(())
    /// ```
    ///
    /// Two fields do not survive the round trip, both deliberately:
    ///
    /// * `history` — [`browser_http::redirect_safe::RedirectHop`] carries each hop's
    ///   status, reason and headers, and a `FetchedResponse` only kept the URLs. Python's
    ///   cache replay also reconstructs a response with `history=[]`.
    /// * `meta` — it is the crawl's merged metadata, not the transport's, and
    ///   [`FetchedResponse::attach_request`] rebuilds it on the way back in.
    #[must_use]
    pub fn response_builder(&self) -> browser_http::response::ResponseBuilder {
        let mut builder = HttpResponse::builder(self.status, self.url.clone())
            .reason(self.reason.clone())
            .body(self.body.clone())
            .headers(self.headers.clone())
            .request_headers(self.request_headers.clone());
        // An empty label would make unit 12 re-resolve the charset from `Content-Type`,
        // which is what a `FetchedResponse` that never carried one wants.
        if !self.encoding.is_empty() {
            builder = builder.encoding(self.encoding.clone());
        }
        if let Some(method) = self.request.as_ref().map(|r| r.session_kwargs.method()) {
            builder = builder.method(method.into());
        }
        if let Value::Object(cookies) = &self.cookies {
            builder = builder.cookies(
                cookies
                    .iter()
                    .filter_map(|(name, value)| {
                        value.as_str().map(|v| (name.clone(), v.to_owned()))
                    })
                    .collect(),
            );
        }
        builder
    }
}

/// One fetcher a crawl can route requests to.
///
/// Implemented by `browser-http`'s pooled session, by the browser sessions, and by test
/// doubles. All four Python behaviours the manager depends on are here explicitly, because
/// Python reaches into `session._is_alive` and `session.__aenter__` directly.
#[async_trait]
pub trait CrawlSession: Send + Sync + 'static {
    /// Whether the session has been opened and not yet closed (`_is_alive`).
    fn is_alive(&self) -> bool;

    /// Open the session (`__aenter__`). Called once by [`SessionManager::start`], or
    /// lazily on first use.
    async fn open(&self) -> Result<()>;

    /// Close the session (`__aexit__`).
    async fn close(&self) -> Result<()>;

    /// Perform one request.
    ///
    /// `kwargs` is borrowed and must not be modified — see the module docs.
    async fn fetch(&self, url: &str, kwargs: &SessionKwargs) -> Result<FetchedResponse>;
}

/// How a session is registered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddOptions {
    /// Make this the default session, overriding the insertion-order default.
    pub default: bool,
    /// Do not open this session in [`SessionManager::start`]; open it on first use
    /// instead. Useful for an expensive browser session a crawl may never reach.
    pub lazy: bool,
}

impl AddOptions {
    /// Register as the default session.
    pub const DEFAULT: Self = Self {
        default: true,
        lazy: false,
    };
    /// Register as a lazily started session.
    pub const LAZY: Self = Self {
        default: false,
        lazy: true,
    };
}

/// The registry of sessions a crawl can route to.
///
/// ```
/// # use std::sync::Arc;
/// # use browser_spider::session::{AddOptions, SessionManager};
/// # fn demo(http: Arc<dyn browser_spider::session::CrawlSession>,
/// #         browser: Arc<dyn browser_spider::session::CrawlSession>)
/// #     -> Result<(), browser_spider::error::SpiderError> {
/// let mut manager = SessionManager::new();
/// manager
///     .add("default", http, AddOptions::default())?
///     .add("browser", browser, AddOptions::LAZY)?;
///
/// // The first session registered is the default unless one asks to be.
/// assert_eq!(manager.default_session_id()?, "default");
/// # Ok(())
/// # }
/// ```
pub struct SessionManager {
    /// Insertion-ordered so `pop` can promote "the next remaining session" to default,
    /// exactly like Python's `next(iter(self._sessions), None)`.
    sessions: IndexMap<String, Arc<dyn CrawlSession>>,
    default_id: Option<String>,
    lazy: HashSet<String>,
    started: AtomicBool,
    /// Guards the lazy-open double check. One lock for all lazy sessions, as in Python.
    lazy_lock: tokio::sync::Mutex<()>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("sessions", &self.session_ids())
            .field("default", &self.default_id)
            .field("lazy", &self.lazy)
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish()
    }
}

impl SessionManager {
    /// An empty manager.
    pub fn new() -> Self {
        Self {
            sessions: IndexMap::new(),
            default_id: None,
            lazy: HashSet::new(),
            started: AtomicBool::new(false),
            lazy_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Register a session under `session_id`.
    ///
    /// The **first** session registered becomes the default; `AddOptions { default: true,
    /// .. }` overrides that at any time. Returns `&mut Self` so registrations chain.
    ///
    /// # Errors
    ///
    /// [`SpiderError::SessionAlreadyRegistered`] if the id is taken.
    pub fn add(
        &mut self,
        session_id: impl Into<String>,
        session: Arc<dyn CrawlSession>,
        options: AddOptions,
    ) -> Result<&mut Self> {
        let session_id = session_id.into();
        if self.sessions.contains_key(&session_id) {
            return Err(SpiderError::SessionAlreadyRegistered { id: session_id });
        }

        if options.default || self.default_id.is_none() {
            self.default_id = Some(session_id.clone());
        }
        if options.lazy {
            self.lazy.insert(session_id.clone());
        }
        self.sessions.insert(session_id, session);
        Ok(self)
    }

    /// Remove a session, discarding it.
    ///
    /// # Errors
    ///
    /// [`SpiderError::SessionNotFound`] if the id is not registered.
    pub fn remove(&mut self, session_id: &str) -> Result<()> {
        self.pop(session_id).map(|_| ())
    }

    /// Remove a session and hand it back.
    ///
    /// If it was the default, the next remaining session **in insertion order** becomes the
    /// default; if none remain, the manager has no default again.
    ///
    /// # Errors
    ///
    /// [`SpiderError::SessionNotFound`] if the id is not registered.
    pub fn pop(&mut self, session_id: &str) -> Result<Arc<dyn CrawlSession>> {
        // `shift_remove`, not `swap_remove`: insertion order is load-bearing here.
        let session =
            self.sessions
                .shift_remove(session_id)
                .ok_or_else(|| SpiderError::SessionNotFound {
                    id: session_id.to_owned(),
                })?;
        self.lazy.remove(session_id);
        if self.default_id.as_deref() == Some(session_id) {
            self.default_id = self.sessions.keys().next().cloned();
        }
        Ok(session)
    }

    /// The id requests fall back to when their `sid` is empty.
    ///
    /// # Errors
    ///
    /// [`SpiderError::NoSessionsRegistered`] when the manager is empty.
    pub fn default_session_id(&self) -> Result<&str> {
        self.default_id
            .as_deref()
            .ok_or(SpiderError::NoSessionsRegistered)
    }

    /// Every registered id, in insertion order.
    pub fn session_ids(&self) -> Vec<&str> {
        self.sessions.keys().map(String::as_str).collect()
    }

    /// Look a session up by id.
    ///
    /// # Errors
    ///
    /// [`SpiderError::UnknownSession`], whose message lists the available ids — Python's
    /// `KeyError(f"... Available: {available}")`.
    pub fn get(&self, session_id: &str) -> Result<&Arc<dyn CrawlSession>> {
        self.sessions
            .get(session_id)
            .ok_or_else(|| SpiderError::UnknownSession {
                id: session_id.to_owned(),
                available: self.session_ids().join(", "),
            })
    }

    /// Whether an id is registered.
    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// Whether a session was registered as lazy.
    pub fn is_lazy(&self, session_id: &str) -> bool {
        self.lazy.contains(session_id)
    }

    /// How many sessions are registered.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether no session is registered.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Whether [`SessionManager::start`] has run.
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Open every non-lazy session that is not already alive.
    ///
    /// Idempotent: a second call is a no-op until [`SessionManager::close`] runs. An error
    /// from a session aborts the rest, exactly like Python — the sessions already opened
    /// stay open and `close()` will still reach them.
    pub async fn start(&self) -> Result<()> {
        if self.is_started() {
            return Ok(());
        }
        for (sid, session) in &self.sessions {
            if !self.lazy.contains(sid) && !session.is_alive() {
                session.open().await?;
            }
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Close every started session.
    ///
    /// A lazy session that was never started is skipped, so closing a manager never opens
    /// a browser just to shut it again.
    ///
    /// Deviation from Python, deliberate: Python's loop aborts at the first failing
    /// `__aexit__` and leaks the rest. This closes **all** of them and returns the first
    /// error, because leaking a browser process is worse than losing an error message.
    pub async fn close(&self) -> Result<()> {
        let mut first_error = None;
        for (sid, session) in &self.sessions {
            if self.lazy.contains(sid) && !session.is_alive() {
                continue;
            }
            if let Err(error) = session.close().await {
                tracing::warn!("Failed to close session {sid:?}: {error}");
                first_error.get_or_insert(error);
            }
        }
        self.started.store(false, Ordering::SeqCst);
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Route `request` to its session and fetch it.
    ///
    /// Resolves an empty `sid` to the default, opens a lazy session under double-checked
    /// locking, performs the fetch, then attaches the request and merges the metadata.
    ///
    /// `request.session_kwargs` is **not** modified — see the module docs.
    ///
    /// # Errors
    ///
    /// [`SpiderError::NoSessionsRegistered`] or [`SpiderError::UnknownSession`] when the
    /// request cannot be routed, plus whatever the session's `open` / `fetch` return.
    pub async fn fetch(&self, request: &Request) -> Result<FetchedResponse> {
        let sid: String = if request.sid.is_empty() {
            self.default_session_id()?.to_owned()
        } else {
            request.sid.clone()
        };
        let session = Arc::clone(self.get(&sid)?);

        if self.lazy.contains(&sid) && !session.is_alive() {
            // Double-checked locking: the cheap `is_alive` test above avoids the lock on
            // every subsequent request, and the second one inside makes concurrent first
            // requests open the session exactly once.
            let _guard = self.lazy_lock.lock().await;
            if !session.is_alive() {
                session.open().await?;
            }
        }

        let mut response = session.fetch(&request.url, &request.session_kwargs).await?;
        response.attach_request(request);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::RequestBody;
    use parking_lot::Mutex;
    use browser_core::types::HttpMethod;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug, Default)]
    struct MockSession {
        alive: AtomicBool,
        opened: AtomicUsize,
        closed: AtomicUsize,
        fetches: Mutex<Vec<(String, SessionKwargs)>>,
        fail_fetch: bool,
    }

    impl MockSession {
        fn arc() -> Arc<MockSession> {
            Arc::new(MockSession::default())
        }
        fn failing() -> Arc<MockSession> {
            Arc::new(MockSession {
                fail_fetch: true,
                ..MockSession::default()
            })
        }
        fn last_kwargs(&self) -> SessionKwargs {
            self.fetches.lock().last().expect("a fetch").1.clone()
        }
    }

    #[async_trait]
    impl CrawlSession for MockSession {
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
        async fn open(&self) -> Result<()> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            self.alive.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            self.closed.fetch_add(1, Ordering::SeqCst);
            self.alive.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn fetch(&self, url: &str, kwargs: &SessionKwargs) -> Result<FetchedResponse> {
            self.fetches.lock().push((url.to_owned(), kwargs.clone()));
            if self.fail_fetch {
                return Err(SpiderError::Fetch {
                    sid: String::new(),
                    url: url.to_owned(),
                    message: "boom".to_owned(),
                });
            }
            let mut response = FetchedResponse::new(200, url, Bytes::from_static(b"hi"));
            response
                .meta
                .insert("from_response".to_owned(), Value::from(true));
            Ok(response)
        }
    }

    fn manager_with(ids: &[&str]) -> (SessionManager, Vec<Arc<MockSession>>) {
        let mut manager = SessionManager::new();
        let mut sessions = Vec::new();
        for id in ids {
            let session = MockSession::arc();
            manager
                .add(*id, session.clone(), AddOptions::default())
                .expect("add");
            sessions.push(session);
        }
        (manager, sessions)
    }

    #[test]
    fn an_empty_manager_has_no_default() {
        let manager = SessionManager::new();
        assert!(manager.is_empty());
        let error = manager.default_session_id().expect_err("no sessions");
        assert_eq!(error.to_string(), "No sessions registered");
    }

    #[test]
    fn the_first_session_is_the_default_unless_overridden() {
        let (manager, _) = manager_with(&["a", "b"]);
        assert_eq!(manager.default_session_id().expect("default"), "a");

        let mut manager = SessionManager::new();
        manager
            .add("a", MockSession::arc(), AddOptions::default())
            .expect("add")
            .add("b", MockSession::arc(), AddOptions::DEFAULT)
            .expect("add");
        assert_eq!(manager.default_session_id().expect("default"), "b");
    }

    #[test]
    fn a_duplicate_id_is_rejected() {
        let (mut manager, _) = manager_with(&["a"]);
        let error = manager
            .add("a", MockSession::arc(), AddOptions::default())
            .expect_err("duplicate");
        assert_eq!(error.to_string(), "Session 'a' already registered");
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn get_lists_the_available_ids() {
        let (manager, _) = manager_with(&["alpha", "beta"]);
        let Err(error) = manager.get("missing") else {
            panic!("an unknown session must not resolve")
        };
        assert_eq!(
            error.to_string(),
            "Session 'missing' not found. Available: alpha, beta"
        );
    }

    #[test]
    fn pop_promotes_the_next_session_in_insertion_order() {
        let (mut manager, _) = manager_with(&["a", "b", "c"]);
        manager.pop("a").expect("pop");
        assert_eq!(manager.default_session_id().expect("default"), "b");
        assert_eq!(manager.session_ids(), ["b", "c"]);

        manager.pop("c").expect("pop"); // not the default: no promotion
        assert_eq!(manager.default_session_id().expect("default"), "b");

        manager.pop("b").expect("pop");
        assert!(manager.default_session_id().is_err());
    }

    #[test]
    fn pop_of_a_missing_id_has_the_short_message() {
        let (mut manager, _) = manager_with(&["a"]);
        let Err(error) = manager.pop("nope") else {
            panic!("an unknown session must not resolve")
        };
        assert_eq!(error.to_string(), "Session 'nope' not found");
    }

    #[tokio::test]
    async fn start_skips_lazy_sessions_and_is_idempotent() {
        let eager = MockSession::arc();
        let lazy = MockSession::arc();
        let mut manager = SessionManager::new();
        manager
            .add("eager", eager.clone(), AddOptions::default())
            .expect("add")
            .add("lazy", lazy.clone(), AddOptions::LAZY)
            .expect("add");

        manager.start().await.expect("start");
        manager.start().await.expect("start again");
        assert_eq!(eager.opened.load(Ordering::SeqCst), 1);
        assert_eq!(lazy.opened.load(Ordering::SeqCst), 0);
        assert!(manager.is_started());
    }

    #[tokio::test]
    async fn close_skips_lazy_sessions_that_never_started() {
        let eager = MockSession::arc();
        let lazy = MockSession::arc();
        let mut manager = SessionManager::new();
        manager
            .add("eager", eager.clone(), AddOptions::default())
            .expect("add")
            .add("lazy", lazy.clone(), AddOptions::LAZY)
            .expect("add");

        manager.start().await.expect("start");
        manager.close().await.expect("close");
        assert_eq!(eager.closed.load(Ordering::SeqCst), 1);
        assert_eq!(lazy.closed.load(Ordering::SeqCst), 0);
        assert!(!manager.is_started());
    }

    #[tokio::test]
    async fn fetch_opens_a_lazy_session_exactly_once() {
        let lazy = MockSession::arc();
        let mut manager = SessionManager::new();
        manager
            .add("lazy", lazy.clone(), AddOptions::LAZY)
            .expect("add");
        manager.start().await.expect("start");
        assert_eq!(lazy.opened.load(Ordering::SeqCst), 0);

        let request = Request::new("http://example.com/a").with_sid("lazy");
        manager.fetch(&request).await.expect("fetch");
        manager.fetch(&request).await.expect("fetch");
        assert_eq!(lazy.opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_first_requests_open_a_lazy_session_once() {
        let lazy = MockSession::arc();
        let mut manager = SessionManager::new();
        manager
            .add("lazy", lazy.clone(), AddOptions::LAZY)
            .expect("add");
        let manager = Arc::new(manager);

        let mut tasks = Vec::new();
        for i in 0..16 {
            let manager = Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                let request = Request::new(format!("http://example.com/{i}")).with_sid("lazy");
                manager.fetch(&request).await.expect("fetch");
            }));
        }
        for task in tasks {
            task.await.expect("join");
        }
        assert_eq!(lazy.opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fetch_resolves_an_empty_sid_to_the_default() {
        let (manager, sessions) = manager_with(&["first", "second"]);
        manager
            .fetch(&Request::new("http://example.com/x"))
            .await
            .expect("fetch");
        assert_eq!(sessions[0].fetches.lock().len(), 1);
        assert_eq!(sessions[1].fetches.lock().len(), 0);
    }

    #[tokio::test]
    async fn fetch_of_an_unknown_sid_reports_the_available_ones() {
        let (manager, _) = manager_with(&["a"]);
        let request = Request::new("http://example.com/x").with_sid("ghost");
        let error = manager.fetch(&request).await.expect_err("unknown session");
        assert_eq!(error.to_string(), "Session 'ghost' not found. Available: a");
    }

    #[tokio::test]
    async fn fetch_never_mutates_the_requests_session_kwargs() {
        // Python's bug: `kwargs.pop("method")` on the request's own dict turned a retried
        // POST into a GET. Fetch twice and assert the method survives both times.
        let (manager, sessions) = manager_with(&["a"]);
        let kwargs = SessionKwargs {
            method: Some(HttpMethod::Post),
            data: Some(RequestBody::Text("x=1".into())),
            ..SessionKwargs::default()
        };
        let request = Request::new("http://example.com/x").with_session_kwargs(kwargs.clone());

        manager.fetch(&request).await.expect("fetch");
        assert_eq!(sessions[0].last_kwargs().method, Some(HttpMethod::Post));
        assert_eq!(request.session_kwargs, kwargs);

        manager.fetch(&request).await.expect("fetch");
        assert_eq!(sessions[0].last_kwargs().method, Some(HttpMethod::Post));
        assert_eq!(request.session_kwargs, kwargs);
    }

    #[tokio::test]
    async fn fetch_attaches_the_request_and_merges_meta_with_the_response_winning() {
        let (manager, _) = manager_with(&["a"]);
        let mut meta = Item::new();
        meta.insert("depth".to_owned(), Value::from(2));
        meta.insert("from_response".to_owned(), Value::from(false));
        let request = Request::new("http://example.com/x").with_meta(meta);

        let response = manager.fetch(&request).await.expect("fetch");
        assert_eq!(response.meta["depth"], Value::from(2));
        // The response's own key wins over the request's.
        assert_eq!(response.meta["from_response"], Value::from(true));
        assert_eq!(
            response.request.as_ref().map(|r| r.url.as_str()),
            Some("http://example.com/x")
        );
        assert_eq!(response.to_string(), "<200 http://example.com/x>");
    }

    #[tokio::test]
    async fn a_failing_fetch_propagates() {
        let mut manager = SessionManager::new();
        manager
            .add("a", MockSession::failing(), AddOptions::default())
            .expect("add");
        let error = manager
            .fetch(&Request::new("http://example.com/x"))
            .await
            .expect_err("fetch fails");
        assert!(matches!(error, SpiderError::Fetch { .. }));
    }

    #[test]
    fn contains_len_and_is_lazy() {
        let mut manager = SessionManager::new();
        manager
            .add("a", MockSession::arc(), AddOptions::default())
            .expect("add")
            .add("b", MockSession::arc(), AddOptions::LAZY)
            .expect("add");
        assert!(manager.contains("a"));
        assert!(!manager.contains("z"));
        assert!(!manager.is_lazy("a"));
        assert!(manager.is_lazy("b"));
        assert_eq!(manager.len(), 2);
    }
}
