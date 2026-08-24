//! A recursive-descent / precedence-climbing parser producing the [`crate::ast`],
//! rejecting everything it cannot evaluate at **compile** time.
//!
//! The precedence ladder, lowest first — exactly XPath 1.0 §3.4's grammar:
//!
//! | level | production | operators |
//! |---|---|---|
//! | 1 | `OrExpr` | `or` |
//! | 2 | `AndExpr` | `and` |
//! | 3 | `EqualityExpr` | `=` `!=` |
//! | 4 | `RelationalExpr` | `<` `<=` `>` `>=` |
//! | 5 | `AdditiveExpr` | `+` `-` |
//! | 6 | `MultiplicativeExpr` | `*` `div` `mod` |
//! | 7 | `UnaryExpr` | prefix `-` |
//! | 8 | `UnionExpr` | `\|` |
//! | 9 | `PathExpr` | `/` `//` and predicates |
//!
//! # What is rejected, and how
//!
//! * an unknown function name, an unknown axis, an extension function
//!   (`my:thing()`), a parenthesised step, `for`/`some`/`every`/`to` and the other
//!   XPath 2.0 keywords → [`crate::error::XPathError::Unsupported`];
//! * a wrong argument count for a core function →
//!   [`crate::error::XPathError::Arity`];
//! * anything else that does not fit the grammar →
//!   [`crate::error::XPathError::Syntax`], with a byte offset.
//!
//! All three are raised before a single node is touched, which is the guarantee
//! `ARCHITECTURE.md` §1.4 asks for: a user who writes XPath 2.0 gets an error, not
//! an empty node-set.

use crate::ast::{Axis, BinaryOp, CoreFunction, Expr, NodeTest, Path, PathStart, Step};
use crate::error::{Result, XPathError};
use crate::lexer::{tokenize, Spanned, Token};

/// Names that XPath 2.0 and later give a meaning that XPath 1.0 does not have.
/// Recognised only to produce a better error than "unexpected name".
const LATER_VERSION_KEYWORDS: &[&str] = &[
    "for",
    "some",
    "every",
    "in",
    "return",
    "satisfies",
    "if",
    "then",
    "else",
    "to",
    "eq",
    "ne",
    "lt",
    "le",
    "gt",
    "ge",
    "is",
    "union",
    "intersect",
    "except",
    "idiv",
    "instance",
    "castable",
    "cast",
    "treat",
];

/// Parse an expression into an AST.
///
/// ```
/// use browser_xpath::parser::parse;
///
/// // Every abbreviation is expanded.
/// assert_eq!(
///     parse("//a/@href")?.to_string(),
///     "/descendant-or-self::node()/child::a/attribute::href"
/// );
/// # Ok::<(), browser_xpath::error::XPathError>(())
/// ```
///
/// # Errors
///
/// See the module documentation.
pub fn parse(expr: &str) -> Result<Expr> {
    let tokens = tokenize(expr)?;
    let mut parser = Parser {
        expr,
        tokens,
        index: 0,
        depth: 0,
    };
    let parsed = parser.parse_expr()?;
    if let Some(spanned) = parser.peek_spanned() {
        let position = spanned.position;
        // A bare name where an operator belongs is the shape every XPath 2.0
        // keyword expression has: `1 to 5`, `for $x in …`, `a eq b`.
        if let Token::Name(name) = &spanned.token {
            if LATER_VERSION_KEYWORDS.contains(&name.as_str()) {
                return Err(XPathError::unsupported(format!(
                    "`{name}` (XPath 2.0 or later)"
                )));
            }
            return Err(parser.error(position, format!("expected an operator before `{name}`")));
        }
        let describe = spanned.token.describe();
        return Err(parser.error(
            position,
            format!("expected end of expression, found {describe}"),
        ));
    }
    Ok(parsed)
}

/// How deeply expressions may nest.
///
/// The parser is recursive descent, so `((((…))))`, `not(not(not(…)))` and
/// `- - - -x` all consume stack. 200 is far beyond any real selector — the
/// deepest expression in the whole corpus nests 6 — and keeps a hostile
/// expression from aborting the process. `libxml2` has the same guard
/// (`XPATH_MAX_RECURSION_DEPTH`).
const MAX_NESTING_DEPTH: usize = 200;

/// The parser state.
struct Parser<'a> {
    expr: &'a str,
    tokens: Vec<Spanned>,
    index: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    // -- token plumbing ----------------------------------------------------

    fn peek_spanned(&self) -> Option<&Spanned> {
        self.tokens.get(self.index)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index).map(|s| &s.token)
    }

    fn position(&self) -> usize {
        self.tokens
            .get(self.index)
            .map_or(self.expr.len(), |s| s.position)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).map(|s| s.token.clone());
        if token.is_some() {
            self.index += 1;
        }
        token
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token) -> Result<()> {
        if self.eat(token) {
            return Ok(());
        }
        let found = self
            .peek()
            .map_or_else(|| "end of expression".to_owned(), Token::describe);
        let expected = token.describe();
        Err(self.error(
            self.position(),
            format!("expected {expected}, found {found}"),
        ))
    }

    fn error(&self, position: usize, message: impl Into<String>) -> XPathError {
        XPathError::syntax(self.expr, position, message)
    }

    // -- expressions -------------------------------------------------------

    /// One more level of nesting, or a syntax error.
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_NESTING_DEPTH {
            return Err(self.error(self.position(), "expression nests too deeply"));
        }
        Ok(())
    }

    /// `Expr ::= OrExpr`
    fn parse_expr(&mut self) -> Result<Expr> {
        self.enter()?;
        let parsed = self.parse_binary(1);
        self.depth -= 1;
        parsed
    }

    /// Precedence climbing over the six left-associative binary levels.
    fn parse_binary(&mut self, min_precedence: u8) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        while let Some((op, precedence)) = self.peek().and_then(binary_operator) {
            if precedence < min_precedence {
                break;
            }
            self.index += 1;
            let rhs = self.parse_binary(precedence + 1)?;
            lhs = Expr::binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    /// `UnaryExpr ::= UnionExpr | '-' UnaryExpr`
    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat(&Token::Minus) {
            self.enter()?;
            let inner = self.parse_unary()?;
            self.depth -= 1;
            return Ok(Expr::Negate(Box::new(inner)));
        }
        self.parse_union()
    }

    /// `UnionExpr ::= PathExpr | UnionExpr '|' PathExpr`
    fn parse_union(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_path_expr()?;
        while self.eat(&Token::Pipe) {
            let rhs = self.parse_path_expr()?;
            lhs = Expr::binary(BinaryOp::Union, lhs, rhs);
        }
        Ok(lhs)
    }

    /// `PathExpr ::= LocationPath | FilterExpr (('/' | '//') RelativeLocationPath)?`
    fn parse_path_expr(&mut self) -> Result<Expr> {
        if !self.at_primary_start() {
            return self.parse_location_path();
        }
        let primary = self.parse_primary()?;
        let predicates = self.parse_predicates()?;
        let mut steps = Vec::new();
        if self.eat(&Token::Slash) {
            steps = self.parse_relative_path()?;
        } else if self.eat(&Token::DoubleSlash) {
            steps.push(Step::descendant_or_self_node());
            steps.extend(self.parse_relative_path()?);
        }
        if predicates.is_empty() && steps.is_empty() {
            return Ok(primary);
        }
        Ok(Expr::path(Path {
            start: PathStart::Filter {
                primary: Box::new(primary),
                predicates,
            },
            steps,
        }))
    }

    /// Whether the cursor is on a `PrimaryExpr`. A node-type name (`text(`) looks
    /// like a function call but starts a step, so it is explicitly excluded.
    fn at_primary_start(&self) -> bool {
        match self.peek() {
            Some(Token::Variable(_) | Token::LParen | Token::Literal(_) | Token::Number(_)) => true,
            Some(Token::Function(name)) => node_type_test(name).is_none(),
            _ => false,
        }
    }

    /// `PrimaryExpr ::= VariableReference | '(' Expr ')' | Literal | Number | FunctionCall`
    fn parse_primary(&mut self) -> Result<Expr> {
        let position = self.position();
        match self.advance() {
            Some(Token::Variable(name)) => Ok(Expr::Variable(name)),
            Some(Token::Literal(value)) => Ok(Expr::Literal(value)),
            Some(Token::Number(value)) => Ok(Expr::Number(value)),
            Some(Token::LParen) => {
                let inner = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(inner)
            }
            Some(Token::Function(name)) => self.parse_function_call(&name),
            other => {
                let found =
                    other.map_or_else(|| "end of expression".to_owned(), |token| token.describe());
                Err(self.error(position, format!("expected an expression, found {found}")))
            }
        }
    }

    /// `FunctionCall ::= FunctionName '(' ( Argument ( ',' Argument )* )? ')'`
    fn parse_function_call(&mut self, name: &str) -> Result<Expr> {
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        if !self.eat(&Token::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                self.expect(&Token::RParen)?;
                break;
            }
        }
        if name.contains(':') {
            return Err(XPathError::unsupported(format!(
                "extension function `{name}()`"
            )));
        }
        let Some(function) = CoreFunction::from_name(name) else {
            return Err(XPathError::unsupported(format!("function `{name}()`")));
        };
        if !function.accepts(args.len()) {
            return Err(XPathError::Arity {
                name: name.to_owned(),
                expected: function.expected_arity(),
                got: args.len(),
            });
        }
        Ok(Expr::Function { function, args })
    }

    /// `Predicate*`
    fn parse_predicates(&mut self) -> Result<Vec<Expr>> {
        let mut predicates = Vec::new();
        while self.eat(&Token::LBracket) {
            predicates.push(self.parse_expr()?);
            self.expect(&Token::RBracket)?;
        }
        Ok(predicates)
    }

    // -- location paths ----------------------------------------------------

    /// `LocationPath ::= RelativeLocationPath | AbsoluteLocationPath`
    fn parse_location_path(&mut self) -> Result<Expr> {
        if self.eat(&Token::Slash) {
            // A lone `/` is the document root.
            if !self.at_step_start() {
                return Ok(Expr::path(Path::absolute(Vec::new())));
            }
            return Ok(Expr::path(Path::absolute(self.parse_relative_path()?)));
        }
        if self.eat(&Token::DoubleSlash) {
            let mut steps = vec![Step::descendant_or_self_node()];
            steps.extend(self.parse_relative_path()?);
            return Ok(Expr::path(Path::absolute(steps)));
        }
        Ok(Expr::path(Path::relative(self.parse_relative_path()?)))
    }

    /// `RelativeLocationPath ::= Step (('/' | '//') Step)*`
    fn parse_relative_path(&mut self) -> Result<Vec<Step>> {
        let mut steps = vec![self.parse_step()?];
        loop {
            if self.eat(&Token::Slash) {
                steps.push(self.parse_step()?);
            } else if self.eat(&Token::DoubleSlash) {
                steps.push(Step::descendant_or_self_node());
                steps.push(self.parse_step()?);
            } else {
                break;
            }
        }
        Ok(steps)
    }

    /// Whether the cursor is on something that can start a `Step`.
    fn at_step_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::Name(_)
                    | Token::Star
                    | Token::PrefixedStar(_)
                    | Token::At
                    | Token::Dot
                    | Token::DotDot
                    | Token::Axis(_)
                    | Token::Function(_)
            )
        )
    }

    /// `Step ::= AxisSpecifier NodeTest Predicate* | '.' | '..'`
    fn parse_step(&mut self) -> Result<Step> {
        let position = self.position();
        if self.eat(&Token::Dot) {
            return Ok(Step::new(Axis::SelfAxis, NodeTest::Node));
        }
        if self.eat(&Token::DotDot) {
            return Ok(Step::new(Axis::Parent, NodeTest::Node));
        }
        if self.peek() == Some(&Token::LParen) {
            return Err(XPathError::unsupported(
                "parenthesised step (XPath 2.0 or later)",
            ));
        }
        let axis = if self.eat(&Token::At) {
            Axis::Attribute
        } else if let Some(Token::Axis(name)) = self.peek().cloned() {
            self.index += 1;
            self.expect(&Token::DoubleColon)?;
            match Axis::from_name(&name) {
                Some(axis) => axis,
                None => {
                    return Err(self.error(position, format!("unknown axis `{name}`")));
                }
            }
        } else {
            Axis::Child
        };
        let node_test = self.parse_node_test()?;
        // `for $x in …`, `some $x satisfies …`: a keyword step followed by a
        // variable is never valid XPath 1.0, and naming the construct beats
        // "expected end of expression".
        if let (
            NodeTest::Name {
                prefix: None,
                local,
            },
            Some(Token::Variable(_)),
        ) = (&node_test, self.peek())
        {
            if LATER_VERSION_KEYWORDS.contains(&local.as_str()) {
                return Err(XPathError::unsupported(format!(
                    "`{local}` (XPath 2.0 or later)"
                )));
            }
        }
        let predicates = self.parse_predicates()?;
        Ok(Step {
            axis,
            node_test,
            predicates,
        })
    }

    /// `NodeTest ::= NameTest | NodeType '(' ')' | 'processing-instruction' '(' Literal ')'`
    fn parse_node_test(&mut self) -> Result<NodeTest> {
        let position = self.position();
        match self.peek().cloned() {
            Some(Token::Star) => {
                self.index += 1;
                Ok(NodeTest::Wildcard)
            }
            Some(Token::PrefixedStar(prefix)) => {
                self.index += 1;
                Ok(NodeTest::NamespaceWildcard(prefix))
            }
            Some(Token::Name(name)) => {
                self.index += 1;
                Ok(name_test(&name))
            }
            Some(Token::Function(name)) => {
                let Some(kind) = node_type_test(&name) else {
                    // A core function in step position is a plain syntax error;
                    // anything else is a construct we do not implement, such as
                    // XPath 2.0's `element()` / `attribute()` kind tests.
                    if CoreFunction::from_name(&name).is_some() {
                        return Err(self.error(
                            position,
                            format!("`{name}()` is a function, and cannot be a step"),
                        ));
                    }
                    return Err(XPathError::unsupported(format!(
                        "`{name}()` as a node test"
                    )));
                };
                self.index += 1;
                self.expect(&Token::LParen)?;
                let test = if matches!(kind, NodeTypeTest::ProcessingInstruction) {
                    match self.peek().cloned() {
                        Some(Token::Literal(target)) => {
                            self.index += 1;
                            NodeTest::ProcessingInstruction(Some(target))
                        }
                        _ => NodeTest::ProcessingInstruction(None),
                    }
                } else {
                    kind.node_test()
                };
                self.expect(&Token::RParen)?;
                Ok(test)
            }
            other => {
                let found =
                    other.map_or_else(|| "end of expression".to_owned(), |token| token.describe());
                Err(self.error(position, format!("expected a node test, found {found}")))
            }
        }
    }
}

/// `prefix:local` or `local`.
fn name_test(name: &str) -> NodeTest {
    match name.split_once(':') {
        Some((prefix, local)) => NodeTest::Name {
            prefix: Some(prefix.to_owned()),
            local: local.to_owned(),
        },
        None => NodeTest::Name {
            prefix: None,
            local: name.to_owned(),
        },
    }
}

/// The four reserved node-type names.
#[derive(Clone, Copy)]
enum NodeTypeTest {
    Text,
    Comment,
    Node,
    ProcessingInstruction,
}

impl NodeTypeTest {
    fn node_test(self) -> NodeTest {
        match self {
            Self::Text => NodeTest::Text,
            Self::Comment => NodeTest::Comment,
            Self::Node => NodeTest::Node,
            Self::ProcessingInstruction => NodeTest::ProcessingInstruction(None),
        }
    }
}

/// Whether `name` is one of the four node types rather than a function.
fn node_type_test(name: &str) -> Option<NodeTypeTest> {
    Some(match name {
        "text" => NodeTypeTest::Text,
        "comment" => NodeTypeTest::Comment,
        "node" => NodeTypeTest::Node,
        "processing-instruction" => NodeTypeTest::ProcessingInstruction,
        _ => return None,
    })
}

/// The operator and its precedence level, for the climbing loop.
fn binary_operator(token: &Token) -> Option<(BinaryOp, u8)> {
    Some(match token {
        Token::Or => (BinaryOp::Or, 1),
        Token::And => (BinaryOp::And, 2),
        Token::Equal => (BinaryOp::Equal, 3),
        Token::NotEqual => (BinaryOp::NotEqual, 3),
        Token::Less => (BinaryOp::LessThan, 4),
        Token::LessEqual => (BinaryOp::LessOrEqual, 4),
        Token::Greater => (BinaryOp::GreaterThan, 4),
        Token::GreaterEqual => (BinaryOp::GreaterOrEqual, 4),
        Token::Plus => (BinaryOp::Add, 5),
        Token::Minus => (BinaryOp::Subtract, 5),
        Token::Multiply => (BinaryOp::Multiply, 6),
        Token::Div => (BinaryOp::Divide, 6),
        Token::Mod => (BinaryOp::Modulo, 6),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unabbreviated(expr: &str) -> String {
        parse(expr).expect(expr).to_string()
    }

    #[test]
    fn abbreviations_expand() {
        assert_eq!(unabbreviated("para"), "child::para");
        assert_eq!(unabbreviated("*"), "child::*");
        assert_eq!(unabbreviated("@name"), "attribute::name");
        assert_eq!(unabbreviated("@*"), "attribute::*");
        assert_eq!(unabbreviated("."), "self::node()");
        assert_eq!(unabbreviated(".."), "parent::node()");
        assert_eq!(unabbreviated("../@lang"), "parent::node()/attribute::lang");
        assert_eq!(unabbreviated("/"), "/");
        assert_eq!(unabbreviated("/doc"), "/child::doc");
        assert_eq!(
            unabbreviated("//para"),
            "/descendant-or-self::node()/child::para"
        );
        assert_eq!(
            unabbreviated("a//b"),
            "child::a/descendant-or-self::node()/child::b"
        );
        assert_eq!(
            unabbreviated(".//para"),
            "self::node()/descendant-or-self::node()/child::para"
        );
        assert_eq!(
            unabbreviated("//a/@href"),
            "/descendant-or-self::node()/child::a/attribute::href"
        );
    }

    #[test]
    fn every_axis_parses() {
        for axis in [
            "ancestor",
            "ancestor-or-self",
            "attribute",
            "child",
            "descendant",
            "descendant-or-self",
            "following",
            "following-sibling",
            "namespace",
            "parent",
            "preceding",
            "preceding-sibling",
            "self",
        ] {
            assert_eq!(unabbreviated(&format!("{axis}::x")), format!("{axis}::x"));
        }
        let error = parse("nosuchaxis::x").expect_err("unknown axis");
        assert!(error.to_string().contains("unknown axis `nosuchaxis`"));
    }

    #[test]
    fn every_node_test_parses() {
        assert_eq!(unabbreviated("text()"), "child::text()");
        assert_eq!(unabbreviated("comment()"), "child::comment()");
        assert_eq!(unabbreviated("node()"), "child::node()");
        assert_eq!(
            unabbreviated("processing-instruction()"),
            "child::processing-instruction()"
        );
        assert_eq!(
            unabbreviated("processing-instruction('php')"),
            "child::processing-instruction('php')"
        );
        assert_eq!(unabbreviated("svg:rect"), "child::svg:rect");
        assert_eq!(unabbreviated("svg:*"), "child::svg:*");
        assert_eq!(unabbreviated("@xml:lang"), "attribute::xml:lang");
    }

    #[test]
    fn operator_precedence_is_the_spec_ladder() {
        assert_eq!(unabbreviated("1 + 2 * 3"), "(1 + (2 * 3))");
        assert_eq!(unabbreviated("1 * 2 + 3"), "((1 * 2) + 3)");
        assert_eq!(unabbreviated("1 - 2 - 3"), "((1 - 2) - 3)");
        assert_eq!(unabbreviated("1 div 2 mod 3"), "((1 div 2) mod 3)");
        assert_eq!(unabbreviated("1 < 2 = 3"), "((1 < 2) = 3)");
        assert_eq!(
            unabbreviated("a and b or c and d"),
            "((child::a and child::b) or (child::c and child::d))"
        );
        assert_eq!(unabbreviated("not(1) or 2"), "(not(1) or 2)");
        assert_eq!(unabbreviated("-1 + 2"), "(-1 + 2)");
        assert_eq!(unabbreviated("- -1"), "--1");
        assert_eq!(unabbreviated("1 - -1"), "(1 - -1)");
        // `|` binds tighter than unary minus, looser than a path.
        assert_eq!(
            unabbreviated("a | b | c"),
            "((child::a | child::b) | child::c)"
        );
        assert_eq!(unabbreviated("1 + 2 = 3"), "((1 + 2) = 3)");
    }

    #[test]
    fn filter_expressions_and_predicates() {
        assert_eq!(
            unabbreviated("(//a)[1]"),
            "(/descendant-or-self::node()/child::a)[1]"
        );
        assert_eq!(
            unabbreviated("(//a | //b)[last()]/@href"),
            "((/descendant-or-self::node()/child::a | /descendant-or-self::node()/child::b))[last()]/attribute::href"
        );
        assert_eq!(unabbreviated("$x[1]"), "($x)[1]");
        assert_eq!(unabbreviated("id('a')/b"), "(id('a'))/child::b");
        assert_eq!(
            unabbreviated("//a[@href][2]"),
            "/descendant-or-self::node()/child::a[attribute::href][2]"
        );
        assert_eq!(
            unabbreviated("//td[text()=$cell_text]"),
            "/descendant-or-self::node()/child::td[(child::text() = $cell_text)]"
        );
    }

    #[test]
    fn the_library_s_own_expressions_parse() {
        // parser.md §11.5 and cli-ai.md §4.4 — the expressions the reference implementation issues.
        for expr in [
            ".//*",
            ".//text()",
            ".//*[normalize-space(text())]",
            ".//a/@href| .//area/@href",
            "//body/div/div[count(ancestor::*) = 3]",
            "//td[text()=$cell_text]",
            "//section[@id=\"reviews\"]//div[contains(@class, \"review\") and @data-rating >= 4]",
            "//article[contains(@class,\"product\")][number(translate(substring-after(.//span[@class=\"price\"], \"$\"), \",\", \"\")) > 15]",
            ".//*[contains(@style,\"display:none\") or contains(@style,\"display: none\")] | .//*[@aria-hidden='true'] | .//template",
            "descendant-or-self::*[@class and contains(concat(' ', normalize-space(@class), ' '), ' product ')]",
            "descendant-or-self::a[@href and substring(@href, string-length(@href)-4) = '.html']",
            "descendant-or-self::e[count(preceding-sibling::*) mod 2 = 0]",
            "descendant-or-self::input[(@selected and name(.) = 'option') or (@checked and (name(.) = 'input' or name(.) = 'command')and (@type = 'checkbox' or @type = 'radio'))]",
            "descendant-or-self::*[1]/div/text()",
            "ancestor-or-self::*[@lang][1][starts-with(concat(translate(@lang,'ABC','abc'), '-'), 'en-')]",
        ] {
            parse(expr).unwrap_or_else(|error| panic!("{expr}: {error}"));
        }
    }

    #[test]
    fn unknown_functions_fail_at_compile_time() {
        for (expr, needle) in [
            ("ends-with(@a, 'b')", "function `ends-with()`"),
            ("matches(., 'x')", "function `matches()`"),
            ("my:thing()", "extension function `my:thing()`"),
            ("//element(x)", "`element()` as a node test"),
        ] {
            let error = parse(expr).expect_err(expr);
            assert!(error.is_compile_time(), "{expr}");
            assert!(
                matches!(error, XPathError::Unsupported { .. }),
                "{expr}: {error}"
            );
            assert!(error.to_string().contains(needle), "{expr}: {error}");
        }
    }

    #[test]
    fn later_xpath_versions_fail_at_compile_time() {
        for expr in [
            "for $x in //a return $x",
            "1 to 5",
            "//a[1] except //b",
            "'a' eq 'b'",
            "//p/(a|b)",
            "//a instance of node()",
        ] {
            let error = parse(expr).expect_err(expr);
            assert!(error.is_compile_time(), "{expr}: {error}");
            assert!(
                matches!(error, XPathError::Unsupported { .. }),
                "{expr}: {error}"
            );
        }
    }

    #[test]
    fn wrong_arity_fails_at_compile_time() {
        for (expr, name) in [
            ("concat('a')", "concat"),
            ("count()", "count"),
            ("count(1, 2)", "count"),
            ("last(1)", "last"),
            ("substring('a', 1, 2, 3)", "substring"),
            ("translate('a', 'b')", "translate"),
        ] {
            let error = parse(expr).expect_err(expr);
            assert!(
                matches!(&error, XPathError::Arity { name: n, .. } if n == name),
                "{expr}: {error}"
            );
        }
        // …and the valid arities all pass.
        for expr in [
            "concat('a', 'b')",
            "concat('a', 'b', 'c', 'd')",
            "substring('a', 1)",
            "substring('a', 1, 2)",
            "string()",
            "string(.)",
            "name()",
            "last()",
        ] {
            parse(expr).unwrap_or_else(|error| panic!("{expr}: {error}"));
        }
    }

    #[test]
    fn syntax_errors_are_reported_with_a_position() {
        for expr in [
            "4 ayo", "//p[", "//p[1", "1 +", "()", "/*/", "a/", ".[1]", "..[1]", "//", "@", "'a",
            "count(1", "(1))", "a::b",
        ] {
            let error = parse(expr).expect_err(expr);
            assert!(error.is_compile_time(), "{expr}: {error}");
        }
    }

    /// A hostile expression must fail, not overflow the stack.
    #[test]
    fn absurd_nesting_is_rejected() {
        let deep = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
        let error = parse(&deep).expect_err("too deep");
        assert!(error.to_string().contains("nests too deeply"), "{error}");

        let negations = format!("{}1", "- ".repeat(10_000));
        let error = parse(&negations).expect_err("too deep");
        assert!(error.to_string().contains("nests too deeply"), "{error}");

        let predicates = format!("//a{}", "[1]".repeat(10_000));
        parse(&predicates).expect("predicates iterate, they do not nest");

        let steps = "/a".repeat(10_000);
        parse(&steps).expect("steps iterate, they do not nest");

        // …and the limit is far above anything real.
        let realistic = format!("{}1{}", "(".repeat(100), ")".repeat(100));
        parse(&realistic).expect("100 levels is fine");
    }

    #[test]
    fn a_lone_slash_is_the_root() {
        assert_eq!(unabbreviated("/"), "/");
        assert_eq!(unabbreviated("count(/)"), "count(/)");
        assert_eq!(unabbreviated("/ | /"), "(/ | /)");
    }

    #[test]
    fn literals_and_numbers() {
        assert_eq!(unabbreviated("'a'"), "'a'");
        assert_eq!(unabbreviated("\"a\""), "'a'");
        assert_eq!(unabbreviated("1.5"), "1.5");
        assert_eq!(unabbreviated(".5"), "0.5");
        assert_eq!(unabbreviated("$v"), "$v");
    }
}
