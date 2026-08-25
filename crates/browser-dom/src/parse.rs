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
//! | `remove_blank_text=True` | [`remove_blank_text`] reimplements libxml2's `areBlanks()` — a blank run is ignorable only where the content model forbids character data |
//! | NUL stripping / `"" -> "<html/>"` | [`prepare_str`] / [`prepare_bytes_with`], before tokenising |
//! | `encoding=…` | [`prepare_bytes_with`] decodes the body with that label before html5ever ever sees it — an explicit label overrides `<meta charset>`, as it does in libxml2 |
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
use markup5ever::LocalName;
use url::Url;

use crate::node::{cdata_payload, Attributes, Element, NodeData};
use crate::qualname::{allows_pcdata, has_cdata_content};
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
/// The bytes are NUL-stripped and then decoded with [`ParseOptions::encoding`],
/// which is what `HTMLParser(encoding=…)` does in Python. Python does *not*
/// trim byte bodies, and neither do we.
#[must_use]
pub fn parse_bytes_with(bytes: &[u8], options: &ParseOptions) -> Document {
    let prepared = prepare_bytes_with(bytes, &options.encoding);
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
///
/// Equivalent to [`prepare_bytes_with`] under the default `"utf-8"` label.
#[must_use]
pub fn prepare_bytes(bytes: &[u8]) -> Cow<'_, str> {
    prepare_bytes_with(bytes, "utf-8")
}

/// `content.replace(b"\x00", b"")` followed by a decode with `encoding`.
///
/// This is the byte half of `Selector.__init__`. Python strips NULs from the
/// raw bytes and then hands them to `HTMLParser(encoding=encoding)`, so the
/// label the caller passed — in practice the `charset=` out of a `Content-Type`
/// header — decides how the body is read. An explicit label **overrides** any
/// `<meta charset>` in the document: libxml2 does not sniff when it is told.
///
/// ```
/// use browser_dom::parse::prepare_bytes_with;
///
/// assert_eq!(prepare_bytes_with(b"<p>caf\xc3\xa9</p>", "utf-8"), "<p>café</p>");
/// assert_eq!(prepare_bytes_with(b"<p>caf\xe9</p>", "windows-1252"), "<p>café</p>");
/// // NULs go first, and an empty body becomes `<html/>`.
/// assert_eq!(prepare_bytes_with(b"<p>a\0b</p>", "utf-8"), "<p>ab</p>");
/// assert_eq!(prepare_bytes_with(b"", "utf-8"), "<html/>");
/// ```
///
/// # Deviations from libxml2
///
/// The label is resolved through the WHATWG Encoding Standard
/// ([`encoding_rs`]); libxml2 resolves it through `iconv`. Two consequences,
/// both recorded in `docs/DIVERGENCES.md`:
///
/// * `iso-8859-1`, `latin1` and `ascii` are WHATWG aliases of **windows-1252**,
///   so bytes `0x80..=0x9F` decode to typographic characters here and to C1
///   controls (or a hard parse failure, for `ascii`) under libxml2.
/// * An unresolvable label, or a byte sequence `iconv` rejects outright, makes
///   Python return an *empty document*. We never destroy a document over an
///   encoding problem: an unknown label logs a warning and falls back to UTF-8,
///   and every decoder is used in replacement mode.
#[must_use]
pub fn prepare_bytes_with<'a>(bytes: &'a [u8], encoding: &str) -> Cow<'a, str> {
    let cleaned: Cow<'a, [u8]> = if bytes.contains(&0) {
        Cow::Owned(bytes.iter().copied().filter(|b| *b != 0).collect())
    } else {
        Cow::Borrowed(bytes)
    };

    let label = if encoding.trim().is_empty() {
        "utf-8"
    } else {
        encoding.trim()
    };
    let codec = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or_else(|| {
        tracing::warn!(
            encoding = label,
            "unknown encoding label; decoding the body as UTF-8"
        );
        encoding_rs::UTF_8
    });

    let text: Cow<'a, str> = match cleaned {
        Cow::Borrowed(b) => decode_without_bom(codec, b),
        Cow::Owned(b) => Cow::Owned(decode_without_bom(codec, &b).into_owned()),
    };

    if text.is_empty() {
        Cow::Borrowed("<html/>")
    } else {
        text
    }
}

/// Decode with `codec` in replacement mode, ignoring any BOM *except* a UTF-8
/// one under a UTF-8 label.
///
/// WHATWG's `decode()` would let a UTF-8 BOM override the caller's label;
/// libxml2 does not sniff at all once an encoding is given — decoding
/// `\xEF\xBB\xBF` as latin-1 really does yield `ï»¿`. The one thing libxml2
/// *does* swallow is a UTF-8 BOM on a UTF-8 body, so that is the only case
/// handled here.
fn decode_without_bom<'a>(codec: &'static encoding_rs::Encoding, bytes: &'a [u8]) -> Cow<'a, str> {
    let bytes = if codec == encoding_rs::UTF_8 {
        bytes
            .strip_prefix(b"\xef\xbb\xbf".as_slice())
            .unwrap_or(bytes)
    } else {
        bytes
    };
    let (text, _had_errors) = codec.decode_without_bom_handling(bytes);
    text
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

/// `true` when every character is HTML whitespace — space, tab, LF, CR or FF.
///
/// This is libxml2's `IS_WS_HTML`, and it is deliberately **not**
/// [`char::is_whitespace`]: `U+000B` (vertical tab), `U+00A0` (no-break space)
/// and every other Unicode space separator are *not* blank, so
/// `<div>&nbsp;</div>` keeps its text. The empty string counts as blank, which
/// is how libxml2's `for` loop over the run answers.
#[must_use]
pub fn is_html_blank(text: &str) -> bool {
    text.bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C))
}

/// What the last surviving non-comment sibling before a blank run was —
/// libxml2's `lastChild` after its comment-skipping loop.
enum Preceding<'a> {
    /// Nothing before it (or only comments).
    Nothing,
    /// A text node: `xmlNodeIsText(lastChild)` is true.
    Text,
    /// Anything else, carrying the name `areBlanks()` looks up: an element's
    /// local name, a doctype's root name, a processing instruction's target.
    Named(&'a str),
}

/// Delete ignorable whitespace-only text nodes — libxml2's
/// `remove_blank_text=True`, implementing its `areBlanks()` decision rather
/// than "delete every blank run".
///
/// A run of [`is_html_blank`] characters is **ignorable** (deleted) when:
///
/// * its parent's character data is not CDATA ([`has_cdata_content`] — inside
///   `<script>`, `<title>`, `<textarea>` … blanks always survive), **and**
/// * looking backwards past any comments:
///   * the previous sibling is a text node → **kept** (`<p>a <b>x</b></p>`);
///   * the previous sibling is an element → deleted unless that element
///     [`allows_pcdata`] (`<p><b>b</b> <i>i</i></p>` keeps its space because
///     `b` does; `<ul><li>a</li> <li>b</li></ul>` keeps its newline for the
///     same reason, while `<div><br> <em/></div>` loses it because `br` does
///     not);
///   * there is no previous sibling → deleted unless the *parent*
///     [`allows_pcdata`] (`<div> </div>` keeps its space, `<ul> <li>a</li></ul>`
///     does not).
///
/// The pass walks each parent's children left to right and only consults
/// siblings that have survived, mirroring the fact that libxml2 decides while
/// building and never inserts the runs it rejects.
pub fn remove_blank_text(tree: &mut Tree<NodeData>) {
    let mut doomed: Vec<NodeId> = Vec::new();
    for parent in all_ids(tree) {
        collect_ignorable_children(tree, parent, &mut doomed);
    }
    for id in doomed {
        if let Some(mut node) = tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Append the ignorable blank children of `parent` to `doomed`.
fn collect_ignorable_children(tree: &Tree<NodeData>, parent: NodeId, doomed: &mut Vec<NodeId>) {
    let Some(node) = tree.get(parent) else {
        return;
    };
    let cdata_parent = node
        .value()
        .as_element()
        .is_some_and(|el| has_cdata_content(el.local_name()));
    if cdata_parent {
        return;
    }
    let parent_allows_pcdata = node
        .value()
        .as_element()
        .is_some_and(|el| allows_pcdata(el.local_name()));

    let mut preceding = Preceding::Nothing;
    for child in node.children() {
        match child.value() {
            // Comments are transparent: libxml2 walks back past them.
            NodeData::Comment(_) => {}
            NodeData::Text(text) => {
                let ignorable = if !is_html_blank(text) {
                    false
                } else {
                    match preceding {
                        Preceding::Text => false,
                        Preceding::Named(name) => !allows_pcdata(&LocalName::from(name)),
                        Preceding::Nothing => !parent_allows_pcdata,
                    }
                };
                if ignorable {
                    doomed.push(child.id());
                } else {
                    preceding = Preceding::Text;
                }
            }
            NodeData::Element(el) => preceding = Preceding::Named(el.local_name().as_ref()),
            NodeData::Doctype { name, .. } => preceding = Preceding::Named(name.as_str()),
            NodeData::ProcessingInstruction { target, .. } => {
                preceding = Preceding::Named(target.as_str());
            }
            NodeData::Document => preceding = Preceding::Named(""),
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
    use crate::node::NodeRef;

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

    /// Every answer here was read off `lxml.html.HTMLParser(encoding=…)` via the
    /// reference interpreter — `Selector(content=…, encoding=…).css('p::text')`.
    /// This is the seam `browser-http`'s `Response` (unit 12) and the CDP
    /// driver (unit 14) hang the `charset=` of a `Content-Type` header on.
    #[test]
    fn the_encoding_label_decides_how_a_byte_body_is_read() {
        fn text(bytes: &[u8], encoding: &str) -> String {
            let options = ParseOptions::default().encoding(encoding);
            Document::parse_bytes(bytes, &options)
                .body()
                .map(|b| b.text_content())
                .unwrap_or_default()
        }

        // Python: 'café' / 'caf�'.
        let latin1 = b"<html><body><p>caf\xe9</p></body></html>";
        assert_eq!(text(latin1, "windows-1252"), "café");
        assert_eq!(text(latin1, "utf-8"), "caf\u{fffd}");

        // Python: 'café' / 'cafÃ©' — a UTF-8 body read as a single-byte charset.
        let utf8 = "<html><body><p>café</p></body></html>".as_bytes();
        assert_eq!(text(utf8, "utf-8"), "café");
        assert_eq!(text(utf8, "windows-1252"), "cafÃ©");

        // Python: '殿秘'. A multi-byte codec, to prove the label is not just a
        // single-byte table lookup.
        assert_eq!(
            text(
                b"<html><body><p>\x93a\x94\xe9</p></body></html>",
                "shift_jis"
            ),
            "殿秘"
        );

        // A UTF-8 BOM is swallowed under a UTF-8 label and is ordinary text
        // under any other, exactly as libxml2 does it (it never re-sniffs once
        // an encoding has been given).
        let bom = b"\xef\xbb\xbf<html><body><p>caf\xc3\xa9</p></body></html>";
        assert_eq!(text(bom, "utf-8"), "café");
        assert_eq!(text(bom, "windows-1252"), "ï»¿cafÃ©");

        // Declared divergence: an unknown label raises `LookupError` in Python.
        // We warn and fall back to UTF-8 rather than lose the document.
        assert_eq!(text(utf8, "not-a-charset"), "café");
        assert_eq!(text(utf8, ""), "café");
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

    /// The whole `areBlanks()` decision table, every row checked against
    /// libxml2 2.14.6 through `lxml` before being written down.
    #[test]
    fn blank_text_survives_where_libxml2_keeps_it() {
        // Helper: the first element of `tag` in the parsed document.
        fn first<'a>(doc: &'a Document, tag: &str) -> NodeRef<'a> {
            doc.elements()
                .find(|e| e.local_name() == Some(tag))
                .expect("element is in the tree")
        }

        // -- no previous sibling: the *parent* decides ---------------------
        let doc = Document::parse("<div> </div>");
        assert_eq!(first(&doc, "div").text(), Some(" "), "div allows PCDATA");
        let doc = Document::parse("<ul> <li>a</li></ul>");
        assert_eq!(first(&doc, "ul").text(), None, "ul forbids PCDATA");
        let doc = Document::parse("<div><ul> </ul></div>");
        assert_eq!(first(&doc, "ul").text(), None);

        // -- previous sibling is an element: *it* decides -------------------
        let doc = Document::parse("<p><b>b</b> <i>i</i> <strong>s</strong></p>");
        let tails: Vec<Option<&str>> = ["b", "i"].iter().map(|t| first(&doc, t).tail()).collect();
        assert_eq!(tails, [Some(" "), Some(" ")], "b and i allow PCDATA");
        let doc = Document::parse("<div><br> <em>y</em></div>");
        assert_eq!(first(&doc, "br").tail(), None, "br does not allow PCDATA");
        let doc = Document::parse("<ul><li>a</li>\n<li>b</li></ul>");
        assert_eq!(first(&doc, "li").tail(), Some("\n"), "li allows PCDATA");

        // -- previous sibling is text: always kept -------------------------
        let doc = Document::parse("<ul>x<em>y</em> <em>z</em></ul>");
        assert_eq!(first(&doc, "em").tail(), Some(" "));

        // -- comments are transparent to the backwards walk ----------------
        let keep = ParseOptions::new().keep_comments(true);
        let doc = Document::parse_with("<ul><!--c--> <li>a</li></ul>", &keep);
        assert_eq!(first(&doc, "ul").children().count(), 2, "blank dropped");
        let doc = Document::parse_with("<div><!--c--> <p>a</p></div>", &keep);
        assert_eq!(first(&doc, "div").children().count(), 3, "blank kept");

        // -- U+00A0 and U+000B are never blank ------------------------------
        let doc = Document::parse("<ul>\u{a0}</ul>");
        assert_eq!(first(&doc, "ul").text(), Some("\u{a0}"));
        let doc = Document::parse("<ul>\u{b}</ul>");
        assert_eq!(first(&doc, "ul").text(), Some("\u{b}"));
        let doc = Document::parse("<ul> \u{a0} </ul>");
        assert_eq!(first(&doc, "ul").text(), Some(" \u{a0} "));

        // -- CDATA-content parents never consult the table ------------------
        // (html5ever eats the one LF right after `<pre>`; see divergence T7.)
        let doc = Document::parse("<pre>\n  \n</pre>");
        assert_eq!(first(&doc, "pre").text(), Some("  \n"), "pre allows PCDATA");
        let doc = Document::parse("<textarea> </textarea>");
        assert_eq!(first(&doc, "textarea").text(), Some(" "));
        let doc = Document::parse("<div><script> </script></div>");
        assert_eq!(first(&doc, "script").text(), Some(" "));

        // -- and `pre` is *not* exempt once a forbidding sibling precedes ----
        let doc = Document::parse("<pre><ul></ul> <em>y</em></pre>");
        assert_eq!(first(&doc, "ul").tail(), None);
    }

    #[test]
    fn pretty_printed_structure_loses_exactly_what_lxml_loses() {
        // `<ul>\n  <li>a</li>\n</ul>` keeps only the tail of the last `<li>`.
        let doc = Document::parse("<ul>\n  <li>a</li>\n</ul>");
        let ul = doc
            .elements()
            .find(|e| e.local_name() == Some("ul"))
            .unwrap();
        assert_eq!(ul.text(), None);
        assert_eq!(ul.children().count(), 2);
        assert_eq!(ul.last_child().and_then(|n| n.as_text()), Some("\n"));

        // `<table>` drops both, because `table` and `tr` both forbid PCDATA.
        let doc = Document::parse("<table>\n  <tr><td>a</td></tr>\n</table>");
        let table = doc
            .elements()
            .find(|e| e.local_name() == Some("table"))
            .unwrap();
        assert!(table
            .descendant_text_nodes()
            .all(|t| t.as_text() == Some("a")));
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
