//! `parser.md` §9 pins one performance budget on the selection layer:
//!
//! > Performance: parsing 5000 nested `div.item` + `css(".item")` must finish
//! > in < 0.5 s.
//!
//! Python's number is for CPython running an optimised libxml2. These tests run
//! in a **debug** build, so the budget here is deliberately loose — they exist
//! to catch an accidental O(n²), not to certify a wall-clock figure.

use std::time::{Duration, Instant};

use browser_dom::Document;
use browser_selectors::parse::parse_selector;

/// Generous because `cargo test` builds without optimisations.
const BUDGET: Duration = Duration::from_secs(3);

fn nested(depth: usize) -> String {
    let mut html = String::with_capacity(depth * 24);
    for i in 0..depth {
        html.push_str(&format!("<div class=\"item\" data-i=\"{i}\">"));
    }
    html.push_str("leaf");
    for _ in 0..depth {
        html.push_str("</div>");
    }
    html
}

fn flat(count: usize) -> String {
    let mut html = String::with_capacity(count * 40);
    html.push_str("<div id=\"root\">");
    for i in 0..count {
        html.push_str(&format!("<p class=\"item\" data-i=\"{i}\">t{i}</p>"));
    }
    html.push_str("</div>");
    html
}

#[test]
fn five_thousand_nested_items() {
    let doc = Document::parse(&nested(5000));
    let group = parse_selector(".item").expect("parses");

    let start = Instant::now();
    let found = group.select(doc.root());
    let elapsed = start.elapsed();

    assert_eq!(found.len(), 5000);
    assert!(
        elapsed < BUDGET,
        "`.item` over 5000 nested divs took {elapsed:?}"
    );
}

#[test]
fn descendant_combinator_over_a_deep_tree_is_not_quadratic() {
    // `div div` has to walk ancestors for every candidate; on a 5000-deep tree
    // a naive implementation is O(n²). The matcher short-circuits at the first
    // ancestor that matches, so this stays linear.
    let doc = Document::parse(&nested(2000));
    let group = parse_selector("div div").expect("parses");

    let start = Instant::now();
    let found = group.select(doc.root());
    let elapsed = start.elapsed();

    assert_eq!(found.len(), 1999);
    assert!(
        elapsed < BUDGET,
        "`div div` over 2000 nested divs took {elapsed:?}"
    );
}

#[test]
fn text_and_attr_tails_stay_linear() {
    let doc = Document::parse(&flat(5000));
    let root = doc.root();

    let start = Instant::now();
    let texts = parse_selector("p.item::text").expect("parses").select(root);
    let attrs = parse_selector("p.item::attr(data-i)")
        .expect("parses")
        .select(root);
    let elapsed = start.elapsed();

    assert_eq!(texts.len(), 5000);
    assert_eq!(attrs.len(), 5000);
    assert_eq!(texts[0].as_str(), Some("t0"));
    assert_eq!(attrs[4999].as_str(), Some("4999"));
    assert!(
        elapsed < BUDGET,
        "tails over 5000 elements took {elapsed:?}"
    );
}

#[test]
fn a_large_group_does_not_multiply_the_walk() {
    let doc = Document::parse(&flat(2000));
    let selector = (0..40)
        .map(|i| format!("p[data-i=\"{i}\"]"))
        .collect::<Vec<_>>()
        .join(", ");
    let group = parse_selector(&selector).expect("parses");
    assert_eq!(group.len(), 40);

    let start = Instant::now();
    let found = group.select(doc.root());
    let elapsed = start.elapsed();

    assert_eq!(found.len(), 40);
    assert!(elapsed < BUDGET, "a 40-part group took {elapsed:?}");
}

#[test]
fn scoped_selection_only_walks_the_subtree() {
    let doc = Document::parse(&flat(5000));
    let root_div = doc
        .elements()
        .find(|e| e.element_id() == Some("root"))
        .expect("#root");
    let leaf = root_div.first_element_child().expect("first <p>");

    let group = parse_selector("p").expect("parses");
    // `descendant-or-self::p` from a leaf `<p>` is just that `<p>`.
    assert_eq!(group.select(leaf).len(), 1);
    assert_eq!(group.select(root_div).len(), 5000);
}

/// `browser-parser` caches compiled selectors and the spider shares them
/// across `rayon` workers, so the compiled form has to cross threads.
#[test]
fn a_compiled_selector_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<browser_selectors::parse::SelectorGroup>();
    assert_send_sync::<browser_selectors::parse::SubSelector>();
    assert_send_sync::<browser_selectors::error::SelectorError>();

    let group = std::sync::Arc::new(parse_selector("div.item, p::text").expect("parses"));
    let clone = std::sync::Arc::clone(&group);
    let handle = std::thread::spawn(move || {
        let doc = Document::parse("<div class='item'>x</div><p>y</p>");
        clone.select(doc.root()).len()
    });
    assert_eq!(handle.join().expect("thread"), 2);
}
