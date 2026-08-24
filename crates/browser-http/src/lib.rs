//! The HTTP fetchers of `browser-rs`.
//!
//! `config`, `merge`, `headers_job`, `retry` and `proxy` contain **no I/O** and
//! are unit-testable without a network; the transport lives behind the
//! `backend::HttpBackend` trait. See `ARCHITECTURE.md` §1.9 and §2.3.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod backend;
pub mod config;
pub mod error;
pub mod headers_job;
pub mod merge;
pub mod proxy;
pub mod redirect_safe;
pub mod response;
pub mod retry;
pub mod session;

#[cfg(feature = "blocking")]
pub mod blocking;
