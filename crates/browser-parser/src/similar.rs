//! [`find_similar`] — the two-stage sibling finder, with the
//! `max(len(a), len(b))` denominator.
//!
//! Stage one is a **hard structural filter**, expressed in Python as one
//! absolute XPath:
//!
//! ```text
//! //<grandparent>/<parent>/<tag>[count(ancestor::*) = <depth>]
//! ```
//!
//! so a candidate must have the same tag, the same parent tag, the same
//! grandparent tag and the same DOM depth — and, because the path is absolute,
//! it may live **anywhere in the document**, not only under the receiver.
//! `ARCHITECTURE.md` §3 replaces the XPath with a depth-filtered document walk;
//! the predicate is identical.
//!
//! Stage two is a fuzzy attribute (and optionally text) score:
//!
//! ```text
//! score  = Σ ratio(original[k], candidate.get(k, ""))  for k in original
//! checks = max(len(original), len(candidate))
//! ```
//!
//! The `max` denominator is deliberate and pinned: a candidate with **fewer**
//! attributes must not get an inflated score from a smaller denominator, and a
//! candidate with **extra** attributes must be penalised. With a 4-attribute
//! original at `threshold = 0.9` and no ignored attributes, an identical
//! 4-attribute candidate scores `4.0/4 = 1.0` and is kept, a 1-attribute
//! candidate scores `1.0/4 = 0.25` and is dropped, and a 5-attribute superset
//! scores `4.0/5 = 0.8` and is dropped too.
//!
//! ```
//! use browser_parser::config::SimilarOpts;
//! use browser_parser::document::Document;
//!
//! let doc = Document::parse(
//!     "<section>\
//!        <div class='product'>1</div>\
//!        <div class='product'>2</div>\
//!        <div class='product'>3</div>\
//!        <section class='product'>not a div</section>\
//!      </section>",
//! );
//! let first = doc.css(".product").unwrap().first().cloned().unwrap();
//! // Three sibling divs -> two similar; the <section> is a different tag.
//! assert_eq!(first.find_similar(&SimilarOpts::default()).len(), 2);
//! ```

use browser_core::fingerprint::AttributeDict;
use browser_core::numeric::round_half_even;
use browser_core::similarity::ratio_str;
use browser_core::text::clean_spaces;
use browser_dom::NodeRef;

use crate::config::SimilarOpts;
use crate::node::Node;
use crate::nodes::Nodes;

/// `Selector.find_similar`.
///
/// Returns the *other* elements that pass both stages, in document order. The
/// receiver itself is always excluded, so a container with N identical siblings
/// yields N-1. A string node yields an empty list, without raising.
#[must_use]
pub fn find_similar<'a>(node: &Node<'a>, opts: &SimilarOpts) -> Nodes<'a> {
    let Some(root) = node.as_node_ref() else {
        return Nodes::new();
    };
    let doc = node.doc();

    let current_depth = root.depth();
    let target_attributes = attributes_for(root, &opts.ignore_attributes);

    // `path_parts`: [grandparent?, parent?, self]
    let parent = root.parent_element();
    let grandparent = parent.and_then(|p| p.parent_element());
    let tag = root.tag();
    let parent_tag = parent.map(|p| p.tag());
    let grandparent_tag = grandparent.map(|g| g.tag());

    doc.dom()
        .root()
        .descendant_elements()
        .filter(|candidate| {
            candidate.tag() == tag
                && candidate.depth() == current_depth
                && matches_ancestor_chain(*candidate, parent_tag, grandparent_tag)
        })
        .filter(|candidate| *candidate != root)
        .filter(|candidate| are_alike(root, &target_attributes, *candidate, opts))
        .map(|candidate| Node::element(doc, candidate))
        .collect()
}

/// The `//gp/p/tag` half of the structural filter.
fn matches_ancestor_chain(
    candidate: NodeRef<'_>,
    parent_tag: Option<&str>,
    grandparent_tag: Option<&str>,
) -> bool {
    let Some(parent_tag) = parent_tag else {
        // `//tag` — no ancestor constraint at all.
        return true;
    };
    let Some(candidate_parent) = candidate.parent_element() else {
        return false;
    };
    if candidate_parent.tag() != parent_tag {
        return false;
    }
    let Some(grandparent_tag) = grandparent_tag else {
        return true;
    };
    candidate_parent
        .parent_element()
        .is_some_and(|gp| gp.tag() == grandparent_tag)
}

/// `__get_attributes`, including its asymmetry: an **empty** ignore list means
/// "use the raw `attrib` map", not "filter nothing out of a copy". Values are
/// neither stripped nor dropped when empty — unlike the adaptive fingerprint's
/// `__clean_attributes`.
fn attributes_for(element: NodeRef<'_>, ignore: &[String]) -> AttributeDict {
    if ignore.is_empty() {
        return element.attrs().to_index_map();
    }
    element
        .attrs()
        .iter()
        .filter(|(name, _)| !ignore.iter().any(|ignored| ignored == name))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// `__are_alike`.
fn are_alike(
    original: NodeRef<'_>,
    original_attributes: &AttributeDict,
    candidate: NodeRef<'_>,
    opts: &SimilarOpts,
) -> bool {
    let candidate_attributes = attributes_for(candidate, &opts.ignore_attributes);
    let mut score = 0.0_f64;
    let mut checks = 0_usize;

    if original_attributes.is_empty() {
        if candidate_attributes.is_empty() {
            // "Both don't have attributes, this must mean something."
            score += 1.0;
            checks += 1;
        }
    } else {
        for (name, value) in original_attributes {
            let other = candidate_attributes.get(name).map_or("", String::as_str);
            score += ratio_str(value, other);
        }
        checks += original_attributes.len().max(candidate_attributes.len());
    }

    if opts.match_text {
        score += ratio_str(
            &clean_spaces(original.text().unwrap_or("")),
            &clean_spaces(candidate.text().unwrap_or("")),
        );
        checks += 1;
    }

    if checks == 0 {
        return false;
    }
    round_half_even(score / checks as f64, 2) >= opts.threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    const PRODUCTS: &str = concat!(
        "<div class='container'><section class='products'>",
        "<article class='product' data-id='1'><h3>A</h3></article>",
        "<article class='product' data-id='2'><h3>B</h3></article>",
        "<article class='product' data-id='3'><h3>C</h3></article>",
        "</section></div>",
    );

    #[test]
    fn three_siblings_yield_two_similar() {
        let doc = Document::parse(PRODUCTS);
        let first = doc.css(".product").expect("selects")[0].clone();
        let similar = first.find_similar(&SimilarOpts::default());
        assert_eq!(similar.len(), 2);
        assert_eq!(similar.attrs("data-id"), ["2", "3"]);
    }

    #[test]
    fn a_different_tag_at_the_same_depth_is_not_similar() {
        let doc = Document::parse(
            "<section>\
             <div class='product'>1</div><div class='product'>2</div>\
             <section class='product'>x</section></section>",
        );
        let first = doc.css("div.product").expect("selects")[0].clone();
        assert_eq!(first.find_similar(&SimilarOpts::default()).len(), 1);
    }

    #[test]
    fn a_higher_threshold_never_returns_more() {
        let doc = Document::parse(PRODUCTS);
        let first = doc.css(".product").expect("selects")[0].clone();
        let mut previous = usize::MAX;
        for threshold in [0.0_f64, 0.2, 0.5, 0.9, 1.0] {
            let count = first
                .find_similar(&SimilarOpts::new().threshold(threshold))
                .len();
            assert!(count <= previous, "threshold {threshold} returned more");
            previous = count;
        }
    }

    #[test]
    fn match_text_never_returns_more_than_without_it() {
        let doc = Document::parse(PRODUCTS);
        let first = doc.css(".product").expect("selects")[0].clone();
        let plain = first.find_similar(&SimilarOpts::default()).len();
        let with_text = first
            .find_similar(&SimilarOpts::new().match_text(true))
            .len();
        assert!(with_text <= plain);
    }

    /// `test_find_similar_attribute_count_mismatch_scoring`.
    #[test]
    fn the_denominator_is_max_of_the_two_attribute_counts() {
        let doc = Document::parse(concat!(
            "<div id='root'>",
            "<p class='c' data-kind='k' data-color='red' data-size='l'>Alpha</p>",
            "<p class='c'>Beta</p>",
            "<p class='c' data-kind='k' data-color='red' data-size='l' data-id='x'>Gamma</p>",
            "<p class='c' data-kind='k' data-color='red' data-size='l'>Delta</p>",
            "</div>",
        ));
        let alpha = doc.css("p").expect("selects")[0].clone();
        assert_eq!(alpha.text(), "Alpha");

        let opts = SimilarOpts::new()
            .threshold(0.9)
            .ignore_attributes(Vec::<String>::new());
        let similar = alpha.find_similar(&opts);
        assert_eq!(
            similar.texts(),
            ["Delta"],
            "Beta (1.0/4) and Gamma (4.0/5) both fall below 0.9"
        );
    }

    #[test]
    fn two_attribute_less_elements_score_a_perfect_match() {
        let doc = Document::parse("<ul><li>a</li><li>b</li></ul>");
        let first = doc.css("li").expect("selects")[0].clone();
        assert_eq!(first.find_similar(&SimilarOpts::default()).len(), 1);
    }

    #[test]
    fn an_empty_ignore_list_uses_the_raw_attribute_map() {
        let doc = Document::parse(
            "<ul><li href='/a' class='x'>a</li><li href='/b' class='x'>b</li></ul>",
        );
        let first = doc.css("li").expect("selects")[0].clone();
        // With `href` ignored the two are identical on `class` alone.
        assert_eq!(first.find_similar(&SimilarOpts::default()).len(), 1);
        // With nothing ignored the differing hrefs drag the score down but the
        // shared class keeps it above the 0.2 default.
        assert_eq!(
            first
                .find_similar(&SimilarOpts::new().ignore_attributes(Vec::<String>::new()))
                .len(),
            1
        );
        // …and a strict threshold now rejects it.
        assert!(first
            .find_similar(
                &SimilarOpts::new()
                    .ignore_attributes(Vec::<String>::new())
                    .threshold(0.95)
            )
            .is_empty());
    }

    #[test]
    fn the_search_is_document_wide_not_subtree_wide() {
        // Two sibling groups under different parents with the same tag chain
        // and depth: an absolute `//div/p` reaches both.
        let doc = Document::parse(
            "<body><div><p class='x'>1</p></div><div><p class='x'>2</p></div></body>",
        );
        let first = doc.css("p").expect("selects")[0].clone();
        let similar = first.find_similar(&SimilarOpts::default());
        assert_eq!(similar.texts(), ["2"]);
    }

    #[test]
    fn a_text_node_is_empty_and_does_not_raise() {
        let doc = Document::parse(PRODUCTS);
        let text = doc.css(".product h3::text").expect("selects")[0].clone();
        assert!(text.find_similar(&SimilarOpts::default()).is_empty());
    }
}
