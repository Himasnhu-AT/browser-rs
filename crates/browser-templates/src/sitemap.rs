//! [`SitemapSpider`] over [`crate::feed::parse_xml`], including the documented
//! callback-inheritance wart and the [`UnruledRoute::RouteToParse`] fix.
//!
//! A port of `reference/spiders/templates/sitemap.py` (`spider-support.md` §9).
//!
//! A `SitemapSpider` seeds its crawl from one or more `sitemap.xml` (or
//! `robots.txt`) URLs, recurses into every `<sitemapindex>` it finds, and routes
//! each `<urlset>` entry through a **first-match-wins** rule table — which is
//! the one place this crate's rule dispatch differs from
//! [`crate::crawl::requests_to_follow`], where *every* rule sees *every*
//! response.
//!
//! ```
//! use browser_templates::sitemap::{parse_sitemap, SitemapResult};
//!
//! let index = parse_sitemap(
//!     br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
//!            <sitemap><loc>https://a.test/posts.xml</loc></sitemap>
//!         </sitemapindex>"#,
//!     None,
//!     false,
//! )?;
//! assert_eq!(index, SitemapResult { urls: vec![], sitemaps: vec!["https://a.test/posts.xml".to_owned()] });
//! # Ok::<(), browser_templates::error::TemplateError>(())
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use browser_spider::canonicalize::urlparse;
use browser_spider::request::{CallbackId, Request};
use browser_spider::robots::{decode_robots_body, RobotsRules};
use browser_spider::spider::{
    CallbackStream, CrawlError, CrawlResponse, FollowOptions, RequestStream, Spider,
    SpiderSettings, Yielded,
};

use crate::crawl::{CallbackFn, CrawlRule};
use crate::error::{Result, TemplateError};
use crate::feed::XmlNode;
use crate::gzip::decompress;
use crate::links::LinkExtractor;

/// The callback name [`SitemapSpider`] routes its own sitemap fetches to.
///
/// Python binds `self._parse_sitemap`; the port names it, because the name is
/// what survives a checkpoint. The leading underscore is kept so a checkpoint
/// written here reads recognisably next to the Python one.
pub const PARSE_SITEMAP: CallbackId = CallbackId::from_static("_parse_sitemap");

/// A parsed sitemap body.
///
/// `urls` holds `<urlset>` entries; `sitemaps` holds the child sitemap URLs of a
/// `<sitemapindex>`, each of which is fetched recursively.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SitemapResult {
    /// `<urlset>` entries, in document order.
    pub urls: Vec<String>,
    /// `<sitemapindex>` children, in document order.
    pub sitemaps: Vec<String>,
}

/// Where a `<urlset>` URL goes when no rule claims it.
///
/// `spider-support.md` §9.6 documents a genuine wart in Python here and
/// §14.6 item 11 asks a port to decide explicitly, so this is the decision, as
/// an option.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnruledRoute {
    /// **The default, and a declared divergence.** An unruled URL is fetched
    /// with `callback = parse`, which is what `SitemapSpider`'s own docstring
    /// says happens ("If there are no rules provided, all non-sitemap urls will
    /// be redirected to `parse()`").
    #[default]
    RouteToParse,
    /// Python's literal behaviour. `_dispatch` writes `response.follow(url)`
    /// with no callback, and `Response.follow` inherits the *originating*
    /// request's — which, in a real crawl, is `_parse_sitemap`. So the target
    /// page is fed back into the sitemap parser and logs "Failed to parse
    /// sitemap XML" for every HTML page it reaches. Python's own unit tests
    /// miss it because their fake response carries a request with
    /// `callback=None`.
    ///
    /// Select this when byte-fidelity with Python matters more than the crawl
    /// working.
    InheritSitemapCallback,
}

// ---------------------------------------------------------------------------
// Body parsing
// ---------------------------------------------------------------------------

/// Parse a sitemap body into its URLs or its child sitemaps.
///
/// Reproduces `_sm_body` + `_extract_urls`:
///
/// * gunzip first ([`decompress`]), by content-type or magic bytes;
/// * the root element's **local name** decides the branch, so the sitemaps.org
///   namespace is not required;
/// * `<sitemapindex>`: for each direct `<sitemap>` child, the **first** `<loc>`
///   with text, trimmed;
/// * `<urlset>`: for each direct `<url>` child, **every** `<loc>` with text,
///   plus — when `alternate_links` — every `<link href>`, interleaved in
///   document order;
/// * any other root is an empty result and a warning.
///
/// # Errors
///
/// [`TemplateError::GzipBomb`] / [`TemplateError::Gzip`] /
/// [`TemplateError::GzipTruncated`] from the inflate step, and
/// [`TemplateError::Xml`] from the parse. Python catches the first two families
/// and the third and warns; [`SitemapSpider`] does the same, and this function
/// hands them up so a caller can choose otherwise.
pub fn parse_sitemap(
    body: &[u8],
    content_type: Option<&str>,
    alternate_links: bool,
) -> Result<SitemapResult> {
    let body = decompress(body, content_type)?;
    let root = crate::feed::parse_xml(&body)?;

    match root.name.as_str() {
        "sitemapindex" => Ok(SitemapResult {
            urls: Vec::new(),
            sitemaps: index_locs(&root),
        }),
        "urlset" => Ok(SitemapResult {
            urls: urlset_locs(&root, alternate_links),
            sitemaps: Vec::new(),
        }),
        other => {
            tracing::warn!("Unknown sitemap root element: '{other}'");
            Ok(SitemapResult::default())
        }
    }
}

/// The first `<loc>` of every direct `<sitemap>` child.
fn index_locs(root: &XmlNode) -> Vec<String> {
    root.children
        .iter()
        .filter(|child| child.name == "sitemap")
        .filter_map(|entry| {
            // `break` after the first `<loc>` with text — Python's index branch
            // takes one per `<sitemap>`, unlike the `<urlset>` branch.
            entry
                .children
                .iter()
                .find(|child| child.name == "loc" && !child.text.is_empty())
                .map(|loc| loc.text.trim().to_owned())
        })
        .collect()
}

/// Every `<loc>` of every direct `<url>` child, plus the alternates.
fn urlset_locs(root: &XmlNode, alternate_links: bool) -> Vec<String> {
    let mut urls = Vec::new();
    for entry in &root.children {
        if entry.name != "url" {
            continue;
        }
        for child in &entry.children {
            if child.name == "loc" && !child.text.is_empty() {
                urls.push(child.text.trim().to_owned());
            } else if alternate_links && child.name == "link" {
                if let Some(href) = child.get("href").filter(|href| !href.is_empty()) {
                    urls.push(href.trim().to_owned());
                }
            }
        }
    }
    urls
}

/// The `Sitemap:` directives of a `robots.txt` body.
///
/// Python: `Protego.parse(body.decode(encoding, errors="replace")).sitemaps`,
/// with every failure swallowed into an empty list and a warning. The port uses
/// unit 16's `protego` port, whose `decode_robots_body` returns `None` exactly
/// where CPython's `bytes.decode` would raise.
#[must_use]
pub fn robots_sitemaps(body: &[u8], encoding: &str) -> Vec<String> {
    let Some(text) = decode_robots_body(body, encoding) else {
        tracing::warn!("Failed to parse robots.txt: unknown encoding '{encoding}'");
        return Vec::new();
    };
    RobotsRules::parse(&text).sitemaps().to_vec()
}

// ---------------------------------------------------------------------------
// The spider
// ---------------------------------------------------------------------------

/// A [`Spider`] that seeds a crawl from sitemap(s) and follows a rule table.
///
/// Unlike [`crate::crawl::CrawlSpider`], dispatch is **first-match-wins** and a
/// URL no rule matches is dropped.
pub struct SitemapSpider {
    name: String,
    settings: SpiderSettings,
    sitemap_urls: Vec<String>,
    sitemap_follow: Option<LinkExtractor>,
    sitemap_alternate_links: bool,
    unruled_route: UnruledRoute,
    rules: Vec<CrawlRule>,
    callbacks: HashMap<String, CallbackFn>,
}

impl std::fmt::Debug for SitemapSpider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.callbacks.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("SitemapSpider")
            .field("name", &self.name)
            .field("sitemap_urls", &self.sitemap_urls)
            .field("sitemap_follow", &self.sitemap_follow.is_some())
            .field("sitemap_alternate_links", &self.sitemap_alternate_links)
            .field("unruled_route", &self.unruled_route)
            .field("rules", &self.rules.len())
            .field("callbacks", &names)
            .finish()
    }
}

impl SitemapSpider {
    /// Start building one.
    #[must_use]
    pub fn builder(name: impl Into<String>) -> SitemapSpiderBuilder {
        SitemapSpiderBuilder::new(name)
    }

    /// The seed sitemap (or `robots.txt`) URLs.
    #[must_use]
    pub fn sitemap_urls(&self) -> &[String] {
        &self.sitemap_urls
    }

    /// The rule table.
    #[must_use]
    pub fn rules(&self) -> &[CrawlRule] {
        &self.rules
    }

    /// Route one `<urlset>` URL through the rules, first match wins.
    ///
    /// Returns `None` when a rule table exists and none of its extractors
    /// matched — Python drops such a URL silently.
    ///
    /// # Errors
    ///
    /// [`TemplateError::Crawl`] when `response` has no originating request.
    pub fn dispatch_url(&self, response: &CrawlResponse, url: &str) -> Result<Option<Request>> {
        if self.rules.is_empty() {
            let options = FollowOptions {
                callback: self.unruled_callback(),
                ..FollowOptions::default()
            };
            return Ok(Some(response.follow_with(url, &options)?));
        }
        for rule in &self.rules {
            if !rule.link_extractor.matches(url) {
                continue;
            }
            let options = FollowOptions {
                // As in `crate::crawl::requests_to_follow`: a rule names its
                // callback, and a rule with none means `parse`.
                callback: Some(rule.effective_callback()),
                priority: rule.priority,
                ..FollowOptions::default()
            };
            let request = response.follow_with(url, &options)?;
            return Ok(match &rule.process_request {
                Some(process) => process(request, response),
                None => Some(request),
            });
        }
        Ok(None)
    }

    /// What `FollowOptions::callback` an unruled URL gets.
    fn unruled_callback(&self) -> Option<CallbackId> {
        match self.unruled_route {
            UnruledRoute::RouteToParse => Some(CallbackId::PARSE),
            // `None` is exactly Python's `response.follow(url)`: inherit.
            UnruledRoute::InheritSitemapCallback => None,
        }
    }

    /// `_parse_sitemap`, as a value.
    fn parse_sitemap_yields(&self, response: &CrawlResponse) -> Vec<Yielded> {
        if urlparse(&response.url).path.ends_with("/robots.txt") {
            let sitemaps = robots_sitemaps(response.body(), &response.encoding);
            if sitemaps.is_empty() {
                tracing::warn!("No Sitemaps found in {}", response.url);
            }
            return sitemaps
                .into_iter()
                .map(|url| self.follow_sitemap(response, &url))
                .collect();
        }

        let content_type = response.header("content-type");
        let result =
            match parse_sitemap(response.body(), content_type, self.sitemap_alternate_links) {
                Ok(result) => result,
                Err(TemplateError::GzipTruncated) => {
                    // `EOFError` is not an `OSError`; Python does not catch it.
                    return vec![Yielded::error(TemplateError::GzipTruncated)];
                }
                Err(error @ (TemplateError::GzipBomb { .. } | TemplateError::Gzip { .. })) => {
                    tracing::warn!("Failed to decompress sitemap: {error}");
                    return Vec::new();
                }
                Err(error) => {
                    tracing::warn!("Failed to parse sitemap XML: {error}");
                    return Vec::new();
                }
            };

        let mut out = Vec::new();
        for child in &result.sitemaps {
            if let Some(follow) = &self.sitemap_follow {
                if !follow.matches(child) {
                    continue;
                }
            }
            out.push(self.follow_sitemap(response, child));
        }
        for url in &result.urls {
            match self.dispatch_url(response, url) {
                Ok(Some(request)) => out.push(Yielded::request(request)),
                Ok(None) => {}
                Err(error) => out.push(Yielded::error(error)),
            }
        }
        out
    }

    /// A recursive fetch of a child sitemap, or the error that stopped it.
    fn follow_sitemap(&self, response: &CrawlResponse, url: &str) -> Yielded {
        let options = FollowOptions {
            callback: Some(PARSE_SITEMAP),
            ..FollowOptions::default()
        };
        match response.follow_with(url, &options) {
            Ok(request) => Yielded::request(request),
            Err(error) => Yielded::error(error),
        }
    }
}

#[async_trait]
impl Spider for SitemapSpider {
    fn name(&self) -> &str {
        &self.name
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    /// One request per `sitemap_urls` entry, each naming [`PARSE_SITEMAP`].
    ///
    /// # Errors
    ///
    /// [`CrawlError::Callback`] carrying
    /// [`TemplateError::MissingSitemapUrls`]'s message when `sitemap_urls` is
    /// empty — Python raises `RuntimeError` from the generator, which the
    /// runner surfaces the same way.
    async fn start_requests(self: Arc<Self>) -> std::result::Result<RequestStream, CrawlError> {
        if self.sitemap_urls.is_empty() {
            return Err(CrawlError::callback(TemplateError::MissingSitemapUrls));
        }
        let requests: Vec<Request> = self
            .sitemap_urls
            .iter()
            .map(|url| Request::new(url.clone()).with_callback(PARSE_SITEMAP))
            .collect();
        Ok(Box::pin(futures::stream::iter(requests)))
    }

    /// Python's `parse` raises `NotImplementedError`; it exists only so
    /// unmatched URLs have a landing place.
    async fn parse(self: Arc<Self>, _response: CrawlResponse) -> CallbackStream {
        Box::pin(futures::stream::iter([Yielded::error(
            TemplateError::NotImplemented {
                spider: self.name.clone(),
                method: "parse",
            },
        )]))
    }

    async fn dispatch(
        self: Arc<Self>,
        callback: CallbackId,
        response: CrawlResponse,
    ) -> CallbackStream {
        if callback.as_str() == PARSE_SITEMAP.as_str() {
            let yielded = self.parse_sitemap_yields(&response);
            return Box::pin(futures::stream::iter(yielded));
        }
        if let Some(handler) = self.callbacks.get(callback.as_str()) {
            let yielded = handler(&response);
            return Box::pin(futures::stream::iter(yielded));
        }
        self.parse(response).await
    }

    fn has_callback(&self, callback: &CallbackId) -> bool {
        callback.as_str() == CallbackId::PARSE.as_str()
            || callback.as_str() == PARSE_SITEMAP.as_str()
            || self.callbacks.contains_key(callback.as_str())
    }
}

/// Builder for [`SitemapSpider`].
#[derive(Default)]
pub struct SitemapSpiderBuilder {
    name: String,
    settings: SpiderSettings,
    sitemap_urls: Vec<String>,
    sitemap_follow: Option<LinkExtractor>,
    sitemap_alternate_links: bool,
    unruled_route: UnruledRoute,
    rules: Vec<CrawlRule>,
    callbacks: HashMap<String, CallbackFn>,
}

impl std::fmt::Debug for SitemapSpiderBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SitemapSpiderBuilder")
            .field("name", &self.name)
            .field("sitemap_urls", &self.sitemap_urls)
            .field("rules", &self.rules.len())
            .finish()
    }
}

impl SitemapSpiderBuilder {
    /// A builder for a sitemap spider called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Every `Spider` class attribute.
    #[must_use]
    pub fn settings(mut self, settings: SpiderSettings) -> Self {
        self.settings = settings;
        self
    }

    /// `sitemap_urls=` — sitemap or `robots.txt` URLs. Required.
    #[must_use]
    pub fn sitemap_urls<I, S>(mut self, urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.sitemap_urls = urls.into_iter().map(Into::into).collect();
        self
    }

    /// `sitemap_follow=` — which children of a `<sitemapindex>` to descend
    /// into. Unset descends into all.
    #[must_use]
    pub fn sitemap_follow(mut self, extractor: LinkExtractor) -> Self {
        self.sitemap_follow = Some(extractor);
        self
    }

    /// `sitemap_alternate_links=` — also route `<xhtml:link rel=alternate>`
    /// hrefs.
    #[must_use]
    pub fn sitemap_alternate_links(mut self, enabled: bool) -> Self {
        self.sitemap_alternate_links = enabled;
        self
    }

    /// Where a URL no rule matched goes. See [`UnruledRoute`].
    #[must_use]
    pub fn unruled_route(mut self, route: UnruledRoute) -> Self {
        self.unruled_route = route;
        self
    }

    /// Append one rule. First match wins, so order matters more here than it
    /// does for [`crate::crawl::CrawlSpider`].
    #[must_use]
    pub fn rule(mut self, rule: CrawlRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Replace the whole rule list.
    #[must_use]
    pub fn rules<I>(mut self, rules: I) -> Self
    where
        I: IntoIterator<Item = CrawlRule>,
    {
        self.rules = rules.into_iter().collect();
        self
    }

    /// Register the handler a `Rule(callback=…)` names.
    #[must_use]
    pub fn callback<F>(mut self, id: CallbackId, handler: F) -> Self
    where
        F: Fn(&CrawlResponse) -> Vec<Yielded> + Send + Sync + 'static,
    {
        self.callbacks
            .insert(id.as_str().to_owned(), Arc::new(handler));
        self
    }

    /// Freeze it.
    #[must_use]
    pub fn build(self) -> SitemapSpider {
        SitemapSpider {
            name: self.name,
            settings: self.settings,
            sitemap_urls: self.sitemap_urls,
            sitemap_follow: self.sitemap_follow,
            sitemap_alternate_links: self.sitemap_alternate_links,
            unruled_route: self.unruled_route,
            rules: self.rules,
            callbacks: self.callbacks,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use futures::StreamExt;
    use browser_spider::session::FetchedResponse;

    use super::*;

    const POST: CallbackId = CallbackId::from_static("parse_post");

    const URLSET: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/posts/1</loc><lastmod>2024-01-01</lastmod></url>
  <url><loc>https://example.com/posts/2</loc></url>
  <url><loc>https://example.com/about</loc></url>
</urlset>"#;

    const INDEX: &[u8] = br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/posts-sitemap.xml</loc><lastmod>x</lastmod></sitemap>
  <sitemap><loc>https://example.com/pages-sitemap.xml</loc></sitemap>
  <sitemap><loc>https://example.com/tags-sitemap.xml</loc></sitemap>
</sitemapindex>"#;

    const ALTERNATES: &[u8] = br#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
        xmlns:xhtml="http://www.w3.org/1999/xhtml">
  <url>
    <loc>https://example.com/en/page</loc>
    <xhtml:link rel="alternate" hreflang="fr" href="https://example.com/fr/page"/>
    <xhtml:link rel="alternate" hreflang="de" href="https://example.com/de/page"/>
  </url>
</urlset>"#;

    fn response(url: &str, body: &[u8], content_type: Option<&str>) -> CrawlResponse {
        let mut fetched = FetchedResponse::new(200, url, body.to_vec());
        if let Some(content_type) = content_type {
            fetched
                .headers
                .insert("content-type".to_owned(), content_type.to_owned());
        }
        fetched.attach_request(
            &Request::new(url)
                .with_sid("default")
                .with_callback(PARSE_SITEMAP),
        );
        CrawlResponse::from_fetched(&fetched, &Default::default(), None)
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).expect("write");
        encoder.finish().expect("finish")
    }

    fn posts_rule() -> CrawlRule {
        CrawlRule::new(
            LinkExtractor::builder()
                .allow([r"/posts/"])
                .build()
                .expect("compiles"),
        )
        .callback(POST)
    }

    fn urls_of(yielded: &[Yielded]) -> Vec<String> {
        yielded
            .iter()
            .filter_map(|item| match item {
                Yielded::Request(request) => Some(request.url.clone()),
                _ => None,
            })
            .collect()
    }

    fn callbacks_of(yielded: &[Yielded]) -> Vec<Option<String>> {
        yielded
            .iter()
            .filter_map(|item| match item {
                Yielded::Request(request) => Some(
                    request
                        .callback
                        .as_ref()
                        .map(|callback| callback.as_str().to_owned()),
                ),
                _ => None,
            })
            .collect()
    }

    // -- body parsing ------------------------------------------------------

    #[test]
    fn a_urlset_yields_its_locs() {
        let result = parse_sitemap(URLSET, None, false).expect("well-formed");
        assert_eq!(
            result.urls,
            [
                "https://example.com/posts/1",
                "https://example.com/posts/2",
                "https://example.com/about",
            ]
        );
        assert!(result.sitemaps.is_empty());
    }

    #[test]
    fn a_sitemapindex_yields_the_first_loc_of_each_child() {
        let result = parse_sitemap(INDEX, None, false).expect("well-formed");
        assert_eq!(result.sitemaps.len(), 3);
        assert_eq!(result.sitemaps[0], "https://example.com/posts-sitemap.xml");
        assert!(result.urls.is_empty());
    }

    #[test]
    fn alternates_are_interleaved_only_when_enabled() {
        assert_eq!(
            parse_sitemap(ALTERNATES, None, false).expect("ok").urls,
            ["https://example.com/en/page"]
        );
        assert_eq!(
            parse_sitemap(ALTERNATES, None, true).expect("ok").urls,
            [
                "https://example.com/en/page",
                "https://example.com/fr/page",
                "https://example.com/de/page",
            ]
        );
    }

    #[test]
    fn locs_are_trimmed_and_empty_ones_skipped() {
        let body = br#"<urlset><url><loc>
             https://a.test/x
        </loc><loc></loc></url></urlset>"#;
        assert_eq!(
            parse_sitemap(body, None, false).expect("ok").urls,
            ["https://a.test/x"]
        );
    }

    #[test]
    fn an_unknown_root_is_an_empty_result() {
        let result = parse_sitemap(b"<html><body/></html>", None, false).expect("parses");
        assert_eq!(result, SitemapResult::default());
    }

    #[test]
    fn only_direct_children_count() {
        // A `<loc>` nested two levels deep is not a sitemap entry.
        let body = br#"<urlset><wrapper><url><loc>https://a.test/x</loc></url></wrapper></urlset>"#;
        assert!(parse_sitemap(body, None, false)
            .expect("ok")
            .urls
            .is_empty());
    }

    #[test]
    fn a_gzipped_body_decompresses_by_magic_bytes_alone() {
        let result = parse_sitemap(&gzip(URLSET), None, false).expect("ok");
        assert_eq!(result.urls.len(), 3);
    }

    #[test]
    fn malformed_xml_is_a_typed_error() {
        assert!(matches!(
            parse_sitemap(b"<urlset><url></urlset>", None, false).expect_err("bad"),
            TemplateError::Xml { .. }
        ));
    }

    #[test]
    fn robots_sitemaps_reads_the_directives() {
        let body = b"User-agent: *\nDisallow: /private\nSitemap: https://example.com/sitemap.xml\n";
        assert_eq!(
            robots_sitemaps(body, "utf-8"),
            ["https://example.com/sitemap.xml"]
        );
        assert!(robots_sitemaps(b"User-agent: *\n", "utf-8").is_empty());
        assert!(robots_sitemaps(body, "no-such-codec").is_empty());
    }

    // -- the spider --------------------------------------------------------

    #[tokio::test]
    async fn start_requests_names_the_sitemap_callback() {
        let spider = Arc::new(
            SitemapSpider::builder("sm")
                .sitemap_urls(["https://example.com/sitemap.xml"])
                .build(),
        );
        let requests: Vec<Request> = spider
            .start_requests()
            .await
            .expect("seeded")
            .collect()
            .await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].callback.as_ref().map(CallbackId::as_str),
            Some("_parse_sitemap")
        );
    }

    #[tokio::test]
    async fn no_sitemap_urls_is_pythons_runtime_error() {
        let spider = Arc::new(SitemapSpider::builder("sm").build());
        let Err(error) = spider.start_requests().await else {
            panic!("a spider with no `sitemap_urls` must refuse to start");
        };
        assert_eq!(
            error.to_string(),
            "`SitemapSpider` needs `sitemap_urls` to be set."
        );
    }

    #[test]
    fn rules_dispatch_first_match_and_drop_the_rest() {
        let spider = SitemapSpider::builder("sm").rule(posts_rule()).build();
        let response = response("https://example.com/sitemap.xml", URLSET, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert_eq!(
            urls_of(&yielded),
            ["https://example.com/posts/1", "https://example.com/posts/2"],
            "`/about` matches no rule and is dropped"
        );
        assert_eq!(
            callbacks_of(&yielded),
            [Some("parse_post".to_owned()), Some("parse_post".to_owned())]
        );
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let second = CrawlRule::new(
            LinkExtractor::builder()
                .allow([r"/posts/1"])
                .build()
                .expect("compiles"),
        )
        .callback(CallbackId::from_static("never"));
        let spider = SitemapSpider::builder("sm")
            .rule(posts_rule())
            .rule(second)
            .build();
        let response = response("https://example.com/sitemap.xml", URLSET, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert_eq!(yielded.len(), 2, "no URL is dispatched twice");
        assert!(callbacks_of(&yielded)
            .iter()
            .all(|name| name.as_deref() == Some("parse_post")));
    }

    #[test]
    fn with_no_rules_the_default_routes_to_parse() {
        let spider = SitemapSpider::builder("sm").build();
        let response = response("https://example.com/sitemap.xml", URLSET, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert_eq!(urls_of(&yielded).len(), 3);
        assert!(
            callbacks_of(&yielded)
                .iter()
                .all(|name| name.as_deref() == Some("parse")),
            "the declared divergence: the docstring's behaviour, not the code's"
        );
    }

    /// The wart, available on request and asserted to still be the wart.
    #[test]
    fn inherit_reproduces_pythons_sitemap_callback_leak() {
        let spider = SitemapSpider::builder("sm")
            .unruled_route(UnruledRoute::InheritSitemapCallback)
            .build();
        let response = response("https://example.com/sitemap.xml", URLSET, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert!(
            callbacks_of(&yielded)
                .iter()
                .all(|name| name.as_deref() == Some("_parse_sitemap")),
            "every target page would be fed back into the sitemap parser"
        );
    }

    #[test]
    fn a_rule_with_no_callback_routes_to_parse_even_under_inherit() {
        // The `unruled_route` switch governs only the *no rules at all* branch;
        // a rule that matched always names its callback.
        let spider = SitemapSpider::builder("sm")
            .unruled_route(UnruledRoute::InheritSitemapCallback)
            .rule(CrawlRule::new(LinkExtractor::default()))
            .build();
        let response = response("https://example.com/sitemap.xml", URLSET, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert!(callbacks_of(&yielded)
            .iter()
            .all(|name| name.as_deref() == Some("parse")));
    }

    #[test]
    fn an_index_yields_recursive_sitemap_requests() {
        let spider = SitemapSpider::builder("sm").build();
        let response = response("https://example.com/sitemap.xml", INDEX, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert_eq!(urls_of(&yielded).len(), 3);
        assert!(callbacks_of(&yielded)
            .iter()
            .all(|name| name.as_deref() == Some("_parse_sitemap")));
    }

    #[test]
    fn sitemap_follow_narrows_the_index() {
        let spider = SitemapSpider::builder("sm")
            .sitemap_follow(
                LinkExtractor::builder()
                    .allow([r"posts-sitemap"])
                    .build()
                    .expect("compiles"),
            )
            .build();
        let response = response("https://example.com/sitemap.xml", INDEX, None);
        let yielded = spider.parse_sitemap_yields(&response);
        assert_eq!(urls_of(&yielded), ["https://example.com/posts-sitemap.xml"]);
    }

    #[test]
    fn a_robots_body_yields_its_sitemap_directives() {
        let spider = SitemapSpider::builder("sm").build();
        let body = b"User-agent: *\nSitemap: https://example.com/sitemap.xml\n";
        let response = response("https://example.com/robots.txt", body, Some("text/plain"));
        let yielded = spider.parse_sitemap_yields(&response);
        assert_eq!(urls_of(&yielded), ["https://example.com/sitemap.xml"]);
        assert_eq!(callbacks_of(&yielded), [Some("_parse_sitemap".to_owned())]);
    }

    #[test]
    fn a_robots_body_with_no_directives_yields_nothing() {
        let spider = SitemapSpider::builder("sm").build();
        let response = response(
            "https://example.com/robots.txt",
            b"User-agent: *\nDisallow: /x\n",
            Some("text/plain"),
        );
        assert!(spider.parse_sitemap_yields(&response).is_empty());
    }

    #[test]
    fn a_gzipped_urlset_reaches_the_rules() {
        let spider = SitemapSpider::builder("sm").rule(posts_rule()).build();
        let response = response("https://example.com/sitemap.xml.gz", &gzip(URLSET), None);
        assert_eq!(spider.parse_sitemap_yields(&response).len(), 2);
    }

    #[test]
    fn malformed_xml_warns_and_yields_nothing() {
        let spider = SitemapSpider::builder("sm").build();
        let response = response("https://example.com/sitemap.xml", b"<urlset><url>", None);
        assert!(spider.parse_sitemap_yields(&response).is_empty());
    }

    #[test]
    fn a_truncated_gzip_body_escapes_the_warn_and_skip_path() {
        let spider = SitemapSpider::builder("sm").build();
        let full = gzip(URLSET);
        let response = response(
            "https://example.com/sitemap.xml.gz",
            &full[..full.len() - 6],
            None,
        );
        let yielded = spider.parse_sitemap_yields(&response);
        assert!(
            matches!(yielded.as_slice(), [Yielded::Error(_)]),
            "{yielded:?}"
        );
    }

    #[tokio::test]
    async fn parse_is_not_implemented_and_says_so() {
        let spider = Arc::new(SitemapSpider::builder("sm").build());
        let response = response("https://example.com/x", b"<html/>", None);
        let yielded: Vec<Yielded> = spider.parse(response).await.collect().await;
        match &yielded[0] {
            Yielded::Error(error) => {
                assert_eq!(error.to_string(), "sm must implement parse() method")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_routes_the_sitemap_callback_and_the_named_ones() {
        let spider = Arc::new(
            SitemapSpider::builder("sm")
                .rule(posts_rule())
                .callback(POST, |_response| {
                    vec![Yielded::item(browser_spider::request::Item::new())]
                })
                .build(),
        );
        let sitemap = response("https://example.com/sitemap.xml", URLSET, None);
        let yielded: Vec<Yielded> = Arc::clone(&spider)
            .dispatch(PARSE_SITEMAP, sitemap)
            .await
            .collect()
            .await;
        assert_eq!(yielded.len(), 2);

        let page = response("https://example.com/posts/1", b"<html/>", None);
        let yielded: Vec<Yielded> = spider.dispatch(POST, page).await.collect().await;
        // An empty item is dropped by the engine, but it *was* dispatched.
        assert_eq!(yielded.len(), 1);
    }

    #[test]
    fn has_callback_covers_parse_the_sitemap_callback_and_the_rules() {
        let spider = SitemapSpider::builder("sm")
            .callback(POST, |_| Vec::new())
            .build();
        assert!(spider.has_callback(&CallbackId::PARSE));
        assert!(spider.has_callback(&PARSE_SITEMAP));
        assert!(spider.has_callback(&POST));
        assert!(!spider.has_callback(&CallbackId::from_static("nope")));
    }
}
