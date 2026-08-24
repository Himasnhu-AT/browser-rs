//! The [`Document`] arena wrapper: root access, node lookup, base URL and
//! encoding.
//!
//! A `Document` owns an `ego_tree::Tree<NodeData>`. Everything else in the
//! crate is a view over it: [`NodeRef`] for reading, the methods in
//! [`crate::edit`] for mutating.
//!
//! `Document` is `Send + Sync` (see [`crate::node`] for why the text payloads
//! are `String` rather than `StrTendril`), so a parsed page can be shared with
//! `rayon` workers — which `relocate` in `browser-parser` relies on.

use std::collections::HashMap;

use ego_tree::{NodeId, Tree};
use url::Url;

use crate::error::DomError;
use crate::node::{NodeData, NodeRef};
use crate::parse::ParseOptions;
use crate::serialize::clean_spaces;

/// A parsed HTML document.
#[derive(Clone)]
pub struct Document {
    tree: Tree<NodeData>,
    /// Base URL used by `urljoin`; `None` when the page was parsed without one.
    pub base_url: Option<Url>,
    /// Character encoding the source was decoded with. Purely informational —
    /// the tree itself is always UTF-8.
    pub encoding: String,
}

impl Document {
    /// Wrap an already-built arena.
    #[must_use]
    pub fn from_tree(tree: Tree<NodeData>, base_url: Option<Url>, encoding: String) -> Self {
        Self {
            tree,
            base_url,
            encoding,
        }
    }

    /// Parse HTML with the default options (comments removed, CDATA folded into
    /// text, blank text removed) — the reference implementation's `Selector(content)` defaults.
    #[must_use]
    pub fn parse(html: &str) -> Self {
        crate::parse::parse_html(html)
    }

    /// Parse HTML with explicit options.
    #[must_use]
    pub fn parse_with(html: &str, options: &ParseOptions) -> Self {
        crate::parse::parse_html_with(html, options)
    }

    /// Parse a byte body. The bytes are decoded as UTF-8, replacing invalid
    /// sequences — matching html5ever's own lossy decoder.
    #[must_use]
    pub fn parse_bytes(bytes: &[u8], options: &ParseOptions) -> Self {
        crate::parse::parse_bytes_with(bytes, options)
    }

    /// The underlying arena.
    #[must_use]
    pub fn tree(&self) -> &Tree<NodeData> {
        &self.tree
    }

    pub(crate) fn tree_mut(&mut self) -> &mut Tree<NodeData> {
        &mut self.tree
    }

    /// The arena root (a [`NodeData::Document`] node).
    #[must_use]
    pub fn root(&self) -> NodeRef<'_> {
        NodeRef::new(self, self.tree.root().id())
    }

    /// Look a node up by id.
    ///
    /// `None` only when this document's arena has no such slot. A [`NodeId`] is an
    /// *arena index*, not a globally unique handle — an id taken from a different
    /// `Document` will usually resolve here too, to an unrelated node. Pass
    /// [`NodeRef`]s (which carry their document) across API boundaries; use raw
    /// ids only within one tree.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<NodeRef<'_>> {
        self.tree.get(id).map(|node| NodeRef::new(self, node.id()))
    }

    /// The `<html>` element — the first element child of the root.
    #[must_use]
    pub fn root_element(&self) -> Option<NodeRef<'_>> {
        self.root().element_children().next()
    }

    /// The `<head>` element, if the parser produced one.
    #[must_use]
    pub fn head(&self) -> Option<NodeRef<'_>> {
        self.find_first_element("head")
    }

    /// The `<body>` element, if the parser produced one.
    #[must_use]
    pub fn body(&self) -> Option<NodeRef<'_>> {
        self.find_first_element("body")
    }

    /// Text of the first `<title>` element.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        self.find_first_element("title").map(|t| t.text_content())
    }

    fn find_first_element(&self, name: &str) -> Option<NodeRef<'_>> {
        self.root()
            .descendant_elements()
            .find(|el| el.local_name() == Some(name))
    }

    /// Every element in the document, in document order.
    pub fn elements(&self) -> impl Iterator<Item = NodeRef<'_>> + '_ {
        self.root().descendant_elements()
    }

    /// Every node in the document (excluding the root), in document order.
    pub fn nodes(&self) -> impl Iterator<Item = NodeRef<'_>> + '_ {
        self.root().descendants()
    }

    /// The base URL, if one was supplied at parse time.
    #[must_use]
    pub fn base_url(&self) -> Option<&Url> {
        self.base_url.as_ref()
    }

    /// Replace the base URL.
    ///
    /// # Errors
    /// [`DomError::InvalidBaseUrl`] when `url` is not an absolute URL.
    pub fn set_base_url(&mut self, url: &str) -> Result<(), DomError> {
        self.base_url = Some(Url::parse(url)?);
        Ok(())
    }

    /// Resolve `relative` against the document's base URL.
    ///
    /// With no base URL the input is returned unchanged, which is what
    /// `urljoin("", rel)` does in Python.
    ///
    /// # Errors
    /// [`DomError::InvalidBaseUrl`] when the join fails.
    pub fn urljoin(&self, relative: &str) -> Result<String, DomError> {
        match &self.base_url {
            Some(base) => Ok(base.join(relative)?.to_string()),
            None => Ok(relative.to_owned()),
        }
    }

    /// The encoding label the document was parsed with.
    #[must_use]
    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    /// Serialise the whole document, doctype included.
    #[must_use]
    pub fn html(&self) -> String {
        self.root().inner_html()
    }

    /// Serialise the whole document with indentation.
    #[must_use]
    pub fn pretty(&self) -> String {
        self.root().pretty()
    }

    /// Precompute a dense document-order rank for every node in one walk.
    ///
    /// [`NodeRef::cmp_document_order`] is O(depth) per comparison, which is the
    /// right tool for a one-off compare but the wrong one inside a sort or inside
    /// an XPath evaluator that unions node-sets repeatedly. This builds the
    /// `document_order(n) -> u64` key that `browser-xpath`'s `TreeModel`
    /// (`ARCHITECTURE.md` §1.4) is specified in terms of.
    ///
    /// The index is a snapshot: any mutation through [`crate::edit`] invalidates
    /// it, so build it after the tree has settled.
    ///
    /// ```
    /// use browser_dom::Document;
    ///
    /// let doc = Document::parse("<div><a>1</a><b>2</b></div>");
    /// let order = doc.document_order();
    /// let a = doc.elements().find(|e| e.tag() == "a").unwrap();
    /// let b = doc.elements().find(|e| e.tag() == "b").unwrap();
    /// assert!(order.rank(a).unwrap() < order.rank(b).unwrap());
    /// ```
    #[must_use]
    pub fn document_order(&self) -> DocumentOrder<'_> {
        let mut ranks = HashMap::new();
        for (rank, node) in self.root().descendants_and_self().enumerate() {
            ranks.insert(node.id(), rank as u64);
        }
        DocumentOrder { doc: self, ranks }
    }
}

/// A precomputed document-order index over one [`Document`], built by
/// [`Document::document_order`].
///
/// Ranks are dense and start at `0` for the document node itself. They are only
/// meaningful relative to one another.
///
/// The index borrows its document, and every lookup is keyed by [`NodeRef`]
/// rather than by [`NodeId`], **on purpose**: a `NodeId` is an index into one
/// arena and the same numeric id exists in every other `Document`, so an id-keyed
/// API would silently answer questions about the wrong tree. `NodeRef` carries
/// the document it came from, so a foreign node is detectable and reported as
/// `None`.
#[derive(Clone, Debug)]
pub struct DocumentOrder<'a> {
    doc: &'a Document,
    ranks: HashMap<NodeId, u64>,
}

impl<'a> DocumentOrder<'a> {
    /// The document-order rank of `node`.
    ///
    /// `None` when `node` belongs to a different document, or was detached before
    /// the index was built.
    #[must_use]
    pub fn rank(&self, node: NodeRef<'_>) -> Option<u64> {
        if !std::ptr::eq(node.document(), self.doc) {
            return None;
        }
        self.ranks.get(&node.id()).copied()
    }

    /// Compare two nodes in O(1). Nodes missing from the index sort last.
    #[must_use]
    pub fn compare(&self, a: NodeRef<'_>, b: NodeRef<'_>) -> std::cmp::Ordering {
        self.rank(a).cmp(&self.rank(b))
    }

    /// The document this index was built from.
    #[must_use]
    pub fn document(&self) -> &'a Document {
        self.doc
    }

    /// Number of indexed nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ranks.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranks.is_empty()
    }

    /// Sort a node set into document order and drop duplicates — the
    /// normalisation every XPath node-set needs, in O(n log n) rather than the
    /// O(n log n · depth) of [`crate::nav::sort_document_order`].
    pub fn sort_dedup(&self, nodes: &mut Vec<NodeRef<'a>>) {
        nodes.sort_by(|a, b| self.compare(*a, *b));
        nodes.dedup();
    }
}

impl std::fmt::Debug for Document {
    /// `<document url='…' html='…'>` with the same `clean_spaces` + 40-char
    /// truncation Python's `Selector.__repr__` uses.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let html = crate::serialize::truncate_repr(&clean_spaces(&self.html()));
        match &self.base_url {
            Some(url) => write!(f, "<document url='{url}' html='{html}'>"),
            None => write!(f, "<document html='{html}'>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn document_is_send_and_sync() {
        assert_send_sync::<Document>();
    }

    #[test]
    fn scaffolding_is_created_for_a_fragment() {
        let doc = Document::parse("<div>hi</div>");
        assert_eq!(
            doc.root_element().and_then(|e| e.local_name()),
            Some("html")
        );
        assert!(doc.head().is_some());
        assert!(doc.body().is_some());
        assert_eq!(doc.title(), None);
    }

    #[test]
    fn title_and_elements() {
        let doc = Document::parse("<html><head><title>Hi </title></head><body><p>x</p></body>");
        assert_eq!(doc.title().as_deref(), Some("Hi "));
        let tags: Vec<_> = doc.elements().filter_map(|e| e.local_name()).collect();
        assert_eq!(tags, ["html", "head", "title", "body", "p"]);
    }

    #[test]
    fn urljoin_uses_the_base_url() {
        let mut doc = Document::parse("<p></p>");
        assert_eq!(doc.urljoin("../other").unwrap(), "../other");
        doc.set_base_url("https://example.com/page").unwrap();
        assert_eq!(
            doc.urljoin("../other").unwrap(),
            "https://example.com/other"
        );
        assert!(doc.set_base_url("not a url").is_err());
    }

    #[test]
    fn document_order_index_agrees_with_the_pairwise_comparator() {
        // Mixed node kinds at several depths, so the index has to be a real
        // pre-order walk and not "arena insertion order".
        let doc = Document::parse_with(
            "<div id='a'>t1<span id='b'>t2</span><!--c--><p id='d'><i id='e'>t3</i>t4</p></div>",
            &ParseOptions::new().keep_comments(true),
        );
        let order = doc.document_order();
        let nodes: Vec<_> = doc.root().descendants_and_self().collect();

        assert_eq!(order.len(), nodes.len());
        for (i, a) in nodes.iter().enumerate() {
            for b in &nodes[i + 1..] {
                assert_eq!(
                    order.compare(*a, *b),
                    a.cmp_document_order(b),
                    "{a:?}/{b:?}"
                );
                assert!(order.rank(*a) < order.rank(*b));
            }
        }

        // sort_dedup normalises a scrambled, duplicated node-set.
        let mut set = vec![nodes[5], nodes[1], nodes[5], nodes[3]];
        order.sort_dedup(&mut set);
        assert_eq!(set, vec![nodes[1], nodes[3], nodes[5]]);
    }

    /// `NodeId` is an arena index, so the *same* numeric id exists in every
    /// document. `DocumentOrder` keys on `NodeRef` precisely so that a node from
    /// another tree is rejected instead of being silently mis-ranked.
    #[test]
    fn document_order_rejects_a_node_from_another_document() {
        let a = Document::parse("<p id='x'></p>");
        let b = Document::parse("<p id='x'></p>");
        let order = a.document_order();

        let mine = a.elements().last().expect("an element");
        let foreign = b.elements().last().expect("an element");

        assert_eq!(mine.id(), foreign.id(), "ids do collide across documents");
        assert!(order.rank(mine).is_some());
        assert_eq!(order.rank(foreign), None);
    }

    #[test]
    fn debug_is_clean_and_truncated() {
        let doc = Document::parse("<div>\n   a   b\n</div>");
        let rendered = format!("{doc:?}");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(rendered.starts_with("<document html='"));
    }
}
