# `browser-templates` — the ready-made spiders

What work unit 18 built, for whoever writes unit 20's re-export surface.
Everything below is implemented, tested, and differentially checked against
`the reference implementation`; nothing here is a sketch.

Modules: `links`, `crawl`, `sitemap`, `shopify`, `feed`, `site_to_markdown`,
`gzip`, `error`.

The crate codes against `crates/browser-spider/API.md` (the `Spider` trait,
`CrawlResponse`, `Request`, `Yielded`, `CallbackId`) and
`crates/browser-parser/API.md` (`Nodes::get`/`texts`/`attrs`). It adds no new
transport and no new response type.

---

## 30-second orientation

```rust
use std::sync::Arc;

use browser_spider::request::{CallbackId, Item};
use browser_spider::spider::{RunOptions, Spider, SpiderRunner, SpiderSettings, Yielded};
use browser_templates::crawl::{CrawlRule, CrawlSpider};
use browser_templates::links::LinkExtractor;

const PRODUCT: CallbackId = CallbackId::from_static("parse_product");

let spider = CrawlSpider::builder("shop")
    .settings(SpiderSettings::default().with_start_urls(["https://shop.test/"]))
    .rule(CrawlRule::new(LinkExtractor::builder().allow([r"/p/\d+$"]).build()?).callback(PRODUCT))
    .callback(PRODUCT, |response| {
        let mut item = Item::new();
        item.insert("name".into(), response.css("h1::text").ok().and_then(|n| n.get()).into());
        vec![Yielded::item(item)]
    })
    .build();

let result = SpiderRunner::new(Arc::new(spider) as Arc<dyn Spider>, RunOptions::default())?
    .run()
    .await?;
```

Four things to internalise before writing anything against this crate:

1. **Python's classes become values, not traits.** `CrawlSpider`,
   `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider` and
   `SiteToMarkdownSpider` are concrete structs with builders. Rust has no
   inheritance, and "subclass and override `rules()`" has no translation; the
   rule list and the callbacks are data.
2. **Rule dispatch is also a free function.** `crawl::requests_to_follow(&[CrawlRule], &CrawlResponse)`
   is the whole of `CrawlSpider.parse`, callable from any `impl Spider`. That is
   how a user composes rules into their own spider without `super()`, and it is
   the shape `crates/browser-rs/tests/integration.rs::crawl_spider_seam` was
   written in.
3. **Callbacks are named, not bound.** `CallbackId` survives a checkpoint;
   a closure does not. Every spider here that has more than one callback
   overrides **both** `Spider::dispatch` and `Spider::has_callback` — the second
   is not optional, or a restored checkpoint rewrites every named callback to
   `parse`.
4. **`links::Response` is `browser_http::response::Response`.** This crate has
   no direct `browser-http` dependency, so the type is named through
   `CrawlResponse`'s `Deref` target. It is the same type; `&CrawlResponse`
   coerces to `&Response` at every call site.

---

## `links` — the URL discovery primitive

```rust
pub type Response = <CrawlResponse as std::ops::Deref>::Target;   // unit 12's Response
pub type ProcessFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub const HTML5_SPACE: [char; 5];             // ' ', '\t', '\n', '\r', '\x0c'
pub const VALID_SCHEMES: [&str; 3];           // http, https, file
pub const IGNORED_EXTENSIONS: [&str; 91];     // sorted; "tar.gz" is one entry
pub const DEFAULT_TAGS: [&str; 2];            // a, area
pub const DEFAULT_ATTRS: [&str; 1];           // href

pub fn strip_html5_whitespace(value: &str) -> &str;
pub fn url_extensions(url: &str) -> Vec<String>;          // ("tar.gz", "gz")
pub fn hostname_of(url: &str) -> String;                  // no port, no userinfo
pub fn safe_url_string(url: &str, encoding: &str) -> Result<String, SafeUrlError>;

pub struct LinkExtractor;                     // Clone is cheap (everything is Arc)
impl LinkExtractor {
    pub fn builder() -> LinkExtractorBuilder;
    pub fn extract(&self, response: &Response) -> Vec<String>;
    pub fn matches(&self, url: &str) -> bool;
    pub fn url_passes(&self, url: &str) -> bool;
    pub fn allow_patterns(&self) -> &[Regex];
    pub fn deny_patterns(&self) -> &[Regex];
    pub fn deny_extensions(&self) -> &HashSet<String>;
}

pub struct LinkExtractorBuilder;              // Python's 13 kwargs, one method each
// allow · deny · allow_domains · deny_domains · restrict_css · restrict_xpath
// tags · attrs · canonicalize · strip · keep_fragment · deny_extensions · process
impl LinkExtractorBuilder { pub fn build(self) -> Result<LinkExtractor>; }
```

Things that will bite:

* **`strip_html5_whitespace` is not `str::trim`.** Five characters, not the whole
  Unicode `White_Space` property. Using `trim` silently changes which `href`s
  survive.
* **`deny_extensions` unset ≠ `deny_extensions([])`.** Unset is Python's `None`
  (use the 91-entry default); an empty iterable denies **nothing**, which is how
  `tags(["link"])` reaches a stylesheet.
* **The scheme test is `url.split("://")[0]`, not a URL parse.** `mailto:x@y`
  has no `://`, so the whole string is compared and rejected. Do not "improve"
  it.
* **`allow_domains`/`deny_domains` compare the `hostname`** — no port — while
  every other domain check in the crawl core (`Request::domain`, AutoThrottle,
  the robots cache, the engine's offsite filter) compares the **netloc**, port
  included. They are not the same and must not be unified.
* **`extract` falls back to the whole document when the `restrict_*` selectors
  match nothing**, not merely when they are unset. It reads like a bug and it is
  Python's behaviour; the u18 differential caught the port getting it wrong.
* **`regex` is not `re`.** No backreferences and no lookaround; such a pattern
  compiles in Python and is `TemplateError::InvalidPattern` here.

## `crawl` — `CrawlRule`, `requests_to_follow`, `CrawlSpider`

```rust
pub type ProcessRequestFn = Arc<dyn Fn(Request, &CrawlResponse) -> Option<Request> + Send + Sync>;
pub type CallbackFn       = Arc<dyn Fn(&CrawlResponse) -> Vec<Yielded> + Send + Sync>;

pub struct CrawlRule {
    pub link_extractor: LinkExtractor,
    pub callback: Option<CallbackId>,     // None means `parse`
    pub priority: Option<i32>,
    pub process_request: Option<ProcessRequestFn>,
}
impl CrawlRule {
    pub fn new(link_extractor: LinkExtractor) -> Self;
    pub fn callback(self, CallbackId) -> Self;
    pub fn priority(self, i32) -> Self;
    pub fn process_request(self, impl Fn(Request, &CrawlResponse) -> Option<Request>) -> Self;
    pub fn effective_callback(&self) -> CallbackId;   // never None
}

pub fn requests_to_follow(rules: &[CrawlRule], response: &CrawlResponse) -> Result<Vec<Request>>;

pub struct CrawlSpider;                     // impl Spider
impl CrawlSpider {
    pub fn builder(name: impl Into<String>) -> CrawlSpiderBuilder;
    pub fn rules(&self) -> &[CrawlRule];
    pub fn follow_rules(&self, response: &CrawlResponse) -> Result<Vec<Request>>;
}
// builder: settings · rule · rules · callback(CallbackId, F) · on_parse(F) · build
```

* **Every rule sees every response** (unlike `SitemapSpider`, which is
  first-match-wins). A URL two rules match is yielded twice and the scheduler
  dedupes it.
* `priority` is applied **before** `process_request`, so `process_request` can
  override it.
* `on_parse` is Python's "override `parse` and delegate with
  `async for req in super().parse(response)`": its items come first, then the
  rule requests.

## `sitemap` — `SitemapSpider`

```rust
pub const PARSE_SITEMAP: CallbackId;         // "_parse_sitemap"

pub struct SitemapResult { pub urls: Vec<String>, pub sitemaps: Vec<String> }
pub enum UnruledRoute { RouteToParse /* default */, InheritSitemapCallback }

pub fn parse_sitemap(body: &[u8], content_type: Option<&str>, alternate_links: bool)
    -> Result<SitemapResult>;
pub fn robots_sitemaps(body: &[u8], encoding: &str) -> Vec<String>;

pub struct SitemapSpider;                    // impl Spider
impl SitemapSpider {
    pub fn builder(name: impl Into<String>) -> SitemapSpiderBuilder;
    pub fn sitemap_urls(&self) -> &[String];
    pub fn rules(&self) -> &[CrawlRule];
    pub fn dispatch_url(&self, response: &CrawlResponse, url: &str) -> Result<Option<Request>>;
}
// builder: settings · sitemap_urls · sitemap_follow · sitemap_alternate_links
//          · unruled_route · rule · rules · callback · build
```

Seeding from a `robots.txt` URL works: `_parse_sitemap` notices a path ending in
`/robots.txt` and follows the `Sitemap:` directives instead of parsing XML.

`sitemap_urls` is **required**; `start_requests` returns `CrawlError` carrying
Python's ``"`SitemapSpider` needs `sitemap_urls` to be set."`` when it is empty.
`parse` is Python's `NotImplementedError` and yields `Yielded::error`, so a
spider with no rules must either set `unruled_route` (the default routes there)
or register callbacks.

## `shopify` — `ShopifySpider`

```rust
pub const PARSE_COLLECTION: CallbackId;      // "parse_collection"
pub const PAGE_LIMIT: u32 = 250;
pub const DEFAULT_NAME: &str = "shopify";

pub fn remove_tags(text: &str) -> String;    // w3lib's regex, verbatim
pub fn python_title(text: &str) -> String;   // CPython's str.title()
pub fn is_truthy(value: &serde_json::Value) -> bool;

pub struct ShopifySpider;                    // impl Spider
impl ShopifySpider {
    pub fn builder() -> ShopifySpiderBuilder;         // .name · .settings · .target_website
    pub fn target_website(&self) -> &str;
    pub fn collections_url(&self, page: u64) -> String;
    pub fn products_url(&self, handle: &str, page: u64) -> String;
    pub fn product_url(&self, handle: &str, product_handle: &str) -> String;
    pub fn process_product(&self, product: &Value, collection_handle: &str) -> Result<Vec<Item>>;
}
```

The item is exactly Python's twelve keys, in Python's order: `name`, `price`,
`category`, `brand`, `identifier`, `sku`, `stock`, `image_url`, `url`,
`description`, `old_price`, `barcode`. `stock` is `null` when available and `0`
when not — never a quantity.

The URL templates are **https-only**, as Python's are.

## `feed` — `XmlNode`, `XMLFeedSpider`, `CSVFeedSpider`

```rust
pub struct XmlNode {
    pub namespace: Option<String>, pub name: String,
    pub attributes: IndexMap<String, String>,   // "{uri}local" or "local", lxml's keying
    pub text: String,                           // lxml's `.text`: before the FIRST child
    pub children: Vec<XmlNode>,
}
impl XmlNode {
    pub fn get(&self, name: &str) -> Option<&str>;
    pub fn find(&self, name: &str) -> Option<&XmlNode>;
    pub fn find_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode>;
    pub fn findtext(&self, name: &str) -> Option<&str>;   // Some("") for an empty child
    pub fn iter(&self) -> XmlNodeIter<'_>;                // depth-first, INCLUDING self
    pub fn stripped(&self) -> XmlNode;                    // namespaces removed
}
pub fn parse_xml(body: &[u8]) -> Result<XmlNode>;

pub type NodeCallbackFn = Arc<dyn Fn(&CrawlResponse, &XmlNode) -> Vec<Yielded> + Send + Sync>;
pub type RowCallbackFn  = Arc<dyn Fn(&CrawlResponse, &Item)    -> Vec<Yielded> + Send + Sync>;
pub const CSV_RESTKEY: &str = "";            // Python's restkey=None

pub struct XMLFeedSpider;                    // builder: settings · itertag · namespaces · parse_node
impl XMLFeedSpider {
    pub fn itertag(&self) -> &str;
    pub fn wanted_tag(&self) -> Result<(Option<&str>, &str)>;
    pub fn iter_nodes(&self, root: &XmlNode) -> Result<Vec<XmlNode>>;
}

pub struct CSVFeedSpider;                    // builder: settings · delimiter · quote · headers · parse_row
impl CSVFeedSpider {
    pub fn delimiter(&self) -> u8;  pub fn quote(&self) -> u8;
    pub fn read_rows(&self, text: &str) -> Result<Vec<Item>>;
}
```

* `itertag` without a prefix matches **any** namespace; with one it matches only
  the URI that prefix maps to, and an undeclared prefix is
  `TemplateError::UndefinedNamespacePrefix`.
* `XmlNode::iter` includes the root, so an `<item>` document root matches
  `itertag = "item"`.
* Ragged CSV rows: surplus fields land in `CSV_RESTKEY` as an array, missing
  fields become `Value::Null`. Blank lines are skipped.

## `site_to_markdown` — `SiteToMarkdownSpider` (feature `markdown`)

```rust
pub struct SiteToMarkdownSpider;             // impl Spider
impl SiteToMarkdownSpider {
    pub fn builder(name: impl Into<String>) -> SiteToMarkdownSpiderBuilder;
    pub fn pages_converted(&self) -> usize;
    pub fn output_dir(&self) -> Option<&Path>;
    pub fn rules(&self) -> &[CrawlRule];
    pub fn filename_for(&self, url: &str) -> String;   // records the name; suffixes on collision
    pub fn page_item(&self, response: &CrawlResponse) -> Result<Item>;
    pub fn write_item(&self, item: &Item) -> Result<()>;
}
// builder: settings · css_selector · main_content_only · output_dir · max_pages · rules · build
```

`allowed_domains` is **required** (`TemplateError::MissingAllowedDomains`).
The item is `{url, title, markdown}`. `max_pages` caps conversion *and*
following: the last converted page enqueues nothing, and a page fetched after
the cap yields nothing at all. The markdown conversion runs on `tokio`'s
blocking pool.

## `gzip`

```rust
pub const GZIP_MAGIC: [u8; 2];
pub const GUNZIP_MAX_SIZE: usize = 64 * 1024 * 1024;
pub const CHUNK_SIZE: usize = 8192;
pub fn is_gzipped(body: &[u8], content_type: Option<&str>) -> bool;
pub fn decompress<'a>(body: &'a [u8], content_type: Option<&str>) -> Result<Cow<'a, [u8]>>;
```

The size check happens **after** a chunk is appended, so the buffer can reach
`GUNZIP_MAX_SIZE + CHUNK_SIZE - 1` bytes before the error. That is Python's
threshold, and `CHUNK_SIZE` is part of the contract for that reason.

`TemplateError::GzipTruncated` is deliberately distinct from
`TemplateError::Gzip`: Python raises `EOFError` for a truncated member and
`EOFError` is **not** an `OSError`, so it escapes the callers' handler and
reaches the engine's callback error path. `SitemapSpider` and the feed spiders
reproduce that split.

## `error`

One enum, `TemplateError`, with the Python messages reproduced verbatim where a
test pins them: `GzipBomb`, `Gzip`, `GzipTruncated`, `InvalidPattern`,
`UndefinedNamespacePrefix`, `MissingSitemapUrls`, `MissingTargetWebsite`,
`MissingAllowedDomains`, `NotImplemented`, `Xml`, `Csv`, `ShopifyJson`,
`Output`, `Crawl`. `pub type Result<T, E = TemplateError>`.

Three messages interpolate Python's `self.__class__.__name__`, which has no Rust
equivalent; the port interpolates the spider's **name** instead.

---

## Reaching this crate from the `browser-rs` façade

Everything below is re-exported flat from `browser-rs` under the **`spider`**
feature — `LinkExtractor`, `CrawlRule`, `requests_to_follow`, `CrawlSpider`,
`SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider`,
`TemplateError`, the builders, and the free functions
(`strip_html5_whitespace`, `url_extensions`, `safe_url_string`, `hostname_of`,
`remove_tags`, `python_title`, `parse_sitemap`, `parse_xml`, `robots_sitemaps`)
— with `SiteToMarkdownSpider` behind `spider` **and** `markdown`. The module
itself is `browser::templates`, which is where `gzip::decompress` and
`IGNORED_EXTENSIONS` live. `crates/browser-rs/src/lib.rs`'s
`the_ready_made_spiders_are_reachable_flat` names every one of them and fails to
compile if one moves.

## Deliberate deviations from Python

Each is asserted, either by a unit test or by a declared row of
`tests/u18_golden.rs` that fails if it ever starts matching.

1. **A `CrawlRule` always names its callback.** Python writes
   `response.follow(url, callback=rule.callback)`, and `follow` *inherits* the
   originating request's callback when that is `None`. For a textbook
   `CrawlSpider` the two agree; they stop agreeing the moment rules are composed
   into a **named** callback, and then every page reached from a product page is
   parsed as a product. `requests_to_follow` writes
   `Some(rule.effective_callback())`. (`docs/DIVERGENCES.md` §"Wave 5
   integration".)
2. **`SitemapSpider`'s unruled route defaults to `parse`.** Python's
   `_dispatch` inherits `_parse_sitemap` for a URL no rule matched, so the target
   page is fed back into the sitemap parser. `UnruledRoute::InheritSitemapCallback`
   reproduces that exactly, for anyone who wants it.
3. **An empty `tags` or `attrs` yields no links** instead of raising
   `XPathEvalError`. Fourteen declared corpus rows.
4. **Comments and processing instructions are dropped when the XML tree is
   built**, so a `<!-- comment -->` among a `<urlset>`'s children does not crash
   the crawl. Python's `etree.QName(comment.tag)` raises `ValueError`, which
   `_sm_body` does not catch. Two declared corpus rows.
5. **`ShopifySpider`'s `allowed_domains` fallback is deterministic** — the
   lexicographically first entry of a `BTreeSet`, where Python takes an
   arbitrary member of a `set`.
6. **`urlsplit` cannot fail here.** CPython raises
   `ValueError("Invalid IPv6 URL")` for an unbalanced bracket in the netloc, and
   neither `LinkExtractor.matches`, `_url_extensions` nor `_filename_for`
   catches it — one malformed `<loc>` takes the whole callback down.
   `browser_spider::canonicalize::urlparse` is total by the workspace-wide
   decision in `docs/DIVERGENCES.md` §"Unit 15" item 2. `safe_url_string` is
   the exception: it re-derives CPython's bracket, host and port errors itself
   and returns `SafeUrlError`, because Python's caller catches those.
   Twenty-three declared corpus rows.
7. **A document whose parsed root is `None` does not crash.**
   `Selector("<!-- c -->")._root` is `None` in lxml and `extract` reaches
   straight for `scope._root.xpath(...)`, so a page that is only a comment
   raises `AttributeError` in Python under *every* configuration. Twenty-seven
   declared corpus rows.
8. **`quick-xml` is not a validating parser.** lxml rejects an undeclared
   namespace prefix, an unescaped `<` in an attribute value and a redefined
   attribute; `_sm_body` turns each into an empty result plus a warning, losing
   the sitemap. This port reads all three, and reads an undeclared `a:item` as
   the *unprefixed* `item` rather than refusing it. Six declared corpus rows.
   `XMLFeedSpider` inherits the same behaviour through `parse_xml`, where it is
   uncorpused — 1,500 fuzzed feeds agree once every prefix is declared, and the
   1,031 that disagreed before that were all this one class.
9. **A lone `\r` inside an unquoted CSV field is a record terminator**, not
   `_csv.Error("new-line character seen in unquoted field")`. Python's
   `CSVFeedSpider.parse` does not catch that error, so a legacy Mac-line-ending
   feed takes the callback down there and is read here. Two declared corpus rows.
10. **`restrict_css="table"` scopes to a different element.** html5ever
    foster-parents an `<a>` out of a `<table>` per the HTML5 spec; libxml2 keeps
    it inside. `docs/DIVERGENCES.md` T3, accepted workspace-wide in
    `ARCHITECTURE.md` §11.6. Two declared corpus rows, and no well-formed page
    reaches it.
11. **The XML declaration's encoding is ignored.** `quick-xml`'s `encoding`
    feature is not enabled and the dependency set is frozen, so a non-UTF-8 feed
    or sitemap is decoded lossily rather than transcoded.
12. **`safe_url_string` always percent-encodes UTF-8**, where w3lib encodes the
    query and fragment with the *page's* charset first, and IDNA-encodes with
    UTS-46 rather than CPython's IDNA 2003 (they differ on `ß`, `ς`, ZWJ and
    ZWNJ — 66 of 1500 fuzzed URLs, all of them `ß`).
13. ~~**`float()` on a `compare_at_price` string is `f64::from_str`**~~ —
    **closed in wave 6.** `browser_spider::throttle::python_float` is now
    `pub` and `to_float` calls it, so `float("٣.٥")` is `3.5`, `float("1_0")` is
    `10.0` and `float("\u{a0}4.5")` is `4.5` here as they are there. Closing it
    exposed a **bug in `python_float` itself**: it stripped with `str.strip()`'s
    rule, where CPython's numeric parsers strip with C's `Py_ISSPACE` *after*
    `_PyUnicode_TransformDecimalAndSpaceToASCII` has folded the non-ASCII
    whitespace — so a C0 separator (`\x1c`–`\x1f`) is ASCII, survives the fold
    and is refused, even though `str.strip()` removes it. 395 of 6,000 fuzzed
    strings disagreed; both `python_float` and `python_int` are now exact on
    6,000/6,000. Pinned by
    `browser_spider::throttle::tests::the_numeric_strip_is_c_isspace_not_str_strip`
    and `shopify::tests::to_float_is_cpythons_float_not_rusts`.
14. **`python_title` is exact except for a Unicode-version skew.** The three
    tables at the top of `shopify.rs` supply `_PyUnicode_ToTitleFull`,
    `_PyUnicode_IsCased` and `_PyUnicode_IsCaseIgnorable`, so `ß` → `"Ss"`,
    `ǅungla` → `ǅungla` and word-final `Σ` → `ς` all reproduce. What remains is
    that CPython 3.14 and `rustc` 1.97 disagree about the case mapping of 28
    code points and about whether 5 others are cased (`꟎ ꟏ ꟒ ꟔ ꟱` and the
    Medefaidrin block, where `rustc` is ahead). 6 of 8,202 fuzzed strings.
15. **`CSVFeedSpider`'s delimiter and quote are bytes**, so a non-ASCII
    one-character Python delimiter cannot be expressed.
16. **`process_request` may return `None` to drop a request.** Python's type is
    `-> Request` and cannot.
17. **A `write_item` failure is logged, not raised.** Python lets the `OSError`
    out of `on_scraped_item`; here the item still reaches the exporter.
18. **`regex` is not `re`.** An `allow`/`deny` pattern using a backreference or
    lookaround compiles in Python and is `TemplateError::InvalidPattern` here.
19. **Error messages interpolate the spider's `name`** where Python interpolates
    `self.__class__.__name__`; a Rust spider is a value, not a type.

---

## The differential corpus

`tools/corpus/u18/generate.py` produces six files into `testdata/`, replayed by
`tests/u18_golden.rs`. **596 of 670 rows reproduce (89.0%)**, and the 74 that do
not are the declared rows above — each asserted to *keep* differing, so a change
that accidentally fixes one is flagged.

| Corpus | Rows | Reproduced |
|---|---:|---|
| `u18_helpers.jsonl` | 153 | 151 (98.7%) |
| `u18_links.jsonl` | 378 | 317 (83.9%) |
| `u18_sitemap.jsonl` | 48 | 40 (83.3%) |
| `u18_shopify.jsonl` | 49 | 49 (100%) |
| `u18_csv.jsonl` | 26 | 24 (92.3%) |
| `u18_filename.jsonl` | 16 | 15 (93.8%) |

The rate is **lower** than an earlier 481/495 (97.2%) on purpose, and both halves
of the drop are deliberate. The corpus grew by 175 rows, 42 of them added
*because* they do not reproduce. And the `matches` comparison was tightened: it
had been folding "CPython raised" into "the URL did not pass" by forcing both
sides to `null`, which made 18 rows agree by construction. They now differ, and
are declared.

### Out-of-corpus fuzzing

Generated, run against the reference interpreter, diffed, and then reverted —
the counts are what the runs measured, not what the committed corpus contains:

| What | Observations | Agree | The remainder |
|---|---:|---:|---|
| `safe_url_string` | 1,500 | 1,434 | all 66 are the IDNA-2003 `ß` (deviation 12) |
| `_url_extensions` | 1,500 | 1,392 | all 108 are CPython's `urlsplit` raising (deviation 6) |
| `remove_tags` | 1,200 | 1,200 | — |
| `str.title()` | 7,002 | 6,996 | all 6 are the Unicode-version skew (deviation 14) |
| `LinkExtractor` (well-formed pages) | 2,400 | 2,400 | — |
| `LinkExtractor` (malformed markup) | 1,500 | 1,472 | 18 foster parenting, 10 null root |
| `csv.DictReader` (CRLF) | 3,000 | 3,000 | — |
| `csv.DictReader` (lone CR in the alphabet) | 2,000 | 1,355 | all 645 are deviation 9 |
| `SitemapSpider._sm_body` | 1,200 | 1,200 | — |
| `_filename_for` (per URL) | 3,614 | 2,905 | 709 CPython raises (deviation 6) |
| `_process_product` | 1,200 | 1,200 | — |
| `XMLFeedSpider._iter_nodes` + `_strip_namespaces` | 1,500 | 1,500 | — |
| `_decompress` | 2,580 | 2,580 | — |

`tests/loopback_crawl.rs` runs all six spiders to completion through the real
`CrawlerEngine` over a `std::net` fixture server, which is where the robots.txt
gate, the scheduler's de-duplication, `max_pages` and the `output_dir` writer are
checked.

### Mutation testing

Thirty-two deliberate bugs were introduced one at a time and the suite re-run.
One "escape" turned out to be a no-op mutation and was caught once it was
written correctly. **Four real escapes** were found, and all four are pinned:

* `<sitemapindex><sitemap><loc>` did not have its whitespace stripped — no row
  had a padded `<loc>` inside an *index* (only inside a `<urlset>`). Corpus row
  `index-whitespace`.
* an alternate link's `href` did not have its whitespace stripped — no row had
  a padded `xhtml:link`. Corpus row `alternate-whitespace`.
* `SiteToMarkdownSpider::claim_page` could drop its guard between the check and
  the increment and no test noticed, because every crawl ran serially. Pinned by
  `site_to_markdown::tests::max_pages_is_not_a_check_then_increment`, which runs
  sixteen callers against `max_pages(8)` on a four-thread runtime, twenty times.
* `ShopifySpider`'s `collected_ids` could split its `contains` from its `insert`
  for the same reason. Pinned by
  `shopify::tests::concurrent_collections_still_emit_each_variant_once`.

## What is **not** verified

* **No real site.** Every test is loopback. Shopify's actual API, a real
  `sitemap.xml` index and a real RSS feed are all unexercised.
* **`ShopifySpider` end to end.** Its URL templates are `https://`-only, so the
  loopback test drives the two callbacks over the fixture rather than the whole
  engine. The paths the socket saw are asserted; TLS is not.
* **Concurrency, beyond the two counters.** Both shared counters are now
  hammered directly (see the mutation-testing list), but the loopback *crawls*
  still run `concurrent_requests(1)` so their item lists are deterministic.
  Nothing here proves the engine's own interleaving matches Python's — that is
  unit 17's ground, and it is unprovable on a loopback fixture anyway, since
  Python's asyncio and a multi-threaded Tokio runtime schedule differently by
  construction.
* **Non-UTF-8 feeds and sitemaps** (deviation 11) have no corpus coverage at all;
  the distance from lxml was never measured, because it could not be closed
  without a dependency change.
* **`safe_url_string`'s `encoding` parameter** is accepted and ignored
  (deviation 12). Every fuzzed and corpus URL is UTF-8, so nothing can see it.
* **`_checknetloc`.** CPython raises `ValueError` for a non-ASCII netloc that
  NFKC-normalises to contain `/?#@:`. Implementing it needs a Unicode
  normaliser, which this crate does not depend on. No fuzz input reached it, so
  its size is unknown.
* **`process` on a `LinkExtractor`** is unit-tested only; it cannot be
  serialised, so it is absent from the differential corpus, and its interaction
  with `canonicalize` ordering rests on reading the Python.
* **`XmlNode`** models only what the template spiders read: tag, namespace,
  attributes, `.text` and children. No `.tail`, no CDATA/text distinction after
  parsing, no XPath.
* **The gzip bomb cap's exact boundary.** It is tested at
  `GUNZIP_MAX_SIZE + CHUNK_SIZE` (rejected) and at a few kilobytes (accepted);
  the true `> GUNZIP_MAX_SIZE` edge would need a 64 MiB allocation in the test
  process.
* **`SitemapSpider::robots_sitemaps`** rests on unit 16's `RobotsRules`; three
  cases are tested here and protego's `Sitemap:` parsing is not re-derived.
