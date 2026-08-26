//! Differential tests for `parse_retry_after` and [`AutoThrottle`] against
//! `reference.spiders.throttle`.
//!
//! Two corpora from `tools/corpus/u16/gen.py`:
//!
//! * `u16_retry_after.jsonl` — 2858 ordered header lists in, a deterministic classification
//!   out. Covers presence/absence/casing/**order** of the header, every `float()` spelling
//!   CPython accepts (Unicode decimal digits and the `\x1c`–`\x1f` separators included),
//!   ~65 hand-written HTTP/RFC-2822 date forms and 2700 fuzzed values over three seeds.
//!   A date's answer depends on "now", so the row pins the *instant* Python resolved the
//!   header to and this test brackets its own clock around the call.
//! * `u16_autothrottle.jsonl` — 167 scripted `delay_for` / `record` / `reset` sequences
//!   replayed against the real `AutoThrottle`, recording each return value **and** the
//!   final `delays` dict: the constructor guards, 30 worked examples, the non-finite
//!   inputs (`inf`/`nan` latency, `Retry-After: inf`, a `nan` floor) and 120 random
//!   sequences across three seeds.
//!
//! The delays are compared **bit-exactly**. Every operation in `record` is an IEEE-754
//! add, divide, `max` and `min` on doubles, so any difference at all would mean the port
//! reordered the arithmetic.
//!
//! The two concurrency properties — independent domains, and no lost updates when several
//! threads `record` one domain — are pinned in `src/throttle.rs`, where the `DashMap` entry
//! guard they depend on lives.

mod u16_support;

use std::collections::BTreeMap;

use browser_spider::throttle::{
    parse_retry_after, AutoThrottle, AutoThrottleConfig, AutoThrottleConfigError,
};
use indexmap::IndexMap;
use serde_json::Value;
use u16_support::{assert_divergences, f64_from_bits, read_corpus, Divergence};

// ---------------------------------------------------------------------------
// parse_retry_after
// ---------------------------------------------------------------------------

/// Header values Python reads and this port does not, or vice versa.
///
/// Empty, and the emptiness is enforced: `assert_divergences` fails on a declaration that
/// starts reproducing, so a stale excuse cannot survive. The rows that *used* to be here —
/// Unicode decimal digits, and the obsolete RFC-822 zone names — are now ported.
const RETRY_AFTER_DIVERGENCES: &[Divergence] = &[];

/// Ordered header pairs, exactly as the corpus recorded Python's insertion order.
///
/// `parse_retry_after` returns the value of the **first** key whose lower-case spelling is
/// `retry-after`, so the order is load-bearing. The corpus stores a JSON *array* of pairs
/// rather than an object for precisely this reason: an object would be reordered by the
/// writer's `sort_keys` and again by `serde_json`'s map, and the ordering rule would
/// quietly stop being tested.
fn headers_of(row: &Value) -> IndexMap<String, String> {
    row["headers"]
        .as_array()
        .expect("headers array")
        .iter()
        .map(|pair| {
            let pair = pair.as_array().expect("a [key, value] pair");
            (
                pair[0].as_str().expect("key").to_owned(),
                pair[1].as_str().expect("value").to_owned(),
            )
        })
        .collect()
}

#[test]
fn parse_retry_after_matches_python() {
    let rows = read_corpus("u16_retry_after.jsonl");
    assert!(rows.len() >= 2858, "corpus shrank to {} rows", rows.len());

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();

    for row in &rows {
        let headers = headers_of(row);
        let name = row["name"].as_str().expect("name").to_owned();
        let outcome = row["outcome"].as_str().expect("outcome");
        *counts.entry(outcome).or_default() += 1;

        // `Utc::now()` is read on both sides of the call so the HTTP-date branch can be
        // bracketed rather than fuzzily compared: the answer must be `now`-relative and
        // inside the interval the two readings allow, which is exact to the microsecond
        // for a past date (both ends clamp to 0.0) and a two-tick window for a future one.
        let before = chrono::Utc::now();
        let actual = parse_retry_after(&headers);
        let after = chrono::Utc::now();

        match outcome {
            "none" => {
                if actual.is_some() {
                    failures.push((name, format!("python=None rust={actual:?}")));
                }
            }
            "number" => {
                let expected = f64_from_bits(&row["value_bits"]).expect("value_bits");
                let matched = match actual {
                    Some(value) if row["value_is_nan"].as_bool() == Some(true) => value.is_nan(),
                    Some(value) => value.to_bits() == expected.to_bits(),
                    None => false,
                };
                if !matched {
                    failures.push((name, format!("python={expected:?} rust={actual:?}")));
                }
            }
            "date" => {
                let micros = row["date_epoch_micros"].as_i64().expect("epoch micros");
                let seconds_from = |now: chrono::DateTime<chrono::Utc>| {
                    let now_micros = now.timestamp_micros();
                    ((micros - now_micros) as f64 / 1_000_000.0).max(0.0)
                };
                // `before` is the earliest `now` the call could have read, so it yields the
                // largest allowed answer; `after` yields the smallest.
                let high = seconds_from(before);
                let low = seconds_from(after);
                match actual {
                    Some(value) if value >= low - 1e-6 && value <= high + 1e-6 => {}
                    other => {
                        failures.push((name, format!("python in [{low}, {high}] rust={other:?}")))
                    }
                }
            }
            other => panic!("unknown outcome {other}"),
        }
    }

    eprintln!(
        "u16 retry-after: {} rows {counts:?}, {} mismatches",
        rows.len(),
        failures.len()
    );
    assert!(
        counts["none"] > 200,
        "only {} unreadable rows",
        counts["none"]
    );
    assert!(
        counts["number"] > 100,
        "only {} numeric rows",
        counts["number"]
    );
    assert!(counts["date"] > 100, "only {} date rows", counts["date"]);
    assert_divergences("parse_retry_after", RETRY_AFTER_DIVERGENCES, &failures);
}

// ---------------------------------------------------------------------------
// AutoThrottle
// ---------------------------------------------------------------------------

/// Scripts the port answers differently. Only the NaN guard, which is deliberate.
const AUTOTHROTTLE_DIVERGENCES: &[Divergence] = &[];

fn config_of(row: &Value) -> AutoThrottleConfig {
    let bits = &row["config_bits"];
    AutoThrottleConfig {
        start_delay: f64_from_bits(&bits["start_delay"]).expect("start_delay"),
        max_delay: f64_from_bits(&bits["max_delay"]).expect("max_delay"),
        target_concurrency: f64_from_bits(&bits["target_concurrency"]).expect("target_concurrency"),
        block_backoff: row["config"]["block_backoff"]
            .as_bool()
            .expect("block_backoff"),
    }
}

#[test]
fn autothrottle_matches_python() {
    let rows = read_corpus("u16_autothrottle.jsonl");
    assert!(rows.len() >= 167, "corpus shrank to {} rows", rows.len());

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut operations = 0usize;

    for row in &rows {
        let name = row["name"].as_str().expect("name").to_owned();
        let config = config_of(row);

        // -- the constructor guards, including which message comes first
        let expected_error = row["raises"].as_str();
        let throttle = match (AutoThrottle::new(config), expected_error) {
            (Err(error), Some(message)) => {
                if error.to_string() != message {
                    failures.push((
                        format!("{name}/raises"),
                        format!("python={message:?} rust={:?}", error.to_string()),
                    ));
                }
                continue;
            }
            (Err(error), None) => {
                failures.push((format!("{name}/raises"), format!("python=ok rust={error}")));
                continue;
            }
            (Ok(_), Some(message)) => {
                failures.push((
                    format!("{name}/raises"),
                    format!("python={message:?} rust=ok"),
                ));
                continue;
            }
            (Ok(throttle), None) => throttle,
        };

        // -- replay the script
        let ops = row["ops"].as_array().expect("ops");
        let ops_bits = row["ops_bits"].as_array().expect("ops_bits");
        let expected_results = row["results_bits"].as_array().expect("results_bits");
        assert_eq!(ops.len(), expected_results.len(), "{name}: shape");

        for (index, (op, expected)) in ops.iter().zip(expected_results).enumerate() {
            operations += 1;
            let numbers = &ops_bits[index];
            let floor = f64_from_bits(&numbers["floor"]).unwrap_or(0.0);
            let actual: Option<f64> = match op["op"].as_str().expect("op") {
                "delay_for" => {
                    Some(throttle.delay_for(op["domain"].as_str().expect("domain"), floor))
                }
                "record" => Some(throttle.record(
                    op["domain"].as_str().expect("domain"),
                    f64_from_bits(&numbers["latency"]).expect("latency"),
                    op["ok"].as_bool().expect("ok"),
                    floor,
                    f64_from_bits(&numbers["retry_after"]),
                )),
                "reset" => {
                    throttle.reset();
                    None
                }
                other => panic!("unknown op {other}"),
            };
            let expected = f64_from_bits(expected);
            if !bit_equal(expected, actual) {
                failures.push((
                    format!("{name}/op{index}"),
                    format!("{op} => python={expected:?} rust={actual:?}"),
                ));
            }
        }

        // -- the final state, which is what lands in `stats.autothrottle_delays`
        let expected_delays: BTreeMap<String, f64> = row["delays_bits"]
            .as_object()
            .expect("delays_bits")
            .iter()
            .map(|(domain, delay)| {
                (
                    domain.clone(),
                    f64_from_bits(delay).expect("a delay bit pattern"),
                )
            })
            .collect();
        let actual_delays = throttle.delays();
        if expected_delays.len() != actual_delays.len()
            || expected_delays.iter().any(|(domain, delay)| {
                !actual_delays
                    .get(domain)
                    .is_some_and(|actual| actual.to_bits() == delay.to_bits())
            })
        {
            failures.push((
                format!("{name}/delays"),
                format!("python={expected_delays:?} rust={actual_delays:?}"),
            ));
        }
    }

    eprintln!(
        "u16 autothrottle: {} scripts, {operations} operations, {} mismatches",
        rows.len(),
        failures.len()
    );
    assert!(operations > 400, "only {operations} operations replayed");
    assert_divergences("AutoThrottle", AUTOTHROTTLE_DIVERGENCES, &failures);
}

/// Bit-exact, because every step is IEEE-754 arithmetic on both sides.
fn bit_equal(expected: Option<f64>, actual: Option<f64>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(left), Some(right)) => left.to_bits() == right.to_bits(),
        _ => false,
    }
}

/// The NaN guard is the one place the port is stricter than Python, and it is not in the
/// corpus because `AutoThrottle(target_concurrency=nan)` is not constructible from JSON.
#[test]
fn nan_target_concurrency_is_rejected_here_and_accepted_there() {
    let error = AutoThrottle::new(AutoThrottleConfig {
        target_concurrency: f64::NAN,
        ..AutoThrottleConfig::default()
    })
    .expect_err("NaN is rejected");
    assert_eq!(error, AutoThrottleConfigError::TargetConcurrency);
    // Python's `if target_concurrency <= 0` is False for NaN, so it builds a throttle whose
    // every delay is NaN. Listed for docs/DIVERGENCES.md; unit 20 folds it in.
}
