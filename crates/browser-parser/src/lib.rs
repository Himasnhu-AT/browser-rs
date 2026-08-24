//! The public document API of `browser-rs` — the reference implementation's `Selector` and
//! `Selectors`, rebuilt as `Document`, `Node` and `Nodes`.
//!
//! See `ARCHITECTURE.md` §3 for the parse + select layer and §4 for adaptive
//! element relocation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod adaptive;
pub mod config;
pub mod document;
pub mod error;
pub mod find;
pub mod generate;
pub mod node;
pub mod nodes;
pub mod similar;
pub mod text;
