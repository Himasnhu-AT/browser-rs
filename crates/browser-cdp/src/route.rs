//! The Playwright↔CDP `resourceType` mapping table and the pure
//! [`RoutePolicy::should_block`] decision. Zero CDP.
//!
//! This is the Rust shape of `create_intercept_handler` in
//! `reference/engines/toolbelt/navigation.py`, split into the *decision* (here)
//! and the *transport* (unit 14's `Fetch`-domain plumbing).
//!
//! # The handler, exactly
//!
//! ```text
//! if resource_type in EXTRA_RESOURCES (and disable_resources):  abort
//! elif blocked_domains is non-empty:
//!     abort if the request host, or any of its parent domains, is listed
//!     else continue
//! else: continue
//! ```
//!
//! Two consequences are pinned by the Python tests and reproduced here:
//! resource-type blocking is evaluated **before** domain blocking, and a
//! non-blocked resource type coming from a blocked domain is still aborted.
//!
//! # Why a mapping table and not `to_lowercase()`
//!
//! Playwright's Chromium backend does **not** lower-case the CDP
//! `Network.ResourceType`; it runs an explicit `switch` (`toResourceType` in
//! `server/chromium/crNetworkManager.ts`). Two things follow that a naive
//! lower-casing gets wrong:
//!
//! * `CSPViolationReport` becomes `"cspreport"` — not `"cspviolationreport"`,
//!   and not `"csp_report"`, which is the spelling the reference implementation's
//!   `EXTRA_RESOURCES` actually lists.
//! * `Prefetch`, `SignedExchange`, `Preflight` and `FedCM` all collapse into
//!   `"other"`, as does any type the switch has not caught up with.
//!
//! Four members of `EXTRA_RESOURCES` therefore never match on Chromium:
//! `beacon` (Firefox and WebKit emit it, Chromium does not), and `object`,
//! `imageset` and `csp_report`, which **no** Playwright backend ever emits —
//! Firefox maps `TYPE_OBJECT` to `other`, `TYPE_IMAGESET` to `image`, and
//! `TYPE_CSP_REPORT` to `cspreport`.
//!
//! In other words: on Chromium, `disable_resources=True` really blocks only
//! `font`, `image`, `media`, `texttrack`, `websocket` and `stylesheet`. That is
//! the reference implementation's behaviour, so it is this crate's default
//! ([`RoutePolicy::decide_cdp`]). [`RoutePolicy::decide_cdp_lenient`] applies
//! the aliases in [`PLAYWRIGHT_ONLY_RESOURCE_TYPES`] first, blocking what
//! the reference implementation evidently *meant* to block, and is opt-in precisely because it
//! blocks strictly more.
//!
//! The table is pinned against the driver the reference implementation actually ships by
//! `tests/resource_types_golden.rs`.

use std::collections::HashSet;

use browser_fingerprint::ad_domains::is_domain_blocked;
use browser_fingerprint::domains::is_extra_resource;

use crate::config::{BrowserConfig, ResolvedFetchParams};

/// Every `Network.ResourceType` CDP can report, paired with the string
/// Playwright hands to a route handler.
///
/// Transcribed from `toResourceType` in Playwright's
/// `server/chromium/crNetworkManager.ts`, in that switch's own source order.
/// It is *not* a lower-casing: `CSPViolationReport` becomes `cspreport`, and
/// `Prefetch`, `SignedExchange`, `Preflight` and `FedCM` all become `other`.
pub const RESOURCE_TYPE_TABLE: &[(&str, &str)] = &[
    ("Document", "document"),
    ("Stylesheet", "stylesheet"),
    ("Image", "image"),
    ("Media", "media"),
    ("Font", "font"),
    ("Script", "script"),
    ("TextTrack", "texttrack"),
    ("XHR", "xhr"),
    ("Fetch", "fetch"),
    ("Prefetch", "other"),
    ("EventSource", "eventsource"),
    ("WebSocket", "websocket"),
    ("Manifest", "manifest"),
    ("SignedExchange", "other"),
    ("Ping", "ping"),
    ("CSPViolationReport", "cspreport"),
    ("Preflight", "other"),
    ("FedCM", "other"),
    ("Other", "other"),
];

/// The resource type Playwright's Chromium backend falls back to for a CDP
/// type its `switch` does not name (its `default:` arm).
pub const CHROMIUM_FALLBACK_RESOURCE_TYPE: &str = "other";

/// The `EXTRA_RESOURCES` spellings Chromium never produces, each paired with
/// the CDP type that carries the same traffic.
///
/// This is why `disable_resources = true` blocks less on Chromium than the
/// constant suggests:
///
/// | `EXTRA_RESOURCES` name | nearest CDP type | note |
/// |---|---|---|
/// | `beacon` | `Ping` | real, but only Firefox and WebKit emit it |
/// | `object` | `Other` | dead everywhere: Firefox maps `TYPE_OBJECT` to `other` |
/// | `imageset` | `Image` | dead everywhere: Firefox maps `TYPE_IMAGESET` to `image` |
/// | `csp_report` | `CSPViolationReport` | dead everywhere: both backends say `cspreport` |
///
/// Only `beacon` is a real Playwright resource type; the other three are
/// spellings no Playwright backend has ever emitted. See
/// [`DEAD_EVERYWHERE_RESOURCE_TYPES`].
pub const PLAYWRIGHT_ONLY_RESOURCE_TYPES: &[(&str, &str)] = &[
    ("beacon", "Ping"),
    ("object", "Other"),
    ("imageset", "Image"),
    ("csp_report", "CSPViolationReport"),
];

/// The `EXTRA_RESOURCES` entries that no Playwright backend — Chromium,
/// Firefox or WebKit — has ever produced, so they can never block anything.
pub const DEAD_EVERYWHERE_RESOURCE_TYPES: &[&str] = &["csp_report", "imageset", "object"];

/// The Playwright spelling of a CDP `Network.ResourceType`.
///
/// Unknown types fall back to [`CHROMIUM_FALLBACK_RESOURCE_TYPE`], which is
/// what Playwright's `default:` arm does for a type its switch has not caught
/// up with.
///
/// ```
/// use browser_cdp::route::playwright_resource_type;
///
/// assert_eq!(playwright_resource_type("Stylesheet"), "stylesheet");
/// assert_eq!(playwright_resource_type("XHR"), "xhr");
/// assert_eq!(playwright_resource_type("CSPViolationReport"), "cspreport");
/// assert_eq!(playwright_resource_type("Preflight"), "other");
/// assert_eq!(playwright_resource_type("BrandNewType"), "other");
/// ```
#[must_use]
pub fn playwright_resource_type(cdp_resource_type: &str) -> &'static str {
    RESOURCE_TYPE_TABLE
        .iter()
        .find(|(cdp, _)| *cdp == cdp_resource_type)
        .map_or(CHROMIUM_FALLBACK_RESOURCE_TYPE, |(_, playwright)| {
            *playwright
        })
}

/// The CDP `Network.ResourceType` behind a Playwright resource-type string.
///
/// The mapping is many-to-one — `Prefetch`, `SignedExchange`, `Preflight`,
/// `FedCM` and `Other` all yield `"other"` — so the inverse is the *canonical*
/// type, i.e. the one whose own name matches (`Other`). Returns `None` for a
/// name Chromium never emits and that has no alias in
/// [`PLAYWRIGHT_ONLY_RESOURCE_TYPES`].
///
/// ```
/// use browser_cdp::route::cdp_resource_type;
///
/// assert_eq!(cdp_resource_type("stylesheet"), Some("Stylesheet"));
/// assert_eq!(cdp_resource_type("cspreport"), Some("CSPViolationReport"));
/// assert_eq!(cdp_resource_type("other"), Some("Other"));
/// assert_eq!(cdp_resource_type("csp_report"), Some("CSPViolationReport"));
/// assert_eq!(cdp_resource_type("imageset"), Some("Image"));
/// assert_eq!(cdp_resource_type("nonsense"), None);
/// ```
#[must_use]
pub fn cdp_resource_type(playwright_resource_type: &str) -> Option<&'static str> {
    RESOURCE_TYPE_TABLE
        .iter()
        .find(|(cdp, playwright)| {
            *playwright == playwright_resource_type && cdp.eq_ignore_ascii_case(playwright)
        })
        .or_else(|| {
            RESOURCE_TYPE_TABLE
                .iter()
                .find(|(_, playwright)| *playwright == playwright_resource_type)
        })
        .map(|(cdp, _)| *cdp)
        .or_else(|| {
            PLAYWRIGHT_ONLY_RESOURCE_TYPES
                .iter()
                .find(|(playwright, _)| *playwright == playwright_resource_type)
                .map(|(_, cdp)| *cdp)
        })
}

/// Whether a Playwright resource-type name can never appear on Chromium.
///
/// ```
/// use browser_cdp::route::is_chromium_unreachable;
///
/// assert!(is_chromium_unreachable("beacon"));
/// assert!(is_chromium_unreachable("csp_report"));
/// assert!(!is_chromium_unreachable("image"));
/// ```
#[must_use]
pub fn is_chromium_unreachable(playwright_resource_type: &str) -> bool {
    PLAYWRIGHT_ONLY_RESOURCE_TYPES
        .iter()
        .any(|(name, _)| *name == playwright_resource_type)
}

/// The Playwright resource types that `disable_resources = true` can actually
/// abort on Chromium.
///
/// `EXTRA_RESOURCES` minus [`PLAYWRIGHT_ONLY_RESOURCE_TYPES`].
#[must_use]
pub fn effective_blocked_resource_types() -> Vec<&'static str> {
    browser_fingerprint::domains::EXTRA_RESOURCES
        .iter()
        .copied()
        .filter(|name| !is_chromium_unreachable(name))
        .collect()
}

/// What the route handler does with one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RouteDecision {
    /// `route.continue_()`.
    #[default]
    Continue,
    /// `route.abort()`.
    Abort,
}

impl RouteDecision {
    /// Whether this decision aborts the request.
    #[must_use]
    pub const fn is_abort(self) -> bool {
        matches!(self, Self::Abort)
    }
}

/// Why a request was aborted — used for the `debug!` line Python logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockReason {
    /// The resource type is in `EXTRA_RESOURCES` and `disable_resources` is on.
    ResourceType,
    /// The request host, or one of its parent domains, is on the blocklist.
    BlockedDomain,
}

/// The per-request blocking policy: `disable_resources` plus the resolved
/// domain blocklist.
///
/// Build one per request — Python re-creates the handler on every `page.route`
/// call, and the blocklist can be overridden per request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutePolicy {
    /// Abort requests whose resource type is in `EXTRA_RESOURCES`.
    pub disable_resources: bool,
    /// Hosts to abort, already unioned with `AD_DOMAINS` if `block_ads` was on.
    pub blocked_domains: HashSet<String>,
}

impl RoutePolicy {
    /// A policy from the two inputs `create_intercept_handler` takes.
    #[must_use]
    pub fn new(disable_resources: bool, blocked_domains: Option<HashSet<String>>) -> Self {
        Self {
            disable_resources,
            blocked_domains: blocked_domains.unwrap_or_default(),
        }
    }

    /// The policy one `fetch()` call runs with.
    #[must_use]
    pub fn from_params(params: &ResolvedFetchParams) -> Self {
        Self::new(params.disable_resources, params.blocked_domains.clone())
    }

    /// The policy a session runs with when no per-request override is given.
    #[must_use]
    pub fn from_config(config: &BrowserConfig) -> Self {
        Self::new(config.disable_resources, config.blocked_domains.clone())
    }

    /// Whether interception has to be installed at all.
    ///
    /// Python registers the handler only `if disable_resources or
    /// blocked_domains`, and `unroute_all` clears it before the tab is reused.
    /// Skipping `Fetch.enable` when nothing can match is a real saving: every
    /// paused request otherwise costs a CDP round trip.
    #[must_use]
    pub fn needs_interception(&self) -> bool {
        self.disable_resources || !self.blocked_domains.is_empty()
    }

    /// The full handler decision for a **Playwright-spelled** resource type.
    ///
    /// `host` is `urlparse(request.url).hostname or ""`; see [`host_of`].
    #[must_use]
    pub fn decide(&self, resource_type: &str, host: &str) -> RouteDecision {
        if self.reason(resource_type, host).is_some() {
            RouteDecision::Abort
        } else {
            RouteDecision::Continue
        }
    }

    /// [`RoutePolicy::decide`] plus why, for logging.
    #[must_use]
    pub fn reason(&self, resource_type: &str, host: &str) -> Option<BlockReason> {
        if self.disable_resources && is_extra_resource(resource_type) {
            return Some(BlockReason::ResourceType);
        }
        if !self.blocked_domains.is_empty() && is_domain_blocked(host, &self.blocked_domains) {
            return Some(BlockReason::BlockedDomain);
        }
        None
    }

    /// The pure decision function: should this request be aborted?
    ///
    /// ```
    /// use browser_cdp::route::RoutePolicy;
    ///
    /// let policy = RoutePolicy::new(true, Some(["doubleclick.net".to_owned()].into()));
    /// assert!(policy.should_block("image", "cdn.example.com"));
    /// assert!(policy.should_block("script", "tracker.ads.doubleclick.net"));
    /// assert!(!policy.should_block("script", "example.com"));
    /// ```
    #[must_use]
    pub fn should_block(&self, resource_type: &str, host: &str) -> bool {
        self.decide(resource_type, host).is_abort()
    }

    /// [`RoutePolicy::decide`] for a raw CDP `Fetch.requestPaused.resourceType`.
    ///
    /// Bug-compatible with the reference implementation: `CSPViolationReport` maps to
    /// `cspreport`, which is **not** in `EXTRA_RESOURCES` (that set spells it
    /// `csp_report`), so it is not blocked.
    #[must_use]
    pub fn decide_cdp(&self, cdp_resource_type: &str, host: &str) -> RouteDecision {
        self.decide(playwright_resource_type(cdp_resource_type), host)
    }

    /// [`RoutePolicy::decide_cdp`] with the [`PLAYWRIGHT_ONLY_RESOURCE_TYPES`]
    /// aliases applied, i.e. what `EXTRA_RESOURCES` evidently *meant*.
    ///
    /// Opt-in, because it blocks strictly more than Python does: on Chromium it
    /// additionally aborts `CSPViolationReport` requests.
    #[must_use]
    pub fn decide_cdp_lenient(&self, cdp_resource_type: &str, host: &str) -> RouteDecision {
        if self.disable_resources {
            let aliased = PLAYWRIGHT_ONLY_RESOURCE_TYPES
                .iter()
                .any(|(name, cdp)| *cdp == cdp_resource_type && is_extra_resource(name));
            if aliased {
                return RouteDecision::Abort;
            }
        }
        self.decide(playwright_resource_type(cdp_resource_type), host)
    }
}

/// `urlparse(url).hostname or ""`.
///
/// Lower-cased, and with the brackets stripped from an IPv6 literal so the
/// result feeds [`is_domain_blocked`] the same string Python does. An
/// unparsable URL yields `""`, which matches no blocklist entry.
///
/// ```
/// use browser_cdp::route::host_of;
///
/// assert_eq!(host_of("https://Ads.Example.com/a?b=c"), "ads.example.com");
/// assert_eq!(host_of("http://[::1]:8080/"), "::1");
/// assert_eq!(host_of("not a url"), "");
/// assert_eq!(host_of("//h/x"), "h", "scheme-relative, like `urlparse`");
/// ```
///
/// A URL whose authority CPython would reject outright — unbalanced `[`/`]`,
/// or a bracketed host that is not an IP literal — yields `""` here, whereas
/// Python's handler would propagate the `ValueError` out of `urlparse` and
/// leave the request unrouted. Chromium never reports such a URL, and `""`
/// matches no blocklist entry, so the request continues either way.
#[must_use]
pub fn host_of(url: &str) -> String {
    // Not `url::Url::parse`: that crate is RFC-3986-strict and returns
    // `RelativeUrlWithoutBase` for a scheme-relative `//host/path`, where
    // `urlparse` happily reports `host`. The shared CPython `urlsplit` port is
    // used instead so the proxy and routing paths agree on one parser.
    crate::config::urlsplit(url)
        .ok()
        .and_then(|parts| parts.hostname)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_fingerprint::domains::EXTRA_RESOURCES;

    fn blocklist(domains: &[&str]) -> Option<HashSet<String>> {
        Some(domains.iter().map(|d| (*d).to_owned()).collect())
    }

    #[test]
    fn resource_type_table_round_trips() {
        for (cdp, playwright) in RESOURCE_TYPE_TABLE {
            assert_eq!(playwright_resource_type(cdp), *playwright);
            // Many CDP types collapse to `other`; the inverse is the canonical
            // one, so only round-trip the unambiguous rows.
            if *playwright != CHROMIUM_FALLBACK_RESOURCE_TYPE {
                assert_eq!(cdp_resource_type(playwright), Some(*cdp));
            }
        }
        assert_eq!(cdp_resource_type("other"), Some("Other"));
        // Unknown CDP types take Playwright's `default:` arm.
        assert_eq!(playwright_resource_type("BrandNewType"), "other");
        assert_eq!(cdp_resource_type("brandnewtype"), None);
    }

    #[test]
    fn firefox_only_names_map_to_their_nearest_cdp_type() {
        assert_eq!(cdp_resource_type("beacon"), Some("Ping"));
        assert_eq!(cdp_resource_type("object"), Some("Other"));
        assert_eq!(cdp_resource_type("imageset"), Some("Image"));
        assert_eq!(cdp_resource_type("csp_report"), Some("CSPViolationReport"));
        for (name, _) in PLAYWRIGHT_ONLY_RESOURCE_TYPES {
            assert!(is_chromium_unreachable(name), "{name}");
            assert!(
                EXTRA_RESOURCES.contains(name),
                "the whole point is that {name} is in EXTRA_RESOURCES"
            );
        }
    }

    #[test]
    fn only_six_of_the_ten_extra_resources_can_fire_on_chromium() {
        let mut effective = effective_blocked_resource_types();
        effective.sort_unstable();
        assert_eq!(
            effective,
            [
                "font",
                "image",
                "media",
                "stylesheet",
                "texttrack",
                "websocket"
            ]
        );
    }

    #[test]
    fn resource_blocking_covers_the_reachable_extra_resources() {
        let policy = RoutePolicy::new(true, None);
        for name in effective_blocked_resource_types() {
            assert!(policy.should_block(name, "example.com"), "{name}");
        }
        for name in ["document", "script", "xhr", "fetch", "other"] {
            assert!(!policy.should_block(name, "example.com"), "{name}");
        }
    }

    #[test]
    fn resource_blocking_is_off_by_default() {
        let policy = RoutePolicy::default();
        assert!(!policy.should_block("image", "example.com"));
        assert!(!policy.needs_interception());
        assert_eq!(
            policy.decide("image", "example.com"),
            RouteDecision::Continue
        );
    }

    #[test]
    fn domain_blocking_walks_the_suffix_chain() {
        let policy = RoutePolicy::new(false, blocklist(&["example.com", "doubleclick.net"]));
        assert!(policy.should_block("document", "example.com"));
        assert!(policy.should_block("document", "sub.example.com"));
        assert!(policy.should_block("document", "a.b.c.example.com"));
        assert!(policy.should_block("script", "tracker.ads.doubleclick.net"));
        assert!(!policy.should_block("document", "notexample.com"));
        assert!(!policy.should_block("document", "example.org"));
        assert!(!policy.should_block("document", ""));
    }

    #[test]
    fn a_bare_tld_is_never_consulted() {
        let policy = RoutePolicy::new(false, blocklist(&["com"]));
        assert!(
            !policy.should_block("document", "sub.example.com"),
            "the suffix chain skips suffixes without a dot"
        );
        assert!(
            policy.should_block("document", "com"),
            "an exact match still hits"
        );
    }

    #[test]
    fn resource_type_is_evaluated_before_the_domain_list() {
        let policy = RoutePolicy::new(true, blocklist(&["allowed.example"]));
        // An image from a host that is *not* blocked: still aborted, and the
        // reason is the resource type.
        assert_eq!(
            policy.reason("image", "cdn.other.example"),
            Some(BlockReason::ResourceType)
        );
        // A non-blocked resource type from a blocked domain is aborted too.
        assert_eq!(
            policy.reason("script", "sub.allowed.example"),
            Some(BlockReason::BlockedDomain)
        );
        // Neither: continue.
        assert_eq!(policy.reason("script", "other.example"), None);
    }

    #[test]
    fn needs_interception_matches_pythons_guard() {
        assert!(!RoutePolicy::new(false, None).needs_interception());
        assert!(!RoutePolicy::new(false, Some(HashSet::new())).needs_interception());
        assert!(RoutePolicy::new(true, None).needs_interception());
        assert!(RoutePolicy::new(false, blocklist(&["x.example"])).needs_interception());
    }

    #[test]
    fn cdp_decisions_are_bug_compatible_by_default() {
        let policy = RoutePolicy::new(true, None);
        assert_eq!(
            policy.decide_cdp("Image", "example.com"),
            RouteDecision::Abort
        );
        assert_eq!(
            policy.decide_cdp("Stylesheet", "example.com"),
            RouteDecision::Abort
        );
        assert_eq!(
            policy.decide_cdp("Document", "example.com"),
            RouteDecision::Continue
        );
        assert_eq!(
            policy.decide_cdp("CSPViolationReport", "example.com"),
            RouteDecision::Continue,
            "Chromium spells it `cspreport`, which EXTRA_RESOURCES does not list"
        );
        assert_eq!(
            policy.decide_cdp_lenient("CSPViolationReport", "example.com"),
            RouteDecision::Abort,
            "the lenient mapping applies the csp_report alias"
        );
        // The lenient variant changes nothing else.
        assert_eq!(
            policy.decide_cdp_lenient("Document", "example.com"),
            RouteDecision::Continue
        );
        // ...and nothing at all when disable_resources is off.
        let off = RoutePolicy::new(false, None);
        assert_eq!(
            off.decide_cdp_lenient("CSPViolationReport", "example.com"),
            RouteDecision::Continue
        );
    }

    #[test]
    fn policies_can_be_built_from_a_config_or_from_resolved_params() {
        let config = BrowserConfig::default()
            .disable_resources(true)
            .block_ads(true)
            .validate()
            .expect("valid");
        let policy = RoutePolicy::from_config(&config);
        assert!(policy.disable_resources);
        assert!(policy.should_block("document", "ads.doubleclick.net"));

        let params = crate::config::FetchParams::new()
            .resolve(&config)
            .expect("resolves");
        let from_params = RoutePolicy::from_params(&params);
        assert_eq!(from_params, policy);
    }

    #[test]
    fn host_extraction_matches_urlparse_hostname() {
        assert_eq!(host_of("https://Ads.Example.com/a?b=c"), "ads.example.com");
        assert_eq!(host_of("http://example.com:8080/"), "example.com");
        assert_eq!(host_of("http://[::1]:8080/"), "::1");
        assert_eq!(host_of("https://user:pw@example.com/"), "example.com");
        assert_eq!(host_of("about:blank"), "");
        assert_eq!(host_of("not a url"), "");
        assert_eq!(host_of(""), "");
        assert_eq!(host_of("data:text/html,<p>hi</p>"), "");
    }

    #[test]
    fn a_blocked_ad_host_from_the_built_in_list_is_aborted() {
        let config = BrowserConfig::default()
            .block_ads(true)
            .validate()
            .expect("valid");
        let policy = RoutePolicy::from_config(&config);
        assert!(policy.should_block("script", "googlesyndication.com"));
        assert!(policy.should_block("script", "pagead2.googlesyndication.com"));
        assert!(!policy.should_block("script", "example.com"));
    }
}
