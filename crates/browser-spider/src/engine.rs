//! [`CrawlerEngine`]: the [`Notify`]-driven main loop, the global spawn cap plus per-domain
//! [`Semaphore`]s, the exact `_process_request` ordering, and the pause / force-stop ladder
//! with the checkpoint saved **before** cancellation.
//!
//! # What the loop does, in Python's order
//!
//! ```text
//! crawl()
//!   reset state, fresh CrawlStats            stats is REPLACED, items cleared in place
//!   restore_from_checkpoint()                -> resuming
//!   sessions.start()                         ── async with session_manager ──┐
//!     seed the stats, on_start(resuming)                                     │
//!     prefetch_robots_txt()                                                  │
//!     try:                                                                   │
//!       start_requests()  (skipped when resuming)                            │
//!       loop:  pause? -> periodic checkpoint? -> idle? -> spawn cap? -> spawn│
//!     finally: on_close(); cleanup() unless paused                           │
//!   sessions.close()                         ───────────────────────────────┘
//!   autothrottle delays, log counts, end_time, log the stats dict
//! ```
//!
//! and one request is:
//!
//! ```text
//! _process_request(request)
//!   1. robots.txt gate       disallowed -> count, log, DROP (no callback)
//!   2. cache replay          hit -> count, callback, RETURN (no delay, no limiter,
//!                                    no is_blocked, no retry)
//!   3. limiter               per-domain when enabled, else global
//!      delay                 AutoThrottle's view of the domain; the SLOT IS HELD
//!      fetch                 failure -> failed_requests_count, on_error, RETURN
//!   4. cache write           outside the limiter
//!   5. block detection       is_blocked(response)
//!      autothrottle feedback record(latency, ok, floor, Retry-After)
//!   6. blocked               -> retry copy (fresh fingerprint) or "max retries exceeded";
//!                               either way the callback NEVER runs
//!   7. callback              _run_callbacks
//! ```
//!
//! # The two deliberate improvements
//!
//! * **No busy-wait.** Python polls the queue every 10–50 ms. The port waits on a
//!   [`Notify`] that every enqueue, every finished task and every pause request signals,
//!   so the *observable* completion condition ("queue empty **and** zero in flight") is
//!   unchanged while end-of-crawl latency drops from ~50 ms to ~0. `ARCHITECTURE.md` §5.5.
//! * **`complete()` runs on cancellation too.** Python relies on a `finally:` inside the
//!   task; the port puts the same bookkeeping in a `Drop` guard that is *owned by the
//!   spawned future*, so a task aborted by a force stop — even one aborted before it was
//!   ever polled — still releases its ticket and its slot.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::stream::StreamExt;
use parking_lot::Mutex;
use browser_core::log::LevelCounter;
use browser_core::types::Proxy;
use browser_http::config::SelectorConfig;
use browser_http::response::DocumentHook;
use serde_json::Value;
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::task::JoinSet;

use crate::cache::{default_cache_dir, ResponseCacheManager};
use crate::checkpoint::{CheckpointData, CheckpointManager};
use crate::items::ItemList;
use crate::request::{Item, Request};
use crate::robots::{effective_domain_delay, fetcher_fn, prefetch_urls, RobotsTxtManager};
use crate::scheduler::{Scheduler, Ticket};
use crate::session::{FetchedResponse, SessionManager};
use crate::spider::{CrawlError, CrawlResponse, RunOptions, Spider, Yielded, STREAM_BUFFER};
use crate::stats::CrawlStats;
use crate::throttle::{parse_retry_after, AutoThrottle, AutoThrottleConfig};

/// `anyio.current_time()`: a monotonic clock that still reads like a wall clock.
///
/// `CrawlStats.start_time` / `end_time` are plain `f64` seconds and their *difference* is
/// what `elapsed_seconds` reports, so the clock has to be monotonic; the tests also pin
/// `end_time >= start_time > 0`, so it cannot start at zero. Anchoring `Instant::now()` to
/// the wall clock once per process satisfies both.
fn now_seconds() -> f64 {
    static ORIGIN: OnceLock<(Instant, f64)> = OnceLock::new();
    let (instant, wall) = ORIGIN.get_or_init(|| {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |since| since.as_secs_f64());
        (Instant::now(), wall)
    });
    wall + instant.elapsed().as_secs_f64()
}

/// Python's `if request._session_kwargs.get("proxy"):` — a truthiness test, not a
/// presence test. An empty proxy URL is falsy; a proxy *dictionary* is truthy whatever it
/// holds, because a non-empty dict is truthy.
fn proxy_is_truthy(proxy: &Proxy) -> bool {
    match proxy {
        Proxy::Url(url) => !url.is_empty(),
        Proxy::Config { .. } => true,
    }
}

/// The single `Ctrl-C` listener the main loop selects on.
///
/// Boxed and kept alive across iterations on purpose: `tokio::signal::ctrl_c()` registers
/// its receiver on first poll and deregisters it when the future is dropped, and tokio's
/// installed SIGINT handler drops the signal when nothing is registered. Re-creating the
/// future at every wait would therefore leave gaps in which a press is lost *and* the
/// process no longer dies from it.
///
/// One consequence has no Python analogue: tokio never uninstalls the OS handler, so after
/// a crawl that watched for `Ctrl-C` the process keeps ignoring the default SIGINT
/// behaviour. Python restores the previous handler in `Spider.start`'s `finally`; there is
/// no equivalent. `RunOptions::handle_ctrl_c = false` (which is what streaming uses) never
/// installs it at all.
type CtrlC = std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>;

/// Bookkeeping that must happen whether the request succeeded, failed, panicked or was
/// cancelled — Python's `finally: scheduler.complete(request); active_tasks -= 1`.
///
/// Owned by the spawned future, so dropping the future (which is what
/// `JoinSet::abort_all` does) is enough.
struct TaskGuard {
    engine: Arc<CrawlerEngine>,
    ticket: Ticket,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.engine.scheduler.lock().complete(self.ticket);
        self.engine.active_tasks.fetch_sub(1, Ordering::AcqRel);
        self.engine.notify.notify_one();
    }
}

/// Clears the streaming channel's sender when the crawl task ends — including when it
/// panics, which would otherwise leave the consumer waiting on a channel nobody can close.
struct StreamGuard(Arc<CrawlerEngine>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        *self.0.item_stream.lock() = None;
    }
}

/// Stops the crawl when the stream it feeds is abandoned. Dropping a bare `JoinHandle`
/// only *detaches* the task, which would leave a crawl fetching for nobody.
struct AbortOnDrop(tokio::task::JoinHandle<Result<Arc<CrawlStats>, CrawlError>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Orchestrates one crawl.
///
/// Constructed fresh for every run (Python does the same) while the
/// [`SessionManager`] is shared across runs, so connection pools and browser processes
/// survive between them.
///
/// Every method takes `&self` except the two that spawn tasks, which take `self: Arc<Self>`
/// because the tasks outlive the call.
pub struct CrawlerEngine {
    spider: Arc<dyn Spider>,
    sessions: Arc<SessionManager>,

    scheduler: Mutex<Scheduler>,
    stats: Mutex<Arc<CrawlStats>>,
    items: Mutex<ItemList>,
    item_stream: Mutex<Option<mpsc::Sender<Item>>>,

    robots: Option<Arc<RobotsTxtManager>>,
    /// Python creates `_domain_delays` **only** when `robots_txt_obey` is on; here the map
    /// exists always and is only ever consulted when `robots` is `Some`, which is the same
    /// thing without an `Option<Mutex<…>>`.
    domain_delays: Mutex<HashMap<String, f64>>,
    cache: Option<ResponseCacheManager>,
    autothrottle: Option<AutoThrottle>,

    global_limiter: Arc<Semaphore>,
    domain_limiters: Mutex<HashMap<String, Arc<Semaphore>>>,

    active_tasks: AtomicUsize,
    running: AtomicBool,
    pause_requested: AtomicBool,
    force_stop: AtomicBool,
    paused: AtomicBool,
    /// Serialises the pause ladder. Python's single-threaded signal handler cannot race
    /// with itself; `request_pause` can be called from any thread here.
    pause_lock: Mutex<()>,
    notify: Notify,

    checkpoint_enabled: bool,
    checkpoints: CheckpointManager,
    last_checkpoint: Mutex<Instant>,

    selector_config: SelectorConfig,
    document_hook: Option<DocumentHook>,
    log_counter: Option<LevelCounter>,
    handle_ctrl_c: bool,
}

impl std::fmt::Debug for CrawlerEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrawlerEngine")
            .field("spider", &self.spider.name())
            .field("queued", &self.scheduler.lock().len())
            .field("active_tasks", &self.active_tasks())
            .field("paused", &self.paused())
            .field("checkpoints", &self.checkpoint_enabled)
            .finish_non_exhaustive()
    }
}

impl CrawlerEngine {
    /// Build the engine for one run of `spider` over `sessions`.
    ///
    /// Mirrors `CrawlerEngine.__init__`: the scheduler takes the spider's three
    /// fingerprint switches, the robots manager is wired to fetch through the session
    /// manager, dev mode logs its warning as it opens the cache, and the checkpoint
    /// manager is constructed **even when checkpointing is disabled** (with an empty
    /// directory), exactly as Python's `CheckpointManager(crawldir or "", interval)`.
    ///
    /// # Errors
    ///
    /// * [`CrawlError::AutoThrottle`] for an invalid AutoThrottle configuration.
    /// * [`CrawlError::Spider`] for an invalid checkpoint interval (negative, or not a
    ///   number).
    pub fn new(
        spider: Arc<dyn Spider>,
        sessions: Arc<SessionManager>,
        options: &RunOptions,
    ) -> Result<Self, CrawlError> {
        let settings = spider.settings();

        let robots = if settings.robots_txt_obey {
            let manager = Arc::clone(&sessions);
            Some(Arc::new(RobotsTxtManager::new(Arc::new(fetcher_fn(
                move |url: String, sid: String| {
                    let manager = Arc::clone(&manager);
                    async move { manager.fetch(&Request::new(url).with_sid(sid)).await }
                },
            )))))
        } else {
            None
        };

        let cache = if settings.development_mode {
            let directory = settings
                .development_cache_dir
                .clone()
                .unwrap_or_else(|| default_cache_dir(spider.name()));
            tracing::warn!(
                "Development mode enabled -- responses will be cached to disk and replayed on subsequent runs"
            );
            Some(ResponseCacheManager::new(directory))
        } else {
            None
        };

        let autothrottle = if settings.autothrottle_enabled {
            #[allow(clippy::cast_precision_loss)]
            let per_domain = settings.concurrent_requests_per_domain as f64;
            let config = AutoThrottleConfig {
                start_delay: settings.autothrottle_start_delay,
                max_delay: settings.autothrottle_max_delay,
                target_concurrency: AutoThrottleConfig::resolve_target_concurrency(
                    settings.autothrottle_target_concurrency,
                    Some(per_domain),
                ),
                block_backoff: settings.autothrottle_block_backoff,
            };
            Some(
                AutoThrottle::new(config).map_err(|error| CrawlError::AutoThrottle {
                    message: error.to_string(),
                })?,
            )
        } else {
            None
        };

        let checkpoint_enabled = options.crawldir.is_some();
        let checkpoints = CheckpointManager::new(
            options.crawldir.clone().unwrap_or_default(),
            options.interval,
        )?;

        let engine = Self {
            scheduler: Mutex::new(Scheduler::new(settings.fingerprint_options())),
            stats: Mutex::new(Arc::new(CrawlStats::new())),
            items: Mutex::new(ItemList::new()),
            item_stream: Mutex::new(None),
            robots,
            domain_delays: Mutex::new(HashMap::new()),
            cache,
            autothrottle,
            global_limiter: Arc::new(Semaphore::new(settings.effective_concurrency())),
            domain_limiters: Mutex::new(HashMap::new()),
            active_tasks: AtomicUsize::new(0),
            running: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            force_stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            pause_lock: Mutex::new(()),
            notify: Notify::new(),
            checkpoint_enabled,
            checkpoints,
            last_checkpoint: Mutex::new(Instant::now()),
            selector_config: options.selector_config.clone(),
            document_hook: options.document_hook.clone(),
            log_counter: options.log_counter.clone(),
            handle_ctrl_c: options.handle_ctrl_c,
            spider,
            sessions,
        };
        Ok(engine)
    }

    // -- public state ----------------------------------------------------

    /// Everything the crawl scraped. **Empty in streaming mode**, where items go to the
    /// channel instead — pinned by Python's tests.
    #[must_use]
    pub fn items(&self) -> ItemList {
        self.items.lock().clone()
    }

    /// The live statistics of the current (or last) run.
    ///
    /// A fresh object is installed at the top of every [`CrawlerEngine::crawl`], so a
    /// caller holding the previous one keeps a frozen snapshot — Python replaces the
    /// attribute for the same reason.
    #[must_use]
    pub fn stats(&self) -> Arc<CrawlStats> {
        Arc::clone(&self.stats.lock())
    }

    /// Whether the crawl stopped at the user's request *with a checkpoint written*.
    /// Always `false` when no `crawldir` was configured, however the crawl ended.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// How many requests are in flight.
    #[must_use]
    pub fn active_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::SeqCst)
    }

    /// How many requests are queued (in-flight ones are not counted).
    #[must_use]
    pub fn queued(&self) -> usize {
        self.scheduler.lock().len()
    }

    /// Whether the checkpoint system is on, i.e. a `crawldir` was configured.
    #[must_use]
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoint_enabled
    }

    /// Ask the crawl to stop.
    ///
    /// The press ladder, exactly as Python's `request_pause`:
    ///
    /// 1. **first** — graceful pause: stop spawning, wait for in-flight requests, then
    ///    write a checkpoint if one was configured;
    /// 2. **second** — force stop: write the checkpoint, *then* cancel the in-flight
    ///    tasks. The order matters; cancelling first would interrupt the save, leave
    ///    `paused` false and make the `finally` delete the user's previous checkpoint;
    /// 3. **third and later** — no-op.
    pub fn request_pause(&self) {
        let _ladder = self.pause_lock.lock();
        if self.force_stop.load(Ordering::SeqCst) {
            return;
        }
        if self.pause_requested.load(Ordering::SeqCst) {
            self.force_stop.store(true, Ordering::SeqCst);
            tracing::warn!("Force stop requested, cancelling immediately...");
        } else {
            self.pause_requested.store(true, Ordering::SeqCst);
            tracing::info!(
                "Pause requested, waiting for in-flight requests to complete (press Ctrl+C again to force stop)..."
            );
        }
        self.notify.notify_one();
    }

    // -- the crawl -------------------------------------------------------

    /// Run the crawl to completion and return its statistics.
    ///
    /// # Errors
    ///
    /// [`CrawlError::NoStartingPoint`] when the spider has no seeds, a
    /// [`CrawlError::Spider`] when a checkpoint could not be written or a session refused
    /// to open or close. A failing *fetch* is not an error here: it increments
    /// `failed_requests_count` and calls [`Spider::on_error`].
    pub async fn crawl(self: Arc<Self>) -> Result<Arc<CrawlStats>, CrawlError> {
        self.running.store(true, Ordering::SeqCst);
        self.items.lock().clear();
        self.paused.store(false, Ordering::SeqCst);
        self.pause_requested.store(false, Ordering::SeqCst);
        self.force_stop.store(false, Ordering::SeqCst);

        let stats = Arc::new(CrawlStats::new());
        stats.set_start_time(now_seconds());
        *self.stats.lock() = Arc::clone(&stats);

        self.domain_limiters.lock().clear();
        if self.robots.is_some() {
            self.domain_delays.lock().clear();
        }
        if let Some(autothrottle) = &self.autothrottle {
            autothrottle.reset();
        }

        let resuming = self.restore_from_checkpoint().await;
        *self.last_checkpoint.lock() = Instant::now();

        // `async with self.session_manager:` — a failing `__aenter__` never reaches
        // `__aexit__`, so a failed `start()` does not try to close anything.
        self.sessions.start().await?;
        let body = Arc::clone(&self).within_sessions(resuming, &stats).await;
        let closed = self.sessions.close().await;
        body?;
        closed?;

        if let Some(autothrottle) = &self.autothrottle {
            stats.set_autothrottle_delays(autothrottle.delays());
        }
        if let Some(counter) = &self.log_counter {
            stats.set_log_counts(counter.counts());
        }
        stats.set_end_time(now_seconds());
        tracing::info!("{}", stats.to_pretty_json());
        Ok(stats)
    }

    /// Everything inside Python's `async with self.session_manager:` block.
    async fn within_sessions(
        self: Arc<Self>,
        resuming: bool,
        stats: &Arc<CrawlStats>,
    ) -> Result<(), CrawlError> {
        {
            let settings = self.spider.settings();
            stats.set_concurrent_requests(settings.concurrent_requests as u64);
            stats
                .set_concurrent_requests_per_domain(settings.concurrent_requests_per_domain as u64);
            stats.set_download_delay(settings.download_delay);
            stats.set_autothrottle_enabled(settings.autothrottle_enabled);
        }
        self.spider.on_start(resuming).await;
        self.prefetch_robots_txt().await;

        let outcome = Arc::clone(&self).main_loop(resuming).await;

        // Python's `finally:` — runs whether the loop finished, paused or raised.
        self.spider.on_close().await;
        if !self.paused() && self.checkpoint_enabled {
            self.checkpoints.cleanup().await;
        }
        outcome
    }

    /// Seed the frontier, then run the spawn loop until the crawl is idle or stopped.
    async fn main_loop(self: Arc<Self>, resuming: bool) -> Result<(), CrawlError> {
        if resuming {
            tracing::info!("Resuming from checkpoint, skipping start_requests()");
        } else {
            let mut seeds = Arc::clone(&self.spider).start_requests().await?;
            while let Some(mut request) = seeds.next().await {
                self.normalize_request(&mut request);
                self.enqueue(request);
            }
        }

        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut failure: Option<CrawlError> = None;
        // One listener for the whole loop. Re-creating `ctrl_c()` at each wait would
        // deregister it in between, and tokio's SIGINT handler — once installed —
        // swallows the signal when nothing is listening, so a press could be lost *and*
        // the process would no longer die from it.
        let mut ctrl_c: Option<CtrlC> = self
            .handle_ctrl_c
            .then(|| Box::pin(tokio::signal::ctrl_c()) as CtrlC);

        while self.running.load(Ordering::SeqCst) {
            // (a) the pause ladder.
            if self.pause_requested.load(Ordering::SeqCst) {
                if self.active_tasks() == 0 || self.force_stop.load(Ordering::SeqCst) {
                    if self.checkpoint_enabled {
                        // BEFORE the cancel, always.
                        if let Err(error) = self.save_checkpoint().await {
                            tasks.abort_all();
                            failure = Some(error);
                            break;
                        }
                        self.paused.store(true, Ordering::SeqCst);
                        tracing::info!("Spider paused, checkpoint saved");
                    } else {
                        tracing::info!("Spider stopped gracefully");
                    }
                    if self.force_stop.load(Ordering::SeqCst) {
                        tracing::warn!("Force stopping with {} active tasks", self.active_tasks());
                        tasks.abort_all();
                    }
                    self.running.store(false, Ordering::SeqCst);
                    break;
                }
                self.wait_for_progress(&mut ctrl_c).await;
                continue;
            }

            // (b) the periodic checkpoint.
            if self.checkpoint_enabled && self.is_checkpoint_time() {
                if let Err(error) = self.save_checkpoint().await {
                    tasks.abort_all();
                    failure = Some(error);
                    break;
                }
            }

            // (c) idle / done detection: queue empty AND nothing in flight.
            if self.scheduler.lock().is_empty() {
                if self.active_tasks() == 0 {
                    self.running.store(false, Ordering::SeqCst);
                    tracing::debug!("Spider idle");
                    break;
                }
                self.wait_for_progress(&mut ctrl_c).await;
                continue;
            }

            // (d) the global spawn cap.
            if self.active_tasks() >= self.spider.settings().effective_concurrency() {
                self.wait_for_progress(&mut ctrl_c).await;
                continue;
            }

            let dequeued = self.scheduler.lock().dequeue();
            let Some((ticket, request)) = dequeued else {
                continue;
            };
            self.active_tasks.fetch_add(1, Ordering::AcqRel);
            let engine = Arc::clone(&self);
            // The guard is built *outside* the future so that a task aborted before its
            // first poll still releases the ticket and the slot.
            let guard = TaskGuard {
                engine: Arc::clone(&self),
                ticket,
            };
            tasks.spawn(async move {
                let _guard = guard;
                engine.process_request(&request).await;
            });
        }

        // Leaving Python's `async with create_task_group()` joins every spawned task.
        while tasks.join_next().await.is_some() {}

        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Sleep until something the loop cares about happens.
    ///
    /// Replaces Python's three `await anyio.sleep(0.05 | 0.01); continue` polls. Wakes on
    /// an enqueue, a finished task, a pause request, `Ctrl-C`, or the next periodic
    /// checkpoint — whichever comes first.
    ///
    /// No wakeup can be lost: a `notify_one` with nobody waiting stores a permit, and
    /// `Notified::enable` claims that permit while registering, so a notification that
    /// landed between the loop's condition test and this call makes the future ready
    /// immediately instead of being dropped on the floor.
    async fn wait_for_progress(&self, ctrl_c: &mut Option<CtrlC>) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let deadline = self.next_checkpoint_deadline();

        /// Which select branch fired.
        enum Woke {
            /// A notification, or the periodic-checkpoint timer.
            Progress,
            /// `Ctrl-C`, or the failure that means we will never see one.
            Signal(std::io::Result<()>),
        }

        let woke = {
            // Borrows `ctrl_c` only for the select, so the listener survives being
            // dropped out of the losing branch and keeps buffering signals.
            let signal = async {
                match ctrl_c.as_mut() {
                    Some(future) => future.await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(signal);
            match deadline {
                Some(deadline) => tokio::select! {
                    () = &mut notified => Woke::Progress,
                    () = tokio::time::sleep_until(deadline) => Woke::Progress,
                    result = &mut signal => Woke::Signal(result),
                },
                None => tokio::select! {
                    () = &mut notified => Woke::Progress,
                    result = &mut signal => Woke::Signal(result),
                },
            }
        };

        match woke {
            Woke::Progress => {}
            Woke::Signal(Ok(())) => {
                self.request_pause();
                // Re-arm, so the second press can force-stop.
                *ctrl_c = Some(Box::pin(tokio::signal::ctrl_c()));
            }
            Woke::Signal(Err(error)) => {
                // Python swallows the `ValueError` you get off the main thread and simply
                // installs no handler; the same idea.
                tracing::debug!("Ctrl-C handling unavailable: {error}");
                *ctrl_c = None;
            }
        }
    }

    /// When the next periodic checkpoint is due, if periodic saves are enabled at all.
    fn next_checkpoint_deadline(&self) -> Option<tokio::time::Instant> {
        if !self.checkpoint_enabled {
            return None;
        }
        let interval = self.checkpoints.interval();
        // `is_finite` already rejects NaN and the infinities, so the comparison below is
        // a total order.
        if !interval.is_finite() || interval <= 0.0 {
            return None;
        }
        let last = *self.last_checkpoint.lock();
        last.checked_add(Duration::from_secs_f64(interval))
            .map(tokio::time::Instant::from_std)
    }

    // -- one request -----------------------------------------------------

    /// `_process_request`. The ordering here is the whole point of this module.
    async fn process_request(&self, request: &Request) {
        let stats = self.stats();
        let settings = self.spider.settings();
        let sid = if request.sid.is_empty() {
            self.default_sid()
        } else {
            request.sid.clone()
        };

        // --- 1. the robots.txt gate ---------------------------------------
        let floor = if let Some(robots) = &self.robots {
            if !robots.can_fetch(&request.url, &request.sid).await {
                stats.add_robots_disallowed(1);
                tracing::info!("Request disallowed by robots.txt: {}", request.url);
                return;
            }
            self.get_domain_delay(request).await
        } else {
            settings.download_delay
        };
        let mut delay = floor;

        // --- 2. the dev-mode cache replay, before any limiter or sleep -----
        if let (Some(cache), Some(fingerprint)) = (&self.cache, request.fingerprint()) {
            if let Some(mut cached) = cache.get(&fingerprint).await {
                cached.attach_request(request);
                stats.add_cache_hits(1);
                stats.increment_requests_count(&sid);
                stats.increment_response_bytes(request.domain(), cached.body.len() as u64);
                stats.increment_status(cached.status);
                tracing::debug!("Cache hit: {}", request.url);
                let response = self.build_response(&cached);
                self.run_callbacks(request, response).await;
                return;
            }
        }

        // --- 3. the fetch, under the rate limiter -------------------------
        let fetched = {
            let limiter = self.rate_limiter(request.domain());
            let Ok(_permit) = limiter.acquire_owned().await else {
                // Unreachable: the engine never closes a semaphore.
                tracing::error!("Rate limiter closed while fetching {}", request.url);
                return;
            };

            if let Some(autothrottle) = &self.autothrottle {
                delay = autothrottle.delay_for(request.domain(), floor);
            }
            // Python's `if delay:`; a zero (or negative, or NaN) delay sleeps not at all,
            // and `Duration::from_secs_f64` would panic on the last two.
            if delay > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }

            if let Some(proxy) = request
                .session_kwargs
                .proxy
                .as_ref()
                .filter(|p| proxy_is_truthy(p))
            {
                stats.push_proxy_config(proxy);
            }
            if let Some(proxies) = request
                .session_kwargs
                .proxies
                .as_ref()
                .filter(|map| !map.is_empty())
            {
                stats.push_proxy(serde_json::to_value(proxies).unwrap_or(Value::Null));
            }

            let started = Instant::now();
            match self.sessions.fetch(request).await {
                Ok(response) => {
                    let latency = started.elapsed().as_secs_f64();
                    stats.increment_requests_count(&sid);
                    stats.increment_response_bytes(request.domain(), response.body.len() as u64);
                    stats.increment_status(response.status);
                    Some((response, latency))
                }
                Err(error) => {
                    // `requests_count` is deliberately NOT incremented here.
                    stats.add_failed_requests(1);
                    self.spider
                        .on_error(request, &CrawlError::from(error))
                        .await;
                    None
                }
            }
        };
        let Some((fetched, latency)) = fetched else {
            return;
        };

        // --- 4. the cache write, outside the limiter ----------------------
        if let (Some(cache), Some(fingerprint)) = (&self.cache, request.fingerprint()) {
            stats.add_cache_misses(1);
            cache
                .put(
                    &fingerprint,
                    &fetched,
                    request.session_kwargs.method().as_str(),
                )
                .await;
        }

        // --- 5. block detection, then the AutoThrottle feedback -----------
        let response = self.build_response(&fetched);
        let blocked = self.spider.is_blocked(&response).await;
        if let Some(autothrottle) = &self.autothrottle {
            let ok = (200..300).contains(&response.status) && !blocked;
            let retry_after = if ok {
                None
            } else {
                parse_retry_after(&response.headers)
            };
            autothrottle.record(request.domain(), latency, ok, floor, retry_after);
        }

        // --- 6. blocked: retry, or give up. Either way, no callback -------
        if blocked {
            stats.add_blocked_requests(1);
            if request.retry_count < settings.max_blocked_retries {
                let mut retry = request.copy();
                retry.retry_count += 1;
                // Python's `priority -= 1` on an unbounded int; saturating is the only
                // sane thing to do at `i32::MIN`.
                retry.priority = retry.priority.saturating_sub(1);
                retry.dont_filter = true;
                // Dropped so a proxy rotator hands out a fresh one.
                retry.session_kwargs.proxy = None;
                retry.session_kwargs.proxies = None;
                let attempt = retry.retry_count;

                let mut prepared = self.spider.retry_blocked_request(retry, &response).await;
                self.normalize_request(&mut prepared);
                self.enqueue(prepared);
                tracing::info!(
                    "Scheduled blocked request for retry ({attempt}/{}): {}",
                    settings.max_blocked_retries,
                    request.url
                );
            } else {
                tracing::warn!("Max retries exceeded for blocked request: {}", request.url);
            }
            return;
        }

        // --- 7. the normal path -------------------------------------------
        self.run_callbacks(request, response).await;
    }

    /// `_run_callbacks`: dispatch, then interpret everything the callback yields.
    ///
    /// `Request` is checked before `Item`, an **empty** item counts as dropped
    /// (Python tests `if processed:`, not `is not None`), and a callback failure logs at
    /// ERROR, calls [`Spider::on_error`] and abandons the rest of the stream — the same
    /// thing Python's `except Exception` around the `async for` does.
    async fn run_callbacks(&self, request: &Request, response: CrawlResponse) {
        let stats = self.stats();
        let label = response.to_string();
        let spider = Arc::clone(&self.spider);
        let mut results = match request.callback.clone() {
            Some(callback) => spider.dispatch(callback, response).await,
            None => spider.parse(response).await,
        };

        while let Some(result) = results.next().await {
            match result {
                Yielded::Request(follow_up) => {
                    let mut follow_up = *follow_up;
                    if self.spider.settings().is_domain_allowed(follow_up.domain()) {
                        self.normalize_request(&mut follow_up);
                        self.enqueue(follow_up);
                    } else {
                        stats.add_offsite_requests(1);
                        tracing::debug!("Filtered offsite request to: {}", follow_up.url);
                    }
                }
                Yielded::Item(item) => {
                    let processed = self.spider.on_scraped_item(item).await;
                    match processed {
                        Some(item) if !item.is_empty() => {
                            stats.add_items_scraped(1);
                            tracing::debug!("Scraped from {}\n{}", label, pretty(&item));
                            let sender = self.item_stream.lock().clone();
                            match sender {
                                Some(sender) => {
                                    if sender.send(item).await.is_err() {
                                        tracing::debug!(
                                            "Item stream closed; dropping the scraped item"
                                        );
                                    }
                                }
                                None => self.items.lock().push(item),
                            }
                        }
                        dropped => {
                            stats.add_items_dropped(1);
                            tracing::warn!(
                                "Dropped from {}\n{}",
                                label,
                                dropped.as_ref().map_or_else(|| "None".to_owned(), pretty)
                            );
                        }
                    }
                }
                Yielded::Skip => {}
                Yielded::Error(error) => {
                    tracing::error!("Spider error processing {request}:\n {error}");
                    self.spider.on_error(request, &error).await;
                    // Python's exception aborts the rest of the generator.
                    break;
                }
            }
        }
    }

    // -- helpers ---------------------------------------------------------

    /// The manager's default session id, or `""` if every session was removed mid-crawl
    /// (Python raises `RuntimeError` there; a spawned task has nowhere to raise it).
    fn default_sid(&self) -> String {
        match self.sessions.default_session_id() {
            Ok(sid) => sid.to_owned(),
            Err(error) => {
                tracing::error!("Cannot resolve the default session id: {error}");
                String::new()
            }
        }
    }

    /// `_normalize_request`: resolve an empty `sid` to the manager's default.
    ///
    /// Must run **before** the request is enqueued, because `sid` is part of the
    /// fingerprint.
    fn normalize_request(&self, request: &mut Request) {
        if request.sid.is_empty() {
            request.sid = self.default_sid();
        }
    }

    /// Enqueue and wake the loop. `false` means the request was dropped as a duplicate.
    fn enqueue(&self, request: Request) -> bool {
        let accepted = self.scheduler.lock().enqueue(request);
        if accepted {
            self.notify.notify_one();
        }
        accepted
    }

    /// `_rate_limiter`: the per-domain semaphore when a per-domain cap is set, otherwise
    /// the global one.
    ///
    /// Note that with a per-domain cap the *global* semaphore is never taken — the global
    /// limit is then enforced by the loop's spawn check, exactly as in Python.
    fn rate_limiter(&self, domain: &str) -> Arc<Semaphore> {
        let per_domain = self.spider.settings().concurrent_requests_per_domain;
        if per_domain == 0 {
            return Arc::clone(&self.global_limiter);
        }
        let mut limiters = self.domain_limiters.lock();
        Arc::clone(
            limiters
                .entry(domain.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(per_domain))),
        )
    }

    /// `_get_domain_delay`: `max(download_delay, robots directives)`, cached per domain
    /// for the whole crawl. Robots can only *raise* the configured delay.
    async fn get_domain_delay(&self, request: &Request) -> f64 {
        let settings = self.spider.settings();
        let Some(robots) = &self.robots else {
            return settings.download_delay;
        };
        if let Some(delay) = self.domain_delays.lock().get(request.domain()).copied() {
            return delay;
        }
        let (crawl_delay, request_rate) = robots
            .get_delay_directives(&request.url, &request.sid)
            .await;
        let delay = effective_domain_delay(settings.download_delay, crawl_delay, request_rate);
        self.domain_delays
            .lock()
            .insert(request.domain().to_owned(), delay);
        delay
    }

    /// Widen a [`FetchedResponse`] into the response a callback sees, with the crawl's
    /// `selector_config` and adaptive-storage hook attached. Live fetches and cache
    /// replays both come through here, which is what makes a replay indistinguishable.
    fn build_response(&self, fetched: &FetchedResponse) -> CrawlResponse {
        CrawlResponse::from_fetched(fetched, &self.selector_config, self.document_hook.as_ref())
    }

    // -- checkpoints -----------------------------------------------------

    /// `_save_checkpoint`. Re-raises, like `CheckpointManager.save`.
    async fn save_checkpoint(&self) -> Result<(), CrawlError> {
        let snapshot = self.scheduler.lock().snapshot();
        self.checkpoints
            .save(&CheckpointData::from(snapshot))
            .await?;
        *self.last_checkpoint.lock() = Instant::now();
        Ok(())
    }

    /// `_is_checkpoint_time`. `false` when checkpointing is off, and `interval == 0`
    /// disables periodic saves without disabling the final one taken on pause.
    fn is_checkpoint_time(&self) -> bool {
        if !self.checkpoint_enabled {
            return false;
        }
        let interval = self.checkpoints.interval();
        if interval == 0.0 {
            return false;
        }
        self.last_checkpoint.lock().elapsed().as_secs_f64() >= interval
    }

    /// `_restore_from_checkpoint`: `true` when a checkpoint was loaded, which is what
    /// makes the loop skip `start_requests()`.
    ///
    /// Callbacks are re-bound *before* the requests reach the scheduler. Python re-binds
    /// afterwards because its scheduler holds the very same objects; the port's scheduler
    /// clones them, so the order has to flip for the effect to survive.
    async fn restore_from_checkpoint(&self) -> bool {
        if !self.checkpoint_enabled {
            return false;
        }
        let Some(mut data) = self.checkpoints.load().await else {
            return false;
        };
        for request in &mut data.requests {
            request.restore_callback(|callback| self.spider.has_callback(callback));
        }
        self.scheduler.lock().restore(&data);
        true
    }

    /// `_prefetch_robots_txt`: one fetch per unique netloc among `start_urls`, keeping the
    /// scheme of the first URL seen for each.
    async fn prefetch_robots_txt(&self) {
        let Some(robots) = &self.robots else {
            return;
        };
        let seeds = {
            let settings = self.spider.settings();
            if settings.start_urls.is_empty() {
                return;
            }
            prefetch_urls(&settings.start_urls)
        };
        let sid = self.default_sid();
        robots.prefetch(&seeds, &sid).await;
    }

    // -- streaming -------------------------------------------------------

    /// Run the crawl and stream items out as they are scraped.
    ///
    /// The channel holds [`STREAM_BUFFER`] items — Python's
    /// `create_memory_object_stream[dict](100)` — so a slow consumer applies backpressure
    /// and slows the crawl instead of growing memory. While a stream is active
    /// [`CrawlerEngine::items`] stays empty.
    ///
    /// A crawl failure has nowhere to go in a `Stream<Item = Item>`: it is logged at ERROR
    /// and ends the stream. Use [`CrawlerEngine::crawl`] when the error matters.
    ///
    /// **Dropping the stream stops the crawl.** Python's `_stream` runs the crawl inside a
    /// task group nested in the generator, so abandoning the generator cancels it; the
    /// port aborts the crawl task for the same reason — a detached crawl would keep
    /// fetching after its consumer walked away. The difference is that an abort drops the
    /// crawl future rather than unwinding it, so [`Spider::on_close`] and the session
    /// shutdown are skipped. Drain the stream when that matters.
    pub fn stream(self: Arc<Self>) -> impl futures::Stream<Item = Item> + Send + 'static {
        let (sender, mut receiver) = mpsc::channel::<Item>(STREAM_BUFFER);
        *self.item_stream.lock() = Some(sender);

        async_stream::stream! {
            let engine = self;
            let mut task = AbortOnDrop({
                let engine = Arc::clone(&engine);
                tokio::spawn(async move {
                    let _guard = StreamGuard(Arc::clone(&engine));
                    engine.crawl().await
                })
            });
            while let Some(item) = receiver.recv().await {
                yield item;
            }
            match (&mut task.0).await {
                Ok(Ok(_stats)) => {}
                Ok(Err(error)) => tracing::error!("Crawl failed: {error}"),
                Err(error) if error.is_cancelled() => {}
                Err(error) => tracing::error!("Crawl task failed: {error}"),
            }
        }
    }
}

/// `pprint.pformat`, near enough: the log lines that carry an item are for humans.
fn pretty(item: &Item) -> String {
    serde_json::to_string_pretty(item).unwrap_or_else(|_| format!("{item:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU32;

    use async_trait::async_trait;
    use browser_core::types::HttpMethod;
    use serde_json::json;

    use crate::error::{Result as SpiderResult, SpiderError};
    use crate::request::{CallbackId, SessionKwargs};
    use crate::session::{AddOptions, CrawlSession};
    use crate::spider::{CallbackStream, RequestStream, SpiderSettings};

    // -- a site to crawl ---------------------------------------------------

    /// A session that serves a fixed table and counts every fetch.
    #[derive(Debug, Default)]
    struct MockSite {
        pages: HashMap<String, (u16, String)>,
        fetched: Mutex<Vec<String>>,
        alive: AtomicBool,
        opened: AtomicUsize,
        closed: AtomicUsize,
    }

    impl MockSite {
        fn new(pages: &[(&str, u16, &str)]) -> Arc<Self> {
            Arc::new(Self {
                pages: pages
                    .iter()
                    .map(|(url, status, body)| ((*url).to_owned(), (*status, (*body).to_owned())))
                    .collect(),
                ..Self::default()
            })
        }

        fn fetch_count(&self, url: &str) -> usize {
            self.fetched.lock().iter().filter(|hit| *hit == url).count()
        }
    }

    #[async_trait]
    impl CrawlSession for MockSite {
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
        async fn open(&self) -> SpiderResult<()> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            self.alive.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn close(&self) -> SpiderResult<()> {
            self.closed.fetch_add(1, Ordering::SeqCst);
            self.alive.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn fetch(&self, url: &str, _kwargs: &SessionKwargs) -> SpiderResult<FetchedResponse> {
            self.fetched.lock().push(url.to_owned());
            match self.pages.get(url) {
                Some((status, body)) => {
                    let mut response = FetchedResponse::new(*status, url, body.clone());
                    response.encoding = "utf-8".to_owned();
                    if *status == 429 {
                        response
                            .headers
                            .insert("Retry-After".to_owned(), "0".to_owned());
                    }
                    Ok(response)
                }
                None => Err(SpiderError::Fetch {
                    sid: "mock".to_owned(),
                    url: url.to_owned(),
                    message: "no such page".to_owned(),
                }),
            }
        }
    }

    // -- a spider ----------------------------------------------------------

    #[derive(Clone, Debug)]
    enum Behaviour {
        Nothing,
        /// One item per response, `{"url": …}`.
        ItemPerPage,
        /// One item that is empty, so `on_scraped_item` returns something falsy.
        EmptyItem,
        /// Follow every `<a href>` in the body.
        FollowLinks,
        /// Yield the given absolute URLs as requests.
        Requests(Vec<String>),
        /// Report a callback failure **and then keep yielding**.
        ///
        /// Python cannot express this — `raise` ends the generator — which is exactly why
        /// it has to be pinned here: `Yielded::Error` is a value, not a control-flow exit,
        /// so only the engine can guarantee that nothing after it is acted on.
        Fail,
        /// One `Skip`, then one item — proves `None` is ignored, not fatal.
        SkipThenItem,
    }

    #[derive(Debug, Default)]
    struct Recorder {
        on_start: AtomicUsize,
        on_close: AtomicUsize,
        resuming: Mutex<Vec<bool>>,
        errors: Mutex<Vec<String>>,
        scraped: AtomicUsize,
    }

    struct TestSpider {
        settings: SpiderSettings,
        behaviour: Behaviour,
        recorder: Arc<Recorder>,
        drop_items: bool,
        seeds: Option<Vec<String>>,
    }

    impl TestSpider {
        fn new(settings: SpiderSettings, behaviour: Behaviour) -> Arc<Self> {
            Arc::new(Self {
                settings,
                behaviour,
                recorder: Arc::new(Recorder::default()),
                drop_items: false,
                seeds: None,
            })
        }
    }

    #[async_trait]
    impl Spider for TestSpider {
        fn name(&self) -> &str {
            "tester"
        }
        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }
        fn configure_sessions(&self, _manager: &mut SessionManager) -> Result<(), CrawlError> {
            Ok(())
        }
        fn has_callback(&self, callback: &CallbackId) -> bool {
            matches!(callback.as_str(), "parse" | "parse_detail")
        }
        async fn start_requests(self: Arc<Self>) -> Result<RequestStream, CrawlError> {
            let urls = match &self.seeds {
                Some(seeds) => seeds.clone(),
                None => self.settings.start_urls.clone(),
            };
            if urls.is_empty() {
                return Err(CrawlError::NoStartingPoint);
            }
            Ok(Box::pin(futures::stream::iter(
                urls.into_iter().map(Request::new),
            )))
        }
        async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
            let behaviour = self.behaviour.clone();
            let url = response.url.clone();
            let body = response.body_text().into_owned();
            Box::pin(async_stream::stream! {
                match behaviour {
                    Behaviour::Nothing => {}
                    Behaviour::ItemPerPage => {
                        let mut item = Item::new();
                        item.insert("url".into(), json!(url));
                        yield Yielded::item(item);
                    }
                    Behaviour::EmptyItem => yield Yielded::item(Item::new()),
                    Behaviour::SkipThenItem => {
                        yield Yielded::Skip;
                        let mut item = Item::new();
                        item.insert("url".into(), json!(url));
                        yield Yielded::item(item);
                    }
                    Behaviour::FollowLinks => {
                        for href in links(&body) {
                            yield Yielded::request(Request::new(response.urljoin(&href)));
                        }
                        let mut item = Item::new();
                        item.insert("url".into(), json!(url));
                        yield Yielded::item(item);
                    }
                    Behaviour::Requests(urls) => {
                        for target in urls {
                            yield Yielded::request(Request::new(target));
                        }
                    }
                    Behaviour::Fail => {
                        yield Yielded::error("callback exploded");
                        // Neither of these may reach the engine.
                        let mut item = Item::new();
                        item.insert("after".to_owned(), json!("the error"));
                        yield Yielded::item(item);
                        yield Yielded::request(
                            Request::new("https://a.test/never").with_sid("mock"),
                        );
                    }
                }
            })
        }
        async fn on_start(&self, resuming: bool) {
            self.recorder.on_start.fetch_add(1, Ordering::SeqCst);
            self.recorder.resuming.lock().push(resuming);
        }
        async fn on_close(&self) {
            self.recorder.on_close.fetch_add(1, Ordering::SeqCst);
        }
        async fn on_error(&self, request: &Request, error: &CrawlError) {
            self.recorder
                .errors
                .lock()
                .push(format!("{}: {error}", request.url));
        }
        async fn on_scraped_item(&self, item: Item) -> Option<Item> {
            self.recorder.scraped.fetch_add(1, Ordering::SeqCst);
            if self.drop_items {
                None
            } else {
                Some(item)
            }
        }
    }

    /// The world's smallest `href` scraper — enough for the fixtures below.
    fn links(body: &str) -> Vec<String> {
        body.split("href=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next().map(str::to_owned))
            .collect()
    }

    // -- wiring ------------------------------------------------------------

    fn manager_for(site: Arc<MockSite>) -> Arc<SessionManager> {
        let mut manager = SessionManager::new();
        manager
            .add("mock", site, AddOptions::DEFAULT)
            .expect("registers");
        Arc::new(manager)
    }

    fn engine_for(
        spider: Arc<TestSpider>,
        site: Arc<MockSite>,
        options: RunOptions,
    ) -> Arc<CrawlerEngine> {
        Arc::new(CrawlerEngine::new(spider, manager_for(site), &options).expect("an engine"))
    }

    fn seeded(urls: &[&str]) -> SpiderSettings {
        SpiderSettings::default().with_start_urls(urls.iter().copied())
    }

    // -- the helpers -------------------------------------------------------

    #[test]
    fn rate_limiter_is_global_until_a_per_domain_cap_is_set() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());

        let global = engine.rate_limiter("a.test");
        assert!(Arc::ptr_eq(&global, &engine.global_limiter));
        assert_eq!(global.available_permits(), 4, "the default concurrency");
        assert!(Arc::ptr_eq(&global, &engine.rate_limiter("b.test")));
    }

    #[test]
    fn per_domain_limiters_are_memoised_and_distinct() {
        let site = MockSite::new(&[]);
        let settings = seeded(&["https://a.test/"]).with_concurrent_requests_per_domain(2);
        let spider = TestSpider::new(settings, Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());

        let first = engine.rate_limiter("a.test");
        let again = engine.rate_limiter("a.test");
        let other = engine.rate_limiter("b.test");
        assert!(Arc::ptr_eq(&first, &again), "memoised per domain");
        assert!(!Arc::ptr_eq(&first, &other));
        assert!(!Arc::ptr_eq(&first, &engine.global_limiter));
        assert_eq!(first.available_permits(), 2);
    }

    #[test]
    fn normalize_request_fills_only_an_empty_sid() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());

        let mut blank = Request::new("https://a.test/");
        engine.normalize_request(&mut blank);
        assert_eq!(blank.sid, "mock");

        let mut explicit = Request::new("https://a.test/").with_sid("other");
        engine.normalize_request(&mut explicit);
        assert_eq!(explicit.sid, "other");
    }

    #[test]
    fn checkpoint_helpers_are_inert_when_disabled() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());
        assert!(!engine.checkpoints_enabled());
        assert!(!engine.is_checkpoint_time());
        assert!(engine.next_checkpoint_deadline().is_none());
    }

    #[tokio::test]
    async fn restore_from_checkpoint_is_false_without_a_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(
            spider,
            site,
            RunOptions::default().with_crawldir(directory.path()),
        );
        assert!(engine.checkpoints_enabled());
        assert!(!engine.restore_from_checkpoint().await);
    }

    #[test]
    fn interval_zero_disables_periodic_saves_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(
            spider,
            site,
            RunOptions::default()
                .with_crawldir(directory.path())
                .with_interval(0.0),
        );
        assert!(engine.checkpoints_enabled());
        assert!(!engine.is_checkpoint_time());
        assert!(engine.next_checkpoint_deadline().is_none());
    }

    // -- one request -------------------------------------------------------

    #[tokio::test]
    async fn a_successful_fetch_updates_the_counters() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::ItemPerPage);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

        let request = Request::new("https://a.test/").with_sid("mock");
        engine.process_request(&request).await;

        let stats = engine.stats();
        assert_eq!(stats.requests_count(), 1);
        assert_eq!(stats.response_bytes(), "<p>ok</p>".len() as u64);
        assert_eq!(stats.status_count(200), 1);
        assert_eq!(stats.session_requests("mock"), 1);
        assert_eq!(stats.items_scraped(), 1);
        assert_eq!(engine.items().len(), 1);
        assert_eq!(stats.failed_requests_count(), 0);
        assert_eq!(site.fetch_count("https://a.test/"), 1);
    }

    #[tokio::test]
    async fn a_failed_fetch_counts_separately_and_calls_on_error() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::ItemPerPage);
        let recorder = Arc::clone(&spider.recorder);
        let engine = engine_for(spider, site, RunOptions::default());

        let request = Request::new("https://a.test/missing").with_sid("mock");
        engine.process_request(&request).await;

        let stats = engine.stats();
        assert_eq!(stats.failed_requests_count(), 1);
        assert_eq!(stats.requests_count(), 0, "a failure is not a request");
        assert_eq!(recorder.errors.lock().len(), 1);
        assert!(recorder.errors.lock()[0].contains("https://a.test/missing"));
    }

    #[tokio::test]
    async fn a_blocked_response_is_retried_without_its_proxies() {
        let site = MockSite::new(&[("https://a.test/x", 429, "go away")]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::ItemPerPage);
        let engine = engine_for(spider, site, RunOptions::default());

        let mut proxies = BTreeMap::new();
        proxies.insert("all".to_owned(), "http://proxy:8080".to_owned());
        let kwargs = SessionKwargs {
            method: Some(HttpMethod::Post),
            proxy: Some(Proxy::Url("http://proxy:8080".to_owned())),
            proxies: Some(proxies),
            ..SessionKwargs::default()
        };
        let request = Request::new("https://a.test/x")
            .with_sid("mock")
            .with_priority(3)
            .with_session_kwargs(kwargs);
        request.update_fingerprint(Default::default());

        engine.process_request(&request).await;

        let stats = engine.stats();
        assert_eq!(stats.blocked_requests_count(), 1);
        assert_eq!(
            stats.items_scraped(),
            0,
            "a blocked response has no callback"
        );
        assert_eq!(stats.proxies().len(), 2, "proxy and proxies both recorded");

        let (_, retry) = engine.scheduler.lock().dequeue().expect("a retry");
        assert_eq!(retry.retry_count, 1);
        assert_eq!(retry.priority, 2, "retries sink one band");
        assert!(retry.dont_filter);
        assert!(retry.session_kwargs.proxy.is_none());
        assert!(retry.session_kwargs.proxies.is_none());
        assert_eq!(
            retry.session_kwargs.method,
            Some(HttpMethod::Post),
            "the method survives the retry"
        );
        // `copy()` cleared the fingerprint and the scheduler recomputed it. With
        // `include_kwargs` off it lands on the same digest as the original, which is
        // precisely why the retry needs `dont_filter` to get past `_seen`.
        assert_eq!(retry.fingerprint(), request.fingerprint());
        assert!(
            engine
                .scheduler
                .lock()
                .has_seen(&request.fingerprint().expect("fingerprinted")),
            "the original is still in the seen set"
        );
    }

    #[tokio::test]
    async fn a_blocked_response_at_the_retry_limit_enqueues_nothing() {
        let site = MockSite::new(&[("https://a.test/x", 503, "nope")]);
        let settings = seeded(&["https://a.test/"]).with_max_blocked_retries(2);
        let spider = TestSpider::new(settings, Behaviour::ItemPerPage);
        let engine = engine_for(spider, site, RunOptions::default());

        let request = Request::new("https://a.test/x")
            .with_sid("mock")
            .with_retry_count(2);
        engine.process_request(&request).await;

        assert_eq!(engine.stats().blocked_requests_count(), 1);
        assert!(engine.scheduler.lock().is_empty());
    }

    #[tokio::test]
    async fn an_offsite_request_is_filtered_not_enqueued() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let settings = seeded(&["https://a.test/"])
            .with_allowed_domains(["a.test"])
            .with_concurrent_requests(1);
        let spider = TestSpider::new(
            settings,
            Behaviour::Requests(vec![
                "https://a.test/inside".to_owned(),
                "https://evil.test/outside".to_owned(),
            ]),
        );
        let engine = engine_for(spider, site, RunOptions::default());

        let request = Request::new("https://a.test/").with_sid("mock");
        engine.process_request(&request).await;

        let stats = engine.stats();
        assert_eq!(stats.offsite_requests_count(), 1);
        let (_, queued) = engine.scheduler.lock().dequeue().expect("the inside link");
        assert_eq!(queued.url, "https://a.test/inside");
        assert_eq!(queued.sid, "mock", "normalised before enqueue");
        assert!(engine.scheduler.lock().is_empty());
    }

    #[tokio::test]
    async fn an_empty_item_is_dropped_by_truthiness() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::EmptyItem);
        let engine = engine_for(spider, site, RunOptions::default());

        engine
            .process_request(&Request::new("https://a.test/").with_sid("mock"))
            .await;

        let stats = engine.stats();
        assert_eq!(stats.items_dropped(), 1);
        assert_eq!(stats.items_scraped(), 0);
        assert!(engine.items().is_empty());
    }

    #[tokio::test]
    async fn on_scraped_item_returning_none_drops_the_item() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let spider = Arc::new(TestSpider {
            settings: seeded(&["https://a.test/"]),
            behaviour: Behaviour::ItemPerPage,
            recorder: Arc::new(Recorder::default()),
            drop_items: true,
            seeds: None,
        });
        let engine = engine_for(spider, site, RunOptions::default());

        engine
            .process_request(&Request::new("https://a.test/").with_sid("mock"))
            .await;

        let stats = engine.stats();
        assert_eq!(stats.items_dropped(), 1);
        assert_eq!(stats.items_scraped(), 0);
        assert!(engine.items().is_empty());
    }

    #[tokio::test]
    async fn a_skip_is_ignored_and_the_stream_continues() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::SkipThenItem);
        let engine = engine_for(spider, site, RunOptions::default());

        engine
            .process_request(&Request::new("https://a.test/").with_sid("mock"))
            .await;

        assert_eq!(engine.stats().items_scraped(), 1);
    }

    #[tokio::test]
    async fn a_callback_failure_calls_on_error_once_and_stops_that_stream() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Fail);
        let recorder = Arc::clone(&spider.recorder);
        let engine = engine_for(spider, site, RunOptions::default());

        engine
            .process_request(&Request::new("https://a.test/").with_sid("mock"))
            .await;

        assert_eq!(recorder.errors.lock().len(), 1);
        assert!(recorder.errors.lock()[0].contains("callback exploded"));
        // The item and the request yielded *after* the error must never be acted on:
        // Python's `raise` ends the generator, and the port has to end the stream.
        assert_eq!(engine.stats().items_scraped(), 0);
        assert_eq!(engine.stats().items_dropped(), 0);
        assert_eq!(engine.items().as_slice().len(), 0);
        assert_eq!(engine.queued(), 0, "nothing after the error was enqueued");
    }

    // -- the loop ----------------------------------------------------------

    #[tokio::test]
    async fn a_whole_crawl_walks_the_site_once() {
        let site = MockSite::new(&[
            (
                "https://a.test/",
                200,
                "<a href=\"/one\">1</a><a href=\"/two\">2</a><a href=\"/one\">1</a>",
            ),
            ("https://a.test/one", 200, "<p>one</p>"),
            ("https://a.test/two", 200, "<a href=\"/one\">1</a>"),
        ]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::FollowLinks);
        let recorder = Arc::clone(&spider.recorder);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

        let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");

        assert_eq!(stats.requests_count(), 3);
        assert_eq!(stats.items_scraped(), 3);
        assert_eq!(engine.items().len(), 3);
        assert_eq!(site.fetch_count("https://a.test/one"), 1, "deduplicated");
        assert_eq!(site.fetch_count("https://a.test/two"), 1);
        assert_eq!(recorder.on_start.load(Ordering::SeqCst), 1);
        assert_eq!(recorder.on_close.load(Ordering::SeqCst), 1);
        assert_eq!(*recorder.resuming.lock(), vec![false]);
        assert!(stats.end_time() >= stats.start_time());
        assert!(stats.start_time() > 0.0);
        assert_eq!(stats.concurrent_requests(), 4);
        assert_eq!(engine.active_tasks(), 0);
        assert!(!engine.paused());
        assert_eq!(site.opened.load(Ordering::SeqCst), 1);
        assert_eq!(site.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_crawl_without_seeds_fails_but_still_closes() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(SpiderSettings::default(), Behaviour::Nothing);
        let recorder = Arc::clone(&spider.recorder);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

        let error = Arc::clone(&engine).crawl().await.expect_err("no seeds");
        assert!(matches!(error, CrawlError::NoStartingPoint));
        assert_eq!(
            recorder.on_close.load(Ordering::SeqCst),
            1,
            "the finally ran"
        );
        assert_eq!(
            site.closed.load(Ordering::SeqCst),
            1,
            "and so did __aexit__"
        );
    }

    #[tokio::test]
    async fn the_global_cap_bounds_the_number_of_tasks_in_flight() {
        let pages: Vec<(&str, u16, &str)> = vec![
            ("https://a.test/", 200, "<a href=\"/1\">1</a><a href=\"/2\">2</a><a href=\"/3\">3</a><a href=\"/4\">4</a><a href=\"/5\">5</a><a href=\"/6\">6</a>"),
            ("https://a.test/1", 200, "<p/>"),
            ("https://a.test/2", 200, "<p/>"),
            ("https://a.test/3", 200, "<p/>"),
            ("https://a.test/4", 200, "<p/>"),
            ("https://a.test/5", 200, "<p/>"),
            ("https://a.test/6", 200, "<p/>"),
        ];
        let site = MockSite::new(&pages);
        let settings = seeded(&["https://a.test/"]).with_concurrent_requests(2);
        let spider = TestSpider::new(settings, Behaviour::FollowLinks);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

        let peak = Arc::new(AtomicUsize::new(0));
        let watcher = {
            let engine = Arc::clone(&engine);
            let peak = Arc::clone(&peak);
            tokio::spawn(async move {
                for _ in 0..2000 {
                    let active = engine.active_tasks();
                    peak.fetch_max(active, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                }
            })
        };

        let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");
        watcher.abort();

        assert_eq!(stats.requests_count(), 7);
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "spawned {} at once with a cap of 2",
            peak.load(Ordering::SeqCst)
        );
    }

    // -- pause, force stop, checkpoints ------------------------------------

    #[test]
    fn the_press_ladder_is_pause_then_force_then_nothing() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());

        engine.request_pause();
        assert!(engine.pause_requested.load(Ordering::SeqCst));
        assert!(!engine.force_stop.load(Ordering::SeqCst));

        engine.request_pause();
        assert!(engine.force_stop.load(Ordering::SeqCst));

        engine.request_pause();
        assert!(engine.force_stop.load(Ordering::SeqCst), "third is a no-op");
        assert!(engine.pause_requested.load(Ordering::SeqCst));
    }

    /// A spider whose callback pauses the crawl the first time it runs.
    struct PausingSpider {
        settings: SpiderSettings,
        engine: Mutex<Option<Arc<CrawlerEngine>>>,
        presses: AtomicU32,
        force: bool,
    }

    #[async_trait]
    impl Spider for PausingSpider {
        fn name(&self) -> &str {
            "pauser"
        }
        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }
        fn configure_sessions(&self, _manager: &mut SessionManager) -> Result<(), CrawlError> {
            Ok(())
        }
        async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
            if self.presses.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some(engine) = self.engine.lock().clone() {
                    engine.request_pause();
                    if self.force {
                        engine.request_pause();
                    }
                }
            }
            let body = response.body_text().into_owned();
            let base = response.url.clone();
            Box::pin(async_stream::stream! {
                for href in links(&body) {
                    let mut joined = base.clone();
                    joined.truncate(joined.rfind('/').map_or(0, |at| at + 1));
                    yield Yielded::request(Request::new(format!("https://a.test{href}")));
                }
            })
        }
    }

    fn pausing_engine(
        force: bool,
        crawldir: Option<&std::path::Path>,
    ) -> (Arc<CrawlerEngine>, Arc<MockSite>) {
        let site = MockSite::new(&[
            (
                "https://a.test/",
                200,
                "<a href=\"/1\">1</a><a href=\"/2\">2</a>",
            ),
            ("https://a.test/1", 200, "<p/>"),
            ("https://a.test/2", 200, "<p/>"),
        ]);
        let spider = Arc::new(PausingSpider {
            settings: seeded(&["https://a.test/"]).with_concurrent_requests(1),
            engine: Mutex::new(None),
            presses: AtomicU32::new(0),
            force,
        });
        let mut options = RunOptions::default();
        if let Some(directory) = crawldir {
            options = options.with_crawldir(directory);
        }
        let engine = Arc::new(
            CrawlerEngine::new(
                Arc::clone(&spider) as Arc<dyn Spider>,
                manager_for(Arc::clone(&site)),
                &options,
            )
            .expect("an engine"),
        );
        *spider.engine.lock() = Some(Arc::clone(&engine));
        (engine, site)
    }

    #[tokio::test]
    async fn a_pause_with_a_crawldir_writes_a_loadable_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (engine, _site) = pausing_engine(false, Some(directory.path()));

        Arc::clone(&engine).crawl().await.expect("the crawl pauses");

        assert!(engine.paused(), "a pause with a crawldir sets paused");
        let path = directory.path().join(crate::checkpoint::CHECKPOINT_FILE);
        assert!(path.exists(), "the checkpoint survived the finally");
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        let data = manager.load().await.expect("a loadable checkpoint");
        assert!(
            !data.requests.is_empty(),
            "the unfetched links are still pending"
        );
    }

    #[tokio::test]
    async fn a_pause_without_a_crawldir_stops_gracefully_and_is_not_paused() {
        let (engine, _site) = pausing_engine(false, None);
        Arc::clone(&engine).crawl().await.expect("the crawl stops");
        assert!(!engine.paused(), "no crawldir, no pause flag");
    }

    #[tokio::test]
    async fn a_force_stop_saves_before_it_cancels() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (engine, _site) = pausing_engine(true, Some(directory.path()));

        Arc::clone(&engine).crawl().await.expect("the crawl stops");

        assert!(engine.paused());
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        assert!(
            manager.load().await.is_some(),
            "the checkpoint was written before the cancel"
        );
        assert_eq!(engine.active_tasks(), 0, "every guard ran");
    }

    /// A spider with two callbacks: one presses the pause ladder twice, the other blocks
    /// forever, so a **task is genuinely in flight** when the force stop fires.
    ///
    /// [`pausing_engine`] cannot show this: it presses from inside the only callback there
    /// is, which has already returned by the time the loop reacts, so `abort_all` has
    /// nothing to abort.
    struct BlockingSpider {
        settings: SpiderSettings,
        engine: Mutex<Option<Arc<CrawlerEngine>>>,
    }

    #[async_trait]
    impl Spider for BlockingSpider {
        fn name(&self) -> &str {
            "blocker"
        }
        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }
        fn configure_sessions(&self, _manager: &mut SessionManager) -> Result<(), CrawlError> {
            Ok(())
        }
        async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
            let holding = response.url.ends_with("/hold");
            if !holding {
                if let Some(engine) = self.engine.lock().clone() {
                    engine.request_pause();
                    engine.request_pause();
                }
            }
            Box::pin(async_stream::stream! {
                if holding {
                    // Long enough that the crawl can only finish by cancelling it.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                yield Yielded::Skip;
            })
        }
    }

    /// A force stop must checkpoint the requests that are still in flight, or they are
    /// lost: cancelling first releases their scheduler tickets, and the snapshot taken
    /// afterwards no longer knows about them.
    #[tokio::test]
    async fn a_force_stop_checkpoints_the_requests_it_is_about_to_cancel() {
        let directory = tempfile::tempdir().expect("tempdir");
        let site = MockSite::new(&[
            ("https://a.test/hold", 200, "<p/>"),
            ("https://a.test/press", 200, "<p/>"),
        ]);
        let spider = Arc::new(BlockingSpider {
            settings: seeded(&["https://a.test/hold", "https://a.test/press"])
                .with_concurrent_requests(2),
            engine: Mutex::new(None),
        });
        let engine = Arc::new(
            CrawlerEngine::new(
                Arc::clone(&spider) as Arc<dyn Spider>,
                manager_for(Arc::clone(&site)),
                &RunOptions::default().with_crawldir(directory.path()),
            )
            .expect("an engine"),
        );
        *spider.engine.lock() = Some(Arc::clone(&engine));

        // The blocked callback sleeps for 30 s; finishing at all proves it was cancelled.
        tokio::time::timeout(Duration::from_secs(10), Arc::clone(&engine).crawl())
            .await
            .expect("the force stop must not wait for the blocked callback")
            .expect("the crawl stops");

        assert!(
            engine.paused(),
            "a force stop with a crawldir is still a pause"
        );
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        let data = manager.load().await.expect("a loadable checkpoint");
        let urls: Vec<&str> = data
            .requests
            .iter()
            .map(|request| request.url.as_str())
            .collect();
        assert!(
            urls.contains(&"https://a.test/hold"),
            "the in-flight request has to survive the cancel, or resuming loses it: {urls:?}"
        );
        assert_eq!(engine.active_tasks(), 0, "every guard still ran");
    }

    #[tokio::test]
    async fn a_force_stop_without_a_crawldir_does_not_crash() {
        let (engine, _site) = pausing_engine(true, None);
        Arc::clone(&engine).crawl().await.expect("the crawl stops");
        assert!(!engine.paused());
    }

    #[tokio::test]
    async fn a_force_stop_never_deletes_an_existing_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        manager
            .save(&CheckpointData {
                requests: vec![Request::new("https://a.test/").with_sid("mock")],
                seen: Default::default(),
            })
            .await
            .expect("a pre-existing checkpoint");

        let (engine, _site) = pausing_engine(true, Some(directory.path()));
        Arc::clone(&engine).crawl().await.expect("the crawl stops");

        assert!(engine.paused());
        assert!(
            directory
                .path()
                .join(crate::checkpoint::CHECKPOINT_FILE)
                .exists(),
            "the finally must not have deleted it"
        );
        assert!(
            manager.load().await.is_some(),
            "and what is there is loadable"
        );
    }

    #[tokio::test]
    async fn a_clean_crawl_deletes_its_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        manager
            .save(&CheckpointData {
                requests: Vec::new(),
                seen: Default::default(),
            })
            .await
            .expect("a stale checkpoint");

        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::ItemPerPage);
        let engine = engine_for(
            spider,
            site,
            RunOptions::default().with_crawldir(directory.path()),
        );

        // The stale checkpoint has no requests, so this run resumes into an empty
        // frontier and finishes immediately — which is exactly what must delete the file.
        Arc::clone(&engine).crawl().await.expect("the crawl runs");
        assert!(!engine.paused());
        assert!(!directory
            .path()
            .join(crate::checkpoint::CHECKPOINT_FILE)
            .exists());
    }

    #[tokio::test]
    async fn resuming_skips_start_requests_and_keeps_the_seen_set() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        let pending = Request::new("https://a.test/resumed")
            .with_sid("mock")
            .with_callback(CallbackId::new("unknown_name"));
        pending.update_fingerprint(Default::default());
        let mut seen = std::collections::HashSet::new();
        seen.insert(pending.update_fingerprint(Default::default()));
        manager
            .save(&CheckpointData {
                requests: vec![pending],
                seen,
            })
            .await
            .expect("a checkpoint");

        let site = MockSite::new(&[
            ("https://a.test/", 200, "<p>seed</p>"),
            ("https://a.test/resumed", 200, "<p>resumed</p>"),
        ]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::ItemPerPage);
        let recorder = Arc::clone(&spider.recorder);
        let engine = engine_for(
            spider,
            Arc::clone(&site),
            RunOptions::default().with_crawldir(directory.path()),
        );

        let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");

        assert_eq!(*recorder.resuming.lock(), vec![true]);
        assert_eq!(stats.requests_count(), 1);
        assert_eq!(site.fetch_count("https://a.test/resumed"), 1);
        assert_eq!(
            site.fetch_count("https://a.test/"),
            0,
            "start_requests() was skipped"
        );
    }

    #[tokio::test]
    async fn an_unknown_callback_name_falls_back_to_parse_on_restore() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        let known = Request::new("https://a.test/a").with_callback(CallbackId::new("parse_detail"));
        let unknown = Request::new("https://a.test/b").with_callback(CallbackId::new("gone"));
        manager
            .save(&CheckpointData {
                requests: vec![known, unknown],
                seen: Default::default(),
            })
            .await
            .expect("a checkpoint");

        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(
            spider,
            site,
            RunOptions::default().with_crawldir(directory.path()),
        );

        assert!(engine.restore_from_checkpoint().await);
        let (_, first) = engine.scheduler.lock().dequeue().expect("a request");
        let (_, second) = engine.scheduler.lock().dequeue().expect("a request");
        assert_eq!(
            first.callback.as_ref().map(CallbackId::as_str),
            Some("parse_detail"),
            "a name the spider knows survives"
        );
        assert_eq!(
            second.callback.as_ref().map(CallbackId::as_str),
            Some("parse"),
            "an unknown name falls back to parse"
        );
    }

    // -- robots, cache, throttling -----------------------------------------

    #[tokio::test]
    async fn robots_txt_is_prefetched_once_per_netloc_and_gates_the_fetch() {
        let site = MockSite::new(&[
            (
                "https://a.test/robots.txt",
                200,
                "User-agent: *\nDisallow: /admin\n",
            ),
            ("https://a.test/", 200, "<a href=\"/admin/secret\">x</a>"),
            ("https://a.test/other", 200, "<p/>"),
            ("https://a.test/admin/secret", 200, "<p>secret</p>"),
        ]);
        let settings =
            seeded(&["https://a.test/", "https://a.test/other"]).with_robots_txt_obey(true);
        let spider = TestSpider::new(settings, Behaviour::FollowLinks);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

        let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");

        assert_eq!(
            site.fetch_count("https://a.test/robots.txt"),
            1,
            "one netloc, one prefetch"
        );
        assert_eq!(stats.robots_disallowed_count(), 1);
        assert_eq!(
            site.fetch_count("https://a.test/admin/secret"),
            0,
            "the gate runs before the fetch"
        );
        assert_eq!(stats.requests_count(), 2);
    }

    #[tokio::test]
    async fn robots_prefetch_is_a_no_op_without_seeds_or_without_robots() {
        let site = MockSite::new(&[]);
        let settings = SpiderSettings::default().with_robots_txt_obey(true);
        let spider = TestSpider::new(settings, Behaviour::Nothing);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());
        engine.prefetch_robots_txt().await;
        assert!(site.fetched.lock().is_empty());

        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());
        engine.prefetch_robots_txt().await;
        assert!(site.fetched.lock().is_empty(), "robots is off");
    }

    #[tokio::test]
    async fn a_domain_delay_is_resolved_once_and_cached() {
        let site = MockSite::new(&[(
            "https://a.test/robots.txt",
            200,
            "User-agent: *\nCrawl-delay: 0.01\n",
        )]);
        let settings = seeded(&["https://a.test/"])
            .with_robots_txt_obey(true)
            .with_download_delay(0.0);
        let spider = TestSpider::new(settings, Behaviour::Nothing);
        let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

        let request = Request::new("https://a.test/page").with_sid("mock");
        let first = engine.get_domain_delay(&request).await;
        let second = engine.get_domain_delay(&request).await;
        assert!((first - 0.01).abs() < f64::EPSILON, "the Crawl-delay wins");
        assert!((second - first).abs() < f64::EPSILON);
        assert_eq!(
            site.fetch_count("https://a.test/robots.txt"),
            1,
            "the second lookup is a cache read"
        );
    }

    #[tokio::test]
    async fn the_dev_cache_replays_the_second_run_without_touching_the_site() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pages: Vec<(&str, u16, &str)> = vec![
            ("https://a.test/", 200, "<a href=\"/one\">1</a>"),
            ("https://a.test/one", 200, "<p>one</p>"),
        ];

        let mut totals = Vec::new();
        for pass in 0..2 {
            let site = MockSite::new(&pages);
            let settings = seeded(&["https://a.test/"])
                .with_development_mode(true)
                .with_development_cache_dir(directory.path());
            let spider = TestSpider::new(settings, Behaviour::FollowLinks);
            let engine = engine_for(spider, Arc::clone(&site), RunOptions::default());

            let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");
            totals.push(stats.items_scraped());

            if pass == 0 {
                assert_eq!(stats.cache_misses(), 2);
                assert_eq!(stats.cache_hits(), 0);
                assert_eq!(site.fetched.lock().len(), 2);
            } else {
                assert_eq!(stats.cache_hits(), 2);
                assert_eq!(stats.cache_misses(), 0);
                assert!(site.fetched.lock().is_empty(), "nothing reached the site");
                assert_eq!(stats.requests_count(), 2, "a replay still counts");
            }
        }
        assert_eq!(totals[0], totals[1], "a replay is not a downgrade");
    }

    #[tokio::test]
    async fn autothrottle_records_the_domain_and_reaches_the_stats() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let settings = seeded(&["https://a.test/"])
            .with_autothrottle(true)
            .with_autothrottle_tuning(0.0, 0.2, Some(1.0));
        let spider = TestSpider::new(settings, Behaviour::ItemPerPage);
        let engine = engine_for(spider, site, RunOptions::default());

        let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");

        assert!(stats.autothrottle_enabled());
        assert!(stats.autothrottle_delay("a.test").is_some());
    }

    // -- streaming ---------------------------------------------------------

    #[tokio::test]
    async fn streaming_yields_items_and_leaves_engine_items_empty() {
        let site = MockSite::new(&[
            ("https://a.test/", 200, "<a href=\"/one\">1</a>"),
            ("https://a.test/one", 200, "<p>one</p>"),
        ]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::FollowLinks);
        let engine = engine_for(spider, site, RunOptions::default());

        let stream = Arc::clone(&engine).stream();
        futures::pin_mut!(stream);
        let mut urls = Vec::new();
        while let Some(item) = stream.next().await {
            urls.push(item["url"].as_str().unwrap_or_default().to_owned());
        }
        urls.sort();

        assert_eq!(urls, ["https://a.test/", "https://a.test/one"]);
        assert!(
            engine.items().is_empty(),
            "streaming mode keeps `items` empty"
        );
        assert_eq!(engine.stats().items_scraped(), 2);
    }

    #[tokio::test]
    async fn a_streamed_crawl_that_fails_ends_the_stream() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(SpiderSettings::default(), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());

        let stream = Arc::clone(&engine).stream();
        futures::pin_mut!(stream);
        assert!(stream.next().await.is_none(), "no seeds, no items");
    }

    // -- the runner --------------------------------------------------------

    #[tokio::test]
    async fn the_runner_returns_a_crawl_result() {
        use crate::spider::SpiderRunner;

        struct Runnable {
            settings: SpiderSettings,
            site: Arc<MockSite>,
        }
        #[async_trait]
        impl Spider for Runnable {
            fn name(&self) -> &str {
                "runnable"
            }
            fn settings(&self) -> &SpiderSettings {
                &self.settings
            }
            fn configure_sessions(&self, manager: &mut SessionManager) -> Result<(), CrawlError> {
                manager.add(
                    "mock",
                    Arc::clone(&self.site) as Arc<dyn CrawlSession>,
                    AddOptions::DEFAULT,
                )?;
                Ok(())
            }
            async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
                let url = response.url.clone();
                Box::pin(async_stream::stream! {
                    let mut item = Item::new();
                    item.insert("url".into(), json!(url));
                    yield Yielded::item(item);
                })
            }
        }

        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let runner = SpiderRunner::new(
            Arc::new(Runnable {
                settings: seeded(&["https://a.test/"]),
                site: Arc::clone(&site),
            }),
            RunOptions::default(),
        )
        .expect("configures");

        let result = runner.run().await.expect("the crawl runs");
        assert_eq!(result.len(), 1);
        assert!(result.completed());
        assert_eq!(result.stats.requests_count(), 1);
        assert!(runner.engine().is_none(), "the slot is cleared afterwards");
        assert!(runner.stats().is_err());
    }

    // -- the task guard ----------------------------------------------------

    #[tokio::test]
    async fn the_guard_releases_the_ticket_and_the_slot_when_dropped_unpolled() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());

        assert!(engine.enqueue(Request::new("https://a.test/x").with_sid("mock")));
        let (ticket, _request) = engine.scheduler.lock().dequeue().expect("a request");
        engine.active_tasks.fetch_add(1, Ordering::AcqRel);
        assert_eq!(engine.scheduler.lock().inflight_len(), 1);
        assert_eq!(
            engine.scheduler.lock().snapshot().0.len(),
            1,
            "an in-flight request is still checkpointed"
        );

        // This is exactly what `JoinSet::abort_all` does to a task that never ran.
        drop(TaskGuard {
            engine: Arc::clone(&engine),
            ticket,
        });

        assert_eq!(engine.active_tasks(), 0);
        assert_eq!(engine.scheduler.lock().inflight_len(), 0);
        assert!(engine.scheduler.lock().snapshot().0.is_empty());
    }

    #[tokio::test]
    async fn a_crawl_of_nothing_but_failures_still_drains() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(
            seeded(&["https://a.test/", "https://a.test/b"]),
            Behaviour::ItemPerPage,
        );
        let recorder = Arc::clone(&spider.recorder);
        let engine = engine_for(spider, site, RunOptions::default());

        let stats = Arc::clone(&engine).crawl().await.expect("the crawl runs");

        assert_eq!(stats.failed_requests_count(), 2);
        assert_eq!(stats.requests_count(), 0);
        assert_eq!(recorder.errors.lock().len(), 2);
        assert_eq!(engine.active_tasks(), 0, "every guard ran");
        assert_eq!(engine.queued(), 0);
    }

    #[tokio::test]
    async fn restore_is_false_when_the_checkpoint_system_is_off() {
        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(spider, site, RunOptions::default());
        assert!(!engine.restore_from_checkpoint().await);
    }

    #[tokio::test]
    async fn a_restored_seen_set_still_deduplicates() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(directory.path(), 300.0).expect("a manager");
        let already = Request::new("https://a.test/seen").with_sid("mock");
        let mut seen = std::collections::HashSet::new();
        seen.insert(already.update_fingerprint(Default::default()));
        manager
            .save(&CheckpointData {
                requests: Vec::new(),
                seen,
            })
            .await
            .expect("a checkpoint");

        let site = MockSite::new(&[]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::Nothing);
        let engine = engine_for(
            spider,
            site,
            RunOptions::default().with_crawldir(directory.path()),
        );

        assert!(engine.restore_from_checkpoint().await);
        assert!(
            !engine.enqueue(Request::new("https://a.test/seen").with_sid("mock")),
            "the restored seen set drops it"
        );
        assert!(engine.enqueue(Request::new("https://a.test/fresh").with_sid("mock")));
    }

    // -- callback routing --------------------------------------------------

    struct DispatchSpider {
        settings: SpiderSettings,
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Spider for DispatchSpider {
        fn name(&self) -> &str {
            "dispatcher"
        }
        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }
        fn configure_sessions(&self, _manager: &mut SessionManager) -> Result<(), CrawlError> {
            Ok(())
        }
        async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
            self.seen.lock().push("parse".to_owned());
            Box::pin(futures::stream::empty())
        }
        async fn dispatch(
            self: Arc<Self>,
            callback: CallbackId,
            response: CrawlResponse,
        ) -> CallbackStream {
            self.seen.lock().push(callback.as_str().to_owned());
            self.parse(response).await
        }
    }

    #[tokio::test]
    async fn a_named_callback_goes_through_dispatch_and_a_bare_one_does_not() {
        let site = MockSite::new(&[("https://a.test/", 200, "<p>ok</p>")]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let spider = Arc::new(DispatchSpider {
            settings: seeded(&["https://a.test/"]),
            seen: Arc::clone(&seen),
        });
        let engine = Arc::new(
            CrawlerEngine::new(
                spider as Arc<dyn Spider>,
                manager_for(Arc::clone(&site)),
                &RunOptions::default(),
            )
            .expect("an engine"),
        );

        engine
            .process_request(
                &Request::new("https://a.test/")
                    .with_sid("mock")
                    .with_callback(CallbackId::new("parse_detail")),
            )
            .await;
        engine
            .process_request(&Request::new("https://a.test/").with_sid("mock"))
            .await;

        assert_eq!(*seen.lock(), ["parse_detail", "parse", "parse"]);
    }

    // -- streaming, live stats ---------------------------------------------

    #[tokio::test]
    async fn stats_are_live_while_streaming() {
        let site = MockSite::new(&[
            ("https://a.test/", 200, "<a href=\"/one\">1</a>"),
            ("https://a.test/one", 200, "<p/>"),
        ]);
        let spider = TestSpider::new(seeded(&["https://a.test/"]), Behaviour::FollowLinks);
        let engine = engine_for(spider, site, RunOptions::default());

        let stream = Arc::clone(&engine).stream();
        futures::pin_mut!(stream);
        let first = stream.next().await.expect("one item");
        assert!(first.contains_key("url"));
        assert!(
            engine.stats().items_scraped() >= 1,
            "the stats object is the live one"
        );
        while stream.next().await.is_some() {}
        assert_eq!(engine.stats().items_scraped(), 2);
    }

    // -- end to end, over a real socket ------------------------------------

    /// A loopback HTTP server: raw sockets, one connection per request.
    ///
    /// The mock session above proves the *engine*; this proves the whole stack, unit 12's
    /// pooled transport and [`crate::spider::HttpCrawlSession`] included, without ever
    /// leaving the machine.
    mod fixture {
        use std::io::{BufRead, BufReader, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        pub(super) struct Server {
            pub(super) port: u16,
            hits: Arc<Mutex<Vec<String>>>,
            running: Arc<AtomicBool>,
        }

        impl Server {
            pub(super) fn start() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let port = listener.local_addr().expect("addr").port();
                let hits = Arc::new(Mutex::new(Vec::new()));
                let running = Arc::new(AtomicBool::new(true));
                let thread_hits = Arc::clone(&hits);
                let thread_running = Arc::clone(&running);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        if !thread_running.load(Ordering::Acquire) {
                            break;
                        }
                        match stream {
                            // One thread per connection: a client that opens a socket
                            // speculatively and never writes must not wedge `accept`,
                            // which is exactly what a pooled, concurrent fetcher does.
                            Ok(stream) => {
                                let hits = Arc::clone(&thread_hits);
                                std::thread::spawn(move || serve(stream, &hits));
                            }
                            Err(_) => break,
                        }
                    }
                });
                Self {
                    port,
                    hits,
                    running,
                }
            }

            pub(super) fn url(&self, path: &str) -> String {
                format!("http://127.0.0.1:{}{path}", self.port)
            }

            pub(super) fn hit_count(&self, path: &str) -> usize {
                self.hits
                    .lock()
                    .expect("not poisoned")
                    .iter()
                    .filter(|hit| *hit == path)
                    .count()
            }
        }

        impl Drop for Server {
            fn drop(&mut self) {
                self.running.store(false, Ordering::Release);
                let _ = TcpStream::connect(("127.0.0.1", self.port));
            }
        }

        fn page(links: &[&str], item: &str) -> String {
            let anchors: String = links
                .iter()
                .map(|href| format!(r#"<a href="{href}">{href}</a>"#))
                .collect();
            format!(
                r#"<!doctype html><html><body><h1 class="item">{item}</h1>{anchors}</body></html>"#
            )
        }

        fn route(path: &str) -> (u16, String) {
            match path {
                "/robots.txt" => (200, "User-agent: *\nDisallow: /admin\n".to_owned()),
                "/" => (200, page(&["/a", "/b", "/admin/secret", "/a"], "home")),
                "/a" => (200, page(&["/", "/b"], "alpha")),
                "/b" => (200, page(&[], "beta")),
                "/admin/secret" => (200, page(&[], "SECRET")),
                "/throttled" => (429, page(&[], "slow down")),
                _ => (404, "not found".to_owned()),
            }
        }

        fn serve(mut stream: TcpStream, hits: &Arc<Mutex<Vec<String>>>) {
            // So an idle connection is reaped instead of holding a thread for the run.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
                return;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_owned();
            hits.lock().expect("not poisoned").push(path.clone());

            let (status, body) = route(&path);
            let extra = if status == 429 {
                "Retry-After: 0\r\n"
            } else {
                ""
            };
            let content_type = if path == "/robots.txt" {
                "text/plain; charset=utf-8"
            } else {
                "text/html; charset=utf-8"
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n\
                 Content-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
            // Half-close, then drain until the peer closes. `Shutdown::Both` here can send
            // an RST that throws away the response we just wrote, which is a rare but real
            // source of "connection closed before message completed".
            let _ = stream.shutdown(Shutdown::Write);
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let mut sink = [0_u8; 256];
            while matches!(std::io::Read::read(&mut stream, &mut sink), Ok(n) if n > 0) {}
        }
    }

    struct SiteSpider {
        settings: SpiderSettings,
        session: Mutex<Option<crate::spider::HttpCrawlSession>>,
    }

    #[async_trait]
    impl Spider for SiteSpider {
        fn name(&self) -> &str {
            "site"
        }
        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }
        fn configure_sessions(&self, manager: &mut SessionManager) -> Result<(), CrawlError> {
            let session = self
                .session
                .lock()
                .take()
                .ok_or_else(|| CrawlError::callback("configure_sessions ran twice"))?;
            manager.add("http", Arc::new(session), AddOptions::DEFAULT)?;
            Ok(())
        }
        async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
            let titles: Vec<String> = response
                .css("h1.item::text")
                .map(|nodes| nodes.texts())
                .unwrap_or_default();
            let hrefs: Vec<String> = response
                .css("a")
                .map(|nodes| nodes.attrs("href"))
                .unwrap_or_default();
            let follow_ups: Vec<Request> = hrefs
                .iter()
                .filter_map(|href| response.follow(href).ok())
                .collect();
            Box::pin(async_stream::stream! {
                for title in titles {
                    let mut item = Item::new();
                    item.insert("title".into(), json!(title));
                    yield Yielded::item(item);
                }
                for follow_up in follow_ups {
                    yield Yielded::request(follow_up);
                }
            })
        }
    }

    #[tokio::test]
    async fn a_real_crawl_over_a_loopback_server() {
        use crate::spider::{HttpCrawlSession, SpiderRunner};
        use browser_http::config::FetcherConfig;
        use browser_http::session::AsyncFetcherSession;

        let server = fixture::Server::start();
        // 30 s, not 5 s, and it is a *load* margin rather than a latency one. The
        // fixture is on loopback and answers in microseconds; but a `robots.txt`
        // fetch that times out is treated as "allow everything" (faithfully —
        // Python does the same), and then `/admin/secret` is crawled and the
        // titles assertion below fails for a reason that has nothing to do with
        // the engine. That was observed once, under a full `cargo test
        // --workspace` on a loaded machine, in wave 6.
        let transport = AsyncFetcherSession::builder()
            .config(
                FetcherConfig::default()
                    .with_retries(Some(1))
                    .with_timeout(Some(Duration::from_secs(30))),
            )
            .build()
            .expect("a session over the default backend");

        let settings = SpiderSettings::default()
            .with_start_urls([server.url("/"), server.url("/throttled")])
            .with_robots_txt_obey(true)
            .with_concurrent_requests(2)
            .with_max_blocked_retries(1);
        let spider = Arc::new(SiteSpider {
            settings,
            session: Mutex::new(Some(HttpCrawlSession::with_session("http", transport))),
        });

        let runner = SpiderRunner::new(spider, RunOptions::default().with_ctrl_c(false))
            .expect("configures");
        let result = runner.run().await.expect("the crawl runs");

        let mut titles: Vec<String> = result
            .items
            .iter()
            .filter_map(|item| item["title"].as_str().map(str::to_owned))
            .collect();
        titles.sort();
        assert_eq!(titles, ["alpha", "beta", "home"]);

        // The robots gate runs before the fetch, not after it.
        assert_eq!(server.hit_count("/admin/secret"), 0);
        assert_eq!(result.stats.robots_disallowed_count(), 1);
        assert_eq!(
            server.hit_count("/robots.txt"),
            1,
            "one netloc, one prefetch"
        );

        // Fingerprint de-duplication: `/a` is linked twice from `/`, `/b` from two pages.
        assert_eq!(server.hit_count("/"), 1);
        assert_eq!(server.hit_count("/a"), 1);
        assert_eq!(server.hit_count("/b"), 1);

        // The 429 is blocked, retried once, then given up on — and never reaches a
        // callback, so it contributes no item.
        assert_eq!(server.hit_count("/throttled"), 2);
        assert_eq!(result.stats.blocked_requests_count(), 2);
        assert_eq!(result.stats.status_count(429), 2);
        // Three 200s and two 429s: the `robots.txt` fetch goes straight through the
        // session manager and is deliberately not counted, and `/admin/secret` was
        // refused before any fetch happened.
        assert_eq!(result.stats.status_count(200), 3);
        assert_eq!(result.stats.requests_count(), 5);
        assert!(result.stats.response_bytes() > 0);
        assert_eq!(result.stats.session_requests("http"), 5);
        assert!(result.completed());
        drop(server);
    }
}
