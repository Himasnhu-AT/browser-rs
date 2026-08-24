//! Ready-made spiders and link extraction for `browser-rs`.
//!
//! See `ARCHITECTURE.md` §1.12.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod crawl;
pub mod error;
pub mod feed;
pub mod gzip;
pub mod links;
pub mod shopify;
pub mod site_to_markdown;
pub mod sitemap;
