# `browser-selectors` — API cheat sheet

Native CSS matching on Servo's `selectors`, extended with the reference implementation's `::text` / `::attr()` and
cssselect's `:contains()`. **No CSS→XPath translation happens anywhere** (`ARCHITECTURE.md` §1.3).

One `use` is enough — the integration wave hoisted the names a caller needs to the crate root
(unit 5 itself was not permitted to edit `lib.rs`):

```rust
use browser_selectors::{
    parse_selector, SelectorGroup, SubSelector, Tail, Match, MatchValue, TextRole, SelectorError,
};
```

`browser_selectors::parse::{…}` re-exports the same set, and every module path below stays valid:

| item | path |
|---|---|
| `parse_selector`, `canonical_parts`, `SelectorGroup`, `CssSelector`, `SubSelector`, `SelectorParser` | `browser_selectors::parse` |
| `Match`, `MatchValue`, `TextRole`, `matches_one`, `select_one`, `select_group*` | `browser_selectors::matching` |
| `Tail`, `BrowserPseudoClass`, `BrowserPseudoElement` | `browser_selectors::pseudo` |
| `BrowserImpl`, `ElementRef`, `CssLocalName`, `CssNamespace`, `CssString` | `browser_selectors::impl_selector` |
| `SelectorError`, `Result` | `browser_selectors::error` |
| `serialize`, `serialize_group` | `browser_selectors::canonical` |

---

## 30-second orientation

```rust
let doc = browser_dom::Document::parse("<p id=x>hi<b>B</b>tail</p><a href='/y'>y</a>");

let group = parse_selector("p::text, a::attr(href)")?;   // one parse, reused
for m in group.select(doc.root()) {
    m.element();        // NodeRef of the <p> / <a> that matched
    m.node();           // the text node for ::text, the element otherwise
    m.as_str();         // Some("hi") / Some("tail") / Some("/y"); None for an element match
    m.is_text_node();   // Python's `_is_text_node`: true for ::text AND ::attr()
    m.text_role();      // Some(TextRole::Text | TextRole::Tail) for ::text only
}
```

`SelectorGroup` is `Clone + Send + Sync` — compile once, share across threads, cache by source
string (this crate deliberately has no cache of its own; `ARCHITECTURE.md` §7 puts it in
`browser-parser`).

---

## `parse` — the entry point

```rust
pub fn parse_selector(selector: &str) -> Result<SelectorGroup, SelectorError>;
pub fn canonical_parts(selector: &str) -> Result<Vec<String>, SelectorError>;

pub struct SelectorGroup;                 // the whole comma-separated list
pub type CssSelector = SelectorGroup;     // ARCHITECTURE.md §1.3's name
impl SelectorGroup {
    fn parse(selector: &str) -> Result<Self, SelectorError>;   // == parse_selector
    fn parts(&self) -> &[SubSelector];
    fn len(&self) -> usize;  fn is_empty(&self) -> bool;  fn is_group(&self) -> bool;

    fn canonical(&self) -> String;             // parts joined with ", "
    fn canonical_parts(&self) -> Vec<String>;  // cssselect's `s.canonical()` per part
    fn common_tail(&self) -> Option<&Tail>;    // None for "p::text, a::attr(href)"

    fn matches(&self, element: NodeRef<'_>) -> bool;            // unscoped, Element.matches()
    fn select<'a>(&self, root: NodeRef<'a>) -> Vec<Match<'a>>;  // the XPath union
    fn select_first<'a>(&self, root: NodeRef<'a>) -> Option<Match<'a>>;
    fn select_elements<'a>(&self, root: NodeRef<'a>) -> Vec<NodeRef<'a>>;  // tails ignored
    fn select_per_part<'a>(&self, root: NodeRef<'a>) -> Vec<Vec<Match<'a>>>;
}
impl FromStr for SelectorGroup;   impl Display for SelectorGroup;

pub struct SubSelector;                   // one part of the group
impl SubSelector {
    fn matcher(&self) -> &selectors::parser::Selector<BrowserImpl>;
    fn subtree_head(&self) -> Option<&selectors::parser::Selector<BrowserImpl>>;  // `div *::text`
    fn has_combinator(&self) -> bool;      // ` `, `>`, `+`, `~` at the top level
    fn tail(&self) -> &Tail;
    fn canonical(&self) -> &str;
    fn specificity(&self) -> u32;
    fn matches(&self, element: NodeRef<'_>) -> bool;
    fn matches_scoped(&self, element: NodeRef<'_>, scope: NodeRef<'_>) -> bool;
    fn select<'a>(&self, root: NodeRef<'a>) -> Vec<Match<'a>>;
}
```

### Which `select` do I want?

`parser.md` §3.4 has **two** paths, and they differ:

```python
if (not adaptive_enabled) or ("," not in selector):
    return self.xpath(css_to_xpath(selector), …)          # ONE XPath, a union
for single in cssselect.parse(selector):
    results += self.xpath(css_to_xpath(single.canonical()), identifier or single.canonical(), …)
```

| Python path | here | behaviour |
|---|---|---|
| non-adaptive, or no comma | `SelectorGroup::select` | XPath union: document order, **de-duplicated** |
| adaptive **and** a comma | `SelectorGroup::select_per_part` + `SubSelector::canonical` as the identifier | one vector per part, concatenated, **not** de-duplicated |

`css("p, p")` therefore returns 1 result on the first path and 2 on the second. That is Python's
behaviour, not a bug of ours.

---

## `matching` — what a match is

```rust
pub struct Match<'a>;
impl<'a> Match<'a> {
    fn element(&self) -> NodeRef<'a>;      // the element that satisfied the selector
    fn value(&self) -> &MatchValue<'a>;
    fn tail(&self) -> Tail;
    fn node(&self) -> NodeRef<'a>;         // text node for ::text, element otherwise
    fn as_str(&self) -> Option<&'a str>;   // the string value of a ::text / ::attr() match
    fn is_text_node(&self) -> bool;        // Python's `_is_text_node`
    fn text_role(&self) -> Option<TextRole>;
}

pub enum MatchValue<'a> {
    Element,
    Text { node: NodeRef<'a>, role: TextRole },
    Attr { name: LocalName, value: &'a str },
}
pub enum TextRole { Text, Tail }           // lxml `element.text` vs `element.tail`
pub enum Tail { Element, Text, Attr(LocalName) }
```

`Match` is `Clone + Eq + Hash + Debug`.

**Mapping to `ARCHITECTURE.md` §3's `Node`:**

| `MatchValue` | `Node` |
|---|---|
| `Element` | `Node::Element { id: m.element().id() }` |
| `Text { node, role: Text }` | `Node::Text { owner: m.element().id(), kind: TextKind::Text, value: node.as_text() }` |
| `Text { node, role: Tail }` | `Node::Text { owner: …, kind: TextKind::Tail, … }` |
| `Attr { name, value }` | `Node::Text { owner: …, kind: TextKind::Attribute(name), value }` |

> lxml's smart string for a *tail* answers `getparent()` with the element it trails, not with the
> element whose `/text()` selected it. `Match::element()` is always the latter (the XML-infoset
> parent). Use `m.node().prev_element_sibling()` if you need lxml's answer.

Free functions, for callers that hold a `SubSelector` directly:

```rust
pub fn matches_one(part: &SubSelector, element: NodeRef<'_>, scope: Option<NodeRef<'_>>) -> bool;
pub fn select_one<'a>(part: &SubSelector, root: NodeRef<'a>) -> Vec<Match<'a>>;
pub fn select_group<'a>(parts: &[SubSelector], root: NodeRef<'a>) -> Vec<Match<'a>>;
pub fn select_group_first<'a>(parts: &[SubSelector], root: NodeRef<'a>) -> Option<Match<'a>>;
pub fn select_group_elements<'a>(parts: &[SubSelector], root: NodeRef<'a>) -> Vec<NodeRef<'a>>;
```

---

## Scoping: `select(root)` is `descendant-or-self::`, not `querySelectorAll`

Python translates every selector with a `descendant-or-self::` prefix rooted at the node the query
was issued on. Two consequences a browser would get wrong:

1. **`root` itself is a candidate** — but only for a selector with no top-level combinator.
   `section.css("section")` returns the section; `section.css("div section")` and
   `section.css("* + *")` do not, because a combinator's anchor (an ancestor, or a preceding
   sibling) would have to live inside `descendant-or-self::section`, and none can.
2. **Ancestors outside `root` do not count.** With
   `<div class=container><section><p>` , `section.css(".container p")` is **empty** — a browser
   would match, because it looks for `.container` anywhere. `ElementRef` clips `parent_element()`
   at the scope to reproduce lxml.

Sibling axes and `:root` are *not* clipped, because cssselect does not clip them either
(`:first-child` is `count(preceding-sibling::*) = 0` over the real document; `:root` is
`not(parent::*)`, i.e. `<html>`). The one place that would have leaked — the query node matching
`+`/`~` through a sibling of its own — is handled by the combinator rule above.

`:scope` is the query node, matching cssselect's `descendant-or-self::*[position() = 1]`.

---

## Supported selector vocabulary

Everything cssselect's `HTMLTranslator` supports, plus what Servo gives us for free.

| group | selectors |
|---|---|
| simple | `*`, `div`, `#id`, `.class`, `*\|a` |
| attribute | `[a]`, `[a=v]`, `[a~=v]`, `[a\|=v]`, `[a^=v]`, `[a$=v]`, `[a*=v]`, `[a!=v]` — values always compare **case-sensitively**, and the Selectors-4 flags `[a=v i]` / `[a=v s]` are rejected, because cssselect rejects them |
| combinators | descendant, `>`, `+`, `~` |
| structural | `:root`, `:scope`, `:empty`, `:first-child`, `:last-child`, `:only-child`, `:first-of-type`, `:last-of-type`, `:only-of-type`, `:nth-child()`, `:nth-last-child()`, `:nth-of-type()`, `:nth-last-of-type()` |
| logical | `:not()`, `:is()`, `:matches()`, `:where()`, `:has()` |
| HTML | `:link`, `:visited`, `:hover`, `:active`, `:focus`, `:target`, `:checked`, `:enabled`, `:disabled`, `:lang()` |
| cssselect | `:contains("…")` |
| the reference implementation | `::text`, `::attr(name)` |

`:visited` / `:hover` / `:active` / `:focus` / `:target` parse and never match — cssselect compiles
them to the constant `0`, and a static tree has no user state.

**`::data()` does not exist.** `parser.md` §6.2: the reference implementation 0.4.15 adds only `::text` and `::attr()`.
`a::data(x)` fails with the same message Python gives.

---

## Errors

```rust
pub enum SelectorError {            // #[non_exhaustive], thiserror
    Syntax(String),                          // cssselect's SelectorSyntaxError
    Empty,                                   // "" / "   "
    EmptyCompound,                           // internal, never escapes `parse`
    UnknownPseudoElement(String),            // "The pseudo-element ::foo is unknown"
    UnknownFunctionalPseudoElement(String),  // "The functional pseudo-element ::data() is unknown"
    UnknownPseudoClass(String),
    UnknownFunctionalPseudoClass(String),
    BadAttrArgument(String),                 // "Expected a single string or ident for ::attr(), got …"
    BadPseudoClassArgument { name, got },
}
impl SelectorError { fn is_expression_error(&self) -> bool; }
```

`Display` is only the *inner* message. `browser-parser` is responsible for the outer wrapping
`SelectorSyntaxError(f"Invalid CSS selector '{selector}': {e}")` (`parser.md` §3.4):

```rust
parse_selector(sel).map_err(|e| ParseError::CssSyntax(format!("Invalid CSS selector '{sel}': {e}")))
```

`is_expression_error()` tells the two Python exception classes apart (`ExpressionError` vs
`SelectorSyntaxError`) for callers that need it.

---

## Canonical identifiers (the adaptive-storage key)

`SubSelector::canonical()` is our `cssselect.Selector.canonical()`. It includes the
pseudo-element, and is a fixed point of the parser (parsing it again yields itself).

`ARCHITECTURE.md` §1.3 already accepts that the *spelling* differs from cssselect's, so adaptive
`.db` files are not identifier-compatible with Python's (the fingerprint JSON, which decides
scoring, still is):

| selector | cssselect | here |
|---|---|---|
| `a[href^="/p/"]` | `a[href^='/p/']` | `a[href^="/p/"]` |
| `h1:contains("Phone")` | `h1:contains('Phone')` | `h1:contains("Phone")` |
| `DIV.a P` | `DIV.a P` | `div.a p` |
| `[a!="b"]` | `[a!='b']` | `:not([a="b"])` |

---

## Divergences from Python (all deliberate)

1. **`[attr!=value]` is rewritten to `:not([attr=value])`** before parsing (`[a!=""]` becomes
   `[a]:not([a=""])`, matching cssselect's `@a != ''`). `!=` is in no CSS level and Servo's
   attribute grammar is not extensible; the rewrite is string-literal aware and only fires when the
   selector contains a `!`. Selection results are identical; the canonical string is not.
2. **The CSS Selectors 4 grammar is wider than cssselect's**, in seven places the differential test
   pins by name. All of them are `SelectorSyntaxError`/`ExpressionError` in Python, so no *working*
   the reference implementation selector changes meaning — `ARCHITECTURE.md` §1.3 signs this class off and names
   `*:nth-of-type()` explicitly:
   `.dot:first-of-type` / `:nth-of-type(1)` (cssselect demands an explicit type selector before an
   `*-of-type` pseudo-class), `li:has(a[href])` and `div:has(p span)` (its `:has()` takes only a
   simple selector sequence), `p:not(a, b)` and `p:not(a b)` (its `:not()` takes a single compound),
   and `li:nth-child(2 of .x)`.
3. **`:link` really tests `@href` on `a`/`link`/`area`**, which is what cssselect compiles it to —
   but `:visited` and friends stay constant-false.
4. **`::attr(NAME)` is not lower-cased** — faithfully. `css_to_xpath("a::attr(HREF)")` is
   `…/@HREF`. html5ever, like libxml2, lower-cases *HTML* attribute names at parse time, so
   `a::attr(HREF)` selects nothing on either side — but html5ever keeps SVG's camel case, so
   `svg::attr(viewBox)` finds the attribute here and nothing in Python.
5. **A group's results can escape the subtree in lxml but not here.** `node.css("* ~ *")` is
   `descendant-or-self::*/following-sibling::*`, so in lxml the *query node's own* following
   siblings are reachable even though they are outside its subtree; we enumerate candidates over
   `descendant-or-self(root)` only and stop there. 8 rows of the differential corpus, all of the
   form "Python found more", are exactly this. (The converse error — the query node matching a
   combinator through an anchor Python cannot use — was a real bug and is fixed; see below.)
6. **Whitespace-only `::text` results differ**, inherited from `browser-dom` deviation #3
   (`remove_blank_text` is stricter than libxml2's `areBlanks()`, and it also drops a `&nbsp;`-only
   text node, which libxml2 never treats as blank). Visible for `::text` — where the golden corpus
   filters blank text on both sides and says so — and for `:empty`, where it cannot be filtered and
   is declared row by row.

### Behaviour corrected by the differential test (`tests/css_golden.rs`)

Measuring against the reference implementation turned up ten places where an earlier decision was
*wrong* rather than merely different. They are now faithful:

* **cssselect's HTML pseudo-classes are namespace-blind.** libxml2's HTML tree namespaces nothing,
  so `name(.)` inside an inline `<svg>` is still `a` — `<svg><a href>` is a `:link` and is
  `:enabled` in Python. `:link`, `:checked`, `:enabled`, `:disabled` and the `ancestor::fieldset`
  walk no longer require the HTML namespace.
* **No namespace prefix is bound**, so `svg|a` is a syntax error, exactly as in Python: cssselect
  translates it to the XPath name test `svg:a` and lxml then refuses to evaluate the unbound
  prefix. `*|a` and a bare `a` still match in every namespace, which is what SVG queries need.
* **The *empty* namespace means "any", too.** `|div` is cssselect's `descendant-or-self::div`, and
  since libxml2 namespaces nothing that matches every `div`, the ones inside `<svg>` included.
  Comparing against html5ever's real namespaces made `|div` select nothing.
* **An unterminated block is rejected**, not recovered: `[href`, `:contains("x` and
  `li:nth-child(2` are errors, as in cssselect. `check_balanced()` runs before the tokenizer, which
  would otherwise close them at EOF. Delimiters inside string literals and after a backslash do not
  count.
* **`:is()` / `:where()` are not forgiving.** `allow_forgiving_selectors()` is off, so
  `div:is(.a, %%%)` fails the whole selector the way this CSS3-era API does in Python.
* **Attribute values compare case-sensitively.** HTML makes selectors match `type`, `rel`, `lang`
  and forty-odd other attributes ASCII-case-insensitively, and `selectors` implements that;
  cssselect emits a plain XPath string test and knows nothing of the list, so `[type="TEXT"]` must
  *not* match `type="text"`. `attr_matches` pins `CaseSensitivity::CaseSensitive`, and the explicit
  `[a="b" i]` / `[a="b" s]` flags are rejected at parse time rather than silently ignored.
* **`:lang()` lower-cases its argument.** cssselect's literal is `value.lower() + '-'`, so
  `:lang(EN)` matches `lang="en-GB"`.
* **`::attr(href)` does not see `xlink:href`.** libxml2 stores the whole prefixed name, so lxml's
  `@href` never finds it; html5ever splits the prefix into the XLink namespace, where a
  namespace-blind lookup would. Attribute lookups for `::attr()`, `:link`, `:checked`, `:enabled`
  and `:lang()` now require the empty attribute namespace.
* **The query node needs a combinator-free selector.** `descendant-or-self::` gives a combinator's
  anchor nowhere to stand when the candidate *is* the query node — ancestors are clipped, and a
  preceding sibling is outside the subtree — so `nav.css("* + *")` no longer returns the `<nav>`.
  `:first-child` still counts the document's real siblings, which is what cssselect compiles it to.
* **A trailing bare `*` is folded into the pseudo-element.** `reference/core/translator.py`
  (`XPathExpr.__str__`) rewrites a `descendant-or-self` step followed by a bare `*`, so
  `div *::text` is `descendant-or-self::div/descendant-or-self::text()` — **every** text node under
  a `div`, its own included — and `div *::attr(class)` reaches the `div`'s own `class`.
  `div > *::text` and `div p::text` are untouched. `SubSelector::subtree_head()` carries the second
  matcher this needs.

---

## Golden corpora

Two, replayed by two integration tests.

### `testdata/css_golden.jsonl` — the differential corpus (`tests/css_golden.rs`)

**343 selectors x 10 HTML pages, plus 729 rows evaluated from a non-root context node — 4159 rows
and 10 134 result entries**, produced by running `the reference implementation`'s own
`Selector(html).css(sel)` and recording what came back. The pages live in `testdata/pages/` so both
engines parse byte-identical input: `shop` (well-formed), `tables` (nested tables, implied
`<tbody>`), `malformed` (unclosed tags, misnesting, duplicate ids, bare attributes), `svg` (inline
SVG + MathML), `unicode` (Cyrillic / CJK / RTL / emoji / entities), `forms` (every form-control
state), `blog` (deep nesting, sibling runs, comments), `quirks` (no doctype, upper-case tags,
attributes and values), `edge` (whitespace-only and comment-only elements, escaped identifiers,
entity-laden attribute values, tab/newline class separators) and `weird` (`<template>`,
`<noscript>`, `<iframe>`, `xlink:href`, `<foreignObject>`, MathML).

Selectors cover type / class / id, every attribute operator (`= ^= $= *= ~= |= !=`, with empty and
odd operands), all four combinators, comma groups, every `:nth-child(an+b)` spelling,
`:nth-of-type`, `:nth-last-*`, `:first`/`:last`/`:only-child`, `:only-of-type`, `:empty`, `:root`,
`:scope`, `:not()`, `:has()`, `:is()`, the HTML state pseudo-classes, `:lang()`, `:contains()`,
`::text`, `::attr()`, escaped identifiers, namespace prefixes, ASCII-case questions on names,
classes, ids and values — and **thirty invalid selectors**, whose Python exception text is recorded
so the port is checked for rejecting what the reference rejects.

**3948/4159 rows reproduce Python exactly (94.9%).** The other 211 are listed in
`EXPECTED_DIVERGENT_SELECTORS` (70 rows: the seven CSS4-vs-cssselect grammar cases of deviation 2)
and `EXPECTED_DIVERGENCES` (141 rows, each with its reason), and the test fails if one of them ever
starts passing, so the table cannot rot into a skip-list. The 141 are 119 rows of
html5ever-vs-libxml2 **tree shape** (an implied `<tbody>`, a `<head>` libxml2 does not synthesise,
the adoption-agency `<b>`/`<i>` reconstruction), 9 rows of `browser-dom`'s stricter
`remove_blank_text` seen through `:empty`, 8 rows of the lxml scoping quirk (deviation 5) and 5 rows
of minimised boolean attributes and SVG attribute-name case.

That "it is only tree shape" claim is *checked*, not asserted: re-running all 343 selectors against
shape-normalised copies of the four affected pages — explicit `<tbody>`, explicit `<head>`,
well-formed nesting, nothing else changed — leaves **1371 of 1372 rows in agreement**, the one
exception being `svg::attr(viewBox)` (deviation 4). A generator-driven random-selector fuzz
(1000+ selectors x 10 pages over several seeds) finds no selector this port *rejects* that Python
accepts, and no node-set difference outside those same buckets.

Node identity is compared as tag + full attribute set + whitespace-stripped string value rather
than as raw outer HTML, because the two serialisers legitimately differ; the row still carries
`html` and lxml's `getpath()`, and the path is asserted too on the pages where the trees agree.

### `testdata/css_selection.jsonl` — the §6.3 translation corpus (`tests/css_selection.rs`)

104 rows generated by `tools/gen_selection_corpus.py` over `testdata/fixture.html`, carrying the
cssselect-derived XPath, the canonical parts and lxml's node set (540 entries), including 14 rows
evaluated from a **non-root context node** to pin the scoping rules.

```sh
path/to/python crates/browser-selectors/tools/gen_selection_corpus.py
```

---

## Extending the matcher

`impl_selector::ElementRef` is the `selectors::Element`. The orphan rule forbids implementing that
trait on `browser_dom::NodeRef` directly, so `ElementRef` is a `Copy` newtype carrying the node
plus an optional scope:

```rust
ElementRef::new(node) -> Option<ElementRef<'_>>                      // unscoped
ElementRef::with_scope(node, Some(scope)) -> Option<ElementRef<'_>>  // ancestors stop at `scope`
```

Both return `None` for a non-element node.
