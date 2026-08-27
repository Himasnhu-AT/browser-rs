//! One request, one parsed page — the smallest useful `browser-rs` program.
//!
//! # ⚠ This example talks to the public internet
//!
//! It makes a real HTTPS request. Nothing in the test suite runs it — `cargo
//! test` compiles examples but never executes them — and it is not wired into
//! any CI job. Run it yourself:
//!
//! ```text
//! cargo run -p browser-rs --example one_shot_fetch --features markdown
//! cargo run -p browser-rs --example one_shot_fetch --features markdown -- https://news.ycombinator.com/
//! ```
//!
//! The default target is `https://example.com/`, IANA's reserved documentation
//! domain, which exists precisely so that examples like this one have somewhere
//! harmless to point.
//!
//! # What it shows
//!
//! * [`AsyncFetcher::get`] — the one-shot form, no session to build or close.
//! * The per-request builder, and why it is a builder: Python's `**kwargs`
//!   distinguish "the caller passed `timeout=None`" from "the caller said
//!   nothing", and calling `.timeout(None)` *at all* is the presence signal.
//! * Impersonation as a **pool**, sampled per request.
//! * The document API landing directly on the reply — `Response` is the parsed
//!   page, not a byte bag you have to feed somewhere else.

use std::time::Duration;

use browser::prelude::*;
use browser::{Impersonate, RedirectMode};

/// Where to fetch from when the caller names nothing.
const DEFAULT_URL: &str = "https://example.com/";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("no URL given, using {DEFAULT_URL} — pass one as the first argument");
        DEFAULT_URL.to_owned()
    });

    eprintln!("GET {url}");
    let page: Response = AsyncFetcher::get(&url)
        // A pool, not a single profile: one element is drawn per request, so a
        // burst of requests does not present one identical TLS fingerprint.
        .impersonate(Impersonate::pool(["chrome", "firefox"]))
        // `Some(..)` is a timeout; `None` here would be an explicit "no
        // timeout", which is not the same as leaving the method uncalled.
        .timeout(Some(Duration::from_secs(30)))
        // Retry the whole request, with a fixed pause and no backoff — the
        // reference implementation does not jitter either.
        .retries(Some(3))
        .retry_delay(Duration::from_secs(1))
        .header("Accept-Language", "en-GB,en;q=0.9")
        // `Safe` downgrades HTTPS→HTTP redirects into a stop rather than
        // following them, which is what you want for anything unattended.
        .follow_redirects(FollowRedirects::Mode(RedirectMode::Safe))
        .send()
        .await?;

    println!("{} {} ({})", page.status, page.reason, page.encoding);
    if !page.history.is_empty() {
        println!("redirects: {}", page.history.len());
    }
    if let Some(content_type) = page.header("content-type") {
        println!("content-type: {content_type}");
    }
    println!("bytes: {}", page.body().len());

    // The reply *is* the document. Nothing was parsed until this line — the
    // tree is built lazily on first query and then cached.
    let title = page.css("title::text")?.get();
    println!("title: {}", title.as_deref().unwrap_or("(none)"));

    let mut headings = page.css("h1::text")?.texts();
    headings.extend(page.css("h2::text")?.texts());
    if headings.is_empty() {
        println!("headings: (none)");
    } else {
        println!("headings:");
        for heading in headings.iter().take(10) {
            println!("  - {}", heading.trim());
        }
    }

    // `urljoin` resolves against the *final* URL, after redirects.
    let links = page.css("a")?.attrs("href");
    println!("links: {}", links.len());
    for href in links.iter().take(5) {
        println!("  - {}", page.urljoin(href));
    }

    // `markdown` is a feature; without it this block is simply absent, which is
    // the whole point of gating it.
    #[cfg(feature = "markdown")]
    {
        let markdown = page.markdown();
        println!("\n--- markdown ({} chars) ---", markdown.len());
        for line in markdown.lines().take(20) {
            println!("{line}");
        }
    }
    #[cfg(not(feature = "markdown"))]
    println!("\n(build with --features markdown to see the page as Markdown)");

    Ok(())
}
