//! Differential test for the two pure helpers `CrawlerEngine` uses, against the real
//! `reference/spiders/engine.py`.
//!
//! `prefetch_urls` is `_prefetch_robots_txt`'s seed-list expression and
//! `effective_domain_delay` is `_get_domain_delay`'s arithmetic. Both live in
//! `browser-spider::robots` because they are pure, and both are easy to get subtly wrong
//! by reading the spec's prose instead of the code: Python does **not** drop a start URL
//! with no netloc, and does **not** default a missing scheme to `https` in the seed list
//! (only `_get_parser` does that), so `"/relative"` really does become the seed `":///"`.
//!
//! The corpus (`testdata/u16_engine_helpers.jsonl`, from `tools/corpus/u16/gen.py`)
//! replays the engine's own expressions rather than a paraphrase of them.

mod u16_support;

use browser_spider::robots::{effective_domain_delay, prefetch_urls};
use u16_support::{assert_divergences, f64_from_bits, read_corpus, Divergence};

/// None: both helpers are exact.
const DIVERGENCES: &[Divergence] = &[];

#[test]
fn engine_helpers_match_python() {
    let rows = read_corpus("u16_engine_helpers.jsonl");
    assert!(rows.len() >= 41, "corpus shrank to {} rows", rows.len());

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut prefetch_cases = 0usize;
    let mut delay_cases = 0usize;

    for row in &rows {
        let name = row["name"].as_str().expect("name").to_owned();
        match row["kind"].as_str().expect("kind") {
            "prefetch_urls" => {
                prefetch_cases += 1;
                let start_urls: Vec<String> = row["start_urls"]
                    .as_array()
                    .expect("start_urls")
                    .iter()
                    .map(|value| value.as_str().expect("a url").to_owned())
                    .collect();
                let expected: Vec<String> = row["seed_urls"]
                    .as_array()
                    .expect("seed_urls")
                    .iter()
                    .map(|value| value.as_str().expect("a url").to_owned())
                    .collect();
                let actual = prefetch_urls(&start_urls);
                if expected != actual {
                    failures.push((
                        name,
                        format!("{start_urls:?}: python={expected:?} rust={actual:?}"),
                    ));
                }
            }
            "domain_delay" => {
                delay_cases += 1;
                let download_delay =
                    f64_from_bits(&row["download_delay_bits"]).expect("download_delay");
                let crawl_delay = f64_from_bits(&row["crawl_delay_bits"]);
                let request_rate = row["request_rate"].as_array().map(|pair| {
                    (
                        pair[0].as_i64().expect("requests"),
                        pair[1].as_i64().expect("seconds"),
                    )
                });
                let expected = f64_from_bits(&row["delay_bits"]).expect("delay");
                let actual = effective_domain_delay(download_delay, crawl_delay, request_rate);
                // Bit-exact: every step is an IEEE-754 divide or `max` on both sides, and
                // a NaN that reaches this from a `Crawl-delay: nan` must stay a NaN.
                if expected.to_bits() != actual.to_bits() {
                    failures.push((
                        name,
                        format!(
                            "download={download_delay} crawl={crawl_delay:?} \
                             rate={request_rate:?}: python={expected:?} rust={actual:?}"
                        ),
                    ));
                }
            }
            other => panic!("unknown row kind {other}"),
        }
    }

    eprintln!(
        "u16 engine helpers: {prefetch_cases} seed lists + {delay_cases} delay cases, \
         {} mismatches",
        failures.len()
    );
    assert!(prefetch_cases >= 20, "only {prefetch_cases} seed lists");
    assert!(delay_cases >= 15, "only {delay_cases} delay cases");
    assert_divergences("engine helpers", DIVERGENCES, &failures);
}
