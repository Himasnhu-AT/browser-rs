//! Differential test for [`RobotsRules`] against the real `protego`.
//!
//! `testdata/u16_robots.jsonl` (from `tools/corpus/u16/gen.py`) holds one row per
//! `(robots.txt body, user agent)` pair, recording what `protego` answered for
//! `crawl_delay`, `request_rate` (value **and** time window), `visit_time` and `sitemaps`,
//! plus one `can_fetch` verdict per URL in the file's shared header row.
//! `RobotsTxtManager` delegates every one of those to the parser, so this file is what
//! makes the manager trustworthy.
//!
//! 150 hand-written bodies covering every documented directive, misspelling, separator,
//! wildcard form and value spelling, crossed with 74 URLs and 9 agents; a 58-pattern
//! matrix run twice (as a bare `Disallow:` and as an `Allow:` over `Disallow: /`); and 350
//! randomly assembled bodies across five seeds. 166,848 assertions in total.

mod u16_support;

use browser_spider::robots::RobotsRules;
use serde_json::Value;
use u16_support::{assert_divergences, f64_from_bits, read_corpus, Divergence};

/// Rows the port deliberately answers differently.
///
/// One cause, nine keys (one per user-agent the matrix asks about): an unbounded Python
/// integer in a `Request-rate`. Every other difference found while writing this parser was
/// a bug in the port and was fixed.
const DIVERGENCES: &[Divergence] = &[
    Divergence {
        key: "request_rate_huge/*/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/googlebot/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/Googlebot-News/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/some-random-bot/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/Googlebot/2.1/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge//request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/bot/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/rôbot/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
    Divergence {
        key: "request_rate_huge/BOT*/request_rate",
        reason: "Python's ints are unbounded, so `Request-rate: 1/99999999999999999999` gives \
                 `seconds = 10^20` and a domain delay of three thousand years. \
                 `RequestRate::seconds` is an `i64`, so the port drops the directive \
                 instead. Both readings make the site uncrawlable; ignoring an absurd \
                 rate is the more useful of the two, and no real `robots.txt` carries \
                 one. The other 166k assertions in this corpus reproduce exactly.",
    },
];

fn field(row: &Value, key: &str) -> Value {
    row.get(key).cloned().unwrap_or(Value::Null)
}

#[test]
fn robots_rules_match_protego() {
    let rows = read_corpus("u16_robots.jsonl");
    assert!(rows.len() >= 2113, "corpus shrank to {} rows", rows.len());

    // The corpus's first line is a header carrying the URL list every group row's verdict
    // string is indexed by. Storing the URLs once is what keeps a 2000-row file at a few
    // hundred KB rather than tens of megabytes.
    let header = &rows[0];
    assert_eq!(header["kind"], "header", "the first row must be the header");
    let urls: Vec<&str> = header["urls"]
        .as_array()
        .expect("urls")
        .iter()
        .map(|value| value.as_str().expect("a url"))
        .collect();
    assert!(urls.len() >= 70, "only {} URLs in the matrix", urls.len());
    let groups = &rows[1..];

    // Guard against a vacuous corpus: a table where every answer is "allowed, no
    // directives" would pass any implementation at all.
    let denied: usize = groups
        .iter()
        .map(|row| {
            row["verdicts"]
                .as_str()
                .expect("verdicts")
                .bytes()
                .filter(|byte| *byte == b'F')
                .count()
        })
        .sum();
    let with_delay = groups
        .iter()
        .filter(|row| !row["crawl_delay"].is_null())
        .count();
    let with_rate = groups
        .iter()
        .filter(|row| !row["request_rate"].is_null())
        .count();
    let with_sitemaps = groups
        .iter()
        .filter(|row| {
            row["sitemaps"]
                .as_array()
                .is_some_and(|list| !list.is_empty())
        })
        .count();
    assert!(denied > 10_000, "only {denied} verdicts are disallowed");
    assert!(
        with_delay > 100,
        "only {with_delay} rows carry a Crawl-delay"
    );
    assert!(
        with_rate > 100,
        "only {with_rate} rows carry a Request-rate"
    );
    assert!(
        with_sitemaps > 50,
        "only {with_sitemaps} rows carry a Sitemap"
    );

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut checked = 0usize;

    for row in groups {
        let body_name = row["body_name"].as_str().expect("body_name");
        let body = row["body"].as_str().expect("body");
        let agent = row["agent"].as_str().expect("agent");
        let rules = RobotsRules::parse(body);

        let key = |what: &str| format!("{body_name}/{agent}/{what}");
        let mut record = |what: &str, detail: String| failures.push((key(what), detail));

        // -- can_fetch, once per URL in the shared matrix
        let expected_verdicts = row["verdicts"].as_str().expect("verdicts");
        assert_eq!(
            expected_verdicts.len(),
            urls.len(),
            "{body_name}: verdict shape"
        );
        for (url, expected) in urls.iter().zip(expected_verdicts.bytes()) {
            let expected = expected == b'T';
            let actual = rules.can_fetch(url, agent);
            checked += 1;
            if actual != expected {
                record(
                    "can_fetch",
                    format!("{url}: python={expected} rust={actual} body={body:?}"),
                );
            }
        }

        // -- crawl_delay, compared by IEEE bit pattern (see `f64_from_bits`)
        let expected = f64_from_bits(&field(row, "crawl_delay_bits"));
        let actual = rules.crawl_delay(agent);
        checked += 1;
        if !bit_equal(expected, actual) {
            record(
                "crawl_delay",
                format!("python={expected:?} rust={actual:?} body={body:?}"),
            );
        }

        // -- request_rate (requests, seconds), as decimal strings because Python's ints
        //    are unbounded and JSON would round a 20-digit `Request-rate` to a float.
        let expected = field(row, "request_rate");
        let actual = rules.request_rate(agent).map_or(Value::Null, |rate| {
            Value::from(vec![
                Value::from(rate.requests.to_string()),
                Value::from(rate.seconds.to_string()),
            ])
        });
        checked += 1;
        if expected != actual {
            record(
                "request_rate",
                format!("python={expected} rust={actual} body={body:?}"),
            );
        }

        // -- the Request-rate time window
        let expected = field(row, "request_rate_window");
        let actual = rules
            .request_rate(agent)
            .and_then(|rate| rate.start_time.zip(rate.end_time))
            .map_or(Value::Null, |(start, end)| {
                Value::from(vec![
                    Value::from(start.hour),
                    Value::from(start.minute),
                    Value::from(end.hour),
                    Value::from(end.minute),
                ])
            });
        checked += 1;
        if expected != actual {
            record(
                "request_rate_window",
                format!("python={expected} rust={actual} body={body:?}"),
            );
        }

        // -- visit_time
        let expected = field(row, "visit_time");
        let actual = rules.visit_time(agent).map_or(Value::Null, |visit| {
            Value::from(vec![
                Value::from(visit.start_time.hour),
                Value::from(visit.start_time.minute),
                Value::from(visit.end_time.hour),
                Value::from(visit.end_time.minute),
            ])
        });
        checked += 1;
        if expected != actual {
            record(
                "visit_time",
                format!("python={expected} rust={actual} body={body:?}"),
            );
        }

        // -- sitemaps
        let expected = field(row, "sitemaps");
        let actual = Value::from(
            rules
                .sitemaps()
                .iter()
                .map(|url| Value::from(url.clone()))
                .collect::<Vec<_>>(),
        );
        checked += 1;
        if expected != actual {
            record(
                "sitemaps",
                format!("python={expected} rust={actual} body={body:?}"),
            );
        }
    }

    eprintln!(
        "u16 robots: {checked} assertions over {} (body, agent) groups, {} mismatching keys",
        groups.len(),
        failures.len()
    );
    assert!(checked > 150_000, "only {checked} assertions");
    assert_divergences("robots.txt", DIVERGENCES, &failures);
}

/// Bit-exact, so `-0.0` is distinguished from `0.0` and a NaN `Crawl-delay` (which
/// `protego`'s `float()` really does accept) compares equal to a NaN.
fn bit_equal(expected: Option<f64>, actual: Option<f64>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(left), Some(right)) => left.to_bits() == right.to_bits(),
        _ => false,
    }
}
