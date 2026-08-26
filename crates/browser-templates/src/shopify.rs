//! [`ShopifySpider`], with a hand-written [`python_title`] and a deterministic
//! domain fallback.
//!
//! A port of `reference/spiders/templates/shopify.py` (`spider-support.md`
//! §10). Every Shopify store exposes `/collections.json` and
//! `/collections/{handle}/products.json`, so the whole catalogue can be walked
//! without touching a single page of HTML. The spider pages through the
//! collections, then pages through each collection's products, and emits one
//! item per **variant**.
//!
//! ```
//! use browser_templates::shopify::ShopifySpider;
//!
//! let spider = ShopifySpider::builder().target_website("example.com").build()?;
//! assert_eq!(spider.target_website(), "example.com");
//! assert_eq!(
//!     spider.collections_url(1),
//!     "https://example.com/collections.json?page=1&limit=250"
//! );
//! # Ok::<(), browser_templates::error::TemplateError>(())
//! ```

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use browser_spider::canonicalize::netloc_of;
use browser_spider::request::{CallbackId, Item, Request};
use browser_spider::spider::{
    CallbackStream, CrawlError, CrawlResponse, RequestStream, Spider, SpiderSettings, Yielded,
};
use regex::Regex;
use serde_json::Value;

use crate::error::{Result, TemplateError};

/// The named callback for a `products.json` page.
///
/// `parse` handles the `collections.json` pages; this one handles each
/// collection's products.
pub const PARSE_COLLECTION: CallbackId = CallbackId::from_static("parse_collection");

/// Shopify's per-page cap. Python's URL templates hard-code it.
pub const PAGE_LIMIT: u32 = 250;

/// The spider's default name — Python's `name = "shopify"`.
pub const DEFAULT_NAME: &str = "shopify";

// ---------------------------------------------------------------------------
// The two w3lib/CPython helpers
// ---------------------------------------------------------------------------

/// `w3lib.html.remove_tags(text)` with no `which_ones` and no `keep`.
///
/// The regex is w3lib's, verbatim: `</?([^ >/]+).*?>` with `DOTALL` and
/// `IGNORECASE`, every match replaced by `""`. Entities are **not** decoded —
/// `&amp;` survives, which is why `"a &lt; b"` comes back unchanged.
///
/// ```
/// use browser_templates::shopify::remove_tags;
///
/// assert_eq!(remove_tags("<p>A <b>matte</b> lipstick</p>"), "A matte lipstick");
/// assert_eq!(remove_tags("a < b"), "a < b", "a lone `<` is not a tag");
/// assert_eq!(remove_tags("<!-- c -->x"), "x");
/// ```
#[must_use]
pub fn remove_tags(text: &str) -> String {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        Regex::new(r"(?is)</?([^ >/]+).*?>").expect("w3lib's tag pattern is valid")
    });
    regex.replace_all(text, "").into_owned()
}

/// Code points whose **full** title-case mapping is not their full upper-case
/// mapping — the table Rust's standard library does not ship.
///
/// `ß` is the one that matters in practice: title-casing it as the *first*
/// character of a run gives `"Ss"`, not `to_uppercase`'s `"SS"`. The rest are
/// the four Latin digraph triples (`Ǆ ǅ ǆ`, …), the Latin and Armenian
/// ligatures (`ﬀ` → `"Ff"`, `և` → `"Եւ"`), the Greek ypogegrammeni block, and
/// Georgian Mkhedruli, which has an upper-case mapping but no title-case one
/// at all.
///
/// Sorted by code point, so [`python_title`] binary-searches it. Generated from
/// CPython 3.14 by `tools/corpus/u18/gen_title_tables.py`.
#[rustfmt::skip]
const TITLECASE_EXCEPTIONS: [(char, &str); 135] = [
    ('\u{df}', "Ss"), ('\u{1c4}', "\u{1c5}"), ('\u{1c5}', "\u{1c5}"), ('\u{1c6}', "\u{1c5}"),
    ('\u{1c7}', "\u{1c8}"), ('\u{1c8}', "\u{1c8}"), ('\u{1c9}', "\u{1c8}"), ('\u{1ca}', "\u{1cb}"),
    ('\u{1cb}', "\u{1cb}"), ('\u{1cc}', "\u{1cb}"), ('\u{1f1}', "\u{1f2}"), ('\u{1f2}', "\u{1f2}"),
    ('\u{1f3}', "\u{1f2}"), ('\u{587}', "\u{535}\u{582}"), ('\u{10d0}', "\u{10d0}"), ('\u{10d1}', "\u{10d1}"),
    ('\u{10d2}', "\u{10d2}"), ('\u{10d3}', "\u{10d3}"), ('\u{10d4}', "\u{10d4}"), ('\u{10d5}', "\u{10d5}"),
    ('\u{10d6}', "\u{10d6}"), ('\u{10d7}', "\u{10d7}"), ('\u{10d8}', "\u{10d8}"), ('\u{10d9}', "\u{10d9}"),
    ('\u{10da}', "\u{10da}"), ('\u{10db}', "\u{10db}"), ('\u{10dc}', "\u{10dc}"), ('\u{10dd}', "\u{10dd}"),
    ('\u{10de}', "\u{10de}"), ('\u{10df}', "\u{10df}"), ('\u{10e0}', "\u{10e0}"), ('\u{10e1}', "\u{10e1}"),
    ('\u{10e2}', "\u{10e2}"), ('\u{10e3}', "\u{10e3}"), ('\u{10e4}', "\u{10e4}"), ('\u{10e5}', "\u{10e5}"),
    ('\u{10e6}', "\u{10e6}"), ('\u{10e7}', "\u{10e7}"), ('\u{10e8}', "\u{10e8}"), ('\u{10e9}', "\u{10e9}"),
    ('\u{10ea}', "\u{10ea}"), ('\u{10eb}', "\u{10eb}"), ('\u{10ec}', "\u{10ec}"), ('\u{10ed}', "\u{10ed}"),
    ('\u{10ee}', "\u{10ee}"), ('\u{10ef}', "\u{10ef}"), ('\u{10f0}', "\u{10f0}"), ('\u{10f1}', "\u{10f1}"),
    ('\u{10f2}', "\u{10f2}"), ('\u{10f3}', "\u{10f3}"), ('\u{10f4}', "\u{10f4}"), ('\u{10f5}', "\u{10f5}"),
    ('\u{10f6}', "\u{10f6}"), ('\u{10f7}', "\u{10f7}"), ('\u{10f8}', "\u{10f8}"), ('\u{10f9}', "\u{10f9}"),
    ('\u{10fa}', "\u{10fa}"), ('\u{10fd}', "\u{10fd}"), ('\u{10fe}', "\u{10fe}"), ('\u{10ff}', "\u{10ff}"),
    ('\u{1f80}', "\u{1f88}"), ('\u{1f81}', "\u{1f89}"), ('\u{1f82}', "\u{1f8a}"), ('\u{1f83}', "\u{1f8b}"),
    ('\u{1f84}', "\u{1f8c}"), ('\u{1f85}', "\u{1f8d}"), ('\u{1f86}', "\u{1f8e}"), ('\u{1f87}', "\u{1f8f}"),
    ('\u{1f88}', "\u{1f88}"), ('\u{1f89}', "\u{1f89}"), ('\u{1f8a}', "\u{1f8a}"), ('\u{1f8b}', "\u{1f8b}"),
    ('\u{1f8c}', "\u{1f8c}"), ('\u{1f8d}', "\u{1f8d}"), ('\u{1f8e}', "\u{1f8e}"), ('\u{1f8f}', "\u{1f8f}"),
    ('\u{1f90}', "\u{1f98}"), ('\u{1f91}', "\u{1f99}"), ('\u{1f92}', "\u{1f9a}"), ('\u{1f93}', "\u{1f9b}"),
    ('\u{1f94}', "\u{1f9c}"), ('\u{1f95}', "\u{1f9d}"), ('\u{1f96}', "\u{1f9e}"), ('\u{1f97}', "\u{1f9f}"),
    ('\u{1f98}', "\u{1f98}"), ('\u{1f99}', "\u{1f99}"), ('\u{1f9a}', "\u{1f9a}"), ('\u{1f9b}', "\u{1f9b}"),
    ('\u{1f9c}', "\u{1f9c}"), ('\u{1f9d}', "\u{1f9d}"), ('\u{1f9e}', "\u{1f9e}"), ('\u{1f9f}', "\u{1f9f}"),
    ('\u{1fa0}', "\u{1fa8}"), ('\u{1fa1}', "\u{1fa9}"), ('\u{1fa2}', "\u{1faa}"), ('\u{1fa3}', "\u{1fab}"),
    ('\u{1fa4}', "\u{1fac}"), ('\u{1fa5}', "\u{1fad}"), ('\u{1fa6}', "\u{1fae}"), ('\u{1fa7}', "\u{1faf}"),
    ('\u{1fa8}', "\u{1fa8}"), ('\u{1fa9}', "\u{1fa9}"), ('\u{1faa}', "\u{1faa}"), ('\u{1fab}', "\u{1fab}"),
    ('\u{1fac}', "\u{1fac}"), ('\u{1fad}', "\u{1fad}"), ('\u{1fae}', "\u{1fae}"), ('\u{1faf}', "\u{1faf}"),
    ('\u{1fb2}', "\u{1fba}\u{345}"), ('\u{1fb3}', "\u{1fbc}"), ('\u{1fb4}', "\u{386}\u{345}"), ('\u{1fb7}', "\u{391}\u{342}\u{345}"),
    ('\u{1fbc}', "\u{1fbc}"), ('\u{1fc2}', "\u{1fca}\u{345}"), ('\u{1fc3}', "\u{1fcc}"), ('\u{1fc4}', "\u{389}\u{345}"),
    ('\u{1fc7}', "\u{397}\u{342}\u{345}"), ('\u{1fcc}', "\u{1fcc}"), ('\u{1ff2}', "\u{1ffa}\u{345}"), ('\u{1ff3}', "\u{1ffc}"),
    ('\u{1ff4}', "\u{38f}\u{345}"), ('\u{1ff7}', "\u{3a9}\u{342}\u{345}"), ('\u{1ffc}', "\u{1ffc}"), ('\u{fb00}', "Ff"),
    ('\u{fb01}', "Fi"), ('\u{fb02}', "Fl"), ('\u{fb03}', "Ffi"), ('\u{fb04}', "Ffl"),
    ('\u{fb05}', "St"), ('\u{fb06}', "St"), ('\u{fb13}', "\u{544}\u{576}"), ('\u{fb14}', "\u{544}\u{565}"),
    ('\u{fb15}', "\u{544}\u{56b}"), ('\u{fb16}', "\u{54e}\u{576}"), ('\u{fb17}', "\u{544}\u{56d}"),
];

/// CPython's `_PyUnicode_IsCased`, as sorted inclusive code-point ranges.
///
/// Not `char::is_lowercase() || char::is_uppercase()`: that misses the whole
/// title-case category (`Lt`), so `"ǅungla"` would have its `u` upper-cased
/// where CPython leaves the run unbroken. Kept as ranges rather than as a delta
/// against the standard library's predicates so the table is pinned to
/// CPython's Unicode version instead of to whichever version the Rust standard
/// library happens to ship — the two are 87 code points apart today.
///
/// Generated from CPython 3.14 by `tools/corpus/u18/gen_title_tables.py`.
#[rustfmt::skip]
const CASED_RANGES: [(char, char); 159] = [
    ('\u{41}', '\u{5a}'), ('\u{61}', '\u{7a}'), ('\u{aa}', '\u{aa}'), ('\u{b5}', '\u{b5}'), ('\u{ba}', '\u{ba}'),
    ('\u{c0}', '\u{d6}'), ('\u{d8}', '\u{f6}'), ('\u{f8}', '\u{1ba}'), ('\u{1bc}', '\u{1bf}'), ('\u{1c4}', '\u{293}'),
    ('\u{295}', '\u{2b8}'), ('\u{2c0}', '\u{2c1}'), ('\u{2e0}', '\u{2e4}'), ('\u{345}', '\u{345}'), ('\u{370}', '\u{373}'),
    ('\u{376}', '\u{377}'), ('\u{37a}', '\u{37d}'), ('\u{37f}', '\u{37f}'), ('\u{386}', '\u{386}'), ('\u{388}', '\u{38a}'),
    ('\u{38c}', '\u{38c}'), ('\u{38e}', '\u{3a1}'), ('\u{3a3}', '\u{3f5}'), ('\u{3f7}', '\u{481}'), ('\u{48a}', '\u{52f}'),
    ('\u{531}', '\u{556}'), ('\u{560}', '\u{588}'), ('\u{10a0}', '\u{10c5}'), ('\u{10c7}', '\u{10c7}'), ('\u{10cd}', '\u{10cd}'),
    ('\u{10d0}', '\u{10fa}'), ('\u{10fc}', '\u{10ff}'), ('\u{13a0}', '\u{13f5}'), ('\u{13f8}', '\u{13fd}'), ('\u{1c80}', '\u{1c8a}'),
    ('\u{1c90}', '\u{1cba}'), ('\u{1cbd}', '\u{1cbf}'), ('\u{1d00}', '\u{1dbf}'), ('\u{1e00}', '\u{1f15}'), ('\u{1f18}', '\u{1f1d}'),
    ('\u{1f20}', '\u{1f45}'), ('\u{1f48}', '\u{1f4d}'), ('\u{1f50}', '\u{1f57}'), ('\u{1f59}', '\u{1f59}'), ('\u{1f5b}', '\u{1f5b}'),
    ('\u{1f5d}', '\u{1f5d}'), ('\u{1f5f}', '\u{1f7d}'), ('\u{1f80}', '\u{1fb4}'), ('\u{1fb6}', '\u{1fbc}'), ('\u{1fbe}', '\u{1fbe}'),
    ('\u{1fc2}', '\u{1fc4}'), ('\u{1fc6}', '\u{1fcc}'), ('\u{1fd0}', '\u{1fd3}'), ('\u{1fd6}', '\u{1fdb}'), ('\u{1fe0}', '\u{1fec}'),
    ('\u{1ff2}', '\u{1ff4}'), ('\u{1ff6}', '\u{1ffc}'), ('\u{2071}', '\u{2071}'), ('\u{207f}', '\u{207f}'), ('\u{2090}', '\u{209c}'),
    ('\u{2102}', '\u{2102}'), ('\u{2107}', '\u{2107}'), ('\u{210a}', '\u{2113}'), ('\u{2115}', '\u{2115}'), ('\u{2119}', '\u{211d}'),
    ('\u{2124}', '\u{2124}'), ('\u{2126}', '\u{2126}'), ('\u{2128}', '\u{2128}'), ('\u{212a}', '\u{212d}'), ('\u{212f}', '\u{2134}'),
    ('\u{2139}', '\u{2139}'), ('\u{213c}', '\u{213f}'), ('\u{2145}', '\u{2149}'), ('\u{214e}', '\u{214e}'), ('\u{2160}', '\u{217f}'),
    ('\u{2183}', '\u{2184}'), ('\u{24b6}', '\u{24e9}'), ('\u{2c00}', '\u{2ce4}'), ('\u{2ceb}', '\u{2cee}'), ('\u{2cf2}', '\u{2cf3}'),
    ('\u{2d00}', '\u{2d25}'), ('\u{2d27}', '\u{2d27}'), ('\u{2d2d}', '\u{2d2d}'), ('\u{a640}', '\u{a66d}'), ('\u{a680}', '\u{a69d}'),
    ('\u{a722}', '\u{a787}'), ('\u{a78b}', '\u{a78e}'), ('\u{a790}', '\u{a7cd}'), ('\u{a7d0}', '\u{a7d1}'), ('\u{a7d3}', '\u{a7d3}'),
    ('\u{a7d5}', '\u{a7dc}'), ('\u{a7f2}', '\u{a7f6}'), ('\u{a7f8}', '\u{a7fa}'), ('\u{ab30}', '\u{ab5a}'), ('\u{ab5c}', '\u{ab69}'),
    ('\u{ab70}', '\u{abbf}'), ('\u{fb00}', '\u{fb06}'), ('\u{fb13}', '\u{fb17}'), ('\u{ff21}', '\u{ff3a}'), ('\u{ff41}', '\u{ff5a}'),
    ('\u{10400}', '\u{1044f}'), ('\u{104b0}', '\u{104d3}'), ('\u{104d8}', '\u{104fb}'), ('\u{10570}', '\u{1057a}'), ('\u{1057c}', '\u{1058a}'),
    ('\u{1058c}', '\u{10592}'), ('\u{10594}', '\u{10595}'), ('\u{10597}', '\u{105a1}'), ('\u{105a3}', '\u{105b1}'), ('\u{105b3}', '\u{105b9}'),
    ('\u{105bb}', '\u{105bc}'), ('\u{10780}', '\u{10780}'), ('\u{10783}', '\u{10785}'), ('\u{10787}', '\u{107b0}'), ('\u{107b2}', '\u{107ba}'),
    ('\u{10c80}', '\u{10cb2}'), ('\u{10cc0}', '\u{10cf2}'), ('\u{10d50}', '\u{10d65}'), ('\u{10d70}', '\u{10d85}'), ('\u{118a0}', '\u{118df}'),
    ('\u{16e40}', '\u{16e7f}'), ('\u{1d400}', '\u{1d454}'), ('\u{1d456}', '\u{1d49c}'), ('\u{1d49e}', '\u{1d49f}'), ('\u{1d4a2}', '\u{1d4a2}'),
    ('\u{1d4a5}', '\u{1d4a6}'), ('\u{1d4a9}', '\u{1d4ac}'), ('\u{1d4ae}', '\u{1d4b9}'), ('\u{1d4bb}', '\u{1d4bb}'), ('\u{1d4bd}', '\u{1d4c3}'),
    ('\u{1d4c5}', '\u{1d505}'), ('\u{1d507}', '\u{1d50a}'), ('\u{1d50d}', '\u{1d514}'), ('\u{1d516}', '\u{1d51c}'), ('\u{1d51e}', '\u{1d539}'),
    ('\u{1d53b}', '\u{1d53e}'), ('\u{1d540}', '\u{1d544}'), ('\u{1d546}', '\u{1d546}'), ('\u{1d54a}', '\u{1d550}'), ('\u{1d552}', '\u{1d6a5}'),
    ('\u{1d6a8}', '\u{1d6c0}'), ('\u{1d6c2}', '\u{1d6da}'), ('\u{1d6dc}', '\u{1d6fa}'), ('\u{1d6fc}', '\u{1d714}'), ('\u{1d716}', '\u{1d734}'),
    ('\u{1d736}', '\u{1d74e}'), ('\u{1d750}', '\u{1d76e}'), ('\u{1d770}', '\u{1d788}'), ('\u{1d78a}', '\u{1d7a8}'), ('\u{1d7aa}', '\u{1d7c2}'),
    ('\u{1d7c4}', '\u{1d7cb}'), ('\u{1df00}', '\u{1df09}'), ('\u{1df0b}', '\u{1df1e}'), ('\u{1df25}', '\u{1df2a}'), ('\u{1e030}', '\u{1e06d}'),
    ('\u{1e900}', '\u{1e943}'), ('\u{1f130}', '\u{1f149}'), ('\u{1f150}', '\u{1f169}'), ('\u{1f170}', '\u{1f189}'),
];

/// CPython's `_PyUnicode_IsCaseIgnorable`, as sorted inclusive code-point
/// ranges.
///
/// Needed only by the Final_Sigma rule below, which scans past case-ignorable
/// characters — combining marks, apostrophes, modifier letters — looking for
/// the next cased one. Generated from CPython 3.14 by
/// `tools/corpus/u18/gen_title_tables.py`.
#[rustfmt::skip]
const CASE_IGNORABLE_RANGES: [(char, char); 452] = [
    ('\u{27}', '\u{27}'), ('\u{2e}', '\u{2e}'), ('\u{3a}', '\u{3a}'), ('\u{5e}', '\u{5e}'), ('\u{60}', '\u{60}'),
    ('\u{a8}', '\u{a8}'), ('\u{ad}', '\u{ad}'), ('\u{af}', '\u{af}'), ('\u{b4}', '\u{b4}'), ('\u{b7}', '\u{b8}'),
    ('\u{2b0}', '\u{36f}'), ('\u{374}', '\u{375}'), ('\u{37a}', '\u{37a}'), ('\u{384}', '\u{385}'), ('\u{387}', '\u{387}'),
    ('\u{483}', '\u{489}'), ('\u{559}', '\u{559}'), ('\u{55f}', '\u{55f}'), ('\u{591}', '\u{5bd}'), ('\u{5bf}', '\u{5bf}'),
    ('\u{5c1}', '\u{5c2}'), ('\u{5c4}', '\u{5c5}'), ('\u{5c7}', '\u{5c7}'), ('\u{5f4}', '\u{5f4}'), ('\u{600}', '\u{605}'),
    ('\u{610}', '\u{61a}'), ('\u{61c}', '\u{61c}'), ('\u{640}', '\u{640}'), ('\u{64b}', '\u{65f}'), ('\u{670}', '\u{670}'),
    ('\u{6d6}', '\u{6dd}'), ('\u{6df}', '\u{6e8}'), ('\u{6ea}', '\u{6ed}'), ('\u{70f}', '\u{70f}'), ('\u{711}', '\u{711}'),
    ('\u{730}', '\u{74a}'), ('\u{7a6}', '\u{7b0}'), ('\u{7eb}', '\u{7f5}'), ('\u{7fa}', '\u{7fa}'), ('\u{7fd}', '\u{7fd}'),
    ('\u{816}', '\u{82d}'), ('\u{859}', '\u{85b}'), ('\u{888}', '\u{888}'), ('\u{890}', '\u{891}'), ('\u{897}', '\u{89f}'),
    ('\u{8c9}', '\u{902}'), ('\u{93a}', '\u{93a}'), ('\u{93c}', '\u{93c}'), ('\u{941}', '\u{948}'), ('\u{94d}', '\u{94d}'),
    ('\u{951}', '\u{957}'), ('\u{962}', '\u{963}'), ('\u{971}', '\u{971}'), ('\u{981}', '\u{981}'), ('\u{9bc}', '\u{9bc}'),
    ('\u{9c1}', '\u{9c4}'), ('\u{9cd}', '\u{9cd}'), ('\u{9e2}', '\u{9e3}'), ('\u{9fe}', '\u{9fe}'), ('\u{a01}', '\u{a02}'),
    ('\u{a3c}', '\u{a3c}'), ('\u{a41}', '\u{a42}'), ('\u{a47}', '\u{a48}'), ('\u{a4b}', '\u{a4d}'), ('\u{a51}', '\u{a51}'),
    ('\u{a70}', '\u{a71}'), ('\u{a75}', '\u{a75}'), ('\u{a81}', '\u{a82}'), ('\u{abc}', '\u{abc}'), ('\u{ac1}', '\u{ac5}'),
    ('\u{ac7}', '\u{ac8}'), ('\u{acd}', '\u{acd}'), ('\u{ae2}', '\u{ae3}'), ('\u{afa}', '\u{aff}'), ('\u{b01}', '\u{b01}'),
    ('\u{b3c}', '\u{b3c}'), ('\u{b3f}', '\u{b3f}'), ('\u{b41}', '\u{b44}'), ('\u{b4d}', '\u{b4d}'), ('\u{b55}', '\u{b56}'),
    ('\u{b62}', '\u{b63}'), ('\u{b82}', '\u{b82}'), ('\u{bc0}', '\u{bc0}'), ('\u{bcd}', '\u{bcd}'), ('\u{c00}', '\u{c00}'),
    ('\u{c04}', '\u{c04}'), ('\u{c3c}', '\u{c3c}'), ('\u{c3e}', '\u{c40}'), ('\u{c46}', '\u{c48}'), ('\u{c4a}', '\u{c4d}'),
    ('\u{c55}', '\u{c56}'), ('\u{c62}', '\u{c63}'), ('\u{c81}', '\u{c81}'), ('\u{cbc}', '\u{cbc}'), ('\u{cbf}', '\u{cbf}'),
    ('\u{cc6}', '\u{cc6}'), ('\u{ccc}', '\u{ccd}'), ('\u{ce2}', '\u{ce3}'), ('\u{d00}', '\u{d01}'), ('\u{d3b}', '\u{d3c}'),
    ('\u{d41}', '\u{d44}'), ('\u{d4d}', '\u{d4d}'), ('\u{d62}', '\u{d63}'), ('\u{d81}', '\u{d81}'), ('\u{dca}', '\u{dca}'),
    ('\u{dd2}', '\u{dd4}'), ('\u{dd6}', '\u{dd6}'), ('\u{e31}', '\u{e31}'), ('\u{e34}', '\u{e3a}'), ('\u{e46}', '\u{e4e}'),
    ('\u{eb1}', '\u{eb1}'), ('\u{eb4}', '\u{ebc}'), ('\u{ec6}', '\u{ec6}'), ('\u{ec8}', '\u{ece}'), ('\u{f18}', '\u{f19}'),
    ('\u{f35}', '\u{f35}'), ('\u{f37}', '\u{f37}'), ('\u{f39}', '\u{f39}'), ('\u{f71}', '\u{f7e}'), ('\u{f80}', '\u{f84}'),
    ('\u{f86}', '\u{f87}'), ('\u{f8d}', '\u{f97}'), ('\u{f99}', '\u{fbc}'), ('\u{fc6}', '\u{fc6}'), ('\u{102d}', '\u{1030}'),
    ('\u{1032}', '\u{1037}'), ('\u{1039}', '\u{103a}'), ('\u{103d}', '\u{103e}'), ('\u{1058}', '\u{1059}'), ('\u{105e}', '\u{1060}'),
    ('\u{1071}', '\u{1074}'), ('\u{1082}', '\u{1082}'), ('\u{1085}', '\u{1086}'), ('\u{108d}', '\u{108d}'), ('\u{109d}', '\u{109d}'),
    ('\u{10fc}', '\u{10fc}'), ('\u{135d}', '\u{135f}'), ('\u{1712}', '\u{1714}'), ('\u{1732}', '\u{1733}'), ('\u{1752}', '\u{1753}'),
    ('\u{1772}', '\u{1773}'), ('\u{17b4}', '\u{17b5}'), ('\u{17b7}', '\u{17bd}'), ('\u{17c6}', '\u{17c6}'), ('\u{17c9}', '\u{17d3}'),
    ('\u{17d7}', '\u{17d7}'), ('\u{17dd}', '\u{17dd}'), ('\u{180b}', '\u{180f}'), ('\u{1843}', '\u{1843}'), ('\u{1885}', '\u{1886}'),
    ('\u{18a9}', '\u{18a9}'), ('\u{1920}', '\u{1922}'), ('\u{1927}', '\u{1928}'), ('\u{1932}', '\u{1932}'), ('\u{1939}', '\u{193b}'),
    ('\u{1a17}', '\u{1a18}'), ('\u{1a1b}', '\u{1a1b}'), ('\u{1a56}', '\u{1a56}'), ('\u{1a58}', '\u{1a5e}'), ('\u{1a60}', '\u{1a60}'),
    ('\u{1a62}', '\u{1a62}'), ('\u{1a65}', '\u{1a6c}'), ('\u{1a73}', '\u{1a7c}'), ('\u{1a7f}', '\u{1a7f}'), ('\u{1aa7}', '\u{1aa7}'),
    ('\u{1ab0}', '\u{1ace}'), ('\u{1b00}', '\u{1b03}'), ('\u{1b34}', '\u{1b34}'), ('\u{1b36}', '\u{1b3a}'), ('\u{1b3c}', '\u{1b3c}'),
    ('\u{1b42}', '\u{1b42}'), ('\u{1b6b}', '\u{1b73}'), ('\u{1b80}', '\u{1b81}'), ('\u{1ba2}', '\u{1ba5}'), ('\u{1ba8}', '\u{1ba9}'),
    ('\u{1bab}', '\u{1bad}'), ('\u{1be6}', '\u{1be6}'), ('\u{1be8}', '\u{1be9}'), ('\u{1bed}', '\u{1bed}'), ('\u{1bef}', '\u{1bf1}'),
    ('\u{1c2c}', '\u{1c33}'), ('\u{1c36}', '\u{1c37}'), ('\u{1c78}', '\u{1c7d}'), ('\u{1cd0}', '\u{1cd2}'), ('\u{1cd4}', '\u{1ce0}'),
    ('\u{1ce2}', '\u{1ce8}'), ('\u{1ced}', '\u{1ced}'), ('\u{1cf4}', '\u{1cf4}'), ('\u{1cf8}', '\u{1cf9}'), ('\u{1d2c}', '\u{1d6a}'),
    ('\u{1d78}', '\u{1d78}'), ('\u{1d9b}', '\u{1dff}'), ('\u{1fbd}', '\u{1fbd}'), ('\u{1fbf}', '\u{1fc1}'), ('\u{1fcd}', '\u{1fcf}'),
    ('\u{1fdd}', '\u{1fdf}'), ('\u{1fed}', '\u{1fef}'), ('\u{1ffd}', '\u{1ffe}'), ('\u{200b}', '\u{200f}'), ('\u{2018}', '\u{2019}'),
    ('\u{2024}', '\u{2024}'), ('\u{2027}', '\u{2027}'), ('\u{202a}', '\u{202e}'), ('\u{2060}', '\u{2064}'), ('\u{2066}', '\u{206f}'),
    ('\u{2071}', '\u{2071}'), ('\u{207f}', '\u{207f}'), ('\u{2090}', '\u{209c}'), ('\u{20d0}', '\u{20f0}'), ('\u{2c7c}', '\u{2c7d}'),
    ('\u{2cef}', '\u{2cf1}'), ('\u{2d6f}', '\u{2d6f}'), ('\u{2d7f}', '\u{2d7f}'), ('\u{2de0}', '\u{2dff}'), ('\u{2e2f}', '\u{2e2f}'),
    ('\u{3005}', '\u{3005}'), ('\u{302a}', '\u{302d}'), ('\u{3031}', '\u{3035}'), ('\u{303b}', '\u{303b}'), ('\u{3099}', '\u{309e}'),
    ('\u{30fc}', '\u{30fe}'), ('\u{a015}', '\u{a015}'), ('\u{a4f8}', '\u{a4fd}'), ('\u{a60c}', '\u{a60c}'), ('\u{a66f}', '\u{a672}'),
    ('\u{a674}', '\u{a67d}'), ('\u{a67f}', '\u{a67f}'), ('\u{a69c}', '\u{a69f}'), ('\u{a6f0}', '\u{a6f1}'), ('\u{a700}', '\u{a721}'),
    ('\u{a770}', '\u{a770}'), ('\u{a788}', '\u{a78a}'), ('\u{a7f2}', '\u{a7f4}'), ('\u{a7f8}', '\u{a7f9}'), ('\u{a802}', '\u{a802}'),
    ('\u{a806}', '\u{a806}'), ('\u{a80b}', '\u{a80b}'), ('\u{a825}', '\u{a826}'), ('\u{a82c}', '\u{a82c}'), ('\u{a8c4}', '\u{a8c5}'),
    ('\u{a8e0}', '\u{a8f1}'), ('\u{a8ff}', '\u{a8ff}'), ('\u{a926}', '\u{a92d}'), ('\u{a947}', '\u{a951}'), ('\u{a980}', '\u{a982}'),
    ('\u{a9b3}', '\u{a9b3}'), ('\u{a9b6}', '\u{a9b9}'), ('\u{a9bc}', '\u{a9bd}'), ('\u{a9cf}', '\u{a9cf}'), ('\u{a9e5}', '\u{a9e6}'),
    ('\u{aa29}', '\u{aa2e}'), ('\u{aa31}', '\u{aa32}'), ('\u{aa35}', '\u{aa36}'), ('\u{aa43}', '\u{aa43}'), ('\u{aa4c}', '\u{aa4c}'),
    ('\u{aa70}', '\u{aa70}'), ('\u{aa7c}', '\u{aa7c}'), ('\u{aab0}', '\u{aab0}'), ('\u{aab2}', '\u{aab4}'), ('\u{aab7}', '\u{aab8}'),
    ('\u{aabe}', '\u{aabf}'), ('\u{aac1}', '\u{aac1}'), ('\u{aadd}', '\u{aadd}'), ('\u{aaec}', '\u{aaed}'), ('\u{aaf3}', '\u{aaf4}'),
    ('\u{aaf6}', '\u{aaf6}'), ('\u{ab5b}', '\u{ab5f}'), ('\u{ab69}', '\u{ab6b}'), ('\u{abe5}', '\u{abe5}'), ('\u{abe8}', '\u{abe8}'),
    ('\u{abed}', '\u{abed}'), ('\u{fb1e}', '\u{fb1e}'), ('\u{fbb2}', '\u{fbc2}'), ('\u{fe00}', '\u{fe0f}'), ('\u{fe13}', '\u{fe13}'),
    ('\u{fe20}', '\u{fe2f}'), ('\u{fe52}', '\u{fe52}'), ('\u{fe55}', '\u{fe55}'), ('\u{feff}', '\u{feff}'), ('\u{ff07}', '\u{ff07}'),
    ('\u{ff0e}', '\u{ff0e}'), ('\u{ff1a}', '\u{ff1a}'), ('\u{ff3e}', '\u{ff3e}'), ('\u{ff40}', '\u{ff40}'), ('\u{ff70}', '\u{ff70}'),
    ('\u{ff9e}', '\u{ff9f}'), ('\u{ffe3}', '\u{ffe3}'), ('\u{fff9}', '\u{fffb}'), ('\u{101fd}', '\u{101fd}'), ('\u{102e0}', '\u{102e0}'),
    ('\u{10376}', '\u{1037a}'), ('\u{10780}', '\u{10785}'), ('\u{10787}', '\u{107b0}'), ('\u{107b2}', '\u{107ba}'), ('\u{10a01}', '\u{10a03}'),
    ('\u{10a05}', '\u{10a06}'), ('\u{10a0c}', '\u{10a0f}'), ('\u{10a38}', '\u{10a3a}'), ('\u{10a3f}', '\u{10a3f}'), ('\u{10ae5}', '\u{10ae6}'),
    ('\u{10d24}', '\u{10d27}'), ('\u{10d4e}', '\u{10d4e}'), ('\u{10d69}', '\u{10d6d}'), ('\u{10d6f}', '\u{10d6f}'), ('\u{10eab}', '\u{10eac}'),
    ('\u{10efc}', '\u{10eff}'), ('\u{10f46}', '\u{10f50}'), ('\u{10f82}', '\u{10f85}'), ('\u{11001}', '\u{11001}'), ('\u{11038}', '\u{11046}'),
    ('\u{11070}', '\u{11070}'), ('\u{11073}', '\u{11074}'), ('\u{1107f}', '\u{11081}'), ('\u{110b3}', '\u{110b6}'), ('\u{110b9}', '\u{110ba}'),
    ('\u{110bd}', '\u{110bd}'), ('\u{110c2}', '\u{110c2}'), ('\u{110cd}', '\u{110cd}'), ('\u{11100}', '\u{11102}'), ('\u{11127}', '\u{1112b}'),
    ('\u{1112d}', '\u{11134}'), ('\u{11173}', '\u{11173}'), ('\u{11180}', '\u{11181}'), ('\u{111b6}', '\u{111be}'), ('\u{111c9}', '\u{111cc}'),
    ('\u{111cf}', '\u{111cf}'), ('\u{1122f}', '\u{11231}'), ('\u{11234}', '\u{11234}'), ('\u{11236}', '\u{11237}'), ('\u{1123e}', '\u{1123e}'),
    ('\u{11241}', '\u{11241}'), ('\u{112df}', '\u{112df}'), ('\u{112e3}', '\u{112ea}'), ('\u{11300}', '\u{11301}'), ('\u{1133b}', '\u{1133c}'),
    ('\u{11340}', '\u{11340}'), ('\u{11366}', '\u{1136c}'), ('\u{11370}', '\u{11374}'), ('\u{113bb}', '\u{113c0}'), ('\u{113ce}', '\u{113ce}'),
    ('\u{113d0}', '\u{113d0}'), ('\u{113d2}', '\u{113d2}'), ('\u{113e1}', '\u{113e2}'), ('\u{11438}', '\u{1143f}'), ('\u{11442}', '\u{11444}'),
    ('\u{11446}', '\u{11446}'), ('\u{1145e}', '\u{1145e}'), ('\u{114b3}', '\u{114b8}'), ('\u{114ba}', '\u{114ba}'), ('\u{114bf}', '\u{114c0}'),
    ('\u{114c2}', '\u{114c3}'), ('\u{115b2}', '\u{115b5}'), ('\u{115bc}', '\u{115bd}'), ('\u{115bf}', '\u{115c0}'), ('\u{115dc}', '\u{115dd}'),
    ('\u{11633}', '\u{1163a}'), ('\u{1163d}', '\u{1163d}'), ('\u{1163f}', '\u{11640}'), ('\u{116ab}', '\u{116ab}'), ('\u{116ad}', '\u{116ad}'),
    ('\u{116b0}', '\u{116b5}'), ('\u{116b7}', '\u{116b7}'), ('\u{1171d}', '\u{1171d}'), ('\u{1171f}', '\u{1171f}'), ('\u{11722}', '\u{11725}'),
    ('\u{11727}', '\u{1172b}'), ('\u{1182f}', '\u{11837}'), ('\u{11839}', '\u{1183a}'), ('\u{1193b}', '\u{1193c}'), ('\u{1193e}', '\u{1193e}'),
    ('\u{11943}', '\u{11943}'), ('\u{119d4}', '\u{119d7}'), ('\u{119da}', '\u{119db}'), ('\u{119e0}', '\u{119e0}'), ('\u{11a01}', '\u{11a0a}'),
    ('\u{11a33}', '\u{11a38}'), ('\u{11a3b}', '\u{11a3e}'), ('\u{11a47}', '\u{11a47}'), ('\u{11a51}', '\u{11a56}'), ('\u{11a59}', '\u{11a5b}'),
    ('\u{11a8a}', '\u{11a96}'), ('\u{11a98}', '\u{11a99}'), ('\u{11c30}', '\u{11c36}'), ('\u{11c38}', '\u{11c3d}'), ('\u{11c3f}', '\u{11c3f}'),
    ('\u{11c92}', '\u{11ca7}'), ('\u{11caa}', '\u{11cb0}'), ('\u{11cb2}', '\u{11cb3}'), ('\u{11cb5}', '\u{11cb6}'), ('\u{11d31}', '\u{11d36}'),
    ('\u{11d3a}', '\u{11d3a}'), ('\u{11d3c}', '\u{11d3d}'), ('\u{11d3f}', '\u{11d45}'), ('\u{11d47}', '\u{11d47}'), ('\u{11d90}', '\u{11d91}'),
    ('\u{11d95}', '\u{11d95}'), ('\u{11d97}', '\u{11d97}'), ('\u{11ef3}', '\u{11ef4}'), ('\u{11f00}', '\u{11f01}'), ('\u{11f36}', '\u{11f3a}'),
    ('\u{11f40}', '\u{11f40}'), ('\u{11f42}', '\u{11f42}'), ('\u{11f5a}', '\u{11f5a}'), ('\u{13430}', '\u{13440}'), ('\u{13447}', '\u{13455}'),
    ('\u{1611e}', '\u{16129}'), ('\u{1612d}', '\u{1612f}'), ('\u{16af0}', '\u{16af4}'), ('\u{16b30}', '\u{16b36}'), ('\u{16b40}', '\u{16b43}'),
    ('\u{16d40}', '\u{16d42}'), ('\u{16d6b}', '\u{16d6c}'), ('\u{16f4f}', '\u{16f4f}'), ('\u{16f8f}', '\u{16f9f}'), ('\u{16fe0}', '\u{16fe1}'),
    ('\u{16fe3}', '\u{16fe4}'), ('\u{1aff0}', '\u{1aff3}'), ('\u{1aff5}', '\u{1affb}'), ('\u{1affd}', '\u{1affe}'), ('\u{1bc9d}', '\u{1bc9e}'),
    ('\u{1bca0}', '\u{1bca3}'), ('\u{1cf00}', '\u{1cf2d}'), ('\u{1cf30}', '\u{1cf46}'), ('\u{1d167}', '\u{1d169}'), ('\u{1d173}', '\u{1d182}'),
    ('\u{1d185}', '\u{1d18b}'), ('\u{1d1aa}', '\u{1d1ad}'), ('\u{1d242}', '\u{1d244}'), ('\u{1da00}', '\u{1da36}'), ('\u{1da3b}', '\u{1da6c}'),
    ('\u{1da75}', '\u{1da75}'), ('\u{1da84}', '\u{1da84}'), ('\u{1da9b}', '\u{1da9f}'), ('\u{1daa1}', '\u{1daaf}'), ('\u{1e000}', '\u{1e006}'),
    ('\u{1e008}', '\u{1e018}'), ('\u{1e01b}', '\u{1e021}'), ('\u{1e023}', '\u{1e024}'), ('\u{1e026}', '\u{1e02a}'), ('\u{1e030}', '\u{1e06d}'),
    ('\u{1e08f}', '\u{1e08f}'), ('\u{1e130}', '\u{1e13d}'), ('\u{1e2ae}', '\u{1e2ae}'), ('\u{1e2ec}', '\u{1e2ef}'), ('\u{1e4eb}', '\u{1e4ef}'),
    ('\u{1e5ee}', '\u{1e5ef}'), ('\u{1e8d0}', '\u{1e8d6}'), ('\u{1e944}', '\u{1e94b}'), ('\u{1f3fb}', '\u{1f3ff}'), ('\u{e0001}', '\u{e0001}'),
    ('\u{e0020}', '\u{e007f}'), ('\u{e0100}', '\u{e01ef}'),
];

/// `_PyUnicode_ToTitleFull(c)`, written into `out`.
fn push_titlecase(character: char, out: &mut String) {
    match TITLECASE_EXCEPTIONS.binary_search_by_key(&character, |(key, _)| *key) {
        Ok(index) => out.push_str(TITLECASE_EXCEPTIONS[index].1),
        // Everywhere else the title-case mapping *is* the upper-case one.
        Err(_) => out.extend(character.to_uppercase()),
    }
}

/// Is `character` inside one of `table`'s sorted, inclusive ranges?
fn in_ranges(table: &[(char, char)], character: char) -> bool {
    table
        .binary_search_by(|(low, high)| {
            if character < *low {
                std::cmp::Ordering::Greater
            } else if character > *high {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `_PyUnicode_IsCased(c)` — Unicode `Lowercase`, `Uppercase` or category `Lt`.
#[must_use]
pub fn is_cased(character: char) -> bool {
    in_ranges(&CASED_RANGES, character)
}

/// `_PyUnicode_IsCaseIgnorable(c)`.
#[must_use]
pub fn is_case_ignorable(character: char) -> bool {
    in_ranges(&CASE_IGNORABLE_RANGES, character)
}

/// CPython's `handle_capital_sigma`.
///
/// Greek `Σ` lower-cases to the final form `ς` when it ends a word — CPython's
/// own comment spells the context out as
///
/// ```text
/// \p{cased} \p{case-ignorable}* U+03A3 !( \p{case-ignorable}* \p{cased} )
/// ```
///
/// so both halves scan outwards past case-ignorable characters and then ask
/// whether what they land on is cased. The backward half is **not** implied by
/// `do_title` having decided the previous character was cased: a character can
/// be cased *and* case-ignorable at once — U+0345 COMBINING GREEK
/// YPOGEGRAMMENI is both — in which case the scan walks off the front of the
/// string and the sigma is not final.
///
/// Without this, the Greek for "Athens Street" would come back with a medial
/// `σ` where CPython writes `ς`. The u18 fuzz found it on 46 of 7,002 strings,
/// and the U+0345 case on 1 more after the first fix.
fn lower_capital_sigma(before: &str, after: &str) -> char {
    let preceded_by_cased = before
        .chars()
        .rev()
        .find(|previous| !is_case_ignorable(*previous))
        .is_some_and(is_cased);
    let followed_by_cased = after
        .chars()
        .find(|next| !is_case_ignorable(*next))
        .is_some_and(is_cased);
    if preceded_by_cased && !followed_by_cased {
        '\u{3c2}'
    } else {
        '\u{3c3}'
    }
}

/// CPython's `str.title()`.
///
/// Title-cases the first cased character of every run and lower-cases the rest,
/// where a run is broken by any character that is not itself cased. A digit is
/// not cased, which is why `"3d-printing"` becomes `"3D Printing"` and not
/// `"3d Printing"`.
///
/// This is `do_title` in CPython's `unicodeobject.c`, transcribed:
///
/// ```text
/// previous_is_cased = 0
/// for c in s:
///     out += _PyUnicode_ToTitleFull(c) if not previous_is_cased else lower(c)
///     previous_is_cased = _PyUnicode_IsCased(c)   # the *original* c
/// ```
///
/// where `lower` is `_PyUnicode_ToLowerFull` except for `Σ`, whose Final_Sigma
/// context is resolved by `lower_capital_sigma`. The three functions Rust's
/// standard library lacks — `_PyUnicode_ToTitleFull`, `_PyUnicode_IsCased` and
/// `_PyUnicode_IsCaseIgnorable` — come from the generated tables above. What is
/// left is a Unicode-version skew: CPython 3.14 and
/// `rustc` 1.97 disagree about the case mapping of 28 code points and about
/// whether 5 others are cased at all (`꟎`, `꟏`, `꟒`, `꟔`, `꟱`, plus the
/// Medefaidrin block, where `rustc` is ahead of CPython). None is reachable
/// from a Shopify collection handle. `tools/corpus/u18/` pins the agreement
/// over 7,002 fuzzed strings and every code point of six Unicode blocks.
///
/// ```
/// use browser_templates::shopify::python_title;
///
/// assert_eq!(python_title("summer sale"), "Summer Sale");
/// assert_eq!(python_title("o'neil"), "O'Neil");
/// assert_eq!(python_title("ALL CAPS"), "All Caps");
/// assert_eq!(python_title("3d printing"), "3D Printing");
/// // `to_uppercase` alone would say "SSeta" here, and "ǄUngla" there.
/// assert_eq!(python_title("ßeta"), "Sseta");
/// assert_eq!(python_title("ǅungla"), "ǅungla");
/// // Final sigma, which `char::to_lowercase` does not know about.
/// assert_eq!(python_title("ΟΔΟΣ ΑΘΗΝΑΣ"), "Οδος Αθηνας");
/// ```
#[must_use]
pub fn python_title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_is_cased = false;
    for (offset, character) in text.char_indices() {
        if previous_is_cased {
            if character == '\u{3a3}' {
                // `lower_ucs4`'s "obscure special case".
                out.push(lower_capital_sigma(
                    &text[..offset],
                    &text[offset + character.len_utf8()..],
                ));
            } else {
                out.extend(character.to_lowercase());
            }
        } else {
            push_titlecase(character, &mut out);
        }
        previous_is_cased = is_cased(character);
    }
    out
}

/// CPython's truthiness, for the JSON values Shopify hands back.
///
/// `null`, `false`, `0`, `""`, `[]` and `{}` are falsy; everything else is
/// truthy. `"0.00"` is a **non-empty string** and therefore truthy, which is
/// exactly why `old_price` needs the second `float(...)` test.
#[must_use]
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `float(value)` for a JSON value, the way Python would spell it.
///
/// `float(True)` is `1.0` — a `bool` **is** an `int` in Python, so a
/// `compare_at_price` of `true` survives the `and float(old_price)` guard and
/// is emitted as-is rather than raising. The u18 fuzz found the port erroring
/// on it.
///
/// `None` means Python would raise: `ValueError` for a string that is not a
/// number, `TypeError` for a list or a dict. Both escape `_process_product`,
/// and both come back here as [`TemplateError::ShopifyJson`].
///
/// The string branch is CPython's, not Rust's: every string-to-number
/// conversion in Python runs `_PyUnicode_TransformDecimalAndSpaceToASCII`
/// first, so `float("٣.٥")` is `3.5`, `float("1_0")` is `10.0`, and the
/// surrounding whitespace it strips is `str.isspace()`'s (which covers
/// `\x1c`–`\x1f`), not `str::trim`'s. This was a declared divergence until
/// wave 6 made [`browser_spider::throttle::python_float`] public — it is
/// unit 16's port, verified against CPython 3.14, and there is no reason for a
/// second copy of it.
fn to_float(value: &Value) -> Option<f64> {
    match value {
        // `float(True) == 1.0`; `False` never reaches here, being falsy.
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        Value::String(text) => browser_spider::throttle::python_float(text),
        _ => None,
    }
}

/// A `set` membership key for a variant id, with Python's equality.
///
/// `self.collected_ids` is a Python `set`, where `1`, `1.0` and `True` are one
/// element and `"1"` is a different one — numbers compare across their types
/// and a `str` never equals a number. Keying on the JSON rendering would make
/// `1` and `1.0` two variants and emit a duplicate item; the u18 fuzz found it
/// on 63 of 1200 generated products.
///
/// # Errors
///
/// A list or a dict is unhashable, so `variant["id"] in self.collected_ids`
/// raises `TypeError` in Python. That is surfaced rather than swallowed.
fn variant_key(id: &Value) -> Result<String> {
    Ok(match id {
        Value::Null => "None".to_owned(),
        // `True == 1` and `False == 0`, and they hash alike.
        Value::Bool(flag) => format!("#{}", i64::from(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(value) => format!("#{value}"),
            None => match number.as_u64() {
                Some(value) => format!("#{value}"),
                None => {
                    let value = number.as_f64().unwrap_or(f64::NAN);
                    // An integral float is the same set element as the int.
                    if value == value.trunc() && value.abs() < 9.0e15 {
                        format!("#{}", value as i64)
                    } else {
                        format!("#{value}")
                    }
                }
            },
        },
        Value::String(text) => format!("s{text}"),
        Value::Array(_) | Value::Object(_) => {
            return Err(TemplateError::ShopifyJson {
                message: format!("unhashable type in variant 'id': {id}"),
            })
        }
    })
}

// ---------------------------------------------------------------------------
// The spider
// ---------------------------------------------------------------------------

/// A spider that extracts every product of a Shopify store through its JSON
/// API.
///
/// Point it at the store's domain — with [`ShopifySpiderBuilder::target_website`],
/// or by leaving that unset and giving `start_urls` or `allowed_domains` in the
/// settings — and it walks `/collections.json`, then each collection's
/// `products.json`, yielding one item per variant.
///
/// Variant de-duplication is **per spider instance**: a variant that appears in
/// two collections is emitted once.
pub struct ShopifySpider {
    name: String,
    settings: SpiderSettings,
    target_website: String,
    collected_ids: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for ShopifySpider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShopifySpider")
            .field("name", &self.name)
            .field("target_website", &self.target_website)
            .field(
                "collected_ids",
                &self.collected_ids.lock().map_or(0, |ids| ids.len()),
            )
            .finish()
    }
}

impl ShopifySpider {
    /// Start building one.
    #[must_use]
    pub fn builder() -> ShopifySpiderBuilder {
        ShopifySpiderBuilder::new()
    }

    /// The store's netloc.
    #[must_use]
    pub fn target_website(&self) -> &str {
        &self.target_website
    }

    /// `collections_url.format(website=…, page=…)`.
    #[must_use]
    pub fn collections_url(&self, page: u64) -> String {
        format!(
            "https://{}/collections.json?page={page}&limit={PAGE_LIMIT}",
            self.target_website
        )
    }

    /// `products_url.format(website=…, handle=…, page=…)`.
    #[must_use]
    pub fn products_url(&self, handle: &str, page: u64) -> String {
        format!(
            "https://{}/collections/{handle}/products.json?page={page}&limit={PAGE_LIMIT}",
            self.target_website
        )
    }

    /// `product_url.format(website=…, handle=…, product_handle=…)`.
    #[must_use]
    pub fn product_url(&self, handle: &str, product_handle: &str) -> String {
        format!(
            "https://{}/collections/{handle}/products/{product_handle}",
            self.target_website
        )
    }

    /// One item per not-yet-seen variant of `product`.
    ///
    /// The exact dict `spider-support.md` §10.6 pins, key order included.
    ///
    /// # Errors
    ///
    /// [`TemplateError::ShopifyJson`] where Python raises `KeyError` (a missing
    /// `variants`, `title`, `vendor`, `images` or `handle`) or `ValueError` (a
    /// non-numeric `compare_at_price`). Python lets both propagate into the
    /// engine's callback error handler; so does [`ShopifySpider::parse`], by
    /// yielding [`Yielded::error`].
    pub fn process_product(&self, product: &Value, collection_handle: &str) -> Result<Vec<Item>> {
        let variants = product
            .get("variants")
            .and_then(Value::as_array)
            .ok_or_else(|| TemplateError::ShopifyJson {
                message: "'variants'".to_owned(),
            })?;
        let title = require_str(product, "title")?;
        let vendor = require(product, "vendor")?.clone();
        let handle = require_str(product, "handle")?;
        let images = require(product, "images")?;
        let image_url = images
            .as_array()
            .and_then(|images| images.first())
            .and_then(|image| image.get("src"))
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        let description = Value::String(remove_tags(
            product
                .get("body_html")
                .filter(|value| !value.is_null())
                .and_then(Value::as_str)
                .unwrap_or(""),
        ));
        let category = Value::String(
            python_title(&collection_handle.replace('-', " "))
                .trim()
                .to_owned(),
        );
        let url = Value::String(self.product_url(collection_handle, handle));

        let mut out = Vec::new();
        for variant in variants {
            let id = require(variant, "id")?;
            {
                let key = variant_key(id)?;
                let mut seen = self.collected_ids.lock().expect("not poisoned");
                if !seen.insert(key) {
                    continue;
                }
            }

            let variant_title = require_str(variant, "title")?;
            let name = if variant_title == "Default Title" {
                title.to_owned()
            } else {
                format!("{title} - {variant_title}")
            };

            let raw_old_price = variant
                .get("compare_at_price")
                .cloned()
                .unwrap_or(Value::Null);
            let old_price = if is_truthy(&raw_old_price) {
                match to_float(&raw_old_price) {
                    Some(value) if value != 0.0 => raw_old_price,
                    Some(_) => Value::String(String::new()),
                    None => {
                        return Err(TemplateError::ShopifyJson {
                            message: format!(
                                "could not convert string to float: {}",
                                raw_old_price
                            ),
                        })
                    }
                }
            } else {
                Value::String(String::new())
            };

            let mut item = Item::new();
            item.insert("name".to_owned(), Value::String(name));
            item.insert("price".to_owned(), require(variant, "price")?.clone());
            item.insert("category".to_owned(), category.clone());
            item.insert("brand".to_owned(), vendor.clone());
            item.insert("identifier".to_owned(), id.clone());
            item.insert("sku".to_owned(), or_empty_string(variant.get("sku")));
            // Never a real quantity: `None` when available, `0` when not.
            item.insert(
                "stock".to_owned(),
                if variant.get("available").is_some_and(is_truthy) {
                    Value::Null
                } else {
                    Value::from(0)
                },
            );
            item.insert("image_url".to_owned(), image_url.clone());
            item.insert("url".to_owned(), url.clone());
            item.insert("description".to_owned(), description.clone());
            item.insert("old_price".to_owned(), old_price);
            item.insert(
                "barcode".to_owned(),
                or_empty_string(variant.get("barcode")),
            );
            out.push(item);
        }
        Ok(out)
    }

    /// `parse`: a `collections.json` page.
    fn parse_yields(&self, response: &CrawlResponse) -> Vec<Yielded> {
        let body = match response.json() {
            Ok(body) => body,
            Err(error) => {
                return vec![Yielded::error(TemplateError::ShopifyJson {
                    message: error.to_string(),
                })]
            }
        };
        let Some(collections) = body.get("collections").and_then(Value::as_array) else {
            return vec![Yielded::error(TemplateError::ShopifyJson {
                message: "'collections'".to_owned(),
            })];
        };
        // `if collections:` — an empty list stops the pagination.
        if collections.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        for collection in collections {
            let Some(handle) = collection.get("handle").and_then(Value::as_str) else {
                out.push(Yielded::error(TemplateError::ShopifyJson {
                    message: "'handle'".to_owned(),
                }));
                continue;
            };
            let Some(count) = collection.get("products_count") else {
                out.push(Yielded::error(TemplateError::ShopifyJson {
                    message: "'products_count'".to_owned(),
                }));
                continue;
            };
            if !is_truthy(count) {
                continue;
            }
            let mut meta = Item::new();
            meta.insert("handle".to_owned(), Value::String(handle.to_owned()));
            meta.insert("page".to_owned(), Value::from(1));
            out.push(Yielded::request(
                Request::new(self.products_url(handle, 1))
                    .with_callback(PARSE_COLLECTION)
                    .with_meta(meta),
            ));
        }

        let next_page = meta_page(response) + 1;
        let mut meta = Item::new();
        meta.insert("page".to_owned(), Value::from(next_page));
        out.push(Yielded::request(
            Request::new(self.collections_url(next_page))
                .with_callback(CallbackId::PARSE)
                .with_meta(meta),
        ));
        out
    }

    /// `parse_collection`: a `products.json` page.
    fn parse_collection_yields(&self, response: &CrawlResponse) -> Vec<Yielded> {
        let Some(handle) = response.meta.get("handle").and_then(Value::as_str) else {
            return vec![Yielded::error(TemplateError::ShopifyJson {
                message: "'handle'".to_owned(),
            })];
        };
        let handle = handle.to_owned();
        let current_page = meta_page(response);

        let body = match response.json() {
            Ok(body) => body,
            Err(error) => {
                return vec![Yielded::error(TemplateError::ShopifyJson {
                    message: error.to_string(),
                })]
            }
        };
        let Some(products) = body.get("products").and_then(Value::as_array) else {
            return vec![Yielded::error(TemplateError::ShopifyJson {
                message: "'products'".to_owned(),
            })];
        };
        if products.is_empty() {
            tracing::debug!("Extracted all products from collection {handle}");
            return Vec::new();
        }

        let mut out = Vec::new();
        for product in products {
            match self.process_product(product, &handle) {
                Ok(items) => out.extend(items.into_iter().map(Yielded::item)),
                // Python raises out of the generator, so nothing after the bad
                // product is yielded either.
                Err(error) => {
                    out.push(Yielded::error(error));
                    return out;
                }
            }
        }

        let next_page = current_page + 1;
        let mut meta = Item::new();
        meta.insert("handle".to_owned(), Value::String(handle.clone()));
        meta.insert("page".to_owned(), Value::from(next_page));
        out.push(Yielded::request(
            Request::new(self.products_url(&handle, next_page))
                .with_callback(PARSE_COLLECTION)
                .with_meta(meta),
        ));
        out
    }
}

/// `response.meta.get("page", 1)`.
fn meta_page(response: &CrawlResponse) -> u64 {
    response
        .meta
        .get("page")
        .and_then(Value::as_u64)
        .unwrap_or(1)
}

/// `product[key]`, or Python's `KeyError`.
fn require<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value.get(key).ok_or_else(|| TemplateError::ShopifyJson {
        message: format!("'{key}'"),
    })
}

/// `product[key]` as a string.
fn require_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    require(value, key)?
        .as_str()
        .ok_or_else(|| TemplateError::ShopifyJson {
            message: format!("'{key}' is not a string"),
        })
}

/// Python's `variant.get(key) or ""`.
fn or_empty_string(value: Option<&Value>) -> Value {
    match value {
        Some(value) if is_truthy(value) => value.clone(),
        _ => Value::String(String::new()),
    }
}

#[async_trait]
impl Spider for ShopifySpider {
    fn name(&self) -> &str {
        &self.name
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    async fn start_requests(self: Arc<Self>) -> std::result::Result<RequestStream, CrawlError> {
        let mut meta = Item::new();
        meta.insert("page".to_owned(), Value::from(1));
        let request = Request::new(self.collections_url(1))
            .with_callback(CallbackId::PARSE)
            .with_meta(meta);
        Ok(Box::pin(futures::stream::iter([request])))
    }

    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
        let yielded = self.parse_yields(&response);
        Box::pin(futures::stream::iter(yielded))
    }

    async fn dispatch(
        self: Arc<Self>,
        callback: CallbackId,
        response: CrawlResponse,
    ) -> CallbackStream {
        if callback.as_str() == PARSE_COLLECTION.as_str() {
            let yielded = self.parse_collection_yields(&response);
            return Box::pin(futures::stream::iter(yielded));
        }
        self.parse(response).await
    }

    fn has_callback(&self, callback: &CallbackId) -> bool {
        callback.as_str() == CallbackId::PARSE.as_str()
            || callback.as_str() == PARSE_COLLECTION.as_str()
    }
}

/// Builder for [`ShopifySpider`].
#[derive(Clone, Debug, Default)]
pub struct ShopifySpiderBuilder {
    name: Option<String>,
    settings: SpiderSettings,
    target_website: Option<String>,
}

impl ShopifySpiderBuilder {
    /// A builder with Python's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the spider's name. Defaults to [`DEFAULT_NAME`].
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Every `Spider` class attribute. `start_urls` and `allowed_domains` are
    /// also the second and third fallbacks for the store's domain.
    #[must_use]
    pub fn settings(mut self, settings: SpiderSettings) -> Self {
        self.settings = settings;
        self
    }

    /// `target_website=` — a bare domain or a full URL.
    #[must_use]
    pub fn target_website(mut self, website: impl Into<String>) -> Self {
        self.target_website = Some(website.into());
        self
    }

    /// Resolve the store's domain and freeze the spider.
    ///
    /// Resolution order is Python's: `target_website`, then the **first**
    /// `start_urls` entry, then `allowed_domains`. A bare domain is prefixed
    /// with `https://` before its netloc is taken.
    ///
    /// **Declared divergence:** Python's third fallback is
    /// `next(iter(self.allowed_domains), "")` over a `set`, whose iteration
    /// order is unspecified. [`SpiderSettings::allowed_domains`] is a
    /// `BTreeSet`, so the port takes the lexicographically first entry —
    /// deterministic where Python is not (`spider-support.md` §14.6 item 7).
    ///
    /// # Errors
    ///
    /// [`TemplateError::MissingTargetWebsite`] when all three are empty.
    pub fn build(self) -> Result<ShopifySpider> {
        let name = self.name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
        let source = self
            .target_website
            .filter(|website| !website.is_empty())
            .or_else(|| self.settings.start_urls.first().cloned())
            .or_else(|| self.settings.allowed_domains.iter().next().cloned())
            .filter(|source| !source.is_empty())
            .ok_or_else(|| TemplateError::MissingTargetWebsite {
                spider: name.clone(),
            })?;

        let target_website = if source.contains("://") {
            netloc_of(&source)
        } else {
            netloc_of(&format!("https://{source}"))
        };

        Ok(ShopifySpider {
            name,
            settings: self.settings,
            target_website,
            collected_ids: Mutex::new(HashSet::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use browser_spider::session::FetchedResponse;
    use futures::StreamExt;
    use serde_json::json;

    use super::*;

    /// `to_float` is CPython's `float()`, not Rust's. Verified against the
    /// reference interpreter:
    ///
    /// ```text
    /// >>> float("٣.٥"), float("1_0"), float("\xa04.5"), float(True)
    /// (3.5, 10.0, 4.5, 1.0)
    /// >>> float(" 4.5\x1c")
    /// ValueError: could not convert string to float: ' 4.5\x1c'
    /// ```
    ///
    /// The first three were a declared divergence until wave 6 made
    /// `browser_spider::throttle::python_float` public.
    #[test]
    fn to_float_is_cpythons_float_not_rusts() {
        assert_eq!(to_float(&json!("٣.٥")), Some(3.5));
        assert_eq!(to_float(&json!("1_0")), Some(10.0));
        // Non-ASCII whitespace is folded to a space and stripped…
        assert_eq!(to_float(&json!("\u{a0}4.5")), Some(4.5));
        // …but a C0 separator is ASCII, so `float()` refuses it even though
        // `str.strip()` would have removed it.
        assert_eq!(to_float(&json!(" 4.5\u{1c}")), None);
        // A bool is an int in Python, so `float(True)` is 1.0 and never raises.
        assert_eq!(to_float(&json!(true)), Some(1.0));
        assert_eq!(to_float(&json!("not a number")), None);
        assert_eq!(to_float(&json!([1])), None);
    }

    fn spider() -> ShopifySpider {
        ShopifySpider::builder()
            .target_website("example.com")
            .build()
            .expect("a target")
    }

    fn response(body: Value, meta: &[(&str, Value)]) -> CrawlResponse {
        let mut fetched = FetchedResponse::new(
            200,
            "https://example.com/collections.json",
            body.to_string(),
        );
        for (key, value) in meta {
            fetched.meta.insert((*key).to_owned(), value.clone());
        }
        fetched.attach_request(&Request::new("https://example.com/collections.json").with_sid("s"));
        CrawlResponse::from_fetched(&fetched, &Default::default(), None)
    }

    fn product() -> Value {
        json!({
            "title": "Matte Lipstick",
            "handle": "matte-lipstick",
            "vendor": "Acme",
            "body_html": "<p>A <b>matte</b> lipstick</p>",
            "images": [{"src": "https://cdn.test/1.png"}],
            "variants": [
                {"id": 1, "title": "Red", "price": "12.00", "sku": "R1",
                 "available": true, "compare_at_price": "15.00", "barcode": "111"},
                {"id": 2, "title": "Blue", "price": "13.00", "sku": null,
                 "available": false, "compare_at_price": null, "barcode": null}
            ]
        })
    }

    // -- the helpers -------------------------------------------------------

    #[test]
    fn remove_tags_matches_w3lib() {
        assert_eq!(
            remove_tags("<p>A <b>matte</b> lipstick</p>"),
            "A matte lipstick"
        );
        assert_eq!(remove_tags(""), "");
        assert_eq!(remove_tags("a < b"), "a < b");
        assert_eq!(remove_tags("a > b"), "a > b");
        assert_eq!(remove_tags("<br/>x<BR />y"), "xy");
        assert_eq!(remove_tags("<p\nclass='x'>hi</p>"), "hi");
        assert_eq!(remove_tags("<!-- c -->x"), "x");
        assert_eq!(remove_tags("<div>a</div><div>b</div>"), "ab");
        assert_eq!(
            remove_tags("&amp; stays"),
            "&amp; stays",
            "entities are not decoded"
        );
    }

    #[test]
    fn python_title_matches_cpython() {
        assert_eq!(python_title("summer sale"), "Summer Sale");
        assert_eq!(python_title("o'neil"), "O'Neil");
        assert_eq!(python_title("a1b"), "A1B");
        assert_eq!(python_title("ALL CAPS"), "All Caps");
        assert_eq!(python_title("mixed CASE items"), "Mixed Case Items");
        assert_eq!(python_title(""), "");
        assert_eq!(python_title("3d printing"), "3D Printing");
    }

    /// The three CPython functions Rust's standard library does not have.
    ///
    /// Every expectation here was read out of the reference interpreter, not
    /// reasoned about: `to_uppercase` alone answers `"SSeta"`, `"ǄUngla"`,
    /// `"FI"` and a medial `σ`.
    #[test]
    fn python_title_uses_title_case_not_upper_case() {
        // `_PyUnicode_ToTitleFull` — ß, the digraphs and the ligatures.
        assert_eq!(python_title("ßeta"), "Sseta");
        assert_eq!(python_title("straße sale"), "Straße Sale");
        assert_eq!(python_title("ﬁle"), "File");
        assert_eq!(python_title("ﬄ x"), "Ffl X");
        assert_eq!(python_title("և x"), "Եւ X");
        assert_eq!(python_title("Ǆ ǅ ǆ"), "ǅ ǅ ǅ");
        // `_PyUnicode_IsCased` — U+01C5 is category `Lt`, which
        // `is_lowercase() || is_uppercase()` misses, so the run is unbroken.
        assert_eq!(python_title("ǅungla"), "ǅungla");
        assert!(is_cased('ǅ'));
        assert!(is_cased('a') && is_cased('A'));
        assert!(!is_cased('1') && !is_cased('-'));
        // Georgian Mkhedruli has an upper-case mapping but no title-case one.
        assert_eq!(python_title("ა x"), "ა X");
    }

    /// `handle_capital_sigma`: `Σ` lower-cases to `ς` at the end of a word.
    #[test]
    fn a_word_final_capital_sigma_lower_cases_to_the_final_form() {
        assert_eq!(python_title("ΟΔΟΣ ΑΘΗΝΑΣ"), "Οδος Αθηνας");
        assert_eq!(python_title("ΣΣΣ"), "Σσς");
        assert_eq!(python_title("aΣb"), "Aσb");
        // U+0345 is cased *and* case-ignorable, so the backward scan walks off
        // the front of the string and the sigma is not final after all.
        assert_eq!(python_title("\u{345}\u{3a3} XY"), "Ισ Xy");
        assert!(is_case_ignorable('\u{345}') && is_cased('\u{345}'));
    }

    #[test]
    fn truthiness_is_pythons() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&json!("")));
        assert!(!is_truthy(&json!([])));
        assert!(!is_truthy(&json!({})));
        assert!(is_truthy(&json!("0.00")), "a non-empty string is truthy");
        assert!(is_truthy(&json!(1)));
        assert!(is_truthy(&json!(true)));
    }

    // -- target resolution -------------------------------------------------

    #[test]
    fn the_domain_resolves_in_pythons_order() {
        assert_eq!(spider().target_website(), "example.com");
        assert_eq!(
            ShopifySpider::builder()
                .target_website("https://example.com/collections/all")
                .build()
                .expect("ok")
                .target_website(),
            "example.com"
        );
        assert_eq!(
            ShopifySpider::builder()
                .settings(
                    SpiderSettings::default()
                        .with_start_urls(["https://www.example.com/collections/mens"])
                )
                .build()
                .expect("ok")
                .target_website(),
            "www.example.com"
        );
        assert_eq!(
            ShopifySpider::builder()
                .settings(SpiderSettings::default().with_allowed_domains(["example.com"]))
                .build()
                .expect("ok")
                .target_website(),
            "example.com"
        );
    }

    #[test]
    fn no_source_at_all_is_pythons_value_error() {
        let error = ShopifySpider::builder().build().expect_err("no source");
        assert_eq!(
            error.to_string(),
            "shopify must set `target_website`, `start_urls`, or `allowed_domains`"
        );
    }

    #[test]
    fn the_allowed_domains_fallback_is_deterministic() {
        // Python picks an arbitrary member of a set; a `BTreeSet` makes it the
        // lexicographically first, every time.
        let spider = ShopifySpider::builder()
            .settings(
                SpiderSettings::default().with_allowed_domains(["z.test", "a.test", "m.test"]),
            )
            .build()
            .expect("ok");
        assert_eq!(spider.target_website(), "a.test");
    }

    #[test]
    fn the_url_templates_carry_the_page_cap() {
        let spider = spider();
        assert_eq!(
            spider.collections_url(1),
            "https://example.com/collections.json?page=1&limit=250"
        );
        assert_eq!(
            spider.products_url("lipsticks", 2),
            "https://example.com/collections/lipsticks/products.json?page=2&limit=250"
        );
        assert_eq!(
            spider.product_url("lipsticks", "matte-lipstick"),
            "https://example.com/collections/lipsticks/products/matte-lipstick"
        );
    }

    // -- start_requests / parse --------------------------------------------

    #[tokio::test]
    async fn start_requests_asks_for_the_first_collections_page() {
        let requests: Vec<Request> = Arc::new(spider())
            .start_requests()
            .await
            .expect("seeded")
            .collect()
            .await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://example.com/collections.json?page=1&limit=250"
        );
        assert_eq!(requests[0].meta["page"], json!(1));
    }

    #[test]
    fn parse_skips_empty_collections_and_paginates() {
        let body = json!({"collections": [
            {"handle": "lipsticks", "products_count": 3},
            {"handle": "empty", "products_count": 0}
        ]});
        let yielded = spider().parse_yields(&response(body, &[("page", json!(1))]));
        assert_eq!(yielded.len(), 2);
        match (&yielded[0], &yielded[1]) {
            (Yielded::Request(first), Yielded::Request(second)) => {
                assert_eq!(
                    first.url,
                    "https://example.com/collections/lipsticks/products.json?page=1&limit=250"
                );
                assert_eq!(first.meta["handle"], json!("lipsticks"));
                assert_eq!(
                    second.url,
                    "https://example.com/collections.json?page=2&limit=250"
                );
                assert_eq!(second.meta["page"], json!(2));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_collections_list_stops_the_pagination() {
        let yielded = spider().parse_yields(&response(json!({"collections": []}), &[]));
        assert!(yielded.is_empty());
    }

    #[test]
    fn a_missing_collections_key_is_a_callback_error() {
        let yielded = spider().parse_yields(&response(json!({}), &[]));
        assert!(
            matches!(yielded.as_slice(), [Yielded::Error(_)]),
            "{yielded:?}"
        );
    }

    // -- parse_collection / process_product --------------------------------

    #[test]
    fn the_item_shape_is_pythons_exactly() {
        let items = spider()
            .process_product(&product(), "summer-sale")
            .expect("well-formed");
        assert_eq!(items.len(), 2);

        let keys: Vec<&str> = items[0].keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "name",
                "price",
                "category",
                "brand",
                "identifier",
                "sku",
                "stock",
                "image_url",
                "url",
                "description",
                "old_price",
                "barcode",
            ]
        );
        assert_eq!(items[0]["name"], json!("Matte Lipstick - Red"));
        assert_eq!(items[0]["price"], json!("12.00"));
        assert_eq!(items[0]["category"], json!("Summer Sale"));
        assert_eq!(items[0]["brand"], json!("Acme"));
        assert_eq!(items[0]["identifier"], json!(1));
        assert_eq!(items[0]["sku"], json!("R1"));
        assert_eq!(
            items[0]["stock"],
            Value::Null,
            "available => None, never a count"
        );
        assert_eq!(items[0]["image_url"], json!("https://cdn.test/1.png"));
        assert_eq!(
            items[0]["url"],
            json!("https://example.com/collections/summer-sale/products/matte-lipstick")
        );
        assert_eq!(items[0]["description"], json!("A matte lipstick"));
        assert_eq!(items[0]["old_price"], json!("15.00"));
        assert_eq!(items[0]["barcode"], json!("111"));

        assert_eq!(items[1]["stock"], json!(0), "unavailable => 0");
        assert_eq!(items[1]["sku"], json!(""), "null => \"\"");
        assert_eq!(items[1]["old_price"], json!(""));
        assert_eq!(items[1]["barcode"], json!(""));
    }

    /// `float(True) == 1.0`: a `bool` is an `int` in Python, so a boolean
    /// `compare_at_price` survives the `and float(old_price)` guard.
    #[test]
    fn a_boolean_compare_at_price_is_emitted_not_an_error() {
        let spider = ShopifySpider::builder()
            .target_website("example.com")
            .build()
            .expect("a target");
        let product = serde_json::json!({
            "title": "T", "handle": "h", "vendor": "V", "images": [], "body_html": "",
            "variants": [
                {"id": 1, "title": "A", "price": "1", "compare_at_price": true},
                {"id": 2, "title": "B", "price": "1", "compare_at_price": false},
            ],
        });
        let items = spider.process_product(&product, "c").expect("well-formed");
        assert_eq!(items[0].get("old_price"), Some(&Value::Bool(true)));
        // `False` is falsy, so it never reaches `float()` and becomes `""`.
        assert_eq!(
            items[1].get("old_price"),
            Some(&Value::String(String::new()))
        );
    }

    /// `self.collected_ids` is a Python `set`: `1`, `1.0` and `True` are one
    /// element, `"1"` is a different one, and a list is unhashable.
    #[test]
    fn variant_ids_are_deduplicated_with_pythons_equality() {
        let spider = ShopifySpider::builder()
            .target_website("example.com")
            .build()
            .expect("a target");
        let product = serde_json::json!({
            "title": "T", "handle": "h", "vendor": "V", "images": [], "body_html": "",
            "variants": [
                {"id": 1, "title": "A", "price": "1"},
                {"id": 1.0, "title": "B", "price": "1"},
                {"id": true, "title": "C", "price": "1"},
                {"id": "1", "title": "D", "price": "1"},
                {"id": 2.5, "title": "E", "price": "1"},
                {"id": null, "title": "F", "price": "1"},
            ],
        });
        let items = spider.process_product(&product, "c").expect("well-formed");
        let names: Vec<&str> = items
            .iter()
            .map(|item| item["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, ["T - A", "T - D", "T - E", "T - F"]);

        // A list id is unhashable: `variant["id"] in self.collected_ids`
        // raises `TypeError` in Python, so this is an error, not a silent skip.
        let unhashable = serde_json::json!({
            "title": "T", "handle": "h", "vendor": "V", "images": [], "body_html": "",
            "variants": [{"id": [1], "title": "A", "price": "1"}],
        });
        assert!(spider.process_product(&unhashable, "c").is_err());
    }

    /// `collected_ids` is shared, so the de-duplication must hold when two
    /// collections are processed at once.
    ///
    /// Python's `set` is protected by the GIL and its `in` / `add` pair is not
    /// atomic across an `await`; here the guard is held across the whole
    /// read-modify-write, and this is the test that notices if it stops being.
    /// Only the *total* is asserted — which thread wins each variant is
    /// genuinely unspecified on both sides.
    #[test]
    fn concurrent_collections_still_emit_each_variant_once() {
        let spider = Arc::new(
            ShopifySpider::builder()
                .target_website("example.com")
                .build()
                .expect("a target"),
        );
        let variants: Vec<Value> = (0..200)
            .map(|id| serde_json::json!({"id": id, "title": "V", "price": "1"}))
            .collect();
        let product = serde_json::json!({
            "title": "T", "handle": "h", "vendor": "V", "images": [], "body_html": "",
            "variants": variants,
        });

        let threads: Vec<_> = (0..8)
            .map(|worker| {
                let spider = Arc::clone(&spider);
                let product = product.clone();
                std::thread::spawn(move || {
                    spider
                        .process_product(&product, &format!("c{worker}"))
                        .expect("well-formed")
                        .len()
                })
            })
            .collect();
        let total: usize = threads
            .into_iter()
            .map(|handle| handle.join().expect("no panic"))
            .sum();
        assert_eq!(
            total, 200,
            "every variant emitted exactly once across 8 threads"
        );
    }

    #[test]
    fn a_default_title_variant_keeps_the_plain_product_name() {
        let mut product = product();
        product["variants"] =
            json!([{"id": 9, "title": "Default Title", "price": "1", "available": true}]);
        let items = spider().process_product(&product, "x").expect("ok");
        assert_eq!(items[0]["name"], json!("Matte Lipstick"));
    }

    #[test]
    fn a_missing_image_and_a_null_body_html_become_empty_strings() {
        let mut product = product();
        product["images"] = json!([]);
        product["body_html"] = Value::Null;
        let items = spider().process_product(&product, "x").expect("ok");
        assert_eq!(items[0]["image_url"], json!(""));
        assert_eq!(items[0]["description"], json!(""));
    }

    #[test]
    fn a_numerically_zero_compare_at_price_becomes_an_empty_string() {
        let mut product = product();
        product["variants"] = json!([
            {"id": 1, "title": "A", "price": "1", "available": true, "compare_at_price": "0.00"},
            {"id": 2, "title": "B", "price": "1", "available": true, "compare_at_price": "9.99"}
        ]);
        let items = spider().process_product(&product, "x").expect("ok");
        assert_eq!(items[0]["old_price"], json!(""));
        assert_eq!(items[1]["old_price"], json!("9.99"));
    }

    #[test]
    fn a_non_numeric_compare_at_price_is_pythons_value_error() {
        let mut product = product();
        product["variants"] = json!([{"id": 1, "title": "A", "price": "1", "available": true, "compare_at_price": "abc"}]);
        let error = spider()
            .process_product(&product, "x")
            .expect_err("not a float");
        assert!(error.to_string().contains("float"), "{error}");
    }

    #[test]
    fn variants_dedupe_per_instance() {
        let spider = spider();
        assert_eq!(
            spider.process_product(&product(), "a").expect("ok").len(),
            2
        );
        assert!(
            spider
                .process_product(&product(), "b")
                .expect("ok")
                .is_empty(),
            "the same variant in a second collection is not emitted twice"
        );
        // …but a fresh instance starts over.
        let fresh = ShopifySpider::builder()
            .target_website("example.com")
            .build()
            .expect("ok");
        assert_eq!(fresh.process_product(&product(), "a").expect("ok").len(), 2);
    }

    #[test]
    fn a_missing_required_key_is_a_typed_error() {
        let mut product = product();
        product.as_object_mut().expect("object").remove("vendor");
        let error = spider()
            .process_product(&product, "x")
            .expect_err("KeyError");
        assert!(error.to_string().contains("'vendor'"), "{error}");
    }

    #[test]
    fn parse_collection_emits_items_then_the_next_page() {
        let body = json!({"products": [product()]});
        let response = response(
            body,
            &[("handle", json!("summer-sale")), ("page", json!(1))],
        );
        let yielded = spider().parse_collection_yields(&response);
        assert_eq!(yielded.len(), 3);
        assert!(matches!(yielded[0], Yielded::Item(_)));
        assert!(matches!(yielded[1], Yielded::Item(_)));
        match &yielded[2] {
            Yielded::Request(request) => {
                assert_eq!(
                    request.url,
                    "https://example.com/collections/summer-sale/products.json?page=2&limit=250"
                );
                assert_eq!(request.meta["page"], json!(2));
                assert_eq!(request.meta["handle"], json!("summer-sale"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_products_list_stops_the_collection() {
        let response = response(json!({"products": []}), &[("handle", json!("x"))]);
        assert!(spider().parse_collection_yields(&response).is_empty());
    }

    #[tokio::test]
    async fn dispatch_routes_parse_collection() {
        let spider = Arc::new(spider());
        let response = response(
            json!({"products": [product()]}),
            &[("handle", json!("x")), ("page", json!(1))],
        );
        let yielded: Vec<Yielded> = spider
            .dispatch(PARSE_COLLECTION, response)
            .await
            .collect()
            .await;
        assert_eq!(yielded.len(), 3);
    }

    #[test]
    fn has_callback_covers_both_names() {
        let spider = spider();
        assert!(spider.has_callback(&CallbackId::PARSE));
        assert!(spider.has_callback(&PARSE_COLLECTION));
        assert!(!spider.has_callback(&CallbackId::from_static("nope")));
    }
}
