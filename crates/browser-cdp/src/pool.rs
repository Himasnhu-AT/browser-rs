//! [`PagePool`] / [`PageInfo`]: the page state machine and its exact error
//! strings. Zero CDP.
//!
//! This is the Rust shape of `reference/engines/_browsers/_page.py`, whose
//! mechanics are pinned by `tests/fetchers/test_pages.py` and
//! `tests/fetchers/test_page_reuse.py`.
//!
//! # The state machine
//!
//! ```text
//!             add_page()                 mark_ready()
//!   (nothing) ──────────► Busy ◄──────────────────────── Ready
//!                          │  ▲                            ▲
//!             mark_error() │  └──────── get_ready_page() ───┘
//!                          ▼
//!                        Error ──► closed and evicted by the caller
//! ```
//!
//! * A **newly added** page starts [`PageState::Busy`] — "a new page is busy for
//!   the request that opened it".
//! * [`PagePool::get_ready_page`] hands out the *first* [`PageState::Ready`]
//!   page and marks it busy; it skips busy **and** errored pages, so a second
//!   call with nothing free returns `None`.
//! * [`PageState::Error`] is terminal: the session closes that tab and calls
//!   [`PagePool::remove_page`], so the next request opens a fresh one.
//!
//! # Shared state, not snapshots
//!
//! Python hands out the *same* `PageInfo` object the pool holds, so
//! `page_info.mark_error()` is immediately visible to `get_ready_page()`. The
//! port keeps that: [`PageInfo`] is a cheap handle whose state lives behind an
//! `Arc<Mutex<…>>` shared with the pool's copy. Cloning a [`PageInfo`] clones
//! the handle, never the state.
//!
//! The mutex is `parking_lot`'s, which does not poison — Python's `RLock` does
//! not either, and a panicking route handler must not wedge the pool.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::error::PoolError;

/// Python's `_max_wait_for_page`: how long the async acquire path waits for a
/// tab to come free, in **seconds**.
pub const DEFAULT_MAX_WAIT_FOR_PAGE: u64 = 60;

/// How often [`PagePool::wait_for_ready_page`] re-checks the pool.
///
/// Python's loop is `await asyncio.sleep(0.05)`.
pub const PAGE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Python's `PagePool(max_pages=5)` default.
///
/// Note that no the reference implementation session ever uses it: `SyncSession` passes `1` and
/// `AsyncSession` passes the validated `max_pages`.
pub const DEFAULT_POOL_SIZE: usize = 5;

/// The state a tab can be in (`PageState = Literal["ready", "busy", "error"]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PageState {
    /// Idle and reusable by the next request.
    #[default]
    Ready,
    /// Serving a request right now.
    Busy,
    /// The request on this tab failed; the tab must be closed and evicted.
    Error,
}

impl PageState {
    /// The lowercase name, exactly as Python spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for PageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A page's mutable half: what [`PageInfo`] shares with the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageStatus {
    /// The current state.
    pub state: PageState,
    /// The URL the current request is for. `Some("")` between requests,
    /// matching Python's `mark_ready()`.
    pub url: Option<String>,
}

/// A monotonically increasing identity for a pooled page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(u64);

impl PageId {
    /// The raw counter value, for logging.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);

fn next_page_id() -> PageId {
    PageId(NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed))
}

/// A tab plus its state (`PageInfo`).
///
/// `P` is the driver's page handle — `chromiumoxide::Page` in unit 14, a test
/// double here. It is only ever cloned, never inspected, so the pool stays free
/// of CDP.
#[derive(Debug)]
pub struct PageInfo<P> {
    page: P,
    id: PageId,
    status: Arc<Mutex<PageStatus>>,
}

impl<P: Clone> Clone for PageInfo<P> {
    /// Clones the *handle*: the clone shares one state cell with the original,
    /// which is what makes `page_info.mark_error()` visible to the pool.
    fn clone(&self) -> Self {
        Self {
            page: self.page.clone(),
            id: self.id,
            status: Arc::clone(&self.status),
        }
    }
}

/// Identity comparison.
///
/// Python compares `self.page == other.page`, i.e. the underlying Playwright
/// object's identity. The port compares the pool-assigned [`PageId`] instead,
/// because a driver page handle need not implement `PartialEq`. The one visible
/// difference: wrapping the *same* driver page in two separately constructed
/// [`PageInfo`]s makes them unequal here and equal in Python. No the reference implementation code
/// path does that.
impl<P> PartialEq for PageInfo<P> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<P> Eq for PageInfo<P> {}

impl<P> PageInfo<P> {
    /// Wrap a page, in the state a freshly opened tab starts in
    /// ([`PageState::Busy`], empty URL).
    #[must_use]
    pub fn new(page: P) -> Self {
        Self {
            page,
            id: next_page_id(),
            status: Arc::new(Mutex::new(PageStatus {
                state: PageState::Busy,
                url: Some(String::new()),
            })),
        }
    }

    /// The driver's page handle.
    #[must_use]
    pub const fn page(&self) -> &P {
        &self.page
    }

    /// Consume the handle, returning the driver's page.
    #[must_use]
    pub fn into_page(self) -> P {
        self.page
    }

    /// This page's pool identity.
    #[must_use]
    pub const fn id(&self) -> PageId {
        self.id
    }

    /// A snapshot of the shared state.
    #[must_use]
    pub fn status(&self) -> PageStatus {
        self.status.lock().clone()
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> PageState {
        self.status.lock().state
    }

    /// The URL the current request is for.
    #[must_use]
    pub fn url(&self) -> Option<String> {
        self.status.lock().url.clone()
    }

    /// `mark_busy`: state `busy`, URL set (Python defaults it to `""`).
    pub fn mark_busy(&self, url: &str) {
        let mut status = self.status.lock();
        status.state = PageState::Busy;
        status.url = Some(url.to_owned());
    }

    /// `mark_ready`: state `ready`, URL cleared to `""`.
    pub fn mark_ready(&self) {
        let mut status = self.status.lock();
        status.state = PageState::Ready;
        status.url = Some(String::new());
    }

    /// `mark_error`: state `error`, **URL untouched** so the failing URL stays
    /// visible in logs.
    pub fn mark_error(&self) {
        self.status.lock().state = PageState::Error;
    }
}

/// Python's `__repr__`: `Page(URL="{url!r}", state={state!r})`.
///
/// The nested quoting is Python's, not a typo — `!r` on a `str` adds its own
/// single quotes inside the double-quoted literal.
///
/// ```
/// use browser_cdp::pool::PageInfo;
///
/// let info = PageInfo::new(());
/// assert_eq!(info.to_string(), r#"Page(URL="''", state='busy')"#);
/// info.mark_busy("https://example.com");
/// assert_eq!(
///     info.to_string(),
///     r#"Page(URL="'https://example.com'", state='busy')"#
/// );
/// ```
impl<P> std::fmt::Display for PageInfo<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self.status.lock();
        let url = match status.url.as_deref() {
            None => "None".to_owned(),
            Some(url) => py_repr(url),
        };
        write!(f, "Page(URL=\"{url}\", state='{}')", status.state)
    }
}

/// CPython's `repr()` for the subset of strings a URL can be.
///
/// Single quotes unless the value contains a `'` and no `"`, backslash and
/// control characters escaped.
fn py_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `get_pool_stats()`: `{"total_pages", "busy_pages", "max_pages"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    /// Tabs currently in the pool, in any state.
    pub total_pages: usize,
    /// Tabs in [`PageState::Busy`].
    pub busy_pages: usize,
    /// The pool's capacity.
    pub max_pages: usize,
}

impl PoolStats {
    /// The dict Python returns, with the keys in its insertion order.
    #[must_use]
    pub fn to_map(self) -> indexmap::IndexMap<&'static str, usize> {
        let mut map = indexmap::IndexMap::with_capacity(3);
        map.insert("total_pages", self.total_pages);
        map.insert("busy_pages", self.busy_pages);
        map.insert("max_pages", self.max_pages);
        map
    }
}

/// A pool of browser tabs with state tracking.
///
/// Every method takes `&self`: the pool is meant to live behind an `Arc` and be
/// shared by concurrent requests, exactly as Python's is shared by concurrent
/// `fetch()` coroutines.
#[derive(Debug)]
pub struct PagePool<P> {
    max_pages: usize,
    pages: Mutex<Vec<PageInfo<P>>>,
}

impl<P: Clone> Default for PagePool<P> {
    /// A pool of [`DEFAULT_POOL_SIZE`], matching `PagePool.__init__`.
    fn default() -> Self {
        Self::new(DEFAULT_POOL_SIZE)
    }
}

impl<P: Clone> PagePool<P> {
    /// A pool that holds at most `max_pages` tabs.
    #[must_use]
    pub fn new(max_pages: usize) -> Self {
        Self {
            max_pages,
            pages: Mutex::new(Vec::new()),
        }
    }

    /// The pool's capacity.
    #[must_use]
    pub const fn max_pages(&self) -> usize {
        self.max_pages
    }

    /// Add a new tab, marked [`PageState::Busy`] for the request that opened it.
    ///
    /// # Errors
    ///
    /// [`PoolError::MaxPagesReached`] — `"Maximum page limit (N) reached"` —
    /// when the pool is already full.
    pub fn add_page(&self, page: P) -> Result<PageInfo<P>, PoolError> {
        let mut pages = self.pages.lock();
        if pages.len() >= self.max_pages {
            return Err(PoolError::MaxPagesReached {
                max_pages: self.max_pages,
            });
        }
        let info = PageInfo::new(page);
        pages.push(info.clone());
        Ok(info)
    }

    /// Take the first [`PageState::Ready`] tab, marking it busy.
    ///
    /// Busy and errored tabs are skipped, so a pool with nothing free yields
    /// `None`.
    #[must_use]
    pub fn get_ready_page(&self) -> Option<PageInfo<P>> {
        let pages = self.pages.lock();
        for info in pages.iter() {
            let mut status = info.status.lock();
            if status.state == PageState::Ready {
                status.state = PageState::Busy;
                status.url = Some(String::new());
                drop(status);
                return Some(info.clone());
            }
        }
        None
    }

    /// Forget a tab, whether it is still in the pool or not.
    ///
    /// Returns whether anything was removed; Python's version is a silent
    /// no-op for an unknown page and is called that way in the rotation path.
    pub fn remove_page(&self, page_info: &PageInfo<P>) -> bool {
        let mut pages = self.pages.lock();
        let before = pages.len();
        pages.retain(|candidate| candidate != page_info);
        pages.len() != before
    }

    /// Forget every tab and return them so the caller can close them.
    ///
    /// The returned list is in insertion order.
    #[must_use]
    pub fn clear(&self) -> Vec<PageInfo<P>> {
        std::mem::take(&mut *self.pages.lock())
    }

    /// Total number of tabs, in any state.
    #[must_use]
    pub fn pages_count(&self) -> usize {
        self.pages.lock().len()
    }

    /// Number of tabs in [`PageState::Busy`].
    #[must_use]
    pub fn busy_count(&self) -> usize {
        self.pages
            .lock()
            .iter()
            .filter(|info| info.status.lock().state == PageState::Busy)
            .count()
    }

    /// Whether the pool is at capacity.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.pages.lock().len() >= self.max_pages
    }

    /// A snapshot for `get_pool_stats()`.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        let pages = self.pages.lock();
        PoolStats {
            total_pages: pages.len(),
            busy_pages: pages
                .iter()
                .filter(|info| info.status.lock().state == PageState::Busy)
                .count(),
            max_pages: self.max_pages,
        }
    }

    /// A snapshot of every tab, in insertion order, without emptying the pool.
    #[must_use]
    pub fn pages(&self) -> Vec<PageInfo<P>> {
        self.pages.lock().clone()
    }

    /// Poll for a free tab, the way the async session does when the pool is at
    /// capacity.
    ///
    /// Python sleeps 50 ms between checks for up to `_max_wait_for_page`
    /// seconds and then raises
    /// `TimeoutError("No pages finished to clear place in the pool within the
    /// {n}s timeout period")`. The port keeps both numbers and the message; it
    /// checks *before* the first sleep as well, which can only make the happy
    /// path faster.
    ///
    /// # Errors
    ///
    /// [`PoolError::NoFreePage`] once the budget elapses.
    pub async fn wait_for_ready_page(&self, max_wait: Duration) -> Result<PageInfo<P>, PoolError> {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            if let Some(info) = self.get_ready_page() {
                return Ok(info);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PoolError::NoFreePage {
                    // Python interpolates `self._max_wait_for_page`, an int.
                    timeout_secs: max_wait.as_secs(),
                });
            }
            tokio::time::sleep(PAGE_POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a driver page handle: cheap, cloneable, comparable.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct FakePage(&'static str);

    fn pool(max: usize) -> PagePool<FakePage> {
        PagePool::new(max)
    }

    #[test]
    fn a_new_page_is_busy_for_the_request_that_opened_it() {
        let pool = pool(2);
        let info = pool.add_page(FakePage("a")).expect("room for one");
        assert_eq!(info.state(), PageState::Busy);
        assert_eq!(info.url().as_deref(), Some(""));
        assert_eq!(pool.pages_count(), 1);
        assert_eq!(pool.busy_count(), 1);
    }

    #[test]
    fn adding_beyond_max_pages_raises_with_pythons_message() {
        let pool = pool(2);
        pool.add_page(FakePage("a")).expect("room");
        pool.add_page(FakePage("b")).expect("room");
        let err = pool.add_page(FakePage("c")).unwrap_err();
        assert_eq!(err, PoolError::MaxPagesReached { max_pages: 2 });
        assert_eq!(err.to_string(), "Maximum page limit (2) reached");
        assert!(pool.is_full());
    }

    #[test]
    fn remove_page_frees_a_slot_and_is_idempotent() {
        let pool = pool(1);
        let info = pool.add_page(FakePage("a")).expect("room");
        assert!(pool.remove_page(&info));
        assert_eq!(pool.pages_count(), 0);
        // Removing again, or removing something that was never in the pool,
        // is a silent no-op.
        assert!(!pool.remove_page(&info));
        let foreign = PageInfo::new(FakePage("z"));
        assert!(!pool.remove_page(&foreign));
        // The freed slot is usable.
        pool.add_page(FakePage("b")).expect("slot was freed");
    }

    #[test]
    fn get_ready_page_skips_busy_and_error_pages() {
        let pool = pool(3);
        let busy = pool.add_page(FakePage("busy")).expect("room");
        let errored = pool.add_page(FakePage("err")).expect("room");
        let ready = pool.add_page(FakePage("ready")).expect("room");
        errored.mark_error();
        ready.mark_ready();

        let taken = pool.get_ready_page().expect("one ready page");
        assert_eq!(taken.page(), &FakePage("ready"));
        assert_eq!(
            taken.state(),
            PageState::Busy,
            "handing it out marks it busy"
        );
        assert_eq!(
            ready.state(),
            PageState::Busy,
            "state is shared, not copied"
        );

        // Nothing left.
        assert!(pool.get_ready_page().is_none());
        assert_eq!(busy.state(), PageState::Busy);
        assert_eq!(errored.state(), PageState::Error);
    }

    #[test]
    fn get_ready_page_returns_the_first_ready_page() {
        let pool = pool(3);
        let first = pool.add_page(FakePage("1")).expect("room");
        let second = pool.add_page(FakePage("2")).expect("room");
        first.mark_ready();
        second.mark_ready();
        assert_eq!(
            pool.get_ready_page().expect("a page").page(),
            &FakePage("1")
        );
        assert_eq!(
            pool.get_ready_page().expect("a page").page(),
            &FakePage("2")
        );
    }

    #[test]
    fn clear_returns_every_page_in_insertion_order_and_empties_the_pool() {
        let pool = pool(3);
        pool.add_page(FakePage("1")).expect("room");
        pool.add_page(FakePage("2")).expect("room");
        pool.add_page(FakePage("3")).expect("room");
        let cleared = pool.clear();
        assert_eq!(
            cleared.iter().map(PageInfo::page).collect::<Vec<_>>(),
            [&FakePage("1"), &FakePage("2"), &FakePage("3")]
        );
        assert_eq!(pool.pages_count(), 0);
        assert!(pool.clear().is_empty());
        // The pool is reusable afterwards.
        pool.add_page(FakePage("4")).expect("room again");
    }

    #[test]
    fn state_transitions_follow_python() {
        let info = PageInfo::new(FakePage("a"));
        assert_eq!(info.state(), PageState::Busy);

        info.mark_ready();
        assert_eq!(
            info.status(),
            PageStatus {
                state: PageState::Ready,
                url: Some(String::new())
            }
        );

        info.mark_busy("https://example.com");
        assert_eq!(
            info.status(),
            PageStatus {
                state: PageState::Busy,
                url: Some("https://example.com".into())
            }
        );

        info.mark_error();
        assert_eq!(
            info.status(),
            PageStatus {
                state: PageState::Error,
                // mark_error leaves the URL alone.
                url: Some("https://example.com".into())
            },
        );

        info.mark_ready();
        assert_eq!(info.url().as_deref(), Some(""), "mark_ready clears the URL");
    }

    #[test]
    fn page_state_spells_itself_like_python() {
        assert_eq!(PageState::Ready.as_str(), "ready");
        assert_eq!(PageState::Busy.as_str(), "busy");
        assert_eq!(PageState::Error.as_str(), "error");
        assert_eq!(PageState::default(), PageState::Ready);
    }

    #[test]
    fn repr_contains_the_state_and_the_url() {
        let info = PageInfo::new(FakePage("a"));
        assert_eq!(info.to_string(), r#"Page(URL="''", state='busy')"#);
        info.mark_busy("https://example.com/a?b=c");
        assert_eq!(
            info.to_string(),
            r#"Page(URL="'https://example.com/a?b=c'", state='busy')"#
        );
        info.mark_error();
        assert!(info.to_string().contains("state='error'"));
    }

    #[test]
    fn py_repr_matches_cpython_for_awkward_urls() {
        assert_eq!(py_repr(""), "''");
        assert_eq!(py_repr("a'b"), "\"a'b\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn equality_is_by_identity_and_survives_cloning() {
        let a = PageInfo::new(FakePage("x"));
        let clone = a.clone();
        let b = PageInfo::new(FakePage("x"));
        assert_eq!(a, clone, "a clone is the same pooled page");
        assert_ne!(a, b, "two tabs are two tabs even with equal handles");
        assert_ne!(a.id(), b.id());
        assert!(a.id().get() > 0);
    }

    #[test]
    fn clones_share_one_state_cell() {
        let a = PageInfo::new(FakePage("x"));
        let clone = a.clone();
        clone.mark_error();
        assert_eq!(a.state(), PageState::Error);
    }

    #[test]
    fn stats_report_totals_busy_and_capacity() {
        let pool = pool(4);
        let one = pool.add_page(FakePage("1")).expect("room");
        pool.add_page(FakePage("2")).expect("room");
        one.mark_ready();
        let stats = pool.stats();
        assert_eq!(
            stats,
            PoolStats {
                total_pages: 2,
                busy_pages: 1,
                max_pages: 4
            }
        );
        let map = stats.to_map();
        assert_eq!(
            map.keys().copied().collect::<Vec<_>>(),
            ["total_pages", "busy_pages", "max_pages"]
        );
        assert_eq!(map["busy_pages"], 1);
        assert_eq!(pool.max_pages(), 4);
        assert_eq!(pool.pages().len(), 2);
    }

    #[test]
    fn default_pool_size_matches_python() {
        let pool: PagePool<FakePage> = PagePool::default();
        assert_eq!(pool.max_pages(), 5);
    }

    #[tokio::test]
    async fn waiting_for_a_free_page_times_out_with_pythons_message() {
        // A real 60 s budget would make the suite unusable and `start_paused`
        // needs tokio's `test-util` feature, which this crate does not enable,
        // so the budget is shortened here. `DEFAULT_MAX_WAIT_FOR_PAGE` and the
        // full 60 s message are asserted in `error.rs`.
        assert_eq!(DEFAULT_MAX_WAIT_FOR_PAGE, 60);
        let pool = pool(1);
        pool.add_page(FakePage("busy")).expect("room");
        let err = pool
            .wait_for_ready_page(Duration::from_millis(150))
            .await
            .unwrap_err();
        assert_eq!(err, PoolError::NoFreePage { timeout_secs: 0 });
        assert_eq!(
            PoolError::NoFreePage {
                timeout_secs: DEFAULT_MAX_WAIT_FOR_PAGE
            }
            .to_string(),
            "No pages finished to clear place in the pool within the 60s timeout period"
        );
    }

    #[tokio::test]
    async fn waiting_returns_as_soon_as_a_page_is_freed() {
        let pool = Arc::new(pool(1));
        let held = pool.add_page(FakePage("busy")).expect("room");

        let waiter = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move { pool.wait_for_ready_page(Duration::from_secs(5)).await })
        };

        tokio::time::sleep(Duration::from_millis(120)).await;
        held.mark_ready();

        let acquired = waiter.await.expect("task joins").expect("a free page");
        assert_eq!(acquired.page(), &FakePage("busy"));
        assert_eq!(acquired.state(), PageState::Busy);
    }

    #[tokio::test]
    async fn waiting_succeeds_immediately_when_a_page_is_already_free() {
        let pool = pool(1);
        let info = pool.add_page(FakePage("a")).expect("room");
        info.mark_ready();
        assert!(pool
            .wait_for_ready_page(Duration::from_secs(60))
            .await
            .is_ok());
    }
}
