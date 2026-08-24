//! Adversarial differential tests against the reference implementation 0.4.15 running on CPython 3.14.
//!
//! Three goldens, all produced by driving the *Python* library
//! (`tools/gen_storage_goldens.py`):
//!
//! | file | proves |
//! |---|---|
//! | `testdata/python_schema.json` | the on-disk schema this crate creates is *textually* the one `SQLiteStorageSystem._setup_database` creates — `sqlite_master.sql` byte for byte, plus `table_info`, the implicit `UNIQUE` index, and the file-level pragmas |
//! | `testdata/python_hashes.jsonl` | `identifier_hash` == `StorageSystemMixin._get_hash` over 60 identifiers chosen to break it (Unicode case folding, final sigma, `İ`, and the whitespace classes where Python's `str.strip()` and Rust's `str::trim()` disagree) |
//! | `testdata/python_adversarial_storage.db` + `…_rows.jsonl` | a database written by Python is readable here: every namespace, every `element_data` BLOB, `INSERT OR REPLACE` de-duplication and the omit-absent-keys JSON shape |
//!
//! The reverse direction — a database written *here*, read by Python — is
//! covered by [`writes_a_database_python_can_read`], which emits the database
//! and an expectations file for `tools/verify_rust_db.py` to check against the
//! real `SQLiteStorageSystem`. That script does one thing this file cannot:
//! it pushes each retrieved row through
//! `Selector.__calculate_similarity_score`, which subscripts
//! `original["parent_attribs"]` and `original["parent_text"]` directly. A row
//! missing either loads fine and only fails there, which is how the
//! `python_json` encoder came to exist.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use browser_core::fingerprint::AttributeDict;
use browser_core::ElementFingerprint;
use browser_storage::hash::identifier_hash;
use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::{
    SqliteStorage, INSERT_SQL, JOURNAL_MODE_SQL, SCHEMA_SQL, SELECT_SQL,
};

use common::TempDb;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

fn read_golden(name: &str) -> String {
    let path = testdata(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn jsonl(name: &str) -> Vec<serde_json::Value> {
    read_golden(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(lineno, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}:{}: {e}", lineno + 1))
        })
        .collect()
}

// --------------------------------------------------------------------------- //
// 1. schema text
// --------------------------------------------------------------------------- //

/// The SQL text this crate sends is the SQL text Python sends.
///
/// Not a nicety: `sqlite_master.sql` is stored verbatim, so a mismatch in a
/// single space produces a file whose schema *reads* differently to anyone
/// diffing two databases, and any change to the column list or to
/// `UNIQUE (url, identifier)` silently changes which rows overwrite which.
#[test]
fn the_sql_statements_are_pythons_character_for_character() {
    let schema: serde_json::Value =
        serde_json::from_str(&read_golden("python_schema.json")).expect("golden JSON");

    assert_eq!(
        SCHEMA_SQL,
        schema["schema_sql_literal"].as_str().expect("literal"),
        "SCHEMA_SQL drifted from _setup_database's argument",
    );
    assert_eq!(
        INSERT_SQL,
        schema["insert_sql_literal"].as_str().expect("literal"),
        "INSERT_SQL drifted from save()'s statement",
    );
    assert_eq!(
        SELECT_SQL,
        schema["select_sql_literal"].as_str().expect("literal"),
        "SELECT_SQL drifted from retrieve()'s statement",
    );
    assert_eq!(JOURNAL_MODE_SQL, "PRAGMA journal_mode=WAL");
}

/// Everything SQLite itself records about the database Python creates, compared
/// against the database this crate creates.
#[test]
fn the_created_database_is_structurally_identical_to_pythons() {
    let expected: serde_json::Value =
        serde_json::from_str(&read_golden("python_schema.json")).expect("golden JSON");

    let db = TempDb::new("schema-parity");
    let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");
    drop(storage);

    let connection = Connection::open(db.path()).expect("reopen");

    // `sqlite_master`, including the CREATE TABLE text verbatim.
    let mut statement = connection
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .expect("prepare");
    let master: Vec<serde_json::Value> = statement
        .query_map([], |row| {
            Ok(serde_json::json!({
                "type": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "tbl_name": row.get::<_, String>(2)?,
                "sql": row.get::<_, Option<String>>(3)?,
            }))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        serde_json::Value::Array(master),
        expected["sqlite_master"],
        "sqlite_master differs from the one Python's _setup_database produces",
    );

    // Column declarations.
    let mut statement = connection
        .prepare("PRAGMA table_info(storage)")
        .expect("prepare");
    let columns: Vec<serde_json::Value> = statement
        .query_map([], |row| {
            Ok(serde_json::json!({
                "cid": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "notnull": row.get::<_, i64>(3)?,
                "dflt_value": row.get::<_, Option<String>>(4)?,
                "pk": row.get::<_, i64>(5)?,
            }))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(serde_json::Value::Array(columns), expected["table_info"]);

    // The implicit index behind `UNIQUE (url, identifier)`, and its column order
    // — which is what makes `WHERE url = ? AND identifier = ?` a lookup rather
    // than a scan.
    let mut statement = connection
        .prepare("PRAGMA index_list(storage)")
        .expect("prepare");
    let indexes: Vec<serde_json::Value> = statement
        .query_map([], |row| {
            Ok(serde_json::json!({
                "seq": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "unique": row.get::<_, i64>(2)?,
                "origin": row.get::<_, String>(3)?,
                "partial": row.get::<_, i64>(4)?,
            }))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        serde_json::Value::Array(indexes.clone()),
        expected["index_list"],
    );

    for index in &indexes {
        let name = index["name"].as_str().expect("index name");
        let mut statement = connection
            .prepare(&format!("PRAGMA index_info(\"{name}\")"))
            .expect("prepare");
        let info: Vec<serde_json::Value> = statement
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "seqno": row.get::<_, i64>(0)?,
                    "cid": row.get::<_, i64>(1)?,
                    "name": row.get::<_, Option<String>>(2)?,
                }))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(serde_json::Value::Array(info), expected["index_info"][name]);
    }

    // File-level pragmas.
    for (pragma, key) in [
        ("PRAGMA journal_mode", "journal_mode"),
        ("PRAGMA encoding", "encoding"),
    ] {
        let value: String = connection
            .query_row(pragma, [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(
            serde_json::Value::String(value),
            expected[key],
            "{pragma} differs",
        );
    }
    for (pragma, key) in [
        ("PRAGMA user_version", "user_version"),
        ("PRAGMA application_id", "application_id"),
    ] {
        let value: i64 = connection
            .query_row(pragma, [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(
            serde_json::Value::from(value),
            expected[key],
            "{pragma} differs",
        );
    }
}

// --------------------------------------------------------------------------- //
// 2. identifier hashes
// --------------------------------------------------------------------------- //

/// `identifier_hash` against `StorageSystemMixin._get_hash` on 60 identifiers.
///
/// The corpus is built to expose the two places the port can silently drift:
/// the *order* of `.lower().strip()` (which matters for `İ`, whose lowercase is
/// two code points), and the difference between the character set Python's
/// `str.strip()` treats as whitespace and the one Rust's `str::trim()` does.
#[test]
fn identifier_hash_matches_get_hash_on_every_case() {
    let cases = jsonl("python_hashes.jsonl");
    assert!(cases.len() >= 60, "the corpus lost cases: {}", cases.len());

    let mut failures = Vec::new();
    for case in &cases {
        let identifier = case["identifier"].as_str().expect("identifier");
        let expected = case["hash"].as_str().expect("hash");
        let got = identifier_hash(identifier);
        if got != expected {
            failures.push(format!(
                "  {identifier:?}\n    (python folds it to {:?}, {} bytes)\n    rust   = {got}\n    python = {expected}",
                case["lowered_then_stripped"].as_str().unwrap_or(""),
                case["byte_len"],
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} identifiers hash differently:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n"),
    );
}

/// The byte-length suffix counts the *post-fold* UTF-8 bytes, and the golden
/// records that count independently, so an accidental `chars().count()` is
/// caught even if the digest somehow still matched.
#[test]
fn the_hash_length_suffix_counts_post_fold_utf8_bytes() {
    for case in jsonl("python_hashes.jsonl") {
        let identifier = case["identifier"].as_str().expect("identifier");
        let expected_len = case["byte_len"].as_u64().expect("byte_len");
        let hashed = identifier_hash(identifier);
        let (_, suffix) = hashed.rsplit_once('_').expect("length suffix");
        assert_eq!(
            suffix.parse::<u64>().expect("numeric suffix"),
            expected_len,
            "length suffix for {identifier:?}",
        );
    }
}

// --------------------------------------------------------------------------- //
// 3. a database written by Python, read here
// --------------------------------------------------------------------------- //

/// Copies the read-only fixture: opening it would switch it to WAL and drop
/// `-wal`/`-shm` files next to the committed artefact.
fn python_database(label: &str) -> TempDb {
    let db = TempDb::new(label);
    let fixture = testdata("python_adversarial_storage.db");
    fs::copy(&fixture, db.path())
        .unwrap_or_else(|e| panic!("cannot copy {}: {e}", fixture.display()));
    db
}

/// Turns the golden's decoded `element_data` back into an [`ElementFingerprint`],
/// so the assertion compares whole values rather than a handful of fields.
fn fingerprint_from_golden(data: &serde_json::Value) -> ElementFingerprint {
    serde_json::from_value(data.clone()).expect("the golden JSON is an ElementFingerprint")
}

#[test]
fn every_row_python_wrote_is_retrievable_here() {
    let db = python_database("py-adversarial");
    let rows = jsonl("python_adversarial_rows.jsonl");
    assert!(rows.len() >= 14, "the corpus lost rows: {}", rows.len());

    for row in &rows {
        let source_url = row["source_url"].as_str();
        let namespace = row["url"].as_str().expect("url column");
        let identifier = row["identifier"].as_str().expect("identifier column");

        let storage = SqliteStorage::open(db.path(), source_url).expect("open");
        assert_eq!(
            storage.base_url(),
            namespace,
            "the namespace this crate derives from {source_url:?} is not the one Python wrote",
        );

        let stored = storage
            .retrieve(identifier)
            .expect("retrieve")
            .unwrap_or_else(|| panic!("no row for ({namespace:?}, {identifier:?})"));
        assert_eq!(
            stored,
            fingerprint_from_golden(&row["element_data"]),
            "({namespace:?}, {identifier:?}) round-tripped differently",
        );
    }
}

/// `orjson.dumps` returns `bytes`, so Python's rows land with BLOB storage class
/// in a column declared `TEXT`. Every fixture row must actually be a BLOB, or
/// the test above would be proving nothing about the `Blob` arm of `retrieve`.
#[test]
fn pythons_element_data_really_is_a_blob() {
    let db = python_database("py-blob");
    let connection = Connection::open(db.path()).expect("open");
    let mut statement = connection
        .prepare("SELECT typeof(element_data) FROM storage")
        .expect("prepare");
    let kinds: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(!kinds.is_empty());
    assert!(
        kinds.iter().all(|kind| kind == "blob"),
        "expected every element_data to be a BLOB, got {kinds:?}",
    );

    for row in jsonl("python_adversarial_rows.jsonl") {
        assert_eq!(row["element_data_typeof"], "blob");
    }
}

/// `INSERT OR REPLACE` + `UNIQUE (url, identifier)`: saving twice under one key
/// leaves one row, holding the second value — and the golden proves Python
/// behaves the same way (its `dup` row has `id` 13, not 12, because the replace
/// deletes and re-inserts).
#[test]
fn a_replaced_row_is_the_second_write_in_pythons_file_too() {
    let rows = jsonl("python_adversarial_rows.jsonl");
    let dup: Vec<_> = rows
        .iter()
        .filter(|row| row["identifier"] == "dup")
        .collect();
    assert_eq!(dup.len(), 1, "INSERT OR REPLACE should leave one row");
    assert_eq!(dup[0]["element_data"]["text"], "second");
    assert!(
        dup[0]["id"].as_i64().expect("id") > 12,
        "the replace should have re-inserted with a fresh rowid, got {}",
        dup[0]["id"],
    );

    let db = python_database("py-dup");
    let storage = SqliteStorage::open(db.path(), Some("https://overwrite.com")).expect("open");
    let stored = storage.retrieve("dup").expect("retrieve").expect("a row");
    assert_eq!(stored.text.as_deref(), Some("second"));
    assert_eq!(storage.len().expect("len"), 1);
}

/// Namespaces are separate: an identifier saved for one site is invisible from
/// another, including the two that both resolve to `"default"` sharing rows and
/// the `com.` / `co.uk...` namespaces that only exist because of `tld`'s
/// trailing-dot quirk.
#[test]
fn the_namespaces_python_wrote_do_not_leak_into_each_other() {
    let db = python_database("py-namespaces");

    // `p.main` exists under both example.com and site-b.co.uk with different
    // contents, which is the whole point of the `url` column.
    let a = SqliteStorage::open(db.path(), Some("https://www.example.com/page")).expect("open");
    let b =
        SqliteStorage::open(db.path(), Some("https://sub.site-b.co.uk/products")).expect("open");
    assert_eq!(
        a.retrieve("p.main")
            .expect("retrieve")
            .expect("a row")
            .attributes
            .get("id")
            .map(String::as_str),
        Some("target"),
    );
    assert_eq!(
        b.retrieve("p.main")
            .expect("retrieve")
            .expect("a row")
            .attributes
            .get("id")
            .map(String::as_str),
        Some("other-site"),
    );

    // A namespace nothing was written under sees nothing.
    let c = SqliteStorage::open(db.path(), Some("https://never-scraped.example")).expect("open");
    assert_eq!(c.retrieve("p.main").expect("retrieve"), None);

    // `com.` only exists because tld returns the *un*-stripped hostname when the
    // host is itself a public suffix. A port that stripped the dot would look in
    // `com` and find nothing.
    let d = SqliteStorage::open(db.path(), Some("https://com.")).expect("open");
    assert_eq!(d.base_url(), "com.");
    assert!(d.retrieve("suffix-host").expect("retrieve").is_some());
    let e = SqliteStorage::open(db.path(), Some("https://com")).expect("open");
    assert_eq!(e.base_url(), "com");
    assert_eq!(e.retrieve("suffix-host").expect("retrieve"), None);

    // Two different unresolvable URLs share the `"default"` bucket, as in Python.
    let f = SqliteStorage::open(db.path(), None).expect("open");
    let g = SqliteStorage::open(db.path(), Some("http://localhost:8000/")).expect("open");
    assert_eq!(g.base_url(), "default");
    assert!(f.retrieve("local").expect("retrieve").is_some());
    assert!(g.retrieve("no-url").expect("retrieve").is_some());
}

/// The JSON Python writes omits absent keys rather than writing `null`s, and
/// distinguishes "no text" (`null`) from "whitespace-only text" (also `null`).
#[test]
fn the_json_shape_python_wrote_survives_deserialisation() {
    let rows = jsonl("python_adversarial_rows.jsonl");

    let root = rows
        .iter()
        .find(|row| row["identifier"] == "root")
        .expect("the parentless row");
    let data = &root["element_data"];
    assert!(data.get("parent_name").is_none(), "{data}");
    assert!(data.get("parent_attribs").is_none(), "{data}");
    assert!(data.get("parent_text").is_none(), "{data}");
    let parsed = fingerprint_from_golden(data);
    assert_eq!(parsed.parent_name, None);
    assert_eq!(parsed.parent_attribs, None);

    // `text` is `element.text.strip() if element.text else None`, so
    // whitespace-only text collapses to the *empty string*, not to `null` — the
    // two are distinguishable on disk and must stay distinguishable here.
    let empty = rows
        .iter()
        .find(|row| row["identifier"] == "empty-text")
        .expect("the whitespace-only row");
    assert_eq!(empty["element_data"]["text"], "");
    assert_eq!(
        fingerprint_from_golden(&empty["element_data"])
            .text
            .as_deref(),
        Some(""),
    );
    // …and its parent, which has no text at all, gets an explicit `null`.
    assert!(empty["element_data"]["parent_text"].is_null());
    assert!(
        empty["element_data"]
            .as_object()
            .expect("object")
            .contains_key("parent_text"),
        "Python writes the parent block whole, `null`s included",
    );

    // `__clean_attributes` drops whitespace-only values but strips the rest;
    // `parent_attribs` is copied raw.
    let mixed = rows
        .iter()
        .find(|row| row["identifier"] == "  Mixed CASE  ")
        .expect("the attribute row");
    let attrs = &mixed["element_data"]["attributes"];
    assert!(attrs.get("data-val").is_none(), "{attrs}");
    assert_eq!(attrs["data-keep"], "kept");

    // Comment children are excluded from `children` but the sibling list keeps
    // whatever lxml reports.
    let span = rows
        .iter()
        .find(|row| row["identifier"] == "span#target")
        .expect("the span row");
    let children = fingerprint_from_golden(&span["element_data"])
        .children
        .expect("children");
    assert_eq!(children, ["b", "i"], "the comment child must be excluded");
}

// --------------------------------------------------------------------------- //
// 4. a database written here, read by Python
// --------------------------------------------------------------------------- //

/// Writes `target/rust_written_storage.db` for `tools/verify_rust_db.py`.
///
/// Cargo cannot run CPython, so the two halves of this round trip are split:
/// this test produces the file deterministically, and the script (run against
/// the pinned `the reference implementation` interpreter) opens it with the real
/// `SQLiteStorageSystem` and asserts every row comes back. Re-run it with
///
/// ```text
/// cargo test -p browser-storage --test python_differential -- writes_a_database
/// python crates/browser-storage/tools/verify_rust_db.py
/// ```
///
/// The expectations the script checks are written next to the database as
/// `rust_written_expectations.json`, so the two never drift apart.
#[test]
fn writes_a_database_python_can_read() {
    let dir = rust_written_dir();
    fs::create_dir_all(&dir).expect("create target dir");
    let path = dir.join("rust_written_storage.db");
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let _ = fs::remove_file(PathBuf::from(p));
    }

    let mut expectations = Vec::new();
    for (url, identifier, fingerprint) in rust_written_rows() {
        let storage = SqliteStorage::open(&path, url).expect("open");
        storage.save(&fingerprint, identifier).expect("save");
        expectations.push(serde_json::json!({
            "url": url,
            "identifier": identifier,
            "base_url": storage.base_url(),
            // Encoded the way `save` encodes it — the derived `Serialize`
            // would drop the `parent_*` nulls and the expectation would then
            // disagree with the row that was actually written.
            "element_data": serde_json::from_str::<serde_json::Value>(
                &browser_storage::python_json::to_string(&fingerprint).expect("serialise"),
            )
            .expect("re-parse"),
        }));
        // Read it straight back through this crate too, so a failure in the
        // Python script is unambiguously a *compatibility* failure.
        assert_eq!(
            storage.retrieve(identifier).expect("retrieve"),
            Some(fingerprint),
        );
    }

    fs::write(
        dir.join("rust_written_expectations.json"),
        serde_json::to_string_pretty(&serde_json::Value::Array(expectations)).expect("serialise"),
    )
    .expect("write expectations");

    // Fold the WAL back into the main file so the artefact is self-contained
    // even if the script runs from a different process.
    let connection = Connection::open(&path).expect("reopen");
    connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("checkpoint");
    assert!(path.exists());
}

/// `<crate>/target/python-roundtrip`, resolved without assuming the workspace
/// layout: `CARGO_TARGET_TMPDIR` is handed to integration tests by Cargo.
fn rust_written_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("python-roundtrip")
}

/// The rows `writes_a_database_python_can_read` emits: one per interesting
/// namespace, plus the fingerprint shapes whose JSON encoding Python has to
/// accept (omitted optional keys, unicode, empty attribute maps).
fn rust_written_rows() -> Vec<(Option<&'static str>, &'static str, ElementFingerprint)> {
    fn attrs(pairs: &[(&str, &str)]) -> AttributeDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    vec![
        (
            Some("https://www.example.com/page"),
            "p.main",
            ElementFingerprint {
                tag: "p".to_owned(),
                attributes: attrs(&[("id", "target"), ("class", "main")]),
                text: Some("Hello".to_owned()),
                path: vec!["html", "body", "div", "p"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                parent_name: Some("div".to_owned()),
                parent_attribs: Some(attrs(&[("class", "wrap")])),
                parent_text: Some("Prefix".to_owned()),
                siblings: Some(vec!["span".to_owned()]),
                children: Some(vec!["b".to_owned(), "i".to_owned()]),
            },
        ),
        (
            // A parentless element: none of the `parent_*` keys may appear.
            Some("https://root.example.org"),
            "root",
            ElementFingerprint {
                tag: "html".to_owned(),
                attributes: AttributeDict::new(),
                text: None,
                path: vec!["html".to_owned()],
                ..Default::default()
            },
        ),
        (
            // IDN namespace, unicode identifier, unicode attribute values.
            Some("https://例え.jp/"),
            "例::text",
            ElementFingerprint {
                tag: "p".to_owned(),
                attributes: attrs(&[("id", "unicode"), ("class", "中文")]),
                text: Some("こんにちは".to_owned()),
                path: vec!["html", "body", "div", "p"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                parent_name: Some("div".to_owned()),
                parent_attribs: Some(AttributeDict::new()),
                parent_text: None,
                siblings: None,
                children: None,
            },
        ),
        (
            // The trailing-dot-on-a-suffix namespace.
            Some("https://com."),
            "suffix-host",
            ElementFingerprint {
                tag: "p".to_owned(),
                attributes: AttributeDict::new(),
                text: Some("dot".to_owned()),
                path: vec!["html", "body", "div", "p"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                parent_name: Some("div".to_owned()),
                parent_attribs: Some(AttributeDict::new()),
                parent_text: None,
                siblings: None,
                children: None,
            },
        ),
        (
            // No URL at all -> the `default` bucket.
            None,
            "no-url",
            ElementFingerprint {
                tag: "p".to_owned(),
                attributes: attrs(&[("id", "anonymous")]),
                text: Some("Anonymous".to_owned()),
                path: vec!["html", "body", "div", "p"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                parent_name: Some("div".to_owned()),
                parent_attribs: Some(AttributeDict::new()),
                parent_text: None,
                siblings: None,
                children: None,
            },
        ),
        (
            // Private-section suffix.
            Some("https://user.github.io/repo"),
            "gh",
            ElementFingerprint {
                tag: "p".to_owned(),
                attributes: attrs(&[("class", "a b")]),
                text: Some("gh".to_owned()),
                path: vec!["html", "body", "div", "p"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                parent_name: Some("div".to_owned()),
                parent_attribs: Some(AttributeDict::new()),
                parent_text: None,
                siblings: None,
                children: None,
            },
        ),
    ]
}

/// Attribute order is preserved end to end: `IndexMap` on this side,
/// `dict` on Python's, and `dict_diff` compares the *ordered* key and value
/// tuples, so a reordering silently changes every similarity score.
#[test]
fn attribute_order_survives_a_round_trip() {
    let db = TempDb::new("attr-order");
    let storage = SqliteStorage::open(db.path(), Some("https://example.com")).expect("open");

    let mut attributes = AttributeDict::new();
    for key in ["z", "a", "m", "b"] {
        attributes.insert(key.to_owned(), format!("v-{key}"));
    }
    let fingerprint = ElementFingerprint {
        tag: "p".to_owned(),
        attributes,
        path: vec!["html".to_owned(), "p".to_owned()],
        ..Default::default()
    };
    storage.save(&fingerprint, "ordered").expect("save");

    let stored = storage
        .retrieve("ordered")
        .expect("retrieve")
        .expect("a row");
    let keys: Vec<&str> = stored.attributes.keys().map(String::as_str).collect();
    assert_eq!(keys, ["z", "a", "m", "b"]);

    // And the JSON on disk keeps the order too, which is what Python's `dict`
    // would see.
    let connection = Connection::open(db.path()).expect("reopen");
    // `element_data` is a BLOB — `save` binds bytes, exactly as Python does.
    let raw: String = connection
        .query_row(
            "SELECT element_data FROM storage WHERE identifier = 'ordered'",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map(|bytes| String::from_utf8(bytes).expect("utf-8"))
        .expect("row");
    let z = raw.find("\"z\"").expect("z");
    let a = raw.find("\"a\"").expect("a");
    let m = raw.find("\"m\"").expect("m");
    let b = raw.find("\"b\"").expect("b");
    assert!(z < a && a < m && m < b, "{raw}");

    // Sanity: the JSON is a flat object with exactly the keys Python writes.
    let value: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&raw).expect("flat object");
    assert_eq!(
        value.keys().map(String::as_str).collect::<Vec<_>>(),
        ["attributes", "path", "tag", "text"],
        "optional keys must be omitted, not written as null",
    );
}

// --------------------------------------------------------------------------- //
// 5. the encoder, byte for byte
// --------------------------------------------------------------------------- //

/// Re-encoding a row Python wrote reproduces Python's bytes exactly.
///
/// This is the strongest available statement of write-compatibility: for each of
/// the 14 fixture rows, the `orjson.dumps` output is parsed into an
/// [`ElementFingerprint`] and encoded again by [`python_json::to_string`]; the
/// two strings must be equal. It catches key *order* drift, `null`-vs-absent
/// drift, and any whitespace `orjson` would not emit — none of which a
/// value-level comparison would see.
///
/// [`python_json::to_string`]: browser_storage::python_json::to_string
#[test]
fn re_encoding_pythons_rows_reproduces_pythons_bytes() {
    let rows = jsonl("python_adversarial_rows.jsonl");
    let mut failures = Vec::new();
    for row in &rows {
        let expected = row["element_data_raw"].as_str().expect("raw JSON");
        let fingerprint: ElementFingerprint =
            serde_json::from_str(expected).expect("Python's JSON is an ElementFingerprint");
        let got = browser_storage::python_json::to_string(&fingerprint).expect("encode");
        if got != expected {
            failures.push(format!(
                "  {}\n    rust   = {got}\n    python = {expected}",
                row["identifier"],
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} rows re-encode differently to orjson:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n"),
    );
}

/// The rows this crate writes carry the two keys the reference implementation's relocation scorer
/// subscripts directly.
///
/// `Selector.__calculate_similarity_score` reads `original["parent_attribs"]`
/// and `original["parent_text"]` without `dict.get`, so a row that omits either
/// raises `KeyError` in Python — *after* `retrieve()` has already returned it,
/// which makes the failure look like a parser bug rather than a storage one.
#[test]
fn written_rows_never_omit_the_keys_python_subscripts() {
    let db = TempDb::new("parent-keys");
    for (url, identifier, fingerprint) in rust_written_rows() {
        let storage = SqliteStorage::open(db.path(), url).expect("open");
        storage.save(&fingerprint, identifier).expect("save");
    }

    let connection = Connection::open(db.path()).expect("reopen");
    let mut statement = connection
        .prepare("SELECT identifier, element_data FROM storage")
        .expect("prepare");
    let rows: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get::<_, Vec<u8>>(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
        .into_iter()
        .map(|(identifier, bytes)| (identifier, String::from_utf8(bytes).expect("utf-8")))
        .collect();
    assert!(!rows.is_empty());

    for (identifier, json) in rows {
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).expect("object");
        for key in ["tag", "attributes", "text", "path"] {
            assert!(
                object.contains_key(key),
                "{identifier}: missing {key}: {json}"
            );
        }
        if object.contains_key("parent_name") {
            assert!(
                object.contains_key("parent_attribs"),
                "{identifier}: KeyError('parent_attribs') waiting to happen: {json}",
            );
            assert!(
                object.contains_key("parent_text"),
                "{identifier}: KeyError('parent_text') waiting to happen: {json}",
            );
            assert!(
                object["parent_attribs"].is_object(),
                "{identifier}: parent_attribs must be a dict, never null: {json}",
            );
        } else {
            // No parent at all: none of the three may appear.
            assert!(!object.contains_key("parent_attribs"), "{json}");
            assert!(!object.contains_key("parent_text"), "{json}");
        }
    }
}
