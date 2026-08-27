//! The dashboard and its live-view socket, driven end to end over real sockets.
//!
//! Every test builds the application router the binary serves — `api::surfaces`
//! plus [`browser_server::dashboard::router`] and
//! [`browser_server::ws::router`], all inside `api::with_layers` — binds it to
//! an ephemeral port, and talks to it with an ordinary HTTP client and an
//! ordinary websocket client. Nothing is stubbed: the auth check, the timeout
//! layer, the engine, the session registry and the JSON codec are the ones the
//! binary runs.
//!
//! No public network is touched: the far end of every navigation is a second
//! loopback socket. The browser-backed test honours
//! `BROWSER_RS_SKIP_BROWSER_TESTS` and prints a skip notice when Chrome is
//! absent.

use std::sync::Arc;
use std::time::Duration;

use browser::AsyncFetcher;
use browser_server::config::ServerConfig;
use browser_server::engine::Engine;
use browser_server::{api, dashboard, ws};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite;

// Shared with `rest_api.rs`, which uses more of it than this file does.
#[allow(dead_code)]
mod local_site;

use local_site::{Incoming, Page, Site};

const TOKEN: &str = "dashboard-test-token";

// ---------------------------------------------------------------------------
// The server under test
// ---------------------------------------------------------------------------

/// A running `browser-server` with all four surfaces mounted.
struct TestServer {
    base: String,
    token: Option<String>,
    engine: Arc<Engine>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServer {
    async fn start(config: ServerConfig) -> Self {
        let token = config.api_token.clone();
        let engine = Arc::new(Engine::new(config));

        // The application the binary serves, layers and all: `api::surfaces`
        // merges `/api`, `/mcp`, the dashboard and `/ws` into one router.
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

    /// The websocket URL for `query`, token included.
    fn socket(&self, query: &str) -> String {
        let base = self.base.replacen("http://", "ws://", 1);
        let token = self.token.as_deref().unwrap_or_default();
        if query.is_empty() {
            format!("{base}/ws?token={token}")
        } else {
            format!("{base}/ws?{query}&token={token}")
        }
    }

    async fn get(&self, path: &str, authenticate: bool) -> (u16, String) {
        let builder = AsyncFetcher::get(self.url(path));
        let builder = match (&self.token, authenticate) {
            (Some(token), true) => builder.header("Authorization", format!("Bearer {token}")),
            _ => builder,
        };
        let response = builder.send().await.expect("the server answered");
        (
            response.status,
            String::from_utf8_lossy(response.body()).into_owned(),
        )
    }

    async fn post(&self, path: &str, body: Value) -> (u16, String) {
        let builder = AsyncFetcher::post(self.url(path)).json(body);
        let builder = match &self.token {
            Some(token) => builder.header("Authorization", format!("Bearer {token}")),
            None => builder,
        };
        let response = builder.send().await.expect("the server answered");
        (
            response.status,
            String::from_utf8_lossy(response.body()).into_owned(),
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// A websocket client
// ---------------------------------------------------------------------------

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str) -> Socket {
    let (socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the socket handshakes");
    socket
}

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(tungstenite::Message::Text(message.to_string().into()))
        .await
        .expect("the message is sent");
}

/// The next message with `t == tag`, or `None` if none arrives in `budget`.
///
/// Skipping the other tags is what makes these tests insensitive to the
/// ordering of a status tick against an activity event, which is genuinely
/// racy and not something worth pinning.
async fn next_tagged(socket: &mut Socket, tag: &str, budget: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next()).await;
        match message {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("the server sends JSON");
                if value["t"] == tag {
                    return Some(value);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) | Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// The fixture site
// ---------------------------------------------------------------------------

/// A page with one large button that tells the *server* it was clicked.
///
/// The proof of a click has to be something no CDP bookkeeping on our side can
/// fake, so the button's handler makes a request that the fixture socket
/// records. Its geometry is fixed and known: 200×60 CSS pixels at (40, 40) in a
/// 1280×720 viewport, so the test can aim at it.
fn clickable(request: &Incoming) -> Option<Page> {
    match request.path.as_str() {
        "/" => Some(Page::html(
            // The pulsing bar is not decoration: Chrome emits a screencast
            // frame when the compositor produces one, so a wholly static page
            // yields exactly one frame and then silence — which would make
            // "frames keep arriving" untestable here. It sits below the button
            // and never overlaps it.
            "<!doctype html><html><head><title>Console</title><style>\
             @keyframes pulse { from { opacity: .2 } to { opacity: 1 } }\
             #beat { position: absolute; left: 40px; top: 300px; width: 300px;\
             height: 80px; background: #c0392b;\
             animation: pulse .6s linear infinite alternate }\
             </style></head>\
             <body style=\"margin:0;background:#fff\">\
             <button id=\"go\" style=\"position:absolute;left:40px;top:40px;\
             width:200px;height:60px;font-size:20px\" \
             onclick=\"fetch('/clicked')\">Sign in</button>\
             <p id=\"out\" style=\"position:absolute;left:40px;top:130px\">idle</p>\
             <div id=\"beat\"></div>\
             </body></html>",
        )),
        "/clicked" => Some(Page {
            status: 200,
            content_type: "text/plain; charset=utf-8",
            body: "ok".to_owned(),
            headers: Vec::new(),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_dashboard_is_one_self_contained_document() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let (status, body) = server.get(&format!("/?token={TOKEN}"), false).await;
    assert_eq!(status, 200);
    assert!(body.starts_with("<!doctype html>"));
    // The stylesheet and the script are *in* the document, not linked from it.
    assert!(body.contains("--bg:"), "the CSS was inlined");
    assert!(body.contains("new WebSocket"), "the JS was inlined");
    for external in ["http://", "https://", "<link", "//cdn"] {
        assert!(
            !body.contains(external),
            "the page must fetch nothing: found {external:?}",
        );
    }
}

#[tokio::test]
async fn the_dashboard_refuses_without_a_token_and_says_how_to_pass_one() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let (status, body) = server.get("/", false).await;
    assert_eq!(status, 401);
    assert!(
        body.contains("?token="),
        "an operator staring at a 401 in a browser needs the next step: {body}",
    );
}

#[tokio::test]
async fn an_unauthenticated_server_serves_the_dashboard_to_anyone() {
    // The documented single-user mode: no token, and the listener is pinned to
    // loopback with a start-up warning. See `ServerConfig::effective_bind`.
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::start(ServerConfig {
        api_token: None,
        profile_root: profiles.path().to_path_buf(),
        ..ServerConfig::default()
    })
    .await;

    let (status, body) = server.get("/", false).await;
    assert_eq!(status, 200);
    assert!(body.contains("<!doctype html>"));
}

#[tokio::test]
async fn the_session_listing_needs_the_token_and_reports_what_is_open() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let (status, _) = server.get(dashboard::SESSIONS_PATH, false).await;
    assert_eq!(status, 401, "no token, no listing");

    let (status, _) = server.post("/api/sessions", json!({"name": "shop"})).await;
    assert_eq!(status, 201, "`POST /api/sessions` creates");

    let (status, body) = server
        .get(
            &format!("{}?token={TOKEN}", dashboard::SESSIONS_PATH),
            false,
        )
        .await;
    assert_eq!(status, 200);
    let overview: dashboard::Overview = serde_json::from_str(&body).expect("an overview");
    assert_eq!(overview.sessions.len(), 1);
    assert_eq!(overview.sessions[0].name, "shop");
    assert!(
        !overview.sessions[0].browser,
        "opening a session launches no browser",
    );
    assert_eq!(overview.sessions[0].url, None);
    assert!(overview.authenticated);
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_socket_without_the_token_is_refused_at_the_handshake() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let url = server.base.replacen("http://", "ws://", 1) + "/ws";
    let error = tokio_tungstenite::connect_async(url)
        .await
        .err()
        .expect("an unauthenticated socket must not open");
    match error {
        tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected a 401, got {other:?}"),
    }
}

#[tokio::test]
async fn a_socket_with_a_wrong_token_is_refused_too() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    let url = server.base.replacen("http://", "ws://", 1) + "/ws?token=nope";
    assert!(
        tokio_tungstenite::connect_async(url).await.is_err(),
        "a wrong token is not a missing one, but it is not a right one either",
    );
}

#[tokio::test]
async fn a_socket_handshakes_and_immediately_says_what_it_is_looking_at() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    let mut socket = connect(&server.socket("session=shop")).await;

    // The session does not exist yet, and that is a message rather than a
    // dropped connection: an operator routinely opens the dashboard first.
    let error = next_tagged(&mut socket, "error", Duration::from_secs(5))
        .await
        .expect("an error message");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("shop"),
        "{error}",
    );

    let status = next_tagged(&mut socket, "status", Duration::from_secs(10))
        .await
        .expect("a status message");
    assert_eq!(status["session"], "shop");
    assert_eq!(status["browser"], false);
    assert_eq!(status["url"], Value::Null);
}

#[tokio::test]
async fn the_index_socket_carries_the_engines_activity_log() {
    let site = Site::start(clickable);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    // No `session=`: the index feed, which is the activity log and no frames.
    let mut socket = connect(&server.socket("")).await;
    let status = next_tagged(&mut socket, "status", Duration::from_secs(10))
        .await
        .expect("a status message");
    assert_eq!(status["session"], "", "an index socket names no session");

    // A plain HTTP fetch — no browser involved — is still engine activity.
    let (code, _) = server
        .post(
            "/api/fetch",
            json!({"url": site.url("/"), "session": "shop"}),
        )
        .await;
    assert_eq!(code, 200);

    let action = next_tagged(&mut socket, "action", Duration::from_secs(10))
        .await
        .expect("an action message");
    assert_eq!(action["kind"], "fetch");
    assert_eq!(
        action["session"], "shop",
        "the index needs to know whose action this was",
    );
    assert!(action["at"].is_string());
    assert!(
        action["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("127.0.0.1"),
        "{action}",
    );
}

// ---------------------------------------------------------------------------
// Control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn input_is_refused_until_control_is_taken() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    let (code, _) = server.post("/api/sessions", json!({"name": "shop"})).await;
    assert_eq!(code, 201);

    let mut socket = connect(&server.socket("session=shop")).await;
    next_tagged(&mut socket, "status", Duration::from_secs(10))
        .await
        .expect("a status message");

    // A stray click while the agent is working is the hazard this guards.
    send(
        &mut socket,
        json!({"t":"mouse","kind":"down","x":10,"y":10,"button":"left","clicks":1}),
    )
    .await;
    let refused = next_tagged(&mut socket, "error", Duration::from_secs(10))
        .await
        .expect("an error message");
    let text = refused["message"].as_str().unwrap_or_default().to_owned();
    assert!(
        text.contains("watching"),
        "the refusal must say why: {text}",
    );

    // Take control, and the *reason* changes: now there is simply no browser.
    send(&mut socket, json!({"t":"control","take":true})).await;
    send(
        &mut socket,
        json!({"t":"mouse","kind":"down","x":10,"y":10,"button":"left","clicks":1}),
    )
    .await;
    let next = next_tagged(&mut socket, "error", Duration::from_secs(10))
        .await
        .expect("an error message");
    let text = next["message"].as_str().unwrap_or_default().to_owned();
    assert!(
        text.contains("no live browser"),
        "control was taken, so the gate is no longer the reason: {text}",
    );
}

#[tokio::test]
async fn a_message_the_server_cannot_read_is_reported_and_not_fatal() {
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;
    let mut socket = connect(&server.socket("")).await;

    send(&mut socket, json!({"t": "launch_missiles"})).await;
    let error = next_tagged(&mut socket, "error", Duration::from_secs(10))
        .await
        .expect("an error message");
    assert!(error["message"]
        .as_str()
        .unwrap_or_default()
        .contains("unreadable"));

    // …and the socket is still usable afterwards.
    assert!(
        next_tagged(&mut socket, "status", Duration::from_secs(10))
            .await
            .is_some(),
        "a bad message must not close the socket",
    );
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
async fn a_live_session_streams_frames_and_a_dashboard_click_reaches_the_page() {
    if skip_browser("a_live_session_streams_frames_and_a_dashboard_click_reaches_the_page") {
        return;
    }
    let site = Site::start(clickable);
    let profiles = tempfile::tempdir().expect("a temp dir");
    let server = TestServer::authenticated(profiles.path()).await;

    if dashboard::chrome_path(&server.engine).is_none() {
        eprintln!("skipping: no Chromium/Chrome binary on this machine");
        return;
    }

    // A real browse on a named session: this is what gives the session a tab
    // for the dashboard to watch.
    let (code, _) = server
        .post(
            "/api/browse",
            json!({"url": site.url("/"), "session": "shop"}),
        )
        .await;
    assert_eq!(code, 200);

    let mut socket = connect(&server.socket("session=shop")).await;

    let status = next_tagged(&mut socket, "status", Duration::from_secs(15))
        .await
        .expect("a status message");
    assert_eq!(status["browser"], true, "{status}");
    assert!(
        status["url"]
            .as_str()
            .unwrap_or_default()
            .contains("127.0.0.1"),
        "the status reports the page the agent is on: {status}",
    );

    // Frames. Several of them, which is also the proof that every one was
    // acknowledged — Chrome stops after one or two otherwise.
    let mut frames = Vec::new();
    while frames.len() < 3 {
        let Some(frame) = next_tagged(&mut socket, "frame", Duration::from_secs(30)).await else {
            break;
        };
        frames.push(frame);
    }
    assert!(
        frames.len() >= 3,
        "only {} frame(s) arrived; an unacknowledged screencast looks exactly \
         like this",
        frames.len(),
    );

    let first = &frames[0];
    assert_eq!(first["format"], "jpeg");
    let width = first["w"].as_f64().expect("a width");
    let height = first["h"].as_f64().expect("a height");
    assert!(
        width > 0.0 && height > 0.0 && width <= f64::from(ws::MAX_FRAME_EDGE),
        "the frame is capped at MAX_FRAME_EDGE: {width}x{height}",
    );
    let data = first["data"].as_str().expect("base64");
    assert!(!data.is_empty());
    // Real pixels of the real viewport, read out of the JPEG's own header.
    let decoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("the frame is base64")
    };
    assert_eq!(
        ws::image_size_of_bytes(&decoded),
        Some((width as u32, height as u32)),
        "`w`/`h` must be the image's own size, because that is the space the \
         dashboard sends coordinates in",
    );

    // Now the whole chain: take control, aim at the button in image space, and
    // let the *fixture socket* say whether the click landed.
    send(&mut socket, json!({"t":"control","take":true})).await;

    // The button is 200x60 CSS pixels at (40, 40) in a 1280x720 viewport, so
    // its centre is (140, 70). Converting to image space is the client's half
    // of the contract; the server does the other half.
    let to_image = |css_x: f64, css_y: f64| (css_x * width / 1280.0, css_y * height / 720.0);
    let (x, y) = to_image(140.0, 70.0);
    for kind in ["move", "down", "up"] {
        send(
            &mut socket,
            json!({"t":"mouse","kind":kind,"x":x,"y":y,"button":"left","clicks":1}),
        )
        .await;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut clicked = false;
    while tokio::time::Instant::now() < deadline && !clicked {
        clicked = site.hits().iter().any(|path| path == "/clicked");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server.engine.sessions().close_all().await;

    assert!(
        clicked,
        "the fixture never saw /clicked — the click did not reach the button. \
         Paths seen: {:?}",
        site.hits(),
    );
}
