//! The typed error enum for this crate: `FetchError, TransportError`.
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Stub: first populated by work unit 11 (see `ARCHITECTURE.md` §7).
//!
//! Ownership note: unit 12 adds the transport and redirect variants to the same
//! enum. Units 11 and 12 run in different waves, so they never edit this file
//! concurrently.
