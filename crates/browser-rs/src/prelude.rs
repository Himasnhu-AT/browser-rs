//! The prelude: the handful of names `use browser::prelude::*` should bring into scope.
//!
//! Everything here is also available flat at the crate root; this module exists
//! so a program that scrapes can start with one `use` line. It is deliberately
//! small — a prelude that re-exports everything is a prelude that collides with
//! everything.
//!
//! ```
//! use browser::prelude::*;
//!
//! # #[cfg(feature = "parser")]
//! # {
//! let doc = Document::parse("<h1>Title</h1><a href='/next'>next</a>");
//! assert_eq!(doc.css("h1::text")?.get(), Some("Title".to_owned()));
//! assert_eq!(doc.css("a")?.attrs("href"), ["/next"]);
//! # }
//! # Ok::<(), browser::ParseError>(())
//! ```
//!
//! Each block below is gated on the feature that supplies it, so the glob is
//! always valid, whatever the build.

// -- always ------------------------------------------------------------------

pub use crate::core::text::{TextExt, TextsExt};
pub use crate::core::types::{ExtractionType, FollowRedirects, HttpMethod, Override, Proxy};

// -- `parser` ----------------------------------------------------------------

#[cfg(feature = "parser")]
#[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
pub use crate::{AllTextOpts, CssOpts, Document, FindQuery, Node, Nodes, ParseError};

// -- `http` ------------------------------------------------------------------

#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub use crate::{AsyncFetcher, AsyncFetcherSession, FetchError, Response};

// -- `blocking` --------------------------------------------------------------

/// The synchronous `Fetcher`. Its async twin is [`AsyncFetcher`].
#[cfg(feature = "blocking")]
#[cfg_attr(docsrs, doc(cfg(feature = "blocking")))]
pub use crate::Fetcher;

// -- `browser` ---------------------------------------------------------------

#[cfg(feature = "browser")]
#[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
pub use crate::{BrowserConfig, BrowserSession, FetchParams};

// -- `spider` ----------------------------------------------------------------

#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use crate::{CrawlResponse, CrawlStats, Item, Spider, SpiderRunner, SpiderSettings, Yielded};

// -- `spider` (the ready-made templates) -------------------------------------

/// The four names a program that uses a ready-made spider writes down, plus
/// `TemplateError` so `?` works in the callback. The rest of
/// `browser-templates` — `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`,
/// `CSVFeedSpider`, the builders, `gzip::decompress` — stays flat at the crate
/// root and in [`crate::templates`], because a prelude that re-exports
/// everything collides with everything.
#[cfg(feature = "spider")]
#[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
pub use crate::{requests_to_follow, CrawlRule, CrawlSpider, LinkExtractor, TemplateError};

/// The RAG template needs `markdown` as well as `spider`.
#[cfg(all(feature = "spider", feature = "markdown"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "spider", feature = "markdown"))))]
pub use crate::SiteToMarkdownSpider;
