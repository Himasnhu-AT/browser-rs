//! Conformance corpus: 600+ expressions run through **lxml / libxml2** over
//! `testdata/pages/*.html` and pinned in `testdata/xpath_golden.tsv`.
//!
//! This is unit 6's acceptance gate (`ARCHITECTURE.md` §7): every expression the
//! eight subsystem specs issue, every axis, every node test, every core function,
//! the four value types with their conversions, and the expressions the reference
//! implementation *rejects*.
//!
//! Regenerate with the pinned interpreter:
//!
//! ```text
//! cd crates/browser-xpath/testdata
//! /path/to/pyref/bin/python gen_xpath_golden.py > xpath_golden.tsv
//! ```
//!
//! # How results are compared
//!
//! `libxml2` and html5ever do not build *identical* trees — that is a parser
//! difference, documented in `browser-dom`'s `API.md`, and this file must not
//! mistake it for an engine difference. So:
//!
//! * node-sets are compared as **signatures** (`e:/html/body/div[1]`,
//!   `a:…/@href=/two`, `t:text`), using libxml2's own `xmlGetNodePath` numbering,
//!   which counts *elements* and is therefore insensitive to whitespace text;
//! * whitespace-only text nodes are dropped from both sides, because
//!   `remove_blank_text` keeps a few of them in libxml2 that html5ever drops;
//! * every page in the corpus has a real `<head>`, so the "html5ever always
//!   synthesises one" difference cannot show up.
//!
//! The context node is the **root element**, not the document node: that is what
//! `ElementTree.xpath()` uses, and it is what `Selector.xpath()` does too
//! (`self._root` is the `<html>` element). `/` still selects the document node in
//! both engines.
//!
//! Anything else is an engine difference and fails the test.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use browser_dom::{Document, NodeRef};
use browser_xpath::error::XPathError;
use browser_xpath::eval::{Variables, XPath};
use browser_xpath::model::{DomModel, NodeKind, TreeModel, XNode};
use browser_xpath::value::{format_number, Value};

const CORPUS: &str = include_str!("../testdata/xpath_golden.tsv");

static PAGES: LazyLock<BTreeMap<&'static str, Document>> = LazyLock::new(|| {
    // `Document::parse` defaults are exactly the flags `reference.parser.Selector`
    // passes to lxml: remove_comments = True, remove_blank_text = True.
    BTreeMap::from([
        (
            "products.html",
            Document::parse(include_str!("../testdata/pages/products.html")),
        ),
        (
            "structure.html",
            Document::parse(include_str!("../testdata/pages/structure.html")),
        ),
        (
            "links.html",
            Document::parse(include_str!("../testdata/pages/links.html")),
        ),
    ])
});

/// One record of the corpus.
struct Case {
    page: &'static str,
    kind: &'static str,
    expr: String,
    value: String,
}

fn cases() -> Vec<Case> {
    CORPUS
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.splitn(4, '\t');
            let page = fields.next().expect("page");
            let kind = fields.next().expect("kind");
            let expr = unescape(fields.next().expect("expression"));
            let value = unescape(fields.next().unwrap_or(""));
            Case {
                page,
                kind,
                expr,
                value,
            }
        })
        .collect()
}

/// The generator's escaping, reversed. `\u` is the node-set member separator.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('u') => out.push('\u{1f}'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Signatures — libxml2's `xmlGetNodePath`, reimplemented over our tree.
// ---------------------------------------------------------------------------

/// The path libxml2 would print for an element: `/html/body/div[2]`, with the
/// index present only when a same-named sibling exists.
fn element_path(element: NodeRef<'_>) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut current = Some(element);
    while let Some(node) = current {
        let Some(name) = node.local_name() else { break };
        let mut occur = node
            .prev_siblings()
            .filter(|sibling| sibling.local_name() == Some(name))
            .count();
        if occur == 0 {
            if node
                .next_siblings()
                .any(|sibling| sibling.local_name() == Some(name))
            {
                occur = 1;
            }
        } else {
            occur += 1;
        }
        segments.push(if occur == 0 {
            format!("/{name}")
        } else {
            format!("/{name}[{occur}]")
        });
        current = node.parent();
    }
    segments.reverse();
    segments.concat()
}

/// The signature the corpus records for one node.
fn signature(model: &DomModel<'_>, node: XNode<'_>) -> String {
    match model.kind(node) {
        NodeKind::Element => format!("e:{}", element_path(node.anchor())),
        NodeKind::Attr => {
            let (name, value) = node.as_attribute().unwrap_or(("", ""));
            format!("a:{}/@{name}={value}", element_path(node.anchor()))
        }
        NodeKind::Text => format!("t:{}", node.string_value()),
        NodeKind::Comment => format!("c:{}", node.string_value()),
        NodeKind::Pi => format!("p:{}", model.name(node).map_or("", |(_, local)| local)),
        NodeKind::Namespace => format!(
            "n:{}={}",
            model.name(node).map_or("", |(_, local)| local),
            node.string_value()
        ),
        NodeKind::Root => "r:".to_owned(),
    }
}

/// Blank text nodes are a parser difference, not an engine one.
fn is_blank_text(signature: &str) -> bool {
    signature
        .strip_prefix("t:")
        .is_some_and(|text| text.trim().is_empty())
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[test]
fn matches_libxml2_on_the_whole_corpus() {
    let cases = cases();
    assert!(
        cases.len() > 400,
        "the corpus should have 400+ cases, found {}",
        cases.len()
    );

    let models: BTreeMap<&str, DomModel<'_>> = PAGES
        .iter()
        .map(|(name, document)| (*name, DomModel::new(document)))
        .collect();

    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for case in &cases {
        let model = models.get(case.page).expect("a known page");
        let context = model.root_element().expect("<html>");
        let variables = Variables::new();

        let compiled = match XPath::compile(&case.expr) {
            Ok(compiled) => compiled,
            Err(error) => {
                if case.kind != "error" {
                    failures.push(format!(
                        "{} `{}`: compile failed: {error}",
                        case.page, case.expr
                    ));
                }
                checked += 1;
                continue;
            }
        };
        let evaluated = compiled.eval(model, context, &variables);
        checked += 1;

        match (case.kind, evaluated) {
            ("error", Ok(value)) => failures.push(format!(
                "{} `{}`: expected a failure, got {}",
                case.page,
                case.expr,
                value.type_name()
            )),
            ("error", Err(_)) => {}
            (_, Err(error)) => failures.push(format!(
                "{} `{}`: evaluation failed: {error}",
                case.page, case.expr
            )),
            ("nodes", Ok(value)) => {
                let Some(nodes) = value.as_node_set() else {
                    failures.push(format!(
                        "{} `{}`: expected a node-set, got {}",
                        case.page,
                        case.expr,
                        value.type_name()
                    ));
                    continue;
                };
                let ours: Vec<String> = nodes
                    .iter()
                    .map(|node| signature(model, *node))
                    .filter(|signature| !is_blank_text(signature))
                    .collect();
                let theirs: Vec<String> = if case.value.is_empty() {
                    Vec::new()
                } else {
                    case.value
                        .split('\u{1f}')
                        .filter(|signature| !is_blank_text(signature))
                        .map(str::to_owned)
                        .collect()
                };
                if ours != theirs {
                    failures.push(format!(
                        "{} `{}`:\n     ours: {ours:?}\n  libxml2: {theirs:?}",
                        case.page, case.expr
                    ));
                }
            }
            ("number", Ok(value)) => {
                let ours = format_number(value.number(model));
                if ours != case.value {
                    failures.push(format!(
                        "{} `{}`: ours {ours:?} != libxml2 {:?}",
                        case.page, case.expr, case.value
                    ));
                }
            }
            ("string", Ok(value)) => {
                let ours = value.string_value(model);
                if ours != case.value {
                    failures.push(format!(
                        "{} `{}`: ours {ours:?} != libxml2 {:?}",
                        case.page, case.expr, case.value
                    ));
                }
            }
            ("boolean", Ok(value)) => {
                let ours = value.boolean();
                let theirs = case.value == "true";
                if !matches!(value, Value::Bool(_)) {
                    failures.push(format!(
                        "{} `{}`: expected a boolean, got {}",
                        case.page,
                        case.expr,
                        value.type_name()
                    ));
                } else if ours != theirs {
                    failures.push(format!(
                        "{} `{}`: ours {ours} != libxml2 {theirs}",
                        case.page, case.expr
                    ));
                }
            }
            (kind, Ok(_)) => failures.push(format!("unknown corpus kind `{kind}`")),
        }
    }

    assert_eq!(checked, cases.len());
    assert!(
        failures.is_empty(),
        "{} of {} corpus cases differ from libxml2:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// The corpus is only meaningful if it covers what the specs actually ask for.
#[test]
fn the_corpus_covers_the_spec_surface() {
    let cases = cases();
    let text: String = cases
        .iter()
        .map(|case| format!("{} {}\n", case.kind, case.expr))
        .collect();

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
        assert!(text.contains(axis), "no corpus case uses `{axis}`");
    }
    for function in [
        "last(",
        "position(",
        "count(",
        "id(",
        "local-name(",
        "namespace-uri",
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
        assert!(text.contains(function), "no corpus case uses `{function}`");
    }
    for kind in ["nodes", "string", "number", "boolean", "error"] {
        assert!(
            cases.iter().any(|case| case.kind == kind),
            "the corpus has no `{kind}` case"
        );
    }
}

/// The expressions the specs pin by name, checked against the numbers those
/// specs state — independently of the generated corpus.
#[test]
fn the_expressions_the_specs_pin() {
    let document = PAGES.get("products.html").expect("the products page");
    let model = DomModel::new(document);
    let variables = Variables::new();

    let count = |expr: &str| -> f64 {
        XPath::compile(expr)
            .expect(expr)
            .eval(&model, model.root_element().expect("<html>"), &variables)
            .expect(expr)
            .number(&model)
    };

    // parser.md §10 — test_general.py's two XPath expectations.
    assert_eq!(
        count(
            r#"count(//section[@id="reviews"]//div[contains(@class, "review") and @data-rating >= 4])"#
        ),
        2.0
    );
    assert_eq!(
        count(
            r#"count(//article[contains(@class,"product")][number(translate(substring-after(.//span[@class="price"], "$"), ",", "")) > 15])"#
        ),
        2.0
    );

    // parser.md §4.2 — the adaptive relocation probe: `"//" + "/".join(path_parts)`
    // plus `[count(ancestor::*) = <depth>]`. `div.product-list` sits under
    // html/body/main/section, i.e. at depth 4.
    assert_eq!(
        count("count(//main/section/div[count(ancestor::*) = 4])"),
        2.0
    );
    assert_eq!(
        count("count(//main/section/div[count(ancestor::*) = 3])"),
        0.0
    );
    assert_eq!(
        count("count(//main/section/div/article[count(ancestor::*) = 5])"),
        3.0
    );

    // parser.md §11.5 — the three pre-compiled expressions. The context node is
    // `<html>`, so `.//*` is every element *below* it: one fewer than `//*`.
    assert_eq!(count("count(.//*)") + 1.0, count("count(//*)"));
    assert!(count("count(.//text())") > 0.0);
    assert!(count("count(.//*[normalize-space(text())])") > 0.0);

    // spider-support.md §2.4 — the link extractor's search selector, verbatim,
    // including the leading space lxml tolerates.
    let document = PAGES.get("links.html").expect("the links page");
    let model = DomModel::new(document);
    let hrefs = XPath::compile(".//a/@href| .//area/@href")
        .expect("compiles")
        .select(&model, model.root_element().expect("<html>"), &variables)
        .expect("evaluates");
    let values: Vec<String> = hrefs
        .iter()
        .map(|node| node.string_value().into_owned())
        .collect();
    assert_eq!(
        values,
        [
            "/one",
            "/two",
            "https://example.com/three",
            "",
            "/inline",
            "/area-one",
            "/area-two",
            "/foot-one",
            "/foot-two",
        ]
    );

    // cli-ai.md §4.4 / toolbelt.md — `_HIDDEN_XPATH`, on a page built for it.
    let hidden_page = Document::parse(
        r#"<html><body>
            <div style="display:none">a</div>
            <div style="display: none">b</div>
            <div style="visibility:hidden">c</div>
            <div style="opacity: 0">d</div>
            <div aria-hidden='true'>e</div>
            <template>f</template>
            <div style="display : none">visible-to-this-selector</div>
        </body></html>"#,
    );
    let hidden_model = DomModel::new(&hidden_page);
    let hidden = XPath::compile(concat!(
        r#".//*[contains(@style,"display:none") or contains(@style,"display: none")"#,
        r#" or contains(@style,"visibility:hidden") or contains(@style,"visibility: hidden")"#,
        r#" or contains(@style,"opacity:0") or contains(@style,"opacity: 0")"#,
        r#" or contains(@style,"font-size:0") or contains(@style,"font-size: 0")"#,
        r#" or contains(@style,"height:0") or contains(@style,"height: 0")"#,
        r#" or contains(@style,"width:0") or contains(@style,"width: 0")]"#,
        " | .//*[@aria-hidden='true']",
        " | .//template",
    ))
    .expect("compiles")
    .select(&hidden_model, hidden_model.root(), &variables)
    .expect("evaluates");
    let text: Vec<String> = hidden
        .iter()
        .map(|node| node.string_value().into_owned())
        .collect();
    assert_eq!(text, ["a", "b", "c", "d", "e", "f"]);

    // parser.md §3.5 — the pinned variable example.
    let table = Document::parse("<table><tr><td>Cell 1</td><td>Cell 2</td></tr></table>");
    let table_model = DomModel::new(&table);
    let mut variables = Variables::new();
    variables.insert("cell_text", "Cell 1");
    let found = XPath::compile("//td[text()=$cell_text]")
        .expect("compiles")
        .select(&table_model, table_model.root(), &variables)
        .expect("evaluates");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].string_value(), "Cell 1");
}

/// `page.xpath("4 ayo")` must raise — parser.md §6.4 pins it.
#[test]
fn invalid_expressions_never_return_an_empty_node_set() {
    let document = PAGES.get("products.html").expect("the products page");
    for expr in [
        "4 ayo",
        "//p[",
        "ends-with(@a, 'b')",
        "for $x in //a return $x",
        "nosuchaxis::x",
        "concat('a')",
        "//p/(a|b)",
    ] {
        let compiled = XPath::compile(expr);
        assert!(compiled.is_err(), "`{expr}` compiled");
        let error = compiled.expect_err(expr);
        assert!(error.is_compile_time(), "`{expr}`: {error}");
    }

    // …and the run-time failures are reported too, never swallowed.
    let model = DomModel::new(document);
    let error = XPath::compile("//article[.=$missing]")
        .expect("compiles")
        .eval(&model, model.root(), &Variables::new())
        .expect_err("unbound variable");
    assert!(matches!(error, XPathError::UndefinedVariable { .. }));
}

/// The places where this engine and `lxml` legitimately disagree, pinned so
/// that a regression is visible and a reader knows the list is exhaustive.
///
/// Every one of them is a **tree** difference (html5ever vs libxml2's HTML
/// parser) or a data-model consequence of one — never an evaluation difference.
/// `browser-dom`'s `API.md` documents the underlying causes as deviation 9
/// (`xml:lang`) and the `default_doctype` row (`<head>`). Rows 1–3 used to be
/// blank-text divergences and are now *agreements*, kept here with their old
/// values in the comments so the fix cannot silently regress.
#[test]
fn the_known_divergences_from_lxml() {
    let structure = PAGES.get("structure.html").expect("the structure page");
    let model = DomModel::new(structure);
    let variables = Variables::new();
    let string = |expr: &str| -> String {
        XPath::compile(expr)
            .expect(expr)
            .eval(&model, model.root_element().expect("<html>"), &variables)
            .expect(expr)
            .string_value(&model)
    };

    // 1. Blank text nodes agree exactly: `remove_blank_text` implements
    //    libxml2's `areBlanks()`, so `count(//text())` and `count(//node())`
    //    are lxml's own answers. These two used to be 23 and 73.
    assert_eq!(string("count(//text())"), "47");
    assert_eq!(string("count(//node())"), "97");

    // 2. …and the string-value of a mixed-content element keeps its separating
    //    whitespace, which is what makes Markdown of ordinary prose come out
    //    right.
    assert_eq!(
        string("normalize-space(//div[@id='d1'])"),
        "alpha bold beta"
    );

    // 3. cssselect's `:empty` translation therefore no longer matches
    //    `<p>   </p>`, agreeing with lxml at 8.
    assert_eq!(string("count(//*[not(*) and not(string-length())])"), "8");

    // 4. html5ever turns `xmlns` into a real namespace declaration; libxml2's HTML
    //    parser ignores it, so this document has four namespace nodes libxml2 has
    //    no idea about. lxml: 50.
    //
    //    `namespace-uri()` used to diverge here too — html5ever gives every HTML
    //    element the XHTML namespace — until `DomModel` started reporting that
    //    namespace as the null one, which is what the lxml corpus says
    //    (`tests/conformance_lxml.rs`, `entities.html`).
    assert_eq!(string("namespace-uri(//div)"), "");
    assert_eq!(string("count(//*/namespace::*)"), "54");

    // 5. html5ever always synthesises `<head>`; libxml2 only builds one when the
    //    source has head content. lxml: 4.
    let fragment = Document::parse("<div><p>x</p></div>");
    let fragment_model = DomModel::new(&fragment);
    assert_eq!(
        XPath::compile("count(//*)")
            .expect("compiles")
            .eval(
                &fragment_model,
                fragment_model.root_element().expect("<html>"),
                &variables
            )
            .expect("evaluates")
            .string_value(&fragment_model),
        "5"
    );

    // A valueless attribute is `""` here and `"checked"` in libxml2 — same
    // family, and the reason the corpus page spells `checked="checked"`.
    let valueless = Document::parse("<input type='checkbox' checked>");
    let valueless_model = DomModel::new(&valueless);
    assert_eq!(
        XPath::compile("string(//input/@checked)")
            .expect("compiles")
            .eval(
                &valueless_model,
                valueless_model.root_element().expect("<html>"),
                &variables
            )
            .expect("evaluates")
            .string_value(&valueless_model),
        ""
    );
}
