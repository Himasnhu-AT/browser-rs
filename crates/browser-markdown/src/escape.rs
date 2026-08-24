//! Markdown escaping, `chomp`, and the Python string primitives markdownify
//! leans on (`str.strip()`, `str.rstrip()`, the `^(.*)$`-per-line rewrite).
//!
//! Python's `str.strip()` uses `str.isspace()`, which is the Unicode
//! `White_Space` property **plus** `U+001C`–`U+001F` (the ASCII file/group/
//! record/unit separators). Rust's [`char::is_whitespace`] is `White_Space`
//! alone, so [`is_py_space`] adds the four separators back — otherwise
//! `"\x1cfoo\x1c".strip()` would disagree with CPython.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::convert::MarkdownOptions;

/// Characters markdownify passes explicitly to `strip(' \t\r\n')`.
pub const ASCII_STRIP_CHARS: [char; 4] = [' ', '\t', '\r', '\n'];

/// `True` for exactly the characters CPython's `str.isspace()` accepts.
#[must_use]
#[inline]
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// Python's `str.strip()` (no argument).
#[must_use]
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// Python's `str.lstrip()` (no argument).
#[must_use]
pub fn py_lstrip(text: &str) -> &str {
    text.trim_start_matches(is_py_space)
}

/// Python's `str.rstrip()` (no argument).
#[must_use]
pub fn py_rstrip(text: &str) -> &str {
    text.trim_end_matches(is_py_space)
}

/// Python's `str.strip(chars)`.
#[must_use]
pub fn strip_chars<'a>(text: &'a str, chars: &[char]) -> &'a str {
    text.trim_matches(|c| chars.contains(&c))
}

/// Python's `str.lstrip(chars)`.
#[must_use]
pub fn lstrip_chars<'a>(text: &'a str, chars: &[char]) -> &'a str {
    text.trim_start_matches(|c| chars.contains(&c))
}

/// Python's `str.rstrip(chars)`.
#[must_use]
pub fn rstrip_chars<'a>(text: &'a str, chars: &[char]) -> &'a str {
    text.trim_end_matches(|c| chars.contains(&c))
}

/// markdownify's `chomp()`: hoist a single leading/trailing **space** out of an
/// inline tag's text so that `<b> foo</b>` becomes `` **foo**`` and not
/// `` ** foo**``.
///
/// Returns `(prefix, suffix, stripped_text)`. Only `' '` counts for the
/// prefix/suffix test, but the strip itself is a full `str.strip()`.
#[must_use]
pub fn chomp(text: &str) -> (&'static str, &'static str, &str) {
    let prefix = if text.starts_with(' ') { " " } else { "" };
    let suffix = if text.ends_with(' ') { " " } else { "" };
    (prefix, suffix, py_strip(text))
}

/// markdownify's `re_line_with_content.sub(fn, text)`: apply `f` to every line,
/// where "line" is what `^(.*)$` matches under `re.MULTILINE` — i.e. the
/// `'\n'`-separated pieces, including a trailing empty one.
pub fn map_lines(text: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&f(line));
    }
    out
}

// ---------------------------------------------------------------------------
// escaping
// ---------------------------------------------------------------------------

/// markdownify's `re_escape_misc_chars`: the punctuation that can start a
/// Markdown construct.
static ESCAPE_MISC_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([\]\\&<`\[>~=+|])").expect("the static pattern is valid"));

/// `re_escape_misc_dash_sequences` — a run of `-` that could read as a setext
/// underline or a list marker.
static ESCAPE_MISC_DASHES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\s|^)(-+(?:\s|$))").expect("the static pattern is valid"));

/// `re_escape_misc_hashes` — up to six `#` that could read as an ATX heading.
static ESCAPE_MISC_HASHES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\s|^)(#{1,6}(?:\s|$))").expect("the static pattern is valid"));

/// `re_escape_misc_list_items` — `1.` / `1)` that could read as an ordered list.
static ESCAPE_MISC_LIST_ITEMS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"((?:\s|^)[0-9]{1,9})([.)](?:\s|$))").expect("the static pattern is valid")
});

fn backslash_group1(caps: &Captures<'_>) -> String {
    format!("\\{}", caps.get(1).map_or("", |m| m.as_str()))
}

fn keep1_backslash2(caps: &Captures<'_>) -> String {
    format!(
        "{}\\{}",
        caps.get(1).map_or("", |m| m.as_str()),
        caps.get(2).map_or("", |m| m.as_str())
    )
}

/// markdownify's `MarkdownConverter.escape()`.
///
/// At the reference implementation's defaults only `escape_asterisks` and `escape_underscores` are
/// on, so this is `*` → `\*` followed by `_` → `\_`. The `escape_misc` branch is
/// ported too, in the same order as Python applies it.
#[must_use]
pub fn escape<'a>(text: &'a str, opts: &MarkdownOptions) -> Cow<'a, str> {
    if text.is_empty() {
        return Cow::Borrowed("");
    }
    let mut out = Cow::Borrowed(text);
    if opts.escape_misc {
        out = replace_all(out, &ESCAPE_MISC_CHARS, backslash_group1);
        out = replace_all(out, &ESCAPE_MISC_DASHES, keep1_backslash2);
        out = replace_all(out, &ESCAPE_MISC_HASHES, keep1_backslash2);
        out = replace_all(out, &ESCAPE_MISC_LIST_ITEMS, keep1_backslash2);
    }
    if opts.escape_asterisks && out.contains('*') {
        out = Cow::Owned(out.replace('*', r"\*"));
    }
    if opts.escape_underscores && out.contains('_') {
        out = Cow::Owned(out.replace('_', r"\_"));
    }
    out
}

fn replace_all<'a>(
    text: Cow<'a, str>,
    re: &Regex,
    rep: impl Fn(&Captures<'_>) -> String,
) -> Cow<'a, str> {
    match re.replace_all(text.as_ref(), rep) {
        Cow::Borrowed(_) => text,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_space_matches_cpython() {
        assert!(is_py_space('\u{1c}'));
        assert!(is_py_space('\u{a0}'));
        assert!(is_py_space('\u{85}'));
        assert!(!is_py_space('\u{200b}'));
        assert!(!is_py_space('a'));
    }

    #[test]
    fn strips_like_python() {
        assert_eq!(py_strip("\u{a0} hi \u{1c}"), "hi");
        assert_eq!(strip_chars(" \u{a0}hi\n", &ASCII_STRIP_CHARS), "\u{a0}hi");
    }

    #[test]
    fn chomp_hoists_spaces() {
        assert_eq!(chomp(" foo"), (" ", "", "foo"));
        assert_eq!(chomp("foo "), ("", " ", "foo"));
        assert_eq!(chomp(""), ("", "", ""));
        assert_eq!(chomp("\nfoo\n"), ("", "", "foo"));
    }

    #[test]
    fn map_lines_matches_multiline_regex() {
        let quoted = map_lines("a\n\nb", |line| {
            if line.is_empty() {
                ">".to_owned()
            } else {
                format!("> {line}")
            }
        });
        assert_eq!(quoted, "> a\n>\n> b");
    }

    #[test]
    fn default_escaping() {
        let opts = MarkdownOptions::default();
        assert_eq!(escape("a*b_c", &opts), r"a\*b\_c");
        assert_eq!(escape("", &opts), "");
    }

    #[test]
    fn misc_escaping() {
        let opts = MarkdownOptions {
            escape_misc: true,
            ..MarkdownOptions::default()
        };
        assert_eq!(escape("a < b", &opts), r"a \< b");
        assert_eq!(escape("- x", &opts), r"\- x");
        assert_eq!(escape("## x", &opts), r"\## x");
        assert_eq!(escape("1. x", &opts), r"1\. x");
    }
}
