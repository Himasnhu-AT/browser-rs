//! The Model Context Protocol surface: the tool catalogue, its JSON schemas,
//! the dispatcher onto [`crate::engine`], and the streamable-HTTP transport
//! mounted at `/mcp`.
//!
//! # Why the tool layer lives here and not in `browser-mcp`
//!
//! There are two transports — the `brs-mcp` stdio binary and this HTTP mount —
//! and API.md §3 requires them to carry **one** tool set. `browser-mcp` already
//! depends on `browser-server`, so the shared half cannot live there without a
//! dependency cycle; and `browser-server` does not depend on `rmcp`. So the
//! parts that are protocol-shaped but SDK-agnostic — schemas, argument parsing,
//! truncation, dispatch — are defined here in plain `serde_json`, and
//! `browser-mcp` adapts them to `rmcp`'s types for stdio. Neither manifest
//! needs to change, and there is exactly one place a tool is described.
//!
//! # Response shape
//!
//! Everything an agent reads comes back as markdown or cleaned text, never raw
//! markup unless it asks for it, and every text payload passes through
//! [`truncate`] so a large page becomes a bounded response with a **visible**
//! marker rather than a silent cut or a megabyte of HTML.

use std::borrow::Cow;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use browser::http::config::HeaderDict;
use browser::{PageLoadState, Proxy, WaitState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::engine::{
    BrowseRequest, ContentFormat, CrawlOutcome, CrawlRequest, Engine,
    ExtractKind as EngineExtractKind, ExtractRequest, ExtractResult, FetchRequest, OpenOpts,
    PageResult, ScreenshotRequest, SessionRequest, Transport,
};
use crate::error::ServerError;
use crate::session::SessionInfo;

// ---------------------------------------------------------------------------
// The tool catalogue
// ---------------------------------------------------------------------------

/// One tool as an agent sees it in `tools/list`.
///
/// This is deliberately a plain struct rather than `rmcp::model::Tool`: it has
/// to be constructible from `browser-server`, which has no MCP SDK dependency.
/// `browser-mcp` converts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    /// The tool name, as called in `tools/call`.
    pub name: &'static str,
    /// What the tool does and — the part agents actually act on — when to
    /// prefer it over its neighbours.
    pub description: &'static str,
    /// A JSON Schema `object` describing the arguments. Every property carries
    /// a `description`; agents choose tools by reading them.
    pub input_schema: Value,
    /// `readOnlyHint`: true when the tool only observes the web. Only the
    /// session tools mutate anything the server owns.
    pub read_only: bool,
    /// `idempotentHint`: true when calling twice with the same arguments is
    /// indistinguishable from calling once.
    pub idempotent: bool,
}

/// Every tool name, in the order [`tool_catalog`] returns them.
///
/// Agents cache the tool list, so this order and these names are part of the
/// public contract; `mcp_tool_list_is_stable` fails if either changes.
pub const TOOL_NAMES: [&str; 8] = [
    "fetch",
    "browse",
    "extract",
    "crawl",
    "screenshot",
    "session_open",
    "session_list",
    "session_close",
];

/// The instructions block returned with `initialize`.
///
/// Clients show this to the model once, ahead of any tool call, so it is the
/// only place to explain the *system* rather than an individual tool: the
/// fetch-versus-browse cost split and the session story.
pub const SERVER_INSTRUCTIONS: &str = "\
browser-rs fetches and reads the web on your behalf.

Cost order, cheapest first — try them in this order:
  1. `fetch`  — plain HTTP, milliseconds. Correct for most articles, docs and APIs.
  2. `extract` — same cost as `fetch`, but returns only the part you asked for.
  3. `browse` — a real Chrome, seconds. Needed for JavaScript-rendered pages,
     pages behind a login, and pages where `fetch` returned a near-empty body
     or a challenge interstitial.
  4. `crawl` / `screenshot` — many page loads, or an image. Bound them tightly.

Sessions: `session_open` gives a name a persistent Chrome profile on the server,
so cookies and logins survive requests *and* server restarts. Pass that same
name to `fetch`, `browse`, `extract`, `crawl` or `screenshot` and the call is
authenticated. A human logs the session in once through the server's dashboard;
you cannot type a password yourself, and you should not ask the user for one —
ask them to log the session in instead.

Every text response is capped and marks its own truncation. If you see a
truncation marker, prefer narrowing with `extract` over raising `max_chars`.";

/// The full tool set, in a stable order.
///
/// Both transports call this; there is no second catalogue anywhere.
pub fn tool_catalog() -> Vec<ToolDescriptor> {
    vec![
        fetch_tool(),
        browse_tool(),
        extract_tool(),
        crawl_tool(),
        screenshot_tool(),
        session_open_tool(),
        session_list_tool(),
        session_close_tool(),
    ]
}

/// Look one tool up by name.
pub fn tool_by_name(name: &str) -> Option<ToolDescriptor> {
    tool_catalog().into_iter().find(|tool| tool.name == name)
}

// -- schema fragments shared by several tools -------------------------------
//
// These exist so that, say, `session` reads identically on all four tools that
// accept one. An agent that has read the word once should not have to re-read a
// paraphrase of it.

/// The `session` property: the same prose everywhere it appears.
fn session_property() -> Value {
    json!({
        "type": "string",
        "description": "Name of a session created by `session_open`. Reuses that \
    session's cookies, logins, proxy and fingerprint, so a site a human logged into \
    once through the dashboard stays authenticated for you. Omit for a stateless \
    request that shares nothing with any other call."
    })
}

/// The `format` property: how a page body is rendered back to the model.
fn format_property() -> Value {
    json!({
        "type": "string",
        "enum": ["markdown", "text", "html"],
        "default": "markdown",
        "description": "How to return the page body. `markdown` (the default) \
    keeps the readable structure — headings, lists, links, tables — and is what you \
    almost always want. `text` is the same content with all markup stripped. `html` \
    is the raw source: ask for it only when you need attributes or scripts, and \
    expect it to hit the truncation cap quickly."
    })
}

/// The `max_chars` property, parameterised by its default.
fn max_chars_property(default: usize) -> Value {
    json!({
        "type": "integer",
        "minimum": 200,
        "maximum": MAX_CHARS_CEILING,
        "default": default,
        "description": format!(
            "Maximum characters of body text to return (default {default}, hard \
    limit {MAX_CHARS_CEILING}). If the page is longer the response is cut here and a \
    visible `[truncated: … ]` marker is appended saying how much was dropped — \
    nothing is ever silently lost. When you hit it, prefer narrowing the page with \
    `extract` over asking for the whole thing again at a larger size."
        )
    })
}

/// The `proxy` property.
fn proxy_property() -> Value {
    json!({
        "type": "string",
        "description": "Egress proxy for this call, e.g. \
    `http://user:pass@host:8080` or `socks5://host:1080`. Defaults to the server's \
    configured proxy or its rotation policy; you rarely need to set it."
    })
}

/// The `impersonate` property.
fn impersonate_property() -> Value {
    json!({
        "type": "string",
        "description": "Browser fingerprint profile to impersonate. It \
    selects the TLS/JA3 signature and the header order, not merely the User-Agent \
    string. Use a family alias — `chrome`, `firefox`, `safari`, `edge`, `tor`, \
    `chrome_android`, `safari_ios` — which tracks the newest profile in that \
    family, or pin an exact one such as `chrome150`, `firefox147` or `safari2601`. \
    Defaults to the server's configured profile, which is normally the right \
    choice."
    })
}

/// The `user_agent` property.
fn user_agent_property() -> Value {
    json!({
        "type": "string",
        "description": "Override the User-Agent header only. Prefer \
    `impersonate`, which changes the whole fingerprint consistently; a User-Agent \
    that disagrees with the TLS signature is more conspicuous than either alone."
    })
}

/// The `timeout_ms` property, parameterised by its default.
fn timeout_property(default: u64) -> Value {
    json!({
        "type": "integer",
        "minimum": 100,
        "default": default,
        "description": format!("How long to wait for the page, in milliseconds \
    (default {default}).")
    })
}

/// Assemble an `object` schema. Keeps property order deterministic, which
/// matters because the catalogue is golden-tested.
fn object_schema(properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut map = Map::new();
    for (name, schema) in properties {
        map.insert(name.to_owned(), schema);
    }
    json!({
        "type": "object",
        "properties": Value::Object(map),
        "required": required,
        "additionalProperties": false,
    })
}

// -- the eight tools --------------------------------------------------------

fn fetch_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "fetch",
        description: "\
Fetch one URL over plain HTTP and return it as markdown.

This is the fast path: no browser process is started, so it costs milliseconds \
rather than seconds. Prefer it for articles, documentation, JSON and XML APIs, \
RSS feeds, and any page whose content is already present in the HTML source. \
Requests are sent with a real browser's TLS fingerprint and header order, so \
ordinary bot filtering does not see a naive HTTP client — but no JavaScript runs.

Use `browse` instead when the page builds its content client-side, sits behind \
a login, or when `fetch` returns a near-empty body or a challenge interstitial: \
those are exactly the cases a browser is needed for.

Pass `session` to reuse a logged-in session's cookies here too — a login does \
not oblige you to pay for a browser on every later request.",
        input_schema: object_schema(
            vec![
                (
                    "url",
                    json!({
                        "type": "string",
                        "description": "Absolute URL to fetch, scheme included, e.g. \
                    `https://example.com/pricing`. Relative URLs are rejected."
                    }),
                ),
                ("session", session_property()),
                ("format", format_property()),
                (
                    "headers",
                    json!({
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Extra request headers merged on top of the \
                    impersonation profile's own, e.g. `{\"Accept-Language\": \"en-GB\"}`. Do not set \
                    User-Agent here; use `impersonate` or `user_agent`."
                    }),
                ),
                ("impersonate", impersonate_property()),
                ("user_agent", user_agent_property()),
                ("proxy", proxy_property()),
                ("timeout_ms", timeout_property(DEFAULT_HTTP_TIMEOUT_MS)),
                ("max_chars", max_chars_property(DEFAULT_MAX_CHARS)),
            ],
            &["url"],
        ),
        read_only: true,
        idempotent: true,
    }
}

fn browse_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "browse",
        description: "\
Load one URL in a real headless Chrome, run its JavaScript, and return the \
*rendered* page as markdown.

Much more expensive than `fetch` — a browser process, a network-bound page load \
and script execution, typically seconds — so reach for it only when `fetch` \
cannot work:
  * the content is rendered client-side (single-page apps, infinite scroll, \
dashboards, anything whose HTML source is a near-empty shell),
  * the page is behind a login — pair it with `session`,
  * `fetch` came back with an interstitial or challenge page.

`wait_until` decides when the page counts as ready. `wait_for_selector` is the \
reliable choice whenever you know which element you are waiting for; \
`networkidle` is the fallback for pages that stream content in and never settle \
on a single element.",
        input_schema: object_schema(
            vec![
                (
                    "url",
                    json!({
                        "type": "string",
                        "description": "Absolute URL to load, scheme included, \
                    e.g. `https://app.example.com/dashboard`. Only `http` and `https` are accepted."
                    }),
                ),
                ("session", session_property()),
                (
                    "wait_until",
                    json!({
                        "type": "string",
                        "enum": ["domcontentloaded", "load", "networkidle"],
                        "default": "load",
                        "description": "When the page counts as ready. \
                    `domcontentloaded` is earliest and cheapest — the HTML is parsed, but images and \
                    late scripts may not have run. `load` (the default) waits for subresources. \
                    `networkidle` waits for half a second of network silence, which is what slow \
                    client-rendered apps need, and is the slowest of the three."
                    }),
                ),
                (
                    "wait_for_selector",
                    json!({
                        "type": "string",
                        "description": "CSS selector that must appear before the \
                    page is read, e.g. `.results-list li` or `#app [data-loaded]`. The most reliable \
                    way to wait for a client-rendered page: it returns as soon as the content you \
                    care about exists, instead of guessing at a timing heuristic. Applied in \
                    addition to `wait_until`."
                    }),
                ),
                (
                    "wait_for_state",
                    json!({
                        "type": "string",
                        "enum": ["attached", "detached", "hidden", "visible"],
                        "default": "attached",
                        "description": "Which state `wait_for_selector` must \
                    reach. `attached` (the default) means merely present in the DOM. Use `visible` \
                    when a site renders an empty container early and fills it later, because \
                    `attached` is already satisfied by the empty one. `detached` waits for something \
                    to go away, which is how you wait out a loading spinner."
                    }),
                ),
                ("format", format_property()),
                ("impersonate", impersonate_property()),
                ("user_agent", user_agent_property()),
                ("proxy", proxy_property()),
                ("timeout_ms", timeout_property(DEFAULT_BROWSER_TIMEOUT_MS)),
                ("max_chars", max_chars_property(DEFAULT_MAX_CHARS)),
            ],
            &["url"],
        ),
        read_only: true,
        idempotent: false,
    }
}

fn extract_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "extract",
        description: "\
Pull one specific part out of a page instead of returning all of it.

Reach for this the moment you know what you are looking for. On the HTTP path it \
costs the same as `fetch` but returns a fraction as much, so it is also the \
answer to a truncated response — narrow the query rather than raising the cap.

Give it exactly one source: `url`, which is fetched for you, or `html`, a \
document you already hold. Set `render: true` to load `url` in a real browser \
first, which is the `browse` cost but saves a round trip when the page is \
JavaScript-rendered and you already know the selector.

`kind` decides what comes back:
  * `text` — the readable page text, boilerplate stripped. The best default \
when you simply want to read a page.
  * `markdown` — the page, or the `selector` subtree, as structured markdown.
  * `css` — one result per element matching the CSS `selector`.
  * `xpath` — the result of an XPath 1.0 expression, which is how you reach an \
attribute rather than an element.",
        input_schema: object_schema(
            vec![
                (
                    "kind",
                    json!({
                        "type": "string",
                        "enum": ["css", "xpath", "markdown", "text"],
                        "default": "css",
                        "description": "What to extract. `text` = the readable \
                    page text, boilerplate stripped. `markdown` = structured markdown. `css` = one \
                    result per element matching `selector`. `xpath` = the result of the XPath \
                    expression in `selector`. `css` and `xpath` require `selector`; for `markdown` \
                    it optionally narrows the conversion, and for `text` it is ignored."
                    }),
                ),
                (
                    "selector",
                    json!({
                        "type": "string",
                        "description": "For `kind: css`, a CSS selector — \
                    `article h2`, `.price`, `a[href^=\"/docs\"]`, `li:nth-child(2)`. Two \
                    pseudo-elements are supported and are usually what you want: `h1::text` returns \
                    the text rather than the markup, and `a::attr(href)` returns an attribute. For \
                    `kind: xpath`, an XPath 1.0 expression — `//div[@id=\"main\"]//a/@href`, \
                    `//table[2]//tr/td[1]/text()`. For `markdown`, an optional CSS selector naming \
                    the subtree to convert, which is the cheapest way to skip a site's chrome."
                    }),
                ),
                (
                    "url",
                    json!({
                        "type": "string",
                        "description": "Absolute URL to fetch and extract from. \
                    Mutually exclusive with `html`. Fetched over plain HTTP unless you also pass \
                    `render: true`."
                    }),
                ),
                (
                    "html",
                    json!({
                        "type": "string",
                        "description": "An HTML document to extract from \
                    directly, when you already hold the markup and want no network request at all. \
                    Mutually exclusive with `url`."
                    }),
                ),
                (
                    "render",
                    json!({
                        "type": "boolean",
                        "default": false,
                        "description": "Load `url` in a real browser before \
                    extracting, instead of fetching it over plain HTTP (default false). Costs what \
                    `browse` costs; set it only when the content you are selecting is rendered by \
                    JavaScript."
                    }),
                ),
                ("session", session_property()),
                ("impersonate", impersonate_property()),
                ("user_agent", user_agent_property()),
                ("proxy", proxy_property()),
                (
                    "max_matches",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": DEFAULT_MAX_MATCHES,
                        "description": format!("For `css` and `xpath`, the \
                    maximum number of matches to return (default {DEFAULT_MAX_MATCHES}). Extra \
                    matches are dropped, and the response says how many there were in total, so a \
                    cut list never looks like a complete one.")
                    }),
                ),
                ("max_chars", max_chars_property(DEFAULT_MAX_CHARS)),
            ],
            &["kind"],
        ),
        read_only: true,
        idempotent: true,
    }
}

fn crawl_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "crawl",
        description: "\
Walk a site from a seed URL, following links, and return an **index** of the \
pages it visited: URL, depth, HTTP status, title, and — if you give a \
`selector` — that selector's matches on each page.

It returns the index, not the page bodies. That is the point: one call maps a \
site cheaply, and you then `fetch` or `extract` only the handful of pages worth \
reading. Use it when you do not yet know the URLs; if you do know one, call \
`fetch`.

Always bounded. It stops at whichever of `max_pages` and `max_depth` it reaches \
first, stays on the seed's host unless you say otherwise, and honours \
`robots.txt`. Every page is a real request against someone else's server, so \
raise the limits deliberately and only as far as you need. The response says \
whether the budget, rather than the link graph, is what ended the crawl.",
        input_schema: object_schema(
            vec![
                (
                    "seed",
                    json!({
                        "type": "string",
                        "description": "Absolute URL to start from, scheme \
                    included. Its host and port are the same-host boundary when `same_host` is true."
                    }),
                ),
                (
                    "max_pages",
                    json!({
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_PAGES_CEILING,
                        "default": DEFAULT_MAX_PAGES,
                        "description": format!("Hard cap on pages fetched, the \
                    seed included (default {DEFAULT_MAX_PAGES}). The server applies its own ceiling \
                    on top and reports the budget it actually ran under. This is a budget against \
                    someone else's server: start small and raise it only if the first crawl was \
                    genuinely cut short.")
                    }),
                ),
                (
                    "max_depth",
                    json!({
                        "type": "integer",
                        "minimum": 0,
                        "maximum": MAX_DEPTH_CEILING,
                        "default": DEFAULT_MAX_DEPTH,
                        "description": format!("How many links deep to follow \
                    (default {DEFAULT_MAX_DEPTH}). Depth 0 fetches the seed alone; depth 1 adds the \
                    pages it links to.")
                    }),
                ),
                (
                    "same_host",
                    json!({
                        "type": "boolean",
                        "default": true,
                        "description": "Stay on the seed's host (default true). \
                    Set it false only when the content you want genuinely spans hosts — a crawl that \
                    leaves its host tends to spend its whole page budget on somebody's footer links."
                    }),
                ),
                (
                    "selector",
                    json!({
                        "type": "string",
                        "description": "A CSS selector run against every page \
                    crawled; its matches ride along on that page's row in the index. This is what \
                    turns a crawl into a scrape — `.price` over a catalogue, `h1::text` over a \
                    documentation set — in one call instead of one `extract` per page."
                    }),
                ),
                ("session", session_property()),
                ("impersonate", impersonate_property()),
                ("user_agent", user_agent_property()),
                ("proxy", proxy_property()),
                ("max_chars", max_chars_property(DEFAULT_CRAWL_MAX_CHARS)),
            ],
            &["seed"],
        ),
        read_only: true,
        idempotent: false,
    }
}

fn screenshot_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "screenshot",
        description: "\
Take a PNG screenshot and return it as an image you can look at.

Two ways to aim it, and they cost very different amounts:
  * `session` with **no** `url` — captures whatever that session currently \
shows, without re-navigating. The cheap path, and the right one straight after \
a login or a `browse`. The session must have browsed something already.
  * `url` — launches a browser, navigates, captures, closes. Pass `session` too \
and the navigation happens in that session instead, keeping its cookies.

Use it when the rendering *is* the question: is the page laid out correctly, \
what does this chart show, did the form submit, is a cookie banner covering the \
content. Do not use it to read text — `browse` and `extract` return text far \
more cheaply and far more accurately than reading it off an image.

Two things about the image itself. It is captured at a device scale factor of \
**2**, so a 1280×720 viewport comes back as a 2560×1440 PNG: **halve the pixel \
coordinates you read off it** to get CSS pixels. And `full_page` multiplies that \
by the length of the document, so a long article can exceed the size limit and \
come back as an error rather than an image — prefer the viewport.",
        input_schema: object_schema(
            vec![
                (
                    "url",
                    json!({
                        "type": "string",
                        "description": "Absolute URL to load and capture. Omit \
                    it, and pass `session`, to capture that session's current page with no fresh \
                    navigation — which is both faster and the only way to photograph a page you \
                    reached by browsing."
                    }),
                ),
                ("session", session_property()),
                (
                    "full_page",
                    json!({
                        "type": "boolean",
                        "default": false,
                        "description": format!("Capture the entire scrollable \
                    document rather than just the viewport (default false). Remember the 2× scale \
                    factor: a 5000 px-tall page becomes a 10000 px-tall PNG, and anything over \
                    {} MiB is refused rather than returned. Prefer the viewport unless you \
                    specifically need what is below the fold.", MAX_SCREENSHOT_BYTES / (1024 * 1024))
                    }),
                ),
                ("impersonate", impersonate_property()),
                ("user_agent", user_agent_property()),
                ("proxy", proxy_property()),
            ],
            &[],
        ),
        read_only: true,
        idempotent: false,
    }
}

fn session_open_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "session_open",
        description: "\
Create — or reattach to — a named browser session.

A session is a name plus a persistent Chrome profile directory on the server, \
so its cookies, local storage and logins survive individual requests *and* a \
restart of the server. Reattaching to a name that already exists never wipes \
the profile.

The intended workflow: you open a session by name, a human logs the site in \
once through the server's dashboard, and every later `fetch`, `browse`, \
`extract`, `crawl` or `screenshot` carrying that name is authenticated. You cannot type a \
password yourself and should not ask the user for one — ask them to log the \
session in.

Live browsers are capped in number and sessions time out when idle, so close \
the ones you are finished with.",
        input_schema: object_schema(
            vec![
                (
                    "name",
                    json!({
                        "type": "string",
                        "minLength": 1,
                        "description": "Stable, human-readable name for the \
                    session, e.g. `github` or `shop-eu`. The name *is* the identity: reusing it \
                    reattaches to the same profile and the same login. Check `session_list` first — \
                    the session you need may already exist and already be logged in."
                    }),
                ),
                ("impersonate", impersonate_property()),
                ("user_agent", user_agent_property()),
                ("proxy", proxy_property()),
            ],
            &["name"],
        ),
        read_only: false,
        idempotent: true,
    }
}

fn session_list_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "session_list",
        description: "\
List the named sessions this server holds: for each, whether a Chrome process \
is live right now, how long it has been idle, how many operations have run on \
it, and which proxy and fingerprint it uses.

Call it before `session_open` — the login you need may already exist, and \
reusing a session is free where logging one in requires a human.",
        input_schema: object_schema(vec![], &[]),
        read_only: true,
        idempotent: true,
    }
}

fn session_close_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "session_close",
        description: "\
Close a named session, shutting down its browser if one is running.

The persistent profile directory is **kept**: that profile is what holds the \
login, so `session_open` with the same name later reattaches still \
authenticated. Closing frees a live-browser slot, nothing more.",
        input_schema: object_schema(
            vec![(
                "name",
                json!({
                    "type": "string",
                    "minLength": 1,
                    "description": "Name of the session to close, as given to \
                `session_open` and as reported by `session_list`."
                }),
            )],
            &["name"],
        ),
        read_only: false,
        idempotent: true,
    }
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Default cap on the characters of body text in one tool response.
///
/// Sized so a typical documentation page arrives whole while a large listing
/// page arrives truncated: roughly 5k tokens, which is a fraction of any modern
/// context window and cheap enough to spend on a page that turns out to be the
/// wrong one.
pub const DEFAULT_MAX_CHARS: usize = 20_000;

/// The largest `max_chars` a caller may ask for.
///
/// A hard ceiling rather than a default, because "the model asked for it" is
/// not a reason to put a megabyte of markup through a context window; a caller
/// that needs more than this wants `extract`, not a bigger number.
pub const MAX_CHARS_CEILING: usize = 200_000;

/// Default total cap for a `crawl`, which sums many pages.
pub const DEFAULT_CRAWL_MAX_CHARS: usize = 40_000;

/// Default per-page cap inside a `crawl`: an excerpt, not the page.
pub const DEFAULT_CRAWL_CHARS_PER_PAGE: usize = 2_000;

/// Default `max_pages` for a crawl.
pub const DEFAULT_MAX_PAGES: usize = 20;

/// The largest `max_pages` a caller may ask for.
pub const MAX_PAGES_CEILING: usize = 500;

/// Default `max_depth` for a crawl.
pub const DEFAULT_MAX_DEPTH: usize = 2;

/// The largest `max_depth` a caller may ask for.
pub const MAX_DEPTH_CEILING: usize = 10;

/// Default cap on `css`/`xpath` matches returned by `extract`.
pub const DEFAULT_MAX_MATCHES: usize = 100;

/// Default HTTP timeout, in milliseconds.
pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Default browser timeout, in milliseconds.
pub const DEFAULT_BROWSER_TIMEOUT_MS: u64 = 45_000;

/// The largest PNG this layer will hand back to a model.
///
/// The engine truncates text and does **not** truncate image bytes, so this is
/// the only thing standing between `full_page: true` on a long document and a
/// multi-megabyte base64 blob in a context window. Chrome captures at a device
/// scale factor of 2, which quadruples the pixel count before compression, so
/// the ceiling is reached sooner than the CSS dimensions suggest.
///
/// 4 MiB of PNG is roughly 5.5 MiB once base64-encoded — already far more than
/// any model needs to answer "what does this page look like", and the refusal
/// names the remedy.
pub const MAX_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// The outcome of applying a character cap to a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    /// The text to hand back, marker included when it was cut.
    pub text: String,
    /// Characters in the original.
    pub total_chars: usize,
    /// Characters of the original kept — excludes the marker.
    pub kept_chars: usize,
}

impl Truncated {
    /// Whether anything was dropped.
    pub fn was_truncated(&self) -> bool {
        self.kept_chars < self.total_chars
    }
}

/// Cap `body` at `max_chars` characters, appending a marker the model can see.
///
/// The marker is the whole point. A silently shortened page reads to a model
/// exactly like a page that ends there, and it will confidently answer from the
/// half it was given; a marker turns that into a decision it can make — narrow
/// the query, or ask for more.
///
/// Counting is in `char`s, not bytes, so the cut never lands inside a UTF-8
/// sequence and a page of CJK is not silently cut to a third of its length.
pub fn truncate(body: &str, max_chars: usize) -> Truncated {
    let total_chars = body.chars().count();
    if total_chars <= max_chars {
        return Truncated {
            text: body.to_owned(),
            total_chars,
            kept_chars: total_chars,
        };
    }

    // `char_indices` gives the byte offset of the first character we drop.
    let cut = body
        .char_indices()
        .nth(max_chars)
        .map(|(offset, _)| offset)
        .unwrap_or(body.len());
    let mut text = body[..cut].to_owned();
    text.push_str(&format!(
        "\n\n[truncated: {max_chars} of {total_chars} characters shown, \
{} dropped. Narrow the page with `extract` (a CSS or XPath selector, or \
kind=\"text\"), or re-run with a larger `max_chars`.]",
        total_chars - max_chars
    ));

    Truncated {
        text,
        total_chars,
        kept_chars: max_chars,
    }
}

/// Clamp a caller-supplied cap into the range the schema advertises.
///
/// Schemas are documentation, not enforcement: plenty of clients send whatever
/// the model produced. Clamping rather than rejecting keeps a slightly wrong
/// number from costing the agent a whole round trip.
pub fn clamp_max_chars(requested: Option<usize>, default: usize) -> usize {
    requested.unwrap_or(default).clamp(1, MAX_CHARS_CEILING)
}

// ---------------------------------------------------------------------------
// Tool results and errors
// ---------------------------------------------------------------------------

/// One block of a tool result.
///
/// SDK-agnostic, for the same reason [`ToolDescriptor`] is: `browser-mcp` maps
/// it onto `rmcp::model::ContentBlock`, and the HTTP transport below serialises
/// it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolContent {
    /// Markdown or plain text for the model to read.
    Text(String),
    /// A base64 image, as `screenshot` returns.
    Image {
        /// Base64-encoded bytes, unwrapped — no `data:` prefix.
        base64: String,
        /// The MIME type, e.g. `image/png`.
        mime_type: &'static str,
    },
}

impl ToolContent {
    /// Serialise to the MCP `ContentBlock` wire shape.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => json!({ "type": "text", "text": text }),
            Self::Image { base64, mime_type } => json!({
                "type": "image",
                "data": base64,
                "mimeType": mime_type,
            }),
        }
    }
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolOutput {
    /// The blocks the model reads.
    pub content: Vec<ToolContent>,
    /// A machine-readable mirror, for clients that use `structuredContent`.
    pub structured: Option<Value>,
}

impl ToolOutput {
    /// A result that is one text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            structured: None,
        }
    }

    /// Attach the structured mirror.
    pub fn with_structured(mut self, structured: Value) -> Self {
        self.structured = Some(structured);
        self
    }
}

/// Why a tool call failed.
///
/// The split matters: MCP renders a *protocol* error opaquely, so the caller
/// never reads the message, whereas a *tool* error is shown to the model and it
/// can act on it. Almost everything here is a tool error for that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// The tool name is not one we serve. This one really is a protocol error.
    UnknownTool,
    /// The arguments did not make sense — a missing field, two mutually
    /// exclusive sources, an unparseable enum.
    InvalidArguments,
    /// The operation ran and failed: a DNS failure, a 500, a browser timeout.
    Execution,
}

/// A tool failure with a message written for the model, not for a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    /// Which sort of failure.
    pub kind: ToolErrorKind,
    /// What went wrong and, where there is one, what to do instead.
    pub message: String,
}

impl ToolError {
    /// An `InvalidArguments` failure.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::InvalidArguments,
            message: message.into(),
        }
    }

    /// An `Execution` failure.
    pub fn execution(message: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::Execution,
            message: message.into(),
        }
    }

    /// An `UnknownTool` failure, listing what we do serve.
    pub fn unknown_tool(name: &str) -> Self {
        Self {
            kind: ToolErrorKind::UnknownTool,
            message: format!(
                "unknown tool {name:?}; this server provides: {}",
                TOOL_NAMES.join(", ")
            ),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<ServerError> for ToolError {
    /// Carry the engine's classification across into MCP.
    ///
    /// The kind tag is prefixed onto the message rather than dropped: an agent
    /// reading "upstream: 503 from …" knows to retry, where "503 from …" alone
    /// reads like something it should fix in its arguments. `Internal` is
    /// redacted by [`ServerError::public_message`] before it gets here, so no
    /// filesystem path reaches the model.
    fn from(error: ServerError) -> Self {
        let kind = match &error {
            ServerError::Unauthorized | ServerError::BadRequest(_) | ServerError::NotFound(_) => {
                ToolErrorKind::InvalidArguments
            }
            _ => ToolErrorKind::Execution,
        };
        Self {
            kind,
            message: format!("{}: {}", error.kind(), error.public_message()),
        }
    }
}

// ---------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------

/// How a page body should be rendered back to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyFormat {
    /// Structured markdown. The default, and what an LLM reads best.
    #[default]
    Markdown,
    /// Readable text with all markup stripped.
    Text,
    /// The raw source. Only when the caller explicitly asks.
    Html,
}

/// When a browser page counts as ready.
///
/// A deliberate subset of [`crate::engine`]'s `PageLoadState`: `commit` is also
/// available there, but it means "navigation started", which is never what an
/// agent reading a page wants, and a fourth option it should never choose is a
/// fourth way for it to choose wrongly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitUntil {
    /// `load`: subresources fetched.
    #[default]
    Load,
    /// `domcontentloaded`: HTML parsed, nothing more.
    #[serde(rename = "domcontentloaded")]
    DomContentLoaded,
    /// `networkidle`: the network has been quiet for half a second.
    #[serde(rename = "networkidle")]
    NetworkIdle,
}

/// The state `wait_for_selector` must reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitForState {
    /// Present in the DOM, however it is styled.
    #[default]
    Attached,
    /// Gone from the DOM — how you wait out a spinner.
    Detached,
    /// In the DOM but not rendered.
    Hidden,
    /// In the DOM and rendered.
    Visible,
}

/// What `extract` should pull out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractKind {
    /// A CSS selector over the document.
    #[default]
    Css,
    /// An XPath 1.0 expression over the document.
    Xpath,
    /// The document, or a subtree of it, as markdown.
    Markdown,
    /// The readable page text, boilerplate removed.
    Text,
}

impl ExtractKind {
    /// The spelling the schema advertises, for use in messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Css => "css",
            Self::Xpath => "xpath",
            Self::Markdown => "markdown",
            Self::Text => "text",
        }
    }
}

/// Arguments to `fetch`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchArgs {
    /// The URL to fetch.
    pub url: String,
    /// Session to borrow cookies and identity from.
    #[serde(default)]
    pub session: Option<String>,
    /// How to render the body.
    #[serde(default)]
    pub format: BodyFormat,
    /// Extra request headers.
    #[serde(default)]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    /// Fingerprint profile.
    #[serde(default)]
    pub impersonate: Option<String>,
    /// User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Egress proxy.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Response cap.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// Arguments to `browse`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseArgs {
    /// The URL to load.
    pub url: String,
    /// Session to load the page in.
    #[serde(default)]
    pub session: Option<String>,
    /// Readiness condition.
    #[serde(default)]
    pub wait_until: WaitUntil,
    /// Selector that must appear before the DOM is read.
    #[serde(default)]
    pub wait_for_selector: Option<String>,
    /// The state that selector must reach.
    #[serde(default)]
    pub wait_for_state: WaitForState,
    /// How to render the body.
    #[serde(default)]
    pub format: BodyFormat,
    /// Fingerprint profile.
    #[serde(default)]
    pub impersonate: Option<String>,
    /// User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Egress proxy.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Response cap.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// Arguments to `extract`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractArgs {
    /// What to pull out.
    #[serde(default)]
    pub kind: ExtractKind,
    /// The CSS selector or XPath expression.
    #[serde(default)]
    pub selector: Option<String>,
    /// Fetch this URL first.
    #[serde(default)]
    pub url: Option<String>,
    /// Extract from markup supplied inline.
    #[serde(default)]
    pub html: Option<String>,
    /// Load `url` in a browser instead of fetching it.
    #[serde(default)]
    pub render: bool,
    /// Session whose cookies and identity to use.
    #[serde(default)]
    pub session: Option<String>,
    /// Fingerprint profile.
    #[serde(default)]
    pub impersonate: Option<String>,
    /// User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Egress proxy.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Cap on `css`/`xpath` matches.
    #[serde(default)]
    pub max_matches: Option<usize>,
    /// Response cap.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// Arguments to `crawl`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrawlArgs {
    /// Where to start.
    pub seed: String,
    /// Page budget.
    #[serde(default)]
    pub max_pages: Option<usize>,
    /// Link depth.
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// Stay on the seed's host.
    #[serde(default)]
    pub same_host: Option<bool>,
    /// A selector run on every page, whose matches ride along in the index.
    #[serde(default)]
    pub selector: Option<String>,
    /// Session to crawl as.
    #[serde(default)]
    pub session: Option<String>,
    /// Fingerprint profile.
    #[serde(default)]
    pub impersonate: Option<String>,
    /// User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Egress proxy.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Response cap for the whole index.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// Arguments to `screenshot`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotArgs {
    /// URL to load and capture.
    #[serde(default)]
    pub url: Option<String>,
    /// Session whose current page to capture.
    #[serde(default)]
    pub session: Option<String>,
    /// Capture past the viewport.
    #[serde(default)]
    pub full_page: bool,
    /// Fingerprint profile.
    #[serde(default)]
    pub impersonate: Option<String>,
    /// User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Egress proxy.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// Arguments to `session_open`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpenArgs {
    /// The session name, which is its identity.
    pub name: String,
    /// Fingerprint profile.
    #[serde(default)]
    pub impersonate: Option<String>,
    /// User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Egress proxy.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// Arguments to `session_close`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCloseArgs {
    /// The session to close.
    pub name: String,
}

/// Parse a tool's arguments, turning serde's complaint into a message the model
/// can act on.
///
/// `deny_unknown_fields` is deliberate: a model that invented a parameter has
/// misread the schema, and telling it so is more useful than quietly ignoring
/// the field and returning something that does not do what it asked.
pub fn parse_args<T: serde::de::DeserializeOwned>(tool: &str, args: Value) -> Result<T, ToolError> {
    // A client that sends no arguments at all sends `null`; the tools with only
    // optional fields must still work.
    let args = if args.is_null() {
        Value::Object(Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|error| {
        ToolError::invalid(format!(
            "`{tool}` arguments are not valid: {error}. Re-read the tool's input \
             schema and try again."
        ))
    })
}

/// Reject a request that named more than one source, or none.
///
/// `extract` and `screenshot` both accept several mutually exclusive sources,
/// and "you gave me two" is a much better message than picking one by
/// precedence and returning a page the caller did not ask for.
pub fn exactly_one_source(
    tool: &str,
    sources: &[(&'static str, bool)],
) -> Result<&'static str, ToolError> {
    let given: Vec<&'static str> = sources
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| *name)
        .collect();
    let names: Vec<&'static str> = sources.iter().map(|(name, _)| *name).collect();
    match given.as_slice() {
        [only] => Ok(only),
        [] => Err(ToolError::invalid(format!(
            "`{tool}` needs exactly one source: give one of {}.",
            names.join(", ")
        ))),
        _ => Err(ToolError::invalid(format!(
            "`{tool}` takes exactly one source but got {}. Pass only one of {}.",
            given.join(" and "),
            names.join(", ")
        ))),
    }
}

/// Render a label for a status line, used by the result formatters.
fn or_dash(value: Option<&str>) -> Cow<'_, str> {
    match value {
        Some(text) if !text.is_empty() => Cow::Borrowed(text),
        _ => Cow::Borrowed("—"),
    }
}

// ---------------------------------------------------------------------------
// The engine-backed invoker
// ---------------------------------------------------------------------------

/// The [`ToolInvoker`] that actually does the work: parse, call [`Engine`],
/// format.
///
/// This is the whole of the MCP layer's behaviour, and it is deliberately
/// small. Nothing here fetches, renders, selects or crawls; `AGENTS.md` is
/// explicit that every operation lives in [`crate::engine`] so that `/api` and
/// `/mcp` cannot drift, and the only thing this type adds is the shape an LLM
/// reads best — markdown with a short header, bounded, and honest about what it
/// left out.
pub struct EngineTools {
    engine: Arc<Engine>,
}

impl EngineTools {
    /// Wrap an engine.
    #[must_use]
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    /// The same thing, ready to hand to [`router`] or to `browser-mcp`.
    #[must_use]
    pub fn shared(engine: Arc<Engine>) -> Arc<dyn ToolInvoker> {
        Arc::new(Self::new(engine))
    }

    /// The engine underneath.
    #[must_use]
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Assemble the options every operation shares.
    ///
    /// `headless` comes from the server's configuration rather than from the
    /// request: whether a human can watch a browser is an operator's decision
    /// about their own machine, not something an agent should be able to flip.
    fn opts(
        &self,
        session: Option<String>,
        impersonate: Option<String>,
        user_agent: Option<String>,
        proxy: Option<String>,
    ) -> OpenOpts {
        OpenOpts {
            session,
            proxy: proxy.map(Proxy::Url),
            impersonate,
            user_agent,
            headless: self.engine.config().headless,
        }
    }

    async fn run_fetch(&self, args: FetchArgs) -> Result<ToolOutput, ToolError> {
        let max_chars = clamp_max_chars(args.max_chars, DEFAULT_MAX_CHARS);
        let request = FetchRequest {
            url: args.url,
            opts: self.opts(args.session, args.impersonate, args.user_agent, args.proxy),
            headers: args
                .headers
                .map(|headers| headers.into_iter().collect::<HeaderDict>()),
            format: args.format.into(),
            timeout_ms: args.timeout_ms,
        };
        let page = self.engine.fetch(&request).await?;
        Ok(ToolOutput::text(page_text(&page, max_chars)))
    }

    async fn run_browse(&self, args: BrowseArgs) -> Result<ToolOutput, ToolError> {
        let max_chars = clamp_max_chars(args.max_chars, DEFAULT_MAX_CHARS);
        let request = BrowseRequest {
            url: args.url,
            opts: self.opts(args.session, args.impersonate, args.user_agent, args.proxy),
            wait_until: Some(args.wait_until.into()),
            // `wait_for_state` is meaningless without a selector, and sending it
            // anyway would make the engine wait on nothing.
            wait_for_state: args
                .wait_for_selector
                .is_some()
                .then(|| args.wait_for_state.into()),
            wait_for_selector: args.wait_for_selector,
            timeout_ms: args.timeout_ms,
            format: args.format.into(),
        };
        let page = self.engine.browse(&request).await?;
        Ok(ToolOutput::text(page_text(&page, max_chars)))
    }

    async fn run_extract(&self, args: ExtractArgs) -> Result<ToolOutput, ToolError> {
        let max_chars = clamp_max_chars(args.max_chars, DEFAULT_MAX_CHARS);
        let max_matches = args
            .max_matches
            .unwrap_or(DEFAULT_MAX_MATCHES)
            .clamp(1, 1_000);
        exactly_one_source(
            "extract",
            &[("url", args.url.is_some()), ("html", args.html.is_some())],
        )?;
        // Caught here rather than in the engine so the message can name the
        // alternative: a model that reached for `css` without a selector nearly
        // always wanted to read the page, and `kind: "text"` is that.
        if matches!(args.kind, ExtractKind::Css | ExtractKind::Xpath) && args.selector.is_none() {
            return Err(ToolError::invalid(format!(
                "`extract` with kind=\"{}\" needs a `selector`. To read the \
                 whole page instead, use kind=\"text\" (readable text) or \
                 kind=\"markdown\" (structure kept).",
                args.kind.as_str()
            )));
        }

        let request = ExtractRequest {
            url: args.url,
            html: args.html,
            opts: self.opts(args.session, args.impersonate, args.user_agent, args.proxy),
            selector: args.selector,
            kind: args.kind.into(),
            render: args.render,
        };
        let result = self.engine.extract(&request).await?;
        Ok(ToolOutput::text(extract_text(
            &result,
            max_matches,
            max_chars,
        )))
    }

    async fn run_crawl(&self, args: CrawlArgs) -> Result<ToolOutput, ToolError> {
        let max_chars = clamp_max_chars(args.max_chars, DEFAULT_CRAWL_MAX_CHARS);
        let request = CrawlRequest {
            seed: args.seed,
            opts: self.opts(args.session, args.impersonate, args.user_agent, args.proxy),
            max_pages: args
                .max_pages
                .unwrap_or(DEFAULT_MAX_PAGES)
                .clamp(1, MAX_PAGES_CEILING),
            max_depth: args
                .max_depth
                .unwrap_or(DEFAULT_MAX_DEPTH)
                .min(MAX_DEPTH_CEILING),
            same_host: args.same_host.unwrap_or(true),
            selector: args.selector,
        };
        let selector = request.selector.clone();
        let outcome = self.engine.crawl(&request).await?;
        Ok(ToolOutput::text(crawl_text(
            &outcome,
            selector.as_deref(),
            max_chars,
        )))
    }

    async fn run_screenshot(&self, args: ScreenshotArgs) -> Result<ToolOutput, ToolError> {
        if args.url.is_none() && args.session.is_none() {
            return Err(ToolError::invalid(
                "`screenshot` needs a `url` to load, or a `session` whose \
                 current page should be captured.",
            ));
        }
        let source = args
            .url
            .clone()
            .or_else(|| {
                args.session
                    .clone()
                    .map(|name| format!("session `{name}`'s current page"))
            })
            .unwrap_or_default();
        let request = ScreenshotRequest {
            url: args.url,
            opts: self.opts(args.session, args.impersonate, args.user_agent, args.proxy),
            full_page: args.full_page,
        };
        let png = self.engine.screenshot(&request).await?;
        screenshot_output(&source, &png, request.full_page)
    }

    async fn run_session_open(&self, args: SessionOpenArgs) -> Result<ToolOutput, ToolError> {
        let request = SessionRequest {
            name: args.name,
            proxy: args.proxy.map(Proxy::Url),
            impersonate: args.impersonate,
            user_agent: args.user_agent,
        };
        let info = self.engine.session_open(&request).await?;
        let text = format!(
            "Session `{}` is open.\n\n{}\n\nCookies and logins live in its \
             profile directory and survive a restart. If the site needs a login, \
             ask the operator to log in once through the dashboard; then pass \
             `session: \"{}\"` to `fetch`, `browse`, `extract` or `crawl`.",
            info.name,
            session_row(&info),
            info.name,
        );
        let structured = serde_json::to_value(&info).unwrap_or(Value::Null);
        Ok(ToolOutput::text(text).with_structured(structured))
    }

    async fn run_session_list(&self) -> Result<ToolOutput, ToolError> {
        let sessions = self.engine.session_list().await;
        let text = if sessions.is_empty() {
            "No sessions are open. `session_open` creates one; its profile \
             directory persists, so a session opened now is still logged in \
             after a restart."
                .to_owned()
        } else {
            let mut text = format!("{} session(s):\n", sessions.len());
            for info in &sessions {
                text.push('\n');
                text.push_str(&session_row(info));
                text.push('\n');
            }
            text
        };
        let structured = serde_json::to_value(&sessions).unwrap_or(Value::Null);
        Ok(ToolOutput::text(text).with_structured(structured))
    }

    async fn run_session_close(&self, args: SessionCloseArgs) -> Result<ToolOutput, ToolError> {
        let name = args.name;
        self.engine.session_close(&name).await?;
        Ok(ToolOutput::text(format!(
            "Session `{name}` is closed. Its profile directory is kept, so \
             `session_open` with the same name reattaches to the same login."
        ))
        .with_structured(json!({ "name": name, "closed": true })))
    }
}

#[async_trait::async_trait]
impl ToolInvoker for EngineTools {
    async fn invoke(&self, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        match name {
            "fetch" => self.run_fetch(parse_args(name, arguments)?).await,
            "browse" => self.run_browse(parse_args(name, arguments)?).await,
            "extract" => self.run_extract(parse_args(name, arguments)?).await,
            "crawl" => self.run_crawl(parse_args(name, arguments)?).await,
            "screenshot" => self.run_screenshot(parse_args(name, arguments)?).await,
            "session_open" => self.run_session_open(parse_args(name, arguments)?).await,
            "session_list" => {
                // No arguments, but a client that sends `{}` must still work.
                let _: SessionListArgs = parse_args(name, arguments)?;
                self.run_session_list().await
            }
            "session_close" => self.run_session_close(parse_args(name, arguments)?).await,
            other => Err(ToolError::unknown_tool(other)),
        }
    }
}

/// `session_list` takes nothing, but `deny_unknown_fields` still has to reject
/// the arguments a confused model invents.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionListArgs {}

// -- conversions onto the engine's vocabulary -------------------------------

impl From<BodyFormat> for ContentFormat {
    fn from(format: BodyFormat) -> Self {
        match format {
            BodyFormat::Markdown => Self::Markdown,
            BodyFormat::Text => Self::Text,
            BodyFormat::Html => Self::Html,
        }
    }
}

impl From<WaitUntil> for PageLoadState {
    fn from(wait: WaitUntil) -> Self {
        match wait {
            WaitUntil::Load => Self::Load,
            WaitUntil::DomContentLoaded => Self::DomContentLoaded,
            WaitUntil::NetworkIdle => Self::NetworkIdle,
        }
    }
}

impl From<WaitForState> for WaitState {
    fn from(state: WaitForState) -> Self {
        match state {
            WaitForState::Attached => Self::Attached,
            WaitForState::Detached => Self::Detached,
            WaitForState::Hidden => Self::Hidden,
            WaitForState::Visible => Self::Visible,
        }
    }
}

impl From<ExtractKind> for EngineExtractKind {
    fn from(kind: ExtractKind) -> Self {
        match kind {
            ExtractKind::Css => Self::Css,
            ExtractKind::Xpath => Self::Xpath,
            ExtractKind::Markdown => Self::Markdown,
            ExtractKind::Text => Self::Text,
        }
    }
}

// -- formatting -------------------------------------------------------------
//
// Everything below turns an engine result into the one thing an agent actually
// consumes: text. The recurring shape is a short header of the facts a model
// cannot infer from the body — the *final* URL after redirects, the status,
// whether a browser was used — and then the content.

/// A `fetch` or `browse` result.
fn page_text(page: &PageResult, max_chars: usize) -> String {
    let mut header = format!(
        "URL: {}\nStatus: {}\nTitle: {}\nTransport: {}\nFormat: {}",
        page.url,
        page.status,
        or_dash(page.title.as_deref()),
        match page.transport {
            Transport::Http => "HTTP (no browser)",
            Transport::Browser => "headless Chrome",
        },
        match page.format {
            ContentFormat::Markdown => "markdown",
            ContentFormat::Text => "text",
            ContentFormat::Html => "html",
        },
    );
    if let Some(session) = &page.session {
        header.push_str(&format!("\nSession: {session}"));
    }
    header.push_str(&format!("\nElapsed: {} ms", page.elapsed_ms));
    if page.truncated {
        // The engine's own ceiling fired before ours did; say so, because the
        // remedy is different — this one the operator configures.
        header.push_str("\nNote: the server's own content ceiling truncated this page.");
    }

    let body = truncate(&page.content, max_chars);
    format!("{header}\n\n{}", body.text)
}

/// An `extract` result.
fn extract_text(result: &ExtractResult, max_matches: usize, max_chars: usize) -> String {
    let kind = match result.kind {
        EngineExtractKind::Css => "css",
        EngineExtractKind::Xpath => "xpath",
        EngineExtractKind::Markdown => "markdown",
        EngineExtractKind::Text => "text",
    };
    let mut header = format!("Extract: kind={kind}");
    if let Some(selector) = &result.selector {
        header.push_str(&format!(", selector={selector:?}"));
    }
    if let Some(url) = &result.url {
        header.push_str(&format!("\nURL: {url}"));
    }

    // `markdown` and `text` produce exactly one match, and numbering a list of
    // one is noise; a selector query is a list and reads as one.
    let body = if matches!(
        result.kind,
        EngineExtractKind::Markdown | EngineExtractKind::Text
    ) {
        result.matches.first().cloned().unwrap_or_default()
    } else {
        let shown = result.matches.len().min(max_matches);
        header.push_str(&format!("\nMatches: {} total, {shown} shown", result.count));
        if result.count == 0 {
            header.push_str(
                "\nNothing matched. Check the selector against the page — \
                 `extract` with kind=\"markdown\" shows you what is there.",
            );
        }
        if shown < result.matches.len() || result.truncated {
            header.push_str("\nNote: the match list was cut short.");
        }
        result
            .matches
            .iter()
            .take(shown)
            .enumerate()
            .map(|(index, value)| format!("{}. {value}", index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!("{header}\n\n{}", truncate(&body, max_chars).text)
}

/// A `crawl` result: an index, as a markdown table.
fn crawl_text(outcome: &CrawlOutcome, selector: Option<&str>, max_chars: usize) -> String {
    let mut header = format!(
        "Crawl of {}\nPages: {} (budget {}, depth limit {})\nElapsed: {} ms",
        outcome.seed,
        outcome.pages.len(),
        outcome.max_pages,
        outcome.max_depth,
        outcome.elapsed_ms,
    );
    if outcome.stopped_at_limit {
        header.push_str(
            "\nNote: the page budget ended this crawl, not the link graph. \
             Raise `max_pages` if you need the rest.",
        );
    }

    let mut body = String::from("| depth | status | url | title |\n|---|---|---|---|\n");
    for page in &outcome.pages {
        body.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            page.depth,
            page.status,
            page.url,
            // A pipe in a title would break the table row.
            or_dash(page.title.as_deref()).replace('|', "\\|"),
        ));
    }

    if let Some(selector) = selector {
        body.push_str(&format!("\n## `{selector}` matches\n\n"));
        for page in &outcome.pages {
            let matches = page.matches.as_deref().unwrap_or_default();
            if matches.is_empty() {
                body.push_str(&format!("- {} — (none)\n", page.url));
            } else {
                body.push_str(&format!("- {} — {}\n", page.url, matches.join(" · ")));
            }
        }
    }

    format!("{header}\n\n{}", truncate(&body, max_chars).text)
}

/// A captured PNG, as the two blocks a model receives: a caption, then the
/// image.
///
/// Split out from the call so the size ceiling is reachable by a test — the
/// alternative is a test that needs Chrome to produce four megabytes of PNG,
/// which is to say no test at all.
fn screenshot_output(source: &str, png: &[u8], full_page: bool) -> Result<ToolOutput, ToolError> {
    // The one payload the engine does not bound. Refusing with a message beats
    // spending several megabytes of someone's context window on an image they
    // asked for by accident — and the remedy is one argument away.
    if png.len() > MAX_SCREENSHOT_BYTES {
        return Err(ToolError::execution(format!(
            "the screenshot of {source} is {:.1} MiB, over the {} MiB limit{}. \
             Capture the viewport instead of the whole document, or `browse` to \
             a narrower page first.",
            png.len() as f64 / (1024.0 * 1024.0),
            MAX_SCREENSHOT_BYTES / (1024 * 1024),
            if full_page {
                " — `full_page: true` on a long document is the usual cause"
            } else {
                ""
            },
        )));
    }

    Ok(ToolOutput {
        content: vec![
            // The caption goes first, and repeats the scale factor that is also
            // in the schema: this is what a model is looking at when it reads a
            // coordinate off the image and halves nothing.
            ToolContent::Text(format!(
                "Screenshot of {source} — PNG, {} bytes, {}. Captured at a \
                 device scale factor of 2: halve any pixel coordinate you read \
                 off it to get CSS pixels.",
                png.len(),
                if full_page {
                    "full page"
                } else {
                    "viewport only"
                }
            )),
            ToolContent::Image {
                base64: BASE64.encode(png),
                mime_type: "image/png",
            },
        ],
        structured: None,
    })
}

/// One session, as a line an agent can read.
fn session_row(info: &SessionInfo) -> String {
    format!(
        "- `{}` — browser {}, idle {} s, {} request(s), proxy {}, impersonate {}",
        info.name,
        if info.browser_live {
            "live"
        } else {
            "not running"
        },
        info.idle_secs,
        info.requests,
        or_dash(info.proxy.as_deref()),
        or_dash(info.impersonate.as_deref()),
    )
}

// ---------------------------------------------------------------------------
// The protocol layer
// ---------------------------------------------------------------------------

/// The attribute [`ToolInvoker`] is declared with, re-exported so an
/// implementation outside this crate — `browser-mcp`'s test stubs, an
/// embedder's fake — can be written without taking its own dependency on it.
pub use async_trait::async_trait;

/// Anything that can run a tool call by name.
///
/// The seam exists so the JSON-RPC layer and the HTTP transport can be built
/// and tested without an engine — and, more usefully, so `browser-mcp`'s `rmcp`
/// handler and this crate's HTTP mount hold the *same* object rather than two
/// copies of the dispatch logic.
#[async_trait::async_trait]
pub trait ToolInvoker: Send + Sync + 'static {
    /// Run `name` with `arguments`.
    async fn invoke(&self, name: &str, arguments: Value) -> Result<ToolOutput, ToolError>;
}

/// The name this server reports in `initialize`.
pub const SERVER_NAME: &str = "browser-rs";

/// The server version reported in `initialize`.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The absolute path this surface serves.
///
/// [`router`] routes on it directly, so `serve.rs` **merges** rather than nests
/// — `API.md` §3a's rule for every surface that owns an absolute path.
pub const MOUNT_PATH: &str = "/mcp";

/// Protocol revisions this server will speak.
///
/// A client's requested version is echoed back when it is one of these, which
/// is what the specification asks for; anything else falls back to
/// [`FALLBACK_PROTOCOL_VERSION`].
pub const KNOWN_PROTOCOL_VERSIONS: [&str; 4] =
    ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// The revision offered to a client that asked for one we do not know.
///
/// `2025-06-18` rather than the newest: it is the revision in which streamable
/// HTTP settled, and it is what the widest set of agent runtimes negotiates
/// today. A client that wants a newer one asks for it and gets it echoed.
pub const FALLBACK_PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC error codes, spelled out where they are used.
mod rpc_error {
    /// The body was not JSON.
    pub const PARSE: i64 = -32700;
    /// The body was JSON but not a JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method, or no such tool.
    pub const METHOD_NOT_FOUND: i64 = -32601;
}

/// Serialise one tool into the `tools/list` wire shape.
pub fn tool_to_json(tool: &ToolDescriptor) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.input_schema,
        "annotations": {
            "readOnlyHint": tool.read_only,
            "idempotentHint": tool.idempotent,
            // Every tool here reaches out to somebody else's web server.
            "openWorldHint": true,
        },
    })
}

/// The `initialize` result, given the version the client asked for.
pub fn initialize_result(requested: Option<&str>) -> Value {
    let version = match requested {
        Some(asked) if KNOWN_PROTOCOL_VERSIONS.contains(&asked) => asked,
        _ => FALLBACK_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        "instructions": SERVER_INSTRUCTIONS,
    })
}

/// The `tools/call` result for a successful run.
pub fn call_tool_result(output: &ToolOutput) -> Value {
    let mut result = Map::new();
    result.insert(
        "content".to_owned(),
        Value::Array(output.content.iter().map(ToolContent::to_json).collect()),
    );
    if let Some(structured) = &output.structured {
        result.insert("structuredContent".to_owned(), structured.clone());
    }
    result.insert("isError".to_owned(), Value::Bool(false));
    Value::Object(result)
}

/// The `tools/call` result for a failure the model should read.
///
/// Deliberately an `isError` *result* rather than a JSON-RPC error: clients
/// render protocol errors opaquely, so a message sent that way never reaches
/// the model, and "the URL you gave has no scheme" is precisely the sort of
/// thing the model needs to read in order to fix its next call.
pub fn call_tool_error(error: &ToolError) -> Value {
    json!({
        "content": [{ "type": "text", "text": error.message }],
        "isError": true,
    })
}

/// Handle one decoded JSON-RPC message.
///
/// Returns `None` for a notification, which by the specification gets no reply
/// at all — the HTTP transport turns that into `202 Accepted`.
pub async fn handle_message(invoker: &dyn ToolInvoker, message: Value) -> Option<Value> {
    let object = message.as_object()?;
    let id = object.get("id").cloned();
    let method = object.get("method").and_then(Value::as_str);

    let Some(method) = method else {
        // A `result` or an `error` alongside an id makes this a *response*, not
        // a request. This server sends no requests, so one can only arrive by
        // mistake — and answering a response with an error is how two peers
        // talk past each other indefinitely. Drop it.
        if object.contains_key("result") || object.contains_key("error") {
            return None;
        }
        return id.map(|id| rpc_error_response(id, rpc_error::INVALID_REQUEST, "missing `method`"));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);

    // Notifications carry no id and get no response, however they turn out.
    let id = id?;

    let result = match method {
        "initialize" => Ok(initialize_result(
            params.get("protocolVersion").and_then(Value::as_str),
        )),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": tool_catalog().iter().map(tool_to_json).collect::<Vec<_>>(),
        })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            if tool_by_name(name).is_none() {
                Err((
                    rpc_error::METHOD_NOT_FOUND,
                    ToolError::unknown_tool(name).message,
                ))
            } else {
                match invoker.invoke(name, arguments).await {
                    Ok(output) => Ok(call_tool_result(&output)),
                    // An `UnknownTool` cannot get here, so everything left is a
                    // failure the model should be shown.
                    Err(error) => Ok(call_tool_error(&error)),
                }
            }
        }
        other => Err((
            rpc_error::METHOD_NOT_FOUND,
            format!("this server does not implement {other:?}"),
        )),
    };

    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error_response(id, code, &message),
    })
}

/// Build a JSON-RPC error response.
fn rpc_error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Decode a request body and answer it.
///
/// Handles the single-message and batch forms; a batch whose members are all
/// notifications produces no reply, exactly as one notification does.
pub async fn handle_body(invoker: &dyn ToolInvoker, body: &[u8]) -> Option<Value> {
    let message: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => {
            return Some(rpc_error_response(
                Value::Null,
                rpc_error::PARSE,
                &format!("request body is not JSON: {error}"),
            ))
        }
    };

    match message {
        Value::Array(messages) => {
            let mut replies = Vec::new();
            for message in messages {
                if let Some(reply) = handle_message(invoker, message).await {
                    replies.push(reply);
                }
            }
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        Value::Object(_) => handle_message(invoker, message).await,
        _ => Some(rpc_error_response(
            Value::Null,
            rpc_error::INVALID_REQUEST,
            "a JSON-RPC message must be an object or an array of objects",
        )),
    }
}

// ---------------------------------------------------------------------------
// The streamable-HTTP transport
// ---------------------------------------------------------------------------

/// What the `/mcp` routes hold.
#[derive(Clone)]
pub struct McpState {
    invoker: Arc<dyn ToolInvoker>,
    token: Option<Arc<str>>,
}

/// The `/mcp` surface, in the shape `API.md` §3a binds every surface to: the
/// engine already captured, no unfilled state, its own absolute path.
///
/// `serve.rs` composes it with `Router::merge` and applies trace, timeout and
/// body-limit layers around the merged router, so **no layer is applied here** —
/// repeating one would double it.
///
/// The bearer check is not a layer and is not repeated from anywhere: it is a
/// per-handler guard reading [`crate::config::ServerConfig::api_token`], and it
/// stays here deliberately. `api::router` scopes its `require_token` middleware
/// to the `/api` routes with `route_layer`, so nothing outside this function
/// currently guards `/mcp`; an unauthenticated MCP endpoint is an open browser
/// proxy onto
/// the operator's logins, and that must not depend on how a caller
/// happened to compose the router. If a global auth layer is added later this check simply
/// passes a second time — checking a valid token twice costs a string compare,
/// where omitting it once costs the operator their sessions.
///
/// Only `POST` is implemented. The specification permits a server to answer
/// `application/json` and refuse the server-initiated `GET` stream with `405`,
/// and nothing in this tool set pushes notifications, so there is no stream to
/// open. `DELETE` — a client ending its session — succeeds trivially: this
/// transport keeps no per-connection state, so there is nothing to tear down.
#[must_use]
pub fn router(engine: Arc<Engine>) -> Router {
    let token = engine.config().api_token.clone();
    router_with(EngineTools::shared(engine), token)
}

/// [`router`], over an arbitrary [`ToolInvoker`].
///
/// The seam an embedder — or a test — needs in order to serve this exact HTTP
/// surface without standing up a real engine.
#[must_use]
pub fn router_with(invoker: Arc<dyn ToolInvoker>, token: Option<String>) -> Router {
    let state = McpState {
        invoker,
        token: token.map(Arc::from),
    };
    Router::new()
        .route(
            MOUNT_PATH,
            post(post_handler).get(get_handler).delete(delete_handler),
        )
        .with_state(state)
}

/// `POST /mcp` — one JSON-RPC message in, at most one out.
async fn post_handler(State(state): State<McpState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(rejection) = check_bearer(&state, &headers) {
        return rejection;
    }
    match handle_body(state.invoker.as_ref(), &body).await {
        Some(reply) => (StatusCode::OK, axum::Json(reply)).into_response(),
        // Nothing to say: the body held only notifications.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// `GET /mcp` — the optional server-initiated SSE stream, which we do not offer.
async fn get_handler(State(state): State<McpState>, headers: HeaderMap) -> Response {
    if let Some(rejection) = check_bearer(&state, &headers) {
        return rejection;
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        axum::Json(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": rpc_error::METHOD_NOT_FOUND,
                "message": "this endpoint does not offer a server-initiated \
        stream; POST JSON-RPC messages instead",
            },
        })),
    )
        .into_response()
}

/// `DELETE /mcp` — a client ending its session, which costs us nothing.
async fn delete_handler(State(state): State<McpState>, headers: HeaderMap) -> Response {
    if let Some(rejection) = check_bearer(&state, &headers) {
        return rejection;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Enforce `Authorization: Bearer <token>` when a token is configured.
///
/// Returns the rejection to send, or `None` when the request may proceed. An
/// `Option` rather than a `Result` because the error case here is a whole
/// `axum::Response`, which is far too large a value to be an `Err`.
fn check_bearer(state: &McpState, headers: &HeaderMap) -> Option<Response> {
    // No token configured: `ServerConfig::effective_bind` has already forced
    // the listener onto loopback, so the port is not reachable from another
    // host.
    let expected = state.token.as_ref()?;
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .trim();
    if presented == expected.as_ref() && !presented.is_empty() {
        None
    } else {
        Some(ServerError::Unauthorized.into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::ServerConfig;

    // -- the catalogue ----------------------------------------------------

    #[test]
    fn catalogue_names_and_order_are_stable() {
        let names: Vec<&str> = tool_catalog().iter().map(|tool| tool.name).collect();
        assert_eq!(names, TOOL_NAMES.to_vec());
    }

    #[test]
    fn every_tool_has_a_description_and_an_object_schema() {
        for tool in tool_catalog() {
            assert!(
                tool.description.len() > 80,
                "{} has a stub description",
                tool.name
            );
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(
                tool.input_schema["properties"].is_object(),
                "{} has no properties object",
                tool.name
            );
        }
    }

    /// API.md §3: "a JSON schema with a description on every field". This is
    /// the test that keeps that true as fields are added.
    #[test]
    fn every_schema_field_carries_a_description() {
        for tool in tool_catalog() {
            let properties = tool.input_schema["properties"]
                .as_object()
                .expect("properties object");
            for (field, schema) in properties {
                let description = schema["description"].as_str().unwrap_or_default();
                assert!(
                    description.len() > 20,
                    "{}.{field} has no useful description",
                    tool.name
                );
            }
        }
    }

    /// Every description in this module is written with `\`-continuations, and
    /// `rustfmt` re-indents the continuation lines whenever the surrounding
    /// code moves. Rust strips the leading whitespace after a continuation, so
    /// the rendered text is unaffected — but only as long as every wrap really
    /// is a continuation. A single forgotten backslash bakes a dozen spaces
    /// into what an agent reads, and nothing else would catch it.
    #[test]
    fn no_description_carries_the_source_files_indentation() {
        let mut texts: Vec<(String, &str)> = Vec::new();
        for tool in tool_catalog() {
            texts.push((tool.name.to_owned(), tool.description));
            let properties = tool.input_schema["properties"]
                .as_object()
                .expect("properties")
                .clone();
            for (field, schema) in properties {
                let description = schema["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                assert!(
                    !description.contains("    "),
                    "{}.{field} contains source indentation: {description:?}",
                    tool.name
                );
            }
        }
        for (name, text) in texts {
            assert!(
                !text.contains("    "),
                "{name}'s description contains source indentation"
            );
        }
        assert!(!SERVER_INSTRUCTIONS.contains("      "));
    }

    /// Every impersonation profile the schema offers as an example must
    /// actually resolve. A schema is the only documentation an agent gets, and
    /// a plausible-looking name that no longer exists costs it a failed call
    /// and a wrong theory about why.
    #[test]
    fn the_impersonation_examples_are_real_profiles() {
        let description = impersonate_property()["description"]
            .as_str()
            .expect("description")
            .to_owned();
        let named: Vec<&str> = description.split('`').skip(1).step_by(2).collect();
        assert!(named.len() >= 8, "the examples went missing: {named:?}");
        for name in named {
            assert!(
                browser_fingerprint::impersonate::ImpersonateProfile::parse(name).is_ok(),
                "the schema offers {name:?}, which browser-fingerprint does not know"
            );
        }
    }

    #[test]
    fn required_fields_exist_as_properties() {
        for tool in tool_catalog() {
            let properties = tool.input_schema["properties"]
                .as_object()
                .expect("properties object");
            for required in tool.input_schema["required"]
                .as_array()
                .expect("required array")
            {
                let name = required.as_str().expect("required name");
                assert!(
                    properties.contains_key(name),
                    "{} requires {name}, which it does not declare",
                    tool.name
                );
            }
        }
    }

    /// The descriptions have one job beyond describing: routing a model between
    /// `fetch` and `browse`, and explaining what a session buys. API.md §3 asks
    /// for both by name.
    #[test]
    fn descriptions_route_between_fetch_and_browse() {
        let fetch = fetch_tool();
        assert!(fetch.description.contains("browse"));
        assert!(fetch.description.contains("no browser"));

        let browse = browse_tool();
        assert!(browse.description.contains("fetch"));
        assert!(browse.description.contains("login"));
    }

    #[test]
    fn session_story_is_told_where_an_agent_will_read_it() {
        assert!(SERVER_INSTRUCTIONS.contains("survive requests"));
        assert!(session_open_tool().description.contains("dashboard"));
        // Every tool that takes a session says what a session is for.
        for tool in tool_catalog() {
            let properties = tool.input_schema["properties"]
                .as_object()
                .expect("properties");
            if let Some(session) = properties.get("session") {
                let text = session["description"].as_str().expect("description");
                assert!(text.contains("session_open"), "{}", tool.name);
                assert!(text.contains("cookies"), "{}", tool.name);
            }
        }
    }

    #[test]
    fn tool_by_name_finds_each_tool_and_nothing_else() {
        for name in TOOL_NAMES {
            assert_eq!(tool_by_name(name).expect("known tool").name, name);
        }
        assert!(tool_by_name("navigate").is_none());
    }

    // -- truncation -------------------------------------------------------

    #[test]
    fn short_bodies_pass_through_untouched() {
        let result = truncate("hello", 100);
        assert_eq!(result.text, "hello");
        assert!(!result.was_truncated());
        assert_eq!(result.total_chars, 5);
        assert_eq!(result.kept_chars, 5);
    }

    #[test]
    fn a_body_exactly_at_the_cap_is_not_marked() {
        let result = truncate("abcde", 5);
        assert_eq!(result.text, "abcde");
        assert!(!result.was_truncated());
    }

    #[test]
    fn truncation_is_visible_and_says_how_much_was_dropped() {
        let body = "x".repeat(1_000);
        let result = truncate(&body, 100);
        assert!(result.was_truncated());
        assert!(result.text.starts_with(&"x".repeat(100)));
        assert!(result
            .text
            .contains("[truncated: 100 of 1000 characters shown"));
        assert!(result.text.contains("900 dropped"));
        // The remedy is named, not merely the problem.
        assert!(result.text.contains("`extract`"));
        assert_eq!(result.kept_chars, 100);
        assert_eq!(result.total_chars, 1_000);
    }

    /// Counting bytes here would both cut a multi-byte character in half and
    /// hand back a third of the requested content for CJK text.
    #[test]
    fn truncation_counts_characters_not_bytes() {
        let body = "日本語".repeat(100); // 300 chars, 900 bytes
        let result = truncate(&body, 150);
        assert_eq!(result.total_chars, 300);
        assert_eq!(result.kept_chars, 150);
        let kept: String = result.text.chars().take(150).collect();
        assert_eq!(kept, "日本語".repeat(50));
    }

    #[test]
    fn caps_are_clamped_into_the_advertised_range() {
        assert_eq!(clamp_max_chars(None, 20_000), 20_000);
        assert_eq!(clamp_max_chars(Some(500), 20_000), 500);
        assert_eq!(clamp_max_chars(Some(0), 20_000), 1);
        assert_eq!(clamp_max_chars(Some(usize::MAX), 20_000), MAX_CHARS_CEILING);
    }

    // -- argument parsing -------------------------------------------------

    #[test]
    fn fetch_args_parse_with_only_a_url() {
        let args: FetchArgs =
            parse_args("fetch", json!({ "url": "https://a.test/" })).expect("valid");
        assert_eq!(args.url, "https://a.test/");
        assert_eq!(args.format, BodyFormat::Markdown);
        assert!(args.max_chars.is_none());
    }

    #[test]
    fn a_missing_required_field_explains_itself() {
        let error = parse_args::<FetchArgs>("fetch", json!({})).unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidArguments);
        assert!(error.message.contains("url"));
        assert!(error.message.contains("input schema"));
    }

    #[test]
    fn an_invented_field_is_rejected_rather_than_ignored() {
        let error = parse_args::<FetchArgs>(
            "fetch",
            json!({ "url": "https://a.test/", "javascript": true }),
        )
        .unwrap_err();
        assert!(error.message.contains("javascript"));
    }

    #[test]
    fn tools_with_no_required_fields_accept_null_arguments() {
        let args: ScreenshotArgs = parse_args("screenshot", Value::Null).expect("valid");
        assert!(args.url.is_none());
        assert!(args.session.is_none());
    }

    #[test]
    fn enums_use_the_spellings_the_schema_advertises() {
        let args: BrowseArgs = parse_args(
            "browse",
            json!({ "url": "https://a.test/", "wait_until": "networkidle" }),
        )
        .expect("valid");
        assert_eq!(args.wait_until, WaitUntil::NetworkIdle);

        let args: BrowseArgs = parse_args(
            "browse",
            json!({ "url": "https://a.test/", "wait_until": "domcontentloaded" }),
        )
        .expect("valid");
        assert_eq!(args.wait_until, WaitUntil::DomContentLoaded);

        // And the schema's `enum` list is exactly those spellings.
        let schema = browse_tool().input_schema;
        assert_eq!(
            schema["properties"]["wait_until"]["enum"],
            json!(["domcontentloaded", "load", "networkidle"])
        );
    }

    #[test]
    fn extract_kinds_match_the_schema() {
        let schema = extract_tool().input_schema;
        assert_eq!(
            schema["properties"]["kind"]["enum"],
            json!(["css", "xpath", "markdown", "text"])
        );
        for kind in ["css", "xpath", "markdown", "text"] {
            let args: ExtractArgs =
                parse_args("extract", json!({ "kind": kind, "selector": "h1" })).expect("valid");
            assert_eq!(
                serde_json::to_value(args.kind).expect("serialisable"),
                json!(kind)
            );
        }
    }

    // -- source selection -------------------------------------------------

    #[test]
    fn exactly_one_source_accepts_one() {
        let chosen =
            exactly_one_source("extract", &[("url", true), ("html", false)]).expect("one source");
        assert_eq!(chosen, "url");
    }

    #[test]
    fn exactly_one_source_rejects_none_and_names_the_options() {
        let error = exactly_one_source("extract", &[("url", false), ("html", false)]).unwrap_err();
        assert!(error.message.contains("url, html"));
    }

    #[test]
    fn exactly_one_source_rejects_two_and_names_both() {
        let error = exactly_one_source("extract", &[("url", true), ("html", true)]).unwrap_err();
        assert!(error.message.contains("url and html"));
    }

    // -- the engine mapping ------------------------------------------------
    //
    // The conversions are one-line matches, and one-line matches are exactly
    // where a transposed pair goes unnoticed: `domcontentloaded` mapped to
    // `Load` would make `browse` quietly return half-rendered pages.

    #[test]
    fn wait_conditions_map_onto_the_engine_one_for_one() {
        assert_eq!(PageLoadState::from(WaitUntil::Load), PageLoadState::Load);
        assert_eq!(
            PageLoadState::from(WaitUntil::DomContentLoaded),
            PageLoadState::DomContentLoaded
        );
        assert_eq!(
            PageLoadState::from(WaitUntil::NetworkIdle),
            PageLoadState::NetworkIdle
        );

        assert_eq!(WaitState::from(WaitForState::Attached), WaitState::Attached);
        assert_eq!(WaitState::from(WaitForState::Detached), WaitState::Detached);
        assert_eq!(WaitState::from(WaitForState::Hidden), WaitState::Hidden);
        assert_eq!(WaitState::from(WaitForState::Visible), WaitState::Visible);
    }

    #[test]
    fn formats_and_kinds_map_onto_the_engine_one_for_one() {
        assert_eq!(
            ContentFormat::from(BodyFormat::Markdown),
            ContentFormat::Markdown
        );
        assert_eq!(ContentFormat::from(BodyFormat::Text), ContentFormat::Text);
        assert_eq!(ContentFormat::from(BodyFormat::Html), ContentFormat::Html);

        assert_eq!(
            EngineExtractKind::from(ExtractKind::Css),
            EngineExtractKind::Css
        );
        assert_eq!(
            EngineExtractKind::from(ExtractKind::Xpath),
            EngineExtractKind::Xpath
        );
        assert_eq!(
            EngineExtractKind::from(ExtractKind::Markdown),
            EngineExtractKind::Markdown
        );
        assert_eq!(
            EngineExtractKind::from(ExtractKind::Text),
            EngineExtractKind::Text
        );
    }

    /// MCP defaults to markdown where the REST surface defaults to HTML. That
    /// divergence is deliberate — an agent wants readable structure — and it
    /// only holds because every request this layer builds sets `format`.
    #[test]
    fn the_mcp_default_format_is_markdown_not_the_engines_html() {
        assert_eq!(BodyFormat::default(), BodyFormat::Markdown);
        assert_eq!(ContentFormat::default(), ContentFormat::Html);
        let args: FetchArgs =
            parse_args("fetch", json!({ "url": "https://a.test/" })).expect("valid");
        assert_eq!(ContentFormat::from(args.format), ContentFormat::Markdown);
    }

    // -- result formatting -------------------------------------------------

    fn a_page() -> PageResult {
        PageResult {
            url: "https://example.test/final".to_owned(),
            status: 200,
            title: Some("Example".to_owned()),
            format: ContentFormat::Markdown,
            content: "# Example\n\nBody text.".to_owned(),
            truncated: false,
            headers: HeaderDict::new(),
            cookies: HeaderDict::new(),
            session: Some("shop".to_owned()),
            proxy: None,
            transport: Transport::Http,
            elapsed_ms: 42,
        }
    }

    #[test]
    fn a_page_header_carries_what_the_body_cannot_say() {
        let text = page_text(&a_page(), DEFAULT_MAX_CHARS);
        // The *final* URL after redirects, which the body never states.
        assert!(text.contains("URL: https://example.test/final"));
        assert!(text.contains("Status: 200"));
        assert!(text.contains("Title: Example"));
        assert!(text.contains("Transport: HTTP (no browser)"));
        assert!(text.contains("Session: shop"));
        assert!(text.ends_with("# Example\n\nBody text."));
    }

    #[test]
    fn a_browser_page_says_so() {
        let mut page = a_page();
        page.transport = Transport::Browser;
        page.session = None;
        let text = page_text(&page, DEFAULT_MAX_CHARS);
        assert!(text.contains("Transport: headless Chrome"));
        assert!(!text.contains("Session:"));
    }

    #[test]
    fn a_long_page_is_cut_visibly() {
        let mut page = a_page();
        page.content = "x".repeat(5_000);
        let text = page_text(&page, 100);
        assert!(text.contains("[truncated: 100 of 5000 characters shown"));
    }

    /// Two ceilings can fire, and they have different remedies — one the agent
    /// controls, one the operator does — so the header names the other one.
    #[test]
    fn the_servers_own_ceiling_is_reported_separately() {
        let mut page = a_page();
        page.truncated = true;
        let text = page_text(&page, DEFAULT_MAX_CHARS);
        assert!(text.contains("server's own content ceiling"));
    }

    fn an_extract(kind: EngineExtractKind, matches: Vec<&str>, count: usize) -> ExtractResult {
        ExtractResult {
            kind,
            url: Some("https://example.test/".to_owned()),
            selector: Some(".price".to_owned()),
            matches: matches.into_iter().map(str::to_owned).collect(),
            count,
            truncated: false,
        }
    }

    #[test]
    fn selector_matches_are_numbered_and_counted() {
        let result = an_extract(EngineExtractKind::Css, vec!["£1", "£2"], 2);
        let text = extract_text(&result, DEFAULT_MAX_MATCHES, DEFAULT_MAX_CHARS);
        assert!(text.contains("kind=css"));
        assert!(text.contains("selector=\".price\""));
        assert!(text.contains("Matches: 2 total, 2 shown"));
        assert!(text.contains("1. £1"));
        assert!(text.contains("2. £2"));
    }

    /// A cut list must never read like a complete one.
    #[test]
    fn a_capped_match_list_says_the_total() {
        let result = an_extract(EngineExtractKind::Css, vec!["a", "b", "c"], 3);
        let text = extract_text(&result, 2, DEFAULT_MAX_CHARS);
        assert!(text.contains("Matches: 3 total, 2 shown"));
        assert!(text.contains("cut short"));
        assert!(!text.contains("3. c"));
    }

    #[test]
    fn no_matches_suggests_what_to_do_next() {
        let result = an_extract(EngineExtractKind::Css, vec![], 0);
        let text = extract_text(&result, DEFAULT_MAX_MATCHES, DEFAULT_MAX_CHARS);
        assert!(text.contains("Nothing matched"));
        assert!(text.contains("markdown"));
    }

    #[test]
    fn a_whole_page_extract_is_not_numbered() {
        let result = an_extract(EngineExtractKind::Text, vec!["The readable text."], 1);
        let text = extract_text(&result, DEFAULT_MAX_MATCHES, DEFAULT_MAX_CHARS);
        assert!(text.ends_with("The readable text."));
        assert!(!text.contains("1. "));
        assert!(!text.contains("Matches:"));
    }

    fn an_outcome(stopped: bool, matches: Option<Vec<&str>>) -> CrawlOutcome {
        CrawlOutcome {
            seed: "https://example.test/".to_owned(),
            pages: vec![crate::engine::CrawlPage {
                url: "https://example.test/a".to_owned(),
                depth: 1,
                status: 200,
                title: Some("A | B".to_owned()),
                matches: matches.map(|list| list.into_iter().map(str::to_owned).collect()),
            }],
            max_pages: 20,
            max_depth: 2,
            stopped_at_limit: stopped,
            elapsed_ms: 900,
        }
    }

    #[test]
    fn a_crawl_is_a_table_with_the_budget_stated() {
        let text = crawl_text(&an_outcome(false, None), None, DEFAULT_CRAWL_MAX_CHARS);
        assert!(text.contains("Pages: 1 (budget 20, depth limit 2)"));
        assert!(text.contains("| depth | status | url | title |"));
        assert!(text.contains("| 1 | 200 | https://example.test/a |"));
        // A pipe in a title would otherwise split the row into extra columns.
        assert!(text.contains("A \\| B"));
        assert!(!text.contains("page budget ended"));
    }

    #[test]
    fn a_crawl_stopped_by_its_budget_says_so() {
        let text = crawl_text(&an_outcome(true, None), None, DEFAULT_CRAWL_MAX_CHARS);
        assert!(text.contains("page budget ended this crawl"));
        assert!(text.contains("Raise `max_pages`"));
    }

    #[test]
    fn crawl_selector_matches_get_their_own_section() {
        let text = crawl_text(
            &an_outcome(false, Some(vec!["£1", "£2"])),
            Some(".price"),
            DEFAULT_CRAWL_MAX_CHARS,
        );
        assert!(text.contains("## `.price` matches"));
        assert!(text.contains("https://example.test/a — £1 · £2"));

        let text = crawl_text(
            &an_outcome(false, Some(vec![])),
            Some(".price"),
            DEFAULT_CRAWL_MAX_CHARS,
        );
        assert!(text.contains("— (none)"));
    }

    // -- screenshots -------------------------------------------------------

    #[test]
    fn a_capture_returns_a_caption_and_then_the_image() {
        let png = b"\x89PNG\r\n\x1a\ntiny".to_vec();
        let output =
            screenshot_output("https://example.test/", &png, false).expect("under the cap");
        assert_eq!(output.content.len(), 2);

        let ToolContent::Text(caption) = &output.content[0] else {
            panic!("the caption must come first, so a model reads it before looking");
        };
        assert!(caption.contains("https://example.test/"), "{caption}");
        assert!(caption.contains("viewport only"), "{caption}");
        // The 2x scale factor is repeated beside the image, not only in the
        // schema: this is where a model is looking when it misreads a
        // coordinate off one.
        assert!(caption.contains("device scale factor of 2"), "{caption}");
        assert!(caption.contains("halve"), "{caption}");

        let ToolContent::Image { base64, mime_type } = &output.content[1] else {
            panic!("expected an image block");
        };
        assert_eq!(*mime_type, "image/png");
        assert_eq!(base64, &BASE64.encode(&png));
    }

    #[test]
    fn a_full_page_capture_says_so_in_its_caption() {
        let output =
            screenshot_output("https://example.test/", b"png", true).expect("under the cap");
        let ToolContent::Text(caption) = &output.content[0] else {
            panic!("expected a caption");
        };
        assert!(caption.contains("full page"), "{caption}");
    }

    /// The engine bounds text and does **not** bound image bytes, so this is
    /// the only thing between `full_page` on a long document and several
    /// megabytes of base64 in a context window.
    #[test]
    fn an_oversized_capture_is_refused_and_names_the_remedy() {
        let huge = vec![0_u8; MAX_SCREENSHOT_BYTES + 1];
        let error = screenshot_output("https://example.test/", &huge, true).unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Execution);
        assert!(
            error.message.contains("over the 4 MiB limit"),
            "{}",
            error.message
        );
        assert!(error.message.contains("full_page"), "{}", error.message);
        assert!(error.message.contains("viewport"), "{}", error.message);
    }

    /// Exactly at the ceiling is under it — an off-by-one here would reject a
    /// capture that fits.
    #[test]
    fn a_capture_exactly_at_the_ceiling_is_returned() {
        let exact = vec![0_u8; MAX_SCREENSHOT_BYTES];
        let output = screenshot_output("https://example.test/", &exact, false).expect("at the cap");
        assert_eq!(output.content.len(), 2);
    }

    /// A viewport capture that is somehow too large should not blame
    /// `full_page`, which the caller did not set.
    #[test]
    fn an_oversized_viewport_capture_does_not_blame_full_page() {
        let huge = vec![0_u8; MAX_SCREENSHOT_BYTES + 1];
        let error = screenshot_output("https://example.test/", &huge, false).unwrap_err();
        assert!(
            !error.message.contains("full_page: true"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_session_row_names_the_facts_an_agent_acts_on() {
        let info = SessionInfo {
            name: "shop".to_owned(),
            profile_dir: "/profiles/shop".to_owned(),
            proxy: None,
            impersonate: Some("chrome".to_owned()),
            user_agent: None,
            browser_live: true,
            created_at: 0,
            idle_secs: 12,
            requests: 4,
        };
        let row = session_row(&info);
        assert!(row.contains("`shop`"));
        assert!(row.contains("browser live"));
        assert!(row.contains("idle 12 s"));
        assert!(row.contains("4 request(s)"));
        assert!(row.contains("impersonate chrome"));
    }

    // -- argument validation, through the real invoker ---------------------
    //
    // These reach `EngineTools` but never reach the network: every one is
    // rejected before an engine method is called. That is the point — a bad
    // call should cost an agent a message, not a request to someone's server.

    fn engine_tools(profiles: &tempfile::TempDir) -> EngineTools {
        EngineTools::new(Arc::new(Engine::new(ServerConfig {
            profile_root: profiles.path().to_path_buf(),
            ..ServerConfig::default()
        })))
    }

    fn invoke(name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let profiles = tempfile::tempdir().expect("tempdir");
        let tools = engine_tools(&profiles);
        block_on(tools.invoke(name, arguments))
    }

    #[test]
    fn extract_without_a_source_says_which_ones_exist() {
        let error = invoke("extract", json!({ "kind": "text" })).unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidArguments);
        assert!(error.message.contains("url, html"), "{}", error.message);
    }

    #[test]
    fn extract_with_two_sources_is_rejected_rather_than_guessed_at() {
        let error = invoke(
            "extract",
            json!({ "kind": "text", "url": "https://a.test/", "html": "<p>hi" }),
        )
        .unwrap_err();
        assert!(error.message.contains("url and html"), "{}", error.message);
    }

    /// A model that asked for `css` with no selector nearly always wanted to
    /// read the page, so the message names the tool call it should have made.
    #[test]
    fn a_selector_query_without_a_selector_names_the_alternative() {
        let error = invoke("extract", json!({ "kind": "css", "html": "<p>hi" })).unwrap_err();
        assert!(error.message.contains("kind=\"css\""), "{}", error.message);
        assert!(error.message.contains("kind=\"text\""), "{}", error.message);
        assert!(
            error.message.contains("kind=\"markdown\""),
            "{}",
            error.message
        );
    }

    #[test]
    fn screenshot_with_neither_a_url_nor_a_session_explains_both() {
        let error = invoke("screenshot", json!({})).unwrap_err();
        assert!(error.message.contains("`url`"), "{}", error.message);
        assert!(error.message.contains("`session`"), "{}", error.message);
    }

    #[test]
    fn the_invoker_rejects_a_tool_it_does_not_serve() {
        let error = invoke("navigate", json!({})).unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::UnknownTool);
    }

    /// `session_list` takes nothing, and both spellings of "nothing" work.
    #[test]
    fn session_list_accepts_empty_and_null_arguments() {
        let profiles = tempfile::tempdir().expect("tempdir");
        let tools = engine_tools(&profiles);
        for arguments in [json!({}), Value::Null] {
            let output = block_on(tools.invoke("session_list", arguments)).expect("no sessions");
            assert!(matches!(output.content.first(), Some(ToolContent::Text(_))));
        }
        // But an invented field is still an error, not a silent no-op.
        let error = block_on(tools.invoke("session_list", json!({ "all": true }))).unwrap_err();
        assert!(error.message.contains("all"), "{}", error.message);
    }

    // -- errors -----------------------------------------------------------

    #[test]
    fn an_unknown_tool_lists_the_known_ones() {
        let error = ToolError::unknown_tool("navigate");
        assert_eq!(error.kind, ToolErrorKind::UnknownTool);
        for name in TOOL_NAMES {
            assert!(error.message.contains(name), "missing {name}");
        }
    }

    // -- content ----------------------------------------------------------

    #[test]
    fn content_serialises_to_the_mcp_wire_shape() {
        assert_eq!(
            ToolContent::Text("hi".into()).to_json(),
            json!({ "type": "text", "text": "hi" })
        );
        assert_eq!(
            ToolContent::Image {
                base64: "AAAA".into(),
                mime_type: "image/png"
            }
            .to_json(),
            json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" })
        );
    }

    #[test]
    fn or_dash_renders_absent_values() {
        assert_eq!(or_dash(Some("title")), "title");
        assert_eq!(or_dash(Some("")), "—");
        assert_eq!(or_dash(None), "—");
    }

    // -- the protocol layer ------------------------------------------------

    /// An invoker that records what it was asked and answers without a network.
    ///
    /// The protocol layer's job is framing, not fetching, and this keeps the
    /// two testable apart.
    struct Recorder {
        calls: std::sync::Mutex<Vec<(String, Value)>>,
        fail: bool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolInvoker for Recorder {
        async fn invoke(&self, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
            self.calls
                .lock()
                .expect("not poisoned")
                .push((name.to_owned(), arguments));
            if self.fail {
                return Err(ToolError::execution("the site refused the connection"));
            }
            Ok(ToolOutput::text("# Example\n\nbody").with_structured(json!({ "ok": true })))
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn initialize_advertises_tools_and_the_instructions() {
        let recorder = Recorder::new();
        let reply = block_on(handle_message(
            &recorder,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" },
            }),
        ))
        .expect("a reply");
        assert_eq!(reply["id"], json!(1));
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            reply["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(reply["result"]["instructions"]
            .as_str()
            .expect("instructions")
            .contains("session_open"));
    }

    #[test]
    fn a_known_protocol_version_is_echoed_and_an_unknown_one_falls_back() {
        for known in KNOWN_PROTOCOL_VERSIONS {
            assert_eq!(initialize_result(Some(known))["protocolVersion"], known);
        }
        assert_eq!(
            initialize_result(Some("1999-01-01"))["protocolVersion"],
            FALLBACK_PROTOCOL_VERSION
        );
        assert_eq!(
            initialize_result(None)["protocolVersion"],
            FALLBACK_PROTOCOL_VERSION
        );
    }

    #[test]
    fn tools_list_returns_the_catalogue_in_wire_shape() {
        let recorder = Recorder::new();
        let reply = block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ))
        .expect("a reply");
        let tools = reply["result"]["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), TOOL_NAMES.len());
        for (tool, expected) in tools.iter().zip(TOOL_NAMES) {
            assert_eq!(tool["name"], expected);
            assert!(tool["description"].as_str().expect("description").len() > 80);
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["annotations"]["openWorldHint"], true);
        }
    }

    #[test]
    fn tools_call_reaches_the_invoker_and_returns_content() {
        let recorder = Recorder::new();
        let reply = block_on(handle_message(
            &recorder,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "fetch", "arguments": { "url": "https://a.test/" } },
            }),
        ))
        .expect("a reply");
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(reply["result"]["content"][0]["type"], "text");
        assert_eq!(reply["result"]["content"][0]["text"], "# Example\n\nbody");
        assert_eq!(reply["result"]["structuredContent"], json!({ "ok": true }));

        let calls = recorder.calls.lock().expect("not poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "fetch");
        assert_eq!(calls[0].1, json!({ "url": "https://a.test/" }));
    }

    /// A failure the model should read comes back as `isError`, not as a
    /// JSON-RPC error — clients render the latter opaquely.
    #[test]
    fn a_tool_failure_is_a_visible_result_not_a_protocol_error() {
        let recorder = Recorder::failing();
        let reply = block_on(handle_message(
            &recorder,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "name": "fetch", "arguments": { "url": "https://a.test/" } },
            }),
        ))
        .expect("a reply");
        assert!(reply.get("error").is_none());
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(
            reply["result"]["content"][0]["text"],
            "the site refused the connection"
        );
    }

    /// An unroutable request, by contrast, really is a protocol error.
    #[test]
    fn an_unknown_tool_is_a_protocol_error_and_never_reaches_the_invoker() {
        let recorder = Recorder::new();
        let reply = block_on(handle_message(
            &recorder,
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": { "name": "navigate", "arguments": {} },
            }),
        ))
        .expect("a reply");
        assert_eq!(reply["error"]["code"], rpc_error::METHOD_NOT_FOUND);
        assert!(reply["error"]["message"]
            .as_str()
            .expect("message")
            .contains("fetch"));
        assert!(recorder.calls.lock().expect("not poisoned").is_empty());
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let recorder = Recorder::new();
        let reply = block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "id": 6, "method": "resources/list" }),
        ))
        .expect("a reply");
        assert_eq!(reply["error"]["code"], rpc_error::METHOD_NOT_FOUND);
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let recorder = Recorder::new();
        assert!(block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        ))
        .is_none());
    }

    /// Answering a stray response with an error is how two peers talk past
    /// each other forever.
    #[test]
    fn a_response_is_dropped_rather_than_answered() {
        let recorder = Recorder::new();
        assert!(block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "id": 9, "result": {} }),
        ))
        .is_none());
        assert!(block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "id": 9, "error": { "code": -1, "message": "x" } }),
        ))
        .is_none());

        // A request that genuinely forgot its method is still an error.
        let reply = block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "id": 9 }),
        ))
        .expect("a reply");
        assert_eq!(reply["error"]["code"], rpc_error::INVALID_REQUEST);
    }

    #[test]
    fn ping_answers_empty() {
        let recorder = Recorder::new();
        let reply = block_on(handle_message(
            &recorder,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }),
        ))
        .expect("a reply");
        assert_eq!(reply["result"], json!({}));
    }

    #[test]
    fn a_body_that_is_not_json_is_a_parse_error() {
        let recorder = Recorder::new();
        let reply = block_on(handle_body(&recorder, b"{not json")).expect("a reply");
        assert_eq!(reply["error"]["code"], rpc_error::PARSE);
        assert_eq!(reply["id"], Value::Null);
    }

    #[test]
    fn a_batch_answers_only_its_requests() {
        let recorder = Recorder::new();
        let body = serde_json::to_vec(&json!([
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
            { "jsonrpc": "2.0", "id": 8, "method": "ping" },
        ]))
        .expect("serialisable");
        let reply = block_on(handle_body(&recorder, &body)).expect("a reply");
        let replies = reply.as_array().expect("array");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], json!(8));

        // All-notification batches say nothing at all.
        let body = serde_json::to_vec(&json!([
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
        ]))
        .expect("serialisable");
        assert!(block_on(handle_body(&recorder, &body)).is_none());
    }

    // -- the HTTP transport -----------------------------------------------
    //
    // Driven over a real loopback socket rather than through a `tower` service
    // call: the auth guard and the method routing are the point of these tests,
    // and both are properties of the assembled router, not of a handler.

    /// One request/response exchange against a router bound to an ephemeral
    /// port. Returns the status line's code and the parsed body.
    async fn exchange(
        token: Option<&str>,
        method: &str,
        auth: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Composed exactly as `serve.rs` composes it, so the test exercises the
        // mount as well as the handler.
        let app = merged(token);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let authorization = match auth {
            Some(value) => format!("Authorization: Bearer {value}\r\n"),
            None => String::new(),
        };
        let request = format!(
            "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             {authorization}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.expect("read");
        server.abort();

        let status = raw
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status code");
        let body = raw
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        (status, body)
    }

    /// The surface as `serve.rs` composes it: **merged**, not nested, because
    /// `router` owns its absolute path (`API.md` §3a).
    fn merged(token: Option<&str>) -> Router {
        Router::new().merge(router_with(
            Arc::new(Recorder::new()),
            token.map(str::to_owned),
        ))
    }

    /// `router` must serve `/mcp` on its own, with no help from a `nest`.
    ///
    /// Building it is half the assertion — `merge` panics on a router with
    /// unresolved state, which is what §3a's "no unfilled state" rule exists to
    /// prevent. The other half is that a request actually lands, which
    /// `exchange` covers by asking for `/mcp` against this very router.
    #[test]
    fn the_router_owns_its_absolute_path() {
        let _ = merged(None);
        let _ = merged(Some("secret"));
        assert_eq!(MOUNT_PATH, "/mcp");
    }

    #[test]
    fn an_unauthenticated_post_is_answered_when_no_token_is_configured() {
        let (status, body) = block_on(exchange(
            None,
            "POST",
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ));
        assert_eq!(status, 200);
        let reply: Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(
            reply["result"]["tools"].as_array().expect("tools").len(),
            TOOL_NAMES.len()
        );
    }

    #[test]
    fn a_configured_token_is_enforced() {
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

        let (status, _) = block_on(exchange(Some("secret"), "POST", None, request));
        assert_eq!(status, 401, "no header at all");

        let (status, _) = block_on(exchange(Some("secret"), "POST", Some("wrong"), request));
        assert_eq!(status, 401, "wrong token");

        let (status, body) = block_on(exchange(Some("secret"), "POST", Some("secret"), request));
        assert_eq!(status, 200, "right token");
        assert!(body.contains("\"fetch\""));
    }

    #[test]
    fn a_notification_over_http_is_accepted_with_no_body() {
        let (status, body) = block_on(exchange(
            None,
            "POST",
            None,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ));
        assert_eq!(status, 202);
        assert!(body.is_empty());
    }

    #[test]
    fn get_refuses_the_server_initiated_stream_and_delete_is_a_no_op() {
        let (status, body) = block_on(exchange(None, "GET", None, ""));
        assert_eq!(status, 405);
        assert!(body.contains("server-initiated"));

        let (status, _) = block_on(exchange(None, "DELETE", None, ""));
        assert_eq!(status, 204);
    }
}
