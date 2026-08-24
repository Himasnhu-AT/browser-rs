//! The typed error enum for this crate: [`StorageError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! Python's storage layer has no error surface at all: `sqlite3` and `orjson`
//! raise, and the reference implementation lets those exceptions escape. The Rust port turns every
//! one of those failure points into a variant of [`StorageError`] instead, and
//! always names the identifier and the database file, because the alternative —
//! a bare `rusqlite::Error` — cannot tell a caller *which* fingerprint failed
//! to save.

use std::path::{Path, PathBuf};

/// Everything that can go wrong inside `browser-storage`.
///
/// Every variant keeps the underlying `rusqlite` / `serde_json` error as its
/// [`source`](std::error::Error::source), so a caller that wants the raw SQLite
/// extended result code can still reach it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// The SQLite database file could not be opened or created.
    #[error("could not open the adaptive-storage database at {}: {source}", path.display())]
    Open {
        /// The database file that was being opened. `:memory:` for an
        /// in-memory database.
        path: PathBuf,
        /// The underlying `rusqlite` failure.
        #[source]
        source: rusqlite::Error,
    },

    /// `PRAGMA journal_mode=WAL` or `CREATE TABLE IF NOT EXISTS storage …`
    /// failed on a database that opened successfully.
    ///
    /// The usual cause is an existing file that is not a SQLite database, or a
    /// `storage` table with an incompatible schema.
    #[error("could not prepare the adaptive-storage schema in {}: {source}", path.display())]
    Schema {
        /// The database file whose schema could not be prepared.
        path: PathBuf,
        /// The underlying `rusqlite` failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The `INSERT OR REPLACE` failed.
    #[error(
        "could not save the fingerprint for identifier {identifier:?} under {base_url:?}: {source}"
    )]
    Save {
        /// The identifier the caller passed to `StorageSystem::save`.
        identifier: String,
        /// The namespace the row would have been written under — the
        /// registrable domain, or `"default"`.
        base_url: String,
        /// The underlying `rusqlite` failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The `SELECT element_data FROM storage WHERE …` failed.
    #[error("could not retrieve the fingerprint for identifier {identifier:?} under {base_url:?}: {source}")]
    Retrieve {
        /// The identifier the caller passed to `StorageSystem::retrieve`.
        identifier: String,
        /// The namespace that was searched — the registrable domain, or
        /// `"default"`.
        base_url: String,
        /// The underlying `rusqlite` failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The fingerprint could not be serialised to JSON.
    ///
    /// [`ElementFingerprint`](browser_core::ElementFingerprint) is a plain
    /// data struct with no custom `Serialize`, so in practice this is
    /// unreachable; it exists so that `save` never panics.
    #[error("could not serialise the fingerprint for identifier {identifier:?}: {source}")]
    Serialize {
        /// The identifier the fingerprint would have been stored under.
        identifier: String,
        /// The underlying `serde_json` failure.
        #[source]
        source: serde_json::Error,
    },

    /// A row was found but its `element_data` is not a JSON document that
    /// deserialises into an
    /// [`ElementFingerprint`](browser_core::ElementFingerprint).
    ///
    /// This is what a database written by an *incompatible* version looks like
    /// from here. It is reported rather than swallowed, because silently
    /// returning `None` would make the adaptive layer re-learn the element and
    /// overwrite the row.
    #[error("the stored fingerprint for identifier {identifier:?} under {base_url:?} is not valid fingerprint JSON: {source}")]
    Deserialize {
        /// The identifier whose row could not be decoded.
        identifier: String,
        /// The namespace the row was read from.
        base_url: String,
        /// The underlying `serde_json` failure.
        #[source]
        source: serde_json::Error,
    },

    /// A row was found but `element_data` holds neither `TEXT` nor a `BLOB`.
    ///
    /// Python writes `orjson.dumps()` *bytes* into the `TEXT` column, which
    /// SQLite stores with BLOB affinity, so both storage classes are accepted;
    /// anything else means the row was not written by either implementation.
    #[error("the stored fingerprint for identifier {identifier:?} under {base_url:?} has SQLite storage class {found}, expected TEXT or BLOB")]
    UnexpectedColumnType {
        /// The identifier whose row could not be decoded.
        identifier: String,
        /// The namespace the row was read from.
        base_url: String,
        /// The offending storage class, as SQLite names it (`NULL`,
        /// `INTEGER`, `REAL`).
        found: &'static str,
    },

    /// [`SqliteStorage::close`](crate::sqlite::SqliteStorage::close) could not close
    /// the connection.
    ///
    /// The connection is returned to the caller by `rusqlite` when this
    /// happens, but it is dropped here — the only recovery is to reopen.
    #[error("could not close the adaptive-storage database at {}: {source}", path.display())]
    Close {
        /// The database file that failed to close.
        path: PathBuf,
        /// The underlying `rusqlite` failure.
        #[source]
        source: rusqlite::Error,
    },
}

impl StorageError {
    /// Builds an [`Open`](StorageError::Open) error for `path`.
    pub(crate) fn open(path: &Path, source: rusqlite::Error) -> Self {
        Self::Open {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Builds a [`Schema`](StorageError::Schema) error for `path`.
    pub(crate) fn schema(path: &Path, source: rusqlite::Error) -> Self {
        Self::Schema {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A [`Result`](std::result::Result) whose error is always a [`StorageError`].
pub type Result<T, E = StorageError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_identifier_and_the_namespace() {
        let err = StorageError::Save {
            identifier: "div.main".to_owned(),
            base_url: "example.com".to_owned(),
            source: rusqlite::Error::QueryReturnedNoRows,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("\"div.main\""), "{rendered}");
        assert!(rendered.contains("\"example.com\""), "{rendered}");
    }

    #[test]
    fn open_errors_carry_the_path() {
        let err = StorageError::open(
            Path::new("/tmp/elements_storage.db"),
            rusqlite::Error::QueryReturnedNoRows,
        );
        assert!(err.to_string().contains("/tmp/elements_storage.db"));
    }

    #[test]
    fn errors_expose_their_source() {
        use std::error::Error as _;

        let err = StorageError::schema(Path::new(":memory:"), rusqlite::Error::QueryReturnedNoRows);
        assert!(err.source().is_some());
    }
}
