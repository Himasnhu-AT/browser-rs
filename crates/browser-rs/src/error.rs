//! The typed error enum for this crate: `Error`.
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Stub: first populated by work unit 20 (see `ARCHITECTURE.md` §7).
//!
//! One aggregating enum with a `#[from]` for each sub-error, so application code
//! can use a single `Result` without reaching for `anyhow`.
