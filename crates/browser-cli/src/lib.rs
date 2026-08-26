//! The `browser-rs` command-line interface, as a library so the argument tree
//! can be unit-tested without spawning a process.
//!
//! `anyhow` lives at this boundary only; everything below returns typed errors.
//! See `ARCHITECTURE.md` §1.13.
//!
//! # The command tree
//!
//! ```text
//! brs [-L LEVEL] [--version]
//! ├── install       [-f/--force] [--path DIR]        (feature = "browser")
//! ├── extract
//! │   ├── get             URL OUTPUT_FILE  <http options>
//! │   ├── post            URL OUTPUT_FILE  <data options> <http options>
//! │   ├── put             URL OUTPUT_FILE  <data options> <http options>
//! │   ├── delete          URL OUTPUT_FILE  <http options>
//! │   ├── fetch           URL OUTPUT_FILE  <browser options>       (feature = "browser")
//! │   └── stealthy-fetch  URL OUTPUT_FILE  <stealth> <browser>     (feature = "browser")
//! └── uncurl        [-o OUTPUT_FILE] [-s SELECTOR] [--ai-targeted] CURL_COMMAND
//! ```
//!
//! Python's `shell` and `mcp` commands are out of scope (`ARCHITECTURE.md` §8);
//! `uncurl` carries over the two shell helpers that do not need a REPL.
//!
//! # Using it as a library
//!
//! Every piece is public and side-effect-free until you call [`run`]:
//!
//! ```
//! use std::time::Duration;
//!
//! use clap::Parser;
//! use browser_cli::args::{Cli, Command, ExtractCommand};
//! use browser_cli::commands::extract::build_http_plan;
//! use browser_cli::convert::extraction_for;
//!
//! let cli = Cli::try_parse_from([
//!     "brs", "extract", "get", "--timeout", "5", "https://example.test/", "out.md",
//! ])
//! .expect("a valid command line");
//!
//! let Command::Extract { command: ExtractCommand::Get(command) } = cli.command else {
//!     unreachable!()
//! };
//! let plan = build_http_plan(&command.options, None).expect("a valid option group");
//!
//! // `--timeout 5` on an HTTP command is five *seconds*.
//! assert_eq!(plan.timeout, Duration::from_secs(5));
//! // …and the output suffix, not a flag, picks the format.
//! assert_eq!(
//!     extraction_for(&command.targets.output_file).expect("a known suffix"),
//!     browser::ExtractionType::Markdown,
//! );
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod args;
pub mod commands;
pub mod convert;

use tracing_subscriber::EnvFilter;

/// Translate Click's six log-level names into `tracing`'s five.
///
/// `critical` and `fatal` are both 50 in Python's `logging`, and `tracing` has
/// nothing above `error`, so both land on `error`. That is the one place the
/// levels are not one-to-one, and it only makes the CLI *more* talkative than
/// Python at those two settings — never less.
///
/// ```
/// use browser_cli::tracing_level;
///
/// assert_eq!(tracing_level("debug"), "debug");
/// assert_eq!(tracing_level("warning"), "warn");
/// assert_eq!(tracing_level("critical"), "error");
/// // Unknown values fall back to Python's default, as `CustomShell` does.
/// assert_eq!(tracing_level("nonsense"), "debug");
/// ```
#[must_use]
pub fn tracing_level(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "info" => "info",
        "warning" => "warn",
        "error" | "critical" | "fatal" => "error",
        // Python's `CustomShell` warns and falls back to DEBUG for anything it
        // does not recognise. `clap` already rejects unknown values, so this
        // arm is only reachable through the library API.
        _ => "debug",
    }
}

/// Install the `tracing` subscriber the CLI logs through.
///
/// `RUST_LOG`, when set, wins over `--loglevel`: it is the escape hatch for
/// turning on one crate's spans without turning on everything. Calling this
/// twice is harmless — the second call is ignored rather than panicking, which
/// matters because tests share a process.
///
/// Logs go to **stderr**, so `brs uncurl … | jq` still works.
pub fn init_logging(level: &str) {
    let filter = std::env::var("RUST_LOG").map_or_else(
        |_| EnvFilter::new(format!("browser_cli={0},browser={0}", tracing_level(level))),
        EnvFilter::new,
    );
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}

/// Run a parsed command line.
///
/// # Errors
///
/// Whatever the subcommand reports. The binary turns any error into exit code
/// 1, which is what an uncaught `ValueError` gives you in Python; `clap` has
/// already handled the exit-2 usage errors before this is called.
pub async fn run(cli: args::Cli) -> anyhow::Result<()> {
    commands::dispatch(cli).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_click_level_name_maps_to_a_tracing_level() {
        assert_eq!(tracing_level("debug"), "debug");
        assert_eq!(tracing_level("info"), "info");
        assert_eq!(tracing_level("warning"), "warn");
        assert_eq!(tracing_level("error"), "error");
        assert_eq!(tracing_level("critical"), "error");
        assert_eq!(tracing_level("fatal"), "error");
    }

    #[test]
    fn level_names_are_case_and_whitespace_insensitive_like_pythons() {
        assert_eq!(tracing_level(" DEBUG "), "debug");
        assert_eq!(tracing_level("Warning"), "warn");
    }

    /// `_known_logging_levels` is strictly ascending for debug→critical, and so
    /// is the mapping: no two adjacent Python levels collapse except the pair
    /// Python itself already collapses (`critical` and `fatal` are both 50).
    #[test]
    fn the_level_order_is_preserved() {
        let order = ["debug", "info", "warning", "error"];
        let mapped: Vec<&str> = order.into_iter().map(tracing_level).collect();
        assert_eq!(mapped, ["debug", "info", "warn", "error"]);
        assert_eq!(tracing_level("critical"), tracing_level("fatal"));
    }

    #[test]
    fn installing_the_subscriber_twice_does_not_panic() {
        init_logging("error");
        init_logging("debug");
    }
}
