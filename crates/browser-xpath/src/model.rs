//! The [`TreeModel`] trait that decouples the engine from `browser-dom`, plus
//! [`DomModel`] — the implementation of it for [`browser_dom::Document`].
//!
//! `ARCHITECTURE.md` §1.4 specifies the engine against a trait so the evaluator is
//! never hard-wired to one tree. Everything in [`crate::eval`], [`crate::axes`] and
//! [`crate::functions`] is generic over `M: TreeModel`; only this module knows what
//! an `html5ever` node is.
//!
//! # The XPath data model over an html5ever tree
//!
//! | XPath node | here |
//! |---|---|
//! | root | [`NodeData::Document`][browser_dom::NodeData::Document] — the parent of `<html>` |
//! | element | an element arena node |
//! | text | a text arena node (CDATA kept by `keep_cdata` is a comment, as in `browser-dom`) |
//! | comment | a comment arena node |
//! | processing instruction | a PI arena node |
//! | attribute | **not an arena node**: the pair `(owner element, index)`, via [`browser_dom::Attributes::nth`] |
//! | namespace | **synthesised**: the in-scope `xmlns`/`xmlns:*` declarations plus the implicit `xml` prefix |
//!
//! [`XNode`] is the one `Copy + Eq + Ord` handle that covers all seven.

use std::borrow::Cow;

use browser_dom::node::Attributes;
use browser_dom::{Document, NodeRef};

/// `http://www.w3.org/XML/1998/namespace` — bound to the `xml` prefix in every
/// document, without a declaration.
pub const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// `http://www.w3.org/2000/xmlns/` — the namespace html5ever puts `xmlns`
/// attributes in inside foreign (SVG / MathML) content.
pub const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

/// `http://www.w3.org/1999/xhtml` — the namespace html5ever gives every HTML
/// element. [`DomModel`] reports it as the null namespace, because libxml2's HTML
/// parser — the reference, and the only parser the reference implementation uses — namespaces nothing,
/// so `namespace-uri()` on an HTML element answers `""` in lxml. Foreign (SVG /
/// MathML) content keeps its real URI.
pub const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// The seven node types of the XPath 1.0 data model.
///
/// The names match `ARCHITECTURE.md` §1.4 (`Element|Text|Comment|Attr|PI|Root`);
/// [`NodeKind::Namespace`] is the seventh, which §1.4's shorthand list omits but
/// the `namespace::` axis needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeKind {
    /// The document node — what `/` selects.
    Root,
    /// An element.
    Element,
    /// A text node.
    Text,
    /// A comment node.
    Comment,
    /// An attribute node.
    Attr,
    /// A namespace node.
    Namespace,
    /// A processing instruction.
    Pi,
}

impl NodeKind {
    /// The name XPath's `name()`/`local-name()` uses for diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Element => "element",
            Self::Text => "text",
            Self::Comment => "comment",
            Self::Attr => "attribute",
            Self::Namespace => "namespace",
            Self::Pi => "processing-instruction",
        }
    }
}

/// Everything the evaluator needs to know about a tree.
///
/// Written exactly as `ARCHITECTURE.md` §1.4 specifies it, plus three methods that
/// carry default bodies so an implementor only has to supply the seven core ones:
/// [`TreeModel::namespaces`] (empty by default — a tree without namespace
/// declarations simply has no namespace nodes), [`TreeModel::root_of`] (walk up) and
/// [`TreeModel::attribute_value`] (linear scan of [`TreeModel::attributes`]).
///
/// # Contract
///
/// * [`TreeModel::document_order`] must be **injective** and **monotone** in
///   document order over every node of the tree, with an element's namespace nodes
///   sorting after the element and before its attributes, and both sorting before
///   the element's children. Node-set de-duplication and ordering are built on it,
///   so a non-injective key silently loses nodes.
/// * [`TreeModel::children`] must never yield attribute or namespace nodes.
/// * [`TreeModel::parent`] of an attribute or namespace node is its element, even
///   though that element does not list it among its children.
/// * [`TreeModel::string_value`] is the XPath string-value: for an element or the
///   root, the concatenation of every descendant text node, with no separator and
///   nothing filtered out.
pub trait TreeModel {
    /// The node handle. `Copy` because the evaluator passes nodes by value
    /// everywhere; `Ord` so node-sets can be sorted without a model in hand.
    type Node: Copy + Eq + Ord;

    /// The parent, or `None` for the root.
    fn parent(&self, n: Self::Node) -> Option<Self::Node>;

    /// The child nodes, in document order. Never attributes or namespaces.
    fn children(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;

    /// Which of the seven node types `n` is.
    fn kind(&self, n: Self::Node) -> NodeKind;

    /// `(namespace-uri, local-name)`, or `None` for nodes that have no name
    /// (root, text, comment).
    fn name(&self, n: Self::Node) -> Option<(&str, &str)>;

    /// The XPath string-value of `n`.
    fn string_value(&self, n: Self::Node) -> Cow<'_, str>;

    /// The attribute nodes of an element, in document order. Empty for every
    /// other node type.
    fn attributes(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;

    /// A key that totally orders every node of the tree in document order.
    fn document_order(&self, n: Self::Node) -> u64;

    /// The namespace nodes in scope on an element.
    ///
    /// The default is "no namespace nodes", which is the right answer for a tree
    /// that does not track namespace declarations.
    fn namespaces(&self, _n: Self::Node) -> impl Iterator<Item = Self::Node> {
        std::iter::empty()
    }

    /// The document node `n` belongs to — what `/` selects.
    ///
    /// The default walks parents, which is O(depth) and correct for any tree.
    /// Named `root_of` rather than `root` so an implementor is free to offer its
    /// own zero-argument `root()` accessor, as [`DomModel`] does.
    fn root_of(&self, n: Self::Node) -> Self::Node {
        let mut current = n;
        while let Some(parent) = self.parent(current) {
            current = parent;
        }
        current
    }

    /// The value of the attribute of `n` whose local name is `local`.
    ///
    /// The default is a linear scan; implementors with an index should override it.
    /// Used by `id()` and `lang()`.
    fn attribute_value(&self, n: Self::Node, local: &str) -> Option<Cow<'_, str>> {
        let matched = self
            .attributes(n)
            .find(|a| self.name(*a).is_some_and(|(_, l)| l == local))?;
        Some(self.string_value(matched))
    }
}

// ---------------------------------------------------------------------------
// The browser-dom implementation.
// ---------------------------------------------------------------------------

/// Where a namespace node's binding was declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NsSource<'a> {
    /// The implicit `xml` prefix, bound in every document without a declaration.
    Xml,
    /// The `index`-th attribute of an ancestor-or-self element is the
    /// `xmlns`/`xmlns:*` declaration that bound this prefix.
    Decl(NodeRef<'a>, usize),
}

/// A node of the XPath data model over a [`Document`].
///
/// `NodeRef` covers every arena node. Attributes and namespaces are not arena
/// nodes, so they carry the element they hang off plus enough information to
/// resolve their name and value — which keeps the whole handle `Copy + Eq + Ord`,
/// as [`TreeModel::Node`] requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum XNode<'a> {
    /// An arena node: root, element, text, comment or processing instruction.
    Tree(NodeRef<'a>),
    /// The `index`-th attribute of the element `owner`.
    Attr(NodeRef<'a>, usize),
    /// A namespace node in scope on the element, and where it was declared.
    Namespace(NodeRef<'a>, NsSource<'a>),
}

impl<'a> XNode<'a> {
    /// Wrap an arena node.
    #[must_use]
    pub fn new(node: NodeRef<'a>) -> Self {
        Self::Tree(node)
    }

    /// The arena node this handle is anchored at: the node itself, or the owner
    /// element of an attribute / namespace node.
    #[must_use]
    pub fn anchor(&self) -> NodeRef<'a> {
        match *self {
            Self::Tree(node) | Self::Attr(node, _) | Self::Namespace(node, _) => node,
        }
    }

    /// The arena node, when this is one — `None` for attributes and namespaces.
    #[must_use]
    pub fn as_tree_node(&self) -> Option<NodeRef<'a>> {
        match *self {
            Self::Tree(node) => Some(node),
            _ => None,
        }
    }

    /// The element behind this handle, when it is one.
    ///
    /// This is what `browser-parser` wraps into a `Selector`.
    #[must_use]
    pub fn as_element(&self) -> Option<NodeRef<'a>> {
        match *self {
            Self::Tree(node) if node.is_element() => Some(node),
            _ => None,
        }
    }

    /// `(local name, value)` when this is an attribute node.
    #[must_use]
    pub fn as_attribute(&self) -> Option<(&'a str, &'a str)> {
        match *self {
            Self::Attr(owner, index) => owner
                .attrs()
                .nth(index)
                .map(|(name, value)| (&*name.local, value)),
            _ => None,
        }
    }

    /// The text of a text node.
    #[must_use]
    pub fn as_text(&self) -> Option<&'a str> {
        self.as_tree_node().and_then(|node| node.as_text())
    }

    /// Which of the seven node types this is.
    #[must_use]
    pub fn kind(&self) -> NodeKind {
        match *self {
            Self::Attr(..) => NodeKind::Attr,
            Self::Namespace(..) => NodeKind::Namespace,
            Self::Tree(node) => {
                if node.is_document() {
                    NodeKind::Root
                } else if node.is_element() {
                    NodeKind::Element
                } else if node.is_text() {
                    NodeKind::Text
                } else if node.is_processing_instruction() {
                    NodeKind::Pi
                } else {
                    // Comments and the doctype. `browser-dom` keeps a doctype
                    // node; XPath 1.0 has no doctype node type, and treating it as
                    // a comment keeps it out of `child::*` and `child::text()`,
                    // which is what lxml's tree does too.
                    NodeKind::Comment
                }
            }
        }
    }

    /// The XPath string-value, borrowed from the tree where possible.
    #[must_use]
    pub fn string_value(&self) -> Cow<'a, str> {
        match *self {
            Self::Attr(owner, index) => owner
                .attrs()
                .nth(index)
                .map_or(Cow::Borrowed(""), |(_, value)| Cow::Borrowed(value)),
            Self::Namespace(_, source) => Cow::Borrowed(namespace_uri_of(source)),
            Self::Tree(node) => match node.as_text().or_else(|| node.as_comment()) {
                Some(text) => Cow::Borrowed(text),
                None => match node.as_processing_instruction() {
                    Some((_, data)) => Cow::Borrowed(data),
                    None => Cow::Owned(node.text_content()),
                },
            },
        }
    }
}

/// The URI a namespace node binds.
/// The XHTML namespace, reported as the **null namespace**.
///
/// The tree under this model always comes out of an HTML parser, and the
/// reference implementation's HTML parser — libxml2's — puts no element in any
/// namespace at all: `namespace-uri(//div)` is `""` in lxml. html5ever has to
/// name a namespace for every element and picks XHTML, which is a formality of
/// the HTML5 tree construction algorithm, not information the document carried.
/// Reporting it would make `namespace-uri()` disagree with lxml on every element
/// of every page, and would contradict this engine's own rule that an unprefixed
/// name test matches whatever the namespace.
///
/// Foreign content keeps its real namespace: an element inside `<svg>` or
/// `<math>` still reports the SVG/MathML URI, so a bound prefix (`//svg:rect`)
/// selects it.
fn html_ns_as_null(ns: &str) -> &str {
    if ns == HTML_NAMESPACE {
        ""
    } else {
        ns
    }
}

fn namespace_uri_of(source: NsSource<'_>) -> &str {
    match source {
        NsSource::Xml => XML_NAMESPACE,
        NsSource::Decl(element, index) => element.attrs().nth(index).map_or("", |(_, value)| value),
    }
}

/// The prefix a namespace node binds — `""` for the default namespace.
fn namespace_prefix_of(source: NsSource<'_>) -> &str {
    match source {
        NsSource::Xml => "xml",
        NsSource::Decl(element, index) => element
            .attrs()
            .nth(index)
            .map_or("", |(name, _)| declaration_prefix(&name.local)),
    }
}

/// `xmlns` → `""`, `xmlns:svg` → `"svg"`, `svg` (already split by html5ever) → `"svg"`.
fn declaration_prefix(local: &str) -> &str {
    match local.strip_prefix("xmlns:") {
        Some(rest) => rest,
        None if local == "xmlns" => "",
        None => local,
    }
}

/// Whether an attribute is a namespace declaration.
///
/// html5ever splits `xmlns:xlink` into `prefix = xmlns` + `local = xlink` **inside
/// foreign content only**; on an HTML element the same source text stays a single
/// attribute whose local name is literally `xmlns:xlink`. Both spellings are
/// recognised.
fn is_namespace_declaration(name: &browser_dom::QualName) -> bool {
    &*name.ns == XMLNS_NAMESPACE || &*name.local == "xmlns" || name.local.starts_with("xmlns:")
}

/// [`TreeModel`] over a [`Document`].
///
/// Building one walks the document once to precompute the document-order index, so
/// **build it once per document and reuse it** across queries:
///
/// ```
/// use browser_dom::Document;
/// use browser_xpath::eval::{Variables, XPath};
/// use browser_xpath::model::DomModel;
///
/// let doc = Document::parse("<div><a href='/one'>one</a></div>");
/// let model = DomModel::new(&doc);
/// let expr = XPath::compile("//a/@href")?;
/// let value = expr.eval(&model, model.root(), &Variables::new())?;
/// assert_eq!(value.string_value(&model), "/one");
/// # Ok::<(), browser_xpath::error::XPathError>(())
/// ```
#[derive(Debug)]
pub struct DomModel<'a> {
    doc: &'a Document,
    order: browser_dom::DocumentOrder<'a>,
}

/// How much room [`DomModel::document_order`] leaves between two arena nodes for
/// the namespace and attribute nodes that sort between them.
const SLOT_BITS: u32 = 20;
/// The first slot used by attribute nodes; namespace nodes take the slots below it.
const ATTR_SLOT_BASE: u64 = 1 << (SLOT_BITS - 1);

impl<'a> DomModel<'a> {
    /// Index `doc` for XPath evaluation.
    #[must_use]
    pub fn new(doc: &'a Document) -> Self {
        Self {
            doc,
            order: doc.document_order(),
        }
    }

    /// The document node — the XPath root, i.e. what `/` selects.
    #[must_use]
    pub fn root(&self) -> XNode<'a> {
        XNode::Tree(self.doc.root())
    }

    /// The `<html>` element — the context node `Selector.xpath()` evaluates
    /// against, since lxml hands out the root *element*, not the document node.
    #[must_use]
    pub fn root_element(&self) -> Option<XNode<'a>> {
        self.doc.root_element().map(XNode::Tree)
    }

    /// The document this model indexes.
    #[must_use]
    pub fn document(&self) -> &'a Document {
        self.doc
    }

    /// Wrap an arena node as an XPath node.
    #[must_use]
    pub fn node(&self, node: NodeRef<'a>) -> XNode<'a> {
        XNode::Tree(node)
    }

    /// The namespace nodes in scope on `element`, nearest declaration winning,
    /// with `xml` always present and `xmlns=""` un-declaring the default namespace.
    fn in_scope_namespaces(element: NodeRef<'a>) -> Vec<XNode<'a>> {
        let mut seen: Vec<(String, NsSource<'a>)> = vec![("xml".to_owned(), NsSource::Xml)];
        let mut current = Some(element);
        while let Some(node) = current {
            if node.is_element() {
                for (index, (name, _value)) in node.attrs().iter_qual().enumerate() {
                    if !is_namespace_declaration(name) {
                        continue;
                    }
                    let prefix = declaration_prefix(&name.local).to_owned();
                    if seen.iter().any(|(p, _)| *p == prefix) {
                        continue; // a nearer declaration already won
                    }
                    seen.push((prefix, NsSource::Decl(node, index)));
                }
            }
            current = node.parent();
        }
        // `xmlns=""` un-declares the default namespace: the binding is recorded
        // above so an outer `xmlns` cannot win, then dropped here.
        seen.retain(|(prefix, source)| {
            !(prefix.is_empty() && namespace_uri_of(*source).is_empty())
        });
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        seen.into_iter()
            .map(|(_, source)| XNode::Namespace(element, source))
            .collect()
    }

    fn attrs_of(node: NodeRef<'a>) -> &'a Attributes {
        node.attrs()
    }
}

impl<'a> TreeModel for DomModel<'a> {
    type Node = XNode<'a>;

    fn parent(&self, n: Self::Node) -> Option<Self::Node> {
        match n {
            // An attribute's (or namespace node's) parent is its element, even
            // though the element does not list it as a child.
            XNode::Attr(owner, _) | XNode::Namespace(owner, _) => Some(XNode::Tree(owner)),
            XNode::Tree(node) => node.parent().map(XNode::Tree),
        }
    }

    fn children(&self, n: Self::Node) -> impl Iterator<Item = Self::Node> {
        let iter = match n {
            XNode::Attr(..) | XNode::Namespace(..) => None,
            XNode::Tree(node) => Some(node.children()),
        };
        // The doctype is not part of the XPath data model, so it is filtered out
        // here and is therefore invisible on *every* axis — which is what
        // `libxml2` does: `count(/node())` is 1 for `<!DOCTYPE html><html>…`.
        iter.into_iter()
            .flatten()
            .filter(|child| !child.is_doctype())
            .map(XNode::Tree)
    }

    fn kind(&self, n: Self::Node) -> NodeKind {
        n.kind()
    }

    fn name(&self, n: Self::Node) -> Option<(&str, &str)> {
        match n {
            XNode::Attr(owner, index) => Self::attrs_of(owner)
                .nth(index)
                .map(|(name, _)| (&*name.ns, &*name.local)),
            XNode::Namespace(_, source) => Some(("", namespace_prefix_of(source))),
            XNode::Tree(node) => match node.as_element() {
                Some(element) => {
                    Some((html_ns_as_null(&element.name().ns), &*element.name().local))
                }
                None => node
                    .as_processing_instruction()
                    .map(|(target, _)| ("", target)),
            },
        }
    }

    fn string_value(&self, n: Self::Node) -> Cow<'_, str> {
        n.string_value()
    }

    fn attributes(&self, n: Self::Node) -> impl Iterator<Item = Self::Node> {
        let owner = match n {
            XNode::Tree(node) if node.is_element() => Some(node),
            _ => None,
        };
        let count = owner.map_or(0, |node| Self::attrs_of(node).len());
        (0..count).filter_map(move |index| owner.map(|node| XNode::Attr(node, index)))
    }

    fn namespaces(&self, n: Self::Node) -> impl Iterator<Item = Self::Node> {
        match n {
            XNode::Tree(node) if node.is_element() => Self::in_scope_namespaces(node).into_iter(),
            _ => Vec::new().into_iter(),
        }
    }

    fn document_order(&self, n: Self::Node) -> u64 {
        // Every arena node owns a block of 2^SLOT_BITS keys. Slot 0 is the node
        // itself, the slots below ATTR_SLOT_BASE are its namespace nodes and the
        // ones above are its attributes — which is exactly the order XPath 1.0
        // §5.3 requires (namespace nodes before attribute nodes, both before the
        // element's children).
        let base = |node: NodeRef<'a>| self.order.rank(node).unwrap_or(u64::MAX >> SLOT_BITS);
        match n {
            XNode::Tree(node) => base(node) << SLOT_BITS,
            XNode::Attr(owner, index) => {
                let slot = ATTR_SLOT_BASE + (index as u64).min(ATTR_SLOT_BASE - 1);
                (base(owner) << SLOT_BITS) + slot
            }
            XNode::Namespace(owner, source) => {
                let index = Self::in_scope_namespaces(owner)
                    .iter()
                    .position(|node| matches!(node, XNode::Namespace(_, s) if *s == source))
                    .unwrap_or(0) as u64;
                (base(owner) << SLOT_BITS) + 1 + index.min(ATTR_SLOT_BASE - 2)
            }
        }
    }

    fn attribute_value(&self, n: Self::Node, local: &str) -> Option<Cow<'_, str>> {
        match n {
            XNode::Tree(node) if node.is_element() => node.attr(local).map(Cow::Borrowed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><body>
  <div id="list" class="c">
    <a href="/one" rel="next">one</a>
    <a href="/two">two</a>
  </div>
</body></html>"#;

    fn first_link(doc: &Document) -> NodeRef<'_> {
        doc.elements().find(|e| e.tag() == "a").expect("<a>")
    }

    #[test]
    fn root_is_the_document_node() {
        let doc = Document::parse(PAGE);
        let model = DomModel::new(&doc);
        assert_eq!(model.kind(model.root()), NodeKind::Root);
        assert_eq!(model.parent(model.root()), None);
        assert_eq!(model.name(model.root()), None);
        // The default `root()` walk agrees.
        let link = XNode::Tree(first_link(&doc));
        assert_eq!(model.root_of(link), model.root());
        assert_eq!(
            model.root_of(model.attributes(link).next().expect("@href")),
            model.root()
        );
    }

    #[test]
    fn attributes_are_first_class_nodes() {
        let doc = Document::parse(PAGE);
        let model = DomModel::new(&doc);
        let link = XNode::Tree(first_link(&doc));

        let attrs: Vec<_> = model.attributes(link).collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(model.kind(attrs[0]), NodeKind::Attr);
        assert_eq!(model.name(attrs[0]), Some(("", "href")));
        assert_eq!(model.string_value(attrs[0]), "/one");
        assert_eq!(attrs[0].as_attribute(), Some(("href", "/one")));
        assert_eq!(model.parent(attrs[0]), Some(link));
        assert_eq!(model.children(attrs[0]).count(), 0);
        assert_eq!(model.attributes(attrs[0]).count(), 0);
        assert_eq!(model.attribute_value(link, "rel").as_deref(), Some("next"));
    }

    #[test]
    fn document_order_is_injective_and_monotone() {
        let doc = Document::parse(PAGE);
        let model = DomModel::new(&doc);

        let mut all = Vec::new();
        for node in doc.root().descendants_and_self() {
            let n = XNode::Tree(node);
            all.push(n);
            all.extend(model.namespaces(n));
            all.extend(model.attributes(n));
        }
        let mut keys: Vec<u64> = all.iter().map(|n| model.document_order(*n)).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "document_order must be injective");

        let nodes: Vec<NodeRef<'_>> = doc.root().descendants_and_self().collect();
        for pair in nodes.windows(2) {
            assert!(
                model.document_order(XNode::Tree(pair[0]))
                    < model.document_order(XNode::Tree(pair[1]))
            );
        }
        // Namespace nodes sort before attributes, both after their element.
        let link = XNode::Tree(first_link(&doc));
        let ns = model.namespaces(link).next().expect("the xml prefix");
        let attr = model.attributes(link).next().expect("@href");
        assert!(model.document_order(link) < model.document_order(ns));
        assert!(model.document_order(ns) < model.document_order(attr));
    }

    #[test]
    fn namespace_nodes_come_from_declarations() {
        let doc = Document::parse(
            "<html><body><svg xmlns='http://www.w3.org/2000/svg'><rect/></svg></body></html>",
        );
        let model = DomModel::new(&doc);
        let svg = XNode::Tree(doc.elements().find(|e| e.tag() == "svg").expect("<svg>"));

        let namespaces: Vec<_> = model.namespaces(svg).collect();
        // The default declaration plus the implicit `xml` prefix.
        assert_eq!(namespaces.len(), 2);
        let named: Vec<(String, String)> = namespaces
            .iter()
            .map(|n| {
                (
                    model.name(*n).expect("named").1.to_owned(),
                    model.string_value(*n).into_owned(),
                )
            })
            .collect();
        assert!(named.contains(&(String::new(), "http://www.w3.org/2000/svg".to_owned())));
        assert!(named.contains(&("xml".to_owned(), XML_NAMESPACE.to_owned())));

        // The declaration is inherited by descendants.
        let rect = XNode::Tree(doc.elements().find(|e| e.tag() == "rect").expect("<rect>"));
        assert_eq!(model.namespaces(rect).count(), 2);
        // …and non-elements have none.
        assert_eq!(model.namespaces(model.root()).count(), 0);
    }

    #[test]
    fn string_value_is_the_xpath_one() {
        let doc = Document::parse(PAGE);
        let model = DomModel::new(&doc);
        let list = XNode::Tree(
            doc.elements()
                .find(|e| e.element_id() == Some("list"))
                .expect("#list"),
        );
        let value = model.string_value(list);
        assert!(value.contains("one") && value.contains("two"));
        assert!(model.string_value(model.root()).contains("one"));

        let text = model
            .children(XNode::Tree(first_link(&doc)))
            .next()
            .expect("text child");
        assert_eq!(model.kind(text), NodeKind::Text);
        assert_eq!(model.string_value(text), "one");
        assert_eq!(text.as_text(), Some("one"));
    }

    #[test]
    fn the_doctype_is_not_in_the_data_model() {
        let doc = Document::parse("<!DOCTYPE html><html><body><p>x</p></body></html>");
        let model = DomModel::new(&doc);
        // libxml2: count(/node()) == 1 — only <html>.
        let children: Vec<_> = model.children(model.root()).collect();
        assert_eq!(children.len(), 1);
        assert_eq!(model.name(children[0]).map(|(_, l)| l), Some("html"));
        assert!(
            doc.root().children().any(|child| child.is_doctype()),
            "the tree does keep a doctype node"
        );
    }

    #[test]
    fn comments_are_comment_nodes_when_kept() {
        let doc = Document::parse_with(
            "<div><!-- hi --></div>",
            &browser_dom::ParseOptions::new().keep_comments(true),
        );
        let model = DomModel::new(&doc);
        let comment = doc
            .root()
            .descendants()
            .find(|n| n.is_comment())
            .map(XNode::Tree)
            .expect("comment");
        assert_eq!(model.kind(comment), NodeKind::Comment);
        assert_eq!(model.string_value(comment), " hi ");
        assert_eq!(model.name(comment), None);
    }
}
