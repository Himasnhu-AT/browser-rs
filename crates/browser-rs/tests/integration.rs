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
    use browser_http::config::{
        FetcherConfig, RequestOverrides, SelectorConfig, PARSER_KEYWORDS,
    };

    let mut requested = SelectorConfig::default();
    requested.insert("keep_comments", true.into());
    requested.insert("huge_tree", true.into());

    let config = FetcherConfig::default().with_selector_config(requested.clone());
    let resolved =
        browser_http::config::resolve_selector_config(&config, &RequestOverrides::new());
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
