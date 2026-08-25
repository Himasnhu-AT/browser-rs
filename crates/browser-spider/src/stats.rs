//! [`CrawlStats`]: `AtomicU64` scalars plus `DashMap` maps so stats can be read live, and
//! [`CrawlStats::to_dict`] in the exact key order.
//!
//! Python's `CrawlStats` is a plain dataclass mutated from a single-threaded event loop.
//! The port has to serve two masters: worker tasks incrementing counters from several
//! threads, and `Spider::stats` reading them *while the crawl runs* (the property is
//! documented as usable from inside `async for item in spider.stream()`). A single
//! `Mutex<CrawlStats>` would serialise every counter bump behind the reader, so scalars are
//! atomics and maps are `DashMap`s, and the whole thing is shared as `Arc<CrawlStats>` with
//! `&self` methods.
//!
//! # Key order
//!
//! [`CrawlStats::to_dict`] reproduces Python's key order exactly — including the rename of
//! `log_levels_counter` to `log_count` — because that dict is dumped to the log at the end
//! of every crawl and users grep it.
//!
//! # One deliberate divergence
//!
//! Python's inner maps (`response_status_count`, `domains_response_bytes`, …) are `dict`s
//! and therefore appear in *first-seen* order. A `DashMap` has no order at all, so
//! [`CrawlStats::to_dict`] emits each inner map **sorted by key**. That is deterministic —
//! two identical crawls produce identical output, which Python cannot promise once tasks
//! interleave — and no test in the reference implementation depends on the insertion order of these maps.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use browser_core::log::LogCounts;
use browser_core::numeric::round_half_even;
use browser_core::types::Proxy;

use crate::items::ItemList;

/// An `f64` stored in an `AtomicU64` as its bit pattern.
///
/// Used for the three float fields: they are written rarely (once per crawl, or once per
/// domain) and read from `to_dict`, so a lock would be pure overhead.
#[derive(Debug, Default)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// Statistics for one crawl run.
///
/// Every method takes `&self`; share it as an `Arc<CrawlStats>`. Python replaces the whole
/// object at the start of each `crawl()`, so a caller holding the previous one sees a
/// frozen snapshot — build a fresh [`CrawlStats`] per run to reproduce that.
#[derive(Debug, Default)]
pub struct CrawlStats {
    requests_count: AtomicU64,
    concurrent_requests: AtomicU64,
    concurrent_requests_per_domain: AtomicU64,
    failed_requests_count: AtomicU64,
    offsite_requests_count: AtomicU64,
    robots_disallowed_count: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    response_bytes: AtomicU64,
    items_scraped: AtomicU64,
    items_dropped: AtomicU64,
    blocked_requests_count: AtomicU64,

    start_time: AtomicF64,
    end_time: AtomicF64,
    download_delay: AtomicF64,
    autothrottle_enabled: AtomicBool,

    autothrottle_delays: DashMap<String, f64>,
    custom_stats: DashMap<String, Value>,
    response_status_count: DashMap<String, u64>,
    domains_response_bytes: DashMap<String, u64>,
    sessions_requests_count: DashMap<String, u64>,

    proxies: Mutex<Vec<Value>>,
    /// `None` until the engine copies the level counter in at the end of the crawl, which
    /// is Python's empty `log_levels_counter` dict.
    log_counts: Mutex<Option<LogCounts>>,
}

macro_rules! counter {
    ($field:ident, $get:ident, $incr:ident, $doc:literal) => {
        #[doc = $doc]
        pub fn $get(&self) -> u64 {
            self.$field.load(Ordering::Relaxed)
        }
        #[doc = concat!("Add `n` to `", stringify!($field), "`.")]
        pub fn $incr(&self, n: u64) {
            self.$field.fetch_add(n, Ordering::Relaxed);
        }
    };
}

impl CrawlStats {
    /// A zeroed stats object.
    pub fn new() -> Self {
        Self::default()
    }

    counter!(
        failed_requests_count,
        failed_requests_count,
        add_failed_requests,
        "Requests whose fetch raised. These do **not** count towards `requests_count`."
    );
    counter!(
        offsite_requests_count,
        offsite_requests_count,
        add_offsite_requests,
        "Requests a callback yielded that fell outside `allowed_domains`."
    );
    counter!(
        robots_disallowed_count,
        robots_disallowed_count,
        add_robots_disallowed,
        "Requests dropped by `robots.txt`."
    );
    counter!(
        cache_hits,
        cache_hits,
        add_cache_hits,
        "Dev-mode cache hits."
    );
    counter!(
        cache_misses,
        cache_misses,
        add_cache_misses,
        "Dev-mode cache misses (counted when a response is written back)."
    );
    counter!(
        items_scraped,
        items_scraped,
        add_items_scraped,
        "Items kept by `on_scraped_item`."
    );
    counter!(
        items_dropped,
        items_dropped,
        add_items_dropped,
        "Items `on_scraped_item` returned falsy for — an empty item counts as dropped."
    );
    counter!(
        blocked_requests_count,
        blocked_requests_count,
        add_blocked_requests,
        "Responses `is_blocked` returned true for, retried or not."
    );

    /// Total requests that produced a response (cache replays included).
    pub fn requests_count(&self) -> u64 {
        self.requests_count.load(Ordering::Relaxed)
    }

    /// Total response bytes across every domain.
    pub fn response_bytes(&self) -> u64 {
        self.response_bytes.load(Ordering::Relaxed)
    }

    /// The configured global concurrency.
    pub fn concurrent_requests(&self) -> u64 {
        self.concurrent_requests.load(Ordering::Relaxed)
    }

    /// Set the configured global concurrency (the engine does this once per run).
    pub fn set_concurrent_requests(&self, value: u64) {
        self.concurrent_requests.store(value, Ordering::Relaxed);
    }

    /// The configured per-domain concurrency; `0` means "no per-domain limiter".
    pub fn concurrent_requests_per_domain(&self) -> u64 {
        self.concurrent_requests_per_domain.load(Ordering::Relaxed)
    }

    /// Set the configured per-domain concurrency.
    pub fn set_concurrent_requests_per_domain(&self, value: u64) {
        self.concurrent_requests_per_domain
            .store(value, Ordering::Relaxed);
    }

    /// When the crawl started, as seconds since an arbitrary epoch.
    pub fn start_time(&self) -> f64 {
        self.start_time.get()
    }

    /// Record the crawl's start time.
    pub fn set_start_time(&self, value: f64) {
        self.start_time.set(value);
    }

    /// When the crawl ended.
    pub fn end_time(&self) -> f64 {
        self.end_time.get()
    }

    /// Record the crawl's end time.
    pub fn set_end_time(&self, value: f64) {
        self.end_time.set(value);
    }

    /// The configured fixed delay before each fetch.
    pub fn download_delay(&self) -> f64 {
        self.download_delay.get()
    }

    /// Record the configured fixed delay.
    pub fn set_download_delay(&self, value: f64) {
        self.download_delay.set(value);
    }

    /// Whether AutoThrottle was enabled for this run.
    pub fn autothrottle_enabled(&self) -> bool {
        self.autothrottle_enabled.load(Ordering::Relaxed)
    }

    /// Record whether AutoThrottle was enabled.
    pub fn set_autothrottle_enabled(&self, value: bool) {
        self.autothrottle_enabled.store(value, Ordering::Relaxed);
    }

    /// `end_time - start_time`.
    pub fn elapsed_seconds(&self) -> f64 {
        self.end_time() - self.start_time()
    }

    /// `requests_count / elapsed_seconds`, or `0.0` when no time has elapsed.
    ///
    /// Python guards on `elapsed == 0` exactly, so a crawl whose stats were never stamped
    /// reports `0.0` rather than an infinity.
    pub fn requests_per_second(&self) -> f64 {
        let elapsed = self.elapsed_seconds();
        if elapsed == 0.0 {
            return 0.0;
        }
        self.requests_count() as f64 / elapsed
    }

    /// `response_status_count["status_{status}"] += 1`.
    pub fn increment_status(&self, status: u16) {
        *self
            .response_status_count
            .entry(format!("status_{status}"))
            .or_insert(0) += 1;
    }

    /// Add `count` to both the global and the per-domain byte totals.
    pub fn increment_response_bytes(&self, domain: &str, count: u64) {
        self.response_bytes.fetch_add(count, Ordering::Relaxed);
        *self
            .domains_response_bytes
            .entry(domain.to_owned())
            .or_insert(0) += count;
    }

    /// Bump the request total and the per-session total together.
    pub fn increment_requests_count(&self, sid: &str) {
        self.requests_count.fetch_add(1, Ordering::Relaxed);
        *self
            .sessions_requests_count
            .entry(sid.to_owned())
            .or_insert(0) += 1;
    }

    /// Record a proxy that was used, as Python appends it: a bare string for `proxy=`, an
    /// object for `proxies=`.
    pub fn push_proxy(&self, proxy: Value) {
        self.proxies.lock().push(proxy);
    }

    /// Record a [`Proxy`] from a request's session kwargs.
    pub fn push_proxy_config(&self, proxy: &Proxy) {
        self.push_proxy(serde_json::to_value(proxy).unwrap_or(Value::Null));
    }

    /// The proxies used so far, in the order they were recorded.
    pub fn proxies(&self) -> Vec<Value> {
        self.proxies.lock().clone()
    }

    /// Replace the AutoThrottle per-domain delays (the engine copies them in at the end).
    pub fn set_autothrottle_delays<I>(&self, delays: I)
    where
        I: IntoIterator<Item = (String, f64)>,
    {
        self.autothrottle_delays.clear();
        for (domain, delay) in delays {
            self.autothrottle_delays.insert(domain, delay);
        }
    }

    /// The recorded AutoThrottle delay for one domain.
    pub fn autothrottle_delay(&self, domain: &str) -> Option<f64> {
        self.autothrottle_delays.get(domain).map(|d| *d)
    }

    /// Write a free-form statistic. Surfaces under `custom_stats` in
    /// [`CrawlStats::to_dict`]; spiders use it for their own counters.
    pub fn set_custom(&self, key: impl Into<String>, value: Value) {
        self.custom_stats.insert(key.into(), value);
    }

    /// Read back a free-form statistic.
    pub fn custom(&self, key: &str) -> Option<Value> {
        self.custom_stats.get(key).map(|v| v.clone())
    }

    /// Add `by` to a numeric free-form statistic, creating it at zero first.
    pub fn increment_custom(&self, key: impl Into<String>, by: i64) {
        let mut entry = self
            .custom_stats
            .entry(key.into())
            .or_insert(Value::from(0));
        let current = entry.as_i64().unwrap_or(0);
        *entry = Value::from(current + by);
    }

    /// How many requests each session handled.
    pub fn session_requests(&self, sid: &str) -> u64 {
        self.sessions_requests_count.get(sid).map_or(0, |n| *n)
    }

    /// How many responses carried a given status.
    pub fn status_count(&self, status: u16) -> u64 {
        self.response_status_count
            .get(&format!("status_{status}"))
            .map_or(0, |n| *n)
    }

    /// How many bytes came from a given domain.
    pub fn domain_bytes(&self, domain: &str) -> u64 {
        self.domains_response_bytes.get(domain).map_or(0, |n| *n)
    }

    /// Record the per-level log tallies (Python's `LogCounterHandler.get_counts()`).
    pub fn set_log_counts(&self, counts: LogCounts) {
        *self.log_counts.lock() = Some(counts);
    }

    /// The recorded per-level log tallies, if the engine has copied them in yet.
    pub fn log_counts(&self) -> Option<LogCounts> {
        *self.log_counts.lock()
    }

    /// The stats as an ordered JSON object, in Python's exact key order.
    ///
    /// Note the last key: Python renames `log_levels_counter` to **`log_count`** here.
    /// Floats are rounded to two decimals with `round()`'s banker's rounding.
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("items_scraped".into(), self.items_scraped().into());
        out.insert("items_dropped".into(), self.items_dropped().into());
        out.insert(
            "elapsed_seconds".into(),
            round2(self.elapsed_seconds()).into(),
        );
        out.insert(
            "download_delay".into(),
            round2(self.download_delay()).into(),
        );
        out.insert(
            "autothrottle_enabled".into(),
            self.autothrottle_enabled().into(),
        );
        out.insert(
            "autothrottle_delays".into(),
            Value::Object(sorted_map(&self.autothrottle_delays, |delay| {
                round2(*delay).into()
            })),
        );
        out.insert(
            "concurrent_requests".into(),
            self.concurrent_requests().into(),
        );
        out.insert(
            "concurrent_requests_per_domain".into(),
            self.concurrent_requests_per_domain().into(),
        );
        out.insert("requests_count".into(), self.requests_count().into());
        out.insert(
            "requests_per_second".into(),
            round2(self.requests_per_second()).into(),
        );
        out.insert(
            "sessions_requests_count".into(),
            Value::Object(sorted_map(&self.sessions_requests_count, |n| (*n).into())),
        );
        out.insert(
            "failed_requests_count".into(),
            self.failed_requests_count().into(),
        );
        out.insert(
            "offsite_requests_count".into(),
            self.offsite_requests_count().into(),
        );
        out.insert(
            "robots_disallowed_count".into(),
            self.robots_disallowed_count().into(),
        );
        out.insert("cache_hits".into(), self.cache_hits().into());
        out.insert("cache_misses".into(), self.cache_misses().into());
        out.insert(
            "blocked_requests_count".into(),
            self.blocked_requests_count().into(),
        );
        out.insert(
            "response_status_count".into(),
            Value::Object(sorted_map(&self.response_status_count, |n| (*n).into())),
        );
        out.insert("response_bytes".into(), self.response_bytes().into());
        out.insert(
            "domains_response_bytes".into(),
            Value::Object(sorted_map(&self.domains_response_bytes, |n| (*n).into())),
        );
        out.insert("proxies".into(), Value::Array(self.proxies()));
        out.insert(
            "custom_stats".into(),
            Value::Object(sorted_map(&self.custom_stats, Clone::clone)),
        );
        out.insert(
            "log_count".into(),
            match self.log_counts() {
                Some(counts) => Value::Object(
                    counts
                        .to_map()
                        .into_iter()
                        .map(|(k, v)| (k, Value::from(v)))
                        .collect(),
                ),
                None => Value::Object(Map::new()),
            },
        );
        out
    }

    /// The stats as the reference implementation logs them at the end of a crawl: `json.dumps(..., indent=4)`.
    pub fn to_pretty_json(&self) -> String {
        let mut buffer = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
        // Serialising a `Map<String, Value>` cannot fail for any value we put in it.
        if serde::Serialize::serialize(&Value::Object(self.to_dict()), &mut serializer).is_err() {
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }
}

/// `round(x, 2)` with CPython's banker's rounding.
fn round2(value: f64) -> f64 {
    round_half_even(value, 2)
}

/// A `DashMap` rendered as a key-sorted JSON object — see the divergence note in the
/// module docs.
fn sorted_map<V, F>(map: &DashMap<String, V>, mut render: F) -> Map<String, Value>
where
    V: Send + Sync,
    F: FnMut(&V) -> Value,
{
    let mut entries: Vec<(String, Value)> = map
        .iter()
        .map(|entry| (entry.key().clone(), render(entry.value())))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().collect()
}

/// The complete result of a spider run.
///
/// Mirrors Python's `CrawlResult` dataclass. `paused` is `true` when the crawl stopped at
/// the user's request with a checkpoint written rather than running to completion.
#[derive(Debug)]
pub struct CrawlResult {
    /// The run's statistics.
    pub stats: std::sync::Arc<CrawlStats>,
    /// Everything the spider scraped (empty in streaming mode — items go to the channel).
    pub items: ItemList,
    /// Whether the crawl was paused instead of finishing.
    pub paused: bool,
}

impl CrawlResult {
    /// `true` when the crawl finished normally.
    pub fn completed(&self) -> bool {
        !self.paused
    }

    /// How many items were collected.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no items were collected.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<'a> IntoIterator for &'a CrawlResult {
    type Item = &'a crate::request::Item;
    type IntoIter = std::slice::Iter<'a, crate::request::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_maps_move_together() {
        let stats = CrawlStats::new();
        stats.increment_requests_count("default");
        stats.increment_requests_count("default");
        stats.increment_requests_count("browser");
        assert_eq!(stats.requests_count(), 3);
        assert_eq!(stats.session_requests("default"), 2);
        assert_eq!(stats.session_requests("browser"), 1);
        assert_eq!(stats.session_requests("nope"), 0);

        stats.increment_response_bytes("example.com", 100);
        stats.increment_response_bytes("example.com", 50);
        stats.increment_response_bytes("other.com", 7);
        assert_eq!(stats.response_bytes(), 157);
        assert_eq!(stats.domain_bytes("example.com"), 150);

        stats.increment_status(200);
        stats.increment_status(200);
        stats.increment_status(404);
        assert_eq!(stats.status_count(200), 2);
        assert_eq!(stats.status_count(500), 0);
    }

    #[test]
    fn requests_per_second_is_zero_when_no_time_elapsed() {
        let stats = CrawlStats::new();
        stats.increment_requests_count("s");
        assert_eq!(stats.elapsed_seconds(), 0.0);
        assert_eq!(stats.requests_per_second(), 0.0);

        stats.set_start_time(10.0);
        stats.set_end_time(14.0);
        assert_eq!(stats.elapsed_seconds(), 4.0);
        assert_eq!(stats.requests_per_second(), 0.25);
    }

    #[test]
    fn to_dict_has_pythons_exact_key_order() {
        let stats = CrawlStats::new();
        let dict = stats.to_dict();
        let keys: Vec<&str> = dict.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "items_scraped",
                "items_dropped",
                "elapsed_seconds",
                "download_delay",
                "autothrottle_enabled",
                "autothrottle_delays",
                "concurrent_requests",
                "concurrent_requests_per_domain",
                "requests_count",
                "requests_per_second",
                "sessions_requests_count",
                "failed_requests_count",
                "offsite_requests_count",
                "robots_disallowed_count",
                "cache_hits",
                "cache_misses",
                "blocked_requests_count",
                "response_status_count",
                "response_bytes",
                "domains_response_bytes",
                "proxies",
                "custom_stats",
                "log_count",
            ]
        );
    }

    #[test]
    fn to_dict_rounds_floats_to_two_places() {
        let stats = CrawlStats::new();
        stats.set_start_time(0.0);
        stats.set_end_time(1.23456);
        stats.set_download_delay(0.555);
        stats.increment_requests_count("s");
        stats.set_autothrottle_delays([("example.com".to_owned(), 1.239)]);

        let dict = stats.to_dict();
        assert_eq!(dict["elapsed_seconds"], Value::from(1.23));
        assert_eq!(dict["download_delay"], Value::from(0.56));
        assert_eq!(dict["requests_per_second"], Value::from(0.81));
        assert_eq!(
            dict["autothrottle_delays"]["example.com"],
            Value::from(1.24)
        );
    }

    #[test]
    fn log_count_is_empty_until_the_engine_fills_it() {
        let stats = CrawlStats::new();
        assert_eq!(stats.to_dict()["log_count"], Value::Object(Map::new()));

        stats.set_log_counts(LogCounts {
            debug: 3,
            info: 2,
            warning: 1,
            error: 0,
            critical: 0,
        });
        let dict = stats.to_dict();
        let keys: Vec<&str> = dict["log_count"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["debug", "info", "warning", "error", "critical"]);
        assert_eq!(dict["log_count"]["debug"], Value::from(3));
    }

    #[test]
    fn proxies_keep_their_recorded_shape_and_order() {
        let stats = CrawlStats::new();
        stats.push_proxy_config(&Proxy::Url("http://one".into()));
        stats.push_proxy(serde_json::json!({"http": "http://two"}));
        let dict = stats.to_dict();
        assert_eq!(
            dict["proxies"],
            serde_json::json!(["http://one", {"http": "http://two"}])
        );
    }

    #[test]
    fn custom_stats_are_free_form() {
        let stats = CrawlStats::new();
        stats.set_custom("pages", Value::from(3));
        stats.increment_custom("pages", 2);
        stats.increment_custom("fresh", 1);
        assert_eq!(stats.custom("pages"), Some(Value::from(5)));
        assert_eq!(stats.to_dict()["custom_stats"]["fresh"], Value::from(1));
    }

    #[test]
    fn pretty_json_uses_four_space_indentation() {
        let stats = CrawlStats::new();
        let text = stats.to_pretty_json();
        assert!(text.starts_with("{\n    \"items_scraped\": 0,\n"), "{text}");
        assert!(text.contains("\"autothrottle_delays\": {}"));
    }

    #[test]
    fn stats_are_shared_across_threads() {
        // The whole point of the atomics-and-DashMap shape: eight worker threads bump the
        // counters while a reader could be calling `to_dict()`, with no global lock.
        let stats = std::sync::Arc::new(CrawlStats::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let stats = std::sync::Arc::clone(&stats);
                scope.spawn(move || {
                    for _ in 0..100 {
                        stats.increment_requests_count("s");
                        stats.increment_response_bytes("example.com", 1);
                        stats.increment_status(200);
                        stats.add_items_scraped(1);
                    }
                });
            }
        });
        assert_eq!(stats.requests_count(), 800);
        assert_eq!(stats.session_requests("s"), 800);
        assert_eq!(stats.response_bytes(), 800);
        assert_eq!(stats.domain_bytes("example.com"), 800);
        assert_eq!(stats.status_count(200), 800);
        assert_eq!(stats.items_scraped(), 800);
    }
}
