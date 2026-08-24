//! Byte-for-byte comparison against Python's `markdownify` 1.2.
//!
//! `testdata/golden*/NAME.md` is `markdownify(testdata/normalized*/NAME.html)`
//! produced by `tools/gen_golden.py` with the real library and no options —
//! which is exactly how the reference implementation calls it. The HTML inputs are what *our* tree
//! serialises to (see `examples/normalize.rs`), so a failure here is a bug in
//! the converter and never a disagreement between `html.parser` and
//! `html5ever`.
//!
//! Regenerate with:
//!
//! ```text
//! cargo run -p browser-markdown --example normalize
//! <python-with-markdownify> crates/browser-markdown/tools/gen_golden.py
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use browser_dom::{Document, ParseOptions};
use browser_markdown::to_markdown_default;

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

fn html_files(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {}", dir.display());
    paths
}

fn run_corpus(html_dir: &str, golden_dir: &str, options: &ParseOptions) {
    let root = testdata();
    let mut failures = Vec::new();
    for path in html_files(&root.join(html_dir)) {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let html = fs::read_to_string(&path).expect("fixture is readable");
        let golden_path = root.join(golden_dir).join(format!("{stem}.md"));
        let expected = fs::read_to_string(&golden_path)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", golden_path.display()));

        let doc = Document::parse_with(&html, options);
        let element = doc.root_element().expect("every fixture has an <html>");
        // the reference implementation converts `element.html_content`, i.e. the element becomes
        // the child of a fresh document — which is what `to_markdown` models.
        let actual = to_markdown_default(element);

        if actual != expected {
            failures.push(format!(
                "--- {html_dir}/{stem} ---\nexpected:\n{expected:?}\nactual:\n{actual:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn matches_markdownify_on_the_default_parser_flags() {
    run_corpus("normalized", "golden", &ParseOptions::new());
}

#[test]
fn matches_markdownify_with_blank_text_preserved() {
    // The interesting corpus: whitespace-only text nodes survive, so
    // `_can_ignore` and `process_text`'s lstrip/rstrip rules actually run.
    run_corpus(
        "normalized-ws",
        "golden-ws",
        &ParseOptions::new().remove_blank_text(false),
    );
}

#[test]
fn converting_the_document_node_matches_converting_the_root_element() {
    let root = testdata();
    for path in html_files(&root.join("normalized")) {
        let html = fs::read_to_string(&path).expect("fixture is readable");
        let doc = Document::parse(&html);
        let element = doc.root_element().expect("every fixture has an <html>");
        assert_eq!(
            to_markdown_default(doc.root()),
            to_markdown_default(element),
            "{}",
            path.display()
        );
    }
}

/// Record separator, shared with `tools/gen_fuzz.py`.
const SEPARATOR: char = '\u{1e}';

#[test]
fn matches_markdownify_on_the_generated_corpus() {
    let root = testdata().join("fuzz");
    let html = fs::read_to_string(root.join("normalized.txt")).expect("fuzz/normalized.txt");
    let golden = fs::read_to_string(root.join("golden.txt")).expect("fuzz/golden.txt");
    let cases: Vec<&str> = html.split(SEPARATOR).collect();
    let expected: Vec<&str> = golden.split(SEPARATOR).collect();
    assert_eq!(cases.len(), expected.len(), "corpora are out of sync");
    assert!(cases.len() > 100, "the generated corpus looks truncated");

    let options = ParseOptions::new().remove_blank_text(false);
    let mut failures = Vec::new();
    for (index, (case, want)) in cases.iter().zip(&expected).enumerate() {
        let doc = Document::parse_with(case, &options);
        let Some(element) = doc.root_element() else {
            continue;
        };
        let got = to_markdown_default(element);
        if got != *want {
            failures.push(format!(
                "--- fuzz case {index} ---\nhtml:\n{case}\nexpected:\n{want:?}\nactual:\n{got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fuzz cases differ\n\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}
