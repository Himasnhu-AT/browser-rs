//! The cleaning layer, checked against the reference implementation 0.4.15 itself.
//!
//! The goldens in `testdata/sanitized/` are produced by running
//!
//! ```python
//! clean = Convertor._sanitize_for_ai(Convertor._strip_noise_tags(Selector(html)))
//! ```
//!
//! with the real library and dumping two things: the element names in document
//! order, and the concatenated text with every whitespace character removed.
//!
//! # Why not compare the Markdown, or the HTML
//!
//! Because the reference implementation's pipeline re-serialises the lxml tree and re-parses it
//! with BeautifulSoup, so a byte comparison would be measuring lxml's HTML
//! *serialiser*, not the cleaning rules. lxml writes an empty `<li></li>` as a
//! bare `<li>`, which `html.parser` then nests the following `<li>` inside —
//! `<ul><li></li><li>x</li></ul>` becomes `* * x` in the reference implementation and `* x` here.
//! Whitespace is also removed from the text before comparison. That used to be
//! load-bearing — `remove_blank_text` dropped blanks libxml2 kept — and since
//! `browser-dom` implements `areBlanks()` it no longer is: the inputs in
//! `testdata/normalized/` are minified, so they carry almost no inter-tag
//! whitespace either way. It is kept because the goldens store the stripped
//! form and regenerating them would churn a corpus this test is not about.
//!
//! Both of those are parser/serialiser artefacts. What this test pins is what
//! the cleaning layer is *for*: exactly the right elements are pruned and
//! exactly the right characters are scrubbed.

use std::fs;
use std::path::PathBuf;

use browser_dom::Document;
use browser_markdown::{sanitize_for_ai, strip_noise_tags};

/// Record separator, shared with the generator scripts.
const SEPARATOR: char = '\u{1e}';

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// `element names in document order` + newline + `text without any whitespace`.
fn dump(html: &str) -> String {
    let doc = Document::parse(html);
    let cleaned = sanitize_for_ai(&strip_noise_tags(&doc));
    let Some(root) = cleaned.root_element() else {
        return String::from("\n");
    };
    let tags: Vec<&str> = std::iter::once(root)
        .chain(root.descendant_elements())
        .map(|node| node.tag())
        .collect();
    // Whitespace is dropped on both sides; see the module docs. What must agree
    // exactly is which elements and which *characters* survive.
    let text: String = root
        .text_content()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    format!("{}\n{text}", tags.join(","))
}

#[test]
fn matches_the_reference_convertor_on_the_fixtures() {
    let root = testdata();
    let golden = fs::read_to_string(root.join("sanitized").join("golden-fixtures.txt"))
        .expect("sanitized/golden-fixtures.txt");
    let expected: Vec<&str> = golden.split(SEPARATOR).collect();

    let mut inputs: Vec<PathBuf> = fs::read_dir(root.join("normalized"))
        .expect("normalized fixtures")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .collect();
    inputs.sort();
    assert_eq!(inputs.len(), expected.len(), "corpora are out of sync");

    for (path, want) in inputs.iter().zip(&expected) {
        let html = fs::read_to_string(path).expect("fixture is readable");
        assert_eq!(&dump(&html), want, "{}", path.display());
    }
}

#[test]
fn matches_the_reference_convertor_on_the_generated_corpus() {
    let root = testdata();
    let html = fs::read_to_string(root.join("fuzz").join("normalized.txt")).expect("fuzz corpus");
    let golden = fs::read_to_string(root.join("sanitized").join("golden-fuzz.txt"))
        .expect("sanitized/golden-fuzz.txt");
    let cases: Vec<&str> = html.split(SEPARATOR).collect();
    let expected: Vec<&str> = golden.split(SEPARATOR).collect();
    assert_eq!(cases.len(), expected.len(), "corpora are out of sync");

    let mut failures = Vec::new();
    for (index, (case, want)) in cases.iter().zip(&expected).enumerate() {
        let got = dump(case);
        if got != *want {
            failures.push(format!(
                "--- case {index} ---\nhtml:\n{case}\nexpected:\n{want}\nactual:\n{got}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ\n\n{}",
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
