//! The Public Suffix List's eight `!exception` rules, and the namespace each
//! one's *parent* belongs to.
//!
//! This pins the one place where `tld` and `psl` disagree by construction
//! rather than by list vintage. `tld` builds its trie by walking a rule's
//! labels in reverse and marking the node it stops on as a leaf; an exception
//! rule stops one label early —
//!
//! ```python
//! for part in reversed(tld.split(".")):
//!     if part.startswith("!"):
//!         node.exception = part[1:]
//!         break
//!     ...
//! node.leaf = True
//! ```
//!
//! — so `!www.ck` marks **`ck`** a leaf and `!city.kawasaki.jp` marks
//! **`kawasaki.jp`**. `psl` implements the PSL algorithm proper, under which
//! `psl::suffix(b"ck")` is `is_known() == false` and `kawasaki.jp` is just a
//! `.jp` domain. Left alone, a page on `https://ck` would be filed under
//! `default` by this crate and under `ck` by the reference implementation: two namespaces for one
//! site, and neither implementation would find the other's rows.
//!
//! `testdata/psl_exception_rules.jsonl` is generated from the suffix file `tld`
//! 0.13.2 bundles, with every expectation taken from the reference implementation's own mixin
//! (`tools/gen_exception_rules.py`). Should upstream add a ninth exception
//! rule, regenerating the file fails this test — which is the point: the Rust
//! constant has to grow with it.

use std::fs;
use std::path::PathBuf;

use browser_storage::base_url::registrable_domain;

/// One `!rule` line, with the five host shapes that surround it.
struct Rule {
    rule: String,
    exception_label: String,
    parent: String,
    parent_base_url: String,
    parent_with_dot_base_url: String,
    exception_base_url: String,
    child_base_url: String,
    deep_child_base_url: String,
}

fn rules() -> Vec<Rule> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/psl_exception_rules.jsonl");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("bad line: {e}"));
            let string = |key: &str| value[key].as_str().expect(key).to_owned();
            Rule {
                rule: string("rule"),
                exception_label: string("exception_label"),
                parent: string("parent"),
                parent_base_url: string("parent_base_url"),
                parent_with_dot_base_url: string("parent_with_dot_base_url"),
                exception_base_url: string("exception_base_url"),
                child_base_url: string("child_base_url"),
                deep_child_base_url: string("deep_child_base_url"),
            }
        })
        .collect()
}

/// The list is the eight rules the PSL has carried for a decade.
#[test]
fn the_exception_rules_are_the_eight_we_know() {
    let rules = rules();
    let names: Vec<&str> = rules.iter().map(|rule| rule.rule.as_str()).collect();
    assert_eq!(
        names,
        [
            "!www.ck",
            "!city.kawasaki.jp",
            "!city.kitakyushu.jp",
            "!city.kobe.jp",
            "!city.nagoya.jp",
            "!city.sapporo.jp",
            "!city.sendai.jp",
            "!city.yokohama.jp",
        ],
        "upstream changed its exception rules; \
         `EXCEPTION_RULE_PARENTS` in src/base_url.rs must be updated to match",
    );
}

/// Each parent, its exception, an ordinary child and a grandchild all agree
/// with the reference implementation.
#[test]
fn every_shape_around_an_exception_rule_matches() {
    for rule in rules() {
        let answer = |host: &str| registrable_domain(Some(&format!("https://{host}")));

        // The parent is a suffix in its own right, so `fld == tld == host`.
        assert_eq!(
            answer(&rule.parent),
            rule.parent_base_url,
            "parent of {}",
            rule.rule,
        );
        // …and the `hostname == tld` branch reads the *un*-stripped hostname,
        // so a trailing dot survives into the namespace key.
        assert_eq!(
            answer(&format!("{}.", rule.parent)),
            rule.parent_with_dot_base_url,
            "dotted parent of {}",
            rule.rule,
        );
        // The exception host itself is a registrable domain.
        assert_eq!(
            answer(&format!("{}.{}", rule.exception_label, rule.parent)),
            rule.exception_base_url,
            "exception host of {}",
            rule.rule,
        );
        // A non-exception child is covered by the wildcard, so it *is* the
        // suffix, and a grandchild is registered under it.
        assert_eq!(
            answer(&format!("zzprobe.{}", rule.parent)),
            rule.child_base_url,
            "child of {}",
            rule.rule,
        );
        assert_eq!(
            answer(&format!("a.zzprobe.{}", rule.parent)),
            rule.deep_child_base_url,
            "grandchild of {}",
            rule.rule,
        );
    }
}

/// The reason the constant exists: `psl` really does call these parents
/// unknown, and really does resolve their children through the wildcard.
///
/// If a future `psl` starts reporting `ck` as a known suffix, this test fails
/// and the constant can be deleted rather than quietly doing nothing.
#[test]
fn psl_still_disagrees_about_every_parent() {
    for rule in rules() {
        let suffix = psl::suffix(rule.parent.as_bytes())
            .unwrap_or_else(|| panic!("no suffix at all for {}", rule.parent));
        assert!(
            !suffix.is_known() || suffix.as_bytes() != rule.parent.as_bytes(),
            "psl now treats {} as a suffix of its own; \
             drop it from EXCEPTION_RULE_PARENTS",
            rule.parent,
        );

        // The wildcard side, which `psl` and `tld` do agree on.
        let child = format!("zzprobe.{}", rule.parent);
        let child_suffix =
            psl::suffix(child.as_bytes()).unwrap_or_else(|| panic!("no suffix for {child}"));
        assert!(child_suffix.is_known(), "{child}");
        assert_eq!(child_suffix.as_bytes(), child.as_bytes(), "{child}");
    }
}
