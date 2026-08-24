//! [`BrowserConfig`] and [`StealthExtras`] with every default, range constraint
//! and the `__post_init__` validation chain. Zero CDP.
//!
//! This is the Rust shape of `reference/engines/_browsers/_validators.py`:
//! `PlaywrightConfig`, `StealthConfig`, `_fetch_params` and `validate_fetch`.
//!
//! # The two-phase contract
//!
//! Python builds a `msgspec.Struct` from a kwargs dict and mutates it in
//! `__post_init__`. The port splits that in two so the "has been validated"
//! state is visible to the type system:
//!
//! 1. [`BrowserConfig`] is the **builder**. Its [`Default`] is the reference implementation's
//!    default for every field, and its chainable setters take the same values
//!    the Python keyword arguments take.
//! 2. [`BrowserConfig::validate`] runs the range constraints *and* the whole
//!    `__post_init__` chain, and hands back a [`ValidatedConfig`]. Only a
//!    `ValidatedConfig` has a normalised proxy, a `blocked_domains` set that
//!    already contains `AD_DOMAINS` when `block_ads` is on, and the
//!    Cloudflare timeout bump applied.
//!
//! ```
//! use browser_cdp::config::BrowserConfig;
//!
//! let cfg = BrowserConfig::default()
//!     .max_pages(4)
//!     .headless(false)
//!     .block_ads(true)
//!     .validate()?;
//!
//! assert_eq!(cfg.max_pages, 4);
//! assert!(cfg.blocked_domains.as_ref().is_some_and(|d| d.contains("doubleclick.net")));
//! # Ok::<(), browser_cdp::error::ConfigError>(())
//! ```
//!
//! # Units
//!
//! Python aliases all three to `Seconds`, which is wrong for two of them.
//! The port names them:
//!
//! | field | unit |
//! |---|---|
//! | [`BrowserConfig::timeout_ms`] | **milliseconds** |
//! | [`BrowserConfig::wait_ms`] | **milliseconds** |
//! | [`BrowserConfig::retry_delay_secs`] | **seconds** |
//!
//! # What is deliberately not here
//!
//! * `page_action` / `page_setup` — they receive the live page handle, so they
//!   belong to the session (unit 14). Python's
//!   `TypeError("page_action must be callable, …")` check has no Rust
//!   counterpart: a boxed closure is callable by construction.
//! * The `__max_pages` private alias — it exists only because the MCP server
//!   injects a pool size into an untyped kwargs dict. A typed builder just
//!   calls [`BrowserConfig::max_pages`].
//! * `msgspec`'s "unknown keyword arguments are silently ignored" behaviour
//!   (`StealthyFetcher.fetch(url, humanize=True)` runs and drops `humanize`).
//!   A typed builder cannot express an unknown field, which is an improvement
//!   but a behaviour change for callers porting such code.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use browser_core::types::{Override, Proxy, SetCookieParam, WaitState};
use browser_fingerprint::ad_domains::AD_DOMAINS;

use crate::error::{ConfigError, EXECUTABLE_LABEL, INIT_SCRIPT_LABEL};
use crate::flags::ExtraFlagsPolicy;

/// Default `max_pages` (`PlaywrightConfig.max_pages`).
pub const DEFAULT_MAX_PAGES: u32 = 1;
/// Inclusive `max_pages` range, from `PagesCount = Annotated[int, Meta(ge=1, le=50)]`.
pub const MAX_PAGES_RANGE: std::ops::RangeInclusive<u32> = 1..=50;
/// Default `retries`.
pub const DEFAULT_RETRIES: u32 = 3;
/// Inclusive `retries` range, from `RetriesCount = Annotated[int, Meta(ge=1, le=10)]`.
pub const RETRIES_RANGE: std::ops::RangeInclusive<u32> = 1..=10;
/// Default `timeout`, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: f64 = 30_000.0;
/// Default `retry_delay`, in seconds.
pub const DEFAULT_RETRY_DELAY_SECS: f64 = 1.0;
/// Floor applied to `timeout` when `solve_cloudflare` is on, in milliseconds.
pub const CLOUDFLARE_MIN_TIMEOUT_MS: f64 = 60_000.0;
/// Proxy URL schemes `construct_proxy_dict` accepts.
pub const PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks4", "socks5"];
/// CDP URL schemes `_is_invalid_cdp_url` accepts.
pub const CDP_SCHEMES: [&str; 4] = ["ws://", "wss://", "http://", "https://"];

/// A source of proxies for rotation mode.
///
/// The concrete `ProxyRotator` (Python's
/// `reference/engines/toolbelt/proxy_rotation.py`) is owned by the HTTP crate;
/// the browser config only needs "is one attached, and what is the next proxy",
/// so it depends on this trait instead. Attaching a rotator switches the
/// session into *fresh-context-per-request* mode (see `browser.md` §4.5) and is
/// mutually exclusive with a static [`BrowserConfig::proxy`].
pub trait ProxyRotation: std::fmt::Debug + Send + Sync {
    /// The next proxy in the rotation. Must be internally synchronised —
    /// Python guards `get_proxy()` with a mutex.
    fn next_proxy(&self) -> Proxy;

    /// How many proxies the rotator holds.
    fn len(&self) -> usize;

    /// Whether the rotator holds no proxies.
    ///
    /// Python's `ProxyRotator.__init__` rejects an empty list outright, so a
    /// well-formed rotator never reports `true`.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The four stealth-only knobs of `StealthConfig`.
///
/// Presence of this struct on a [`BrowserConfig`] is what selects the *stealthy*
/// engine: it turns on `STEALTH_ARGS`, the stealth context options and the
/// Cloudflare solver. See [`crate::flags`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StealthExtras {
    /// `False` adds `--disable-webgl --disable-webgl-image-chromium --disable-webgl2`.
    pub allow_webgl: bool,
    /// `True` adds `--fingerprinting-canvas-image-data-noise`.
    pub hide_canvas: bool,
    /// `True` adds the two WebRTC IP-handling flags.
    pub block_webrtc: bool,
    /// Run the Turnstile / Interstitial solver after navigation.
    ///
    /// Turning this on at **construction** raises `timeout` to at least
    /// [`CLOUDFLARE_MIN_TIMEOUT_MS`]; turning it on **per request** does not
    /// (see [`FetchParams`]).
    pub solve_cloudflare: bool,
}

impl Default for StealthExtras {
    /// `allow_webgl=True`, everything else `False`.
    fn default() -> Self {
        Self {
            allow_webgl: true,
            hide_canvas: false,
            block_webrtc: false,
            solve_cloudflare: false,
        }
    }
}

impl StealthExtras {
    /// The Python defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `allow_webgl`.
    #[must_use]
    pub const fn allow_webgl(mut self, value: bool) -> Self {
        self.allow_webgl = value;
        self
    }

    /// Set `hide_canvas`.
    #[must_use]
    pub const fn hide_canvas(mut self, value: bool) -> Self {
        self.hide_canvas = value;
        self
    }

    /// Set `block_webrtc`.
    #[must_use]
    pub const fn block_webrtc(mut self, value: bool) -> Self {
        self.block_webrtc = value;
        self
    }

    /// Set `solve_cloudflare`.
    #[must_use]
    pub const fn solve_cloudflare(mut self, value: bool) -> Self {
        self.solve_cloudflare = value;
        self
    }
}

/// Every keyword argument of `PlaywrightSession` / `StealthSession`.
///
/// Fields are public so a caller can build one literally from a
/// [`BrowserConfig::default`], but the chainable setters keep call sites
/// readable. Nothing is validated until [`BrowserConfig::validate`] runs.
#[derive(Clone, Debug)]
pub struct BrowserConfig {
    /// Max simultaneously open tabs (`1..=50`, default `1`).
    ///
    /// Only the async sessions honour it; the sync façade pins the pool to 1.
    pub max_pages: u32,
    /// Headless vs headful (default `true`). Also decides whether a synthetic
    /// User-Agent is injected.
    pub headless: bool,
    /// Abort requests whose resource type is in `EXTRA_RESOURCES`.
    pub disable_resources: bool,
    /// Additionally wait for the `networkidle` load state.
    pub network_idle: bool,
    /// Additionally wait for `domcontentloaded` (default `true`).
    pub load_dom: bool,
    /// CSS selector to wait for after navigation / `page_action`.
    pub wait_selector: Option<String>,
    /// State [`BrowserConfig::wait_selector`] must reach (default
    /// [`WaitState::Attached`]).
    pub wait_selector_state: WaitState,
    /// Cookies added to the browser context at `start()`.
    pub cookies: Vec<SetCookieParam>,
    /// Send `Referer: https://www.google.com/` unless `extra_headers` already
    /// carries a `referer` (case-insensitive). Default `true`.
    pub google_search: bool,
    /// Milliseconds to idle after everything else (default `0`).
    pub wait_ms: f64,
    /// Playwright context `timezone_id` (default `Some("")`).
    pub timezone_id: Option<String>,
    /// Static proxy. Mutually exclusive with [`BrowserConfig::proxy_rotator`].
    pub proxy: Option<Proxy>,
    /// Proxy rotator; forces fresh-context-per-request mode.
    pub proxy_rotator: Option<Arc<dyn ProxyRotation>>,
    /// Context-level `extra_http_headers`, re-applied per page per request.
    pub extra_headers: Option<IndexMap<String, String>>,
    /// Default navigation + default timeout, in milliseconds (default `30000`).
    pub timeout_ms: f64,
    /// Absolute path to a `.js` file registered as an init script.
    pub init_script: Option<PathBuf>,
    /// Chromium profile directory; `""` means "let the driver make a temp one".
    pub user_data_dir: String,
    /// Extra kwargs forwarded to the `Selector`/`Response` constructor.
    pub selector_config: serde_json::Map<String, serde_json::Value>,
    /// Merged **last** into the context options — highest priority.
    pub additional_args: serde_json::Map<String, serde_json::Value>,
    /// Browser language. Applied as launch flags, not as the context option
    /// (unless `cdp_url` is set); see [`crate::flags`].
    pub locale: Option<String>,
    /// Launch channel `"chrome"` instead of `"chromium"`.
    pub real_chrome: bool,
    /// Connect over CDP instead of launching.
    pub cdp_url: Option<String>,
    /// Explicit User-Agent string.
    pub useragent: Option<String>,
    /// Extra browser launch flags. **Caution:** with the default
    /// [`ExtraFlagsPolicy::PythonCompatible`] this *suppresses* `STEALTH_ARGS`;
    /// use [`BrowserConfig::extra_flags_union`] for the fixed behaviour.
    pub extra_flags: Vec<String>,
    /// How [`BrowserConfig::extra_flags`] combines with the stealth flag set.
    pub extra_flags_policy: ExtraFlagsPolicy,
    /// Domains (and all sub-domains) whose requests are aborted.
    pub blocked_domains: Option<HashSet<String>>,
    /// Union `AD_DOMAINS` into [`BrowserConfig::blocked_domains`].
    pub block_ads: bool,
    /// Attempts per `fetch()` call (`1..=10`, default `3`).
    pub retries: u32,
    /// Seconds slept between attempts (default `1`).
    pub retry_delay_secs: f64,
    /// Regex; matching `xhr`/`fetch` responses are captured.
    pub capture_xhr: Option<String>,
    /// Absolute path to a browser binary.
    pub executable_path: Option<PathBuf>,
    /// Add `--dns-over-https-templates=https://cloudflare-dns.com/dns-query`.
    pub dns_over_https: bool,
    /// Stealth knobs. `Some(..)` selects the stealthy engine.
    pub stealth: Option<StealthExtras>,
}

impl Default for BrowserConfig {
    /// Every default of `PlaywrightConfig`, verbatim.
    fn default() -> Self {
        Self {
            max_pages: DEFAULT_MAX_PAGES,
            headless: true,
            disable_resources: false,
            network_idle: false,
            load_dom: true,
            wait_selector: None,
            wait_selector_state: WaitState::Attached,
            cookies: Vec::new(),
            google_search: true,
            wait_ms: 0.0,
            timezone_id: Some(String::new()),
            proxy: None,
            proxy_rotator: None,
            extra_headers: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            init_script: None,
            user_data_dir: String::new(),
            selector_config: serde_json::Map::new(),
            additional_args: serde_json::Map::new(),
            locale: None,
            real_chrome: false,
            cdp_url: None,
            useragent: None,
            extra_flags: Vec::new(),
            extra_flags_policy: ExtraFlagsPolicy::PythonCompatible,
            blocked_domains: None,
            block_ads: false,
            retries: DEFAULT_RETRIES,
            retry_delay_secs: DEFAULT_RETRY_DELAY_SECS,
            capture_xhr: None,
            executable_path: None,
            dns_over_https: false,
            stealth: None,
        }
    }
}

/// Chainable setters. Each mirrors one Python keyword argument.
impl BrowserConfig {
    /// A config with every the reference implementation default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A config with every the reference implementation default **plus** the stealth extras, i.e.
    /// `StealthySession(**{})`.
    #[must_use]
    pub fn stealthy() -> Self {
        Self {
            stealth: Some(StealthExtras::default()),
            ..Self::default()
        }
    }

    /// Set `max_pages` (validated to `1..=50`).
    #[must_use]
    pub const fn max_pages(mut self, value: u32) -> Self {
        self.max_pages = value;
        self
    }

    /// Set `headless`.
    #[must_use]
    pub const fn headless(mut self, value: bool) -> Self {
        self.headless = value;
        self
    }

    /// Set `disable_resources`.
    #[must_use]
    pub const fn disable_resources(mut self, value: bool) -> Self {
        self.disable_resources = value;
        self
    }

    /// Set `network_idle`.
    #[must_use]
    pub const fn network_idle(mut self, value: bool) -> Self {
        self.network_idle = value;
        self
    }

    /// Set `load_dom`.
    #[must_use]
    pub const fn load_dom(mut self, value: bool) -> Self {
        self.load_dom = value;
        self
    }

    /// Set `wait_selector`.
    #[must_use]
    pub fn wait_selector(mut self, value: impl Into<String>) -> Self {
        self.wait_selector = Some(value.into());
        self
    }

    /// Set `wait_selector_state`.
    #[must_use]
    pub const fn wait_selector_state(mut self, value: WaitState) -> Self {
        self.wait_selector_state = value;
        self
    }

    /// Set `cookies`.
    #[must_use]
    pub fn cookies(mut self, value: impl IntoIterator<Item = SetCookieParam>) -> Self {
        self.cookies = value.into_iter().collect();
        self
    }

    /// Set `google_search`.
    #[must_use]
    pub const fn google_search(mut self, value: bool) -> Self {
        self.google_search = value;
        self
    }

    /// Set `wait`, in **milliseconds**.
    #[must_use]
    pub const fn wait_ms(mut self, value: f64) -> Self {
        self.wait_ms = value;
        self
    }

    /// Set `timezone_id`.
    #[must_use]
    pub fn timezone_id(mut self, value: impl Into<String>) -> Self {
        self.timezone_id = Some(value.into());
        self
    }

    /// Set the static `proxy`. Conflicts with
    /// [`BrowserConfig::proxy_rotator`].
    #[must_use]
    pub fn proxy(mut self, value: Proxy) -> Self {
        self.proxy = Some(value);
        self
    }

    /// Attach a `proxy_rotator`. Conflicts with [`BrowserConfig::proxy`].
    #[must_use]
    pub fn proxy_rotator(mut self, value: Arc<dyn ProxyRotation>) -> Self {
        self.proxy_rotator = Some(value);
        self
    }

    /// Set `extra_headers`.
    #[must_use]
    pub fn extra_headers<K, V>(mut self, value: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.extra_headers = Some(
            value
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        );
        self
    }

    /// Set `timeout`, in **milliseconds**.
    #[must_use]
    pub const fn timeout_ms(mut self, value: f64) -> Self {
        self.timeout_ms = value;
        self
    }

    /// Set `init_script` (must be an existing, absolute file).
    #[must_use]
    pub fn init_script(mut self, value: impl Into<PathBuf>) -> Self {
        self.init_script = Some(value.into());
        self
    }

    /// Set `user_data_dir`.
    #[must_use]
    pub fn user_data_dir(mut self, value: impl Into<String>) -> Self {
        self.user_data_dir = value.into();
        self
    }

    /// Set `selector_config`.
    #[must_use]
    pub fn selector_config(mut self, value: serde_json::Map<String, serde_json::Value>) -> Self {
        self.selector_config = value;
        self
    }

    /// Set `additional_args`.
    #[must_use]
    pub fn additional_args(mut self, value: serde_json::Map<String, serde_json::Value>) -> Self {
        self.additional_args = value;
        self
    }

    /// Set `locale`.
    #[must_use]
    pub fn locale(mut self, value: impl Into<String>) -> Self {
        self.locale = Some(value.into());
        self
    }

    /// Set `real_chrome`.
    #[must_use]
    pub const fn real_chrome(mut self, value: bool) -> Self {
        self.real_chrome = value;
        self
    }

    /// Set `cdp_url`.
    #[must_use]
    pub fn cdp_url(mut self, value: impl Into<String>) -> Self {
        self.cdp_url = Some(value.into());
        self
    }

    /// Set `useragent`.
    #[must_use]
    pub fn useragent(mut self, value: impl Into<String>) -> Self {
        self.useragent = Some(value.into());
        self
    }

    /// Set `extra_flags`, **reproducing Python's quirk**: supplying launch
    /// flags replaces the whole `STEALTH_ARGS` tuple, leaving
    /// `DEFAULT_ARGS ∪ your flags`.
    ///
    /// This is bug-compatible with `the reference implementation`
    /// (`_base.py::__generate_options__`: `config.extra_flags or extra_flags`).
    /// Prefer [`BrowserConfig::extra_flags_union`] unless you are matching
    /// Python byte for byte.
    ///
    /// ```
    /// use browser_cdp::{config::BrowserConfig, flags::launch_options};
    ///
    /// let quirky = BrowserConfig::stealthy().extra_flags(["--foo"]).validate()?;
    /// let opts = launch_options(&quirky).expect("launching, not connecting");
    /// assert!(opts.args.contains("--foo"));
    /// assert!(!opts.args.contains("--disable-blink-features=AutomationControlled"));
    /// # Ok::<(), browser_cdp::error::ConfigError>(())
    /// ```
    #[must_use]
    pub fn extra_flags<S: Into<String>>(mut self, value: impl IntoIterator<Item = S>) -> Self {
        self.extra_flags = value.into_iter().map(Into::into).collect();
        self.extra_flags_policy = ExtraFlagsPolicy::PythonCompatible;
        self
    }

    /// Set `extra_flags` and **union** them with the stealth flag set instead
    /// of replacing it — the behaviour users expect.
    ///
    /// ```
    /// use browser_cdp::{config::BrowserConfig, flags::launch_options};
    ///
    /// let fixed = BrowserConfig::stealthy().extra_flags_union(["--foo"]).validate()?;
    /// let opts = launch_options(&fixed).expect("launching, not connecting");
    /// assert!(opts.args.contains("--foo"));
    /// assert!(opts.args.contains("--disable-blink-features=AutomationControlled"));
    /// # Ok::<(), browser_cdp::error::ConfigError>(())
    /// ```
    #[must_use]
    pub fn extra_flags_union<S: Into<String>>(
        mut self,
        value: impl IntoIterator<Item = S>,
    ) -> Self {
        self.extra_flags = value.into_iter().map(Into::into).collect();
        self.extra_flags_policy = ExtraFlagsPolicy::Union;
        self
    }

    /// Set `blocked_domains`.
    #[must_use]
    pub fn blocked_domains<S: Into<String>>(mut self, value: impl IntoIterator<Item = S>) -> Self {
        self.blocked_domains = Some(value.into_iter().map(Into::into).collect());
        self
    }

    /// Set `block_ads`.
    #[must_use]
    pub const fn block_ads(mut self, value: bool) -> Self {
        self.block_ads = value;
        self
    }

    /// Set `retries` (validated to `1..=10`).
    #[must_use]
    pub const fn retries(mut self, value: u32) -> Self {
        self.retries = value;
        self
    }

    /// Set `retry_delay`, in **seconds**.
    #[must_use]
    pub const fn retry_delay_secs(mut self, value: f64) -> Self {
        self.retry_delay_secs = value;
        self
    }

    /// Set `capture_xhr`.
    #[must_use]
    pub fn capture_xhr(mut self, value: impl Into<String>) -> Self {
        self.capture_xhr = Some(value.into());
        self
    }

    /// Set `executable_path` (must be an existing, absolute file).
    #[must_use]
    pub fn executable_path(mut self, value: impl Into<PathBuf>) -> Self {
        self.executable_path = Some(value.into());
        self
    }

    /// Set `dns_over_https`.
    #[must_use]
    pub const fn dns_over_https(mut self, value: bool) -> Self {
        self.dns_over_https = value;
        self
    }

    /// Attach (or replace) the stealth extras, selecting the stealthy engine.
    #[must_use]
    pub const fn stealth(mut self, value: StealthExtras) -> Self {
        self.stealth = Some(value);
        self
    }

    /// Whether this config selects the stealthy engine.
    #[must_use]
    pub const fn is_stealthy(&self) -> bool {
        self.stealth.is_some()
    }

    /// Whether the session will connect over CDP rather than launch a browser.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        self.cdp_url.is_some()
    }

    /// Which of the three mutually exclusive launch modes this config selects.
    #[must_use]
    pub fn launch_mode(&self) -> LaunchMode {
        if self.cdp_url.is_some() {
            LaunchMode::Cdp
        } else if self.proxy_rotator.is_some() {
            LaunchMode::Rotation
        } else {
            LaunchMode::Persistent
        }
    }

    /// Lower-cased keys of [`BrowserConfig::extra_headers`].
    ///
    /// Python caches this as `self._headers_keys` in `__validate_routine__`
    /// purely so the `referer` test in `fetch()` is a set lookup.
    #[must_use]
    pub fn header_keys(&self) -> HashSet<String> {
        self.extra_headers
            .iter()
            .flat_map(|headers| headers.keys())
            .map(|key| key.to_lowercase())
            .collect()
    }

    /// Run the range constraints and the whole `__post_init__` chain.
    ///
    /// Order matters and is Python's: ranges first (declaration order), then
    /// the proxy/rotator conflict, proxy normalisation, CDP URL validation,
    /// falsy-value normalisation, the two file checks, the `block_ads` union
    /// and finally the stealth `solve_cloudflare` timeout floor.
    ///
    /// # Errors
    ///
    /// [`ConfigError`], whose `Display` is the Python message verbatim.
    pub fn validate(mut self) -> Result<ValidatedConfig, ConfigError> {
        // ---- msgspec range constraints, in struct declaration order --------
        check_int_range("max_pages", self.max_pages, &MAX_PAGES_RANGE)?;
        check_non_negative("wait", self.wait_ms)?;
        check_non_negative("timeout", self.timeout_ms)?;
        check_int_range("retries", self.retries, &RETRIES_RANGE)?;
        check_non_negative("retry_delay", self.retry_delay_secs)?;

        // ---- PlaywrightConfig.__post_init__ --------------------------------
        // 1. page_action / page_setup callability: not expressible in Rust.
        // 2. proxy + proxy_rotator conflict. Python guards this with
        //    `if self.proxy and self.proxy_rotator`, so a falsy proxy — the
        //    empty string — conflicts with nothing.
        if self.proxy.as_ref().is_some_and(proxy_is_truthy) && self.proxy_rotator.is_some() {
            return Err(ConfigError::ProxyAndRotator);
        }
        // 3. normalise the static proxy, again only `if self.proxy`. An empty
        //    proxy string is therefore left exactly as given rather than being
        //    rejected as an invalid proxy string.
        if self.proxy.as_ref().is_some_and(proxy_is_truthy) {
            let proxy = self
                .proxy
                .take()
                .unwrap_or_else(|| Proxy::Url(String::new()));
            self.proxy = Some(construct_proxy(&proxy)?);
        }
        // 4. CDP URL.
        if let Some(cdp_url) = self.cdp_url.as_deref() {
            validate_cdp_url(cdp_url)?;
        }
        // 5. falsy normalisation. `cookies`, `extra_flags`, `selector_config`
        //    and `additional_args` are already non-optional here, so only
        //    `capture_xhr`'s empty-string-means-None rule needs doing.
        if self.capture_xhr.as_deref().is_some_and(str::is_empty) {
            self.capture_xhr = None;
        }
        // 6 & 7. file checks.
        if let Some(path) = self.init_script.as_deref() {
            check_file_path(path, INIT_SCRIPT_LABEL)?;
        }
        if let Some(path) = self.executable_path.as_deref() {
            check_file_path(path, EXECUTABLE_LABEL)?;
        }
        // 8. block_ads union. Python builds a brand-new set, never mutating the
        //    caller's, and leaves `block_ads` itself set.
        if self.block_ads {
            let mut merged = self.blocked_domains.take().unwrap_or_default();
            merged.extend(AD_DOMAINS.iter().map(|domain| (*domain).to_owned()));
            self.blocked_domains = Some(merged);
        }

        // ---- StealthConfig.__post_init__ -----------------------------------
        if let Some(stealth) = self.stealth {
            if stealth.solve_cloudflare && self.timeout_ms < CLOUDFLARE_MIN_TIMEOUT_MS {
                self.timeout_ms = CLOUDFLARE_MIN_TIMEOUT_MS;
            }
        }

        Ok(ValidatedConfig(self))
    }
}

/// The three mutually exclusive ways a session gets a browser (`browser.md` §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaunchMode {
    /// `cdp_url` is set: connect over CDP, one shared context unless a rotator
    /// is attached.
    Cdp,
    /// A rotator is attached: launch a browser with **no** shared context and
    /// open a fresh one per request.
    Rotation,
    /// The default: `launch_persistent_context`.
    Persistent,
}

/// A [`BrowserConfig`] that has been through [`BrowserConfig::validate`].
///
/// Deref-ing gives read access to every field; the wrapper exists so a function
/// can *require* that the `__post_init__` chain already ran. There is no
/// `DerefMut`: mutating a validated config would let the invariants rot.
#[derive(Clone, Debug)]
pub struct ValidatedConfig(BrowserConfig);

impl ValidatedConfig {
    /// The wrapped config.
    #[must_use]
    pub fn into_inner(self) -> BrowserConfig {
        self.0
    }

    /// Borrow the wrapped config.
    #[must_use]
    pub const fn as_config(&self) -> &BrowserConfig {
        &self.0
    }
}

impl std::ops::Deref for ValidatedConfig {
    type Target = BrowserConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<BrowserConfig> for ValidatedConfig {
    fn as_ref(&self) -> &BrowserConfig {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Per-request parameters (`_fetch_params` + `validate_fetch`).
// ---------------------------------------------------------------------------

/// The per-request overrides `fetch()` accepts.
///
/// Every field is an [`Override`] so that "not passed" and "passed as `None`"
/// stay distinguishable, exactly as Python's `key in kwargs` test does.
/// Anything left [`Override::Unset`] is inherited from the session config by
/// [`FetchParams::resolve`].
///
/// `page_action` / `page_setup` are session-level in this port (they need the
/// live page handle) and so are absent here.
#[derive(Clone, Debug, Default)]
pub struct FetchParams {
    /// Override `google_search`.
    pub google_search: Override<bool>,
    /// Override `timeout`, in milliseconds.
    pub timeout_ms: Override<f64>,
    /// Override `wait`, in milliseconds.
    pub wait_ms: Override<f64>,
    /// Override `extra_headers`.
    pub extra_headers: Override<Option<IndexMap<String, String>>>,
    /// Override `disable_resources`.
    pub disable_resources: Override<bool>,
    /// Override `wait_selector`.
    pub wait_selector: Override<Option<String>>,
    /// Override `wait_selector_state`.
    pub wait_selector_state: Override<WaitState>,
    /// Override `network_idle`.
    pub network_idle: Override<bool>,
    /// Override `load_dom`.
    pub load_dom: Override<bool>,
    /// Override `blocked_domains`.
    ///
    /// **Not** unioned with `AD_DOMAINS`: Python validates per-request
    /// overrides against a throw-away config whose `block_ads` is `False`.
    pub blocked_domains: Override<Option<HashSet<String>>>,
    /// Override `solve_cloudflare`.
    ///
    /// Passing this per request does **not** raise `timeout` to 60 s — the
    /// floor lives in `StealthConfig.__post_init__` and only the
    /// `solve_cloudflare` key is copied out of the throw-away config.
    pub solve_cloudflare: Override<bool>,
    /// Override `selector_config`.
    pub selector_config: Override<serde_json::Map<String, serde_json::Value>>,
    /// A per-request static proxy that overrides both the session proxy and
    /// any rotator.
    ///
    /// Python `pop`s this out of the kwargs before validation, so it never
    /// reaches `_fetch_params`; it is normalised at use time instead.
    pub proxy: Option<Proxy>,
}

/// The fully resolved parameters one `fetch()` call runs with.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedFetchParams {
    /// Whether to send the Google referer.
    pub google_search: bool,
    /// Navigation + default timeout, in milliseconds.
    pub timeout_ms: f64,
    /// Idle time before snapshotting, in milliseconds.
    pub wait_ms: f64,
    /// Per-request extra headers (`None` clears the previous request's).
    pub extra_headers: Option<IndexMap<String, String>>,
    /// Abort `EXTRA_RESOURCES` requests.
    pub disable_resources: bool,
    /// Selector to wait for.
    pub wait_selector: Option<String>,
    /// State that selector must reach.
    pub wait_selector_state: WaitState,
    /// Wait for `networkidle`.
    pub network_idle: bool,
    /// Wait for `domcontentloaded`.
    pub load_dom: bool,
    /// Domains to abort.
    pub blocked_domains: Option<HashSet<String>>,
    /// Run the Cloudflare solver (inert on a non-stealth session).
    pub solve_cloudflare: bool,
    /// Extra kwargs for the response parser.
    pub selector_config: serde_json::Map<String, serde_json::Value>,
    /// The per-request static proxy, already normalised.
    pub proxy: Option<Proxy>,
}

/// The synthetic referer `google_search = true` sends.
pub const GOOGLE_REFERER: &str = "https://www.google.com/";

impl ResolvedFetchParams {
    /// The `referer` argument for `page.goto`.
    ///
    /// `fetch()` computes it as
    /// `"https://www.google.com/" if params.google_search and "referer" not in
    /// request_headers_keys else None` — where `request_headers_keys` are the
    /// lower-cased keys of the **per-request** `extra_headers`, so a caller's
    /// own `Referer` always wins.
    ///
    /// ```
    /// use browser_cdp::config::{BrowserConfig, FetchParams, GOOGLE_REFERER};
    ///
    /// let session = BrowserConfig::default().validate()?;
    /// let params = FetchParams::new().resolve(&session)?;
    /// assert_eq!(params.referer(), Some(GOOGLE_REFERER));
    /// # Ok::<(), browser_cdp::error::ConfigError>(())
    /// ```
    #[must_use]
    pub fn referer(&self) -> Option<&'static str> {
        if !self.google_search {
            return None;
        }
        // Python builds `{k.lower() for k in headers}` and tests membership.
        let has_referer = self
            .extra_headers
            .iter()
            .flat_map(IndexMap::keys)
            .any(|key| key.to_lowercase() == "referer");
        if has_referer {
            None
        } else {
            Some(GOOGLE_REFERER)
        }
    }

    /// Lower-cased keys of [`ResolvedFetchParams::extra_headers`].
    #[must_use]
    pub fn header_keys(&self) -> HashSet<String> {
        self.extra_headers
            .iter()
            .flat_map(IndexMap::keys)
            .map(|key| key.to_lowercase())
            .collect()
    }
}

impl FetchParams {
    /// An empty override set: every parameter inherited from the session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve against a session config, reproducing `validate_fetch`.
    ///
    /// The session's values are the base; only keys the caller actually set
    /// override them, so a validated *default* can never clobber a session
    /// value. `solve_cloudflare` falls back to the session's stealth extras and
    /// then to `False`, which is how a `DynamicSession` — whose
    /// `PlaywrightConfig` has no such field — ends up with `False`.
    ///
    /// # Errors
    ///
    /// The overridden `timeout` / `wait` are re-checked for `>= 0`, and a
    /// per-request `proxy` is normalised, so [`ConfigError`] can surface here.
    pub fn resolve(&self, config: &BrowserConfig) -> Result<ResolvedFetchParams, ConfigError> {
        if let Override::Set(timeout) = self.timeout_ms {
            check_non_negative("timeout", timeout)?;
        }
        if let Override::Set(wait) = self.wait_ms {
            check_non_negative("wait", wait)?;
        }
        // `_page_generator` guards with `if proxy:`, matching `__post_init__`.
        let proxy = match self.proxy.as_ref() {
            Some(proxy) if proxy_is_truthy(proxy) => Some(construct_proxy(proxy)?),
            other => other.cloned(),
        };

        // `validate_fetch` re-runs the *whole* model on the override dict alone,
        // so `StealthConfig.__post_init__`'s Cloudflare floor fires only when
        // `solve_cloudflare` and `timeout` are overridden **together** — the
        // fresh model defaults `solve_cloudflare` to `False` when it is absent,
        // and a bumped timeout is copied back out only when `timeout` was one of
        // the overridden keys. A session-level `solve_cloudflare` therefore does
        // *not* raise a per-request timeout override. Pinned by
        // `tests/fetch_params_golden.rs`.
        let mut timeout_ms = self.timeout_ms.or_default(config.timeout_ms);
        if config.is_stealthy()
            && matches!(self.solve_cloudflare, Override::Set(true))
            && matches!(self.timeout_ms, Override::Set(_))
            && timeout_ms < CLOUDFLARE_MIN_TIMEOUT_MS
        {
            timeout_ms = CLOUDFLARE_MIN_TIMEOUT_MS;
        }

        Ok(ResolvedFetchParams {
            google_search: self.google_search.or_default(config.google_search),
            timeout_ms,
            wait_ms: self.wait_ms.or_default(config.wait_ms),
            extra_headers: self
                .extra_headers
                .clone()
                .or_else_default(|| config.extra_headers.clone()),
            disable_resources: self.disable_resources.or_default(config.disable_resources),
            wait_selector: self
                .wait_selector
                .clone()
                .or_else_default(|| config.wait_selector.clone()),
            wait_selector_state: self
                .wait_selector_state
                .or_default(config.wait_selector_state),
            network_idle: self.network_idle.or_default(config.network_idle),
            load_dom: self.load_dom.or_default(config.load_dom),
            blocked_domains: self
                .blocked_domains
                .clone()
                .or_else_default(|| config.blocked_domains.clone()),
            solve_cloudflare: self
                .solve_cloudflare
                .or_else_default(|| config.stealth.is_some_and(|s| s.solve_cloudflare)),
            selector_config: self
                .selector_config
                .clone()
                .or_else_default(|| config.selector_config.clone()),
            proxy,
        })
    }
}

// ---------------------------------------------------------------------------
// Validation primitives.
// ---------------------------------------------------------------------------

fn check_int_range(
    field: &'static str,
    value: u32,
    range: &std::ops::RangeInclusive<u32>,
) -> Result<(), ConfigError> {
    if value < *range.start() {
        return Err(ConfigError::IntTooSmall {
            field,
            min: i64::from(*range.start()),
        });
    }
    if value > *range.end() {
        return Err(ConfigError::IntTooLarge {
            field,
            max: i64::from(*range.end()),
        });
    }
    Ok(())
}

fn check_non_negative(field: &'static str, value: f64) -> Result<(), ConfigError> {
    if value.is_nan() {
        return Err(ConfigError::FloatNotANumber { field, min: 0.0 });
    }
    if value < 0.0 {
        return Err(ConfigError::FloatTooSmall { field, min: 0.0 });
    }
    Ok(())
}

/// `_is_invalid_file_path`: exists, then is-a-file, then is-absolute.
///
/// The order is load bearing — a relative path that does not exist reports
/// "path not found", not "is not an absolute path".
fn check_file_path(path: &Path, label: &'static str) -> Result<(), ConfigError> {
    let rendered = path.display().to_string();
    if !path.exists() {
        return Err(ConfigError::FileNotFound {
            label,
            path: rendered,
        });
    }
    if !path.is_file() {
        return Err(ConfigError::NotAFile {
            label,
            path: rendered,
        });
    }
    if !path.is_absolute() {
        return Err(ConfigError::NotAbsolute {
            label,
            path: rendered,
        });
    }
    Ok(())
}

/// `_is_invalid_cdp_url`: an accepted scheme prefix and a non-empty authority.
///
/// # Errors
///
/// [`ConfigError::CdpUrlScheme`] or [`ConfigError::CdpUrlHostname`].
///
/// ```
/// use browser_cdp::config::validate_cdp_url;
///
/// assert!(validate_cdp_url("ws://localhost:9222").is_ok());
/// assert!(validate_cdp_url("localhost:9222").is_err());
/// assert!(validate_cdp_url("http://").is_err());
/// ```
pub fn validate_cdp_url(cdp_url: &str) -> Result<(), ConfigError> {
    // `str.startswith` runs on the *raw* string, before `urlparse` gets a
    // chance to strip anything, so `" ws://h"` is a scheme error.
    if !CDP_SCHEMES.iter().any(|scheme| cdp_url.starts_with(scheme)) {
        return Err(ConfigError::CdpUrlScheme);
    }
    // `urlparse(cdp_url).netloc` — which deletes tabs and newlines first, and
    // raises on a malformed bracketed authority. That ValueError is not caught
    // anywhere, so it reaches the caller with its own message.
    if urlsplit(cdp_url)?.netloc.is_empty() {
        return Err(ConfigError::CdpUrlHostname);
    }
    Ok(())
}

/// `construct_proxy_dict`: normalise a proxy into Playwright's three-field form.
///
/// A [`Proxy::Url`] is parsed with Python's `urlparse` semantics — the scheme
/// and hostname are lower-cased, userinfo is *not* percent-decoded, and the
/// port is re-rendered from its integer value so `":080"` becomes `":80"`.
/// A [`Proxy::Config`] only has to carry a `server`; a missing `username` or
/// `password` becomes `""`, never `None`, because that is the literal dict
/// Python hands to Playwright.
///
/// # Errors
///
/// [`ConfigError::InvalidProxyString`] for an unsupported scheme or a missing
/// host, [`ConfigError::InvalidProxyFormat`] for an unparsable port, and
/// [`ConfigError::ProxyDictMissingServer`] for an empty `server`.
///
/// ```
/// use browser_cdp::config::construct_proxy;
/// use browser_core::types::Proxy;
///
/// let normalised = construct_proxy(&Proxy::Url("http://u:pw@Host:8080".into()))?;
/// assert_eq!(
///     normalised,
///     Proxy::Config {
///         server: "http://host:8080".into(),
///         username: Some("u".into()),
///         password: Some("pw".into()),
///     }
/// );
/// # Ok::<(), browser_cdp::error::ConfigError>(())
/// ```
pub fn construct_proxy(proxy: &Proxy) -> Result<Proxy, ConfigError> {
    match proxy {
        Proxy::Url(raw) => parse_proxy_url(raw),
        Proxy::Config {
            server,
            username,
            password,
        } => {
            // No emptiness check: `convert({"server": ""}, ProxyDict)` succeeds
            // in Python and the empty string is handed straight to Playwright.
            // [`ConfigError::ProxyDictMissingServer`] covers the *missing* key,
            // which `Proxy::Config` makes unrepresentable.
            Ok(Proxy::Config {
                server: server.clone(),
                username: Some(username.clone().unwrap_or_default()),
                password: Some(password.clone().unwrap_or_default()),
            })
        }
    }
}

/// Python's truthiness for a `ProxyType`: a non-empty string, or any dict.
///
/// `Proxy::Config` always carries a `server` key, so it is a non-empty dict and
/// always truthy; only `Proxy::Url("")` is falsy. Every `if self.proxy:` /
/// `if proxy:` guard in the reference implementation turns on this.
#[must_use]
pub fn proxy_is_truthy(proxy: &Proxy) -> bool {
    match proxy {
        Proxy::Url(raw) => !raw.is_empty(),
        Proxy::Config { .. } => true,
    }
}

/// The `isinstance(proxy_string, str)` branch of `construct_proxy_dict`.
fn parse_proxy_url(raw: &str) -> Result<Proxy, ConfigError> {
    // `urlsplit` raises *before* `construct_proxy_dict`'s `try`, so its
    // ValueErrors keep their own message instead of becoming
    // `InvalidProxyFormat`.
    let parts = urlsplit(raw)?;
    let hostname = parts.hostname.as_deref().unwrap_or_default();
    if !PROXY_SCHEMES.contains(&parts.scheme.as_str()) || hostname.is_empty() {
        return Err(ConfigError::InvalidProxyString);
    }

    let mut server = format!("{}://{hostname}", parts.scheme);
    // `urlparse(...).port` raises for a non-numeric or out-of-range port, and
    // `construct_proxy_dict` turns that into its own message. Port 0 is falsy
    // in Python, so it is never appended.
    if let Some(port) = parts.port.as_deref() {
        // `port.isdigit() and port.isascii()` — deliberately *not*
        // `str::parse`, which would accept a leading `+` and reject nothing
        // else that Python accepts.
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ConfigError::InvalidProxyFormat);
        }
        let parsed: u64 = port.parse().map_err(|_| ConfigError::InvalidProxyFormat)?;
        if parsed > 65535 {
            return Err(ConfigError::InvalidProxyFormat);
        }
        if parsed != 0 {
            server.push_str(&format!(":{parsed}"));
        }
    }

    Ok(Proxy::Config {
        server,
        username: Some(parts.username.unwrap_or_default()),
        password: Some(parts.password.unwrap_or_default()),
    })
}

/// CPython's `_WHATWG_C0_CONTROL_OR_SPACE`: `U+0000..=U+001F` plus a space.
///
/// `urlsplit` **left**-strips these; the trailing side is deliberately kept
/// ("some applications rely on preserving trailing space"), which is why
/// `"  http://h:1  "` fails on its port while `"  http://h:1"` parses.
const C0_CONTROL_OR_SPACE: fn(char) -> bool = |c| c <= '\u{1f}' || c == ' ';

/// CPython's `_UNSAFE_URL_BYTES_TO_REMOVE`: deleted from anywhere in the input.
const UNSAFE_URL_CHARS: [char; 3] = ['\t', '\r', '\n'];

/// The pieces of `urllib.parse.urlsplit` that `construct_proxy_dict` and
/// `create_intercept_handler` read.
#[derive(Debug)]
pub(crate) struct UrlPieces {
    pub(crate) scheme: String,
    /// The authority, exactly as `SplitResult.netloc` reports it: not
    /// lower-cased, userinfo and port still attached, `""` when the URL has no
    /// `//`.
    pub(crate) netloc: String,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    /// Lower-cased and with IPv6 brackets stripped, like `SplitResult.hostname`.
    pub(crate) hostname: Option<String>,
    pub(crate) port: Option<String>,
}

/// A literal port of CPython's `urlsplit` + `SplitResult._userinfo` /
/// `_hostinfo`, restricted to what proxy strings need.
///
/// Using the `url` crate here would be wrong twice over: it rejects
/// authority-less inputs that Python happily parses, and it does not lower-case
/// the host of non-special schemes such as `socks5`.
pub(crate) fn urlsplit(raw: &str) -> Result<UrlPieces, ConfigError> {
    // `_urlsplit`, lines 1-3: left-strip C0/space, then delete tab/CR/LF from
    // anywhere. Both happen before the scheme is looked for, so `"\thttp://h"`
    // and `"ht\ttp://h"` both parse as `http`.
    let cleaned: String = raw
        .trim_start_matches(C0_CONTROL_OR_SPACE)
        .chars()
        .filter(|c| !UNSAFE_URL_CHARS.contains(c))
        .collect();
    let cleaned = cleaned.as_str();

    let mut scheme = String::new();
    let mut rest = cleaned;

    if let Some(colon) = cleaned.find(':') {
        let candidate = &cleaned[..colon];
        let mut chars = candidate.chars();
        let starts_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
        let body_ok = candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if starts_ok && body_ok {
            scheme = candidate.to_ascii_lowercase();
            rest = &cleaned[colon + 1..];
        }
    }

    let netloc = if let Some(after) = rest.strip_prefix("//") {
        let netloc = after.split(['/', '?', '#']).next().unwrap_or_default();
        // `_urlsplit`: unbalanced brackets are rejected outright, and a
        // balanced pair is handed to `_check_bracketed_netloc`.
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err(ConfigError::InvalidIpv6Url);
        }
        if open && close {
            check_bracketed_netloc(netloc)?;
        }
        netloc
    } else {
        ""
    };

    // `_checknetloc(netloc)` — the last thing `_urlsplit` does, after the
    // fragment and query have been split off. `netloc` is `None` (falsy) when
    // the URL had no `//`, which is our `""`.
    check_netloc(netloc)?;

    // `_userinfo`: rpartition('@').
    let (userinfo, hostinfo) = match netloc.rfind('@') {
        Some(idx) => (Some(&netloc[..idx]), &netloc[idx + 1..]),
        None => (None, netloc),
    };
    let (username, password) = match userinfo {
        Some(info) => match info.find(':') {
            Some(idx) => (
                Some(info[..idx].to_owned()),
                Some(info[idx + 1..].to_owned()),
            ),
            None => (Some(info.to_owned()), None),
        },
        None => (None, None),
    };

    // `_hostinfo`: bracketed IPv6 first, then partition(':').
    let (host, port) = if let Some(open) = hostinfo.find('[') {
        let bracketed = &hostinfo[open + 1..];
        match bracketed.find(']') {
            Some(close) => {
                let after = &bracketed[close + 1..];
                let port = after.find(':').map(|idx| after[idx + 1..].to_owned());
                (bracketed[..close].to_owned(), port)
            }
            None => (bracketed.to_owned(), None),
        }
    } else {
        match hostinfo.find(':') {
            Some(idx) => (
                hostinfo[..idx].to_owned(),
                Some(hostinfo[idx + 1..].to_owned()),
            ),
            None => (hostinfo.to_owned(), None),
        }
    };

    Ok(UrlPieces {
        scheme,
        netloc: netloc.to_owned(),
        username,
        password,
        hostname: if host.is_empty() {
            None
        } else {
            Some(lower_host(&host))
        },
        // Python maps an empty port string to `None`.
        port: port.filter(|value| !value.is_empty()),
    })
}

/// `SplitResult.hostname`: lower-case everything up to the first `%`, and leave
/// an IPv6 zone identifier alone (`[fe80::1%tESt]` keeps `tESt`).
fn lower_host(host: &str) -> String {
    match host.find('%') {
        Some(idx) => format!("{}{}", host[..idx].to_lowercase(), &host[idx..]),
        None => host.to_lowercase(),
    }
}

/// CPython's `_checknetloc`.
///
/// ```python
/// def _checknetloc(netloc):
///     if not netloc or netloc.isascii():
///         return
///     n = netloc.replace('@', '').replace(':', '').replace('#', '').replace('?', '')
///     netloc2 = unicodedata.normalize('NFKC', n)
///     if n == netloc2:
///         return
///     for c in '/?#@:':
///         if c in netloc2:
///             raise ValueError(f"netloc {netloc!r} contains invalid characters "
///                              "under NFKC normalization")
/// ```
///
/// Rather than take a normalisation crate for a nineteen-character question,
/// [`NFKC_DELIMITER_EXPANSIONS`] enumerates every code point whose NFKC
/// expansion contains one of `/?#@:`. The two formulations agree exactly: a
/// code point can only introduce a delimiter by *decomposing* (NFKC composition
/// only ever produces precomposed letters, never ASCII punctuation), and any
/// such code point also makes `n != netloc2`, which is the guard Python tests
/// first.
///
/// This is the same table `browser-storage`'s `base_url` module carries; the
/// two crates cannot share it without one depending on the other, so the
/// derivation is repeated in both doc comments instead.
fn check_netloc(netloc: &str) -> Result<(), ConfigError> {
    if netloc.is_ascii() {
        return Ok(());
    }
    if netloc
        .chars()
        .any(|c| NFKC_DELIMITER_EXPANSIONS.contains(&c))
    {
        return Err(ConfigError::NetlocNfkc {
            netloc: netloc.to_owned(),
        });
    }
    Ok(())
}

/// Every code point whose NFKC expansion contains `/`, `?`, `#`, `@` or `:`.
///
/// Enumerated over the whole of Unicode 16.0.0 (the version CPython 3.14
/// ships); there are exactly nineteen. Re-derive with:
///
/// ```python
/// [chr(c) for c in range(0x80, 0x110000)
///  if not unicodedata.is_normalized("NFKC", chr(c))
///  and any(d in unicodedata.normalize("NFKC", chr(c)) for d in "/?#@:")]
/// ```
const NFKC_DELIMITER_EXPANSIONS: [char; 19] = [
    '\u{2047}', // ⁇  DOUBLE QUESTION MARK       -> "??"
    '\u{2048}', // ⁈  QUESTION EXCLAMATION MARK  -> "?!"
    '\u{2049}', // ⁉  EXCLAMATION QUESTION MARK  -> "!?"
    '\u{2100}', // ℀  ACCOUNT OF                 -> "a/c"
    '\u{2101}', // ℁  ADDRESSED TO THE SUBJECT   -> "a/s"
    '\u{2105}', // ℅  CARE OF                    -> "c/o"
    '\u{2106}', // ℆  CADA UNA                   -> "c/u"
    '\u{2a74}', // ⩴  DOUBLE COLON EQUAL         -> "::="
    '\u{fe13}', // ︓ VERTICAL COLON             -> ":"
    '\u{fe16}', // ︖ VERTICAL QUESTION MARK     -> "?"
    '\u{fe55}', // ﹕ SMALL COLON                -> ":"
    '\u{fe56}', // ﹖ SMALL QUESTION MARK        -> "?"
    '\u{fe5f}', // ﹟ SMALL NUMBER SIGN          -> "#"
    '\u{fe6b}', // ﹫ SMALL COMMERCIAL AT        -> "@"
    '\u{ff03}', // ＃ FULLWIDTH NUMBER SIGN      -> "#"
    '\u{ff0f}', // ／ FULLWIDTH SOLIDUS          -> "/"
    '\u{ff1a}', // ： FULLWIDTH COLON            -> ":"
    '\u{ff1f}', // ？ FULLWIDTH QUESTION MARK    -> "?"
    '\u{ff20}', // ＠ FULLWIDTH COMMERCIAL AT    -> "@"
];

/// CPython's `_check_bracketed_netloc`, which "must mirror the splitting done
/// in `NetlocResultMixins._hostinfo()`".
fn check_bracketed_netloc(netloc: &str) -> Result<(), ConfigError> {
    let hostname_and_port = netloc.rsplit('@').next().unwrap_or(netloc);
    let host = if let Some(open) = hostname_and_port.find('[') {
        // No data is allowed before the bracket.
        if open != 0 {
            return Err(ConfigError::InvalidIpv6Url);
        }
        let bracketed = &hostname_and_port[open + 1..];
        let (host, after) = match bracketed.find(']') {
            Some(close) => (&bracketed[..close], &bracketed[close + 1..]),
            None => (bracketed, ""),
        };
        // No data is allowed after the bracket but before the port delimiter.
        if !after.is_empty() && !after.starts_with(':') {
            return Err(ConfigError::InvalidIpv6Url);
        }
        host
    } else {
        // The bracket is in the *userinfo*, not the host — `[::1]:1@h:2`. The
        // host still goes through `_check_bracketed_host`, which is how that
        // URL ends up reported as "'h' does not appear to be an IPv4 or IPv6
        // address" rather than being accepted.
        hostname_and_port
            .split_once(':')
            .map_or(hostname_and_port, |(host, _)| host)
    };
    check_bracketed_host(host)
}

/// CPython's `_check_bracketed_host`.
fn check_bracketed_host(host: &str) -> Result<(), ConfigError> {
    if let Some(rest) = host.strip_prefix('v') {
        // `re.match(r"\Av[a-fA-F0-9]+\..+\z", hostname)`, spelled out: at
        // least one hex digit, then a dot, then at least one more character,
        // and `.` in a Python regex does not match a newline — but `urlsplit`
        // has already deleted every newline by this point.
        let hex_len = rest.chars().take_while(char::is_ascii_hexdigit).count();
        let ok = hex_len > 0
            && rest[hex_len..].starts_with('.')
            && rest[hex_len + 1..].chars().count() >= 1;
        return if ok {
            Ok(())
        } else {
            Err(ConfigError::IpvFutureInvalid)
        };
    }
    // `ipaddress.ip_address(hostname)`, which accepts an IPv6 scope id.
    let (address, zone) = match host.find('%') {
        Some(idx) => (&host[..idx], Some(&host[idx + 1..])),
        None => (host, None),
    };
    // `ipaddress` requires a non-empty scope id that contains no further `%`.
    if zone.is_some_and(|z| z.is_empty() || z.contains('%')) {
        return Err(ConfigError::BracketedHostNotAnIp {
            host: host.to_owned(),
        });
    }
    if address.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(());
    }
    // A plain IPv4 literal parses but is rejected; anything else is not an IP.
    if zone.is_none() && address.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(ConfigError::Ipv4InBrackets);
    }
    Err(ConfigError::BracketedHostNotAnIp {
        host: host.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedRotator;

    impl ProxyRotation for FixedRotator {
        fn next_proxy(&self) -> Proxy {
            Proxy::Url("http://rotated.example:1".into())
        }
        fn len(&self) -> usize {
            1
        }
    }

    fn cfg() -> BrowserConfig {
        BrowserConfig::default()
    }

    #[test]
    fn defaults_match_playwright_config() {
        let c = cfg();
        assert_eq!(c.max_pages, 1);
        assert!(c.headless);
        assert!(!c.disable_resources);
        assert!(!c.network_idle);
        assert!(c.load_dom);
        assert_eq!(c.wait_selector, None);
        assert_eq!(c.wait_selector_state, WaitState::Attached);
        assert!(c.cookies.is_empty());
        assert!(c.google_search);
        assert!((c.wait_ms - 0.0).abs() < f64::EPSILON);
        assert_eq!(c.timezone_id.as_deref(), Some(""));
        assert!(c.proxy.is_none());
        assert!(c.proxy_rotator.is_none());
        assert!(c.extra_headers.is_none());
        assert!((c.timeout_ms - 30_000.0).abs() < f64::EPSILON);
        assert!(c.init_script.is_none());
        assert_eq!(c.user_data_dir, "");
        assert!(c.selector_config.is_empty());
        assert!(c.additional_args.is_empty());
        assert!(c.locale.is_none());
        assert!(!c.real_chrome);
        assert!(c.cdp_url.is_none());
        assert!(c.useragent.is_none());
        assert!(c.extra_flags.is_empty());
        assert!(c.blocked_domains.is_none());
        assert!(!c.block_ads);
        assert_eq!(c.retries, 3);
        assert!((c.retry_delay_secs - 1.0).abs() < f64::EPSILON);
        assert!(c.capture_xhr.is_none());
        assert!(c.executable_path.is_none());
        assert!(!c.dns_over_https);
        assert!(c.stealth.is_none());
    }

    #[test]
    fn stealth_defaults_match_stealth_config() {
        let extras = StealthExtras::default();
        assert!(extras.allow_webgl);
        assert!(!extras.hide_canvas);
        assert!(!extras.block_webrtc);
        assert!(!extras.solve_cloudflare);
        assert!(BrowserConfig::stealthy().is_stealthy());
    }

    #[test]
    fn max_pages_and_retries_are_range_checked() {
        assert_eq!(
            cfg().max_pages(0).validate().unwrap_err().to_string(),
            "Expected `int` >= 1 - at `$.max_pages`"
        );
        assert_eq!(
            cfg().max_pages(51).validate().unwrap_err().to_string(),
            "Expected `int` <= 50 - at `$.max_pages`"
        );
        assert!(cfg().max_pages(1).validate().is_ok());
        assert!(cfg().max_pages(50).validate().is_ok());
        assert_eq!(
            cfg().retries(0).validate().unwrap_err().to_string(),
            "Expected `int` >= 1 - at `$.retries`"
        );
        assert_eq!(
            cfg().retries(11).validate().unwrap_err().to_string(),
            "Expected `int` <= 10 - at `$.retries`"
        );
    }

    #[test]
    fn seconds_fields_reject_negatives_and_nan() {
        assert_eq!(
            cfg().timeout_ms(-1.0).validate().unwrap_err().to_string(),
            "Expected `float` >= 0.0 - at `$.timeout`"
        );
        assert_eq!(
            cfg().wait_ms(-1.0).validate().unwrap_err().to_string(),
            "Expected `float` >= 0.0 - at `$.wait`"
        );
        assert_eq!(
            cfg()
                .retry_delay_secs(-0.5)
                .validate()
                .unwrap_err()
                .to_string(),
            "Expected `float` >= 0.0 - at `$.retry_delay`"
        );
        assert_eq!(
            cfg().timeout_ms(f64::NAN).validate().unwrap_err(),
            ConfigError::FloatNotANumber {
                field: "timeout",
                min: 0.0
            }
        );
        assert!(cfg().timeout_ms(0.0).validate().is_ok());
    }

    #[test]
    fn proxy_and_rotator_conflict() {
        let err = cfg()
            .proxy(Proxy::Url("http://a".into()))
            .proxy_rotator(Arc::new(FixedRotator))
            .validate()
            .unwrap_err();
        assert_eq!(err, ConfigError::ProxyAndRotator);
        // Either alone is fine.
        assert!(cfg()
            .proxy(Proxy::Url("http://a".into()))
            .validate()
            .is_ok());
        assert!(cfg()
            .proxy_rotator(Arc::new(FixedRotator))
            .validate()
            .is_ok());
    }

    #[test]
    fn cdp_url_validation_matches_python() {
        for good in [
            "ws://localhost:9222",
            "wss://host/path",
            "http://127.0.0.1:9222",
            "https://example.com",
        ] {
            assert!(validate_cdp_url(good).is_ok(), "{good} should be accepted");
        }
        assert_eq!(
            validate_cdp_url("blahblah").unwrap_err(),
            ConfigError::CdpUrlScheme
        );
        assert_eq!(
            validate_cdp_url("ftp://x").unwrap_err(),
            ConfigError::CdpUrlScheme
        );
        assert_eq!(
            validate_cdp_url("localhost:9222").unwrap_err(),
            ConfigError::CdpUrlScheme
        );
        assert_eq!(
            validate_cdp_url("http://").unwrap_err(),
            ConfigError::CdpUrlHostname
        );
        assert_eq!(
            cfg()
                .cdp_url("blahblah")
                .validate()
                .unwrap_err()
                .to_string(),
            "CDP URL must use 'ws://', 'wss://', 'http://', or 'https://' scheme"
        );
    }

    #[test]
    fn proxy_strings_normalise_like_construct_proxy_dict() {
        let cases = [
            (
                "http://p.example.com:8080",
                ("http://p.example.com:8080", "", ""),
            ),
            (
                "http://u:pw@p.example.com:8080",
                ("http://p.example.com:8080", "u", "pw"),
            ),
            ("socks5://h", ("socks5://h", "", "")),
            ("HTTP://Host:1", ("http://host:1", "", "")),
            (
                "socks4://SOCKS.example:1080",
                ("socks4://socks.example:1080", "", ""),
            ),
            ("http://h:080", ("http://h:80", "", "")),
            ("http://h:0", ("http://h", "", "")),
            ("http://user@h", ("http://h", "user", "")),
            ("http://h/path?q#f", ("http://h", "", "")),
        ];
        for (raw, (server, username, password)) in cases {
            let got = construct_proxy(&Proxy::Url(raw.into()))
                .unwrap_or_else(|e| panic!("{raw} failed: {e}"));
            assert_eq!(
                got,
                Proxy::Config {
                    server: server.into(),
                    username: Some(username.into()),
                    password: Some(password.into()),
                },
                "{raw}"
            );
        }
    }

    #[test]
    fn bad_proxy_strings_are_rejected() {
        for raw in ["bad", "http://:8080", "ftp://h", "", "//h"] {
            assert_eq!(
                construct_proxy(&Proxy::Url(raw.into())).unwrap_err(),
                ConfigError::InvalidProxyString,
                "{raw}"
            );
        }
        assert_eq!(
            construct_proxy(&Proxy::Url("http://h:notaport".into())).unwrap_err(),
            ConfigError::InvalidProxyFormat
        );
        assert_eq!(
            construct_proxy(&Proxy::Url("http://h:99999".into())).unwrap_err(),
            ConfigError::InvalidProxyFormat
        );
    }

    #[test]
    fn proxy_dicts_fill_missing_credentials_with_empty_strings() {
        let got = construct_proxy(&Proxy::Config {
            server: "http://x:1".into(),
            username: None,
            password: None,
        })
        .expect("a server key is all Playwright needs");
        assert_eq!(
            got,
            Proxy::Config {
                server: "http://x:1".into(),
                username: Some(String::new()),
                password: Some(String::new()),
            }
        );
        // An *empty* server is accepted: `convert({"server": ""}, ProxyDict)`
        // succeeds in Python and the empty string reaches Playwright as-is.
        // Only a *missing* key raises, and `Proxy::Config` cannot express that.
        assert_eq!(
            construct_proxy(&Proxy::Config {
                server: String::new(),
                username: None,
                password: None,
            })
            .expect("an empty server is not a validation error"),
            Proxy::Config {
                server: String::new(),
                username: Some(String::new()),
                password: Some(String::new()),
            }
        );
    }

    #[test]
    fn validate_normalises_the_static_proxy_in_place() {
        let validated = cfg()
            .proxy(Proxy::Url("http://u:p@Host:8080".into()))
            .validate()
            .expect("valid proxy");
        assert_eq!(
            validated.proxy,
            Some(Proxy::Config {
                server: "http://host:8080".into(),
                username: Some("u".into()),
                password: Some("p".into()),
            })
        );
    }

    #[test]
    fn file_checks_report_python_messages_in_python_order() {
        let missing = cfg()
            .init_script("/nonexistent/x.js")
            .validate()
            .unwrap_err();
        assert_eq!(
            missing.to_string(),
            "Init script path not found: /nonexistent/x.js"
        );

        // A relative path that does not exist still reports "not found" first.
        let relative_missing = cfg().init_script("relative.js").validate().unwrap_err();
        assert_eq!(
            relative_missing.to_string(),
            "Init script path not found: relative.js"
        );

        let dir = std::env::temp_dir();
        let not_a_file = cfg().executable_path(&dir).validate().unwrap_err();
        assert_eq!(
            not_a_file.to_string(),
            format!("Browser executable is not a file: {}", dir.display())
        );

        let script = dir.join("browser-cdp-unit13-init.js");
        std::fs::write(&script, b"// noop\n").expect("temp dir is writable");
        assert!(cfg().init_script(&script).validate().is_ok());

        // Relative but existing: Python reports "is not an absolute path".
        let cwd = std::env::current_dir().expect("a cwd");
        let relative = pathdiff(&script, &cwd);
        if let Some(relative) = relative {
            let err = cfg().init_script(&relative).validate().unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "Init script is not an absolute path: {}",
                    relative.display()
                )
            );
        }
        let _ = std::fs::remove_file(&script);
    }

    /// Cheap `Path::relative_to`, enough for the one test that needs it.
    fn pathdiff(target: &Path, base: &Path) -> Option<PathBuf> {
        let target = target.canonicalize().ok()?;
        let base = base.canonicalize().ok()?;
        let stripped = target.strip_prefix(&base).ok()?;
        Some(stripped.to_path_buf())
    }

    #[test]
    fn block_ads_unions_the_ad_list_without_touching_the_callers_set() {
        let user: HashSet<String> = ["mine.example".to_owned()].into_iter().collect();
        let validated = cfg()
            .blocked_domains(user.iter().cloned())
            .block_ads(true)
            .validate()
            .expect("valid");
        let merged = validated.blocked_domains.as_ref().expect("merged set");
        assert!(merged.contains("mine.example"));
        assert!(merged.contains("doubleclick.net"));
        assert!(merged.contains("googlesyndication.com"));
        assert!(merged.len() > 1000);
        assert_eq!(user.len(), 1, "the caller's set must be untouched");

        // Without block_ads the set is exactly what the caller gave.
        let plain = cfg()
            .blocked_domains(["mine.example"])
            .validate()
            .expect("valid");
        assert_eq!(plain.blocked_domains.as_ref().map(HashSet::len), Some(1));

        // block_ads with no user domains still fills the set.
        let ads_only = cfg().block_ads(true).validate().expect("valid");
        assert!(ads_only
            .blocked_domains
            .as_ref()
            .is_some_and(|d| d.contains("doubleclick.net")));

        // blocked_domains defaults to None, and is inherited by stealth configs.
        assert!(BrowserConfig::stealthy()
            .validate()
            .expect("valid")
            .blocked_domains
            .is_none());
    }

    #[test]
    fn solve_cloudflare_raises_the_timeout_floor_at_construction() {
        let bumped = BrowserConfig::stealthy()
            .stealth(StealthExtras::default().solve_cloudflare(true))
            .timeout_ms(10_000.0)
            .validate()
            .expect("valid");
        assert!((bumped.timeout_ms - 60_000.0).abs() < f64::EPSILON);

        let defaulted = BrowserConfig::stealthy()
            .stealth(StealthExtras::default().solve_cloudflare(true))
            .validate()
            .expect("valid");
        assert!((defaulted.timeout_ms - 60_000.0).abs() < f64::EPSILON);

        let higher = BrowserConfig::stealthy()
            .stealth(StealthExtras::default().solve_cloudflare(true))
            .timeout_ms(90_000.0)
            .validate()
            .expect("valid");
        assert!((higher.timeout_ms - 90_000.0).abs() < f64::EPSILON);

        // Off, or non-stealth: untouched.
        let off = BrowserConfig::stealthy()
            .timeout_ms(10_000.0)
            .validate()
            .expect("valid");
        assert!((off.timeout_ms - 10_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_capture_xhr_becomes_none() {
        let validated = cfg().capture_xhr("").validate().expect("valid");
        assert!(validated.capture_xhr.is_none());
        let kept = cfg().capture_xhr("api/.*").validate().expect("valid");
        assert_eq!(kept.capture_xhr.as_deref(), Some("api/.*"));
    }

    #[test]
    fn launch_mode_picks_the_python_branch() {
        assert_eq!(cfg().launch_mode(), LaunchMode::Persistent);
        assert_eq!(
            cfg().proxy_rotator(Arc::new(FixedRotator)).launch_mode(),
            LaunchMode::Rotation
        );
        assert_eq!(cfg().cdp_url("ws://h:1").launch_mode(), LaunchMode::Cdp);
        // CDP wins over rotation, as in `start()`.
        assert_eq!(
            cfg()
                .cdp_url("ws://h:1")
                .proxy_rotator(Arc::new(FixedRotator))
                .launch_mode(),
            LaunchMode::Cdp
        );
    }

    #[test]
    fn header_keys_are_lowercased() {
        let c = cfg().extra_headers([("Referer", "https://x/"), ("X-Test", "1")]);
        let keys = c.header_keys();
        assert!(keys.contains("referer"));
        assert!(keys.contains("x-test"));
        assert!(cfg().header_keys().is_empty());
    }

    #[test]
    fn fetch_params_inherit_the_session_and_override_only_what_was_set() {
        let session = cfg()
            .timeout_ms(5_000.0)
            .google_search(false)
            .network_idle(true)
            .blocked_domains(["a.example"])
            .validate()
            .expect("valid");

        let inherited = FetchParams::new().resolve(&session).expect("resolves");
        assert!((inherited.timeout_ms - 5_000.0).abs() < f64::EPSILON);
        assert!(!inherited.google_search);
        assert!(inherited.network_idle);
        assert_eq!(
            inherited.blocked_domains.as_ref().map(HashSet::len),
            Some(1)
        );
        assert!(!inherited.solve_cloudflare);

        let overridden = FetchParams {
            timeout_ms: Override::Set(2_000.0),
            wait_selector: Override::Set(Some("#main".into())),
            ..FetchParams::new()
        }
        .resolve(&session)
        .expect("resolves");
        assert!((overridden.timeout_ms - 2_000.0).abs() < f64::EPSILON);
        assert_eq!(overridden.wait_selector.as_deref(), Some("#main"));
        // Untouched keys still come from the session, not from the defaults.
        assert!(!overridden.google_search);
        assert!(overridden.network_idle);
    }

    #[test]
    fn per_request_overrides_are_range_checked() {
        let session = cfg().validate().expect("valid");
        let err = FetchParams {
            timeout_ms: Override::Set(-1.0),
            ..FetchParams::new()
        }
        .resolve(&session)
        .unwrap_err();
        assert_eq!(err.to_string(), "Expected `float` >= 0.0 - at `$.timeout`");
    }

    #[test]
    fn explicit_none_clears_a_session_value() {
        let session = cfg()
            .wait_selector("#a")
            .extra_headers([("X", "1")])
            .validate()
            .expect("valid");
        let cleared = FetchParams {
            wait_selector: Override::Set(None),
            extra_headers: Override::Set(None),
            ..FetchParams::new()
        }
        .resolve(&session)
        .expect("resolves");
        assert!(cleared.wait_selector.is_none());
        assert!(cleared.extra_headers.is_none());
    }

    #[test]
    fn per_request_solve_cloudflare_does_not_bump_the_timeout() {
        let session = BrowserConfig::stealthy()
            .timeout_ms(10_000.0)
            .validate()
            .expect("valid");
        let resolved = FetchParams {
            solve_cloudflare: Override::Set(true),
            ..FetchParams::new()
        }
        .resolve(&session)
        .expect("resolves");
        assert!(resolved.solve_cloudflare);
        assert!(
            (resolved.timeout_ms - 10_000.0).abs() < f64::EPSILON,
            "the 60s floor lives in __post_init__, not in validate_fetch"
        );
    }

    #[test]
    fn per_request_solve_cloudflare_is_inert_but_preserved_on_a_dynamic_session() {
        let session = cfg().validate().expect("valid");
        let resolved = FetchParams {
            solve_cloudflare: Override::Set(true),
            ..FetchParams::new()
        }
        .resolve(&session)
        .expect("resolves");
        assert!(
            resolved.solve_cloudflare,
            "Python copies the key through even though PlaywrightConfig lacks the field"
        );
    }

    #[test]
    fn per_request_blocked_domains_skip_the_ad_union() {
        let session = cfg().block_ads(true).validate().expect("valid");
        let resolved = FetchParams {
            blocked_domains: Override::Set(Some(["only.example".to_owned()].into_iter().collect())),
            ..FetchParams::new()
        }
        .resolve(&session)
        .expect("resolves");
        assert_eq!(resolved.blocked_domains.as_ref().map(HashSet::len), Some(1));
    }

    #[test]
    fn per_request_proxy_is_normalised() {
        let session = cfg().validate().expect("valid");
        let resolved = FetchParams {
            proxy: Some(Proxy::Url("socks5://Host:1080".into())),
            ..FetchParams::new()
        }
        .resolve(&session)
        .expect("resolves");
        assert_eq!(
            resolved.proxy,
            Some(Proxy::Config {
                server: "socks5://host:1080".into(),
                username: Some(String::new()),
                password: Some(String::new()),
            })
        );
    }

    #[test]
    fn google_referer_is_suppressed_by_an_explicit_referer_header() {
        let session = cfg().validate().expect("valid");
        assert_eq!(
            FetchParams::new().resolve(&session).unwrap().referer(),
            Some(GOOGLE_REFERER)
        );

        // Case-insensitively, and from the per-request headers.
        let with_referer = FetchParams {
            extra_headers: Override::Set(Some(
                [("ReFeReR".to_owned(), "https://x/".to_owned())]
                    .into_iter()
                    .collect(),
            )),
            ..FetchParams::new()
        }
        .resolve(&session)
        .unwrap();
        assert_eq!(with_referer.referer(), None);
        assert!(with_referer.header_keys().contains("referer"));

        // Other headers do not suppress it.
        let other = FetchParams {
            extra_headers: Override::Set(Some(
                [("X-A".to_owned(), "1".to_owned())].into_iter().collect(),
            )),
            ..FetchParams::new()
        }
        .resolve(&session)
        .unwrap();
        assert_eq!(other.referer(), Some(GOOGLE_REFERER));

        // google_search=False means no referer at all.
        let off = cfg().google_search(false).validate().expect("valid");
        assert_eq!(FetchParams::new().resolve(&off).unwrap().referer(), None);
    }

    #[test]
    fn validated_config_derefs_and_unwraps() {
        let validated = cfg().max_pages(7).validate().expect("valid");
        assert_eq!(validated.max_pages, 7);
        assert_eq!(validated.as_config().max_pages, 7);
        assert_eq!(validated.into_inner().max_pages, 7);
    }

    /// CPython's `_checknetloc`, ported in the integration wave to match the
    /// table `browser-storage` already carried. Before this, both crates
    /// disagreed with each other *and* one of them with Python.
    #[test]
    fn a_netloc_that_nfkc_grows_a_delimiter_is_rejected() {
        // U+2100 ACCOUNT OF normalises to "a/c", so CPython refuses to parse
        // the URL at all — the host-spoofing case the check exists for.
        let err = urlsplit("http://\u{2100}.com:1").expect_err("rejected");
        assert_eq!(
            err.to_string(),
            "netloc '\u{2100}.com:1' contains invalid characters under NFKC normalization",
        );
        assert_eq!(err.python_exception(), "TypeError");

        // U+FF20 FULLWIDTH COMMERCIAL AT -> "@": the userinfo-spoofing shape.
        assert!(urlsplit("http://example.com\u{ff20}evil.com:1").is_err());

        // A non-ASCII authority that normalises to itself is fine, and so is
        // one whose NFKC form grows something that is *not* a delimiter.
        assert!(urlsplit("http://\u{fc}n\u{ef}.example:1").is_ok());
        assert!(urlsplit("http://\u{130}stanbul.example:1").is_ok());
        // Plain ASCII never even reaches the table.
        assert!(urlsplit("http://h:1").is_ok());
        // No `//` at all: Python passes `None` to `_checknetloc`, which returns.
        assert!(urlsplit("mailto:a\u{2100}b").is_ok());
    }

    /// The table is derived, not guessed: exactly the code points whose NFKC
    /// expansion contains one of `/?#@:` in Unicode 16.0.0.
    #[test]
    fn the_nfkc_delimiter_table_is_sorted_and_deduplicated() {
        assert_eq!(NFKC_DELIMITER_EXPANSIONS.len(), 19);
        assert!(NFKC_DELIMITER_EXPANSIONS.windows(2).all(|w| w[0] < w[1]));
        assert!(NFKC_DELIMITER_EXPANSIONS.iter().all(|c| !c.is_ascii()));
    }
}
