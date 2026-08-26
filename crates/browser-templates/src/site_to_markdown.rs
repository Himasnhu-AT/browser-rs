//! [`SiteToMarkdownSpider`], calling the markdown converter through
//! `spawn_blocking`.
//!
//! A port of `reference/spiders/templates/site_to_markdown.py`
//! (`spider-support.md` §12). It crawls a site and emits one item per page —
//! `{url, title, markdown}` — optionally writing each page to its own `.md`
//! file. It is the RAG/LLM ingestion template, and the one place in this crate
//! where a callback does real CPU work, which is why the conversion is handed
//! to `tokio`'s blocking pool rather than run on the crawl's reactor thread.
//!
//! The whole spider is behind the crate's **`markdown`** feature, because
//! `Response::markdown()` lives in `browser-http` behind the same one.
//!
//! Its premise is that markdown conversion is a property of the **response**,
//! not of the transport, so the same spider works over the HTTP fetcher and
//! over real Chrome. `crates/browser-rs/tests/integration.rs::site_to_markdown_seam`
//! is the proof, and it runs both arms.
//!
//! ```
//! # #[cfg(feature = "markdown")] {
//! use browser_templates::site_to_markdown::SiteToMarkdownSpider;
//! use browser_spider::spider::SpiderSettings;
//!
//! let spider = SiteToMarkdownSpider::builder("docs")
//!     .settings(
//!         SpiderSettings::default()
//!             .with_start_urls(["https://docs.test/"])
//!             .with_allowed_domains(["docs.test"]),
//!     )
//!     .css_selector("article")
//!     .max_pages(50)
//!     .build()?;
//!
//! assert_eq!(spider.filename_for("https://docs.test/guide/intro"), "docs.test-guide-intro");
//! # }
//! # Ok::<(), browser_templates::error::TemplateError>(())
//! ```

#[cfg(feature = "markdown")]
use std::collections::HashSet;
#[cfg(feature = "markdown")]
use std::path::{Path, PathBuf};
#[cfg(feature = "markdown")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "markdown")]
use async_trait::async_trait;
#[cfg(feature = "markdown")]
use browser_spider::request::Item;
#[cfg(feature = "markdown")]
use browser_spider::spider::{CallbackStream, CrawlResponse, Spider, SpiderSettings, Yielded};
#[cfg(feature = "markdown")]
use regex::Regex;
#[cfg(feature = "markdown")]
use serde_json::Value;

#[cfg(feature = "markdown")]
use crate::crawl::{requests_to_follow, CrawlRule};
#[cfg(feature = "markdown")]
use crate::error::{Result, TemplateError};
#[cfg(feature = "markdown")]
use crate::links::LinkExtractor;

// ---------------------------------------------------------------------------
// SHA-1
// ---------------------------------------------------------------------------

/// FIPS 180-4 SHA-1, hand-written.
///
/// `_filename_for` suffixes a colliding name with the first eight hex digits of
/// `sha1(url)`, and that suffix is **observable** — it is part of the file name
/// a user ends up with. `browser-templates` has no `sha1` dependency and this
/// workspace's dependency set is frozen, so the twenty lines are written out
/// rather than approximated with a different hash.
#[cfg(feature = "markdown")]
fn sha1_hex(message: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    let bit_length = (message.len() as u64).wrapping_mul(8);
    padded.extend_from_slice(&bit_length.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0_u32; 80];
        for (index, word) in block.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

// ---------------------------------------------------------------------------
// The spider
// ---------------------------------------------------------------------------

/// A spider that crawls a website and converts every page to LLM-ready
/// Markdown.
///
/// Yields one item per page with `url`, `title` and `markdown` keys, and — when
/// `output_dir` is set — writes each page to a file named after its URL. Every
/// link is followed by default; give it rules to narrow the crawl.
///
/// `allowed_domains` is **required**, so the crawl stays bound to the target
/// site.
#[cfg(feature = "markdown")]
pub struct SiteToMarkdownSpider {
    name: String,
    settings: SpiderSettings,
    css_selector: Option<String>,
    main_content_only: bool,
    output_dir: Option<PathBuf>,
    max_pages: usize,
    rules: Vec<CrawlRule>,
    /// Python's `self._page_count`. A `Mutex` rather than an atomic because the
    /// cap check and the increment must be one step: Python's engine is
    /// single-threaded cooperative, so its `if … >= max_pages` and its `+= 1`
    /// cannot interleave, and with four workers here they otherwise would.
    page_count: Mutex<usize>,
    /// Python's `self._used_names`.
    used_names: Mutex<HashSet<String>>,
}

#[cfg(feature = "markdown")]
impl std::fmt::Debug for SiteToMarkdownSpider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteToMarkdownSpider")
            .field("name", &self.name)
            .field("css_selector", &self.css_selector)
            .field("main_content_only", &self.main_content_only)
            .field("output_dir", &self.output_dir)
            .field("max_pages", &self.max_pages)
            .field("rules", &self.rules.len())
            .field("pages", &self.pages_converted())
            .finish()
    }
}

#[cfg(feature = "markdown")]
impl SiteToMarkdownSpider {
    /// Start building one.
    #[must_use]
    pub fn builder(name: impl Into<String>) -> SiteToMarkdownSpiderBuilder {
        SiteToMarkdownSpiderBuilder::new(name)
    }

    /// How many pages have been converted so far.
    #[must_use]
    pub fn pages_converted(&self) -> usize {
        *self.page_count.lock().expect("not poisoned")
    }

    /// Where the `.md` files go, when they go anywhere.
    #[must_use]
    pub fn output_dir(&self) -> Option<&Path> {
        self.output_dir.as_deref()
    }

    /// The rule table. Defaults to a single follow-everything rule.
    #[must_use]
    pub fn rules(&self) -> &[CrawlRule] {
        &self.rules
    }

    /// A unique, filesystem-safe name for `url`, **without** the `.md` suffix.
    ///
    /// `netloc + path`, stripped of surrounding `/`, with every run of
    /// characters outside `[A-Za-z0-9._-]` replaced by a single `-`, then
    /// stripped of surrounding `-`, then truncated to 150 characters, falling
    /// back to `"index"` when nothing is left. On a collision an eight-hex-digit
    /// SHA-1 of the URL is appended.
    ///
    /// Query strings and fragments are **not** part of the name, so two URLs
    /// that differ only by query collide and the second gets the hash suffix.
    /// The suffixed name is what is recorded; the original stays recorded from
    /// the first call.
    ///
    /// This mutates `self` (it records the name), exactly as Python's does.
    #[must_use]
    pub fn filename_for(&self, url: &str) -> String {
        static UNSAFE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let unsafe_run =
            UNSAFE.get_or_init(|| Regex::new(r"[^A-Za-z0-9._-]+").expect("a valid pattern"));

        let parts = browser_spider::canonicalize::urlparse(url);
        let joined = format!("{}{}", parts.netloc, parts.path);
        let substituted = unsafe_run.replace_all(joined.trim_matches('/'), "-");
        let trimmed = substituted.trim_matches('-');
        // Python slices by code point, not by byte.
        let truncated: String = trimmed.chars().take(150).collect();
        let mut name = if truncated.is_empty() {
            "index".to_owned()
        } else {
            truncated
        };

        let mut used = self.used_names.lock().expect("not poisoned");
        if used.contains(&name) {
            name = format!("{name}-{}", &sha1_hex(url.as_bytes())[..8]);
        }
        used.insert(name.clone());
        name
    }

    /// Take one page off the `max_pages` budget.
    ///
    /// Returns `false` when the cap has already been reached, in which case
    /// `parse` yields **nothing at all** — not even the follow requests.
    fn claim_page(&self) -> bool {
        let mut count = self.page_count.lock().expect("not poisoned");
        if self.max_pages != 0 && *count >= self.max_pages {
            return false;
        }
        *count += 1;
        true
    }

    /// Whether this page may still enqueue its links.
    fn may_follow(&self) -> bool {
        self.max_pages == 0 || self.pages_converted() < self.max_pages
    }

    /// The item Python's `parse` yields first.
    ///
    /// # Errors
    ///
    /// [`TemplateError::Output`] carrying the response URL when `css_selector`
    /// does not compile — the only way the conversion can fail. `parse` turns
    /// it into [`Yielded::error`], which is where Python's `SelectorError`
    /// out of `response.markdown(...)` would land too.
    pub fn page_item(&self, response: &CrawlResponse) -> Result<Item> {
        let markdown = response
            .markdown_with(self.css_selector.as_deref(), self.main_content_only)
            .map_err(|error| TemplateError::Output {
                path: response.url.clone(),
                message: error.to_string(),
            })?;
        let title = response
            .css("title::text")
            .ok()
            .and_then(|nodes| nodes.get())
            .unwrap_or_default()
            .trim()
            .to_owned();

        let mut item = Item::new();
        item.insert("url".to_owned(), Value::String(response.url.clone()));
        item.insert("title".to_owned(), Value::String(title));
        item.insert("markdown".to_owned(), Value::String(markdown));
        Ok(item)
    }

    /// Write one item to `output_dir`, when there is one.
    ///
    /// # Errors
    ///
    /// [`TemplateError::Output`] when the directory or the file cannot be
    /// written. Python lets the `OSError` propagate out of `on_scraped_item`,
    /// which the engine's item pipeline turns into a dropped item plus an
    /// error log; the port logs and keeps the item, which is the safer half of
    /// that trade and is noted in `API.md`.
    pub fn write_item(&self, item: &Item) -> Result<()> {
        let Some(directory) = &self.output_dir else {
            return Ok(());
        };
        let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
        let markdown = item
            .get("markdown")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = directory.join(format!("{}.md", self.filename_for(url)));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| TemplateError::Output {
                path: parent.display().to_string(),
                message: error.to_string(),
            })?;
        }
        std::fs::write(&path, markdown).map_err(|error| TemplateError::Output {
            path: path.display().to_string(),
            message: error.to_string(),
        })
    }
}

#[cfg(feature = "markdown")]
#[async_trait]
impl Spider for SiteToMarkdownSpider {
    fn name(&self) -> &str {
        &self.name
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
        if !self.claim_page() {
            // The cap is reached: this page is fetched but neither converted
            // nor followed.
            return Box::pin(futures::stream::iter(Vec::new()));
        }

        // The conversion walks the whole DOM twice (strip, then render) and is
        // the only genuinely CPU-bound step in the crate, so it goes to the
        // blocking pool. `CrawlResponse` is `Send`, so it can be moved across
        // and handed back.
        let spider = Arc::clone(&self);
        let (item, response) = match tokio::runtime::Handle::try_current() {
            Ok(_) => tokio::task::spawn_blocking(move || {
                let item = spider.page_item(&response);
                (item, response)
            })
            .await
            .unwrap_or_else(|error| {
                unreachable!("the markdown conversion cannot panic or be cancelled: {error}")
            }),
            // No runtime (a `futures::executor` harness, say): run it inline.
            Err(_) => {
                let item = spider.page_item(&response);
                (item, response)
            }
        };

        let mut yielded = match item {
            Ok(item) => vec![Yielded::item(item)],
            Err(error) => return Box::pin(futures::stream::iter([Yielded::error(error)])),
        };

        if self.may_follow() {
            match requests_to_follow(&self.rules, &response) {
                Ok(requests) => yielded.extend(requests.into_iter().map(Yielded::request)),
                Err(error) => yielded.push(Yielded::error(error)),
            }
        }
        Box::pin(futures::stream::iter(yielded))
    }

    /// Write the item to `output_dir`, then pass it through unchanged.
    ///
    /// Python never drops an item here, and neither does this — a write failure
    /// is logged and the item still reaches the exporter.
    async fn on_scraped_item(&self, item: Item) -> Option<Item> {
        if self.output_dir.is_some() {
            if let Err(error) = self.write_item(&item) {
                tracing::warn!("Failed to write markdown item: {error}");
            }
        }
        Some(item)
    }
}

/// Builder for [`SiteToMarkdownSpider`].
#[cfg(feature = "markdown")]
#[derive(Default)]
pub struct SiteToMarkdownSpiderBuilder {
    name: String,
    settings: SpiderSettings,
    css_selector: Option<String>,
    main_content_only: Option<bool>,
    output_dir: Option<PathBuf>,
    max_pages: usize,
    rules: Option<Vec<CrawlRule>>,
}

#[cfg(feature = "markdown")]
impl std::fmt::Debug for SiteToMarkdownSpiderBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteToMarkdownSpiderBuilder")
            .field("name", &self.name)
            .field("css_selector", &self.css_selector)
            .field("output_dir", &self.output_dir)
            .field("max_pages", &self.max_pages)
            .finish()
    }
}

#[cfg(feature = "markdown")]
impl SiteToMarkdownSpiderBuilder {
    /// A builder for a spider called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Every `Spider` class attribute. `allowed_domains` must not be empty.
    #[must_use]
    pub fn settings(mut self, settings: SpiderSettings) -> Self {
        self.settings = settings;
        self
    }

    /// `css_selector=` — convert only the matching elements of each page.
    #[must_use]
    pub fn css_selector(mut self, selector: impl Into<String>) -> Self {
        self.css_selector = Some(selector.into());
        self
    }

    /// `main_content_only=` — convert only what is inside `<body>`. Default
    /// `true`.
    #[must_use]
    pub fn main_content_only(mut self, enabled: bool) -> Self {
        self.main_content_only = Some(enabled);
        self
    }

    /// `output_dir=` — when set, each page is also written there as a `.md`.
    #[must_use]
    pub fn output_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(directory.into());
        self
    }

    /// `max_pages=` — the conversion cap. `0` disables it.
    #[must_use]
    pub fn max_pages(mut self, max_pages: usize) -> Self {
        self.max_pages = max_pages;
        self
    }

    /// Replace the default follow-every-link rule.
    #[must_use]
    pub fn rules<I>(mut self, rules: I) -> Self
    where
        I: IntoIterator<Item = CrawlRule>,
    {
        self.rules = Some(rules.into_iter().collect());
        self
    }

    /// Freeze it.
    ///
    /// # Errors
    ///
    /// [`TemplateError::MissingAllowedDomains`] when `allowed_domains` is
    /// empty — Python raises `ValueError` from `__init__` for the same reason:
    /// without it the crawl is unbounded.
    pub fn build(self) -> Result<SiteToMarkdownSpider> {
        if self.settings.allowed_domains.is_empty() {
            return Err(TemplateError::MissingAllowedDomains { spider: self.name });
        }
        Ok(SiteToMarkdownSpider {
            name: self.name,
            settings: self.settings,
            css_selector: self.css_selector,
            main_content_only: self.main_content_only.unwrap_or(true),
            output_dir: self.output_dir,
            max_pages: self.max_pages,
            // Python's default `rules()` is `[CrawlRule(LinkExtractor())]`:
            // follow every link, with the default extension deny-list applied.
            // Off-site links are dropped later by the engine's
            // `allowed_domains` check.
            rules: self
                .rules
                .unwrap_or_else(|| vec![CrawlRule::new(LinkExtractor::default())]),
            page_count: Mutex::new(0),
            used_names: Mutex::new(HashSet::new()),
        })
    }
}

#[cfg(all(test, feature = "markdown"))]
mod tests {
    use browser_spider::request::Request;
    use browser_spider::session::FetchedResponse;
    use futures::StreamExt;

    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>  Welcome  </title></head>
<body>
  <h1>Welcome</h1>
  <div class="content">Main content.</div>
  <a href="/docs/page-1">docs</a>
  <a href="/blog/post-1">blog</a>
</body></html>"#;

    fn spider() -> Arc<SiteToMarkdownSpider> {
        Arc::new(
            SiteToMarkdownSpider::builder("site")
                .settings(
                    SpiderSettings::default()
                        .with_start_urls(["https://example.com/"])
                        .with_allowed_domains(["example.com"]),
                )
                .build()
                .expect("allowed_domains is set"),
        )
    }

    fn response() -> CrawlResponse {
        let mut fetched = FetchedResponse::new(200, "https://example.com/", PAGE);
        fetched.attach_request(&Request::new("https://example.com/").with_sid("default"));
        CrawlResponse::from_fetched(&fetched, &Default::default(), None)
    }

    // -- sha1 --------------------------------------------------------------

    #[test]
    fn the_hand_written_sha1_matches_the_published_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // Two blocks, to exercise the chunk loop.
        assert_eq!(
            sha1_hex(&[b'a'; 1000]),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    // -- construction ------------------------------------------------------

    #[test]
    fn allowed_domains_is_required() {
        let error = SiteToMarkdownSpider::builder("site")
            .build()
            .expect_err("unbounded");
        assert_eq!(
            error.to_string(),
            "site must set `allowed_domains` to bound the crawl to the target website(s)"
        );
    }

    #[test]
    fn the_default_rule_follows_every_link() {
        assert_eq!(spider().rules().len(), 1);
    }

    // -- filename ----------------------------------------------------------

    #[test]
    fn filenames_match_pythons() {
        let spider = spider();
        assert_eq!(spider.filename_for("https://example.com/"), "example.com");
        assert_eq!(
            spider.filename_for("https://example.com/docs/page-1"),
            "example.com-docs-page-1"
        );
    }

    #[test]
    fn a_collision_gets_an_eight_hex_sha1_suffix() {
        let spider = spider();
        assert_eq!(spider.filename_for("https://example.com/"), "example.com");
        // Same netloc+path, different query: the name collides.
        let second = spider.filename_for("https://example.com/?a=1");
        assert!(second.starts_with("example.com-"), "{second}");
        assert_eq!(second.len(), "example.com-".len() + 8, "{second}");
        assert_eq!(
            second,
            format!(
                "example.com-{}",
                &sha1_hex(b"https://example.com/?a=1")[..8]
            )
        );
    }

    #[test]
    fn an_empty_name_falls_back_to_index() {
        let spider = spider();
        assert_eq!(spider.filename_for("https:///"), "index");
    }

    #[test]
    fn the_name_is_truncated_to_one_hundred_and_fifty_characters() {
        let spider = spider();
        let long = format!("https://example.com/{}", "a".repeat(400));
        assert_eq!(spider.filename_for(&long).chars().count(), 150);
    }

    // -- parse -------------------------------------------------------------

    #[tokio::test]
    async fn parse_yields_the_item_then_the_follow_requests() {
        let spider = spider();
        let yielded: Vec<Yielded> = Arc::clone(&spider).parse(response()).await.collect().await;
        assert_eq!(yielded.len(), 3);
        match &yielded[0] {
            Yielded::Item(item) => {
                assert_eq!(item["url"], "https://example.com/");
                assert_eq!(item["title"], "Welcome", "the title is trimmed");
                let markdown = item["markdown"].as_str().expect("a string");
                assert!(markdown.contains("Welcome"), "{markdown:?}");
                assert!(
                    !markdown.contains("  Welcome  "),
                    "main_content_only keeps <head> out: {markdown:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        let urls: Vec<String> = yielded[1..]
            .iter()
            .filter_map(|item| match item {
                Yielded::Request(request) => Some(request.url.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            urls,
            [
                "https://example.com/docs/page-1",
                "https://example.com/blog/post-1"
            ]
        );
    }

    #[tokio::test]
    async fn max_pages_stops_conversion_and_following() {
        let spider = Arc::new(
            SiteToMarkdownSpider::builder("site")
                .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
                .max_pages(1)
                .build()
                .expect("ok"),
        );
        let first: Vec<Yielded> = Arc::clone(&spider).parse(response()).await.collect().await;
        assert_eq!(first.len(), 1, "one item and no requests: {first:?}");
        assert!(matches!(first[0], Yielded::Item(_)));

        let second: Vec<Yielded> = spider.parse(response()).await.collect().await;
        assert!(second.is_empty(), "the cap yields nothing at all");
    }

    /// `claim_page` is a read-modify-write under one guard, and this is what
    /// notices if it stops being.
    ///
    /// Python's `if self._page_count >= self.max_pages: return` followed by
    /// `self._page_count += 1` is a check-then-increment; under the GIL and a
    /// single-threaded event loop it cannot interleave, but a Rust engine runs
    /// callbacks on a multi-threaded runtime and it can. Only the count is
    /// asserted — *which* eight of the sixteen callers win is unspecified.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn max_pages_is_not_a_check_then_increment() {
        for _ in 0..20 {
            let spider = Arc::new(
                SiteToMarkdownSpider::builder("site")
                    .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
                    .max_pages(8)
                    .build()
                    .expect("ok"),
            );
            let callers: Vec<_> = (0..16)
                .map(|_| {
                    let spider = Arc::clone(&spider);
                    tokio::spawn(async move {
                        let yielded: Vec<Yielded> = spider.parse(response()).await.collect().await;
                        usize::from(
                            yielded
                                .first()
                                .is_some_and(|first| matches!(first, Yielded::Item(_))),
                        )
                    })
                })
                .collect();
            let mut converted = 0;
            for caller in callers {
                converted += caller.await.expect("no panic");
            }
            assert_eq!(converted, 8, "max_pages(8) let {converted} pages through");
            assert_eq!(spider.pages_converted(), 8);
        }
    }

    #[tokio::test]
    async fn rules_narrow_the_crawl() {
        let blog = SiteToMarkdownSpider::builder("site")
            .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
            .rules([CrawlRule::new(
                LinkExtractor::builder()
                    .allow([r"/blog/"])
                    .build()
                    .expect("compiles"),
            )])
            .build()
            .expect("ok");
        let yielded: Vec<Yielded> = Arc::new(blog).parse(response()).await.collect().await;
        assert_eq!(yielded.len(), 2);
        match &yielded[1] {
            Yielded::Request(request) => {
                assert_eq!(request.url, "https://example.com/blog/post-1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_css_selector_narrows_the_markdown() {
        let spider = Arc::new(
            SiteToMarkdownSpider::builder("site")
                .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
                .css_selector(".content")
                .build()
                .expect("ok"),
        );
        let yielded: Vec<Yielded> = spider.parse(response()).await.collect().await;
        match &yielded[0] {
            Yielded::Item(item) => {
                let markdown = item["markdown"].as_str().expect("a string");
                assert!(markdown.contains("Main content."), "{markdown:?}");
                assert!(!markdown.contains("Welcome"), "{markdown:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_bad_css_selector_is_a_callback_error() {
        let spider = Arc::new(
            SiteToMarkdownSpider::builder("site")
                .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
                .css_selector("!!!")
                .build()
                .expect("ok"),
        );
        let yielded: Vec<Yielded> = spider.parse(response()).await.collect().await;
        assert!(
            matches!(yielded.as_slice(), [Yielded::Error(_)]),
            "{yielded:?}"
        );
    }

    // -- on_scraped_item ---------------------------------------------------

    #[tokio::test]
    async fn output_dir_writes_one_file_per_page() {
        let directory = std::env::temp_dir().join(format!(
            "browser-templates-{}-{}",
            std::process::id(),
            sha1_hex(b"output_dir_writes_one_file_per_page")
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let spider = Arc::new(
            SiteToMarkdownSpider::builder("site")
                .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
                .output_dir(&directory)
                .build()
                .expect("ok"),
        );

        let yielded: Vec<Yielded> = Arc::clone(&spider).parse(response()).await.collect().await;
        let Yielded::Item(item) = &yielded[0] else {
            panic!("expected an item")
        };

        let returned = spider.on_scraped_item(item.clone()).await;
        assert_eq!(returned.as_ref(), Some(item), "the item is never dropped");

        let path = directory.join("example.com.md");
        let written = std::fs::read_to_string(&path).expect("written");
        assert_eq!(written, item["markdown"].as_str().expect("a string"));

        // A second call for the same URL lands on a hashed name.
        spider.on_scraped_item(item.clone()).await;
        let names: Vec<String> = std::fs::read_dir(&directory)
            .expect("readable")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names
                .iter()
                .any(|name| name.starts_with("example.com-") && name.ends_with(".md")),
            "{names:?}"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn without_an_output_dir_nothing_is_written() {
        let spider = spider();
        let mut item = Item::new();
        item.insert(
            "url".to_owned(),
            Value::String("https://a.test/".to_owned()),
        );
        item.insert("markdown".to_owned(), Value::String("x".to_owned()));
        assert_eq!(spider.on_scraped_item(item.clone()).await, Some(item));
        assert!(spider.output_dir().is_none());
    }
}
