//! [`NodeData`], [`Element`], [`Attributes`] and the [`NodeRef`] handle.
//!
//! # Storage types
//!
//! html5ever hands us `StrTendril` values. We copy them into plain `String`s
//! when building the arena, because `StrTendril` is `!Send`/`!Sync` (it is
//! `Tendril<UTF8, NonAtomic>`) and that would make the whole [`Document`]
//! unusable from `rayon` — which `browser-parser`'s `relocate` and the spider
//! crawl loop both need. See `API.md` for the divergence note.
//!
//! [`Document`]: crate::tree::Document

use std::fmt;
use std::hash::{Hash, Hasher};

use ego_tree::NodeId;
use indexmap::IndexMap;
use markup5ever::{LocalName, QualName};

use crate::tree::Document;

/// An insertion-ordered attribute map.
///
/// Insertion order is **load-bearing**: the adaptive fingerprint in
/// `browser-core` scores attribute maps position by position, so the order
/// html5ever saw the attributes in is the order we keep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    inner: IndexMap<QualName, String>,
}

impl Attributes {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty map with room for `capacity` attributes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: IndexMap::with_capacity(capacity),
        }
    }

    /// Number of attributes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` when the element carries no attributes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Look an attribute up by its *local* name, ignoring the namespace.
    ///
    /// This is what every caller in the port wants (`el.attrs().get("href")`);
    /// use [`Attributes::get_qual`] when the namespace matters.
    #[must_use]
    pub fn get(&self, local: &str) -> Option<&str> {
        self.inner
            .iter()
            .find(|(k, _)| k.local.as_ref() == local)
            .map(|(_, v)| v.as_str())
    }

    /// Look an attribute up by its fully qualified name.
    #[must_use]
    pub fn get_qual(&self, name: &QualName) -> Option<&str> {
        self.inner.get(name).map(String::as_str)
    }

    /// `true` when an attribute with this local name exists.
    #[must_use]
    pub fn contains(&self, local: &str) -> bool {
        self.get(local).is_some()
    }

    /// Insert (or overwrite) an attribute, keeping the existing position when
    /// the key is already present — `IndexMap` semantics.
    pub fn insert(&mut self, name: QualName, value: impl Into<String>) -> Option<String> {
        self.inner.insert(name, value.into())
    }

    /// Insert an attribute in the empty namespace.
    pub fn insert_local(&mut self, local: &str, value: impl Into<String>) -> Option<String> {
        self.insert(crate::qualname::attr_name(local), value)
    }

    /// Insert only when no attribute with that name exists yet (html5ever's
    /// `add_attrs_if_missing`).
    pub fn insert_if_missing(&mut self, name: QualName, value: impl Into<String>) {
        if !self.inner.contains_key(&name) {
            self.inner.insert(name, value.into());
        }
    }

    /// Remove the first attribute with this local name, returning its value.
    ///
    /// Uses `shift_remove`, so the order of the remaining attributes is kept.
    pub fn remove(&mut self, local: &str) -> Option<String> {
        let key = self
            .inner
            .keys()
            .find(|k| k.local.as_ref() == local)
            .cloned()?;
        self.inner.shift_remove(&key)
    }

    /// Iterate `(local name, value)` pairs in document order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.inner
            .iter()
            .map(|(k, v)| (k.local.as_ref(), v.as_str()))
    }

    /// Iterate `(qualified name, value)` pairs in document order.
    pub fn iter_qual(&self) -> impl Iterator<Item = (&QualName, &str)> + '_ {
        self.inner.iter().map(|(k, v)| (k, v.as_str()))
    }

    /// Local attribute names, in document order.
    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        self.inner.keys().map(|k| k.local.as_ref())
    }

    /// Attribute values, in document order.
    pub fn values(&self) -> impl Iterator<Item = &str> + '_ {
        self.inner.values().map(String::as_str)
    }

    /// The `index`-th attribute in document order.
    ///
    /// Attributes are not arena nodes, so an XPath attribute node is addressed
    /// as `(element id, attribute index)`; this is the lookup for that.
    #[must_use]
    pub fn nth(&self, index: usize) -> Option<(&QualName, &str)> {
        self.inner.get_index(index).map(|(k, v)| (k, v.as_str()))
    }

    /// A plain owned map, in document order — the shape the adaptive
    /// fingerprint serialises.
    ///
    /// This is `browser_core::AttributeDict`, so the result drops straight into
    /// [`browser_core::ElementFingerprint::attributes`].
    #[must_use]
    pub fn to_index_map(&self) -> IndexMap<String, String> {
        self.inner
            .iter()
            .map(|(k, v)| (crate::qualname::to_string(k), v.clone()))
            .collect()
    }

    /// The same pairs as `browser-core`'s read-only presentation type — Python's
    /// `AttributesHandler`, with `search_values` and `json_string`.
    ///
    /// This crate stores attributes as `QualName -> String` because XPath needs the
    /// namespace and the CSS matcher needs the interning; `AttributeMap` is the
    /// flattened, user-facing view `browser-parser` returns from `Node::attrib`.
    ///
    /// ```
    /// use browser_dom::Document;
    ///
    /// let doc = Document::parse("<a href='/x' class='btn'>go</a>");
    /// let a = doc.elements().find(|e| e.tag() == "a").unwrap();
    /// let attrs = a.attrs().to_attribute_map();
    /// assert_eq!(attrs.get("href"), Some("/x"));
    /// assert_eq!(attrs.search_values("btn", false).count(), 1);
    /// ```
    #[must_use]
    pub fn to_attribute_map(&self) -> browser_core::AttributeMap {
        browser_core::AttributeMap::from(self.to_index_map())
    }
}

impl FromIterator<(QualName, String)> for Attributes {
    fn from_iter<T: IntoIterator<Item = (QualName, String)>>(iter: T) -> Self {
        let mut attrs = Self::new();
        for (name, value) in iter {
            attrs.insert_if_missing(name, value);
        }
        attrs
    }
}

impl<'a> IntoIterator for &'a Attributes {
    type Item = (&'a str, &'a str);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a str)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// An element node: its name, its attributes and the two derived indexes the
/// CSS matcher needs (`id`, `class`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    name: QualName,
    attrs: Attributes,
    id: Option<String>,
    classes: Vec<LocalName>,
}

impl Element {
    /// Build an element, deriving the `id`/`class` indexes from `attrs`.
    #[must_use]
    pub fn new(name: QualName, attrs: Attributes) -> Self {
        let id = attrs.get("id").map(str::to_owned);
        let classes = Self::derive_classes(&attrs);
        Self {
            name,
            attrs,
            id,
            classes,
        }
    }

    fn derive_classes(attrs: &Attributes) -> Vec<LocalName> {
        let mut classes: Vec<LocalName> = Vec::new();
        if let Some(value) = attrs.get("class") {
            for token in value.split_whitespace() {
                let atom = LocalName::from(token);
                if !classes.contains(&atom) {
                    classes.push(atom);
                }
            }
        }
        classes
    }

    fn reindex(&mut self) {
        self.id = self.attrs.get("id").map(str::to_owned);
        self.classes = Self::derive_classes(&self.attrs);
    }

    /// The fully qualified element name.
    #[must_use]
    pub fn name(&self) -> &QualName {
        &self.name
    }

    /// The interned local name (`div`, `a`, …).
    #[must_use]
    pub fn local_name(&self) -> &LocalName {
        &self.name.local
    }

    /// The local name as a string — lxml's `element.tag`.
    #[must_use]
    pub fn tag(&self) -> &str {
        self.name.local.as_ref()
    }

    /// The element's attributes, in document order.
    #[must_use]
    pub fn attrs(&self) -> &Attributes {
        &self.attrs
    }

    /// Value of an attribute, by local name.
    #[must_use]
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs.get(local)
    }

    /// `true` when the attribute exists (whatever its value).
    #[must_use]
    pub fn has_attr(&self, local: &str) -> bool {
        self.attrs.contains(local)
    }

    /// The `id` attribute, if any.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The whitespace-separated tokens of the `class` attribute, deduplicated,
    /// in source order.
    #[must_use]
    pub fn classes(&self) -> &[LocalName] {
        &self.classes
    }

    /// Case-sensitive class-token membership — lxml's `class_name in
    /// element.classes`.
    #[must_use]
    pub fn has_class(&self, class: &str) -> bool {
        self.classes.iter().any(|c| c.as_ref() == class)
    }

    /// ASCII-case-insensitive class-token membership (quirks-mode matching).
    #[must_use]
    pub fn has_class_ignore_ascii_case(&self, class: &str) -> bool {
        self.classes
            .iter()
            .any(|c| c.as_ref().eq_ignore_ascii_case(class))
    }

    /// Set an attribute, refreshing the derived `id`/`class` indexes.
    pub fn set_attr(&mut self, name: QualName, value: impl Into<String>) -> Option<String> {
        let previous = self.attrs.insert(name, value);
        self.reindex();
        previous
    }

    /// Insert an attribute only if absent — html5ever's `add_attrs_if_missing`.
    pub fn insert_attr_if_missing(&mut self, name: QualName, value: impl Into<String>) {
        self.attrs.insert_if_missing(name, value);
        self.reindex();
    }

    /// Remove an attribute by local name, refreshing the derived indexes.
    pub fn remove_attr(&mut self, local: &str) -> Option<String> {
        let previous = self.attrs.remove(local);
        self.reindex();
        previous
    }
}

/// The payload of one arena node.
///
/// The variants mirror `ARCHITECTURE.md` §1.2. There is deliberately **no**
/// `Cdata` variant: HTML has no CDATA sections outside foreign content, and
/// html5ever tokenises `<![CDATA[x]]>` in HTML content as a bogus comment. See
/// [`crate::parse::ParseOptions::keep_cdata`] and [`cdata_payload`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeData {
    /// The arena root. Exactly one per [`crate::tree::Document`].
    Document,
    /// `<!DOCTYPE …>`.
    Doctype {
        /// Root element name, e.g. `html`.
        name: String,
        /// `PUBLIC` identifier, empty when absent.
        public_id: String,
        /// `SYSTEM` identifier, empty when absent.
        system_id: String,
    },
    /// An element.
    Element(Element),
    /// A character-data node. Adjacent text nodes are always merged, so a
    /// parent never has two `Text` children in a row.
    Text(String),
    /// `<!-- … -->`. Also carries CDATA sections when `keep_cdata` is set.
    Comment(String),
    /// `<?target data>`.
    ProcessingInstruction {
        /// The PI target.
        target: String,
        /// The PI body.
        data: String,
    },
}

/// If `comment` is html5ever's bogus-comment encoding of a CDATA section
/// (`<![CDATA[x]]>` becomes a comment whose data is `[CDATA[x]]`), return the
/// section's payload.
#[must_use]
pub fn cdata_payload(comment: &str) -> Option<&str> {
    comment
        .strip_prefix("[CDATA[")
        .and_then(|rest| rest.strip_suffix("]]"))
}

impl NodeData {
    /// Is this the arena root?
    #[must_use]
    pub fn is_document(&self) -> bool {
        matches!(self, Self::Document)
    }

    /// Is this an element?
    #[must_use]
    pub fn is_element(&self) -> bool {
        matches!(self, Self::Element(_))
    }

    /// Is this a text node?
    #[must_use]
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text(_))
    }

    /// Is this a comment (including a preserved CDATA section)?
    #[must_use]
    pub fn is_comment(&self) -> bool {
        matches!(self, Self::Comment(_))
    }

    /// Is this a preserved CDATA section?
    #[must_use]
    pub fn is_cdata(&self) -> bool {
        match self {
            Self::Comment(c) => cdata_payload(c).is_some(),
            _ => false,
        }
    }

    /// Is this a doctype?
    #[must_use]
    pub fn is_doctype(&self) -> bool {
        matches!(self, Self::Doctype { .. })
    }

    /// Is this a processing instruction?
    #[must_use]
    pub fn is_processing_instruction(&self) -> bool {
        matches!(self, Self::ProcessingInstruction { .. })
    }

    /// Borrow the [`Element`] payload.
    #[must_use]
    pub fn as_element(&self) -> Option<&Element> {
        match self {
            Self::Element(el) => Some(el),
            _ => None,
        }
    }

    /// Mutably borrow the [`Element`] payload.
    #[must_use]
    pub fn as_element_mut(&mut self) -> Option<&mut Element> {
        match self {
            Self::Element(el) => Some(el),
            _ => None,
        }
    }

    /// Borrow the character data of a text node.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t.as_str()),
            _ => None,
        }
    }

    /// Borrow the body of a comment (or the payload of a CDATA section).
    #[must_use]
    pub fn as_comment(&self) -> Option<&str> {
        match self {
            Self::Comment(c) => Some(cdata_payload(c).unwrap_or(c.as_str())),
            _ => None,
        }
    }

    /// lxml-flavoured node name: the local name for elements, and the
    /// `#`-prefixed pseudo-names Python's `Selector.tag` uses elsewhere.
    #[must_use]
    pub fn tag(&self) -> &str {
        match self {
            Self::Document => "#document",
            Self::Doctype { .. } => "#doctype",
            Self::Element(el) => el.tag(),
            Self::Text(_) => "#text",
            Self::Comment(_) => "#comment",
            Self::ProcessingInstruction { .. } => "#processing-instruction",
        }
    }
}

/// A cheap, `Copy` handle to one node of a [`crate::tree::Document`].
///
/// Every navigation step is O(1): the arena stores parent/child/sibling ids, so
/// `parent()`, `next_sibling()` and friends are pointer chases, never scans.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    doc: &'a Document,
    id: NodeId,
}

impl<'a> NodeRef<'a> {
    pub(crate) fn new(doc: &'a Document, id: NodeId) -> Self {
        Self { doc, id }
    }

    /// The document this handle points into.
    #[must_use]
    pub fn document(&self) -> &'a Document {
        self.doc
    }

    /// The arena id — stable for the lifetime of the document, even after the
    /// node is detached.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    pub(crate) fn ego(&self) -> ego_tree::NodeRef<'a, NodeData> {
        // Safe by construction: a `NodeRef` is only ever handed out for ids the
        // document actually contains (see `Document::node`).
        match self.doc.tree().get(self.id) {
            Some(node) => node,
            None => unreachable!("NodeRef points at a node id that left the arena"),
        }
    }

    /// The node's payload.
    #[must_use]
    pub fn value(&self) -> &'a NodeData {
        self.ego().value()
    }

    /// Borrow the [`Element`] payload, if this is an element.
    #[must_use]
    pub fn as_element(&self) -> Option<&'a Element> {
        self.value().as_element()
    }

    /// Borrow the character data, if this is a text node.
    #[must_use]
    pub fn as_text(&self) -> Option<&'a str> {
        self.value().as_text()
    }

    /// Borrow the comment body, if this is a comment or CDATA section.
    #[must_use]
    pub fn as_comment(&self) -> Option<&'a str> {
        self.value().as_comment()
    }

    /// Borrow `(target, data)`, if this is a processing instruction.
    #[must_use]
    pub fn as_processing_instruction(&self) -> Option<(&'a str, &'a str)> {
        match self.value() {
            NodeData::ProcessingInstruction { target, data } => {
                Some((target.as_str(), data.as_str()))
            }
            _ => None,
        }
    }

    /// Is this the document's root element (`<html>`)?
    #[must_use]
    pub fn is_root_element(&self) -> bool {
        self.is_element() && self.parent().is_some_and(|p| p.is_document())
    }

    /// Is this an element?
    #[must_use]
    pub fn is_element(&self) -> bool {
        self.value().is_element()
    }

    /// Is this a text node?
    #[must_use]
    pub fn is_text(&self) -> bool {
        self.value().is_text()
    }

    /// Is this a comment (including preserved CDATA)?
    #[must_use]
    pub fn is_comment(&self) -> bool {
        self.value().is_comment()
    }

    /// Is this a preserved CDATA section?
    #[must_use]
    pub fn is_cdata(&self) -> bool {
        self.value().is_cdata()
    }

    /// Is this a doctype?
    #[must_use]
    pub fn is_doctype(&self) -> bool {
        self.value().is_doctype()
    }

    /// Is this a processing instruction?
    #[must_use]
    pub fn is_processing_instruction(&self) -> bool {
        self.value().is_processing_instruction()
    }

    /// Is this the arena root?
    #[must_use]
    pub fn is_document(&self) -> bool {
        self.value().is_document()
    }

    /// lxml-flavoured node name — see [`NodeData::tag`].
    #[must_use]
    pub fn tag(&self) -> &'a str {
        self.value().tag()
    }

    /// The element's local name, or `None` for non-elements.
    #[must_use]
    pub fn local_name(&self) -> Option<&'a str> {
        self.as_element().map(Element::tag)
    }

    /// The element's attributes; an empty map for non-elements.
    #[must_use]
    pub fn attrs(&self) -> &'a Attributes {
        static EMPTY: std::sync::OnceLock<Attributes> = std::sync::OnceLock::new();
        match self.as_element() {
            Some(el) => el.attrs(),
            None => EMPTY.get_or_init(Attributes::new),
        }
    }

    /// Value of an attribute; always `None` for non-elements.
    #[must_use]
    pub fn attr(&self, local: &str) -> Option<&'a str> {
        self.as_element().and_then(|el| el.attr(local))
    }

    /// The `id` attribute; always `None` for non-elements.
    #[must_use]
    pub fn element_id(&self) -> Option<&'a str> {
        self.as_element().and_then(Element::id)
    }

    /// Case-sensitive class-token membership; `false` for non-elements.
    #[must_use]
    pub fn has_class(&self, class: &str) -> bool {
        self.as_element().is_some_and(|el| el.has_class(class))
    }
}

impl PartialEq for NodeRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.doc, other.doc) && self.id == other.id
    }
}

impl Eq for NodeRef<'_> {}

impl Hash for NodeRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::from_ref(self.doc).hash(state);
        self.id.hash(state);
    }
}

impl PartialOrd for NodeRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Arena order, **not** document order.
///
/// This exists so `NodeRef` satisfies the `Copy + Eq + Ord` bound the XPath
/// engine's `TreeModel` asks for (cheap sorting / dedup of node sets). Use
/// [`NodeRef::cmp_document_order`] when you need the real thing.
impl Ord for NodeRef<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        std::ptr::from_ref(self.doc)
            .cmp(&std::ptr::from_ref(other.doc))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl fmt::Display for NodeRef<'_> {
    /// The outer HTML of the node — Python's `str(selector)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.outer_html())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qualname::{attr_name, html_name};

    fn attrs(pairs: &[(&str, &str)]) -> Attributes {
        pairs
            .iter()
            .map(|(k, v)| (attr_name(k), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn attributes_keep_insertion_order() {
        let a = attrs(&[("z", "1"), ("a", "2"), ("m", "3")]);
        assert_eq!(a.keys().collect::<Vec<_>>(), ["z", "a", "m"]);
        assert_eq!(a.get("a"), Some("2"));
        assert_eq!(a.get("nope"), None);
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn duplicate_attributes_keep_the_first_value() {
        let a = attrs(&[("id", "first"), ("id", "second")]);
        assert_eq!(a.len(), 1);
        assert_eq!(a.get("id"), Some("first"));
    }

    #[test]
    fn removing_an_attribute_preserves_order() {
        let mut a = attrs(&[("a", "1"), ("b", "2"), ("c", "3")]);
        assert_eq!(a.remove("b"), Some("2".to_owned()));
        assert_eq!(a.keys().collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(a.remove("b"), None);
    }

    #[test]
    fn element_derives_id_and_classes() {
        let el = Element::new(
            html_name("div"),
            attrs(&[("id", "x"), ("class", " a  b a ")]),
        );
        assert_eq!(el.id(), Some("x"));
        assert_eq!(el.classes().len(), 2);
        assert!(el.has_class("a"));
        assert!(el.has_class("b"));
        assert!(!el.has_class("A"));
        assert!(el.has_class_ignore_ascii_case("A"));
    }

    #[test]
    fn mutating_attributes_reindexes() {
        let mut el = Element::new(html_name("div"), Attributes::new());
        el.set_attr(attr_name("class"), "one two");
        assert!(el.has_class("two"));
        el.set_attr(attr_name("id"), "here");
        assert_eq!(el.id(), Some("here"));
        el.remove_attr("class");
        assert!(el.classes().is_empty());
        assert!(!el.has_class("one"));
    }

    #[test]
    fn cdata_payload_only_matches_the_bogus_comment_shape() {
        assert_eq!(cdata_payload("[CDATA[hello]]"), Some("hello"));
        assert_eq!(cdata_payload("just a comment"), None);
        assert_eq!(cdata_payload("[CDATA[unterminated"), None);
    }

    #[test]
    fn attributes_are_index_addressable() {
        let a = attrs(&[("a", "1"), ("b", "2")]);
        assert_eq!(
            a.nth(0).map(|(k, v)| (k.local.as_ref(), v)),
            Some(("a", "1"))
        );
        assert_eq!(
            a.nth(1).map(|(k, v)| (k.local.as_ref(), v)),
            Some(("b", "2"))
        );
        assert!(a.nth(2).is_none());
    }

    #[test]
    fn node_data_tags() {
        assert_eq!(NodeData::Text("x".into()).tag(), "#text");
        assert_eq!(NodeData::Comment("x".into()).tag(), "#comment");
        assert_eq!(NodeData::Document.tag(), "#document");
        assert!(NodeData::Comment("[CDATA[x]]".into()).is_cdata());
        assert!(!NodeData::Comment("x".into()).is_cdata());
    }
}
