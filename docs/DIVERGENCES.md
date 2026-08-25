# Divergences from the Python reference

Every intentional difference in behaviour, in one place, so that none of them is
an accident.

> **Status.** Work unit 20 compiles this file in full from `ARCHITECTURE.md` §9.
> Until then it records only the divergences that already exist in the tree, so
> that no later unit has to remember them.

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
   the reference implementation ever uses — namespaces nothing. `DomModel` maps the XHTML namespace
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
   the reference implementation renders `<ul><li></li><li>x</li></ul>` as `* * x`. We emit `* x`.
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
   the reference implementation's own type annotation advertises them, but `curl_cffi` only
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

Verified against browser-rs 0.4.15 / w3lib 2.4.1 / CPython 3.14 by the corpora under
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
    `method="post"` fingerprints differently from `method="POST"`. the reference implementation's own
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
  over HTTP. the reference implementation's own `__CHARSET_RE__` (`charset=["\']?([\w-]+)`) *does* accept
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
   directly still answers `None`, exactly like CPython. 5 corpus keys. **The real fix belongs to
   unit 17:** default `FetchedResponse::encoding` to `"utf-8"` at construction.
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
