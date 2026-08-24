//! The XPath 1.0 tokeniser, including the disambiguation rules that make `*`,
//! `and`, `or`, `div`, `mod`, `.` and `..` unambiguous.
//!
//! # The four disambiguation rules (XPath 1.0 §3.7)
//!
//! 1. If there is a preceding token and it is **not** one of `@`, `::`, `(`, `[`,
//!    `,` or an operator, then `*` is the multiply operator and an `NCName` that
//!    spells `and`/`or`/`div`/`mod` is that operator. Otherwise `*` is a wildcard
//!    name test and those four names are ordinary names.
//! 2. If the character following an `NCName` (ignoring whitespace) is `(`, the name
//!    is a node type or a function name.
//! 3. If the two characters following an `NCName` (ignoring whitespace) are `::`,
//!    the name is an axis name.
//! 4. Otherwise it is a plain name.
//!
//! Rule 1 is applied first, so `a mod(3)` is `a` *mod* `(3)` and never a call to a
//! function named `mod` — which is what `libxml2` does.
//!
//! # Deviations from the letter of the grammar
//!
//! `Number` also accepts an exponent (`1e5`, `1E-2`). XPath 1.0 has no exponent
//! syntax, but `libxml2` — the engine this port replaces — accepts one, and
//! accepting a superset can never change the meaning of a valid XPath 1.0
//! expression. [`crate::value::parse_number`] matches it for `number('1e5')`.

use crate::error::{Result, XPathError};

/// One lexical token.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// `/`
    Slash,
    /// `//`
    DoubleSlash,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `@`
    At,
    /// `,`
    Comma,
    /// `::`
    DoubleColon,
    /// `.`
    Dot,
    /// `..`
    DotDot,
    /// `|`
    Pipe,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*` in operator position.
    Multiply,
    /// `*` in node-test position — the wildcard name test.
    Star,
    /// `=`
    Equal,
    /// `!=`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterEqual,
    /// `and`
    And,
    /// `or`
    Or,
    /// `div`
    Div,
    /// `mod`
    Mod,
    /// A quoted string, with the quotes removed.
    Literal(String),
    /// A numeric literal.
    Number(f64),
    /// `$name` — the name is stored without the `$`.
    Variable(String),
    /// A `QName` used as a name: `div`, `svg:rect`.
    Name(String),
    /// `prefix:*`, with the prefix stored.
    PrefixedStar(String),
    /// A name immediately followed by `(`.
    Function(String),
    /// A name immediately followed by `::`.
    Axis(String),
}

impl Token {
    /// Whether this token is an `Operator` in the sense of §3.7's rule 1.
    #[must_use]
    pub fn is_operator(&self) -> bool {
        matches!(
            self,
            Self::Slash
                | Self::DoubleSlash
                | Self::Pipe
                | Self::Plus
                | Self::Minus
                | Self::Multiply
                | Self::And
                | Self::Or
                | Self::Div
                | Self::Mod
                | Self::Equal
                | Self::NotEqual
                | Self::Less
                | Self::LessEqual
                | Self::Greater
                | Self::GreaterEqual
        )
    }

    /// Whether a `*` or an `and`/`or`/`div`/`mod` that *follows* this token is a
    /// name rather than an operator (§3.7 rule 1).
    fn allows_name_test_after(&self) -> bool {
        matches!(
            self,
            Self::At | Self::DoubleColon | Self::LParen | Self::LBracket | Self::Comma
        ) || self.is_operator()
    }

    /// The token as it appeared in the source, for error messages.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Slash => "`/`".into(),
            Self::DoubleSlash => "`//`".into(),
            Self::LParen => "`(`".into(),
            Self::RParen => "`)`".into(),
            Self::LBracket => "`[`".into(),
            Self::RBracket => "`]`".into(),
            Self::At => "`@`".into(),
            Self::Comma => "`,`".into(),
            Self::DoubleColon => "`::`".into(),
            Self::Dot => "`.`".into(),
            Self::DotDot => "`..`".into(),
            Self::Pipe => "`|`".into(),
            Self::Plus => "`+`".into(),
            Self::Minus => "`-`".into(),
            Self::Multiply => "`*` (multiply)".into(),
            Self::Star => "`*`".into(),
            Self::Equal => "`=`".into(),
            Self::NotEqual => "`!=`".into(),
            Self::Less => "`<`".into(),
            Self::LessEqual => "`<=`".into(),
            Self::Greater => "`>`".into(),
            Self::GreaterEqual => "`>=`".into(),
            Self::And => "`and`".into(),
            Self::Or => "`or`".into(),
            Self::Div => "`div`".into(),
            Self::Mod => "`mod`".into(),
            Self::Literal(value) => format!("string literal `{value}`"),
            Self::Number(value) => format!("number `{value}`"),
            Self::Variable(name) => format!("`${name}`"),
            Self::Name(name) => format!("name `{name}`"),
            Self::PrefixedStar(prefix) => format!("`{prefix}:*`"),
            Self::Function(name) => format!("`{name}(`"),
            Self::Axis(name) => format!("axis `{name}::`"),
        }
    }
}

/// A token plus the byte offset it starts at.
#[derive(Clone, Debug, PartialEq)]
pub struct Spanned {
    /// The token.
    pub token: Token,
    /// Byte offset of the token's first character in the source expression.
    pub position: usize,
}

/// Tokenise a whole expression.
///
/// ```
/// use browser_xpath::lexer::{tokenize, Token};
///
/// let tokens = tokenize("//a[@href]")?;
/// assert_eq!(tokens[0].token, Token::DoubleSlash);
/// assert_eq!(tokens[1].token, Token::Name("a".into()));
/// # Ok::<(), browser_xpath::error::XPathError>(())
/// ```
pub fn tokenize(expr: &str) -> Result<Vec<Spanned>> {
    Lexer::new(expr).run()
}

/// The tokeniser state.
struct Lexer<'a> {
    expr: &'a str,
    chars: Vec<(usize, char)>,
    index: usize,
    tokens: Vec<Spanned>,
}

impl<'a> Lexer<'a> {
    fn new(expr: &'a str) -> Self {
        Self {
            expr,
            chars: expr.char_indices().collect(),
            index: 0,
            tokens: Vec::new(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).map(|(_, c)| *c)
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.index + ahead).map(|(_, c)| *c)
    }

    fn offset(&self) -> usize {
        self.chars
            .get(self.index)
            .map_or(self.expr.len(), |(offset, _)| *offset)
    }

    fn bump(&mut self) -> Option<char> {
        let current = self.peek();
        if current.is_some() {
            self.index += 1;
        }
        current
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
            self.index += 1;
        }
    }

    /// The next non-whitespace characters, for rules 2 and 3.
    fn lookahead_non_space(&self) -> (Option<char>, Option<char>) {
        let mut cursor = self.index;
        while matches!(
            self.chars.get(cursor).map(|(_, c)| *c),
            Some(' ' | '\t' | '\r' | '\n')
        ) {
            cursor += 1;
        }
        (
            self.chars.get(cursor).map(|(_, c)| *c),
            self.chars.get(cursor + 1).map(|(_, c)| *c),
        )
    }

    fn push(&mut self, token: Token, position: usize) {
        self.tokens.push(Spanned { token, position });
    }

    fn error(&self, position: usize, message: impl Into<String>) -> XPathError {
        XPathError::syntax(self.expr, position, message)
    }

    /// Whether a `*` or an operator name here is an operator (§3.7 rule 1).
    fn in_operator_position(&self) -> bool {
        match self.tokens.last() {
            None => false,
            Some(previous) => !previous.token.allows_name_test_after(),
        }
    }

    fn run(mut self) -> Result<Vec<Spanned>> {
        loop {
            self.skip_whitespace();
            let position = self.offset();
            let Some(current) = self.peek() else { break };
            match current {
                '(' => {
                    self.bump();
                    self.push(Token::LParen, position);
                }
                ')' => {
                    self.bump();
                    self.push(Token::RParen, position);
                }
                '[' => {
                    self.bump();
                    self.push(Token::LBracket, position);
                }
                ']' => {
                    self.bump();
                    self.push(Token::RBracket, position);
                }
                '@' => {
                    self.bump();
                    self.push(Token::At, position);
                }
                ',' => {
                    self.bump();
                    self.push(Token::Comma, position);
                }
                '+' => {
                    self.bump();
                    self.push(Token::Plus, position);
                }
                '-' => {
                    self.bump();
                    self.push(Token::Minus, position);
                }
                '=' => {
                    self.bump();
                    self.push(Token::Equal, position);
                }
                '|' => {
                    self.bump();
                    self.push(Token::Pipe, position);
                }
                '*' => {
                    self.bump();
                    let token = if self.in_operator_position() {
                        Token::Multiply
                    } else {
                        Token::Star
                    };
                    self.push(token, position);
                }
                '/' => {
                    self.bump();
                    if self.peek() == Some('/') {
                        self.bump();
                        self.push(Token::DoubleSlash, position);
                    } else {
                        self.push(Token::Slash, position);
                    }
                }
                '!' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        self.push(Token::NotEqual, position);
                    } else {
                        return Err(self.error(position, "expected `!=`"));
                    }
                }
                '<' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        self.push(Token::LessEqual, position);
                    } else {
                        self.push(Token::Less, position);
                    }
                }
                '>' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        self.push(Token::GreaterEqual, position);
                    } else {
                        self.push(Token::Greater, position);
                    }
                }
                '.' => {
                    if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
                        let number = self.read_number()?;
                        self.push(Token::Number(number), position);
                    } else {
                        self.bump();
                        if self.peek() == Some('.') {
                            self.bump();
                            self.push(Token::DotDot, position);
                        } else {
                            self.push(Token::Dot, position);
                        }
                    }
                }
                ':' => {
                    if self.peek_at(1) == Some(':') {
                        self.bump();
                        self.bump();
                        self.push(Token::DoubleColon, position);
                    } else {
                        return Err(self.error(position, "unexpected `:`"));
                    }
                }
                '\'' | '"' => {
                    let literal = self.read_literal(current)?;
                    self.push(Token::Literal(literal), position);
                }
                '$' => {
                    self.bump();
                    let name_position = self.offset();
                    let name = self.read_qname().ok_or_else(|| {
                        self.error(name_position, "expected a variable name after `$`")
                    })?;
                    self.push(Token::Variable(name), position);
                }
                c if c.is_ascii_digit() => {
                    let number = self.read_number()?;
                    self.push(Token::Number(number), position);
                }
                c if is_name_start(c) => {
                    let name = self
                        .read_qname_or_wildcard()
                        .ok_or_else(|| self.error(position, "expected a name"))?;
                    self.push_name(name, position)?;
                }
                other => {
                    return Err(self.error(position, format!("unexpected character `{other}`")));
                }
            }
        }
        Ok(self.tokens)
    }

    /// Apply §3.7's rules 1-4 to a freshly-read name.
    fn push_name(&mut self, name: NameToken, position: usize) -> Result<()> {
        match name {
            NameToken::Wildcard(prefix) => {
                self.push(Token::PrefixedStar(prefix), position);
                Ok(())
            }
            NameToken::Name(name) => {
                // Rule 1 first: in operator position the four operator names win,
                // whatever follows them.
                if self.in_operator_position() {
                    let token = match name.as_str() {
                        "and" => Some(Token::And),
                        "or" => Some(Token::Or),
                        "div" => Some(Token::Div),
                        "mod" => Some(Token::Mod),
                        _ => None,
                    };
                    if let Some(token) = token {
                        self.push(token, position);
                        return Ok(());
                    }
                    // Any other name here is a syntax error — but the *parser*
                    // reports it, so that `1 to 5` and `a eq b` can be named as
                    // the XPath 2.0 constructs they are instead of dying here.
                    self.push(Token::Name(name), position);
                    return Ok(());
                }
                // Rules 2 and 3.
                let (next, after) = self.lookahead_non_space();
                if next == Some('(') {
                    self.push(Token::Function(name), position);
                } else if next == Some(':') && after == Some(':') {
                    self.push(Token::Axis(name), position);
                } else {
                    self.push(Token::Name(name), position);
                }
                Ok(())
            }
        }
    }

    fn read_literal(&mut self, quote: char) -> Result<String> {
        let open = self.offset();
        self.bump();
        let mut value = String::new();
        loop {
            match self.bump() {
                Some(c) if c == quote => return Ok(value),
                Some(c) => value.push(c),
                None => return Err(self.error(open, "unterminated string literal")),
            }
        }
    }

    /// `Digits ('.' Digits?)? | '.' Digits`, plus `libxml2`'s exponent extension.
    fn read_number(&mut self) -> Result<f64> {
        let start = self.offset();
        let mut text = String::new();
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            text.push(self.bump().unwrap_or('0'));
        }
        if self.peek() == Some('.') {
            text.push('.');
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                text.push(self.bump().unwrap_or('0'));
            }
        }
        if matches!(self.peek(), Some('e' | 'E'))
            && (self.peek_at(1).is_some_and(|c| c.is_ascii_digit())
                || (matches!(self.peek_at(1), Some('+' | '-'))
                    && self.peek_at(2).is_some_and(|c| c.is_ascii_digit())))
        {
            text.push('e');
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                text.push(self.bump().unwrap_or('+'));
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                text.push(self.bump().unwrap_or('0'));
            }
        }
        // `1.` and `.5` both parse in Rust; an empty mantissa cannot occur here.
        text.parse::<f64>()
            .map_err(|_| self.error(start, format!("invalid number `{text}`")))
    }

    /// An `NCName`, or `None` if the cursor is not on a name start character.
    fn read_ncname(&mut self) -> Option<String> {
        if !self.peek().is_some_and(is_name_start) {
            return None;
        }
        let mut name = String::new();
        while self.peek().is_some_and(is_name_char) {
            name.push(self.bump()?);
        }
        Some(name)
    }

    /// A `QName`: `local` or `prefix:local`.
    fn read_qname(&mut self) -> Option<String> {
        let mut name = self.read_ncname()?;
        if self.peek() == Some(':') && self.peek_at(1).is_some_and(is_name_start) {
            self.bump();
            let local = self.read_ncname()?;
            name.push(':');
            name.push_str(&local);
        }
        Some(name)
    }

    /// A `QName` or the `prefix:*` form of a name test.
    fn read_qname_or_wildcard(&mut self) -> Option<NameToken> {
        let name = self.read_ncname()?;
        if self.peek() == Some(':') && self.peek_at(1) == Some('*') {
            self.bump();
            self.bump();
            return Some(NameToken::Wildcard(name));
        }
        if self.peek() == Some(':') && self.peek_at(1).is_some_and(is_name_start) {
            self.bump();
            let local = self.read_ncname()?;
            return Some(NameToken::Name(format!("{name}:{local}")));
        }
        Some(NameToken::Name(name))
    }
}

/// What [`Lexer::read_qname_or_wildcard`] found.
enum NameToken {
    Name(String),
    Wildcard(String),
}

/// XML `NameStartChar`, approximated with Unicode letters plus `_`.
fn is_name_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

/// XML `NameChar`.
fn is_name_char(c: char) -> bool {
    c == '_'
        || c == '-'
        || c == '.'
        || c == '\u{00B7}'
        || c.is_alphanumeric()
        || matches!(c, '\u{0300}'..='\u{036F}' | '\u{203F}' | '\u{2040}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(expr: &str) -> Vec<Token> {
        tokenize(expr)
            .expect("lexes")
            .into_iter()
            .map(|s| s.token)
            .collect()
    }

    #[test]
    fn punctuation_and_paths() {
        assert_eq!(
            lex("//a[@href]/text()"),
            vec![
                Token::DoubleSlash,
                Token::Name("a".into()),
                Token::LBracket,
                Token::At,
                Token::Name("href".into()),
                Token::RBracket,
                Token::Slash,
                Token::Function("text".into()),
                Token::LParen,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn star_is_a_wildcard_only_in_node_test_position() {
        assert_eq!(lex("*"), vec![Token::Star]);
        assert_eq!(lex("//*"), vec![Token::DoubleSlash, Token::Star]);
        assert_eq!(lex("@*"), vec![Token::At, Token::Star]);
        assert_eq!(
            lex("child::*"),
            vec![Token::Axis("child".into()), Token::DoubleColon, Token::Star]
        );
        assert_eq!(
            lex("[*]"),
            vec![Token::LBracket, Token::Star, Token::RBracket]
        );
        assert_eq!(
            lex("2 * 3"),
            vec![Token::Number(2.0), Token::Multiply, Token::Number(3.0)]
        );
        assert_eq!(
            lex("count(*) * 2"),
            vec![
                Token::Function("count".into()),
                Token::LParen,
                Token::Star,
                Token::RParen,
                Token::Multiply,
                Token::Number(2.0),
            ]
        );
        assert_eq!(
            lex("* * *"),
            vec![Token::Star, Token::Multiply, Token::Star]
        );
        assert_eq!(lex("svg:*"), vec![Token::PrefixedStar("svg".into())]);
    }

    #[test]
    fn operator_names_are_names_in_node_test_position() {
        // `div div div` — an element named div, divided by an element named div.
        assert_eq!(
            lex("div div div"),
            vec![
                Token::Name("div".into()),
                Token::Div,
                Token::Name("div".into())
            ]
        );
        assert_eq!(
            lex("//and/or/mod"),
            vec![
                Token::DoubleSlash,
                Token::Name("and".into()),
                Token::Slash,
                Token::Name("or".into()),
                Token::Slash,
                Token::Name("mod".into()),
            ]
        );
        assert_eq!(
            lex("a and b"),
            vec![Token::Name("a".into()), Token::And, Token::Name("b".into())]
        );
        // Rule 1 beats rule 2: `mod(` in operator position is still the operator.
        assert_eq!(
            lex("a mod(3)"),
            vec![
                Token::Name("a".into()),
                Token::Mod,
                Token::LParen,
                Token::Number(3.0),
                Token::RParen,
            ]
        );
        // …and a name in operator position that is not one of the four is left
        // for the parser to reject (it can name XPath 2.0 constructs).
        assert_eq!(
            lex("a b"),
            vec![Token::Name("a".into()), Token::Name("b".into())]
        );
        assert_eq!(
            lex("1 to 5"),
            vec![
                Token::Number(1.0),
                Token::Name("to".into()),
                Token::Number(5.0)
            ]
        );
    }

    #[test]
    fn dots_axes_and_functions() {
        assert_eq!(lex("."), vec![Token::Dot]);
        assert_eq!(lex(".."), vec![Token::DotDot]);
        assert_eq!(lex(".5"), vec![Token::Number(0.5)]);
        assert_eq!(
            lex("../@id"),
            vec![
                Token::DotDot,
                Token::Slash,
                Token::At,
                Token::Name("id".into())
            ]
        );
        assert_eq!(
            lex("ancestor-or-self :: node ( )"),
            vec![
                Token::Axis("ancestor-or-self".into()),
                Token::DoubleColon,
                Token::Function("node".into()),
                Token::LParen,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn numbers_literals_and_variables() {
        assert_eq!(lex("1"), vec![Token::Number(1.0)]);
        assert_eq!(lex("1.5"), vec![Token::Number(1.5)]);
        assert_eq!(lex("1."), vec![Token::Number(1.0)]);
        assert_eq!(lex("1e2"), vec![Token::Number(100.0)]);
        assert_eq!(lex("1E-2"), vec![Token::Number(0.01)]);
        assert_eq!(
            lex("'it''s'"),
            vec![Token::Literal("it".into()), Token::Literal("s".into())]
        );
        assert_eq!(lex("\"a'b\""), vec![Token::Literal("a'b".into())]);
        assert_eq!(lex("''"), vec![Token::Literal(String::new())]);
        assert_eq!(lex("$cell_text"), vec![Token::Variable("cell_text".into())]);
        assert_eq!(lex("$ns:v"), vec![Token::Variable("ns:v".into())]);
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(
            lex("@a>=1 and @b!=2 or @c<=3"),
            vec![
                Token::At,
                Token::Name("a".into()),
                Token::GreaterEqual,
                Token::Number(1.0),
                Token::And,
                Token::At,
                Token::Name("b".into()),
                Token::NotEqual,
                Token::Number(2.0),
                Token::Or,
                Token::At,
                Token::Name("c".into()),
                Token::LessEqual,
                Token::Number(3.0),
            ]
        );
    }

    #[test]
    fn positions_point_at_the_token() {
        let tokens = tokenize("  //a").expect("lexes");
        assert_eq!(tokens[0].position, 2);
        assert_eq!(tokens[1].position, 4);
    }

    #[test]
    fn errors_are_reported_with_a_position() {
        for (expr, needle) in [
            ("'unterminated", "unterminated string literal"),
            ("a ! b", "expected `!=`"),
            ("$", "expected a variable name"),
            ("#", "unexpected character `#`"),
            ("a:b:c", "unexpected `:`"),
        ] {
            let error = tokenize(expr).expect_err(expr);
            assert!(
                error.to_string().contains(needle),
                "{expr}: {error} does not mention {needle}"
            );
        }
    }

    #[test]
    fn unicode_names_lex() {
        assert_eq!(
            lex("//日本語"),
            vec![Token::DoubleSlash, Token::Name("日本語".into())]
        );
        assert_eq!(
            lex("//a.b-c_d"),
            vec![Token::DoubleSlash, Token::Name("a.b-c_d".into())]
        );
    }
}
