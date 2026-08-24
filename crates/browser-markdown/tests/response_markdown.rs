//! The `tests/fetchers/test_response_markdown.py` expectations, ported.
//!
//! `Response.markdown()` itself belongs to `browser-parser` (it needs CSS
//! selection, which lives there); what this file pins is the part this crate
//! owns — the exact algorithm that method runs:
//!
//! ```text
//! page  = (css("body").first or self) if main_content_only else self
//! page  = _sanitize_for_ai(_strip_noise_tags(page))
//! pages = [page] if not css_selector else page.css(css_selector)
//! "".join(_convert_to_markdown(el.html_content) for el in pages)
//! ```
//!
//! Note the ordering the Python tests pin: the cleaning passes run **always**
//! and **before** the selector, so `markdown(css_selector=".a")` still cannot
//! surface hidden text from inside a matched element.

use browser_dom::{Document, NodeRef};
use browser_markdown::{clean_for_ai, to_markdown_default};

/// The fixture from `test_response_markdown.py`, unchanged.
const HTML: &str = r#"
<html>
  <head><title>Docs Home</title><style>.hint { color: red; }</style></head>
  <body>
    <h1>Title</h1>
    <p>Visible content.</p>
    <p style="display:none">hidden instructions</p>
    <script>console.log("noise")</script>
    <div class="a">First</div>
    <div class="a">Second</div>
  </body>
</html>
"#;

/// `Response.markdown()` for the cases that need no CSS engine.
fn markdown(main_content_only: bool) -> String {
    let doc = Document::parse(HTML);
    let scope = if main_content_only {
        doc.body().map(|body| body.id())
    } else {
        None
    };
    let clean = clean_for_ai(&doc);
    let page: NodeRef<'_> = scope
        .and_then(|id| clean.node(id))
        .or_else(|| clean.root_element())
        .unwrap_or_else(|| clean.root());
    to_markdown_default(page)
}

/// `Response.markdown(css_selector=".a")`, with the selector stood in for by a
/// class check so this crate does not need the CSS engine.
fn markdown_class(class: &str) -> String {
    let doc = Document::parse(HTML);
    let clean = clean_for_ai(&doc);
    clean
        .elements()
        .filter(|element| element.has_class(class))
        .map(to_markdown_default)
        .collect()
}

#[test]
fn returns_markdown_with_a_setext_h1() {
    let md = markdown(false);
    assert!(md.contains("Title\n====="), "{md:?}");
    assert!(md.contains("Visible content."), "{md:?}");
    assert!(md.contains("Docs Home"), "{md:?}");
}

#[test]
fn sanitization_is_always_applied() {
    let md = markdown(false);
    assert!(!md.contains("hidden instructions"), "{md:?}");
    assert!(!md.contains("console.log"), "{md:?}");
    assert!(!md.contains(".hint"), "{md:?}");
}

#[test]
fn main_content_only_scopes_to_body() {
    let md = markdown(true);
    assert!(!md.contains("Docs Home"), "{md:?}");
    assert!(md.contains("Visible content."), "{md:?}");
}

#[test]
fn css_selector_concatenates_all_matches() {
    let md = markdown_class("a");
    assert!(md.contains("First"), "{md:?}");
    assert!(md.contains("Second"), "{md:?}");
    assert!(!md.contains("Visible content."), "{md:?}");
}

#[test]
fn cleaning_survives_node_identity() {
    // The whole approach above depends on arena ids surviving both passes.
    let doc = Document::parse(HTML);
    let body = doc.body().expect("body");
    let clean = clean_for_ai(&doc);
    assert_eq!(clean.node(body.id()).map(|node| node.tag()), Some("body"));
}
