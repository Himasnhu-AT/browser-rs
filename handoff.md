# Handoff — the reference implementation → Rust port

**Date:** 2026-08-25
**Repo:** `/Users/himanshu/codes/browser-rs` (branch `main`)
**Upstream being ported:** [Himasnhu-AT/browser-rs](https://github.com/Himasnhu-AT/browser-rs) v0.4.15 (~16k lines of Python)

---

## TL;DR — where things stand

| | |
|---|---|
| Build | `cargo build --workspace --all-features` ✅ green |
| Tests | **1791 passing, 0 failing, 2 ignored** under `--all-features`; 66 of them launch **real Chrome** and skip cleanly without it |
| Clippy | `--workspace --all-targets --all-features -D warnings` ✅ clean |
| Format | `cargo fmt --all --check` ✅ clean |
| Code written | ~101k lines across 14 crates (~74k of it `src/`) |
| Plan progress | **17 of 20 work units done** (units 1–17) |
| Last commit | `23edc39 Wave 5: the CDP browser driver, the crawler engine, and the unit-18/19 seams` |

The two ignored tests are both regeneration aids, not checks, and are meant to stay ignored:
`browser-markdown`'s `emit_rust_answers` (reads `$MD_CASES`, writes `$MD_OUT` for
`tools/corpus/mdgold/step2_golden.py`) and one `ignore`d doc fence.

Commits so far:

```
23edc39   Wave 5: the CDP browser driver, the crawler engine, and the unit-18/19 seams
9ac6251   handoff: record the wave-4 commit hash
80241ef   Wave 4: the HTTP driver, spider support services, and the unit-14/17 seams
d78c06e   handoff: record the wave-3 commit hash
3824cbb   Wave 3: the public document API, the spider data plane, and the Response seam
9a29998   handoff: mark the two preservation TODOs done
e837c0b   Preserve specs, corpus generators and the session handoff in-repo
8228e99   Wave 2: selectors, XPath, storage, markdown, HTTP policy, browser config
8cc1f38   Units 2/3/4/9: core, dom and fingerprint foundation crates
fe30e17   Unit 1: workspace scaffolding for the Rust port of the reference implementation
```

Working tree is clean. Nothing is red. Pick up at **units 18 and 19** — they touch disjoint
crates (`browser-templates` and `browser-cli`) and are parallel-safe — then **unit 20**.

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
5. **`crates/browser-rs/tests/integration.rs`** — the **26** cross-crate seam tests, and the best map
   of how the crates fit together. `response_seam` is the executable spec unit 12 was written
   against; `browser_response_seam` was unit 14's and `crawl_loop_seam` unit 17's. Wave 5 added the
   three the *next* wave is written against:
   * `crawl_spider_seam` — a user-written `Spider` with a `LinkExtractor`, `CrawlRule`s and a named
     callback, run to completion by the real `CrawlerEngine` over a loopback site. **Unit 18's
     `CrawlSpider`, in miniature.**
   * `site_to_markdown_seam` — `Response::markdown()` on a response from the HTTP fetcher *and* one
     from **real Chrome**, plus the `CrawlResponse` deref path. **Unit 18's `SiteToMarkdownSpider`.**
   * `cli_extract_seam` — `extract get <url> <out>` end to end: option group → `AsyncFetcher::get`
     → `_extract_content` → `write_content_to_file`, all three suffixes. **Unit 19's core command.**
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
| `browser-spider` | 20397 | ✅ done | **units 15 + 16 + 17** — `Request` + SHA-1 fingerprint, `canonicalize_url`, `Scheduler`, `SessionManager` + `CrawlSession`, `CrawlStats`, `ItemList` exporters, `CheckpointManager`, `AutoThrottle` + `Retry-After`, the `protego` port + `RobotsTxtManager`, `ResponseCacheManager`, the `Response` ⇄ `FetchedResponse` conversions — **and now `engine.rs` + `spider.rs`: the `Spider` trait, `CrawlerEngine`, `SpiderRunner`, `HttpCrawlSession`** |
| `browser-cdp` | 18113 | ✅ done | **units 13 + 14** — config/flags/pool/route, plus the raw-CDP driver: `CdpPage` with one deadline across `Page.navigate` + `load`, the hand-rolled `networkidle`, `Fetch`-domain interception, `Page.createIsolatedWorld`, the redirect ledger with `…ExtraInfo` header resolution, the response/XHR factory, and the Cloudflare Turnstile solver |
| `browser-templates` | 49 | ❌ stub | Unit 18 |
| `browser-cli` | 49 | ❌ stub | Unit 19 |
| `browser-rs` (façade) | 3102 | 🟡 partial | 26 cross-crate seam tests, all passing; the re-export surface is unit 20 |

**~101k lines across 14 crates** (~74k of it `src/`).

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
- **Crawler engine (unit 17): 1255/1255 field comparisons (100%)** across **36 real crawls** run
  on both implementations over the same declarative two-host loopback fixture — the URLs the
  *servers* saw, the items and their depths, `CrawlStats.to_dict()` key by key, the lifecycle
  hooks, `on_error`, the checkpoint contents and four pause→resume round trips (including one that
  pauses with the blocked-retry ladder in flight, which pins `_retry_count` and `dont_filter`
  surviving a checkpoint, and one with four requests in flight). Five relaxations are declared with
  reasons and three are asserted to keep differing. The suite was **mutation-tested with 16
  deliberate engine bugs**; the three that escaped are now pinned.
- **Browser driver (unit 14): 311/311 in-crate tests, 66 of them against real Chrome.** The corpus
  (35 rows, `tools/corpus/u14/`) was produced by driving **real the reference implementation 0.4.15 + Playwright + the
  stock Google Chrome** over a declarative loopback fixture, and is replayed by driving **the same
  Chrome binary** through `BrowserSession`. Every resource-blocking assertion is made against what
  the *socket* saw, not against CDP bookkeeping. The differential found **four real defects**, all
  fixed: `timeout` not bounding `page.goto` (30 s where Python raises at 3 s), a `referer` in
  `extra_headers` making Chrome refuse every navigation with `net::ERR_BLOCKED_BY_CLIENT`, a
  same-document (fragment) navigation burning the whole timeout, and `Response.headers` losing every
  `Set-Cookie` because `Network.responseReceived` reports the *redacted* header set. One divergence
  is declared and asserted to keep differing (`a_download_is_not_a_navigation`).

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

### ~~Unit 14 — `browser-cdp` CDP driver + Cloudflare solver~~ ✅ **done** (wave 5)
`cdp/{mod,networkidle,fetch_domain,isolated_world,history}.rs`, `session.rs`, `response.rs`,
`cloudflare.rs` — 311 in-crate tests, 66 of them driving **real Chrome** against a loopback fixture
that is the Rust twin of the Python one (`tools/corpus/u14/`, which also emits
`testdata/u14_fixture.json` so the two servers cannot drift). It does **not** write a second
`Response`: `BrowserResponseFactory` builds unit 12's, exactly as `browser_response_seam` predicted.

What units 18–20 must know:

- **`BrowserSession::fetch(url, FetchParams)` → `browser_http::response::Response`.**
  `fetch_detailed` returns the `BrowserFetch` if you want the XHR captures. `fetch_once` is the
  one-shot form. `browser-cdp` now has `markdown` and `adaptive` features that forward to
  `browser-http` — see the feature note below.
- **Browser tests skip cleanly with no Chrome.** `live_u14::browser_available()` honours
  `BROWSER_RS_SKIP_BROWSER_TESTS` and the executable probe; every live test prints a skip notice and
  returns. Verified both ways in wave 5: with Chrome the browser lib suite is ~48 s, and with
  executable resolution forced to fail all 244 lib tests pass in 0.25 s with 66 skip notices.
- **One 3-permit semaphore for the whole crate's live tests** (`live_u14::LIVE_BROWSERS`, shared
  with `session::live`). Cold-starting six Chromes at once really did fail to hand back a WebSocket
  URL. Anything new that launches a browser inside `browser-cdp` must take a permit.
- **Still only structurally verified: Cloudflare.** `/challenge*` reproduces the `cType` marker, the
  widget at a known absolute position and the mousedown-clears-it transition, which pins the state
  machine, the attempt counts, the polling cadence, the click-offset arithmetic and the deadline. It
  does **not** prove Cloudflare accepts the click, and the `embedded` branch plus the real
  cross-origin challenge-iframe path are unit-tested against a trait double only.
- **Untested end to end:** TLS (`ignore_https_errors`, HSTS, `--ignore-certificate-errors`), proxies
  (no Python-side differential — only the pre-existing `session::live` tests against two local
  proxies), and the `media` / `websocket` / `texttrack` resource types (golden mapping table only).
- **Accepted-but-inert:** `additional_args` (there is no Playwright context to merge into).
  `chromiumoxide`'s `FrameManager` still emits `Runtime.enable` on attach and appends
  `--disable-extensions`; both are upstream and documented.

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

### ~~Unit 17 — spider engine~~ ✅ **done**
`engine.rs`, `spider.rs` — the `Spider` trait, `SpiderSettings`, `CrawlResponse` + `follow`,
`Yielded`, `HttpCrawlSession`, `RunOptions`/`SpiderRunner`, and `CrawlerEngine` with the
Notify-driven loop, the global spawn cap plus per-domain semaphores, the exact `_process_request`
ordering, the pause/force-stop ladder and streaming.

**Verified by running 36 real crawls on both implementations** over the same declarative two-host
loopback fixture and comparing 1,255 fields: the URLs the *servers* saw (in arrival order where
the scenario is serial, as a multiset where it is not), the items and their depths,
`CrawlStats.to_dict()` key by key, the lifecycle-hook call records, `on_error`, the checkpoint
contents and the pause→resume round trips. **1255/1255 reproduce.** Generator and fixture:
`tools/corpus/u17/` (with its own `README.md`); the comparison is
`crates/browser-spider/tests/u17_engine_golden.rs`, and it needs neither Python nor a network.
Five relaxations are declared in the test with reasons, three of which are still asserted (see
`docs/DIVERGENCES.md` §"Unit 17").

The differential was mutation-tested — 16 deliberate engine/spider bugs were introduced one at a
time — and **three of them escaped the suite as it stood**; all three are now pinned:

1. a callback error not stopping the rest of its stream (the old test's callback yielded nothing
   after the error, so the assertion in its name was never made);
2. a force stop cancelling *before* it checkpoints, which loses every in-flight request (the old
   test pressed the ladder from the only callback there was, so `abort_all` had nothing to abort);
3. the `robots.txt` `Crawl-delay` never reaching the pre-fetch sleep — every count and every stat
   is identical whether the engine sleeps or not, so only a wall-clock **lower bound** can see it.

The three side jobs are done too: `httpdate` and `texting_robots` are gone from
`crates/browser-spider/Cargo.toml` (and `tempfile` moved to `[dev-dependencies]`),
`FetchedResponse::encoding` now defaults to `"utf-8"`, and `HttpCrawlSession` is in
`spider.rs`. The unit-17 error variants stay in a module-local `CrawlError` in `spider.rs`, which
`crates/browser-spider/API.md` explicitly sanctions.

~~**Left for the wave's green agent:** `crawl_loop_seam::Fixture::serve` closes with
`stream.shutdown(Shutdown::Both)`~~ — **done in wave 5**: it now half-closes with
`Shutdown::Write` and drains to EOF, and the new `local_site::Site` fixture does the same.

Known holes, none of them closable on loopback: **Chrome was never involved** (the `browser`
feature routing a crawl through `browser-cdp` is completely untested), proxies / TLS /
impersonation have no differential, `Ctrl-C` handling rests on reading rather than running (SIGINT
cannot be delivered to one `#[tokio::test]`), `SpiderRunner::start_blocking` is tested only on its
refuse-to-nest branch, and `SessionKwargs` cannot express Python's `data=None` as an explicit clear
(unit-17 divergence 14).

### Unit 18 — `browser-templates` *(parallel-safe with 19)*
`LinkExtractor` (13 kwargs, `IGNORED_EXTENSIONS`, `strip_html5_whitespace` — the five HTML5 space chars
only, **not** `str::trim`), `CrawlSpider`/`CrawlRule` (rule dispatch as a **free function** so users can
compose it), `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider`, `SiteToMarkdownSpider`,
gzip `_decompress` with the 64 MiB bomb cap.

**Start from the seam tests, not from the spec.** `crates/browser-rs/tests/integration.rs::crawl_spider_seam`
already contains a working `strip_html5_whitespace`, an `allow`/`deny` `LinkExtractor`, a `CrawlRule`,
`requests_to_follow` **as a free function**, and a two-callback `Spider` that the real
`CrawlerEngine` runs to completion over a loopback site — with robots.txt, de-duplication and a
`deny` rule all asserted. `site_to_markdown_seam` does the same for `SiteToMarkdownSpider`, over
**both** the HTTP fetcher and real Chrome. Lift them into the crate; they were written to be lifted.

Five things that will bite (all written up in `docs/DIVERGENCES.md` §"Wave 5 integration"):

- **A rule must set `FollowOptions::callback` explicitly, even a follow-only one.** `follow`
  *inherits* the originating request's callback when it is `None`, so a rule that leaves it unset
  makes every page reached from a product page get parsed as a product. Write
  `Some(rule.callback.clone().unwrap_or(CallbackId::PARSE))`.
- **`Nodes::get()` is Python's `.get()`**; `Nodes::first()` returns `Option<&Node>`. Extractors want
  `get()` / `texts()` / `attrs()`.
- **A spider that overrides `dispatch` must override `has_callback` too**, or a restored checkpoint
  rewrites every named callback to `parse`.
- **`Scheduler::dequeue` clones**; mutating a dequeued request will not show up in a later
  `snapshot()`.
- The crate already has `csv`, `flate2`, `quick-xml` and `regex` wired in `Cargo.toml`, and a
  `markdown` feature that now correctly forwards to `browser-http` (see below).

### Unit 19 — `browser-cli` *(parallel-safe with 18)*
clap-derive `browser-rs` binary: `extract get/post/put/delete/fetch/stealthy-fetch`, the shared option
groups, the **seconds-vs-milliseconds split** between HTTP and browser commands, `write_content_to_file`
with the `.md`/`.html`/`.txt` mode map, `--ai-targeted`, `BROWSER_RS_EXECUTABLE_PATH` precedence,
`install` (Chromium via `chromiumoxide_fetcher`, not Python Playwright), and `uncurl`.

**`crates/browser-rs/tests/integration.rs::cli_extract_seam` is `extract get` already written**:
the parsed option group → `AsyncFetcher::get` with headers/params/cookies/timeout → `_extract_content`
in all three modes → `write_content_to_file`. It pins the four details that are easy to get wrong:

- the generator's **trailing `""`** (so `pieces.len() == matches + 1`);
- `text` mode collapsing `\n`, `\r`, `\t`, ` ` **independently and in that order**, each with its own
  `[c]+ -> c` substitution — one pass over a combined character class is wrong;
- an unknown suffix is a **`ValueError`**, not a silent default, and the match is on the suffix
  *string*, not on `Path::extension`;
- the **seconds-vs-milliseconds split**: `extract get/post/put/delete` take seconds and feed
  `RequestBuilder::timeout(Option<Duration>)`; `fetch` / `stealthy-fetch` take milliseconds and feed
  `BrowserConfig::timeout_ms(f64)`.

`BROWSER_RS_EXECUTABLE_PATH` is **the CLI's** concern, not the browser crate's: `resolve_executable`
in `browser-cdp` reads `BrowserConfig::executable_path` and then falls through to
`chromiumoxide`'s detection (which honours `CHROME`). Unit 19 implements the precedence
`--executable-path` > `BROWSER_RS_EXECUTABLE_PATH` > absent, per `cli-ai.md` §1394.

### Unit 20 — façade, docs, conformance, benchmarks
`crates/browser-rs/src/{lib,prelude}.rs` re-export surface + feature flags, end-to-end tests against a
local server, criterion benches (`parse`, `relocate`, `markdown` — the spec pins parse of 5000
nested `div.item` + `css(".item")` under 0.5 s; we should beat that by ~50×), finished README,
`DIVERGENCES.md`, runnable examples for the four entry points.

**The feature graph is now correct but is still only *forwarded*, never *re-exported*.**
`crates/browser-rs/src/lib.rs` is 26 lines. Wave 5 fixed a real hole in it — `markdown` and
`adaptive` were forwarded only to `browser-parser`, so `Response::markdown()` (which lives in
`browser-http`) was unreachable through the façade and would have broken unit 19 on day one.
Every forwarding list now names `browser-http` too, `browser-cdp` gained `markdown` /
`adaptive`, and two tests pin it by *not compiling* if it regresses
(`browser-spider::spider::tests::the_markdown_feature_reaches_the_response_through_deref`,
`browser-cdp::response::tests::a_browser_response_converts_to_markdown`). Unit 20 should keep
that discipline as it writes the re-export surface: **every `#[cfg(feature = …)]` item in a member
crate needs a matching forwarding entry here.**

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
5. ~~`cargo-deny` is not installed~~ — **it is now, it was run, and it found a real problem.**
   `cargo deny check` reports **`advisories ok, bans ok, licenses FAILED, sources ok`**.

   🔴 **`wreq-util 0.1.0` is GPL-3.0 and this workspace is MIT.** It is the emulation-profile table
   behind `browser-http`'s `impersonate` feature — which is **on by default** — so as things stand
   a default build of `browser-rs` links copyleft code into an MIT crate. `wreq` itself is
   Apache-2.0; only the profile crate is GPL. This is **not** something the green agent should
   allowlist away, so `deny.toml`'s `exceptions` is deliberately still empty and the check is
   deliberately still red. A human has to pick one of:

   1. relicense the workspace to GPL-3.0;
   2. take `impersonate` out of `browser-http`'s default features and ship the JA3/JA4 profiles as
      an opt-in GPL build, documented as such;
   3. replace `wreq-util` with permissively licensed profile tables — the mapping in
      `crates/browser-http/src/backend/wreq.rs` is ours, only the profile *data* is theirs.

   The reasoning is repeated in `deny.toml` next to the empty `exceptions`, so nobody closes it by
   accident. One licence *was* added legitimately: `CDLA-Permissive-2.0`, for `webpki-root-certs` —
   the Mozilla root store, a permissive **data** licence with no copyleft reach into the code.

   **This is the one thing that is still red in the repo.**
6. ~~The one `#[ignore]`d test~~ — **done, it is not a bug.** It is
   `browser-markdown/tests/markdown_golden.rs::emit_rust_answers`, a regeneration aid that reads
   `$MD_CASES` and writes `$MD_OUT` for `tools/corpus/mdgold/step2_golden.py`. It fails under bare
   `--ignored` because those variables are unset; that is correct. Leave it ignored.
7. `browser-markdown/tests/sanitize_golden.rs` still strips whitespace before comparing, because
   its golden files store only the stripped form and its generator is **not** in `tools/corpus/`.
   Now mostly moot (the inputs in `testdata/normalized/` are minified), but if anyone regenerates
   that corpus, tighten the comparison at the same time.
8. **`wiremock` is declared in `[workspace.dependencies]` and is not in `Cargo.lock`.** Nothing uses
   it, and pulling it in needs crates.io, which is not reachable here. There are now **five**
   hand-rolled `std::net` / `tokio` fixture servers instead
   (`crates/browser-http/tests/u12_support/mod.rs`, `crates/browser-spider/tests/u17_support/mod.rs`,
   `crates/browser-cdp/src/live_u14/server.rs`, and `crawl_loop_seam::Fixture` +
   `local_site::Site` in the façade's integration test); unit 20 should either vendor `wiremock`
   deliberately or drop the line. `local_site::Site` is the one to copy: it takes a
   `fn(&str) -> Option<Page>` route table, serves each connection on its own thread (a browser opens
   several at once), and half-closes rather than RSTing.
9. **`hyper` is not a `browser-http` dependency**, which is the only reason a non-canonical reason
   phrase is lost (corpus rows `status-299`, `status-222`). One line in `Cargo.toml` closes it.
10. ~~**`httpdate` and `texting_robots` are unused**~~ — **done** in unit 17: both are gone from
    `crates/browser-spider/Cargo.toml`, and `tempfile` moved to `[dev-dependencies]` (it is only
    used by tests and doc examples). `tracing-subscriber` was added there for the u17 differential,
    which installs a counting-only subscriber so `CrawlStats.log_count` is a real tally.

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

Waves 1–5 are done (units 1–17). Remaining: **18 + 19 in parallel** (disjoint crates), then **20**.

Wave 4 added a fourth job to the green agent's list that is worth keeping: **write the next wave's
seam test before the next wave starts.** `browser_response_seam` and `crawl_loop_seam` in
`crates/browser-rs/tests/integration.rs` are what turned three vague API questions into three small
commits (`From<&Response> for FetchedResponse`, `FetchedResponse::response_builder`, and
`Method`/`HttpMethod` conversions) instead of into a merge conflict two weeks later. Wave 5 did the
same for units 18 and 19 (`crawl_spider_seam`, `site_to_markdown_seam`, `cli_extract_seam`) and it
paid immediately: writing them surfaced that the `markdown` feature never reached
`browser-http`, so `Response::markdown()` — the one method unit 19's `extract get out.md` is
built on — was unreachable through the façade. That would have been unit 19's first hour.

A fifth job is now worth adding: **run `cargo deny check`.** It had never been run before wave 5,
and the first run found a GPL-3.0 crate in an MIT workspace (housekeeping item 5).

```bash
cd /Users/himanshu/codes/browser-rs
cargo build --workspace --all-features \
  && cargo test --workspace --all-features \
  && cargo clippy --workspace --all-targets --all-features -- -D warnings \
  && cargo fmt --all --check
# and, separately, because it is expected to FAIL on licences until the
# `wreq-util` GPL question is answered:
cargo deny check          # advisories ok, bans ok, licenses FAILED, sources ok

# The browser suite takes ~48 s because 66 tests launch real Chrome. To run
# without one (and prove the skip path still works):
BROWSER_RS_SKIP_BROWSER_TESTS=1 cargo test --workspace --all-features
```
