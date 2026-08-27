//! The dashboard websocket: screencast frames out, input events in.
//!
//! `GET /ws?session=NAME` is the live view of one session. `GET /ws` with no
//! `session` is the index's feed: the activity log only, no frames. The wire
//! format is `API.md` §5's tagged JSON in both directions —
//! [`ServerMessage`] and [`ClientMessage`] are that document, as types.
//!
//! # The feature this exists for
//!
//! The operator watches their agent drive a real browser, takes the controls,
//! logs in by hand, and gives them back. The agent carries on in the same
//! Chromium profile, now authenticated. Everything below is in service of that:
//! the frames go one way, the clicks and keystrokes go the other, and the
//! session's profile directory outlives both.
//!
//! # Control is off until it is asked for
//!
//! A socket starts **watching**. Mouse and key messages are refused until a
//! `{"t":"control","take":true}` arrives, because a stray click landing on a
//! page the agent is halfway through filling in is a real way to lose work.
//! The server enforces it; the client's own toggle is only a mirror.
//!
//! # Coordinates
//!
//! The dashboard sends coordinates in **rendered-image space** — the pixel
//! grid of the JPEG it is displaying, which is neither the CSS size of its
//! `<img>` nor the page's own coordinates. Chrome's viewport is 1280×720 CSS
//! pixels rendered at a device scale factor of 2, and the frame may be capped
//! smaller still by [`MAX_FRAME_EDGE`], so three different sizes are in play.
//! [`Geometry`] holds the two that matter and
//! [`Geometry::to_page_space`] converts between them. `browser-cdp` is
//! documented to take page space only, so this is the layer that must get it
//! right.
//!
//! # Backpressure
//!
//! Frames are dropped, never queued — and the dropping already happened
//! upstream, in `browser_cdp::screencast`, whose bounded sink keeps the two
//! newest frames and discards the rest. A dashboard on a slow link simply sees
//! a lower frame rate. Nothing in this module buffers a frame.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use browser_cdp::cdp::CdpPage;
use browser_cdp::input::{Button, KeyEvent, KeyEventKind, Modifiers, MouseEvent, MouseEventKind};
use browser_cdp::screencast::{Frame, ScreencastOpts};
use futures::stream::SplitSink;
use futures::{SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::engine::{ActivityEvent, Engine};
use crate::error::ServerError;
use crate::session::Session;

/// Where [`router`] mounts the socket.
pub const WS_PATH: &str = "/ws";

/// How often the socket re-reads the session's state.
///
/// This is also when a screencast is attached to a browser that has appeared
/// since the last tick, and detached from one that has gone — which is what
/// lets an operator open the dashboard *before* the agent starts working and
/// watch the browser show up.
pub const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// The longest edge, in device pixels, a screencast frame may have.
///
/// A 1280×720 viewport at a device scale factor of 2 is a 2560×1440 JPEG, and
/// every one of those crosses a websocket. Capping the long edge roughly
/// quarters the bytes and costs nothing a human watching a browser can see.
pub const MAX_FRAME_EDGE: u32 = 1280;

/// How much of a frame is decoded when looking for the image's own dimensions.
///
/// The size lives in the first marker segment of a JPEG and at a fixed offset
/// in a PNG, so a prefix is enough; decoding a whole 200 KB frame to read four
/// numbers out of its header would not be.
pub const HEADER_SCAN_BYTES: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// The protocol — `API.md` §5, as types
// ---------------------------------------------------------------------------

/// Server → client.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ServerMessage {
    /// One screencast frame.
    Frame {
        /// Base64, exactly as CDP delivered it.
        data: String,
        /// `"jpeg"` or `"png"`.
        format: String,
        /// The image's own width in pixels — what an `<img>` reports as
        /// `naturalWidth`, and the space [`ClientMessage::Mouse`] coordinates
        /// are in.
        w: u32,
        /// The image's own height in pixels.
        h: u32,
    },
    /// What the session looks like right now.
    Status {
        /// The session's name, or `""` for an index socket with no session.
        session: String,
        /// The page's current URL. `null` when no browser is attached — a
        /// session that has never browsed has no URL, and inventing one would
        /// be worse than saying so.
        url: Option<String>,
        /// Whether a Chromium process is attached to this session.
        browser: bool,
    },
    /// One entry of the engine's activity log.
    ///
    /// `at`, `kind` and `detail` are `API.md` §5's three fields. `session` is
    /// one key more than §5 shows, and it is omitted when the event belongs to
    /// no session — so an event that §5 describes is encoded exactly as §5
    /// writes it. It is carried because the index page watches every session at
    /// once and has to know which row an action belongs to; without it, "last
    /// action" cannot be shown per session at all.
    Action {
        /// RFC 3339, UTC.
        at: String,
        /// `fetch`, `browse`, `crawl`, …
        kind: String,
        /// One line of human-readable detail.
        detail: String,
        /// Which session it ran on, when it ran on one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Something the operator should see: a refused click, a browser that
    /// could not be attached, a message that did not parse.
    Error {
        /// What went wrong.
        message: String,
    },
}

impl ServerMessage {
    /// An [`ServerMessage::Error`] from anything printable.
    pub fn error(message: impl std::fmt::Display) -> Self {
        Self::Error {
            message: message.to_string(),
        }
    }

    /// The `{"t":"action"}` form of an engine activity event.
    #[must_use]
    pub fn from_activity(event: &ActivityEvent) -> Self {
        Self::Action {
            at: event.at.clone(),
            kind: event.kind.clone(),
            detail: event.detail.clone(),
            session: event.session.clone(),
        }
    }
}

/// Which mouse event the dashboard is reporting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseKind {
    /// A button went down.
    Down,
    /// A button came up.
    Up,
    /// The pointer moved.
    Move,
    /// The wheel turned.
    Wheel,
}

impl MouseKind {
    /// The `browser-cdp` event this becomes.
    #[must_use]
    pub fn as_cdp(self) -> MouseEventKind {
        match self {
            Self::Down => MouseEventKind::Pressed,
            Self::Up => MouseEventKind::Released,
            Self::Move => MouseEventKind::Moved,
            Self::Wheel => MouseEventKind::Wheel,
        }
    }
}

/// Which key event the dashboard is reporting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    /// `keydown`.
    Down,
    /// `keyup`.
    Up,
    /// The event that actually types a character.
    Char,
}

impl KeyKind {
    /// The `browser-cdp` event this becomes.
    #[must_use]
    pub fn as_cdp(self) -> KeyEventKind {
        match self {
            Self::Down => KeyEventKind::Down,
            Self::Up => KeyEventKind::Up,
            Self::Char => KeyEventKind::Char,
        }
    }
}

/// Client → server.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ClientMessage {
    /// A mouse event, in rendered-image space.
    Mouse {
        /// Which event.
        kind: MouseKind,
        /// X in the frame's pixel space — see [`ServerMessage::Frame::w`].
        x: f64,
        /// Y in the frame's pixel space.
        y: f64,
        /// `"left"`, `"middle"`, `"right"`, `"back"`, `"forward"`, `"none"`.
        /// Absent means the primary button for a press or release, and no
        /// button at all for a move or a wheel.
        #[serde(default)]
        button: Option<String>,
        /// Click count; `2` is the second click of a double-click.
        #[serde(default)]
        clicks: Option<u32>,
        /// CDP's modifier bits: `Alt=1`, `Ctrl=2`, `Meta=4`, `Shift=8`.
        #[serde(default)]
        modifiers: Option<u8>,
        /// Horizontal wheel delta, in CSS pixels.
        #[serde(default)]
        dx: Option<f64>,
        /// Vertical wheel delta, in CSS pixels.
        #[serde(default)]
        dy: Option<f64>,
    },
    /// A keyboard event.
    Key {
        /// Which event.
        kind: KeyKind,
        /// The character to insert. **Required** for
        /// [`KeyKind::Char`] — that field is the whole reason a `char` event
        /// exists.
        #[serde(default)]
        text: Option<String>,
        /// DOM `event.key`.
        #[serde(default)]
        key: Option<String>,
        /// DOM `event.code`, the physical key.
        #[serde(default)]
        code: Option<String>,
        /// CDP's modifier bits. Absent leaves whatever `browser-cdp`'s key
        /// table infers, which is Shift for a key that needs it.
        #[serde(default)]
        modifiers: Option<u8>,
    },
    /// Insert a whole string at the focused element, as an IME would.
    ///
    /// One key more than `API.md` §5 defines, and it earns its place: this is
    /// how a **paste** reaches the remote page. A synthetic Ctrl+V does not,
    /// because Chrome will not run an editing command for a synthetic key
    /// event and the browser's clipboard is on the operator's machine, not the
    /// server's. Without it, logging in by hand means typing a forty-character
    /// password from a password manager one key at a time — which is the exact
    /// task this whole surface exists to make possible.
    ///
    /// A client written to §5 and unaware of this message still works; it
    /// simply cannot paste.
    Text {
        /// The string to insert.
        text: String,
    },
    /// Take the controls, or give them back.
    Control {
        /// `true` to control, `false` to go back to watching.
        take: bool,
    },
}

/// The button name a dashboard sends, as a [`Button`].
///
/// Unknown names become [`Button::None`] rather than an error: a mouse event
/// with a button this server does not know is still a real thing that
/// happened, and dropping it would be worse than dispatching it as a move.
#[must_use]
pub fn button_from_wire(name: &str) -> Button {
    match name {
        "left" => Button::Left,
        "middle" => Button::Middle,
        "right" => Button::Right,
        "back" => Button::Back,
        "forward" => Button::Forward,
        _ => Button::None,
    }
}

// ---------------------------------------------------------------------------
// Coordinates
// ---------------------------------------------------------------------------

/// The last frame's two coordinate spaces.
///
/// `image_*` is the JPEG the dashboard is displaying. `page_*` is Chrome's
/// viewport in CSS pixels, which is what `browser-cdp` dispatches into. They
/// differ by the device scale factor and again by [`MAX_FRAME_EDGE`], so the
/// ratio is not a constant and has to come from the frame itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// The frame image's width in pixels.
    pub image_width: f64,
    /// The frame image's height in pixels.
    pub image_height: f64,
    /// The viewport's width in CSS pixels.
    pub page_width: f64,
    /// The viewport's height in CSS pixels.
    pub page_height: f64,
}

impl Geometry {
    /// Read both spaces off a frame.
    ///
    /// When the image's own header cannot be read the image is assumed to be
    /// the viewport at 1:1, which makes [`Geometry::to_page_space`] the
    /// identity. That is the right failure: an unscaled click is off by the
    /// device scale factor, while a click scaled by a guess could land
    /// anywhere.
    #[must_use]
    pub fn from_frame(frame: &Frame) -> Self {
        let page_width = frame.metadata.device_width;
        let page_height = frame.metadata.device_height;
        let (image_width, image_height) = image_size(&frame.data)
            .map_or((page_width, page_height), |(width, height)| {
                (f64::from(width), f64::from(height))
            });
        Self {
            image_width,
            image_height,
            page_width,
            page_height,
        }
    }

    /// Map a point from rendered-image space into page space.
    ///
    /// Clamped to the viewport: a click on the last row of pixels must not
    /// round to a coordinate one pixel outside the page, where it would hit
    /// nothing. Non-finite input becomes `0.0` rather than reaching CDP.
    #[must_use]
    pub fn to_page_space(self, x: f64, y: f64) -> (f64, f64) {
        (
            project(x, self.image_width, self.page_width),
            project(y, self.image_height, self.page_height),
        )
    }
}

/// One axis of [`Geometry::to_page_space`].
fn project(value: f64, from: f64, to: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    // A zero-width frame is not a thing Chrome sends, but dividing by it would
    // produce an infinity that then reaches the browser as a CDP parameter.
    let scaled = if from > 0.0 && to > 0.0 {
        value * to / from
    } else {
        value
    };
    scaled.clamp(0.0, (to - 1.0).max(0.0))
}

/// An image's own width and height, out of the base64 CDP delivered.
///
/// Understands the two formats a screencast can produce. Only the first
/// [`HEADER_SCAN_BYTES`] are decoded.
///
/// ```
/// use browser_server::ws::image_size;
///
/// // A 1×1 PNG.
/// let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
/// assert_eq!(image_size(png), Some((1, 1)));
/// assert_eq!(image_size("not base64 at all!!"), None);
/// ```
#[must_use]
pub fn image_size(base64_data: &str) -> Option<(u32, u32)> {
    use base64::Engine as _;

    // Four base64 characters are exactly three bytes, so a prefix cut on a
    // multiple of four decodes without needing the padding that follows it.
    let take = base64_data.len().min(HEADER_SCAN_BYTES / 3 * 4) / 4 * 4;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data.get(..take)?)
        .ok()?;
    image_size_of_bytes(&bytes)
}

/// [`image_size`], on bytes that are already decoded.
#[must_use]
pub fn image_size_of_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        // IHDR is the first chunk and its position is fixed by the spec.
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        return Some((width, height));
    }
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    // Walk the JPEG marker segments to the start-of-frame, which is the only
    // one that carries the size.
    let mut at = 2usize;
    while at + 9 < bytes.len() {
        if bytes[at] != 0xff {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        // SOF0..SOF15, less the three codes in that range that are a Huffman
        // table, an arithmetic-coding table and a restart interval.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = u32::from(u16::from_be_bytes([bytes[at + 5], bytes[at + 6]]));
            let width = u32::from(u16::from_be_bytes([bytes[at + 7], bytes[at + 8]]));
            return Some((width, height));
        }
        at = at.checked_add(2)?.checked_add(length)?;
    }
    None
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Who may open a dashboard socket, and how they say so.
///
/// This surface carries its own check rather than sitting behind a layer,
/// because [`crate::api::with_layers`] deliberately applies none: a browser
/// opening `/` cannot set an `Authorization` header at all, and `/api/health`
/// has to answer without one. Shared with [`crate::dashboard`], which has the
/// same problem for the same reason.
pub mod auth {
    use axum::http::{header, HeaderMap};

    /// The name of the query parameter that carries the token.
    pub const TOKEN_PARAM: &str = "token";

    /// Whether a request may proceed.
    ///
    /// `configured` of `None` is the documented single-user mode: no token, no
    /// check, and a listener pinned to loopback with a start-up warning. See
    /// [`crate::config::ServerConfig::effective_bind`].
    ///
    /// A token is accepted from an `Authorization: Bearer …` header **or** the
    /// `?token=` query parameter. The query parameter is not decoration: a
    /// `<img>`, a top-level navigation and a `WebSocket` constructor can none
    /// of them set a header, so a browser-based dashboard has no other way to
    /// present a credential without a cookie. The cost is that the token lands
    /// in the URL bar and in browser history.
    ///
    /// ```
    /// use axum::http::HeaderMap;
    /// use browser_server::ws::auth::authorized;
    ///
    /// // No token configured: everything passes.
    /// assert!(authorized(None, &HeaderMap::new(), None));
    /// // Configured: the query parameter is enough.
    /// assert!(authorized(Some("s3cret"), &HeaderMap::new(), Some("s3cret")));
    /// assert!(!authorized(Some("s3cret"), &HeaderMap::new(), Some("wrong")));
    /// assert!(!authorized(Some("s3cret"), &HeaderMap::new(), None));
    /// ```
    #[must_use]
    pub fn authorized(
        configured: Option<&str>,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> bool {
        let Some(expected) = configured else {
            return true;
        };
        let offered = bearer(headers).or(query_token);
        offered.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
    }

    /// The token out of an `Authorization: Bearer …` header.
    #[must_use]
    pub fn bearer(headers: &HeaderMap) -> Option<&str> {
        let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
        let (scheme, token) = value.split_once(' ')?;
        scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| token.trim())
            .filter(|token| !token.is_empty())
    }

    /// Compare two byte strings without an early return.
    ///
    /// The same reasoning as `api.rs`: a very weak timing oracle over a
    /// network for a token this long, and constant-time is no harder to write.
    #[must_use]
    pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
        if left.len() != right.len() {
            return false;
        }
        left.iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
    }
}

// ---------------------------------------------------------------------------
// The route
// ---------------------------------------------------------------------------

/// `/ws`'s query string.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Params {
    /// Which session to watch. Absent means the index feed: the activity log
    /// with no frames.
    #[serde(default)]
    pub session: Option<String>,
    /// The bearer token, for clients that cannot set a header. See
    /// [`auth::authorized`].
    #[serde(default)]
    pub token: Option<String>,
}

/// The `/ws` route, ready to merge into the application router.
///
/// Per `API.md` §3a this returns a plain `Router` with the engine already
/// captured, and applies none of the layers `bin/serve.rs` puts around the
/// merged router. That matters here in one specific way: `tower-http`'s
/// timeout bounds the *upgrade*, which completes as soon as the `101` is
/// written, and not the socket that follows it.
pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(WS_PATH, get(upgrade))
        .with_state(engine)
}

async fn upgrade(
    State(engine): State<Arc<Engine>>,
    Query(params): Query<Params>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !auth::authorized(
        engine.config().api_token.as_deref(),
        &headers,
        params.token.as_deref(),
    ) {
        return ServerError::Unauthorized.into_response();
    }
    upgrade.on_upgrade(move |socket| serve(socket, engine, params.session))
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// The screencast stream, boxed so it can sit in an `Option`.
type Frames = std::pin::Pin<Box<dyn Stream<Item = Frame> + Send>>;

/// What one connected dashboard is doing.
struct Live {
    /// The session being watched, if the client named one that exists.
    session: Option<Arc<Session>>,
    /// The name it asked for, echoed in every status message.
    name: String,
    /// Whether the operator has taken the controls. Starts `false`.
    controlling: bool,
    /// The most recent frame's two coordinate spaces.
    geometry: Option<Geometry>,
    /// The running screencast, when a browser is attached.
    frames: Option<Frames>,
    /// The tab that screencast is of, for noticing when it is replaced.
    page: Option<CdpPage>,
}

/// Drive one dashboard socket until it closes.
///
/// One task, one `select!`. The alternative — a writer task and a reader task —
/// would need the geometry shared behind a lock for no gain: a send that blocks
/// means a client that has stopped reading, and there is no input to handle
/// from a client in that state.
async fn serve(socket: WebSocket, engine: Arc<Engine>, session: Option<String>) {
    let (mut sender, mut receiver) = socket.split();

    let name = session.clone().unwrap_or_default();
    let found = session
        .as_deref()
        .and_then(|name| engine.sessions().get(name));
    if let (Some(asked), None) = (session.as_deref(), found.as_ref()) {
        // Not fatal: the operator may have opened the dashboard first. The
        // socket stays up, the activity log still flows, and the next tick
        // picks the session up if it appears.
        if !send(
            &mut sender,
            &ServerMessage::error(format!(
                "no session named {asked:?} is open yet — it will appear here \
                 when one is"
            )),
        )
        .await
        {
            return;
        }
    }

    let mut live = Live {
        session: found,
        name,
        controlling: false,
        geometry: None,
        frames: None,
        page: None,
    };
    let mut activity = engine.subscribe();
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let alive = tokio::select! {
            incoming = receiver.next() => {
                handle_incoming(incoming, &mut live, &mut sender).await
            }
            event = activity.recv() => {
                handle_activity(event, &mut live, &mut sender).await
            }
            frame = next_frame(&mut live.frames) => {
                handle_frame(frame, &mut live, &mut sender).await
            }
            _ = ticker.tick() => {
                refresh(&engine, &mut live, &mut sender).await
            }
        };
        if !alive {
            break;
        }
    }

    // The stream's own `Drop` would ask Chrome to stop, but only on a
    // best-effort spawn. Doing it here means the browser is quiet before this
    // task ends.
    if live.frames.take().is_some() {
        if let Some(page) = live.page.as_ref() {
            let _ = page.stop_screencast().await;
        }
    }
}

/// The next screencast frame, or a future that never completes when there is
/// no screencast running.
///
/// Cancel-safe: `unfold`'s in-progress future lives in the stream, not in the
/// `Next` this returns, so a `select!` that drops it loses nothing.
async fn next_frame(frames: &mut Option<Frames>) -> Option<Frame> {
    match frames {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Send one message. `false` means the socket is gone and the loop should end.
async fn send(sender: &mut SplitSink<WebSocket, Message>, message: &ServerMessage) -> bool {
    let json = match serde_json::to_string(message) {
        Ok(json) => json,
        Err(error) => {
            // Only reachable if a message stops being serialisable, which is a
            // bug here rather than anything the client did.
            tracing::error!(%error, "a dashboard message would not serialise");
            return true;
        }
    };
    sender.send(Message::Text(json.into())).await.is_ok()
}

async fn handle_frame(
    frame: Option<Frame>,
    live: &mut Live,
    sender: &mut SplitSink<WebSocket, Message>,
) -> bool {
    let Some(frame) = frame else {
        // The stream ended: the tab closed, or the browser did. The next tick
        // decides whether there is a new one to attach to.
        live.frames = None;
        live.page = None;
        live.geometry = None;
        return true;
    };
    let geometry = Geometry::from_frame(&frame);
    live.geometry = Some(geometry);
    send(
        sender,
        &ServerMessage::Frame {
            data: frame.data,
            format: frame.format.as_str().to_owned(),
            w: geometry.image_width as u32,
            h: geometry.image_height as u32,
        },
    )
    .await
}

async fn handle_activity(
    event: std::result::Result<ActivityEvent, RecvError>,
    live: &mut Live,
    sender: &mut SplitSink<WebSocket, Message>,
) -> bool {
    match event {
        Ok(event) => {
            if relevant(&event, &live.name) {
                send(sender, &ServerMessage::from_activity(&event)).await
            } else {
                true
            }
        }
        // Lossy on purpose — see `Engine::subscribe`. A dashboard that fell
        // behind skips forward rather than the server buffering for it.
        Err(RecvError::Lagged(skipped)) => {
            tracing::debug!(skipped, "a dashboard lagged the activity log");
            true
        }
        Err(RecvError::Closed) => false,
    }
}

/// Whether an activity event belongs on this socket.
///
/// An index socket (no session) sees everything. A session socket sees its own
/// session's events plus the ones that belong to no session, because a
/// stateless `fetch` running alongside is still the agent working and the
/// operator is watching to see the agent work.
#[must_use]
pub fn relevant(event: &ActivityEvent, session: &str) -> bool {
    session.is_empty() || event.session.is_none() || event.session.as_deref() == Some(session)
}

async fn handle_incoming(
    incoming: Option<std::result::Result<Message, axum::Error>>,
    live: &mut Live,
    sender: &mut SplitSink<WebSocket, Message>,
) -> bool {
    let text = match incoming {
        Some(Ok(Message::Text(text))) => text,
        // Ping and pong are answered by axum; a binary frame is not part of
        // this protocol and is ignored rather than treated as an error.
        Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => return true,
        Some(Ok(Message::Close(_))) | None => return false,
        Some(Err(error)) => {
            tracing::debug!(%error, "a dashboard socket failed");
            return false;
        }
    };

    let message = match serde_json::from_str::<ClientMessage>(text.as_str()) {
        Ok(message) => message,
        Err(error) => {
            return send(
                sender,
                &ServerMessage::error(format!("unreadable: {error}")),
            )
            .await;
        }
    };

    match message {
        ClientMessage::Control { take } => {
            live.controlling = take;
            tracing::info!(session = %live.name, controlling = take, "dashboard control");
            true
        }
        ClientMessage::Mouse { .. } | ClientMessage::Key { .. } | ClientMessage::Text { .. }
            if !live.controlling =>
        {
            send(
                sender,
                &ServerMessage::error(
                    "input ignored: this socket is watching. Take control first.",
                ),
            )
            .await
        }
        other => dispatch(other, live, sender).await,
    }
}

/// Send one input event to the live tab.
async fn dispatch(
    message: ClientMessage,
    live: &mut Live,
    sender: &mut SplitSink<WebSocket, Message>,
) -> bool {
    let Some(page) = live
        .session
        .as_ref()
        .and_then(|session| session.live_page())
    else {
        return send(
            sender,
            &ServerMessage::error("no live browser to send that to"),
        )
        .await;
    };

    let outcome = match message {
        ClientMessage::Mouse {
            kind,
            x,
            y,
            button,
            clicks,
            modifiers,
            dx,
            dy,
        } => {
            // The one scaling step in the whole pipeline. Without a frame yet
            // there is nothing to scale by, and passing the point through is
            // the only honest option.
            let (x, y) = live
                .geometry
                .map_or((x, y), |geometry| geometry.to_page_space(x, y));
            let event = mouse_event(kind, x, y, button.as_deref(), clicks, modifiers, dx, dy);
            page.dispatch_mouse(event).await
        }
        ClientMessage::Key {
            kind,
            text,
            key,
            code,
            modifiers,
        } => match key_event(
            kind,
            text.as_deref(),
            key.as_deref(),
            code.as_deref(),
            modifiers,
        ) {
            Some(event) => page.dispatch_key(event).await,
            None => {
                return send(
                    sender,
                    &ServerMessage::error("a char event needs a `text` field"),
                )
                .await;
            }
        },
        ClientMessage::Text { text } => page.insert_text(&text).await,
        ClientMessage::Control { .. } => return true,
    };

    match outcome {
        Ok(()) => true,
        Err(error) => send(sender, &ServerMessage::error(error)).await,
    }
}

/// Build the `browser-cdp` mouse event a dashboard message describes.
///
/// Coordinates are already page space. Split out so the mapping is testable
/// without a socket or a browser.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn mouse_event(
    kind: MouseKind,
    x: f64,
    y: f64,
    button: Option<&str>,
    clicks: Option<u32>,
    modifiers: Option<u8>,
    dx: Option<f64>,
    dy: Option<f64>,
) -> MouseEvent {
    // A press or release with no button named is the primary one; a move or a
    // wheel with none is exactly that, no button.
    let default_button = match kind {
        MouseKind::Down | MouseKind::Up => Button::Left,
        MouseKind::Move | MouseKind::Wheel => Button::None,
    };
    MouseEvent {
        kind: kind.as_cdp(),
        x,
        y,
        button: button.map_or(default_button, button_from_wire),
        // CDP treats a click count of 0 as "not a click"; a dashboard that
        // omits it means one.
        click_count: clicks.unwrap_or(1).max(1),
        modifiers: Modifiers::from_bits(modifiers.unwrap_or(0)),
        delta_x: dx.unwrap_or(0.0),
        delta_y: dy.unwrap_or(0.0),
    }
}

/// Build the `browser-cdp` key event a dashboard message describes.
///
/// `None` when a [`KeyKind::Char`] arrived with no `text`, which is the one
/// case that cannot be dispatched: a `char` event without text types nothing,
/// so sending it would look like a silent failure to the operator.
#[must_use]
pub fn key_event(
    kind: KeyKind,
    text: Option<&str>,
    key: Option<&str>,
    code: Option<&str>,
    modifiers: Option<u8>,
) -> Option<KeyEvent> {
    let mut event = match kind {
        KeyKind::Char => KeyEvent::char(text?),
        KeyKind::Down | KeyKind::Up => {
            let mut event = KeyEvent::new(kind.as_cdp(), key.unwrap_or_default());
            // `browser-cdp`'s US-layout table fills `text` in for a printable
            // key, which makes a bare `keyDown` insert a character. The
            // dashboard sends a separate `char` event for the insertion, so a
            // `keyDown` that also typed would double every keystroke. Only an
            // explicit `text` from the client survives.
            event.text = text.map(ToOwned::to_owned);
            event
        }
    };
    // The dashboard knows the physical key from `event.code`; the US-layout
    // table `browser-cdp` falls back to does not, for anything else.
    if let Some(code) = code.filter(|code| !code.is_empty()) {
        event = event.with_code(code);
    }
    if let Some(bits) = modifiers {
        event = event.with_modifiers(Modifiers::from_bits(bits));
    }
    Some(event)
}

/// Re-read the session, send a status, and attach or detach the screencast.
async fn refresh(
    engine: &Arc<Engine>,
    live: &mut Live,
    sender: &mut SplitSink<WebSocket, Message>,
) -> bool {
    // A session named before it existed is picked up here.
    if live.session.is_none() && !live.name.is_empty() {
        live.session = engine.sessions().get(&live.name);
    }

    let (browser, url) = match live.session.as_ref() {
        Some(session) => {
            let browser = session.browser_is_live().await;
            let url = match session.live_page() {
                // A failing `url()` is how a tab that died under us announces
                // itself; the screencast attached to it is dropped below.
                Some(page) => page.url().await.ok(),
                None => None,
            };
            (browser, url)
        }
        None => (false, None),
    };

    if !send(
        sender,
        &ServerMessage::Status {
            session: live.name.clone(),
            url,
            browser,
        },
    )
    .await
    {
        return false;
    }

    if live.name.is_empty() {
        // An index socket never screencasts.
        return true;
    }

    let page = live
        .session
        .as_ref()
        .and_then(|session| session.live_page());
    match page {
        None => {
            live.frames = None;
            live.page = None;
            live.geometry = None;
            true
        }
        Some(page) if !same_tab(live.page.as_ref(), &page) => {
            // A new tab: start over. `CdpPage` is an `Arc` handle, so identity
            // is the pointer and not the URL.
            live.frames = None;
            live.geometry = None;
            let opts = screencast_opts();
            match page.start_screencast(opts).await {
                Ok(stream) => {
                    live.frames = Some(Box::pin(stream));
                    live.page = Some(page);
                    true
                }
                Err(error) => {
                    // Remember the page anyway, so this is reported once
                    // rather than every two seconds.
                    live.page = Some(page);
                    send(
                        sender,
                        &ServerMessage::error(format!("cannot watch this tab: {error}")),
                    )
                    .await
                }
            }
        }
        Some(_) => true,
    }
}

/// Whether `page` is the tab a screencast is already running on.
fn same_tab(current: Option<&CdpPage>, page: &CdpPage) -> bool {
    current.is_some_and(|current| std::ptr::eq(current.raw(), page.raw()))
}

/// What the dashboard asks Chrome for.
///
/// JPEG at `browser-cdp`'s default quality, capped at [`MAX_FRAME_EDGE`]. See
/// that constant for why the cap is worth having.
#[must_use]
pub fn screencast_opts() -> ScreencastOpts {
    ScreencastOpts {
        max_width: Some(MAX_FRAME_EDGE),
        max_height: Some(MAX_FRAME_EDGE),
        ..ScreencastOpts::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ClientMessage {
        serde_json::from_str(json).expect("the message parses")
    }

    // -- the wire format, exactly as `API.md` §5 writes it -------------------

    #[test]
    fn the_server_messages_encode_to_the_documented_shapes() {
        let frame = ServerMessage::Frame {
            data: "AAAA".to_owned(),
            format: "jpeg".to_owned(),
            w: 1280,
            h: 720,
        };
        assert_eq!(
            serde_json::to_value(&frame).expect("serialises"),
            serde_json::json!({"t":"frame","data":"AAAA","format":"jpeg","w":1280,"h":720}),
        );

        let status = ServerMessage::Status {
            session: "shop".to_owned(),
            url: Some("https://example.test/".to_owned()),
            browser: true,
        };
        assert_eq!(
            serde_json::to_value(&status).expect("serialises"),
            serde_json::json!({
                "t":"status","session":"shop","url":"https://example.test/","browser":true
            }),
        );

        let action = ServerMessage::from_activity(&ActivityEvent {
            at: "2026-08-27T09:00:00.000Z".to_owned(),
            kind: "browse".to_owned(),
            session: None,
            detail: "https://example.test/".to_owned(),
        });
        assert_eq!(
            serde_json::to_value(&action).expect("serialises"),
            serde_json::json!({
                "t":"action","at":"2026-08-27T09:00:00.000Z","kind":"browse",
                "detail":"https://example.test/"
            }),
            "an event with no session is encoded exactly as API.md §5 writes it",
        );

        let scoped = ServerMessage::from_activity(&ActivityEvent {
            at: "2026-08-27T09:00:00.000Z".to_owned(),
            kind: "browse".to_owned(),
            session: Some("shop".to_owned()),
            detail: "https://example.test/".to_owned(),
        });
        assert_eq!(
            serde_json::to_value(&scoped).expect("serialises")["session"],
            "shop",
            "and one that has a session says which, for the index table",
        );

        assert_eq!(
            serde_json::to_value(ServerMessage::error("boom")).expect("serialises"),
            serde_json::json!({"t":"error","message":"boom"}),
        );
    }

    #[test]
    fn a_session_with_no_browser_reports_a_null_url_rather_than_an_empty_one() {
        let status = ServerMessage::Status {
            session: "cold".to_owned(),
            url: None,
            browser: false,
        };
        assert_eq!(
            serde_json::to_value(&status).expect("serialises"),
            serde_json::json!({"t":"status","session":"cold","url":null,"browser":false}),
        );
    }

    #[test]
    fn the_documented_client_messages_parse() {
        assert_eq!(
            parse(r#"{"t":"mouse","kind":"down","x":410,"y":233,"button":"left","clicks":1}"#),
            ClientMessage::Mouse {
                kind: MouseKind::Down,
                x: 410.0,
                y: 233.0,
                button: Some("left".to_owned()),
                clicks: Some(1),
                modifiers: None,
                dx: None,
                dy: None,
            },
        );
        assert_eq!(
            parse(r#"{"t":"key","kind":"char","text":"a","key":"a","code":"KeyA"}"#),
            ClientMessage::Key {
                kind: KeyKind::Char,
                text: Some("a".to_owned()),
                key: Some("a".to_owned()),
                code: Some("KeyA".to_owned()),
                modifiers: None,
            },
        );
        assert_eq!(
            parse(r#"{"t":"control","take":true}"#),
            ClientMessage::Control { take: true },
        );
    }

    #[test]
    fn a_mouse_message_needs_only_its_kind_and_its_point() {
        assert_eq!(
            parse(r#"{"t":"mouse","kind":"move","x":1,"y":2}"#),
            ClientMessage::Mouse {
                kind: MouseKind::Move,
                x: 1.0,
                y: 2.0,
                button: None,
                clicks: None,
                modifiers: None,
                dx: None,
                dy: None,
            },
        );
    }

    #[test]
    fn an_unknown_message_is_rejected_rather_than_guessed_at() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"t":"launch_missiles"}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"t":"mouse","kind":"down"}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"t":"key","kind":"sideways"}"#).is_err());
    }

    // -- coordinates --------------------------------------------------------

    fn geometry(image: (f64, f64), page: (f64, f64)) -> Geometry {
        Geometry {
            image_width: image.0,
            image_height: image.1,
            page_width: page.0,
            page_height: page.1,
        }
    }

    #[test]
    fn a_click_on_a_double_resolution_frame_halves_into_page_space() {
        // What `browser-cdp` actually produces: a 1280x720 CSS viewport
        // rendered at a device scale factor of 2.
        let geometry = geometry((2560.0, 1440.0), (1280.0, 720.0));
        assert_eq!(geometry.to_page_space(0.0, 0.0), (0.0, 0.0));
        assert_eq!(geometry.to_page_space(1280.0, 720.0), (640.0, 360.0));
        assert_eq!(geometry.to_page_space(820.0, 466.0), (410.0, 233.0));
    }

    #[test]
    fn a_capped_frame_scales_by_its_own_size_and_not_by_a_constant() {
        // `MAX_FRAME_EDGE` shrank the 2560x1440 frame to 1280x720, so the
        // device scale factor is no longer the ratio.
        let geometry = geometry((1280.0, 720.0), (1280.0, 720.0));
        assert_eq!(geometry.to_page_space(410.0, 233.0), (410.0, 233.0));

        let third = geometry.to_page_space(320.0, 180.0);
        assert_eq!(third, (320.0, 180.0));
    }

    #[test]
    fn a_click_on_the_last_pixel_stays_inside_the_viewport() {
        let geometry = geometry((2560.0, 1440.0), (1280.0, 720.0));
        // The bottom-right corner of the image maps to 1280,720 — one past the
        // last addressable CSS pixel, where a click would hit nothing.
        assert_eq!(geometry.to_page_space(2560.0, 1440.0), (1279.0, 719.0));
        assert_eq!(geometry.to_page_space(99_999.0, 99_999.0), (1279.0, 719.0));
        assert_eq!(geometry.to_page_space(-40.0, -40.0), (0.0, 0.0));
    }

    #[test]
    fn a_nonsense_coordinate_never_reaches_cdp() {
        let geometry = geometry((2560.0, 1440.0), (1280.0, 720.0));
        assert_eq!(geometry.to_page_space(f64::NAN, f64::INFINITY), (0.0, 0.0));
        assert_eq!(
            geometry.to_page_space(f64::NEG_INFINITY, f64::NAN),
            (0.0, 0.0)
        );
    }

    #[test]
    fn a_degenerate_frame_falls_back_to_passing_the_point_through() {
        let geometry = geometry((0.0, 0.0), (1280.0, 720.0));
        assert_eq!(geometry.to_page_space(410.0, 233.0), (410.0, 233.0));

        // And a viewport of nothing clamps to the origin rather than dividing.
        let nothing = geometry.to_page_space(410.0, 233.0);
        assert!(nothing.0.is_finite() && nothing.1.is_finite());
    }

    // -- image headers ------------------------------------------------------

    #[test]
    fn a_png_header_gives_its_size() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2560u32.to_be_bytes());
        png.extend_from_slice(&1440u32.to_be_bytes());
        assert_eq!(image_size_of_bytes(&png), Some((2560, 1440)));
    }

    #[test]
    fn a_jpeg_start_of_frame_gives_its_size_past_the_segments_before_it() {
        let mut jpeg = vec![0xff, 0xd8];
        // An APP0 segment, so the scan has to walk past something first.
        jpeg.extend_from_slice(&[0xff, 0xe0, 0x00, 0x10]);
        jpeg.extend_from_slice(&[0u8; 14]);
        // SOF0: length, precision, height, width, components.
        jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&1440u16.to_be_bytes());
        jpeg.extend_from_slice(&2560u16.to_be_bytes());
        jpeg.extend_from_slice(&[0u8; 10]);
        assert_eq!(image_size_of_bytes(&jpeg), Some((2560, 1440)));
    }

    #[test]
    fn a_huffman_table_is_not_mistaken_for_a_start_of_frame() {
        let mut jpeg = vec![0xff, 0xd8];
        // 0xC4 sits in the SOF marker range but is a Huffman table.
        jpeg.extend_from_slice(&[0xff, 0xc4, 0x00, 0x08]);
        jpeg.extend_from_slice(&[0u8; 6]);
        jpeg.extend_from_slice(&[0xff, 0xc2, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&720u16.to_be_bytes());
        jpeg.extend_from_slice(&1280u16.to_be_bytes());
        jpeg.extend_from_slice(&[0u8; 10]);
        assert_eq!(
            image_size_of_bytes(&jpeg),
            Some((1280, 720)),
            "the progressive SOF2 after it is the real one",
        );
    }

    #[test]
    fn something_that_is_not_an_image_has_no_size() {
        assert_eq!(image_size_of_bytes(b"<html>"), None);
        assert_eq!(image_size_of_bytes(&[]), None);
        // A truncated JPEG: the marker walk must stop, not spin or panic.
        assert_eq!(image_size_of_bytes(&[0xff, 0xd8, 0xff, 0xc0]), None);
    }

    // -- input mapping ------------------------------------------------------

    #[test]
    fn a_press_with_no_button_named_is_the_primary_one() {
        let event = mouse_event(MouseKind::Down, 10.0, 20.0, None, None, None, None, None);
        assert_eq!(event.kind, MouseEventKind::Pressed);
        assert_eq!(event.button, Button::Left);
        assert_eq!(event.click_count, 1);
        assert_eq!(event.buttons(), 1);
    }

    #[test]
    fn a_move_with_no_button_named_holds_nothing() {
        let event = mouse_event(MouseKind::Move, 10.0, 20.0, None, None, None, None, None);
        assert_eq!(event.kind, MouseEventKind::Moved);
        assert_eq!(event.button, Button::None);
        assert_eq!(event.buttons(), 0, "a hover, not a drag");
    }

    #[test]
    fn a_named_button_and_a_double_click_survive_the_mapping() {
        let event = mouse_event(
            MouseKind::Down,
            1.0,
            2.0,
            Some("right"),
            Some(2),
            Some(8),
            None,
            None,
        );
        assert_eq!(event.button, Button::Right);
        assert_eq!(event.click_count, 2);
        assert_eq!(event.modifiers, Modifiers::SHIFT);
    }

    #[test]
    fn a_zero_click_count_becomes_one_because_cdp_reads_zero_as_not_a_click() {
        let event = mouse_event(MouseKind::Down, 1.0, 2.0, None, Some(0), None, None, None);
        assert_eq!(event.click_count, 1);
    }

    #[test]
    fn a_wheel_carries_its_deltas() {
        let event = mouse_event(
            MouseKind::Wheel,
            1.0,
            2.0,
            None,
            None,
            None,
            Some(0.0),
            Some(-120.0),
        );
        assert_eq!(event.kind, MouseEventKind::Wheel);
        assert_eq!(event.delta_y, -120.0);
    }

    #[test]
    fn an_unknown_button_name_is_no_button_rather_than_a_dropped_event() {
        assert_eq!(button_from_wire("thumb"), Button::None);
        assert_eq!(button_from_wire("left"), Button::Left);
        assert_eq!(button_from_wire("forward"), Button::Forward);
    }

    #[test]
    fn a_char_event_carries_its_text_which_is_what_types() {
        let event = key_event(KeyKind::Char, Some("a"), Some("a"), Some("KeyA"), None)
            .expect("a char event with text");
        assert_eq!(event.kind, KeyEventKind::Char);
        assert_eq!(event.text.as_deref(), Some("a"));
        assert_eq!(event.code, "KeyA");
        assert_eq!(event.windows_virtual_key_code, Some(65));
    }

    #[test]
    fn a_char_event_with_no_text_is_refused_instead_of_typing_nothing() {
        assert!(key_event(KeyKind::Char, None, Some("a"), Some("KeyA"), None).is_none());
    }

    #[test]
    fn a_key_down_gets_its_code_and_key_code_from_the_table() {
        let event = key_event(KeyKind::Down, None, Some("Enter"), None, None).expect("a key event");
        assert_eq!(event.kind, KeyEventKind::Down);
        assert_eq!(event.code, "Enter");
        assert_eq!(event.windows_virtual_key_code, Some(13));
    }

    #[test]
    fn the_dashboards_own_code_wins_over_the_us_layout_guess() {
        // A French keyboard types `a` on the physical `KeyQ`. The browser knows
        // that and says so; the fallback table cannot.
        let event = key_event(KeyKind::Down, Some("a"), Some("a"), Some("KeyQ"), Some(0))
            .expect("a key event");
        assert_eq!(event.code, "KeyQ");
        assert_eq!(event.text.as_deref(), Some("a"));
    }

    #[test]
    fn explicit_modifiers_replace_the_shift_the_table_inferred() {
        let inferred = key_event(KeyKind::Down, None, Some("A"), None, None).expect("a key event");
        assert_eq!(inferred.modifiers, Modifiers::SHIFT);

        let stated = key_event(KeyKind::Down, None, Some("A"), None, Some(2)).expect("a key event");
        assert_eq!(stated.modifiers, Modifiers::CTRL);
    }

    // -- activity filtering -------------------------------------------------

    fn activity(session: Option<&str>) -> ActivityEvent {
        ActivityEvent {
            at: "2026-08-27T09:00:00.000Z".to_owned(),
            kind: "browse".to_owned(),
            session: session.map(ToOwned::to_owned),
            detail: "https://example.test/".to_owned(),
        }
    }

    #[test]
    fn an_index_socket_sees_every_session() {
        assert!(relevant(&activity(Some("shop")), ""));
        assert!(relevant(&activity(None), ""));
    }

    #[test]
    fn a_session_socket_sees_its_own_and_the_session_less_ones() {
        assert!(relevant(&activity(Some("shop")), "shop"));
        assert!(
            relevant(&activity(None), "shop"),
            "a stateless fetch is still the agent working",
        );
        assert!(!relevant(&activity(Some("bank")), "shop"));
    }

    // -- auth ---------------------------------------------------------------

    #[test]
    fn a_header_token_is_accepted_as_well_as_a_query_one() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer s3cret".parse().expect("valid"),
        );
        assert!(auth::authorized(Some("s3cret"), &headers, None));
        assert!(!auth::authorized(Some("other"), &headers, None));
    }

    #[test]
    fn a_wrong_header_does_not_fall_through_to_a_right_query_parameter() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer wrong".parse().expect("valid"),
        );
        assert!(
            !auth::authorized(Some("s3cret"), &headers, Some("s3cret")),
            "a presented credential is the one that is checked",
        );
    }

    #[test]
    fn the_screencast_the_dashboard_asks_for_is_capped_jpeg() {
        let opts = screencast_opts();
        assert_eq!(opts.max_width, Some(MAX_FRAME_EDGE));
        assert_eq!(opts.max_height, Some(MAX_FRAME_EDGE));
        assert_eq!(opts.format, browser_cdp::screencast::Format::Jpeg);
    }
}
