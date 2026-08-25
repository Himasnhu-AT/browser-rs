# `browser-xpath` — API cheat sheet

A complete XPath 1.0 engine evaluated directly over the `browser-dom` tree: 13 axes, every node
test, all 27 core functions, the four value types with the 1.0 conversion rules, `$variables`, and
node-set results in document order with attribute nodes as first-class members.

> **Both spellings work.** The names below are re-exported from the crate root by the integration
> wave, so `browser_xpath::XPath` and `browser_xpath::eval::XPath` are the same item. Anything
> not in the root list is reached through its module.

```rust
use browser_xpath::error::XPathError;
use browser_xpath::eval::{self, Context, Namespaces, Variable, Variables, XPath};
use browser_xpath::model::{DomModel, NodeKind, TreeModel, XNode};
use browser_xpath::value::Value;
```

---

## 30-second orientation

```rust
use browser_dom::Document;
use browser_xpath::eval::{Variables, XPath};
use browser_xpath::model::DomModel;

let doc = Document::parse("<ul><li>one</li><li>two</li></ul>");

// One-shot: compile + evaluate + require a node-set.
let nodes = browser_xpath::eval::select("//li/text()", &doc, doc.root())?;
assert_eq!(nodes[1].string_value(), "two");

// Repeated use: compile once, index the document once.
let model = DomModel::new(&doc);                 // one walk of the tree — reuse it
let expr = XPath::compile("//li[$n]/text()")?;   // parse errors surface HERE
let mut vars = Variables::new();
vars.insert("n", 2.0);
let value = expr.eval(&model, model.root(), &vars)?;
assert_eq!(value.string_value(&model), "two");
# Ok::<(), browser_xpath::error::XPathError>(())
```

**Context node.** `Selector.xpath()` in Python evaluates against `self._root`, the `<html>`
*element*, not the document node — `model.root_element()` here. `/` still means the document node in
both. Use `model.root()` (the document node) only when you really mean `/`.

---

## `eval` — the entry point

```rust
pub struct XPath;                                  // Clone + Debug + Send + Sync
impl XPath {
    pub fn compile(expr: &str) -> Result<Self, XPathError>;
    pub fn source(&self) -> &str;
    pub fn ast(&self) -> &ast::Expr;               // Display prints the unabbreviated form
    pub fn is_fast_path(&self) -> bool;

    // generic over any TreeModel
    pub fn eval<M: TreeModel>(&self, m: &M, ctx: M::Node, vars: &Variables)
        -> Result<Value<M::Node>, XPathError>;
    pub fn eval_with<M: TreeModel>(&self, m: &M, ctx: M::Node, vars: &Variables, ns: &Namespaces)
        -> Result<Value<M::Node>, XPathError>;
    pub fn select<M: TreeModel>(&self, m: &M, ctx: M::Node, vars: &Variables)
        -> Result<Vec<M::Node>, XPathError>;       // errors if the result is not a node-set

    // convenience over browser-dom (each call builds a DomModel)
    pub fn select_nodes<'a>(&self, doc: &'a Document, ctx: NodeRef<'a>, vars: &Variables)
        -> Result<Vec<XNode<'a>>, XPathError>;
    pub fn evaluate<'a>(&self, doc: &'a Document, ctx: NodeRef<'a>, vars: &Variables)
        -> Result<Value<XNode<'a>>, XPathError>;
}

// compile + evaluate in one call
pub fn select<'a>(expr: &str, doc: &'a Document, ctx: NodeRef<'a>)
    -> Result<Vec<XNode<'a>>, XPathError>;
pub fn evaluate<'a>(expr: &str, doc: &'a Document, ctx: NodeRef<'a>)
    -> Result<Value<XNode<'a>>, XPathError>;

pub struct Variables;                              // Clone + Debug + Default + PartialEq
impl Variables {
    pub fn new() -> Self;
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<Variable>) -> Option<Variable>;
    pub fn get(&self, name: &str) -> Option<&Variable>;
    pub fn len(&self) -> usize;  pub fn is_empty(&self) -> bool;
}
impl<K: Into<String>, V: Into<Variable>> FromIterator<(K, V)> for Variables;

pub enum Variable { Str(String), Number(f64), Bool(bool) }
// From<&str>, From<String>, From<f64>, From<i64>, From<bool>

pub struct Namespaces;                             // `xml` is pre-bound
impl Namespaces {
    pub fn new() -> Self;
    pub fn insert(&mut self, prefix: impl Into<String>, uri: impl Into<String>) -> Option<String>;
    pub fn resolve(&self, prefix: &str) -> Result<&str, XPathError>;
}

pub struct Context<N> { pub node: N, pub position: usize, pub size: usize }
impl<N: Copy> Context<N> { pub fn new(node: N) -> Self; }
```

`Selector.xpath(selector, **kwargs)` maps to: `XPath::compile(selector)?` →
`Variables` from the kwargs → `select(&model, root_element, &vars)`. A scalar result
(`count(…)`, `string(…)`) comes back as `XPathError::NotANodeSet` from `select`; use `eval` when you
want the scalar.

## `value::Value<N>` — the four types

```rust
pub enum Value<N> { NodeSet(Vec<N>), Str(String), Number(f64), Bool(bool) }
impl<N: Copy + Eq + Ord> Value<N> {
    pub fn empty_node_set() -> Self;
    pub fn type_name(&self) -> &'static str;              // "node-set" | "string" | "number" | "boolean"
    pub fn is_node_set(&self) -> bool;
    pub fn as_node_set(&self) -> Option<&[N]>;
    pub fn into_node_set(self, context: &str) -> Result<Vec<N>, XPathError>;
    pub fn boolean(&self) -> bool;                                        // boolean()
    pub fn string_value<M: TreeModel<Node = N>>(&self, m: &M) -> String;  // string()
    pub fn string_cow<'v, M: TreeModel<Node = N>>(&'v self, m: &'v M) -> Cow<'v, str>;
    pub fn number<M: TreeModel<Node = N>>(&self, m: &M) -> f64;           // number()
}

pub fn format_number(value: f64) -> String;   // string(number), libxml2-compatible
pub fn parse_number(text: &str) -> f64;       // number(string)
pub fn normalize_space(text: &str) -> String;
pub fn round_half_up(value: f64) -> f64;      // round(): half towards +Infinity
```

**Node-sets are always in document order and de-duplicated.**

## `model` — the tree seam

```rust
pub trait TreeModel {
    type Node: Copy + Eq + Ord;
    fn parent(&self, n: Self::Node) -> Option<Self::Node>;
    fn children(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;
    fn kind(&self, n: Self::Node) -> NodeKind;
    fn name(&self, n: Self::Node) -> Option<(&str /*ns*/, &str /*local*/)>;
    fn string_value(&self, n: Self::Node) -> Cow<'_, str>;
    fn attributes(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;
    fn document_order(&self, n: Self::Node) -> u64;
    // defaulted:
    fn namespaces(&self, n: Self::Node) -> impl Iterator<Item = Self::Node>;   // empty
    fn root_of(&self, n: Self::Node) -> Self::Node;                            // walk up
    fn attribute_value(&self, n: Self::Node, local: &str) -> Option<Cow<'_, str>>;
}

pub enum NodeKind { Root, Element, Text, Comment, Attr, Namespace, Pi }

pub enum XNode<'a> {                       // Copy + Eq + Ord + Hash + Debug
    Tree(NodeRef<'a>),
    Attr(NodeRef<'a>, usize),              // (owner element, attribute index)
    Namespace(NodeRef<'a>, NsSource<'a>),
}
impl<'a> XNode<'a> {
    pub fn new(node: NodeRef<'a>) -> Self;
    pub fn anchor(&self) -> NodeRef<'a>;              // the node, or an attribute's element
    pub fn as_tree_node(&self) -> Option<NodeRef<'a>>;
    pub fn as_element(&self) -> Option<NodeRef<'a>>;  // -> wrap in a Selector
    pub fn as_attribute(&self) -> Option<(&'a str, &'a str)>;   // (local name, value)
    pub fn as_text(&self) -> Option<&'a str>;
    pub fn kind(&self) -> NodeKind;
    pub fn string_value(&self) -> Cow<'a, str>;       // -> wrap in a TextHandler
}

pub struct DomModel<'a>;
impl<'a> DomModel<'a> {
    pub fn new(doc: &'a Document) -> Self;      // one document walk; build once, reuse
    pub fn root(&self) -> XNode<'a>;            // the document node — what `/` selects
    pub fn root_element(&self) -> Option<XNode<'a>>;   // <html> — Python's `_root`
    pub fn document(&self) -> &'a Document;
    pub fn node(&self, node: NodeRef<'a>) -> XNode<'a>;
}

pub const XML_NAMESPACE: &str;    pub const XMLNS_NAMESPACE: &str;
pub const HTML_NAMESPACE: &str;   // reported as the NULL namespace — see deviation 2
```

Wrapping an XPath result for `browser-parser`:

```rust
for node in nodes {
    match node.as_element() {
        Some(element) => { /* Selector::from(element) */ }
        None => { /* TextHandler::from(node.string_value()) — text and attribute nodes */ }
    }
}
```

## `error::XPathError`

```rust
pub enum XPathError {                       // #[non_exhaustive], thiserror, Clone + PartialEq
    Syntax { expr: String, position: usize, message: String },
    Unsupported { construct: String },
    Arity { name: String, expected: String, got: usize },
    UndefinedVariable { name: String },
    UndefinedPrefix { prefix: String },
    NotANodeSet { context: String, actual: &'static str },
}
impl XPathError { pub fn is_compile_time(&self) -> bool; }
pub type Result<T, E = XPathError> = std::result::Result<T, E>;
```

`Selector.xpath` maps **every** variant to `SelectorSyntaxError(f"Invalid XPath selector: {selector}")`.

**Compile time** (`XPath::compile`): syntax errors, unknown functions, unknown axes, extension
functions, wrong arity, XPath 2.0 constructs (`for`, `to`, `eq`, parenthesised steps, `element()`).
**Run time** (`eval`): unbound `$variable`, unbound namespace prefix, an operator applied to a
non-node-set (`'a' | 'b'`). Nothing is ever silently answered with an empty node-set.

## The other modules

`lexer` (`tokenize`, `Token`, `Spanned`), `parser` (`parse`), `ast` (`Expr`, `Path`, `PathStart`,
`Step`, `Axis`, `NodeTest`, `BinaryOp`, `CoreFunction`), `axes` (`axis_nodes`, `descendants`,
`matches_test`), `functions` (`substring`, `translate`, `substring_before`, `substring_after`,
`lang_matches`, `number_to_string`) are public for testing and tooling; day-to-day use only needs
`eval`, `model`, `value` and `error`.

---

## Behaviour you can rely on

| XPath 1.0 | here |
|---|---|
| node-set → string | string-value of the node first **in document order** |
| `string(1.0)` / `string(1 div 3)` / `string(1e15)` | `1` / `0.333333333333333` / `1e+15` — libxml2's formatting, not the REC's |
| `string(0 div 0)`, `string(1 div 0)` | `NaN`, `Infinity` |
| `round(-0.5)` | `-0`, which prints as `0` |
| `boolean('0')` | `true` |
| predicate returning a number | true when it equals `position()` |
| `preceding-sibling::*[1]` | the **nearest** preceding sibling (proximity position) |
| `string-length('😀')` | `1` — characters, not bytes or UTF-16 units |
| `//a[1]` | the first `a` child *of each parent*; `(//a)[1]` is the first in the document |
| `@*`, `.//a/@href` | attribute nodes, ordered after their element and before its children |
| `namespace::*` | the in-scope `xmlns` declarations plus the implicit `xml` prefix |
| the doctype | not in the data model: invisible to `node()` and every axis |

### Deviations worth knowing

1. **An unprefixed name test matches on the local name**, whatever the namespace. Strict XPath 1.0
   matches only the null namespace, which would make `//div` select nothing (html5ever puts HTML
   elements in the XHTML namespace) — and lxml's HTML parser, the reference, has no namespaces at
   all. A *prefixed* test still compares the resolved URI.
2. **The XHTML namespace is reported as the null namespace.** html5ever has to put every HTML
   element in a namespace and picks XHTML; libxml2's HTML parser — the reference — has no
   namespaces at all, so `namespace-uri(//div)` is `""` in both. Foreign content keeps its real
   namespace: an element inside `<svg>`/`<math>` still reports the SVG/MathML URI, so a bound
   prefix (`//svg:rect`) selects it. `name()`/`local-name()` agree with lxml either way.
3. **`lang()` and `@xml:lang` do not see the HTML spelling.** html5ever keeps `xml:lang` on an HTML
   element as one un-namespaced attribute whose local name is literally `xml:lang`; libxml2 does the
   same and its `lang()` misses it too, so both engines answer `false`. Inside SVG/MathML the
   attribute really is namespaced and both work. `@*[local-name() = 'xml:lang']` reaches the HTML one.
4. **Blank text nodes.** `browser-dom` drops every whitespace-only text node; libxml2 keeps a few
   in mixed content. So `count(//text())`, `count(//node())` and the string-value of a mixed-content
   element (`normalize-space(//nav)` → `HomeAboutContact`, not `Home About Contact`) differ from
   lxml. `tests/conformance.rs::the_known_divergences_from_lxml` pins every case.
5. **Node-set `$variables` are not bindable** — `Variables` holds string/number/boolean, which is
   exactly what `Selector.xpath(**kwargs)` can pass.

## Conformance

Two golden corpora, both generated by running every expression through **lxml 6.1.2 / libxml2
2.14.6** and both re-runnable:

| file | cases | what it pins |
|---|---|---|
| `testdata/xpath_golden.tsv` | 769 | node-set *signatures* over 3 pages, plus the expressions the eight subsystem specs issue |
| `testdata/xpath_conformance.jsonl` | 3 077 | 14 documents × the whole language, node-sets serialised as **outer HTML** |

`tests/conformance_lxml.rs` is the gate for the second one: 3 077 / 3 077 agree. Its header
documents the encoding; the short version is that a row carries the result type, the value, and —
for the 80 rows where the two engines genuinely differ — a `divergence` string naming the cause and
lxml's own answer beside ours. Nothing is ever dropped to raise the score. (It was 85 before
`browser-dom` adopted libxml2's `areBlanks()`: five rows on `divergent.html` that only differed
over blank text now agree.) The differences are:

* **63 rows on `divergent.html`**, a page built entirely out of constructs where html5ever and
  libxml2 build *different trees* (the adoption agency algorithm cloning `<i>` across `</b>`, a
  synthesised `<tbody>`, a valueless attribute). Their expected values
  come from `divergent_as_html5.html`, a page libxml2 parses into exactly the tree html5ever builds
  for `divergent.html` — so they are still externally derived, never hand-written.
* **3 rows**: libxml2's HTML parser ignores `xmlns:*` attributes, we keep them and synthesise
  namespace nodes.
* **7 rows**: lxml's *Python binding* refuses a top-level `position()`/`last()` ("Invalid context
  size"); XPath 1.0 says both are 1, which is what every predicate in the corpus proves both
  engines otherwise do.
* **4 rows**: libxml2's optimiser drops a lone `[1]` predicate on a computed non-node-set, so
  `(1 + 2)[1]` is 3 there. It rejects `(1)[1]`, `(1 + 2)[2]` and `count(//li)[1]` in the same
  breath; XPath 1.0 makes all of them errors and so do we.
* **3 rows**: libxml2's hand-rolled decimal→double conversion underflows `5e-324` (the smallest
  positive double) to zero; Rust's is correctly rounded.

## Fast paths

`XPath::compile` recognises the expressions the library issues itself and evaluates them as a single
descendant walk (`XPath::is_fast_path()` reports it): `.//*`, `//*`, `descendant::*`, `.//text()`,
`.//tag`, `//tag`, `.//tag/@attr`, `//*/@attr`, `.//*[normalize-space(text())]`, and unions of those
— including the link extractor's `.//a/@href| .//area/@href`. Results are identical to the general
evaluator's; `eval::tests::fast_paths_agree_with_the_general_evaluator` proves it expression by
expression.

## Crate-root re-exports

Applied by the integration wave (unit 6 itself was not permitted to edit `lib.rs`). The module
paths remain valid; these are just the short spellings:

```rust
pub use crate::error::{Result, XPathError};
pub use crate::eval::{evaluate, select, Namespaces, Variable, Variables, XPath};
pub use crate::model::{DomModel, NodeKind, TreeModel, XNode};
pub use crate::value::Value;
```
