//! The lxml conformance gate: 2 000 `(document, expression)` cases whose answers
//! were produced by **lxml 6.1.2 / libxml2 2.14.6**, the engine this crate
//! replaces, and pinned in `testdata/xpath_conformance.jsonl`.
//!
//! Regenerate with the pinned reference interpreter:
//!
//! ```text
//! /Users/himanshu/.claude/jobs/a62e4b8e/tmp/pyref/bin/python \
//!     /Users/himanshu/.claude/jobs/a62e4b8e/tmp/xconf/gen.py \
//!     crates/browser-xpath/testdata
//! ```
//!
//! The generator is deliberately kept outside the repository — the corpus, not the
//! script, is the artefact — but it is deterministic, and every expression it runs
//! is reproduced verbatim in the `expr` field of the row it produced, so the whole
//! table can be read straight out of `xpath_conformance.jsonl`.
//!
//! # What a row means
//!
//! Every row is `doc`, `expr` and the result lxml returned when the expression was
//! evaluated **with the `<html>` element as the context node** — exactly what
//! `reference.parser.Selector.xpath()` does (`self._root.xpath(...)`). Optional
//! `ctx` moves the context node, optional `vars` binds `$variables`.
//!
//! * `"type": "nodeset"` — one member per node: `e` element (outer HTML),
//!   `a` attribute (`n` = name, `v` = value), `t` text, `c` comment,
//!   `p` processing instruction, `n` namespace (`prefix\turi`).
//! * `"type": "number"` — Python's `repr(float)`, which round-trips exactly, or
//!   `nan` / `inf` / `-inf`. Compared bit for bit, so `-0` must stay `-0`.
//! * `"type": "string"` / `"boolean"` — verbatim.
//! * `"type": "error"` — libxml2 **rejects** the expression; so must we.
//! * `"divergence"` — the two engines genuinely disagree; `value` is still the
//!   answer this engine must give, `lxml` records what lxml said, and the string
//!   explains why. Nothing is ever dropped from the corpus to raise the score.
//!
//! # Why element markup is comparable at all
//!
//! Node-set elements are compared as outer HTML, and the generator serialises the
//! lxml tree with **`browser-dom`'s** serialiser rules rather than libxml2's
//! (`etree.tostring(method="html")` single-quotes attribute values containing `"`,
//! escapes `<`/`>` inside attribute values, percent-encodes URI attributes and
//! drops the value of HTML boolean attributes). Those are writer conventions, not
//! tree or XPath differences. Everything else — element order, tag names,
//! attribute order and values, text — is compared verbatim.
//!
//! The pages are minified on purpose: inter-tag whitespace is the one place
//! `remove_blank_text` and html5ever disagree. `indented.html` is the exception and
//! is parsed with `remove_blank_text = false` on both sides, where they agree
//! exactly; `divergent.html` collects every construct where the two parsers build
//! different trees and every one of its rows is a recorded divergence.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use browser_dom::{Document, NodeData, ParseOptions};
use browser_xpath::eval::{Variable, Variables, XPath};
use browser_xpath::model::{DomModel, NodeKind, TreeModel, XNode};
use browser_xpath::value::Value;
use serde_json::{json, Value as Json};

const CORPUS: &str = include_str!("../testdata/xpath_conformance.jsonl");

/// Every page, by the id the corpus uses, with the parse options that id implies.
fn page(doc_id: &str) -> Document {
    let keep_comments = matches!(doc_id, "remarks.html" | "divergent.html");
    let blanks = doc_id.ends_with("#blanks");
    let options = ParseOptions::new()
        .keep_comments(keep_comments)
        .remove_blank_text(!blanks);
    let source = match doc_id.split(['#', '!']).next().unwrap_or(doc_id) {
        "tables.html" => include_str!("../testdata/pages/tables.html"),
        "nesting.html" => include_str!("../testdata/pages/nesting.html"),
        "entities.html" => include_str!("../testdata/pages/entities.html"),
        "mixed.html" => include_str!("../testdata/pages/mixed.html"),
        "remarks.html" => include_str!("../testdata/pages/remarks.html"),
        "broken.html" => include_str!("../testdata/pages/broken.html"),
        "forms.html" => include_str!("../testdata/pages/forms.html"),
        "roster.html" => include_str!("../testdata/pages/roster.html"),
        "rawtext.html" => include_str!("../testdata/pages/rawtext.html"),
        "journal.html" => include_str!("../testdata/pages/journal.html"),
        "indented.html" => include_str!("../testdata/pages/indented.html"),
        "oddnames.html" => include_str!("../testdata/pages/oddnames.html"),
        "divergent.html" => include_str!("../testdata/pages/divergent.html"),
        other => panic!("corpus names an unknown page: {other}"),
    };
    Document::parse_with(source, &options)
}

/// One corpus row.
struct Case {
    line: usize,
    doc: String,
    expr: String,
    ctx: Option<String>,
    vars: Option<Json>,
    expected: Json,
    divergence: Option<String>,
}

fn corpus() -> Vec<Case> {
    CORPUS
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let row: Json = serde_json::from_str(line).expect("corpus line is JSON");
            Case {
                line: index + 1,
                doc: row["doc"].as_str().expect("doc").to_owned(),
                expr: row["expr"].as_str().expect("expr").to_owned(),
                ctx: row.get("ctx").and_then(Json::as_str).map(str::to_owned),
                vars: row.get("vars").cloned(),
                expected: json!({ "type": row["type"], "value": row["value"] }),
                divergence: row
                    .get("divergence")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Our result, in the corpus's encoding.
// ---------------------------------------------------------------------------

/// A node-set member, encoded exactly as the generator encodes lxml's.
fn member(model: &DomModel<'_>, node: XNode<'_>) -> Json {
    match node.kind() {
        NodeKind::Element => {
            let element = node.as_element().expect("element");
            json!({ "k": "e", "v": element.outer_html() })
        }
        NodeKind::Attr => {
            let (name, value) = node.as_attribute().expect("attribute");
            json!({ "k": "a", "n": name, "v": value })
        }
        NodeKind::Text => json!({ "k": "t", "v": node.string_value() }),
        NodeKind::Namespace => {
            let prefix = TreeModel::name(model, node).map_or("", |(_, local)| local);
            json!({ "k": "n", "v": format!("{prefix}\t{}", node.string_value()) })
        }
        NodeKind::Root => json!({ "k": "r", "v": "" }),
        NodeKind::Comment | NodeKind::Pi => {
            let tree = node.as_tree_node().expect("tree node");
            match tree.value() {
                NodeData::Comment(text) => json!({ "k": "c", "v": text }),
                NodeData::ProcessingInstruction { target, data } => {
                    json!({ "k": "p", "v": format!("{target} {data}") })
                }
                _ => json!({ "k": "?", "v": node.string_value() }),
            }
        }
    }
}

/// The corpus's number encoding: a round-tripping decimal, or `nan`/`inf`/`-inf`.
/// JSON has no way to spell the specials, so numbers travel as strings on both
/// sides and are parsed back to `f64` before they are compared.
fn number_text(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    format!("{value:?}")
}

/// `repr(float)` in Python, `nan`/`inf`/`-inf` for the specials.
fn expected_number(text: &str) -> f64 {
    match text {
        "nan" => f64::NAN,
        "inf" => f64::INFINITY,
        "-inf" => f64::NEG_INFINITY,
        other => other.parse().expect("corpus number"),
    }
}

/// Bit-for-bit, with NaN equal to NaN — so `-0` must stay `-0`.
fn same_number(ours: f64, theirs: f64) -> bool {
    if ours.is_nan() || theirs.is_nan() {
        return ours.is_nan() && theirs.is_nan();
    }
    ours.to_bits() == theirs.to_bits()
}

fn variables(vars: Option<&Json>) -> Variables {
    let mut bound = Variables::new();
    let Some(Json::Object(map)) = vars else {
        return bound;
    };
    for (name, value) in map {
        let variable = match value {
            Json::String(text) => Variable::Str(text.clone()),
            Json::Bool(flag) => Variable::Bool(*flag),
            Json::Number(number) => Variable::Number(number.as_f64().expect("finite")),
            other => panic!("unbindable variable {name}: {other}"),
        };
        bound.insert(name.clone(), variable);
    }
    bound
}

/// Evaluate one case and describe the answer the way the corpus does.
fn actual(model: &DomModel<'_>, case: &Case) -> Json {
    let context = match &case.ctx {
        None => model.root_element().expect("root element"),
        Some(path) => {
            let anchor = XPath::compile(path)
                .and_then(|compiled| {
                    compiled.select(
                        model,
                        model.root_element().expect("root element"),
                        &Variables::new(),
                    )
                })
                .unwrap_or_else(|error| panic!("context {path} failed: {error}"));
            assert_eq!(anchor.len(), 1, "context {path} selected {}", anchor.len());
            anchor[0]
        }
    };
    let bound = variables(case.vars.as_ref());
    let compiled = match XPath::compile(&case.expr) {
        Ok(compiled) => compiled,
        Err(error) => return json!({ "type": "error", "value": error.to_string() }),
    };
    match compiled.eval(model, context, &bound) {
        Err(error) => json!({ "type": "error", "value": error.to_string() }),
        Ok(Value::NodeSet(nodes)) => {
            let members: Vec<Json> = nodes.iter().map(|node| member(model, *node)).collect();
            json!({ "type": "nodeset", "value": members })
        }
        Ok(Value::Str(text)) => json!({ "type": "string", "value": text }),
        Ok(Value::Bool(flag)) => json!({ "type": "boolean", "value": flag }),
        Ok(Value::Number(number)) => json!({ "type": "number", "value": number_text(number) }),
    }
}

/// Does our answer match the recorded one? Errors match on *kind*, not message:
/// lxml only ever says `XPathEvalError`.
fn matches(ours: &Json, expected: &Json) -> bool {
    let (Some(our_kind), Some(their_kind)) = (ours["type"].as_str(), expected["type"].as_str())
    else {
        return false;
    };
    if our_kind != their_kind {
        return false;
    }
    match our_kind {
        "error" => true,
        "number" => same_number(
            expected_number(ours["value"].as_str().expect("our number text")),
            expected_number(expected["value"].as_str().expect("corpus number text")),
        ),
        _ => ours["value"] == expected["value"],
    }
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[test]
fn every_case_agrees_with_lxml() {
    let cases = corpus();
    let mut by_doc: BTreeMap<&str, Vec<&Case>> = BTreeMap::new();
    for case in &cases {
        by_doc.entry(case.doc.as_str()).or_default().push(case);
    }

    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for (doc_id, doc_cases) in &by_doc {
        let document = page(doc_id);
        let model = DomModel::new(&document);
        for case in doc_cases {
            checked += 1;
            let ours = actual(&model, case);
            if matches(&ours, &case.expected) {
                continue;
            }
            let mut report = String::new();
            let _ = write!(
                report,
                "{}:{} [{}] {}\n     expected {}\n     got      {}",
                case.doc,
                case.line,
                case.expected["type"].as_str().unwrap_or("?"),
                case.expr,
                serde_json::to_string(&case.expected["value"]).unwrap_or_default(),
                serde_json::to_string(&ours["value"]).unwrap_or_default(),
            );
            if let Some(reason) = &case.divergence {
                let _ = write!(report, "\n     (recorded divergence: {reason})");
            }
            failures.push(report);
        }
    }

    let passed = checked - failures.len();
    println!(
        "lxml conformance: {passed}/{checked} ({:.2}%) over {} documents",
        100.0 * passed as f64 / checked as f64,
        by_doc.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {checked} cases disagree with lxml:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The corpus is only a gate if it is broad; these are the properties the
/// generator asserts too, restated here so a trimmed corpus cannot pass.
#[test]
fn the_corpus_covers_the_language() {
    let cases = corpus();
    assert!(
        cases.len() >= 400,
        "the corpus must hold at least 400 cases, found {}",
        cases.len()
    );

    let documents: BTreeMap<&str, ()> = cases.iter().map(|c| (c.doc.as_str(), ())).collect();
    assert!(
        documents.len() >= 8,
        "the corpus must span at least 8 documents, found {}",
        documents.len()
    );

    let expressions: String = cases
        .iter()
        .map(|case| case.expr.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    for axis in [
        "ancestor::",
        "ancestor-or-self::",
        "attribute::",
        "child::",
        "descendant::",
        "descendant-or-self::",
        "following::",
        "following-sibling::",
        "namespace::",
        "parent::",
        "preceding::",
        "preceding-sibling::",
        "self::",
    ] {
        assert!(expressions.contains(axis), "no case exercises {axis}");
    }
    for function in [
        "last(",
        "position(",
        "count(",
        "id(",
        "local-name(",
        "namespace-uri(",
        "name(",
        "string(",
        "concat(",
        "starts-with(",
        "contains(",
        "substring-before(",
        "substring-after(",
        "substring(",
        "string-length(",
        "normalize-space(",
        "translate(",
        "boolean(",
        "not(",
        "true(",
        "false(",
        "lang(",
        "number(",
        "sum(",
        "floor(",
        "ceiling(",
        "round(",
    ] {
        assert!(
            expressions.contains(function),
            "no case exercises {function}"
        );
    }

    let kinds: BTreeMap<&str, usize> = cases.iter().fold(BTreeMap::new(), |mut seen, case| {
        *seen
            .entry(case.expected["type"].as_str().unwrap_or("?"))
            .or_default() += 1;
        seen
    });
    for kind in ["nodeset", "string", "number", "boolean", "error"] {
        assert!(
            kinds.get(kind).copied().unwrap_or_default() > 0,
            "no {kind} results in the corpus"
        );
    }
    assert!(
        kinds["nodeset"] >= 100,
        "too few node-set cases: {}",
        kinds["nodeset"]
    );
    assert!(
        kinds["error"] >= 20,
        "too few rejection cases: {}",
        kinds["error"]
    );
}

/// Every divergence is deliberate, explained, and confined to the two places the
/// parsers — not the engines — disagree.
#[test]
fn divergences_are_declared_and_bounded() {
    let cases = corpus();
    let divergent: Vec<&Case> = cases.iter().filter(|c| c.divergence.is_some()).collect();
    for case in &divergent {
        let reason = case.divergence.as_deref().unwrap_or_default();
        assert!(
            reason.len() > 40,
            "divergence on {} needs a real explanation",
            case.expr
        );
        assert!(
            case.doc == "divergent.html"
                || matches!(
                    case.expr.as_str(),
                    "last()"
                        | "position()"
                        | "position() = last()"
                        | "string(5e-324)"
                        | "number('5e-324')"
                        | "string(number('5e-324'))"
                        | "(1 + 2)[1]"
                        | "((1+2))[1]"
                        | "(count(//li))[1]"
                        | "(true())[1]"
                ),
            "unexpected divergence outside divergent.html: {} on {}",
            case.expr,
            case.doc
        );
    }
    assert!(
        divergent.len() * 20 < cases.len(),
        "divergences must stay a rounding error: {} of {}",
        divergent.len(),
        cases.len()
    );
}
