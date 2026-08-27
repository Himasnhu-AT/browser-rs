//! The REST surface, driven end to end over a real loopback socket.
//!
//! Every test here builds the **actual** axum application from
//! [`browser_server::api::app`], binds it to an ephemeral port and talks to it
//! with an ordinary HTTP client. Nothing is stubbed at the router level: the
//! auth middleware, the body limit, the JSON extractor, the engine and the
//! fetcher stack are all the ones the binary runs. The far end of every fetch
//! is a second loopback socket — `local_site::Site`, copied from the façade's
//! `tests/integration.rs` and extended so a route can see the request's cookies
//! and set its own, which is what the persistence proof needs.
//!
//! No public network is touched. The browser-backed test honours
//! `BROWSER_RS_SKIP_BROWSER_TESTS`.

use std::sync::Arc;

use browser::AsyncFetcher;
use browser_server::api;
use browser_server::config::ServerConfig;
use browser_server::engine::Engine;
use serde_json::{json, Value};

mod local_site;

use local_site::{Incoming, Page, Site};

// ---------------------------------------------------------------------------
// The server under test
// ---------------------------------------------------------------------------

/// A running `browser-server`, stopped when it is dropped.
struct TestServer {
    base: String,
    token: Option<String>,
    engine: Arc<Engine>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServer {
    /// Start a server over `config`, on an ephemeral loopback port.
    async fn start(config: ServerConfig) -> Self {
        let token = config.api_token.clone();
        let engine = Arc::new(Engine::new(config));
        let app = api::app(Arc::clone(&engine));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));

        let (shutdown, stop) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await;
        });

        Self {
            base,
            token,
            engine,
            shutdown: Some(shutdown),
        }
    }

    /// A server with a token and a throw-away profile root.
    async fn authenticated(profiles: &std::path::Path) -> Self {
        Self::start(ServerConfig {
            api_token: Some(TOKEN.to_owned()),
            profile_root: profiles.to_path_buf(),
            ..ServerConfig::default()
        })
        .await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `GET path`, with the bearer token when one is configured.
    async fn get(&self, path: &str) -> Reply {
        self.send(AsyncFetcher::get(self.url(path)), true).await
    }

    /// `GET path` with no `Authorization` header at all.
    async fn get_anonymous(&self, path: &str) -> Reply {
        self.send(AsyncFetcher::get(self.url(path)), false).await
    }

    /// `POST path` with a JSON body and the bearer token.
    async fn post(&self, path: &str, body: Value) -> Reply {
        self.send(AsyncFetcher::post(self.url(path)).json(body), true)
            .await
    }

    /// `POST path` with a JSON body and no token.
    async fn post_anonymous(&self, path: &str, body: Value) -> Reply {
        self.send(AsyncFetcher::post(self.url(path)).json(body), false)
            .await
    }

    async fn delete(&self, path: &str) -> Reply {
        self.send(AsyncFetcher::delete(self.url(path)), true).await
    }

    async fn send(&self, builder: browser::RequestBuilder, authenticate: bool) -> Reply {
        let builder = match (&self.token, authenticate) {
            (Some(token), true) => builder.header("Authorization", format!("Bearer {token}")),
            _ => builder,
        };
        let response = builder.send().await.expect("the server answered");
        Reply {
            status: response.status,
            content_type: response
                .header("content-type")
                .unwrap_or_default()
                .to_owned(),
            body: response.body().to_vec(),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// One reply, kept as bytes so the PNG route can be checked too.
struct Reply {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "not JSON ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// The `error.kind` tag, for a failing reply.
    fn error_kind(&self) -> String {
        self.json()["error"]["kind"]
            .as_str()
            .unwrap_or("<no kind>")
            .to_owned()
    }
}

const TOKEN: &str = "a-token-that-is-long-enough-to-be-a-token";

// ---------------------------------------------------------------------------
// The fixture site
// ---------------------------------------------------------------------------

/// A small shop: four linked pages, a login that sets a cookie, and a page that
/// echoes back whatever cookie it was sent.
fn shop(request: &Incoming) -> Option<Page> {
    match request.path.as_str() {
        "/" => Some(Page::html(
            r#"<!doctype html><html><head><title>Home</title></head><body>
               <h1>Shop</h1>
               <a href="/p/1">one</a> <a href="/p/2">two</a>
               <a href="/p/3">three</a> <a href="/about">about</a>
               </body></html>"#,
        )),
        "/p/1" => Some(product("Widget", "9.99")),
        "/p/2" => Some(product("Gadget", "19.99")),
        "/p/3" => Some(product("Doohickey", "29.99")),
        "/about" => Some(Page::html(
            r#"<!doctype html><html><head><title>About</title></head><body>
               <p>About us</p><a href="/">home</a></body></html>"#,
        )),

        // The login sets a cookie and nothing else. Two separate requests to
        // the *server* have to share it for the session design to be real.
        "/login" => Some(
            Page::html(
                r#"<!doctype html><html><head><title>Welcome</title></head>
                          <body><p id="who">signed in</p></body></html>"#,
            )
            .with_header("Set-Cookie", "sid=abc123; Path=/"),
        ),
        // A document several viewports tall, so a full-page capture has
        // something to reach past.
        "/tall" => Some(Page::html(format!(
            r#"<!doctype html><html><head><title>Tall</title></head>
               <body style="margin:0">{}</body></html>"#,
            (0..40)
                .map(|n| format!(r#"<div style="height:100px">row {n}</div>"#))
                .collect::<String>()
        ))),
        // …and this is the page that tells you whether it worked.
        "/whoami" => Some(Page::html(format!(
            r#"<!doctype html><html><head><title>Who</title></head>
               <body><p id="who">{}</p></body></html>"#,
            request.cookie.as_deref().unwrap_or("anonymous")
        ))),
        _ => None,
    }
}

fn product(name: &str, price: &str) -> Page {
    Page::html(format!(
        r#"<!doctype html><html><head><title>{name}</title></head><body>
           <h1 class="name">{name}</h1><span class="price">{price}</span>
           <a href="/">home</a></body></html>"#
    ))
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_needs_no_token_and_says_whether_one_is_configured() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server.get_anonymous("/api/health").await;
    assert_eq!(reply.status, 200);
    let body = reply.json();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["authenticated"], true);
    assert_eq!(body["sessions"], 0);
}

#[tokio::test]
async fn every_other_route_rejects_a_request_without_the_token() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post_anonymous("/api/fetch", json!({"url": "http://127.0.0.1:1/"}))
        .await;
    assert_eq!(reply.status, 401);
    assert_eq!(reply.error_kind(), "unauthorized");

    let reply = server.get_anonymous("/api/sessions").await;
    assert_eq!(reply.status, 401);
}

#[tokio::test]
async fn the_wrong_token_is_no_better_than_none() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .send(
            AsyncFetcher::get(server.url("/api/sessions")).header("Authorization", "Bearer wrong"),
            false,
        )
        .await;
    assert_eq!(reply.status, 401);
}

/// The rule from `AGENTS.md`, at the router level: an unauthenticated server
/// still serves — the protection is the loopback bind, and that is asserted in
/// `config.rs`. This checks the other half, that "no token" does not
/// accidentally mean "no access".
#[tokio::test]
async fn an_unauthenticated_server_serves_every_route() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::start(ServerConfig {
        api_token: None,
        profile_root: profiles.path().to_path_buf(),
        ..ServerConfig::default()
    })
    .await;

    let reply = server.get_anonymous("/api/sessions").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), json!([]));
}

/// A path that matches nothing must be a 404, not a 401 — which is why the auth
/// middleware is a `route_layer`.
#[tokio::test]
async fn an_unknown_path_is_a_not_found_even_without_a_token() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    assert_eq!(server.get_anonymous("/api/nope").await.status, 404);
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// `api::surfaces` is the one place the four routers meet, and a wrong mount
/// path is exactly the kind of mistake that compiles, passes every unit test
/// and is only found by pointing a client at the port. So: point a client at
/// the port.
///
/// This asserts *reachability*, not behaviour — each surface owns its own
/// tests. What it catches is a surface silently not being mounted at all.
#[tokio::test]
async fn every_surface_is_reachable_on_the_one_port() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    // `/api` — the surface this file is about.
    assert_eq!(server.get_anonymous("/api/health").await.status, 200);

    // `/mcp` — a JSON-RPC `tools/list`, which must come back with a catalogue
    // rather than a 404 from an unmounted nest.
    let mcp = server
        .post(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
    assert_eq!(mcp.status, 200, "{}", String::from_utf8_lossy(&mcp.body));
    let tools = mcp.json()["result"]["tools"]
        .as_array()
        .expect("a tool array")
        .len();
    assert!(tools > 0, "the MCP catalogue came back empty");

    // …and it is guarded, by its own check rather than by my `route_layer`.
    let unauthenticated = server
        .post_anonymous(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
    assert_ne!(
        unauthenticated.status, 200,
        "an unauthenticated /mcp is an open browser proxy"
    );

    // `/` and `/ws` — the dashboard and its socket. A plain `GET /ws` is not a
    // websocket handshake, so anything but 404 means the route exists.
    let dashboard = server.get("/").await;
    assert_ne!(dashboard.status, 404, "the dashboard is not mounted");
    assert_ne!(server.get("/ws").await.status, 404, "/ws is not mounted");

    // A path nobody owns still 404s, which is what proves the assertions above
    // are not just "everything answers something".
    assert_eq!(server.get("/nothing-here").await.status, 404);
}

// ---------------------------------------------------------------------------
// The happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openapi_json_describes_the_surface() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server.get("/api/openapi.json").await;
    assert_eq!(reply.status, 200);
    let document = reply.json();
    assert_eq!(document["openapi"], "3.1.0");
    for path in [
        "/api/fetch",
        "/api/browse",
        "/api/extract",
        "/api/crawl",
        "/api/screenshot",
        "/api/sessions",
    ] {
        assert!(document["paths"][path].is_object(), "{path} is missing");
    }
}

#[tokio::test]
async fn fetch_returns_the_page() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post("/api/fetch", json!({"url": site.url("/p/1")}))
        .await;
    assert_eq!(reply.status, 200);

    assert_eq!(reply.content_type, "application/json");

    let page = reply.json();
    assert_eq!(page["status"], 200);
    assert_eq!(page["title"], "Widget");
    assert_eq!(page["transport"], "http");
    assert_eq!(page["truncated"], false);
    assert!(page["content"]
        .as_str()
        .expect("a string")
        .contains("Widget"));
    assert!(page["session"].is_null());
}

#[tokio::test]
async fn fetch_can_return_markdown_instead_of_markup() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/fetch",
            json!({"url": site.url("/p/1"), "format": "markdown"}),
        )
        .await;
    assert_eq!(reply.status, 200);
    let content = reply.json()["content"]
        .as_str()
        .expect("a string")
        .to_owned();
    assert!(content.contains("Widget"));
    assert!(!content.contains("<h1"), "markdown, not markup: {content}");
}

#[tokio::test]
async fn fetch_forwards_the_headers_it_is_given() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/fetch",
            json!({"url": site.url("/whoami"), "headers": {"Cookie": "sid=from-a-header"}}),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert!(reply.json()["content"]
        .as_str()
        .expect("a string")
        .contains("sid=from-a-header"));
}

#[tokio::test]
async fn extract_runs_a_selector_against_a_fetched_page() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/extract",
            json!({"url": site.url("/p/2"), "selector": "h1.name::text", "kind": "css"}),
        )
        .await;
    assert_eq!(reply.status, 200);
    let result = reply.json();
    assert_eq!(result["matches"], json!(["Gadget"]));
    assert_eq!(result["count"], 1);
    assert_eq!(result["kind"], "css");
}

#[tokio::test]
async fn extract_runs_xpath_and_markdown_over_inline_html() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/extract",
            json!({"html": "<ul><li>a</li><li>b</li></ul>", "selector": "//li/text()",
                   "kind": "xpath"}),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["matches"], json!(["a", "b"]));

    let reply = server
        .post(
            "/api/extract",
            json!({"html": "<h1>Title</h1><script>evil()</script>", "kind": "markdown"}),
        )
        .await;
    assert_eq!(reply.status, 200);
    let markdown = reply.json()["matches"][0]
        .as_str()
        .expect("a string")
        .to_owned();
    assert!(markdown.contains("Title"));
    assert!(!markdown.contains("evil"), "the AI pass strips scripts");
}

#[tokio::test]
async fn a_crawl_is_bounded_by_max_pages() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/crawl",
            json!({"seed": site.url("/"), "max_pages": 3, "max_depth": 3,
                   "selector": "h1.name::text"}),
        )
        .await;
    assert_eq!(reply.status, 200);

    let outcome = reply.json();
    let pages = outcome["pages"].as_array().expect("an array");
    assert_eq!(
        pages.len(),
        3,
        "the budget is spent on enqueue, so it is exact: {outcome:#}"
    );
    assert_eq!(outcome["max_pages"], 3);
    assert_eq!(outcome["stopped_at_limit"], true);

    // The seed is page one, and it is the only page at depth 0.
    assert_eq!(pages[0]["url"], site.url("/"));
    assert_eq!(pages[0]["depth"], 0);

    // …and the crawl never fetched more than it was allowed to.
    assert!(
        site.hits().len() <= 4,
        "three pages plus robots.txt at most, saw {:?}",
        site.hits()
    );
}

#[tokio::test]
async fn a_crawl_with_depth_zero_fetches_only_the_seed() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/crawl",
            json!({"seed": site.url("/"), "max_pages": 10, "max_depth": 0}),
        )
        .await;
    assert_eq!(reply.status, 200);
    let outcome = reply.json();
    assert_eq!(outcome["pages"].as_array().expect("an array").len(), 1);
    assert_eq!(
        outcome["stopped_at_limit"], false,
        "the depth limit, not the page budget, ended it"
    );
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_selector_is_a_four_hundred_with_the_parser_s_message() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/extract",
            json!({"html": "<p>hi</p>", "selector": "a[", "kind": "css"}),
        )
        .await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.error_kind(), "bad_request");
    assert!(
        !reply.json()["error"]["message"]
            .as_str()
            .expect("a string")
            .is_empty(),
        "an agent needs to be told what it got wrong"
    );
}

#[tokio::test]
async fn a_malformed_body_gets_the_same_error_shape_as_everything_else() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .send(
            AsyncFetcher::post(server.url("/api/fetch"))
                .header("Content-Type", "application/json")
                .body(browser::Body::Text("{ not json".to_owned())),
            true,
        )
        .await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.error_kind(), "bad_request");
}

/// The trap this closes: `{"url": "…", "sesion": "shop"}` used to deserialise
/// into a perfectly valid **stateless** request. A 200, a plausible body, no
/// session, no cookies, no login — and nothing anywhere saying why. An agent
/// hitting that does not diagnose it; it retries with different wording.
#[tokio::test]
async fn a_misspelt_field_is_rejected_by_name_rather_than_silently_ignored() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/fetch",
            json!({"url": site.url("/login"), "sesion": "shop"}),
        )
        .await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.error_kind(), "bad_request");

    let message = reply.json()["error"]["message"]
        .as_str()
        .expect("a string")
        .to_owned();
    assert!(message.contains("sesion"), "names the offender: {message}");
    assert!(message.contains("session"), "offers the fix: {message}");

    // …and nothing was fetched, because the request never reached the engine.
    assert!(site.hits().is_empty(), "{:?}", site.hits());
}

/// Every route goes through the same extractor, so the check has to hold on
/// all of them — and the *correct* spelling has to keep working, which is the
/// half a too-strict check would break.
#[tokio::test]
async fn the_unknown_field_check_covers_every_route_without_rejecting_real_ones() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    for (path, body) in [
        (
            "/api/fetch",
            json!({"url": "https://x.test/", "nonsense": 1}),
        ),
        (
            "/api/browse",
            json!({"url": "https://x.test/", "nonsense": 1}),
        ),
        ("/api/extract", json!({"html": "<p/>", "nonsense": 1})),
        (
            "/api/crawl",
            json!({"seed": "https://x.test/", "nonsense": 1}),
        ),
        (
            "/api/screenshot",
            json!({"url": "https://x.test/", "nonsense": 1}),
        ),
        ("/api/sessions", json!({"name": "shop", "nonsense": 1})),
    ] {
        let reply = server.post(path, body).await;
        assert_eq!(reply.status, 400, "{path} accepted an unknown field");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .expect("a string")
                .contains("nonsense"),
            "{path} did not name the field"
        );
    }

    // The control: a body using only real names still works.
    let ok = server
        .post(
            "/api/sessions",
            json!({"name": "shop", "impersonate": "chrome", "user_agent": "UA/1"}),
        )
        .await;
    assert_eq!(ok.status, 201, "{}", String::from_utf8_lossy(&ok.body));
}

/// A body that is valid JSON but not an object — serde would otherwise report
/// this as a type error naming an internal struct name.
#[tokio::test]
async fn a_non_object_body_is_refused_in_the_house_shape() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post("/api/fetch", json!(["not", "an", "object"]))
        .await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.error_kind(), "bad_request");
}

#[tokio::test]
async fn a_non_http_scheme_is_refused_before_anything_is_opened() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    for url in [
        "file:///etc/passwd",
        "data:text/html,<p>hi",
        "ftp://x.test/",
    ] {
        let reply = server.post("/api/fetch", json!({"url": url})).await;
        assert_eq!(reply.status, 400, "{url} should be refused");
        assert_eq!(reply.error_kind(), "bad_request");
    }
}

#[tokio::test]
async fn an_oversized_body_is_refused_by_the_limit_layer() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::start(ServerConfig {
        api_token: Some(TOKEN.to_owned()),
        profile_root: profiles.path().to_path_buf(),
        max_body_bytes: 256,
        ..ServerConfig::default()
    })
    .await;

    let reply = server
        .post(
            "/api/extract",
            json!({"html": "x".repeat(4096), "kind": "text"}),
        )
        .await;
    assert_eq!(reply.status, 413);
    // …and it is refused in the same shape as every other failure, rather than
    // being the one bodiless response on the surface.
    assert_eq!(reply.content_type, "application/json");
    assert_eq!(reply.error_kind(), "bad_request");
}

/// The other bodiless failure a `tower-http` layer can produce, and the two
/// axum answers for a route that does not exist.
#[tokio::test]
async fn a_bare_failure_from_a_layer_still_arrives_in_the_house_shape() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let missing = server.get_anonymous("/api/nope").await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.error_kind(), "not_found");

    // `GET` on a `POST`-only route: axum's own 405, also bodiless.
    let wrong_method = server.get("/api/fetch").await;
    assert_eq!(wrong_method.status, 405);
    assert_eq!(wrong_method.error_kind(), "bad_request");
}

#[tokio::test]
async fn extract_without_a_source_is_a_four_hundred() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    let reply = server.post("/api/extract", json!({"kind": "text"})).await;
    assert_eq!(reply.status, 400);
}

/// A session that has never browsed has no tab to capture, and the route says
/// which of the two things the caller forgot.
#[tokio::test]
async fn a_screenshot_of_a_session_with_no_page_says_so() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post("/api/screenshot", json!({"session": "shop"}))
        .await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.error_kind(), "bad_request");
    assert!(reply.json()["error"]["message"]
        .as_str()
        .expect("a string")
        .contains("no page yet"));
}

#[tokio::test]
async fn screenshot_without_a_url_or_a_session_is_a_four_hundred() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    let reply = server.post("/api/screenshot", json!({})).await;
    assert_eq!(reply.status, 400);
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_can_be_created_listed_reattached_and_closed() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let created = server
        .post(
            "/api/sessions",
            json!({"name": "shop", "impersonate": "firefox147",
                   "proxy": "http://alice:hunter2@egress.test:8080"}),
        )
        .await;
    assert_eq!(created.status, 201);
    let info = created.json();
    assert_eq!(info["name"], "shop");
    assert_eq!(info["impersonate"], "firefox147");
    assert_eq!(info["browser_live"], false);

    // The listing must never carry proxy credentials.
    let shown = info["proxy"].as_str().expect("a string");
    assert!(!shown.contains("hunter2"), "credentials leaked: {shown}");
    assert!(shown.contains("egress.test"));

    let profile_dir = info["profile_dir"].as_str().expect("a string").to_owned();
    assert!(std::path::Path::new(&profile_dir).is_dir());

    let listed = server.get("/api/sessions").await;
    assert_eq!(listed.status, 200);
    let sessions = listed.json();
    assert_eq!(sessions.as_array().expect("an array").len(), 1);
    assert_eq!(sessions[0]["name"], "shop");

    // Reattaching returns the same session and the same directory.
    let again = server.post("/api/sessions", json!({"name": "shop"})).await;
    assert_eq!(again.status, 201);
    assert_eq!(again.json()["profile_dir"], profile_dir);
    assert_eq!(
        server
            .get("/api/sessions")
            .await
            .json()
            .as_array()
            .expect("an array")
            .len(),
        1,
        "reattach, not a second session"
    );

    let closed = server.delete("/api/sessions/shop").await;
    assert_eq!(closed.status, 204);
    assert_eq!(
        server.get("/api/sessions").await.json(),
        json!([]),
        "closed sessions are gone from the listing"
    );
    assert!(
        std::path::Path::new(&profile_dir).is_dir(),
        "…but the profile directory stays: that is the whole point"
    );
}

#[tokio::test]
async fn closing_an_unknown_session_is_a_not_found() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    let reply = server.delete("/api/sessions/nothing").await;
    assert_eq!(reply.status, 404);
    assert_eq!(reply.error_kind(), "not_found");
}

/// A name from a request body must not be able to name a directory outside the
/// profile root.
#[tokio::test]
async fn a_traversing_session_name_stays_inside_the_profile_root() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post("/api/sessions", json!({"name": "../../escape"}))
        .await;
    assert_eq!(reply.status, 201);
    let dir = reply.json()["profile_dir"]
        .as_str()
        .expect("a string")
        .to_owned();
    assert!(
        std::path::Path::new(&dir).starts_with(profiles.path()),
        "{dir} escaped {}",
        profiles.path().display()
    );
}

// ---------------------------------------------------------------------------
// The property the whole design rests on
// ---------------------------------------------------------------------------

/// **A session retains a cookie across two separate requests.**
///
/// Two independent HTTP calls to `browser-server`, each with its own socket and
/// its own handler invocation, naming the same session. The first is a login
/// that the fixture answers with `Set-Cookie: sid=abc123`; the second asks the
/// fixture to echo back the `Cookie` header it received. If the session did not
/// carry the jar between them, the second page would say `anonymous`.
///
/// The third call is the control: the same URL with **no** session, which must
/// see nothing — otherwise this test would pass just as well against a server
/// with one global jar, which is not the design.
#[tokio::test]
async fn a_session_retains_a_cookie_across_two_separate_requests() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    // Request one: log in.
    let login = server
        .post(
            "/api/fetch",
            json!({"url": site.url("/login"), "session": "shop"}),
        )
        .await;
    assert_eq!(login.status, 200);
    assert_eq!(
        login.json()["cookies"]["sid"],
        "abc123",
        "the fixture really did set a cookie"
    );

    // Request two: a different call, a different socket, the same session.
    let who = server
        .post(
            "/api/extract",
            json!({"url": site.url("/whoami"), "session": "shop",
                   "selector": "#who::text", "kind": "css"}),
        )
        .await;
    assert_eq!(who.status, 200);
    assert_eq!(
        who.json()["matches"],
        json!(["sid=abc123"]),
        "the session carried the cookie from one request to the next"
    );

    // The control: no session, no cookie.
    let anonymous = server
        .post(
            "/api/extract",
            json!({"url": site.url("/whoami"), "selector": "#who::text", "kind": "css"}),
        )
        .await;
    assert_eq!(anonymous.json()["matches"], json!(["anonymous"]));
}

/// …and it survives the session being closed and reopened, because the mirror
/// lives in the profile directory rather than in the process.
///
/// This is the restart case in everything but the `exec`: a second [`Engine`]
/// over the same profile root, with a fresh registry and a fresh cookie jar,
/// reading state the first one wrote to disk.
#[tokio::test]
async fn a_session_cookie_survives_the_server_being_rebuilt() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");

    {
        let server = TestServer::authenticated(profiles.path()).await;
        let login = server
            .post(
                "/api/fetch",
                json!({"url": site.url("/login"), "session": "shop"}),
            )
            .await;
        assert_eq!(login.status, 200);
        // Closing is what flushes the mirror to the profile directory.
        assert_eq!(server.delete("/api/sessions/shop").await.status, 204);
    }

    // A completely new server, sharing only the directory on disk.
    let restarted = TestServer::authenticated(profiles.path()).await;
    let who = restarted
        .post(
            "/api/extract",
            json!({"url": site.url("/whoami"), "session": "shop",
                   "selector": "#who::text", "kind": "css"}),
        )
        .await;
    assert_eq!(who.status, 200);
    assert_eq!(
        who.json()["matches"],
        json!(["sid=abc123"]),
        "the cookie mirror in the profile directory outlived the process that wrote it"
    );
}

/// A crawl started on a session inherits that session's jar, so a login carries
/// into every page it fetches.
#[tokio::test]
async fn a_crawl_inherits_the_session_it_runs_on() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    server
        .post(
            "/api/fetch",
            json!({"url": site.url("/login"), "session": "shop"}),
        )
        .await;

    let reply = server
        .post(
            "/api/crawl",
            json!({"seed": site.url("/whoami"), "session": "shop", "max_pages": 1,
                   "max_depth": 0, "selector": "#who::text"}),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["pages"][0]["matches"], json!(["sid=abc123"]));
}

/// The idle sweep closes a session that has not been used, and the engine's own
/// registry is what proves it — there is no route for "sweep now", by design.
#[tokio::test]
async fn an_idle_session_is_swept() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::start(ServerConfig {
        api_token: Some(TOKEN.to_owned()),
        profile_root: profiles.path().to_path_buf(),
        session_idle_ttl: std::time::Duration::from_millis(1),
        ..ServerConfig::default()
    })
    .await;

    assert_eq!(
        server
            .post("/api/sessions", json!({"name": "shop"}))
            .await
            .status,
        201
    );
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    server.engine.sessions().sweep_and_close().await;

    assert_eq!(server.get("/api/sessions").await.json(), json!([]));
}

// ---------------------------------------------------------------------------
// The browser
// ---------------------------------------------------------------------------

/// `true` when this run must not launch Chrome.
fn skip_browser(what: &str) -> bool {
    if std::env::var_os("BROWSER_RS_SKIP_BROWSER_TESTS").is_some() {
        eprintln!("skipping {what}: BROWSER_RS_SKIP_BROWSER_TESTS is set");
        return true;
    }
    false
}

#[tokio::test]
async fn browse_renders_a_page_in_a_real_browser() {
    if skip_browser("browse_renders_a_page_in_a_real_browser") {
        return;
    }
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/browse",
            json!({"url": site.url("/p/1"), "wait_until": "domcontentloaded"}),
        )
        .await;

    // No Chrome on this machine is a documented outcome, not a failure: the
    // route says so with a 501 and a message naming what to install.
    if reply.status == 501 {
        eprintln!("skipping: no browser is installed");
        return;
    }
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let page = reply.json();
    assert_eq!(page["transport"], "browser");
    assert_eq!(page["title"], "Widget");
}

#[tokio::test]
async fn a_browser_session_reports_itself_as_live() {
    if skip_browser("a_browser_session_reports_itself_as_live") {
        return;
    }
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let reply = server
        .post(
            "/api/browse",
            json!({"url": site.url("/"), "session": "shop"}),
        )
        .await;
    if reply.status == 501 {
        eprintln!("skipping: no browser is installed");
        return;
    }
    assert_eq!(reply.status, 200);

    let sessions = server.get("/api/sessions").await.json();
    assert_eq!(sessions[0]["name"], "shop");
    assert_eq!(sessions[0]["browser_live"], true);

    // …and closing it stops the process rather than leaking it.
    assert_eq!(server.delete("/api/sessions/shop").await.status, 204);
}

/// The PNG's own header, so the assertion is about pixels rather than about a
/// status code: an eight-byte signature, then the `IHDR` chunk whose payload
/// opens with two big-endian `u32` dimensions.
fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert!(
        bytes.len() > 24,
        "too short to be a PNG: {} bytes",
        bytes.len()
    );
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    assert_eq!(&bytes[12..16], b"IHDR", "the first chunk should be IHDR");
    let read = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    (read(16), read(20))
}

/// `/api/screenshot` against a session's **current** page.
///
/// The interesting half is that the second call passes no `url`: the session is
/// browsed first, and the capture then shoots whatever tab that session is
/// parked on. That only works because `page_action` files the live `CdpPage`
/// away, so this is the test that the seam holds — and the fixture's hit count
/// proves the page was captured rather than quietly re-fetched.
#[tokio::test]
async fn a_session_page_can_be_screenshotted_without_reloading_it() {
    if skip_browser("a_session_page_can_be_screenshotted_without_reloading_it") {
        return;
    }
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let browsed = server
        .post(
            "/api/browse",
            json!({"url": site.url("/p/1"), "session": "shop"}),
        )
        .await;
    if browsed.status == 501 {
        eprintln!("skipping: no browser is installed");
        return;
    }
    assert_eq!(browsed.status, 200);
    let hits_before = site.hits().len();

    let reply = server
        .post("/api/screenshot", json!({"session": "shop"}))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.status);
    assert_eq!(reply.content_type, "image/png");

    let (width, height) = png_dimensions(&reply.body);
    assert!(width > 0 && height > 0, "{width}x{height}");
    assert_eq!(
        site.hits().len(),
        hits_before,
        "the tab was captured, not re-fetched"
    );
}

/// A stateless capture: no session, so a throw-away browser is launched,
/// navigated, shot and closed.
///
/// `full_page` is the assertion that earns its keep. `browser-cdp` clips to
/// `cssContentSize` rather than resizing the viewport, so a document taller
/// than the fold must come back as a taller image than the viewport shot of
/// the same page — which is the only way to tell the two apart from out here.
#[tokio::test]
async fn a_stateless_full_page_screenshot_reaches_past_the_fold() {
    if skip_browser("a_stateless_full_page_screenshot_reaches_past_the_fold") {
        return;
    }
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let viewport = server
        .post("/api/screenshot", json!({"url": site.url("/tall")}))
        .await;
    if viewport.status == 501 {
        eprintln!("skipping: no browser is installed");
        return;
    }
    assert_eq!(viewport.status, 200, "{}", viewport.status);
    let (_, viewport_height) = png_dimensions(&viewport.body);

    let full = server
        .post(
            "/api/screenshot",
            json!({"url": site.url("/tall"), "full_page": true}),
        )
        .await;
    assert_eq!(full.status, 200, "{}", full.status);
    let (_, full_height) = png_dimensions(&full.body);

    assert!(
        full_height > viewport_height,
        "a full-page shot of a 4000px document should be taller than the \
         viewport shot: {full_height} vs {viewport_height}"
    );
}
