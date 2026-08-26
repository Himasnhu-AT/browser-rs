//! Matching a compiled `CssSelector` against the tree, including `Tail` extraction for
//! `::text` / `::attr(name)`.
//!
//! # The shape of a result
//!
//! lxml's XPath returns a heterogeneous node-set: `descendant-or-self::p` gives
//! elements, `…/text()` gives `_ElementUnicodeResult` smart strings, `…/@href`
//! gives attribute values (also smart strings). `parser.md` §2.1 calls the last
//! two "text nodes" and degrades most of `Selector`'s API on them.
//!
//! [`Match`] is that node-set entry: the element that satisfied the compound
//! selector, plus whatever the [`Tail`] resolved to.
//!
//! # Scoping
//!
//! Python prefixes every translated selector with `descendant-or-self::`
//! (`parser.md` §6.1), rooted at the node the query was issued on. So this
//! module enumerates `root` and its descendants as candidates, and clips each
//! candidate's ancestor chain at `root` (see [`crate::impl_selector`]).

use std::collections::HashSet;

use browser_dom::NodeRef;
use markup5ever::LocalName;
use selectors::context::{
    MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode, SelectorCaches,
};
use selectors::matching::{matches_selector, MatchingContext};

use crate::impl_selector::{unprefixed_attr, BrowserImpl, ElementRef};
use crate::parse::SubSelector;
use crate::pseudo::Tail;

/// Which lxml attribute a `::text` result came from.
///
/// lxml stores an element's leading text in `element.text` and the text that
/// *follows* a child element in that child's `element.tail`. `/text()` returns
/// both; this is how a caller tells them apart, which
/// `browser-parser`'s `TextKind` needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextRole {
    /// The text node is the element's first child — lxml's `element.text`.
    Text,
    /// The text node follows a child element — lxml's `previous_sibling.tail`.
    Tail,
}

/// What a single match yielded, once the trailing pseudo-element was applied.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MatchValue<'a> {
    /// No pseudo-element: the element itself is the result.
    Element,
    /// `::text`: one direct text-node child of the matched element.
    Text {
        /// The text node.
        node: NodeRef<'a>,
        /// Whether it is lxml's `.text` or a child's `.tail`.
        role: TextRole,
    },
    /// `::attr(name)`: the value of the matched element's `name` attribute.
    Attr {
        /// The attribute's local name, as written in the selector.
        name: LocalName,
        /// The attribute's value, borrowed from the document.
        value: &'a str,
    },
}

/// One entry of the result node-set.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Match<'a> {
    element: NodeRef<'a>,
    value: MatchValue<'a>,
}

impl<'a> Match<'a> {
    /// The element that satisfied the selector — the *owner* of a `::text` or
    /// `::attr()` result, and the result itself otherwise.
    #[must_use]
    pub fn element(&self) -> NodeRef<'a> {
        self.element
    }

    /// What the tail resolved to.
    #[must_use]
    pub fn value(&self) -> &MatchValue<'a> {
        &self.value
    }

    /// The tail that produced this match.
    #[must_use]
    pub fn tail(&self) -> Tail {
        match &self.value {
            MatchValue::Element => Tail::Element,
            MatchValue::Text { .. } => Tail::Text,
            MatchValue::Attr { name, .. } => Tail::Attr(name.clone()),
        }
    }

    /// The DOM node this match points at: the text node for `::text`, and the
    /// element for everything else (an attribute is not a node in this tree).
    #[must_use]
    pub fn node(&self) -> NodeRef<'a> {
        match &self.value {
            MatchValue::Text { node, .. } => *node,
            MatchValue::Element | MatchValue::Attr { .. } => self.element,
        }
    }

    /// The string value for a `::text` / `::attr()` match; `None` for an
    /// element match.
    #[must_use]
    pub fn as_str(&self) -> Option<&'a str> {
        match &self.value {
            MatchValue::Element => None,
            MatchValue::Text { node, .. } => node.as_text(),
            MatchValue::Attr { value, .. } => Some(value),
        }
    }

    /// Python's `Selector._is_text_node`: `True` for both `::text` and
    /// `::attr()` results, because lxml represents both as
    /// `_ElementUnicodeResult` (`parser.md` §2.1).
    #[must_use]
    pub fn is_text_node(&self) -> bool {
        !matches!(self.value, MatchValue::Element)
    }

    /// `Some` for a `::text` match only.
    #[must_use]
    pub fn text_role(&self) -> Option<TextRole> {
        match &self.value {
            MatchValue::Text { role, .. } => Some(*role),
            _ => None,
        }
    }
}

/// The matching mode a sub-selector needs.
///
/// `selectors` will only skip a trailing pseudo-element when asked to; in
/// `MatchingMode::Normal` it would try to walk from the pseudo-element to its
/// "originating element" instead.
fn mode_for(part: &SubSelector) -> MatchingMode {
    if part.matcher().has_pseudo_element() {
        MatchingMode::ForStatelessPseudoElement
    } else {
        MatchingMode::Normal
    }
}

fn context<'a>(
    part: &SubSelector,
    caches: &'a mut SelectorCaches,
    scope: Option<NodeRef<'_>>,
) -> MatchingContext<'a, BrowserImpl> {
    let mut ctx = MatchingContext::new(
        mode_for(part),
        // No ancestor bloom filter: building one costs more than it saves for
        // the one-shot queries this crate serves, and `add_element_unique_hashes`
        // is a no-op accordingly.
        None,
        caches,
        // cssselect is case-sensitive about ids and classes; that is
        // `NoQuirks`.
        QuirksMode::NoQuirks,
        // The tree is immutable during matching, so there is no style
        // invalidation bookkeeping to record.
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    // cssselect translates `:scope` to `descendant-or-self::*[position() = 1]`,
    // i.e. the node the query was issued on. When that node is not an element
    // (the document root), leaving this `None` makes `:scope` fall back to the
    // root element, which is the same node lxml would pick.
    ctx.scope_element = scope
        .and_then(ElementRef::new)
        .map(|e| selectors::Element::opaque(&e));
    ctx
}

/// Does `element` match `part`, with the ancestor chain optionally clipped?
#[must_use]
pub fn matches_one(part: &SubSelector, element: NodeRef<'_>, scope: Option<NodeRef<'_>>) -> bool {
    let Some(eref) = ElementRef::with_scope(element, scope) else {
        return false;
    };
    let mut caches = SelectorCaches::default();
    part_matches(part, &eref, &mut caches, scope, scope == Some(element))
}

/// Try every matcher the part carries; `at_scope_root` suppresses the ones with
/// a top-level combinator.
///
/// A part usually has exactly one matcher. It has two when a trailing bare `*`
/// was folded into the pseudo-element — see [`SubSelector::subtree_head`].
///
/// `at_scope_root` is the query node itself. lxml prefixes every selector with
/// `descendant-or-self::`, so a combinator's anchor (an ancestor for ` `/`>`, a
/// preceding sibling for `+`/`~`) has to be inside the queried subtree, and for
/// the query node no such anchor can exist. Ancestors are already clipped by
/// [`ElementRef`]; siblings deliberately are not, because cssselect counts the
/// document's *real* siblings in `:first-child` — so without this check
/// `nav.css("* + *")` would return the `<nav>` itself.
fn part_matches(
    part: &SubSelector,
    eref: &ElementRef<'_>,
    caches: &mut SelectorCaches,
    scope: Option<NodeRef<'_>>,
    at_scope_root: bool,
) -> bool {
    part.matchers().any(|(matcher, has_combinator)| {
        if at_scope_root && has_combinator {
            return false;
        }
        let mut ctx = context(part, caches, scope);
        matches_selector(matcher, 0, None, eref, &mut ctx)
    })
}

/// The XPath union `A | B | …`: document order, duplicates removed.
///
/// # Why the walk visits text nodes, not just elements
///
/// `p::text` is `descendant-or-self::p/text()`, and an XPath node-set comes back
/// in **document order of the yielded nodes**. For
/// `<div>a<b>x</b>c</div>`, `*::text` is `a, x, c` — the `<div>`'s two text
/// children are *not* adjacent. Emitting per matched element would give
/// `a, c, x`, so the walk iterates every node and asks a text node's parent
/// whether it matched. Attribute results are emitted at their element, which is
/// where XPath puts them (after the element, before its children).
fn select_union<'a>(parts: &[SubSelector], root: NodeRef<'a>, first_only: bool) -> Vec<Match<'a>> {
    let mut caches = SelectorCaches::default();
    let mut out: Vec<Match<'a>> = Vec::new();
    let wants_text = parts.iter().any(|p| matches!(p.tail(), Tail::Text));
    // Elements for which at least one `::text` part matched. Populated as the
    // pre-order walk passes the element, read when it later reaches its text
    // children.
    let mut text_owners: HashSet<NodeRef<'a>> = HashSet::new();

    for node in root.descendants_and_self() {
        if let Some(eref) = ElementRef::with_scope(node, Some(root)) {
            let mut matched_text = false;
            // Two passes so a mixed group emits `div, div::attr(class)` in
            // XPath document order: the element, then its attributes.
            for wanted_attr in [false, true] {
                for part in parts {
                    if matches!(part.tail(), Tail::Attr(_)) != wanted_attr {
                        continue;
                    }
                    let hit = part_matches(part, &eref, &mut caches, Some(root), node == root);
                    if !hit {
                        continue;
                    }
                    match part.tail() {
                        Tail::Text => matched_text = true,
                        Tail::Element => push_unique(
                            &mut out,
                            Match {
                                element: node,
                                value: MatchValue::Element,
                            },
                        ),
                        Tail::Attr(name) => {
                            // `::attr(x)` is lxml's `/@x`, and an attribute
                            // written `xlink:href` is *not* `href` there — see
                            // `impl_selector::unprefixed_attr`.
                            if let Some(value) = unprefixed_attr(node, name) {
                                push_unique(
                                    &mut out,
                                    Match {
                                        element: node,
                                        value: MatchValue::Attr {
                                            name: name.clone(),
                                            value,
                                        },
                                    },
                                );
                            }
                        }
                    }
                    if first_only && !out.is_empty() {
                        return out;
                    }
                }
            }
            if matched_text {
                text_owners.insert(node);
            }
        } else if wants_text && node.is_text() {
            let Some(parent) = node.parent() else {
                continue;
            };
            if !text_owners.contains(&parent) {
                continue;
            }
            push_unique(
                &mut out,
                Match {
                    element: parent,
                    value: MatchValue::Text {
                        node,
                        role: if node.prev_sibling().is_none() {
                            TextRole::Text
                        } else {
                            TextRole::Tail
                        },
                    },
                },
            );
            if first_only && !out.is_empty() {
                return out;
            }
        }
    }
    out
}

/// Append unless an identical entry is already there.
///
/// A selector group is an XPath union, and a union is a set: `css("p, p#x")`
/// returns each `<p>` once. Duplicates can only come from *different parts*
/// agreeing on the same element, so the scan is over the tail of `out` that
/// belongs to the element currently being visited — bounded by the number of
/// parts, not by the result size.
fn push_unique<'a>(out: &mut Vec<Match<'a>>, candidate: Match<'a>) {
    let already = out
        .iter()
        .rev()
        .take_while(|m| m.element == candidate.element)
        .any(|m| *m == candidate);
    if !already {
        out.push(candidate);
    }
}

/// Everything one sub-selector selects under `root`, in document order.
#[must_use]
pub fn select_one<'a>(part: &SubSelector, root: NodeRef<'a>) -> Vec<Match<'a>> {
    select_union(std::slice::from_ref(part), root, false)
}

/// The XPath union of every part: document order, duplicates removed.
#[must_use]
pub fn select_group<'a>(parts: &[SubSelector], root: NodeRef<'a>) -> Vec<Match<'a>> {
    select_union(parts, root, false)
}

/// [`select_group`], stopping at the first entry.
#[must_use]
pub fn select_group_first<'a>(parts: &[SubSelector], root: NodeRef<'a>) -> Option<Match<'a>> {
    select_union(parts, root, true).into_iter().next()
}

/// Only the elements that matched, ignoring every tail; document order, no
/// duplicates.
#[must_use]
pub fn select_group_elements<'a>(parts: &[SubSelector], root: NodeRef<'a>) -> Vec<NodeRef<'a>> {
    let mut caches = SelectorCaches::default();
    let mut out = Vec::new();
    for candidate in root.descendants_and_self() {
        let Some(eref) = ElementRef::with_scope(candidate, Some(root)) else {
            continue;
        };
        let matched = parts.iter().any(|part| {
            let mut ctx = context(part, &mut caches, Some(root));
            matches_selector(part.matcher(), 0, None, &eref, &mut ctx)
        });
        if matched {
            out.push(candidate);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_selector;
    use browser_dom::Document;

    const PAGE: &str = r#"<html><body>
        <main id="products">
          <div class="product-list">
            <article class="product" data-id="1"><span class="price">$10.99</span><a href="/p/1">One</a></article>
            <article class="product" data-id="2"><span class="price">$20.99</span><a href="/p/2">Two</a></article>
            <article class="product out" data-id="3"><span class="price">Out of stock</span></article>
          </div>
        </main>
        <p id="p1">alpha<b>bold</b>beta</p>
        <p id="p2">gamma</p>
      </body></html>"#;

    fn doc() -> Document {
        Document::parse(PAGE)
    }

    fn strings(matches: &[Match<'_>]) -> Vec<String> {
        matches
            .iter()
            .map(|m| {
                m.as_str()
                    .map_or_else(|| m.element().tag().to_owned(), ToOwned::to_owned)
            })
            .collect()
    }

    #[test]
    fn element_selection_is_document_order() {
        let doc = doc();
        let g = parse_selector("main #products .product-list article.product")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 0, "#products is itself <main>, not a descendant");

        let g = parse_selector("#products .product-list article.product")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 3);
        assert!(g.iter().all(|m| !m.is_text_node()));
        assert_eq!(
            g.iter()
                .map(|m| m.element().attr("data-id"))
                .collect::<Vec<_>>(),
            vec![Some("1"), Some("2"), Some("3")]
        );
    }

    #[test]
    fn contains_and_not_compose() {
        let doc = doc();
        let g = parse_selector(r#"article.product:not(:contains("Out of stock"))"#)
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn text_tail_yields_each_direct_text_child() {
        let doc = doc();
        let g = parse_selector("p::text")
            .expect("parses")
            .select(doc.root());
        assert_eq!(strings(&g), vec!["alpha", "beta", "gamma"]);
        assert_eq!(
            g.iter().map(|m| m.text_role()).collect::<Vec<_>>(),
            vec![
                Some(TextRole::Text),
                Some(TextRole::Tail),
                Some(TextRole::Text)
            ]
        );
        assert!(g.iter().all(Match::is_text_node));
        assert_eq!(g[1].element().tag(), "p", "the tail belongs to the <p>");
        assert!(g[1].node().is_text());
    }

    #[test]
    fn universal_text_is_every_text_node() {
        let doc = Document::parse("<div>a<b>x</b>c</div>");
        let g = parse_selector("*::text")
            .expect("parses")
            .select(doc.root());
        assert_eq!(strings(&g), vec!["a", "x", "c"]);
    }

    #[test]
    fn attr_tail_yields_values_and_skips_missing() {
        let doc = doc();
        let g = parse_selector("a::attr(href)")
            .expect("parses")
            .select(doc.root());
        assert_eq!(strings(&g), vec!["/p/1", "/p/2"]);
        assert_eq!(g[0].tail(), Tail::Attr(LocalName::from("href")));

        let g = parse_selector("article::attr(nope)")
            .expect("parses")
            .select(doc.root());
        assert!(g.is_empty());
    }

    #[test]
    fn a_group_is_a_deduplicated_union_in_document_order() {
        let doc = doc();
        let g = parse_selector("#p2, #p1")
            .expect("parses")
            .select(doc.root());
        assert_eq!(
            g.iter().map(|m| m.element().attr("id")).collect::<Vec<_>>(),
            vec![Some("p1"), Some("p2")],
            "union results come back in document order, not selector order"
        );

        let g = parse_selector("p, p#p1")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 2, "the union deduplicates");

        // ...but the adaptive path does not.
        let per_part = parse_selector("p, p#p1")
            .expect("parses")
            .select_per_part(doc.root());
        assert_eq!(per_part.len(), 2);
        assert_eq!(per_part[0].len() + per_part[1].len(), 3);
    }

    #[test]
    fn mixed_tail_groups_work() {
        let doc = doc();
        let g = parse_selector("#p2::text, a::attr(href)")
            .expect("parses")
            .select(doc.root());
        assert_eq!(strings(&g), vec!["/p/1", "/p/2", "gamma"]);
    }

    #[test]
    fn the_query_node_itself_needs_a_combinator_free_selector() {
        // lxml prefixes every selector with `descendant-or-self::`, so a
        // combinator's anchor — an ancestor, or a preceding sibling — has to
        // live inside the queried subtree. For the query node itself no such
        // anchor can exist, and Python returns nothing. Ancestors are clipped
        // by `ElementRef`; siblings deliberately are not (cssselect counts
        // *real* siblings in `:first-child`), so this is checked explicitly.
        let doc = Document::parse("<p id='before'>b</p><nav id='n'><a>x</a><b>y</b></nav>");
        let nav = doc.elements().find(|e| e.tag() == "nav").expect("nav");

        let ids = |sel: &str| {
            parse_selector(sel)
                .expect("parses")
                .select(nav)
                .iter()
                .map(|m| m.element().tag().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids("* + *"),
            vec!["b"],
            "<nav> must not match via its own previous sibling"
        );
        assert_eq!(ids("* ~ *"), vec!["b"]);
        assert_eq!(ids("p + nav"), Vec::<String>::new());
        // …while a combinator-free selector still selects the query node, and
        // `:first-child` still sees the document's real siblings.
        assert_eq!(ids("nav"), vec!["nav"]);
        assert_eq!(
            ids("*:first-child"),
            vec!["a"],
            "<nav> is not a first child"
        );
        // A pseudo-element is not a combinator: `::attr()` on the query node
        // must still fire.
        assert_eq!(
            parse_selector("*::attr(id)")
                .expect("parses")
                .select(nav)
                .iter()
                .filter_map(Match::as_str)
                .collect::<Vec<_>>(),
            vec!["n"]
        );
    }

    #[test]
    fn a_trailing_bare_star_is_folded_into_the_pseudo_element() {
        // `reference/core/translator.py` rewrites `div *::text` to
        // `descendant-or-self::div/descendant-or-self::text()`, so it is every
        // text node under a `div` — the `div`'s own text included — and
        // `div *::attr(class)` reaches the `div`'s own `class`. Only a
        // *descendant* combinator followed by a bare `*` is folded.
        let doc = Document::parse(
            "<div class='d'>top<span class='s'>in</span><p class='p'>deep<b>b</b></p></div>",
        );
        let root = doc.root();
        let texts = |sel: &str| {
            parse_selector(sel)
                .expect("parses")
                .select(root)
                .iter()
                .filter_map(Match::as_str)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts("div *::text"), vec!["top", "in", "deep", "b"]);
        assert_eq!(texts("div ::text"), vec!["top", "in", "deep", "b"]);
        assert_eq!(texts("div *::attr(class)"), vec!["d", "s", "p"]);
        // A child combinator is left alone…
        assert_eq!(texts("div > *::text"), vec!["in", "deep"]);
        // …and a second bare `*` folds only once: `div * *::text` is the text
        // under each of the div's *children*, so the div's own "top" is out
        // while the children's own text is in.
        assert_eq!(texts("div * *::text"), vec!["in", "deep", "b"]);
        // The element tail is never folded.
        assert_eq!(
            parse_selector("div *")
                .expect("parses")
                .select(root)
                .iter()
                .map(|m| m.element().tag().to_owned())
                .collect::<Vec<_>>(),
            vec!["span", "p", "b"]
        );
    }

    #[test]
    fn attr_tails_ignore_prefixed_attributes() {
        // libxml2's HTML parser stores `xlink:href` under that literal name, so
        // lxml's `/@href` does not find it. html5ever splits the prefix off
        // into the XLink namespace, and a namespace-blind lookup would.
        let doc = Document::parse("<svg><a xlink:href='/s'><text>t</text></a></svg>");
        let g = parse_selector("a::attr(href)")
            .expect("parses")
            .select(doc.root());
        assert!(g.is_empty(), "xlink:href is not href");
        assert!(
            parse_selector(":link")
                .expect("parses")
                .select(doc.root())
                .is_empty(),
            "and so the <a> is not a :link either"
        );
    }

    #[test]
    fn selection_is_scoped_to_the_subtree() {
        let doc = Document::parse(
            "<div class='container'><section id='s'><p>in</p></section></div><p>out</p>",
        );
        let section = doc.elements().find(|e| e.tag() == "section").expect("s");

        // Browser `querySelectorAll` would find the <p>, because `.container`
        // is an ancestor. lxml's `descendant-or-self::` prefix does not.
        let g = parse_selector(".container p")
            .expect("parses")
            .select(section);
        assert!(g.is_empty(), "ancestors outside the scope must not count");

        let g = parse_selector("p").expect("parses").select(section);
        assert_eq!(g.len(), 1);

        // descendant-or-self: the root itself is a candidate.
        let g = parse_selector("section").expect("parses").select(section);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].element(), section);
    }

    #[test]
    fn scope_pseudo_class_is_the_context_node() {
        let doc = Document::parse("<div id='a'><div id='b'><span>x</span></div></div>");
        let b = doc
            .elements()
            .find(|e| e.element_id() == Some("b"))
            .expect("b");
        let g = parse_selector(":scope > span").expect("parses").select(b);
        assert_eq!(g.len(), 1);
        let g = parse_selector(":scope").expect("parses").select(b);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].element(), b);
    }

    #[test]
    fn matches_is_unscoped() {
        let doc = Document::parse("<div class='c'><p id='x'>hi</p></div>");
        let p = doc
            .elements()
            .find(|e| e.element_id() == Some("x"))
            .expect("p");
        let g = parse_selector(".c p").expect("parses");
        assert!(g.matches(p));
        assert!(!g.parts()[0].matches_scoped(p, p));
    }

    #[test]
    fn select_first_short_circuits() {
        let doc = doc();
        let g = parse_selector("article.product").expect("parses");
        let first = g.select_first(doc.root()).expect("a match");
        assert_eq!(first.element().attr("data-id"), Some("1"));
        assert!(parse_selector("nothing")
            .expect("parses")
            .select_first(doc.root())
            .is_none());
    }

    #[test]
    fn select_elements_drops_the_tail() {
        let doc = doc();
        let els = parse_selector("p::text")
            .expect("parses")
            .select_elements(doc.root());
        assert_eq!(els.len(), 2);
        assert!(els.iter().all(|e| e.tag() == "p"));
    }

    #[test]
    fn structural_pseudo_classes_use_the_real_tree() {
        let doc = doc();
        let g = parse_selector("article:first-child")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 1);
        let g = parse_selector("article:nth-of-type(2)")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g[0].element().attr("data-id"), Some("2"));
        let g = parse_selector(":root").expect("parses").select(doc.root());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].element().tag(), "html");
    }

    #[test]
    fn has_and_is_work() {
        let doc = doc();
        let g = parse_selector("article:has(a)")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 2);
        let g = parse_selector("article:has(> span.price)")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 3);
        let g = parse_selector("article:is(.out, [data-id='1'])")
            .expect("parses")
            .select(doc.root());
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn never_matching_pseudo_classes_never_match() {
        let doc = doc();
        for sel in ["a:hover", "a:visited", "a:active", "a:focus", "a:target"] {
            assert!(
                parse_selector(sel)
                    .expect("parses")
                    .select(doc.root())
                    .is_empty(),
                "{sel} must never match"
            );
        }
        let g = parse_selector("a:link").expect("parses").select(doc.root());
        assert_eq!(g.len(), 2, ":link is real, unlike cssselect's constant 0");
    }
}
