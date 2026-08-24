//! O(1) navigation over the arena, plus the `text`/`tail` view.
//!
//! # The `text` / `tail` view — how lxml's model maps onto html5ever's
//!
//! This is the subtlest part of the port, and `get_all_text`,
//! `element_to_dict` and `drop_tree` all depend on getting it right.
//!
//! **lxml/libxml2** does not give an element a list of text children. It gives
//! every element two string slots:
//!
//! * `element.text` — the characters between the element's start tag and its
//!   first child node;
//! * `element.tail` — the characters between the element's end tag and the next
//!   sibling node (or the parent's end tag).
//!
//! So `<div>a<b/>c<!--x-->d</div>` is, in lxml:
//! `div.text == "a"`, `b.tail == "c"`, `comment.tail == "d"`.
//!
//! **html5ever** (like the DOM, and like every browser) makes each run of
//! characters a real node. The same markup is a `div` element with four
//! children: `Text("a")`, `Element(b)`, `Text("c")`, `Comment(x)`, `Text("d")`.
//!
//! The mapping is therefore mechanical:
//!
//! | lxml | here |
//! |---|---|
//! | `el.text` | [`NodeRef::text`] — the first child, if it is a text node |
//! | `el.tail` | [`NodeRef::tail`] — the next sibling, if it is a text node |
//! | `text_node.getparent()` when it is a `text` | the text node's parent element |
//! | `text_node.getparent()` when it is a `tail` | the element it trails — i.e. our text node's *previous sibling* |
//! | `.//text()` | [`NodeRef::descendant_text_nodes`] |
//!
//! Two consequences matter downstream, and both are wins:
//!
//! 1. **`get_all_text`'s owner rule collapses.** Python has to write
//!    `owner = parent.getparent() if text_node.is_tail else parent`, because a
//!    tail's `getparent()` is the element it follows, not the element it lives
//!    in. Walk that through: for a tail of `X` inside `E`, the owner is
//!    `X.getparent() == E`; for a text of `E`, the owner is `E`. In *both*
//!    cases the owner is the containing element — which is exactly
//!    `text_node.parent()` in our tree. So the `ignore_tags` check becomes
//!    "is this text node's parent element in the ignored set", with no
//!    tail/text distinction at all. Text that follows `</script>` keeps being
//!    kept, and text inside `<script>` keeps being dropped.
//! 2. **`drop_tree` is just a detach.** lxml has to re-parent the removed
//!    element's tail onto the previous sibling or the parent; here the tail is
//!    already a separate sibling node that detaching the element does not
//!    touch. See [`crate::edit`].
//!
//! One place the two models genuinely differ: adjacent text runs. libxml2 and
//! html5ever both merge them, and so do we ([`crate::parse::merge_adjacent_text`]
//! re-merges after comment removal), so an element never has two consecutive
//! text children and `text()`/`tail()` are unambiguous.
//!
//! # Comment-skipping variants
//!
//! Python's `Selector.children`, `.next`, `.previous` and `siblings` filter
//! `html_forbidden = (HtmlComment,)`. In lxml a text run is *never* a sibling
//! node, so the faithful equivalent of `.next` is
//! [`NodeRef::next_element_sibling`], not [`NodeRef::next_non_comment_sibling`]
//! (which will happily hand you a text node). Both are provided; pick the one
//! that matches the semantics you are porting.

use std::cmp::Ordering;

use crate::node::{NodeData, NodeRef};

impl<'a> NodeRef<'a> {
    // -- upwards ----------------------------------------------------------

    /// The parent node — including the arena root for `<html>`.
    #[must_use]
    pub fn parent(&self) -> Option<NodeRef<'a>> {
        let doc = self.document();
        self.ego().parent().map(|n| NodeRef::new(doc, n.id()))
    }

    /// The parent, if it is an element. `None` at `<html>` (whose parent is the
    /// document node) — matching lxml's `getparent()` on the root element.
    #[must_use]
    pub fn parent_element(&self) -> Option<NodeRef<'a>> {
        self.parent().filter(NodeRef::is_element)
    }

    /// Element ancestors, closest first — lxml's `iterancestors()`.
    ///
    /// The document node is not yielded.
    pub fn ancestors(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.ancestor_nodes().filter(NodeRef::is_element)
    }

    /// Every ancestor including the document node — the XPath `ancestor::` axis.
    pub fn ancestor_nodes(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.document();
        self.ego()
            .ancestors()
            .map(move |n| NodeRef::new(doc, n.id()))
    }

    /// Number of element ancestors — `count(ancestor::*)`, and the value
    /// `find_similar` compares depths with.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.ancestors().count()
    }

    /// First ancestor satisfying `predicate` — `find_ancestor`.
    pub fn find_ancestor(&self, predicate: impl Fn(&NodeRef<'a>) -> bool) -> Option<NodeRef<'a>> {
        self.ancestors().find(|n| predicate(n))
    }

    /// Is `self` inside `other`'s subtree?
    #[must_use]
    pub fn is_descendant_of(&self, other: &NodeRef<'a>) -> bool {
        self.ancestor_nodes().any(|a| a == *other)
    }

    // -- downwards --------------------------------------------------------

    /// All child nodes, in document order.
    pub fn children(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.document();
        self.ego()
            .children()
            .map(move |n| NodeRef::new(doc, n.id()))
    }

    /// Child elements only.
    pub fn element_children(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.children().filter(NodeRef::is_element)
    }

    /// Child nodes except comments — Python's `Selector.children` filter
    /// (`html_forbidden`), which keeps processing instructions.
    pub fn non_comment_children(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.children().filter(|n| !n.is_comment())
    }

    /// The first child node.
    #[must_use]
    pub fn first_child(&self) -> Option<NodeRef<'a>> {
        let doc = self.document();
        self.ego().first_child().map(|n| NodeRef::new(doc, n.id()))
    }

    /// The last child node.
    #[must_use]
    pub fn last_child(&self) -> Option<NodeRef<'a>> {
        let doc = self.document();
        self.ego().last_child().map(|n| NodeRef::new(doc, n.id()))
    }

    /// The first child element.
    #[must_use]
    pub fn first_element_child(&self) -> Option<NodeRef<'a>> {
        self.element_children().next()
    }

    /// The last child element.
    #[must_use]
    pub fn last_element_child(&self) -> Option<NodeRef<'a>> {
        self.element_children().last()
    }

    /// `true` when the node has at least one child.
    #[must_use]
    pub fn has_children(&self) -> bool {
        self.ego().has_children()
    }

    /// Every descendant node in document order, excluding `self`.
    pub fn descendants(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.document();
        self.ego()
            .descendants()
            .skip(1)
            .map(move |n| NodeRef::new(doc, n.id()))
    }

    /// `self` followed by every descendant, in document order.
    pub fn descendants_and_self(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.document();
        self.ego()
            .descendants()
            .map(move |n| NodeRef::new(doc, n.id()))
    }

    /// Descendant elements in document order, excluding `self` — the direct
    /// tree walk that replaces the reference implementation's compiled `.//*` XPath.
    pub fn descendant_elements(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.descendants().filter(NodeRef::is_element)
    }

    /// Descendant text nodes in document order — the direct tree walk that
    /// replaces the reference implementation's compiled `.//text()` XPath.
    ///
    /// This yields both what lxml calls `text` and what it calls `tail`; the
    /// owning element of each is simply [`NodeRef::parent`] (see the module
    /// docs).
    pub fn descendant_text_nodes(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.descendants().filter(NodeRef::is_text)
    }

    /// Descendant elements whose **direct** text is non-blank — the tree walk
    /// that replaces `.//*[normalize-space(text())]`.
    pub fn descendants_with_nonblank_direct_text(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.descendant_elements()
            .filter(|el| el.text().is_some_and(|t| !t.trim().is_empty()))
    }

    // -- sideways ---------------------------------------------------------

    /// The next sibling node of any kind.
    #[must_use]
    pub fn next_sibling(&self) -> Option<NodeRef<'a>> {
        let doc = self.document();
        self.ego().next_sibling().map(|n| NodeRef::new(doc, n.id()))
    }

    /// The previous sibling node of any kind.
    #[must_use]
    pub fn prev_sibling(&self) -> Option<NodeRef<'a>> {
        let doc = self.document();
        self.ego().prev_sibling().map(|n| NodeRef::new(doc, n.id()))
    }

    /// The next sibling **element** — the faithful equivalent of lxml's
    /// `getnext()` with comments skipped, i.e. Python's `Selector.next`.
    #[must_use]
    pub fn next_element_sibling(&self) -> Option<NodeRef<'a>> {
        self.next_siblings().find(NodeRef::is_element)
    }

    /// The previous sibling **element** — Python's `Selector.previous`.
    #[must_use]
    pub fn prev_element_sibling(&self) -> Option<NodeRef<'a>> {
        self.prev_siblings().find(NodeRef::is_element)
    }

    /// The next sibling that is not a comment (text nodes included).
    #[must_use]
    pub fn next_non_comment_sibling(&self) -> Option<NodeRef<'a>> {
        self.next_siblings().find(|n| !n.is_comment())
    }

    /// The previous sibling that is not a comment (text nodes included).
    #[must_use]
    pub fn prev_non_comment_sibling(&self) -> Option<NodeRef<'a>> {
        self.prev_siblings().find(|n| !n.is_comment())
    }

    /// Following siblings, in document order.
    pub fn next_siblings(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.document();
        self.ego()
            .next_siblings()
            .map(move |n| NodeRef::new(doc, n.id()))
    }

    /// Preceding siblings, nearest first (reverse document order) — the XPath
    /// `preceding-sibling::` axis order.
    pub fn prev_siblings(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.document();
        self.ego()
            .prev_siblings()
            .map(move |n| NodeRef::new(doc, n.id()))
    }

    /// Every sibling except `self`, in document order.
    pub fn siblings(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let mut before: Vec<NodeRef<'a>> = self.prev_siblings().collect();
        before.reverse();
        before.into_iter().chain(self.next_siblings())
    }

    /// Sibling elements except `self`, in document order — Python's
    /// `Selector.siblings`.
    pub fn element_siblings(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.siblings().filter(NodeRef::is_element)
    }

    /// Sibling nodes except `self` and except comments.
    pub fn non_comment_siblings(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        self.siblings().filter(|n| !n.is_comment())
    }

    /// 0-based position among **all** the parent's children.
    #[must_use]
    pub fn index_in_parent(&self) -> usize {
        self.prev_siblings().count()
    }

    /// 1-based position among preceding sibling elements of the same tag —
    /// the number CSS `:nth-of-type()` / XPath `[n]` uses in generated
    /// selectors. Returns `1` for a node with no parent.
    #[must_use]
    pub fn nth_of_type(&self) -> usize {
        let Some(name) = self.local_name() else {
            return 1;
        };
        1 + self
            .prev_siblings()
            .filter(|n| n.local_name() == Some(name))
            .count()
    }

    // -- text / tail ------------------------------------------------------

    /// lxml's `element.text`: the direct text before the first child node.
    ///
    /// Returns `None` when the first child is not a text node — including the
    /// "no children at all" case.
    #[must_use]
    pub fn text(&self) -> Option<&'a str> {
        self.first_child().and_then(|n| n.as_text())
    }

    /// lxml's `element.tail`: the text between this node's end tag and the next
    /// sibling node.
    #[must_use]
    pub fn tail(&self) -> Option<&'a str> {
        self.next_sibling().and_then(|n| n.as_text())
    }

    /// All descendant character data concatenated, in document order — the
    /// XPath string-value of the node.
    ///
    /// This is *not* `get_all_text`: there is no separator, no `ignore_tags`
    /// and no stripping. `browser-parser` builds `get_all_text` on top of
    /// [`NodeRef::descendant_text_nodes`].
    #[must_use]
    pub fn text_content(&self) -> String {
        match self.value() {
            NodeData::Text(text) => text.clone(),
            NodeData::Comment(_) | NodeData::ProcessingInstruction { .. } => String::new(),
            _ => {
                let mut out = String::new();
                for node in self.descendant_text_nodes() {
                    if let Some(text) = node.as_text() {
                        out.push_str(text);
                    }
                }
                out
            }
        }
    }

    // -- document order ---------------------------------------------------

    /// The node's position as the chain of child indices from the arena root.
    ///
    /// Lexicographic comparison of two paths is document order.
    #[must_use]
    pub fn document_path(&self) -> Vec<usize> {
        let mut path = Vec::new();
        let mut current = *self;
        while let Some(parent) = current.parent() {
            path.push(current.index_in_parent());
            current = parent;
        }
        path.reverse();
        path
    }

    /// Compare two nodes in document order.
    ///
    /// O(depth); use [`sort_document_order`] to order a whole node set, which
    /// computes each path once.
    #[must_use]
    pub fn cmp_document_order(&self, other: &NodeRef<'a>) -> Ordering {
        self.document_path().cmp(&other.document_path())
    }
}

/// Sort a node set into document order (decorate–sort–undecorate).
pub fn sort_document_order(nodes: &mut [NodeRef<'_>]) {
    let mut keyed: Vec<(Vec<usize>, NodeRef<'_>)> =
        nodes.iter().map(|n| (n.document_path(), *n)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    for (slot, (_, node)) in nodes.iter_mut().zip(keyed) {
        *slot = node;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOptions;
    use crate::tree::Document;

    fn body_of(html: &str) -> Document {
        Document::parse(html)
    }

    #[test]
    fn text_and_tail_reproduce_the_lxml_split() {
        let doc = Document::parse_with(
            "<div>a<b>B</b>c<!--x-->d</div>",
            &ParseOptions::default().keep_comments(true),
        );
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        assert_eq!(div.text(), Some("a"));
        let b = div.first_element_child().unwrap();
        assert_eq!(b.text(), Some("B"));
        assert_eq!(b.tail(), Some("c"));
        let comment = div.children().find(NodeRef::is_comment).unwrap();
        assert_eq!(comment.tail(), Some("d"));
        assert_eq!(div.tail(), None);
    }

    #[test]
    fn text_is_none_when_the_first_child_is_an_element() {
        let doc = body_of("<div><b>B</b>tail</div>");
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        assert_eq!(div.text(), None);
        assert_eq!(div.first_element_child().unwrap().tail(), Some("tail"));
    }

    #[test]
    fn text_node_owner_is_always_the_containing_element() {
        // The rule `get_all_text` needs: text following </script> belongs to the
        // div, text inside <script> belongs to the script.
        let doc = body_of("<div>keep<script>drop</script>done</div>");
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        let owners: Vec<_> = div
            .descendant_text_nodes()
            .map(|t| {
                (
                    t.as_text().unwrap_or_default().to_owned(),
                    t.parent().and_then(|p| p.local_name()).unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            owners,
            [
                ("keep".to_owned(), "div"),
                ("drop".to_owned(), "script"),
                ("done".to_owned(), "div"),
            ]
        );
    }

    #[test]
    fn ancestor_navigation_matches_the_pinned_order() {
        let html = r#"<div id="level4"><section><article><p><span id="target">x</span></p></article></section></div>"#;
        let doc = body_of(html);
        let target = doc
            .elements()
            .find(|e| e.element_id() == Some("target"))
            .unwrap();
        let chain: Vec<_> = target.ancestors().filter_map(|a| a.local_name()).collect();
        assert_eq!(chain, ["p", "article", "section", "div", "body", "html"]);
        assert_eq!(target.depth(), 6);
        assert_eq!(
            target
                .find_ancestor(|a| a.local_name() == Some("article"))
                .and_then(|a| a.local_name()),
            Some("article")
        );
        assert!(target
            .find_ancestor(|a| a.local_name() == Some("table"))
            .is_none());
        let html_el = doc.root_element().unwrap();
        assert_eq!(html_el.ancestors().count(), 0);
        assert!(html_el.parent_element().is_none());
        assert!(html_el.parent().is_some_and(|p| p.is_document()));
    }

    #[test]
    fn sibling_navigation_skips_comments_and_text() {
        let doc = Document::parse_with(
            "<ul><li data-id='1'>a</li><!--c-->gap<li data-id='2'>b</li></ul>",
            &ParseOptions::default().keep_comments(true),
        );
        let first = doc
            .elements()
            .find(|e| e.attr("data-id") == Some("1"))
            .unwrap();
        assert_eq!(
            first.next_element_sibling().and_then(|n| n.attr("data-id")),
            Some("2")
        );
        assert!(first
            .next_non_comment_sibling()
            .is_some_and(|n| n.is_text()));
        assert_eq!(
            first
                .next_element_sibling()
                .and_then(|n| n.prev_element_sibling())
                .and_then(|n| n.attr("data-id")),
            Some("1")
        );
        assert_eq!(first.element_siblings().count(), 1);
        assert_eq!(first.non_comment_children().count(), 1);
    }

    #[test]
    fn nth_of_type_counts_same_tag_predecessors() {
        let doc = body_of("<div><p>1</p><span>s</span><p>2</p><p>3</p></div>");
        let ps: Vec<_> = doc
            .elements()
            .filter(|e| e.local_name() == Some("p"))
            .collect();
        assert_eq!(
            ps.iter().map(NodeRef::nth_of_type).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let span = doc
            .elements()
            .find(|e| e.local_name() == Some("span"))
            .unwrap();
        assert_eq!(span.nth_of_type(), 1);
        assert_eq!(span.index_in_parent(), 1);
    }

    #[test]
    fn descendant_walks_are_document_ordered() {
        let doc = body_of("<div><p>1</p><section><p>2</p><b>x</b></section></div>");
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        let tags: Vec<_> = div
            .descendant_elements()
            .filter_map(|e| e.local_name())
            .collect();
        assert_eq!(tags, ["p", "section", "p", "b"]);
        let texts: Vec<_> = div
            .descendant_text_nodes()
            .filter_map(|t| t.as_text())
            .collect();
        assert_eq!(texts, ["1", "2", "x"]);
        assert_eq!(div.text_content(), "12x");
        assert_eq!(
            div.descendants_with_nonblank_direct_text()
                .filter_map(|e| e.local_name())
                .collect::<Vec<_>>(),
            ["p", "p", "b"]
        );
    }

    #[test]
    fn document_order_comparison_and_sorting() {
        let doc = body_of("<div><a id='1'>x</a><span><a id='2'>y</a></span><a id='3'>z</a></div>");
        let mut nodes: Vec<_> = doc
            .elements()
            .filter(|e| e.local_name() == Some("a"))
            .collect();
        nodes.reverse();
        assert_eq!(nodes[0].cmp_document_order(&nodes[1]), Ordering::Greater);
        sort_document_order(&mut nodes);
        assert_eq!(
            nodes
                .iter()
                .filter_map(NodeRef::element_id)
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        assert!(nodes[1].is_descendant_of(&doc.body().unwrap()));
        assert!(!doc.body().unwrap().is_descendant_of(&nodes[1]));
    }

    #[test]
    fn deep_nesting_does_not_blow_up() {
        let depth = 500;
        let mut html = String::new();
        for _ in 0..depth {
            html.push_str("<div>");
        }
        html.push_str("deep");
        for _ in 0..depth {
            html.push_str("</div>");
        }
        let doc = body_of(&html);
        let deepest = doc
            .elements()
            .filter(|e| e.local_name() == Some("div"))
            .last()
            .unwrap();
        assert_eq!(deepest.text(), Some("deep"));
        assert_eq!(doc.body().unwrap().text_content(), "deep");
    }
}
