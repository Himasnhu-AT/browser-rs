//! The `browser-dom` → `browser-selectors` seam.
//!
//! `ARCHITECTURE.md` §1.3 commits this crate to matching CSS natively with Servo's
//! `selectors`, which means `NodeRef` must be able to back a `selectors::Element`.
//! Every required method of that trait is listed below with the `browser-dom`
//! call that satisfies it, and the non-obvious ones are executed.
//!
//! This file deliberately does **not** implement `SelectorImpl`/`Element` — that is
//! `src/impl_selector.rs`, and it belongs to the unit that builds the matcher. What
//! it does is prove, before that unit starts, that no required capability is
//! missing from the tree, so the matcher is a mechanical translation rather than a
//! renegotiation of the DOM API.
//!
//! | `selectors::Element` method | backed by |
//! |---|---|
//! | `opaque` | `NodeRef::value()` — a `&NodeData` borrowed from the arena |
//! | `parent_element` | `NodeRef::parent_element` |
//! | `prev_sibling_element` / `next_sibling_element` | `prev_element_sibling` / `next_element_sibling` |
//! | `first_element_child` | `NodeRef::first_element_child` |
//! | `has_local_name` / `is_same_type` | `Element::local_name` (interned `LocalName`) |
//! | `has_namespace` / `is_html_element_in_html_document` | `Element::name()` → `QualName::ns` |
//! | `attr_matches` / `has_attr_in_no_namespace` | `Attributes::iter_qual` |
//! | `has_id` (both `CaseSensitivity`) | `Element::id` |
//! | `has_class` (both `CaseSensitivity`) | `has_class` / `has_class_ignore_ascii_case` |
//! | `is_empty` (`:empty`) | `element_children` + `as_text` |
//! | `is_root` (`:root`) | `NodeRef::is_root_element` |
//! | `is_link` (`:link`) | `local_name` + `attr("href")` |
//! | `add_element_unique_hashes` | `local_name` / `id` / `classes` |
//! | `apply_selector_flags`, shadow DOM, parts, custom state | inapplicable to a static tree |
//!
//! `:nth-child` / `:nth-of-type` come from `NodeRef::index_in_parent` and
//! `NodeRef::nth_of_type`; `::text` / `::attr()` from `text_content` / `attr`.

use browser_dom::{Document, NodeRef};
use markup5ever::{LocalName, Namespace};
use selectors::attr::CaseSensitivity;
use selectors::OpaqueElement;

const PAGE: &str = r#"
<html><body>
  <div id="Wrapper" class="Card wide" data-role="box">
    <a href="/one" class="link">one</a>
    <a class="link">no href</a>
    <span></span>
    <span>text</span>
    <svg><a xlink:href="/svg">s</a></svg>
    <p xml:lang="en">p</p>
  </div>
</body></html>
"#;

fn doc() -> Document {
    Document::parse(PAGE)
}

fn by_id<'a>(doc: &'a Document, id: &str) -> NodeRef<'a> {
    doc.elements()
        .find(|e| e.element_id() == Some(id))
        .expect("element exists")
}

/// `opaque()` is the identity `selectors` uses to detect cycles and to compare
/// elements across matching passes. `NodeRef` is `Copy` and re-created constantly,
/// so its *own* address is worthless — but `value()` hands back a reference into
/// the arena, whose address is stable for the life of the `Document`.
#[test]
fn opaque_identity_is_stable_across_independently_obtained_handles() {
    let doc = doc();

    let first = by_id(&doc, "Wrapper");
    let second = by_id(&doc, "Wrapper"); // fresh walk, fresh handle
    assert_eq!(first, second, "NodeRef equality is document-scoped");

    let a = OpaqueElement::new(first.value());
    let b = OpaqueElement::new(second.value());
    assert_eq!(a, b, "the same node must produce the same OpaqueElement");

    let other = OpaqueElement::new(doc.body().expect("body").value());
    assert_ne!(a, other, "different nodes must differ");

    // The property the above relies on, stated directly.
    assert!(std::ptr::eq(first.value(), second.value()));
}

/// `Hash`/`Eq` are document-scoped, so `NodeRef` can key the caches the matcher
/// and the `nth_index_cache` keep.
#[test]
fn noderef_is_a_usable_map_key() {
    use std::collections::HashSet;

    let doc = doc();
    let mut seen: HashSet<NodeRef<'_>> = HashSet::new();
    for el in doc.elements() {
        assert!(seen.insert(el), "document order must not repeat a node");
    }
    assert!(seen.contains(&by_id(&doc, "Wrapper")));
    assert_eq!(seen.len(), doc.elements().count());
}

#[test]
fn local_name_and_namespace_are_available_for_type_selectors() {
    let doc = doc();
    let wrapper = by_id(&doc, "Wrapper");
    let element = wrapper.as_element().expect("an element");

    // has_local_name: compare against an interned LocalName, no allocation.
    assert_eq!(element.local_name(), &LocalName::from("div"));

    // has_namespace / is_html_element_in_html_document.
    assert_eq!(
        element.name().ns,
        Namespace::from("http://www.w3.org/1999/xhtml")
    );

    // is_same_type.
    let first_a = doc.elements().find(|e| e.tag() == "a").expect("an <a>");
    let second_a = doc
        .elements()
        .filter(|e| e.tag() == "a")
        .nth(1)
        .expect("2nd <a>");
    assert_eq!(
        first_a.as_element().map(browser_dom::Element::local_name),
        second_a.as_element().map(browser_dom::Element::local_name)
    );
}

#[test]
fn attribute_selectors_see_namespaces_and_values() {
    let doc = doc();
    let wrapper = by_id(&doc, "Wrapper");

    // has_attr_in_no_namespace / AttrSelectorOperation::Exists.
    assert_eq!(wrapper.attr("data-role"), Some("box"));
    assert_eq!(wrapper.attr("missing"), None);

    // NamespaceConstraint::Any is just the flat iterator.
    assert!(wrapper
        .attrs()
        .iter()
        .any(|(k, v)| k == "class" && v == "Card wide"));
}

/// `attr_matches` takes a `NamespaceConstraint`, so namespaced attributes have to
/// survive the parse with their namespace intact. They do — in *foreign content*.
///
/// The asymmetry below is the HTML5 spec's, not this tree's, and unit 5 needs to
/// know it before writing `[xml|lang]` handling: html5ever only runs the "adjust
/// foreign attributes" step inside SVG/MathML subtrees. On an HTML element,
/// `xml:lang` is a no-namespace attribute whose *local name literally contains a
/// colon*. Every browser behaves this way; lxml (an XML parser in HTML mode) does
/// not, so this is a real, if obscure, divergence from Python.
#[test]
fn namespaced_attributes_keep_their_namespace_in_foreign_content() {
    let doc = doc();

    // SVG subtree: prefix and namespace are both preserved.
    let svg_a = doc
        .elements()
        .find(|e| {
            e.tag() == "a"
                && e.as_element()
                    .is_some_and(|el| el.name().ns.to_string() == "http://www.w3.org/2000/svg")
        })
        .expect("the <a> inside <svg>");
    let (name, value) = svg_a.attrs().nth(0).expect("one attribute");
    assert_eq!(name.local.to_string(), "href");
    assert_eq!(name.ns.to_string(), "http://www.w3.org/1999/xlink");
    assert_eq!(
        name.prefix.as_ref().map(ToString::to_string).as_deref(),
        Some("xlink")
    );
    assert_eq!(value, "/svg");

    // The element itself is in the SVG namespace, so `is_html_element_in_html_document`
    // is answerable.
    assert_eq!(
        svg_a.as_element().expect("element").name().ns.to_string(),
        "http://www.w3.org/2000/svg"
    );

    // HTML subtree: no namespace, colon kept in the local name.
    let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
    let (name, value) = p.attrs().nth(0).expect("one attribute");
    assert_eq!(name.ns.to_string(), "");
    assert_eq!(name.local.to_string(), "xml:lang");
    assert_eq!(value, "en");
    assert_eq!(p.attr("xml:lang"), Some("en"));
}

#[test]
fn id_and_class_matching_covers_both_case_sensitivities() {
    let doc = doc();
    let wrapper = by_id(&doc, "Wrapper");
    let element = wrapper.as_element().expect("an element");

    for mode in [
        CaseSensitivity::CaseSensitive,
        CaseSensitivity::AsciiCaseInsensitive,
    ] {
        let id_matches = match mode {
            CaseSensitivity::CaseSensitive => element.id() == Some("wrapper"),
            CaseSensitivity::AsciiCaseInsensitive => element
                .id()
                .is_some_and(|id| id.eq_ignore_ascii_case("wrapper")),
        };
        assert_eq!(id_matches, mode == CaseSensitivity::AsciiCaseInsensitive);
    }

    // has_class, both modes, straight off the precomputed index.
    assert!(element.has_class("Card"));
    assert!(!element.has_class("card"));
    assert!(element.has_class_ignore_ascii_case("card"));
    assert_eq!(element.classes().len(), 2, "{:?}", element.classes());
}

#[test]
fn structural_pseudo_classes_have_everything_they_need() {
    let doc = doc();
    let wrapper = by_id(&doc, "Wrapper");

    // :root — note this is the <html> element, not the arena root.
    let html = doc.root_element().expect("<html>");
    assert!(html.is_root_element());
    assert!(!wrapper.is_root_element());

    // combinators
    let first_a = wrapper.first_element_child().expect("first <a>");
    assert_eq!(first_a.tag(), "a");
    assert!(first_a.prev_element_sibling().is_none());
    assert_eq!(first_a.next_element_sibling().map(|n| n.tag()), Some("a"));
    assert_eq!(wrapper.parent_element().map(|n| n.tag()), Some("body"));

    // :nth-child / :nth-of-type
    let p = doc.elements().find(|e| e.tag() == "p").expect("a <p>");
    assert_eq!(p.nth_of_type(), 1);
    assert_eq!(
        doc.elements()
            .filter(|e| e.tag() == "a")
            .nth(1)
            .map(|a| a.nth_of_type()),
        Some(2)
    );

    // :empty — no element children and no non-zero-length text node.
    let spans: Vec<_> = doc.elements().filter(|e| e.tag() == "span").collect();
    let is_empty = |n: &NodeRef<'_>| {
        n.element_children().next().is_none()
            && n.children().all(|c| c.as_text().is_none_or(str::is_empty))
    };
    assert!(is_empty(&spans[0]), "<span></span> is :empty");
    assert!(!is_empty(&spans[1]), "<span>text</span> is not :empty");

    // Caveat for unit 5: the default `remove_blank_text` (Python passes
    // `remove_blank_text=True` to lxml's HTMLParser — parser.py:157) implements
    // libxml2's `areBlanks()`, so whether a whitespace-only text node survives
    // depends on the content model around it, and so therefore does `:empty`.
    // `<span> </span>` keeps its space — `span` admits character data — and is
    // *not* `:empty`, in the reference implementation and in a browser alike. `<ul> </ul>` loses
    // it and is.
    for (html, empty) in [
        ("<span> </span>", false),
        ("<span>\u{a0}</span>", false),
        ("<ul> </ul>", true),
        ("<table> </table>", true),
    ] {
        let doc = Document::parse(html);
        let el = doc
            .elements()
            .find(|e| e.tag() != "html" && e.tag() != "body" && e.tag() != "head")
            .expect("the element under test");
        assert_eq!(is_empty(&el), empty, "{html} :empty");
        // Turning the pass off always keeps the blank, so nothing is `:empty`.
        let kept = Document::parse_with(
            html,
            &browser_dom::ParseOptions::new().remove_blank_text(false),
        );
        let el = kept
            .elements()
            .find(|e| e.tag() != "html" && e.tag() != "body" && e.tag() != "head")
            .expect("the element under test");
        assert!(!is_empty(&el), "{html} with remove_blank_text off");
    }

    // :link
    let a_with_href = doc
        .elements()
        .find(|e| e.tag() == "a" && e.attr("href").is_some())
        .expect("a linked <a>");
    assert_eq!(a_with_href.attr("href"), Some("/one"));
    let a_without = doc
        .elements()
        .find(|e| e.tag() == "a" && e.attr("href").is_none());
    assert!(a_without.is_some(), ":link must not match a bare <a>");
}

/// The reference implementation's `::text` / `::attr()` pseudo-elements and cssselect's `:contains()`
/// read the tree, not the selector engine — this is the data they read.
#[test]
fn the_ports_own_extensions_are_backed_by_the_tree() {
    let doc = doc();
    let first_a = doc.elements().find(|e| e.tag() == "a").expect("an <a>");

    assert_eq!(first_a.text_content(), "one"); // ::text
    assert_eq!(first_a.attr("href"), Some("/one")); // ::attr(href)
    assert!(by_id(&doc, "Wrapper").text_content().contains("no href")); // :contains()
}
