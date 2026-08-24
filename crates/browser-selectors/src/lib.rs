//! Native CSS selection for `browser-rs`, built on Servo's `selectors` crate
//! and extended with the reference implementation's `::text`, `::attr()` and `:contains()`.
//!
//! See `ARCHITECTURE.md` §1.3 — we match CSS directly rather than translating
//! it to XPath.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod canonical;
pub mod error;
pub mod impl_selector;
pub mod matching;
pub mod parse;
pub mod pseudo;
