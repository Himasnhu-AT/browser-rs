//! The `browser-rs` command-line interface, as a library so the argument tree
//! can be unit-tested without spawning a process.
//!
//! `anyhow` lives at this boundary only; everything below returns typed errors.
//! See `ARCHITECTURE.md` §1.13.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod args;
pub mod commands;
pub mod convert;
