# browser-rs

An undetectable, adaptive web-scraping library for Rust.

`browser-rs` is one library with four entry points — **parse** a string, **fetch** over HTTP,
**browse** with a real headless Chromium, or **crawl** a whole site — and one `Response` type that
all of them hand back. Whatever the transport, what you query is the same, so code written against
one entry point works against the next unchanged.

* **A browser-grade HTML tree.** `html5ever` parses exactly what a browser would, into an arena
  with O(1) parent/sibling/ancestor walks. CSS selection runs on Servo's `selectors` — the same
  matcher Firefox ships — extended with `::text`, `::attr()` and `:contains()`.
  XPath 1.0 is a complete, from-scratch engine over the same tree, so node identity is shared
  between the CSS and XPath paths and nothing is ever parsed twice.
* **Fetchers that are hard to fingerprint.** TLS/JA3 impersonation via BoringSSL, browser-consistent
  header sets in the right *order*, and a headless-Chromium engine driven over raw CDP so the
  tell-tale automation commands are simply never sent.
* **Selectors that survive a redesign.** The adaptive feature stores a fingerprint of the element
  you matched; when the selector stops matching, it relocates the element by structural similarity
  instead of returning nothing.

No `unsafe`, no system C libraries, no Python runtime. Every crate is `#![forbid(unsafe_code)]`.

> **Status: pre-release.** The library is complete and differentially tested against a Python
> reference across every crate; `docs/DIVERGENCES.md` is the register of every place the two
> deliberately disagree. Nothing is published to crates.io yet.

```toml
[dependencies]
browser-rs = { version = "0.1", features = ["full"] }
```

The crate is `browser-rs`; the library it builds is `browser`, so you write `use browser::Document;`.

---

## The four entry points

Each of the four is a complete, runnable program under
[`crates/browser-rs/examples/`](crates/browser-rs/examples). The snippets below are those programs
with the printing removed.

### 1. Parse

No I/O at all — HTML in, queries out. This is the whole document API, and everything below is built
on it.

```rust
use browser::prelude::*;
use browser::SimilarOpts;

let doc = Document::parse(html);

// CSS, with `::text` and `::attr()` pseudo-elements.
let names:  Vec<String>    = doc.css(".item h3::text")?.texts();
let hrefs:  Vec<String>    = doc.css(".item a")?.attrs("href");
let title:  Option<String> = doc.css("title::text")?.get();

// XPath 1.0 over the same tree — 13 axes, 27 functions, variables.
let cards = doc.xpath("//div[@class='item']")?;
let count = doc.xpath_value("count(//a)", &Default::default())?;

// `find_all`: a builder with a *fixed* evaluation order (tags and attributes,
// then regex, then predicates) regardless of the order you call the methods.
let cells = doc.find_all(
    &FindQuery::new()
        .tag("div").tag("span")
        .attr("class", "stock")      // `class` matches per whitespace-separated token
        .attr("href*", "/p/")        // attribute *keys* are not escaped, on purpose
        .regex(r"In stock: \d+")     // matched against the element's DIRECT text
        .predicate(|e| e.attrib().len() > 2),
)?;

// "Find me the other things that look like this one."
let first = doc.css_first(".item")?.expect("matched");
let siblings = first.find_similar(&SimilarOpts::default());
```

### 2. Fetch over HTTP

`AsyncFetcher` for one-shot requests, `AsyncFetcherSession` when you are making more than one.
A session keeps a cookie jar and a connection pool alive and is worth roughly a round trip per
request. `Fetcher` / `FetcherSession` are the same API without the `.await`, behind the `blocking`
feature.

```rust
use std::time::Duration;

use browser::prelude::*;
use browser::{Impersonate, RedirectMode};

let page: Response = AsyncFetcher::get("https://example.com/")
    .impersonate(Impersonate::pool(["chrome", "firefox"]))  // one drawn per request
    .timeout(Some(Duration::from_secs(30)))
    .retries(Some(3))
    .retry_delay(Duration::from_secs(1))
    .header("Accept-Language", "en-GB,en;q=0.9")
    .follow_redirects(FollowRedirects::Mode(RedirectMode::Safe))
    .send().await?;

// The reply *is* the document. Nothing is parsed until the first query.
let title = page.css("title::text")?.get();
```

Per-request options are a builder rather than a struct because Python's `**kwargs` distinguish
"the caller passed `timeout=None`" from "the caller said nothing". Calling the method **at all** is
the presence signal, and the `Option` inside carries the value:

```rust
let session = AsyncFetcherSession::builder()
    .config(
        FetcherConfig::new()
            .with_impersonate(Impersonate::single("chrome"))
            .with_timeout(Some(Duration::from_secs(30)))
            .with_proxy_rotator(Some(Arc::new(
                browser::http::proxy::ProxyRotator::from_urls([p1, p2])?,
            )))
            .with_retries(Some(2)),
    )
    .build()?;

let a = session.get("https://example.com/1").send().await?;
let b = session.post("https://example.com/2").json(payload).send().await?;
session.close().await?;                       // returns the pooled connections
```

### 3. Browse with real Chrome

For pages whose content arrives after the HTML does. Same `Response` type comes back.

```rust
use browser::prelude::*;
use browser::fetch_once;

let config = BrowserConfig::new()
    .headless(true)
    .disable_resources(true)   // abort images, fonts, media, stylesheets
    .network_idle(true)        // wait for the network to go quiet, not just DOMContentLoaded
    .block_ads(true);

// One shot: launch, fetch, close.
let page = fetch_once(config.clone(), "https://example.com/", FetchParams::default()).await?;

// Or keep the browser, which is what makes the ~1 s cold start worth paying.
let session = BrowserSession::new(config)?;
session.start().await?;
let first  = session.fetch("https://example.com/a", FetchParams::default()).await?;
let second = session.fetch("https://example.com/b", FetchParams::default()).await?;
session.close().await;
```

`BrowserConfig::user_data_dir` is a persistent Chromium profile directory. Point two runs at the
same directory and they are the same browser profile, cookies and local storage included — which is
how a login survives a process restart.

### 4. Crawl

`CrawlSpider` is the ready-made spider: a `LinkExtractor` decides what to follow, a `CrawlRule` says
which callback handles it, and the engine does scheduling, throttling, `robots.txt`, retries and
checkpoints.

```rust
use browser::prelude::*;
use browser::{CallbackId, RunOptions, SpiderRunner, Yielded};

const PRODUCT: CallbackId = CallbackId::from_static("parse_product");

let extractor = LinkExtractor::builder()
    .allow([r"/p/"])
    .deny([r"/admin/"])
    .build()?;

let spider = Arc::new(
    CrawlSpider::builder("catalogue")
        .settings(
            SpiderSettings::default()
                .with_start_urls(["https://shop.example/"])
                .with_allowed_domains(["shop.example"])
                .with_robots_txt_obey(true)
                .with_concurrent_requests(8),
        )
        .rule(CrawlRule::new(extractor).callback(PRODUCT))
        .callback(PRODUCT, |response| {
            let mut item = Item::new();
            item.insert("url".to_owned(), response.url.clone().into());
            item.insert("title".to_owned(), response.css("h1::text").unwrap().get().into());
            vec![Yielded::item(item)]
        })
        .build(),
);

let result = SpiderRunner::new(spider, RunOptions::default())?.run().await?;
result.items.to_jsonl("items.jsonl")?;
```

Callbacks are addressed **by name**, not by closure identity, so an interrupted crawl can resume
from a checkpoint and still know which handler a queued request was for.

For anything the rules cannot express, implement `Spider` yourself — `browser::async_trait` and
`browser::futures` are re-exported so that needs no extra dependency of your own. `SitemapSpider`,
`ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider` and `SiteToMarkdownSpider` (crawl a site, get one
`{url, title, markdown}` item per page) are the other ready-made templates.

---

## Adaptive selectors

The *adaptive* in the tagline. Save a fingerprint of the element you matched; when the site is
redesigned and the selector dies, find the element again by structural similarity.

```rust
use browser::prelude::*;
use browser::{DocumentConfig, storage::{SqliteStorage, StorageSystem}};

let storage: Arc<dyn StorageSystem> =
    Arc::new(SqliteStorage::open("elements.db", Some("https://shop.example/"))?);

let config = DocumentConfig {
    url: "https://shop.example/".to_owned(),
    adaptive: true,
    storage: Some(Arc::clone(&storage)),
    ..DocumentConfig::new()
};

// Today: the element matches, so remember what it looked like.
let doc  = Document::parse_with(html_before, &config);
let card = doc.css_first(".card")?.expect("matched");
doc.save(&card, "product-card")?;

// After the redesign: `.card` is gone, but the element is still there.
let doc = Document::parse_with(html_after, &config);
assert!(doc.css(".card")?.is_empty());

let fingerprint = doc.retrieve("product-card")?.expect("stored");
let found = doc.relocate(&fingerprint, 40);   // ≥40% structural similarity
```

The SQLite schema is byte-compatible with the Python reference's `elements_storage.db`, in both
directions.

`relocate` scores the fingerprint against every element in the tree, and part of that score is a
`SequenceMatcher` comparison of the element's *sibling* list — so its cost grows with how many
siblings the element has, not merely with page size. `benches/relocate.rs` measures the curve,
including the one-sibling step change `difflib`'s autojunk heuristic puts at 200.

---

## Examples and benchmarks

```sh
cargo run   -p browser-rs --example spider          --features spider     # offline: serves its own fixture
cargo run   -p browser-rs --example one_shot_fetch  --features markdown   # network
cargo run   -p browser-rs --example session                               # network
cargo run   -p browser-rs --example browser_fetch   --features browser    # network + Chrome

cargo bench -p browser-rs --benches --all-features
```

Three of the four examples make real requests and say so at the top of the file; `spider` starts a
loopback HTTP server and crawls that, so it needs no network at all. None of them runs during
`cargo test`.

The benches are criterion: `parse` (parse and query a 5000-card catalogue), `relocate` (the adaptive
round trip) and `markdown` (HTML→Markdown and the AI sanitiser).

One headline number, since it is the one the design brief pinned: parsing a document of 5000 nested
`div.item` and then running `css(".item")` over it takes **15.5 ms** — the budget was 0.5 s. Selection
is not where the time goes: the parse is 15.0 ms of that and the selector is 0.28 ms. Measured on an
Apple M5 Pro with `rustc` 1.97.0 under `[profile.bench]`, on a machine that was not idle; repeat runs
varied by tens of percent under load. `docs/DIVERGENCES.md` has the full table and the caveats, and
`cargo bench` on your own machine is the only number worth trusting.

---

## Feature flags

Set on the `browser-rs` façade.

| Feature | Default | Implies | Effect |
|---|:---:|---|---|
| `parser` | ✅ | | The document API: parsing, CSS, XPath, `find_all`. |
| `http` | ✅ | `parser` | The HTTP fetchers. |
| `browser` | | `http` | Headless-Chromium fetchers over CDP. |
| `spider` | | `http` | The crawling engine and the ready-made spiders. |
| `markdown` | | `parser` | `Document::markdown()` and the AI sanitiser. |
| `adaptive` | | `parser` | Adaptive selector storage. Pulls a bundled SQLite. |
| `blocking` | | `http` | The synchronous `Fetcher` / `FetcherSession`. |
| `full` | | all of the above | |

Three build-time notes:

* `browser-http` enables its `impersonate` feature by default, which builds `wreq` on BoringSSL.
  That needs **cmake and a C++ toolchain**. Turn it off with `default-features = false` if you only
  want the parser.
* `adaptive` vendors SQLite via `rusqlite/bundled`, so there is no system dependency — but the C
  compile is real, and it is why the feature is opt-in.
* The `Implies` column costs nothing to compile. `markdown`, `adaptive` and `blocking` each already
  force their member crate into the dependency graph; naming `parser` / `http` alongside is what
  makes the *re-exports* appear. Without it, `--no-default-features --features markdown` built
  cleanly and exposed no `Document` at all.

---

## Crates

The workspace is 16 crates. Most users depend only on the `browser-rs` façade, which re-exports a
curated surface from the first fourteen and owns the feature flags.

| Crate | What it owns |
|---|---|
| [`browser-rs`](crates/browser-rs) | The façade: a curated re-export surface, a `prelude`, an aggregating `error::Error`, and the feature flags. |
| [`browser-core`](crates/browser-core) | Shared vocabulary with zero I/O: text helpers, HTML entities, ordered attribute maps, `SequenceMatcher`, element fingerprints. |
| [`browser-dom`](crates/browser-dom) | The tree: `html5ever` into an `ego-tree` arena, plus the post-parse passes that reproduce lxml's flags. |
| [`browser-selectors`](crates/browser-selectors) | Native CSS matching on Servo's `selectors`, with `::text`, `::attr()` and `:contains()`. |
| [`browser-xpath`](crates/browser-xpath) | A complete XPath 1.0 engine over our own tree — 13 axes, 27 functions, variables. |
| [`browser-storage`](crates/browser-storage) | Adaptive persistence in SQLite, schema-compatible with the reference's `elements_storage.db`. |
| [`browser-markdown`](crates/browser-markdown) | A faithful `markdownify` port, plus noise stripping and AI sanitisation. |
| [`browser-parser`](crates/browser-parser) | The public document API: `Document`, `Node`, `find_all`, `find_similar`, adaptive relocation. |
| [`browser-fingerprint`](crates/browser-fingerprint) | Impersonation profiles, synthesised header sets, default user agents, status phrases, ad domains. |
| [`browser-http`](crates/browser-http) | The HTTP fetchers: impersonation, the `safe` redirect guard, retry policy, sessions. |
| [`browser-cdp`](crates/browser-cdp) | Headless Chromium over raw CDP: `networkidle`, request interception, the Cloudflare solver. |
| [`browser-spider`](crates/browser-spider) | The crawling engine: scheduler, sessions, stats, throttling, robots.txt, checkpoints. |
| [`browser-templates`](crates/browser-templates) | Ready-made spiders: link extraction, crawl rules, sitemaps, Shopify, feeds, site-to-markdown. |
| [`browser-cli`](crates/browser-cli) | The `brs` binary — one-shot fetches, extraction to markdown/HTML/text, and `uncurl`. |
| [`browser-server`](crates/browser-server) | The `brs-serve` binary: REST, MCP and an operator dashboard over one engine. |
| [`browser-mcp`](crates/browser-mcp) | The `brs-mcp` binary: the same operations over MCP stdio. |

Dependency edges point downward; there are no cycles. Read it as layers — everything on a line
depends only on lines above it.

```text
  browser-core                                          every crate depends on it

  browser-dom          browser-storage    browser-fingerprint

  browser-selectors    browser-xpath      browser-markdown        (all three on browser-dom)

  browser-parser                          dom + selectors + xpath; markdown / storage by feature

  browser-http                            parser + fingerprint

  browser-cdp                             http + fingerprint

  browser-spider                          http + parser; cdp by feature

  browser-templates                       spider + parser; markdown by feature

  browser-rs                              the façade over all of the above

  browser-cli (brs)    browser-server (brs-serve)    browser-mcp (brs-mcp)
```

`browser-parser` reaches `browser-markdown` and `browser-storage` only through the `markdown` and
`adaptive` features, and `browser-spider` reaches `browser-cdp` only through `browser`.

---

## The command line

```sh
brs extract get https://example.com/ page.md          # fetch and convert
brs extract fetch https://example.com/ page.html      # …through real Chrome
brs uncurl 'curl https://example.com -H "x: y"'       # a curl command line, translated
brs install                                           # fetch a Chromium to drive
```

## The server

`brs-serve` hosts one engine behind three surfaces on one port (default 8080): a REST API at
`/api/*`, an MCP streamable-HTTP endpoint at `/mcp` for AI agents, and an operator dashboard at `/`.
`brs-mcp` is the same operations over MCP stdio. Sessions are named persistent browser profiles, so
a human can log in once through the dashboard and an agent stays authenticated afterwards.
Authentication is a bearer token from `BROWSER_RS_API_TOKEN`; with it unset the server binds
loopback only. There is a Dockerfile and a compose file at the repository root;
[`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md) covers running it and
[`crates/browser-server`](crates/browser-server) is the full contract.

---

## Divergences

[`docs/DIVERGENCES.md`](docs/DIVERGENCES.md) is the register of every intentional behavioural
difference between this port and the Python reference it was modelled on — what diverges, why, and
which test asserts it. Nothing there is an accident, and each entry is pinned by a test that fails
if the divergence silently disappears.

## Development

```sh
cargo build   --workspace --all-features
BROWSER_RS_SKIP_BROWSER_TESTS=1 cargo test --workspace --all-features
cargo clippy  --workspace --all-targets --all-features -- -D warnings
cargo fmt     --all
```

The toolchain is pinned in `rust-toolchain.toml` (1.97.0). Every third-party version lives in the
root `[workspace.dependencies]` table; **member crates must never write a version literal** — use
`foo.workspace = true`. Golden corpora shared across crates live in `testdata/`. Browser tests
honour `BROWSER_RS_SKIP_BROWSER_TESTS`; without it they need a Chrome on `PATH`.

[`docs/AGENTS.md`](docs/AGENTS.md) is the working contract for this repository.

## License

BSD 3-Clause with a commercial-use restriction — see [`LICENSE`](LICENSE).

Personal, academic, research, and open source use is unrestricted under the usual BSD terms.
Commercial use is also free **if the work using browser-rs publishes its own complete source under
an OSI-approved open source license**. Commercial use inside a closed-source product, service, or
internal system needs a separate written commercial license — contact
[Himanshu](https://github.com/Himasnhu-AT) &lt;hyattherate2005@gmail.com&gt; to arrange one.

This is a source-available license, not an OSI-approved open source license.
