//! A complete XPath 1.0 engine evaluated directly over the `browser-rs` tree.
//!
//! Unimplemented constructs fail at **compile** time with
//! `XPathError::Unsupported`, never by silently returning an empty node-set.
//! See `ARCHITECTURE.md` §1.4.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod ast;
pub mod axes;
pub mod error;
pub mod eval;
pub mod functions;
pub mod lexer;
pub mod model;
pub mod parser;
pub mod value;
