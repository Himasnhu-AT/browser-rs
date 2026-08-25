//! Differential test for [`ResponseCacheManager`] against `reference/spiders/cache.py`.
//!
//! Two corpora from `tools/corpus/u16/gen.py`:
//!
//! * `u16_cache.jsonl` — 25 responses put through `put`'s dict construction (the
//!   tuple-vs-dict cookie branch included) with the exact bytes `orjson.dumps` produced.
//!   The Rust side writes a real cache entry and compares the file **byte for byte**, then
//!   reads it back and checks the round trip. Byte equality is what lets a cache directory
//!   written by the Python spider be replayed by the Rust one and the other way round, so
//!   the corpus deliberately includes the places two JSON writers could disagree: C0
//!   control characters, `U+2028`/`U+2029`, astral-plane emoji, `DEL`, forward slashes,
//!   quotes and backslashes.
//! * `u16_cache_get.jsonl` — 55 hand-corrupted cache files (truncated, empty, wrong
//!   top-level type, invalid UTF-8, each key missing in turn, each field given the wrong
//!   type, nine cookie shapes, a path that is a directory), recording what Python's `get`
//!   did with each. `get` wraps its whole read in `except Exception: return None`, so the
//!   contract is that it degrades rather than raises — on both sides.

mod u16_support;

use bytes::Bytes;
use indexmap::IndexMap;
use browser_spider::cache::ResponseCacheManager;
use browser_spider::session::FetchedResponse;
use serde_json::Value;
use u16_support::{assert_divergences, read_corpus, Divergence};

/// Rows whose bytes differ from `orjson`'s. None: `serde_json` and `orjson` agree on
/// compact separators, raw (unescaped) non-ASCII and the `\"` / `\\` escapes.
const DIVERGENCES: &[Divergence] = &[];

fn string_map(value: &Value) -> IndexMap<String, String> {
    value
        .as_object()
        .expect("an object")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().expect("a string").to_owned()))
        .collect()
}

fn response_of(row: &Value) -> FetchedResponse {
    let body = hex_decode(row["body_hex"].as_str().expect("body_hex"));
    FetchedResponse {
        status: u16::try_from(row["status"].as_u64().expect("status")).expect("u16"),
        reason: row["reason"].as_str().expect("reason").to_owned(),
        url: row["url"].as_str().expect("url").to_owned(),
        body: Bytes::from(body),
        encoding: row["encoding"].as_str().expect("encoding").to_owned(),
        headers: string_map(&row["headers"]),
        request_headers: string_map(&row["request_headers"]),
        cookies: row["cookies"].clone(),
        history: Vec::new(),
        meta: browser_spider::request::Item::new(),
        request: None,
    }
}

fn hex_decode(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

#[tokio::test]
async fn cache_files_match_python_byte_for_byte() {
    let rows = read_corpus("u16_cache.jsonl");
    assert!(rows.len() >= 25, "corpus shrank to {} rows", rows.len());

    let directory = tempfile::tempdir().expect("tempdir");
    let cache = ResponseCacheManager::new(directory.path());
    let mut failures: Vec<(String, String)> = Vec::new();

    for (index, row) in rows.iter().enumerate() {
        let name = row["name"].as_str().expect("name").to_owned();
        let response = response_of(row);
        let method = row["method"].as_str().expect("method");
        let fingerprint = [u8::try_from(index).expect("small corpus"); 20];

        assert!(
            cache.put(&fingerprint, &response, method).await,
            "{name}: put failed"
        );

        // -- the bytes on disk
        let expected = row["serialized"].as_str().expect("serialized");
        let actual = tokio::fs::read_to_string(cache.cache_path(&fingerprint))
            .await
            .expect("read back");
        if expected != actual {
            failures.push((
                format!("{name}/bytes"),
                format!("python={expected}\n      rust={actual}"),
            ));
        }

        // -- the round trip
        let entry = cache
            .get_entry(&fingerprint)
            .await
            .unwrap_or_else(|| panic!("{name}: cache miss on an entry we just wrote"));
        let replayed = entry.response;
        assert_eq!(entry.method, method, "{name}: method");
        assert_eq!(replayed.url, response.url, "{name}: url");
        assert_eq!(replayed.body, response.body, "{name}: body");
        assert_eq!(replayed.status, response.status, "{name}: status");
        assert_eq!(replayed.reason, response.reason, "{name}: reason");
        assert_eq!(replayed.encoding, response.encoding, "{name}: encoding");
        assert_eq!(replayed.headers, response.headers, "{name}: headers");
        assert_eq!(
            replayed.request_headers, response.request_headers,
            "{name}: request_headers"
        );
        assert_eq!(replayed.cookies, response.cookies, "{name}: cookies");

        // -- the shape rule from regression #376: an array stays an array.
        let expect_list = row["cookies_are_a_list"].as_bool().expect("bool");
        assert_eq!(
            replayed.cookies.is_array(),
            expect_list,
            "{name}: cookie shape"
        );
    }

    eprintln!(
        "u16 cache: {} entries, {} mismatches",
        rows.len(),
        failures.len()
    );
    assert_divergences("ResponseCacheManager", DIVERGENCES, &failures);
}

#[tokio::test]
async fn the_key_is_the_lowercase_hex_fingerprint() {
    let directory = tempfile::tempdir().expect("tempdir");
    let cache = ResponseCacheManager::new(directory.path());
    let fingerprint: [u8; 20] = [
        0x00, 0x0f, 0x10, 0xff, 0xa5, 0x5a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
        0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
    ];
    let response = FetchedResponse::new(200, "https://example.com/", "x");
    assert!(cache.put(&fingerprint, &response, "GET").await);

    let expected = directory
        .path()
        .join("000f10ffa55a0102030405060708090a0b0c0d0e.json");
    assert!(expected.exists(), "{}", expected.display());
}

// ---------------------------------------------------------------------------
// Degradation: what `get` does with a file `put` did not write
// ---------------------------------------------------------------------------

/// Cache files where Python's untyped `get` succeeds and the port's typed decoder misses.
///
/// Python hands `Response(...)` whatever `orjson` produced — a `status` of `None`, of
/// `"200"`, of `200.5`, a `headers` that is a list, a `url` that is `None`. The Rust
/// `FetchedResponse` is a typed struct, so those cannot be represented, and the entry is
/// treated as a miss instead: the request is re-fetched, which is exactly what would have
/// happened had it never been cached, and is the only outcome that cannot hand the rest of
/// the engine a `status` it has no field for.
///
/// Every one of these files is unreachable from `put` — they are hand-corrupted. The
/// declarations are self-policing: `assert_divergences` fails if one starts matching.
const GET_DIVERGENCES: &[Divergence] = &[
    Divergence {
        key: "status_string",
        reason: "python keeps the string \"200\"; `status: u16` cannot hold it",
    },
    Divergence {
        key: "status_null",
        reason: "python keeps `None`; `status: u16` cannot hold it",
    },
    Divergence {
        key: "status_float",
        reason: "python keeps `200.5`; `status: u16` cannot hold it",
    },
    Divergence {
        key: "status_negative",
        reason: "python keeps `-1`; `status: u16` cannot hold it",
    },
    Divergence {
        key: "status_huge",
        reason: "python keeps `1e20`; `status: u16` cannot hold it",
    },
    Divergence {
        key: "url_null",
        reason: "python keeps `None`; `url: String` cannot hold it",
    },
    Divergence {
        key: "reason_null",
        reason: "python keeps `None`; `reason: String` cannot hold it",
    },
    Divergence {
        key: "encoding_null",
        reason: "python keeps `None`; `encoding: String` cannot hold it",
    },
    Divergence {
        key: "headers_list",
        reason: "python keeps the list; `headers: IndexMap<String, String>` cannot hold it",
    },
    Divergence {
        key: "headers_null",
        reason: "python keeps `None`; `headers: IndexMap<String, String>` cannot hold it",
    },
    Divergence {
        key: "headers_nested",
        reason: "python keeps the nested object; the header values are `String` here",
    },
    Divergence {
        key: "method_null",
        reason: "python keeps `None`; `CacheEntry::method` is a `String`",
    },
    Divergence {
        key: "method_number",
        reason: "python keeps `7`; `CacheEntry::method` is a `String`",
    },
];

#[tokio::test]
async fn cache_get_degrades_like_python() {
    let rows = read_corpus("u16_cache_get.jsonl");
    assert!(rows.len() >= 55, "corpus shrank to {} rows", rows.len());

    let hits = rows.iter().filter(|row| row["outcome"] == "hit").count();
    let misses = rows.iter().filter(|row| row["outcome"] == "miss").count();
    assert!(hits >= 20, "only {hits} rows are a hit in Python");
    assert!(misses >= 20, "only {misses} rows are a miss in Python");
    assert!(
        !rows.iter().any(|row| row["outcome"] == "raised"),
        "Python's `get` raised - it is supposed to swallow everything"
    );

    let mut failures: Vec<(String, String)> = Vec::new();

    for row in &rows {
        let name = row["name"].as_str().expect("name").to_owned();
        let directory = tempfile::tempdir().expect("tempdir");
        let cache = ResponseCacheManager::new(directory.path());
        let fingerprint: [u8; 20] =
            std::array::from_fn(|index| u8::try_from(index).expect("a small index"));
        let path = cache.cache_path(&fingerprint);

        match row["file_hex"].as_str() {
            Some(hex) => tokio::fs::write(&path, hex_decode(hex))
                .await
                .expect("write the corpus file"),
            // A directory where the entry should be: unreadable, and `get` must not raise.
            None => tokio::fs::create_dir(&path).await.expect("mkdir"),
        }

        // The contract under test: this call may never panic, whatever is on disk.
        let entry = cache.get_entry(&fingerprint).await;

        let expected_hit = row["outcome"] == "hit";
        match (expected_hit, entry) {
            (false, None) => {}
            (false, Some(entry)) => failures.push((
                name,
                format!("python=miss rust=hit({:?})", entry.response.url),
            )),
            (true, None) => failures.push((name, "python=hit rust=miss".to_owned())),
            (true, Some(entry)) => {
                let response = entry.response;
                let mut differences: Vec<String> = Vec::new();
                let expect = |what: &str, expected: &serde_json::Value, actual: String| {
                    let expected = match expected.as_str() {
                        Some(text) => text.to_owned(),
                        None => expected.to_string(),
                    };
                    (expected != actual).then(|| format!("{what}: {expected:?} vs {actual:?}"))
                };
                differences.extend(expect("url", &row["url"], response.url.clone()));
                differences.extend(expect(
                    "status",
                    &row["status"],
                    response.status.to_string(),
                ));
                differences.extend(expect("reason", &row["reason"], response.reason.clone()));
                differences.extend(expect(
                    "encoding",
                    &row["encoding"],
                    response.encoding.clone(),
                ));
                let body_hex: String = response
                    .body
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                differences.extend(expect("body", &row["body_hex"], body_hex));
                // The shape rule: a JSON array must still come back as an array and an
                // object as an object, whatever else is wrong with the file.
                if row["cookies_is_tuple"].as_bool() == Some(true) && !response.cookies.is_array() {
                    differences.push(format!(
                        "cookies: expected an array, got {}",
                        response.cookies
                    ));
                }
                if row["cookies_is_dict"].as_bool() == Some(true) && !response.cookies.is_object() {
                    differences.push(format!(
                        "cookies: expected an object, got {}",
                        response.cookies
                    ));
                }
                if !differences.is_empty() {
                    failures.push((name, differences.join("; ")));
                }
            }
        }
    }

    eprintln!(
        "u16 cache get: {} corrupt-entry scenarios, {} mismatches",
        rows.len(),
        failures.len()
    );
    assert_divergences("ResponseCacheManager::get", GET_DIVERGENCES, &failures);
}
