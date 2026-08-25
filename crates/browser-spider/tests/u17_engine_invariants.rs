//! Engine invariants that no differential corpus can express, because they are about how
//! the *port* fails rather than about what the reference implementation computes.
//!
//! Python's tests pin four of these (`test_engine.py`, `test_spider.py`,
//! `test_force_stop_checkpoint.py`): `complete()` always runs, `Request` is dispatched
//! before `Item`, a falsy item is dropped, and a pause writes the checkpoint *before* it
//! cancels. The fifth has no Python analogue at all: a Rust callback can **panic**, and a
//! panicking task must not wedge the crawl — the ticket and the concurrency slot it holds
//! have to come back, or the loop waits forever on a task that will never finish.
//!
//! These run against an in-memory [`CrawlSession`], not a socket: the point is the loop's
//! bookkeeping, and `u17_engine_golden.rs` already drives the whole thing over a real
//! server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use browser_spider::error::{Result as SpiderResult, SpiderError};
use browser_spider::request::{Item, Request, SessionKwargs};
use browser_spider::session::{AddOptions, CrawlSession, FetchedResponse, SessionManager};
use browser_spider::spider::{
    CallbackStream, CrawlError, CrawlResponse, RunOptions, Spider, SpiderRunner, SpiderSettings,
    Yielded,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// A site in a HashMap
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MemorySite {
    pages: HashMap<String, String>,
    fetched: Mutex<Vec<String>>,
    alive: AtomicBool,
}

impl MemorySite {
    fn new(pages: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            pages: pages
                .iter()
                .map(|(url, body)| ((*url).to_owned(), (*body).to_owned()))
                .collect(),
            ..Self::default()
        })
    }

    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().clone()
    }
}

#[async_trait]
impl CrawlSession for MemorySite {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    async fn open(&self) -> SpiderResult<()> {
        self.alive.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn close(&self) -> SpiderResult<()> {
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn fetch(&self, url: &str, _kwargs: &SessionKwargs) -> SpiderResult<FetchedResponse> {
        self.fetched.lock().push(url.to_owned());
        match self.pages.get(url) {
            Some(body) => {
                let mut response = FetchedResponse::new(200, url, body.clone());
                response.encoding = "utf-8".to_owned();
                Ok(response)
            }
            None => Err(SpiderError::Fetch {
                sid: "mem".to_owned(),
                url: url.to_owned(),
                message: "no such page".to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// A spider whose callback misbehaves on demand
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Misbehaviour {
    /// Panic inside the callback stream, after yielding one request.
    PanicAfterYielding,
    /// Panic inside `on_scraped_item`.
    PanicInThePipeline,
    /// Yield a `Request` and an `Item` from the same stream, request first.
    RequestThenItem,
    /// Empty the item in `on_scraped_item`, which is falsy and must be dropped.
    EmptyTheItem,
}

struct Misbehaving {
    settings: SpiderSettings,
    what: Misbehaviour,
    site: Arc<MemorySite>,
    panics: AtomicUsize,
}

#[async_trait]
impl Spider for Misbehaving {
    fn name(&self) -> &str {
        "misbehaving"
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    fn configure_sessions(&self, manager: &mut SessionManager) -> Result<(), CrawlError> {
        manager.add(
            "mem",
            Arc::clone(&self.site) as Arc<dyn CrawlSession>,
            AddOptions::DEFAULT,
        )?;
        Ok(())
    }

    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
        Box::pin(async_stream::stream! {
            let url = response.url.clone();
            match self.what {
                Misbehaviour::PanicAfterYielding => {
                    if url.ends_with("/a") {
                        yield Yielded::request(Request::new("mem://site/c").with_sid("mem"));
                        self.panics.fetch_add(1, Ordering::SeqCst);
                        panic!("the callback for {url} panicked");
                    }
                    let mut item = Item::new();
                    item.insert("url".into(), Value::String(url));
                    yield Yielded::item(item);
                }
                Misbehaviour::PanicInThePipeline => {
                    let mut item = Item::new();
                    item.insert("url".into(), Value::String(url));
                    yield Yielded::item(item);
                }
                Misbehaviour::RequestThenItem => {
                    if url.ends_with("/a") {
                        yield Yielded::request(Request::new("mem://site/b").with_sid("mem"));
                    }
                    let mut item = Item::new();
                    item.insert("url".into(), Value::String(url));
                    yield Yielded::item(item);
                }
                Misbehaviour::EmptyTheItem => {
                    let mut item = Item::new();
                    item.insert("url".into(), Value::String(url));
                    yield Yielded::item(item);
                }
            }
        })
    }

    async fn on_scraped_item(&self, item: Item) -> Option<Item> {
        match self.what {
            Misbehaviour::PanicInThePipeline
                if item.get("url").and_then(Value::as_str) == Some("mem://site/a") =>
            {
                self.panics.fetch_add(1, Ordering::SeqCst);
                panic!("the item pipeline panicked");
            }
            // `Some(empty)` is falsy in Python (`if processed:`), so it must be dropped
            // exactly as `None` is.
            Misbehaviour::EmptyTheItem => Some(Item::new()),
            _ => Some(item),
        }
    }
}

fn spider(what: Misbehaviour, seeds: &[&str]) -> Arc<Misbehaving> {
    let site = MemorySite::new(&[
        ("mem://site/a", "<h1>a</h1>"),
        ("mem://site/b", "<h1>b</h1>"),
        ("mem://site/c", "<h1>c</h1>"),
    ]);
    Arc::new(Misbehaving {
        settings: SpiderSettings::default()
            .with_start_urls(seeds.iter().map(|url| (*url).to_owned()))
            .with_concurrent_requests(2),
        what,
        site,
        panics: AtomicUsize::new(0),
    })
}

async fn run(spider: Arc<Misbehaving>) -> (Arc<Misbehaving>, Vec<String>, Vec<Item>, u64, u64) {
    let runner = SpiderRunner::new(
        Arc::clone(&spider) as Arc<dyn Spider>,
        RunOptions::default().with_ctrl_c(false),
    )
    .expect("the runner builds");
    let result = tokio::time::timeout(Duration::from_secs(10), runner.run())
        .await
        .expect("the crawl must not hang")
        .expect("the crawl itself does not fail");
    let fetched = spider.site.fetched();
    let items = result.items.into_vec();
    let scraped = result.stats.items_scraped();
    let dropped = result.stats.items_dropped();
    (spider, fetched, items, scraped, dropped)
}

// ---------------------------------------------------------------------------

/// A panicking callback must release the ticket and the concurrency slot it holds.
///
/// Python cannot express this: an exception there is caught by `_run_callbacks`. In the
/// port the panic unwinds through the spawned future, and only `TaskGuard`'s `Drop`
/// stands between it and a loop that waits forever for a task that already died.
#[tokio::test]
async fn a_panicking_callback_still_completes_its_request() {
    let (spider, fetched, items, _scraped, _dropped) =
        run(spider(Misbehaviour::PanicAfterYielding, &["mem://site/a"])).await;

    assert_eq!(
        spider.panics.load(Ordering::SeqCst),
        1,
        "the panic really happened"
    );
    // `/c` was yielded *before* the panic, so it is in the queue; the crawl has to reach
    // it, which it can only do if the panicking task gave its slot back.
    assert_eq!(fetched, ["mem://site/a", "mem://site/c"]);
    assert_eq!(items.len(), 1, "only `/c` produced an item");
    assert_eq!(items[0]["url"], Value::String("mem://site/c".into()));
}

/// The same, one layer further in: the panic happens in `on_scraped_item`.
#[tokio::test]
async fn a_panicking_item_pipeline_still_completes_its_request() {
    let (spider, fetched, _items, _scraped, _dropped) = run(spider(
        Misbehaviour::PanicInThePipeline,
        &["mem://site/a", "mem://site/b"],
    ))
    .await;

    assert_eq!(spider.panics.load(Ordering::SeqCst), 1);
    // Both seeds were fetched: the panicking one did not take the other's slot with it.
    let mut fetched = fetched;
    fetched.sort();
    assert_eq!(fetched, ["mem://site/a", "mem://site/b"]);
}

/// `Request` is dispatched as a request and `Item` as an item, from one stream, in order.
///
/// Python's `_run_callbacks` tests `isinstance(result, Request)` **before**
/// `isinstance(result, dict)`; the port's `Yielded` makes the two unmistakable, and this
/// pins that the engine acts on both rather than on whichever came first.
#[tokio::test]
async fn a_request_and_an_item_from_one_stream_are_both_acted_on() {
    let (_spider, fetched, items, scraped, dropped) =
        run(spider(Misbehaviour::RequestThenItem, &["mem://site/a"])).await;

    assert_eq!(
        fetched,
        ["mem://site/a", "mem://site/b"],
        "the request was enqueued"
    );
    assert_eq!(items.len(), 2, "and both items were kept");
    assert_eq!(scraped, 2);
    assert_eq!(dropped, 0);
}

/// `on_scraped_item` returning an **empty** item drops it, because Python tests
/// `if processed_result:` and an empty dict is falsy.
#[tokio::test]
async fn an_emptied_item_is_dropped_like_none() {
    let (_spider, fetched, items, scraped, dropped) =
        run(spider(Misbehaviour::EmptyTheItem, &["mem://site/a"])).await;

    assert_eq!(fetched, ["mem://site/a"]);
    assert!(items.is_empty());
    assert_eq!(scraped, 0);
    assert_eq!(dropped, 1);
}

/// Two runs on one [`SpiderRunner`] are independent, and the second reuses the sessions
/// the first closed.
///
/// Python's `Spider.start()` builds a fresh `CrawlerEngine` per call while
/// `_session_manager` lives on the spider, so the scheduler's `seen` set is *not* carried
/// over but the connection pools are. The port has to match on both halves: a second run
/// that inherited `seen` would fetch nothing, and one that could not reopen its sessions
/// would fail outright.
#[tokio::test]
async fn the_same_runner_can_crawl_twice() {
    let spider = spider(Misbehaviour::RequestThenItem, &["mem://site/a"]);
    let site = Arc::clone(&spider.site);
    let runner = SpiderRunner::new(
        Arc::clone(&spider) as Arc<dyn Spider>,
        RunOptions::default().with_ctrl_c(false),
    )
    .expect("the runner builds");

    let first = runner.run().await.expect("first crawl");
    assert_eq!(first.items.as_slice().len(), 2);
    assert!(!site.is_alive(), "the session manager closed its sessions");

    let second = runner.run().await.expect("second crawl");
    assert_eq!(
        second.items.as_slice().len(),
        2,
        "a fresh engine means a fresh `seen` set"
    );
    assert_eq!(site.fetched().len(), 4, "both pages, twice");
    assert_eq!(first.stats.requests_count(), 2);
    assert_eq!(
        second.stats.requests_count(),
        2,
        "the stats object is replaced, not shared"
    );
}

/// Abandoning a streamed crawl stops it instead of detaching it.
///
/// Python nests the crawl in a task group *inside* the async generator, so dropping the
/// generator cancels the crawl; a bare `JoinHandle` would only detach it and leave a crawl
/// fetching for nobody. The observable consequence is that the site stops being fetched.
#[tokio::test]
async fn dropping_a_stream_stops_the_crawl() {
    let spider = Arc::new(Misbehaving {
        settings: SpiderSettings::default()
            .with_start_urls(["mem://site/a"])
            .with_concurrent_requests(1),
        what: Misbehaviour::RequestThenItem,
        site: MemorySite::new(&[
            ("mem://site/a", "<h1>a</h1>"),
            ("mem://site/b", "<h1>b</h1>"),
        ]),
        panics: AtomicUsize::new(0),
    });
    let site = Arc::clone(&spider.site);
    let runner = SpiderRunner::new(
        Arc::clone(&spider) as Arc<dyn Spider>,
        RunOptions::default().with_ctrl_c(false),
    )
    .expect("the runner builds");

    {
        let stream = runner.stream().expect("the stream starts");
        futures::pin_mut!(stream);
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            futures::StreamExt::next(&mut stream),
        )
        .await
        .expect("an item arrives");
        assert!(first.is_some());
        // `stream` is dropped here, which aborts the crawl task.
    }
    let after = site.fetched().len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        site.fetched().len(),
        after,
        "no fetch happened after the drop"
    );
    assert!(
        runner.engine().is_none(),
        "the runner released the engine slot"
    );
}
