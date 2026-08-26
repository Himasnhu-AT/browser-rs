//! Differential test of [`launch_options`] / [`context_options`] against the
//! real `BaseSessionMixin.__generate_options__` and
//! `StealthySessionMixin.__generate_stealth_options` from the reference implementation.
//!
//! `testdata/flags_golden.jsonl` is produced by `gen_flags_golden.py` (kept
//! outside the repo), which instantiates the actual session mixins and dumps
//! their `_browser_options`, `_context_options` and `_headers_keys`.
//!
//! # Why `args` is compared as a set
//!
//! Python assembles the flag list with `list(set(tuple(a) + tuple(b)))` as soon
//! as any extra flags are in play, so the order it hands Chromium is
//! hash-random and differs between interpreter runs. The port uses an
//! `IndexSet` for a stable order; only membership is comparable there, so both
//! sides are sorted before the assertion.
//!
//! Where Python's order *is* reproducible — the `list(set(...))` branch is not
//! taken, i.e. a dynamic session with no `extra_flags` — the generator records
//! it as `args_ordered` and the order is asserted exactly. The generator only
//! emits that field when three runs under different `PYTHONHASHSEED`s agree,
//! so a row can never claim an order Python does not actually guarantee.

use std::collections::BTreeSet;

use browser_cdp::config::{BrowserConfig, StealthExtras};
use browser_cdp::flags::{context_options, launch_options, Channel};
use browser_core::types::Proxy;
use serde_json::{json, Map, Value};

const GOLDEN: &str = include_str!("../testdata/flags_golden.jsonl");

fn crate_dir() -> String {
    env!("CARGO_MANIFEST_DIR").to_owned()
}

fn expand(text: &str) -> String {
    text.replace("{CRATE}", &crate_dir())
}

/// Build the config a golden row describes.
fn build(kind: &str, kwargs: &Map<String, Value>) -> BrowserConfig {
    let mut cfg = if kind == "stealth" {
        BrowserConfig::stealthy()
    } else {
        BrowserConfig::new()
    };
    let mut stealth = cfg.stealth.unwrap_or_default();

    for (key, value) in kwargs {
        match key.as_str() {
            "headless" => cfg = cfg.headless(value.as_bool().expect("a bool")),
            "real_chrome" => cfg = cfg.real_chrome(value.as_bool().expect("a bool")),
            "dns_over_https" => cfg = cfg.dns_over_https(value.as_bool().expect("a bool")),
            "block_ads" => cfg = cfg.block_ads(value.as_bool().expect("a bool")),
            "useragent" => cfg = cfg.useragent(value.as_str().expect("a string")),
            "locale" => cfg = cfg.locale(value.as_str().expect("a string")),
            "cdp_url" => cfg = cfg.cdp_url(value.as_str().expect("a string")),
            "user_data_dir" => cfg = cfg.user_data_dir(value.as_str().expect("a string")),
            "executable_path" => {
                cfg = cfg.executable_path(expand(value.as_str().expect("a string")))
            }
            "timezone_id" => cfg.timezone_id = value.as_str().map(str::to_owned),
            "extra_headers" => {
                cfg = cfg.extra_headers(
                    value
                        .as_object()
                        .expect("an object")
                        .iter()
                        .map(|(k, v)| (k.clone(), v.as_str().expect("a string").to_owned())),
                );
            }
            "additional_args" => {
                cfg = cfg.additional_args(value.as_object().expect("an object").clone());
            }
            "extra_flags" => {
                cfg = cfg.extra_flags(
                    value
                        .as_array()
                        .expect("an array")
                        .iter()
                        .map(|f| f.as_str().expect("a string")),
                );
            }
            "proxy" => {
                cfg = cfg.proxy(match value {
                    Value::String(url) => Proxy::Url(url.clone()),
                    Value::Object(map) => Proxy::Config {
                        server: map["server"].as_str().expect("a server").to_owned(),
                        username: map
                            .get("username")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        password: map
                            .get("password")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    },
                    other => panic!("unsupported proxy literal {other}"),
                });
            }
            "block_webrtc" => stealth = stealth.block_webrtc(value.as_bool().expect("a bool")),
            "allow_webgl" => stealth = stealth.allow_webgl(value.as_bool().expect("a bool")),
            "hide_canvas" => stealth = stealth.hide_canvas(value.as_bool().expect("a bool")),
            "solve_cloudflare" => {
                stealth = stealth.solve_cloudflare(value.as_bool().expect("a bool"));
            }
            other => panic!("golden row uses an unmapped kwarg: {other}"),
        }
    }

    if kind == "stealth" {
        cfg = cfg.stealth(stealth);
    }
    cfg
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("a string").to_owned())
        .collect()
}

/// serde_json distinguishes `2` from `2.0`; Python's `device_scale_factor` is
/// the int `2` and this crate's is an `f64`. Compare numerically.
fn normalise(value: &Value) -> Value {
    match value {
        Value::Number(n) => n
            .as_f64()
            .map_or_else(|| value.clone(), |f| json!(format!("num:{f}"))),
        Value::Array(items) => Value::Array(items.iter().map(normalise).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), normalise(v))).collect())
        }
        other => other.clone(),
    }
}

#[test]
fn flags_and_context_match_the_reference() {
    let mut checked = 0_usize;
    let mut ordered_rows = 0_usize;
    let mut failures: Vec<String> = Vec::new();
    fn push(sink: &mut Vec<String>, name: &str, what: &str, want: String, got: String) {
        if want != got {
            sink.push(format!("{name}.{what}: want {want}, got {got}"));
        }
    }

    for line in GOLDEN.lines().filter(|l| !l.trim().is_empty()) {
        let row: Value = serde_json::from_str(line).expect("a golden row");
        let name = row["name"].as_str().expect("a name");
        let kind = row["kind"].as_str().expect("a kind");
        assert!(
            row["ok"].as_bool().unwrap_or(false),
            "{name}: generator failed"
        );
        checked += 1;

        let cfg = build(kind, row["kwargs"].as_object().expect("kwargs"))
            .validate()
            .unwrap_or_else(|e| panic!("{name}: config rejected: {e}"));

        // ---- launch options -------------------------------------------
        match (launch_options(&cfg), row["args"].is_null()) {
            (None, true) => {
                assert!(
                    row["browser_options_keys"]
                        .as_array()
                        .expect("an array")
                        .is_empty(),
                    "{name}: Python kept browser options in CDP mode",
                );
            }
            (Some(opts), false) => {
                let want: BTreeSet<String> = strings(&row["args"]).into_iter().collect();
                let got: BTreeSet<String> = opts.args.iter().cloned().collect();
                push(
                    &mut failures,
                    name,
                    "args",
                    format!("{:?}", want.difference(&got).collect::<Vec<_>>()),
                    format!("{:?}", got.difference(&want).collect::<Vec<_>>()),
                );
                assert_eq!(
                    opts.args.len(),
                    row["args_len"].as_u64().expect("a length") as usize,
                    "{name}: arg count (duplicates collapse the same way)",
                );
                if let Some(ordered) = row.get("args_ordered") {
                    ordered_rows += 1;
                    push(
                        &mut failures,
                        name,
                        "args_ordered",
                        format!("{:?}", strings(ordered)),
                        format!("{:?}", opts.args_vec()),
                    );
                }
                push(
                    &mut failures,
                    name,
                    "ignore_default_args",
                    format!("{:?}", strings(&row["ignore_default_args"])),
                    format!("{:?}", opts.ignore_default_args),
                );
                push(
                    &mut failures,
                    name,
                    "headless",
                    row["headless"].to_string(),
                    opts.headless.to_string(),
                );
                push(
                    &mut failures,
                    name,
                    "channel",
                    row["channel"].as_str().expect("a channel").to_owned(),
                    match opts.channel {
                        Channel::Chrome => "chrome".to_owned(),
                        Channel::Chromium => "chromium".to_owned(),
                    },
                );
                push(
                    &mut failures,
                    name,
                    "executable_path",
                    row["executable_path"]
                        .as_str()
                        .map_or_else(|| "None".to_owned(), expand),
                    opts.executable_path
                        .as_ref()
                        .map_or_else(|| "None".to_owned(), |p| p.display().to_string()),
                );
                push(
                    &mut failures,
                    name,
                    "user_data_dir",
                    row["user_data_dir"].as_str().expect("a dir").to_owned(),
                    opts.user_data_dir.clone(),
                );
            }
            (Some(_), true) => failures.push(format!("{name}: Rust launches, Python does not")),
            (None, false) => failures.push(format!("{name}: Python launches, Rust does not")),
        }

        // ---- context options ------------------------------------------
        let ctx = context_options(&cfg).to_json();
        let want_keys = strings(&row["context_keys"]);
        let got_keys: Vec<String> = ctx.keys().cloned().collect();
        push(
            &mut failures,
            name,
            "context_keys",
            format!("{want_keys:?}"),
            format!("{got_keys:?}"),
        );

        let want_ctx = row["context"].as_object().expect("a context").clone();
        for (key, expected) in &want_ctx {
            let actual = ctx.get(key).unwrap_or(&Value::Null);
            // The default User-Agent is generated per interpreter run, so the
            // generator records only which of the two defaults was picked.
            let (want, got) = match expected.as_str() {
                Some("<default>") => (
                    json!("<default>"),
                    json!(if actual.as_str()
                        == Some(browser_fingerprint::useragent::default_browser_ua())
                    {
                        "<default>"
                    } else {
                        "<other>"
                    }),
                ),
                Some("<default_chrome>") => (
                    json!("<default_chrome>"),
                    json!(if actual.as_str()
                        == Some(browser_fingerprint::useragent::default_chrome_ua())
                    {
                        "<default_chrome>"
                    } else {
                        "<other>"
                    }),
                ),
                _ => (normalise(expected), normalise(actual)),
            };
            push(
                &mut failures,
                name,
                &format!("context.{key}"),
                want.to_string(),
                got.to_string(),
            );
        }

        // ---- cached header keys ---------------------------------------
        let mut got_headers: Vec<String> = cfg.header_keys().into_iter().collect();
        got_headers.sort();
        push(
            &mut failures,
            name,
            "headers_keys",
            format!("{:?}", strings(&row["headers_keys"])),
            format!("{got_headers:?}"),
        );
    }

    assert!(checked >= 45, "corpus shrank to {checked} rows");
    assert!(
        ordered_rows >= 20,
        "only {ordered_rows} rows pin an exact flag order",
    );
    assert!(
        failures.is_empty(),
        "{}/{checked} rows disagree:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

/// `_build_context_with_proxy` swaps the proxy and leaves everything else.
#[test]
fn context_with_proxy_matches_python() {
    let cfg = BrowserConfig::stealthy()
        .stealth(StealthExtras::default().hide_canvas(true))
        .proxy(Proxy::Url("http://a:1".into()))
        .validate()
        .expect("valid");
    let base = context_options(&cfg);
    let swapped = base
        .with_proxy(Some(&Proxy::Url("socks5://U:P@Rot.example:1080".into())))
        .expect("a valid rotation proxy");

    assert_eq!(
        swapped.proxy,
        Some(Proxy::Config {
            server: "socks5://rot.example:1080".into(),
            username: Some("U".into()),
            password: Some("P".into()),
        })
    );
    assert_eq!(base.stealth, swapped.stealth);
    assert_eq!(
        base.with_proxy(None).expect("no proxy is a no-op").proxy,
        base.proxy
    );
}
