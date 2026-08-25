# Handoff — the reference implementation → Rust port

**Date:** 2026-08-25
**Repo:** `/Users/himanshu/codes/browser-rs` (branch `main`)
**Upstream being ported:** [Himasnhu-AT/browser-rs](https://github.com/Himasnhu-AT/browser-rs) v0.4.15 (~16k lines of Python)

---

## TL;DR — where things stand

| | |
|---|---|
| Build | `cargo build --workspace --all-features` ✅ green |
| Tests | **1268 passing, 0 failing, 1 ignored** (same under `--all-features`) |
| Clippy | `--workspace --all-targets -D warnings` ✅ clean, with and without `--all-features` |
| Code written | ~65k lines across 14 crates |
| Plan progress | **13 of 20 work units done** (units 1–11, 13, 15) |
| Last commit | `3824cbb Wave 3: the public document API, the spider data plane, and the Response seam` |

The one ignored test is `browser-markdown`'s `emit_rust_answers` — a **corpus regeneration aid**,
not a check: it reads `$MD_CASES` and writes `$MD_OUT` for `tools/corpus/mdgold/step2_golden.py`.
It is meant to stay ignored. (Housekeeping item 6, closed.)

Commits so far:

```
3824cbb  Wave 3: the public document API, the spider data plane, and the Response seam
9a29998  handoff: mark the two preservation TODOs done
e837c0b  Preserve specs, corpus generators and the session handoff in-repo
8228e99  Wave 2: selectors, XPath, storage, markdown, HTTP policy, browser config
8cc1f38  Units 2/3/4/9: core, dom and fingerprint foundation crates
fe30e17  Unit 1: workspace scaffolding for the Rust port of the reference implementation
```

Working tree is clean. Nothing is red. Pick up at **Unit 12**, in parallel with **Unit 16**.

---

## Read these first (in this order)

1. **`docs/specs/ARCHITECTURE.md`** — 87 KB. Crate layout (§1), every dependency with justification
   (§2), the parse+select design (§3), adaptive relocation (§4), async/error/API ergonomics (§5),
   **the 20-unit plan (§7)**, out-of-scope (§8), the tree-shape sign-off (§11.6).
2. **Subsystem specs** — `docs/specs/`: `parser.md`, `core-types.md`, `http-fetch.md`, `browser.md`,
   `toolbelt.md`, `spider-core.md`, `spider-support.md`, `cli-ai.md` (60–90 KB each, generated from
   the Python source; the implementers work from these, not from the Python).
3. **`crates/browser-parser/API.md`** — the public `Document`/`Node`/`Nodes` surface. Every crate
   from unit 12 onwards codes against this one.
4. **`crates/browser-dom/API.md`** — the DOM cheat sheet underneath it.
5. **`crates/browser-rs/tests/integration.rs`** — the cross-crate seam tests. `response_seam` is the
   skeleton of unit 12's `Response`.
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

All 49 corpus generators are in-repo at `tools/corpus/` with a `README.md` mapping each directory to
the `testdata/` it regenerates. Their path constants were fixed for `mdgold` and `xconf` during wave 3;
the rest may still carry absolute paths from an older session — check before re-running.

---

## What exists today

| Crate | LOC | Status | What it is |
|---|---:|---|---|
| `browser-core` | 4338 | ✅ done | `Override<T>`, `TextExt` (Python `findall` group-flattening), w3lib entity port + cp1252 remap, `AttributeMap`, `round_half_even`, cookie/header parsing, tracing `LevelCounter` — **plus** a bit-exact CPython `difflib.SequenceMatcher` port, `ElementFingerprint`, `similarity_score`, `dict_diff` |
| `browser-dom` | 4584 | ✅ done | html5ever → ego-tree arena, `ParseOptions` with the libxml2-compat passes (incl. libxml2's real `areBlanks()`), byte bodies **decoded with the caller's encoding**, `NodeRef` navigation, lxml `text`/`tail` view, libxml2-faithful serialization, edit ops |
| `browser-fingerprint` | 2825 | ✅ done | impersonation profiles + aliases, precomputed header sampler (replaces browserforge), `AD_DOMAINS` as a build.rs `phf::Set`, `StatusText`, launch-arg constants |
| `browser-selectors` | 5847 | ✅ done | native CSS via Servo `selectors`+`cssparser`, `::text`/`::attr()`/`:contains()`, selector-group splitting for adaptive identifiers |
| `browser-xpath` | 8039 | ✅ done | **hand-written XPath 1.0 engine** — 13 axes, 27 functions, full type coercion, over a `TreeModel` trait |
| `browser-storage` | 6147 | ✅ done | `StorageSystem` trait, `SqliteStorage` (schema byte-identical to Python's, DBs interoperate both directions), `registrable_domain`, `identifier_hash` |
| `browser-markdown` | 3651 | ✅ done | markdownify port at the reference implementation's defaults, `strip_noise_tags`, `sanitize_for_ai` |
| `browser-parser` | 7991 | ✅ done | **unit 10** — the public `Document`/`Node`/`Nodes` API, `FindQuery`, `find_similar`, `find_by_text`/`_regex`, the four selector generators, the adaptive save/relocate branch. `API.md` is the downstream contract |
| `browser-spider` | 7662 | 🟡 **data plane done** | **unit 15** — `Request` + SHA-1 fingerprint, `canonicalize_url`, `Scheduler`, `SessionManager` + the `CrawlSession` trait, `CrawlStats`, `ItemList` exporters, `CheckpointManager`. **`throttle.rs`, `robots.rs`, `cache.rs` (16) and `engine.rs`, `spider.rs` (17) are still stubs** |
| `browser-http` | 6172 | 🟡 **half** | Unit 11 (config/merge/headers_job/retry/proxy — pure policy, zero I/O) done. **`backend/`, `response.rs`, `session.rs`, `redirect_safe.rs`, `blocking.rs` are still stubs** |
| `browser-cdp` | 6687 | 🟡 **half** | Unit 13 (config/flags/pool/route) done. **`cdp/`, `session.rs`, `response.rs`, `cloudflare.rs` are still stubs** |
| `browser-templates` | 49 | ❌ stub | Unit 18 |
| `browser-cli` | 49 | ❌ stub | Unit 19 |
| `browser-rs` (façade) | 1225 | 🟡 partial | 12 cross-crate seam tests, all passing; the re-export surface is unit 20 |

**~65k lines across 14 crates.**

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

### Unit 12 — `browser-http` driver, `Response`, sessions ⬅️ **START HERE**
*Deps: 11, 10. Parallel-safe with unit 16.*
Files: `backend/{mod,wreq}.rs`, `response.rs`, `session.rs`, `redirect_safe.rs`, `blocking.rs`
- `HttpBackend` trait + `wreq` impl (**pinned 0.15.3** — 0.16 needs rustc 1.98, we're on 1.97)
- the hand-rolled `follow_redirects="safe"` SSRF guard; `max_redirects = -1` → `u32::MAX`
- `Response { status, reason, headers, request_headers, cookies, history, meta, url, encoding, body,
  doc: OnceLock<Document> }`, **lazy** parsing (documented deviation — Python parses eagerly), the
  `adaptive_domain`-replaces-`url` rule
- `AsyncFetcherSession` (pooled, cookie jar) + one-shot `AsyncFetcher`/`Fetcher` + blocking facade

**The seam is proven — read `crates/browser-rs/tests/integration.rs::response_seam` first.** It builds
the exact `Response` shape above out of raw bytes + headers + a `SelectorConfig`, parses it lazily
through `Document::parse_content`, runs css/xpath/find_all on it, drives a full adaptive
save-then-relocate across **two separate responses sharing one `SqliteStorage`**, and races the lazy
parse from two threads. Five things it settles for you:

1. **`Document::parse_content(Option<&Body>, &DocumentConfig)`** is `Selector.__init__`'s
   "content or root" check — use it, so a redirect hop with an empty body is representable.
2. **`SelectorConfig` → `DocumentConfig` is yours to write**; it cannot live in either crate without
   a new dependency edge. The seam test has a working version, and
   `every_fetcher_parser_keyword_lands_somewhere_in_document_config` pins that all seven
   `PARSER_KEYWORDS` have a home.
3. **`adaptive_domain` replaces the URL only for storage**, via `DocumentConfig::effective_url()`;
   `Response.url` keeps the fetched URL.
4. **`charset=` now matters.** `browser-dom` decodes a byte body with `ParseOptions::encoding` as of
   this wave; before it did not, and the label was dead metadata. Port
   `__CHARSET_RE__` = `charset=["']?([\w-]+)` (the seam test carries a copy) for backends that do not
   resolve a charset themselves.
5. The policy types compose into an attempt loop —
   `the_http_policy_types_compose_into_one_attempt_loop`.

Owed to unit 15: `impl From<Response> for FetchedResponse` (and the reverse construction for the
dev-mode cache replay). `a_fetched_response_round_trips_to_a_parsed_document` writes both by hand.
`CrawlSession::fetch` takes `&Request` and **must not mutate `request.session_kwargs`** — Python had a
bug there that turned a retried POST into a GET.

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
- its `Response` is the same shape as unit 12's; share the `SelectorConfig` → `DocumentConfig` mapping
  rather than writing a second one. `__extract_browser_encoding` is *only* used on this side in
  Python (`curl_cffi` resolves the charset for the HTTP engines), and it now has real effect.

### ~~Unit 15 — `browser-spider` data plane~~ ✅ **done** (wave 3)
`request.rs`, `canonicalize.rs`, `scheduler.rs`, `session.rs`, `stats.rs`, `items.rs`, `checkpoint.rs`
— 2352/2358 corpus rows, 125 tests. Two things unit 17 must know:
- **`Scheduler::dequeue` clones** the request out of the pending mirror, so mutating a dequeued
  request will not show up in a later `snapshot()`. Python's mirror holds the same object and does.
  Either don't mutate in-flight requests, or re-enqueue a copy. Not corpus-covered.
- **`CheckpointManager` is JSON, not pickle** (declared divergence): a `checkpoint.pkl` written by
  Python is not resumable, by design. `save` re-raises, `cleanup` swallows.

### Unit 16 — spider support services *(deps 15; parallel-safe with 12 and 14)*
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
   `mdgold` and `xconf` had their absolute paths fixed in wave 3 and were re-run successfully;
   `u10` and `u15` were written in-repo. The rest may still carry paths from the original session —
   check the constants at the top before re-running.
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
- **Lazy `Response` parsing** (Python parses eagerly, even for binary bodies). Proven to work end to
  end by `response_seam` before unit 12 exists.
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

Waves 1–3 are done (units 1–11, 13, 15). Remaining: **12 + 16 in parallel**, then **14**, then **17**,
then **18 + 19**, then **20**. Unit 12 is the one everything else waits on — 14 needs its `Response`
shape and 17 needs a real `CrawlSession`.

```bash
cd /Users/himanshu/codes/browser-rs
cargo build --workspace --all-features \
  && cargo test --workspace \
  && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo fmt --all --check
```
