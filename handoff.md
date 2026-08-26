# Handoff — the reference implementation → Rust port

**Date:** 2026-08-26
**Repo:** `/Users/himanshu/codes/browser-rs` (branch `main`)
**Upstream being ported:** [Himasnhu-AT/browser-rs](https://github.com/Himasnhu-AT/browser-rs) v0.4.15 (~16k lines of Python)

---

## TL;DR — where things stand

| | |
|---|---|
| Build | `cargo build --workspace --all-features` ✅ green |
| Tests | **2237 passing, 0 failing, 2 ignored** under `--all-features`; 68 of them launch **real Chrome** and skip cleanly without it |
| Clippy | `--workspace --all-targets --all-features -D warnings` ✅ clean |
| Format | `cargo fmt --all --check` ✅ clean |
| Code written | **120,146 lines across 14 crates** (88,773 of it `src/`) |
| Plan progress | **19 of 20 work units done** (units 1–19) |
| Last commit | `WAVE6HASH Wave 6: the ready-made spiders, the CLI, and the façade re-export surface` |

The two ignored tests are both regeneration aids, not checks, and are meant to stay ignored:
`browser-markdown`'s `emit_rust_answers` (reads `$MD_CASES`, writes `$MD_OUT` for
`tools/corpus/mdgold/step2_golden.py`) and one `ignore`d doc fence in
`browser-http/src/response.rs`.

Commits so far:

```
WAVE6HASH Wave 6: the ready-made spiders, the CLI, and the façade re-export surface
5488cd6   handoff: record the wave-5 commit hash
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

Working tree is clean. **`cargo deny check`'s licence pass is the one red thing** and is
deliberately still red (housekeeping item 5). Everything remaining is **unit 20**.

---

## Read these first (in this order)

1. **`docs/specs/ARCHITECTURE.md`** — 87 KB. Crate layout (§1), every dependency with justification
   (§2), the parse+select design (§3), adaptive relocation (§4), async/error/API ergonomics (§5),
   **the 20-unit plan (§7)**, out-of-scope (§8), the tree-shape sign-off (§11.6).
2. **Subsystem specs** — `docs/specs/`: `parser.md`, `core-types.md`, `http-fetch.md`, `browser.md`,
   `toolbelt.md`, `spider-core.md`, `spider-support.md`, `cli-ai.md` (60–90 KB each, generated from
   the Python source; the implementers work from these, not from the Python).
3. **The four `API.md` contracts** — `crates/browser-parser/API.md` (the `Document`/`Node`/`Nodes`
   surface), `crates/browser-spider/API.md` (the crawl core, `throttle`/`robots`/`cache`, and the
   `Response` ⇄ `FetchedResponse` seam), `crates/browser-templates/API.md` (the ready-made spiders
   and their 19 deviations) and `crates/browser-dom/API.md` (the DOM cheat sheet underneath).
4. **`crates/browser-rs/tests/integration.rs`** — the **34** cross-crate seam tests, and the best map
   of how the crates fit together. Every wave wrote the *next* wave's seam before starting it, and
   it paid every single time. The one to read first now is the newest:
   * **`unit_20_facade_seam`** — four end-to-end user journeys (parse · fetch over HTTP · fetch
     through Chrome · crawl with a templated *and* a user-written spider) written **only** against
     `browser::`. It is the only module in that file which does not reach past the façade, and
     that restraint is its whole assertion. **Unit 20's executable specification.**
   * `crawl_spider_seam`, `site_to_markdown_seam`, `cli_extract_seam` — wave 5's, now shipped as
     units 18 and 19.
5. **`docs/DIVERGENCES.md`** — every intentional behavioural difference from Python, in one place.
   Now ~1,390 lines, through unit 19.

### The reference implementation is installed — use it

```
/Users/himanshu/.claude/jobs/a62e4b8e/tmp/pyref/bin/python
```

A venv with **the reference implementation, lxml 6.1.2, cssselect, markdownify, w3lib, protego**, on CPython
3.14. This is what made every wave trustworthy: instead of reasoning about what Python does, run it
and diff. Every corpus in `crates/*/testdata/*.jsonl` was generated from it, and every "bug found"
below was found by disagreeing with it — never by reading its source and believing yourself.

```bash
PYREF=/Users/himanshu/.claude/jobs/a62e4b8e/tmp/pyref/bin/python
$PYREF -c "from reference.parser import Selector; print(Selector('<p>hi</p>').css('p::text').get())"
```

⚠️ This venv is in a **job temp dir**. If it disappears: `python3 -m venv pyref && ./pyref/bin/pip
install lxml cssselect markdownify w3lib protego the reference implementation`. The Python source is then
readable at `<venv>/lib/python3.14/site-packages/browser-rs` (and the real CLI console script at
`<venv>/bin/browser-rs`), or re-clone Himasnhu-AT/browser-rs from GitHub.

All 87 corpus generator scripts (19 directories) are in-repo at `tools/corpus/` with a `README.md`
mapping each directory to the `testdata/` it regenerates, and a section naming the scripts that are
**not** replayed by `cargo test`. `u10`, `u12`, `u15`, `u16`, `u17`, `u18` and `u19` were written
in-repo and carry no absolute paths; `mdgold` and `xconf` were fixed in wave 3; the rest may still
carry paths from the original session — check the constants at the top before re-running.

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
| `browser-markdown` | 3651 | ✅ done | markdownify port at the reference implementation's defaults, `strip_noise_tags`, `sanitize_for_ai`, `clean_for_ai` |
| `browser-parser` | 8083 | ✅ done | **unit 10** — the public `Document`/`Node`/`Nodes` API, `FindQuery`, `find_similar`, `find_by_text`/`_regex`, the four selector generators, the adaptive save/relocate branch. Wave 6 added `Document::clean_for_ai` and `Node::markdown`. `API.md` is the downstream contract |
| `browser-http` | 15473 | ✅ done | **units 11 + 12** — policy (config/merge/headers_job/retry/proxy), the `HttpBackend` trait + `wreq` driver, the hand-rolled `follow_redirects="safe"` SSRF guard, `Response` (lazy parse, `OnceLock`), `ResponseBuilder`/`ResponseFactory`, the RFC 6265 `CookieJar`, `AsyncFetcherSession`/`AsyncFetcherClient`/`AsyncFetcher` and the blocking façade |
| `browser-spider` | 20471 | ✅ done | **units 15 + 16 + 17** — `Request` + SHA-1 fingerprint, `canonicalize_url`, `Scheduler`, `SessionManager` + `CrawlSession`, `CrawlStats`, `ItemList` exporters, `CheckpointManager`, `AutoThrottle` + `Retry-After`, the `protego` port + `RobotsTxtManager`, `ResponseCacheManager`, `engine.rs` + `spider.rs` (the `Spider` trait, `CrawlerEngine`, `SpiderRunner`, `HttpCrawlSession`) |
| `browser-cdp` | 18113 | ✅ done | **units 13 + 14** — config/flags/pool/route, plus the raw-CDP driver: `CdpPage` with one deadline across `Page.navigate` + `load`, the hand-rolled `networkidle`, `Fetch`-domain interception, `Page.createIsolatedWorld`, the redirect ledger with `…ExtraInfo` header resolution, the response/XHR factory, and the Cloudflare Turnstile solver |
| `browser-templates` | 9697 | ✅ done | **unit 18** — `LinkExtractor` (13 kwargs, `IGNORED_EXTENSIONS`, `strip_html5_whitespace`, `safe_url_string`), `CrawlRule` + `requests_to_follow` **as a free function** + `CrawlSpider`, `SitemapSpider`, `ShopifySpider`, `XMLFeedSpider`, `CSVFeedSpider`, `SiteToMarkdownSpider`, gzip `_decompress` with the 64 MiB bomb cap. `API.md` lists all 19 deviations |
| `browser-cli` | 8530 | ✅ done | **unit 19** — the clap-derive `browser-rs` binary: `extract get/post/put/delete/fetch/stealthy-fetch`, the seconds-vs-milliseconds split, `Convertor` (`write_content_to_file`, the three-mode extension map, `--ai-targeted`), `install`, and `uncurl` with a transcription of CPython's `argparse` **and** `shlex.read_token` |
| `browser-rs` (façade) | 4347 | 🟡 partial | 34 cross-crate seam tests, all passing; the re-export surface is complete and proven by `unit_20_facade_seam`, but the README, benches, examples and `--no-default-features` `cfg` aliases are unit 20 |

**120,146 lines across 14 crates** (88,773 of it `src/`).

All 14 crates were **pre-scaffolded**: every module file exists, every `pub mod` is declared, every
dependency is wired in `[workspace.dependencies]`. Implementers fill in files; they never touch
`Cargo.toml` or `lib.rs`. Keep that discipline — it is what let six agents work the same repo in
parallel without a single merge conflict. (The per-wave *green* agent may touch them, and did in
waves 3, 5 and 6.)

---

## Fidelity achieved so far

Every finished unit was adversarially differential-tested against the real Python:

- **XPath: 3077/3077 (100%)** against lxml/libxml2 — 14 documents, 756 node-set / 1707 number /
  394 string / 144 boolean / 76 rejection cases. 80 rows are *declared* divergences with reasons.
- **CSS: 3933/4159 (94.6%)** against cssselect+lxml. All 226 remainders are itemised in two
  divergence tables (the test **fails if any of them starts passing**). A control run against
  shape-normalised pages gets 1371/1372 — proving the bulk really is html5ever-vs-libxml2 tree shape,
  not matcher bugs. Random-selector fuzzing (~10k comparisons, 3 seeds) found zero new classes.
- **Markdown: 265/279 cases byte-exact on both arms**, 0 differing from markdownify on an identical
  tree.
- **Parser (unit 10) — two corpora, 10496 rows, 10033 reproduce (95.6%).**
  `parser_golden.jsonl` (3488 rows, 96.3%) pins the *algorithms*; `parser_api_golden.jsonl`
  (7008 rows, 95.2%) pins the *public API*, including **23 end-to-end adaptive scenarios through a
  real SQLite database — all 23 reproduce, score tables included.** The 333 misses are itemised one
  by one in `crates/browser-parser/tests/support/divergences.rs`; **not one** is an algorithm
  difference.
- **HTTP driver (unit 12): 2271/2271 field checks (100%)** across 188 corpus rows / 210
  observations against `the reference implementation` + `curl_cffi==0.16.1`, driven over a two-port loopback
  fixture. 15 rows are *declared* divergences. It started at 1925/2060 (93.45%) and the gap was nine
  real bugs.
- **Spider data plane (unit 15): 2352/2358 (99.75%).**
- **Spider support services (unit 16): 172,311/172,447 assertions (99.92%)** over 6,322 corpus rows
  in nine files. Out-of-corpus confidence runs: 47,858 `Retry-After` rows → 0 mismatches; 553,948
  `protego` assertions over 7,012 groups → only the same 9 declared keys. Eleven bugs fixed —
  **and a twelfth in wave 6**, see unit 18 below.
- **Crawler engine (unit 17): 1255/1255 field comparisons (100%)** across **36 real crawls** run on
  both implementations over the same declarative two-host loopback fixture. Mutation-tested with 16
  deliberate engine bugs; the three that escaped are now pinned.
- **Browser driver (unit 14): 311/311 in-crate tests, 66 of them against real Chrome**, replaying a
  35-row corpus produced by driving real the reference implementation + Playwright + stock Google Chrome over a
  declarative loopback fixture. Found four real defects. One declared divergence.
- **Ready-made spiders (unit 18): 596/670 corpus rows (89.0%)** against `the reference implementation` +
  lxml 6.1.2 + w3lib + protego + CPython 3.14, in six files. Per corpus: helpers 151/153 (98.7%),
  links 317/378 (83.9%), sitemap 40/48 (83.3%), shopify 49/49 (100%), csv 24/26 (92.3%), filename
  15/16 (93.8%). **All 74 misses are declared in `tests/u18_golden.rs::DECLARED_DIVERGENCES` and
  asserted to keep differing.**

  **Read the 89.0% correctly: it is *down* from an interim 481/495 (97.2%) on purpose, and both
  halves of the drop are deliberate.** The corpus grew by 175 rows, 42 of them added *because* they
  do not reproduce; and the `matches` comparison was **un-loosened** — it had been forcing both
  sides to `null` whenever CPython raised, which made 18 rows agree by construction. A lower honest
  number replaced a higher dishonest one.

  Out of corpus (generated, run, diffed, reverted): **28,634 of 30,196 observations** across thirteen
  surfaces, every remainder in a declared class — `safe_url_string` 1434/1500, `_url_extensions`
  1392/1500, `remove_tags` 1200/1200, `str.title()` 6996/7002, `LinkExtractor` on well-formed pages
  2400/2400 and on malformed markup 1472/1500, `csv.DictReader` 3000/3000 (CRLF) and 1355/2000 (lone
  CR), `_sm_body` 1200/1200, `_filename_for` 2905/3614, `_process_product` 1200/1200, `XMLFeedSpider`
  1500/1500, `_decompress` 2580/2580. Mutation-tested with 32 deliberate bugs; the four real escapes
  are pinned, including both shared counters.
- **CLI (unit 19): 3,281/3,281 comparable field checks (100%)** — 1105 curl fields over 142 command
  lines, 1093 Click-kwargs fields over 73 invocations, 13 extension-map rows — plus the `Convertor`
  corpus at **1196/1302 rows (91.9%)**, with the remaining 106 in five declared families.

  **That is up from 1083/1302 (83.2%)**: wave 6's green agent closed three whole families, 113 rows,
  by adding the two APIs unit 19 had been approximating around (`Document::clean_for_ai` and
  `Node::markdown`). Four rows filed under those families turned out to have a *different* cause once
  the real one was removed, and were re-classified rather than deleted — which is exactly the signal
  the "assert a declared row still differs" rule exists to produce.

  Out of corpus: ~104,000 generated curl command lines across 17 seeds (102,000 re-run against the
  final code, all agreeing), 24,000 random `extract` argv lines, 9,000 random HTML documents, and —
  the check nobody had run before — **the real `browser-rs` console script versus the built Rust
  binary over one loopback server: 112/126 output files byte-identical**, all 14 differences in
  already-declared families. Mutation-tested with 30 deliberate bugs; 28 caught, and the 2 escapes
  are proven equivalent mutants (documented in code, one with a `debug_assert` that makes the
  redundant step load-bearing).

- **Storage:** identical SQLite schema; a DB written by Python reads in Rust and vice-versa.
- **difflib:** f64 bit-equality against CPython on a generated corpus.

Real bugs this caught (a sample): `:lang(EN)` never matching because cssselect lowercases *both*
sides; attribute values being compared case-insensitively when cssselect emits a case-sensitive XPath
string test; `xlink:href` being seen as `href` (in **both** XPath evaluation paths — the fast path's
agreement test had been passing because both sides were wrong); `round(-0.5)` losing the sign of
zero; the trailing-bare-`*` rewrite in the reference implementation's own translator (`div *::text`) that a fuzzer
found; `<!DOCTYPE>` leaking into `Node::next`/`previous`/`children`; the scheduler collapsing
`i32::MIN` and `i32::MIN + 1` into one queue key; an empty `BytesIO` body being treated as absent (it
is *truthy* in Python); `ParseOptions::encoding` being a label the decoder ignored; the `"safe"` SSRF
guard refusing *every* redirect served by a private host; `timeout` not bounding `page.goto`; a
`referer` in `extra_headers` making Chrome refuse every navigation; and — new in wave 6 —
`python_float` stripping with `str.strip()`'s rule where CPython's numeric parsers strip with C's
`Py_ISSPACE`, so `float(" 4.5\x1c")` answered `Some(4.5)` where CPython raises (395 of 6,000 fuzzed
strings; both `python_float` and `python_int` are now 6,000/6,000).

---

## TODO — remaining work

### ~~Unit 10 — `browser-parser`~~ ✅ **done** (wave 3)
The public `Document`/`Node`/`Nodes` API, verified against 10496 corpus rows.
**`crates/browser-parser/API.md` is the contract every crate from here on codes against.**

### ~~Unit 12 — `browser-http` driver, `Response`, sessions~~ ✅ **done** (wave 4)
2271/2271 corpus field checks, 305 in-crate tests. **`Response` is the only response type in the
workspace**; `Response.url` is the fetched URL (`selector_url()` is the adaptive key);
`follow_redirects="safe"` exempts same-origin hops. Still open and cheap: a non-canonical reason
phrase (`299 Totally Fine`) is lost because it lives in `hyper::ext::ReasonPhrase` and `wreq` does
not re-export it — adding `hyper` to `crates/browser-http/Cargo.toml` closes corpus rows
`status-299` and `status-222`.

### ~~Unit 14 — `browser-cdp` CDP driver + Cloudflare solver~~ ✅ **done** (wave 5)
311 in-crate tests, 66 driving **real Chrome**. `BrowserSession::fetch(url, FetchParams)` →
`browser_http::response::Response`. Browser tests honour `BROWSER_RS_SKIP_BROWSER_TESTS` and an
executable probe. **One 3-permit semaphore** (`live_u14::LIVE_BROWSERS`) guards every live test in
that crate. Cloudflare is structurally verified only.

### ~~Unit 15 — `browser-spider` data plane~~ ✅ **done** (wave 3)
2352/2358 corpus rows. `Scheduler::dequeue` **clones**; `CheckpointManager` is JSON, not pickle.

### ~~Unit 16 — spider support services~~ ✅ **done** (wave 4)
172,311/172,447 differential assertions. **`AutoThrottle::delay_for` is a write** — never call it
from inside `record`, that deadlocks. `RobotsTxtManager` has no single-flight, faithfully.
`ResponseCacheManager::put` never fails a crawl.
**Amended in wave 6:** `throttle::python_float`, `python_int`, `python_is_space`, `python_strip` and
`transform_decimal_and_space` are now `pub` (a second crate wanted them), and the first two had a
real bug — see the last paragraph of "Real bugs this caught" above.

### ~~Unit 17 — spider engine~~ ✅ **done** (wave 5)
1255/1255 field comparisons across 36 real crawls, mutation-tested with 16 deliberate bugs.
A spider that overrides `dispatch` **must** override `has_callback` too, or a restored checkpoint
rewrites every named callback to `parse`.

### ~~Unit 18 — `browser-templates`~~ ✅ **done** (wave 6)
`links.rs`, `crawl.rs`, `sitemap.rs`, `shopify.rs`, `feed.rs`, `site_to_markdown.rs`, `gzip.rs` —
596/670 corpus rows (89.0%) plus 28,634/30,196 out-of-corpus fuzz observations, 213 tests in the
crate. `tests/loopback_crawl.rs` runs all six spiders to completion through the real `CrawlerEngine`
over a `std::net` fixture. What unit 20 must know:

- **`crates/browser-templates/API.md` is the contract**, and its "Deliberate deviations from
  Python" section is 19 numbered items, each pinned by a test or a declared corpus row.
- **A `CrawlRule` always names its callback explicitly.** `requests_to_follow` writes
  `Some(rule.effective_callback())`, because `follow` *inherits* the originating request's callback
  when it is `None` and would parse every page reached from a product page as a product.
- **`SitemapSpider`'s unruled route defaults to `parse`.** `UnruledRoute::InheritSitemapCallback`
  reproduces Python's `_parse_sitemap` inheritance, for anyone who wants it.
- **"`quick-xml` is not a validating parser" is a *new* divergence family**, distinct from the
  html5ever T1–T7 tree-shape table, and it affects `XMLFeedSpider` as well as `SitemapSpider`.
- **A lone `\r` in an unquoted CSV field is a record terminator here and a `_csv.Error` there**, and
  `CSVFeedSpider.parse` does not catch it — so a legacy Mac-line-ending feed kills the callback in
  Python and is read here.
- **Everything is re-exported flat from the façade** under `spider`, with `SiteToMarkdownSpider`
  needing `markdown` as well. `crates/browser-rs/src/lib.rs::the_ready_made_spiders_are_reachable_flat`
  names every one and fails to compile if one moves.
- **Not verified:** no real site anywhere; `ShopifySpider` is never driven end to end by the engine
  (its URL templates are https-only, so the loopback test drives the callbacks directly); the
  loopback crawls run `concurrent_requests(1)`, so nothing proves the engine's *interleaving* matches
  Python's; non-UTF-8 feeds and sitemaps have **zero** coverage; `_checknetloc` is unimplemented and
  no fuzz input reached it, so the size of that class is unknown; `LinkExtractor`'s `process`
  callback cannot be serialised into a corpus row and is unit-tested only; and `XmlNode` models only
  what the template spiders read (no `.tail`, no CDATA distinction, no XPath).

### ~~Unit 19 — `browser-cli`~~ ✅ **done** (wave 6)
`args.rs`, `convert.rs`, `commands/{extract,install,uncurl}.rs` — 3281/3281 comparable field checks,
1196/1302 `Convertor` rows, 206 tests in the crate. What unit 20 must know:

- **`uncurl` carries two transcriptions of CPython**, not wrappers: `argparse`'s `_parse_optional` /
  `_match_arguments_partial` / `_get_values` (including the `--` handling and the 3.11+ prefix-form
  `_negative_number_matcher`), and `shlex.read_token` for `posix=True, whitespace_split=True,
  commenters=''`. The `shlex` **crate** is a different function and is no longer a dependency.
- **Click lets a non-`multiple` option be repeated and takes the last**; clap refused. Every
  non-repeatable argument in `args.rs` now overrides itself. That was 388 of 1,500 random argv lines.
- **The mode comes from the whole path string**, `str::ends_with`, not `Path::extension` and not
  `Path::file_name` — so `archive.tar.md` is markdown and `dir.md/` is an error, on both sides.
- **`extract_content` is Python's `_extract_content` statement for statement** since wave 6:
  cleaning happens only when `main_content_only` is set, for all three modes, and each CSS match is
  one piece. `scrub_for_ai` is still public but is no longer on that path.
- **Not verified:** `browser-rs install`'s download has **never been run** (~150 MB over a network
  this environment does not have) and `install` has only ever been exercised on macOS;
  `extract fetch` / `stealthy-fetch` are never run end to end (what is checked is that the assembled
  `BrowserConfig` matches Python's kwargs dict field for field over 24,000 fuzzed command lines);
  proxies, TLS verification and impersonation are compared as *arguments only*; the output file is
  always UTF-8, with no corpus row for a non-UTF-8 page; the help *output* was compared by eye, not
  by a corpus (five exit codes were checked against the real console script); and `e2e_compare.py`'s
  112/126 is a manual point-in-time measurement, not a regression guard — nothing in `cargo test`
  reproduces it.

### Unit 20 — façade, docs, conformance, benchmarks

**This is all that remains.** Four deliverables, in rough order of what is blocked on what.

**1. The re-export surface is *done* and proven — do not redo it, extend it.**
`crates/browser-rs/src/lib.rs` is 722 lines and re-exports every member crate that appears in a public
signature. Wave 6 closed five holes while writing `unit_20_facade_seam`:

- `browser-dom`, `browser-xpath`, `browser-storage` and `browser-markdown` were
  `[dev-dependencies]` only, so four types the public API *hands back* had no path through the
  façade. They are now optional `[dependencies]` re-exported as `browser::{dom, xpath, storage,
  markdown}`. None of them adds a compilation unit.
- `CallbackStream` was missing from the flat spider surface.
- **`async_trait` and `futures` were not re-exported at all**, and a user-written `Spider` needs both
  — the trait is declared `#[async_trait]` and `CallbackStream` is a `futures` type alias. A surface
  that makes a trait nameable but not implementable looks complete and is not.

The rule to keep: **every `#[cfg(feature = …)]` item in a member crate needs a matching forwarding
entry here, and nothing a public method returns may be unnameable.** The three tests that enforce it
are `the_ready_made_spiders_are_reachable_flat`, `the_types_the_document_api_hands_back_have_facade_paths`
and `unit_20_facade_seam`.

What is genuinely left on the surface: the **`cfg` aliases** for the sharp edge documented in
`lib.rs` — `markdown` and `adaptive` are written as `browser-parser/markdown`, which pulls the
crate in *without* turning on `feature = "parser"` here, so `--no-default-features --features
markdown` builds and exposes no `Document`. The failing direction cannot happen, so this is a
*smaller* surface and never a broken one, but it should be closed.

**2. Criterion benches (`parse`, `relocate`, `markdown`) — BLOCKED, read this first.**
`criterion` is declared in `[workspace.dependencies]` and is **not in `Cargo.lock`**, and crates.io
is not reachable from this environment. `cargo bench` cannot be wired up until someone runs
`cargo add --dev criterion -p browser-rs` on a machine with a network. The spec pins parse of 5000
nested `div.item` + `css(".item")` under 0.5 s; a rough `cargo test --release` timing suggests we
beat that by ~50×, but **that has not been measured with criterion and should not be claimed until
it is.**

**3. The README, `examples/`, and the doc pass.** The four entry points to write examples for are
exactly the four journeys in `unit_20_facade_seam` — lift them. `README.md` at the repo root is still
the wave-1 scaffold-era text and needs rewriting against what shipped.

**4. `docs/DIVERGENCES.md` is complete through unit 19** (~1,390 lines) and needs only a final
editorial pass — the T1–T7 html5ever table, the CSS-Selectors-4 supersets, and the two new families
wave 6 added ("`quick-xml` is not a validating parser", and "lxml has no root") would read better
merged into one top-level tree-shape section than as five per-unit lists.

**One more thing worth doing:** `crates/browser-rs/tests/integration.rs` is now 3,500+ lines and 34
tests. `crawl_spider_seam` and `site_to_markdown_seam` are duplicated in spirit by
`crates/browser-templates/tests/loopback_crawl.rs`, which runs the *shipped* types rather than
sketches of them. If the wave wants to trim, those two seam modules are the redundant half —
`unit_20_facade_seam` is not, because it is the only thing that checks the façade from outside.

---

## Housekeeping TODOs

1. ~~Copy `specs/` into the repo~~ — **done**, commit `e837c0b`. Now at `docs/specs/` (684 KB).
2. ~~Move the corpus generators into the repo~~ — **done**, commit `e837c0b`. All 87 scripts now at
   `tools/corpus/` across 19 directories, with a `README.md` mapping each to the testdata it
   regenerates and a section naming the ones `cargo test` does **not** replay.
3. ~~Fix `browser-dom`'s `remove_blank_text`~~ — **done.** It now implements libxml2's
   `areBlanks()`, established empirically against libxml2 2.14.6.
4. ~~Add `iterchildren()` to `browser-dom`~~ — **done**, pinned by
   `tests/downstream_contract.rs::iterchildren_matches_lxmls_child_sequence`.
5. **`cargo deny check` reports `advisories ok, bans ok, licenses FAILED, sources ok`.**

   🔴 **STILL OPEN, AND STILL THE ONE RED THING IN THE REPO.** There is exactly **one**
   `error[rejected]`, unchanged since wave 5: **`wreq-util 0.1.0` is GPL-3.0 and this workspace is
   MIT.** It is the emulation-profile table behind `browser-http`'s `impersonate` feature — which
   is **on by default** — so as things stand a default build of `browser-rs` links copyleft code
   into an MIT crate. `wreq` itself is Apache-2.0; only the profile crate is GPL. This is **not**
   something a green agent should allowlist away, so `deny.toml`'s `exceptions` is deliberately still
   empty. A human has to pick one of:

   1. relicense the workspace to GPL-3.0;
   2. take `impersonate` out of `browser-http`'s default features and ship the JA3/JA4 profiles as
      an opt-in GPL build, documented as such;
   3. replace `wreq-util` with permissively licensed profile tables — the mapping in
      `crates/browser-http/src/backend/wreq.rs` is ours, only the profile *data* is theirs.

   The reasoning is repeated in `deny.toml` next to the empty `exceptions`, so nobody closes it by
   accident. One licence *was* added legitimately: `CDLA-Permissive-2.0`, for `webpki-root-certs` —
   the Mozilla root store, a permissive **data** licence with no copyleft reach into the code.
6. ~~The one `#[ignore]`d test~~ — **done, it is not a bug.** It is `emit_rust_answers`, a
   regeneration aid that reads `$MD_CASES` and writes `$MD_OUT`. Leave it ignored.
7. **One test was observed flaky, once.**
   `browser-spider::engine::tests::a_real_crawl_over_a_loopback_server` failed a single time
   during wave 6 under a full `cargo test --workspace` on a loaded machine, with `/admin/secret`
   crawled. The mechanism is that a `robots.txt` fetch which times out is treated as "allow
   everything" — **faithfully**, Python does the same — so a slow loopback reply turns into a wrong
   answer rather than an error. Its transport timeout was raised from 5 s to 30 s, which is a load
   margin and not a latency one. It has not reproduced since (three full plain runs, two full
   `--all-features` runs), but it is written down here because a test that fails once in fifty is
   worse than one that fails always. If it comes back, the fix is probably to give the fixture's
   `robots.txt` route priority rather than to raise the timeout again.
8. `browser-markdown/tests/sanitize_golden.rs` still strips whitespace before comparing, because
   its golden files store only the stripped form and its generator is **not** in `tools/corpus/`.
   Now mostly moot (the inputs in `testdata/normalized/` are minified), but if anyone regenerates
   that corpus, tighten the comparison at the same time.
9. ~~**`wiremock` is declared and is not in `Cargo.lock`**~~ — **done in wave 6: the line is gone.**
   Vendoring it needs crates.io, which is not reachable here, and the workspace now has **six**
   hand-rolled `std::net` / `tokio` loopback fixtures that do the job
   (`crates/browser-http/tests/u12_support/mod.rs`,
   `crates/browser-spider/tests/u17_support/mod.rs`,
   `crates/browser-cdp/src/live_u14/server.rs`,
   `crates/browser-templates/tests/loopback_crawl.rs`, `crates/browser-cli/tests/support/mod.rs`
   and `crawl_loop_seam::Fixture` + `local_site::Site` in the façade's integration test).
   **`local_site::Site` is the one to copy**: a `fn(&str) -> Option<Page>` route table, one thread per
   connection (a browser opens several at once), and it half-closes (`Shutdown::Write` + drain to
   EOF) rather than RSTing.
10. **`hyper` is not a `browser-http` dependency**, which is the only reason a non-canonical reason
   phrase is lost (corpus rows `status-299`, `status-222`). One line in `Cargo.toml` closes it.
11. ~~**`httpdate` and `texting_robots` are unused**~~ — **done.** `httpdate` went in unit 17;
    `texting_robots` and `shlex` were the last two dangling `[workspace.dependencies]` lines and both
    are gone in wave 6.
12. **`Document::markdown_with` re-parses each CSS match out of context.** For a `<td>` — and any
    other element with a restricted parent — the HTML5 algorithm foster-parents it away and the cell
    is lost, where markdownify keeps it. The fix is to parse the fragment in its *parent's* context,
    the way `innerHTML` does. Four corpus rows are declared as `TABLE_CELL_FRAGMENT` in
    `crates/browser-cli/tests/u19_convert_golden.rs` and would need deleting when it lands.
13. **Non-UTF-8 in three places, all one missing dependency edge.** `crates/browser-templates` has
    no `encoding_rs` and `quick-xml`'s `encoding` feature is off, so an `<?xml encoding=…?>` is
    ignored; `crates/browser-cli` has none either, so the output file is always UTF-8; and
    `safe_url_string`'s `encoding` parameter is accepted and ignored. All three close together.

---

## Known divergences carried forward (all deliberate, all in `docs/DIVERGENCES.md`)

- **html5ever vs libxml2 tree shape** — implied `<tbody>`, synthesised `<head>`, the HTML5 adoption
  agency on `<p>x<b>y<i>z</p>`. ~250 corpus rows now, across units 5, 6, 10 and 19. Accepted by
  ARCHITECTURE §11.6: html5ever's answer is the one *browsers* produce, which is what a scraper
  wants. Two sub-families worth knowing by name: **`<noscript>` and `<template>` leave `<head>`**
  under the HTML5 algorithm, and **lxml has no root at all** for a document with no element — where
  Python *raises* and this port succeeds (42 rows in unit 19, 27 in unit 18, found independently).
- **`quick-xml` is not a validating parser** — a *new* family in unit 18, distinct from the above.
  lxml rejects an undeclared namespace prefix, an unescaped `<` in an attribute value and a redefined
  attribute; `_sm_body` turns each into an empty result plus a warning, losing the sitemap. This port
  reads all three. Affects `SitemapSpider` and `XMLFeedSpider`.
- **CSS-Selectors-4 supersets** — we accept `:has()`, `:not(a, b)`, `.x:first-of-type`,
  `:nth-child(2 of .x)`; cssselect rejects them. Cannot change the meaning of any selector that works
  in Python.
- **lxml's sibling-combinator subtree escape** — `node.css("* ~ *")` reaching the query node's own
  following siblings. Unfixed; Servo's `Element` trait cannot distinguish a combinator step from
  `:first-child`. The opposite-direction bug (over-matching) *was* fixed.
- **Vintage drift** — `psl`'s public-suffix snapshot is newer than `tld` 0.13.2's (40/20306 hosts
  differ); `char::to_lowercase` is newer than CPython 3.14's UCD 16.0.0. Unit 18's `python_title`
  tables are the same family from the other side: `rustc` 1.97 is *ahead* of CPython on 28 case
  mappings and 5 cased-ness answers.
- **`urlsplit` cannot fail.** CPython raises `ValueError("Invalid IPv6 URL")` for an unbalanced
  bracket and most callers do not catch it. `canonicalize::urlparse` is total workspace-wide;
  `safe_url_string` is the deliberate exception, because w3lib's caller *does* catch.
- **Checkpoints are JSON, not pickle.** Directory layout, atomic write, `interval` semantics and log
  lines all preserved.
- **Lazy `Response` parsing** (Python parses eagerly, even for binary bodies), behind a `OnceLock`,
  which is what keeps it `Send + Sync`.
- **`follow_redirects="safe"` exempts same-origin hops**, because curl runs `CURLFOLLOW_SAFE` inside
  connection setup and a reused connection is never re-checked. The security property is unchanged.
- **`Response.url` is the fetched URL, not `adaptive_domain`.** Python's substitution quietly breaks
  `urljoin`; here the domain is only the adaptive storage key (`Response::selector_url()`).
- **Nine `robots.txt` text codecs decode as UTF-8** (all ASCII supersets, so the ruleset is
  identical), and a `Request-rate` denominator larger than `i64` is ignored.
- **Encoding labels go through WHATWG, not `iconv`** — `iso-8859-1`/`latin1`/`ascii` are aliases of
  windows-1252, so the 32 bytes `0x80..=0x9F` differ; and an encoding problem never destroys the
  document (Python raises `LookupError`, or lxml returns an empty tree, where we warn and degrade to
  U+FFFD).
- **Boolean attributes** — libxml2 gives `checked` the value `"checked"`, html5ever gives `""` and
  cannot tell `checked` from `checked=""` at all. 89 corpus rows in unit 10, and it moves
  `find_similar` scores, not just serialisation.
- **IDNA is UTS-46 non-transitional, not IDNA 2003** — they differ on `ß`, `ς`, ZWJ and ZWNJ.
  Units 15 and 18, same cause.
- IPv6 proxy hosts keep their brackets (Python emits the unusable `http://::1:9050`).

## Out of scope (decided, with reasons in ARCHITECTURE §8)

MCP/AI server · IPython shell · Scrapy integration · browserforge's Bayesian header sampler ·
curl-impersonate FFI · Patchright · pickle compatibility · libxml2 recovery bug-compat · `huge_tree` ·
Camoufox-era options (`geoip`/`humanize`/`block_images`/… — already deleted upstream in v0.3.13).

---

## How to drive the next session

The pattern that worked six times, and is worth repeating for the last one:

1. One `Workflow` per wave. Parallel `agent()` per unit, each owning a **disjoint file set**;
   never let two agents touch one file.
2. Immediately after each implementer, a **verifier** agent in the same pipeline stage that generates
   a golden corpus from the reference Python and fixes the Rust until it matches. Instruct it
   explicitly: *never shrink the corpus to raise the pass rate; encode unreproducible cases as
   declared divergence rows that fail if they ever start passing.* That instruction is what produced
   the honest 89.0% instead of a fake 100% — and, in wave 6, what caught four rows that had been
   filed under the wrong cause.
3. A final **green** agent per wave that may edit anything: reconcile cross-crate API drift, run
   `build`/`test`/`clippy`/`fmt`, action the implementers' `requests_for_green`, write a composition
   test proving the next wave's seam works, append to `DIVERGENCES.md`, and commit.
4. **Write the next wave's seam test before the next wave starts.** This has paid every single time.
   Wave 4's `browser_response_seam` / `crawl_loop_seam` turned three vague API questions into three
   small commits. Wave 5's `crawl_spider_seam` / `site_to_markdown_seam` / `cli_extract_seam`
   surfaced that the `markdown` feature never reached `browser-http`, which would have been unit
   19's first hour. Wave 6's `unit_20_facade_seam` found five holes in the re-export surface,
   including two — `async_trait` and `futures` — that would have made the `Spider` trait *nameable
   but not implementable* for every façade user.
5. **Run `cargo deny check`.** It found a GPL-3.0 crate in an MIT workspace the first time it was
   run, and that question is still open.

Waves 1–6 are done (units 1–19). Remaining: **unit 20**, and it is one agent's work, not a wave.

```bash
cd /Users/himanshu/codes/browser-rs
cargo build --workspace --all-features \
  && cargo test --workspace --all-features \
  && cargo clippy --workspace --all-targets --all-features -- -D warnings \
  && cargo fmt --all --check
# and, separately, because it is expected to FAIL on licences until the
# `wreq-util` GPL question is answered:
cargo deny check          # advisories ok, bans ok, licenses FAILED, sources ok

# The browser suite takes ~50 s because 68 tests launch real Chrome. To run
# without one (and prove the skip path still works):
BROWSER_RS_SKIP_BROWSER_TESTS=1 cargo test --workspace --all-features
```
