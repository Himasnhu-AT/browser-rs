//! The `tests/core/test_storage_core.py` expectations, mirrored.
//!
//! `ARCHITECTURE.md` §7 makes the 15 storage expectations of the reference implementation's own
//! suite the acceptance criteria for this crate. The ones that exercise
//! `_StorageTools.element_to_dict` (13, 14) belong to the DOM/parser units and
//! are not repeated here; everything that touches the storage layer is.
//!
//! The class-level `lru_cache` rituals of the Python tests
//! (`SQLiteStorageSystem.cache_clear()`, `_get_base_url.cache_clear()`) have no
//! counterpart: this port hands out ordinary owned values, so there is no
//! process-wide cache to reset between cases.

mod common;

use std::fs;

use browser_core::ElementFingerprint;
use browser_storage::base_url::{registrable_domain, DEFAULT_BASE_URL};
use browser_storage::hash::identifier_hash;
use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::{SqliteStorage, IN_MEMORY_PATH, SCHEMA_SQL};

use common::{target_paragraph, TempDb};

// --- TestGetBaseUrl ------------------------------------------------------

/// 1. `url=None` and `url=""` both land in the `"default"` namespace.
#[test]
fn returns_default_when_url_is_missing_or_empty() {
    for url in [None, Some("")] {
        let storage = SqliteStorage::in_memory(url).expect("open");
        assert_eq!(storage.base_url(), DEFAULT_BASE_URL);
        assert_eq!(storage.base_url(), "default");
    }
}

/// 2. A full URL is reduced to its registrable domain, not its host.
#[test]
fn returns_the_registrable_domain_for_a_valid_url() {
    let storage = SqliteStorage::in_memory(Some("https://www.example.com/page")).expect("open");
    assert_eq!(storage.base_url(), "example.com");
}

/// 3. The constructor lowercases the whole URL (a `tld` workaround, and
///    observable through `storage.url`).
#[test]
fn the_url_is_lowercased() {
    let storage = SqliteStorage::in_memory(Some("https://WWW.EXAMPLE.COM/Page")).expect("open");
    assert_eq!(storage.url(), Some("https://www.example.com/page"));
}

// --- TestGetHash ---------------------------------------------------------

/// 4. `_get_hash` is deterministic, injective enough, whitespace- and
///    case-insensitive, and shaped `<64 hex>_<utf8 byte length>`.
#[test]
fn identifier_hash_matches_get_hash() {
    assert_eq!(
        identifier_hash("test-identifier"),
        identifier_hash("test-identifier"),
    );
    assert_ne!(
        identifier_hash("identifier-a"),
        identifier_hash("identifier-b")
    );
    assert_eq!(identifier_hash("  Hello  "), identifier_hash("hello"));

    let hashed = identifier_hash("test");
    let (digest, length) = hashed.rsplit_once('_').expect("a `_` separator");
    assert_eq!(digest.len(), 64);
    assert_eq!(length, "4");

    // The bundled SQLite backend deliberately does *not* hash: it stores the
    // identifier raw, so a Python-written row is found by the same key.
    let db = TempDb::new("raw-identifier");
    let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");
    storage.save(&target_paragraph(), "test").expect("save");
    let stored: String = read_one(
        &storage,
        "SELECT identifier FROM storage WHERE url = 'example.com'",
    );
    assert_eq!(stored, "test");
}

// --- TestSQLiteStorageSystem --------------------------------------------

/// 5. Constructing with a real path creates the file.
#[test]
fn constructing_with_a_file_path_creates_the_file() {
    let db = TempDb::new("creates-file");
    assert!(!db.path().exists());

    let storage = SqliteStorage::open(db.path(), None).expect("open");
    assert!(db.path().exists());
    assert_eq!(storage.storage_file(), db.path());
    storage.close().expect("close");
}

#[test]
fn an_in_memory_database_reports_the_python_path() {
    let storage = SqliteStorage::in_memory(None).expect("open");
    assert_eq!(storage.storage_file().to_str(), Some(IN_MEMORY_PATH));
    assert_eq!(IN_MEMORY_PATH, ":memory:");
}

#[test]
fn the_url_argument_is_kept_verbatim_apart_from_case() {
    let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
    assert_eq!(storage.url(), Some("https://example.com"));
}

// --- TestSaveRetrieveRoundTrip ------------------------------------------

/// 6. Save/retrieve round trip: tag, attributes and text survive.
#[test]
fn save_and_retrieve() {
    let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
    let fingerprint = target_paragraph();
    storage.save(&fingerprint, "test-element").expect("save");

    let result = storage
        .retrieve("test-element")
        .expect("retrieve")
        .expect("a row");
    assert_eq!(result.tag, "p");
    assert_eq!(
        result.attributes.get("id").map(String::as_str),
        Some("target")
    );
    assert_eq!(
        result.attributes.get("class").map(String::as_str),
        Some("main")
    );
    assert_eq!(result.text.as_deref(), Some("Hello"));
    assert_eq!(result, fingerprint);
}

/// 7. An unknown identifier is `None`, not an error.
#[test]
fn retrieving_a_nonexistent_identifier_returns_none() {
    let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
    assert_eq!(storage.retrieve("does-not-exist").expect("retrieve"), None);
}

/// 8. Re-saving an identifier overwrites; `INSERT OR REPLACE` never appends.
#[test]
fn save_overwrites_existing() {
    let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");

    let mut first = target_paragraph();
    first.attributes.insert("id".to_owned(), "v1".to_owned());
    first.text = Some("First".to_owned());
    storage.save(&first, "my-element").expect("save");

    let mut second = target_paragraph();
    second.attributes.insert("id".to_owned(), "v2".to_owned());
    second.text = Some("Second".to_owned());
    storage.save(&second, "my-element").expect("save");

    let result = storage
        .retrieve("my-element")
        .expect("retrieve")
        .expect("a row");
    assert_eq!(result.attributes.get("id").map(String::as_str), Some("v2"));
    assert_eq!(result.text.as_deref(), Some("Second"));
    assert_eq!(storage.len().expect("count"), 1);
}

/// 9. Two URLs sharing one database file cannot see each other's identifiers.
#[test]
fn url_isolation() {
    let db = TempDb::new("url-isolation");

    let site_a = SqliteStorage::open(db.path(), Some("https://site-a.com")).expect("open a");
    site_a.save(&target_paragraph(), "shared-id").expect("save");

    let site_b = SqliteStorage::open(db.path(), Some("https://site-b.com")).expect("open b");
    assert_eq!(site_b.retrieve("shared-id").expect("retrieve"), None);

    site_a.close().expect("close a");
    site_b.close().expect("close b");
}

/// 10. `path` survives the tuple -> JSON array -> `Vec<String>` conversion and
///     still ends with the element's own tag.
#[test]
fn the_element_path_is_stored() {
    let storage = SqliteStorage::in_memory(Some("https://example.com")).expect("open");
    storage
        .save(&target_paragraph(), "path-test")
        .expect("save");

    let result = storage
        .retrieve("path-test")
        .expect("retrieve")
        .expect("a row");
    assert_eq!(result.path.last().map(String::as_str), Some("p"));
    assert!(result.path.contains(&"div".to_owned()));
    assert!(result.path.contains(&"body".to_owned()));
}

/// 11. `children` and `siblings` round-trip, and stay absent when empty.
#[test]
fn children_and_siblings_round_trip() {
    let db = TempDb::new("children-siblings");
    let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");

    let mut fingerprint = target_paragraph();
    fingerprint.tag = "span".to_owned();
    fingerprint.children = Some(vec!["b".to_owned(), "i".to_owned()]);
    fingerprint.siblings = Some(vec!["p".to_owned()]);
    storage.save(&fingerprint, "with-children").expect("save");

    let result = storage
        .retrieve("with-children")
        .expect("retrieve")
        .expect("a row");
    let children = result.children.as_ref().expect("children");
    assert!(children.contains(&"b".to_owned()));
    assert!(children.contains(&"i".to_owned()));
    assert!(result
        .siblings
        .as_ref()
        .expect("siblings")
        .contains(&"p".to_owned()));

    // An element with neither must omit both keys rather than store `null`s.
    let bare = ElementFingerprint {
        tag: "p".to_owned(),
        ..ElementFingerprint::default()
    };
    storage.save(&bare, "bare").expect("save");
    let json = String::from_utf8(read_one::<Vec<u8>>(
        &storage,
        "SELECT element_data FROM storage WHERE identifier = 'bare'",
    ))
    .expect("utf-8");
    assert!(!json.contains("children"), "{json}");
    assert!(!json.contains("siblings"), "{json}");
    assert_eq!(storage.retrieve("bare").expect("retrieve"), Some(bare));
}

// --- schema and on-disk compatibility ------------------------------------

#[test]
fn the_schema_is_the_python_schema() {
    // The exact statement text, so a `.db` file created here is the one Python
    // would have created.
    assert!(SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS storage"));
    assert!(SCHEMA_SQL.contains("id INTEGER PRIMARY KEY"));
    assert!(SCHEMA_SQL.contains("url TEXT"));
    assert!(SCHEMA_SQL.contains("identifier TEXT"));
    assert!(SCHEMA_SQL.contains("element_data TEXT"));
    assert!(SCHEMA_SQL.contains("UNIQUE (url, identifier)"));

    let db = TempDb::new("schema");
    let storage = SqliteStorage::open(db.path(), None).expect("open");

    let sql: String = read_one(
        &storage,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'storage'",
    );
    assert!(sql.contains("UNIQUE (url, identifier)"), "{sql}");

    let index_count: i64 = read_one(
        &storage,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'storage'",
    );
    assert_eq!(index_count, 1, "the UNIQUE constraint's implicit index");

    storage.close().expect("close");
}

#[test]
fn a_file_backed_database_is_journalled_with_wal() {
    let db = TempDb::new("wal");
    let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");
    storage.save(&target_paragraph(), "wal").expect("save");

    let mode: String = read_one(&storage, "PRAGMA journal_mode");
    assert_eq!(mode, "wal");

    // WAL is a property of the file, so a second connection sees it too.
    let again = SqliteStorage::open(db.path(), Some("https://example.com")).expect("reopen");
    assert_eq!(read_one::<String>(&again, "PRAGMA journal_mode"), "wal");

    storage.close().expect("close");
    again.close().expect("close");
}

#[test]
fn the_url_column_holds_the_namespace_not_the_url() {
    let db = TempDb::new("url-column");
    let storage = SqliteStorage::open(db.path(), Some("https://www.example.com/deep/page?a=b"))
        .expect("open");
    storage.save(&target_paragraph(), "id").expect("save");

    let stored: String = read_one(&storage, "SELECT url FROM storage");
    assert_eq!(stored, "example.com");
}

#[test]
fn a_database_written_here_is_shaped_like_pythons() {
    let db = TempDb::new("python-shape");
    let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");
    storage.save(&target_paragraph(), "p.price").expect("save");
    storage.close().expect("close");

    // Re-open with a bare `rusqlite` connection, the way another process (or
    // Python) would.
    let raw = rusqlite::Connection::open(db.path()).expect("open raw");
    // `element_data` is a BLOB, as it is in every file Python writes.
    let (url, identifier, data): (String, String, String) = raw
        .query_row(
            "SELECT url, identifier, element_data FROM storage",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, Vec<u8>>(2)?)),
        )
        .map(|(url, identifier, bytes)| (url, identifier, String::from_utf8(bytes).expect("utf-8")))
        .expect("row");
    assert_eq!(url, "example.com");
    assert_eq!(identifier, "p.price");

    let parsed: serde_json::Value = serde_json::from_str(&data).expect("json");
    assert_eq!(parsed["tag"], "p");
    assert_eq!(parsed["attributes"]["id"], "target");
    assert_eq!(parsed["text"], "Hello");
    assert_eq!(parsed["path"][3], "p");
    // Compact, orjson-style separators.
    assert!(!data.contains(", "), "{data}");
    assert!(!data.contains(": "), "{data}");

    let bytes = fs::metadata(db.path()).expect("metadata").len();
    assert!(bytes > 0);
}

/// `registrable_domain` is reachable without constructing a storage, because
/// the spider layer needs the same namespace key.
#[test]
fn the_namespace_helper_is_public() {
    assert_eq!(
        registrable_domain(Some("https://www.example.com/page")),
        "example.com"
    );
    assert_eq!(registrable_domain(None), DEFAULT_BASE_URL);
}

/// Reads a single value out of a storage's database, through a *separate*
/// connection — the way Python, or another process, would see the file.
///
/// `SqliteStorage` exposes no raw connection on purpose, so this only works for
/// file-backed storages; an in-memory database is private to its connection.
fn read_one<T: rusqlite::types::FromSql>(storage: &SqliteStorage, sql: &str) -> T {
    let path = storage.storage_file();
    assert_ne!(
        path.to_str(),
        Some(IN_MEMORY_PATH),
        "read_one() needs a file-backed storage",
    );
    let connection = rusqlite::Connection::open(path).expect("open raw connection");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("query")
}
