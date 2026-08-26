//! Adversarial differential corpus for `base_url::registrable_domain`.
//!
//! `testdata/base_url_parity.jsonl` is produced by `tools/gen_base_url_parity.py`,
//! which drives the reference implementation 0.4.15's
//! own `StorageSystemMixin` (and therefore `tld` 0.13.2 and CPython's
//! `urllib.parse.urlsplit`) over 115 URLs under CPython 3.14. Each line is
//! `{"url": <constructor argument>, "self_url": <StorageSystemMixin.url>,
//! "expected": <_get_base_url()>}`.
//!
//! This complements `base_url_golden.rs`: that corpus checks the happy paths of
//! the decision tree, this one is built to *break* the port — userinfo with an
//! embedded `@`, unbalanced and non-IP brackets, IPv6 zone identifiers,
//! percent-encoded hosts, backslashes, tab/newline injection, empty labels,
//! trailing dots on a host that is itself a suffix, punycode vs. unicode IDN,
//! ports out of range, and every non-hierarchical scheme (`mailto:`, `data:`,
//! `javascript:`, `about:`) that `fix_protocol` has to leave alone.
//!
//! A disagreement here is not cosmetic: `base_url` is the first column of the
//! `UNIQUE (url, identifier)` key, so one wrong answer silently partitions a
//! shared `elements_storage.db`.

use std::fs;
use std::path::PathBuf;

use browser_storage::base_url::{normalise_url, registrable_domain};
use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::SqliteStorage;

struct Case {
    url: Option<String>,
    self_url: Option<String>,
    expected: String,
}

fn corpus() -> Vec<Case> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/base_url_parity.jsonl");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let mut cases = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let case: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{}:{}: bad JSON: {e}", path.display(), lineno + 1));
        let opt = |v: &serde_json::Value| match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => Some(s.clone()),
            other => panic!("{}:{}: unexpected {other}", path.display(), lineno + 1),
        };
        cases.push(Case {
            url: opt(&case["url"]),
            self_url: opt(&case["self_url"]),
            expected: case["expected"]
                .as_str()
                .unwrap_or_else(|| panic!("{}:{}: missing expected", path.display(), lineno + 1))
                .to_owned(),
        });
    }
    assert!(
        cases.len() >= 150,
        "the corpus lost cases: only {} left",
        cases.len(),
    );
    cases
}

#[test]
fn registrable_domain_matches_the_reference_on_every_case() {
    let mut failures = Vec::new();
    for case in corpus() {
        let got = registrable_domain(case.url.as_deref());
        if got != case.expected {
            failures.push(format!(
                "  {:?}\n    rust   = {got:?}\n    python = {:?}",
                case.url, case.expected,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of the golden cases disagree with the reference implementation:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

#[test]
fn normalise_url_matches_storage_system_mixin_dot_url() {
    let mut failures = Vec::new();
    for case in corpus() {
        let got = normalise_url(case.url.as_deref());
        if got != case.self_url {
            failures.push(format!(
                "  {:?}\n    rust   = {got:?}\n    python = {:?}",
                case.url, case.self_url,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} constructor normalisations disagree:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

#[test]
fn the_namespace_reaching_the_url_column_matches() {
    for case in corpus() {
        let storage = SqliteStorage::in_memory(case.url.as_deref()).expect("open");
        assert_eq!(
            StorageSystem::base_url(&storage),
            case.expected,
            "SqliteStorage::in_memory({:?}).base_url()",
            case.url,
        );
    }
}
