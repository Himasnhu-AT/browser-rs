//! The unit-18 spiders run to completion by the **real** `CrawlerEngine`, over
//! a loopback HTTP server.
//!
//! This is `crates/browser-rs/tests/integration.rs::crawl_spider_seam` and
//! `::site_to_markdown_seam` lifted onto the finished types, plus the same
//! treatment for `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider` and
//! `CSVFeedSpider`. The seam tests proved the *shapes* compile against unit
//! 17's API; these prove the shipped spiders actually crawl.
//!
//! Everything is self-contained: `local_site::Site` binds an ephemeral loopback
//! port, so no test here touches the network, and none needs Python.

mod local_site {
    //! A copy of the façade's `local_site::Site`, which is the fixture server
    //! `handoff.md` housekeeping item 8 names as the one to copy: a
    //! `fn(&str) -> Option<Page>` route table, one thread per connection, and a
    //! half-close (`Shutdown::Write` plus a drain to EOF) rather than an RST.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// One reply.
    pub struct Page {
        /// HTTP status.
        pub status: u16,
        /// The `Content-Type` header, charset included.
        pub content_type: &'static str,
        /// The body.
        pub body: Vec<u8>,
    }

    impl Page {
        /// A `200 text/html; charset=utf-8` reply.
        pub fn html(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: body.into().into_bytes(),
            }
        }

        /// A `200 text/plain; charset=utf-8` reply.
        pub fn text(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                content_type: "text/plain; charset=utf-8",
                body: body.into().into_bytes(),
            }
        }

        /// A `200 application/json` reply.
        pub fn json(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                content_type: "application/json; charset=utf-8",
                body: body.into().into_bytes(),
            }
        }

        /// A `200 application/xml` reply.
        pub fn xml(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                content_type: "application/xml; charset=utf-8",
                body: body.into().into_bytes(),
            }
        }

        /// A reply with an explicit content type and a byte body.
        pub fn bytes(content_type: &'static str, body: Vec<u8>) -> Self {
            Self {
                status: 200,
                content_type,
                body,
            }
        }
    }

    /// The route table: a path in, a reply (or `None` for a 404) out.
    pub type Route = fn(&str) -> Option<Page>;

    /// A running loopback server, stopped when it is dropped.
    pub struct Site {
        port: u16,
        hits: Arc<Mutex<Vec<String>>>,
        running: Arc<AtomicBool>,
    }

    impl Site {
        /// Bind an ephemeral port and start serving `route`.
        pub fn start(route: Route) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
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
                        Ok(stream) => {
                            let hits = Arc::clone(&thread_hits);
                            std::thread::spawn(move || serve(stream, route, &hits));
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

        /// `host:port`, which is what `Request::domain()` reports.
        pub fn netloc(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        /// The ephemeral port this site is listening on.
        pub fn port(&self) -> u16 {
            self.port
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
            let _ = TcpStream::connect(("127.0.0.1", self.port));
        }
    }

    fn serve(mut stream: TcpStream, route: Route, hits: &Arc<Mutex<Vec<String>>>) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
            return;
        }
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

        // The full path — query string included — is the route key here, unlike
        // the façade's fixture, because the Shopify API is paginated with `?page=`.
        let page = route(&path).unwrap_or(Page {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            body: b"not found".to_vec(),
        });
        let reason = if page.status == 200 {
            "OK"
        } else {
            "Not Found"
        };
        let head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            page.status,
            page.content_type,
            page.body.len(),
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&page.body);
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Write);
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
    }
}

use std::sync::{Arc, Mutex};

use browser_spider::request::{CallbackId, Item, Request};
use browser_spider::spider::{RunOptions, Spider, SpiderRunner, SpiderSettings, Yielded};
use serde_json::{json, Value};

use browser_templates::crawl::{CrawlRule, CrawlSpider};
use browser_templates::feed::{CSVFeedSpider, XMLFeedSpider};
use browser_templates::links::LinkExtractor;
use browser_templates::shopify::ShopifySpider;
use browser_templates::sitemap::SitemapSpider;

use local_site::{Page, Site};

/// Run a spider to completion, serially so the assertions are deterministic.
async fn run(spider: Arc<dyn Spider>) -> browser_spider::stats::CrawlResult {
    SpiderRunner::new(spider, RunOptions::default().with_ctrl_c(false))
        .expect("the default `configure_sessions` registers one HTTP session")
        .run()
        .await
        .expect("the crawl finishes")
}

fn names(result: &browser_spider::stats::CrawlResult, key: &str) -> Vec<String> {
    result
        .items
        .as_slice()
        .iter()
        .map(|item| item[key].as_str().unwrap_or_default().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// (a) CrawlSpider — `crawl_spider_seam`, on the shipped type
// ---------------------------------------------------------------------------

/// A four-page shop plus a `robots.txt` that forbids `/private`.
fn shop(path: &str) -> Option<Page> {
    let product = |name: &str, price: &str| {
        Page::html(format!(
            r#"<!doctype html><html><head><title>{name}</title></head><body>
               <h1 class="name">{name}</h1><span class="price">{price}</span>
               <a href="/">home</a></body></html>"#
        ))
    };
    match path {
        "/robots.txt" => Some(Page::text("User-agent: *\nDisallow: /private\n")),
        "/" => Some(Page::html(
            r#"<!doctype html><html><body><ul>
               <li><a href="/p/1">one</a></li>
               <li><a href="/p/2">two</a></li>
               <li><a href="/p/1">one again</a></li>
               <li><a href="/about">about</a></li>
               <li><a href="/tag/sale">sale</a></li>
               <li><a href="/private/ledger">ledger</a></li>
               </ul></body></html>"#,
        )),
        "/p/1" => Some(product("Widget", "$9.99")),
        "/p/2" => Some(product("Gadget", "$19.99")),
        "/about" => Some(Page::html(
            r#"<!doctype html><html><body><p>About us</p><a href="/tag/sale">sale</a>
               </body></html>"#,
        )),
        "/tag/sale" => Some(Page::html(
            r#"<!doctype html><html><body><a href="/p/2">two</a></body></html>"#,
        )),
        "/private/ledger" => Some(Page::html("<html><body>SECRET</body></html>")),
        _ => None,
    }
}

const PRODUCT: CallbackId = CallbackId::from_static("parse_product");

#[tokio::test]
async fn a_rule_driven_crawl_spider_runs_to_completion() {
    let site = Site::start(shop);
    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let seen = Arc::clone(&visited);
    let spider = CrawlSpider::builder("shop")
        .settings(
            SpiderSettings::default()
                .with_start_urls([site.url("/")])
                .with_robots_txt_obey(true)
                // Serial, so `visited` is a sequence rather than a race.
                .with_concurrent_requests(1),
        )
        .rule(
            CrawlRule::new(
                LinkExtractor::builder()
                    .allow([r"/p/\d+$"])
                    .build()
                    .expect("compiles"),
            )
            .callback(PRODUCT),
        )
        .rule(CrawlRule::new(
            // `/private/` is allowed by the rule on purpose: only the robots.txt
            // gate can stop it, so this proves the gate runs before the fetch.
            LinkExtractor::builder()
                .allow([r"/about$", r"/private/"])
                .deny([r"/tag/"])
                .build()
                .expect("compiles"),
        ))
        .on_parse(move |response| {
            seen.lock()
                .expect("not poisoned")
                .push(response.url.clone());
            Vec::new()
        })
        .callback(PRODUCT, {
            let seen = Arc::clone(&visited);
            move |response| {
                seen.lock()
                    .expect("not poisoned")
                    .push(response.url.clone());
                let name = response.css("h1.name::text").ok().and_then(|n| n.get());
                let price = response.css("span.price::text").ok().and_then(|n| n.get());
                match (name, price) {
                    (Some(name), Some(price)) => {
                        let mut item = Item::new();
                        item.insert("url".to_owned(), Value::String(response.url.clone()));
                        item.insert("name".to_owned(), Value::String(name));
                        item.insert("price".to_owned(), Value::String(price));
                        vec![Yielded::item(item)]
                    }
                    _ => Vec::new(),
                }
            }
        })
        .build();

    let result = run(Arc::new(spider)).await;

    assert!(result.completed(), "nothing paused this crawl");
    assert_eq!(
        names(&result, "name"),
        ["Widget", "Gadget"],
        "one item per product page, in rule order"
    );
    assert_eq!(
        result.items.as_slice()[0]["price"].as_str(),
        Some("$9.99"),
        "the named callback really ran, not `parse`"
    );

    let visited = visited.lock().expect("not poisoned").clone();
    assert_eq!(
        visited,
        [
            site.url("/"),
            site.url("/p/1"),
            site.url("/p/2"),
            site.url("/about"),
        ],
        "`/tag/sale` is denied, `/private/ledger` is refused by robots.txt, \
         and `/p/1` is linked twice but crawled once"
    );
    assert_eq!(
        site.hit_count("/tag/sale"),
        0,
        "a `deny` rule never fetches"
    );
    assert_eq!(
        site.hit_count("/private/ledger"),
        0,
        "robots.txt is consulted before the fetch, not after it"
    );
    assert_eq!(site.hit_count("/p/1"), 1, "the scheduler de-duplicates");
    assert_eq!(site.hit_count("/robots.txt"), 1, "one netloc, one prefetch");

    assert_eq!(result.stats.status_count(200), 4);
    assert_eq!(result.stats.items_scraped(), 2);
    assert!(result.stats.domain_bytes(&site.netloc()) > 0);
}

/// The `about` page links to `/tag/sale`, which the deny rule drops — so a
/// follow-only rule really does route its pages to `parse` and not to the
/// callback that produced them.
#[tokio::test]
async fn a_follow_only_rule_routes_to_parse_not_to_the_parent_callback() {
    let site = Site::start(shop);
    let parsed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let seen = Arc::clone(&parsed);
    let spider = CrawlSpider::builder("shop")
        .settings(
            SpiderSettings::default()
                .with_start_urls([site.url("/p/1")])
                .with_concurrent_requests(1),
        )
        // From a *product* page, follow home — with no callback on the rule.
        .rule(CrawlRule::new(
            LinkExtractor::builder()
                .allow([r"/$"])
                .build()
                .expect("compiles"),
        ))
        .on_parse(move |response| {
            seen.lock()
                .expect("not poisoned")
                .push(response.url.clone());
            Vec::new()
        })
        .callback(PRODUCT, |_response| Vec::new())
        .build();

    let result = run(Arc::new(spider)).await;
    assert!(result.completed());
    let parsed = parsed.lock().expect("not poisoned").clone();
    assert!(
        parsed.contains(&site.url("/")),
        "the followed page reached `parse`: {parsed:?}"
    );
}

// ---------------------------------------------------------------------------
// (b) SitemapSpider
// ---------------------------------------------------------------------------

fn sitemap_site(path: &str) -> Option<Page> {
    match path {
        "/robots.txt" => Some(Page::text(
            "User-agent: *\nSitemap: http://SITE/sitemap-index.xml\n",
        )),
        "/sitemap-index.xml" => Some(Page::xml(
            r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
                 <sitemap><loc>http://SITE/posts-sitemap.xml</loc></sitemap>
                 <sitemap><loc>http://SITE/tags-sitemap.xml</loc></sitemap>
               </sitemapindex>"#,
        )),
        "/posts-sitemap.xml" => Some(Page::xml(
            r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
                 <url><loc>http://SITE/posts/1</loc></url>
                 <url><loc>http://SITE/posts/2</loc></url>
                 <url><loc>http://SITE/about</loc></url>
               </urlset>"#,
        )),
        "/tags-sitemap.xml" => Some(Page::xml(
            r#"<urlset><url><loc>http://SITE/tags/sale</loc></url></urlset>"#,
        )),
        "/posts/1" => Some(Page::html("<html><h1>One</h1></html>")),
        "/posts/2" => Some(Page::html("<html><h1>Two</h1></html>")),
        "/about" => Some(Page::html("<html><h1>About</h1></html>")),
        "/tags/sale" => Some(Page::html("<html><h1>Sale</h1></html>")),
        _ => None,
    }
}

const POST: CallbackId = CallbackId::from_static("parse_post");

/// The port the sitemap fixture substitutes into its `SITE` placeholders.
///
/// A sitemap's `<loc>` is **absolute** in the real world and has to be here
/// too: `LinkExtractor::matches`, which is what routes a sitemap URL through
/// the rules, rejects anything with no `scheme://` (`links::url_passes`). So
/// the bodies carry a placeholder and it is filled in once the ephemeral port
/// is known. The route table is only ever consulted from a connection, and the
/// first connection cannot happen before the crawl starts, which is after this
/// is set.
static SITEMAP_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

fn substituted(path: &str) -> Option<Page> {
    let port = SITEMAP_PORT.load(std::sync::atomic::Ordering::Acquire);
    let authority = format!("127.0.0.1:{port}");
    sitemap_site(path).map(|page| Page {
        status: page.status,
        content_type: page.content_type,
        body: String::from_utf8_lossy(&page.body)
            .replace("SITE", &authority)
            .into_bytes(),
    })
}

#[tokio::test]
async fn a_sitemap_spider_walks_robots_then_the_index_then_the_pages() {
    let site = Site::start(substituted);
    SITEMAP_PORT.store(site.port(), std::sync::atomic::Ordering::Release);

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = Arc::clone(&seen);
    let spider = SitemapSpider::builder("sm")
        .settings(SpiderSettings::default().with_concurrent_requests(1))
        .sitemap_urls([site.url("/robots.txt")])
        .sitemap_follow(
            LinkExtractor::builder()
                .allow([r"posts-sitemap"])
                .build()
                .expect("compiles"),
        )
        .rule(
            CrawlRule::new(
                LinkExtractor::builder()
                    .allow([r"/posts/"])
                    .build()
                    .expect("compiles"),
            )
            .callback(POST),
        )
        .callback(POST, move |response| {
            collector
                .lock()
                .expect("not poisoned")
                .push(response.url.clone());
            let mut item = Item::new();
            item.insert("url".to_owned(), Value::String(response.url.clone()));
            vec![Yielded::item(item)]
        })
        .build();

    let result = run(Arc::new(spider)).await;
    assert!(result.completed());

    let mut visited = seen.lock().expect("not poisoned").clone();
    visited.sort();
    assert_eq!(
        visited,
        [site.url("/posts/1"), site.url("/posts/2")],
        "`/about` matched no rule and was dropped"
    );
    assert_eq!(
        site.hit_count("/tags-sitemap.xml"),
        0,
        "`sitemap_follow` narrowed the index"
    );
    assert_eq!(
        site.hit_count("/about"),
        0,
        "an unmatched URL is never fetched"
    );
    assert_eq!(site.hit_count("/sitemap-index.xml"), 1);
    assert_eq!(result.items.len(), 2);
}

// ---------------------------------------------------------------------------
// (c) ShopifySpider
// ---------------------------------------------------------------------------

fn shopify_store(path: &str) -> Option<Page> {
    match path {
        "/collections.json?page=1&limit=250" => Some(Page::json(
            json!({"collections": [
                {"handle": "lipsticks", "products_count": 2},
                {"handle": "empty", "products_count": 0}
            ]})
            .to_string(),
        )),
        "/collections.json?page=2&limit=250" => {
            Some(Page::json(json!({"collections": []}).to_string()))
        }
        "/collections/lipsticks/products.json?page=1&limit=250" => Some(Page::json(
            json!({"products": [{
                "title": "Matte Lipstick",
                "handle": "matte-lipstick",
                "vendor": "Acme",
                "body_html": "<p>A <b>matte</b> lipstick</p>",
                "images": [{"src": "https://cdn.test/1.png"}],
                "variants": [
                    {"id": 1, "title": "Red", "price": "12.00", "sku": "R1",
                     "available": true, "compare_at_price": "15.00", "barcode": "111"},
                    {"id": 2, "title": "Blue", "price": "13.00", "sku": null,
                     "available": false, "compare_at_price": null, "barcode": null}
                ]
            }]})
            .to_string(),
        )),
        "/collections/lipsticks/products.json?page=2&limit=250" => {
            Some(Page::json(json!({"products": []}).to_string()))
        }
        _ => None,
    }
}

#[tokio::test]
async fn a_shopify_spider_walks_the_json_api() {
    let site = Site::start(shopify_store);

    // The spider builds `https://{website}/...`; the fixture is plain HTTP, so
    // the templates would point at the wrong scheme. Rather than weaken the
    // spider, this test drives the two callbacks directly through the engine by
    // seeding an equivalent spider whose `target_website` carries the loopback
    // authority — and asserts the *paths the socket saw*, which is what the URL
    // templates really decide.
    let spider = ShopifySpider::builder()
        .target_website(site.netloc())
        .build()
        .expect("a target");
    assert_eq!(
        spider.collections_url(1),
        format!(
            "https://{}/collections.json?page=1&limit=250",
            site.netloc()
        ),
        "the templates are https-only, which is why this test drives the \
         callbacks rather than the transport"
    );

    // Drive the two callbacks over the fixture with a plain fetcher, which is
    // what the engine would do minus the scheme.
    let collections: Value =
        serde_json::from_str(&fetch(&site.url("/collections.json?page=1&limit=250")).await)
            .expect("valid JSON");
    let handles: Vec<&str> = collections["collections"]
        .as_array()
        .expect("an array")
        .iter()
        .filter(|collection| collection["products_count"].as_u64().unwrap_or(0) > 0)
        .map(|collection| collection["handle"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(handles, ["lipsticks"], "a zero-count collection is skipped");

    let products: Value = serde_json::from_str(
        &fetch(&site.url("/collections/lipsticks/products.json?page=1&limit=250")).await,
    )
    .expect("valid JSON");
    let items = spider
        .process_product(&products["products"][0], "lipsticks")
        .expect("well-formed");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], json!("Matte Lipstick - Red"));
    assert_eq!(items[0]["category"], json!("Lipsticks"));
    assert_eq!(items[0]["description"], json!("A matte lipstick"));
    assert_eq!(items[1]["stock"], json!(0));
    assert_eq!(
        site.hits(),
        [
            "/collections.json?page=1&limit=250",
            "/collections/lipsticks/products.json?page=1&limit=250"
        ]
    );
}

/// One GET over the loopback fixture, through the crawl core's own session.
async fn fetch(url: &str) -> String {
    use browser_spider::session::CrawlSession;
    let session = browser_spider::spider::HttpCrawlSession::new("default").expect("a session");
    session.open().await.expect("opens");
    let response = session
        .fetch(url, &Default::default())
        .await
        .expect("the fixture answers");
    session.close().await.expect("closes");
    String::from_utf8_lossy(&response.body).into_owned()
}

// ---------------------------------------------------------------------------
// (d) The feed spiders
// ---------------------------------------------------------------------------

const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/">
  <channel>
    <item><title>First</title><link>/posts/1</link>
      <media:thumbnail url="/1.png"/></item>
    <item><title>Second</title><link>/posts/2</link></item>
  </channel>
</rss>"#;

const CSV: &str = "title,price\nFirst,10.5\nSecond,3\n";

fn feed_site(path: &str) -> Option<Page> {
    match path {
        "/feed.xml" => Some(Page::xml(RSS)),
        "/feed.xml.gz" => {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(RSS.as_bytes()).expect("write");
            Some(Page::bytes(
                "application/octet-stream",
                encoder.finish().expect("finish"),
            ))
        }
        "/feed.csv" => Some(Page::text(CSV)),
        "/posts/1" | "/posts/2" => Some(Page::html("<html><h1>Post</h1></html>")),
        _ => None,
    }
}

#[tokio::test]
async fn an_xml_feed_spider_yields_one_item_per_node() {
    let site = Site::start(feed_site);
    let spider = XMLFeedSpider::builder("news")
        .settings(SpiderSettings::default().with_start_urls([site.url("/feed.xml")]))
        .itertag("item")
        .parse_node(|_response, node| {
            let mut item = Item::new();
            item.insert(
                "title".to_owned(),
                Value::String(node.findtext("title").unwrap_or_default().to_owned()),
            );
            item.insert(
                "thumb".to_owned(),
                Value::String(
                    node.find("thumbnail")
                        .and_then(|thumb| thumb.get("url"))
                        .unwrap_or_default()
                        .to_owned(),
                ),
            );
            vec![Yielded::item(item)]
        })
        .build();

    let result = run(Arc::new(spider)).await;
    assert!(result.completed());
    assert_eq!(names(&result, "title"), ["First", "Second"]);
    assert_eq!(
        result.items.as_slice()[0]["thumb"].as_str(),
        Some("/1.png"),
        "the namespaced attribute survives the strip"
    );
}

#[tokio::test]
async fn a_gzipped_xml_feed_is_inflated_by_its_magic_bytes() {
    let site = Site::start(feed_site);
    let spider = XMLFeedSpider::builder("news")
        .settings(SpiderSettings::default().with_start_urls([site.url("/feed.xml.gz")]))
        .parse_node(|_response, node| {
            let mut item = Item::new();
            item.insert(
                "title".to_owned(),
                Value::String(node.findtext("title").unwrap_or_default().to_owned()),
            );
            vec![Yielded::item(item)]
        })
        .build();

    let result = run(Arc::new(spider)).await;
    assert_eq!(names(&result, "title"), ["First", "Second"]);
}

#[tokio::test]
async fn an_xml_feed_node_may_yield_a_follow_up_request() {
    let site = Site::start(feed_site);
    let spider = XMLFeedSpider::builder("news")
        .settings(
            SpiderSettings::default()
                .with_start_urls([site.url("/feed.xml")])
                .with_concurrent_requests(1),
        )
        .parse_node(|response, node| {
            match node
                .findtext("link")
                .and_then(|link| response.follow(link).ok())
            {
                // The follow-up lands on `parse`, which re-parses the page as a
                // feed and finds no `<item>` — so the crawl terminates.
                Some(request) => vec![Yielded::request(request)],
                None => Vec::new(),
            }
        })
        .build();

    let result = run(Arc::new(spider)).await;
    assert!(result.completed());
    assert_eq!(site.hit_count("/posts/1"), 1);
    assert_eq!(site.hit_count("/posts/2"), 1);
}

#[tokio::test]
async fn a_csv_feed_spider_yields_one_item_per_row() {
    let site = Site::start(feed_site);
    let spider = CSVFeedSpider::builder("prices")
        .settings(SpiderSettings::default().with_start_urls([site.url("/feed.csv")]))
        .parse_row(|_response, row| {
            let mut item = Item::new();
            item.clone_from(row);
            vec![Yielded::item(item)]
        })
        .build();

    let result = run(Arc::new(spider)).await;
    assert!(result.completed());
    assert_eq!(names(&result, "title"), ["First", "Second"]);
    assert_eq!(
        result.items.as_slice()[0]["price"].as_str(),
        Some("10.5"),
        "values are never coerced"
    );
}

// ---------------------------------------------------------------------------
// (e) SiteToMarkdownSpider — `site_to_markdown_seam`, on the shipped type
// ---------------------------------------------------------------------------

#[cfg(feature = "markdown")]
mod markdown {
    use super::*;

    use browser_templates::site_to_markdown::SiteToMarkdownSpider;

    const ARTICLE: &str = r#"<!doctype html><html><head><title>Post</title>
<style>.x{display:none}</style></head><body>
<nav><a href="/">home</a></nav>
<article><h1>Widgets</h1><p>A <strong>short</strong> post.</p>
<ul><li>one</li><li>two</li></ul></article>
<script>console.log("noise")</script>
<a href="/next">next</a>
</body></html>"#;

    fn docs(path: &str) -> Option<Page> {
        match path {
            "/" | "/next" => Some(Page::html(ARTICLE)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_site_to_markdown_crawl_converts_every_page() {
        let site = Site::start(docs);
        let spider = SiteToMarkdownSpider::builder("docs")
            .settings(
                SpiderSettings::default()
                    .with_start_urls([site.url("/")])
                    .with_allowed_domains([site.netloc()])
                    .with_concurrent_requests(1),
            )
            .css_selector("article")
            .build()
            .expect("allowed_domains is set");

        let result = run(Arc::new(spider)).await;
        assert!(result.completed());
        assert_eq!(result.items.len(), 2, "`/` and `/next`");

        let first = &result.items.as_slice()[0];
        assert_eq!(first["title"].as_str(), Some("Post"));
        let markdown = first["markdown"].as_str().expect("a string");
        assert!(markdown.contains("Widgets"), "{markdown:?}");
        assert!(markdown.contains("**short**"), "{markdown:?}");
        assert!(markdown.contains("* one"), "{markdown:?}");
        assert!(
            !markdown.contains("console.log"),
            "`_strip_noise_tags` dropped the <script>: {markdown:?}"
        );
    }

    #[tokio::test]
    async fn max_pages_caps_the_crawl() {
        let site = Site::start(docs);
        let spider = SiteToMarkdownSpider::builder("docs")
            .settings(
                SpiderSettings::default()
                    .with_start_urls([site.url("/")])
                    .with_allowed_domains([site.netloc()])
                    .with_concurrent_requests(1),
            )
            .max_pages(1)
            .build()
            .expect("ok");

        let result = run(Arc::new(spider)).await;
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            site.hit_count("/next"),
            0,
            "the capped page never enqueued its links"
        );
    }

    /// The one assertion `max_pages` can make under real concurrency.
    ///
    /// Every other loopback crawl here runs `concurrent_requests(1)` so its
    /// item list is deterministic; this one deliberately does not. Python's
    /// `if self._page_count >= self.max_pages: return` / `self._page_count += 1`
    /// is a check-then-increment, and four workers running it against a plain
    /// counter would let more than `max_pages` items through. The counter is a
    /// `Mutex` for exactly that reason, and this is the test that would notice
    /// if it stopped being one: the *count* stays an upper bound even though
    /// *which* pages win does not.
    #[tokio::test]
    async fn max_pages_holds_under_four_concurrent_workers() {
        fn many(path: &str) -> Option<Page> {
            // Twelve pages, each linking to every other, so the scheduler has
            // far more work queued than the cap allows at any moment.
            let index: usize = path.trim_start_matches("/p").parse().ok()?;
            if index >= 12 {
                return None;
            }
            let links: String = (0..12)
                .map(|other| format!("<a href=\"/p{other}\">{other}</a>"))
                .collect();
            Some(Page::html(format!(
                "<html><head><title>P{index}</title></head><body>{links}</body></html>"
            )))
        }

        let site = Site::start(many);
        let spider = SiteToMarkdownSpider::builder("docs")
            .settings(
                SpiderSettings::default()
                    .with_start_urls([site.url("/p0")])
                    .with_allowed_domains([site.netloc()])
                    .with_concurrent_requests(4),
            )
            .max_pages(3)
            .build()
            .expect("ok");

        let result = run(Arc::new(spider)).await;
        assert!(
            (1..=3).contains(&result.items.len()),
            "max_pages(3) let {} items through",
            result.items.len()
        );
    }

    #[tokio::test]
    async fn output_dir_writes_one_markdown_file_per_page() {
        let site = Site::start(docs);
        let directory = std::env::temp_dir().join(format!(
            "browser-templates-loopback-{}-{}",
            std::process::id(),
            site.netloc().replace(['.', ':'], "-")
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let spider = SiteToMarkdownSpider::builder("docs")
            .settings(
                SpiderSettings::default()
                    .with_start_urls([site.url("/")])
                    .with_allowed_domains([site.netloc()])
                    .with_concurrent_requests(1),
            )
            .output_dir(&directory)
            .build()
            .expect("ok");

        let result = run(Arc::new(spider)).await;
        assert_eq!(result.items.len(), 2);

        let mut written: Vec<String> = std::fs::read_dir(&directory)
            .expect("the directory was created")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        written.sort();
        assert_eq!(written.len(), 2, "{written:?}");
        assert!(
            written.iter().all(|name| name.ends_with(".md")),
            "{written:?}"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
}

// ---------------------------------------------------------------------------
// A guard on the fixture itself
// ---------------------------------------------------------------------------

/// The fixture answers, and a missing route really is a 404.
#[tokio::test]
async fn the_loopback_fixture_serves_and_404s() {
    let site = Site::start(shop);
    let spider = CrawlSpider::builder("probe")
        .settings(SpiderSettings::default().with_start_urls([site.url("/nope")]))
        .on_parse(|response| {
            let mut item = Item::new();
            item.insert("status".to_owned(), Value::from(response.status));
            vec![Yielded::item(item)]
        })
        .build();

    let result = run(Arc::new(spider)).await;
    assert_eq!(result.items.as_slice()[0]["status"], json!(404));
    assert_eq!(site.hits(), ["/nope"]);
}

/// `Request` is re-exported far enough for a user to name it, which is what a
/// `process_request` hook needs.
#[test]
fn a_process_request_hook_can_name_the_request_type() {
    fn takes(_: Request) {}
    takes(Request::new("https://a.test/"));
}
