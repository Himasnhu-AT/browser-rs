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
