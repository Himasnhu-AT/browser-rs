//! Adaptive-selector persistence for `browser-rs`.
//!
//! The schema, SQL text and journal mode match the reference implementation's Python
//! implementation, so an existing `elements_storage.db` is readable and
//! writable by both. See `ARCHITECTURE.md` §1.5.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod base_url;
pub mod error;
pub mod hash;
pub mod python_json;
pub mod sqlite;
pub mod r#trait;

pub use crate::base_url::{registrable_domain, DEFAULT_BASE_URL};
pub use crate::error::{Result, StorageError};
pub use crate::hash::identifier_hash;
pub use crate::python_json::PythonShaped;
pub use crate::r#trait::StorageSystem;
pub use crate::sqlite::SqliteStorage;
