//! Reads a database written by the *Python* implementation.
//!
//! `testdata/python_elements_storage.db` was produced by
//! `tools/gen_python_db.py`, which drives the reference implementation 0.4.15's own
//! `SQLiteStorageSystem` over real `lxml` elements under CPython 3.14. Nothing
//! in it was written by this crate.
//!
//! It exercises the three things that make the format shared rather than merely
//! similar:
//!
//! * `element_data` is a **BLOB** — `orjson.dumps` returns `bytes`, and SQLite
//!   keeps the storage class even though the column is declared `TEXT`;
//! * the `url` column holds `tld`'s registrable domain, including `例え.jp`
//!   *un*-punycoded, which is why [`base_url`] parses hosts the way CPython's
//!   `urlsplit` does rather than the way the WHATWG URL parser does;
//! * the fingerprint JSON omits absent keys instead of writing `null`s, and
//!   [`ElementFingerprint`] has to deserialise it as-is.
//!
//! [`base_url`]: browser_storage::base_url

mod common;

use std::fs;
use std::path::PathBuf;

use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::SqliteStorage;

use common::TempDb;

/// Copies the read-only fixture to a scratch path.
///
/// Opening a `SqliteStorage` switches the file to WAL, which mutates it and
/// spills `-wal`/`-shm` companions next to it, so the committed artefact is
/// never opened in place.
fn python_database(label: &str) -> TempDb {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/python_elements_storage.db");
    let db = TempDb::new(label);
    fs::copy(&fixture, db.path())
        .unwrap_or_else(|e| panic!("cannot copy {}: {e}", fixture.display()));
    db
}

#[test]
fn reads_a_fingerprint_python_wrote() {
    let db = python_database("py-read");
    let storage =
        SqliteStorage::open(db.path(), Some("https://www.example.com/page")).expect("open");
    assert_eq!(storage.base_url(), "example.com");

    let stored = storage
        .retrieve("p.main")
        .expect("retrieve")
        .expect("a row");
    assert_eq!(stored.tag, "p");
    assert_eq!(
        stored.attributes.get("id").map(String::as_str),
        Some("target")
    );
    assert_eq!(
        stored.attributes.get("class").map(String::as_str),
        Some("main")
    );
    assert_eq!(stored.text.as_deref(), Some("Hello"));
    assert_eq!(stored.path, ["html", "body", "div", "p"]);
    assert_eq!(stored.parent_name.as_deref(), Some("div"));
    assert_eq!(stored.parent_text.as_deref(), Some("Prefix"));
    assert_eq!(
        stored
            .parent_attribs
            .as_ref()
            .and_then(|attrs| attrs.get("class"))
            .map(String::as_str),
        Some("wrap"),
    );
    // Absent in the JSON, so `None` here — not an empty vector.
    assert_eq!(stored.siblings, None);
    assert_eq!(stored.children, None);
}

#[test]
fn reads_siblings_and_children_python_wrote() {
    let db = python_database("py-children");
    let storage =
        SqliteStorage::open(db.path(), Some("https://www.example.com/page")).expect("open");

    let stored = storage
        .retrieve("span#target")
        .expect("retrieve")
        .expect("a row");
    assert_eq!(stored.tag, "span");
    assert_eq!(stored.text, None);
    assert_eq!(
        stored.children.as_deref(),
        Some(["b".to_owned(), "i".to_owned()].as_slice())
    );
    assert_eq!(
        stored.siblings.as_deref(),
        Some(["p".to_owned()].as_slice())
    );
}

#[test]
fn whitespace_only_attributes_were_already_dropped_by_python() {
    let db = python_database("py-attrs");
    let storage =
        SqliteStorage::open(db.path(), Some("https://www.example.com/page")).expect("open");

    let stored = storage
        .retrieve("p[data-val]")
        .expect("retrieve")
        .expect("a row");
    assert!(!stored.attributes.contains_key("data-val"));
    assert_eq!(
        stored.attributes.get("data-keep").map(String::as_str),
        Some("kept"),
    );
}

#[test]
fn the_namespaces_python_wrote_are_reachable_from_the_matching_urls() {
    // Same file, four namespaces, each only visible from its own URL.
    let db = python_database("py-namespaces");

    let example =
        SqliteStorage::open(db.path(), Some("https://www.example.com/page")).expect("example");
    let site_b = SqliteStorage::open(db.path(), Some("https://site-b.com/x")).expect("site-b");
    let anonymous = SqliteStorage::open(db.path(), None).expect("anonymous");

    assert!(example.retrieve("p.main").expect("retrieve").is_some());
    assert!(site_b.retrieve("p.main").expect("retrieve").is_some());
    assert_eq!(
        site_b
            .retrieve("p.main")
            .expect("retrieve")
            .and_then(|fp| fp.attributes.get("id").cloned())
            .as_deref(),
        Some("other-site"),
    );

    assert_eq!(example.retrieve("no-url").expect("retrieve"), None);
    assert!(anonymous.retrieve("no-url").expect("retrieve").is_some());
    assert_eq!(anonymous.retrieve("p.main").expect("retrieve"), None);
}

#[test]
fn an_idn_namespace_matches_without_punycoding() {
    let db = python_database("py-idn");
    let storage = SqliteStorage::open(db.path(), Some("https://例え.jp/")).expect("open");
    assert_eq!(storage.base_url(), "例え.jp");

    let stored = storage
        .retrieve("例::text")
        .expect("retrieve")
        .expect("a row");
    assert_eq!(stored.text.as_deref(), Some("こんにちは"));
    assert_eq!(
        stored.attributes.get("class").map(String::as_str),
        Some("中文"),
    );
}

#[test]
fn writing_into_a_python_database_keeps_it_readable_by_python() {
    let db = python_database("py-write-back");
    let storage =
        SqliteStorage::open(db.path(), Some("https://www.example.com/page")).expect("open");

    // Overwrite one of Python's rows and add one of our own.
    let mut updated = storage
        .retrieve("p.main")
        .expect("retrieve")
        .expect("a row");
    updated.text = Some("Rewritten".to_owned());
    storage.save(&updated, "p.main").expect("save");
    storage.save(&updated, "written-by-rust").expect("save");
    storage.close().expect("close");

    // Inspect the file the way another process would.
    let connection = rusqlite::Connection::open(db.path()).expect("open raw");
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'storage'",
            [],
            |row| row.get(0),
        )
        .expect("schema");
    assert!(schema.contains("UNIQUE (url, identifier)"), "{schema}");

    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM storage", [], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, 7, "one row replaced, one added");

    // The rewritten row is a BLOB, like every row Python wrote: `save` binds
    // the encoded fingerprint as bytes precisely so that a row this crate
    // rewrites is indistinguishable from the one it replaced.
    let (kind, bytes): (String, Vec<u8>) = connection
        .query_row(
            "SELECT typeof(element_data), element_data FROM storage
             WHERE url = 'example.com' AND identifier = 'p.main'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row");
    assert_eq!(kind, "blob");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(parsed["text"], "Rewritten");
    assert_eq!(parsed["parent_attribs"]["class"], "wrap");

    // Python's untouched rows are still BLOBs.
    let untouched: String = connection
        .query_row(
            "SELECT typeof(element_data) FROM storage
             WHERE url = 'example.com' AND identifier = 'span#target'",
            [],
            |row| row.get(0),
        )
        .expect("row");
    assert_eq!(untouched, "blob");
}

/// An empty database this crate creates is byte-identical to the reference implementation's.
///
/// `testdata/python_empty_storage.db` is what `SQLiteStorageSystem.__init__`
/// leaves behind on its own: connect, `PRAGMA journal_mode=WAL`, `CREATE TABLE
/// IF NOT EXISTS storage (…)`, commit, close. No rows, so every one of its
/// 12 288 bytes is schema — page size, text encoding, the table's b-tree, the
/// `sqlite_autoindex_storage_1` the `UNIQUE` constraint creates, and the
/// verbatim `CREATE TABLE` text SQLite stores in `sqlite_master`.
///
/// Comparing the files rather than the `.schema` string is what makes this
/// worth having: `SCHEMA_SQL` could match character for character and the file
/// still differ in journal mode, page size or encoding, any of which another
/// tool would notice.
///
/// The **first 100 bytes** are excluded, and only those: that is SQLite's file
/// header, which carries the change counter, the page count and the version of
/// the library that last wrote the file. In practice exactly one byte inside it
/// differs here — the last byte of `SQLITE_VERSION_NUMBER` at offset 96,
/// because CPython links a slightly different SQLite patch release than
/// `rusqlite` does. Nothing outside the header differs at all.
#[test]
fn an_empty_database_is_byte_identical_to_pythons() {
    let db = TempDb::new("empty-shape");
    SqliteStorage::open(db.path(), Some("https://example.com"))
        .expect("open")
        .close()
        .expect("close");

    let ours = fs::read(db.path()).expect("read ours");
    let theirs = fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/python_empty_storage.db"),
    )
    .expect("read the fixture");

    assert_eq!(
        ours.len(),
        theirs.len(),
        "different file sizes: SQLite's default page size has changed on one \
         side, which is not a compatibility problem but does invalidate this \
         byte-level comparison",
    );

    // Everything after SQLite's 100-byte file header.
    const HEADER: usize = 100;
    let differing: Vec<usize> = (HEADER..ours.len())
        .filter(|&i| ours[i] != theirs[i])
        .collect();
    assert!(
        differing.is_empty(),
        "{} bytes outside the file header differ, first at offset {:?}",
        differing.len(),
        differing.first(),
    );

    // And within the header, only the library-version stamp may move.
    let header_differences: Vec<usize> = (0..HEADER).filter(|&i| ours[i] != theirs[i]).collect();
    assert!(
        header_differences
            .iter()
            .all(|&offset| (96..100).contains(&offset)),
        "the file header differs outside SQLITE_VERSION_NUMBER: {header_differences:?}",
    );
}
