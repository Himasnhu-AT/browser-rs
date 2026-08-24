//! [`SqliteStorage`]: the byte-identical schema, `INSERT OR REPLACE`, WAL journalling and a
//! `parking_lot::Mutex<Connection>`.
//!
//! # On-disk compatibility
//!
//! The point of this module is that the reference implementation's `elements_storage.db` is a
//! *shared* format: a database written by the Python implementation must be
//! readable and writable here, and vice versa. So the schema text
//! ([`SCHEMA_SQL`]), the two statements ([`INSERT_SQL`], [`SELECT_SQL`]) and the
//! journal mode ([`JOURNAL_MODE_SQL`]) are copied verbatim from
//! `reference/core/storage.py` rather than generated.
//!
//! One difference is unavoidable and harmless. Python binds
//! `orjson.dumps(...)`, which is `bytes`, to the `element_data` column; SQLite
//! is dynamically typed, so the value lands with BLOB affinity even though the
//! column is declared `TEXT`. This implementation writes real `TEXT`. Both
//! sides read both storage classes — [`SqliteStorage::retrieve`] accepts
//! `Text` and `Blob` alike — so the only observable consequence is that a
//! byte-level diff of two databases will differ.
//!
//! # Concurrency
//!
//! Python uses `sqlite3.connect(check_same_thread=False)` guarded by a
//! `threading.RLock`. Here: one [`rusqlite::Connection`] behind a
//! [`parking_lot::Mutex`], which is *not* reentrant — the Python code never
//! re-enters the lock (`save`/`retrieve` call `_get_base_url` *before* taking
//! it), and neither does this port, so the two are equivalent. `PRAGMA
//! journal_mode=WAL` gives readers in other processes concurrency with the
//! writer.
//!
//! Across *connections* — several `SqliteStorage` instances on one file, or a
//! Python process sharing it — the mutex is no help and SQLite's own busy
//! handler takes over. `rusqlite` arms it with a 5-second timeout on open,
//! which is exactly `sqlite3.connect`'s default `timeout=5.0`, so the two
//! implementations wait for each other the same way.
//!
//! Note that `base_url` is computed once, in the constructor: Python memoises
//! `_get_base_url` in an LRU keyed on `(self, default)` that keeps its
//! instances alive forever, but `self.url` is immutable after `__init__`, so
//! hoisting the computation is observationally identical (`ARCHITECTURE.md`
//! §1.5).

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use browser_core::ElementFingerprint;

use crate::base_url::{normalise_url, registrable_domain};
use crate::error::StorageError;
use crate::r#trait::StorageSystem;

/// The `storage` table, verbatim from `SQLiteStorageSystem._setup_database`.
///
/// `UNIQUE (url, identifier)` is what makes [`INSERT_SQL`]'s `OR REPLACE`
/// overwrite instead of appending.
pub const SCHEMA_SQL: &str = "
            CREATE TABLE IF NOT EXISTS storage (
                id INTEGER PRIMARY KEY,
                url TEXT,
                identifier TEXT,
                element_data TEXT,
                UNIQUE (url, identifier)
            )
        ";

/// The write statement, verbatim from `SQLiteStorageSystem.save`.
pub const INSERT_SQL: &str = "
                INSERT OR REPLACE INTO storage (url, identifier, element_data)
                VALUES (?, ?, ?)
            ";

/// The read statement, verbatim from `SQLiteStorageSystem.retrieve`.
pub const SELECT_SQL: &str = "SELECT element_data FROM storage WHERE url = ? AND identifier = ?";

/// The journal-mode pragma. WAL lets readers and the writer run concurrently.
pub const JOURNAL_MODE_SQL: &str = "PRAGMA journal_mode=WAL";

/// The path Python passes for an in-memory database, and the value
/// [`SqliteStorage::in_memory`] reports from
/// [`storage_file`](SqliteStorage::storage_file).
pub const IN_MEMORY_PATH: &str = ":memory:";

/// The default, thread-safe adaptive-storage backend.
///
/// ```
/// use browser_core::ElementFingerprint;
/// use browser_storage::r#trait::StorageSystem;
/// use browser_storage::sqlite::SqliteStorage;
///
/// let storage = SqliteStorage::in_memory(Some("https://www.example.com/page"))?;
/// assert_eq!(storage.base_url(), "example.com");
///
/// let fingerprint = ElementFingerprint {
///     tag: "p".into(),
///     text: Some("Hello".into()),
///     path: vec!["html".into(), "body".into(), "div".into(), "p".into()],
///     ..Default::default()
/// };
/// storage.save(&fingerprint, "p.price")?;
///
/// assert_eq!(storage.retrieve("p.price")?, Some(fingerprint));
/// assert_eq!(storage.retrieve("never-seen")?, None);
/// # Ok::<(), browser_storage::error::StorageError>(())
/// ```
#[derive(Debug)]
pub struct SqliteStorage {
    /// The database file, or [`IN_MEMORY_PATH`].
    storage_file: PathBuf,
    /// The constructor's `url` argument, lowercased; `None` when it was absent
    /// or empty. Python's `self.url`.
    url: Option<String>,
    /// `_get_base_url()`, computed once — the value of the `url` column.
    base_url: String,
    /// Python's `self.connection` + `self.cursor` + `self.lock` in one.
    connection: Mutex<Connection>,
}

impl SqliteStorage {
    /// Opens (creating it if needed) the database at `path`.
    ///
    /// `url` is the address of the site being scraped — a full URL or a bare
    /// domain, since the registrable-domain extraction prepends `https://` when
    /// there is no scheme. It only ever namespaces the rows; passing `None`
    /// puts them under `"default"`.
    ///
    /// # Errors
    ///
    /// [`StorageError::Open`] if the file cannot be opened or created, and
    /// [`StorageError::Schema`] if the journal mode or the table cannot be set
    /// up (an existing file that is not a SQLite database fails here).
    pub fn open(path: impl AsRef<Path>, url: Option<&str>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let connection = Connection::open(path).map_err(|e| StorageError::open(path, e))?;
        Self::from_connection(connection, path.to_path_buf(), url)
    }

    /// Opens a private, in-memory database — Python's
    /// `SQLiteStorageSystem(storage_file=":memory:")`.
    ///
    /// Nothing is shared with any other connection and nothing survives the
    /// drop. WAL does not apply to in-memory databases; SQLite silently keeps
    /// its `memory` journal mode, exactly as it does for Python.
    ///
    /// # Errors
    ///
    /// As [`open`](SqliteStorage::open).
    pub fn in_memory(url: Option<&str>) -> Result<Self, StorageError> {
        let path = PathBuf::from(IN_MEMORY_PATH);
        let connection = Connection::open_in_memory().map_err(|e| StorageError::open(&path, e))?;
        Self::from_connection(connection, path, url)
    }

    /// Applies the pragma and the schema to a freshly opened connection.
    fn from_connection(
        connection: Connection,
        storage_file: PathBuf,
        url: Option<&str>,
    ) -> Result<Self, StorageError> {
        // `PRAGMA journal_mode` answers with the mode it settled on, so it has
        // to be run as a query; `execute` would report "Execute returned
        // results".
        let journal_mode: String = connection
            .query_row(JOURNAL_MODE_SQL, [], |row| row.get(0))
            .map_err(|e| StorageError::schema(&storage_file, e))?;

        connection
            .execute_batch(SCHEMA_SQL)
            .map_err(|e| StorageError::schema(&storage_file, e))?;

        let normalised = normalise_url(url);
        let base_url = registrable_domain(normalised.as_deref());

        // Python: `log.debug(f'Storage system loaded with arguments
        // (storage_file="{storage_file}", url="{url}")')`. The message text is
        // kept verbatim — it is the one line users grep for when adaptive
        // storage goes to the wrong namespace — and the structured fields carry
        // the values a `tracing` subscriber would rather filter on.
        tracing::debug!(
            storage_file = %storage_file.display(),
            url = url.unwrap_or("None"),
            base_url = %base_url,
            journal_mode = %journal_mode,
            "Storage system loaded with arguments (storage_file=\"{}\", url=\"{}\")",
            storage_file.display(),
            url.unwrap_or("None"),
        );

        Ok(Self {
            storage_file,
            url: normalised,
            base_url,
            connection: Mutex::new(connection),
        })
    }

    /// The database file this instance was opened on, or [`IN_MEMORY_PATH`].
    #[must_use]
    pub fn storage_file(&self) -> &Path {
        &self.storage_file
    }

    /// The constructor's `url` argument, lowercased — Python's `self.url`.
    ///
    /// `None` when the argument was absent or empty.
    ///
    /// ```
    /// use browser_storage::sqlite::SqliteStorage;
    ///
    /// let storage = SqliteStorage::in_memory(Some("https://WWW.EXAMPLE.COM/Page"))?;
    /// assert_eq!(storage.url(), Some("https://www.example.com/page"));
    /// # Ok::<(), browser_storage::error::StorageError>(())
    /// ```
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The namespace every row of this instance is keyed under: the registrable
    /// domain of [`url`](SqliteStorage::url), or `"default"`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Closes the connection, reporting a failure instead of swallowing it.
    ///
    /// Dropping a `SqliteStorage` closes the database too — this exists for the
    /// callers that want to know whether the close succeeded, and as the
    /// counterpart of Python's `close()` (which spiders call from
    /// `Spider.closed()`). Nothing needs committing first: `rusqlite` runs in
    /// autocommit mode, so every `save` is already durable when it returns.
    ///
    /// # Errors
    ///
    /// [`StorageError::Close`] when SQLite refuses to close the connection,
    /// which in practice means a statement is still running.
    pub fn close(self) -> Result<(), StorageError> {
        let path = self.storage_file;
        let connection = self.connection.into_inner();
        connection
            .close()
            .map_err(|(_, source)| StorageError::Close { path, source })
    }

    /// The number of rows this instance's namespace holds.
    ///
    /// Not part of the Python API; it exists because "did the write land?" is
    /// otherwise unanswerable without a second `retrieve` per identifier.
    ///
    /// # Errors
    ///
    /// [`StorageError::Retrieve`] if the count cannot be read.
    pub fn len(&self) -> Result<i64, StorageError> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT COUNT(*) FROM storage WHERE url = ?",
                [&self.base_url],
                |row| row.get(0),
            )
            .map_err(|source| StorageError::Retrieve {
                identifier: String::new(),
                base_url: self.base_url.clone(),
                source,
            })
    }

    /// Whether this instance's namespace holds no rows at all.
    ///
    /// # Errors
    ///
    /// As [`len`](SqliteStorage::len).
    pub fn is_empty(&self) -> Result<bool, StorageError> {
        Ok(self.len()? == 0)
    }
}

impl StorageSystem for SqliteStorage {
    /// `SQLiteStorageSystem.save`.
    ///
    /// `INSERT OR REPLACE` plus the `UNIQUE (url, identifier)` constraint means
    /// a second save under the same identifier overwrites the first — and, as
    /// in Python, the row is deleted and re-inserted, so it takes a fresh
    /// `id`.
    fn save(&self, fingerprint: &ElementFingerprint, identifier: &str) -> Result<(), StorageError> {
        // Serialise outside the lock: it cannot touch the connection, and
        // holding the mutex across it would serialise the JSON work too.
        //
        // Note the encoder: `crate::python_json`, not `ElementFingerprint`'s own
        // `Serialize`. The derived one skips every `None` optional, but
        // `element_to_dict` writes the three `parent_*` keys as a block whenever
        // the element had a parent — and the reference implementation's relocation scorer
        // subscripts `original["parent_attribs"]` / `original["parent_text"]`
        // directly, so a row missing them raises `KeyError` on the Python side
        // long after `retrieve()` happily returned it.
        let element_data = crate::python_json::to_string(fingerprint).map_err(|source| {
            StorageError::Serialize {
                identifier: identifier.to_owned(),
                source,
            }
        })?;

        // Bound as **bytes**, so SQLite files the value under the BLOB storage
        // class. That is not a stylistic choice: `element_data` is declared
        // `TEXT`, but Python binds `orjson.dumps(...)`, which returns `bytes`,
        // and TEXT affinity leaves a blob a blob. Binding a `String` here would
        // make every row this crate writes a `typeof() = 'text'` row — readable
        // by Python (`orjson.loads` takes both), yet a visible difference
        // between two files that are otherwise the same database. Writing bytes
        // keeps a Rust-written `elements_storage.db` indistinguishable from a
        // Python-written one, which is what `tools/verify_rust_db.py` checks.
        let connection = self.connection.lock();
        connection
            .execute(
                INSERT_SQL,
                rusqlite::params![&self.base_url, identifier, element_data.as_bytes()],
            )
            .map_err(|source| StorageError::Save {
                identifier: identifier.to_owned(),
                base_url: self.base_url.clone(),
                source,
            })?;
        Ok(())
    }

    /// `SQLiteStorageSystem.retrieve`.
    ///
    /// Accepts both storage classes for `element_data`: the `BLOB` both
    /// implementations write, and `TEXT`, which is what a row bound as a string
    /// — by an older build of this crate, or by hand at the `sqlite3` prompt —
    /// leaves in a column of `TEXT` affinity.
    fn retrieve(&self, identifier: &str) -> Result<Option<ElementFingerprint>, StorageError> {
        let connection = self.connection.lock();
        let mut statement =
            connection
                .prepare_cached(SELECT_SQL)
                .map_err(|source| StorageError::Retrieve {
                    identifier: identifier.to_owned(),
                    base_url: self.base_url.clone(),
                    source,
                })?;

        let mut rows = statement
            .query(rusqlite::params![&self.base_url, identifier])
            .map_err(|source| StorageError::Retrieve {
                identifier: identifier.to_owned(),
                base_url: self.base_url.clone(),
                source,
            })?;

        let Some(row) = rows.next().map_err(|source| StorageError::Retrieve {
            identifier: identifier.to_owned(),
            base_url: self.base_url.clone(),
            source,
        })?
        else {
            // `cursor.fetchone()` returned nothing: not an error, just a
            // fingerprint that has never been stored.
            return Ok(None);
        };

        let raw = row.get_ref(0).map_err(|source| StorageError::Retrieve {
            identifier: identifier.to_owned(),
            base_url: self.base_url.clone(),
            source,
        })?;

        let bytes = match raw {
            ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes,
            ValueRef::Null => {
                return Err(StorageError::UnexpectedColumnType {
                    identifier: identifier.to_owned(),
                    base_url: self.base_url.clone(),
                    found: "NULL",
                })
            }
            ValueRef::Integer(_) => {
                return Err(StorageError::UnexpectedColumnType {
                    identifier: identifier.to_owned(),
                    base_url: self.base_url.clone(),
                    found: "INTEGER",
                })
            }
            ValueRef::Real(_) => {
                return Err(StorageError::UnexpectedColumnType {
                    identifier: identifier.to_owned(),
                    base_url: self.base_url.clone(),
                    found: "REAL",
                })
            }
        };

        let fingerprint =
            serde_json::from_slice(bytes).map_err(|source| StorageError::Deserialize {
                identifier: identifier.to_owned(),
                base_url: self.base_url.clone(),
                source,
            })?;
        Ok(Some(fingerprint))
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use browser_core::fingerprint::AttributeDict;

    use super::*;

    /// A database file in the system temp directory that deletes itself — plus
    /// its `-wal` and `-shm` companions — when the test ends.
    ///
    /// `tempfile` is not a dependency of this crate, and a handful of lines
    /// here is cheaper than adding one for tests alone.
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};

            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "browser-storage-unit-{label}-{}-{unique}.db",
                std::process::id(),
            ));
            let _ = std::fs::remove_file(&path);
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.path.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(path));
            }
        }
    }

    /// `<div><p id="target" class="main">Hello</p></div>`, as
    /// `_StorageTools.element_to_dict` would fingerprint the `<p>`.
    fn target_paragraph() -> ElementFingerprint {
        let mut attributes = AttributeDict::new();
        attributes.insert("id".to_owned(), "target".to_owned());
        attributes.insert("class".to_owned(), "main".to_owned());

        let mut parent_attribs = AttributeDict::new();
        parent_attribs.insert("class".to_owned(), "  wrap  ".to_owned());

        ElementFingerprint {
            tag: "p".to_owned(),
            attributes,
            text: Some("Hello".to_owned()),
            path: vec![
                "html".to_owned(),
                "body".to_owned(),
                "div".to_owned(),
                "p".to_owned(),
            ],
            parent_name: Some("div".to_owned()),
            parent_attribs: Some(parent_attribs),
            parent_text: None,
            siblings: Some(vec!["span".to_owned()]),
            children: Some(vec!["b".to_owned(), "i".to_owned()]),
        }
    }

    #[test]
    fn in_memory_round_trip() {
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
        let fingerprint = target_paragraph();
        storage.save(&fingerprint, "test-element").expect("save");

        let retrieved = storage.retrieve("test-element").expect("retrieve");
        assert_eq!(retrieved, Some(fingerprint));
    }

    #[test]
    fn retrieving_an_unknown_identifier_yields_none() {
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
        assert_eq!(storage.retrieve("does-not-exist").expect("retrieve"), None);
    }

    #[test]
    fn saving_twice_overwrites() {
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");

        let mut first = target_paragraph();
        first.attributes.insert("id".to_owned(), "v1".to_owned());
        first.text = Some("First".to_owned());
        storage.save(&first, "my-element").expect("save");

        let mut second = target_paragraph();
        second.attributes.insert("id".to_owned(), "v2".to_owned());
        second.text = Some("Second".to_owned());
        storage.save(&second, "my-element").expect("save");

        let retrieved = storage
            .retrieve("my-element")
            .expect("retrieve")
            .expect("a row");
        assert_eq!(
            retrieved.attributes.get("id").map(String::as_str),
            Some("v2")
        );
        assert_eq!(retrieved.text.as_deref(), Some("Second"));
        assert_eq!(
            storage.len().expect("count"),
            1,
            "OR REPLACE must not append"
        );
    }

    #[test]
    fn the_url_argument_is_lowercased_and_reduced_to_its_domain() {
        let storage = SqliteStorage::in_memory(Some("https://WWW.EXAMPLE.COM/Page")).expect("open");
        assert_eq!(storage.url(), Some("https://www.example.com/page"));
        assert_eq!(storage.base_url(), "example.com");
        assert_eq!(StorageSystem::base_url(&storage), "example.com");
    }

    #[test]
    fn a_missing_or_empty_url_falls_back_to_the_default_namespace() {
        for url in [None, Some("")] {
            let storage = SqliteStorage::in_memory(url).expect("open");
            assert_eq!(storage.url(), None);
            assert_eq!(storage.base_url(), "default");
        }
    }

    #[test]
    fn opening_a_path_creates_the_file() {
        let db = TempDb::new("creates-file");
        assert!(!db.path().exists());

        let storage = SqliteStorage::open(db.path(), None).expect("open");
        assert!(db.path().exists());
        assert_eq!(storage.storage_file(), db.path());
        storage.close().expect("close");
    }

    #[test]
    fn rows_survive_reopening_the_same_file() {
        let db = TempDb::new("survives-reopen");
        let fingerprint = target_paragraph();

        let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");
        storage.save(&fingerprint, "persisted").expect("save");
        storage.close().expect("close");

        let reopened = SqliteStorage::open(db.path(), Some("https://example.com")).expect("reopen");
        assert_eq!(
            reopened.retrieve("persisted").expect("retrieve"),
            Some(fingerprint)
        );
    }

    #[test]
    fn a_file_backed_database_uses_wal() {
        let db = TempDb::new("wal-mode");
        let storage = SqliteStorage::open(db.path(), None).expect("open");

        let mode: String = storage
            .connection
            .lock()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(mode, "wal");
    }

    #[test]
    fn element_data_is_stored_as_a_compact_json_blob() {
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
        storage.save(&target_paragraph(), "shape").expect("save");

        let (type_name, bytes): (String, Vec<u8>) = storage
            .connection
            .lock()
            .query_row(
                "SELECT typeof(element_data), element_data FROM storage WHERE identifier = ?",
                ["shape"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row");

        // A BLOB, exactly like the `bytes` Python's `orjson.dumps` binds.
        assert_eq!(type_name, "blob");
        let json = String::from_utf8(bytes).expect("utf-8");
        // `orjson` emits no spaces, and `crate::python_json` matches it — key
        // for key, in `element_to_dict`'s insertion order.
        assert_eq!(
            json,
            concat!(
                r#"{"tag":"p","attributes":{"id":"target","class":"main"},"#,
                r#""text":"Hello","path":["html","body","div","p"],"#,
                r#""parent_name":"div","parent_attribs":{"class":"  wrap  "},"#,
                r#""parent_text":null,"siblings":["span"],"children":["b","i"]}"#,
            ),
        );
        // The parent block is written whole: `parent_text` is an explicit
        // `null`, never a missing key, because the reference implementation's scorer subscripts it.
        assert!(json.contains(r#""parent_text":null"#), "{json}");
        // `parent_attribs` is copied raw — unlike `attributes`, its values are
        // neither stripped nor dropped when blank.
        assert!(
            json.contains(r#""parent_attribs":{"class":"  wrap  "}"#),
            "{json}"
        );
    }

    #[test]
    fn a_blob_written_by_python_is_readable() {
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
        let fingerprint = target_paragraph();
        let bytes = crate::python_json::to_string(&fingerprint)
            .expect("serialise")
            .into_bytes();

        // This is exactly what `orjson.dumps` bound to a TEXT column produces.
        storage
            .connection
            .lock()
            .execute(
                INSERT_SQL,
                rusqlite::params![storage.base_url(), "from-python", bytes],
            )
            .expect("insert blob");

        let stored_type: String = storage
            .connection
            .lock()
            .query_row(
                "SELECT typeof(element_data) FROM storage WHERE identifier = ?",
                ["from-python"],
                |row| row.get(0),
            )
            .expect("typeof");
        assert_eq!(stored_type, "blob");

        assert_eq!(
            storage.retrieve("from-python").expect("retrieve"),
            Some(fingerprint)
        );
    }

    #[test]
    fn a_text_row_is_still_readable() {
        // Rows written as strings — by an earlier build of this crate, or by
        // hand at the `sqlite3` prompt — keep the TEXT storage class. Nothing
        // writes them any more, but they must not become unreadable.
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
        let fingerprint = target_paragraph();
        let json = crate::python_json::to_string(&fingerprint).expect("serialise");

        storage
            .connection
            .lock()
            .execute(
                INSERT_SQL,
                rusqlite::params![storage.base_url(), "legacy-text", json],
            )
            .expect("insert text");

        let stored_type: String = storage
            .connection
            .lock()
            .query_row(
                "SELECT typeof(element_data) FROM storage WHERE identifier = ?",
                ["legacy-text"],
                |row| row.get(0),
            )
            .expect("typeof");
        assert_eq!(stored_type, "text");

        assert_eq!(
            storage.retrieve("legacy-text").expect("retrieve"),
            Some(fingerprint)
        );
    }

    #[test]
    fn a_corrupt_row_is_reported_not_swallowed() {
        let storage = SqliteStorage::in_memory(None).expect("open");
        storage
            .connection
            .lock()
            .execute(
                INSERT_SQL,
                rusqlite::params![storage.base_url(), "broken", "{not json"],
            )
            .expect("insert");

        let err = storage
            .retrieve("broken")
            .expect_err("must not be Ok(None)");
        assert!(matches!(err, StorageError::Deserialize { .. }), "{err:?}");
        assert!(err.to_string().contains("\"broken\""));
    }

    #[test]
    fn a_row_of_the_wrong_storage_class_is_reported() {
        let storage = SqliteStorage::in_memory(None).expect("open");
        storage
            .connection
            .lock()
            .execute(
                INSERT_SQL,
                rusqlite::params![storage.base_url(), "empty", rusqlite::types::Null],
            )
            .expect("insert");

        let err = storage.retrieve("empty").expect_err("must not decode");
        assert!(
            matches!(
                err,
                StorageError::UnexpectedColumnType { found: "NULL", .. }
            ),
            "{err:?}",
        );
    }

    #[test]
    fn a_number_written_into_the_text_column_is_read_back_as_text() {
        // The column's TEXT affinity converts INTEGER and REAL on the way in,
        // which is why those two arms of `UnexpectedColumnType` are effectively
        // unreachable through this schema — the value arrives as `Text("42")`
        // and fails as *JSON*, not as a storage class.
        let storage = SqliteStorage::in_memory(None).expect("open");
        storage
            .connection
            .lock()
            .execute(
                INSERT_SQL,
                rusqlite::params![storage.base_url(), "numeric", 42_i64],
            )
            .expect("insert");

        let err = storage.retrieve("numeric").expect_err("must not decode");
        assert!(matches!(err, StorageError::Deserialize { .. }), "{err:?}");
    }

    #[test]
    fn opening_a_path_that_cannot_exist_fails_with_an_open_error() {
        let missing = std::env::temp_dir()
            .join("browser-storage-no-such-directory")
            .join("elements_storage.db");
        let err = SqliteStorage::open(&missing, None).expect_err("must reject");
        assert!(matches!(err, StorageError::Open { .. }), "{err:?}");
        assert!(err.to_string().contains("elements_storage.db"));
    }

    #[test]
    fn opening_a_file_that_is_not_a_database_fails_with_a_schema_error() {
        let db = TempDb::new("not-a-database");
        std::fs::write(db.path(), b"this is not a SQLite database").expect("write");

        let err = SqliteStorage::open(db.path(), None).expect_err("must reject");
        assert!(matches!(err, StorageError::Schema { .. }), "{err:?}");
    }

    #[test]
    fn two_urls_do_not_see_each_others_identifiers() {
        let db = TempDb::new("url-isolation-unit");

        let site_a = SqliteStorage::open(db.path(), Some("https://site-a.com")).expect("open a");
        site_a.save(&target_paragraph(), "shared-id").expect("save");

        let site_b = SqliteStorage::open(db.path(), Some("https://site-b.com")).expect("open b");
        assert_eq!(site_b.retrieve("shared-id").expect("retrieve"), None);

        // …while the owner still sees it, and both rows coexist.
        assert!(site_a.retrieve("shared-id").expect("retrieve").is_some());
        site_b.save(&target_paragraph(), "shared-id").expect("save");
        assert_eq!(site_a.len().expect("count"), 1);
        assert_eq!(site_b.len().expect("count"), 1);

        site_a.close().expect("close a");
        site_b.close().expect("close b");
    }

    #[test]
    fn concurrent_saves_from_twenty_threads() {
        let db = TempDb::new("concurrent-unit");
        let storage =
            Arc::new(SqliteStorage::open(db.path(), Some("https://example.com")).expect("open"));

        thread::scope(|scope| {
            for index in 0..20 {
                let storage = Arc::clone(&storage);
                scope.spawn(move || {
                    let mut fingerprint = target_paragraph();
                    fingerprint
                        .attributes
                        .insert("id".to_owned(), format!("elem-{index}"));
                    fingerprint.text = Some(format!("Text {index}"));
                    storage
                        .save(&fingerprint, &format!("element-{index}"))
                        .expect("save must not fail under contention");
                });
            }
        });

        for index in 0..20 {
            let identifier = format!("element-{index}");
            let retrieved = storage
                .retrieve(&identifier)
                .expect("retrieve")
                .unwrap_or_else(|| panic!("{identifier} not found after concurrent save"));
            assert_eq!(
                retrieved.text.as_deref(),
                Some(format!("Text {index}").as_str())
            );
        }
        assert_eq!(storage.len().expect("count"), 20);
    }

    #[test]
    fn len_and_is_empty_track_the_namespace() {
        let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
        assert!(storage.is_empty().expect("is_empty"));
        storage.save(&target_paragraph(), "one").expect("save");
        assert!(!storage.is_empty().expect("is_empty"));
        assert_eq!(storage.len().expect("len"), 1);
    }
}
