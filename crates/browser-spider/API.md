# `browser-spider` — data-plane cheat sheet

What work unit 15 built, for whoever writes units 16 (`throttle` / `robots` / `cache`) and 17
(`engine` / `spider`). Everything below is implemented, tested and differentially checked
against Python; nothing here is a sketch.

Modules owned by unit 15: `request`, `canonicalize`, `scheduler`, `session`, `stats`, `items`,
`checkpoint`, `error`. Modules still stubbed: `throttle`, `robots`, `cache`, `engine`,
`spider`.

---

## `request` — the unit of work

```rust
pub type Item = serde_json::Map<String, serde_json::Value>;  // ordered (preserve_order is on)
pub type Fingerprint = [u8; 20];

pub struct CallbackId(/* private */);
impl CallbackId {
    pub const PARSE: Self;                       // "parse"
    pub const fn from_static(name: &'static str) -> Self;
    pub fn new(name: impl Into<String>) -> Self;
    pub fn as_str(&self) -> &str;
}

pub enum RequestBody {
    Form(Vec<(String, String)>),
    Text(String),
    Bytes(Vec<u8>),                              // empty => FALSY, so `json=` takes over
    Io(Vec<u8>),                                 // Python's BytesIO: NEVER falsy, even empty
}
impl RequestBody {
    pub fn is_empty(&self) -> bool;              // byte length is zero
    pub fn is_falsy(&self) -> bool;              // Python truthiness; gates the `json=` fallback
}

pub struct SessionKwargs {
    pub method: Option<HttpMethod>,              // browser_core::types::HttpMethod
    pub data: Option<RequestBody>,
    pub json: Option<Value>,
    pub headers: Option<IndexMap<String, String>>,
    pub extra_headers: Option<IndexMap<String, String>>,
    pub proxy: Option<Proxy>,                    // browser_core::types::Proxy
    pub proxies: Option<BTreeMap<String, String>>,
    pub extra: IndexMap<String, Value>,          // INSERTION-ordered: see the note below
}
impl SessionKwargs {
    pub fn method(&self) -> HttpMethod;                                  // defaults to GET
    pub fn effective_headers(&self) -> Option<&IndexMap<String, String>>; // headers or extra_headers
}
```

Two of these bite, and both are pinned by `testdata/u15_request_fingerprint.jsonl`:

* **`extra` is insertion-ordered, and that is load-bearing.** `include_kwargs` lower-cases
  every key before hashing, so `timeout=` and `TIMEOUT=` collapse into one entry — and
  Python, whose `**kwargs` is an ordered `dict`, keeps the **last** spelling. A sorted map
  would keep whichever key sorts last instead. Do not "simplify" it to a `BTreeMap`.
* **`RequestBody::Io` is not `Bytes` with a different name.** `update_fingerprint` gates
  the `json=` fallback on `if post_data:`, and a `BytesIO` defines neither `__bool__` nor
  `__len__` — so `data=BytesIO(b"")` suppresses `json=` and `data=b""` does not. Use
  `is_falsy()`, never `is_empty()`, when deciding whether a body counts.

```rust

pub struct FingerprintOptions { pub include_kwargs: bool, pub include_headers: bool, pub keep_fragments: bool }
impl FingerprintOptions { pub const DEFAULT: Self; }

pub struct Request {
    pub url: String,
    pub sid: String,                    // "" until the engine normalises it
    pub callback: Option<CallbackId>,   // None => spider.parse
    pub priority: i32,
    pub dont_filter: bool,
    pub meta: Item,
    pub retry_count: u32,
    pub session_kwargs: SessionKwargs,
    // private: memoised fingerprint + memoised domain
}
impl Request {
    pub fn new(url: impl Into<String>) -> Self;
    pub fn with_sid / with_callback / with_priority / with_dont_filter
           / with_meta / with_session_kwargs / with_retry_count (builders, take self)

    pub fn copy(&self) -> Self;                    // Python's copy(): RESETS the fingerprint
    pub fn domain(&self) -> &str;                  // netloc, port and userinfo included
    pub fn fingerprint(&self) -> Option<Fingerprint>;
    pub fn update_fingerprint(&self, options: FingerprintOptions) -> Fingerprint;  // memoised
    pub fn reset_fingerprint(&mut self);
    pub fn same_fingerprint(&self, other: &Self) -> Option<bool>;   // None if either is unset
    pub fn priority_cmp(&self, other: &Self) -> Ordering;
    pub fn repr(&self) -> String;                  // <Request(url) priority=N callback=NAME>
    pub fn restore_callback(&mut self, known: impl FnOnce(&CallbackId) -> bool);
}
```

Things that will bite if you forget them:

* **`Clone` keeps the fingerprint, `copy()` clears it.** The blocked-retry path in
  `_process_request` must use `copy()`, or the retry dedupes against the original.
* `update_fingerprint` **ignores its arguments** once a fingerprint exists.
* `Display` is the URL (Python's `__str__`); `repr()` is the angle form.
* There is no `PartialEq` and no `Hash`, on purpose — Python's `__eq__` raises when a
  fingerprint is missing.

## `canonicalize` — w3lib's URL canonicaliser

```rust
pub fn canonicalize_url(url: &str, keep_fragments: bool) -> String;
pub fn canonicalize_url_with(url: &str, keep_blank_values: bool, keep_fragments: bool) -> String;
pub fn urlparse(url: &str) -> UrlParts;          // CPython's six-field ParseResult
pub fn netloc_of(url: &str) -> String;           // what Request::domain uses
pub fn urlencode_form(pairs: &[(String, String)]) -> String;  // quote_plus form encoding
```

Do **not** substitute the `url` crate here: it applies WHATWG normalisation (path resolution,
default-port stripping) that would change which pages dedupe together.

## `scheduler` — the priority queue

```rust
pub struct QueueKey { pub neg_priority: i64, pub counter: u64 }   // i64: -(i32::MIN) must fit
pub struct Ticket(/* private */);   pub fn counter(self) -> u64;

pub struct Scheduler;
impl Scheduler {
    pub fn new(options: FingerprintOptions) -> Self;
    pub fn options(&self) -> FingerprintOptions;
    pub fn enqueue(&mut self, request: Request) -> bool;      // false = dropped as duplicate
    pub fn dequeue(&mut self) -> Option<(Ticket, Request)>;   // None = queue empty
    pub fn complete(&mut self, ticket: Ticket);               // idempotent
    pub fn len(&self) -> usize;                               // QUEUE only, not in-flight
    pub fn is_empty(&self) -> bool;
    pub fn inflight_len(&self) -> usize;
    pub fn pending_len(&self) -> usize;                       // queued + in-flight
    pub fn seen_len(&self) -> usize;
    pub fn has_seen(&self, fingerprint: &Fingerprint) -> bool;
    pub fn snapshot(&self) -> (Vec<Request>, HashSet<Fingerprint>);
    pub fn restore(&mut self, data: &CheckpointData);
}
```

`dequeue` returns `None` instead of awaiting an empty queue; the completion condition ("queue
empty **and** zero active tasks") belongs to the engine, which is free to replace Python's
50 ms busy-wait with a `Notify`. A request stays in `snapshot()` until its ticket is
`complete`d, which is what makes an in-flight request survive a pause.

## `session` — routing and the fetch seam

```rust
pub struct FetchedResponse {
    pub status: u16, pub reason: String, pub url: String,
    pub body: Bytes, pub encoding: String,
    pub headers: IndexMap<String, String>, pub request_headers: IndexMap<String, String>,
    pub cookies: Value, pub history: Vec<String>,
    pub meta: Item, pub request: Option<Request>,
}
impl FetchedResponse {
    pub fn new(status: u16, url: impl Into<String>, body: impl Into<Bytes>) -> Self;
    pub fn attach_request(&mut self, request: &Request);   // sets .request, merges meta
}

#[async_trait]
pub trait CrawlSession: Send + Sync + 'static {
    fn is_alive(&self) -> bool;
    async fn open(&self) -> Result<()>;
    async fn close(&self) -> Result<()>;
    async fn fetch(&self, url: &str, kwargs: &SessionKwargs) -> Result<FetchedResponse>;
}

pub struct AddOptions { pub default: bool, pub lazy: bool }   // ::DEFAULT, ::LAZY

pub struct SessionManager;
impl SessionManager {
    pub fn new() -> Self;
    pub fn add(&mut self, id: impl Into<String>, session: Arc<dyn CrawlSession>, options: AddOptions)
        -> Result<&mut Self>;                    // chainable
    pub fn remove(&mut self, id: &str) -> Result<()>;
    pub fn pop(&mut self, id: &str) -> Result<Arc<dyn CrawlSession>>;
    pub fn default_session_id(&self) -> Result<&str>;
    pub fn session_ids(&self) -> Vec<&str>;
    pub fn get(&self, id: &str) -> Result<&Arc<dyn CrawlSession>>;
    pub fn contains(&self, id: &str) -> bool;
    pub fn is_lazy(&self, id: &str) -> bool;
    pub fn len(&self) -> usize;  pub fn is_empty(&self) -> bool;  pub fn is_started(&self) -> bool;
    pub async fn start(&self) -> Result<()>;     // idempotent, skips lazy sessions
    pub async fn close(&self) -> Result<()>;
    pub async fn fetch(&self, request: &Request) -> Result<FetchedResponse>;
}
```

`start`, `close` and `fetch` take `&self`, so the engine can hold an `Arc<SessionManager>`;
only `add` / `remove` / `pop` need `&mut`, which is exactly the `configure_sessions` phase.

**`FetchedResponse` is the seam for work unit 12.** When `browser_http::response::Response`
exists, implement `CrawlSession` for the pooled HTTP session and convert on the way out
(`From<Response> for FetchedResponse`), and add the reverse conversion for the dev-mode cache.
Nothing in this crate needs to change.

## `stats` — live counters

```rust
pub struct CrawlStats;      // all methods take &self; share as Arc<CrawlStats>
impl CrawlStats {
    pub fn new() -> Self;
    // counters
    pub fn requests_count / response_bytes / failed_requests_count / offsite_requests_count
         / robots_disallowed_count / cache_hits / cache_misses / items_scraped / items_dropped
         / blocked_requests_count (&self) -> u64;
    pub fn add_failed_requests / add_offsite_requests / add_robots_disallowed / add_cache_hits
         / add_cache_misses / add_items_scraped / add_items_dropped / add_blocked_requests (&self, n: u64);
    pub fn increment_requests_count(&self, sid: &str);
    pub fn increment_response_bytes(&self, domain: &str, count: u64);
    pub fn increment_status(&self, status: u16);
    // configuration echoed into the dict
    pub fn set_concurrent_requests / set_concurrent_requests_per_domain (&self, u64);
    pub fn set_start_time / set_end_time / set_download_delay (&self, f64);
    pub fn set_autothrottle_enabled(&self, bool);
    pub fn set_autothrottle_delays(&self, impl IntoIterator<Item = (String, f64)>);
    pub fn set_log_counts(&self, LogCounts);      // browser_core::log::LogCounts
    // reads
    pub fn elapsed_seconds(&self) -> f64;  pub fn requests_per_second(&self) -> f64;
    pub fn session_requests(&self, sid: &str) -> u64;
    pub fn status_count(&self, status: u16) -> u64;
    pub fn domain_bytes(&self, domain: &str) -> u64;
    pub fn push_proxy(&self, Value);  pub fn push_proxy_config(&self, &Proxy);  pub fn proxies(&self) -> Vec<Value>;
    pub fn set_custom / custom / increment_custom
    pub fn to_dict(&self) -> serde_json::Map<String, Value>;   // Python's exact key order
    pub fn to_pretty_json(&self) -> String;                    // json.dumps(..., indent=4)
}

pub struct CrawlResult { pub stats: Arc<CrawlStats>, pub items: ItemList, pub paused: bool }
impl CrawlResult { pub fn completed(&self) -> bool; pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool; }
```

`to_dict`'s last key is `log_count`, renamed from Python's `log_levels_counter`; it is `{}`
until `set_log_counts` runs.

## `items` — the exporters

```rust
pub struct ItemList(/* Vec<Item>, Deref/DerefMut to Vec<Item> */);
impl ItemList {
    pub fn new() -> Self;  pub fn push(&mut self, Item);  pub fn clear(&mut self);
    pub fn to_json(&self, path, indent: bool) -> Result<()>;
    pub fn to_jsonl(&self, path) -> Result<()>;
    pub fn to_csv(&self, path, &CsvOptions) -> Result<()>;
    pub fn to_xml(&self, path, &XmlOptions) -> Result<()>;
    pub fn render_xml(&self, &XmlOptions) -> String;
}
pub struct CsvOptions { pub fields: Option<Vec<String>>, pub delimiter: u8 }     // default: None, b','
pub struct XmlOptions { pub root_tag: String, pub item_tag: String, pub indent: bool }  // "items"/"item"/true

pub fn stringify(&Value) -> String;   // None => "", containers => compact JSON, else str()
pub fn xml_tag(&str) -> String;       // [^\w.-] => "_", prefix "_" unless it starts with a letter/_
pub fn scrub_xml(&str) -> String;     // delete characters XML 1.0 cannot represent
```

All four exporters are byte-identical to Python's, including `\r\n` CSV terminators and
`ElementTree`'s `<?xml version='1.0' encoding='utf-8'?>` / `<tag />` / two-space indentation.

## `checkpoint` — pause and resume

```rust
pub const CHECKPOINT_FILE: &str = "checkpoint.json";
pub const CHECKPOINT_TEMP_FILE: &str = "checkpoint.tmp";
pub const CHECKPOINT_VERSION: u32 = 1;
pub const DEFAULT_INTERVAL_SECS: f64 = 300.0;

pub struct CheckpointData { pub requests: Vec<Request>, pub seen: HashSet<Fingerprint> }
impl From<(Vec<Request>, HashSet<Fingerprint>)> for CheckpointData {}  // straight from snapshot()

pub struct CheckpointManager;
impl CheckpointManager {
    pub fn new(crawldir: impl Into<PathBuf>, interval: f64) -> Result<Self>;  // validates interval
    pub fn crawldir(&self) -> &Path;  pub fn path(&self) -> &Path;
    pub fn interval(&self) -> f64;    pub fn interval_duration(&self) -> Duration;
    pub async fn has_checkpoint(&self) -> bool;
    pub async fn save(&self, &CheckpointData) -> Result<()>;   // RE-RAISES on failure
    pub async fn load(&self) -> Option<CheckpointData>;        // swallows: corrupt => start fresh
    pub async fn cleanup(&self);                               // swallows
}
```

The `save` / `cleanup` asymmetry is load-bearing: on a force stop the engine saves **before**
cancelling, and a failed save must not be mistaken for a successful pause — otherwise the
`finally` block deletes the user's previous checkpoint.

Format is versioned JSON, not pickle (`docs/DIVERGENCES.md`). Python-written `checkpoint.pkl`
files are not resumable here.

## `error`

One enum, `SpiderError`, with the Python messages reproduced verbatim where a test pins them:
`SessionAlreadyRegistered`, `SessionNotFound`, `UnknownSession`, `NoSessionsRegistered`,
`Fetch`, `SessionLifecycle`, `CheckpointIntervalNotANumber`, `CheckpointIntervalNegative`,
`CheckpointSave`, `Serialize`, `Export`. `pub type Result<T, E = SpiderError>`.

Units 16 and 17 should add their variants here rather than inventing a second enum — except
where both units would need to edit it in the same wave, in which case keep a module-local
type and let unit 20 unify.

---

## Where the engine (unit 17) plugs in

`tests/data_plane_composition.rs` is a working miniature of `CrawlerEngine`: seeds → dequeue →
`SessionManager::fetch` → stats → callback → enqueue → checkpoint → resume → export. Start
from it. What is *not* here and belongs to unit 17: the `Spider` trait, `SpiderSettings`,
`BLOCKED_CODES`, the concurrency limiters, the `_process_request` ordering, the pause ladder
and the streaming channel.
