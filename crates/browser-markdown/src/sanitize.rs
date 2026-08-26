//! The reference implementation's cleaning layer — `Convertor._strip_noise_tags` and
//! `Convertor._sanitize_for_ai` from `reference/core/shell.py`.
//!
//! Both take a document, copy it, prune it and hand back a new document; the
//! input is never modified. `Response.markdown()` runs them in that order and
//! **always**, even when `main_content_only` is false — the CSS selector is
//! applied afterwards, so hidden prompt-injection payloads can never reach the
//! model.
//!
//! # What `_sanitize_for_ai` removes
//!
//! The reference implementation's `_HIDDEN_XPATH` is a naive `contains()` test on the raw `style`
//! attribute, matching only the twelve literal substrings in
//! [`HIDDEN_STYLE_SUBSTRINGS`] (no-space and one-space spellings of six
//! properties). It does not parse CSS, which is deliberate but has consequences:
//! `opacity:0` also matches `opacity:0.5`, and `width:0` also matches
//! `width:0px`. That is reproduced exactly.
//!
//! Then, for every text node, zero-width characters are removed **before** C0
//! control characters. The order matters: `\u{200b}` is not a control character,
//! so doing it the other way round would leave a zero-width joiner that had
//! been split by a control byte intact.

use std::borrow::Cow;

use browser_dom::{Document, NodeData, NodeRef};

/// The elements `_strip_noise_tags` drops, subtree and all.
pub const NOISE_TAGS: [&str; 4] = ["script", "style", "noscript", "svg"];

/// The twelve `style` substrings `_HIDDEN_XPATH` tests with `contains()`.
pub const HIDDEN_STYLE_SUBSTRINGS: [&str; 12] = [
    "display:none",
    "display: none",
    "visibility:hidden",
    "visibility: hidden",
    "opacity:0",
    "opacity: 0",
    "font-size:0",
    "font-size: 0",
    "height:0",
    "height: 0",
    "width:0",
    "width: 0",
];

/// The zero-width characters removed first.
pub const ZERO_WIDTH_CHARS: [char; 6] = [
    '\u{200b}', // zero width space
    '\u{200c}', // zero width non-joiner
    '\u{200d}', // zero width joiner
    '\u{feff}', // zero width no-break space / BOM
    '\u{2060}', // word joiner
    '\u{180e}', // mongolian vowel separator
];

/// `True` for the C0 control characters `_CONTROL_CHARS_PATTERN` removes —
/// everything below `U+0020` except `\t`, `\n` and `\r`.
#[must_use]
#[inline]
pub fn is_stripped_control_char(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}')
}

/// `True` for a zero-width character.
#[must_use]
#[inline]
pub fn is_zero_width_char(c: char) -> bool {
    ZERO_WIDTH_CHARS.contains(&c)
}

/// Remove zero-width characters, then C0 control characters — in that order.
#[must_use]
pub fn scrub_text(text: &str) -> Cow<'_, str> {
    if text
        .chars()
        .any(|c| is_zero_width_char(c) || is_stripped_control_char(c))
    {
        Cow::Owned(
            text.chars()
                .filter(|c| !is_zero_width_char(*c))
                .filter(|c| !is_stripped_control_char(*c))
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// `True` when `_HIDDEN_XPATH` would select this element.
///
/// The three alternatives of the XPath union: an inline style that looks
/// hidden, `aria-hidden="true"`, or a `<template>`.
#[must_use]
pub fn is_hidden_element(node: NodeRef<'_>) -> bool {
    let Some(element) = node.as_element() else {
        return false;
    };
    if element.tag() == "template" {
        return true;
    }
    if node.attr("aria-hidden") == Some("true") {
        return true;
    }
    node.attr("style").is_some_and(|style| {
        HIDDEN_STYLE_SUBSTRINGS
            .iter()
            .any(|needle| style.contains(needle))
    })
}

/// The reference implementation's `Convertor._strip_noise_tags`.
///
/// Drops every `<script>`, `<style>`, `<noscript>` and `<svg>` subtree. lxml's
/// `drop_tree()` re-parents the element's tail text; here the tail is already a
/// separate sibling node, so detaching the element is enough — but the now
/// adjacent text siblings are merged so the result is exactly the tree lxml
/// would have produced.
#[must_use]
pub fn strip_noise_tags(doc: &Document) -> Document {
    rebuild(doc, |node| {
        node.local_name()
            .is_some_and(|name| NOISE_TAGS.contains(&name))
    })
}

/// The reference implementation's `Convertor._sanitize_for_ai`.
///
/// Drops hidden elements (see [`is_hidden_element`]) and then scrubs every text
/// node. Like the XPath it ports, the document's root element is never itself a
/// removal candidate — `_HIDDEN_XPATH` starts with `.//*`.
#[must_use]
pub fn sanitize_for_ai(doc: &Document) -> Document {
    let root_element = doc.root_element().map(|node| node.id());
    let mut out = rebuild(doc, |node| {
        Some(node.id()) != root_element && is_hidden_element(node)
    });
    scrub_all_text(&mut out);
    out
}

/// Both cleaning passes, in the order `Response.markdown()` applies them.
///
/// `Response.markdown()` runs this **always** — before any CSS selector — so
/// that a hidden prompt-injection payload can never reach a model even when the
/// caller asked for a narrow slice of the page.
///
/// # Node identity survives
///
/// Arena ids are stable across both passes, so a caller holding an id from
/// `doc` can look the same node up in the result:
///
/// ```
/// use browser_dom::Document;
/// use browser_markdown::{clean_for_ai, to_markdown_default};
///
/// let doc = Document::parse("<body><div>keep<script>x</script></div></body>");
/// let div = doc.elements().find(|e| e.tag() == "div").unwrap();
///
/// let clean = clean_for_ai(&doc);
/// let same = clean.node(div.id()).unwrap();
/// assert_eq!(to_markdown_default(same), "keep");
/// ```
///
/// A node that was *removed* keeps its id too, but is no longer reachable from
/// the root — check [`browser_dom::NodeRef::parent`] if that matters.
#[must_use]
pub fn clean_for_ai(doc: &Document) -> Document {
    sanitize_for_ai(&strip_noise_tags(doc))
}

/// Copy `doc`, detach every node matching `should_remove`, then re-merge text.
fn rebuild(doc: &Document, should_remove: impl Fn(NodeRef<'_>) -> bool) -> Document {
    let mut out = doc.clone();
    out.remove_matching(should_remove);
    // `remove_matching` leaves the siblings of a removed element untouched, so a
    // `text <script/> text` run becomes two adjacent text nodes. lxml's
    // `drop_tree()` would have concatenated them, and markdownify's whitespace
    // collapsing is sensitive to the difference.
    let mut tree = out.tree().clone();
    browser_dom::parse::merge_adjacent_text(&mut tree);
    Document::from_tree(tree, doc.base_url().cloned(), doc.encoding().to_owned())
}

/// Apply [`scrub_text`] to every text node of `doc`.
fn scrub_all_text(doc: &mut Document) {
    let edits: Vec<_> = doc
        .nodes()
        .filter_map(|node| match node.value() {
            NodeData::Text(text) => match scrub_text(text) {
                Cow::Owned(scrubbed) => Some((node.id(), scrubbed)),
                Cow::Borrowed(_) => None,
            },
            _ => None,
        })
        .collect();
    for (id, scrubbed) in edits {
        // `set_value` only fails for an id that is not in the tree, and every id
        // here came out of this very tree.
        let _ = doc.set_value(id, NodeData::Text(scrubbed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::to_markdown_default;

    #[test]
    fn strips_the_four_noise_tags() {
        let doc = Document::parse(
            "<div>keep<script>bad()</script><style>.x{}</style><noscript>n</noscript><svg><path/></svg>tail</div>",
        );
        let clean = strip_noise_tags(&doc);
        let html = clean.html();
        assert!(!html.contains("script"));
        assert!(!html.contains("style"));
        assert!(!html.contains("noscript"));
        assert!(!html.contains("svg"));
        assert!(html.contains("keep"));
        assert!(html.contains("tail"));
    }

    #[test]
    fn merges_text_across_a_removed_element() {
        let doc = Document::parse("<p>a <script>x</script> b</p>");
        let clean = strip_noise_tags(&doc);
        let p = clean.elements().find(|e| e.tag() == "p").expect("p");
        assert_eq!(p.children().count(), 1);
        assert_eq!(p.text_content(), "a  b");
    }

    #[test]
    fn hidden_predicate_covers_all_three_alternatives() {
        let doc = Document::parse(
            "<div><i style=\"color:red;display: none\">a</i><b aria-hidden=\"true\">b</b><template>c</template><u>d</u></div>",
        );
        let hidden: Vec<&str> = doc
            .elements()
            .filter(|node| is_hidden_element(*node))
            .map(|node| node.tag())
            .collect();
        assert_eq!(hidden, vec!["i", "b", "template"]);
    }

    #[test]
    fn naive_contains_matches_opacity_point_five() {
        let doc = Document::parse("<p style=\"opacity:0.5\">x</p><p style=\"width:0px\">y</p>");
        assert_eq!(
            doc.elements()
                .filter(|node| is_hidden_element(*node))
                .count(),
            2
        );
    }

    #[test]
    fn sanitize_removes_hidden_and_scrubs_text() {
        let html = "<div><p style=\"display:none\">hidden instructions</p><p>Vis\u{200b}ible\u{7}!</p></div>";
        let doc = Document::parse(html);
        let clean = sanitize_for_ai(&doc);
        let text = clean.root().text_content();
        assert!(!text.contains("hidden instructions"));
        assert!(text.contains("Visible!"));
        assert!(!text.contains('\u{200b}'));
        assert!(!text.contains('\u{7}'));
    }

    #[test]
    fn scrub_keeps_tab_newline_carriage_return() {
        assert_eq!(scrub_text("a\tb\nc\rd"), "a\tb\nc\rd");
        assert_eq!(scrub_text("a\u{b}b\u{c}c"), "abc");
    }

    #[test]
    fn zero_width_removed_before_control_chars() {
        // A ZWSP split by a control character: removing ZWCs first leaves
        // nothing behind for the control-char pass to reunite.
        assert_eq!(scrub_text("a\u{200b}\u{1}\u{200d}b"), "ab");
    }

    #[test]
    fn end_to_end_matches_response_markdown_expectations() {
        let html = "<html><head><title>Docs Home</title><style>.hint{color:red}</style></head>\
                    <body><h1>Title</h1><p>Visible content.</p>\
                    <p style=\"display:none\">hidden instructions</p>\
                    <script>console.log(\"noise\")</script>\
                    <div class=\"a\">First</div><div class=\"a\">Second</div></body></html>";
        let doc = Document::parse(html);
        let cleaned = sanitize_for_ai(&strip_noise_tags(&doc));
        let md = to_markdown_default(cleaned.root());
        assert!(md.contains("Title\n====="), "{md:?}");
        assert!(md.contains("Visible content."));
        assert!(md.contains("Docs Home"));
        assert!(!md.contains("hidden instructions"));
        assert!(!md.contains("console.log"));
        assert!(!md.contains(".hint"));
    }
}
