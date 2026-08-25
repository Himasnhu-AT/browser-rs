//! Playwright's `networkidle` load state, reimplemented on Network-domain
//! events.
//!
//! There is no CDP primitive for `networkidle`. Playwright synthesises it in
//! `packages/playwright-core/src/server/frames.ts`:
//!
//! ```text
//! _onRequestStarted(request)  { frame._inflightRequests.add(request);
//!                               if (size === 1) frame._stopNetworkIdleTimer(); }
//! _onRequestFinished(request) { frame._inflightRequests.delete(request);
//!                               if (size === 0) frame._startNetworkIdleTimer(); }
//! _startNetworkIdleTimer()    { setTimeout(() => fire('networkidle'), 500); }
//! ```
//!
//! so the rule is exactly: **the number of in-flight requests has been zero,
//! continuously, for 500 ms**. Not "≤ 2" — that is Puppeteer's `networkidle2` —
//! and not "zero right now", which would fire between two resources of the same
//! page. [`NETWORK_IDLE_QUIET_PERIOD`] is that 500 ms and
//! [`NetworkTracker::wait_for_idle`] is that timer.
//!
//! # Mapping CDP events onto "started" and "finished"
//!
//! | CDP event | effect |
//! |---|---|
//! | `Network.requestWillBeSent` | [`NetworkTracker::request_started`] |
//! | `Network.loadingFinished` | [`NetworkTracker::request_finished`] |
//! | `Network.loadingFailed` | [`NetworkTracker::request_finished`] |
//! | `Page.lifecycleEvent` (`init`, main frame) | [`NetworkTracker::reset`] |
//!
//! The last row is Playwright's `_onClearLifecycle`: on a committed new-document
//! navigation it throws away the previous document's in-flight requests and
//! restarts the 500 ms timer, so a page that navigates away mid-download can
//! still reach `networkidle`.
//!
//! Favicon requests are excluded, exactly as Playwright excludes them
//! (`Request._isFavicon = url.endsWith('/favicon.ico')`) — Chrome fetches one
//! for every top-level page and a slow or hanging favicon would otherwise stop
//! `networkidle` from ever firing. See [`is_favicon`].
//!
//! A redirect re-uses its `requestId`, and `requestWillBeSent` fires again with
//! `redirectResponse` set. Because in-flight requests are tracked as a *set of
//! request ids*, that re-entry is a no-op and the count stays at one for the
//! whole chain — which is what Playwright ends up with too (it closes the old
//! `Request` object and opens a new one in the same tick).
//!
//! # Why a set and not a counter
//!
//! Each CDP event type reaches us through its own `EventStream`, so two events
//! that Chrome emitted in order can be *observed* out of order. A counter would
//! then drift permanently negative or positive. A set is idempotent, and
//! [`NetworkTracker::request_finished`] remembers ids it has never seen started
//! so that a late `requestWillBeSent` for an already-finished request does not
//! leak an entry that never clears.
//!
//! # Timeouts are not errors
//!
//! Both Python call sites wrap the wait in `try/except: pass` — "never-ending
//! requests must not break the fetch". [`NetworkTracker::wait_for_idle`]
//! therefore returns an [`IdleOutcome`] rather than a `Result`, so the caller
//! cannot accidentally propagate a timeout.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Playwright's quiet period: no in-flight requests for this long is `networkidle`.
pub const NETWORK_IDLE_QUIET_PERIOD: Duration = Duration::from_millis(500);

/// The upper bound on the out-of-order compensation set.
///
/// Each entry is a request id whose completion was observed before its start.
/// The set is normally empty and always drained by the matching start, but a
/// long-lived tab must not accumulate them without limit.
const FINISHED_EARLY_CAPACITY: usize = 4_096;

/// Playwright's `Request._isFavicon`.
///
/// ```
/// use browser_cdp::cdp::networkidle::is_favicon;
///
/// assert!(is_favicon("https://shop.test/favicon.ico"));
/// assert!(!is_favicon("https://shop.test/img/favicon.ico.png"));
/// assert!(!is_favicon("https://shop.test/"));
/// ```
#[must_use]
pub fn is_favicon(url: &str) -> bool {
    url.ends_with("/favicon.ico")
}

/// The budget `_cloudflare_solver` gives its very first idle wait.
///
/// `_wait_for_networkidle(page, timeout=5000)` — the only call site in
/// the reference implementation that overrides the page default.
pub const CLOUDFLARE_IDLE_BUDGET: Duration = Duration::from_millis(5_000);

/// How a [`NetworkTracker::wait_for_idle`] ended.
///
/// Python cannot tell these apart — `wait_for_load_state("networkidle")` either
/// returns or raises a `TimeoutError` that is immediately swallowed. Keeping
/// them distinct costs nothing and makes the `tracing` line honest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdleOutcome {
    /// The quiet period elapsed with nothing in flight.
    Idle,
    /// The budget ran out first. Callers must treat this as a no-op.
    TimedOut,
}

impl IdleOutcome {
    /// Whether the page really went idle.
    #[must_use]
    pub const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}

#[derive(Debug, Default)]
struct TrackerState {
    /// Request ids that have started and not yet finished.
    in_flight: HashSet<String>,
    /// Ids whose `loadingFinished`/`loadingFailed` was observed *before* their
    /// `requestWillBeSent`, thanks to per-event-type streams.
    finished_early: HashSet<String>,
    /// When the in-flight set last became (or has been) empty.
    idle_since: Option<Instant>,
    /// Bumped on every observable change so waiters can detect races.
    generation: u64,
}

/// The in-flight-request bookkeeping behind one tab's `networkidle`.
///
/// Cheap to clone through an [`Arc`]; the event pumps and the waiters share one
/// instance per page.
#[derive(Debug)]
pub struct NetworkTracker {
    state: Mutex<TrackerState>,
    notify: Notify,
}

impl Default for NetworkTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkTracker {
    /// A tracker that starts out idle.
    ///
    /// The clock starts now, so a tab that never issues a request reports
    /// `networkidle` after one quiet period rather than immediately — the same
    /// as Playwright, whose timer is armed when the frame is created.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TrackerState {
                idle_since: Some(Instant::now()),
                ..TrackerState::default()
            }),
            notify: Notify::new(),
        }
    }

    /// An [`Arc`]-wrapped tracker, the shape every caller actually wants.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// `Network.requestWillBeSent`.
    ///
    /// Idempotent: a redirect re-announces the same `requestId` and must not
    /// double-count.
    pub fn request_started(&self, request_id: &str) {
        let mut state = self.state.lock();
        if state.finished_early.remove(request_id) {
            // The completion for this id was already observed; the request is
            // over before we learned it began.
            return;
        }
        if state.in_flight.insert(request_id.to_owned()) {
            state.idle_since = None;
            state.generation += 1;
            drop(state);
            self.notify.notify_waiters();
        }
    }

    /// `Network.loadingFinished` or `Network.loadingFailed`.
    pub fn request_finished(&self, request_id: &str) {
        let mut state = self.state.lock();
        if !state.in_flight.remove(request_id) {
            // Out-of-order delivery: remember it so the matching `started`
            // does not open an entry that will never be closed.
            if state.finished_early.len() >= FINISHED_EARLY_CAPACITY {
                // Every entry here is either about to be drained by its start
                // or belongs to a document that has since been thrown away;
                // dropping the batch cannot strand an in-flight request.
                state.finished_early.clear();
            }
            state.finished_early.insert(request_id.to_owned());
            return;
        }
        if state.in_flight.is_empty() {
            state.idle_since = Some(Instant::now());
        }
        state.generation += 1;
        drop(state);
        self.notify.notify_waiters();
    }

    /// How many requests are in flight right now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.state.lock().in_flight.len()
    }

    /// Whether the quiet period has already elapsed.
    #[must_use]
    pub fn is_idle_now(&self, quiet: Duration) -> bool {
        let state = self.state.lock();
        state.in_flight.is_empty() && state.idle_since.is_some_and(|at| at.elapsed() >= quiet)
    }

    /// Forget every in-flight request and restart the quiet clock.
    ///
    /// Used when a tab is handed to a new request: the previous request's
    /// never-ending downloads (a long-poll, an unfinished video) must not keep
    /// the next one from ever seeing `networkidle`. Playwright gets this for
    /// free because it drops the whole `Frame` on cross-document navigation.
    pub fn reset(&self) {
        {
            let mut state = self.state.lock();
            state.in_flight.clear();
            state.finished_early.clear();
            state.idle_since = Some(Instant::now());
            state.generation += 1;
        }
        self.notify.notify_waiters();
    }

    /// Wait until nothing has been in flight for `quiet`, or `budget` elapses.
    ///
    /// Never returns an error: a timeout is [`IdleOutcome::TimedOut`], which
    /// every caller treats as "carry on", exactly like Python's
    /// `except: pass`.
    pub async fn wait_for_idle(&self, quiet: Duration, budget: Duration) -> IdleOutcome {
        let deadline = Instant::now() + budget;
        loop {
            // Arm the waiter *before* sampling, or a change that lands between
            // the sample and the `select!` would be missed entirely —
            // `notify_waiters` stores no permit.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let wait_for = {
                let state = self.state.lock();
                if state.in_flight.is_empty() {
                    let elapsed = state.idle_since.map_or(Duration::ZERO, |at| at.elapsed());
                    if elapsed >= quiet {
                        return IdleOutcome::Idle;
                    }
                    quiet - elapsed
                } else {
                    // Nothing to time out on but the overall budget.
                    Duration::MAX
                }
            };

            let now = Instant::now();
            if now >= deadline {
                return IdleOutcome::TimedOut;
            }
            let sleep_for = wait_for.min(deadline - now);

            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep(sleep_for) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the 500 ms quiet period, short enough for a real clock.
    ///
    /// `tokio`'s `test-util` feature (`start_paused`, `advance`) is not enabled
    /// anywhere in this workspace, so these tests run against the wall clock
    /// with a scaled-down quiet period. The state machine is identical; only
    /// the constant differs, and
    /// [`the_shipped_quiet_period_is_playwrights`] pins the real one.
    const QUIET: Duration = Duration::from_millis(60);

    /// Comfortably longer than [`QUIET`], to keep a loaded CI box from flaking.
    const GENEROUS: Duration = Duration::from_millis(5_000);

    async fn sleep(millis: u64) {
        tokio::time::sleep(Duration::from_millis(millis)).await;
    }

    #[test]
    fn the_shipped_quiet_period_is_playwrights() {
        assert_eq!(NETWORK_IDLE_QUIET_PERIOD, Duration::from_millis(500));
        assert_eq!(CLOUDFLARE_IDLE_BUDGET, Duration::from_millis(5_000));
    }

    #[tokio::test]
    async fn an_untouched_tracker_goes_idle_after_one_quiet_period() {
        let tracker = NetworkTracker::new();
        assert_eq!(tracker.in_flight(), 0);
        assert!(!tracker.is_idle_now(QUIET));
        assert_eq!(
            tracker.wait_for_idle(QUIET, GENEROUS).await,
            IdleOutcome::Idle
        );
        assert!(tracker.is_idle_now(QUIET));
    }

    #[tokio::test]
    async fn one_in_flight_request_prevents_idle() {
        let tracker = Arc::new(NetworkTracker::new());
        tracker.request_started("1");
        assert_eq!(tracker.in_flight(), 1);

        let waiter = {
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move { tracker.wait_for_idle(QUIET, GENEROUS).await })
        };

        // Several quiet periods with a request in flight: still not idle.
        sleep(QUIET.as_millis() as u64 * 3).await;
        assert!(
            !waiter.is_finished(),
            "an in-flight request blocks networkidle"
        );

        tracker.request_finished("1");
        assert_eq!(waiter.await.expect("waiter"), IdleOutcome::Idle);
    }

    #[tokio::test]
    async fn a_new_request_restarts_the_quiet_period() {
        let tracker = Arc::new(NetworkTracker::new());
        tracker.request_started("1");
        let waiter = {
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move { tracker.wait_for_idle(QUIET, GENEROUS).await })
        };
        tracker.request_finished("1");
        // A late resource lands before the quiet period could elapse.
        sleep(QUIET.as_millis() as u64 / 3).await;
        tracker.request_started("2");
        sleep(QUIET.as_millis() as u64 * 2).await;
        assert!(
            !waiter.is_finished(),
            "the second request must have restarted the timer"
        );
        tracker.request_finished("2");
        assert_eq!(waiter.await.expect("waiter"), IdleOutcome::Idle);
    }

    #[tokio::test]
    async fn a_never_ending_request_times_out_instead_of_hanging() {
        let tracker = NetworkTracker::new();
        tracker.request_started("stream");
        let outcome = tracker
            .wait_for_idle(QUIET, Duration::from_millis(100))
            .await;
        assert_eq!(
            outcome,
            IdleOutcome::TimedOut,
            "the Cloudflare budget must expire, not hang"
        );
        assert!(!outcome.is_idle());
    }

    #[test]
    fn a_redirect_reuses_its_request_id_without_double_counting() {
        let tracker = NetworkTracker::new();
        tracker.request_started("nav");
        // `requestWillBeSent` fires again with `redirectResponse` set.
        tracker.request_started("nav");
        tracker.request_started("nav");
        assert_eq!(tracker.in_flight(), 1);
        tracker.request_finished("nav");
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn a_completion_observed_before_its_start_does_not_leak() {
        let tracker = NetworkTracker::new();
        // Separate `EventStream`s can deliver these two out of order.
        tracker.request_finished("racy");
        assert_eq!(tracker.in_flight(), 0);
        tracker.request_started("racy");
        assert_eq!(
            tracker.in_flight(),
            0,
            "the late start must not open an entry nothing will close"
        );
        // The compensation is consumed, so a genuine second request with a
        // recycled id still counts.
        tracker.request_started("racy");
        assert_eq!(tracker.in_flight(), 1);
    }

    #[test]
    fn favicons_are_not_counted() {
        assert!(is_favicon("http://h/favicon.ico"));
        assert!(!is_favicon("http://h/favicon.icon"));
        assert!(
            !is_favicon("favicon.ico"),
            "Playwright's test is on the path"
        );
    }

    #[test]
    fn an_unknown_completion_is_harmless() {
        let tracker = NetworkTracker::new();
        tracker.request_started("a");
        tracker.request_finished("b");
        assert_eq!(tracker.in_flight(), 1);
        tracker.request_finished("a");
        assert_eq!(tracker.in_flight(), 0);
    }

    #[tokio::test]
    async fn reset_forgets_the_previous_requests_downloads() {
        let tracker = NetworkTracker::new();
        tracker.request_started("long-poll");
        assert_eq!(tracker.in_flight(), 1);
        tracker.reset();
        assert_eq!(tracker.in_flight(), 0);
        assert_eq!(
            tracker.wait_for_idle(QUIET, GENEROUS).await,
            IdleOutcome::Idle
        );
    }

    #[tokio::test]
    async fn a_zero_budget_times_out_without_waiting() {
        let tracker = NetworkTracker::new();
        assert_eq!(
            tracker.wait_for_idle(QUIET, Duration::ZERO).await,
            IdleOutcome::TimedOut
        );
    }
}
