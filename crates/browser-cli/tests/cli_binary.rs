//! The real `browser-rs` binary, run as a subprocess.
//!
//! Everything else in this crate tests the library; this file tests the two
//! things only a process can show — **exit codes** and **what lands on stdout /
//! stderr** — against what the reference `browser-rs` console script does. Those
//! were established by running
//! `/…/pyref/bin/browser-rs --help`, `--version`, `extract get` and `nosuch` and
//! reading the codes off, not by reading Click's source.
//!
//! | Situation | Click | here |
//! |---|---|---|
//! | `--help`, `--version` | 0 | 0 |
//! | missing argument, unknown subcommand | 2 | 2 |
//! | a `ValueError` escaping a command | 1 (traceback) | 1 (`Error: …`) |
//!
//! The fixture server runs in this process and the binary talks to it over
//! loopback, so no network and no Python are involved.

mod support;

use std::process::{Command, Output};

use support::{Page, Site};

/// The binary Cargo just built for this test.
const BIN: &str = env!("CARGO_BIN_EXE_browser-rs");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        // Keep the subprocess quiet and deterministic: an inherited `RUST_LOG`
        // would change what lands on stderr.
        .env_remove("RUST_LOG")
        .env_remove("BROWSER_RS_EXECUTABLE_PATH")
        .output()
        .expect("the binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn site_pages(path: &str) -> Option<Page> {
    match path {
        "/doc" => Some(Page::html(
            "<html><head><title>Doc</title></head><body><h1>Title</h1>\
             <p>Body</p></body></html>",
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Exit codes and the version banner
// ---------------------------------------------------------------------------

#[test]
fn help_exits_zero_and_lists_the_three_commands() {
    let output = run(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(help.contains("extract"), "{help}");
    assert!(help.contains("uncurl"), "{help}");
    #[cfg(feature = "browser")]
    assert!(help.contains("install"), "{help}");
}

/// Click's `version_option(prog_name="the reference implementation")` prints
/// `the reference implementation, version 0.4.15`. The number here is this crate's, but the shape
/// is the same so a script that greps for it keeps working.
#[test]
fn version_uses_clicks_prog_name_form() {
    let output = run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout(&output).starts_with("the reference implementation, version "),
        "{:?}",
        stdout(&output)
    );
}

#[test]
fn a_missing_argument_exits_two_like_click() {
    let output = run(&["extract", "get"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("URL"), "{}", stderr(&output));
}

#[test]
fn an_unknown_subcommand_exits_two_like_click() {
    let output = run(&["nosuch"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn an_unknown_extract_subcommand_exits_two() {
    let output = run(&["extract", "head", "https://x.test/", "o.md"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn extract_get_help_carries_the_output_file_note() {
    let output = run(&["extract", "get", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(
        help.contains("`.html`/`.md`/`.txt`"),
        "__OUTPUT_FILE_HELP__ should be there: {help}"
    );
    assert!(help.contains("Request timeout in seconds"), "{help}");
}

#[cfg(feature = "browser")]
#[test]
fn extract_fetch_help_says_milliseconds() {
    let output = run(&["extract", "fetch", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(
        help.contains("Timeout in milliseconds"),
        "the browser group's unit must be visible in the help: {help}"
    );
    assert!(help.contains("BROWSER_RS_EXECUTABLE_PATH"), "{help}");
}

// ---------------------------------------------------------------------------
// A real fetch, end to end through the process boundary
// ---------------------------------------------------------------------------

#[test]
fn extract_get_writes_the_file_and_exits_zero() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("page.txt");

    let output = run(&[
        "extract",
        "get",
        &site.url("/doc"),
        path.to_str().expect("utf-8"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "Doc\nTitle\nBody"
    );
}

#[test]
fn extract_get_writes_markdown_when_the_suffix_says_so() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("page.md");

    let output = run(&[
        "extract",
        "get",
        &site.url("/doc"),
        path.to_str().expect("utf-8"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let markdown = std::fs::read_to_string(&path).expect("read");
    assert!(
        markdown.contains("Title\n====="),
        "markdownify's setext headings: {markdown:?}"
    );
}

/// A `ValueError` in Python is exit 1 with a traceback; here it is exit 1 with
/// one `Error: …` line.
#[test]
fn a_bad_output_suffix_exits_one_with_pythons_message() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("page.xyz");

    let output = run(&[
        "extract",
        "get",
        &site.url("/doc"),
        path.to_str().expect("utf-8"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output)
            .contains("Unknown file type: filename must end with '.md', '.html', or '.txt'"),
        "{}",
        stderr(&output)
    );
    assert!(!path.exists());
}

#[test]
fn a_header_without_a_colon_exits_one() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let output = run(&[
        "extract",
        "get",
        "-H",
        "NoColonHere",
        &site.url("/doc"),
        out.path().join("page.txt").to_str().expect("utf-8"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Could not parse header without colon"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unreachable_host_exits_one_rather_than_panicking() {
    let out = tempfile::tempdir().expect("tempdir");
    // Port 1 on loopback: nothing listens, and nothing leaves the machine.
    let output = run(&[
        "extract",
        "get",
        "http://127.0.0.1:1/",
        out.path().join("page.txt").to_str().expect("utf-8"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    // The retry ladder logs two warnings and an error first — the `Error: …`
    // line `main` prints is the last one, not the first.
    assert!(
        stderr(&output).contains("Error: transport error"),
        "{}",
        stderr(&output)
    );
}

// ---------------------------------------------------------------------------
// `uncurl`
// ---------------------------------------------------------------------------

#[test]
fn uncurl_prints_the_parsed_request_as_json_on_stdout() {
    let output = run(&[
        "uncurl",
        "curl https://example.test/ -H 'A: 1' -X POST -d 'a=1'",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let parsed: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("stdout is JSON");
    assert_eq!(parsed["method"], "post");
    assert_eq!(parsed["url"], "https://example.test/");
    assert_eq!(parsed["headers"]["A"], "1");
    assert_eq!(parsed["data"], "a=1");
    assert_eq!(parsed["follow_redirects"], "safe");
}

/// `cli-ai.md` §5.3's pinned example: this is an `AttributeError` in Python and
/// exit 1 here, with the same message.
#[test]
fn uncurl_reports_unknown_arguments_and_exits_one() {
    let output = run(&["uncurl", "not a curl command"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Unknown/Unsupported curl arguments: ['curl', 'command']"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn uncurl_with_an_output_file_actually_runs_the_request() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("page.txt");

    let output = run(&[
        "uncurl",
        "-o",
        path.to_str().expect("utf-8"),
        &format!("curl {} -H 'X-From: uncurl'", site.url("/doc")),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(std::fs::read_to_string(&path)
        .expect("read")
        .contains("Title"));
    assert_eq!(site.last().header("x-from"), Some("uncurl"));
}

#[test]
fn uncurl_refuses_an_unsupported_method_before_it_fetches() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let output = run(&[
        "uncurl",
        "-o",
        out.path().join("page.txt").to_str().expect("utf-8"),
        &format!("curl {} -X PATCH", site.url("/doc")),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("isn't supported by the reference implementation yet"),
        "{}",
        stderr(&output)
    );
    assert!(site.paths().is_empty(), "no request was made");
}

// ---------------------------------------------------------------------------
// `install`
// ---------------------------------------------------------------------------

/// The download itself is never run — it needs the network. What is checked is
/// that the command exists, that `--force` and `--path` are accepted, and that a
/// pre-existing marker short-circuits it **without touching the network**, which
/// is the only branch of `install` a hermetic test can reach.
#[cfg(feature = "browser")]
#[test]
fn install_short_circuits_on_an_existing_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path()
            .join(browser_cli::commands::install::MARKER_FILE),
        b"",
    )
    .expect("marker written");

    let output = run(&["install", "--path", dir.path().to_str().expect("utf-8")]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        browser_cli::commands::install::ALREADY_INSTALLED
    );
}

#[cfg(feature = "browser")]
#[test]
fn install_help_documents_force_and_path() {
    let output = run(&["install", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(help.contains("--force"), "{help}");
    assert!(help.contains("--path"), "{help}");
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// The "Content successfully saved to …" line is `log.info` in Python, so it
/// only appears at `info` or below — and it goes to **stderr**, so piping
/// `uncurl`'s JSON still works.
#[test]
fn the_saved_notice_respects_the_log_level_and_goes_to_stderr() {
    let site = Site::start(site_pages);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("page.txt");

    let noisy = run(&[
        "extract",
        "get",
        &site.url("/doc"),
        path.to_str().expect("utf-8"),
    ]);
    assert!(
        stderr(&noisy).contains("Content successfully saved to"),
        "{}",
        stderr(&noisy)
    );
    assert!(
        stdout(&noisy).is_empty(),
        "stdout stays clean: {:?}",
        stdout(&noisy)
    );

    let quiet = run(&[
        "-L",
        "error",
        "extract",
        "get",
        &site.url("/doc"),
        path.to_str().expect("utf-8"),
    ]);
    assert!(
        !stderr(&quiet).contains("Content successfully saved to"),
        "{}",
        stderr(&quiet)
    );
}

#[test]
fn an_unknown_log_level_is_a_usage_error() {
    let output = run(&["-L", "trace", "uncurl", "curl https://x.test/"]);
    assert_eq!(output.status.code(), Some(2));
}
