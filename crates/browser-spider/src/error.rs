//! The typed error enum for this crate: [`SpiderError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Python raises bare `ValueError` / `KeyError` / `RuntimeError` from the
//! session manager and the checkpoint manager. Every one of those messages is
//! pinned by `tests/spiders/test_session.py` and `test_spider.py`, so each gets
//! its own variant here and reproduces the message byte for byte —
//! see [`SpiderError::SessionAlreadyRegistered`], [`SpiderError::UnknownSession`],
//! [`SpiderError::NoSessionsRegistered`] and the two checkpoint-interval variants.
//!
//! Ownership note: units 16 and 17 run in the SAME wave. If either needs a new
//! variant, add it here only if the other does not — otherwise keep a
//! module-local error type in your own file and let unit 20 unify them.

use std::path::PathBuf;

/// Everything that can go wrong inside `browser-spider`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpiderError {
    /// `SessionManager::add` was given an id that is already registered.
    ///
    /// Python: `ValueError(f"Session '{session_id}' already registered")`.
    #[error("Session '{id}' already registered")]
    SessionAlreadyRegistered {
        /// The duplicate session id.
        id: String,
    },

    /// `SessionManager::pop` / `remove` was given an id that is not registered.
    ///
    /// Python: `KeyError(f"Session '{session_id}' not found")` — note that this
    /// message, unlike [`SpiderError::UnknownSession`], has no `Available:` tail.
    #[error("Session '{id}' not found")]
    SessionNotFound {
        /// The missing session id.
        id: String,
    },

    /// `SessionManager::get` / `fetch` was given an id that is not registered.
    ///
    /// Python: `KeyError(f"Session '{session_id}' not found. Available: {available}")`.
    #[error("Session '{id}' not found. Available: {available}")]
    UnknownSession {
        /// The missing session id.
        id: String,
        /// The registered ids, comma-separated, in insertion order.
        available: String,
    },

    /// `SessionManager::default_session_id` was read while no session was registered.
    ///
    /// Python: `RuntimeError("No sessions registered")`.
    #[error("No sessions registered")]
    NoSessionsRegistered,

    /// A session's `open` / `close` / `fetch` failed.
    ///
    /// The transport crates own their own error enums; rather than depend on
    /// them (this crate must stay usable with a mock session), the message is
    /// flattened into a string at the boundary. `ARCHITECTURE.md` §5.3 forbids
    /// `Box<dyn Error>` in a library crate, and a session implementor is free
    /// to log the structured error before converting.
    #[error("session {sid:?} failed to fetch {url}: {message}")]
    Fetch {
        /// The session id the request was routed to.
        sid: String,
        /// The URL that was being fetched.
        url: String,
        /// The transport's own `Display` output.
        message: String,
    },

    /// A session's `open`/`close` lifecycle call failed.
    #[error("session {sid:?} failed to {action}: {message}")]
    SessionLifecycle {
        /// The session id.
        sid: String,
        /// Either `"open"` or `"close"`.
        action: &'static str,
        /// The transport's own `Display` output.
        message: String,
    },

    /// The checkpoint interval was not a finite number.
    ///
    /// Python: `TypeError("Checkpoints interval must be integer or float.")` —
    /// reachable there by passing a string; reachable here only via `f64::NAN`
    /// or an infinity.
    #[error("Checkpoints interval must be integer or float.")]
    CheckpointIntervalNotANumber,

    /// The checkpoint interval was negative.
    ///
    /// Python: `ValueError("Checkpoints interval must be equal or greater than 0.")`.
    #[error("Checkpoints interval must be equal or greater than 0.")]
    CheckpointIntervalNegative,

    /// Writing (or renaming) the checkpoint file failed.
    ///
    /// `CheckpointManager::save` re-raises, exactly like Python's; only
    /// `cleanup` swallows.
    #[error("could not save the checkpoint to {}: {source}", path.display())]
    CheckpointSave {
        /// The file that was being written.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// Serialising the checkpoint (or an item export) to JSON failed.
    #[error("could not serialise {what}: {source}")]
    Serialize {
        /// A short description of the value that failed to serialise.
        what: &'static str,
        /// The underlying `serde_json` failure.
        #[source]
        source: serde_json::Error,
    },

    /// An [`crate::items::ItemList`] export could not be written.
    #[error("could not export items to {}: {source}", path.display())]
    Export {
        /// The output file.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// `Result` specialised to [`SpiderError`].
pub type Result<T, E = SpiderError> = std::result::Result<T, E>;
