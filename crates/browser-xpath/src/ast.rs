//! The expression AST: location paths, steps, predicates, operators and function
//! calls.
//!
//! Every abbreviation of the grammar is expanded here, so the evaluator only ever
//! sees the unabbreviated form:
//!
//! | source | AST |
//! |---|---|
//! | `foo` | `child::foo` |
//! | `@foo` | `attribute::foo` |
//! | `.` | `self::node()` |
//! | `..` | `parent::node()` |
//! | `//foo` | `/descendant-or-self::node()/child::foo` |
//! | `a//b` | `a/descendant-or-self::node()/child::b` |
//!
//! [`Expr`] implements [`std::fmt::Display`], which prints the *unabbreviated*
//! expression — handy in tests and error messages, and a cheap way to see exactly
//! what the parser understood.

use std::fmt;

/// A compiled XPath expression tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A location path, or a filter expression with a path glued onto it.
    Path(Box<Path>),
    /// A binary operator.
    Binary {
        /// Which operator.
        op: BinaryOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// Unary minus.
    Negate(Box<Expr>),
    /// A string literal.
    Literal(String),
    /// A numeric literal.
    Number(f64),
    /// `$name`.
    Variable(String),
    /// A call to one of the 27 core functions.
    Function {
        /// Which function.
        function: CoreFunction,
        /// The argument expressions.
        args: Vec<Expr>,
    },
}

impl Expr {
    /// Wrap a path.
    #[must_use]
    pub fn path(path: Path) -> Self {
        Self::Path(Box::new(path))
    }

    /// Build a binary node.
    #[must_use]
    pub fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Self {
        Self::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }
}

/// The binary operators of XPath 1.0, in increasing precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `or`
    Or,
    /// `and`
    And,
    /// `=`
    Equal,
    /// `!=`
    NotEqual,
    /// `<`
    LessThan,
    /// `<=`
    LessOrEqual,
    /// `>`
    GreaterThan,
    /// `>=`
    GreaterOrEqual,
    /// `+`
    Add,
    /// `-`
    Subtract,
    /// `*`
    Multiply,
    /// `div`
    Divide,
    /// `mod`
    Modulo,
    /// `|`
    Union,
}

impl BinaryOp {
    /// The operator's source spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Or => "or",
            Self::And => "and",
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::LessThan => "<",
            Self::LessOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterOrEqual => ">=",
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "div",
            Self::Modulo => "mod",
            Self::Union => "|",
        }
    }
}

/// A location path, or a filter expression followed by one.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    /// Where the path starts.
    pub start: PathStart,
    /// The steps, already unabbreviated.
    pub steps: Vec<Step>,
}

impl Path {
    /// A path starting at the context node.
    #[must_use]
    pub fn relative(steps: Vec<Step>) -> Self {
        Self {
            start: PathStart::Context,
            steps,
        }
    }

    /// A path starting at the document root.
    #[must_use]
    pub fn absolute(steps: Vec<Step>) -> Self {
        Self {
            start: PathStart::Root,
            steps,
        }
    }
}

/// The initial node-set of a [`Path`].
#[derive(Clone, Debug, PartialEq)]
pub enum PathStart {
    /// `/…` — the document node.
    Root,
    /// A relative path — the context node.
    Context,
    /// `FilterExpr` — a primary expression plus its predicates, e.g. `(//a)[1]`.
    Filter {
        /// The primary expression.
        primary: Box<Expr>,
        /// Predicates applied to it, in source order.
        predicates: Vec<Expr>,
    },
}

/// One `axis::node-test[predicate]*` step.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// The axis to walk.
    pub axis: Axis,
    /// The node test applied to every node of the axis.
    pub node_test: NodeTest,
    /// Predicates, applied left to right.
    pub predicates: Vec<Expr>,
}

impl Step {
    /// A step with no predicates.
    #[must_use]
    pub fn new(axis: Axis, node_test: NodeTest) -> Self {
        Self {
            axis,
            node_test,
            predicates: Vec::new(),
        }
    }

    /// The `descendant-or-self::node()` step that `//` expands to.
    #[must_use]
    pub fn descendant_or_self_node() -> Self {
        Self::new(Axis::DescendantOrSelf, NodeTest::Node)
    }
}

/// The 13 axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    /// `ancestor::`
    Ancestor,
    /// `ancestor-or-self::`
    AncestorOrSelf,
    /// `attribute::`, abbreviated `@`
    Attribute,
    /// `child::` — the default axis
    Child,
    /// `descendant::`
    Descendant,
    /// `descendant-or-self::`
    DescendantOrSelf,
    /// `following::`
    Following,
    /// `following-sibling::`
    FollowingSibling,
    /// `namespace::`
    Namespace,
    /// `parent::`, abbreviated `..`
    Parent,
    /// `preceding::`
    Preceding,
    /// `preceding-sibling::`
    PrecedingSibling,
    /// `self::`, abbreviated `.`
    SelfAxis,
}

impl Axis {
    /// Resolve an axis name as written in an expression.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "ancestor" => Self::Ancestor,
            "ancestor-or-self" => Self::AncestorOrSelf,
            "attribute" => Self::Attribute,
            "child" => Self::Child,
            "descendant" => Self::Descendant,
            "descendant-or-self" => Self::DescendantOrSelf,
            "following" => Self::Following,
            "following-sibling" => Self::FollowingSibling,
            "namespace" => Self::Namespace,
            "parent" => Self::Parent,
            "preceding" => Self::Preceding,
            "preceding-sibling" => Self::PrecedingSibling,
            "self" => Self::SelfAxis,
            _ => return None,
        })
    }

    /// The axis name as written in an expression.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ancestor => "ancestor",
            Self::AncestorOrSelf => "ancestor-or-self",
            Self::Attribute => "attribute",
            Self::Child => "child",
            Self::Descendant => "descendant",
            Self::DescendantOrSelf => "descendant-or-self",
            Self::Following => "following",
            Self::FollowingSibling => "following-sibling",
            Self::Namespace => "namespace",
            Self::Parent => "parent",
            Self::Preceding => "preceding",
            Self::PrecedingSibling => "preceding-sibling",
            Self::SelfAxis => "self",
        }
    }

    /// Whether the axis walks *backwards* through the document.
    ///
    /// XPath 1.0 §2.2: the `ancestor`, `ancestor-or-self`, `preceding` and
    /// `preceding-sibling` axes are reverse axes; every other axis is forward. It
    /// matters because a predicate's `position()` is the node's *proximity*
    /// position, which counts along the axis — so `preceding-sibling::*[1]` is the
    /// **nearest** preceding sibling, not the first one in the document.
    #[must_use]
    pub fn is_reverse(self) -> bool {
        matches!(
            self,
            Self::Ancestor | Self::AncestorOrSelf | Self::Preceding | Self::PrecedingSibling
        )
    }

    /// The principal node type: what `*` selects on this axis.
    #[must_use]
    pub fn principal_node_kind(self) -> crate::model::NodeKind {
        match self {
            Self::Attribute => crate::model::NodeKind::Attr,
            Self::Namespace => crate::model::NodeKind::Namespace,
            _ => crate::model::NodeKind::Element,
        }
    }
}

/// The node tests of XPath 1.0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeTest {
    /// `name` or `prefix:name`.
    Name {
        /// The namespace prefix, if the test was written with one.
        prefix: Option<String>,
        /// The local part.
        local: String,
    },
    /// `*` — every node of the axis's principal node type.
    Wildcard,
    /// `prefix:*` — every principal node in that namespace.
    NamespaceWildcard(String),
    /// `text()`
    Text,
    /// `comment()`
    Comment,
    /// `node()` — every node whatever its type.
    Node,
    /// `processing-instruction()` or `processing-instruction('target')`.
    ProcessingInstruction(Option<String>),
}

/// The 27 core functions of XPath 1.0 (§4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoreFunction {
    // §4.1 node-set functions
    /// `last()`
    Last,
    /// `position()`
    Position,
    /// `count(node-set)`
    Count,
    /// `id(object)`
    Id,
    /// `local-name(node-set?)`
    LocalName,
    /// `namespace-uri(node-set?)`
    NamespaceUri,
    /// `name(node-set?)`
    Name,
    // §4.2 string functions
    /// `string(object?)`
    String,
    /// `concat(string, string, string*)`
    Concat,
    /// `starts-with(string, string)`
    StartsWith,
    /// `contains(string, string)`
    Contains,
    /// `substring-before(string, string)`
    SubstringBefore,
    /// `substring-after(string, string)`
    SubstringAfter,
    /// `substring(string, number, number?)`
    Substring,
    /// `string-length(string?)`
    StringLength,
    /// `normalize-space(string?)`
    NormalizeSpace,
    /// `translate(string, string, string)`
    Translate,
    // §4.3 boolean functions
    /// `boolean(object)`
    Boolean,
    /// `not(boolean)`
    Not,
    /// `true()`
    True,
    /// `false()`
    False,
    /// `lang(string)`
    Lang,
    // §4.4 number functions
    /// `number(object?)`
    Number,
    /// `sum(node-set)`
    Sum,
    /// `floor(number)`
    Floor,
    /// `ceiling(number)`
    Ceiling,
    /// `round(number)`
    Round,
}

impl CoreFunction {
    /// Resolve a function name.
    ///
    /// `None` means the name is not a core function — which
    /// [`crate::eval::XPath::compile`] turns into
    /// [`crate::error::XPathError::Unsupported`] rather than a silently empty
    /// node-set.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "last" => Self::Last,
            "position" => Self::Position,
            "count" => Self::Count,
            "id" => Self::Id,
            "local-name" => Self::LocalName,
            "namespace-uri" => Self::NamespaceUri,
            "name" => Self::Name,
            "string" => Self::String,
            "concat" => Self::Concat,
            "starts-with" => Self::StartsWith,
            "contains" => Self::Contains,
            "substring-before" => Self::SubstringBefore,
            "substring-after" => Self::SubstringAfter,
            "substring" => Self::Substring,
            "string-length" => Self::StringLength,
            "normalize-space" => Self::NormalizeSpace,
            "translate" => Self::Translate,
            "boolean" => Self::Boolean,
            "not" => Self::Not,
            "true" => Self::True,
            "false" => Self::False,
            "lang" => Self::Lang,
            "number" => Self::Number,
            "sum" => Self::Sum,
            "floor" => Self::Floor,
            "ceiling" => Self::Ceiling,
            "round" => Self::Round,
            _ => return None,
        })
    }

    /// The function's name as written in an expression.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Last => "last",
            Self::Position => "position",
            Self::Count => "count",
            Self::Id => "id",
            Self::LocalName => "local-name",
            Self::NamespaceUri => "namespace-uri",
            Self::Name => "name",
            Self::String => "string",
            Self::Concat => "concat",
            Self::StartsWith => "starts-with",
            Self::Contains => "contains",
            Self::SubstringBefore => "substring-before",
            Self::SubstringAfter => "substring-after",
            Self::Substring => "substring",
            Self::StringLength => "string-length",
            Self::NormalizeSpace => "normalize-space",
            Self::Translate => "translate",
            Self::Boolean => "boolean",
            Self::Not => "not",
            Self::True => "true",
            Self::False => "false",
            Self::Lang => "lang",
            Self::Number => "number",
            Self::Sum => "sum",
            Self::Floor => "floor",
            Self::Ceiling => "ceiling",
            Self::Round => "round",
        }
    }

    /// `(minimum, maximum)` argument counts; `None` as the maximum means
    /// "unbounded".
    #[must_use]
    pub fn arity(self) -> (usize, Option<usize>) {
        match self {
            Self::Last | Self::Position | Self::True | Self::False => (0, Some(0)),
            Self::LocalName | Self::NamespaceUri | Self::Name => (0, Some(1)),
            Self::String | Self::StringLength | Self::NormalizeSpace | Self::Number => (0, Some(1)),
            Self::Count | Self::Id | Self::Boolean | Self::Not | Self::Lang => (1, Some(1)),
            Self::Sum | Self::Floor | Self::Ceiling | Self::Round => (1, Some(1)),
            Self::StartsWith | Self::Contains | Self::SubstringBefore | Self::SubstringAfter => {
                (2, Some(2))
            }
            Self::Substring => (2, Some(3)),
            Self::Translate => (3, Some(3)),
            Self::Concat => (2, None),
        }
    }

    /// The arity, spelled for an error message.
    #[must_use]
    pub fn expected_arity(self) -> String {
        match self.arity() {
            (min, None) => format!("at least {min}"),
            (min, Some(max)) if min == max => min.to_string(),
            (min, Some(max)) => format!("{min} or {max}"),
        }
    }

    /// Whether `count` argument values are acceptable.
    #[must_use]
    pub fn accepts(self, count: usize) -> bool {
        let (min, max) = self.arity();
        count >= min && max.is_none_or(|max| count <= max)
    }
}

// ---------------------------------------------------------------------------
// Display — the unabbreviated form of the expression.
// ---------------------------------------------------------------------------

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => write!(f, "{path}"),
            Self::Binary { op, lhs, rhs } => write!(f, "({lhs} {} {rhs})", op.as_str()),
            Self::Negate(inner) => write!(f, "-{inner}"),
            Self::Literal(value) => write!(f, "'{value}'"),
            Self::Number(value) => write!(f, "{}", crate::value::format_number(*value)),
            Self::Variable(name) => write!(f, "${name}"),
            Self::Function { function, args } => {
                write!(f, "{}(", function.as_str())?;
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                write!(f, ")")
            }
        }
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.start {
            PathStart::Root => write!(f, "/")?,
            PathStart::Context => {}
            PathStart::Filter {
                primary,
                predicates,
            } => {
                write!(f, "({primary})")?;
                for predicate in predicates {
                    write!(f, "[{predicate}]")?;
                }
                if !self.steps.is_empty() {
                    write!(f, "/")?;
                }
            }
        }
        for (index, step) in self.steps.iter().enumerate() {
            if index > 0 {
                write!(f, "/")?;
            }
            write!(f, "{step}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.axis.as_str(), self.node_test)?;
        for predicate in &self.predicates {
            write!(f, "[{predicate}]")?;
        }
        Ok(())
    }
}

impl fmt::Display for NodeTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name {
                prefix: Some(prefix),
                local,
            } => write!(f, "{prefix}:{local}"),
            Self::Name {
                prefix: None,
                local,
            } => write!(f, "{local}"),
            Self::Wildcard => write!(f, "*"),
            Self::NamespaceWildcard(prefix) => write!(f, "{prefix}:*"),
            Self::Text => write!(f, "text()"),
            Self::Comment => write!(f, "comment()"),
            Self::Node => write!(f, "node()"),
            Self::ProcessingInstruction(None) => write!(f, "processing-instruction()"),
            Self::ProcessingInstruction(Some(target)) => {
                write!(f, "processing-instruction('{target}')")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_names_round_trip() {
        for axis in [
            Axis::Ancestor,
            Axis::AncestorOrSelf,
            Axis::Attribute,
            Axis::Child,
            Axis::Descendant,
            Axis::DescendantOrSelf,
            Axis::Following,
            Axis::FollowingSibling,
            Axis::Namespace,
            Axis::Parent,
            Axis::Preceding,
            Axis::PrecedingSibling,
            Axis::SelfAxis,
        ] {
            assert_eq!(Axis::from_name(axis.as_str()), Some(axis));
        }
        assert_eq!(Axis::from_name("nosuchaxis"), None);
    }

    #[test]
    fn exactly_the_four_reverse_axes() {
        let reverse: Vec<&str> = [
            Axis::Ancestor,
            Axis::AncestorOrSelf,
            Axis::Attribute,
            Axis::Child,
            Axis::Descendant,
            Axis::DescendantOrSelf,
            Axis::Following,
            Axis::FollowingSibling,
            Axis::Namespace,
            Axis::Parent,
            Axis::Preceding,
            Axis::PrecedingSibling,
            Axis::SelfAxis,
        ]
        .into_iter()
        .filter(|axis| axis.is_reverse())
        .map(Axis::as_str)
        .collect();
        assert_eq!(
            reverse,
            [
                "ancestor",
                "ancestor-or-self",
                "preceding",
                "preceding-sibling"
            ]
        );
    }

    #[test]
    fn all_twenty_seven_core_functions_resolve() {
        let names = [
            "last",
            "position",
            "count",
            "id",
            "local-name",
            "namespace-uri",
            "name",
            "string",
            "concat",
            "starts-with",
            "contains",
            "substring-before",
            "substring-after",
            "substring",
            "string-length",
            "normalize-space",
            "translate",
            "boolean",
            "not",
            "true",
            "false",
            "lang",
            "number",
            "sum",
            "floor",
            "ceiling",
            "round",
        ];
        assert_eq!(names.len(), 27);
        for name in names {
            let function = CoreFunction::from_name(name).expect(name);
            assert_eq!(function.as_str(), name);
        }
        assert_eq!(CoreFunction::from_name("ends-with"), None);
        assert_eq!(CoreFunction::from_name("matches"), None);
    }

    #[test]
    fn arities_are_the_spec_ones() {
        assert!(CoreFunction::Concat.accepts(2));
        assert!(CoreFunction::Concat.accepts(9));
        assert!(!CoreFunction::Concat.accepts(1));
        assert_eq!(CoreFunction::Concat.expected_arity(), "at least 2");
        assert!(CoreFunction::Substring.accepts(2));
        assert!(CoreFunction::Substring.accepts(3));
        assert!(!CoreFunction::Substring.accepts(4));
        assert_eq!(CoreFunction::Substring.expected_arity(), "2 or 3");
        assert!(CoreFunction::Last.accepts(0));
        assert!(!CoreFunction::Last.accepts(1));
        assert_eq!(CoreFunction::Last.expected_arity(), "0");
        assert!(CoreFunction::String.accepts(0));
        assert!(CoreFunction::String.accepts(1));
    }

    #[test]
    fn display_prints_the_unabbreviated_form() {
        let path = Path::absolute(vec![
            Step::descendant_or_self_node(),
            Step {
                axis: Axis::Child,
                node_test: NodeTest::Name {
                    prefix: None,
                    local: "a".into(),
                },
                predicates: vec![Expr::Number(1.0)],
            },
        ]);
        assert_eq!(
            Expr::path(path).to_string(),
            "/descendant-or-self::node()/child::a[1]"
        );
    }
}
