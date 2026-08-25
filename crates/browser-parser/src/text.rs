//! Text extraction: [`get_all_text`] with its tail-attributed-to-grandparent
//! rule, and the two text searches [`find_by_text`] / [`find_by_regex`].
//!
//! # `get_all_text` and the tail rule
//!
//! Python walks `.//text()` — every descendant text node, in document order —
//! and decides whether each is *visible* by looking at its owner:
//!
//! ```python
//! parent = text_node.getparent()
//! owner  = parent.getparent() if text_node.is_tail else parent
//! ```
//!
//! In lxml a *tail* string's `getparent()` is the element it trails, so its
//! owner is that element's parent. In this tree a text node is a real node, so
//! both cases collapse to `text_node.parent()` — the tail of `</script>` is a
//! sibling of the `<script>`, therefore a child of the script's parent, and it
//! survives an `ignore_tags=("script",)` pass exactly as it does in Python.
//!
//! ```
//! use browser_parser::config::{AllTextOpts, DocumentConfig};
//! use browser_parser::document::Document;
//!
//! let doc = Document::parse("<div>keep<script>drop()</script>done</div>");
//! assert_eq!(doc.get_all_text(), "keep\ndone");
//!
//! // Text after a comment is kept, because the comment is not an ignored tag.
//! let with_comments = Document::parse_with(
//!     "<div>before<!-- c -->after<p>x</p></div>",
//!     &DocumentConfig::new().keep_comments(true),
//! );
//! assert_eq!(with_comments.get_all_text(), "before\nafter\nx");
//!
//! // Every keyword argument has a field.
//! let opts = AllTextOpts::new().separator(" | ").strip(true);
//! assert_eq!(doc.get_all_text_with(&opts), "keep | done");
//! ```
//!
//! # The return-type asymmetry of the two searches
//!
//! Python's `find_by_text` / `find_by_regex` return a bare `Selector` when
//! `first_match=True` **and** something matched, and a `Selectors` otherwise —
//! so *no match under `first_match=True` yields an empty list, not `None`*.
//! Rust has no union return, so both always give [`Nodes`]: length 0 or 1 under
//! `first_match`, and the "empty, not `None`" half of the asymmetry falls out
//! for free. `.first()` recovers the Python value.

use std::collections::HashSet;

use browser_core::text::{ReOptions, RegexSpec, TextExt};
use browser_dom::NodeRef;

use crate::config::{AllTextOpts, ByRegexOpts, ByTextOpts};
use crate::error::Result;
use crate::node::Node;
use crate::nodes::Nodes;

/// `Selector.get_all_text`.
///
/// A string node answers with its own value, before any of the machinery runs.
#[must_use]
pub fn get_all_text(node: &Node<'_>, opts: &AllTextOpts) -> String {
    let Some(root) = node.as_node_ref() else {
        return node.text().to_owned();
    };

    // `for element in self._root.iter(*ignore_tags)` — self included.
    let mut ignored: HashSet<NodeRef<'_>> = HashSet::new();
    if !opts.ignore_tags.is_empty() {
        for element in root.descendants_and_self().filter(NodeRef::is_element) {
            if !ignored.contains(&element)
                && opts.ignore_tags.iter().any(|tag| tag == element.tag())
            {
                ignored.extend(element.descendants_and_self().filter(NodeRef::is_element));
            }
        }
    }

    let mut parts: Vec<&str> = Vec::new();
    for text_node in root.descendant_text_nodes() {
        let text = text_node.as_text().unwrap_or("");
        if text.is_empty() {
            continue;
        }
        // The tail/text branch collapses to one lookup here — see the module docs.
        let Some(owner) = text_node.parent() else {
            continue;
        };
        if ignored.contains(&owner) {
            continue;
        }
        let processed = if opts.strip { text.trim() } else { text };
        if !opts.valid_values || !processed.trim().is_empty() {
            parts.push(processed);
        }
    }

    parts.join(&opts.separator)
}

/// The candidate set of both searches: `.//*[normalize-space(text())]`.
///
/// XPath's `text()` inside a predicate is the *node-set* of child text nodes and
/// `normalize-space()` converts the **first** of them, so the test is "the first
/// child text node, wherever it sits among the children, is not all XPath
/// whitespace". That is subtly wider than lxml's `element.text`, which is only
/// the first child when the first child is text — an element whose first text
/// child comes after a child element is a candidate here but answers `""` to
/// `.text`.
fn is_candidate(element: NodeRef<'_>) -> bool {
    element
        .children()
        .find(NodeRef::is_text)
        .and_then(|child| child.as_text())
        .is_some_and(|text| !is_xpath_blank(text))
}

/// XPath's whitespace set: space, tab, CR and LF. Deliberately *not* Unicode
/// whitespace — `normalize-space("\u{a0}")` is not empty.
fn is_xpath_blank(text: &str) -> bool {
    text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

fn candidates<'a>(root: NodeRef<'a>) -> impl Iterator<Item = NodeRef<'a>> + 'a {
    root.descendant_elements().filter(|el| is_candidate(*el))
}

/// `Selector.find_by_text`.
///
/// Comparison is against each candidate's **direct** text only.
#[must_use]
pub fn find_by_text<'a>(node: &Node<'a>, text: &str, opts: &ByTextOpts) -> Nodes<'a> {
    let Some(root) = node.as_node_ref() else {
        return Nodes::new();
    };
    let doc = node.doc();

    let needle = if opts.case_sensitive {
        text.to_owned()
    } else {
        text.to_lowercase()
    };

    let mut results = Nodes::new();
    for element in candidates(root) {
        let mut candidate_text = element.text().unwrap_or("").to_owned();
        if opts.clean_match {
            candidate_text = candidate_text.clean(false);
        }
        if !opts.case_sensitive {
            candidate_text = candidate_text.to_lowercase();
        }

        let matched = if opts.partial {
            candidate_text.contains(&needle)
        } else {
            candidate_text == needle
        };
        if matched {
            results.push(Node::element(doc, element));
        }
        if opts.first_match && !results.is_empty() {
            break;
        }
    }
    results
}

/// `Selector.find_by_regex`.
///
/// # Errors
///
/// [`ParseError::Regex`](crate::error::ParseError::Regex) when the pattern does
/// not compile or fails at match time.
pub fn find_by_regex<'a>(
    node: &Node<'a>,
    pattern: RegexSpec<'_>,
    opts: &ByRegexOpts,
) -> Result<Nodes<'a>> {
    let Some(root) = node.as_node_ref() else {
        return Ok(Nodes::new());
    };
    let doc = node.doc();

    let re_opts = ReOptions::default()
        .clean_match(opts.clean_match)
        .case_sensitive(opts.case_sensitive);

    let mut results = Nodes::new();
    for element in candidates(root) {
        if element.text().unwrap_or("").re_check(pattern, re_opts)? {
            results.push(Node::element(doc, element));
        }
        if opts.first_match && !results.is_empty() {
            break;
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocumentConfig;
    use crate::document::Document;

    #[test]
    fn script_and_style_subtrees_are_skipped_but_their_tails_are_not() {
        let doc = Document::parse("<div>keep<script>evil()</script>done</div>");
        assert_eq!(doc.get_all_text(), "keep\ndone");

        let doc = Document::parse("<div>a<style>.x{}</style>b<script>c</script>d</div>");
        assert_eq!(doc.get_all_text(), "a\nb\nd");
    }

    #[test]
    fn a_tail_after_a_comment_is_attributed_to_the_grandparent() {
        let doc = Document::parse_with(
            "<div>before<!-- c -->after<p>x</p></div>",
            &DocumentConfig::new().keep_comments(true),
        );
        assert_eq!(doc.get_all_text(), "before\nafter\nx");
    }

    #[test]
    fn interleaved_text_keeps_document_order() {
        let doc = Document::parse(
            "<div>string1<span>string2</span>string3<p>string4<b>string5</b>string6</p>string7</div>",
        );
        assert_eq!(
            doc.get_all_text(),
            "string1\nstring2\nstring3\nstring4\nstring5\nstring6\nstring7"
        );
    }

    #[test]
    fn every_keyword_argument_is_honoured() {
        let doc = Document::parse("<div> a <p> b </p></div>");
        assert_eq!(doc.get_all_text(), " a \n b ");
        assert_eq!(
            doc.get_all_text_with(&AllTextOpts::new().strip(true)),
            "a\nb"
        );
        assert_eq!(
            doc.get_all_text_with(&AllTextOpts::new().separator(" | ").strip(true)),
            "a | b"
        );

        let table = Document::parse("<div>before<table><td>Cell</td></table></div>");
        let without = table.get_all_text_with(&AllTextOpts::new().ignore_tags(["table"]));
        assert!(!without.contains("Cell"), "{without}");
        // An empty ignore list disables the pass entirely.
        let all = table.get_all_text_with(&AllTextOpts::new().ignore_tags(Vec::<String>::new()));
        assert!(all.contains("Cell"));
    }

    #[test]
    fn valid_values_false_keeps_blank_strings() {
        let doc = Document::parse("<p>a</p><p> </p><p>b</p>");
        let kept = doc.get_all_text_with(&AllTextOpts::new().valid_values(false));
        assert_eq!(kept, "a\n \nb");
        assert_eq!(doc.get_all_text(), "a\nb");
    }

    #[test]
    fn ignoring_the_receivers_own_tag_drops_everything() {
        let doc = Document::parse("<div>x<p>y</p></div>");
        let div = doc.css("div").expect("selects")[0].clone();
        assert_eq!(
            div.get_all_text_with(&AllTextOpts::new().ignore_tags(["div"])),
            ""
        );
    }

    #[test]
    fn deep_nesting_does_not_blow_up() {
        let mut html = String::new();
        for _ in 0..500 {
            html.push_str("<div>");
        }
        html.push_str("deep");
        for _ in 0..500 {
            html.push_str("</div>");
        }
        let doc = Document::parse(&html);
        assert_eq!(doc.get_all_text(), "deep");
    }

    #[test]
    fn find_by_text_is_case_insensitive_and_partial_on_demand() {
        let doc = Document::parse(
            "<div class='p'>In stock: 5</div>\
             <div class='p'>In stock: 3</div>\
             <div class='p'>Out of stock</div>",
        );
        assert_eq!(
            doc.find_by_text(
                "In stock:",
                &ByTextOpts::new().partial(true).first_match(false)
            )
            .len(),
            2
        );
        assert_eq!(
            doc.find_by_text("Out of stock", &ByTextOpts::new().first_match(false))
                .len(),
            1
        );
        // Case-insensitive by default.
        assert_eq!(
            doc.find_by_text("out of stock", &ByTextOpts::new().first_match(false))
                .len(),
            1
        );
        assert!(doc
            .find_by_text(
                "out of stock",
                &ByTextOpts::new().first_match(false).case_sensitive(true)
            )
            .is_empty());
    }

    #[test]
    fn no_match_under_first_match_is_empty_not_none() {
        let doc = Document::parse("<p>hello</p>");
        let hits = doc.find_by_text("absent", &ByTextOpts::default());
        assert!(hits.is_empty(), "empty Nodes, never a None");
        assert_eq!(hits.first(), None);

        let hits = doc
            .find_by_regex(RegexSpec::Pattern("absent"), &ByRegexOpts::default())
            .expect("valid pattern");
        assert!(hits.is_empty());
    }

    #[test]
    fn first_match_caps_the_result_at_one() {
        let doc = Document::parse("<p>hello</p><p>hello</p>");
        assert_eq!(doc.find_by_text("hello", &ByTextOpts::default()).len(), 1);
        assert_eq!(
            doc.find_by_text("hello", &ByTextOpts::new().first_match(false))
                .len(),
            2
        );
    }

    #[test]
    fn clean_match_collapses_whitespace_before_comparing() {
        let doc = Document::parse("<p>  Hello\tWorld  </p>");
        assert_eq!(
            doc.find_by_text("Hello World", &ByTextOpts::default())
                .len(),
            1
        );
        assert!(doc
            .find_by_text("Hello World", &ByTextOpts::new().clean_match(false))
            .is_empty());
    }

    #[test]
    fn find_by_regex_matches_direct_text() {
        let doc =
            Document::parse("<div>In stock: 5</div><div>In stock: 3</div><div>Out of stock</div>");
        assert_eq!(
            doc.find_by_regex(
                RegexSpec::Pattern(r"In stock: \d+"),
                &ByRegexOpts::new().first_match(false)
            )
            .expect("valid")
            .len(),
            2
        );
        assert_eq!(
            doc.find_by_regex(
                RegexSpec::Pattern(r"In stock: \d+"),
                &ByRegexOpts::new().case_sensitive(true)
            )
            .expect("valid")[0]
                .text(),
            "In stock: 5"
        );
    }

    #[test]
    fn a_text_node_search_is_empty_and_never_raises() {
        let doc = Document::parse("<p>hello</p>");
        let text = doc.css("p::text").expect("selects")[0].clone();
        assert!(text
            .find_by_text("hello", &ByTextOpts::default())
            .is_empty());
        assert!(text
            .find_by_regex(RegexSpec::Pattern("hello"), &ByRegexOpts::default())
            .expect("guarded")
            .is_empty());
        assert_eq!(text.get_all_text(), "hello");
    }

    #[test]
    fn xpath_blankness_is_not_unicode_blankness() {
        assert!(is_xpath_blank(" \t\r\n"));
        assert!(
            !is_xpath_blank("\u{a0}"),
            "a non-breaking space is not blank"
        );
        assert!(!is_xpath_blank("x"));
        assert!(is_xpath_blank(""));
    }
}
