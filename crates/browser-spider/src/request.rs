//! `Request` with [`SessionKwargs`], [`CallbackId`], memoised SHA-1 fingerprints, and
//! [`Request::copy`] which resets the fingerprint.
//!
//! A [`Request`] is the unit of work a crawl schedules. It is deliberately *data* — no
//! closures, no borrowed session handles — because it has to survive a checkpoint round
//! trip to disk and back. Python achieves that by pickling the callback's `__name__` and
//! re-binding it with `getattr` after load; the port replaces both halves with a
//! [`CallbackId`], which the engine resolves through the spider (`ARCHITECTURE.md` §5.5).
//!
//! # Fingerprints
//!
//! The dedup key is a 20-byte SHA-1 over a canonical JSON object built from the session id,
//! the request body, the method and [`canonicalize_url`]'d
//! URL — optionally extended with the remaining session kwargs and the headers. It is
//! **memoised**: once computed, [`Request::update_fingerprint`] ignores its arguments and
//! returns the cached value, exactly like Python's. Only [`Request::copy`] clears it.
//!
//! ```
//! use browser_spider::request::{FingerprintOptions, Request};
//!
//! let a = Request::new("http://example.com/?b=2&a=1");
//! let b = Request::new("http://example.com/?a=1&b=2");
//! // canonicalize_url sorts the query, so these two dedupe against each other.
//! assert_eq!(
//!     a.update_fingerprint(FingerprintOptions::default()),
//!     b.update_fingerprint(FingerprintOptions::default())
//! );
//! ```
//!
//! # Cross-language stability
//!
//! With the default options the fingerprint is byte-identical to Python's, and
//! `testdata/request_fingerprint.jsonl` pins that against the reference interpreter. With
//! `include_kwargs` it is **not** portable, and cannot be: Python falls back to `repr(value)`
//! for kwargs that are not JSON-serialisable, which has no Rust analogue
//! (spider-core spec §9.4 item 4).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};

use browser_core::types::{HttpMethod, Proxy};

use crate::canonicalize::{canonicalize_url, netloc_of, urlencode_form};

/// A scraped item: an ordered JSON object.
///
/// `serde_json`'s `Map` is an `IndexMap` in this workspace (`browser-core` turns on the
/// `preserve_order` feature), so items and `meta` keep the order the spider wrote them in —
/// which is what Python's `dict` does and what the CSV/XML exporters rely on.
pub type Item = serde_json::Map<String, Value>;

/// A 20-byte SHA-1 request fingerprint.
pub type Fingerprint = [u8; 20];

/// The name of a spider callback.
///
/// Python stores a bound method on the request and re-binds it by name after unpickling.
/// The port stores only the name: it is what actually persists, it keeps [`Request`]
/// `Serialize`, and it keeps the type `Send + Sync` without a `Box<dyn Fn>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallbackId(Cow<'static, str>);

impl CallbackId {
    /// `spider.parse` — the callback every request falls back to.
    pub const PARSE: Self = Self(Cow::Borrowed("parse"));

    /// A callback id from a `&'static str`, usable in a `const`.
    pub const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// A callback id from an owned name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(Cow::Owned(name.into()))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CallbackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&'static str> for CallbackId {
    fn from(name: &'static str) -> Self {
        Self::from_static(name)
    }
}

impl From<String> for CallbackId {
    fn from(name: String) -> Self {
        Self(Cow::Owned(name))
    }
}

/// A request body: Python's `data=` keyword.
///
/// Python accepts a `dict`, a list/tuple of pairs, a `str`, `bytes` or a `BytesIO`; the
/// first two are form-encoded for the fingerprint, the rest are used as-is.
///
/// [`RequestBody::Io`] is a separate variant from [`RequestBody::Bytes`] for one reason,
/// and it is not cosmetic: `Request.update_fingerprint` gates the `json=` fallback on
/// `if post_data:`, and a `BytesIO` defines neither `__bool__` nor `__len__`, so an **empty**
/// `BytesIO` is *truthy* while an empty `bytes` is falsy. `data=BytesIO(b"")` therefore
/// suppresses `json=` and `data=b""` does not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestBody {
    /// Form fields, in order. Fingerprinted as `urlencode(...)`, so duplicate keys and
    /// ordering both matter.
    Form(Vec<(String, String)>),
    /// A textual body, fingerprinted as its UTF-8 bytes.
    Text(String),
    /// A binary body, fingerprinted as-is. Empty means *falsy*, so `json=` takes over.
    Bytes(Vec<u8>),
    /// Python's `BytesIO`. Fingerprinted as its bytes, but **never falsy**, so it
    /// suppresses `json=` even when it holds nothing.
    Io(Vec<u8>),
}

impl RequestBody {
    /// Whether this body carries no bytes at all.
    ///
    /// Note this is *not* the fingerprint's fallback condition — see [`RequestBody::is_falsy`],
    /// which an empty [`RequestBody::Io`] answers differently.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Form(pairs) => pairs.is_empty(),
            Self::Text(text) => text.is_empty(),
            Self::Bytes(bytes) | Self::Io(bytes) => bytes.is_empty(),
        }
    }

    /// Python's truthiness test, which is what gates the `json=` fallback: an empty form,
    /// string or byte string is *falsy*, but a `BytesIO` never is.
    ///
    /// ```
    /// use browser_spider::request::RequestBody;
    ///
    /// assert!(RequestBody::Bytes(Vec::new()).is_falsy());
    /// // `bool(BytesIO(b""))` is True in Python — no `__bool__`, no `__len__`.
    /// assert!(!RequestBody::Io(Vec::new()).is_falsy());
    /// ```
    pub fn is_falsy(&self) -> bool {
        match self {
            Self::Io(_) => false,
            other => other.is_empty(),
        }
    }

    /// The bytes this body contributes to a fingerprint.
    fn to_fingerprint_bytes(&self) -> Vec<u8> {
        match self {
            Self::Form(pairs) => urlencode_form(pairs).into_bytes(),
            Self::Text(text) => text.as_bytes().to_vec(),
            Self::Bytes(bytes) | Self::Io(bytes) => bytes.clone(),
        }
    }
}

/// The `**kwargs` a [`Request`] forwards to its session.
///
/// Python keeps one untyped `dict`. The port names the seven keys the crawl core itself
/// inspects and keeps the rest in [`SessionKwargs::extra`], which is an `IndexMap` so it
/// keeps the order the caller wrote — load-bearing for the `include_kwargs` fingerprint,
/// where Python lower-cases every key and a later duplicate overwrites an earlier one.
///
/// Every field is `Option`, and "absent" is not the same as "default": Python only puts a
/// key in the fingerprint's `kwargs` tuple if the caller actually passed it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionKwargs {
    /// `method=`. Absent means `GET` for fingerprinting and for the fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<HttpMethod>,
    /// `data=` — the request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<RequestBody>,
    /// `json=` — a JSON body, used only when `data` is absent or empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    /// `headers=` — the HTTP-session spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// `extra_headers=` — the browser-session spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<IndexMap<String, String>>,
    /// `proxy=` — a single proxy, recorded in `CrawlStats::proxies` when used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// `proxies=` — a scheme-keyed proxy map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxies: Option<BTreeMap<String, String>>,
    /// Everything else, forwarded opaquely to the session, **in the order it was set**.
    ///
    /// The order matters exactly once: `include_kwargs` lower-cases every key, so two
    /// entries that differ only in case collapse into one, and Python — whose `**kwargs` is
    /// an insertion-ordered `dict` — keeps the *last* one. An `IndexMap` reproduces that;
    /// a sorted map would keep whichever key sorts last instead.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra: IndexMap<String, Value>,
}

impl SessionKwargs {
    /// The method, defaulting to `GET` as Python's `kwargs.get("method", "GET")` does.
    pub fn method(&self) -> HttpMethod {
        self.method.unwrap_or(HttpMethod::Get)
    }

    /// `headers or extra_headers or {}` — Python's truthiness chain, so an **empty**
    /// `headers` map falls through to `extra_headers`.
    pub fn effective_headers(&self) -> Option<&IndexMap<String, String>> {
        match self.headers.as_ref() {
            Some(h) if !h.is_empty() => Some(h),
            _ => match self.extra_headers.as_ref() {
                Some(h) if !h.is_empty() => Some(h),
                _ => None,
            },
        }
    }

    /// The kwargs as Python would see them for `include_kwargs`: every key that was
    /// actually set, lower-cased, minus `data` and `json`, mapped to its stable repr.
    fn stable_pairs(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(method) = self.method {
            out.insert(
                "method".to_owned(),
                stable_repr(&Value::from(method.as_str())),
            );
        }
        if let Some(headers) = &self.headers {
            out.insert("headers".to_owned(), stable_repr(&map_to_value(headers)));
        }
        if let Some(headers) = &self.extra_headers {
            out.insert(
                "extra_headers".to_owned(),
                stable_repr(&map_to_value(headers)),
            );
        }
        if let Some(proxy) = &self.proxy {
            out.insert(
                "proxy".to_owned(),
                stable_repr(&serde_json::to_value(proxy).unwrap_or(Value::Null)),
            );
        }
        if let Some(proxies) = &self.proxies {
            out.insert(
                "proxies".to_owned(),
                stable_repr(&serde_json::to_value(proxies).unwrap_or(Value::Null)),
            );
        }
        // Extras win over the named fields on a key collision, mirroring Python's dict
        // comprehension where a later duplicate overwrites an earlier one. Iterating
        // `extra` in insertion order is what makes two case-colliding extras resolve the
        // way Python resolves them.
        for (key, value) in &self.extra {
            out.insert(key.to_lowercase(), stable_repr(value));
        }
        out
    }
}

fn map_to_value(map: &IndexMap<String, String>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect(),
    )
}

/// `w3lib`-free equivalent of `orjson.dumps(value, OPT_SORT_KEYS, default=repr).decode()`.
///
/// Object keys are sorted recursively, matching `OPT_SORT_KEYS`.
fn stable_repr(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&map[k])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

/// Which optional parts a fingerprint covers.
///
/// These are the spider's `fp_include_kwargs` / `fp_include_headers` / `fp_keep_fragments`
/// class attributes, which the scheduler passes down on every enqueue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintOptions {
    /// Fold every session kwarg except `data`/`json` into the fingerprint.
    pub include_kwargs: bool,
    /// Fold the request headers into the fingerprint.
    pub include_headers: bool,
    /// Keep `#fragment` when canonicalising the URL.
    pub keep_fragments: bool,
}

impl FingerprintOptions {
    /// All three flags off — Python's defaults.
    pub const DEFAULT: Self = Self {
        include_kwargs: false,
        include_headers: false,
        keep_fragments: false,
    };
}

/// One unit of crawl work.
///
/// Construct with [`Request::new`] and the `with_*` builders; the fingerprint and the
/// cached domain are private because they memoise.
///
/// ```
/// use browser_spider::request::{CallbackId, Request};
///
/// let request = Request::new("https://example.com/page")
///     .with_sid("browser")
///     .with_priority(10)
///     .with_callback(CallbackId::from_static("parse_page"));
///
/// assert_eq!(request.domain(), "example.com");
/// assert_eq!(request.to_string(), "https://example.com/page");
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    /// The URL, exactly as given — never normalised in place.
    pub url: String,
    /// The session id. `""` means "resolve to the manager's default at enqueue time".
    #[serde(default)]
    pub sid: String,
    /// The callback to hand the response to. `None` means `spider.parse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<CallbackId>,
    /// Higher is dequeued first. Blocked retries decrement it, so it may go negative.
    #[serde(default)]
    pub priority: i32,
    /// Skip the dedup filter. The fingerprint is still recorded as seen.
    #[serde(default)]
    pub dont_filter: bool,
    /// Arbitrary user data, merged into the response's `meta`.
    #[serde(default)]
    pub meta: Item,
    /// How many times this request has already been retried after a blocked response.
    #[serde(default)]
    pub retry_count: u32,
    /// Everything forwarded to the session.
    #[serde(default)]
    pub session_kwargs: SessionKwargs,

    /// The memoised fingerprint. `None` until [`Request::update_fingerprint`] runs.
    #[serde(default, with = "fp_serde")]
    fp: OnceLock<Fingerprint>,
    /// The memoised netloc.
    #[serde(skip)]
    domain: OnceLock<String>,
}

/// Serialises the memoised fingerprint as lower-case hex (or `null`), so a checkpoint
/// round-trips it the way Python's pickle does.
mod fp_serde {
    use super::Fingerprint;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::OnceLock;

    pub(super) fn serialize<S: Serializer>(
        value: &OnceLock<Fingerprint>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value.get() {
            Some(fp) => serializer.serialize_some(&hex::encode(fp)),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OnceLock<Fingerprint>, D::Error> {
        let cell = OnceLock::new();
        if let Some(text) = Option::<String>::deserialize(deserializer)? {
            let mut fp = Fingerprint::default();
            hex::decode_to_slice(&text, &mut fp).map_err(serde::de::Error::custom)?;
            let _ = cell.set(fp);
        }
        Ok(cell)
    }
}

impl Request {
    /// A `GET` request for `url` with no session id, callback, priority or meta.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            sid: String::new(),
            callback: None,
            priority: 0,
            dont_filter: false,
            meta: Item::new(),
            retry_count: 0,
            session_kwargs: SessionKwargs::default(),
            fp: OnceLock::new(),
            domain: OnceLock::new(),
        }
    }

    /// Route this request through the session registered under `sid`.
    pub fn with_sid(mut self, sid: impl Into<String>) -> Self {
        self.sid = sid.into();
        self
    }

    /// Hand the response to `callback` instead of `spider.parse`.
    pub fn with_callback(mut self, callback: impl Into<CallbackId>) -> Self {
        self.callback = Some(callback.into());
        self
    }

    /// Set the queue priority; higher is dequeued first.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Bypass the dedup filter.
    pub fn with_dont_filter(mut self, dont_filter: bool) -> Self {
        self.dont_filter = dont_filter;
        self
    }

    /// Attach user data, merged into the response's `meta`.
    pub fn with_meta(mut self, meta: Item) -> Self {
        self.meta = meta;
        self
    }

    /// Replace the session kwargs wholesale.
    pub fn with_session_kwargs(mut self, kwargs: SessionKwargs) -> Self {
        self.session_kwargs = kwargs;
        self
    }

    /// Set the blocked-retry counter (the engine's `_retry_count`).
    pub fn with_retry_count(mut self, retry_count: u32) -> Self {
        self.retry_count = retry_count;
        self
    }

    /// Python's `Request.copy()`: same fields, a fresh `meta`, and **no fingerprint**.
    ///
    /// This is the difference between `copy()` and [`Clone`], which preserves the cached
    /// fingerprint. The engine's blocked-retry path relies on the reset: the copy has its
    /// priority and retry count changed, and must hash to a new value.
    ///
    /// ```
    /// use browser_spider::request::{FingerprintOptions, Request};
    ///
    /// let original = Request::new("http://example.com/");
    /// original.update_fingerprint(FingerprintOptions::default());
    /// assert!(original.fingerprint().is_some());
    ///
    /// let copy = original.copy();
    /// assert!(copy.fingerprint().is_none());
    /// assert!(original.clone().fingerprint().is_some()); // Clone keeps it
    /// ```
    pub fn copy(&self) -> Self {
        Self {
            url: self.url.clone(),
            sid: self.sid.clone(),
            callback: self.callback.clone(),
            priority: self.priority,
            dont_filter: self.dont_filter,
            meta: self.meta.clone(),
            retry_count: self.retry_count,
            session_kwargs: self.session_kwargs.clone(),
            fp: OnceLock::new(),
            domain: OnceLock::new(),
        }
    }

    /// The netloc of [`Request::url`] — **including the port and any userinfo**.
    ///
    /// `https://localhost:8080/x` has domain `localhost:8080`, so the per-domain limiter,
    /// the byte counters and `allowed_domains` all treat a port as part of the domain.
    /// Memoised, like Python's `cached_property`.
    pub fn domain(&self) -> &str {
        self.domain.get_or_init(|| netloc_of(&self.url))
    }

    /// The cached fingerprint, if one has been computed.
    pub fn fingerprint(&self) -> Option<Fingerprint> {
        self.fp.get().copied()
    }

    /// Compute — or return the memoised — SHA-1 fingerprint.
    ///
    /// **The arguments are ignored once a fingerprint exists.** That is Python's behaviour
    /// and it is load-bearing: a request re-enqueued after its URL changed keeps the
    /// original fingerprint, and only [`Request::copy`] escapes it.
    pub fn update_fingerprint(&self, options: FingerprintOptions) -> Fingerprint {
        *self.fp.get_or_init(|| self.compute_fingerprint(options))
    }

    /// Force the fingerprint to be recomputed on the next
    /// [`update_fingerprint`](Request::update_fingerprint).
    pub fn reset_fingerprint(&mut self) {
        self.fp = OnceLock::new();
    }

    /// Python's `__eq__`, without the panic.
    ///
    /// Returns `None` when either side has no fingerprint yet — Python raises
    /// `RuntimeError("Cannot compare requests before generating their fingerprints!")`
    /// there, which is not a thing a `PartialEq` impl may do, so equality is a method.
    pub fn same_fingerprint(&self, other: &Self) -> Option<bool> {
        Some(self.fingerprint()? == other.fingerprint()?)
    }

    /// Python's `__lt__` / `__gt__`: requests order by priority alone.
    pub fn priority_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }

    /// Python's `__repr__`: `<Request(url) priority=N callback=NAME>`.
    pub fn repr(&self) -> String {
        let callback = self
            .callback
            .as_ref()
            .map_or("None", |callback| callback.as_str());
        format!(
            "<Request({}) priority={} callback={}>",
            self.url, self.priority, callback
        )
    }

    /// Python's `_restore_callback(spider)`, applied after loading a checkpoint.
    ///
    /// `known` answers "does the spider have a callback by this name?". An unknown name
    /// falls back to [`CallbackId::PARSE`], exactly like `getattr(spider, name, None) or
    /// spider.parse`; a request that had no callback keeps none, and the engine will use
    /// `parse` for it anyway.
    pub fn restore_callback(&mut self, known: impl FnOnce(&CallbackId) -> bool) {
        if let Some(callback) = &self.callback {
            if !known(callback) {
                self.callback = Some(CallbackId::PARSE);
            }
        }
    }

    /// The body bytes this request contributes to its fingerprint.
    ///
    /// `data` wins when it is *truthy*; otherwise a truthy `json` is serialised; otherwise
    /// the body is empty. Note the truthiness: `data=""` and `json={}` both count as
    /// absent, but `data=BytesIO(b"")` does not — see [`RequestBody::is_falsy`].
    fn fingerprint_body(&self) -> Vec<u8> {
        match &self.session_kwargs.data {
            Some(body) if !body.is_falsy() => body.to_fingerprint_bytes(),
            _ => match &self.session_kwargs.json {
                Some(json) if is_truthy(json) => serde_json::to_vec(json).unwrap_or_default(),
                _ => Vec::new(),
            },
        }
    }

    fn compute_fingerprint(&self, options: FingerprintOptions) -> Fingerprint {
        let body = self.fingerprint_body();

        // The keys are inserted in sorted order so the serialised bytes match
        // `orjson.dumps(data, option=OPT_SORT_KEYS)`.
        let mut data = serde_json::Map::new();
        data.insert("body".to_owned(), Value::from(hex::encode(&body)));

        if options.include_headers {
            let mut processed: IndexMap<String, String> = IndexMap::new();
            if let Some(headers) = self.session_kwargs.effective_headers() {
                for (key, value) in headers {
                    // Keys are lower-cased, values are not: `X-Test: A` and `X-Test: a`
                    // are different requests.
                    processed.insert(
                        hex::encode(key.to_lowercase().as_bytes()),
                        hex::encode(value.as_bytes()),
                    );
                }
            }
            data.insert(
                "headers".to_owned(),
                Value::Array(
                    processed
                        .into_iter()
                        .map(|(k, v)| Value::Array(vec![Value::from(k), Value::from(v)]))
                        .collect(),
                ),
            );
        }

        if options.include_kwargs {
            data.insert(
                "kwargs".to_owned(),
                Value::Array(
                    self.session_kwargs
                        .stable_pairs()
                        .into_iter()
                        .map(|(k, v)| Value::Array(vec![Value::from(k), Value::from(v)]))
                        .collect(),
                ),
            );
        }

        data.insert(
            "method".to_owned(),
            Value::from(self.session_kwargs.method().as_str()),
        );
        data.insert("sid".to_owned(), Value::from(self.sid.as_str()));
        data.insert(
            "url".to_owned(),
            Value::from(canonicalize_url(&self.url, options.keep_fragments)),
        );

        let encoded = Value::Object(data).to_string();
        let digest = Sha1::digest(encoded.as_bytes());
        let mut fp = Fingerprint::default();
        fp.copy_from_slice(&digest);
        fp
    }
}

/// Python's truthiness for a JSON value: `None`, `false`, `0`, `""`, `[]` and `{}` are all
/// falsy, and a falsy `json=` produces an empty fingerprint body.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

impl std::fmt::Display for Request {
    /// Python's `__str__`: just the URL. Log lines interpolate requests this way.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp_of(request: &Request) -> Fingerprint {
        request.update_fingerprint(FingerprintOptions::default())
    }

    #[test]
    fn defaults_match_python() {
        let request = Request::new("http://example.com/");
        assert_eq!(request.sid, "");
        assert!(request.callback.is_none());
        assert_eq!(request.priority, 0);
        assert!(!request.dont_filter);
        assert!(request.meta.is_empty());
        assert_eq!(request.retry_count, 0);
        assert_eq!(request.session_kwargs, SessionKwargs::default());
        assert!(request.fingerprint().is_none());
    }

    #[test]
    fn meta_is_never_shared_between_requests() {
        let mut a = Request::new("http://example.com/");
        let b = Request::new("http://example.com/");
        a.meta.insert("k".to_owned(), Value::from(1));
        assert!(b.meta.is_empty());
    }

    #[test]
    fn domain_keeps_the_port() {
        assert_eq!(
            Request::new("http://localhost:8080/x").domain(),
            "localhost:8080"
        );
    }

    #[test]
    fn fingerprints_are_twenty_bytes_and_memoised() {
        let request = Request::new("http://example.com/");
        let first = fp_of(&request);
        assert_eq!(first.len(), 20);
        // Different options, but the memoised value wins.
        let second = request.update_fingerprint(FingerprintOptions {
            include_kwargs: true,
            include_headers: true,
            keep_fragments: true,
        });
        assert_eq!(first, second);
    }

    #[test]
    fn same_url_different_sid_is_a_different_request() {
        let a = Request::new("http://example.com/").with_sid("one");
        let b = Request::new("http://example.com/").with_sid("two");
        assert_ne!(fp_of(&a), fp_of(&b));
    }

    #[test]
    fn get_and_post_are_different_requests() {
        let get = Request::new("http://example.com/");
        let post = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            method: Some(HttpMethod::Post),
            ..SessionKwargs::default()
        });
        assert_ne!(fp_of(&get), fp_of(&post));
    }

    #[test]
    fn query_order_does_not_matter_but_fragments_do_not_count() {
        let a = Request::new("http://example.com/?b=2&a=1");
        let b = Request::new("http://example.com/?a=1&b=2#frag");
        assert_eq!(fp_of(&a), fp_of(&b));

        let c = Request::new("http://example.com/?a=1&b=2#frag");
        let d = Request::new("http://example.com/?a=1&b=2");
        let opts = FingerprintOptions {
            keep_fragments: true,
            ..FingerprintOptions::DEFAULT
        };
        assert_ne!(c.update_fingerprint(opts), d.update_fingerprint(opts));
    }

    #[test]
    fn an_empty_data_body_falls_through_to_json() {
        let empty_data = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            data: Some(RequestBody::Text(String::new())),
            json: Some(serde_json::json!({"a": 1})),
            ..SessionKwargs::default()
        });
        let json_only = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            json: Some(serde_json::json!({"a": 1})),
            ..SessionKwargs::default()
        });
        assert_eq!(fp_of(&empty_data), fp_of(&json_only));
    }

    #[test]
    fn an_empty_json_body_is_the_same_as_no_body() {
        let empty_json = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            json: Some(serde_json::json!({})),
            ..SessionKwargs::default()
        });
        assert_eq!(
            fp_of(&empty_json),
            fp_of(&Request::new("http://example.com/"))
        );
    }

    #[test]
    fn form_bodies_are_urlencoded_for_the_fingerprint() {
        let a = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            data: Some(RequestBody::Form(vec![("a".into(), "1 2".into())])),
            ..SessionKwargs::default()
        });
        let b = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            data: Some(RequestBody::Text("a=1+2".into())),
            ..SessionKwargs::default()
        });
        assert_eq!(fp_of(&a), fp_of(&b));
    }

    #[test]
    fn include_kwargs_distinguishes_extra_values() {
        let opts = FingerprintOptions {
            include_kwargs: true,
            ..FingerprintOptions::DEFAULT
        };
        let mut one = SessionKwargs::default();
        one.extra.insert("timeout".to_owned(), Value::from(1));
        let mut two = SessionKwargs::default();
        two.extra.insert("timeout".to_owned(), Value::from(2));

        let a = Request::new("http://example.com/").with_session_kwargs(one);
        let b = Request::new("http://example.com/").with_session_kwargs(two);
        assert_ne!(a.update_fingerprint(opts), b.update_fingerprint(opts));
    }

    #[test]
    fn include_kwargs_ignores_data_and_json() {
        let opts = FingerprintOptions {
            include_kwargs: true,
            ..FingerprintOptions::DEFAULT
        };
        let plain = Request::new("http://example.com/");
        let with_body = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            data: Some(RequestBody::Text("x".into())),
            ..SessionKwargs::default()
        });
        // The bodies still differ (via `body`), but the `kwargs` tuple must not mention
        // them; strip the body difference by comparing two bodyless requests whose only
        // difference is a `json` kwarg.
        let with_json = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            json: Some(serde_json::json!({})),
            ..SessionKwargs::default()
        });
        assert_eq!(
            plain.update_fingerprint(opts),
            with_json.update_fingerprint(opts)
        );
        assert_ne!(
            plain.update_fingerprint(opts),
            with_body.update_fingerprint(opts)
        );
    }

    #[test]
    fn include_headers_is_case_sensitive_on_values_only() {
        let opts = FingerprintOptions {
            include_headers: true,
            ..FingerprintOptions::DEFAULT
        };
        let make = |key: &str, value: &str| {
            let mut headers = IndexMap::new();
            headers.insert(key.to_owned(), value.to_owned());
            Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
                headers: Some(headers),
                ..SessionKwargs::default()
            })
        };
        assert_eq!(
            make("X-Test", "A").update_fingerprint(opts),
            make("x-test", "A").update_fingerprint(opts)
        );
        assert_ne!(
            make("X-Test", "A").update_fingerprint(opts),
            make("X-Test", "a").update_fingerprint(opts)
        );
    }

    #[test]
    fn include_headers_falls_back_to_extra_headers() {
        let opts = FingerprintOptions {
            include_headers: true,
            ..FingerprintOptions::DEFAULT
        };
        let mut headers = IndexMap::new();
        headers.insert("X-A".to_owned(), "1".to_owned());

        let via_headers = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            headers: Some(headers.clone()),
            ..SessionKwargs::default()
        });
        let via_extra = Request::new("http://example.com/").with_session_kwargs(SessionKwargs {
            headers: Some(IndexMap::new()),
            extra_headers: Some(headers),
            ..SessionKwargs::default()
        });
        assert_eq!(
            via_headers.update_fingerprint(opts),
            via_extra.update_fingerprint(opts)
        );
    }

    #[test]
    fn copy_resets_the_fingerprint_but_clone_does_not() {
        let mut original = Request::new("http://example.com/");
        original.meta.insert("a".to_owned(), Value::from(1));
        fp_of(&original);

        let copy = original.copy();
        assert!(copy.fingerprint().is_none());
        assert_eq!(copy.meta, original.meta);
        assert!(original.clone().fingerprint().is_some());
    }

    #[test]
    fn copy_gives_an_independent_meta() {
        let mut original = Request::new("http://example.com/");
        original.meta.insert("a".to_owned(), Value::from(1));
        let mut copy = original.copy();
        copy.meta.insert("b".to_owned(), Value::from(2));
        assert_eq!(original.meta.len(), 1);
        assert_eq!(copy.meta.len(), 2);
    }

    #[test]
    fn equality_needs_both_fingerprints() {
        let a = Request::new("http://example.com/");
        let b = Request::new("http://example.com/");
        assert_eq!(a.same_fingerprint(&b), None);
        fp_of(&a);
        assert_eq!(a.same_fingerprint(&b), None);
        fp_of(&b);
        assert_eq!(a.same_fingerprint(&b), Some(true));
    }

    #[test]
    fn priority_does_not_affect_equality() {
        let a = Request::new("http://example.com/").with_priority(10);
        let b = Request::new("http://example.com/").with_priority(-5);
        fp_of(&a);
        fp_of(&b);
        assert_eq!(a.same_fingerprint(&b), Some(true));
        assert_eq!(a.priority_cmp(&b), std::cmp::Ordering::Greater);
    }

    #[test]
    fn repr_matches_python() {
        let request = Request::new("http://example.com/").with_priority(3);
        assert_eq!(
            request.repr(),
            "<Request(http://example.com/) priority=3 callback=None>"
        );
        let named = request.with_callback(CallbackId::from_static("parse_item"));
        assert_eq!(
            named.repr(),
            "<Request(http://example.com/) priority=3 callback=parse_item>"
        );
    }

    #[test]
    fn restore_callback_falls_back_to_parse() {
        let mut request =
            Request::new("http://example.com/").with_callback(CallbackId::from_static("gone"));
        request.restore_callback(|_| false);
        assert_eq!(request.callback, Some(CallbackId::PARSE));

        let mut kept =
            Request::new("http://example.com/").with_callback(CallbackId::from_static("here"));
        kept.restore_callback(|_| true);
        assert_eq!(kept.callback.as_ref().map(CallbackId::as_str), Some("here"));

        let mut none = Request::new("http://example.com/");
        none.restore_callback(|_| false);
        assert!(none.callback.is_none());
    }

    #[test]
    fn round_trips_through_json_with_its_fingerprint() {
        let mut request = Request::new("http://example.com/?b=1&a=2")
            .with_sid("s")
            .with_priority(4)
            .with_dont_filter(true)
            .with_retry_count(2)
            .with_callback(CallbackId::from_static("cb"));
        request.meta.insert("depth".to_owned(), Value::from(3));
        request.session_kwargs.method = Some(HttpMethod::Post);
        request.session_kwargs.data = Some(RequestBody::Form(vec![("a".into(), "b".into())]));
        let fp = fp_of(&request);

        let text = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&text).expect("deserialize");

        assert_eq!(back.url, request.url);
        assert_eq!(back.sid, "s");
        assert_eq!(back.priority, 4);
        assert!(back.dont_filter);
        assert_eq!(back.retry_count, 2);
        assert_eq!(back.callback, request.callback);
        assert_eq!(back.meta, request.meta);
        assert_eq!(back.session_kwargs, request.session_kwargs);
        assert_eq!(back.fingerprint(), Some(fp));
        assert_eq!(back.domain(), "example.com");
    }

    #[test]
    fn a_request_without_a_fingerprint_round_trips_as_unset() {
        let request = Request::new("http://example.com/");
        let text = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&text).expect("deserialize");
        assert!(back.fingerprint().is_none());
    }

    #[test]
    fn serde_json_maps_preserve_insertion_order() {
        // The whole crate depends on this: `Item`, `meta` and `CrawlStats::to_dict` are
        // all `serde_json::Map`s whose order is observable. `browser-core` turns on
        // serde_json's `preserve_order` feature for the whole workspace; if that ever
        // goes away, fingerprints of `json=` bodies change and this test says so.
        let value: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).expect("parse");
        assert_eq!(value.to_string(), r#"{"b":1,"a":2}"#);
    }

    #[test]
    fn case_colliding_kwargs_resolve_the_way_python_resolves_them() {
        // `include_kwargs` lower-cases every key, so these two collapse into one entry and
        // Python's ordered `**kwargs` dict keeps the LAST spelling. A sorted `extra` map
        // kept whichever key sorted last instead.
        let fingerprint_of = |pairs: &[(&str, i64)]| {
            let mut kwargs = SessionKwargs::default();
            for (key, value) in pairs {
                kwargs.extra.insert((*key).to_owned(), Value::from(*value));
            }
            let request = Request::new("http://example.com/cc").with_session_kwargs(kwargs);
            request.update_fingerprint(FingerprintOptions {
                include_kwargs: true,
                ..FingerprintOptions::DEFAULT
            })
        };

        let only_one = fingerprint_of(&[("timeout", 1)]);
        assert_eq!(fingerprint_of(&[("TIMEOUT", 2), ("timeout", 1)]), only_one);
        assert_ne!(fingerprint_of(&[("timeout", 1), ("TIMEOUT", 2)]), only_one);
    }

    #[test]
    fn an_empty_bytesio_body_still_suppresses_the_json_fallback() {
        // `bool(BytesIO(b""))` is True — no `__bool__`, no `__len__` — so `if post_data:`
        // takes the data branch and `json=` never runs. `bool(b"")` is False.
        let with_io = Request::new("http://example.com/b").with_session_kwargs(SessionKwargs {
            data: Some(RequestBody::Io(Vec::new())),
            json: Some(serde_json::json!({"x": 1})),
            ..SessionKwargs::default()
        });
        let with_bytes = Request::new("http://example.com/b").with_session_kwargs(SessionKwargs {
            data: Some(RequestBody::Bytes(Vec::new())),
            json: Some(serde_json::json!({"x": 1})),
            ..SessionKwargs::default()
        });
        let bare = Request::new("http://example.com/b");

        assert_eq!(
            with_io.update_fingerprint(FingerprintOptions::DEFAULT),
            bare.update_fingerprint(FingerprintOptions::DEFAULT),
            "an empty BytesIO hashes as an empty body, not as the json payload"
        );
        assert_ne!(
            with_bytes.update_fingerprint(FingerprintOptions::DEFAULT),
            bare.update_fingerprint(FingerprintOptions::DEFAULT),
            "empty bytes are falsy, so json= takes over"
        );
    }
}
