# `browser-parser` — API cheat sheet

The public document API of the port: Python's `Selector` / `Selectors`, rebuilt as
[`Document`], [`Node`] and [`Nodes`]. Everything downstream (`browser-http`'s `Response`, the
spider, the templates, the CLI) codes against this file.

`lib.rs` is owned by the scaffolding, so there are **no crate-root re-exports yet** — reach through
the module path. Unit 20 hoists them into the façade.

```rust
use browser_parser::config::{
    AllTextOpts, ByRegexOpts, ByTextOpts, CssOpts, DocumentConfig, SimilarOpts,
};
use browser_parser::document::{Body, Document};
use browser_parser::error::{ParseError, Result};
use browser_parser::find::{escape_css_string, FindQuery};
use browser_parser::node::{Node, TextKind};
use browser_parser::nodes::Nodes;
use browser_parser::adaptive::{element_to_fingerprint, relocate_among};
use browser_parser::generate::general_selection;
```

Everything here is **synchronous** and has no `tokio` dependency (`ARCHITECTURE.md` §5.1).
`Document` is `Send + Sync`; `Node<'a>` and `Nodes<'a>` borrow it.

---

## 30-second orientation

```rust
use browser_parser::document::Document;
use browser_parser::find::FindQuery;

let doc = Document::parse("<div id=p><a href='/1'>one</a><a href='/2'>two</a></div>");

doc.css("a::text")?.texts();          // ["one", "two"]
doc.css("a")?.attrs("href");          // ["/1", "/2"]        (no ::attr() needed)
doc.css_first("a")?;                  // Option<Node>
doc.xpath("//a[1]")?;                 // Nodes
doc.find_all(&FindQuery::new().tag("a").attr("href*", "/1"))?;
doc.get_all_text();                   // "one\ntwo"
doc.css("a")?[0].css_path();          // "#p > a"
# Ok::<(), browser_parser::error::ParseError>(())
```

---

## `Document` — the owner

```rust
pub struct Document;                                  // Send + Sync + Debug

// construction
fn parse(html: &str) -> Document;                     // Python's defaults, infallible
fn parse_with(html: &str, config: &DocumentConfig) -> Document;
fn parse_bytes(bytes: &[u8], config: &DocumentConfig) -> Document;   // NOT trimmed
fn from_dom(dom: browser_dom::Document, config: &DocumentConfig) -> Document;
fn parse_options(config: &DocumentConfig) -> browser_dom::ParseOptions;

// accessors
fn url(&self) -> &str;               fn encoding(&self) -> &str;
fn body(&self) -> &Body;             fn dom(&self) -> &browser_dom::Document;
fn keep_comments(&self) -> bool;     fn keep_cdata(&self) -> bool;   fn huge_tree(&self) -> bool;
fn html(&self) -> String;            fn pretty(&self) -> String;
fn urljoin(&self, relative: &str) -> String;          // infallible, like Python's

// nodes
fn root(&self) -> Node<'_>;                           // `<html>` — Python's `_root`
fn root_node(&self) -> Node<'_>;                      // the document node, XPath's `/`
fn wrap(&self, node: browser_dom::NodeRef<'_>) -> Node<'_>;
fn elements(&self) -> impl Iterator<Item = Node<'_>>;

// selection — every one delegates to `root()`
fn css(&self, selector: &str) -> Result<Nodes<'_>>;
fn css_with(&self, selector: &str, opts: &CssOpts) -> Result<Nodes<'_>>;
fn css_first(&self, selector: &str) -> Result<Option<Node<'_>>>;
fn xpath(&self, expression: &str) -> Result<Nodes<'_>>;
fn xpath_with(&self, expression: &str, opts: &CssOpts, vars: &Variables) -> Result<Nodes<'_>>;
fn xpath_value(&self, expression: &str, vars: &Variables) -> Result<Value<XNode<'_>>>;
fn find_all(&self, query: &FindQuery<'_>) -> Result<Nodes<'_>>;
fn find(&self, query: &FindQuery<'_>) -> Result<Option<Node<'_>>>;
fn find_by_text(&self, text: &str, opts: &ByTextOpts) -> Nodes<'_>;
fn find_by_regex(&self, pattern: RegexSpec<'_>, opts: &ByRegexOpts) -> Result<Nodes<'_>>;
fn find_similar(&self, opts: &SimilarOpts) -> Nodes<'_>;
fn get_all_text(&self) -> String;
fn get_all_text_with(&self, opts: &AllTextOpts) -> String;
fn json(&self) -> Result<serde_json::Value>;

// adaptive  (`save`/`retrieve`/`storage` need the `adaptive` feature to do anything)
fn adaptive_enabled(&self) -> bool;   fn has_storage(&self) -> bool;
fn storage(&self) -> Option<&Arc<dyn StorageSystem>>;                    // feature = "adaptive"
fn save(&self, node: &Node<'_>, identifier: &str) -> Result<()>;
fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>>;
fn relocate(&self, fingerprint: &ElementFingerprint, percentage: u8) -> Nodes<'_>;

// markdown  (feature = "markdown")
fn markdown(&self) -> String;
fn markdown_with(&self, css_selector: Option<&str>, main_content_only: bool) -> Result<String>;
```

`Body` is `Text(String) | Bytes(Vec<u8>)` with `as_str() -> Cow<str>`, `as_bytes()`, `is_empty()`.

### `DocumentConfig` — Python's selector config

| field | default | Python |
|---|---|---|
| `url` | `""` | `url` |
| `encoding` | `"utf-8"` | `encoding` |
| `keep_comments` | `false` | `remove_comments = !keep_comments` |
| `keep_cdata` | `false` | `strip_cdata = !keep_cdata` |
| `huge_tree` | `true` | accepted and **ignored** — no libxml2 |
| `adaptive` | `false` | the master switch |
| `adaptive_domain` | `None` | overrides `url` as the storage key; `""` means unset |
| `storage` | `None` | replaces `_storage` / `storage` / `storage_args` (feature `adaptive`) |

All seven fetcher `parser_keywords` are covered. Builder setters are chainable; `.storage(..)` also
turns `adaptive` on. `effective_url()` is `adaptive_domain or url`.

---

## `Node<'a>` — Python's dual-natured `Selector`

```rust
pub enum Node<'a> {
    Element { doc: &'a Document, node: NodeRef<'a> },
    Text    { doc: &'a Document, owner: NodeRef<'a>, kind: TextKind, value: Cow<'a, str> },
}
pub enum TextKind { Text, Tail, Attribute(String) }
```

`Clone + PartialEq + Eq`, `Debug` = Python's `__repr__`, `Display` = `get()`.
Two handles for the same underlying node compare equal, whichever engine produced them.

```rust
// construction — the seams from the other crates
fn element(doc, node: NodeRef<'a>) -> Node<'a>;
fn from_text_node(doc, node: NodeRef<'a>) -> Node<'a>;      // lxml's text/tail rule
fn from_attribute(doc, owner, name: &str, value: &'a str) -> Node<'a>;
fn from_match(doc, m: &browser_selectors::Match<'a>) -> Node<'a>;
fn from_xnode(doc, n: browser_xpath::XNode<'a>) -> Node<'a>;

// identity
fn doc(&self) -> &'a Document;
fn is_text_node(&self) -> bool;                 // Python's `_is_text_node`
fn text_kind(&self) -> Option<&TextKind>;
fn element_ref(&self) -> NodeRef<'a>;           // the element, or a string node's owner
fn as_node_ref(&self) -> Option<NodeRef<'a>>;   // None for a string node

// properties
fn tag(&self) -> &'a str;                fn text(&self) -> &str;
fn attrib(&self) -> AttributeMap;        fn attr(&self, name: &str) -> Option<&'a str>;
fn get_attr(&self, name: &str) -> Result<&'a str>;      // `selector[name]`
fn contains_attr(&self, name: &str) -> bool;            // `name in selector`
fn has_class(&self, class_name: &str) -> bool;
fn html_content(&self) -> String;        fn prettify(&self) -> String;
fn get(&self) -> String;                 fn getall(&self) -> Vec<String>;
fn body(&self) -> &Body;                 fn urljoin(&self, relative: &str) -> String;
fn json(&self) -> Result<serde_json::Value>;
fn re(&self, pattern: RegexSpec<'_>, o: ReOptions) -> Result<Vec<String>>;
fn re_first(&self, pattern: RegexSpec<'_>, o: ReOptions) -> Result<Option<String>>;

// navigation
fn parent(&self) -> Option<Node<'a>>;    fn children(&self) -> Nodes<'a>;
fn below_elements(&self) -> Nodes<'a>;   fn siblings(&self) -> Nodes<'a>;
fn next(&self) -> Option<Node<'a>>;      fn previous(&self) -> Option<Node<'a>>;
fn ancestors(&self) -> impl Iterator<Item = Node<'a>>;   // `iterancestors()`
fn find_ancestor(&self, p: impl Fn(&Node<'a>) -> bool) -> Option<Node<'a>>;
fn path(&self) -> Nodes<'a>;

// selection — same set as `Document`, plus
fn find_similar_default(&self) -> Nodes<'a>;

// selector generation
fn css_path(&self) -> String;            fn css_path_full(&self) -> String;
fn xpath_path(&self) -> String;          fn xpath_path_full(&self) -> String;

// adaptive
fn save(&self, identifier: &str) -> Result<()>;
fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>>;
fn relocate(&self, fingerprint: &ElementFingerprint, percentage: u8) -> Nodes<'a>;
fn fingerprint(&self) -> ElementFingerprint;
```

### The text-node degradation table (`parser.md` §2.1), in full

| member | on a string node |
|---|---|
| `tag` | `"#text"` |
| `text` | the string value |
| `attrib` | empty map |
| `get_attr` | `Err(TextNodeHasNoAttributes)` |
| `contains_attr`, `has_class` | `false` |
| `html_content`, `prettify`, `get`, `Display` | the string value |
| `body` | empty |
| `get_all_text` | the string value |
| `below_elements`, `children`, `css`, `xpath`, `find_all`, `find_similar`, `find_by_text`, `find_by_regex` | empty `Nodes` |
| `ancestors` | yields nothing |
| `next`, `previous` | `None` |
| `parent` | **not guarded** — the owning element |
| `siblings` | **not guarded** — the owner's children (Python writes it in terms of `parent`) |
| `css_path` &c. | `""` |
| `Debug` | `<text='…'>` |

A `::attr()` result is a string node too, with `TextKind::Attribute(name)`.
A `::text` result that is a *tail* reports its **owner** as the element it trails, which is lxml's
`getparent()` for such a string — not the tree parent.

---

## `Nodes<'a>` — Python's `Selectors`

`Deref<Target = [Node<'a>]>`, so `len`, `is_empty`, `first`, `last`, `iter`, indexing and slicing
come from the slice. `IntoIterator` (owned and by reference), `FromIterator`, `From<Vec<_>>`.

```rust
fn new() / from_vec(Vec<Node>) / into_vec() / from_matches(doc, Vec<Match>);
fn push(&mut self, Node);          fn extend(&mut self, impl IntoIterator<Item = Node>);
fn length(&self) -> usize;         fn at(&self, index: usize) -> Option<&Node<'a>>;

fn css(&self, &str) -> Result<Nodes>;       fn css_with(&self, &str, &CssOpts) -> Result<Nodes>;
fn xpath(&self, &str) -> Result<Nodes>;     fn xpath_with(&self, &str, &CssOpts, &Variables) -> Result<Nodes>;
fn re(&self, RegexSpec, ReOptions) -> Result<Vec<String>>;
fn re_first(&self, RegexSpec, ReOptions) -> Result<Option<String>>;
fn search(&self, impl Fn(&Node) -> bool) -> Option<Node>;
fn filter(&self, impl Fn(&Node) -> bool) -> Nodes;
fn get(&self) -> Option<String>;   fn get_or(&self, default: &str) -> String;
fn getall(&self) -> Vec<String>;
fn texts(&self) -> Vec<String>;    fn all_texts(&self) -> Vec<String>;
fn htmls(&self) -> Vec<String>;    fn attrs(&self, name: &str) -> Vec<String>;
```

> **`get()` shadows the slice's `get(index)`** — deliberately, because Python's `Selectors.get()`
> takes no index and returns a *string*. Use [`Nodes::at`] for the index lookup.

`Nodes::css`/`xpath` force `adaptive = false`, because Python's `Selectors.css`/`xpath` have no
`adaptive` parameter.

---

## Option structs — every Python keyword argument

```rust
CssOpts     { identifier: Option<String>, adaptive: bool, auto_save: bool, percentage: u8 }  // None,false,false,40
AllTextOpts { separator: String, strip: bool, ignore_tags: Vec<String>, valid_values: bool }  // "\n",false,["script","style"],true
SimilarOpts { threshold: f64, ignore_attributes: Vec<String>, match_text: bool }              // 0.2,["href","src"],false
ByTextOpts  { first_match: bool, partial: bool, case_sensitive: bool, clean_match: bool }     // true,false,false,true
ByRegexOpts { first_match: bool, case_sensitive: bool, clean_match: bool }                    // true,false,true
```

All implement `Default` (= Python's defaults) and chainable setters. `CssOpts` doubles as the
options type for `xpath_with`; XPath's `**kwargs` are a separate `browser_xpath::Variables`.

---

## `FindQuery` — `find_all(*args, **kwargs)`

```rust
FindQuery::new()
    .tag("div").tags(["span", "p"])
    .attr("class", "stock")        // -> [class~="stock"] per whitespace token
    .attr("href*", "/p/")          // keys are NOT escaped, on purpose
    .class("a b").id("main")       // sugar for .attr("class"/"id", ..)
    .regex(r"In stock: \d+")       // against the element's DIRECT text
    .regex_spec(RegexSpec::Compiled(&compiled))
    .predicate(|e| e.attrib().len() > 2)
```

* **Fixed evaluation order**, whatever order you call the setters in:
  tags × attributes → regexes → predicates.
* No tag/attribute part ⇒ the search space is `below_elements`.
* `selector()` exposes the generated CSS (`None` when it would be bare `*`).
* `escape_css_string(v)` is `_escape_css_string`: `"` → `\"`, LF → `\A `, CR → `\D `, FF → `\C `,
  backslashes untouched.
* An entirely empty query is `ParseError::EmptyFindQuery` — Python's `TypeError`.
* `class_=` / `for_=` are gone: Rust has no reserved-word problem.
* `inspect.signature` arity checking is gone: the compiler does it.

---

## Errors

```rust
pub enum ParseError {                      // #[non_exhaustive], thiserror
    NeedsContent,                          // ValueError, verbatim
    CssSyntax(String),                     // already "Invalid CSS selector '{sel}': {inner}"
    XPathSyntax(String),                   // already "Invalid XPath selector: {expr}"
    NotANodeSet { expr, actual },          // ours: a scalar where a node-set was wanted
    AdaptiveDisabled,                      // RuntimeError, verbatim
    Storage(StorageError),                 // feature = "adaptive"
    Regex(RegexError),
    Json(serde_json::Error),
    TextNodeHasNoAttributes,               // TypeError, verbatim
    MissingAttribute(String),              // KeyError
    EmptyFindQuery,                        // TypeError, verbatim
    Url(url::ParseError),
}
```

Three Python errors cannot occur here and are absent by design: the `content` type check (the type
system), and the two storage `ValueError`s about `lru_cache` wrapping and mixin inheritance (the
`StorageSystem` trait bound).

---

## The adaptive layer

```rust
// browser_parser::adaptive
const AUTO_SAVE_IGNORED: &str;   // the exact Python warning, verbatim
const ADAPTIVE_IGNORED:  &str;
fn element_to_fingerprint(el: NodeRef<'_>) -> ElementFingerprint;   // _StorageTools.element_to_dict
fn clean_attributes(el: NodeRef<'_>) -> AttributeDict;
fn element_path(el: NodeRef<'_>) -> Vec<String>;
fn relocate_among(fp: &ElementFingerprint, candidates: Vec<NodeRef>, percentage: u8) -> Vec<NodeRef>;
fn below_threshold_warning(percentage: u8, top_score: f64) -> String;
```

`css_with` / `xpath_with` implement `parser.md` §3.5's branch exactly:

```text
elements = run()
if elements:  auto_save && enabled -> save(elements[0], id);  auto_save && !enabled -> warn
              return elements
elif enabled: adaptive -> retrieve(id) -> relocate(fp, percentage)
                          (auto_save && !relocated.is_empty()) -> save(relocated[0], id)
              return relocated-or-empty
else:         adaptive -> warn ADAPTIVE_IGNORED   elif auto_save -> warn AUTO_SAVE_IGNORED
              return empty
```

Only the **first** element is auto-saved — Python's documented "Known issue", reproduced.
`relocate` returns every node tied at the best score, or empty (never `None`) below the threshold,
and logs `below_threshold_warning` — with the score formatted the Python way (`74.91`, `100.0`).
Scoring runs under `rayon` above 64 candidates; ties are collected, not raced.

The comma-splitting path in `css_with` is taken only when the document has adaptive **enabled** and
the selector text contains a comma, exactly as Python decides it. On that path results are
concatenated per part and **not** de-duplicated (`css("p, p")` returns each `p` twice).

---

## Behaviour you can rely on

| Python / lxml | here |
|---|---|
| `content.strip().replace("\x00","") or "<html/>"` | `Document::parse` (via `browser_dom::prepare_str`) |
| bytes body NUL-stripped, **not** trimmed | `Document::parse_bytes` |
| `Selector.body` is the original argument; children get `""` | `Node::body` is non-empty only on `Document::root()` |
| `get_all_text` attributes a tail to `parent.getparent()` | one lookup: the text node's tree parent |
| `find_by_text`/`find_by_regex` candidates | `.//*[normalize-space(text())]` — the first *child text node*, XPath whitespace only |
| `find_by_text(..., first_match=True)` with no match | empty `Nodes`, never `None` |
| `find_similar` denominator | `max(len(original), len(candidate))` |
| `find_similar` with an empty `ignore_attributes` | the **raw** attrib map on both sides |
| short CSS path stops at the nearest `id` | `#id`; short XPath is `//*[@id='x']/…` |
| full path stops at `<html>` | leftmost segment is `body` |
| the document root | `""` (CSS) / `"//"` (XPath) |
| `round(x, 2)` half-to-even | `browser_core::round_half_even` |
| `@lru_cache(256)` on `css_to_xpath` | compiled `SelectorGroup` / `XPath` caches, 256 entries each |

---

## Divergences from Python (deliberate)

1. **`relocate`'s `selector_type` flag is gone.** Python returns raw `HtmlElement`s unless asked for
   `Selectors`; there is only one node type here, so `relocate` always returns `Nodes`.
2. **`xpath()` rejects a scalar** with `ParseError::NotANodeSet`. Python does not guard it and ends
   up iterating a string per character. `parser.md` §3.5 explicitly permits the rejection; use
   `xpath_value` when a `count()` / `string()` result is what you want.
3. **Comments and processing instructions from an XPath result are `Node::Element`**, and `tag()`
   answers `"#comment"` / `"#processing-instruction"`. lxml models them as `HtmlElement`
   subclasses whose `.tag` is a *function object*, which stringifies to
   `<built-in function Comment>`.
4. **No pickling error.** Python's `__getstate__` raises `TypeError`; a `Node` is simply not
   `Serialize`, so the case cannot arise.
5. **Tag iteration in `FindQuery` is ordered** (`IndexSet`), where Python iterates a `set`. Results
   are in document order either way — the union XPath sorts them — so this only makes the
   *generated selector* deterministic. `ARCHITECTURE.md` §11.7 signs it off.
6. **`urljoin` is infallible** and returns `relative` unchanged when the base is absent or
   unparsable, which is what Python does. `ARCHITECTURE.md` §3 sketched a `Result`.
7. **The default adaptive database is `./elements_storage.db`** (or `$BROWSER_RS_ADAPTIVE_DB`), not
   `<package dir>/elements_storage.db` — a Rust crate has no writable package directory. Failing to
   open it warns and leaves the document without storage rather than poisoning the parse.
8. **`Node::attrib()` returns an owned `AttributeMap`** instead of a cached borrow. Python memoises
   `tag` / `text` / `attrib` per selector because attribute access there is expensive; here `tag`
   and `text` are already `&str` straight out of the arena, and only `attrib` allocates. Use
   `attr(name)` when you want one value and no allocation.
9. **`::text` / `::attr()` are optional**, not mandatory: `Nodes::texts()` and `Nodes::attrs(name)`
   do the same job without changing the selector.

Everything in `docs/DIVERGENCES.md` about the *tree* (implied `<tbody>`, the synthesised `<head>`,
the adoption agency, minimised boolean attributes, SVG name case, end-of-document whitespace)
applies here too and accounts for **every one** of the 130 declared corpus rows below.

---

## The differential corpora

Two of them, both recorded from `the reference implementation` itself and both regenerable
from the repository (`tools/corpus/u10/`, which has its own README):

```sh
/path/to/pyref/bin/python tools/corpus/u10/gen_parser_corpus.py   # 3488 rows
/path/to/pyref/bin/python tools/corpus/u10/gen_api_corpus.py      # 7008 rows
```

### `testdata/parser_golden.jsonl` — the algorithms

**3488 rows, 3358 reproduce (96.3 %)**, replayed by
`document::golden::the_reference_corpus_replays`. Fourteen pages, 232 receivers
addressed as `(css selector, index)`, and per receiver: the four generated
selectors, `get_all_text` under six option sets, the property/navigation block,
`find_similar` under four option sets and the fingerprint JSON — plus per page 18
`find_all` queries, 9 `find_by_text` and 6 `find_by_regex` queries, and 10
whole-document `relocate` rows that carry Python's **entire per-element score
table**. Those ten pin `element_to_dict`, `similarity_score`, `dict_diff`, the
`difflib` port and `round_half_even` against CPython in one shot.

### `testdata/parser_api_golden.jsonl` — the public API

**7008 rows, 6675 reproduce (95.2 %)**, replayed by
`tests/parser_golden.rs::the_public_api_corpus_replays`. Seventeen pages — the
fourteen above plus three *mutated* copies (`blog_v2`, `shop_v2`, `similar_v2`)
built for the adaptive scenarios — and per page:

| family | what is compared |
|---|---|
| `css` (52 queries) | result count, node identities, `.text` of every result, `.get()`, `.get(default)`, `.getall()`, and the syntax-error rows |
| `xpath` (25 queries) | the same, plus `//text()`, `//@href`, `//node()`, `//comment()` and the rejection rows |
| text-node degradation (8 probes) | every row of the table above: `tag`, `text`, `str`, `repr`, `attrib`, `get`/`getall`/`html_content`, `has_class`, `in`, `children`, `siblings`, `below_elements`, `parent`, `next`, `previous`, `iterancestors`, `path`, the two generated selectors, `get_all_text`, a nested `css`/`xpath`, `body` |
| `find_all` / `find` (62 specs each) | tag / iterable-of-tags / attribute dict / keyword arguments / the `class_`+`for_` map / dict-and-kwargs-together / un-escaped key patterns (`href*`, `href$`, `href^`, `class~`, `lang\|`) / compiled regex / named predicates / the empty-query `TypeError`, and the CSS-value escaping edge cases: embedded `"`, `\n`, `\r`, `\f`, backslashes, spaces, dots, digits-first ids, `@`, `/`, and non-ASCII in both keys and values |
| `find_similar` | five option sets over every receiver — 1725 rows |
| `find_by_text` (18) / `find_by_regex` (12) | `first_match` both ways, partial vs exact, case sensitivity, `clean_match`, the empty query, Unicode and emoji, and the no-match asymmetry |
| selector generation | the four properties for 345 elements |
| navigation | `tag`/`text`/`attrib`/`children`/`siblings`/`below_elements`/`next`/`previous`/`parent`/`iterancestors`/`path` for the same 345 |
| `Nodes` | `css`, `xpath`, `re`, `re_first`, `get`, `getall`, `search`, `filter`, `first`, `last`, `length` |
| `get_all_text` | five option sets on the whole document |
| **adaptive, end to end** | 23 scenarios: `css(sel, auto_save=True)` on page A through a real `SqliteStorage`, then `css(sel, adaptive=True)` on a mutated copy — comparing the stored fingerprint, the fingerprint read back out of SQLite, the relocated elements **and the similarity score of every element on the mutated page** |

**All 23 adaptive rows reproduce, score tables included**, across four
relocations that had to survive a renamed wrapper, reordered siblings, changed
classes, an inserted nesting level and an `id` turned into a `data-` attribute —
plus the below-threshold cases at 90 % and 99 % that must return *empty* rather
than raise.

### The 333 that do not

Each is listed one by one in `tests/support/divergences.rs` with a written
reason, and **the test fails if any of them ever starts passing**:

| rows | cause |
|---:|---|
| 70 | libxml2 gives a valueless boolean attribute (`checked`) the attribute *name* as its value where html5ever gives `""` — which then changes every `SequenceMatcher` ratio computed over it |
| 68 | html5ever's synthesised `<head>` |
| 62 | html5ever's implied `<tbody>` |
| 39 | SVG/MathML camel case and the `xlink:`/`xmlns` prefix split |
| 34 | libxml2 refuses to compile an XPath string holding a raw control character, so `find_all(..., {"title": "form\ffeed"})` raises `ValueError` in Python and simply matches here |
| 21 | the whitespace between `</body>` and `</html>` |
| 19 | html5ever cannot tell `data-x` from `data-x=""` |
| 18 | the HTML5 adoption agency rebuilding `<b>`/`<i>` after the stray `</p>` |
| 2 | element counts on `malformed.html`'s `<html>`/`<body>`, where three of the above compound |

Every one is a `browser-dom` parse-shape or libxml2-convention difference
already accepted by `ARCHITECTURE.md` §11.6 and recorded in
`docs/DIVERGENCES.md`. **Not one** is a `css` / `xpath` / `find_all` /
`find_similar` / `find_by_text` / `get_all_text` / selector-generation /
fingerprint / relocation *algorithm* difference: where the two trees agree, so do
the answers.

### Debugging a replay

`tests/parser_golden.rs` writes its complete mismatch list, one row per line, to
the path named by `$PARSER_GOLDEN_DUMP` instead of the first forty:

```sh
PARSER_GOLDEN_DUMP=/tmp/fail.txt cargo test -p browser-parser --all-features --test parser_golden
```

### Bugs the API corpus caught

1. **`Node::next`/`previous`/`children` leaked the `<!DOCTYPE>`.** libxml2 keeps
   the doctype out of the element tree entirely (it lives in `docinfo`), so
   `<html>.getprevious()` is `None` in Python; html5ever models it as a real
   child of the document node and it was coming back as a sibling.
2. **`browser-dom` serialised attributes the html5ever way, not libxml2's.**
   `.get()` / `.html_content()` now pick single quotes for a value containing `"`
   and no `'`, escape `<`/`>` inside attribute values, percent-escape `href` /
   `src` / `action` (and `name` on `<a>`), and write U+00A0 through verbatim
   instead of as `&nbsp;` — all four established against libxml2 2.14.6.
3. **`browser-xpath` matched attributes by local name alone**, so `//a/@href`
   reached an `xlink:href` that lxml only answers to as `@xlink:href`. Both the
   general evaluator and the single-walk fast path now require the null
   namespace on the attribute axis.
