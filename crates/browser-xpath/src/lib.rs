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

// The names every consumer reaches for. Unit 6 was not permitted to edit
// `lib.rs`, so `API.md` documented these as "suggested"; the integration wave
// applies them, so `browser_xpath::XPath` works alongside the module paths
// (which stay valid). The set is exactly the one `API.md` §"Suggested crate-root
// re-exports" names.
pub use crate::error::{Result, XPathError};
pub use crate::eval::{evaluate, select, Namespaces, Variable, Variables, XPath};
pub use crate::model::{DomModel, NodeKind, TreeModel, XNode};
pub use crate::value::Value;
