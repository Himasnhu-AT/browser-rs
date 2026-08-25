# Handoff — the reference implementation → Rust port

**Date:** 2026-08-25
**Repo:** `/Users/himanshu/codes/browser-rs` (branch `main`)
**Upstream being ported:** [Himasnhu-AT/browser-rs](https://github.com/Himasnhu-AT/browser-rs) v0.4.15 (~16k lines of Python)

---

## TL;DR — where things stand

| | |
|---|---|
| Build | `cargo build --workspace --all-features` ✅ green |
| Tests | **1529 passing, 0 failing, 2 ignored** (same under `--all-features` and `--no-default-features`) |
| Clippy | `--workspace --all-targets -D warnings` ✅ clean with default, `--all-features` and `--no-default-features` |
| Format | `cargo fmt --all --check` ✅ clean |
| Code written | ~81k lines across 14 crates (~58k of it `src/`) |
| Plan progress | **15 of 20 work units done** (units 1–13, 15, 16) |
| Last commit | see `git log` — wave 4 is the commit titled *"Wave 4: the HTTP driver, spider support services, and the unit-14/17 seams"* |

The two ignored tests are both regeneration aids, not checks, and are meant to stay ignored:
`browser-markdown`'s `emit_rust_answers` (reads `$MD_CASES`, writes `$MD_OUT` for
`tools/corpus/mdgold/step2_golden.py`) and one `ignore`d doc fence.

Commits so far:

```
(wave 4)  Wave 4: the HTTP driver, spider support services, and the unit-14/17 seams
d78c06e   handoff: record the wave-3 commit hash
3824cbb   Wave 3: the public document API, the spider data plane, and the Response seam
9a29998   handoff: mark the two preservation TODOs done
e837c0b   Preserve specs, corpus generators and the session handoff in-repo
8228e99   Wave 2: selectors, XPath, storage, markdown, HTTP policy, browser config
8cc1f38   Units 2/3/4/9: core, dom and fingerprint foundation crates
fe30e17   Unit 1: workspace scaffolding for the Rust port of the reference implementation
```

Working tree is clean. Nothing is red. Pick up at **Unit 14** (the CDP browser driver) —
**unit 17** (the crawler engine) is unblocked and parallel-safe with it.

---

## Read these first (in this order)

1. **`docs/specs/ARCHITECTURE.md`** — 87 KB. Crate layout (§1), every dependency with justification
   (§2), the parse+select design (§3), adaptive relocation (§4), async/error/API ergonomics (§5),
   **the 20-unit plan (§7)**, out-of-scope (§8), the tree-shape sign-off (§11.6).
2. **Subsystem specs** — `docs/specs/`: `parser.md`, `core-types.md`, `http-fetch.md`, `browser.md`,
   `toolbelt.md`, `spider-core.md`, `spider-support.md`, `cli-ai.md` (60–90 KB each, generated from
   the Python source; the implementers work from these, not from the Python).
3. **`crates/browser-parser/API.md`** — the public `Document`/`Node`/`Nodes` surface, and
   **`crates/browser-spider/API.md`** — the crawl core, now including `throttle`/`robots`/`cache`
   and the `Response` ⇄ `FetchedResponse` seam. Every crate from unit 12 onwards codes against these.
4. **`crates/browser-dom/API.md`** — the DOM cheat sheet underneath it.
5. **`crates/browser-rs/tests/integration.rs`** — the 18 cross-crate seam tests, and the best map of
   how the crates fit together. `response_seam` is the executable spec unit 12 was written against;
   `browser_response_seam` is unit 14's, and `crawl_loop_seam` is unit 17's — the latter runs a real
   crawl (robots → cache → throttle → fetch → parse → enqueue) against a loopback server.
6. **`docs/DIVERGENCES.md`** — every intentional behavioural difference from Python, in one place.

### The reference implementation is installed — use it

```
/Users/himanshu/.claude/jobs/a62e4b8e/tmp/pyref/bin/python
```

A venv with **the reference implementation, lxml 6.1.2, cssselect, markdownify, w3lib**. This is what made
wave 2 trustworthy: instead of reasoning about what Python does, run it and diff. Every corpus in
`crates/*/testdata/*.jsonl` was generated from it.

```bash
PYREF=/Users/himanshu/.claude/jobs/a62e4b8e/tmp/pyref/bin/python
$PYREF -c "from reference.parser import Selector; print(Selector('<p>hi</p>').css('p::text').get())"
```

⚠️ This venv is in a **job temp dir**. If it disappears: `python3 -m venv pyref && ./pyref/bin/pip
install lxml cssselect markdownify w3lib the reference implementation`. The Python source is then readable at
`<venv>/lib/python3.14/site-packages/browser-rs` (or re-clone Himasnhu-AT/browser-rs from GitHub).

All 69 corpus generator scripts (15 directories) are in-repo at `tools/corpus/` with a `README.md` mapping each directory to
the `testdata/` it regenerates. `u12/` and `u16/` were added in wave 4 and both carry their own
`README.md`; `u12/`'s endpoint table is *also* interpreted by the Rust fixture server, so the two
implementations cannot drift. Their path constants were fixed for `mdgold` and `xconf` during wave 3;
the rest may still carry absolute paths from an older session — check before re-running.

---

## What exists today

| Crate | LOC | Status | What it is |
|---|---:|---|---|
| `browser-core` | 4338 | ✅ done | `Override<T>`, `TextExt` (Python `findall` group-flattening), w3lib entity port + cp1252 remap, `AttributeMap`, `round_half_even`, cookie/header parsing, tracing `LevelCounter` — **plus** a bit-exact CPython `difflib.SequenceMatcher` port, `ElementFingerprint`, `similarity_score`, `dict_diff` |
| `browser-dom` | 4585 | ✅ done | html5ever → ego-tree arena, `ParseOptions` with the libxml2-compat passes (incl. libxml2's real `areBlanks()`), byte bodies **decoded with the caller's encoding**, `NodeRef` navigation, lxml `text`/`tail` view, libxml2-faithful serialization, edit ops |
| `browser-fingerprint` | 2825 | ✅ done | impersonation profiles + aliases, precomputed header sampler (replaces browserforge), `AD_DOMAINS` as a build.rs `phf::Set`, `StatusText`, launch-arg constants |
| `browser-selectors` | 5847 | ✅ done | native CSS via Servo `selectors`+`cssparser`, `::text`/`::attr()`/`:contains()`, selector-group splitting for adaptive identifiers |
| `browser-xpath` | 8039 | ✅ done | **hand-written XPath 1.0 engine** — 13 axes, 27 functions, full type coercion, over a `TreeModel` trait |
| `browser-storage` | 6147 | ✅ done | `StorageSystem` trait, `SqliteStorage` (schema byte-identical to Python's, DBs interoperate both directions), `registrable_domain`, `identifier_hash` |
| `browser-markdown` | 3651 | ✅ done | markdownify port at the reference implementation's defaults, `strip_noise_tags`, `sanitize_for_ai` |
| `browser-parser` | 7992 | ✅ done | **unit 10** — the public `Document`/`Node`/`Nodes` API, `FindQuery`, `find_similar`, `find_by_text`/`_regex`, the four selector generators, the adaptive save/relocate branch. `API.md` is the downstream contract |
| `browser-http` | 15473 | ✅ done | **units 11 + 12** — policy (config/merge/headers_job/retry/proxy), the `HttpBackend` trait + `wreq` driver, the hand-rolled `follow_redirects="safe"` SSRF guard, `Response` (lazy parse, `OnceLock`), `ResponseBuilder`/`ResponseFactory`, the RFC 6265 `CookieJar`, `AsyncFetcherSession`/`AsyncFetcherClient`/`AsyncFetcher` and the blocking façade |
| `browser-spider` | 13569 | 🟡 **all but the engine** | **units 15 + 16** — `Request` + SHA-1 fingerprint, `canonicalize_url`, `Scheduler`, `SessionManager` + `CrawlSession`, `CrawlStats`, `ItemList` exporters, `CheckpointManager`, `AutoThrottle` + `Retry-After`, the `protego` port + `RobotsTxtManager`, `ResponseCacheManager`, and the `Response` ⇄ `FetchedResponse` conversions. **`engine.rs` and `spider.rs` (unit 17) are still stubs** |
| `browser-cdp` | 6687 | 🟡 **half** | Unit 13 (config/flags/pool/route) done. **`cdp/`, `session.rs`, `response.rs`, `cloudflare.rs` are still stubs** |
| `browser-templates` | 49 | ❌ stub | Unit 18 |
| `browser-cli` | 49 | ❌ stub | Unit 19 |
| `browser-rs` (façade) | 2074 | 🟡 partial | 18 cross-crate seam tests, all passing; the re-export surface is unit 20 |

**~81k lines across 14 crates** (~58k of it `src/`).

All 14 crates are **pre-scaffolded**: every module file exists, every `pub mod` is declared, every
dependency is wired in `[workspace.dependencies]`. Implementers fill in files; they never touch
`Cargo.toml` or `lib.rs`. Keep that discipline — it is what let six agents work the same repo in
parallel without a single merge conflict. (The per-wave *green* agent may touch them, and did twice
in wave 3: `encoding_rs` was added to `browser-dom`, and the façade's `[dev-dependencies]` grew
`browser-parser/adaptive` + `browser-spider` so the seam tests run under a plain `cargo test`.)

---

## Fidelity achieved so far

Every finished unit was adversarially differential-tested against the real Python:

- **XPath: 3077/3077 (100%)** against lxml/libxml2 — 14 documents, 756 node-set / 1707 number /
  394 string / 144 boolean / 76 rejection cases. 80 rows are *declared* divergences with reasons
  (85 before the `areBlanks()` fix).
- **CSS: 3933/4159 (94.6%)** against cssselect+lxml. All 226 remainders are itemised in two
  divergence tables (the test **fails if any of them starts passing**). A control run against
  shape-normalised pages gets 1371/1372 — proving the bulk really is html5ever-vs-libxml2 tree shape,
  not matcher bugs. Random-selector fuzzing (~10k comparisons, 3 seeds) found zero new classes.
  The rate moved *down* from 3948/4159 on purpose: the `areBlanks()` fix let the blanket
  "drop whitespace-only text results from both sides" filter be deleted, which exposed 20 rows of a
  real end-of-document tree-shape difference that the filter had been hiding. 5 rows were fixed
  outright and 4 were re-labelled to their true cause.
- **Markdown: 265/279 cases byte-exact on both arms** (258 before the fix), 0 differing from
  markdownify on an identical tree.
- **Parser (unit 10) — two corpora, 10496 rows, 10033 reproduce (95.6%).**
  `parser_golden.jsonl` (3488 rows, 96.3%) pins the *algorithms*: `element_to_dict`,
  `similarity_score`, `dict_diff`, the difflib port and `round_half_even`, including ten whole-page
  relocation rows that carry Python's entire per-element score table. `parser_api_golden.jsonl`
  (7008 rows, 95.2%) pins the *public API*: 17 pages × css/xpath/find_all/find_similar/find_by_text/
  selector-generation/navigation/`Nodes`, plus **23 end-to-end adaptive scenarios through a real
  SQLite database — all 23 reproduce, score tables included.** The 333 misses are itemised one by
  one in `crates/browser-parser/tests/support/divergences.rs`; **not one** is an algorithm
  difference, they are all `browser-dom` tree shape or libxml2 serialisation convention.
- **Spider data plane (unit 15): 2352/2358 (99.75%).** 584 `canonicalize_url` rows exact against
  w3lib (plus the 4 where w3lib itself raises, pinned rather than skipped), 1605 fingerprint digests
  across 100 hand-written + 1500 seeded-fuzz rows, 22 scheduler replays, 22 `SessionManager`, 20
  `CrawlStats`, 83 byte-identical exports, 21/23 checkpoint.
- **HTTP driver (unit 12): 2271/2271 field checks (100%)** across 188 corpus rows / 210
  observations against `the reference implementation` + `curl_cffi==0.16.1`, driven over a two-port loopback
  fixture. 15 rows are *declared* divergences (each asserted to still differ) and 2 fields get an
  explicit either-of allowance because the **host** decides them. It started at 1925/2060 (93.45%)
  and the gap was nine real bugs — the largest being the `"safe"` SSRF guard refusing *every*
  redirect served by a private host (curl only checks a hop that needs a new connection, so the
  guard now exempts same-origin hops), `Response.cookies` being jar-matched instead of built from
  the final reply's `Set-Cookie` lines, a 301/302/303 keeping its request body, and `Authorization`
  not actually being stripped cross-origin because `auth` lives outside the headers.
  The Rust fixture server **interprets the same endpoint JSON the Python one does**, so replaying
  the corpus needs neither Python nor a network.
- **Spider support services (unit 16): 172,311/172,447 assertions (99.92%)** over 6,322 corpus rows
  in nine files, against `the reference implementation` + `protego` + CPython 3.14. Per corpus: `Retry-After`
  2858/2858, AutoThrottle 873 ops + 167 final-delay dicts + 167 constructor outcomes all bit-exact,
  `protego` 166,839/166,848, `RobotsTxtManager` 387/392 + 21/21 cache-keying, `bytes.decode`
  891/1000 (109 rows in nine declared codecs), cache `put` 25/25 byte-identical to `orjson`, cache
  `get` 42/55, engine helpers 41/41. Out-of-corpus confidence runs (generated, run, reverted):
  47,858 `Retry-After` rows → 0 mismatches; 553,948 `protego` assertions over 7,012 groups → only
  the same 9 declared keys. Eleven bugs fixed, including CPython's `str.isspace()` covering
  `\x1c`–`\x1f`, `_PyUnicode_TransformDecimalAndSpaceToASCII` (so `float("٣.٥") == 3.5` — a
  previously *declared* divergence, now implemented), and a from-scratch port of
  `email._parseaddr._parsedate_tz` that fixed eight classes of wrongly rejected HTTP date.
- **Storage:** identical SQLite schema; a DB written by Python reads in Rust and vice-versa.
- **difflib:** f64 bit-equality against CPython on a generated corpus.
- **Markdown / HTTP policy / browser config:** golden corpora generated from the reference interpreter.

Real bugs this caught (a sample): `:lang(EN)` never matching because cssselect lowercases *both*
sides; attribute values being compared case-insensitively when cssselect emits a case-sensitive XPath
string test; `xlink:href` being seen as `href` (in **both** XPath evaluation paths — the fast path's
agreement test had been passing because both sides were wrong); `round(-0.5)` losing the sign of
zero; the trailing-bare-`*` rewrite in the reference implementation's own translator (`div *::text`) that a fuzzer
found; `<!DOCTYPE>` leaking into `Node::next`/`previous`/`children`; four separate ways the attribute
serialiser followed html5ever instead of libxml2; the scheduler collapsing `i32::MIN` and
`i32::MIN + 1` into one queue key; case-colliding session kwargs resolving in the wrong order; an
empty `BytesIO` body being treated as absent (it is *truthy* in Python, and an empty `bytes` is not);
and — in the wave-3 integration pass — `ParseOptions::encoding` being a label the decoder ignored.

---

## TODO — remaining work, in order

### ~~Unit 10 — `browser-parser`~~ ✅ **done** (wave 3)
The public `Document`/`Node`/`Nodes` API, verified against 10496 corpus rows.
**`crates/browser-parser/API.md` is the contract every crate from here on codes against —
read it before writing anything downstream.** It also lists the 333 declared divergences by cause and
documents the nine deliberate API deviations (`xpath()` rejects a scalar; `relocate` has no
`selector_type`; `::text`/`::attr()` are optional because `Nodes::texts()`/`attrs()` exist; …).

### ~~Unit 12 — `browser-http` driver, `Response`, sessions~~ ✅ **done** (wave 4)
`backend/{mod,wreq}.rs`, `response.rs`, `session.rs`, `redirect_safe.rs`, `blocking.rs` —
2271/2271 corpus field checks, 305 in-crate tests. What downstream units must know:

- **`Response` is the only response type in the workspace.** `Response::builder(status, url)`
  takes a `String` or `Bytes` body, headers, an explicit `encoding`, `history: Vec<RedirectHop>`,
  a `SelectorConfig` and an optional `DocumentHook` (which is how adaptive storage is injected).
  Unit 14 builds its browser responses through it — proven by
  `crates/browser-rs/tests/integration.rs::browser_response_seam`.
- **Two charset regexes, deliberately.** `charset_from_content_type` is the reference implementation's
  `__CHARSET_RE__` (accepts a quoted value) and is the **browser** one unit 14 wants;
  `charset_from_content_type_http` is `curl_cffi`'s stricter one and is what the HTTP path uses,
  because `curl_cffi` is what resolved the charset in Python.
- **`Response.url` is the fetched URL**; `adaptive_domain` replaces it only as the storage key,
  readable via `Response::selector_url()`. `Response::body_text()` is the decoded body —
  there is deliberately no `text()`, because Python's means something else.
- **`follow_redirects="safe"`** exempts same-origin hops (scheme+host+port) from the
  private-address check, because curl implements `CURLFOLLOW_SAFE` inside connection setup and a
  reused connection is never re-checked. One measured difference remains, declared as corpus row
  `redirect-close-safe`.
- **Still open, cheap:** a non-canonical reason phrase (`299 Totally Fine`) is lost because it
  lives in `hyper::ext::ReasonPhrase` and `wreq` does not re-export it. Adding `hyper` to
  `crates/browser-http/Cargo.toml` closes rows `status-299` and `status-222`.
- **Untested by the corpus and out of reach of a loopback fixture:** proxies, TLS/`verify=false`,
  client certificates, HTTP/3, and the impersonation fingerprint itself (profiles are exercised,
  but the JA3/JA4 bytes are never compared).

### Unit 14 — `browser-cdp` CDP driver + Cloudflare solver
*Deps: 13, 12. Not parallel. The hardest unit left.*
Files: `cdp/{mod,networkidle,fetch_domain,isolated_world,history}.rs`, `session.rs`, `response.rs`, `cloudflare.rs`
- chromiumoxide session, three launch modes (CDP / rotation / persistent)
- **reimplement Playwright primitives on raw CDP**: `networkidle` from Network events, `Fetch`-domain
  interception replacing `page.route`, `Page.createIsolatedWorld`
- the per-request page reset sequence whose *call counts* are pinned by `test_page_reuse.py`
- redirect history from `redirectResponse`; response + XHR capture
- Cloudflare Turnstile solver with all its magic numbers (5000 ms idle budget, 3 attempts, 20×500 ms,
  100×100 ms, click offsets x+26..28 / y+25..27, hold 100..200 ms)
- **deliberate deviation:** Python's `frame_element().is_visible()` loop is unbounded; give it the
  request `timeout` as a deadline — an unbounded loop in an async task is a hang, not a slow request

**The response half is already settled — do not write a second `Response`.**
`crates/browser-rs/tests/integration.rs::browser_response_seam` builds a browser-shaped response
out of exactly what CDP gives you (a rendered `String`, a status, a `Network.responseReceived`
header map and `redirectResponse` hops), drives it through the same reader as an HTTP-driven one,
and runs a full adaptive save-then-relocate on it. Copy `from_browser_page` out of that test:

```rust
let encoding = headers.get("content-type")
    .and_then(|v| browser_http::response::charset_from_content_type(v))   // __CHARSET_RE__
    .unwrap_or("utf-8");
let mut builder = browser_http::response::Response::builder(status, url)
    .method(Method::Get)
    .body(rendered_html)          // a String; the declared encoding round-trips it
    .headers(headers)
    .encoding(encoding)
    .history(hops)                // Vec<RedirectHop> from redirectResponse
    .selector_config(selector_config);
if let Some(storage) = storage {                     // adaptive storage goes in through the hook
    builder = builder.document_hook(Arc::new(move |c| c.storage(Arc::clone(&storage))));
}
builder.build()
```

Use `charset_from_content_type` (the quote-tolerant `__CHARSET_RE__`), **not**
`charset_from_content_type_http`: `__extract_browser_encoding` is only used on this side in
Python, because `curl_cffi` resolves the charset for the HTTP engines — and now that
`browser-dom` really decodes with the label, it has a real effect.

### ~~Unit 15 — `browser-spider` data plane~~ ✅ **done** (wave 3)
`request.rs`, `canonicalize.rs`, `scheduler.rs`, `session.rs`, `stats.rs`, `items.rs`, `checkpoint.rs`
— 2352/2358 corpus rows, 125 tests. Two things unit 17 must know:
- **`Scheduler::dequeue` clones** the request out of the pending mirror, so mutating a dequeued
  request will not show up in a later `snapshot()`. Python's mirror holds the same object and does.
  Either don't mutate in-flight requests, or re-enqueue a copy. Not corpus-covered.
- **`CheckpointManager` is JSON, not pickle** (declared divergence): a `checkpoint.pkl` written by
  Python is not resumable, by design. `save` re-raises, `cleanup` swallows.

### ~~Unit 16 — spider support services~~ ✅ **done** (wave 4)
`throttle.rs`, `robots.rs`, `cache.rs` — 172,311/172,447 differential assertions, 246 tests.
Three things unit 17 must know:

- **`AutoThrottle::delay_for` is a write** (it seeds the domain), and `record` holds the domain's
  `DashMap` entry guard for the whole read-modify-write — so **never call `delay_for` from inside
  `record`**; that deadlocks.
- **`RobotsTxtManager` has no single-flight.** Two concurrent misses for one netloc both fetch and
  the last writer wins, faithfully to Python. The fetch-count corpus pins it; adding one would be
  a strict improvement and no test forbids it.
- **`ResponseCacheManager::put` never fails a crawl** — every error is logged and swallowed, and
  the `bool` only reports whether the entry landed. Any unreadable or type-mismatched entry is an
  ordinary miss.

Six divergence families are written up in `docs/DIVERGENCES.md` under "Unit 16"; the one that is
really a **to-do for unit 17** is item 3: default `FetchedResponse::encoding` to `"utf-8"` instead
of `""`, which is in `session.rs` and removes a whole declared divergence.

### Unit 17 — spider engine ⬅️ **UNBLOCKED, and parallel-safe with 14**
*Deps: 15, 16 — both done.*
`engine.rs`, `spider.rs`. The `Spider` trait replacing Python subclassing; `CrawlerEngine` with the
Notify-driven loop, global spawn cap + per-domain semaphores, the exact `_process_request` ordering
(robots → cache replay → limiter → delay → fetch → cache write → block detection → autothrottle
feedback → retry-or-callback), Request-before-Item dispatch, the pause/force-stop ladder with
**checkpoint saved before cancel**, streaming via `mpsc::channel(100)`, `ctrl_c()` in the select loop.

**The whole loop already runs.** `crates/browser-rs/tests/integration.rs::crawl_loop_seam` writes
`_process_request`'s ordering out by hand against a **real** `AsyncFetcherSession` and a loopback
HTTP server, using `Scheduler`, `SessionManager`, `RobotsTxtManager`, `AutoThrottle`,
`ResponseCacheManager`, `CrawlStats` and unit 12's fetcher — and asserts that a `robots.txt`
`Disallow` stops a fetch *before* it happens, that de-duplication holds, that a 429 feeds
`Retry-After` into `AutoThrottle::record`, and that a second pass replays entirely from cache
without touching the server. Unit 17 has nothing left to invent, only to arrange. Three concrete
starting jobs:

1. **Promote `crawl_loop_seam::HttpCrawlSession` into `browser-spider`** — `CrawlSession` over
   `AsyncFetcherSession`, about thirty mechanical lines. Then extend its `SessionKwargs` →
   `RequestOverrides` mapping past `method`/`headers`/`json` to `proxy`, `proxies`, `timeout` and
   the opaque `extra`, which the test does not need and a real crawl does.
2. **Default `FetchedResponse::encoding` to `"utf-8"`** in `session.rs`, which deletes a declared
   unit-16 divergence (`docs/DIVERGENCES.md`, unit 16 item 3).
3. **Drop `httpdate` and `texting_robots`** from `crates/browser-spider/Cargo.toml`. Both are now
   unused — unit 16 replaced the date parser with a `_parsedate_tz` port over `chrono::NaiveDate`,
   and `texting_robots` was never used at all. That file was off-limits to unit 16; it is yours.

### Unit 18 — `browser-templates` *(parallel-safe with 19)*
`LinkExtractor` (13 kwargs, `IGNORED_EXTENSIONS`, `strip_html5_whitespace` — the five HTML5 space chars
only, **not** `str::trim`), `CrawlSpider`/`CrawlRule` (rule dispatch as a **free function** so users can
compose it), `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider`, `SiteToMarkdownSpider`,
gzip `_decompress` with the 64 MiB bomb cap.

### Unit 19 — `browser-cli` *(parallel-safe with 18)*
clap-derive `browser-rs` binary: `extract get/post/put/delete/fetch/stealthy-fetch`, the shared option
groups, the **seconds-vs-milliseconds split** between HTTP and browser commands, `write_content_to_file`
with the `.md`/`.html`/`.txt` mode map, `--ai-targeted`, `BROWSER_RS_EXECUTABLE_PATH` precedence,
`install` (Chromium via `chromiumoxide_fetcher`, not Python Playwright), and `uncurl`.

### Unit 20 — façade, docs, conformance, benchmarks
`crates/browser-rs/src/{lib,prelude}.rs` re-export surface + feature flags, end-to-end tests against a
local server, criterion benches (`parse`, `relocate`, `markdown` — the spec pins parse of 5000
nested `div.item` + `css(".item")` under 0.5 s; we should beat that by ~50×), finished README,
`DIVERGENCES.md`, runnable examples for the four entry points.

---

## Housekeeping TODOs (small, do them early)

1. ~~Copy `specs/` into the repo~~ — **done**, commit `e837c0b`. Now at `docs/specs/` (684 KB).
2. ~~Move the corpus generators into the repo~~ — **done**, commit `e837c0b`. All 69 scripts now at
   `tools/corpus/`, with a `README.md` mapping each directory to the testdata it regenerates.
   `mdgold` and `xconf` had their absolute paths fixed in wave 3 and were re-run successfully;
   `u10`, `u12`, `u15` and `u16` were written in-repo and carry no absolute paths. The rest may
   still carry paths from the original session — check the constants at the top before re-running.
3. ~~Fix `browser-dom`'s `remove_blank_text`~~ — **done.** It now implements libxml2's
   `areBlanks()`, established empirically against libxml2 2.14.6 rather than read off a header.
   That closed 5 CSS `:empty` rows, 3 XPath conformance divergences and all 7 markdown pipeline
   failures, and let the blanket "filter whitespace-only text out of both sides" escape hatch be
   deleted from both CSS differential tests. The rule and its two lexer-level residues are in
   `docs/DIVERGENCES.md` §"Blank text" and `crates/browser-dom/API.md` deviation 3.
4. ~~Add `iterchildren()` to `browser-dom`~~ — **done**: `NodeRef::iterchildren()`, pinned by
   `tests/downstream_contract.rs::iterchildren_matches_lxmls_child_sequence`.
5. `cargo-deny` is **not installed** on this machine, so `deny.toml` has never actually run.
   `cargo install cargo-deny` and verify CI's deny step passes. **Still open — the only one.**
6. ~~The one `#[ignore]`d test~~ — **done, it is not a bug.** It is
   `browser-markdown/tests/markdown_golden.rs::emit_rust_answers`, a regeneration aid that reads
   `$MD_CASES` and writes `$MD_OUT` for `tools/corpus/mdgold/step2_golden.py`. It fails under bare
   `--ignored` because those variables are unset; that is correct. Leave it ignored.
7. `browser-markdown/tests/sanitize_golden.rs` still strips whitespace before comparing, because
   its golden files store only the stripped form and its generator is **not** in `tools/corpus/`.
   Now mostly moot (the inputs in `testdata/normalized/` are minified), but if anyone regenerates
   that corpus, tighten the comparison at the same time.
8. **`wiremock` is declared in `[workspace.dependencies]` and is not in `Cargo.lock`.** Nothing uses
   it, and pulling it in needs crates.io, which is not reachable here. The two local-server test
   fixtures written so far are hand-rolled `std::net` listeners
   (`crates/browser-http/tests/u12_support/mod.rs` and `crawl_loop_seam::Fixture` in the façade's
   integration test); unit 20 should either vendor `wiremock` deliberately or drop the line.
9. **`hyper` is not a `browser-http` dependency**, which is the only reason a non-canonical reason
   phrase is lost (corpus rows `status-299`, `status-222`). One line in `Cargo.toml` closes it.
10. **`httpdate` and `texting_robots` are unused** by `browser-spider` and still listed in its
    `Cargo.toml`. Unit 17 owns that file now — see its TODO.

---

## Known divergences carried forward (all deliberate, all in `docs/DIVERGENCES.md`)

- **html5ever vs libxml2 tree shape** — implied `<tbody>`, synthesised `<head>`, the HTML5 adoption
  agency on `<p>x<b>y<i>z</p>`. ~187 corpus rows across units 5 and 6. Accepted by ARCHITECTURE §11.6:
  html5ever's answer is the one *browsers* produce, which is what a scraper wants.
- **CSS-Selectors-4 supersets** — we accept `:has()`, `:not(a, b)`, `.x:first-of-type`,
  `:nth-child(2 of .x)`; cssselect rejects them. Cannot change the meaning of any selector that works
  in Python.
- **lxml's sibling-combinator subtree escape** — `node.css("* ~ *")` reaching the query node's own
  following siblings. Unfixed; Servo's `Element` trait cannot distinguish a combinator step from
  `:first-child`. The opposite-direction bug (over-matching) *was* fixed.
- **Vintage drift** — `psl`'s public-suffix snapshot is newer than `tld` 0.13.2's (40/20306 hosts
  differ); `char::to_lowercase` is newer than CPython 3.14's UCD 16.0.0 (28/1112064 code points, all
  unassigned).
- **Checkpoints are JSON, not pickle.** Directory layout, atomic write, `interval` semantics and log
  lines all preserved.
- **Lazy `Response` parsing** (Python parses eagerly, even for binary bodies). Designed by
  `response_seam` before unit 12 existed, and shipped in unit 12's real `Response` behind a
  `OnceLock`, which is what keeps it `Send + Sync`.
- **`follow_redirects="safe"` exempts same-origin hops.** curl runs `CURLFOLLOW_SAFE` inside
  connection setup, so a hop that reuses the open connection is never checked; a port that does not
  own the connection pool cannot see reuse. The security property is unchanged — a redirect still
  cannot pivot to an address the caller had not already reached. Corpus row `redirect-close-safe`.
- **`Response.url` is the fetched URL, not `adaptive_domain`.** Python's substitution quietly breaks
  `urljoin`; here the domain is only the adaptive storage key (`Response::selector_url()`).
- **Nine `robots.txt` text codecs decode as UTF-8** (all ASCII supersets, so the ruleset is
  identical), and a `Request-rate` denominator larger than `i64` is ignored. See `DIVERGENCES.md`
  §"Unit 16".
- **Encoding labels go through WHATWG, not `iconv`** — `iso-8859-1`/`latin1`/`ascii` are aliases of
  windows-1252, so the 32 bytes `0x80..=0x9F` differ; and an encoding problem never destroys the
  document (Python raises `LookupError`, or lxml returns an empty tree, where we warn and degrade to
  U+FFFD). New in wave 3, when `ParseOptions::encoding` started actually decoding.
- **Boolean attributes** — libxml2 gives `checked` the value `"checked"`, html5ever gives `""` and
  cannot tell `checked` from `checked=""` at all. 89 corpus rows in unit 10, and it moves
  `find_similar` scores, not just serialisation.
- IPv6 proxy hosts keep their brackets (Python emits the unusable `http://::1:9050`).

## Out of scope (decided, with reasons in ARCHITECTURE §8)

MCP/AI server · IPython shell · Scrapy integration · browserforge's Bayesian header sampler ·
curl-impersonate FFI · Patchright · pickle compatibility · libxml2 recovery bug-compat · `huge_tree` ·
Camoufox-era options (`geoip`/`humanize`/`block_images`/… — already deleted upstream in v0.3.13).

---

## How to drive the next session

The pattern that worked, and is worth repeating:

1. One `Workflow` per wave. Parallel `agent()` per unit, each owning a **disjoint file set**;
   never let two agents touch one file.
2. Immediately after each implementer, a **verifier** agent in the same pipeline stage that generates
   a golden corpus from the reference Python and fixes the Rust until it matches. Instruct it
   explicitly: *never shrink the corpus to raise the pass rate; encode unreproducible cases as
   declared divergence rows that fail if they ever start passing.* That instruction is what produced
   the honest 94.9% instead of a fake 100%.
3. A final **green** agent per wave that may edit anything: reconcile cross-crate API drift, run
   `build`/`test`/`clippy`/`fmt`, write a composition test proving the next wave's seam works, append
   to `DIVERGENCES.md`, and commit.

Waves 1–4 are done (units 1–13, 15, 16). Remaining: **14 + 17 in parallel** (they touch disjoint
crates and 17 no longer waits on 14), then **18 + 19** in parallel, then **20**.

Wave 4 added a fourth job to the green agent's list that is worth keeping: **write the next wave's
seam test before the next wave starts.** `browser_response_seam` and `crawl_loop_seam` in
`crates/browser-rs/tests/integration.rs` are what turned three vague API questions into three small
commits (`From<&Response> for FetchedResponse`, `FetchedResponse::response_builder`, and
`Method`/`HttpMethod` conversions) instead of into a merge conflict two weeks later.

```bash
cd /Users/himanshu/codes/browser-rs
cargo build --workspace --all-features \
  && cargo test --workspace \
  && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo fmt --all --check
```
