# Running `browser-rs` as a server

One container, one port. It gives you a REST API, an MCP endpoint for AI agents,
and a dashboard where you can watch a live browser and take the controls.

> **Status:** the Dockerfile in this repository has been written and reviewed but
> **has not been built**, because no Docker daemon was available in the
> environment where it was authored. Treat the first `docker build` as
> unverified. The server itself runs fine outside Docker.

## Quick start, without Docker

```bash
cargo run --release -p browser-server --bin brs-serve
```

Then open <http://127.0.0.1:8080/>. With no `BROWSER_RS_API_TOKEN` set the server
binds loopback only and logs a warning — that is deliberate.

## With Docker

Create a `.env` next to `docker-compose.yml`:

```dotenv
BROWSER_RS_API_TOKEN=<openssl rand -hex 32>
# BROWSER_RS_PROXY=http://user:pass@host:3128
```

```bash
docker compose up --build
```

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `BROWSER_RS_API_TOKEN` | *(unset)* | Bearer token for `/api` and `/mcp`. **Unset ⇒ loopback-only bind.** |
| `BROWSER_RS_BIND` | `127.0.0.1:8080` | Listen address. |
| `BROWSER_RS_PROFILE_ROOT` | *(temp dir)* | Where per-session Chromium profiles live. Mount it to keep logins. |
| `BROWSER_RS_PROXY` | *(unset)* | Default egress proxy for fetches and browser sessions. |
| `CHROME` | `/usr/bin/chromium` in the image | Browser binary. Read by the CDP crate's executable detection. |
| `RUST_LOG` | `info` | Standard `tracing` filter. |

## Pointing an AI agent at it

**Over stdio** — what a desktop agent runtime spawns:

```json
{
  "mcpServers": {
    "browser-rs": {
      "command": "brs-mcp",
      "env": { "BROWSER_RS_PROFILE_ROOT": "/var/lib/browser-rs/profiles" }
    }
  }
}
```

**Over HTTP** — for a remote or containerised agent:

```
http://127.0.0.1:8080/mcp        Authorization: Bearer <token>
```

Plain HTTP clients can use `/api/*` directly; `GET /api/openapi.json` describes
the whole surface, which is usually enough to hand an agent on its own.

## Staying logged in

This is the part worth understanding, because it is what makes the rest useful.

Every **named session** owns its own Chromium profile directory under
`BROWSER_RS_PROFILE_ROOT`. Cookies, local storage and logins live there, so they
survive both separate API calls and a `docker restart` — provided that directory
is on a volume, which the compose file arranges.

So the flow is:

1. Ask the agent (or the API) to open a session by name, e.g. `shop`.
2. Open the dashboard, pick that session, and press **Take control**.
3. Log in by hand — real mouse, real keyboard, into the real page.
4. Press **Release control**.
5. Every later call that passes `session: "shop"` is already authenticated.

Nothing about your password passes through the agent, and you never have to give
it credentials.

## Choosing your exit address

`BROWSER_RS_PROXY` sets a default for everything; individual calls can override
it per request or per session. The proxy applies to both the HTTP fetch path and
the browser path, so a session's traffic leaves from one consistent address.

Browser fingerprint is controlled the same way: an impersonation profile and a
user agent, set per session or per request. The TLS/JA3-JA4 layer underneath is
`browser-http`'s, and it is applied whether or not a browser is involved.

Note that "IP spoofing" here means **choosing and rotating the egress proxy**.
Nothing forges packets; that is neither possible from userspace nor useful over
TCP.

## Chromium sandboxing

Chromium's own sandbox relies on user namespaces, which a default container does
not provide. There are two honest options and you should pick deliberately:

- **`seccomp=unconfined` (what the compose file ships).** Chromium's internal
  sandbox still runs; you have relaxed the *container's* seccomp filter. The
  container remains the security boundary, and the process runs as a non-root
  user. This works everywhere.
- **Host user namespaces + a Chromium seccomp profile.** Stricter, and preferable
  if you are exposing this beyond your own machine, but it depends on host
  configuration this repository cannot set for you.

What you should **not** do is reach for `--no-sandbox` as a first move. It
disables Chromium's own renderer sandbox, which is the thing standing between a
hostile page and the rest of the container.

## Security

Be blunt about what this is: a service that **fetches arbitrary URLs and runs a
real browser on their content**, with an interface that lets a caller drive it.

- Do not expose it to the internet. The compose file publishes to `127.0.0.1` on
  purpose. If you need it on a network, put it behind a reverse proxy with TLS
  and keep the bearer token.
- Set `BROWSER_RS_API_TOKEN`. Without it the server refuses non-loopback binds,
  but a token is what protects you once you change the bind.
- The dashboard is operator UI. Anyone who reaches it can drive your
  authenticated browser sessions — which is exactly as much access as the person
  who logged in has.
- Session profiles contain live credentials. The volume holding them deserves the
  same care as a password store.
- Server-side request forgery is inherent to the job. If the host can reach
  private network ranges, so can anything that can call this API.

## CI

`.github/workflows/ci.yml` runs fmt, clippy `-D warnings`, and the test suite with
`BROWSER_RS_SKIP_BROWSER_TESTS=1`, plus a Docker build job.
