//! Byte-for-byte differential test for the four `ItemList` exporters.
//!
//! `testdata/u15_item_exports.jsonl` (from `tools/corpus/u15/gen_exports.py`) carries the
//! exact bytes `reference.spiders.result.ItemList` wrote for each `(items, kind, options)`
//! triple, base64-encoded. The port writes to a temp file and the two are compared as
//! bytes, so CSV quoting and `\r\n` terminators, `orjson`'s float formatting, the XML
//! declaration's single quotes, `<tag />` short forms, `ElementTree.indent` and the
//! control-character scrubbing are all pinned rather than approximated.
//!
//! The item table is hostile on purpose: unicode keys and values, nested containers, every
//! C0 control character, CSV metacharacters, heterogeneous items with missing keys, and key
//! names that are not valid XML names.
//!
//! Rows are never removed; anything unreproducible belongs in [`DIVERGENCES`], which fails
//! if it ever starts matching.

mod u15_support;

use std::collections::HashMap;

use base64::Engine;
use browser_spider::items::{CsvOptions, ItemList, XmlOptions};
use browser_spider::request::Item;
use serde_json::Value;

use u15_support::read_corpus;

/// Cases that deliberately differ from Python, keyed by the row's `name`.
const DIVERGENCES: &[(&str, &str)] = &[];

fn items_of(row: &Value) -> ItemList {
    row["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item.as_object().expect("item is an object").clone())
        .collect::<Vec<Item>>()
        .into()
}

fn write_export(items: &ItemList, kind: &str, options: &Value, path: &std::path::Path) {
    match kind {
        "json" => items
            .to_json(path, options["indent"].as_bool().unwrap_or(false))
            .expect("to_json"),
        "jsonl" => items.to_jsonl(path).expect("to_jsonl"),
        "csv" => {
            let delimiter = options
                .get("delimiter")
                .and_then(Value::as_str)
                .unwrap_or(",");
            let bytes = delimiter.as_bytes();
            assert_eq!(
                bytes.len(),
                1,
                "the corpus only uses single-byte delimiters"
            );
            let csv_options = CsvOptions {
                fields: options
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|fields| {
                        fields
                            .iter()
                            .map(|f| f.as_str().expect("field name").to_owned())
                            .collect()
                    }),
                delimiter: bytes[0],
            };
            items.to_csv(path, &csv_options).expect("to_csv");
        }
        "xml" => {
            let xml_options = XmlOptions {
                root_tag: options
                    .get("root_tag")
                    .and_then(Value::as_str)
                    .unwrap_or("items")
                    .to_owned(),
                item_tag: options
                    .get("item_tag")
                    .and_then(Value::as_str)
                    .unwrap_or("item")
                    .to_owned(),
                indent: options
                    .get("indent")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            };
            items.to_xml(path, &xml_options).expect("to_xml");
        }
        other => panic!("unknown export kind {other}"),
    }
}

/// Renders bytes for an assertion message: printable ASCII stays, everything else is escaped.
fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).escape_debug().to_string()
}

#[test]
fn item_exports_match_the_reference_byte_for_byte_u15() {
    let corpus = read_corpus("u15_item_exports.jsonl");
    let divergences: HashMap<&str, &str> = DIVERGENCES.iter().copied().collect();
    let mut seen_divergences: HashMap<String, usize> = HashMap::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut failures = Vec::new();
    let mut matched = 0usize;

    for (index, row) in corpus.iter().enumerate() {
        let name = row["name"].as_str().expect("name");
        let kind = row["kind"].as_str().expect("kind");
        let expected = base64::engine::general_purpose::STANDARD
            .decode(row["content_b64"].as_str().expect("content_b64"))
            .expect("base64");

        let items = items_of(row);
        let path = dir.path().join(format!("out-{index}"));
        write_export(&items, kind, &row["options"], &path);
        let got = std::fs::read(&path).expect("read back the export");

        match divergences.get(name) {
            Some(why) => {
                *seen_divergences.entry(name.to_owned()).or_default() += 1;
                assert_ne!(
                    got, expected,
                    "declared divergence {name:?} now MATCHES Python. Delete the row from \
                     DIVERGENCES — the port improved. Reason recorded was: {why}"
                );
            }
            None if got == expected => matched += 1,
            None => failures.push(format!(
                "  {name}:\n    python: {}\n    rust:   {}",
                show(&expected),
                show(&got)
            )),
        }

        // `render_xml` must produce exactly what `to_xml` writes, minus nothing.
        if kind == "xml" {
            let xml_options = XmlOptions {
                root_tag: row["options"]
                    .get("root_tag")
                    .and_then(Value::as_str)
                    .unwrap_or("items")
                    .to_owned(),
                item_tag: row["options"]
                    .get("item_tag")
                    .and_then(Value::as_str)
                    .unwrap_or("item")
                    .to_owned(),
                indent: row["options"]
                    .get("indent")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            };
            assert_eq!(
                items.render_xml(&xml_options).as_bytes(),
                got.as_slice(),
                "{name}: render_xml and to_xml disagree"
            );
        }
    }

    for (name, why) in DIVERGENCES {
        assert!(
            seen_divergences.contains_key(*name),
            "DIVERGENCES lists {name:?}, which is not in the corpus. Never shrink a corpus \
             to raise the pass rate. Reason recorded was: {why}"
        );
    }
    assert!(
        failures.is_empty(),
        "{} of {} exports differ from Python:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
    assert!(
        corpus.len() >= 80,
        "the export corpus shrank to {} cases",
        corpus.len()
    );
    eprintln!("item exports: {matched}/{} byte-identical", corpus.len());
}
