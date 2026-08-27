//! The `brs-serve` binary.
//!
//! One process, one port, three surfaces over one engine. This file does four
//! things and nothing else: read the configuration, warn if the server is
//! unauthenticated, build the app, and serve it until `Ctrl-C`.
//!
//! ```text
//! BROWSER_RS_API_TOKEN=… BROWSER_RS_BIND=0.0.0.0:8080 brs-serve
//! ```
//!
//! With no `BROWSER_RS_API_TOKEN` the listener is pinned to loopback and a
//! warning is printed. That is not a soft default: this process drives a real
//! browser, holds the operator's logged-in cookies and fetches whatever it is
//! told to, so an unauthenticated one reachable from another host would be an
//! open proxy with someone's sessions in it.
//!
//! Which surfaces exist is [`api::surfaces`]'s business, not this file's — that
//! is what lets the integration tests drive the same router the binary serves.
//! Anything it does not mount answers `404`.

use std::process::ExitCode;
use std::sync::Arc;

use browser_server::api;
use browser_server::config::ServerConfig;
use browser_server::engine::Engine;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();

    let config = match ServerConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "the environment is not a usable configuration");
            return ExitCode::FAILURE;
        }
    };

    let (bind, rewritten) = config.effective_bind();
    if !config.is_authenticated() {
        tracing::warn!(
            "BROWSER_RS_API_TOKEN is not set: every request is accepted, so the \
             listener is pinned to loopback. Set a token before binding a public \
             address — this server drives a real browser and fetches arbitrary URLs."
        );
    }
    if rewritten {
        tracing::warn!(
            requested = %config.bind,
            actual = %bind,
            "refusing to bind a public address without a token"
        );
    }

    let ttl = config.session_idle_ttl;
    let profile_root = config.profile_root.clone();
    let engine = Arc::new(Engine::new(config));

    // The sweeper is the only reason an *idle* server ever closes a browser:
    // the lazy sweep inside the registry only runs when a request arrives.
    if !ttl.is_zero() {
        let sessions = Arc::clone(engine.sessions());
        tokio::spawn(sessions.run_sweeper(ttl / 4));
    }

    // `API.md` §3a's composition lives in `api::surfaces`, not here, so the
    // integration tests drive the same router this binary serves.
    let app = api::app(Arc::clone(&engine));

    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%bind, %error, "could not bind");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        %bind,
        profiles = %profile_root.display(),
        "brs-serve listening; REST at /api, description at /api/openapi.json"
    );

    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await;

    // Sessions are closed after the listener stops, so a Chrome process never
    // outlives the server that launched it.
    engine.sessions().close_all().await;

    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the server stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Logs to stderr, `RUST_LOG` honoured, defaulting to this crate at `info`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("browser_server=info,browser=warn,tower_http=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn shutdown() {
    if tokio::signal::ctrl_c().await.is_err() {
        // No signal handler available — the future simply never completes, and
        // the server runs until the process is killed.
        std::future::pending::<()>().await;
    }
    tracing::info!("shutting down");
}
