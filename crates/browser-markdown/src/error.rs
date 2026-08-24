//! The single error type this crate can produce.
//!
//! Conversion itself is infallible — markdownify never fails on malformed HTML
//! (BeautifulSoup recovers, and so does `html5ever`). The only thing Python's
//! `MarkdownConverter.__init__` rejects is a contradictory option set, so that
//! is the only variant here.

use thiserror::Error;

/// Everything that can go wrong in `browser-markdown`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MarkdownError {
    /// Both [`MarkdownOptions::strip`] and [`MarkdownOptions::convert`] were set.
    ///
    /// The message is markdownify's verbatim.
    ///
    /// [`MarkdownOptions::strip`]: crate::convert::MarkdownOptions::strip
    /// [`MarkdownOptions::convert`]: crate::convert::MarkdownOptions::convert
    #[error("You may specify either tags to strip or tags to convert, but not both.")]
    StripAndConvert,
}

/// `Result` alias for this crate.
pub type Result<T, E = MarkdownError> = std::result::Result<T, E>;
