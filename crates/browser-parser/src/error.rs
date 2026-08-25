//! The typed error enum for this crate: [`ParseError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that `parser.md` §9 pins as observable are reproduced **verbatim**,
//! because user code greps for them.
//!
//! # Mapping to Python
//!
//! | Python | here |
//! |---|---|
//! | `ValueError("Selector class needs HTML content, or root arguments to work")` | [`ParseError::NeedsContent`] |
//! | `SelectorSyntaxError(f"Invalid CSS selector '{sel}': {e}")` | [`ParseError::CssSyntax`] |
//! | `SelectorSyntaxError(f"Invalid XPath selector: {sel}")` | [`ParseError::XPathSyntax`] |
//! | `RuntimeError("Can't use `adaptive` features …")` | [`ParseError::AdaptiveDisabled`] |
//! | `TypeError("Text nodes do not have attributes")` | [`ParseError::TextNodeHasNoAttributes`] |
//! | `TypeError("You have to pass something to search with, …")` | [`ParseError::EmptyFindQuery`] |
//! | `KeyError` from `selector["missing"]` | [`ParseError::MissingAttribute`] |
//! | invalid JSON out of `.json()` | [`ParseError::Json`] |
//!
//! Three Python errors have **no** Rust analogue and are deliberately absent —
//! `TypeError("content argument must be str or bytes …")` (the type system
//! decides), `ValueError("Storage class must be wrapped with lru_cache …")` and
//! `ValueError("Storage system must be inherited from …")` (the
//! `StorageSystem` trait bound decides). See `ARCHITECTURE.md` §11.9.

use thiserror::Error;

/// Everything `browser-parser` can fail with.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParseError {
    /// Neither HTML content nor an existing node was supplied.
    #[error("Selector class needs HTML content, or root arguments to work")]
    NeedsContent,

    /// A CSS selector did not parse, or used a construct the translator rejects.
    ///
    /// The payload is already the full Python message,
    /// `Invalid CSS selector '{selector}': {inner}`.
    #[error("{0}")]
    CssSyntax(String),

    /// An XPath expression did not compile, or failed at evaluation time.
    ///
    /// The payload is already the full Python message,
    /// `Invalid XPath selector: {selector}`.
    #[error("{0}")]
    XPathSyntax(String),

    /// An XPath expression produced a scalar where a node-set was required.
    ///
    /// Python does not guard this — the scalar falls into `__handle_elements`
    /// and is iterated as a sequence, which for a string means per-character
    /// `Selector`s. `parser.md` §3.5 explicitly allows a port to reject it
    /// instead; use [`crate::node::Node::xpath_value`] when a scalar is what
    /// you wanted.
    #[error("XPath expression `{expr}` returned a {actual}, not a node-set")]
    NotANodeSet {
        /// The offending expression.
        expr: String,
        /// `"string"`, `"number"` or `"boolean"`.
        actual: &'static str,
    },

    /// `save`, `retrieve` or an adaptive `css`/`xpath` branch was reached while
    /// the adaptive feature was switched off for this document.
    #[error(
        "Can't use `adaptive` features while it's disabled globally, \
         you have to start a new class instance."
    )]
    AdaptiveDisabled,

    /// The adaptive storage backend failed.
    #[cfg(feature = "adaptive")]
    #[error(transparent)]
    Storage(#[from] browser_storage::StorageError),

    /// A caller-supplied regular expression failed to compile or to run.
    #[error(transparent)]
    Regex(#[from] browser_core::error::RegexError),

    /// `json()` was called on something that is not JSON.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Attribute access on a text node.
    #[error("Text nodes do not have attributes")]
    TextNodeHasNoAttributes,

    /// `selector["key"]` where `key` is not an attribute of the element.
    #[error("no attribute named `{0}`")]
    MissingAttribute(String),

    /// A [`FindQuery`](crate::find::FindQuery) with no tags, attributes,
    /// patterns or predicates was executed.
    #[error(
        "You have to pass something to search with, like tag name(s), \
         tag attributes, or both."
    )]
    EmptyFindQuery,

    /// A URL could not be joined against this document's base URL.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
}

/// Convenience alias for results carrying a [`ParseError`].
pub type Result<T, E = ParseError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_messages_are_verbatim() {
        assert_eq!(
            ParseError::NeedsContent.to_string(),
            "Selector class needs HTML content, or root arguments to work"
        );
        assert_eq!(
            ParseError::AdaptiveDisabled.to_string(),
            "Can't use `adaptive` features while it's disabled globally, \
             you have to start a new class instance."
        );
        assert_eq!(
            ParseError::TextNodeHasNoAttributes.to_string(),
            "Text nodes do not have attributes"
        );
        assert_eq!(
            ParseError::EmptyFindQuery.to_string(),
            "You have to pass something to search with, like tag name(s), tag attributes, or both."
        );
    }

    #[test]
    fn selector_errors_carry_the_whole_python_sentence() {
        let css = ParseError::CssSyntax("Invalid CSS selector '4 ayo': boom".to_owned());
        assert_eq!(css.to_string(), "Invalid CSS selector '4 ayo': boom");
        let xpath = ParseError::XPathSyntax("Invalid XPath selector: 4 ayo".to_owned());
        assert_eq!(xpath.to_string(), "Invalid XPath selector: 4 ayo");
    }
}
