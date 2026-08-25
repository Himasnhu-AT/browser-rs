//! Differential test for [`browser_spider::canonicalize::canonicalize_url`] against
//! `w3lib.url.canonicalize_url` running on CPython 3.14 / w3lib 2.x.
//!
//! Two corpora, both generated under `tools/corpus/spider/`:
//!
//! * `canonicalize_url.jsonl` — a hand-picked table of URLs × 4 flag combinations,
//!   covering every branch of the algorithm.
//! * `canonicalize_url_fuzz.jsonl` — 3000 seeded-random URLs assembled from adversarial
//!   component pools (half-formed escapes, raw delimiters, control characters, IDN hosts).
//!
//! Neither is ever trimmed: a row the port cannot reproduce is listed in [`DIVERGENCES`]
//! with a written reason, and this test **fails if such a row ever starts matching**, so
//! the table can never quietly rot.

use std::collections::HashMap;
use std::path::Path;

use browser_spider::canonicalize::canonicalize_url_with;

/// Rows that deliberately differ from Python, keyed by the input URL.
///
/// The value is `(the string this port produces, why it differs)`. Every entry is a
/// documented limitation, never a bug being hidden: see the "Known divergences" section of
/// the `canonicalize` module docs.
const DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "http://straße.example/",
        "http://xn--strae-oqa.example/",
        "IDNA 2003 (CPython's `str.encode(\"idna\")`) maps ß to `ss` during nameprep, so \
         Python yields `http://strasse.example/`. The `idna` crate implements UTS-46 \
         non-transitional processing, where ß is a deviation character that is kept and \
         punycoded. Browsers and the WHATWG URL Standard agree with UTS-46, so matching \
         Python here would mean fingerprinting a different host than the fetcher resolves.",
    ),
    (
        "http://σόλος.example/",
        "http://xn--wxaijb9b.example/",
        "The same deviation-character rule for final sigma (ς): IDNA 2003 folds ς to σ \
         (Python yields `xn--wxaikc6b.example`), UTS-46 non-transitional keeps it. One code \
         point is enough to change the whole punycode label.",
    ),
];

struct Outcome {
    rows: usize,
    matched: usize,
    declared: usize,
    failures: Vec<String>,
    hits: HashMap<String, usize>,
}

fn run_corpus(name: &str, divergences: &HashMap<&str, (&str, &str)>) -> Outcome {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name);
    let corpus = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));

    let mut outcome = Outcome {
        rows: 0,
        matched: 0,
        declared: 0,
        failures: Vec::new(),
        hits: HashMap::new(),
    };

    for (lineno, line) in corpus.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{name} line {}: {e}", lineno + 1));
        let url = row["url"].as_str().expect("url");
        let keep_blank_values = row["keep_blank_values"]
            .as_bool()
            .expect("keep_blank_values");
        let keep_fragments = row["keep_fragments"].as_bool().expect("keep_fragments");

        if let Some(raises) = row.get("raises").and_then(|v| v.as_str()) {
            // w3lib itself raised. Nothing to compare against, but the row stays in the
            // corpus so a regeneration that starts raising somewhere new is visible.
            eprintln!("note: python raised for {url:?}: {raises}");
            continue;
        }
        let expected = row["expected"].as_str().expect("expected");

        outcome.rows += 1;
        let got = canonicalize_url_with(url, keep_blank_values, keep_fragments);

        match divergences.get(url) {
            Some((ours, why)) => {
                outcome.declared += 1;
                *outcome.hits.entry(url.to_owned()).or_default() += 1;
                assert_ne!(
                    got, expected,
                    "declared divergence for {url:?} now MATCHES Python. Delete the row \
                     from DIVERGENCES in this file — the port improved. Reason recorded \
                     was: {why}"
                );
                assert_eq!(
                    got, *ours,
                    "declared divergence for {url:?} changed shape: it no longer produces \
                     the string the table records. Re-check the port before editing the \
                     table. Reason recorded was: {why}"
                );
            }
            None if got == expected => outcome.matched += 1,
            None => outcome.failures.push(format!(
                "  [{name}:{}] {url:?} (keep_blank_values={keep_blank_values}, keep_fragments={keep_fragments})\n    python: {expected:?}\n    rust:   {got:?}",
                lineno + 1
            )),
        }
    }
    outcome
}

#[test]
fn canonicalize_url_matches_w3lib() {
    let divergences: HashMap<&str, (&str, &str)> = DIVERGENCES
        .iter()
        .map(|(url, ours, why)| (*url, (*ours, *why)))
        .collect();

    let table = run_corpus("canonicalize_url.jsonl", &divergences);
    let fuzz = run_corpus("canonicalize_url_fuzz.jsonl", &divergences);

    for (url, _, why) in DIVERGENCES {
        assert!(
            table.hits.contains_key(*url) || fuzz.hits.contains_key(*url),
            "DIVERGENCES lists {url:?}, which is not in either corpus any more. Never \
             shrink a corpus to raise the pass rate — regenerate it, or delete the stale \
             row if the case is genuinely gone. Reason recorded was: {why}"
        );
    }

    let failures: Vec<&String> = table.failures.iter().chain(fuzz.failures.iter()).collect();
    assert!(
        failures.is_empty(),
        "{} of {} canonicalize_url rows differ from w3lib:\n{}",
        failures.len(),
        table.rows + fuzz.rows,
        failures
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Guard against a future edit quietly emptying either file.
    assert!(
        table.rows > 400,
        "hand-written corpus shrank to {} rows",
        table.rows
    );
    assert!(fuzz.rows > 2900, "fuzz corpus shrank to {} rows", fuzz.rows);

    eprintln!(
        "canonicalize_url: {}/{} exact ({} declared divergence rows)",
        table.matched + fuzz.matched,
        table.rows + fuzz.rows,
        table.declared + fuzz.declared
    );
}
