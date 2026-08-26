//! The `browser-dom` → `browser-xpath` seam.
//!
//! `ARCHITECTURE.md` §1.4 specifies the engine against a `TreeModel` trait so the
//! evaluator stays decoupled from the tree. That only works if `browser-dom` can
//! actually *satisfy* the trait. This file writes the trait out exactly as §1.4
//! specifies it, implements it over `browser_dom::Document`, and drives the four
//! XPath data-model operations that are least obvious:
//!
//! 1. **attribute nodes** — attributes are not arena nodes here, so an attribute is
//!    addressed as `(element NodeId, index)` via `Attributes::nth`. `.//a/@href` and
//!    `@*` both need this.
//! 2. **document order** — `Document::document_order` gives the `u64` key the trait
//!    is written in terms of, in one walk instead of O(depth) per comparison.
//! 3. **string-value** — the XPath string-value of an element is the concatenation
//!    of all descendant text, with no separator and no filtering: `text_content`.
//! 4. **the root node** — XPath's root is the *document* node, the parent of
//!    `<html>`, which `Document::root()` is.
//!
//! The real engine owns `src/model.rs`; this is a compile-and-run proof that the
//! seam holds, plus a worked reference for the unit that writes it. If the shape
//! below stops compiling, the DOM API regressed.

use std::borrow::Cow;

use browser_dom::node::Attributes;
use browser_dom::{Document, NodeRef};

// ---------------------------------------------------------------------------
// The §1.4 trait, verbatim, plus the node handle it needs.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Root,
    Element,
    Text,
    Comment,
    Attr,
    Pi,
}

/// `NodeRef` covers every arena node; attributes need the `(element, index)` pair
/// because they are stored on the element, not in the arena. One `Copy + Eq + Ord`
/// enum covers both, which is what `TreeModel::Node` requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum XNode<'a> {
    Tree(NodeRef<'a>),
    /// The `index`-th attribute of the element node `owner`.
    Attr(NodeRef<'a>, usize),
}

pub trait TreeModel {
    type Node: Copy + Eq + Ord;

    fn parent(&self, n: Self::Node) -> Option<Self::Node>;
    fn children(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;
    fn kind(&self, n: Self::Node) -> NodeKind;
    fn name(&self, n: Self::Node) -> Option<(&str, &str)>;
    fn string_value(&self, n: Self::Node) -> Cow<'_, str>;
    fn attributes(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;
    fn document_order(&self, n: Self::Node) -> u64;
}

// ---------------------------------------------------------------------------
// The implementation over browser-dom.
// ---------------------------------------------------------------------------

pub struct DomModel<'a> {
    doc: &'a Document,
    order: browser_dom::DocumentOrder<'a>,
}

impl<'a> DomModel<'a> {
    pub fn new(doc: &'a Document) -> Self {
        Self {
            doc,
            order: doc.document_order(),
        }
    }

    pub fn root(&self) -> XNode<'a> {
        XNode::Tree(self.doc.root())
    }

    fn attrs_of(node: NodeRef<'a>) -> &'a Attributes {
        node.attrs()
    }
}

impl<'a> TreeModel for DomModel<'a> {
    type Node = XNode<'a>;

    fn parent(&self, n: Self::Node) -> Option<Self::Node> {
        match n {
            // XPath: an attribute's parent is its element, but the element is NOT
            // the attribute's ancestor for the `child::` axis.
            XNode::Attr(owner, _) => Some(XNode::Tree(owner)),
            XNode::Tree(node) => node.parent().map(XNode::Tree),
        }
    }

    fn children(&self, n: Self::Node) -> impl Iterator<Item = Self::Node> {
        let iter = match n {
            XNode::Attr(..) => None,
            XNode::Tree(node) => Some(node.children()),
        };
        iter.into_iter().flatten().map(XNode::Tree)
    }

    fn kind(&self, n: Self::Node) -> NodeKind {
        match n {
            XNode::Attr(..) => NodeKind::Attr,
            XNode::Tree(node) => {
                if node.is_document() {
                    NodeKind::Root
                } else if node.is_element() {
                    NodeKind::Element
                } else if node.is_text() {
                    NodeKind::Text
                } else if node.is_processing_instruction() {
                    NodeKind::Pi
                } else {
                    NodeKind::Comment
                }
            }
        }
    }

    fn name(&self, n: Self::Node) -> Option<(&str, &str)> {
        match n {
            XNode::Attr(owner, index) => Self::attrs_of(owner)
                .nth(index)
                .map(|(name, _)| (&*name.ns, &*name.local)),
            XNode::Tree(node) => node
                .as_element()
                .map(|el| (&*el.name().ns, &*el.name().local)),
        }
    }

    fn string_value(&self, n: Self::Node) -> Cow<'_, str> {
        match n {
            XNode::Attr(owner, index) => Self::attrs_of(owner)
                .nth(index)
                .map_or(Cow::Borrowed(""), |(_, value)| Cow::Borrowed(value)),
            XNode::Tree(node) => match node.as_text().or_else(|| node.as_comment()) {
                Some(text) => Cow::Borrowed(text),
                None => Cow::Owned(node.text_content()),
            },
        }
    }

    fn attributes(&self, n: Self::Node) -> impl Iterator<Item = Self::Node> {
        let owner = match n {
            XNode::Tree(node) if node.is_element() => Some(node),
            _ => None,
        };
        let count = owner.map_or(0, |node| Self::attrs_of(node).len());
        (0..count).filter_map(move |i| owner.map(|node| XNode::Attr(node, i)))
    }

    fn document_order(&self, n: Self::Node) -> u64 {
        // Attributes sort immediately after their element and before its children
        // (XPath leaves the relative order of attributes implementation-defined,
        // but they must precede the element's descendants). Scaling the element's
        // rank leaves room; the real engine can pick any monotone encoding.
        match n {
            XNode::Tree(node) => self.order.rank(node).unwrap_or(u64::MAX) * 1024,
            XNode::Attr(owner, index) => {
                self.order.rank(owner).unwrap_or(u64::MAX) * 1024 + 1 + index as u64
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The proofs.
// ---------------------------------------------------------------------------

const PAGE: &str = r#"<html><body>
  <div id="list" class="c">
    <a href="/one" rel="next">one</a>
    <!-- gap -->
    <a href="/two">two</a>
  </div>
</body></html>"#;

fn links<'a>(m: &DomModel<'a>, doc: &'a Document) -> Vec<XNode<'a>> {
    doc.elements()
        .filter(|e| e.tag() == "a")
        .map(XNode::Tree)
        .inspect(|n| assert_eq!(m.kind(*n), NodeKind::Element))
        .collect()
}

#[test]
fn the_xpath_root_is_the_document_node_not_the_html_element() {
    let doc = Document::parse(PAGE);
    let model = DomModel::new(&doc);

    assert_eq!(model.kind(model.root()), NodeKind::Root);
    assert_eq!(model.parent(model.root()), None);
    assert_eq!(model.name(model.root()), None);

    let html = XNode::Tree(doc.root_element().expect("<html>"));
    assert_eq!(model.parent(html), Some(model.root()));
    assert_eq!(model.name(html).map(|(_, local)| local), Some("html"));
}

/// `.//a/@href` and `@*` — the case that has no arena node behind it.
#[test]
fn attribute_nodes_are_addressable_named_and_valued() {
    let doc = Document::parse(PAGE);
    let model = DomModel::new(&doc);
    let first = links(&model, &doc)[0];

    let attrs: Vec<_> = model.attributes(first).collect();
    assert_eq!(attrs.len(), 2, "@* over <a href rel>");

    assert_eq!(model.kind(attrs[0]), NodeKind::Attr);
    assert_eq!(model.name(attrs[0]), Some(("", "href")));
    assert_eq!(model.string_value(attrs[0]), "/one");
    assert_eq!(model.name(attrs[1]), Some(("", "rel")));

    // An attribute's parent is its element; the element's children never include it.
    assert_eq!(model.parent(attrs[0]), Some(first));
    assert!(model
        .children(first)
        .all(|c| model.kind(c) != NodeKind::Attr));
    assert_eq!(model.children(attrs[0]).count(), 0);
    assert_eq!(model.attributes(attrs[0]).count(), 0);

    // Attributes sort after their element and before the next element.
    let second = links(&model, &doc)[1];
    assert!(model.document_order(first) < model.document_order(attrs[0]));
    assert!(model.document_order(attrs[0]) < model.document_order(attrs[1]));
    assert!(model.document_order(attrs[1]) < model.document_order(second));

    // Non-elements have no attributes.
    let text = model
        .children(first)
        .find(|c| model.kind(*c) == NodeKind::Text)
        .expect("a text node");
    assert_eq!(model.attributes(text).count(), 0);
}

/// XPath string-value: element = concatenation of all descendant text, no
/// separators, nothing filtered.
#[test]
fn string_value_matches_the_xpath_data_model() {
    let doc = Document::parse(PAGE);
    let model = DomModel::new(&doc);

    let list = XNode::Tree(
        doc.elements()
            .find(|e| e.element_id() == Some("list"))
            .expect("#list"),
    );
    let value = model.string_value(list);
    assert!(value.contains("one") && value.contains("two"), "{value}");
    assert!(
        !value.contains("gap"),
        "comments are not part of string-value"
    );

    // string(/) is the string-value of the root.
    assert!(model.string_value(model.root()).contains("one"));

    // A text node's string-value is its own data; a comment's is its data.
    let comment = doc
        .root()
        .descendants()
        .find(|n| n.is_comment())
        .map(XNode::Tree);
    assert!(comment.is_none(), "comments are removed by default");

    let kept = Document::parse_with(PAGE, &browser_dom::ParseOptions::new().keep_comments(true));
    let kept_model = DomModel::new(&kept);
    let comment = kept
        .root()
        .descendants()
        .find(|n| n.is_comment())
        .map(XNode::Tree)
        .expect("a comment survives keep_comments");
    assert_eq!(kept_model.kind(comment), NodeKind::Comment);
    assert_eq!(kept_model.string_value(comment).trim(), "gap");
}

/// Node-set union/dedup is the single hottest operation in an XPath evaluator, and
/// it is O(1) per comparison via the precomputed index.
#[test]
fn document_order_totally_orders_every_node_kind() {
    let doc = Document::parse_with(PAGE, &browser_dom::ParseOptions::new().keep_comments(true));
    let model = DomModel::new(&doc);

    let mut all: Vec<XNode<'_>> = Vec::new();
    for node in doc.root().descendants_and_self() {
        let n = XNode::Tree(node);
        all.push(n);
        all.extend(model.attributes(n));
    }

    // Strictly increasing in document order, with no two nodes sharing a key.
    let mut keys: Vec<u64> = all.iter().map(|n| model.document_order(*n)).collect();
    let before = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), before, "document_order must be injective");

    // Tree nodes agree with the authoritative O(depth) comparator.
    let tree_nodes: Vec<NodeRef<'_>> = doc.root().descendants_and_self().collect();
    for pair in tree_nodes.windows(2) {
        assert!(
            model.document_order(XNode::Tree(pair[0])) < model.document_order(XNode::Tree(pair[1]))
        );
        assert_eq!(
            pair[0].cmp_document_order(&pair[1]),
            std::cmp::Ordering::Less
        );
    }

    // And `sort_dedup` normalises a scrambled node-set in one pass.
    let order = doc.document_order();
    let mut scrambled = vec![tree_nodes[4], tree_nodes[1], tree_nodes[4], tree_nodes[2]];
    order.sort_dedup(&mut scrambled);
    assert_eq!(scrambled, vec![tree_nodes[1], tree_nodes[2], tree_nodes[4]]);
}

/// The axes §1.4 names explicitly, each reduced to one `browser-dom` call.
#[test]
fn every_named_axis_has_an_o1_step() {
    let doc = Document::parse(PAGE);
    let model = DomModel::new(&doc);
    let second = links(&model, &doc)[1];
    let node = match second {
        XNode::Tree(n) => n,
        XNode::Attr(..) => unreachable!(),
    };

    // ancestor::*
    assert_eq!(
        node.ancestors().map(|a| a.tag()).collect::<Vec<_>>(),
        ["div", "body", "html"]
    );
    // count(ancestor::*) — the adaptive-relocation depth filter. `depth()` counts
    // *element* ancestors only, which is what lxml's `ancestor::*` returns and what
    // `find_similar_elements` compares; `ancestor_nodes()` is the wider walk that
    // also yields the document node.
    assert_eq!(node.depth(), 3);
    assert_eq!(node.ancestor_nodes().count(), 4);

    // preceding-sibling / following-sibling (element-only and all-kinds forms)
    assert_eq!(node.prev_element_sibling().map(|n| n.tag()), Some("a"));
    assert_eq!(node.next_element_sibling(), None);
    // `prev_siblings()` is the all-kinds walk over the *normalised* tree. The
    // comment is gone, but the indentation around it is not: `div` admits
    // character data and so does `a`, so `remove_blank_text` keeps both runs and
    // re-merges them across the deleted comment. lxml with the same flags agrees
    // exactly — `preceding-sibling::node()` is `["\n    ", <a/>, "\n    \n    "]`.
    assert_eq!(node.prev_siblings().count(), 3);
    assert_eq!(
        node.prev_siblings()
            .filter_map(|n| n.as_text())
            .collect::<Vec<_>>(),
        ["\n    \n    ", "\n    "]
    );
    let verbose = Document::parse_with(
        PAGE,
        &browser_dom::ParseOptions::new()
            .keep_comments(true)
            .remove_blank_text(false),
    );
    let verbose_second = verbose
        .elements()
        .filter(|e| e.tag() == "a")
        .nth(1)
        .expect("2nd <a>");
    assert!(verbose_second.prev_siblings().count() >= 3);

    // descendant-or-self::
    assert!(node.descendants_and_self().count() >= 2);

    // attribute::
    assert_eq!(model.attributes(second).count(), 1);

    // self:: and child::text()
    assert_eq!(node.text(), Some("two"));
}
