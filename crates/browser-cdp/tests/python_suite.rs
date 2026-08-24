//! Direct ports of the reference implementation's own browser-configuration tests.
//!
//! * `tests/fetchers/test_validator.py` → [`validators`]
//! * `tests/fetchers/test_constants.py` → [`constants`]
//!
//! One Rust test per Python test method, same name, same assertions. These are
//! deliberately redundant with `config_golden.rs`: the goldens pin the
//! *observed* behaviour of one pinned release, these pin what upstream
//! considers its contract.

use std::collections::HashSet;

use browser_cdp::config::{BrowserConfig, StealthExtras};
use browser_cdp::route::{effective_blocked_resource_types, RESOURCE_TYPE_TABLE};
use browser_core::types::Proxy;
use browser_fingerprint::domains::{DEFAULT_ARGS, EXTRA_RESOURCES, HARMFUL_ARGS, STEALTH_ARGS};

/// `tests/fetchers/test_validator.py::TestValidators`.
mod validators {
    use super::*;

    #[test]
    fn test_playwright_config_valid() {
        let config = BrowserConfig::new()
            .max_pages(2)
            .headless(true)
            .timeout_ms(30_000.0)
            .proxy(Proxy::Url("http://proxy.example.com:8080".into()))
            .validate()
            .expect("a valid config");

        assert_eq!(config.max_pages, 2);
        assert!(config.headless);
        assert!((config.timeout_ms - 30_000.0).abs() < f64::EPSILON);
        // `isinstance(config.proxy, dict)`: normalised into the three-field form.
        assert_eq!(
            config.proxy,
            Some(Proxy::Config {
                server: "http://proxy.example.com:8080".into(),
                username: Some(String::new()),
                password: Some(String::new()),
            })
        );
    }

    #[test]
    fn test_playwright_config_invalid_max_pages() {
        assert!(BrowserConfig::new().max_pages(0).validate().is_err());
        assert!(BrowserConfig::new().max_pages(51).validate().is_err());
        // Both boundaries are still accepted.
        assert!(BrowserConfig::new().max_pages(1).validate().is_ok());
        assert!(BrowserConfig::new().max_pages(50).validate().is_ok());
    }

    #[test]
    fn test_playwright_config_invalid_timeout() {
        assert!(BrowserConfig::new().timeout_ms(-1.0).validate().is_err());
        assert!(BrowserConfig::new().timeout_ms(0.0).validate().is_ok());
    }

    #[test]
    fn test_playwright_config_invalid_cdp_url() {
        for cdp_url in [
            "invalid-url",
            "ftp://localhost:9222",
            "localhost:9222",
            "http://",
        ] {
            assert!(
                BrowserConfig::new().cdp_url(cdp_url).validate().is_err(),
                "{cdp_url} should have been rejected"
            );
        }
    }

    #[test]
    fn test_playwright_config_cdp_url_schemes() {
        for cdp_url in [
            "ws://localhost:9222",
            "wss://cdp.example.com/session",
            "http://localhost:9222",
            "https://cdp.example.com",
        ] {
            let config = BrowserConfig::new()
                .cdp_url(cdp_url)
                .validate()
                .unwrap_or_else(|e| panic!("{cdp_url} should have been accepted: {e}"));
            assert_eq!(config.cdp_url.as_deref(), Some(cdp_url));
        }
    }

    #[test]
    fn test_stealth_config_valid() {
        let config = BrowserConfig::stealthy()
            .max_pages(1)
            .headless(true)
            .stealth(StealthExtras::default().solve_cloudflare(false))
            .timeout_ms(30_000.0)
            .validate()
            .expect("a valid config");

        assert_eq!(config.max_pages, 1);
        assert!(config.headless);
        assert_eq!(
            config.stealth.map(|s| s.solve_cloudflare),
            Some(false),
            "solve_cloudflare stays off",
        );
        assert!((config.timeout_ms - 30_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_stealth_config_cloudflare_timeout() {
        let config = BrowserConfig::stealthy()
            .stealth(StealthExtras::default().solve_cloudflare(true))
            .timeout_ms(10_000.0)
            .validate()
            .expect("a valid config");

        assert!(
            (config.timeout_ms - 60_000.0).abs() < f64::EPSILON,
            "the Cloudflare floor should have raised it",
        );
    }

    #[test]
    fn test_playwright_config_blocked_domains() {
        let config = BrowserConfig::new()
            .blocked_domains(["ads.example.com", "tracker.io"])
            .validate()
            .expect("a valid config");

        assert_eq!(
            config.blocked_domains,
            Some(HashSet::from([
                "ads.example.com".to_owned(),
                "tracker.io".to_owned(),
            ]))
        );
    }

    #[test]
    fn test_playwright_config_blocked_domains_default_none() {
        let config = BrowserConfig::new().validate().expect("a valid config");
        assert_eq!(config.blocked_domains, None);
    }

    #[test]
    fn test_stealth_config_blocked_domains() {
        let config = BrowserConfig::stealthy()
            .blocked_domains(["ads.example.com"])
            .validate()
            .expect("a valid config");

        assert_eq!(
            config.blocked_domains,
            Some(HashSet::from(["ads.example.com".to_owned()]))
        );
    }
}

/// `tests/fetchers/test_constants.py::TestConstants`.
mod constants {
    use super::*;

    /// Python's `EXTRA_RESOURCES`, verbatim — pinned against the real constant
    /// by `tests/resource_types_golden.rs`.
    fn extra_resources() -> HashSet<&'static str> {
        EXTRA_RESOURCES.iter().copied().collect()
    }

    #[test]
    fn test_default_disabled_resources() {
        let resources = extra_resources();
        assert!(resources.contains("image"));
        assert!(resources.contains("font"));
        assert!(resources.contains("stylesheet"));
        assert!(resources.contains("media"));
    }

    #[test]
    fn test_harmful_default_args() {
        assert!(HARMFUL_ARGS.contains(&"--enable-automation"));
        assert!(HARMFUL_ARGS.contains(&"--disable-popup-blocking"));
    }

    #[test]
    fn test_flags() {
        assert!(DEFAULT_ARGS.contains(&"--no-pings"));
        assert!(STEALTH_ARGS.contains(&"--disable-blink-features=AutomationControlled"));
    }

    /// Not in the Python suite: of the ten `EXTRA_RESOURCES` spellings, only
    /// six can ever be produced by Chromium. Playwright's Chromium backend
    /// maps the CDP `Network.ResourceType` through an explicit `switch`, and
    /// `beacon`, `object`, `imageset` and `csp_report` are not in its range.
    /// See `browser_cdp::route`.
    #[test]
    fn only_six_extra_resources_are_reachable_on_chromium() {
        let reachable: HashSet<&str> = effective_blocked_resource_types().into_iter().collect();
        assert_eq!(
            reachable,
            HashSet::from([
                "font",
                "image",
                "media",
                "texttrack",
                "websocket",
                "stylesheet",
            ])
        );
        for unreachable in ["beacon", "object", "imageset", "csp_report"] {
            assert!(
                extra_resources().contains(unreachable),
                "{unreachable} is listed by Python",
            );
            assert!(
                !RESOURCE_TYPE_TABLE
                    .iter()
                    .any(|(_, playwright)| *playwright == unreachable),
                "{unreachable} is not a spelling Chromium emits",
            );
        }
    }
}

/// `tests/fetchers/test_pages.py::TestPageInfo` and `::TestPagePool`.
///
/// Not requested by the porting brief, but the pool is the other half of
/// unit 13's surface and its `__repr__` is observable in the reference implementation's logs.
mod pages {
    use browser_cdp::error::PoolError;
    use browser_cdp::pool::{PageInfo, PagePool, PageState};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct MockPage(u32);

    #[test]
    fn test_page_info_creation() {
        let info = PageInfo::new(MockPage(1));
        assert_eq!(info.page(), &MockPage(1));
        // Python's `PagePool.add_page` constructs `PageInfo(page, "busy", "")`;
        // the three-argument constructor with an arbitrary state is a test-only
        // affordance there, so the port only exposes the pool's shape.
        assert_eq!(info.state(), PageState::Busy);
        assert_eq!(info.url().as_deref(), Some(""));
    }

    #[test]
    fn test_page_info_marking() {
        let info = PageInfo::new(MockPage(1));

        info.mark_busy("https://example.com");
        assert_eq!(info.state(), PageState::Busy);
        assert_eq!(info.url().as_deref(), Some("https://example.com"));

        info.mark_error();
        assert_eq!(info.state(), PageState::Error);

        info.mark_ready();
        assert_eq!(info.state(), PageState::Ready);
        assert_eq!(info.url().as_deref(), Some(""));
    }

    #[test]
    fn test_page_info_equality() {
        let pool = PagePool::new(3);
        let one = pool.add_page(MockPage(1)).expect("room");
        let same = one.clone();
        let other = pool.add_page(MockPage(2)).expect("room");

        same.mark_ready();
        assert_eq!(one, same, "state does not affect identity");
        assert_ne!(one, other);
    }

    /// `__repr__` outputs captured from the reference interpreter.
    ///
    /// The nested quoting is Python's: `f'Page(URL="{url!r}", state={state!r})'`
    /// wraps `repr(url)` — which brings its own quotes — inside a double-quoted
    /// literal.
    #[test]
    fn test_page_info_repr() {
        let cases: &[(&str, &str)] = &[
            ("", r#"Page(URL="''", state='busy')"#),
            (
                "https://example.com",
                r#"Page(URL="'https://example.com'", state='busy')"#,
            ),
            ("it's", r#"Page(URL=""it's"", state='busy')"#),
            ("a\"b", "Page(URL=\"'a\"b'\", state='busy')"),
            ("a'b\"c", "Page(URL=\"'a\\'b\"c'\", state='busy')"),
            ("a\\b", r#"Page(URL="'a\\b'", state='busy')"#),
            ("a\nb", r#"Page(URL="'a\nb'", state='busy')"#),
        ];
        for (url, want) in cases {
            let info = PageInfo::new(MockPage(1));
            info.mark_busy(url);
            assert_eq!(&info.to_string(), want, "{url:?}");
        }
    }

    #[test]
    fn test_page_pool_creation() {
        let pool: PagePool<MockPage> = PagePool::new(5);
        assert_eq!(pool.max_pages(), 5);
        assert_eq!(pool.pages_count(), 0);
        assert_eq!(pool.busy_count(), 0);
    }

    #[test]
    fn test_add_page() {
        let pool = PagePool::new(2);
        let info = pool.add_page(MockPage(1)).expect("room");
        assert_eq!(info.page(), &MockPage(1));
        assert_eq!(
            info.state(),
            PageState::Busy,
            "a new page is busy for the request that opened it",
        );
        assert_eq!(pool.pages_count(), 1);
        assert_eq!(pool.busy_count(), 1);
    }

    #[test]
    fn test_add_page_limit_exceeded() {
        let pool = PagePool::new(1);
        pool.add_page(MockPage(1)).expect("room");
        assert_eq!(
            pool.add_page(MockPage(2)),
            Err(PoolError::MaxPagesReached { max_pages: 1 })
        );
        assert_eq!(
            PoolError::MaxPagesReached { max_pages: 1 }.to_string(),
            "Maximum page limit (1) reached"
        );
    }

    #[test]
    fn test_proxy_rotation_pool_leak() {
        let pool = PagePool::new(1);
        let info = pool.add_page(MockPage(1)).expect("room");
        assert_eq!(pool.pages_count(), 1);
        pool.remove_page(&info);
        assert_eq!(pool.pages_count(), 0);
        pool.add_page(MockPage(2)).expect("room again");
        assert_eq!(pool.pages_count(), 1);
    }

    #[test]
    fn test_get_ready_page_skips_busy_and_error_pages() {
        let pool = PagePool::new(3);
        let busy = pool.add_page(MockPage(1)).expect("room");
        let errored = pool.add_page(MockPage(2)).expect("room");
        errored.mark_error();
        assert!(pool.get_ready_page().is_none());

        busy.mark_ready();
        let taken = pool.get_ready_page().expect("the freed page");
        assert_eq!(taken, busy);
        assert_eq!(
            taken.state(),
            PageState::Busy,
            "the returned page is reserved for the caller",
        );
        assert!(pool.get_ready_page().is_none());
    }

    #[test]
    fn test_remove_page_ignores_unknown_pages() {
        let pool = PagePool::new(2);
        let info = pool.add_page(MockPage(1)).expect("room");
        assert!(pool.remove_page(&info));
        assert!(!pool.remove_page(&info));
        assert_eq!(pool.pages_count(), 0);
    }

    #[test]
    fn test_clear_returns_every_page_and_empties_the_pool() {
        let pool = PagePool::new(3);
        let pages: Vec<_> = (1..=3)
            .map(|i| pool.add_page(MockPage(i)).expect("room"))
            .collect();
        pages[1].mark_ready();

        assert_eq!(pool.clear(), pages);
        assert_eq!(pool.pages_count(), 0);
        pool.add_page(MockPage(9)).expect("room again");
        assert_eq!(pool.pages_count(), 1);
    }
}
