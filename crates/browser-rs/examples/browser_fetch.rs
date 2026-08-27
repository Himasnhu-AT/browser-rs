//! Fetching through a real headless Chromium, driven over raw CDP.
//!
//! # ⚠ This example needs Chrome *and* the public internet
//!
//! It launches a browser and navigates it to a real URL. Nothing in the test
//! suite runs it — `cargo test` compiles examples but never executes them — and
//! it is not wired into any CI job. If no Chrome or Chromium can be found it
//! prints where it looked and exits 0 rather than failing; installing one is
//! `brs install`.
//!
//! ```text
//! cargo run -p browser-rs --example browser_fetch --features browser
//! cargo run -p browser-rs --example browser_fetch --features browser -- https://example.com/
//! ```
//!
//! # What it shows
//!
//! * [`fetch_once`] — launch, fetch, close. The one-shot form, for a single
//!   page.
//! * [`BrowserSession`] — one browser kept alive across several pages, which is
//!   the form that is worth the ~1 s cold start.
//! * That both hand back the **same** [`Response`] the HTTP fetcher produces.
//!   That is the design point of the whole layer: the transport changes, the
//!   thing you query does not, and code written against one works against the
//!   other unchanged.
//! * The knobs that matter in practice — `disable_resources`, `network_idle`,
//!   `wait_selector`, `block_ads`.
//!
//! # Staying logged in
//!
//! [`BrowserConfig::user_data_dir`] is a persistent Chromium profile directory.
//! A session started against the same directory twice is the same browser
//! profile twice, cookies and local storage included — which is how a login
//! survives a process restart. It is commented out below because an example
//! should not silently write a profile into someone's working tree.

use std::time::Instant;

use browser::prelude::*;
use browser::{fetch_once, BrowserFetch};

/// Where to navigate when the caller names nothing.
const DEFAULT_URL: &str = "https://example.com/";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("no URL given, using {DEFAULT_URL} — pass one as the first argument");
        DEFAULT_URL.to_owned()
    });

    // The config is validated separately from being used, so a typo is a clear
    // error at build time rather than a browser that launches and misbehaves.
    let config = BrowserConfig::new()
        .headless(true)
        .timeout_ms(30_000.0)
        // Abort images, fonts, media and stylesheets. On a content-scraping run
        // this is usually the single biggest win available.
        .disable_resources(true)
        // Wait for the network to go quiet, not just for DOMContentLoaded —
        // the point of using a browser at all is content that arrives later.
        .network_idle(true)
        .block_ads(true);
    // .user_data_dir("/path/to/profile")  // ← a login that survives a restart

    let Ok(validated) = config.clone().validate() else {
        eprintln!("the browser config is invalid");
        return Ok(());
    };
    match browser::browser::session::resolve_executable(validated.as_config()) {
        Ok(path) => eprintln!("using {}", path.display()),
        Err(error) => {
            // Not a failure of the example: a machine without Chrome is an
            // ordinary machine.
            eprintln!("no Chrome or Chromium on this machine: {error}");
            eprintln!("install one with `brs install`, then run this again.");
            return Ok(());
        }
    }

    // -- one-shot: launch, fetch, close ------------------------------------

    eprintln!("one-shot fetch of {url}");
    let started = Instant::now();
    let page: Response = fetch_once(config.clone(), &url, FetchParams::default()).await?;
    println!(
        "  {} in {:?} (cold start included)",
        page.status,
        started.elapsed()
    );
    println!(
        "  title: {}",
        page.css("title::text")?.get_or("(none)").trim()
    );

    // -- a session: one browser, several pages -----------------------------

    eprintln!("session fetch (browser kept alive)");
    let session = BrowserSession::new(config)?;
    session.start().await?;

    let started = Instant::now();
    let first = session.fetch(&url, FetchParams::default()).await?;
    println!("  first page:  {} in {:?}", first.status, started.elapsed());

    // The second page pays no launch cost. `wait_selector` is per request, not
    // per session, so a page that needs one can ask for it on its own.
    //
    // Every field is an `Override`, not an `Option`: `Unset` means "inherit the
    // session's value", and `Set(None)` means "the caller explicitly asked for
    // no selector". Python's `**kwargs` make that distinction and so does this.
    let started = Instant::now();
    let detailed: BrowserFetch = session
        .fetch_detailed(
            &url,
            FetchParams {
                wait_selector: Override::Set(Some("body".to_owned())),
                ..FetchParams::default()
            },
        )
        .await?;
    println!(
        "  second page: {} in {:?} (no launch)",
        detailed.response.status,
        started.elapsed()
    );

    // `fetch_detailed` is the same fetch with the browser-side extras kept:
    // whatever `capture_xhr` matched, and the final URL after client-side
    // navigation.
    println!("  final url: {}", detailed.response.url);
    println!("  captured xhr responses: {}", detailed.captured_xhr.len());

    // The response is an ordinary `Response`, so the whole document API is
    // here — including the ones that have nothing to do with a browser.
    let links = first.css("a")?.attrs("href");
    println!("  links: {}", links.len());
    for href in links.iter().take(5) {
        println!("    - {}", first.urljoin(href));
    }

    // Closing shuts the browser down. Skipping it leaks a Chromium process.
    session.close().await;
    println!("browser closed");

    Ok(())
}
