//! Differential test of [`FetchParams::resolve`] against the real
//! `reference.engines._browsers._validators.validate_fetch`.
//!
//! `testdata/fetch_params_golden.jsonl` is produced by `gen_fetch_golden.py`
//! (kept outside the repo): each row is a session config plus a `fetch()`
//! keyword dict, and records the resulting `_fetch_params` dataclass.
//!
//! The corpus exists mainly to pin one non-obvious rule: `validate_fetch`
//! re-validates the *override dict alone* against a fresh model, so the
//! `solve_cloudflare` timeout floor applies only when both keys are overridden
//! in the same call.

use std::collections::HashSet;

use browser_cdp::config::{BrowserConfig, FetchParams, StealthExtras};
use browser_core::types::{Override, WaitState};
use serde_json::{json, Map, Value};

const GOLDEN: &str = include_str!("../testdata/fetch_params_golden.jsonl");

fn wait_state(name: &str) -> WaitState {
    match name {
        "attached" => WaitState::Attached,
        "detached" => WaitState::Detached,
        "hidden" => WaitState::Hidden,
        "visible" => WaitState::Visible,
        other => panic!("unknown wait_selector_state {other}"),
    }
}

fn headers(value: &Value) -> Option<indexmap::IndexMap<String, String>> {
    value.as_object().map(|map| {
        map.iter()
            .map(|(k, v)| (k.clone(), v.as_str().expect("a string").to_owned()))
            .collect()
    })
}

fn domains(value: &Value) -> Option<HashSet<String>> {
    value.as_array().map(|list| {
        list.iter()
            .map(|d| d.as_str().expect("a string").to_owned())
            .collect()
    })
}

fn session_config(model: &str, kwargs: &Map<String, Value>) -> BrowserConfig {
    let mut cfg = if model == "StealthConfig" {
        BrowserConfig::stealthy()
    } else {
        BrowserConfig::new()
    };
    let mut stealth = cfg.stealth.unwrap_or_default();

    for (key, value) in kwargs {
        match key.as_str() {
            "timeout" => cfg = cfg.timeout_ms(value.as_f64().expect("a number")),
            "wait" => cfg = cfg.wait_ms(value.as_f64().expect("a number")),
            "google_search" => cfg = cfg.google_search(value.as_bool().expect("a bool")),
            "network_idle" => cfg = cfg.network_idle(value.as_bool().expect("a bool")),
            "load_dom" => cfg = cfg.load_dom(value.as_bool().expect("a bool")),
            "disable_resources" => {
                cfg = cfg.disable_resources(value.as_bool().expect("a bool"));
            }
            "block_ads" => cfg = cfg.block_ads(value.as_bool().expect("a bool")),
            "wait_selector" => cfg = cfg.wait_selector(value.as_str().expect("a string")),
            "wait_selector_state" => {
                cfg = cfg.wait_selector_state(wait_state(value.as_str().expect("a string")));
            }
            "blocked_domains" => {
                cfg = cfg.blocked_domains(
                    value
                        .as_array()
                        .expect("an array")
                        .iter()
                        .map(|d| d.as_str().expect("a string")),
                );
            }
            "selector_config" => {
                cfg = cfg.selector_config(value.as_object().expect("an object").clone());
            }
            "extra_headers" => {
                cfg = cfg.extra_headers(
                    value
                        .as_object()
                        .expect("an object")
                        .iter()
                        .map(|(k, v)| (k.clone(), v.as_str().expect("a string").to_owned())),
                );
            }
            "solve_cloudflare" => {
                stealth = stealth.solve_cloudflare(value.as_bool().expect("a bool"));
            }
            other => panic!("golden row uses an unmapped session kwarg: {other}"),
        }
    }
    if model == "StealthConfig" {
        cfg = cfg.stealth(stealth);
    }
    cfg
}

fn fetch_params(kwargs: &Map<String, Value>) -> FetchParams {
    let mut params = FetchParams::new();
    for (key, value) in kwargs {
        match key.as_str() {
            "timeout" => params.timeout_ms = Override::Set(value.as_f64().expect("a number")),
            "wait" => params.wait_ms = Override::Set(value.as_f64().expect("a number")),
            "google_search" => {
                params.google_search = Override::Set(value.as_bool().expect("a bool"));
            }
            "network_idle" => {
                params.network_idle = Override::Set(value.as_bool().expect("a bool"));
            }
            "load_dom" => params.load_dom = Override::Set(value.as_bool().expect("a bool")),
            "disable_resources" => {
                params.disable_resources = Override::Set(value.as_bool().expect("a bool"));
            }
            "solve_cloudflare" => {
                params.solve_cloudflare = Override::Set(value.as_bool().expect("a bool"));
            }
            "wait_selector" => {
                params.wait_selector = Override::Set(value.as_str().map(str::to_owned));
            }
            "wait_selector_state" => {
                params.wait_selector_state =
                    Override::Set(wait_state(value.as_str().expect("a string")));
            }
            "extra_headers" => params.extra_headers = Override::Set(headers(value)),
            "blocked_domains" => params.blocked_domains = Override::Set(domains(value)),
            "selector_config" => {
                params.selector_config =
                    Override::Set(value.as_object().cloned().unwrap_or_default());
            }
            other => panic!("golden row uses an unmapped fetch kwarg: {other}"),
        }
    }
    params
}

/// Mirror the generator's `blocked_domains` rendering.
fn dump_domains(value: Option<&HashSet<String>>) -> Value {
    let Some(set) = value else { return Value::Null };
    let mut ordered: Vec<&str> = set.iter().map(String::as_str).collect();
    ordered.sort_unstable();
    if ordered.len() <= 40 {
        return json!(ordered);
    }
    json!({
        "kind": "digest",
        "len": ordered.len(),
        "first": ordered[..3],
        "last": ordered[ordered.len() - 3..],
    })
}

#[test]
fn fetch_params_match_the_reference() {
    let mut checked = 0_usize;
    let mut failures: Vec<String> = Vec::new();

    for line in GOLDEN.lines().filter(|l| !l.trim().is_empty()) {
        let row: Value = serde_json::from_str(line).expect("a golden row");
        let name = row["name"].as_str().expect("a name");
        let model = row["model"].as_str().expect("a model");
        checked += 1;

        let config = session_config(model, row["session"].as_object().expect("session kwargs"))
            .validate()
            .unwrap_or_else(|e| panic!("{name}: session config rejected: {e}"));
        let params = fetch_params(row["fetch"].as_object().expect("fetch kwargs"));

        match (
            params.resolve(&config),
            row["ok"].as_bool().unwrap_or(false),
        ) {
            (Ok(resolved), true) => {
                let want = row["params"].as_object().expect("params").clone();
                let mut got = Map::new();
                got.insert("google_search".into(), json!(resolved.google_search));
                got.insert("timeout".into(), json!(resolved.timeout_ms));
                got.insert("wait".into(), json!(resolved.wait_ms));
                got.insert(
                    "extra_headers".into(),
                    resolved.extra_headers.as_ref().map_or(Value::Null, |h| {
                        Value::Object(
                            h.iter()
                                .map(|(k, v)| (k.clone(), Value::from(v.clone())))
                                .collect(),
                        )
                    }),
                );
                got.insert(
                    "disable_resources".into(),
                    json!(resolved.disable_resources),
                );
                got.insert("wait_selector".into(), json!(resolved.wait_selector));
                got.insert(
                    "wait_selector_state".into(),
                    json!(resolved.wait_selector_state.as_str()),
                );
                got.insert("network_idle".into(), json!(resolved.network_idle));
                got.insert("load_dom".into(), json!(resolved.load_dom));
                got.insert(
                    "blocked_domains".into(),
                    dump_domains(resolved.blocked_domains.as_ref()),
                );
                got.insert("solve_cloudflare".into(), json!(resolved.solve_cloudflare));
                got.insert(
                    "selector_config".into(),
                    Value::Object(resolved.selector_config.clone()),
                );

                for (key, expected) in &want {
                    let actual = got.get(key).unwrap_or(&Value::Null);
                    // `2` vs `2.0`.
                    let same = expected.as_f64().is_some_and(|want| {
                        actual
                            .as_f64()
                            .is_some_and(|got| (want - got).abs() < f64::EPSILON)
                    }) || actual == expected;
                    if !same {
                        failures.push(format!("{name}.{key}: want {expected}, got {actual}"));
                    }
                }
            }
            (Err(err), false) => {
                let want = row["msg"].as_str().expect("a message");
                let got = err.python_message();
                if got != want {
                    failures.push(format!("{name}: want message {want:?}, got {got:?}"));
                }
            }
            (Ok(_), false) => {
                failures.push(format!("{name}: Rust accepted it, Python raised"));
            }
            (Err(err), true) => {
                failures.push(format!("{name}: Rust rejected it with {err}"));
            }
        }
    }

    assert!(checked >= 25, "corpus shrank to {checked} rows");
    assert!(
        failures.is_empty(),
        "{}/{checked} rows disagree:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

/// The `referer` rule from `fetch()`: Google's referer is injected only when
/// `google_search` is on and the caller did not already set a `Referer`.
#[test]
fn google_referer_respects_an_explicit_header() {
    let config = BrowserConfig::stealthy()
        .stealth(StealthExtras::default())
        .validate()
        .expect("valid");

    let plain = FetchParams::new().resolve(&config).expect("resolves");
    assert_eq!(plain.referer(), Some("https://www.google.com/"));

    let mut with_referer = FetchParams::new();
    with_referer.extra_headers = Override::Set(Some(
        [("ReFeReR".to_owned(), "https://x.test/".to_owned())]
            .into_iter()
            .collect(),
    ));
    let resolved = with_referer.resolve(&config).expect("resolves");
    assert_eq!(resolved.referer(), None, "case-insensitive header match");
    assert_eq!(
        resolved.header_keys(),
        HashSet::from(["referer".to_owned()])
    );
}
