//! A crawl, start to finish — and it runs offline.
//!
//! Unlike the other three examples this one needs **no network and no
//! browser**. It starts a two-hundred-line HTTP server on `127.0.0.1` on an
//! ephemeral port, crawls it, prints the items and exits. Crawling somebody
//! else's site to demonstrate a crawler is bad manners and makes the output
//! depend on their deploy schedule; a fixture makes the example both polite and
//! reproducible.
//!
//! ```text
//! cargo run -p browser-rs --example spider --features spider
//! ```
//!
//! # What it shows
//!
//! * [`CrawlSpider`] — the ready-made spider. A `LinkExtractor` decides what to
//!   follow, a [`CrawlRule`] says which callback handles it, and the engine
//!   does the rest.
//! * Callbacks addressed **by name** ([`CallbackId`]) rather than by closure
//!   identity, which is what lets an interrupted crawl resume from a checkpoint
//!   and still know which handler a queued request was for.
//! * `robots.txt` being obeyed, and a `deny` pattern being obeyed — both are
//!   demonstrated by `/admin/secret` never being requested, and the fixture
//!   counts requests so the example can *assert* that rather than claim it.
//! * The end of a crawl: [`CrawlStats`], and [`ItemList::to_jsonl`].
//!
//! A hand-written [`Spider`] — the `#[async_trait]` form, for anything the
//! rules cannot express — is in `crates/browser-rs/tests/integration.rs`, in
//! `unit_20_facade_seam::a_user_written_spider_needs_nothing_but_the_facade`.

use std::sync::Arc;

use browser::prelude::*;
use browser::{CallbackId, CrawlResult, RunOptions, SpiderRunner, Yielded};

use fixture::Site;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Nothing leaves the loopback interface.
    let site = Site::start();
    println!("fixture serving {}", site.url("/"));

    // The callback name. A `&'static str` promoted to an id, so the request
    // queue can carry it and a checkpoint can serialise it.
    const PRODUCT: CallbackId = CallbackId::from_static("parse_product");

    // What to follow: product pages only, and never the admin area. `deny`
    // wins over `allow`.
    let extractor = LinkExtractor::builder()
        .allow([r"/p/"])
        .deny([r"/admin/"])
        .build()?;

    let spider = Arc::new(
        CrawlSpider::builder("catalogue")
            .settings(
                SpiderSettings::default()
                    .with_start_urls([site.url("/")])
                    // Without this the crawler will not leave the start URL's
                    // host — the single most common reason a first crawl
                    // "finds nothing".
                    .with_allowed_domains([site.netloc()])
                    .with_robots_txt_obey(true)
                    .with_concurrent_requests(2)
                    .with_download_delay(0.0),
            )
            .rule(CrawlRule::new(extractor).callback(PRODUCT))
            // The handler is synchronous and returns what it yields. It gets a
            // `CrawlResponse`, which derefs to the same `Response` the fetchers
            // produce — so the whole document API is available here.
            .callback(PRODUCT, |response| {
                let mut item = Item::new();
                item.insert("url".to_owned(), response.url.clone().into());
                item.insert(
                    "title".to_owned(),
                    text_of(response, "h1::text").unwrap_or_default().into(),
                );
                item.insert(
                    "price".to_owned(),
                    text_of(response, ".price::text").unwrap_or_default().into(),
                );
                item.insert(
                    "in_stock".to_owned(),
                    response
                        .css(".stock")
                        .map(|nodes| !nodes.is_empty())
                        .unwrap_or(false)
                        .into(),
                );
                vec![Yielded::item(item)]
            })
            .build(),
    );

    println!("crawling…");
    let result: CrawlResult = SpiderRunner::new(spider, RunOptions::default().with_ctrl_c(false))?
        .run()
        .await?;

    // -- what came out -----------------------------------------------------

    println!(
        "\nfinished: {}",
        if result.completed() {
            "yes"
        } else {
            "interrupted"
        }
    );
    println!("items scraped: {}", result.stats.items_scraped());
    println!("requests made: {}", site.hits().len());

    println!("\nitems:");
    for item in result.items.as_slice() {
        println!(
            "  {:<28} {:<10} {}",
            item["title"].as_str().unwrap_or("?"),
            item["price"].as_str().unwrap_or("?"),
            item["url"].as_str().unwrap_or("?"),
        );
    }

    // The two exclusions, checked rather than asserted in prose. `/admin/` is
    // blocked twice over — by `robots.txt` and by the extractor's `deny` — and
    // a crawl that requested it even once would be a bug worth failing on.
    assert_eq!(
        site.hit_count("/admin/secret"),
        0,
        "the admin page must never be requested"
    );
    println!(
        "\n/admin/secret requested {} time(s) — robots.txt and the `deny` rule both held",
        site.hit_count("/admin/secret")
    );

    // Where a crawl ends: one JSON object per line, ready for anything that
    // reads JSONL.
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("items.jsonl");
    result.items.to_jsonl(&path)?;
    println!("\nexported {} to {}:", result.items.len(), path.display());
    for line in std::fs::read_to_string(&path)?.lines() {
        println!("  {line}");
    }

    Ok(())
}

/// First match of `selector` as text, or `None` when nothing matched.
///
/// A selector that does not *parse* is a programming error and would be worth
/// surfacing; one that simply does not match is an ordinary missing field.
fn text_of(response: &browser::CrawlResponse, selector: &str) -> Option<String> {
    response
        .css(selector)
        .expect("the selectors in this example are literals and always parse")
        .get()
}

/// A loopback site to crawl: an index, six products, and an admin page that
/// `robots.txt` forbids.
///
/// This is the same shape as `local_site::Site` in
/// `crates/browser-rs/tests/integration.rs`, trimmed to what an example needs.
/// It is deliberately hand-rolled `std::net` — a crawler example that made you
/// install a server first would not be much of an example.
mod fixture {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// A running loopback server, stopped when it is dropped.
    pub struct Site {
        port: u16,
        hits: Arc<Mutex<Vec<String>>>,
        running: Arc<AtomicBool>,
    }

    impl Site {
        /// Bind an ephemeral port and start serving.
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is always bindable");
            let port = listener
                .local_addr()
                .expect("a bound socket has an address")
                .port();
            let hits = Arc::new(Mutex::new(Vec::new()));
            let running = Arc::new(AtomicBool::new(true));

            let thread_hits = Arc::clone(&hits);
            let thread_running = Arc::clone(&running);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if !thread_running.load(Ordering::Acquire) {
                        break;
                    }
                    match stream {
                        // One thread per connection: the crawler opens several
                        // at once and keeps them alive.
                        Ok(stream) => {
                            let hits = Arc::clone(&thread_hits);
                            std::thread::spawn(move || serve(stream, &hits));
                        }
                        Err(_) => break,
                    }
                }
            });

            Self {
                port,
                hits,
                running,
            }
        }

        /// An absolute URL for `path`.
        pub fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        /// `host:port`, which is what `allowed_domains` is matched against.
        pub fn netloc(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        /// Every path the socket saw, in arrival order.
        pub fn hits(&self) -> Vec<String> {
            self.hits.lock().expect("not poisoned").clone()
        }

        /// How many times `path` was requested.
        pub fn hit_count(&self, path: &str) -> usize {
            self.hits().iter().filter(|hit| *hit == path).count()
        }
    }

    impl Drop for Site {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Release);
            // Unblock the accept loop so the thread can notice and exit.
            let _ = TcpStream::connect(("127.0.0.1", self.port));
        }
    }

    /// The route table.
    fn page(path: &str) -> Option<(&'static str, String)> {
        const PRODUCTS: [(&str, &str, bool); 6] = [
            ("widget", "$9.99", true),
            ("gadget", "$19.99", true),
            ("doohickey", "$4.50", false),
            ("thingamajig", "$34.00", true),
            ("whatsit", "$2.25", true),
            ("gizmo", "$12.75", false),
        ];

        match path {
            "/robots.txt" => Some((
                "text/plain; charset=utf-8",
                "User-agent: *\nDisallow: /admin/\n".to_owned(),
            )),
            "/" => {
                let links: String = PRODUCTS
                    .iter()
                    .map(|(slug, _, _)| format!("<li><a href=\"/p/{slug}\">{slug}</a></li>"))
                    .collect();
                Some((
                    "text/html; charset=utf-8",
                    format!(
                        "<!doctype html><html><head><title>Catalogue</title></head><body>\
                         <h1>Catalogue</h1><ul>{links}</ul>\
                         <a href=\"/admin/secret\">admin</a>\
                         </body></html>"
                    ),
                ))
            }
            "/admin/secret" => Some(("text/html; charset=utf-8", "<h1>secrets</h1>".to_owned())),
            path => {
                let slug = path.strip_prefix("/p/")?;
                let (_, price, in_stock) = PRODUCTS
                    .iter()
                    .find(|(name, _, _)| *name == slug)
                    .copied()?;
                let stock = if in_stock {
                    "<span class=\"stock\">in stock</span>"
                } else {
                    ""
                };
                Some((
                    "text/html; charset=utf-8",
                    format!(
                        "<!doctype html><html><head><title>{slug}</title></head><body>\
                         <nav><a href=\"/\">home</a></nav>\
                         <article><h1>{slug}</h1><span class=\"price\">{price}</span>{stock}\
                         </article></body></html>"
                    ),
                ))
            }
        }
    }

    fn serve(mut stream: TcpStream, hits: &Arc<Mutex<Vec<String>>>) {
        let mut reader = BufReader::new(stream.try_clone().expect("a connected socket clones"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
            return;
        }
        // Drain the headers; the body is never read because nothing POSTs here.
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line == "\r\n" || line == "\n" => break,
                Ok(_) => {}
                Err(_) => return,
            }
        }

        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_owned();
        hits.lock().expect("not poisoned").push(path.clone());

        let key = path.split('?').next().unwrap_or("/");
        let (status, content_type, body) = match page(key) {
            Some((content_type, body)) => (200, content_type, body),
            None => (404, "text/plain; charset=utf-8", "not found".to_owned()),
        };
        let reason = if status == 200 { "OK" } else { "Not Found" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
        // Half-close and drain, so the peer is never RST before it has read.
        let _ = stream.shutdown(Shutdown::Write);
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
    }
}
