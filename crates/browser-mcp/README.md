# browser-mcp

An [MCP](https://modelcontextprotocol.io) server that lets any AI agent fetch,
read and crawl the web through `browser-rs` — plain HTTP when that is enough, a
real headless Chrome when it is not, and named sessions that stay logged in.

Two transports carry **one** tool set:

| Transport | What runs it | Use it for |
|---|---|---|
| **stdio** | the `brs-mcp` binary, spawned as a child process | a desktop agent runtime on the same machine |
| **streamable HTTP** | `brs-serve`, at `/mcp` | a remote or containerised agent, a shared server |

Both serve the same eight tools from the same `browser_server::engine`, so a
session logged in through the dashboard is available over either.

## The tools

| Tool | What it does |
|---|---|
| `fetch` | Fetch a URL over plain HTTP and return it as markdown. Milliseconds, no browser. |
| `browse` | Load a URL in a real headless Chrome and return the *rendered* page as markdown. |
| `extract` | Return only the part you asked for: a CSS selector, an XPath expression, the page as markdown, or its readable text. |
| `crawl` | Walk a site from a seed and return a bounded **index** of pages — URL, depth, status, title, and any selector matches. |
| `screenshot` | Capture a page as a PNG the model can look at — a session's current page, or a URL loaded fresh. |
| `session_open` | Create or reattach a named session with a persistent Chrome profile. |
| `session_list` | List the sessions this server holds and whether each has a live browser. |
| `session_close` | Close a session's browser, keeping its profile — and therefore its login. |

Tool names and their order are stable; agents cache the list, and
`tests/mcp_protocol.rs` fails if either changes.

### Which one to reach for

`fetch` first. It costs milliseconds and handles most articles, documentation,
JSON APIs and feeds — and it still sends a real browser's TLS fingerprint and
header order, so it is not a naive HTTP client.

`browse` only when `fetch` cannot work: content rendered client-side, a page
behind a login, or a `fetch` that came back near-empty or as a challenge page.
It costs a browser process and seconds.

`extract` whenever you already know what you are looking for. Same cost as
`fetch`, a fraction of the output — and it is the right answer to a truncated
response, rather than raising `max_chars`.

`screenshot` only when the *rendering* is the question, never to read text.
Captures are taken at a device scale factor of 2, so a 1280×720 viewport is a
2560×1440 PNG — halve any pixel coordinate read off one to get CSS pixels — and
anything over 4 MiB is refused rather than returned, which `full_page: true` on
a long document will reach.

### Sessions

A session is a **name** plus a persistent Chrome profile directory on the
server. Cookies, local storage and logins survive individual requests *and* a
restart of the server. Reattaching to a name never wipes the profile, and
`session_close` keeps it.

The workflow the design exists for:

1. The agent calls `session_open` with a name, say `github`.
2. A human logs that site in **once**, through the server's dashboard.
3. Every later `fetch`, `browse`, `extract` or `crawl` carrying
   `session: "github"` is authenticated.

An agent cannot type a password and should not ask a user for one; it asks for
the session to be logged in instead.

### Response size

Every text response is capped — 20 000 characters by default, `max_chars` per
call — and a response that was cut says so, in the text the model reads:

```
[truncated: 20000 of 138422 characters shown, 118422 dropped. Narrow the page
with `extract` (a CSS or XPath selector, or kind="text"), or re-run with a
larger `max_chars`.]
```

Nothing is ever silently dropped, and the marker names the cheaper remedy first.

## stdio: the `brs-mcp` binary

Build it:

```bash
cargo build --release -p browser-mcp     # target/release/brs-mcp
```

Then give the agent runtime this block. **Claude Desktop** —
`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS,
`%APPDATA%\Claude\claude_desktop_config.json` on Windows:

```json
{
  "mcpServers": {
    "browser-rs": {
      "command": "/absolute/path/to/target/release/brs-mcp",
      "env": {
        "BROWSER_RS_PROFILE_ROOT": "/absolute/path/to/browser-rs-profiles",
        "RUST_LOG": "browser_mcp=info"
      }
    }
  }
}
```

**Claude Code** — the same object, in `.mcp.json` at the root of a project, or
in one line:

```bash
claude mcp add browser-rs /absolute/path/to/target/release/brs-mcp
```

Other agent runtimes and MCP debugging tools take the same
`command` / `args` / `env` shape; check where yours keeps its config. Use an
**absolute path** for `command`: a spawned server does not inherit your shell's
`PATH` or working directory.

Two things about stdio worth knowing:

- **stdout belongs to the protocol.** `brs-mcp` writes every log line to stderr,
  which the runtime collects. Anything else on stdout would desynchronise the
  client.
- **`BROWSER_RS_API_TOKEN` does nothing here.** The transport is a pipe to the
  process that spawned it; there is no network surface to guard. The token
  guards the HTTP transport.

## Streamable HTTP: `brs-serve`

```bash
BROWSER_RS_API_TOKEN=$(openssl rand -hex 32) \
BROWSER_RS_BIND=0.0.0.0:8080 \
  cargo run --release -p browser-server --bin brs-serve
```

The endpoint is `POST http://<host>:8080/mcp`, with the bearer token:

```json
{
  "mcpServers": {
    "browser-rs": {
      "type": "http",
      "url": "http://127.0.0.1:8080/mcp",
      "headers": {
        "Authorization": "Bearer YOUR_TOKEN_HERE"
      }
    }
  }
}
```

Or, from a shell:

```bash
claude mcp add --transport http browser-rs http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer $BROWSER_RS_API_TOKEN"
```

A quick check without any client — `tools/list` over raw JSON-RPC:

```bash
curl -sS http://127.0.0.1:8080/mcp \
  -H "Authorization: Bearer $BROWSER_RS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools[].name'
```

**If `BROWSER_RS_API_TOKEN` is unset the server binds loopback only** and logs a
warning. That is deliberate: this process drives a real browser, holds your
logins and fetches whatever it is told to, so an unauthenticated one reachable
from another host is an open proxy. Set the token before setting
`BROWSER_RS_BIND` to anything but `127.0.0.1`.

The endpoint answers `POST` with `application/json`. `GET` — the optional
server-initiated SSE stream — returns `405`, because no tool here pushes
notifications; every MCP client falls back to plain POST. `DELETE` succeeds
trivially: the transport keeps no per-connection state.

## Configuration

Both transports read the same environment variables; see
`browser_server::config::ServerConfig::from_env` for the full table. The ones
that matter most here:

| Variable | Default | Meaning |
|---|---|---|
| `BROWSER_RS_PROFILE_ROOT` | a platform data directory | where session profiles — and therefore logins — live |
| `BROWSER_RS_API_TOKEN` | unset | bearer token for `/mcp`; unset forces loopback-only |
| `BROWSER_RS_MAX_BROWSERS` | `3` | concurrent live Chrome processes |
| `BROWSER_RS_SESSION_TTL_SECS` | `900` | how long a session may idle before it is swept |
| `BROWSER_RS_IMPERSONATE` | `chrome` | default fingerprint profile |
| `BROWSER_RS_PROXY` | unset | default egress proxy |

Point both transports at the same `BROWSER_RS_PROFILE_ROOT` and they share
sessions.

## How it fits together

The tool catalogue, the schemas, argument parsing, truncation and the dispatch
onto the engine all live in **`browser_server::mcp`** — deliberately, and not in
this crate. `browser-mcp` depends on `browser-server`, so the shared half cannot
live here without a dependency cycle, and `browser-server` does not depend on
`rmcp`. So:

```
browser_server::engine            the operations, shared with the REST API
        │
browser_server::mcp               tools, schemas, truncation, dispatch,
        │                         and the /mcp HTTP transport
        ├──────────────► /mcp     (axum, hand-rolled JSON-RPC)
        │
browser_mcp::server               the same tools, adapted to rmcp
        └──────────────► stdio    (brs-mcp)
```

A tool described twice eventually gets described differently. There is one
description of each tool, and both transports read it.
