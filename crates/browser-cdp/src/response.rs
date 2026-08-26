//! `ResponseFactory.from_playwright_response`: a rendered page becomes the
//! **same** [`browser_http::response::Response`] an HTTP fetch produces.
//!
//! That sameness is the contract with the rest of the library (`browser.md`
//! §1): "whatever engine produced the bytes, downstream parsing, markdown
//! conversion, spiders and the MCP server treat it identically". Unit 12 owns
//! the type; this module owns the translation, and
//! `crates/browser-rs/tests/integration.rs::browser_response_seam` is the
//! executable proof that the translation needs nothing unit 12 does not already
//! expose.
//!
//! # The algorithm, from `convertor.py`
//!
//! ```text
//! final    = final_response or first_response      # else ValueError
//! encoding = charset of final's content-type       # the *browser* regex
//! reason   = final.status_text or StatusText.get(final.status)
//! history  = redirect chain of first_response      # empty bodies
//! body     = page.content() if "html" in final's content-type else final.body()
//! Response(url=page.url, content=body, status=final.status, reason=…, encoding=…,
//!          cookies=page.context.cookies(), headers=first.headers,
//!          request_headers=first.request.headers, history=…, meta=…)
//! ```
//!
//! Note which response each field comes from: the **status, reason and
//! encoding** are the *final* navigation response's, while the **headers and
//! request headers** are the *first*'s. That asymmetry is Python's, it is
//! observable, and it is reproduced.
//!
//! # `captured_xhr`
//!
//! Python attaches the captured XHR responses to an attribute on the returned
//! object. Unit 12's `Response` is a fixed struct with no such field, so they
//! ride alongside it in [`BrowserFetch`], which `Deref`s to the response. A
//! caller that only wants the page is unaffected; downstream still sees one
//! `Response` type.

use browser_http::backend::Method;
use browser_http::config::SelectorConfig;
use browser_http::redirect_safe::RedirectHop;
use browser_http::response::{charset_from_content_type, DocumentHook, Response, DEFAULT_ENCODING};
use bytes::Bytes;
use indexmap::IndexMap;
use serde_json::Value;

use crate::cdp::history::{Capture, ResponseRecord};
use crate::error::{BrowserError, Result};

/// The meta key both engines write the request's proxy under.
pub use browser_http::response::META_PROXY_KEY;

/// The method a browser navigation reports.
///
/// Python's browser factory never passes `method`, so the `Response`
/// constructor's default wins. A form POST that Chrome followed still reports
/// `GET` there; keeping the same default keeps the two engines comparable.
pub const BROWSER_RESPONSE_METHOD: Method = Method::Get;

/// The page state the factory reads.
///
/// A trait rather than [`crate::cdp::CdpPage`] directly so the factory is
/// testable without a browser — and so the `page=None` branch of Python's
/// factory (used for every captured XHR) is simply `None` here.
#[async_trait::async_trait]
pub trait PageSource: Send + Sync {
    /// `ResponseFactory._get_page_content(page)` — the serialised live DOM.
    ///
    /// # Errors
    ///
    /// [`BrowserError::PageContentUnavailable`].
    async fn content(&self) -> Result<String>;

    /// `response.body()` for a finished request, or `None` when Chrome no
    /// longer has it.
    async fn response_body(&self, request_id: &str) -> Option<Bytes>;

    /// `page.url`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    async fn url(&self) -> Result<String>;

    /// `page.context.cookies()`, as name-to-value pairs.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    async fn cookies(&self) -> Result<IndexMap<String, String>>;
}

/// One browser fetch: the page response, plus anything `capture_xhr` matched.
///
/// `Deref`s to the [`Response`], so `fetch(...)?.css("h1")` reads exactly like
/// an HTTP fetch.
pub struct BrowserFetch {
    /// The page.
    pub response: Response,
    /// `Response.captured_xhr` — the `xhr`/`fetch` responses whose URL matched
    /// the session's `capture_xhr` pattern, in arrival order.
    pub captured_xhr: Vec<Response>,
}

impl BrowserFetch {
    /// Discard the captured XHR responses and keep the page.
    #[must_use]
    pub fn into_response(self) -> Response {
        self.response
    }
}

impl std::ops::Deref for BrowserFetch {
    type Target = Response;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

impl std::fmt::Debug for BrowserFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserFetch")
            .field("response", &self.response.to_string())
            .field("captured_xhr", &self.captured_xhr.len())
            .finish()
    }
}

/// `ResponseFactory`, carrying the parser configuration one request runs with.
#[derive(Clone, Default)]
pub struct BrowserResponseFactory {
    /// The resolved `selector_config` for this request.
    pub selector_config: SelectorConfig,
    /// An optional [`DocumentHook`], e.g. to supply adaptive storage.
    pub document_hook: Option<DocumentHook>,
}

impl std::fmt::Debug for BrowserResponseFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserResponseFactory")
            .field("selector_config", &self.selector_config)
            .field("document_hook", &self.document_hook.is_some())
            .finish()
    }
}

impl BrowserResponseFactory {
    /// A factory with no parser configuration.
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

    /// `ResponseFactory.from_playwright_response(page, first, final, …)`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::NoResponseFromPage`] when the capture window saw no
    /// main-frame document response at all — Python's
    /// `ValueError("Failed to get a response from the page")`.
    pub async fn from_browser_page(
        &self,
        page: &dyn PageSource,
        capture: &Capture,
        meta: IndexMap<String, Value>,
    ) -> Result<BrowserFetch> {
        let first = capture
            .first
            .as_ref()
            .ok_or(BrowserError::NoResponseFromPage)?;
        let final_response = capture.effective_final().unwrap_or(first);

        let mut encoding = browser_encoding(final_response);
        let history: Vec<RedirectHop> = capture
            .history
            .iter()
            .cloned()
            .map(RedirectHop::from)
            .collect();

        // `if page and "html" in final_response.all_headers()["content-type"]`
        let body = if final_response.is_html() {
            match page.content().await {
                Ok(content) => {
                    encoding = DEFAULT_ENCODING.to_owned();
                    Bytes::from(content.into_bytes())
                }
                Err(error) => {
                    // Python: `except Exception: log.error(...); page_content = b""`
                    tracing::error!("Error getting page content: {error}");
                    Bytes::new()
                }
            }
        } else {
            page.response_body(&final_response.request_id)
                .await
                .unwrap_or_default()
        };

        // `page.url` — the URL after every redirect and client-side navigation,
        // which is why it is read from the page and not from a response.
        let url = match page.url().await {
            Ok(url) => url,
            Err(error) => {
                tracing::debug!(
                    "Could not read page.url, falling back to the response URL: {error}"
                );
                first.url.clone()
            }
        };
        let cookies = match page.cookies().await {
            Ok(cookies) => cookies,
            Err(error) => {
                tracing::debug!("Could not read the context cookies: {error}");
                IndexMap::new()
            }
        };

        let response = self.build(
            &url,
            final_response,
            first,
            body,
            encoding,
            cookies,
            history,
            meta,
        );

        let mut captured_xhr = Vec::with_capacity(capture.xhr.len());
        for record in &capture.xhr {
            let body = page
                .response_body(&record.request_id)
                .await
                .unwrap_or_default();
            captured_xhr.push(self.build_xhr(record, body));
        }

        Ok(BrowserFetch {
            response,
            captured_xhr,
        })
    }

    /// `from_playwright_response(None, xhr, None, {}, collect_history=False)`.
    ///
    /// Every difference from the page response is Python's: no page means the
    /// URL comes from the response, the body is the transport body, there are
    /// no cookies and no history — and the parser arguments are an **empty**
    /// dict, not the session's `selector_config`.
    #[must_use]
    pub fn build_xhr(&self, record: &ResponseRecord, body: Bytes) -> Response {
        BrowserResponseFactory::default().build(
            &record.url,
            record,
            record,
            body,
            browser_encoding(record),
            IndexMap::new(),
            Vec::new(),
            IndexMap::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        url: &str,
        final_response: &ResponseRecord,
        first: &ResponseRecord,
        body: Bytes,
        encoding: String,
        cookies: IndexMap<String, String>,
        history: Vec<RedirectHop>,
        meta: IndexMap<String, Value>,
    ) -> Response {
        let mut builder = Response::builder(final_response.status, url)
            .method(BROWSER_RESPONSE_METHOD)
            .reason(final_response.reason.clone())
            .encoding(encoding)
            .body(body)
            // Python reads `headers` and `request_headers` off the *first*
            // response, not the final one.
            .headers(first.headers.clone())
            .request_headers(first.request_headers.clone())
            .cookies(cookies)
            .history(history)
            .selector_config(self.selector_config.clone());
        for (key, value) in meta {
            builder = builder.meta(key, value);
        }
        if let Some(hook) = self.document_hook.as_ref() {
            builder = builder.document_hook(hook.clone());
        }
        builder.build()
    }
}

/// `ResponseFactory.__extract_browser_encoding(content_type, default="utf-8")`.
///
/// Deliberately the *browser* charset regex — `charset=["']?([\w-]+)`, which
/// accepts the quoted values Chrome reports — and not the stricter HTTP one
/// unit 12 uses for socket responses. The difference is a declared divergence.
#[must_use]
pub fn browser_encoding(record: &ResponseRecord) -> String {
    charset_from_content_type(record.content_type())
        .unwrap_or(DEFAULT_ENCODING)
        .to_owned()
}

/// `meta={"proxy": proxy}` — always present, `null` when there was no proxy.
#[must_use]
pub fn proxy_meta(proxy: Option<&browser_core::types::Proxy>) -> IndexMap<String, Value> {
    let mut meta = IndexMap::new();
    let value = proxy.map_or(Value::Null, |proxy| {
        serde_json::to_value(proxy).unwrap_or(Value::Null)
    });
    meta.insert(META_PROXY_KEY.to_owned(), value);
    meta
}

#[cfg(test)]
mod tests {
    use browser_http::config::HeaderDict;

    use super::*;
    use crate::cdp::history::RedirectRecord;

    const RENDERED: &str = "<html><head><title>Shop</title></head><body>\
         <p class=\"price\">$9.99</p></body></html>";

    #[derive(Debug, Default)]
    struct FakePage {
        content: String,
        url: String,
        cookies: IndexMap<String, String>,
        bodies: IndexMap<String, Bytes>,
        content_fails: bool,
    }

    #[async_trait::async_trait]
    impl PageSource for FakePage {
        async fn content(&self) -> Result<String> {
            if self.content_fails {
                return Err(BrowserError::PageContentUnavailable { elapsed_ms: 10_000 });
            }
            Ok(self.content.clone())
        }

        async fn response_body(&self, request_id: &str) -> Option<Bytes> {
            self.bodies.get(request_id).cloned()
        }

        async fn url(&self) -> Result<String> {
            Ok(self.url.clone())
        }

        async fn cookies(&self) -> Result<IndexMap<String, String>> {
            Ok(self.cookies.clone())
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn record(url: &str, status: u16, content_type: &str) -> ResponseRecord {
        ResponseRecord {
            awaiting_raw_headers: false,
            request_id: "REQ".to_owned(),
            url: url.to_owned(),
            status,
            reason: "OK".to_owned(),
            headers: headers(&[("content-type", content_type)]),
            request_headers: headers(&[("user-agent", "Chrome"), ("referer", "https://g/")]),
            resource_type: "document",
        }
    }

    fn html_capture() -> Capture {
        let first = record("https://shop.test/", 200, "text/html; charset=utf-8");
        Capture {
            first: Some(first.clone()),
            last: Some(first),
            xhr: Vec::new(),
            history: Vec::new(),
        }
    }

    /// `SiteToMarkdownSpider` points at the browser as readily as at the HTTP
    /// engine, so the crate's `markdown` feature has to reach
    /// **`browser-http`** — `Response::markdown()` lives there, on the one
    /// response type in the workspace. This body does not compile unless
    /// `Cargo.toml` forwards the flag.
    #[cfg(feature = "markdown")]
    #[tokio::test]
    async fn a_browser_response_converts_to_markdown() {
        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://shop.test/".to_owned(),
            ..FakePage::default()
        };
        let response = BrowserResponseFactory::new()
            .from_browser_page(&page, &html_capture(), proxy_meta(None))
            .await
            .expect("a response")
            .into_response();

        assert!(response.markdown().contains("$9.99"));
    }

    #[tokio::test]
    async fn a_rendered_page_becomes_the_shared_response_type() {
        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://shop.test/catalogue".to_owned(),
            cookies: [("session".to_owned(), "abc".to_owned())]
                .into_iter()
                .collect(),
            ..FakePage::default()
        };
        let factory = BrowserResponseFactory::new();
        let fetch = factory
            .from_browser_page(&page, &html_capture(), proxy_meta(None))
            .await
            .expect("a response");

        assert_eq!(fetch.status, 200);
        assert_eq!(fetch.reason, "OK");
        assert_eq!(
            fetch.url, "https://shop.test/catalogue",
            "`page.url`, not the response URL"
        );
        assert_eq!(fetch.encoding, "utf-8");
        assert_eq!(fetch.cookies["session"], "abc");
        assert_eq!(fetch.request_headers["user-agent"], "Chrome");
        assert_eq!(fetch.method, Method::Get);
        // The parsed document is the *rendered* DOM.
        assert_eq!(fetch.css("p.price::text").expect("css").texts(), ["$9.99"]);
        assert!(fetch.captured_xhr.is_empty());
        assert_eq!(fetch.to_string(), "<200 https://shop.test/catalogue>");
    }

    #[tokio::test]
    async fn a_non_html_response_uses_the_transport_body() {
        let mut capture = html_capture();
        let json = record("https://shop.test/api", 200, "application/json");
        capture.first = Some(json.clone());
        capture.last = Some(json);

        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://shop.test/api".to_owned(),
            bodies: [("REQ".to_owned(), Bytes::from_static(br#"{"ok":true}"#))]
                .into_iter()
                .collect(),
            ..FakePage::default()
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &capture, proxy_meta(None))
            .await
            .expect("a response");
        assert_eq!(fetch.body().as_ref(), br#"{"ok":true}"#);
        assert_eq!(
            fetch.json().expect("json")["ok"],
            Value::Bool(true),
            "a JSON body is not replaced by the serialised DOM"
        );
    }

    #[tokio::test]
    async fn a_body_chrome_no_longer_has_becomes_empty() {
        let mut capture = html_capture();
        let binary = record("https://shop.test/file.zip", 200, "application/zip");
        capture.first = Some(binary.clone());
        capture.last = Some(binary);
        let page = FakePage {
            url: "https://shop.test/file.zip".to_owned(),
            ..FakePage::default()
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &capture, proxy_meta(None))
            .await
            .expect("a response");
        assert!(fetch.body().is_empty());
    }

    #[tokio::test]
    async fn a_page_whose_content_cannot_be_read_yields_an_empty_body() {
        let page = FakePage {
            content_fails: true,
            url: "https://shop.test/".to_owned(),
            ..FakePage::default()
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &html_capture(), proxy_meta(None))
            .await
            .expect("the factory must not propagate a content failure");
        assert!(fetch.body().is_empty());
        assert_eq!(
            fetch.status, 200,
            "the status still comes from the response"
        );
    }

    #[tokio::test]
    async fn status_comes_from_the_final_response_and_headers_from_the_first() {
        let first = ResponseRecord {
            headers: headers(&[("content-type", "text/html"), ("x-first", "1")]),
            ..record("https://h/one", 200, "text/html")
        };
        let last = ResponseRecord {
            status: 403,
            reason: "Forbidden".to_owned(),
            headers: headers(&[("content-type", "text/html"), ("x-last", "1")]),
            ..record("https://h/two", 403, "text/html")
        };
        let capture = Capture {
            first: Some(first),
            last: Some(last),
            xhr: Vec::new(),
            history: Vec::new(),
        };
        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://h/two".to_owned(),
            ..FakePage::default()
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &capture, proxy_meta(None))
            .await
            .expect("a response");
        assert_eq!(fetch.status, 403);
        assert_eq!(fetch.reason, "Forbidden");
        assert_eq!(fetch.header("x-first"), Some("1"));
        assert_eq!(
            fetch.header("x-last"),
            None,
            "Python reads `headers` off the first response"
        );
    }

    #[tokio::test]
    async fn the_redirect_chain_survives_as_history() {
        let mut capture = html_capture();
        capture.history = vec![
            RedirectRecord {
                url: "http://shop.test/".to_owned(),
                status: 301,
                reason: "Moved Permanently".to_owned(),
                headers: headers(&[("location", "https://shop.test/")]),
                location: "https://shop.test/".to_owned(),
            },
            RedirectRecord {
                url: "https://shop.test/".to_owned(),
                status: 302,
                reason: "Found".to_owned(),
                headers: HeaderDict::new(),
                location: "https://shop.test/catalogue".to_owned(),
            },
        ];
        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://shop.test/catalogue".to_owned(),
            ..FakePage::default()
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &capture, proxy_meta(None))
            .await
            .expect("a response");
        assert_eq!(fetch.history.len(), 2);
        assert_eq!(fetch.history[0].status, 301);
        assert_eq!(fetch.history[0].url, "http://shop.test/");
        assert_eq!(fetch.history[1].location, "https://shop.test/catalogue");
    }

    #[tokio::test]
    async fn captured_xhr_responses_carry_their_bodies_and_no_history() {
        let mut capture = html_capture();
        capture.xhr = vec![ResponseRecord {
            request_id: "XHR1".to_owned(),
            url: "https://shop.test/api/items".to_owned(),
            resource_type: "xhr",
            headers: headers(&[("content-type", "application/json")]),
            ..record("https://shop.test/api/items", 200, "application/json")
        }];
        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://shop.test/".to_owned(),
            bodies: [("XHR1".to_owned(), Bytes::from_static(br#"[1,2,3]"#))]
                .into_iter()
                .collect(),
            ..FakePage::default()
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &capture, proxy_meta(None))
            .await
            .expect("a response");
        assert_eq!(fetch.captured_xhr.len(), 1);
        let xhr = &fetch.captured_xhr[0];
        assert_eq!(xhr.url, "https://shop.test/api/items");
        assert_eq!(xhr.body().as_ref(), b"[1,2,3]");
        assert!(xhr.history.is_empty(), "collect_history=False");
        assert!(xhr.cookies.is_empty(), "page=None means no cookies");
    }

    #[tokio::test]
    async fn a_capture_with_no_navigation_response_is_an_error() {
        let page = FakePage::default();
        let error = BrowserResponseFactory::new()
            .from_browser_page(&page, &Capture::default(), proxy_meta(None))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Failed to get a response from the page");
    }

    #[tokio::test]
    async fn the_proxy_rides_along_in_meta() {
        use browser_core::types::Proxy;

        let page = FakePage {
            content: RENDERED.to_owned(),
            url: "https://shop.test/".to_owned(),
            ..FakePage::default()
        };
        let proxy = Proxy::Config {
            server: "http://proxy:8080".into(),
            username: Some(String::new()),
            password: Some(String::new()),
        };
        let fetch = BrowserResponseFactory::new()
            .from_browser_page(&page, &html_capture(), proxy_meta(Some(&proxy)))
            .await
            .expect("a response");
        assert_eq!(
            fetch.proxy().and_then(|value| value.get("server")),
            Some(&Value::String("http://proxy:8080".to_owned()))
        );

        let no_proxy = BrowserResponseFactory::new()
            .from_browser_page(&page, &html_capture(), proxy_meta(None))
            .await
            .expect("a response");
        assert_eq!(
            no_proxy.meta.get(META_PROXY_KEY),
            Some(&Value::Null),
            "`meta={{\"proxy\": None}}` keeps the key, and unit 12's \
             `Response::proxy()` filters the null back out"
        );
        assert!(no_proxy.proxy().is_none());
    }

    #[test]
    fn the_browser_charset_regex_accepts_quoted_values() {
        let quoted = record("https://h/", 200, r#"text/html; charset="iso-8859-1""#);
        assert_eq!(browser_encoding(&quoted), "iso-8859-1");
        let bare = record("https://h/", 200, "text/html; charset=utf-8");
        assert_eq!(browser_encoding(&bare), "utf-8");
        let missing = record("https://h/", 200, "text/html");
        assert_eq!(browser_encoding(&missing), "utf-8");
    }
}
