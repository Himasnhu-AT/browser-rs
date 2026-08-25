//! Adaptive element relocation: the fingerprint, the `rayon`-parallel
//! `relocate`, and the exact warning strings `css`/`xpath` emit.
//!
//! `parser.md` §4, `ARCHITECTURE.md` §4. Three pieces live in three crates: the
//! fingerprint *shape* and the scorer are `browser-core`'s
//! ([`ElementFingerprint`], [`similarity_score`](browser_core::fingerprint::similarity_score)),
//! persistence is
//! `browser-storage`'s `StorageSystem`, and the tree walking is here.
//!
//! # The fingerprint
//!
//! [`element_to_fingerprint`] is `_StorageTools.element_to_dict` verbatim,
//! including its two asymmetries:
//!
//! * `attributes` is *cleaned* — values stripped, empty and whitespace-only
//!   values dropped — while `parent_attribs` is **raw**;
//! * `siblings` is taken from lxml's `iterchildren()` and therefore **includes
//!   comment tags**, while `children` filters them out.
//!
//! Reproducing both is what makes a `.db` written by Python score identically
//! here, and vice versa.
//!
//! # The warnings
//!
//! Python logs, never raises, when an adaptive argument is passed to a document
//! that has the feature switched off. The two strings are pinned by
//! `parser.md` §9 and are emitted through `tracing::warn!`.

use browser_core::fingerprint::{AttributeDict, ElementFingerprint};
use browser_dom::NodeRef;

/// Logged when `auto_save=true` reaches a document with adaptive disabled.
pub const AUTO_SAVE_IGNORED: &str = "Argument `auto_save` will be ignored because `adaptive` wasn't enabled on initialization. Check docs for more info.";

/// Logged when `adaptive=true` reaches a document with adaptive disabled.
pub const ADAPTIVE_IGNORED: &str = "Argument `adaptive` will be ignored because `adaptive` wasn't enabled on initialization. Check docs for more info.";

/// Below this many candidates, scoring stays on the calling thread: `rayon`'s
/// fork/join costs more than the work it distributes.
const PARALLEL_THRESHOLD: usize = 64;

/// `_StorageTools.__clean_attributes`: values stripped, empty and
/// whitespace-only values dropped, insertion order preserved.
#[must_use]
pub fn clean_attributes(element: NodeRef<'_>) -> AttributeDict {
    element
        .attrs()
        .iter()
        .filter_map(|(name, value)| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| (name.to_owned(), trimmed.to_owned()))
        })
        .collect()
}

/// `element.text.strip() if element.text else None`.
///
/// Note the Python shape: a whitespace-only `.text` is *truthy*, so it becomes
/// `Some("")`, not `None`. Only a genuinely absent `.text` is `None`.
fn stripped_text(element: NodeRef<'_>) -> Option<String> {
    element.text().map(|t| t.trim().to_owned())
}

/// `_get_element_path`: tag names from the document root down to and including
/// the element itself, so `path.len() == depth + 1`.
#[must_use]
pub fn element_path(element: NodeRef<'_>) -> Vec<String> {
    let mut path: Vec<String> = element.ancestors().map(|a| a.tag().to_owned()).collect();
    path.reverse();
    path.push(element.tag().to_owned());
    path
}

/// `_StorageTools.element_to_dict` — the on-disk record.
///
/// ```
/// use browser_dom::Document;
/// use browser_parser::adaptive::element_to_fingerprint;
///
/// let doc = Document::parse(r#"<div id="wrap"><p class=" price "> $9 <b>!</b></p></div>"#);
/// let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
/// let fp = element_to_fingerprint(p);
///
/// assert_eq!(fp.tag, "p");
/// assert_eq!(fp.attributes["class"], "price");     // cleaned + stripped
/// assert_eq!(fp.text.as_deref(), Some("$9"));
/// assert_eq!(fp.path, ["html", "body", "div", "p"]);
/// assert_eq!(fp.parent_name.as_deref(), Some("div"));
/// assert_eq!(fp.children.as_deref(), Some(&["b".to_owned()][..]));
/// assert_eq!(fp.siblings, None);                   // absent, not empty
/// ```
#[must_use]
pub fn element_to_fingerprint(element: NodeRef<'_>) -> ElementFingerprint {
    let mut fingerprint = ElementFingerprint {
        tag: element.tag().to_owned(),
        attributes: clean_attributes(element),
        text: stripped_text(element),
        path: element_path(element),
        ..ElementFingerprint::default()
    };

    if let Some(parent) = element.parent_element() {
        fingerprint.parent_name = Some(parent.tag().to_owned());
        // NOT cleaned and NOT stripped — the asymmetry is Python's.
        fingerprint.parent_attribs = Some(parent.attrs().to_index_map());
        fingerprint.parent_text = stripped_text(parent);

        // `siblings` goes through lxml's `iterchildren()`, which keeps
        // comments; `children` below filters them (`html_forbidden`).
        let siblings: Vec<String> = parent
            .iterchildren()
            .filter(|c| *c != element)
            .map(|c| c.tag().to_owned())
            .collect();
        if !siblings.is_empty() {
            fingerprint.siblings = Some(siblings);
        }
    }

    let children: Vec<String> = element
        .iterchildren()
        .filter(|c| !c.is_comment())
        .map(|c| c.tag().to_owned())
        .collect();
    if !children.is_empty() {
        fingerprint.children = Some(children);
    }

    fingerprint
}

/// One scored candidate, in document order.
#[derive(Clone, Copy, Debug)]
struct Scored<'a> {
    score: f64,
    node: NodeRef<'a>,
}

/// The core of `Selector.relocate`, over an explicit candidate list.
///
/// Returns **every** node tied at the highest score when that score clears
/// `percentage`, in document order; an empty vector otherwise. The loop never
/// short-circuits — not even at 100 % — because ties must all be collected
/// (`parser.md` §4.2).
///
/// `candidates` is scored with `rayon` once it is long enough to pay for the
/// fork/join. Parallelism must not change tie collection, hence
/// collect-then-group rather than a running maximum.
#[must_use]
pub fn relocate_among<'a>(
    original: &ElementFingerprint,
    candidates: Vec<NodeRef<'a>>,
    percentage: u8,
) -> Vec<NodeRef<'a>> {
    if candidates.is_empty() {
        // Python's `if score_table:` guard: an empty table logs nothing.
        return Vec::new();
    }

    let scored: Vec<Scored<'a>> = if candidates.len() >= PARALLEL_THRESHOLD {
        use rayon::prelude::*;
        candidates
            .par_iter()
            .map(|node| Scored {
                score: score_candidate(original, *node),
                node: *node,
            })
            .collect()
    } else {
        candidates
            .iter()
            .map(|node| Scored {
                score: score_candidate(original, *node),
                node: *node,
            })
            .collect()
    };

    // `max(score_table.keys())` — every score is finite, so a plain fold is
    // enough and `total_cmp` keeps clippy happy about float ordering.
    let highest = scored
        .iter()
        .map(|s| s.score)
        .fold(f64::NEG_INFINITY, f64::max);

    if highest >= f64::from(percentage) {
        if tracing::enabled!(tracing::Level::DEBUG) {
            log_top_five(&scored, highest);
        }
        return scored
            .into_iter()
            .filter(|s| s.score == highest)
            .map(|s| s.node)
            .collect();
    }

    tracing::warn!("{}", below_threshold_warning(percentage, highest),);
    Vec::new()
}

/// `log.warning(...)` from `relocate`, as one string.
///
/// Exposed so tests can pin the sentence without capturing a `tracing`
/// subscriber. `top_score` is formatted the way Python formats a float, so an
/// integral score reads `100.0`, not `100`.
#[must_use]
pub fn below_threshold_warning(percentage: u8, top_score: f64) -> String {
    format!(
        "Adaptive relocation found no element above the {percentage}% threshold (top score: {}%). \
         Lower `percentage` if this is the right element.",
        python_float(top_score)
    )
}

/// Python's `str(float)`: shortest round-trip, but always with a decimal point.
///
/// Rust prints `100f64` as `100`; Python prints `100.0`. The difference is
/// visible in the relocation warning, which the spec pins verbatim.
fn python_float(value: f64) -> String {
    let rendered = format!("{value}");
    if rendered.contains(['.', 'e', 'E']) || !rendered.bytes().any(|b| b.is_ascii_digit()) {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

fn score_candidate(original: &ElementFingerprint, candidate: NodeRef<'_>) -> f64 {
    browser_core::fingerprint::similarity_score(original, &element_to_fingerprint(candidate))
}

/// `relocate`'s DEBUG dump: the five best distinct scores, best first.
fn log_top_five(scored: &[Scored<'_>], highest: f64) {
    tracing::debug!("Highest probability was {}%", python_float(highest));
    tracing::debug!("Top 5 best matching elements are: ");

    let mut distinct: Vec<f64> = scored.iter().map(|s| s.score).collect();
    distinct.sort_by(|a, b| b.total_cmp(a));
    distinct.dedup_by(|a, b| a == b);

    for percent in distinct.into_iter().take(5) {
        let tags: Vec<&str> = scored
            .iter()
            .filter(|s| s.score == percent)
            .map(|s| s.node.tag())
            .collect();
        tracing::debug!("{} -> {:?}", python_float(percent), tags);
    }
}

#[cfg(test)]
mod tests {
    use browser_dom::{Document, ParseOptions};

    use super::*;

    #[test]
    fn python_float_formatting_keeps_the_decimal_point() {
        assert_eq!(python_float(62.63), "62.63");
        assert_eq!(python_float(100.0), "100.0");
        assert_eq!(python_float(0.0), "0.0");
        assert_eq!(python_float(4.0), "4.0");
    }

    #[test]
    fn the_below_threshold_warning_is_verbatim() {
        assert_eq!(
            below_threshold_warning(95, 62.63),
            "Adaptive relocation found no element above the 95% threshold (top score: 62.63%). \
             Lower `percentage` if this is the right element."
        );
    }

    #[test]
    fn the_two_ignored_argument_warnings_are_verbatim() {
        assert_eq!(
            AUTO_SAVE_IGNORED,
            "Argument `auto_save` will be ignored because `adaptive` wasn't enabled on \
             initialization. Check docs for more info."
        );
        assert_eq!(
            ADAPTIVE_IGNORED,
            "Argument `adaptive` will be ignored because `adaptive` wasn't enabled on \
             initialization. Check docs for more info."
        );
    }

    #[test]
    fn siblings_keep_comments_but_children_drop_them() {
        let doc = Document::parse_with(
            "<div><p>a</p><!--c--><span>b</span></div>",
            &ParseOptions::new().keep_comments(true),
        );
        let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
        let fp = element_to_fingerprint(p);
        assert_eq!(
            fp.siblings.as_deref(),
            Some(&["#comment".to_owned(), "span".to_owned()][..]),
            "`siblings` goes through iterchildren() and keeps comments"
        );

        let div = doc.elements().find(|e| e.tag() == "div").expect("a <div>");
        let fp = element_to_fingerprint(div);
        assert_eq!(
            fp.children.as_deref(),
            Some(&["p".to_owned(), "span".to_owned()][..]),
            "`children` filters html_forbidden"
        );
    }

    #[test]
    fn whitespace_only_text_becomes_some_empty_not_none() {
        let doc = Document::parse("<p> <b>x</b></p>");
        let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
        // `remove_blank_text` keeps the space (p admits character data), so
        // `.text` is truthy in Python and strips to "".
        assert_eq!(element_to_fingerprint(p).text.as_deref(), Some(""));

        let doc = Document::parse("<p><b>x</b></p>");
        let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
        assert_eq!(element_to_fingerprint(p).text, None);
    }

    #[test]
    fn parent_attribs_are_raw_while_attributes_are_cleaned() {
        let doc = Document::parse(r#"<div class="  wrap  " data-x=" "><p id=" q "></p></div>"#);
        let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
        let fp = element_to_fingerprint(p);
        assert_eq!(fp.attributes["id"], "q");
        let parent = fp.parent_attribs.expect("a parent");
        assert_eq!(parent["class"], "  wrap  ");
        assert_eq!(parent["data-x"], " ", "empty values survive on the parent");
    }

    // -----------------------------------------------------------------
    // The full round trip: parse -> select -> auto_save -> (redesign) ->
    // retrieve -> relocate, through a real storage backend.
    //
    // Every expectation here was captured from `the reference implementation` driving the
    // same two pages (`testdata/pages/redesign_{old,new}.html`).
    // -----------------------------------------------------------------

    #[cfg(feature = "adaptive")]
    mod round_trip {
        use std::sync::Arc;

        use browser_storage::{SqliteStorage, StorageSystem};

        use crate::config::{CssOpts, DocumentConfig};
        use crate::document::Document;
        use crate::error::ParseError;

        const OLD: &str = include_str!("../testdata/pages/redesign_old.html");
        const NEW: &str = include_str!("../testdata/pages/redesign_new.html");

        fn storage() -> Arc<dyn StorageSystem> {
            Arc::new(SqliteStorage::in_memory(Some("https://example.com")).expect("opens"))
        }

        fn adaptive(html: &str, storage: Arc<dyn StorageSystem>) -> Document {
            Document::parse_with(
                html,
                &DocumentConfig::new()
                    .url("https://example.com")
                    .storage(storage),
            )
        }

        /// `test_element_relocation`, end to end.
        #[test]
        fn a_comma_group_saves_one_fingerprint_per_sub_selector_and_relocates() {
            let storage = storage();
            let old = adaptive(OLD, Arc::clone(&storage));

            let saved = old
                .css_with("#p1, #p2", &CssOpts::new().auto_save(true))
                .expect("selects");
            assert_eq!(saved.attrs("id"), ["p1", "p2"]);

            // One row per canonical sub-selector; nothing under the group text.
            assert!(storage.retrieve("#p1").expect("reads").is_some());
            assert!(storage.retrieve("#p2").expect("reads").is_some());
            assert_eq!(storage.retrieve("#p1, #p2").expect("reads"), None);

            // The redesign kills the selector outright.
            let new = adaptive(NEW, Arc::clone(&storage));
            assert!(new.css("#p1").expect("selects").is_empty());

            let relocated = new
                .css_with("#p1", &CssOpts::new().adaptive(true))
                .expect("relocates");
            assert_eq!(relocated.len(), 1);
            assert_eq!(relocated[0].tag(), "article");
            assert_eq!(relocated[0].attr("data-id"), Some("p1"));
            assert!(relocated[0].has_class("new-class"));
            assert_eq!(
                relocated[0]
                    .css(".new-description")
                    .expect("selects")
                    .texts(),
                ["Description 1"]
            );
        }

        /// `test_relocation_auto_save_no_match_above_threshold`: an unreachable
        /// threshold returns empty and must not index into an empty list.
        #[test]
        fn a_threshold_nothing_clears_returns_empty_without_panicking() {
            let storage = storage();
            let old = adaptive(OLD, Arc::clone(&storage));
            old.css_with("#p1", &CssOpts::new().auto_save(true))
                .expect("selects");

            let new = adaptive(NEW, storage);
            let found = new
                .css_with(
                    "#p1",
                    &CssOpts::new().adaptive(true).auto_save(true).percentage(95),
                )
                .expect("no panic");
            assert!(found.is_empty(), "the best score is 74.91 %");
        }

        #[test]
        fn xpath_takes_the_same_adaptive_branch_as_css() {
            let storage = storage();
            let old = adaptive(OLD, Arc::clone(&storage));
            old.xpath_with(
                "//article[@id='p2']",
                &CssOpts::new().identifier("second").auto_save(true),
                &browser_xpath::eval::Variables::new(),
            )
            .expect("selects");

            let new = adaptive(NEW, storage);
            let relocated = new
                .xpath_with(
                    "//article[@id='p2']",
                    &CssOpts::new().identifier("second").adaptive(true),
                    &browser_xpath::eval::Variables::new(),
                )
                .expect("relocates");
            assert_eq!(relocated.len(), 1);
            assert_eq!(relocated[0].attr("data-id"), Some("p2"));
        }

        #[test]
        fn saving_a_text_node_stores_its_owning_element() {
            let storage = storage();
            let doc = adaptive(OLD, Arc::clone(&storage));
            let text = doc.css("h3::text").expect("selects")[0].clone();
            assert!(text.is_text_node());
            doc.save(&text, "heading").expect("saves");
            assert_eq!(
                storage
                    .retrieve("heading")
                    .expect("reads")
                    .expect("a row")
                    .tag,
                "h3"
            );
        }

        #[test]
        fn an_unsaved_identifier_relocates_to_nothing_rather_than_erroring() {
            let doc = adaptive(NEW, storage());
            assert!(doc
                .css_with("#never-seen", &CssOpts::new().adaptive(true))
                .expect("no error")
                .is_empty());
        }

        #[test]
        fn adaptive_arguments_are_ignored_when_the_feature_is_off() {
            // No `.storage(..)`, so `adaptive` stays false — Python logs a
            // warning and carries on rather than raising.
            let doc = Document::parse(OLD);
            assert!(!doc.adaptive_enabled());
            assert!(doc
                .css_with("#nope", &CssOpts::new().adaptive(true))
                .expect("warns, does not raise")
                .is_empty());
            assert!(doc
                .css_with("#nope", &CssOpts::new().auto_save(true))
                .expect("warns, does not raise")
                .is_empty());
            // A *successful* selection with auto_save also only warns.
            assert_eq!(
                doc.css_with("#p1", &CssOpts::new().auto_save(true))
                    .expect("warns, does not raise")
                    .len(),
                1
            );
            let node = doc.css("#p1").expect("selects")[0].clone();
            assert!(matches!(
                doc.save(&node, "x"),
                Err(ParseError::AdaptiveDisabled)
            ));
            assert!(matches!(
                doc.retrieve("x"),
                Err(ParseError::AdaptiveDisabled)
            ));
        }

        #[test]
        fn storage_flows_down_to_every_node_of_the_tree() {
            let storage = storage();
            let doc = adaptive(OLD, Arc::clone(&storage));
            let section = doc.css("section.products").expect("selects")[0].clone();
            section.save("section").expect("child nodes can save");
            assert_eq!(
                storage
                    .retrieve("section")
                    .expect("reads")
                    .expect("a row")
                    .tag,
                "section"
            );
            // …and a child node can relocate through the same handle.
            let new = adaptive(NEW, storage);
            let fingerprint = new
                .retrieve("section")
                .expect("reads")
                .expect("saved above");
            let found = new.relocate(&fingerprint, 40);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].tag(), "section");
        }
    }

    #[test]
    fn relocate_returns_every_tie_and_nothing_below_the_threshold() {
        let doc = Document::parse("<ul><li class=x>a</li><li class=x>a</li></ul>");
        let first = doc.elements().find(|e| e.tag() == "li").expect("an <li>");
        let fingerprint = element_to_fingerprint(first);

        let candidates: Vec<_> = doc
            .root_element()
            .expect("<html>")
            .descendant_elements()
            .collect();
        let hits = relocate_among(&fingerprint, candidates.clone(), 40);
        assert_eq!(hits.len(), 2, "both identical <li>s tie at the top score");

        assert!(relocate_among(&fingerprint, candidates, 101).is_empty());
        assert!(relocate_among(&fingerprint, Vec::new(), 0).is_empty());
    }
}
