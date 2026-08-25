//! [`ResponseCacheManager`]: the base64 JSON dev cache, which never raises from `put` and
//! has no TTL.
//!
//! Port of `reference/spiders/cache.py` (spec: `docs/specs/spider-support.md` §3).
//!
//! # What it is for
//!
//! "Development mode": cache every response on the first run and replay it from disk on
//! every later run, so a `parse()` callback can be iterated without hitting the network
//! again. It is *not* an HTTP cache — no `Cache-Control`, no `ETag`, no `Vary`, and
//! deliberately **no expiry of any kind**. Nothing in the file records a timestamp. To
//! force a fresh crawl, delete the directory or call [`ResponseCacheManager::clear`].
//!
//! # The key
//!
//! The raw 20-byte request fingerprint from
//! [`Request::update_fingerprint`](crate::request::Request::update_fingerprint), rendered
//! lower-case hex: `<cache_dir>/<40 hex chars>.json`. Because the fingerprint already
//! covers sid + method + canonical URL + body — and, depending on the spider's flags,
//! kwargs and headers too — flipping any of those flags changes every key and produces a
//! clean re-crawl for free.
//!
//! # The cookie shape rule
//!
//! Static engines hand back cookies as a flat object; the browser engines hand back an
//! array of full cookie dicts. Upstream regression #376 was a `put` that coerced anything
//! non-dict to `{}`, silently dropping every cookie from a browser-engine response. Here
//! [`FetchedResponse::cookies`] is already a [`serde_json::Value`], so both shapes survive
//! a round trip untouched — and [`ResponseCacheManager::put`] asserts that by refusing to
//! flatten an array.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use bytes::Bytes;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::request::Fingerprint;
use crate::session::FetchedResponse;

/// The directory the engine caches into when the spider does not name one:
/// `.browser-rs_cache/<spider name>`, relative to the current working directory.
pub const DEFAULT_CACHE_ROOT: &str = ".browser-rs_cache";

/// `.browser-rs_cache/{spider_name}` — the engine's default `development_cache_dir`.
///
/// ```
/// use browser_spider::cache::default_cache_dir;
///
/// assert_eq!(
///     default_cache_dir("quotes"),
///     std::path::Path::new(".browser-rs_cache/quotes")
/// );
/// ```
#[must_use]
pub fn default_cache_dir(spider_name: &str) -> PathBuf {
    Path::new(DEFAULT_CACHE_ROOT).join(spider_name)
}

/// The JSON document one cache file holds.
///
/// Field order is the order Python's dict literal writes them in, and `serde_json` emits
/// struct fields in declaration order, so a file written here is byte-identical to one
/// written by `orjson.dumps` for the same response. Nothing reads the file positionally,
/// though — `get` is order-insensitive.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheDocument {
    url: String,
    /// Standard-alphabet base64 **with** padding, matching `base64.b64encode`.
    content: String,
    status: u16,
    reason: String,
    encoding: String,
    /// An object for the static engines, an array for the browser engines. Never coerced.
    cookies: Value,
    headers: IndexMap<String, String>,
    request_headers: IndexMap<String, String>,
    method: String,
}

/// What a cache hit restores.
///
/// Python reconstructs a `Response`, whose constructor takes `method` but does not keep it
/// (`method` only ever reaches a log line). The port keeps it instead of throwing it away,
/// so the engine can log the same "Fetched (GET) …" line on a replay as on a live fetch.
#[derive(Clone, Debug)]
pub struct CacheEntry {
    /// The replayed response. `history` is empty, `meta` is empty and `request` is `None` —
    /// the engine fills those in, exactly as in Python.
    pub response: FetchedResponse,
    /// The request method that produced it.
    pub method: String,
}

/// On-disk replay of responses, keyed by request fingerprint.
///
/// Every method takes `&self`; the engine holds one `Arc<ResponseCacheManager>` for the
/// whole crawl.
///
/// ```no_run
/// # async fn demo() {
/// use browser_spider::cache::ResponseCacheManager;
/// use browser_spider::session::FetchedResponse;
///
/// let cache = ResponseCacheManager::new("/tmp/browser-rs-cache-demo");
/// let fingerprint = [7u8; 20];
///
/// let response = FetchedResponse::new(200, "https://example.com/", "<html/>");
/// cache.put(&fingerprint, &response, "GET").await;
///
/// let replayed = cache.get(&fingerprint).await.expect("a cache hit");
/// assert_eq!(replayed.body.as_ref(), b"<html/>");
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct ResponseCacheManager {
    cache_dir: PathBuf,
}

impl ResponseCacheManager {
    /// Point a cache at a directory. The directory is created lazily, on the first
    /// [`ResponseCacheManager::put`].
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// The directory this cache writes to.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `<cache_dir>/<fingerprint hex>.json`.
    #[must_use]
    pub fn cache_path(&self, fingerprint: &Fingerprint) -> PathBuf {
        self.cache_dir
            .join(format!("{}.json", hex::encode(fingerprint)))
    }

    /// `<cache_dir>/<fingerprint hex>.tmp` — `Path.with_suffix(".tmp")`.
    fn temp_path(&self, fingerprint: &Fingerprint) -> PathBuf {
        self.cache_dir
            .join(format!("{}.tmp", hex::encode(fingerprint)))
    }

    /// Replay a cached response, or `None` on a miss.
    ///
    /// A miss is any of: no file, an unreadable file, a truncated or corrupt JSON document,
    /// a missing key, or base64 that will not decode. None of them raise — a broken cache
    /// entry is a cache miss, never a crawl failure.
    pub async fn get(&self, fingerprint: &Fingerprint) -> Option<FetchedResponse> {
        self.get_entry(fingerprint)
            .await
            .map(|entry| entry.response)
    }

    /// [`ResponseCacheManager::get`], keeping the cached request method as well.
    pub async fn get_entry(&self, fingerprint: &Fingerprint) -> Option<CacheEntry> {
        let path = self.cache_path(fingerprint);
        // Python checks `await path.exists()` first and only then opens; a missing file is
        // an ordinary miss and is not worth a warning.
        match tokio::fs::read(&path).await {
            Ok(bytes) => match Self::decode(&bytes) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::warn!(
                        fingerprint = %hex::encode(fingerprint),
                        %error,
                        "Failed to read cached response"
                    );
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(
                    fingerprint = %hex::encode(fingerprint),
                    %error,
                    "Failed to read cached response"
                );
                None
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<CacheEntry, CacheReadError> {
        let document: CacheDocument = serde_json::from_slice(bytes)?;
        let body = base64::engine::general_purpose::STANDARD
            .decode(discard_non_base64(&document.content))?;

        let response = FetchedResponse {
            status: document.status,
            reason: document.reason,
            url: document.url,
            body: Bytes::from(body),
            encoding: document.encoding,
            headers: document.headers,
            request_headers: document.request_headers,
            // The array/object distinction is the whole point; it is carried through
            // unmodified. See the module docs.
            cookies: document.cookies,
            history: Vec::new(),
            meta: crate::request::Item::new(),
            request: None,
        };
        Ok(CacheEntry {
            response,
            method: document.method,
        })
    }

    /// Store a response under `fingerprint`, overwriting any previous entry.
    ///
    /// The write is a temp file plus an atomic rename, so a crash never leaves a
    /// half-written entry that would replay as a truncated page. `tokio::fs::rename` has
    /// replace semantics on every platform, matching Python's `Path.replace` (a plain
    /// `rename` would fail on Windows when the destination exists).
    ///
    /// **This never fails the crawl.** Every error — a directory that cannot be created, a
    /// full disk, a serialisation problem — is logged at `warn` and swallowed; the temp
    /// file is removed on the way out. The `bool` says whether the entry landed, which is
    /// the one place the port is more informative than Python's `None` (and, unlike
    /// Python, a `mkdir` failure is swallowed here too rather than propagating out of
    /// `put`).
    pub async fn put(
        &self,
        fingerprint: &Fingerprint,
        response: &FetchedResponse,
        method: &str,
    ) -> bool {
        match self.try_put(fingerprint, response, method).await {
            Ok(()) => true,
            Err(error) => {
                // Best-effort cleanup, exactly like the Python `except` arm.
                let _ = tokio::fs::remove_file(self.temp_path(fingerprint)).await;
                tracing::warn!(
                    fingerprint = %hex::encode(fingerprint),
                    %error,
                    "Failed to cache response"
                );
                false
            }
        }
    }

    async fn try_put(
        &self,
        fingerprint: &Fingerprint,
        response: &FetchedResponse,
        method: &str,
    ) -> Result<(), CacheWriteError> {
        tokio::fs::create_dir_all(&self.cache_dir).await?;

        let document = CacheDocument {
            url: response.url.clone(),
            content: base64::engine::general_purpose::STANDARD.encode(&response.body),
            status: response.status,
            reason: response.reason.clone(),
            encoding: response.encoding.clone(),
            cookies: normalise_cookies(&response.cookies),
            headers: response.headers.clone(),
            request_headers: response.request_headers.clone(),
            method: method.to_owned(),
        };
        let serialized = serde_json::to_vec(&document)?;

        let temp_path = self.temp_path(fingerprint);
        tokio::fs::write(&temp_path, &serialized).await?;
        tokio::fs::rename(&temp_path, self.cache_path(fingerprint)).await?;
        Ok(())
    }

    /// Delete every cached response.
    ///
    /// Only `*.json` entries are removed; a stray `.tmp` from an interrupted write is left
    /// alone, which is Python's behaviour. A missing directory is a no-op.
    pub async fn clear(&self) {
        let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(
                    directory = %self.cache_dir.display(),
                    %error,
                    "Failed to clear the response cache"
                );
                return;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                if let Err(error) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(
                        file = %path.display(),
                        %error,
                        "Failed to remove a cached response"
                    );
                }
            }
        }
        tracing::info!(directory = %self.cache_dir.display(), "Cleared response cache");
    }

    /// How many entries the cache currently holds. Not a Python method; handy for tests
    /// and for the engine's start-up log line.
    pub async fn len(&self) -> usize {
        let Ok(mut entries) = tokio::fs::read_dir(&self.cache_dir).await else {
            return 0;
        };
        let mut count = 0usize;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                count += 1;
            }
        }
        count
    }

    /// Whether the cache holds nothing.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Python's `list(cookies) if isinstance(cookies, tuple) else dict(cookies)`.
///
/// An array stays an array (browser engines) and an object stays an object (static
/// engines). A `null` — the [`FetchedResponse::default`] value — becomes `{}`, because
/// Python's `dict(None)` would raise and lose the whole entry over a response that simply
/// carried no cookies. Anything else (a bare string or number, which no real engine
/// produces) is also flattened to `{}` and logged, since `dict()` would raise there too.
fn normalise_cookies(cookies: &Value) -> Value {
    match cookies {
        Value::Array(_) | Value::Object(_) => cookies.clone(),
        Value::Null => Value::Object(serde_json::Map::new()),
        other => {
            tracing::debug!(
                cookies = %other,
                "response cookies were neither an object nor an array; caching them as {{}}"
            );
            Value::Object(serde_json::Map::new())
        }
    }
}

/// Why a cache entry could not be read. Never escapes the module: `get` turns it into a miss.
#[derive(Debug, thiserror::Error)]
enum CacheReadError {
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Base64(#[from] base64::DecodeError),
}

/// Why a cache entry could not be written. Never escapes the module: `put` logs and swallows.
#[derive(Debug, thiserror::Error)]
enum CacheWriteError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

/// Everything CPython's `base64.b64decode` would feed its decoder.
///
/// `b64decode(s, validate=False)` — the default — **silently discards every character
/// outside the base64 alphabet** before it checks the padding, so a `"content"` of
/// `" aGk= "` is `b"hi"` and one of `"!!!!"` is an empty body rather than an error. Both
/// really do come back as cache hits in Python, so the port strips the same characters.
/// Padding is still checked afterwards, which is why `"aGk"` remains a miss on both sides.
fn discard_non_base64(content: &str) -> Vec<u8> {
    content
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(seed: u8) -> Fingerprint {
        [seed; 20]
    }

    fn sample() -> FetchedResponse {
        let mut headers = IndexMap::new();
        headers.insert("Content-Type".to_owned(), "text/html".to_owned());
        headers.insert("X-Trace".to_owned(), "abc".to_owned());
        let mut request_headers = IndexMap::new();
        request_headers.insert("User-Agent".to_owned(), "browser-rs".to_owned());

        FetchedResponse {
            status: 201,
            reason: "Created".to_owned(),
            url: "https://example.com/page?a=1".to_owned(),
            body: Bytes::from_static(b"<html><body>hi</body></html>"),
            encoding: "utf-8".to_owned(),
            headers,
            request_headers,
            cookies: serde_json::json!({"session": "abc"}),
            history: vec!["https://example.com/old".to_owned()],
            meta: crate::request::Item::new(),
            request: None,
        }
    }

    #[tokio::test]
    async fn round_trip_preserves_every_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        let original = sample();
        assert!(cache.put(&fingerprint(1), &original, "POST").await);

        let entry = cache.get_entry(&fingerprint(1)).await.expect("hit");
        assert_eq!(entry.method, "POST");
        let replayed = entry.response;
        assert_eq!(replayed.url, original.url);
        assert_eq!(replayed.body, original.body);
        assert_eq!(replayed.status, original.status);
        assert_eq!(replayed.reason, original.reason);
        assert_eq!(replayed.encoding, original.encoding);
        assert_eq!(replayed.headers, original.headers);
        assert_eq!(replayed.request_headers, original.request_headers);
        assert_eq!(replayed.cookies, original.cookies);
        // Not persisted: the engine restores these.
        assert!(replayed.history.is_empty());
        assert!(replayed.meta.is_empty());
        assert!(replayed.request.is_none());
    }

    #[tokio::test]
    async fn the_file_is_named_after_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        cache.put(&fingerprint(0xab), &sample(), "GET").await;
        let expected = dir.path().join(format!("{}.json", "ab".repeat(20)));
        assert!(expected.exists(), "{}", expected.display());
    }

    #[tokio::test]
    async fn the_document_shape_matches_python() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        cache.put(&fingerprint(2), &sample(), "GET").await;
        let raw = tokio::fs::read_to_string(cache.cache_path(&fingerprint(2)))
            .await
            .expect("read");
        // Compact, no trailing newline, no indentation — `orjson.dumps` defaults.
        assert!(!raw.ends_with('\n'));
        assert!(!raw.contains(": "));
        assert!(!raw.contains(", "));

        // Keys in Python's dict-literal order. `serde_json` is built with `preserve_order`
        // across this workspace, so the parsed object keeps the file's order.
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        let object = parsed.as_object().expect("an object");
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "url",
                "content",
                "status",
                "reason",
                "encoding",
                "cookies",
                "headers",
                "request_headers",
                "method"
            ]
        );
        // The body really is base64 of the raw bytes, not an escaped string.
        assert_eq!(
            object["content"],
            Value::from("PGh0bWw+PGJvZHk+aGk8L2JvZHk+PC9odG1sPg==")
        );
    }

    #[tokio::test]
    async fn browser_cookies_stay_an_array_and_static_cookies_stay_an_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());

        let mut browser = sample();
        browser.cookies = serde_json::json!([
            {"name": "sid", "value": "1", "domain": ".example.com", "path": "/",
             "expires": -1.0, "httpOnly": true, "secure": true, "sameSite": "Lax"}
        ]);
        cache.put(&fingerprint(3), &browser, "GET").await;
        let replayed = cache.get(&fingerprint(3)).await.expect("hit");
        assert!(replayed.cookies.is_array());
        assert_eq!(replayed.cookies, browser.cookies);

        let static_response = sample();
        cache.put(&fingerprint(4), &static_response, "GET").await;
        let replayed = cache.get(&fingerprint(4)).await.expect("hit");
        assert!(replayed.cookies.is_object());
        assert_eq!(replayed.cookies, static_response.cookies);
    }

    #[tokio::test]
    async fn null_cookies_become_an_empty_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        let response = FetchedResponse::new(200, "https://example.com/", "x");
        assert!(response.cookies.is_null());
        cache.put(&fingerprint(5), &response, "GET").await;
        let replayed = cache.get(&fingerprint(5)).await.expect("hit");
        assert_eq!(replayed.cookies, serde_json::json!({}));
    }

    #[tokio::test]
    async fn a_second_put_overwrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        let mut first = sample();
        first.body = Bytes::from_static(b"first");
        cache.put(&fingerprint(6), &first, "GET").await;

        let mut second = sample();
        second.body = Bytes::from_static(b"second");
        assert!(cache.put(&fingerprint(6), &second, "GET").await);

        let replayed = cache.get(&fingerprint(6)).await.expect("hit");
        assert_eq!(replayed.body.as_ref(), b"second");
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn a_missing_file_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        assert!(cache.get(&fingerprint(7)).await.is_none());
    }

    #[tokio::test]
    async fn a_corrupt_file_is_a_miss_and_never_raises() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        tokio::fs::write(cache.cache_path(&fingerprint(8)), b"not valid json{{{")
            .await
            .expect("write");
        assert!(cache.get(&fingerprint(8)).await.is_none());
    }

    #[tokio::test]
    async fn a_missing_key_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        tokio::fs::write(
            cache.cache_path(&fingerprint(9)),
            br#"{"url":"https://example.com/","status":200}"#,
        )
        .await
        .expect("write");
        assert!(cache.get(&fingerprint(9)).await.is_none());
    }

    #[tokio::test]
    async fn undecodable_base64_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        tokio::fs::write(
            cache.cache_path(&fingerprint(10)),
            br#"{"url":"u","content":"!!!not base64!!!","status":200,"reason":"OK",
                 "encoding":"utf-8","cookies":{},"headers":{},"request_headers":{},"method":"GET"}"#,
        )
        .await
        .expect("write");
        assert!(cache.get(&fingerprint(10)).await.is_none());
    }

    #[tokio::test]
    async fn every_byte_survives_base64() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        let body: Vec<u8> = (0..=255u8).collect();
        let mut response = sample();
        response.body = Bytes::from(body.clone());
        cache.put(&fingerprint(11), &response, "GET").await;
        let replayed = cache.get(&fingerprint(11)).await.expect("hit");
        assert_eq!(replayed.body.as_ref(), &body[..]);
    }

    #[tokio::test]
    async fn put_creates_nested_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a").join("b").join("c");
        let cache = ResponseCacheManager::new(&nested);
        assert!(cache.put(&fingerprint(12), &sample(), "GET").await);
        assert!(nested.join(format!("{}.json", "0c".repeat(20))).exists());
    }

    #[tokio::test]
    async fn clear_removes_json_and_leaves_tmp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        cache.put(&fingerprint(13), &sample(), "GET").await;
        cache.put(&fingerprint(14), &sample(), "GET").await;
        let stray = dir.path().join("deadbeef.tmp");
        tokio::fs::write(&stray, b"junk").await.expect("write");

        assert_eq!(cache.len().await, 2);
        cache.clear().await;
        assert_eq!(cache.len().await, 0);
        assert!(cache.is_empty().await);
        assert!(stray.exists(), "a stray .tmp is left alone");
    }

    #[tokio::test]
    async fn clear_on_a_missing_directory_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path().join("never-created"));
        cache.clear().await;
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn put_never_raises_when_the_directory_is_unusable() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A *file* where the cache directory should be: `create_dir_all` cannot succeed.
        let blocked = dir.path().join("blocked");
        tokio::fs::write(&blocked, b"i am not a directory")
            .await
            .expect("write");
        let cache = ResponseCacheManager::new(&blocked);
        assert!(!cache.put(&fingerprint(15), &sample(), "GET").await);
        assert!(cache.get(&fingerprint(15)).await.is_none());
    }

    #[tokio::test]
    async fn no_temp_file_survives_a_successful_put() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        cache.put(&fingerprint(16), &sample(), "GET").await;
        let mut entries = tokio::fs::read_dir(dir.path()).await.expect("read_dir");
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec![format!("{}.json", "10".repeat(20))]);
    }

    #[tokio::test]
    async fn there_is_no_ttl() {
        // Nothing in the document records a time, so an entry written "long ago" replays.
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        cache.put(&fingerprint(17), &sample(), "GET").await;
        let raw = tokio::fs::read_to_string(cache.cache_path(&fingerprint(17)))
            .await
            .expect("read");
        for forbidden in ["timestamp", "expires_at", "cached_at", "ttl"] {
            assert!(
                !raw.contains(forbidden),
                "{forbidden} leaked into the cache file"
            );
        }
        assert!(cache.get(&fingerprint(17)).await.is_some());
    }

    #[tokio::test]
    async fn a_different_fingerprint_is_a_different_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(dir.path());
        cache.put(&fingerprint(18), &sample(), "GET").await;
        assert!(cache.get(&fingerprint(19)).await.is_none());
        assert!(cache.get(&fingerprint(18)).await.is_some());
    }

    #[test]
    fn default_cache_dir_is_relative_to_cwd() {
        assert_eq!(
            default_cache_dir("quotes"),
            Path::new(".browser-rs_cache").join("quotes")
        );
    }
}
