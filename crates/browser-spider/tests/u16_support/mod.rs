//! Shared corpus loading for the unit-16 golden tests.
//!
//! The corpora under `testdata/u16_*.jsonl` come from `tools/corpus/u16/gen.py`, which runs
//! the real `reference.spiders.throttle` and the real `protego`. Every row is a recorded
//! fact about Python; a row this port cannot reproduce is declared as a divergence rather
//! than deleted, and the declaration is itself asserted (a divergence that starts passing
//! fails the test).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

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
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "could not read {} ({error}). Regenerate it with tools/corpus/u16/gen.py.",
            path.display()
        )
    });
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(number, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("{name} line {}: {error}", number + 1))
        })
        .collect()
}

/// `Option<f64>` from a JSON `null`-or-number.
pub fn optional_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Null => None,
        other => Some(other.as_f64().expect("a number or null")),
    }
}

/// `Option<f64>` from a recorded IEEE-754 **bit pattern**.
///
/// The corpora record every float twice: as a readable JSON number and as its exact bits.
/// The bits are what the tests compare, because this workspace builds `serde_json` without
/// its `float_roundtrip` feature and the fast decimal parser can land one ULP away from the
/// correctly rounded value. Reading the bits takes the harness out of the comparison, so a
/// mismatch always means the port's arithmetic really differs from Python's.
pub fn f64_from_bits(value: &Value) -> Option<f64> {
    match value {
        Value::Null => None,
        other => Some(f64::from_bits(
            other.as_u64().expect("a u64 bit pattern or null"),
        )),
    }
}

/// A declared divergence: a corpus row the port deliberately does not reproduce.
///
/// The reason is compiled in, so a reader of the test knows *why* without leaving the file,
/// and [`assert_divergences`] fails if one of them silently starts passing.
pub struct Divergence {
    /// Something that identifies the row, matched exactly against the key the test builds.
    pub key: &'static str,
    /// Why the port answers differently.
    pub reason: &'static str,
}

/// Check a set of declared divergences against what actually happened.
///
/// * every key in `failures` must be declared, otherwise it is a real regression;
/// * every declared key must be in `failures`, otherwise the declaration is stale and the
///   note in the docs has quietly become a lie.
pub fn assert_divergences(what: &str, declared: &[Divergence], failures: &[(String, String)]) {
    let mut undeclared: Vec<&(String, String)> = Vec::new();
    for failure in failures {
        if !declared.iter().any(|entry| entry.key == failure.0) {
            undeclared.push(failure);
        }
    }
    let mut stale: Vec<&str> = Vec::new();
    for entry in declared {
        if !failures.iter().any(|failure| failure.0 == entry.key) {
            stale.push(entry.key);
        }
    }

    assert!(
        undeclared.is_empty(),
        "{what}: {} undeclared mismatches against Python.\n{}",
        undeclared.len(),
        undeclared
            .iter()
            .take(25)
            .map(|(key, detail)| format!("  {key}: {detail}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        stale.is_empty(),
        "{what}: {} declared divergences now REPRODUCE. Delete the declaration and update \
         docs/DIVERGENCES.md.\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}
