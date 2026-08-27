//! [`BrowserSession`]: the three launch modes, the page pool, the retry loop
//! and the `fetch()` algorithm of `browser.md` §4 and §5.
//!
//! # The three launch modes
//!
//! | mode | condition | what starts | context |
//! |---|---|---|---|
//! | [`LaunchMode::Cdp`] | `cdp_url` set | nothing — we connect | the browser's default, unless a rotator is attached |
//! | [`LaunchMode::Rotation`] | a `proxy_rotator` is attached | a browser with no shared context | one fresh browser context per request |
//! | [`LaunchMode::Persistent`] | otherwise | a browser on a profile directory | its default context |
//!
//! Playwright's `launch_persistent_context(user_data_dir=…)` is, in CDP terms,
//! `--user-data-dir=<path>` plus the default browser context, so the persistent
//! mode is a plain launch with that flag; `user_data_dir=""` means a temporary
//! profile this session creates and deletes, exactly as Playwright does.
//!
//! # Async-first
//!
//! `ARCHITECTURE.md` §13.2: go async, and let the blocking façade wrap it.
//! [`BrowserSession`] is the async session; Python's *sync* sessions differ in
//! only two ways — a fixed pool of one tab, and no wait-for-a-free-tab step —
//! and the first is [`BrowserSession::with_pool_size`].
//!
//! # Everything is `&self`
//!
//! Three concurrent `fetch()` calls against one session is a pinned scenario
//! (`tests/fetchers/async/test_dynamic_session.py`), so the session's mutable
//! state lives behind a `tokio::sync::Mutex` and every method takes `&self`.
//! That also makes `Arc<BrowserSession>` the natural sharing unit for unit 17's
//! crawler.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use browser_core::types::Proxy;
use browser_http::config::SelectorConfig;
use browser_http::response::Response;
use chromiumoxide::browser::{Browser, BrowserConfig as ChromiumConfig};
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams, DisposeBrowserContextParams,
};
use chromiumoxide::handler::viewport::Viewport as ChromiumViewport;
use futures::future::BoxFuture;
use futures::StreamExt;
use indexmap::IndexMap;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cdp::isolated_world::BoundingBox;
use crate::cdp::{get_page, millis, release_page, CdpPage, PageControl, PageEmulation};
use crate::cloudflare::{self, CloudflarePage};
use crate::config::{
    proxy_is_truthy, BrowserConfig, FetchParams, LaunchMode, ResolvedFetchParams, ValidatedConfig,
};
use crate::error::{BrowserError, Result};
use crate::flags::{context_options, launch_options, ContextOptions, LaunchOptions};
use crate::pool::{PagePool, PoolStats, DEFAULT_MAX_WAIT_FOR_PAGE};
use crate::response::{proxy_meta, BrowserFetch, BrowserResponseFactory, PageSource};
use crate::route::RoutePolicy;

/// Where a stock Google Chrome lives, per platform, for `real_chrome=True`.
///
/// Playwright resolves `channel="chrome"` through its own browser registry;
/// driving CDP there is no registry, so the well-known install paths are
/// probed instead. `executable_path` always wins over this list.
pub const CHROME_PATHS: &[&str] = &[
    // macOS
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
    // Linux
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/opt/google/chrome/chrome",
    // Windows
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
];

/// How long to wait for the browser to print its WebSocket URL.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The per-command CDP timeout.
///
/// Generous, because the request-level budget is `timeout` and a single command
/// timing out early would turn a slow page into a failed one.
pub const CDP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A `page_setup` / `page_action` hook.
///
/// `ARCHITECTURE.md` §13.4 note 6 sketches this as
/// `Box<dyn Fn(&Page) -> BoxFuture<'_, …>>`. The page is taken **by value**
/// instead: [`CdpPage`] is an `Arc` handle, cloning it is free, and a
/// higher-ranked closure returning a borrow-capturing boxed future is close to
/// unwritable in stable Rust. Use [`page_hook`] to build one from an async
/// function.
pub type PageHook = Arc<dyn Fn(CdpPage) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/// Wrap an async function as a [`PageHook`].
///
/// ```
/// use browser_cdp::session::page_hook;
///
/// let hook = page_hook(|page| async move {
///     page.wait_for_timeout(50.0).await;
///     Ok(())
/// });
/// # let _ = hook;
/// ```
pub fn page_hook<F, Fut>(hook: F) -> PageHook
where
    F: Fn(CdpPage) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    Arc::new(move |page| Box::pin(hook(page)))
}

/// The two user callbacks a session runs.
///
/// Both are best-effort: "Exceptions are caught and logged" (`browser.md`
/// §2.4). A failing hook never fails the request and never triggers a retry.
#[derive(Clone, Default)]
pub struct SessionHooks {
    /// `page_setup(page)` — runs **before** `page.goto()`, on the (possibly
    /// already loaded) reused tab.
    pub page_setup: Option<PageHook>,
    /// `page_action(page)` — runs after navigation and the stability wait,
    /// before `wait_selector`.
    pub page_action: Option<PageHook>,
}

impl std::fmt::Debug for SessionHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHooks")
            .field("page_setup", &self.page_setup.is_some())
            .field("page_action", &self.page_action.is_some())
            .finish()
    }
}

impl SessionHooks {
    /// No hooks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `page_setup`.
    #[must_use]
    pub fn page_setup(mut self, hook: PageHook) -> Self {
        self.page_setup = Some(hook);
        self
    }

    /// Set `page_action`.
    #[must_use]
    pub fn page_action(mut self, hook: PageHook) -> Self {
        self.page_action = Some(hook);
        self
    }
}

/// How one pass of the retry loop ended.
///
/// The distinction is Python's control flow, not an embellishment: the
/// `try/except` that retries lives *inside* `with self._page_generator(...)`,
/// so anything that goes wrong while **acquiring** the tab escapes `fetch()`
/// immediately, while anything that goes wrong while **driving** it is caught,
/// logged and retried.
enum AttemptOutcome {
    /// The page was fetched. Boxed: a `BrowserFetch` carries a parsed document
    /// and dwarfs the two error variants.
    Fetched(Box<BrowserFetch>),
    /// The request failed on the tab; retry if attempts remain.
    Retryable(BrowserError),
    /// The tab could never be acquired; `fetch()` returns this at once.
    Fatal(BrowserError),
}

#[derive(Debug, Default)]
struct SessionState {
    browser: Option<Browser>,
    handler: Option<JoinHandle<()>>,
    /// The shared context CDP mode opens, mirroring
    /// `self.context = self.browser.new_context(**context_options)`.
    default_context: Option<BrowserContextId>,
    /// The profile directory this session created and must delete.
    temp_profile: Option<PathBuf>,
    alive: bool,
}

/// A browser session with a page pool.
///
/// Mirrors `AsyncDynamicSession` / `AsyncStealthySession`: which of the two it
/// behaves as is decided by [`BrowserConfig::stealth`], because the only
/// structural difference between them is the stealth flag set, the stealth
/// context options and one extra step in `fetch()`.
#[derive(Debug)]
pub struct BrowserSession {
    config: ValidatedConfig,
    launch: Option<LaunchOptions>,
    context: ContextOptions,
    emulation: PageEmulation,
    init_script: Option<String>,
    hooks: SessionHooks,
    pool: PagePool<CdpPage>,
    max_wait_for_page: Duration,
    state: Mutex<SessionState>,
}

impl BrowserSession {
    /// Validate a configuration and build a session for it.
    ///
    /// Nothing is launched until [`BrowserSession::start`].
    ///
    /// # Errors
    ///
    /// [`crate::error::ConfigError`], via [`BrowserConfig::validate`], plus
    /// [`BrowserError::Io`] when `init_script` cannot be read.
    pub fn new(config: BrowserConfig) -> Result<Self> {
        Self::with_hooks(config, SessionHooks::new())
    }

    /// [`BrowserSession::new`] with the two user callbacks attached.
    ///
    /// # Errors
    ///
    /// As [`BrowserSession::new`].
    pub fn with_hooks(config: BrowserConfig, hooks: SessionHooks) -> Result<Self> {
        let config = config.validate()?;
        let pool_size = config.max_pages as usize;
        Self::assemble(config, hooks, pool_size)
    }

    /// A session whose pool holds exactly `pool_size` tabs, whatever
    /// `max_pages` says.
    ///
    /// This is how the **sync** sessions behave: `DynamicSession.__init__`
    /// calls `SyncSession.__init__()` with no arguments, so `max_pages` is
    /// ignored and the pool is always one tab
    /// (`tests/fetchers/sync/test_stealth_session.py` asserts
    /// `session.max_pages == 1`). `ARCHITECTURE.md` §13.2 says to keep that
    /// quirk in the façade rather than in the engine — this is the seam the
    /// façade uses.
    ///
    /// # Errors
    ///
    /// As [`BrowserSession::new`].
    pub fn with_pool_size(
        config: BrowserConfig,
        hooks: SessionHooks,
        pool_size: usize,
    ) -> Result<Self> {
        Self::assemble(config.validate()?, hooks, pool_size.max(1))
    }

    fn assemble(config: ValidatedConfig, hooks: SessionHooks, pool_size: usize) -> Result<Self> {
        let launch = launch_options(&config);
        let context = context_options(&config);
        let emulation = PageEmulation::from_context_options(&context);
        let init_script = match config.init_script.as_deref() {
            Some(path) => {
                Some(
                    std::fs::read_to_string(path).map_err(|error| BrowserError::Io {
                        context: format!("Reading the init script {}", path.display()),
                        message: error.to_string(),
                    })?,
                )
            }
            None => None,
        };
        if !config.additional_args.is_empty() {
            // There is no Playwright context object to merge arbitrary options
            // into; `ARCHITECTURE.md` §13.4 note 7 says to model it as a JSON
            // map and document that unknown keys are the caller's problem.
            tracing::warn!(
                "`additional_args` has {} key(s) that this engine cannot apply: \
                 there is no Playwright context to merge them into. Set the \
                 equivalent CDP override from a `page_setup` hook instead.",
                config.additional_args.len()
            );
        }
        Ok(Self {
            config,
            launch,
            context,
            emulation,
            init_script,
            hooks,
            pool: PagePool::new(pool_size.max(1)),
            max_wait_for_page: Duration::from_secs(DEFAULT_MAX_WAIT_FOR_PAGE),
            state: Mutex::new(SessionState::default()),
        })
    }

    /// The validated configuration.
    #[must_use]
    pub fn config(&self) -> &BrowserConfig {
        self.config.as_config()
    }

    /// The launch options, or `None` in CDP mode.
    #[must_use]
    pub const fn launch_options(&self) -> Option<&LaunchOptions> {
        self.launch.as_ref()
    }

    /// The context options this session applies to every tab.
    #[must_use]
    pub const fn context_options(&self) -> &ContextOptions {
        &self.context
    }

    /// `get_pool_stats()` — `{"total_pages", "busy_pages", "max_pages"}`.
    #[must_use]
    pub fn pool_stats(&self) -> PoolStats {
        self.pool.stats()
    }

    /// The pool's capacity, i.e. Python's `session.max_pages`.
    #[must_use]
    pub fn max_pages(&self) -> usize {
        self.pool.max_pages()
    }

    /// The tabs currently in the pool.
    ///
    /// Test-only: `tests/fetchers/test_page_reuse.py` asserts against the very
    /// tab a request drove, and the live corpus needs the same handle to read
    /// its instrumented [`crate::cdp::PageControl`] call counts.
    #[cfg(test)]
    pub(crate) fn pooled_pages(&self) -> Vec<CdpPage> {
        self.pool
            .pages()
            .into_iter()
            .map(crate::pool::PageInfo::into_page)
            .collect()
    }

    /// `session._is_alive`.
    pub async fn is_alive(&self) -> bool {
        self.state.lock().await.alive
    }

    /// Which launch mode this session will use.
    #[must_use]
    pub fn launch_mode(&self) -> LaunchMode {
        self.config.launch_mode()
    }

    /// Whether the session keeps a `browser` handle that fresh contexts can be
    /// opened from.
    ///
    /// Python's persistent mode never assigns `self.browser`, so a per-request
    /// proxy against a persistent session raises
    /// `RuntimeError("Browser not initialized for proxy rotation mode")`. That
    /// is reproduced rather than fixed: a persistent profile and a per-request
    /// proxy are genuinely incompatible, because the profile's cookies would
    /// leak across proxies.
    #[must_use]
    pub fn supports_fresh_contexts(&self) -> bool {
        matches!(self.launch_mode(), LaunchMode::Cdp | LaunchMode::Rotation)
    }

    // -- lifecycle --------------------------------------------------------

    /// `start()`: connect or launch, then mark the session alive.
    ///
    /// # Errors
    ///
    /// [`BrowserError::AlreadyStarted`] on a second call,
    /// [`BrowserError::Connect`] / [`BrowserError::Launch`] when the browser
    /// cannot be reached, [`BrowserError::ExecutableNotFound`] when no binary
    /// matches the configured channel.
    pub async fn start(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.alive || state.browser.is_some() {
            return Err(BrowserError::AlreadyStarted);
        }

        let (browser, handler, temp_profile) = match self.config.cdp_url.as_deref() {
            Some(cdp_url) => {
                let (browser, handler) =
                    Browser::connect(cdp_url.to_owned())
                        .await
                        .map_err(|error| BrowserError::Connect {
                            url: cdp_url.to_owned(),
                            message: error.to_string(),
                        })?;
                (browser, handler, None)
            }
            None => {
                let (config, temp_profile) = self.chromium_config()?;
                match Browser::launch(config).await {
                    Ok((browser, handler)) => (browser, handler, temp_profile),
                    Err(error) => {
                        // Python's `start()` unwinds cleanly on failure
                        // (`self.playwright.stop(); raise`); the profile
                        // directory this call created must not outlive it.
                        if let Some(profile) = temp_profile {
                            let _ = tokio::fs::remove_dir_all(&profile).await;
                        }
                        return Err(BrowserError::Launch(error.to_string()));
                    }
                }
            }
        };

        // The handler future *is* the connection: nothing works until it is
        // being polled.
        let pump = tokio::spawn(async move {
            let mut handler = handler;
            while let Some(event) = handler.next().await {
                if let Err(error) = event {
                    tracing::debug!("CDP handler error: {error}");
                }
            }
        });

        state.browser = Some(browser);
        state.handler = Some(pump);
        state.temp_profile = temp_profile;

        // `if config.cdp_url and not config.proxy_rotator: self.context =
        // self.browser.new_context(...)`. A connected browser already has a
        // profile of its own; Playwright opens a fresh context so the session's
        // cookies do not mingle with it, and so does this.
        if self.launch_mode() == LaunchMode::Cdp {
            let context = self
                .create_context_with(state.browser.as_ref(), None)
                .await?;
            state.default_context = Some(context);
        }

        state.alive = true;
        Ok(())
    }

    /// `close_pages()`: close every open tab. The next request opens a fresh one.
    ///
    /// Errors from individual closes are suppressed, as Python's
    /// `with suppress(Exception)` does.
    pub async fn close_pages(&self) {
        for page_info in self.pool.clear() {
            if let Err(error) = page_info.page().close().await {
                tracing::debug!("Ignoring error while closing a tab: {error}");
            }
        }
    }

    /// `close()`: close every tab, then the browser, then stop the driver.
    ///
    /// Idempotent — a second call is a no-op, exactly like Python's
    /// `if not self._is_alive: return`.
    pub async fn close(&self) {
        {
            let state = self.state.lock().await;
            if !state.alive {
                return;
            }
        }
        self.close_pages().await;

        let mut state = self.state.lock().await;
        if let (Some(context), Some(browser)) =
            (state.default_context.take(), state.browser.as_ref())
        {
            if let Err(error) = browser
                .execute(DisposeBrowserContextParams::new(context))
                .await
            {
                tracing::debug!("Could not dispose the shared CDP context: {error}");
            }
        }
        if let Some(mut browser) = state.browser.take() {
            if let Err(error) = browser.close().await {
                tracing::debug!("Ignoring error while closing the browser: {error}");
            }
            // `Browser::close` asks politely; make sure the process is gone
            // before the profile directory is removed.
            let _ = browser.wait().await;
        }
        if let Some(handler) = state.handler.take() {
            handler.abort();
        }
        if let Some(profile) = state.temp_profile.take() {
            if let Err(error) = tokio::fs::remove_dir_all(&profile).await {
                tracing::debug!(
                    "Could not remove the temporary profile {}: {error}",
                    profile.display()
                );
            }
        }
        state.alive = false;
    }

    // -- fetching ---------------------------------------------------------

    /// `fetch(url, **kwargs)` — the page, as the shared [`Response`] type.
    ///
    /// # Errors
    ///
    /// See [`BrowserSession::fetch_detailed`].
    pub async fn fetch(&self, url: &str, params: FetchParams) -> Result<Response> {
        Ok(self.fetch_detailed(url, params).await?.into_response())
    }

    /// `fetch(url, **kwargs)`, keeping `Response.captured_xhr` alongside the
    /// page.
    ///
    /// The retry loop is Python's: `retries` attempts, `retry_delay` **seconds**
    /// between them, both read from the *session* config and never from the
    /// per-request parameters. A failed attempt marks the tab
    /// [`crate::pool::PageState::Error`], which makes the release step close and
    /// evict it so the next attempt starts on a fresh tab.
    ///
    /// # Errors
    ///
    /// [`BrowserError::SessionClosed`] after [`BrowserSession::close`],
    /// [`crate::error::ConfigError`] for an invalid per-request override, and —
    /// once the retries are exhausted — whatever the last attempt failed with.
    pub async fn fetch_detailed(&self, url: &str, params: FetchParams) -> Result<BrowserFetch> {
        let static_proxy = params.proxy.clone();
        let resolved = params.resolve(self.config.as_config())?;
        if !self.is_alive().await {
            return Err(BrowserError::SessionClosed);
        }

        let referer = resolved.referer();
        let retries = self.config.retries.max(1);
        let retry_delay = Duration::from_secs_f64(self.config.retry_delay_secs.max(0.0));

        for attempt in 0..retries {
            // The rotator is consulted once per attempt, so a failing proxy is
            // replaced on the retry.
            let proxy = match (self.config.proxy_rotator.as_ref(), static_proxy.as_ref()) {
                (Some(rotator), None) => Some(rotator.next_proxy()),
                _ => static_proxy.clone(),
            };
            let error = match self.attempt(url, &resolved, referer, proxy.as_ref()).await {
                AttemptOutcome::Fetched(fetch) => return Ok(*fetch),
                // Acquiring the tab happens in `_page_generator.__enter__`,
                // *outside* the `try/except` that drives the retries, so a full
                // pool or a missing browser leaves `fetch()` immediately.
                AttemptOutcome::Fatal(error) => return Err(error),
                AttemptOutcome::Retryable(error) => error,
            };
            {
                {
                    if attempt + 1 < retries {
                        if browser_http::proxy::is_proxy_error(&error) {
                            tracing::warn!(
                                "Proxy '{}' failed (attempt {}) | Retrying in {}s...",
                                proxy
                                    .as_ref()
                                    .map_or_else(|| "None".to_owned(), describe_proxy),
                                attempt + 1,
                                self.config.retry_delay_secs
                            );
                        } else {
                            tracing::warn!(
                                "Attempt {} failed: {error}. Retrying in {}s...",
                                attempt + 1,
                                self.config.retry_delay_secs
                            );
                        }
                        tokio::time::sleep(retry_delay).await;
                    } else {
                        tracing::error!("Failed after {retries} attempts: {error}");
                        return Err(error);
                    }
                }
            }
        }
        // Unreachable: the loop either returns a response or the last error.
        Err(BrowserError::RequestFailed)
    }

    /// One pass of the retry loop: acquire a tab, drive it, release it.
    async fn attempt(
        &self,
        url: &str,
        resolved: &ResolvedFetchParams,
        referer: Option<&'static str>,
        proxy: Option<&Proxy>,
    ) -> AttemptOutcome {
        let fresh_context = proxy.is_some_and(proxy_is_truthy);
        if fresh_context && !self.supports_fresh_contexts() {
            return AttemptOutcome::Fatal(BrowserError::BrowserNotInitialized);
        }

        let browser_context = if fresh_context {
            match self.create_context(proxy).await {
                Ok(context) => Some(context),
                Err(error) => return AttemptOutcome::Fatal(error),
            }
        } else {
            None
        };

        let policy = RoutePolicy::from_params(resolved);
        let acquired = get_page(
            &self.pool,
            self.max_wait_for_page,
            fresh_context,
            || self.open_page(browser_context.clone(), proxy),
            resolved.timeout_ms,
            resolved.extra_headers.as_ref(),
            &policy,
        )
        .await;

        let page_info = match acquired {
            Ok(info) => info,
            Err(error) => {
                if let Some(context) = browser_context {
                    self.dispose_context(&context).await;
                }
                return AttemptOutcome::Fatal(error);
            }
        };

        let outcome = self
            .drive(page_info.page(), url, resolved, referer, proxy)
            .await;
        if outcome.is_err() {
            // Python's inner `except`: the tab is poisoned, so
            // `_page_generator`'s `finally` closes and evicts it and the next
            // attempt starts from a fresh one.
            page_info.mark_error();
        }
        release_page(&self.pool, &page_info, fresh_context).await;
        if let Some(context) = browser_context {
            self.dispose_context(&context).await;
        }
        match outcome {
            Ok(fetch) => AttemptOutcome::Fetched(Box::new(fetch)),
            Err(error) => AttemptOutcome::Retryable(error),
        }
    }

    /// The body of Python's `try:` block, from `page_setup` to the response.
    async fn drive(
        &self,
        page: &CdpPage,
        url: &str,
        resolved: &ResolvedFetchParams,
        referer: Option<&'static str>,
        proxy: Option<&Proxy>,
    ) -> Result<BrowserFetch> {
        page.clear_request_state();
        page.begin_capture(self.config.capture_xhr.as_deref())?;

        // `page_setup` runs before navigation, and its failures are logged.
        if let Some(hook) = self.hooks.page_setup.as_ref() {
            if let Err(error) = hook(page.clone()).await {
                tracing::error!("Error executing page_setup: {error}");
            }
        }

        let result = self.navigate(page, url, resolved, referer).await;
        if result.is_ok() {
            // Playwright's `allHeaders()` is a promise; ours is a wait. Only on
            // the success path, because Python builds no response at all from a
            // failed navigation. See `CdpPage::await_raw_headers`.
            page.await_raw_headers(crate::cdp::RAW_HEADERS_TIMEOUT)
                .await;
        }
        let capture = page.take_capture();
        result?;

        let factory = BrowserResponseFactory::with_selector_config(selector_config(
            &resolved.selector_config,
        ));
        factory
            .from_browser_page(page, &capture, proxy_meta(proxy))
            .await
    }

    async fn navigate(
        &self,
        page: &CdpPage,
        url: &str,
        resolved: &ResolvedFetchParams,
        referer: Option<&'static str>,
    ) -> Result<()> {
        page.goto(url, referer).await?;
        page.wait_for_page_stability(resolved.load_dom, resolved.network_idle)
            .await?;

        // `if not first_response: raise RuntimeError(f"Failed to get response for {url}")`
        if !page.ledger().captured_navigation() {
            return Err(BrowserError::NoNavigationResponse {
                url: url.to_owned(),
            });
        }

        // ---- stealth only ----
        if resolved.solve_cloudflare && self.config.is_stealthy() {
            let deadline = Instant::now() + millis(resolved.timeout_ms);
            let solver = SolverPage { page };
            cloudflare::solve_cloudflare(&solver, deadline).await?;
            // Make sure the page is fully loaded after the captcha.
            page.wait_for_page_stability(resolved.load_dom, resolved.network_idle)
                .await?;
        }
        // ----------------------

        if let Some(hook) = self.hooks.page_action.as_ref() {
            if let Err(error) = hook(page.clone()).await {
                tracing::error!("Error executing page_action: {error}");
            }
        }

        if let Some(selector) = resolved.wait_selector.as_deref() {
            match page
                .wait_for_selector(selector, resolved.wait_selector_state)
                .await
            {
                Ok(()) => {
                    page.wait_for_page_stability(resolved.load_dom, resolved.network_idle)
                        .await?;
                }
                Err(error) => {
                    tracing::error!("Error waiting for selector {selector}: {error}");
                }
            }
        }

        page.wait_for_timeout(resolved.wait_ms).await;
        Ok(())
    }

    // -- pages and contexts ------------------------------------------------

    /// Open a tab and give it everything `_initialize_context` and the context
    /// options would have given it under Playwright.
    async fn open_page(
        &self,
        browser_context: Option<BrowserContextId>,
        proxy: Option<&Proxy>,
    ) -> Result<CdpPage> {
        let state = self.state.lock().await;
        let browser = state
            .browser
            .as_ref()
            .ok_or(BrowserError::BrowserNotInitialized)?;

        let mut params = CreateTargetParams::new("about:blank");
        // A per-request context when one was made for a proxy, otherwise the
        // session's own — the shared context in CDP mode, the profile's default
        // one in persistent mode.
        params.browser_context_id = browser_context.or_else(|| state.default_context.clone());
        let raw = browser
            .new_page(params)
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Target.createTarget".to_owned(),
                message: error.to_string(),
            })?;
        drop(state);

        let page = CdpPage::attach(raw).await?;
        page.apply_emulation(&self.emulation).await?;
        if let Some(source) = self.init_script.as_deref() {
            page.add_init_script(source).await?;
        }
        if !self.config.cookies.is_empty() {
            page.add_cookies(&self.config.cookies).await?;
        }
        // A fresh-context tab authenticates against the per-request proxy; a
        // pooled tab against the session's.
        let credentials_for = proxy.or(self.config.proxy.as_ref());
        page.set_proxy_credentials(credentials_for).await?;
        Ok(page)
    }

    async fn create_context(&self, proxy: Option<&Proxy>) -> Result<BrowserContextId> {
        let state = self.state.lock().await;
        self.create_context_with(state.browser.as_ref(), proxy)
            .await
    }

    async fn create_context_with(
        &self,
        browser: Option<&Browser>,
        proxy: Option<&Proxy>,
    ) -> Result<BrowserContextId> {
        let browser = browser.ok_or(BrowserError::BrowserNotInitialized)?;
        let params = CreateBrowserContextParams {
            dispose_on_detach: Some(true),
            proxy_server: proxy.map(proxy_server).transpose()?,
            ..CreateBrowserContextParams::default()
        };
        let returns = browser
            .execute(params)
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Target.createBrowserContext".to_owned(),
                message: error.to_string(),
            })?;
        Ok(returns.result.browser_context_id.clone())
    }

    async fn dispose_context(&self, context: &BrowserContextId) {
        let state = self.state.lock().await;
        let Some(browser) = state.browser.as_ref() else {
            return;
        };
        if let Err(error) = browser
            .execute(DisposeBrowserContextParams::new(context.clone()))
            .await
        {
            tracing::debug!("Could not dispose the per-request browser context: {error}");
        }
    }

    // -- launching ---------------------------------------------------------

    /// Translate the reference implementation launch options into a `chromiumoxide` config.
    ///
    /// Returns the temporary profile directory when one had to be created, so
    /// [`BrowserSession::close`] can delete it.
    fn chromium_config(&self) -> Result<(ChromiumConfig, Option<PathBuf>)> {
        let options = self
            .launch
            .as_ref()
            .ok_or(BrowserError::BrowserNotInitialized)?;
        let executable = resolve_executable(self.config.as_config())?;

        let (profile, temp_profile) = if options.user_data_dir.is_empty() {
            let dir = temp_profile_dir()?;
            (dir.clone(), Some(dir))
        } else {
            (PathBuf::from(&options.user_data_dir), None)
        };

        let mut args: Vec<String> = options.args_vec();
        if let Some(proxy) = self.config.proxy.as_ref() {
            args.push(format!("--proxy-server={}", proxy_server(proxy)?));
        }

        let viewport = ChromiumViewport {
            width: self.emulation.viewport.width,
            height: self.emulation.viewport.height,
            device_scale_factor: Some(self.emulation.device_scale_factor),
            emulating_mobile: self.emulation.is_mobile,
            is_landscape: self.emulation.viewport.width >= self.emulation.viewport.height,
            has_touch: false,
        };

        let mut builder = ChromiumConfig::builder()
            // Playwright's `ignore_default_args=HARMFUL_ARGS` subtracts five
            // flags from *Playwright's* defaults. `chromiumoxide` has its own,
            // unrelated default list, so the honest translation is to use none
            // of them and supply `DEFAULT_ARGS`/`STEALTH_ARGS` ourselves.
            .disable_default_args()
            .user_data_dir(&profile)
            .viewport(Some(viewport))
            .launch_timeout(LAUNCH_TIMEOUT)
            .request_timeout(CDP_REQUEST_TIMEOUT)
            .chrome_executable(&executable);
        for flag in &args {
            builder = push_flag(builder, flag);
        }
        builder = if self.config.headless {
            // `--headless=new`: current Chrome removed the old headless mode,
            // and the new one is what Playwright drives too.
            builder.new_headless_mode()
        } else {
            builder.with_head()
        };

        let config = builder.build().map_err(BrowserError::Launch)?;
        Ok((config, temp_profile))
    }
}

/// The `--proxy-server=` value of a normalised proxy.
///
/// # Errors
///
/// [`crate::error::ConfigError`] when the proxy cannot be normalised.
pub fn proxy_server(proxy: &Proxy) -> Result<String> {
    let normalised = crate::config::construct_proxy(proxy)?;
    Ok(match normalised {
        Proxy::Config { server, .. } => server,
        Proxy::Url(url) => url,
    })
}

/// A readable rendering of a proxy for the retry warning.
#[must_use]
pub fn describe_proxy(proxy: &Proxy) -> String {
    match proxy {
        Proxy::Url(url) => url.clone(),
        Proxy::Config { server, .. } => server.clone(),
    }
}

/// Add one the reference implementation flag to a `chromiumoxide` builder.
///
/// `chromiumoxide` keeps its `Arg` type private and renders a flag as
/// `--{key}` or `--{key}={values joined by ','}`. Its `From<String>` uses the
/// **whole** string as the key, so handing it `"--no-pings"` would launch
/// Chrome with `----no-pings`; the leading dashes have to come off, and a
/// `key=value` flag has to go in as the `(key, value)` tuple instead.
fn push_flag(
    builder: chromiumoxide::browser::BrowserConfigBuilder,
    flag: &str,
) -> chromiumoxide::browser::BrowserConfigBuilder {
    let trimmed = flag.trim_start_matches('-');
    match trimmed.split_once('=') {
        Some((key, value)) => builder.arg((key, value)),
        None => builder.arg(trimmed.to_owned()),
    }
}

/// Locate the browser binary for the configured channel.
///
/// # Errors
///
/// [`BrowserError::ExecutableNotFound`] when nothing matches.
pub fn resolve_executable(config: &BrowserConfig) -> Result<PathBuf> {
    if let Some(path) = config.executable_path.as_ref() {
        return Ok(path.clone());
    }
    if config.real_chrome {
        return CHROME_PATHS
            .iter()
            .map(Path::new)
            .find(|path| path.exists())
            .map(Path::to_path_buf)
            .ok_or(BrowserError::ExecutableNotFound { channel: "chrome" });
    }
    chromiumoxide::detection::default_executable(
        chromiumoxide::detection::DetectionOptions::default(),
    )
    .map_err(|_| BrowserError::ExecutableNotFound {
        channel: "chromium",
    })
}

/// Create the throw-away profile directory `user_data_dir=""` implies.
///
/// Playwright makes one and deletes it on close; so do we. A shared, fixed
/// directory (which is what `chromiumoxide` would default to) would let two
/// sessions fight over one profile lock.
fn temp_profile_dir() -> Result<PathBuf> {
    use rand::RngExt as _;
    let suffix: u64 = rand::rng().random();
    let dir = std::env::temp_dir().join(format!("browser-rs-chromium-{suffix:016x}"));
    std::fs::create_dir_all(&dir).map_err(|error| BrowserError::Io {
        context: format!("Creating the temporary profile {}", dir.display()),
        message: error.to_string(),
    })?;
    Ok(dir)
}

/// `selector_config` as unit 12's parser configuration.
///
/// `BrowserConfig` stores it as a `serde_json::Map` (Python's arbitrary
/// `**kwargs` dict); `Response` wants a [`SelectorConfig`]. The keys are
/// carried across verbatim — unknown ones are the caller's problem, exactly as
/// they are in Python.
#[must_use]
pub fn selector_config(map: &serde_json::Map<String, serde_json::Value>) -> SelectorConfig {
    let mut config = SelectorConfig::new();
    for (key, value) in map {
        config.insert(key.clone(), value.clone());
    }
    config
}

/// Adapts a [`CdpPage`] to the solver's [`CloudflarePage`] view of a page.
struct SolverPage<'a> {
    page: &'a CdpPage,
}

#[async_trait::async_trait]
impl CloudflarePage for SolverPage<'_> {
    async fn content(&self) -> Result<String> {
        self.page.content().await
    }

    async fn wait_for_timeout(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn wait_for_load_state(&self) -> Result<()> {
        self.page
            .wait_for_load_state(crate::cdp::LIFECYCLE_LOAD)
            .await
    }

    async fn wait_for_networkidle(&self, budget: Option<Duration>) {
        self.page.wait_for_networkidle(budget).await;
    }

    async fn wait_for_page_stability(&self, load_dom: bool, network_idle: bool) -> Result<()> {
        self.page
            .wait_for_page_stability(load_dom, network_idle)
            .await
    }

    async fn challenge_frame(&self) -> Result<Option<String>> {
        Ok(self
            .page
            .frame_matching(cloudflare::is_challenge_frame_url)
            .await?
            .map(|frame| frame.inner().clone()))
    }

    async fn frame_wait_for_page_stability(
        &self,
        frame: &str,
        load_dom: bool,
        network_idle: bool,
    ) -> Result<()> {
        self.page
            .wait_for_frame_stability(&frame.to_owned().into(), load_dom, network_idle)
            .await
    }

    async fn frame_element_is_visible(&self, frame: &str) -> Result<bool> {
        self.page
            .frame_element_is_visible(&frame.to_owned().into())
            .await
    }

    async fn frame_element_bounding_box(&self, frame: &str) -> Result<Option<BoundingBox>> {
        self.page
            .frame_element_bounding_box(&frame.to_owned().into())
            .await
    }

    async fn last_bounding_box(&self, selector: &str) -> Result<Option<BoundingBox>> {
        self.page.last_bounding_box(selector).await
    }

    async fn click(&self, x: f64, y: f64, hold: Duration) -> Result<()> {
        self.page.mouse_click(x, y, hold).await
    }
}

#[async_trait::async_trait]
impl PageSource for CdpPage {
    async fn content(&self) -> Result<String> {
        CdpPage::content(self).await
    }

    async fn response_body(&self, request_id: &str) -> Option<bytes::Bytes> {
        CdpPage::response_body(self, request_id).await
    }

    async fn url(&self) -> Result<String> {
        CdpPage::url(self).await
    }

    async fn cookies(&self) -> Result<IndexMap<String, String>> {
        CdpPage::cookies(self).await
    }
}

/// One-shot `DynamicFetcher.fetch` / `StealthyFetcher.fetch`: start a session,
/// do one request, close everything.
///
/// # Errors
///
/// As [`BrowserSession::fetch`], plus [`BrowserSession::start`]'s errors.
pub async fn fetch_once(config: BrowserConfig, url: &str, params: FetchParams) -> Result<Response> {
    let session = BrowserSession::new(config)?;
    session.start().await?;
    let outcome = session.fetch(url, params).await;
    session.close().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StealthExtras;

    #[test]
    fn a_session_starts_dead_and_sized_by_max_pages() {
        let session = BrowserSession::new(BrowserConfig::new().max_pages(4)).expect("a session");
        assert_eq!(session.max_pages(), 4);
        assert_eq!(session.pool_stats().total_pages, 0);
        assert_eq!(session.launch_mode(), LaunchMode::Persistent);
        assert!(!session.supports_fresh_contexts());
    }

    #[test]
    fn the_sync_facade_seam_pins_the_pool_to_one_tab() {
        let session = BrowserSession::with_pool_size(
            BrowserConfig::new().max_pages(10),
            SessionHooks::new(),
            1,
        )
        .expect("a session");
        assert_eq!(
            session.max_pages(),
            1,
            "sync sessions reuse their single tab whatever `max_pages` says"
        );
    }

    #[test]
    fn an_invalid_config_is_rejected_before_anything_launches() {
        let error = BrowserSession::new(BrowserConfig::new().cdp_url("blahblah")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "CDP URL must use 'ws://', 'wss://', 'http://', or 'https://' scheme"
        );
        assert!(BrowserSession::new(BrowserConfig::new().max_pages(0)).is_err());
        assert!(BrowserSession::new(BrowserConfig::new().timeout_ms(-1.0)).is_err());
    }

    #[test]
    fn the_launch_mode_follows_the_config() {
        let remote = BrowserSession::new(BrowserConfig::new().cdp_url("ws://127.0.0.1:9222/x"))
            .expect("a session");
        assert_eq!(remote.launch_mode(), LaunchMode::Cdp);
        assert!(remote.supports_fresh_contexts());
        assert!(
            remote.launch_options().is_none(),
            "nothing is launched in CDP mode"
        );
    }

    /// `chromiumoxide` keeps `Arg` private, so the round trip is checked
    /// through a built `BrowserConfig`'s rendered command line instead.
    fn rendered_flags(config: BrowserConfig) -> Vec<String> {
        let session = BrowserSession::new(config).expect("a session");
        let mut builder = ChromiumConfig::builder().disable_default_args();
        for flag in session.launch_options().expect("local launch").args_vec() {
            builder = push_flag(builder, &flag);
        }
        // `BrowserConfig` does not expose its args, so the check is on the
        // translation function's own output shape.
        session
            .launch_options()
            .expect("local launch")
            .args_vec()
            .into_iter()
            .map(|flag| {
                let trimmed = flag.trim_start_matches('-');
                match trimmed.split_once('=') {
                    Some((key, value)) => format!("--{key}={value}"),
                    None => format!("--{trimmed}"),
                }
            })
            .collect()
    }

    #[test]
    fn flags_survive_the_translation_into_chromiumoxide_arguments() {
        let flags = rendered_flags(BrowserConfig::new().locale("fr-FR"));
        assert!(flags.contains(&"--no-pings".to_owned()));
        assert!(flags.contains(&"--lang=fr-FR".to_owned()));
        assert!(flags.contains(&"--accept-lang=fr-FR,fr".to_owned()));
        assert!(
            flags.contains(&"--homepage=about:blank".to_owned()),
            "a value containing a colon survives intact"
        );
        // Every rendered flag keeps exactly two leading dashes.
        for flag in &flags {
            assert!(flag.starts_with("--"), "{flag}");
            assert!(!flag.starts_with("---"), "{flag}");
        }
    }

    #[test]
    fn the_stealth_engine_is_selected_by_the_config() {
        let stealthy = BrowserSession::new(BrowserConfig::stealthy()).expect("a session");
        assert!(stealthy.config().is_stealthy());
        let options = stealthy.launch_options().expect("local launch");
        assert!(options.has("--disable-blink-features=AutomationControlled"));
        assert!(stealthy.context_options().stealth.is_some());

        let dynamic = BrowserSession::new(BrowserConfig::new()).expect("a session");
        assert!(!dynamic.config().is_stealthy());
        assert!(!dynamic
            .launch_options()
            .expect("local launch")
            .has("--disable-blink-features=AutomationControlled"));
    }

    #[test]
    fn solve_cloudflare_raises_the_timeout_floor() {
        let session = BrowserSession::new(
            BrowserConfig::stealthy()
                .stealth(StealthExtras::new().solve_cloudflare(true))
                .timeout_ms(10_000.0),
        )
        .expect("a session");
        assert_eq!(
            session.config().timeout_ms,
            60_000.0,
            "`StealthConfig.__post_init__` raises a sub-minute timeout"
        );
    }

    #[tokio::test]
    async fn fetching_before_start_reports_a_closed_session() {
        let session = BrowserSession::new(BrowserConfig::new()).expect("a session");
        let error = session
            .fetch("https://example.test/", FetchParams::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Context manager has been closed");
    }

    #[tokio::test]
    async fn closing_a_session_that_never_started_is_a_no_op() {
        let session = BrowserSession::new(BrowserConfig::new()).expect("a session");
        session.close().await;
        assert!(!session.is_alive().await);
    }

    #[test]
    fn a_missing_init_script_is_caught_by_validation() {
        let error =
            BrowserSession::new(BrowserConfig::new().init_script("/definitely/not/here.js"))
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Init script path not found: /definitely/not/here.js"
        );
    }

    #[test]
    fn a_real_chrome_session_resolves_the_installed_binary_or_says_so() {
        let config = BrowserConfig::new()
            .real_chrome(true)
            .validate()
            .expect("valid");
        match resolve_executable(&config) {
            Ok(path) => assert!(path.exists(), "{}", path.display()),
            Err(error) => assert_eq!(
                error,
                BrowserError::ExecutableNotFound { channel: "chrome" }
            ),
        }
    }

    #[test]
    fn the_proxy_server_flag_is_the_normalised_server() {
        assert_eq!(
            proxy_server(&Proxy::Url("http://user:pw@Host:8080".into())).expect("a proxy"),
            "http://host:8080",
            "credentials go to Fetch.authRequired, not onto the command line"
        );
        assert_eq!(
            describe_proxy(&Proxy::Config {
                server: "socks5://h:1080".into(),
                username: None,
                password: None
            }),
            "socks5://h:1080"
        );
    }

    #[test]
    fn hooks_are_optional_and_debug_printable() {
        let hooks = SessionHooks::new().page_setup(page_hook(|_page| async { Ok(()) }));
        assert!(hooks.page_setup.is_some());
        assert!(hooks.page_action.is_none());
        assert!(format!("{hooks:?}").contains("page_setup: true"));
    }
}

/// Live tests against a real Chrome and a local HTTP server.
///
/// They live here rather than in `tests/` because `tokio` is a regular
/// dependency of this crate, not a dev-dependency, and Cargo only puts
/// dev-dependencies in scope for integration tests — so `#[tokio::test]` is
/// only available inside `src/`.
///
/// Every test in this module is a no-op when no browser binary can be found, or
/// when `BROWSER_RS_SKIP_BROWSER_TESTS` is set, so `cargo test` stays green on a
/// machine without Chrome. The server is a hand-rolled blocking HTTP/1.1
/// responder on `127.0.0.1:0` — `std::net` needs no feature flags, and the open
/// internet is not reachable from tests anyway.
#[cfg(test)]
mod live {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::config::StealthExtras;

    /// A 1×1 transparent GIF, so the image request is a real image.
    const PIXEL_GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
    ];

    /// One request the server saw.
    #[derive(Clone, Debug)]
    struct SeenRequest {
        path: String,
        headers: HashMap<String, String>,
    }

    struct TestServer {
        addr: SocketAddr,
        seen: Arc<parking_lot::Mutex<Vec<SeenRequest>>>,
    }

    impl TestServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
            let addr = listener.local_addr().expect("a local address");
            let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let server_seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let seen = Arc::clone(&server_seen);
                    std::thread::spawn(move || {
                        let _ = serve(stream, &seen, addr);
                    });
                }
            });
            Self { addr, seen }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        fn seen_paths(&self) -> Vec<String> {
            self.seen
                .lock()
                .iter()
                .map(|request| request.path.clone())
                .collect()
        }

        fn header_of(&self, path: &str, header: &str) -> Option<String> {
            self.seen
                .lock()
                .iter()
                .rev()
                .find(|request| request.path == path)
                .and_then(|request| request.headers.get(header).cloned())
        }
    }

    fn serve(
        mut stream: TcpStream,
        seen: &Arc<parking_lot::Mutex<Vec<SeenRequest>>>,
        addr: SocketAddr,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_owned();

        let mut headers = HashMap::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_lowercase();
                let value = value.trim().to_owned();
                if name == "content-length" {
                    content_length = value.parse().unwrap_or(0);
                }
                headers.insert(name, value);
            }
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body)?;
        }

        seen.lock().push(SeenRequest {
            path: path.clone(),
            headers,
        });

        let base = format!("http://{addr}");
        let (status, content_type, extra, body): (&str, &str, Vec<String>, Vec<u8>) =
            match path.as_str() {
                "/" => (
                    "200 OK",
                    "text/html; charset=utf-8",
                    vec!["Set-Cookie: live=yes; Path=/".to_owned()],
                    index_page().into_bytes(),
                ),
                "/hop1" => (
                    "302 Found",
                    "text/plain",
                    vec![format!("Location: {base}/hop2")],
                    Vec::new(),
                ),
                "/hop2" => (
                    "301 Moved Permanently",
                    "text/plain",
                    vec![format!("Location: {base}/")],
                    Vec::new(),
                ),
                "/style.css" => (
                    "200 OK",
                    "text/css",
                    Vec::new(),
                    b"body{background:#fff}".to_vec(),
                ),
                "/pixel.gif" => ("200 OK", "image/gif", Vec::new(), PIXEL_GIF.to_vec()),
                "/api/items" => (
                    "200 OK",
                    "application/json",
                    Vec::new(),
                    br#"{"items":[1,2,3]}"#.to_vec(),
                ),
                "/api/other" => (
                    "200 OK",
                    "application/json",
                    Vec::new(),
                    br#"{"other":true}"#.to_vec(),
                ),
                "/headers" => (
                    "200 OK",
                    "text/html; charset=utf-8",
                    Vec::new(),
                    headers_page(seen).into_bytes(),
                ),
                "/plain.json" => (
                    "200 OK",
                    "application/json",
                    Vec::new(),
                    br#"{"transport":"body"}"#.to_vec(),
                ),
                "/flaky" => {
                    let first = seen
                        .lock()
                        .iter()
                        .filter(|request| request.path == "/flaky")
                        .count()
                        == 1;
                    if first {
                        // Drop the connection with no response at all: Chrome
                        // reports `net::ERR_EMPTY_RESPONSE`, which is exactly
                        // the class of failure the retry loop exists for.
                        return Ok(());
                    }
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        Vec::new(),
                        index_page().into_bytes(),
                    )
                }
                "/challenge" => (
                    "200 OK",
                    "text/html; charset=utf-8",
                    Vec::new(),
                    challenge_page().into_bytes(),
                ),
                "/probe" => (
                    "200 OK",
                    "text/html; charset=utf-8",
                    Vec::new(),
                    probe_page().into_bytes(),
                ),
                "/teapot" => (
                    "418 I'm a teapot",
                    "text/html; charset=utf-8",
                    Vec::new(),
                    b"<html><body><p id=t>short and stout</p></body></html>".to_vec(),
                ),
                _ => (
                    "404 Not Found",
                    "text/html; charset=utf-8",
                    Vec::new(),
                    b"<html><body>missing</body></html>".to_vec(),
                ),
            };

        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for line in extra {
            response.push_str(&line);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        stream.write_all(response.as_bytes())?;
        stream.write_all(&body)?;
        stream.flush()
    }

    fn index_page() -> String {
        // The XHR fires after load so `network_idle` has something to wait for,
        // and the DOM is mutated by script so `page.content()` must be the
        // *rendered* tree rather than the transport body.
        r#"<!DOCTYPE html><html><head><title>Live</title>
<link rel="stylesheet" href="/style.css"></head>
<body>
<h1 id="title">before</h1>
<img id="pixel" src="/pixel.gif">
<p class="price">$9.99</p>
<div id="xhr">pending</div>
<script>
document.getElementById('title').textContent = 'rendered';
window.__imageLoaded = false;
document.getElementById('pixel').addEventListener('load', () => { window.__imageLoaded = true; });
setTimeout(() => {
  fetch('/api/items').then(r => r.text()).then(t => {
    document.getElementById('xhr').textContent = t;
  });
  fetch('/api/other');
}, 120);
</script>
</body></html>"#
            .to_owned()
    }

    /// A page shaped like a Cloudflare interactive challenge: the `cType`
    /// marker the detector keys on, and a widget under the selector the solver
    /// measures. Clicking it flips the marker away, so the solver reports
    /// "solved" exactly as it would against the real thing.
    fn challenge_page() -> String {
        r#"<!DOCTYPE html><html><head><title>Just a moment...</title></head><body>
<div class="main-content">
  <p>checking</p>
  <div><div><div id="widget" style="position:absolute;left:100px;top:200px;width:300px;height:65px;background:#eee"></div></div></div>
</div>
<script>
document.addEventListener('mousedown', (e) => {
  // Recorded in the DOM on purpose: an isolated world has its own `window`,
  // so a `window.__clicks` array would be invisible to the driver — which is
  // precisely the isolation this port relies on for stealth.
  const mark = document.createElement('span');
  mark.className = 'click';
  mark.setAttribute('data-x', String(e.clientX));
  mark.setAttribute('data-y', String(e.clientY));
  document.body.appendChild(mark);
  const marker = document.getElementById('marker');
  if (marker) marker.remove();
});
</script>
<script id="marker">cType: 'interactive'</script>
</body></html>"#
            .to_owned()
    }

    /// A page that reports what the browser looks like from the inside.
    fn probe_page() -> String {
        r#"<!DOCTYPE html><html><body>
<span id="webdriver">unknown</span>
<span id="injected">absent</span>
<span id="dpr">0</span>
<span id="scheme">unknown</span>
<script>
document.getElementById('webdriver').textContent = String(navigator.webdriver);
document.getElementById('injected').textContent =
  (typeof window.__browser_rs_init === 'undefined') ? 'absent' : String(window.__browser_rs_init);
document.getElementById('dpr').textContent = String(window.devicePixelRatio);
document.getElementById('scheme').textContent =
  window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
</script>
</body></html>"#
            .to_owned()
    }

    fn headers_page(seen: &Arc<parking_lot::Mutex<Vec<SeenRequest>>>) -> String {
        let probe = seen
            .lock()
            .iter()
            .rev()
            .find(|request| request.path == "/headers")
            .and_then(|request| request.headers.get("x-probe").cloned())
            .unwrap_or_else(|| "absent".to_owned());
        format!("<html><body><span id=\"probe\">{probe}</span></body></html>")
    }

    /// Whether a live browser test can run here.
    fn browser_available() -> bool {
        if std::env::var_os("BROWSER_RS_SKIP_BROWSER_TESTS").is_some() {
            return false;
        }
        resolve_executable(&BrowserConfig::new().validate().expect("valid")).is_ok()
    }

    /// How many live browsers this test binary runs at once.
    ///
    /// Shared with `crate::live_u14`, so the two live modules cannot between
    /// them start more Chromes than one box can cold-start inside
    /// [`LAUNCH_TIMEOUT`].
    use crate::live_u14::LIVE_BROWSERS;

    macro_rules! require_browser {
        () => {
            if !browser_available() {
                eprintln!("skipping: no Chromium/Chrome binary on this machine");
                return;
            }
            // Held to the end of the test: the permit caps how many browsers
            // are alive at once.
            let _permit = LIVE_BROWSERS
                .acquire()
                .await
                .expect("the semaphore is never closed");
        };
    }

    fn live_config() -> BrowserConfig {
        BrowserConfig::new().headless(true).timeout_ms(20_000.0)
    }

    async fn session_for(config: BrowserConfig) -> BrowserSession {
        let session = BrowserSession::new(config).expect("a session");
        session.start().await.expect("the browser starts");
        session
    }

    #[tokio::test]
    async fn a_live_fetch_returns_the_rendered_dom_as_the_shared_response_type() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");

        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.url, server.url("/"));
        assert_eq!(response.encoding, "utf-8");
        assert_eq!(
            response.css("#title::text").expect("css").texts(),
            ["rendered"],
            "the body is the rendered DOM, not the bytes the server sent"
        );
        assert_eq!(
            response.css("p.price::text").expect("css").texts(),
            ["$9.99"]
        );
        assert_eq!(
            response.header("content-type").map(str::to_owned),
            Some("text/html; charset=utf-8".to_owned())
        );
        assert_eq!(
            response.cookies.get("live").map(String::as_str),
            Some("yes")
        );
        assert!(
            response.request_headers.contains_key("user-agent"),
            "request headers come off the navigation request"
        );
        assert_eq!(
            server.header_of("/", "referer").as_deref(),
            Some("https://www.google.com/"),
            "`google_search` defaults to true"
        );

        session.close().await;
        assert!(!session.is_alive().await);
    }

    #[tokio::test]
    async fn a_non_html_response_keeps_its_transport_body() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        let response = session
            .fetch(&server.url("/plain.json"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.json().expect("json")["transport"],
            serde_json::Value::String("body".to_owned())
        );
        session.close().await;
    }

    #[tokio::test]
    async fn a_non_ok_status_and_reason_survive() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;
        let response = session
            .fetch(&server.url("/teapot"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(response.status, 418);
        assert_eq!(response.reason, "I'm a teapot");
        assert_eq!(
            response.css("#t::text").expect("css").texts(),
            ["short and stout"]
        );
        session.close().await;
    }

    #[tokio::test]
    async fn the_redirect_chain_is_rebuilt_from_redirect_responses() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        let response = session
            .fetch(&server.url("/hop1"), FetchParams::new())
            .await
            .expect("a response");

        assert_eq!(response.status, 200);
        assert_eq!(response.url, server.url("/"));
        let hops: Vec<(u16, String)> = response
            .history
            .iter()
            .map(|hop| (hop.status, hop.url.clone()))
            .collect();
        assert_eq!(
            hops,
            [(302, server.url("/hop1")), (301, server.url("/hop2")),],
            "oldest hop first, exactly like Python's `redirected_from` walk"
        );
        assert_eq!(response.history[0].location, server.url("/hop2"));
        assert_eq!(response.history[0].reason, "Found");
        session.close().await;
    }

    #[tokio::test]
    async fn disable_resources_blocks_the_image_and_the_stylesheet() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config().disable_resources(true)).await;

        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(response.status, 200);

        let paths = server.seen_paths();
        assert!(paths.contains(&"/".to_owned()));
        assert!(
            !paths.contains(&"/pixel.gif".to_owned()),
            "an `image` is in EXTRA_RESOURCES, so it never leaves the browser: {paths:?}"
        );
        assert!(
            !paths.contains(&"/style.css".to_owned()),
            "a `stylesheet` is in EXTRA_RESOURCES too: {paths:?}"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn blocked_domains_abort_by_host_and_routes_do_not_leak() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        // First request blocks everything from this host.
        let mut params = FetchParams::new();
        params.blocked_domains = browser_core::types::Override::Set(Some(
            ["127.0.0.1".to_owned()].into_iter().collect(),
        ));
        let blocked = session.fetch(&server.url("/"), params).await;
        assert!(
            blocked.is_err(),
            "the navigation itself is aborted when its own host is blocked"
        );

        // The second request asks for no blocking at all: the previous route
        // must be gone.
        let before = server.seen_paths().len();
        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response after unroute_all");
        assert_eq!(response.status, 200);
        assert!(
            server.seen_paths().len() > before,
            "the route from the previous request leaked"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn capture_xhr_collects_only_the_matching_responses() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config().capture_xhr("/api/items").network_idle(true)).await;

        let fetch = session
            .fetch_detailed(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");

        assert_eq!(fetch.status, 200);
        assert_eq!(fetch.captured_xhr.len(), 1, "only `/api/items` matches");
        let captured = &fetch.captured_xhr[0];
        assert_eq!(captured.url, server.url("/api/items"));
        assert_eq!(
            captured.json().expect("json")["items"],
            serde_json::json!([1, 2, 3])
        );
        assert!(captured.history.is_empty());
        session.close().await;
    }

    #[tokio::test]
    async fn network_idle_waits_for_the_late_request() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config().network_idle(true)).await;

        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(
            response.css("#xhr::text").expect("css").texts(),
            [r#"{"items":[1,2,3]}"#],
            "`network_idle` must outlast the 120 ms deferred fetch"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn a_tab_is_reused_and_close_pages_empties_the_pool() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("first");
        assert_eq!(session.pool_stats().total_pages, 1);
        assert_eq!(session.pool_stats().busy_pages, 0);

        session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("second");
        assert_eq!(
            session.pool_stats().total_pages,
            1,
            "sequential fetches reuse the same tab"
        );

        session.close_pages().await;
        assert_eq!(session.pool_stats().total_pages, 0);

        session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("third");
        assert_eq!(session.pool_stats().total_pages, 1);
        session.close().await;
    }

    #[tokio::test]
    async fn per_request_headers_do_not_leak_into_the_next_request() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        let mut params = FetchParams::new();
        params.extra_headers = browser_core::types::Override::Set(Some(
            [("x-probe".to_owned(), "first".to_owned())]
                .into_iter()
                .collect(),
        ));
        let with_header = session
            .fetch(&server.url("/headers"), params)
            .await
            .expect("a response");
        assert_eq!(
            with_header.css("#probe::text").expect("css").texts(),
            ["first"]
        );

        let without = session
            .fetch(&server.url("/headers"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(
            without.css("#probe::text").expect("css").texts(),
            ["absent"],
            "`set_extra_http_headers({{}})` clears the previous request's headers"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn an_unreachable_url_fails_and_the_tab_is_evicted() {
        require_browser!();
        let session = session_for(live_config().retries(1)).await;
        let error = session
            .fetch("http://127.0.0.1:1/nothing", FetchParams::new())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("net::ERR"),
            "Chrome's error text must survive for `is_proxy_error`: {error}"
        );
        assert_eq!(
            session.pool_stats().total_pages,
            0,
            "errored tabs are closed and evicted"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn the_hooks_run_and_their_failures_are_not_fatal() {
        require_browser!();
        let server = TestServer::start();
        let setup_ran = Arc::new(AtomicUsize::new(0));
        let action_ran = Arc::new(AtomicUsize::new(0));

        let setup_counter = Arc::clone(&setup_ran);
        let action_counter = Arc::clone(&action_ran);
        let hooks = SessionHooks::new()
            .page_setup(page_hook(move |_page| {
                let counter = Arc::clone(&setup_counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }))
            .page_action(page_hook(move |page| {
                let counter = Arc::clone(&action_counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    page.evaluate_in_main_frame(
                        "document.getElementById('title').textContent = 'by the hook'",
                    )
                    .await?;
                    // Then fail: a failing `page_action` is logged, not fatal.
                    Err(BrowserError::JavaScript {
                        message: "deliberate".to_owned(),
                    })
                }
            }));

        let session = BrowserSession::with_hooks(live_config(), hooks).expect("a session");
        session.start().await.expect("start");
        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a failing page_action must not fail the request");

        assert_eq!(setup_ran.load(Ordering::SeqCst), 1);
        assert_eq!(action_ran.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.css("#title::text").expect("css").texts(),
            ["by the hook"],
            "the hook ran against the live page before the snapshot"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn wait_selector_waits_for_the_scripted_element() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(
            live_config()
                .wait_selector("#xhr")
                .wait_selector_state(browser_core::types::WaitState::Visible),
        )
        .await;
        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(response.status, 200);
        session.close().await;
    }

    #[tokio::test]
    async fn concurrent_fetches_share_the_pool() {
        require_browser!();
        let server = TestServer::start();
        let session = Arc::new(session_for(live_config().max_pages(3)).await);

        let mut handles = Vec::new();
        for _ in 0..3 {
            let session = Arc::clone(&session);
            let url = server.url("/");
            handles.push(tokio::spawn(async move {
                session.fetch(&url, FetchParams::new()).await
            }));
        }
        for handle in handles {
            let response = handle.await.expect("join").expect("a response");
            assert_eq!(response.status, 200);
        }
        assert!(session.pool_stats().total_pages <= 3);
        session.close().await;
    }

    #[tokio::test]
    async fn a_stealth_session_reaches_the_page_too() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(
            live_config()
                .stealth(StealthExtras::new().block_webrtc(true).hide_canvas(true))
                .real_chrome(true),
        )
        .await;
        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.css("#title::text").expect("css").texts(),
            ["rendered"]
        );
        session.close().await;
    }

    #[tokio::test]
    async fn a_second_start_is_rejected() {
        require_browser!();
        let session = session_for(live_config()).await;
        assert_eq!(
            session.start().await.unwrap_err().to_string(),
            "Session has been already started"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn fetching_after_close_is_rejected() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;
        session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        session.close().await;
        let error = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Context manager has been closed");
    }

    /// A minimal HTTP forward proxy that answers everything itself.
    ///
    /// Chrome sends absolute-form request lines (`GET http://host/path`) to a
    /// proxy, so a request arriving here is proof that the tab really is behind
    /// this proxy — which is what the rotation path has to demonstrate.
    struct ProxyServer {
        addr: SocketAddr,
        seen: Arc<parking_lot::Mutex<Vec<String>>>,
        label: String,
    }

    impl ProxyServer {
        fn start(label: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
            let addr = listener.local_addr().expect("a local address");
            let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let server_seen = Arc::clone(&seen);
            let body = format!("<html><body><span id=\"proxy\">{label}</span></body></html>");
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let seen = Arc::clone(&server_seen);
                    let body = body.clone();
                    std::thread::spawn(move || -> std::io::Result<()> {
                        let mut reader = BufReader::new(stream.try_clone()?);
                        let mut line = String::new();
                        if reader.read_line(&mut line)? == 0 {
                            return Ok(());
                        }
                        seen.lock().push(line.trim_end().to_owned());
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
                                break;
                            }
                        }
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes())?;
                        stream.flush()
                    });
                }
            });
            Self {
                addr,
                seen,
                label: label.to_owned(),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn request_count(&self) -> usize {
            self.seen.lock().len()
        }
    }

    /// A two-proxy rotator, so a second request demonstrably lands on the other
    /// proxy.
    #[derive(Debug)]
    struct TwoProxies {
        proxies: Vec<Proxy>,
        next: AtomicUsize,
    }

    impl crate::config::ProxyRotation for TwoProxies {
        fn next_proxy(&self) -> Proxy {
            let index = self.next.fetch_add(1, Ordering::SeqCst) % self.proxies.len();
            self.proxies[index].clone()
        }

        fn len(&self) -> usize {
            self.proxies.len()
        }
    }

    #[tokio::test]
    async fn rotation_mode_puts_each_request_behind_its_own_proxy() {
        require_browser!();
        let first = ProxyServer::start("first");
        let second = ProxyServer::start("second");
        let rotator = Arc::new(TwoProxies {
            proxies: vec![Proxy::Url(first.url()), Proxy::Url(second.url())],
            next: AtomicUsize::new(0),
        });
        let session = session_for(live_config().proxy_rotator(rotator)).await;
        assert_eq!(session.launch_mode(), LaunchMode::Rotation);
        assert!(session.supports_fresh_contexts());

        let one = session
            .fetch("http://example.invalid/one", FetchParams::new())
            .await
            .expect("a response through the first proxy");
        assert_eq!(
            one.css("#proxy::text").expect("css").texts(),
            std::slice::from_ref(&first.label)
        );
        assert_eq!(
            session.pool_stats().total_pages,
            0,
            "a rotation tab leaves the pool with its context"
        );

        let two = session
            .fetch("http://example.invalid/two", FetchParams::new())
            .await
            .expect("a response through the second proxy");
        assert_eq!(
            two.css("#proxy::text").expect("css").texts(),
            std::slice::from_ref(&second.label)
        );

        assert!(first.request_count() >= 1);
        assert!(second.request_count() >= 1);
        assert_eq!(
            one.meta.get(crate::response::META_PROXY_KEY),
            Some(&serde_json::Value::String(first.url())),
            "`meta={{\"proxy\": proxy}}` records the proxy exactly as the rotator \
             handed it over — a string stays a string, as in Python"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn a_per_request_proxy_against_a_persistent_profile_is_refused() {
        require_browser!();
        let server = TestServer::start();
        let proxy = ProxyServer::start("per-request");
        let session = session_for(live_config().retries(1)).await;

        let mut params = FetchParams::new();
        params.proxy = Some(Proxy::Url(proxy.url()));
        let error = session.fetch(&server.url("/"), params).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Browser not initialized for proxy rotation mode",
            "Python's persistent mode never assigns `self.browser`"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn a_cdp_url_session_drives_an_already_running_browser() {
        require_browser!();
        let server = TestServer::start();

        // A free port, then a Chrome of our own on it.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("a free port");
            probe.local_addr().expect("addr").port()
        };
        let profile = std::env::temp_dir().join(format!("browser-rs-cdp-test-{port}"));
        std::fs::create_dir_all(&profile).expect("a profile directory");
        let executable = resolve_executable(&BrowserConfig::new().validate().expect("valid"))
            .expect("a browser");
        let mut child = tokio::process::Command::new(executable)
            .arg("--headless=new")
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("chrome starts");

        // Wait for the debugging port to answer.
        let mut ready = false;
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(ready, "the remote browser never opened its debugging port");

        let session =
            BrowserSession::new(live_config().cdp_url(format!("http://127.0.0.1:{port}")))
                .expect("a session");
        assert_eq!(session.launch_mode(), LaunchMode::Cdp);
        assert!(
            session.launch_options().is_none(),
            "nothing is launched in CDP mode"
        );
        session.start().await.expect("connect over CDP");

        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response from the remote browser");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.css("#title::text").expect("css").texts(),
            ["rendered"]
        );

        session.close().await;
        let _ = child.kill().await;
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[tokio::test]
    async fn the_stealth_flags_and_context_options_reach_the_page() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config().stealth(StealthExtras::new())).await;
        let response = session
            .fetch(&server.url("/probe"), FetchParams::new())
            .await
            .expect("a response");

        assert_eq!(
            response.css("#webdriver::text").expect("css").texts(),
            ["false"],
            "`--disable-blink-features=AutomationControlled` is in STEALTH_ARGS"
        );
        assert_eq!(
            response.css("#dpr::text").expect("css").texts(),
            ["2"],
            "`device_scale_factor: 2`"
        );
        assert_eq!(
            response.css("#scheme::text").expect("css").texts(),
            ["dark"],
            "`color_scheme: 'dark'` defeats creepjs's prefersLightColor check"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn an_init_script_runs_before_the_pages_own_scripts() {
        require_browser!();
        let server = TestServer::start();
        let script =
            std::env::temp_dir().join(format!("browser-rs-init-{}.js", std::process::id()));
        std::fs::write(&script, "window.__browser_rs_init = 'ran';").expect("write the script");

        let session = session_for(live_config().init_script(&script)).await;
        let response = session
            .fetch(&server.url("/probe"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(
            response.css("#injected::text").expect("css").texts(),
            ["ran"],
            "`add_init_script` must land in the page's own world, before its scripts"
        );
        session.close().await;
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn a_custom_useragent_and_cookies_reach_the_server() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config().useragent("browser-rs/1.0 probe").cookies([
            browser_core::types::SetCookieParam {
                name: "seeded".to_owned(),
                value: "yes".to_owned(),
                url: Some(server.url("/")),
                ..Default::default()
            },
        ]))
        .await;

        let response = session
            .fetch(&server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(
            server.header_of("/", "user-agent").as_deref(),
            Some("browser-rs/1.0 probe")
        );
        assert!(
            server
                .header_of("/", "cookie")
                .unwrap_or_default()
                .contains("seeded=yes"),
            "cookies added at start reach the request"
        );
        assert_eq!(response.status, 200);
        session.close().await;
    }

    #[tokio::test]
    async fn the_solver_detects_clicks_and_clears_a_turnstile_shaped_page() {
        require_browser!();
        let server = TestServer::start();
        let session =
            session_for(live_config().stealth(StealthExtras::new().solve_cloudflare(true))).await;

        let mut params = FetchParams::new();
        params.solve_cloudflare = browser_core::types::Override::Set(true);
        let response = session
            .fetch(&server.url("/challenge"), params)
            .await
            .expect("a response");

        assert_eq!(response.status, 200);
        assert!(
            !response.html().contains("cType: 'interactive'"),
            "the click must have cleared the challenge marker"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn the_solver_clicks_inside_the_widget_at_pythons_offsets() {
        require_browser!();
        let server = TestServer::start();
        let session =
            session_for(live_config().stealth(StealthExtras::new().solve_cloudflare(true))).await;

        let mut params = FetchParams::new();
        params.solve_cloudflare = browser_core::types::Override::Set(true);
        let response = session
            .fetch(&server.url("/challenge"), params)
            .await
            .expect("a response");
        assert_eq!(response.status, 200);

        // The page records every `mousedown` in the DOM, which the rendered
        // body carries back here. The widget sits at (100, 200) and Python
        // clicks at x + 26..=28, y + 25..=27.
        let marks = response.css("span.click").expect("css");
        assert_eq!(marks.len(), 1, "one click cleared the challenge");
        let xs = marks.attrs("data-x");
        let ys = response.css("span.click").expect("css").attrs("data-y");
        let x: f64 = xs[0].parse().expect("a number");
        let y: f64 = ys[0].parse().expect("a number");
        assert!((126.0..=128.0).contains(&x), "{x}");
        assert!((225.0..=227.0).contains(&y), "{y}");
        session.close().await;
    }

    #[tokio::test]
    async fn a_failed_attempt_is_retried_on_a_fresh_tab() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config().retries(3).retry_delay_secs(0.0)).await;

        let response = session
            .fetch(&server.url("/flaky"), FetchParams::new())
            .await
            .expect("the second attempt succeeds");
        assert_eq!(response.status, 200);
        assert_eq!(
            server
                .seen_paths()
                .iter()
                .filter(|path| *path == "/flaky")
                .count(),
            2,
            "the first attempt died and the second one went out"
        );
        assert_eq!(
            session.pool_stats().total_pages,
            1,
            "the errored tab was evicted and a fresh one opened"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn every_attempt_can_fail_and_the_last_error_is_returned() {
        require_browser!();
        let session = session_for(live_config().retries(2).retry_delay_secs(0.0)).await;
        let error = session
            .fetch("http://127.0.0.1:1/never", FetchParams::new())
            .await
            .unwrap_err();
        assert!(
            error.to_string().starts_with("page.goto: net::ERR"),
            "{error}"
        );
        assert_eq!(session.pool_stats().total_pages, 0);
        session.close().await;
    }

    #[tokio::test]
    async fn the_wait_parameter_idles_before_snapshotting() {
        require_browser!();
        let server = TestServer::start();
        let session = session_for(live_config()).await;

        let mut params = FetchParams::new();
        params.wait_ms = browser_core::types::Override::Set(400.0);
        let started = std::time::Instant::now();
        session
            .fetch(&server.url("/"), params)
            .await
            .expect("a response");
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "`wait` is milliseconds of idling after everything else"
        );
        session.close().await;
    }

    #[tokio::test]
    async fn the_one_shot_fetcher_starts_and_closes_a_whole_session() {
        require_browser!();
        let server = TestServer::start();
        let response = fetch_once(live_config(), &server.url("/"), FetchParams::new())
            .await
            .expect("a response");
        assert_eq!(response.status, 200);
    }
}
