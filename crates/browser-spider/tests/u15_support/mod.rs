//! Shared decoding for the unit-15 golden corpora.
//!
//! The generators under `tools/corpus/u15/` emit request *specs* — small JSON objects —
//! and `tools/corpus/u15/_spec.py` turns them into Python `Request`s. This module is the
//! other half: it turns the identical spec into a Rust [`Request`], so a corpus row means
//! exactly one thing on both sides.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use browser_core::types::{HttpMethod, Proxy};
use browser_spider::request::{Item, Request, RequestBody, SessionKwargs};
use indexmap::IndexMap;
use serde_json::Value;

/// Absolute path of a file in the crate's `testdata/` directory.
pub fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

/// Reads a `.jsonl` corpus from `testdata/`, one [`Value`] per non-blank line.
pub fn read_corpus(name: &str) -> Vec<Value> {
    let path = testdata(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {} ({e}). Regenerate it with tools/corpus/u15/.",
            path.display()
        )
    });
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{name} line {}: {e}", n + 1))
        })
        .collect()
}

/// Decodes `{"form": [[k, v], ...]}` / `{"text": ...}` / `{"bytes_hex": ...}` / `{"io_hex": ...}`.
pub fn decode_body(value: &Value) -> RequestBody {
    if let Some(form) = value.get("form") {
        RequestBody::Form(
            form.as_array()
                .expect("form is an array")
                .iter()
                .map(|pair| {
                    let pair = pair.as_array().expect("pair");
                    (
                        pair[0].as_str().expect("form key").to_owned(),
                        pair[1].as_str().expect("form value").to_owned(),
                    )
                })
                .collect(),
        )
    } else if let Some(text) = value.get("text") {
        RequestBody::Text(text.as_str().expect("text body").to_owned())
    } else if let Some(hex_body) = value.get("bytes_hex") {
        RequestBody::Bytes(hex::decode(hex_body.as_str().expect("hex body")).expect("valid hex"))
    } else if let Some(hex_body) = value.get("io_hex") {
        // `BytesIO`, not `bytes`: an empty one is still truthy and still suppresses `json=`.
        RequestBody::Io(hex::decode(hex_body.as_str().expect("hex body")).expect("valid hex"))
    } else {
        panic!("bad body spec: {value}")
    }
}

fn to_header_map(value: &Value) -> IndexMap<String, String> {
    value
        .as_object()
        .expect("header map")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("header value").to_owned()))
        .collect()
}

/// Decodes a kwargs spec into [`SessionKwargs`].
///
/// Any key that is not one of the seven the port names lands in
/// [`SessionKwargs::extra`], exactly where Python's untyped `**kwargs` would put it.
pub fn decode_kwargs(spec: &Value) -> SessionKwargs {
    let mut kwargs = SessionKwargs::default();
    let Some(object) = spec.as_object() else {
        return kwargs;
    };

    for (key, value) in object {
        match key.as_str() {
            "method" => {
                kwargs.method = Some(match value.as_str().expect("method") {
                    "GET" => HttpMethod::Get,
                    "POST" => HttpMethod::Post,
                    "PUT" => HttpMethod::Put,
                    "DELETE" => HttpMethod::Delete,
                    other => panic!("unsupported method in corpus: {other}"),
                });
            }
            "data" => kwargs.data = Some(decode_body(value)),
            "json" => kwargs.json = Some(value.clone()),
            "headers" => kwargs.headers = Some(to_header_map(value)),
            "extra_headers" => kwargs.extra_headers = Some(to_header_map(value)),
            "proxy" => {
                kwargs.proxy = Some(match value {
                    Value::String(url) => Proxy::Url(url.clone()),
                    other => serde_json::from_value(other.clone()).expect("proxy config"),
                });
            }
            "proxies" => {
                let map: BTreeMap<String, String> = value
                    .as_object()
                    .expect("proxies")
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().expect("proxy url").to_owned()))
                    .collect();
                kwargs.proxies = Some(map);
            }
            other => {
                kwargs.extra.insert(other.to_owned(), value.clone());
            }
        }
    }
    kwargs
}

/// Builds a [`Request`] from a corpus request spec.
pub fn build_request(spec: &Value) -> Request {
    let mut meta = Item::new();
    if let Some(object) = spec.get("meta").and_then(Value::as_object) {
        for (k, v) in object {
            meta.insert(k.clone(), v.clone());
        }
    }
    if let Some(tag) = spec.get("tag") {
        meta.insert("tag".to_owned(), tag.clone());
    }

    Request::new(spec["url"].as_str().expect("url"))
        .with_sid(spec.get("sid").and_then(Value::as_str).unwrap_or(""))
        .with_priority(
            spec.get("priority")
                .and_then(Value::as_i64)
                .map(|p| i32::try_from(p).expect("priority fits in i32"))
                .unwrap_or(0),
        )
        .with_dont_filter(
            spec.get("dont_filter")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
        .with_meta(meta)
        .with_retry_count(
            spec.get("retry_count")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(0),
        )
        .with_session_kwargs(decode_kwargs(spec.get("kwargs").unwrap_or(&Value::Null)))
}
