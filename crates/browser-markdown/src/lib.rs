//! A faithful port of Python's `markdownify` for `browser-rs`, plus the HTML
//! noise stripping and AI sanitisation passes.
//!
//! No existing Rust markdown converter reproduces markdownify's setext
//! headings, escaping, bullet cycling or table rules — see `ARCHITECTURE.md`
//! §1.6 for the reasoning.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod convert;
pub mod error;
pub mod escape;
pub mod sanitize;
pub mod tables;
pub mod traversal;
