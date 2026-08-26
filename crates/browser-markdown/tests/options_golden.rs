//! The option matrix, checked against markdownify.
//!
//! The reference implementation only ever uses the defaults, but [`MarkdownOptions`] exposes the
//! whole markdownify surface and every field of it is a real code path. This
//! runs each non-default option set over every normalized fixture and compares
//! with `markdownify(html, **kwargs)`.
//!
//! The list below must stay in the same order as `OPTION_SETS` in
//! `tools/gen_golden.py`; the test asserts the record count so a drift is loud.
//!
//! `wrap` is included: `wrap_line` is a real port of `textwrap.fill` with
//! `break_long_words=False, break_on_hyphens=False`, down to keeping whitespace
//! chunks verbatim and applying `replace_whitespace`.

use std::fs;
use std::path::PathBuf;

use browser_dom::Document;
use browser_markdown::{
    to_markdown, DocumentStrip, HeadingStyle, MarkdownOptions, NewlineStyle, PreStrip,
};

/// Record separator, shared with the generator scripts.
const SEPARATOR: char = '\u{1e}';

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

fn option_sets() -> Vec<(&'static str, MarkdownOptions)> {
    let base = MarkdownOptions::default;
    vec![
        (
            "atx",
            MarkdownOptions {
                heading_style: HeadingStyle::Atx,
                ..base()
            },
        ),
        (
            "atx_closed",
            MarkdownOptions {
                heading_style: HeadingStyle::AtxClosed,
                ..base()
            },
        ),
        (
            "backslash",
            MarkdownOptions {
                newline_style: NewlineStyle::Backslash,
                ..base()
            },
        ),
        (
            "escape_misc",
            MarkdownOptions {
                escape_misc: true,
                ..base()
            },
        ),
        (
            "no_escape",
            MarkdownOptions {
                escape_asterisks: false,
                escape_underscores: false,
                ..base()
            },
        ),
        (
            "strip_pre_one",
            MarkdownOptions {
                strip_pre: Some(PreStrip::StripOne),
                ..base()
            },
        ),
        (
            "strip_pre_none",
            MarkdownOptions {
                strip_pre: None,
                ..base()
            },
        ),
        (
            "strip_doc_lstrip",
            MarkdownOptions {
                strip_document: Some(DocumentStrip::Lstrip),
                ..base()
            },
        ),
        (
            "strip_doc_rstrip",
            MarkdownOptions {
                strip_document: Some(DocumentStrip::Rstrip),
                ..base()
            },
        ),
        (
            "strip_doc_none",
            MarkdownOptions {
                strip_document: None,
                ..base()
            },
        ),
        (
            "infer_header",
            MarkdownOptions {
                table_infer_header: true,
                ..base()
            },
        ),
        (
            "one_bullet",
            MarkdownOptions {
                bullets: "-".to_owned(),
                ..base()
            },
        ),
        (
            "underscore_em",
            MarkdownOptions {
                strong_em_symbol: '_',
                ..base()
            },
        ),
        (
            "sub_sup",
            MarkdownOptions {
                sub_symbol: "~".to_owned(),
                sup_symbol: "^".to_owned(),
                ..base()
            },
        ),
        (
            "keep_images",
            MarkdownOptions {
                keep_inline_images_in: vec!["td".to_owned(), "th".to_owned()],
                ..base()
            },
        ),
        (
            "default_title",
            MarkdownOptions {
                default_title: true,
                ..base()
            },
        ),
        (
            "no_autolinks",
            MarkdownOptions {
                autolinks: false,
                ..base()
            },
        ),
        (
            "code_language",
            MarkdownOptions {
                code_language: "rust".to_owned(),
                ..base()
            },
        ),
        (
            "strip_tags",
            MarkdownOptions {
                strip: Some(vec!["a".to_owned(), "b".to_owned(), "table".to_owned()]),
                ..base()
            },
        ),
        (
            "convert_tags",
            MarkdownOptions {
                convert: Some(vec![
                    "p".to_owned(),
                    "h1".to_owned(),
                    "h2".to_owned(),
                    "li".to_owned(),
                    "ul".to_owned(),
                ]),
                ..base()
            },
        ),
        (
            "wrap_20",
            MarkdownOptions {
                wrap: true,
                wrap_width: Some(20),
                ..base()
            },
        ),
        (
            "wrap_none",
            MarkdownOptions {
                wrap: true,
                wrap_width: None,
                ..base()
            },
        ),
    ]
}

#[test]
fn matches_markdownify_across_the_option_matrix() {
    let root = testdata();
    let golden = fs::read_to_string(root.join("options").join("golden.txt"))
        .expect("options/golden.txt — run tools/gen_golden.py");
    let expected: Vec<&str> = golden.split(SEPARATOR).collect();

    let mut inputs: Vec<PathBuf> = fs::read_dir(root.join("normalized"))
        .expect("normalized fixtures")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .collect();
    inputs.sort();

    let sets = option_sets();
    assert_eq!(
        expected.len(),
        sets.len() * inputs.len(),
        "the option matrix and its golden are out of sync"
    );

    let mut record = 0;
    let mut failures = Vec::new();
    for (name, opts) in &sets {
        for path in &inputs {
            let html = fs::read_to_string(path).expect("fixture is readable");
            let doc = Document::parse(&html);
            let element = doc.root_element().expect("every fixture has an <html>");
            let got = to_markdown(element, opts);
            let want = expected.get(record).copied().unwrap_or_default();
            if got != want {
                failures.push(format!(
                    "--- {name} / {} ---\nexpected:\n{want:?}\nactual:\n{got:?}",
                    path.display()
                ));
            }
            record += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {record} records differ\n\n{}",
        failures.len(),
        failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}
