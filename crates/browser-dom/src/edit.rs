//! Mutation: `detach`, `drop_tree`, `drop_tag` and subtree removal.
//!
//! # `drop_tree` is free here
//!
//! lxml's `drop_tree()` has to do real work: it removes the element *and*
//! re-parents the element's tail text onto the previous sibling's tail (or the
//! parent's text), because in lxml the tail is a string slot on the element
//! being removed. In our tree the tail is already a separate sibling text node
//! that detaching the element does not touch — so `drop_tree` is literally
//! "detach the subtree", and the observable result is identical. See
//! `ARCHITECTURE.md` §1.2 and the module docs of [`crate::nav`].
//!
//! Detached nodes stay in the arena (their ids remain valid) but are no longer
//! reachable from the root, so they disappear from every traversal and from
//! serialisation.

use ego_tree::NodeId;

use crate::error::DomError;
use crate::node::{NodeData, NodeRef};
use crate::qualname::attr_name;
use crate::tree::Document;

impl Document {
    fn require(&self, id: NodeId) -> Result<(), DomError> {
        if self.tree().get(id).is_some() {
            Ok(())
        } else {
            Err(DomError::MissingNode(id))
        }
    }

    /// Detach a node (and its subtree) from its parent.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] for an unknown id, [`DomError::RootNotDetachable`]
    /// for the arena root.
    pub fn detach(&mut self, id: NodeId) -> Result<(), DomError> {
        self.require(id)?;
        if id == self.tree().root().id() {
            return Err(DomError::RootNotDetachable);
        }
        let parent = self.tree().get(id).and_then(|n| n.parent()).map(|n| n.id());
        if let Some(mut node) = self.tree_mut().get_mut(id) {
            node.detach();
        }
        if let Some(parent) = parent {
            crate::parse::merge_text_children(self.tree_mut(), parent);
        }
        Ok(())
    }

    /// lxml's `drop_tree()`: remove the element and its subtree, leaving the
    /// surrounding text alone.
    ///
    /// # Errors
    /// As [`Document::detach`].
    pub fn drop_tree(&mut self, id: NodeId) -> Result<(), DomError> {
        self.detach(id)
    }

    /// lxml's `drop_tag()`: remove the element but keep its children, splicing
    /// them into its place.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] for an unknown id, [`DomError::NotAnElement`]
    /// when `id` is not an element, [`DomError::RootNotDetachable`] when it has
    /// no parent.
    pub fn drop_tag(&mut self, id: NodeId) -> Result<(), DomError> {
        self.require(id)?;
        if !self.tree().get(id).is_some_and(|n| n.value().is_element()) {
            return Err(DomError::NotAnElement(id));
        }
        let Some(parent) = self.tree().get(id).and_then(|n| n.parent()).map(|n| n.id()) else {
            return Err(DomError::RootNotDetachable);
        };
        let children: Vec<NodeId> = self
            .tree()
            .get(id)
            .map(|n| n.children().map(|c| c.id()).collect())
            .unwrap_or_default();
        for child in children {
            if let Some(mut node) = self.tree_mut().get_mut(id) {
                node.insert_id_before(child);
            }
        }
        if let Some(mut node) = self.tree_mut().get_mut(id) {
            node.detach();
        }
        crate::parse::merge_text_children(self.tree_mut(), parent);
        Ok(())
    }

    /// Remove every child of a node, keeping the node itself.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] for an unknown id.
    pub fn remove_children(&mut self, id: NodeId) -> Result<(), DomError> {
        self.require(id)?;
        let children: Vec<NodeId> = self
            .tree()
            .get(id)
            .map(|n| n.children().map(|c| c.id()).collect())
            .unwrap_or_default();
        for child in children {
            if let Some(mut node) = self.tree_mut().get_mut(child) {
                node.detach();
            }
        }
        Ok(())
    }

    /// Detach every node matching `predicate`, returning how many were removed.
    ///
    /// The predicate is evaluated once, over the whole document in document
    /// order, before anything is removed — so removing a parent and one of its
    /// children in the same pass is safe.
    ///
    /// This is the primitive behind `strip_noise_tags` / `sanitize_for_ai` in
    /// `browser-markdown`.
    pub fn remove_matching(&mut self, predicate: impl Fn(NodeRef<'_>) -> bool) -> usize {
        let ids: Vec<NodeId> = self
            .nodes()
            .filter(|node| predicate(*node))
            .map(|node| node.id())
            .collect();
        let mut removed = 0;
        for id in ids {
            if let Some(mut node) = self.tree_mut().get_mut(id) {
                node.detach();
                removed += 1;
            }
        }
        removed
    }

    /// Detach every element whose local name is in `names`.
    pub fn remove_elements_named(&mut self, names: &[&str]) -> usize {
        self.remove_matching(|node| node.local_name().is_some_and(|name| names.contains(&name)))
    }

    /// Set an attribute on an element.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] / [`DomError::NotAnElement`].
    pub fn set_attr(
        &mut self,
        id: NodeId,
        name: &str,
        value: &str,
    ) -> Result<Option<String>, DomError> {
        self.require(id)?;
        let mut node = self
            .tree_mut()
            .get_mut(id)
            .ok_or(DomError::MissingNode(id))?;
        match node.value().as_element_mut() {
            Some(el) => Ok(el.set_attr(attr_name(name), value)),
            None => Err(DomError::NotAnElement(id)),
        }
    }

    /// Remove an attribute from an element.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] / [`DomError::NotAnElement`].
    pub fn remove_attr(&mut self, id: NodeId, name: &str) -> Result<Option<String>, DomError> {
        self.require(id)?;
        let mut node = self
            .tree_mut()
            .get_mut(id)
            .ok_or(DomError::MissingNode(id))?;
        match node.value().as_element_mut() {
            Some(el) => Ok(el.remove_attr(name)),
            None => Err(DomError::NotAnElement(id)),
        }
    }

    /// Append a new node as the last child of `parent`.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] for an unknown parent id.
    pub fn append_child(&mut self, parent: NodeId, data: NodeData) -> Result<NodeId, DomError> {
        self.require(parent)?;
        let id = self.tree_mut().orphan(data).id();
        if let Some(mut node) = self.tree_mut().get_mut(parent) {
            node.append_id(id);
        }
        Ok(id)
    }

    /// Insert a new node immediately before `sibling`.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] for an unknown id, [`DomError::RootNotDetachable`]
    /// when `sibling` has no parent.
    pub fn insert_before(&mut self, sibling: NodeId, data: NodeData) -> Result<NodeId, DomError> {
        self.require(sibling)?;
        if self.tree().get(sibling).and_then(|n| n.parent()).is_none() {
            return Err(DomError::RootNotDetachable);
        }
        let id = self.tree_mut().orphan(data).id();
        if let Some(mut node) = self.tree_mut().get_mut(sibling) {
            node.insert_id_before(id);
        }
        Ok(id)
    }

    /// Replace a node's payload in place.
    ///
    /// # Errors
    /// [`DomError::MissingNode`] for an unknown id.
    pub fn set_value(&mut self, id: NodeId, data: NodeData) -> Result<(), DomError> {
        self.require(id)?;
        if let Some(mut node) = self.tree_mut().get_mut(id) {
            *node.value() = data;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOptions;

    fn doc(html: &str) -> Document {
        Document::parse(html)
    }

    fn id_of(document: &Document, tag: &str) -> NodeId {
        document
            .elements()
            .find(|e| e.local_name() == Some(tag))
            .map(|e| e.id())
            .expect("element present")
    }

    #[test]
    fn drop_tree_removes_the_subtree_and_keeps_the_tail() {
        let mut document = doc("<div>before<b>bold<i>i</i></b>after</div>");
        let b = id_of(&document, "b");
        document.drop_tree(b).unwrap();
        let div = document
            .body()
            .and_then(|b| b.first_element_child())
            .unwrap();
        assert_eq!(div.outer_html(), "<div>beforeafter</div>");
        // adjacent text was re-merged, exactly as lxml's tail re-parenting does
        assert_eq!(div.children().count(), 1);
        assert_eq!(div.text(), Some("beforeafter"));
    }

    #[test]
    fn drop_tag_keeps_the_children() {
        let mut document = doc("<div>a<b>bold</b>c</div>");
        let b = id_of(&document, "b");
        document.drop_tag(b).unwrap();
        let div = document
            .body()
            .and_then(|b| b.first_element_child())
            .unwrap();
        assert_eq!(div.outer_html(), "<div>aboldc</div>");
        assert_eq!(div.children().count(), 1);
    }

    #[test]
    fn drop_tag_rejects_non_elements() {
        let mut document = doc("<div>text</div>");
        let text = document
            .body()
            .and_then(|b| b.first_element_child())
            .and_then(|d| d.first_child())
            .map(|t| t.id())
            .unwrap();
        assert!(matches!(
            document.drop_tag(text),
            Err(DomError::NotAnElement(_))
        ));
    }

    #[test]
    fn detaching_the_root_is_an_error() {
        let mut document = doc("<p>x</p>");
        let root = document.root().id();
        assert!(matches!(
            document.detach(root),
            Err(DomError::RootNotDetachable)
        ));
    }

    #[test]
    fn unknown_ids_are_reported() {
        // Node ids are arena indices: an id from a much larger document cannot
        // exist in a tiny one.
        let big = doc("<div><p>1</p><p>2</p><p>3</p><p>4</p><p>5</p><p>6</p><p>7</p></div>");
        let far = big.elements().last().map(|e| e.id()).expect("element");
        let mut small = doc("<p>x</p>");
        assert!(matches!(small.detach(far), Err(DomError::MissingNode(_))));
        assert!(matches!(
            small.remove_children(far),
            Err(DomError::MissingNode(_))
        ));
        assert!(matches!(
            small.set_attr(far, "a", "b"),
            Err(DomError::MissingNode(_))
        ));
    }

    #[test]
    fn remove_matching_strips_noise_tags() {
        let mut document = doc("<div><script>x</script><p>keep</p><style>y</style></div>");
        let removed = document.remove_elements_named(&["script", "style"]);
        assert_eq!(removed, 2);
        let div = document
            .body()
            .and_then(|b| b.first_element_child())
            .unwrap();
        assert_eq!(div.outer_html(), "<div><p>keep</p></div>");
    }

    #[test]
    fn remove_matching_handles_nested_matches() {
        let mut document =
            doc("<div><section><script>a</script></section><script>b</script></div>");
        assert_eq!(
            document.remove_matching(|n| n.local_name() == Some("script")),
            2
        );
        assert_eq!(
            document.body().map(|b| b.inner_html()),
            Some("<div><section></section></div>".to_owned())
        );
    }

    #[test]
    fn attributes_can_be_edited() {
        let mut document = doc("<div class='a'>x</div>");
        let div = id_of(&document, "div");
        assert_eq!(
            document.set_attr(div, "class", "a b").unwrap(),
            Some("a".to_owned())
        );
        assert!(document.node(div).unwrap().has_class("b"));
        assert_eq!(
            document.remove_attr(div, "class").unwrap(),
            Some("a b".to_owned())
        );
        assert!(!document.node(div).unwrap().has_class("a"));
        let text = document
            .node(div)
            .and_then(|d| d.first_child())
            .unwrap()
            .id();
        assert!(matches!(
            document.set_attr(text, "x", "y"),
            Err(DomError::NotAnElement(_))
        ));
    }

    #[test]
    fn nodes_can_be_added_and_replaced() {
        let mut document = doc("<div><p>x</p></div>");
        let div = id_of(&document, "div");
        let added = document
            .append_child(div, NodeData::Text("tail".into()))
            .unwrap();
        assert_eq!(
            document.node(div).map(|d| d.inner_html()),
            Some("<p>x</p>tail".to_owned())
        );
        document
            .set_value(added, NodeData::Comment("c".into()))
            .unwrap();
        assert_eq!(
            document.node(div).map(|d| d.inner_html()),
            Some("<p>x</p><!--c-->".to_owned())
        );
        let p = id_of(&document, "p");
        document
            .insert_before(p, NodeData::Text("head".into()))
            .unwrap();
        assert_eq!(
            document.node(div).map(|d| d.inner_html()),
            Some("head<p>x</p><!--c-->".to_owned())
        );
    }

    #[test]
    fn remove_children_empties_an_element() {
        let mut document = Document::parse_with(
            "<div><p>x</p><!--c-->text</div>",
            &ParseOptions::default().keep_comments(true),
        );
        let div = id_of(&document, "div");
        document.remove_children(div).unwrap();
        assert_eq!(
            document.node(div).map(|d| d.outer_html()),
            Some("<div></div>".to_owned())
        );
    }
}
