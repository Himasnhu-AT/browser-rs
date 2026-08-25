//! [`Response`]: status, reason, headers, cookies, redirect history, and the
//! lazily parsed [`Document`].
//!
//! In Python, `Response` **subclasses** `Selector`: it is an HTTP reply *and* a
//! parsed HTML document at the same time, and that dual nature is the unification
//! point of the whole library — the browser engines produce the same type through
//! sibling factory methods, and the spiders, the CLI and the AI server all consume
//! it. Rust has no inheritance, so [`Response`] holds a [`Document`] and forwards
//! the selection API to it.
//!
//! # Two deliberate deviations
//!
//! 1. **Parsing is lazy.** Python calls `Selector.__init__` inside
//!    `Response.__init__`, so a 4 MB PDF or a 403 body is parsed as HTML before
//!    the caller has looked at `status`. Here the first `css`/`xpath`/`find_all`
//!    parses, through a [`OnceLock`] so the whole thing stays `Send + Sync` and
//!    two tasks racing the first call get one tree. Nothing observable changes:
//!    lxml runs in `recover=True` mode, so the eager parse never raised.
//!
//! 2. **`adaptive_domain` replaces the URL only for storage.** Python passes
//!    `url=(adaptive_domain or url)` into `Selector.__init__`, which makes
//!    `response.url` report the domain *and* quietly breaks `urljoin`. Here
//!    [`Response::url`] keeps the URL actually fetched, the document is parsed
//!    with that URL as its base, and only
//!    [`DocumentConfig::effective_url`] — the adaptive storage key — sees the
//!    domain. [`Response::selector_url`] returns what Python's `Response.url`
//!    would have.
//!
//! The first is already recorded in `docs/DIVERGENCES.md`; the second is
//! reported for the next wave's entry, and is the choice
//! `crates/browser-rs/tests/integration.rs::response_seam` already pinned before
//! this module existed.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use indexmap::IndexMap;
use browser_core::fingerprint::ElementFingerprint;
use browser_core::text::{ReOptions, RegexSpec};
use browser_parser::config::{
    AllTextOpts, ByRegexOpts, ByTextOpts, CssOpts, DocumentConfig, SimilarOpts,
};
use browser_parser::document::{Body as DocumentBody, Document};
use browser_parser::find::FindQuery;
use browser_parser::node::Node;
use browser_parser::nodes::Nodes;
use serde_json::Value;

use crate::backend::{Method, WireResponse};
use crate::config::{HeaderDict, SelectorConfig};
use crate::error::{FetchError, Result};

pub use crate::redirect_safe::RedirectHop;

/// The default charset when neither the transport nor `Content-Type` names one.
///
/// `ResponseFactory.from_http_request` writes `response.encoding or "utf-8"`.
pub const DEFAULT_ENCODING: &str = "utf-8";

/// The metadata key `_make_request` records the attempt's proxy under.
pub const META_PROXY_KEY: &str = "proxy";

// ---------------------------------------------------------------------------
// Encoding resolution
// ---------------------------------------------------------------------------

/// Extract the charset from a `Content-Type` value.
///
/// A hand-rolled port of the reference implementation's `ResponseFactory.__CHARSET_RE__`,
/// `charset=["']?([\w-]+)`, which is `curl_cffi`'s `charset=([\w-]+)` plus
/// optional quoting. Written by hand because `regex` is not a dependency of this
/// crate — and because a two-branch scanner is easier to audit than a pattern.
///
/// Faithful details:
///
/// * the literal `charset=` is matched **case-sensitively**, as both Python
///   patterns do (the *header name* lookup is case-insensitive, the pattern is
///   not);
/// * the value runs to the first character that is neither alphanumeric, `_` nor
///   `-`, so `text/html;charset=utf-8; foo=bar` yields `utf-8`;
/// * `\w` is treated as ASCII. Every registered encoding label is ASCII, and a
///   non-ASCII label would be rejected by `encoding_rs` anyway.
///
/// # Examples
///
/// ```
/// use browser_http::response::charset_from_content_type;
///
/// assert_eq!(charset_from_content_type("text/html; charset=utf-8"), Some("utf-8"));
/// assert_eq!(charset_from_content_type(r#"text/html; charset="Shift_JIS""#), Some("Shift_JIS"));
/// assert_eq!(charset_from_content_type("text/html;charset=iso-8859-1;x=1"), Some("iso-8859-1"));
/// assert_eq!(charset_from_content_type("text/html"), None);
/// // Case-sensitive, exactly like the Python patterns.
/// assert_eq!(charset_from_content_type("text/html; CHARSET=utf-8"), None);
/// ```
#[must_use]
pub fn charset_from_content_type(content_type: &str) -> Option<&str> {
    let mut rest = content_type;
    while let Some(index) = rest.find("charset=") {
        let after = &rest[index + "charset=".len()..];
        let after = after
            .strip_prefix('"')
            .or_else(|| after.strip_prefix('\''))
            .unwrap_or(after);
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if end > 0 {
            return Some(&after[..end]);
        }
        // `charset=` with nothing usable after it: `[\w-]+` needs one character,
        // so the regex would keep searching. Do the same.
        rest = &rest[index + "charset=".len()..];
    }
    None
}

/// Resolve the charset for a reply.
///
/// `curl_cffi.Response.encoding` resolves in this order, and so does this:
///
/// 1. an encoding the transport already resolved (`_encoding`);
/// 2. `charset=` in the `Content-Type` header;
/// 3. `default_encoding`, which the reference implementation leaves at `"utf-8"`.
///
/// # Examples
///
/// ```
/// use indexmap::IndexMap;
/// use browser_http::response::resolve_encoding;
///
/// let mut headers = IndexMap::new();
/// headers.insert("content-type".to_owned(), "text/html; charset=windows-1252".to_owned());
///
/// assert_eq!(resolve_encoding(&headers, None), "windows-1252");
/// // A transport that resolved one itself wins.
/// assert_eq!(resolve_encoding(&headers, Some("utf-8")), "utf-8");
/// // Nothing anywhere falls back to UTF-8.
/// assert_eq!(resolve_encoding(&IndexMap::new(), None), "utf-8");
/// ```
#[must_use]
pub fn resolve_encoding(headers: &HeaderDict, transport_encoding: Option<&str>) -> String {
    if let Some(encoding) = transport_encoding {
        if !encoding.is_empty() {
            return encoding.to_owned();
        }
    }
    crate::redirect_safe::header_value(headers, "content-type")
        .and_then(charset_from_content_type_http)
        .map_or_else(|| DEFAULT_ENCODING.to_owned(), str::to_owned)
}

/// Extract the charset from a `Content-Type` value the way the **HTTP** stack
/// does.
///
/// [`charset_from_content_type`] is the *browser* pattern, the reference implementation's
/// `__CHARSET_RE__ = charset=["']?([\w-]+)`, which tolerates a quoted value.
/// `curl_cffi.requests.models.CHARSET_RE` is `charset=([\w-]+)` — no quoting —
/// and `Response.encoding` is what unit 12 must reproduce. The difference is
/// observable: `Content-Type: text/html; charset="windows-1252"` resolves to
/// **`utf-8`** over HTTP, because the regex finds no match and the default wins.
///
/// # Examples
///
/// ```
/// use browser_http::response::{charset_from_content_type, charset_from_content_type_http};
///
/// assert_eq!(charset_from_content_type_http("text/html; charset=utf-8"), Some("utf-8"));
/// // The quoted form matches the browser pattern but not curl's.
/// assert_eq!(charset_from_content_type(r#"text/html; charset="Shift_JIS""#), Some("Shift_JIS"));
/// assert_eq!(charset_from_content_type_http(r#"text/html; charset="Shift_JIS""#), None);
/// ```
#[must_use]
pub fn charset_from_content_type_http(content_type: &str) -> Option<&str> {
    let mut rest = content_type;
    while let Some(index) = rest.find("charset=") {
        let after = &rest[index + "charset=".len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if end > 0 {
            return Some(&after[..end]);
        }
        rest = &rest[index + "charset=".len()..];
    }
    None
}

/// The cookie attribute names `http.cookies.SimpleCookie` treats as attributes
/// rather than as another cookie.
///
/// Python's `Morsel._reserved`, verbatim.
const RESERVED_COOKIE_ATTRIBUTES: [&str; 11] = [
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
    "comurl",
];

/// `dict(curl_cffi.Response.cookies)` — the cookies of *one* reply.
///
/// `curl_cffi` builds `Response.cookies` by feeding each `Set-Cookie` line of the
/// **final** reply to `http.cookies.SimpleCookie` and keeping the resulting
/// name/value pairs. Three consequences the corpus pins, all of them different
/// from what a cookie *jar* would say, and all of them reproduced here:
///
/// * no expiry check — a `Max-Age=0` or a 1970 `Expires` still appears;
/// * no domain, path or `Secure` matching — a `Secure` cookie delivered over
///   plain HTTP appears;
/// * nothing from a redirect hop — only the last reply is read.
///
/// A quoted value is unquoted (`SimpleCookie._unquote`), and a `key=value` pair
/// whose key is not a [reserved attribute](RESERVED_COOKIE_ATTRIBUTES) starts a
/// new cookie, so `a=1; b=2` yields two.
///
/// # Examples
///
/// ```
/// use browser_http::response::cookies_from_set_cookie;
///
/// let cookies = cookies_from_set_cookie(&[
///     "a=1".to_owned(),
///     "b=2; Path=/; HttpOnly; SameSite=Lax".to_owned(),
///     r#"k="quoted value""#.to_owned(),
///     "gone=1; Max-Age=0".to_owned(),
/// ]);
/// assert_eq!(cookies["a"], "1");
/// assert_eq!(cookies["b"], "2");
/// assert_eq!(cookies["k"], "quoted value");
/// assert_eq!(cookies["gone"], "1");   // no expiry check, exactly like Python
/// ```
#[must_use]
pub fn cookies_from_set_cookie(set_cookies: &[String]) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for line in set_cookies {
        for pair in line.split(';') {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if RESERVED_COOKIE_ATTRIBUTES
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                continue;
            }
            out.insert(name.to_owned(), unquote_cookie_value(value.trim()));
        }
    }
    out
}

/// `http.cookies._unquote`: strip the wrapping quotes and undo `\\` escapes.
fn unquote_cookie_value(value: &str) -> String {
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return value.to_owned();
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Decode `body` with `label`, degrading rather than failing.
///
/// Labels go through the WHATWG index, not `iconv`: `iso-8859-1`, `latin1` and
/// `ascii` are all aliases of `windows-1252`, so the 32 bytes `0x80..=0x9F`
/// differ from Python's. An unknown label warns and falls back to UTF-8 instead
/// of raising `LookupError`. Both are recorded in `docs/DIVERGENCES.md` and both
/// were already true of `browser-dom`, which decodes the body for the parse;
/// this function only matters for [`Response::body_text`].
#[must_use]
pub fn decode_body<'a>(body: &'a [u8], label: &str) -> Cow<'a, str> {
    let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or_else(|| {
        tracing::warn!(
            label,
            "unknown encoding label; decoding the body as UTF-8 instead"
        );
        encoding_rs::UTF_8
    });
    let (text, _, had_errors) = encoding.decode(body);
    if had_errors {
        tracing::debug!(
            label,
            "the body did not decode cleanly; malformed sequences became U+FFFD"
        );
    }
    text
}

// ---------------------------------------------------------------------------
// SelectorConfig -> DocumentConfig
// ---------------------------------------------------------------------------

/// A last-chance customisation of the [`DocumentConfig`] a [`Response`] is built
/// with.
///
/// It exists because attaching adaptive storage means naming
/// `Arc<dyn browser_storage::StorageSystem>`, and `browser-http` deliberately
/// does not depend on `browser-storage` — the parser owns that edge. A caller
/// that *does* (the façade, the spider, an application) supplies a closure:
///
/// ```ignore
/// let storage = Arc::new(SqliteStorage::in_memory(Some("https://shop.test/"))?);
/// let hook: DocumentHook = Arc::new(move |config: DocumentConfig| {
///     config.storage(Arc::clone(&storage) as Arc<dyn StorageSystem>)
/// });
/// ```
pub type DocumentHook = Arc<dyn Fn(DocumentConfig) -> DocumentConfig + Send + Sync>;

/// Map the fetcher's `selector_config` onto the parser's [`DocumentConfig`].
///
/// This is the seam unit 10 and unit 12 were designed around: it cannot live in
/// either crate without a new dependency edge, so it lives here and unit 14's
/// browser responses share it rather than growing a second copy.
///
/// All seven `PARSER_KEYWORDS` are accounted for: `huge_tree`, `keep_comments`
/// and `keep_cdata` become [`DocumentConfig`] fields, `adaptive` and
/// `adaptive_domain` drive the adaptive layer, and `storage` + `storage_args`
/// collapse into the one storage slot a [`DocumentHook`] fills.
///
/// # Examples
///
/// ```
/// use browser_http::config::SelectorConfig;
/// use browser_http::response::document_config;
///
/// let selector_config = SelectorConfig::default()
///     .with("keep_comments", true.into())
///     .with_adaptive_domain("shop.example.com");
///
/// let config = document_config(
///     "https://shop.example.com/catalogue?page=1",
///     "utf-8",
///     &selector_config,
/// );
///
/// assert!(config.keep_comments);
/// // The fetched URL is the document's base…
/// assert_eq!(config.url, "https://shop.example.com/catalogue?page=1");
/// // …and the domain is only the adaptive storage key.
/// assert_eq!(config.effective_url(), "shop.example.com");
/// ```
#[must_use]
pub fn document_config(
    url: &str,
    encoding: &str,
    selector_config: &SelectorConfig,
) -> DocumentConfig {
    let mut config = DocumentConfig::new()
        .url(url)
        .encoding(encoding)
        .keep_comments(selector_config.keep_comments())
        .keep_cdata(selector_config.keep_cdata())
        .huge_tree(selector_config.huge_tree())
        .adaptive(selector_config.adaptive());
    if let Some(domain) = selector_config.adaptive_domain() {
        config = config.adaptive_domain(domain);
    }
    config
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// An HTTP reply that is also a parsed document.
///
/// # Examples
///
/// ```
/// use browser_http::response::Response;
///
/// let response = Response::builder(200, "https://shop.test/")
///     .body("<div id=p><a href='/1'>one</a><a href='/2'>two</a></div>")
///     .header("Content-Type", "text/html; charset=utf-8")
///     .build();
///
/// assert_eq!(response.status, 200);
/// assert_eq!(response.reason, "OK");
/// assert_eq!(response.encoding, "utf-8");
/// assert!(!response.is_parsed(), "nothing is parsed until it is asked for");
///
/// assert_eq!(response.css("a::text")?.texts(), ["one", "two"]);
/// assert_eq!(response.css("a")?.attrs("href"), ["/1", "/2"]);
/// assert_eq!(response.xpath("//a/@href")?.texts(), ["/1", "/2"]);
/// assert_eq!(response.urljoin("/3"), "https://shop.test/3");
/// assert_eq!(response.to_string(), "<200 https://shop.test/>");
/// # Ok::<(), browser_http::error::FetchError>(())
/// ```
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The reason phrase from the wire, or
    /// [`browser_fingerprint::status::StatusText`]'s.
    pub reason: String,
    /// The URL actually fetched, after redirects.
    ///
    /// Unlike Python, this is **not** replaced by `adaptive_domain`; see the
    /// module docs and [`Response::selector_url`].
    pub url: String,
    /// The method of the final request. A 303 turns a `POST` into a `GET`, so
    /// this is not necessarily the method the caller asked for.
    pub method: Method,
    /// Response headers, in wire order.
    pub headers: HeaderDict,
    /// The headers actually sent with the final request.
    pub request_headers: HeaderDict,
    /// Cookies in effect after the exchange, name to value —
    /// `dict(response.cookies)`.
    pub cookies: IndexMap<String, String>,
    /// Every redirect hop, oldest first. Bodies are genuinely unavailable.
    pub history: Vec<RedirectHop>,
    /// Free-form metadata. The HTTP engines write `{"proxy": …}`.
    pub meta: IndexMap<String, Value>,
    /// The resolved charset, never empty.
    pub encoding: String,
    body: Bytes,
    config: DocumentConfig,
    doc: OnceLock<Document>,
}

impl Response {
    /// Start building a response by hand — for tests, cache replays and the
    /// browser engines.
    #[must_use]
    pub fn builder(status: u16, url: impl Into<String>) -> ResponseBuilder {
        ResponseBuilder::new(status, url)
    }

    /// `ResponseFactory.from_http_request`: one completed exchange plus the
    /// caller's parser configuration becomes a `Response`.
    ///
    /// `history` comes from [`crate::redirect_safe::follow_redirects`],
    /// `request_headers` from the final hop, and `meta` from the driver
    /// (`{"proxy": …}`).
    #[must_use]
    pub fn from_wire(
        wire: WireResponse,
        method: Method,
        history: Vec<RedirectHop>,
        cookies: IndexMap<String, String>,
        meta: IndexMap<String, Value>,
        selector_config: &SelectorConfig,
        document_hook: Option<&DocumentHook>,
    ) -> Self {
        let encoding = resolve_encoding(&wire.headers, wire.encoding.as_deref());
        let mut config = document_config(&wire.url, &encoding, selector_config);
        if let Some(hook) = document_hook {
            config = hook(config);
        }

        let referer = crate::redirect_safe::header_value(&wire.request_headers, "referer")
            .unwrap_or("None")
            .to_owned();
        tracing::info!(
            "Fetched ({}) <{} {}> (referer: {})",
            wire.status,
            method.as_str(),
            wire.url,
            referer
        );

        Self {
            status: wire.status,
            reason: wire.reason,
            url: wire.url,
            method,
            headers: wire.headers,
            request_headers: wire.request_headers,
            cookies,
            history,
            meta,
            encoding,
            body: wire.body,
            config,
            doc: OnceLock::new(),
        }
    }

    // -- the HTTP half ----------------------------------------------------

    /// The raw, unparsed body — Python's `Response.body`.
    #[must_use]
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The body decoded with [`Response::encoding`].
    ///
    /// Deliberately **not** called `text`: Python's `Response.text` is inherited
    /// from `Selector` and returns the *root element's* text, which for a whole
    /// document is almost always empty. Naming this `text` would silently mean
    /// something else than it does in Python.
    #[must_use]
    pub fn body_text(&self) -> Cow<'_, str> {
        decode_body(&self.body, &self.encoding)
    }

    /// `200 <= status < 300` — `curl_cffi`'s `Response.ok`.
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// Whether the reply is a redirect that was not followed.
    #[must_use]
    pub fn is_redirect(&self) -> bool {
        crate::redirect_safe::REDIRECT_STATI.contains(&self.status)
    }

    /// A response header, looked up case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        crate::redirect_safe::header_value(&self.headers, name)
    }

    /// The proxy that served this response, if any — `meta["proxy"]`.
    #[must_use]
    pub fn proxy(&self) -> Option<&Value> {
        self.meta
            .get(META_PROXY_KEY)
            .filter(|value| !value.is_null())
    }

    /// The URL Python's `Response.url` would report: `adaptive_domain or url`.
    ///
    /// Provided for parity with code that relied on the substitution; prefer
    /// [`Response::url`], which is always the URL actually fetched.
    #[must_use]
    pub fn selector_url(&self) -> &str {
        self.config.effective_url()
    }

    /// The [`DocumentConfig`] the body will be (or was) parsed with.
    #[must_use]
    pub fn document_config(&self) -> &DocumentConfig {
        &self.config
    }

    // -- the document half ------------------------------------------------

    /// Whether the body has been parsed yet.
    ///
    /// Only useful for tests and diagnostics; the lazy parse is otherwise
    /// invisible.
    #[must_use]
    pub fn is_parsed(&self) -> bool {
        self.doc.get().is_some()
    }

    /// The parsed document, parsing on first use.
    ///
    /// Infallible: [`Document::parse_bytes`] always produces a tree — lxml runs
    /// with `recover=True` and `browser-dom` follows the HTML5 parser, which
    /// has no failure mode. (`Document::parse_content`'s `NeedsContent` branch
    /// cannot arise here: a `Response` always carries a body, even an empty
    /// one.)
    ///
    /// Racing this from several threads is safe: [`OnceLock`] publishes exactly
    /// one tree and every caller sees the same nodes.
    pub fn doc(&self) -> &Document {
        self.doc
            .get_or_init(|| Document::parse_bytes(&self.body, &self.config))
    }

    /// The document's root element — Python's `Selector._root`.
    pub fn root(&self) -> Node<'_> {
        self.doc().root()
    }

    /// `Selector.css`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when the selector does not compile.
    pub fn css(&self, selector: &str) -> Result<Nodes<'_>> {
        Ok(self.doc().css(selector)?)
    }

    /// `Selector.css` with `identifier` / `adaptive` / `auto_save` /
    /// `percentage`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when the selector does not compile, or when the
    /// adaptive layer is asked for while disabled.
    pub fn css_with(&self, selector: &str, opts: &CssOpts) -> Result<Nodes<'_>> {
        Ok(self.doc().css_with(selector, opts)?)
    }

    /// The first CSS match, or `None`.
    ///
    /// # Errors
    ///
    /// As [`Response::css`].
    pub fn css_first(&self, selector: &str) -> Result<Option<Node<'_>>> {
        Ok(self.doc().css_first(selector)?)
    }

    /// `Selector.xpath`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when the expression does not compile, or evaluates
    /// to a scalar rather than a node-set. Reach through [`Response::doc`] for
    /// `xpath_value` when a scalar is what you want.
    pub fn xpath(&self, expression: &str) -> Result<Nodes<'_>> {
        Ok(self.doc().xpath(expression)?)
    }

    /// `Selector.find_all`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] for an empty query or an invalid regex.
    pub fn find_all(&self, query: &FindQuery<'_>) -> Result<Nodes<'_>> {
        Ok(self.doc().find_all(query)?)
    }

    /// `Selector.find`.
    ///
    /// # Errors
    ///
    /// As [`Response::find_all`].
    pub fn find(&self, query: &FindQuery<'_>) -> Result<Option<Node<'_>>> {
        Ok(self.doc().find(query)?)
    }

    /// `Selector.find_by_text`.
    #[must_use]
    pub fn find_by_text(&self, text: &str, opts: &ByTextOpts) -> Nodes<'_> {
        self.doc().find_by_text(text, opts)
    }

    /// `Selector.find_by_regex`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when the pattern does not compile.
    pub fn find_by_regex(&self, pattern: RegexSpec<'_>, opts: &ByRegexOpts) -> Result<Nodes<'_>> {
        Ok(self.doc().find_by_regex(pattern, opts)?)
    }

    /// `Selector.find_similar`.
    #[must_use]
    pub fn find_similar(&self, opts: &SimilarOpts) -> Nodes<'_> {
        self.doc().find_similar(opts)
    }

    /// `Selector.get_all_text()` with Python's defaults.
    #[must_use]
    pub fn get_all_text(&self) -> String {
        self.doc().get_all_text()
    }

    /// `Selector.get_all_text(...)`.
    #[must_use]
    pub fn get_all_text_with(&self, opts: &AllTextOpts) -> String {
        self.doc().get_all_text_with(opts)
    }

    /// `Selector.html_content`.
    #[must_use]
    pub fn html(&self) -> String {
        self.doc().html()
    }

    /// `Selector.prettify()`.
    #[must_use]
    pub fn pretty(&self) -> String {
        self.doc().pretty()
    }

    /// `Selector.urljoin(relative)`, resolved against the **fetched** URL.
    ///
    /// Python resolves against `adaptive_domain or url`, so setting
    /// `adaptive_domain` breaks `urljoin` there. It does not here.
    #[must_use]
    pub fn urljoin(&self, relative: &str) -> String {
        self.doc().urljoin(relative)
    }

    /// The body as JSON.
    ///
    /// Python's `Selector.json()` prefers the raw body over the parsed tree, and
    /// so does this: the body is deserialised directly, and only an empty body
    /// falls through to the document's text.
    ///
    /// # Errors
    ///
    /// [`FetchError::Json`] when the body is not valid JSON.
    pub fn json(&self) -> Result<Value> {
        if self.body.is_empty() {
            return Ok(self.doc().json()?);
        }
        serde_json::from_slice(&self.body).map_err(FetchError::Json)
    }

    /// `Selector.re(...)` over the document's text.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when the pattern does not compile.
    pub fn re(&self, pattern: RegexSpec<'_>, opts: ReOptions) -> Result<Vec<String>> {
        Ok(self.doc().root().re(pattern, opts)?)
    }

    /// `Selector.re_first(...)`.
    ///
    /// # Errors
    ///
    /// As [`Response::re`].
    pub fn re_first(&self, pattern: RegexSpec<'_>, opts: ReOptions) -> Result<Option<String>> {
        Ok(self.doc().root().re_first(pattern, opts)?)
    }

    // -- the adaptive layer ----------------------------------------------

    /// Whether `adaptive=True` reached the parser.
    #[must_use]
    pub fn adaptive_enabled(&self) -> bool {
        self.doc().adaptive_enabled()
    }

    /// Whether a storage backend was attached.
    #[must_use]
    pub fn has_storage(&self) -> bool {
        self.doc().has_storage()
    }

    /// `Selector.save(element, identifier)`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when adaptive storage is disabled or the write
    /// fails.
    pub fn save(&self, node: &Node<'_>, identifier: &str) -> Result<()> {
        Ok(self.doc().save(node, identifier)?)
    }

    /// `Selector.retrieve(identifier)`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when adaptive storage is disabled or the read
    /// fails.
    pub fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>> {
        Ok(self.doc().retrieve(identifier)?)
    }

    /// Relocate a stored fingerprint on this document.
    #[must_use]
    pub fn relocate(&self, fingerprint: &ElementFingerprint, percentage: u8) -> Nodes<'_> {
        self.doc().relocate(fingerprint, percentage)
    }

    // -- markdown ---------------------------------------------------------

    /// `Response.markdown()` with Python's defaults.
    #[cfg(feature = "markdown")]
    #[must_use]
    pub fn markdown(&self) -> String {
        self.doc().markdown()
    }

    /// `Response.markdown(css_selector=…, main_content_only=…)`.
    ///
    /// # Errors
    ///
    /// [`FetchError::Parse`] when `css_selector` does not compile.
    #[cfg(feature = "markdown")]
    pub fn markdown_with(
        &self,
        css_selector: Option<&str>,
        main_content_only: bool,
    ) -> Result<String> {
        Ok(self.doc().markdown_with(css_selector, main_content_only)?)
    }
}

impl std::fmt::Display for Response {
    /// Python's `Response.__str__`: `<200 https://example.com>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} {}>", self.status, self.url)
    }
}

impl std::fmt::Debug for Response {
    /// Everything except the body and the tree, which are both unbounded.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("reason", &self.reason)
            .field("url", &self.url)
            .field("method", &self.method)
            .field("encoding", &self.encoding)
            .field("headers", &self.headers.len())
            .field("cookies", &self.cookies.len())
            .field("history", &self.history.len())
            .field("meta", &self.meta)
            .field("body_len", &self.body.len())
            .field("parsed", &self.is_parsed())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Assemble a [`Response`] by hand.
///
/// The driver goes through [`Response::from_wire`]; this is for tests, for cache
/// replays that never had a live exchange, and for the browser engines.
pub struct ResponseBuilder {
    status: u16,
    url: String,
    reason: Option<String>,
    method: Method,
    headers: HeaderDict,
    request_headers: HeaderDict,
    cookies: IndexMap<String, String>,
    history: Vec<RedirectHop>,
    meta: IndexMap<String, Value>,
    encoding: Option<String>,
    body: Bytes,
    selector_config: SelectorConfig,
    document_hook: Option<DocumentHook>,
}

impl ResponseBuilder {
    /// Start from a status and a URL.
    #[must_use]
    pub fn new(status: u16, url: impl Into<String>) -> Self {
        Self {
            status,
            url: url.into(),
            reason: None,
            method: Method::Get,
            headers: HeaderDict::new(),
            request_headers: HeaderDict::new(),
            cookies: IndexMap::new(),
            history: Vec::new(),
            meta: IndexMap::new(),
            encoding: None,
            body: Bytes::new(),
            selector_config: SelectorConfig::new(),
            document_hook: None,
        }
    }

    /// Override the reason phrase. Left alone it comes from
    /// [`browser_fingerprint::status::StatusText`].
    #[must_use]
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Set the method of the final request.
    #[must_use]
    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// Set the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Add a response header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Replace the response headers wholesale.
    #[must_use]
    pub fn headers(mut self, headers: HeaderDict) -> Self {
        self.headers = headers;
        self
    }

    /// Record the headers that were sent.
    #[must_use]
    pub fn request_headers(mut self, headers: HeaderDict) -> Self {
        self.request_headers = headers;
        self
    }

    /// Record the cookies in effect.
    #[must_use]
    pub fn cookies(mut self, cookies: IndexMap<String, String>) -> Self {
        self.cookies = cookies;
        self
    }

    /// Record the redirect history.
    #[must_use]
    pub fn history(mut self, history: Vec<RedirectHop>) -> Self {
        self.history = history;
        self
    }

    /// Add one metadata entry.
    #[must_use]
    pub fn meta(mut self, key: impl Into<String>, value: Value) -> Self {
        self.meta.insert(key.into(), value);
        self
    }

    /// Force the charset instead of resolving it from `Content-Type`.
    #[must_use]
    pub fn encoding(mut self, encoding: impl Into<String>) -> Self {
        self.encoding = Some(encoding.into());
        self
    }

    /// Set the parser configuration.
    #[must_use]
    pub fn selector_config(mut self, selector_config: SelectorConfig) -> Self {
        self.selector_config = selector_config;
        self
    }

    /// Attach a [`DocumentHook`], e.g. to supply adaptive storage.
    #[must_use]
    pub fn document_hook(mut self, hook: DocumentHook) -> Self {
        self.document_hook = Some(hook);
        self
    }

    /// Finish.
    #[must_use]
    pub fn build(self) -> Response {
        let reason = self.reason.unwrap_or_else(|| {
            browser_fingerprint::status::StatusText::get(self.status).to_owned()
        });
        // A hand-built response reports the `Set-Cookie` lines it was given,
        // `\n`-separated when several were folded into one header value.
        let set_cookies: Vec<String> = self
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .flat_map(|(_, value)| value.split('\n'))
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        let wire = WireResponse {
            status: self.status,
            reason,
            url: self.url,
            headers: self.headers,
            set_cookies,
            request_headers: self.request_headers,
            body: self.body,
            encoding: self.encoding,
            remote_addr: None,
        };
        Response::from_wire(
            wire,
            self.method,
            self.history,
            self.cookies,
            self.meta,
            &self.selector_config,
            self.document_hook.as_ref(),
        )
    }
}

/// The parser-side half of the driver: `selector_config` plus an optional
/// [`DocumentHook`], reused for every response a session produces.
///
/// Python's `ResponseFactory` is a namespace of static methods; here it carries
/// the two pieces of state a `Response` needs and that the transport knows
/// nothing about.
#[derive(Clone, Default)]
pub struct ResponseFactory {
    /// The resolved `selector_config` for this request.
    pub selector_config: SelectorConfig,
    /// The optional [`DocumentConfig`] customiser.
    pub document_hook: Option<DocumentHook>,
}

impl ResponseFactory {
    /// A factory with no parser configuration at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A factory for one request's `selector_config`.
    #[must_use]
    pub fn with_selector_config(selector_config: SelectorConfig) -> Self {
        Self {
            selector_config,
            document_hook: None,
        }
    }

    /// Attach a [`DocumentHook`].
    #[must_use]
    pub fn with_document_hook(mut self, hook: Option<DocumentHook>) -> Self {
        self.document_hook = hook;
        self
    }

    /// `ResponseFactory.from_http_request(response, parser_arguments, meta)`.
    #[must_use]
    pub fn from_http_request(
        &self,
        wire: WireResponse,
        method: Method,
        history: Vec<RedirectHop>,
        cookies: IndexMap<String, String>,
        meta: IndexMap<String, Value>,
    ) -> Response {
        Response::from_wire(
            wire,
            method,
            history,
            cookies,
            meta,
            &self.selector_config,
            self.document_hook.as_ref(),
        )
    }
}

impl std::fmt::Debug for ResponseFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseFactory")
            .field("selector_config", &self.selector_config)
            .field("document_hook", &self.document_hook.is_some())
            .finish()
    }
}

/// A document body built from the response's bytes.
///
/// Exposed because unit 15's `FetchedResponse` and unit 14's browser responses
/// both need to hand the same bytes to the parser, and `browser-spider` cannot
/// see this crate's private fields.
#[must_use]
pub fn body_for_parser(body: &Bytes) -> DocumentBody {
    DocumentBody::Bytes(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = concat!(
        r#"<!doctype html><html><head><title>Shop</title></head><body>"#,
        r#"<div id="wrap" class="container">"#,
        r#"<p class="price" data-sku="A-1">$9.99</p>"#,
        r#"<a href="/p/1">one</a><a href="/p/2">two</a>"#,
        r#"</div></body></html>"#,
    );

    fn page_response() -> Response {
        Response::builder(200, "https://shop.example.com/catalogue")
            .header("Content-Type", "text/html; charset=utf-8")
            .header("Server", "nginx")
            .body(PAGE)
            .build()
    }

    #[test]
    fn the_charset_scanner_matches_the_python_pattern() {
        // The rows the seam test pins, plus the quoting curl_cffi's own pattern
        // does not handle and the reference implementation's does.
        assert_eq!(
            charset_from_content_type("text/html; charset=utf-8"),
            Some("utf-8")
        );
        assert_eq!(
            charset_from_content_type("text/html;charset=UTF-8"),
            Some("UTF-8")
        );
        assert_eq!(
            charset_from_content_type("text/html; charset=Shift_JIS"),
            Some("Shift_JIS")
        );
        assert_eq!(
            charset_from_content_type(r#"text/html; charset="windows-1252""#),
            Some("windows-1252")
        );
        assert_eq!(
            charset_from_content_type("text/html; charset='iso-8859-1'"),
            Some("iso-8859-1")
        );
        assert_eq!(
            charset_from_content_type("text/html; charset=utf-8; boundary=x"),
            Some("utf-8")
        );
        assert_eq!(charset_from_content_type("text/html"), None);
        assert_eq!(charset_from_content_type(""), None);
        // `[\w-]+` needs at least one character, so a bare `charset=` does not
        // match — and the scan keeps going, exactly like `re.search`.
        assert_eq!(charset_from_content_type("text/html; charset="), None);
        assert_eq!(
            charset_from_content_type("text/html; charset=; charset=utf-8"),
            Some("utf-8")
        );
        // Case-sensitive on the literal, like both Python patterns.
        assert_eq!(charset_from_content_type("text/html; CharSet=utf-8"), None);
    }

    #[test]
    fn the_encoding_resolves_transport_then_header_then_utf8() {
        let mut headers = HeaderDict::new();
        headers.insert(
            "Content-Type".to_owned(),
            "text/html; charset=windows-1252".to_owned(),
        );
        assert_eq!(resolve_encoding(&headers, None), "windows-1252");
        assert_eq!(resolve_encoding(&headers, Some("gbk")), "gbk");
        // An empty transport answer is not an answer.
        assert_eq!(resolve_encoding(&headers, Some("")), "windows-1252");
        assert_eq!(resolve_encoding(&HeaderDict::new(), None), DEFAULT_ENCODING);

        // The header lookup is case-insensitive even though the pattern is not.
        let mut lower = HeaderDict::new();
        lower.insert(
            "content-type".to_owned(),
            "text/html; charset=gbk".to_owned(),
        );
        assert_eq!(resolve_encoding(&lower, None), "gbk");
    }

    #[test]
    fn the_body_is_decoded_with_the_resolved_encoding() {
        let latin1 = b"<html><body><p>9,99\xa0\xe9</p></body></html>".to_vec();

        let mislabelled = Response::builder(200, "https://a.test/")
            .header("Content-Type", "text/html; charset=utf-8")
            .body(latin1.clone())
            .build();
        assert_eq!(mislabelled.encoding, "utf-8");
        assert_eq!(
            mislabelled.css("p::text").expect("css").texts(),
            ["9,99\u{fffd}\u{fffd}"]
        );

        let labelled = Response::builder(200, "https://a.test/")
            .header("Content-Type", "text/html; charset=windows-1252")
            .body(latin1)
            .build();
        assert_eq!(labelled.encoding, "windows-1252");
        assert_eq!(
            labelled.css("p::text").expect("css").texts(),
            ["9,99\u{a0}é"]
        );
        // …and `body_text` decodes the same way, through `encoding_rs`.
        assert!(labelled.body_text().contains("9,99\u{a0}é"));
    }

    #[test]
    fn an_unknown_encoding_label_degrades_instead_of_raising() {
        // Python raises `LookupError`; `docs/DIVERGENCES.md` records the choice.
        let response = Response::builder(200, "https://a.test/")
            .header("Content-Type", "text/html; charset=x-not-an-encoding")
            .body("<p>hi</p>")
            .build();
        assert_eq!(response.encoding, "x-not-an-encoding");
        assert_eq!(response.body_text(), "<p>hi</p>");
        assert_eq!(response.css("p::text").expect("css").texts(), ["hi"]);
    }

    #[test]
    fn nothing_parses_until_a_selector_is_asked_for() {
        let response = page_response();
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.header("server"), Some("nginx"));
        assert!(response.ok());
        assert!(!response.is_redirect());
        assert!(!response.is_parsed());

        assert_eq!(
            response.css("p.price::text").expect("css").texts(),
            ["$9.99"]
        );
        assert!(response.is_parsed());
    }

    #[test]
    fn every_engine_sees_the_same_tree() {
        let response = page_response();
        assert_eq!(
            response.xpath("//a/@href").expect("xpath").texts(),
            ["/p/1", "/p/2"]
        );
        let by_css = response.css("p.price").expect("css");
        let by_xpath = response.xpath("//p[@class='price']").expect("xpath");
        assert_eq!(by_css.at(0), by_xpath.at(0));

        let hits = response
            .find_all(&FindQuery::new().tag("a").attr("href*", "/p/2"))
            .expect("find_all");
        assert_eq!(hits.texts(), ["two"]);
        assert_eq!(
            response.find_by_text("one", &ByTextOpts::new()).texts(),
            ["one"]
        );
        assert!(response.get_all_text().contains("$9.99"));
        assert!(response.html().contains("data-sku"));
    }

    #[test]
    fn the_base_url_is_the_fetched_url_not_the_adaptive_domain() {
        let response = Response::builder(200, "https://shop.example.com/catalogue?page=1")
            .selector_config(
                SelectorConfig::default()
                    .with("adaptive", true.into())
                    .with_adaptive_domain("shop.example.com"),
            )
            .body(PAGE)
            .build();

        // Python reports the domain and breaks `urljoin`; we keep both right.
        assert_eq!(response.url, "https://shop.example.com/catalogue?page=1");
        assert_eq!(
            response.doc().url(),
            "https://shop.example.com/catalogue?page=1"
        );
        assert_eq!(response.urljoin("/p/3"), "https://shop.example.com/p/3");
        // …and the storage key is still the domain, which is what it is for.
        assert_eq!(response.selector_url(), "shop.example.com");
        assert_eq!(
            response.document_config().effective_url(),
            "shop.example.com"
        );
        assert!(response.adaptive_enabled());
    }

    #[test]
    fn the_parser_keywords_reach_the_parse() {
        let response = Response::builder(200, "https://a.test/")
            .selector_config(SelectorConfig::default().with("keep_comments", true.into()))
            .body("<div><!-- keep me --></div>")
            .build();
        assert!(response.document_config().keep_comments);
        assert!(response.doc().dom().nodes().any(|node| node.is_comment()));

        let stripped = Response::builder(200, "https://a.test/")
            .body("<div><!-- drop me --></div>")
            .build();
        assert!(!stripped.doc().dom().nodes().any(|node| node.is_comment()));
    }

    #[test]
    fn json_reads_the_raw_body_not_the_tree() {
        let response = Response::builder(200, "https://a.test/api")
            .header("Content-Type", "application/json")
            .body(r#"{"items": [1, 2, 3]}"#)
            .build();
        assert_eq!(response.json().expect("json")["items"][2], 3);
        // The body survives the parse either way.
        assert_eq!(response.body().len(), 20);

        let broken = Response::builder(200, "https://a.test/api")
            .body("not json")
            .build();
        assert!(matches!(broken.json(), Err(FetchError::Json(_))));
    }

    #[test]
    fn the_display_and_debug_shapes_are_pythons() {
        let response = page_response();
        assert_eq!(
            response.to_string(),
            "<200 https://shop.example.com/catalogue>"
        );
        let debug = format!("{response:?}");
        assert!(debug.contains("status: 200"), "{debug}");
        assert!(debug.contains("parsed: false"), "{debug}");
    }

    #[test]
    fn a_response_is_send_and_sync_across_the_lazy_parse() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Response>();

        let response = Arc::new(page_response());
        let texts: Vec<Vec<String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let response = Arc::clone(&response);
                    scope.spawn(move || response.css("a::text").expect("css").texts())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("no panic"))
                .collect()
        });
        assert_eq!(texts.len(), 4);
        for row in texts {
            assert_eq!(row, ["one", "two"]);
        }
    }

    #[test]
    fn an_empty_body_is_a_representable_state() {
        // Python builds redirect hops with `content=""`; the lazy parse must
        // survive it rather than panicking.
        let response = Response::builder(302, "https://a.test/")
            .header("Location", "/next")
            .build();
        assert!(response.is_redirect());
        assert!(response.body().is_empty());
        assert!(response.css("p").expect("css").is_empty());
        assert_eq!(response.body_text(), "");
        // A body that is *only* NUL bytes still parses — Python strips them.
        let nul = Response::builder(200, "https://a.test/")
            .body(b"Hello\x00World".to_vec())
            .build();
        assert!(nul.get_all_text().contains("Hello"));
    }

    #[test]
    fn the_document_hook_can_reshape_the_config() {
        let hook: DocumentHook = Arc::new(|config: DocumentConfig| config.keep_cdata(true));
        let response = Response::builder(200, "https://a.test/")
            .document_hook(hook)
            .body("<p>x</p>")
            .build();
        assert!(response.document_config().keep_cdata);
    }

    #[test]
    fn the_factory_reuses_one_selector_config() {
        let factory = ResponseFactory::with_selector_config(
            SelectorConfig::default().with_adaptive_domain("a.test"),
        );
        let mut meta = IndexMap::new();
        meta.insert(
            META_PROXY_KEY.to_owned(),
            Value::String("http://p:1".into()),
        );

        let response = factory.from_http_request(
            WireResponse::new(200, "https://a.test/x", Bytes::from_static(b"<p>x</p>")),
            Method::Get,
            Vec::new(),
            IndexMap::new(),
            meta,
        );
        assert_eq!(response.selector_url(), "a.test");
        assert_eq!(response.proxy().and_then(Value::as_str), Some("http://p:1"));
        assert!(format!("{factory:?}").contains("document_hook: false"));
    }

    #[test]
    fn history_is_a_typed_hop_list_not_a_leaked_transport_object() {
        let hop = RedirectHop {
            url: "https://a.test/one".to_owned(),
            status: 301,
            reason: "Moved Permanently".to_owned(),
            headers: HeaderDict::new(),
            location: "https://a.test/two".to_owned(),
        };
        let response = Response::builder(200, "https://a.test/two")
            .history(vec![hop.clone()])
            .build();
        assert_eq!(response.history, [hop]);
        assert_eq!(response.history[0].to_string(), "<301 https://a.test/one>");
    }

    #[test]
    fn body_for_parser_round_trips_the_bytes() {
        let body = Bytes::from_static(b"<p>hi</p>");
        assert_eq!(body_for_parser(&body).as_bytes(), b"<p>hi</p>");
    }
}
