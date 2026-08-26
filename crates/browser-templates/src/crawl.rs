//! [`CrawlRule`] / [`CrawlSpider`], with rule dispatch exposed as a free
//! function so user spiders can compose it.
//!
//! A port of `reference/spiders/templates/crawler.py` (`spider-support.md` §8).
//!
//! # Why the dispatch step is a free function
//!
//! Python's `CrawlSpider.parse` is a method, so a user who wants rules *and*
//! their own `parse` has to remember to `async for req in super().parse(...)`.
//! `spider-support.md` §14.1 asks for the composition to be explicit instead,
//! and `crates/browser-rs/tests/integration.rs::crawl_spider_seam` writes it
//! that way against the real engine. [`requests_to_follow`] is that function:
//! a plain `fn(&[CrawlRule], &CrawlResponse) -> Result<Vec<Request>>` that any
//! [`Spider`] can call from any callback.
//!
//! [`CrawlSpider`] is then a ready-made [`Spider`] built on top of it, for the
//! common case where the rules *are* the spider.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use browser_spider::request::CallbackId;
//! use browser_spider::spider::{RunOptions, Spider, SpiderRunner, SpiderSettings, Yielded};
//! use browser_templates::crawl::{CrawlRule, CrawlSpider};
//! use browser_templates::links::LinkExtractor;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! const PRODUCT: CallbackId = CallbackId::from_static("parse_product");
//!
//! let spider = CrawlSpider::builder("shop")
//!     .settings(SpiderSettings::default().with_start_urls(["https://shop.test/"]))
//!     .rule(CrawlRule::new(
//!         LinkExtractor::builder().allow([r"/p/\d+$"]).build()?,
//!     ).callback(PRODUCT))
//!     .callback(PRODUCT, |response| {
//!         let mut item = browser_spider::request::Item::new();
//!         item.insert("url".to_owned(), response.url.clone().into());
//!         vec![Yielded::item(item)]
//!     })
//!     .build();
//!
//! let result = SpiderRunner::new(Arc::new(spider) as Arc<dyn Spider>, RunOptions::default())?
//!     .run()
//!     .await?;
//! # let _ = result;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use browser_spider::request::{CallbackId, Request};
use browser_spider::spider::{
    CallbackStream, CrawlResponse, FollowOptions, Spider, SpiderSettings, Yielded,
};

use crate::error::Result;
use crate::links::LinkExtractor;

/// `process_request=` — `(request, response) -> request`.
///
/// Python's may mutate the request or return a wholly different one; both work
/// here because the request is passed by value. Returning `None` drops it,
/// which Python cannot express (its type is `-> Request`) but which costs
/// nothing to allow and is what Scrapy's identically-named hook does.
pub type ProcessRequestFn = Arc<dyn Fn(Request, &CrawlResponse) -> Option<Request> + Send + Sync>;

/// A user callback, resolved by name.
///
/// Python binds a method (`Rule(callback=self.parse_post)`); a checkpoint has
/// to survive the round trip, so the port names callbacks instead
/// (`ARCHITECTURE.md` §1.11) and [`CrawlSpider`] holds the name-to-closure
/// table.
pub type CallbackFn = Arc<dyn Fn(&CrawlResponse) -> Vec<Yielded> + Send + Sync>;

// ---------------------------------------------------------------------------
// CrawlRule
// ---------------------------------------------------------------------------

/// One `Rule(LinkExtractor(...), callback=..., priority=..., process_request=...)`.
///
/// ```
/// use browser_spider::request::CallbackId;
/// use browser_templates::crawl::CrawlRule;
/// use browser_templates::links::LinkExtractor;
///
/// let rule = CrawlRule::new(LinkExtractor::default())
///     .callback(CallbackId::from_static("parse_post"))
///     .priority(99);
///
/// assert_eq!(rule.callback.as_ref().map(CallbackId::as_str), Some("parse_post"));
/// assert_eq!(rule.priority, Some(99));
/// ```
#[derive(Clone)]
pub struct CrawlRule {
    /// Produces the URLs this rule dispatches.
    pub link_extractor: LinkExtractor,
    /// Which callback the produced requests name.
    ///
    /// `None` means [`CallbackId::PARSE`] — see [`requests_to_follow`] for why
    /// that is spelled out rather than inherited.
    pub callback: Option<CallbackId>,
    /// Overrides the dispatched request's priority when set.
    pub priority: Option<i32>,
    /// Runs after `priority` is applied, so it can override it.
    pub process_request: Option<ProcessRequestFn>,
}

impl std::fmt::Debug for CrawlRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrawlRule")
            .field("link_extractor", &self.link_extractor)
            .field("callback", &self.callback.as_ref().map(CallbackId::as_str))
            .field("priority", &self.priority)
            .field("process_request", &self.process_request.is_some())
            .finish()
    }
}

impl CrawlRule {
    /// A rule that follows every URL `link_extractor` produces, into `parse`.
    #[must_use]
    pub fn new(link_extractor: LinkExtractor) -> Self {
        Self {
            link_extractor,
            callback: None,
            priority: None,
            process_request: None,
        }
    }

    /// `callback=` — the named callback the produced requests carry.
    #[must_use]
    pub fn callback(mut self, callback: CallbackId) -> Self {
        self.callback = Some(callback);
        self
    }

    /// `priority=`.
    #[must_use]
    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = Some(priority);
        self
    }

    /// `process_request=`.
    #[must_use]
    pub fn process_request<F>(mut self, process: F) -> Self
    where
        F: Fn(Request, &CrawlResponse) -> Option<Request> + Send + Sync + 'static,
    {
        self.process_request = Some(Arc::new(process));
        self
    }

    /// The callback a produced request must carry.
    ///
    /// **Never `None`.** See [`requests_to_follow`].
    #[must_use]
    pub fn effective_callback(&self) -> CallbackId {
        self.callback.clone().unwrap_or(CallbackId::PARSE)
    }
}

// ---------------------------------------------------------------------------
// The free function
// ---------------------------------------------------------------------------

/// `CrawlSpider.parse`, as a function over rules.
///
/// Every rule is applied to every response — this is **not** first-match-wins,
/// unlike [`crate::sitemap::SitemapSpider`]'s dispatch. A URL two rules both
/// match is yielded twice, and the engine's scheduler dedupes it by
/// fingerprint. Priority is applied *before* `process_request`, so
/// `process_request` can override it.
///
/// # The one deliberate deviation
///
/// Python writes `response.follow(url, callback=rule.callback)`, and
/// `Response.follow` *inherits* the originating request's callback when the
/// argument is `None`. For a textbook `CrawlSpider` the two spellings agree,
/// because `parse` only ever runs on requests whose callback is already `None`.
/// They stop agreeing the moment a user composes this function into a **named**
/// callback — which is the whole reason it is a free function — and then
/// inheritance silently routes every page reached from a product page back into
/// the product parser.
///
/// So the rule's callback is always set explicitly, to
/// [`CrawlRule::effective_callback`]. `Rule(callback=None)` means "use `parse`",
/// which is what the reference implementation's own docstring says it means. `docs/DIVERGENCES.md`
/// §"Wave 5 integration" carries the same note.
///
/// # Errors
///
/// [`TemplateError::Crawl`] wrapping `CrawlError::NoRequestOnResponse` when
/// `response` was built by hand and has no originating request to inherit `sid`
/// and the session kwargs from.
pub fn requests_to_follow(rules: &[CrawlRule], response: &CrawlResponse) -> Result<Vec<Request>> {
    let mut out = Vec::new();
    for rule in rules {
        for url in rule.link_extractor.extract(response) {
            let options = FollowOptions {
                callback: Some(rule.effective_callback()),
                priority: rule.priority,
                ..FollowOptions::default()
            };
            let request = response.follow_with(&url, &options)?;
            let request = match &rule.process_request {
                Some(process) => process(request, response),
                None => Some(request),
            };
            if let Some(request) = request {
                out.push(request);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CrawlSpider
// ---------------------------------------------------------------------------

/// A generic [`Spider`] that extracts and follows links from a rule table.
///
/// Python's `CrawlSpider` is a base class you subclass and override `rules()`
/// on. Rust has no inheritance, so the port is a **value**: a name, settings, a
/// rule list and a table of named callbacks. Anything more bespoke is written
/// as a plain `impl Spider` that calls [`requests_to_follow`] — which is the
/// shape `crawl_spider_seam` demonstrates and the shape
/// [`crate::site_to_markdown::SiteToMarkdownSpider`] uses.
///
/// The rule list is fixed at build time. Python's `rules()` is re-evaluated per
/// response and so may be stateful; a spider that needs that writes its own
/// `parse` and calls [`requests_to_follow`] with whatever list it computes.
pub struct CrawlSpider {
    name: String,
    settings: SpiderSettings,
    rules: Vec<CrawlRule>,
    callbacks: HashMap<String, CallbackFn>,
    parse_hook: Option<CallbackFn>,
}

impl std::fmt::Debug for CrawlSpider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.callbacks.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("CrawlSpider")
            .field("name", &self.name)
            .field("rules", &self.rules.len())
            .field("callbacks", &names)
            .field("parse_hook", &self.parse_hook.is_some())
            .finish()
    }
}

impl CrawlSpider {
    /// Start building one.
    #[must_use]
    pub fn builder(name: impl Into<String>) -> CrawlSpiderBuilder {
        CrawlSpiderBuilder::new(name)
    }

    /// The rule table.
    #[must_use]
    pub fn rules(&self) -> &[CrawlRule] {
        &self.rules
    }

    /// The requests this spider's rules produce for `response`.
    ///
    /// # Errors
    ///
    /// As [`requests_to_follow`].
    pub fn follow_rules(&self, response: &CrawlResponse) -> Result<Vec<Request>> {
        requests_to_follow(&self.rules, response)
    }

    /// What `parse` yields: the `parse_hook`'s items, then the rule requests.
    fn parse_yields(&self, response: &CrawlResponse) -> Vec<Yielded> {
        let mut yielded = match &self.parse_hook {
            Some(hook) => hook(response),
            None => Vec::new(),
        };
        match self.follow_rules(response) {
            Ok(requests) => yielded.extend(requests.into_iter().map(Yielded::request)),
            // Python would raise `TypeError("This response has no request set
            // yet.")` out of the callback; the engine catches it, calls
            // `on_error` and continues. `Yielded::error` is that same path.
            Err(error) => yielded.push(Yielded::error(error)),
        }
        yielded
    }
}

#[async_trait]
impl Spider for CrawlSpider {
    fn name(&self) -> &str {
        &self.name
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
        let yielded = self.parse_yields(&response);
        Box::pin(futures::stream::iter(yielded))
    }

    async fn dispatch(
        self: Arc<Self>,
        callback: CallbackId,
        response: CrawlResponse,
    ) -> CallbackStream {
        if let Some(handler) = self.callbacks.get(callback.as_str()) {
            let yielded = handler(&response);
            return Box::pin(futures::stream::iter(yielded));
        }
        self.parse(response).await
    }

    /// Overridden alongside [`Spider::dispatch`], as the trait's own docs
    /// require: without it a restored checkpoint rewrites every named callback
    /// to `parse`.
    fn has_callback(&self, callback: &CallbackId) -> bool {
        callback.as_str() == CallbackId::PARSE.as_str()
            || self.callbacks.contains_key(callback.as_str())
    }
}

/// Builder for [`CrawlSpider`].
#[derive(Default)]
pub struct CrawlSpiderBuilder {
    name: String,
    settings: SpiderSettings,
    rules: Vec<CrawlRule>,
    callbacks: HashMap<String, CallbackFn>,
    parse_hook: Option<CallbackFn>,
}

impl std::fmt::Debug for CrawlSpiderBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrawlSpiderBuilder")
            .field("name", &self.name)
            .field("rules", &self.rules.len())
            .field("callbacks", &self.callbacks.len())
            .finish()
    }
}

impl CrawlSpiderBuilder {
    /// A builder for a spider called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            settings: SpiderSettings::default(),
            rules: Vec::new(),
            callbacks: HashMap::new(),
            parse_hook: None,
        }
    }

    /// Every `Spider` class attribute.
    #[must_use]
    pub fn settings(mut self, settings: SpiderSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Append one rule. Rules apply in the order they are added.
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
    ///
    /// Registering [`CallbackId::PARSE`] here is *not* how you extend `parse` —
    /// use [`CrawlSpiderBuilder::on_parse`], which runs alongside rule dispatch
    /// rather than replacing it.
    #[must_use]
    pub fn callback<F>(mut self, id: CallbackId, handler: F) -> Self
    where
        F: Fn(&CrawlResponse) -> Vec<Yielded> + Send + Sync + 'static,
    {
        self.callbacks
            .insert(id.as_str().to_owned(), Arc::new(handler));
        self
    }

    /// Yield extra items from `parse`, before its rule requests.
    ///
    /// This is Python's "override `parse` and delegate with
    /// `async for req in super().parse(response)`" pattern, which
    /// `spider-support.md` §8.2 pins and
    /// [`crate::site_to_markdown::SiteToMarkdownSpider`] is built on.
    #[must_use]
    pub fn on_parse<F>(mut self, handler: F) -> Self
    where
        F: Fn(&CrawlResponse) -> Vec<Yielded> + Send + Sync + 'static,
    {
        self.parse_hook = Some(Arc::new(handler));
        self
    }

    /// Freeze it.
    #[must_use]
    pub fn build(self) -> CrawlSpider {
        CrawlSpider {
            name: self.name,
            settings: self.settings,
            rules: self.rules,
            callbacks: self.callbacks,
            parse_hook: self.parse_hook,
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use browser_spider::request::Item;
    use browser_spider::session::FetchedResponse;

    use super::*;

    const POST: CallbackId = CallbackId::from_static("parse_post");

    const PAGE: &str = r#"<!doctype html><html><body>
        <a href="/posts/1">one</a>
        <a href="/posts/2">two</a>
        <a href="/about">about</a>
        </body></html>"#;

    fn response() -> CrawlResponse {
        let mut fetched = FetchedResponse::new(200, "https://example.com/", PAGE);
        fetched.attach_request(&Request::new("https://example.com/").with_sid("default"));
        CrawlResponse::from_fetched(&fetched, &Default::default(), None)
    }

    fn detached_response() -> CrawlResponse {
        let fetched = FetchedResponse::new(200, "https://example.com/", PAGE);
        CrawlResponse::from_fetched(&fetched, &Default::default(), None)
    }

    fn posts_rule() -> CrawlRule {
        CrawlRule::new(
            LinkExtractor::builder()
                .allow([r"/posts/"])
                .build()
                .expect("compiles"),
        )
    }

    // -- requests_to_follow -------------------------------------------------

    #[test]
    fn no_rules_yields_nothing() {
        assert!(requests_to_follow(&[], &response())
            .expect("a crawled response")
            .is_empty());
    }

    #[test]
    fn one_rule_yields_its_matches_in_document_order() {
        let requests = requests_to_follow(&[posts_rule()], &response()).expect("crawled");
        let urls: Vec<&str> = requests.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://example.com/posts/1", "https://example.com/posts/2"]
        );
    }

    #[test]
    fn every_rule_is_applied_to_every_response() {
        let about = CrawlRule::new(
            LinkExtractor::builder()
                .allow([r"/about"])
                .build()
                .expect("compiles"),
        );
        let requests = requests_to_follow(&[posts_rule(), about], &response()).expect("crawled");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].url, "https://example.com/about");
    }

    #[test]
    fn a_url_two_rules_match_is_yielded_twice() {
        let requests =
            requests_to_follow(&[posts_rule(), posts_rule()], &response()).expect("crawled");
        assert_eq!(requests.len(), 4, "the scheduler, not the rules, dedupes");
    }

    /// The deviation `docs/DIVERGENCES.md` §"Wave 5 integration" names: a rule
    /// with no callback routes to `parse`, and never inherits.
    #[test]
    fn a_rule_always_names_its_callback() {
        let with_name =
            requests_to_follow(&[posts_rule().callback(POST)], &response()).expect("crawled");
        assert_eq!(
            with_name[0].callback.as_ref().map(CallbackId::as_str),
            Some("parse_post")
        );

        let without = requests_to_follow(&[posts_rule()], &response()).expect("crawled");
        assert_eq!(
            without[0].callback.as_ref().map(CallbackId::as_str),
            Some("parse"),
            "`None` means `parse`, not `whatever the parent used`"
        );
    }

    #[test]
    fn a_follow_only_rule_does_not_inherit_a_named_parent_callback() {
        // The response's own request names a callback, which is exactly the
        // situation Python's inheritance gets wrong.
        let mut fetched = FetchedResponse::new(200, "https://example.com/", PAGE);
        fetched.attach_request(
            &Request::new("https://example.com/")
                .with_sid("default")
                .with_callback(POST),
        );
        let response = CrawlResponse::from_fetched(&fetched, &Default::default(), None);

        let requests = requests_to_follow(&[posts_rule()], &response).expect("crawled");
        assert_eq!(
            requests[0].callback.as_ref().map(CallbackId::as_str),
            Some("parse"),
            "a follow-only rule must not turn every linked page into a post"
        );
    }

    #[test]
    fn priority_is_applied_before_process_request() {
        let rule = posts_rule().priority(99);
        let requests = requests_to_follow(&[rule], &response()).expect("crawled");
        assert_eq!(requests[0].priority, 99);

        let overriding = posts_rule().priority(99).process_request(|mut request, _| {
            request.priority = -1;
            Some(request)
        });
        let requests = requests_to_follow(&[overriding], &response()).expect("crawled");
        assert_eq!(requests[0].priority, -1);
    }

    #[test]
    fn process_request_may_replace_or_drop() {
        let replacing =
            posts_rule().process_request(|_, _| Some(Request::new("https://elsewhere.test/")));
        let requests = requests_to_follow(&[replacing], &response()).expect("crawled");
        assert!(requests.iter().all(|r| r.url == "https://elsewhere.test/"));

        let dropping = posts_rule().process_request(|_, _| None);
        assert!(requests_to_follow(&[dropping], &response())
            .expect("crawled")
            .is_empty());
    }

    #[test]
    fn follow_sets_the_referer_header() {
        let requests = requests_to_follow(&[posts_rule()], &response()).expect("crawled");
        let headers = requests[0]
            .session_kwargs
            .headers
            .as_ref()
            .expect("referer_flow set them");
        assert_eq!(
            headers.get("referer").map(String::as_str),
            Some("https://example.com/")
        );
    }

    #[test]
    fn a_response_with_no_request_is_an_error_not_a_panic() {
        let error =
            requests_to_follow(&[posts_rule()], &detached_response()).expect_err("no request");
        assert!(
            matches!(error, crate::error::TemplateError::Crawl(_)),
            "{error:?}"
        );
    }

    // -- CrawlSpider --------------------------------------------------------

    fn spider() -> Arc<CrawlSpider> {
        Arc::new(
            CrawlSpider::builder("shop")
                .settings(SpiderSettings::default().with_start_urls(["https://example.com/"]))
                .rule(posts_rule().callback(POST))
                .callback(POST, |response| {
                    let mut item = Item::new();
                    item.insert("url".to_owned(), response.url.clone().into());
                    vec![Yielded::item(item)]
                })
                .build(),
        )
    }

    #[tokio::test]
    async fn parse_yields_the_rule_requests() {
        let yielded: Vec<Yielded> = spider().parse(response()).await.collect().await;
        assert_eq!(yielded.len(), 2);
        assert!(yielded
            .iter()
            .all(|item| matches!(item, Yielded::Request(_))));
    }

    #[tokio::test]
    async fn dispatch_routes_a_named_callback_and_falls_back_to_parse() {
        let yielded: Vec<Yielded> = spider().dispatch(POST, response()).await.collect().await;
        assert_eq!(yielded.len(), 1);
        assert!(matches!(yielded[0], Yielded::Item(_)));

        let unknown: Vec<Yielded> = spider()
            .dispatch(CallbackId::from_static("nope"), response())
            .await
            .collect()
            .await;
        assert_eq!(unknown.len(), 2, "an unknown name falls through to parse");
    }

    #[test]
    fn has_callback_covers_parse_and_every_registered_name() {
        let spider = spider();
        assert!(spider.has_callback(&CallbackId::PARSE));
        assert!(spider.has_callback(&POST));
        assert!(!spider.has_callback(&CallbackId::from_static("nope")));
    }

    #[tokio::test]
    async fn on_parse_items_come_before_the_rule_requests() {
        let spider = Arc::new(
            CrawlSpider::builder("shop")
                .rule(posts_rule())
                .on_parse(|response| {
                    let mut item = Item::new();
                    item.insert("title".to_owned(), response.url.clone().into());
                    vec![Yielded::item(item)]
                })
                .build(),
        );
        let yielded: Vec<Yielded> = spider.parse(response()).await.collect().await;
        assert_eq!(yielded.len(), 3);
        assert!(matches!(yielded[0], Yielded::Item(_)));
        assert!(matches!(yielded[1], Yielded::Request(_)));
        assert!(matches!(yielded[2], Yielded::Request(_)));
    }

    #[tokio::test]
    async fn a_detached_response_yields_an_error_rather_than_failing_the_crawl() {
        let yielded: Vec<Yielded> = spider().parse(detached_response()).await.collect().await;
        assert_eq!(yielded.len(), 1);
        assert!(matches!(yielded[0], Yielded::Error(_)), "{yielded:?}");
    }

    /// The seam `crawl_spider_seam` states: an extractor takes a `&Response`
    /// and a callback is handed a `CrawlResponse`, and the two meet through
    /// `Deref` with no conversion in between.
    #[test]
    fn an_extractor_takes_a_crawl_response_by_deref_coercion() {
        let response = response();
        let extractor = LinkExtractor::default();
        assert_eq!(
            extractor.extract(&response).len(),
            3,
            "`&CrawlResponse` coerced to `&Response` at the call site"
        );
    }

    #[test]
    fn the_rule_table_is_readable_and_clone_is_cheap() {
        let spider = spider();
        assert_eq!(spider.rules().len(), 1);
        let cloned = spider.rules()[0].clone();
        assert_eq!(
            cloned.callback.as_ref().map(CallbackId::as_str),
            Some("parse_post")
        );
        // Cloning a rule clones its extractor, which must still work.
        assert_eq!(cloned.link_extractor.extract(&response()).len(), 2);
    }

    #[test]
    fn follow_rules_is_the_free_function_under_another_name() {
        let spider = spider();
        let response = response();
        let direct = requests_to_follow(spider.rules(), &response).expect("crawled");
        let method = spider.follow_rules(&response).expect("crawled");
        let urls = |requests: &[Request]| -> Vec<String> {
            requests.iter().map(|request| request.url.clone()).collect()
        };
        assert_eq!(urls(&direct), urls(&method));
    }

    #[test]
    fn debug_lists_the_callbacks_deterministically() {
        let rendered = format!("{:?}", spider());
        assert!(
            rendered.contains(r#"callbacks: ["parse_post"]"#),
            "{rendered}"
        );
    }
}
