# `browser-storage`

Adaptive-selector persistence for `browser-rs`: the SQLite store behind
The reference implementation's "find the element again after the site was redesigned" feature.

The on-disk format is **shared with the Python implementation**. An existing
`elements_storage.db` written by the reference implementation 0.4.15 is readable and writable here,
and vice versa — same schema text, same statements, same journal mode, same
namespace keys. Both directions are *tested*, not asserted:

* **Python → Rust** — `tests/python_db_compat.rs` and `tests/python_differential.rs`
  read databases this repository ships, written by the reference implementation 0.4.15 under
  CPython 3.14 with real `lxml` elements and `orjson` BLOBs.
* **Rust → Python** — `writes_a_database_python_can_read` emits a database and an
  expectations file, and `tools/verify_rust_db.py` opens them with the reference implementation's own
  `SQLiteStorageSystem`, checks every row matches, and then feeds each row through
  `Selector.__calculate_similarity_score` — the adaptive path the storage exists to
  serve, and the one that catches a row Python can *load* but not *use*.
* **Byte for byte** — `tests/python_payload_parity.rs` takes 81 fingerprints that
  The reference implementation produced from real `lxml` elements, re-encodes each one here and
  compares against `orjson`'s original bytes; `tools/verify_rust_payloads.py`
  then reads the database this crate writes from them and checks the stored bytes
  and storage class are identical. An *empty* database is compared byte for byte
  too: the only difference between the two files is the SQLite library version
  stamp in the header (`tests/python_db_compat.rs`).

## What it stores

| Column | Value |
|---|---|
| `url` | the **registrable domain** of the page (`example.com`), or `"default"` |
| `identifier` | whatever the caller passed — raw, never hashed |
| `element_data` | the [`ElementFingerprint`] JSON from `browser-core` |

`UNIQUE (url, identifier)` plus `INSERT OR REPLACE` means a re-save overwrites.
Two different sites never see each other's identifiers, even in one file.

## Using it

```rust
use std::sync::Arc;

use browser_core::ElementFingerprint;
use browser_storage::r#trait::StorageSystem;
use browser_storage::sqlite::SqliteStorage;

let storage: Arc<dyn StorageSystem> =
    Arc::new(SqliteStorage::open("elements_storage.db", Some("https://example.com"))?);

assert_eq!(storage.base_url(), "example.com");

let fingerprint = ElementFingerprint { tag: "p".into(), ..Default::default() };
storage.save(&fingerprint, "p.price")?;
assert_eq!(storage.retrieve("p.price")?, Some(fingerprint));
assert_eq!(storage.retrieve("never-seen")?, None);   // absent is not an error
# Ok::<(), browser_storage::error::StorageError>(())
```

`SqliteStorage` is `Send + Sync`: share one instance behind an `Arc` across as
many threads as you like. A `parking_lot::Mutex<Connection>` serialises the
statements, WAL keeps other processes' readers unblocked, and SQLite's
five-second busy handler (the same default `sqlite3.connect` uses) covers
contention between connections.

## Module map

| Module | Contents |
|---|---|
| `r#trait` | `StorageSystem` — the backend contract (`save`, `retrieve`, `base_url`) |
| `sqlite` | `SqliteStorage`, plus the verbatim `SCHEMA_SQL` / `INSERT_SQL` / `SELECT_SQL` |
| `base_url` | `registrable_domain()`, `normalise_url()`, `DEFAULT_BASE_URL` |
| `python_json` | `to_string()` — the `element_to_dict` + `orjson.dumps` encoding |
| `hash` | `identifier_hash()` — Python's `_get_hash`, for third-party backends |
| `error` | `StorageError` |

## Differences from the Python original

* **No singleton.** Python wraps the class in `lru_cache(1)`, so constructing a
  storage for a second `(file, url)` pair evicts — and closes — the first. Rust
  hands out an `Arc` and lets the caller own the lifetime.
* **`base_url` is computed once,** in the constructor, instead of through an
  instance-keyed LRU. `url` is immutable after construction, so the value is
  the same.
* **Errors are typed.** Python lets `sqlite3` and `orjson` exceptions escape;
  every one of those points is a `StorageError` variant here, naming the
  identifier and the namespace involved.
* **Rust can store what Python cannot.** `element_to_dict` puts `child.tag` in
  `siblings`, and for an `lxml` comment or processing instruction that is a
  Cython function object, which `orjson.dumps` refuses. Every element with a
  comment sibling therefore raises inside Python's `save()`; here it stores
  normally. The four elements this affects are recorded in
  `testdata/python_payload_divergences.jsonl`.

## Known divergences from the reference implementation

Every one of these is pinned by a test, so it fails loudly if it widens.

| What | Size | Why it cannot be closed here |
|---|---|---|
| Public suffix list | 40 of 20 306 swept rules (`tests/psl_sweep.rs`) | The `psl` crate's compiled snapshot is *newer* than `tld` 0.13.2's bundled `effective_tld_names.dat.txt`; the 39 affected hosts are rules the PSL has since retired. This crate agrees with today's list, Python with an older one. Closing it means vendoring `tld`'s snapshot and going stale against reality instead. |
| `str.lower()` | 28 of 1 112 064 code points (`tests/lowercase_parity.rs`) | CPython 3.14 ships UCD 16.0.0; `char::to_lowercase` is newer. All 28 are *unassigned* in Unicode 16 (`U+A7CE`, `U+A7D2`, `U+A7D4`, `U+16EA0..=U+16EB8`), so Python leaves them alone. None is below `U+0100`. |

Four divergences that looked like these and turned out to be *bugs* are now
fixed, and live in the corpora rather than the table above:

* `tld` returns the **un**-dot-stripped hostname when the host is itself a public
  suffix, so `https://com.` namespaces under `com.`, dot included;
* `str.strip()` removes four code points `str::trim` does not (`U+001C..U+001F`);
* `urlsplit` rejects a bracketed host that is not a valid IPv6 or IPvFuture
  literal, so `https://[example.com]/x` is `default`, not `example.com`;
* `element_to_dict` writes the three `parent_*` keys as a block, `null`s
  included — omitting them makes the reference implementation's own relocation scorer raise
  `KeyError`;
* `tld` marks the **parent** of an exception rule as a public suffix of its own
  (`!www.ck` makes `ck` one), where `psl` follows the PSL algorithm and does not;
* CPython validates a bracketed netloc **after** stripping userinfo, so
  `https://user@[::1]@evil.com` is rejected outright rather than resolving to
  `evil.com`;
* CPython refuses a non-ASCII netloc whose NFKC form grows a URL delimiter, so
  `https://example.com＠evil.com` (fullwidth `@`) is `default`;
* `element_data` is bound as **bytes**, so a row this crate writes has the same
  BLOB storage class as one Python writes — the two files no longer differ at
  all outside SQLite's own header.

## Regenerating the test fixtures

Every script needs an interpreter with `browser-rs`, `tld` and `lxml` installed.
They write straight into `testdata/`:

```sh
python tools/gen_base_urls.py > testdata/tld_base_urls.jsonl  # namespace corpus
python tools/gen_base_url_parity.py testdata                  # 180 adversarial URLs
python tools/gen_base_url_fuzz.py testdata/base_url_fuzz.jsonl  # 4000 random URLs
python tools/gen_exception_rules.py testdata/psl_exception_rules.jsonl
python tools/gen_identifier_corpus.py testdata                # 2000 identifiers + a .db
python tools/gen_payload_corpus.py testdata                   # 81 real-element rows
python tools/gen_empty_db.py testdata/python_empty_storage.db
python tools/gen_psl_sweep.py testdata                        # all 10 153 PSL rules
python tools/gen_lowercase_map.py testdata                    # str.lower() + str.strip()
python tools/gen_storage_goldens.py testdata                  # schema, hashes, a .db
python tools/gen_python_db.py                                 # the original .db fixture
```

The Rust → Python direction is a two-step, because Cargo cannot run CPython:

```sh
cargo test -p browser-storage --test python_differential -- writes_a_database
python tools/verify_rust_db.py

cargo test -p browser-storage --test python_payload_parity -- writes_a_database
python tools/verify_rust_payloads.py
```

[`ElementFingerprint`]: https://docs.rs/browser-core
