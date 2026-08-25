//! The unit-14 live corpus: **real Chrome**, driven by this crate, against a
//! **local fixture server**, replaying what **real the reference implementation on real Chrome**
//! did over the same fixture.
//!
//! # Where the numbers come from
//!
//! `tools/corpus/u14/` holds three files:
//!
//! * `fixture.py` — the endpoint table, written out to
//!   `testdata/u14_fixture.json`. [`server`] is its Rust twin: it *interprets*
//!   that JSON rather than re-declaring anything, so the Python and Rust
//!   servers cannot drift.
//! * `server.py` — the Python twin of [`server`].
//! * `generate.py` — drives `the reference implementation` + Playwright + the stock Google
//!   Chrome over that fixture and writes `testdata/u14_browser_golden.json`.
//!
//! The tests here start the Rust server, drive the **same** Chrome binary
//! through [`crate::session::BrowserSession`], and compare against the golden
//! rows. No Python and no network are involved at `cargo test` time.
//!
//! # What "the server received" proves
//!
//! Every resource-blocking assertion is made against
//! [`server::Fixture::received`] — the list of requests that actually arrived
//! at the socket. A request Chrome aborted never gets there, and no amount of
//! CDP bookkeeping on our side can fake it.
//!
//! # Gating
//!
//! Every test is a no-op with a printed notice when no Chrome/Chromium binary
//! can be found or `BROWSER_RS_SKIP_BROWSER_TESTS` is set, so a CI box without a
//! browser stays green. On a machine with Chrome they all really run.

pub mod corpus;
pub mod server;
pub mod tests;

use crate::config::BrowserConfig;
use crate::session::{resolve_executable, BrowserSession, SessionHooks};

use self::server::Fixture;

/// How many live browsers this **crate's** tests run at once.
///
/// One semaphore for the whole test binary, shared with `session::live`:
/// `cargo test` fans out over every core, and a box cold-starting six Chromes
/// at once starts failing to hand back a WebSocket URL inside
/// [`crate::session::LAUNCH_TIMEOUT`].
pub(crate) static LIVE_BROWSERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);

/// Whether a live browser test can run here.
pub fn browser_available() -> bool {
    if std::env::var_os("BROWSER_RS_SKIP_BROWSER_TESTS").is_some() {
        return false;
    }
    resolve_executable(&BrowserConfig::new().validate().expect("a valid default")).is_ok()
}

/// Skip the test — with a printed reason — when there is no browser, and
/// otherwise take a slot in [`LIVE_BROWSERS`].
macro_rules! require_browser {
    () => {
        if !$crate::live_u14::browser_available() {
            eprintln!(
                "skipping {}: no Chromium/Chrome binary on this machine \
                 (or BROWSER_RS_SKIP_BROWSER_TESTS is set)",
                module_path!()
            );
            return;
        }
        let _permit = $crate::live_u14::acquire_browser_slot().await;
    };
}

pub(crate) use require_browser;

/// One slot in the live-browser semaphore, held until the test ends.
pub async fn acquire_browser_slot() -> tokio::sync::SemaphorePermit<'static> {
    LIVE_BROWSERS
        .acquire()
        .await
        .expect("the semaphore is never closed")
}

/// The session config every test starts from.
///
/// `--host-resolver-rules` is what makes a multi-host fixture possible on one
/// loopback port: `main.test`, `cdn.test`, `ads.doubleclick.net` and `api.test`
/// all resolve to 127.0.0.1 without any DNS. It is exactly the flag
/// `generate.py` passed when it produced the corpus.
pub fn live_config(fixture: &Fixture) -> BrowserConfig {
    BrowserConfig::new()
        .headless(true)
        .timeout_ms(20_000.0)
        .extra_flags([fixture.resolver_rule()])
}

/// A started session for `config`.
pub async fn session_for(config: BrowserConfig) -> BrowserSession {
    let session = BrowserSession::new(config).expect("a valid session config");
    session.start().await.expect("the browser starts");
    session
}

/// A started session for `config` with hooks.
pub async fn session_with_hooks(config: BrowserConfig, hooks: SessionHooks) -> BrowserSession {
    let session = BrowserSession::with_hooks(config, hooks).expect("a valid session config");
    session.start().await.expect("the browser starts");
    session
}
