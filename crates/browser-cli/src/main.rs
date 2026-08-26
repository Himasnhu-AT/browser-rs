//! Entry point for the `browser-rs` binary.
//!
//! Everything worth testing lives in the library next door; this file only
//! decides three things, and each of them mirrors Click:
//!
//! * a **usage** error (missing argument, unknown subcommand, bad `--loglevel`)
//!   exits **2**, printed by `clap` — `browser-rs extract get` with no URL is
//!   exit 2 in Python too;
//! * a **runtime** error exits **1** with `Error: …` on stderr, which is what an
//!   uncaught `ValueError` costs you in Python;
//! * success exits **0**.
//!
//! The whole CLI is `async` underneath because every fetcher in this workspace
//! is; `cli-ai.md` §11.3 suggests exactly this shape — block on a runtime at the
//! `main` boundary and let the command functions stay ordinary `async fn`s.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;

use browser_cli::args::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    // `parse` exits the process itself for `--help`, `--version` and usage
    // errors, so nothing below runs in those cases.
    let cli = Cli::parse();
    browser_cli::init_logging(&cli.loglevel);

    match browser_cli::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // `{error:#}` prints the whole `anyhow` chain on one line, which is
            // the closest thing to Python's traceback that is worth showing a
            // CLI user.
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
