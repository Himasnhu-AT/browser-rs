//! [`parse_cookie_string`] and [`parse_header_lines`]: the `SimpleCookie`-compatible
//! state machine, including attribute swallowing.
//!
//! Port of `reference/core/utils/_shell.py` (`_CookieParser`, `_ParseHeaders`), used by
//! the CLI and the interactive shell to turn `curl`-style `-H` lines into a header map
//! plus a cookie map.
//!
//! The interesting half is `http.cookies.SimpleCookie.load()`, whose behaviour the CLI
//! inherits and whose quirks users depend on:
//!
//! * **Attribute swallowing.** `Path`, `Domain`, `Expires`, `Max-Age`, `Secure`,
//!   `HttpOnly`, `SameSite`, `Version`, `Comment` and `Partitioned` attach to the
//!   *preceding* cookie and never become cookies themselves.
//! * **All-or-nothing validation.** The string is parsed in full before anything is
//!   applied; one syntactically invalid token discards *the entire string*, silently.
//!   `"invalid_cookie_format"` therefore yields nothing rather than raising.
//! * **Quoted values are unquoted**, including `\\ooo` octal and `\\c` escapes.
//! * A reserved attribute before any cookie, or a flag-less attribute with no value,
//!   also invalidates the whole string.

use std::sync::OnceLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::error::{CookieParseError, HeaderParseError};

/// An ordered map of header names to values, in the order the lines were given.
pub type HeaderMap = IndexMap<String, String>;

/// An ordered map of cookie names to values.
pub type CookieMap = IndexMap<String, String>;

/// What [`parse_header_lines`] returns: the headers, then the cookies.
pub type ParsedHeaders = (HeaderMap, CookieMap);

/// `http.cookies._LegalKeyChars`, with `\w`/`\d` expanded for `re.ASCII`.
const LEGAL_KEY_CHARS: &str = r"A-Za-z0-9_!#%&'~`><@,:/\$\*\+\-\.\^\|\)\(\?\}\{\=";
/// `http.cookies._LegalValueChars` — the key set plus `[` and `]`.
const LEGAL_VALUE_CHARS: &str = r"A-Za-z0-9_!#%&'~`><@,:/\$\*\+\-\.\^\|\)\(\?\}\{\=\[\]";
/// `re.ASCII` whitespace: `[ \t\n\r\f\v]`.
const ASCII_WS: &str = r" \t\n\r\x0c\x0b";

/// `http.cookies.Morsel._reserved` — attribute names, lowercase.
const RESERVED: [&str; 10] = [
    "expires",
    "path",
    "comment",
    "domain",
    "max-age",
    "secure",
    "httponly",
    "version",
    "samesite",
    "partitioned",
];

/// `http.cookies.Morsel._flags` — the reserved attributes that may appear bare.
const FLAGS: [&str; 3] = ["secure", "httponly", "partitioned"];

/// `http.cookies._CookiePattern`, anchored so it can be applied at a cursor.
fn cookie_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pattern = format!(
            concat!(
                r"\A[{ws}]*",
                r"(?P<key>[{key}]+?)",
                r"(?:[{ws}]*=[{ws}]*",
                r"(?P<val>",
                r#""(?:[^\\"]|\\.)*""#,
                r"|[A-Za-z0-9_]{{3}},[{ws}][A-Za-z0-9_{ws}-]{{9,11}}[{ws}][0-9:]{{8}}[{ws}]GMT",
                r"|[{val}]*",
                r")",
                r")?",
                r"[{ws}]*(?:[{ws}]+|;|$)",
            ),
            ws = ASCII_WS,
            key = LEGAL_KEY_CHARS,
            val = LEGAL_VALUE_CHARS,
        );
        Regex::new(&pattern).expect("the transcribed SimpleCookie pattern is valid")
    })
}

/// `http.cookies._is_legal_key`: `_LegalChars` only, and at least one character.
fn is_legal_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~:".contains(c))
}

fn octal_escape(bytes: &[u8], j: usize) -> Option<char> {
    let digits = bytes.get(j + 1..j + 4)?;
    let value = digits.iter().try_fold(0u32, |acc, &b| match b {
        b'0'..=b'7' => Some(acc * 8 + u32::from(b - b'0')),
        _ => None,
    })?;
    char::from_u32(value)
}

/// `http.cookies._unquote`.
///
/// Strings shorter than two characters, or not wrapped in double quotes, come back
/// untouched. Otherwise the quotes are stripped and `\\ooo` (three octal digits) and
/// `\\c` escapes are decoded, left to right.
fn unquote(value: &str) -> String {
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return value.to_owned();
    }
    let inner = &value[1..value.len() - 1];
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if let Some(c) = octal_escape(bytes, i) {
                out.push(c);
                i += 4;
                continue;
            }
            // `_QuotePatt` is `[\\].` — a backslash plus any one character, where
            // Python's `.` does not match a newline.
            if let Some(next) = inner[i + 1..].chars().next() {
                if next != '\n' {
                    out.push(next);
                    i += 1 + next.len_utf8();
                    continue;
                }
            }
        }
        // Not an escape: copy one character.
        let c = inner[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// One entry of `SimpleCookie.__parse_string`'s `parsed_items` list.
enum Parsed {
    /// A cookie attribute; swallowed by the preceding cookie, never yielded.
    Attribute,
    /// A real `name=value` cookie.
    KeyValue(String, String),
}

/// Parse a `Cookie:` header value into `(name, value)` pairs, exactly as
/// `_CookieParser` (i.e. `http.cookies.SimpleCookie.load`) does.
///
/// Cookie *attributes* are consumed by the preceding cookie and never appear in the
/// result. A syntactically invalid string yields an empty result rather than an error
/// — Python's parser discards everything it parsed and returns silently.
///
/// ```
/// use browser_core::shell::parse_cookie_string;
///
/// assert_eq!(
///     parse_cookie_string("session_id=abc123").unwrap(),
///     vec![("session_id".to_string(), "abc123".to_string())]
/// );
///
/// // Attributes attach to the cookie before them.
/// let cookies = parse_cookie_string("a=1; Path=/; HttpOnly; Secure; b=2").unwrap();
/// assert_eq!(cookies.len(), 2);
///
/// // Garbage is discarded silently, not reported.
/// assert!(parse_cookie_string("invalid_cookie_format").unwrap().is_empty());
/// assert!(parse_cookie_string("").unwrap().is_empty());
/// ```
pub fn parse_cookie_string(cookie_string: &str) -> Result<Vec<(String, String)>, CookieParseError> {
    let patt = cookie_pattern();
    let bytes = cookie_string.len();
    let mut i = 0usize;
    let mut parsed: Vec<Parsed> = Vec::new();
    let mut morsel_seen = false;

    while i < bytes {
        let Some(caps) = patt.captures(&cookie_string[i..]) else {
            break; // No more cookies.
        };
        let whole = caps.get(0).map_or(0, |m| m.end());
        if whole == 0 {
            break; // Defensive: a zero-width match would loop forever.
        }
        let key = caps.name("key").map_or("", |m| m.as_str()).to_owned();
        let value = caps.name("val").map(|m| m.as_str().to_owned());
        i += whole;

        if key.starts_with('$') {
            // `$Version` and friends describe the cookie mechanism, not a cookie.
            if !morsel_seen {
                continue;
            }
            parsed.push(Parsed::Attribute);
            continue;
        }

        let lowered = key.to_lowercase();
        if RESERVED.contains(&lowered.as_str()) {
            if !morsel_seen {
                return Ok(Vec::new()); // Invalid cookie string.
            }
            match value {
                None if FLAGS.contains(&lowered.as_str()) => parsed.push(Parsed::Attribute),
                None => return Ok(Vec::new()), // Invalid cookie string.
                Some(_) => parsed.push(Parsed::Attribute),
            }
        } else if let Some(value) = value {
            parsed.push(Parsed::KeyValue(key, unquote(&value)));
            morsel_seen = true;
        } else {
            return Ok(Vec::new()); // Invalid cookie string.
        }
    }

    // The string is valid; apply it. `IndexMap` reproduces `dict`'s "overwrite in
    // place, keep the original position" behaviour for a repeated cookie name.
    let mut out: IndexMap<String, String> = IndexMap::new();
    for item in parsed {
        match item {
            Parsed::Attribute => {}
            Parsed::KeyValue(key, value) => {
                // `Morsel.set()` refuses keys outside `_LegalChars`, raising
                // `CookieError`; `_ParseHeaders` turns that into a `ValueError`.
                if !is_legal_key(&key) {
                    return Err(CookieParseError::IllegalKey { key });
                }
                out.insert(key, value);
            }
        }
    }
    Ok(out.into_iter().collect())
}

/// Split raw `Name: value` header lines into `(headers, cookies)`, exactly as
/// `_ParseHeaders` does.
///
/// * The split is on the **first** colon only, and both halves are trimmed, so
///   `"Authorization: Bearer: token123"` keeps the value `"Bearer: token123"`.
/// * A line with no colon that ends with `;` becomes `{"<name>": ""}`; a line with no
///   colon and no trailing `;` is an error.
/// * Only a header literally named `cookie` (case-insensitively) is diverted into the
///   cookie map — `Set-Cookie` stays in the headers.
/// * A second `Cookie:` header **replaces** the cookie map rather than merging into it.
/// * Repeated headers overwrite, last one wins.
///
/// ```
/// use browser_core::shell::parse_header_lines;
///
/// let (headers, cookies) = parse_header_lines(
///     &["User-Agent: x", "Cookie: a=1; b=2", "Set-Cookie: c=3"],
///     true,
/// )
/// .unwrap();
/// assert_eq!(headers["User-Agent"], "x");
/// assert!(headers.contains_key("Set-Cookie"));
/// assert!(!headers.contains_key("Cookie"));
/// assert_eq!(cookies["a"], "1");
/// ```
pub fn parse_header_lines<S: AsRef<str>>(
    header_lines: &[S],
    parse_cookies: bool,
) -> Result<ParsedHeaders, HeaderParseError> {
    let mut headers: HeaderMap = HeaderMap::new();
    let mut cookies: CookieMap = CookieMap::new();

    for line in header_lines {
        let line = line.as_ref();
        let Some((key, value)) = line.split_once(':') else {
            if let Some(stripped) = line.strip_suffix(';') {
                headers.insert(stripped.trim().to_owned(), String::new());
                continue;
            }
            return Err(HeaderParseError::MissingColon {
                line: line.to_owned(),
            });
        };
        let key = key.trim();
        let value = value.trim();

        if parse_cookies && key.eq_ignore_ascii_case("cookie") {
            // Note: replaces, never merges — a second `Cookie:` header wins outright.
            cookies = parse_cookie_string(value)
                .map_err(|source| HeaderParseError::Cookie {
                    value: value.to_owned(),
                    source,
                })?
                .into_iter()
                .collect();
        } else {
            headers.insert(key.to_owned(), value.to_owned());
        }
    }

    Ok((headers, cookies))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(s: &str) -> Vec<(String, String)> {
        parse_cookie_string(s).expect("valid cookie string")
    }

    // §6.45
    #[test]
    fn single_cookie() {
        assert_eq!(
            pairs("session_id=abc123"),
            vec![("session_id".into(), "abc123".into())]
        );
    }

    #[test]
    fn multiple_semicolon_separated_cookies() {
        let got = pairs("a=1; b=2; c=3");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], ("a".to_string(), "1".to_string()));
        assert_eq!(got[2], ("c".to_string(), "3".to_string()));
    }

    #[test]
    fn attributes_are_swallowed_by_the_preceding_cookie() {
        let got = pairs("session=abc; Path=/; HttpOnly; Secure; Domain=example.com");
        assert_eq!(got, vec![("session".to_string(), "abc".to_string())]);

        let got = pairs("a=1; Path=/; b=2; Secure");
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn expires_attribute_is_swallowed() {
        let got = pairs("a=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Path=/");
        assert_eq!(got, vec![("a".to_string(), "1".to_string())]);
    }

    #[test]
    fn empty_and_garbage_yield_nothing_without_raising() {
        assert!(pairs("").is_empty());
        assert!(pairs("invalid_cookie_format").is_empty());
        assert!(pairs("   ").is_empty());
    }

    #[test]
    fn one_bad_token_discards_the_whole_string() {
        // `SimpleCookie` validates everything before applying anything.
        assert!(pairs("a=1; bare_token").is_empty());
        // A reserved attribute before any cookie is equally fatal.
        assert!(pairs("Path=/; a=1").is_empty());
        // A non-flag reserved attribute with no value, too.
        assert!(pairs("a=1; Domain").is_empty());
    }

    #[test]
    fn empty_values_are_kept() {
        assert_eq!(pairs("a=; b=2")[0], ("a".to_string(), String::new()));
    }

    #[test]
    fn quoted_values_are_unquoted() {
        assert_eq!(
            pairs(r#"a="hello world""#),
            vec![("a".into(), "hello world".into())]
        );
        assert_eq!(
            pairs(r#"a="say \"hi\"""#),
            vec![("a".into(), "say \"hi\"".into())]
        );
        assert_eq!(
            pairs(r#"a="tab\011here""#),
            vec![("a".into(), "tab\there".into())]
        );
    }

    #[test]
    fn repeated_names_overwrite_in_place() {
        let got = pairs("a=1; b=2; a=3");
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "3".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn illegal_keys_are_reported() {
        // `,` is a legal *pattern* character but not a legal *morsel key* character,
        // so Python raises `CookieError` here.
        let err = parse_cookie_string("a,b=1").unwrap_err();
        assert!(matches!(err, CookieParseError::IllegalKey { .. }));
    }

    // §6.46
    #[test]
    fn headers_split_on_the_first_colon_only() {
        let (headers, cookies) =
            parse_header_lines(&["Authorization: Bearer: token123"], true).unwrap();
        assert_eq!(headers["Authorization"], "Bearer: token123");
        assert!(cookies.is_empty());
    }

    #[test]
    fn header_sides_are_trimmed() {
        let (headers, _) = parse_header_lines(&["  Accept  :   text/html  "], true).unwrap();
        assert_eq!(headers["Accept"], "text/html");
    }

    // §6.47
    #[test]
    fn colonless_line_ending_in_semicolon_becomes_an_empty_value() {
        let (headers, _) = parse_header_lines(&["InvalidHeader;"], true).unwrap();
        assert_eq!(headers["InvalidHeader"], "");
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn colonless_line_is_an_error_with_the_pinned_message() {
        let err = parse_header_lines(&["InvalidHeader"], true).unwrap_err();
        assert!(err
            .to_string()
            .contains("Could not parse header without colon"));
        assert_eq!(
            err.to_string(),
            "Could not parse header without colon: 'InvalidHeader'."
        );
    }

    // §6.48
    #[test]
    fn set_cookie_stays_in_the_headers() {
        let (headers, cookies) =
            parse_header_lines(&["Set-Cookie: a=1; Path=/", "Cookie: b=2"], true).unwrap();
        assert_eq!(headers["Set-Cookie"], "a=1; Path=/");
        assert!(!headers.contains_key("Cookie"));
        assert_eq!(cookies["b"], "2");
        assert_eq!(cookies.len(), 1);
    }

    #[test]
    fn cookie_header_matching_is_case_insensitive() {
        let (headers, cookies) = parse_header_lines(&["COOKIE: a=1"], true).unwrap();
        assert!(headers.is_empty());
        assert_eq!(cookies["a"], "1");
    }

    #[test]
    fn a_second_cookie_header_replaces_rather_than_merges() {
        let (_, cookies) = parse_header_lines(&["Cookie: a=1", "Cookie: b=2"], true).unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies["b"], "2");
        assert!(!cookies.contains_key("a"));
    }

    #[test]
    fn parse_cookies_false_leaves_the_cookie_header_alone() {
        let (headers, cookies) = parse_header_lines(&["Cookie: a=1"], false).unwrap();
        assert_eq!(headers["Cookie"], "a=1");
        assert!(cookies.is_empty());
    }

    #[test]
    fn repeated_headers_overwrite() {
        let (headers, _) = parse_header_lines(&["X: 1", "X: 2"], true).unwrap();
        assert_eq!(headers["X"], "2");
        assert_eq!(headers.len(), 1);
    }

    // §6.49
    #[test]
    fn empty_input_gives_two_empty_maps() {
        let empty: [&str; 0] = [];
        let (headers, cookies) = parse_header_lines(&empty, true).unwrap();
        assert!(headers.is_empty());
        assert!(cookies.is_empty());
    }

    #[test]
    fn header_order_is_preserved() {
        let (headers, _) = parse_header_lines(&["A: 1", "B: 2", "C: 3"], true).unwrap();
        assert_eq!(headers.keys().collect::<Vec<_>>(), vec!["A", "B", "C"]);
    }
}
