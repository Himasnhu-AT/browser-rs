//! The typed error enum for this crate: [`TemplateError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Python's template spiders raise bare `ValueError`, `RuntimeError`,
//! `NotImplementedError` and `OSError`. `spider-support.md` §9.2, §10.2, §11.1,
//! §12.2 and §7 pin the message text of five of them, and each gets its own
//! variant here so the text survives.
//!
//! Three of those messages interpolate `self.__class__.__name__`, which has no
//! Rust equivalent — a `CrawlSpider` subclass is a *value*, not a type. The port
//! interpolates `CrawlSpider::name` (Python's `spider.name`)
//! instead, which is the identifier a user actually recognises in a log line.

/// Everything that can go wrong inside `browser-templates`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TemplateError {
    /// A gzipped body inflated past [`crate::gzip::GUNZIP_MAX_SIZE`].
    ///
    /// Python: `OSError(f"gzip output exceeds {_GUNZIP_MAX_SIZE} bytes")`,
    /// raised from `templates/_utils.py::_decompress`. Every caller catches
    /// `OSError`, logs a warning and yields nothing.
    #[error("gzip output exceeds {limit} bytes")]
    GzipBomb {
        /// The cap, in bytes.
        limit: usize,
    },

    /// A body that announced itself as gzip did not inflate.
    ///
    /// Python: `gzip.BadGzipFile`, which is an `OSError` subclass, so it takes
    /// the same "warn and yield nothing" path as [`TemplateError::GzipBomb`].
    #[error("failed to decompress gzip body: {message}")]
    Gzip {
        /// `std::io::Error`'s own `Display` output.
        message: String,
    },

    /// A gzip member ended before its end-of-stream marker.
    ///
    /// Python raises `EOFError` here, and `EOFError` is **not** an `OSError` —
    /// so unlike [`TemplateError::Gzip`] and [`TemplateError::GzipBomb`] it
    /// escapes the callers' `except OSError` and reaches the engine's callback
    /// error handler (`on_error` plus an `ERROR` log, crawl continues). The
    /// port keeps the two apart for exactly that reason: see
    /// [`crate::gzip::decompress`] and how [`crate::sitemap::SitemapSpider`]
    /// and [`crate::feed::XMLFeedSpider`] route each of them.
    #[error("Compressed file ended before the end-of-stream marker was reached")]
    GzipTruncated,

    /// A `LinkExtractor` pattern did not compile.
    ///
    /// Python compiles at construction time too (`_compile_patterns` calls
    /// `re.compile`), so `re.error` surfaces from `LinkExtractor(...)`.
    #[error("invalid {field} pattern {pattern:?}: {message}")]
    InvalidPattern {
        /// `"allow"` or `"deny"`.
        field: &'static str,
        /// The pattern as written.
        pattern: String,
        /// `regex`'s own explanation.
        message: String,
    },

    /// [`crate::feed::XMLFeedSpider`]'s `itertag` names a prefix that
    /// `namespaces` does not define.
    ///
    /// Python: `` ValueError(f"`itertag` prefix {prefix!r} is not defined in
    /// `namespaces`") ``. `{prefix!r}` is CPython's `repr`, which single-quotes
    /// a `str`; the single quotes are written literally here rather than using
    /// Rust's `{:?}` (which would double-quote). A prefix containing a quote or
    /// a backslash would repr differently, but an XML prefix cannot contain
    /// either.
    #[error("`itertag` prefix '{prefix}' is not defined in `namespaces`")]
    UndefinedNamespacePrefix {
        /// The prefix that was written before the `:`.
        prefix: String,
    },

    /// [`crate::sitemap::SitemapSpider`] was started with no `sitemap_urls`.
    ///
    /// Python: ``RuntimeError("`SitemapSpider` needs `sitemap_urls` to be
    /// set.")``.
    #[error("`SitemapSpider` needs `sitemap_urls` to be set.")]
    MissingSitemapUrls,

    /// [`crate::shopify::ShopifySpider`] could not work out which store to walk.
    ///
    /// Python: `ValueError(f"{cls} must set `target_website`, `start_urls`, or
    /// `allowed_domains`")`.
    #[error("{spider} must set `target_website`, `start_urls`, or `allowed_domains`")]
    MissingTargetWebsite {
        /// The spider's name, standing in for Python's class name.
        spider: String,
    },

    /// [`crate::site_to_markdown::SiteToMarkdownSpider`] was built without an
    /// offsite filter.
    ///
    /// Python: `ValueError(f"{cls} must set `allowed_domains` to bound the
    /// crawl to the target website(s)")`.
    #[error("{spider} must set `allowed_domains` to bound the crawl to the target website(s)")]
    MissingAllowedDomains {
        /// The spider's name, standing in for Python's class name.
        spider: String,
    },

    /// An abstract callback was never supplied.
    ///
    /// Python: `NotImplementedError(f"{cls} must implement {method}() method")`
    /// from `SitemapSpider.parse`, `XMLFeedSpider.parse_node` and
    /// `CSVFeedSpider.parse_row`. The port cannot leave a required closure
    /// unset at compile time *and* keep the builders ergonomic, so the check is
    /// where Python's is: at the moment the callback would have run.
    #[error("{spider} must implement {method}() method")]
    NotImplemented {
        /// The spider's name, standing in for Python's class name.
        spider: String,
        /// The method that should have been provided.
        method: &'static str,
    },

    /// A feed or sitemap body was not well-formed XML.
    ///
    /// Python: `lxml.etree.XMLSyntaxError`, caught and logged by every caller.
    #[error("failed to parse XML: {message}")]
    Xml {
        /// `quick-xml`'s own explanation.
        message: String,
    },

    /// A CSV feed could not be read.
    #[error("failed to read CSV feed: {message}")]
    Csv {
        /// The `csv` crate's own explanation.
        message: String,
    },

    /// A JSON body the Shopify spider needed was missing or the wrong shape.
    ///
    /// Python lets the `KeyError`/`JSONDecodeError` propagate into the engine's
    /// callback error handler, which calls `on_error` and continues the crawl;
    /// the port turns it into [`browser_spider::spider::Yielded::error`],
    /// which lands in exactly the same place.
    #[error("unexpected Shopify JSON: {message}")]
    ShopifyJson {
        /// What was missing.
        message: String,
    },

    /// `output_dir` could not be written.
    #[error("failed to write {path}: {message}")]
    Output {
        /// The file that could not be written.
        path: String,
        /// `std::io::Error`'s own `Display` output.
        message: String,
    },

    /// A `follow` failed, almost always because the response was built by hand
    /// and has no originating request.
    #[error(transparent)]
    Crawl(#[from] browser_spider::spider::CrawlError),
}

/// `Result` with this crate's error as the default failure type.
pub type Result<T, E = TemplateError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_messages_are_byte_for_byte_pythons() {
        assert_eq!(
            TemplateError::GzipBomb {
                limit: 64 * 1024 * 1024
            }
            .to_string(),
            "gzip output exceeds 67108864 bytes"
        );
        assert_eq!(
            TemplateError::MissingSitemapUrls.to_string(),
            "`SitemapSpider` needs `sitemap_urls` to be set."
        );
        assert_eq!(
            TemplateError::UndefinedNamespacePrefix {
                prefix: "media".to_owned()
            }
            .to_string(),
            "`itertag` prefix 'media' is not defined in `namespaces`"
        );
        assert_eq!(
            TemplateError::MissingTargetWebsite {
                spider: "shopify".to_owned()
            }
            .to_string(),
            "shopify must set `target_website`, `start_urls`, or `allowed_domains`"
        );
        assert_eq!(
            TemplateError::MissingAllowedDomains {
                spider: "site".to_owned()
            }
            .to_string(),
            "site must set `allowed_domains` to bound the crawl to the target website(s)"
        );
        assert_eq!(
            TemplateError::NotImplemented {
                spider: "feed".to_owned(),
                method: "parse_node",
            }
            .to_string(),
            "feed must implement parse_node() method"
        );
    }

    #[test]
    fn a_crawl_error_converts() {
        let error: TemplateError = browser_spider::spider::CrawlError::NoRequestOnResponse.into();
        assert!(matches!(error, TemplateError::Crawl(_)));
    }
}
