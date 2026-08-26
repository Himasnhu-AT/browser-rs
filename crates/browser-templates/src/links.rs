//! [`LinkExtractor`] with its 13-option builder, [`IGNORED_EXTENSIONS`], the
//! compound-suffix chain, [`strip_html5_whitespace`], [`safe_url_string`] and
//! first-seen-order dedup.
//!
//! A port of `reference/spiders/links.py` (`spider-support.md` §2). This is the
//! "pure URL discovery primitive" the whole templates crate is built on:
//! [`crate::crawl::CrawlSpider`] runs one per rule against every response, and
//! [`crate::sitemap::SitemapSpider`] runs [`LinkExtractor::matches`] against
//! bare sitemap URLs where there is no response to extract from.
//!
//! ```
//! use browser_templates::links::{LinkExtractor, Response};
//!
//! let response = Response::builder(200, "https://shop.test/list")
//!     .body(r#"<a href="/p/1">one</a><a href="/p/2.pdf">pdf</a><a href="mailto:x@y">mail</a>"#)
//!     .build();
//!
//! let extractor = LinkExtractor::builder().build()?;
//! assert_eq!(extractor.extract(&response), ["https://shop.test/p/1"]);
//! # Ok::<(), browser_templates::error::TemplateError>(())
//! ```
//!
//! `.pdf` is dropped by [`IGNORED_EXTENSIONS`] and `mailto:` by the scheme
//! check, which between them are most of what this type is for.
//!
//! # Two things that are easy to get wrong
//!
//! * **[`strip_html5_whitespace`] is not [`str::trim`].** It strips exactly the
//!   five HTML5 space characters. `trim` also strips U+00A0, U+2028, U+180E and
//!   the rest of Unicode `White_Space`, which would silently change which
//!   `href`s survive.
//! * **Extensions are a chain, not a suffix.** `/dataset.tar.gz` yields
//!   `("tar.gz", "gz")`, which is why `"tar.gz"` in the deny set works at all.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use regex::Regex;
use browser_spider::canonicalize::{canonicalize_url, urlparse};
use browser_spider::spider::CrawlResponse;

use crate::error::{Result, TemplateError};

/// Unit 12's `browser_http::response::Response` — the one response type in
/// the workspace.
///
/// `browser-templates` does not depend on `browser-http` directly (its
/// `Cargo.toml` names `browser-core`, `-parser` and `-spider`), so the type
/// is named through [`CrawlResponse`]'s [`Deref`](std::ops::Deref) target
/// rather than by crate path. That is not a workaround for a missing
/// dependency: it is the same statement `crawl_spider_seam` makes — a callback
/// is handed a `CrawlResponse`, a `CrawlResponse` *is* a `Response`, and there
/// is exactly one of them. Every extractor here therefore takes `&Response`,
/// and `&CrawlResponse` coerces at the call site.
pub type Response = <CrawlResponse as std::ops::Deref>::Target;

// ---------------------------------------------------------------------------
// Module constants
// ---------------------------------------------------------------------------

/// The five HTML5 "space characters", and only those.
///
/// `https://www.w3.org/TR/html5/infrastructure.html#space-character`, which is
/// also w3lib's `HTML5_WHITESPACE = " \t\n\r\x0c"`.
pub const HTML5_SPACE: [char; 5] = [' ', '\t', '\n', '\r', '\u{0c}'];

/// The schemes a link may have. Python's `valid_schemas`.
///
/// Note `file` is in the set and `ftp` is not.
pub const VALID_SCHEMES: [&str; 3] = ["http", "https", "file"];

/// The 91 extensions dropped by default — Python's `IGNORED_EXTENSIONS`.
///
/// Lower-case, no leading dot, **sorted** so [`LinkExtractor`] can binary-search
/// it and so a diff against Python is a diff of two sorted lists. The Python
/// set is unordered, and `spider-support.md` §2.1 miscounts it as 100 entries;
/// the real `len(IGNORED_EXTENSIONS)` is 91.
///
/// `"tar.gz"` is a **compound** extension and is matched as one — see
/// [`url_extensions`].
pub const IGNORED_EXTENSIONS: [&str; 91] = [
    "3gp", "7z", "7zip", "aac", "ai", "aiff", "apk", "asf", "asx", "au", "avi", "bat", "bin",
    "bmp", "bz2", "cdr", "cpl", "css", "dmg", "doc", "docb", "docm", "docx", "dotm", "dotx", "drw",
    "dxf", "eps", "exe", "flv", "gif", "hta", "ico", "iso", "jar", "jpeg", "jpg", "js", "m4a",
    "m4v", "mid", "mng", "mov", "mp3", "mp4", "mpg", "msi", "msp", "odg", "odp", "ods", "odt",
    "ogg", "pct", "pdf", "png", "potm", "potx", "pps", "ppt", "pptm", "pptx", "ps", "psp", "pst",
    "py", "qt", "ra", "rar", "rb", "rm", "rss", "sh", "svg", "swf", "tar", "tar.gz", "tif", "tiff",
    "wav", "webm", "webp", "wma", "wmv", "xls", "xlsm", "xlsx", "xltm", "xltx", "xz", "zip",
];

/// The default `tags`: `("a", "area")`.
pub const DEFAULT_TAGS: [&str; 2] = ["a", "area"];

/// The default `attrs`: `("href",)`.
pub const DEFAULT_ATTRS: [&str; 1] = ["href"];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `w3lib.html.strip_html5_whitespace` — the five HTML5 space characters only.
///
/// ```
/// use browser_templates::links::strip_html5_whitespace;
///
/// assert_eq!(strip_html5_whitespace("  /spaced\n"), "/spaced");
/// assert_eq!(strip_html5_whitespace("\x0c/ff\r"), "/ff");
/// // `str::trim` would eat the non-breaking space; this must not.
/// assert_eq!(strip_html5_whitespace("\u{a0}/nbsp"), "\u{a0}/nbsp");
/// ```
#[must_use]
pub fn strip_html5_whitespace(value: &str) -> &str {
    value.trim_matches(|c| HTML5_SPACE.contains(&c))
}

/// Every suffix chain of the last path segment, longest first.
///
/// Python's `_url_extensions`:
///
/// ```text
/// path  = urlsplit(url).path
/// last  = path.rpartition("/")[2]
/// if "." not in last: return ()
/// parts = last.lower().split(".")
/// return tuple(".".join(parts[i:]) for i in range(1, len(parts)) if parts[i])
/// ```
///
/// ```
/// use browser_templates::links::url_extensions;
///
/// assert_eq!(url_extensions("https://a.test/d.tar.gz"), ["tar.gz", "gz"]);
/// assert_eq!(url_extensions("https://a.test/d.gz"), ["gz"]);
/// assert!(url_extensions("https://a.test/index").is_empty());
/// assert_eq!(url_extensions("https://a.test/a.b.c.d"), ["b.c.d", "c.d", "d"]);
/// // An empty segment is skipped as a starting point, so `a..gz` yields
/// // only `gz` — the `".gz"` chain that would start at the empty part is
/// // filtered out by Python's `if parts[i]`.
/// assert_eq!(url_extensions("https://a.test/a..gz"), ["gz"]);
/// // A dotfile's whole name is its extension, and a trailing dot survives.
/// assert_eq!(url_extensions("https://a.test/.hidden"), ["hidden"]);
/// assert_eq!(url_extensions("https://a.test/a.b."), ["b."]);
/// // `urlsplit`, not `urlparse`: a `;params` suffix stays part of the last
/// // segment, so a `;jsessionid=` defeats the extension deny-list exactly as
/// // it does in Python.
/// assert_eq!(
///     url_extensions("https://a.test/f.pdf;jsessionid=1"),
///     ["pdf;jsessionid=1"],
/// );
/// ```
#[must_use]
pub fn url_extensions(url: &str) -> Vec<String> {
    // `urlsplit(url).path` — **not** `urlparse(url).path`, which peels off
    // `;params`. The difference is visible on a perfectly ordinary URL:
    // `/report.pdf;jsessionid=…` has the single extension
    // `"pdf;jsessionid=…"`, which is not in `IGNORED_EXTENSIONS`, so Python
    // *keeps* the link. Reading it as `"pdf"` would drop it. The u18 fuzz
    // caught this on 10 of 1500 random URLs.
    let (_, _, path, _, _) = split_url(url);
    let last = path.rsplit('/').next().unwrap_or("");
    if !last.contains('.') {
        return Vec::new();
    }
    let lowered = last.to_lowercase();
    let parts: Vec<&str> = lowered.split('.').collect();
    (1..parts.len())
        .filter(|index| !parts[*index].is_empty())
        .map(|index| parts[index..].join("."))
        .collect()
}

// ---------------------------------------------------------------------------
// safe_url_string
// ---------------------------------------------------------------------------

/// C0 controls and space, which `w3lib.url._strip` removes from both ends.
const C0_CONTROL_OR_SPACE: fn(char) -> bool = |c| c <= '\u{20}';

/// `_PATH_SAFEST_CHARS` — `_safe_chars` minus `#`, `[`, `]` and `|`.
///
/// `_safe_chars` is RFC 3986's reserved + unreserved sets plus `|` and `%`.
/// `_QUERY_SAFEST_CHARS` and `_FRAGMENT_SAFEST_CHARS` are the same set.
const PATH_SAFEST: &[u8] = b":/?@!$&'()*+,;=-._~%";

/// `_SPECIAL_QUERY_SAFEST_CHARS` — [`PATH_SAFEST`] minus `'`.
const SPECIAL_QUERY_SAFEST: &[u8] = b":/?@!$&()*+,;=-._~%";

/// `_USERINFO_SAFEST_CHARS` — RFC 3986 userinfo-safe minus `:`, `;` and `=`.
const USERINFO_SAFEST: &[u8] = b"!$&'()*+,-._~";

/// The WHATWG "special" schemes, whose query gets the stricter safe set.
const SPECIAL_SCHEMES: [&str; 6] = ["ftp", "file", "http", "https", "ws", "wss"];

/// CPython's `urllib.parse.uses_netloc`, minus the `""` entry that its guard
/// (`if scheme and scheme in uses_netloc`) can never reach.
///
/// This list decides whether `urlunsplit` writes an empty authority as `//`.
/// `safe_url_string("http:/x")` is `"http:///x"` because `http` is here, and
/// `safe_url_string("mailto:/x")` is `"mailto:/x"` because `mailto` is not.
const USES_NETLOC: [&str; 26] = [
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

/// Why [`safe_url_string`] gave up. Python raises `ValueError`; the caller logs
/// at debug and drops the URL.
///
/// The variants are finer-grained than Python's single `ValueError` only so a
/// log line can say what happened; every one of them is a `ValueError` on the
/// other side.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SafeUrlError {
    /// The netloc had an unbalanced `[` or `]`, or data before the `[`, or
    /// data between the `]` and the `:port` — CPython's `_urlsplit` and
    /// `_check_bracketed_netloc` both raise `ValueError("Invalid IPv6 URL")`.
    InvalidIpv6,
    /// A bracketed host was neither a valid IPv6 literal nor a well-formed
    /// `IPvFuture` — CPython's `_check_bracketed_host` raises from
    /// `ipaddress.ip_address`. A bracketed **IPv4** address lands here too:
    /// CPython rejects it explicitly.
    InvalidBracketedHost,
    /// The `:port` was not a decimal number in `0..=65535` — CPython raises
    /// from the `port` property.
    InvalidPort,
}

impl std::fmt::Display for SafeUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIpv6 => f.write_str("Invalid IPv6 URL"),
            Self::InvalidBracketedHost => f.write_str("invalid bracketed host"),
            Self::InvalidPort => f.write_str("invalid port"),
        }
    }
}

/// `w3lib.url.safe_url_string(url, encoding)`.
///
/// Percent-encodes every character that is not legal in a URI, IDNA-encodes the
/// host, and returns `Err` on a URL it cannot make safe.
///
/// # Divergences, both declared
///
/// * **`encoding` is ignored for the non-ASCII → bytes step.** Python encodes
///   the query and fragment with the *page's* charset before percent-encoding,
///   so a `é` in a windows-1252 page becomes `%E9` rather than `%C3%A9`.
///   Reproducing that needs the full `encoding_rs` encoder chain in a crate
///   that does not depend on it; the port always encodes UTF-8, which is what
///   every modern page uses. The parameter is kept so the signature — and the
///   fix — stay in reach.
/// * **IDNA is UTS-46 non-transitional**, not CPython's IDNA 2003, exactly as
///   `browser_spider::canonicalize` documents. They agree except on `ß`, `ς`,
///   ZWJ and ZWNJ.
///
/// # Errors
///
/// [`SafeUrlError`] for every input CPython's `urlsplit` rejects: an unbalanced
/// or misplaced bracket, a bracketed host that is not an IPv6 or `IPvFuture`
/// literal, and a `:port` that is not `0..=65535` in ASCII digits. This is the
/// one place the port reproduces those raises rather than answering
/// best-effort, because `LinkExtractor.extract` **catches** them — see
/// `SafeUrlError`.
///
/// The one check that is not reproduced is `_checknetloc`, which raises for a
/// non-ASCII netloc whose NFKC normalisation contains `/?#@:` (`\u{2100}`
/// expands to `a/c`). It needs a Unicode normaliser and this crate has none;
/// no fuzz input reached it.
///
/// ```
/// use browser_templates::links::safe_url_string;
///
/// assert_eq!(safe_url_string("https://a.test/a b", "utf-8")?, "https://a.test/a%20b");
/// assert_eq!(safe_url_string("https://a.test/x?q=1", "utf-8")?, "https://a.test/x?q=1");
/// // `urlunsplit` writes an empty authority as `//` for a `uses_netloc`
/// // scheme, and leaves it out for any other.
/// assert_eq!(safe_url_string("http:/x", "utf-8")?, "http:///x");
/// assert_eq!(safe_url_string("mailto:/x", "utf-8")?, "mailto:/x");
/// # Ok::<(), browser_templates::links::SafeUrlError>(())
/// ```
pub fn safe_url_string(url: &str, encoding: &str) -> std::result::Result<String, SafeUrlError> {
    let _ = encoding;
    let stripped = strip_url(url);
    let (scheme, netloc, path, query, fragment) = split_url(&stripped);

    let (userinfo, host, port) = split_netloc(&netloc)?;

    let mut authority = String::new();
    if let Some(userinfo) = userinfo {
        let (username, password) = match userinfo.as_str().split_once(':') {
            Some((user, pass)) => (user, Some(pass)),
            None => (userinfo.as_str(), None),
        };
        authority.push_str(&quote(&unquote(username), USERINFO_SAFEST));
        if let Some(password) = password {
            authority.push(':');
            authority.push_str(&quote(&unquote(password), USERINFO_SAFEST));
        }
        authority.push('@');
    }
    if !host.is_empty() {
        if host.contains(':') {
            // `urlsplit` strips the brackets off an IPv6 hostname; they are
            // required again when the netloc is rebuilt.
            authority.push('[');
            authority.push_str(&host);
            authority.push(']');
        } else {
            authority.push_str(&idna_host(&host));
        }
    }
    if let Some(port) = port {
        authority.push(':');
        authority.push_str(&port.to_string());
    }

    let safe_query = if SPECIAL_SCHEMES.contains(&scheme.as_str()) {
        SPECIAL_QUERY_SAFEST
    } else {
        PATH_SAFEST
    };

    let mut out = String::with_capacity(stripped.len() + 8);
    if !scheme.is_empty() {
        out.push_str(&scheme);
        out.push(':');
    }
    // CPython's `urlunsplit` decides the `//` marker from the **rebuilt**
    // netloc — `netloc_bytes.decode()`, which is what `authority` is here, not
    // the netloc `urlsplit` parsed. `http://:/x` rebuilds to an *empty*
    // authority (no username, no hostname, no port), so it is the rule below
    // and not the original `":"` that decides.
    //
    // ```text
    // if not netloc:
    //     if scheme and scheme in uses_netloc and (not url or url[:1] == "/"):
    //         netloc = ""          # -> "//" is written with nothing after it
    //     else:
    //         netloc = None        # -> no "//" at all
    // ```
    //
    // That is why `safe_url_string("http:/x")` is `"http:///x"` while
    // `safe_url_string("mailto:/x")` is `"mailto:/x"`, and why `"ftp://?q"`
    // keeps its `//`. The u18 fuzz caught the port writing `"http:/x"` for the
    // first of those on 146 of 1500 random URLs.
    let quoted_path = quote(&path, PATH_SAFEST);
    let has_netloc = !authority.is_empty()
        || (!scheme.is_empty()
            && USES_NETLOC.contains(&scheme.as_str())
            && (quoted_path.is_empty() || quoted_path.starts_with('/')));
    if has_netloc {
        out.push_str("//");
        out.push_str(&authority);
        // `if url and url[:1] != "/": url = "/" + url`. Unreachable from
        // `urlsplit`, which never leaves a relative path beside a netloc, but
        // it is one line and it is what `_urlunsplit` says.
        if !quoted_path.is_empty() && !quoted_path.starts_with('/') {
            out.push('/');
        }
    } else if quoted_path.starts_with("//") {
        // `elif url[:2] == "//": url = "//" + url` — a path that would
        // otherwise be re-read as an authority gets a third slash's worth of
        // separation. Reachable only for a scheme outside `uses_netloc`, e.g.
        // `mailto:////x`.
        out.push_str("//");
    }
    out.push_str(&quoted_path);
    if !query.is_empty() {
        out.push('?');
        out.push_str(&quote(&query, safe_query));
    }
    if !fragment.is_empty() {
        out.push('#');
        out.push_str(&quote(&fragment, PATH_SAFEST));
    }
    Ok(out)
}

/// `w3lib.url._strip`: drop C0-or-space from both ends, delete every tab, CR
/// and LF anywhere.
fn strip_url(url: &str) -> String {
    url.trim_matches(C0_CONTROL_OR_SPACE)
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// CPython's `urlsplit`, reduced to what [`safe_url_string`] and
/// [`url_extensions`] need.
///
/// [`browser_spider::canonicalize::urlparse`] is the full port, but it also
/// peels off `;params`, which neither caller here must do — both use
/// `urlsplit`, not `urlparse`.
///
/// `_urlsplit` starts by left-stripping C0-or-space and deleting every tab, CR
/// and LF anywhere in the URL. [`strip_url`] has already done a superset of
/// that for `safe_url_string`, but [`url_extensions`] hands over the raw URL,
/// so it happens here.
///
/// **Declared divergence:** CPython raises `ValueError` for an unbalanced
/// bracket in the netloc and for an NFKC-hostile one. This is total, following
/// the workspace-wide decision recorded in `docs/DIVERGENCES.md` §"Unit 15"
/// item 2 — `safe_url_string` re-derives the bracket errors itself in
/// [`split_netloc`], so only [`url_extensions`] is left unable to fail.
fn split_url(url: &str) -> (String, String, String, String, String) {
    let lstripped = url.trim_start_matches(C0_CONTROL_OR_SPACE);
    // Almost every URL is already clean, and `url_passes` calls this for every
    // link on every page, so the copy is worth avoiding.
    let cleaned: Cow<'_, str> = if lstripped.contains(['\t', '\n', '\r']) {
        Cow::Owned(
            lstripped
                .chars()
                .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
                .collect(),
        )
    } else {
        Cow::Borrowed(lstripped)
    };
    let mut rest = cleaned.as_ref();
    let mut fragment = String::new();
    if let Some((head, tail)) = rest.split_once('#') {
        fragment = tail.to_owned();
        rest = head;
    }
    let mut query = String::new();
    if let Some((head, tail)) = rest.split_once('?') {
        query = tail.to_owned();
        rest = head;
    }
    let mut scheme = String::new();
    if let Some((candidate, tail)) = rest.split_once(':') {
        if !candidate.is_empty()
            && candidate.starts_with(|c: char| c.is_ascii_alphabetic())
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            scheme = candidate.to_ascii_lowercase();
            rest = tail;
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_owned();
        rest = &after[end..];
    }
    (scheme, netloc, rest.to_owned(), query, fragment)
}

/// Split an authority into `(userinfo, hostname, port)`, the way CPython's
/// `SplitResult.username` / `.hostname` / `.port` do.
///
/// The hostname is lower-cased and, for an IPv6 literal, unbracketed.
type NetlocParts = (Option<String>, String, Option<u16>);

fn split_netloc(netloc: &str) -> std::result::Result<NetlocParts, SafeUrlError> {
    // `_urlsplit` checks bracket balance across the **whole** netloc — the
    // userinfo included — before anything is split off it.
    let has_open = netloc.contains('[');
    let has_close = netloc.contains(']');
    if has_open != has_close {
        return Err(SafeUrlError::InvalidIpv6);
    }
    if has_open {
        check_bracketed_netloc(netloc)?;
    }

    let (userinfo, hostinfo) = match netloc.rsplit_once('@') {
        Some((user, host)) => (Some(user.to_owned()), host),
        None => (None, netloc),
    };

    // `_hostinfo` splits on the **first** `:`, not the last, so `a:1:2` has the
    // host `a` and the port `1:2` — which then fails to be an integer. Reading
    // it as `a:1` + `2` would silently rewrite the URL into an IPv6 literal.
    let (host, port_text) = match hostinfo.split_once('[') {
        Some((_, rest)) => {
            // `before` is guaranteed empty by `check_bracketed_netloc`.
            let (inside, after) = rest.split_once(']').unwrap_or((rest, ""));
            (inside.to_owned(), after.split_once(':').map(|(_, p)| p))
        }
        None => match hostinfo.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (hostinfo.to_owned(), None),
        },
    };

    let port = match port_text {
        // `urlsplit("http://a:/x").port` is `None`, not an error: `_hostinfo`
        // ends with `if not port: port = None`.
        None | Some("") => None,
        Some(text) => Some(parse_port(text)?),
    };
    Ok((userinfo, lowercase_host(&host), port))
}

/// The `port` property: `port.isdigit() and port.isascii()`, then `int(port)`,
/// then `0 <= port <= 65535`.
///
/// Not `str::parse::<u16>`, which accepts a leading `+` that CPython's
/// `isdigit()` gate rejects.
fn parse_port(text: &str) -> std::result::Result<u16, SafeUrlError> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SafeUrlError::InvalidPort);
    }
    // `int()` has no width limit; the range check is what rejects a big one.
    let value: u64 = text.parse().map_err(|_| SafeUrlError::InvalidPort)?;
    u16::try_from(value).map_err(|_| SafeUrlError::InvalidPort)
}

/// The `hostname` property: lower-case, but **only** up to the `%` that starts
/// an IPv6 zone id, which must not be lower-cased.
///
/// CPython's comment cites `http://[fe80::822a:a8ff:fe49:470c%tESt]:1234/keys`.
fn lowercase_host(host: &str) -> String {
    match host.split_once('%') {
        Some((address, zone)) => format!("{}%{zone}", address.to_lowercase()),
        None => host.to_lowercase(),
    }
}

/// CPython's `_check_bracketed_netloc`, run whenever the netloc has both
/// brackets.
fn check_bracketed_netloc(netloc: &str) -> std::result::Result<(), SafeUrlError> {
    let hostinfo = netloc.rsplit('@').next().unwrap_or(netloc);
    let Some((before, bracketed)) = hostinfo.split_once('[') else {
        // No bracket left in the host half — the brackets were in the
        // userinfo. `_check_bracketed_host` still runs on the whole host.
        let host = hostinfo.split_once(':').map_or(hostinfo, |(host, _)| host);
        return check_bracketed_host(host);
    };
    // "No data is allowed before a bracket."
    if !before.is_empty() {
        return Err(SafeUrlError::InvalidIpv6);
    }
    let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
    // "No data is allowed after the bracket but before the port delimiter."
    if !after.is_empty() && !after.starts_with(':') {
        return Err(SafeUrlError::InvalidIpv6);
    }
    check_bracketed_host(host)
}

/// CPython's `_check_bracketed_host`: an `IPvFuture` literal, or an address
/// that `ipaddress.ip_address` accepts and that is **not** IPv4.
fn check_bracketed_host(host: &str) -> std::result::Result<(), SafeUrlError> {
    if let Some(rest) = host.strip_prefix('v') {
        // `\Av[a-fA-F0-9]+\..+\z`.
        let Some((hex, tail)) = rest.split_once('.') else {
            return Err(SafeUrlError::InvalidBracketedHost);
        };
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) || tail.is_empty() {
            return Err(SafeUrlError::InvalidBracketedHost);
        }
        return Ok(());
    }
    // `ipaddress.ip_address` accepts a scoped IPv6 address (`::1%eth0`); the
    // zone must be present and must not contain a second `%`.
    let address = match host.split_once('%') {
        Some((_, zone)) if zone.is_empty() || zone.contains('%') => {
            return Err(SafeUrlError::InvalidBracketedHost)
        }
        Some((address, _)) => address,
        None => host,
    };
    if address.parse::<std::net::Ipv6Addr>().is_ok() {
        Ok(())
    } else {
        // Either it is not an address at all, or it is IPv4 — and CPython
        // rejects "An IPv4 address cannot be in brackets" as loudly as it
        // rejects nonsense.
        Err(SafeUrlError::InvalidBracketedHost)
    }
}

/// `hostname.encode("idna")`, falling back to the raw host when IDNA refuses
/// (CPython catches `UnicodeError` and encodes with the page charset instead).
fn idna_host(host: &str) -> String {
    if host.is_ascii() {
        return host.to_owned();
    }
    // `url::Host::parse` runs UTS-46 `domain_to_ascii`. CPython's
    // `str.encode("idna")` is IDNA 2003; they agree on everything except `ß`,
    // `ς`, ZWJ and ZWNJ, which is the same divergence
    // `browser_spider::canonicalize` already declares.
    match url::Host::parse(host) {
        Ok(url::Host::Domain(ascii)) => ascii,
        Ok(other) => other.to_string(),
        Err(_) => host.to_owned(),
    }
}

/// CPython's `urllib.parse.quote(string, safe)` over UTF-8 bytes.
///
/// ASCII letters, digits, `_`, `.`, `-` and `~` are always safe; `safe` adds to
/// that set. Everything else becomes `%XX` in upper case.
fn quote(value: &str, safe: &[u8]) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        // CPython's `always_safe` set, plus whatever the caller added.
        let always_safe = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~');
        if always_safe || safe.contains(byte) {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// CPython's `urllib.parse.unquote`, lossily decoded as UTF-8.
///
/// Only ever applied to userinfo, which in practice is ASCII.
fn unquote(value: &str) -> String {
    if !value.contains('%') {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// LinkExtractor
// ---------------------------------------------------------------------------

/// Python's `process=` callable: applied to each absolutised URL, returning
/// `None` to drop it (Python's "falsy" is exactly "drop").
pub type ProcessFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Extracts and filters URLs from a [`Response`], or from a single URL via
/// [`LinkExtractor::matches`].
///
/// All matching is regex-based. Build one with [`LinkExtractor::builder`];
/// Python's thirteen keyword arguments are the builder's thirteen methods.
///
/// `Clone` is cheap: the compiled patterns and the `process` closure are behind
/// `Arc`, so a `rules()` implementation that rebuilds its `Vec<CrawlRule>` on
/// every response — which is what Python does — costs a refcount bump.
#[derive(Clone)]
pub struct LinkExtractor {
    allow: Arc<Vec<Regex>>,
    deny: Arc<Vec<Regex>>,
    allow_domains: Arc<Vec<String>>,
    deny_domains: Arc<Vec<String>>,
    restrict_css: Arc<Vec<String>>,
    restrict_xpath: Arc<Vec<String>>,
    tags: Arc<Vec<String>>,
    attrs: Arc<Vec<String>>,
    canonicalize: bool,
    strip: bool,
    keep_fragment: bool,
    deny_extensions: Arc<HashSet<String>>,
    process: Option<ProcessFn>,
}

impl std::fmt::Debug for LinkExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkExtractor")
            .field(
                "allow",
                &self.allow.iter().map(Regex::as_str).collect::<Vec<_>>(),
            )
            .field(
                "deny",
                &self.deny.iter().map(Regex::as_str).collect::<Vec<_>>(),
            )
            .field("allow_domains", &self.allow_domains)
            .field("deny_domains", &self.deny_domains)
            .field("restrict_css", &self.restrict_css)
            .field("restrict_xpath", &self.restrict_xpath)
            .field("tags", &self.tags)
            .field("attrs", &self.attrs)
            .field("canonicalize", &self.canonicalize)
            .field("strip", &self.strip)
            .field("keep_fragment", &self.keep_fragment)
            .field("deny_extensions", &self.deny_extensions.len())
            .field("process", &self.process.is_some())
            .finish()
    }
}

impl Default for LinkExtractor {
    /// `LinkExtractor()` — every default Python has.
    fn default() -> Self {
        LinkExtractorBuilder::new()
            .build()
            .expect("the default builder compiles no patterns")
    }
}

impl LinkExtractor {
    /// Start configuring one. `LinkExtractor::builder().build()` is Python's
    /// no-argument `LinkExtractor()`.
    #[must_use]
    pub fn builder() -> LinkExtractorBuilder {
        LinkExtractorBuilder::new()
    }

    /// Absolute, filtered, de-duplicated URLs from `response`, in first-seen
    /// order.
    ///
    /// The pipeline, straight out of `spider-support.md` §2.4:
    ///
    /// 1. scope the search to `restrict_xpath` then `restrict_css` — falling
    ///    back to the whole document whenever those produce **no scopes**,
    ///    which includes both "neither was set" and "both were set and matched
    ///    nothing";
    /// 2. read `tags` × `attrs` in document order;
    /// 3. [`strip_html5_whitespace`] when `strip`;
    /// 4. `urljoin` against the response URL;
    /// 5. `process`, which may drop;
    /// 6. `canonicalize_url` when `canonicalize`;
    /// 7. [`safe_url_string`], dropping the URL on failure;
    /// 8. [`LinkExtractor::url_passes`];
    /// 9. dedup, keeping the first spelling seen.
    ///
    /// Takes a [`Response`] rather than a `CrawlResponse` so both work — the
    /// latter derefs to the former.
    #[must_use]
    pub fn extract(&self, response: &Response) -> Vec<String> {
        let mut raw: Vec<String> = Vec::new();
        let selector = self.search_selector();

        // XPath scopes first, then CSS scopes — Python's order.
        let mut scopes = Vec::new();
        for expression in self.restrict_xpath.iter() {
            match response.xpath(expression) {
                Ok(nodes) => scopes.extend(nodes.into_vec()),
                Err(error) => {
                    tracing::debug!("restrict_xpath {expression:?} failed: {error}");
                }
            }
        }
        for expression in self.restrict_css.iter() {
            match response.css(expression) {
                Ok(nodes) => scopes.extend(nodes.into_vec()),
                Err(error) => {
                    tracing::debug!("restrict_css {expression:?} failed: {error}");
                }
            }
        }

        if scopes.is_empty() {
            // `if not scopes: scopes = [response]` — the fallback is on the
            // **result**, not on whether the options were set. A `restrict_css`
            // that matches nothing therefore searches the whole document, which
            // reads like a bug and is Python's behaviour; the u18 differential
            // caught the port getting this wrong on four pages.
            //
            // `Response::xpath` evaluates from the document node, and `.//a`
            // from there reaches the same elements lxml's
            // `response._root.xpath` does from the root element.
            match response.xpath(&selector) {
                Ok(nodes) => raw.extend(nodes.texts()),
                Err(error) => {
                    tracing::debug!("LinkExtractor selector {selector:?} failed: {error}");
                }
            }
        } else {
            for scope in &scopes {
                match scope.xpath(&selector) {
                    Ok(nodes) => raw.extend(nodes.texts()),
                    Err(error) => {
                        tracing::debug!("LinkExtractor selector {selector:?} failed: {error}");
                    }
                }
            }
        }

        let mut out: Vec<String> = Vec::with_capacity(raw.len());
        for value in raw {
            if let Some(url) = self.refine(&value, response) {
                if !out.contains(&url) {
                    out.push(url);
                }
            }
        }
        out
    }

    /// Steps 3–8 for one raw attribute value. `None` means "dropped".
    fn refine(&self, value: &str, response: &Response) -> Option<String> {
        // `if not url: continue` — an empty attribute never reaches urljoin.
        if value.is_empty() {
            return None;
        }
        let trimmed = if self.strip {
            let trimmed = strip_html5_whitespace(value);
            if trimmed.is_empty() {
                return None;
            }
            trimmed
        } else {
            value
        };

        let mut url = response.urljoin(trimmed);
        if let Some(process) = &self.process {
            url = process(&url)?;
            if url.is_empty() {
                return None;
            }
        }
        if self.canonicalize {
            url = canonicalize_url(&url, self.keep_fragment);
        }
        let url = match safe_url_string(&url, &response.encoding) {
            Ok(url) => url,
            Err(error) => {
                tracing::debug!("Skipping the extraction of bad URL {url:?}: {error}");
                return None;
            }
        };
        if !self.url_passes(&url) {
            return None;
        }
        Some(url)
    }

    /// URL-only filter, with no response to extract from.
    ///
    /// Canonicalizes first (so `allow = r"a=1&b=2$"` matches `?b=2&a=1`) and
    /// then applies [`LinkExtractor::url_passes`]. No stripping, no `urljoin`,
    /// no `process`, no [`safe_url_string`] — this is
    /// [`crate::sitemap::SitemapSpider`]'s rule router.
    #[must_use]
    pub fn matches(&self, url: &str) -> bool {
        let candidate: Cow<'_, str> = if self.canonicalize {
            Cow::Owned(canonicalize_url(url, self.keep_fragment))
        } else {
            Cow::Borrowed(url)
        };
        self.url_passes(&candidate)
    }

    /// The allow/deny gauntlet, in Python's order: scheme → extension → allow →
    /// deny → domains.
    ///
    /// The scheme test is deliberately **string-prefix based**, not a URL parse:
    /// Python writes `url.split("://", 1)[0]`, so `mailto:x@y` (which has no
    /// `://`) yields the whole string and is rejected. Replacing it with a
    /// parser would change which URLs pass.
    #[must_use]
    pub fn url_passes(&self, url: &str) -> bool {
        let scheme = url.split("://").next().unwrap_or(url);
        if !VALID_SCHEMES.contains(&scheme) {
            return false;
        }

        if !self.deny_extensions.is_empty() {
            let extensions = url_extensions(url);
            if extensions
                .iter()
                .any(|extension| self.deny_extensions.contains(extension))
            {
                return false;
            }
        }

        // `p.search(url)` — unanchored, which is `Regex::is_match`.
        if !self.allow.is_empty() && !self.allow.iter().any(|rule| rule.is_match(url)) {
            return false;
        }
        if self.deny.iter().any(|rule| rule.is_match(url)) {
            return false;
        }

        if !self.allow_domains.is_empty() || !self.deny_domains.is_empty() {
            let host = hostname_of(url);
            if !self.allow_domains.is_empty() && !domain_matches(&host, &self.allow_domains) {
                return false;
            }
            if domain_matches(&host, &self.deny_domains) {
                return false;
            }
        }
        true
    }

    /// `"| ".join(f".//{tag}/@{attr}" for tag in tags for attr in attrs)`.
    ///
    /// The leading space after each `|` is Python's, and is harmless in XPath.
    ///
    /// **Declared divergence.** An extractor with no `tags`, or no `attrs`,
    /// produces the empty expression — and `lxml.xpath("")` raises
    /// `XPathEvalError`, taking the whole callback down over a configuration
    /// that plainly means "look nowhere". [`LinkExtractor::extract`] returns no
    /// links instead. Twelve rows of `testdata/u18_links.jsonl` pin it, and the
    /// replay asserts it keeps differing.
    fn search_selector(&self) -> String {
        let mut parts = Vec::with_capacity(self.tags.len() * self.attrs.len());
        for tag in self.tags.iter() {
            for attr in self.attrs.iter() {
                parts.push(format!(".//{tag}/@{attr}"));
            }
        }
        parts.join("| ")
    }

    /// The compiled `allow` patterns, for tests and for a rule that wants to
    /// explain itself.
    #[must_use]
    pub fn allow_patterns(&self) -> &[Regex] {
        &self.allow
    }

    /// The compiled `deny` patterns.
    #[must_use]
    pub fn deny_patterns(&self) -> &[Regex] {
        &self.deny
    }

    /// The extensions this extractor drops.
    #[must_use]
    pub fn deny_extensions(&self) -> &HashSet<String> {
        &self.deny_extensions
    }
}

/// `urlsplit(url).hostname` — lower-case, no port, no userinfo.
///
/// Deliberately **not** the netloc: every other domain check in the crawl core
/// (`Request::domain`, the AutoThrottle key, the robots cache key, the engine's
/// `allowed_domains` filter) uses the netloc, port included.
/// `LinkExtractor`'s does not. `spider-support.md` §14.6 item 4.
#[must_use]
pub fn hostname_of(url: &str) -> String {
    let netloc = urlparse(url).netloc;
    let hostport = netloc.rsplit('@').next().unwrap_or(&netloc);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    host.to_lowercase()
}

/// `host == d or host.endswith("." + d)` for any `d`.
fn domain_matches(host: &str, domains: &[String]) -> bool {
    domains
        .iter()
        .any(|domain| host == domain || host.ends_with(&format!(".{domain}")))
}

// ---------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------

/// Python's thirteen `LinkExtractor` keyword arguments, one method each.
///
/// Every method takes `self`, so a call chain reads like the Python
/// constructor:
///
/// ```
/// use browser_templates::links::LinkExtractor;
///
/// let extractor = LinkExtractor::builder()
///     .allow([r"/posts/"])
///     .deny([r"/posts/2"])
///     .allow_domains(["example.com"])
///     .canonicalize(false)
///     .build()?;
/// assert!(extractor.url_passes("https://api.example.com/posts/1"));
/// assert!(!extractor.url_passes("https://api.example.com/posts/2"));
/// assert!(!extractor.url_passes("https://other.com/posts/1"));
/// # Ok::<(), browser_templates::error::TemplateError>(())
/// ```
#[derive(Clone, Default)]
pub struct LinkExtractorBuilder {
    allow: Vec<String>,
    deny: Vec<String>,
    allow_domains: Vec<String>,
    deny_domains: Vec<String>,
    restrict_css: Vec<String>,
    restrict_xpath: Vec<String>,
    tags: Option<Vec<String>>,
    attrs: Option<Vec<String>>,
    canonicalize: Option<bool>,
    strip: Option<bool>,
    keep_fragment: Option<bool>,
    deny_extensions: Option<Vec<String>>,
    process: Option<ProcessFn>,
}

impl std::fmt::Debug for LinkExtractorBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkExtractorBuilder")
            .field("allow", &self.allow)
            .field("deny", &self.deny)
            .field("allow_domains", &self.allow_domains)
            .field("deny_domains", &self.deny_domains)
            .field("restrict_css", &self.restrict_css)
            .field("restrict_xpath", &self.restrict_xpath)
            .field("tags", &self.tags)
            .field("attrs", &self.attrs)
            .field("canonicalize", &self.canonicalize)
            .field("strip", &self.strip)
            .field("keep_fragment", &self.keep_fragment)
            .field("deny_extensions", &self.deny_extensions)
            .field("process", &self.process.is_some())
            .finish()
    }
}

/// Collect any `&str`-ish iterable into owned strings.
fn to_strings<I, S>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    values.into_iter().map(Into::into).collect()
}

impl LinkExtractorBuilder {
    /// A builder holding every Python default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `allow=` — patterns a URL must match. Empty means match all.
    #[must_use]
    pub fn allow<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow = to_strings(patterns);
        self
    }

    /// `deny=` — patterns a URL must **not** match. Beats `allow`.
    #[must_use]
    pub fn deny<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny = to_strings(patterns);
        self
    }

    /// `allow_domains=` — the exact host, or any subdomain of it.
    #[must_use]
    pub fn allow_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_domains = to_strings(domains);
        self
    }

    /// `deny_domains=` — same matching, opposite verdict.
    #[must_use]
    pub fn deny_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_domains = to_strings(domains);
        self
    }

    /// `restrict_css=` — CSS selectors scoping the DOM search.
    #[must_use]
    pub fn restrict_css<I, S>(mut self, selectors: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.restrict_css = to_strings(selectors);
        self
    }

    /// `restrict_xpath=` — XPath expressions scoping the DOM search. Applied
    /// **before** `restrict_css`.
    #[must_use]
    pub fn restrict_xpath<I, S>(mut self, expressions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.restrict_xpath = to_strings(expressions);
        self
    }

    /// `tags=` — the element names URLs are read from. Default `("a", "area")`.
    #[must_use]
    pub fn tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = Some(to_strings(tags));
        self
    }

    /// `attrs=` — the attributes URLs are read from. Default `("href",)`.
    #[must_use]
    pub fn attrs<I, S>(mut self, attrs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.attrs = Some(to_strings(attrs));
        self
    }

    /// `canonicalize=` — sort query parameters and normalise the path. Default
    /// `true`.
    #[must_use]
    pub fn canonicalize(mut self, enabled: bool) -> Self {
        self.canonicalize = Some(enabled);
        self
    }

    /// `strip=` — [`strip_html5_whitespace`] the raw attribute value. Default
    /// `true`.
    #[must_use]
    pub fn strip(mut self, enabled: bool) -> Self {
        self.strip = Some(enabled);
        self
    }

    /// `keep_fragment=` — keep `#fragment` when canonicalizing. Default
    /// `false`.
    #[must_use]
    pub fn keep_fragment(mut self, enabled: bool) -> Self {
        self.keep_fragment = Some(enabled);
        self
    }

    /// `deny_extensions=` — **replaces** [`IGNORED_EXTENSIONS`].
    ///
    /// Leaving it unset is Python's `None` (use the 91-entry default); passing
    /// an empty iterable is Python's `()` and denies **nothing**, which is how
    /// `tags=("link",)` can extract a stylesheet. Each entry is lower-cased and
    /// has a leading `.` stripped.
    #[must_use]
    pub fn deny_extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_extensions = Some(to_strings(extensions));
        self
    }

    /// `process=` — run on each absolutised URL; return `None` to drop it.
    #[must_use]
    pub fn process<F>(mut self, process: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        self.process = Some(Arc::new(process));
        self
    }

    /// Compile the patterns and freeze the extractor.
    ///
    /// # Errors
    ///
    /// [`TemplateError::InvalidPattern`] when an `allow` or `deny` pattern does
    /// not compile — Python's `re.compile` raises `re.error` from the
    /// constructor for the same input.
    ///
    /// `regex` is not `re`: it has no backreferences and no lookaround. A
    /// pattern using either compiles in Python and is rejected here, which is a
    /// declared, unavoidable divergence (`spider-support.md` §14.3).
    pub fn build(self) -> Result<LinkExtractor> {
        let compile = |patterns: Vec<String>, field: &'static str| -> Result<Vec<Regex>> {
            patterns
                .into_iter()
                .map(|pattern| {
                    Regex::new(&pattern).map_err(|error| TemplateError::InvalidPattern {
                        field,
                        pattern,
                        message: error.to_string(),
                    })
                })
                .collect()
        };

        let deny_extensions: HashSet<String> = match self.deny_extensions {
            Some(extensions) => extensions
                .into_iter()
                .map(|extension| extension.to_lowercase().trim_start_matches('.').to_owned())
                .collect(),
            None => IGNORED_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_owned())
                .collect(),
        };

        Ok(LinkExtractor {
            allow: Arc::new(compile(self.allow, "allow")?),
            deny: Arc::new(compile(self.deny, "deny")?),
            allow_domains: Arc::new(
                self.allow_domains
                    .iter()
                    .map(|domain| domain.to_lowercase())
                    .collect(),
            ),
            deny_domains: Arc::new(
                self.deny_domains
                    .iter()
                    .map(|domain| domain.to_lowercase())
                    .collect(),
            ),
            restrict_css: Arc::new(self.restrict_css),
            restrict_xpath: Arc::new(self.restrict_xpath),
            tags: Arc::new(
                self.tags
                    .unwrap_or_else(|| to_strings(DEFAULT_TAGS.iter().copied())),
            ),
            attrs: Arc::new(
                self.attrs
                    .unwrap_or_else(|| to_strings(DEFAULT_ATTRS.iter().copied())),
            ),
            canonicalize: self.canonicalize.unwrap_or(true),
            strip: self.strip.unwrap_or(true),
            keep_fragment: self.keep_fragment.unwrap_or(false),
            deny_extensions: Arc::new(deny_extensions),
            process: self.process,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><body>
        <main>
          <a href="/posts/1">one</a>
          <a href="/posts/2">two</a>
          <area href="/area" />
        </main>
        <div id="content"><a href="/content/1">c1</a></div>
        <a href="mailto:x@y.test">mail</a>
        <a href="javascript:void(0)">js</a>
        <a href="/doc.pdf">pdf</a>
        <a href="/about">about</a>
        <a href="  /spaced  ">spaced</a>
        <a href="">empty</a>
        <link rel="stylesheet" href="/style.css" />
        </body></html>"#;

    fn page() -> Response {
        Response::builder(200, "https://example.com/sub/")
            .body(PAGE)
            .build()
    }

    fn extract(builder: LinkExtractorBuilder) -> Vec<String> {
        builder.build().expect("compiles").extract(&page())
    }

    // -- constants ---------------------------------------------------------

    #[test]
    fn ignored_extensions_is_the_ninety_one_python_has_and_is_sorted() {
        assert_eq!(IGNORED_EXTENSIONS.len(), 91);
        let mut sorted = IGNORED_EXTENSIONS;
        sorted.sort_unstable();
        assert_eq!(sorted, IGNORED_EXTENSIONS);
        let unique: HashSet<&&str> = IGNORED_EXTENSIONS.iter().collect();
        assert_eq!(unique.len(), IGNORED_EXTENSIONS.len());
        assert!(IGNORED_EXTENSIONS.contains(&"tar.gz"));
        assert!(!IGNORED_EXTENSIONS.contains(&"html"));
    }

    #[test]
    fn html5_whitespace_is_five_characters_and_not_unicode_whitespace() {
        assert_eq!(strip_html5_whitespace(" \t\n\r\u{0c}x \t\n\r\u{0c}"), "x");
        for wide in ['\u{a0}', '\u{2028}', '\u{2003}', '\u{3000}'] {
            let value = format!("{wide}x{wide}");
            assert_eq!(strip_html5_whitespace(&value), value, "{wide:?}");
            // …and this is exactly where `str::trim` would differ.
            assert_ne!(value.trim(), value, "{wide:?} really is Unicode whitespace");
        }
    }

    #[test]
    fn url_extension_chains_match_python() {
        assert_eq!(url_extensions("https://a.test/d.tar.gz"), ["tar.gz", "gz"]);
        assert_eq!(url_extensions("https://a.test/d.gz"), ["gz"]);
        assert!(url_extensions("https://a.test/index").is_empty());
        assert!(url_extensions("https://a.test/").is_empty());
        assert_eq!(
            url_extensions("https://a.test/a.b.c.d"),
            ["b.c.d", "c.d", "d"]
        );
        assert_eq!(url_extensions("https://a.test/a..gz"), ["gz"]);
        assert_eq!(url_extensions("https://a.test/.hidden"), ["hidden"]);
        assert_eq!(url_extensions("https://a.test/a.b."), ["b."]);
        // The query is not part of the path.
        assert_eq!(url_extensions("https://a.test/x.PDF?a=1"), ["pdf"]);
        // A dot in an earlier segment is not an extension.
        assert!(url_extensions("https://a.test/v1.2/index").is_empty());
    }

    // -- extraction --------------------------------------------------------

    #[test]
    fn defaults_take_a_and_area_only_and_absolutise() {
        let links = extract(LinkExtractor::builder());
        assert_eq!(
            links,
            [
                "https://example.com/posts/1",
                "https://example.com/posts/2",
                "https://example.com/area",
                "https://example.com/content/1",
                "https://example.com/about",
                "https://example.com/spaced",
            ],
            "mailto/javascript by scheme, .pdf/.css by extension, <link> by tag"
        );
    }

    #[test]
    fn relative_hrefs_resolve_against_the_response_url() {
        let response = Response::builder(200, "https://example.com/sub/")
            .body(r#"<a href="foo/bar">x</a>"#)
            .build();
        assert_eq!(
            LinkExtractor::default().extract(&response),
            ["https://example.com/sub/foo/bar"]
        );
    }

    #[test]
    fn allow_selects_and_deny_beats_allow() {
        assert_eq!(
            extract(LinkExtractor::builder().allow([r"/posts/"])),
            ["https://example.com/posts/1", "https://example.com/posts/2"]
        );
        assert_eq!(
            extract(
                LinkExtractor::builder()
                    .allow([r"/posts/"])
                    .deny([r"/posts/2"])
            ),
            ["https://example.com/posts/1"]
        );
    }

    #[test]
    fn allow_domains_matches_subdomains() {
        let extractor = LinkExtractor::builder()
            .allow_domains(["example.com"])
            .build()
            .expect("compiles");
        assert!(extractor.url_passes("https://example.com/x"));
        assert!(extractor.url_passes("https://api.example.com/x"));
        assert!(!extractor.url_passes("https://other.com/x"));
        assert!(!extractor.url_passes("https://notexample.com/x"));
        // The hostname drops the port, unlike every other domain check.
        assert!(extractor.url_passes("https://example.com:8443/x"));
    }

    #[test]
    fn deny_domains_wins_over_allow_domains() {
        let extractor = LinkExtractor::builder()
            .allow_domains(["example.com"])
            .deny_domains(["api.example.com"])
            .build()
            .expect("compiles");
        assert!(extractor.url_passes("https://example.com/x"));
        assert!(!extractor.url_passes("https://api.example.com/x"));
        assert!(!extractor.url_passes("https://deep.api.example.com/x"));
    }

    #[test]
    fn restrict_css_and_restrict_xpath_scope_the_search() {
        assert_eq!(
            extract(LinkExtractor::builder().restrict_css(["main"])),
            [
                "https://example.com/posts/1",
                "https://example.com/posts/2",
                "https://example.com/area",
            ]
        );
        assert_eq!(
            extract(LinkExtractor::builder().restrict_xpath([r#"//div[@id="content"]"#])),
            ["https://example.com/content/1"]
        );
        // Both: xpath scopes first, then css.
        assert_eq!(
            extract(
                LinkExtractor::builder()
                    .restrict_xpath([r#"//div[@id="content"]"#])
                    .restrict_css(["main"])
            ),
            [
                "https://example.com/content/1",
                "https://example.com/posts/1",
                "https://example.com/posts/2",
                "https://example.com/area",
            ]
        );
    }

    /// `if not scopes: scopes = [response]` tests the **result**, not whether
    /// the option was set — so a `restrict_css` that matches nothing searches
    /// the whole document. It reads like a bug and it is Python's behaviour;
    /// the u18 differential caught the port getting it wrong on four pages.
    #[test]
    fn a_restriction_that_matches_nothing_falls_back_to_the_whole_document() {
        let all = extract(LinkExtractor::builder());
        assert_eq!(
            extract(LinkExtractor::builder().restrict_css(["nope"])),
            all
        );
        assert_eq!(
            extract(LinkExtractor::builder().restrict_xpath(["//nope"])),
            all
        );
        assert_eq!(
            extract(
                LinkExtractor::builder()
                    .restrict_css(["nope"])
                    .restrict_xpath(["//nope"])
            ),
            all
        );
    }

    #[test]
    fn tags_and_attrs_and_an_empty_deny_set_reach_the_stylesheet() {
        assert_eq!(
            extract(
                LinkExtractor::builder()
                    .tags(["link"])
                    .attrs(["href"])
                    .deny_extensions(Vec::<String>::new())
            ),
            ["https://example.com/style.css"]
        );
    }

    #[test]
    fn deny_extensions_replaces_the_default_set() {
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="/x.zip">z</a><a href="/y.pdf">p</a>"#)
            .build();
        let extractor = LinkExtractor::builder()
            .deny_extensions(["zip"])
            .build()
            .expect("compiles");
        assert_eq!(extractor.extract(&response), ["https://a.test/y.pdf"]);
    }

    #[test]
    fn a_compound_extension_denies_only_the_compound() {
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="/d.tar.gz">a</a><a href="/d.gz">b</a>"#)
            .build();
        let extractor = LinkExtractor::builder()
            .deny_extensions(["tar.gz"])
            .build()
            .expect("compiles");
        assert_eq!(extractor.extract(&response), ["https://a.test/d.gz"]);
    }

    #[test]
    fn a_leading_dot_and_upper_case_are_normalised_away() {
        let extractor = LinkExtractor::builder()
            .deny_extensions([".ZIP"])
            .build()
            .expect("compiles");
        assert!(extractor.deny_extensions().contains("zip"));
        assert!(!extractor.url_passes("https://a.test/x.zip"));
    }

    #[test]
    fn canonicalization_sorts_the_query_and_drops_the_fragment() {
        let body = r#"<a href="/x?b=2&amp;a=1#f">x</a>"#;
        let response = Response::builder(200, "https://a.test/").body(body).build();

        assert_eq!(
            LinkExtractor::default().extract(&response),
            ["https://a.test/x?a=1&b=2"]
        );
        assert_eq!(
            LinkExtractor::builder()
                .keep_fragment(true)
                .build()
                .expect("compiles")
                .extract(&response),
            ["https://a.test/x?a=1&b=2#f"]
        );
        assert_eq!(
            LinkExtractor::builder()
                .canonicalize(false)
                .build()
                .expect("compiles")
                .extract(&response),
            ["https://a.test/x?b=2&a=1#f"],
            "without canonicalisation nothing sorts and nothing drops the fragment"
        );
    }

    #[test]
    fn dedup_keeps_the_first_spelling() {
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="/x">1</a><a href="/x">2</a><a href="/x?">3</a>"#)
            .build();
        assert_eq!(
            LinkExtractor::default().extract(&response),
            ["https://a.test/x"],
            "canonicalisation collapses `/x?` into `/x`"
        );
    }

    #[test]
    fn strip_is_on_by_default_and_can_be_turned_off() {
        let response = Response::builder(200, "https://example.com/")
            .body("<a href=\"  /spaced  \">x</a>")
            .build();
        assert_eq!(
            LinkExtractor::default().extract(&response),
            ["https://example.com/spaced"]
        );
        // Python gets the same answer without `strip`, because CPython's
        // `urljoin` strips C0-or-space from the relative part on its own. The
        // difference `strip` really makes is the `if not url: continue` guard
        // on an href that is *only* whitespace.
        assert_eq!(
            LinkExtractor::builder()
                .strip(false)
                .build()
                .expect("compiles")
                .extract(&response),
            ["https://example.com/spaced"]
        );
    }

    #[test]
    fn a_whitespace_only_href_is_dropped_by_strip() {
        let response = Response::builder(200, "https://example.com/sub/")
            .body("<a href=\"  \">blank</a><a href=\"/ok\">ok</a>")
            .build();
        assert_eq!(
            LinkExtractor::default().extract(&response),
            ["https://example.com/ok"]
        );
    }

    #[test]
    fn process_can_rewrite_or_drop() {
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="/keep">k</a><a href="/drop">d</a>"#)
            .build();
        let extractor = LinkExtractor::builder()
            .process(|url: &str| {
                if url.contains("/drop") {
                    None
                } else {
                    Some(url.replace("/keep", "/kept"))
                }
            })
            .build()
            .expect("compiles");
        assert_eq!(extractor.extract(&response), ["https://a.test/kept"]);
    }

    #[test]
    fn an_empty_string_from_process_drops_the_url() {
        // Python's `if not url: continue` treats `""` as a drop, not as a URL.
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="/x">x</a>"#)
            .build();
        let extractor = LinkExtractor::builder()
            .process(|_: &str| Some(String::new()))
            .build()
            .expect("compiles");
        assert!(extractor.extract(&response).is_empty());
    }

    // -- matches() ---------------------------------------------------------

    #[test]
    fn matches_canonicalizes_first() {
        let extractor = LinkExtractor::builder()
            .allow([r"a=1&b=2$"])
            .build()
            .expect("compiles");
        assert!(extractor.matches("https://a.test/x?b=2&a=1"));
        assert!(!extractor.clone().matches("https://a.test/x?b=2&a=3"));
    }

    #[test]
    fn matches_honours_every_filter() {
        let extractor = LinkExtractor::builder()
            .allow([r"/posts/"])
            .deny([r"/posts/9"])
            .allow_domains(["example.com"])
            .build()
            .expect("compiles");
        assert!(extractor.matches("https://example.com/posts/1"));
        assert!(!extractor.matches("https://example.com/posts/9"));
        assert!(!extractor.matches("https://other.com/posts/1"));
        assert!(!extractor.matches("ftp://example.com/posts/1"));
        assert!(!extractor.matches("https://example.com/posts/1.pdf"));
        assert!(!extractor.matches("mailto:posts@example.com"));
    }

    #[test]
    fn the_scheme_test_is_a_string_split_not_a_parse() {
        let extractor = LinkExtractor::default();
        assert!(extractor.url_passes("file:///tmp/x"));
        assert!(!extractor.url_passes("ftp://a.test/x"));
        // No "://" at all: the whole string is compared, and never matches.
        assert!(!extractor.url_passes("mailto:x@y"));
        assert!(!extractor.url_passes("file:/x"));
        assert!(!extractor.url_passes("/relative"));
    }

    // -- builder -----------------------------------------------------------

    #[test]
    fn a_bad_pattern_is_a_typed_error_not_a_panic() {
        let error = LinkExtractor::builder()
            .allow(["("])
            .build()
            .expect_err("does not compile");
        assert!(
            matches!(
                &error,
                TemplateError::InvalidPattern { field: "allow", pattern, .. } if pattern == "("
            ),
            "{error:?}"
        );
    }

    #[test]
    fn the_default_extractor_is_the_no_argument_python_one() {
        let extractor = LinkExtractor::default();
        assert!(extractor.allow_patterns().is_empty());
        assert!(extractor.deny_patterns().is_empty());
        assert_eq!(extractor.deny_extensions().len(), 91);
        assert_eq!(extractor.search_selector(), ".//a/@href| .//area/@href");
    }

    /// Declared divergence: `lxml.xpath("")` raises, this returns no links.
    /// Twelve rows of `testdata/u18_links.jsonl` assert it keeps differing.
    #[test]
    fn an_extractor_with_no_tags_finds_nothing_rather_than_erroring() {
        for builder in [
            LinkExtractor::builder().tags(Vec::<String>::new()),
            LinkExtractor::builder().attrs(Vec::<String>::new()),
        ] {
            let extractor = builder.build().expect("compiles");
            assert_eq!(extractor.search_selector(), "");
            assert!(extractor.extract(&page()).is_empty());
        }
    }

    #[test]
    fn process_sees_the_absolutised_url_not_the_raw_href() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let extractor = LinkExtractor::builder()
            .process(move |url: &str| {
                recorder.lock().expect("not poisoned").push(url.to_owned());
                Some(url.to_owned())
            })
            .build()
            .expect("compiles");
        let response = Response::builder(200, "https://example.com/sub/")
            .body(r#"<a href="foo/bar">x</a>"#)
            .build();
        let _ = extractor.extract(&response);
        assert_eq!(
            seen.lock().expect("not poisoned").clone(),
            ["https://example.com/sub/foo/bar"],
            "urljoin runs before `process`, and canonicalisation after it"
        );
    }

    #[test]
    fn a_cloned_extractor_behaves_identically() {
        let extractor = LinkExtractor::builder()
            .allow([r"/posts/"])
            .build()
            .expect("compiles");
        let cloned = extractor.clone();
        assert_eq!(extractor.extract(&page()), cloned.extract(&page()));
        assert_eq!(cloned.allow_patterns().len(), 1);
    }

    #[test]
    fn several_attrs_come_back_in_document_order() {
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="/h1" data-url="/d1">x</a><a data-url="/d2" href="/h2">y</a>"#)
            .build();
        let extractor = LinkExtractor::builder()
            .attrs(["href", "data-url"])
            .build()
            .expect("compiles");
        assert_eq!(
            extractor.extract(&response),
            [
                "https://a.test/h1",
                "https://a.test/d1",
                "https://a.test/d2",
                "https://a.test/h2"
            ],
            "`| `-joining builds a union, and lxml returns the union's nodes in \
             document order — which, for two attributes of one element, is their \
             *source* order and not the order of the `attrs` tuple, so `d2` \
             precedes `h2`. Pinned against Python by the `attr-order` page of \
             testdata/u18_links.jsonl."
        );
    }

    #[test]
    fn debug_does_not_leak_the_closure() {
        let rendered = format!("{:?}", LinkExtractor::default());
        assert!(rendered.contains("LinkExtractor"), "{rendered}");
        assert!(rendered.contains("process: false"), "{rendered}");
    }

    // -- safe_url_string ---------------------------------------------------

    #[test]
    fn safe_url_string_percent_encodes_and_leaves_safe_urls_alone() {
        assert_eq!(
            safe_url_string("https://a.test/a b", "utf-8").expect("safe"),
            "https://a.test/a%20b"
        );
        assert_eq!(
            safe_url_string("https://a.test/x?q=1&r=2#f", "utf-8").expect("safe"),
            "https://a.test/x?q=1&r=2#f"
        );
        assert_eq!(
            safe_url_string("https://a.test/r\u{e9}sum\u{e9}", "utf-8").expect("safe"),
            "https://a.test/r%C3%A9sum%C3%A9"
        );
        assert_eq!(
            safe_url_string("https://A.TEST:443/X", "utf-8").expect("safe"),
            "https://a.test:443/X",
            "the host lower-cases, the path does not"
        );
    }

    #[test]
    fn safe_url_string_keeps_an_existing_escape() {
        // `%` is in the safe set, so an already-escaped URL round-trips.
        assert_eq!(
            safe_url_string("https://a.test/a%20b", "utf-8").expect("safe"),
            "https://a.test/a%20b"
        );
    }

    #[test]
    fn safe_url_string_brackets_an_ipv6_host() {
        assert_eq!(
            safe_url_string("http://[::1]:8080/x", "utf-8").expect("safe"),
            "http://[::1]:8080/x"
        );
    }

    #[test]
    fn safe_url_string_rejects_what_cpython_rejects() {
        assert_eq!(
            safe_url_string("http://[::1/x", "utf-8"),
            Err(SafeUrlError::InvalidIpv6)
        );
        assert_eq!(
            safe_url_string("http://a.test:notaport/x", "utf-8"),
            Err(SafeUrlError::InvalidPort)
        );
        assert_eq!(
            safe_url_string("http://a.test:99999/x", "utf-8"),
            Err(SafeUrlError::InvalidPort)
        );
    }

    /// `urlunsplit` decides the `//` marker from `uses_netloc`, and it decides
    /// it from the **rebuilt** authority.
    ///
    /// The port originally wrote `"http:/x"` for the first of these; the u18
    /// fuzz found it on 146 of 1500 random URLs.
    #[test]
    fn an_empty_authority_is_written_for_a_uses_netloc_scheme_only() {
        for (input, expected) in [
            // In `uses_netloc`: the empty authority is written as `//`.
            ("http:/x", "http:///x"),
            ("https:/x", "https:///x"),
            ("ws:/x", "ws:///x"),
            ("file:/x", "file:///x"),
            ("http://", "http://"),
            ("ftp://?q=1", "ftp://?q=1"),
            // Not in `uses_netloc`: no `//` at all.
            ("mailto:/x", "mailto:/x"),
            ("javascript:/x", "javascript:/x"),
            ("a+b-c.d:/x", "a+b-c.d:/x"),
            // `//` that would be re-read as an authority gets separated.
            ("mailto:////x", "mailto:////x"),
            // `":"` rebuilds to an *empty* authority — no user, host or port —
            // so it is the scheme, not the parsed netloc, that decides.
            ("javascript://:/x", "javascript:/x"),
            // No scheme at all: never a `//`.
            ("/x", "/x"),
        ] {
            assert_eq!(
                safe_url_string(input, "utf-8").expect("safe"),
                expected,
                "{input}"
            );
        }
    }

    /// `_hostinfo` splits host from port on the **first** colon, so `a:1:2` has
    /// the port `"1:2"` and is rejected — not the host `a:1`, which would be
    /// rebuilt as an IPv6 literal.
    #[test]
    fn the_port_is_split_on_the_first_colon_and_must_be_ascii_digits() {
        assert_eq!(
            safe_url_string("http://a:1:2/x", "utf-8"),
            Err(SafeUrlError::InvalidPort)
        );
        // `str::parse::<u16>` would accept the sign; `port.isdigit()` does not.
        assert_eq!(
            safe_url_string("http://a:+1/x", "utf-8"),
            Err(SafeUrlError::InvalidPort)
        );
        // A full-width digit is `isdigit()` but not `isascii()`.
        assert_eq!(
            safe_url_string("http://a:\u{ff10}/x", "utf-8"),
            Err(SafeUrlError::InvalidPort)
        );
        // An empty port is `None`, not an error.
        assert_eq!(
            safe_url_string("http://a:/x", "utf-8").expect("safe"),
            "http://a/x"
        );
        assert_eq!(
            safe_url_string("http://a:0/x", "utf-8").expect("safe"),
            "http://a:0/x"
        );
    }

    /// `_check_bracketed_netloc` + `_check_bracketed_host`.
    #[test]
    fn a_bracketed_host_must_be_ipv6_or_ipvfuture() {
        assert_eq!(
            safe_url_string("http://[::1]/x", "utf-8").expect("safe"),
            "http://[::1]/x"
        );
        // IPvFuture: `\Av[a-fA-F0-9]+\..+\z`. Note the port does *not*
        // re-bracket it, because Python only re-brackets a host with a `:`.
        assert_eq!(
            safe_url_string("http://[v1.abc]/x", "utf-8").expect("safe"),
            "http://v1.abc/x"
        );
        for bad in [
            "http://[abc]/x",
            // "An IPv4 address cannot be in brackets".
            "http://[1.2.3.4]/x",
            "http://[v.abc]/x",
            "http://[vz1.abc]/x",
        ] {
            assert_eq!(
                safe_url_string(bad, "utf-8"),
                Err(SafeUrlError::InvalidBracketedHost),
                "{bad}"
            );
        }
        // "No data is allowed before a bracket", and none between the `]` and
        // the port delimiter. CPython spells both of those `Invalid IPv6 URL`.
        for bad in [
            "http://x[::1]/y",
            "http://[::1]z:80/y",
            "http://u[@a.test/x",
        ] {
            assert_eq!(
                safe_url_string(bad, "utf-8"),
                Err(SafeUrlError::InvalidIpv6),
                "{bad}"
            );
        }
        // A zone id keeps its case; the address half does not.
        assert_eq!(
            safe_url_string("http://[::1%25tESt]:1234/keys", "utf-8").expect("safe"),
            "http://[::1%25tESt]:1234/keys"
        );
    }

    /// `url_extensions` reads `urlsplit(url).path`, which keeps `;params`.
    ///
    /// A `;jsessionid=` therefore defeats the extension deny-list — in Python
    /// and, since this fix, here. Reading it with `urlparse` dropped the
    /// parameters and turned a kept link into a dropped one.
    #[test]
    fn a_params_suffix_stays_part_of_the_extension_chain() {
        assert_eq!(
            url_extensions("https://a.test/report.pdf;jsessionid=ABC"),
            ["pdf;jsessionid=abc"]
        );
        assert!(LinkExtractor::default().url_passes("https://a.test/report.pdf;jsessionid=ABC"));
        assert!(!LinkExtractor::default().url_passes("https://a.test/report.pdf"));
        // `_urlsplit` deletes tab/CR/LF anywhere and left-strips C0-or-space.
        assert_eq!(url_extensions("\u{1}\thttps://a.test/g.pdf"), ["pdf"]);
        assert_eq!(url_extensions("https://a.test/f.p\tdf"), ["pdf"]);
    }

    #[test]
    fn safe_url_string_strips_controls_and_deletes_tabs() {
        assert_eq!(
            safe_url_string("  https://a.test/x\t\r\n  ", "utf-8").expect("safe"),
            "https://a.test/x"
        );
    }

    #[test]
    fn safe_url_string_quotes_the_userinfo() {
        assert_eq!(
            safe_url_string("http://u ser:p=ss@a.test/", "utf-8").expect("safe"),
            "http://u%20ser:p%3Dss@a.test/"
        );
    }

    /// Python's `except ValueError: log.debug("Skipping the extraction of bad
    /// URL …")` branch, exercised the way Python can actually reach it.
    ///
    /// An unbalanced `[` reaches it in *neither* implementation: CPython's
    /// `urljoin` raises `ValueError("Invalid IPv6 URL")` two lines earlier,
    /// outside the `try`, and takes the whole callback down. A bad port is the
    /// case that really lands here.
    #[test]
    fn a_url_safe_url_string_rejects_is_dropped_not_fatal() {
        let response = Response::builder(200, "https://a.test/")
            .body(r#"<a href="http://a.test:99999/x">bad</a><a href="/ok">ok</a>"#)
            .build();
        assert_eq!(
            LinkExtractor::default().extract(&response),
            ["https://a.test/ok"]
        );
    }

    #[test]
    fn hostname_drops_the_port_and_the_userinfo() {
        assert_eq!(
            hostname_of("https://User@Example.COM:8443/x"),
            "example.com"
        );
        assert_eq!(hostname_of("http://[::1]:80/x"), "::1");
        assert_eq!(hostname_of("/relative"), "");
    }
}
