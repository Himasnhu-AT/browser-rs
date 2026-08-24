//! The typed error enum for this crate: [`XPathError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI.
//!
//! # The rule that shapes this enum
//!
//! `ARCHITECTURE.md` §1.4, risk containment item 1: **never answer wrongly.**
//! Anything the engine cannot evaluate is reported, never silently turned into an
//! empty node-set. Everything that can be decided from the expression text alone —
//! syntax, unknown axes, unknown functions, wrong arity, XPath 2.0 constructs — is
//! decided by [`crate::eval::XPath::compile`], so a user who writes XPath 2.0 gets
//! a clear error before a single node is touched. Only the three things that
//! genuinely depend on the evaluation context stay at run time: an unbound
//! `$variable`, an unbound namespace prefix, and an operator applied to a value of
//! the wrong type (`'a' | 'b'`).

/// Everything that can go wrong compiling or evaluating an XPath expression.
///
/// `Clone + PartialEq` so callers can cache and compare compile failures — the
/// `browser-parser` crate re-raises them as `SelectorSyntaxError`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum XPathError {
    /// The expression is not well-formed XPath 1.0.
    ///
    /// `position` is a byte offset into the source expression.
    #[error("invalid XPath expression at byte {position}: {message} (in `{expr}`)")]
    Syntax {
        /// The whole expression, as given.
        expr: String,
        /// Byte offset of the offending token.
        position: usize,
        /// What the parser expected.
        message: String,
    },

    /// A construct that is syntactically recognisable but that this engine does
    /// not implement — an unknown function, or XPath 2.0+ syntax.
    ///
    /// Always raised at **compile** time. See the module docs.
    #[error("unsupported XPath construct: {construct}")]
    Unsupported {
        /// Human-readable description of the construct, e.g. ``function `ends-with()` ``.
        construct: String,
    },

    /// A core function was called with the wrong number of arguments.
    #[error("{name}() takes {expected} argument(s), got {got}")]
    Arity {
        /// The function name, without parentheses.
        name: String,
        /// The accepted arity, e.g. `"1"`, `"2 or 3"`, `"at least 2"`.
        expected: String,
        /// How many arguments were supplied.
        got: usize,
    },

    /// `$name` was referenced but no binding was supplied to
    /// [`crate::eval::XPath::eval`].
    #[error("undefined XPath variable: ${name}")]
    UndefinedVariable {
        /// The variable name, including any prefix.
        name: String,
    },

    /// A `prefix:name` test, `$prefix:var` or `prefix:fn()` used a prefix that is
    /// not bound in the [`crate::eval::Namespaces`] passed to the evaluator.
    #[error("undefined namespace prefix: {prefix}")]
    UndefinedPrefix {
        /// The unbound prefix.
        prefix: String,
    },

    /// An operator or function was applied to a value of the wrong type — the
    /// only run-time type error XPath 1.0 has, since every other conversion is
    /// defined. `libxml2` reports these as `XPathEvalError: Invalid type`.
    #[error("{context} requires a node-set, got {actual}")]
    NotANodeSet {
        /// Where the requirement comes from, e.g. `"the `|` operator"`.
        context: String,
        /// The type that was supplied: `string`, `number` or `boolean`.
        actual: &'static str,
    },
}

/// `Result` with this crate's error type as the default failure mode.
pub type Result<T, E = XPathError> = std::result::Result<T, E>;

impl XPathError {
    /// Build a [`XPathError::Syntax`] for `expr` at `position`.
    pub(crate) fn syntax(expr: &str, position: usize, message: impl Into<String>) -> Self {
        Self::Syntax {
            expr: expr.to_owned(),
            position,
            message: message.into(),
        }
    }

    /// Build a [`XPathError::Unsupported`].
    pub(crate) fn unsupported(construct: impl Into<String>) -> Self {
        Self::Unsupported {
            construct: construct.into(),
        }
    }

    /// True for errors [`crate::eval::XPath::compile`] can raise.
    ///
    /// Used by the crate's own tests to assert the "fail at compile time"
    /// guarantee; also useful to callers that want to distinguish a bad
    /// expression from a bad evaluation context.
    #[must_use]
    pub fn is_compile_time(&self) -> bool {
        matches!(
            self,
            Self::Syntax { .. } | Self::Unsupported { .. } | Self::Arity { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_problem() {
        let e = XPathError::syntax("4 ayo", 2, "expected end of expression");
        assert_eq!(
            e.to_string(),
            "invalid XPath expression at byte 2: expected end of expression (in `4 ayo`)"
        );
        assert!(e.is_compile_time());

        let e = XPathError::unsupported("function `ends-with()`");
        assert_eq!(
            e.to_string(),
            "unsupported XPath construct: function `ends-with()`"
        );
        assert!(e.is_compile_time());

        let e = XPathError::UndefinedVariable {
            name: "cell_text".into(),
        };
        assert_eq!(e.to_string(), "undefined XPath variable: $cell_text");
        assert!(!e.is_compile_time());

        let e = XPathError::Arity {
            name: "concat".into(),
            expected: "at least 2".into(),
            got: 0,
        };
        assert_eq!(
            e.to_string(),
            "concat() takes at least 2 argument(s), got 0"
        );

        let e = XPathError::NotANodeSet {
            context: "the `|` operator".into(),
            actual: "string",
        };
        assert_eq!(
            e.to_string(),
            "the `|` operator requires a node-set, got string"
        );
    }
}
