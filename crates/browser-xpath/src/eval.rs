//! The tree-walking evaluator: [`XPath`], the context node/position/size, predicate
//! filtering, and document-order results.
//!
//! # The two invariants
//!
//! 1. **Every node-set this module produces is in document order and
//!    de-duplicated.** Steps merge their per-context-node results and re-sort, and
//!    `|` does the same, so `string()` can take the first node and callers can
//!    iterate results in document order without sorting again.
//! 2. **Predicates see axis order.** A node's `position()` is its *proximity
//!    position*, which counts along the axis — so `preceding-sibling::*[1]` is the
//!    nearest preceding sibling, and only after the predicates have run does the
//!    result go back into document order.
//!
//! # Fast paths
//!
//! `ARCHITECTURE.md` §1.4 notes that the expressions the library issues itself —
//! `.//*`, `.//text()`, `.//*[normalize-space(text())]`, `.//a/@href| .//area/@href`
//! — should be direct tree walks. [`XPath::compile`] recognises them and stores a
//! [`XPath::is_fast_path`] plan that walks descendants once, with no intermediate
//! node-sets, no predicate evaluation and no sort. The engine's own tests assert
//! that each fast path returns exactly what the general evaluator returns.

use std::borrow::Cow;

use indexmap::IndexMap;
use browser_dom::{Document, NodeRef};

use crate::ast::{Axis, BinaryOp, Expr, NodeTest, Path, PathStart, Step};
use crate::axes::{axis_nodes, descendants, matches_test};
use crate::error::{Result, XPathError};
use crate::functions;
use crate::model::{DomModel, NodeKind, TreeModel, XNode, XML_NAMESPACE};
use crate::parser::parse;
use crate::value::{normalize_space, Value};

// ---------------------------------------------------------------------------
// Variables and namespaces.
// ---------------------------------------------------------------------------

/// The value bound to a `$variable`.
///
/// XPath 1.0 variables can hold any of the four value types, but a node-set
/// binding cannot cross this API's boundary without infecting it with the node
/// type, and `Selector.xpath(**kwargs)` — the only caller in the port — can only
/// pass scalars. Node-set variables are therefore not bound here; everything else
/// is.
#[derive(Clone, Debug, PartialEq)]
pub enum Variable {
    /// A string, the shape `page.xpath("//td[text()=$cell_text]", cell_text="…")` uses.
    Str(String),
    /// A number.
    Number(f64),
    /// A boolean.
    Bool(bool),
}

impl From<&str> for Variable {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for Variable {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<f64> for Variable {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<i64> for Variable {
    fn from(value: i64) -> Self {
        #[allow(clippy::cast_precision_loss)]
        Self::Number(value as f64)
    }
}

impl From<bool> for Variable {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// `$name` bindings for one evaluation.
///
/// ```
/// use browser_xpath::eval::Variables;
///
/// let mut variables = Variables::new();
/// variables.insert("cell_text", "Cell 1");
/// assert!(variables.get("cell_text").is_some());
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variables {
    bindings: IndexMap<String, Variable>,
}

impl Variables {
    /// An empty binding set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name`, returning any previous binding.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        value: impl Into<Variable>,
    ) -> Option<Variable> {
        self.bindings.insert(name.into(), value.into())
    }

    /// Look a binding up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Variable> {
        self.bindings.get(name)
    }

    /// How many variables are bound.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether nothing is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

impl<K: Into<String>, V: Into<Variable>> FromIterator<(K, V)> for Variables {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut variables = Self::new();
        for (name, value) in iter {
            variables.insert(name, value);
        }
        variables
    }
}

/// `prefix` → namespace-URI bindings for name tests such as `svg:rect`.
///
/// The `xml` prefix is always bound. An unprefixed name test never consults this
/// map: it matches on the local name alone (see [`crate::axes::matches_test`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Namespaces {
    bindings: IndexMap<String, String>,
}

impl Default for Namespaces {
    fn default() -> Self {
        Self::new()
    }
}

impl Namespaces {
    /// A map with only the implicit `xml` prefix bound.
    #[must_use]
    pub fn new() -> Self {
        let mut bindings = IndexMap::new();
        bindings.insert("xml".to_owned(), XML_NAMESPACE.to_owned());
        Self { bindings }
    }

    /// Bind `prefix` to `uri`.
    pub fn insert(&mut self, prefix: impl Into<String>, uri: impl Into<String>) -> Option<String> {
        self.bindings.insert(prefix.into(), uri.into())
    }

    /// The URI bound to `prefix`.
    ///
    /// # Errors
    ///
    /// [`XPathError::UndefinedPrefix`] when the prefix is not bound — never an
    /// empty node-set.
    pub fn resolve(&self, prefix: &str) -> Result<&str> {
        self.bindings
            .get(prefix)
            .map(String::as_str)
            .ok_or_else(|| XPathError::UndefinedPrefix {
                prefix: prefix.to_owned(),
            })
    }
}

// ---------------------------------------------------------------------------
// The evaluation context.
// ---------------------------------------------------------------------------

/// The context an expression is evaluated in: a node, plus its proximity position
/// in the node-set being filtered and that node-set's size.
///
/// `position()` returns [`Context::position`] and `last()` returns
/// [`Context::size`]; outside a predicate both are `1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context<N> {
    /// The context node.
    pub node: N,
    /// 1-based proximity position.
    pub position: usize,
    /// The context size.
    pub size: usize,
}

impl<N: Copy> Context<N> {
    /// The initial context for `node`: position 1 of a node-set of size 1.
    #[must_use]
    pub fn new(node: N) -> Self {
        Self {
            node,
            position: 1,
            size: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// The evaluator.
// ---------------------------------------------------------------------------

/// The tree-walking evaluator. Crate-internal: [`XPath`] is the entry point.
pub(crate) struct Evaluator<'a, M: TreeModel> {
    pub(crate) model: &'a M,
    pub(crate) variables: &'a Variables,
    pub(crate) namespaces: &'a Namespaces,
}

impl<M: TreeModel> Evaluator<'_, M> {
    /// Evaluate an expression.
    pub(crate) fn eval(&self, expr: &Expr, context: Context<M::Node>) -> Result<Value<M::Node>> {
        match expr {
            Expr::Literal(value) => Ok(Value::Str(value.clone())),
            Expr::Number(value) => Ok(Value::Number(*value)),
            Expr::Variable(name) => match self.variables.get(name) {
                Some(Variable::Str(value)) => Ok(Value::Str(value.clone())),
                Some(Variable::Number(value)) => Ok(Value::Number(*value)),
                Some(Variable::Bool(value)) => Ok(Value::Bool(*value)),
                None => Err(XPathError::UndefinedVariable { name: name.clone() }),
            },
            Expr::Negate(inner) => {
                let value = self.eval(inner, context)?;
                Ok(Value::Number(-value.number(self.model)))
            }
            Expr::Function { function, args } => functions::call(self, *function, args, context),
            Expr::Path(path) => Ok(Value::NodeSet(self.eval_path(path, context)?)),
            Expr::Binary { op, lhs, rhs } => self.eval_binary(*op, lhs, rhs, context),
        }
    }

    /// Evaluate an expression that must produce a node-set.
    pub(crate) fn node_set(
        &self,
        expr: &Expr,
        context: Context<M::Node>,
        wanted_by: &str,
    ) -> Result<Vec<M::Node>> {
        self.eval(expr, context)?.into_node_set(wanted_by)
    }

    /// Sort a node-set into document order and drop duplicates.
    pub(crate) fn sort_dedup(&self, nodes: &mut Vec<M::Node>) {
        nodes.sort_by_key(|node| self.model.document_order(*node));
        nodes.dedup();
    }

    /// `Path` — the location-path machinery.
    fn eval_path(&self, path: &Path, context: Context<M::Node>) -> Result<Vec<M::Node>> {
        let mut current = match &path.start {
            PathStart::Root => vec![self.model.root_of(context.node)],
            PathStart::Context => vec![context.node],
            PathStart::Filter {
                primary,
                predicates,
            } => {
                let mut nodes = self.node_set(primary, context, "a filter expression")?;
                self.sort_dedup(&mut nodes);
                for predicate in predicates {
                    nodes = self.filter(nodes, predicate)?;
                }
                nodes
            }
        };

        for step in &path.steps {
            let mut next: Vec<M::Node> = Vec::new();
            for node in current {
                next.extend(self.eval_step(step, node)?);
            }
            self.sort_dedup(&mut next);
            current = next;
        }
        Ok(current)
    }

    /// One `axis::node-test[predicate]*` step from one context node, in axis order.
    fn eval_step(&self, step: &Step, node: M::Node) -> Result<Vec<M::Node>> {
        let candidates = axis_nodes(self.model, node, step.axis);
        let mut matched = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if matches_test(
                self.model,
                candidate,
                &step.node_test,
                step.axis,
                self.namespaces,
            )? {
                matched.push(candidate);
            }
        }
        for predicate in &step.predicates {
            matched = self.filter(matched, predicate)?;
        }
        Ok(matched)
    }

    /// Apply one predicate to a node-set that is already in axis order.
    ///
    /// A predicate whose value is a **number** is true when that number equals the
    /// context position — this is what makes `[1]` mean "the first one" and
    /// `[last()]` mean "the last one".
    fn filter(&self, nodes: Vec<M::Node>, predicate: &Expr) -> Result<Vec<M::Node>> {
        let size = nodes.len();
        let mut kept = Vec::with_capacity(size);
        for (index, node) in nodes.into_iter().enumerate() {
            let context = Context {
                node,
                position: index + 1,
                size,
            };
            let keep = match self.eval(predicate, context)? {
                #[allow(clippy::cast_precision_loss)]
                Value::Number(number) => number == context.position as f64,
                other => other.boolean(),
            };
            if keep {
                kept.push(node);
            }
        }
        Ok(kept)
    }

    fn eval_binary(
        &self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        context: Context<M::Node>,
    ) -> Result<Value<M::Node>> {
        match op {
            // `or` and `and` short-circuit — XPath 1.0 §3.4 requires it.
            BinaryOp::Or => {
                if self.eval(lhs, context)?.boolean() {
                    return Ok(Value::Bool(true));
                }
                Ok(Value::Bool(self.eval(rhs, context)?.boolean()))
            }
            BinaryOp::And => {
                if !self.eval(lhs, context)?.boolean() {
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(self.eval(rhs, context)?.boolean()))
            }
            BinaryOp::Union => {
                let mut nodes = self.node_set(lhs, context, "the `|` operator")?;
                nodes.extend(self.node_set(rhs, context, "the `|` operator")?);
                self.sort_dedup(&mut nodes);
                Ok(Value::NodeSet(nodes))
            }
            BinaryOp::Equal | BinaryOp::NotEqual => {
                let left = self.eval(lhs, context)?;
                let right = self.eval(rhs, context)?;
                Ok(Value::Bool(self.equality(
                    op == BinaryOp::Equal,
                    &left,
                    &right,
                )))
            }
            BinaryOp::LessThan
            | BinaryOp::LessOrEqual
            | BinaryOp::GreaterThan
            | BinaryOp::GreaterOrEqual => {
                let left = self.eval(lhs, context)?;
                let right = self.eval(rhs, context)?;
                Ok(Value::Bool(self.relational(op, &left, &right)))
            }
            BinaryOp::Add
            | BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Modulo => {
                let left = self.eval(lhs, context)?.number(self.model);
                let right = self.eval(rhs, context)?.number(self.model);
                Ok(Value::Number(match op {
                    BinaryOp::Add => left + right,
                    BinaryOp::Subtract => left - right,
                    BinaryOp::Multiply => left * right,
                    BinaryOp::Divide => left / right,
                    // XPath's `mod` is C's `fmod`: the result takes the sign of
                    // the dividend, so `-3 mod 2` is -1 and `3 mod -2` is 1.
                    _ => left % right,
                }))
            }
        }
    }

    /// `=` and `!=` (§3.4), including the "some pair compares true" node-set rule.
    fn equality(&self, want_equal: bool, left: &Value<M::Node>, right: &Value<M::Node>) -> bool {
        let test = |a: &str, b: &str| (a == b) == want_equal;
        match (left, right) {
            (Value::NodeSet(xs), Value::NodeSet(ys)) => {
                let ys: Vec<Cow<'_, str>> =
                    ys.iter().map(|n| self.model.string_value(*n)).collect();
                xs.iter().any(|x| {
                    let x = self.model.string_value(*x);
                    ys.iter().any(|y| test(&x, y))
                })
            }
            (Value::NodeSet(nodes), other) | (other, Value::NodeSet(nodes)) => match other {
                Value::Bool(flag) => {
                    let as_boolean = !nodes.is_empty();
                    (as_boolean == *flag) == want_equal
                }
                Value::Number(number) => nodes.iter().any(|node| {
                    let value = crate::value::parse_number(&self.model.string_value(*node));
                    (value == *number) == want_equal
                }),
                _ => {
                    let text = other.string_value(self.model);
                    nodes
                        .iter()
                        .any(|node| test(&self.model.string_value(*node), &text))
                }
            },
            (Value::Bool(_), _) | (_, Value::Bool(_)) => {
                (left.boolean() == right.boolean()) == want_equal
            }
            (Value::Number(_), _) | (_, Value::Number(_)) => {
                (left.number(self.model) == right.number(self.model)) == want_equal
            }
            _ => test(
                &left.string_value(self.model),
                &right.string_value(self.model),
            ),
        }
    }

    /// `<`, `<=`, `>` and `>=` (§3.4): always a numeric comparison.
    fn relational(&self, op: BinaryOp, left: &Value<M::Node>, right: &Value<M::Node>) -> bool {
        let compare = |a: f64, b: f64| match op {
            BinaryOp::LessThan => a < b,
            BinaryOp::LessOrEqual => a <= b,
            BinaryOp::GreaterThan => a > b,
            _ => a >= b,
        };
        let numbers = |nodes: &[M::Node]| -> Vec<f64> {
            nodes
                .iter()
                .map(|node| crate::value::parse_number(&self.model.string_value(*node)))
                .collect()
        };
        match (left, right) {
            (Value::NodeSet(xs), Value::NodeSet(ys)) => {
                let ys = numbers(ys);
                numbers(xs)
                    .into_iter()
                    .any(|x| ys.iter().any(|y| compare(x, *y)))
            }
            (Value::NodeSet(xs), other) => {
                let other = other.number(self.model);
                numbers(xs).into_iter().any(|x| compare(x, other))
            }
            (other, Value::NodeSet(ys)) => {
                let other = other.number(self.model);
                numbers(ys).into_iter().any(|y| compare(other, y))
            }
            _ => compare(left.number(self.model), right.number(self.model)),
        }
    }
}

// ---------------------------------------------------------------------------
// Fast paths.
// ---------------------------------------------------------------------------

/// A recognised hot expression, evaluated by walking descendants once.
#[derive(Clone, Debug, PartialEq)]
enum FastPath {
    /// `.//*`, `//*`, `descendant::*`
    Elements(Descendants),
    /// `.//text()`, `//text()`
    Text(Descendants),
    /// `.//name`, `//name`
    Named(Descendants, String),
    /// `.//*[normalize-space(text())]`
    ElementsWithText(Descendants),
    /// `.//name/@attr`, `//@attr`
    Attribute(Descendants, Option<String>, String),
    /// `A | B` where both sides are fast paths — the link extractor's
    /// `.//a/@href| .//area/@href`.
    Union(Vec<FastPath>),
}

/// Where a fast path starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Descendants {
    /// From the context node.
    FromContext,
    /// From the document root.
    FromRoot,
}

impl FastPath {
    /// Recognise a fast path, or `None` to fall back to the general evaluator.
    fn detect(expr: &Expr) -> Option<Self> {
        if let Expr::Binary {
            op: BinaryOp::Union,
            lhs,
            rhs,
        } = expr
        {
            let mut branches = Vec::new();
            for side in [lhs.as_ref(), rhs.as_ref()] {
                match Self::detect(side)? {
                    Self::Union(inner) => branches.extend(inner),
                    other => branches.push(other),
                }
            }
            return Some(Self::Union(branches));
        }

        let Expr::Path(path) = expr else { return None };
        let start = match path.start {
            PathStart::Root => Descendants::FromRoot,
            PathStart::Context => Descendants::FromContext,
            PathStart::Filter { .. } => return None,
        };

        // Skip a leading no-op `self::node()` — what a relative `.//x` starts with.
        let mut steps = path.steps.as_slice();
        if let Some(first) = steps.first() {
            if first.axis == Axis::SelfAxis
                && first.node_test == NodeTest::Node
                && first.predicates.is_empty()
            {
                steps = &steps[1..];
            }
        }

        // An optional trailing `attribute::name` step.
        let mut attribute = None;
        if let Some(last) = steps.last() {
            if last.axis == Axis::Attribute && last.predicates.is_empty() {
                if let NodeTest::Name {
                    prefix: None,
                    local,
                } = &last.node_test
                {
                    attribute = Some(local.clone());
                    steps = &steps[..steps.len() - 1];
                }
            }
        }

        // What is left must be `descendant-or-self::node()/child::TEST` (the
        // expansion of `//`) or the equivalent `descendant::TEST`. Anything else —
        // including a bare `.//@href`, which is `descendant-or-self::node()`
        // followed by the attribute step and therefore also collects the *context*
        // node's own attributes — goes to the general evaluator.
        let (test, predicates) = match steps {
            [only] if only.axis == Axis::Descendant => (&only.node_test, &only.predicates),
            [first, second]
                if first.axis == Axis::DescendantOrSelf
                    && first.node_test == NodeTest::Node
                    && first.predicates.is_empty()
                    && second.axis == Axis::Child =>
            {
                (&second.node_test, &second.predicates)
            }
            _ => return None,
        };

        match (test, attribute) {
            // `.//a/@href`
            (
                NodeTest::Name {
                    prefix: None,
                    local,
                },
                Some(attribute),
            ) if predicates.is_empty() => {
                Some(Self::Attribute(start, Some(local.clone()), attribute))
            }
            // `//*/@href`
            (NodeTest::Wildcard, Some(attribute)) if predicates.is_empty() => {
                Some(Self::Attribute(start, None, attribute))
            }
            // `.//*` and `.//*[normalize-space(text())]`
            (NodeTest::Wildcard, None) => match predicates.as_slice() {
                [] => Some(Self::Elements(start)),
                [only] if is_normalize_space_text(only) => Some(Self::ElementsWithText(start)),
                _ => None,
            },
            // `.//text()`
            (NodeTest::Text, None) if predicates.is_empty() => Some(Self::Text(start)),
            // `.//a`
            (
                NodeTest::Name {
                    prefix: None,
                    local,
                },
                None,
            ) if predicates.is_empty() => Some(Self::Named(start, local.clone())),
            _ => None,
        }
    }

    /// Run the plan.
    fn run<M: TreeModel>(&self, model: &M, context: M::Node) -> Vec<M::Node> {
        let from = |start: Descendants| match start {
            Descendants::FromContext => context,
            Descendants::FromRoot => model.root_of(context),
        };
        match self {
            Self::Elements(start) => descendants(model, from(*start))
                .into_iter()
                .filter(|node| model.kind(*node) == NodeKind::Element)
                .collect(),
            Self::Text(start) => descendants(model, from(*start))
                .into_iter()
                .filter(|node| model.kind(*node) == NodeKind::Text)
                .collect(),
            Self::Named(start, local) => descendants(model, from(*start))
                .into_iter()
                .filter(|node| {
                    model.kind(*node) == NodeKind::Element
                        && model.name(*node).is_some_and(|(_, name)| name == local)
                })
                .collect(),
            Self::ElementsWithText(start) => descendants(model, from(*start))
                .into_iter()
                .filter(|node| {
                    model.kind(*node) == NodeKind::Element
                        && model
                            .children(*node)
                            .find(|child| model.kind(*child) == NodeKind::Text)
                            .is_some_and(|text| {
                                !normalize_space(&model.string_value(text)).is_empty()
                            })
                })
                .collect(),
            Self::Attribute(start, element, attribute) => {
                let mut out = Vec::new();
                for node in descendants(model, from(*start)) {
                    if model.kind(node) != NodeKind::Element {
                        continue;
                    }
                    if let Some(local) = element {
                        if !model.name(node).is_some_and(|(_, name)| name == local) {
                            continue;
                        }
                    }
                    out.extend(
                        model
                            .attributes(node)
                            .filter(|a| model.name(*a).is_some_and(|(_, name)| name == attribute)),
                    );
                }
                out
            }
            Self::Union(branches) => {
                let mut out = Vec::new();
                for branch in branches {
                    out.extend(branch.run(model, context));
                }
                out.sort_by_key(|node| model.document_order(*node));
                out.dedup();
                out
            }
        }
    }
}

/// Whether a predicate is exactly `normalize-space(text())`.
fn is_normalize_space_text(predicate: &Expr) -> bool {
    let Expr::Function { function, args } = predicate else {
        return false;
    };
    if *function != crate::ast::CoreFunction::NormalizeSpace || args.len() != 1 {
        return false;
    }
    let Expr::Path(path) = &args[0] else {
        return false;
    };
    matches!(path.start, PathStart::Context)
        && matches!(
            path.steps.as_slice(),
            [step] if step.axis == Axis::Child
                && step.node_test == NodeTest::Text
                && step.predicates.is_empty()
        )
}

// ---------------------------------------------------------------------------
// The public entry point.
// ---------------------------------------------------------------------------

/// A compiled XPath 1.0 expression.
///
/// ```
/// use browser_dom::Document;
/// use browser_xpath::eval::{Variables, XPath};
///
/// let doc = Document::parse("<ul><li>one</li><li>two</li></ul>");
/// let expr = XPath::compile("//li[2]/text()")?;
/// let nodes = expr.select_nodes(&doc, doc.root(), &Variables::new())?;
/// assert_eq!(nodes.len(), 1);
/// assert_eq!(nodes[0].string_value(), "two");
/// # Ok::<(), browser_xpath::error::XPathError>(())
/// ```
#[derive(Clone, Debug)]
pub struct XPath {
    source: String,
    ast: Expr,
    fast: Option<FastPath>,
}

impl XPath {
    /// Compile an expression.
    ///
    /// # Errors
    ///
    /// Every failure that can be decided from the expression alone happens here:
    /// [`XPathError::Syntax`], [`XPathError::Unsupported`] (unknown function,
    /// unknown axis, XPath 2.0 syntax) and [`XPathError::Arity`]. An unsupported
    /// construct is **never** allowed to become an empty node-set at run time.
    pub fn compile(expr: &str) -> Result<Self> {
        let ast = parse(expr)?;
        let fast = FastPath::detect(&ast);
        Ok(Self {
            source: expr.to_owned(),
            ast,
            fast,
        })
    }

    /// The expression as it was written.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The parsed expression. [`std::fmt::Display`] prints it unabbreviated.
    #[must_use]
    pub fn ast(&self) -> &Expr {
        &self.ast
    }

    /// Whether this expression is one of the recognised hot paths, evaluated by a
    /// single descendant walk. Purely an optimisation: the result is identical.
    #[must_use]
    pub fn is_fast_path(&self) -> bool {
        self.fast.is_some()
    }

    /// Evaluate against any [`TreeModel`].
    ///
    /// # Errors
    ///
    /// [`XPathError::UndefinedVariable`], [`XPathError::UndefinedPrefix`] and
    /// [`XPathError::NotANodeSet`] — the three failures that depend on the
    /// evaluation context rather than on the expression.
    pub fn eval<M: TreeModel>(
        &self,
        model: &M,
        context: M::Node,
        variables: &Variables,
    ) -> Result<Value<M::Node>> {
        self.eval_with(model, context, variables, &Namespaces::new())
    }

    /// Evaluate with namespace prefixes bound, for name tests such as `svg:rect`.
    ///
    /// # Errors
    ///
    /// As [`XPath::eval`].
    pub fn eval_with<M: TreeModel>(
        &self,
        model: &M,
        context: M::Node,
        variables: &Variables,
        namespaces: &Namespaces,
    ) -> Result<Value<M::Node>> {
        if let Some(fast) = &self.fast {
            return Ok(Value::NodeSet(fast.run(model, context)));
        }
        let evaluator = Evaluator {
            model,
            variables,
            namespaces,
        };
        evaluator.eval(&self.ast, Context::new(context))
    }

    /// Evaluate and require a node-set — what `Selector.xpath()` needs.
    ///
    /// # Errors
    ///
    /// As [`XPath::eval`], plus [`XPathError::NotANodeSet`] when the expression
    /// produces a string, number or boolean (`count(…)`, `string(…)`, `1 = 1`).
    pub fn select<M: TreeModel>(
        &self,
        model: &M,
        context: M::Node,
        variables: &Variables,
    ) -> Result<Vec<M::Node>> {
        self.eval(model, context, variables)?
            .into_node_set("this expression")
    }

    /// Evaluate over a [`Document`], returning the node-set.
    ///
    /// Builds a [`DomModel`] — one walk of the document — for this call. Build the
    /// model yourself and use [`XPath::eval`] when you evaluate more than one
    /// expression against the same document.
    ///
    /// # Errors
    ///
    /// As [`XPath::select`].
    pub fn select_nodes<'a>(
        &self,
        document: &'a Document,
        context: NodeRef<'a>,
        variables: &Variables,
    ) -> Result<Vec<XNode<'a>>> {
        let model = DomModel::new(document);
        self.select(&model, XNode::new(context), variables)
    }

    /// Evaluate over a [`Document`], returning the raw value.
    ///
    /// # Errors
    ///
    /// As [`XPath::eval`].
    pub fn evaluate<'a>(
        &self,
        document: &'a Document,
        context: NodeRef<'a>,
        variables: &Variables,
    ) -> Result<Value<XNode<'a>>> {
        let model = DomModel::new(document);
        self.eval(&model, XNode::new(context), variables)
    }
}

/// Compile and evaluate in one call, returning the node-set.
///
/// The convenience entry point `ARCHITECTURE.md` §1.4 calls "the concrete
/// convenience API": for repeated evaluation, compile once with
/// [`XPath::compile`] and keep one [`DomModel`].
///
/// ```
/// use browser_dom::Document;
///
/// let doc = Document::parse("<div><a href='/one'>one</a><a href='/two'>two</a></div>");
/// let hrefs = browser_xpath::eval::select(".//a/@href", &doc, doc.root())?;
/// let values: Vec<_> = hrefs.iter().map(|n| n.string_value()).collect();
/// assert_eq!(values, ["/one", "/two"]);
/// # Ok::<(), browser_xpath::error::XPathError>(())
/// ```
///
/// # Errors
///
/// As [`XPath::compile`] and [`XPath::select`].
pub fn select<'a>(
    expr: &str,
    document: &'a Document,
    context: NodeRef<'a>,
) -> Result<Vec<XNode<'a>>> {
    XPath::compile(expr)?.select_nodes(document, context, &Variables::new())
}

/// Compile and evaluate in one call, returning the raw value.
///
/// # Errors
///
/// As [`XPath::compile`] and [`XPath::eval`].
pub fn evaluate<'a>(
    expr: &str,
    document: &'a Document,
    context: NodeRef<'a>,
) -> Result<Value<XNode<'a>>> {
    XPath::compile(expr)?.evaluate(document, context, &Variables::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><body>
        <div id="list" class="items">
            <a href="/one" rel="next">one</a>
            <a href="/two">two</a>
            <p>tail text</p>
            <p>   </p>
        </div>
        <div id="other"><span>span text</span></div>
    </body></html>"#;

    fn doc() -> Document {
        Document::parse(PAGE)
    }

    fn strings(nodes: &[XNode<'_>]) -> Vec<String> {
        nodes
            .iter()
            .map(|node| node.string_value().into_owned())
            .collect()
    }

    fn eval_str(document: &Document, expr: &str) -> String {
        let model = DomModel::new(document);
        let compiled = XPath::compile(expr).expect(expr);
        compiled
            .eval(&model, model.root(), &Variables::new())
            .expect(expr)
            .string_value(&model)
    }

    fn eval_num(document: &Document, expr: &str) -> f64 {
        let model = DomModel::new(document);
        let compiled = XPath::compile(expr).expect(expr);
        compiled
            .eval(&model, model.root(), &Variables::new())
            .expect(expr)
            .number(&model)
    }

    fn nodes<'a>(document: &'a Document, expr: &str) -> Vec<XNode<'a>> {
        select(expr, document, document.root()).expect(expr)
    }

    #[test]
    fn paths_select_in_document_order() {
        let document = doc();
        assert_eq!(strings(&nodes(&document, "//a/@href")), ["/one", "/two"]);
        assert_eq!(strings(&nodes(&document, "//a/text()")), ["one", "two"]);
        assert_eq!(eval_num(&document, "count(//a)"), 2.0);
        assert_eq!(eval_num(&document, "count(//div)"), 2.0);
        // 10, not lxml's 9: html5ever always builds a `<head>`, libxml2 only
        // builds one when the source has head content. A tree difference, not an
        // engine one — `browser-dom`'s API.md documents it.
        assert_eq!(eval_num(&document, "count(//*)"), 10.0);
        // Union results are sorted and de-duplicated.
        assert_eq!(eval_num(&document, "count(//a | //a)"), 2.0);
        assert_eq!(eval_num(&document, "count(//a | //p)"), 4.0);
        assert_eq!(
            strings(&nodes(&document, "//p/text() | //a/@href")),
            ["/one", "/two", "tail text"]
        );
    }

    #[test]
    fn predicates_use_proximity_position() {
        let document = doc();
        assert_eq!(eval_str(&document, "string(//a[1]/@href)"), "/one");
        assert_eq!(eval_str(&document, "string(//a[2]/@href)"), "/two");
        assert_eq!(eval_str(&document, "string(//a[last()]/@href)"), "/two");
        assert_eq!(eval_str(&document, "string((//a)[1]/@href)"), "/one");
        assert_eq!(eval_num(&document, "count(//a[position() > 1])"), 1.0);
        // Reverse axis: [1] is the *nearest* preceding sibling.
        assert_eq!(
            eval_str(&document, "string(//p[1]/preceding-sibling::*[1]/@href)"),
            "/two"
        );
        assert_eq!(
            eval_str(
                &document,
                "string(//p[1]/preceding-sibling::a[last()]/@href)"
            ),
            "/one"
        );
        // Chained predicates renumber.
        assert_eq!(eval_num(&document, "count(//a[position()>0][2])"), 1.0);
        assert_eq!(eval_num(&document, "count(//a[1][1])"), 1.0);
        assert_eq!(eval_num(&document, "count(//a[1][2])"), 0.0);
    }

    #[test]
    fn variables_bind() {
        let document = doc();
        let model = DomModel::new(&document);
        let compiled = XPath::compile("//a[text()=$label]/@href").expect("compiles");
        let mut variables = Variables::new();
        variables.insert("label", "two");
        let value = compiled
            .eval(&model, model.root(), &variables)
            .expect("evaluates");
        assert_eq!(value.string_value(&model), "/two");

        // …and an unbound variable is an error, not an empty node-set.
        let error = compiled
            .eval(&model, model.root(), &Variables::new())
            .expect_err("unbound");
        assert!(matches!(error, XPathError::UndefinedVariable { .. }));
        assert!(!error.is_compile_time());

        // Numbers and booleans bind too.
        let compiled = XPath::compile("$n + 1").expect("compiles");
        let variables: Variables = [("n", 41.0)].into_iter().collect();
        assert_eq!(
            compiled
                .eval(&model, model.root(), &variables)
                .expect("evaluates"),
            Value::Number(42.0)
        );
    }

    #[test]
    fn comparisons_follow_the_node_set_rules() {
        let document = Document::parse(
            r#"<div><span data-rating="5">a</span><span data-rating="4">b</span></div>"#,
        );
        assert_eq!(eval_num(&document, "count(//span[@data-rating >= 4])"), 2.0);
        assert_eq!(eval_num(&document, "count(//span[@data-rating > 4])"), 1.0);
        assert_eq!(eval_num(&document, "count(//span[@data-rating = 5])"), 1.0);
        assert_eq!(
            eval_num(&document, "count(//span[@data-rating = '5'])"),
            1.0
        );
        assert_eq!(eval_num(&document, "count(//span[. = 'a'])"), 1.0);
        assert_eq!(eval_num(&document, "count(//span[@data-rating != 5])"), 1.0);
        // node-set = boolean converts the node-set to a boolean.
        assert_eq!(eval_str(&document, "//span = true()"), "true");
        assert_eq!(eval_str(&document, "//nothing = false()"), "true");
        // NaN never equals itself.
        assert_eq!(eval_str(&document, "number('x') = number('x')"), "false");
        assert_eq!(eval_str(&document, "'1' = 1"), "true");
        assert_eq!(eval_str(&document, "true() = 'x'"), "true");
        assert_eq!(eval_str(&document, "'' = false()"), "true");
        assert_eq!(eval_str(&document, "1 < '2'"), "true");
        assert_eq!(eval_str(&document, "'abc' < 'abd'"), "false");
    }

    #[test]
    fn arithmetic_matches_the_spec() {
        let document = doc();
        assert_eq!(eval_str(&document, "string(2 * 3 + 1)"), "7");
        assert_eq!(eval_str(&document, "string(1 div 0)"), "Infinity");
        assert_eq!(eval_str(&document, "string(-1 div 0)"), "-Infinity");
        assert_eq!(eval_str(&document, "string(0 div 0)"), "NaN");
        assert_eq!(eval_str(&document, "string(-3 mod 2)"), "-1");
        assert_eq!(eval_str(&document, "string(3 mod -2)"), "1");
        assert_eq!(eval_str(&document, "string(5 mod 1.5)"), "0.5");
        assert_eq!(eval_str(&document, "string(1 - -1)"), "2");
        assert_eq!(eval_str(&document, "string(- -2)"), "2");
        assert_eq!(eval_str(&document, "string(-'2')"), "-2");
        assert_eq!(eval_str(&document, "string(1 div 3)"), "0.333333333333333");
    }

    #[test]
    fn boolean_operators_short_circuit() {
        let document = doc();
        // `$undefined` would raise if it were evaluated.
        let model = DomModel::new(&document);
        let compiled = XPath::compile("1 = 1 or $undefined").expect("compiles");
        assert_eq!(
            compiled
                .eval(&model, model.root(), &Variables::new())
                .expect("short-circuits"),
            Value::Bool(true)
        );
        let compiled = XPath::compile("1 = 2 and $undefined").expect("compiles");
        assert_eq!(
            compiled
                .eval(&model, model.root(), &Variables::new())
                .expect("short-circuits"),
            Value::Bool(false)
        );
    }

    #[test]
    fn non_node_set_results_are_reported() {
        let document = doc();
        let error = select("count(//a)", &document, document.root()).expect_err("not a node-set");
        assert!(matches!(
            error,
            XPathError::NotANodeSet {
                actual: "number",
                ..
            }
        ));
        let error = select("'a' | 'b'", &document, document.root()).expect_err("not a node-set");
        assert!(matches!(error, XPathError::NotANodeSet { .. }));
        // …but the same expression through `evaluate` is fine.
        assert_eq!(
            evaluate("count(//a)", &document, document.root()).expect("evaluates"),
            Value::Number(2.0)
        );
    }

    #[test]
    fn context_node_scopes_relative_paths_but_not_absolute_ones() {
        let document = doc();
        let model = DomModel::new(&document);
        let other = XNode::new(
            document
                .elements()
                .find(|e| e.element_id() == Some("other"))
                .expect("#other"),
        );
        let relative = XPath::compile(".//span").expect("compiles");
        let absolute = XPath::compile("//a").expect("compiles");
        assert_eq!(
            relative
                .select(&model, other, &Variables::new())
                .expect("evaluates")
                .len(),
            1
        );
        // An absolute path searches the whole document whatever the context node,
        // which is what `Selector.xpath("//…")` relies on.
        assert_eq!(
            absolute
                .select(&model, other, &Variables::new())
                .expect("evaluates")
                .len(),
            2
        );
    }

    /// Each fast path must return exactly what the general evaluator returns.
    #[test]
    fn fast_paths_agree_with_the_general_evaluator() {
        let document = Document::parse(PAGE);
        let model = DomModel::new(&document);
        let context = model.root_element().expect("<html>");

        for expr in [
            ".//*",
            "//*",
            ".//text()",
            ".//a",
            "//a",
            ".//a/@href",
            "//*/@href",
            ".//*[normalize-space(text())]",
            ".//a/@href| .//area/@href",
            "descendant::*",
        ] {
            let compiled = XPath::compile(expr).expect(expr);
            assert!(compiled.is_fast_path(), "{expr} should be a fast path");
            let fast = compiled
                .select(&model, context, &Variables::new())
                .expect(expr);

            // The same expression with the fast path disabled.
            let slow_ast = XPath {
                source: compiled.source.clone(),
                ast: compiled.ast.clone(),
                fast: None,
            };
            let slow = slow_ast
                .select(&model, context, &Variables::new())
                .expect(expr);
            assert_eq!(fast, slow, "{expr}");
        }

        // Expressions that must *not* be mistaken for a fast path.
        for expr in [
            ".//*[1]",
            ".//a[@href]",
            // `.//@href` is `descendant-or-self::node()/attribute::href`, which
            // includes the *context* element's own attributes — not the same walk.
            ".//@href",
            "//a/@href/parent::*",
            ".//*[normalize-space(.)]",
            "(//a)[1]",
            ".//a/@href | count(//a)",
        ] {
            assert!(
                !XPath::compile(expr).expect(expr).is_fast_path(),
                "{expr} must not be a fast path"
            );
        }
    }

    #[test]
    fn fast_path_results_are_the_documented_ones() {
        let document = doc();
        assert_eq!(
            strings(&nodes(&document, ".//a/@href| .//p/text()")),
            ["/one", "/two", "tail text"]
        );
        // `.//*[normalize-space(text())]` skips the blank-only paragraph.
        let with_text = nodes(&document, ".//*[normalize-space(text())]");
        let tags: Vec<&str> = with_text
            .iter()
            .filter_map(|n| n.as_element().map(|e| e.tag()))
            .collect();
        assert_eq!(tags, ["a", "a", "p", "span"]);
    }

    #[test]
    fn namespaces_are_resolved_for_prefixed_tests() {
        let document = Document::parse(
            "<html><body><svg xmlns='http://www.w3.org/2000/svg'><rect/></svg></body></html>",
        );
        let model = DomModel::new(&document);
        let compiled = XPath::compile("//svg:rect").expect("compiles");

        let error = compiled
            .eval(&model, model.root(), &Variables::new())
            .expect_err("unbound prefix");
        assert!(matches!(error, XPathError::UndefinedPrefix { .. }));

        let mut namespaces = Namespaces::new();
        namespaces.insert("svg", "http://www.w3.org/2000/svg");
        let value = compiled
            .eval_with(&model, model.root(), &Variables::new(), &namespaces)
            .expect("evaluates");
        assert_eq!(value.as_node_set().map(<[XNode<'_>]>::len), Some(1));
    }

    /// A compiled expression is shared across spider workers, so it has to be
    /// `Send + Sync` — and so do the error and value types that come back.
    #[test]
    fn the_public_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<XPath>();
        assert_send_sync::<Variables>();
        assert_send_sync::<Namespaces>();
        assert_send_sync::<XPathError>();
        assert_send_sync::<Value<XNode<'static>>>();
    }

    /// 10 000 nested elements: every walk in the engine is iterative, so nothing
    /// here may overflow the stack.
    #[test]
    fn deep_documents_do_not_blow_the_stack() {
        const DEPTH: usize = 10_000;
        let mut html = String::with_capacity(DEPTH * 12);
        html.push_str("<html><body>");
        for _ in 0..DEPTH {
            html.push_str("<div>");
        }
        html.push_str("deep");
        for _ in 0..DEPTH {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");

        let document = Document::parse(&html);
        let model = DomModel::new(&document);
        let context = model.root_element().expect("<html>");
        let variables = Variables::new();
        let count = |expr: &str| -> f64 {
            XPath::compile(expr)
                .expect(expr)
                .eval(&model, context, &variables)
                .expect(expr)
                .number(&model)
        };

        assert_eq!(count("count(.//div)"), DEPTH as f64);
        assert_eq!(count("count(//div)"), DEPTH as f64);
        assert_eq!(count("count(.//text())"), 1.0);
        assert_eq!(count("count(.//div[last()])"), DEPTH as f64);
        assert_eq!(
            count("count(//div[not(div)]/ancestor::*)"),
            DEPTH as f64 + 1.0
        );
        assert_eq!(count("count(//div[not(div)]/preceding::*)"), 1.0);
        assert_eq!(count("count(//body/following::*)"), 0.0);
    }

    #[test]
    fn the_thirteen_axes_all_evaluate() {
        let document = doc();
        assert_eq!(eval_num(&document, "count(//p[1]/ancestor::*)"), 3.0);
        assert_eq!(
            eval_num(&document, "count(//p[1]/ancestor-or-self::*)"),
            4.0
        );
        assert_eq!(eval_num(&document, "count(//a[1]/attribute::*)"), 2.0);
        assert_eq!(eval_num(&document, "count(//div[1]/child::*)"), 4.0);
        assert_eq!(eval_num(&document, "count(//div[1]/descendant::*)"), 4.0);
        assert_eq!(
            eval_num(&document, "count(//div[1]/descendant-or-self::*)"),
            5.0
        );
        assert_eq!(eval_num(&document, "count(//a[1]/following::*)"), 5.0);
        assert_eq!(
            eval_num(&document, "count(//a[1]/following-sibling::*)"),
            3.0
        );
        assert_eq!(eval_num(&document, "count(//a[1]/namespace::*)"), 1.0);
        assert_eq!(eval_num(&document, "count(//a[1]/parent::*)"), 1.0);
        // 6 = div#list, a, a, p, p and the synthesised <head>.
        assert_eq!(eval_num(&document, "count(//span/preceding::*)"), 6.0);
        assert_eq!(
            eval_num(&document, "count(//p[2]/preceding-sibling::*)"),
            3.0
        );
        assert_eq!(eval_num(&document, "count(//a[1]/self::*)"), 1.0);
    }
}
