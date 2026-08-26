//! The unit-18 differential: replay `tools/corpus/u18/`'s golden files against
//! this crate and account for every row.
//!
//! The corpus was produced **once** from `the reference implementation`, `lxml`, `w3lib`
//! and CPython 3.14 (see `tools/corpus/u18/README.md`) and committed, so this
//! replay needs neither Python nor a network.
//!
//! # The corpus rule
//!
//! A row that does not reproduce is **never deleted, skipped, or loosened**. It
//! goes into [`DECLARED_DIVERGENCES`] with a written reason, and the test then
//! asserts that it **still differs** — so a future change that accidentally
//! fixes it is flagged rather than silently drifting. The pass rate this file
//! prints is therefore an honest count, not a construction.
//!
//! As it stands: **596 of 670 rows reproduce (89.0%)**, over six corpora.
//! That rate came *down* from an earlier 481/495 (97.2%) for two deliberate
//! reasons — 175 rows were added, 42 of them chosen precisely because they do
//! not reproduce, and the `matches` comparison was tightened. It had been
//! forcing both sides to `null` whenever CPython raised, which made 18 rows
//! agree by construction; they now differ and are declared.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use browser_templates::feed::{CSVFeedSpider, CSV_RESTKEY};
use browser_templates::gzip::decompress;
use browser_templates::links::{
    safe_url_string, strip_html5_whitespace, url_extensions, LinkExtractor, LinkExtractorBuilder,
    Response,
};
use browser_templates::shopify::{python_title, remove_tags, ShopifySpider};
use browser_templates::sitemap::parse_sitemap;

// ---------------------------------------------------------------------------
// Declared divergences
// ---------------------------------------------------------------------------

/// The twelve `links/extract` rows where an empty `tags` or `attrs` makes lxml
/// raise, one reason shared between them.
const EMPTY_CROSS_PRODUCT: &str =
    "`tags=()` (or `attrs=()`) makes Python's `\"| \".join(...)` produce the \
     empty string, and `lxml`'s `xpath(\"\")` raises `XPathEvalError` — so the \
     whole callback dies on a configuration that plainly means \"look nowhere\". \
     `LinkExtractor::search_selector` produces the same empty expression and \
     `extract` short-circuits to no links instead. Deliberate, documented on \
     `LinkExtractor::search_selector`, and asserted here to keep differing.";

/// Rows this port knowingly does not reproduce, each with its reason.
///
/// The key is `"<corpus>/<case>"`. Every entry is asserted to **still differ**;
/// if one starts matching, this test fails and the entry must be removed.
const DECLARED_DIVERGENCES: &[(&str, &str)] = &[
    ("links/extract/shop-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/shop-24", EMPTY_CROSS_PRODUCT),
    ("links/extract/dupes-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/dupes-24", EMPTY_CROSS_PRODUCT),
    ("links/extract/archives-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/archives-24", EMPTY_CROSS_PRODUCT),
    ("links/extract/attrs-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/attr-order-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/attr-order-24", EMPTY_CROSS_PRODUCT),
    ("links/extract/attrs-24", EMPTY_CROSS_PRODUCT),
    ("links/extract/nested-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/nested-24", EMPTY_CROSS_PRODUCT),
    (
        "sitemap/comment-alt0",
        "A `<!-- comment -->` among a `<urlset>`'s direct children CRASHES Python. \
         `_extract_urls` calls `_get_type(child)` = `etree.QName(el.tag).localname` \
         on every child, and an lxml comment's `.tag` is a *function*, not a \
         string, so `QName` raises `ValueError` — which `_sm_body` does not catch \
         (it catches only `XMLSyntaxError`). The port drops comments and \
         processing instructions when it builds the tree, so it parses the \
         sitemap and returns its URLs. `spider-support.md` §9.5 notes the hazard \
         and observes that real sitemaps have none at that level. This is a \
         strict improvement and is deliberate; it stays declared so that a \
         future change which starts crashing here gets flagged.",
    ),
    (
        "sitemap/comment-alt1",
        "As `sitemap/comment-alt0`: `sitemap_alternate_links` does not change \
         which children `_extract_urls` walks, so the same comment crashes \
         Python on this row too.",
    ),
    ("links/extract/tables-23", EMPTY_CROSS_PRODUCT),
    ("links/extract/tables-24", EMPTY_CROSS_PRODUCT),
    ("links/extract/tables-25", FOSTER_PARENTING),
    ("links/extract/tables-26", FOSTER_PARENTING),
    ("links/extract/comment-only-*", NULL_ROOT),
    ("links/matches/match-*-http://[::1/posts/1", TOTAL_URLSPLIT),
    ("links/matches/match-*-http://a]b/posts/1", TOTAL_URLSPLIT),
    ("helpers/url_extensions/http://[::1/x.pdf", TOTAL_URLSPLIT),
    ("helpers/url_extensions/http://a]b/x.pdf", TOTAL_URLSPLIT),
    ("filename/filenames-14", TOTAL_URLSPLIT),
    ("sitemap/undeclared-prefix-*", NON_VALIDATING_XML),
    ("sitemap/unescaped-lt-in-attribute-*", NON_VALIDATING_XML),
    ("sitemap/duplicate-attribute-*", NON_VALIDATING_XML),
    ("csv/lone-cr", LONE_CARRIAGE_RETURN),
    ("csv/lone-cr-midfield", LONE_CARRIAGE_RETURN),
];

/// `restrict_css="table"` — the tree-shape family `docs/DIVERGENCES.md` T3
/// already names.
const FOSTER_PARENTING: &str =
    "html5ever foster-parents an `<a>` out of a `<table>` exactly as the HTML5 \
     spec requires, so it is a *sibling* of the table here and a *child* of it \
     in libxml2. `restrict_css=\"table\"` therefore scopes to an element that \
     contains the links in Python and contains nothing here. This is \
     `docs/DIVERGENCES.md` T3, accepted workspace-wide in `ARCHITECTURE.md` \
     §11.6, and it is not a `LinkExtractor` difference: the same page with the \
     links outside the table reproduces exactly. The u18 fuzz found it on 18 \
     of 1500 malformed-markup pages and on none of 2400 well-formed ones.";

/// A document whose parsed root is `None`.
const NULL_ROOT: &str = "A document that is *only* a comment CRASHES Python. \
     `Selector(\"<!-- c -->\")._root` is `None`, and `extract` reaches \
     straight for `scope._root.xpath(...)`, so every configuration raises \
     `AttributeError` before a single link is considered — including the ones \
     that never touch the DOM. `browser-dom` always has a document node, so \
     the port returns no links. A strict improvement, and one no crawl should \
     be able to trip over; declared so that a change which starts crashing \
     here is flagged.";

/// CPython's `urlsplit` raises; this port's does not.
const TOTAL_URLSPLIT: &str =
    "CPython's `urlsplit` raises `ValueError(\"Invalid IPv6 URL\")` on an \
     unbalanced bracket in the netloc, and neither `LinkExtractor.matches`, \
     `_url_extensions` nor `_filename_for` catches it — one malformed `<loc>` \
     in a sitemap takes the whole callback down. \
     `browser_spider::canonicalize::urlparse` is deliberately total \
     (`docs/DIVERGENCES.md` §\"Unit 15\" item 2: a request that cannot be \
     hashed cannot be scheduled), so the port answers best-effort. Note the \
     knock-on inside `filename/filenames-14`: because Python raised on the \
     first URL it never recorded that name, so nothing downstream collides. \
     `filename/filenames-15` is the boundary case and is *not* declared — a \
     `[` in the path is not in the netloc, CPython does not raise, and the two \
     agree.";

/// `quick-xml` accepts documents lxml refuses.
const NON_VALIDATING_XML: &str =
    "`lxml` is a validating-enough parser to reject an undeclared namespace \
     prefix, an unescaped `<` inside an attribute value and a redefined \
     attribute; `_sm_body` catches the `XMLSyntaxError` and returns an empty \
     result plus a warning, so the whole sitemap is silently lost. `quick-xml` \
     does none of those checks and reads the document. Closing the gap would \
     mean writing a namespace and attribute validator on top of `quick-xml`, \
     which would make well-formed sitemaps no better and malformed ones \
     strictly worse. The u18 sitemap fuzz reaches these three classes and no \
     others: 1200/1200 well-formed bodies reproduce exactly.";

/// A bare `\r` inside an unquoted CSV field.
const LONE_CARRIAGE_RETURN: &str =
    "A lone `\r` inside an unquoted field makes CPython's reader raise \
     `_csv.Error(\"new-line character seen in unquoted field\")`, which \
     `CSVFeedSpider.parse` does not catch — a legacy Mac-line-ending feed \
     takes the callback down rather than being read. The `csv` crate treats \
     `\r` as a record terminator and reads the feed. The u18 CSV fuzz hits \
     this on 645 of 2000 rows when `\r` is in the alphabet and on 0 of 3000 \
     when it is not, and every non-raising row reproduces exactly.";

/// Whether `key` is declared, and its reason.
///
/// A table entry containing one `*` matches any key with that prefix and
/// suffix, which is how one reason covers a whole page's worth of
/// configurations. It does **not** weaken the rule: every row is still counted
/// individually, and a wildcarded entry that starts covering a row which now
/// *agrees* makes the `fixed` assertion fire just as an exact one would.
fn declared(key: &str) -> Option<&'static str> {
    DECLARED_DIVERGENCES
        .iter()
        .find(|(candidate, _)| match candidate.split_once('*') {
            Some((prefix, suffix)) => {
                key.len() >= prefix.len() + suffix.len()
                    && key.starts_with(prefix)
                    && key.ends_with(suffix)
            }
            None => *candidate == key,
        })
        .map(|(_, reason)| *reason)
}

// ---------------------------------------------------------------------------
// Tally
// ---------------------------------------------------------------------------

/// Counts rows and enforces the corpus rule.
#[derive(Default)]
struct Tally {
    total: usize,
    reproduced: usize,
    /// Declared rows that really did still differ.
    honoured: Vec<String>,
    /// Rows that differ and are **not** declared: real failures.
    unexpected: Vec<String>,
    /// Declared rows that started passing: also failures, in the other
    /// direction.
    fixed: Vec<String>,
}

impl Tally {
    fn record(&mut self, key: &str, expected: &Value, actual: &Value) {
        self.total += 1;
        let agrees = expected == actual;
        match (agrees, declared(key)) {
            (true, None) => self.reproduced += 1,
            (true, Some(_)) => self.fixed.push(key.to_owned()),
            (false, Some(_)) => self.honoured.push(key.to_owned()),
            (false, None) => self.unexpected.push(format!(
                "{key}\n     python: {expected}\n     rust:   {actual}"
            )),
        }
    }

    fn finish(self, name: &str) -> (usize, usize) {
        let rate = if self.total == 0 {
            100.0
        } else {
            100.0 * self.reproduced as f64 / self.total as f64
        };
        eprintln!(
            "u18 {name}: {}/{} ({rate:.1}%), {} declared divergences honoured",
            self.reproduced,
            self.total,
            self.honoured.len()
        );
        assert!(
            self.unexpected.is_empty(),
            "{} row(s) of `{name}` do not reproduce and are not declared. \
             Fix the port, or add each to DECLARED_DIVERGENCES with a reason — \
             never delete the row.\n  {}",
            self.unexpected.len(),
            self.unexpected.join("\n  ")
        );
        assert!(
            self.fixed.is_empty(),
            "{} row(s) of `{name}` are declared divergences but now MATCH Python. \
             That is good news: delete them from DECLARED_DIVERGENCES.\n  {}",
            self.fixed.len(),
            self.fixed.join("\n  ")
        );
        (self.reproduced, self.total)
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

fn load(name: &str) -> Vec<Value> {
    let path = testdata(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is missing: {error}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON object per line"))
        .collect()
}

fn text_of(row: &Value, key: &str) -> String {
    row[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is a string in {row}"))
        .to_owned()
}

fn base64_decode(encoded: &str) -> Vec<u8> {
    // A tiny standard-alphabet decoder: the corpus carries binary gzip bodies
    // and this crate has no `base64` dependency.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits = 0_u32;
    for byte in encoded.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(index) = ALPHABET.iter().position(|candidate| *candidate == byte) else {
            continue;
        };
        buffer = (buffer << 6) | index as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shared builders
// ---------------------------------------------------------------------------

/// A `LinkExtractor` from the corpus's JSON kwargs.
fn extractor_from(config: &Value) -> LinkExtractor {
    let mut builder = LinkExtractorBuilder::new();
    let Some(config) = config.as_object() else {
        return builder.build().expect("no patterns");
    };

    let strings = |value: &Value| -> Vec<String> {
        match value {
            Value::String(text) => vec![text.clone()],
            Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_owned())
                .collect(),
            _ => Vec::new(),
        }
    };

    for (key, value) in config {
        builder = match key.as_str() {
            "allow" => builder.allow(strings(value)),
            "deny" => builder.deny(strings(value)),
            "allow_domains" => builder.allow_domains(strings(value)),
            "deny_domains" => builder.deny_domains(strings(value)),
            "restrict_css" => builder.restrict_css(strings(value)),
            "restrict_xpath" => builder.restrict_xpath(strings(value)),
            "tags" => builder.tags(strings(value)),
            "attrs" => builder.attrs(strings(value)),
            "deny_extensions" => builder.deny_extensions(strings(value)),
            "canonicalize" => builder.canonicalize(value.as_bool().unwrap_or(true)),
            "strip" => builder.strip(value.as_bool().unwrap_or(true)),
            "keep_fragment" => builder.keep_fragment(value.as_bool().unwrap_or(false)),
            other => panic!("unknown LinkExtractor kwarg {other:?}"),
        };
    }
    builder
        .build()
        .expect("the corpus only carries valid patterns")
}

// ---------------------------------------------------------------------------
// 1. helpers
// ---------------------------------------------------------------------------

#[test]
fn helpers_reproduce_w3lib_and_cpython() {
    let mut tally = Tally::default();
    for row in load("u18_helpers.jsonl") {
        let kind = text_of(&row, "kind");
        match kind.as_str() {
            "strip_html5_whitespace" => {
                let input = text_of(&row, "input");
                tally.record(
                    &format!("helpers/strip/{input:?}"),
                    &row["out"],
                    &json!(strip_html5_whitespace(&input)),
                );
            }
            "url_extensions" => {
                let input = text_of(&row, "input");
                // A `null` `out` means CPython's `urlsplit` raised; the port
                // is total, so those rows are declared, not folded into the
                // answer.
                let expected = if row["error"].is_null() {
                    json!({ "chain": row["out"].clone() })
                } else {
                    json!({ "raised": row["error"].clone() })
                };
                tally.record(
                    &format!("helpers/url_extensions/{input}"),
                    &expected,
                    &json!({ "chain": url_extensions(&input) }),
                );
            }
            "safe_url_string" => {
                let input = text_of(&row, "input");
                let expected = if row["error"].is_null() {
                    row["out"].clone()
                } else {
                    Value::Null
                };
                let actual = match safe_url_string(&input, "utf-8") {
                    Ok(url) => json!(url),
                    Err(_) => Value::Null,
                };
                tally.record(
                    &format!("helpers/safe_url_string/{input}"),
                    &expected,
                    &actual,
                );
            }
            "remove_tags" => {
                let input = text_of(&row, "input");
                tally.record(
                    &format!("helpers/remove_tags/{input:?}"),
                    &row["out"],
                    &json!(remove_tags(&input)),
                );
            }
            "title" => {
                let input = text_of(&row, "input");
                // The corpus records the Shopify `category` expression:
                // `handle.replace("-", " ").title().strip()`.
                let actual = python_title(&input.replace('-', " ")).trim().to_owned();
                tally.record(
                    &format!("helpers/title/{input:?}"),
                    &row["out"],
                    &json!(actual),
                );
            }
            "decompress" => {
                let case = text_of(&row, "case");
                let body = base64_decode(&text_of(&row, "body_b64"));
                let content_type = row["content_type"].as_str();
                // Python distinguishes three outcomes; the port must land on
                // the same one. `OSError` is the bomb cap, `BadGzipFile` a bad
                // member, `EOFError` a truncated one.
                let expected = match row["error"].as_str() {
                    None => json!({"ok": row["out_b64"]}),
                    Some(name) => json!({"error": name}),
                };
                let actual = match decompress(&body, content_type) {
                    Ok(out) => {
                        json!({"ok": base64_encode(out.as_ref())})
                    }
                    Err(browser_templates::error::TemplateError::GzipTruncated) => {
                        json!({"error": "EOFError"})
                    }
                    Err(browser_templates::error::TemplateError::GzipBomb { .. }) => {
                        json!({"error": "OSError"})
                    }
                    Err(_) => json!({"error": "BadGzipFile"}),
                };
                tally.record(&format!("helpers/decompress/{case}"), &expected, &actual);
            }
            other => panic!("unknown helper kind {other:?}"),
        }
    }
    let (reproduced, total) = tally.finish("helpers");
    assert!(total >= 153, "the corpus shrank: {total} rows");
    assert!(reproduced > 0);
}

/// Standard-alphabet base64 with padding, matching Python's `b64encode`.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |byte| u32::from(*byte));
        let b2 = chunk.get(2).map_or(0, |byte| u32::from(*byte));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[((triple >> 18) & 0x3F) as usize]));
        out.push(char::from(ALPHABET[((triple >> 12) & 0x3F) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[((triple >> 6) & 0x3F) as usize]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(triple & 0x3F) as usize]));
        } else {
            out.push('=');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 2. LinkExtractor
// ---------------------------------------------------------------------------

#[test]
fn the_link_extractor_reproduces_python() {
    let mut tally = Tally::default();
    for row in load("u18_links.jsonl") {
        let kind = text_of(&row, "kind");
        let case = text_of(&row, "case");
        let extractor = extractor_from(&row["config"]);
        match kind.as_str() {
            "extract" => {
                let response = Response::builder(200, text_of(&row, "url"))
                    .body(text_of(&row, "html"))
                    .encoding("utf-8")
                    .build();
                // An `error` row means lxml raised. That is kept as a distinct
                // outcome rather than folded into "no links": the ten rows
                // where it happens are real, declared divergences.
                let expected = if row["error"].is_null() {
                    json!({ "urls": row["out"].clone() })
                } else {
                    json!({ "raised": row["error"].clone() })
                };
                let actual = json!({ "urls": extractor.extract(&response) });
                tally.record(&format!("links/extract/{case}"), &expected, &actual);
            }
            "matches" => {
                let url = text_of(&row, "url");
                // `matches` can raise: `canonicalize_url` and
                // `_url_extensions` both call CPython's `urlsplit`. That is a
                // *different* outcome from "the URL did not pass", and folding
                // the two together would hide a whole class of difference —
                // so the port's answer is computed unconditionally and the two
                // outcomes are compared as tagged values.
                let expected = if row["error"].is_null() {
                    json!({ "passes": row["out"].clone() })
                } else {
                    json!({ "raised": row["error"].clone() })
                };
                let actual = json!({ "passes": extractor.matches(&url) });
                tally.record(&format!("links/matches/{case}"), &expected, &actual);
            }
            other => panic!("unknown links kind {other:?}"),
        }
    }
    let (_, total) = tally.finish("links");
    assert!(total >= 378, "the corpus shrank: {total} rows");
}

// ---------------------------------------------------------------------------
// 3. SitemapSpider
// ---------------------------------------------------------------------------

#[test]
fn sitemap_bodies_reproduce_python() {
    let mut tally = Tally::default();
    for row in load("u18_sitemap.jsonl") {
        let case = text_of(&row, "case");
        let body = base64_decode(&text_of(&row, "body_b64"));
        let content_type = row["content_type"].as_str();
        let alternates = row["alternate_links"].as_bool().unwrap_or(false);

        // `_sm_body` catches `XMLSyntaxError` and `OSError` itself and returns
        // an empty result plus a warning, so a `null` `urls` in the corpus
        // means an exception escaped it entirely. That distinction is kept:
        // folding it into "empty result" would hide a whole class of
        // difference.
        let expected = if row["error"].is_null() {
            json!({"urls": row["urls"].clone(), "sitemaps": row["sitemaps"].clone()})
        } else {
            json!({"raised": row["error"].clone()})
        };
        let actual = match parse_sitemap(&body, content_type, alternates) {
            Ok(result) => json!({"urls": result.urls, "sitemaps": result.sitemaps}),
            // `parse_sitemap` hands its error up; `SitemapSpider` is the one
            // that warns and yields nothing, exactly as `_sm_body` does.
            Err(_) => json!({"urls": [], "sitemaps": []}),
        };
        tally.record(&format!("sitemap/{case}"), &expected, &actual);
    }
    let (_, total) = tally.finish("sitemap");
    assert!(total >= 48, "the corpus shrank: {total} rows");
}

// ---------------------------------------------------------------------------
// 4. ShopifySpider
// ---------------------------------------------------------------------------

/// Drop the key order and compare values, since the generator wrote the corpus
/// with `sort_keys=True`.
///
/// Key order is not lost as a property: it is pinned separately and exactly by
/// `shopify::tests::the_item_shape_is_pythons_exactly`.
fn sorted_items(items: &[Map<String, Value>]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| {
                let sorted: BTreeMap<&String, &Value> = item.iter().collect();
                serde_json::to_value(sorted).expect("an object")
            })
            .collect(),
    )
}

#[test]
fn shopify_items_reproduce_python() {
    let mut tally = Tally::default();
    for row in load("u18_shopify.jsonl") {
        let case = text_of(&row, "case");

        if case.starts_with("domain-") {
            let mut settings = browser_spider::spider::SpiderSettings::default();
            let mut builder = ShopifySpider::builder();
            if let Some(kwargs) = row["kwargs"].as_object() {
                for (key, value) in kwargs {
                    match key.as_str() {
                        "target_website" => {
                            builder = builder.target_website(value.as_str().unwrap_or_default());
                        }
                        "start_urls" => {
                            settings = settings.with_start_urls(
                                value
                                    .as_array()
                                    .map(|urls| {
                                        urls.iter()
                                            .map(|url| url.as_str().unwrap_or_default().to_owned())
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default(),
                            );
                        }
                        "allowed_domains" => {
                            settings = settings.with_allowed_domains(
                                value
                                    .as_array()
                                    .map(|domains| {
                                        domains
                                            .iter()
                                            .map(|domain| {
                                                domain.as_str().unwrap_or_default().to_owned()
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default(),
                            );
                        }
                        other => panic!("unknown Shopify kwarg {other:?}"),
                    }
                }
            }
            let expected = json!({
                "target": row["target_website"].clone(),
                "collections": row.get("collections_url").cloned().unwrap_or(Value::Null),
                "products": row.get("products_url").cloned().unwrap_or(Value::Null),
                "product": row.get("product_url").cloned().unwrap_or(Value::Null),
            });
            let actual = match builder.settings(settings).build() {
                Ok(spider) => json!({
                    "target": spider.target_website(),
                    "collections": spider.collections_url(1),
                    "products": spider.products_url("h", 2),
                    "product": spider.product_url("h", "p"),
                }),
                Err(_) => json!({
                    "target": Value::Null,
                    "collections": Value::Null,
                    "products": Value::Null,
                    "product": Value::Null,
                }),
            };
            tally.record(&format!("shopify/{case}"), &expected, &actual);
            continue;
        }

        let spider = ShopifySpider::builder()
            .target_website("example.com")
            .build()
            .expect("a target");

        if case == "dedup-across-collections" {
            let first = spider
                .process_product(&row["product"], "a")
                .expect("well-formed");
            let second = spider
                .process_product(&row["product"], "b")
                .expect("well-formed");
            let mut both = first.clone();
            both.extend(second.clone());
            let expected = json!({
                "items": row["items"].clone(),
                "first": row["first_len"].clone(),
                "second": row["second_len"].clone(),
            });
            let actual = json!({
                "items": sorted_items(&both),
                "first": first.len(),
                "second": second.len(),
            });
            tally.record(&format!("shopify/{case}"), &expected, &actual);
            continue;
        }

        let handle = text_of(&row, "handle");
        let expected = if row["error"].is_null() {
            row["items"].clone()
        } else {
            Value::Null
        };
        let actual = match spider.process_product(&row["product"], &handle) {
            Ok(items) => sorted_items(&items),
            Err(_) => Value::Null,
        };
        tally.record(&format!("shopify/{case}"), &expected, &actual);
    }
    let (_, total) = tally.finish("shopify");
    assert!(total >= 49, "the corpus shrank: {total} rows");
}

// ---------------------------------------------------------------------------
// 5. csv.DictReader
// ---------------------------------------------------------------------------

#[test]
fn csv_rows_reproduce_dictreader() {
    let mut tally = Tally::default();
    for row in load("u18_csv.jsonl") {
        let case = text_of(&row, "case");
        let text = text_of(&row, "text");
        let delimiter = text_of(&row, "delimiter").bytes().next().unwrap_or(b',');
        let quote = text_of(&row, "quotechar").bytes().next().unwrap_or(b'"');

        let mut builder = CSVFeedSpider::builder("corpus")
            .delimiter(delimiter)
            .quote(quote);
        if let Some(headers) = row["headers"].as_array() {
            builder = builder.headers(
                headers
                    .iter()
                    .map(|name| name.as_str().unwrap_or_default().to_owned())
                    .collect::<Vec<_>>(),
            );
        }
        let spider = builder.build();

        let actual = match spider.read_rows(&text) {
            Ok(rows) => Value::Array(
                rows.into_iter()
                    .map(|row| {
                        // The corpus wrote the restkey as `""`; make the
                        // comparison key-order-insensitive, as `sort_keys=True`
                        // already made the expected side.
                        let sorted: BTreeMap<String, Value> = row.into_iter().collect();
                        serde_json::to_value(sorted).expect("an object")
                    })
                    .collect(),
            ),
            Err(_) => Value::Null,
        };
        // A `null` `rows` means `csv.DictReader` raised — a lone `\r` in an
        // unquoted field. That stays a distinct outcome.
        let expected = match row["rows"].as_array() {
            None => json!({ "raised": row["error"].clone() }),
            Some(entries) => Value::Array(
                entries
                    .iter()
                    .map(|entry| {
                        let sorted: BTreeMap<&String, &Value> =
                            entry.as_object().expect("an object").iter().collect();
                        serde_json::to_value(sorted).expect("an object")
                    })
                    .collect(),
            ),
        };
        tally.record(&format!("csv/{case}"), &expected, &actual);
    }
    let (_, total) = tally.finish("csv");
    assert!(total >= 26, "the corpus shrank: {total} rows");
    // The sentinel really is what the corpus assumes.
    assert_eq!(CSV_RESTKEY, "");
}

// ---------------------------------------------------------------------------
// 6. SiteToMarkdownSpider filenames
// ---------------------------------------------------------------------------

#[cfg(feature = "markdown")]
#[test]
fn output_filenames_reproduce_python() {
    use browser_spider::spider::SpiderSettings;
    use browser_templates::site_to_markdown::SiteToMarkdownSpider;

    let mut tally = Tally::default();
    for row in load("u18_filename.jsonl") {
        let case = text_of(&row, "case");
        let spider = SiteToMarkdownSpider::builder("corpus")
            .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
            .build()
            .expect("allowed_domains is set");
        // An entry that is an object rather than a string is a URL CPython's
        // `urlparse` refused; the port is total and answers with a name.
        let names: Vec<Value> = row["urls"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|url| json!(spider.filename_for(url.as_str().unwrap_or_default())))
            .collect();
        tally.record(
            &format!("filename/{case}"),
            &row["names"],
            &Value::Array(names),
        );
    }
    let (_, total) = tally.finish("filename");
    assert!(total >= 16, "the corpus shrank: {total} rows");
}

// ---------------------------------------------------------------------------
// The corpus itself
// ---------------------------------------------------------------------------

/// The corpus files exist and none of them has been trimmed.
#[test]
fn the_corpus_is_intact() {
    let expected: &[(&str, usize)] = &[
        ("u18_helpers.jsonl", 153),
        ("u18_links.jsonl", 378),
        ("u18_sitemap.jsonl", 48),
        ("u18_shopify.jsonl", 49),
        ("u18_csv.jsonl", 26),
        ("u18_filename.jsonl", 16),
    ];
    for (name, rows) in expected {
        assert_eq!(
            load(name).len(),
            *rows,
            "{name} has been resized; regenerate it with tools/corpus/u18/generate.py \
             rather than editing it, and never shrink it to raise a pass rate"
        );
    }
}
