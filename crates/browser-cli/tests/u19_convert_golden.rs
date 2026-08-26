//! Replay of `testdata/u19_convert.jsonl` — the `Convertor` differential.
//!
//! The corpus was produced by running **the reference implementation 0.4.15's own
//! `Convertor._extract_content`** over 31 documents × 3 modes × 7 selectors ×
//! `main_content_only` on/off, plus the 13 filename rows that pin the extension
//! map (`tools/corpus/u19/gen_convert.py`). It replays in pure Rust: no Python,
//! no network.
//!
//! Six of the documents and the `td` selector were added in wave 6, promoted
//! from `tools/corpus/u19/fuzz_convert.py` — 9000 random documents across three
//! seeds, which found four divergence families the hand-written matrix missed
//! and **no** defect in this crate's own code.
//!
//! Two things are compared per row: the **pieces** the generator yields (so the
//! trailing `""` and the one-piece-per-match rule are checked, not just the
//! concatenation) and the **joined** string that ends up in the file.
//!
//! # The honest number
//!
//! This is the corpus with real divergences in it. Every row that does not
//! reproduce Python is named in [`DIVERGENCES`] with a reason, and the test
//! **fails if a declared row starts passing** — because a refactor that
//! accidentally fixes one should be noticed, not absorbed.
//!
//! **1196 of 1302 extraction rows reproduce (91.9%).** Wave 6's green agent
//! closed three whole families, 113 rows, by adding the two APIs
//! `browser_cli::convert` had been approximating around —
//! [`Document::clean_for_ai`](browser::Document::clean_for_ai) and
//! [`Node::markdown`](browser::Node::markdown). `_extract_content` is now
//! Python's control flow statement for statement, so:
//!
//! * cleaning happens **only** when `main_content_only` is set, and for all
//!   three modes rather than just markdown (families A and B, 100 rows);
//! * markdown yields **one piece per CSS match** rather than one aggregated
//!   string (family C, 14 rows).
//!
//! Four rows that had been filed under A and B turned out to have a different
//! cause once the real one was removed, and were re-classified rather than
//! deleted: the three `list-and-table/html/*/ai` rows are the implied `<tbody>`
//! and `template-first/markdown/body/plain` is `<template>` being parsed into
//! `<head>`.
//!
//! What is left is not this crate's to fix. In order of size:
//!
//! 1. **lxml has no root** for a document with no element, so Python *raises*
//!    where html5ever builds the standard skeleton — 42 rows.
//! 2. **html5ever versus libxml2 tree shape** — the synthesised `<head>` and
//!    the implied `<tbody>`. Carried since unit 5 — 27 rows.
//! 3. **The synthesised `<html><head></head><body></body></html>` skeleton**
//!    for a document that has none of it — 16 rows.
//! 4. **`<noscript>` and `<template>` leave `<head>`** under the HTML5
//!    algorithm, which is what browsers do — 13 rows.
//! 5. **A `td` re-parsed on its own** loses its cell to foster parenting — 4
//!    rows. Python re-parses too (markdownify takes a string); the difference
//!    is that `html.parser` has no foster-parenting rule.

mod support;

use browser::http::config::SelectorConfig;
use browser::{ExtractionType, Response};
use browser_cli::convert::{extract_content, extraction_for};
use serde_json::Value;

/// **Family 2.** `html5ever` synthesises the `<head></head>` libxml2 leaves out
/// and inserts the `<tbody>` the HTML5 parsing algorithm requires. Carried since
/// unit 5 and accepted by `ARCHITECTURE.md` §11.6: html5ever's answer is the one
/// browsers produce, which is what a scraper wants.
const HTML5EVER_TREE_SHAPE: &str =
    "html5ever vs libxml2 tree shape: the synthesised <head></head> and the \
     implied <tbody>";

/// **Family 1.** A document with no element in it — `<!-- just a comment -->`
/// with `keep_comments=False` — leaves `Selector._root` as `None` in lxml, so
/// **Python raises** (`AttributeError: 'NoneType' object has no attribute
/// 'xpath'`, or `TypeError: Type 'NoneType' cannot be serialized` for the
/// no-selector `html` and `markdown` rows). html5ever's tree builder always
/// produces `<html><head></head><body></body></html>`, so the Rust side
/// succeeds and writes an empty file.
///
/// Reproducing the crash would mean teaching `browser-dom` to return no root
/// for a document that HTML5 says has one, which is a worse answer for a
/// scraper. Found by `fuzz_convert.py` seed 61.
const LXML_HAS_NO_ROOT: &str =
    "lxml leaves _root as None for a document with no element and Python raises; \
     html5ever always builds <html><head></head><body></body></html>";

/// **Family 4.** An element HTML5 allows in `<head>`, written before any body
/// content in a document with no explicit `<head>`, ends up **in `<head>`** —
/// so `css("body")` and `get_all_text` on `<body>` never see it, while libxml2
/// leaves it exactly where it was written.
///
/// Two shapes are in the corpus:
///
/// * `<noscript>no js</noscript>` — the "in head noscript" insertion mode pops
///   the element when a character token arrives and reprocesses the text in
///   `<body>`, which is what every browser does. The `<noscript>` *element*
///   stays in `<head>` and its text moves, so `ignore_tags` no longer skips it.
/// * `<template><p>TPL</p></template>` — kept whole, in `<head>`.
///
/// Same sign-off as the rest of the tree-shape family (`ARCHITECTURE.md`
/// §11.6). Found by `fuzz_convert.py` seeds 71 and 211.
const NOSCRIPT_LEAVES_HEAD: &str =
    "html5ever's `in head noscript` mode moves the text out of <noscript> and \
     into <body> the way browsers do; libxml2 keeps it in <head>";

/// **Family 3.** `""`, `<html></html>` and friends: html5ever fills in the
/// whole `<html><head></head><body></body></html>` skeleton, so `css("body")`
/// matches an empty element where lxml finds nothing at all, and the
/// no-selector `html` row serialises the skeleton rather than `<html></html>`.
/// The same tree-shape sign-off. Found by `fuzz_convert.py` seed 61.
const SYNTHESISED_HTML_SKELETON: &str =
    "html5ever synthesises <head> and <body> for a document that has neither, so \
     `body` matches where lxml has nothing to match";

/// **Family 5.** `markdown` with a `td` selector. Python hands markdownify the
/// serialised `<td>a</td>`, which BeautifulSoup keeps as a cell and
/// `convert_td` renders as `" a |"`. `Document::markdown_with` hands
/// `browser-markdown` the same string, but it re-parses it **as a document**,
/// where the HTML5 algorithm foster-parents a `<td>` with no table around it
/// and only the bare text survives.
///
/// The fix is in `browser-markdown` / `Document::markdown_with` (parse the
/// fragment in its parent's context, the way `innerHTML` does), not in this
/// crate — filed as a request for the green agent. Found by `fuzz_convert.py`
/// seed 61.
const TABLE_CELL_FRAGMENT: &str =
    "markdown of a `td` re-parses `<td>a</td>` as a document, where HTML5 \
     foster-parents the cell away; markdownify keeps it and emits ` a |`";

/// Every row that does not reproduce Python, with why.
///
/// **106 of 1302 rows**, in five families, all of them `browser-dom`'s tree
/// shape or `browser-markdown`'s re-parse rather than this crate's. The test
/// asserts that each of these **still** differs, so a change that accidentally
/// fixes one is reported rather than absorbed — which is exactly how wave 6
/// noticed that the three families it closed had taken four unrelated rows down
/// with them.
const DIVERGENCES: &[(&str, &str)] = &[
    // -- LXML_HAS_NO_ROOT: 42 rows ------------------------------
    ("comment-only/html/-/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/-/plain", LXML_HAS_NO_ROOT),
    ("comment-only/html/.item/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/.item/plain", LXML_HAS_NO_ROOT),
    ("comment-only/html/a/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/a/plain", LXML_HAS_NO_ROOT),
    ("comment-only/html/body/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/body/plain", LXML_HAS_NO_ROOT),
    ("comment-only/html/main/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/main/plain", LXML_HAS_NO_ROOT),
    ("comment-only/html/p/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/p/plain", LXML_HAS_NO_ROOT),
    ("comment-only/html/td/ai", LXML_HAS_NO_ROOT),
    ("comment-only/html/td/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/-/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/-/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/.item/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/.item/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/a/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/a/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/body/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/body/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/main/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/main/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/p/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/p/plain", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/td/ai", LXML_HAS_NO_ROOT),
    ("comment-only/markdown/td/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/-/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/-/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/.item/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/.item/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/a/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/a/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/body/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/body/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/main/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/main/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/p/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/p/plain", LXML_HAS_NO_ROOT),
    ("comment-only/text/td/ai", LXML_HAS_NO_ROOT),
    ("comment-only/text/td/plain", LXML_HAS_NO_ROOT),
    // -- HTML5EVER_TREE_SHAPE: 30 rows --------------------------
    ("aria-hidden/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("comments/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("control-chars/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("entities/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-dimensions/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-display-none/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-display-space/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-font-size/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-not-matched/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-opacity/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("hidden-visibility/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("items/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("links/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("list-and-table/html/-/ai", HTML5EVER_TREE_SHAPE),
    ("list-and-table/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("list-and-table/html/body/ai", HTML5EVER_TREE_SHAPE),
    ("list-and-table/html/body/plain", HTML5EVER_TREE_SHAPE),
    ("list-and-table/html/main/ai", HTML5EVER_TREE_SHAPE),
    ("list-and-table/html/main/plain", HTML5EVER_TREE_SHAPE),
    ("mixed-hidden-and-noise/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("no-body/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("noise-tags/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("table-cells/html/-/ai", HTML5EVER_TREE_SHAPE),
    ("table-cells/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("table-cells/html/body/ai", HTML5EVER_TREE_SHAPE),
    ("table-cells/html/body/plain", HTML5EVER_TREE_SHAPE),
    ("template/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("unicode/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("whitespace-runs/html/-/plain", HTML5EVER_TREE_SHAPE),
    ("zero-width/html/-/plain", HTML5EVER_TREE_SHAPE),
    // -- SYNTHESISED_HTML_SKELETON: 16 rows ---------------------
    ("empty-string/html/-/ai", SYNTHESISED_HTML_SKELETON),
    ("empty-string/html/-/plain", SYNTHESISED_HTML_SKELETON),
    ("empty-string/html/body/ai", SYNTHESISED_HTML_SKELETON),
    ("empty-string/html/body/plain", SYNTHESISED_HTML_SKELETON),
    ("empty-string/markdown/body/ai", SYNTHESISED_HTML_SKELETON),
    (
        "empty-string/markdown/body/plain",
        SYNTHESISED_HTML_SKELETON,
    ),
    ("empty-string/text/body/ai", SYNTHESISED_HTML_SKELETON),
    ("empty-string/text/body/plain", SYNTHESISED_HTML_SKELETON),
    ("html-tag-only/html/-/ai", SYNTHESISED_HTML_SKELETON),
    ("html-tag-only/html/-/plain", SYNTHESISED_HTML_SKELETON),
    ("html-tag-only/html/body/ai", SYNTHESISED_HTML_SKELETON),
    ("html-tag-only/html/body/plain", SYNTHESISED_HTML_SKELETON),
    ("html-tag-only/markdown/body/ai", SYNTHESISED_HTML_SKELETON),
    (
        "html-tag-only/markdown/body/plain",
        SYNTHESISED_HTML_SKELETON,
    ),
    ("html-tag-only/text/body/ai", SYNTHESISED_HTML_SKELETON),
    ("html-tag-only/text/body/plain", SYNTHESISED_HTML_SKELETON),
    // -- NOSCRIPT_LEAVES_HEAD: 14 rows --------------------------
    ("noscript-first/html/-/ai", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/html/-/plain", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/html/body/ai", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/html/body/plain", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/markdown/-/ai", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/markdown/body/ai", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/text/-/ai", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/text/-/plain", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/text/body/ai", NOSCRIPT_LEAVES_HEAD),
    ("noscript-first/text/body/plain", NOSCRIPT_LEAVES_HEAD),
    ("template-first/html/-/plain", NOSCRIPT_LEAVES_HEAD),
    ("template-first/html/body/plain", NOSCRIPT_LEAVES_HEAD),
    ("template-first/markdown/body/plain", NOSCRIPT_LEAVES_HEAD),
    ("template-first/text/body/plain", NOSCRIPT_LEAVES_HEAD),
    // -- TABLE_CELL_FRAGMENT: 4 rows ---------------------------
    ("list-and-table/markdown/td/ai", TABLE_CELL_FRAGMENT),
    ("list-and-table/markdown/td/plain", TABLE_CELL_FRAGMENT),
    ("table-cells/markdown/td/ai", TABLE_CELL_FRAGMENT),
    ("table-cells/markdown/td/plain", TABLE_CELL_FRAGMENT),
];

fn corpus() -> Vec<Value> {
    include_str!("../testdata/u19_convert.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON row"))
        .collect()
}

/// Build the page the CLI would have: a `Response` carrying the fetched body,
/// with `BaseFetcher._generate_parser_arguments()`'s defaults — which are the
/// same as `Selector(html)`'s, and are what the corpus generator used.
fn page(html: &str) -> Response {
    Response::builder(200, "https://example.test/")
        .body(html.to_owned())
        .header("Content-Type", "text/html; charset=utf-8")
        .selector_config(SelectorConfig::parser_defaults())
        .build()
}

fn mode_of(name: &str) -> ExtractionType {
    match name {
        "markdown" => ExtractionType::Markdown,
        "html" => ExtractionType::Html,
        "text" => ExtractionType::Text,
        other => panic!("unknown mode {other}"),
    }
}

/// What one row produced on the Rust side.
enum Outcome {
    Pieces(Vec<String>),
    Error(String),
}

fn run(row: &Value) -> Outcome {
    let response = page(row["html"].as_str().expect("html"));
    let mode = mode_of(row["mode"].as_str().expect("mode"));
    let selector = row["css_selector"].as_str();
    let main_only = row["main_content_only"].as_bool().expect("a bool");
    match extract_content(&response, mode, selector, main_only) {
        Ok(pieces) => Outcome::Pieces(pieces),
        Err(error) => Outcome::Error(error.to_string()),
    }
}

/// Whether the row reproduces Python, and a description of the first difference.
fn agrees(row: &Value, outcome: &Outcome) -> Result<(), String> {
    let id = row["id"].as_str().expect("an id");
    match (row.get("error"), outcome) {
        (Some(error), Outcome::Error(got)) => {
            let want = error.as_str().expect("a message");
            if want.ends_with(got) {
                Ok(())
            } else {
                Err(format!("{id}: python raised {want:?}, rust said {got:?}"))
            }
        }
        (Some(error), Outcome::Pieces(_)) => Err(format!(
            "{id}: python raised {:?}, rust succeeded",
            error.as_str().unwrap_or_default()
        )),
        (None, Outcome::Error(got)) => Err(format!("{id}: rust failed with {got:?}")),
        (None, Outcome::Pieces(pieces)) => {
            let want_pieces: Vec<String> = row["pieces"]
                .as_array()
                .expect("pieces")
                .iter()
                .map(|piece| piece.as_str().expect("a string").to_owned())
                .collect();
            let want_joined = row["joined"].as_str().expect("joined");
            let got_joined = pieces.concat();
            if want_pieces != *pieces {
                return Err(format!(
                    "{id}: pieces differ\n  python: {want_pieces:?}\n  rust:   {pieces:?}"
                ));
            }
            if want_joined != got_joined {
                return Err(format!(
                    "{id}: joined differs\n  python: {want_joined:?}\n  rust:   {got_joined:?}"
                ));
            }
            Ok(())
        }
    }
}

#[test]
fn the_convertor_reproduces_python() {
    let rows: Vec<Value> = corpus()
        .into_iter()
        .filter(|row| row.get("html").is_some())
        .collect();
    assert!(
        rows.len() >= 1300,
        "the corpus is 1302 extraction rows, got {}",
        rows.len()
    );

    let mut failures: Vec<String> = Vec::new();
    let mut matched = 0_usize;
    let mut divergent_still_differ: Vec<String> = Vec::new();

    for row in &rows {
        let id = row["id"].as_str().expect("an id").to_owned();
        let declared = DIVERGENCES.iter().any(|(name, _)| *name == id);
        let outcome = run(row);
        match (agrees(row, &outcome), declared) {
            (Ok(()), false) => matched += 1,
            (Ok(()), true) => divergent_still_differ.push(id),
            (Err(_), true) => {}
            (Err(message), false) => failures.push(message),
        }
    }

    assert!(
        divergent_still_differ.is_empty(),
        "these rows are declared as divergences but now reproduce Python — \
         delete them from DIVERGENCES together with the reason:\n{}",
        divergent_still_differ.join("\n")
    );

    let comparable = rows.len() - DIVERGENCES.len();
    println!(
        "convertor: {matched}/{comparable} comparable rows reproduce Python \
         ({} declared divergences of {} total)",
        DIVERGENCES.len(),
        rows.len()
    );
    assert!(
        failures.is_empty(),
        "{matched}/{comparable} reproduce; {} rows differ and are not declared:\n{}",
        failures.len(),
        failures
            .iter()
            .take(200)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The 13 filename rows: the extension map and its two error branches.
#[test]
fn the_extension_map_reproduces_python() {
    let rows: Vec<Value> = corpus()
        .into_iter()
        .filter(|row| row.get("filename").is_some())
        .collect();
    assert_eq!(rows.len(), 13, "the filename corpus is 13 rows");

    for row in &rows {
        let filename = row["filename"].as_str().expect("a filename");
        match (row.get("mode"), row.get("error")) {
            (Some(mode), None) => {
                let want = mode_of(mode.as_str().expect("a mode"));
                assert_eq!(
                    extraction_for(filename).expect("accepted"),
                    want,
                    "{filename:?}"
                );
            }
            (None, Some(error)) => {
                let want = error.as_str().expect("a message");
                let got = extraction_for(filename).expect_err("rejected").to_string();
                assert!(
                    want.ends_with(&got),
                    "{filename:?}: python said {want:?}, rust said {got:?}"
                );
            }
            other => panic!("{filename:?}: malformed row {other:?}"),
        }
    }
}

/// A guard on the divergence table: a stale entry asserts nothing.
#[test]
fn every_declared_divergence_names_a_real_row() {
    let rows = corpus();
    for (name, reason) in DIVERGENCES {
        assert!(
            rows.iter().any(|row| row["id"].as_str() == Some(name)),
            "declared divergence {name} is not a corpus row"
        );
        assert!(!reason.is_empty(), "{name} has no reason");
    }
}

/// A diagnostic, not a check: with `U19_DUMP=1` it prints the actual
/// difference behind every declared divergence, which is how each row above was
/// classified in the first place. It asserts nothing, so it cannot fail; the
/// assertion that a declared row still differs lives in
/// [`the_convertor_reproduces_python`].
#[test]
fn dump_declared_divergences() {
    if std::env::var("U19_DUMP").is_err() {
        return;
    }
    for row in corpus().iter().filter(|row| row.get("html").is_some()) {
        let id = row["id"].as_str().expect("an id");
        let Some((_, reason)) = DIVERGENCES.iter().find(|(name, _)| *name == id) else {
            continue;
        };
        let outcome = run(row);
        match agrees(row, &outcome) {
            Ok(()) => println!("### {id} NOW AGREES"),
            Err(message) => println!("### [{}] {message}", &reason[..20]),
        }
    }
}
