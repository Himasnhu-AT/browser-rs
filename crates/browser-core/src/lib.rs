//! Shared vocabulary for `browser-rs`: text helpers, HTML entities, attribute
//! maps, sequence similarity and element fingerprints.
//!
//! This crate performs **no I/O** and has no `tokio` dependency, so it stays
//! usable without an async runtime. See `ARCHITECTURE.md` §1.1.

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
