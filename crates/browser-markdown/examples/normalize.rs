//! Regenerate `testdata/normalized/*.html` from `testdata/src/*.html`.
//!
//! The golden Markdown in `testdata/golden` is produced by running the real
//! `markdownify` over the *normalized* files, not the raw ones. That takes the
//! parser out of the comparison: markdownify parses with Python's lenient
//! `html.parser` while we parse with `html5ever`, so feeding Python the HTML
//! our tree serialises to is the only way a byte-for-byte golden test measures
//! the *converter* rather than the two parsers' disagreements.
//!
//! ```text
//! cargo run -p browser-markdown --example normalize
//! python3 crates/browser-markdown/tools/gen_golden.py <python-with-markdownify>
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use browser_dom::{Document, ParseOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let testdata = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let src = testdata.join("src");

    let mut names: Vec<PathBuf> = fs::read_dir(&src)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .collect();
    names.sort();

    // Two corpora. The default one is what the reference implementation's parser flags produce;
    // the `-ws` one keeps whitespace-only text nodes, which is the only way to
    // exercise `_can_ignore` and the lstrip/rstrip rules in `process_text`.
    for (dir, options) in [
        ("normalized", ParseOptions::new()),
        (
            "normalized-ws",
            ParseOptions::new().remove_blank_text(false),
        ),
    ] {
        let out = testdata.join(dir);
        fs::create_dir_all(&out)?;
        for path in &names {
            let raw = fs::read_to_string(path)?;
            let doc = Document::parse_with(&raw, &options);
            let Some(root) = doc.root_element() else {
                eprintln!("{}: no root element", path.display());
                continue;
            };
            let normalized = root.outer_html();

            // The golden test parses the normalized file, so serialisation has
            // to round-trip or the golden would describe a different tree.
            let reparsed = Document::parse_with(&normalized, &options);
            let again = reparsed.root_element().map(|node| node.outer_html());
            assert_eq!(
                again.as_deref(),
                Some(normalized.as_str()),
                "{}: serialisation is not idempotent",
                path.display()
            );

            let name = file_name(path);
            fs::write(out.join(&name), &normalized)?;
            println!("{dir}/{name}: {} bytes", normalized.len());
        }
    }

    normalize_fuzz(&testdata)?;
    Ok(())
}

/// Record separator, shared with `tools/gen_fuzz.py` and `tools/gen_golden.py`.
const SEPARATOR: char = '\u{1e}';

/// Run the generated corpus (`testdata/fuzz/raw.txt`) through the tree.
///
/// The fuzz cases keep whitespace-only text nodes, since that is the harder
/// input for markdownify's whitespace rules.
fn normalize_fuzz(testdata: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let raw_path = testdata.join("fuzz").join("raw.txt");
    let Ok(raw) = fs::read_to_string(&raw_path) else {
        eprintln!(
            "{}: missing, run tools/gen_fuzz.py first",
            raw_path.display()
        );
        return Ok(());
    };
    let options = ParseOptions::new().remove_blank_text(false);
    let mut normalized = Vec::new();
    for case in raw.split(SEPARATOR) {
        let doc = Document::parse_with(case, &options);
        let Some(root) = doc.root_element() else {
            continue;
        };
        normalized.push(root.outer_html());
    }
    let count = normalized.len();
    fs::write(
        testdata.join("fuzz").join("normalized.txt"),
        normalized.join(&SEPARATOR.to_string()),
    )?;
    println!("fuzz/normalized.txt: {count} cases");
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
