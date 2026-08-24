//! The `HttpBackend` trait plus `WireRequest` / `WireResponse`, so a second
//! transport can be added behind the same seam.
//!
//! Stub: implemented by work unit 12 (see `ARCHITECTURE.md` §7).

#[cfg(feature = "impersonate")]
pub mod wreq;
