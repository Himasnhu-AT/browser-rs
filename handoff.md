# Handoff — the reference implementation → Rust port

**Date:** 2026-08-24
**Repo:** `/Users/himanshu/codes/browser-rs` (branch `main`)
**Upstream being ported:** [Himasnhu-AT/browser-rs](https://github.com/Himasnhu-AT/browser-rs) v0.4.15 (~16k lines of Python)

---

## TL;DR — where things stand

| | |
|---|---|
| Build | `cargo build --workspace` ✅ green |
| Tests | **1011 passing, 0 failing, 1 ignored** |
| Clippy | `--workspace --all-targets -D warnings` ✅ clean |
| Code written | ~48k lines across 14 crates |
| Plan progress | **11 of 20 work units done** (units 1–9, 11, 13) |
| Last commit | `8228e99 Wave 2: selectors, XPath, storage, markdown, HTTP policy, browser config` |

Three commits so far:

```
8228e99  Wave 2: selectors, XPath, storage, markdown, HTTP policy, browser config
8cc1f38  Units 2/3/4/9: core, dom and fingerprint foundation crates
fe30e17  Unit 1: workspace scaffolding for the Rust port of the reference implementation
```

Working tree is clean. Nothing is red. Pick up at **Unit 10**.

---

## Read these first (in this order)

1. **`ARCHITECTURE.md`** — `/private/tmp/claude-501/-Users-himanshu-codes-browser-rs/e75f7008-418e-4830-a940-a8d4bd0450a7/scratchpad/specs/ARCHITECTURE.md`
   87 KB. Crate layout (§1), every dependency with justification (§2), the parse+select design (§3),
   adaptive relocation (§4), async/error/API ergonomics (§5), **the 20-unit plan (§7)**, out-of-scope (§8).
   ⚠️ This lives in a **temp directory**. First action tomorrow: copy `specs/` into the repo under
   `docs/specs/` and commit it, before something reaps `/private/tmp`.
2. **Subsystem specs** — same directory: `parser.md`, `core-types.md`, `http-fetch.md`, `browser.md`,
   `toolbelt.md`, `spider-core.md`, `spider-support.md`, `cli-ai.md` (60–90 KB each, generated from
   the Python source; the implementers work from these, not from the Python).
3. **`crates/browser-dom/API.md`** — the DOM cheat sheet every downstream crate codes against.
4. **`docs/DIVERGENCES.md`** — every intentional behavioural difference from Python, in one place.

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

⚠️ This venv is in a **job temp dir too**. If it disappears: `python3 -m venv pyref && ./pyref/bin/pip
install lxml cssselect markdownify w3lib the reference implementation`. The Python source is cloned at
`/private/tmp/claude-501/-Users-himanshu-codes-browser-rs/e75f7008-418e-4830-a940-a8d4bd0450a7/scratchpad/the reference implementation`
(re-clone from GitHub if gone).

---

## What exists today

| Crate | LOC | Status | What it is |
|---|---:|---|---|
| `browser-core` | 4338 | ✅ done | `Override<T>`, `TextExt` (Python `findall` group-flattening), w3lib entity port + cp1252 remap, `AttributeMap`, `round_half_even`, cookie/header parsing, tracing `LevelCounter` — **plus** a bit-exact CPython `difflib.SequenceMatcher` port, `ElementFingerprint`, `similarity_score`, `dict_diff` |
| `browser-dom` | 4022 | ✅ done | html5ever → ego-tree arena, `ParseOptions` with the libxml2-compat passes, `NodeRef` navigation, lxml `text`/`tail` view, serialization, edit ops |
| `browser-fingerprint` | 2825 | ✅ done | impersonation profiles + aliases, precomputed header sampler (replaces browserforge), `AD_DOMAINS` as a build.rs `phf::Set`, `StatusText`, launch-arg constants |
| `browser-selectors` | 5579 | ✅ done | native CSS via Servo `selectors`+`cssparser`, `::text`/`::attr()`/`:contains()`, selector-group splitting for adaptive identifiers |
| `browser-xpath` | 8012 | ✅ done | **hand-written XPath 1.0 engine** — 13 axes, 27 functions, full type coercion, over a `TreeModel` trait |
| `browser-storage` | 6147 | ✅ done | `StorageSystem` trait, `SqliteStorage` (schema byte-identical to Python's, DBs interoperate both directions), `registrable_domain`, `identifier_hash` |
| `browser-markdown` | 3641 | ✅ done | markdownify port at the reference implementation's defaults, `strip_noise_tags`, `sanitize_for_ai` |
| `browser-http` | 6172 | 🟡 **half** | Unit 11 (config/merge/headers_job/retry/proxy — pure policy, zero I/O) done. **`backend/`, `response.rs`, `session.rs`, `redirect_safe.rs`, `blocking.rs` are still stubs** |
| `browser-cdp` | 6687 | 🟡 **half** | Unit 13 (config/flags/pool/route) done. **`cdp/`, `session.rs`, `response.rs`, `cloudflare.rs` are still stubs** |
| `browser-parser` | 59 | ❌ stub | Unit 10 — the public `Selector`/`Selectors` API |
| `browser-spider` | 81 | ❌ stub | Units 15–17 |
| `browser-templates` | 49 | ❌ stub | Unit 18 |
| `browser-cli` | 49 | ❌ stub | Unit 19 |
| `browser-rs` (façade) | 676 | 🟡 partial | integration tests exist and pass; re-export surface is unit 20 |

All 14 crates are **pre-scaffolded**: every module file exists, every `pub mod` is declared, every
dependency is wired in `[workspace.dependencies]`. Implementers fill in files; they never touch
`Cargo.toml` or `lib.rs`. Keep that discipline — it is what let six agents work the same repo in
parallel without a single merge conflict.

---

## Fidelity achieved so far

Every finished unit was adversarially differential-tested against the real Python:

- **XPath: 3077/3077 (100%)** against lxml/libxml2 — 14 documents, 756 node-set / 1707 number /
  394 string / 144 boolean / 76 rejection cases. 85 rows are *declared* divergences with reasons.
- **CSS: 3948/4159 (94.9%)** against cssselect+lxml. All 211 remainders are itemised in two
  divergence tables (the test **fails if any of them starts passing**). A control run against
  shape-normalised pages gets 1371/1372 — proving the bulk really is html5ever-vs-libxml2 tree shape,
  not matcher bugs. Random-selector fuzzing (~10k comparisons, 3 seeds) found zero new classes.
- **Storage:** identical SQLite schema; a DB written by Python reads in Rust and vice-versa.
- **difflib:** f64 bit-equality against CPython on a generated corpus.
- **Markdown / HTTP policy / browser config:** golden corpora generated from the reference interpreter.

Real bugs this caught (a sample): `:lang(EN)` never matching because cssselect lowercases *both*
sides; attribute values being compared case-insensitively when cssselect emits a case-sensitive XPath
string test; `xlink:href` being seen as `href`; `round(-0.5)` losing the sign of zero; and the
trailing-bare-`*` rewrite in the reference implementation's own translator (`div *::text`) that a fuzzer found.

---

## TODO — remaining work, in order

### Unit 10 — `browser-parser` ⬅️ **START HERE**
*Deps: 2,3,4,5,6,7 (+8). Not parallel — this is the integration point.*
Files: `crates/browser-parser/src/{document,node,nodes,find,similar,text,generate,adaptive,config}.rs`
Spec: `parser.md` (whole file) + ARCHITECTURE §3 and §4.
- The public `Selector`/`Selectors` API — the `Node` enum with the complete text-node degradation table
- `css()`/`xpath()` **with the adaptive save/relocate branch** and its exact warning strings
- `get_all_text` with tail-attributed-to-grandparent semantics
- `FindQuery` builder with the fixed evaluation order + CSS-value escaping
- `find_similar` two-stage filter with the `max(len(a), len(b))` denominator
- `find_by_text` / `find_by_regex` incl. the return-type asymmetry (no match under `first_match=true`
  yields **empty**, not `None`)
- the four selector-generation properties; `relocate` parallelised with rayon

**The seam is proven.** `crates/browser-rs/tests/integration.rs` already implements
`_StorageTools.element_to_dict` using only `browser-dom`'s public API and drives a full adaptive
round trip (parse → CSS-select → XPath on the same tree → fingerprint → save → retrieve → relocate).
Read that test before starting; it is the skeleton of unit 10.

*One naming gap flagged for you:* `browser-dom` has `element_children()` and
`non_comment_children()` but nothing matching lxml's `iterchildren()` (elements + comments + PIs, no
text nodes), which `element_to_dict`'s sibling logic wants. Add it to `browser-dom`.

### Unit 12 — `browser-http` driver, `Response`, sessions
*Deps: 11, 10. Not parallel.*
Files: `backend/{mod,wreq}.rs`, `response.rs`, `session.rs`, `redirect_safe.rs`, `blocking.rs`
- `HttpBackend` trait + `wreq` impl (**pinned 0.15.3** — 0.16 needs rustc 1.98, we're on 1.97)
- the hand-rolled `follow_redirects="safe"` SSRF guard; `max_redirects = -1` → `u32::MAX`
- `Response { status, reason, headers, request_headers, cookies, history, meta, url, encoding, body,
  doc: OnceCell<Document> }`, **lazy** parsing (documented deviation — Python parses eagerly), the
  `adaptive_domain`-replaces-`url` rule
- `AsyncFetcherSession` (pooled, cookie jar) + one-shot `AsyncFetcher`/`Fetcher` + blocking facade
- The policy types are already proven to compose into an attempt loop —
  see `the_http_policy_types_compose_into_one_attempt_loop` in the integration tests.

### Unit 14 — `browser-cdp` CDP driver + Cloudflare solver
*Deps: 13, 12. Not parallel. Hardest remaining unit after 10.*
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

### Unit 15 — `browser-spider` data plane *(parallel-safe with 14)*
`request.rs`, `canonicalize.rs`, `scheduler.rs`, `session.rs`, `stats.rs`, `items.rs`, `checkpoint.rs`
`Request` + SHA-1 fingerprint over sorted-key JSON, `canonicalize_url` (needs its own golden corpus),
`Scheduler` (BinaryHeap + pending mirror + seen set + ticket-based in-flight), `SessionManager`,
`CrawlStats` (atomics + DashMap), `ItemList` exporters (JSON/JSONL/CSV/XML), `CheckpointManager`
(**JSON, not pickle** — see divergences; atomic temp-file+rename; `save` re-raises, `cleanup` swallows).

### Unit 16 — spider support services *(deps 15)*
`throttle.rs` (AutoThrottle: exact `record` formula, `[2,4,8,16,32]` doubling, per-domain DashMap entry
lock **not** a global lock), `robots.rs` (netloc-keyed cache, any-failure-degrades-to-permissive,
`Request-rate` pre-scan), `cache.rs` (base64 JSON, never raises from put, no TTL).

### Unit 17 — spider engine *(deps 15, 16)*
`engine.rs`, `spider.rs`. The `Spider` trait replacing Python subclassing; `CrawlerEngine` with the
Notify-driven loop, global spawn cap + per-domain semaphores, the exact `_process_request` ordering
(robots → cache replay → limiter → delay → fetch → cache write → block detection → autothrottle
feedback → retry-or-callback), Request-before-Item dispatch, the pause/force-stop ladder with
**checkpoint saved before cancel**, streaming via `mpsc::channel(100)`, `ctrl_c()` in the select loop.

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
`wiremock` server, criterion benches (`parse`, `relocate`, `markdown` — the spec pins parse of 5000
nested `div.item` + `css(".item")` under 0.5 s; we should beat that by ~50×), finished README,
`DIVERGENCES.md`, runnable examples for the four entry points.

---

## Housekeeping TODOs (small, do them early)

1. ~~Copy `specs/` into the repo~~ — **done**, commit `e837c0b`. Now at `docs/specs/` (684 KB).
2. ~~Move the corpus generators into the repo~~ — **done**, commit `e837c0b`. All 49 scripts now at
   `tools/corpus/`, with a `README.md` mapping each directory to the testdata it regenerates.
   Note: the scripts still carry absolute paths from the original session — adjust the constants at
   the top before re-running.
3. **Fix `browser-dom`'s `remove_blank_text`** — the single highest-value correctness fix
   outstanding. It currently deletes *every* whitespace-only text node outside `pre`/`textarea`;
   libxml2's `areBlanks()` keeps blanks whose parent allows PCDATA and **never** treats U+00A0 as
   blank. This one bug accounts for 9 CSS divergence rows and **7 of the markdown pipeline
   failures** (`<p><b>b</b> <i>i</i></p>` loses its spaces). Adopting libxml2's rule should close all
   of them at once.
4. Add `iterchildren()` to `browser-dom` (see Unit 10 above).
5. `cargo-deny` is **not installed** on this machine, so `deny.toml` has never actually run.
   `cargo install cargo-deny` and verify CI's deny step passes.
6. The one `#[ignore]`d test — find it (`cargo test --workspace -- --ignored`) and either fix or
   document it.

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
- **Lazy `Response` parsing** (Python parses eagerly, even for binary bodies).
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

Next wave should be: **unit 10 alone** (it's the integration point and everything downstream needs it),
then **12 + 15 in parallel**, then **14 + 16**, then **17**, then **18 + 19**, then **20**.

```bash
cd /Users/himanshu/codes/browser-rs
cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```
