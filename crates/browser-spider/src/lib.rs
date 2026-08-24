//! The crawling engine of `browser-rs`.
//!
//! Callbacks are named ids rather than closures, because they have to survive a
//! checkpoint round-trip. See `ARCHITECTURE.md` §1.11 and §5.5.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod cache;
pub mod canonicalize;
pub mod checkpoint;
pub mod engine;
pub mod error;
pub mod items;
pub mod request;
pub mod robots;
pub mod scheduler;
pub mod session;
pub mod spider;
pub mod stats;
pub mod throttle;
