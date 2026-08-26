//! Cross-crate **seam** tests for the façade.
//!
//! Wave 2 built units 5 (`browser-selectors`), 6 (`browser-xpath`),
//! 7 (`browser-storage`), 8 (`browser-markdown`), 11 (`browser-http`
//! policy) and 13 (`browser-cdp` config) in parallel, each verified against
//! the Python reference in isolation. Wave 3 builds unit 10
//! (`browser-parser`) on top of 3/4/5/6/7 + `browser-core`'s
//! similarity/fingerprint, and unit 12 (the HTTP driver) on top of unit 11's
//! policy types.
//!
//! These tests exist to prove that those APIs actually *compose* — that one
//! `Document` can be handed to the CSS matcher, to the XPath engine and to the
//! adaptive layer at the same time, and that unit 11's policy types are enough
//! to drive one attempt loop. They are compile-time evidence as much as runtime
//! evidence: if a wave-3 unit cannot be written against these signatures, this
//! file stops building.
//!
//! End-to-end tests against a local `wiremock` server are unit 20's job and
//! still to come.

use std::sync::Arc;
use std::time::Duration;

use browser_core::fingerprint::{similarity_score, AttributeDict, ElementFingerprint};
use browser_dom::{Document, NodeRef};
use browser_selectors::{MatchValue, SelectorGroup, Tail, TextRole};
use browser_storage::{registrable_domain, SqliteStorage, StorageSystem};
use browser_xpath::{DomModel, Value, Variables, XPath};

/// The page whose `<p class="price">` is fingerprinted and stored.
///
/// Minified on purpose: a whitespace-only text node between `</div>` and `<p>`
/// is one of the two places `browser-dom` and libxml2 disagree (see
/// `docs/DIVERGENCES.md`), and this test is about the seam, not about that.
const ORIGINAL: &str = concat!(
    r#"<html><body><div id="wrap" class="container">"#,
    r#"<p class="price" data-sku="A-1">  $9.99  </p>"#,
    r#"<span>note</span></div></body></html>"#,
);

/// The same page after a redesign: the wrapper became a `<main>`, a `<section>`
/// was inserted, and the price element picked up an extra class and a new value.
/// No CSS selector written against `ORIGINAL` still finds it.
const REDESIGNED: &str = concat!(
    r#"<html><body><main class="container" id="wrap"><section>"#,
    r#"<p class="price cost" data-sku="A-1">$10.99</p>"#,
    r#"<span>note</span></section></main></body></html>"#,
);

// ---------------------------------------------------------------------------
// The adaptive fingerprint, built from nothing but `browser-dom`'s API.
//
// This is `_StorageTools.element_to_dict` (parser.md §4.1). It belongs to unit
// 10, and it is reproduced here — deliberately, once — because it is the single
// place where units 3, 4 and 7 have to line up: if `browser-dom` does not
// expose enough to fill every field of `browser_core::ElementFingerprint`,
// unit 10 cannot be written and this file will not compile.
// ---------------------------------------------------------------------------

/// `__clean_attributes`: values stripped, empty and whitespace-only values dropped.
fn clean_attributes(element: NodeRef<'_>) -> AttributeDict {
    element
        .attrs()
        .to_index_map()
        .into_iter()
        .filter_map(|(k, v)| {
            let trimmed = v.trim();
            (!trimmed.is_empty()).then(|| (k, trimmed.to_owned()))
        })
        .collect()
}

/// `element.text.strip() if element.text else None`.
///
/// Note the Python shape: a whitespace-only `.text` is *truthy*, so it becomes
/// `Some("")`, not `None`.
fn stripped_text(element: NodeRef<'_>) -> Option<String> {
    element.text().map(|t| t.trim().to_owned())
}

/// lxml's `iterchildren()`: elements, comments and PIs — never text nodes,
/// because lxml has no text-node objects at all.
fn iter_children(element: NodeRef<'_>) -> impl Iterator<Item = NodeRef<'_>> {
    element.children().filter(|c| !c.is_text())
}

/// `_get_element_path`: root-first tag names, the element's own tag included.
fn element_path(element: NodeRef<'_>) -> Vec<String> {
    let mut path: Vec<String> = element.ancestors().map(|a| a.tag().to_owned()).collect();
    path.reverse();
    path.push(element.tag().to_owned());
    path
}

/// `_StorageTools.element_to_dict`.
fn element_to_fingerprint(element: NodeRef<'_>) -> ElementFingerprint {
    let mut fingerprint = ElementFingerprint {
        tag: element.tag().to_owned(),
        attributes: clean_attributes(element),
        text: stripped_text(element),
        path: element_path(element),
        ..ElementFingerprint::default()
    };

    if let Some(parent) = element.parent_element() {
        fingerprint.parent_name = Some(parent.tag().to_owned());
        // NOT cleaned and NOT stripped — the asymmetry is Python's.
        fingerprint.parent_attribs = Some(parent.attrs().to_index_map());
        fingerprint.parent_text = stripped_text(parent);

        let siblings: Vec<String> = iter_children(parent)
            .filter(|c| *c != element)
            .map(|c| c.tag().to_owned())
            .collect();
        if !siblings.is_empty() {
            fingerprint.siblings = Some(siblings);
        }
    }

    // `children` excludes comments (`html_forbidden`), unlike `siblings`.
    let children: Vec<String> = iter_children(element)
        .filter(|c| !c.is_comment())
        .map(|c| c.tag().to_owned())
        .collect();
    if !children.is_empty() {
        fingerprint.children = Some(children);
    }

    fingerprint
}

/// `Selector.relocate`: score every descendant element, return everyone tied at
/// the best score when it clears `percentage`.
fn relocate<'a>(
    document: &'a Document,
    original: &ElementFingerprint,
    percentage: f64,
) -> Vec<NodeRef<'a>> {
    let mut best = f64::NEG_INFINITY;
    let mut winners: Vec<NodeRef<'a>> = Vec::new();

    for node in document.root().descendant_elements() {
        let score = similarity_score(original, &element_to_fingerprint(node));
        if score > best {
            best = score;
            winners.clear();
        }
        if score == best {
            winners.push(node);
        }
    }

    if best >= percentage {
        winners
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Unit 10's seam: dom + selectors + xpath + storage + core, on one tree.
// ---------------------------------------------------------------------------

/// The whole adaptive round trip, in the order `browser-parser` will do it.
///
/// Parse → CSS-select → fingerprint → save → (page redesign) → retrieve →
/// relocate. Every intermediate value is pinned against `the reference implementation`,
/// captured with the reference interpreter.
#[test]
fn css_xpath_and_adaptive_storage_compose_on_one_tree() {
    let doc = Document::parse(ORIGINAL);

    // --- 1. CSS selection, including the reference implementation's pseudo-elements -----------
    let price = SelectorGroup::parse("div#wrap > p.price").expect("selector parses");
    let matches = price.select(doc.root());
    assert_eq!(matches.len(), 1);
    let element = matches[0].element();
    assert_eq!(element.tag(), "p");

    let text = SelectorGroup::parse("p.price::text").expect("::text parses");
    let text_matches = text.select(doc.root());
    assert_eq!(text_matches.len(), 1);
    assert_eq!(text_matches[0].as_str(), Some("  $9.99  "));
    assert!(matches!(
        text_matches[0].value(),
        MatchValue::Text {
            role: TextRole::Text,
            ..
        }
    ));
    // The `::text` match still knows which *element* owns it — that identity is
    // what `Selector.css(...)` needs to keep the adaptive branch reachable.
    assert_eq!(text_matches[0].element(), element);

    let sku = SelectorGroup::parse("p::attr(data-sku)").expect("::attr parses");
    assert_eq!(sku.select(doc.root())[0].as_str(), Some("A-1"));

    // --- 2. XPath over the *same* tree, using the CSS hit as context ------
    // One `DomModel` is reusable across expressions; the node the CSS matcher
    // returned drops straight into it.
    let model = DomModel::new(&doc);
    let context = model.node(element);

    let siblings = XPath::compile("../*").expect("compiles");
    let selected = siblings
        .select(&model, context, &Variables::new())
        .expect("evaluates");
    let tags: Vec<&str> = selected
        .iter()
        .filter_map(|n| n.as_element().map(|e| e.tag()))
        .collect();
    assert_eq!(tags, ["p", "span"]);

    // A non-node-set result, from the same model, in the same borrow.
    let name = XPath::compile("string(./@class)").expect("compiles");
    match name
        .eval(&model, context, &Variables::new())
        .expect("evals")
    {
        Value::Str(s) => assert_eq!(s, "price"),
        other => panic!("expected a string, got {other:?}"),
    }
    let count = XPath::compile("count(//span)").expect("compiles");
    match count
        .eval(&model, model.root(), &Variables::new())
        .expect("evals")
    {
        Value::Number(n) => assert!((n - 1.0).abs() < f64::EPSILON),
        other => panic!("expected a number, got {other:?}"),
    }

    // The convenience entry point takes a `Document` + `NodeRef` directly.
    let hrefs = browser_xpath::eval::select("//p/@data-sku", &doc, doc.root()).expect("evals");
    assert_eq!(hrefs[0].string_value(), "A-1");

    // --- 3. Fingerprint, pinned against Python ----------------------------
    let fingerprint = element_to_fingerprint(element);
    assert_eq!(fingerprint.tag, "p");
    assert_eq!(
        fingerprint.attributes,
        AttributeDict::from_iter([
            ("class".to_owned(), "price".to_owned()),
            ("data-sku".to_owned(), "A-1".to_owned()),
        ])
    );
    assert_eq!(fingerprint.text.as_deref(), Some("$9.99"));
    assert_eq!(fingerprint.path, ["html", "body", "div", "p"]);
    assert_eq!(fingerprint.parent_name.as_deref(), Some("div"));
    assert_eq!(
        fingerprint.parent_attribs,
        Some(AttributeDict::from_iter([
            ("id".to_owned(), "wrap".to_owned()),
            ("class".to_owned(), "container".to_owned()),
        ]))
    );
    assert_eq!(fingerprint.parent_text, None);
    assert_eq!(
        fingerprint.siblings.as_deref(),
        Some(&["span".to_owned()][..])
    );
    assert_eq!(fingerprint.children, None);

    // The JSON is Python's, key for key — this is the on-disk record.
    assert_eq!(
        browser_storage::python_json::to_string(&fingerprint).expect("serialises"),
        concat!(
            r#"{"tag":"p","attributes":{"class":"price","data-sku":"A-1"},"#,
            r#""text":"$9.99","path":["html","body","div","p"],"#,
            r#""parent_name":"div","parent_attribs":{"id":"wrap","class":"container"},"#,
            r#""parent_text":null,"siblings":["span"]}"#,
        )
    );

    // --- 4. Save and retrieve through the storage trait -------------------
    let storage: Arc<dyn StorageSystem> =
        Arc::new(SqliteStorage::in_memory(Some("https://shop.example.com/p/1")).expect("opens"));
    assert_eq!(storage.base_url(), "example.com");
    assert_eq!(
        registrable_domain(Some("https://shop.example.com/p/1")),
        "example.com"
    );

    assert_eq!(storage.retrieve("div#wrap > p.price").expect("reads"), None);
    storage
        .save(&fingerprint, "div#wrap > p.price")
        .expect("writes");
    let retrieved = storage
        .retrieve("div#wrap > p.price")
        .expect("reads")
        .expect("row is there");
    assert_eq!(retrieved, fingerprint);

    // --- 5. Relocate on the redesigned page -------------------------------
    let redesigned = Document::parse(REDESIGNED);
    assert!(
        SelectorGroup::parse("div#wrap > p.price")
            .expect("selector parses")
            .select(redesigned.root())
            .is_empty(),
        "the original selector must be dead, or the test proves nothing"
    );

    let found = relocate(&redesigned, &retrieved, 40.0);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tag(), "p");
    assert_eq!(found[0].attr("class"), Some("price cost"));
    assert_eq!(found[0].text(), Some("$10.99"));

    // The exact scores Python computes, element for element. `relocate` walks
    // `.//*` from `self._root`, i.e. the root **element**, so `<html>` itself is
    // never a candidate. `round_half_even` on the binary double makes these
    // reproducible.
    //
    // The `<head>` row is ours alone: html5ever synthesises a `<head>` that
    // libxml2 does not build for this markup (the declared parser divergence in
    // `docs/DIVERGENCES.md`). It scores 4.17 — an order of magnitude below the
    // winner and far below any usable threshold — so it cannot change a
    // relocation outcome. Every element Python *does* see scores identically.
    let scores: Vec<(&str, f64)> = redesigned
        .root_element()
        .expect("<html>")
        .descendant_elements()
        .map(|n| {
            (
                n.tag(),
                similarity_score(&retrieved, &element_to_fingerprint(n)),
            )
        })
        .collect();
    assert_eq!(
        scores,
        [
            ("head", 4.17), // not present in libxml2's tree
            ("body", 8.33),
            ("main", 15.62),
            ("section", 16.07),
            ("p", 62.63),
            ("span", 8.06),
        ]
    );

    // Above threshold but not by much: 40 relocates, 70 does not.
    assert!(relocate(&redesigned, &retrieved, 70.0).is_empty());
}

/// `relocate` is specified to run under `rayon`, and the spider shares one
/// parsed page across tasks. Both need `Document: Send + Sync`, and the CSS /
/// XPath handles have to survive being built on another thread.
#[test]
fn the_tree_and_the_compiled_queries_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Document>();
    assert_send_sync::<SelectorGroup>();
    assert_send_sync::<XPath>();
    assert_send_sync::<ElementFingerprint>();
    assert_send_sync::<Arc<dyn StorageSystem>>();

    // And really cross a thread boundary with a live tree.
    let doc = Document::parse(ORIGINAL);
    let selector = SelectorGroup::parse("p.price").expect("parses");
    let xpath = XPath::compile("count(//*)").expect("compiles");
    let tags = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let hit = selector.select_elements(doc.root());
                let model = DomModel::new(&doc);
                let total = xpath
                    .eval(&model, model.root(), &Variables::new())
                    .expect("evals");
                (hit.len(), total)
            })
            .join()
            .expect("thread does not panic")
    });
    assert_eq!(tags.0, 1);
    assert_eq!(tags.1, Value::Number(6.0));
}

/// A `NodeRef` obtained from the CSS matcher, the XPath engine and plain
/// navigation must be the *same* handle, or unit 10 cannot deduplicate results
/// or feed one API's output into the other's input.
#[test]
fn node_identity_is_shared_across_the_three_engines() {
    let doc = Document::parse(ORIGINAL);

    let by_css = SelectorGroup::parse("p.price")
        .expect("parses")
        .select_elements(doc.root())[0];
    let by_xpath = browser_xpath::eval::select("//p", &doc, doc.root()).expect("evals")[0]
        .as_element()
        .expect("an element");
    let by_nav = doc
        .elements()
        .find(|e| e.tag() == "p")
        .expect("the paragraph");

    assert_eq!(by_css, by_xpath);
    assert_eq!(by_css, by_nav);
    assert_eq!(by_css.id(), by_nav.id());

    // Ordering across engines uses the document's own order key.
    let order = doc.document_order();
    let mut nodes = vec![by_nav, doc.root(), by_css];
    order.sort_dedup(&mut nodes);
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0], doc.root());
    assert_eq!(nodes[1], by_css);
}

// ---------------------------------------------------------------------------
// Unit 12's seam: unit 11's policy types have to drive one attempt loop.
// ---------------------------------------------------------------------------

/// The shape of the driver loop unit 12 will write, with the transport stubbed
/// out. Nothing here does I/O; the point is that `FetcherConfig`,
/// `RequestOverrides`, `RetryPolicy`, `proxy_for_attempt` and
/// `merge_request_args` compose into one loop without any missing piece.
#[test]
fn the_http_policy_types_compose_into_one_attempt_loop() {
    use browser_core::types::Proxy;
    use browser_http::config::{FetcherConfig, FollowRedirects, Impersonate, RequestOverrides};
    use browser_http::merge::{merge_request_args, RequestContext};
    use browser_http::proxy::{is_proxy_error, ProxyRotator};
    use browser_http::retry::{failure_message, proxy_for_attempt, RetryDecision, RetryPolicy};

    let rotator = Arc::new(
        ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080"]).expect("valid proxies"),
    );
    let config = FetcherConfig::default()
        .with_impersonate(Impersonate::single("chrome"))
        .with_retries(Some(2))
        .with_retry_delay(Duration::from_millis(5))
        .with_proxy_rotator(Some(rotator));

    let overrides = RequestOverrides::new()
        .header("X-Test", "1")
        .follow_redirects(FollowRedirects::Bool(true));

    let policy = RetryPolicy::resolve(&config, &overrides);
    assert_eq!(policy.attempts, 2);

    // The transport always fails, so the loop runs to exhaustion.
    let transport_error = "Failed to connect to proxy";
    assert!(is_proxy_error(transport_error));

    let mut used_proxies = Vec::new();
    let mut last = None;
    for attempt in 0..policy.attempts {
        let proxy = proxy_for_attempt(&config, &overrides);
        let context = RequestContext::new()
            .with_stealth(true)
            .with_proxy(proxy.clone());
        let merged = merge_request_args("https://example.com/a", &config, &overrides, &context);

        assert_eq!(merged.url, "https://example.com/a");
        assert_eq!(merged.impersonate.as_deref(), Some("chrome"));
        assert_eq!(merged.allow_redirects, FollowRedirects::Bool(true));
        assert!(merged.headers.keys().any(|k| k == "X-Test"));
        assert_eq!(merged.proxy_source, proxy);
        used_proxies.push(merged.proxy.clone().expect("a proxy per attempt"));

        last = Some(policy.decide(attempt));
    }

    // The rotator advanced on every attempt, so the retry did not reuse the
    // proxy that just failed.
    assert_eq!(used_proxies, ["http://p1:8080", "http://p2:8080"]);
    assert_eq!(last, Some(RetryDecision::GiveUp));
    assert_eq!(
        failure_message(policy.attempts, transport_error),
        "Failed after 2 attempts: Failed to connect to proxy"
    );

    // A per-request proxy pins every attempt and never touches the rotator.
    let pinned = RequestOverrides::new().proxy_url("http://pinned:1");
    assert_eq!(
        proxy_for_attempt(&config, &pinned),
        Some(Proxy::Url("http://pinned:1".into()))
    );
}

/// Unit 12 hands `Response.doc` the parser keywords the caller passed to the
/// fetcher; unit 10 turns them into `ParseOptions`. Check the two ends line up.
#[test]
fn the_selector_config_carries_the_parser_keywords_unit_10_needs() {
    use browser_http::config::{FetcherConfig, RequestOverrides, SelectorConfig, PARSER_KEYWORDS};

    let mut requested = SelectorConfig::default();
    requested.insert("keep_comments", true.into());
    requested.insert("huge_tree", true.into());

    let config = FetcherConfig::default().with_selector_config(requested.clone());
    let resolved = browser_http::config::resolve_selector_config(&config, &RequestOverrides::new());
    assert_eq!(resolved.get("keep_comments"), Some(&true.into()));

    // Every keyword the fetchers accept has to be one unit 10 knows what to do
    // with. Three map onto `ParseOptions` directly, four onto the adaptive layer.
    for keyword in PARSER_KEYWORDS {
        assert!(
            matches!(
                keyword,
                // -> ParseOptions
                "huge_tree" | "keep_comments" | "keep_cdata"
                // -> the adaptive layer (unit 7 + browser-core)
                | "adaptive" | "storage" | "storage_args" | "adaptive_domain"
            ),
            "unexpected parser keyword {keyword}: unit 10 must grow a mapping for it"
        );
    }
    // The typed accessors `SelectorConfig` already offers cover the first four.
    assert!(resolved.keep_comments());
    assert!(resolved.huge_tree());
    assert!(!resolved.keep_cdata());
    assert!(!resolved.adaptive());
    assert_eq!(
        SelectorConfig::default()
            .with_adaptive_domain("example.com")
            .adaptive_domain(),
        Some("example.com")
    );

    // The two `browser-dom` actually models, mapped the way unit 10 will.
    let options = browser_dom::ParseOptions::new()
        .keep_comments(true)
        .keep_cdata(false);
    let doc = browser_dom::parse_html_with("<div><!-- keep me --></div>", &options);
    assert!(doc.nodes().any(|n| n.is_comment()));
}

/// The comma-splitting adaptive path (`parser.md` §3.4), end to end.
///
/// Unit 5's verifier reported `SelectorGroup::select_per_part` as the one branch
/// it could not differentially test, because exercising Python's adaptive branch
/// needs unit 7's storage layer. This is that test: one fingerprint saved per
/// sub-selector, keyed by cssselect's `canonical()`, then relocated on a
/// restructured page. It is the exact shape `Selector.css(sel, auto_save=True)`
/// will take in unit 10.
#[test]
fn the_comma_split_adaptive_path_stores_one_fingerprint_per_sub_selector() {
    let doc = Document::parse(ORIGINAL);
    let group = SelectorGroup::parse("div#wrap > p.price, span").expect("parses");

    // The storage identifiers are cssselect's canonical re-serialisation, one
    // per part — verified against `cssselect.parse(...)`.
    assert_eq!(group.canonical_parts(), ["div#wrap > p.price", "span"]);
    assert!(group.is_group());
    // A mixed group has no shared tail; this one is all-elements.
    assert_eq!(group.common_tail(), Some(&Tail::Element));

    let storage = SqliteStorage::in_memory(Some("https://shop.example.com/")).expect("opens");

    // `auto_save`: only the FIRST element of each part is stored — Python's
    // documented "Known issue", which unit 10 has to reproduce.
    let per_part = group.select_per_part(doc.root());
    assert_eq!(per_part.len(), 2);
    for (identifier, matches) in group.canonical_parts().iter().zip(&per_part) {
        let first = matches.first().expect("each part matches here");
        storage
            .save(&element_to_fingerprint(first.element()), identifier)
            .expect("writes");
    }

    // The redesign kills the first part outright; `span` still matches, which
    // is the realistic case — only the dead part needs relocating.
    let redesigned = Document::parse(REDESIGNED);
    let after = group.select_per_part(redesigned.root());
    assert!(after[0].is_empty(), "div#wrap > p.price must be dead");
    assert_eq!(after[1].len(), 1);

    // Relocation, per part, exactly as `xpath()`'s adaptive branch does it.
    let mut relocated = Vec::new();
    for identifier in group.canonical_parts() {
        let stored = storage
            .retrieve(&identifier)
            .expect("reads")
            .expect("saved above");
        let hits = relocate(&redesigned, &stored, 40.0);
        relocated.push((identifier, hits.iter().map(|n| n.tag()).collect::<Vec<_>>()));
    }
    assert_eq!(
        relocated,
        [
            ("div#wrap > p.price".to_owned(), vec!["p"]),
            ("span".to_owned(), vec!["span"]),
        ]
    );

    // A threshold nothing can clear returns an empty vector, not an error and
    // not a panic — the `test_relocation_auto_save_no_match_above_threshold`
    // regression.
    let stored = storage.retrieve("span").expect("reads").expect("saved");
    assert!(relocate(&redesigned, &stored, 95.0).is_empty());

    // An identifier that was never saved is `None`, not an error: the normal
    // first-visit path.
    assert_eq!(storage.retrieve("#never-seen").expect("reads"), None);
}

// ---------------------------------------------------------------------------
// Wave 4 seam: a `Response`-shaped value driven through unit 10's public API.
//
// Units 12 (`browser-http`'s driver) and 14 (the CDP driver) both end in the
// same place — Python's `Response(Selector)`, a bag of HTTP metadata that *is*
// a parsed document. Neither crate exists yet, so this module builds the shape
// they will build and drives it through every part of `browser-parser` they
// are expected to expose: lazy parsing, `css`/`xpath`/`find_all`, and one full
// adaptive save-then-relocate across two fetches sharing one storage backend.
//
// If unit 12 cannot be written against these signatures, this file stops
// compiling — which is the point.
// ---------------------------------------------------------------------------
mod response_seam {
    use std::sync::{Arc, OnceLock};

    use indexmap::IndexMap;
    use regex::Regex;

    use browser_http::config::{RequestOverrides, SelectorConfig};
    use browser_parser::config::{CssOpts, DocumentConfig};
    use browser_parser::document::{Body, Document};
    use browser_parser::error::Result;
    use browser_parser::find::FindQuery;
    use browser_parser::node::Node;
    use browser_parser::nodes::Nodes;
    use browser_storage::{SqliteStorage, StorageSystem};

    /// `ResponseFactory.__CHARSET_RE__`, verbatim.
    ///
    /// Python only applies it to browser responses, because `curl_cffi` already
    /// resolves the charset for the HTTP engines; unit 14 needs it and unit 12
    /// needs the same fallback when a backend does not resolve one.
    fn charset_of(headers: &IndexMap<String, String>, default: &str) -> String {
        let re = Regex::new(r#"charset=["']?([\w-]+)"#).expect("the Python pattern compiles");
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .and_then(|(_, value)| re.captures(value))
            .map(|caps| caps[1].to_owned())
            .unwrap_or_else(|| default.to_owned())
    }

    /// The `Response` unit 12 will put in `browser-http/src/response.rs`.
    ///
    /// The fields are Python's `Response.__init__` parameters; `doc` is the
    /// documented deviation — Python calls `Selector.__init__` eagerly, even on
    /// a binary body, and we defer it to the first selector call.
    struct Response {
        status: u16,
        reason: String,
        /// The URL actually fetched. **Not** necessarily the document's `url`:
        /// `adaptive_domain` replaces it there and only there.
        url: String,
        headers: IndexMap<String, String>,
        request_headers: IndexMap<String, String>,
        history: Vec<String>,
        body: Body,
        config: DocumentConfig,
        doc: OnceLock<Document>,
    }

    impl Response {
        /// What a backend hands over: bytes, headers and the caller's
        /// `parser_keywords`. Nothing is parsed here.
        fn from_wire(
            url: &str,
            status: u16,
            reason: &str,
            headers: IndexMap<String, String>,
            body: Vec<u8>,
            selector_config: &SelectorConfig,
            storage: Option<Arc<dyn StorageSystem>>,
        ) -> Self {
            // The whole of `SelectorConfig` -> `DocumentConfig`. Unit 12 owns
            // this function; that it can be written from the two public APIs is
            // exactly what this test asserts.
            let mut config = DocumentConfig::new()
                .url(url)
                .encoding(charset_of(&headers, "utf-8"))
                .keep_comments(selector_config.keep_comments())
                .keep_cdata(selector_config.keep_cdata())
                .huge_tree(selector_config.huge_tree())
                .adaptive(selector_config.adaptive());
            if let Some(domain) = selector_config.adaptive_domain() {
                config = config.adaptive_domain(domain);
            }
            if let Some(storage) = storage {
                config = config.storage(storage);
            }

            Self {
                status,
                reason: reason.to_owned(),
                url: url.to_owned(),
                headers,
                request_headers: IndexMap::new(),
                history: Vec::new(),
                body: Body::Bytes(body),
                config,
                doc: OnceLock::new(),
            }
        }

        /// Whether the body has been parsed yet. Only this test needs it.
        fn is_parsed(&self) -> bool {
            self.doc.get().is_some()
        }

        /// The lazily parsed document.
        ///
        /// `parse_content` is `Selector.__init__`'s "content or root" check, so
        /// a `Response` holding no body at all — a redirect hop, which Python
        /// constructs with `content=""` — is a representable state rather than
        /// a panic.
        fn doc(&self) -> &Document {
            self.doc.get_or_init(|| {
                Document::parse_content(Some(&self.body), &self.config)
                    .expect("a `Response` always carries a body, even an empty one")
            })
        }

        fn css(&self, selector: &str) -> Result<Nodes<'_>> {
            self.doc().css(selector)
        }

        fn css_with(&self, selector: &str, opts: &CssOpts) -> Result<Nodes<'_>> {
            self.doc().css_with(selector, opts)
        }

        fn xpath(&self, expression: &str) -> Result<Nodes<'_>> {
            self.doc().xpath(expression)
        }

        fn find_all(&self, query: &FindQuery<'_>) -> Result<Nodes<'_>> {
            self.doc().find_all(query)
        }

        fn save(&self, node: &Node<'_>, identifier: &str) -> Result<()> {
            self.doc().save(node, identifier)
        }
    }

    const PAGE_ONE: &str = concat!(
        r#"<!doctype html><html><head><title>Shop</title></head><body>"#,
        r#"<div id="wrap" class="container">"#,
        r#"<p class="price" data-sku="A-1">$9.99</p>"#,
        r#"<a href="/p/1">one</a><a href="/p/2">two</a>"#,
        r#"</div></body></html>"#,
    );

    /// The same shop after a redesign — the wrapper is a `<main>`, a `<section>`
    /// was inserted, the price gained a class. `div#wrap > p.price` is dead.
    const PAGE_TWO: &str = concat!(
        r#"<!doctype html><html><head><title>Shop</title></head><body>"#,
        r#"<main id="wrap" class="container"><section>"#,
        r#"<p class="price cost" data-sku="A-1">$10.99</p>"#,
        r#"<a href="/p/1">one</a><a href="/p/2">two</a>"#,
        r#"</section></main></body></html>"#,
    );

    fn html_headers(charset: &str) -> IndexMap<String, String> {
        let mut headers = IndexMap::new();
        headers.insert(
            "Content-Type".to_owned(),
            format!("text/html; charset={charset}"),
        );
        headers.insert("Server".to_owned(), "nginx".to_owned());
        headers
    }

    /// Raw bytes + headers in, `Nodes` out, with nothing parsed until asked.
    #[test]
    fn a_response_parses_lazily_and_answers_css_xpath_and_find_all() {
        let response = Response::from_wire(
            "https://shop.example.com/catalogue",
            200,
            "OK",
            html_headers("utf-8"),
            PAGE_ONE.as_bytes().to_vec(),
            &SelectorConfig::default(),
            None,
        );

        // The metadata half is available without touching the parser at all —
        // this is what makes the lazy deviation worth having, since a spider
        // rejecting a 403 never pays for a parse.
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.url, "https://shop.example.com/catalogue");
        assert_eq!(response.headers["Server"], "nginx");
        assert!(response.request_headers.is_empty());
        assert!(response.history.is_empty());
        assert!(!response.is_parsed(), "no selector call has been made yet");

        // Unit 5's engine.
        assert_eq!(
            response.css("p.price::text").expect("css").texts(),
            ["$9.99"]
        );
        assert!(response.is_parsed(), "the first selector call parses");
        assert_eq!(
            response.css("a").expect("css").attrs("href"),
            ["/p/1", "/p/2"]
        );

        // Unit 6's engine, on the very same tree.
        assert_eq!(
            response.xpath("//a/@href").expect("xpath").texts(),
            ["/p/1", "/p/2"]
        );
        assert_eq!(
            response
                .xpath("//p[@data-sku='A-1']")
                .expect("xpath")
                .texts(),
            ["$9.99"]
        );

        // Unit 10's own query builder.
        let hits = response
            .find_all(&FindQuery::new().tag("a").attr("href*", "/p/2"))
            .expect("find_all");
        assert_eq!(hits.texts(), ["two"]);

        // …and the two-way identity: a node found by XPath is the node found by
        // CSS, so a caller can mix engines and compare results.
        let by_css = response.css("p.price").expect("css");
        let by_xpath = response.xpath("//p[@class='price']").expect("xpath");
        assert_eq!(by_css.at(0), by_xpath.at(0));

        // Base-URL resolution comes from the response URL, not from the page.
        assert_eq!(
            response.doc().urljoin("/p/3"),
            "https://shop.example.com/p/3"
        );
        // `body` survives the parse — Python's `Response.body` is the raw bytes.
        assert_eq!(response.body.as_bytes(), PAGE_ONE.as_bytes());
    }

    /// The `charset=` of a `Content-Type` header has to reach the decoder.
    ///
    /// It did not until this wave: `browser-dom` recorded the label and then
    /// decoded every byte body as UTF-8 regardless, so unit 12 would have
    /// carried `encoding` as dead metadata. See `docs/DIVERGENCES.md`.
    #[test]
    fn the_content_type_charset_reaches_the_decoder() {
        let latin1 = b"<html><body><p class='price'>9,99\xa0\xe9</p></body></html>".to_vec();

        let mislabelled = Response::from_wire(
            "https://shop.example.com/",
            200,
            "OK",
            html_headers("utf-8"),
            latin1.clone(),
            &SelectorConfig::default(),
            None,
        );
        assert_eq!(mislabelled.doc().encoding(), "utf-8");
        assert_eq!(
            mislabelled.css("p::text").expect("css").texts(),
            ["9,99\u{fffd}\u{fffd}"],
            "an undeclared latin-1 body read as UTF-8 is replacement characters, as in Python"
        );

        let labelled = Response::from_wire(
            "https://shop.example.com/",
            200,
            "OK",
            html_headers("windows-1252"),
            latin1,
            &SelectorConfig::default(),
            None,
        );
        assert_eq!(labelled.doc().encoding(), "windows-1252");
        assert_eq!(
            labelled.css("p::text").expect("css").texts(),
            ["9,99\u{a0}é"]
        );

        // A header with no charset falls back to utf-8, which is what
        // `__extract_browser_encoding`'s `default` argument does.
        let mut bare = IndexMap::new();
        bare.insert("content-type".to_owned(), "text/html".to_owned());
        assert_eq!(charset_of(&bare, "utf-8"), "utf-8");
        assert_eq!(charset_of(&IndexMap::new(), "utf-8"), "utf-8");
        assert_eq!(charset_of(&html_headers("Shift_JIS"), "utf-8"), "Shift_JIS");
    }

    /// Two fetches, one storage backend, one adaptive relocation.
    ///
    /// This is the whole reason `Response` subclasses `Selector` in Python: the
    /// fetcher's `parser_keywords` carry `adaptive`, `adaptive_domain` and the
    /// storage handle, and the selector saved on Monday has to relocate on
    /// Tuesday's response.
    #[test]
    fn an_adaptive_round_trip_survives_two_separate_responses() {
        let storage: Arc<dyn StorageSystem> =
            Arc::new(SqliteStorage::in_memory(Some("https://shop.example.com/")).expect("opens"));

        // `adaptive_domain` is the one parser keyword that changes the parse:
        // Python passes `url=adaptive_domain or url` into `Selector.__init__`,
        // so the storage key is the domain and `Selector.url` reports it too.
        let selector_config = SelectorConfig::default()
            .with("adaptive", true.into())
            .with_adaptive_domain("shop.example.com");

        let monday = Response::from_wire(
            "https://shop.example.com/catalogue?page=1",
            200,
            "OK",
            html_headers("utf-8"),
            PAGE_ONE.as_bytes().to_vec(),
            &selector_config,
            Some(Arc::clone(&storage)),
        );
        assert!(monday.doc().adaptive_enabled());
        assert!(monday.doc().has_storage());
        assert_eq!(
            monday.doc().url(),
            "https://shop.example.com/catalogue?page=1",
            "the fetched URL is what the caller asked for"
        );
        assert_eq!(
            monday.config.effective_url(),
            "shop.example.com",
            "`adaptive_domain` is what storage is keyed on"
        );

        // Explicit save, the `Selector.save(element, identifier)` path.
        let price = monday
            .css("div#wrap > p.price")
            .expect("css")
            .at(0)
            .cloned()
            .expect("the price is on page one");
        monday.save(&price, "price").expect("saves");

        // Implicit save, the `css(..., auto_save=True)` path — same storage,
        // different identifier, and only the *first* match is stored.
        let saved = monday
            .css_with("a", &CssOpts::new().identifier("links").auto_save(true))
            .expect("css");
        assert_eq!(saved.texts(), ["one", "two"]);

        // --- Tuesday: the redesign ---------------------------------------
        let tuesday = Response::from_wire(
            "https://shop.example.com/catalogue?page=1",
            200,
            "OK",
            html_headers("utf-8"),
            PAGE_TWO.as_bytes().to_vec(),
            &selector_config,
            Some(Arc::clone(&storage)),
        );

        // The old selector is dead …
        assert!(tuesday.css("div#wrap > p.price").expect("css").is_empty());

        // … and the adaptive branch finds it anyway, through the fingerprint
        // the *other* `Response` wrote.
        let relocated = tuesday
            .css_with(
                "div#wrap > p.price",
                &CssOpts::new().identifier("price").adaptive(true),
            )
            .expect("css");
        assert_eq!(relocated.texts(), ["$10.99"]);
        assert_eq!(relocated.at(0).map(Node::tag), Some("p"));
        assert_eq!(
            relocated.at(0).and_then(|n| n.attr("class")),
            Some("price cost")
        );

        // A threshold nothing clears returns empty, never an error: the
        // `test_relocation_auto_save_no_match_above_threshold` regression.
        assert!(tuesday
            .css_with(
                "div#wrap > p.price",
                &CssOpts::new()
                    .identifier("price")
                    .adaptive(true)
                    .percentage(99),
            )
            .expect("css")
            .is_empty());

        // The fingerprint really did go through SQLite, not through a cache
        // living on the first `Response`.
        let stored = storage.retrieve("price").expect("reads").expect("saved");
        assert_eq!(stored.tag, "p");
        assert_eq!(tuesday.doc().relocate(&stored, 40).texts(), ["$10.99"]);
    }

    /// A `Response` is handed between tasks, so it must cross threads.
    ///
    /// `Document` is `Send + Sync`; wrapping it in a `OnceLock` keeps both, and
    /// that is what lets unit 12 return a `Response` out of an async fetch and
    /// unit 17 hand it to a spawned callback.
    #[test]
    fn a_response_is_send_and_sync_across_the_lazy_parse() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Response>();
        assert_send_sync::<Document>();

        let response = Arc::new(Response::from_wire(
            "https://shop.example.com/",
            200,
            "OK",
            html_headers("utf-8"),
            PAGE_ONE.as_bytes().to_vec(),
            &SelectorConfig::default(),
            None,
        ));

        // Race the lazy parse from two threads: `OnceLock` must publish one
        // tree, and the `Node`s each thread pulls out must agree.
        let texts: Vec<Vec<String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let response = Arc::clone(&response);
                    scope.spawn(move || response.css("a::text").expect("css").texts())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("no panic"))
                .collect()
        });
        assert_eq!(texts, [["one", "two"], ["one", "two"]]);
    }

    /// Unit 12's `resolve_selector_config` output is a legal `DocumentConfig`.
    ///
    /// The older seam test proved every `PARSER_KEYWORDS` entry has a *meaning*;
    /// this one proves the mapping is total now that `DocumentConfig` exists.
    #[test]
    fn every_fetcher_parser_keyword_lands_somewhere_in_document_config() {
        use browser_http::config::{FetcherConfig, PARSER_KEYWORDS};

        let requested = SelectorConfig::default()
            .with("keep_comments", true.into())
            .with("keep_cdata", true.into())
            .with("huge_tree", false.into())
            .with("adaptive", true.into())
            .with_adaptive_domain("shop.example.com");
        let fetcher = FetcherConfig::default().with_selector_config(requested);
        let resolved =
            browser_http::config::resolve_selector_config(&fetcher, &RequestOverrides::new());

        let config = DocumentConfig::new()
            .url("https://shop.example.com/x")
            .keep_comments(resolved.keep_comments())
            .keep_cdata(resolved.keep_cdata())
            .huge_tree(resolved.huge_tree())
            .adaptive(resolved.adaptive())
            .adaptive_domain(resolved.adaptive_domain().unwrap_or_default());

        assert!(config.keep_comments && config.keep_cdata && config.adaptive);
        assert!(!config.huge_tree);
        assert_eq!(config.effective_url(), "shop.example.com");

        // `storage` and `storage_args` are the two keywords that do not map to
        // a scalar: they collapse into the one `Arc<dyn StorageSystem>` slot.
        let with_storage = config.clone().storage(Arc::new(
            SqliteStorage::in_memory(Some("https://shop.example.com/")).expect("opens"),
        ));
        assert!(with_storage.storage.is_some());
        for keyword in PARSER_KEYWORDS {
            assert!(
                matches!(
                    keyword,
                    "huge_tree"
                        | "keep_comments"
                        | "keep_cdata"
                        | "adaptive"
                        | "adaptive_domain"
                        | "storage"
                        | "storage_args"
                ),
                "keyword {keyword} has no `DocumentConfig` home"
            );
        }

        // `keep_comments` really is honoured all the way to the tree.
        let doc = Document::parse_with("<div><!--k--></div>", &config);
        assert!(doc.dom().nodes().any(|n| n.is_comment()));
    }

    /// Unit 15's `FetchedResponse` is unit 12's `Response`, narrowed.
    ///
    /// `browser-spider` deliberately does not depend on the parser, so the
    /// crawl core reads a flat struct. Two conversions have to exist for unit
    /// 12 to wire the two together, and both are written here to prove the
    /// field sets line up: `Response -> FetchedResponse` on the way into the
    /// engine, and `FetchedResponse -> Document` for a spider callback (and for
    /// a dev-mode cache replay, which never had a live `Response`).
    #[test]
    fn a_fetched_response_round_trips_to_a_parsed_document() {
        use browser_spider::request::Request;
        use browser_spider::session::FetchedResponse;

        let response = Response::from_wire(
            "https://shop.example.com/catalogue",
            200,
            "OK",
            html_headers("utf-8"),
            PAGE_ONE.as_bytes().to_vec(),
            &SelectorConfig::default(),
            None,
        );

        // Unit 12 writes this as `impl From<Response> for FetchedResponse`.
        let mut fetched = FetchedResponse {
            status: response.status,
            reason: response.reason.clone(),
            url: response.url.clone(),
            body: bytes::Bytes::from(response.body.as_bytes().to_vec()),
            encoding: response.doc().encoding().to_owned(),
            headers: response.headers.clone(),
            request_headers: response.request_headers.clone(),
            cookies: serde_json::Value::Object(serde_json::Map::new()),
            history: response.history.clone(),
            meta: Default::default(),
            request: None,
        };
        let request = Request::new("https://shop.example.com/catalogue");
        fetched.attach_request(&request);
        assert_eq!(
            fetched.to_string(),
            "<200 https://shop.example.com/catalogue>"
        );
        assert_eq!(
            fetched.request.as_ref().map(|r| r.url.as_str()),
            Some("https://shop.example.com/catalogue")
        );

        // …and back to a document, which is what a spider callback needs. The
        // encoding label survives the narrowing, which is the whole reason
        // `FetchedResponse` carries one.
        let config = DocumentConfig::new()
            .url(&fetched.url)
            .encoding(&fetched.encoding);
        let doc = Document::parse_bytes(&fetched.body, &config);
        assert_eq!(doc.css("p.price::text").expect("css").texts(), ["$9.99"]);
        assert_eq!(doc.url(), "https://shop.example.com/catalogue");
    }
}

// ---------------------------------------------------------------------------
// Wave 4 seam (a): a *browser-shaped* response is the same `Response`.
//
// Unit 12 landed `browser_http::response::Response` for real, so the shadow
// struct in `response_seam` above is now history — it stays because it is the
// executable specification the unit was written against, and because it pins
// that the shape is reconstructible from nothing but the two public APIs.
//
// Unit 14 (the CDP driver) is next, and its inputs are different in kind from
// unit 12's: chromiumoxide hands over `Page.content()` as an already-decoded
// **String**, a status and header map from `Network.responseReceived`, and a
// redirect chain assembled from `redirectResponse` events. Nothing about that
// is bytes-off-a-socket. The question this module answers is whether unit 14
// can therefore reuse unit 12's `Response`, or whether it needs one of its own.
//
// It can reuse it. `Response::builder` is the seam, and these tests exercise
// every part of it a browser response needs.
// ---------------------------------------------------------------------------
mod browser_response_seam {
    use std::sync::Arc;

    use browser_http::backend::mock::{MockBackend, MockReply};
    use browser_http::backend::Method;
    use browser_http::config::SelectorConfig;
    use browser_http::redirect_safe::RedirectHop;
    use browser_http::response::{charset_from_content_type, DocumentHook, Response};
    use browser_http::session::AsyncFetcherSession;
    use browser_parser::config::CssOpts;
    use browser_parser::node::Node;
    use browser_storage::{SqliteStorage, StorageSystem};
    use indexmap::IndexMap;

    /// What `Page.content()` returns after Chrome has run the scripts: a
    /// serialised DOM, not the bytes the server sent.
    const RENDERED: &str = concat!(
        r#"<!DOCTYPE html><html><head><title>Shop</title></head><body>"#,
        r#"<div id="wrap" class="container">"#,
        r#"<p class="price" data-sku="A-1">$9.99</p>"#,
        r#"<a href="/p/1">one</a><a href="/p/2">two</a>"#,
        r#"</div></body></html>"#,
    );

    /// The same page after a redesign, as unit 14 would render it tomorrow.
    const RERENDERED: &str = concat!(
        r#"<!DOCTYPE html><html><head><title>Shop</title></head><body>"#,
        r#"<main id="wrap" class="container"><section>"#,
        r#"<p class="price cost" data-sku="A-1">$10.99</p>"#,
        r#"<a href="/p/1">one</a><a href="/p/2">two</a>"#,
        r#"</section></main></body></html>"#,
    );

    /// `PageSession._get_page_content` + `ResponseFactory.from_browser_page`,
    /// written against nothing but `browser-http`'s public builder.
    ///
    /// Every argument is what CDP actually gives you: a `String` of rendered
    /// HTML, an `i64` status, a header map from `Network.responseReceived`, and
    /// a redirect chain built from `redirectResponse`.
    fn from_browser_page(
        url: &str,
        status: u16,
        headers: IndexMap<String, String>,
        rendered: String,
        redirects: &[(&str, u16, &str)],
        selector_config: SelectorConfig,
        storage: Option<Arc<dyn StorageSystem>>,
    ) -> Response {
        // `__extract_browser_encoding`: the *browser* charset regex, which
        // accepts a quoted value. Unit 12's HTTP path deliberately uses the
        // stricter `charset_from_content_type_http` instead — see
        // `docs/DIVERGENCES.md`.
        let encoding = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .and_then(|(_, value)| charset_from_content_type(value))
            .unwrap_or("utf-8")
            .to_owned();

        let history: Vec<RedirectHop> = redirects
            .iter()
            .map(|(hop_url, hop_status, location)| RedirectHop {
                url: (*hop_url).to_owned(),
                status: *hop_status,
                reason: reason_phrase(*hop_status).to_owned(),
                headers: IndexMap::new(),
                location: (*location).to_owned(),
            })
            .collect();

        let mut builder = Response::builder(status, url)
            .method(Method::Get)
            // The rendered DOM is already text; `Bytes::from(String)` is the
            // only conversion, and the declared encoding then round-trips it.
            .body(rendered)
            .headers(headers)
            .encoding(encoding)
            .history(history)
            .selector_config(selector_config);
        if let Some(storage) = storage {
            let hook: DocumentHook = Arc::new(move |config| config.storage(Arc::clone(&storage)));
            builder = builder.document_hook(hook);
        }
        builder.build()
    }

    fn html_headers(content_type: &str) -> IndexMap<String, String> {
        let mut headers = IndexMap::new();
        headers.insert("content-type".to_owned(), content_type.to_owned());
        headers
    }

    /// The two phrases this module needs. `browser-fingerprint`'s `StatusText`
    /// is the real source and unit 14 will use it directly; the façade does not
    /// depend on that crate for tests, so the table is inlined here.
    fn reason_phrase(status: u16) -> &'static str {
        match status {
            301 => "Moved Permanently",
            302 => "Found",
            other => panic!("no phrase for {other} in this fixture"),
        }
    }

    /// Anything that reads a response reads *one* type, whoever produced it.
    ///
    /// This is the actual assertion of this module: the function is monomorphic
    /// in `browser_http::response::Response`, and both engines feed it.
    fn extract_prices(response: &Response) -> Vec<String> {
        response
            .css("p.price::text")
            .expect("css")
            .texts()
            .into_iter()
            .map(|text| text.trim().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_browser_response_and_an_http_response_are_the_same_type() {
        // --- the browser half: a String and a header map, no socket ---------
        let rendered = from_browser_page(
            "https://shop.test/catalogue",
            200,
            // Chrome reports the charset quoted often enough that the reference implementation's
            // browser regex accepts quotes; the HTTP one does not.
            html_headers(r#"text/html; charset="utf-8""#),
            RENDERED.to_owned(),
            &[("https://shop.test/", 301, "https://shop.test/catalogue")],
            SelectorConfig::default(),
            None,
        );

        // --- the HTTP half: unit 12's real driver over a mock transport -----
        let backend = Arc::new(MockBackend::always(
            MockReply::new(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .body(RENDERED),
        ));
        let session = AsyncFetcherSession::builder()
            .backend(backend)
            .build()
            .expect("session");
        session.open().await.expect("open");
        let fetched = session
            .get("https://shop.test/catalogue")
            .send()
            .await
            .expect("send");
        session.close().await.expect("close");

        // One reader, two producers. If unit 14 needed its own `Response` this
        // line would not compile.
        assert_eq!(extract_prices(&rendered), ["$9.99"]);
        assert_eq!(extract_prices(&fetched), extract_prices(&rendered));

        // Everything the metadata half promises is filled in on both sides.
        for response in [&rendered, &fetched] {
            assert_eq!(response.status, 200);
            assert_eq!(response.reason, "OK");
            assert_eq!(response.url, "https://shop.test/catalogue");
            assert_eq!(response.method, Method::Get);
            assert_eq!(response.encoding, "utf-8");
            assert!(response
                .header("content-type")
                .is_some_and(|value| value.starts_with("text/html")));
            assert_eq!(
                response.xpath("//a/@href").expect("xpath").texts(),
                ["/p/1", "/p/2"]
            );
            assert_eq!(response.urljoin("/p/3"), "https://shop.test/p/3");
            assert_eq!(response.to_string(), "<200 https://shop.test/catalogue>");
        }

        // The browser side is the one that carries a redirect chain built from
        // `redirectResponse`; the mock transport had nothing to redirect.
        assert_eq!(rendered.history.len(), 1);
        assert_eq!(rendered.history[0].status, 301);
        assert_eq!(rendered.history[0].to_string(), "<301 https://shop.test/>");
        assert!(fetched.history.is_empty());
    }

    /// The browser charset regex is the looser of the two, and it is public.
    #[test]
    fn the_browser_charset_regex_is_the_one_unit_14_needs() {
        use browser_http::response::charset_from_content_type_http;

        // The reference implementation's `__CHARSET_RE__` — `charset=["']?([\w-]+)` — accepts a
        // quoted value; `curl_cffi`'s does not, and unit 12 is bound by
        // `curl_cffi`'s because that is what resolved the charset in Python.
        assert_eq!(
            charset_from_content_type(r#"text/html; charset="windows-1252""#),
            Some("windows-1252")
        );
        assert_eq!(
            charset_from_content_type_http(r#"text/html; charset="windows-1252""#),
            None
        );
        // Unquoted, they agree.
        assert_eq!(
            charset_from_content_type("text/html; charset=windows-1252"),
            charset_from_content_type_http("text/html; charset=windows-1252"),
        );
    }

    /// A browser response drives the adaptive layer exactly like an HTTP one.
    ///
    /// `StealthyFetcher` is the fetcher people point at the pages that break,
    /// so this is the path adaptive selectors matter most on.
    #[test]
    fn a_browser_response_saves_and_relocates_through_shared_storage() {
        let storage: Arc<dyn StorageSystem> =
            Arc::new(SqliteStorage::in_memory(Some("https://shop.test/")).expect("opens"));
        let selector_config = SelectorConfig::default()
            .with("adaptive", true.into())
            .with_adaptive_domain("shop.test");

        let monday = from_browser_page(
            "https://shop.test/catalogue?page=1",
            200,
            html_headers("text/html; charset=utf-8"),
            RENDERED.to_owned(),
            &[],
            selector_config.clone(),
            Some(Arc::clone(&storage)),
        );
        assert!(monday.adaptive_enabled());
        assert!(monday.has_storage());
        assert_eq!(monday.url, "https://shop.test/catalogue?page=1");
        assert_eq!(monday.selector_url(), "shop.test");

        let price = monday
            .css("div#wrap > p.price")
            .expect("css")
            .at(0)
            .cloned()
            .expect("the price is on the rendered page");
        monday.save(&price, "price").expect("saves");

        // Tomorrow's render, through the *same* storage.
        let tuesday = from_browser_page(
            "https://shop.test/catalogue?page=1",
            200,
            html_headers("text/html; charset=utf-8"),
            RERENDERED.to_owned(),
            &[],
            selector_config,
            Some(Arc::clone(&storage)),
        );
        assert!(tuesday.css("div#wrap > p.price").expect("css").is_empty());

        let relocated = tuesday
            .css_with(
                "div#wrap > p.price",
                &CssOpts::new().identifier("price").adaptive(true),
            )
            .expect("css");
        assert_eq!(relocated.texts(), ["$10.99"]);
        assert_eq!(relocated.at(0).map(Node::tag), Some("p"));
    }

    /// The narrowing unit 17 reads, from a response unit 14 produced.
    #[test]
    fn a_browser_response_narrows_into_a_fetched_response_and_back() {
        use browser_spider::session::FetchedResponse;

        let rendered = from_browser_page(
            "https://shop.test/catalogue",
            200,
            html_headers("text/html; charset=utf-8"),
            RENDERED.to_owned(),
            &[("https://shop.test/", 302, "https://shop.test/catalogue")],
            SelectorConfig::default(),
            None,
        );

        // `impl From<&Response> for FetchedResponse` — unit 15 owed this and it
        // now exists in `browser-spider`, which is the crate that may name
        // both types.
        let narrowed = FetchedResponse::from(&rendered);
        assert_eq!(narrowed.status, 200);
        assert_eq!(narrowed.url, "https://shop.test/catalogue");
        assert_eq!(narrowed.encoding, "utf-8");
        assert_eq!(narrowed.history, ["https://shop.test/"]);
        assert_eq!(narrowed.body.as_ref(), RENDERED.as_bytes());
        assert_eq!(narrowed.to_string(), "<200 https://shop.test/catalogue>");

        // …and widening again gives a callback the same document. This is the
        // dev-mode cache replay path: no live exchange ever happened.
        let replayed = narrowed.response_builder().build();
        assert_eq!(extract_prices(&replayed), ["$9.99"]);
        assert_eq!(replayed.encoding, "utf-8");
        assert_eq!(replayed.url, narrowed.url);
        // The two documented losses.
        assert!(replayed.history.is_empty());
        assert!(replayed.meta.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Wave 4 seam (b): unit 17's crawl loop, written from the parts that exist.
//
// `CrawlerEngine` (unit 17) is the last big piece of the spider, and it owns no
// mechanism of its own — it is an *ordering* over unit 15's `Scheduler` and
// `SessionManager`, unit 16's `AutoThrottle`, `RobotsTxtManager` and response
// cache, and unit 12's fetcher. `_process_request`'s order (spec §5.4) is:
//
//     robots -> cache replay -> limiter -> delay -> fetch -> cache write
//     -> block detection -> autothrottle feedback -> retry-or-callback
//
// This module writes that order out by hand against a loopback server. It is
// deliberately *not* a `CrawlerEngine`: the point is that unit 17 has nothing
// left to invent, only to arrange. Anything the loop could not express would be
// an API gap to fix now — which is how `From<&Response> for FetchedResponse`,
// `FetchedResponse::response_builder` and `Method: From<HttpMethod>` came to
// exist this wave.
//
// No public network: the fixture is a `std::net::TcpListener` on 127.0.0.1.
// ---------------------------------------------------------------------------
mod crawl_loop_seam {
    use std::collections::BTreeSet;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use browser_http::backend::Method;
    use browser_http::config::{FetcherConfig, SelectorConfig};
    use browser_http::session::AsyncFetcherSession;
    use browser_spider::cache::ResponseCacheManager;
    use browser_spider::error::{Result as SpiderResult, SpiderError};
    use browser_spider::request::{FingerprintOptions, Request, SessionKwargs};
    use browser_spider::robots::{effective_domain_delay, fetcher_fn, RobotsTxtManager};
    use browser_spider::scheduler::Scheduler;
    use browser_spider::session::{AddOptions, CrawlSession, FetchedResponse, SessionManager};
    use browser_spider::stats::CrawlStats;
    use browser_spider::throttle::{parse_retry_after, AutoThrottle, AutoThrottleConfig};

    // -- the fixture ------------------------------------------------------

    /// A four-page site plus a `robots.txt` that forbids `/admin`.
    ///
    /// `/a` links back to `/` and forward to `/b`, so the crawl has to
    /// de-duplicate; `/admin/secret` is linked from `/` and must never be
    /// requested; `/throttled` answers `429` with a `Retry-After`.
    fn route(path: &str) -> Option<(u16, &'static str, String)> {
        let page = |links: &[&str], item: &str| {
            let anchors: String = links
                .iter()
                .map(|href| format!(r#"<a href="{href}">{href}</a>"#))
                .collect();
            format!(
                r#"<!doctype html><html><body><h1 class="item">{item}</h1>{anchors}</body></html>"#
            )
        };
        match path {
            "/robots.txt" => Some((
                200,
                "text/plain; charset=utf-8",
                // A tiny non-zero `Crawl-delay` so the floor is observable
                // without making the test slow.
                "User-agent: *\nDisallow: /admin\nCrawl-delay: 0.01\n".to_owned(),
            )),
            "/" => Some((
                200,
                "text/html; charset=utf-8",
                page(&["/a", "/b", "/admin/secret", "/a"], "home"),
            )),
            "/a" => Some((200, "text/html; charset=utf-8", page(&["/", "/b"], "alpha"))),
            "/b" => Some((200, "text/html; charset=utf-8", page(&[], "beta"))),
            "/admin/secret" => Some((200, "text/html; charset=utf-8", page(&[], "SECRET"))),
            "/throttled" => Some((429, "text/html; charset=utf-8", page(&[], "slow down"))),
            _ => None,
        }
    }

    /// The loopback fixture. Raw sockets, one connection per request.
    struct Fixture {
        port: u16,
        hits: Arc<Mutex<Vec<String>>>,
        running: Arc<AtomicBool>,
    }

    impl Fixture {
        fn start() -> Self {
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
                        Ok(stream) => serve(stream, &thread_hits),
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

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().expect("not poisoned").clone()
        }

        fn hit_count(&self, path: &str) -> usize {
            self.hits().iter().filter(|hit| *hit == path).count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Release);
            // Unblock the `accept` so the thread notices and exits.
            let _ = TcpStream::connect(("127.0.0.1", self.port));
        }
    }

    fn serve(mut stream: TcpStream, hits: &Arc<Mutex<Vec<String>>>) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
            return;
        }
        // Drain the headers; nothing here reads a body.
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

        let (status, content_type, body, extra) = match route(&path) {
            Some((429, content_type, body)) => (429_u16, content_type, body, "Retry-After: 0\r\n"),
            Some((status, content_type, body)) => (status, content_type, body, ""),
            None => (404, "text/plain; charset=utf-8", "not found".to_owned(), ""),
        };
        let reason = match status {
            200 => "OK",
            429 => "Too Many Requests",
            _ => "Not Found",
        };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
             Content-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
        // Half-close, then drain to EOF. `Shutdown::Both` here would let the
        // kernel RST a response the client has not finished reading — rare
        // because this fixture is driven sequentially, but a real race.
        let _ = stream.shutdown(Shutdown::Write);
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
    }

    // -- the adapter unit 17 will promote into `browser-spider` ----------

    /// Unit 12's pooled session behind unit 15's [`CrawlSession`] trait.
    ///
    /// Thirty lines, all of them mechanical — which is the finding. Unit 17
    /// should move this into `browser-spider` verbatim; it is written here so
    /// that the *test* proves it needs nothing that is not already public.
    struct HttpCrawlSession {
        sid: String,
        inner: AsyncFetcherSession,
    }

    #[async_trait]
    impl CrawlSession for HttpCrawlSession {
        fn is_alive(&self) -> bool {
            self.inner.is_alive()
        }

        async fn open(&self) -> SpiderResult<()> {
            self.inner
                .open()
                .await
                .map_err(|error| SpiderError::SessionLifecycle {
                    sid: self.sid.clone(),
                    action: "open",
                    message: error.to_string(),
                })
        }

        async fn close(&self) -> SpiderResult<()> {
            self.inner
                .close()
                .await
                .map_err(|error| SpiderError::SessionLifecycle {
                    sid: self.sid.clone(),
                    action: "close",
                    message: error.to_string(),
                })
        }

        async fn fetch(&self, url: &str, kwargs: &SessionKwargs) -> SpiderResult<FetchedResponse> {
            // `kwargs` is borrowed and stays untouched — the pinned invariant
            // that stops a retried POST from decaying into a GET.
            let method: Method = kwargs.method().into();
            let mut builder = self.inner.request(method, url);
            if let Some(headers) = kwargs.effective_headers() {
                for (name, value) in headers {
                    builder = builder.header(name.clone(), value.clone());
                }
            }
            if let Some(json) = kwargs.json.as_ref() {
                builder = builder.json(json.clone());
            }
            let response = builder.send().await.map_err(|error| SpiderError::Fetch {
                sid: self.sid.clone(),
                url: url.to_owned(),
                message: error.to_string(),
            })?;
            // The narrowing unit 15 asked for, now a real `From` impl.
            Ok(FetchedResponse::from(&response))
        }
    }

    /// `CrawlerEngine._is_blocked`, in its status-code half.
    fn is_blocked(response: &FetchedResponse) -> bool {
        matches!(response.status, 403 | 429 | 503)
    }

    // -- the loop ----------------------------------------------------------

    #[tokio::test]
    async fn a_whole_crawl_composes_from_units_12_15_and_16() {
        let fixture = Fixture::start();
        let cache_dir = tempfile::tempdir().expect("tempdir");

        // --- unit 12: the transport ---------------------------------------
        let session = AsyncFetcherSession::builder()
            .config(
                FetcherConfig::default()
                    .with_retries(Some(1))
                    .with_timeout(Some(Duration::from_secs(5))),
            )
            .build()
            .expect("a session over the default backend");

        // --- unit 15: routing and the frontier ----------------------------
        let mut manager = SessionManager::new();
        manager
            .add(
                "http",
                Arc::new(HttpCrawlSession {
                    sid: "http".to_owned(),
                    inner: session,
                }),
                AddOptions::default(),
            )
            .expect("registers");
        let manager = Arc::new(manager);
        manager.start().await.expect("opens every session");

        let mut scheduler = Scheduler::new(FingerprintOptions::default());

        // --- unit 16: robots, throttle, cache -----------------------------
        let robots_manager = Arc::clone(&manager);
        let robots = Arc::new(RobotsTxtManager::new(Arc::new(fetcher_fn(
            move |url: String, sid: String| {
                let manager = Arc::clone(&robots_manager);
                async move { manager.fetch(&Request::new(url).with_sid(sid)).await }
            },
        ))));

        let throttle = AutoThrottle::new(AutoThrottleConfig {
            start_delay: 0.0,
            max_delay: 0.2,
            target_concurrency: 1.0,
            block_backoff: true,
        })
        .expect("a valid config");
        let cache = ResponseCacheManager::new(cache_dir.path());

        // --- the engine's own state ---------------------------------------
        let stats = CrawlStats::new();
        let download_delay = 0.0_f64;
        let selector_config = SelectorConfig::default();

        // `CrawlerEngine._prefetch_robots_txt`, verbatim: one fetch per seed
        // netloc before the loop starts.
        let seeds = vec![fixture.url("/"), fixture.url("/throttled")];
        robots
            .prefetch(&browser_spider::robots::prefetch_urls(&seeds), "http")
            .await;
        assert_eq!(fixture.hit_count("/robots.txt"), 1, "one netloc, one fetch");

        for seed in &seeds {
            assert!(scheduler.enqueue(Request::new(seed.clone()).with_sid("http")));
        }

        let mut items: Vec<String> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        let mut replayed = 0_usize;

        while let Some((ticket, request)) = scheduler.dequeue() {
            // 1. robots.txt.
            if !robots.can_fetch(&request.url, "*").await {
                refused.push(request.url.clone());
                scheduler.complete(ticket);
                continue;
            }

            // 2. the delay floor `robots.txt` imposes, then AutoThrottle's view
            //    of the domain. Unit 16 owns both halves of this expression.
            let (crawl_delay, request_rate) =
                robots.get_delay_directives(&request.url, "http").await;
            let floor = effective_domain_delay(download_delay, crawl_delay, request_rate);
            assert!(floor >= 0.01, "the fixture's Crawl-delay reaches the floor");
            let delay = throttle.delay_for(request.domain(), floor);
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;

            // 3. the cache replay, keyed on unit 15's fingerprint.
            let fingerprint = request.update_fingerprint(scheduler.options());
            let started = Instant::now();
            let (response, from_cache) = match cache.get(&fingerprint).await {
                Some(mut hit) => {
                    hit.attach_request(&request);
                    replayed += 1;
                    (hit, true)
                }
                None => {
                    let fetched = manager.fetch(&request).await.expect("the fixture answers");
                    cache
                        .put(
                            &fingerprint,
                            &fetched,
                            request.session_kwargs.method().as_str(),
                        )
                        .await;
                    (fetched, false)
                }
            };

            // 4. bookkeeping — the stats a crawl reports.
            stats.increment_requests_count(&request.sid);
            stats.increment_status(response.status);
            stats.increment_response_bytes(request.domain(), response.body.len() as u64);

            // 5. block detection feeding AutoThrottle.
            let ok = (200..300).contains(&response.status) && !is_blocked(&response);
            let retry_after = parse_retry_after(&response.headers);
            if !from_cache {
                throttle.record(
                    request.domain(),
                    started.elapsed().as_secs_f64(),
                    ok,
                    floor,
                    retry_after,
                );
            }
            if !ok {
                blocked.push(request.url.clone());
                assert_eq!(retry_after, Some(0.0), "the fixture sends `Retry-After: 0`");
                scheduler.complete(ticket);
                continue;
            }

            // 6. the callback: parse through unit 12's `Response`, yield items,
            //    enqueue links. A cache replay takes the same path, which is
            //    what `FetchedResponse::response_builder` is for.
            let parsed = response
                .response_builder()
                .selector_config(selector_config.clone())
                .build();
            items.extend(parsed.css("h1.item::text").expect("css").texts());
            for href in parsed.css("a").expect("css").attrs("href") {
                let absolute = parsed.urljoin(&href);
                scheduler.enqueue(Request::new(absolute).with_sid("http"));
            }

            scheduler.complete(ticket);
        }

        // --- what the loop proves -----------------------------------------
        assert!(scheduler.is_empty() && scheduler.inflight_len() == 0);

        // `/admin/secret` was linked from `/` and never fetched: the robots
        // check runs *before* the fetch, not after it.
        assert_eq!(refused, [fixture.url("/admin/secret")]);
        assert_eq!(fixture.hit_count("/admin/secret"), 0);

        // Every allowed page was visited exactly once, however many times it
        // was linked — unit 15's fingerprint de-duplication.
        assert_eq!(
            items,
            ["home", "alpha", "beta"],
            "`/throttled` yielded nothing and `/admin/secret` was never fetched"
        );
        assert_eq!(fixture.hit_count("/"), 1);
        assert_eq!(fixture.hit_count("/a"), 1, "`/a` is linked twice from `/`");
        assert_eq!(
            fixture.hit_count("/b"),
            1,
            "`/b` is linked from `/` and `/a`"
        );
        assert_eq!(blocked, [fixture.url("/throttled")]);
        assert_eq!(replayed, 0, "a cold cache replays nothing");

        // AutoThrottle learned a delay for the domain, and the 429 pushed it up
        // to the configured ceiling rather than down towards the latency.
        let domain = format!("127.0.0.1:{}", fixture.port);
        assert!(throttle.contains(&domain));
        stats.set_autothrottle_delays(throttle.delays());
        assert_eq!(
            stats.autothrottle_delay(&domain),
            Some(throttle.delays()[&domain])
        );

        // The stats a crawl reports are all reachable.
        assert_eq!(stats.session_requests("http"), 4);
        assert_eq!(stats.status_count(200), 3);
        assert_eq!(stats.status_count(429), 1);
        assert!(stats.domain_bytes(&domain) > 0);

        manager.close().await.expect("closes every session");
        drop(fixture);
    }

    /// The same loop, second run: every page is a cache hit and the server is
    /// never touched again.
    #[tokio::test]
    async fn the_second_pass_replays_from_the_unit_16_cache() {
        let fixture = Fixture::start();
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(cache_dir.path());

        let session = AsyncFetcherSession::builder()
            .config(FetcherConfig::default().with_retries(Some(1)))
            .build()
            .expect("session");
        let mut manager = SessionManager::new();
        manager
            .add(
                "http",
                Arc::new(HttpCrawlSession {
                    sid: "http".to_owned(),
                    inner: session,
                }),
                AddOptions::default(),
            )
            .expect("registers");
        let manager = Arc::new(manager);
        manager.start().await.expect("start");

        let options = FingerprintOptions::default();
        let mut seen_bodies: Vec<BTreeSet<String>> = Vec::new();

        for pass in 0..2 {
            let mut scheduler = Scheduler::new(options);
            scheduler.enqueue(Request::new(fixture.url("/")).with_sid("http"));
            let mut bodies = BTreeSet::new();

            while let Some((ticket, request)) = scheduler.dequeue() {
                let fingerprint = request.update_fingerprint(options);
                let response = match cache.get(&fingerprint).await {
                    Some(mut hit) => {
                        assert_eq!(pass, 1, "nothing is cached on the first pass");
                        hit.attach_request(&request);
                        hit
                    }
                    None => {
                        assert_eq!(pass, 0, "everything is cached by the second pass");
                        let fetched = manager.fetch(&request).await.expect("fetch");
                        cache.put(&fingerprint, &fetched, "GET").await;
                        fetched
                    }
                };
                // A replayed response carries the request the engine attached,
                // and reparses into the same document as the live one.
                assert_eq!(
                    response.request.as_ref().map(|r| r.url.clone()),
                    Some(request.url.clone())
                );
                let parsed = response.response_builder().build();
                bodies.extend(parsed.css("h1.item::text").expect("css").texts());
                for href in parsed.css("a").expect("css").attrs("href") {
                    // `/admin` is not consulted here; this pass is about the
                    // cache, so every link is followed.
                    scheduler.enqueue(Request::new(parsed.urljoin(&href)).with_sid("http"));
                }
                scheduler.complete(ticket);
            }
            seen_bodies.push(bodies);
        }

        assert_eq!(
            seen_bodies[0], seen_bodies[1],
            "a replay is not a downgrade"
        );
        assert!(seen_bodies[0].contains("home"));
        // Four distinct URLs, fetched on pass 0 and replayed on pass 1.
        let hits = fixture.hits();
        let paths: BTreeSet<&str> = hits.iter().map(String::as_str).collect();
        assert_eq!(
            paths,
            BTreeSet::from(["/", "/a", "/b", "/admin/secret"]),
            "the first pass fetched every link"
        );
        assert_eq!(hits.len(), 4, "the second pass touched the server not once");

        manager.close().await.expect("close");
        drop(fixture);
    }
}

// ===========================================================================
// Wave 5 → wave 6 seams: unit 18 (`browser-templates`) and unit 19
// (`browser-cli`).
//
// Same contract as `browser_response_seam` and `crawl_loop_seam` before them:
// the next wave's hardest API questions are answered *here*, in a test, against
// the crates as they actually are, so that unit 18 and unit 19 are assembly and
// not API archaeology. Every one of these modules is a working sketch of a
// wave-6 deliverable:
//
// * `crawl_spider_seam`      → `LinkExtractor` + `CrawlRule` + `CrawlSpider`
// * `site_to_markdown_seam`  → `SiteToMarkdownSpider`
// * `cli_extract_seam`       → `brs extract get <url> <output>`
//
// All three run against a loopback server this file starts. Nothing reaches the
// public internet, and the browser half skips cleanly when there is no Chrome.
// ===========================================================================

/// A loopback site the wave-6 seam tests crawl.
///
/// The same shape as [`crawl_loop_seam::Fixture`], generalised over the route
/// table so the three modules below can each declare their own pages.
mod local_site {
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
        pub body: String,
    }

    impl Page {
        /// A `200 text/html; charset=utf-8` reply.
        pub fn html(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: body.into(),
            }
        }

        /// A `200 text/plain; charset=utf-8` reply.
        pub fn text(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                content_type: "text/plain; charset=utf-8",
                body: body.into(),
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
                            // One thread per connection: a browser opens
                            // several at once and keeps them alive.
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

        // The query string is not part of the route key; `?a=1` still lands on
        // the page, which is what the CLI seam's `params=` needs.
        let key = path.split('?').next().unwrap_or("/");
        let page = route(key).unwrap_or(Page {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            body: "not found".to_owned(),
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
        let _ = stream.write_all(page.body.as_bytes());
        let _ = stream.flush();
        // Half-close and drain, so the peer is never RST before it has read.
        let _ = stream.shutdown(Shutdown::Write);
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
    }
}

// ---------------------------------------------------------------------------
// (a) Unit 18's `CrawlSpider`: a user-written `Spider` with rules, run to
//     completion by `CrawlerEngine` over a loopback site.
// ---------------------------------------------------------------------------

/// Proof that `CrawlSpider` is expressible on the unit-17 `Spider` trait.
///
/// Everything unit 18 has to build is sketched here against the *real* API:
/// `strip_html5_whitespace`, a `LinkExtractor` with `allow`/`deny`, a
/// `CrawlRule` carrying a callback name, and — the design point spelled out in
/// the plan — **rule dispatch as a free function**, so a user can call it from
/// their own `parse` instead of inheriting behaviour they cannot see.
///
/// If any of this stopped compiling, unit 18 would be blocked.
mod crawl_spider_seam {
    use std::sync::Arc;

    use std::sync::Mutex;

    use async_trait::async_trait;
    use regex::Regex;
    use serde_json::Value;

    use browser_spider::request::{CallbackId, Item, Request};
    use browser_spider::spider::{
        CallbackStream, CrawlError, CrawlResponse, FollowOptions, RunOptions, Spider, SpiderRunner,
        SpiderSettings, Yielded,
    };

    use super::local_site::{Page, Site};

    // -- the fixture ------------------------------------------------------

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

    // -- the pieces unit 18 will own --------------------------------------

    /// The five HTML5 space characters, and **only** those.
    ///
    /// `str::trim` would also strip U+00A0, U+2028, the Mongolian vowel
    /// separator and every other Unicode `White_Space` code point, which would
    /// silently change which `href`s a `LinkExtractor` accepts. Unit 18 must
    /// use this, not `trim`.
    fn strip_html5_whitespace(value: &str) -> &str {
        const HTML5_SPACE: [char; 5] = ['\t', '\n', '\u{0c}', '\r', ' '];
        value.trim_matches(|c| HTML5_SPACE.contains(&c))
    }

    /// The `allow` / `deny` core of `LinkExtractor`.
    ///
    /// Unit 18 grows the other eleven kwargs (`allow_domains`, `deny_domains`,
    /// `restrict_css`, `restrict_xpath`, `tags`, `attrs`, `canonicalize`,
    /// `unique`, `process_value`, `strip`, `IGNORED_EXTENSIONS`) on top; the
    /// point of the sketch is that everything it needs — `css`, `attrs`,
    /// `urljoin` — is already on `CrawlResponse` through its `Deref`.
    struct LinkExtractor {
        allow: Vec<Regex>,
        deny: Vec<Regex>,
    }

    impl LinkExtractor {
        fn new(allow: &[&str], deny: &[&str]) -> Self {
            let compile = |patterns: &[&str]| {
                patterns
                    .iter()
                    .map(|p| Regex::new(p).expect("a valid pattern"))
                    .collect()
            };
            Self {
                allow: compile(allow),
                deny: compile(deny),
            }
        }

        /// Absolute, de-duplicated, document-ordered links this extractor keeps.
        fn extract(&self, response: &CrawlResponse) -> Vec<String> {
            let mut kept: Vec<String> = Vec::new();
            for href in response.css("a[href]").expect("css").attrs("href") {
                let absolute = response.urljoin(strip_html5_whitespace(&href));
                let allowed =
                    self.allow.is_empty() || self.allow.iter().any(|rule| rule.is_match(&absolute));
                let denied = self.deny.iter().any(|rule| rule.is_match(&absolute));
                if allowed && !denied && !kept.contains(&absolute) {
                    kept.push(absolute);
                }
            }
            kept
        }
    }

    /// One `Rule(LinkExtractor(...), callback=..., follow=...)`.
    struct CrawlRule {
        extractor: LinkExtractor,
        /// `None` means "route it to `parse`", which is also what makes the
        /// followed page get its own links extracted.
        callback: Option<CallbackId>,
    }

    /// **Rule dispatch as a free function.**
    ///
    /// Python's `CrawlSpider._requests_to_follow` is a method, so a user who
    /// wants rules *and* their own `parse` has to call `super()`. Here it is a
    /// plain function over `&[CrawlRule]`, so a spider composes it — which is
    /// exactly what `ShopSpider::parse` below does.
    ///
    /// # Errors
    ///
    /// [`CrawlError::NoRequestOnResponse`] when `response` was built by hand and
    /// has no originating request to inherit from.
    fn requests_to_follow(
        rules: &[CrawlRule],
        response: &CrawlResponse,
    ) -> Result<Vec<Request>, CrawlError> {
        let mut requests = Vec::new();
        for rule in rules {
            for url in rule.extractor.extract(response) {
                let options = FollowOptions {
                    // Never inherited: a rule names its callback, and a rule
                    // with none means `parse`. Leaving it `None` would inherit
                    // the *current* callback, so a product page would try to
                    // parse the pages it links to as products.
                    callback: Some(rule.callback.clone().unwrap_or(CallbackId::PARSE)),
                    ..FollowOptions::default()
                };
                requests.push(response.follow_with(&url, &options)?);
            }
        }
        Ok(requests)
    }

    // -- the user's spider -------------------------------------------------

    /// The named callback `Rule(callback="parse_product")` points at.
    const PRODUCT: CallbackId = CallbackId::from_static("parse_product");

    struct ShopSpider {
        settings: SpiderSettings,
        rules: Vec<CrawlRule>,
        /// Every URL that reached a callback, in arrival order.
        visited: Mutex<Vec<String>>,
    }

    impl ShopSpider {
        /// `Rule(callback="parse_product")`'s target: one item per product page.
        fn parse_product(&self, response: &CrawlResponse) -> Option<Item> {
            let name = response.css("h1.name::text").ok()?.get()?;
            let price = response.css("span.price::text").ok()?.get()?;
            let mut item = Item::new();
            item.insert("url".to_owned(), Value::String(response.url.clone()));
            item.insert("name".to_owned(), Value::String(name));
            item.insert("price".to_owned(), Value::String(price));
            Some(item)
        }
    }

    #[async_trait]
    impl Spider for ShopSpider {
        fn name(&self) -> &str {
            "shop"
        }

        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }

        async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
            self.visited
                .lock()
                .expect("not poisoned")
                .push(response.url.clone());
            let requests = requests_to_follow(&self.rules, &response).expect("a crawled response");
            Box::pin(futures::stream::iter(
                requests.into_iter().map(Yielded::request),
            ))
        }

        async fn dispatch(
            self: Arc<Self>,
            callback: CallbackId,
            response: CrawlResponse,
        ) -> CallbackStream {
            if callback.as_str() == PRODUCT.as_str() {
                self.visited
                    .lock()
                    .expect("not poisoned")
                    .push(response.url.clone());
                let yielded: Vec<Yielded> = self
                    .parse_product(&response)
                    .into_iter()
                    .map(Yielded::item)
                    .collect();
                return Box::pin(futures::stream::iter(yielded));
            }
            self.parse(response).await
        }

        fn has_callback(&self, callback: &CallbackId) -> bool {
            callback.as_str() == CallbackId::PARSE.as_str() || callback.as_str() == PRODUCT.as_str()
        }
    }

    // -- the test ----------------------------------------------------------

    #[tokio::test]
    async fn a_rule_driven_spider_runs_to_completion_on_the_real_engine() {
        let site = Site::start(shop);

        let spider = Arc::new(ShopSpider {
            settings: SpiderSettings::default()
                .with_start_urls([site.url("/")])
                .with_robots_txt_obey(true)
                // Serial, so `visited` is a deterministic sequence rather than
                // a race between four workers.
                .with_concurrent_requests(1),
            rules: vec![
                CrawlRule {
                    extractor: LinkExtractor::new(&[r"/p/\d+$"], &[]),
                    callback: Some(PRODUCT),
                },
                CrawlRule {
                    // `/private/` is *allowed by the rule* on purpose: the only
                    // thing that can stop it is the robots.txt gate, so this row
                    // proves the gate runs before the fetch.
                    extractor: LinkExtractor::new(&[r"/about$", r"/private/"], &[r"/tag/"]),
                    callback: None,
                },
            ],
            visited: Mutex::new(Vec::new()),
        });

        let runner = SpiderRunner::new(
            Arc::clone(&spider) as Arc<dyn Spider>,
            RunOptions::default().with_ctrl_c(false),
        )
        .expect("the default `configure_sessions` registers one HTTP session");

        let result = runner.run().await.expect("the crawl finishes");

        // --- the items ----------------------------------------------------
        assert!(result.completed(), "nothing paused this crawl");
        let names: Vec<&str> = result
            .items
            .as_slice()
            .iter()
            .map(|item| item["name"].as_str().expect("a string"))
            .collect();
        assert_eq!(
            names,
            ["Widget", "Gadget"],
            "one item per product page, in rule order"
        );
        assert_eq!(
            result.items.as_slice()[0]["price"].as_str(),
            Some("$9.99"),
            "the named callback really ran, not `parse`"
        );

        // --- the rules ----------------------------------------------------
        let visited = spider.visited.lock().expect("not poisoned").clone();
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

        // --- the stats ----------------------------------------------------
        assert_eq!(result.stats.status_count(200), 4);
        assert_eq!(result.stats.items_scraped(), 2);
        assert!(result.stats.domain_bytes(&site.netloc()) > 0);
    }

    /// The `Spider` trait's default methods are enough for the *degenerate*
    /// `CrawlSpider`: no rules, no named callbacks, just `start_urls`.
    ///
    /// Unit 18's `SitemapSpider` and `XMLFeedSpider` are this shape.
    #[tokio::test]
    async fn a_spider_with_no_rules_still_needs_only_three_methods() {
        struct Bare(SpiderSettings);

        #[async_trait]
        impl Spider for Bare {
            fn name(&self) -> &str {
                "bare"
            }
            fn settings(&self) -> &SpiderSettings {
                &self.0
            }
            async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
                let mut item = Item::new();
                item.insert(
                    "title".to_owned(),
                    Value::String(
                        response
                            .css("title::text")
                            .ok()
                            .and_then(|nodes| nodes.get())
                            .unwrap_or_default(),
                    ),
                );
                Box::pin(futures::stream::iter([Yielded::item(item)]))
            }
        }

        let site = Site::start(shop);
        let spider = Arc::new(Bare(
            SpiderSettings::default().with_start_urls([site.url("/p/1")]),
        ));
        let result = SpiderRunner::new(spider, RunOptions::default().with_ctrl_c(false))
            .expect("runner")
            .run()
            .await
            .expect("crawl");

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items.as_slice()[0]["title"].as_str(), Some("Widget"));
    }
}

// ---------------------------------------------------------------------------
// (b) Unit 18's `SiteToMarkdownSpider`: the markdown crate consuming a
//     `Response` from BOTH engines.
// ---------------------------------------------------------------------------

/// Proof that `SiteToMarkdownSpider` is expressible.
///
/// Its whole premise is that markdown conversion is a property of the
/// **response**, not of the transport — so a spider can be pointed at the HTTP
/// fetcher or at the browser and produce the same document. That only holds if
/// there is exactly one `Response` type and the `markdown` feature reaches it
/// from both sides, which is what these tests check.
///
/// The browser half **skips cleanly** when there is no Chrome (or when
/// `BROWSER_RS_SKIP_BROWSER_TESTS` is set) and really runs when there is.
mod site_to_markdown_seam {
    use std::sync::Arc;

    use browser_cdp::config::{BrowserConfig, FetchParams};
    use browser_cdp::session::{resolve_executable, BrowserSession};
    use browser_http::response::Response;
    use browser_http::session::AsyncFetcher;
    use browser_spider::session::FetchedResponse;
    use browser_spider::spider::CrawlResponse;

    use super::local_site::{Page, Site};

    /// One article, wrapped in the noise `_strip_noise_tags` is supposed to
    /// remove — a `<script>`, a `<nav>` and a `<style>`.
    const ARTICLE: &str = r#"<!doctype html><html><head><title>Post</title>
<style>.x{display:none}</style></head><body>
<nav><a href="/">home</a></nav>
<article><h1>Widgets</h1><p>A <strong>short</strong> post.</p>
<ul><li>one</li><li>two</li></ul></article>
<script>console.log("noise")</script>
</body></html>"#;

    fn article(path: &str) -> Option<Page> {
        match path {
            "/" | "/post" => Some(Page::html(ARTICLE)),
            _ => None,
        }
    }

    /// What a `SiteToMarkdownSpider` callback does with the response it is
    /// handed: one function, both engines.
    ///
    /// Taking `&Response` rather than a transport-specific type is the seam.
    fn to_markdown(response: &Response) -> String {
        response
            .markdown_with(Some("article"), true)
            .expect("`article` is a valid selector")
    }

    /// Whether a live-browser test can run here.
    fn browser_available() -> bool {
        if std::env::var_os("BROWSER_RS_SKIP_BROWSER_TESTS").is_some() {
            return false;
        }
        BrowserConfig::new()
            .validate()
            .is_ok_and(|config| resolve_executable(&config).is_ok())
    }

    #[tokio::test]
    async fn the_markdown_crate_consumes_an_http_driven_response() {
        let site = Site::start(article);
        let response = AsyncFetcher::get(site.url("/post"))
            .send()
            .await
            .expect("the fixture answers");

        assert_eq!(response.status, 200);
        let markdown = to_markdown(&response);
        assert!(markdown.contains("Widgets"), "{markdown:?}");
        assert!(markdown.contains("**short**"), "{markdown:?}");
        assert!(markdown.contains("* one"), "{markdown:?}");
        assert!(
            !markdown.contains("console.log"),
            "`_strip_noise_tags` dropped the <script>: {markdown:?}"
        );

        // The whole-document form, which is what `Response.markdown()` is.
        assert!(response.markdown().contains("Widgets"));
    }

    #[tokio::test]
    async fn the_markdown_crate_consumes_a_browser_driven_response() {
        if !browser_available() {
            eprintln!(
                "skipping site_to_markdown_seam: no Chromium/Chrome binary on this \
                 machine (or BROWSER_RS_SKIP_BROWSER_TESTS is set)"
            );
            return;
        }

        let site = Site::start(article);
        let session = BrowserSession::new(
            BrowserConfig::new()
                .headless(true)
                .timeout_ms(20_000.0)
                .disable_resources(true),
        )
        .expect("a valid session config");
        session.start().await.expect("Chrome starts");

        let response = session
            .fetch(&site.url("/post"), FetchParams::default())
            .await
            .expect("the browser reaches the fixture");
        session.close().await;

        assert_eq!(response.status, 200);
        // The **same** function as the HTTP arm, on the same type.
        let markdown = to_markdown(&response);
        assert!(markdown.contains("Widgets"), "{markdown:?}");
        assert!(markdown.contains("**short**"), "{markdown:?}");
        assert!(markdown.contains("* one"), "{markdown:?}");
        assert!(!markdown.contains("console.log"), "{markdown:?}");

        // And the two engines agree byte for byte on this page. That is not a
        // law — a page whose DOM JavaScript rewrites cannot agree, which is the
        // entire reason the browser engine exists — but on a static document the
        // rendered DOM round-trips through `browser-dom` to the same tree, and
        // any drift here would be a real regression in one of the two paths.
        let over_http = AsyncFetcher::get(site.url("/post"))
            .send()
            .await
            .expect("the fixture answers");
        assert_eq!(
            markdown,
            to_markdown(&over_http),
            "a rendered DOM and a raw body converge on one markdown document"
        );
    }

    /// The spider-side seam: a `CrawlResponse` — what a callback is handed — is
    /// a `Response`, so `SiteToMarkdownSpider` writes the *same* line.
    #[test]
    fn a_crawl_response_reaches_markdown_through_its_deref() {
        let fetched = FetchedResponse::new(200, "https://shop.test/post", ARTICLE);
        let response = CrawlResponse::from_fetched(&fetched, &Default::default(), None);

        // `CrawlResponse` derefs to `Response`; no second conversion, no second
        // markdown entry point.
        let markdown = to_markdown(&response);
        assert!(markdown.contains("Widgets"));
        assert!(markdown.contains("* two"));

        // …and the item a `SiteToMarkdownSpider` would yield.
        let mut item = browser_spider::request::Item::new();
        item.insert(
            "url".to_owned(),
            serde_json::Value::String(response.url.clone()),
        );
        item.insert("markdown".to_owned(), serde_json::Value::String(markdown));
        assert_eq!(
            item["url"].as_str(),
            Some("https://shop.test/post"),
            "the item is an ordinary `Item`, exportable by `ItemList`"
        );
    }

    /// `Arc<Response>` crosses tasks, which is what a concurrent
    /// `SiteToMarkdownSpider` needs.
    #[test]
    fn a_response_carrying_markdown_is_still_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Response>();
        assert_send_sync::<Arc<Response>>();
        assert_send_sync::<CrawlResponse>();
    }
}

// ---------------------------------------------------------------------------
// (c) Unit 19's `brs extract get <url> <output>`, end to end.
// ---------------------------------------------------------------------------

/// The whole `extract get` call chain, from parsed arguments to a file on disk.
///
/// `cli-ai.md` §2 and §4.5–4.7 describe four steps:
///
/// 1. `__BuildRequest` turns the shared option group into fetcher kwargs;
/// 2. `Fetcher.get(url, **kwargs)` runs one request;
/// 3. `Convertor._extract_content` renders the page in one of three modes;
/// 4. `Convertor.write_content_to_file` picks the mode off the **filename
///    suffix** and writes with the *page's* encoding.
///
/// Every one of those is written here against the real API, so unit 19 is a
/// `clap` derive plus this body. The parts that turned out to matter — the
/// generator's trailing `""`, the per-character whitespace collapsing in `text`
/// mode, and the fact that a suffix that is not one of the three is an error
/// rather than a default — are all pinned.
mod cli_extract_seam {
    use std::time::Duration;

    use regex::Regex;

    use browser_http::config::{Cookies, Query};
    use browser_http::response::Response;
    use browser_http::session::AsyncFetcher;
    use browser_parser::config::AllTextOpts;

    use super::local_site::{Page, Site};

    fn docs(path: &str) -> Option<Page> {
        match path {
            "/doc" => Some(Page::html(
                "<!doctype html><html><head><title>Doc</title>\
                 <script>var noise = 1;</script></head><body>\
                 <main><h2>Heading</h2><p>Body   text\n\n\nhere.</p>\
                 <a href=\"/next\">next</a></main>\
                 <footer>  \t footer  </footer></body></html>",
            )),
            _ => None,
        }
    }

    // -- step 1: the shared option group ----------------------------------

    /// `brs extract get`'s shared options, as `clap` will parse them.
    ///
    /// Deliberately the *parsed* form: `--header a:b` has already become a pair
    /// and `--timeout 5` has already become seconds. Unit 19 owns the parsing;
    /// this pins what the parsed values are then used for.
    #[derive(Default)]
    struct GetArgs {
        headers: Vec<(String, String)>,
        cookies: Cookies,
        params: Query,
        /// **Seconds** — the HTTP commands' unit. The browser commands take
        /// milliseconds, and keeping the two apart is unit 19's job.
        timeout_secs: Option<u64>,
        css_selector: Option<String>,
        ai_targeted: bool,
    }

    /// `__BuildRequest` + `Fetcher.get`.
    async fn run_get(url: &str, args: &GetArgs) -> Response {
        let mut builder = AsyncFetcher::get(url.to_owned());
        for (name, value) in &args.headers {
            builder = builder.header(name.clone(), value.clone());
        }
        if !args.cookies.is_empty() {
            builder = builder.cookies(args.cookies.clone());
        }
        if !args.params.is_empty() {
            builder = builder.params(args.params.clone());
        }
        builder = builder.timeout(args.timeout_secs.map(Duration::from_secs));
        builder.send().await.expect("the fixture answers")
    }

    // -- step 3: `Convertor._extract_content` ------------------------------

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Extraction {
        Markdown,
        Html,
        Text,
    }

    /// `_extension_map`: the suffix decides the mode, and an unknown suffix is
    /// an error rather than a default.
    fn extraction_for(filename: &str) -> Result<Extraction, String> {
        if filename.ends_with(".md") {
            Ok(Extraction::Markdown)
        } else if filename.ends_with(".html") {
            Ok(Extraction::Html)
        } else if filename.ends_with(".txt") {
            Ok(Extraction::Text)
        } else {
            Err(format!(
                "Unknown file type: filename must end with '.md', '.html', or '.txt': {filename}"
            ))
        }
    }

    /// `text` mode's whitespace collapsing: runs of `\n`, then `\r`, then `\t`,
    /// then ` `, each collapsed **independently and in that order**.
    fn collapse_whitespace(text: &str) -> String {
        let mut out = text.to_owned();
        for ch in ['\n', '\r', '\t', ' '] {
            let pattern = Regex::new(&format!("[{}]+", regex::escape(&ch.to_string())))
                .expect("a valid pattern");
            out = pattern.replace_all(&out, ch.to_string()).into_owned();
        }
        out
    }

    /// `Convertor._extract_content`, including the trailing `""` the generator
    /// always yields — so `pieces.len()` is `matches + 1`.
    fn extract_content(
        response: &Response,
        extraction: Extraction,
        css_selector: Option<&str>,
        main_content_only: bool,
    ) -> Vec<String> {
        let mut pieces: Vec<String> = Vec::new();
        match extraction {
            Extraction::Markdown => {
                // `markdown_with` already does the `<body>` narrowing, the noise
                // stripping and the AI sanitising, per selector.
                pieces.push(
                    response
                        .markdown_with(css_selector, main_content_only)
                        .expect("a valid selector"),
                );
            }
            Extraction::Html => match css_selector {
                Some(selector) => pieces.extend(
                    response
                        .css(selector)
                        .expect("a valid selector")
                        .iter()
                        .map(browser_parser::node::Node::html_content),
                ),
                None => pieces.push(response.html()),
            },
            Extraction::Text => {
                let opts = AllTextOpts {
                    strip: true,
                    ignore_tags: ["script", "style", "noscript", "svg", "iframe"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    ..AllTextOpts::default()
                };
                match css_selector {
                    Some(selector) => pieces.extend(
                        response
                            .css(selector)
                            .expect("a valid selector")
                            .iter()
                            .map(|node| collapse_whitespace(&node.get_all_text_with(&opts))),
                    ),
                    None => {
                        pieces.push(collapse_whitespace(&response.get_all_text_with(&opts)));
                    }
                }
            }
        }
        // The generator's final `""`. It changes `len()` and nothing else.
        pieces.push(String::new());
        pieces
    }

    // -- step 4: `Convertor.write_content_to_file` -------------------------

    /// Writes with the page's own encoding — here always a UTF-8 superset, so
    /// `std::fs::write` is enough; unit 19 must re-encode when it is not.
    fn write_content_to_file(
        response: &Response,
        path: &std::path::Path,
        css_selector: Option<&str>,
        main_content_only: bool,
    ) -> Result<(), String> {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("Filename must be provided")?;
        if filename.trim().is_empty() {
            return Err("Filename must be provided".to_owned());
        }
        let extraction = extraction_for(filename)?;
        let body = extract_content(response, extraction, css_selector, main_content_only).concat();
        std::fs::write(path, body).map_err(|error| error.to_string())
    }

    // -- the test ----------------------------------------------------------

    #[tokio::test]
    async fn extract_get_runs_end_to_end_into_all_three_output_modes() {
        let site = Site::start(docs);
        let out = tempfile::tempdir().expect("tempdir");

        let args = GetArgs {
            headers: vec![("x-seam".to_owned(), "unit-19".to_owned())],
            params: vec![("q".to_owned(), "widget".to_owned())],
            timeout_secs: Some(5),
            css_selector: Some("main".to_owned()),
            ai_targeted: true,
            ..GetArgs::default()
        };

        let response = run_get(&site.url("/doc"), &args).await;
        assert_eq!(response.status, 200);
        assert_eq!(
            site.hits(),
            ["/doc?q=widget"],
            "`params=` really reached the wire"
        );

        // --- .md ----------------------------------------------------------
        let md = out.path().join("page.md");
        write_content_to_file(
            &response,
            &md,
            args.css_selector.as_deref(),
            args.ai_targeted,
        )
        .expect("markdown is written");
        let markdown = std::fs::read_to_string(&md).expect("read");
        assert!(markdown.contains("Heading"), "{markdown:?}");
        assert!(
            !markdown.contains("var noise"),
            "`--ai-targeted` strips the noise tags: {markdown:?}"
        );

        // --- .html --------------------------------------------------------
        let html = out.path().join("page.html");
        write_content_to_file(&response, &html, args.css_selector.as_deref(), false)
            .expect("html is written");
        let rendered = std::fs::read_to_string(&html).expect("read");
        assert!(rendered.starts_with("<main>"), "outer HTML: {rendered:?}");
        assert!(rendered.contains("<h2>Heading</h2>"));

        // --- .txt ---------------------------------------------------------
        let txt = out.path().join("page.txt");
        write_content_to_file(&response, &txt, None, false).expect("text is written");
        let text = std::fs::read_to_string(&txt).expect("read");
        assert!(text.contains("Heading"), "{text:?}");
        assert!(
            !text.contains("var noise"),
            "`ignore_tags` drops <script> even without --ai-targeted: {text:?}"
        );
        assert!(
            !text.contains("  "),
            "runs of spaces are collapsed: {text:?}"
        );
        assert!(
            !text.contains("\n\n"),
            "runs of newlines are collapsed: {text:?}"
        );

        // --- the suffix map is exhaustive and closed ----------------------
        assert_eq!(extraction_for("a.md"), Ok(Extraction::Markdown));
        assert_eq!(extraction_for("a.html"), Ok(Extraction::Html));
        assert_eq!(extraction_for("a.txt"), Ok(Extraction::Text));
        assert!(extraction_for("output.invalid").is_err());
        assert!(extraction_for("output").is_err());
        assert!(
            write_content_to_file(&response, &out.path().join("page.xyz"), None, false).is_err(),
            "an unknown suffix is a `ValueError`, not a silent default"
        );

        // --- the generator's trailing empty string ------------------------
        let pieces = extract_content(&response, Extraction::Html, Some("main a"), false);
        assert_eq!(pieces.len(), 2, "one match plus the trailing \"\"");
        assert_eq!(pieces.last().map(String::as_str), Some(""));
    }

    /// `extract get` on a 404 still writes a file: the CLI reports the status
    /// but does not refuse to extract, exactly as Python's does.
    #[tokio::test]
    async fn a_non_200_is_still_extractable() {
        let site = Site::start(docs);
        let out = tempfile::tempdir().expect("tempdir");

        let response = run_get(&site.url("/missing"), &GetArgs::default()).await;
        assert_eq!(response.status, 404);

        let path = out.path().join("missing.txt");
        write_content_to_file(&response, &path, None, false).expect("written");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read").trim(),
            "not found"
        );
    }
}

// ---------------------------------------------------------------------------
// (d) Unit 20's façade: four end-to-end user journeys, written **only** against
//     `browser::`.
// ---------------------------------------------------------------------------

/// Proof that unit 20 is expressible from the API that exists today.
///
/// Every other module in this file reaches past the façade on purpose — that is
/// what makes them *seam* tests, and `Cargo.toml`'s `[dev-dependencies]` block
/// says so. **This one does not.** Nothing below names `browser_parser`,
/// `browser_http`, `browser_cdp`, `browser_spider`,
/// `browser_templates`, `browser_dom`, `browser_markdown`,
/// `browser_storage` or `browser_xpath`; every path starts at `browser::`.
/// That restraint is the whole assertion. A member crate that is in the graph
/// but has no path through the façade is invisible to a real user, and this
/// module is the only thing in the repo that would notice.
///
/// Unit 20's deliverables are the re-export surface, the feature flags,
/// end-to-end tests against a local server, criterion benches and runnable
/// examples for the four entry points. So there is one journey per entry point:
///
/// 1. **parse** — `Document::parse`, CSS, XPath, `find_similar`, and the
///    adaptive save → relocate round trip through a real SQLite database;
/// 2. **fetch over HTTP** — `AsyncFetcher` and the blocking `Fetcher` against a
///    loopback site, then the document API on the reply;
/// 3. **fetch through Chrome** — `BrowserSession`, skipping cleanly when there
///    is no browser;
/// 4. **crawl** — a ready-made `SiteToMarkdownSpider` *and* a user-written
///    `Spider`, both run to completion by `SpiderRunner` over a loopback site.
///
/// Writing it found five holes, all now closed:
///
/// * `browser-markdown`, `browser-dom`, `browser-xpath` and
///   `browser-storage` were `[dev-dependencies]` only, so four types the
///   public API *hands back* had no name — `Document::dom`,
///   `Document::xpath_value`, the `StorageSystem` implementation and the two
///   sanitiser passes;
/// * `CallbackStream` was missing from the flat spider surface;
/// * `async_trait` was not re-exported at all, and **every** user-written
///   `Spider` needs it, because the trait is declared with it;
/// * neither was `futures`, and `CallbackStream` is a type alias for
///   `futures::stream::BoxStream<'static, Yielded>`, so a callback cannot
///   construct its own return value without it.
///
/// The last two are the sharp ones: a re-export surface that makes a trait
/// *nameable* but not *implementable* looks complete and is not.
///
/// # Feature gate
///
/// The module needs `spider`, `browser`, `markdown`, `adaptive` and `blocking`
/// all at once, because it exercises all four entry points. That is what
/// `--all-features` gives it, and it is how the workspace's own definition of
/// done runs. Under a smaller feature set it is replaced by a single test that
/// says so out loud, rather than vanishing silently.
#[cfg(all(
    feature = "spider",
    feature = "browser",
    feature = "markdown",
    feature = "adaptive",
    feature = "blocking"
))]
mod unit_20_facade_seam {
    use std::sync::{Arc, Mutex};

    use browser::prelude::*;
    use browser::{
        async_trait, hostname_of, strip_html5_whitespace, url_extensions, CallbackStream,
        CrawlRule, CssOpts, DocumentConfig, LinkExtractor, RunOptions, SimilarOpts,
        SiteToMarkdownSpider, SpiderRunner, IGNORED_EXTENSIONS,
    };

    use super::local_site::{Page, Site};

    /// A small catalogue: an index, two products with a repeating card
    /// structure, and a `robots.txt`.
    fn catalogue(path: &str) -> Option<Page> {
        match path {
            "/robots.txt" => Some(Page::text("User-agent: *\nDisallow: /admin/\n")),
            "/" => Some(Page::html(
                r#"<!doctype html><html><head><title>Catalogue</title></head><body>
<main>
  <div class="card"><h2>Widget</h2><span class="price">$9.99</span></div>
  <div class="card"><h2>Gadget</h2><span class="price">$19.99</span></div>
  <div class="card"><h2>Doohickey</h2><span class="price">$4.50</span></div>
</main>
<a href="/p/widget">Widget</a> <a href="/p/gadget">Gadget</a>
<a href="/admin/secret">admin</a>
</body></html>"#,
            )),
            "/p/widget" | "/p/gadget" => Some(Page::html(format!(
                r#"<!doctype html><html><head><title>{name}</title></head><body>
<nav><a href="/">home</a></nav>
<article><h1>{name}</h1><p>A <strong>fine</strong> {name}.</p></article>
<script>track()</script>
</body></html>"#,
                name = if path.ends_with("widget") {
                    "Widget"
                } else {
                    "Gadget"
                },
            ))),
            _ => None,
        }
    }

    // -- (1) parse ---------------------------------------------------------

    /// The document entry point: parse a string, query it four ways, and run
    /// the adaptive round trip — all through `browser::`.
    #[test]
    fn the_parse_entry_point_is_complete_through_the_facade() {
        let html = catalogue("/").expect("the index").body;
        let doc = Document::parse(&html);

        // CSS, including the `::text` pseudo-element and the option struct.
        assert_eq!(doc.css("h2::text").expect("valid").texts().len(), 3);
        assert_eq!(
            doc.css_with(".price", &CssOpts::default())
                .expect("valid")
                .texts(),
            ["$9.99", "$19.99", "$4.50"]
        );

        // XPath — both the node-set form and the scalar one, whose return type
        // needed `browser-xpath` to be nameable at all.
        assert_eq!(doc.xpath("//div[@class='card']").expect("valid").len(), 3);
        let count = doc
            .xpath_value("count(//a)", &browser::xpath::eval::Variables::default())
            .expect("valid");
        assert!(matches!(count, browser::xpath::Value::Number(n) if n == 3.0));

        // `find_all` and `find_similar`, the two the reference implementation-specific queries.
        let first_card = doc.css_first(".card").expect("valid").expect("matched");
        assert_eq!(
            first_card
                .find_similar(&SimilarOpts::default())
                .texts()
                .len(),
            2,
            "the other two cards, the query node excluded"
        );
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("span"))
                .expect("valid")
                .len(),
            3
        );

        // The tree underneath, which `Document::dom()` hands back.
        let tree: &browser::dom::Document = doc.dom();
        assert!(tree.root_element().is_some());
    }

    /// The adaptive branch, end to end: save a fingerprint against a real
    /// SQLite database, re-parse a page whose markup has *moved*, and relocate.
    ///
    /// This is the journey that needed `browser-storage` promoted — there is
    /// exactly one `StorageSystem` implementation and a façade-only user could
    /// not construct it.
    #[test]
    fn the_adaptive_round_trip_runs_through_the_facade() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage: Arc<dyn browser::storage::StorageSystem> = Arc::new(
            browser::storage::SqliteStorage::open(
                dir.path().join("elements.db"),
                Some("https://shop.test/"),
            )
            .expect("a database"),
        );

        let config = DocumentConfig {
            url: "https://shop.test/".to_owned(),
            adaptive: true,
            storage: Some(Arc::clone(&storage)),
            ..DocumentConfig::new()
        };

        let before = Document::parse_with(
            r#"<html><body><main><div class="card" id="w"><h2>Widget</h2>
               <span class="price">$9.99</span></div></main></body></html>"#,
            &config,
        );
        let card = before.css_first(".card").expect("valid").expect("matched");
        before.save(&card, "product-card").expect("saved");

        // The site redesigns: the class is gone and the element has moved.
        let after = Document::parse_with(
            r#"<html><body><section><article id="w" data-role="product">
               <h2>Widget</h2><span class="price">$9.99</span></article></section></body></html>"#,
            &config,
        );
        assert!(
            after.css(".card").expect("valid").is_empty(),
            "the old selector really is dead"
        );

        let fingerprint = after
            .retrieve("product-card")
            .expect("readable")
            .expect("stored");
        let relocated = after.relocate(&fingerprint, 40);
        assert_eq!(
            relocated.len(),
            1,
            "the element is found by similarity, not by selector"
        );
        assert!(relocated
            .first()
            .expect("one")
            .get_all_text_with(&AllTextOpts::default())
            .contains("Widget"));
    }

    // -- (2) fetch over HTTP -----------------------------------------------

    /// The async fetcher entry point: one request, then the whole document API
    /// on the reply, then markdown.
    #[tokio::test]
    async fn the_http_entry_point_is_complete_through_the_facade() {
        let site = Site::start(catalogue);

        let response: Response = AsyncFetcher::get(site.url("/p/widget"))
            .header("Accept", "text/html")
            .timeout(Some(std::time::Duration::from_secs(10)))
            .send()
            .await
            .expect("the fixture answers");

        assert_eq!(response.status, 200);
        assert_eq!(response.encoding, "utf-8");
        assert_eq!(
            response.css("h1::text").expect("valid").get().as_deref(),
            Some("Widget")
        );
        assert!(response.markdown().contains("**fine**"));
        assert!(
            !response.markdown().contains("track()"),
            "`Response::markdown` sanitises unconditionally, as in Python"
        );

        // A session, which is the shape a scraping program actually uses.
        let session = AsyncFetcherSession::builder()
            .build()
            .expect("a default session");
        let index = session
            .get(site.url("/"))
            .send()
            .await
            .expect("the fixture answers");
        assert_eq!(index.css(".card").expect("valid").len(), 3);

        // …and a failure is an ordinary error, not a panic.
        let missing = session
            .get(site.url("/nope"))
            .send()
            .await
            .expect("a reply");
        assert_eq!(missing.status, 404);
    }

    /// The blocking façade, which is what a script and the CLI's sync paths
    /// use. It owns its own runtime, so it cannot run inside `#[tokio::test]`.
    #[test]
    fn the_blocking_fetcher_is_complete_through_the_facade() {
        let site = Site::start(catalogue);
        let response = Fetcher::get(site.url("/"))
            .send()
            .expect("the fixture answers");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.css("title::text").expect("valid").get().as_deref(),
            Some("Catalogue")
        );
    }

    // -- (3) fetch through Chrome ------------------------------------------

    /// Whether a live-browser test can run here — the same probe the rest of
    /// the workspace uses, written through the façade.
    fn browser_available() -> bool {
        if std::env::var_os("BROWSER_RS_SKIP_BROWSER_TESTS").is_some() {
            return false;
        }
        BrowserConfig::new()
            .validate()
            .is_ok_and(|config| browser::browser::session::resolve_executable(&config).is_ok())
    }

    /// The browser entry point: launch Chrome, fetch, and get back the *same*
    /// `Response` the HTTP fetcher produces.
    #[tokio::test]
    async fn the_browser_entry_point_is_complete_through_the_facade() {
        if !browser_available() {
            eprintln!(
                "skipping unit_20_facade_seam::the_browser_entry_point: no Chrome on \
                 this machine (or BROWSER_RS_SKIP_BROWSER_TESTS is set)"
            );
            return;
        }

        let site = Site::start(catalogue);
        let config = BrowserConfig::new()
            .headless(true)
            .timeout_ms(20_000.0)
            .disable_resources(true);

        // The one-shot form — `DynamicFetcher.fetch` in Python.
        let once: Response = browser::fetch_once(
            config.clone(),
            &site.url("/p/gadget"),
            FetchParams::default(),
        )
        .await
        .expect("Chrome reaches the fixture");
        assert_eq!(once.status, 200);
        assert_eq!(
            once.css("h1::text").expect("valid").get().as_deref(),
            Some("Gadget")
        );

        // …and the session form, for more than one page.
        let session = BrowserSession::new(config).expect("a valid config");
        session.start().await.expect("Chrome starts");
        let index = session
            .fetch(&site.url("/"), FetchParams::default())
            .await
            .expect("the browser reaches the fixture");
        session.close().await;
        assert_eq!(index.css(".card").expect("valid").len(), 3);
    }

    // -- (4) crawl ---------------------------------------------------------

    /// The ready-made template, run to completion by the real engine.
    ///
    /// `SiteToMarkdownSpider` needs `spider` **and** `markdown`, which is the
    /// only two-feature gate on the whole surface.
    #[tokio::test]
    async fn a_ready_made_spider_runs_to_completion_through_the_facade() {
        let site = Site::start(catalogue);
        let out = tempfile::tempdir().expect("tempdir");

        let spider = Arc::new(
            SiteToMarkdownSpider::builder("catalogue")
                .settings(
                    SpiderSettings::default()
                        .with_start_urls([site.url("/")])
                        .with_allowed_domains([site.netloc()])
                        .with_robots_txt_obey(true)
                        .with_concurrent_requests(1),
                )
                .css_selector("article")
                .output_dir(out.path())
                .build()
                .expect("a valid spider"),
        );

        let result = SpiderRunner::new(spider, RunOptions::default().with_ctrl_c(false))
            .expect("a runner")
            .run()
            .await
            .expect("the crawl finishes");

        assert!(result.completed());
        // One item per page reached, index included — the spider emits whatever
        // the selector found, empty or not, which is Python's behaviour.
        let urls: Vec<&str> = result
            .items
            .as_slice()
            .iter()
            .map(|item| item["url"].as_str().expect("a string"))
            .collect();
        assert_eq!(
            urls,
            [site.url("/"), site.url("/p/widget"), site.url("/p/gadget")],
            "the index and both products, in discovery order"
        );
        let markdown: Vec<&str> = result
            .items
            .as_slice()
            .iter()
            .map(|item| item["markdown"].as_str().expect("a string"))
            .collect();
        assert_eq!(markdown[0], "", "the index has no <article>");
        assert!(markdown[1].contains("**fine**"), "{:?}", markdown[1]);
        assert!(
            !markdown[1].contains("track()"),
            "the noise strip runs: {:?}",
            markdown[1]
        );
        assert_eq!(
            site.hit_count("/admin/secret"),
            0,
            "robots.txt is obeyed through the façade too"
        );
        assert!(
            std::fs::read_dir(out.path())
                .expect("readable")
                .any(|entry| entry
                    .expect("entry")
                    .path()
                    .extension()
                    .is_some_and(|e| e == "md")),
            "`output_dir` really wrote the pages"
        );
    }

    /// A **user-written** spider: the trait, the attribute macro, the callback
    /// stream and rule dispatch as a free function, all from `browser::`.
    ///
    /// This is the journey that found the `async_trait` hole. Without the
    /// re-export the `impl` block below does not compile, and a façade-only user
    /// would have had to add `async-trait` to their own manifest and hope the
    /// version unified with the one the trait was declared with.
    #[tokio::test]
    async fn a_user_written_spider_needs_nothing_but_the_facade() {
        const PRODUCT: browser::CallbackId = browser::CallbackId::from_static("parse_product");

        struct Shop {
            settings: SpiderSettings,
            rules: Vec<CrawlRule>,
            seen: Mutex<Vec<String>>,
        }

        #[async_trait]
        impl Spider for Shop {
            fn name(&self) -> &str {
                "shop"
            }

            fn settings(&self) -> &SpiderSettings {
                &self.settings
            }

            async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
                // The free function, not an inherited method: a spider that is
                // not a `CrawlSpider` composes rule dispatch into its own
                // callback.
                let requests = requests_to_follow(&self.rules, &response).unwrap_or_default();
                Box::pin(browser::futures::stream::iter(
                    requests
                        .into_iter()
                        .map(Yielded::request)
                        .collect::<Vec<_>>(),
                ))
            }

            async fn dispatch(
                self: Arc<Self>,
                callback: browser::CallbackId,
                response: CrawlResponse,
            ) -> CallbackStream {
                if callback.as_str() != PRODUCT.as_str() {
                    return self.parse(response).await;
                }
                self.seen
                    .lock()
                    .expect("not poisoned")
                    .push(response.url.clone());
                let mut item = Item::new();
                item.insert(
                    "title".to_owned(),
                    serde_json::Value::String(
                        response
                            .css("h1::text")
                            .ok()
                            .and_then(|nodes| nodes.get())
                            .unwrap_or_default(),
                    ),
                );
                Box::pin(browser::futures::stream::iter([Yielded::item(item)]))
            }

            // A spider that overrides `dispatch` must override this too, or a
            // restored checkpoint rewrites every named callback to `parse`.
            fn has_callback(&self, callback: &browser::CallbackId) -> bool {
                callback.as_str() == browser::CallbackId::PARSE.as_str()
                    || callback.as_str() == PRODUCT.as_str()
            }
        }

        let site = Site::start(catalogue);
        let extractor = LinkExtractor::builder()
            .allow([r"/p/"])
            .deny([r"/admin/"])
            .build()
            .expect("valid patterns");

        let spider = Arc::new(Shop {
            settings: SpiderSettings::default()
                .with_start_urls([site.url("/")])
                .with_allowed_domains([site.netloc()])
                .with_concurrent_requests(1),
            rules: vec![CrawlRule::new(extractor).callback(PRODUCT)],
            seen: Mutex::new(Vec::new()),
        });

        let result = SpiderRunner::new(
            Arc::clone(&spider) as Arc<dyn Spider>,
            RunOptions::default().with_ctrl_c(false),
        )
        .expect("a runner")
        .run()
        .await
        .expect("the crawl finishes");

        let titles: Vec<&str> = result
            .items
            .as_slice()
            .iter()
            .map(|item| item["title"].as_str().expect("a string"))
            .collect();
        assert_eq!(titles, ["Widget", "Gadget"]);
        assert_eq!(spider.seen.lock().expect("not poisoned").len(), 2);
        assert_eq!(site.hit_count("/admin/secret"), 0, "the `deny` rule holds");

        // The stats and the exporter, which is where a crawl ends.
        assert_eq!(result.stats.items_scraped(), 2);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("items.jsonl");
        result.items.to_jsonl(&path).expect("exports");
        let exported = std::fs::read_to_string(&path).expect("readable");
        assert_eq!(exported.lines().count(), 2, "{exported}");
        assert!(exported.contains("Widget"), "{exported}");
    }

    /// The URL primitives on their own, which is what someone extending a
    /// template reaches for.
    #[test]
    fn the_link_extraction_primitives_are_reachable_flat() {
        assert_eq!(strip_html5_whitespace("\u{c}\n /a \t"), "/a");
        // NOT `str::trim`: U+00A0 is Unicode whitespace and is *not* one of the
        // five HTML5 space characters.
        assert_eq!(strip_html5_whitespace("\u{a0}/a"), "\u{a0}/a");
        assert_eq!(url_extensions("/files/report.tar.gz"), ["tar.gz", "gz"]);
        assert!(IGNORED_EXTENSIONS.contains(&"pdf"));
        assert!(!IGNORED_EXTENSIONS.contains(&"html"));
        assert_eq!(hostname_of("https://Shop.TEST:443/a"), "shop.test");
    }
}

/// The notice the façade seam leaves behind when it is compiled out.
///
/// A test that disappears with a feature flag is indistinguishable from a test
/// that was deleted. This one is always compiled, so `cargo test -p browser-rs`
/// on the default feature set still says why eight tests are missing.
#[cfg(not(all(
    feature = "spider",
    feature = "browser",
    feature = "markdown",
    feature = "adaptive",
    feature = "blocking"
)))]
#[test]
fn unit_20_facade_seam_needs_all_features() {
    eprintln!(
        "skipping unit_20_facade_seam: it drives all four entry points and so \
         needs `spider`, `browser`, `markdown`, `adaptive` and `blocking` \
         together. Run `cargo test -p browser-rs --all-features`."
    );
}
