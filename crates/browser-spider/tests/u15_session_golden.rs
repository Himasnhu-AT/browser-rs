//! Behavioural differential test for `SessionManager` against
//! `reference.spiders.session.SessionManager`.
//!
//! `testdata/u15_session.jsonl` (from `tools/corpus/u15/gen_session.py`) drives Python's
//! manager with a fake session that counts opens, closes and fetches, and records the
//! resulting trace. This test replays each script against the port with an equivalent fake
//! and demands the same trace, pinning the parts a crawl depends on: which session becomes
//! the default (and what happens when it is popped), `start()`'s idempotence and skip
//! rules, lazy sessions opening exactly once on first use, and the
//! `{**request.meta, **response.meta}` merge with the response winning.
//!
//! Rows are never removed; [`DIVERGENCES`] holds what the port cannot reproduce and fails
//! if any of it ever starts matching.

mod u15_support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use browser_spider::error::{Result, SpiderError};
use browser_spider::request::{Item, Request, SessionKwargs};
use browser_spider::session::{AddOptions, CrawlSession, FetchedResponse, SessionManager};
use serde_json::{json, Value};

use u15_support::{build_request, read_corpus};

/// Scripts whose trace deliberately differs from Python, keyed by script name.
///
/// `close()` is the port's one behavioural deviation here and it does not appear in this
/// corpus: Python's `close()` aborts at the first failing `__aexit__` and leaks every
/// remaining session, while the port closes them all and returns the first error. No script
/// exercises a failing close, because the two runtimes would legitimately disagree — the
/// deviation is covered by a unit test in `src/session.rs` and recorded in
/// `docs/DIVERGENCES.md`.
const DIVERGENCES: &[(&str, &str)] = &[];

/// Stands in for a real fetcher, counting what the manager did to it.
struct FakeSession {
    name: String,
    alive: AtomicBool,
    opens: AtomicU64,
    closes: AtomicU64,
    fetches: AtomicU64,
    response_meta: Item,
    fail_open: bool,
    fail_close: bool,
    fail_fetch: bool,
}

impl FakeSession {
    fn new(name: &str, config: &Value) -> Self {
        Self {
            name: name.to_owned(),
            alive: AtomicBool::new(false),
            opens: AtomicU64::new(0),
            closes: AtomicU64::new(0),
            fetches: AtomicU64::new(0),
            response_meta: config
                .get("response_meta")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            fail_open: config
                .get("fail_open")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            fail_close: config
                .get("fail_close")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            fail_fetch: config
                .get("fail_fetch")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    fn counts(&self) -> Value {
        json!({
            "opens": self.opens.load(Ordering::SeqCst),
            "closes": self.closes.load(Ordering::SeqCst),
            "fetches": self.fetches.load(Ordering::SeqCst),
            "alive": self.alive.load(Ordering::SeqCst),
        })
    }

    fn refused(&self, action: &'static str) -> SpiderError {
        SpiderError::SessionLifecycle {
            sid: self.name.clone(),
            action,
            message: format!("{} refused to {action}", self.name),
        }
    }
}

#[async_trait]
impl CrawlSession for FakeSession {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn open(&self) -> Result<()> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if self.fail_open {
            return Err(self.refused("open"));
        }
        self.alive.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.fail_close {
            return Err(self.refused("close"));
        }
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn fetch(&self, url: &str, _kwargs: &SessionKwargs) -> Result<FetchedResponse> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if self.fail_fetch {
            return Err(SpiderError::Fetch {
                sid: self.name.clone(),
                url: url.to_owned(),
                message: format!("{} refused to fetch", self.name),
            });
        }
        let mut response = FetchedResponse::new(200, url, Vec::new());
        // The corpus identifies the answering session by name; `reason` is the one free
        // string field the manager never touches, so it carries it without disturbing meta.
        response.reason = self.name.clone();
        response.meta = self.response_meta.clone();
        Ok(response)
    }
}

async fn replay(row: &Value) -> Vec<Value> {
    let mut manager = SessionManager::new();
    let fakes: Vec<(String, Arc<FakeSession>)> = row["sessions"]
        .as_object()
        .expect("sessions")
        .iter()
        .map(|(name, config)| (name.clone(), Arc::new(FakeSession::new(name, config))))
        .collect();
    let by_name: HashMap<&str, &Arc<FakeSession>> = fakes
        .iter()
        .map(|(name, fake)| (name.as_str(), fake))
        .collect();

    let mut trace = Vec::new();
    let mut last_request: Option<Request> = None;

    for op in row["ops"].as_array().expect("ops") {
        let object = op.as_object().expect("op");
        let (kind, value) = object.iter().next().expect("one key per op");
        match kind.as_str() {
            "add" => {
                let name = value.as_str().expect("session name");
                let options = AddOptions {
                    default: op.get("default").and_then(Value::as_bool).unwrap_or(false),
                    lazy: op.get("lazy").and_then(Value::as_bool).unwrap_or(false),
                };
                let session: Arc<dyn CrawlSession> = Arc::clone(by_name[name]) as _;
                let verdict = if manager.add(name, session, options).is_ok() {
                    "ok"
                } else {
                    "err"
                };
                trace.push(json!({ "add": verdict }));
            }
            "pop" => {
                let name = value.as_str().expect("session name");
                trace.push(match manager.pop(name) {
                    // The port returns the session itself; the corpus records its name, and
                    // ids are unique, so echoing the requested id is equivalent.
                    Ok(_) => json!({ "pop": name }),
                    Err(_) => json!({ "pop": "err" }),
                });
            }
            "get" => {
                let name = value.as_str().expect("session name");
                trace.push(match manager.get(name) {
                    Ok(_) => json!({ "get": name }),
                    Err(_) => json!({ "get": "err" }),
                });
            }
            "default" => trace.push(match manager.default_session_id() {
                Ok(id) => json!({ "default": id }),
                Err(_) => json!({ "default": "err" }),
            }),
            "ids" => trace.push(json!({ "ids": manager.session_ids() })),
            "len" => trace.push(json!({ "len": manager.len() })),
            "contains" => {
                trace.push(json!({ "contains": manager.contains(value.as_str().expect("id")) }))
            }
            "is_lazy" => {
                trace.push(json!({ "is_lazy": manager.is_lazy(value.as_str().expect("id")) }))
            }
            "start" => {
                let verdict = if manager.start().await.is_ok() {
                    "ok"
                } else {
                    "err"
                };
                trace.push(json!({ "start": verdict }));
            }
            "close" => {
                let verdict = if manager.close().await.is_ok() {
                    "ok"
                } else {
                    "err"
                };
                trace.push(json!({ "close": verdict }));
            }
            "fetch" => {
                let request = build_request(value);
                let outcome = manager.fetch(&request).await;
                trace.push(match outcome {
                    Ok(response) => json!({ "fetch": {
                        "session": response.reason,
                        "url": response.url,
                        "meta": Value::Object(response.meta.clone()),
                        "request_is_attached": response
                            .request
                            .as_ref()
                            .is_some_and(|attached| attached.url == request.url),
                    }}),
                    Err(_) => json!({ "fetch": "err" }),
                });
                last_request = Some(request);
            }
            "counts" => {
                let mut counts = serde_json::Map::new();
                for (name, fake) in &fakes {
                    counts.insert(name.clone(), fake.counts());
                }
                trace.push(json!({ "counts": Value::Object(counts) }));
            }
            "request_kwargs_unchanged" => {
                let request = last_request.as_ref().expect("a fetch ran first");
                trace.push(json!({
                    "request_kwargs_unchanged": request.session_kwargs.method.is_some()
                }));
            }
            other => panic!("unknown op {other}"),
        }
    }
    trace
}

#[tokio::test]
async fn session_manager_behaviour_matches_the_reference() {
    let corpus = read_corpus("u15_session.jsonl");
    let divergences: HashMap<&str, &str> = DIVERGENCES.iter().copied().collect();
    let mut seen_divergences: HashMap<String, usize> = HashMap::new();
    let mut failures = Vec::new();
    let mut matched = 0usize;

    for row in &corpus {
        let name = row["name"].as_str().expect("name");
        let expected = row["trace"].as_array().expect("trace");
        let got = replay(row).await;
        let same = got.len() == expected.len() && got.iter().zip(expected).all(|(a, b)| a == b);

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
                serde_json::to_string(expected).unwrap_or_default(),
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
        "{} of {} SessionManager scripts diverge from Python:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
    assert!(
        corpus.len() >= 15,
        "the session corpus shrank to {} scripts",
        corpus.len()
    );
    eprintln!(
        "SessionManager: {matched}/{} scripts behave identically",
        corpus.len()
    );
}
