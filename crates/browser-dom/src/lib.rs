//! The `browser-rs` HTML tree: an `html5ever` parse driven into an `ego-tree`
//! arena, normalised to match what lxml's flags produce.
//!
//! See `ARCHITECTURE.md` §1.2, and `API.md` in this crate for the full cheat sheet.
//!
//! ```
//! use browser_dom::Document;
//!
//! let doc = Document::parse("<div id='x'>hello <b>world</b>tail</div>");
//! let div = doc.elements().find(|e| e.element_id() == Some("x")).unwrap();
//! assert_eq!(div.tag(), "div");
//! assert_eq!(div.text_content(), "hello worldtail");
//! ```
//!
//! # What downstream crates get from here
//!
//! [`NodeRef`] is the single node handle, and it satisfies both consumers named in
//! `ARCHITECTURE.md`:
//!
//! * **`browser-selectors` (§1.3)** needs a `selectors::Element`. `NodeRef` is
//!   `Copy + Eq + Hash + Ord`, exposes `local_name`/`element_id`/`classes`/
//!   `has_class`/`parent_element`/`prev_element_sibling`/`next_element_sibling`/
//!   `is_root_element`, and — critically for `OpaqueElement` — [`NodeRef::value`]
//!   hands back a `&'a NodeData` borrowed from the *arena*, so its address is a
//!   stable identity that outlives the (`Copy`, freely re-created) handle.
//! * **`browser-xpath` (§1.4)** needs a `TreeModel`. `NodeRef` covers every axis
//!   step in O(1); attributes are not arena nodes, so an XPath attribute node is
//!   addressed as `(NodeId, index)` via [`node::Attributes::nth`]; and
//!   [`tree::DocumentOrder`] provides the `document_order(n) -> u64` key the trait
//!   asks for, precomputed in one walk.
//!
//! Both seams are pinned by `tests/downstream_contract.rs` here and by
//! `tests/dom_seam.rs` in each of those two crates.

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

// ---------------------------------------------------------------------------
// Crate-root re-exports. Every consumer previously had to spell the module path;
// these are the names units 5/6/8/10 reach for constantly.
// ---------------------------------------------------------------------------

pub use crate::error::{DomError, Result};
pub use crate::node::{Attributes, Element, NodeData, NodeRef};
pub use crate::parse::{parse_html, parse_html_with, ParseOptions};
pub use crate::tree::{Document, DocumentOrder};

/// Re-exported so downstream crates can name arena ids and interned names without
/// taking their own (version-coupled) dependency on `ego-tree` / `markup5ever`.
pub use ego_tree::NodeId;
pub use markup5ever::{LocalName, Namespace, QualName};
