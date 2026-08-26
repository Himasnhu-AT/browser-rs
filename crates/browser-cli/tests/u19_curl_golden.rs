//! Replay of `testdata/u19_curl.jsonl` — the `CurlParser` differential.
//!
//! The corpus was produced by running **the reference implementation 0.4.15's own
//! `CurlParser.parse`** over 93 curl command lines
//! (`tools/corpus/u19/gen_curl.py`) and recording the `Request` namedtuple, or
//! the exception, or the bare `None` it produced. This file replays it in pure
//! Rust: no Python, no network.
//!
//! Python has three outcomes and they are all pinned separately, because the
//! difference is observable:
//!
//! | Python | here |
//! |---|---|
//! | a `Request` | `Ok(CurlRequest)` |
//! | `raise AttributeError` | `Err(e)` with `e.is_reraised()` |
//! | `return None` (a logged `ValueError`) | `Err(e)` with `!e.is_reraised()` |
//!
//! Every declared divergence below is asserted to **keep** differing, so a
//! future change that accidentally fixes one fails this test rather than
//! drifting silently.

mod support;

use std::collections::BTreeMap;

use browser_cli::commands::uncurl::{parse_curl, CurlBody, CurlParseError, CurlRequest};
use serde_json::Value;

/// Rows whose Rust answer is deliberately not Python's, with the reason.
///
/// Each entry is `(row id, what differs and why)`. The test asserts that the row
/// **still** differs; delete an entry only together with the behaviour that
/// caused it.
const DIVERGENCES: &[(&str, &str)] = &[];

/// One field comparison.
struct Tally {
    checked: usize,
    matched: usize,
    failures: Vec<String>,
}

impl Tally {
    fn new() -> Self {
        Self {
            checked: 0,
            matched: 0,
            failures: Vec::new(),
        }
    }

    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, row: &str, field: &str, want: &T, got: &T) {
        self.checked += 1;
        if want == got {
            self.matched += 1;
        } else {
            self.failures
                .push(format!("{row}.{field}: want {want:?}, got {got:?}"));
        }
    }
}

fn corpus() -> Vec<Value> {
    let raw = include_str!("../testdata/u19_curl.jsonl");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON row"))
        .collect()
}

fn object_of(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .expect("an object")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.as_str().expect("a string value").to_owned(),
            )
        })
        .collect()
}

fn rust_map(map: &browser_cli::commands::uncurl::OrderedMap) -> BTreeMap<String, String> {
    map.iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn compare(row_id: &str, want: &Value, got: &CurlRequest, tally: &mut Tally) {
    tally.eq(
        row_id,
        "method",
        &want["method"].as_str().expect("a string").to_owned(),
        &got.method,
    );
    tally.eq(
        row_id,
        "url",
        &want["url"].as_str().expect("a string").to_owned(),
        &got.url,
    );
    tally.eq(
        row_id,
        "params",
        &object_of(&want["params"]),
        &rust_map(&got.params),
    );
    tally.eq(
        row_id,
        "headers",
        &object_of(&want["headers"]),
        &rust_map(&got.headers),
    );
    tally.eq(
        row_id,
        "cookies",
        &object_of(&want["cookies"]),
        &rust_map(&got.cookies),
    );

    // The body, by kind and by value. Python's `data` is one of `str`, `bytes`,
    // `dict` or `None`, and the four are four different Rust shapes.
    let kind = want["data_kind"].as_str().expect("a kind");
    let (want_kind, want_body): (&str, Option<CurlBody>) = match kind {
        "none" => ("none", None),
        "str" => (
            "str",
            Some(CurlBody::Text(
                want["data"].as_str().expect("a string").to_owned(),
            )),
        ),
        "bytes" => (
            "bytes",
            Some(CurlBody::Bytes(
                want["data"].as_str().expect("a string").as_bytes().to_vec(),
            )),
        ),
        "dict" => (
            "dict",
            Some(CurlBody::Form(
                want["data"]
                    .as_object()
                    .expect("an object")
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().expect("a string").to_owned()))
                    .collect(),
            )),
        ),
        other => panic!("{row_id}: unknown data kind {other}"),
    };
    let got_kind = match &got.data {
        None => "none",
        Some(CurlBody::Text(_)) => "str",
        Some(CurlBody::Bytes(_)) => "bytes",
        Some(CurlBody::Form(_)) => "dict",
    };
    tally.eq(row_id, "data_kind", &want_kind, &got_kind);
    tally.eq(row_id, "data", &want_body, &got.data);

    tally.eq(
        row_id,
        "json_data",
        &want["json_data"],
        &json_of(got.json.as_ref()),
    );

    // `{"http": …, "https": …}` or `None`.
    let want_proxy: Option<(String, String)> = want["proxy"].as_object().map(|proxy| {
        (
            proxy["http"].as_str().expect("a string").to_owned(),
            proxy["https"].as_str().expect("a string").to_owned(),
        )
    });
    let got_proxy = got.proxy.as_ref().map(|spec| {
        (
            spec.http.clone().unwrap_or_default(),
            spec.https.clone().unwrap_or_default(),
        )
    });
    tally.eq(row_id, "proxy", &want_proxy, &got_proxy);

    tally.eq(
        row_id,
        "follow_redirects",
        &want["follow_redirects"]
            .as_str()
            .expect("a string")
            .to_owned(),
        &"safe".to_owned(),
    );
}

fn json_of(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or(Value::Null)
}

#[test]
fn the_curl_parser_reproduces_python() {
    let rows = corpus();
    assert!(
        rows.len() >= 90,
        "the corpus is 93 rows, got {}",
        rows.len()
    );

    let mut tally = Tally::new();
    let mut divergent_still_differ: Vec<&str> = Vec::new();

    for row in &rows {
        let id = row["id"].as_str().expect("an id");
        let command = row["command"].as_str().expect("a command");
        let declared = DIVERGENCES.iter().any(|(name, _)| *name == id);

        let outcome = row["outcome"].as_str().expect("an outcome");
        let parsed = parse_curl(command);

        let agreed = match (outcome, &parsed) {
            ("request", Ok(request)) => {
                if declared {
                    // Compare into a throw-away tally so a declared row never
                    // contributes to the headline number in either direction.
                    let mut scratch = Tally::new();
                    compare(id, &row["request"], request, &mut scratch);
                    scratch.failures.is_empty()
                } else {
                    let before = tally.failures.len();
                    compare(id, &row["request"], request, &mut tally);
                    tally.failures.len() == before
                }
            }
            ("none", Err(error)) => {
                let agreed = !error.is_reraised();
                if !declared {
                    tally.eq(
                        id,
                        "outcome",
                        &"none",
                        &if agreed { "none" } else { "raised" },
                    );
                }
                agreed
            }
            ("raised", Err(error)) => {
                let agreed = error.is_reraised()
                    && row["error"].as_str().expect("a message")
                        == format!("{}: {error}", error.python_exception());
                if !declared {
                    tally.eq(
                        id,
                        "error",
                        &row["error"].as_str().expect("a message").to_owned(),
                        &format!("{}: {error}", error.python_exception()),
                    );
                }
                agreed
            }
            (expected, actual) => {
                if !declared {
                    tally.checked += 1;
                    tally.failures.push(format!(
                        "{id}.outcome: python said {expected}, rust said {}",
                        match actual {
                            Ok(_) => "request".to_owned(),
                            Err(error) => format!("error({error})"),
                        }
                    ));
                }
                false
            }
        };

        if declared && agreed {
            divergent_still_differ.push(id);
        }
    }

    assert!(
        divergent_still_differ.is_empty(),
        "these rows are declared as divergences but now reproduce Python — \
         delete them from DIVERGENCES together with the reason: {divergent_still_differ:?}"
    );

    assert!(
        tally.failures.is_empty(),
        "{}/{} field checks reproduce Python; {} failed:\n{}",
        tally.matched,
        tally.checked,
        tally.failures.len(),
        tally.failures.join("\n")
    );
    // A floor, so a corpus that silently shrank is caught.
    assert!(
        tally.checked >= 500,
        "only {} field checks ran; the corpus must have shrunk",
        tally.checked
    );
    println!(
        "curl parser: {}/{} field checks over {} rows",
        tally.matched,
        tally.checked,
        rows.len()
    );
}

/// The corpus must keep covering every outcome Python can produce, or the
/// "reproduces Python" claim is about one branch only.
#[test]
fn the_corpus_covers_all_three_outcomes() {
    let rows = corpus();
    for outcome in ["request", "none", "raised"] {
        assert!(
            rows.iter()
                .any(|row| row["outcome"].as_str() == Some(outcome)),
            "no row with outcome {outcome}"
        );
    }
}

/// A guard on the divergence table itself: a name in it must exist in the
/// corpus, or the entry is stale and asserts nothing.
#[test]
fn every_declared_divergence_names_a_real_row() {
    let rows = corpus();
    for (name, reason) in DIVERGENCES {
        assert!(
            rows.iter().any(|row| row["id"].as_str() == Some(name)),
            "declared divergence {name} is not a corpus row"
        );
        assert!(!reason.is_empty(), "{name} has no reason");
    }
}

/// The `Unknown/Unsupported curl arguments:` message is rendered exactly like
/// Python's list repr, because a user greps for it.
#[test]
fn the_unknown_arguments_message_matches_pythons_list_repr() {
    let error = parse_curl("not a curl command").expect_err("raises");
    assert_eq!(
        error,
        CurlParseError::UnknownArguments(vec!["curl".to_owned(), "command".to_owned()])
    );
    assert_eq!(
        error.to_string(),
        "Unknown/Unsupported curl arguments: ['curl', 'command']"
    );
}
