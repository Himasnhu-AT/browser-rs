//! Response capture and redirect-history reconstruction from Network events.
//!
//! Python gets both for free from Playwright:
//!
//! ```python
//! page.on("response", handler)                   # §5.2 capture
//! first_response.request.redirected_from         # §5.3 history
//! ```
//!
//! CDP has neither. `Network.responseReceived` says nothing about whether a
//! response is a *navigation*, and there is no `redirected_from` chain at all —
//! a redirect shows up only as the `redirectResponse` field of the **next**
//! `Network.requestWillBeSent` for the same `requestId`. Both therefore have to
//! be reconstructed by bookkeeping, which is what [`RequestLedger`] is.
//!
//! # Everything in this module is CDP-shaped but CDP-free
//!
//! The ledger is fed [`RequestStarted`] / [`ResponseArrived`] structs rather
//! than `chromiumoxide` event types, so the whole state machine is unit-testable
//! without a browser. [`crate::cdp::CdpPage`] owns the (trivial) conversion.
//!
//! # Navigation detection
//!
//! Playwright's Chromium backend decides it in one line
//! (`crNetworkManager.ts`):
//!
//! ```text
//! const isNavigationRequest = event.requestId === event.loaderId && event.type === 'Document';
//! ```
//!
//! That is [`RequestStarted::is_navigation`], and it is the `request.is_navigation_request()`
//! half of Python's handler condition. The `request.frame == page.main_frame`
//! half is [`RequestLedger::set_main_frame`].

use std::collections::HashMap;

use browser_core::text::compile as compile_regex;
use browser_fingerprint::status::StatusText;
use browser_http::config::HeaderDict;
use browser_http::redirect_safe::RedirectHop;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::error::BrowserError;
use crate::route::playwright_resource_type;

/// The status Python falls back to for a redirect hop whose response object is
/// missing: `current_response.status if current_response else 301`.
pub const REDIRECT_STATUS_FALLBACK: u16 = 301;

/// The two Playwright resource types `capture_xhr` looks at.
pub const XHR_RESOURCE_TYPES: [&str; 2] = ["xhr", "fetch"];

/// The Playwright resource type a navigation response must have.
pub const DOCUMENT_RESOURCE_TYPE: &str = "document";

/// Turn a CDP `Network.Headers` object into Playwright's `all_headers()` shape.
///
/// Playwright lower-cases header names and joins repeated values with `\n`;
/// Chrome has already done the joining by the time the event reaches us, so
/// only the lower-casing is left. Non-string values (Chrome never sends any)
/// are rendered with `Display` rather than dropped.
///
/// ```
/// use browser_cdp::cdp::history::headers_from_json;
///
/// let headers = headers_from_json(&serde_json::json!({
///     "Content-Type": "text/html; charset=utf-8",
///     "Set-Cookie": "a=1\nb=2",
/// }));
/// assert_eq!(headers["content-type"], "text/html; charset=utf-8");
/// assert_eq!(headers["set-cookie"], "a=1\nb=2");
/// ```
#[must_use]
pub fn headers_from_json(value: &serde_json::Value) -> HeaderDict {
    let mut out = IndexMap::new();
    if let Some(map) = value.as_object() {
        for (name, value) in map {
            let rendered = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            out.insert(name.to_lowercase(), rendered);
        }
    }
    out
}

/// One `Network.requestWillBeSent`, reduced to what the ledger needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestStarted {
    /// `requestId`.
    pub request_id: String,
    /// `loaderId`. Equal to `request_id` exactly for navigation requests.
    pub loader_id: String,
    /// `request.url`.
    pub url: String,
    /// `request.headers`, already lower-cased by [`headers_from_json`].
    pub headers: HeaderDict,
    /// `type`, in CDP spelling (`Document`, `XHR`, `Stylesheet`, …).
    pub cdp_resource_type: String,
    /// `frameId`, absent for worker-issued requests.
    pub frame_id: Option<String>,
    /// `redirectResponse`, present on every hop after the first.
    pub redirect_response: Option<RedirectResponse>,
    /// `redirectHasExtraInfo` — whether the hop in `redirect_response` will get
    /// its own `Network.responseReceivedExtraInfo` with the unredacted headers.
    pub redirect_has_extra_info: bool,
}

impl RequestStarted {
    /// Playwright's `request.is_navigation_request()`.
    #[must_use]
    pub fn is_navigation(&self) -> bool {
        self.request_id == self.loader_id && self.cdp_resource_type == "Document"
    }

    /// Playwright's `request.resource_type`.
    #[must_use]
    pub fn resource_type(&self) -> &'static str {
        playwright_resource_type(&self.cdp_resource_type)
    }
}

/// The `redirectResponse` half of a `Network.requestWillBeSent`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedirectResponse {
    /// The URL that answered with the redirect.
    pub url: String,
    /// Its status. `0` means Chrome had none, and becomes
    /// [`REDIRECT_STATUS_FALLBACK`].
    pub status: u16,
    /// Its reason phrase, often empty.
    pub status_text: String,
    /// Its response headers.
    pub headers: HeaderDict,
}

/// One `Network.responseReceived`, reduced to what the ledger needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseArrived {
    /// `requestId`.
    pub request_id: String,
    /// `response.url`.
    pub url: String,
    /// `response.status`.
    pub status: u16,
    /// `response.statusText`, frequently `""` — "PlayWright API sometimes give
    /// empty status text for some reason!"
    pub status_text: String,
    /// `response.headers`.
    pub headers: HeaderDict,
    /// `type`, in CDP spelling.
    pub cdp_resource_type: String,
    /// `frameId`.
    pub frame_id: Option<String>,
    /// `hasExtraInfo` — whether a `Network.responseReceivedExtraInfo` carrying
    /// this response's *unredacted* headers is on its way.
    ///
    /// Playwright branches on exactly this: `processResponse(..., hasExtraInfo)`
    /// resolves the raw-headers promise from the redacted headers when it is
    /// `false`, and otherwise leaves it pending until the extra info lands. See
    /// [`RequestLedger::on_response_extra_info`].
    pub has_extra_info: bool,
}

/// A captured response, in the shape [`crate::response`] consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseRecord {
    /// The CDP `requestId`, needed to ask for the body later.
    pub request_id: String,
    /// The response URL.
    pub url: String,
    /// The status code.
    pub status: u16,
    /// `status_text or StatusText.get(status)`, already resolved.
    pub reason: String,
    /// The response headers, lower-cased.
    pub headers: HeaderDict,
    /// The headers of the request that produced it.
    pub request_headers: HeaderDict,
    /// The Playwright resource type.
    pub resource_type: &'static str,
    /// Whether [`ResponseRecord::headers`] are the *redacted* ones and a
    /// `Network.responseReceivedExtraInfo` carrying the real ones is still
    /// owed. Always `false` by the time a [`Capture`] leaves the ledger.
    pub awaiting_raw_headers: bool,
}

impl ResponseRecord {
    /// The `content-type` header, or `""`.
    #[must_use]
    pub fn content_type(&self) -> &str {
        self.headers
            .get("content-type")
            .map_or("", std::string::String::as_str)
    }

    /// Whether the factory should serialise the live DOM instead of using the
    /// transport body: `"html" in final_response.all_headers()["content-type"]`.
    #[must_use]
    pub fn is_html(&self) -> bool {
        self.content_type().contains("html")
    }
}

/// One hop of a redirect chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectRecord {
    /// The URL that answered with the redirect.
    pub url: String,
    /// Its status, with [`REDIRECT_STATUS_FALLBACK`] already applied.
    pub status: u16,
    /// Its reason phrase.
    pub reason: String,
    /// Its response headers.
    pub headers: HeaderDict,
    /// Where it pointed, i.e. the URL of the following request.
    pub location: String,
}

impl From<RedirectRecord> for RedirectHop {
    fn from(record: RedirectRecord) -> Self {
        Self {
            url: record.url,
            status: record.status,
            reason: record.reason,
            headers: record.headers,
            location: record.location,
        }
    }
}

/// `re.search(xhr_pattern, url)`.
///
/// The compiled pattern lives in `browser-core`'s process-wide cache, so this
/// is a hash lookup after the first call. A pattern that fails to compile — or
/// a `fancy-regex` backtrack limit — reports "no match" rather than killing the
/// event pump; [`RequestLedger::begin_capture`] has already rejected patterns
/// that cannot compile at all.
#[must_use]
pub fn pattern_matches(pattern: &str, url: &str) -> bool {
    compile_regex(pattern, true).is_ok_and(|regex| regex.is_match(url).unwrap_or(false))
}

/// `status_text or StatusText.get(status)`.
#[must_use]
pub fn reason_for(status: u16, status_text: &str) -> String {
    if status_text.is_empty() {
        StatusText::get(status).to_owned()
    } else {
        status_text.to_owned()
    }
}

/// What one `fetch()` captured: Python's `first_response`, `final_response[0]`
/// and `xhr_captured`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// The first main-frame document response of the window — what
    /// `page.goto()` returns.
    pub first: Option<ResponseRecord>,
    /// The last main-frame document response of the window — `final_response[0]`.
    pub last: Option<ResponseRecord>,
    /// Responses whose URL matched `capture_xhr`, in arrival order.
    pub xhr: Vec<ResponseRecord>,
    /// The redirect chain of [`Capture::first`], oldest hop first.
    pub history: Vec<RedirectRecord>,
}

impl Capture {
    /// `final_response = final_response if final_response else first_response`.
    #[must_use]
    pub fn effective_final(&self) -> Option<&ResponseRecord> {
        self.last.as_ref().or(self.first.as_ref())
    }

    /// Whether the navigation produced any response at all.
    ///
    /// `if not first_response: raise RuntimeError(f"Failed to get response for {url}")`.
    #[must_use]
    pub const fn has_navigation_response(&self) -> bool {
        self.first.is_some()
    }
}

#[derive(Debug, Default)]
struct RequestRecord {
    headers: HeaderDict,
    is_navigation: bool,
    frame_id: Option<String>,
}

#[derive(Debug, Default)]
struct CaptureState {
    /// `(request_id, hop)` pairs whose raw headers have not arrived yet.
    awaiting_raw: Vec<(String, usize)>,
    pattern: Option<String>,
    first: Option<ResponseRecord>,
    last: Option<ResponseRecord>,
    xhr: Vec<ResponseRecord>,
    navigation_request_id: Option<String>,
}

#[derive(Debug, Default)]
struct LedgerState {
    main_frame: Option<String>,
    requests: HashMap<String, RequestRecord>,
    redirects: HashMap<String, Vec<RedirectRecord>>,
    capture: Option<CaptureState>,
    /// `Network.requestWillBeSentExtraInfo` headers, one entry per hop, in
    /// arrival order. See [`RequestLedger::on_request_extra_info`].
    request_extra: HashMap<String, Vec<HeaderDict>>,
    /// `Network.responseReceivedExtraInfo` headers, one entry per hop.
    response_extra: HashMap<String, Vec<HeaderDict>>,
}

/// Per-tab bookkeeping over `Network.requestWillBeSent` /
/// `Network.responseReceived`.
///
/// One instance lives for the life of a tab; [`RequestLedger::begin_capture`]
/// and [`RequestLedger::take_capture`] open and close the per-request window
/// that Python opens with `page.on("response", handler)` and closes with
/// `page.remove_listener`.
#[derive(Debug, Default)]
pub struct RequestLedger {
    state: Mutex<LedgerState>,
}

impl RequestLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the ledger which frame id is the main frame.
    ///
    /// Until this is set no response can satisfy
    /// `request.frame == page.main_frame`, so nothing is captured — which is
    /// the safe direction to fail in.
    pub fn set_main_frame(&self, frame_id: impl Into<String>) {
        self.state.lock().main_frame = Some(frame_id.into());
    }

    /// The main frame id, if it is known yet.
    #[must_use]
    pub fn main_frame(&self) -> Option<String> {
        self.state.lock().main_frame.clone()
    }

    /// Open a capture window, as `page.on("response", handler)` does.
    ///
    /// `xhr_pattern` is the session's `capture_xhr`; an unparsable pattern is
    /// reported rather than silently ignored, because Python would have raised
    /// from `re.search` on the first response.
    ///
    /// # Errors
    ///
    /// [`BrowserError::InvalidXhrPattern`] when the regex does not compile.
    pub fn begin_capture(&self, xhr_pattern: Option<&str>) -> Result<(), BrowserError> {
        let pattern = match xhr_pattern.filter(|pattern| !pattern.is_empty()) {
            Some(pattern) => {
                // Compile eagerly so a bad pattern is a `fetch()` error rather
                // than a surprise on whichever response happens to arrive
                // first; the result is cached, so the per-response
                // `pattern_matches` below is a lookup.
                compile_regex(pattern, true).map_err(|error| BrowserError::InvalidXhrPattern {
                    pattern: pattern.to_owned(),
                    message: error.to_string(),
                })?;
                Some(pattern.to_owned())
            }
            None => None,
        };
        self.state.lock().capture = Some(CaptureState {
            pattern,
            ..CaptureState::default()
        });
        Ok(())
    }

    /// Close the capture window and take what it collected, as
    /// `page.remove_listener("response", handler)` does.
    ///
    /// Returns an empty [`Capture`] when no window was open.
    pub fn take_capture(&self) -> Capture {
        let mut state = self.state.lock();
        let Some(capture) = state.capture.take() else {
            return Capture::default();
        };
        let mut history = capture
            .navigation_request_id
            .as_ref()
            .and_then(|id| state.redirects.get(id))
            .cloned()
            .unwrap_or_default();

        // Swap the redacted headers for the raw ones, hop by hop. See
        // `on_response_extra_info`.
        if let Some(id) = capture.navigation_request_id.as_deref() {
            if let Some(raw) = state.response_extra.get(id) {
                for (hop, headers) in history.iter_mut().zip(raw.iter()) {
                    hop.headers.clone_from(headers);
                }
            }
        }
        let mut first = capture.first;
        let mut last = capture.last;
        let mut xhr = capture.xhr;
        for record in first
            .iter_mut()
            .chain(last.iter_mut())
            .chain(xhr.iter_mut())
        {
            // The record's own hop is the one after every redirect it followed.
            let hop = state.redirects.get(&record.request_id).map_or(0, Vec::len);
            if let Some(headers) = state
                .response_extra
                .get(&record.request_id)
                .and_then(|raw| raw.get(hop))
            {
                record.headers.clone_from(headers);
            }
            if let Some(headers) = state
                .request_extra
                .get(&record.request_id)
                .and_then(|raw| raw.get(hop))
            {
                record.request_headers.clone_from(headers);
            }
        }

        Capture {
            first,
            last,
            xhr,
            history,
        }
    }

    /// Drop everything a previous request left behind.
    ///
    /// A pooled tab outlives the request that opened it, so its ledger has to
    /// be emptied between requests or the redirect chain of request *n* would
    /// leak into request *n + 1*.
    pub fn reset(&self) {
        let mut state = self.state.lock();
        state.requests.clear();
        state.redirects.clear();
        state.request_extra.clear();
        state.response_extra.clear();
        state.capture = None;
    }

    /// Feed one `Network.requestWillBeSentExtraInfo`.
    ///
    /// See [`RequestLedger::on_response_extra_info`] for why these exist.
    pub fn on_request_extra_info(&self, request_id: &str, headers: HeaderDict) {
        self.state
            .lock()
            .request_extra
            .entry(request_id.to_owned())
            .or_default()
            .push(headers);
    }

    /// Feed one `Network.responseReceivedExtraInfo`.
    ///
    /// # Why this event exists at all
    ///
    /// `Network.responseReceived` reports the headers Chrome's *renderer* got,
    /// and the browser strips the ones the renderer is not allowed to see —
    /// `Set-Cookie` above all. The unredacted, on-the-wire headers arrive on
    /// `Network.responseReceivedExtraInfo` instead.
    ///
    /// Playwright's `Response.allHeaders()` — which is what
    /// `convertor.py` reads for `Response.headers`, and
    /// `Request.allHeaders()` for `Response.request_headers` — resolves to
    /// these raw headers and falls back to the redacted ones only when no
    /// extra info was emitted (`setRawResponseHeaders(headers || this._headers)`).
    /// Without them a browser `Response` silently loses every `Set-Cookie`.
    ///
    /// # Pairing
    ///
    /// Chrome emits one of these per *hop*, in order, all under the request's
    /// one `requestId` — the same order in which the hops appear in the
    /// redirect chain, with the final response last. They are therefore paired
    /// by position in [`RequestLedger::take_capture`], which is also how
    /// Playwright's `ResponseExtraInfoTracker` pairs them
    /// (`responses[index]` ↔ `responseReceivedExtraInfo[index]`).
    pub fn on_response_extra_info(&self, request_id: &str, headers: HeaderDict) {
        self.state
            .lock()
            .response_extra
            .entry(request_id.to_owned())
            .or_default()
            .push(headers);
    }

    /// Feed one `Network.requestWillBeSent`.
    pub fn on_request_started(&self, event: &RequestStarted) {
        let mut state = self.state.lock();
        if let Some(redirect) = event.redirect_response.as_ref() {
            let status = if redirect.status == 0 {
                REDIRECT_STATUS_FALLBACK
            } else {
                redirect.status
            };
            let hop = RedirectRecord {
                url: redirect.url.clone(),
                status,
                reason: reason_for(status, &redirect.status_text),
                headers: redirect.headers.clone(),
                location: event.url.clone(),
            };
            // Oldest first, exactly like Python's `history.insert(0, …)` walk
            // of `redirected_from` — the walk is backwards, ours is forwards.
            let chain = state.redirects.entry(event.request_id.clone()).or_default();
            let index = chain.len();
            chain.push(hop);
            if event.redirect_has_extra_info {
                if let Some(capture) = state.capture.as_mut() {
                    capture.awaiting_raw.push((event.request_id.clone(), index));
                }
            }
        }
        state.requests.insert(
            event.request_id.clone(),
            RequestRecord {
                headers: event.headers.clone(),
                is_navigation: event.is_navigation(),
                frame_id: event.frame_id.clone(),
            },
        );
    }

    /// Feed one `Network.responseReceived`.
    ///
    /// Mirrors `_create_response_handler`: the document branch and the XHR
    /// branch are `if`/`elif`, so a navigation response is never also an XHR
    /// capture.
    pub fn on_response_received(&self, event: &ResponseArrived) {
        let mut state = self.state.lock();
        let main_frame = state.main_frame.clone();
        let record_meta = state.requests.get(&event.request_id).map(|record| {
            (
                record.headers.clone(),
                record.is_navigation,
                record.frame_id.clone(),
            )
        });
        // The hop this response belongs to: one past every redirect the
        // request already followed. Computed before `capture` is borrowed.
        let hop = state.redirects.get(&event.request_id).map_or(0, Vec::len);
        let Some(capture) = state.capture.as_mut() else {
            return;
        };
        let (request_headers, is_navigation, request_frame) = record_meta.unwrap_or_default();
        let resource_type = playwright_resource_type(&event.cdp_resource_type);
        let record = ResponseRecord {
            request_id: event.request_id.clone(),
            url: event.url.clone(),
            status: event.status,
            reason: reason_for(event.status, &event.status_text),
            headers: event.headers.clone(),
            request_headers,
            resource_type,
            awaiting_raw_headers: event.has_extra_info,
        };

        // Python compares `request.frame` with `page.main_frame`; CDP puts the
        // frame on both events, but `Network.responseReceived` omits it for a
        // handful of resource kinds, so the request's own frame is the
        // fallback.
        let frame = event.frame_id.as_deref().or(request_frame.as_deref());
        let is_main_frame = match (frame, main_frame.as_deref()) {
            (Some(frame), Some(main)) => frame == main,
            _ => false,
        };
        if resource_type == DOCUMENT_RESOURCE_TYPE && is_navigation && is_main_frame {
            if capture.first.is_none() {
                capture.first = Some(record.clone());
                capture.navigation_request_id = Some(event.request_id.clone());
            }
            capture.last = Some(record);
            if event.has_extra_info {
                capture.awaiting_raw.push((event.request_id.clone(), hop));
            }
            return;
        }

        if XHR_RESOURCE_TYPES.contains(&resource_type)
            && capture
                .pattern
                .as_deref()
                .is_some_and(|pattern| pattern_matches(pattern, &event.url))
        {
            capture.xhr.push(record);
            if event.has_extra_info {
                capture.awaiting_raw.push((event.request_id.clone(), hop));
            }
        }
    }

    /// Whether any captured response is still owed its unredacted headers.
    ///
    /// Playwright's `Response.allHeaders()` is a promise that stays pending
    /// until `Network.responseReceivedExtraInfo` resolves it, so `convertor.py`
    /// — which awaits it — never sees the redacted headers for a response whose
    /// `hasExtraInfo` was `true`. This port builds the `Response` eagerly, so
    /// the same guarantee has to be had by waiting: see
    /// [`crate::cdp::CdpPage::await_raw_headers`].
    #[must_use]
    pub fn raw_headers_pending(&self) -> bool {
        let state = self.state.lock();
        let Some(capture) = state.capture.as_ref() else {
            return false;
        };
        capture.awaiting_raw.iter().any(|(id, hop)| {
            state
                .response_extra
                .get(id)
                .is_none_or(|raw| raw.len() <= *hop)
        })
    }

    /// Whether the open capture window has already seen the main-frame
    /// document response.
    ///
    /// This is `if not first_response:` — Python asks the return value of
    /// `page.goto()`, which is the same thing: the navigation's response.
    #[must_use]
    pub fn captured_navigation(&self) -> bool {
        self.state
            .lock()
            .capture
            .as_ref()
            .is_some_and(|capture| capture.first.is_some())
    }

    /// The redirect chain recorded for one request id, oldest hop first.
    #[must_use]
    pub fn redirect_chain(&self, request_id: &str) -> Vec<RedirectRecord> {
        self.state
            .lock()
            .redirects
            .get(request_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn ledger() -> RequestLedger {
        let ledger = RequestLedger::new();
        ledger.set_main_frame("MAIN");
        ledger.begin_capture(None).expect("no pattern");
        ledger
    }

    fn navigation(request_id: &str, url: &str) -> RequestStarted {
        RequestStarted {
            redirect_has_extra_info: false,
            request_id: request_id.to_owned(),
            loader_id: request_id.to_owned(),
            url: url.to_owned(),
            headers: headers(&[("user-agent", "Chrome")]),
            cdp_resource_type: "Document".to_owned(),
            frame_id: Some("MAIN".to_owned()),
            redirect_response: None,
        }
    }

    fn document_response(request_id: &str, url: &str, status: u16) -> ResponseArrived {
        ResponseArrived {
            has_extra_info: false,
            request_id: request_id.to_owned(),
            url: url.to_owned(),
            status,
            status_text: String::new(),
            headers: headers(&[("content-type", "text/html; charset=utf-8")]),
            cdp_resource_type: "Document".to_owned(),
            frame_id: Some("MAIN".to_owned()),
        }
    }

    #[test]
    fn navigation_detection_matches_playwrights_one_liner() {
        assert!(navigation("A", "https://h/").is_navigation());
        let subresource = RequestStarted {
            loader_id: "B".to_owned(),
            ..navigation("A", "https://h/x.js")
        };
        assert!(!subresource.is_navigation());
        let iframe_document = RequestStarted {
            cdp_resource_type: "Stylesheet".to_owned(),
            ..navigation("A", "https://h/x.css")
        };
        assert!(!iframe_document.is_navigation());
    }

    #[test]
    fn the_last_main_frame_document_response_wins() {
        let ledger = ledger();
        ledger.on_request_started(&navigation("A", "https://h/one"));
        ledger.on_response_received(&document_response("A", "https://h/one", 200));
        ledger.on_request_started(&navigation("B", "https://h/two"));
        ledger.on_response_received(&document_response("B", "https://h/two", 201));

        let capture = ledger.take_capture();
        assert_eq!(capture.first.as_ref().expect("first").url, "https://h/one");
        assert_eq!(capture.last.as_ref().expect("last").url, "https://h/two");
        assert_eq!(capture.effective_final().expect("final").status, 201);
        assert!(capture.has_navigation_response());
    }

    #[test]
    fn a_missing_final_response_falls_back_to_the_first() {
        let mut capture = Capture::default();
        assert!(capture.effective_final().is_none());
        assert!(!capture.has_navigation_response());
        capture.first = Some(ResponseRecord {
            awaiting_raw_headers: false,
            request_id: "A".into(),
            url: "https://h/".into(),
            status: 200,
            reason: "OK".into(),
            headers: HeaderDict::new(),
            request_headers: HeaderDict::new(),
            resource_type: "document",
        });
        assert_eq!(capture.effective_final().expect("final").status, 200);
    }

    #[test]
    fn subframe_documents_are_not_captured() {
        let ledger = ledger();
        let subframe = RequestStarted {
            frame_id: Some("IFRAME".to_owned()),
            ..navigation("A", "https://ads.test/frame")
        };
        ledger.on_request_started(&subframe);
        ledger.on_response_received(&ResponseArrived {
            frame_id: Some("IFRAME".to_owned()),
            ..document_response("A", "https://ads.test/frame", 200)
        });
        assert!(ledger.take_capture().first.is_none());
    }

    #[test]
    fn a_redirect_chain_is_rebuilt_oldest_first() {
        let ledger = ledger();
        ledger.on_request_started(&navigation("A", "http://h/one"));
        ledger.on_request_started(&RequestStarted {
            url: "http://h/two".to_owned(),
            redirect_response: Some(RedirectResponse {
                url: "http://h/one".to_owned(),
                status: 301,
                status_text: String::new(),
                headers: headers(&[("location", "http://h/two")]),
            }),
            ..navigation("A", "http://h/two")
        });
        ledger.on_request_started(&RequestStarted {
            url: "http://h/three".to_owned(),
            redirect_response: Some(RedirectResponse {
                url: "http://h/two".to_owned(),
                status: 302,
                status_text: "Found".to_owned(),
                headers: headers(&[("location", "http://h/three")]),
            }),
            ..navigation("A", "http://h/three")
        });
        ledger.on_response_received(&document_response("A", "http://h/three", 200));

        let capture = ledger.take_capture();
        let urls: Vec<&str> = capture.history.iter().map(|hop| hop.url.as_str()).collect();
        assert_eq!(urls, ["http://h/one", "http://h/two"]);
        assert_eq!(capture.history[0].status, 301);
        assert_eq!(
            capture.history[0].reason, "Moved Permanently",
            "an empty statusText falls back to the status table"
        );
        assert_eq!(capture.history[0].location, "http://h/two");
        assert_eq!(capture.history[1].reason, "Found");
        // A redirect hop keeps no body: `content=""` in Python.
        let hop: RedirectHop = capture.history[0].clone().into();
        assert_eq!(hop.location, "http://h/two");
        assert_eq!(hop.status, 301);
    }

    #[test]
    fn a_zero_status_redirect_falls_back_to_301() {
        let ledger = ledger();
        ledger.on_request_started(&RequestStarted {
            redirect_response: Some(RedirectResponse {
                url: "http://h/one".to_owned(),
                status: 0,
                status_text: String::new(),
                headers: HeaderDict::new(),
            }),
            ..navigation("A", "http://h/two")
        });
        let chain = ledger.redirect_chain("A");
        assert_eq!(chain[0].status, REDIRECT_STATUS_FALLBACK);
        assert_eq!(chain[0].reason, "Moved Permanently");
    }

    #[test]
    fn xhr_capture_honours_the_pattern_and_the_resource_type() {
        let ledger = RequestLedger::new();
        ledger.set_main_frame("MAIN");
        ledger.begin_capture(Some("/api/")).expect("valid pattern");

        for (id, url, cdp_type) in [
            ("1", "https://h/api/items", "XHR"),
            ("2", "https://h/api/prices", "Fetch"),
            ("3", "https://h/static/app.js", "Script"),
            ("4", "https://h/other/thing", "XHR"),
        ] {
            ledger.on_request_started(&RequestStarted {
                request_id: id.to_owned(),
                loader_id: "nav".to_owned(),
                url: url.to_owned(),
                cdp_resource_type: cdp_type.to_owned(),
                frame_id: Some("MAIN".to_owned()),
                ..RequestStarted::default()
            });
            ledger.on_response_received(&ResponseArrived {
                request_id: id.to_owned(),
                url: url.to_owned(),
                status: 200,
                cdp_resource_type: cdp_type.to_owned(),
                frame_id: Some("MAIN".to_owned()),
                ..ResponseArrived::default()
            });
        }

        let captured: Vec<String> = ledger
            .take_capture()
            .xhr
            .into_iter()
            .map(|record| record.url)
            .collect();
        assert_eq!(captured, ["https://h/api/items", "https://h/api/prices"]);
    }

    #[test]
    fn without_a_pattern_nothing_is_captured() {
        let ledger = ledger();
        ledger.on_request_started(&RequestStarted {
            request_id: "1".to_owned(),
            loader_id: "nav".to_owned(),
            url: "https://h/api/items".to_owned(),
            cdp_resource_type: "XHR".to_owned(),
            frame_id: Some("MAIN".to_owned()),
            ..RequestStarted::default()
        });
        ledger.on_response_received(&ResponseArrived {
            request_id: "1".to_owned(),
            url: "https://h/api/items".to_owned(),
            status: 200,
            cdp_resource_type: "XHR".to_owned(),
            frame_id: Some("MAIN".to_owned()),
            ..ResponseArrived::default()
        });
        assert!(ledger.take_capture().xhr.is_empty());
    }

    #[test]
    fn a_navigation_response_never_lands_in_the_xhr_list() {
        // Python's handler is `if … elif …`, and a `capture_xhr` of `.*`
        // would otherwise swallow the document response too.
        let ledger = RequestLedger::new();
        ledger.set_main_frame("MAIN");
        ledger.begin_capture(Some(".*")).expect("valid pattern");
        ledger.on_request_started(&navigation("A", "https://h/"));
        ledger.on_response_received(&document_response("A", "https://h/", 200));
        let capture = ledger.take_capture();
        assert!(capture.first.is_some());
        assert!(capture.xhr.is_empty());
    }

    #[test]
    fn an_invalid_pattern_is_reported_not_swallowed() {
        let ledger = RequestLedger::new();
        let error = ledger.begin_capture(Some("(unbalanced")).unwrap_err();
        assert!(matches!(error, BrowserError::InvalidXhrPattern { .. }));
    }

    #[test]
    fn events_outside_a_capture_window_are_ignored() {
        let ledger = RequestLedger::new();
        ledger.set_main_frame("MAIN");
        ledger.on_request_started(&navigation("A", "https://h/"));
        ledger.on_response_received(&document_response("A", "https://h/", 200));
        let capture = ledger.take_capture();
        assert!(capture.first.is_none());
        assert!(capture.last.is_none());
    }

    #[test]
    fn reset_drops_a_previous_requests_redirect_chain() {
        let ledger = ledger();
        ledger.on_request_started(&RequestStarted {
            redirect_response: Some(RedirectResponse {
                url: "http://h/one".to_owned(),
                status: 301,
                status_text: String::new(),
                headers: HeaderDict::new(),
            }),
            ..navigation("A", "http://h/two")
        });
        assert_eq!(ledger.redirect_chain("A").len(), 1);
        ledger.reset();
        assert!(ledger.redirect_chain("A").is_empty());
        assert_eq!(ledger.take_capture(), Capture::default());
    }

    #[test]
    fn request_headers_come_from_the_request_not_the_response() {
        let ledger = ledger();
        ledger.on_request_started(&navigation("A", "https://h/"));
        ledger.on_response_received(&document_response("A", "https://h/", 200));
        let capture = ledger.take_capture();
        let first = capture.first.expect("first");
        assert_eq!(first.request_headers["user-agent"], "Chrome");
        assert!(first.is_html());
        assert_eq!(first.content_type(), "text/html; charset=utf-8");
        assert_eq!(first.reason, "OK");
    }

    #[test]
    fn headers_are_lower_cased_like_playwrights_all_headers() {
        let parsed = headers_from_json(&serde_json::json!({
            "CONTENT-TYPE": "text/plain",
            "X-Count": 3,
        }));
        assert_eq!(parsed["content-type"], "text/plain");
        assert_eq!(parsed["x-count"], "3");
        assert!(headers_from_json(&serde_json::Value::Null).is_empty());
    }
}
