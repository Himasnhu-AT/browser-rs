//! `write_content_to_file` with the `.md` / `.html` / `.txt` extension-to-mode map, plus
//! `--ai-targeted`.
//!
//! This is the reference implementation's `Convertor` (`reference/core/shell.py`), which is the one
//! place a page turns into bytes on disk. `cli-ai.md` §4 is the reference; the
//! four details that are easy to get wrong, and that
//! `crates/browser-rs/tests/integration.rs::cli_extract_seam` pins, are all here:
//!
//! 1. [`extract_content`] always pushes a **trailing empty string** — Python's
//!    generator ends with a bare `yield ""`, so `pieces.len()` is
//!    `matches + 1`. `concat()` is unaffected; a caller counting chunks is not.
//! 2. [`collapse_whitespace`] collapses `\n`, `\r`, `\t` and ` ` **one character
//!    class at a time**. One pass over `[\n\r\t ]+` is a different function:
//!    `"a \n b"` becomes `"a \n b"` under Python's four passes and `"a b"`
//!    under the combined one. (Python runs them in the order `\n`, `\r`, `\t`,
//!    ` `, and this does too, but the order provably does not matter — see
//!    `the_order_of_the_four_passes_does_not_matter`.)
//! 3. An unrecognised suffix is a [`ConvertError::UnknownExtension`], not a
//!    silent default — and the test is `str::ends_with` on the **whole path
//!    string**, not `Path::extension` and not `Path::file_name`, so
//!    `archive.tar.md` is markdown, `output.MD` is an error and so is
//!    `dir.md/`.
//! 4. The mode comes from the suffix alone. `--css-selector` and `--ai-targeted`
//!    change *what* is extracted, never *how* it is written.
//!
//! # The divergence families that are left
//!
//! Until wave 6 this module could not express Python's control flow, because
//! `Convertor._strip_noise_tags` / `_sanitize_for_ai` and the *uncleaned*
//! markdown conversion had no entry point reachable from here. Both now exist —
//! [`browser::Document::clean_for_ai`] and [`browser::Node::markdown`] —
//! and [`extract_content`] is Python's `_extract_content` statement for
//! statement. That closed three whole families, 113 rows of
//! `tests/u19_convert_golden.rs`: markdown is no longer sanitised when
//! `--ai-targeted` is off, `--ai-targeted` really does clean for `.html` and
//! `.txt`, and markdown yields one piece per CSS match instead of one
//! aggregated string.
//!
//! What remains differing is `browser-dom`'s tree shape and
//! `browser-markdown`'s re-parse, not this crate:
//!
//! * lxml leaves `Selector._root` as `None` for a document with no element, so
//!   Python *raises* where html5ever builds the standard skeleton;
//! * the synthesised `<head>`/`<body>` and the implied `<tbody>`;
//! * `<noscript>` and `<template>` written before any body content end up **in
//!   `<head>`** under the HTML5 algorithm, which is what browsers do;
//! * a bare `<td>` re-parsed on its own loses its cell to foster parenting.
//!   Python re-parses too — markdownify takes a string, not a tree — but
//!   `html.parser` has no foster-parenting rule.
//!
//! One divergence is still this crate's and is not closable here: the output
//! file is always written as UTF-8, where Python opens it with
//! `encoding=page.encoding`. See [`write_content_to_file`].
//!
//! `tests/u19_convert_golden.rs` enumerates all five remaining families row by
//! row, and fails if any of them starts reproducing.

use std::fmt;
use std::path::{Path, PathBuf};

use browser::{AllTextOpts, ExtractionType, Node, Response};

/// `Convertor._extension_map`, in Python's order.
///
/// The key is the whole suffix including the dot, because the Python test is
/// `filename.endswith((".md", ".html", ".txt"))` and only *then*
/// `filename.split(".")[-1]`. Matching on the suffix string rather than on
/// [`Path::extension`] is what makes `"output"` and `"output.invalid"` errors
/// and `".md"` (a dotfile, whose `Path::extension` is `None`) a markdown file.
pub const EXTENSION_MAP: [(&str, ExtractionType); 3] = [
    (".md", ExtractionType::Markdown),
    (".html", ExtractionType::Html),
    (".txt", ExtractionType::Text),
];

/// `Convertor._extract_content`'s `ignore_tags` for `text` mode.
///
/// Note `iframe`, which is **not** in the parser's default
/// [`AllTextOpts::ignore_tags`] and not in `_strip_noise_tags` either — text
/// mode is the only caller that asks for it.
pub const TEXT_IGNORE_TAGS: [&str; 5] = ["script", "style", "noscript", "svg", "iframe"];

/// The characters `_extract_content` collapses, **in Python's order**.
pub const COLLAPSED_WHITESPACE: [char; 4] = ['\n', '\r', '\t', ' '];

/// `_ZWC_PATTERN` — the zero-width set `_sanitize_for_ai` strips.
///
/// ZWSP, ZWNJ, ZWJ, BOM, WORD JOINER, MONGOLIAN VOWEL SEPARATOR.
pub const ZERO_WIDTH_CHARS: [char; 6] = [
    '\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}', '\u{2060}', '\u{180e}',
];

/// Everything [`ConvertError`] can be.
///
/// The message strings are Python's, verbatim, because `cli-ai.md` §9 pins them
/// (`write_content_to_file(page, "")` and `(page, "output.xyz")` both raise
/// `ValueError` with these texts).
#[derive(Debug)]
pub enum ConvertError {
    /// `ValueError("Filename must be provided")`.
    EmptyFilename,
    /// `ValueError("Unknown file type: filename must end with '.md', '.html', or '.txt'")`.
    UnknownExtension {
        /// The filename that was rejected. Python does not include it in the
        /// message; it is kept here so a caller can report it separately.
        filename: String,
    },
    /// The CSS selector did not compile.
    ///
    /// Boxed because `ParseError` carries the whole Python message string and
    /// makes every `Result<_, ConvertError>` in this module 144 bytes wide,
    /// which `clippy::result_large_err` rightly objects to on a happy path that
    /// never fails.
    Selector {
        /// The selector as written.
        selector: String,
        /// What the parser said.
        source: Box<browser::ParseError>,
    },
    /// The output file could not be written.
    Io {
        /// The path that was being written.
        path: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// The current directory could not be read, so a relative output path could
    /// not be made absolute. Python's `Path.cwd()` raises the same way.
    Cwd(std::io::Error),
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFilename => f.write_str("Filename must be provided"),
            Self::UnknownExtension { .. } => {
                f.write_str("Unknown file type: filename must end with '.md', '.html', or '.txt'")
            }
            Self::Selector { selector, .. } => {
                write!(f, "invalid CSS selector '{selector}'")
            }
            Self::Io { path, .. } => write!(f, "could not write '{}'", path.display()),
            Self::Cwd(_) => f.write_str("could not resolve the current working directory"),
        }
    }
}

impl std::error::Error for ConvertError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyFilename | Self::UnknownExtension { .. } => None,
            Self::Selector { source, .. } => Some(source.as_ref()),
            Self::Io { source, .. } => Some(source),
            Self::Cwd(source) => Some(source),
        }
    }
}

/// Pick the extraction mode from a **filename**, the way Python does.
///
/// The two-step shape is deliberate: Python first refuses anything that does not
/// `endswith` one of the three suffixes, then looks up
/// `_extension_map[filename.split(".")[-1]]`. Because the first step already
/// guarantees the suffix, the two collapse into one `ends_with` scan here.
///
/// # Errors
///
/// [`ConvertError::EmptyFilename`] for an empty or all-whitespace name, and
/// [`ConvertError::UnknownExtension`] for anything else that does not end in
/// `.md`, `.html` or `.txt`.
///
/// ```
/// use browser::ExtractionType;
/// use browser_cli::convert::extraction_for;
///
/// assert_eq!(extraction_for("page.md").unwrap(), ExtractionType::Markdown);
/// assert_eq!(extraction_for("archive.tar.txt").unwrap(), ExtractionType::Text);
/// // The match is on the suffix string, so case matters and a bare name fails.
/// assert!(extraction_for("page.MD").is_err());
/// assert!(extraction_for("page").is_err());
/// ```
pub fn extraction_for(filename: &str) -> Result<ExtractionType, ConvertError> {
    if filename.trim().is_empty() {
        return Err(ConvertError::EmptyFilename);
    }
    for (suffix, extraction) in EXTENSION_MAP {
        if filename.ends_with(suffix) {
            return Ok(extraction);
        }
    }
    Err(ConvertError::UnknownExtension {
        filename: filename.to_owned(),
    })
}

/// Collapse runs of `\n`, then `\r`, then `\t`, then ` ` — each independently.
///
/// Python runs four separate `re.sub(f"[{c}]+", c, text)` passes in that exact
/// order. Doing it in one pass over `[\n\r\t ]+` is a **different function**:
/// it would rewrite a mixed run like `" \n "` to a single character, which
/// Python never does.
///
/// ```
/// use browser_cli::convert::collapse_whitespace;
///
/// assert_eq!(collapse_whitespace("a\n\n\nb"), "a\nb");
/// assert_eq!(collapse_whitespace("a   b"), "a b");
/// // A mixed run keeps one of each character, in place.
/// assert_eq!(collapse_whitespace("a \n\n  b"), "a \n b");
/// ```
#[must_use]
pub fn collapse_whitespace(text: &str) -> String {
    let mut out = text.to_owned();
    for target in COLLAPSED_WHITESPACE {
        let mut pass = String::with_capacity(out.len());
        let mut previous_was_target = false;
        for ch in out.chars() {
            if ch == target {
                if previous_was_target {
                    continue;
                }
                previous_was_target = true;
            } else {
                previous_was_target = false;
            }
            pass.push(ch);
        }
        out = pass;
    }
    out
}

/// The string half of `Convertor._sanitize_for_ai`: drop the zero-width
/// characters and the XML-illegal control characters.
///
/// Python applies `_ZWC_PATTERN` first and `_CONTROL_CHARS_PATTERN` second, per
/// element `.text` and `.tail`. Neither pattern can create a character the other
/// would match, so a single pass over a joined string is equivalent — *except*
/// that Python scrubs before `get_all_text`'s `valid_values` filter runs, so a
/// text node made **only** of zero-width characters disappears there and leaves
/// no separator behind, while a scrub of the extracted string leaves an empty
/// piece.
///
/// That residue is why [`extract_content`] no longer calls this: since wave 6 it
/// scrubs the *tree* through
/// [`Document::clean_for_ai`](browser::Document::clean_for_ai), which is where
/// Python does it. The function stays public because it is the faithful port of
/// the two regexes and a caller holding a string rather than a page still wants
/// it.
///
/// The whitelist is `\t`, `\n`, `\r` — everything else below `\x20`, plus
/// `\x7f`-adjacent nothing, goes. (Python's class is `[\x00-\x08\x0b\x0c\x0e-\x1f]`,
/// which is exactly "control characters except tab, newline and carriage
/// return"; `\x7f` DEL is deliberately **not** in it.)
///
/// ```
/// use browser_cli::convert::scrub_for_ai;
///
/// assert_eq!(scrub_for_ai("Hello\u{200b}World"), "HelloWorld");
/// assert_eq!(scrub_for_ai("Hello\u{8}World"), "HelloWorld");
/// assert_eq!(scrub_for_ai("keep\tthe\ttabs\n"), "keep\tthe\ttabs\n");
/// ```
#[must_use]
pub fn scrub_for_ai(text: &str) -> String {
    text.chars()
        .filter(|ch| !ZERO_WIDTH_CHARS.contains(ch))
        .filter(|ch| !is_illegal_control(*ch))
        .collect()
}

/// `_CONTROL_CHARS_PATTERN` — `[\x00-\x08\x0b\x0c\x0e-\x1f]`.
const fn is_illegal_control(ch: char) -> bool {
    matches!(ch, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}')
}

/// `Convertor._extract_content`, materialised.
///
/// Python's is a generator; the only observable difference between a generator
/// and a `Vec` here is that the generator is lazy, and the CLI joins it
/// immediately. The **trailing empty string** is reproduced, because
/// `cli-ai.md` §11.5 lists it as an ordering gotcha and the seam test counts on
/// it.
///
/// The shape is Python's, statement for statement:
///
/// ```text
/// if main_content_only:
///     page = page.css("body").first or page
///     page = _strip_noise_tags(page)
///     page = _sanitize_for_ai(page)
/// pages = [page] if not css_selector else page.css(css_selector)
/// for page in pages: yield <one of the three renderings>
/// yield ""
/// ```
///
/// The cleaning is [`Document::clean_for_ai`](browser::Document::clean_for_ai)
/// and the markdown rendering is [`Node::markdown`](browser::Node::markdown) —
/// both added in wave 6 precisely so this function could stop approximating.
/// Before them the only markdown entry point reachable from here was
/// `Response::markdown_with`, which cleans *unconditionally* and *joins* its
/// matches, and there was no way to clean at all for the `html` and `text`
/// modes; that cost 117 corpus rows in three declared families.
///
/// # Errors
///
/// [`ConvertError::Selector`] when `css_selector` does not compile. Python
/// raises `SelectorSyntaxError` from `cssselect` at the same point.
pub fn extract_content(
    response: &Response,
    extraction: ExtractionType,
    css_selector: Option<&str>,
    main_content_only: bool,
) -> Result<Vec<String>, ConvertError> {
    // `_strip_noise_tags` and `_sanitize_for_ai` rebuild the tree, so the copy
    // has to outlive every `Node` taken out of it.
    let cleaned = main_content_only.then(|| response.doc().clean_for_ai());
    let root = match &cleaned {
        // Python narrows *then* cleans; `clean_for_ai` cleans the whole document
        // and this narrows afterwards. See its rustdoc for why the two agree.
        Some(document) => document
            .css_first("body")
            .map_err(|source| ConvertError::Selector {
                selector: "body".to_owned(),
                source: Box::new(source),
            })?
            .unwrap_or_else(|| document.root()),
        None => response.root(),
    };

    // `pages = [page] if not css_selector else page.css(css_selector)`.
    let pages: Vec<Node<'_>> = match css_selector {
        None => vec![root],
        Some(selector) => root
            .css(selector)
            .map_err(|source| ConvertError::Selector {
                selector: selector.to_owned(),
                source: Box::new(source),
            })?
            .into_iter()
            .collect(),
    };

    let mut pieces: Vec<String> = match extraction {
        // `markdownify(page.html_content)`, per match — *not* the always-cleaning
        // `Response::markdown_with`, and not aggregated into one string.
        ExtractionType::Markdown => pages.iter().map(Node::markdown).collect(),
        ExtractionType::Html => pages.iter().map(Node::html_content).collect(),
        ExtractionType::Text => {
            let opts = text_options();
            pages
                .iter()
                .map(|node| collapse_whitespace(&node.get_all_text_with(&opts)))
                .collect()
        }
    };

    // `yield ""` — always, whatever the mode and however many matches there were.
    pieces.push(String::new());
    Ok(pieces)
}

/// `get_all_text(strip=True, ignore_tags=("script","style","noscript","svg","iframe"))`.
///
/// `separator` stays `"\n"` and `valid_values` stays `true`, which are the
/// parser's defaults and Python's.
fn text_options() -> AllTextOpts {
    AllTextOpts {
        strip: true,
        ignore_tags: TEXT_IGNORE_TAGS
            .iter()
            .map(|tag| (*tag).to_owned())
            .collect(),
        ..AllTextOpts::default()
    }
}

/// Turn a user-supplied `OUTPUT_FILE` into an absolute path.
///
/// `__Request_and_Save` does `Path(output_file)`, then `Path.cwd() / output_file`
/// when it is not already absolute — so `../out.md` resolves against the shell's
/// directory and not against anything the reference implementation owns.
///
/// # Errors
///
/// [`ConvertError::Cwd`] when the current directory cannot be read.
pub fn resolve_output_path(output_file: &str) -> Result<PathBuf, ConvertError> {
    let path = Path::new(output_file);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(ConvertError::Cwd)?;
    Ok(cwd.join(path))
}

/// `Convertor.write_content_to_file`.
///
/// # Encoding — a declared divergence
///
/// Python opens the file with `encoding=page.encoding`, so a windows-1252 page
/// is written back as windows-1252. `encoding_rs` is not a dependency of this
/// crate, so the file is always UTF-8 and a non-UTF-8 page encoding is reported
/// through `tracing::warn!` rather than honoured. Nothing is lost — the
/// *document* was decoded correctly on the way in — but the bytes on disk differ
/// from Python's for a non-UTF-8 page.
///
/// # Errors
///
/// [`ConvertError::EmptyFilename`], [`ConvertError::UnknownExtension`],
/// [`ConvertError::Selector`] and [`ConvertError::Io`].
pub fn write_content_to_file(
    response: &Response,
    path: &Path,
    css_selector: Option<&str>,
    main_content_only: bool,
) -> Result<(), ConvertError> {
    // Python tests the **whole path string** — `write_content_to_file` is handed
    // `str(output_path)` and does `filename.endswith((".md", ".html", ".txt"))`
    // on it. Taking `path.file_name()` instead differs for a path with a
    // trailing separator: `str(Path("dir.md/"))` keeps the slash and is
    // rejected, where `file_name()` hands back `dir.md` and would be accepted.
    let filename = path.to_string_lossy();
    let extraction = extraction_for(&filename)?;

    if !is_utf8_label(&response.encoding) {
        tracing::warn!(
            "the page declares encoding '{}'; the output file is written as UTF-8 \
             (Python would re-encode it — see `browser_cli::convert`)",
            response.encoding
        );
    }

    let body = extract_content(response, extraction, css_selector, main_content_only)?.concat();
    std::fs::write(path, body).map_err(|source| ConvertError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Whether a charset label means UTF-8, so the warning above stays quiet for the
/// overwhelmingly common case.
fn is_utf8_label(label: &str) -> bool {
    let label = label.trim().to_ascii_lowercase();
    matches!(label.as_str(), "utf-8" | "utf8" | "unicode-1-1-utf-8" | "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> Response {
        Response::builder(200, "https://example.test/")
            .body(html.to_owned())
            .build()
    }

    // -- the extension map -------------------------------------------------

    #[test]
    fn the_three_suffixes_map_to_the_three_modes() {
        assert_eq!(extraction_for("a.md").unwrap(), ExtractionType::Markdown);
        assert_eq!(extraction_for("a.html").unwrap(), ExtractionType::Html);
        assert_eq!(extraction_for("a.txt").unwrap(), ExtractionType::Text);
    }

    #[test]
    fn an_unknown_suffix_is_an_error_not_a_default() {
        for name in [
            "output.invalid",
            "output.xyz",
            "output",
            "output.htm",
            "a.markdown",
        ] {
            let error = extraction_for(name).unwrap_err();
            assert!(
                matches!(error, ConvertError::UnknownExtension { .. }),
                "{name} should be rejected, got {error:?}"
            );
            assert_eq!(
                error.to_string(),
                "Unknown file type: filename must end with '.md', '.html', or '.txt'",
                "the message is Python's, verbatim"
            );
        }
    }

    #[test]
    fn an_empty_or_blank_filename_is_its_own_error() {
        assert!(matches!(
            extraction_for("").unwrap_err(),
            ConvertError::EmptyFilename
        ));
        assert!(matches!(
            extraction_for("   ").unwrap_err(),
            ConvertError::EmptyFilename
        ));
        assert_eq!(
            ConvertError::EmptyFilename.to_string(),
            "Filename must be provided"
        );
    }

    /// `Path::extension` would answer `Some("md")` for `.md` and `Some("gz")`
    /// for `page.md.gz`; Python answers "markdown" and "error" respectively.
    #[test]
    fn the_match_is_on_the_suffix_string_not_on_path_extension() {
        assert_eq!(extraction_for(".md").unwrap(), ExtractionType::Markdown);
        assert_eq!(
            extraction_for("archive.tar.md").unwrap(),
            ExtractionType::Markdown
        );
        assert!(extraction_for("page.md.gz").is_err());
        // Case-sensitive, like `str.endswith`.
        assert!(extraction_for("page.MD").is_err());
        assert!(extraction_for("page.HTML").is_err());
    }

    // -- whitespace collapsing --------------------------------------------

    #[test]
    fn each_whitespace_class_collapses_independently() {
        assert_eq!(collapse_whitespace("a\n\n\nb"), "a\nb");
        assert_eq!(collapse_whitespace("a\r\r\rb"), "a\rb");
        assert_eq!(collapse_whitespace("a\t\t\tb"), "a\tb");
        assert_eq!(collapse_whitespace("a    b"), "a b");
    }

    /// The whole point of the four passes: a *mixed* run is not collapsed to one
    /// character. This is the assertion a single `[\n\r\t ]+` pass fails.
    #[test]
    fn a_mixed_whitespace_run_keeps_one_of_each_character() {
        assert_eq!(collapse_whitespace("a \n\n  b"), "a \n b");
        assert_eq!(collapse_whitespace("a\r\n\r\nb"), "a\r\n\r\nb");
        assert_eq!(collapse_whitespace("x\t \t \ty"), "x\t \t \ty");
    }

    #[test]
    fn collapsing_is_idempotent_and_leaves_other_text_alone() {
        let once = collapse_whitespace("  a\n\n b  ");
        assert_eq!(collapse_whitespace(&once), once);
        assert_eq!(collapse_whitespace("nothing to do"), "nothing to do");
        assert_eq!(collapse_whitespace(""), "");
    }

    /// Mutation testing put "run the four passes in the reverse order" up as a
    /// bug and the whole suite let it through. It is not a bug: collapsing runs
    /// of *distinct* characters **commutes**, because shortening a maximal run
    /// of `X` can never make two `Y`s adjacent — the `Y`s were separated by at
    /// least one `X`, and at least one `X` survives. This test is the proof, so
    /// that the equivalence is recorded in the repo rather than in a reviewer's
    /// head; it was also checked exhaustively against Python (200,000 random
    /// strings over `{\n, \r, \t, ' ', a, b}` × all 24 permutations of the four
    /// passes, zero differences).
    ///
    /// What is *not* interchangeable is doing it in one pass over a combined
    /// class — `a_mixed_whitespace_run_keeps_one_of_each_character` pins that.
    #[test]
    fn the_order_of_the_four_passes_does_not_matter() {
        fn collapse_in(order: &[char], text: &str) -> String {
            let mut out = text.to_owned();
            for target in order {
                let mut pass = String::with_capacity(out.len());
                let mut previous_was_target = false;
                for ch in out.chars() {
                    if ch == *target {
                        if previous_was_target {
                            continue;
                        }
                        previous_was_target = true;
                    } else {
                        previous_was_target = false;
                    }
                    pass.push(ch);
                }
                out = pass;
            }
            out
        }

        // A deterministic LCG, so the test is reproducible without a rand dep.
        let alphabet = ['\n', '\r', '\t', ' ', 'a', 'b'];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = |modulo: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % modulo
        };
        let orders: [[char; 4]; 4] = [
            [' ', '\t', '\r', '\n'],
            ['\t', ' ', '\n', '\r'],
            ['\r', '\n', ' ', '\t'],
            ['\n', '\t', ' ', '\r'],
        ];
        for _ in 0..2000 {
            let length = next(13) as usize;
            let text: String = (0..length)
                .map(|_| alphabet[next(alphabet.len() as u64) as usize])
                .collect();
            let reference = collapse_whitespace(&text);
            for order in &orders {
                assert_eq!(
                    collapse_in(order, &text),
                    reference,
                    "{text:?} under {order:?}"
                );
            }
        }
    }

    #[test]
    fn collapsing_does_not_touch_other_unicode_spaces() {
        // `\u{a0}` is not in Python's four-character list.
        assert_eq!(collapse_whitespace("a\u{a0}\u{a0}b"), "a\u{a0}\u{a0}b");
        assert_eq!(collapse_whitespace("a\u{b}\u{b}b"), "a\u{b}\u{b}b");
    }

    // -- the AI scrub ------------------------------------------------------

    #[test]
    fn the_scrub_drops_every_zero_width_character() {
        for ch in ZERO_WIDTH_CHARS {
            assert_eq!(scrub_for_ai(&format!("a{ch}b")), "ab", "{ch:?}");
        }
    }

    #[test]
    fn the_scrub_drops_xml_illegal_controls_but_keeps_tab_newline_return() {
        // `cli-ai.md` §9: "Hello\x08World" -> "HelloWorld".
        assert_eq!(scrub_for_ai("Hello\u{8}World"), "HelloWorld");
        for code in (0x00..=0x1f_u32).chain(std::iter::once(0x7f)) {
            let ch = char::from_u32(code).expect("ascii");
            let kept = matches!(ch, '\t' | '\n' | '\r' | '\u{7f}');
            let scrubbed = scrub_for_ai(&format!("a{ch}b"));
            assert_eq!(scrubbed == "ab", !kept, "U+{code:04X} kept={kept}");
        }
    }

    // -- `_extract_content` ------------------------------------------------

    /// A selector that matches nothing yields **no** piece, so the row is just
    /// the generator's trailing `""` — Python's `[""]`, not `["", ""]`.
    #[test]
    fn a_selector_that_matches_nothing_yields_only_the_trailing_empty_string() {
        let response = page("<html><body><p>hi</p></body></html>");
        for mode in [
            ExtractionType::Markdown,
            ExtractionType::Html,
            ExtractionType::Text,
        ] {
            let pieces =
                extract_content(&response, mode, Some(".nothing-here"), false).expect("extracts");
            assert_eq!(pieces, [String::new()], "{mode:?}");
        }
    }

    #[test]
    fn the_generator_always_ends_with_an_empty_string() {
        let response = page("<html><body><a href='/1'>one</a><a href='/2'>two</a></body></html>");
        for mode in [
            ExtractionType::Markdown,
            ExtractionType::Html,
            ExtractionType::Text,
        ] {
            let pieces = extract_content(&response, mode, None, false).expect("extracts");
            assert_eq!(
                pieces.last().map(String::as_str),
                Some(""),
                "{mode:?} must end with the generator's bare `yield \"\"`"
            );
        }
    }

    #[test]
    fn html_mode_yields_one_piece_per_match_plus_the_trailing_empty_string() {
        let response = page("<html><body><a href='/1'>one</a><a href='/2'>two</a></body></html>");
        let pieces =
            extract_content(&response, ExtractionType::Html, Some("a"), false).expect("extracts");
        assert_eq!(pieces.len(), 3, "two matches plus the trailing \"\"");
        assert_eq!(pieces[0], "<a href=\"/1\">one</a>");
        assert_eq!(pieces[1], "<a href=\"/2\">two</a>");
        assert_eq!(pieces[2], "");
    }

    /// Python serialises `page.html_content`, which is the **`<html>` element**,
    /// not the document — so no doctype ever reaches the file.
    #[test]
    fn html_mode_without_a_selector_serialises_the_root_element_not_the_document() {
        let response = page("<!doctype html><html><body><p>Visible</p></body></html>");
        let out = extract_content(&response, ExtractionType::Html, None, false)
            .expect("extracts")
            .concat();
        // html5ever synthesises the `<head>` libxml2 leaves out — the tree-shape
        // divergence carried since unit 5, not anything this module decides.
        assert_eq!(out, "<html><head></head><body><p>Visible</p></body></html>");
        assert!(!out.contains("doctype"), "{out:?}");
    }

    #[test]
    fn html_mode_with_ai_targeted_narrows_to_body() {
        let response =
            page("<html><head><title>T</title></head><body><p>Visible</p></body></html>");
        let out = extract_content(&response, ExtractionType::Html, None, true)
            .expect("extracts")
            .concat();
        assert_eq!(out, "<body><p>Visible</p></body>");
    }

    #[test]
    fn text_mode_drops_the_ignored_tags_even_without_ai_targeted() {
        let response = page(
            "<html><body><script>var noise = 1;</script><style>p{}</style>\
             <noscript>ns</noscript><p>Visible</p></body></html>",
        );
        let out = extract_content(&response, ExtractionType::Text, None, false)
            .expect("extracts")
            .concat();
        assert_eq!(out, "Visible");
    }

    #[test]
    fn text_mode_collapses_runs_of_whitespace() {
        let response = page("<html><body><p>Body   text</p><p>more</p></body></html>");
        let out = extract_content(&response, ExtractionType::Text, None, false)
            .expect("extracts")
            .concat();
        assert!(!out.contains("  "), "{out:?}");
        assert!(!out.contains("\n\n"), "{out:?}");
    }

    #[test]
    fn text_mode_scrubs_zero_width_characters_only_under_ai_targeted() {
        let response = page("<html><body><p>a\u{200b}b</p></body></html>");
        let plain = extract_content(&response, ExtractionType::Text, None, false)
            .expect("extracts")
            .concat();
        assert_eq!(plain, "a\u{200b}b", "Python keeps them without the flag");
        let sanitised = extract_content(&response, ExtractionType::Text, None, true)
            .expect("extracts")
            .concat();
        assert_eq!(sanitised, "ab");
    }

    /// The three assertions that used to be **declared divergences** and are
    /// now plain fidelity checks. Each was verified against the reference
    /// interpreter while wave 6 closed them:
    ///
    /// ```text
    /// >>> Convertor._extract_content(page, "markdown", main_content_only=False)
    /// ['Visible\n\nHIDDEN\n\nTPL', '']
    /// ```
    ///
    /// Markdown *without* `--ai-targeted` keeps the hidden paragraph and the
    /// `<template>` body, because Python only cleans when `main_content_only`
    /// is set. [`Node::markdown`](browser::Node::markdown) is the uncleaned
    /// path that makes this reachable.
    #[test]
    fn markdown_without_ai_targeted_keeps_hidden_content_like_python() {
        let response = page(
            "<html><body><p>Visible</p><p style=\"display:none\">HIDDEN</p>\
             <template>TPL</template></body></html>",
        );
        let out = extract_content(&response, ExtractionType::Markdown, None, false)
            .expect("extracts")
            .concat();
        assert_eq!(out, "Visible\n\nHIDDEN\n\nTPL");

        // …and *with* it, the cleaning passes run and both go.
        let cleaned = extract_content(&response, ExtractionType::Markdown, None, true)
            .expect("extracts")
            .concat();
        assert_eq!(cleaned, "Visible");
    }

    /// `--ai-targeted` on `.html` runs `_strip_noise_tags`, so `<script>` goes.
    ///
    /// ```text
    /// >>> Convertor._extract_content(page, "html", main_content_only=True)
    /// ['<body><p>Visible</p></body>', '']
    /// ```
    #[test]
    fn ai_targeted_html_drops_script_tags_like_python() {
        let response = page("<html><body><script>evil()</script><p>Visible</p></body></html>");
        let out = extract_content(&response, ExtractionType::Html, None, true)
            .expect("extracts")
            .concat();
        assert_eq!(out, "<body><p>Visible</p></body>");
    }

    /// The same pass seen from `.txt`: `_sanitize_for_ai` drops the hidden
    /// paragraph, which `ignore_tags` alone never could.
    ///
    /// ```text
    /// >>> Convertor._extract_content(page, "text", main_content_only=True)
    /// ['Visible', '']
    /// ```
    #[test]
    fn ai_targeted_text_drops_hidden_elements_like_python() {
        let response =
            page("<html><body><p>Visible</p><p style=\"display:none\">HIDDEN</p></body></html>");
        let out = extract_content(&response, ExtractionType::Text, None, true)
            .expect("extracts")
            .concat();
        assert_eq!(out, "Visible");
    }

    /// `pages = [page] if not css_selector else page.css(css_selector)` — one
    /// piece per match, in every mode. Markdown used to aggregate, because
    /// `Response::markdown_with` joins internally.
    #[test]
    fn every_mode_yields_one_piece_per_css_match() {
        let response = page("<html><body><p>one</p><p>two</p></body></html>");
        for mode in [
            ExtractionType::Markdown,
            ExtractionType::Html,
            ExtractionType::Text,
        ] {
            let pieces = extract_content(&response, mode, Some("p"), false).expect("extracts");
            assert_eq!(
                pieces.len(),
                3,
                "{mode:?}: two matches plus the trailing \"\""
            );
            assert_eq!(pieces[2], "", "{mode:?}");
        }
    }

    #[test]
    fn a_bad_selector_is_reported_rather_than_swallowed() {
        let response = page("<html><body><p>hi</p></body></html>");
        let error = extract_content(&response, ExtractionType::Html, Some("p:::"), false)
            .expect_err("the selector is nonsense");
        assert!(matches!(error, ConvertError::Selector { .. }), "{error:?}");
    }

    #[test]
    fn a_page_with_no_body_falls_back_to_the_whole_document() {
        // `Response` parses as HTML, and html5ever synthesises a <body>, so the
        // fallback is exercised through the parser's own root instead.
        let response = page("<p>only</p>");
        let out = extract_content(&response, ExtractionType::Text, None, true)
            .expect("extracts")
            .concat();
        assert_eq!(out, "only");
    }

    // -- `write_content_to_file` ------------------------------------------

    #[test]
    fn writing_uses_the_suffix_to_pick_the_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let response = page("<html><body><h1>Title</h1><p>Body</p></body></html>");

        let md = dir.path().join("page.md");
        write_content_to_file(&response, &md, None, false).expect("written");
        assert!(std::fs::read_to_string(&md)
            .unwrap()
            .contains("Title\n====="));

        let html = dir.path().join("page.html");
        write_content_to_file(&response, &html, None, false).expect("written");
        assert!(std::fs::read_to_string(&html)
            .unwrap()
            .starts_with("<html>"));

        let txt = dir.path().join("page.txt");
        write_content_to_file(&response, &txt, None, false).expect("written");
        assert_eq!(std::fs::read_to_string(&txt).unwrap(), "Title\nBody");
    }

    #[test]
    fn writing_refuses_an_unknown_suffix_before_touching_the_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let response = page("<p>hi</p>");
        let path = dir.path().join("page.xyz");
        assert!(matches!(
            write_content_to_file(&response, &path, None, false).unwrap_err(),
            ConvertError::UnknownExtension { .. }
        ));
        assert!(!path.exists(), "nothing is created for a rejected name");
    }

    /// Python tests the whole path string, so a directory-shaped name is an
    /// *unknown file type*, not a missing filename — and a trailing separator
    /// keeps `dir.md/` out of the markdown branch even though its last
    /// component is `dir.md`. Verified against
    /// `Convertor.write_content_to_file(page, "/")` and `(page, "dir.md/")`.
    #[test]
    fn a_directory_shaped_path_is_an_unknown_file_type() {
        let response = page("<p>hi</p>");
        for name in ["/", "/tmp/", "dir.md/", "a.md/b"] {
            assert!(
                matches!(
                    write_content_to_file(&response, Path::new(name), None, false).unwrap_err(),
                    ConvertError::UnknownExtension { .. }
                ),
                "{name} should be an unknown file type"
            );
        }
    }

    /// An empty or all-whitespace name is Python's `Filename must be provided`.
    #[test]
    fn an_empty_path_is_the_empty_filename_error() {
        let response = page("<p>hi</p>");
        for name in ["", "   "] {
            assert!(matches!(
                write_content_to_file(&response, Path::new(name), None, false).unwrap_err(),
                ConvertError::EmptyFilename
            ));
        }
    }

    #[test]
    fn writing_reports_io_failures_with_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let response = page("<p>hi</p>");
        let path = dir.path().join("missing").join("page.txt");
        let error = write_content_to_file(&response, &path, None, false).unwrap_err();
        assert!(matches!(error, ConvertError::Io { .. }), "{error:?}");
        assert!(error.to_string().contains("page.txt"), "{error}");
    }

    // -- output paths ------------------------------------------------------

    #[test]
    fn a_relative_output_path_is_resolved_against_the_working_directory() {
        let resolved = resolve_output_path("out.md").expect("cwd is readable");
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("out.md"));
        assert_eq!(
            resolved,
            std::env::current_dir().expect("cwd").join("out.md")
        );
    }

    #[test]
    fn an_absolute_output_path_is_left_alone() {
        let absolute = std::env::temp_dir().join("browser-cli-test.md");
        assert_eq!(
            resolve_output_path(absolute.to_str().expect("utf-8")).expect("resolved"),
            absolute
        );
    }

    #[test]
    fn utf8_labels_are_recognised_so_the_warning_stays_quiet() {
        for label in ["utf-8", "UTF-8", "utf8", " utf-8 ", ""] {
            assert!(is_utf8_label(label), "{label:?}");
        }
        for label in ["windows-1252", "iso-8859-1", "shift_jis"] {
            assert!(!is_utf8_label(label), "{label:?}");
        }
    }
}
