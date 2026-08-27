# Divergences from the Python reference

Every intentional difference in behaviour, in one place, so that none of them is
an accident.

## How to read this file

The register is ordered the way the port was built — by **wave**, then by the
**work unit** inside it. That ordering is kept rather than regrouped by subject,
because most entries only make sense next to the differential corpus that found
them, and the corpora were generated per unit.

Four kinds of section recur, and the distinction between them is the point:

| Section | What it means |
|---|---|
| *Still divergent* | The port behaves differently on purpose. There is a reason, and a test asserts the difference. |
| *Fixed here, not divergences* | The reference is wrong or inconsistent and this port is not. Recorded so nobody "restores fidelity" by reintroducing a bug. |
| *Not divergences after all* | Suspected differences that the corpus disproved. Recorded so they are not re-investigated. |
| *Notes* | Decisions this port made where the reference has no opinion at all. |

Two rules hold everywhere in this document, and they are why it can be trusted:

1. **Every divergence is asserted by a test.** The corpus rows carry both
   answers. A divergence that silently *stopped* being one would fail the suite
   just as loudly as one that appeared — see `docs/AGENTS.md`, "corpus honesty".
2. **Nothing is removed to raise a pass rate.** Golden rows are not deleted,
   corpora are not shrunk, and comparisons are not loosened. An unreproducible
   case is encoded as a declared divergence with a written reason.

## Contents

| Wave | Units | Starts at |
|---|---|---|
| Build level | 1 | [Build-level divergences](#build-level-divergences-unit-1) |
| 2 | 5, 6, 7, 8, 11, 13 | [Wave 2](#wave-2--units-5-6-7-8-11-and-13) |
| 3 | 10 | [Wave 3 — unit 10](#wave-3--unit-10-browser-parser) |
| 3 | 15 | [Wave 3 — unit 15](#wave-3--unit-15-browser-spider-data-plane) |
| 3 integration | 12/14 preflight | [the `Response` seam](#wave-3-integration--the-response-seam-units-12--14-preflight) |
| 4 | 12, 14, 16, 17 | [Wave 4](#wave-4--units-12-14-16-and-17) |
| 5 integration | 18/19 preflight | [Wave 5 integration](#wave-5-integration--the-seams-for-units-18-and-19) |
| 6 | 18, 19, façade preflight | [Wave 6](#wave-6--units-18-and-19-and-the-façade-re-export-surface) |
| 7 | 20 (the façade) | [Wave 7](#wave-7--unit-20-the-façade) |

## Build-level divergences (unit 1)

### `wreq` is pinned to 0.15.x, not 0.16

`ARCHITECTURE.md` §2.3 selects `wreq` 0.16.0 with `wreq-util` 0.2.0. Both of
those releases declare `rust-version = 1.98`, and this workspace is pinned to
1.97.0 in `rust-toolchain.toml`, so the MSRV-aware resolver will not select
them. We pin one minor back:

| Crate | §2 says | Pinned here |
|---|---|---|
| `wreq` | 0.16.0 | **0.15.3** |
| `wreq-util` | 0.2.0 | **0.1.0** |

This matters to work unit 12, which writes the `wreq` backend: `wreq-util` 0.1
exposes a smaller set of browser emulation profiles than 0.2, and the
`Emulation` builder API changed between the two. Check the 0.1 docs rather than
the 0.2 docs. Raising `rust-toolchain.toml` to 1.98 later is the whole of the
upgrade.

### `chromiumoxide` is taken at its default features only

`chromiumoxide`'s `rustls` and `native-tls` features expand to
`chromiumoxide_fetcher/<tls>` using the non-optional syntax, which
*force-enables* the optional `chromiumoxide_fetcher` crate while granting it
only that one feature — never its default `zip8` backend. The result fails to
compile under `--no-default-features`. `browser-cdp` therefore depends on
`chromiumoxide` with its own default (`bytes`) and nothing else; the Chromium
downloader that unit 19 needs comes from a direct `chromiumoxide_fetcher`
dependency in `browser-cli`, with that crate's own defaults intact.

### `serde_json` is built with `preserve_order`

Attribute and key order is load-bearing in several places the reference implementation's tests pin
(`dict_diff` compares ordered key/value tuples; `CrawlStats::to_dict` has an
exact key order; `orjson` preserves insertion order). Enabling
`serde_json/preserve_order` workspace-wide makes `serde_json::Map` an
`IndexMap`, which matches Python `dict` semantics. This is a deliberate global
choice, not an accident of feature unification.

---

# Wave 2 — units 5, 6, 7, 8, 11 and 13

Each of these was found by differentially verifying the Rust crate against
`the reference implementation` (with `lxml` 6.1.2 / libxml2 2.14.6, `cssselect`,
`markdownify` 1.2 and CPython 3.14) over a generated corpus. Everything listed
here is **asserted** by a test: the corpus rows carry both answers, and the test
fails if a divergence silently disappears.

## The shared root cause: the HTML tree (units 3/4)

Three of the six units divergences trace back to one thing — `browser-dom`
parses with html5ever, and the reference parses with libxml2's HTML parser.
This is `ARCHITECTURE.md` §11.6's accepted trade: html5ever is the browser's
answer, libxml2 is not. It is recorded once here rather than six times below.

| # | Difference | Where it shows |
|---|---|---|
| T1 | html5ever synthesises an implied `<tbody>` and a `<head>` that libxml2 does not build | CSS `:nth-child` / child combinators, XPath step counts, `markdownify`'s `convert_tr` (a `<caption>` before the first row becomes invisible, and an empty header row is inferred), and one extra low-scoring relocation candidate |
| T2 | ~~`remove_blank_text` deletes **every** whitespace-only text node~~ — **fixed.** `remove_blank_text` now reimplements libxml2's `areBlanks()`; see "Blank text: the one rule that is now shared" below for the two lexer-level residues | was `:empty`, `::text`, `element.text` in a fingerprint, and Markdown for ordinary prose |
| T3 | Malformed markup recovers per the HTML5 spec (adoption agency, foster parenting, auto-closing), not per libxml2 | `<p>x<b>y<i>z</p>`, text inside `<table>`, `<h6><h6>x</h6></h6>`, a `<table>` inside an `<h4>` |
| T4 | A valueless HTML attribute is `""` here; libxml2 gives it its own name (`checked="checked"`), and lower-cases SVG attribute names where html5ever keeps the camel case (`viewBox`) | `[checked="checked"]`, `svg::attr(viewBox)` |
| T5 | html5ever keeps `xmlns:*` attributes that libxml2's HTML parser discards, and applies the HTML5 named-character-reference rule (`&notreal;` → `¬real;`) that `html.parser` does not | `count(//*/namespace::*)`, entity round-trips |
| T6 | `<noscript>` is tokenised with `scripting_enabled: false`, so its children are markup — matching libxml2 and `html.parser`. A `<noscript>` that *starts* the document still lands in `<head>`, where HTML5 pops it and moves the following `<p>` into `<body>` | `<noscript>` at the very start of a document |
| T7 | The HTML5 tokenizer eats one `U+000A` immediately after `<pre>`/`<textarea>`; libxml2 does not. Our serialiser also does not re-emit the newline the HTML serialisation spec requires, so `<pre>` is not round-trip stable | invisible at the reference implementation's defaults (`strip_pre` removes leading newlines anyway); visible under a non-default `strip_pre` |

## Blank text: the one rule that is now shared

`browser-dom`'s `remove_blank_text` is a faithful reimplementation of
libxml2's `areBlanks()`, established by probing libxml2 2.14.6 through `lxml`
over a matrix of every candidate parent element, every sibling context and every
whitespace character, rather than by reading a header. The rule is written out
in `crates/browser-dom/API.md` deviation 3 and asserted by
`blank_text_survives_where_libxml2_keeps_it`.

That closed, at a stroke:

* 5 CSS golden rows (`#nbsp:empty`, `#nl:empty`, `#ws:empty`, `*:empty`,
  `div:empty` on the `edge` page);
* 3 XPath conformance divergences (`count(//text())` 23 → **47**,
  `count(//node())` 73 → **97**, and `normalize-space(//div[@id='d1'])`, which
  is now `"alpha bold beta"` and not `"alphaboldbeta"`);
* 7 Markdown pipeline divergences, including the one that mattered:
  `<p><b>b</b> <i>i</i> <strong>s</strong></p>` renders `**b** *i* **s**` on
  both sides now;
* and the blanket "filter whitespace-only text out of both sides before
  comparing" escape hatch in the two CSS differential tests, which is gone: 50
  of the 51 text nodes `*::text` selects on the selectors fixture now match lxml
  node for node.

**Two residues remain**, both lexer-level and both declared:

1. **A blank run interrupted by a character reference is kept by libxml2.**
   `areBlanks()` bails out with `if (CUR != '<') return(0)`, and libxml2 flushes
   its character-data chunk when it reaches the `&`. So `<ul> &#32;</ul>` keeps
   its whitespace there and loses it here — html5ever resolves the reference
   into the same text node, so we cannot see the boundary.
2. **A blank run containing a bare `CR` not followed by `LF` is kept**, for the
   same reason: the chunk is flushed at the `CR`. html5ever normalises `CR` and
   `CRLF` to `LF` in the input stream, so no bare `CR` ever reaches our tree.

Both are unreachable from well-formed HTML and neither is worth reproducing a
chunking artefact for.

### The tree-shape difference this uncovered

With blank text no longer filtered out of the CSS comparison, one *genuine*
tree-shape difference became visible and is now declared in
`EXPECTED_DIVERGENCES` (20 rows) and in `css_selection.rs`: html5ever's
*after body* insertion mode reprocesses the whitespace between `</body>` and
`</html>` with the in-body rules, which append it to the character run already
inside `<body>`. libxml2 makes it the tail of `<body>`. Python therefore sees
two text nodes (`"\n"`, `"\n"`) where we see one (`"\n\n"`). It belongs to
T1/T3, not to T2.

## Unit 5 — `browser-selectors`

1. **CSS-Selectors-4 supersets that `cssselect` rejects are accepted.** Sanctioned by
   `ARCHITECTURE.md` §1.3: `.dot:first-of-type` and `:nth-of-type(1)` without an
   explicit type selector, `:has()` with a descendant or attribute argument,
   `:not()` with a selector list or a complex selector, and `:nth-child(An+B of S)`.
   None of these can change the meaning of a selector that already works in Python.
2. **The `[a="b" i]` / `[a="b" s]` case-sensitivity flags are rejected at parse
   time**, including inside `:has()`. Attribute *values* are compared
   case-**sensitively** even for HTML's case-insensitive attribute list, because
   `cssselect` emits a plain XPath string test: `[type="TEXT"]` must not match
   `type="text"`.
3. **A sibling combinator cannot escape the query node's subtree.** `lxml`
   compiles `node.css("* ~ *")` to `descendant-or-self::*/following-sibling::*`,
   so the query node's own following siblings — outside its subtree — are
   reachable there and not here. The opposite-direction bug (the query node
   matching a combinator through an anchor Python cannot use) *was* fixed.
4. **Error message text is not reproduced.** A selector Python rejects is
   rejected here too, but the wording differs and carries no token offsets.

## Unit 6 — `browser-xpath`

1. **`namespace-uri()` on an HTML element answers `""`.** html5ever puts HTML
   elements in the XHTML namespace; libxml2's HTML parser — the only parser
   The reference implementation ever uses — namespaces nothing. `DomModel` maps the XHTML namespace
   to the null namespace so `lxml`'s answer is reproduced. Foreign (SVG/MathML)
   content keeps its real URI.
2. **A top-level `position()` / `last()` returns `1`.** XPath 1.0 and libxml2
   itself agree; only `lxml`'s *Python binding* raises
   `XPathEvalError: Invalid context size`, which is not reproducible through
   `lxml` at all.
3. **A predicate on a computed non-node-set is always an error.** libxml2's
   optimiser silently drops a lone `[1]`, so `(1 + 2)[1]` is `3` there while
   `(1)[1]`, `('a')[1]`, `(1 + 2)[2]` and `1[1]` are rejected in the same breath.
   XPath 1.0 makes all of them errors and we reject all of them.
4. **Decimal literals are correctly rounded.** libxml2's hand-rolled
   decimal-to-double conversion underflows `5e-324` to zero; Rust's parser does
   not. libxml2 formats that same value exactly as we do once arithmetic
   produces it.

## Unit 7 — `browser-storage`

1. **The public-suffix list is `psl`'s snapshot, not `tld` 0.13.2's bundled
   copy.** 40 of 20,306 swept rules disagree — every one a rule the PSL has
   since retired (`biz.at`, `deta.app`, `cloudapps.digital`, …). A Python
   process pinned to `tld` 0.13.2 files those domains under a different
   namespace. Closing the gap means vendoring a snapshot that goes stale against
   the real web, so it is left open deliberately.
2. **`str.lower()` follows the current UCD, not CPython 3.14's Unicode 16.0.0.**
   28 of 1,112,064 code points differ; all 28 are unassigned in Unicode 16 and
   none is below `U+0100`.
3. **This crate can write rows Python cannot.** the reference implementation's own `save()` *raises*
   on an element with a comment or PI sibling — `element_to_dict` puts
   `child.tag` in `siblings`, and for an `lxml` comment that is a Cython function
   object `orjson` refuses to serialise. We store `"#comment"` happily.
4. **No `lru_cache` singleton semantics.** Python requires a storage backend to
   be `lru_cache`-decorated and evicts (and therefore closes) the previous
   instance when a second `(file, url)` pair appears. `ARCHITECTURE.md` §8 drops
   the ritual for an explicitly owned `Arc<dyn StorageSystem>`.
5. **A brand-new empty database differs from Python's by one field:** the
   4-byte `SQLITE_VERSION_NUMBER` stamp at header offset 96. Everything else is
   byte-identical.

## Unit 8 — `browser-markdown`

Beyond T1/T3/T5/T6/T7 above, all of which land here (T2 is fixed):

1. ~~**Attribute values are the author's.**~~ **Fixed in the unit-10
   verification wave.** `browser-dom`'s serialiser now percent-escapes
   URI-valued attributes exactly as libxml2 2.14.6 does (`href`, `src`,
   `action`, and `name` on `<a>`; leading XML whitespace dropped, then the space
   character, every C0 control and every non-ASCII byte written as `%XX` over
   UTF-8). See the unit-10 section below for the other three serialiser fixes
   that landed with it.
2. **An emptied `<li>` stays its own list item.** `lxml` writes `<li></li>` as a
   bare `<li>`, and `html.parser` then nests the *following* `<li>` inside it, so
   The reference implementation renders `<ul><li></li><li>x</li></ul>` as `* * x`. We emit `* x`.
3. **`<ol start>` is arbitrary-precision and accepts Unicode decimal digits**, as
   Python's `int()` does. A `start` that is numeric but has no decimal value
   (`²`, `Ⅰ`) makes Python raise a `ValueError`; we fall back to `1` rather than
   panicking. `colspan` is still ASCII-digits-only.
4. **`code_language_callback` and `expand_tabs` are not ported** — both are
   unreachable from the reference implementation.

## Unit 11 — `browser-http` (policy)

1. **An IPv6 proxy host keeps its brackets.** Python's `urlparse().hostname`
   strips them and the reference implementation re-emits the unusable `http://::1:9050`.
   Deliberately more correct than the reference.
2. **A whitespace-padded proxy string is accepted.** `urlparse` strips leading
   spaces but leaves trailing ones inside the port, so `.port` raises there; the
   `url` crate follows WHATWG and trims both ends.
3. **`follow_redirects` = `"all"` / `"obeycode"` / `"firstonly"` are honoured.**
   The reference implementation's own type annotation advertises them, but `curl_cffi` only
   special-cases `"safe"` and then calls `int(allow_redirects)`, which raises.
   We map them to CurlFollow 1/2/3 — the evident intent, not the runtime
   behaviour. Upstream bug.
4. **Retry logs render a dict proxy as its `server` only.** Python interpolates
   the whole dict repr, password included. Keeping credentials out of the log is
   deliberate.
5. **`None` is not a distinct state for `verify`, `max_redirects`,
   `follow_redirects`, `proxies` or a `(connect, read)` tuple timeout.** Python
   forwards the `None` verbatim; a typed field has no null state, so an explicit
   null inherits the session default. Likewise `Proxy::Config { server: "" }` is
   this port's stand-in for Python's *missing* `server` key, so a genuinely empty
   server string is rejected where Python accepts it.
6. **`ProxySpec` normalises the proxies table to a fixed key order** and rejects
   an unmodelled scheme; Python forwards the user's dict verbatim in insertion
   order. Equivalent for curl.

## Unit 13 — `browser-cdp` (config)

1. **~~CPython `_checknetloc`'s NFKC trap is not implemented here.~~ Closed by
   the integration wave.** Unit 13 reported this as its only divergence, on the
   grounds that it needs a normalisation dependency; unit 7 had already shown it
   does not, via an exhaustive 19-code-point table. The table now lives in both
   crates and the two golden rows are ordinary rejections. Listed here because
   the wave-2 report claims otherwise.
2. **`route::host_of` returns `""` for a URL CPython would reject**, where
   Python's route handler would propagate the `ValueError` and leave the request
   unrouted. Chromium never reports such a URL, and `""` matches no blocklist
   entry, so the request continues either way.
3. **Invalid states are unrepresentable rather than rejected at run time**:
   a negative `max_pages` (a `u32`), a bad `wait_selector` state (an enum), a
   proxy dict with no `server` (a non-optional field), and the
   `page_action`/`page_setup` "must be callable" `TypeError`s (a boxed async
   closure is callable by construction). The `__max_pages` private kwarg alias
   and `msgspec`'s silent acceptance of unknown kwargs are likewise not
   reproducible against a typed builder.
4. **Range validation runs in struct-declaration order**; `msgspec`'s order
   follows input-dict iteration order, which is not reproducible. Only one error
   surfaces either way.
5. **`PagePool::wait_for_ready_page` polls before its first sleep**, where
   Python sleeps 50 ms first.

## Integration wave

1. **`browser-http::config` re-exports `FollowRedirects`, `Override`, `Proxy`
   and `RedirectMode` from `browser-core`.** They appear in public fields of
   `FetcherConfig`, `RequestOverrides` and `MergedRequest`, and without the
   re-export a downstream crate could read a field whose type it could not name.
2. **`browser-selectors` and `browser-xpath` gained crate-root re-exports.**
   Units 5 and 6 were not permitted to edit `lib.rs`, so both `API.md` files
   documented module paths only. The module paths remain valid; the root
   spellings now exist too, matching `browser-core`, `browser-dom` and
   `browser-storage`.

# Wave 3 — unit 10 (`browser-parser`)

## Fixed here, not divergences

The unit-10 API corpus (7008 rows, `tools/corpus/u10/gen_api_corpus.py`) found
three real bugs, all now corrected:

1. **The `<!DOCTYPE>` leaked into navigation.** libxml2 keeps the doctype out of
   the element tree entirely — it lives in `docinfo` — so `<html>.getprevious()`
   is `None` in Python. html5ever models it as a real child of the document node
   and `Node::previous` was returning it. `next`, `previous` and `children` now
   skip it, alongside comments.
2. **`browser-dom`'s attribute serialisation followed html5ever, not
   libxml2.** Established against libxml2 2.14.6 and fixed in
   `crates/browser-dom/src/serialize.rs`:
   * a value containing `"` and no `'` is written inside **single quotes** with
     the double quote bare; only a value containing both falls back to `&quot;`;
   * `<` and `>` are escaped **inside attribute values**;
   * `href` / `src` / `action`, and `name` on `<a>`, are percent-escaped;
   * U+00A0 is written **verbatim** in text and attributes rather than as
     `&nbsp;` — libxml2 only reaches for a character entity when the output
     encoding cannot hold the code point, and the reference implementation always serialises UTF-8.

   `tools/corpus/xconf/gen.py` reimplements this serialiser to compare XPath
   node-sets, so it was updated in step and `xpath_conformance.jsonl`
   regenerated; the XPath corpus stays at **3077/3077**.
3. **`browser-xpath` matched attributes by local name alone.** `//a/@href`
   reached an `xlink:href`, which lxml only answers to as `@xlink:href` with a
   namespace map. XPath 1.0 §2.3 gives an unprefixed name test the null
   namespace on the attribute axis; both `matches_test` and the single-walk fast
   path in `eval.rs` now require it. Element name tests still match by local
   name, because libxml2's HTML parser puts every element — `svg` and `math`
   included — in no namespace, and `//svg//rect` has to keep working.

## Still divergent

Nothing new. All 333 unreproduced rows of the API corpus, and all 130 of the
algorithm corpus, are the tree-shape and libxml2-convention differences already
listed above (T1–T7, the synthesised `<head>`, the implied `<tbody>`, the
adoption agency, SVG/MathML naming, end-of-document whitespace), plus two
consequences of them that are worth naming:

1. **A valueless boolean attribute.** libxml2 gives `checked` the value
   `"checked"`; html5ever gives it `""`, and html5ever cannot tell `checked`
   from `checked=""` at all. This is not only a serialisation difference: it
   changes every `SequenceMatcher` ratio computed over that attribute, so
   `find_similar` can answer differently on `forms.html`. 70 rows.
2. **libxml2 will not compile an XPath string holding a raw control
   character** (`ValueError: All strings must be XML compatible`). Python's
   `find_all(..., {"title": "form\ffeed"})` therefore *raises*; the port has no
   XML-document constraint and simply matches the element. 34 rows. Emulating
   the rejection would be libxml2 bug-compatibility, which `ARCHITECTURE.md` §8
   rules out.

The unit-10 divergence tables live next to their tests, one row at a time with a
written reason, and both tests fail if a declared row ever starts passing:
`crates/browser-parser/src/document.rs` (`EXPECTED_DIVERGENCES`) and
`crates/browser-parser/tests/support/divergences.rs`.

---

# Wave 3 — unit 15 (`browser-spider`, data plane)

Verified against the reference implementation / w3lib 2.4.1 / CPython 3.14 by the corpora under
`tools/corpus/u15/` (generators) and `crates/browser-spider/testdata/u15_*.jsonl`
(goldens). Every table below is mirrored by a `DIVERGENCES` constant next to its test,
and every one of those tests **fails if a declared row ever starts matching**.

## Fixed here, not divergences

Three real defects the unit-15 verification pass found and repaired:

1. **Two distinct priorities collapsed into one queue key.** `Scheduler::enqueue`
   negated the priority with `i32::saturating_neg()`, so `priority = i32::MIN` and
   `priority = i32::MIN + 1` both mapped to `i32::MAX` and were then ordered by
   insertion counter — the wrong way round. Python's ints are unbounded and order them
   apart. `QueueKey::neg_priority` is now `i64`. (`u15_scheduler.jsonl`,
   `extreme-priorities`.)
2. **Case-colliding kwargs resolved in the wrong order.** `include_kwargs`
   lower-cases every kwargs key, so `timeout=` and `TIMEOUT=` collapse into one entry
   and Python — whose `**kwargs` is an insertion-ordered `dict` — keeps the *last*
   spelling. `SessionKwargs::extra` was a `BTreeMap`, which kept whichever key sorted
   last instead. It is now an `IndexMap`. (`u15_request_fingerprint.jsonl`,
   `kwargs-case-collision-*`; 77 of the 1500 fuzz rows also hit it.)
3. **An empty `BytesIO` body was treated as absent.** `update_fingerprint` gates the
   `json=` fallback on `if post_data:`, and a `BytesIO` defines neither `__bool__` nor
   `__len__` — so an empty one is *truthy* and suppresses `json=`, while an empty
   `bytes` is falsy and does not. The port mapped both onto `RequestBody::Bytes`.
   `RequestBody::Io` now models `BytesIO`, and `RequestBody::is_falsy` is the
   truthiness test (`is_empty` remains the byte-length test).
   (`u15_request_fingerprint.jsonl`, `empty-bytesio-suppresses-json`.)

## Still divergent

1. **`canonicalize_url` uses UTS-46, CPython uses IDNA 2003.** For hosts containing a
   deviation character (ß, final sigma) `str.encode("idna")` maps them during nameprep
   and the `idna` crate keeps them, so the punycode labels differ. Browsers and the
   WHATWG URL Standard agree with UTS-46, so matching Python would mean fingerprinting a
   host the fetcher never resolves. 2 declared rows in
   `crates/browser-spider/tests/canonicalize_golden.rs`; the second corpus
   (`u15_canonicalize.jsonl`, 147 URLs × 4 flag combinations) reaches none of them and
   is **584/584 exact**, with all 584 `urlparse`/`netloc`/`Request::domain` triples
   matching CPython as well.
2. **`canonicalize_url` cannot fail.** CPython's `urlsplit` raises `ValueError` on an
   unbalanced-bracket netloc (`http://[not-an-ip]/`) and on an NFKC-hostile netloc. A
   `Request` that cannot be hashed cannot be scheduled, so the port canonicalises those
   best-effort. The rows stay in the corpus and the port's answer is pinned by
   `RAISING_ROWS` in `tests/u15_canonicalize_golden.rs`.
3. **`CrawlStats::to_dict` renders its five nested maps sorted by key.** Python renders
   them in first-seen `dict` order. `CrawlStats` is `Arc`-shared with `&self` mutators,
   so those maps are `DashMap`s and have no insertion order to preserve; sorting is
   deterministic where Python's order depends on how the crawl's tasks interleaved. The
   **top-level** key order — including the `log_levels_counter` → `log_count` rename —
   is exact, and `log_count` itself keeps Python's fixed
   `debug/info/warning/error/critical` order. `tests/u15_crawl_stats_golden.rs` asserts
   the divergence is still real, so the note cannot rot into a silent match.
4. **Checkpoints are versioned JSON, not pickle.** `checkpoint.json` (version 1) instead
   of `checkpoint.pkl`. The directory layout, the `checkpoint.tmp` + atomic-rename
   dance, the interval validation, and the save-re-raises / load-and-cleanup-swallow
   asymmetry are all reproduced and pinned behaviourally by `u15_checkpoint.jsonl`
   (21 of 23 scripts identical). A `checkpoint.pkl` written by Python is not resumable.
5. **A NaN or infinite checkpoint interval is rejected.** Python only rejects a
   *negative* interval — `isinstance(nan, float)` is true and `nan < 0` is false — so it
   accepts both and the crawl later sleeps for NaN seconds.
   `Duration::from_secs_f64` *panics* on either, which would turn a caller's typo into
   an abort inside a spawned task, so `CheckpointManager::new` returns
   `SpiderError::CheckpointIntervalNotANumber`. 2 declared rows
   (`interval-nan`, `interval-infinity`).
6. **`SessionManager::close` closes every session.** Python's loop aborts at the first
   failing `__aexit__` and leaks the rest; the port closes them all and returns the
   first error, because leaking a browser process is worse than losing an error message.
   `start()` keeps Python's abort-on-first-error behaviour, which
   `u15_session.jsonl`'s `start-aborts-on-the-first-failing-session` pins.
7. **`Scheduler::dequeue` returns `Option` and `complete` takes a `Ticket`.** Python
   awaits an empty queue and keys its in-flight table by `id(request)`. The completion
   condition belongs to the engine, and the ticket removes the
   same-instance-enqueued-twice fragility. All 22 scripted replays agree, including
   `restore()` appending a *second* copy of every still-pending request.
8. **`Request` implements neither `PartialEq` nor `Hash`.** Python's `__eq__` raises when
   either side has no fingerprint yet, which a `PartialEq` impl may not do; equality is
   the explicit `Request::same_fingerprint() -> Option<bool>`. `Clone` keeps the memoised
   fingerprint, `copy()` clears it — Python's `copy()`.
9. **`include_kwargs` fingerprints are portable only for JSON-native values.** Python
   falls back to `repr(value)` for a kwargs value `orjson` cannot serialise. There is no
   Rust analogue and nothing to declare as a row: `SessionKwargs::extra` is a
   `serde_json::Value`, which is always serialisable. All 1605 corpus digests match,
   `include_kwargs` rows included.
10. **`method=` is an enum.** Python stores whatever string the caller passed, so
    `method="post"` fingerprints differently from `method="POST"`. The reference implementation's own
    `SUPPORTED_HTTP_METHODS` is `Literal["GET", "POST", "PUT", "DELETE"]`, so the enum
    loses only inputs the reference implementation already declares unsupported.
11. **A named `SessionKwargs` field and an `extra` key that collide only after case
    folding.** Python has one flat `**kwargs` dict, so `headers=` and `HEADERS=` resolve by
    call order. The port's `stable_pairs` emits the seven named fields first and then the
    `extra` entries, so the extra always wins regardless of order. This is inherent to
    replacing an untyped dict with a struct — there is no interleaving left to preserve —
    and it needs a caller to set `SessionKwargs::headers` *and*
    `SessionKwargs::extra["HEADERS"]` on the same request, so no corpus row encodes it.
    Extra-vs-extra collisions, which a caller can plausibly write, are exact (divergence 2
    above).

---

# Wave 3 integration — the `Response` seam (units 12 / 14 preflight)

The wave-3 green pass drove a `Response`-shaped value — raw bytes, headers, a
`SelectorConfig`, a lazily parsed `Document` — through the whole of unit 10's public API,
in `crates/browser-rs/tests/integration.rs::response_seam`. One thing did not compose, and
fixing it introduced two new declared divergences.

## Fixed here, not a divergence

**`ParseOptions::encoding` was a label, not a decoder.** `browser-dom::parse_bytes_with`
recorded the encoding on the `Document` and then decoded every byte body as UTF-8 with
replacement, so the `charset=` a fetcher pulls out of a `Content-Type` header would have
reached `Response.encoding` and changed nothing at all. Python hands the label to
`HTMLParser(encoding=…)`, which really does pick the codec:
`Selector(content=b"<p>caf\xe9</p>", encoding="iso-8859-1").css("p::text").get()` is
`'café'` and the same bytes under `encoding="utf-8"` are `'caf�'`.

`prepare_bytes_with(bytes, encoding)` now NUL-strips and then decodes with the named codec
before html5ever sees anything. An explicit label **overrides** any `<meta charset>`, which
is also what libxml2 does — it stops sniffing once it has been told. Pinned by
`browser-dom`'s `parse::tests::the_encoding_label_decides_how_a_byte_body_is_read` and by
`response_seam::the_content_type_charset_reaches_the_decoder`, every expected value in both
read off the reference interpreter.

## Still divergent

1. **Encoding labels resolve through WHATWG, not `iconv`.** `encoding_rs` implements the
   WHATWG Encoding Standard, in which `iso-8859-1`, `latin1` and `ascii` are all aliases of
   **windows-1252**. libxml2 goes through `iconv`, where `iso-8859-1` is true ISO-8859-1 and
   `ascii` is 7-bit. The two disagree on exactly the 32 bytes `0x80..=0x9F`: measured
   against the reference, `b"<p>\x93a\x94\xe9</p>"` is `'\x93a\x94é'` under Python's
   `iso-8859-1` and `'“a”é'` here, while Python's `ascii` fails the parse outright and
   returns nothing. Every other label the two share — `utf-8`, `windows-1252`, `shift_jis`,
   `koi8-r`, `euc-*`, `gb*` — agrees. Reproducing `iconv`'s table would mean shipping a
   second, non-WHATWG decoder for a case that only arises when a server both mislabels its
   charset and uses C1 bytes; `ARCHITECTURE.md` §11.6's "match the browser" rule points the
   other way anyway, since browsers are WHATWG.
2. **An encoding problem never destroys the document.** An unresolvable label raises
   `LookupError` in Python; a body `iconv` rejects (`euc-jp` over latin-1 bytes, `ascii`
   over high bytes) makes lxml return an *empty tree*, so `css()` answers `None` for the
   whole page. Here an unknown label logs a `tracing::warn!` and falls back to UTF-8, and
   every decoder runs in replacement mode, so a mislabelled body degrades to U+FFFD instead
   of vanishing. A fetcher must not lose a 200 OK because a header was wrong.
3. **A UTF-8 BOM is stripped only under a UTF-8 label.** WHATWG's `decode()` would let a
   BOM override the caller's label; libxml2 does not re-sniff. `decode_without_bom_handling`
   plus an explicit UTF-8-only BOM strip reproduces both halves of what the reference does:
   `\xEF\xBB\xBF…` under `utf-8` loses the BOM, and under `windows-1252` really does begin
   with `ï»¿`.

## Local abstractions unit 12 still has to wire up

> **Status after unit 12 landed:** the `SelectorConfig` → `DocumentConfig` mapping is
> `browser_http::response::document_config`, and the `__CHARSET_RE__` seam is
> `charset_from_content_type` (the browser pattern) plus `charset_from_content_type_http`
> (`curl_cffi`'s, which is what `Response.encoding` uses — see unit 12's list below).
> `impl From<Response> for FetchedResponse` is still open and belongs to unit 17, because
> `browser-spider` depends on `browser-http` and not the reverse.


Not divergences — open seams, recorded so the next session does not rediscover them:

* **`FetchedResponse` is unit 15's stand-in for `Response`.** `browser-spider` does not
  depend on `browser-parser`, so `CrawlSession::fetch` returns the flat struct in
  `spider/session.rs`. Unit 12 owes an `impl From<Response> for FetchedResponse` and, for
  the dev-mode cache replay, the reverse construction. `a_fetched_response_round_trips_to_a_parsed_document`
  writes both by hand to prove the field sets line up; the `encoding` field is what lets the
  reverse direction rebuild a correctly decoded `Document`.
* **`SelectorConfig` → `DocumentConfig` has no implementation yet** — it cannot live in
  either crate without a new dependency edge, so it belongs in unit 12's `response.rs`.
  `every_fetcher_parser_keyword_lands_somewhere_in_document_config` pins that all seven
  `PARSER_KEYWORDS` have a home and that `storage` + `storage_args` collapse into the one
  `Arc<dyn StorageSystem>` slot.
* **`__CHARSET_RE__` has no home yet.** `charset=["']?([\w-]+)` is Python's, applied only to
  browser responses because `curl_cffi` resolves the charset for the HTTP engines. Unit 12
  needs the same fallback wherever a backend does not resolve one; the seam test carries a
  copy.
* **Unit 15 left no other abstraction for unit 12.** `SessionKwargs` already speaks
  `browser_core::types::{HttpMethod, Proxy}`, the same types `browser-http::config`
  re-exports, so the session pool implements `CrawlSession` without a translation layer.
  The one contract to honour is the pinned invariant in `spider/session.rs`:
  `fetch` takes `&Request` and must not mutate `request.session_kwargs`.

---

# Wave 4 — units 12, 14, 16 and 17

The transport wave: the HTTP driver, the CDP driver, and the two halves of the
crawling engine. These four sections were nested under the wave-3 integration
heading by accident of chronology; they are their own wave.

## Unit 12 — `browser-http` (the driver, `Response`, sessions, the `"safe"` guard)

Everything below was **measured**, not reasoned about: `tools/corpus/u12` serves a
declarative ~90-endpoint fixture over two loopback ports, drives 188
(endpoint × fetcher-argument) rows through the real `the reference implementation` /
`curl_cffi==0.16.1`, and `crates/browser-http/tests/u12_http_golden.rs` replays the same
rows against the same fixture in Rust and compares 2271 fields. **2271/2271 agree**, with
the 15 divergences below declared in the test — each one asserted to *still* differ, so the
test fails if the port ever starts matching and nobody notices.

### What `follow_redirects="safe"` actually does — and where this port differs

`CURLFOLLOW_SAFE` (=4) is a curl-impersonate extension, documented only as "reject
redirects to internal/private IP addresses". Probing the reference pins it down much
further, and two of the findings are load-bearing:

1. **The check runs only when the hop needs a new connection.** `Fetcher.get` with the
   default `follow_redirects="safe"` follows `http://127.0.0.1:P/r` → `/end` happily, and
   refuses the identical hop the moment the host *or the port* changes —
   `127.0.0.1:P` → `127.0.0.1:Q` and `127.0.0.1:P` → `localhost:P` are both refused with
   `curl: (7) Redirect to internal IP 127.0.0.1 rejected (SSRF protection)`. The guard here
   reproduces that as a **same-origin exemption** (scheme + host + port), which is the
   closest a port that does not own the connection pool can get.
2. **Consequence, and the one declared difference:** curl checks a same-origin hop when the
   reply carried `Connection: close`, because it must dial again; this port does not.
   Corpus row `redirect-close-safe`. The security property that matters is unchanged — a
   redirect still cannot *pivot* to an address the caller had not already reached.

Two smaller guard differences, both declared:

3. **Non-HTTP(S) redirect targets are refused in every mode.** curl_cffi's build enables
   FTP in redirects, so `follow_redirects=True` really does dial `ftp://127.0.0.1:9`; this
   port refuses it, matching curl's own `CURLOPT_REDIR_PROTOCOLS` default. Rows
   `redirect-toftp-true`, `redirect-toftp-safe` (where curl also orders the two checks the
   other way round and reports `ssrf` rather than `protocol`). `file:` is refused by both.
4. **Four extra non-routable ranges** beyond `ARCHITECTURE.md` §2.3's ten (IPv4 multicast
   `224/4`, the limited broadcast address, `198.18/15`, and the unspecified `::`). No
   corpus row separates them; they are strictly a superset.

### `Response` fidelity fixes this unit made (not divergences — bugs found and closed)

Recorded because each was wrong before the differential run and each is now pinned:

* **`Response.cookies` is the final reply's `Set-Cookie` lines, not the jar.** `curl_cffi`
  builds it with `http.cookies.SimpleCookie` from the last response alone: no expiry check
  (a `Max-Age=0` cookie *appears*), no domain/path/`Secure` matching (a `Secure` cookie
  delivered over plain HTTP appears), and nothing from a redirect hop. The jar is a separate
  thing and keeps its own, correct RFC 6265 rules.
* **Repeated headers fold on `", "` — cookies included.** `dict(response.headers)` joins the
  multidict that way without special-casing `Set-Cookie`, `Expires` comma and all, so
  `WireResponse` reports the unfolded lines separately in `set_cookies` and both the jar and
  `Response.cookies` read *that*.
* **`Response.encoding` uses `curl_cffi`'s `charset=([\w-]+)`, which does not accept a
  quoted value.** `Content-Type: text/html; charset="windows-1252"` resolves to **`utf-8`**
  over HTTP. The reference implementation's own `__CHARSET_RE__` (`charset=["\']?([\w-]+)`) *does* accept
  quotes, but it is only ever applied to browser responses; both are now separate functions.
* **A 301/302/303 always drops the body, even when the method survives.** `PUT` with `data=`
  through a 303 arrives as a bodiless `PUT` — the method table said the body survived.
* **The `Content-Type` the dropped body implied stays on the wire.** curl clears the payload
  but not the headers.
* **A raw `data=` payload is labelled `application/octet-stream`,** which is what `curl_cffi`
  sends; the port sent no `Content-Type` at all.
* **Cookies are not stripped on a cross-origin hop; `Authorization` is.** Cookies are scoped
  by the jar's domain and path rules rather than by origin, so curl sends a `127.0.0.1`
  cookie to `127.0.0.1:9999` (cookies ignore ports) and never sends it to another host. A
  per-request `cookies=` is now seeded into a request-scoped overlay jar with `Path=/` at the
  request host, which is exactly what `Cookies.get_cookies_for_curl` does. `WireRequest.auth`
  is cleared alongside the `Authorization` header, which it was not before.
* **obs-fold header continuations are accepted.** libcurl still reads them; hyper rejects
  them by default and the whole exchange failed. `wreq`'s
  `allow_obsolete_multiline_headers_in_responses` is now on.

### Transport-level divergences that cannot be closed under the `wreq` 0.15.3 pin

5. **A non-canonical reason phrase is lost.** hyper keeps it in
   `hyper::ext::ReasonPhrase`, a type this crate cannot name without a direct hyper
   dependency (wreq does not re-export it). `299 Totally Fine` and an empty reason phrase
   both read as `StatusText`'s `Unknown Status Code`. Every status with a canonical phrase
   matches exactly. Rows `status-299`, `status-222`.
6. **`Content-Encoding` and `Content-Length` disappear once the body is decoded.** wreq's
   transparent decoder removes both, on the grounds that neither still describes what
   `body()` holds; curl leaves them in place. The decoded bytes match byte for byte. Rows
   `encoding-gzip`, `encoding-deflate`, `encoding-br`, `encoding-gzip-chunked`.
7. **A raw (headerless) deflate stream is rejected.** libcurl sniffs zlib-vs-raw; wreq
   decodes `deflate` strictly as zlib. RFC 9110 says `deflate` means zlib, so the *fixture*
   is the non-conforming party — but Python accepts it and this port does not. Row
   `encoding-deflate-raw`.
8. **The `Cookie:` header carries the same pairs in a different order.** libcurl emits its
   cookie list in reverse insertion order (a list-prepend artifact); the jar here emits
   insertion order. RFC 6265 §5.4 makes the order non-semantic and mimicking the artifact is
   not worth pinning. Rows `echo-cookies`, `session-cookie-multi`.
9. **An unknown charset label does not raise.** `Content-Type: charset=definitely-not-a-charset`
   makes Python raise `LookupError` out of `Selector.__init__`; this port warns and decodes as
   UTF-8. Already listed above as a `browser-dom` decoding divergence; row
   `charset-unknown-label` is where it becomes observable on a `Response`.
10. **`cert=`, `http3=`, `interface=` and SOCKS proxies.** wreq 0.15.3 has no `ClientBuilder`
    entry point for client certificates (refused with `TransportError::Build` rather than
    silently ignored), no QUIC (a `http3=true` warns and sends HTTP/2, which is what Python
    does for `http3` + `impersonate` anyway), `interface=` is `#[cfg]`-gated upstream so it is
    not offered at all, and SOCKS needs a wreq feature the workspace pin does not enable.

### Non-transport notes

11. **`Response.url` is the URL actually fetched.** Python passes
    `url=(adaptive_domain or url)` into `Selector.__init__`, which makes `response.url` report
    the *domain* when `adaptive_domain` is set — and quietly breaks `urljoin`. Here
    `adaptive_domain` replaces the URL only as the adaptive **storage key**
    (`DocumentConfig::effective_url`); `Response::selector_url()` returns what Python's
    `Response.url` would have said.
12. **`Response::text()` does not exist.** Python's `Response.text` is inherited from
    `Selector` and returns the *root element's* text, usually empty — not the body. The
    decoded body is `Response::body_text()`, so one name cannot mean two things.
13. **Parsing is lazy** (already declared): the first `css`/`xpath`/`find_all` parses, behind
    a `OnceLock`, so `Response` stays `Send + Sync`. lxml runs in `recover=True`, so the eager
    parse never raised and nothing observable changes.
14. **`request_headers` reports the set, not the sequence.** The on-the-wire order is the
    emulation profile's `headers_order`, which is the fingerprint-correct choice; Python
    reports dict insertion order.
15. **The one-shot `Fetcher` shares a pooled connection cache** instead of building and
    closing a client per call (`ARCHITECTURE.md` §6.5 sanctions this). The observable half is
    preserved: every one-shot request gets a brand-new, empty `CookieJar`, pinned by corpus
    row `oneshot-cookie-isolation`.

---

## Unit 16 — `browser-spider` support services (`throttle.rs`, `robots.rs`, `cache.rs`)

Verified against `the reference implementation` + `protego` + CPython 3.14 over 6,322 corpus rows in nine
files, 172,447 differential assertions, 172,311 of them exact (**99.92 %**). Everything below is
one of the 136 that are not, and every one is pinned by a `DIVERGENCES` table in
`crates/browser-spider/tests/u16_*_golden.rs` that **fails the test if it ever starts
reproducing**.

1. **`Request-rate` with an integer no `i64` can hold.** `Request-rate: 1/99999999999999999999`
   gives Python (whose ints are unbounded) a per-request delay of roughly three thousand years;
   `RequestRate::seconds` is an `i64`, so the port ignores the directive instead. 9 corpus keys,
   one cause. Pinned in `u16_robots_golden.rs::DIVERGENCES`.
2. **Nine text codecs resolve but decode as UTF-8.** `cp1251`, `koi8_r`, `iso8859_15`,
   `mac_roman`, `big5`, `gb2312`, `shift_jis`, `utf_7` and `unicode_escape` are accepted as
   labels — so a body carrying them is *not* treated as the `LookupError` that would make the
   domain allow everything — but the bytes are read as UTF-8. Every one is an ASCII superset and
   every `robots.txt` directive is ASCII, so the resulting **ruleset is identical**; only
   non-ASCII bytes inside comments and paths differ. 109 corpus rows. Implementing them properly
   means carrying ~90 single- and double-byte tables into this crate for no behavioural gain.
   (UTF-16 and UTF-32 *are* implemented, BOMs included — those change the ruleset.)
3. **An empty `FetchedResponse::encoding` is read as "unset", not as the empty codec label.**
   Python's `Response.encoding` defaults to `"utf-8"` and can never be empty; `FetchedResponse`
   derives `Default` and gets `""`. Treating that as a label would make `bytes.decode("")` raise
   and every such `robots.txt` silently allow everything — the dangerous direction — so
   `RobotsTxtManager::parser_for` decodes UTF-8 instead. `decode_robots_body(body, "")` called
   directly still answers `None`, exactly like CPython. 5 corpus keys. **Half fixed in unit 17:**
   `FetchedResponse::encoding` now defaults to `"utf-8"`, as Python's `Response.encoding` does, so
   an empty label is no longer reachable from an ordinary construction. The 5 corpus rows still
   diverge because they set `""` deliberately, which is why the row stays here.
4. **A hand-corrupted cache file whose values Python leaves untyped is a miss, not a replay.**
   `status: null` / `"200"` / `200.5` / `-1` / `1e20`, `url: null`, `reason: null`,
   `encoding: null`, `headers: []`, `method: 7` and friends all deserialise into Python's
   `Response(**data)` and produce *something*; a typed `FetchedResponse` cannot represent them,
   so the entry is treated as a cache miss and the request is re-fetched. 13 rows. None of these
   files is reachable from `ResponseCacheManager::put` — they only exist if something else wrote
   the directory.
5. **`AutoThrottle::delays()` is sorted by domain, not insertion-ordered.** Python hands the
   engine `dict(autothrottle.delays)`. The port's store is a `DashMap`, which has no insertion
   order at all, so the snapshot is a `BTreeMap` — deterministic rather than arrival-order
   dependent. Only observable in `stats.autothrottle_delays`.
6. **A NaN `autothrottle_target_concurrency` is rejected.** Python's guard is
   `target_concurrency <= 0`, and `nan <= 0` is `False`, so a NaN slips through and then poisons
   every delay for the rest of the crawl by way of `latency / nan`. `AutoThrottleConfig::validate`
   rejects NaN with the same `TargetConcurrency` error. Pinned by `u16_throttle_golden.rs`.
7. **`RobotsTxtManager` has no in-flight de-duplication** — two concurrent misses for one netloc
   both fetch and the last writer wins. Faithful to Python, which has no lock either, and the
   fetch-count corpus pins it. A single-flight would be a strict improvement and no test forbids
   one.

### Fixed here, not divergences

The differential pass closed eleven real bugs, the largest of which were: CPython's
`str.isspace()` covering `\x1c`–`\x1f` (which `char::is_whitespace` does not, so
`Retry-After: "\x1c5"` answered `None`); every CPython string→number conversion running
`_PyUnicode_TransformDecimalAndSpaceToASCII` first, so `float("٣.٥") == 3.5` and
`Crawl-delay: ٥` is `5.0` — this had previously been *declared* a permanent divergence and is
now implemented; the HTTP-date branch being replaced by a direct port of CPython's
`email._parseaddr._parsedate_tz`, which fixed eight separate classes of wrongly rejected header
(a mismatched day-of-week name, trailing junk, a parenthesised comment, RFC 850 with a four-digit
year, the `dd-mmm-yy` form, `asctime` with a zone, the `_timezones` names, and the POSIX 69/68
two-digit-year window); `decode_body` falling back to UTF-8 for an unknown label, which made the
port *apply* rules Python ignores; `prefetch_urls` dropping start URLs with no netloc and
defaulting a missing scheme to `https`, neither of which `_prefetch_robots_txt` does; and
`b64decode(validate=False)` silently discarding non-alphabet characters, so `" aGk= "` is a cache
**hit** in Python and had been a miss here.

---

## Wave 4 integration — the seams for units 14 and 17

Nothing below is a difference from Python; they are the port's own cross-crate decisions, made
while proving that units 14 and 17 can be written from what exists. All three are exercised by
`crates/browser-rs/tests/integration.rs`.

1. **There is exactly one `Response` type.** Unit 14's CDP driver produces an already-rendered
   `String`, a status and a header map — never bytes off a socket — and it still builds
   `browser_http::response::Response` through `Response::builder(...)`, with
   `charset_from_content_type` (the quote-tolerant *browser* regex, the reference implementation's
   `__CHARSET_RE__`) supplying the encoding and `RedirectHop`s assembled from
   `redirectResponse` events. `browser_response_seam` drives a browser-built and an
   HTTP-driven response through the same monomorphic reader, the same adaptive
   save-then-relocate and the same `DocumentHook`. A second `Response` type would be a bug.
2. **`Response` ⇄ `FetchedResponse` lives in `browser-spider`.** `browser-spider` depends on
   `browser-http` and not the reverse, so `impl From<&Response> for FetchedResponse` and
   `FetchedResponse::response_builder()` are in `crates/browser-spider/src/session.rs`. The
   round trip drops two fields on purpose: `history`, because `RedirectHop` carries each hop's
   status/reason/headers and a `FetchedResponse` only kept the URLs (Python's cache replay also
   rebuilds with `history=[]`), and `meta`, because it is the crawl's merged metadata rather than
   the transport's and `attach_request` rebuilds it.
3. **`browser_http::backend::Method` and `browser_core::types::HttpMethod` stay separate,
   with a `From` in both directions.** They name the same closed set of four verbs, but
   `browser-spider`'s `Request` is serialised into a checkpoint file and a transport type has
   no business in that format. The conversions in `crates/browser-http/src/backend/mod.rs` are
   the single seam between them.

---

## Unit 17 — `browser-spider` engine (`engine.rs`, `spider.rs`)

Verified against `the reference implementation`'s own `reference/spiders/engine.py` by running **36 real
crawls** on both implementations over the same two-host loopback fixture and comparing 1,255
fields: the URLs the *servers* saw (in arrival order where the scenario is serial), the items and
their depths, `CrawlStats.to_dict()` key by key, the lifecycle-hook call records, the checkpoint
contents and the pause/resume round trips. **1255/1255 reproduce.** The corpus, the fixture and
the scenario table are in `tools/corpus/u17/`; the comparison is
`crates/browser-spider/tests/u17_engine_golden.rs`.

Everything below is either a field the corpus cannot compare verbatim — each one still asserted,
and the last one asserted to *keep* differing — or a structural difference the port could not
avoid.

### Fields the corpus relaxes, and how

1. **`elapsed_seconds` / `requests_per_second` are wall-clock** readings of two different runs on
   two different runtimes. Both are still asserted finite and non-negative, and for the three
   scenarios where a *mandated* sleep dominates the clock (`robots.txt`'s `Crawl-delay: 0.05`, in
   `s03` / `s06` / `s15`) a **lower bound** is asserted on both sides — `MINIMUM_ELAPSED`. That
   bound is what catches an engine that resolves the delay and then never sleeps; nothing else in
   the corpus would.
2. **`autothrottle_delays` values are latency-derived**, so only the *domains the controller
   learned* are compared. (The port's snapshot is also sorted rather than insertion-ordered —
   unit 16, item 5.)
3. **`log_count` counts `tracing` events, not `logging` records.** The port emits the message
   text Python's tests pin but not the same *set* of lines, and `tracing` has no level above
   `ERROR`, so `critical` is always `0`. Compared on Python's exact five-key shape, and asserted
   to still differ in value.
4. **An item's `depth` under `concurrent_requests > 1` is a discovery-order race.** `depth` is
   `parent.depth + 1` of whichever parent enqueued it *first*, and `/cat/1` is linked both from
   `/` (depth 1) and from `/cat/0` (depth 2). The reference implementation is stable here only
   because CPython's `lxml` parse is much quicker than the fetch it races; the port lost that race
   once in five runs of `s06`, whose two seeds start 50 ms apart because one host has a
   `Crawl-delay` and the other does not. The five concurrent scenarios therefore compare items
   with `depth` elided and bound each depth by `max_depth`; the 31 serial ones compare it
   verbatim. `DEPTH_IS_A_DISCOVERY_ORDER_RACE`.
5. **Three rows are timing-dependent in their *content*** — a force stop (`s18`) and the two
   halves of a concurrent pause (`s28`) — because how much got done before the axe fell is a race
   in both implementations. They are not skipped: `paused`, `start_requests_calls`, the
   lifecycle hooks and the checkpoint's existence are still compared, and `RESUME_PAIRS` requires
   that the paused run plus its resume fetched exactly what the uninterrupted crawl fetched.

### Structural differences

6. **`Ctrl-C` is watched with one long-lived `tokio::signal::ctrl_c()` future** pinned across loop
   iterations. Re-creating it per wait would deregister the receiver in between, and tokio's
   installed SIGINT handler silently drops a signal nobody is listening for — the press would be
   lost *and* the process would no longer die from it. The consequence with no Python analogue:
   tokio never uninstalls the OS handler, so after a crawl that watched for `Ctrl-C` the process
   keeps ignoring the default SIGINT action. Python restores the previous handler in `start()`'s
   `finally`. `RunOptions::handle_ctrl_c = false` — what streaming uses, matching Python's
   handler-free `stream()` — never installs it.
7. **Dropping a stream aborts the crawl** (Python's `_stream` nests the crawl in a task group
   *inside* the generator, so abandoning the generator cancels it). Unlike anyio's cancel, a tokio
   abort drops the future rather than unwinding it, so `on_close` and the session shutdown are
   skipped when the stream is abandoned. Drain the stream when that matters.
   `u17_engine_invariants.rs::dropping_a_stream_stops_the_crawl`.
8. **A closed item stream is logged, not raised.** Python's `send` on a closed memory stream
   raises `BrokenResourceError`, which its `except Exception` turns into an `on_error` call that
   abandons the rest of the callback; the port logs at DEBUG and keeps consuming. Unreachable in
   practice, because dropping the stream aborts the crawl first (7).
9. **`_restore_from_checkpoint` re-binds callbacks *before* `Scheduler::restore`, not after.**
   Python re-binds afterwards because its scheduler holds the very same `Request` objects; the
   port's scheduler clones them into the queue, so the order has to flip for the effect to
   survive. An unknown name still falls back to `parse` — `s26` pins a *known* one surviving a
   pause/resume round trip.
10. **`concurrent_requests` is clamped to at least 1.** `CapacityLimiter(0)` raises in Python; a
    zero-permit `Semaphore` would deadlock and the spawn check `active >= 0` would spin.
    `stats.concurrent_requests` still reports the configured value.
11. **Blocked retries use `priority.saturating_sub(1)`.** Python's ints are unbounded;
    `i32::MIN - 1` is not.
12. **`Yielded::Error` is a value, where Python raises.** `Yielded` makes "the spider returned
    something that is neither a `Request`, a `dict` nor `None`" unrepresentable, so Python's
    `log.error("Spider must return Request, dict or None…")` branch has no counterpart, and
    `Yielded::Error` carries what Python would have raised. Because it is a *value*, a callback
    can keep yielding after it — and the engine must not act on anything that follows, which is
    what Python's `except Exception` around the `async for` does. Pinned twice: by
    `a_callback_failure_calls_on_error_once_and_stops_that_stream`, whose callback yields an item
    and a request after the error, and by `s09`, where the Python side `raise`s and the Rust side
    keeps yielding.
13. **`on_error` receives a `CrawlError`, not the original exception.** The corpus compares the
    request *paths* that reached the hook; the exception class name (`RuntimeError`,
    `ConnectionError`) has no analogue.
14. **`SessionKwargs` cannot express `data=None` as an explicit clear.** Python's
    `{**inherited, **kwargs}` puts a literal `None` in `_session_kwargs["data"]`; a typed
    `Option` cannot tell "explicitly `None`" from "absent", so the port says the same thing with
    an **empty** body — `RequestBody::is_falsy` is true for it, which is exactly how Python's
    `if post_data:` reads `None`. Same fingerprint, same wire bytes. `s23` drives both.
15. **`SpiderSettings::logging_level` and `log_file` are declarative.** `tracing` has no
    per-logger `%`-format, so Python's `logging_format` / `logging_date_format` are dropped and a
    runner or CLI builds the subscriber from the two fields that survive. The engine itself only
    reads `RunOptions::log_counter`.
16. **`Spider` behaviour and `SpiderSettings` state are split, and a separate `SpiderRunner` owns
    the `SessionManager`,** because Rust has no subclassing. The invariants that mattered survive:
    one `SessionManager` for the runner's lifetime, so `configure_sessions` runs once and pools
    are reused, and a fresh `CrawlerEngine` per run —
    `u17_engine_invariants.rs::the_same_runner_can_crawl_twice`.
17. **`SpiderRunner::start_blocking` builds a current-thread runtime** and refuses to run inside
    an existing one rather than deadlocking. Callers who want worker threads build their own
    runtime and `run().await`.
18. **The busy-wait is gone.** Python polls the queue every 10–50 ms; the port waits on a
    `tokio::sync::Notify` signalled by every enqueue, every finished task and every pause request,
    with `Notified::enable()` claiming a stored permit so no wakeup can be lost. The *observable*
    completion condition (queue empty **and** zero in flight) is unchanged. Periodic checkpoints
    are a `sleep_until` deadline in the same select, not a poll.
19. **`complete()` runs on cancellation and on a panic too.** Python relies on a `finally:` inside
    the task; the port puts the same bookkeeping in a `Drop` guard *owned by the spawned future*,
    so a task aborted by a force stop — even one aborted before its first poll — still releases
    its ticket and its concurrency slot. A panicking callback has no Python analogue at all and is
    pinned separately (`a_panicking_callback_still_completes_its_request`).

### Fixed here, not divergences

Three gaps the differential opened and closed. Each is a mutation the tests **did not** catch
before the fix and do now:

* **A callback error did not have to stop the stream.** `Behaviour::Fail` yielded only the error,
  so "and stops that stream" was in the test's name and not in its assertions; removing the
  `break` in `_run_callbacks` passed the whole suite. The callback now yields an item and a
  request *after* the error, and the corpus scenario keeps yielding where Python raises.
* **A force stop did not have to checkpoint what it was cancelling.**
  `a_force_stop_saves_before_it_cancels` pressed the ladder from inside the only callback there
  was, which had already returned by the time the loop reacted — so `abort_all` had nothing to
  abort and cancelling first changed nothing.
  `a_force_stop_checkpoints_the_requests_it_is_about_to_cancel` now blocks a second callback for
  30 s, and asserts the in-flight request is *in* the saved checkpoint.
* **The `robots.txt` `Crawl-delay` did not have to reach the sleep.** Every count, every URL and
  every stat is identical whether the engine sleeps or not; only the clock differs. Hence
  `MINIMUM_ELAPSED` above.

Unit 16 item 3 is also **closed here**: `FetchedResponse::encoding` now defaults to `"utf-8"`, as
Python's `Response.encoding` does, so the empty label is no longer reachable from an ordinary
construction. (The declared unit-16 corpus rows still diverge, because they set `""`
deliberately.)

---

## Unit 14 — `browser-cdp` (the CDP driver, the `Response` factory, the Cloudflare solver)

Verified against **real Google Chrome** driven by `the reference implementation` +
Playwright 1.62 over a local fixture — see `tools/corpus/u14/README.md` and
`crates/browser-cdp/src/live_u14/`. Everything below is a difference that
survived that comparison.

### Still divergent

1. **`route.abort()` maps to `Fetch.failRequest{errorReason: "Failed"}`**, not
   `BlockedByClient` as `ARCHITECTURE.md` §13.4 suggests. This is not really a
   divergence from Python at all — Playwright's `Route.abort()` defaults to the
   error code `"failed"` — and the live corpus confirms it: blocking the
   document host makes Python report `Page.goto: net::ERR_FAILED`, and so does
   the port (`blocking_the_document_itself_aborts_the_navigation_with_err_failed`).
   Constant: `cdp::fetch_domain::ABORT_ERROR_REASON`.
2. **A navigation that turns into a download** fails with the navigation
   timeout rather than Playwright's immediate
   `Page.goto: Download is starting`. Playwright reads `isDownload` off the
   `Page.navigate` reply; `chromiumoxide` intercepts `Page.navigate` and
   withholds the reply until its own navigation watcher sees a `load` lifecycle
   event, and a download commits no document, so the reply never arrives. Both
   engines fail the request; the port takes one `timeout` to do it. The
   `isDownload` branch is written and ready in `CdpPage::goto`, and
   `live_u14::tests::a_download_is_a_declared_divergence_not_pythons_download_is_starting`
   **fails if the divergence ever closes**, so a driver upgrade cannot silently
   leave the dead branch behind. Corpus row: `a_download_is_not_a_navigation`.
3. **`page_setup` / `page_action` hooks take `CdpPage` by value**
   (`PageHook = Arc<dyn Fn(CdpPage) -> BoxFuture<'static, Result<()>>>`), not
   `&Page` as §13.4 note 6 sketches. `CdpPage` is an `Arc` handle, so cloning is
   free, and a higher-ranked closure returning a borrow-capturing boxed future
   is close to unwritable in stable Rust. `session::page_hook` builds one from
   an async fn.
4. **`Response.captured_xhr` rides alongside the response**, in
   `BrowserFetch { response, captured_xhr }`, which `Deref`s to `Response`.
   Unit 12's `Response` is a fixed struct with no such field. `fetch()` returns
   the plain `Response`; `fetch_detailed()` returns the pair.
5. **`additional_args` is accepted and validated but not applied** — there is no
   Playwright context object to merge arbitrary options into. The session logs a
   `warn!` naming the key count and pointing at `page_setup`. (§13.4 note 7
   anticipated this.)
6. **`--disable-extensions` is unavoidably present**, one of Playwright's five
   `HARMFUL_ARGS`. `chromiumoxide` 0.9.1's launcher appends it whenever no
   extension is loaded and keeps its `Arg`/`ArgsBuilder` types private. The
   other four are never added, because the launch uses `disable_default_args()`
   and supplies `DEFAULT_ARGS`/`STEALTH_ARGS` explicitly.
7. **`chromiumoxide`'s `FrameManager` issues `Runtime.enable` on attach.**
   Upstream, unsuppressable without forking. Nothing in this crate emits it,
   nothing uses `Runtime.addBinding`, and every script runs in an isolated
   world — but the one CDP tell patchright exists to remove is still there at
   attach time. Documented at the top of `cdp/isolated_world.rs`, and
   demonstrated to be otherwise real by the solver's click test, which can only
   see the page's clicks through the DOM.
8. **Non-stealth sessions get a 1280×720 viewport** (Playwright's default for a
   context that does not ask for one) with `device_scale_factor: 2`; stealth
   sessions get 1920×1080. The reference implementation only sets `viewport` for stealth sessions,
   so the dynamic value is inherited from Playwright rather than stated anywhere
   in the reference implementation.
9. **`page.is_closed()` is a bounded CDP probe** (`Page.getFrameTree`, 5 s
   `TEARDOWN_TIMEOUT`), not the local flag Playwright maintains from
   `Target.targetDestroyed`, which `chromiumoxide` does not expose. One round
   trip per completed request — pinned at exactly one per request by
   `the_per_request_reset_runs_with_the_call_counts_test_page_reuse_pins`.
10. **`capture_xhr` is compiled eagerly** when the capture window opens, so an
    invalid pattern is a clean `BrowserError::InvalidXhrPattern` from `fetch()`
    rather than a `re.search` raising inside a response listener on whichever
    response happens to arrive first.
11. **A per-request `proxy` against a *persistent* session raises
    `BrowserNotInitialized`**, reproducing Python bug-for-bug:
    `launch_persistent_context` never assigns `self.browser`, so
    `_page_generator`'s `if not self.browser: raise` fires. Kept rather than
    fixed — a persistent profile and a per-request proxy are genuinely
    incompatible, since the profile's cookies would leak across proxies.
12. **The sync sessions' fixed pool of one tab is a façade concern** (§13.2):
    `BrowserSession::new` honours `max_pages`, and
    `BrowserSession::with_pool_size(config, hooks, 1)` is the seam the blocking
    façade uses to reproduce `session.max_pages == 1`.
13. **`Response.cookies` is unit 12's `IndexMap<String, String>`** (name to
    value), not Python's tuple of full cookie dicts, so `httpOnly` / `sameSite`
    / `partitionKey` are dropped for browser responses. **`Response.method` is
    always `GET`** for a browser response, which is what Python's
    `Response.__init__` default produces, since the browser factory never passes
    `method`.
14. **The Cloudflare solver takes a deadline.** `_cloudflare_solver` has two
    `while` loops that poll forever in Python — `frame_element().is_visible()`
    (the one §7 names) and the `non-interactive` wait-page loop. An unbounded
    loop in an async task is a hang, not a slow request, so `solve_cloudflare`
    takes one deadline (the request `timeout`) and bounds both. Every *bounded*
    loop keeps Python's exact counts (20 × 500 ms, 100 × 100 ms, 3 attempts).
15. **An *embedded* Turnstile with no challenge iframe returns without ever
    clicking**, because `_challenge_cleared` is unconditionally true for
    `embedded` and the `if not iframe or not outer_box:` branch reports success
    first. Faithful to Python, and surprising enough to have its own pinned
    test.

### Not divergences after all — behaviours the live corpus pinned

* **`Page.navigate` carries `referrerPolicy: "unsafeUrl"`**, matching
  Playwright's `crPage.js`. Without it Chrome's default
  `strict-origin-when-cross-origin` drops the referer on any HTTPS-to-HTTP
  navigation, which would make `google_search=True` a no-op for every `http://`
  target. Pinned live by
  `google_search_sends_the_referer_across_an_https_to_http_downgrade`, which
  reads the `Referer` **the fixture server received**.
* **Favicon requests are excluded from the `networkidle` in-flight count**
  (`Request._isFavicon = url.endsWith('/favicon.ico')`) and **a committed
  new-document navigation clears the previous document's in-flight set**
  (`_onClearLifecycle`). Neither is in the spec; both are in Playwright's
  shipped `frames.js`, and the first is now confirmed against real the reference implementation —
  `network_idle_ignores_a_hanging_favicon` returns in ~2.9 s where an ordinary
  hanging XHR takes the whole budget.
* **`load_dom` is a no-op.** `_wait_for_page_stability` waits for `load`
  *unconditionally* and only then, optionally, for `domcontentloaded` — which
  `load` already implies. The corpus rows `load_waits_for_the_image` and
  `load_dom_off` are identical, and the port reproduces both.

### Fixed here, not divergences

Four defects the live differential opened and closed. Each is a behaviour the
pre-existing tests did **not** catch and now do:

* **`timeout` did not bound `page.goto`.** `Page.navigate` was awaited with only
  `chromiumoxide`'s own budget, so a server that accepts a connection and never
  answers took **30 s** where Python raises `Page.goto: Timeout 3000ms exceeded`
  at 3 s. `CdpPage::goto` now carries one deadline across both halves — the
  command *and* the wait for `load` — exactly as Playwright's single navigation
  timeout does. Corpus rows `navigation_timeout` and `load_never_fires`;
  tests `a_navigation_that_never_commits_fails_at_the_configured_timeout` and
  `a_document_that_commits_but_never_reaches_load_fails_at_the_timeout_too`.
* **A `referer` in `extra_headers` made Chrome refuse the navigation.**
  `Referer` is a forbidden header for Chrome's network service: a request whose
  `ResourceRequest.headers` carry one — which is where
  `Network.setExtraHTTPHeaders` puts it — is rejected with
  `net::ERR_BLOCKED_BY_CLIENT`. Playwright's `gotoImpl` lifts it out of the
  extra headers into `Page.navigate`'s own `referrer` parameter, and the reference implementation
  depends on that: `fetch()` passes `referer=None` to `goto` precisely when the
  caller supplied one in `extra_headers`. `CdpPage::goto` now does the same
  merge. Corpus row `an_explicit_referer_beats_google_search`.
* **A same-document (fragment) navigation burned the whole timeout.** Python
  fails in 0.02 s with `RuntimeError: Failed to get response for <url>`, because
  Playwright sees no `loaderId` and waits for no lifecycle event. The port waited
  `timeout` for a `load` that cannot fire. `CdpPage::goto` now races
  `Page.navigate` against `Page.navigatedWithinDocument` for the main frame and
  returns immediately, restoring the previous document's lifecycle so the
  `_wait_for_page_stability` that follows still has its `load`. As part of that,
  `clear_request_state` no longer resets the lifecycle: it belongs to the
  document, not the request, and Playwright clears it only from
  `_onClearLifecycle`. Corpus row `a_fragment_on_the_same_document`.
* **`Response.headers` silently lost every `Set-Cookie`.**
  `Network.responseReceived` reports the headers the *renderer* is allowed to
  see; the unredacted ones arrive on `Network.responseReceivedExtraInfo`, and
  `hasExtraInfo` on the first event says whether they are coming. Playwright's
  `Response.allHeaders()` — which is what `convertor.py` reads for both
  `Response.headers` and `Response.request_headers` — is a *promise* that stays
  pending until that event resolves it. The port built the response eagerly from
  the redacted headers, so whether `Set-Cookie` survived was a race it usually
  lost. The ledger now consumes both `…ExtraInfo` events, pairs them to hops by
  position (as Playwright's `ResponseExtraInfoTracker` does), and
  `CdpPage::await_raw_headers` waits — bounded by `RAW_HEADERS_TIMEOUT` — for
  every response whose `hasExtraInfo` was true. Test
  `the_headers_come_from_the_first_response_and_the_request_headers_from_its_request`.

---

# Wave 5 integration — the seams for units 18 and 19

Written by the wave's green agent in `crates/browser-rs/tests/integration.rs`
(modules `crawl_spider_seam`, `site_to_markdown_seam`, `cli_extract_seam`), in
the same spirit as `browser_response_seam` and `crawl_loop_seam` before them:
answer the next wave's API questions in a test, against the crates as they
actually are, rather than in a merge conflict later.

## Fixed here, not divergences

* **The `markdown` and `adaptive` feature flags did not reach
  `browser-http`.** `Response::markdown()` / `markdown_with()` are
  `#[cfg(feature = "markdown")]` **in `browser-http`**, because `Response` is
  unit 12's type and is the only response type in the workspace. But every
  downstream crate forwarded the flag only to `browser-parser`:

  ```toml
  markdown = ["browser-parser/markdown"]   # browser-spider, before
  ```

  So `browser-spider --features markdown` compiled and yet a `CrawlResponse`
  had no `.markdown()` — and the façade's own `markdown` feature had the same
  hole, which would have hit unit 19 first (`browser-cli` depends on
  `browser-rs` with `features = ["markdown"]`, and `extract get out.md` is
  exactly that call). `browser-cdp` had no `markdown` feature at all, so
  `SiteToMarkdownSpider` could never have been pointed at the browser engine.

  Now: `browser-cdp` gains `markdown` / `adaptive`, and every forwarding
  list names `browser-http` as well as `browser-parser`. The façade uses the
  `dep?/feature` form, so enabling `markdown` alone still does not pull the
  browser or the spider into the graph. Two tests pin it, and neither *compiles*
  if the forwarding is removed:
  `browser-spider`'s `the_markdown_feature_reaches_the_response_through_deref`
  and `browser-cdp`'s `a_browser_response_converts_to_markdown`.

* **`crawl_loop_seam::Fixture::serve` half-closes and drains.** It used to
  `shutdown(Shutdown::Both)` straight after writing, which can RST a response
  the client has not read yet. Now `Shutdown::Write` followed by a
  `read_to_end`. Carried over into the new `local_site::Site` fixture, which
  additionally serves each connection on its own thread — a browser opens
  several at once and keeps them alive, so the single-threaded accept loop the
  wave-4 fixture used would have deadlocked the unit-18 markdown seam.

## Notes the unit-18 and unit-19 implementers must not rediscover

* **A `CrawlRule` must set `FollowOptions::callback` explicitly, even for a
  follow-only rule.** `Response.follow` *inherits* the originating request's
  callback when the option is `None` (`spider.md` §"follow"), so a rule that
  leaves it unset makes every page reached from a product page get parsed **as a
  product**. `requests_to_follow` in the seam test therefore writes
  `Some(rule.callback.clone().unwrap_or(CallbackId::PARSE))`. This is faithful
  to Python — `Rule(callback=None)` there means "use `parse`", not "use whatever
  the parent used" — but the two spellings look identical at the call site.
* **`strip_html5_whitespace` is not `str::trim`.** It strips exactly
  `\t`, `\n`, `\x0c`, `\r` and ` `. `str::trim` also strips U+00A0, U+2028,
  U+180E and the rest of Unicode `White_Space`, which would silently change
  which `href`s a `LinkExtractor` accepts. The seam test carries the correct
  implementation.
* **`Nodes::get()`, not `Nodes::first()`, is Python's `.get()`.** `first()`
  returns `Option<&Node>` (the element); `get()` returns `Option<String>` (the
  `::text` / `::attr()` result). Unit 18's extractors want `get()` / `texts()` /
  `attrs()`.
* **`_extract_content`'s trailing `""` is real and is asserted.** The generator
  always yields one final empty string, so `pieces.len() == matches + 1`. It
  does not change `"".join(...)`, and unit 19 should keep it so that a future
  test of the *generator* shape still matches Python.
* **`text` mode collapses `\n`, `\r`, `\t`, ` ` independently and in that
  order**, each with its own `[c]+ -> c` substitution. Doing it in one pass over
  a character class would merge a `"\n \n"` into a single space and is wrong.
* **The unknown-suffix case is an error, not a default.** `output.invalid` and
  `output` both raise; only `.md`, `.html` and `.txt` are accepted, matched by
  suffix and not by `Path::extension`.
* **Seconds versus milliseconds.** The seam test's `GetArgs::timeout_secs` is
  deliberately named: `extract get/post/put/delete` take **seconds** and hand a
  `Duration` to `RequestBuilder::timeout`, while `fetch` / `stealthy-fetch` take
  **milliseconds** and hand an `f64` to `BrowserConfig::timeout_ms`. Two units,
  one flag name, and unit 19 owns the split.

## Still divergent

Nothing new. The browser half of `site_to_markdown_seam` asserts that a
*rendered* DOM and a *raw* body converge on one markdown document for a static
page — which is an assertion about this fixture, not a law: a page whose DOM
JavaScript rewrites cannot agree, and that is the entire reason the browser
engine exists. It is asserted anyway because drift there would be a real
regression in one of the two paths.

---

# Wave 6 — units 18 and 19, and the façade re-export surface

---

## Unit 18 — `browser-templates` (the ready-made spiders)

Verified against `the reference implementation` + lxml 6.1.2 + w3lib + protego + CPython 3.14 over a **670-row
corpus in six files**, of which **596 reproduce (89.0 %)**, plus 30,196 out-of-corpus fuzz
observations (28,634 agreeing) across thirteen surfaces. Every one of the 74 misses is a row of
`crates/browser-templates/tests/u18_golden.rs::DECLARED_DIVERGENCES` and is asserted to **keep**
differing. The full table, with per-corpus rates, is in `crates/browser-templates/API.md`.

The rate is *lower* than an interim 481/495 (97.2 %) on purpose, and both halves of the drop are
deliberate: the corpus grew by 175 rows, 42 of them added **because** they do not reproduce, and the
`matches` comparison was un-loosened — it had been forcing both sides to `null` whenever CPython
raised, which made 18 rows agree by construction.

1. **`tags=()` or `attrs=()` yields no links instead of raising.** Python's `"| ".join(())` is `""`
   and `lxml.xpath("")` raises `XPathEvalError`, so a configuration that means "look nowhere" takes
   the whole callback down. 14 declared rows.
2. **A `<!-- comment -->` among a `<urlset>`'s direct children does not crash.** `etree.QName(
   comment.tag)` raises `ValueError` in `_extract_urls`, and `_sm_body` catches only
   `XMLSyntaxError`. Comments and processing instructions are dropped when the XML tree is built
   here. 2 declared rows.
3. **`urlsplit` cannot fail.** CPython raises `ValueError("Invalid IPv6 URL")` for an unbalanced
   bracket in the netloc, and neither `LinkExtractor.matches`, `_url_extensions` nor `_filename_for`
   catches it — one malformed `<loc>` kills the callback. `browser_spider::canonicalize::urlparse`
   is total by the workspace decision in §"Unit 15" item 2. 23 declared rows.
   **`safe_url_string` is the exception**: it re-derives CPython's bracket, host and port errors
   itself and returns a `SafeUrlError`, because w3lib's caller catches those.
4. **A document whose parsed root is `None` does not crash.** `Selector("<!-- c -->")._root` is
   `None` in lxml and `extract` reaches straight for `scope._root.xpath(...)`, so a comment-only page
   raises `AttributeError` in Python under *every* configuration. html5ever always builds a root. 27
   declared rows. (This is the same family as unit 19's `LXML_HAS_NO_ROOT`, found independently on
   the other side of the port.)
5. **`quick-xml` is not a validating parser — a new family, distinct from the html5ever tree-shape
   one.** lxml rejects an undeclared namespace prefix, an unescaped `<` in an attribute value and a
   redefined attribute; `_sm_body` turns each into an empty result plus a warning, losing the
   sitemap. This port reads all three, and reads an undeclared `a:item` as the *unprefixed* `item`. 6
   declared rows, and `XMLFeedSpider` inherits the same behaviour through `parse_xml`, where it is
   uncorpused (1,500 fuzzed feeds agree once every prefix is declared; the 1,031 that disagreed
   before that were all this).
6. **A lone `\r` inside an unquoted CSV field is a record terminator**, not
   `_csv.Error("new-line character seen in unquoted field")` — which `CSVFeedSpider.parse` does not
   catch, so a legacy Mac-line-ending feed takes the callback down in Python and is read here. 2
   declared rows; 645 of 2,000 fuzzed rows when `\r` is in the alphabet.
7. **`restrict_css="table"` scopes to a different element**, because html5ever foster-parents an
   `<a>` out of a `<table>` per the HTML5 spec and libxml2 does not. The T3 family, accepted
   workspace-wide in `ARCHITECTURE.md` §11.6. 2 declared rows; 18 of 1,500 malformed-markup fuzz
   pages and **0** of 2,400 well-formed ones.
8. **`safe_url_string` IDNA-encodes with UTS-46 non-transitional, not CPython's IDNA 2003.** They
   differ on `ß`, `ς`, ZWJ and ZWNJ — 66 of 1,500 fuzzed URLs, all `ß`. The same family as the
   unit-15 `canonicalize` divergence. Its `encoding` parameter is also accepted and **ignored**:
   w3lib encodes the query and fragment with the page's charset before percent-encoding, and no
   corpus or fuzz row is non-UTF-8, so nothing can see it.
9. **`_checknetloc` is not implemented.** CPython raises `ValueError` for a non-ASCII netloc whose
   NFKC normalisation contains `/?#@:`. It needs a Unicode normaliser this crate does not depend on,
   and no fuzz input reached it, so the size of the class is **unknown**.
10. **`python_title` carries a Unicode-version skew.** The three generated tables at the top of
    `shopify.rs` supply `_PyUnicode_ToTitleFull`, `_PyUnicode_IsCased` and
    `_PyUnicode_IsCaseIgnorable`, so `ß` → `"Ss"`, `ǅungla` → `ǅungla` and a word-final `Σ` → `ς` all
    reproduce. What is left is that CPython 3.14 and `rustc` 1.97 disagree about the case mapping of
    28 code points and about whether 5 others are cased (`꟎ ꟏ ꟒ ꟔ ꟱` and the Medefaidrin block,
    where `rustc` is ahead). 6 of 8,202 fuzzed strings. The same vintage-drift family as `psl`'s
    public-suffix snapshot.
11. **The XML declaration's encoding is ignored.** `quick-xml`'s `encoding` feature is off and the
    dependency set is frozen, so a non-UTF-8 feed or sitemap is decoded lossily rather than
    transcoded. **Zero coverage** — the distance from lxml was never measured, because it could not
    be closed without a dependency change.
12. **A `CrawlRule` always names its callback explicitly**, never `None` — the requirement the wave-5
    integration note predicted (§"Wave 5 integration"). `requests_to_follow` writes
    `Some(rule.effective_callback())`.
13. **`SitemapSpider`'s unruled route defaults to `parse`**, where Python's `_dispatch` inherits
    `_parse_sitemap` and feeds the target page back into the sitemap parser.
    `UnruledRoute::InheritSitemapCallback` reproduces Python exactly, for anyone who wants it.
14. **Smaller, all asserted by unit tests**: `ShopifySpider`'s `allowed_domains` fallback is the
    lexicographically first `BTreeSet` entry where Python takes an arbitrary `set` member;
    `process_request` may return `None` to drop a request, where Python's type cannot; a `write_item`
    failure is logged rather than raised; `CSVFeedSpider`'s delimiter and quote are `u8`, so a
    non-ASCII one-character Python delimiter cannot be expressed; `regex` has no backreferences or
    lookaround, so a pattern using them is `TemplateError::InvalidPattern`; and error messages
    interpolate the spider's `name` where Python interpolates `self.__class__.__name__`.

### Fixed here, not divergences

Seven real bugs came out of the fuzzing, all found by running Python rather than reading it:
`url_extensions` used `urlparse` (which peels off `;params`) where Python uses `urlsplit`, so
`/report.pdf;jsessionid=X` was dropped by the extension deny-list; `safe_url_string` ignored CPython's
`uses_netloc` rule in `urlunsplit` (146 of 1,500 random URLs wrong), split host from port on the
*last* colon rather than the first, accepted a `+`-signed port that `port.isdigit()` rejects, checked
bracket balance on the wrong substring, and never ran `_check_bracketed_host` at all; `python_title`
upper-cased where CPython title-cases (995 mismatches in 7,002 strings, and the divergence had been
mis-declared as "digraphs only, unreachable"); `CSVFeedSpider` skipped blank lines when picking the
header, where `DictReader.fieldnames` is a bare `next(self.reader)`; `ShopifySpider` errored on
`float(True)` (a bool **is** an int in Python) and keyed `collected_ids` on the JSON rendering, so `1`
and `1.0` were two variants where Python's `set` has one; and `gzip::decompress` refused an empty body
that Python inflates to `b""` and misclassified a stray trailing byte as truncation rather than
`BadGzipFile`.

**One more was closed by the wave-6 green agent.** Item 13 of the old list —
`float("٣.٥")` — is gone: `browser_spider::throttle::python_float` is now `pub` and `to_float`
calls it. Making that call exposed a **bug in `python_float` itself**, dating from unit 16. It
stripped with `str.strip()`'s rule; CPython's numeric parsers strip with C's `Py_ISSPACE` *after*
`_PyUnicode_TransformDecimalAndSpaceToASCII` has folded the **non-ASCII** whitespace to spaces. A C0
separator (`\x1c`–`\x1f`) is ASCII, so it survives the fold and is refused by the strip — even though
`str.strip()` removes it and `str.isspace()` correctly calls it whitespace:

```text
>>> float('\xa04.5')     # NBSP: folded to ' ', then stripped
4.5
>>> float(' 4.5\x1c')    # FILE SEPARATOR: ASCII, never folded
ValueError
>>> ' 4.5\x1c'.strip()   # …even though str.strip() does remove it
'4.5'
```

395 of 6,000 fuzzed strings disagreed. `python_float` and `python_int` are now 6,000/6,000 on both.
Pinned by `throttle::tests::the_numeric_strip_is_c_isspace_not_str_strip`. Unit 16's whole 172,447-
assertion corpus still passes unchanged, because its caller (`parse_retry_after_value`) does Python's
`.strip()` itself before calling `float()`, exactly as `reference/spiders/throttle.py` line 15 does.

---

## Unit 19 — `browser-cli` (the `brs` binary)

Verified against `the reference implementation` and, for the CLI surface, the real `browser-rs` console script.
Three committed corpora: 142 curl command lines (1,105 field checks), 73 Click invocations (1,093
field checks) and **1,302 `Convertor._extract_content` rows**. All 3,281 comparable field checks
reproduce. Row-level on the convert corpus: **1,196 of 1,302 (91.9 %)**, with the remaining 106 in
five declared families in `crates/browser-cli/tests/u19_convert_golden.rs::DIVERGENCES`, each
asserted to keep differing.

### Three families the green agent closed, 113 rows

Unit 19 was told not to edit `Cargo.toml`, so `Convertor._strip_noise_tags` / `_sanitize_for_ai` and
the *uncleaned* markdown conversion had no entry point reachable from the CLI. The consequences were
declared as `MARKDOWN_ALWAYS_SANITISES` (38 rows), `AI_TARGETED_CANNOT_CLEAN` (65) and
`MARKDOWN_PIECES_AGGREGATED` (14). Wave 6 added the two APIs that were actually missing —
`Document::clean_for_ai` and `Node::markdown`, both in `browser-parser` behind `markdown` — and
`extract_content` is now Python's control flow statement for statement. All 113 rows were deleted
from the divergence table.

Four rows that had been filed under those families turned out to have a *different* cause once the
real one was removed, and were **re-classified rather than deleted**: the three
`list-and-table/html/*/ai` rows are the implied `<tbody>`, and `template-first/markdown/body/plain` is
`<template>` being parsed into `<head>`. That is exactly the signal the "assert a declared row still
differs" rule exists to produce.

### What is left, none of it this crate's

1. **`LXML_HAS_NO_ROOT` (42 rows).** A document with no element — `<!-- just a comment -->` with
   `keep_comments=False` — leaves `Selector._root` as `None`, so **Python raises**
   (`AttributeError: 'NoneType' object has no attribute 'xpath'`, or `TypeError: Type 'NoneType'
   cannot be serialized` for the no-selector `html` and `markdown` rows). html5ever's tree builder
   always produces `<html><head></head><body></body></html>`, so the port succeeds and writes an
   empty file. Reproducing the crash would mean teaching `browser-dom` to return no root for a
   document HTML5 says has one, which is a worse answer for a scraper. Found by
   `tools/corpus/u19/fuzz_convert.py` seed 61.
2. **`HTML5EVER_TREE_SHAPE` (27 rows).** The synthesised `<head></head>` and the implied `<tbody>`.
   Carried since unit 5; `ARCHITECTURE.md` §11.6.
3. **`SYNTHESISED_HTML_SKELETON` (16 rows).** For `""` and `<html></html>`, html5ever fills in the
   whole skeleton, so `css("body")` matches an empty element where lxml finds nothing at all.
4. **`NOSCRIPT_LEAVES_HEAD` (13 rows).** An element HTML5 allows in `<head>` (`<noscript>`,
   `<template>`) written before any body content ends up **in `<head>`** under the HTML5 algorithm,
   which is what browsers do; libxml2 leaves it where it was written. So `css("body")` and
   `ignore_tags` see different things on the two sides.
5. **`TABLE_CELL_FRAGMENT` (4 rows).** `markdown` with a `td` selector. Both implementations
   *re-parse* — markdownify takes an HTML string, not a tree — but `html.parser` has no
   foster-parenting rule, so Python keeps the cell and emits `" a |"` where the HTML5 algorithm
   throws the `<td>` away and leaves the bare text. The fix is to parse the fragment in its parent's
   context, the way `innerHTML` does, and it belongs in `browser-dom` / `browser-markdown`.

### CLI-level divergences

6. **Output files are always written as UTF-8.** Python opens with `encoding=page.encoding`, so a
   windows-1252 page round-trips as windows-1252 there. `encoding_rs` is not a dependency of this
   crate; a non-UTF-8 page encoding is reported through `tracing::warn!` instead. **No corpus row
   covers this** and the end-to-end comparator serves UTF-8 only.
7. **`--version` prints this crate's version**, not `0.4.15`. Same shape, different number.
8. **Help *layout* is clap's, help *text* is Python's.** Click renders a boolean pair as one
   `--verify / --no-verify` line; clap renders two.
9. **`shell` and `mcp` are not implemented** (`ARCHITECTURE.md` §8). `uncurl` is a new subcommand
   carrying the shell's `uncurl()` and, with `--output`, `curl2fetcher()`.
10. **`-L/--loglevel` is global**, where Python exposes it only on `shell`; `critical` and `fatal`
    both map to `tracing`'s `error`, which makes the CLI slightly more talkative at those two
    settings and never less.
11. **`install` downloads Chromium through `chromiumoxide_fetcher`**, not `python -m playwright
    install chromium`. `install-deps` and `update_tld_names` have no analogue. The marker file keeps
    Python's name and semantics but lives in the browser cache directory. **The download has never
    been run** — it needs ~150 MB over a network this environment does not have.
12. **`--timeout` and `--wait` are `u64`**, so a negative value is a clap usage error (exit 2).
    Python's `type=int` accepts `-5` and passes it down to the transport.
13. **`platform_cache_dir` on Windows reads `%LOCALAPPDATA%`**, where `directories 6.0.0` reads the
    `FOLDERID_LocalAppData` known folder; they agree on a stock machine and can differ if the
    variable is unset or redirected. And where `dirs_sys::home_dir` falls back to `getpwuid` when
    `$HOME` is unset, this returns `None` and `install` reports `NoCacheDirectory`. Established by
    reading `directories`/`dirs-sys`/`chromiumoxide_fetcher` source, not by running on those
    platforms.
14. **The argparse emulation approximates one thing**: an unknown *short* option takes the rest of
    its cluster into the unknown list, where argparse's `consume_optional` recovery is finer-grained.
    102,000 fuzzed command lines agree; inputs outside them may not. The error *message texts* for
    `MissingValue` / `AmbiguousOption` / `UnexpectedValue` are paraphrases, because Python returns
    `None` for all of them and the text is never observable.
15. **`python_repr`'s printability test treats an unassigned code point (Cn) as printable**, where
    Python escapes it. No Unicode category table is available in this crate, and it can only ever
    change the *text of an error message*.

### Fixed here, not divergences

The differential caught nine real bugs in this crate. The largest: `--` was being swallowed
(argparse writes a `'-'` into its pattern, keeps the token in `arg_strings`, and drops it again only
from the slice of the action whose range covers it — 99 of the first 2,000 fuzz rows); a missing URL
must beat an unknown argument, because argparse raises the required-argument error *inside*
`parse_known_args` as a `ValueError` that `parse()` swallows into `None`; `-H ';'` really does produce
`{"": ""}`; four options tested presence where Python tests truthiness (`-X ''`, `-x ''`, `-U ''`,
`-b ''`); `repr()` chooses its quote character, so a rejected `{'a':1}` prints as `"{'a':1}"`;
`_negative_number_matcher` in 3.11+ is the *prefix* pattern `-\.?\d`, so `-d -0NaNfalse` is accepted
and `-d -Infinity` is not; the `shlex` **crate** is not `shlex.split` (it treats `#` as a comment
introducer, does not count `\r` as whitespace, and unescapes `\$` inside double quotes — 274 hits in
12,000 rows), so `uncurl` now carries a transcription of CPython's `shlex.read_token`; Click lets a
non-`multiple` option be repeated and takes the last, where clap refused (**388 of 1,500** random
argv lines); and `write_content_to_file` tested `Path::file_name` where Python does
`str(output_path).endswith(...)` on the whole path, so `dir.md/` was accepted here and raises there.

---

## Wave 6 integration — the façade re-export surface (unit 20 preflight)

Nothing here is a difference from Python. These are the port's own decisions, made while proving that
unit 20's four entry points are expressible from what exists —
`crates/browser-rs/tests/integration.rs::unit_20_facade_seam`, the only module in that file that does
**not** reach past the façade.

1. **Nothing a public method returns may be unnameable.** Four member crates were
   `[dev-dependencies]` of `browser-rs` only, so four types the public API hands back had no path
   through the façade: `Document::dom()` → `browser_dom::Document`, `Document::xpath_value()` →
   `browser_xpath::Value<XNode>`, the only `StorageSystem` implementation there is, and the two
   `Convertor` sanitiser passes. All four are now optional `[dependencies]` re-exported as
   `browser::{dom, xpath, storage, markdown}` under `parser` / `parser` / `adaptive` / `markdown`.
   None of them adds a compilation unit — `browser-parser` already depended on the first three
   unconditionally.
2. **A trait that can be named but not implemented is not re-exported.** `Spider` is declared
   `#[async_trait]` and its callbacks return `CallbackStream`, which is a type alias for
   `futures::stream::BoxStream<'static, Yielded>`. Without both macro and crate a façade-only user
   cannot write an `impl` block at all, and adding their own `async-trait` risks a version that does
   not unify with the one the trait was declared with. `browser::async_trait` and
   `browser::futures` are now re-exported under `spider`, and `CallbackStream` joined the flat
   surface.
3. **`browser-selectors` is still absent, deliberately.** No public signature anywhere mentions it.
   `browser-fingerprint`'s types arrive through the `http::config` re-exports.
4. **The feature-gating sharp edge is unchanged and is still only ever *smaller*.** `markdown` and
   `adaptive` are written as `browser-parser/markdown` and so on, which pulls the member crate in
   *without* turning on `feature = "parser"` here, and `blocking` does the same for
   `browser-http`. So `--no-default-features --features markdown` builds and exposes no `Document`.
   The failing direction — a `cfg` that is on while its crate is absent — cannot happen, because
   `parser`, `http`, `browser` and `spider` all name `dep:` explicitly. Unit 20 should add the `cfg`
   aliases that close it. **→ Closed in wave 7, and not with `cfg` aliases** — see
   [Wave 7, item 4](#4-the-feature-gap-is-closed-in-cargotoml-because-a-cfg-alias-cannot-close-it).
5. **The façade seam test is gated on five features at once** (`spider`, `browser`, `markdown`,
   `adaptive`, `blocking`), because it drives all four entry points. A test that vanishes with a
   feature flag is indistinguishable from a test that was deleted, so a
   `unit_20_facade_seam_needs_all_features` notice is compiled in its place under any smaller set.

---

# Wave 7 — unit 20 (the façade)

The last unit of the original plan: the benches, the examples, the feature-flag
closure and the docs. Nothing here is a difference from Python — the façade has
no Python counterpart at all. These are the port's own decisions, recorded for
the same reason as everything above: so that none of them is an accident.

## 1. The benches measure the round trip, not the pieces

`ARCHITECTURE.md` §7 pins one number for this crate: parse 5000 nested
`div.item`, then `css(".item")`, under 0.5 s. `benches/parse.rs` writes that as
**one** criterion benchmark (`budget/parse_then_css_5000`) rather than as the
sum of a parse benchmark and a query benchmark, because a caller who parses and
then queries pays for the tree once and the sum would overstate it.

"Nested" is read as *structural* nesting — 5000 cards, each with three element
children, under a six-deep wrapper — not as a 5000-deep tree. A 5000-deep DOM
measures recursion limits rather than parsing, and no real page is one.

Two benchmarks assert inside the timed loop (`budget/parse_then_css_5000` checks
that 5000 elements matched; `relocate/*` checks that the element was found).
That is deliberate: without it, a benchmark cannot distinguish "this got faster"
from "this stopped doing the work".

## 2. Relocation is not linear in page size, and the curve has a step in it

`benches/relocate.rs` was written expecting a linear scan and found neither a
linear curve nor a smooth one. Two things are worth recording, because both are
faithful behaviour that a reader would otherwise take for a bug:

* `similarity_score` compares the element's **sibling tag list** with a
  `SequenceMatcher` ratio. On a page whose cards are all siblings, growing the
  page grows both the candidate count and the length of every comparison, and
  the two multiply.
* CPython's `difflib` purges "popular" elements from the sequence it indexes,
  but only once that sequence reaches 200 entries — `AUTOJUNK_THRESHOLD` in
  `browser-core`'s `similarity` module, which reproduces the heuristic exactly.
  `siblings` excludes the element itself, so a page of **201** identical cards
  gets the purge and a page of **200** does not, and the larger page relocates
  **33× faster** than the smaller one: 26.9 ms against 0.81 ms, one card apart.
  `relocate_autojunk` is that pair.

The `relocate` size sweep therefore stays entirely above 200 cards so that its
three numbers are comparable, and a separate `relocate_autojunk` group pins the
boundary at the single card that crosses it. Nothing was changed to smooth the
curve; the benchmark reports it.

## 3. `relocate` and `save`/`retrieve` are benchmarked against an in-memory database

`benches/relocate.rs` uses `SqliteStorage::in_memory`, so what the
`adaptive_storage` group reports is SQLite and serialisation cost with the
filesystem taken out. A real `SqliteStorage::open` against a disk file is slower
by whatever that machine's fsync costs, and that number belongs to the machine
rather than to this library.

## 4. The feature gap is closed in `Cargo.toml`, because a `cfg` alias cannot close it

The wave-6 preflight (item 4 above) recorded that
`--no-default-features --features markdown` compiled and exposed no `Document`,
and proposed `cfg` aliases. `cfg` aliases do not work here, and the reason is
worth writing down:

* `markdown` and `adaptive` name `browser-parser/markdown` and
  `browser-parser/adaptive` — the non-`?` form, which force-enables the optional
  `browser-parser` dependency without turning on the façade's own
  `feature = "parser"`. `blocking` does the same for `browser-http`.
* Widening the `cfg` on the `browser_parser` re-exports would work, because
  `browser-parser` really is in the graph in those builds.
* It would **not** work for `browser-dom` and `browser-xpath`, which are separate
  optional dependencies that only `parser` enables. Under markdown-only they are
  not *nameable* from the façade however the `cfg`s are written.
* So a `cfg` alias buys a build in which `Document::dom()` and
  `Document::xpath_value()` exist and return types that have no path through
  `browser::` — precisely the property `crates/browser-rs/src/lib.rs` says this
  crate maintains, and the property wave 6 added four dependencies to restore.

The fix is therefore in the feature table: `markdown` and `adaptive` name
`"parser"`, and `blocking` names `"http"`. It costs nothing to compile —
`browser-parser` depends on `browser-dom`, `browser-selectors` and
`browser-xpath` unconditionally, and `browser-http` depends on `browser-parser`
unconditionally, so every crate the implication adds was already being built.
What it adds is the re-export path, not the code.

The invariant is now enforced at compile time rather than by a test. Three
`compile_error!` guards at the top of `crates/browser-rs/src/lib.rs` fire in
exactly the configurations that regressed — `markdown` or `adaptive` without
`parser`, `blocking` without `http`. A test could not do this: no test binary in
this repository is ever built under `--no-default-features --features markdown`.

## 5. Two names were added to the curated surface

* **`CrawlError`** (`browser_spider::spider::CrawlError`) is now flat, next to
  `SpiderError`. It is what `SpiderRunner::new` and `SpiderRunner::run` return,
  and a caller who cannot name the error of the entry point cannot write the
  entry point's signature. It was reachable through
  `browser::spider::spider::CrawlError` before, so this is a shortening, not a
  hole being closed.
* **`browser::error::Error`** now exists. `crates/browser-rs/src/error.rs` had
  been a documented stub since unit 1, leaving `pub mod error;` exporting an
  empty module. It is one `thiserror` enum with a `#[from]` for each member
  crate's error, per `ARCHITECTURE.md` §5.3 — an application that reaches for two
  entry points can write one `Result` and one `?`.

  It is deliberately **not** re-exported flat at the crate root: `browser::Error`
  next to `ParseError` and `FetchError` would read as the one a given call
  returns, and none of them return it; and a flat `Result` would shadow
  `std::result::Result` in any module that glob-imported the crate root. Every
  variant is `#[error(transparent)]`, so a message the specs pin as observable
  survives being carried through it unchanged — asserted by
  `error::tests::wrapping_does_not_change_the_message`.

  **Three variants are boxed and the rest are not**, which is worth explaining
  because it looks arbitrary. `ParseError`, `FetchError` and `StorageError` are
  120 bytes each and every other member error is 72 or less; left inline the
  aggregate came to exactly 128 bytes, which is `clippy::result_large_err`'s
  threshold, and the lint is right — `Result<T, Error>` is what a *caller's*
  signatures become, so the size is paid on their success paths too. Boxing
  those three brings it to 80; boxing a 16-byte `DomError` as well would be a
  heap allocation bought for nothing. `#[from]` cannot box, so those three have
  a hand-written `From` generated by a local macro, and nothing changes at a
  call site: `?` still converts and `Box<E>` derefs.
  `error::tests::the_aggregate_stays_under_the_large_err_threshold` fails before
  clippy does if a new variant pushes it back over.

## 6. Three of the four examples use the public network; the crawler does not

`examples/spider.rs` starts its own loopback HTTP server and crawls that.
Crawling a third party's site to demonstrate a crawler is bad manners and makes
the output depend on their deploy schedule, and a crawl is the one case where
"just point it at a real site" has a cost to somebody else.

`one_shot_fetch`, `session` and `browser_fetch` do make real requests, to
`https://example.com/` unless a URL is given as the first argument — IANA's
reserved documentation domain, which exists for this. Each says so in a marked
block at the top of the file. None of them runs during `cargo test`: cargo
compiles example targets and never executes them, and no CI job invokes them.

Every example declares its features through `required-features`, so
`cargo build --examples` on a smaller feature set *skips* the ones it cannot
build instead of failing. `browser_fetch` additionally probes for a browser
binary and exits 0 with an explanation when there is none, because a machine
without Chrome is an ordinary machine.

## 7. The measured numbers, and what they are worth

Recorded because the wave-6 handoff carried an unmeasured claim about the §7
budget, and an unmeasured number in a register like this one is worse than no
number. Every figure below is a criterion mean from a single clean
`cargo bench -p browser-rs --benches --all-features` run.

**Machine:** Apple M5 Pro (15 cores), macOS 26.5.2, rustc 1.97.0, `[profile.bench]`
(`lto = "thin"`, `codegen-units = 1`). **It was not a quiet machine** — this is a
shared working tree and other builds ran on it during the session. Repeat runs of
the same benchmark varied by up to ~50% under load, so treat every figure as an
order of magnitude and a shape, not as a number to regress against. The
`target/criterion` baselines are the thing to compare against, on one machine, at
one time.

### The §7 budget

| | |
|---|---|
| Spec | parse 5000 nested `div.item`, then `css(".item")`, **under 0.5 s** |
| Measured | **15.5 ms** (`budget/parse_then_css_5000`, 95% CI 15.4–15.8 ms) |
| Margin | **~32× under budget** |

The wave-6 handoff guessed "~50×" from a rough release-mode timing. The measured
margin is 32×, on the harness, with the assertion in the loop. Both the guess and
the measurement clear the budget comfortably; only one of them is a number.

### Everything else

| Group / benchmark | Mean |
|---|---|
| `parse/1000` | 2.71 ms |
| `parse/5000` | 15.04 ms |
| `parse/20000` | 64.01 ms |
| `query/css_class` (5000 cards) | 280 µs |
| `query/css_with_opts` | 279 µs |
| `query/find_all` | 283 µs |
| `query/css_text_pseudo` | 696 µs |
| `query/xpath_predicate` | 5.07 ms |
| `relocate/500` | 2.16 ms |
| `relocate/1000` | 7.44 ms |
| `relocate/5000` | 163.2 ms |
| `relocate_autojunk/200_cards` | 26.87 ms |
| `relocate_autojunk/201_cards` | 813 µs |
| `relocate_threshold/{10,40,80}` | 8.30 / 7.25 / 7.85 ms |
| `adaptive_storage/save` (in-memory) | 24.9 µs |
| `adaptive_storage/retrieve` (in-memory) | 29.0 µs |
| `html_to_markdown/{10,100,500}` sections | 237 µs / 1.56 ms / 7.49 ms |
| `convert/to_markdown_default` (100 sections, pre-parsed) | 814 µs |
| `sanitize/strip_noise_tags` | 191 µs |
| `sanitize/sanitize_for_ai` | 239 µs |
| `sanitize/clean_for_ai` (both passes) | 907 µs |
| `document/markdown` | 1.97 ms |
| `document/markdown_with_selector` | 3.11 ms |
| `document/parse_then_markdown` | 2.82 ms |

Three readings worth keeping:

* **Selection is not where the time goes.** Parsing 5000 cards costs 15 ms and
  selecting all of them costs 0.28 ms — about 2%. CSS, `find_all` and
  `css_with` are within a few percent of each other; XPath with a predicate is
  ~18× the CSS cost, which is the price of a general expression evaluator rather
  than a compiled matcher.
* **`markdown_with_selector` is not the cheap option.** Narrowing to `<article>`
  costs *more* than converting the whole document (3.11 ms against 1.97 ms),
  because the selector runs over the sanitised tree on top of everything the
  unnarrowed call already did. Narrow for output quality, not for speed.
* **The threshold buys nothing.** `relocate_threshold` at 10, 40 and 80 are
  within noise of each other, because the scan happens whatever the threshold is
  and only the *result* is gated.
