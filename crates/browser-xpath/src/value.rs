//! [`Value`] — node-set, string, number and boolean — with the XPath 1.0
//! conversion rules (§3, §4.2, §4.4), plus the two number/string conversions
//! that every XPath engine gets subtly wrong.
//!
//! # Number formatting
//!
//! [`format_number`] reproduces **`libxml2`'s** `xmlXPathFormatNumber`, not the
//! letter of the W3C REC, because `libxml2` is the engine this port replaces and
//! `Selector.xpath("string(…)")` results are observable Python behaviour:
//!
//! | expression | `libxml2` (and this crate) | strict XPath 1.0 |
//! |---|---|---|
//! | `string(1.0)` | `1` | `1` |
//! | `string(0 div 0)` | `NaN` | `NaN` |
//! | `string(1 div 0)` | `Infinity` | `Infinity` |
//! | `string(1 div 3)` | `0.333333333333333` | `0.3333333333333333` |
//! | `string(100000*100000*100000)` | `1e+15` | `1000000000000000` |
//!
//! The algorithm: `NaN`/`±Infinity`/`0` (including `-0`) are spelled out; a value
//! that round-trips through a C `int` is printed as an integer; anything larger
//! than `1e9` or smaller than `1e-5` uses scientific notation with 15 significant
//! digits; everything else uses fixed notation with `15 - ⌊log10|x|⌋` fraction
//! digits. Trailing fractional zeroes — and a trailing `.` — are then removed.

use std::borrow::Cow;

use crate::error::{Result, XPathError};
use crate::model::TreeModel;

/// One of the four XPath 1.0 value types.
#[derive(Clone, Debug, PartialEq)]
pub enum Value<N> {
    /// A set of nodes. The engine always hands these back sorted in document
    /// order and de-duplicated.
    NodeSet(Vec<N>),
    /// A string.
    Str(String),
    /// A number — an IEEE 754 double, so `NaN` and `±Infinity` are values.
    Number(f64),
    /// A boolean.
    Bool(bool),
}

impl<N: Copy + Eq + Ord> Value<N> {
    /// An empty node-set — the value of a location path that selects nothing.
    #[must_use]
    pub fn empty_node_set() -> Self {
        Self::NodeSet(Vec::new())
    }

    /// The XPath type name, for error messages: `node-set`, `string`, `number`
    /// or `boolean`.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::NodeSet(_) => "node-set",
            Self::Str(_) => "string",
            Self::Number(_) => "number",
            Self::Bool(_) => "boolean",
        }
    }

    /// Whether this is a node-set.
    #[must_use]
    pub fn is_node_set(&self) -> bool {
        matches!(self, Self::NodeSet(_))
    }

    /// The nodes, if this is a node-set.
    #[must_use]
    pub fn as_node_set(&self) -> Option<&[N]> {
        match self {
            Self::NodeSet(nodes) => Some(nodes),
            _ => None,
        }
    }

    /// The nodes, consuming the value.
    ///
    /// # Errors
    ///
    /// [`XPathError::NotANodeSet`] when the value is a string, number or boolean.
    /// `context` names the operator or function that required a node-set — the
    /// only run-time type error XPath 1.0 has.
    pub fn into_node_set(self, context: &str) -> Result<Vec<N>> {
        match self {
            Self::NodeSet(nodes) => Ok(nodes),
            other => Err(XPathError::NotANodeSet {
                context: context.to_owned(),
                actual: other.type_name(),
            }),
        }
    }

    /// The `boolean()` conversion (§4.3).
    ///
    /// * node-set → non-empty
    /// * number → not zero and not `NaN`
    /// * string → not empty (so `boolean('0')` is **true**)
    /// * boolean → itself
    #[must_use]
    pub fn boolean(&self) -> bool {
        match self {
            Self::NodeSet(nodes) => !nodes.is_empty(),
            Self::Str(text) => !text.is_empty(),
            Self::Number(value) => *value != 0.0 && !value.is_nan(),
            Self::Bool(value) => *value,
        }
    }

    /// The `string()` conversion (§4.2).
    ///
    /// A node-set converts to the string-value of the node that comes **first in
    /// document order**, or `""` when it is empty.
    #[must_use]
    pub fn string_value<M: TreeModel<Node = N>>(&self, model: &M) -> String {
        match self {
            Self::NodeSet(nodes) => match first_in_document_order(model, nodes) {
                Some(node) => model.string_value(node).into_owned(),
                None => String::new(),
            },
            Self::Str(text) => text.clone(),
            Self::Number(value) => format_number(*value),
            Self::Bool(true) => "true".to_owned(),
            Self::Bool(false) => "false".to_owned(),
        }
    }

    /// Like [`Value::string_value`], but borrows when it can.
    #[must_use]
    pub fn string_cow<'v, M: TreeModel<Node = N>>(&'v self, model: &'v M) -> Cow<'v, str> {
        match self {
            Self::Str(text) => Cow::Borrowed(text.as_str()),
            Self::NodeSet(nodes) => match first_in_document_order(model, nodes) {
                Some(node) => model.string_value(node),
                None => Cow::Borrowed(""),
            },
            other => Cow::Owned(other.string_value(model)),
        }
    }

    /// The `number()` conversion (§4.4).
    ///
    /// Strings that are not a valid XPath number convert to `NaN`; booleans
    /// convert to `1`/`0`; a node-set converts through its string-value.
    #[must_use]
    pub fn number<M: TreeModel<Node = N>>(&self, model: &M) -> f64 {
        match self {
            Self::Number(value) => *value,
            Self::Bool(true) => 1.0,
            Self::Bool(false) => 0.0,
            Self::Str(text) => parse_number(text),
            Self::NodeSet(_) => parse_number(&self.string_cow(model)),
        }
    }
}

/// The node of `nodes` that comes first in document order.
fn first_in_document_order<M: TreeModel>(model: &M, nodes: &[M::Node]) -> Option<M::Node> {
    nodes
        .iter()
        .copied()
        .min_by_key(|node| model.document_order(*node))
}

// ---------------------------------------------------------------------------
// Number ⇄ string.
// ---------------------------------------------------------------------------

/// `libxml2`'s cut-off above which numbers are printed in scientific notation.
const UPPER_DOUBLE: f64 = 1e9;
/// `libxml2`'s cut-off below which numbers are printed in scientific notation.
const LOWER_DOUBLE: f64 = 1e-5;
/// `DBL_DIG` — the number of significant decimal digits `libxml2` prints.
const DBL_DIG: i32 = 15;

/// Format a number the way `string()` does.
///
/// ```
/// use browser_xpath::value::format_number;
///
/// assert_eq!(format_number(1.0), "1");
/// assert_eq!(format_number(-0.0), "0");
/// assert_eq!(format_number(1.5), "1.5");
/// assert_eq!(format_number(f64::NAN), "NaN");
/// assert_eq!(format_number(f64::INFINITY), "Infinity");
/// assert_eq!(format_number(f64::NEG_INFINITY), "-Infinity");
/// assert_eq!(format_number(1.0 / 3.0), "0.333333333333333");
/// assert_eq!(format_number(1e15), "1e+15");
/// ```
#[must_use]
pub fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if value == 0.0 {
        // Also catches -0.0, which XPath prints as "0".
        return "0".to_owned();
    }
    // The C `int` fast path: `(number > INT_MIN) && (number < INT_MAX) && (number == (int) number)`.
    if value > f64::from(i32::MIN) && value < f64::from(i32::MAX) && value == value.trunc() {
        return format!("{}", value as i64);
    }

    let absolute = value.abs();
    // libxml2: `(absolute_value > UPPER_DOUBLE) || (absolute_value < LOWER_DOUBLE)`.
    let rendered = if !(LOWER_DOUBLE..=UPPER_DOUBLE).contains(&absolute) {
        scientific(value, (DBL_DIG - 1) as usize)
    } else {
        let integer_place = absolute.log10().trunc() as i32;
        let fraction_place = if integer_place > 0 {
            DBL_DIG - integer_place - 1
        } else {
            DBL_DIG - integer_place
        };
        format!("{value:.*}", fraction_place.max(0) as usize)
    };
    trim_fraction(&rendered)
}

/// C's `%.<precision>e`: a mantissa plus a signed, at-least-two-digit exponent.
fn scientific(value: f64, precision: usize) -> String {
    let rendered = format!("{value:.precision$e}");
    match rendered.split_once('e') {
        Some((mantissa, exponent)) => {
            let (sign, digits) = match exponent.strip_prefix('-') {
                Some(digits) => ('-', digits),
                None => ('+', exponent.strip_prefix('+').unwrap_or(exponent)),
            };
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => rendered,
    }
}

/// Drop trailing zeroes from the fractional part, and the `.` if nothing is left.
///
/// Operates on the mantissa only, so `1.00000000000000e+15` becomes `1e+15`.
fn trim_fraction(rendered: &str) -> String {
    let (mantissa, exponent) = match rendered.find(['e', 'E']) {
        Some(index) => rendered.split_at(index),
        None => (rendered, ""),
    };
    if !mantissa.contains('.') {
        return rendered.to_owned();
    }
    let trimmed = mantissa.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    format!("{trimmed}{exponent}")
}

/// The `number(string)` conversion (§4.4), as `libxml2` implements it.
///
/// Optional surrounding whitespace, an optional leading `-`, then digits with an
/// optional fractional part — plus `libxml2`'s exponent extension. Anything else
/// is `NaN`.
///
/// ```
/// use browser_xpath::value::parse_number;
///
/// assert_eq!(parse_number("  12.5 "), 12.5);
/// assert_eq!(parse_number(".5"), 0.5);
/// assert_eq!(parse_number("5."), 5.0);
/// assert_eq!(parse_number("1e5"), 100000.0);
/// assert!(parse_number("+5").is_nan());
/// assert!(parse_number("0x10").is_nan());
/// assert!(parse_number("").is_nan());
/// ```
#[must_use]
pub fn parse_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'));
    let chars: Vec<char> = trimmed.chars().collect();
    let mut index = 0;
    let mut buffer = String::with_capacity(chars.len());

    if chars.get(index) == Some(&'-') {
        buffer.push('-');
        index += 1;
    }
    let mut digits = 0;
    while chars.get(index).is_some_and(char::is_ascii_digit) {
        buffer.push(chars[index]);
        index += 1;
        digits += 1;
    }
    if chars.get(index) == Some(&'.') {
        buffer.push('.');
        index += 1;
        while chars.get(index).is_some_and(char::is_ascii_digit) {
            buffer.push(chars[index]);
            index += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return f64::NAN;
    }
    // `libxml2` consumes an exponent marker even when no digits follow it, so
    // `number('1e')` is 1 rather than NaN.
    if matches!(chars.get(index), Some('e' | 'E')) {
        index += 1;
        let mut exponent = String::new();
        if matches!(chars.get(index), Some('+' | '-')) {
            exponent.push(chars[index]);
            index += 1;
        }
        let mut exponent_digits = 0;
        while chars.get(index).is_some_and(char::is_ascii_digit) {
            exponent.push(chars[index]);
            index += 1;
            exponent_digits += 1;
        }
        if exponent_digits > 0 {
            buffer.push('e');
            buffer.push_str(&exponent);
        }
    }
    if index != chars.len() {
        return f64::NAN;
    }
    buffer.parse::<f64>().unwrap_or(f64::NAN)
}

/// The `normalize-space()` conversion (§4.2): leading and trailing whitespace
/// stripped, internal runs collapsed to a single space.
///
/// XPath whitespace is `#x20`, `#x9`, `#xD` and `#xA` only — `\u{a0}` is *not*
/// whitespace, which is why `normalize-space('\u{a0}a')` keeps the NBSP.
#[must_use]
pub fn normalize_space(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// `round()` (§4.4): half rounds **towards positive infinity**, so `round(-0.5)`
/// is `-0` and `round(-1.5)` is `-1`.
#[must_use]
pub fn round_half_up(value: f64) -> f64 {
    if value.is_nan() || value.is_infinite() {
        return value;
    }
    // `(x + 0.5).floor()` is wrong for values just below .5 that the addition
    // rounds up, so use the exact rule instead.
    let floor = value.floor();
    let rounded = if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    };
    // §4.4 spells the sign of zero out: "if the argument is negative zero or is
    // less than zero but greater than or equal to -0.5 then negative zero is
    // returned". `-1.0 + 1.0` is `+0`, so restore the sign by hand.
    if rounded == 0.0 && value.is_sign_negative() {
        -0.0
    } else {
        rounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::Document;

    use crate::model::{DomModel, XNode};

    /// Every expectation below was produced by `lxml` 6.1.2 / `libxml2` 2.14.6:
    /// `etree.HTML("<p/>").xpath("string(<expr>)")`.
    #[test]
    fn number_formatting_matches_libxml2() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1"),
            (0.0, "0"),
            (-0.0, "0"),
            (-1.0, "-1"),
            (1.5, "1.5"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (0.1 + 0.2, "0.3"),
            (1.0 / 3.0, "0.333333333333333"),
            (2.0 / 3.0, "0.666666666666667"),
            (1.0 / 7.0, "0.142857142857143"),
            (1.0000000000000002, "1"),
            (123.456, "123.456"),
            (0.0001, "0.0001"),
            (0.00001, "0.00001"),
            (0.000001, "1e-06"),
            (1e-7, "1e-07"),
            (-1e-7, "-1e-07"),
            (5e-6, "5e-06"),
            (-0.000_123_456_789_012_345_67, "-0.000123456789012346"),
            (1e9, "1000000000"),
            (1000000000.5, "1.0000000005e+09"),
            (3e9, "3e+09"),
            (2147483647.0, "2.147483647e+09"),
            (2147483648.0, "2.147483648e+09"),
            (-2147483648.0, "-2.147483648e+09"),
            (1e15, "1e+15"),
            (123456789012345678.0, "1.23456789012346e+17"),
            (12345678901234567890.0, "1.23456789012346e+19"),
            (-1234567890.5, "-1.2345678905e+09"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for &(value, expected) in cases {
            assert_eq!(format_number(value), expected, "format_number({value})");
        }
    }

    #[test]
    fn string_to_number_matches_libxml2() {
        assert_eq!(parse_number("12.5"), 12.5);
        assert_eq!(parse_number("  12.5  "), 12.5);
        assert_eq!(parse_number("-12.5"), -12.5);
        assert_eq!(parse_number("\t\r\n1\n"), 1.0);
        assert_eq!(parse_number(".5"), 0.5);
        assert_eq!(parse_number("-.5"), -0.5);
        assert_eq!(parse_number("5."), 5.0);
        assert_eq!(parse_number("1e5"), 100_000.0);
        assert_eq!(parse_number("1E-2"), 0.01);
        assert_eq!(parse_number("  1e5  "), 100_000.0);
        assert_eq!(parse_number("1e"), 1.0);
        assert_eq!(parse_number("0"), 0.0);
        for bad in [
            "+5", "0x10", "INF", "nan", "Infinity", "", "  ", "1 2", "abc", "1.2.3", "-", ".",
        ] {
            assert!(parse_number(bad).is_nan(), "number({bad:?}) should be NaN");
        }
    }

    #[test]
    fn round_is_half_towards_positive_infinity() {
        assert_eq!(round_half_up(0.5), 1.0);
        assert_eq!(round_half_up(1.5), 2.0);
        assert_eq!(round_half_up(-1.5), -1.0);
        // `round()` of a negative argument that lands on zero keeps the sign,
        // which `assert_eq!` alone cannot see: `0.0 == -0.0`.
        assert!(round_half_up(-0.4).is_sign_negative());
        assert!(round_half_up(-0.5).is_sign_negative());
        assert_eq!(round_half_up(-0.5), -0.0);
        assert_eq!(format_number(round_half_up(-0.5)), "0");
        assert!(round_half_up(-0.0).is_sign_negative());
        assert!(!round_half_up(0.4).is_sign_negative());
        assert_eq!(round_half_up(2.5), 3.0);
        assert!(round_half_up(f64::NAN).is_nan());
        assert_eq!(round_half_up(f64::INFINITY), f64::INFINITY);
    }

    #[test]
    fn normalize_space_uses_xpath_whitespace() {
        assert_eq!(normalize_space("  a \t b \n c  "), "a b c");
        assert_eq!(normalize_space(" a"), "a");
        assert_eq!(normalize_space(""), "");
        assert_eq!(normalize_space("   "), "");
        assert_eq!(normalize_space("\u{a0}a"), "\u{a0}a");
    }

    #[test]
    fn conversions_follow_the_spec() {
        let doc = Document::parse("<div><p>hello</p><p>world</p></div>");
        let model = DomModel::new(&doc);
        let paragraphs: Vec<XNode<'_>> = doc
            .elements()
            .filter(|e| e.tag() == "p")
            .map(XNode::new)
            .collect();
        let node_set = Value::NodeSet(paragraphs);

        // boolean()
        assert!(node_set.boolean());
        assert!(!Value::<XNode<'_>>::empty_node_set().boolean());
        assert!(Value::<XNode<'_>>::Str("0".into()).boolean());
        assert!(!Value::<XNode<'_>>::Str(String::new()).boolean());
        assert!(!Value::<XNode<'_>>::Number(0.0).boolean());
        assert!(!Value::<XNode<'_>>::Number(f64::NAN).boolean());
        assert!(Value::<XNode<'_>>::Number(-1.0).boolean());

        // string(): the first node in document order.
        assert_eq!(node_set.string_value(&model), "hello");
        assert_eq!(
            Value::<XNode<'_>>::empty_node_set().string_value(&model),
            ""
        );
        assert_eq!(Value::<XNode<'_>>::Bool(true).string_value(&model), "true");
        assert_eq!(Value::<XNode<'_>>::Number(1.0).string_value(&model), "1");

        // number()
        assert!(node_set.number(&model).is_nan());
        assert_eq!(Value::<XNode<'_>>::Bool(true).number(&model), 1.0);
        assert_eq!(Value::<XNode<'_>>::Str("12".into()).number(&model), 12.0);

        // type errors
        assert_eq!(node_set.type_name(), "node-set");
        let error = Value::<XNode<'_>>::Number(1.0)
            .into_node_set("the `|` operator")
            .expect_err("not a node-set");
        assert!(matches!(
            error,
            XPathError::NotANodeSet {
                actual: "number",
                ..
            }
        ));
    }

    /// The node-set → string rule is "first in document order", not "first in the
    /// vector", so a scrambled node-set converts identically.
    #[test]
    fn node_set_string_ignores_vector_order() {
        let doc = Document::parse("<div><p>hello</p><p>world</p></div>");
        let model = DomModel::new(&doc);
        let mut paragraphs: Vec<XNode<'_>> = doc
            .elements()
            .filter(|e| e.tag() == "p")
            .map(XNode::new)
            .collect();
        paragraphs.reverse();
        assert_eq!(Value::NodeSet(paragraphs).string_value(&model), "hello");
    }
}
