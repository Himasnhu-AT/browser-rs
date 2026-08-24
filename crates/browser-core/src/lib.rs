//! Shared vocabulary for `browser-rs`: text helpers, HTML entities, attribute
//! maps, sequence similarity and element fingerprints.
//!
//! This crate performs **no I/O** and has no `tokio` dependency, so it stays
//! usable without an async runtime. See `ARCHITECTURE.md` §1.1.
//!
//! # Module map
//!
//! | Module | What lives there |
//! |---|---|
//! | [`error`] | [`CoreError`] and the three typed leaf errors it wraps |
//! | [`types`] | the shared enums every fetcher argument list is built from, plus [`Override`] |
//! | [`text`] | [`TextExt`] / [`TextsExt`] (Python's `TextHandler`), the regex cache, [`text::clean_spaces`] |
//! | [`entities`] | `w3lib.html.replace_entities` |
//! | [`attrs`] | [`AttributeMap`] — the *user-facing*, read-only attribute view |
//! | [`similarity`] | the `difflib.SequenceMatcher` port |
//! | [`fingerprint`] | [`ElementFingerprint`], [`similarity_score`], [`dict_diff`] |
//! | [`numeric`] | [`round_half_even`] (CPython `round()` semantics) |
//! | [`shell`] | `SimpleCookie` / header-line parsing for `uncurl` |
//! | [`log`] | `tracing` setup and the level counter behind `CrawlStats` |
//!
//! # The two attribute types, and which one you want
//!
//! There are deliberately two:
//!
//! * [`attrs::AttributeMap`] is the **presentation** type — Python's
//!   `AttributesHandler`. It is read-only, insertion-ordered and carries
//!   [`AttributeMap::search_values`] and [`AttributeMap::json_string`]. This is what
//!   `browser-parser` hands back to users from `Node::attrib`.
//! * [`fingerprint::AttributeDict`] is a bare `IndexMap<String, String>` and is the
//!   **wire** type: it is what [`ElementFingerprint`] serialises, so its shape is
//!   pinned by JSON compatibility with Python and must not become a newtype.
//!
//! They convert in both directions for free — [`AttributeMap::as_index_map`],
//! [`AttributeMap::into_inner`] and `AttributeMap::from(dict)` — so the
//! DOM-facing boundary (`browser_dom::node::Attributes::to_index_map`) feeds
//! either one without a copy of the strings beyond the one allocation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod attrs;
pub mod entities;
pub mod error;
pub mod fingerprint;
pub mod log;
pub mod numeric;
pub mod shell;
pub mod similarity;
pub mod text;
pub mod types;

// ---------------------------------------------------------------------------
// Curated crate-root re-exports (`ARCHITECTURE.md` §1.1 lists `lib.rs` as
// "re-exports, `pub mod` list"). Downstream crates should be able to write
// `use browser_core::{TextExt, ElementFingerprint};` without knowing which
// module a name lives in. The modules stay public, so both spellings work.
// ---------------------------------------------------------------------------

pub use crate::attrs::AttributeMap;
pub use crate::entities::{replace_entities, replace_entities_with};
pub use crate::error::{CookieParseError, CoreError, HeaderParseError, RegexError, Result};
pub use crate::fingerprint::{
    dict_diff, similarity_score, AttributeDict, ElementFingerprint, DISTINGUISHING_ATTRIBUTES,
};
pub use crate::numeric::round_half_even;
pub use crate::shell::{parse_cookie_string, parse_header_lines, CookieMap, HeaderMap};
pub use crate::similarity::{ratio_seq, ratio_slice, ratio_str, Match, SequenceMatcher};
pub use crate::text::{clean_spaces, ReOptions, RegexSpec, TextExt, TextsExt};
pub use crate::types::{
    ExtractionType, FollowRedirects, HttpMethod, Override, PageLoadState, Proxy, RedirectMode,
    SameSite, SetCookieParam, WaitState,
};
