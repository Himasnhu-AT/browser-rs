//! The `brs-mcp` binary: MCP over stdio.
//!
//! This is the process a desktop agent runtime spawns. It speaks
//! newline-delimited JSON-RPC on stdin and stdout and serves exactly the tools
//! `browser-server` serves at `/mcp` — same catalogue, same engine, same
//! session directories, so a session logged in through the dashboard is
//! available here too.
//!
//! # stdout belongs to the protocol
//!
//! Anything written to stdout that is not a JSON-RPC frame desynchronises the
//! client, usually as a mysterious "failed to parse message" and a dead server.
//! So logging goes to **stderr**, and nothing in this binary prints. Set
//! `RUST_LOG=browser_mcp=debug` (or `browser_server=debug`) to see it; the agent
//! runtime collects stderr into its own log.
//!
//! # Configuration
//!
//! The same environment variables `brs-serve` reads — see
//! [`browser_server::config::ServerConfig::from_env`]. `BROWSER_RS_API_TOKEN`
//! is **not** among the ones that matter here: stdio has no network surface to
//! guard, the transport is a pipe to the process that spawned us, and access
//! control is whoever can run the binary. It is the HTTP mount that needs a
//! bearer token.

use std::process::ExitCode;
use std::sync::Arc;

use browser_mcp::error::McpServerError;
use browser_mcp::server::BrowserMcpServer;
use browser_server::config::ServerConfig;
use browser_server::engine::Engine;
use browser_server::mcp::EngineTools;

/// How often idle sessions are swept.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

fn main() -> ExitCode {
    // The subscriber must be installed before anything can log, and it must
    // write to stderr — see the module docs.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("browser_mcp=info")),
        )
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "could not start the async runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "brs-mcp stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), McpServerError> {
    let config = ServerConfig::from_env()?;
    tracing::info!(
        profile_root = %config.profile_root.display(),
        max_live_browsers = config.max_live_browsers,
        "brs-mcp starting"
    );

    let engine = Arc::new(Engine::new(config));

    // Every session is a real Chrome profile and, when live, a real Chrome
    // process. Without the sweeper a long-lived stdio server accumulates both.
    let sessions = Arc::clone(engine.sessions());
    let sweeper = tokio::spawn(Arc::clone(&sessions).run_sweeper(SWEEP_INTERVAL));

    let server = BrowserMcpServer::new(EngineTools::shared(Arc::clone(&engine)));
    let running = server.serve_stdio().await?;
    tracing::info!(
        "client connected; serving {} tools",
        browser_server::mcp::TOOL_NAMES.len()
    );

    // Resolves when the client closes the pipe, which is how an agent runtime
    // shuts a stdio server down: it does not send a message, it goes away.
    let reason = running
        .waiting()
        .await
        .map_err(|error| McpServerError::transport(format!("service task failed: {error}")))?;
    tracing::info!(?reason, "client disconnected");

    sweeper.abort();
    // Leave no orphaned Chrome behind; the profile directories stay, which is
    // what keeps the logins.
    sessions.close_all().await;
    Ok(())
}
