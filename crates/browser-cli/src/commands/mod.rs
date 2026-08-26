//! Subcommand implementations for the `browser-rs` binary.
//!
//! One module per top-level command, plus [`dispatch`], which is the whole
//! `main.add_command(...)` block from `cli.py` reduced to a `match`.

pub mod extract;
#[cfg(feature = "browser")]
pub mod install;
pub mod uncurl;

use crate::args::{Cli, Command, ExtractCommand};

/// Run whatever the parsed command line asked for.
///
/// Everything below this returns a typed error; `anyhow` starts here, at the
/// boundary, per `ARCHITECTURE.md` §1.13.
///
/// # Errors
///
/// Anything a subcommand reports. Argument *syntax* errors never reach this
/// function — `clap` has already exited 2 by then, which is Click's exit code
/// for the same mistake.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        #[cfg(feature = "browser")]
        Command::Install(options) => install::run(options).await,

        Command::Extract { command } => match command {
            ExtractCommand::Get(command) => {
                extract::run_http(
                    extract::HttpVerb::Get,
                    &command.targets,
                    &command.options,
                    None,
                )
                .await
            }
            ExtractCommand::Post(command) => {
                extract::run_http(
                    extract::HttpVerb::Post,
                    &command.targets,
                    &command.options,
                    Some(&command.body),
                )
                .await
            }
            ExtractCommand::Put(command) => {
                extract::run_http(
                    extract::HttpVerb::Put,
                    &command.targets,
                    &command.options,
                    Some(&command.body),
                )
                .await
            }
            ExtractCommand::Delete(command) => {
                extract::run_http(
                    extract::HttpVerb::Delete,
                    &command.targets,
                    &command.options,
                    None,
                )
                .await
            }
            #[cfg(feature = "browser")]
            ExtractCommand::Fetch(command) => {
                extract::run_browser(&command.targets, &command.options, None).await
            }
            #[cfg(feature = "browser")]
            ExtractCommand::StealthyFetch(command) => {
                extract::run_browser(&command.targets, &command.options.clone(), Some(&command))
                    .await
            }
        },

        Command::Uncurl(options) => uncurl::run(options).await,
    }
}
