//! Replay of `testdata/u19_cli.jsonl` — the command-line differential.
//!
//! The corpus was produced by driving **the reference implementation 0.4.15's real Click CLI**
//! through `click.testing.CliRunner` with the fetch replaced by a recorder
//! (`tools/corpus/u19/gen_cli.py`), so every row is the exact kwargs dict
//! `Fetcher.get` / `DynamicFetcher.fetch` / `StealthyFetcher.fetch` would have
//! received — plus the exit code Click produced.
//!
//! It is the one corpus that covers the whole argument surface at once: the
//! defaults, the `--flag` / `--no-flag` pairs and their last-wins ordering, the
//! `-p key=value` loop, `--impersonate`'s comma split, the
//! `BROWSER_RS_EXECUTABLE_PATH` precedence, and — the reason it exists — the
//! **seconds-versus-milliseconds split** between the two option groups.
//!
//! Comparison happens against [`build_http_plan`] and
//! [`assemble_browser_config`], the *unvalidated* assembly, because Python
//! records its dict before `PlaywrightConfig.__post_init__` runs.
//!
//! No Python, no network: the corpus is committed JSON and the replay is pure
//! Rust.

mod support;

use std::collections::BTreeMap;

use clap::Parser;
use browser::Impersonate;
use browser_cli::args::{Cli, Command, ExtractCommand};
use browser_cli::commands::extract::{build_http_plan, HttpPlan};
use serde_json::Value;

/// Rows whose Rust answer is deliberately not Python's, with the reason.
const DIVERGENCES: &[(&str, &str)] = &[];

struct Tally {
    checked: usize,
    matched: usize,
    failures: Vec<String>,
}

impl Tally {
    fn new() -> Self {
        Self {
            checked: 0,
            matched: 0,
            failures: Vec::new(),
        }
    }

    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, row: &str, field: &str, want: &T, got: &T) {
        self.checked += 1;
        if want == got {
            self.matched += 1;
        } else {
            self.failures
                .push(format!("{row}.{field}: python {want:?}, rust {got:?}"));
        }
    }
}

fn corpus() -> Vec<Value> {
    include_str!("../testdata/u19_cli.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON row"))
        .collect()
}

fn argv_of(row: &Value) -> Vec<String> {
    std::iter::once("browser-rs".to_owned())
        .chain(
            row["argv"]
                .as_array()
                .expect("argv")
                .iter()
                .map(|arg| arg.as_str().expect("a string").to_owned()),
        )
        .collect()
}

/// Python's `None`-or-dict, flattened to a comparable map (`None` == `{}`,
/// because `_merge_request_args` drops a `None` and `_headers_job` treats it as
/// `{}`).
fn map_or_empty(value: &Value) -> BTreeMap<String, String> {
    value.as_object().map_or_else(BTreeMap::new, |object| {
        object
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().expect("a string").to_owned()))
            .collect()
    })
}

fn rust_map<'a, I>(pairs: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (&'a String, &'a String)>,
{
    pairs
        .into_iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// `impersonate` is `None`, a `str` or a `list[str]`.
fn want_impersonate(value: Option<&Value>) -> Impersonate {
    match value {
        None | Some(Value::Null) => Impersonate::None,
        Some(Value::String(single)) => Impersonate::single(single.clone()),
        Some(Value::Array(pool)) => Impersonate::pool(
            pool.iter()
                .map(|item| item.as_str().expect("a string").to_owned()),
        ),
        Some(other) => panic!("unexpected impersonate {other:?}"),
    }
}

fn compare_http(id: &str, kwargs: &Value, plan: &HttpPlan, tally: &mut Tally) {
    tally.eq(
        id,
        "headers",
        &map_or_empty(&kwargs["headers"]),
        &rust_map(plan.headers.iter()),
    );
    tally.eq(
        id,
        "cookies",
        &map_or_empty(&kwargs["cookies"]),
        &rust_map(plan.cookies.iter()),
    );
    // `params` is absent when empty; `Query` is a `Vec`, and Python's is a dict.
    tally.eq(
        id,
        "params",
        &map_or_empty(kwargs.get("params").unwrap_or(&Value::Null)),
        &plan
            .params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    );
    tally.eq(
        id,
        "json",
        &kwargs.get("json").cloned().unwrap_or(Value::Null),
        &plan.json.clone().unwrap_or(Value::Null),
    );
    tally.eq(
        id,
        "data",
        &kwargs
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_owned),
        &plan.data.clone(),
    );
    tally.eq(
        id,
        "timeout_seconds",
        &kwargs["timeout"].as_u64().expect("an int"),
        &plan.timeout.as_secs(),
    );
    tally.eq(
        id,
        "follow_redirects",
        &kwargs["follow_redirects"].as_bool().expect("a bool"),
        &matches!(
            plan.follow_redirects,
            browser::FollowRedirects::Bool(true)
        ),
    );
    tally.eq(
        id,
        "verify",
        &kwargs["verify"].as_bool().expect("a bool"),
        &plan.verify,
    );
    tally.eq(
        id,
        "stealthy_headers",
        &kwargs["stealthy_headers"].as_bool().expect("a bool"),
        &plan.stealthy_headers,
    );
    tally.eq(
        id,
        "impersonate",
        &want_impersonate(kwargs.get("impersonate")),
        &plan.impersonate,
    );
    tally.eq(
        id,
        "proxy",
        &kwargs["proxy"].as_str().map(str::to_owned),
        &plan.proxy.as_ref().map(|proxy| match proxy {
            browser::Proxy::Url(url) => url.clone(),
            browser::Proxy::Config { server, .. } => server.clone(),
        }),
    );
}

#[cfg(feature = "browser")]
fn compare_browser(
    id: &str,
    kwargs: &Value,
    config: &browser::BrowserConfig,
    resolved_executable: &Option<String>,
    stealthy: bool,
    tally: &mut Tally,
) {
    let want_bool = |key: &str| kwargs[key].as_bool().expect("a bool");
    tally.eq(id, "headless", &want_bool("headless"), &config.headless);
    tally.eq(
        id,
        "disable_resources",
        &want_bool("disable_resources"),
        &config.disable_resources,
    );
    tally.eq(
        id,
        "network_idle",
        &want_bool("network_idle"),
        &config.network_idle,
    );
    tally.eq(
        id,
        "real_chrome",
        &want_bool("real_chrome"),
        &config.real_chrome,
    );
    tally.eq(
        id,
        "dns_over_https",
        &want_bool("dns_over_https"),
        &config.dns_over_https,
    );
    tally.eq(id, "block_ads", &want_bool("block_ads"), &config.block_ads);

    // The unit that matters: Python's `timeout` here is milliseconds.
    tally.eq(
        id,
        "timeout_milliseconds",
        &kwargs["timeout"].as_f64().expect("a number"),
        &config.timeout_ms,
    );
    // `wait` is present only when `> 0`; absent means the default `0`.
    tally.eq(
        id,
        "wait_milliseconds",
        &kwargs.get("wait").and_then(Value::as_f64).unwrap_or(0.0),
        &config.wait_ms,
    );
    tally.eq(
        id,
        "wait_selector",
        &kwargs
            .get("wait_selector")
            .and_then(Value::as_str)
            .map(str::to_owned),
        &config.wait_selector.clone(),
    );
    tally.eq(
        id,
        "locale",
        &kwargs["locale"].as_str().map(str::to_owned),
        &config.locale.clone(),
    );
    tally.eq(
        id,
        "proxy",
        &kwargs
            .get("proxy")
            .and_then(Value::as_str)
            .map(str::to_owned),
        &config.proxy.as_ref().map(|proxy| match proxy {
            browser::Proxy::Url(url) => url.clone(),
            browser::Proxy::Config { server, .. } => server.clone(),
        }),
    );
    tally.eq(
        id,
        "extra_headers",
        &kwargs
            .get("extra_headers")
            .map(map_or_empty)
            .unwrap_or_default(),
        &config
            .extra_headers
            .as_ref()
            .map(|headers| rust_map(headers.iter()))
            .unwrap_or_default(),
    );
    // The precedence test: `--executable-path` > `$BROWSER_RS_EXECUTABLE_PATH` >
    // **absent**, where "absent" means the key is not in the kwargs at all.
    //
    // It is compared against `resolved_executable`, the answer the *pure*
    // `resolve_executable_path` gave for this row's environment, rather than
    // against `config.executable_path`. That is deliberate: reproducing the
    // row by mutating the process environment would leak `set_var` into a
    // multi-threaded test binary, and the pure function is where the whole of
    // the precedence rule lives — `BrowserOptions::resolve_executable_path` is
    // a two-line wrapper that supplies `std::env::var_os`, and its doc test
    // covers that wrapping.
    tally.eq(
        id,
        "executable_path",
        &kwargs
            .get("executable_path")
            .and_then(Value::as_str)
            .map(str::to_owned),
        resolved_executable,
    );

    if stealthy {
        let extras = config.stealth.as_ref().expect("stealth extras");
        tally.eq(
            id,
            "block_webrtc",
            &want_bool("block_webrtc"),
            &extras.block_webrtc,
        );
        tally.eq(
            id,
            "solve_cloudflare",
            &want_bool("solve_cloudflare"),
            &extras.solve_cloudflare,
        );
        tally.eq(
            id,
            "allow_webgl",
            &want_bool("allow_webgl"),
            &extras.allow_webgl,
        );
        tally.eq(
            id,
            "hide_canvas",
            &want_bool("hide_canvas"),
            &extras.hide_canvas,
        );
    } else {
        tally.eq(id, "stealth_absent", &true, &config.stealth.is_none());
    }
}

#[test]
fn the_command_line_reproduces_pythons_kwargs() {
    let rows = corpus();
    assert!(
        rows.len() >= 55,
        "the corpus is 60 rows, got {}",
        rows.len()
    );

    let mut tally = Tally::new();
    let mut divergent_still_differ: Vec<String> = Vec::new();

    for row in &rows {
        let id = row["id"].as_str().expect("an id").to_owned();
        let declared = DIVERGENCES.iter().any(|(name, _)| *name == id);
        // A declared row is compared into a throw-away tally, so it contributes
        // to the headline number in neither direction.
        let mut sink = Tally::new();
        let target: &mut Tally = if declared { &mut sink } else { &mut tally };

        let argv = argv_of(row);
        let exit_code = row["exit_code"].as_u64().expect("an exit code");
        let parsed = Cli::try_parse_from(&argv);

        if exit_code == 2 {
            // Click exits 2 for usage errors; `clap` must refuse the same lines.
            target.eq(
                &id,
                "usage_error",
                &2_i32,
                &parsed.as_ref().err().map_or(0, clap::Error::exit_code),
            );
            if declared && sink.failures.is_empty() {
                divergent_still_differ.push(id);
            }
            continue;
        }

        let Ok(cli) = parsed else {
            target.checked += 1;
            target
                .failures
                .push(format!("{id}: rust refused a command line Python accepted"));
            continue;
        };

        if exit_code == 1 {
            // Click parsed it, then the command body raised. The only such row
            // is `--json '{oops}'`; it is checked in detail by
            // `an_invalid_json_body_is_rejected_with_pythons_message`, so here
            // it is enough that the plan build fails too.
            let failed = match cli.command {
                Command::Extract {
                    command: ExtractCommand::Post(command),
                } => build_http_plan(&command.options, Some(&command.body)).is_err(),
                Command::Extract {
                    command: ExtractCommand::Put(command),
                } => build_http_plan(&command.options, Some(&command.body)).is_err(),
                _ => false,
            };
            target.eq(&id, "runtime_error", &true, &failed);
            if declared && sink.failures.is_empty() {
                divergent_still_differ.push(id);
            }
            continue;
        }

        // The environment the row asks for, applied around the one call that
        // reads it. Tests share a process, so this is scoped as tightly as
        // possible and restored immediately.
        let env: Vec<(String, Option<String>)> = row["env"]
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().map(str::to_owned)))
                    .collect()
            })
            .unwrap_or_default();

        match cli.command {
            Command::Extract { command } => match command {
                ExtractCommand::Get(command) | ExtractCommand::Delete(command) => {
                    check_http(&id, row, &command.targets, &command.options, None, target);
                }
                ExtractCommand::Post(command) | ExtractCommand::Put(command) => {
                    check_http(
                        &id,
                        row,
                        &command.targets,
                        &command.options,
                        Some(&command.body),
                        target,
                    );
                }
                #[cfg(feature = "browser")]
                ExtractCommand::Fetch(command) => {
                    check_browser(
                        &id,
                        row,
                        &command.targets,
                        &command.options,
                        None,
                        &env,
                        target,
                    );
                }
                #[cfg(feature = "browser")]
                ExtractCommand::StealthyFetch(command) => {
                    let options = command.options.clone();
                    check_browser(
                        &id,
                        row,
                        &command.targets,
                        &options,
                        Some(&command),
                        &env,
                        target,
                    );
                }
            },
            other => panic!("{id}: unexpected command {other:?}"),
        }

        if declared && sink.failures.is_empty() {
            divergent_still_differ.push(id);
        }
    }

    assert!(
        divergent_still_differ.is_empty(),
        "these rows are declared as divergences but now reproduce Python: \
         {divergent_still_differ:?}"
    );
    assert!(
        tally.failures.is_empty(),
        "{}/{} field checks reproduce Python; {} failed:\n{}",
        tally.matched,
        tally.checked,
        tally.failures.len(),
        tally.failures.join("\n")
    );
    assert!(
        tally.checked >= 400,
        "only {} field checks ran; the corpus must have shrunk",
        tally.checked
    );
    println!(
        "cli: {}/{} field checks over {} rows",
        tally.matched,
        tally.checked,
        rows.len()
    );
}

fn check_http(
    id: &str,
    row: &Value,
    targets: &browser_cli::args::Targets,
    options: &browser_cli::args::HttpOptions,
    body: Option<&browser_cli::args::DataOptions>,
    tally: &mut Tally,
) {
    tally.eq(
        id,
        "url",
        &row["url"].as_str().expect("a url").to_owned(),
        &targets.url,
    );
    tally.eq(
        id,
        "output_file",
        &row["output_file"].as_str().expect("a path").to_owned(),
        &targets.output_file,
    );
    tally.eq(
        id,
        "css_selector",
        &row["css_selector"].as_str().map(str::to_owned),
        &options.css_selector.clone(),
    );
    tally.eq(
        id,
        "ai_targeted",
        &row["ai_targeted"].as_bool().expect("a bool"),
        &options.ai_targeted,
    );
    match build_http_plan(options, body) {
        Ok(plan) => compare_http(id, &row["kwargs"], &plan, tally),
        Err(error) => {
            tally.checked += 1;
            tally
                .failures
                .push(format!("{id}: rust failed to build the plan: {error}"));
        }
    }
}

#[cfg(feature = "browser")]
fn check_browser(
    id: &str,
    row: &Value,
    targets: &browser_cli::args::Targets,
    options: &browser_cli::args::BrowserOptions,
    stealth: Option<&browser_cli::args::StealthyCommand>,
    env: &[(String, Option<String>)],
    tally: &mut Tally,
) {
    tally.eq(
        id,
        "url",
        &row["url"].as_str().expect("a url").to_owned(),
        &targets.url,
    );
    tally.eq(
        id,
        "output_file",
        &row["output_file"].as_str().expect("a path").to_owned(),
        &targets.output_file,
    );
    tally.eq(
        id,
        "css_selector",
        &row["css_selector"].as_str().map(str::to_owned),
        &options.css_selector.clone(),
    );
    tally.eq(
        id,
        "ai_targeted",
        &row["ai_targeted"].as_bool().expect("a bool"),
        &options.ai_targeted,
    );

    // Resolve `--executable-path` / `$BROWSER_RS_EXECUTABLE_PATH` through the
    // pure function with **this row's** environment, then hand the answer to
    // the assembler as if it had been given on the command line. Nothing
    // touches the process environment, so this test is safe to run alongside
    // any other.
    let resolved_executable = browser_cli::args::resolve_executable_path(
        options.executable_path.as_deref(),
        |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.clone())
                .map(std::ffi::OsString::from)
        },
    );
    let mut options = options.clone();
    options.executable_path = resolved_executable.clone();

    match browser_cli::commands::extract::assemble_browser_config(&options, stealth) {
        Ok(config) => compare_browser(
            id,
            &row["kwargs"],
            &config,
            &resolved_executable,
            stealth.is_some(),
            tally,
        ),
        Err(error) => {
            tally.checked += 1;
            tally
                .failures
                .push(format!("{id}: rust failed to assemble the config: {error}"));
        }
    }
}

/// The corpus must keep both exit codes and both command families, or the claim
/// is about one branch only.
#[test]
fn the_corpus_covers_both_exit_codes_and_both_fetcher_families() {
    let rows = corpus();
    assert!(rows.iter().any(|row| row["exit_code"] == 0));
    assert!(rows.iter().any(|row| row["exit_code"] == 2));
    assert!(rows.iter().any(|row| row["kind"] == "http"));
    assert!(rows.iter().any(|row| row["kind"] == "browser"));
    // The one row where Python raises inside the command body rather than in
    // Click's parser.
    assert!(rows
        .iter()
        .any(|row| row["id"] == "post-json-invalid" && row["exit_code"] == 1));
}

/// `--json '{oops}'` is a `ValueError` in Python and exit 1. Here it is an
/// `ExtractError` with the same message shape, which `main` turns into exit 1.
#[test]
fn an_invalid_json_body_is_rejected_with_pythons_message() {
    let row = corpus()
        .into_iter()
        .find(|row| row["id"] == "post-json-invalid")
        .expect("the row exists");
    let cli = Cli::try_parse_from(argv_of(&row)).expect("it parses; the failure is later");
    let Command::Extract {
        command: ExtractCommand::Post(command),
    } = cli.command
    else {
        panic!("expected post")
    };
    let error = build_http_plan(&command.options, Some(&command.body)).expect_err("rejected");
    assert!(
        error
            .to_string()
            .starts_with("Invalid JSON data '{oops}': "),
        "python said {:?}, rust said {error:?}",
        row["exception"]
    );
}

#[test]
fn every_declared_divergence_names_a_real_row() {
    let rows = corpus();
    for (name, reason) in DIVERGENCES {
        assert!(
            rows.iter().any(|row| row["id"].as_str() == Some(name)),
            "declared divergence {name} is not a corpus row"
        );
        assert!(!reason.is_empty(), "{name} has no reason");
    }
}
