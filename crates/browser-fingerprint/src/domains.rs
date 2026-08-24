//! `EXTRA_RESOURCES`, `HARMFUL_ARGS`, `DEFAULT_ARGS` and `STEALTH_ARGS` as
//! `&[&str]` constants.
//!
//! Copied verbatim, in source order, from
//! `reference/engines/constants.py` (the reference implementation v0.4.15). The Python
//! `EXTRA_RESOURCES` is a `set` and the three arg tables are tuples; here they
//! are all ordered slices, because Chromium's command line is order-sensitive
//! and a slice is the cheapest thing to hand to a process builder.

/// Playwright / CDP `request.resource_type` values dropped when
/// `disable_resources = true`.
///
/// Checked **before** the domain blocklist by the interception handler, which
/// short-circuits on a match.
pub const EXTRA_RESOURCES: &[&str] = &[
    "font",
    "image",
    "media",
    "beacon",
    "object",
    "imageset",
    "texttrack",
    "websocket",
    "csp_report",
    "stylesheet",
];

/// Chromium default flags that must be *removed* (`ignore_default_args`).
///
/// `--enable-automation` is the headline detection signal; dropping the others
/// also sidesteps the popup-crash abuse of
/// <https://issues.chromium.org/issues/340836884>.
pub const HARMFUL_ARGS: &[&str] = &[
    "--enable-automation",
    "--disable-popup-blocking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-extensions",
];

/// Flags added to every Chromium launch to speed the browser up.
pub const DEFAULT_ARGS: &[&str] = &[
    "--no-pings",
    "--no-first-run",
    "--disable-infobars",
    "--disable-breakpad",
    "--no-service-autorun",
    "--homepage=about:blank",
    "--password-store=basic",
    "--disable-hang-monitor",
    "--no-default-browser-check",
    "--disable-session-crashed-bubble",
    "--disable-search-engine-choice-screen",
];

/// Flags added on the stealth path, on top of [`DEFAULT_ARGS`].
///
/// Reference: <https://peter.sh/experiments/chromium-command-line-switches/>.
/// `--incognito` and `--disable-popup-blocking` are commented out in the
/// Python source and are therefore absent here too.
pub const STEALTH_ARGS: &[&str] = &[
    "--test-type",
    "--mute-audio",
    "--disable-sync",
    "--hide-scrollbars",
    "--disable-logging",
    // For headless check bypass.
    "--start-maximized",
    "--enable-async-dns",
    "--use-mock-keychain",
    "--disable-translate",
    "--disable-voice-input",
    "--window-position=0,0",
    "--disable-wake-on-wifi",
    "--ignore-gpu-blocklist",
    "--enable-tcp-fast-open",
    "--enable-web-bluetooth",
    "--disable-cloud-import",
    "--disable-print-preview",
    "--disable-dev-shm-usage",
    "--metrics-recording-only",
    "--disable-crash-reporter",
    "--disable-partial-raster",
    "--disable-gesture-typing",
    "--disable-checker-imaging",
    "--disable-prompt-on-repost",
    "--force-color-profile=srgb",
    "--font-render-hinting=none",
    "--aggressive-cache-discard",
    "--disable-cookie-encryption",
    "--disable-domain-reliability",
    "--disable-threaded-animation",
    "--disable-threaded-scrolling",
    "--enable-simple-cache-backend",
    "--disable-background-networking",
    "--enable-surface-synchronization",
    "--disable-image-animation-resync",
    "--disable-renderer-backgrounding",
    "--disable-ipc-flooding-protection",
    "--prerender-from-omnibox=disabled",
    "--safebrowsing-disable-auto-update",
    "--disable-offer-upload-credit-cards",
    "--disable-background-timer-throttling",
    "--disable-new-content-rendering-timeout",
    "--run-all-compositor-stages-before-draw",
    "--disable-client-side-phishing-detection",
    "--disable-backgrounding-occluded-windows",
    "--disable-layer-tree-host-memory-pressure",
    "--autoplay-policy=user-gesture-required",
    "--disable-offer-store-unmasked-wallet-cards",
    "--disable-blink-features=AutomationControlled",
    "--disable-component-extensions-with-background-pages",
    "--enable-features=NetworkService,NetworkServiceInProcess,TrustTokens,TrustTokensAlwaysAllowIssuance",
    "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4",
    "--disable-features=AudioServiceOutOfProcess,TranslateUI,BlinkGenPropertyTrees",
];

/// Whether `resource_type` should be aborted when `disable_resources` is on.
///
/// # Examples
///
/// ```
/// use browser_fingerprint::domains::is_extra_resource;
///
/// assert!(is_extra_resource("stylesheet"));
/// assert!(!is_extra_resource("document"));
/// ```
#[must_use]
pub fn is_extra_resource(resource_type: &str) -> bool {
    EXTRA_RESOURCES.contains(&resource_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_match_the_python_source() {
        assert_eq!(EXTRA_RESOURCES.len(), 10);
        assert_eq!(HARMFUL_ARGS.len(), 5);
        assert_eq!(DEFAULT_ARGS.len(), 11);
        assert_eq!(STEALTH_ARGS.len(), 53);
    }

    #[test]
    fn every_flag_is_a_switch_and_unique() {
        for table in [HARMFUL_ARGS, DEFAULT_ARGS, STEALTH_ARGS] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "duplicate flag in {table:?}");
            for flag in table {
                assert!(flag.starts_with("--"), "{flag} is not a Chromium switch");
            }
        }
    }

    #[test]
    fn commented_out_python_entries_stay_out() {
        assert!(!STEALTH_ARGS.contains(&"--incognito"));
        assert!(!STEALTH_ARGS.contains(&"--disable-popup-blocking"));
    }

    #[test]
    fn stealth_args_do_not_reintroduce_harmful_defaults() {
        for harmful in HARMFUL_ARGS {
            assert!(
                !DEFAULT_ARGS.contains(harmful),
                "{harmful} is both removed and added"
            );
        }
    }

    #[test]
    fn resource_membership() {
        for resource in EXTRA_RESOURCES {
            assert!(is_extra_resource(resource));
        }
        for resource in ["document", "script", "xhr", "fetch", "manifest", ""] {
            assert!(!is_extra_resource(resource), "{resource}");
        }
    }
}
