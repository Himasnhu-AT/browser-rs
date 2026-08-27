//! The axum routes under `/api`.
//!
//! A **thin** translation layer, and deliberately so: each handler
//! deserialises a body, calls exactly one [`crate::engine::Engine`] method and
//! serialises what comes back. There is no logic here to find, because the MCP
//! layer has to be able to call the same operations without going through HTTP,
//! and anything a handler decided for itself would be a decision `/mcp` did not
//! get. If you are looking for what a route *does*, it is in
//! [`crate::engine`].
//!
//! # The surface
//!
//! | Method | Path |
//! |---|---|
//! | `GET` | `/api/health` — unauthenticated |
//! | `GET` | `/api/openapi.json` |
//! | `POST` | `/api/fetch` |
//! | `POST` | `/api/browse` |
//! | `POST` | `/api/extract` |
//! | `POST` | `/api/crawl` |
//! | `POST` | `/api/screenshot` → `image/png` |
//! | `GET` | `/api/sessions` |
//! | `POST` | `/api/sessions` |
//! | `DELETE` | `/api/sessions/{name}` |
//!
//! # Auth
//!
//! `Authorization: Bearer <BROWSER_RS_API_TOKEN>` on everything but
//! `/api/health`. With no token configured the check passes and
//! [`crate::config::ServerConfig::effective_bind`] has already pinned the
//! listener to loopback — the two halves of one rule, and neither is optional.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::config::ServerConfig;
use crate::engine::{
    reject_unknown_fields, BrowseRequest, CrawlOutcome, CrawlRequest, Engine, ExtractRequest,
    ExtractResult, FetchRequest, PageResult, RequestFields, ScreenshotRequest, SessionRequest,
};
use crate::error::{Result, ServerError};
use crate::session::SessionInfo;

/// The one place a REST body becomes a request type.
///
/// It does two things axum's `Json<T>` does not.
///
/// **Its rejection is a [`ServerError`].** axum's own is a bare-text `422`,
/// which would be the single response on this surface that does not look like
/// every other one. An agent that sends a malformed body deserves the same
/// `{"error": {…}}` shape it gets for a malformed selector, and it deserves the
/// parser's message — that is how it works out which field it got wrong.
///
/// **It rejects unknown fields**, which serde cannot do for these types: every
/// request `#[serde(flatten)]`s an [`OpenOpts`], and `deny_unknown_fields` is
/// not supported alongside `flatten`. Doing it here rather than in the structs
/// keeps it to the REST path, which is where the hole is — the MCP layer parses
/// into its own `deny_unknown_fields` argument types before it ever builds one
/// of these, and constructs them in Rust rather than through serde.
///
/// The cost is parsing the body twice, once to [`Value`] and once out of it.
/// That is worth it: the alternative is `{"url": "…", "sesion": "shop"}`
/// quietly running **stateless** — right status, plausible body, no session, no
/// login — which is the failure mode nobody diagnoses.
#[derive(Debug)]
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned + RequestFields,
    Json<Value>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request(
        request: Request,
        state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        let Json(body) = Json::<Value>::from_request(request, state)
            .await
            .map_err(|rejection| ServerError::BadRequest(rejection.body_text()))?;

        let object = body.as_object().ok_or_else(|| {
            ServerError::BadRequest("the request body must be a JSON object".to_owned())
        })?;
        reject_unknown_fields(object.keys(), T::FIELDS)?;

        serde_json::from_value(body).map(Self).map_err(|error| {
            // `serde_json`'s message is the useful half here — it names the
            // field and the type it wanted.
            ServerError::BadRequest(error.to_string())
        })
    }
}

/// `GET /api/health`'s body.
#[derive(Debug, Serialize)]
pub struct Health {
    /// Always `"ok"`.
    pub status: &'static str,
    /// The crate version this server was built from.
    pub version: &'static str,
    /// Whether a bearer token is configured. `false` means the listener is
    /// loopback-only, and an operator checking that from another host will not
    /// be able to reach this route at all — which is itself the answer.
    pub authenticated: bool,
    /// How many sessions are open.
    pub sessions: usize,
}

/// The `/api` routes, ready to merge into the application router.
///
/// Auth, the body limit and the request timeout are **not** applied here; they
/// are in [`app`], so the dashboard and the MCP transport get the same
/// treatment rather than each remembering to ask for it.
pub fn router(engine: Arc<Engine>) -> Router {
    let authenticated = Router::new()
        .route("/api/openapi.json", get(openapi))
        .route("/api/fetch", post(fetch))
        .route("/api/browse", post(browse))
        .route("/api/extract", post(extract))
        .route("/api/crawl", post(crawl))
        .route("/api/screenshot", post(screenshot))
        .route("/api/sessions", get(sessions_list).post(sessions_open))
        .route("/api/sessions/{name}", delete(sessions_close))
        // `route_layer` and not `layer`: a layer would also run for requests
        // that match no route, turning every 404 on this server into a 401.
        .route_layer(from_fn_with_state(Arc::clone(&engine), require_token));

    Router::new()
        .route("/api/health", get(health))
        .merge(authenticated)
        .with_state(engine)
}

/// Wrap a router — any router — in the layers every surface on this port needs.
///
/// `API.md` §3a puts these around the **merged** router so no surface has to
/// remember to ask for them, which is why this takes a `Router` rather than an
/// engine: [`surfaces`] merges `/api`, `/mcp`, `/ws` and the dashboard first,
/// then this goes on once, around all of them.
///
/// The two `tower-http` layers §2 asks for are here, and the order matters: the
/// body limit is outermost so an oversized body is refused before anything
/// reads it, and the timeout is inside it so it bounds the handler rather than
/// the upload.
///
/// Auth is deliberately **not** one of them. `/api/health` has to answer
/// without a token, a dashboard opened in a browser cannot send an
/// `Authorization` header at all, and `/mcp` carries its own check so that it
/// is guarded however the router is composed. One blanket auth layer would have
/// to be wrong for at least one of those three.
pub fn with_layers(router: Router, config: &ServerConfig) -> Router {
    router
        .layer(TraceLayer::new_for_http())
        // 504, not `TimeoutLayer`'s default 408: the budget that ran out is
        // this server's own, waiting on a site or a browser, and that is what a
        // gateway timeout means. `ServerError::Timeout` maps to the same code,
        // so a caller sees one status for one condition.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            config.request_timeout,
        ))
        .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
        // Outermost, so it sees what the two layers above produce.
        .layer(axum::middleware::from_fn(give_bare_failures_a_body))
}

/// Give the `tower-http` layers' bodiless failures the same shape as everything
/// else on this server.
///
/// `RequestBodyLimitLayer` answers `413` with an empty body and `TimeoutLayer`
/// answers `504` with an empty body. Both are real conditions an agent has to
/// handle, and both would be the only two responses on the whole surface that
/// are not `{"error": {"kind", "message"}}` — which is exactly the sort of
/// inconsistency that turns into a special case in the client.
///
/// # What it rewrites, and what it must not touch
///
/// The test is the `Content-Type`, never the body — this middleware is
/// outermost and a full-page screenshot is measured in megabytes, so buffering
/// to look inside would be the wrong trade. A failing response qualifies when
/// it has **no** content type (axum's routing 404/405, the timeout layer) or a
/// `text/plain` one (`RequestBodyLimitLayer`'s 413). Everything this crate
/// answers deliberately says `application/json` or `image/png`, and the
/// dashboard says `text/html`, so nothing a handler produced is in scope.
///
/// **The status is always preserved.** `ServerError::into_response` picks the
/// status from the variant, which is right when a handler returns one and wrong
/// here — a `405` rewritten through `BadRequest` would silently become a `400`.
/// So the original goes back on afterwards, every time.
async fn give_bare_failures_a_body(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }
    let bare = match response.headers().get(header::CONTENT_TYPE) {
        None => true,
        Some(value) => value
            .to_str()
            .is_ok_and(|value| value.starts_with("text/plain")),
    };
    if !bare {
        return response;
    }

    let error = match status {
        StatusCode::PAYLOAD_TOO_LARGE => ServerError::BadRequest(
            "the request body is larger than this server accepts".to_owned(),
        ),
        StatusCode::GATEWAY_TIMEOUT | StatusCode::REQUEST_TIMEOUT => {
            ServerError::Timeout("the request took longer than this server allows".to_owned())
        }
        StatusCode::NOT_FOUND => ServerError::NotFound("no such route".to_owned()),
        StatusCode::METHOD_NOT_ALLOWED => {
            ServerError::BadRequest("that method is not allowed on this route".to_owned())
        }
        // Something else answered bare. Say as little about it as possible, but
        // say it in the house shape.
        other => ServerError::Internal(format!("a bare {other} escaped a layer")),
    };

    let mut rewritten = error.into_response();
    *rewritten.status_mut() = status;
    rewritten
}

/// Every surface this build has, merged — `API.md` §3a's composition point.
///
/// One function rather than one in `bin/serve.rs`, so that the tests drive the
/// *same* router the binary serves. A composition that only exists inside a
/// `main` is a composition nothing can test, and mounting `/mcp` is exactly the
/// kind of thing that is easy to get subtly wrong and never notice.
///
/// Each surface hands back a plain `Router` with the engine already captured,
/// per §3a. `mcp::router` additionally carries its **own** bearer check rather
/// than relying on a layer further out, which is right: an unauthenticated MCP
/// endpoint is an open browser proxy, and that must not depend on how someone
/// happened to compose the router.
///
/// `merge` throughout, never `nest`: all four routers register their own
/// absolute paths, and nesting `mcp::router` — which registers
/// [`crate::mcp::MOUNT_PATH`] itself — would serve it at `/mcp/mcp`. That is a
/// mistake that compiles, so `every_surface_is_reachable_on_the_one_port` in
/// `tests/rest_api.rs` posts a real `tools/list` and checks the catalogue comes
/// back.
pub fn surfaces(engine: Arc<Engine>) -> Router {
    router(Arc::clone(&engine))
        .merge(crate::dashboard::router(Arc::clone(&engine)))
        .merge(crate::ws::router(Arc::clone(&engine)))
        .merge(crate::mcp::router(engine))
}

/// [`surfaces`] with the layers applied: the whole application.
///
/// What `bin/serve.rs` serves and what the integration tests drive.
pub fn app(engine: Arc<Engine>) -> Router {
    let config = Arc::clone(engine.config());
    with_layers(surfaces(engine), &config)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Reject anything without the configured bearer token.
///
/// With no token configured every request passes: that is not a hole, it is the
/// documented single-user mode, and it is paired with a loopback-only listener
/// and a start-up warning. See [`crate::config::ServerConfig::effective_bind`].
async fn require_token(
    State(engine): State<Arc<Engine>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = engine.config().api_token.as_deref() else {
        return next.run(request).await;
    };
    match bearer(request.headers()) {
        // A plain `==` on a secret is a timing oracle in principle. It is a
        // very weak one over a network for a token this long, but constant-time
        // is not harder to write, so there is no reason to take the trade.
        Some(offered) if constant_time_eq(offered.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => ServerError::Unauthorized.into_response(),
    }
}

/// The token out of an `Authorization: Bearer …` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// Compare two byte strings without an early return.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health(State(engine): State<Arc<Engine>>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        authenticated: engine.config().is_authenticated(),
        sessions: engine.sessions().all().len(),
    })
}

async fn fetch(
    State(engine): State<Arc<Engine>>,
    ApiJson(request): ApiJson<FetchRequest>,
) -> Result<Json<PageResult>> {
    engine.fetch(&request).await.map(Json)
}

async fn browse(
    State(engine): State<Arc<Engine>>,
    ApiJson(request): ApiJson<BrowseRequest>,
) -> Result<Json<PageResult>> {
    engine.browse(&request).await.map(Json)
}

async fn extract(
    State(engine): State<Arc<Engine>>,
    ApiJson(request): ApiJson<ExtractRequest>,
) -> Result<Json<ExtractResult>> {
    engine.extract(&request).await.map(Json)
}

async fn crawl(
    State(engine): State<Arc<Engine>>,
    ApiJson(request): ApiJson<CrawlRequest>,
) -> Result<Json<CrawlOutcome>> {
    engine.crawl(&request).await.map(Json)
}

/// The one route that does not answer JSON.
async fn screenshot(
    State(engine): State<Arc<Engine>>,
    ApiJson(request): ApiJson<ScreenshotRequest>,
) -> Result<Response> {
    let png = engine.screenshot(&request).await?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

async fn sessions_list(State(engine): State<Arc<Engine>>) -> Json<Vec<SessionInfo>> {
    Json(engine.session_list().await)
}

async fn sessions_open(
    State(engine): State<Arc<Engine>>,
    ApiJson(request): ApiJson<SessionRequest>,
) -> Result<(StatusCode, Json<SessionInfo>)> {
    let info = engine.session_open(&request).await?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn sessions_close(
    State(engine): State<Arc<Engine>>,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    engine.session_close(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// The hand-written OpenAPI document
// ---------------------------------------------------------------------------

async fn openapi() -> Json<Value> {
    Json(openapi_document())
}

/// The description of this surface, written by hand.
///
/// Hand-written rather than derived, because `API.md` §2 asks for it and
/// because the useful half of an OpenAPI document for an agent is the prose: a
/// generator can tell you `selector` is a string, and only a person can tell
/// you it accepts `::text` and when to reach for `browse` instead of `fetch`.
/// Every field carries a description for the same reason `API.md` §3 demands it
/// of the MCP tools — this is the same information, for whoever is not speaking
/// MCP.
#[must_use]
pub fn openapi_document() -> Value {
    let open_opts = json!({
        "session": {
            "type": "string",
            "description": "A named, persistent session. Cookies and logins survive \
                            across requests and across restarts, because the session \
                            owns a real Chromium profile directory. Naming a session \
                            that does not exist opens it. Omit for a stateless call."
        },
        "proxy": {
            "type": "string",
            "description": "Egress proxy URL, e.g. http://user:pass@host:8080. \
                            Overrides the session's proxy and the server default for \
                            this one call."
        },
        "impersonate": {
            "type": "string",
            "description": "TLS/HTTP2 fingerprint profile, e.g. \"chrome\" or \
                            \"firefox147\". Applies to fetch/extract/crawl; a real \
                            browser already presents a real browser handshake, so \
                            browse ignores it."
        },
        "user_agent": {
            "type": "string",
            "description": "User-Agent for this call."
        }
    });

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "browser-rs server",
            "version": env!("CARGO_PKG_VERSION"),
            "description":
                "Fetch, extract and crawl the web — with a real browser when the page \
                 needs one, through a chosen egress proxy and a chosen browser \
                 fingerprint. Every operation takes an optional `session` name; the \
                 same name keeps cookies and logins, and a human can log in once \
                 through the dashboard and leave the agent authenticated afterwards."
        },
        "servers": [{"url": "/"}],
        "components": {
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description":
                        "The value of BROWSER_RS_API_TOKEN. When that variable is \
                         unset the server accepts any request and binds loopback only."
                }
            }
        },
        "security": [{"bearer": []}],
        "paths": {
            "/api/health": {
                "get": {
                    "summary": "Liveness. The one route that needs no token.",
                    "security": [],
                    "responses": {"200": {"description": "The server is up."}}
                }
            },
            "/api/openapi.json": {
                "get": {
                    "summary": "This document.",
                    "responses": {"200": {"description": "The API description."}}
                }
            },
            "/api/fetch": {
                "post": {
                    "summary": "Fetch a URL over HTTP. Fast, cheap, no browser.",
                    "description":
                        "Prefer this to /api/browse whenever the page's content is in \
                         the HTML the server sends. It costs a fraction as much and \
                         starts no process. Reach for /api/browse only when the page \
                         is rendered by JavaScript or needs a logged-in browser.",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {
                        "type": "object",
                        "required": ["url"],
                        "properties": merge(&open_opts, json!({
                            "url": {"type": "string", "description":
                                "Absolute http/https URL. Other schemes are refused."},
                            "headers": {"type": "object", "additionalProperties": {"type": "string"},
                                "description": "Extra request headers."},
                            "format": {"enum": ["html", "markdown", "text"], "default": "html",
                                "description":
                                    "html is the page as served. markdown and text are \
                                     noise-stripped and AI-sanitised — prefer them when \
                                     the consumer is a language model."},
                            "timeout_ms": {"type": "integer", "description":
                                "Per-request timeout in milliseconds."}
                        }))
                    }}}},
                    "responses": {
                        "200": {"description": "The page. A 4xx or 5xx from the site is \
                                                reported in `status`, not as an error."},
                        "400": {"description": "Bad URL or bad body."},
                        "502": {"description": "The site, or the proxy, failed."},
                        "504": {"description": "It ran out of time."}
                    }
                }
            },
            "/api/browse": {
                "post": {
                    "summary": "Navigate a real Chrome and return the rendered DOM.",
                    "description":
                        "Use for JavaScript-rendered pages and for pages that need the \
                         session's login. Each call holds one of a small number of \
                         browser slots; a 429 means they are all busy, and retrying is \
                         the right response.",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {
                        "type": "object",
                        "required": ["url"],
                        "properties": merge(&open_opts, json!({
                            "url": {"type": "string", "description": "Absolute http/https URL."},
                            "wait_until": {
                                "enum": ["commit", "domcontentloaded", "load", "networkidle"],
                                "default": "domcontentloaded",
                                "description":
                                    "How long to wait before snapshotting. networkidle is \
                                     the most patient and the slowest. \"commit\" cannot be \
                                     honoured by the CDP driver and behaves as \"load\"."
                            },
                            "wait_for_selector": {"type": "string", "description":
                                "A CSS selector that must appear before the snapshot."},
                            "wait_for_state": {
                                "enum": ["attached", "detached", "hidden", "visible"],
                                "default": "attached",
                                "description": "The state wait_for_selector must reach."
                            },
                            "timeout_ms": {"type": "integer", "description":
                                "Navigation timeout in milliseconds."},
                            "headless": {"type": "boolean", "default": true, "description":
                                "Headful is what a human uses to log in through the dashboard."},
                            "format": {"enum": ["html", "markdown", "text"], "default": "html",
                                "description":
                                    "html is the rendered DOM. markdown and text are \
                                     noise-stripped and AI-sanitised — prefer them when the \
                                     consumer is a language model."}
                        }))
                    }}}},
                    "responses": {
                        "200": {"description": "The rendered page."},
                        "429": {"description": "Every browser slot is in use."},
                        "501": {"description": "No browser is installed on this server."}
                    }
                }
            },
            "/api/extract": {
                "post": {
                    "summary": "Run a selector, or convert a page to markdown or text.",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {
                        "type": "object",
                        "properties": merge(&open_opts, json!({
                            "url": {"type": "string", "description":
                                "Fetch this first. Give either url or html, not neither."},
                            "html": {"type": "string", "description":
                                "Extract from this markup and fetch nothing."},
                            "selector": {"type": "string", "description":
                                "For kind=css, a CSS selector; the ::text and \
                                 ::attr(name) pseudo-elements work, so \"h1::text\" and \
                                 \"a::attr(href)\" do what they look like. For \
                                 kind=xpath, an XPath 1.0 expression. For kind=markdown \
                                 it narrows the conversion to a subtree, and it is \
                                 ignored for kind=text. Required for css and xpath."},
                            "kind": {"enum": ["css", "xpath", "markdown", "text"], "default": "css",
                                "description":
                                    "css/xpath return one string per match. markdown and \
                                     text return exactly one string for the whole page, \
                                     with scripts, styles and other noise removed."},
                            "render": {"type": "boolean", "default": false, "description":
                                "Fetch the url with a real browser instead of the HTTP \
                                 fetcher. Costs a browser slot."}
                        }))
                    }}}},
                    "responses": {
                        "200": {"description": "The matches. Long results are truncated \
                                                with a visible marker and `truncated: true`."},
                        "400": {"description": "No source, no selector, or a selector that \
                                                does not parse."}
                    }
                }
            },
            "/api/crawl": {
                "post": {
                    "summary": "Crawl from a seed. Always bounded, always robots-respecting.",
                    "description":
                        "max_pages bounds the number of pages actually fetched, including \
                         the seed, and is clamped to the server's own ceiling. Crawling \
                         stays on the seed's host and port unless same_host is false. \
                         robots.txt is always obeyed and is not an option.",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {
                        "type": "object",
                        "required": ["seed"],
                        "properties": merge(&open_opts, json!({
                            "seed": {"type": "string", "description": "Where to start."},
                            "max_pages": {"type": "integer", "default": 10, "description":
                                "Total pages fetched, seed included."},
                            "max_depth": {"type": "integer", "default": 2, "description":
                                "Links deep to follow. 0 fetches the seed alone."},
                            "same_host": {"type": "boolean", "default": true, "description":
                                "Refuse links off the seed's host and port."},
                            "selector": {"type": "string", "description":
                                "A CSS selector run on every page; its matches ride along \
                                 on each result."}
                        }))
                    }}}},
                    "responses": {
                        "200": {"description":
                            "The pages. `stopped_at_limit` says whether the budget rather \
                             than the link graph ended it."}
                    }
                }
            },
            "/api/screenshot": {
                "post": {
                    "summary": "Capture a page as PNG.",
                    "description":
                        "With a session, a url is navigated to first if given and then whatever \
                         page that session is on is captured — so you can browse, read the \
                         result, and shoot the same page without reloading it. Without a \
                         session, a url is required and a throw-away browser lives exactly as \
                         long as the capture.",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {
                        "type": "object",
                        "properties": merge(&open_opts, json!({
                            "url": {"type": "string", "description":
                                "Navigate here first. Omit to shoot the session's current page."},
                            "full_page": {"type": "boolean", "default": false, "description":
                                "Capture the whole scrollable page, not just the viewport. \
                                 A long document makes a correspondingly large image."}
                        }))
                    }}}},
                    "responses": {
                        "200": {"description":
                                    "PNG bytes, at the browser's device scale factor of 2 — \
                                     a default 1280x720 viewport is a 2560x1440 image. Divide \
                                     by two if you need CSS pixels.",
                                "content": {"image/png": {"schema": {"type": "string",
                                                                     "format": "binary"}}}},
                        "400": {"description":
                            "Neither a url nor a session, or a session that has not browsed \
                             anything yet."},
                        "429": {"description": "Every browser slot is in use."}
                    }
                }
            },
            "/api/sessions": {
                "get": {
                    "summary": "Every open session.",
                    "responses": {"200": {"description":
                        "Names, profile directories, egress (proxy credentials redacted), \
                         whether a browser is live, and idle time."}}
                },
                "post": {
                    "summary": "Open a session, or reattach to an existing one.",
                    "description":
                        "Reattaching never wipes the profile — that is what makes a login \
                         survive. The egress fields apply only when the session is \
                         created; to change them, close it and open it again.",
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {
                        "type": "object",
                        "required": ["name"],
                        "properties": {
                            "name": {"type": "string", "description":
                                "The name every later request uses. Reattaching to an \
                                 existing name keeps its profile, its cookies and its \
                                 browser."},
                            "proxy": {"type": "string", "description":
                                "Egress proxy URL for every request on this session."},
                            "impersonate": {"type": "string", "description":
                                "TLS/HTTP2 fingerprint profile for this session's HTTP \
                                 requests, e.g. \"chrome\"."},
                            "user_agent": {"type": "string", "description":
                                "User-Agent for this session, in the browser and over HTTP."}
                        }
                    }}}},
                    "responses": {"201": {"description": "The session."}}
                }
            },
            "/api/sessions/{name}": {
                "delete": {
                    "summary": "Close a session. Its profile directory is kept.",
                    "parameters": [{"name": "name", "in": "path", "required": true,
                                    "schema": {"type": "string"}}],
                    "responses": {
                        "204": {"description": "Closed."},
                        "404": {"description": "No session by that name."}
                    }
                }
            }
        }
    })
}

/// Merge the shared option properties into a route's own.
fn merge(shared: &Value, own: Value) -> Value {
    let mut merged = shared.clone();
    if let (Some(target), Some(source)) = (merged.as_object_mut(), own.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_header_is_parsed_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().expect("valid"));
        assert_eq!(bearer(&headers), Some("abc"));

        headers.insert(header::AUTHORIZATION, "bearer abc".parse().expect("valid"));
        assert_eq!(bearer(&headers), Some("abc"));
    }

    #[test]
    fn anything_that_is_not_a_bearer_header_is_no_token() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);

        headers.insert(header::AUTHORIZATION, "Basic abc".parse().expect("valid"));
        assert_eq!(bearer(&headers), None);

        headers.insert(header::AUTHORIZATION, "Bearer".parse().expect("valid"));
        assert_eq!(bearer(&headers), None);

        headers.insert(header::AUTHORIZATION, "Bearer  ".parse().expect("valid"));
        assert_eq!(bearer(&headers), None);
    }

    #[test]
    fn the_comparison_is_length_aware_and_still_correct() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreu"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(constant_time_eq(b"", b""));
    }

    /// The document is hand-written, so the thing worth asserting is that it is
    /// *complete* — every route in the table above appears in it.
    #[test]
    fn the_openapi_document_describes_every_route() {
        let document = openapi_document();
        let paths = document["paths"].as_object().expect("an object");
        for path in [
            "/api/health",
            "/api/openapi.json",
            "/api/fetch",
            "/api/browse",
            "/api/extract",
            "/api/crawl",
            "/api/screenshot",
            "/api/sessions",
            "/api/sessions/{name}",
        ] {
            assert!(paths.contains_key(path), "{path} is missing");
        }
        assert!(paths["/api/sessions"].get("get").is_some());
        assert!(paths["/api/sessions"].get("post").is_some());
        assert!(paths["/api/health"]["get"]["security"]
            .as_array()
            .expect("an array")
            .is_empty());
    }

    /// `API.md` §3's rule, applied to the REST surface too: an agent picks a
    /// route by reading the descriptions, so a field without one is a field it
    /// will misuse.
    #[test]
    fn every_request_property_carries_a_description() {
        let document = openapi_document();
        let mut checked = 0;
        for (path, methods) in document["paths"].as_object().expect("an object") {
            for (method, operation) in methods.as_object().expect("an object") {
                let Some(schema) = operation.get("requestBody").and_then(|body| {
                    body["content"]["application/json"]["schema"].get("properties")
                }) else {
                    continue;
                };
                for (field, spec) in schema.as_object().expect("an object") {
                    assert!(
                        spec.get("description").is_some(),
                        "{method} {path}: {field} has no description"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 20, "only {checked} fields were checked");
    }

    #[test]
    fn merging_the_shared_options_keeps_both_halves() {
        let shared = json!({"session": {"type": "string", "description": "s"}});
        let merged = merge(
            &shared,
            json!({"url": {"type": "string", "description": "u"}}),
        );
        assert!(merged["session"].is_object());
        assert!(merged["url"].is_object());
    }
}
