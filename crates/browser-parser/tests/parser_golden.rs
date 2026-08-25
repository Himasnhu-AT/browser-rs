//! Replay of `testdata/parser_api_golden.jsonl` — unit 10's *public API*
//! differential corpus, recorded from `the reference implementation` itself.
//!
//! The generator lives in the repository at `tools/corpus/u10/gen_api_corpus.py`
//! and is re-runnable with no editing:
//!
//! ```sh
//! /path/to/pyref/bin/python tools/corpus/u10/gen_api_corpus.py
//! ```
//!
//! Where `src/document.rs`'s in-crate `golden` module pins the *internal*
//! algorithms (fingerprints, similarity scores, the four generated selectors),
//! this file drives the surface a user actually calls: `css` / `xpath` with and
//! without pseudo-elements, `.get()` / `.getall()` / defaults / empty results,
//! the whole text-node degradation table, `find_all` / `find` in every argument
//! form (including the CSS-value escaping edge cases), `find_similar`,
//! `find_by_text` / `find_by_regex` with the no-match asymmetry, the navigation
//! properties, the `Nodes` (Python `Selectors`) methods, and the **end-to-end
//! adaptive path** — `css(sel, auto_save=True)` on a page, then
//! `css(sel, adaptive=True)` on a mutated copy of it, through a real SQLite
//! storage, compared against Python's entire per-element score table.
//!
//! ## The rule this file enforces
//!
//! A row the port cannot reproduce is **never** deleted from the corpus. It is
//! listed in [`EXPECTED_DIVERGENCES`] with a written reason, and the test fails
//! if it ever starts *passing*, so the table can never decay into a skip-list.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[cfg(feature = "adaptive")]
use browser_core::fingerprint::{similarity_score, ElementFingerprint};
use browser_core::text::{ReOptions, RegexSpec};
use browser_dom::NodeRef;
use serde_json::Value;

use browser_parser::config::{AllTextOpts, ByRegexOpts, ByTextOpts, SimilarOpts};
use browser_parser::document::Document;
use browser_parser::find::FindQuery;
use browser_parser::node::Node;
use browser_parser::nodes::Nodes;

// --------------------------------------------------------------------------- //
// Corpus plumbing
// --------------------------------------------------------------------------- //

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

fn page_html(name: &str) -> String {
    let path = testdata().join("pages").join(format!("{name}.html"));
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn corpus_rows() -> Vec<Value> {
    let path = testdata().join("parser_api_golden.jsonl");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("corpus line is JSON"))
        .collect()
}

/// The corpus' element identity: `tag | sorted attributes | normalised subtree text`.
///
/// Raw outer HTML cannot be an identity here — the two serialisers legitimately
/// differ on quoting and minimised attributes, and html5ever and libxml2 build
/// different trees for the malformed pages. Serialisation *is* compared, but in
/// its own fields (`get`, `getall`, `html_content`) so a shape difference shows
/// up where it belongs instead of poisoning every key.
fn element_key(node: NodeRef<'_>) -> String {
    let mut attributes: Vec<(&str, &str)> = node.attrs().iter().collect();
    attributes.sort_unstable();
    let attributes = attributes
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    let text = node
        .text_content()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!("{}|{}|{}", node.tag(), attributes, text)
}

/// The identity of one `css()` / `xpath()` result — element or string node.
fn result_key(node: &Node<'_>) -> String {
    if node.is_text_node() {
        format!("#text|{}", node.text())
    } else {
        element_key(node.element_ref())
    }
}

fn keys(nodes: &Nodes<'_>) -> Vec<String> {
    nodes.iter().map(result_key).collect()
}

fn expected_list(row: &Value, field: &str) -> Vec<String> {
    row[field]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn text_of(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// Record `label` as a mismatch when the two sides differ.
fn diff_list(into: &mut Vec<String>, label: &str, expected: &[String], actual: &[String]) {
    if expected != actual {
        into.push(format!("{label}: expected {expected:?}, got {actual:?}"));
    }
}

fn diff_str(into: &mut Vec<String>, label: &str, expected: &str, actual: &str) {
    if expected != actual {
        into.push(format!("{label}: expected {expected:?}, got {actual:?}"));
    }
}

fn diff_opt(into: &mut Vec<String>, label: &str, expected: Option<&str>, actual: Option<&str>) {
    if expected != actual {
        into.push(format!("{label}: expected {expected:?}, got {actual:?}"));
    }
}

fn diff_usize(into: &mut Vec<String>, label: &str, expected: u64, actual: usize) {
    if expected != actual as u64 {
        into.push(format!("{label}: expected {expected}, got {actual}"));
    }
}

fn diff_bool(into: &mut Vec<String>, label: &str, expected: bool, actual: bool) {
    if expected != actual {
        into.push(format!("{label}: expected {expected}, got {actual}"));
    }
}

fn finish(mismatches: Vec<String>) -> Result<(), String> {
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(mismatches.join("; "))
    }
}

// --------------------------------------------------------------------------- //
// Row identity
// --------------------------------------------------------------------------- //

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// A stable, human-readable id for one corpus row.
fn row_id(row: &Value) -> String {
    let page = text_of(&row["page"]);
    match text_of(&row["kind"]) {
        "css" => format!("{page}/css/{}", text_of(&row["q"])),
        "xpath" => format!("{page}/xpath/{}", text_of(&row["q"])),
        "textnode" => format!(
            "{page}/textnode/{}[{}]",
            text_of(&row["q"]),
            row["idx"].as_u64().unwrap_or_default()
        ),
        "generate" => format!(
            "{page}/generate/{}[{}]",
            text_of(&row["sel"]),
            row["idx"].as_u64().unwrap_or_default()
        ),
        "nav" => format!(
            "{page}/nav/{}[{}]",
            text_of(&row["sel"]),
            row["idx"].as_u64().unwrap_or_default()
        ),
        "find_similar" => format!(
            "{page}/find_similar/{}[{}]/{}",
            text_of(&row["sel"]),
            row["idx"].as_u64().unwrap_or_default(),
            compact(&row["opts"])
        ),
        "find_all" => format!("{page}/find_all/{}", compact(&row["spec"])),
        "find" => format!("{page}/find/{}", compact(&row["spec"])),
        "by_text" => format!(
            "{page}/by_text/{}/{}",
            text_of(&row["text"]),
            compact(&row["opts"])
        ),
        "by_regex" => format!(
            "{page}/by_regex/{}/{}",
            text_of(&row["pattern"]),
            compact(&row["opts"])
        ),
        "nodes" => format!(
            "{page}/nodes/{}/{}/{}",
            text_of(&row["base"]),
            text_of(&row["op"]),
            text_of(&row["arg"])
        ),
        "doc_text" => format!("{page}/doc_text"),
        "adaptive" => format!(
            "adaptive/{}->{page}/{}/{}",
            text_of(&row["origin"]),
            text_of(&row["save_sel"]),
            row["percentage"].as_u64().unwrap_or_default()
        ),
        other => format!("{page}/{other}"),
    }
}

// --------------------------------------------------------------------------- //
// Named predicates — `find_all`'s callable filters, and `Nodes::filter`/`search`
// --------------------------------------------------------------------------- //

fn named_predicate(name: &str) -> fn(&Node<'_>) -> bool {
    match name {
        "has_id" => |node: &Node<'_>| node.attr("id").is_some_and(|value| !value.is_empty()),
        "two_plus_attrs" => |node: &Node<'_>| node.attrib().len() >= 2,
        "text_nonempty" => |node: &Node<'_>| !node.text().trim().is_empty(),
        "tag_is_a" => |node: &Node<'_>| node.tag() == "a",
        "has_class_link" => |node: &Node<'_>| node.has_class("link"),
        other => panic!("unknown predicate {other}"),
    }
}

/// Python's `_whitelisted` map: keyword arguments only.
fn unreserve(key: &str) -> &str {
    match key {
        "class_" => "class",
        "for_" => "for",
        other => other,
    }
}

// --------------------------------------------------------------------------- //
// Option decoding
// --------------------------------------------------------------------------- //

fn all_text_opts(separator: &str, strip: bool, ignore: &[&str], valid: bool) -> AllTextOpts {
    AllTextOpts::new()
        .separator(separator)
        .strip(strip)
        .ignore_tags(ignore.iter().copied())
        .valid_values(valid)
}

fn similar_opts(opts: &Value) -> SimilarOpts {
    let ignore: Vec<String> = opts["ignore"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    SimilarOpts::new()
        .threshold(opts["threshold"].as_f64().unwrap_or(0.2))
        .ignore_attributes(ignore)
        .match_text(opts["match_text"].as_bool().unwrap_or(false))
}

fn by_text_opts(opts: &Value) -> ByTextOpts {
    ByTextOpts::new()
        .first_match(opts["first_match"].as_bool().unwrap_or(true))
        .partial(opts["partial"].as_bool().unwrap_or(false))
        .case_sensitive(opts["case_sensitive"].as_bool().unwrap_or(false))
        .clean_match(opts["clean_match"].as_bool().unwrap_or(true))
}

fn by_regex_opts(opts: &Value) -> ByRegexOpts {
    ByRegexOpts::new()
        .first_match(opts["first_match"].as_bool().unwrap_or(true))
        .case_sensitive(opts["case_sensitive"].as_bool().unwrap_or(false))
        .clean_match(opts["clean_match"].as_bool().unwrap_or(true))
}

// --------------------------------------------------------------------------- //
// Row checks
// --------------------------------------------------------------------------- //

fn check_css(doc: &Document, row: &Value) -> Result<(), String> {
    let query = text_of(&row["q"]);
    let found = match doc.css(query) {
        Ok(found) => {
            if row["error"].as_bool().unwrap_or(false) {
                return Err(format!(
                    "expected a syntax error for {query:?}, got a result"
                ));
            }
            found
        }
        Err(error) => {
            return if row["error"].as_bool().unwrap_or(false) {
                Ok(())
            } else {
                Err(format!("css({query:?}) failed: {error}"))
            };
        }
    };

    let mut bad = Vec::new();
    diff_usize(
        &mut bad,
        "n",
        row["n"].as_u64().unwrap_or_default(),
        found.len(),
    );
    diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
    diff_list(
        &mut bad,
        "texts",
        &expected_list(row, "texts"),
        &found.texts(),
    );
    diff_opt(&mut bad, "get", row["get"].as_str(), found.get().as_deref());
    diff_str(
        &mut bad,
        "get_default",
        text_of(&row["get_default"]),
        &found.get_or("<none>"),
    );
    diff_list(
        &mut bad,
        "getall",
        &expected_list(row, "getall"),
        &found.getall(),
    );
    finish(bad)
}

fn check_xpath(doc: &Document, row: &Value) -> Result<(), String> {
    let query = text_of(&row["q"]);
    let found = match doc.xpath(query) {
        Ok(found) => {
            if row["error"].as_bool().unwrap_or(false) {
                return Err(format!(
                    "expected a syntax error for {query:?}, got a result"
                ));
            }
            found
        }
        Err(error) => {
            return if row["error"].as_bool().unwrap_or(false) {
                Ok(())
            } else {
                Err(format!("xpath({query:?}) failed: {error}"))
            };
        }
    };

    let mut bad = Vec::new();
    diff_usize(
        &mut bad,
        "n",
        row["n"].as_u64().unwrap_or_default(),
        found.len(),
    );
    diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
    diff_list(
        &mut bad,
        "texts",
        &expected_list(row, "texts"),
        &found.texts(),
    );
    diff_opt(&mut bad, "get", row["get"].as_str(), found.get().as_deref());
    diff_list(
        &mut bad,
        "getall",
        &expected_list(row, "getall"),
        &found.getall(),
    );
    finish(bad)
}

fn check_textnode(doc: &Document, row: &Value) -> Result<(), String> {
    let query = text_of(&row["q"]);
    let index = row["idx"].as_u64().unwrap_or_default() as usize;
    let found = doc
        .css(query)
        .map_err(|error| format!("css({query:?}) failed: {error}"))?;
    let node = match found.at(index) {
        Some(node) => node.clone(),
        None => return Err(format!("css({query:?}) has no result {index}")),
    };

    let mut bad = Vec::new();
    diff_bool(
        &mut bad,
        "is_text",
        row["is_text"].as_bool().unwrap_or(false),
        node.is_text_node(),
    );
    diff_str(&mut bad, "tag", text_of(&row["tag"]), node.tag());
    diff_str(&mut bad, "text", text_of(&row["text"]), node.text());
    diff_str(
        &mut bad,
        "string",
        text_of(&row["string"]),
        &node.to_string(),
    );
    if let Some(expected) = row["repr"].as_str() {
        diff_str(&mut bad, "repr", expected, &format!("{node:?}"));
    }
    let attrib = node.attrib();
    let mut ours: Vec<(String, String)> = attrib
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    ours.sort();
    let mut theirs: Vec<(String, String)> = row["attrib"]
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    theirs.sort();
    if ours != theirs {
        bad.push(format!("attrib: expected {theirs:?}, got {ours:?}"));
    }
    diff_str(&mut bad, "get", text_of(&row["get"]), &node.get());
    diff_list(
        &mut bad,
        "getall",
        &expected_list(row, "getall"),
        &node.getall(),
    );
    diff_str(
        &mut bad,
        "html_content",
        text_of(&row["html_content"]),
        &node.html_content(),
    );
    diff_bool(
        &mut bad,
        "has_class_link",
        row["has_class_link"].as_bool().unwrap_or(false),
        node.has_class("link"),
    );
    diff_bool(
        &mut bad,
        "contains_id",
        row["contains_id"].as_bool().unwrap_or(false),
        node.contains_attr("id"),
    );
    diff_list(
        &mut bad,
        "children",
        &expected_list(row, "children"),
        &keys(&node.children()),
    );
    diff_list(
        &mut bad,
        "siblings",
        &expected_list(row, "siblings"),
        &keys(&node.siblings()),
    );
    diff_usize(
        &mut bad,
        "below",
        row["below"].as_u64().unwrap_or_default(),
        node.below_elements().len(),
    );
    diff_opt(
        &mut bad,
        "parent",
        row["parent"].as_str(),
        node.parent().map(|n| result_key(&n)).as_deref(),
    );
    diff_opt(
        &mut bad,
        "next",
        row["next"].as_str(),
        node.next().map(|n| result_key(&n)).as_deref(),
    );
    diff_opt(
        &mut bad,
        "previous",
        row["previous"].as_str(),
        node.previous().map(|n| result_key(&n)).as_deref(),
    );
    let ancestors: Vec<String> = node.ancestors().map(|n| n.tag().to_owned()).collect();
    diff_list(
        &mut bad,
        "ancestors",
        &expected_list(row, "ancestors"),
        &ancestors,
    );
    diff_usize(
        &mut bad,
        "path",
        row["path"].as_u64().unwrap_or_default(),
        node.path().len(),
    );
    diff_str(
        &mut bad,
        "css_path",
        text_of(&row["css_path"]),
        &node.css_path(),
    );
    diff_str(
        &mut bad,
        "xpath_path",
        text_of(&row["xpath_path"]),
        &node.xpath_path(),
    );
    diff_str(
        &mut bad,
        "all_text",
        text_of(&row["all_text"]),
        &node.get_all_text(),
    );
    diff_str(
        &mut bad,
        "all_text_strip",
        text_of(&row["all_text_strip"]),
        &node.get_all_text_with(&all_text_opts("\n", true, &["script", "style"], true)),
    );
    let css_empty = node
        .css("div")
        .map_err(|error| format!("nested css failed: {error}"))?;
    diff_list(
        &mut bad,
        "css_empty",
        &expected_list(row, "css_empty"),
        &keys(&css_empty),
    );
    let xpath_empty = node
        .xpath(".//div")
        .map_err(|error| format!("nested xpath failed: {error}"))?;
    diff_list(
        &mut bad,
        "xpath_empty",
        &expected_list(row, "xpath_empty"),
        &keys(&xpath_empty),
    );
    diff_str(
        &mut bad,
        "body",
        text_of(&row["body"]),
        &node.body().as_str(),
    );
    finish(bad)
}

/// Look a receiver up as `(css selector, index)`, the way the corpus addresses it.
fn receiver<'a>(doc: &'a Document, row: &Value) -> Result<Node<'a>, String> {
    let selector = text_of(&row["sel"]);
    let index = row["idx"].as_u64().unwrap_or_default() as usize;
    let found = doc
        .css(selector)
        .map_err(|error| format!("css({selector:?}) failed: {error}"))?;
    found
        .at(index)
        .cloned()
        .ok_or_else(|| format!("css({selector:?}) has no result {index}"))
}

fn check_generate(doc: &Document, row: &Value) -> Result<(), String> {
    let node = receiver(doc, row)?;
    let mut bad = Vec::new();
    diff_str(&mut bad, "css", text_of(&row["css"]), &node.css_path());
    diff_str(
        &mut bad,
        "css_full",
        text_of(&row["css_full"]),
        &node.css_path_full(),
    );
    diff_str(
        &mut bad,
        "xpath",
        text_of(&row["xpath"]),
        &node.xpath_path(),
    );
    diff_str(
        &mut bad,
        "xpath_full",
        text_of(&row["xpath_full"]),
        &node.xpath_path_full(),
    );
    finish(bad)
}

fn check_nav(doc: &Document, row: &Value) -> Result<(), String> {
    let node = receiver(doc, row)?;
    let mut bad = Vec::new();
    diff_str(&mut bad, "tag", text_of(&row["tag"]), node.tag());
    diff_str(&mut bad, "text", text_of(&row["text"]), node.text());

    let attrib = node.attrib();
    let mut ours: Vec<(String, String)> = attrib
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    ours.sort();
    let mut theirs: Vec<(String, String)> = row["attrib"]
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    theirs.sort();
    if ours != theirs {
        bad.push(format!("attrib: expected {theirs:?}, got {ours:?}"));
    }

    diff_list(
        &mut bad,
        "children",
        &expected_list(row, "children"),
        &keys(&node.children()),
    );
    diff_list(
        &mut bad,
        "siblings",
        &expected_list(row, "siblings"),
        &keys(&node.siblings()),
    );
    diff_usize(
        &mut bad,
        "below",
        row["below"].as_u64().unwrap_or_default(),
        node.below_elements().len(),
    );
    diff_opt(
        &mut bad,
        "next",
        row["next"].as_str(),
        node.next().map(|n| result_key(&n)).as_deref(),
    );
    diff_opt(
        &mut bad,
        "previous",
        row["previous"].as_str(),
        node.previous().map(|n| result_key(&n)).as_deref(),
    );
    diff_opt(
        &mut bad,
        "parent",
        row["parent"].as_str(),
        node.parent().map(|n| result_key(&n)).as_deref(),
    );
    let ancestors: Vec<String> = node.ancestors().map(|n| n.tag().to_owned()).collect();
    diff_list(
        &mut bad,
        "ancestors",
        &expected_list(row, "ancestors"),
        &ancestors,
    );
    let path: Vec<String> = node.path().iter().map(|n| n.tag().to_owned()).collect();
    diff_list(&mut bad, "path", &expected_list(row, "path"), &path);
    let first_class = node
        .attr("class")
        .and_then(|value| value.split_whitespace().next())
        .map(str::to_owned);
    diff_opt(
        &mut bad,
        "has_class_first",
        row["has_class_first"].as_str(),
        first_class.as_deref(),
    );
    finish(bad)
}

fn check_find_similar(doc: &Document, row: &Value) -> Result<(), String> {
    let node = receiver(doc, row)?;
    let found = node.find_similar(&similar_opts(&row["opts"]));
    let mut bad = Vec::new();
    diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
    finish(bad)
}

/// Rebuild one `find_all` invocation from its recorded spec.
///
/// The `regex` pattern has to outlive the query, so the caller owns it.
fn build_query<'f>(spec: &Value, pattern: &'f str) -> FindQuery<'f> {
    let mut query = FindQuery::new();
    for tag in spec["tags"].as_array().into_iter().flatten() {
        query = query.tag(tag.as_str().unwrap_or_default());
    }
    for pair in spec["attrs"].as_array().into_iter().flatten() {
        query = query.attr(text_of(&pair[0]), text_of(&pair[1]));
    }
    for pair in spec["kwargs"].as_array().into_iter().flatten() {
        query = query.attr(unreserve(text_of(&pair[0])), text_of(&pair[1]));
    }
    if !pattern.is_empty() {
        query = query.regex(pattern);
    }
    if let Some(name) = spec["pred"].as_str() {
        query = query.predicate(named_predicate(name));
    }
    query
}

fn check_find_all(doc: &Document, row: &Value) -> Result<(), String> {
    let spec = &row["spec"];
    let pattern = spec["regex"].as_str().unwrap_or_default().to_owned();
    let query = build_query(spec, &pattern);
    match doc.find_all(&query) {
        Ok(found) => {
            if row["error"].as_bool().unwrap_or(false) {
                return Err("expected an error, got a result".to_owned());
            }
            let mut bad = Vec::new();
            diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
            finish(bad)
        }
        Err(error) => {
            if row["error"].as_bool().unwrap_or(false) {
                Ok(())
            } else {
                Err(format!("find_all failed: {error}"))
            }
        }
    }
}

fn check_find(doc: &Document, row: &Value) -> Result<(), String> {
    let spec = &row["spec"];
    let pattern = spec["regex"].as_str().unwrap_or_default().to_owned();
    let query = build_query(spec, &pattern);
    match doc.find(&query) {
        Ok(found) => {
            if row["error"].as_bool().unwrap_or(false) {
                return Err("expected an error, got a result".to_owned());
            }
            let mut bad = Vec::new();
            diff_opt(
                &mut bad,
                "key",
                row["key"].as_str(),
                found.map(|n| result_key(&n)).as_deref(),
            );
            finish(bad)
        }
        Err(error) => {
            if row["error"].as_bool().unwrap_or(false) {
                Ok(())
            } else {
                Err(format!("find failed: {error}"))
            }
        }
    }
}

fn check_by_text(doc: &Document, row: &Value) -> Result<(), String> {
    let found = doc.find_by_text(text_of(&row["text"]), &by_text_opts(&row["opts"]));
    let mut bad = Vec::new();
    diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
    // Python's return-type asymmetry, re-expressed: `single` is true exactly
    // when it handed back a bare `Selector`, which is exactly a one-element
    // `Nodes` here. A `first_match` search with no hit is `single = false` with
    // an empty list — never `None`.
    let single = row["single"].as_bool().unwrap_or(false);
    if single && found.len() != 1 {
        bad.push(format!("single result expected, got {}", found.len()));
    }
    finish(bad)
}

fn check_by_regex(doc: &Document, row: &Value) -> Result<(), String> {
    let pattern = text_of(&row["pattern"]);
    let found = doc
        .find_by_regex(RegexSpec::Pattern(pattern), &by_regex_opts(&row["opts"]))
        .map_err(|error| format!("find_by_regex failed: {error}"))?;
    let mut bad = Vec::new();
    diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
    let single = row["single"].as_bool().unwrap_or(false);
    if single && found.len() != 1 {
        bad.push(format!("single result expected, got {}", found.len()));
    }
    finish(bad)
}

fn check_nodes(doc: &Document, row: &Value) -> Result<(), String> {
    let base = doc
        .css(text_of(&row["base"]))
        .map_err(|error| format!("base css failed: {error}"))?;
    let argument = text_of(&row["arg"]);
    let mut bad = Vec::new();
    match text_of(&row["op"]) {
        "css" => {
            let found = base
                .css(argument)
                .map_err(|error| format!("Nodes::css failed: {error}"))?;
            diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
        }
        "xpath" => {
            let found = base
                .xpath(argument)
                .map_err(|error| format!("Nodes::xpath failed: {error}"))?;
            diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
        }
        "re" => {
            let values = base
                .re(RegexSpec::Pattern(argument), ReOptions::new())
                .map_err(|error| format!("Nodes::re failed: {error}"))?;
            diff_list(&mut bad, "values", &expected_list(row, "values"), &values);
        }
        "re_first" => {
            let value = base
                .re_first(RegexSpec::Pattern(argument), ReOptions::new())
                .map_err(|error| format!("Nodes::re_first failed: {error}"))?;
            diff_opt(&mut bad, "value", row["value"].as_str(), value.as_deref());
        }
        "get" => diff_opt(
            &mut bad,
            "value",
            row["value"].as_str(),
            base.get().as_deref(),
        ),
        "getall" => diff_list(
            &mut bad,
            "values",
            &expected_list(row, "values"),
            &base.getall(),
        ),
        "search" => {
            let found = base.search(named_predicate(argument));
            diff_opt(
                &mut bad,
                "key",
                row["key"].as_str(),
                found.map(|n| result_key(&n)).as_deref(),
            );
        }
        "filter" => {
            let found = base.filter(named_predicate(argument));
            diff_list(&mut bad, "keys", &expected_list(row, "keys"), &keys(&found));
        }
        "first" => diff_opt(
            &mut bad,
            "key",
            row["key"].as_str(),
            base.first().map(result_key).as_deref(),
        ),
        "last" => diff_opt(
            &mut bad,
            "key",
            row["key"].as_str(),
            base.last().map(result_key).as_deref(),
        ),
        "length" => diff_usize(
            &mut bad,
            "value",
            row["value"].as_u64().unwrap_or_default(),
            base.length(),
        ),
        other => return Err(format!("unknown Nodes op {other}")),
    }
    finish(bad)
}

fn check_doc_text(doc: &Document, row: &Value) -> Result<(), String> {
    let mut bad = Vec::new();
    diff_str(
        &mut bad,
        "default",
        text_of(&row["default"]),
        &doc.get_all_text(),
    );
    diff_str(
        &mut bad,
        "strip",
        text_of(&row["strip"]),
        &doc.get_all_text_with(&all_text_opts("\n", true, &["script", "style"], true)),
    );
    diff_str(
        &mut bad,
        "joined",
        text_of(&row["joined"]),
        &doc.get_all_text_with(&all_text_opts(" ", true, &["script", "style"], true)),
    );
    diff_str(
        &mut bad,
        "no_ignore",
        text_of(&row["no_ignore"]),
        &doc.get_all_text_with(&all_text_opts("\n", false, &[], true)),
    );
    diff_str(
        &mut bad,
        "invalid_ok",
        text_of(&row["invalid_ok"]),
        &doc.get_all_text_with(&all_text_opts("\n", false, &["script", "style"], false)),
    );
    finish(bad)
}

// --------------------------------------------------------------------------- //
// The end-to-end adaptive path
// --------------------------------------------------------------------------- //

#[cfg(feature = "adaptive")]
fn check_adaptive(row: &Value, directory: &std::path::Path) -> Result<(), String> {
    use std::sync::Arc;

    use browser_parser::config::{CssOpts, DocumentConfig};
    use browser_storage::SqliteStorage;

    let identifier = text_of(&row["identifier"]);
    let percentage = row["percentage"].as_u64().unwrap_or(40) as u8;
    let database = directory.join(format!("{:x}.db", identifier.len() as u64));
    let _ = fs::remove_file(&database);
    let storage = SqliteStorage::open(&database, Some("https://example.com/"))
        .map_err(|error| format!("open storage: {error}"))?;
    let config = DocumentConfig::new()
        .url("https://example.com/")
        .storage(Arc::new(storage) as Arc<dyn browser_storage::StorageSystem>);

    let original = Document::parse_with(&page_html(text_of(&row["origin"])), &config);
    let saved = original
        .css_with(
            text_of(&row["save_sel"]),
            &CssOpts::new().identifier(identifier).auto_save(true),
        )
        .map_err(|error| format!("save-side css failed: {error}"))?;
    let mut bad = Vec::new();
    match saved.first() {
        Some(node) => diff_str(
            &mut bad,
            "saved_key",
            text_of(&row["saved_key"]),
            &result_key(node),
        ),
        None => bad.push("save-side css matched nothing".to_owned()),
    }
    if let Some(node) = saved.first() {
        let expected: ElementFingerprint = serde_json::from_str(text_of(&row["fingerprint"]))
            .map_err(|error| format!("bad fingerprint: {error}"))?;
        if node.fingerprint() != expected {
            bad.push(format!(
                "fingerprint: expected {expected:?}, got {:?}",
                node.fingerprint()
            ));
        }
    }

    let mutated = Document::parse_with(&page_html(text_of(&row["page"])), &config);
    let retrieved = mutated
        .retrieve(identifier)
        .map_err(|error| format!("retrieve failed: {error}"))?
        .ok_or_else(|| "retrieve returned nothing — auto_save did not store".to_owned())?;
    let expected_retrieved: ElementFingerprint =
        serde_json::from_str(text_of(&row["retrieved"]))
            .map_err(|error| format!("bad retrieved fingerprint: {error}"))?;
    if retrieved != expected_retrieved {
        bad.push(format!(
            "retrieved: expected {expected_retrieved:?}, got {retrieved:?}"
        ));
    }

    let relocated = mutated
        .css_with(
            text_of(&row["query_sel"]),
            &CssOpts::new()
                .identifier(identifier)
                .adaptive(true)
                .percentage(percentage),
        )
        .map_err(|error| format!("adaptive css failed: {error}"))?;
    diff_list(
        &mut bad,
        "keys",
        &expected_list(row, "keys"),
        &keys(&relocated),
    );

    // Every element Python scored must score identically here. Elements only
    // html5ever builds (a synthesised `<head>`, an implied `<tbody>`) add
    // entries on our side and are covered by the `keys` comparison above.
    let mut ours: HashMap<String, Vec<f64>> = HashMap::new();
    for element in mutated.root().below_elements().iter() {
        let candidate = element.element_ref();
        let score = similarity_score(
            &expected_retrieved,
            &browser_parser::adaptive::element_to_fingerprint(candidate),
        );
        ours.entry(element_key(candidate)).or_default().push(score);
    }
    for entry in row["scores"].as_array().into_iter().flatten() {
        let key = text_of(&entry[0]);
        let score = entry[1].as_f64().unwrap_or(f64::NAN);
        match ours.get_mut(key) {
            Some(scores) => match scores.iter().position(|value| *value == score) {
                Some(at) => {
                    scores.remove(at);
                }
                None => bad.push(format!("score for {key}: expected {score}, got {scores:?}")),
            },
            None => bad.push(format!("no element keyed {key}")),
        }
    }
    finish(bad)
}

// --------------------------------------------------------------------------- //
// Declared divergences
// --------------------------------------------------------------------------- //

include!("support/divergences.rs");

// --------------------------------------------------------------------------- //
// The replay
// --------------------------------------------------------------------------- //

fn dispatch(doc: &Document, row: &Value) -> Result<(), String> {
    match text_of(&row["kind"]) {
        "css" => check_css(doc, row),
        "xpath" => check_xpath(doc, row),
        "textnode" => check_textnode(doc, row),
        "generate" => check_generate(doc, row),
        "nav" => check_nav(doc, row),
        "find_similar" => check_find_similar(doc, row),
        "find_all" => check_find_all(doc, row),
        "find" => check_find(doc, row),
        "by_text" => check_by_text(doc, row),
        "by_regex" => check_by_regex(doc, row),
        "nodes" => check_nodes(doc, row),
        "doc_text" => check_doc_text(doc, row),
        other => Err(format!("unknown row kind {other}")),
    }
}

#[test]
fn the_public_api_corpus_replays() {
    let rows = corpus_rows();
    assert!(
        rows.len() >= 1500,
        "the corpus must stay broad; found only {} rows",
        rows.len()
    );

    let declared: HashMap<&str, &str> = EXPECTED_DIVERGENCES.iter().copied().collect();
    let mut failures: Vec<String> = Vec::new();
    let mut unexpectedly_passing: Vec<String> = Vec::new();
    let mut passed = 0usize;
    let mut diverged = 0usize;

    // Page-at-a-time so each tree is parsed once and `Node`'s borrow is simple.
    let mut pages: Vec<&str> = rows
        .iter()
        .filter(|row| text_of(&row["kind"]) != "adaptive")
        .map(|row| text_of(&row["page"]))
        .collect();
    pages.sort_unstable();
    pages.dedup();

    for page in pages {
        let doc = Document::parse(&page_html(page));
        for row in rows
            .iter()
            .filter(|row| text_of(&row["kind"]) != "adaptive" && text_of(&row["page"]) == page)
        {
            let id = row_id(row);
            let outcome = dispatch(&doc, row);
            match (outcome, declared.get(id.as_str())) {
                (Ok(()), None) => passed += 1,
                (Ok(()), Some(reason)) => {
                    unexpectedly_passing.push(format!("{id} — declared divergent because {reason}"))
                }
                (Err(message), Some(_)) => {
                    let _ = message;
                    diverged += 1;
                }
                (Err(message), None) => failures.push(format!("{id}: {message}")),
            }
        }
    }

    #[cfg(feature = "adaptive")]
    {
        let directory = tempfile::tempdir().expect("temp dir");
        for (index, row) in rows
            .iter()
            .filter(|row| text_of(&row["kind"]) == "adaptive")
            .enumerate()
        {
            let id = row_id(row);
            let slot = directory.path().join(index.to_string());
            fs::create_dir_all(&slot).expect("scenario dir");
            let outcome = check_adaptive(row, &slot);
            match (outcome, declared.get(id.as_str())) {
                (Ok(()), None) => passed += 1,
                (Ok(()), Some(reason)) => {
                    unexpectedly_passing.push(format!("{id} — declared divergent because {reason}"))
                }
                (Err(_), Some(_)) => diverged += 1,
                (Err(message), None) => failures.push(format!("{id}: {message}")),
            }
        }
    }

    let total = passed + diverged + failures.len();
    let report = format!(
        "{passed}/{total} rows reproduce the reference implementation ({:.1} %), {diverged} declared divergences",
        100.0 * passed as f64 / total as f64
    );

    if let Ok(path) = std::env::var("PARSER_GOLDEN_DUMP") {
        let _ = fs::write(path, failures.join("\n"));
    }

    assert!(
        unexpectedly_passing.is_empty(),
        "{} declared divergences now PASS — re-read their reasons and delete the entries:\n{}",
        unexpectedly_passing.len(),
        unexpectedly_passing.join("\n")
    );
    assert!(
        failures.is_empty(),
        "{}\n{} undeclared mismatches:\n{}",
        report,
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!("{report}");
}
