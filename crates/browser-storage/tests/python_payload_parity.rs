//! Byte-for-byte parity on *real* fingerprints, over a database the reference implementation wrote.
//!
//! The other differential tests use fingerprints assembled by hand. This one
//! uses 81 rows that the reference implementation 0.4.15 produced end to end under CPython 3.14:
//! twelve HTML documents parsed by `lxml.html`, every element walked with
//! `tree.iter()`, each one passed through `_StorageTools.element_to_dict`,
//! `orjson.dumps` and `SQLiteStorageSystem.save` into
//! `testdata/python_payloads.db`. `testdata/python_payloads.jsonl` records the
//! exact bytes each row holds, alongside the URL the storage instance was
//! constructed with and the namespace Python derived from it.
//!
//! Both directions of the format are checked against those bytes:
//!
//! * **read** — every row deserialises into an [`ElementFingerprint`];
//! * **write** — re-encoding that fingerprint through [`python_json::to_string`]
//!   reproduces `orjson`'s output *character for character*, and saving it
//!   through [`SqliteStorage::save`] puts those same bytes back in the column.
//!
//! The documents are chosen to break an encoder rather than to flatter it:
//! attribute values holding tabs, newlines, quotes, backslashes, emoji and RTL
//! text; a `U+007F` control character in element text; an IDN namespace
//! (`例え.jp`); a 2000-character text node; twelve levels of nesting; void
//! elements, `<script>` with `</p>` inside a string, inline SVG and MathML,
//! unclosed tags recovered by libxml2, and a document with no URL at all.
//!
//! # The one thing Python cannot round-trip
//!
//! `testdata/python_payload_divergences.jsonl` lists four elements that
//! the reference implementation itself **fails** to store: `element_to_dict` puts `child.tag` into
//! `siblings`, and for an `lxml` comment or processing instruction that `.tag`
//! is a *Cython function object*, which `orjson.dumps` rejects with
//! `TypeError: Type is not JSON serializable`. Any element with a comment
//! sibling — and every comment node itself — therefore raises inside
//! `save()` on the Python side. Rust has no such hole, so those rows exist in
//! the corpus as recorded failures rather than as expectations; see
//! [`what_python_cannot_store`].
//!
//! [`python_json::to_string`]: browser_storage::python_json::to_string

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;
use browser_core::ElementFingerprint;
use browser_storage::base_url::registrable_domain;
use browser_storage::python_json;
use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::SqliteStorage;

use common::TempDb;

/// One line of `testdata/python_payloads.jsonl`.
struct Row {
    /// Which of the twelve source documents the element came from.
    document: String,
    /// The URL `SQLiteStorageSystem(...)` was constructed with; `None` for the
    /// document stored without one.
    source_url: Option<String>,
    /// The namespace Python's `_get_base_url()` derived from `source_url`.
    base_url: String,
    /// The identifier the row is filed under.
    identifier: String,
    /// `orjson.dumps(element_to_dict(element))`, decoded as UTF-8.
    element_data: String,
    /// `typeof(element_data)` as SQLite reports it — always `"blob"`.
    typeof_column: String,
}

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

fn corpus() -> Vec<Row> {
    let path = testdata("python_payloads.jsonl");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("bad corpus line: {e}"));
            let string = |key: &str| -> String {
                value[key]
                    .as_str()
                    .unwrap_or_else(|| panic!("`{key}` missing from {line}"))
                    .to_owned()
            };
            Row {
                document: string("document"),
                source_url: value["source_url"].as_str().map(str::to_owned),
                base_url: string("base_url"),
                identifier: string("identifier"),
                element_data: string("element_data"),
                typeof_column: string("element_data_typeof"),
            }
        })
        .collect()
}

/// Copies the Python-written fixture somewhere writable.
///
/// Opening it switches the journal to WAL and drops `-wal`/`-shm` companions
/// beside it, so the committed artefact is never opened in place.
fn python_database(label: &str) -> TempDb {
    let db = TempDb::new(label);
    let fixture = testdata("python_payloads.db");
    fs::copy(&fixture, db.path())
        .unwrap_or_else(|e| panic!("cannot copy {}: {e}", fixture.display()));
    db
}

/// The corpus must stay big and varied; a future edit that quietly shrinks it
/// would weaken every test in this file.
#[test]
fn the_corpus_covers_every_document() {
    let rows = corpus();
    assert_eq!(rows.len(), 81, "the corpus lost rows");

    let documents: BTreeMap<&str, usize> = rows.iter().fold(BTreeMap::new(), |mut acc, row| {
        *acc.entry(row.document.as_str()).or_default() += 1;
        acc
    });
    for expected in [
        "attrs",
        "comments",
        "entities",
        "longtext",
        "malformed",
        "nested",
        "plain",
        "script",
        "svg",
        "table",
        "voids",
        "whitespace",
    ] {
        assert!(
            documents.contains_key(expected),
            "document `{expected}` vanished from the corpus: {documents:?}",
        );
    }

    // Every row is a BLOB, because `orjson.dumps` hands `sqlite3` `bytes` and a
    // `TEXT`-affinity column keeps a blob a blob.
    assert!(rows.iter().all(|row| row.typeof_column == "blob"));
}

/// Python's namespace for each source URL is the one this crate computes.
#[test]
fn every_namespace_python_derived_is_reproduced() {
    for row in corpus() {
        assert_eq!(
            registrable_domain(row.source_url.as_deref()),
            row.base_url,
            "namespace mismatch for {:?}",
            row.source_url,
        );
    }
}

/// Every row the reference implementation wrote is readable here, under the URL the reference implementation used.
#[test]
fn every_python_row_is_retrievable_by_its_url() {
    let db = python_database("payload-read");

    for row in corpus() {
        let storage = SqliteStorage::open(db.path(), row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open: {e}"));
        assert_eq!(storage.base_url(), row.base_url);

        let stored = storage
            .retrieve(&row.identifier)
            .unwrap_or_else(|e| panic!("retrieve {}: {e}", row.identifier))
            .unwrap_or_else(|| panic!("no row for {} / {}", row.base_url, row.identifier));

        // The fingerprint's own invariants, as `element_to_dict` guarantees
        // them: the path ends with the element's tag, and a stored `siblings`
        // or `children` list is never empty (Python guards both with `if`).
        assert_eq!(
            stored.path.last().map(String::as_str),
            Some(stored.tag.as_str()),
            "{}: path does not end at the element",
            row.identifier,
        );
        assert!(stored.siblings.as_ref().is_none_or(|s| !s.is_empty()));
        assert!(stored.children.as_ref().is_none_or(|c| !c.is_empty()));
    }
}

/// Re-encoding a row Python wrote reproduces `orjson`'s bytes exactly.
///
/// This is the check that would catch a key in the wrong order, an omitted
/// `null`, a `[]` written where Python omits the key, or an escape sequence
/// `serde_json` renders differently from `orjson`.
#[test]
fn re_encoding_every_python_row_reproduces_orjsons_bytes() {
    let db = python_database("payload-encode");

    for row in corpus() {
        let storage = SqliteStorage::open(db.path(), row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open: {e}"));
        let stored = storage
            .retrieve(&row.identifier)
            .unwrap_or_else(|e| panic!("retrieve: {e}"))
            .unwrap_or_else(|| panic!("no row for {}", row.identifier));

        let encoded = python_json::to_string(&stored).unwrap_or_else(|e| panic!("encode: {e}"));
        assert_eq!(
            encoded, row.element_data,
            "{} re-encoded differently from orjson",
            row.identifier,
        );
    }
}

/// Saving a Python row back through this crate leaves the column byte-identical.
///
/// The previous test exercises the encoder; this one exercises the *writer* —
/// binding, affinity and all — by reading the raw column afterwards.
#[test]
fn re_saving_a_python_row_leaves_the_bytes_untouched() {
    let db = python_database("payload-rewrite");
    let rows = corpus();

    for row in &rows {
        let storage = SqliteStorage::open(db.path(), row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open: {e}"));
        let stored = storage
            .retrieve(&row.identifier)
            .unwrap_or_else(|e| panic!("retrieve: {e}"))
            .unwrap_or_else(|| panic!("no row for {}", row.identifier));
        storage
            .save(&stored, &row.identifier)
            .unwrap_or_else(|e| panic!("save: {e}"));
    }

    let connection = Connection::open(db.path()).expect("reopen");
    for row in &rows {
        let bytes: Vec<u8> = connection
            .query_row(
                "SELECT element_data FROM storage WHERE url = ? AND identifier = ?",
                rusqlite::params![&row.base_url, &row.identifier],
                |sqlite_row| sqlite_row.get(0),
            )
            .unwrap_or_else(|e| panic!("select {}: {e}", row.identifier));
        assert_eq!(
            String::from_utf8(bytes).expect("utf-8"),
            row.element_data,
            "{} changed when it was written back",
            row.identifier,
        );
    }
}

/// A round trip through a *fresh* database keeps the fingerprint identical.
///
/// Reading Python's row, writing it into an empty file of this crate's own
/// making and reading it back must be the identity — otherwise a fingerprint
/// degrades every time a spider re-learns it.
#[test]
fn a_fresh_database_round_trips_every_fingerprint() {
    let source = python_database("payload-source");
    let target = TempDb::new("payload-target");

    for row in corpus() {
        let python_side = SqliteStorage::open(source.path(), row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open source: {e}"));
        let stored = python_side
            .retrieve(&row.identifier)
            .unwrap_or_else(|e| panic!("retrieve: {e}"))
            .unwrap_or_else(|| panic!("no row for {}", row.identifier));

        let rust_side = SqliteStorage::open(target.path(), row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open target: {e}"));
        rust_side
            .save(&stored, &row.identifier)
            .unwrap_or_else(|e| panic!("save: {e}"));
        let reread = rust_side
            .retrieve(&row.identifier)
            .unwrap_or_else(|e| panic!("re-retrieve: {e}"))
            .unwrap_or_else(|| panic!("row vanished: {}", row.identifier));

        assert_eq!(
            reread, stored,
            "{} did not survive a round trip",
            row.identifier
        );
    }
}

/// The four elements the reference implementation cannot store — and which this crate stores fine.
///
/// `testdata/python_payload_divergences.jsonl` is generated, not written by
/// hand: the generator records every element whose `orjson.dumps` raised. The
/// test pins *why* they fail and shows that the same fingerprint, once the
/// comment sibling is spelled as a string, goes through here without trouble.
/// This is the single deliberate behavioural divergence in the payload path:
/// Rust can write rows Python cannot, never the other way round.
#[test]
fn what_python_cannot_store() {
    let path = testdata("python_payload_divergences.jsonl");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let recorded: Vec<serde_json::Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("divergence line"))
        .collect();

    assert_eq!(recorded.len(), 4, "the divergence list changed");
    for row in &recorded {
        assert_eq!(row["document"], "comments");
        assert_eq!(row["stage"], "orjson.dumps");
        assert!(
            row["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("TypeError: Type is not JSON serializable")),
            "unexpected failure: {row}",
        );
    }

    // Two of the four are the comment nodes themselves, whose `tag` is the
    // Cython sentinel; the other two are ordinary `<p>` elements that merely
    // have a comment among their siblings.
    let tags: Vec<&str> = recorded
        .iter()
        .filter_map(|row| row["tag"].as_str())
        .collect();
    assert_eq!(
        tags,
        ["p", "<cyfunction Comment>", "p", "<cyfunction Comment>"]
    );

    // The same shape, with the sentinel rendered as a string, stores and
    // retrieves here without a murmur.
    let db = TempDb::new("comment-siblings");
    let storage = SqliteStorage::open(db.path(), Some("https://comments.example.com"))
        .unwrap_or_else(|e| panic!("open: {e}"));
    let fingerprint = ElementFingerprint {
        tag: "p".to_owned(),
        text: Some("a".to_owned()),
        path: vec!["html".to_owned(), "body".to_owned(), "p".to_owned()],
        parent_name: Some("div".to_owned()),
        parent_attribs: Some(Default::default()),
        parent_text: None,
        siblings: Some(vec!["comment".to_owned(), "p".to_owned()]),
        ..Default::default()
    };
    storage
        .save(&fingerprint, "with-comment-sibling")
        .expect("save");
    assert_eq!(
        storage.retrieve("with-comment-sibling").expect("retrieve"),
        Some(fingerprint),
    );
}

/// Writes `<target-tmp>/payload-roundtrip/rust_payloads.db` for
/// `tools/verify_rust_payloads.py`.
///
/// The other direction of the round trip: every one of the 81 fingerprints is
/// read out of Python's database and written into a *fresh* one by this crate
/// alone, under the same URLs. Cargo cannot run CPython, so the check itself
/// lives in the companion script, which opens the file with the real
/// `SQLiteStorageSystem` and asserts that
///
/// * `retrieve()` returns a dict for every (url, identifier) pair, and
/// * the raw column bytes are `orjson`'s original bytes, unchanged.
///
/// ```text
/// cargo test -p browser-storage --test python_payload_parity -- writes_a_database
/// python crates/browser-storage/tools/verify_rust_payloads.py
/// ```
#[test]
fn writes_a_database_python_can_read() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("payload-roundtrip");
    fs::create_dir_all(&dir).expect("create target dir");
    let path = dir.join("rust_payloads.db");
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.clone().into_os_string();
        candidate.push(suffix);
        let _ = fs::remove_file(PathBuf::from(candidate));
    }

    let source = python_database("payload-mirror");
    for row in corpus() {
        let python_side = SqliteStorage::open(source.path(), row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open source: {e}"));
        let fingerprint = python_side
            .retrieve(&row.identifier)
            .unwrap_or_else(|e| panic!("retrieve: {e}"))
            .unwrap_or_else(|| panic!("no row for {}", row.identifier));

        let rust_side = SqliteStorage::open(&path, row.source_url.as_deref())
            .unwrap_or_else(|e| panic!("open target: {e}"));
        rust_side
            .save(&fingerprint, &row.identifier)
            .unwrap_or_else(|e| panic!("save: {e}"));
    }

    // Fold the WAL back in so the artefact stands on its own.
    let connection = Connection::open(&path).expect("reopen");
    connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("checkpoint");

    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM storage", [], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, 81);
}
