//! The typed error enum for this crate: [`DomError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Parsing itself is **infallible** (libxml2's `recover=True` is matched by
//! html5ever, which never rejects input), so every fallible entry point in this
//! crate is a *mutation* or a *base-URL* operation.

use ego_tree::NodeId;

/// Everything that can go wrong inside `browser-dom`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DomError {
    /// The given [`NodeId`] does not belong to this [`crate::tree::Document`]
    /// (or was already removed from the arena).
    #[error("no node with id {0:?} exists in this document")]
    MissingNode(NodeId),

    /// The operation is only defined for element nodes.
    #[error("node {0:?} is not an element")]
    NotAnElement(NodeId),

    /// The arena root (`NodeData::Document`) has no parent and cannot be
    /// detached, dropped or unwrapped.
    #[error("the document root cannot be detached")]
    RootNotDetachable,

    /// A base URL string could not be parsed by the `url` crate.
    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = DomError> = std::result::Result<T, E>;
