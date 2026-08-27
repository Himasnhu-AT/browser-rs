//! The engine: every operation the REST layer and the MCP layer share.
//!
//! `AGENTS.md` states the rule this module exists to satisfy: **every operation
//! lives here, never in a route handler**, because `/api` and `/mcp` must run
//! exactly the same code. A handler in [`crate::api`] deserialises a body,
//! calls one method here and serialises what comes back; if it does anything
//! else, the two surfaces have started to drift.
//!
//! # The six operations
//!
//! | Method | Transport | What it is for |
//! |---|---|---|
//! | [`Engine::fetch`] | HTTP only | fast, cheap, no browser |
//! | [`Engine::browse`] | real Chrome | JS-rendered or logged-in pages |
//! | [`Engine::extract`] | either | CSS / XPath / markdown / cleaned text |
//! | [`Engine::crawl`] | HTTP | bounded, same-host, robots-respecting |
//! | [`Engine::screenshot`] | real Chrome | PNG bytes |
//! | [`Engine::session_open`] / `session_list` / `session_close` | — | identity |
//!
//! # Egress and fingerprint
//!
//! Neither is implemented here. `browser-http` already does TLS/JA3-JA4
//! impersonation through `wreq`, `browser-core` already has the [`Proxy`] type
//! that both fetchers take, and `browser-cdp` already turns a proxy into a
//! `--proxy-server=` flag. This module only decides **which** values apply, in
//! one order that never varies: *the request, else the session, else the server
//! default, else the rotator `browser-http` is holding.* That chain is
//! [`crate::session::Egress::or_defaults`], and it is written once.
//!
//! # Sessions
//!
//! Passing `session: Some(name)` routes the operation through a named,
//! persistent session — see [`crate::session`]. Passing `None` makes it
//! stateless: a one-off request with no jar and, for [`Engine::browse`], a
//! throw-away profile that is deleted when the page closes.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use browser::http::config::HeaderDict;
use browser::{
    AsyncFetcher, BrowserSession, FetchParams, Impersonate, Override, PageLoadState, Proxy,
    RequestBuilder, Response, WaitState,
};
use browser_cdp::cdp::CdpPage;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use url::Url;

use crate::config::ServerConfig;
use crate::error::{Result, ServerError};
use crate::session::{
    page_slot_hooks, redact_proxy, Egress, PageSlot, Session, SessionInfo, SessionRegistry,
};

/// How many activity events a slow subscriber may fall behind by.
///
/// `API.md` §5 is explicit that the dashboard must **drop** rather than queue,
/// and a `broadcast` channel does exactly that: a receiver that falls this far
/// behind gets a `Lagged` error and skips forward instead of the sender
/// growing without bound.
pub const ACTIVITY_BUFFER: usize = 256;

// ---------------------------------------------------------------------------
// The vocabulary
// ---------------------------------------------------------------------------

/// How an operation was carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// The HTTP fetcher: no browser, no JavaScript.
    Http,
    /// A real Chrome over CDP.
    Browser,
}

/// The shape a page is returned in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentFormat {
    /// The page's HTML, as served (or as the DOM stands, after `browse`).
    #[default]
    Html,
    /// Markdown, noise tags stripped. What an LLM should usually be given.
    Markdown,
    /// Visible text, after the same AI-sanitisation pass markdown uses.
    Text,
}

/// What [`Engine::extract`] was asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractKind {
    /// A CSS selector. `::text` and `::attr(name)` pseudo-elements are
    /// supported, so `h1::text` and `a::attr(href)` do what they look like.
    #[default]
    Css,
    /// An XPath 1.0 expression.
    Xpath,
    /// The whole page as markdown; `selector` optionally narrows it first.
    Markdown,
    /// The whole page as AI-cleaned visible text.
    Text,
}

/// The options every operation shares, as `API.md` §1 names them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenOpts {
    /// A named, persistent session. `None` is stateless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The egress proxy; else the session's, else the server default, else the
    /// rotator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// The impersonation profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,
    /// The User-Agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Whether a launched browser is headless. Ignored by [`Engine::fetch`].
    #[serde(default = "yes")]
    pub headless: bool,
}

const fn yes() -> bool {
    true
}

impl OpenOpts {
    /// The per-request egress this carries.
    #[must_use]
    pub fn egress(&self) -> Egress {
        Egress {
            proxy: self.proxy.clone(),
            impersonate: self.impersonate.clone(),
            user_agent: self.user_agent.clone(),
        }
    }
}

/// One thing the engine did, for the dashboard's activity log.
///
/// The shape is `API.md` §3b's, verbatim: `ws.rs` gets `{"t":"action",…}` from
/// here and nowhere else, so this struct is a contract and not an
/// implementation detail. Published on a bounded [`broadcast`] channel; see
/// [`ACTIVITY_BUFFER`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// When it happened, as an RFC 3339 UTC timestamp.
    pub at: String,
    /// The operation: `fetch`, `browse`, `extract`, `crawl`, `screenshot`,
    /// `session_open`, `session_close`.
    pub kind: String,
    /// The session it ran on, if any.
    pub session: Option<String>,
    /// A one-line human description — the URL, the selector, the page count.
    pub detail: String,
}

/// `1970-01-01T00:00:00Z`, from milliseconds since the epoch.
///
/// Hand-rolled because `chrono` is not a dependency of this crate and one
/// timestamp is not worth adding it — `AGENTS.md` says to say so rather than
/// edit `Cargo.toml`. The civil-from-days conversion is the standard one
/// (Howard Hinnant's `civil_from_days`), which is exact for every date in the
/// proleptic Gregorian calendar and has no leap-second notion, exactly like
/// Unix time itself.
#[must_use]
pub fn rfc3339(unix_millis: u64) -> String {
    let seconds = unix_millis / 1_000;
    let millis = unix_millis % 1_000;
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;

    // Shift the epoch to 0000-03-01 so a leap day is always the last day of the
    // "year", which is what removes every special case from the arithmetic.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60,
    )
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// The field names a request body may carry.
///
/// # Why this exists rather than `deny_unknown_fields`
///
/// Every request below `#[serde(flatten)]`s an [`OpenOpts`], and serde does not
/// support `deny_unknown_fields` alongside `flatten` — the flattened struct has
/// to be allowed to see keys the outer one did not claim, so "unknown" is not a
/// thing serde can decide on its own. Without this, `{"url": "…", "sesion":
/// "shop"}` deserialises happily into a **stateless** request: right status,
/// plausible body, no session, no cookies, no login. That is the worst shape a
/// bug can have, because it looks like success and only surfaces later as "why
/// am I not authenticated".
///
/// So the list is written out, and [`Self::rot_check`] keeps it honest.
pub trait RequestFields: Serialize {
    /// Every key this request accepts, in any order.
    const FIELDS: &'static [&'static str];

    /// Assert that [`Self::FIELDS`] still matches what serde emits.
    ///
    /// A hand-written list rots the moment someone adds a field. Serialising a
    /// **fully populated** value — every `Option` `Some`, so nothing is skipped
    /// — and comparing the key set catches a field added, removed or renamed on
    /// either side. Each request type has a test that calls this.
    ///
    /// # Panics
    ///
    /// When the two disagree, naming the difference.
    fn rot_check(fully_populated: &Self) {
        let value = serde_json::to_value(fully_populated).expect("a serialisable request");
        let actual: std::collections::BTreeSet<&str> = value
            .as_object()
            .expect("a request serialises to an object")
            .keys()
            .map(String::as_str)
            .collect();
        let declared: std::collections::BTreeSet<&str> = Self::FIELDS.iter().copied().collect();
        assert_eq!(
            actual,
            declared,
            "FIELDS is out of date: serde has {:?}, FIELDS has {:?}",
            actual.difference(&declared).collect::<Vec<_>>(),
            declared.difference(&actual).collect::<Vec<_>>(),
        );
    }
}

/// Reject any key the request type does not accept.
///
/// The suggestion is the point. An agent that gets `unknown field "sesion"`
/// retries with different wording; one that gets `did you mean "session"?`
/// fixes it. The threshold is a third of the field's length, so short names
/// need a close match and long ones tolerate a typo or two.
///
/// # Errors
///
/// [`ServerError::BadRequest`] naming the first offending key.
pub fn reject_unknown_fields<'a>(
    keys: impl Iterator<Item = &'a String>,
    known: &[&str],
) -> Result<()> {
    for key in keys {
        if known.contains(&key.as_str()) {
            continue;
        }
        let suggestion = known
            .iter()
            .map(|candidate| (*candidate, edit_distance(key, candidate)))
            .filter(|(candidate, distance)| *distance <= 1 + candidate.len() / 3)
            .min_by_key(|(_, distance)| *distance)
            .map(|(candidate, _)| candidate);
        return Err(ServerError::BadRequest(match suggestion {
            Some(candidate) => {
                format!("unknown field {key:?}; did you mean {candidate:?}?")
            }
            None => format!("unknown field {key:?}; expected one of {known:?}"),
        }));
    }
    Ok(())
}

/// Levenshtein distance, two rows rather than a full matrix.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (i, a) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let substitute = previous[j] + usize::from(a != *b);
            current[j + 1] = substitute.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// `POST /api/fetch`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FetchRequest {
    /// The absolute `http`/`https` URL to fetch.
    pub url: String,
    /// Session name, proxy, impersonation profile, User-Agent.
    #[serde(flatten)]
    pub opts: OpenOpts,
    /// Extra request headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderDict>,
    /// What shape to return the page in. Default `html`.
    pub format: ContentFormat,
    /// A per-request timeout, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `POST /api/browse`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowseRequest {
    /// The absolute `http`/`https` URL to navigate to.
    pub url: String,
    /// Session name, proxy, impersonation profile, User-Agent, headless.
    #[serde(flatten)]
    pub opts: OpenOpts,
    /// How long to wait before snapshotting the DOM.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_until: Option<PageLoadState>,
    /// A selector that must appear before the DOM is snapshotted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_for_selector: Option<String>,
    /// The state [`BrowseRequest::wait_for_selector`] must reach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_for_state: Option<WaitState>,
    /// The navigation timeout, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// What shape to return the page in. Default `html`.
    pub format: ContentFormat,
}

/// `POST /api/extract`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractRequest {
    /// Fetch this URL first. Mutually exclusive with
    /// [`ExtractRequest::html`]; one of the two is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extract from this markup instead of fetching anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Session name, proxy, impersonation profile, User-Agent.
    #[serde(flatten)]
    pub opts: OpenOpts,
    /// The CSS selector or XPath expression. Required for `css` and `xpath`,
    /// optional for `markdown` (it narrows the conversion) and ignored for
    /// `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Which query language `selector` is in. Default `css`.
    pub kind: ExtractKind,
    /// Fetch with a real browser rather than the HTTP fetcher.
    ///
    /// Not in `API.md`'s route table; added because "extract from this URL"
    /// against a JS-rendered page is otherwise impossible to express without
    /// two round trips through the agent.
    pub render: bool,
}

/// `POST /api/crawl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlRequest {
    /// Where to start.
    pub seed: String,
    /// Session name, proxy, impersonation profile, User-Agent.
    #[serde(flatten)]
    pub opts: OpenOpts,
    /// The hard ceiling on pages fetched, including the seed. Clamped to
    /// [`ServerConfig::max_crawl_pages`].
    pub max_pages: usize,
    /// How many links deep to follow. `0` fetches the seed alone.
    pub max_depth: usize,
    /// Stay on the seed's host, port included. Default `true`.
    pub same_host: bool,
    /// A CSS selector run against every page; its matches ride along on each
    /// [`CrawlPage`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

impl Default for CrawlRequest {
    fn default() -> Self {
        Self {
            seed: String::new(),
            opts: OpenOpts::default(),
            max_pages: 10,
            max_depth: 2,
            same_host: true,
            selector: None,
        }
    }
}

/// `POST /api/screenshot`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotRequest {
    /// The URL to navigate to first. Omit to shoot a session's current page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Session name, proxy, impersonation profile, User-Agent, headless.
    #[serde(flatten)]
    pub opts: OpenOpts,
    /// Capture the whole scrollable page rather than the viewport.
    ///
    /// The image is at the browser's device scale factor of 2, so a default
    /// viewport is a 2560×1440 PNG and a 5000 px document is 10000 px tall.
    pub full_page: bool,
}

/// `POST /api/sessions`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRequest {
    /// The name to open or reattach to.
    pub name: String,
    /// This session's egress proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// This session's impersonation profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,
    /// This session's User-Agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

// The accepted-key lists. Each is checked against serde by a test below; the
// five `OpenOpts` names repeat in every one because that is what `flatten`
// means on the wire — they are top-level keys, not a nested object.

impl RequestFields for FetchRequest {
    const FIELDS: &'static [&'static str] = &[
        "url",
        "headers",
        "format",
        "timeout_ms", // its own
        "session",
        "proxy",
        "impersonate",
        "user_agent",
        "headless", // OpenOpts
    ];
}

impl RequestFields for BrowseRequest {
    const FIELDS: &'static [&'static str] = &[
        "url",
        "wait_until",
        "wait_for_selector",
        "wait_for_state",
        "timeout_ms",
        "format",
        "session",
        "proxy",
        "impersonate",
        "user_agent",
        "headless",
    ];
}

impl RequestFields for ExtractRequest {
    const FIELDS: &'static [&'static str] = &[
        "url",
        "html",
        "selector",
        "kind",
        "render",
        "session",
        "proxy",
        "impersonate",
        "user_agent",
        "headless",
    ];
}

impl RequestFields for CrawlRequest {
    const FIELDS: &'static [&'static str] = &[
        "seed",
        "max_pages",
        "max_depth",
        "same_host",
        "selector",
        "session",
        "proxy",
        "impersonate",
        "user_agent",
        "headless",
    ];
}

impl RequestFields for ScreenshotRequest {
    const FIELDS: &'static [&'static str] = &[
        "url",
        "full_page",
        "session",
        "proxy",
        "impersonate",
        "user_agent",
        "headless",
    ];
}

impl RequestFields for SessionRequest {
    const FIELDS: &'static [&'static str] = &["name", "proxy", "impersonate", "user_agent"];
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// What [`Engine::fetch`] and [`Engine::browse`] return.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageResult {
    /// The final URL, after redirects.
    pub url: String,
    /// The HTTP status. A 4xx or 5xx is a result, not an error — the page is
    /// still returned, exactly as the fetchers below behave.
    pub status: u16,
    /// The `<title>`, when there is one.
    pub title: Option<String>,
    /// Which shape [`PageResult::content`] is in.
    pub format: ContentFormat,
    /// The page.
    pub content: String,
    /// Whether [`PageResult::content`] was cut short.
    pub truncated: bool,
    /// The response headers.
    pub headers: HeaderDict,
    /// The `Set-Cookie` pairs this response carried.
    pub cookies: HeaderDict,
    /// The session it ran on, if any.
    pub session: Option<String>,
    /// The egress proxy, credentials redacted.
    pub proxy: Option<String>,
    /// Whether a browser was involved.
    pub transport: Transport,
    /// How long it took.
    pub elapsed_ms: u64,
}

/// What [`Engine::extract`] returns.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtractResult {
    /// Which query language ran.
    pub kind: ExtractKind,
    /// The page it ran against, when one was fetched.
    pub url: Option<String>,
    /// The selector or expression, echoed back.
    pub selector: Option<String>,
    /// One string per match. `markdown` and `text` produce exactly one.
    pub matches: Vec<String>,
    /// How many matches there were before any truncation.
    pub count: usize,
    /// Whether the matches were cut short.
    pub truncated: bool,
}

/// One page of a crawl.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawlPage {
    /// The page's URL.
    pub url: String,
    /// How many links from the seed it was.
    pub depth: usize,
    /// Its HTTP status.
    pub status: u16,
    /// Its `<title>`.
    pub title: Option<String>,
    /// [`CrawlRequest::selector`]'s matches on this page, if one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches: Option<Vec<String>>,
}

/// What [`Engine::crawl`] returns.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawlOutcome {
    /// Where it started.
    pub seed: String,
    /// The pages, in the order their callbacks ran.
    pub pages: Vec<CrawlPage>,
    /// The budget it ran under, after clamping.
    pub max_pages: usize,
    /// The depth limit it ran under.
    pub max_depth: usize,
    /// `true` when the budget, rather than the link graph, ended the crawl.
    pub stopped_at_limit: bool,
    /// How long it took.
    pub elapsed_ms: u64,
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// The shared core. One per process.
#[derive(Debug)]
pub struct Engine {
    config: Arc<ServerConfig>,
    sessions: Arc<SessionRegistry>,
    activity: broadcast::Sender<ActivityEvent>,
}

impl Engine {
    /// Build an engine over a configuration.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        let config = Arc::new(config);
        let sessions = Arc::new(SessionRegistry::new(Arc::clone(&config)));
        let (activity, _) = broadcast::channel(ACTIVITY_BUFFER);
        Self {
            config,
            sessions,
            activity,
        }
    }

    /// The configuration this engine runs under.
    #[must_use]
    pub fn config(&self) -> &Arc<ServerConfig> {
        &self.config
    }

    /// The session registry, for the dashboard and the session routes.
    #[must_use]
    pub fn sessions(&self) -> &Arc<SessionRegistry> {
        &self.sessions
    }

    /// Subscribe to the activity log.
    ///
    /// Bounded and lossy by design: a websocket client that cannot keep up is
    /// told it lagged and skips forward, rather than the server buffering for
    /// it. See [`ACTIVITY_BUFFER`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ActivityEvent> {
        self.activity.subscribe()
    }

    fn publish(&self, kind: &'static str, session: Option<&str>, detail: impl Into<String>) {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let event = ActivityEvent {
            at: rfc3339(u64::try_from(millis).unwrap_or(u64::MAX)),
            kind: kind.to_owned(),
            session: session.map(ToOwned::to_owned),
            detail: detail.into(),
        };
        // No subscribers is the normal case for a headless deployment, and it
        // is not a failure.
        let _ = self.activity.send(event);
    }

    // -- sessions ---------------------------------------------------------

    /// Open a session, or reattach to the existing one of that name.
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] for an empty name; an internal error when
    /// the profile directory cannot be made.
    pub async fn session_open(&self, request: &SessionRequest) -> Result<SessionInfo> {
        let egress = Egress {
            proxy: request.proxy.clone(),
            impersonate: request.impersonate.clone(),
            user_agent: request.user_agent.clone(),
        };
        let session = self.sessions.open(&request.name, egress).await?;
        self.publish(
            "session_open",
            Some(&session.name),
            format!("profile {}", session.profile_dir.display()),
        );
        Ok(session.info().await)
    }

    /// Every open session.
    pub async fn session_list(&self) -> Vec<SessionInfo> {
        let mut infos = Vec::new();
        for session in self.sessions.all() {
            infos.push(session.info().await);
        }
        infos
    }

    /// Close a session, keeping its profile directory.
    ///
    /// # Errors
    ///
    /// [`ServerError::NotFound`] when no session has that name.
    pub async fn session_close(&self, name: &str) -> Result<()> {
        if self.sessions.close(name).await {
            self.publish("session_close", Some(name), "closed");
            Ok(())
        } else {
            Err(ServerError::NotFound(format!("no session named {name:?}")))
        }
    }

    /// The session an operation should run on, or `None` when it is stateless.
    async fn session_for(&self, opts: &OpenOpts) -> Result<Option<Arc<Session>>> {
        match opts.session.as_deref() {
            // Naming a session that is not open opens it. An agent that has to
            // remember to create a session before using it will forget, and the
            // profile directory makes "create" and "reattach" the same act.
            Some(name) => Ok(Some(self.sessions.open(name, opts.egress()).await?)),
            None => Ok(None),
        }
    }

    // -- fetch ------------------------------------------------------------

    /// Fetch a URL over HTTP. No browser, no JavaScript.
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] for a URL that is not absolute `http`/
    /// `https`; [`ServerError::Upstream`] or [`ServerError::Timeout`] when the
    /// far end fails.
    pub async fn fetch(&self, request: &FetchRequest) -> Result<PageResult> {
        let url = validate_url(&request.url)?;
        let session = self.session_for(&request.opts).await?;
        let egress = self.resolve_egress(&request.opts, session.as_deref());
        let started = Instant::now();

        let builder = match &session {
            Some(session) => session.http().get(url.as_str()),
            None => AsyncFetcher::get(url.as_str()),
        };
        let builder = apply_egress(builder, &request.opts, session.is_none(), &egress);
        let builder = match &request.headers {
            Some(headers) => headers.iter().fold(builder, |builder, (name, value)| {
                builder.header(name, value)
            }),
            None => builder,
        };
        let builder = match request.timeout_ms {
            Some(millis) => builder.timeout(Some(Duration::from_millis(millis))),
            None => builder,
        };

        let response = builder.send().await?;
        if let Some(session) = &session {
            session.touch();
            session.observe(&response).await;
        }

        self.publish(
            "fetch",
            session.as_ref().map(|s| s.name.as_str()),
            format!("{} {}", response.status, response.url),
        );
        Ok(self.page_result(
            &response,
            request.format,
            Transport::Http,
            session.as_deref(),
            &egress,
            started,
        ))
    }

    // -- browse -----------------------------------------------------------

    /// Navigate a real Chrome to a URL and return the rendered DOM.
    ///
    /// A named session reuses that session's browser and profile, so a login
    /// performed earlier is still in effect. A stateless call launches a
    /// throw-away browser with a temporary profile and closes it again.
    ///
    /// # Errors
    ///
    /// [`ServerError::BrowserLimit`] when every browser slot is taken;
    /// [`ServerError::Unsupported`] when no Chrome is installed; whatever the
    /// navigation failed with otherwise.
    pub async fn browse(&self, request: &BrowseRequest) -> Result<PageResult> {
        let url = validate_url(&request.url)?;
        let session = self.session_for(&request.opts).await?;
        let egress = self.resolve_egress(&request.opts, session.as_deref());
        let params = fetch_params(request);
        let started = Instant::now();

        let response = match &session {
            Some(session) => {
                let browser = session.browser(self.sessions.slots()).await?;
                browser.fetch(url.as_str(), params).await?
            }
            None => {
                // A stateless browse still counts against the slot cap: it is
                // just as much a Chrome process as a session's is.
                let permit = Arc::clone(self.sessions.slots())
                    .try_acquire_owned()
                    .map_err(|_| ServerError::BrowserLimit {
                        max: self.config.max_live_browsers.max(1),
                    })?;
                // An empty path leaves `user_data_dir` empty, so `browser-cdp`
                // makes a temporary profile and deletes it on close. That is
                // what stateless means.
                let config = egress.browser_config(Path::new(""), request.opts.headless);
                let result = browser::fetch_once(config, url.as_str(), params).await;
                drop(permit);
                result?
            }
        };

        if let Some(session) = &session {
            session.touch();
            session.observe(&response).await;
        }
        self.publish(
            "browse",
            session.as_ref().map(|s| s.name.as_str()),
            format!("{} {}", response.status, response.url),
        );
        Ok(self.page_result(
            &response,
            request.format,
            Transport::Browser,
            session.as_deref(),
            &egress,
            started,
        ))
    }

    // -- extract ----------------------------------------------------------

    /// Run a CSS selector, an XPath expression, or a whole-page conversion.
    ///
    /// The source is either `html` (nothing is fetched) or `url` (fetched
    /// first, over HTTP or — with `render` — a browser).
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] when neither `url` nor `html` is given, when
    /// `css`/`xpath` is asked for without a selector, or when the selector does
    /// not parse.
    pub async fn extract(&self, request: &ExtractRequest) -> Result<ExtractResult> {
        let (response, url) = match (&request.html, &request.url) {
            (Some(html), _) => (
                Response::builder(200, "about:blank")
                    .body(html.clone())
                    .build(),
                None,
            ),
            (None, Some(url)) => {
                let response = if request.render {
                    let page = BrowseRequest {
                        url: url.clone(),
                        opts: request.opts.clone(),
                        format: ContentFormat::Html,
                        ..BrowseRequest::default()
                    };
                    // `browse` already publishes, touches and mirrors cookies;
                    // re-fetching here would double-count, so the page comes
                    // back through the same path any other caller uses.
                    let result = self.browse(&page).await?;
                    Response::builder(result.status, result.url.clone())
                        .body(result.content)
                        .build()
                } else {
                    let page = FetchRequest {
                        url: url.clone(),
                        opts: request.opts.clone(),
                        format: ContentFormat::Html,
                        ..FetchRequest::default()
                    };
                    let result = self.fetch(&page).await?;
                    Response::builder(result.status, result.url.clone())
                        .body(result.content)
                        .build()
                };
                let final_url = response.url.clone();
                (response, Some(final_url))
            }
            (None, None) => {
                return Err(ServerError::BadRequest(
                    "extract needs either `url` or `html`".to_owned(),
                ))
            }
        };

        let selector = request.selector.as_deref();
        let matches = match request.kind {
            ExtractKind::Css => {
                let selector = require_selector(selector, "css")?;
                response.css(selector)?.getall()
            }
            ExtractKind::Xpath => {
                let expression = require_selector(selector, "xpath")?;
                response.xpath(expression)?.getall()
            }
            ExtractKind::Markdown => vec![response.markdown_with(selector, false)?],
            ExtractKind::Text => vec![response.doc().clean_for_ai().get_all_text()],
        };

        let count = matches.len();
        let (matches, truncated) = truncate_matches(matches, self.config.max_content_chars);
        self.publish(
            "extract",
            request.opts.session.as_deref(),
            format!(
                "{:?} {} → {count} match(es)",
                request.kind,
                selector.unwrap_or("-")
            ),
        );
        Ok(ExtractResult {
            kind: request.kind,
            url,
            selector: request.selector.clone(),
            matches,
            count,
            truncated,
        })
    }

    // -- crawl ------------------------------------------------------------

    /// Crawl from a seed, bounded by page count and depth.
    ///
    /// Same-host by default, `robots.txt` always respected, and the page budget
    /// is spent when a request is **enqueued** rather than when it completes —
    /// so `max_pages` bounds the number of fetches, not merely the number of
    /// results, even with several requests in flight.
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] for a bad seed or a malformed selector.
    pub async fn crawl(&self, request: &CrawlRequest) -> Result<CrawlOutcome> {
        let seed = validate_url(&request.seed)?;
        let session = self.session_for(&request.opts).await?;
        let egress = self.resolve_egress(&request.opts, session.as_deref());
        let max_pages = request.max_pages.clamp(1, self.config.max_crawl_pages);
        let started = Instant::now();

        let outcome = crawler::run(crawler::Job {
            seed: seed.clone(),
            max_pages,
            max_depth: request.max_depth,
            same_host: request.same_host,
            selector: request.selector.clone(),
            egress: egress.clone(),
            http: session.as_ref().map(|s| s.http().clone()),
        })
        .await?;

        if let Some(session) = &session {
            session.touch();
        }
        self.publish(
            "crawl",
            session.as_ref().map(|s| s.name.as_str()),
            format!("{} → {} page(s)", seed, outcome.pages.len()),
        );
        Ok(CrawlOutcome {
            seed: seed.to_string(),
            max_pages,
            max_depth: request.max_depth,
            stopped_at_limit: outcome.stopped_at_limit,
            pages: outcome.pages,
            elapsed_ms: elapsed_ms(started),
        })
    }

    // -- screenshot -------------------------------------------------------

    /// Capture a page as PNG bytes.
    ///
    /// With a `session`, a `url` is navigated to first (if given) and then
    /// whatever tab that session is on is captured — so an agent can browse,
    /// look at the result, and shoot the same page without reloading it. With
    /// no session a `url` is required, and a throw-away browser lives exactly
    /// as long as the capture.
    ///
    /// The PNG is at the browser's device scale factor, which `browser-cdp`
    /// pins at 2 — see [`capture_png`](fn@capture_png).
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] with neither a `url` nor a page to shoot,
    /// [`ServerError::BrowserLimit`] when every slot is taken, and whatever the
    /// navigation or the capture failed with.
    pub async fn screenshot(&self, request: &ScreenshotRequest) -> Result<Vec<u8>> {
        let session = self.session_for(&request.opts).await?;

        let png = match (&session, &request.url) {
            (Some(session), url) => {
                if let Some(url) = url {
                    // Through `browse`, not around it: the session's slot cap,
                    // its egress, its activity event and its cookie mirror are
                    // all things a second navigation path would have to
                    // reimplement and would eventually get wrong.
                    self.browse(&BrowseRequest {
                        url: url.clone(),
                        opts: request.opts.clone(),
                        format: ContentFormat::Html,
                        ..BrowseRequest::default()
                    })
                    .await?;
                }
                let page = session.live_page().ok_or_else(|| {
                    ServerError::BadRequest(
                        "this session has no page yet; pass a `url`, or browse with it first"
                            .to_owned(),
                    )
                })?;
                capture_png(&page, request.full_page).await?
            }
            (None, Some(url)) => self.screenshot_once(url, request).await?,
            (None, None) => {
                return Err(ServerError::BadRequest(
                    "screenshot needs either `url` or `session`".to_owned(),
                ))
            }
        };

        self.publish(
            "screenshot",
            request.opts.session.as_deref(),
            request
                .url
                .clone()
                .unwrap_or_else(|| "<the session's current page>".to_owned()),
        );
        Ok(png)
    }

    /// A stateless screenshot: launch, navigate, capture, close.
    ///
    /// [`Engine::browse`]'s stateless path uses `fetch_once`, which closes the
    /// browser before it returns — fine for HTML, useless for pixels. This is
    /// the same shape with the close deferred until after the capture.
    async fn screenshot_once(&self, url: &str, request: &ScreenshotRequest) -> Result<Vec<u8>> {
        let url = validate_url(url)?;
        let egress = self.resolve_egress(&request.opts, None);

        let permit = Arc::clone(self.sessions.slots())
            .try_acquire_owned()
            .map_err(|_| ServerError::BrowserLimit {
                max: self.config.max_live_browsers.max(1),
            })?;

        let slot = PageSlot::default();
        // An empty `user_data_dir` is what makes `browser-cdp` create a
        // temporary profile and delete it on close. Stateless means stateless.
        let config = egress.browser_config(Path::new(""), request.opts.headless);
        let browser = BrowserSession::with_hooks(config, page_slot_hooks(&slot))?;
        browser.start().await?;

        // Every path from here has to reach the `close()` below, including the
        // error ones, or a stateless call leaks a Chrome process and its slot.
        let captured = async {
            browser.fetch(url.as_str(), FetchParams::default()).await?;
            let page = slot
                .lock()
                .expect("not poisoned")
                .clone()
                .ok_or_else(|| ServerError::internal("the page_action hook did not run"))?;
            capture_png(&page, request.full_page).await
        }
        .await;

        browser.close().await;
        drop(permit);
        captured
    }

    // -- shared plumbing --------------------------------------------------

    /// The effective egress for one operation: request, else session, else
    /// server default.
    fn resolve_egress(&self, opts: &OpenOpts, session: Option<&Session>) -> Egress {
        let base = session.map_or_else(Egress::default, |session| session.egress.clone());
        base.overlay(&opts.egress()).or_defaults(&self.config)
    }

    fn page_result(
        &self,
        response: &Response,
        format: ContentFormat,
        transport: Transport,
        session: Option<&Session>,
        egress: &Egress,
        started: Instant,
    ) -> PageResult {
        let body = match format {
            ContentFormat::Html => response.html(),
            ContentFormat::Markdown => response.markdown(),
            ContentFormat::Text => response.doc().clean_for_ai().get_all_text(),
        };
        let (content, truncated) = truncate(body, self.config.max_content_chars);
        PageResult {
            url: response.url.clone(),
            status: response.status,
            title: page_title(response),
            format,
            content,
            truncated,
            headers: response.headers.clone(),
            cookies: response.cookies.clone(),
            session: session.map(|session| session.name.clone()),
            proxy: egress.proxy.as_ref().map(redact_proxy),
            transport,
            elapsed_ms: elapsed_ms(started),
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Capture a live tab as PNG bytes.
///
/// One line, on purpose. `API.md` §4 puts `CdpPage::screenshot` on the far side
/// of the `browser-cdp` boundary precisely so that no CDP type — and no
/// `chromiumoxide` dependency — has to reach this crate. The whole of the work
/// is on the other two sides of it: `browser-cdp` clips to
/// `Page.getLayoutMetrics`'s `cssContentSize` for a full-page shot rather than
/// resizing the viewport, and [`crate::session::page_slot_hooks`] is what gets
/// a tab up here in the first place.
///
/// # A pixel is not a CSS pixel
///
/// `browser-cdp` emulates `device_scale_factor: 2`, so a 1280×720 viewport
/// comes back as a **2560×1440** PNG. That is right for a human looking at the
/// image and wrong for anything that assumes CSS pixels, so
/// [`ScreenshotRequest`]'s description says it and the OpenAPI document
/// repeats it. A full-page shot of a long document is correspondingly large —
/// a 5000 px page is a 10000 px image — which is worth remembering next to a
/// body-size limit if one is ever added to *responses*.
///
/// # Errors
///
/// [`ServerError::Internal`] for a CDP failure (`Page.getLayoutMetrics` or
/// `Page.captureScreenshot`), via the [`browser::BrowserError`] conversion.
pub async fn capture_png(page: &CdpPage, full_page: bool) -> Result<Vec<u8>> {
    page.screenshot(full_page).await.map_err(Into::into)
}

/// Reject anything that is not an absolute `http`/`https` URL.
///
/// The scheme check is the point. This server fetches whatever it is told to,
/// so `file:///etc/shadow` and `data:` have to be refused here, once, rather
/// than in five handlers.
fn validate_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw.trim())?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(ServerError::BadRequest(format!(
            "scheme {other:?} is not fetchable; use http or https"
        ))),
    }
}

fn require_selector<'a>(selector: Option<&'a str>, kind: &str) -> Result<&'a str> {
    selector
        .filter(|selector| !selector.trim().is_empty())
        .ok_or_else(|| ServerError::BadRequest(format!("{kind} extraction needs a `selector`")))
}

/// Apply the request's egress to a fluent request builder.
///
/// `stateless` decides how much has to be said: a session's fetcher already
/// carries its proxy, profile and User-Agent in its own `FetcherConfig`, so
/// only what the *request* overrode is applied on top. A stateless request has
/// no such config and gets the whole resolved egress.
fn apply_egress(
    builder: RequestBuilder,
    opts: &OpenOpts,
    stateless: bool,
    resolved: &Egress,
) -> RequestBuilder {
    let effective = if stateless { resolved } else { &opts.egress() };
    let mut builder = builder;
    if let Some(proxy) = &effective.proxy {
        builder = builder.proxy(Some(proxy.clone()));
    }
    if let Some(profile) = &effective.impersonate {
        builder = builder.impersonate(Impersonate::single(profile.clone()));
    }
    if let Some(agent) = &effective.user_agent {
        builder = builder.header("User-Agent", agent.clone());
    }
    builder
}

/// The `FetchParams` a browse request describes.
///
/// **One divergence worth naming:** `browser-cdp` always waits for the `load`
/// event before it looks at `load_dom` or `network_idle`, so `wait_until:
/// "commit"` cannot be honoured — it behaves as `load`. The other three are
/// exact.
fn fetch_params(request: &BrowseRequest) -> FetchParams {
    let (load_dom, network_idle) = match request.wait_until.unwrap_or_default() {
        PageLoadState::Commit | PageLoadState::Load => (false, false),
        PageLoadState::DomContentLoaded => (true, false),
        PageLoadState::NetworkIdle => (true, true),
    };
    FetchParams {
        load_dom: Override::Set(load_dom),
        network_idle: Override::Set(network_idle),
        wait_selector: Override::Set(request.wait_for_selector.clone()),
        wait_selector_state: request
            .wait_for_state
            .map_or(Override::Unset, Override::Set),
        timeout_ms: request
            .timeout_ms
            .map_or(Override::Unset, |ms| Override::Set(ms as f64)),
        // A session's browser was launched with the session's proxy, so this is
        // the only way a *request* can override it without relaunching Chrome —
        // `browser-cdp` opens a fresh context for the call. Redundant on the
        // stateless path, where the same proxy is already a launch flag, and
        // harmless there.
        proxy: request.opts.proxy.clone(),
        ..FetchParams::default()
    }
}

/// The page's `<title>`, trimmed, or `None`.
fn page_title(response: &Response) -> Option<String> {
    response
        .css_first("title")
        .ok()
        .flatten()
        .map(|node| node.get_all_text().trim().to_owned())
        .filter(|title| !title.is_empty())
}

/// Cut `text` to `max` **characters**, leaving a marker the reader can see.
///
/// `API.md` §3 asks for truncation a model can notice rather than a silently
/// short answer, so the marker says how much went missing. Counting characters
/// and not bytes is what keeps the cut on a `char` boundary.
fn truncate(text: String, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text, false);
    }
    let kept: String = text.chars().take(max).collect();
    let dropped = text.chars().count() - max;
    (
        format!("{kept}\n\n… [truncated: {dropped} more characters]"),
        true,
    )
}

/// Truncate a list of matches against one shared character budget.
fn truncate_matches(matches: Vec<String>, max: usize) -> (Vec<String>, bool) {
    let mut budget = max;
    let mut out = Vec::with_capacity(matches.len());
    let total = matches.len();
    for (index, value) in matches.into_iter().enumerate() {
        if budget == 0 {
            out.push(format!(
                "… [truncated: {} more match(es)]",
                total.saturating_sub(index)
            ));
            return (out, true);
        }
        let (value, cut) = truncate(value, budget);
        budget = budget.saturating_sub(value.chars().count());
        out.push(value);
        if cut {
            return (out, true);
        }
    }
    (out, false)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A counter the crawler uses to spend its page budget atomically.
type Budget = AtomicUsize;

/// Spend one unit of a budget, returning `false` when it is exhausted.
fn spend(budget: &Budget) -> bool {
    budget
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_ok()
}

// ---------------------------------------------------------------------------
// The crawler
// ---------------------------------------------------------------------------

/// A bounded crawl, expressed as a `browser-spider` [`Spider`].
///
/// [`Spider`]: browser::Spider
///
/// Nothing here re-implements crawling. `browser-spider` already owns the
/// scheduler, the duplicate filter, the offsite gate, the throttle and the
/// `robots.txt` cache, and `browser-templates` already owns link extraction;
/// this module is a spider that spends a budget, which is the one thing
/// `SpiderSettings` has no field for.
///
/// # How the bound is enforced
///
/// `max_pages` is spent when a request is **enqueued**, not when it finishes.
/// Spending on completion would let a burst of in-flight requests overshoot the
/// budget by the concurrency factor, and a caller who asks for ten pages and
/// gets fourteen has not been given a bound. The seed costs one, every followed
/// link costs one, and the counter is a `fetch_update` so several callbacks
/// racing cannot spend the same unit twice.
///
/// The cost of choosing that direction: a link the scheduler then rejects — a
/// duplicate, or an offsite URL the extractor let through — has already been
/// paid for, so a crawl can come back with **fewer** pages than `max_pages`
/// even where more were reachable. That is the error worth having. `max_pages`
/// is a ceiling on how much of someone's site this server will pull, and a
/// ceiling that can be exceeded is not one.
mod crawler {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use browser::spider::request::{CallbackId, Item};
    use browser::spider::session::{AddOptions, SessionManager};
    use browser::spider::spider::{
        CallbackStream, CrawlError, CrawlResponse, FollowOptions, HttpCrawlSession, RunOptions,
        Spider, SpiderRunner, SpiderSettings, Yielded,
    };
    use browser::{async_trait, AsyncFetcherSession, Document, LinkExtractor};
    use serde_json::Value;
    use url::Url;

    use super::{spend, Budget, CrawlPage, Egress};
    use crate::error::{Result, ServerError};

    /// How many requests a crawl runs at once.
    ///
    /// Four, not `browser-spider`'s default: this is one HTTP call inside an
    /// HTTP handler that a caller is waiting on, so the crawl should be brisk
    /// without hammering one host. The per-domain limiter and `robots.txt`'s
    /// crawl-delay still apply on top.
    const CONCURRENCY: usize = 4;

    /// The meta key each request carries its distance from the seed in.
    const DEPTH: &str = "depth";

    /// One crawl to run.
    pub struct Job {
        /// The validated seed.
        pub seed: Url,
        /// The already-clamped page budget.
        pub max_pages: usize,
        /// How many links deep to follow.
        pub max_depth: usize,
        /// Whether to stay on the seed's host and port.
        pub same_host: bool,
        /// A CSS selector to run on every page.
        pub selector: Option<String>,
        /// The resolved egress, used when the crawl has no session.
        pub egress: Egress,
        /// The session's fetcher, when the crawl runs on one. Sharing it is
        /// what lets a crawl inherit a login: the cookie jar is behind an
        /// `Arc`, so the clone and the session are the same jar.
        pub http: Option<AsyncFetcherSession>,
    }

    /// What one crawl produced.
    pub struct Outcome {
        /// The pages, in callback order.
        pub pages: Vec<CrawlPage>,
        /// Whether the budget, rather than the link graph, ended it.
        pub stopped_at_limit: bool,
    }

    struct ServerSpider {
        settings: SpiderSettings,
        extractor: LinkExtractor,
        max_depth: usize,
        selector: Option<String>,
        /// Links still affordable. The seed is already paid for.
        budget: Budget,
        hit_limit: AtomicBool,
        fetcher: AsyncFetcherSession,
    }

    #[async_trait]
    impl Spider for ServerSpider {
        fn name(&self) -> &str {
            "browser-server"
        }

        fn settings(&self) -> &SpiderSettings {
            &self.settings
        }

        /// Route every request through the session's fetcher.
        ///
        /// The default would build a fresh `HttpCrawlSession`, with a fresh
        /// cookie jar and none of the session's egress — which is precisely
        /// the bug this override exists to prevent.
        fn configure_sessions(
            &self,
            manager: &mut SessionManager,
        ) -> std::result::Result<(), CrawlError> {
            let session = HttpCrawlSession::with_session("default", self.fetcher.clone());
            manager.add("default", Arc::new(session), AddOptions::DEFAULT)?;
            Ok(())
        }

        async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
            let depth = response
                .meta
                .get(DEPTH)
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;

            let mut yielded = vec![Yielded::item(self.page_item(&response, depth))];

            if depth < self.max_depth {
                for url in self.extractor.extract(&response) {
                    if !spend(&self.budget) {
                        // Out of budget with links still on the page: that is
                        // the difference between "crawled the whole site" and
                        // "stopped early", and the caller is told which.
                        self.hit_limit.store(true, Ordering::SeqCst);
                        break;
                    }
                    let mut meta = Item::new();
                    meta.insert(DEPTH.to_owned(), Value::from(depth as u64 + 1));
                    let options = FollowOptions {
                        callback: Some(CallbackId::PARSE),
                        meta: Some(meta),
                        ..FollowOptions::default()
                    };
                    match response.follow_with(&url, &options) {
                        Ok(request) => yielded.push(Yielded::request(request)),
                        Err(error) => {
                            tracing::debug!(%url, %error, "could not follow a link");
                            // The unit is refunded: a link that never became a
                            // request never cost a fetch.
                            self.budget.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }

            Box::pin(browser::futures::stream::iter(yielded))
        }
    }

    impl ServerSpider {
        fn page_item(&self, response: &CrawlResponse, depth: usize) -> Item {
            let mut item = Item::new();
            item.insert("url".to_owned(), Value::from(response.url.clone()));
            item.insert("depth".to_owned(), Value::from(depth as u64));
            item.insert("status".to_owned(), Value::from(response.status));
            if let Some(title) = super::page_title(response) {
                item.insert("title".to_owned(), Value::from(title));
            }
            if let Some(selector) = &self.selector {
                // The selector was validated before the crawl started, so a
                // failure here is a page that could not be parsed at all.
                let matches = response
                    .css(selector)
                    .map(|nodes| nodes.getall())
                    .unwrap_or_default();
                item.insert(
                    "matches".to_owned(),
                    Value::from(matches.into_iter().map(Value::from).collect::<Vec<_>>()),
                );
            }
            item
        }
    }

    /// Run one crawl to completion.
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] when the selector does not parse, and
    /// whatever `browser-spider` reports otherwise.
    pub async fn run(job: Job) -> Result<Outcome> {
        // Fail on a bad selector before a single request goes out, so the
        // caller gets a 400 rather than a crawl that quietly matches nothing.
        if let Some(selector) = &job.selector {
            Document::parse("<p></p>").css(selector)?;
        }

        let mut settings = SpiderSettings::default()
            .with_start_urls([job.seed.to_string()])
            .with_robots_txt_obey(true)
            .with_concurrent_requests(CONCURRENCY);
        if job.same_host {
            let host = job
                .seed
                .host_str()
                .ok_or_else(|| ServerError::BadRequest("the seed URL has no host".to_owned()))?;
            // The netloc, port included — `SpiderSettings::allowed_domains`
            // compares against exactly that, which is what makes two loopback
            // fixtures on different ports different sites.
            settings = settings.with_allowed_domains([match job.seed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            }]);
        }

        let fetcher = match job.http {
            Some(session) => session,
            None => AsyncFetcherSession::builder()
                .config(job.egress.fetcher_config())
                .build()
                .map_err(ServerError::from)?,
        };

        let spider = Arc::new(ServerSpider {
            settings,
            extractor: LinkExtractor::builder()
                .build()
                .map_err(ServerError::bad_request)?,
            max_depth: job.max_depth,
            selector: job.selector,
            // The seed is already spent; this is what is left for its links.
            budget: Budget::new(job.max_pages.saturating_sub(1)),
            hit_limit: AtomicBool::new(false),
            fetcher,
        });

        let runner = SpiderRunner::new(
            Arc::clone(&spider) as Arc<dyn Spider>,
            RunOptions::default().with_ctrl_c(false),
        )?;
        let result = runner.run().await?;

        let pages = result
            .items
            .as_slice()
            .iter()
            .map(|item| CrawlPage {
                url: item
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                depth: item.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
                status: item
                    .get("status")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .try_into()
                    .unwrap_or(0),
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                matches: item.get("matches").and_then(Value::as_array).map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                }),
            })
            .collect();

        Ok(Outcome {
            pages,
            stopped_at_limit: spider.hit_limit.load(Ordering::SeqCst),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written `FIELDS` lists, checked against what serde actually
    /// emits. Every `Option` is `Some` so nothing is skipped on the way out —
    /// that is what makes the key sets comparable in both directions.
    #[test]
    fn the_accepted_field_lists_match_what_serde_accepts() {
        let opts = OpenOpts {
            session: Some("s".to_owned()),
            proxy: Some(Proxy::Url("http://p:1".to_owned())),
            impersonate: Some("chrome".to_owned()),
            user_agent: Some("UA".to_owned()),
            headless: true,
        };
        FetchRequest::rot_check(&FetchRequest {
            url: "https://x.test/".to_owned(),
            opts: opts.clone(),
            headers: Some(HeaderDict::new()),
            format: ContentFormat::Html,
            timeout_ms: Some(1),
        });
        BrowseRequest::rot_check(&BrowseRequest {
            url: "https://x.test/".to_owned(),
            opts: opts.clone(),
            wait_until: Some(PageLoadState::Load),
            wait_for_selector: Some("h1".to_owned()),
            wait_for_state: Some(WaitState::Attached),
            timeout_ms: Some(1),
            format: ContentFormat::Html,
        });
        ExtractRequest::rot_check(&ExtractRequest {
            url: Some("https://x.test/".to_owned()),
            html: Some("<p/>".to_owned()),
            opts: opts.clone(),
            selector: Some("p".to_owned()),
            kind: ExtractKind::Css,
            render: false,
        });
        CrawlRequest::rot_check(&CrawlRequest {
            seed: "https://x.test/".to_owned(),
            opts: opts.clone(),
            max_pages: 1,
            max_depth: 1,
            same_host: true,
            selector: Some("p".to_owned()),
        });
        ScreenshotRequest::rot_check(&ScreenshotRequest {
            url: Some("https://x.test/".to_owned()),
            opts,
            full_page: true,
        });
        SessionRequest::rot_check(&SessionRequest {
            name: "s".to_owned(),
            proxy: Some(Proxy::Url("http://p:1".to_owned())),
            impersonate: Some("chrome".to_owned()),
            user_agent: Some("UA".to_owned()),
        });
    }

    /// The whole point of the check: a plausible typo is named, and the field
    /// it was probably meant to be is offered.
    #[test]
    fn an_unknown_field_is_rejected_with_the_field_it_was_probably_meant_to_be() {
        let keys = ["sesion".to_owned()];
        let error = reject_unknown_fields(keys.iter(), FetchRequest::FIELDS).expect_err("rejected");
        assert_eq!(error.kind(), "bad_request");
        let message = error.public_message();
        assert!(message.contains("sesion"), "{message}");
        assert!(message.contains("session"), "{message}");
    }

    #[test]
    fn a_key_nothing_resembles_is_rejected_without_a_wrong_guess() {
        let keys = ["quantum_flux".to_owned()];
        let message = reject_unknown_fields(keys.iter(), FetchRequest::FIELDS)
            .expect_err("rejected")
            .public_message();
        assert!(message.contains("quantum_flux"), "{message}");
        assert!(!message.contains("did you mean"), "{message}");
        assert!(
            message.contains("url"),
            "the alternatives are listed: {message}"
        );
    }

    #[test]
    fn the_real_field_names_are_accepted() {
        let keys: Vec<String> = FetchRequest::FIELDS
            .iter()
            .map(|f| (*f).to_owned())
            .collect();
        assert!(reject_unknown_fields(keys.iter(), FetchRequest::FIELDS).is_ok());
    }

    #[test]
    fn edit_distance_is_the_usual_one() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("session", "session"), 0);
        assert_eq!(edit_distance("sesion", "session"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    #[test]
    fn only_http_urls_are_fetchable() {
        assert!(validate_url("https://example.test/a").is_ok());
        assert!(validate_url("http://example.test/a").is_ok());

        let error = validate_url("file:///etc/shadow").expect_err("refused");
        assert_eq!(error.kind(), "bad_request");
        assert!(error.public_message().contains("file"));

        assert_eq!(
            validate_url("/relative").expect_err("refused").kind(),
            "bad_request"
        );
    }

    #[test]
    fn truncation_is_visible_and_lands_on_a_char_boundary() {
        let (kept, cut) = truncate("é".repeat(10), 4);
        assert!(cut);
        assert!(kept.starts_with("éééé"));
        assert!(kept.contains("6 more characters"));

        let (kept, cut) = truncate("short".to_owned(), 100);
        assert!(!cut);
        assert_eq!(kept, "short");
    }

    #[test]
    fn a_list_of_matches_shares_one_budget() {
        let matches = vec!["aaaa".to_owned(), "bbbb".to_owned(), "cccc".to_owned()];
        let (out, cut) = truncate_matches(matches, 5);
        assert!(cut);
        assert_eq!(out[0], "aaaa");
        assert!(out.last().expect("a marker").contains("truncated"));

        let (out, cut) = truncate_matches(vec!["a".to_owned()], 100);
        assert!(!cut);
        assert_eq!(out, ["a"]);
    }

    /// Three fixed points and the two that are easy to get wrong: a leap day,
    /// and the day after one.
    #[test]
    fn the_timestamp_is_a_real_rfc_3339_instant() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_000), "1970-01-01T00:00:01.000Z");
        // 2001-09-09T01:46:40Z, the one every clock-arithmetic bug agrees on.
        assert_eq!(rfc3339(1_000_000_000_000), "2001-09-09T01:46:40.000Z");
        // 2024-02-29, a leap day in a leap century-rule year.
        assert_eq!(rfc3339(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(1_709_251_199_999), "2024-02-29T23:59:59.999Z");
        assert_eq!(rfc3339(1_709_251_200_000), "2024-03-01T00:00:00.000Z");
        // 1900 is *not* a leap year, which the naive /4 rule gets wrong.
        assert_eq!(rfc3339(4_107_542_400_000), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn a_budget_cannot_be_overspent() {
        let budget = Budget::new(2);
        assert!(spend(&budget));
        assert!(spend(&budget));
        assert!(!spend(&budget));
        assert_eq!(budget.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_request_wins_over_the_session_which_wins_over_the_server() {
        let engine = Engine::new(ServerConfig {
            default_impersonate: Some("firefox147".to_owned()),
            default_user_agent: Some("Server/1".to_owned()),
            ..ServerConfig::default()
        });
        let opts = OpenOpts {
            user_agent: Some("Request/1".to_owned()),
            ..OpenOpts::default()
        };
        let egress = engine.resolve_egress(&opts, None);
        assert_eq!(egress.user_agent.as_deref(), Some("Request/1"));
        assert_eq!(egress.impersonate.as_deref(), Some("firefox147"));
    }

    /// `commit` has nowhere to map to; the comment on [`fetch_params`] says so
    /// and this is the assertion that keeps the comment honest.
    #[test]
    fn the_wait_states_map_onto_the_two_flags_browser_cdp_exposes() {
        let params = |state: Option<PageLoadState>| {
            fetch_params(&BrowseRequest {
                url: "https://x.test/".to_owned(),
                wait_until: state,
                ..BrowseRequest::default()
            })
        };
        assert_eq!(
            (
                params(Some(PageLoadState::Load)).load_dom,
                params(Some(PageLoadState::Load)).network_idle
            ),
            (Override::Set(false), Override::Set(false))
        );
        assert_eq!(
            params(Some(PageLoadState::Commit)).load_dom,
            params(Some(PageLoadState::Load)).load_dom,
            "commit cannot be honoured and behaves as load"
        );
        assert_eq!(
            params(Some(PageLoadState::NetworkIdle)).network_idle,
            Override::Set(true)
        );
        // The default is `domcontentloaded`, matching `BrowserConfig`.
        assert_eq!(params(None).load_dom, Override::Set(true));
    }

    #[tokio::test]
    async fn extract_refuses_a_source_less_request() {
        let engine = Engine::new(ServerConfig::default());
        let error = engine
            .extract(&ExtractRequest::default())
            .await
            .expect_err("no source");
        assert_eq!(error.kind(), "bad_request");
    }

    #[tokio::test]
    async fn extract_runs_against_inline_html_without_a_network() {
        let engine = Engine::new(ServerConfig::default());
        let result = engine
            .extract(&ExtractRequest {
                html: Some("<h1>One</h1><h1>Two</h1>".to_owned()),
                selector: Some("h1::text".to_owned()),
                kind: ExtractKind::Css,
                ..ExtractRequest::default()
            })
            .await
            .expect("extracted");
        assert_eq!(result.matches, ["One", "Two"]);
        assert_eq!(result.count, 2);
        assert!(result.url.is_none());
    }

    #[tokio::test]
    async fn a_malformed_selector_is_a_bad_request_not_a_panic() {
        let engine = Engine::new(ServerConfig::default());
        let error = engine
            .extract(&ExtractRequest {
                html: Some("<p>hi</p>".to_owned()),
                selector: Some("a[".to_owned()),
                kind: ExtractKind::Css,
                ..ExtractRequest::default()
            })
            .await
            .expect_err("malformed");
        assert_eq!(error.kind(), "bad_request");
    }

    #[tokio::test]
    async fn css_and_xpath_both_need_a_selector() {
        let engine = Engine::new(ServerConfig::default());
        for kind in [ExtractKind::Css, ExtractKind::Xpath] {
            let error = engine
                .extract(&ExtractRequest {
                    html: Some("<p>hi</p>".to_owned()),
                    kind,
                    ..ExtractRequest::default()
                })
                .await
                .expect_err("no selector");
            assert_eq!(error.kind(), "bad_request");
        }
    }

    #[tokio::test]
    async fn markdown_and_text_need_no_selector() {
        let engine = Engine::new(ServerConfig::default());
        let markdown = engine
            .extract(&ExtractRequest {
                html: Some("<h1>Title</h1><script>evil()</script>".to_owned()),
                kind: ExtractKind::Markdown,
                ..ExtractRequest::default()
            })
            .await
            .expect("converted");
        assert!(markdown.matches[0].contains("Title"));
        assert!(!markdown.matches[0].contains("evil"));

        let text = engine
            .extract(&ExtractRequest {
                html: Some("<p>keep</p><script>evil()</script>".to_owned()),
                kind: ExtractKind::Text,
                ..ExtractRequest::default()
            })
            .await
            .expect("cleaned");
        assert!(text.matches[0].contains("keep"));
        assert!(!text.matches[0].contains("evil"));
    }

    #[tokio::test]
    async fn the_activity_log_carries_every_operation() {
        let engine = Engine::new(ServerConfig::default());
        let mut events = engine.subscribe();
        engine
            .extract(&ExtractRequest {
                html: Some("<p>hi</p>".to_owned()),
                selector: Some("p".to_owned()),
                ..ExtractRequest::default()
            })
            .await
            .expect("extracted");
        let event = events.try_recv().expect("one event");
        assert_eq!(event.kind, "extract");
    }

    /// A session that has never browsed has no tab to shoot, and saying so is
    /// more useful than launching one the caller did not ask for.
    #[tokio::test]
    async fn a_screenshot_of_a_session_with_no_page_says_so() {
        let profiles = tempfile::tempdir().expect("a temp dir");
        let engine = Engine::new(ServerConfig {
            profile_root: profiles.path().to_path_buf(),
            ..ServerConfig::default()
        });
        let error = engine
            .screenshot(&ScreenshotRequest {
                opts: OpenOpts {
                    session: Some("shop".to_owned()),
                    ..OpenOpts::default()
                },
                ..ScreenshotRequest::default()
            })
            .await
            .expect_err("no page");
        assert_eq!(error.kind(), "bad_request");
        assert!(error.public_message().contains("no page yet"));
    }

    #[tokio::test]
    async fn a_screenshot_with_neither_a_url_nor_a_session_is_a_bad_request() {
        let engine = Engine::new(ServerConfig::default());
        let error = engine
            .screenshot(&ScreenshotRequest::default())
            .await
            .expect_err("no target");
        assert_eq!(error.kind(), "bad_request");
    }
}
