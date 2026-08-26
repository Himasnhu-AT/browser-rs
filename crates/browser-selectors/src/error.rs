//! The typed error enum for this crate: `SelectorError`.
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Python raises two distinct exceptions from `cssselect`:
//!
//! * `SelectorSyntaxError` — the token stream is not a selector at all
//!   (`page.css("4 ayo")`), and
//! * `ExpressionError` — the selector parses but names something the translator
//!   does not implement (`a::data(x)`, `a::foo`).
//!
//! Both are subclasses of `cssselect.SelectorError`, and `Selector.css` catches
//! both and re-raises `SelectorSyntaxError(f"Invalid CSS selector '{selector}': {e}")`
//! (`parser.md` §3.4). This enum is the union of the two; `browser-parser` does
//! the outer wrapping, so the `Display` of every variant here is only the inner
//! `{e}` part.

use cssparser::{BasicParseErrorKind, ParseError, ParseErrorKind, ToCss};
use selectors::parser::SelectorParseErrorKind;
use thiserror::Error;

/// Everything that can go wrong while parsing a CSS selector.
///
/// The `Display` strings of the `Unknown*`/`Bad*` variants are copied verbatim
/// from `cssselect` so that the message a user finally sees is the one
/// `parser.md` §6.2 and §9 pin.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SelectorError {
    /// The input is not a syntactically valid selector.
    ///
    /// Python: `cssselect.SelectorSyntaxError`.
    #[error("{0}")]
    Syntax(String),

    /// The input contained no selector at all (`""`, `"   "`).
    #[error("Expected selector, got <EOF>")]
    Empty,

    /// Internal: `selectors` found a compound selector with no components.
    ///
    /// Never surfaces to callers — the parser rewrites it into
    /// [`SelectorError::Syntax`] naming the offending token, or into
    /// [`SelectorError::Empty`] when there is no token to name.
    #[error("Expected selector")]
    EmptyCompound,

    /// `::foo` — a plain pseudo-element this port does not implement.
    ///
    /// Python: `ExpressionError("The pseudo-element ::foo is unknown")`.
    #[error("The pseudo-element ::{0} is unknown")]
    UnknownPseudoElement(String),

    /// `::data(x)` — a *functional* pseudo-element this port does not implement.
    ///
    /// `::data()` deliberately does not exist: `parser.md` §6.2 states that
    /// The reference implementation 0.4.15 adds only `::text` and `::attr()` on top of cssselect.
    ///
    /// Python: `ExpressionError("The functional pseudo-element ::data() is unknown")`.
    #[error("The functional pseudo-element ::{0}() is unknown")]
    UnknownFunctionalPseudoElement(String),

    /// `:foo` — a pseudo-class cssselect does not define either.
    #[error("The pseudo-class :{0} is unknown")]
    UnknownPseudoClass(String),

    /// `:foo(x)` — a functional pseudo-class cssselect does not define either.
    #[error("The pseudo-class :{0}() is unknown")]
    UnknownFunctionalPseudoClass(String),

    /// `::attr()` did not receive exactly one `IDENT` or `STRING` token.
    ///
    /// Python: `ExpressionError("Expected a single string or ident for ::attr(), got …")`.
    #[error("Expected a single string or ident for ::attr(), got {0}")]
    BadAttrArgument(String),

    /// `:contains()` / `:lang()` did not receive exactly one `IDENT` or `STRING`.
    #[error("Expected a single string or ident for :{name}(), got {got}")]
    BadPseudoClassArgument {
        /// The pseudo-class name, without the leading colon.
        name: String,
        /// A rendering of what was actually passed.
        got: String,
    },
}

impl SelectorError {
    /// `true` for the variants Python raises as `cssselect.ExpressionError`
    /// rather than `cssselect.SelectorSyntaxError`.
    ///
    /// `Selector.css` treats them identically, but `Selector.xpath` does not,
    /// so the distinction is preserved for `browser-parser`.
    #[must_use]
    pub fn is_expression_error(&self) -> bool {
        matches!(
            self,
            Self::UnknownPseudoElement(_)
                | Self::UnknownFunctionalPseudoElement(_)
                | Self::UnknownPseudoClass(_)
                | Self::UnknownFunctionalPseudoClass(_)
                | Self::BadAttrArgument(_)
                | Self::BadPseudoClassArgument { .. }
        )
    }

    /// Flatten a `cssparser` error into this enum, keeping our own custom
    /// payloads intact and rendering the library's own kinds as `Syntax`.
    ///
    /// `source` is the selector text, used to name the offending token the way
    /// cssselect does (`"Expected selector, got <NUMBER '4' at 0>"`).
    pub(crate) fn from_parse_error(source: &str, err: ParseError<'_, Self>) -> Self {
        let location = err.location;
        match err.kind {
            // `selectors` reports "the compound selector at this position is
            // empty" for anything that cannot start a selector at all, and
            // gives us no token. Recover it from the source so the message is
            // as useful as cssselect's.
            ParseErrorKind::Custom(Self::EmptyCompound) => {
                match token_at(source, location.line, location.column) {
                    Some(token) => Self::Syntax(format!("Expected selector, got {token}")),
                    None => Self::Empty,
                }
            }
            ParseErrorKind::Custom(e) => e,
            ParseErrorKind::Basic(basic) => Self::Syntax(render_basic(&basic)),
        }
    }
}

/// Render the first token at a 1-based (line, column) position of `source`.
fn token_at(source: &str, line: u32, column: u32) -> Option<String> {
    let mut offset = 0usize;
    for _ in 0..line {
        offset += source[offset..].find('\n')? + 1;
    }
    // `column` counts characters from 1; selectors are short, so walking is fine.
    let rest = &source[offset..];
    let start = rest
        .char_indices()
        .nth(column.saturating_sub(1) as usize)
        .map_or(rest.len(), |(i, _)| i);
    let mut input = cssparser::ParserInput::new(&rest[start..]);
    let mut parser = cssparser::Parser::new(&mut input);
    let token = parser.next().ok()?;
    Some(render_token(token))
}

fn render_basic(kind: &BasicParseErrorKind<'_>) -> String {
    match kind {
        BasicParseErrorKind::UnexpectedToken(t) => {
            format!("Expected selector, got {}", render_token(t))
        }
        BasicParseErrorKind::EndOfInput => "Expected selector, got <EOF>".to_owned(),
        BasicParseErrorKind::AtRuleInvalid(name) => format!("Unexpected at-rule @{name}"),
        BasicParseErrorKind::AtRuleBodyInvalid => "Invalid at-rule body".to_owned(),
        BasicParseErrorKind::QualifiedRuleInvalid => "Invalid qualified rule".to_owned(),
    }
}

fn render_token(token: &cssparser::Token<'_>) -> String {
    let mut out = String::new();
    if token.to_css(&mut out).is_err() {
        return format!("{token:?}");
    }
    out
}

impl<'i> From<SelectorParseErrorKind<'i>> for SelectorError {
    fn from(kind: SelectorParseErrorKind<'i>) -> Self {
        use SelectorParseErrorKind as K;
        match kind {
            K::EmptySelector => Self::EmptyCompound,
            K::NoQualifiedNameInAttributeSelector(t) => Self::Syntax(format!(
                "Expected a qualified name in the attribute selector, got {}",
                render_token(&t)
            )),
            K::DanglingCombinator => Self::Syntax("Expected selector, got <EOF>".to_owned()),
            K::NonCompoundSelector => {
                Self::Syntax("Expected a compound selector, got a complex one".to_owned())
            }
            K::NonPseudoElementAfterSlotted | K::InvalidPseudoElementAfterSlotted => {
                Self::Syntax("Invalid pseudo-element after ::slotted()".to_owned())
            }
            K::InvalidPseudoElementInsideWhere => {
                Self::Syntax("Pseudo-elements are not allowed inside :is()/:where()".to_owned())
            }
            K::InvalidState => Self::Syntax(
                "Got pseudo-element not at the end of a selector, or a selector after it"
                    .to_owned(),
            ),
            K::UnexpectedTokenInAttributeSelector(t) => Self::Syntax(format!(
                "Expected ']', got {} in the attribute selector",
                render_token(&t)
            )),
            K::PseudoElementExpectedColon(t) => {
                Self::Syntax(format!("Expected ':', got {}", render_token(&t)))
            }
            K::PseudoElementExpectedIdent(t) | K::NoIdentForPseudo(t) => Self::Syntax(format!(
                "Expected a pseudo-class or pseudo-element name, got {}",
                render_token(&t)
            )),
            K::UnsupportedPseudoClassOrElement(name) => Self::UnknownPseudoClass(name.to_string()),
            K::UnexpectedIdent(name) => Self::Syntax(format!("Unexpected ident {name}")),
            K::ExpectedNamespace(name) => {
                Self::Syntax(format!("Undefined namespace prefix {name}"))
            }
            K::ExpectedBarInAttr(t) => Self::Syntax(format!(
                "Expected '|', got {} in the attribute selector",
                render_token(&t)
            )),
            K::BadValueInAttr(t) => Self::Syntax(format!(
                "Expected a string or ident, got {} in the attribute selector",
                render_token(&t)
            )),
            K::InvalidQualNameInAttr(t) => Self::Syntax(format!(
                "Expected a qualified name, got {} in the attribute selector",
                render_token(&t)
            )),
            K::ExplicitNamespaceUnexpectedToken(t) => Self::Syntax(format!(
                "Expected a name after '|', got {}",
                render_token(&t)
            )),
            K::ClassNeedsIdent(t) => {
                Self::Syntax(format!("Expected a class name, got {}", render_token(&t)))
            }
        }
    }
}

/// The crate-local `Result` alias.
pub type Result<T, E = SelectorError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_errors_are_flagged() {
        assert!(SelectorError::UnknownPseudoElement("foo".into()).is_expression_error());
        assert!(!SelectorError::Syntax("boom".into()).is_expression_error());
    }

    #[test]
    fn messages_match_cssselect() {
        assert_eq!(
            SelectorError::UnknownPseudoElement("foo".into()).to_string(),
            "The pseudo-element ::foo is unknown"
        );
        assert_eq!(
            SelectorError::UnknownFunctionalPseudoElement("data".into()).to_string(),
            "The functional pseudo-element ::data() is unknown"
        );
        assert_eq!(
            SelectorError::BadAttrArgument("[]".into()).to_string(),
            "Expected a single string or ident for ::attr(), got []"
        );
        assert_eq!(
            SelectorError::BadPseudoClassArgument {
                name: "contains".into(),
                got: "[]".into()
            }
            .to_string(),
            "Expected a single string or ident for :contains(), got []"
        );
    }
}
