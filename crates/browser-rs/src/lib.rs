//! **browser-rs** — an undetectable, adaptive web-scraping library for Rust.
//!
//! This is the façade users depend on. It re-exports a curated surface from the
//! member crates and owns the feature flags; it contains no logic of its own.
//! See `ARCHITECTURE.md` §1.13 and §5.4.
//!
//! # Features
//!
//! | Feature | Pulls in | Implies |
//! |---|---|---|
//! | `parser` *(default)* | the document API | |
//! | `http` *(default)* | the HTTP fetchers | `parser` |
//! | `browser` | headless Chromium over CDP | `http` |
//! | `spider` | the crawling engine and the ready-made spiders | `http` |
//! | `markdown` | HTML-to-markdown conversion and AI sanitisation | `parser` |
//! | `adaptive` | adaptive selector storage (bundled SQLite) | `parser` |
//! | `blocking` | the synchronous fetcher façade | `http` |
//! | `full` | all of the above | |
//!
//! The `Implies` column costs nothing to compile — see
//! [the note below](#feature-gating-and-the-gap-that-used-to-be-here).
//!
//! # The two spellings
//!
//! Every member crate is re-exported whole under a short module name, so nothing
//! is ever unreachable:
//!
//! ```
//! # #[cfg(feature = "http")]
//! # fn _demo() {
//! use browser::http::config::Impersonate; // the long spelling
//! use browser::Response; // the curated one
//! # }
//! ```
//!
//! The flat names at the crate root are the ones a scraping program reaches for
//! constantly; anything rarer keeps its module path. [`prelude`] bundles the
//! handful worth glob-importing.
//!
//! # Feature gating, and the gap that used to be here
//!
//! Every re-export below is gated on the **façade's** feature name, not on
//! whether the member crate happens to be in the graph. Those are not the same
//! thing, and for a while they came apart: `markdown` and `adaptive` are
//! written as `browser-parser/markdown` and so on, which pulls `browser-parser`
//! into the graph *without* turning on `feature = "parser"` here, and
//! `blocking` did the same for `browser-http`. `--no-default-features --features
//! markdown` therefore compiled and exposed no [`Document`] at all — the
//! *smaller* surface, never a broken one, but not a surface anyone asked for.
//!
//! It is closed in `Cargo.toml`, not here: `markdown` and `adaptive` now name
//! `"parser"` and `blocking` names `"http"`. That is the only fix available.
//! A `cfg` alias — `#[cfg(any(feature = "parser", feature = "markdown"))]` —
//! looks like it would do the job and does not: [`dom`] and [`xpath`] are
//! separate optional dependencies which only `parser` enables, so under
//! markdown-only they are not *nameable* from this crate however the `cfg`s are
//! written, and [`Document::dom`] would be a method returning a type with no
//! path through `browser::` — exactly the thing the section below says this
//! crate does not ship.
//!
//! The implication costs nothing. `browser-parser` depends on `browser-dom`,
//! `browser-selectors` and `browser-xpath` unconditionally, and `browser-http`
//! depends on `browser-parser` unconditionally, so those crates were already
//! being compiled in every build that reached this gap; what the implication
//! adds is the re-export path, not the code.
//!
//! The opposite direction — a `cfg` that is on while its crate is absent, which
//! would fail to compile — cannot happen: `parser`, `http`, `browser` and
//! `spider` all name `dep:` explicitly.
//!
//! # Nothing in the public API is unnameable
//!
//! That is a property this crate maintains deliberately, and wave 6 had to add
//! four dependencies to restore it. If a method on a re-exported type hands back
//! a value, the value's *type* must have a path through `browser::` — the
//! alternative is a user who can call the method and cannot store what it
//! returns. So [`dom`], [`xpath`], [`storage`] and [`markdown`] are here, each
//! behind the feature that makes it reachable:
//!
//! * [`Document::dom`] returns a [`dom::Document`];
//! * [`Document::xpath_value`] returns an [`xpath::Value`];
//! * [`storage::SqliteStorage`] is the only [`storage::StorageSystem`] there is,
//!   and `adaptive` is useless without one;
//! * [`markdown::strip_noise_tags`] and [`markdown::sanitize_for_ai`] are the
//!   two passes `Convertor` runs, and having no path to them cost
//!   `browser-cli` 113 differential corpus rows until wave 6.
//!
//! The same rule applies to *implementing* a trait, not only to naming one.
//! [`Spider`] is declared `#[async_trait]` and its callbacks return a
//! [`CallbackStream`], which is a `futures` type alias — so [`macro@async_trait`] and
//! [`futures`] are re-exported too, under `spider`. A surface that makes a trait
//! nameable but not implementable looks complete and is not.
//!
//! The one member crate still absent is `browser-selectors`: nothing in any
//! public signature mentions it. `browser-fingerprint`'s types arrive through
//! [`http::config`] re-exports.
//!
//! `crates/browser-rs/tests/integration.rs::unit_20_facade_seam` is the check —
//! four end-to-end journeys that name nothing but `browser::`. It is the only
//! module in that file which does not reach past the façade, and that restraint
//! is its whole assertion.
//!
//! # One error type, when you want one
//!
//! Each member crate raises its own `thiserror` enum, and a function that only
//! talks to one of them is better off naming that enum. [`error::Error`] is the
//! aggregate for everything else: one `From` per member crate, so an
//! application that fetches *and* parses *and* writes a file still needs one
//! `Result` and one `?`. It is not re-exported flat — see the module docs.
//!
//! # Runnable programs
//!
//! `crates/browser-rs/examples/` has one complete program per entry point:
//! `one_shot_fetch`, `session`, `browser_fetch` and `spider`. `spider` serves
//! its own loopback fixture and needs no network; the other three make real
//! requests and say so at the top of the file. None of them runs during
//! `cargo test`.
//!
//! `crates/browser-rs/benches/` has the criterion benches — `parse`, `relocate`
//! and `markdown`. `cargo bench -p browser-rs --benches --all-features`.
//!
//! [`Document::dom`]: parser::document::Document::dom
//! [`Document::xpath_value`]: parser::document::Document::xpath_value

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod error;
pub mod prelude;

// ---------------------------------------------------------------------------
// The feature-gap guards.
//
// `markdown`, `adaptive` and `blocking` each force a member crate into the
// graph through a `dep/feature` entry, and each therefore *has* to name the
// façade feature that exposes it — see the module docs above. A test cannot
// check this: the configuration that regressed is
// `--no-default-features --features markdown`, and no test binary in this repo
// is ever built under it. A `compile_error!` is, and it fires in exactly that
// build and in no other.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "markdown", not(feature = "parser")))]
compile_error!(
    "`markdown` pulls `browser-parser` into the graph and must therefore imply the \
     façade's `parser` feature; without it this crate exposes no `Document`. \
     Add \"parser\" to the `markdown` entry in Cargo.toml."
);

#[cfg(all(feature = "adaptive", not(feature = "parser")))]
compile_error!(
    "`adaptive` pulls `browser-parser` into the graph and must therefore imply the \
     façade's `parser` feature; without it there is no `Document` to relocate on. \
     Add \"parser\" to the `adaptive` entry in Cargo.toml."
);

#[cfg(all(feature = "blocking", not(feature = "http")))]
compile_error!(
    "`blocking` pulls `browser-http` into the graph and must therefore imply the \
     façade's `http` feature; without it there is no `Response` for `Fetcher` to \
     return. Add \"http\" to the `blocking` entry in Cargo.toml."
);

// ---------------------------------------------------------------------------
// The member crates, re-exported whole.
// ---------------------------------------------------------------------------

/// The shared vocabulary crate: text helpers, entity replacement, attribute
/// maps, `difflib`, element fingerprints and the CLI's cookie/header parsers.
///
/// Not feature-gated — every other crate depends on it.
pub use browser_core as core;

/// The document API: `Document`, `Node`, `Nodes` and the query option structs.
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser as parser;

/// The HTTP fetchers: configuration, the `wreq` driver, `Response`, sessions.
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use browser_http as http;

/// The headless-Chromium fetchers, driven over raw CDP.
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser_cdp as browser;

/// The crawling engine: `Spider`, `CrawlerEngine`, the scheduler and the
/// support services (throttle, robots, cache, checkpoints).
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider as spider;

/// The ready-made spiders and link extraction: `LinkExtractor`, `CrawlSpider`,
/// `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider` and
/// `SiteToMarkdownSpider`.
///
/// The spiders themselves are re-exported flat below; this module is how you
/// reach the rest — `templates::gzip::decompress`, the builder types, the
/// `IGNORED_EXTENSIONS` table.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates as templates;

/// The HTML tree underneath [`Document`]: `NodeRef`, `ParseOptions`, the
/// libxml2-compatible serialiser and the edit operations.
///
/// [`Document::dom`](parser::document::Document::dom) hands back one of these.
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_dom as dom;

/// The hand-written XPath 1.0 engine.
///
/// [`Document::xpath_value`](parser::document::Document::xpath_value) returns a
/// [`xpath::Value`], which is how a scalar expression (`count(//a)`,
/// `string(//title)`) comes back. `Document::xpath` itself returns [`Nodes`] and
/// needs nothing from here.
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_xpath as xpath;

/// The markdownify port and the two AI-sanitisation passes.
///
/// [`Document::markdown`](parser::document::Document::markdown) and
/// [`Document::clean_for_ai`](parser::document::Document::clean_for_ai) are the
/// convenient spellings; this module is what they are made of, and it is what a
/// caller holding raw HTML rather than a page wants.
#[cfg(feature = "markdown")]
#[cfg_attr(docsrs, doc(cfg(feature = "markdown")))]
pub use browser_markdown as markdown;

/// Adaptive selector storage: the [`storage::StorageSystem`] trait and the
/// SQLite backend whose schema is byte-identical to Python's.
///
/// A database written by the reference implementation 0.4.15 reads here and vice versa.
#[cfg(feature = "adaptive")]
#[cfg_attr(docsrs, doc(cfg(feature = "adaptive")))]
pub use browser_storage as storage;

// ---------------------------------------------------------------------------
// Curated flat re-exports — `browser-core`.
// ---------------------------------------------------------------------------

pub use browser_core::attrs::AttributeMap;
pub use browser_core::entities::{replace_entities, replace_entities_with};
pub use browser_core::error::{CookieParseError, CoreError, HeaderParseError};
pub use browser_core::fingerprint::{dict_diff, similarity_score, ElementFingerprint};
pub use browser_core::numeric::round_half_even;
pub use browser_core::shell::{parse_cookie_string, parse_header_lines, CookieMap};
pub use browser_core::text::{clean_spaces, ReOptions, RegexSpec, TextExt, TextsExt};
pub use browser_core::types::{
    ExtractionType, FollowRedirects, HttpMethod, Override, PageLoadState, Proxy, RedirectMode,
    SameSite, SetCookieParam, WaitState,
};

// ---------------------------------------------------------------------------
// `browser-parser`.
// ---------------------------------------------------------------------------

/// The query-option structs: `Document::css_with`, `get_all_text_with`,
/// `find_similar`, `find_by_text` and `find_by_regex` each take one.
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser::config::{
    AllTextOpts, ByRegexOpts, ByTextOpts, CssOpts, DocumentConfig, SimilarOpts,
};
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser::document::Document;
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser::error::ParseError;
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser::find::FindQuery;
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser::node::Node;
#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use browser_parser::nodes::Nodes;

// ---------------------------------------------------------------------------
// `browser-http`.
// ---------------------------------------------------------------------------

/// The transport seam. Swap [`HttpBackend`] for a double and the whole fetcher
/// stack runs without a socket.
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use browser_http::backend::{HttpBackend, Method, WireRequest, WireResponse};
/// `Body` here is the **request** body (`data=` / `json=`). The parser's
/// parse-input body — the one `Document::parse_content` takes — is
/// [`parser::document::Body`].
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use browser_http::config::{
    Body, ClientCert, Cookies, FetcherConfig, HeaderDict, HttpVersion, Impersonate, ProxySpec,
    Query, RequestOverrides, SelectorConfig,
};
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use browser_http::error::{FetchError, TransportError};
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use browser_http::response::{
    DocumentHook, RedirectHop, Response, ResponseBuilder, ResponseFactory,
};
/// `RequestBuilder` is the **async** one. The blocking façade has its own, at
/// [`blocking::RequestBuilder`].
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use browser_http::session::{
    AsyncFetcher, AsyncFetcherClient, AsyncFetcherSession, CookieJar, RequestBuilder,
    SessionBuilder,
};

/// The synchronous façade — `Fetcher.get(...)` without an `.await`.
///
/// It owns a private multi-thread runtime and refuses to nest inside one, so it
/// is the entry point for scripts and for the CLI's non-async paths.
#[cfg(feature = "blocking")]
#[cfg_attr(docsrs, doc(cfg(feature = "blocking")))]
pub use browser_http::blocking;
#[cfg(feature = "blocking")]
#[cfg_attr(docsrs, doc(cfg(feature = "blocking")))]
pub use browser_http::blocking::{Fetcher, FetcherClient, FetcherSession};

// ---------------------------------------------------------------------------
// `browser-cdp`.
// ---------------------------------------------------------------------------

#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser_cdp::config::{
    BrowserConfig, FetchParams, ProxyRotation, ResolvedFetchParams, StealthExtras, ValidatedConfig,
};
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser_cdp::error::{BrowserError, PoolError};
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser_cdp::flags::ExtraFlagsPolicy;
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser_cdp::response::BrowserFetch;
/// `fetch_once` is `DynamicFetcher.fetch` / `StealthyFetcher.fetch`: launch,
/// fetch, close. [`BrowserSession`] is the one to keep if you want the browser
/// to survive more than one page.
#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use browser_cdp::session::{fetch_once, BrowserSession, SessionHooks};

// ---------------------------------------------------------------------------
// `browser-spider`.
// ---------------------------------------------------------------------------

#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::engine::CrawlerEngine;
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::error::SpiderError;
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::items::ItemList;
/// The spider's own request type is **not** re-exported flat, because
/// `browser::Request` next to [`RequestBuilder`] would read as the HTTP one.
/// It is [`spider::request::Request`].
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::request::{CallbackId, Item, RequestBody, SessionKwargs};
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::session::{CrawlSession, FetchedResponse};
/// `CrawlError` is the one [`SpiderRunner`] itself raises — a spider with no
/// name, no configured session, or a run that could not start. It is flat
/// alongside [`SpiderError`] because it is what `SpiderRunner::new` and
/// `SpiderRunner::run` return, and a caller who cannot name the error of the
/// entry point cannot write the entry point's signature.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::spider::{
    CallbackStream, CrawlError, CrawlResponse, FollowOptions, HttpCrawlSession, RunOptions, Spider,
    SpiderRunner, SpiderSettings, Yielded,
};

/// The attribute macro [`Spider`] is declared with.
///
/// `#[async_trait]` is not sugar a caller can skip: the trait's methods are
/// rewritten into `-> Pin<Box<dyn Future>>` by the macro, so an `impl` block
/// without it does not typecheck. Re-exporting it here is what makes
/// `browser-rs` a complete dependency — and pins the *version*, which two
/// unrelated `async-trait`s in one build would not.
///
/// ```
/// # #[cfg(feature = "spider")] {
/// use std::sync::Arc;
///
/// use browser::{async_trait, CallbackStream, CrawlResponse, Spider, SpiderSettings};
///
/// struct MySpider(SpiderSettings);
///
/// #[async_trait]
/// impl Spider for MySpider {
///     fn name(&self) -> &str {
///         "mine"
///     }
///     fn settings(&self) -> &SpiderSettings {
///         &self.0
///     }
///     async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
///         let _ = response.css("h1::text");
///         Box::pin(browser::futures::stream::iter([]))
///     }
/// }
/// # }
/// ```
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use async_trait::async_trait;

#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_spider::stats::{CrawlResult, CrawlStats};
/// The stream crate [`CallbackStream`] is made of.
///
/// `CallbackStream` is a type **alias** for
/// `futures::stream::BoxStream<'static, Yielded>`, so a callback cannot return
/// one without constructing a `futures` stream —
/// `Box::pin(futures::stream::iter(...))` for the common "here are my items"
/// case, or a `async_stream::stream!` for a callback that yields as it goes.
/// Re-exported for the same reason [`macro@async_trait`] is: a façade user should not
/// have to guess the version.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use futures;

// ---------------------------------------------------------------------------
// `browser-templates` — the ready-made spiders.
//
// Gated on the façade's `spider`, which is what pulls the crate in. Everything
// here is a *value* a scraping program writes down by name; the free functions
// that only a port would call (`strip_html5_whitespace`, `safe_url_string`,
// `python_title`, `remove_tags`) are here too, because they are exactly the
// pieces someone extending a template needs and they have no shorter path.
// ---------------------------------------------------------------------------

/// `CrawlSpider` and its rule machinery.
///
/// [`requests_to_follow`] is a **free function**, deliberately: Python's
/// `CrawlSpider._requests_to_follow` is a method, and making it free is what
/// lets a spider that is not a `CrawlSpider` compose rule dispatch into its own
/// callback. `crates/browser-rs/tests/integration.rs::crawl_spider_seam` is that
/// shape written out.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates::crawl::{
    requests_to_follow, CallbackFn, CrawlRule, CrawlSpider, CrawlSpiderBuilder, ProcessRequestFn,
};
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates::error::TemplateError;
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates::feed::{
    parse_xml, CSVFeedSpider, CSVFeedSpiderBuilder, XMLFeedSpider, XMLFeedSpiderBuilder, XmlNode,
    CSV_RESTKEY,
};
/// URL discovery. [`strip_html5_whitespace`] is the five HTML5 space characters
/// only — **not** `str::trim`, which would also eat U+00A0 and the rest of
/// Unicode `White_Space`.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates::links::{
    hostname_of, safe_url_string, strip_html5_whitespace, url_extensions, LinkExtractor,
    LinkExtractorBuilder, SafeUrlError, IGNORED_EXTENSIONS,
};
/// The Shopify template, plus the two string helpers its item builder needs.
/// [`python_title`] is CPython's `str.title()`, not `str::to_uppercase` — `ß`
/// title-cases to `"Ss"` and a word-final `Σ` lower-cases to `ς`.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates::shopify::{
    python_title, remove_tags, ShopifySpider, ShopifySpiderBuilder,
};
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use browser_templates::sitemap::{
    parse_sitemap, robots_sitemaps, SitemapResult, SitemapSpider, SitemapSpiderBuilder,
    UnruledRoute,
};

/// The RAG ingestion template: crawl a site, emit one `{url, title, markdown}`
/// item per page.
///
/// Needs **both** features — `spider` for the engine and `markdown` for
/// `Response::markdown()`, which is the whole premise. It works over the HTTP
/// fetcher and over real Chrome alike, because the conversion is a property of
/// the response and not of the transport.
#[cfg(all(feature = "spider", feature = "markdown"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "spider", feature = "markdown"))))]
pub use browser_templates::site_to_markdown::{SiteToMarkdownSpider, SiteToMarkdownSpiderBuilder};

#[cfg(test)]
mod tests {
    //! The re-export surface is a *contract*, so it is tested the only way a
    //! re-export can be: by naming every one of these paths and refusing to
    //! compile if it moves.

    #[test]
    fn the_core_vocabulary_is_reachable_flat() {
        let _ = crate::Override::<bool>::Unset;
        let _ = crate::HttpMethod::Get;
        let _ = crate::ExtractionType::Markdown;
        let _ = crate::Proxy::Url("http://p:1".to_owned());
        assert_eq!(crate::round_half_even(0.5, 0), 0.0);
        assert!(crate::parse_cookie_string("a=1").expect("valid").len() == 1);
    }

    #[test]
    fn the_member_crates_are_reachable_by_their_short_names() {
        // A module path, not a type: this fails to compile if the alias goes.
        let _ = crate::core::text::clean_spaces("a  b");
        #[cfg(feature = "parser")]
        let _ = crate::parser::config::DEFAULT_PERCENTAGE;
        #[cfg(feature = "http")]
        let _ = crate::http::config::DEFAULT_TIMEOUT;
        #[cfg(feature = "browser")]
        let _ = crate::browser::config::DEFAULT_TIMEOUT_MS;
        #[cfg(feature = "spider")]
        let _ = crate::spider::spider::DEFAULT_CONCURRENT_REQUESTS;
    }

    #[cfg(feature = "parser")]
    #[test]
    fn the_document_api_is_reachable_flat() {
        let doc = crate::Document::parse("<p id='x'>hi</p>");
        let node: crate::Node<'_> = doc.css_first("p").expect("valid").expect("matched");
        assert_eq!(node.get_all_text_with(&crate::AllTextOpts::default()), "hi");
        let nodes: crate::Nodes<'_> = doc.css("p").expect("valid");
        assert_eq!(nodes.len(), 1);
    }

    #[cfg(feature = "http")]
    #[test]
    fn the_response_api_is_reachable_flat() {
        let response: crate::Response = crate::Response::builder(200, "https://x.test/")
            .body("<p>hi</p>")
            .build();
        assert_eq!(response.status, 200);
        assert_eq!(response.css("p::text").expect("valid").texts(), ["hi"]);
    }

    /// The hole wave 5 found and fixed in `Cargo.toml`: `markdown` has to reach
    /// `browser-http`, not only `browser-parser`, or `Response::markdown()`
    /// is unreachable through the façade and unit 19 cannot write a `.md` file.
    /// This does not compile if that regresses.
    #[cfg(all(feature = "http", feature = "markdown"))]
    #[test]
    fn the_markdown_feature_reaches_response_through_the_facade() {
        let response = crate::Response::builder(200, "https://x.test/")
            .body("<h1>Title</h1>")
            .build();
        assert_eq!(response.markdown(), "Title\n=====");
        assert_eq!(
            response.markdown_with(Some("h1"), true).expect("valid"),
            "Title\n====="
        );
    }

    /// Likewise for `adaptive`: the feature is forwarded to `browser-http`,
    /// so the storage-backed methods exist on the façade's `Response`.
    #[cfg(all(feature = "http", feature = "adaptive"))]
    #[test]
    fn the_adaptive_feature_reaches_response_through_the_facade() {
        let response = crate::Response::builder(200, "https://x.test/")
            .body("<p>hi</p>")
            .build();
        assert!(!response.adaptive_enabled());
        assert!(!response.has_storage());
    }

    #[cfg(feature = "blocking")]
    #[test]
    fn the_blocking_facade_is_reachable() {
        // Naming the types is the assertion; building a runtime is not.
        fn _takes(_: &crate::blocking::FetcherSession, _: &crate::FetcherClient) {}
        let _ = crate::Fetcher::get("https://x.test/");
    }

    /// The templates surface, named type by type. Unit 18 shipped
    /// `browser-templates` in parallel with unit 19, so wave 5 could only
    /// re-export the module; these are the flat names wave 6 settled on.
    #[cfg(feature = "spider")]
    #[test]
    fn the_ready_made_spiders_are_reachable_flat() {
        // The URL primitives, which are the ones a user calls directly.
        assert_eq!(crate::strip_html5_whitespace("\u{c}\n /a \t"), "/a");
        assert_eq!(crate::url_extensions("/a/report.pdf"), ["pdf"]);
        assert_eq!(
            crate::safe_url_string("http://x.test/a b", "utf-8").expect("valid"),
            "http://x.test/a%20b"
        );
        assert_eq!(crate::hostname_of("https://X.test:8080/a"), "x.test");
        assert!(crate::IGNORED_EXTENSIONS.contains(&"pdf"));
        assert_eq!(crate::remove_tags("<b>hi</b>"), "hi");
        assert_eq!(crate::python_title("straße-sale"), "Straße-Sale");

        // The extractor, the rule and the free dispatch function.
        let extractor = crate::LinkExtractor::builder()
            .allow(["/product/"])
            .build()
            .expect("valid");
        let _rule = crate::CrawlRule::new(extractor).callback(crate::CallbackId::PARSE);
        fn _dispatch(
            rules: &[crate::CrawlRule],
            response: &crate::CrawlResponse,
        ) -> Result<Vec<crate::spider::request::Request>, crate::TemplateError> {
            crate::requests_to_follow(rules, response)
        }

        // The spiders. Naming the types is the assertion.
        fn _spiders(
            _: &crate::CrawlSpider,
            _: &crate::SitemapSpider,
            _: &crate::ShopifySpider,
            _: &crate::XMLFeedSpider,
            _: &crate::CSVFeedSpider,
        ) {
        }
        let _routes = crate::UnruledRoute::InheritSitemapCallback;
        let _restkey: &str = crate::CSV_RESTKEY;

        // …and the module, for everything that stays long-form.
        let body = b"not gzip";
        assert_eq!(
            crate::templates::gzip::decompress(body, Some("text/xml")).expect("passthrough"),
            std::borrow::Cow::Borrowed(&body[..])
        );
    }

    /// `SiteToMarkdownSpider` needs `spider` **and** `markdown`, which is the
    /// one two-feature gate on the whole surface.
    #[cfg(all(feature = "spider", feature = "markdown"))]
    #[test]
    fn the_rag_template_is_reachable_when_both_features_are_on() {
        let spider = crate::SiteToMarkdownSpider::builder("docs")
            .settings(
                crate::SpiderSettings::default()
                    .with_start_urls(["https://docs.test/"])
                    .with_allowed_domains(["docs.test"]),
            )
            .build()
            .expect("valid");
        assert_eq!(
            spider.filename_for("https://docs.test/guide/intro"),
            "docs.test-guide-intro"
        );
    }

    /// Nothing a public method returns should be unnameable through the façade.
    /// Wave 6 added four dependencies to make that true again.
    #[cfg(feature = "parser")]
    #[test]
    fn the_types_the_document_api_hands_back_have_facade_paths() {
        let doc = crate::Document::parse("<p>hi</p><p>there</p>");

        let _tree: &crate::dom::Document = doc.dom();
        let value: crate::xpath::Value<_> = doc
            .xpath_value("count(//p)", &crate::xpath::eval::Variables::default())
            .expect("valid");
        assert!(matches!(value, crate::xpath::Value::Number(count) if count == 2.0));
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn the_two_sanitiser_passes_have_facade_paths() {
        let tree = crate::dom::Document::parse("<p>keep<script>evil()</script>");
        let clean = crate::markdown::sanitize_for_ai(&crate::markdown::strip_noise_tags(&tree));
        assert_eq!(crate::markdown::to_markdown_default(clean.root()), "keep");
        assert_eq!(crate::markdown::html_to_markdown("<h1>T</h1>"), "T\n=");
    }

    #[cfg(feature = "adaptive")]
    #[test]
    fn the_storage_backend_can_be_constructed_through_the_facade() {
        fn _takes(_: &dyn crate::storage::StorageSystem) {}
        let storage = crate::storage::SqliteStorage::in_memory(Some("https://x.test/"))
            .expect("an in-memory database");
        _takes(&storage);
    }

    #[test]
    fn the_prelude_brings_the_common_names_into_scope() {
        #[allow(unused_imports)]
        use crate::prelude::*;

        #[cfg(feature = "parser")]
        {
            let doc = Document::parse("<p>hi</p>");
            assert_eq!(doc.css("p").expect("valid").len(), 1);
        }
        #[cfg(feature = "http")]
        {
            let _ = AsyncFetcher::get("https://x.test/");
        }
    }
}
