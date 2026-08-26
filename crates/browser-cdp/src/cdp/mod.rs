//! The CDP tab: [`CdpPage`], the per-request reset sequence, and the page
//! lifecycle Python gets from Playwright.
//!
//! # What lives here
//!
//! * [`CdpPage`] — one Chromium tab, with the Network/Page event pumps wired to
//!   [`networkidle::NetworkTracker`] and [`history::RequestLedger`], the
//!   `Fetch` interceptor of [`fetch_domain`], and an
//!   [`isolated_world::IsolatedWorlds`] cache for every script it runs.
//! * [`PageControl`] — the five operations Python's `_get_page` performs on a
//!   tab, as a trait, so the reset sequence whose **call counts** are pinned by
//!   `tests/fetchers/test_page_reuse.py` can be tested without a browser.
//! * [`reset_for_request`], [`get_page`] and [`release_page`] — the tab
//!   lifecycle of `browser.md` §4.4 / §4.5, generic over [`PageControl`].
//!
//! # Stealth
//!
//! Nothing in this module calls `Runtime.enable`, `Runtime.addBinding` or
//! `chromiumoxide`'s own `Page::evaluate` / `Page::content` (which use the main
//! world). Every script goes through [`isolated_world`]. See that module for
//! the reasoning and for the one upstream leak we cannot close.

pub mod fetch_domain;
pub mod history;
pub mod isolated_world;
pub mod networkidle;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use browser_core::types::{Proxy, SetCookieParam, WaitState};
use browser_http::config::HeaderDict;
use bytes::Bytes;
use chromiumoxide::cdp::browser_protocol::dom::{
    EnableParams as DomEnableParams, GetFrameOwnerParams, ResolveNodeParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams, SetLocaleOverrideParams,
    SetTimezoneOverrideParams, SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
    EventRequestWillBeSentExtraInfo, EventResponseReceived, EventResponseReceivedExtraInfo,
    GetResponseBodyParams, Headers as CdpHeaders, SetCookiesParams as NetworkSetCookiesParams,
    SetExtraHttpHeadersParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CloseParams, EventLifecycleEvent,
    EventNavigatedWithinDocument, FrameId, FrameTree, GetFrameTreeParams, NavigateParams,
    ReferrerPolicy,
};
use chromiumoxide::cdp::browser_protocol::storage::GetCookiesParams as StorageGetCookiesParams;
use chromiumoxide::Page;
use futures::StreamExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::error::{BrowserError, Result};
use crate::flags::{ContextOptions, Viewport};
use crate::pool::{PageInfo, PagePool, PageState};
use crate::route::RoutePolicy;

use self::fetch_domain::{FetchInterceptor, InterceptSettings, ProxyCredentials};
use self::history::{
    headers_from_json, Capture, RedirectResponse, RequestLedger, RequestStarted, ResponseArrived,
};
use self::isolated_world::{
    BoundingBox, IsolatedWorlds, BOUNDING_BOX_JS, IS_VISIBLE_JS, PAGE_CONTENT_JS,
};
use self::networkidle::{IdleOutcome, NetworkTracker, NETWORK_IDLE_QUIET_PERIOD};

/// Playwright's default viewport for a context that does not ask for one.
///
/// The reference implementation only sets `viewport` for stealth sessions (1920×1080); dynamic
/// sessions inherit Playwright's default, which this reproduces so a page's
/// media queries behave the same under both engines.
pub const DEFAULT_VIEWPORT: Viewport = Viewport {
    width: 1280,
    height: 720,
};

/// `_get_page_content`'s retry count — the Windows `page.content()` workaround
/// for [playwright#16108](https://github.com/microsoft/playwright/issues/16108).
pub const PAGE_CONTENT_MAX_RETRIES: u32 = 20;

/// The pause between those retries.
pub const PAGE_CONTENT_RETRY_DELAY: Duration = Duration::from_millis(500);

/// How long [`PageControl::is_closed`] and [`PageControl::close`] wait for the
/// browser to answer.
///
/// Both run on the teardown path of every request, so neither may inherit the
/// generous per-command budget: a browser that died mid-request would otherwise
/// stall the pool for two minutes instead of having its tab evicted.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`CdpPage::await_raw_headers`] waits for
/// `Network.responseReceivedExtraInfo`.
///
/// Generous, because the fallback is a `Response` with no `Set-Cookie`; short
/// enough that a Chrome that never sends the event cannot stall a request.
pub const RAW_HEADERS_TIMEOUT: Duration = Duration::from_secs(5);

/// How often that wait re-checks.
pub const RAW_HEADERS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How often [`CdpPage::wait_for_selector`] re-checks the DOM.
///
/// Playwright uses a `MutationObserver`; polling avoids injecting anything
/// persistent into the page and is indistinguishable at this granularity.
pub const SELECTOR_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Playwright's message when `Page.navigate` reports `isDownload`.
///
/// `crPage.js`: `throw new NavigationAbortedError(loaderId, 'Download is
/// starting')`. A download never commits a document, so waiting for `load`
/// would burn the whole navigation timeout instead of failing at once.
pub const DOWNLOAD_STARTING: &str = "Download is starting";

/// The CDP lifecycle-event name behind Playwright's `"load"` state.
pub const LIFECYCLE_LOAD: &str = "load";

/// The CDP lifecycle-event name behind Playwright's `"domcontentloaded"` state.
pub const LIFECYCLE_DOM_CONTENT_LOADED: &str = "DOMContentLoaded";

/// The CDP lifecycle event that starts a new document.
///
/// Chrome fires it before any other lifecycle event of the new document, which
/// makes it the reset point for both the fired-event set and the isolated-world
/// cache.
pub const LIFECYCLE_INIT: &str = "init";

// ---------------------------------------------------------------------------
// The reset sequence, as a trait
// ---------------------------------------------------------------------------

/// The operations `_get_page` performs on a tab before every request.
///
/// Extracted as a trait for one reason: `tests/fetchers/test_page_reuse.py`
/// pins **how many times** each of them is called on a reused tab, and pinning
/// that in Rust needs a countable test double. [`CdpPage`] is the real
/// implementation.
#[async_trait::async_trait]
pub trait PageControl: Send + Sync {
    /// `page.set_default_navigation_timeout(timeout)`.
    ///
    /// # Errors
    ///
    /// Implementation-defined; [`CdpPage`] cannot fail here because Playwright
    /// keeps these client-side too.
    async fn set_default_navigation_timeout(&self, timeout_ms: f64) -> Result<()>;

    /// `page.set_default_timeout(timeout)`.
    ///
    /// # Errors
    ///
    /// Implementation-defined.
    async fn set_default_timeout(&self, timeout_ms: f64) -> Result<()>;

    /// `page.set_extra_http_headers(extra_headers or {})`.
    ///
    /// The empty map is meaningful: it *clears* the previous request's headers.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Network.setExtraHTTPHeaders` fails.
    async fn set_extra_http_headers(&self, headers: &HeaderDict) -> Result<()>;

    /// `page.unroute_all(behavior="ignoreErrors")`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Fetch.disable` fails.
    async fn unroute_all(&self) -> Result<()>;

    /// `page.route("**/*", create_intercept_handler(...))`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Fetch.enable` fails.
    async fn route(&self, policy: RoutePolicy) -> Result<()>;

    /// `page.is_closed()`.
    async fn is_closed(&self) -> bool;

    /// `page.close()`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`]; every caller suppresses it, as Python does.
    async fn close(&self) -> Result<()>;
}

/// The per-request reset of `browser.md` §4.4, in Python's exact order.
///
/// ```text
/// page.set_default_navigation_timeout(timeout)
/// page.set_default_timeout(timeout)
/// page.set_extra_http_headers(extra_headers or {})
/// page.unroute_all(behavior="ignoreErrors")
/// if disable_resources or blocked_domains:
///     page.route("**/*", handler)
/// ```
///
/// The order is load-bearing: `unroute_all` before `route` is what stops the
/// previous request's blocklist from surviving into this one, and the `{}` in
/// step three is what stops its headers from surviving.
///
/// # Errors
///
/// Whatever the [`PageControl`] returns.
pub async fn reset_for_request<P>(
    page: &P,
    timeout_ms: f64,
    extra_headers: Option<&HeaderDict>,
    policy: &RoutePolicy,
) -> Result<()>
where
    P: PageControl + ?Sized,
{
    page.set_default_navigation_timeout(timeout_ms).await?;
    page.set_default_timeout(timeout_ms).await?;
    let empty = HeaderDict::new();
    page.set_extra_http_headers(extra_headers.unwrap_or(&empty))
        .await?;
    page.unroute_all().await?;
    if policy.needs_interception() {
        page.route(policy.clone()).await?;
    }
    Ok(())
}

/// `_get_page`: take a ready tab or open one, then apply [`reset_for_request`].
///
/// `fresh_context` is Python's `context is not None` — in rotation mode the tab
/// belongs to a throw-away context, so the pool is never consulted for a
/// reusable one.
///
/// The at-capacity wait is [`PagePool::wait_for_ready_page`], which keeps
/// Python's 60 s budget and 50 ms poll and its
/// `TimeoutError("No pages finished …")` message.
///
/// # Errors
///
/// [`crate::error::PoolError`] when the pool is full and stays full, or
/// whatever `new_page` and the reset return.
pub async fn get_page<P, N, Fut>(
    pool: &PagePool<P>,
    max_wait: Duration,
    fresh_context: bool,
    new_page: N,
    timeout_ms: f64,
    extra_headers: Option<&HeaderDict>,
    policy: &RoutePolicy,
) -> Result<PageInfo<P>>
where
    P: PageControl + Clone,
    N: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<P>>,
{
    let mut page_info = if fresh_context {
        None
    } else {
        pool.get_ready_page()
    };
    if page_info.is_none() && !fresh_context && pool.pages_count() >= pool.max_pages() {
        page_info = Some(pool.wait_for_ready_page(max_wait).await?);
    }
    let page_info = match page_info {
        Some(info) => info,
        None => pool.add_page(new_page().await?)?,
    };

    reset_for_request(page_info.page(), timeout_ms, extra_headers, policy).await?;
    Ok(page_info)
}

/// The `finally:` half of `_page_generator`.
///
/// * rotation mode (`fresh_context`): the tab always leaves the pool, because
///   its context is about to be disposed;
/// * persistent mode: an errored or browser-closed tab is closed and evicted,
///   anything else is marked ready for the next request.
///
/// Errors from `close()` are suppressed, matching `with suppress(Exception)`.
pub async fn release_page<P>(pool: &PagePool<P>, page_info: &PageInfo<P>, fresh_context: bool)
where
    P: PageControl + Clone,
{
    if fresh_context {
        pool.remove_page(page_info);
        return;
    }
    if page_info.state() == PageState::Error || page_info.page().is_closed().await {
        if let Err(error) = page_info.page().close().await {
            tracing::debug!("Ignoring error while closing an evicted tab: {error}");
        }
        pool.remove_page(page_info);
    } else {
        page_info.mark_ready();
    }
}

// ---------------------------------------------------------------------------
// Lifecycle tracking
// ---------------------------------------------------------------------------

/// Which `Page.lifecycleEvent`s have fired for each frame's current document.
///
/// Playwright keeps the same set per `Frame` and clears it on `init`; that is
/// what makes `wait_for_load_state("load")` return immediately for a page that
/// already loaded, and block for one that has just started navigating.
#[derive(Debug, Default)]
pub struct LifecycleTracker {
    fired: Mutex<HashMap<FrameId, HashSet<String>>>,
    notify: Notify,
}

impl LifecycleTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one `Page.lifecycleEvent`.
    ///
    /// Returns `true` when the event started a new document, so the caller can
    /// drop that frame's isolated world.
    pub fn on_event(&self, frame: &FrameId, name: &str) -> bool {
        let is_init = name == LIFECYCLE_INIT;
        {
            let mut fired = self.fired.lock();
            let entry = fired.entry(frame.clone()).or_default();
            if is_init {
                entry.clear();
            }
            entry.insert(name.to_owned());
        }
        self.notify.notify_waiters();
        is_init
    }

    /// Whether `name` has already fired for `frame`'s current document.
    #[must_use]
    pub fn has_fired(&self, frame: &FrameId, name: &str) -> bool {
        self.fired
            .lock()
            .get(frame)
            .is_some_and(|names| names.contains(name))
    }

    /// Forget everything about `frame`, as `page.goto` does before navigating.
    pub fn reset_frame(&self, frame: &FrameId) {
        let _ = self.take_frame(frame);
    }

    /// [`LifecycleTracker::reset_frame`], handing back what was forgotten.
    ///
    /// `page.goto` clears the frame before it navigates, so that a wait for
    /// `load` cannot be satisfied by the *previous* document's `load` before
    /// Chrome gets round to firing `init` for the new one. A navigation that
    /// turns out to be same-document loads no new document at all, so what was
    /// cleared has to go back — see [`CdpPage::goto`].
    pub fn take_frame(&self, frame: &FrameId) -> HashSet<String> {
        let taken = self.fired.lock().remove(frame).unwrap_or_default();
        self.notify.notify_waiters();
        taken
    }

    /// Put a [`LifecycleTracker::take_frame`] snapshot back.
    pub fn restore_frame(&self, frame: &FrameId, fired: HashSet<String>) {
        self.fired
            .lock()
            .entry(frame.clone())
            .or_default()
            .extend(fired);
        self.notify.notify_waiters();
    }

    /// Block until `name` has fired for `frame`, or `budget` runs out.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Timeout`], whose text mirrors Playwright's
    /// `Timeout 30000ms exceeded.`.
    pub async fn wait_for(&self, frame: &FrameId, name: &str, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.has_fired(frame, name) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(BrowserError::Timeout {
                    operation: format!("the {name:?} load state"),
                    timeout_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                });
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Emulation
// ---------------------------------------------------------------------------

/// The Playwright context options that become per-page CDP overrides.
///
/// `launch_persistent_context(**context_options)` applies these at launch;
/// driving CDP there is no context object, so they are applied to each tab.
/// The mapping is the one `ARCHITECTURE.md` §13.4 note 3 lists.
#[derive(Clone, Debug, PartialEq)]
pub struct PageEmulation {
    /// Viewport size — 1920×1080 for stealth sessions, [`DEFAULT_VIEWPORT`]
    /// otherwise.
    pub viewport: Viewport,
    /// `device_scale_factor`, always `2`.
    pub device_scale_factor: f64,
    /// `color_scheme`, always `"dark"` — it defeats creepjs's
    /// `prefersLightColor` check.
    pub color_scheme: &'static str,
    /// `is_mobile`.
    pub is_mobile: bool,
    /// `timezone_id`; the empty string means "leave the system timezone".
    pub timezone_id: Option<String>,
    /// `locale`, set only for remote browsers (a local one gets `--lang`).
    pub locale: Option<String>,
    /// The User-Agent override, per `browser.md` §3.2.
    pub user_agent: Option<String>,
    /// `extra_http_headers`.
    pub extra_http_headers: Option<HeaderDict>,
}

impl PageEmulation {
    /// Read the emulation out of the context options unit 13 assembled.
    #[must_use]
    pub fn from_context_options(options: &ContextOptions) -> Self {
        Self {
            viewport: options
                .stealth
                .map_or(DEFAULT_VIEWPORT, |stealth| stealth.viewport),
            device_scale_factor: options.device_scale_factor,
            color_scheme: options.color_scheme,
            is_mobile: options.stealth.is_some_and(|stealth| stealth.is_mobile),
            // `timezone_id` defaults to `""`, which Playwright treats as "no
            // override" — `Emulation.setTimezoneOverride("")` would be an
            // error, so the empty string is dropped here.
            timezone_id: options.timezone_id.clone().filter(|tz| !tz.is_empty()),
            locale: options.locale.clone(),
            user_agent: options.user_agent.clone(),
            extra_http_headers: options.extra_http_headers.clone(),
        }
    }

    /// `Accept-Language` for the User-Agent override, derived from the locale.
    ///
    /// Mirrors `--accept-lang=<locale>,<base>`: a remote browser cannot take the
    /// launch flag, so the same value rides along with the UA override.
    #[must_use]
    pub fn accept_language(&self) -> Option<String> {
        let locale = self.locale.as_deref()?;
        let base = locale.split('-').next().unwrap_or(locale).to_lowercase();
        Some(if base == locale.to_lowercase() {
            locale.to_owned()
        } else {
            format!("{locale},{base}")
        })
    }
}

// ---------------------------------------------------------------------------
// CdpPage
// ---------------------------------------------------------------------------

/// The client-side timeouts Playwright keeps per page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeouts {
    /// `set_default_navigation_timeout`, in milliseconds.
    pub navigation_ms: f64,
    /// `set_default_timeout`, in milliseconds.
    pub default_ms: f64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            navigation_ms: crate::config::DEFAULT_TIMEOUT_MS,
            default_ms: crate::config::DEFAULT_TIMEOUT_MS,
        }
    }
}

#[derive(Debug)]
struct PageInner {
    page: Page,
    main_frame: FrameId,
    net: Arc<NetworkTracker>,
    ledger: Arc<RequestLedger>,
    lifecycle: Arc<LifecycleTracker>,
    worlds: Arc<IsolatedWorlds>,
    interceptor: FetchInterceptor,
    timeouts: Mutex<Timeouts>,
    /// The headers `page.set_extra_http_headers` last installed.
    ///
    /// Kept because `page.goto` needs them: Playwright's `gotoImpl` reads
    /// `this._page.extraHTTPHeaders()` and lifts any `referer` out of them
    /// into `Page.navigate`'s own `referrer` parameter. See [`CdpPage::goto`].
    extra_headers: Mutex<HeaderDict>,
    same_document: Arc<SameDocumentSignal>,
    closed: AtomicBool,
    dom_enabled: AtomicBool,
    pumps: Vec<JoinHandle<()>>,
    #[cfg(test)]
    counts: ControlCounts,
}

/// How many times each [`PageControl`] operation ran on one real tab.
///
/// `tests/fetchers/test_page_reuse.py` pins these counts against mocked
/// Playwright objects; the trait double in this module's tests reproduces that
/// pinning without a browser. This counter exists so the *same* counts can be
/// asserted against the tab a live request actually drove — the trait double
/// proves the algorithm, this proves the wiring.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct ControlCounts {
    navigation_timeout: std::sync::atomic::AtomicUsize,
    default_timeout: std::sync::atomic::AtomicUsize,
    extra_headers: std::sync::atomic::AtomicUsize,
    unroute_all: std::sync::atomic::AtomicUsize,
    route: std::sync::atomic::AtomicUsize,
    is_closed: std::sync::atomic::AtomicUsize,
    close: std::sync::atomic::AtomicUsize,
}

/// A snapshot of [`ControlCounts`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ControlCountsSnapshot {
    pub navigation_timeout: usize,
    pub default_timeout: usize,
    pub extra_headers: usize,
    pub unroute_all: usize,
    pub route: usize,
    pub is_closed: usize,
    pub close: usize,
}

#[cfg(test)]
impl ControlCounts {
    fn bump(counter: &std::sync::atomic::AtomicUsize) {
        counter.fetch_add(1, Ordering::SeqCst);
    }

    fn snapshot(&self) -> ControlCountsSnapshot {
        let read = |counter: &std::sync::atomic::AtomicUsize| counter.load(Ordering::SeqCst);
        ControlCountsSnapshot {
            navigation_timeout: read(&self.navigation_timeout),
            default_timeout: read(&self.default_timeout),
            extra_headers: read(&self.extra_headers),
            unroute_all: read(&self.unroute_all),
            route: read(&self.route),
            is_closed: read(&self.is_closed),
            close: read(&self.close),
        }
    }
}

impl Drop for PageInner {
    fn drop(&mut self) {
        for pump in &self.pumps {
            pump.abort();
        }
    }
}

/// One Chromium tab, with everything a reference implementation request needs bolted to it.
///
/// Cloning is cheap and shares state — the [`crate::pool::PagePool`] holds one
/// clone and the request holds another, exactly as Python hands out the same
/// `Page` object.
#[derive(Clone, Debug)]
pub struct CdpPage {
    inner: Arc<PageInner>,
}

impl CdpPage {
    /// Wire the event pumps and the `Fetch` interceptor to a freshly opened tab.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when the frame tree cannot be read or a listener
    /// cannot be registered.
    pub async fn attach(page: Page) -> Result<Self> {
        let tree = page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.getFrameTree".to_owned(),
                message: error.to_string(),
            })?;
        let main_frame = tree.result.frame_tree.frame.id.clone();

        let net = NetworkTracker::shared();
        let ledger = Arc::new(RequestLedger::new());
        ledger.set_main_frame(main_frame.inner().clone());
        let lifecycle = Arc::new(LifecycleTracker::new());
        let worlds = Arc::new(IsolatedWorlds::new());
        let interceptor = FetchInterceptor::install(&page).await?;

        let same_document = Arc::new(SameDocumentSignal::default());

        let mut pumps = Vec::with_capacity(7);
        pumps.push(spawn_same_document_pump(&page, &same_document, &main_frame).await?);
        pumps.push(spawn_request_pump(&page, &net, &ledger).await?);
        pumps.push(spawn_response_pump(&page, &ledger).await?);
        pumps.push(spawn_extra_info_pumps(&page, &ledger).await?);
        pumps.push(spawn_finished_pump(&page, &net).await?);
        pumps.push(spawn_failed_pump(&page, &net).await?);
        pumps.push(spawn_lifecycle_pump(&page, &lifecycle, &worlds, &net, &main_frame).await?);

        Ok(Self {
            inner: Arc::new(PageInner {
                page,
                main_frame,
                net,
                ledger,
                lifecycle,
                worlds,
                interceptor,
                timeouts: Mutex::new(Timeouts::default()),
                extra_headers: Mutex::new(HeaderDict::new()),
                same_document,
                closed: AtomicBool::new(false),
                dom_enabled: AtomicBool::new(false),
                pumps,
                #[cfg(test)]
                counts: ControlCounts::default(),
            }),
        })
    }

    /// How many times each [`PageControl`] operation has run on this tab.
    #[cfg(test)]
    pub(crate) fn control_counts(&self) -> ControlCountsSnapshot {
        self.inner.counts.snapshot()
    }

    /// The underlying `chromiumoxide` page, for callers that need raw CDP.
    #[must_use]
    pub fn raw(&self) -> &Page {
        &self.inner.page
    }

    /// The main frame's id.
    #[must_use]
    pub fn main_frame(&self) -> &FrameId {
        &self.inner.main_frame
    }

    /// The in-flight-request tracker behind `networkidle`.
    #[must_use]
    pub fn network(&self) -> &Arc<NetworkTracker> {
        &self.inner.net
    }

    /// The response-capture ledger.
    #[must_use]
    pub fn ledger(&self) -> &Arc<RequestLedger> {
        &self.inner.ledger
    }

    /// The lifecycle-event tracker.
    #[must_use]
    pub fn lifecycle(&self) -> &Arc<LifecycleTracker> {
        &self.inner.lifecycle
    }

    /// The current client-side timeouts.
    #[must_use]
    pub fn timeouts(&self) -> Timeouts {
        *self.inner.timeouts.lock()
    }

    /// `page.set_default_timeout` as a [`Duration`].
    #[must_use]
    pub fn default_timeout(&self) -> Duration {
        millis(self.inner.timeouts.lock().default_ms)
    }

    /// `page.set_default_navigation_timeout` as a [`Duration`].
    #[must_use]
    pub fn navigation_timeout(&self) -> Duration {
        millis(self.inner.timeouts.lock().navigation_ms)
    }

    /// Attach proxy credentials so `Fetch.authRequired` can be answered.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn set_proxy_credentials(&self, proxy: Option<&Proxy>) -> Result<()> {
        let credentials = proxy.and_then(ProxyCredentials::from_proxy);
        self.inner.interceptor.set_credentials(credentials).await
    }

    /// The `Fetch` interceptor, for tests and diagnostics.
    #[must_use]
    pub fn intercept_settings(&self) -> InterceptSettings {
        self.inner.interceptor.settings()
    }

    /// Drop every trace of the previous request from this tab.
    ///
    /// A pooled tab keeps its ledger and its in-flight set across requests,
    /// which would leak request *n*'s redirect chain and never-ending downloads
    /// into request *n + 1*. Python gets this for free because the listener is
    /// registered and removed per request.
    ///
    /// The **lifecycle** is deliberately not cleared here. It belongs to the
    /// document, not to the request — Playwright clears it from
    /// `_onClearLifecycle`, i.e. when a new document commits — and
    /// [`CdpPage::goto`] clears it for the frame it is about to navigate. A
    /// `goto` that turns out to be same-document loads no new document, so the
    /// standing `load` has to survive for the `_wait_for_page_stability` that
    /// follows; clearing it here would leave that wait with nothing to wait
    /// for.
    pub fn clear_request_state(&self) {
        self.inner.ledger.reset();
        self.inner.net.reset();
    }

    /// Wait until every captured response has its **unredacted** headers, or
    /// `budget` runs out.
    ///
    /// `Network.responseReceived` reports the headers the renderer is allowed
    /// to see, which is missing `Set-Cookie`; the real ones arrive separately
    /// on `Network.responseReceivedExtraInfo`, and `hasExtraInfo` on the first
    /// event says whether they are coming. Playwright models the gap as a
    /// promise — `Response.allHeaders()` simply awaits it, and `convertor.py`
    /// awaits that — so a `Response` built eagerly has to wait here instead, or
    /// it silently loses every `Set-Cookie` whenever the two events happen to
    /// arrive out of order.
    ///
    /// Never an error: a budget that elapses leaves the redacted headers in
    /// place, which is what Playwright falls back to when `hasExtraInfo` is
    /// `false`.
    pub async fn await_raw_headers(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        while self.inner.ledger.raw_headers_pending() {
            if Instant::now() >= deadline {
                tracing::debug!(
                    "raw response headers did not arrive within {}ms; \
                     falling back to the redacted ones",
                    budget.as_millis()
                );
                return;
            }
            tokio::time::sleep(RAW_HEADERS_POLL_INTERVAL).await;
        }
    }

    /// Open a capture window: `page.on("response", handler)`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::InvalidXhrPattern`] when `capture_xhr` will not compile.
    pub fn begin_capture(&self, xhr_pattern: Option<&str>) -> Result<()> {
        self.inner.ledger.begin_capture(xhr_pattern)
    }

    /// Close the capture window: `page.remove_listener("response", handler)`.
    #[must_use]
    pub fn take_capture(&self) -> Capture {
        self.inner.ledger.take_capture()
    }

    /// `Page.addScriptToEvaluateOnNewDocument` for the session `init_script`.
    ///
    /// The script is registered **without** a `worldName`, i.e. in the page's
    /// main world, because that is where Playwright's `add_init_script` puts it
    /// and where user code expects the globals it installs to be visible.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn add_init_script(&self, source: &str) -> Result<()> {
        self.inner
            .page
            .execute(AddScriptToEvaluateOnNewDocumentParams::new(source))
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.addScriptToEvaluateOnNewDocument".to_owned(),
                message: error.to_string(),
            })?;
        Ok(())
    }

    /// Apply the context options as per-page CDP overrides.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] on the first override Chrome rejects.
    pub async fn apply_emulation(&self, emulation: &PageEmulation) -> Result<()> {
        let page = &self.inner.page;
        let metrics = SetDeviceMetricsOverrideParams {
            width: i64::from(emulation.viewport.width),
            height: i64::from(emulation.viewport.height),
            device_scale_factor: emulation.device_scale_factor,
            mobile: emulation.is_mobile,
            scale: None,
            screen_width: None,
            screen_height: None,
            position_x: None,
            position_y: None,
            dont_set_visible_size: None,
            screen_orientation: None,
            viewport: None,
        };
        execute(page, metrics, "Emulation.setDeviceMetricsOverride").await?;

        let media = SetEmulatedMediaParams {
            media: None,
            features: Some(vec![MediaFeature::new(
                "prefers-color-scheme",
                emulation.color_scheme,
            )]),
        };
        execute(page, media, "Emulation.setEmulatedMedia").await?;

        if let Some(timezone) = emulation.timezone_id.as_deref() {
            execute(
                page,
                SetTimezoneOverrideParams::new(timezone),
                "Emulation.setTimezoneOverride",
            )
            .await?;
        }
        if let Some(locale) = emulation.locale.as_deref() {
            execute(
                page,
                SetLocaleOverrideParams {
                    locale: Some(locale.to_owned()),
                },
                "Emulation.setLocaleOverride",
            )
            .await?;
        }
        if let Some(user_agent) = emulation.user_agent.as_deref() {
            let mut params = SetUserAgentOverrideParams::new(user_agent);
            params.accept_language = emulation.accept_language();
            execute(page, params, "Emulation.setUserAgentOverride").await?;
        }
        if let Some(headers) = emulation.extra_http_headers.as_ref() {
            self.set_extra_http_headers(headers).await?;
        }
        Ok(())
    }

    /// `context.add_cookies(cookies)`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn add_cookies(&self, cookies: &[SetCookieParam]) -> Result<()> {
        if cookies.is_empty() {
            return Ok(());
        }
        let params = NetworkSetCookiesParams::new(
            cookies.iter().map(cookie_param_to_cdp).collect::<Vec<_>>(),
        );
        execute(&self.inner.page, params, "Network.setCookies").await?;
        Ok(())
    }

    /// `page.context.cookies()`, as the name-to-value map unit 12's `Response`
    /// carries.
    ///
    /// Python hands downstream code a tuple of full cookie dicts; unit 12's
    /// `Response.cookies` is an `IndexMap<String, String>` for both engines, so
    /// only the names and values survive. Recorded in `DIVERGENCES.md`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn cookies(&self) -> Result<IndexMap<String, String>> {
        let returns = self
            .inner
            .page
            .execute(StorageGetCookiesParams::default())
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Storage.getCookies".to_owned(),
                message: error.to_string(),
            })?;
        Ok(returns
            .result
            .cookies
            .iter()
            .map(|cookie| (cookie.name.clone(), cookie.value.clone()))
            .collect())
    }

    /// The page's current URL, as `page.url` reports it.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn url(&self) -> Result<String> {
        let tree = self
            .inner
            .page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.getFrameTree".to_owned(),
                message: error.to_string(),
            })?;
        Ok(tree.result.frame_tree.frame.url.clone())
    }

    /// `page.goto(url, referer=…)`.
    ///
    /// Sends `Page.navigate`, turns Chrome's `errorText` into
    /// [`BrowserError::NavigationFailed`] — keeping the `net::ERR_…` text so
    /// [`browser_http::proxy::is_proxy_error`] can classify it — and then
    /// waits for the `load` lifecycle event, which is what Playwright's default
    /// `wait_until="load"` does.
    ///
    /// # A `referer` in the extra headers becomes `Page.navigate`'s `referrer`
    ///
    /// Playwright's `gotoImpl` does this before it navigates:
    ///
    /// ```js
    /// const headers = this._page.extraHTTPHeaders() || [];
    /// const refererHeader = headers.find(h => h.name.toLowerCase() === 'referer');
    /// let referer = refererHeader ? refererHeader.value : undefined;
    /// if (options.referer !== undefined) { …; referer = options.referer; }
    /// await this._page.delegate.navigateFrame(this, url, referer);
    /// ```
    ///
    /// It is not cosmetic. `Referer` is a forbidden header for Chrome's network
    /// service: a navigation whose `ResourceRequest.headers` carry one — which
    /// is where `Network.setExtraHTTPHeaders` puts it — is refused outright
    /// with `net::ERR_BLOCKED_BY_CLIENT`. Lifting it into the `referrer`
    /// parameter is what makes `extra_headers={"referer": …}` work at all, and
    /// The reference implementation relies on that: `fetch()` passes `referer=None` to `goto`
    /// precisely when the caller supplied one in `extra_headers`.
    ///
    /// # The whole call is bounded by `set_default_navigation_timeout`
    ///
    /// Playwright's `page.goto` raises `Page.goto: Timeout 30000ms exceeded.`
    /// once the navigation timeout elapses, whether the hold-up is the server
    /// (a socket that accepts and never answers, so the navigation never
    /// commits) or the document (a commit that never reaches `load`). Over raw
    /// CDP the first of those is a `Page.navigate` command that simply does not
    /// return, so the *command* has to carry the deadline too — otherwise the
    /// only bound left is `chromiumoxide`'s own request timeout, and a request
    /// with `timeout=3000` hangs for tens of seconds. Both halves therefore
    /// share one budget, exactly as Playwright's does.
    ///
    /// # Errors
    ///
    /// [`BrowserError::NavigationFailed`] for Chrome's `net::ERR_…`,
    /// [`BrowserError::Timeout`] when the navigation timeout elapses, or
    /// [`BrowserError::Cdp`].
    pub async fn goto(&self, url: &str, referer: Option<&str>) -> Result<()> {
        let previous_lifecycle = self.inner.lifecycle.take_frame(&self.inner.main_frame);
        let mut params = NavigateParams::new(url);
        params.frame_id = Some(self.inner.main_frame.clone());
        // Playwright pins `referrerPolicy: 'unsafeUrl'` on every
        // `Page.navigate`: Chrome's default `strict-origin-when-cross-origin`
        // drops the referrer entirely on an HTTPS-to-HTTP navigation, which
        // would silently turn `google_search=True` into a no-op for every
        // `http://` target.
        params.referrer_policy = Some(ReferrerPolicy::UnsafeUrl);
        params.referrer = referer.map(str::to_owned).or_else(|| self.header_referer());

        let budget = self.navigation_timeout();
        let deadline = Instant::now() + budget;
        let goto_timeout = || BrowserError::Timeout {
            operation: format!("page.goto({url})"),
            timeout_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
        };

        self.inner.same_document.arm();
        let sent = {
            let execute = self.inner.page.execute(params);
            tokio::pin!(execute);
            let same_document = self.inner.same_document.wait();
            tokio::pin!(same_document);
            tokio::select! {
                // Chrome says the fragment moved and no document was loaded.
                // Nothing further will happen for this navigation, so there is
                // nothing to wait for. Python's next step —
                // `if not first_response: raise RuntimeError(...)` — is the
                // caller's `captured_navigation()` check, and it fires at once
                // rather than after the navigation timeout.
                () = &mut same_document => {
                    tracing::debug!("goto({url}) was a same-document navigation");
                    // No new document, so the old one's lifecycle still stands.
                    self.inner
                        .lifecycle
                        .restore_frame(&self.inner.main_frame, previous_lifecycle);
                    return Ok(());
                }
                sent = &mut execute => sent,
                () = tokio::time::sleep_until(deadline) => return Err(goto_timeout()),
            }
        };
        let returns = sent.map_err(|error| BrowserError::NavigationFailed {
            url: url.to_owned(),
            message: error.to_string(),
        })?;
        // Playwright's `_navigate`, in its order:
        //   if (response.isDownload) throw NavigationAbortedError('Download is starting');
        //   if (response.errorText)  throw NavigationAbortedError(`${errorText} at ${url}`);
        //   return { newDocumentId: response.loaderId };
        if returns.result.is_download.unwrap_or(false) {
            return Err(BrowserError::NavigationFailed {
                url: url.to_owned(),
                message: DOWNLOAD_STARTING.to_owned(),
            });
        }
        if let Some(error_text) = returns.result.error_text.as_ref() {
            return Err(BrowserError::NavigationFailed {
                url: url.to_owned(),
                message: error_text.clone(),
            });
        }
        if returns.result.loader_id.is_none() {
            // A same-document navigation — a fragment on the URL the tab is
            // already showing. CDP omits `loaderId` for exactly this case, and
            // Playwright's `newDocumentId` is then `undefined`, so it never
            // waits for a lifecycle event that will not fire. Python's next
            // step is `if not first_response: raise RuntimeError(...)`, which
            // is what the caller's `captured_navigation()` check reproduces —
            // immediately, rather than after the navigation timeout.
            tracing::debug!("goto({url}) was a same-document navigation");
            return Ok(());
        }
        // Whatever the command left of the budget is what the `load` wait gets,
        // so `goto` as a whole cannot outlive `timeout`.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if self
            .inner
            .lifecycle
            .wait_for(&self.inner.main_frame, LIFECYCLE_LOAD, remaining)
            .await
            .is_err()
        {
            return Err(goto_timeout());
        }
        Ok(())
    }

    /// The `referer` value in the tab's extra headers, if it carries one.
    ///
    /// Playwright's `headers.find(h => h.name.toLowerCase() === 'referer')`.
    fn header_referer(&self) -> Option<String> {
        self.inner
            .extra_headers
            .lock()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("referer"))
            .map(|(_, value)| value.clone())
    }

    /// `page.wait_for_load_state(state=…)` for the main frame.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Timeout`].
    pub async fn wait_for_load_state(&self, name: &str) -> Result<()> {
        self.wait_for_frame_load_state(&self.inner.main_frame, name)
            .await
    }

    /// `frame.wait_for_load_state(state=…)`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Timeout`].
    pub async fn wait_for_frame_load_state(&self, frame: &FrameId, name: &str) -> Result<()> {
        self.inner
            .lifecycle
            .wait_for(frame, name, self.default_timeout())
            .await
    }

    /// `_wait_for_networkidle(page, timeout)`.
    ///
    /// `budget` of `None` means "the page default", which is what Python's
    /// `timeout=None` resolves to. Never fails — a timeout is a no-op.
    pub async fn wait_for_networkidle(&self, budget: Option<Duration>) -> IdleOutcome {
        let budget = budget.unwrap_or_else(|| self.default_timeout());
        let outcome = self
            .inner
            .net
            .wait_for_idle(NETWORK_IDLE_QUIET_PERIOD, budget)
            .await;
        if outcome == IdleOutcome::TimedOut {
            tracing::debug!(
                "networkidle budget of {}ms elapsed with {} request(s) still in flight",
                budget.as_millis(),
                self.inner.net.in_flight()
            );
        }
        outcome
    }

    /// `_wait_for_page_stability(page, load_dom, network_idle)`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Timeout`] from either load-state wait. The
    /// `networkidle` step never contributes an error.
    pub async fn wait_for_page_stability(&self, load_dom: bool, network_idle: bool) -> Result<()> {
        self.wait_for_frame_stability(&self.inner.main_frame, load_dom, network_idle)
            .await
    }

    /// `_wait_for_page_stability` against an arbitrary frame — the solver calls
    /// it on the challenge iframe.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Timeout`].
    pub async fn wait_for_frame_stability(
        &self,
        frame: &FrameId,
        load_dom: bool,
        network_idle: bool,
    ) -> Result<()> {
        self.wait_for_frame_load_state(frame, LIFECYCLE_LOAD)
            .await?;
        if load_dom {
            self.wait_for_frame_load_state(frame, LIFECYCLE_DOM_CONTENT_LOADED)
                .await?;
        }
        if network_idle {
            self.wait_for_networkidle(None).await;
        }
        Ok(())
    }

    /// `page.wait_for_timeout(ms)`.
    pub async fn wait_for_timeout(&self, millis_: f64) {
        if millis_ > 0.0 {
            tokio::time::sleep(millis(millis_)).await;
        }
    }

    /// `ResponseFactory._get_page_content(page)` — the serialised live DOM.
    ///
    /// Keeps the 20 × 500 ms retry loop of the Windows workaround; the final
    /// failure is [`BrowserError::PageContentUnavailable`] with Python's exact
    /// message.
    ///
    /// # Errors
    ///
    /// [`BrowserError::PageContentUnavailable`].
    pub async fn content(&self) -> Result<String> {
        let mut last: Option<BrowserError> = None;
        for _ in 0..PAGE_CONTENT_MAX_RETRIES {
            match self.evaluate_in_main_frame(PAGE_CONTENT_JS).await {
                Ok(value) => {
                    // `page.content() or ""`.
                    return Ok(value.as_str().unwrap_or_default().to_owned());
                }
                Err(error) => {
                    tracing::debug!("page.content() failed, retrying: {error}");
                    last = Some(error);
                    tokio::time::sleep(PAGE_CONTENT_RETRY_DELAY).await;
                }
            }
        }
        tracing::error!(
            "Error getting page content: {}",
            last.map_or_else(|| "unknown".to_owned(), |error| error.to_string())
        );
        Err(BrowserError::PageContentUnavailable {
            elapsed_ms: u64::from(PAGE_CONTENT_MAX_RETRIES)
                * u64::try_from(PAGE_CONTENT_RETRY_DELAY.as_millis()).unwrap_or(500),
        })
    }

    /// Evaluate an expression in the main frame's isolated world.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] or [`BrowserError::JavaScript`].
    pub async fn evaluate_in_main_frame(&self, expression: &str) -> Result<Value> {
        self.inner
            .worlds
            .evaluate(&self.inner.page, &self.inner.main_frame, expression)
            .await
    }

    /// `page.locator(selector).first.wait_for(state=…)`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Timeout`] when the state is never reached, or the
    /// evaluation errors.
    pub async fn wait_for_selector(&self, selector: &str, state: WaitState) -> Result<()> {
        let budget = self.default_timeout();
        let deadline = Instant::now() + budget;
        let expression = isolated_world::selector_state_expression(selector);
        loop {
            let observed = self.evaluate_in_main_frame(&expression).await?;
            let observed = observed.as_str().unwrap_or("missing");
            let satisfied = match state {
                WaitState::Attached => observed != "missing",
                WaitState::Detached => observed == "missing",
                WaitState::Visible => observed == "visible",
                // Playwright's `hidden` is satisfied by *absent* too.
                WaitState::Hidden => observed == "hidden" || observed == "missing",
            };
            if satisfied {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout {
                    operation: format!("selector {selector:?} to be {}", state.as_str()),
                    timeout_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                });
            }
            tokio::time::sleep(SELECTOR_POLL_INTERVAL).await;
        }
    }

    /// `page.mouse.click(x, y, delay=…, button="left")`.
    ///
    /// Playwright moves the pointer first, then presses, holds for `delay`, and
    /// releases. The move matters: a Turnstile widget listens for `mousemove`
    /// before it accepts the click.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn mouse_click(&self, x: f64, y: f64, delay: Duration) -> Result<()> {
        let page = &self.inner.page;
        let mut moved = DispatchMouseEventParams::new(DispatchMouseEventType::MouseMoved, x, y);
        moved.button = Some(MouseButton::None);
        moved.buttons = Some(0);
        execute(page, moved, "Input.dispatchMouseEvent(mouseMoved)").await?;

        let mut pressed = DispatchMouseEventParams::new(DispatchMouseEventType::MousePressed, x, y);
        pressed.button = Some(MouseButton::Left);
        pressed.buttons = Some(1);
        pressed.click_count = Some(1);
        execute(page, pressed, "Input.dispatchMouseEvent(mousePressed)").await?;

        tokio::time::sleep(delay).await;

        let mut released =
            DispatchMouseEventParams::new(DispatchMouseEventType::MouseReleased, x, y);
        released.button = Some(MouseButton::Left);
        released.buttons = Some(0);
        released.click_count = Some(1);
        execute(page, released, "Input.dispatchMouseEvent(mouseReleased)").await?;
        Ok(())
    }

    /// The body of a finished response, as `response.body()` returns it.
    ///
    /// `None` when Chrome no longer has it — which is normal for a redirect and
    /// for a body evicted from the network cache. Python's factory catches the
    /// same failure and falls back to `b""`.
    pub async fn response_body(&self, request_id: &str) -> Option<Bytes> {
        let params = GetResponseBodyParams::new(request_id.to_owned());
        let returns = match self.inner.page.execute(params).await {
            Ok(returns) => returns,
            Err(error) => {
                tracing::debug!("Network.getResponseBody({request_id}) failed: {error}");
                return None;
            }
        };
        let body = &returns.result;
        if body.base64_encoded {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(&body.body) {
                Ok(bytes) => Some(Bytes::from(bytes)),
                Err(error) => {
                    tracing::debug!("Response body was not valid base64: {error}");
                    None
                }
            }
        } else {
            Some(Bytes::from(body.body.clone().into_bytes()))
        }
    }

    /// `page.frame(url=predicate)` — the first frame in the tree whose URL
    /// satisfies `predicate`, main frame included.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn frame_matching<F>(&self, predicate: F) -> Result<Option<FrameId>>
    where
        F: Fn(&str) -> bool,
    {
        let tree = self
            .inner
            .page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.getFrameTree".to_owned(),
                message: error.to_string(),
            })?;
        Ok(find_frame(&tree.result.frame_tree, &predicate))
    }

    /// `page.locator(selector).last.bounding_box()`.
    ///
    /// The **last** match, not the first: a challenge page can hold several
    /// `div`s under the widget selector and Cloudflare's is the last of them.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] or [`BrowserError::JavaScript`].
    pub async fn last_bounding_box(&self, selector: &str) -> Result<Option<BoundingBox>> {
        let expression = isolated_world::last_bounding_box_expression(selector);
        let value = self.evaluate_in_main_frame(&expression).await?;
        Ok(BoundingBox::from_json(&value))
    }

    /// `frame.frame_element().bounding_box()`.
    ///
    /// Resolves the `<iframe>` element that owns `frame` **into the parent's
    /// isolated world**, so the measuring function never touches the page's
    /// main world.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] or [`BrowserError::JavaScript`].
    pub async fn frame_element_bounding_box(&self, frame: &FrameId) -> Result<Option<BoundingBox>> {
        let value = self.call_on_frame_element(frame, BOUNDING_BOX_JS).await?;
        Ok(BoundingBox::from_json(&value))
    }

    /// `frame.frame_element().is_visible()`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] or [`BrowserError::JavaScript`].
    pub async fn frame_element_is_visible(&self, frame: &FrameId) -> Result<bool> {
        let value = self.call_on_frame_element(frame, IS_VISIBLE_JS).await?;
        Ok(value.as_bool().unwrap_or(false))
    }

    async fn call_on_frame_element(&self, frame: &FrameId, function: &str) -> Result<Value> {
        let page = &self.inner.page;
        self.ensure_dom_enabled().await;

        let owner = page
            .execute(GetFrameOwnerParams::new(frame.clone()))
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "DOM.getFrameOwner".to_owned(),
                message: error.to_string(),
            })?;
        let context = self
            .inner
            .worlds
            .context_for(page, &self.inner.main_frame)
            .await?;
        let params = ResolveNodeParams {
            backend_node_id: Some(owner.result.backend_node_id),
            execution_context_id: Some(context),
            ..ResolveNodeParams::default()
        };
        let resolved = page
            .execute(params)
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "DOM.resolveNode".to_owned(),
                message: error.to_string(),
            })?;
        let Some(object_id) = resolved.result.object.object_id.as_ref() else {
            return Ok(Value::Null);
        };
        isolated_world::call_on(page, object_id.inner(), function).await
    }

    async fn ensure_dom_enabled(&self) {
        if self.inner.dom_enabled.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.inner.page.execute(DomEnableParams::default()).await {
            // Not fatal: `DOM.getFrameOwner` works without the agent enabled on
            // current Chrome, and the retry cost of guessing wrong is nil.
            tracing::debug!("DOM.enable failed: {error}");
        }
    }
}

#[async_trait::async_trait]
impl PageControl for CdpPage {
    async fn set_default_navigation_timeout(&self, timeout_ms: f64) -> Result<()> {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.navigation_timeout);
        self.inner.timeouts.lock().navigation_ms = timeout_ms;
        Ok(())
    }

    async fn set_default_timeout(&self, timeout_ms: f64) -> Result<()> {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.default_timeout);
        self.inner.timeouts.lock().default_ms = timeout_ms;
        Ok(())
    }

    async fn set_extra_http_headers(&self, headers: &HeaderDict) -> Result<()> {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.extra_headers);
        *self.inner.extra_headers.lock() = headers.clone();
        let json: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        let params = SetExtraHttpHeadersParams::new(CdpHeaders::new(Value::Object(json)));
        execute(&self.inner.page, params, "Network.setExtraHTTPHeaders").await?;
        Ok(())
    }

    async fn unroute_all(&self) -> Result<()> {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.unroute_all);
        self.inner.interceptor.unroute_all().await
    }

    async fn route(&self, policy: RoutePolicy) -> Result<()> {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.route);
        self.inner.interceptor.route(policy).await
    }

    async fn is_closed(&self) -> bool {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.is_closed);
        if self.inner.closed.load(Ordering::SeqCst) {
            return true;
        }
        // Playwright answers from a local flag it maintains from
        // `Target.targetDestroyed`; `chromiumoxide` exposes no such flag, so
        // the tab is probed instead. One round trip per request is noise next
        // to a page load, and it genuinely catches a tab the *browser* closed.
        let probe = tokio::time::timeout(
            TEARDOWN_TIMEOUT,
            self.inner.page.execute(GetFrameTreeParams::default()),
        )
        .await;
        match probe {
            Ok(Ok(_)) => false,
            Ok(Err(error)) => {
                tracing::debug!("Tab looks closed: {error}");
                self.inner.closed.store(true, Ordering::SeqCst);
                true
            }
            Err(_) => {
                tracing::debug!("Tab did not answer within {TEARDOWN_TIMEOUT:?}; evicting it");
                self.inner.closed.store(true, Ordering::SeqCst);
                true
            }
        }
    }

    async fn close(&self) -> Result<()> {
        #[cfg(test)]
        ControlCounts::bump(&self.inner.counts.close);
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match tokio::time::timeout(
            TEARDOWN_TIMEOUT,
            execute(&self.inner.page, CloseParams::default(), "Page.close"),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(BrowserError::Timeout {
                operation: "Page.close".to_owned(),
                timeout_ms: u64::try_from(TEARDOWN_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Milliseconds — as the reference implementation spells every browser duration — to a [`Duration`].
///
/// Negative and non-finite values become zero; `validate()` has already
/// rejected them, so this is belt and braces rather than policy.
///
/// ```
/// use std::time::Duration;
/// use browser_cdp::cdp::millis;
///
/// assert_eq!(millis(1500.0), Duration::from_millis(1500));
/// assert_eq!(millis(-1.0), Duration::ZERO);
/// assert_eq!(millis(f64::NAN), Duration::ZERO);
/// ```
#[must_use]
pub fn millis(value: f64) -> Duration {
    if value.is_finite() && value > 0.0 {
        Duration::from_secs_f64(value / 1000.0)
    } else {
        Duration::ZERO
    }
}

/// Depth-first search of a frame tree, main frame first.
fn find_frame<F>(tree: &FrameTree, predicate: &F) -> Option<FrameId>
where
    F: Fn(&str) -> bool,
{
    if predicate(&tree.frame.url) {
        return Some(tree.frame.id.clone());
    }
    for child in tree.child_frames.iter().flatten() {
        if let Some(found) = find_frame(child, predicate) {
            return Some(found);
        }
    }
    None
}

async fn execute<T>(page: &Page, command: T, context: &str) -> Result<()>
where
    T: chromiumoxide::types::Command,
{
    page.execute(command)
        .await
        .map_err(|error| BrowserError::Cdp {
            context: context.to_owned(),
            message: error.to_string(),
        })?;
    Ok(())
}

fn cookie_param_to_cdp(cookie: &SetCookieParam) -> CookieParam {
    let mut param = CookieParam::new(cookie.name.clone(), cookie.value.clone());
    param.url = cookie.url.clone();
    param.domain = cookie.domain.clone();
    param.path = cookie.path.clone();
    param.secure = cookie.secure;
    param.http_only = cookie.http_only;
    param.same_site = cookie.same_site.map(|site| match site {
        browser_core::types::SameSite::Lax => {
            chromiumoxide::cdp::browser_protocol::network::CookieSameSite::Lax
        }
        browser_core::types::SameSite::Strict => {
            chromiumoxide::cdp::browser_protocol::network::CookieSameSite::Strict
        }
        browser_core::types::SameSite::None => {
            chromiumoxide::cdp::browser_protocol::network::CookieSameSite::None
        }
    });
    param.expires = cookie
        .expires
        .map(chromiumoxide::cdp::browser_protocol::network::TimeSinceEpoch::new);
    param
}

async fn spawn_request_pump(
    page: &Page,
    net: &Arc<NetworkTracker>,
    ledger: &Arc<RequestLedger>,
) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Network.requestWillBeSent listener".to_owned(),
            message: error.to_string(),
        })?;
    let net = Arc::clone(net);
    let ledger = Arc::clone(ledger);
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let request_id = event.request_id.inner().clone();
            // Playwright keeps favicons out of the in-flight count; Chrome
            // fetches one for every top-level page and a slow one would stop
            // `networkidle` from ever firing.
            if !networkidle::is_favicon(&event.request.url) {
                net.request_started(&request_id);
            }
            ledger.on_request_started(&RequestStarted {
                request_id,
                loader_id: event.loader_id.inner().clone(),
                url: event.request.url.clone(),
                headers: headers_from_json(event.request.headers.inner()),
                cdp_resource_type: event
                    .r#type
                    .as_ref()
                    .map(|kind| kind.as_ref().to_owned())
                    .unwrap_or_default(),
                frame_id: event.frame_id.as_ref().map(|id| id.inner().clone()),
                redirect_has_extra_info: event.redirect_has_extra_info,
                redirect_response: event.redirect_response.as_ref().map(|response| {
                    RedirectResponse {
                        url: response.url.clone(),
                        status: u16::try_from(response.status).unwrap_or_default(),
                        status_text: response.status_text.clone(),
                        headers: headers_from_json(response.headers.inner()),
                    }
                }),
            });
        }
    }))
}

async fn spawn_response_pump(page: &Page, ledger: &Arc<RequestLedger>) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventResponseReceived>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Network.responseReceived listener".to_owned(),
            message: error.to_string(),
        })?;
    let ledger = Arc::clone(ledger);
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            ledger.on_response_received(&ResponseArrived {
                request_id: event.request_id.inner().clone(),
                url: event.response.url.clone(),
                status: u16::try_from(event.response.status).unwrap_or_default(),
                status_text: event.response.status_text.clone(),
                headers: headers_from_json(event.response.headers.inner()),
                cdp_resource_type: event.r#type.as_ref().to_owned(),
                frame_id: event.frame_id.as_ref().map(|id| id.inner().clone()),
                has_extra_info: event.has_extra_info,
            });
        }
    }))
}

/// The two `…ExtraInfo` events that carry the **unredacted** headers.
///
/// `Network.requestWillBeSent` / `Network.responseReceived` report what the
/// renderer is allowed to see, which drops `Set-Cookie` and fills in the
/// `sec-ch-*` client hints only sometimes. Playwright's `allHeaders()` — the
/// accessor `convertor.py` uses for both `Response.headers` and
/// `Response.request_headers` — resolves to these instead. One task drives
/// both streams because they are two halves of the same job.
async fn spawn_extra_info_pumps(
    page: &Page,
    ledger: &Arc<RequestLedger>,
) -> Result<JoinHandle<()>> {
    let mut requests = page
        .event_listener::<EventRequestWillBeSentExtraInfo>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Network.requestWillBeSentExtraInfo listener".to_owned(),
            message: error.to_string(),
        })?;
    let mut responses = page
        .event_listener::<EventResponseReceivedExtraInfo>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Network.responseReceivedExtraInfo listener".to_owned(),
            message: error.to_string(),
        })?;
    let ledger = Arc::clone(ledger);
    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                event = requests.next() => match event {
                    Some(event) => ledger.on_request_extra_info(
                        event.request_id.inner(),
                        headers_from_json(event.headers.inner()),
                    ),
                    None => break,
                },
                event = responses.next() => match event {
                    Some(event) => ledger.on_response_extra_info(
                        event.request_id.inner(),
                        headers_from_json(event.headers.inner()),
                    ),
                    None => break,
                },
            }
        }
    }))
}

async fn spawn_finished_pump(page: &Page, net: &Arc<NetworkTracker>) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventLoadingFinished>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Network.loadingFinished listener".to_owned(),
            message: error.to_string(),
        })?;
    let net = Arc::clone(net);
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            net.request_finished(event.request_id.inner());
        }
    }))
}

async fn spawn_failed_pump(page: &Page, net: &Arc<NetworkTracker>) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventLoadingFailed>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Network.loadingFailed listener".to_owned(),
            message: error.to_string(),
        })?;
    let net = Arc::clone(net);
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            net.request_finished(event.request_id.inner());
        }
    }))
}

/// Whether the main frame has navigated **within** the current document since
/// the last [`SameDocumentSignal::arm`].
///
/// `Page.navigatedWithinDocument` is Chrome telling us that a `Page.navigate`
/// moved the fragment rather than loading a document. Playwright learns the
/// same thing from a `Page.navigate` reply with no `loaderId`; the driver this
/// port uses withholds that reply until a lifecycle completes, and for a
/// same-document navigation no lifecycle ever will — so the event is watched
/// directly instead. See [`CdpPage::goto`].
#[derive(Debug, Default)]
struct SameDocumentSignal {
    fired: AtomicBool,
    notify: Notify,
}

impl SameDocumentSignal {
    /// Forget any earlier same-document navigation.
    fn arm(&self) {
        self.fired.store(false, Ordering::SeqCst);
    }

    /// Record one, and wake anything waiting.
    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Resolve once one has happened since [`SameDocumentSignal::arm`].
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.fired.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

async fn spawn_same_document_pump(
    page: &Page,
    signal: &Arc<SameDocumentSignal>,
    main_frame: &FrameId,
) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventNavigatedWithinDocument>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Page.navigatedWithinDocument listener".to_owned(),
            message: error.to_string(),
        })?;
    let signal = Arc::clone(signal);
    let main_frame = main_frame.clone();
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            if event.frame_id == main_frame {
                signal.fire();
            }
        }
    }))
}

async fn spawn_lifecycle_pump(
    page: &Page,
    lifecycle: &Arc<LifecycleTracker>,
    worlds: &Arc<IsolatedWorlds>,
    net: &Arc<NetworkTracker>,
    main_frame: &FrameId,
) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventLifecycleEvent>()
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Page.lifecycleEvent listener".to_owned(),
            message: error.to_string(),
        })?;
    let lifecycle = Arc::clone(lifecycle);
    let worlds = Arc::clone(worlds);
    let net = Arc::clone(net);
    let main_frame = main_frame.clone();
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            if lifecycle.on_event(&event.frame_id, &event.name) {
                // A new document means a new main world, and the isolated world
                // that hung off the old one is gone with it.
                worlds.invalidate(&event.frame_id);
                if event.frame_id == main_frame {
                    // Playwright's `_onClearLifecycle`: the previous document's
                    // in-flight requests are dropped and the 500 ms timer
                    // restarts, so a page that navigates away mid-download can
                    // still reach `networkidle`.
                    net.reset();
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A countable stand-in for a tab, so the reset sequence of
    /// `test_page_reuse.py` can be pinned without a browser.
    #[derive(Clone, Debug, Default)]
    struct FakePage {
        state: Arc<FakeState>,
    }

    #[derive(Debug, Default)]
    struct FakeState {
        navigation_timeouts: Mutex<Vec<f64>>,
        default_timeouts: Mutex<Vec<f64>>,
        headers: Mutex<Vec<HeaderDict>>,
        unroute_calls: AtomicUsize,
        route_calls: AtomicUsize,
        close_calls: AtomicUsize,
        closed: AtomicBool,
    }

    impl FakePage {
        fn new() -> Self {
            Self::default()
        }

        fn unroute_count(&self) -> usize {
            self.state.unroute_calls.load(Ordering::SeqCst)
        }

        fn route_count(&self) -> usize {
            self.state.route_calls.load(Ordering::SeqCst)
        }

        fn close_count(&self) -> usize {
            self.state.close_calls.load(Ordering::SeqCst)
        }

        fn last_default_timeout(&self) -> Option<f64> {
            self.state.default_timeouts.lock().last().copied()
        }

        fn last_headers(&self) -> Option<HeaderDict> {
            self.state.headers.lock().last().cloned()
        }
    }

    #[async_trait::async_trait]
    impl PageControl for FakePage {
        async fn set_default_navigation_timeout(&self, timeout_ms: f64) -> Result<()> {
            self.state.navigation_timeouts.lock().push(timeout_ms);
            Ok(())
        }

        async fn set_default_timeout(&self, timeout_ms: f64) -> Result<()> {
            self.state.default_timeouts.lock().push(timeout_ms);
            Ok(())
        }

        async fn set_extra_http_headers(&self, headers: &HeaderDict) -> Result<()> {
            self.state.headers.lock().push(headers.clone());
            Ok(())
        }

        async fn unroute_all(&self) -> Result<()> {
            self.state.unroute_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn route(&self, _policy: RoutePolicy) -> Result<()> {
            self.state.route_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn is_closed(&self) -> bool {
            self.state.closed.load(Ordering::SeqCst)
        }

        async fn close(&self) -> Result<()> {
            self.state.close_calls.fetch_add(1, Ordering::SeqCst);
            self.state.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    async fn acquire(
        pool: &PagePool<FakePage>,
        timeout_ms: f64,
        extra_headers: Option<&HeaderDict>,
        policy: &RoutePolicy,
    ) -> Result<PageInfo<FakePage>> {
        get_page(
            pool,
            Duration::from_secs(60),
            false,
            || async { Ok(FakePage::new()) },
            timeout_ms,
            extra_headers,
            policy,
        )
        .await
    }

    /// `test_page_reuse.py::test_second_request_reuses_the_tab_and_resets_its_settings`.
    #[tokio::test]
    async fn a_second_request_reuses_the_tab_and_resets_its_settings() {
        let pool = PagePool::<FakePage>::new(1);
        let blocking = RoutePolicy::new(true, None);
        let first = acquire(&pool, 1000.0, Some(&headers(&[("x-test", "1")])), &blocking)
            .await
            .expect("first tab");
        let first_page = first.page().clone();
        release_page(&pool, &first, false).await;
        assert_eq!(pool.pages_count(), 1);
        assert_eq!(first.state(), PageState::Ready);

        let second = acquire(&pool, 2000.0, None, &RoutePolicy::default())
            .await
            .expect("second tab");
        assert_eq!(second.id(), first.id(), "the same tab is reused");
        assert_eq!(second.state(), PageState::Busy);
        release_page(&pool, &second, false).await;

        assert_eq!(first_page.last_default_timeout(), Some(2000.0));
        assert_eq!(
            first_page.last_headers(),
            Some(HeaderDict::new()),
            "`set_extra_http_headers({{}})` clears the previous request's headers"
        );
        assert_eq!(first_page.unroute_count(), 2, "once per request");
        assert_eq!(
            first_page.route_count(),
            1,
            "only the request that asked for blocking installs a route"
        );
    }

    /// `test_page_reuse.py::test_errored_tab_is_closed_and_evicted`.
    #[tokio::test]
    async fn an_errored_tab_is_closed_and_evicted() {
        let pool = PagePool::<FakePage>::new(1);
        let info = acquire(&pool, 1000.0, None, &RoutePolicy::default())
            .await
            .expect("a tab");
        info.mark_error();
        release_page(&pool, &info, false).await;

        assert_eq!(info.page().close_count(), 1);
        assert_eq!(pool.pages_count(), 0);

        // The next request opens a brand-new tab.
        let fresh = acquire(&pool, 1000.0, None, &RoutePolicy::default())
            .await
            .expect("a fresh tab");
        assert_ne!(fresh.id(), info.id());
        assert_eq!(pool.pages_count(), 1);
    }

    /// `test_page_reuse.py::test_tab_closed_by_the_browser_is_evicted`.
    #[tokio::test]
    async fn a_tab_the_browser_closed_is_evicted() {
        let pool = PagePool::<FakePage>::new(1);
        let info = acquire(&pool, 1000.0, None, &RoutePolicy::default())
            .await
            .expect("a tab");
        info.page().state.closed.store(true, Ordering::SeqCst);
        release_page(&pool, &info, false).await;
        assert_eq!(pool.pages_count(), 0);
    }

    /// Rotation mode: the tab always leaves the pool with its context.
    #[tokio::test]
    async fn a_rotation_tab_never_stays_in_the_pool() {
        let pool = PagePool::<FakePage>::new(2);
        let info = get_page(
            &pool,
            Duration::from_secs(60),
            true,
            || async { Ok(FakePage::new()) },
            1000.0,
            None,
            &RoutePolicy::default(),
        )
        .await
        .expect("a tab");
        assert_eq!(pool.pages_count(), 1);
        release_page(&pool, &info, true).await;
        assert_eq!(pool.pages_count(), 0);
        assert_eq!(
            info.page().close_count(),
            0,
            "the context close takes the tab with it"
        );
    }

    /// `test_page_reuse.py::test_pool_waits_for_a_ready_tab_at_capacity`.
    ///
    /// The Python test shrinks `_max_wait_for_page` to 1 s for exactly the same
    /// reason: `tokio`'s clock cannot be paused in this workspace.
    #[tokio::test]
    async fn at_capacity_the_pool_times_out_with_pythons_message() {
        let pool = PagePool::<FakePage>::new(1);
        let _busy = acquire(&pool, 1000.0, None, &RoutePolicy::default())
            .await
            .expect("a tab");
        let error = get_page(
            &pool,
            Duration::from_millis(120),
            false,
            || async { Ok(FakePage::new()) },
            1000.0,
            None,
            &RoutePolicy::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "No pages finished to clear place in the pool within the 0s timeout period",
            "Python interpolates `_max_wait_for_page`, an int number of seconds"
        );
    }

    #[tokio::test]
    async fn the_reset_sequence_runs_in_pythons_order() {
        let page = FakePage::new();
        reset_for_request(&page, 5000.0, None, &RoutePolicy::default())
            .await
            .expect("reset");
        assert_eq!(page.state.navigation_timeouts.lock().as_slice(), [5000.0]);
        assert_eq!(page.state.default_timeouts.lock().as_slice(), [5000.0]);
        assert_eq!(page.unroute_count(), 1);
        assert_eq!(
            page.route_count(),
            0,
            "an empty policy installs no route, matching `if disable_resources or blocked_domains`"
        );
    }

    #[test]
    fn milliseconds_convert_defensively() {
        assert_eq!(millis(0.0), Duration::ZERO);
        assert_eq!(millis(250.0), Duration::from_millis(250));
        assert_eq!(millis(f64::INFINITY), Duration::ZERO);
    }

    #[test]
    fn emulation_reads_the_stealth_viewport() {
        use crate::config::{BrowserConfig, StealthExtras};
        use crate::flags::context_options;

        let dynamic = BrowserConfig::new().validate().expect("valid");
        let emulation = PageEmulation::from_context_options(&context_options(&dynamic));
        assert_eq!(emulation.viewport, DEFAULT_VIEWPORT);
        assert_eq!(emulation.device_scale_factor, 2.0);
        assert_eq!(emulation.color_scheme, "dark");
        assert!(!emulation.is_mobile);
        assert!(
            emulation.timezone_id.is_none(),
            "the default `timezone_id` is the empty string, i.e. no override"
        );

        let stealthy = BrowserConfig::new()
            .stealth(StealthExtras::new())
            .validate()
            .expect("valid");
        let emulation = PageEmulation::from_context_options(&context_options(&stealthy));
        assert_eq!(
            emulation.viewport,
            Viewport {
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn accept_language_mirrors_the_launch_flag() {
        use crate::config::BrowserConfig;
        use crate::flags::context_options;

        let remote = BrowserConfig::new()
            .locale("fr-FR")
            .cdp_url("ws://h:9222")
            .validate()
            .expect("valid");
        let emulation = PageEmulation::from_context_options(&context_options(&remote));
        assert_eq!(emulation.accept_language().as_deref(), Some("fr-FR,fr"));

        let same = PageEmulation {
            locale: Some("fr".to_owned()),
            ..emulation
        };
        assert_eq!(same.accept_language().as_deref(), Some("fr"));
    }

    #[tokio::test]
    async fn lifecycle_events_reset_on_a_new_document() {
        let tracker = LifecycleTracker::new();
        let frame = FrameId::new("MAIN");
        assert!(!tracker.on_event(&frame, LIFECYCLE_LOAD));
        assert!(tracker.has_fired(&frame, LIFECYCLE_LOAD));
        assert!(
            tracker.on_event(&frame, LIFECYCLE_INIT),
            "`init` reports that a new document started"
        );
        assert!(
            !tracker.has_fired(&frame, LIFECYCLE_LOAD),
            "the previous document's events must not satisfy this one's waits"
        );
        tracker.on_event(&frame, LIFECYCLE_LOAD);
        tracker
            .wait_for(&frame, LIFECYCLE_LOAD, Duration::from_millis(10))
            .await
            .expect("already fired");
    }

    #[tokio::test]
    async fn a_load_state_that_never_fires_times_out() {
        let tracker = LifecycleTracker::new();
        let error = tracker
            .wait_for(
                &FrameId::new("MAIN"),
                LIFECYCLE_LOAD,
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, BrowserError::Timeout { .. }));
        assert_eq!(
            error.to_string(),
            "Timeout 50ms exceeded while waiting for the \"load\" load state."
        );
    }

    #[tokio::test]
    async fn lifecycle_tracking_is_per_frame() {
        let tracker = LifecycleTracker::new();
        let main = FrameId::new("MAIN");
        let child = FrameId::new("CHILD");
        tracker.on_event(&main, LIFECYCLE_LOAD);
        assert!(!tracker.has_fired(&child, LIFECYCLE_LOAD));
        tracker.on_event(&child, LIFECYCLE_LOAD);
        assert!(tracker.has_fired(&child, LIFECYCLE_LOAD));
        tracker.reset_frame(&main);
        assert!(!tracker.has_fired(&main, LIFECYCLE_LOAD));
        assert!(tracker.has_fired(&child, LIFECYCLE_LOAD));
    }
}
