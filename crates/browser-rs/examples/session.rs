//! A session: one cookie jar, one connection pool, many requests.
//!
//! # ⚠ This example talks to the public internet
//!
//! It makes several real HTTPS requests. Nothing in the test suite runs it —
//! `cargo test` compiles examples but never executes them — and it is not wired
//! into any CI job. Run it yourself:
//!
//! ```text
//! cargo run -p browser-rs --example session
//! cargo run -p browser-rs --example session -- https://example.com/
//! ```
//!
//! # What it shows
//!
//! * [`AsyncFetcherSession`] built once and reused. This is the difference
//!   between a script and a scraper: the TLS handshake, the connection and the
//!   impersonation profile are paid for once instead of per request.
//! * The cookie jar surviving across requests, which is what "staying logged
//!   in" means at the HTTP layer.
//! * Session-level defaults (`FetcherConfig`) versus per-request overrides —
//!   the override wins, and *only* for that request.
//! * Closing the session, which returns the connections rather than leaving
//!   them to a drop.
//!
//! For a login that has to survive a process restart, the browser side of the
//! library is the answer instead: a persistent Chromium profile directory. See
//! `browser_fetch.rs` and `BrowserConfig::user_data_dir`.

use std::time::{Duration, Instant};

use browser::prelude::*;
use browser::{FetcherConfig, Impersonate, SessionBuilder};

/// Where to fetch from when the caller names nothing.
const DEFAULT_URL: &str = "https://example.com/";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("no URL given, using {DEFAULT_URL} — pass one as the first argument");
        DEFAULT_URL.to_owned()
    });

    // Session-level defaults. Every request inherits these unless it overrides
    // them; a request that overrides one does not disturb the session.
    let session: AsyncFetcherSession = SessionBuilder::new()
        .config(
            FetcherConfig::new()
                .with_impersonate(Impersonate::single("chrome"))
                .with_timeout(Some(Duration::from_secs(30)))
                .with_retries(Some(2))
                .with_retry_delay(Duration::from_secs(1))
                .with_header("Accept-Language", "en-GB,en;q=0.9"),
        )
        .build()?;

    // `open` marks the session active — it is Python's `async with`, and it is
    // what `close` below is paired with. It does *not* dial anything: the DNS
    // lookup, the TCP connect and the TLS handshake all happen on the first
    // request, which is exactly why the first timing below is the slow one.
    session.open().await?;

    println!("first request (handshake included)");
    let started = Instant::now();
    let first = session.get(&url).send().await?;
    let cold = started.elapsed();
    println!("  {} in {cold:?}", first.status);
    println!(
        "  title: {}",
        first.css("title::text")?.get_or("(none)").trim()
    );

    // The same session again. The handshake is already done, so what is left is
    // the round trip and the parse.
    println!("second request (same connection)");
    let started = Instant::now();
    let second = session.get(&url).send().await?;
    let warm = started.elapsed();
    println!("  {} in {warm:?}", second.status);

    if warm < cold {
        println!("  reuse saved {:?}", cold - warm);
    } else {
        // Say so rather than quietly implying a win that did not happen: on a
        // fast local network the handshake can be lost in the noise.
        println!("  no measurable saving on this network — one sample proves little");
    }

    // A per-request override. `timeout` here replaces the session's 30 s for
    // this request only.
    println!("third request (per-request override)");
    let third = session
        .get(&url)
        .timeout(Some(Duration::from_secs(5)))
        .header("X-Example", "per-request")
        .send()
        .await?;
    println!("  {} — session timeout unchanged", third.status);

    // The jar is the session's, not the response's: whatever any reply set is
    // visible here and is sent back on the next request to a matching host.
    let jar = session.cookies();
    println!("cookie jar: {} cookie(s)", jar.len());

    // Anything the document API can do to a `Document`, it can do to a reply.
    let links = second.css("a")?.attrs("href");
    println!("links on the page: {}", links.len());
    for href in links.iter().take(5) {
        println!("  - {}", second.urljoin(href));
    }

    // Not optional housekeeping: this returns the pooled connections instead of
    // leaving them for a `Drop` that cannot await.
    session.close().await?;
    println!("session closed (alive: {})", session.is_alive());

    Ok(())
}
