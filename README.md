# browser-rs

A Rust port of [the reference implementation](https://github.com/Himasnhu-AT/browser-rs) — an undetectable, adaptive
web-scraping library.

`browser-rs` gives you the three things a real scraper needs, in one workspace:

* **A browser-grade HTML tree.** `html5ever` parses exactly what a browser would, into an arena
  with O(1) parent/sibling/ancestor walks. CSS selection runs on Servo's `selectors` — the same
  matcher Firefox ships — extended with the reference implementation's `::text`, `::attr()` and `:contains()`.
  XPath 1.0 is a complete, from-scratch engine over the same tree, so node identity is shared
  between the CSS and XPath paths and nothing is ever parsed twice.
* **Fetchers that are hard to fingerprint.** TLS/JA3 impersonation via BoringSSL, browser-consistent
  header sets in the right *order*, and a headless-Chromium engine driven over raw CDP so the
  tell-tale automation commands are simply never sent.
* **Selectors that survive a redesign.** The adaptive feature stores a fingerprint of the element
  you matched; when the selector stops matching, it relocates the element by structural similarity
  instead of returning nothing.

No `unsafe`, no system C libraries, no Python runtime. Every crate is `#![forbid(unsafe_code)]`.

> **Status: scaffolding.** The workspace, dependency graph and module layout are in place; the
> subsystems are being filled in unit by unit. See `docs/DIVERGENCES.md` for where behaviour
> deliberately differs from the Python reference.

## Quickstart

```toml
[dependencies]
browser-rs = { version = "0.1", features = ["blocking"] }
```

Blocking — the "just scrape a page" path:

```rust
use browser::prelude::*;

let page = Fetcher::get("https://example.com").send()?;

let titles: Vec<String>    = page.css("h3 a::text")?.texts();
let hrefs:  Vec<String>    = page.css("h3 a")?.attrs("href");
let price:  Option<String> = page.css("#price")?.re_first(r"[\d.]+", Default::default())?;
```

Async, with per-request overrides. Python's `**kwargs` become a builder that preserves
presence-versus-`None` semantics — calling a method *at all* is the "key is present" signal, and
the `Option` inside distinguishes `timeout=30` from an explicit `timeout=None`:

```rust
let page = AsyncFetcher::get("https://example.com")
    .impersonate(Impersonate::pool(["chrome", "firefox"]))  // random pick per request
    .timeout(None)                                          // explicit "no timeout"
    .header("x-api-key", key)
    .stealthy_headers(false)
    .retries(5)
    .send().await?;
```

Sessions keep a cookie jar and a connection pool alive, and are roughly 10× faster than one-shot
requests:

```rust
let session = FetcherSession::builder()
    .impersonate("chrome")
    .proxy_rotator(ProxyRotator::new(vec![p1, p2], Cyclic)?)
    .retries(3)
    .retry_delay(Duration::from_secs(1))
    .build().await?;

let a = session.get("https://example.com/1").send().await?;
let b = session.post("https://example.com/2").json(&payload).send().await?;
session.close().await?;
```

Querying is a builder with a *fixed* evaluation order (tags and attributes, then regex, then
predicates) regardless of the order you call the methods:

```rust
let cells = page.find_all(
    &FindQuery::new()
        .tag("div").tag("span")
        .attr("class", "stock")     // `class` matches per whitespace-separated token
        .attr("href*", "/p/")       // attribute *keys* are not escaped, on purpose
        .regex(r"In stock: \d+")    // matched against the element's DIRECT text
        .predicate(|e| e.attrib().len() > 2),
)?;
```

## Crates

The workspace is 14 crates. Most users depend only on the `browser-rs` façade.

| Crate | What it owns |
|---|---|
| [`browser-rs`](crates/browser-rs) | The façade: a curated re-export surface, a `prelude`, and the feature flags. |
| [`browser-cli`](crates/browser-cli) | The `browser-rs` binary — one-shot fetches, extraction to markdown/HTML/text, and `uncurl`. |
| [`browser-core`](crates/browser-core) | Shared vocabulary with zero I/O: text helpers, HTML entities, ordered attribute maps, `SequenceMatcher`, element fingerprints. |
| [`browser-dom`](crates/browser-dom) | The tree: `html5ever` into an `ego-tree` arena, plus the post-parse passes that reproduce lxml's flags. |
| [`browser-selectors`](crates/browser-selectors) | Native CSS matching on Servo's `selectors`, with `::text`, `::attr()` and `:contains()`. |
| [`browser-xpath`](crates/browser-xpath) | A complete XPath 1.0 engine over our own tree — 13 axes, 27 functions, variables. |
| [`browser-storage`](crates/browser-storage) | Adaptive persistence in SQLite, schema-compatible with Python's `elements_storage.db`. |
| [`browser-markdown`](crates/browser-markdown) | A faithful `markdownify` port, plus noise stripping and AI sanitisation. |
| [`browser-parser`](crates/browser-parser) | The public document API: `Document`, `Node`, `find_all`, `find_similar`, adaptive relocation. |
| [`browser-fingerprint`](crates/browser-fingerprint) | Impersonation profiles, synthesised header sets, default user agents, status phrases, ad domains. |
| [`browser-http`](crates/browser-http) | The HTTP fetchers: impersonation, the `safe` redirect guard, retry policy, sessions. |
| [`browser-cdp`](crates/browser-cdp) | Headless Chromium over raw CDP: `networkidle`, request interception, the Cloudflare solver. |
| [`browser-spider`](crates/browser-spider) | The crawling engine: scheduler, sessions, stats, throttling, robots.txt, checkpoints. |
| [`browser-templates`](crates/browser-templates) | Ready-made spiders: link extraction, crawl rules, sitemaps, Shopify, feeds, site-to-markdown. |

Dependency edges point downward; there are no cycles.

```
                        browser-core  <-- everything
                             |    |
        +--------------------+    +--------------+----------------+
   browser-dom                      browser-storage   browser-fingerprint
     |    |    |                             |                   |       |
     |    |    +-- browser-markdown        |                   |       |
     |    +------- browser-xpath           |                   |       |
     +------------ browser-selectors       |                   |       |
                          |                  |                   |       |
                   browser-parser <--------+                   |       |
                          |                                      |       |
                   browser-http <------------------------------+       |
                     |       |                                          |
                     |  browser-cdp <-----------------------------+
                     |       |
              browser-spider
                     |
              browser-templates
                     |
                 browser-rs  <--  browser-cli (bin `browser-rs`)
```

`browser-parser` reaches `browser-markdown` and `browser-storage` only
through the `markdown` and `adaptive` features, and `browser-spider` reaches
`browser-cdp` only through `browser`.

## Feature flags

Set on the `browser-rs` façade.

| Feature | Default | Effect |
|---|:---:|---|
| `parser` | ✅ | The document API: parsing, CSS, XPath, `find_all`. |
| `http` | ✅ | The HTTP fetchers. Implies `parser`. |
| `browser` | | Headless-Chromium fetchers over CDP. Implies `http`. |
| `spider` | | The crawling engine and the ready-made spiders. Implies `http`. |
| `markdown` | | `Document::markdown()` and the AI sanitiser (Python's `rag` extra). |
| `adaptive` | | Adaptive selector storage. Pulls a bundled SQLite. |
| `blocking` | | The synchronous `Fetcher` / `FetcherSession` façade. |
| `full` | | `browser` + `spider` + `markdown` + `adaptive` + `blocking`. |

Two build-time notes:

* `browser-http` enables its `impersonate` feature by default, which builds `wreq` on BoringSSL.
  That needs **cmake and a C++ toolchain**. Turn it off with `default-features = false` if you only
  want the parser.
* `adaptive` vendors SQLite via `rusqlite/bundled`, so there is no system dependency — but the C
  compile is real, and it is why the feature is opt-in.

## Development

```sh
cargo build   --workspace
cargo test    --workspace --all-features
cargo clippy  --workspace --all-targets --all-features -- -D warnings
cargo fmt     --all
cargo deny    check
```

The toolchain is pinned in `rust-toolchain.toml` (1.97.0). Every third-party version lives in the
root `[workspace.dependencies]` table; **member crates must never write a version literal** — use
`foo.workspace = true`. Golden corpora shared across crates live in `testdata/`.

## License

MIT.
