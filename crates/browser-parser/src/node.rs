//! [`Node`] — Python's dual-natured `Selector`, and its complete text-node
//! degradation table.
//!
//! A `Selector` wraps **either** an element **or** one of lxml's "smart
//! strings" (the `str` subclass returned by `/text()`, `/@attr`, `::text` and
//! `::attr()`), and `_is_text_node` gates nearly every method so that a text
//! selector degrades instead of raising. An enum is the honest translation
//! (`ARCHITECTURE.md` §3): every method matches on the variant.
//!
//! # The degradation table (`parser.md` §2.1), reproduced in full
//!
//! | member | on a text node |
//! |---|---|
//! | [`tag`](Node::tag) | `"#text"` |
//! | [`text`](Node::text) | the string value itself |
//! | [`attrib`](Node::attrib) | an empty map |
//! | [`get_attr`](Node::get_attr) | `Err(ParseError::TextNodeHasNoAttributes)` |
//! | [`contains_attr`](Node::contains_attr) | `false` |
//! | [`has_class`](Node::has_class) | `false` |
//! | [`html_content`](Node::html_content), [`prettify`](Node::prettify), [`get`](Node::get), `Display` | the string value |
//! | [`body`](Node::body) | empty |
//! | [`get_all_text`](Node::get_all_text) | the string value |
//! | [`below_elements`](Node::below_elements), [`children`](Node::children), [`css`](Node::css), [`xpath`](Node::xpath), [`find_all`](Node::find_all), [`find_similar`](Node::find_similar), [`find_by_text`](Node::find_by_text), [`find_by_regex`](Node::find_by_regex) | empty [`Nodes`] |
//! | [`ancestors`](Node::ancestors) | yields nothing |
//! | [`next`](Node::next), [`previous`](Node::previous) | `None` |
//! | [`parent`](Node::parent) | **not** guarded — the owning element |
//! | [`siblings`](Node::siblings) | **not** guarded either — the owner's children |
//! | selector generation | `""` |
//! | `Debug` | `<text='…'>` |
//!
//! `parent` and `siblings` really are unguarded in Python: lxml's smart strings
//! implement `getparent()`, and `siblings` is written in terms of `parent`.
//!
//! ```
//! use browser_parser::document::Document;
//!
//! let doc = Document::parse("<div><p>hi</p><span>x</span></div>");
//! let text = doc.css("p::text").unwrap().first().cloned().unwrap();
//!
//! assert!(text.is_text_node());
//! assert_eq!(text.tag(), "#text");
//! assert_eq!(text.text(), "hi");
//! assert!(text.css("*").unwrap().is_empty());
//! assert!(text.next().is_none());
//! assert_eq!(text.parent().unwrap().tag(), "p");  // unguarded, on purpose
//! ```

use std::borrow::Cow;
use std::fmt;

use browser_core::attrs::AttributeMap;
use browser_core::fingerprint::ElementFingerprint;
use browser_core::text::{ReOptions, RegexSpec, TextExt};
use browser_dom::serialize::truncate_repr;
use browser_dom::NodeRef;
use browser_selectors::MatchValue;
use browser_xpath::eval::Variables;
use browser_xpath::model::{DomModel, XNode};
use browser_xpath::value::Value;

use crate::adaptive::{ADAPTIVE_IGNORED, AUTO_SAVE_IGNORED};
use crate::config::{AllTextOpts, ByRegexOpts, ByTextOpts, CssOpts, SimilarOpts};
use crate::document::{compile_css, compile_xpath, Body, Document};
use crate::error::{ParseError, Result};
use crate::find::FindQuery;
use crate::nodes::Nodes;

/// Which lxml smart string a [`Node::Text`] came from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TextKind {
    /// The owner's `element.text` — the character data before its first child.
    Text,
    /// The owner's `element.tail` — the character data after its end tag. The
    /// owner is then the node the text *trails*, which is lxml's `getparent()`
    /// for such a string and one level below the tree parent.
    Tail,
    /// An attribute value, from `::attr(name)` or `/@name`.
    Attribute(String),
}

/// One node of a parsed document: an element, or a text/attribute string.
///
/// Cheap to clone (a reference, a node id and a borrowed string), `Send`,
/// `Sync`, and comparable — two handles for the same underlying node are equal.
#[derive(Clone)]
pub enum Node<'a> {
    /// An element — or, from an XPath result, a comment or processing
    /// instruction, which `browser-dom` models as tree nodes too.
    Element {
        /// The document this node belongs to.
        doc: &'a Document,
        /// The tree handle.
        node: NodeRef<'a>,
    },
    /// A string node: `element.text`, `element.tail` or an attribute value.
    Text {
        /// The document this node belongs to.
        doc: &'a Document,
        /// lxml's `getparent()` for this smart string.
        owner: NodeRef<'a>,
        /// Which flavour of string this is.
        kind: TextKind,
        /// The string value.
        value: Cow<'a, str>,
    },
}

impl PartialEq for Node<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Element { node: a, .. }, Self::Element { node: b, .. }) => a == b,
            (
                Self::Text {
                    owner: ao,
                    kind: ak,
                    value: av,
                    ..
                },
                Self::Text {
                    owner: bo,
                    kind: bk,
                    value: bv,
                    ..
                },
            ) => ao == bo && ak == bk && av == bv,
            _ => false,
        }
    }
}

impl Eq for Node<'_> {}

/// `Selector.__repr__`.
impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { value, .. } => write!(f, "<text='{}'>", truncate_repr(value)),
            Self::Element { .. } => {
                let content =
                    browser_dom::serialize::clean_spaces(&self.html_content()).into_owned();
                write!(f, "<data='{}'", truncate_repr(&content))?;
                if let Some(parent) = self.parent() {
                    let parent_content =
                        browser_dom::serialize::clean_spaces(&parent.html_content()).into_owned();
                    write!(f, " parent='{}'", truncate_repr(&parent_content))?;
                }
                write!(f, ">")
            }
        }
    }
}

/// `Selector.__str__` — the string value for a text node, the outer HTML for an
/// element.
impl fmt::Display for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.get())
    }
}

/// `Selector.body` for a node that is not the document root.
static EMPTY_BODY: Body = Body::Text(String::new());

impl<'a> Node<'a> {
    // -- construction -----------------------------------------------------

    /// Wrap a tree node (element, comment, processing instruction, document).
    #[must_use]
    pub fn element(doc: &'a Document, node: NodeRef<'a>) -> Self {
        Self::Element { doc, node }
    }

    /// Wrap a text node the way lxml's smart strings do: a text node with a
    /// previous sibling is that sibling's *tail*, otherwise it is the parent's
    /// *text*.
    #[must_use]
    pub fn from_text_node(doc: &'a Document, node: NodeRef<'a>) -> Self {
        let value = Cow::Borrowed(node.as_text().unwrap_or(""));
        match node.prev_sibling() {
            Some(previous) => Self::Text {
                doc,
                owner: previous,
                kind: TextKind::Tail,
                value,
            },
            None => Self::Text {
                doc,
                owner: node.parent().unwrap_or(node),
                kind: TextKind::Text,
                value,
            },
        }
    }

    /// Wrap an attribute value.
    #[must_use]
    pub fn from_attribute(
        doc: &'a Document,
        owner: NodeRef<'a>,
        name: &str,
        value: &'a str,
    ) -> Self {
        Self::Text {
            doc,
            owner,
            kind: TextKind::Attribute(name.to_owned()),
            value: Cow::Borrowed(value),
        }
    }

    /// Convert one CSS [`Match`](browser_selectors::Match).
    #[must_use]
    pub fn from_match(doc: &'a Document, matched: &browser_selectors::Match<'a>) -> Self {
        match matched.value() {
            MatchValue::Element => Self::element(doc, matched.element()),
            MatchValue::Text { node, .. } => Self::from_text_node(doc, *node),
            MatchValue::Attr { name, value } => {
                Self::from_attribute(doc, matched.element(), name, value)
            }
        }
    }

    /// Convert one XPath node.
    ///
    /// Elements stay elements; text nodes become [`Node::Text`]; attribute
    /// nodes become [`TextKind::Attribute`]; comments, processing instructions
    /// and the document node stay [`Node::Element`] (they are tree nodes here,
    /// where lxml models them as `HtmlElement` subclasses); a namespace node
    /// degrades to its URI as text.
    #[must_use]
    pub fn from_xnode(doc: &'a Document, node: XNode<'a>) -> Self {
        if let Some(tree) = node.as_tree_node() {
            if tree.is_text() {
                return Self::from_text_node(doc, tree);
            }
            return Self::element(doc, tree);
        }
        if let Some((name, value)) = node.as_attribute() {
            return Self::from_attribute(doc, node.anchor(), name, value);
        }
        Self::Text {
            doc,
            owner: node.anchor(),
            kind: TextKind::Text,
            value: node.string_value(),
        }
    }

    // -- identity ---------------------------------------------------------

    /// The document this node belongs to.
    #[must_use]
    pub fn doc(&self) -> &'a Document {
        match self {
            Self::Element { doc, .. } | Self::Text { doc, .. } => doc,
        }
    }

    /// Python's `_is_text_node` — true for `::text` **and** `::attr()` results.
    #[must_use]
    pub fn is_text_node(&self) -> bool {
        matches!(self, Self::Text { .. })
    }

    /// Which flavour of string this is, or `None` for an element.
    #[must_use]
    pub fn text_kind(&self) -> Option<&TextKind> {
        match self {
            Self::Text { kind, .. } => Some(kind),
            Self::Element { .. } => None,
        }
    }

    /// The tree handle backing this node — the element itself, or the owner of
    /// a string node (lxml's `getparent()`).
    #[must_use]
    pub fn element_ref(&self) -> NodeRef<'a> {
        match self {
            Self::Element { node, .. } => *node,
            Self::Text { owner, .. } => *owner,
        }
    }

    /// The tree handle, but only for a real element node.
    #[must_use]
    pub fn as_node_ref(&self) -> Option<NodeRef<'a>> {
        match self {
            Self::Element { node, .. } => Some(*node),
            Self::Text { .. } => None,
        }
    }

    fn element_parts(&self) -> Option<(&'a Document, NodeRef<'a>)> {
        match self {
            Self::Element { doc, node } => Some((doc, *node)),
            Self::Text { .. } => None,
        }
    }

    // -- properties -------------------------------------------------------

    /// `Selector.tag` — `"#text"` for a string node.
    #[must_use]
    pub fn tag(&self) -> &'a str {
        match self {
            Self::Element { node, .. } => node.tag(),
            Self::Text { .. } => "#text",
        }
    }

    /// `Selector.text` — the **direct** text before the first child, or the
    /// string node's own value. Never `None`; Python returns `""`.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Element { node, .. } => node.text().unwrap_or(""),
            Self::Text { value, .. } => value,
        }
    }

    /// `Selector.attrib` — an empty map for a string node.
    #[must_use]
    pub fn attrib(&self) -> AttributeMap {
        match self {
            Self::Element { node, .. } => node.attrs().to_attribute_map(),
            Self::Text { .. } => AttributeMap::new(),
        }
    }

    /// One attribute value, or `None` — `selector.attrib.get(name)`.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&'a str> {
        match self {
            Self::Element { node, .. } => node.attr(name),
            Self::Text { .. } => None,
        }
    }

    /// `selector[name]` — the fallible subscript.
    ///
    /// # Errors
    ///
    /// [`ParseError::TextNodeHasNoAttributes`] on a string node (Python's
    /// `TypeError`), [`ParseError::MissingAttribute`] when the element has no
    /// such attribute (Python's `KeyError`).
    pub fn get_attr(&self, name: &str) -> Result<&'a str> {
        match self {
            Self::Text { .. } => Err(ParseError::TextNodeHasNoAttributes),
            Self::Element { node, .. } => node
                .attr(name)
                .ok_or_else(|| ParseError::MissingAttribute(name.to_owned())),
        }
    }

    /// `name in selector` — `false` on a string node.
    #[must_use]
    pub fn contains_attr(&self, name: &str) -> bool {
        match self {
            Self::Element { node, .. } => node.attrs().contains(name),
            Self::Text { .. } => false,
        }
    }

    /// `Selector.has_class` — `false` on a string node.
    #[must_use]
    pub fn has_class(&self, class_name: &str) -> bool {
        match self {
            Self::Element { node, .. } => node.has_class(class_name),
            Self::Text { .. } => false,
        }
    }

    /// `Selector.html_content` — the **outer** HTML, tail excluded.
    #[must_use]
    pub fn html_content(&self) -> String {
        match self {
            Self::Element { node, .. } => node.outer_html().trim().to_owned(),
            Self::Text { value, .. } => value.clone().into_owned(),
        }
    }

    /// `Selector.prettify`.
    #[must_use]
    pub fn prettify(&self) -> String {
        match self {
            Self::Element { node, .. } => node.pretty().trim().to_owned(),
            Self::Text { value, .. } => value.clone().into_owned(),
        }
    }

    /// `Selector.get` / `extract_first` — the string value, or the outer HTML.
    #[must_use]
    pub fn get(&self) -> String {
        self.html_content()
    }

    /// `Selector.getall` / `extract` — a one-element list.
    #[must_use]
    pub fn getall(&self) -> Vec<String> {
        vec![self.get()]
    }

    /// `Selector.body` — the original `content` argument, and only on the
    /// document's root node. Every other node reports an empty body, exactly as
    /// Python's child selectors do.
    #[must_use]
    pub fn body(&self) -> &Body {
        match self {
            Self::Element { doc, node } if Some(*node) == doc.dom().root_element() => doc.body(),
            _ => &EMPTY_BODY,
        }
    }

    /// `Selector.urljoin`.
    #[must_use]
    pub fn urljoin(&self, relative: &str) -> String {
        self.doc().urljoin(relative)
    }

    /// `Selector.json` — the raw body, then the direct text, then all the text.
    ///
    /// # Errors
    ///
    /// [`ParseError::Json`] when nothing along that chain is valid JSON.
    pub fn json(&self) -> Result<serde_json::Value> {
        if let Self::Text { value, .. } = self {
            return Ok(value.json_value()?);
        }
        let body = self.body();
        if !body.is_empty() {
            return Ok(body.as_str().json_value()?);
        }
        let text = self.text();
        if !text.is_empty() {
            return Ok(text.json_value()?);
        }
        Ok(self
            .get_all_text_with(&AllTextOpts::new().strip(true))
            .json_value()?)
    }

    /// `Selector.re` — the pattern applied to this node's **direct** text.
    ///
    /// # Errors
    ///
    /// [`ParseError::Regex`] when the pattern does not compile or run.
    pub fn re(&self, pattern: RegexSpec<'_>, opts: ReOptions) -> Result<Vec<String>> {
        Ok(self.text().re_all(pattern, opts)?)
    }

    /// `Selector.re_first`.
    ///
    /// # Errors
    ///
    /// As [`Node::re`].
    pub fn re_first(&self, pattern: RegexSpec<'_>, opts: ReOptions) -> Result<Option<String>> {
        Ok(self.text().re_first(pattern, opts)?)
    }

    // -- navigation -------------------------------------------------------

    /// `Selector.parent`. **Not** guarded for string nodes.
    #[must_use]
    pub fn parent(&self) -> Option<Node<'a>> {
        match self {
            Self::Element { doc, node } => node.parent_element().map(|p| Self::element(doc, p)),
            Self::Text { doc, owner, .. } => Some(Self::element(doc, *owner)),
        }
    }

    /// `Selector.children` — lxml's `iterchildren()` minus comments, so
    /// elements and processing instructions but never text.
    ///
    /// The `<!DOCTYPE>` is skipped as well: libxml2 keeps it out of the element
    /// tree entirely (it lives in `docinfo`), so no Python caller can ever see
    /// one here, while html5ever models it as a real child of the document node.
    #[must_use]
    pub fn children(&self) -> Nodes<'a> {
        let Some((doc, node)) = self.element_parts() else {
            return Nodes::new();
        };
        node.iterchildren()
            .filter(|child| !child.is_comment() && !child.is_doctype())
            .map(|child| Self::element(doc, child))
            .collect()
    }

    /// `Selector.below_elements` — every descendant **element**, self excluded.
    #[must_use]
    pub fn below_elements(&self) -> Nodes<'a> {
        let Some((doc, node)) = self.element_parts() else {
            return Nodes::new();
        };
        node.descendant_elements()
            .map(|child| Self::element(doc, child))
            .collect()
    }

    /// `Selector.siblings` — the parent's [`children`](Node::children) minus
    /// this node. Unguarded for string nodes, exactly as Python leaves it.
    #[must_use]
    pub fn siblings(&self) -> Nodes<'a> {
        match self.parent() {
            Some(parent) => parent
                .children()
                .into_iter()
                .filter(|child| child != self)
                .collect(),
            None => Nodes::new(),
        }
    }

    /// `Selector.next` — the next sibling that is neither text, a comment, nor
    /// the `<!DOCTYPE>` (which libxml2 does not put in the tree at all, so
    /// `<html>.getprevious()` is `None` in Python).
    #[must_use]
    pub fn next(&self) -> Option<Node<'a>> {
        let (doc, node) = self.element_parts()?;
        node.next_siblings()
            .find(|sibling| !sibling.is_text() && !sibling.is_comment() && !sibling.is_doctype())
            .map(|sibling| Self::element(doc, sibling))
    }

    /// `Selector.previous`.
    #[must_use]
    pub fn previous(&self) -> Option<Node<'a>> {
        let (doc, node) = self.element_parts()?;
        node.prev_siblings()
            .find(|sibling| !sibling.is_text() && !sibling.is_comment() && !sibling.is_doctype())
            .map(|sibling| Self::element(doc, sibling))
    }

    /// `Selector.iterancestors` — the immediate parent first, up to `<html>`.
    /// Yields nothing for a string node.
    pub fn ancestors(&self) -> impl Iterator<Item = Node<'a>> + 'a {
        let parts = self.element_parts();
        parts
            .into_iter()
            .flat_map(|(doc, node)| node.ancestors().map(move |a| Self::element(doc, a)))
    }

    /// `Selector.find_ancestor` — the closest ancestor satisfying `predicate`.
    #[must_use]
    pub fn find_ancestor(&self, predicate: impl Fn(&Node<'a>) -> bool) -> Option<Node<'a>> {
        self.ancestors().find(|a| predicate(a))
    }

    /// `Selector.path` — the ancestors as a list, bottom-up, self excluded.
    #[must_use]
    pub fn path(&self) -> Nodes<'a> {
        self.ancestors().collect()
    }

    // -- selection --------------------------------------------------------

    /// `Selector.css` with Python's defaults.
    ///
    /// # Errors
    ///
    /// [`ParseError::CssSyntax`] carrying
    /// `Invalid CSS selector '{selector}': {reason}`.
    pub fn css(&self, selector: &str) -> Result<Nodes<'a>> {
        self.css_with(selector, &CssOpts::default())
    }

    /// The first CSS match, or `None` — the reference implementation's `css_first`.
    ///
    /// # Errors
    ///
    /// As [`Node::css`].
    pub fn css_first(&self, selector: &str) -> Result<Option<Node<'a>>> {
        Ok(self.css(selector)?.into_iter().next())
    }

    /// `Selector.css` with every keyword argument.
    ///
    /// The comma-splitting branch exists **only** so adaptive storage keeps one
    /// fingerprint per sub-selector; it is taken when this document has
    /// adaptive enabled *and* the selector text contains a comma, exactly as
    /// Python decides it. On that branch results are concatenated per part and
    /// **not** de-duplicated, so `css("p, p")` returns each `p` twice — Python's
    /// behaviour, not a bug.
    ///
    /// # Errors
    ///
    /// [`ParseError::CssSyntax`], plus [`ParseError::Storage`] from the
    /// adaptive save/relocate branch.
    pub fn css_with(&self, selector: &str, opts: &CssOpts) -> Result<Nodes<'a>> {
        let Some((doc, node)) = self.element_parts() else {
            return Ok(Nodes::new());
        };
        let group = compile_css(selector)?;

        if !doc.adaptive_enabled() || !selector.contains(',') {
            let identifier = opts
                .identifier
                .clone()
                .unwrap_or_else(|| selector.to_owned());
            let hits = Nodes::from_matches(doc, group.select(node));
            return self.dispatch_adaptive(&identifier, opts, hits);
        }

        let mut results = Nodes::new();
        for (part, canonical) in group.parts().iter().zip(group.canonical_parts()) {
            let identifier = opts.identifier.clone().unwrap_or(canonical);
            let hits = Nodes::from_matches(doc, part.select(node));
            results.extend(self.dispatch_adaptive(&identifier, opts, hits)?);
        }
        Ok(results)
    }

    /// `Selector.xpath` with Python's defaults and no variables.
    ///
    /// # Errors
    ///
    /// [`ParseError::XPathSyntax`] carrying `Invalid XPath selector: {expr}`,
    /// or [`ParseError::NotANodeSet`] for a scalar result.
    pub fn xpath(&self, expression: &str) -> Result<Nodes<'a>> {
        self.xpath_with(expression, &CssOpts::default(), &Variables::new())
    }

    /// `Selector.xpath` with every keyword argument.
    ///
    /// `variables` is Python's `**kwargs`: `page.xpath("//td[text()=$cell]",
    /// cell_text="Cell 1")` becomes a [`Variables`] map.
    ///
    /// # Errors
    ///
    /// As [`Node::xpath`], plus [`ParseError::Storage`] from the adaptive
    /// branch.
    pub fn xpath_with(
        &self,
        expression: &str,
        opts: &CssOpts,
        variables: &Variables,
    ) -> Result<Nodes<'a>> {
        let Some((doc, _)) = self.element_parts() else {
            return Ok(Nodes::new());
        };
        let hits = match self.eval_xpath(expression, variables)? {
            Value::NodeSet(list) => list
                .into_iter()
                .map(|x| Self::from_xnode(doc, x))
                .collect::<Nodes<'a>>(),
            other => {
                return Err(ParseError::NotANodeSet {
                    expr: expression.to_owned(),
                    actual: other.type_name(),
                })
            }
        };
        let identifier = opts
            .identifier
            .clone()
            .unwrap_or_else(|| expression.to_owned());
        self.dispatch_adaptive(&identifier, opts, hits)
    }

    /// Evaluate an XPath expression and keep whatever type it produced.
    ///
    /// `count(…)`, `string(…)` and boolean predicates come back as scalars here
    /// instead of the [`ParseError::NotANodeSet`] [`Node::xpath`] raises.
    ///
    /// # Errors
    ///
    /// [`ParseError::XPathSyntax`] when the expression does not compile or
    /// evaluate.
    pub fn xpath_value(&self, expression: &str, variables: &Variables) -> Result<Value<XNode<'a>>> {
        if self.is_text_node() {
            return Ok(Value::empty_node_set());
        }
        self.eval_xpath(expression, variables)
    }

    fn eval_xpath(&self, expression: &str, variables: &Variables) -> Result<Value<XNode<'a>>> {
        let Some((doc, node)) = self.element_parts() else {
            return Ok(Value::empty_node_set());
        };
        let compiled = compile_xpath(expression)?;
        let model = DomModel::new(doc.dom());
        let context = model.node(node);
        compiled
            .eval(&model, context, variables)
            .map_err(|_| ParseError::XPathSyntax(format!("Invalid XPath selector: {expression}")))
    }

    /// `Selector.find_all`.
    ///
    /// # Errors
    ///
    /// [`ParseError::EmptyFindQuery`] for a query with no filters at all, or a
    /// CSS error from the selector the query builds.
    pub fn find_all(&self, query: &FindQuery<'_>) -> Result<Nodes<'a>> {
        crate::find::find_all(self, query)
    }

    /// `Selector.find` — the first result of [`Node::find_all`].
    ///
    /// # Errors
    ///
    /// As [`Node::find_all`].
    pub fn find(&self, query: &FindQuery<'_>) -> Result<Option<Node<'a>>> {
        Ok(self.find_all(query)?.into_iter().next())
    }

    /// `Selector.find_similar` with Python's defaults.
    #[must_use]
    pub fn find_similar_default(&self) -> Nodes<'a> {
        self.find_similar(&SimilarOpts::default())
    }

    /// `Selector.find_similar`.
    #[must_use]
    pub fn find_similar(&self, opts: &SimilarOpts) -> Nodes<'a> {
        crate::similar::find_similar(self, opts)
    }

    /// `Selector.find_by_text`.
    #[must_use]
    pub fn find_by_text(&self, text: &str, opts: &ByTextOpts) -> Nodes<'a> {
        crate::text::find_by_text(self, text, opts)
    }

    /// `Selector.find_by_regex`.
    ///
    /// # Errors
    ///
    /// [`ParseError::Regex`] when the pattern does not compile or run.
    pub fn find_by_regex(&self, pattern: RegexSpec<'_>, opts: &ByRegexOpts) -> Result<Nodes<'a>> {
        crate::text::find_by_regex(self, pattern, opts)
    }

    /// `Selector.get_all_text` with Python's defaults.
    #[must_use]
    pub fn get_all_text(&self) -> String {
        self.get_all_text_with(&AllTextOpts::default())
    }

    /// `Selector.get_all_text`.
    #[must_use]
    pub fn get_all_text_with(&self, opts: &AllTextOpts) -> String {
        crate::text::get_all_text(self, opts)
    }

    // -- selector generation ---------------------------------------------

    /// `generate_css_selector` — shortest path, stopping at the nearest ancestor
    /// with an `id`.
    #[must_use]
    pub fn css_path(&self) -> String {
        crate::generate::general_selection(self, true, false)
    }

    /// `generate_full_css_selector` — the whole path down from `<body>`.
    #[must_use]
    pub fn css_path_full(&self) -> String {
        crate::generate::general_selection(self, true, true)
    }

    /// `generate_xpath_selector`.
    #[must_use]
    pub fn xpath_path(&self) -> String {
        crate::generate::general_selection(self, false, false)
    }

    /// `generate_full_xpath_selector`.
    #[must_use]
    pub fn xpath_path_full(&self) -> String {
        crate::generate::general_selection(self, false, true)
    }

    // -- the adaptive layer ----------------------------------------------

    /// `Selector.save` — store this node's fingerprint under `identifier`.
    ///
    /// # Errors
    ///
    /// As [`Document::save`].
    pub fn save(&self, identifier: &str) -> Result<()> {
        self.doc().save(self, identifier)
    }

    /// `Selector.retrieve`.
    ///
    /// # Errors
    ///
    /// As [`Document::retrieve`].
    pub fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>> {
        self.doc().retrieve(identifier)
    }

    /// `Selector.relocate` — score every descendant element of this node and
    /// return everyone tied at the best score, when it clears `percentage`.
    #[must_use]
    pub fn relocate(&self, fingerprint: &ElementFingerprint, percentage: u8) -> Nodes<'a> {
        let Some((doc, node)) = self.element_parts() else {
            return Nodes::new();
        };
        let candidates: Vec<NodeRef<'a>> = node.descendant_elements().collect();
        crate::adaptive::relocate_among(fingerprint, candidates, percentage)
            .into_iter()
            .map(|found| Self::element(doc, found))
            .collect()
    }

    /// This node's adaptive fingerprint, as it would be stored.
    #[must_use]
    pub fn fingerprint(&self) -> ElementFingerprint {
        crate::adaptive::element_to_fingerprint(self.element_ref())
    }

    /// The `xpath()` save/relocate branch of `parser.md` §3.5, shared by
    /// [`Node::css_with`] and [`Node::xpath_with`].
    fn dispatch_adaptive(
        &self,
        identifier: &str,
        opts: &CssOpts,
        elements: Nodes<'a>,
    ) -> Result<Nodes<'a>> {
        let doc = self.doc();
        let enabled = doc.adaptive_enabled();

        if !elements.is_empty() {
            if !enabled && opts.auto_save {
                tracing::warn!("{}", AUTO_SAVE_IGNORED);
            } else if enabled && opts.auto_save {
                // Only the FIRST element is saved — Python's documented
                // "Known issue", reproduced deliberately.
                doc.save(&elements[0], identifier)?;
            }
            return Ok(elements);
        }

        if enabled {
            if opts.adaptive {
                if let Some(fingerprint) = doc.retrieve(identifier)? {
                    let relocated = self.relocate(&fingerprint, opts.percentage);
                    // `relocate` returns an empty list, never `None`; the guard
                    // is the `test_relocation_auto_save_no_match_above_threshold`
                    // regression fix.
                    if !relocated.is_empty() && opts.auto_save {
                        doc.save(&relocated[0], identifier)?;
                    }
                    return Ok(relocated);
                }
            }
            return Ok(Nodes::new());
        }

        if opts.adaptive {
            tracing::warn!("{}", ADAPTIVE_IGNORED);
        } else if opts.auto_save {
            tracing::warn!("{}", AUTO_SAVE_IGNORED);
        }
        Ok(Nodes::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocumentConfig;

    fn doc(html: &str) -> Document {
        Document::parse(html)
    }

    #[test]
    fn the_text_node_degradation_table_holds() {
        let doc = doc("<div id='d'><p class='c'>hi</p><span>x</span></div>");
        let text = doc
            .css("p::text")
            .expect("selects")
            .first()
            .cloned()
            .expect("one text node");

        assert!(text.is_text_node());
        assert_eq!(text.tag(), "#text");
        assert_eq!(text.text(), "hi");
        assert!(text.attrib().is_empty());
        assert!(matches!(
            text.get_attr("class"),
            Err(ParseError::TextNodeHasNoAttributes)
        ));
        assert!(!text.contains_attr("class"));
        assert!(!text.has_class("c"));
        assert_eq!(text.html_content(), "hi");
        assert_eq!(text.prettify(), "hi");
        assert_eq!(text.get(), "hi");
        assert_eq!(text.to_string(), "hi");
        assert!(text.body().is_empty());
        assert_eq!(text.get_all_text(), "hi");
        assert!(text.below_elements().is_empty());
        assert!(text.children().is_empty());
        assert!(text.css("*").expect("guarded").is_empty());
        assert!(text.xpath("//*").expect("guarded").is_empty());
        assert!(text.find_similar_default().is_empty());
        assert!(text.find_by_text("hi", &ByTextOpts::default()).is_empty());
        assert_eq!(text.ancestors().count(), 0);
        assert!(text.next().is_none());
        assert!(text.previous().is_none());
        assert_eq!(text.parent().expect("unguarded").tag(), "p");
        assert_eq!(text.css_path(), "");
        assert_eq!(text.xpath_path(), "");
        assert_eq!(format!("{text:?}"), "<text='hi'>");
    }

    #[test]
    fn an_attribute_result_is_a_text_node_too() {
        let doc = doc("<a href='/x' data-k='v'>y</a>");
        let attr = doc
            .css("a::attr(href)")
            .expect("selects")
            .first()
            .cloned()
            .expect("one attribute node");
        assert!(attr.is_text_node());
        assert_eq!(attr.text(), "/x");
        assert_eq!(
            attr.text_kind(),
            Some(&TextKind::Attribute("href".to_owned()))
        );
        assert_eq!(attr.parent().expect("owner").tag(), "a");
    }

    #[test]
    fn a_tail_string_belongs_to_the_element_it_trails() {
        let doc = doc("<div><p>a</p>tail</div>");
        let texts = doc.css("div::text").expect("selects");
        // `div::text` selects div's own text nodes: only the tail of <p> here.
        assert_eq!(texts.texts(), ["tail"]);
        let tail = texts.first().cloned().expect("a tail");
        assert_eq!(tail.text_kind(), Some(&TextKind::Tail));
        assert_eq!(
            tail.parent().expect("owner").tag(),
            "p",
            "lxml's getparent() for a tail is the element it trails"
        );
    }

    #[test]
    fn navigation_matches_the_python_semantics() {
        let doc = Document::parse_with(
            "<div><!--c--><p id='a'>1</p><span>2</span><p id='b'>3</p></div>",
            &DocumentConfig::new().keep_comments(true),
        );
        let div = doc
            .css("div")
            .expect("selects")
            .first()
            .cloned()
            .expect("a div");
        assert_eq!(
            div.children().iter().map(Node::tag).collect::<Vec<_>>(),
            ["p", "span", "p"],
            "comments are filtered, text is never a child"
        );
        let first = doc
            .css("#a")
            .expect("selects")
            .first()
            .cloned()
            .expect("#a");
        assert_eq!(first.next().expect("next").tag(), "span");
        assert_eq!(
            first
                .next()
                .and_then(|n| n.previous())
                .expect("previous")
                .tag(),
            "p"
        );
        assert_eq!(first.siblings().len(), 2);
        assert_eq!(first.parent().expect("parent").tag(), "div");
        assert_eq!(
            first.ancestors().map(|a| a.tag()).collect::<Vec<_>>(),
            ["div", "body", "html"]
        );
        assert_eq!(first.path().len(), 3);
        assert_eq!(
            first
                .find_ancestor(|a| a.tag() == "body")
                .expect("body")
                .tag(),
            "body"
        );
        assert!(first.find_ancestor(|a| a.tag() == "table").is_none());
    }

    #[test]
    fn html_ancestors_stop_at_the_root_element() {
        let doc = doc("<p>x</p>");
        let html = doc.root();
        assert_eq!(html.tag(), "html");
        assert!(
            html.parent().is_none(),
            "lxml's getparent() is None at <html>"
        );
        assert_eq!(html.ancestors().count(), 0);
    }

    #[test]
    fn xpath_variables_are_pythons_kwargs() {
        let doc = doc("<table><tr><td>Cell 1</td><td>Cell 2</td></tr></table>");
        let mut vars = Variables::new();
        vars.insert("cell_text", "Cell 1");
        let hits = doc
            .xpath_with("//td[text()=$cell_text]", &CssOpts::default(), &vars)
            .expect("evaluates");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text(), "Cell 1");
    }

    #[test]
    fn a_scalar_xpath_is_rejected_by_xpath_and_kept_by_xpath_value() {
        let doc = doc("<p>a</p><p>b</p>");
        let error = doc.xpath("count(//p)").expect_err("a number");
        assert!(matches!(error, ParseError::NotANodeSet { .. }));
        let value = doc
            .xpath_value("count(//p)", &Variables::new())
            .expect("evaluates");
        assert_eq!(value, Value::Number(2.0));
    }

    #[test]
    fn invalid_selectors_carry_pythons_sentences() {
        let doc = doc("<p>x</p>");
        let css = doc.css("4 ayo").expect_err("invalid");
        assert!(css
            .to_string()
            .starts_with("Invalid CSS selector '4 ayo': "));
        let xpath = doc.xpath("4 ayo").expect_err("invalid");
        assert_eq!(xpath.to_string(), "Invalid XPath selector: 4 ayo");
    }

    #[test]
    fn json_falls_through_body_then_text_then_all_text() {
        let body = doc(r#"{"a": 1}"#);
        assert_eq!(body.json().expect("body is JSON")["a"], 1);

        let doc = doc(r#"<script id="d">{"totalProducts": 3}</script>"#);
        let script = doc
            .css("#d")
            .expect("selects")
            .first()
            .cloned()
            .expect("script");
        assert_eq!(script.json().expect("text is JSON")["totalProducts"], 3);

        let text = doc
            .css("#d::text")
            .expect("selects")
            .first()
            .cloned()
            .expect("text node");
        assert_eq!(text.json().expect("string node")["totalProducts"], 3);
    }

    #[test]
    fn repr_matches_pythons_shape() {
        let doc = doc("<div><p>x</p></div>");
        let p = doc.css("p").expect("selects").first().cloned().expect("p");
        assert_eq!(
            format!("{p:?}"),
            "<data='<p>x</p>' parent='<div><p>x</p></div>'>"
        );
        let html = doc.root();
        assert!(format!("{html:?}").starts_with("<data='"));
        assert!(
            !format!("{html:?}").contains("parent="),
            "the root element has no parent"
        );
    }

    #[test]
    fn attribute_access_mirrors_getitem_and_contains() {
        let doc = doc(r#"<div data-id="1" class="a b">x</div>"#);
        let div = doc
            .css("div")
            .expect("selects")
            .first()
            .cloned()
            .expect("div");
        assert_eq!(div.get_attr("data-id").expect("present"), "1");
        assert!(matches!(
            div.get_attr("nope"),
            Err(ParseError::MissingAttribute(_))
        ));
        assert!(div.contains_attr("data-id"));
        assert!(!div.contains_attr("nope"));
        assert!(div.has_class("a") && div.has_class("b") && !div.has_class("c"));
        assert_eq!(div.attrib().len(), 2);
        assert_eq!(div.attr("class"), Some("a b"));
    }

    /// `siblings` is written in terms of the unguarded `parent`, so a string
    /// node really does report the owner's children. Pinned against
    /// `the reference implementation`, which answers `['b', 'i']` here.
    #[test]
    fn a_text_nodes_siblings_are_its_owners_children() {
        let page = doc("<div><p>hi<b>B</b><i>I</i></p></div>");
        let text = page.css("p::text").expect("selects")[0].clone();
        assert_eq!(text.text(), "hi");
        assert_eq!(
            text.siblings().iter().map(Node::tag).collect::<Vec<_>>(),
            ["b", "i"]
        );
        // …and it is empty when the owner has no element children, which is why
        // this looks guarded but is not.
        let childless = doc("<div><p>hi</p></div>");
        assert!(childless.css("p::text").expect("selects")[0]
            .siblings()
            .is_empty());
    }

    /// `descendant-or-self::`, not `querySelectorAll`: the query node is itself
    /// a candidate for a combinator-free selector.
    #[test]
    fn the_receiver_is_a_candidate_for_its_own_selector() {
        let doc = doc("<div><p>hi</p></div>");
        assert_eq!(doc.css("html").expect("selects").len(), 1);
        let div = doc.css("div").expect("selects")[0].clone();
        assert_eq!(div.css("div").expect("selects").len(), 1);
        assert!(div.css("body div").expect("selects").is_empty());
    }

    #[test]
    fn repr_truncates_at_forty_characters() {
        let long = "abcdefghij".repeat(6);
        let doc = doc(&format!("<p>{long}</p>"));
        let text = doc.css("p::text").expect("selects")[0].clone();
        assert_eq!(
            format!("{text:?}"),
            "<text='abcdefghijabcdefghijabcdefghijabcdefghij...'>"
        );
        let element = doc.css("p").expect("selects")[0].clone();
        assert!(
            format!("{element:?}")
                .starts_with("<data='<p>abcdefghijabcdefghijabcdefghijabcdefg...' parent='<body>"),
            "{element:?}"
        );
    }

    #[test]
    fn nodes_from_different_engines_compare_equal() {
        let doc = doc("<div><p class='x'>a</p></div>");
        let by_css = doc
            .css("p.x")
            .expect("selects")
            .first()
            .cloned()
            .expect("p");
        let by_xpath = doc
            .xpath("//p")
            .expect("selects")
            .first()
            .cloned()
            .expect("p");
        assert_eq!(by_css, by_xpath);
        let text_a = doc.css("p::text").expect("selects")[0].clone();
        let text_b = doc.xpath("//p/text()").expect("selects")[0].clone();
        assert_eq!(text_a, text_b);
        assert_ne!(by_css, text_a);
    }
}
