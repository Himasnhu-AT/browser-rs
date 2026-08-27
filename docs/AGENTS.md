# Working in this repo

Read this before writing code. It is the shared contract; your task prompt only
adds what is specific to your piece.

## Rule 0 — naming, and it is absolute

This project is **browser-rs**. Crates: `browser-core`, `browser-dom`,
`browser-fingerprint`, `browser-selectors`, `browser-xpath`, `browser-storage`,
`browser-markdown`, `browser-parser`, `browser-http`, `browser-spider`,
`browser-cdp`, `browser-templates`, `browser-cli`, `browser-server`,
`browser-mcp`, and the façade `browser-rs` (library name `browser`, so users
write `use browser::Document;`). Binaries: `brs`, `brs-serve`, `brs-mcp`.

This repository's history was deliberately scrubbed of the name of a third-party
project whose behaviour some of this library was originally modelled on. **Never
write that name, or any other third-party project name, anywhere** — not in code,
comments, docs, commit messages, test names, fixture filenames or user-visible
strings. Existing comments say "the reference implementation"; keep that phrasing.
This is checked with `git grep -i` before any wave is accepted.

## Toolchain — settled, do not re-litigate

- rustc is pinned to **1.97.0**; MSRV is 1.97.0.
- crates.io **is** reachable. You may add dependencies.
- `wreq` is pinned at **0.15.3** because 0.16 needs rustc 1.98. Do not bump it.
- Already wired and probe-compiled on 1.97.0: **axum 0.8.9** (`ws`, `macros`),
  **rmcp 2.2.0** (`server`, `transport-io`), **tower-http 0.6.11**,
  **tokio-tungstenite 0.28**, **uuid 1**, **criterion 0.8.2**.
- Member manifests use `<key>.workspace = true` and never a version literal.

## What already exists — build on it, do not rebuild it

- **Fingerprint control is done.** `browser-http` does TLS/JA3-JA4 impersonation
  through `wreq`; `browser-fingerprint` holds the profiles, the header-order
  sampler and the launch-arg constants. `BrowserConfig` takes a `useragent`.
- **Egress control is done at the type level.** `browser_core::types::Proxy`,
  `BrowserConfig::proxy()`, `BrowserConfig::proxy_rotator(Arc<dyn ProxyRotation>)`.
  "IP spoofing" in this project means **choosing and rotating the egress proxy**.
  Nothing operates at the packet level and nothing should.
- **Auth persistence has its foundation.** `BrowserConfig::user_data_dir` is a
  persistent Chromium profile (`browser-cdp/src/flags.rs`). A profile directory
  that survives a restart is how a logged-in session stays logged in.
- **`browser_http::response::Response` is the one response type** in the
  workspace. `browser-cdp` builds browser responses through it. Do not add a second.
- Contracts worth reading: `crates/browser-parser/API.md`,
  `crates/browser-spider/API.md`, `crates/browser-templates/API.md`,
  `crates/browser-dom/API.md`, `crates/browser-server/API.md`.
- `crates/browser-rs/tests/integration.rs` holds 34 cross-crate seam tests.
  `unit_20_facade_seam` is four end-to-end journeys written only against `browser::`.

## Server layer shape

One process, one port (default **8080**), three surfaces over one engine:

| Path | Surface |
|---|---|
| `/api/*` | REST, for any HTTP client |
| `/mcp` | MCP streamable HTTP, for AI agents |
| `/`, `/ws` | operator dashboard and its live-view socket |

Every operation lives in `browser_server::engine`, never in a route handler,
because the REST layer and the MCP layer must call the same code.

Auth: a bearer token from `BROWSER_RS_API_TOKEN`. **If it is unset, bind loopback
only and log a warning** — never silently expose an unauthenticated server that
drives a real browser and fetches arbitrary URLs.

## Purpose

This is a self-hosted toolkit the repository owner runs themselves so their own AI
agents can browse and scrape on their behalf. Anti-bot evasion is the established
premise of this library. Build it as ordinary engineering. Do not add scanning,
credential-testing, mass-targeting or attack tooling — none of that is wanted.

## House rules

**File ownership.** Every crate is pre-scaffolded: module files exist, `pub mod`
is declared, dependencies are wired. You fill in files. Your prompt names the
paths you own — do not write outside them, and **never edit `Cargo.toml` or the
`pub mod` lines in a `lib.rs`**. If you need something outside your paths, say so
in your report instead of doing it.

**Do not `git add` or `git commit`.** Agents share one working tree; concurrent
commits corrupt the index. A checkpoint step commits after each phase.

**Do not make decisions that belong to the owner** — licensing, dependency
version bumps, anything outward-facing (pushing, publishing, opening PRs). If your
prompt mentions an open question as background, that is context, not a task.

**Tests must be self-contained.** No public network: use a loopback fixture
(copy `local_site::Site` from the façade's `tests/integration.rs`). Browser tests
must honour `BROWSER_RS_SKIP_BROWSER_TESTS` and print a skip notice when Chrome is
absent. Anything launching Chrome inside `browser-cdp` must take a permit from
`live_u14::LIVE_BROWSERS` — a 3-permit semaphore, because cold-starting six
Chromes at once really does fail.

**Corpus honesty.** Never delete a golden row, shrink a corpus, or loosen a
comparison to raise a pass rate. Encode an unreproducible case as a declared
divergence that *fails if it ever starts passing*, with a written reason.

**Style.** Rustdoc every public item (`#![warn(missing_docs)]`). Match the
surrounding comment density and idiom; read a neighbouring finished crate first.
Comments should explain *why*.

## The gate

```bash
cargo build --workspace --all-features
BROWSER_RS_SKIP_BROWSER_TESTS=1 cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
```

All green, **nothing failing and nothing newly `#[ignore]`d**. The suite is at
**2237 passing**; it must not go down.

`cargo deny check` reports `licenses FAILED` for a GPL-3.0 transitive crate. That
is a known open question for a human. Leave it red.
