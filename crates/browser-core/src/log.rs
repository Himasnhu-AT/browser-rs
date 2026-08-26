//! `tracing` initialisation plus the [`LevelCounter`] layer that backs
//! `CrawlStats.log_levels_counter`.
//!
//! Python keeps one process-wide `logging.Logger` named `"brs"` behind a
//! `ContextVar` so a spider run can swap in its own logger and restore it afterwards.
//! The Rust equivalent is `tracing`: [`init`] installs a global subscriber, and a
//! scoped override is `tracing::subscriber::set_default(...)`, whose returned
//! `DefaultGuard` is the analogue of `reset_logger(token)` (`ARCHITECTURE.md` §7.7).
//!
//! `LogCounterHandler` from `reference/spiders/spider.py` becomes [`LevelCounter`], a
//! `tracing` [`Layer`] that tallies events per level. It buckets exactly the way the
//! Python handler does: *everything below `INFO` counts as `debug`*, so `TRACE` events
//! land in the `debug` bucket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::util::TryInitError;

/// A snapshot of [`LevelCounter`]'s tallies.
///
/// The field names — and the key order of [`LogCounts::to_map`] — are Python's
/// `get_counts()` dictionary: `debug`, `info`, `warning`, `error`, `critical`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogCounts {
    /// Events at `DEBUG` **or `TRACE`** — Python's handler folds everything below
    /// `INFO` into this bucket.
    pub debug: u64,
    /// Events at `INFO`.
    pub info: u64,
    /// Events at `WARN`.
    pub warning: u64,
    /// Events at `ERROR`.
    pub error: u64,
    /// Always `0`: `tracing` has no level above `ERROR`, so nothing can land here.
    /// The key is kept so `CrawlStats.log_count` has the same shape as Python's.
    pub critical: u64,
}

impl LogCounts {
    /// The counts as an ordered map, in Python's key order.
    pub fn to_map(self) -> IndexMap<String, u64> {
        let mut map = IndexMap::with_capacity(5);
        map.insert("debug".to_owned(), self.debug);
        map.insert("info".to_owned(), self.info);
        map.insert("warning".to_owned(), self.warning);
        map.insert("error".to_owned(), self.error);
        map.insert("critical".to_owned(), self.critical);
        map
    }

    /// Total number of events counted.
    pub fn total(self) -> u64 {
        self.debug + self.info + self.warning + self.error + self.critical
    }
}

#[derive(Debug, Default)]
struct Counters {
    debug: AtomicU64,
    info: AtomicU64,
    warning: AtomicU64,
    error: AtomicU64,
}

/// A `tracing` [`Layer`] that counts events per level.
///
/// Cheap to clone — clones share one set of counters — so the crawl engine can hand a
/// clone to the subscriber and keep one to read at the end of the run.
///
/// ```
/// use browser_core::log::LevelCounter;
/// use tracing_subscriber::layer::SubscriberExt;
///
/// let counter = LevelCounter::new();
/// let subscriber = tracing_subscriber::registry().with(counter.clone());
///
/// tracing::subscriber::with_default(subscriber, || {
///     tracing::info!("fetched");
///     tracing::warn!("retrying");
///     tracing::trace!("noise");
/// });
///
/// let counts = counter.counts();
/// assert_eq!(counts.info, 1);
/// assert_eq!(counts.warning, 1);
/// assert_eq!(counts.debug, 1); // TRACE buckets into `debug`, as in Python
/// assert_eq!(counts.critical, 0);
/// ```
#[derive(Clone, Debug, Default)]
pub struct LevelCounter {
    inner: Arc<Counters>,
}

impl LevelCounter {
    /// A counter with every tally at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one event at `level`, using Python's bucketing.
    pub fn record(&self, level: &Level) {
        let cell = match *level {
            Level::ERROR => &self.inner.error,
            Level::WARN => &self.inner.warning,
            Level::INFO => &self.inner.info,
            // DEBUG and TRACE both count as `debug`.
            _ => &self.inner.debug,
        };
        cell.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the current tallies. Equivalent to Python's `get_counts()`.
    pub fn counts(&self) -> LogCounts {
        LogCounts {
            debug: self.inner.debug.load(Ordering::Relaxed),
            info: self.inner.info.load(Ordering::Relaxed),
            warning: self.inner.warning.load(Ordering::Relaxed),
            error: self.inner.error.load(Ordering::Relaxed),
            critical: 0,
        }
    }

    /// Reset every tally to zero.
    pub fn reset(&self) {
        self.inner.debug.store(0, Ordering::Relaxed);
        self.inner.info.store(0, Ordering::Relaxed);
        self.inner.warning.store(0, Ordering::Relaxed);
        self.inner.error.store(0, Ordering::Relaxed);
    }
}

impl<S: Subscriber> Layer<S> for LevelCounter {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.record(event.metadata().level());
    }
}

/// Install a global `tracing` subscriber writing to **stderr**, the way
/// `setup_logger()` does.
///
/// `default_directive` is used when `RUST_LOG` is unset; pass `"info"` to match
/// Python's default level. Calling this more than once in a process fails, exactly
/// like `tracing_subscriber`'s `try_init`.
///
/// **Divergence:** the line format is `tracing-subscriber`'s, not Python's
/// `"[%(asctime)s] %(levelname)s: %(message)s"`. Message *text* is reproduced verbatim
/// where the specs pin it; the surrounding decoration is not.
pub fn init(default_directive: &str) -> Result<(), TryInitError> {
    init_with(default_directive, None)
}

/// [`init`], additionally installing a [`LevelCounter`].
pub fn init_with(
    default_directive: &str,
    counter: Option<LevelCounter>,
) -> Result<(), TryInitError> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(counter)
        .try_init()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn counts_events_per_level_with_python_bucketing() {
        let counter = LevelCounter::new();
        let subscriber = tracing_subscriber::registry().with(counter.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!("t");
            tracing::debug!("d");
            tracing::debug!("d2");
            tracing::info!("i");
            tracing::warn!("w");
            tracing::error!("e");
        });
        let counts = counter.counts();
        assert_eq!(counts.debug, 3); // TRACE + 2 × DEBUG
        assert_eq!(counts.info, 1);
        assert_eq!(counts.warning, 1);
        assert_eq!(counts.error, 1);
        assert_eq!(counts.critical, 0);
        assert_eq!(counts.total(), 6);
    }

    #[test]
    fn to_map_uses_pythons_key_order() {
        let counts = LogCounts {
            debug: 1,
            info: 2,
            warning: 3,
            error: 4,
            critical: 0,
        };
        let map = counts.to_map();
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            vec!["debug", "info", "warning", "error", "critical"]
        );
        assert_eq!(map["warning"], 3);
    }

    #[test]
    fn clones_share_counters_and_reset_clears_them() {
        let a = LevelCounter::new();
        let b = a.clone();
        a.record(&Level::INFO);
        b.record(&Level::ERROR);
        assert_eq!(a.counts(), b.counts());
        assert_eq!(a.counts().total(), 2);
        b.reset();
        assert_eq!(a.counts(), LogCounts::default());
    }

    #[test]
    fn a_fresh_counter_is_all_zeroes() {
        assert_eq!(LevelCounter::new().counts(), LogCounts::default());
        assert_eq!(LogCounts::default().total(), 0);
    }
}
