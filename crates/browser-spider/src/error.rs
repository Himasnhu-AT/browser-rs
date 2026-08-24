//! The typed error enum for this crate: `SpiderError`.
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Stub: first populated by work unit 15 (see `ARCHITECTURE.md` §7).
//!
//! Ownership note: units 16 and 17 run in the SAME wave. If either needs a new
//! variant, add it here only if the other does not — otherwise keep a
//! module-local error type in your own file and let unit 20 unify them.
