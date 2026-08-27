//! The typed error enum for this crate: [`Error`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! The façade owns no logic, so it raises none of these itself. What it owns is
//! the **aggregate**: one enum with a `From` for each member crate's error, so
//! an application that reaches for two entry points can still write one
//! `Result` and one `?`.
//!
//! ```
//! # #[cfg(feature = "http")]
//! # mod demo {
//! use std::path::Path;
//!
//! use browser::error::Result;
//! use browser::Response;
//!
//! /// `css` fails with a `ParseError` and `write` with an `io::Error`; both
//! /// reach the same `?` with no conversion written at either call site.
//! fn save_title(page: &Response, path: &Path) -> Result<()> {
//!     let title = page.css("title::text")?.get().unwrap_or_default();
//!     std::fs::write(path, title)?;
//!     Ok(())
//! }
//! # }
//! ```
//!
//! It is deliberately *not* re-exported flat at the crate root. `browser::Error`
//! next to [`ParseError`](crate::ParseError) and `FetchError` would read as the
//! one a given call returns, and none of them return this; and a flat `Result`
//! would shadow `std::result::Result` in any module that glob-imported the
//! crate root. Write `browser::error::Error`, or alias it locally.
//!
//! Each variant is gated on the feature that supplies its source type, so the
//! enum is exactly as wide as the build is.

use thiserror::Error as ThisError;

/// Every error any `browser-rs` entry point can raise, in one enum.
///
/// Use it as the error type of a `main` or of an application-level function.
/// Library code closer to a single member crate is usually better off naming
/// that crate's error directly — this aggregate is a convenience for callers,
/// not a replacement for the specific types.
#[derive(Debug, ThisError)]
#[non_exhaustive]
pub enum Error {
    /// A shared-vocabulary failure: a bad regular expression, an unparseable
    /// cookie string or header line, or JSON that did not round-trip.
    #[error(transparent)]
    Core(#[from] browser_core::error::CoreError),

    /// The document API: a CSS or XPath syntax error, or a failed query.
    ///
    /// Boxed — see the note under the enum.
    #[cfg(feature = "parser")]
    #[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
    #[error(transparent)]
    Parse(Box<browser_parser::error::ParseError>),

    /// The HTML tree underneath the document API.
    #[cfg(feature = "parser")]
    #[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
    #[error(transparent)]
    Dom(#[from] browser_dom::error::DomError),

    /// The XPath engine.
    #[cfg(feature = "parser")]
    #[cfg_attr(docsrs, doc(cfg(feature = "parser")))]
    #[error(transparent)]
    XPath(#[from] browser_xpath::error::XPathError),

    /// An HTTP fetch: transport, redirect policy, retries, configuration.
    ///
    /// Boxed — see the note under the enum.
    #[cfg(feature = "http")]
    #[cfg_attr(docsrs, doc(cfg(feature = "http")))]
    #[error(transparent)]
    Fetch(Box<browser_http::error::FetchError>),

    /// A headless-Chromium fetch, or the browser failing to launch at all.
    #[cfg(feature = "browser")]
    #[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
    #[error(transparent)]
    Browser(#[from] browser_cdp::error::BrowserError),

    /// The crawling engine: the scheduler, a session, a checkpoint.
    #[cfg(feature = "spider")]
    #[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
    #[error(transparent)]
    Spider(#[from] browser_spider::error::SpiderError),

    /// Starting or running a crawl: an unnamed spider, no configured session.
    ///
    /// Separate from [`Error::Spider`] because it is a separate enum —
    /// `SpiderRunner::new` and `SpiderRunner::run` raise this one, and it is
    /// what an application's `main` actually meets first.
    #[cfg(feature = "spider")]
    #[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
    #[error(transparent)]
    Crawl(#[from] browser_spider::spider::CrawlError),

    /// A ready-made spider: link extraction, a sitemap, a feed.
    #[cfg(feature = "spider")]
    #[cfg_attr(docsrs, doc(cfg(feature = "spider")))]
    #[error(transparent)]
    Template(#[from] browser_templates::error::TemplateError),

    /// Markdown conversion or AI sanitisation.
    #[cfg(feature = "markdown")]
    #[cfg_attr(docsrs, doc(cfg(feature = "markdown")))]
    #[error(transparent)]
    Markdown(#[from] browser_markdown::error::MarkdownError),

    /// Adaptive selector storage.
    ///
    /// Boxed — see the note under the enum.
    #[cfg(feature = "adaptive")]
    #[cfg_attr(docsrs, doc(cfg(feature = "adaptive")))]
    #[error(transparent)]
    Storage(Box<browser_storage::error::StorageError>),

    /// Reading or writing a file — a checkpoint directory, an exported
    /// `items.jsonl`, a `SiteToMarkdownSpider` output page.
    ///
    /// The member crates that touch the filesystem wrap `std::io::Error` in
    /// their own enums; this variant is for the application's own I/O, so that
    /// a `main` writing its results out still needs only one error type.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Why three variants are boxed and the rest are not.
//
// `ParseError`, `FetchError` and `StorageError` are 120 bytes each, and every
// other member error is 72 or less. Left inline, the aggregate is 128 bytes,
// which is exactly `clippy::result_large_err`'s threshold — and the lint is
// right: `Result<T, Error>` is what a *caller's* signatures become, so the size
// is paid on every success path in their program too, not just on failure.
// Boxing those three brings it to 80 and leaves the small ones alone, because a
// `Box` on a 16-byte `DomError` would be a heap allocation bought for nothing.
//
// The `#[from]` derive cannot box, so those three get a hand-written `From`.
// Nothing at a call site changes: `?` still converts, and `Box<E>` derefs, so
// `Err(Error::Parse(error)) => error.to_string()` reads the same as before.
// ---------------------------------------------------------------------------

/// Generate a `From<E>` that boxes on the way in.
macro_rules! boxed_from {
    ($( $(#[$attr:meta])* $variant:ident($source:path) );+ $(;)?) => {
        $(
            $(#[$attr])*
            impl From<$source> for Error {
                fn from(source: $source) -> Self {
                    Self::$variant(Box::new(source))
                }
            }
        )+
    };
}

boxed_from! {
    #[cfg(feature = "parser")]
    Parse(browser_parser::error::ParseError);
    #[cfg(feature = "http")]
    Fetch(browser_http::error::FetchError);
    #[cfg(feature = "adaptive")]
    Storage(browser_storage::error::StorageError);
}

/// `Result` with [`Error`] as the default error type.
///
/// Same shape as every member crate's alias, so `browser::error::Result<T>` and
/// `browser_parser::Result<T>` read the same way at a call site.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::Error;

    /// The aggregate has to be usable as the error type of a `main`, which
    /// means `Send + Sync + 'static` — a `?` in an async task otherwise fails
    /// to compile for reasons that have nothing to do with the error.
    #[test]
    fn the_aggregate_is_send_sync_and_static() {
        fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}
        assert_error::<Error>();
    }

    /// The reason three variants are boxed. `clippy::result_large_err` fires at
    /// 128 bytes, and this enum sat exactly on it; the boxes are what keep
    /// `Result<T, Error>` a reasonable thing for a *caller* to write. If a new
    /// variant pushes it back over, this fails before clippy does — and with a
    /// clearer message.
    #[test]
    fn the_aggregate_stays_under_the_large_err_threshold() {
        let size = std::mem::size_of::<Error>();
        assert!(
            size < 128,
            "`Error` is {size} bytes; box the variant that grew it, as \
             `Parse`, `Fetch` and `Storage` already are"
        );
    }

    /// `#[error(transparent)]` everywhere: the aggregate must never add a
    /// prefix of its own. A message the specs pin as observable has to survive
    /// being carried through here unchanged.
    #[test]
    fn wrapping_does_not_change_the_message() {
        let inner = browser_core::error::HeaderParseError::MissingColon {
            line: "InvalidHeader".to_owned(),
        };
        let expected = inner.to_string();
        let wrapped: Error = browser_core::error::CoreError::from(inner).into();
        assert_eq!(wrapped.to_string(), expected);
        assert_eq!(
            expected,
            "Could not parse header without colon: 'InvalidHeader'."
        );
    }

    /// The point of the enum: errors from two different crates reaching one `?`
    /// with no conversion written at either call site, and each still
    /// distinguishable afterwards.
    #[cfg(feature = "parser")]
    #[test]
    fn errors_from_two_crates_share_one_question_mark() {
        /// Read a file, parse it, count the matches. `std::io` and
        /// `browser-parser` both fail into `super::Result`; neither call site
        /// mentions the other's error type.
        fn count(path: &std::path::Path, selector: &str) -> super::Result<usize> {
            let html = std::fs::read_to_string(path)?;
            Ok(crate::Document::parse(&html).css(selector)?.len())
        }

        let dir = tempfile::tempdir().expect("a writable temp directory");
        let path = dir.path().join("page.html");
        std::fs::write(&path, "<p>a</p><p>b</p>").expect("writable");

        assert_eq!(count(&path, "p").expect("both calls succeed"), 2);
        // A selector syntax error arrives as `Parse`…
        assert!(matches!(count(&path, "<<<"), Err(Error::Parse(_))));
        // …and a missing file as `Io`, through the same signature.
        assert!(matches!(
            count(&dir.path().join("absent.html"), "p"),
            Err(Error::Io(_))
        ));
    }
}
