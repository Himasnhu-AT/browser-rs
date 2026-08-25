//! The live assertions.
//!
//! Everything here drives **real Chrome** against the **local fixture server**
//! of [`super::server`], and compares against
//! `testdata/u14_browser_golden.json`, which real the reference implementation produced over the
//! same fixture with the same browser binary.
//!
//! The one exception is the Cloudflare section, and it says so at the top of
//! each test: a real Turnstile challenge is not reachable offline, so the
//! solver is driven against a **structural fixture** that reproduces the DOM
//! and the state transitions it keys on. That verifies the state machine, the
//! attempt counts, the polling cadence and the click arithmetic — it does not
//! verify that Cloudflare accepts the click.

#![allow(clippy::too_many_lines)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use browser_core::types::{Override, WaitState};

use super::corpus::{corpus, with_port};
use super::server::Fixture;
use super::{live_config, require_browser, session_for, session_with_hooks};
use crate::config::{FetchParams, StealthExtras};
use crate::session::{page_hook, BrowserSession, SessionHooks};

/// The fetch parameters a scenario starts from.
fn params() -> FetchParams {
    FetchParams::default()
}

/// `(host, path)` pairs the fixture saw, in the corpus's shape.
fn received(fixture: &Fixture) -> Vec<(String, String)> {
    fixture.received_set()
}

/// Pretty-print a `(host, path)` list for an assertion message.
fn show(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(host, path)| format!("{host}{path}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// A. The response is unit 12's type, and the DOM is the rendered one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_rendered_dom_becomes_unit_12s_response_and_css_and_xpath_work() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    let response = session
        .fetch(&fixture.main_url("/"), params())
        .await
        .expect("the page is fetched");
    session.close().await;

    let golden = corpus().row("baseline").observed();

    // The type: this is literally `browser_http::response::Response`.
    let _: &browser_http::response::Response = &response;

    assert_eq!(response.status, golden.status);
    assert_eq!(response.reason, golden.reason);
    assert_eq!(response.encoding, golden.encoding);
    assert_eq!(
        response.url,
        with_port(&golden.url, fixture.port()),
        "page.url is the post-navigation URL"
    );

    let document = &response;
    assert_eq!(
        document.css("title::text").expect("a title").get(),
        golden.css("title").map(str::to_owned),
    );
    // `before` in the transport body, `rendered` after the page's own script
    // ran: this is the live DOM, not the bytes the server sent.
    assert_eq!(
        document.css("#title::text").expect("a title").get(),
        Some("rendered".to_owned()),
    );
    assert_eq!(golden.css("h1"), Some("rendered"));
    assert!(
        !response.body().windows(6).any(|w| w == b"before"),
        "the serialised DOM must not still say `before`",
    );

    assert_eq!(
        document.css(".price::text").expect("a price").get(),
        golden.css("price").map(str::to_owned),
    );
    assert_eq!(
        document
            .xpath("//h1[@id='title']/text()")
            .expect("the xpath runs")
            .get(),
        golden.xpath_title.clone(),
    );
    assert_eq!(
        document
            .xpath("//p[@class='price']/text()")
            .expect("the xpath runs")
            .get(),
        golden.xpath_price.clone(),
    );

    assert!(golden.body_is_html);
    assert!(response.body().starts_with(b"<html") || response.body().starts_with(b"<!DOCTYPE"));
}

// ---------------------------------------------------------------------------
// B. Resource blocking, measured at the socket
// ---------------------------------------------------------------------------

/// The blocking cases all have the same shape: fetch, then compare the set of
/// requests the *server* received against what the reference implementation's own run received.
async fn blocking_case(name: &str, configure: impl FnOnce(FetchParams) -> FetchParams, ads: bool) {
    let fixture = Fixture::start().await;
    let mut config = live_config(&fixture);
    if ads {
        config = config.block_ads(true);
    }
    let session = session_for(config).await;
    let response = session
        .fetch(&fixture.main_url("/"), configure(params()))
        .await
        .expect("the page is fetched");
    session.close().await;
    assert_eq!(response.status, 200);

    let observed = received(&fixture);
    let expected = corpus().row(name).received_pairs();
    assert_eq!(
        observed,
        expected,
        "\n{name}:\n  rust:   {}\n  python: {}",
        show(&observed),
        show(&expected),
    );
}

#[tokio::test]
async fn disable_resources_blocks_exactly_what_python_blocks() {
    require_browser!();
    blocking_case(
        "disable_resources",
        |p| FetchParams {
            disable_resources: Override::Set(true),
            ..p
        },
        false,
    )
    .await;
    // Belt and braces on the direction of the claim: the stylesheet, the image
    // and the font never reached the socket, while both scripts did.
    let fixture = corpus().row("disable_resources").received_pairs();
    let paths: HashSet<&str> = fixture.iter().map(|(_, path)| path.as_str()).collect();
    assert!(!paths.contains("/style.css"), "stylesheet blocked");
    assert!(!paths.contains("/pixel.gif"), "image blocked");
    assert!(!paths.contains("/font.woff2"), "font blocked");
    assert!(paths.contains("/app.js"), "script not blocked");
    assert!(paths.contains("/tracker.js"), "script not blocked");
}

#[tokio::test]
async fn block_ads_blocks_only_the_ad_host() {
    require_browser!();
    blocking_case("block_ads", |p| p, true).await;
    let expected = corpus().row("block_ads").received_pairs();
    assert!(
        !expected
            .iter()
            .any(|(host, _)| host == "ads.doubleclick.net"),
        "the ad host is refused",
    );
    assert!(
        expected.iter().any(|(_, path)| path == "/style.css"),
        "everything else still loads",
    );
}

#[tokio::test]
async fn blocked_domains_blocks_the_named_host_and_its_subdomains_only() {
    require_browser!();
    blocking_case(
        "blocked_domains_cdn",
        |p| FetchParams {
            blocked_domains: Override::Set(Some(HashSet::from(["cdn.test".to_owned()]))),
            ..p
        },
        false,
    )
    .await;
    // The fixture serves one script from `assets.cdn.test`, so the parent-domain
    // walk is measured at the socket rather than inferred: blocking `cdn.test`
    // must take the sub-domain with it, and must not touch the ad host.
    let expected = corpus().row("blocked_domains_cdn").received_pairs();
    assert!(
        !expected
            .iter()
            .any(|(host, _)| host == "assets.cdn.test" || host == "cdn.test"),
        "the sub-domain goes with its parent: {expected:?}",
    );
    assert!(
        expected
            .iter()
            .any(|(host, path)| host == "ads.doubleclick.net" && path == "/tracker.js"),
        "and nothing else is touched",
    );
    let baseline = corpus().row("baseline").received_pairs();
    assert!(
        baseline
            .iter()
            .any(|(host, path)| host == "assets.cdn.test" && path == "/sub.js"),
        "…which only means anything because the sub-domain loads without the block",
    );
}

#[tokio::test]
async fn resource_types_and_domains_block_together() {
    require_browser!();
    blocking_case(
        "blocked_domains_and_resources",
        |p| FetchParams {
            disable_resources: Override::Set(true),
            blocked_domains: Override::Set(Some(HashSet::from(["ads.doubleclick.net".to_owned()]))),
            ..p
        },
        false,
    )
    .await;
}

#[tokio::test]
async fn a_route_does_not_leak_into_the_next_request_on_the_same_tab() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    session
        .fetch(
            &fixture.main_url("/"),
            FetchParams {
                disable_resources: Override::Set(true),
                ..params()
            },
        )
        .await
        .expect("the blocked fetch");
    fixture.reset();
    session
        .fetch(&fixture.main_url("/"), params())
        .await
        .expect("the unblocked fetch");
    session.close().await;

    let observed = received(&fixture);
    let paths: HashSet<&str> = observed.iter().map(|(_, path)| path.as_str()).collect();
    for path in ["/style.css", "/pixel.gif", "/font.woff2"] {
        assert!(
            paths.contains(path),
            "{path} must load once the route is gone; saw {}",
            show(&observed),
        );
    }
}

// ---------------------------------------------------------------------------
// C. The wait modes resolve at the right moment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_waits_for_a_slow_sub_resource_and_load_dom_is_a_no_op() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    for (name, load_dom) in [("load_waits_for_the_image", true), ("load_dom_off", false)] {
        fixture.reset();
        let started = Instant::now();
        let response = session
            .fetch(
                &fixture.main_url("/slow-image"),
                FetchParams {
                    load_dom: Override::Set(load_dom),
                    ..params()
                },
            )
            .await
            .expect("the page is fetched");
        let elapsed = started.elapsed();
        let document = &response;
        let golden = corpus().row(name).observed();

        let dcl = document.css("#dcl::text").expect("a probe").get();
        let load = document.css("#load::text").expect("a probe").get();
        assert_eq!(
            dcl.as_deref(),
            golden.css("dcl"),
            "{name}: DOMContentLoaded"
        );
        assert_eq!(load.as_deref(), golden.css("load"), "{name}: load");
        assert_eq!(
            load.as_deref(),
            Some("yes"),
            "{name}: `page.goto` waits for `load`, which cannot fire before the \
             900 ms image settles — `load_dom` only ever adds a state `load` \
             already implies",
        );
        assert!(
            elapsed >= Duration::from_millis(850),
            "{name}: the fetch took {elapsed:?}, which is too fast for a 900 ms image",
        );
        assert!(
            fixture.saw("cdn.test", "/slow-pixel.gif"),
            "{name}: the slow image really was requested",
        );
    }
    session.close().await;
}

#[tokio::test]
async fn network_idle_is_the_difference_between_zero_and_all_of_the_background_xhrs() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    for (name, idle, expected_hits) in [
        ("network_idle_off", false, 0usize),
        ("network_idle_on", true, 4usize),
    ] {
        fixture.reset();
        let response = session
            .fetch(
                &fixture.main_url("/xhr-storm"),
                FetchParams {
                    network_idle: Override::Set(idle),
                    ..params()
                },
            )
            .await
            .expect("the page is fetched");
        let document = &response;
        let done = document.css("#done::text").expect("a probe").get();
        let golden = corpus().row(name).observed();

        assert_eq!(done.as_deref(), golden.css("done"), "{name}: settled XHRs");
        assert_eq!(
            done.as_deref(),
            Some(if idle { "4" } else { "0" }),
            "{name}: networkidle is 'nothing in flight for 500 ms', so it cannot \
             fire until every one of the four 700 ms XHRs has settled",
        );
        assert_eq!(
            fixture.hits("/api/slow-item"),
            expected_hits,
            "{name}: what the server actually served",
        );
    }
    session.close().await;
}

// ---------------------------------------------------------------------------
// D. `wait_selector` and `wait_selector_state`
// ---------------------------------------------------------------------------

/// The `/wait-states` fixture's milestones, in milliseconds.
const MILESTONE_VISIBLE: u64 = 1_000;
const MILESTONE_DETACHED: u64 = 2_500;
const MILESTONE_BORN: u64 = 4_000;

/// How long after a milestone the snapshot may land.
///
/// Playwright's own runs came in ~300 ms late; the polling loop here runs every
/// 50 ms. The window is generous on purpose — the assertion that matters is
/// *which* milestone the wait resolved after, and the milestones are 1.5 s
/// apart.
const MILESTONE_SLACK: u64 = 1_200;

#[tokio::test]
async fn wait_selector_defaults_to_attached_and_waits_for_a_scripted_element() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    // Without the wait, the element injected at 600 ms is simply not there.
    let bare = session
        .fetch(&fixture.main_url("/late"), params())
        .await
        .expect("the bare fetch");
    assert!(
        bare.css("#ready").expect("a selector").is_empty(),
        "the element only exists after 600 ms",
    );

    let waited = session
        .fetch(
            &fixture.main_url("/late"),
            FetchParams {
                wait_selector: Override::Set(Some("#ready".to_owned())),
                ..params()
            },
        )
        .await
        .expect("the waited fetch");
    session.close().await;

    let golden = corpus().row("wait_selector_attached").observed();
    let document = &waited;
    assert_eq!(
        document.css("#ready::text").expect("a probe").get(),
        golden.css("ready").map(str::to_owned),
    );
    assert_eq!(
        document
            .css("#ready::attr(data-sku)")
            .expect("a probe")
            .get(),
        golden.css("sku").map(str::to_owned),
    );
    assert_eq!(
        document.css("#title::text").expect("a probe").get(),
        golden.css("h1").map(str::to_owned),
    );
}

/// Drive `/wait-states` once and report the `#clock` value baked into the
/// snapshot, i.e. roughly when the page was serialised.
async fn wait_state_clock(
    session: &BrowserSession,
    fixture: &Fixture,
    selector: &str,
    state: WaitState,
) -> (u64, browser_http::response::Response) {
    let response = session
        .fetch(
            &fixture.main_url("/wait-states"),
            FetchParams {
                wait_selector: Override::Set(Some(selector.to_owned())),
                wait_selector_state: Override::Set(state),
                ..params()
            },
        )
        .await
        .expect("the page is fetched");
    let clock = response
        .css("#clock::text")
        .expect("a clock")
        .get()
        .expect("the clock has a value")
        .parse()
        .expect("the clock is a number");
    (clock, response)
}

#[tokio::test]
async fn every_wait_selector_state_resolves_at_its_own_milestone() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    // No wait at all: the snapshot is taken as soon as `load` fires, before any
    // milestone. `#hidden-then-visible` is still `display:none` and
    // `#present-then-detached` is still there.
    let bare = session
        .fetch(&fixture.main_url("/wait-states"), params())
        .await
        .expect("the bare fetch");
    let bare_doc = &bare;
    let bare_clock: u64 = bare_doc
        .css("#clock::text")
        .expect("a clock")
        .get()
        .expect("a value")
        .parse()
        .expect("a number");
    let golden_bare = corpus().row("wait_states_no_wait").observed();
    assert!(
        golden_bare.clock_ms() < MILESTONE_VISIBLE,
        "Python's own bare snapshot landed before the first milestone, at {} ms",
        golden_bare.clock_ms(),
    );
    assert!(
        bare_clock < MILESTONE_VISIBLE,
        "no wait: snapshot at {bare_clock} ms, before the {MILESTONE_VISIBLE} ms milestone",
    );
    assert_eq!(
        bare_doc
            .css("#hidden-then-visible::attr(style)")
            .expect("a probe")
            .get()
            .as_deref(),
        golden_bare.css("toggler_style"),
        "the toggler is still hidden",
    );
    assert!(!bare_doc
        .css("#present-then-detached")
        .expect("ok")
        .is_empty());
    assert!(bare_doc.css("#stamp-visible").expect("ok").is_empty());

    // visible: resolves at the 1000 ms milestone, before the 2500 ms one.
    let (clock, response) = wait_state_clock(
        &session,
        &fixture,
        "#hidden-then-visible",
        WaitState::Visible,
    )
    .await;
    let document = &response;
    assert!(
        (MILESTONE_VISIBLE..MILESTONE_DETACHED).contains(&clock),
        "visible: snapshot at {clock} ms, expected [{MILESTONE_VISIBLE}, {MILESTONE_DETACHED})",
    );
    assert!(
        clock < MILESTONE_VISIBLE + MILESTONE_SLACK,
        "visible: {clock} ms is late"
    );
    assert!(!document.css("#stamp-visible").expect("ok").is_empty());
    assert!(document.css("#stamp-detached").expect("ok").is_empty());
    assert_eq!(
        document
            .css("#hidden-then-visible::attr(style)")
            .expect("a probe")
            .get()
            .as_deref(),
        corpus()
            .row("wait_selector_visible")
            .observed()
            .css("toggler_style"),
    );

    // hidden: `#present-then-detached` is visible until 2500 ms, and
    // Playwright's `hidden` is satisfied by *absent* too — so it resolves when
    // the node is removed.
    let (clock, document_hidden) = wait_state_clock(
        &session,
        &fixture,
        "#present-then-detached",
        WaitState::Hidden,
    )
    .await;
    let document_hidden = &document_hidden;
    assert!(
        (MILESTONE_DETACHED..MILESTONE_BORN).contains(&clock),
        "hidden: snapshot at {clock} ms, expected [{MILESTONE_DETACHED}, {MILESTONE_BORN})",
    );
    assert!(document_hidden
        .css("#present-then-detached")
        .expect("ok")
        .is_empty());
    assert!(!document_hidden
        .css("#stamp-detached")
        .expect("ok")
        .is_empty());
    assert!(document_hidden.css("#born-late").expect("ok").is_empty());

    // detached: same milestone, different predicate.
    let (clock, document_detached) = wait_state_clock(
        &session,
        &fixture,
        "#present-then-detached",
        WaitState::Detached,
    )
    .await;
    let document_detached = &document_detached;
    assert!(
        (MILESTONE_DETACHED..MILESTONE_BORN).contains(&clock),
        "detached: snapshot at {clock} ms, expected [{MILESTONE_DETACHED}, {MILESTONE_BORN})",
    );
    assert!(document_detached.css("#born-late").expect("ok").is_empty());

    // attached, on an element that does not exist yet: the 4000 ms milestone.
    let (clock, document_born) =
        wait_state_clock(&session, &fixture, "#born-late", WaitState::Attached).await;
    let document_born = &document_born;
    assert!(
        clock >= MILESTONE_BORN,
        "attached: snapshot at {clock} ms, expected >= {MILESTONE_BORN}",
    );
    assert!(
        clock < MILESTONE_BORN + MILESTONE_SLACK,
        "attached: {clock} ms is late"
    );
    assert!(!document_born.css("#born-late").expect("ok").is_empty());

    session.close().await;
}

#[tokio::test]
async fn a_wait_selector_that_never_matches_is_logged_not_fatal() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    let started = Instant::now();
    let response = session
        .fetch(
            &fixture.main_url("/late"),
            FetchParams {
                wait_selector: Override::Set(Some("#never-appears".to_owned())),
                timeout_ms: Override::Set(2_000.0),
                ..params()
            },
        )
        .await
        .expect("a selector that never matches must not fail the request");
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus()
        .row("wait_selector_missing_is_logged_not_fatal")
        .observed();
    assert_eq!(response.status, golden.status);
    assert_eq!(
        response.css("#ready::text").expect("a probe").get(),
        golden.css("ready").map(str::to_owned),
        "the page is still returned, with whatever it had at the deadline",
    );
    assert!(
        elapsed >= Duration::from_millis(1_800),
        "the wait really consumed its 2 s budget (took {elapsed:?})",
    );
}

// ---------------------------------------------------------------------------
// E. History, cookies, status, transport bodies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_redirect_chain_is_rebuilt_from_the_redirect_responses() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/hop1"), params())
        .await
        .expect("the redirect chain is followed");
    session.close().await;

    let golden = corpus().row("redirect_chain").observed();
    let expected: Vec<(String, u16)> = golden
        .history_pairs()
        .into_iter()
        .map(|(url, status)| (with_port(&url, fixture.port()), status))
        .collect();
    let observed: Vec<(String, u16)> = response
        .history
        .iter()
        .map(|hop| (hop.url.clone(), hop.status))
        .collect();
    assert_eq!(observed, expected);
    assert_eq!(response.url, with_port(&golden.url, fixture.port()));
    assert_eq!(response.status, golden.status);
    assert_eq!(
        response.css("#c::text").expect("a probe").get(),
        golden.css("cookie_marker").map(str::to_owned),
    );
    // Every hop carries its own reason phrase…
    let reasons: Vec<String> = response
        .history
        .iter()
        .map(|hop| hop.reason.clone())
        .collect();
    assert_eq!(reasons, golden.history_reasons);
    assert_eq!(reasons, ["Found", "Moved Permanently"]);
    // …and its own headers, including the `Location` that points at the next.
    for (hop, expected) in response.history.iter().zip(&golden.history_header_names) {
        let mut names: Vec<String> = hop.headers.keys().map(|k| k.to_lowercase()).collect();
        names.sort();
        names.dedup();
        assert_eq!(&names, expected, "hop {}", hop.url);
    }
}

#[tokio::test]
async fn set_cookie_headers_reach_the_response() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/cookie"), params())
        .await
        .expect("the page is fetched");
    session.close().await;

    let golden = corpus().row("cookies").observed();
    for (name, value) in &golden.cookies {
        assert_eq!(
            response.cookies.get(name).map(String::as_str),
            Some(value.as_str()),
            "cookie {name}",
        );
    }
    assert!(
        golden.cookies.contains_key("alpha"),
        "the corpus really saw cookies"
    );
}

#[tokio::test]
async fn a_non_ok_status_and_its_reason_phrase_survive() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/teapot"), params())
        .await
        .expect("a 418 is still a response");
    session.close().await;

    let golden = corpus().row("non_ok_status").observed();
    assert_eq!(response.status, golden.status);
    assert_eq!(response.status, 418);
    assert_eq!(response.reason, golden.reason);
    assert_eq!(
        response.css("#t::text").expect("a probe").get(),
        golden.css("teapot").map(str::to_owned),
    );
}

#[tokio::test]
async fn a_non_html_response_keeps_its_transport_body() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/plain.json"), params())
        .await
        .expect("the page is fetched");
    session.close().await;

    let golden = corpus().row("non_html_body").observed();
    assert!(!golden.body_is_html);
    assert_eq!(response.body().len(), golden.body_len);
    assert_eq!(
        String::from_utf8_lossy(response.body()),
        r#"{"transport":"body"}"#,
        "a non-HTML content type takes `response.body()`, not the serialised DOM",
    );
}

// ---------------------------------------------------------------------------
// F. `capture_xhr`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capture_xhr_collects_only_the_matching_responses() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture).capture_xhr("/api/(items|other)")).await;
    let fetched = session
        .fetch_detailed(
            &fixture.main_url("/"),
            FetchParams {
                network_idle: Override::Set(true),
                ..params()
            },
        )
        .await
        .expect("the page is fetched");
    session.close().await;

    let golden = corpus().row("capture_xhr").observed();
    let mut urls: Vec<String> = fetched.captured_xhr.iter().map(|r| r.url.clone()).collect();
    urls.sort();
    let expected: Vec<String> = golden
        .captured_xhr
        .iter()
        .map(|url| with_port(url, fixture.port()))
        .collect();
    assert_eq!(urls, expected);

    let mut bodies: Vec<String> = fetched
        .captured_xhr
        .iter()
        .map(|r| String::from_utf8_lossy(r.body()).into_owned())
        .collect();
    bodies.sort();
    assert_eq!(bodies, golden.captured_xhr_bodies);

    let mut statuses: Vec<u16> = fetched.captured_xhr.iter().map(|r| r.status).collect();
    statuses.sort_unstable();
    assert_eq!(statuses, golden.captured_xhr_status);

    // The page itself is never also an XHR capture, and the resolved body made
    // it into the DOM because `network_idle` waited for it.
    assert_eq!(
        fetched.css("#late::text").expect("a probe").get(),
        golden.css("late").map(str::to_owned),
    );
}

// ---------------------------------------------------------------------------
// G. Timeouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_navigation_that_never_commits_fails_at_the_configured_timeout() {
    require_browser!();
    let fixture = Fixture::start().await;
    // `/slow` accepts the connection and never answers, so the navigation never
    // commits. Python raises `Page.goto: Timeout 3000ms exceeded.` after
    // exactly `timeout`; anything that waits for the CDP command budget instead
    // would hang for two minutes.
    let session = session_for(live_config(&fixture).timeout_ms(3_000.0).retries(1)).await;

    let started = Instant::now();
    let outcome = session.fetch(&fixture.main_url("/slow"), params()).await;
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus().row("navigation_timeout");
    assert!(
        golden
            .error
            .as_deref()
            .is_some_and(|text| text.contains("Timeout 3000ms exceeded")),
        "the corpus recorded Python's timeout",
    );
    let error = outcome.expect_err("a navigation that never commits must fail");
    assert!(
        elapsed < Duration::from_secs(15),
        "the failure took {elapsed:?}; `timeout` must bound `Page.navigate`, \
         not the 120 s CDP command budget",
    );
    assert!(
        elapsed >= Duration::from_millis(2_800),
        "it must not give up before `timeout` either (took {elapsed:?})",
    );
    let text = error.to_string();
    assert!(
        text.contains("3000") && text.to_lowercase().contains("timeout"),
        "the error names the budget it blew: {text}",
    );
    assert!(
        fixture.saw("main.test", "/slow"),
        "the request really went out"
    );
}

// ---------------------------------------------------------------------------
// H. Retries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_first_connection_is_retried_on_a_fresh_tab() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture).retries(2)).await;
    let response = session
        .fetch(&fixture.main_url("/flaky"), params())
        .await
        .expect("the second attempt succeeds");
    session.close().await;

    let golden = corpus().row("retry_after_a_dropped_connection").observed();
    assert_eq!(response.status, golden.status);
    assert_eq!(
        response.css("#title::text").expect("a probe").get(),
        golden.css("h1").map(str::to_owned),
    );
    assert_eq!(fixture.hits("/flaky"), 2, "one dropped attempt, one served");
}

// ---------------------------------------------------------------------------
// I. Tab reuse and the per-request reset
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_per_request_reset_runs_with_the_call_counts_test_page_reuse_pins() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    // Request 1: with extra headers *and* a route (so `route` is installed).
    session
        .fetch(
            &fixture.main_url("/echo"),
            FetchParams {
                extra_headers: Override::Set(Some(
                    [("x-probe".to_owned(), "first".to_owned())]
                        .into_iter()
                        .collect(),
                )),
                disable_resources: Override::Set(true),
                ..params()
            },
        )
        .await
        .expect("the first fetch");

    let first_page = session
        .pooled_pages()
        .into_iter()
        .next()
        .expect("the tab stayed in the pool");

    // Request 2: no headers and no route, on the same tab.
    let second = session
        .fetch(&fixture.main_url("/echo"), params())
        .await
        .expect("the second fetch");

    let pages = session.pooled_pages();
    assert_eq!(pages.len(), 1, "the tab was reused, not replaced");
    let counts = pages[0].control_counts();
    assert!(
        std::ptr::eq(pages[0].raw(), first_page.raw()),
        "literally the same tab",
    );

    // `test_page_reuse.py`:
    //   assert session.context.new_page.call_count == 1
    //   first_page.set_default_timeout.assert_called_with(2000)
    //   first_page.set_extra_http_headers.assert_called_with({})
    //   assert first_page.unroute_all.call_count == 2
    //   first_page.route.assert_called_once()
    assert_eq!(counts.navigation_timeout, 2, "once per request");
    assert_eq!(counts.default_timeout, 2, "once per request");
    assert_eq!(
        counts.extra_headers, 2,
        "once per request, `{{}}` the second time"
    );
    assert_eq!(
        counts.unroute_all, 2,
        "unconditional: it is what stops the previous route from surviving",
    );
    assert_eq!(
        counts.route, 1,
        "installed only for the request that asked for blocking",
    );
    assert_eq!(
        counts.is_closed, 2,
        "`_page_generator`'s `finally` probes the tab once per completed request",
    );
    assert_eq!(counts.close, 0, "a healthy tab is never closed");

    // And the headers really were cleared: `/echo` reflects `x-probe`.
    let probe = second.css("#probe::text").expect("a probe").get();
    assert_eq!(probe.as_deref(), Some("absent"), "the header did not leak");
    assert_eq!(
        fixture.header_of("/echo", "x-probe").as_deref(),
        None,
        "and the server did not see it either",
    );

    let golden = corpus().row("tab_reuse_two_requests");
    assert_eq!(golden.kind.as_deref(), Some("sequence"));
    let python_stats = golden
        .pool_stats
        .as_ref()
        .expect("the corpus recorded get_pool_stats()");
    let stats = session.pool_stats();
    assert_eq!(
        i64::try_from(stats.total_pages).ok(),
        python_stats.get("total_pages").copied()
    );
    assert_eq!(
        i64::try_from(stats.busy_pages).ok(),
        python_stats.get("busy_pages").copied()
    );
    session.close().await;
    assert_eq!(session.pool_stats().total_pages, 0);
}

#[tokio::test]
async fn per_request_headers_match_pythons_echo() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    let first = session
        .fetch(
            &fixture.main_url("/echo"),
            FetchParams {
                extra_headers: Override::Set(Some(
                    [("x-probe".to_owned(), "first".to_owned())]
                        .into_iter()
                        .collect(),
                )),
                ..params()
            },
        )
        .await
        .expect("the first fetch");
    let second = session
        .fetch(&fixture.main_url("/echo"), params())
        .await
        .expect("the second fetch");
    session.close().await;

    let golden = corpus().row("tab_reuse_two_requests");
    assert_eq!(golden.observations.len(), 2);
    let python_order: Vec<(String, String)> = golden
        .received_order
        .iter()
        .filter_map(|pair| Some((pair.first()?.clone(), pair.get(1)?.clone())))
        .filter(|(_, path)| path != "/favicon.ico")
        .collect();
    assert_eq!(
        fixture
            .received()
            .into_iter()
            .filter(|(_, path)| path != "/favicon.ico")
            .collect::<Vec<_>>(),
        python_order,
        "the same two requests, in the same order",
    );
    let probe = |response: &browser_http::response::Response| {
        response.css("#probe::text").expect("a probe").get()
    };
    assert_eq!(probe(&first).as_deref(), Some("first"));
    assert_eq!(probe(&second).as_deref(), Some("absent"));
    assert_eq!(first.body().len(), golden.observations[0].body_len);
    assert_eq!(second.body().len(), golden.observations[1].body_len);
}

// ---------------------------------------------------------------------------
// J. Hooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_hooks_run_in_order_and_a_failing_page_action_is_not_fatal() {
    require_browser!();
    let fixture = Fixture::start().await;

    let setup_calls = Arc::new(AtomicUsize::new(0));
    let action_calls = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(parking_lot::Mutex::new(Vec::<&'static str>::new()));

    let setup_counter = Arc::clone(&setup_calls);
    let setup_order = Arc::clone(&order);
    let action_counter = Arc::clone(&action_calls);
    let action_order = Arc::clone(&order);

    let hooks = SessionHooks::new()
        .page_setup(page_hook(move |_page| {
            let counter = Arc::clone(&setup_counter);
            let order = Arc::clone(&setup_order);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                order.lock().push("setup");
                Ok(())
            }
        }))
        .page_action(page_hook(move |page| {
            let counter = Arc::clone(&action_counter);
            let order = Arc::clone(&action_order);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                order.lock().push("action");
                // The hook can drive the page…
                let title = page
                    .evaluate_in_main_frame("document.title")
                    .await
                    .expect("the hook can evaluate");
                assert_eq!(title.as_str(), Some("Multi"));
                // …and then fail. A failing hook is logged, never fatal.
                Err(crate::error::BrowserError::JavaScript {
                    message: "deliberate".to_owned(),
                })
            }
        }));

    let session = session_with_hooks(live_config(&fixture), hooks).await;
    let response = session
        .fetch(&fixture.main_url("/"), params())
        .await
        .expect("a failing page_action must not fail the request");
    session.close().await;

    assert_eq!(setup_calls.load(Ordering::SeqCst), 1);
    assert_eq!(action_calls.load(Ordering::SeqCst), 1);
    assert_eq!(order.lock().as_slice(), ["setup", "action"]);
    assert_eq!(response.status, 200);
}

// ---------------------------------------------------------------------------
// K. Cloudflare — against a STRUCTURAL FIXTURE, not against Cloudflare
// ---------------------------------------------------------------------------

/// The Turnstile widget's position in `/challenge`, from the fixture's inline
/// style.
const WIDGET_X: f64 = 100.0;
const WIDGET_Y: f64 = 200.0;

/// The stubborn challenge's widget position.
const STUBBORN_X: f64 = 40.0;
const STUBBORN_Y: f64 = 120.0;

fn stealth_solver_config(fixture: &Fixture) -> crate::config::BrowserConfig {
    live_config(fixture).stealth(StealthExtras::new().solve_cloudflare(true))
}

fn solve_params() -> FetchParams {
    FetchParams {
        solve_cloudflare: Override::Set(true),
        ..params()
    }
}

/// Read the clicks the fixture recorded into its own DOM.
///
/// They are recorded in the DOM rather than on `window` on purpose: an isolated
/// world has its own `window`, so a `window.__clicks` array would be invisible
/// to this driver. That the clicks *are* visible here, and only through the
/// DOM, is the isolation working.
fn recorded_clicks(response: &browser_http::response::Response) -> Vec<(f64, f64)> {
    let document = &response;
    document
        .css("span.click")
        .expect("a selector")
        .iter()
        .map(|node| {
            let x = node
                .attr("data-x")
                .expect("an x")
                .parse()
                .expect("a number");
            let y = node.attr("data-y").expect("a y").parse().expect("a number");
            (x, y)
        })
        .collect()
}

#[tokio::test]
async fn the_solver_clears_a_turnstile_shaped_fixture_at_pythons_click_offsets() {
    // NOT Cloudflare: a local page that reproduces the Turnstile DOM
    // (`cType: 'interactive'`, `.main-content p+div>div>div`) and the state
    // transition the solver keys on (the marker disappears once the widget is
    // clicked).
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(stealth_solver_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/challenge"), solve_params())
        .await
        .expect("the solver returns a response");
    session.close().await;

    let clicks = recorded_clicks(&response);
    assert_eq!(
        clicks.len(),
        1,
        "one click for a challenge that clears at once"
    );
    let (x, y) = clicks[0];
    assert!(
        (WIDGET_X + 26.0..=WIDGET_X + 28.0).contains(&x),
        "x = {x}, expected `box.x + randint(26, 28)`",
    );
    assert!(
        (WIDGET_Y + 25.0..=WIDGET_Y + 27.0).contains(&y),
        "y = {y}, expected `box.y + randint(25, 27)`",
    );

    // The marker script is gone, so `_challenge_cleared` is true and the solver
    // returned through its "Cloudflare captcha is solved" branch.
    let document = &response;
    assert!(document.css("#marker").expect("ok").is_empty());
    assert_eq!(
        crate::cloudflare::detect_cloudflare(&String::from_utf8_lossy(response.body())),
        None,
    );
}

#[tokio::test]
async fn a_stubborn_challenge_is_re_attempted_and_each_attempt_clicks_once() {
    // Structural fixture again: the widget only lets go on the *second*
    // mousedown, so the solver has to come back round its attempt loop.
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(stealth_solver_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/challenge-stubborn"), solve_params())
        .await
        .expect("the solver returns a response");
    session.close().await;

    let clicks = recorded_clicks(&response);
    assert_eq!(
        clicks.len(),
        2,
        "attempt 1 clicks and the marker survives; attempt 2 clicks and it goes",
    );
    for (x, y) in &clicks {
        assert!(
            (STUBBORN_X + 26.0..=STUBBORN_X + 28.0).contains(x),
            "x = {x}"
        );
        assert!(
            (STUBBORN_Y + 25.0..=STUBBORN_Y + 27.0).contains(y),
            "y = {y}"
        );
    }
    assert!(
        response.css("#marker").expect("ok").is_empty(),
        "the second attempt cleared it",
    );
}

#[tokio::test]
async fn a_non_interactive_wait_page_is_polled_until_it_clears() {
    // Structural fixture: `cType: 'non-interactive'` with a marker that removes
    // itself after 1.5 s. Python's branch polls `wait_for_timeout(1000)` +
    // `wait_for_load_state()` until `_detect_cloudflare` stops saying
    // `non-interactive`, and never clicks.
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(stealth_solver_config(&fixture)).await;
    let started = Instant::now();
    let response = session
        .fetch(&fixture.main_url("/challenge-wait"), solve_params())
        .await
        .expect("the solver returns a response");
    let elapsed = started.elapsed();
    session.close().await;

    assert!(
        elapsed >= Duration::from_millis(1_400),
        "the solver waited for the page to clear (took {elapsed:?})",
    );
    assert!(
        recorded_clicks(&response).is_empty(),
        "a non-interactive challenge is never clicked",
    );
    assert!(response.css("#marker").expect("ok").is_empty());
    assert_eq!(
        crate::cloudflare::detect_cloudflare(&String::from_utf8_lossy(response.body())),
        None,
    );
}

// ---------------------------------------------------------------------------
// L. The referer, and the two timeouts Python treats differently
// ---------------------------------------------------------------------------

/// Fetch `/echo` and report what the *server* saw in `Referer`.
async fn referer_seen(
    fixture: &Fixture,
    session: &BrowserSession,
    label: &str,
    params: FetchParams,
) -> String {
    fixture.reset();
    let response = session
        .fetch(&fixture.main_url("/echo"), params)
        .await
        .unwrap_or_else(|error| panic!("the echo page is fetched (case {label}): {error}"));
    let in_dom = response
        .css("#referer::text")
        .expect("a probe")
        .get()
        .expect("the echo page always renders the span");
    let at_socket = fixture
        .header_of("/echo", "referer")
        .unwrap_or_else(|| "absent".to_owned());
    assert_eq!(in_dom, at_socket, "the page and the socket agree");
    in_dom
}

#[tokio::test]
async fn google_search_sends_the_referer_across_an_https_to_http_downgrade() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    // `google_search` defaults on. `https://www.google.com/` -> `http://…` is
    // exactly the downgrade Chrome's default `strict-origin-when-cross-origin`
    // policy strips, so this is also the live proof that `Page.navigate` is
    // sent with `referrerPolicy: "unsafeUrl"`.
    let on = referer_seen(&fixture, &session, "google-on", params()).await;
    assert_eq!(
        on.as_str(),
        corpus()
            .row("google_search_referer")
            .observed()
            .expect_css("referer"),
    );
    assert_eq!(on, "https://www.google.com/");

    let off = referer_seen(
        &fixture,
        &session,
        "google-off",
        FetchParams {
            google_search: Override::Set(false),
            ..params()
        },
    )
    .await;
    assert_eq!(
        off.as_str(),
        corpus()
            .row("google_search_off")
            .observed()
            .expect_css("referer"),
    );
    assert_eq!(off, "absent");

    // `referer` in `extra_headers` suppresses the synthetic one.
    let explicit = referer_seen(
        &fixture,
        &session,
        "explicit-referer",
        FetchParams {
            extra_headers: Override::Set(Some(
                [(
                    "referer".to_owned(),
                    "https://example.test/from-header".to_owned(),
                )]
                .into_iter()
                .collect(),
            )),
            ..params()
        },
    )
    .await;
    assert_eq!(
        explicit.as_str(),
        corpus()
            .row("an_explicit_referer_beats_google_search")
            .observed()
            .expect_css("referer"),
    );
    assert_eq!(explicit, "https://example.test/from-header");

    session.close().await;
}

#[tokio::test]
async fn a_document_that_commits_but_never_reaches_load_fails_at_the_timeout_too() {
    require_browser!();
    let fixture = Fixture::start().await;
    // `/never-loads` commits immediately and then holds `load` open forever on
    // an image served by the `hang` endpoint. Playwright raises
    // `Page.goto: Timeout 3000ms exceeded.` — the *same* error as the
    // never-commits case, because one budget covers both halves of `goto`.
    let session = session_for(live_config(&fixture).timeout_ms(3_000.0).retries(1)).await;

    let started = Instant::now();
    let outcome = session
        .fetch(&fixture.main_url("/never-loads"), params())
        .await;
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus().row("load_never_fires");
    assert!(
        golden
            .error
            .as_deref()
            .is_some_and(|text| text.contains("Page.goto: Timeout 3000ms exceeded")),
        "the corpus recorded Python's timeout",
    );
    let error = outcome.expect_err("a document that never loads must fail");
    assert!(
        (Duration::from_millis(2_800)..Duration::from_secs(9)).contains(&elapsed),
        "the failure took {elapsed:?}, expected ~3 s — one budget for the whole \
         `goto`, not one per half",
    );
    assert!(error.to_string().contains("3000"), "{error}");
    assert!(
        fixture.saw("cdn.test", "/slow"),
        "the stuck image really went out"
    );
}

#[tokio::test]
async fn a_networkidle_that_can_never_be_satisfied_is_swallowed_not_raised() {
    require_browser!();
    let fixture = Fixture::start().await;
    // Python: `_wait_for_networkidle` is `try: … except (PlaywrightError,
    // Exception): pass`. A page whose XHR never settles must therefore still
    // produce a response, after the budget rather than instead of one.
    let session = session_for(live_config(&fixture).timeout_ms(4_000.0)).await;

    let started = Instant::now();
    let response = session
        .fetch(
            &fixture.main_url("/hanging-xhr"),
            FetchParams {
                network_idle: Override::Set(true),
                ..params()
            },
        )
        .await
        .expect("a networkidle that never fires is not an error");
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus().row("network_idle_with_a_hanging_xhr").observed();
    assert_eq!(response.status, golden.status);
    assert_eq!(
        response.css("#late::text").expect("a probe").get(),
        golden.css("late").map(str::to_owned),
        "the XHR never settled, so the DOM still says `pending`",
    );
    assert!(
        elapsed >= Duration::from_millis(3_500),
        "the wait really consumed its budget (took {elapsed:?})",
    );
    assert!(
        elapsed < Duration::from_secs(12),
        "…and no more than one budget (took {elapsed:?})",
    );
}

#[tokio::test]
async fn the_wait_parameter_idles_after_everything_else() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    let baseline = Instant::now();
    session
        .fetch(&fixture.main_url("/late"), params())
        .await
        .expect("the bare fetch");
    let bare = baseline.elapsed();

    let started = Instant::now();
    let waited = session
        .fetch(
            &fixture.main_url("/late"),
            FetchParams {
                wait_ms: Override::Set(900.0),
                ..params()
            },
        )
        .await
        .expect("the waited fetch");
    let elapsed = started.elapsed();
    session.close().await;

    assert!(
        bare < Duration::from_millis(900),
        "the bare fetch of the same page is well under the wait ({bare:?})",
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "`wait=900` sleeps after everything else finishes ({bare:?} -> {elapsed:?})",
    );
    let golden = corpus().row("wait_parameter").observed();
    assert_eq!(
        waited.css("#ready::text").expect("a probe").get(),
        golden.css("ready").map(str::to_owned),
        "the 600 ms element is there because the wait outlasted it",
    );
}

#[tokio::test]
async fn a_hanging_favicon_does_not_stop_networkidle_from_firing() {
    require_browser!();
    let fixture = Fixture::start().await;
    // Playwright excludes favicon requests from the in-flight set
    // (`Request._isFavicon = url.endsWith('/favicon.ico')`). `/favicon-idle`
    // hangs one such request forever and settles one ordinary XHR, so the two
    // outcomes are far apart: idle shortly after the XHR, or never.
    let session = session_for(live_config(&fixture).timeout_ms(8_000.0)).await;

    let started = Instant::now();
    let response = session
        .fetch(
            &fixture.main_url("/favicon-idle"),
            FetchParams {
                network_idle: Override::Set(true),
                ..params()
            },
        )
        .await
        .expect("the page is fetched");
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus()
        .row("network_idle_ignores_a_hanging_favicon")
        .observed();
    assert_eq!(
        response.css("#late::text").expect("a probe").get(),
        golden.css("late").map(str::to_owned),
        "the ordinary XHR settled and networkidle waited for it",
    );
    assert_eq!(golden.css("late"), Some(r#"{"items":[1,2,3]}"#));
    assert!(
        fixture.saw("api.test", "/icons/favicon.ico"),
        "the hanging favicon really was requested",
    );
    assert!(
        elapsed < Duration::from_millis(6_000),
        "networkidle fired without waiting out the 8 s budget (took {elapsed:?}), \
         so the favicon was excluded from the in-flight count",
    );
}

// ---------------------------------------------------------------------------
// M. Navigations that never become a document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocking_the_document_itself_aborts_the_navigation_with_err_failed() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture).retries(1)).await;

    let outcome = session
        .fetch(
            &fixture.main_url("/"),
            FetchParams {
                blocked_domains: Override::Set(Some(HashSet::from(["main.test".to_owned()]))),
                ..params()
            },
        )
        .await;
    session.close().await;

    let golden = corpus().row("the_document_itself_is_blocked");
    // Python: `Error: Page.goto: net::ERR_FAILED at …` — Playwright's
    // `route.abort()` defaults to the error code `"failed"`, which is why this
    // crate's `ABORT_ERROR_REASON` is `Failed` and not `BlockedByClient`.
    assert!(
        golden
            .error
            .as_deref()
            .is_some_and(|text| text.contains("net::ERR_FAILED")),
        "the corpus recorded Playwright's abort reason: {:?}",
        golden.error,
    );
    let error = outcome.expect_err("a blocked document is a failed navigation");
    let text = error.to_string();
    assert!(
        text.contains("net::ERR_FAILED"),
        "the port must report the same net error, got: {text}",
    );
    assert!(
        !text.contains("BLOCKED_BY_CLIENT"),
        "`BlockedByClient` is the reason Playwright does *not* use: {text}",
    );
    assert!(
        !fixture.saw("main.test", "/"),
        "and the request never reached the socket",
    );
}

/// **Declared divergence** — see `docs/DIVERGENCES.md`, "a navigation that turns
/// into a download".
///
/// Playwright reads `isDownload` off the `Page.navigate` reply and raises
/// `Page.goto: Download is starting` at once. `chromiumoxide` withholds that
/// reply until its own navigation watcher sees a `load` lifecycle event, and a
/// download never commits a document, so the reply never arrives and the port
/// falls back to the navigation-timeout bound instead. Both engines fail the
/// request; only the message and the latency differ.
///
/// This test asserts the **divergent** behaviour on purpose. If a driver
/// upgrade ever delivers the reply, it fails — and whoever sees that should
/// restore the `DOWNLOAD_STARTING` branch's assertion and delete the
/// divergence row, not weaken this test.
#[tokio::test]
async fn a_download_is_a_declared_divergence_not_pythons_download_is_starting() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture).retries(1).timeout_ms(5_000.0)).await;

    let started = Instant::now();
    let outcome = session
        .fetch(&fixture.main_url("/download"), params())
        .await;
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus().row("a_download_is_not_a_navigation");
    assert!(
        golden
            .error
            .as_deref()
            .is_some_and(|text| text.contains("Download is starting")),
        "the corpus recorded Playwright's message: {:?}",
        golden.error,
    );
    let error = outcome.expect_err("a download is not a page, in either engine");
    let text = error.to_string();
    assert!(
        !text.contains(crate::cdp::DOWNLOAD_STARTING),
        "THE DIVERGENCE HAS CLOSED: the driver now delivers `isDownload`, so the \
         `DOWNLOAD_STARTING` branch of `CdpPage::goto` fires. Re-point this test \
         at Python's behaviour and drop the row from docs/DIVERGENCES.md. Got: {text}",
    );
    assert!(
        text.contains("5000") && text.contains("page.goto"),
        "the fallback is the navigation-timeout bound: {text}",
    );
    assert!(
        (Duration::from_millis(4_800)..Duration::from_secs(11)).contains(&elapsed),
        "…which is one `timeout`, not the driver's own 30 s (took {elapsed:?})",
    );
    assert!(
        fixture.saw("main.test", "/download"),
        "the request really went out",
    );
}

#[tokio::test]
async fn a_fragment_on_the_document_already_shown_fails_immediately() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture).retries(1).timeout_ms(5_000.0)).await;

    session
        .fetch(&fixture.main_url("/cookie"), params())
        .await
        .expect("the first fetch");

    let started = Instant::now();
    let outcome = session
        .fetch(&format!("{}#frag", fixture.main_url("/cookie")), params())
        .await;
    let elapsed = started.elapsed();
    session.close().await;

    let golden = corpus().row("a_fragment_on_the_same_document");
    assert!(
        golden
            .error
            .as_deref()
            .is_some_and(|text| text.contains("Failed to get response for")),
        "Python raises `RuntimeError: Failed to get response for …`: {:?}",
        golden.error,
    );
    let error = outcome.expect_err("a same-document navigation produces no response");
    assert!(
        error.to_string().contains("Failed to get response for"),
        "the port must raise the same thing, got: {error}",
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "CDP omits `loaderId` for a same-document navigation, so there is no \
         lifecycle event to wait for — this must not burn the 5 s navigation \
         timeout (took {elapsed:?})",
    );
    assert_eq!(
        fixture.hits("/cookie"),
        1,
        "the fragment navigation never went to the network",
    );
}

// ---------------------------------------------------------------------------
// N. The fields `convertor.py` takes from two different responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_headers_come_from_the_first_response_and_the_request_headers_from_its_request() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;
    let response = session
        .fetch(&fixture.main_url("/cookie"), params())
        .await
        .expect("the page is fetched");
    session.close().await;

    let golden = corpus().row("cookies").observed();

    let mut names: Vec<String> = response
        .headers
        .keys()
        .map(|name| name.to_lowercase())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(names, golden.header_names, "Response.headers");
    assert!(
        names.iter().any(|name| name == "set-cookie"),
        "`Set-Cookie` is stripped from `Network.responseReceived`; only \
         `Network.responseReceivedExtraInfo` carries it, and Playwright's \
         `allHeaders()` — which is what `Response.headers` is — uses that: {names:?}",
    );
    assert_eq!(
        response.header("set-cookie"),
        Some("alpha=one; Path=/\nbeta=two; Path=/"),
        "duplicate `Set-Cookie` headers are joined with a newline, as Playwright's \
         `headersObjectToArray(headers, \"\\n\")` joins them",
    );
    assert_eq!(
        response.header("content-type").map(str::to_owned),
        golden.content_type.clone(),
    );

    // Chrome fills in the `sec-ch-*` client hints at different moments
    // depending on how far the request got, so only the stable core of the
    // request headers is compared — plus the referer, which is the field
    // `google_search` exists to set.
    let request_names: Vec<String> = response
        .request_headers
        .keys()
        .map(|name| name.to_lowercase())
        .collect();
    for expected in ["accept", "host", "referer", "user-agent"] {
        assert!(
            request_names.iter().any(|name| name == expected),
            "Response.request_headers is missing {expected}: {request_names:?}",
        );
        assert!(
            golden.request_header_names.iter().any(|n| n == expected),
            "…and the corpus agrees it should be there",
        );
    }
    assert_eq!(
        response
            .request_headers
            .get("referer")
            .or_else(|| response.request_headers.get("Referer"))
            .map(String::as_str),
        golden.request_referer.as_deref(),
    );
}

#[tokio::test]
async fn the_serialised_dom_is_byte_for_byte_the_length_playwright_produced() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture)).await;

    // Three pages with no scripted mutation, so the serialised DOM is a pure
    // function of the bytes the server sent and the serialiser. If this crate's
    // `page.content()` differed from Playwright's by so much as a whitespace
    // node, the lengths would not match.
    for (path, row) in [
        ("/cookie", "cookies"),
        ("/teapot", "non_ok_status"),
        ("/plain.json", "non_html_body"),
    ] {
        let response = session
            .fetch(&fixture.main_url(path), params())
            .await
            .expect("the page is fetched");
        let golden = corpus().row(row).observed();
        assert_eq!(
            response.body().len(),
            golden.body_len,
            "{path}: serialised as {:?}",
            String::from_utf8_lossy(response.body()),
        );
        assert_eq!(response.status, golden.status);
    }
    session.close().await;
}

// ---------------------------------------------------------------------------
// O. Concurrency: one policy per tab
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_concurrent_requests_keep_their_own_blocking_policies() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = Arc::new(session_for(live_config(&fixture).max_pages(2)).await);

    // Both fetch the *same* page, so the server cannot tell them apart by URL —
    // only by count. One blocks its sub-resources and one does not, so
    // `/style.css` must arrive exactly once: twice would mean the route never
    // took, zero would mean it leaked onto the other tab.
    let blocked = {
        let session = Arc::clone(&session);
        let url = fixture.main_url("/");
        tokio::spawn(async move {
            session
                .fetch(
                    &url,
                    FetchParams {
                        disable_resources: Override::Set(true),
                        ..params()
                    },
                )
                .await
        })
    };
    let open = {
        let session = Arc::clone(&session);
        let url = fixture.main_url("/");
        tokio::spawn(async move { session.fetch(&url, params()).await })
    };

    let blocked = blocked.await.expect("the task joins").expect("a response");
    let open = open.await.expect("the task joins").expect("a response");
    assert_eq!(blocked.status, 200);
    assert_eq!(open.status, 200);
    assert_eq!(session.pool_stats().total_pages, 2, "two tabs were used");
    session.close().await;

    assert_eq!(fixture.hits("/"), 2, "both requests really went out");
    assert_eq!(
        fixture.hits("/style.css"),
        1,
        "exactly one of the two tabs was blocking: saw {}",
        show(&received(&fixture)),
    );
    assert_eq!(fixture.hits("/pixel.gif"), 1, "…and the same for the image");
    assert_eq!(
        fixture.hits("/app.js"),
        2,
        "scripts are not in EXTRA_RESOURCES, so both tabs fetched it",
    );
}

#[tokio::test]
async fn a_wait_selector_that_is_not_valid_css_is_logged_not_fatal() {
    require_browser!();
    let fixture = Fixture::start().await;
    let session = session_for(live_config(&fixture).timeout_ms(3_000.0)).await;

    // Python: `page.locator("[[[")` raises from inside the `try`, which logs
    // `Error waiting for selector …` and carries on to build the response.
    let response = session
        .fetch(
            &fixture.main_url("/cookie"),
            FetchParams {
                wait_selector: Override::Set(Some("[[[".to_owned())),
                ..params()
            },
        )
        .await
        .expect("a malformed selector must not fail the request");
    session.close().await;

    let golden = corpus().row("cookies").observed();
    assert_eq!(response.status, golden.status);
    assert_eq!(
        response.css("#c::text").expect("a probe").get(),
        golden.css("cookie_marker").map(str::to_owned),
    );
}

/// Not an assertion about the port — a printed note so a run's log says which
/// browser binary produced its results.
#[tokio::test]
async fn report_the_browser_binary_these_tests_drove() {
    require_browser!();
    let path = crate::session::resolve_executable(
        &crate::config::BrowserConfig::new()
            .validate()
            .expect("valid"),
    )
    .expect("a browser");
    eprintln!("live_u14 is driving: {}", path.display());
    assert!(path.exists());
}
