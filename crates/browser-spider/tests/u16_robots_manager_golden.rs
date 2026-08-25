//! Differential test for [`RobotsTxtManager`] against `reference.spiders.robotstxt`.
//!
//! `tests/u16_robots_golden.rs` pins the *parser*. This file pins the layer above it — the
//! one the protego corpus cannot reach:
//!
//! * which `robots.txt` URL is requested for a given page URL, including the `https`
//!   default when the page URL has no scheme;
//! * what the cache is keyed on (`urlparse(url).netloc`, so host **and port** and **case**,
//!   but not scheme, and not path), and therefore how many fetches a sequence of URLs
//!   costs;
//! * the degradation rule — a non-200 status, a transport error, an undecodable body, a
//!   binary body and an unparseable body must all end at "allow everything, no delays",
//!   because a broken `robots.txt` may never stop a crawl.
//!
//! Both corpora come from `tools/corpus/u16/gen.py`, which drives the real
//! `RobotsTxtManager` with a scripted `fetch_fn` under `asyncio.run`.

mod u16_support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;

use browser_spider::error::SpiderError;
use browser_spider::robots::{fetcher_fn, RobotsTxtManager};
use browser_spider::session::FetchedResponse;
use u16_support::{assert_divergences, f64_from_bits, read_corpus, Divergence};

/// Scenarios the port answers differently from Python.
const MANAGER_DIVERGENCES: &[Divergence] = &[
    Divergence {
        key: "body_empty_encoding_label/can_fetch",
        reason: "Python's `Response.encoding` defaults to `\"utf-8\"` and is never the empty \
                 string, so `body.decode(\"\")` is unreachable there; this corpus row \
                 reaches it only because the fake response sets it deliberately. \
                 `FetchedResponse` derives `Default`, which *does* leave `encoding` \
                 empty, and reading that as the empty codec label would make every \
                 such `robots.txt` silently allow everything - the dangerous \
                 direction. `parser_for` therefore reads an empty field as \"unset\" \
                 and decodes as UTF-8. `decode_robots_body(body, \"\")` on its own \
                 still answers `None`, exactly like `bytes.decode(\"\")`, and \
                 `u16_decode.jsonl` pins that.",
    },
    Divergence {
        key: "body_empty_encoding_label/crawl_delay",
        reason: "Python's `Response.encoding` defaults to `\"utf-8\"` and is never the empty \
                 string, so `body.decode(\"\")` is unreachable there; this corpus row \
                 reaches it only because the fake response sets it deliberately. \
                 `FetchedResponse` derives `Default`, which *does* leave `encoding` \
                 empty, and reading that as the empty codec label would make every \
                 such `robots.txt` silently allow everything - the dangerous \
                 direction. `parser_for` therefore reads an empty field as \"unset\" \
                 and decodes as UTF-8. `decode_robots_body(body, \"\")` on its own \
                 still answers `None`, exactly like `bytes.decode(\"\")`, and \
                 `u16_decode.jsonl` pins that.",
    },
    Divergence {
        key: "body_empty_encoding_label/request_rate",
        reason: "Python's `Response.encoding` defaults to `\"utf-8\"` and is never the empty \
                 string, so `body.decode(\"\")` is unreachable there; this corpus row \
                 reaches it only because the fake response sets it deliberately. \
                 `FetchedResponse` derives `Default`, which *does* leave `encoding` \
                 empty, and reading that as the empty codec label would make every \
                 such `robots.txt` silently allow everything - the dangerous \
                 direction. `parser_for` therefore reads an empty field as \"unset\" \
                 and decodes as UTF-8. `decode_robots_body(body, \"\")` on its own \
                 still answers `None`, exactly like `bytes.decode(\"\")`, and \
                 `u16_decode.jsonl` pins that.",
    },
    Divergence {
        key: "body_empty_encoding_label/sitemaps",
        reason: "Python's `Response.encoding` defaults to `\"utf-8\"` and is never the empty \
                 string, so `body.decode(\"\")` is unreachable there; this corpus row \
                 reaches it only because the fake response sets it deliberately. \
                 `FetchedResponse` derives `Default`, which *does* leave `encoding` \
                 empty, and reading that as the empty codec label would make every \
                 such `robots.txt` silently allow everything - the dangerous \
                 direction. `parser_for` therefore reads an empty field as \"unset\" \
                 and decodes as UTF-8. `decode_robots_body(body, \"\")` on its own \
                 still answers `None`, exactly like `bytes.decode(\"\")`, and \
                 `u16_decode.jsonl` pins that.",
    },
    Divergence {
        key: "body_empty_encoding_label/probes",
        reason: "Python's `Response.encoding` defaults to `\"utf-8\"` and is never the empty \
                 string, so `body.decode(\"\")` is unreachable there; this corpus row \
                 reaches it only because the fake response sets it deliberately. \
                 `FetchedResponse` derives `Default`, which *does* leave `encoding` \
                 empty, and reading that as the empty codec label would make every \
                 such `robots.txt` silently allow everything - the dangerous \
                 direction. `parser_for` therefore reads an empty field as \"unset\" \
                 and decodes as UTF-8. `decode_robots_body(body, \"\")` on its own \
                 still answers `None`, exactly like `bytes.decode(\"\")`, and \
                 `u16_decode.jsonl` pins that.",
    },
];

/// A fetcher that records every requested URL and replays one scripted outcome.
fn scripted(
    log: Arc<Mutex<Vec<String>>>,
    outcome: Result<FetchedResponse, String>,
) -> Arc<dyn browser_spider::robots::RobotsFetcher> {
    Arc::new(fetcher_fn(move |url: String, _sid: String| {
        let log = Arc::clone(&log);
        let outcome = outcome.clone();
        async move {
            log.lock().expect("not poisoned").push(url);
            outcome.map_err(|message| SpiderError::Fetch {
                sid: "sid-1".to_owned(),
                url: "robots.txt".to_owned(),
                message,
            })
        }
    }))
}

fn response_from(fetch: &serde_json::Value) -> Result<FetchedResponse, String> {
    match fetch["kind"].as_str().expect("kind") {
        "error" => Err(fetch["message"].as_str().expect("message").to_owned()),
        "ok" => {
            let body = hex_bytes(fetch["body_hex"].as_str().expect("body_hex"));
            let mut response = FetchedResponse::new(
                u16::try_from(fetch["status"].as_u64().expect("status")).expect("a status"),
                "https://example.com/robots.txt",
                body,
            );
            response.encoding = fetch["encoding"].as_str().expect("encoding").to_owned();
            Ok(response)
        }
        other => panic!("unknown fetch kind {other}"),
    }
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("hex pair"))
        .collect()
}

#[tokio::test]
async fn robots_manager_matches_python() {
    let rows = read_corpus("u16_robots_manager.jsonl");
    assert!(rows.len() >= 56, "corpus shrank to {} rows", rows.len());

    // Guard against a vacuous corpus: if nothing were ever denied, an always-allow
    // implementation would pass.
    let denied = rows
        .iter()
        .filter(|row| row["can_fetch"] == serde_json::Value::Bool(false))
        .count();
    assert!(denied >= 10, "only {denied} scenarios deny the URL");

    let mut failures: Vec<(String, String)> = Vec::new();

    for row in &rows {
        let name = row["name"].as_str().expect("name");
        let url = row["url"].as_str().expect("url");
        let log = Arc::new(Mutex::new(Vec::new()));
        let manager =
            RobotsTxtManager::new(scripted(Arc::clone(&log), response_from(&row["fetch"])));

        let can_fetch = manager.can_fetch(url, "sid-1").await;
        let (crawl_delay, rate) = manager.get_delay_directives(url, "sid-1").await;
        let sitemaps = manager.sitemaps(url, "sid-1").await;
        let mut probes: Vec<(String, bool)> = Vec::new();
        for probe in row["probes"].as_array().expect("probes") {
            let probe_url = probe[0].as_str().expect("probe url").to_owned();
            let verdict = manager.can_fetch(&probe_url, "sid-1").await;
            probes.push((probe_url, verdict));
        }

        let mut check = |what: &str, expected: String, actual: String| {
            if expected != actual {
                failures.push((
                    format!("{name}/{what}"),
                    format!("python={expected} rust={actual}"),
                ));
            }
        };

        check(
            "can_fetch",
            row["can_fetch"].to_string(),
            can_fetch.to_string(),
        );
        check(
            "crawl_delay",
            format!("{:?}", f64_from_bits(&row["crawl_delay_bits"])),
            format!("{crawl_delay:?}"),
        );
        let expected_rate = row["request_rate"].as_array().map(|pair| {
            (
                pair[0].as_i64().expect("requests"),
                pair[1].as_i64().expect("seconds"),
            )
        });
        check(
            "request_rate",
            format!("{expected_rate:?}"),
            format!("{rate:?}"),
        );
        let expected_sitemaps: Vec<String> = row["sitemaps"]
            .as_array()
            .expect("sitemaps")
            .iter()
            .map(|value| value.as_str().expect("sitemap").to_owned())
            .collect();
        check(
            "sitemaps",
            format!("{expected_sitemaps:?}"),
            format!("{sitemaps:?}"),
        );

        // Exactly one fetch per netloc: the four calls above plus the probes must not
        // re-request a domain that is already cached.
        let expected_requested: Vec<String> = row["requested"]
            .as_array()
            .expect("requested")
            .iter()
            .map(|value| value.as_str().expect("a url").to_owned())
            .collect();
        let actual_requested = log.lock().expect("not poisoned").clone();
        check(
            "requested",
            format!("{expected_requested:?}"),
            format!("{actual_requested:?}"),
        );

        let expected_domains: Vec<String> = row["cached_domains"]
            .as_array()
            .expect("cached_domains")
            .iter()
            .map(|value| value.as_str().expect("a domain").to_owned())
            .collect();
        check(
            "cached_domains",
            format!("{expected_domains:?}"),
            format!("{:?}", manager.cached_domains()),
        );

        let expected_probes: Vec<(String, bool)> = row["probes"]
            .as_array()
            .expect("probes")
            .iter()
            .map(|probe| {
                (
                    probe[0].as_str().expect("url").to_owned(),
                    probe[1].as_bool().expect("verdict"),
                )
            })
            .collect();
        check(
            "probes",
            format!("{expected_probes:?}"),
            format!("{probes:?}"),
        );
    }

    eprintln!(
        "u16 robots manager: {} scenarios, {} mismatches",
        rows.len(),
        failures.len()
    );
    assert_divergences("RobotsTxtManager", MANAGER_DIVERGENCES, &failures);
}

#[tokio::test]
async fn robots_manager_cache_keying_matches_python() {
    let rows = read_corpus("u16_robots_cache.jsonl");
    assert!(rows.len() >= 7, "corpus shrank to {} rows", rows.len());

    let mut failures: Vec<(String, String)> = Vec::new();

    for row in &rows {
        let name = row["name"].as_str().expect("name");
        let log = Arc::new(Mutex::new(Vec::new()));
        let manager = RobotsTxtManager::new(scripted(
            Arc::clone(&log),
            Ok({
                let mut response = FetchedResponse::new(
                    200,
                    "https://a.test/robots.txt",
                    "User-agent: *\nDisallow: /admin/\n",
                );
                response.encoding = "utf-8".to_owned();
                response
            }),
        ));

        let mut verdicts: Vec<bool> = Vec::new();
        for url in row["urls"].as_array().expect("urls") {
            verdicts.push(manager.can_fetch(url.as_str().expect("a url"), "s").await);
        }

        let expected_requested: Vec<String> = row["requested"]
            .as_array()
            .expect("requested")
            .iter()
            .map(|value| value.as_str().expect("a url").to_owned())
            .collect();
        let actual_requested = log.lock().expect("not poisoned").clone();
        if expected_requested != actual_requested {
            failures.push((
                format!("{name}/requested"),
                format!("python={expected_requested:?} rust={actual_requested:?}"),
            ));
        }

        let expected_verdicts: Vec<bool> = row["verdicts"]
            .as_array()
            .expect("verdicts")
            .iter()
            .map(|value| value.as_bool().expect("a verdict"))
            .collect();
        if expected_verdicts != verdicts {
            failures.push((
                format!("{name}/verdicts"),
                format!("python={expected_verdicts:?} rust={verdicts:?}"),
            ));
        }

        let expected_domains: Vec<String> = row["cached_domains"]
            .as_array()
            .expect("cached_domains")
            .iter()
            .map(|value| value.as_str().expect("a domain").to_owned())
            .collect();
        if expected_domains != manager.cached_domains() {
            failures.push((
                format!("{name}/cached_domains"),
                format!(
                    "python={expected_domains:?} rust={:?}",
                    manager.cached_domains()
                ),
            ));
        }
    }

    eprintln!(
        "u16 robots manager cache: {} sequences, {} mismatches",
        rows.len(),
        failures.len()
    );
    assert_divergences("RobotsTxtManager cache", &[], &failures);
}

// ---------------------------------------------------------------------------
// The decode step between the fetch and the parser
// ---------------------------------------------------------------------------

/// Codec labels this port resolves but does not implement byte-for-byte.
///
/// Every one of them is an ASCII superset, and every `robots.txt` *directive* is ASCII, so
/// the ruleset a spider ends up with is identical — only the code points inside a comment
/// or a non-ASCII path differ. The UTF-16 and UTF-32 families, where an ASCII-shaped body
/// would come out unreadable, are implemented rather than declared, as are the codecs
/// `bytes.decode` refuses outright.
///
/// The list is self-policing: [`assert_divergences`] is fed one key per declared codec, so
/// a codec that starts round-tripping — or that stops being exercised because a corpus row
/// was removed — fails this test instead of sitting here as a stale excuse.
const DECODE_DIVERGENCES: &[Divergence] = &[
    Divergence {
        key: "cp1251",
        reason: "the Cyrillic single-byte page; decoded as UTF-8 here",
    },
    Divergence {
        key: "koi8_r",
        reason: "the other Cyrillic single-byte page; decoded as UTF-8 here",
    },
    Divergence {
        key: "iso8859_15",
        reason: "latin-9: eight code points differ from latin-1; decoded as UTF-8 here",
    },
    Divergence {
        key: "mac_roman",
        reason: "the classic Mac OS single-byte page; decoded as UTF-8 here",
    },
    Divergence {
        key: "big5",
        reason: "traditional Chinese, a double-byte ASCII superset; decoded as UTF-8 here",
    },
    Divergence {
        key: "gb2312",
        reason: "simplified Chinese, a double-byte ASCII superset; decoded as UTF-8 here",
    },
    Divergence {
        key: "shift_jis",
        reason: "Japanese, a double-byte ASCII superset; decoded as UTF-8 here",
    },
    Divergence {
        key: "utf_7",
        reason: "a modified-base64 spelling of UTF-16 that leaves unreserved ASCII alone, \
                 so directives survive; decoded as UTF-8 here",
    },
    Divergence {
        key: "unicode_escape",
        reason: "a source-literal codec, never a document encoding; decoded as UTF-8 here",
    },
];

#[test]
fn decode_matches_python() {
    let rows = read_corpus("u16_decode.jsonl");
    assert!(rows.len() >= 1000, "corpus shrank to {} rows", rows.len());

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut declared_hits: BTreeSet<String> = BTreeSet::new();
    let mut lookup_errors = 0usize;
    let mut exact = 0usize;
    let mut declared_rows = 0usize;

    for row in &rows {
        let body = hex_bytes(row["body_hex"].as_str().expect("body_hex"));
        let label = row["label"].as_str().expect("label");
        let name = format!("{}/{label}", row["body_name"].as_str().expect("body_name"));
        let actual = browser_spider::robots::decode_robots_body(&body, label);

        // A declared codec is still required to *resolve* the same way Python does; only
        // the decoded code points are allowed to differ.
        let declared = declared_module(label);
        let record = |failures: &mut Vec<(String, String)>, detail: String| {
            failures.push((declared.unwrap_or(&name).to_owned(), detail));
        };

        match row["decoded"].as_str() {
            None => {
                lookup_errors += 1;
                exact += 1;
                if let Some(text) = actual {
                    record(
                        &mut failures,
                        format!(
                            "{name}: python raised {} rust={text:?}",
                            row["raises"].as_str().unwrap_or("?")
                        ),
                    );
                }
            }
            Some(expected) => match actual {
                None => record(
                    &mut failures,
                    format!("{name}: python={expected:?} rust=LookupError"),
                ),
                Some(text) if text == expected => exact += 1,
                Some(text) => match declared {
                    Some(module) => {
                        declared_rows += 1;
                        declared_hits.insert(module.to_owned());
                    }
                    None => failures.push((name, format!("python={expected:?} rust={text:?}"))),
                },
            },
        }
    }

    // Feed one synthetic "failure" per declared codec that really did differ, so
    // `assert_divergences` can flag a declaration that no longer fires.
    let declared_hits_count = declared_hits.len();
    let mut declared_failures: Vec<(String, String)> = declared_hits
        .into_iter()
        .map(|module| (module, "decoded as UTF-8 instead".to_owned()))
        .collect();
    declared_failures.append(&mut failures);

    eprintln!(
        "u16 decode: {} rows ({lookup_errors} raise in Python), {exact} exact, \
         {declared_rows} in declared codecs, {} undeclared",
        rows.len(),
        declared_failures.len() - declared_hits_count
    );
    assert!(
        lookup_errors >= 50,
        "only {lookup_errors} rows raise in Python"
    );
    assert_divergences("decode_robots_body", DECODE_DIVERGENCES, &declared_failures);
}

/// The declared codec a label belongs to, if any. Mirrors Python's alias resolution for
/// just the labels the corpus uses, which keeps the divergence list keyed on the codec
/// rather than on each of its spellings.
fn declared_module(label: &str) -> Option<&'static str> {
    Some(match label {
        "cp1251" => "cp1251",
        "koi8-r" => "koi8_r",
        "iso-8859-15" => "iso8859_15",
        "mac-roman" => "mac_roman",
        "big5" => "big5",
        "gb2312" => "gb2312",
        "shift_jis" => "shift_jis",
        "utf-7" => "utf_7",
        "unicode-escape" => "unicode_escape",
        _ => return None,
    })
}
