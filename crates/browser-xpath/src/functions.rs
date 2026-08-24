//! The 27 core functions of XPath 1.0 (§4).
//!
//! | §4.1 node-set | §4.2 string | §4.3 boolean | §4.4 number |
//! |---|---|---|---|
//! | `last` `position` `count` `id` `local-name` `namespace-uri` `name` | `string` `concat` `starts-with` `contains` `substring-before` `substring-after` `substring` `string-length` `normalize-space` `translate` | `boolean` `not` `true` `false` `lang` | `number` `sum` `floor` `ceiling` `round` |
//!
//! Anything not in that table is rejected by [`crate::eval::XPath::compile`] with
//! [`crate::error::XPathError::Unsupported`], and the argument counts are checked
//! there too — so nothing here can be reached with the wrong arity.
//!
//! The string functions operate on **characters** (Unicode scalar values), not
//! bytes and not UTF-16 code units: `string-length('😀')` is 1, exactly as
//! `libxml2` reports it.

use crate::ast::CoreFunction;
use crate::error::Result;
use crate::eval::{Context, Evaluator};
use crate::model::{NodeKind, TreeModel, XML_NAMESPACE};
use crate::value::{format_number, normalize_space, round_half_up, Value};

/// Evaluate a core function call.
pub(crate) fn call<M: TreeModel>(
    evaluator: &Evaluator<'_, M>,
    function: CoreFunction,
    args: &[crate::ast::Expr],
    context: Context<M::Node>,
) -> Result<Value<M::Node>> {
    match function {
        // -- §4.1 node-set functions ---------------------------------------
        #[allow(clippy::cast_precision_loss)]
        CoreFunction::Last => Ok(Value::Number(context.size as f64)),
        #[allow(clippy::cast_precision_loss)]
        CoreFunction::Position => Ok(Value::Number(context.position as f64)),
        #[allow(clippy::cast_precision_loss)]
        CoreFunction::Count => {
            let nodes = evaluator.node_set(&args[0], context, "count()")?;
            Ok(Value::Number(nodes.len() as f64))
        }
        CoreFunction::Id => {
            let value = evaluator.eval(&args[0], context)?;
            let wanted: Vec<String> = match &value {
                Value::NodeSet(nodes) => nodes
                    .iter()
                    .flat_map(|node| {
                        evaluator
                            .model
                            .string_value(*node)
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .collect(),
                other => other
                    .string_value(evaluator.model)
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            };
            let mut found = Vec::new();
            if !wanted.is_empty() {
                let root = evaluator.model.root_of(context.node);
                for node in crate::axes::descendants(evaluator.model, root) {
                    if evaluator.model.kind(node) != NodeKind::Element {
                        continue;
                    }
                    if let Some(id) = evaluator.model.attribute_value(node, "id") {
                        if wanted.iter().any(|want| want == id.as_ref()) {
                            found.push(node);
                        }
                    }
                }
            }
            evaluator.sort_dedup(&mut found);
            Ok(Value::NodeSet(found))
        }
        CoreFunction::LocalName => {
            let node = optional_node(evaluator, args, context)?;
            Ok(Value::Str(match node {
                Some(node) => evaluator
                    .model
                    .name(node)
                    .map_or_else(String::new, |(_, local)| local.to_owned()),
                None => String::new(),
            }))
        }
        CoreFunction::NamespaceUri => {
            let node = optional_node(evaluator, args, context)?;
            Ok(Value::Str(match node {
                Some(node) => evaluator
                    .model
                    .name(node)
                    .map_or_else(String::new, |(uri, _)| uri.to_owned()),
                None => String::new(),
            }))
        }
        CoreFunction::Name => {
            let node = optional_node(evaluator, args, context)?;
            // The tree carries no prefixes — html5ever records `xlink:href` as
            // (namespace, local) and HTML elements have no prefix at all — so the
            // expanded name's local part *is* the QName, which is also what
            // lxml's HTML parser reports.
            Ok(Value::Str(match node {
                Some(node) => evaluator
                    .model
                    .name(node)
                    .map_or_else(String::new, |(_, local)| local.to_owned()),
                None => String::new(),
            }))
        }

        // -- §4.2 string functions -----------------------------------------
        CoreFunction::String => Ok(Value::Str(optional_string(evaluator, args, context)?)),
        CoreFunction::Concat => {
            let mut out = String::new();
            for arg in args {
                out.push_str(&evaluator.eval(arg, context)?.string_value(evaluator.model));
            }
            Ok(Value::Str(out))
        }
        CoreFunction::StartsWith => {
            let haystack = string_arg(evaluator, args, 0, context)?;
            let needle = string_arg(evaluator, args, 1, context)?;
            Ok(Value::Bool(haystack.starts_with(&needle)))
        }
        CoreFunction::Contains => {
            let haystack = string_arg(evaluator, args, 0, context)?;
            let needle = string_arg(evaluator, args, 1, context)?;
            Ok(Value::Bool(haystack.contains(&needle)))
        }
        CoreFunction::SubstringBefore => {
            let haystack = string_arg(evaluator, args, 0, context)?;
            let needle = string_arg(evaluator, args, 1, context)?;
            Ok(Value::Str(substring_before(&haystack, &needle).to_owned()))
        }
        CoreFunction::SubstringAfter => {
            let haystack = string_arg(evaluator, args, 0, context)?;
            let needle = string_arg(evaluator, args, 1, context)?;
            Ok(Value::Str(substring_after(&haystack, &needle).to_owned()))
        }
        CoreFunction::Substring => {
            let text = string_arg(evaluator, args, 0, context)?;
            let start = evaluator.eval(&args[1], context)?.number(evaluator.model);
            let length = match args.get(2) {
                Some(arg) => Some(evaluator.eval(arg, context)?.number(evaluator.model)),
                None => None,
            };
            Ok(Value::Str(substring(&text, start, length)))
        }
        #[allow(clippy::cast_precision_loss)]
        CoreFunction::StringLength => {
            let text = optional_string(evaluator, args, context)?;
            Ok(Value::Number(text.chars().count() as f64))
        }
        CoreFunction::NormalizeSpace => {
            let text = optional_string(evaluator, args, context)?;
            Ok(Value::Str(normalize_space(&text)))
        }
        CoreFunction::Translate => {
            let text = string_arg(evaluator, args, 0, context)?;
            let from = string_arg(evaluator, args, 1, context)?;
            let to = string_arg(evaluator, args, 2, context)?;
            Ok(Value::Str(translate(&text, &from, &to)))
        }

        // -- §4.3 boolean functions ----------------------------------------
        CoreFunction::Boolean => Ok(Value::Bool(evaluator.eval(&args[0], context)?.boolean())),
        CoreFunction::Not => Ok(Value::Bool(!evaluator.eval(&args[0], context)?.boolean())),
        CoreFunction::True => Ok(Value::Bool(true)),
        CoreFunction::False => Ok(Value::Bool(false)),
        CoreFunction::Lang => {
            let wanted = string_arg(evaluator, args, 0, context)?;
            Ok(Value::Bool(lang(evaluator, context.node, &wanted)))
        }

        // -- §4.4 number functions -----------------------------------------
        CoreFunction::Number => match args.first() {
            Some(arg) => Ok(Value::Number(
                evaluator.eval(arg, context)?.number(evaluator.model),
            )),
            None => Ok(Value::Number(crate::value::parse_number(
                &evaluator.model.string_value(context.node),
            ))),
        },
        CoreFunction::Sum => {
            let nodes = evaluator.node_set(&args[0], context, "sum()")?;
            let total = nodes.iter().fold(0.0, |total, node| {
                total + crate::value::parse_number(&evaluator.model.string_value(*node))
            });
            Ok(Value::Number(total))
        }
        CoreFunction::Floor => Ok(Value::Number(
            evaluator
                .eval(&args[0], context)?
                .number(evaluator.model)
                .floor(),
        )),
        CoreFunction::Ceiling => Ok(Value::Number(
            evaluator
                .eval(&args[0], context)?
                .number(evaluator.model)
                .ceil(),
        )),
        CoreFunction::Round => Ok(Value::Number(round_half_up(
            evaluator.eval(&args[0], context)?.number(evaluator.model),
        ))),
    }
}

/// Evaluate `args[index]` as a string.
fn string_arg<M: TreeModel>(
    evaluator: &Evaluator<'_, M>,
    args: &[crate::ast::Expr],
    index: usize,
    context: Context<M::Node>,
) -> Result<String> {
    Ok(evaluator
        .eval(&args[index], context)?
        .string_value(evaluator.model))
}

/// The first argument as a string, defaulting to the string-value of the context
/// node — the `string()`, `string-length()` and `normalize-space()` shape.
fn optional_string<M: TreeModel>(
    evaluator: &Evaluator<'_, M>,
    args: &[crate::ast::Expr],
    context: Context<M::Node>,
) -> Result<String> {
    match args.first() {
        Some(arg) => Ok(evaluator.eval(arg, context)?.string_value(evaluator.model)),
        None => Ok(evaluator.model.string_value(context.node).into_owned()),
    }
}

/// The first node in document order of the first argument's node-set, defaulting
/// to the context node — the `name()`, `local-name()`, `namespace-uri()` shape.
///
/// `None` means "an empty node-set was given", which those three functions turn
/// into `""`.
fn optional_node<M: TreeModel>(
    evaluator: &Evaluator<'_, M>,
    args: &[crate::ast::Expr],
    context: Context<M::Node>,
) -> Result<Option<M::Node>> {
    let Some(arg) = args.first() else {
        return Ok(Some(context.node));
    };
    let nodes = evaluator.node_set(arg, context, "this function")?;
    Ok(nodes
        .into_iter()
        .min_by_key(|node| evaluator.model.document_order(*node)))
}

/// `lang(string)` (§4.3): does the context node's language match?
///
/// The language comes from the nearest `xml:lang` attribute on an ancestor-or-self
/// element, and matching is case-insensitive, with `en` matching `en-GB`.
///
/// **HTML's plain `lang=` attribute is not consulted**, and neither is the
/// un-namespaced `xml:lang` that html5ever produces on an HTML element (its local
/// name is literally `xml:lang`). `libxml2` behaves identically — `lang('en')` is
/// false on `<div lang="en"><p/></div>` *and* on `<div xml:lang="en"><p/></div>`
/// in both engines — so `lang()` is only ever true inside SVG/MathML content,
/// where the attribute really is namespaced.
fn lang<M: TreeModel>(evaluator: &Evaluator<'_, M>, node: M::Node, wanted: &str) -> bool {
    let model = evaluator.model;
    let mut current = Some(node);
    while let Some(candidate) = current {
        if model.kind(candidate) == NodeKind::Element {
            let found = model.attributes(candidate).find(|attribute| {
                model
                    .name(*attribute)
                    .is_some_and(|(uri, local)| uri == XML_NAMESPACE && local == "lang")
            });
            if let Some(attribute) = found {
                return lang_matches(&model.string_value(attribute), wanted);
            }
        }
        current = model.parent(candidate);
    }
    false
}

/// Whether `declared` (an `xml:lang` value) satisfies a `lang(wanted)` test.
///
/// ```
/// use browser_xpath::functions::lang_matches;
///
/// assert!(lang_matches("en", "en"));
/// assert!(lang_matches("EN", "en"));
/// assert!(lang_matches("en-GB", "en"));
/// assert!(!lang_matches("en", "en-GB"));
/// assert!(!lang_matches("eng", "en"));
/// ```
#[must_use]
pub fn lang_matches(declared: &str, wanted: &str) -> bool {
    if declared.eq_ignore_ascii_case(wanted) {
        return true;
    }
    declared.len() > wanted.len()
        && declared[..wanted.len()].eq_ignore_ascii_case(wanted)
        && declared.as_bytes().get(wanted.len()) == Some(&b'-')
}

/// `substring-before(haystack, needle)` (§4.2).
#[must_use]
pub fn substring_before<'a>(haystack: &'a str, needle: &str) -> &'a str {
    match haystack.find(needle) {
        Some(index) => &haystack[..index],
        None => "",
    }
}

/// `substring-after(haystack, needle)` (§4.2).
#[must_use]
pub fn substring_after<'a>(haystack: &'a str, needle: &str) -> &'a str {
    match haystack.find(needle) {
        Some(index) => &haystack[index + needle.len()..],
        None => "",
    }
}

/// `substring(text, start, length?)` (§4.2), including the `NaN`/infinity rules.
///
/// The characters kept are those at 1-based position `p` with
/// `p >= round(start)` and, when a length is given, `p < round(start) + round(length)`.
/// Every comparison is IEEE 754, so a `NaN` bound keeps nothing.
///
/// ```
/// use browser_xpath::functions::substring;
///
/// assert_eq!(substring("12345", 1.5, Some(2.6)), "234");
/// assert_eq!(substring("12345", 0.0, Some(3.0)), "12");
/// assert_eq!(substring("12345", 2.0, None), "2345");
/// assert_eq!(substring("12345", -42.0, Some(f64::INFINITY)), "12345");
/// assert_eq!(substring("12345", f64::NEG_INFINITY, Some(f64::INFINITY)), "");
/// assert_eq!(substring("12345", f64::NAN, None), "");
/// ```
#[must_use]
pub fn substring(text: &str, start: f64, length: Option<f64>) -> String {
    let start = round_half_up(start);
    let end = match length {
        // -inf + inf is NaN, which keeps nothing — the spec's own example.
        Some(length) => start + round_half_up(length),
        None => f64::INFINITY,
    };
    text.chars()
        .enumerate()
        .filter_map(|(index, character)| {
            #[allow(clippy::cast_precision_loss)]
            let position = (index + 1) as f64;
            (position >= start && position < end).then_some(character)
        })
        .collect()
}

/// `translate(text, from, to)` (§4.2).
///
/// A character of `text` that occurs in `from` is replaced by the character at the
/// same position in `to`; if `to` is shorter, the character is **removed**. Only
/// the first occurrence of a character in `from` counts.
///
/// ```
/// use browser_xpath::functions::translate;
///
/// assert_eq!(translate("bar", "abc", "ABC"), "BAr");
/// assert_eq!(translate("--aaa--", "abc-", "ABC"), "AAA");
/// assert_eq!(translate("1,234", ",", ""), "1234");
/// ```
#[must_use]
pub fn translate(text: &str, from: &str, to: &str) -> String {
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match from.iter().position(|c| *c == character) {
            Some(index) => {
                if let Some(replacement) = to.get(index) {
                    out.push(*replacement);
                }
            }
            None => out.push(character),
        }
    }
    out
}

/// The string a number formats to — re-exported from [`crate::value`] because it
/// is `string(number)`, one of the 27.
#[must_use]
pub fn number_to_string(value: f64) -> String {
    format_number(value)
}

#[cfg(test)]
mod tests {
    use browser_dom::Document;

    use super::*;
    use crate::eval::{Variables, XPath};
    use crate::model::DomModel;

    const PAGE: &str = r#"<html><body>
        <div id="d1" xml:lang="en-GB"><p id="p1">Hello</p><p>  spaced  </p></div>
        <div id="d2"><span class="price">$1,234.50</span></div>
    </body></html>"#;

    fn evaluate(expr: &str) -> String {
        let document = Document::parse(PAGE);
        let model = DomModel::new(&document);
        let compiled = XPath::compile(expr).expect(expr);
        compiled
            .eval(&model, model.root(), &Variables::new())
            .expect(expr)
            .string_value(&model)
    }

    #[test]
    fn node_set_functions() {
        assert_eq!(evaluate("count(//p)"), "2");
        assert_eq!(evaluate("count(//nothing)"), "0");
        assert_eq!(evaluate("string(id('p1'))"), "Hello");
        assert_eq!(evaluate("count(id('p1 d2'))"), "2");
        assert_eq!(evaluate("count(id('nope'))"), "0");
        assert_eq!(evaluate("count(id(//p/@id))"), "1");
        assert_eq!(evaluate("local-name(//p)"), "p");
        assert_eq!(evaluate("name(//p)"), "p");
        assert_eq!(evaluate("local-name(//nothing)"), "");
        assert_eq!(evaluate("name(//p/@id)"), "id");
        assert_eq!(evaluate("count(//p[position() = last()])"), "1");
        assert_eq!(evaluate("//p[position() = 2]"), "  spaced  ");
    }

    #[test]
    fn string_functions() {
        assert_eq!(evaluate("string(//p)"), "Hello");
        assert_eq!(evaluate("concat('a', 1, true(), //p)"), "a1trueHello");
        assert_eq!(evaluate("starts-with('abc', 'ab')"), "true");
        assert_eq!(evaluate("starts-with('abc', 'b')"), "false");
        assert_eq!(evaluate("contains('abc', 'b')"), "true");
        assert_eq!(evaluate("substring-before('1999/04/01', '/')"), "1999");
        assert_eq!(evaluate("substring-after('1999/04/01', '/')"), "04/01");
        assert_eq!(evaluate("substring-before('abc', 'z')"), "");
        assert_eq!(evaluate("substring-after('abc', 'z')"), "");
        assert_eq!(evaluate("substring('12345', 1.5, 2.6)"), "234");
        assert_eq!(evaluate("substring('12345', 0, 3)"), "12");
        assert_eq!(evaluate("substring('12345', 2)"), "2345");
        assert_eq!(evaluate("string-length('abc')"), "3");
        assert_eq!(evaluate("string-length(//p)"), "5");
        assert_eq!(evaluate("normalize-space(//p[2])"), "spaced");
        assert_eq!(evaluate("translate('bar', 'abc', 'ABC')"), "BAr");
        // parser.md §10: the price predicate, end to end.
        assert_eq!(
            evaluate("number(translate(substring-after(//span[@class='price'], '$'), ',', ''))"),
            "1234.5"
        );
    }

    #[test]
    fn boolean_functions() {
        assert_eq!(evaluate("boolean(//p)"), "true");
        assert_eq!(evaluate("boolean(//nothing)"), "false");
        assert_eq!(evaluate("boolean('0')"), "true");
        assert_eq!(evaluate("boolean(0)"), "false");
        assert_eq!(evaluate("not(//nothing)"), "true");
        assert_eq!(evaluate("true()"), "true");
        assert_eq!(evaluate("false()"), "false");
        // `xml:lang` on an *HTML* element is un-namespaced here (and invisible to
        // libxml2's `xmlNodeGetLang` too), so `lang()` is false — the same answer
        // lxml gives for this page.
        assert_eq!(evaluate("count(//p[lang('en')])"), "0");
        assert_eq!(evaluate("count(//p[lang('en-GB')])"), "0");
        assert_eq!(evaluate("count(//span[lang('en')])"), "0");
    }

    /// Inside SVG/MathML html5ever runs the HTML5 "adjust foreign attributes"
    /// step, so `xml:lang` really is namespaced there and `lang()` works.
    #[test]
    fn lang_reads_a_namespaced_xml_lang() {
        let document = Document::parse(
            "<html><body><div xml:lang='en'><p>html</p></div>\
             <svg xml:lang='en-GB'><text>svg</text></svg></body></html>",
        );
        let model = DomModel::new(&document);
        let count = |expr: &str| {
            XPath::compile(expr)
                .expect(expr)
                .eval(&model, model.root(), &Variables::new())
                .expect(expr)
                .number(&model)
        };
        assert_eq!(count("count(//text[lang('en')])"), 1.0);
        assert_eq!(count("count(//text[lang('en-GB')])"), 1.0);
        assert_eq!(count("count(//text[lang('fr')])"), 0.0);
        assert_eq!(count("count(//svg[lang('en')])"), 1.0);
        // …but the HTML spelling is invisible, exactly as in lxml.
        assert_eq!(count("count(//p[lang('en')])"), 0.0);
        assert_eq!(count("count(//*[@xml:lang])"), 1.0, "only the SVG one");
        assert_eq!(
            count("count(//*[@*[local-name() = 'xml:lang']])"),
            1.0,
            "the HTML spelling is reachable through local-name()"
        );
    }

    #[test]
    fn number_functions() {
        assert_eq!(evaluate("number('12.5')"), "12.5");
        assert_eq!(evaluate("number('abc')"), "NaN");
        assert_eq!(evaluate("string(sum(//nothing))"), "0");
        assert_eq!(evaluate("string(sum(//p))"), "NaN");
        assert_eq!(evaluate("floor(-0.5)"), "-1");
        assert_eq!(evaluate("ceiling(-0.5)"), "0");
        assert_eq!(evaluate("round(-0.5)"), "0");
        assert_eq!(evaluate("round(0.5)"), "1");
        assert_eq!(evaluate("round(1.5)"), "2");
        assert_eq!(evaluate("round(-1.5)"), "-1");
        assert_eq!(evaluate("number()"), "NaN");
    }

    #[test]
    fn sum_adds_string_values() {
        let document = Document::parse(
            "<ul><li data-n='1'>1</li><li data-n='2'>2</li><li data-n='3'>3</li></ul>",
        );
        let model = DomModel::new(&document);
        let compiled = XPath::compile("sum(//li/@data-n)").expect("compiles");
        assert_eq!(
            compiled
                .eval(&model, model.root(), &Variables::new())
                .expect("evaluates")
                .number(&model),
            6.0
        );
    }

    #[test]
    fn translate_removes_when_to_is_shorter() {
        assert_eq!(translate("--aaa--", "abc-", "ABC"), "AAA");
        assert_eq!(translate("1,234,567", ",", ""), "1234567");
        assert_eq!(translate("aaa", "aa", "bc"), "bbb", "first occurrence wins");
        assert_eq!(translate("😀x", "x", "y"), "😀y");
    }

    #[test]
    fn string_functions_count_characters_not_bytes() {
        assert_eq!(evaluate("string-length('😀')"), "1");
        assert_eq!(evaluate("substring('😀abc', 2, 1)"), "a");
        assert_eq!(evaluate("substring('héllo', 2, 2)"), "él");
    }

    #[test]
    fn substring_edge_cases() {
        assert_eq!(substring("12345", -42.0, Some(f64::INFINITY)), "12345");
        assert_eq!(
            substring("12345", f64::NEG_INFINITY, Some(f64::INFINITY)),
            ""
        );
        assert_eq!(substring("12345", f64::NAN, Some(3.0)), "");
        assert_eq!(substring("12345", 1.0, Some(f64::NAN)), "");
        assert_eq!(substring("12345", 6.0, None), "");
        assert_eq!(number_to_string(1.0), "1");
    }
}
