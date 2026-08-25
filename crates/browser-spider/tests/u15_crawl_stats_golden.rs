//! Differential test for `CrawlStats::to_dict` against `reference.spiders.result.CrawlStats`.
//!
//! `testdata/u15_crawl_stats.jsonl` (from `tools/corpus/u15/gen_stats.py`) drives a fresh
//! `CrawlStats` through a scripted call list and records Python's answer three ways: the
//! exact top-level key **order** (which pins the `log_levels_counter` -> `log_count`
//! rename), the dict itself (which pins every `round(x, 2)`), and the first-seen key order
//! of the five nested maps.
//!
//! That last one is the port's one declared divergence here — it renders the nested maps
//! sorted rather than in insertion order — and [`INNER_ORDER_DIVERGENCE`] asserts it is
//! *still* a divergence, so the note can never quietly become false.

mod u15_support;

use browser_core::log::LogCounts;
use browser_spider::stats::CrawlStats;
use serde_json::{Map, Value};

use u15_support::read_corpus;

/// Why the nested maps are sorted here and insertion-ordered in Python.
const INNER_ORDER_DIVERGENCE: &str = "\
`CrawlStats` is `Arc`-shared and every mutator takes `&self`, so its inner maps are \
`DashMap`s, which have no insertion order to preserve. Rendering them sorted is \
deterministic; rendering them in Python's first-seen order would depend on how the crawl's \
tasks happened to interleave. The top-level key order — the part a consumer parses — is \
exact. Recorded in docs/DIVERGENCES.md.";

fn apply(stats: &CrawlStats, call: &Value) {
    let object = call.as_object().expect("call is an object");
    let (key, value) = object.iter().next().expect("one key per call");
    match key.as_str() {
        "add" => {
            let pair = value.as_array().expect("add pair");
            let field = pair[0].as_str().expect("field");
            let amount = pair[1].as_u64().expect("amount");
            match field {
                "items_scraped" => stats.add_items_scraped(amount),
                "items_dropped" => stats.add_items_dropped(amount),
                "failed_requests_count" => stats.add_failed_requests(amount),
                "offsite_requests_count" => stats.add_offsite_requests(amount),
                "robots_disallowed_count" => stats.add_robots_disallowed(amount),
                "cache_hits" => stats.add_cache_hits(amount),
                "cache_misses" => stats.add_cache_misses(amount),
                "blocked_requests_count" => stats.add_blocked_requests(amount),
                other => panic!("unknown counter {other}"),
            }
        }
        "set" => {
            let pair = value.as_array().expect("set pair");
            let field = pair[0].as_str().expect("field");
            match field {
                "concurrent_requests" => {
                    stats.set_concurrent_requests(pair[1].as_u64().expect("u64"))
                }
                "concurrent_requests_per_domain" => {
                    stats.set_concurrent_requests_per_domain(pair[1].as_u64().expect("u64"))
                }
                "start_time" => stats.set_start_time(pair[1].as_f64().expect("f64")),
                "end_time" => stats.set_end_time(pair[1].as_f64().expect("f64")),
                "download_delay" => stats.set_download_delay(pair[1].as_f64().expect("f64")),
                "autothrottle_enabled" => {
                    stats.set_autothrottle_enabled(pair[1].as_bool().expect("bool"))
                }
                other => panic!("unknown field {other}"),
            }
        }
        "increment_status" => {
            stats.increment_status(value.as_u64().expect("status") as u16);
        }
        "increment_response_bytes" => {
            let pair = value.as_array().expect("domain/count pair");
            stats.increment_response_bytes(
                pair[0].as_str().expect("domain"),
                pair[1].as_u64().expect("count"),
            );
        }
        "increment_requests_count" => {
            stats.increment_requests_count(value.as_str().expect("sid"));
        }
        "set_delays" => {
            // Python's engine assigns the whole table at once
            // (`stats.autothrottle_delays = dict(self._autothrottle.delays)`), which is
            // exactly what `set_autothrottle_delays` does — it replaces, not merges.
            stats.set_autothrottle_delays(
                value
                    .as_object()
                    .expect("delay map")
                    .iter()
                    .map(|(domain, delay)| (domain.clone(), delay.as_f64().expect("delay")))
                    .collect::<Vec<_>>(),
            );
        }
        "set_custom" => {
            let pair = value.as_array().expect("key/value pair");
            stats.set_custom(pair[0].as_str().expect("key"), pair[1].clone());
        }
        "push_proxy" => stats.push_proxy(value.clone()),
        "log_counts" => {
            let counts = value.as_object().expect("log counts");
            stats.set_log_counts(LogCounts {
                debug: counts["debug"].as_u64().expect("debug"),
                info: counts["info"].as_u64().expect("info"),
                warning: counts["warning"].as_u64().expect("warning"),
                error: counts["error"].as_u64().expect("error"),
                critical: counts["critical"].as_u64().expect("critical"),
            });
        }
        other => panic!("unknown call {other}"),
    }
}

/// Compares two JSON values, treating objects as unordered maps.
fn same_ignoring_object_order(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter().all(|(key, value)| {
                    y.get(key)
                        .is_some_and(|other| same_ignoring_object_order(value, other))
                })
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(value, other)| same_ignoring_object_order(value, other))
        }
        (Value::Number(x), Value::Number(y)) => {
            x == y || x.as_f64().zip(y.as_f64()).is_some_and(|(x, y)| x == y)
        }
        _ => a == b,
    }
}

#[test]
fn crawl_stats_to_dict_matches_browser-rs() {
    let corpus = read_corpus("u15_crawl_stats.jsonl");
    let mut failures: Vec<String> = Vec::new();
    let mut matched = 0usize;
    let mut inner_order_divergences = 0usize;

    for row in &corpus {
        let name = row["name"].as_str().expect("name");
        let stats = CrawlStats::new();
        for call in row["calls"].as_array().expect("calls") {
            apply(&stats, call);
        }
        let rendered = stats.to_dict();

        // 1. The top-level key order must be byte-for-byte Python's, rename included.
        let expected_order: Vec<&str> = row["key_order"]
            .as_array()
            .expect("key_order")
            .iter()
            .map(|k| k.as_str().expect("key"))
            .collect();
        let got_order: Vec<&str> = rendered.keys().map(String::as_str).collect();
        if got_order != expected_order {
            failures.push(format!(
                "  {name}: key order\n    python: {expected_order:?}\n    rust:   {got_order:?}"
            ));
            continue;
        }
        assert!(
            rendered.contains_key("log_count") && !rendered.contains_key("log_levels_counter"),
            "{name}: to_dict must rename log_levels_counter to log_count"
        );

        // 2. Every value must match, comparing nested objects as unordered maps.
        let expected: &Map<String, Value> = row["expected"].as_object().expect("expected");
        let mut value_failures = Vec::new();
        for (key, expected_value) in expected {
            let got_value = &rendered[key];
            if !same_ignoring_object_order(got_value, expected_value) {
                value_failures.push(format!(
                    "{key}: python {expected_value} != rust {got_value}"
                ));
            }
        }
        if value_failures.is_empty() {
            matched += 1;
        } else {
            failures.push(format!("  {name}: {}", value_failures.join("; ")));
        }

        // 3. The declared divergence: the nested maps are sorted, not insertion-ordered.
        for (map_name, python_order) in row["inner_key_order"].as_object().expect("inner_key_order")
        {
            let python_order: Vec<&str> = python_order
                .as_array()
                .expect("order")
                .iter()
                .map(|k| k.as_str().expect("key"))
                .collect();
            let our_order: Vec<&str> = rendered[map_name]
                .as_object()
                .expect("nested map")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                our_order.len(),
                python_order.len(),
                "{name}/{map_name}: the port and Python disagree on which keys exist"
            );
            if map_name == "log_count" {
                // Not a DashMap — the port keeps Python's fixed key order here.
                assert_eq!(
                    our_order, python_order,
                    "{name}/log_count must keep Python's debug/info/warning/error/critical order"
                );
                continue;
            }
            let mut sorted = python_order.clone();
            sorted.sort_unstable();
            assert_eq!(
                our_order, sorted,
                "{name}/{map_name}: the port must render nested maps sorted by key"
            );
            if python_order != sorted {
                inner_order_divergences += 1;
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} CrawlStats rows differ from Python:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
    assert!(
        inner_order_divergences > 0,
        "no corpus row exercises the nested-map ordering divergence any more, so the note \
         below is untested. Never shrink a corpus to make a divergence disappear — add a \
         row whose keys arrive out of sorted order.\n{INNER_ORDER_DIVERGENCE}"
    );
    assert!(
        corpus.len() >= 15,
        "the stats corpus shrank to {} rows",
        corpus.len()
    );
    eprintln!(
        "CrawlStats::to_dict: {matched}/{} rows match ({inner_order_divergences} nested maps \
         exercise the declared ordering divergence)",
        corpus.len()
    );
}
