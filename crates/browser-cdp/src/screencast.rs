//! `Page.startScreencast` — the live view the operator dashboard renders.
//!
//! # What Chrome expects of us
//!
//! `Page.startScreencast` makes Chrome push a `Page.screencastFrame` event on
//! every compositor frame. **Every one of them has to be answered with
//! `Page.screencastFrameAck` carrying that frame's `sessionId`**: Chrome keeps
//! at most one frame in flight, so a consumer that forgets the ack sees one or
//! two frames and then silence. [`CdpPage::start_screencast`] therefore acks
//! from its own pump task, before the frame ever reaches the caller — a slow
//! reader can never stall the browser.
//!
//! # Buffering policy
//!
//! The pump hands frames to the caller through a queue of exactly
//! [`FRAME_BUFFER`] frames. When it is full the **oldest** frame is dropped to
//! make room for the newest. Both halves of that matter:
//!
//! * *Bounded* — the caller is a websocket to a browser tab somewhere on the
//!   operator's network. A client that stops reading must not turn into an
//!   ever-growing pile of half-megabyte JPEGs in this process.
//! * *Newest wins* — this is a live view. A frame from four seconds ago has no
//!   value once a newer one exists; showing it would only add latency the
//!   viewer then never recovers from.
//!
//! Two rather than one, because one frame is routinely in the caller's hands
//! (being base64-forwarded down a socket) while the next arrives, and dropping
//! *that* one would halve the frame rate of an otherwise healthy viewer.
//!
//! # Format
//!
//! [`Format::Jpeg`] at [`DEFAULT_QUALITY`] by default. These frames cross a
//! websocket several times a second; PNG is available for when a pixel-exact
//! frame is worth the bytes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, ScreencastFrameMetadata, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams,
};
use chromiumoxide::Page;
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::cdp::CdpPage;
use crate::error::{BrowserError, Result};

/// The JPEG quality a screencast uses unless the caller asks for another.
///
/// Modest on purpose: at 1280×720×2 a quality-90 frame is roughly four times
/// the bytes of a quality-60 one and the difference is invisible on a live
/// view of a web page, which is mostly flat colour and text.
pub const DEFAULT_QUALITY: u32 = 60;

/// How many frames the pump holds for a caller that has not read yet.
///
/// See the module docs for why it is two and why the *oldest* frame is the one
/// that goes when a third arrives.
pub const FRAME_BUFFER: usize = 2;

// ---------------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------------

/// The image compression a screencast asks Chrome for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Format {
    /// JPEG, the default: small enough to push over a websocket continuously.
    #[default]
    Jpeg,
    /// PNG — lossless, and several times the bytes.
    Png,
}

impl Format {
    /// The CDP spelling.
    #[must_use]
    pub fn as_cdp(self) -> StartScreencastFormat {
        match self {
            Self::Jpeg => StartScreencastFormat::Jpeg,
            Self::Png => StartScreencastFormat::Png,
        }
    }

    /// The wire name the dashboard protocol uses (`"jpeg"` / `"png"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        }
    }
}

/// What Chrome says the frame is a picture *of*.
///
/// This is the whole coordinate story: the image the dashboard renders is in
/// device pixels, while [`CdpPage::dispatch_mouse`] wants CSS pixels of the
/// page. `device_width`/`device_height` are the viewport in DIP and
/// `scroll_offset_x`/`scroll_offset_y` are where that viewport sits in the
/// document, which is everything the server needs to map a click back.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameMetadata {
    /// Top offset in DIP.
    pub offset_top: f64,
    /// Page scale factor (pinch zoom), `1.0` on a desktop page.
    pub page_scale_factor: f64,
    /// Viewport width in DIP.
    pub device_width: f64,
    /// Viewport height in DIP.
    pub device_height: f64,
    /// Horizontal scroll position in CSS pixels.
    pub scroll_offset_x: f64,
    /// Vertical scroll position in CSS pixels.
    pub scroll_offset_y: f64,
    /// Frame swap timestamp, when Chrome supplied one.
    pub timestamp: Option<f64>,
}

impl FrameMetadata {
    /// Read the metadata out of a `Page.screencastFrame` event.
    #[must_use]
    pub fn from_cdp(metadata: &ScreencastFrameMetadata) -> Self {
        Self {
            offset_top: metadata.offset_top,
            page_scale_factor: metadata.page_scale_factor,
            device_width: metadata.device_width,
            device_height: metadata.device_height,
            scroll_offset_x: metadata.scroll_offset_x,
            scroll_offset_y: metadata.scroll_offset_y,
            timestamp: metadata.timestamp.as_ref().map(|stamp| *stamp.inner()),
        }
    }
}

/// One screencast frame, already acknowledged.
///
/// [`Frame::data`] is kept as the base64 string CDP delivered rather than
/// decoded bytes, because its next stop is a JSON websocket message that wants
/// base64 again. Nothing in the pipeline needs the pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// Base64, exactly as CDP delivered it.
    pub data: String,
    /// The format the screencast was started with.
    ///
    /// Chrome does not repeat it per frame; this is what we asked for.
    pub format: Format,
    /// What the frame is a picture of.
    pub metadata: FrameMetadata,
}

impl Frame {
    /// Build a frame from a `Page.screencastFrame` event.
    #[must_use]
    pub fn from_event(event: &EventScreencastFrame, format: Format) -> Self {
        Self {
            // `Binary` is `AsRef<str>` *and* `AsRef<[u8]>`; we want the string.
            data: {
                let base64: &str = event.data.as_ref();
                base64.to_owned()
            },
            format,
            metadata: FrameMetadata::from_cdp(&event.metadata),
        }
    }

    /// The decoded image bytes.
    ///
    /// `None` when [`Frame::data`] is not valid base64, which a healthy Chrome
    /// never produces. Provided for callers that want the pixels — writing a
    /// frame to disk, say — rather than the string.
    #[must_use]
    pub fn decoded(&self) -> Option<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(self.data.as_bytes())
            .ok()
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// What to ask `Page.startScreencast` for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreencastOpts {
    /// Image compression.
    pub format: Format,
    /// Compression quality, `0..=100`; clamped, and ignored for PNG.
    pub quality: u32,
    /// Cap the frame width in device pixels.
    pub max_width: Option<u32>,
    /// Cap the frame height in device pixels.
    pub max_height: Option<u32>,
    /// Send every n-th compositor frame; `1` is every frame.
    pub every_nth_frame: u32,
}

impl Default for ScreencastOpts {
    fn default() -> Self {
        Self {
            format: Format::Jpeg,
            quality: DEFAULT_QUALITY,
            max_width: None,
            max_height: None,
            every_nth_frame: 1,
        }
    }
}

/// The `Page.startScreencast` parameters for `opts`.
///
/// Split out from [`CdpPage::start_screencast`] so the wire shape can be
/// pinned by a test with no browser in it.
#[must_use]
pub fn screencast_params(opts: &ScreencastOpts) -> StartScreencastParams {
    StartScreencastParams {
        format: Some(opts.format.as_cdp()),
        quality: Some(i64::from(opts.quality.min(100))),
        max_width: opts.max_width.map(i64::from),
        max_height: opts.max_height.map(i64::from),
        // Never zero: Chrome divides by it.
        every_nth_frame: Some(i64::from(opts.every_nth_frame.max(1))),
    }
}

// ---------------------------------------------------------------------------
// The bounded, stale-dropping queue
// ---------------------------------------------------------------------------

/// The hand-off between the ack pump and the caller's stream.
#[derive(Debug)]
struct FrameSink {
    frames: Mutex<VecDeque<Frame>>,
    ready: Notify,
    closed: AtomicBool,
    dropped: AtomicU64,
}

impl FrameSink {
    fn new() -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(FRAME_BUFFER)),
            ready: Notify::new(),
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        }
    }

    /// Queue a frame, evicting the oldest ones until it fits.
    fn push(&self, frame: Frame) {
        {
            let mut frames = self.frames.lock();
            while frames.len() >= FRAME_BUFFER {
                frames.pop_front();
                let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::trace!("screencast: dropped a stale frame ({total} so far)");
            }
            frames.push_back(frame);
        }
        // `notify_one` stores a permit when nobody is waiting, so a consumer
        // that has not reached its `await` yet still wakes up.
        self.ready.notify_one();
    }

    fn pop(&self) -> Option<Frame> {
        self.frames.lock().pop_front()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_one();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// The pump task, owned by the stream so that dropping the stream ends it.
#[derive(Debug)]
struct ScreencastPump {
    sink: Arc<FrameSink>,
    task: JoinHandle<()>,
    page: Page,
}

impl Drop for ScreencastPump {
    fn drop(&mut self) {
        self.task.abort();
        // Nothing is acking any more, so Chrome would sit waiting for an ack
        // that cannot arrive. Tell it to stop, best-effort: `Drop` cannot
        // await, and there may be no runtime left to spawn on.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let page = self.page.clone();
            handle.spawn(async move {
                let _ = page.execute(StopScreencastParams::default()).await;
            });
        }
    }
}

// ---------------------------------------------------------------------------
// The page methods
// ---------------------------------------------------------------------------

impl CdpPage {
    /// Start a screencast and stream its frames.
    ///
    /// Frames are acknowledged by an internal task the moment they arrive, so
    /// the caller can read at whatever rate it likes without stalling Chrome;
    /// see the module docs for the buffering policy that makes that safe.
    ///
    /// Dropping the returned stream stops the pump and asks Chrome to stop the
    /// screencast. [`CdpPage::stop_screencast`] is the awaitable way to do the
    /// same thing.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when the `Page.screencastFrame` listener cannot be
    /// registered or `Page.startScreencast` is refused.
    pub async fn start_screencast(
        &self,
        opts: ScreencastOpts,
    ) -> Result<impl Stream<Item = Frame> + Send + Unpin + 'static> {
        let page = self.raw().clone();

        // Register before starting, or the first frames land nowhere.
        let mut events = page
            .event_listener::<EventScreencastFrame>()
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.screencastFrame listener".to_owned(),
                message: error.to_string(),
            })?;

        let sink = Arc::new(FrameSink::new());
        let format = opts.format;
        let pump_sink = Arc::clone(&sink);
        let pump_page = page.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                pump_sink.push(Frame::from_event(&event, format));
                // The ack is not optional. Chrome keeps one frame in flight
                // and simply stops sending without it.
                if let Err(error) = pump_page
                    .execute(ScreencastFrameAckParams::new(event.session_id))
                    .await
                {
                    tracing::debug!(
                        "Page.screencastFrameAck({}) failed, ending the screencast: {error}",
                        event.session_id,
                    );
                    break;
                }
            }
            pump_sink.close();
        });

        // Built before the `startScreencast` call so that a refusal still
        // takes the pump task down: dropping `pump` aborts it.
        let pump = ScreencastPump { sink, task, page };
        execute(&pump.page, screencast_params(&opts), "Page.startScreencast").await?;

        Ok(Box::pin(futures::stream::unfold(pump, |pump| async move {
            loop {
                if let Some(frame) = pump.sink.pop() {
                    return Some((frame, pump));
                }
                if pump.sink.is_closed() {
                    return None;
                }
                pump.sink.ready.notified().await;
            }
        })))
    }

    /// Stop a screencast started by [`CdpPage::start_screencast`].
    ///
    /// Chrome tolerates this on a page that is not screencasting, so it is
    /// safe on a teardown path that is not sure.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Page.stopScreencast` fails.
    pub async fn stop_screencast(&self) -> Result<()> {
        execute(
            self.raw(),
            StopScreencastParams::default(),
            "Page.stopScreencast",
        )
        .await
    }
}

/// `page.execute`, with this crate's error on top.
///
/// A local twin of the one in [`crate::cdp`], which is private to that module.
async fn execute<T>(page: &Page, command: T, context: &str) -> Result<()>
where
    T: chromiumoxide::types::Command,
{
    page.execute(command)
        .await
        .map_err(|error| BrowserError::Cdp {
            context: context.to_owned(),
            message: error.to_string(),
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: &str) -> Frame {
        Frame {
            data: tag.to_owned(),
            format: Format::Jpeg,
            metadata: FrameMetadata::default(),
        }
    }

    #[test]
    fn the_default_options_are_jpeg_at_a_modest_quality_and_every_frame() {
        let opts = ScreencastOpts::default();
        assert_eq!(opts.format, Format::Jpeg);
        assert_eq!(opts.quality, DEFAULT_QUALITY);
        assert_eq!(opts.every_nth_frame, 1);
        assert_eq!(opts.max_width, None);
        assert_eq!(opts.max_height, None);
    }

    #[test]
    fn the_default_options_serialise_to_the_start_screencast_call_we_mean() {
        let params = screencast_params(&ScreencastOpts::default());
        assert_eq!(
            serde_json::to_value(&params).expect("the params serialise"),
            serde_json::json!({
                "format": "jpeg",
                "quality": 60,
                "everyNthFrame": 1,
            }),
        );
    }

    #[test]
    fn the_size_caps_only_appear_when_they_are_asked_for() {
        let params = screencast_params(&ScreencastOpts {
            format: Format::Png,
            quality: 100,
            max_width: Some(640),
            max_height: Some(360),
            every_nth_frame: 3,
        });
        assert_eq!(
            serde_json::to_value(&params).expect("the params serialise"),
            serde_json::json!({
                "format": "png",
                "quality": 100,
                "maxWidth": 640,
                "maxHeight": 360,
                "everyNthFrame": 3,
            }),
        );
    }

    #[test]
    fn an_out_of_range_quality_is_clamped_and_every_nth_frame_is_never_zero() {
        let params = screencast_params(&ScreencastOpts {
            quality: 4_000,
            every_nth_frame: 0,
            ..ScreencastOpts::default()
        });
        assert_eq!(params.quality, Some(100));
        assert_eq!(params.every_nth_frame, Some(1), "Chrome divides by this");
    }

    #[test]
    fn the_ack_carries_the_frames_session_id() {
        let ack = ScreencastFrameAckParams::new(7_i64);
        assert_eq!(
            serde_json::to_value(&ack).expect("the ack serialises"),
            serde_json::json!({ "sessionId": 7 }),
        );
    }

    #[test]
    fn a_frame_keeps_the_base64_and_the_metadata_cdp_delivered() {
        // Literally the shape Chrome puts on the wire.
        let event: EventScreencastFrame = serde_json::from_value(serde_json::json!({
            "data": "/9j/4AAQ",
            "metadata": {
                "offsetTop": 0.0,
                "pageScaleFactor": 1.0,
                "deviceWidth": 1280.0,
                "deviceHeight": 720.0,
                "scrollOffsetX": 0.0,
                "scrollOffsetY": 120.0,
                "timestamp": 1_700_000_000.5,
            },
            "sessionId": 3,
        }))
        .expect("the event parses");

        let frame = Frame::from_event(&event, Format::Jpeg);
        assert_eq!(frame.data, "/9j/4AAQ", "still base64, not decoded");
        assert_eq!(frame.format, Format::Jpeg);
        assert_eq!(frame.metadata.device_width, 1280.0);
        assert_eq!(frame.metadata.device_height, 720.0);
        assert_eq!(frame.metadata.scroll_offset_y, 120.0);
        assert_eq!(frame.metadata.page_scale_factor, 1.0);
        assert_eq!(frame.metadata.timestamp, Some(1_700_000_000.5));
        // …and the pixels are still recoverable: this is a JPEG SOI marker.
        assert_eq!(
            frame.decoded().expect("valid base64")[..2],
            [0xff, 0xd8],
            "the base64 really is the image",
        );
    }

    #[test]
    fn a_full_buffer_drops_the_oldest_frame_not_the_newest() {
        let sink = FrameSink::new();
        for tag in ["a", "b", "c", "d", "e"] {
            sink.push(frame(tag));
        }

        assert_eq!(sink.frames.lock().len(), FRAME_BUFFER, "bounded");
        assert_eq!(
            sink.dropped.load(Ordering::Relaxed),
            3,
            "five frames pushed, two kept",
        );
        assert_eq!(sink.pop().expect("a frame").data, "d");
        assert_eq!(
            sink.pop().expect("a frame").data,
            "e",
            "the newest survived"
        );
        assert!(sink.pop().is_none());
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_pump_closes_the_sink() {
        let sink = Arc::new(FrameSink::new());
        sink.push(frame("only"));
        sink.close();

        // The same loop `start_screencast` returns, without a browser.
        let stream = futures::stream::unfold(Arc::clone(&sink), |sink| async move {
            loop {
                if let Some(frame) = sink.pop() {
                    return Some((frame, sink));
                }
                if sink.is_closed() {
                    return None;
                }
                sink.ready.notified().await;
            }
        });
        let collected: Vec<Frame> = stream.collect().await;
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].data, "only");
    }

    #[test]
    fn the_formats_wire_names_match_the_dashboard_protocol() {
        assert_eq!(Format::Jpeg.as_str(), "jpeg");
        assert_eq!(Format::Png.as_str(), "png");
        assert_eq!(Format::default(), Format::Jpeg);
    }
}

// ---------------------------------------------------------------------------
// Live: real Chrome, a loopback page, real frames
// ---------------------------------------------------------------------------

#[cfg(test)]
mod live {
    use std::time::Duration;

    use futures::StreamExt as _;

    use super::{Format, Frame, ScreencastOpts};
    use crate::config::{BrowserConfig, FetchParams};
    use crate::live_u14::require_browser;
    use crate::session::BrowserSession;

    /// A one-page loopback server, in the shape the façade's integration tests
    /// use. No public network is ever involved in a test in this workspace.
    mod site {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        /// A running server, stopped when it is dropped.
        pub struct Site {
            port: u16,
            running: Arc<AtomicBool>,
        }

        impl Site {
            /// Bind an ephemeral loopback port and serve `body` at every path.
            pub fn start(body: &'static str) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let port = listener.local_addr().expect("addr").port();
                let running = Arc::new(AtomicBool::new(true));

                let thread_running = Arc::clone(&running);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        if !thread_running.load(Ordering::Acquire) {
                            break;
                        }
                        match stream {
                            // One thread per connection: a browser opens
                            // several at once and keeps them alive.
                            Ok(stream) => {
                                std::thread::spawn(move || serve(stream, body));
                            }
                            Err(_) => break,
                        }
                    }
                });

                Self { port, running }
            }

            /// An absolute URL for `path`.
            pub fn url(&self, path: &str) -> String {
                format!("http://127.0.0.1:{}{path}", self.port)
            }
        }

        impl Drop for Site {
            fn drop(&mut self) {
                self.running.store(false, Ordering::Release);
                let _ = TcpStream::connect(("127.0.0.1", self.port));
            }
        }

        fn serve(mut stream: TcpStream, body: &'static str) {
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

            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
            // Half-close and drain, so the peer is never RST before it read.
            let _ = stream.shutdown(Shutdown::Write);
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
        }
    }

    /// A page that never stops repainting, because Chrome only emits a
    /// screencast frame when the compositor produces one.
    const ANIMATED: &str = "<!DOCTYPE html><html><head><title>Screencast</title><style>\
        @keyframes slide { from { left: 0 } to { left: 400px } }\
        #box { position: absolute; top: 40px; width: 160px; height: 160px;\
               background: #c0392b; animation: slide 1s linear infinite alternate }\
        </style></head><body style=\"margin:0;background:#fff\">\
        <div id=\"box\"></div></body></html>";

    /// Width and height out of a JPEG's or PNG's own header.
    ///
    /// Reading the picture rather than the CDP metadata is the point: it is
    /// what proves Chrome sent real pixels of the real viewport.
    fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
            let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
            return Some((width, height));
        }
        if !bytes.starts_with(&[0xff, 0xd8]) {
            return None;
        }
        // Walk the JPEG segments to the start-of-frame, which carries the size.
        let mut at = 2usize;
        while at + 9 < bytes.len() {
            if bytes[at] != 0xff {
                at += 1;
                continue;
            }
            let marker = bytes[at + 1];
            let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
            // SOF0..SOF3 and SOF5..SOF15; DHT/DAC/RST share the 0xC4/0xC8/0xCC
            // codes and are not frame headers.
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                let height = u32::from(u16::from_be_bytes([bytes[at + 5], bytes[at + 6]]));
                let width = u32::from(u16::from_be_bytes([bytes[at + 7], bytes[at + 8]]));
                return Some((width, height));
            }
            at += 2 + length;
        }
        None
    }

    /// Collect `want` frames, or give up after `budget`.
    async fn collect(
        stream: &mut (impl futures::Stream<Item = Frame> + Unpin),
        want: usize,
        budget: Duration,
    ) -> Vec<Frame> {
        let mut frames = Vec::with_capacity(want);
        let deadline = tokio::time::Instant::now() + budget;
        while frames.len() < want {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) | Err(_) => break,
            }
        }
        frames
    }

    /// A started session on a page that is repainting.
    async fn animated_session() -> (site::Site, BrowserSession) {
        let site = site::Site::start(ANIMATED);
        let session = BrowserSession::new(BrowserConfig::new().headless(true).timeout_ms(20_000.0))
            .expect("a valid session config");
        session.start().await.expect("the browser starts");
        session
            .fetch(&site.url("/"), FetchParams::default())
            .await
            .expect("the page loads");
        (site, session)
    }

    #[tokio::test]
    async fn frames_keep_arriving_which_is_only_possible_if_every_one_was_acked() {
        require_browser!();
        let (_site, session) = animated_session().await;
        let page = session
            .pooled_pages()
            .into_iter()
            .next()
            .expect("the tab stayed in the pool");

        let mut stream = page
            .start_screencast(ScreencastOpts::default())
            .await
            .expect("the screencast starts");
        // Chrome keeps one frame in flight. Without `Page.screencastFrameAck`
        // this stops at one; eight is the assertion that acking works.
        let frames = collect(&mut stream, 8, Duration::from_secs(20)).await;
        page.stop_screencast().await.expect("the screencast stops");
        drop(stream);
        session.close().await;

        assert!(
            frames.len() >= 8,
            "only {} frame(s) arrived — Chrome stopped, which is what an \
             unacked screencast looks like",
            frames.len(),
        );
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.format, Format::Jpeg);
            let bytes = frame
                .decoded()
                .unwrap_or_else(|| panic!("frame {index} is base64"));
            let (width, height) = image_size(&bytes)
                .unwrap_or_else(|| panic!("frame {index} is not a readable JPEG"));
            assert!(width > 0 && height > 0, "frame {index} has real pixels");
            // The viewport this crate emulates, in DIP, as the page sees it.
            assert_eq!(frame.metadata.device_width, 1280.0);
            assert_eq!(frame.metadata.device_height, 720.0);
            assert_eq!(frame.metadata.scroll_offset_x, 0.0);
        }
    }

    #[tokio::test]
    async fn the_size_cap_and_the_png_format_reach_chrome() {
        require_browser!();
        let (_site, session) = animated_session().await;
        let page = session
            .pooled_pages()
            .into_iter()
            .next()
            .expect("the tab stayed in the pool");

        let mut stream = page
            .start_screencast(ScreencastOpts {
                format: Format::Png,
                max_width: Some(320),
                max_height: Some(320),
                ..ScreencastOpts::default()
            })
            .await
            .expect("the screencast starts");
        let frames = collect(&mut stream, 3, Duration::from_secs(20)).await;
        page.stop_screencast().await.expect("the screencast stops");
        drop(stream);
        session.close().await;

        assert!(frames.len() >= 3, "got {} frame(s)", frames.len());
        for frame in &frames {
            assert_eq!(frame.format, Format::Png);
            let bytes = frame.decoded().expect("base64");
            assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G'], "a real PNG");
            let (width, height) = image_size(&bytes).expect("a readable PNG");
            assert!(
                width <= 320 && height <= 320,
                "maxWidth/maxHeight were ignored: {width}x{height}",
            );
            // The cap shrinks the picture, never the page.
            assert_eq!(frame.metadata.device_width, 1280.0);
        }
    }
}
