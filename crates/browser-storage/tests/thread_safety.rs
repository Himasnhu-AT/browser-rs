//! Concurrency and namespace-isolation gates.
//!
//! `SQLiteStorageSystem`'s docstring calls itself "race condition safe and
//! thread safe … built so the library can run in threaded frameworks like
//! scrapy", and `tests/core/test_storage_core.py::TestStorageThreadSafety`
//! pins it with 20 threads saving at once. `ARCHITECTURE.md` §7 carries both
//! that test and the URL-isolation test over as acceptance criteria for this
//! unit, so they live here rather than only as unit tests.
//!
//! What the tests actually protect: a single [`rusqlite::Connection`] is
//! `Send` but not `Sync`, and SQLite would return `SQLITE_MISUSE` (or corrupt
//! its own statement state) if two threads stepped a statement at once. The
//! `parking_lot::Mutex<Connection>` inside
//! [`SqliteStorage`](browser_storage::sqlite::SqliteStorage) is what makes
//! the shared `Arc` safe, and these tests fail loudly if it is ever removed.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::SqliteStorage;

use common::{numbered_paragraph, target_paragraph, TempDb};

/// The Python test, transliterated: 20 threads, 20 identifiers, zero errors,
/// and every record retrievable afterwards.
#[test]
fn twenty_threads_saving_at_once() {
    const THREADS: usize = 20;

    let db = TempDb::new("concurrent-saves");
    let storage =
        Arc::new(SqliteStorage::open(db.path(), Some("https://example.com")).expect("open"));

    // A barrier makes the threads collide on purpose instead of trickling in.
    let barrier = Arc::new(Barrier::new(THREADS));
    let failures = Arc::new(AtomicUsize::new(0));

    thread::scope(|scope| {
        for index in 0..THREADS {
            let storage = Arc::clone(&storage);
            let barrier = Arc::clone(&barrier);
            let failures = Arc::clone(&failures);
            scope.spawn(move || {
                barrier.wait();
                if storage
                    .save(&numbered_paragraph(index), &format!("element-{index}"))
                    .is_err()
                {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(failures.load(Ordering::Relaxed), 0, "thread safety errors");

    for index in 0..THREADS {
        let identifier = format!("element-{index}");
        let stored = storage
            .retrieve(&identifier)
            .expect("retrieve")
            .unwrap_or_else(|| panic!("{identifier} not found after concurrent save"));
        assert_eq!(stored, numbered_paragraph(index));
    }
    assert_eq!(storage.len().expect("count"), THREADS as i64);
}

/// Readers and writers interleaved on the same connection: 10 writers and 10
/// readers, all racing, none of them allowed to fail.
#[test]
fn readers_and_writers_interleave_safely() {
    const PAIRS: usize = 10;
    const ROUNDS: usize = 25;

    let db = TempDb::new("readers-writers");
    let storage =
        Arc::new(SqliteStorage::open(db.path(), Some("https://example.com")).expect("open"));

    // Seed every key so the readers have something to find.
    for index in 0..PAIRS {
        storage
            .save(&numbered_paragraph(index), &format!("element-{index}"))
            .expect("seed");
    }

    let barrier = Arc::new(Barrier::new(PAIRS * 2));
    thread::scope(|scope| {
        for index in 0..PAIRS {
            let writer = Arc::clone(&storage);
            let writer_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                writer_barrier.wait();
                for _ in 0..ROUNDS {
                    writer
                        .save(&numbered_paragraph(index), &format!("element-{index}"))
                        .expect("save under contention");
                }
            });

            let reader = Arc::clone(&storage);
            let reader_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                reader_barrier.wait();
                for _ in 0..ROUNDS {
                    let stored = reader
                        .retrieve(&format!("element-{index}"))
                        .expect("retrieve under contention")
                        .expect("the seeded row must never disappear");
                    // `INSERT OR REPLACE` deletes and re-inserts; a reader must
                    // never observe the gap.
                    assert_eq!(stored, numbered_paragraph(index));
                }
            });
        }
    });

    assert_eq!(storage.len().expect("count"), PAIRS as i64);
}

/// Two threads racing to save *the same* identifier: last write wins, exactly
/// one row exists, and it is one of the two values written.
#[test]
fn concurrent_writes_to_one_identifier_leave_one_row() {
    let db = TempDb::new("same-identifier");
    let storage =
        Arc::new(SqliteStorage::open(db.path(), Some("https://example.com")).expect("open"));

    let barrier = Arc::new(Barrier::new(2));
    thread::scope(|scope| {
        for index in 0..2 {
            let storage = Arc::clone(&storage);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..50 {
                    storage
                        .save(&numbered_paragraph(index), "contended")
                        .expect("save");
                }
            });
        }
    });

    assert_eq!(storage.len().expect("count"), 1);
    let stored = storage
        .retrieve("contended")
        .expect("retrieve")
        .expect("a row");
    assert!(
        stored == numbered_paragraph(0) || stored == numbered_paragraph(1),
        "{stored:?}",
    );
}

/// Separate `SqliteStorage` instances on one file, hammered from many threads.
///
/// This is the shape a spider takes when it crawls several domains at once:
/// one process, one database file, one storage per site.
#[test]
fn separate_instances_share_one_file_safely() {
    const SITES: usize = 8;
    const ROWS: usize = 20;

    let db = TempDb::new("multi-instance");
    let sites: Vec<Arc<SqliteStorage>> = (0..SITES)
        .map(|index| {
            Arc::new(
                SqliteStorage::open(db.path(), Some(&format!("https://site-{index}.com")))
                    .expect("open"),
            )
        })
        .collect();

    let barrier = Arc::new(Barrier::new(SITES));
    thread::scope(|scope| {
        for (index, storage) in sites.iter().enumerate() {
            let storage = Arc::clone(storage);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for row in 0..ROWS {
                    storage
                        .save(&numbered_paragraph(row), &format!("shared-{row}"))
                        .unwrap_or_else(|e| panic!("site-{index} row {row}: {e}"));
                }
            });
        }
    });

    for (index, storage) in sites.iter().enumerate() {
        assert_eq!(storage.base_url(), format!("site-{index}.com"));
        assert_eq!(storage.len().expect("count"), ROWS as i64);
        for row in 0..ROWS {
            assert_eq!(
                storage
                    .retrieve(&format!("shared-{row}"))
                    .expect("retrieve"),
                Some(numbered_paragraph(row)),
            );
        }
    }
}

/// Two different URLs never see each other's identifiers — the property that
/// makes one shared `elements_storage.db` safe for a whole crawl.
#[test]
fn different_urls_do_not_see_each_others_identifiers() {
    let db = TempDb::new("namespace-isolation");

    let site_a = SqliteStorage::open(db.path(), Some("https://site-a.com/products")).expect("a");
    let site_b = SqliteStorage::open(db.path(), Some("https://site-b.com/products")).expect("b");
    let anonymous = SqliteStorage::open(db.path(), None).expect("anonymous");

    assert_eq!(site_a.base_url(), "site-a.com");
    assert_eq!(site_b.base_url(), "site-b.com");
    assert_eq!(anonymous.base_url(), "default");

    let mut a_fingerprint = target_paragraph();
    a_fingerprint.text = Some("A".to_owned());
    let mut b_fingerprint = target_paragraph();
    b_fingerprint.text = Some("B".to_owned());

    site_a.save(&a_fingerprint, "shared-id").expect("save a");
    assert_eq!(site_b.retrieve("shared-id").expect("retrieve"), None);
    assert_eq!(anonymous.retrieve("shared-id").expect("retrieve"), None);

    site_b.save(&b_fingerprint, "shared-id").expect("save b");

    // Each namespace keeps its own value; neither overwrote the other.
    assert_eq!(
        site_a.retrieve("shared-id").expect("retrieve"),
        Some(a_fingerprint),
    );
    assert_eq!(
        site_b.retrieve("shared-id").expect("retrieve"),
        Some(b_fingerprint),
    );
    assert_eq!(anonymous.retrieve("shared-id").expect("retrieve"), None);

    // Subdomains of one site share a namespace, because the key is the
    // *registrable* domain.
    let deep = SqliteStorage::open(db.path(), Some("https://shop.site-a.com/x?y=1")).expect("deep");
    assert_eq!(deep.base_url(), "site-a.com");
    assert!(deep.retrieve("shared-id").expect("retrieve").is_some());
}

/// Reading concurrently from many namespaces of one file.
#[test]
fn concurrent_reads_across_namespaces_stay_isolated() {
    const SITES: usize = 6;

    let db = TempDb::new("namespace-reads");
    let sites: Vec<Arc<SqliteStorage>> = (0..SITES)
        .map(|index| {
            let storage = Arc::new(
                SqliteStorage::open(db.path(), Some(&format!("https://site-{index}.com")))
                    .expect("open"),
            );
            storage
                .save(&numbered_paragraph(index), "same-identifier")
                .expect("seed");
            storage
        })
        .collect();

    let barrier = Arc::new(Barrier::new(SITES));
    thread::scope(|scope| {
        for (index, storage) in sites.iter().enumerate() {
            let storage = Arc::clone(storage);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..50 {
                    assert_eq!(
                        storage.retrieve("same-identifier").expect("retrieve"),
                        Some(numbered_paragraph(index)),
                        "site-{index} read another namespace's row",
                    );
                }
            });
        }
    });
}

/// The trait object is what the adaptive layer actually holds, so it has to be
/// shareable across threads too.
#[test]
fn an_erased_storage_is_shareable() {
    let storage: Arc<dyn StorageSystem> =
        Arc::new(SqliteStorage::in_memory(Some("https://example.com")).expect("open"));

    thread::scope(|scope| {
        for index in 0..12 {
            let storage = Arc::clone(&storage);
            scope.spawn(move || {
                storage
                    .save(&numbered_paragraph(index), &format!("dyn-{index}"))
                    .expect("save");
                assert_eq!(
                    storage.retrieve(&format!("dyn-{index}")).expect("retrieve"),
                    Some(numbered_paragraph(index)),
                );
                assert_eq!(storage.base_url(), "example.com");
            });
        }
    });
}
