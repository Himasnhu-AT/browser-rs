//! Behavioural differential test for `CheckpointManager`.
//!
//! The port stores checkpoints as versioned JSON where Python pickles, so a byte-level
//! golden is impossible by construction (declared in `docs/DIVERGENCES.md`). What is
//! comparable — and what a resumed crawl actually depends on — is the observable
//! behaviour, and `testdata/u15_checkpoint.jsonl` records Python's:
//!
//! * the files present in the crawl directory after every step, which pins the
//!   `checkpoint.tmp` + atomic-rename dance and the temp-file cleanup;
//! * whether `save` **raised** while `cleanup` and `load` **swallowed** — the asymmetry the
//!   engine's pause path is built on (a failed save must not look like a successful pause);
//! * what `load` gave back, which is the restore round trip.
//!
//! Rows are never removed; [`DIVERGENCES`] holds the ones the port cannot reproduce and
//! fails if any of them ever starts matching.

mod u15_support;

use std::collections::HashMap;
use std::path::Path;

use browser_spider::checkpoint::{
    CheckpointData, CheckpointManager, CHECKPOINT_FILE, CHECKPOINT_TEMP_FILE,
};
use browser_spider::request::Request;
use serde_json::{json, Value};

use u15_support::{build_request, read_corpus};

/// Scripts whose trace deliberately differs from Python, keyed by script name.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "interval-nan",
        "Python's constructor only rejects a *negative* interval — `isinstance(nan, float)` \
         is true and `nan < 0` is false, so `CheckpointManager(dir, nan)` is accepted and \
         the crawl later sleeps for NaN seconds. The port rejects it up front with \
         `SpiderError::CheckpointIntervalNotANumber`, because `Duration::from_secs_f64(NaN)` \
         panics: accepting it would turn a caller's typo into an abort inside a spawned \
         task. Constructing is the only place this can be reported.",
    ),
    (
        "interval-infinity",
        "Same as `interval-nan`: `inf < 0` is false so Python accepts it, but \
         `Duration::from_secs_f64(inf)` panics (it overflows the `Duration` range), so the \
         port rejects an infinite interval at construction.",
    ),
];

/// `"ok"` vs `"raise:<PythonExceptionType>"`, collapsed to just ok/err.
///
/// The two runtimes cannot agree on exception *types* — Python raises `FileExistsError`
/// where the port returns `SpiderError::CheckpointSave` — so only the verdict is compared.
fn verdict(value: &Value) -> &str {
    match value.as_str() {
        Some("ok") => "ok",
        Some(_) => "err",
        None => panic!("expected an ok/raise verdict, got {value}"),
    }
}

fn alias(name: &str) -> &str {
    match name {
        "CHECKPOINT" => CHECKPOINT_FILE,
        "TEMP" => CHECKPOINT_TEMP_FILE,
        other => other,
    }
}

fn unalias(filename: &str) -> String {
    if filename == CHECKPOINT_FILE {
        "CHECKPOINT".to_owned()
    } else if filename == CHECKPOINT_TEMP_FILE {
        "TEMP".to_owned()
    } else {
        filename.to_owned()
    }
}

fn list_files(crawldir: &Path) -> Value {
    if !crawldir.is_dir() {
        return Value::Null;
    }
    let mut names: Vec<String> = std::fs::read_dir(crawldir)
        .expect("read_dir")
        .map(|entry| unalias(&entry.expect("entry").file_name().to_string_lossy()))
        .collect();
    names.sort();
    json!(names)
}

fn make_data(row: &Value, index: usize) -> CheckpointData {
    let spec = &row["data_sets"][index];
    let requests: Vec<Request> = spec["requests"]
        .as_array()
        .expect("request indices")
        .iter()
        .map(|i| {
            let request = build_request(&row["requests"][i.as_u64().expect("index") as usize]);
            request.update_fingerprint(Default::default());
            request
        })
        .collect();
    let seen = (0..spec["seen"].as_u64().expect("seen count") as u8)
        .map(|byte| [byte; 20])
        .collect();
    CheckpointData { requests, seen }
}

fn summarise(data: Option<&CheckpointData>) -> Value {
    let Some(data) = data else {
        return Value::Null;
    };
    let mut seen: Vec<String> = data.seen.iter().map(hex::encode).collect();
    seen.sort();
    json!({
        "requests": data.requests.iter().map(|r| json!({
            "url": r.url,
            "sid": r.sid,
            "priority": r.priority,
            "dont_filter": r.dont_filter,
            "meta": Value::Object(r.meta.clone()),
            "retry_count": r.retry_count,
            "fingerprint": r.fingerprint().map(hex::encode),
        })).collect::<Vec<_>>(),
        "seen": seen,
    })
}

async fn replay(row: &Value, root: &Path) -> Vec<Value> {
    let mut trace = Vec::new();
    let crawldir = root.join(row.get("subdir").and_then(Value::as_str).unwrap_or("crawl"));

    if row
        .get("crawldir_is_a_file")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(parent) = crawldir.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(&crawldir, "not a directory").expect("write the blocking file");
    }

    let interval = match &row["interval"] {
        Value::String(tag) => match tag.as_str() {
            "nan" => f64::NAN,
            "inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            other => panic!("unknown interval tag {other}"),
        },
        number => number.as_f64().expect("interval"),
    };

    let manager = match CheckpointManager::new(&crawldir, interval) {
        Ok(manager) => {
            trace.push(json!({ "construct": "ok" }));
            manager
        }
        Err(_) => {
            trace.push(json!({ "construct": "err" }));
            return trace;
        }
    };

    for op in row["ops"].as_array().expect("ops") {
        match op["op"].as_str().expect("op kind") {
            "save" => {
                let data = make_data(row, op["data"].as_u64().expect("data index") as usize);
                let verdict = if manager.save(&data).await.is_ok() {
                    "ok"
                } else {
                    "err"
                };
                trace.push(json!({ "save": verdict }));
            }
            "load" => trace.push(json!({ "load": summarise(manager.load().await.as_ref()) })),
            "cleanup" => {
                // The port's `cleanup` cannot fail — that IS the asymmetry under test.
                manager.cleanup().await;
                trace.push(json!({ "cleanup": "ok" }));
            }
            "has" => trace.push(json!({ "has": manager.has_checkpoint().await })),
            "files" => trace.push(json!({ "files": list_files(&crawldir) })),
            "write_raw" => {
                std::fs::create_dir_all(&crawldir).expect("crawldir");
                std::fs::write(
                    crawldir.join(alias(op["name"].as_str().expect("name"))),
                    op["content"].as_str().expect("content"),
                )
                .expect("write_raw");
            }
            "truncate" => {
                let path = crawldir.join(alias(op["name"].as_str().expect("name")));
                let content = std::fs::read(&path).expect("read");
                let keep = op["keep"].as_u64().expect("keep") as usize;
                std::fs::write(&path, &content[..keep.min(content.len())]).expect("truncate");
            }
            "mkdir" => {
                std::fs::create_dir_all(crawldir.join(alias(op["name"].as_str().expect("name"))))
                    .expect("mkdir");
            }
            "interval" => trace.push(json!({ "interval": manager.interval() })),
            other => panic!("unknown op {other}"),
        }
    }
    trace
}

/// Collapses Python's `"raise:<Type>"` verdicts to `"err"` so the two sides are comparable.
fn normalise_expected(trace: &[Value]) -> Vec<Value> {
    trace
        .iter()
        .map(|entry| {
            let object = entry.as_object().expect("trace entry");
            let (key, value) = object.iter().next().expect("one key");
            match key.as_str() {
                "construct" | "save" | "cleanup" => json!({ key.clone(): verdict(value) }),
                _ => entry.clone(),
            }
        })
        .collect()
}

#[tokio::test]
async fn checkpoint_behaviour_matches_the_reference() {
    let corpus = read_corpus("u15_checkpoint.jsonl");
    let divergences: HashMap<&str, &str> = DIVERGENCES.iter().copied().collect();
    let mut seen_divergences: HashMap<String, usize> = HashMap::new();
    let mut failures = Vec::new();
    let mut matched = 0usize;

    for row in &corpus {
        let name = row["name"].as_str().expect("name");
        let expected = normalise_expected(row["trace"].as_array().expect("trace"));
        let dir = tempfile::tempdir().expect("tempdir");
        let got = replay(row, dir.path()).await;
        let same = got == expected;

        match divergences.get(name) {
            Some(why) => {
                *seen_divergences.entry(name.to_owned()).or_default() += 1;
                assert!(
                    !same,
                    "declared divergence {name:?} now MATCHES Python. Delete the row from \
                     DIVERGENCES — the port improved. Reason recorded was: {why}"
                );
            }
            None if same => matched += 1,
            None => failures.push(format!(
                "  {name}:\n    python: {}\n    rust:   {}",
                serde_json::to_string(&expected).unwrap_or_default(),
                serde_json::to_string(&got).unwrap_or_default()
            )),
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
        "{} of {} checkpoint scripts diverge from Python:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
    assert!(
        corpus.len() >= 15,
        "the checkpoint corpus shrank to {} scripts",
        corpus.len()
    );
    eprintln!(
        "checkpoint: {matched}/{} scripts behave identically ({} declared divergences)",
        corpus.len(),
        DIVERGENCES.len()
    );
}
