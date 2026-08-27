# `browser-server` — the contract

Four agents build against this document in parallel. Where it is specific, follow
it exactly rather than improving on it; where it is silent, use judgement and
record the decision in your report.

## 1. `engine` — the shared core

Everything lives here. `api.rs` and `mcp.rs` are thin translation layers over it.

```rust
pub struct Engine { /* SessionRegistry, ServerConfig, default egress */ }

pub struct OpenOpts {
    pub session: Option<String>,   // named, persistent. None = stateless.
    pub proxy: Option<Proxy>,      // else server default, else rotator
    pub impersonate: Option<String>,
    pub user_agent: Option<String>,
    pub headless: bool,
}
```

Operations, each returning a typed result and each callable from REST **and** MCP:

| Operation | Notes |
|---|---|
| `fetch(url, opts)` | HTTP only. Fast, no browser. |
| `browse(url, opts, wait)` | Real Chrome. `wait` = load / domcontentloaded / networkidle / selector. |
| `extract(source, query)` | `query` = CSS, XPath, `markdown`, or `text` (AI-cleaned). |
| `crawl(seed, limits)` | Bounded: `max_pages`, `max_depth`, same-host default, robots respected. |
| `screenshot(url_or_session, opts)` | PNG bytes, optionally full-page. |
| `session_open/list/close` | See below. |

### Sessions

A session is a **name** plus a **persistent profile directory** under
`ServerConfig::profile_root`, so cookies and logins survive both requests and
process restarts. That persistence is the point of the whole design: a human logs
in once through the dashboard and the agent stays authenticated afterwards.

Sessions need an idle **TTL** and a **cap** on concurrent live browsers — each one
is a real Chrome process. Reattaching by name must not wipe the profile.

## 2. REST — `/api`

All routes require `Authorization: Bearer <BROWSER_RS_API_TOKEN>` when the token
is configured. JSON in, JSON out. Errors: one error type, mapped to status codes,
never a panic and never a leaked internal path.

| Method | Path | Body / query |
|---|---|---|
| `GET` | `/api/health` | — (unauthenticated) |
| `GET` | `/api/openapi.json` | hand-written description of this surface |
| `POST` | `/api/fetch` | `{url, session?, proxy?, impersonate?, headers?}` |
| `POST` | `/api/browse` | `{url, session?, wait_until?, wait_for_selector?, timeout_ms?}` |
| `POST` | `/api/extract` | `{url?, session?, html?, selector?, kind: "css"\|"xpath"\|"markdown"\|"text"}` |
| `POST` | `/api/crawl` | `{seed, max_pages, max_depth, same_host?, selector?}` |
| `POST` | `/api/screenshot` | `{url?, session?, full_page?}` → PNG |
| `GET` | `/api/sessions` | list |
| `POST` | `/api/sessions` | `{name, proxy?, impersonate?, user_agent?}` |
| `DELETE` | `/api/sessions/{name}` | close |

Also: request timeouts and body-size limits via `tower-http`.

## 3. MCP — `/mcp` and `brs-mcp`

Same operations as tools. Two transports, one tool set:

- **stdio** — the `brs-mcp` binary, spawned by a desktop agent runtime.
- **streamable HTTP** at `/mcp`, mounted by `browser-server`, same bearer token.

Tool design is most of the value:

- Every tool needs a **JSON schema with a description on every field**. Agents
  choose tools by reading them. Say what `selector` accepts, that `crawl` is
  bounded and what the defaults are, and when to prefer `browse` (JS-rendered or
  logged-in pages) over `fetch` (fast, cheap, no browser).
- **Return what an LLM can use**: default to markdown or cleaned text, not raw
  HTML, and **cap the response** with truncation the model can see rather than
  returning a megabyte of markup.
- Explain the session story in the descriptions: the same `session` name keeps
  cookies and logins, and a human can log in once via the dashboard.
- Errors are tool errors with a useful message.

## 4. Screencast and input (`browser-cdp`)

```rust
// browser_cdp::screencast
pub struct Frame {
    pub data: String,        // base64, as CDP delivers it
    pub format: Format,      // Jpeg | Png
    pub metadata: FrameMetadata, // device width/height, page scale, scroll offset
}
pub struct ScreencastOpts { format, quality, max_width, max_height, every_nth_frame }

impl CdpPage {
    pub async fn start_screencast(&self, opts: ScreencastOpts)
        -> Result<impl Stream<Item = Frame>>;
    pub async fn stop_screencast(&self) -> Result<()>;
}
```

Three things that will bite:

1. **Every frame must be acknowledged** with `Page.screencastFrameAck` carrying
   that frame's `sessionId`, or Chrome stops sending after a couple of frames.
   This is the single most common way to get screencast wrong.
2. Buffering is **bounded** and **drops stale frames** rather than growing — a slow
   websocket client must not become a memory leak. Document the policy chosen.
3. Default to JPEG at modest quality; these frames cross a websocket.

```rust
// browser_cdp::input
impl CdpPage {
    pub async fn dispatch_mouse(&self, ev: MouseEvent) -> Result<()>;
    pub async fn dispatch_key(&self, ev: KeyEvent) -> Result<()>;
    pub async fn insert_text(&self, text: &str) -> Result<()>;
}
```

`Input.dispatchMouseEvent` (`mousePressed`/`mouseReleased`/`mouseMoved`/
`mouseWheel`, button, `clickCount`, modifiers) and `Input.dispatchKeyEvent`
(`keyDown`/`keyUp`/`char` with `text`, `key`, `code`, `windowsVirtualKeyCode`).
**Real typing needs the `text` field on a `char` event** — a bare `keyDown` does
not insert characters.

**Coordinate space:** frames carry their device metadata, and the dashboard sends
coordinates in **rendered-image space**. The *server* (`ws.rs`) scales them to page
space using the metadata of the most recent frame before calling `dispatch_mouse`.
`browser-cdp` receives page-space coordinates only.

## 5. Dashboard websocket — `/ws?session=NAME`

Tagged JSON, both directions.

Server → client:
```jsonc
{"t":"frame","data":"<base64>","format":"jpeg","w":1280,"h":720}
{"t":"status","session":"shop","url":"https://…","browser":true}
{"t":"action","at":"…","kind":"browse","detail":"…"}   // engine activity log
{"t":"error","message":"…"}
```

Client → server:
```jsonc
{"t":"mouse","kind":"down","x":410,"y":233,"button":"left","clicks":1}
{"t":"key","kind":"char","text":"a","key":"a","code":"KeyA"}
{"t":"control","take":true}    // watching  <->  controlling
```

Control is **explicitly toggled**, defaulting to watching — a stray click while the
agent is working is a real hazard. Backpressure: drop frames, never queue.
