//! The operator dashboard page.
//!
//! Two routes and no state of its own: [`INDEX_PATH`] serves one self-contained
//! HTML document, and [`SESSIONS_PATH`] serves the JSON that document polls.
//! Everything live — frames, activity, input — goes over [`crate::ws`].
//!
//! # What it is for
//!
//! The repository owner opens a port, watches their agent drive a real browser,
//! **takes the controls and logs in by hand**, and gives them back. The agent
//! carries on in the same Chromium profile, now authenticated. The page says
//! that in as many words rather than leaving an operator to infer it from a
//! toggle button, because it is the feature and not a side effect.
//!
//! # Self-contained, on purpose
//!
//! The HTML, the CSS and the JavaScript are three files under `assets/`,
//! `include_str!`d into the binary and stitched into one document at first
//! request. No CDN, no web font, no npm, no build step: the container ships one
//! executable, and a dashboard that fetched anything from the internet would be
//! a strange thing to bolt onto a tool whose whole point is controlling what
//! this process talks to.
//!
//! Serving one document rather than three files also settles the credential
//! problem: a `<link>` or a `<script src>` would not carry the `?token=` the
//! navigation was opened with, and a browser cannot put a bearer token on a
//! top-level navigation. See [`crate::ws::auth`].

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use browser_cdp::session::resolve_executable;
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::session::Egress;
use crate::ws::auth;

/// Where the page is served.
pub const INDEX_PATH: &str = "/";

/// Where the page polls for the session list.
///
/// Not `/api/sessions`: that route answers with [`crate::session::SessionInfo`],
/// which has no current URL in it and cannot have one, because reading a URL
/// means asking a live tab. This route is the dashboard's own shape.
pub const SESSIONS_PATH: &str = "/dashboard/sessions.json";

/// The page's three parts, and the markers they replace.
const HTML: &str = include_str!("../assets/dashboard.html");
const CSS: &str = include_str!("../assets/dashboard.css");
const JS: &str = include_str!("../assets/dashboard.js");

/// The marker inside the document's `<style>`.
const CSS_MARKER: &str = "/* @@CSS@@ */";

/// The marker inside the document's `<script>`.
const JS_MARKER: &str = "/* @@JS@@ */";

/// The assembled document, built once.
static PAGE: OnceLock<String> = OnceLock::new();

/// The dashboard as one HTML document.
///
/// Assembled on first use and then handed out as a `&'static str`, so a request
/// copies nothing.
#[must_use]
pub fn page() -> &'static str {
    PAGE.get_or_init(|| HTML.replace(CSS_MARKER, CSS).replace(JS_MARKER, JS))
        .as_str()
}

// ---------------------------------------------------------------------------
// What the page polls for
// ---------------------------------------------------------------------------

/// One row of the session table.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionRow {
    /// The name an agent addresses this session by.
    pub name: String,
    /// The page's current URL, when a tab is live to ask.
    pub url: Option<String>,
    /// Whether a Chromium process is attached.
    pub browser: bool,
    /// How long since the session was opened.
    pub age_secs: u64,
    /// How long since it was last used.
    pub idle_secs: u64,
    /// How many requests have run on it.
    pub requests: u64,
    /// The persistent profile directory — the thing that keeps a login.
    pub profile_dir: String,
}

/// Everything the index needs in one request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Overview {
    /// The sessions that exist, newest last.
    pub sessions: Vec<SessionRow>,
    /// The Chromium binary this server would launch, if it can find one.
    ///
    /// `None` is the "no browser installed" case the page has to state
    /// plainly: `fetch` and `extract` still work, `browse` and the live view
    /// cannot, and an operator staring at an empty frame deserves to be told
    /// which of those they are looking at.
    pub chrome: Option<String>,
    /// Whether a bearer token is configured.
    pub authenticated: bool,
    /// Whether browsers launch headless. A headful one is what a human uses to
    /// log in on the server's own display; the dashboard works either way.
    pub headless: bool,
    /// The cap on concurrent live browsers.
    pub max_live_browsers: usize,
}

/// The Chromium binary this server's configuration would actually launch.
///
/// Probed through [`Egress::browser_config`] rather than a bare default, so
/// what the page reports is what a `browse` would use.
#[must_use]
pub fn chrome_path(engine: &Engine) -> Option<String> {
    let config = engine.config();
    let probe = Egress::default().browser_config(&config.profile_root, config.headless);
    resolve_executable(&probe)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Read every session into the dashboard's shape.
async fn overview(engine: &Arc<Engine>) -> Overview {
    let now = now_secs();
    let mut sessions = Vec::new();
    for session in engine.sessions().all() {
        let info = session.info().await;
        // Only ask a live tab for its URL. There is no cached one, and a
        // session that has never browsed has no answer to give.
        let url = match session.live_page() {
            Some(page) => page.url().await.ok(),
            None => None,
        };
        sessions.push(SessionRow {
            url,
            browser: info.browser_live,
            age_secs: now.saturating_sub(info.created_at),
            idle_secs: info.idle_secs,
            requests: info.requests,
            profile_dir: info.profile_dir,
            name: info.name,
        });
    }
    let config = engine.config();
    Overview {
        sessions,
        chrome: chrome_path(engine),
        authenticated: config.is_authenticated(),
        headless: config.headless,
        max_live_browsers: config.max_live_browsers,
    }
}

// ---------------------------------------------------------------------------
// The routes
// ---------------------------------------------------------------------------

/// The query string both routes accept.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Params {
    /// The bearer token, for a browser that cannot set a header. See
    /// [`crate::ws::auth`].
    #[serde(default)]
    pub token: Option<String>,
    /// Which session to open the live view on. The page reads this itself; the
    /// server only needs to accept it so the URL parses.
    #[serde(default)]
    pub session: Option<String>,
}

/// The dashboard routes, ready to merge into the application router.
///
/// Per `API.md` §3a: a plain `Router` with the engine already captured, and
/// none of the layers that go around the merged router.
pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(INDEX_PATH, get(index))
        .route(SESSIONS_PATH, get(sessions))
        .with_state(engine)
}

async fn index(
    State(engine): State<Arc<Engine>>,
    Query(params): Query<Params>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&engine, &headers, &params) {
        // Deliberately not the JSON body `/api` returns. Whoever is reading
        // this is looking at it in a browser, and "401" alone does not tell
        // them the one thing they need to do next.
        return (StatusCode::UNAUTHORIZED, Html(UNAUTHORIZED_PAGE)).into_response();
    }
    Html(page()).into_response()
}

async fn sessions(
    State(engine): State<Arc<Engine>>,
    Query(params): Query<Params>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&engine, &headers, &params) {
        return crate::error::ServerError::Unauthorized.into_response();
    }
    Json(overview(&engine).await).into_response()
}

fn authorized(engine: &Engine, headers: &HeaderMap, params: &Params) -> bool {
    auth::authorized(
        engine.config().api_token.as_deref(),
        headers,
        params.token.as_deref(),
    )
}

/// What an operator sees when they open the port with no token.
const UNAUTHORIZED_PAGE: &str = concat!(
    "<!doctype html><meta charset=\"utf-8\">",
    "<title>browser-rs — token required</title>",
    "<style>body{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;margin:0;",
    "min-height:100vh;display:grid;place-items:center;background:#101216;color:#e6e8ee}",
    "div{max-width:34rem;padding:2rem}code{background:#1b1e26;padding:.15em .4em;",
    "border-radius:4px;font-family:ui-monospace,monospace}</style>",
    "<div><h1>Token required</h1>",
    "<p>This server has <code>BROWSER_RS_API_TOKEN</code> set, so the dashboard ",
    "needs it too. A browser cannot put a bearer token on a page it navigates to, ",
    "so pass it in the URL:</p>",
    "<p><code>/?token=YOUR_TOKEN</code></p>",
    "<p>The token then stays in the address bar and in your browser history. If ",
    "that is not acceptable on the machine you are sitting at, use the REST API ",
    "with an <code>Authorization</code> header instead.</p></div>",
);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(ServerConfig {
            profile_root: std::env::temp_dir().join("browser-rs-dashboard-tests"),
            ..ServerConfig::default()
        }))
    }

    #[test]
    fn the_page_is_one_document_with_its_style_and_script_inlined() {
        let page = page();
        assert!(page.starts_with("<!doctype html>"), "a whole document");
        assert!(
            !page.contains(CSS_MARKER) && !page.contains(JS_MARKER),
            "both markers were replaced",
        );
        // The three files really are stitched together, not merely present.
        assert!(page.contains("--bg:"), "the stylesheet is in the document");
        assert!(page.contains("WebSocket"), "the script is in the document",);
    }

    #[test]
    fn the_page_asks_the_network_for_nothing() {
        let page = page();
        for external in ["http://", "https://", "//cdn", "<link", "src=\"http"] {
            assert!(
                !page.contains(external),
                "the dashboard must be self-contained, found {external:?}",
            );
        }
    }

    #[test]
    fn the_page_explains_the_take_control_and_log_in_flow() {
        let page = page().to_ascii_lowercase();
        // The feature the whole server exists for has to be on the page, not
        // implied by a button.
        assert!(page.contains("take control"));
        assert!(page.contains("log in"));
        assert!(page.contains("release"));
        assert!(
            page.contains("profile"),
            "and that the session's profile is what keeps the login",
        );
    }

    #[test]
    fn assembling_the_page_twice_gives_the_same_pointer() {
        assert!(std::ptr::eq(page(), page()), "built once, not per request");
    }

    #[tokio::test]
    async fn an_overview_of_an_empty_server_still_reports_the_configuration() {
        let engine = engine();
        let overview = overview(&engine).await;
        assert!(overview.sessions.is_empty());
        assert!(!overview.authenticated, "no token in the default config");
        assert!(overview.headless);
        assert_eq!(
            overview.max_live_browsers,
            ServerConfig::default().max_live_browsers,
        );
    }

    #[tokio::test]
    async fn a_session_appears_in_the_overview_with_no_browser_and_no_url() {
        let profiles = tempfile::tempdir().expect("a temp dir");
        let engine = Arc::new(Engine::new(ServerConfig {
            profile_root: profiles.path().to_path_buf(),
            ..ServerConfig::default()
        }));
        engine
            .sessions()
            .open("shop", Egress::default())
            .await
            .expect("the session opens");

        let overview = overview(&engine).await;
        assert_eq!(overview.sessions.len(), 1);
        let row = &overview.sessions[0];
        assert_eq!(row.name, "shop");
        assert!(!row.browser, "opening a session launches no browser");
        assert_eq!(row.url, None, "and so there is no URL to report");
        assert!(
            row.profile_dir.contains("shop"),
            "the profile directory is what survives a restart: {}",
            row.profile_dir,
        );
    }

    #[test]
    fn the_unauthorized_page_says_what_to_do_rather_than_just_refusing() {
        assert!(UNAUTHORIZED_PAGE.contains("?token="));
        assert!(
            UNAUTHORIZED_PAGE.contains("history"),
            "including the cost of putting it there",
        );
    }
}
