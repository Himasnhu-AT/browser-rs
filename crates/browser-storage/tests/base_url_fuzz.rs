//! 4000 randomly assembled URLs, answered by the reference implementation and by this crate.
//!
//! `tests/base_url_parity.rs` checks the cases a person thinks of.
//! `testdata/base_url_fuzz.jsonl` checks the ones nobody does: a seeded
//! generator (`tools/gen_base_url_fuzz.py`) crosses sixteen scheme prefixes,
//! eight userinfo shapes, twenty-seven label families (empty, hyphen-leading,
//! 80 characters long, IDN, punycode, `*`, `!www`, a NUL, a soft hyphen…),
//! twenty-nine suffix tails (public, private, wildcard, exception, unknown),
//! sixteen host literals, eight ports, four trailing-dot runs and eleven
//! path/query/fragment tails, then injects a tab, CR, LF, NUL, `U+001F`, NBSP
//! or zero-width space at a random offset in a fifth of them.
//!
//! Every expectation is whatever `StorageSystemMixin(url)._get_base_url()`
//! returned under CPython 3.14 with `tld` 0.13.2 — the `url.lower()`,
//! `fix_protocol`, `urlsplit`, trie walk and `fld or domain or "default"` chain
//! all included. 1007 of the 4000 resolve to a real registrable domain across
//! 430 distinct namespaces; the rest are the `"default"` bucket, which matters
//! just as much, because a URL that Python files under `"default"` and this
//! crate files under a domain (or the reverse) is a row neither can find again.
//!
//! Regenerate after a `psl` bump:
//!
//! ```text
//! <pyref>/bin/python tools/gen_base_url_fuzz.py testdata/base_url_fuzz.jsonl
//! ```

use std::fs;
use std::path::PathBuf;

use browser_storage::base_url::{normalise_url, registrable_domain};

/// One line of the corpus.
struct Case {
    /// The URL handed to the constructor.
    url: String,
    /// `StorageSystemMixin.__init__`'s `self.url` — the lowercased form.
    self_url: Option<String>,
    /// What `_get_base_url()` answered.
    expected: String,
}

fn corpus() -> Vec<Case> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/base_url_fuzz.jsonl");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("bad line: {e}"));
            Case {
                url: value["url"].as_str().expect("url").to_owned(),
                self_url: value["self_url"].as_str().map(str::to_owned),
                expected: value["expected"].as_str().expect("expected").to_owned(),
            }
        })
        .collect()
}

/// The corpus must not quietly shrink; its size is its whole point.
#[test]
fn the_corpus_is_intact() {
    let cases = corpus();
    assert_eq!(cases.len(), 4000);

    let resolved = cases.iter().filter(|c| c.expected != "default").count();
    assert_eq!(
        resolved, 1007,
        "the mix of resolved vs default answers moved"
    );

    let namespaces: std::collections::BTreeSet<&str> =
        cases.iter().map(|c| c.expected.as_str()).collect();
    assert_eq!(namespaces.len(), 430);
}

/// Every one of the 4000 answers matches, with no allowance for divergence.
///
/// Unlike `tests/psl_sweep.rs` — which sweeps `tld`'s entire suffix file and
/// therefore trips over the forty rules `psl` has since retired — this corpus
/// only names suffixes both lists still agree on, so the expected disagreement
/// count is zero. A `psl` bump that changes any of these is a real regression
/// in namespace compatibility, not a vintage difference.
#[test]
fn registrable_domain_matches_browser-rs_on_every_fuzzed_url() {
    let mut mismatches = Vec::new();

    for case in corpus() {
        let actual = registrable_domain(Some(&case.url));
        if actual != case.expected {
            mismatches.push(format!(
                "{:?}\n     rust: {actual:?}\n   python: {:?}",
                case.url, case.expected,
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of 4000 fuzzed URLs disagree with the reference implementation:\n  {}",
        mismatches.len(),
        mismatches
            .iter()
            .take(50)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  "),
    );
}

/// `normalise_url` is `self.url` for all 4000, junk and all.
///
/// The namespace is derived from `self.url`, so a lowercasing difference on any
/// of these would move rows even when `_get_base_url` happens to agree.
#[test]
fn normalise_url_matches_self_dot_url_on_every_fuzzed_url() {
    for case in corpus() {
        assert_eq!(
            normalise_url(Some(&case.url)),
            case.self_url,
            "self.url differs for {:?}",
            case.url,
        );
    }
}
