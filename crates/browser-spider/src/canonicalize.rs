//! `canonicalize_url`: percent-encoding normalisation, query sorting and IDNA, pinned by a
//! golden corpus.
//!
//! This is a line-by-line port of `w3lib.url.canonicalize_url`, which the reference implementation calls from
//! [`crate::request::Request::update_fingerprint`]. It is *not* a general-purpose URL
//! normaliser: its only job is to make two spellings of the same page hash to the same
//! fingerprint, and any divergence from w3lib silently changes which pages a crawl visits.
//!
//! Because w3lib is built on CPython's `urllib.parse`, porting it means porting a slice of
//! `urllib.parse` too — `urlsplit`, `urlunparse`, `quote`, `unquote_to_bytes`, `quote_plus`
//! and `urlencode`. The [`url`] crate cannot stand in: it applies the WHATWG URL Standard,
//! which normalises the path (`/a/../b` → `/b`), strips default ports, refuses relative
//! input and re-encodes a different character set. All of those would change fingerprints.
//!
//! The whole module is byte-oriented for the same reason w3lib is: a percent-escape may
//! decode to bytes that are not valid UTF-8 (`%A3` in a Latin-1 page), and those bytes have
//! to survive the decode/re-encode round trip.
//!
//! # What the algorithm does
//!
//! 1. Strip C0 controls and spaces from both ends, and delete every tab/CR/LF anywhere.
//! 2. Split with CPython's `urlparse` (which also peels off the `;params` segment).
//! 3. IDNA-encode the netloc; percent-encode path, params, query and fragment.
//! 4. Parse the query into raw `(key, value)` byte pairs, **sort them**, re-encode.
//! 5. Percent-decode the path and re-encode it, which upper-cases every escape; an empty
//!    path becomes `"/"`.
//! 6. Drop the fragment unless `keep_fragments`.
//! 7. Lower-case the host (but not the userinfo) and strip a trailing `:`.
//!
//! ```
//! use browser_spider::canonicalize::canonicalize_url;
//!
//! assert_eq!(
//!     canonicalize_url("http://www.example.com/do?c=3&b=5&b=2&a=50", false),
//!     "http://www.example.com/do?a=50&b=2&b=5&c=3"
//! );
//! assert_eq!(
//!     canonicalize_url("http://www.example.com/r\u{e9}sum\u{e9}", false),
//!     "http://www.example.com/r%C3%A9sum%C3%A9"
//! );
//! ```
//!
//! # Known divergences from w3lib
//!
//! * **Non-ASCII hosts.** CPython's `str.encode("idna")` is IDNA 2003 (`nameprep` +
//!   Punycode). We use the [`idna`] crate, which is UTS-46 non-transitional. They agree on
//!   everything except the four "deviation" characters (`ß`, `ς`, ZWJ, ZWNJ) and a handful
//!   of disallowed-code-point edge cases. ASCII hosts — every host a crawl realistically
//!   sees — take a fast path that is exact.
//! * **`ValueError`s are not raised.** CPython's `urlsplit` rejects an unbalanced `[`/`]`
//!   and a netloc that NFKC-normalises into a delimiter. Fingerprinting cannot fail here,
//!   so those inputs are canonicalised on a best-effort basis instead of aborting the crawl.
//!
//! Both are encoded as declared-divergence rows in `testdata/canonicalize_url.jsonl`, which
//! fails if one of them ever starts matching Python.

use std::borrow::Cow;

/// Schemes for which CPython's `urlparse` peels a `;params` segment off the path.
///
/// `urllib.parse.uses_params`, CPython 3.14.
const USES_PARAMS: [&str; 16] = [
    "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu",
    "sip", "sips", "mms", "sftp", "tel",
];

/// Schemes that get a `//` authority prefix even with an empty netloc.
///
/// `urllib.parse.uses_netloc`, CPython 3.14.
const USES_NETLOC: [&str; 27] = [
    "",
    "ftp",
    "http",
    "gopher",
    "nntp",
    "telnet",
    "imap",
    "wais",
    "file",
    "mms",
    "https",
    "shttp",
    "snews",
    "prospero",
    "rtsp",
    "rtsps",
    "rtspu",
    "rsync",
    "svn",
    "svn+ssh",
    "sftp",
    "nfs",
    "git",
    "git+ssh",
    "ws",
    "wss",
    "itms-services",
];

/// `w3lib.url._safe_chars`: RFC 3986 reserved + unreserved + `|` + `%`.
///
/// Used for the params, query and fragment components.
const SAFE_CHARS: &[u8] = b":/?#[]@!$&'()*+,;=|%";

/// `w3lib.url._path_safe_chars`: [`SAFE_CHARS`] with `#` removed.
const PATH_SAFE_CHARS: &[u8] = b":/?[]@!$&'()*+,;=|%";

/// The four code points CPython's IDNA codec treats as label separators.
const IDNA_DOTS: [char; 4] = ['.', '\u{3002}', '\u{ff0e}', '\u{ff61}'];

/// The six components CPython's `urlparse` returns.
///
/// Field names and semantics are `urllib.parse.ParseResult`'s: `netloc` is the raw
/// authority *including* any `user:password@` and `:port`, and `params` is the `;`-suffixed
/// tail of the last path segment (only split off for the schemes CPython's private
/// `uses_params` list names: `ftp`, `hdl`, `prospero`, `http`, `imap`, `https`, `shttp`,
/// `rtsp`, `rtspu`, `sip`, `sips`, `mms`, `sftp` and `tel`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UrlParts {
    /// Lower-cased scheme, without the `:`. Empty when the URL is relative.
    pub scheme: String,
    /// The authority, without the leading `//`. Empty when there is none.
    pub netloc: String,
    /// The path, including its leading `/` when present.
    pub path: String,
    /// The `;params` tail of the last path segment.
    pub params: String,
    /// The query, without the leading `?`.
    pub query: String,
    /// The fragment, without the leading `#`.
    pub fragment: String,
}

/// CPython's `urllib.parse.urlparse`, minus the `scheme` default and `allow_fragments`.
///
/// ```
/// use browser_spider::canonicalize::urlparse;
///
/// let parts = urlparse("HTTP://user@Example.COM:8080/a;p?q=1#frag");
/// assert_eq!(parts.scheme, "http");
/// assert_eq!(parts.netloc, "user@Example.COM:8080");
/// assert_eq!(parts.path, "/a");
/// assert_eq!(parts.params, "p");
/// assert_eq!(parts.query, "q=1");
/// assert_eq!(parts.fragment, "frag");
/// ```
pub fn urlparse(url: &str) -> UrlParts {
    let (scheme, netloc, mut path, query, fragment) = urlsplit(url);
    let mut params = String::new();
    if USES_PARAMS.contains(&scheme.as_str()) && path.contains(';') {
        let (head, tail) = split_params(&path);
        params = tail;
        path = head;
    }
    UrlParts {
        scheme,
        netloc,
        path,
        params,
        query,
        fragment,
    }
}

/// The `netloc` of `url` — the authority, port and userinfo included.
///
/// This is `urlparse(url).netloc`, which is exactly what Python's
/// `Request.domain` cached property returns: `https://localhost:8080/x` yields
/// `localhost:8080`, **with** the port, and `http://u:p@h/` yields `u:p@h`.
///
/// ```
/// use browser_spider::canonicalize::netloc_of;
///
/// assert_eq!(netloc_of("https://localhost:8080/a"), "localhost:8080");
/// assert_eq!(netloc_of("/relative/path"), "");
/// ```
pub fn netloc_of(url: &str) -> String {
    urlsplit(url).1
}

/// `w3lib.url.canonicalize_url(url, keep_blank_values=True, keep_fragments=keep_fragments)`.
///
/// This is the spelling the reference implementation uses; [`canonicalize_url_with`] exposes the
/// `keep_blank_values` knob for completeness.
pub fn canonicalize_url(url: &str, keep_fragments: bool) -> String {
    canonicalize_url_with(url, true, keep_fragments)
}

/// The full `w3lib.url.canonicalize_url` signature.
///
/// `keep_blank_values=false` drops `?a=&b=2` down to `?b=2`; the reference implementation never does this, but
/// the corpus covers it so the port stays a faithful w3lib.
pub fn canonicalize_url_with(url: &str, keep_blank_values: bool, keep_fragments: bool) -> String {
    let stripped = strip_url(url);
    let parts = urlparse(&stripped);

    // --- `_safe_ParseResult`: IDNA the netloc, percent-encode everything else. ---
    let netloc = idna_encode_netloc(&parts.netloc);
    let path = quote_from_bytes(parts.path.as_bytes(), PATH_SAFE_CHARS);
    let params = quote_from_bytes(parts.params.as_bytes(), SAFE_CHARS);
    let query = quote_from_bytes(parts.query.as_bytes(), SAFE_CHARS);
    let fragment = quote_from_bytes(parts.fragment.as_bytes(), SAFE_CHARS);

    // --- 1. sort the query, keeping raw bytes so a non-UTF-8 escape survives. ---
    let mut keyvals = parse_qsl_to_bytes(&query, keep_blank_values);
    keyvals.sort();
    let query = urlencode_pairs(&keyvals);

    // --- 2. re-quote the path, which normalises escapes to upper-case hex. ---
    let unquoted_path = unquote_path(&path);
    let mut path = quote_from_bytes(&unquoted_path, PATH_SAFE_CHARS);
    if path.is_empty() {
        path.push('/');
    }

    let fragment = if keep_fragments {
        fragment
    } else {
        String::new()
    };

    // --- 3. lower-case the host but never the userinfo. ---
    let netloc = lower_host(&netloc);

    urlunparse(&parts.scheme, &netloc, &path, &params, &query, &fragment)
}

// ---------------------------------------------------------------------------
// CPython `urllib.parse` internals
// ---------------------------------------------------------------------------

/// `w3lib.url._strip`: strip C0 controls and spaces from both ends, then delete every
/// tab, CR and LF anywhere in the string.
fn strip_url(url: &str) -> Cow<'_, str> {
    let trimmed = url.trim_matches(|c: char| (c as u32) <= 0x20);
    if trimmed.contains(['\t', '\n', '\r']) {
        Cow::Owned(trimmed.replace(['\t', '\n', '\r'], ""))
    } else {
        Cow::Borrowed(trimmed)
    }
}

/// CPython's `urllib.parse.urlsplit`, returning `(scheme, netloc, path, query, fragment)`.
///
/// The two `ValueError` branches (unbalanced brackets, and `_checknetloc`'s NFKC test) are
/// deliberately not reproduced — see the module docs.
fn urlsplit(url: &str) -> (String, String, String, String, String) {
    // `urlsplit` lstrips C0-or-space and removes tab/CR/LF; `_strip` already did both for
    // the canonicalize path, but `netloc_of` calls in here directly.
    let url = url.trim_start_matches(|c: char| (c as u32) <= 0x20);
    let url: Cow<'_, str> = if url.contains(['\t', '\n', '\r']) {
        Cow::Owned(url.replace(['\t', '\n', '\r'], ""))
    } else {
        Cow::Borrowed(url)
    };

    let mut rest: &str = &url;
    let mut scheme = String::new();

    if let Some(i) = rest.find(':') {
        let head = &rest[..i];
        if i > 0
            && head.starts_with(|c: char| c.is_ascii_alphabetic())
            && head
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            scheme = head.to_ascii_lowercase();
            rest = &rest[i + 1..];
        }
    }

    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_owned();
        rest = &after[end..];
    }

    let mut fragment = String::new();
    if let Some((head, frag)) = rest.split_once('#') {
        fragment = frag.to_owned();
        rest = head;
    }

    let mut query = String::new();
    if let Some((head, q)) = rest.split_once('?') {
        query = q.to_owned();
        rest = head;
    }

    (scheme, netloc, rest.to_owned(), query, fragment)
}

/// CPython's `urllib.parse._splitparams`.
fn split_params(path: &str) -> (String, String) {
    let idx = match path.rfind('/') {
        Some(slash) => match path[slash..].find(';') {
            Some(offset) => slash + offset,
            None => return (path.to_owned(), String::new()),
        },
        None => match path.find(';') {
            Some(i) => i,
            None => return (path.to_owned(), String::new()),
        },
    };
    (path[..idx].to_owned(), path[idx + 1..].to_owned())
}

/// CPython's `urllib.parse.urlunparse` composed with `_urlunsplit`.
///
/// This follows the 3.14 shape, which differs from older releases in two ways that the
/// fuzz corpus catches: an empty netloc is only re-emitted as `//` when the scheme is in
/// [`USES_NETLOC`] *and* the path is empty or absolute, and a path that starts with `//`
/// under a `None` netloc gets a second `//` so it cannot be re-read as an authority
/// (`svn+ssh:////a` round-trips instead of collapsing to `svn+ssh://a`).
fn urlunparse(
    scheme: &str,
    netloc: &str,
    path: &str,
    params: &str,
    query: &str,
    fragment: &str,
) -> String {
    // The netloc decision is made on the bare path, *before* `;params` is appended.
    let netloc: Option<&str> = if netloc.is_empty() {
        (!scheme.is_empty()
            && USES_NETLOC.contains(&scheme)
            && (path.is_empty() || path.starts_with('/')))
        .then_some("")
    } else {
        Some(netloc)
    };

    let mut url = if params.is_empty() {
        path.to_owned()
    } else {
        format!("{path};{params}")
    };

    match netloc {
        Some(netloc) => {
            if !url.is_empty() && !url.starts_with('/') {
                url.insert(0, '/');
            }
            url = format!("//{netloc}{url}");
        }
        None if url.starts_with("//") => url.insert_str(0, "//"),
        None => {}
    }
    if !scheme.is_empty() {
        url = format!("{scheme}:{url}");
    }
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    if !fragment.is_empty() {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

/// CPython's `urllib.parse.quote_from_bytes`.
///
/// `safe` is added to the always-safe set (ASCII alphanumerics plus `_.-~`); bytes ≥ 128 in
/// `safe` are ignored, as in CPython. Escapes are upper-case hex.
fn quote_from_bytes(bs: &[u8], safe: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bs.len());
    for &b in bs {
        let always_safe = b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~');
        if always_safe || (b < 128 && safe.contains(&b)) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 0x0f)] as char);
        }
    }
    out
}

/// CPython's `urllib.parse.unquote_to_bytes`.
///
/// An escape that is not two hex digits is left alone, byte for byte.
fn unquote_to_bytes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    if !bytes.contains(&b'%') {
        return bytes.to_vec();
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut chunks = bytes.split(|&b| b == b'%');
    if let Some(first) = chunks.next() {
        out.extend_from_slice(first);
    }
    for chunk in chunks {
        match chunk
            .first()
            .zip(chunk.get(1))
            .and_then(|(hi, lo)| hex_pair(*hi, *lo))
        {
            Some(byte) => {
                out.push(byte);
                out.extend_from_slice(&chunk[2..]);
            }
            None => {
                out.push(b'%');
                out.extend_from_slice(chunk);
            }
        }
    }
    out
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    Some((digit(hi)? << 4) | digit(lo)?)
}

/// `w3lib.url._unquotepath`.
///
/// `%2F` and `%3F` are double-escaped first so an encoded slash or question mark does not
/// silently become a real delimiter when the path is re-quoted.
fn unquote_path(path: &str) -> Vec<u8> {
    let mut escaped = path.to_owned();
    for reserved in ["2f", "2F", "3f", "3F"] {
        let upper = reserved.to_ascii_uppercase();
        escaped = escaped.replace(&format!("%{reserved}"), &format!("%25{upper}"));
    }
    unquote_to_bytes(&escaped)
}

/// `w3lib.url.parse_qsl_to_bytes` — CPython's `parse_qsl` with `unquote_to_bytes`.
///
/// Note that it splits on `;` as well as `&`, which CPython's own `parse_qsl` stopped doing
/// in 3.10; w3lib carries the older copy, so the port does too.
fn parse_qsl_to_bytes(qs: &str, keep_blank_values: bool) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for pair in qs.split('&').flat_map(|s| s.split(';')) {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((n, v)) => (n, v),
            None => {
                if keep_blank_values {
                    (pair, "")
                } else {
                    continue;
                }
            }
        };
        if value.is_empty() && !keep_blank_values {
            continue;
        }
        out.push((
            unquote_to_bytes(&name.replace('+', " ")),
            unquote_to_bytes(&value.replace('+', " ")),
        ));
    }
    out
}

/// CPython's `urllib.parse.urlencode` over string pairs, with the default `quote_plus`.
///
/// This is the form encoding a `data={"a": "1 2"}` body gets before it is hashed into a
/// request fingerprint, so it lives here next to the rest of the quoting rules rather than
/// being re-derived in `request.rs`.
///
/// ```
/// use browser_spider::canonicalize::urlencode_form;
///
/// assert_eq!(
///     urlencode_form(&[("a".into(), "1 2".into()), ("b".into(), "x&y".into())]),
///     "a=1+2&b=x%26y"
/// );
/// ```
pub fn urlencode_form(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&quote_plus_bytes(k.as_bytes()));
        out.push('=');
        out.push_str(&quote_plus_bytes(v.as_bytes()));
    }
    out
}

/// CPython's `urllib.parse.urlencode` over byte pairs, with the default `quote_plus`.
fn urlencode_pairs(pairs: &[(Vec<u8>, Vec<u8>)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&quote_plus_bytes(k));
        out.push('=');
        out.push_str(&quote_plus_bytes(v));
    }
    out
}

/// CPython's `urllib.parse.quote_plus(bs, safe="")`: space becomes `+`.
fn quote_plus_bytes(bs: &[u8]) -> String {
    if bs.contains(&b' ') {
        quote_from_bytes(bs, b" ").replace(' ', "+")
    } else {
        quote_from_bytes(bs, b"")
    }
}

/// `netloc.encode("idna")`, with CPython's fallback: any `UnicodeError` leaves the netloc
/// exactly as it was.
fn idna_encode_netloc(netloc: &str) -> String {
    if netloc.is_empty() {
        return String::new();
    }
    if netloc.is_ascii() {
        // CPython's fast path returns the netloc unchanged when every label fits, and
        // raises otherwise — and the caller swallows the raise and uses the netloc
        // unchanged. Both branches produce the same string, so there is nothing to check.
        return netloc.to_owned();
    }

    let mut labels: Vec<&str> = netloc.split(IDNA_DOTS).collect();
    let trailing_dot = if labels.last().is_some_and(|l| l.is_empty()) {
        labels.pop();
        "."
    } else {
        ""
    };

    let mut encoded = Vec::with_capacity(labels.len());
    for label in labels {
        match label_to_ascii(label) {
            Some(ascii) => encoded.push(ascii),
            // `UnicodeError` from any label: keep the whole netloc as it was.
            None => return netloc.to_owned(),
        }
    }
    format!("{}{trailing_dot}", encoded.join("."))
}

/// CPython's `encodings.idna.ToASCII` for one label.
fn label_to_ascii(label: &str) -> Option<String> {
    if label.is_ascii() {
        return (!label.is_empty() && label.len() < 64).then(|| label.to_owned());
    }
    // Step 2-7: nameprep + Punycode. UTS-46 non-transitional stands in for IDNA 2003;
    // see the module docs for the deviation characters where they disagree.
    let ascii = idna::domain_to_ascii(label).ok()?;
    (!ascii.is_empty() && ascii.len() < 64).then_some(ascii)
}

/// `netloc.split("@")` with the final element lower-cased and its trailing `:` stripped.
fn lower_host(netloc: &str) -> String {
    match netloc.rsplit_once('@') {
        Some((userinfo, host)) => {
            format!("{userinfo}@{}", host.to_lowercase().trim_end_matches(':'))
        }
        None => netloc.to_lowercase().trim_end_matches(':').to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_query_arguments_by_key_then_value() {
        assert_eq!(
            canonicalize_url("http://www.example.com/do?c=3&b=5&b=2&a=50", false),
            "http://www.example.com/do?a=50&b=2&b=5&c=3"
        );
    }

    #[test]
    fn percent_encodes_non_ascii_paths_as_utf8() {
        assert_eq!(
            canonicalize_url("http://www.example.com/r\u{e9}sum\u{e9}", false),
            "http://www.example.com/r%C3%A9sum%C3%A9"
        );
    }

    #[test]
    fn normalises_escape_case_but_not_encoded_delimiters() {
        assert_eq!(
            canonicalize_url("http://example.com/a%2fb%3fc%7ed", false),
            "http://example.com/a%2Fb%3Fc~d"
        );
    }

    #[test]
    fn empty_path_becomes_a_slash() {
        assert_eq!(
            canonicalize_url("http://example.com", false),
            "http://example.com/"
        );
    }

    #[test]
    fn drops_the_fragment_unless_asked_to_keep_it() {
        assert_eq!(
            canonicalize_url("http://example.com/a#frag", false),
            "http://example.com/a"
        );
        assert_eq!(
            canonicalize_url("http://example.com/a#frag", true),
            "http://example.com/a#frag"
        );
    }

    #[test]
    fn lower_cases_the_host_but_not_the_userinfo_or_path() {
        assert_eq!(
            canonicalize_url("HTTP://User@Example.COM:8080/PaTh", false),
            "http://User@example.com:8080/PaTh"
        );
    }

    #[test]
    fn keeps_blank_values_by_default_and_drops_them_on_request() {
        assert_eq!(
            canonicalize_url("http://example.com/?a=&b=2", false),
            "http://example.com/?a=&b=2"
        );
        assert_eq!(
            canonicalize_url_with("http://example.com/?a=&b=2", false, false),
            "http://example.com/?b=2"
        );
    }

    #[test]
    fn plus_in_a_query_value_is_a_space() {
        assert_eq!(
            canonicalize_url("http://example.com/?q=a+b", false),
            "http://example.com/?q=a+b"
        );
        assert_eq!(
            canonicalize_url("http://example.com/?q=a%20b", false),
            "http://example.com/?q=a+b"
        );
    }

    #[test]
    fn keeps_non_utf8_percent_escapes_in_the_query() {
        // w3lib decodes to raw bytes and re-encodes, so `%a3` survives as `%A3` instead of
        // becoming U+FFFD.
        assert_eq!(
            canonicalize_url("http://example.com/?q=b%a3", false),
            "http://example.com/?q=b%A3"
        );
    }

    #[test]
    fn splits_params_off_the_last_path_segment() {
        assert_eq!(
            canonicalize_url("http://example.com/a;p1/b;p2?x=1", false),
            "http://example.com/a;p1/b;p2?x=1"
        );
        let parts = urlparse("http://example.com/a;p1/b;p2");
        assert_eq!(parts.path, "/a;p1/b");
        assert_eq!(parts.params, "p2");
    }

    #[test]
    fn strips_control_characters_and_embedded_newlines() {
        assert_eq!(
            canonicalize_url("  http://exa\tmple.com/a\nb  ", false),
            "http://example.com/ab"
        );
    }

    #[test]
    fn netloc_of_keeps_the_port_and_userinfo() {
        assert_eq!(netloc_of("https://localhost:8080/x"), "localhost:8080");
        assert_eq!(netloc_of("https://u:p@h.example/x"), "u:p@h.example");
        assert_eq!(netloc_of("not a url"), "");
    }

    #[test]
    fn idna_encodes_a_unicode_host() {
        assert_eq!(
            canonicalize_url("http://www.b\u{fc}cher.de/", false),
            "http://www.xn--bcher-kva.de/"
        );
    }

    #[test]
    fn a_relative_url_keeps_its_shape() {
        assert_eq!(canonicalize_url("/a/b?z=1&a=2", false), "/a/b?a=2&z=1");
    }
}
