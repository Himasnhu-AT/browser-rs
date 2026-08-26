//! Differential test for [`browser_spider::stats::CrawlStats::to_dict`] against
//! `reference.spiders.result.CrawlStats.to_dict` (the reference implementation, CPython 3.14).
//!
//! Pins three things Python's tests pin: the **exact top-level key order**, the rename of
//! `log_levels_counter` to `log_count`, and `round(x, 2)`'s banker's rounding on the four
//! float-valued entries.
//!
//! Corpus: `tools/corpus/spider/gen_crawl_stats.py`.
//!
//! # The one declared divergence, checked here rather than hidden
//!
//! Python's inner maps are `dict`s in first-seen order; the port's are `DashMap`s rendered
//! key-sorted (see the `stats` module docs). So the inner maps are compared **as maps** —
//! same keys, same values — while the top-level order is compared as a sequence. If the
//! port ever started preserving insertion order, [`INNER_MAPS_ARE_SORTED`] would make this
//! test fail so the divergence note can be deleted.

use std::path::Path;
use std::sync::Arc;

use browser_core::log::LogCounts;
use browser_spider::stats::CrawlStats;
use serde_json::{Map, Value};

/// The keys whose values are maps rendered in a different order than Python's.
const INNER_MAPS_ARE_SORTED: &[&str] = &[
    "autothrottle_delays",
    "sessions_requests_count",
    "response_status_count",
    "domains_response_bytes",
    "custom_stats",
];

fn build(fields: &Map<String, Value>, calls: &[Value]) -> Arc<CrawlStats> {
    let stats = Arc::new(CrawlStats::new());
    for (key, value) in fields {
        match key.as_str() {
            "concurrent_requests" => {
                stats.set_concurrent_requests(value.as_u64().expect("u64"));
            }
            "concurrent_requests_per_domain" => {
                stats.set_concurrent_requests_per_domain(value.as_u64().expect("u64"));
            }
            "download_delay" => stats.set_download_delay(value.as_f64().expect("f64")),
            "start_time" => stats.set_start_time(value.as_f64().expect("f64")),
            "end_time" => stats.set_end_time(value.as_f64().expect("f64")),
            "autothrottle_enabled" => {
                stats.set_autothrottle_enabled(value.as_bool().expect("bool"));
            }
            "items_scraped" => stats.add_items_scraped(value.as_u64().expect("u64")),
            "items_dropped" => stats.add_items_dropped(value.as_u64().expect("u64")),
            "failed_requests_count" => stats.add_failed_requests(value.as_u64().expect("u64")),
            "offsite_requests_count" => stats.add_offsite_requests(value.as_u64().expect("u64")),
            "robots_disallowed_count" => stats.add_robots_disallowed(value.as_u64().expect("u64")),
            "cache_hits" => stats.add_cache_hits(value.as_u64().expect("u64")),
            "cache_misses" => stats.add_cache_misses(value.as_u64().expect("u64")),
            "blocked_requests_count" => stats.add_blocked_requests(value.as_u64().expect("u64")),
            "autothrottle_delays" => stats.set_autothrottle_delays(
                value
                    .as_object()
                    .expect("object")
                    .iter()
                    .map(|(domain, delay)| (domain.clone(), delay.as_f64().expect("f64"))),
            ),
            "custom_stats" => {
                for (name, custom) in value.as_object().expect("object") {
                    stats.set_custom(name.clone(), custom.clone());
                }
            }
            "proxies" => {
                for proxy in value.as_array().expect("array") {
                    stats.push_proxy(proxy.clone());
                }
            }
            "log_levels_counter" => {
                let counts = value.as_object().expect("object");
                let get = |name: &str| counts.get(name).and_then(Value::as_u64).unwrap_or(0);
                stats.set_log_counts(LogCounts {
                    debug: get("debug"),
                    info: get("info"),
                    warning: get("warning"),
                    error: get("error"),
                    critical: get("critical"),
                });
            }
            other => panic!("corpus sets an unhandled field: {other}"),
        }
    }

    for call in calls {
        let call = call.as_array().expect("call");
        match call[0].as_str().expect("method") {
            "increment_requests_count" => {
                stats.increment_requests_count(call[1].as_str().expect("sid"));
            }
            "increment_response_bytes" => stats.increment_response_bytes(
                call[1].as_str().expect("domain"),
                call[2].as_u64().expect("count"),
            ),
            "increment_status" => {
                stats.increment_status(call[1].as_u64().expect("status") as u16);
            }
            other => panic!("corpus calls an unhandled method: {other}"),
        }
    }
    stats
}

#[test]
fn crawl_stats_to_dict_matches_the_reference() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/crawl_stats.jsonl");
    let corpus = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));

    let mut rows = 0usize;
    for (lineno, line) in corpus.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {}: {e}", lineno + 1));
        let name = row["name"].as_str().expect("name");
        let expected: Value = serde_json::from_str(row["expected"].as_str().expect("expected"))
            .expect("expected is JSON");
        let expected = expected.as_object().expect("object");

        let stats = build(
            row["fields"].as_object().expect("fields"),
            row["calls"].as_array().expect("calls"),
        );
        let got = stats.to_dict();

        rows += 1;

        let expected_keys: Vec<&str> = expected.keys().map(String::as_str).collect();
        let got_keys: Vec<&str> = got.keys().map(String::as_str).collect();
        assert_eq!(got_keys, expected_keys, "[{name}] key order differs");

        for (key, want) in expected {
            let have = &got[key];
            if INNER_MAPS_ARE_SORTED.contains(&key.as_str()) {
                let want_map = want.as_object().expect("map");
                let have_map = have.as_object().expect("map");
                assert_eq!(
                    want_map.len(),
                    have_map.len(),
                    "[{name}] {key}: different number of entries"
                );
                for (inner, want_value) in want_map {
                    assert_eq!(
                        have_map.get(inner),
                        Some(want_value),
                        "[{name}] {key}[{inner}] differs"
                    );
                }
                // The divergence is *only* about order. If the port ever starts matching
                // Python's insertion order for a multi-entry map, this is the reminder to
                // delete the note in the `stats` module docs.
                if want_map.len() > 1 {
                    let want_order: Vec<&String> = want_map.keys().collect();
                    let have_order: Vec<&String> = have_map.keys().collect();
                    if want_order == have_order {
                        let mut sorted = want_order.clone();
                        sorted.sort();
                        assert_eq!(
                            want_order, sorted,
                            "[{name}] {key}: Python's insertion order now matches the \
                             port's sorted order for a case where they used to differ. If \
                             this is no longer a divergence anywhere, remove the note from \
                             the `stats` module docs."
                        );
                    }
                }
            } else {
                assert_eq!(have, want, "[{name}] {key} differs");
            }
        }
    }
    assert!(rows >= 4, "corpus shrank to {rows} rows");
}
