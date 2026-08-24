//! Typed errors for this crate: [`CoreError`], [`HeaderParseError`], [`CookieParseError`]
//! and [`RegexError`].
//!
//! Message strings that the Python test-suite pins are reproduced **verbatim** — user
//! code and the CLI match on the text (see `ARCHITECTURE.md` §5.3). In particular
//! `"Could not parse header without colon: '<line>'."` and
//! `"Could not parse cookie string from header '<value>': <cause>"`.

use thiserror::Error;

/// A regular expression supplied by a caller could not be compiled.
///
/// Patterns go through [`fancy-regex`](fancy_regex), which understands
/// backreferences and look-around, so most patterns copied from Python keep
/// working. Constructs `fancy-regex` still rejects surface here.
#[derive(Debug, Error)]
pub enum RegexError {
    /// The pattern text is not a valid regular expression.
    #[error("invalid regular expression `{pattern}`: {message}")]
    Compile {
        /// The offending pattern, as supplied by the caller.
        pattern: String,
        /// The engine's diagnostic.
        message: String,
    },
    /// Matching failed at run time (a backtracking limit was hit, for example).
    #[error("regular expression evaluation failed: {0}")]
    Runtime(String),
}

impl RegexError {
    /// Wrap a `fancy-regex` compilation failure, remembering the pattern text.
    pub(crate) fn compile(pattern: &str, err: fancy_regex::Error) -> Self {
        Self::Compile {
            pattern: pattern.to_owned(),
            message: err.to_string(),
        }
    }
}

/// A cookie string could not be parsed.
///
/// Mirrors the failure modes of Python's `http.cookies.SimpleCookie.load()`, which
/// either silently yields nothing (a *syntactically* invalid string is discarded in
/// full) or raises `CookieError` for a key it refuses to store.
#[derive(Debug, Error)]
pub enum CookieParseError {
    /// `Morsel.set()` rejected the key: it is not made only of
    /// `http.cookies._LegalChars`.
    #[error("Illegal key {key:?}")]
    IllegalKey {
        /// The rejected cookie name.
        key: String,
    },
}

/// A raw `Name: value` header line (or its embedded cookie string) could not be parsed.
///
/// Reproduces `reference.core.utils._shell._ParseHeaders`, which raises `ValueError`
/// with these exact messages.
#[derive(Debug, Error)]
pub enum HeaderParseError {
    /// The line contains no `:` and does not end with `;`.
    #[error("Could not parse header without colon: '{line}'.")]
    MissingColon {
        /// The offending line, verbatim.
        line: String,
    },
    /// A `Cookie:` header carried a cookie string that could not be parsed.
    #[error("Could not parse cookie string from header '{value}': {source}")]
    Cookie {
        /// The header value that failed to parse.
        value: String,
        /// The underlying cookie failure.
        source: CookieParseError,
    },
}

/// The single error type of `browser-core`.
///
/// Every fallible entry point in this crate either returns one of the specific
/// enums above or this aggregate, which converts from all of them.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A caller-supplied regular expression failed.
    #[error(transparent)]
    Regex(#[from] RegexError),
    /// A header line failed to parse.
    #[error(transparent)]
    HeaderParse(#[from] HeaderParseError),
    /// A cookie string failed to parse.
    #[error(transparent)]
    CookieParse(#[from] CookieParseError),
    /// JSON encoding or decoding failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias for results carrying a [`CoreError`].
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_colon_message_is_pinned() {
        let e = HeaderParseError::MissingColon {
            line: "InvalidHeader".into(),
        };
        assert_eq!(
            e.to_string(),
            "Could not parse header without colon: 'InvalidHeader'."
        );
        // The CLI greps for this prefix.
        assert!(e
            .to_string()
            .contains("Could not parse header without colon"));
    }

    #[test]
    fn cookie_header_message_is_pinned() {
        let e = HeaderParseError::Cookie {
            value: "a=b".into(),
            source: CookieParseError::IllegalKey { key: "a,b".into() },
        };
        assert_eq!(
            e.to_string(),
            "Could not parse cookie string from header 'a=b': Illegal key \"a,b\""
        );
    }
}
