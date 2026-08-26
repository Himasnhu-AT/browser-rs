//! The [`Spider`] trait (`ARCHITECTURE.md` §5.5), [`SpiderSettings`] and [`BLOCKED_CODES`],
//! plus the response wrapper a callback receives and the [`SpiderRunner`] entry points.
//!
//! Python builds a crawl by *subclassing* `Spider` and overriding async methods; every
//! knob is a class attribute. Rust has no subclassing, so the port splits that in two:
//!
//! * the **behaviour** becomes a trait with default method bodies ([`Spider`]), and
//! * the **class attributes** become a plain struct ([`SpiderSettings`]) the trait hands
//!   back from [`Spider::settings`].
//!
//! The one structural change is that **callbacks are named ids, not closures**
//! ([`crate::request::CallbackId`]). Python stores a bound method on the `Request` and
//! re-binds it with `getattr` after unpickling; a `Box<dyn Fn>` could not survive a
//! checkpoint round-trip at all, so the port stores the name and resolves it through
//! [`Spider::dispatch`]. An unknown name falls back to [`Spider::parse`], exactly like
//! `getattr(spider, name, None) or spider.parse`.
//!
//! # A whole spider
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use async_trait::async_trait;
//! use browser_spider::request::{Item, Request};
//! use browser_spider::spider::{CallbackStream, CrawlResponse, RunOptions, Spider,
//!                                SpiderRunner, SpiderSettings, Yielded};
//!
//! struct Quotes {
//!     settings: SpiderSettings,
//! }
//!
//! #[async_trait]
//! impl Spider for Quotes {
//!     fn name(&self) -> &str {
//!         "quotes"
//!     }
//!     fn settings(&self) -> &SpiderSettings {
//!         &self.settings
//!     }
//!
//!     async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
//!         Box::pin(async_stream::stream! {
//!             for quote in response.css(".quote").into_iter().flatten() {
//!                 let mut item = Item::new();
//!                 item.insert("text".into(), quote.text().into());
//!                 yield Yielded::item(item);
//!             }
//!             if let Ok(Some(next)) = response.css_first("li.next a::attr(href)") {
//!                 if let Ok(request) = response.follow(&next.text()) {
//!                     yield Yielded::request(request);
//!                 }
//!             }
//!         })
//!     }
//! }
//!
//! # async fn run() -> Result<(), browser_spider::spider::CrawlError> {
//! let spider = Arc::new(Quotes {
//!     settings: SpiderSettings::default().with_start_urls(["https://quotes.test/"]),
//! });
//! let runner = SpiderRunner::new(spider, RunOptions::default())?;
//! let result = runner.run().await?;
//! println!("{} items", result.len());
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use browser_core::log::LevelCounter;
use browser_http::config::{Body, HeaderDict, ProxySpec, SelectorConfig};
use browser_http::response::{DocumentHook, Response as HttpResponse};
use browser_http::session::AsyncFetcherSession;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;

use crate::engine::CrawlerEngine;
use crate::error::{Result as SpiderResult, SpiderError};
use crate::request::{CallbackId, FingerprintOptions, Item, Request, RequestBody, SessionKwargs};
use crate::session::{AddOptions, CrawlSession, FetchedResponse, SessionManager};
use crate::stats::{CrawlResult, CrawlStats};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The statuses [`Spider::is_blocked`] treats as "we got blocked" by default.
///
/// Python's `BLOCKED_CODES = {401, 403, 407, 429, 444, 500, 502, 503, 504}` in
/// `reference/spiders/spider.py`, as a sorted array so membership is a linear scan over
/// nine `u16`s rather than a hash lookup.
///
/// ```
/// use browser_spider::spider::{is_blocked_status, BLOCKED_CODES};
///
/// assert_eq!(BLOCKED_CODES.len(), 9);
/// assert!(is_blocked_status(403));
/// assert!(is_blocked_status(429));
/// assert!(!is_blocked_status(200));
/// assert!(!is_blocked_status(404));
/// ```
pub const BLOCKED_CODES: [u16; 9] = [401, 403, 407, 429, 444, 500, 502, 503, 504];

/// `status in BLOCKED_CODES` — the default block test.
#[must_use]
pub fn is_blocked_status(status: u16) -> bool {
    BLOCKED_CODES.contains(&status)
}

/// Default global in-flight cap (`Spider.concurrent_requests`).
pub const DEFAULT_CONCURRENT_REQUESTS: usize = 4;
/// Default per-domain cap (`Spider.concurrent_requests_per_domain`); `0` disables it.
pub const DEFAULT_CONCURRENT_REQUESTS_PER_DOMAIN: usize = 0;
/// Default retry budget for blocked responses (`Spider.max_blocked_retries`).
pub const DEFAULT_MAX_BLOCKED_RETRIES: u32 = 3;
/// Default AutoThrottle start delay, in seconds.
pub const DEFAULT_AUTOTHROTTLE_START_DELAY: f64 = 5.0;
/// Default AutoThrottle ceiling, in seconds.
pub const DEFAULT_AUTOTHROTTLE_MAX_DELAY: f64 = 60.0;
/// The item channel's capacity in streaming mode — Python's
/// `create_memory_object_stream[dict](100)`.
pub const STREAM_BUFFER: usize = 100;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything a crawl can fail with.
///
/// Kept separate from [`SpiderError`] (which unit 15 owns and this unit may not edit)
/// and layered on top of it: every data-plane failure arrives here through
/// [`CrawlError::Spider`]. The message strings Python's tests pin are reproduced verbatim.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CrawlError {
    /// A data-plane failure: routing, fetching, checkpointing or exporting.
    #[error(transparent)]
    Spider(#[from] SpiderError),

    /// `start_requests()` was called with no `start_urls` and was not overridden.
    ///
    /// Python: `RuntimeError("Spider has no starting point, either set `start_urls` or
    /// override `start_requests` function.")`.
    #[error(
        "Spider has no starting point, either set `start_urls` or override `start_requests` function."
    )]
    NoStartingPoint,

    /// [`Spider::configure_sessions`] returned an error.
    ///
    /// Python raises `SessionConfigurationError(f"Error in {cls}.configure_sessions(): {e}")`.
    #[error("Error in {spider}.configure_sessions(): {message}")]
    SessionConfiguration {
        /// The spider's name.
        spider: String,
        /// The underlying failure's `Display` output.
        message: String,
    },

    /// [`Spider::configure_sessions`] registered nothing.
    ///
    /// Python: `SessionConfigurationError(f"{cls}.configure_sessions() did not add any sessions")`.
    #[error("{spider}.configure_sessions() did not add any sessions")]
    NoSessionsConfigured {
        /// The spider's name.
        spider: String,
    },

    /// [`Spider::name`] returned an empty string.
    ///
    /// Python: `ValueError(f"{cls} must have a name.")`, raised when the class attribute
    /// is still `None`. The port cannot express "no name", so an empty one is the
    /// equivalent.
    #[error("Spider must have a name.")]
    MissingName,

    /// [`SpiderRunner::pause`] was called with no crawl running.
    ///
    /// Python: `RuntimeError("No active crawl to stop")`.
    #[error("No active crawl to stop")]
    NoActiveCrawl,

    /// [`SpiderRunner::stats`] was read with no crawl running.
    #[error("No active crawl. Use this property inside `async for item in spider.stream():`")]
    NoActiveCrawlStats,

    /// [`CrawlResponse::follow`] was called on a response with no request attached.
    ///
    /// Python: `TypeError("This response has no request set yet.")`.
    #[error("This response has no request set yet.")]
    NoRequestOnResponse,

    /// A user callback failed. This is what [`Yielded::Error`] carries and what
    /// [`Spider::on_error`] receives for a callback failure, mirroring the `except
    /// Exception` around Python's `async for result in callback(response)`.
    #[error("{message}")]
    Callback {
        /// The failure's message.
        message: String,
    },

    /// The spider's AutoThrottle settings are not a valid [`crate::throttle::AutoThrottle`]
    /// configuration — a non-positive target concurrency, or a ceiling below the start
    /// delay.
    #[error("invalid AutoThrottle configuration: {message}")]
    AutoThrottle {
        /// The validator's own message.
        message: String,
    },

    /// [`SpiderRunner::start_blocking`] could not build (or was called from inside) a
    /// `tokio` runtime.
    #[error("the crawl runtime could not be started: {message}")]
    Runtime {
        /// What went wrong.
        message: String,
    },
}

impl CrawlError {
    /// Wrap a callback failure, for [`Yielded::error`] and [`Spider::on_error`].
    #[must_use]
    pub fn callback(message: impl std::fmt::Display) -> Self {
        Self::Callback {
            message: message.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// What a callback yields
// ---------------------------------------------------------------------------

/// One value a spider callback yields.
///
/// Python's callbacks are async generators yielding `dict | Request | None`, and the
/// engine dispatches with `isinstance`. This enum replaces that dispatch, so
/// "spider returned something that is neither" — which Python logs at ERROR and drops —
/// is unrepresentable here.
///
/// [`Yielded::Error`] has no Python counterpart *as a value*: it is how a callback
/// reports the exception Python would have raised. The engine treats it identically —
/// log at ERROR, call [`Spider::on_error`], stop consuming the stream.
///
/// The [`Request`] is boxed because it is an order of magnitude larger than an [`Item`]
/// and would otherwise set the size of every yielded value.
#[derive(Debug)]
pub enum Yielded {
    /// A follow-up request. Checked *before* [`Yielded::Item`], as Python checks
    /// `isinstance(result, Request)` first.
    Request(Box<Request>),
    /// A scraped item. An **empty** item is dropped, because Python tests `if processed:`.
    Item(Item),
    /// Python's `yield None`: silently ignored.
    Skip,
    /// The callback failed. Equivalent to raising inside Python's async generator.
    Error(CrawlError),
}

impl Yielded {
    /// Yield a follow-up request.
    #[must_use]
    pub fn request(request: Request) -> Self {
        Self::Request(Box::new(request))
    }

    /// Yield a scraped item.
    #[must_use]
    pub fn item(item: Item) -> Self {
        Self::Item(item)
    }

    /// Report a callback failure.
    #[must_use]
    pub fn error(message: impl std::fmt::Display) -> Self {
        Self::Error(CrawlError::callback(message))
    }
}

impl From<Request> for Yielded {
    fn from(request: Request) -> Self {
        Self::request(request)
    }
}

impl From<Item> for Yielded {
    fn from(item: Item) -> Self {
        Self::Item(item)
    }
}

/// What [`Spider::parse`] and [`Spider::dispatch`] return: Python's async generator.
pub type CallbackStream = BoxStream<'static, Yielded>;

/// What [`Spider::start_requests`] returns.
pub type RequestStream = BoxStream<'static, Request>;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Every `Spider` class attribute, as a struct.
///
/// Python's "custom settings" mechanism is plain class-attribute overriding; there is no
/// settings dictionary. A struct with a `Default` matching Python's defaults is the direct
/// translation, and it keeps the trait free of a dozen getter methods.
///
/// ```
/// use browser_spider::spider::SpiderSettings;
///
/// let settings = SpiderSettings::default();
/// assert_eq!(settings.concurrent_requests, 4);
/// assert_eq!(settings.concurrent_requests_per_domain, 0);
/// assert_eq!(settings.download_delay, 0.0);
/// assert_eq!(settings.max_blocked_retries, 3);
/// assert!(!settings.robots_txt_obey);
/// assert!(!settings.autothrottle_enabled);
/// assert_eq!(settings.autothrottle_start_delay, 5.0);
/// assert_eq!(settings.autothrottle_max_delay, 60.0);
/// assert!(settings.autothrottle_block_backoff);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SpiderSettings {
    /// Seed URLs for the default [`Spider::start_requests`], and the domains
    /// `robots.txt` is prefetched for.
    pub start_urls: Vec<String>,
    /// Empty allows everything. A domain matches when it is equal to an entry or ends
    /// with `"." + entry`; the comparison uses the **netloc**, so a port is part of the
    /// domain and `{"example.com"}` rejects `https://example.com:8443/`.
    pub allowed_domains: BTreeSet<String>,
    /// Consult `robots.txt` before every fetch, and let it raise the per-domain delay.
    pub robots_txt_obey: bool,
    /// Cache every response to disk and replay it on the next run.
    pub development_mode: bool,
    /// Where the dev-mode cache lives. `None` means `.browser_rs_cache/{name}`.
    pub development_cache_dir: Option<PathBuf>,
    /// Global in-flight cap, and the spawn cap in the main loop. Clamped to at least 1.
    pub concurrent_requests: usize,
    /// Per-domain in-flight cap. `0` disables it, and the global limiter is used instead.
    pub concurrent_requests_per_domain: usize,
    /// Seconds slept before each fetch, and AutoThrottle's floor.
    pub download_delay: f64,
    /// How many times a blocked request is retried.
    pub max_blocked_retries: u32,
    /// Enable the AutoThrottle feedback loop.
    pub autothrottle_enabled: bool,
    /// AutoThrottle's initial per-domain delay.
    pub autothrottle_start_delay: f64,
    /// AutoThrottle's ceiling.
    pub autothrottle_max_delay: f64,
    /// AutoThrottle's target concurrency. `None` resolves to
    /// `concurrent_requests_per_domain or 1.0`.
    pub autothrottle_target_concurrency: Option<f64>,
    /// Whether a blocked response doubles the delay.
    pub autothrottle_block_backoff: bool,
    /// Fingerprint the non-body session kwargs.
    pub fp_include_kwargs: bool,
    /// Keep `#fragment` when canonicalising the URL for the fingerprint.
    pub fp_keep_fragments: bool,
    /// Fingerprint the request headers.
    pub fp_include_headers: bool,
    /// The level the run's subscriber should be configured at.
    ///
    /// **Declarative only.** `tracing` has no per-logger level the way Python's `logging`
    /// does, so nothing in this crate reads it: a runner (or the CLI) builds the
    /// subscriber from it. Python's `logging_format` / `logging_date_format` have no
    /// analogue at all and are dropped — see `browser_core::log`.
    pub logging_level: tracing::Level,
    /// An extra log file the run's subscriber should write to. Declarative, as above.
    pub log_file: Option<PathBuf>,
}

impl Default for SpiderSettings {
    fn default() -> Self {
        Self {
            start_urls: Vec::new(),
            allowed_domains: BTreeSet::new(),
            robots_txt_obey: false,
            development_mode: false,
            development_cache_dir: None,
            concurrent_requests: DEFAULT_CONCURRENT_REQUESTS,
            concurrent_requests_per_domain: DEFAULT_CONCURRENT_REQUESTS_PER_DOMAIN,
            download_delay: 0.0,
            max_blocked_retries: DEFAULT_MAX_BLOCKED_RETRIES,
            autothrottle_enabled: false,
            autothrottle_start_delay: DEFAULT_AUTOTHROTTLE_START_DELAY,
            autothrottle_max_delay: DEFAULT_AUTOTHROTTLE_MAX_DELAY,
            autothrottle_target_concurrency: None,
            autothrottle_block_backoff: true,
            fp_include_kwargs: false,
            fp_keep_fragments: false,
            fp_include_headers: false,
            logging_level: tracing::Level::DEBUG,
            log_file: None,
        }
    }
}

impl SpiderSettings {
    /// The three fingerprint switches, as the [`crate::scheduler::Scheduler`] wants them.
    #[must_use]
    pub fn fingerprint_options(&self) -> FingerprintOptions {
        FingerprintOptions {
            include_kwargs: self.fp_include_kwargs,
            include_headers: self.fp_include_headers,
            keep_fragments: self.fp_keep_fragments,
        }
    }

    /// The global cap, clamped to at least one.
    ///
    /// `CapacityLimiter(0)` raises in Python; a `Semaphore` with zero permits would
    /// simply deadlock, and the spawn check `active_tasks >= 0` would spin forever, so
    /// the port clamps instead.
    #[must_use]
    pub fn effective_concurrency(&self) -> usize {
        self.concurrent_requests.max(1)
    }

    /// `spider.allowed_domains` matching: equal, or a subdomain of, an allowed entry.
    ///
    /// ```
    /// use browser_spider::spider::SpiderSettings;
    ///
    /// let settings = SpiderSettings::default().with_allowed_domains(["example.com"]);
    /// assert!(settings.is_domain_allowed("example.com"));
    /// assert!(settings.is_domain_allowed("deep.sub.example.com"));
    /// assert!(!settings.is_domain_allowed("notexample.com"));
    /// // The netloc carries the port, so an explicit one is a different domain.
    /// assert!(!settings.is_domain_allowed("example.com:8443"));
    /// // An empty set allows everything.
    /// assert!(SpiderSettings::default().is_domain_allowed("anything.test"));
    /// ```
    #[must_use]
    pub fn is_domain_allowed(&self, domain: &str) -> bool {
        if self.allowed_domains.is_empty() {
            return true;
        }
        self.allowed_domains
            .iter()
            .any(|allowed| domain == allowed || domain.ends_with(&format!(".{allowed}")))
    }

    /// Builder: seed URLs.
    #[must_use]
    pub fn with_start_urls<I, S>(mut self, urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.start_urls = urls.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: the offsite filter.
    #[must_use]
    pub fn with_allowed_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_domains = domains.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: obey `robots.txt`.
    #[must_use]
    pub fn with_robots_txt_obey(mut self, obey: bool) -> Self {
        self.robots_txt_obey = obey;
        self
    }

    /// Builder: the dev-mode response cache.
    #[must_use]
    pub fn with_development_mode(mut self, enabled: bool) -> Self {
        self.development_mode = enabled;
        self
    }

    /// Builder: where the dev-mode cache lives.
    #[must_use]
    pub fn with_development_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.development_cache_dir = Some(dir.into());
        self
    }

    /// Builder: the global in-flight cap.
    #[must_use]
    pub fn with_concurrent_requests(mut self, count: usize) -> Self {
        self.concurrent_requests = count;
        self
    }

    /// Builder: the per-domain in-flight cap (`0` disables it).
    #[must_use]
    pub fn with_concurrent_requests_per_domain(mut self, count: usize) -> Self {
        self.concurrent_requests_per_domain = count;
        self
    }

    /// Builder: the fixed pre-fetch sleep, in seconds.
    #[must_use]
    pub fn with_download_delay(mut self, seconds: f64) -> Self {
        self.download_delay = seconds;
        self
    }

    /// Builder: the blocked-retry budget.
    #[must_use]
    pub fn with_max_blocked_retries(mut self, retries: u32) -> Self {
        self.max_blocked_retries = retries;
        self
    }

    /// Builder: enable AutoThrottle.
    #[must_use]
    pub fn with_autothrottle(mut self, enabled: bool) -> Self {
        self.autothrottle_enabled = enabled;
        self
    }

    /// Builder: AutoThrottle's start delay, ceiling and target concurrency.
    #[must_use]
    pub fn with_autothrottle_tuning(
        mut self,
        start_delay: f64,
        max_delay: f64,
        target_concurrency: Option<f64>,
    ) -> Self {
        self.autothrottle_start_delay = start_delay;
        self.autothrottle_max_delay = max_delay;
        self.autothrottle_target_concurrency = target_concurrency;
        self
    }

    /// Builder: the three fingerprint switches.
    #[must_use]
    pub fn with_fingerprint_options(mut self, options: FingerprintOptions) -> Self {
        self.fp_include_kwargs = options.include_kwargs;
        self.fp_include_headers = options.include_headers;
        self.fp_keep_fragments = options.keep_fragments;
        self
    }
}

// ---------------------------------------------------------------------------
// The response a callback sees
// ---------------------------------------------------------------------------

/// The response handed to a spider callback.
///
/// Unit 12's [`browser_http::response::Response`] is the only response type in the
/// workspace, and this is it plus the two members Python's `Response` grows once a crawl
/// owns it: the [`Request`] that produced it and the **merged** metadata
/// (`{**request.meta, **response.meta}`).
///
/// [`Deref`](std::ops::Deref) makes the whole parse API — `css`, `xpath`, `urljoin`,
/// `body_text`, `status`, `headers` — reachable directly.
///
/// ```
/// use browser_spider::session::FetchedResponse;
/// use browser_spider::spider::CrawlResponse;
///
/// let fetched = FetchedResponse::new(200, "https://shop.test/", "<h1>Widget</h1>");
/// let response = CrawlResponse::from_fetched(&fetched, &Default::default(), None);
///
/// assert_eq!(response.status, 200);
/// assert_eq!(response.css("h1::text")?.texts(), ["Widget"]);
/// assert_eq!(response.to_string(), "<200 https://shop.test/>");
/// # Ok::<(), browser_http::error::FetchError>(())
/// ```
#[derive(Debug)]
pub struct CrawlResponse {
    inner: HttpResponse,
    /// The request this response answers. `None` only for a hand-built response.
    pub request: Option<Request>,
    /// `{**request.meta, **response.meta}` — the response's own keys win.
    pub meta: Item,
}

impl CrawlResponse {
    /// Wrap a finished [`browser_http::response::Response`] with no request attached.
    #[must_use]
    pub fn new(response: HttpResponse) -> Self {
        let meta = response
            .meta
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            inner: response,
            request: None,
            meta,
        }
    }

    /// Build the response a callback sees out of what the crawl core carries.
    ///
    /// This is the single place a live fetch and a dev-cache replay converge: both hand
    /// over a [`FetchedResponse`], and both are widened through
    /// [`FetchedResponse::response_builder`] with the crawl's `selector_config` and its
    /// adaptive-storage `document_hook` attached, so a replayed page parses exactly like
    /// a fresh one.
    #[must_use]
    pub fn from_fetched(
        fetched: &FetchedResponse,
        selector_config: &SelectorConfig,
        document_hook: Option<&DocumentHook>,
    ) -> Self {
        let mut builder = fetched
            .response_builder()
            .selector_config(selector_config.clone());
        if let Some(hook) = document_hook {
            builder = builder.document_hook(Arc::clone(hook));
        }
        Self {
            inner: builder.build(),
            request: fetched.request.clone(),
            meta: fetched.meta.clone(),
        }
    }

    /// The wrapped transport response.
    #[must_use]
    pub fn inner(&self) -> &HttpResponse {
        &self.inner
    }

    /// Unwrap, dropping the crawl-level members.
    #[must_use]
    pub fn into_inner(self) -> HttpResponse {
        self.inner
    }

    /// Build a follow-up [`Request`] for `url`, inheriting everything from this response's
    /// own request. The canonical way for a callback to walk a site.
    ///
    /// # Errors
    ///
    /// [`CrawlError::NoRequestOnResponse`] when no request is attached — Python raises
    /// `TypeError("This response has no request set yet.")`.
    pub fn follow(&self, url: &str) -> Result<Request, CrawlError> {
        self.follow_with(url, &FollowOptions::default())
    }

    /// [`CrawlResponse::follow`] with every knob.
    ///
    /// Reproduces `Response.follow` from `reference/engines/toolbelt/custom.py`:
    ///
    /// 1. the URL is joined against this response's URL;
    /// 2. `sid`, `callback` and `priority` are inherited when the option is empty/`None`;
    /// 3. **`dont_filter` is never inherited** — it always starts from the option;
    /// 4. `meta` is `{**response.meta, **options.meta}`;
    /// 5. session kwargs are `{**request.session_kwargs, **options.session_kwargs}`;
    /// 6. with `referer_flow`, `headers["referer"]` and `extra_headers["referer"]` are set
    ///    to this response's URL and `google_search` is forced to `false`.
    ///
    /// **Deliberate deviation:** Python mutates the *inherited* headers dict in place, so a
    /// referer can leak back into a shared `headers` object (spec §8 gotcha 17). The port
    /// clones, so it cannot.
    ///
    /// # Errors
    ///
    /// As [`CrawlResponse::follow`].
    pub fn follow_with(&self, url: &str, options: &FollowOptions) -> Result<Request, CrawlError> {
        let request = self
            .request
            .as_ref()
            .ok_or(CrawlError::NoRequestOnResponse)?;

        let mut kwargs =
            merge_session_kwargs(&request.session_kwargs, options.session_kwargs.as_ref());
        if options.referer_flow {
            kwargs
                .headers
                .get_or_insert_with(HeaderDict::new)
                .insert("referer".to_owned(), self.inner.url.clone());
            kwargs
                .extra_headers
                .get_or_insert_with(HeaderDict::new)
                .insert("referer".to_owned(), self.inner.url.clone());
            kwargs
                .extra
                .insert("google_search".to_owned(), Value::Bool(false));
        }

        let mut meta = self.meta.clone();
        if let Some(extra) = &options.meta {
            for (key, value) in extra {
                meta.insert(key.clone(), value.clone());
            }
        }

        let sid = if options.sid.is_empty() {
            request.sid.clone()
        } else {
            options.sid.clone()
        };
        let callback = options
            .callback
            .clone()
            .or_else(|| request.callback.clone());
        let priority = options.priority.unwrap_or(request.priority);

        let mut follow_up = Request::new(self.inner.urljoin(url))
            .with_sid(sid)
            .with_priority(priority)
            .with_dont_filter(options.dont_filter)
            .with_meta(meta)
            .with_session_kwargs(kwargs);
        follow_up.callback = callback;
        Ok(follow_up)
    }
}

impl std::ops::Deref for CrawlResponse {
    type Target = HttpResponse;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::fmt::Display for CrawlResponse {
    /// Python's `Response.__str__`: `<200 https://example.com/>`, which is what the
    /// engine's "Scraped from …" and "Dropped from …" log lines interpolate.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} {}>", self.inner.status, self.inner.url)
    }
}

/// The keyword arguments of `Response.follow`.
///
/// `referer_flow` defaults to `true`; everything else defaults to "inherit", which is why
/// the fields are `Option` rather than plain values.
#[derive(Clone, Debug)]
pub struct FollowOptions {
    /// `sid=`; empty inherits the response's request's session id.
    pub sid: String,
    /// `callback=`; `None` inherits.
    pub callback: Option<CallbackId>,
    /// `priority=`; `None` inherits.
    pub priority: Option<i32>,
    /// `dont_filter=`. **Never inherited.**
    pub dont_filter: bool,
    /// `meta=`; merged on top of the response's meta.
    pub meta: Option<Item>,
    /// `referer_flow=`; sets `referer` on both header maps and disables `google_search`.
    pub referer_flow: bool,
    /// `**kwargs`; merged over the originating request's session kwargs.
    pub session_kwargs: Option<SessionKwargs>,
}

impl Default for FollowOptions {
    fn default() -> Self {
        Self {
            sid: String::new(),
            callback: None,
            priority: None,
            dont_filter: false,
            meta: None,
            referer_flow: true,
            session_kwargs: None,
        }
    }
}

/// `{**base, **over}` for typed session kwargs: a set field on `over` wins, an unset one
/// falls through to `base`, and `extra` merges key by key.
fn merge_session_kwargs(base: &SessionKwargs, over: Option<&SessionKwargs>) -> SessionKwargs {
    let mut merged = base.clone();
    let Some(over) = over else { return merged };

    if over.method.is_some() {
        merged.method = over.method;
    }
    if over.data.is_some() {
        merged.data.clone_from(&over.data);
    }
    if over.json.is_some() {
        merged.json.clone_from(&over.json);
    }
    if over.headers.is_some() {
        merged.headers.clone_from(&over.headers);
    }
    if over.extra_headers.is_some() {
        merged.extra_headers.clone_from(&over.extra_headers);
    }
    if over.proxy.is_some() {
        merged.proxy.clone_from(&over.proxy);
    }
    if over.proxies.is_some() {
        merged.proxies.clone_from(&over.proxies);
    }
    for (key, value) in &over.extra {
        merged.extra.insert(key.clone(), value.clone());
    }
    merged
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// A user-written spider.
///
/// `ARCHITECTURE.md` §5.5. Every method has the default body Python's base class has, so a
/// minimal spider implements three things: [`Spider::name`], [`Spider::settings`] and
/// [`Spider::parse`].
///
/// The trait is object-safe on purpose — the engine holds an `Arc<dyn Spider>` — which is
/// why the two stream-producing methods take `self: Arc<Self>`: the returned stream is
/// `'static` and usually needs to keep the spider alive.
#[async_trait]
pub trait Spider: Send + Sync + 'static {
    /// The spider's name. Used for the dev-cache directory and the log span.
    ///
    /// Must not be empty: [`SpiderRunner::new`] rejects an empty name with
    /// [`CrawlError::MissingName`], which is Python's `ValueError(f"{cls} must have a
    /// name.")`.
    fn name(&self) -> &str;

    /// Every class attribute Python would let a subclass override.
    fn settings(&self) -> &SpiderSettings;

    /// Register the sessions requests can be routed to.
    ///
    /// **Synchronous**, like Python's, and run exactly once per [`SpiderRunner`]. The
    /// default registers one [`HttpCrawlSession`] under `"default"`, mirroring
    /// `manager.add("default", FetcherSession())`.
    ///
    /// # Errors
    ///
    /// Anything the transport or [`SessionManager::add`] returns; the runner wraps it in
    /// [`CrawlError::SessionConfiguration`].
    fn configure_sessions(&self, manager: &mut SessionManager) -> Result<(), CrawlError> {
        let session = HttpCrawlSession::new("default")?;
        manager.add("default", Arc::new(session), AddOptions::DEFAULT)?;
        Ok(())
    }

    /// The seed requests.
    ///
    /// The default yields one `GET` per entry of [`SpiderSettings::start_urls`] and errors
    /// when there are none, exactly like Python's.
    ///
    /// **Deviation, immaterial:** Python stamps the manager's default session id here;
    /// the port leaves `sid` empty and lets the engine's `_normalize_request` fill it in
    /// before the fingerprint is computed, which is the same fingerprint and the same
    /// routing.
    ///
    /// # Errors
    ///
    /// [`CrawlError::NoStartingPoint`] when `start_urls` is empty and this is not
    /// overridden.
    async fn start_requests(self: Arc<Self>) -> Result<RequestStream, CrawlError> {
        let urls = self.settings().start_urls.clone();
        if urls.is_empty() {
            return Err(CrawlError::NoStartingPoint);
        }
        Ok(Box::pin(futures::stream::iter(
            urls.into_iter().map(Request::new),
        )))
    }

    /// The default callback. Every response whose request names no callback lands here.
    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream;

    /// Route a response to the callback the request names.
    ///
    /// Replaces `getattr(spider, request._callback_name)`. The default ignores the name
    /// and calls [`Spider::parse`], which is the `... or spider.parse` half of Python's
    /// lookup; a spider with several callbacks overrides this with a `match`.
    async fn dispatch(
        self: Arc<Self>,
        _callback: CallbackId,
        response: CrawlResponse,
    ) -> CallbackStream {
        self.parse(response).await
    }

    /// Whether `callback` names a real callback on this spider.
    ///
    /// Used only when a checkpoint is restored: an unknown name is rewritten to
    /// [`CallbackId::PARSE`], reproducing `getattr(spider, name, None) or spider.parse`.
    /// A spider that overrides [`Spider::dispatch`] should override this too.
    fn has_callback(&self, callback: &CallbackId) -> bool {
        callback.as_str() == CallbackId::PARSE.as_str()
    }

    /// Called once before the loop, after the sessions are open and the stats seeded.
    async fn on_start(&self, resuming: bool) {
        if resuming {
            tracing::debug!("Resuming spider from checkpoint");
        } else {
            tracing::debug!("Starting spider");
        }
    }

    /// Called once when the crawl ends — completed, paused or errored.
    async fn on_close(&self) {
        tracing::debug!("Spider closed");
    }

    /// Called for a fetch failure **and** for a callback failure. Never fails the crawl.
    async fn on_error(&self, _request: &Request, _error: &CrawlError) {}

    /// The item pipeline. Return `None` — or an **empty** item — to drop it.
    async fn on_scraped_item(&self, item: Item) -> Option<Item> {
        Some(item)
    }

    /// Whether the response means "we got blocked". Defaults to [`BLOCKED_CODES`].
    async fn is_blocked(&self, response: &CrawlResponse) -> bool {
        is_blocked_status(response.status)
    }

    /// Prepare the already-built retry copy of a blocked request.
    ///
    /// The engine has already incremented `retry_count`, decremented `priority`, set
    /// `dont_filter` and stripped the proxy kwargs; this may adjust or replace it.
    async fn retry_blocked_request(&self, request: Request, _response: &CrawlResponse) -> Request {
        request
    }
}

// ---------------------------------------------------------------------------
// The HTTP session adapter
// ---------------------------------------------------------------------------

/// Unit 12's pooled [`AsyncFetcherSession`] behind unit 15's [`CrawlSession`] trait.
///
/// This is the `FetcherSession` Python's default `configure_sessions` registers. It is
/// mechanical: the only interesting part is that `kwargs` is **borrowed** and never
/// mutated, which is the invariant that stops a retried `POST` from decaying into a `GET`.
///
/// ```no_run
/// use std::sync::Arc;
/// use browser_spider::session::{AddOptions, SessionManager};
/// use browser_spider::spider::HttpCrawlSession;
///
/// # fn main() -> Result<(), browser_spider::spider::CrawlError> {
/// let mut manager = SessionManager::new();
/// manager.add("http", Arc::new(HttpCrawlSession::new("http")?), AddOptions::DEFAULT)?;
/// # Ok(())
/// # }
/// ```
pub struct HttpCrawlSession {
    sid: String,
    inner: AsyncFetcherSession,
}

impl std::fmt::Debug for HttpCrawlSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpCrawlSession")
            .field("sid", &self.sid)
            .finish_non_exhaustive()
    }
}

impl HttpCrawlSession {
    /// A session over the default backend, labelled `sid` in error messages.
    ///
    /// # Errors
    ///
    /// [`CrawlError::SessionConfiguration`] when the transport cannot be built.
    pub fn new(sid: impl Into<String>) -> Result<Self, CrawlError> {
        let sid = sid.into();
        let inner =
            AsyncFetcherSession::new().map_err(|error| CrawlError::SessionConfiguration {
                spider: sid.clone(),
                message: error.to_string(),
            })?;
        Ok(Self { sid, inner })
    }

    /// Wrap a session the caller configured (impersonation, proxies, retries, …).
    #[must_use]
    pub fn with_session(sid: impl Into<String>, session: AsyncFetcherSession) -> Self {
        Self {
            sid: sid.into(),
            inner: session,
        }
    }

    /// The session id this adapter reports in errors.
    #[must_use]
    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// The wrapped transport session, for cookie-jar access and configuration reads.
    #[must_use]
    pub fn session(&self) -> &AsyncFetcherSession {
        &self.inner
    }
}

/// The `proxies={"http": …, "https": …}` map, as unit 12's per-scheme table.
fn proxy_spec(proxies: &BTreeMap<String, String>) -> ProxySpec {
    let mut spec = ProxySpec::default();
    for (scheme, url) in proxies {
        match scheme.to_ascii_lowercase().as_str() {
            "all" => spec.all = Some(url.clone()),
            "http" => spec.http = Some(url.clone()),
            "https" => spec.https = Some(url.clone()),
            "ws" => spec.ws = Some(url.clone()),
            "wss" => spec.wss = Some(url.clone()),
            other => tracing::debug!("Ignoring unknown proxy scheme {other:?}"),
        }
    }
    spec
}

#[async_trait]
impl CrawlSession for HttpCrawlSession {
    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    async fn open(&self) -> SpiderResult<()> {
        self.inner
            .open()
            .await
            .map_err(|error| SpiderError::SessionLifecycle {
                sid: self.sid.clone(),
                action: "open",
                message: error.to_string(),
            })
    }

    async fn close(&self) -> SpiderResult<()> {
        self.inner
            .close()
            .await
            .map_err(|error| SpiderError::SessionLifecycle {
                sid: self.sid.clone(),
                action: "close",
                message: error.to_string(),
            })
    }

    async fn fetch(&self, url: &str, kwargs: &SessionKwargs) -> SpiderResult<FetchedResponse> {
        let mut builder = self.inner.request(kwargs.method().into(), url);

        if let Some(headers) = kwargs.effective_headers() {
            builder = builder.headers(headers.clone());
        }
        // `data=` wins over `json=`, matching `update_fingerprint`'s precedence, and an
        // empty `BytesIO` still counts as a body (`RequestBody::is_falsy`).
        match (&kwargs.data, &kwargs.json) {
            (Some(data), _) if !data.is_falsy() => {
                builder = builder.body(match data {
                    RequestBody::Form(pairs) => Body::Form(pairs.clone()),
                    RequestBody::Text(text) => Body::Text(text.clone()),
                    RequestBody::Bytes(bytes) | RequestBody::Io(bytes) => {
                        Body::Raw(Bytes::from(bytes.clone()))
                    }
                });
            }
            (_, Some(json)) => builder = builder.json(json.clone()),
            _ => {}
        }
        if let Some(proxy) = &kwargs.proxy {
            builder = builder.proxy(Some(proxy.clone()));
        }
        if let Some(proxies) = &kwargs.proxies {
            builder = builder.proxies(proxy_spec(proxies));
        }

        // The opaque tail. Four keys have a typed home in `RequestOverrides` and would
        // otherwise be forwarded as raw JSON the backend ignores; everything else really
        // is pass-through.
        for (key, value) in &kwargs.extra {
            builder = match (key.to_ascii_lowercase().as_str(), value) {
                ("timeout", Value::Null) => builder.timeout(None),
                ("timeout", value) => match value.as_f64() {
                    Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                        builder.timeout(Some(Duration::from_secs_f64(seconds)))
                    }
                    _ => builder.extra(key.clone(), value.clone()),
                },
                ("retries", Value::Null) => builder.retries(None),
                ("retries", value) => match value.as_i64() {
                    Some(retries) => builder.retries(Some(clamp_i32(retries))),
                    None => builder.extra(key.clone(), value.clone()),
                },
                ("max_redirects", value) => match value.as_i64() {
                    Some(limit) => builder.max_redirects(clamp_i32(limit)),
                    None => builder.extra(key.clone(), value.clone()),
                },
                ("verify", Value::Bool(verify)) => builder.verify(*verify),
                _ => builder.extra(key.clone(), value.clone()),
            };
        }

        let response = builder.send().await.map_err(|error| SpiderError::Fetch {
            sid: self.sid.clone(),
            url: url.to_owned(),
            message: error.to_string(),
        })?;
        Ok(FetchedResponse::from(&response))
    }
}

/// Saturating `i64` → `i32`, so a hand-written `retries: 99999999999` is a big number
/// rather than a wrapped negative one.
fn clamp_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

// ---------------------------------------------------------------------------
// Running a spider
// ---------------------------------------------------------------------------

/// What `Spider.__init__(crawldir, interval)` took, plus the two things `tracing` and the
/// parser need that Python resolved through globals.
#[derive(Clone)]
pub struct RunOptions {
    /// Checkpoint directory. `None` disables the checkpoint system entirely.
    pub crawldir: Option<PathBuf>,
    /// Seconds between periodic checkpoint saves. `0` disables periodic saves but not the
    /// final one taken on pause.
    pub interval: f64,
    /// Watch for `Ctrl-C` in the crawl loop.
    ///
    /// Replaces Python's `signal.signal(SIGINT, …)` swap. `true` for
    /// [`SpiderRunner::run`], `false` for [`SpiderRunner::stream`] — Python's `stream()`
    /// installs no handler either.
    pub handle_ctrl_c: bool,
    /// The `tracing` layer that backs `CrawlStats.log_count`. Install the same
    /// [`LevelCounter`] on the subscriber and hand a clone here; without it `log_count`
    /// stays `{}`.
    pub log_counter: Option<LevelCounter>,
    /// `selector_config` for every response a callback receives.
    pub selector_config: SelectorConfig,
    /// The hook that injects adaptive storage into each parsed document.
    pub document_hook: Option<DocumentHook>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            crawldir: None,
            interval: crate::checkpoint::DEFAULT_INTERVAL_SECS,
            handle_ctrl_c: true,
            log_counter: None,
            selector_config: SelectorConfig::default(),
            document_hook: None,
        }
    }
}

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("crawldir", &self.crawldir)
            .field("interval", &self.interval)
            .field("handle_ctrl_c", &self.handle_ctrl_c)
            .field("log_counter", &self.log_counter.is_some())
            .field("selector_config", &self.selector_config)
            .field("document_hook", &self.document_hook.is_some())
            .finish()
    }
}

impl RunOptions {
    /// Builder: enable checkpointing into `crawldir`.
    #[must_use]
    pub fn with_crawldir(mut self, crawldir: impl Into<PathBuf>) -> Self {
        self.crawldir = Some(crawldir.into());
        self
    }

    /// Builder: the periodic checkpoint interval, in seconds.
    #[must_use]
    pub fn with_interval(mut self, seconds: f64) -> Self {
        self.interval = seconds;
        self
    }

    /// Builder: whether the loop watches for `Ctrl-C`.
    #[must_use]
    pub fn with_ctrl_c(mut self, handle: bool) -> Self {
        self.handle_ctrl_c = handle;
        self
    }

    /// Builder: the log-level counter feeding `CrawlStats.log_count`.
    #[must_use]
    pub fn with_log_counter(mut self, counter: LevelCounter) -> Self {
        self.log_counter = Some(counter);
        self
    }

    /// Builder: the parser configuration each response is built with.
    #[must_use]
    pub fn with_selector_config(mut self, config: SelectorConfig) -> Self {
        self.selector_config = config;
        self
    }

    /// Builder: the adaptive-storage document hook.
    #[must_use]
    pub fn with_document_hook(mut self, hook: DocumentHook) -> Self {
        self.document_hook = Some(hook);
        self
    }
}

/// Owns a spider, its sessions and the engine of the run currently in flight.
///
/// Python keeps this state on the `Spider` instance itself (`_session_manager`, `_engine`);
/// splitting it out keeps [`Spider`] a pure behaviour trait. The important half of the
/// original design survives: **one `SessionManager` for the lifetime of the runner** — so
/// [`Spider::configure_sessions`] runs once and connection pools are reused across runs —
/// and **a fresh [`CrawlerEngine`] per run**.
pub struct SpiderRunner {
    spider: Arc<dyn Spider>,
    sessions: Arc<SessionManager>,
    options: RunOptions,
    engine: Arc<Mutex<Option<Arc<CrawlerEngine>>>>,
}

/// Clears the runner's "engine in flight" slot when a streamed crawl ends, whether the
/// consumer drained the stream or dropped it.
struct EngineSlotGuard(Arc<Mutex<Option<Arc<CrawlerEngine>>>>);

impl Drop for EngineSlotGuard {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

impl std::fmt::Debug for SpiderRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpiderRunner")
            .field("spider", &self.spider.name())
            .field("sessions", &self.sessions.session_ids())
            .field("options", &self.options)
            .field("running", &self.engine.lock().is_some())
            .finish()
    }
}

impl SpiderRunner {
    /// Configure the spider's sessions and get ready to run.
    ///
    /// # Errors
    ///
    /// * [`CrawlError::MissingName`] when [`Spider::name`] is empty.
    /// * [`CrawlError::SessionConfiguration`] when [`Spider::configure_sessions`] fails.
    /// * [`CrawlError::NoSessionsConfigured`] when it registers nothing.
    pub fn new(spider: Arc<dyn Spider>, options: RunOptions) -> Result<Self, CrawlError> {
        if spider.name().is_empty() {
            return Err(CrawlError::MissingName);
        }
        let mut sessions = SessionManager::new();
        spider.configure_sessions(&mut sessions).map_err(|error| {
            CrawlError::SessionConfiguration {
                spider: spider.name().to_owned(),
                message: error.to_string(),
            }
        })?;
        if sessions.is_empty() {
            return Err(CrawlError::NoSessionsConfigured {
                spider: spider.name().to_owned(),
            });
        }
        tracing::info!("Spider initialized");
        Ok(Self {
            spider,
            sessions: Arc::new(sessions),
            options,
            engine: Arc::new(Mutex::new(None)),
        })
    }

    /// The spider being run.
    #[must_use]
    pub fn spider(&self) -> &Arc<dyn Spider> {
        &self.spider
    }

    /// The session manager `configure_sessions` filled in.
    #[must_use]
    pub fn sessions(&self) -> &Arc<SessionManager> {
        &self.sessions
    }

    /// The engine of the run in flight, if any.
    #[must_use]
    pub fn engine(&self) -> Option<Arc<CrawlerEngine>> {
        self.engine.lock().clone()
    }

    /// Run the crawl to completion.
    ///
    /// Python's `Spider.start()`, minus the event-loop construction: the caller already
    /// has a runtime. [`SpiderRunner::start_blocking`] is the version that does not.
    ///
    /// # Errors
    ///
    /// Whatever the crawl fails with — a `start_requests` error, a checkpoint save that
    /// could not be written, or a session that refused to open or close.
    pub async fn run(&self) -> Result<CrawlResult, CrawlError> {
        let engine = Arc::new(CrawlerEngine::new(
            Arc::clone(&self.spider),
            Arc::clone(&self.sessions),
            &self.options,
        )?);
        *self.engine.lock() = Some(Arc::clone(&engine));
        let outcome = Arc::clone(&engine).crawl().await;
        *self.engine.lock() = None;
        let stats = outcome?;
        Ok(CrawlResult {
            stats,
            items: engine.items(),
            paused: engine.paused(),
        })
    }

    /// Stream items as they are scraped.
    ///
    /// Python's `Spider.stream()`. `Ctrl-C` handling is **off** for the duration, exactly
    /// as there, and [`CrawlerEngine::items`] stays empty because every item goes to the
    /// channel instead. The channel holds [`STREAM_BUFFER`] items, so a slow consumer
    /// slows the crawl rather than growing memory.
    ///
    /// Read [`SpiderRunner::stats`] during iteration for live statistics.
    ///
    /// # Errors
    ///
    /// As [`CrawlerEngine::new`]; failures *during* the crawl are logged and end the
    /// stream, because a `Stream<Item = Item>` has nowhere to put them.
    pub fn stream(&self) -> Result<impl futures::Stream<Item = Item> + Send + 'static, CrawlError> {
        let mut options = self.options.clone();
        options.handle_ctrl_c = false;
        let engine = Arc::new(CrawlerEngine::new(
            Arc::clone(&self.spider),
            Arc::clone(&self.sessions),
            &options,
        )?);
        *self.engine.lock() = Some(Arc::clone(&engine));
        // `stream` takes the `Arc` by value; the slot above keeps the engine reachable
        // for `pause()` and `stats()` while the stream is being consumed, and the guard
        // clears it again however the stream ends — drained or abandoned.
        let guard = EngineSlotGuard(Arc::clone(&self.engine));
        let inner = engine.stream();
        Ok(async_stream::stream! {
            let _guard = guard;
            futures::pin_mut!(inner);
            while let Some(item) = inner.next().await {
                yield item;
            }
        })
    }

    /// Ask the running crawl to pause.
    ///
    /// First call: graceful pause (waits for in-flight requests). Second: force stop.
    /// Third and later: no-op.
    ///
    /// # Errors
    ///
    /// [`CrawlError::NoActiveCrawl`] when nothing is running — Python's
    /// `RuntimeError("No active crawl to stop")`.
    pub fn pause(&self) -> Result<(), CrawlError> {
        match self.engine() {
            Some(engine) => {
                engine.request_pause();
                Ok(())
            }
            None => Err(CrawlError::NoActiveCrawl),
        }
    }

    /// The live statistics of the run in flight.
    ///
    /// # Errors
    ///
    /// [`CrawlError::NoActiveCrawlStats`] when nothing is running.
    pub fn stats(&self) -> Result<Arc<CrawlStats>, CrawlError> {
        self.engine()
            .map(|engine| engine.stats())
            .ok_or(CrawlError::NoActiveCrawlStats)
    }

    /// [`SpiderRunner::run`] from synchronous code, on a runtime built for the call.
    ///
    /// Python's `Spider.start()` builds an event loop with `anyio.run`; this builds a
    /// **current-thread** `tokio` runtime, which is enough for an I/O-bound crawl and
    /// keeps the crate off `rt-multi-thread`. A caller who wants worker threads should
    /// build their own runtime and use [`SpiderRunner::run`].
    ///
    /// # Errors
    ///
    /// [`CrawlError::Runtime`] when a runtime is already running on this thread (calling
    /// `block_on` from inside one deadlocks) or cannot be built, plus whatever
    /// [`SpiderRunner::run`] returns.
    pub fn start_blocking(&self) -> Result<CrawlResult, CrawlError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(CrawlError::Runtime {
                message: "a tokio runtime is already running on this thread; use `run().await`"
                    .to_owned(),
            });
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| CrawlError::Runtime {
                message: error.to_string(),
            })?;
        runtime.block_on(self.run())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Minimal(SpiderSettings);

    #[async_trait]
    impl Spider for Minimal {
        fn name(&self) -> &str {
            "minimal"
        }
        fn settings(&self) -> &SpiderSettings {
            &self.0
        }
        fn configure_sessions(&self, _manager: &mut SessionManager) -> Result<(), CrawlError> {
            Ok(())
        }
        async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
            Box::pin(futures::stream::empty())
        }
    }

    fn minimal(settings: SpiderSettings) -> Arc<Minimal> {
        Arc::new(Minimal(settings))
    }

    /// The `markdown` feature has to reach **`browser-http`**, not only
    /// `browser-parser`: `Response::markdown()` is defined in unit 12's crate
    /// and [`CrawlResponse`] gets it through its `Deref`. This body does not
    /// compile unless `Cargo.toml` forwards the flag both ways.
    #[cfg(feature = "markdown")]
    #[test]
    fn the_markdown_feature_reaches_the_response_through_deref() {
        let fetched = FetchedResponse::new(200, "https://shop.test/", "<h1>Widget</h1>");
        let response = CrawlResponse::from_fetched(&fetched, &SelectorConfig::default(), None);
        assert!(response.markdown().contains("Widget"));
    }

    #[test]
    fn blocked_codes_match_python() {
        let mut sorted = BLOCKED_CODES;
        sorted.sort_unstable();
        assert_eq!(BLOCKED_CODES, sorted, "kept sorted so the scan is monotone");
        for status in [401, 403, 407, 429, 444, 500, 502, 503, 504] {
            assert!(is_blocked_status(status));
        }
        for status in [200, 201, 301, 404, 418, 451, 505] {
            assert!(!is_blocked_status(status));
        }
    }

    #[test]
    fn allowed_domains_matches_suffixes_only() {
        let settings = SpiderSettings::default().with_allowed_domains(["example.com", "b.test"]);
        assert!(settings.is_domain_allowed("example.com"));
        assert!(settings.is_domain_allowed("www.example.com"));
        assert!(settings.is_domain_allowed("deep.sub.example.com"));
        assert!(settings.is_domain_allowed("b.test"));
        assert!(!settings.is_domain_allowed("notexample.com"));
        assert!(!settings.is_domain_allowed("example.com.evil.test"));
        assert!(!settings.is_domain_allowed("example.com:8443"));
    }

    #[test]
    fn effective_concurrency_never_reaches_zero() {
        assert_eq!(SpiderSettings::default().effective_concurrency(), 4);
        let zero = SpiderSettings::default().with_concurrent_requests(0);
        assert_eq!(zero.effective_concurrency(), 1);
    }

    #[test]
    fn fingerprint_options_come_from_the_three_switches() {
        let settings = SpiderSettings {
            fp_include_kwargs: true,
            fp_keep_fragments: true,
            ..SpiderSettings::default()
        };
        let options = settings.fingerprint_options();
        assert!(options.include_kwargs);
        assert!(options.keep_fragments);
        assert!(!options.include_headers);
    }

    #[tokio::test]
    async fn default_start_requests_errors_without_seeds() {
        let spider = minimal(SpiderSettings::default());
        let error = match spider.start_requests().await {
            Err(error) => error,
            Ok(_) => panic!("a spider with no seeds must not produce a stream"),
        };
        assert_eq!(
            error.to_string(),
            "Spider has no starting point, either set `start_urls` or override `start_requests` function."
        );
    }

    #[tokio::test]
    async fn default_start_requests_yields_one_request_per_seed() {
        let spider = minimal(
            SpiderSettings::default().with_start_urls(["https://a.test/", "https://b.test/"]),
        );
        let requests: Vec<Request> = spider
            .start_requests()
            .await
            .expect("seeds")
            .collect()
            .await;
        assert_eq!(
            requests.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            ["https://a.test/", "https://b.test/"]
        );
        assert!(
            requests.iter().all(|r| r.sid.is_empty()),
            "the engine stamps the sid"
        );
    }

    #[tokio::test]
    async fn default_hooks_match_python() {
        let spider = minimal(SpiderSettings::default());
        let mut item = Item::new();
        item.insert("a".into(), json!(1));
        assert_eq!(spider.on_scraped_item(item.clone()).await, Some(item));

        let request = Request::new("https://a.test/");
        let kept = spider
            .retry_blocked_request(request.clone(), &blank_response(200))
            .await;
        assert_eq!(kept.url, request.url);

        assert!(spider.is_blocked(&blank_response(429)).await);
        assert!(!spider.is_blocked(&blank_response(200)).await);
        assert!(spider.has_callback(&CallbackId::PARSE));
        assert!(!spider.has_callback(&CallbackId::new("other")));
    }

    fn blank_response(status: u16) -> CrawlResponse {
        CrawlResponse::from_fetched(
            &FetchedResponse::new(status, "https://a.test/", "<p>x</p>"),
            &SelectorConfig::default(),
            None,
        )
    }

    fn response_with_request(request: Request) -> CrawlResponse {
        let mut fetched = FetchedResponse::new(
            200,
            "https://shop.test/catalog/page-2",
            "<a href=\"item?id=3\">go</a>",
        );
        fetched.meta.insert("depth".into(), json!(1));
        fetched.attach_request(&request);
        CrawlResponse::from_fetched(&fetched, &SelectorConfig::default(), None)
    }

    #[test]
    fn follow_without_a_request_is_an_error() {
        let response = blank_response(200);
        let error = response.follow("/next").expect_err("no request");
        assert_eq!(error.to_string(), "This response has no request set yet.");
    }

    #[test]
    fn follow_inherits_everything_but_dont_filter() {
        let mut kwargs = SessionKwargs::default();
        kwargs.extra.insert("timeout".into(), json!(5));
        let origin = Request::new("https://shop.test/catalog/page-2")
            .with_sid("http")
            .with_callback(CallbackId::new("parse_item"))
            .with_priority(7)
            .with_dont_filter(true)
            .with_session_kwargs(kwargs);
        let response = response_with_request(origin);

        let follow_up = response.follow("item?id=3").expect("a request");
        assert_eq!(follow_up.url, "https://shop.test/catalog/item?id=3");
        assert_eq!(follow_up.sid, "http");
        assert_eq!(
            follow_up.callback.as_ref().map(CallbackId::as_str),
            Some("parse_item")
        );
        assert_eq!(follow_up.priority, 7);
        assert!(!follow_up.dont_filter, "dont_filter is never inherited");
        assert_eq!(follow_up.meta["depth"], json!(1));
        assert_eq!(follow_up.session_kwargs.extra["timeout"], json!(5));
        // referer_flow, on by default.
        assert_eq!(
            follow_up
                .session_kwargs
                .headers
                .as_ref()
                .and_then(|h| h.get("referer")),
            Some(&"https://shop.test/catalog/page-2".to_owned())
        );
        assert_eq!(
            follow_up
                .session_kwargs
                .extra_headers
                .as_ref()
                .and_then(|h| h.get("referer")),
            Some(&"https://shop.test/catalog/page-2".to_owned())
        );
        assert_eq!(
            follow_up.session_kwargs.extra["google_search"],
            Value::Bool(false)
        );
    }

    #[test]
    fn follow_options_override_and_merge() {
        let origin = Request::new("https://shop.test/catalog/page-2")
            .with_sid("http")
            .with_priority(1);
        let response = response_with_request(origin);

        let mut meta = Item::new();
        meta.insert("depth".into(), json!(2));
        meta.insert("from".into(), json!("page-2"));
        let mut over = SessionKwargs::default();
        over.extra.insert("timeout".into(), json!(30));

        let follow_up = response
            .follow_with(
                "/other",
                &FollowOptions {
                    sid: "browser".to_owned(),
                    callback: Some(CallbackId::new("parse_other")),
                    priority: Some(-4),
                    dont_filter: true,
                    meta: Some(meta),
                    referer_flow: false,
                    session_kwargs: Some(over),
                },
            )
            .expect("a request");

        assert_eq!(follow_up.url, "https://shop.test/other");
        assert_eq!(follow_up.sid, "browser");
        assert_eq!(follow_up.priority, -4);
        assert!(follow_up.dont_filter);
        assert_eq!(follow_up.meta["depth"], json!(2), "the option wins");
        assert_eq!(follow_up.meta["from"], json!("page-2"));
        assert_eq!(follow_up.session_kwargs.extra["timeout"], json!(30));
        assert!(
            follow_up.session_kwargs.headers.is_none(),
            "referer_flow=false adds no headers"
        );
        assert!(!follow_up.session_kwargs.extra.contains_key("google_search"));
    }

    #[test]
    fn follow_does_not_mutate_the_originating_request() {
        let origin = Request::new("https://shop.test/catalog/page-2").with_sid("http");
        let response = response_with_request(origin);
        let _ = response.follow("/x").expect("a request");
        let request = response.request.as_ref().expect("attached");
        assert!(
            request.session_kwargs.headers.is_none(),
            "Python leaks the referer back into a shared dict; the port clones"
        );
    }

    #[test]
    fn merge_session_kwargs_prefers_the_override() {
        let mut base = SessionKwargs {
            method: Some(browser_core::types::HttpMethod::Post),
            ..SessionKwargs::default()
        };
        base.extra.insert("a".into(), json!(1));
        base.extra.insert("b".into(), json!(1));
        let mut over = SessionKwargs::default();
        over.extra.insert("b".into(), json!(2));

        let merged = merge_session_kwargs(&base, Some(&over));
        assert_eq!(merged.method, Some(browser_core::types::HttpMethod::Post));
        assert_eq!(merged.extra["a"], json!(1));
        assert_eq!(merged.extra["b"], json!(2));
        assert_eq!(merge_session_kwargs(&base, None).extra["b"], json!(1));
    }

    #[test]
    fn proxy_spec_maps_every_scheme() {
        let mut proxies = BTreeMap::new();
        proxies.insert("http".to_owned(), "http://p1".to_owned());
        proxies.insert("HTTPS".to_owned(), "http://p2".to_owned());
        proxies.insert("nope".to_owned(), "http://p3".to_owned());
        let spec = proxy_spec(&proxies);
        assert_eq!(spec.http.as_deref(), Some("http://p1"));
        assert_eq!(spec.https.as_deref(), Some("http://p2"));
        assert!(spec.all.is_none());
    }

    #[test]
    fn yielded_constructors_and_conversions() {
        let mut item = Item::new();
        item.insert("k".into(), json!("v"));
        assert!(matches!(Yielded::from(item.clone()), Yielded::Item(_)));
        assert!(matches!(
            Yielded::from(Request::new("https://a.test/")),
            Yielded::Request(_)
        ));
        assert!(matches!(Yielded::error("boom"), Yielded::Error(_)));
        match Yielded::error("boom") {
            Yielded::Error(error) => assert_eq!(error.to_string(), "boom"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn runner_rejects_a_nameless_spider() {
        struct Nameless;
        #[async_trait]
        impl Spider for Nameless {
            fn name(&self) -> &str {
                ""
            }
            fn settings(&self) -> &SpiderSettings {
                static SETTINGS: std::sync::OnceLock<SpiderSettings> = std::sync::OnceLock::new();
                SETTINGS.get_or_init(SpiderSettings::default)
            }
            async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
                Box::pin(futures::stream::empty())
            }
        }
        let error =
            SpiderRunner::new(Arc::new(Nameless), RunOptions::default()).expect_err("no name");
        assert_eq!(error.to_string(), "Spider must have a name.");
    }

    #[test]
    fn runner_rejects_a_spider_that_registers_nothing() {
        let error = SpiderRunner::new(minimal(SpiderSettings::default()), RunOptions::default())
            .expect_err("no sessions");
        assert_eq!(
            error.to_string(),
            "minimal.configure_sessions() did not add any sessions"
        );
    }

    #[test]
    fn runner_wraps_a_configure_sessions_failure() {
        struct Broken(SpiderSettings);
        #[async_trait]
        impl Spider for Broken {
            fn name(&self) -> &str {
                "broken"
            }
            fn settings(&self) -> &SpiderSettings {
                &self.0
            }
            fn configure_sessions(&self, _manager: &mut SessionManager) -> Result<(), CrawlError> {
                Err(CrawlError::callback("no transport here"))
            }
            async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
                Box::pin(futures::stream::empty())
            }
        }
        let error = SpiderRunner::new(
            Arc::new(Broken(SpiderSettings::default())),
            RunOptions::default(),
        )
        .expect_err("configure_sessions failed");
        assert_eq!(
            error.to_string(),
            "Error in broken.configure_sessions(): no transport here"
        );
    }

    #[test]
    fn pause_and_stats_need_a_running_crawl() {
        struct WithSession(SpiderSettings);
        #[async_trait]
        impl Spider for WithSession {
            fn name(&self) -> &str {
                "with-session"
            }
            fn settings(&self) -> &SpiderSettings {
                &self.0
            }
            fn configure_sessions(&self, manager: &mut SessionManager) -> Result<(), CrawlError> {
                manager.add("s", Arc::new(NoopSession), AddOptions::DEFAULT)?;
                Ok(())
            }
            async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
                Box::pin(futures::stream::empty())
            }
        }
        let runner = SpiderRunner::new(
            Arc::new(WithSession(SpiderSettings::default())),
            RunOptions::default(),
        )
        .expect("configures");
        assert_eq!(
            runner.pause().expect_err("idle").to_string(),
            "No active crawl to stop"
        );
        assert_eq!(
            runner.stats().expect_err("idle").to_string(),
            "No active crawl. Use this property inside `async for item in spider.stream():`"
        );
        assert!(runner.engine().is_none());
    }

    /// A session that answers nothing; enough to satisfy `configure_sessions`.
    #[derive(Debug)]
    pub(crate) struct NoopSession;

    #[async_trait]
    impl CrawlSession for NoopSession {
        fn is_alive(&self) -> bool {
            true
        }
        async fn open(&self) -> SpiderResult<()> {
            Ok(())
        }
        async fn close(&self) -> SpiderResult<()> {
            Ok(())
        }
        async fn fetch(&self, url: &str, _kwargs: &SessionKwargs) -> SpiderResult<FetchedResponse> {
            Ok(FetchedResponse::new(204, url, ""))
        }
    }

    // -- the HTTP session adapter -----------------------------------------

    fn mock_session(backend: Arc<browser_http::backend::mock::MockBackend>) -> HttpCrawlSession {
        let session = AsyncFetcherSession::builder()
            .backend(backend)
            .build()
            .expect("a session over the mock backend");
        HttpCrawlSession::with_session("http", session)
    }

    #[tokio::test]
    async fn the_http_adapter_maps_every_kwarg_it_knows() {
        use browser_http::backend::mock::{MockBackend, MockReply};
        use browser_http::backend::Method;

        let backend = Arc::new(MockBackend::always(
            MockReply::new(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .body("<p>ok</p>"),
        ));
        let session = mock_session(Arc::clone(&backend));
        session.open().await.expect("opens");

        let mut headers = HeaderDict::new();
        headers.insert("X-Test".to_owned(), "A".to_owned());
        let mut proxies = BTreeMap::new();
        proxies.insert("https".to_owned(), "http://scheme-proxy:3128".to_owned());
        let mut kwargs = SessionKwargs {
            method: Some(browser_core::types::HttpMethod::Post),
            headers: Some(headers),
            json: Some(json!({"a": 1})),
            proxies: Some(proxies),
            ..SessionKwargs::default()
        };
        kwargs.extra.insert("timeout".to_owned(), json!(2.5));
        kwargs.extra.insert("verify".to_owned(), json!(false));
        kwargs.extra.insert("retries".to_owned(), json!(1));
        kwargs
            .extra
            .insert("accept_encoding".to_owned(), json!("gzip"));

        let response = session
            .fetch("https://a.test/submit", &kwargs)
            .await
            .expect("the mock answers");
        assert_eq!(response.status, 200);

        let sent = backend.sent();
        let wire = sent.first().expect("one request");
        assert_eq!(wire.method, Method::Post);
        assert_eq!(wire.headers.get("X-Test").map(String::as_str), Some("A"));
        assert!(matches!(wire.body, Some(Body::Json(_))));
        assert_eq!(wire.timeout, Some(Duration::from_secs_f64(2.5)));
        assert!(!wire.verify);
        assert_eq!(
            wire.proxies.https.as_deref(),
            Some("http://scheme-proxy:3128")
        );
        assert_eq!(
            wire.extra.get("accept_encoding"),
            Some(&json!("gzip")),
            "an unknown kwarg is forwarded verbatim"
        );

        // The pinned invariant: a retried POST must not decay into a GET.
        assert_eq!(
            kwargs.method,
            Some(browser_core::types::HttpMethod::Post),
            "fetch borrows the kwargs and never mutates them"
        );
        session.close().await.expect("closes");
    }

    #[tokio::test]
    async fn the_http_adapter_prefers_a_truthy_data_body_over_json() {
        use browser_http::backend::mock::{MockBackend, MockReply};

        let backend = Arc::new(MockBackend::always(MockReply::new(200).body("ok")));
        let session = mock_session(Arc::clone(&backend));
        session.open().await.expect("opens");

        // A `GET` cannot carry a body — unit 12 drops it with a warning — so every case
        // here is a `POST`.
        let kwargs = SessionKwargs {
            method: Some(browser_core::types::HttpMethod::Post),
            data: Some(RequestBody::Form(vec![("q".to_owned(), "rust".to_owned())])),
            json: Some(json!({"ignored": true})),
            ..SessionKwargs::default()
        };
        session
            .fetch("https://a.test/", &kwargs)
            .await
            .expect("the mock answers");
        assert!(matches!(backend.sent()[0].body, Some(Body::Form(_))));

        // An empty `bytes` is falsy, so `json=` takes over — the same rule the
        // fingerprint uses.
        let kwargs = SessionKwargs {
            method: Some(browser_core::types::HttpMethod::Post),
            data: Some(RequestBody::Bytes(Vec::new())),
            json: Some(json!({"used": true})),
            ..SessionKwargs::default()
        };
        session
            .fetch("https://a.test/", &kwargs)
            .await
            .expect("the mock answers");
        assert!(matches!(backend.sent()[1].body, Some(Body::Json(_))));

        // An empty `BytesIO` is *truthy* and suppresses `json=`.
        let kwargs = SessionKwargs {
            method: Some(browser_core::types::HttpMethod::Post),
            data: Some(RequestBody::Io(Vec::new())),
            json: Some(json!({"ignored": true})),
            ..SessionKwargs::default()
        };
        session
            .fetch("https://a.test/", &kwargs)
            .await
            .expect("the mock answers");
        assert!(matches!(backend.sent()[2].body, Some(Body::Raw(_))));
    }

    #[tokio::test]
    async fn the_http_adapter_reports_a_transport_failure_as_a_spider_error() {
        use browser_http::backend::mock::MockBackend;
        use browser_http::error::TransportError;

        let backend = Arc::new(MockBackend::always_failing(TransportError::connect(
            "no route",
        )));
        let session = mock_session(backend);
        session.open().await.expect("opens");

        let error = session
            .fetch("https://a.test/", &SessionKwargs::default())
            .await
            .expect_err("the backend refuses");
        assert!(matches!(error, SpiderError::Fetch { .. }));
        assert!(error.to_string().contains("https://a.test/"));
    }

    #[test]
    fn start_blocking_refuses_to_nest_inside_a_runtime() {
        struct WithSession(SpiderSettings);
        #[async_trait]
        impl Spider for WithSession {
            fn name(&self) -> &str {
                "nested"
            }
            fn settings(&self) -> &SpiderSettings {
                &self.0
            }
            fn configure_sessions(&self, manager: &mut SessionManager) -> Result<(), CrawlError> {
                manager.add("s", Arc::new(NoopSession), AddOptions::DEFAULT)?;
                Ok(())
            }
            async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
                Box::pin(futures::stream::empty())
            }
        }
        let runner = SpiderRunner::new(
            Arc::new(WithSession(SpiderSettings::default())),
            RunOptions::default(),
        )
        .expect("configures");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let error = runner.start_blocking().expect_err("nested");
            assert!(error.to_string().contains("already running"));
        });
    }
}
