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
| T2 | `remove_blank_text` deletes **every** whitespace-only text node outside `pre`/`textarea`; libxml2's `areBlanks()` keeps blanks whose parent's content model allows PCDATA, and never treats U+00A0 as blank | `:empty`, `::text` results, `element.text` in an adaptive fingerprint, and — the costliest case — Markdown for ordinary prose: `<p><b>b</b> <i>i</i></p>` renders `**b***i*` here and `**b** *i*` there |
| T3 | Malformed markup recovers per the HTML5 spec (adoption agency, foster parenting, auto-closing), not per libxml2 | `<p>x<b>y<i>z</p>`, text inside `<table>`, `<h6><h6>x</h6></h6>`, a `<table>` inside an `<h4>` |
| T4 | A valueless HTML attribute is `""` here; libxml2 gives it its own name (`checked="checked"`), and lower-cases SVG attribute names where html5ever keeps the camel case (`viewBox`) | `[checked="checked"]`, `svg::attr(viewBox)` |
| T5 | html5ever keeps `xmlns:*` attributes that libxml2's HTML parser discards, and applies the HTML5 named-character-reference rule (`&notreal;` → `¬real;`) that `html.parser` does not | `count(//*/namespace::*)`, entity round-trips |
| T6 | `<noscript>` is tokenised with `scripting_enabled: false`, so its children are markup — matching libxml2 and `html.parser`. A `<noscript>` that *starts* the document still lands in `<head>`, where HTML5 pops it and moves the following `<p>` into `<body>` | `<noscript>` at the very start of a document |
| T7 | The HTML5 tokenizer eats one `U+000A` immediately after `<pre>`/`<textarea>`; libxml2 does not. Our serialiser also does not re-emit the newline the HTML serialisation spec requires, so `<pre>` is not round-trip stable | invisible at the reference implementation's defaults (`strip_pre` removes leading newlines anyway); visible under a non-default `strip_pre` |

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

Beyond T1/T2/T3/T5/T6/T7 above, all of which land here:

1. **Attribute values are the author's.** `lxml`'s HTML serialiser
   percent-escapes URI-valued attributes during the reference implementation's `.html_content`
   round trip (`href="/a b"` → `/a%20b`); we convert the live tree and keep the
   href as written.
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
