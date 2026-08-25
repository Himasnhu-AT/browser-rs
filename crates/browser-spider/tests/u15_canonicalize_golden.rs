//! Second, independent differential test for
//! [`browser_spider::canonicalize::canonicalize_url_with`] against `w3lib.url`.
//!
//! `tests/canonicalize_golden.rs` already replays the corpus written while the module was
//! being built. This one replays `testdata/u15_canonicalize.jsonl`, a corpus written
//! afterwards from the spec's category list without looking at the first — 147 URLs, each
//! under all four flag combinations, plus collision *groups* whose members must all
//! canonicalise to the same string (which a port that simply echoed its input would fail
//! even while matching row-by-row on the easy rows).
//!
//! Rows are never removed; anything unreproducible belongs in [`DIVERGENCES`], which fails
//! if it ever starts matching.

mod u15_support;

use std::collections::{BTreeMap, HashMap};

use browser_spider::canonicalize::{canonicalize_url_with, netloc_of, urlparse};
use browser_spider::request::Request;
use serde_json::Value;

use u15_support::read_corpus;

/// Rows that deliberately differ from Python, keyed by the input URL.
///
/// The value is `(what this port produces, why)`. Both entries are the IDNA 2003 vs UTS-46
/// deviation-character split already recorded in the `canonicalize` module docs and in
/// `docs/DIVERGENCES.md`; they are repeated here because this corpus reaches them through
/// different URLs than the first one does.
const DIVERGENCES: &[(&str, &str, &str)] = &[];

/// URLs where `w3lib` itself raised, mapped to what this port returns instead.
///
/// Fingerprinting must not be able to fail — a `Request` that cannot be hashed cannot be
/// scheduled — so the port canonicalises these best-effort. The rows stay in the corpus and
/// the port's answer is pinned here, so a change on either side is visible rather than
/// silently skipped. (Declared in `docs/DIVERGENCES.md`.)
const RAISING_ROWS: &[(&str, &str)] = &[("http://[not-an-ip]/", "http://[not-an-ip]/")];

/// `(group, keep_blank_values, keep_fragments)` -> our canonical form -> the members that
/// produced it, each tagged with the bucket id w3lib put it in.
type Groups = BTreeMap<(String, bool, bool), BTreeMap<String, Vec<(u64, String)>>>;

#[test]
fn canonicalize_url_matches_w3lib_second_corpus() {
    let corpus = read_corpus("u15_canonicalize.jsonl");
    let divergences: HashMap<&str, (&str, &str)> = DIVERGENCES
        .iter()
        .map(|(url, ours, why)| (*url, (*ours, *why)))
        .collect();
    let mut seen_divergences: HashMap<String, usize> = HashMap::new();

    let mut rows = 0usize;
    let mut matched = 0usize;
    let mut raised = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut split_failures: Vec<String> = Vec::new();
    let mut split_checks = 0usize;
    let mut groups: Groups = BTreeMap::new();

    for (index, row) in corpus.iter().enumerate() {
        let url = row["url"].as_str().expect("url");
        let keep_blank_values = row["keep_blank_values"]
            .as_bool()
            .expect("keep_blank_values");
        let keep_fragments = row["keep_fragments"].as_bool().expect("keep_fragments");
        let got = canonicalize_url_with(url, keep_blank_values, keep_fragments);

        if let Some(group) = row.get("group").and_then(Value::as_str) {
            if let Some(bucket) = row.get("group_bucket").and_then(Value::as_u64) {
                groups
                    .entry((group.to_owned(), keep_blank_values, keep_fragments))
                    .or_default()
                    .entry(got.clone())
                    .or_default()
                    .push((bucket, url.to_owned()));
            }
        }

        // `Request::domain` is Python's `urlparse(url).netloc`; the whole six-field split
        // is pinned because the port reimplements `urlsplit` rather than binding to it.
        if let Some(parts) = row.get("parts").and_then(Value::as_object) {
            split_checks += 1;
            let got = urlparse(url);
            let mine = [
                ("scheme", got.scheme.as_str()),
                ("netloc", got.netloc.as_str()),
                ("path", got.path.as_str()),
                ("params", got.params.as_str()),
                ("query", got.query.as_str()),
                ("fragment", got.fragment.as_str()),
            ];
            for (field, value) in mine {
                let expected = parts[field].as_str().expect("part");
                if value != expected {
                    split_failures.push(format!(
                        "  urlparse({url:?}).{field}: python {expected:?} != rust {value:?}"
                    ));
                }
            }
            let expected_netloc = row["netloc"].as_str().expect("netloc");
            let got_netloc = netloc_of(url);
            if got_netloc != expected_netloc {
                split_failures.push(format!(
                    "  netloc_of({url:?}): python {expected_netloc:?} != rust {got_netloc:?}"
                ));
            }
            let domain = Request::new(url).domain().to_owned();
            if domain != expected_netloc {
                split_failures.push(format!(
                    "  Request::domain({url:?}): python {expected_netloc:?} != rust {domain:?}"
                ));
            }
        }

        if let Some(raises) = row.get("raises").and_then(Value::as_str) {
            // w3lib itself raised. There is nothing to compare, but the row stays so a
            // regeneration that starts raising somewhere new is visible in the count.
            raised += 1;
            let pinned = RAISING_ROWS
                .iter()
                .find(|(candidate, _)| *candidate == url)
                .unwrap_or_else(|| {
                    panic!(
                        "w3lib raised for {url:?} ({raises}) and RAISING_ROWS does not pin \
                         what this port returns. Add it — never skip a corpus row."
                    )
                });
            assert_eq!(
                got, pinned.1,
                "w3lib raises for {url:?} ({raises}); the port used to answer {:?} and now \
                 answers {got:?}",
                pinned.1
            );
            continue;
        }
        let expected = row["expected"].as_str().expect("expected");
        rows += 1;

        match divergences.get(url) {
            Some((ours, why)) => {
                *seen_divergences.entry(url.to_owned()).or_default() += 1;
                assert_ne!(
                    got, expected,
                    "declared divergence for {url:?} now MATCHES Python. Delete the row \
                     from DIVERGENCES — the port improved. Reason recorded was: {why}"
                );
                assert_eq!(
                    got, *ours,
                    "declared divergence for {url:?} changed shape. Re-check the port \
                     before editing the table. Reason recorded was: {why}"
                );
            }
            None if got == expected => matched += 1,
            None => failures.push(format!(
                "  [line {}] {url:?} (keep_blank_values={keep_blank_values}, keep_fragments={keep_fragments})\n    python: {expected:?}\n    rust:   {got:?}",
                index + 1
            )),
        }
    }

    // Structural check: two URLs in a group must collide in Rust exactly when they collide
    // in Python. This catches a port that matched every row by echoing its input.
    for ((group, kbv, kf), forms) in &groups {
        for (form, members) in forms {
            let first = members[0].0;
            assert!(
                members.iter().all(|(bucket, _)| *bucket == first),
                "group {group:?} (keep_blank_values={kbv}, keep_fragments={kf}): the port \
                 canonicalises {members:?} to the same string {form:?}, but w3lib puts them \
                 in different buckets"
            );
        }
        let our_buckets = forms.len();
        let python_buckets = forms
            .values()
            .flat_map(|members| members.iter().map(|(bucket, _)| *bucket))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert_eq!(
            our_buckets, python_buckets,
            "group {group:?} (keep_blank_values={kbv}, keep_fragments={kf}): the port splits \
             it into {our_buckets} canonical forms, w3lib into {python_buckets}: {forms:?}"
        );
    }

    for (url, _, why) in DIVERGENCES {
        assert!(
            seen_divergences.contains_key(*url),
            "DIVERGENCES lists {url:?}, which is no longer in the corpus. Never shrink a \
             corpus to raise the pass rate. Reason recorded was: {why}"
        );
    }
    // De-duplicate: every URL appears under four flag combinations, but its split does not
    // depend on them, so a single broken URL would otherwise be reported four times.
    split_failures.sort();
    split_failures.dedup();
    assert!(
        split_failures.is_empty(),
        "{} urlparse/netloc results differ from CPython:\n{}",
        split_failures.len(),
        split_failures.join("\n")
    );
    assert!(
        failures.is_empty(),
        "{} of {rows} canonicalize_url rows differ from w3lib:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        corpus.len() >= 300,
        "the corpus shrank to {} rows (the unit requires at least 300 URLs)",
        corpus.len()
    );
    eprintln!(
        "canonicalize_url (u15 corpus): {matched}/{rows} exact, {} declared divergences, \
         {raised} rows where w3lib itself raised; {split_checks} urlparse/netloc/domain \
         triples match CPython",
        seen_divergences.values().sum::<usize>()
    );
}
