//! Chromium flag assembly with an [`IndexSet`] for deterministic order, plus the
//! documented `extra_flags` quirk. Zero CDP.
//!
//! This is the Rust shape of `BaseSessionMixin.__generate_options__` and
//! `StealthySessionMixin.__generate_stealth_options` — the whole of
//! `browser.md` §3.
//!
//! # Determinism
//!
//! Python computes the launch flags with `list(set(tuple(a) + tuple(b)))`, so
//! the command line it hands Chromium is in **hash order** and differs between
//! interpreter runs. Nothing depends on that order (the Python tests assert
//! membership only), so the port uses an [`IndexSet`]: first `DEFAULT_ARGS` in
//! source order, then `STEALTH_ARGS`, then the conditional stealth flags, then
//! the user's, then `--dns-over-https-templates`, then the locale pair.
//! Duplicates collapse to their first occurrence.
//!
//! # The `extra_flags` quirk
//!
//! `__generate_options__` reads `config.extra_flags or extra_flags`, where the
//! left operand is the user's list and the right one is the stealth flag tuple
//! the stealth mixin just computed. A non-empty user list therefore makes the
//! **entire** `STEALTH_ARGS` set — and the `block_webrtc` / `allow_webgl` /
//! `hide_canvas` flags derived from it — disappear, leaving
//! `DEFAULT_ARGS ∪ user_flags`. That is almost certainly a bug, and it silently
//! un-stealths a stealth session.
//!
//! Per `ARCHITECTURE.md` §7 unit 13 the port ships **both**:
//!
//! * [`ExtraFlagsPolicy::PythonCompatible`] — the default, bug-for-bug.
//!   Selected by [`BrowserConfig::extra_flags`].
//! * [`ExtraFlagsPolicy::Union`] — what users expect. Selected by
//!   [`BrowserConfig::extra_flags_union`].
//!
//! [`BrowserConfig::extra_flags`]: crate::config::BrowserConfig::extra_flags
//! [`BrowserConfig::extra_flags_union`]: crate::config::BrowserConfig::extra_flags_union

use indexmap::{IndexMap, IndexSet};
use browser_core::types::Proxy;
use browser_fingerprint::domains::{DEFAULT_ARGS, HARMFUL_ARGS, STEALTH_ARGS};
use browser_fingerprint::useragent::{default_browser_ua, default_chrome_ua};
use serde_json::{json, Map, Value};

use crate::config::{construct_proxy, BrowserConfig, StealthExtras};
use crate::error::ConfigError;

/// The DNS-over-HTTPS launch flag, added when `dns_over_https = true`.
pub const DNS_OVER_HTTPS_FLAG: &str =
    "--dns-over-https-templates=https://cloudflare-dns.com/dns-query";

/// Flags added when `block_webrtc = true`.
pub const BLOCK_WEBRTC_FLAGS: [&str; 2] = [
    "--webrtc-ip-handling-policy=disable_non_proxied_udp",
    // Ensures the policy is enforced.
    "--force-webrtc-ip-handling-policy",
];

/// Flags added when `allow_webgl = false`.
pub const DISABLE_WEBGL_FLAGS: [&str; 3] = [
    "--disable-webgl",
    "--disable-webgl-image-chromium",
    "--disable-webgl2",
];

/// Flag added when `hide_canvas = true`.
pub const HIDE_CANVAS_FLAG: &str = "--fingerprinting-canvas-image-data-noise";

/// The stealth screen and viewport size, in CSS pixels.
pub const STEALTH_VIEWPORT: Viewport = Viewport {
    width: 1920,
    height: 1080,
};

/// The `device_scale_factor` every session runs with.
pub const DEVICE_SCALE_FACTOR: f64 = 2.0;

/// The `color_scheme` every session runs with.
///
/// Dark deliberately defeats creepjs's `prefersLightColor` check.
pub const COLOR_SCHEME: &str = "dark";

/// Permissions the stealth context grants up front.
pub const STEALTH_PERMISSIONS: [&str; 2] = ["geolocation", "notifications"];

/// How [`BrowserConfig::extra_flags`] combines with the stealth flag set.
///
/// [`BrowserConfig::extra_flags`]: crate::config::BrowserConfig::extra_flags
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExtraFlagsPolicy {
    /// Bug-for-bug with `the reference implementation`: a non-empty user flag list
    /// **replaces** `STEALTH_ARGS` and every conditional stealth flag.
    #[default]
    PythonCompatible,
    /// The fixed behaviour: user flags are unioned on top of everything else.
    Union,
}

/// Chromium's launch channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Playwright's bundled Chromium build (`real_chrome = false`).
    #[default]
    Chromium,
    /// The system Google Chrome install (`real_chrome = true`).
    Chrome,
}

impl Channel {
    /// The string Playwright expects, and which the CLI surfaces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Chrome => "chrome",
        }
    }
}

/// A screen or viewport size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Viewport {
    /// Width in CSS pixels.
    pub width: u32,
    /// Height in CSS pixels.
    pub height: u32,
}

impl Viewport {
    /// `{"width": …, "height": …}`, the shape Playwright and CDP both want.
    #[must_use]
    pub fn to_json(self) -> Value {
        json!({ "width": self.width, "height": self.height })
    }
}

/// Everything `browser_options` carries in `__generate_options__`.
///
/// Python replaces the whole dict with `{}` when `cdp_url` is set — a remote
/// browser is already running, so there is nothing to launch. [`launch_options`]
/// models that as `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchOptions {
    /// The `args` list, deduplicated and in a deterministic order.
    pub args: IndexSet<String>,
    /// `ignore_default_args`: always `HARMFUL_ARGS`.
    pub ignore_default_args: &'static [&'static str],
    /// Launch headless.
    pub headless: bool,
    /// `chromium` or `chrome`.
    pub channel: Channel,
    /// An explicit browser binary, if one was configured.
    pub executable_path: Option<std::path::PathBuf>,
    /// The Chromium profile directory. `""` means "let the driver make a
    /// temporary one and delete it on close".
    pub user_data_dir: String,
}

impl LaunchOptions {
    /// The flags as a `Vec`, ready for a process builder.
    #[must_use]
    pub fn args_vec(&self) -> Vec<String> {
        self.args.iter().cloned().collect()
    }

    /// Whether a flag is present.
    #[must_use]
    pub fn has(&self, flag: &str) -> bool {
        self.args.contains(flag)
    }
}

/// The stealth-only half of the Playwright context options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StealthContextOptions {
    /// Always `false`.
    pub is_mobile: bool,
    /// Always `false`.
    pub has_touch: bool,
    /// Always `"allow"`.
    pub service_workers: &'static str,
    /// Always `true`.
    pub ignore_https_errors: bool,
    /// Always 1920×1080.
    pub screen: Viewport,
    /// Always 1920×1080.
    pub viewport: Viewport,
    /// Always `["geolocation", "notifications"]`.
    pub permissions: [&'static str; 2],
}

impl Default for StealthContextOptions {
    fn default() -> Self {
        Self {
            is_mobile: false,
            has_touch: false,
            service_workers: "allow",
            ignore_https_errors: true,
            screen: STEALTH_VIEWPORT,
            viewport: STEALTH_VIEWPORT,
            permissions: STEALTH_PERMISSIONS,
        }
    }
}

/// Everything `context_options` carries in `__generate_options__`.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextOptions {
    /// Always [`COLOR_SCHEME`].
    pub color_scheme: &'static str,
    /// Always [`DEVICE_SCALE_FACTOR`].
    pub device_scale_factor: f64,
    /// Present only for stealth sessions.
    pub stealth: Option<StealthContextOptions>,
    /// The normalised proxy, or `None`. Python leaves the key present and
    /// `None`, so [`ContextOptions::to_json`] emits `null`.
    pub proxy: Option<Proxy>,
    /// `timezone_id`; `Some("")` by default.
    pub timezone_id: Option<String>,
    /// `extra_http_headers`.
    pub extra_http_headers: Option<IndexMap<String, String>>,
    /// Set **only** when a locale is configured *and* `cdp_url` is set, because
    /// launch flags cannot reach a remote browser. See [`locale_flags`].
    pub locale: Option<String>,
    /// The User-Agent override, per the policy in [`resolve_user_agent`].
    pub user_agent: Option<String>,
    /// `additional_args`, merged last so the caller wins over everything.
    pub additional_args: Map<String, Value>,
}

impl ContextOptions {
    /// The dict Python hands to `new_context` / `launch_persistent_context`,
    /// with the keys in Python's insertion order.
    #[must_use]
    pub fn to_json(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("color_scheme".into(), Value::from(self.color_scheme));
        out.insert(
            "device_scale_factor".into(),
            Value::from(self.device_scale_factor),
        );
        if let Some(stealth) = self.stealth {
            out.insert("is_mobile".into(), Value::from(stealth.is_mobile));
            out.insert("has_touch".into(), Value::from(stealth.has_touch));
            out.insert(
                "service_workers".into(),
                Value::from(stealth.service_workers),
            );
            out.insert(
                "ignore_https_errors".into(),
                Value::from(stealth.ignore_https_errors),
            );
            out.insert("screen".into(), stealth.screen.to_json());
            out.insert("viewport".into(), stealth.viewport.to_json());
            out.insert(
                "permissions".into(),
                Value::from(stealth.permissions.to_vec()),
            );
        }
        out.insert("proxy".into(), proxy_to_json(self.proxy.as_ref()));
        out.insert(
            "timezone_id".into(),
            self.timezone_id
                .as_ref()
                .map_or(Value::Null, |tz| Value::from(tz.clone())),
        );
        out.insert(
            "extra_http_headers".into(),
            self.extra_http_headers.as_ref().map_or(Value::Null, |h| {
                Value::Object(
                    h.iter()
                        .map(|(k, v)| (k.clone(), Value::from(v.clone())))
                        .collect(),
                )
            }),
        );
        if let Some(locale) = self.locale.as_ref() {
            out.insert("locale".into(), Value::from(locale.clone()));
        }
        if let Some(ua) = self.user_agent.as_ref() {
            out.insert("user_agent".into(), Value::from(ua.clone()));
        }
        for (key, value) in &self.additional_args {
            out.insert(key.clone(), value.clone());
        }
        out
    }

    /// `_build_context_with_proxy`: a copy with the proxy replaced.
    ///
    /// A falsy proxy leaves the options alone, exactly as Python's
    /// `if proxy:` guard does.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when `proxy` cannot be normalised.
    pub fn with_proxy(&self, proxy: Option<&Proxy>) -> Result<Self, ConfigError> {
        let mut out = self.clone();
        // `if proxy:` — a falsy proxy (the empty string) leaves the context's
        // own proxy in place, exactly as in `_build_context_with_proxy`.
        if let Some(proxy) = proxy.filter(|p| crate::config::proxy_is_truthy(p)) {
            out.proxy = Some(construct_proxy(proxy)?);
        }
        Ok(out)
    }
}

fn proxy_to_json(proxy: Option<&Proxy>) -> Value {
    match proxy {
        None => Value::Null,
        Some(Proxy::Url(url)) => Value::from(url.clone()),
        Some(Proxy::Config {
            server,
            username,
            password,
        }) => json!({
            "server": server,
            "username": username.clone().unwrap_or_default(),
            "password": password.clone().unwrap_or_default(),
        }),
    }
}

/// The `STEALTH_ARGS` tuple plus the three conditional groups.
///
/// Order is `STEALTH_ARGS`, then WebRTC, then WebGL, then canvas — Python's.
///
/// ```
/// use browser_cdp::{config::StealthExtras, flags::stealth_flags};
///
/// let flags = stealth_flags(&StealthExtras::default().block_webrtc(true));
/// assert!(flags.contains("--disable-blink-features=AutomationControlled"));
/// assert!(flags.contains("--force-webrtc-ip-handling-policy"));
/// assert!(!flags.contains("--disable-webgl"));
/// ```
#[must_use]
pub fn stealth_flags(extras: &StealthExtras) -> IndexSet<String> {
    let mut flags: IndexSet<String> = STEALTH_ARGS.iter().map(|f| (*f).to_owned()).collect();
    if extras.block_webrtc {
        flags.extend(BLOCK_WEBRTC_FLAGS.iter().map(|f| (*f).to_owned()));
    }
    if !extras.allow_webgl {
        flags.extend(DISABLE_WEBGL_FLAGS.iter().map(|f| (*f).to_owned()));
    }
    if extras.hide_canvas {
        flags.insert(HIDE_CANVAS_FLAG.to_owned());
    }
    flags
}

/// The `--lang` / `--accept-lang` pair for a locale.
///
/// The context-level `locale` option only patches the main thread; Web Workers
/// keep the real browser language and WAFs flag the mismatch. Launch flags set
/// it browser-wide, so workers, `Intl` and `Accept-Language` all agree.
///
/// `--accept-lang` gets `"<locale>,<base>"`, where `<base>` is the part before
/// the first `-`, lower-cased — unless that already equals the lower-cased
/// locale, in which case the locale alone is used.
///
/// ```
/// use browser_cdp::flags::locale_flags;
///
/// assert_eq!(locale_flags("fr-FR"), ["--lang=fr-FR".to_owned(), "--accept-lang=fr-FR,fr".to_owned()]);
/// assert_eq!(locale_flags("en-GB"), ["--lang=en-GB".to_owned(), "--accept-lang=en-GB,en".to_owned()]);
/// assert_eq!(locale_flags("fr"), ["--lang=fr".to_owned(), "--accept-lang=fr".to_owned()]);
/// ```
#[must_use]
pub fn locale_flags(locale: &str) -> [String; 2] {
    let base = locale.split('-').next().unwrap_or(locale).to_lowercase();
    let accept = if base == locale.to_lowercase() {
        locale.to_owned()
    } else {
        format!("{locale},{base}")
    };
    [
        format!("--lang={locale}"),
        format!("--accept-lang={accept}"),
    ]
}

/// The User-Agent the context should override, if any.
///
/// * A **non-empty** explicit `useragent` always wins. An empty string is
///   falsy in Python and behaves exactly like `None`.
/// * Headless with no explicit UA gets the generated default — the *chrome*
///   flavour when `real_chrome` is on.
/// * **Headful with no explicit UA gets no override at all**: modern
///   Playwright already reports a correct UA, and overriding it is a tell.
#[must_use]
pub fn resolve_user_agent(config: &BrowserConfig) -> Option<String> {
    // Python's guard is `if config.useragent:`, so an *empty* string is falsy
    // and falls through to the generated default exactly like `None` does.
    if let Some(ua) = config.useragent.as_ref().filter(|ua| !ua.is_empty()) {
        return Some(ua.clone());
    }
    if config.headless {
        return Some(if config.real_chrome {
            default_chrome_ua().to_owned()
        } else {
            default_browser_ua().to_owned()
        });
    }
    None
}

/// Assemble the Chromium launch options, or `None` in CDP mode.
///
/// Reproduces `__generate_options__`'s `if not config.cdp_url:` branch in full:
/// the base flag set (subject to [`ExtraFlagsPolicy`]), the DNS-over-HTTPS flag,
/// the locale pair, `headless`, `channel`, `executable_path` and
/// `_user_data_dir`.
///
/// ```
/// use browser_cdp::{config::BrowserConfig, flags::launch_options};
///
/// let remote = BrowserConfig::default().cdp_url("ws://localhost:9222").validate()?;
/// assert!(launch_options(&remote).is_none(), "nothing is launched in CDP mode");
///
/// let local = BrowserConfig::default().validate()?;
/// let opts = launch_options(&local).expect("a local launch");
/// assert!(opts.has("--no-pings"));
/// # Ok::<(), browser_cdp::error::ConfigError>(())
/// ```
#[must_use]
pub fn launch_options(config: &BrowserConfig) -> Option<LaunchOptions> {
    if config.cdp_url.is_some() {
        return None;
    }

    let mut args: IndexSet<String> = DEFAULT_ARGS.iter().map(|f| (*f).to_owned()).collect();
    let stealth = config
        .stealth
        .as_ref()
        .map(stealth_flags)
        .unwrap_or_default();

    match config.extra_flags_policy {
        ExtraFlagsPolicy::PythonCompatible => {
            // `config.extra_flags or extra_flags`: a non-empty user list wins
            // outright and the stealth set is dropped on the floor.
            if config.extra_flags.is_empty() {
                args.extend(stealth);
            } else {
                args.extend(config.extra_flags.iter().cloned());
            }
        }
        ExtraFlagsPolicy::Union => {
            args.extend(stealth);
            args.extend(config.extra_flags.iter().cloned());
        }
    }

    if config.dns_over_https {
        args.insert(DNS_OVER_HTTPS_FLAG.to_owned());
    }
    if let Some(locale) = config.locale.as_deref() {
        args.extend(locale_flags(locale));
    }

    Some(LaunchOptions {
        args,
        ignore_default_args: HARMFUL_ARGS,
        headless: config.headless,
        channel: if config.real_chrome {
            Channel::Chrome
        } else {
            Channel::Chromium
        },
        executable_path: config.executable_path.clone(),
        user_data_dir: config.user_data_dir.clone(),
    })
}

/// Assemble the Playwright context options.
///
/// Unlike [`launch_options`] this is built in every mode — a CDP session still
/// opens a context. The `locale` key appears **only** when a locale is set and
/// the session is remote, because a local session gets [`locale_flags`] instead.
///
/// ```
/// use browser_cdp::{config::BrowserConfig, flags::context_options};
///
/// let local = BrowserConfig::default().locale("fr-FR").validate()?;
/// assert!(context_options(&local).locale.is_none(), "flags, not a context option");
///
/// let remote = BrowserConfig::default().locale("fr-FR").cdp_url("ws://h:9222").validate()?;
/// assert_eq!(context_options(&remote).locale.as_deref(), Some("fr-FR"));
/// # Ok::<(), browser_cdp::error::ConfigError>(())
/// ```
#[must_use]
pub fn context_options(config: &BrowserConfig) -> ContextOptions {
    ContextOptions {
        color_scheme: COLOR_SCHEME,
        device_scale_factor: DEVICE_SCALE_FACTOR,
        stealth: config
            .stealth
            .as_ref()
            .map(|_| StealthContextOptions::default()),
        proxy: config.proxy.clone(),
        timezone_id: config.timezone_id.clone(),
        extra_http_headers: config.extra_headers.clone(),
        locale: match (config.locale.as_ref(), config.cdp_url.as_ref()) {
            (Some(locale), Some(_)) => Some(locale.clone()),
            _ => None,
        },
        user_agent: resolve_user_agent(config),
        additional_args: config.additional_args.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserConfig;

    fn opts(config: BrowserConfig) -> LaunchOptions {
        launch_options(&config.validate().expect("valid config")).expect("a local launch")
    }

    #[test]
    fn default_args_are_always_present_and_ordered() {
        let launched = opts(BrowserConfig::default());
        let ordered: Vec<&str> = launched.args.iter().map(String::as_str).collect();
        assert_eq!(&ordered[..DEFAULT_ARGS.len()], DEFAULT_ARGS);
        assert!(launched.has("--no-pings"));
        assert_eq!(launched.ignore_default_args, HARMFUL_ARGS);
        assert!(launched
            .ignore_default_args
            .contains(&"--enable-automation"));
        assert!(launched
            .ignore_default_args
            .contains(&"--disable-popup-blocking"));
    }

    #[test]
    fn assembly_is_deterministic_across_runs() {
        let first = opts(
            BrowserConfig::stealthy()
                .locale("de-DE")
                .dns_over_https(true),
        );
        let second = opts(
            BrowserConfig::stealthy()
                .locale("de-DE")
                .dns_over_https(true),
        );
        assert_eq!(first.args_vec(), second.args_vec());
    }

    #[test]
    fn dynamic_sessions_get_no_stealth_args() {
        let launched = opts(BrowserConfig::default());
        assert!(!launched.has("--disable-blink-features=AutomationControlled"));
        assert_eq!(launched.args.len(), DEFAULT_ARGS.len());
    }

    #[test]
    fn stealth_sessions_get_default_plus_stealth_args() {
        let launched = opts(BrowserConfig::stealthy());
        assert!(launched.has("--disable-blink-features=AutomationControlled"));
        assert!(launched.has("--no-pings"));
        assert_eq!(launched.args.len(), DEFAULT_ARGS.len() + STEALTH_ARGS.len());
    }

    #[test]
    fn conditional_stealth_flags_follow_their_switches() {
        let webrtc = opts(
            BrowserConfig::stealthy()
                .stealth(crate::config::StealthExtras::default().block_webrtc(true)),
        );
        assert!(webrtc.has("--webrtc-ip-handling-policy=disable_non_proxied_udp"));
        assert!(webrtc.has("--force-webrtc-ip-handling-policy"));

        let no_webgl = opts(
            BrowserConfig::stealthy()
                .stealth(crate::config::StealthExtras::default().allow_webgl(false)),
        );
        for flag in DISABLE_WEBGL_FLAGS {
            assert!(no_webgl.has(flag), "{flag}");
        }

        let canvas = opts(
            BrowserConfig::stealthy()
                .stealth(crate::config::StealthExtras::default().hide_canvas(true)),
        );
        assert!(canvas.has(HIDE_CANVAS_FLAG));

        let plain = opts(BrowserConfig::stealthy());
        assert!(!plain.has(HIDE_CANVAS_FLAG));
        assert!(!plain.has("--disable-webgl"));
        assert!(!plain.has("--force-webrtc-ip-handling-policy"));
    }

    #[test]
    fn extra_flags_suppress_stealth_args_bug_for_bug() {
        let launched = opts(
            BrowserConfig::stealthy()
                .stealth(crate::config::StealthExtras::default().hide_canvas(true))
                .extra_flags(["--foo", "--bar"]),
        );
        assert!(launched.has("--foo"));
        assert!(launched.has("--bar"));
        assert!(launched.has("--no-pings"), "DEFAULT_ARGS survive");
        assert!(
            !launched.has("--disable-blink-features=AutomationControlled"),
            "Python drops STEALTH_ARGS when extra_flags is non-empty"
        );
        assert!(
            !launched.has(HIDE_CANVAS_FLAG),
            "and the conditional stealth flags with them"
        );
        assert_eq!(launched.args.len(), DEFAULT_ARGS.len() + 2);
    }

    #[test]
    fn extra_flags_union_keeps_everything() {
        let launched = opts(
            BrowserConfig::stealthy()
                .stealth(crate::config::StealthExtras::default().hide_canvas(true))
                .extra_flags_union(["--foo"]),
        );
        assert!(launched.has("--foo"));
        assert!(launched.has("--disable-blink-features=AutomationControlled"));
        assert!(launched.has(HIDE_CANVAS_FLAG));
        assert_eq!(
            launched.args.len(),
            DEFAULT_ARGS.len() + STEALTH_ARGS.len() + 2
        );
    }

    #[test]
    fn extra_flags_on_a_dynamic_session_just_add() {
        let launched = opts(BrowserConfig::default().extra_flags(["--foo"]));
        assert!(launched.has("--foo"));
        assert_eq!(launched.args.len(), DEFAULT_ARGS.len() + 1);
    }

    #[test]
    fn duplicate_flags_collapse() {
        let launched = opts(BrowserConfig::default().extra_flags(["--no-pings", "--no-pings"]));
        assert_eq!(launched.args.len(), DEFAULT_ARGS.len());
    }

    #[test]
    fn dns_over_https_adds_the_cloudflare_template() {
        assert!(opts(BrowserConfig::default().dns_over_https(true)).has(DNS_OVER_HTTPS_FLAG));
        assert!(!opts(BrowserConfig::default()).has(DNS_OVER_HTTPS_FLAG));
        // Both engines, and after the extra-flags quirk has fired.
        assert!(opts(
            BrowserConfig::stealthy()
                .extra_flags(["--foo"])
                .dns_over_https(true)
        )
        .has(DNS_OVER_HTTPS_FLAG));
    }

    #[test]
    fn locale_becomes_launch_flags_unless_remote() {
        let launched = opts(BrowserConfig::default().locale("en-GB"));
        assert!(launched.has("--lang=en-GB"));
        assert!(launched.has("--accept-lang=en-GB,en"));

        let stealthy = opts(BrowserConfig::stealthy().locale("fr-FR"));
        assert!(stealthy.has("--lang=fr-FR"));
        assert!(stealthy.has("--accept-lang=fr-FR,fr"));

        let none = opts(BrowserConfig::default());
        assert!(!none.args.iter().any(|f| f.starts_with("--lang=")));
        assert!(!none.args.iter().any(|f| f.starts_with("--accept-lang=")));

        let remote = BrowserConfig::default()
            .locale("fr-FR")
            .cdp_url("ws://localhost:9222")
            .validate()
            .expect("valid");
        assert!(launch_options(&remote).is_none());
        assert_eq!(context_options(&remote).locale.as_deref(), Some("fr-FR"));
    }

    #[test]
    fn locale_accept_lang_collapses_when_base_equals_locale() {
        assert_eq!(
            locale_flags("fr"),
            ["--lang=fr".to_owned(), "--accept-lang=fr".to_owned()]
        );
        assert_eq!(
            locale_flags("FR"),
            ["--lang=FR".to_owned(), "--accept-lang=FR".to_owned()],
            "base is lower-cased before the comparison, so `FR` collapses too"
        );
        assert_eq!(
            locale_flags("zh-Hans-CN"),
            [
                "--lang=zh-Hans-CN".to_owned(),
                "--accept-lang=zh-Hans-CN,zh".to_owned()
            ]
        );
    }

    #[test]
    fn channel_and_executable_follow_the_config() {
        let chromium = opts(BrowserConfig::default());
        assert_eq!(chromium.channel, Channel::Chromium);
        assert_eq!(chromium.channel.as_str(), "chromium");
        assert!(chromium.executable_path.is_none());
        assert_eq!(chromium.user_data_dir, "");

        let chrome = opts(
            BrowserConfig::default()
                .real_chrome(true)
                .user_data_dir("/tmp/profile"),
        );
        assert_eq!(chrome.channel, Channel::Chrome);
        assert_eq!(chrome.channel.as_str(), "chrome");
        assert_eq!(chrome.user_data_dir, "/tmp/profile");

        let headful = opts(BrowserConfig::default().headless(false));
        assert!(!headful.headless);
    }

    #[test]
    fn user_agent_policy_matches_python() {
        let explicit = BrowserConfig::default().useragent("UA/1.0");
        assert_eq!(resolve_user_agent(&explicit).as_deref(), Some("UA/1.0"));

        // Headful + no useragent: no override at all.
        let headful = BrowserConfig::default().headless(false);
        assert!(resolve_user_agent(&headful).is_none());

        // Headful + explicit useragent: still overridden.
        let headful_explicit = BrowserConfig::default().headless(false).useragent("UA/2");
        assert_eq!(
            resolve_user_agent(&headful_explicit).as_deref(),
            Some("UA/2")
        );

        let headless = BrowserConfig::default();
        assert_eq!(
            resolve_user_agent(&headless).as_deref(),
            Some(default_browser_ua())
        );

        let real_chrome = BrowserConfig::default().real_chrome(true);
        assert_eq!(
            resolve_user_agent(&real_chrome).as_deref(),
            Some(default_chrome_ua())
        );
    }

    #[test]
    fn context_options_carry_the_fixed_and_configured_keys() {
        let config = BrowserConfig::default()
            .timezone_id("Europe/Paris")
            .extra_headers([("X-A", "1")])
            .validate()
            .expect("valid");
        let ctx = context_options(&config);
        assert_eq!(ctx.color_scheme, "dark");
        assert!((ctx.device_scale_factor - 2.0).abs() < f64::EPSILON);
        assert!(ctx.stealth.is_none());

        let json = ctx.to_json();
        let keys: Vec<&str> = json.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "color_scheme",
                "device_scale_factor",
                "proxy",
                "timezone_id",
                "extra_http_headers",
                "user_agent"
            ]
        );
        assert_eq!(json["proxy"], Value::Null);
        assert_eq!(json["timezone_id"], Value::from("Europe/Paris"));
        assert_eq!(json["extra_http_headers"]["X-A"], Value::from("1"));
    }

    #[test]
    fn stealth_context_options_are_inserted_before_the_generated_ones() {
        let config = BrowserConfig::stealthy().validate().expect("valid");
        let ctx = context_options(&config);
        let stealth = ctx.stealth.expect("stealth options");
        assert!(!stealth.is_mobile);
        assert!(!stealth.has_touch);
        assert_eq!(stealth.service_workers, "allow");
        assert!(stealth.ignore_https_errors);
        assert_eq!(stealth.screen, STEALTH_VIEWPORT);
        assert_eq!(stealth.viewport, STEALTH_VIEWPORT);
        assert_eq!(stealth.permissions, ["geolocation", "notifications"]);

        let json = ctx.to_json();
        let keys: Vec<&str> = json.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "color_scheme",
                "device_scale_factor",
                "is_mobile",
                "has_touch",
                "service_workers",
                "ignore_https_errors",
                "screen",
                "viewport",
                "permissions",
                "proxy",
                "timezone_id",
                "extra_http_headers",
                "user_agent"
            ]
        );
        assert_eq!(json["screen"], json!({"width": 1920, "height": 1080}));
    }

    #[test]
    fn additional_args_are_merged_last_and_win() {
        let mut extra = Map::new();
        extra.insert("color_scheme".into(), Value::from("light"));
        extra.insert("custom".into(), Value::from(7));
        let config = BrowserConfig::default()
            .additional_args(extra)
            .validate()
            .expect("valid");
        let json = context_options(&config).to_json();
        assert_eq!(json["color_scheme"], Value::from("light"));
        assert_eq!(json["custom"], Value::from(7));
    }

    #[test]
    fn context_proxy_is_the_normalised_dict() {
        let config = BrowserConfig::default()
            .proxy(Proxy::Url("http://u:p@Host:8080".into()))
            .validate()
            .expect("valid");
        let json = context_options(&config).to_json();
        assert_eq!(
            json["proxy"],
            json!({"server": "http://host:8080", "username": "u", "password": "p"})
        );
    }

    #[test]
    fn build_context_with_proxy_overrides_and_normalises() {
        let config = BrowserConfig::default().validate().expect("valid");
        let base = context_options(&config);
        let rotated = base
            .with_proxy(Some(&Proxy::Url("socks5://Rot:1080".into())))
            .expect("valid proxy");
        assert_eq!(
            rotated.to_json()["proxy"],
            json!({"server": "socks5://rot:1080", "username": "", "password": ""})
        );
        // No proxy leaves the copy untouched.
        assert_eq!(base.with_proxy(None).expect("no-op"), base);
    }
}
