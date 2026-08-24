//! The typed error enum for this crate: `BrowserError`.
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Stub: first populated by work unit 13 (see `ARCHITECTURE.md` §7).
//!
//! Ownership note: unit 14 adds the CDP and Cloudflare variants. Units 13 and 14
//! run in different waves, so they never edit this file concurrently.
