//! MCP end to end: a real `rmcp` server, a real handshake, real `tools/call`
//! round trips against a loopback site.
//!
//! The server side is the genuine article — [`BrowserMcpServer`] served through
//! `rmcp`'s own async-read/write transport, over a `tokio::io::duplex` pair
//! standing in for the pipe an agent runtime would give it. The client side is
//! hand-written newline-delimited JSON-RPC rather than `rmcp`'s client, because
//! the workspace pins `rmcp` with the `server` and `transport-io` features and
//! not `client`, and `AGENTS.md` forbids changing a manifest to suit a test.
//! Hand-framing costs about thirty lines and has a compensating virtue: what
//! goes over the wire is written out literally, so a test failure names the
//! actual bytes an agent runtime would have sent.
//!
//! No network. [`local_site::Site`] is the loopback fixture copied from the
//! façade's `tests/integration.rs`. All but one test takes the HTTP path and
//! needs no browser; `a_real_capture_comes_back_as_an_image_block` launches
//! Chrome and honours `BROWSER_RS_SKIP_BROWSER_TESTS`, per `AGENTS.md`.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use browser_mcp::server::BrowserMcpServer;
use browser_server::config::ServerConfig;
use browser_server::engine::Engine;
use browser_server::mcp::{EngineTools, TOOL_NAMES};

use local_site::{Page, Site};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// A three-page shop: a listing, two products, and a `robots.txt` that allows
/// everything, so `crawl` has a link graph to walk.
fn shop(path: &str) -> Option<Page> {
    match path {
        "/robots.txt" => Some(Page::text("User-agent: *\nAllow: /\n")),
        "/" => Some(Page::html(
            r#"<!doctype html><html><head><title>The Shop</title></head><body>
               <h1>The Shop</h1>
               <p>Two things for sale.</p>
               <ul>
                 <li><a href="/p/1">Kettle</a></li>
                 <li><a href="/p/2">Teapot</a></li>
               </ul>
               </body></html>"#,
        )),
        "/p/1" => Some(Page::html(
            r#"<!doctype html><html><head><title>Kettle</title></head><body>
               <h1 class="name">Kettle</h1><span class="price">£19.99</span>
               <a href="/">home</a></body></html>"#,
        )),
        "/p/2" => Some(Page::html(
            r#"<!doctype html><html><head><title>Teapot</title></head><body>
               <h1 class="name">Teapot</h1><span class="price">£24.00</span>
               <a href="/">home</a></body></html>"#,
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The in-process MCP client
// ---------------------------------------------------------------------------

/// One end of a duplex pipe, speaking newline-delimited JSON-RPC.
struct Client {
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    next_id: i64,
}

impl Client {
    /// Start a server over a duplex pair and return the client end.
    ///
    /// The handshake is not performed here: [`Client::initialize`] does it, so
    /// a test can assert on the `initialize` response itself.
    fn start(engine: Arc<Engine>) -> (Self, tokio::task::JoinHandle<()>) {
        // 64 KiB each way: a `tools/list` response carrying eight full tool
        // descriptions is comfortably over the default 8 KiB, and a duplex that
        // fills up deadlocks rather than failing.
        let (server_side, client_side) = tokio::io::duplex(64 * 1024);
        let server = BrowserMcpServer::new(EngineTools::shared(engine));
        let handle = tokio::spawn(async move {
            match server.serve(server_side).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(error) => panic!("the server refused the handshake: {error}"),
            }
        });

        let (read, write) = tokio::io::split(client_side);
        (
            Self {
                reader: BufReader::new(read),
                writer: write,
                next_id: 1,
            },
            handle,
        )
    }

    /// Send a request and read its response.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;

        let reply = self.receive().await;
        assert_eq!(
            reply["jsonrpc"], "2.0",
            "{method} reply is not JSON-RPC 2.0"
        );
        assert_eq!(reply["id"], json!(id), "{method} reply has the wrong id");
        reply
    }

    /// Send a notification, which by definition gets no reply.
    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).expect("serialisable");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write to the server");
        self.writer.flush().await.expect("flush");
    }

    async fn receive(&mut self) -> Value {
        let mut line = String::new();
        // A hung server is a much clearer failure with a deadline on it than as
        // a test that never returns.
        let read = tokio::time::timeout(Duration::from_secs(30), self.reader.read_line(&mut line))
            .await
            .expect("the server answered within 30 s")
            .expect("read from the server");
        assert!(read > 0, "the server closed the pipe");
        serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("the server sent something that is not JSON: {error}\n{line}")
        })
    }

    /// The `initialize` / `notifications/initialized` handshake.
    async fn initialize(&mut self) -> Value {
        let reply = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "browser-rs-test", "version": "0" },
                }),
            )
            .await;
        self.notify("notifications/initialized", json!({})).await;
        reply
    }

    /// `tools/call`, returning the `result` object.
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        assert!(
            reply.get("error").is_none(),
            "{name} came back as a protocol error: {}",
            reply["error"]
        );
        reply["result"].clone()
    }

    /// The text of the first content block.
    fn text_of(result: &Value) -> String {
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text block in {result}"))
            .to_owned()
    }
}

/// An engine whose session profiles live in a directory that is deleted with
/// the test.
fn test_engine(profile_root: &tempfile::TempDir) -> Arc<Engine> {
    Arc::new(Engine::new(ServerConfig {
        profile_root: profile_root.path().to_path_buf(),
        ..ServerConfig::default()
    }))
}

// ---------------------------------------------------------------------------
// The handshake and the catalogue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_negotiates_and_advertises_tools() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));

    let reply = client.initialize().await;
    let result = &reply["result"];

    assert_eq!(
        result["protocolVersion"], "2025-06-18",
        "the version we asked for"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "a server with tools must say so, or clients never call tools/list"
    );
    assert_eq!(result["serverInfo"]["name"], "browser-rs");
    assert!(!result["serverInfo"]["version"]
        .as_str()
        .expect("version")
        .is_empty());

    // The instructions are the only text a model sees before it picks a tool.
    let instructions = result["instructions"].as_str().expect("instructions");
    assert!(instructions.contains("`fetch`"));
    assert!(instructions.contains("`browse`"));
    assert!(instructions.contains("session_open"));

    server.abort();
}

/// Agents cache the tool list. A rename, a reorder or a lost description is a
/// breaking change for every agent that has already read it.
#[tokio::test]
async fn the_tool_list_is_stable() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let reply = client.request("tools/list", json!({})).await;
    let tools = reply["result"]["tools"].as_array().expect("tools array");

    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        TOOL_NAMES.to_vec(),
        "the tool set and its order are the contract"
    );

    for tool in tools {
        let name = tool["name"].as_str().expect("a name");
        assert!(
            tool["description"].as_str().expect("a description").len() > 80,
            "{name} has a stub description"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        let properties = tool["inputSchema"]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} has no properties"));
        for (field, schema) in properties {
            assert!(
                schema["description"].as_str().unwrap_or_default().len() > 20,
                "{name}.{field} reaches the agent without a description"
            );
        }
    }

    server.abort();
}

#[tokio::test]
async fn ping_works_before_and_after_the_handshake() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));

    // The specification permits a ping ahead of `initialize`.
    let reply = client.request("ping", json!({})).await;
    assert!(reply["result"].is_object());

    client.initialize().await;
    let reply = client.request("ping", json!({})).await;
    assert!(reply["result"].is_object());

    server.abort();
}

// ---------------------------------------------------------------------------
// tools/call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_returns_the_page_as_markdown() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client.call("fetch", json!({ "url": site.url("/") })).await;

    assert_eq!(result["isError"], false, "{result}");
    let text = Client::text_of(&result);

    // The header states what the body cannot: the final URL and the status.
    assert!(text.contains(&format!("URL: {}", site.url("/"))), "{text}");
    assert!(text.contains("Status: 200"), "{text}");
    assert!(text.contains("Title: The Shop"), "{text}");
    assert!(text.contains("Transport: HTTP (no browser)"), "{text}");
    assert!(text.contains("Format: markdown"), "{text}");

    // Markdown, not raw HTML — API.md §3's first rule about what to return.
    assert!(text.contains("The Shop"), "{text}");
    assert!(!text.contains("<html"), "raw markup leaked:\n{text}");
    assert!(!text.contains("<li>"), "raw markup leaked:\n{text}");

    assert_eq!(site.hit_count("/"), 1);
    server.abort();
}

#[tokio::test]
async fn extract_pulls_out_only_what_was_asked_for() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call(
            "extract",
            json!({
                "url": site.url("/p/1"),
                "kind": "css",
                "selector": ".price::text",
            }),
        )
        .await;

    assert_eq!(result["isError"], false, "{result}");
    let text = Client::text_of(&result);
    assert!(text.contains("kind=css"), "{text}");
    assert!(text.contains("Matches: 1 total, 1 shown"), "{text}");
    assert!(text.contains("1. £19.99"), "{text}");
    // The point of `extract`: the rest of the page does not come back.
    assert!(
        !text.contains("Kettle"),
        "the whole page came back:\n{text}"
    );

    server.abort();
}

#[tokio::test]
async fn a_bounded_crawl_returns_an_index_with_its_selector_matches() {
    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call(
            "crawl",
            json!({
                "seed": site.url("/"),
                "max_pages": 5,
                "max_depth": 1,
                "selector": ".price::text",
            }),
        )
        .await;

    assert_eq!(result["isError"], false, "{result}");
    let text = Client::text_of(&result);
    assert!(
        text.contains(&format!("Crawl of {}", site.url("/"))),
        "{text}"
    );
    assert!(text.contains("| depth | status | url | title |"), "{text}");
    assert!(text.contains("/p/1"), "{text}");
    assert!(text.contains("## `.price::text` matches"), "{text}");
    assert!(text.contains("£19.99") || text.contains("£24.00"), "{text}");

    server.abort();
}

#[tokio::test]
async fn session_list_is_empty_and_says_what_to_do_about_it() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client.call("session_list", json!({})).await;
    assert_eq!(result["isError"], false, "{result}");
    let text = Client::text_of(&result);
    assert!(text.contains("No sessions are open"), "{text}");
    assert!(text.contains("session_open"), "{text}");

    server.abort();
}

/// The full session round trip, without a browser: opening a session creates
/// its persistent profile directory, listing shows it, closing keeps the
/// directory — which is the whole point of the design.
#[tokio::test]
async fn a_session_opens_lists_and_closes_without_losing_its_profile() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call(
            "session_open",
            json!({ "name": "shop", "impersonate": "chrome" }),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let text = Client::text_of(&result);
    assert!(text.contains("Session `shop` is open"), "{text}");
    assert!(
        text.contains("dashboard"),
        "the login story is told:\n{text}"
    );

    let profile_dir = result["structuredContent"]["profile_dir"]
        .as_str()
        .expect("a profile directory")
        .to_owned();
    assert!(
        std::path::Path::new(&profile_dir).exists(),
        "the profile directory is what survives a restart"
    );

    let result = client.call("session_list", json!({})).await;
    let text = Client::text_of(&result);
    assert!(text.contains("`shop`"), "{text}");
    assert_eq!(result["structuredContent"][0]["name"], "shop", "{result}");

    let result = client
        .call("session_close", json!({ "name": "shop" }))
        .await;
    assert_eq!(result["isError"], false, "{result}");
    assert!(Client::text_of(&result).contains("profile directory is kept"));
    assert!(
        std::path::Path::new(&profile_dir).exists(),
        "closing a session must not delete the login"
    );

    server.abort();
}

// ---------------------------------------------------------------------------
// Failure modes
// ---------------------------------------------------------------------------

/// A failure the model can act on must arrive as an `isError` result. A
/// JSON-RPC error is rendered opaquely by most clients, so the message would
/// never reach the model.
#[tokio::test]
async fn a_bad_url_is_a_visible_tool_error_not_a_protocol_error() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client.call("fetch", json!({ "url": "not-a-url" })).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = Client::text_of(&result);
    assert!(!text.is_empty(), "an error with no message helps nobody");

    server.abort();
}

#[tokio::test]
async fn an_invented_argument_is_rejected_with_an_explanation() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call(
            "fetch",
            json!({ "url": "https://example.test/", "javascript": true }),
        )
        .await;
    assert_eq!(result["isError"], true, "{result}");
    let text = Client::text_of(&result);
    assert!(text.contains("javascript"), "{text}");
    assert!(text.contains("input schema"), "{text}");

    server.abort();
}

/// An unroutable request is the one case that really is a protocol error.
#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let reply = client
        .request("tools/call", json!({ "name": "navigate", "arguments": {} }))
        .await;
    assert_eq!(reply["error"]["code"], -32601, "{reply}");

    server.abort();
}

/// `screenshot` needs somewhere to point. Neither argument is an argument
/// error, and the message has to name both ways out.
#[tokio::test]
async fn screenshot_without_a_target_names_both_ways_to_give_it_one() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client.call("screenshot", json!({})).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = Client::text_of(&result);
    assert!(text.contains("`url`"), "{text}");
    assert!(text.contains("`session`"), "{text}");

    server.abort();
}

/// A session that has never browsed has nothing to photograph. No Chrome is
/// launched to find that out — naming an unopened session merely creates its
/// profile directory — so this runs on every machine.
#[tokio::test]
async fn screenshotting_a_session_with_no_page_says_what_is_missing() {
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call("screenshot", json!({ "session": "fresh" }))
        .await;
    assert_eq!(result["isError"], true, "{result}");
    let text = Client::text_of(&result);
    assert!(text.contains("no page yet"), "{text}");
    // And it names the ways forward, not just the problem.
    assert!(text.contains("url") && text.contains("browse"), "{text}");

    server.abort();
}

/// The real thing: a Chrome capture, returned as an image content block.
///
/// Honours `BROWSER_RS_SKIP_BROWSER_TESTS` per `AGENTS.md` and prints a skip
/// notice — this is the only test in the file that needs a browser.
#[tokio::test]
async fn a_real_capture_comes_back_as_an_image_block() {
    if std::env::var_os("BROWSER_RS_SKIP_BROWSER_TESTS").is_some() {
        eprintln!(
            "skipping a_real_capture_comes_back_as_an_image_block: \
             BROWSER_RS_SKIP_BROWSER_TESTS is set"
        );
        return;
    }

    let site = Site::start(shop);
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call("screenshot", json!({ "url": site.url("/") }))
        .await;
    assert_eq!(result["isError"], false, "{result}");

    // Two blocks: the caption a model reads, then the image it looks at.
    let content = result["content"].as_array().expect("content");
    assert_eq!(content.len(), 2, "{result}");
    assert_eq!(content[0]["type"], "text");
    let caption = content[0]["text"].as_str().expect("caption");
    assert!(caption.contains("viewport only"), "{caption}");
    // The scale factor is stated next to the image, which is where a model is
    // looking when it misreads a coordinate off one.
    assert!(caption.contains("device scale factor of 2"), "{caption}");

    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    let data = content[1]["data"].as_str().expect("base64 data");
    // Base64 of a real PNG: the magic bytes survive encoding as `iVBORw0`.
    assert!(
        data.starts_with("iVBORw0"),
        "not a PNG: {}",
        &data[..16.min(data.len())]
    );

    server.abort();
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// A capped response must announce its own truncation. A model handed a
/// silently shortened page answers confidently from half of it.
#[tokio::test]
async fn a_capped_response_marks_what_it_dropped() {
    fn long(path: &str) -> Option<Page> {
        (path == "/long").then(|| {
            let paragraphs = (0..400)
                .map(|n| format!("<p>Paragraph number {n} with some filler text in it.</p>"))
                .collect::<String>();
            Page::html(format!(
                "<!doctype html><html><head><title>Long</title></head>\
                 <body><h1>Long</h1>{paragraphs}</body></html>"
            ))
        })
    }

    let site = Site::start(long);
    let profiles = tempfile::tempdir().expect("tempdir");
    let (mut client, server) = Client::start(test_engine(&profiles));
    client.initialize().await;

    let result = client
        .call(
            "fetch",
            json!({ "url": site.url("/long"), "max_chars": 500 }),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let text = Client::text_of(&result);

    assert!(text.contains("[truncated:"), "{text}");
    assert!(text.contains("characters shown"), "{text}");
    // The remedy is named, not merely the problem.
    assert!(text.contains("`extract`"), "{text}");
    // And the cap is honoured: header plus 500 characters plus the marker is
    // nowhere near the whole page.
    assert!(
        text.len() < 2_000,
        "the cap was not applied: {} bytes",
        text.len()
    );

    server.abort();
}

// ---------------------------------------------------------------------------
// The loopback fixture, copied from `crates/browser-rs/tests/integration.rs`
// as `AGENTS.md` directs. No network, no third-party server.
// ---------------------------------------------------------------------------

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
