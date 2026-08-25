//! The data plane composed into one miniature crawl.
//!
//! Unit 15 owns seven pieces that unit 17's `CrawlerEngine` will bolt together. This test
//! is that assembly, in the small: a scheduler feeding a session manager, statistics
//! collected on the way through, a "callback" yielding items and follow-up requests, a
//! checkpoint taken mid-flight and resumed into a fresh scheduler, and the items exported.
//!
//! It exists so the engine implementer has a working skeleton to grow, and so a change to
//! any one module that breaks the seam fails here rather than three units later.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use serde_json::{json, Value};

use browser_spider::checkpoint::{CheckpointData, CheckpointManager};
use browser_spider::error::{Result, SpiderError};
use browser_spider::items::{CsvOptions, ItemList};
use browser_spider::request::{FingerprintOptions, Item, Request, SessionKwargs};
use browser_spider::scheduler::Scheduler;
use browser_spider::session::{AddOptions, CrawlSession, FetchedResponse, SessionManager};
use browser_spider::stats::{CrawlResult, CrawlStats};

/// A session that serves a fixed site map and counts what it was asked for.
struct FakeSite {
    pages: HashMap<String, (u16, &'static str)>,
    fetched: Mutex<Vec<String>>,
    alive: Mutex<bool>,
}

impl FakeSite {
    fn new() -> Arc<Self> {
        let mut pages = HashMap::new();
        pages.insert(
            "https://example.com/".to_owned(),
            (200u16, "index: /a /b /a?utm=1"),
        );
        pages.insert("https://example.com/a".to_owned(), (200, "item: Widget 9"));
        pages.insert("https://example.com/b".to_owned(), (200, "item: Gadget 4"));
        pages.insert("https://example.com/blocked".to_owned(), (429, "go away"));
        Arc::new(Self {
            pages,
            fetched: Mutex::new(Vec::new()),
            alive: Mutex::new(false),
        })
    }
}

#[async_trait]
impl CrawlSession for FakeSite {
    fn is_alive(&self) -> bool {
        *self.alive.lock()
    }
    async fn open(&self) -> Result<()> {
        *self.alive.lock() = true;
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        *self.alive.lock() = false;
        Ok(())
    }
    async fn fetch(&self, url: &str, _kwargs: &SessionKwargs) -> Result<FetchedResponse> {
        self.fetched.lock().push(url.to_owned());
        match self.pages.get(url) {
            Some((status, body)) => Ok(FetchedResponse::new(
                *status,
                url,
                Bytes::from_static(body.as_bytes()),
            )),
            None => Err(SpiderError::Fetch {
                sid: "site".to_owned(),
                url: url.to_owned(),
                message: "404 in the fake site".to_owned(),
            }),
        }
    }
}

/// The "spider callback": turn a response into follow-up requests and items.
fn parse(response: &FetchedResponse) -> (Vec<Request>, Vec<Item>) {
    let body = String::from_utf8_lossy(&response.body).into_owned();
    let mut requests = Vec::new();
    let mut items = Vec::new();

    if let Some(links) = body.strip_prefix("index: ") {
        for link in links.split_whitespace() {
            requests.push(Request::new(format!("https://example.com{link}")).with_priority(1));
        }
    } else if let Some(rest) = body.strip_prefix("item: ") {
        let mut parts = rest.split_whitespace();
        let mut item = Item::new();
        item.insert("name".to_owned(), json!(parts.next().unwrap_or("")));
        item.insert(
            "price".to_owned(),
            json!(parts.next().and_then(|p| p.parse::<i64>().ok())),
        );
        item.insert("url".to_owned(), json!(response.url));
        items.push(item);
    }
    (requests, items)
}

const BLOCKED_CODES: [u16; 9] = [401, 403, 407, 429, 444, 500, 502, 503, 504];

#[tokio::test]
async fn the_data_plane_runs_a_small_crawl_end_to_end() {
    let site = FakeSite::new();
    let mut manager = SessionManager::new();
    manager
        .add("site", site.clone(), AddOptions::default())
        .expect("add");
    manager.start().await.expect("start");

    let stats = Arc::new(CrawlStats::new());
    stats.set_start_time(0.0);
    stats.set_concurrent_requests(4);
    let mut scheduler = Scheduler::new(FingerprintOptions::default());
    let mut items = ItemList::new();

    // Seeds, including one that will come back 429 and one that fails to fetch.
    assert!(scheduler.enqueue(Request::new("https://example.com/")));
    assert!(scheduler.enqueue(Request::new("https://example.com/blocked")));
    assert!(scheduler.enqueue(Request::new("https://example.com/missing")));

    let mut max_blocked_retries = 1u32;

    while let Some((ticket, request)) = scheduler.dequeue() {
        match manager.fetch(&request).await {
            Ok(response) => {
                stats.increment_requests_count(&request.sid);
                stats.increment_response_bytes(request.domain(), response.body.len() as u64);
                stats.increment_status(response.status);

                if BLOCKED_CODES.contains(&response.status) {
                    stats.add_blocked_requests(1);
                    if request.retry_count < max_blocked_retries {
                        // The blocked-retry path: a *copy*, so the fingerprint resets.
                        let mut retry = request.copy();
                        retry.retry_count += 1;
                        retry.priority -= 1;
                        retry.dont_filter = true;
                        retry.session_kwargs.proxy = None;
                        retry.session_kwargs.proxies = None;
                        assert!(scheduler.enqueue(retry));
                        max_blocked_retries = 0; // stop after one retry, for the test
                    }
                    scheduler.complete(ticket);
                    continue;
                }

                let (follow_ups, scraped) = parse(&response);
                for follow_up in follow_ups {
                    // Offsite filtering would go here; everything is on-domain.
                    scheduler.enqueue(follow_up);
                }
                for item in scraped {
                    stats.add_items_scraped(1);
                    items.push(item);
                }
            }
            Err(_) => {
                stats.add_failed_requests(1);
            }
        }
        scheduler.complete(ticket);
    }

    stats.set_end_time(4.0);
    manager.close().await.expect("close");

    // `/a?utm=1` canonicalises to a different URL than `/a`, so it *is* fetched; the
    // duplicate `/a` link is not.
    let fetched = site.fetched.lock().clone();
    assert_eq!(
        fetched
            .iter()
            .filter(|url| url.as_str() == "https://example.com/a")
            .count(),
        1
    );
    // The blocked page was fetched twice: once, then once more as the retry.
    assert_eq!(
        fetched
            .iter()
            .filter(|url| url.as_str() == "https://example.com/blocked")
            .count(),
        2
    );

    assert_eq!(stats.items_scraped(), 2);
    assert_eq!(items.len(), 2);
    assert_eq!(stats.failed_requests_count(), 2); // /missing and /a?utm=1
    assert_eq!(stats.blocked_requests_count(), 2);
    assert_eq!(stats.status_count(200), 3);
    assert_eq!(stats.status_count(429), 2);
    assert!(stats.domain_bytes("example.com") > 0);
    assert_eq!(stats.session_requests(""), 5);
    assert_eq!(stats.requests_per_second(), 1.25);

    // Everything scheduled has been completed, so a checkpoint now would be empty.
    assert_eq!(scheduler.pending_len(), 0);
    assert!(scheduler.is_empty());

    // The stats dict is complete and log_count is present even though no logger ran.
    let dict = stats.to_dict();
    assert_eq!(dict["items_scraped"], Value::from(2));
    assert_eq!(dict["log_count"], json!({}));

    // And the items export.
    let dir = tempfile::tempdir().expect("tempdir");
    let csv_path = dir.path().join("out/items.csv");
    items
        .to_csv(&csv_path, &CsvOptions::default())
        .expect("csv");
    let csv = std::fs::read_to_string(&csv_path).expect("read");
    assert!(csv.starts_with("name,price,url\r\n"), "{csv}");
    assert_eq!(csv.lines().count(), 3);

    let result = CrawlResult {
        stats: Arc::clone(&stats),
        items,
        paused: false,
    };
    assert!(result.completed());
    assert_eq!(result.len(), 2);
    assert_eq!(result.into_iter().count(), 2);
}

#[tokio::test]
async fn a_checkpoint_taken_mid_flight_resumes_the_rest_of_the_crawl() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manager_dir = dir.path().join("crawl");
    let checkpoints = CheckpointManager::new(&manager_dir, 300.0).expect("manager");

    let site = FakeSite::new();
    let mut sessions = SessionManager::new();
    sessions
        .add("site", site.clone(), AddOptions::default())
        .expect("add");
    sessions.start().await.expect("start");

    let options = FingerprintOptions::default();
    let mut scheduler = Scheduler::new(options);
    scheduler.enqueue(Request::new("https://example.com/"));
    scheduler.enqueue(Request::new("https://example.com/a"));
    scheduler.enqueue(Request::new("https://example.com/b"));

    // Take one request out and leave it in flight — the pause case the checkpoint has to
    // survive: it is not in the queue any more, but it must be re-fetched after a resume.
    let (in_flight_ticket, in_flight) = scheduler.dequeue().expect("queued");
    assert_eq!(in_flight.url, "https://example.com/");
    let _ = in_flight_ticket;

    let (requests, seen) = scheduler.snapshot();
    assert_eq!(requests.len(), 3, "the in-flight request is still pending");
    checkpoints
        .save(&CheckpointData { requests, seen })
        .await
        .expect("save");
    assert!(checkpoints.has_checkpoint().await);

    // --- a new process picks the crawl up ---
    let mut resumed = Scheduler::new(options);
    let data = checkpoints.load().await.expect("load");
    resumed.restore(&data);
    assert_eq!(resumed.len(), 3);

    // Already-seen URLs stay filtered across the restart.
    assert!(!resumed.enqueue(Request::new("https://example.com/a")));

    let mut fetched = Vec::new();
    while let Some((ticket, request)) = resumed.dequeue() {
        if let Ok(response) = sessions.fetch(&request).await {
            fetched.push(response.url.clone());
        }
        resumed.complete(ticket);
    }
    fetched.sort();
    assert_eq!(
        fetched,
        [
            "https://example.com/",
            "https://example.com/a",
            "https://example.com/b"
        ]
    );

    // A clean finish deletes the checkpoint; cleanup never fails.
    checkpoints.cleanup().await;
    assert!(!checkpoints.has_checkpoint().await);
    sessions.close().await.expect("close");
}

#[tokio::test]
async fn a_request_survives_a_checkpoint_with_its_callback_body_and_meta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let checkpoints = CheckpointManager::new(dir.path(), 0.0).expect("manager");

    let mut request = Request::new("https://example.com/search?q=a+b&page=2")
        .with_sid("browser")
        .with_priority(7)
        .with_retry_count(2)
        .with_callback("parse_results");
    request.meta.insert("depth".to_owned(), json!(3));
    request.session_kwargs.method = Some(browser_core::types::HttpMethod::Post);
    request.session_kwargs.data = Some(browser_spider::request::RequestBody::Form(vec![(
        "q".to_owned(),
        "a b".to_owned(),
    )]));
    let fingerprint = request.update_fingerprint(FingerprintOptions::default());

    let mut scheduler = Scheduler::new(FingerprintOptions::default());
    scheduler.enqueue(request);
    checkpoints
        .save(&CheckpointData::from(scheduler.snapshot()))
        .await
        .expect("save");

    let data = checkpoints.load().await.expect("load");
    let mut restored = data.requests[0].clone();
    // The engine re-binds callbacks by name; an unknown one falls back to `parse`.
    restored.restore_callback(|id| id.as_str() == "parse_results");

    assert_eq!(restored.sid, "browser");
    assert_eq!(restored.priority, 7);
    assert_eq!(restored.retry_count, 2);
    assert_eq!(restored.meta["depth"], json!(3));
    assert_eq!(
        restored.session_kwargs.method(),
        browser_core::types::HttpMethod::Post
    );
    assert_eq!(
        restored.callback.as_ref().map(|c| c.as_str()),
        Some("parse_results")
    );
    assert_eq!(restored.fingerprint(), Some(fingerprint));
    assert!(data.seen.contains(&fingerprint));
}
