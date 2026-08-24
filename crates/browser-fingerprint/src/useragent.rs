//! Lazily computed [`DEFAULT_UA`], [`DEFAULT_BROWSER_UA`] and
//! [`DEFAULT_CHROME_UA`].
//!
//! Python computes all three **at import time** as module-level side effects:
//!
//! ```text
//! fingerprints.py            __default_useragent__        = generate_headers(browser_mode=False)["User-Agent"]
//! _browsers/_config_tools.py __default_useragent__        = generate_headers(browser_mode=True)["User-Agent"]
//! _browsers/_config_tools.py __default_chrome_useragent__ = generate_headers(browser_mode="chrome")["User-Agent"]
//! ```
//!
//! `ARCHITECTURE.md` §1.8 makes them `LazyLock` instead: a CLI that never
//! fetches should not pay for header generation. Each is sampled exactly once
//! and then stable for the process lifetime, which is the property Python's
//! import-time computation actually guarantees.

use std::sync::LazyLock;

use crate::headers::{generate_headers, BrowserMode};

/// The UA used if header generation somehow yields a profile without one.
///
/// Python's `__default_useragent__` is `Optional[str]` and callers cope with
/// `None`; a `&'static str` is friendlier and this can only be reached if the
/// vendored profile table is broken, which `build.rs` already rules out.
pub const FALLBACK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                               AppleWebKit/537.36 (KHTML, like Gecko) \
                               Chrome/150.0.0.0 Safari/537.36";

/// The static/HTTP-engine default `User-Agent`
/// (`fingerprints.py::__default_useragent__`).
///
/// Sampled from Chrome/Firefox/Edge across all three desktop platforms, so it
/// does not leak the real host OS. Used by the HTTP engine as the fallback UA
/// when `stealthy_headers` is off **and** `impersonate` is off.
pub static DEFAULT_UA: LazyLock<String> = LazyLock::new(|| ua_for(BrowserMode::Http));

/// The browser-engine default `User-Agent`
/// (`_config_tools.py::__default_useragent__`).
///
/// Chrome only, host OS only, with the `Chrome/…` token rewritten to the
/// version the automation driver actually launches.
pub static DEFAULT_BROWSER_UA: LazyLock<String> = LazyLock::new(|| ua_for(BrowserMode::Browser));

/// The "real Chrome" default `User-Agent`
/// (`_config_tools.py::__default_chrome_useragent__`).
///
/// Generated exactly like [`DEFAULT_BROWSER_UA`] — Python's `"chrome"` string
/// is merely truthy — but sampled independently, so the two need not be equal.
pub static DEFAULT_CHROME_UA: LazyLock<String> = LazyLock::new(|| ua_for(BrowserMode::Chrome));

fn ua_for(mode: BrowserMode) -> String {
    let ua = generate_headers(mode)
        .user_agent()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| FALLBACK_UA.to_owned());
    tracing::debug!(?mode, user_agent = %ua, "computed default user agent");
    ua
}

/// [`DEFAULT_UA`] as a borrowed string.
#[must_use]
pub fn default_ua() -> &'static str {
    &DEFAULT_UA
}

/// [`DEFAULT_BROWSER_UA`] as a borrowed string.
#[must_use]
pub fn default_browser_ua() -> &'static str {
    &DEFAULT_BROWSER_UA
}

/// [`DEFAULT_CHROME_UA`] as a borrowed string.
#[must_use]
pub fn default_chrome_ua() -> &'static str {
    &DEFAULT_CHROME_UA
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_is_a_plausible_user_agent() {
        for ua in [default_ua(), default_browser_ua(), default_chrome_ua()] {
            assert!(ua.starts_with("Mozilla/5.0 "), "{ua}");
            assert!(ua.len() > 40, "{ua}");
            assert!(!ua.contains("*MISSING_VALUE*"), "{ua}");
            assert!(!ua.contains('\n'), "{ua}");
        }
    }

    #[test]
    fn browser_defaults_are_chrome() {
        for ua in [default_browser_ua(), default_chrome_ua()] {
            assert!(ua.contains("Chrome/"), "{ua}");
            assert!(!ua.contains("Firefox/"), "{ua}");
            assert!(!ua.contains("Edg/"), "{ua}");
        }
    }

    #[test]
    fn the_http_default_may_be_any_of_the_three_http_families() {
        let ua = default_ua();
        assert!(ua.contains("Chrome/") || ua.contains("Firefox/"), "{ua}");
    }

    #[test]
    fn defaults_are_stable_for_the_process_lifetime() {
        assert_eq!(default_ua(), default_ua());
        assert_eq!(default_ua(), DEFAULT_UA.as_str());
        assert_eq!(default_browser_ua(), DEFAULT_BROWSER_UA.as_str());
        assert_eq!(default_chrome_ua(), DEFAULT_CHROME_UA.as_str());
    }

    #[test]
    fn the_fallback_is_itself_plausible() {
        assert!(FALLBACK_UA.starts_with("Mozilla/5.0 "));
        assert!(FALLBACK_UA.contains("Chrome/"));
        assert!(!FALLBACK_UA.contains("  "), "{FALLBACK_UA}");
    }
}
