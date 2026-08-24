//! The `browser-rs` HTML tree: an `html5ever` parse driven into an `ego-tree`
//! arena, normalised to match what lxml's flags produce.
//!
//! See `ARCHITECTURE.md` §1.2.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod edit;
pub mod error;
pub mod nav;
pub mod node;
pub mod parse;
pub mod qualname;
pub mod serialize;
pub mod tree;
