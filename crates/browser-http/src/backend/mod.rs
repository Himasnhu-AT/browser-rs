//! The [`HttpBackend`] trait plus [`WireRequest`] / [`WireResponse`], so a second
//! transport can be added behind the same seam.
//!
//! `ARCHITECTURE.md` §2.3 defines the trait and ships exactly one implementation
//! for v1, [`wreq`](crate::backend::wreq) — a reqwest-shaped client over BoringSSL
//! whose browser emulation profiles are the only way to reproduce
//! curl-impersonate's JA3/JA4 and HTTP/2 fingerprints from Rust. A `reqwest`
//! backend can be added later at near-zero cost.
//!
//! # What lives on which side of the seam
//!
//! The backend performs **one** HTTP exchange and nothing else. It does not
//! follow redirects (that is [`crate::redirect_safe`], which needs to inspect
//! every hop), it does not retry (that is [`crate::retry`]), it does not merge
//! configuration (that is [`crate::merge`]) and it does not parse (that is
//! [`crate::response`]). Everything it needs arrives in one [`WireRequest`], and
//! everything it learned leaves in one [`WireResponse`].
//!
//! That split is what makes the whole driver testable without a network:
//! [`mock::MockBackend`] implements the same trait over a table of canned
//! replies, and every redirect, retry and proxy-rotation test in this crate runs
//! against it.

use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use indexmap::IndexMap;
use serde_json::Value;
use url::Url;

use crate::config::{Body, ClientCert, Cookies, HeaderDict, HttpVersion, ProxySpec, Query};
use crate::error::TransportError;
use browser_core::types::Proxy;

#[cfg(feature = "impersonate")]
pub mod wreq;

/// A scripted [`HttpBackend`] for tests — this crate's and yours.
///
/// `NETWORK`-free by construction: it answers from a table keyed by URL, or from
/// a queue of replies, and records everything it was asked. Every redirect,
/// retry, proxy-rotation and cookie test in this crate runs against it, and the
/// spider crate's `CrawlSession` tests can too.
///
/// # Examples
///
/// ```
/// use browser_http::backend::mock::{MockBackend, MockReply};
/// use browser_http::backend::{HttpBackend, WireRequest};
/// use url::Url;
///
/// let runtime = tokio::runtime::Builder::new_current_thread().build()?;
/// runtime.block_on(async {
///     let backend = MockBackend::new([
///         ("https://a.test/", MockReply::new(200).body("<p>hi</p>")),
///     ]);
///     let reply = backend
///         .execute(WireRequest::get(Url::parse("https://a.test/")?))
///         .await?;
///     assert_eq!(reply.status, 200);
///     assert_eq!(backend.requested(), ["https://a.test/"]);
///     Ok::<(), Box<dyn std::error::Error>>(())
/// })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub mod mock {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use bytes::Bytes;

    use super::{HttpBackend, WireRequest, WireResponse};
    use crate::config::HeaderDict;
    use crate::error::TransportError;

    /// A canned reply.
    #[derive(Clone, Debug)]
    pub struct MockReply {
        /// The status to answer with.
        pub status: u16,
        /// Response headers.
        pub headers: HeaderDict,
        /// The body.
        pub body: Bytes,
        /// A charset to claim the transport resolved.
        pub encoding: Option<String>,
        /// The peer address to claim.
        pub remote_addr: Option<std::net::SocketAddr>,
    }

    impl MockReply {
        /// An empty reply with `status`.
        #[must_use]
        pub fn new(status: u16) -> Self {
            Self {
                status,
                headers: HeaderDict::new(),
                body: Bytes::new(),
                encoding: None,
                remote_addr: None,
            }
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

        /// Claim a resolved charset.
        #[must_use]
        pub fn encoding(mut self, encoding: impl Into<String>) -> Self {
            self.encoding = Some(encoding.into());
            self
        }

        /// Claim a peer address, so the `"safe"` guard's post-flight check can
        /// be exercised.
        #[must_use]
        pub fn remote_addr(mut self, addr: std::net::SocketAddr) -> Self {
            self.remote_addr = Some(addr);
            self
        }

        fn into_response(self, request: &WireRequest) -> WireResponse {
            WireResponse {
                status: self.status,
                reason: browser_fingerprint::status::StatusText::get(self.status).to_owned(),
                url: request.effective_url().to_string(),
                set_cookies: self
                    .headers
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                    .flat_map(|(_, value)| value.split('\n'))
                    .map(str::to_owned)
                    .collect(),
                headers: self.headers,
                request_headers: request.headers.clone(),
                body: self.body,
                encoding: self.encoding,
                remote_addr: self.remote_addr,
            }
        }
    }

    #[derive(Debug)]
    enum Script {
        /// Answer by URL, ignoring order.
        Table(HashMap<String, MockReply>),
        /// Answer in order, whatever the URL.
        Queue(Mutex<std::collections::VecDeque<Result<MockReply, TransportError>>>),
        /// Always answer the same way.
        Always(Result<MockReply, TransportError>),
    }

    /// A scripted backend that records every request it is handed.
    #[derive(Debug)]
    pub struct MockBackend {
        script: Script,
        sent: Mutex<Vec<WireRequest>>,
    }

    impl MockBackend {
        /// Answer from a URL table. A URL that is not in the table produces a
        /// [`TransportError::Connect`], which is what an unreachable host looks
        /// like — never a silent 200.
        #[must_use]
        pub fn new<I, S>(entries: I) -> Self
        where
            I: IntoIterator<Item = (S, MockReply)>,
            S: Into<String>,
        {
            Self {
                script: Script::Table(
                    entries
                        .into_iter()
                        .map(|(url, reply)| (url.into(), reply))
                        .collect(),
                ),
                sent: Mutex::new(Vec::new()),
            }
        }

        /// Answer in order, whatever the URL — the shape a retry test needs.
        #[must_use]
        pub fn sequence<I>(replies: I) -> Self
        where
            I: IntoIterator<Item = Result<MockReply, TransportError>>,
        {
            Self {
                script: Script::Queue(Mutex::new(replies.into_iter().collect())),
                sent: Mutex::new(Vec::new()),
            }
        }

        /// Always answer the same way.
        #[must_use]
        pub fn always(reply: MockReply) -> Self {
            Self {
                script: Script::Always(Ok(reply)),
                sent: Mutex::new(Vec::new()),
            }
        }

        /// Always fail the same way.
        #[must_use]
        pub fn always_failing(error: TransportError) -> Self {
            Self {
                script: Script::Always(Err(error)),
                sent: Mutex::new(Vec::new()),
            }
        }

        /// Every request handed to the backend, in order.
        #[must_use]
        pub fn sent(&self) -> Vec<WireRequest> {
            self.sent.lock().expect("mock lock").clone()
        }

        /// The effective URL of every request, in order.
        #[must_use]
        pub fn requested(&self) -> Vec<String> {
            self.sent
                .lock()
                .expect("mock lock")
                .iter()
                .map(|request| request.effective_url().to_string())
                .collect()
        }

        /// How many requests were made.
        #[must_use]
        pub fn call_count(&self) -> usize {
            self.sent.lock().expect("mock lock").len()
        }
    }

    #[async_trait]
    impl HttpBackend for MockBackend {
        async fn execute(&self, request: WireRequest) -> Result<WireResponse, TransportError> {
            let url = request.effective_url().to_string();
            self.sent.lock().expect("mock lock").push(request.clone());

            let reply = match &self.script {
                Script::Table(table) => table
                    .get(&url)
                    .cloned()
                    .ok_or_else(|| TransportError::connect(format!("Failed to connect to {url}"))),
                Script::Queue(queue) => queue
                    .lock()
                    .expect("mock lock")
                    .pop_front()
                    .unwrap_or_else(|| {
                        Err(TransportError::other("mock backend ran out of replies"))
                    }),
                Script::Always(reply) => reply.clone(),
            };

            reply.map(|reply| reply.into_response(&request))
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }
}

/// The four HTTP methods the reference implementation exposes.
///
/// `SUPPORTED_HTTP_METHODS = Literal["GET","POST","PUT","DELETE"]`. The enum is
/// deliberately **closed**: `curl_cffi` can send `HEAD`, `PATCH` and `OPTIONS`,
/// and the reference implementation deliberately does not expose them, so adding a variant would be
/// a behaviour change rather than a convenience.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    /// `GET`.
    #[default]
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
}

impl Method {
    /// Every supported method, in the order the spec lists them.
    pub const ALL: [Self; 4] = [Self::Get, Self::Post, Self::Put, Self::Delete];

    /// The uppercase wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }

    /// Whether this method may carry a request body.
    ///
    /// `GET` may not — Python's `GetRequestParams` has no `data`/`json` keys,
    /// while `DataRequestParams` (POST/PUT/DELETE) does.
    #[must_use]
    pub const fn allows_body(self) -> bool {
        !matches!(self, Self::Get)
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Method {
    type Err = TransportError;

    /// Parse a method name, case-insensitively.
    ///
    /// # Errors
    ///
    /// [`TransportError::Build`] naming the four supported methods.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        for method in Self::ALL {
            if value.eq_ignore_ascii_case(method.as_str()) {
                return Ok(method);
            }
        }
        Err(TransportError::build(format!(
            "Unsupported HTTP method {value:?}; expected one of GET, POST, PUT, DELETE"
        )))
    }
}

impl From<Method> for http::Method {
    fn from(method: Method) -> Self {
        match method {
            Method::Get => Self::GET,
            Method::Post => Self::POST,
            Method::Put => Self::PUT,
            Method::Delete => Self::DELETE,
        }
    }
}

impl From<browser_core::types::HttpMethod> for Method {
    /// `browser-core` spells the same closed set of four verbs for the crawl core,
    /// which cannot depend on this crate's transport types.
    ///
    /// The two enums exist separately on purpose — `browser-spider`'s `Request` is
    /// serialisable into a checkpoint and a transport type has no business in that
    /// file format — and this conversion is the one seam between them, used by
    /// `browser-spider`'s `CrawlSession` adapters.
    fn from(method: browser_core::types::HttpMethod) -> Self {
        use browser_core::types::HttpMethod;
        match method {
            HttpMethod::Get => Self::Get,
            HttpMethod::Post => Self::Post,
            HttpMethod::Put => Self::Put,
            HttpMethod::Delete => Self::Delete,
        }
    }
}

impl From<Method> for browser_core::types::HttpMethod {
    /// The inverse of [`From<browser_core::types::HttpMethod>`].
    fn from(method: Method) -> Self {
        match method {
            Method::Get => Self::Get,
            Method::Post => Self::Post,
            Method::Put => Self::Put,
            Method::Delete => Self::Delete,
        }
    }
}

/// Everything one HTTP exchange needs.
///
/// The field names follow the merged Python request dictionary
/// (`http-fetch.md` §4.1) so the two can be read side by side. `allow_redirects`
/// and `max_redirects` are **absent** on purpose: the backend never follows a
/// redirect, [`crate::redirect_safe::follow_redirects`] does.
#[derive(Clone, Debug)]
pub struct WireRequest {
    /// The method.
    pub method: Method,
    /// The absolute target URL. `params` are appended by the backend, not baked
    /// in here, so a redirect can drop them.
    pub url: Url,
    /// Ordered, case-preserving request headers — Python's plain `dict`.
    pub headers: HeaderDict,
    /// `params=`: query-string pairs appended to `url`.
    pub params: Option<Query>,
    /// `cookies=`: cookies for this request only.
    pub cookies: Option<Cookies>,
    /// `auth=`: HTTP basic auth.
    pub auth: Option<(String, String)>,
    /// `data=` / `json=`.
    pub body: Option<Body>,
    /// Request timeout; `None` means no timeout, which is a legal value.
    pub timeout: Option<Duration>,
    /// The proxy chosen for this attempt.
    pub proxy: Option<Proxy>,
    /// The per-scheme proxy table, used when `proxy` is absent.
    pub proxies: ProxySpec,
    /// Basic auth for the proxy.
    pub proxy_auth: Option<(String, String)>,
    /// TLS peer verification.
    pub verify: bool,
    /// Client certificate.
    pub cert: Option<ClientCert>,
    /// The impersonation profile, already sampled out of any pool.
    pub impersonate: Option<String>,
    /// A forced HTTP version, e.g. from `http3=true`.
    pub http_version: Option<HttpVersion>,
    /// The pass-through option bag. A backend ignores what it does not know and
    /// says so at `debug` level.
    pub extra: IndexMap<String, Value>,
}

impl WireRequest {
    /// A bare request with every knob at its default.
    #[must_use]
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HeaderDict::new(),
            params: None,
            cookies: None,
            auth: None,
            body: None,
            timeout: None,
            proxy: None,
            proxies: ProxySpec::default(),
            proxy_auth: None,
            verify: true,
            cert: None,
            impersonate: None,
            http_version: None,
            extra: IndexMap::new(),
        }
    }

    /// A bare `GET`.
    #[must_use]
    pub fn get(url: Url) -> Self {
        Self::new(Method::Get, url)
    }

    /// Build the wire request for one attempt out of a merged configuration.
    ///
    /// This is the join between [`crate::merge::MergedRequest`] — pure policy,
    /// no I/O — and the transport. `merged.url` has already been resolved by the
    /// caller (it is a `String` there, an absolute [`Url`] here).
    ///
    /// A `GET` never carries a body: Python's `GetRequestParams` has no `data`
    /// or `json` key, so a body reaching a `GET` is a caller mistake and is
    /// dropped with a warning rather than silently sent.
    #[must_use]
    pub fn from_merged(method: Method, url: Url, merged: &crate::merge::MergedRequest) -> Self {
        let mut body = merged.body.clone();
        if body.is_some() && !method.allows_body() {
            tracing::warn!(
                method = method.as_str(),
                "a request body was supplied for a method that takes none; dropping it"
            );
            body = None;
        }
        Self {
            method,
            url,
            headers: merged.headers.clone(),
            params: merged.params.clone(),
            cookies: merged.cookies.clone(),
            auth: merged.auth.clone(),
            body,
            timeout: merged.timeout,
            proxy: merged.proxy_source.clone(),
            proxies: merged.proxies.clone(),
            proxy_auth: merged.proxy_auth.clone(),
            verify: merged.verify,
            cert: merged.cert.clone(),
            impersonate: merged.impersonate.clone(),
            http_version: merged.http_version,
            extra: merged.extra.clone(),
        }
    }

    /// Set a header, replacing any entry with the same **exact** name.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// The URL with `params` applied, which is what actually goes on the wire.
    ///
    /// `curl_cffi` appends `params` to whatever query the URL already has, so
    /// `?a=1` plus `params={"b": "2"}` becomes `?a=1&b=2`.
    #[must_use]
    pub fn effective_url(&self) -> Url {
        let mut url = self.url.clone();
        if let Some(params) = &self.params {
            if !params.is_empty() {
                let mut pairs = url.query_pairs_mut();
                for (key, value) in params {
                    pairs.append_pair(key, value);
                }
                drop(pairs);
            }
        }
        url
    }
}

/// The `Content-Type` a body implies when the caller named none.
///
/// `curl_cffi` labels a form `application/x-www-form-urlencoded`, `json=`
/// `application/json`, and any raw `data=` payload `application/octet-stream`.
/// The redirect follower needs the same answer as the backend does, because a
/// 301/302/303 drops the payload but keeps the header it implied.
#[must_use]
pub const fn implied_content_type(body: &crate::config::Body) -> Option<&'static str> {
    match body {
        crate::config::Body::Form(_) => Some("application/x-www-form-urlencoded"),
        crate::config::Body::Json(_) => Some("application/json"),
        crate::config::Body::Text(_) | crate::config::Body::Raw(_) => {
            Some("application/octet-stream")
        }
    }
}

/// Everything one HTTP exchange produced.
///
/// One hop only: `history` is assembled by
/// [`crate::redirect_safe::follow_redirects`] out of several of these.
#[derive(Clone, Debug)]
pub struct WireResponse {
    /// The status code.
    pub status: u16,
    /// The reason phrase from the wire, or the one
    /// [`browser_fingerprint::status::StatusText`] supplies.
    pub reason: String,
    /// The URL this reply came from, after any transport-level normalisation.
    pub url: String,
    /// Response headers, in wire order.
    ///
    /// A [`HeaderDict`] cannot hold a repeated name, so a backend folds repeats
    /// on `", "` — RFC 9110 §5.3, and byte for byte what `dict(response.headers)`
    /// produces in Python. That folding is lossy for `Set-Cookie` (an `Expires`
    /// attribute contains a comma of its own), which is why the unfolded values
    /// are also reported in [`WireResponse::set_cookies`].
    pub headers: HeaderDict,
    /// Every `Set-Cookie` line, unfolded and in wire order.
    ///
    /// `Response.cookies` and the session jar are both built from this, never
    /// from the folded header.
    pub set_cookies: Vec<String>,
    /// The headers actually sent, as far as the backend can report them. A
    /// transport that adds headers of its own (an impersonation profile does)
    /// should report the full set.
    pub request_headers: HeaderDict,
    /// The body, already decompressed but **not** decoded.
    pub body: Bytes,
    /// The charset the transport resolved from `Content-Type`, if it resolved
    /// one. `None` leaves the decision to
    /// [`crate::response::resolve_encoding`].
    pub encoding: Option<String>,
    /// The address the transport actually connected to, when it can say. The
    /// `"safe"` redirect guard re-checks it to catch DNS rebinding.
    pub remote_addr: Option<SocketAddr>,
}

impl WireResponse {
    /// A minimal reply, for tests and for backends with nothing else to report.
    #[must_use]
    pub fn new(status: u16, url: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            reason: browser_fingerprint::status::StatusText::get(status).to_owned(),
            url: url.into(),
            headers: HeaderDict::new(),
            set_cookies: Vec::new(),
            request_headers: HeaderDict::new(),
            body: body.into(),
            encoding: None,
            remote_addr: None,
        }
    }

    /// Whether the status is in `200..300` — `curl_cffi`'s `Response.ok`.
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// One HTTP transport.
///
/// Implementations must be cheap to clone or share behind an [`std::sync::Arc`]:
/// a session holds exactly one for its whole life, and the one-shot fetchers
/// share a process-wide instance.
///
/// # Contract
///
/// * Perform exactly one exchange. **Never** follow redirects.
/// * Return a [`WireResponse`] for *any* completed exchange, whatever the status
///   — a 404 and a 503 are successes here, because Python returns them as a
///   `Response` and never retries them.
/// * Return [`TransportError`] only for failures `curl_cffi` would raise
///   `CurlError` for, because that is exactly the set the retry loop retries.
#[async_trait]
pub trait HttpBackend: Send + Sync {
    /// Perform one request.
    ///
    /// # Errors
    ///
    /// [`TransportError`] for a transport-level failure. A completed exchange is
    /// always `Ok`, whatever its status code.
    async fn execute(&self, request: WireRequest) -> Result<WireResponse, TransportError>;

    /// A short name for logs and diagnostics.
    fn name(&self) -> &'static str {
        "http-backend"
    }

    /// Release pooled connections. The default does nothing.
    async fn close(&self) {}
}

#[async_trait]
impl<T: HttpBackend + ?Sized> HttpBackend for std::sync::Arc<T> {
    async fn execute(&self, request: WireRequest) -> Result<WireResponse, TransportError> {
        (**self).execute(request).await
    }

    fn name(&self) -> &'static str {
        (**self).name()
    }

    async fn close(&self) {
        (**self).close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_method_set_is_closed_and_round_trips() {
        assert_eq!(
            Method::ALL.map(Method::as_str),
            ["GET", "POST", "PUT", "DELETE"]
        );
        for method in Method::ALL {
            assert_eq!(
                method.as_str().parse::<Method>().expect("round trip"),
                method
            );
            assert_eq!(
                method
                    .as_str()
                    .to_lowercase()
                    .parse::<Method>()
                    .expect("case"),
                method
            );
        }
        // The methods the reference implementation deliberately does not expose stay unsupported.
        for unsupported in ["HEAD", "PATCH", "OPTIONS", "TRACE", "QUERY"] {
            let err = unsupported.parse::<Method>().expect_err(unsupported);
            assert!(err.to_string().contains("GET, POST, PUT, DELETE"));
        }
    }

    #[test]
    fn only_get_refuses_a_body() {
        assert!(!Method::Get.allows_body());
        assert!(Method::Post.allows_body());
        assert!(Method::Put.allows_body());
        assert!(Method::Delete.allows_body());
    }

    #[test]
    fn params_append_to_an_existing_query() {
        let mut request = WireRequest::get(Url::parse("https://a.test/x?a=1").expect("url"));
        request.params = Some(vec![
            ("b".to_owned(), "2".to_owned()),
            ("b".to_owned(), "3".to_owned()),
        ]);
        assert_eq!(
            request.effective_url().as_str(),
            "https://a.test/x?a=1&b=2&b=3"
        );

        // No params leaves the URL untouched, including its query.
        request.params = None;
        assert_eq!(request.effective_url().as_str(), "https://a.test/x?a=1");
        request.params = Some(Vec::new());
        assert_eq!(request.effective_url().as_str(), "https://a.test/x?a=1");
    }

    #[test]
    fn from_merged_drops_a_body_a_get_cannot_carry() {
        use crate::config::{FetcherConfig, RequestOverrides};
        use crate::merge::{merge_request_args, RequestContext};

        let overrides = RequestOverrides::new().body(Body::Text("a=1".to_owned()));
        let merged = merge_request_args(
            "https://a.test/",
            &FetcherConfig::default(),
            &overrides,
            &RequestContext::new(),
        );

        let url = Url::parse("https://a.test/").expect("url");
        assert!(WireRequest::from_merged(Method::Get, url.clone(), &merged)
            .body
            .is_none());
        assert!(WireRequest::from_merged(Method::Post, url, &merged)
            .body
            .is_some());
    }

    #[test]
    fn a_wire_response_fills_its_reason_from_the_status_table() {
        assert_eq!(WireResponse::new(200, "https://a.test/", "").reason, "OK");
        assert_eq!(
            WireResponse::new(451, "https://a.test/", "").reason,
            "Unavailable For Legal Reasons"
        );
        assert_eq!(
            WireResponse::new(599, "https://a.test/", "").reason,
            "Unknown Status Code"
        );
        assert!(WireResponse::new(204, "https://a.test/", "").ok());
        assert!(!WireResponse::new(304, "https://a.test/", "").ok());
    }
}
