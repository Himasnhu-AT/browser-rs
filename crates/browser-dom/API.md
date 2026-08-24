# `browser-dom` — API cheat sheet

The HTML tree for the port: html5ever parsed into an `ego-tree` arena, normalised to match lxml's
parser flags. **Everything here is synchronous, allocation-light and `Send + Sync`.**

The names you reach for most are re-exported from the crate root:

```rust
use browser_dom::{Document, DocumentOrder, NodeRef, NodeData, Element, Attributes};
use browser_dom::{ParseOptions, parse_html, parse_html_with, DomError};
use browser_dom::{NodeId, LocalName, Namespace, QualName};   // re-exported deps
```

Everything else stays behind its module:

```rust
use browser_dom::qualname::{attr_name, html_name, is_void_element};
use browser_dom::serialize::{clean_spaces, escape_attr, escape_text};
use browser_dom::nav::sort_document_order;
```

Other third-party types you will touch: `ego_tree::Tree`, `url::Url`, `indexmap::IndexMap`.

---

## 30-second orientation

```rust
let doc = Document::parse("<div id=x>hello <b>world</b>tail</div>");   // Python's Selector(content)
let div = doc.elements().find(|e| e.element_id() == Some("x")).unwrap();

div.tag();                 // "div"
div.text();                // Some("hello ")      <- lxml `element.text`
div.first_element_child().unwrap().tail();        // Some("tail")   <- lxml `element.tail`
div.text_content();        // "hello worldtail"   <- XPath string-value
div.outer_html();          // "<div id=\"x\">hello <b>world</b>tail</div>"
div.attr("id");            // Some("x")
div.has_class("card");     // false
for a in div.ancestors() { /* body, html */ }
```

`NodeRef<'a>` is `Copy` — pass it by value freely. It is `Eq + Hash + Ord` (arena order; use
`cmp_document_order` for real document order), `Debug` (Python's `__repr__`) and `Display`
(outer HTML).

---

## `parse` — `ParseOptions` and entry points

```rust
pub struct ParseOptions {
    pub keep_comments: bool,     // default false  -> remove_comments = !keep_comments
    pub keep_cdata: bool,        // default false  -> strip_cdata     = !keep_cdata
    pub remove_blank_text: bool, // default true
    pub base_url: Option<Url>,   // default None
    pub encoding: String,        // default "utf-8"
}
impl Default for ParseOptions;
impl ParseOptions {
    fn new() -> Self;
    fn keep_comments(self, keep: bool) -> Self;      // builder, all -> Self
    fn keep_cdata(self, keep: bool) -> Self;
    fn remove_blank_text(self, remove: bool) -> Self;
    fn base_url(self, url: Url) -> Self;
    fn base_url_str(self, url: &str) -> Self;        // "" or unparsable -> None, never panics
    fn encoding(self, encoding: impl Into<String>) -> Self;
}

pub fn parse_html(html: &str) -> Document;                              // defaults
pub fn parse_html_with(html: &str, options: &ParseOptions) -> Document;
pub fn parse_bytes_with(bytes: &[u8], options: &ParseOptions) -> Document;

// input normalisation, exposed for tests/callers that pre-process bodies
pub fn prepare_str(html: &str) -> Cow<'_, str>;    // trim, strip NUL, "" -> "<html/>"
pub fn prepare_bytes(bytes: &[u8]) -> Cow<'_, str>;// strip NUL, lossy UTF-8, NO trim

// the post-parse passes, runnable individually on a raw Tree
pub fn fold_cdata_sections(tree: &mut Tree<NodeData>, keep_cdata: bool);
pub fn remove_comments(tree: &mut Tree<NodeData>, keep_cdata: bool);
pub fn merge_adjacent_text(tree: &mut Tree<NodeData>);
pub fn remove_blank_text(tree: &mut Tree<NodeData>);
```

Parsing is **infallible** (html5ever = libxml2 `recover=True`) — no `Result`.

## `tree::Document`

```rust
pub struct Document { pub base_url: Option<Url>, pub encoding: String, /* private tree */ }

impl Document {
    fn parse(html: &str) -> Self;
    fn parse_with(html: &str, options: &ParseOptions) -> Self;
    fn parse_bytes(bytes: &[u8], options: &ParseOptions) -> Self;
    fn from_tree(tree: Tree<NodeData>, base_url: Option<Url>, encoding: String) -> Self;

    fn tree(&self) -> &Tree<NodeData>;
    fn root(&self) -> NodeRef<'_>;                    // the NodeData::Document node
    fn node(&self, id: NodeId) -> Option<NodeRef<'_>>;
    fn root_element(&self) -> Option<NodeRef<'_>>;    // <html>
    fn head(&self) -> Option<NodeRef<'_>>;
    fn body(&self) -> Option<NodeRef<'_>>;
    fn title(&self) -> Option<String>;
    fn elements(&self) -> impl Iterator<Item = NodeRef<'_>>;  // document order
    fn nodes(&self) -> impl Iterator<Item = NodeRef<'_>>;     // every node, document order

    fn base_url(&self) -> Option<&Url>;
    fn set_base_url(&mut self, url: &str) -> Result<(), DomError>;
    fn urljoin(&self, relative: &str) -> Result<String, DomError>;  // no base -> input unchanged
    fn encoding(&self) -> &str;

    fn html(&self) -> String;      // whole document incl. doctype
    fn pretty(&self) -> String;

    /// One walk, then O(1) document-order comparison — the `TreeModel::document_order`
    /// key. Keyed by `NodeRef`, not `NodeId`, because ids collide across documents.
    fn document_order(&self) -> DocumentOrder<'_>;
}

pub struct DocumentOrder<'a>;
impl<'a> DocumentOrder<'a> {
    fn rank(&self, node: NodeRef<'_>) -> Option<u64>;          // None for a foreign node
    fn compare(&self, a: NodeRef<'_>, b: NodeRef<'_>) -> Ordering;
    fn sort_dedup(&self, nodes: &mut Vec<NodeRef<'a>>);        // XPath node-set normalisation
    fn document(&self) -> &'a Document;
    fn len(&self) -> usize;  fn is_empty(&self) -> bool;
}
```

> **`NodeId` is an arena index, not a globally unique handle.** The same numeric id
> exists in every `Document`, so `doc.node(foreign_id)` will usually return an
> unrelated node rather than `None`. Pass `NodeRef`s across API boundaries — their
> `Eq`/`Hash`/`Ord` all include the document pointer — and use raw ids only inside
> one tree.

`Document: Clone + Send + Sync + Debug`.

## `node` — payloads

```rust
pub enum NodeData {
    Document,
    Doctype { name: String, public_id: String, system_id: String },
    Element(Element),
    Text(String),
    Comment(String),                                   // also holds preserved CDATA
    ProcessingInstruction { target: String, data: String },
}
impl NodeData {
    fn is_document/is_element/is_text/is_comment/is_cdata/is_doctype/is_processing_instruction(&self) -> bool;
    fn as_element(&self) -> Option<&Element>;
    fn as_element_mut(&mut self) -> Option<&mut Element>;
    fn as_text(&self) -> Option<&str>;
    fn as_comment(&self) -> Option<&str>;              // CDATA -> payload without the marker
    fn tag(&self) -> &str;                             // "div" | "#text" | "#comment" | "#document" | "#doctype" | "#processing-instruction"
}
pub fn cdata_payload(comment: &str) -> Option<&str>;   // "[CDATA[x]]" -> Some("x")

pub struct Element;      // Clone + Debug + PartialEq + Eq
impl Element {
    fn new(name: QualName, attrs: Attributes) -> Self;
    fn name(&self) -> &QualName;
    fn local_name(&self) -> &LocalName;                // interned, for the CSS matcher
    fn tag(&self) -> &str;
    fn attrs(&self) -> &Attributes;
    fn attr(&self, local: &str) -> Option<&str>;
    fn has_attr(&self, local: &str) -> bool;
    fn id(&self) -> Option<&str>;
    fn classes(&self) -> &[LocalName];                 // deduped, source order
    fn has_class(&self, class: &str) -> bool;
    fn has_class_ignore_ascii_case(&self, class: &str) -> bool;
    fn set_attr(&mut self, name: QualName, value: impl Into<String>) -> Option<String>;
    fn insert_attr_if_missing(&mut self, name: QualName, value: impl Into<String>);
    fn remove_attr(&mut self, local: &str) -> Option<String>;
}

pub struct Attributes;   // insertion-ordered (IndexMap), order is load-bearing for fingerprints
impl Attributes {
    fn new() / with_capacity(n) -> Self;
    fn len(&self) -> usize;  fn is_empty(&self) -> bool;
    fn get(&self, local: &str) -> Option<&str>;         // ignores namespace
    fn get_qual(&self, name: &QualName) -> Option<&str>;
    fn contains(&self, local: &str) -> bool;
    fn nth(&self, index: usize) -> Option<(&QualName, &str)>;   // XPath attribute nodes
    fn iter(&self) -> impl Iterator<Item = (&str, &str)>;       // (local name, value)
    fn iter_qual(&self) -> impl Iterator<Item = (&QualName, &str)>;
    fn keys(&self) / values(&self) -> impl Iterator<Item = &str>;
    fn to_index_map(&self) -> IndexMap<String, String>;         // == browser_core::AttributeDict
    fn to_attribute_map(&self) -> browser_core::AttributeMap; // Python's AttributesHandler
    fn insert(&mut self, name: QualName, value: impl Into<String>) -> Option<String>;
    fn insert_local(&mut self, local: &str, value: impl Into<String>) -> Option<String>;
    fn insert_if_missing(&mut self, name: QualName, value: impl Into<String>);
    fn remove(&mut self, local: &str) -> Option<String>;
}
impl FromIterator<(QualName, String)> for Attributes;   // duplicates: first wins
```

## `node::NodeRef<'a>` — the handle

```rust
// identity / payload
fn document(&self) -> &'a Document;
fn id(&self) -> NodeId;
fn value(&self) -> &'a NodeData;
fn as_element(&self) -> Option<&'a Element>;
fn as_text(&self) -> Option<&'a str>;
fn as_comment(&self) -> Option<&'a str>;
fn as_processing_instruction(&self) -> Option<(&'a str, &'a str)>;
fn is_element/is_text/is_comment/is_cdata/is_doctype/is_processing_instruction/is_document(&self) -> bool;
fn is_root_element(&self) -> bool;                     // <html>
fn tag(&self) -> &'a str;                              // NodeData::tag
fn local_name(&self) -> Option<&'a str>;               // None for non-elements
fn attrs(&self) -> &'a Attributes;                     // empty map for non-elements
fn attr(&self, local: &str) -> Option<&'a str>;
fn element_id(&self) -> Option<&'a str>;
fn has_class(&self, class: &str) -> bool;
```

### navigation (`nav`) — every step is O(1)

```rust
// up
fn parent(&self) -> Option<NodeRef<'a>>;               // includes the document node
fn parent_element(&self) -> Option<NodeRef<'a>>;       // lxml getparent(): None at <html>
fn ancestors(&self) -> impl Iterator<Item = NodeRef<'a>>;        // elements, closest first
fn ancestor_nodes(&self) -> impl Iterator<Item = NodeRef<'a>>;   // + the document node
fn depth(&self) -> usize;                              // count(ancestor::*)
fn find_ancestor(&self, pred: impl Fn(&NodeRef<'a>) -> bool) -> Option<NodeRef<'a>>;
fn is_descendant_of(&self, other: &NodeRef<'a>) -> bool;

// down
fn children(&self) -> impl Iterator<Item = NodeRef<'a>>;             // all kinds
fn element_children(&self) -> impl Iterator<Item = NodeRef<'a>>;
fn non_comment_children(&self) -> impl Iterator<Item = NodeRef<'a>>; // Python's `children`
fn first_child/last_child/first_element_child/last_element_child(&self) -> Option<NodeRef<'a>>;
fn has_children(&self) -> bool;
fn descendants(&self) -> impl Iterator<Item = NodeRef<'a>>;          // excludes self
fn descendants_and_self(&self) -> impl Iterator<Item = NodeRef<'a>>;
fn descendant_elements(&self) -> impl Iterator<Item = NodeRef<'a>>;  // replaces `.//*`
fn descendant_text_nodes(&self) -> impl Iterator<Item = NodeRef<'a>>;// replaces `.//text()`
fn descendants_with_nonblank_direct_text(&self) -> impl Iterator<Item = NodeRef<'a>>;

// sideways
fn next_sibling/prev_sibling(&self) -> Option<NodeRef<'a>>;                 // any kind
fn next_element_sibling/prev_element_sibling(&self) -> Option<NodeRef<'a>>; // Python's next/previous
fn next_non_comment_sibling/prev_non_comment_sibling(&self) -> Option<NodeRef<'a>>;
fn next_siblings(&self) -> impl Iterator<Item = NodeRef<'a>>;   // document order
fn prev_siblings(&self) -> impl Iterator<Item = NodeRef<'a>>;   // nearest first
fn siblings(&self) -> impl Iterator<Item = NodeRef<'a>>;        // all but self, document order
fn element_siblings/non_comment_siblings(&self) -> impl Iterator<Item = NodeRef<'a>>;
fn index_in_parent(&self) -> usize;                             // 0-based, all children
fn nth_of_type(&self) -> usize;                                 // 1-based, same-tag elements

// text / tail  (see the `nav` module docs for the full lxml mapping)
fn text(&self) -> Option<&'a str>;          // lxml element.text  = first child if it is text
fn tail(&self) -> Option<&'a str>;          // lxml element.tail  = next sibling if it is text
fn text_content(&self) -> String;           // XPath string-value (no separators, no filtering)

// document order
fn document_path(&self) -> Vec<usize>;
fn cmp_document_order(&self, other: &NodeRef<'a>) -> Ordering;
pub fn sort_document_order(nodes: &mut [NodeRef<'_>]);   // free fn in `nav`
```

**`get_all_text` note.** Python attributes a *tail* to `parent.getparent()` and a *text* to
`parent`. In this tree both resolve to the same thing: `text_node.parent()`. So the `ignore_tags`
check is just "is this text node's parent element in the ignored set" — no tail/text branch.

### serialisation (`serialize`)

```rust
fn outer_html(&self) -> String;   // Python's html_content; tail NOT included
fn inner_html(&self) -> String;
fn pretty(&self) -> String;       // Python's prettify(); text-bearing elements stay on one line

pub const REPR_LENGTH_LIMIT: usize = 40;
pub use browser_core::text::clean_spaces;  // parser.md §7.4; -> Cow<'_, str>
pub fn truncate_repr(content: &str) -> String;
pub fn escape_text(text: &str) -> String;
pub fn escape_attr(value: &str) -> String;
```

`Debug for NodeRef` is Python's `__repr__`: `<text='…'>` or `<data='…' parent='…'>`.

### mutation (`edit`, on `&mut Document`)

```rust
fn detach(&mut self, id: NodeId) -> Result<(), DomError>;
fn drop_tree(&mut self, id: NodeId) -> Result<(), DomError>;   // lxml drop_tree == detach here
fn drop_tag(&mut self, id: NodeId) -> Result<(), DomError>;    // unwrap: keep the children
fn remove_children(&mut self, id: NodeId) -> Result<(), DomError>;
fn remove_matching(&mut self, pred: impl Fn(NodeRef<'_>) -> bool) -> usize;
fn remove_elements_named(&mut self, names: &[&str]) -> usize;
fn set_attr(&mut self, id: NodeId, name: &str, value: &str) -> Result<Option<String>, DomError>;
fn remove_attr(&mut self, id: NodeId, name: &str) -> Result<Option<String>, DomError>;
fn append_child(&mut self, parent: NodeId, data: NodeData) -> Result<NodeId, DomError>;
fn insert_before(&mut self, sibling: NodeId, data: NodeData) -> Result<NodeId, DomError>;
fn set_value(&mut self, id: NodeId, data: NodeData) -> Result<(), DomError>;
```

Detaching merges the now-adjacent text siblings, which is exactly what lxml's tail re-parenting
produces. Detached nodes keep their ids but leave every traversal.

### names (`qualname`)

```rust
pub fn html_name(local: &str) -> QualName;       // ns = html
pub fn attr_name(local: &str) -> QualName;       // ns = "" (html5ever's convention)
pub fn prefixed_name(prefix: &str, ns: &str, local: &str) -> QualName;
pub fn to_string(name: &QualName) -> String;     // "href" | "xlink:href"
pub fn is_void_element(local: &LocalName) -> bool;
pub fn is_raw_text_element(local: &LocalName) -> bool;   // script/style/… : never escaped
pub fn preserves_whitespace(local: &LocalName) -> bool;  // pre/textarea/… : blank text kept
```

### errors (`error`)

```rust
pub enum DomError {                   // #[non_exhaustive], thiserror
    MissingNode(NodeId),              // "no node with id {0:?} exists in this document"
    NotAnElement(NodeId),             // "node {0:?} is not an element"
    RootNotDetachable,                // "the document root cannot be detached"
    InvalidBaseUrl(url::ParseError),  // "invalid base URL: {0}"
}
pub type Result<T, E = DomError> = std::result::Result<T, E>;
```

---

## Behaviour you can rely on

| Python / lxml | here |
|---|---|
| `content.strip().replace("\x00","") or "<html/>"` | `prepare_str`, applied by every `parse*` entry point |
| bytes body: NUL-stripped, **not** trimmed | `prepare_bytes` |
| `remove_comments=True` | comment nodes deleted, adjacent text re-merged |
| `remove_blank_text=True` | whitespace-only text nodes deleted outside `pre/textarea/script/style/title/xmp/listing` |
| `strip_cdata=True` | `<![CDATA[x]]>` becomes a text node `x` |
| `keep_cdata=True` | the section stays one node: `is_cdata()`, `as_comment() == Some("x")`, serialises back as `<![CDATA[x]]>` |
| `default_doctype=True` | free — html5ever always builds `html`/`head`/`body`, so a bare fragment is wrapped |
| duplicate attributes | first occurrence wins |
| valueless attribute (`<div hidden>`) | `Some("")` |

## Deviations worth knowing (also in the unit's report)

1. **Text is `String`, not `StrTendril`.** `StrTendril` is `!Send`/`!Sync`; storing it would make
   `Document` unusable from `rayon`, which `relocate` and the spider need. `NodeData::Text(String)`.
2. **No `Cdata` variant.** html5ever never produces one in HTML content (it tokenises
   `<![CDATA[…]]>` as a bogus comment), so a preserved section is a `Comment` carrying the
   `[CDATA[…]]` marker. `is_cdata()` / `as_comment()` / `cdata_payload()` hide that. With the
   default `strip_cdata` behaviour you never see it at all. lxml would expose the payload as the
   parent's `.text`; we expose it as a distinct node.
3. **`remove_blank_text` is simpler than libxml2's `areBlanks()`.** We delete every whitespace-only
   text node outside whitespace-preserving elements. libxml2 additionally keeps whitespace that
   follows inline content in a few mixed-content cases. Since adjacent text is always merged, the
   difference only ever shows up in `outer_html` spacing, never in `get_all_text` (whose default
   `valid_values=true` drops blank parts anyway).
4. **Malformed HTML recovers per the HTML5 spec, not per libxml2.** `ARCHITECTURE.md` §11.6 accepts
   this; `tests/malformed.rs` pins the actual behaviour for foster parenting, adoption agency,
   stray end tags, unterminated comments and raw-text elements.
5. **`Element`'s fields are private.** `ARCHITECTURE.md` sketches `pub attrs`, but `id`/`classes`
   are derived indexes that must stay in sync, so mutation goes through `set_attr`/`remove_attr`.
6. **`Ord for NodeRef` is arena order**, not document order — it exists only to satisfy
   `Copy + Eq + Ord` bounds (the XPath `TreeModel`). Use `cmp_document_order` /
   `sort_document_order` for real ordering.
7. **`Attributes` and `browser-core`'s `AttributeMap` are both real, and that is deliberate.**
   This crate stores `QualName -> String` because XPath needs the namespace and the CSS matcher
   needs the interning. `AttributeMap` is the flat, read-only *presentation* type (Python's
   `AttributesHandler`, with `search_values` / `json_string`) that `browser-parser` returns to
   users. `Attributes::to_attribute_map()` and `Attributes::to_index_map()` cross the boundary; the
   latter is exactly `browser_core::AttributeDict`, i.e. what `ElementFingerprint` serialises.
8. **`:empty` and the `preceding-sibling` axis see the *normalised* tree.** `remove_blank_text` and
   `remove_comments` both default to on (Python passes the same flags to lxml — `parser.py:157`),
   so `<span> </span>` is `:empty` here. Parse with `ParseOptions::new().remove_blank_text(false)`
   for the browser answer.
9. **`xml:lang` on an HTML element has no namespace and a colon in its local name.** html5ever only
   runs the HTML5 "adjust foreign attributes" step inside SVG/MathML, where `xlink:href` &c. *do*
   get a real namespace and prefix. This matches every browser; lxml, being an XML parser, differs.
   Pinned by `browser-selectors/tests/dom_seam.rs`.
