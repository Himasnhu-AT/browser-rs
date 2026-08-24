# `browser-markdown` — the markdownify port + AI sanitiser

Ports two things from the reference implementation 0.4.15:

* **`markdownify` 1.2 at its defaults** — the serializer `Convertor._convert_to_markdown`
  calls with no options at all.
* **`reference/core/shell.py::Convertor`'s cleaning layer** — `_strip_noise_tags` and
  `_sanitize_for_ai`.

Everything is synchronous, allocation-light and operates on live `browser_dom` nodes; nothing
is re-serialised or re-parsed.

---

## 30-second orientation

```rust
use browser_dom::Document;
use browser_markdown::{clean_for_ai, to_markdown_default};

let doc = Document::parse("<h1>Title</h1><p>Body</p><script>evil()</script>");
let clean = clean_for_ai(&doc);                     // strip noise, then sanitise
let md = to_markdown_default(clean.root());         // markdownify(body)

assert_eq!(md, "Title\n=====\n\nBody");
```

## Public API

```rust
// convert.rs
pub fn to_markdown(node: NodeRef<'_>, opts: &MarkdownOptions) -> String;
pub fn to_markdown_default(node: NodeRef<'_>) -> String;
pub fn html_to_markdown(html: &str) -> String;      // parse + convert, = _convert_to_markdown

pub struct MarkdownOptions { /* every markdownify option, pre-filled */ }
impl Default for MarkdownOptions;                   // == the reference implementation's configuration
impl MarkdownOptions {
    fn validate(&self) -> Result<(), MarkdownError>;
    fn should_convert_tag(&self, tag: &str) -> bool;
}
pub enum HeadingStyle { Atx, AtxClosed, Underlined }         // default Underlined
pub enum NewlineStyle { Spaces, Backslash }                  // default Spaces
pub enum DocumentStrip { Lstrip, Rstrip, Strip }             // default Some(Strip)
pub enum PreStrip { Strip, StripOne }                        // default Some(Strip)

pub struct MarkdownConverter;                       // validated + reusable
impl MarkdownConverter {
    fn new(opts: MarkdownOptions) -> Result<Self, MarkdownError>;
    fn options(&self) -> &MarkdownOptions;
    fn convert(&self, node: NodeRef<'_>) -> String;
    fn convert_html(&self, html: &str) -> String;
}

// sanitize.rs
pub fn strip_noise_tags(doc: &Document) -> Document;   // script/style/noscript/svg
pub fn sanitize_for_ai(doc: &Document) -> Document;    // hidden nodes + ZWC + control chars
pub fn clean_for_ai(doc: &Document) -> Document;       // both, in Response.markdown()'s order
pub fn is_hidden_element(node: NodeRef<'_>) -> bool;
pub fn scrub_text(text: &str) -> Cow<'_, str>;
pub const NOISE_TAGS: [&str; 4];
pub const HIDDEN_STYLE_SUBSTRINGS: [&str; 12];
pub const ZERO_WIDTH_CHARS: [char; 6];

// error.rs
pub enum MarkdownError { StripAndConvert }             // #[non_exhaustive], thiserror
```

`escape`, `tables` and `traversal` stay behind their modules; they hold the ported primitives
(`chomp`, `py_strip`, `map_lines`, `colspan`, `ParentTags`, `Walker`, …) for anyone who needs
markdownify's exact string semantics.

---

## Building `Response.markdown()` on top of this

`toolbelt.md` §2.2 pins the algorithm. Note the ordering: **both cleaning passes run always,
and before the CSS selector.**

```rust
let scope = main_content_only.then(|| doc.body().map(|b| b.id())).flatten();
let clean = clean_for_ai(&doc);
let page = scope.and_then(|id| clean.node(id))
                .or_else(|| clean.root_element())
                .unwrap_or_else(|| clean.root());
let markdown: String = match css_selector {
    None => to_markdown_default(page),
    // all matches, concatenated with no separator
    Some(sel) => select(page, sel).map(to_markdown_default).collect(),
};
```

**Arena ids survive both cleaning passes**, which is what makes the `body` lookup above work.
A node that was pruned keeps its id but is unreachable from the root.

**`to_markdown(node)` treats `node` as a fresh document.** Nothing above or beside it is
visible — no ancestor `<ul>` inflates a bullet's nesting depth, no sibling `<p>` changes a
list's trailing newline. That is exactly what Python gets by re-parsing `element.html_content`,
which is why per-element conversion composes.

---

## Fidelity

`tests/` compares against the real libraries, not against hand-written expectations:

| test | what it pins |
|---|---|
| `golden.rs` | `markdownify(html)` byte-for-byte over 10 hand fixtures × 2 parser-flag sets, plus 800 generated documents |
| `options_golden.rs` | 22 non-default option sets × 10 fixtures |
| `sanitize_golden.rs` | `Convertor._sanitize_for_ai(_strip_noise_tags(...))` — surviving elements and characters — over the fixtures and all 800 generated documents |
| `response_markdown.rs` | the `tests/fetchers/test_response_markdown.py` expectations |
| `markdown_golden.rs` | 279 hand-written cases (270 fragments, 9 full pages) run through the **real** the reference implementation 0.4.15 / markdownify 1.2, each recorded with four Python answers |

`markdown_golden.rs` is the adversarial arm and it works in two tiers. Every row stores both
`markdownify(raw_html)` and `markdownify(<the HTML our own tree serialises to>)`. The second
comparison removes both parsers from the picture, so **a mismatch there is always a converter
bug** and no row is allowed to opt out of it; a mismatch only against the raw-HTML answer is a
tree difference and has to name the specific parser rule that causes it. At the time of writing
the same-tree tier is exact on all 279 cases, on 1857 sub-element conversions taken from them,
and on 2000 randomly generated documents.

The other golden inputs are the HTML *our* tree serialises to (`examples/normalize.rs`), so the
comparison measures the converter and never `html.parser` versus `html5ever`. To regenerate:

```sh
python3 crates/browser-markdown/tools/gen_fuzz.py 800 777
cargo run -p browser-markdown --example normalize
<python-with-markdownify-and-browser-rs> crates/browser-markdown/tools/gen_golden.py
```

## Known deviations

1. **The parser underneath differs.** markdownify uses BeautifulSoup + `html.parser`, which does
   no implied-tag insertion; `html5ever` synthesises `<tbody>`, relocates foster-parented
   content and closes tags per the HTML5 spec. `convert_tr` reaches the same answer for both
   table shapes (`ARCHITECTURE.md` §11.6 accepts the rest).
2. **Whitespace-only text nodes come from `browser-dom`.** Its `remove_blank_text` (on by
   default, matching the reference implementation's parser flags) deletes every whitespace-only text node outside
   `pre`/`textarea`/…, while libxml2's `areBlanks()` keeps some mixed-content whitespace. Parse
   with `ParseOptions::new().remove_blank_text(false)` for the browser answer;
   `testdata/normalized-ws` pins that path too.
3. **the reference implementation's own pipeline round-trips through lxml's serialiser** — it feeds
   `clean.html_content` back to BeautifulSoup. lxml writes an empty `<li></li>` as a bare
   `<li>`, which `html.parser` then nests the next `<li>` inside, so the reference implementation turns
   `<ul><li></li><li>x</li></ul>` into `* * x`. We convert the tree directly and produce `* x`.
4. **`<ol start>` follows Python's `int()`, `colspan` does not.** `start` is read with
   arbitrary precision (`<ol start="99999999999999999999">` renders exactly that) and accepts
   any Unicode decimal digit, matching `int()`. Where a value is *numeric* but has no decimal
   value — `²`, `Ⅰ`, `༪` — `int()` raises and markdownify propagates the exception; we fall back
   to `1` instead of panicking. `colspan` is still read as ASCII digits only.
5. **`code_language_callback` is not ported** — it is a Python callable and the reference implementation never sets
   it. `code_language` (the static string) is.
6. **`expand_tabs` is not ported** inside `wrap`. It runs before `replace_whitespace` in
   `textwrap`, but `process_text` has already collapsed every tab by the time `convert_p` wraps,
   so it is unreachable. `replace_whitespace` *is* ported, because a vertical tab or form feed
   can reach `wrap` — markdownify's collapse class does not cover them.
7. **`<noscript>` is parsed as markup, not raw text.** `browser-dom` sets html5ever's
   `scripting_enabled: false` so `<noscript>` children become elements the way libxml2 and
   `html.parser` build them. One shape still differs: a `<noscript>` that starts the document
   is placed in `<head>`, where the *in head noscript* insertion mode forbids `<p>` and moves
   the paragraph to `<body>` — libxml2 keeps the whole construct in `<body>`, so
   `_strip_noise_tags` deletes more there. `<noscript>` inside `<body>`, which is the shape
   real pages use, matches exactly.
8. **A newline straight after `<pre>` is eaten by the tokenizer.** HTML5 requires it; libxml2
   does not implement it, so `Selector('<pre>\nx</pre>').text` is `"\nx"` in Python and `"x"`
   here. Invisible at the reference implementation's defaults, because `strip_pre = Strip` removes leading
   newlines anyway — the difference only surfaces under `strip_pre: None` / `StripOne` /
   `convert`, which the reference implementation never sets. Pinned by
   `convert::tests::pre_drops_one_leading_newline_at_the_tokenizer`.
9. **Processing instructions are ignored.** Python routes them through the *text* branch (they
   subclass `NavigableString`); the HTML tokenizer never emits one, so we drop them like
   comments.
