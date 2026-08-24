//! **browser-rs** — an undetectable, adaptive web-scraping library for Rust.
//!
//! This is the façade users depend on. It re-exports a curated surface from the
//! member crates and owns the feature flags; it contains no logic of its own.
//! See `ARCHITECTURE.md` §1.13 and §5.4.
//!
//! # Features
//!
//! | Feature | Pulls in |
//! |---|---|
//! | `parser` *(default)* | the document API |
//! | `http` *(default)* | the HTTP fetchers |
//! | `browser` | headless Chromium over CDP |
//! | `spider` | the crawling engine and the ready-made spiders |
//! | `markdown` | HTML-to-markdown conversion and AI sanitisation |
//! | `adaptive` | adaptive selector storage (bundled SQLite) |
//! | `blocking` | the synchronous fetcher façade |
//! | `full` | all of the above |

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod error;
pub mod prelude;
