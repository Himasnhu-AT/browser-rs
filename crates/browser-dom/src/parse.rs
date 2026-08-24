//! Driving `html5ever` into an `ego_tree` arena, [`ParseOptions`], and the
//! post-parse passes that reproduce libxml2's parser flags.
//!
//! # The libxml2 compatibility passes
//!
//! Python calls
//! `HTMLParser(recover=True, remove_blank_text=True, remove_comments=not keep_comments,
//! strip_cdata=not keep_cdata, huge_tree=…, default_doctype=True)`.
//! html5ever has none of those knobs — it is a *spec* parser — so each flag
//! becomes a pass over the finished tree:
//!
//! | libxml2 flag | our equivalent |
//! |---|---|
//! | `recover=True` | free: html5ever never rejects input |
//! | `default_doctype=True` | free: html5ever always builds `html`/`head`/`body` |
//! | `strip_cdata=True` | [`fold_cdata_sections`] rewrites html5ever's bogus `[CDATA[…]]` comment into a text node |
//! | `remove_comments=True` | [`remove_comments`] deletes comment nodes, then adjacent text is re-merged (libxml2 merges text the same way) |
//! | `remove_blank_text=True` | [`remove_blank_text`] deletes whitespace-only text nodes outside whitespace-preserving elements |
//! | NUL stripping / `"" -> "<html/>"` | [`prepare_str`] / [`prepare_bytes`], before tokenising |
//!
//! The passes run in that order: CDATA first (so a preserved section is not
//! mistaken for a comment), then comment removal, then text merging, then blank
//! text removal (merging can *create* a whitespace-only node).

use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};

use ego_tree::{NodeId, Tree};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, QualName};
use url::Url;

use crate::node::{cdata_payload, Attributes, Element, NodeData};
use crate::qualname::preserves_whitespace;
use crate::tree::Document;

/// Parser configuration — the Rust face of the reference implementation's `selector_config`.
///
/// The defaults are Python's: comments dropped, CDATA folded into text, blank
/// text removed, no base URL, UTF-8.
#[derive(Clone, Debug)]
pub struct ParseOptions {
    /// Keep comment nodes. `remove_comments = !keep_comments`. Default `false`.
    pub keep_comments: bool,
    /// Keep CDATA sections as distinct nodes instead of folding them into text.
    /// `strip_cdata = !keep_cdata`. Default `false`.
    pub keep_cdata: bool,
    /// Drop whitespace-only text nodes. Default `true` (libxml2's
    /// `remove_blank_text`).
    pub remove_blank_text: bool,
    /// Base URL for `urljoin`.
    pub base_url: Option<Url>,
    /// Encoding label recorded on the document.
    pub encoding: String,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            keep_comments: false,
            keep_cdata: false,
            remove_blank_text: true,
            base_url: None,
            encoding: "utf-8".to_owned(),
        }
    }
}

impl ParseOptions {
    /// The Python defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep comment nodes in the tree.
    #[must_use]
    pub fn keep_comments(mut self, keep: bool) -> Self {
        self.keep_comments = keep;
        self
    }

    /// Keep CDATA sections as separate nodes.
    #[must_use]
    pub fn keep_cdata(mut self, keep: bool) -> Self {
        self.keep_cdata = keep;
        self
    }

    /// Toggle the `remove_blank_text` pass.
    #[must_use]
    pub fn remove_blank_text(mut self, remove: bool) -> Self {
        self.remove_blank_text = remove;
        self
    }

    /// Set the base URL from an already-parsed [`Url`].
    #[must_use]
    pub fn base_url(mut self, url: Url) -> Self {
        self.base_url = Some(url);
        self
    }

    /// Set the base URL from a string.
    ///
    /// An empty or unparsable string leaves the base URL unset — Python treats
    /// `url=""` as "no base URL" and never raises here.
    #[must_use]
    pub fn base_url_str(mut self, url: &str) -> Self {
        self.base_url = if url.is_empty() {
            None
        } else {
            Url::parse(url).ok()
        };
        self
    }

    /// Record the encoding label the body was decoded with.
    #[must_use]
    pub fn encoding(mut self, encoding: impl Into<String>) -> Self {
        self.encoding = encoding.into();
        self
    }
}

/// Parse HTML with the default options.
#[must_use]
pub fn parse_html(html: &str) -> Document {
    parse_html_with(html, &ParseOptions::default())
}

/// Parse HTML with explicit options.
#[must_use]
pub fn parse_html_with(html: &str, options: &ParseOptions) -> Document {
    let prepared = prepare_str(html);
    let tree = tokenize(&prepared);
    finish(tree, options)
}

/// Parse a byte body with explicit options.
///
/// The bytes are NUL-stripped and decoded as UTF-8 with replacement — the same
/// lossy decode html5ever's own `from_utf8()` sink performs. Python does *not*
/// trim byte bodies, and neither do we.
#[must_use]
pub fn parse_bytes_with(bytes: &[u8], options: &ParseOptions) -> Document {
    let prepared = prepare_bytes(bytes);
    let tree = tokenize(&prepared);
    finish(tree, options)
}

fn tokenize(html: &str) -> Tree<NodeData> {
    // `scripting_enabled: false` is the one non-default: with scripting on,
    // html5ever tokenises `<noscript>` as RAWTEXT and its markup becomes a
    // single text node. libxml2 (and Python's `html.parser`) always parse
    // `<noscript>` children as elements, and matching lxml is the contract, so
    // the tree builder has to be told the page is not scripted.
    let options = html5ever::ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    html5ever::parse_document(DomSink::new(), options).one(html)
}

fn finish(mut tree: Tree<NodeData>, options: &ParseOptions) -> Document {
    fold_cdata_sections(&mut tree, options.keep_cdata);
    if !options.keep_comments {
        remove_comments(&mut tree, options.keep_cdata);
    }
    merge_adjacent_text(&mut tree);
    if options.remove_blank_text {
        remove_blank_text(&mut tree);
    }
    Document::from_tree(tree, options.base_url.clone(), options.encoding.clone())
}

/// `content.strip().replace("\x00", "") or "<html/>"` — Python's `str` branch.
#[must_use]
pub fn prepare_str(html: &str) -> Cow<'_, str> {
    let trimmed = html.trim();
    let cleaned: Cow<'_, str> = if trimmed.contains('\0') {
        Cow::Owned(trimmed.replace('\0', ""))
    } else {
        Cow::Borrowed(trimmed)
    };
    if cleaned.is_empty() {
        Cow::Borrowed("<html/>")
    } else {
        cleaned
    }
}

/// `content.replace(b"\x00", b"")` — Python's `bytes` branch (no trimming),
/// followed by a lossy UTF-8 decode.
#[must_use]
pub fn prepare_bytes(bytes: &[u8]) -> Cow<'_, str> {
    let cleaned: Cow<'_, [u8]> = if bytes.contains(&0) {
        Cow::Owned(bytes.iter().copied().filter(|b| *b != 0).collect())
    } else {
        Cow::Borrowed(bytes)
    };
    let text = match cleaned {
        Cow::Borrowed(b) => String::from_utf8_lossy(b),
        Cow::Owned(b) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
    };
    if text.is_empty() {
        Cow::Borrowed("<html/>")
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// post-parse passes
// ---------------------------------------------------------------------------

fn all_ids(tree: &Tree<NodeData>) -> Vec<NodeId> {
    tree.root().descendants().map(|n| n.id()).collect()
}

/// Rewrite html5ever's bogus `[CDATA[…]]` comments.
///
/// With `keep_cdata == false` (libxml2's `strip_cdata=True`, the default) the
/// section becomes a plain text node holding its payload. With `keep_cdata ==
/// true` the node is left alone; it stays a comment carrying the `[CDATA[…]]`
/// marker, is exempt from [`remove_comments`], and is serialised back as
/// `<![CDATA[…]]>`.
pub fn fold_cdata_sections(tree: &mut Tree<NodeData>, keep_cdata: bool) {
    if keep_cdata {
        return;
    }
    for id in all_ids(tree) {
        let payload = match tree.get(id).map(|n| n.value()) {
            Some(NodeData::Comment(text)) => cdata_payload(text).map(str::to_owned),
            _ => None,
        };
        if let (Some(payload), Some(mut node)) = (payload, tree.get_mut(id)) {
            *node.value() = NodeData::Text(payload);
        }
    }
}

/// Delete comment nodes (libxml2's `remove_comments=True`).
///
/// Preserved CDATA sections are comments at the arena level but are *not*
/// comments to the user, so they survive when `keep_cdata` is set.
pub fn remove_comments(tree: &mut Tree<NodeData>, keep_cdata: bool) {
    for id in all_ids(tree) {
        let drop = match tree.get(id).map(|n| n.value()) {
            Some(NodeData::Comment(text)) => {
                !(keep_cdata && cdata_payload(text.as_str()).is_some())
            }
            _ => false,
        };
        if drop {
            if let Some(mut node) = tree.get_mut(id) {
                node.detach();
            }
        }
    }
}

/// Merge runs of adjacent text siblings into one node.
///
/// html5ever already merges while parsing; this pass matters *after* comment
/// removal, where `text <!--c--> text` collapses into a single text node — the
/// same thing libxml2's SAX handler does when it never creates the comment.
pub fn merge_adjacent_text(tree: &mut Tree<NodeData>) {
    for parent in all_ids(tree) {
        merge_text_children(tree, parent);
    }
}

/// Merge runs of adjacent text children of a single node.
pub(crate) fn merge_text_children(tree: &mut Tree<NodeData>, parent: NodeId) {
    let mut merges: Vec<(NodeId, NodeId)> = Vec::new();
    if let Some(node) = tree.get(parent) {
        let mut run_start: Option<NodeId> = None;
        for child in node.children() {
            if child.value().is_text() {
                match run_start {
                    Some(first) => merges.push((first, child.id())),
                    None => run_start = Some(child.id()),
                }
            } else {
                run_start = None;
            }
        }
    }
    for (into, from) in merges {
        let Some(text) = tree
            .get(from)
            .and_then(|n| n.value().as_text())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Some(mut node) = tree.get_mut(into) {
            if let NodeData::Text(existing) = node.value() {
                existing.push_str(&text);
            }
        }
        if let Some(mut node) = tree.get_mut(from) {
            node.detach();
        }
    }
}

/// Delete whitespace-only text nodes (libxml2's `remove_blank_text=True`).
///
/// Whitespace inside `pre`, `textarea`, `script`, `style`, `title`, `xmp` and
/// `listing` is significant and is kept — the same exemption libxml2 applies.
pub fn remove_blank_text(tree: &mut Tree<NodeData>) {
    for id in all_ids(tree) {
        let blank = match tree.get(id) {
            Some(node) => match node.value() {
                NodeData::Text(text) => {
                    text.chars().all(char::is_whitespace)
                        && !node
                            .parent()
                            .and_then(|p| {
                                p.value()
                                    .as_element()
                                    .map(|el| preserves_whitespace(el.local_name()))
                            })
                            .unwrap_or(false)
                }
                _ => false,
            },
            None => false,
        };
        if blank {
            if let Some(mut node) = tree.get_mut(id) {
                node.detach();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the html5ever tree sink
// ---------------------------------------------------------------------------

/// The `TreeSink` that builds our arena.
///
/// html5ever 0.39 takes `&self` in every sink method, so the arena lives behind
/// a `RefCell` for the duration of the parse only; the finished [`Tree`] is
/// handed out by value and is `Send + Sync`.
struct DomSink {
    tree: RefCell<Tree<NodeData>>,
    quirks: Cell<QuirksMode>,
}

impl DomSink {
    fn new() -> Self {
        Self {
            tree: RefCell::new(Tree::new(NodeData::Document)),
            quirks: Cell::new(QuirksMode::NoQuirks),
        }
    }

    fn orphan(&self, data: NodeData) -> NodeId {
        self.tree.borrow_mut().orphan(data).id()
    }
}

fn to_attributes(attrs: Vec<Attribute>) -> Attributes {
    attrs
        .into_iter()
        .map(|attr| (attr.name, attr.value.to_string()))
        .collect()
}

impl TreeSink for DomSink {
    type Handle = NodeId;
    type Output = Tree<NodeData>;
    type ElemName<'a>
        = Ref<'a, QualName>
    where
        Self: 'a;

    fn finish(self) -> Self::Output {
        self.tree.into_inner()
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        tracing::trace!(target: "browser_dom::parse", error = %msg, "html5ever parse error");
    }

    fn get_document(&self) -> NodeId {
        self.tree.borrow().root().id()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.tree.borrow(), |tree| {
            match tree.get(*target).map(|n| n.value()) {
                Some(NodeData::Element(el)) => el.name(),
                // The tree builder never asks for the name of a non-element;
                // the `TreeSink` contract explicitly allows panicking here.
                _ => panic!("elem_name called on a non-element node"),
            }
        })
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> NodeId {
        self.orphan(NodeData::Element(Element::new(name, to_attributes(attrs))))
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.orphan(NodeData::Comment(text.to_string()))
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.orphan(NodeData::ProcessingInstruction {
            target: target.to_string(),
            data: data.to_string(),
        })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut tree = self.tree.borrow_mut();
        match child {
            NodeOrText::AppendNode(id) => {
                if let Some(mut node) = tree.get_mut(*parent) {
                    node.append_id(id);
                }
            }
            NodeOrText::AppendText(text) => {
                let tail_text = tree
                    .get(*parent)
                    .and_then(|node| node.last_child())
                    .filter(|node| node.value().is_text())
                    .map(|node| node.id());
                match tail_text {
                    Some(id) => {
                        if let Some(mut node) = tree.get_mut(id) {
                            if let NodeData::Text(existing) = node.value() {
                                existing.push_str(&text);
                            }
                        }
                    }
                    None => {
                        let id = tree.orphan(NodeData::Text(text.to_string())).id();
                        if let Some(mut node) = tree.get_mut(*parent) {
                            node.append_id(id);
                        }
                    }
                }
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self
            .tree
            .borrow()
            .get(*element)
            .and_then(|node| node.parent())
            .is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let doctype = NodeData::Doctype {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        };
        let mut tree = self.tree.borrow_mut();
        let id = tree.orphan(doctype).id();
        tree.root_mut().append_id(id);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        // We keep template contents inline, in the template element itself:
        // scrapers want to see them, and nothing in the port distinguishes a
        // template's document fragment from its element.
        *target
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks.set(mode);
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut tree = self.tree.borrow_mut();
        let has_parent = tree.get(*sibling).and_then(|node| node.parent()).is_some();
        if !has_parent {
            return;
        }
        match new_node {
            NodeOrText::AppendNode(id) => {
                if let Some(mut node) = tree.get_mut(*sibling) {
                    node.insert_id_before(id);
                }
            }
            NodeOrText::AppendText(text) => {
                let prev_text = tree
                    .get(*sibling)
                    .and_then(|node| node.prev_sibling())
                    .filter(|node| node.value().is_text())
                    .map(|node| node.id());
                match prev_text {
                    Some(id) => {
                        if let Some(mut node) = tree.get_mut(id) {
                            if let NodeData::Text(existing) = node.value() {
                                existing.push_str(&text);
                            }
                        }
                    }
                    None => {
                        let id = tree.orphan(NodeData::Text(text.to_string())).id();
                        if let Some(mut node) = tree.get_mut(*sibling) {
                            node.insert_id_before(id);
                        }
                    }
                }
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut tree = self.tree.borrow_mut();
        if let Some(mut node) = tree.get_mut(*target) {
            if let NodeData::Element(el) = node.value() {
                for attr in attrs {
                    el.insert_attr_if_missing(attr.name, attr.value.to_string());
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        if let Some(mut node) = self.tree.borrow_mut().get_mut(*target) {
            node.detach();
        }
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut tree = self.tree.borrow_mut();
        if let Some(mut parent) = tree.get_mut(*new_parent) {
            parent.reparent_from_id_append(*node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_blank_content_becomes_an_html_element() {
        assert_eq!(prepare_str(""), "<html/>");
        assert_eq!(prepare_str("   \n\t "), "<html/>");
        assert_eq!(prepare_str("\0\0"), "<html/>");
        let doc = Document::parse("");
        assert_eq!(
            doc.root_element().and_then(|e| e.local_name()),
            Some("html")
        );
    }

    #[test]
    fn nul_bytes_are_stripped_from_both_paths() {
        assert_eq!(prepare_str("  <p>a\0b</p> "), "<p>ab</p>");
        assert_eq!(prepare_bytes(b"<p>a\0b</p>"), "<p>ab</p>");
        // bytes are not trimmed, matching Python
        assert_eq!(prepare_bytes(b"  <p>x</p>"), "  <p>x</p>");
        let doc = Document::parse("<p>a\0b</p>");
        assert_eq!(doc.body().map(|b| b.text_content()), Some("ab".to_owned()));
    }

    #[test]
    fn invalid_utf8_bytes_decode_lossily() {
        let doc = Document::parse_bytes(
            &[b'<', b'p', b'>', 0xff, b'<', b'/', b'p', b'>'],
            &ParseOptions::default(),
        );
        assert!(doc.body().is_some());
    }

    #[test]
    fn comments_are_removed_by_default_and_text_is_merged() {
        let doc = Document::parse("<div>before<!-- c -->after</div>");
        let div = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        assert_eq!(div.children().count(), 1);
        assert_eq!(div.text(), Some("beforeafter"));
    }

    #[test]
    fn comments_are_kept_when_asked() {
        let options = ParseOptions::default().keep_comments(true);
        let doc = Document::parse_with("<div>before<!-- c -->after<p>x</p></div>", &options);
        let div = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        let kinds: Vec<_> = div.children().map(|c| c.tag().to_owned()).collect();
        assert_eq!(kinds, ["#text", "#comment", "#text", "p"]);
        assert_eq!(div.text(), Some("before"));
    }

    #[test]
    fn cdata_is_folded_into_text_by_default() {
        let doc = Document::parse("<div><![CDATA[payload]]></div>");
        let div = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        assert_eq!(div.text(), Some("payload"));
        assert!(!doc.nodes().any(|n| n.is_comment()));
    }

    #[test]
    fn cdata_survives_comment_removal_when_kept() {
        let options = ParseOptions::default().keep_cdata(true);
        let doc = Document::parse_with("<div><!--gone--><![CDATA[stay]]></div>", &options);
        let div = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        let children: Vec<_> = div.children().collect();
        assert_eq!(children.len(), 1);
        assert!(children[0].is_cdata());
        assert_eq!(children[0].as_comment(), Some("stay"));
        assert_eq!(div.outer_html(), "<div><![CDATA[stay]]></div>");
    }

    #[test]
    fn blank_text_nodes_are_removed_outside_pre() {
        let doc = Document::parse("<div>\n   <p>a</p>\n   <p>b</p>\n</div>");
        let div = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        assert_eq!(div.children().count(), 2);
        assert_eq!(div.text(), None);

        let doc = Document::parse("<pre>\n  \n</pre>");
        let pre = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        assert_eq!(pre.text(), Some("  \n"));
    }

    #[test]
    fn blank_text_can_be_kept() {
        let options = ParseOptions::default().remove_blank_text(false);
        let doc = Document::parse_with("<div>\n  <p>a</p>\n</div>", &options);
        let div = doc
            .body()
            .and_then(|b| b.element_children().next())
            .unwrap();
        assert_eq!(div.children().count(), 3);
    }

    #[test]
    fn options_builder_defaults_match_python() {
        let o = ParseOptions::default();
        assert!(!o.keep_comments);
        assert!(!o.keep_cdata);
        assert!(o.remove_blank_text);
        assert!(o.base_url.is_none());
        assert_eq!(o.encoding, "utf-8");
        let o = ParseOptions::new().base_url_str("").encoding("latin-1");
        assert!(o.base_url.is_none());
        assert_eq!(o.encoding, "latin-1");
        let o = ParseOptions::new().base_url_str("https://example.com/a");
        assert_eq!(
            o.base_url.map(|u| u.to_string()),
            Some("https://example.com/a".to_owned())
        );
    }

    #[test]
    fn doctype_is_preserved() {
        let doc = Document::parse("<!DOCTYPE html><html><body><p>x</p></body></html>");
        let doctype = doc.root().children().next().unwrap();
        assert!(doctype.is_doctype());
        assert!(doc.html().starts_with("<!DOCTYPE html>"));
    }
}
