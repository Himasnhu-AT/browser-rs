//! The precomputed header-profile sampler plus the reference implementation's own rules: desktop
//! widening on the HTTP path, the `Chrome/[\d.]+` rewrite and the per-family
//! version floors. Header **order** is preserved.
//!
//! # Why precomputed
//!
//! Python samples headers from `browserforge`, which draws from two Bayesian
//! networks shipped as ~30 MB of data by `apify-fingerprint-datapoints`. There
//! is no Rust port and the dataset carries its own licence, so
//! `ARCHITECTURE.md` §1.8 takes tier 2 of `toolbelt.md` §11.5: an offline
//! script (`tools/gen_profiles.py`) vendors self-consistent, correctly
//! *ordered* header sets into `assets/header_profiles.json`, and we sample
//! from those uniformly at runtime.
//!
//! Everything the reference implementation itself owns is reproduced exactly:
//! [`MINIMUM_VERSION`] (149), [`FIREFOX_MINIMUM_VERSION`] (147),
//! [`EDGE_MINIMUM_VERSION`] (145), the OS → device mapping, the
//! desktop-widening rule on the HTTP path, the
//! `Chrome/[\d.]+` → `Chrome/{driven}.0.0.0` rewrite in browser mode, and the
//! `playwright` → `patchright` fallback order.
//!
//! Critically, this whole module is **off the default path**: with
//! `impersonate` enabled (the reference implementation's default) curl-impersonate supplies a
//! browser-consistent header set and `generate_headers` is never called.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};

use crate::error::FingerprintError;
use crate::impersonate::{BrowserFamily, ImpersonateProfile};

include!(concat!(env!("OUT_DIR"), "/header_profiles.rs"));

/// Minimum Chrome major version to generate (`fingerprints.py::MINIMUM_VERSION`).
pub const MINIMUM_VERSION: u32 = 149;

/// Minimum Firefox major version to generate, on the HTTP path only.
pub const FIREFOX_MINIMUM_VERSION: u32 = 147;

/// Minimum Edge major version to generate, on the HTTP path only.
pub const EDGE_MINIMUM_VERSION: u32 = 145;

// ---------------------------------------------------------------------------
// OS / device vocabulary
// ---------------------------------------------------------------------------

/// A host operating system, matching `fingerprints.py`'s `OSName` literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsName {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    Macos,
    /// Desktop Linux.
    Linux,
    /// Android.
    Android,
    /// Apple iOS / iPadOS.
    Ios,
}

impl OsName {
    /// The lowercase name used in `assets/header_profiles.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }

    /// Whether this OS is one of the three desktop platforms the reference implementation widens
    /// across on the HTTP path.
    #[must_use]
    pub const fn is_desktop(self) -> bool {
        matches!(self, Self::Linux | Self::Macos | Self::Windows)
    }

    /// The device class implied by this OS.
    #[must_use]
    pub const fn device(self) -> Device {
        if self.is_desktop() {
            Device::Desktop
        } else {
            Device::Mobile
        }
    }
}

impl fmt::Display for OsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `fingerprints.py::SUPPORTED_OPERATING_SYSTEMS`, in source order.
pub const SUPPORTED_OPERATING_SYSTEMS: &[OsName] = &[
    OsName::Windows,
    OsName::Macos,
    OsName::Linux,
    OsName::Android,
    OsName::Ios,
];

/// `fingerprints.py::DESKTOP_OS`, in source order.
pub const DESKTOP_OS: &[OsName] = &[OsName::Linux, OsName::Macos, OsName::Windows];

/// The two device classes browserforge distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Device {
    /// A desktop or laptop browser.
    Desktop,
    /// A phone or tablet browser.
    Mobile,
}

impl Device {
    /// The lowercase name used in `assets/header_profiles.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `fingerprints.py::SUPPORTED_DEVICES`.
pub const SUPPORTED_DEVICES: &[Device] = &[Device::Desktop, Device::Mobile];

/// The host OS this process runs on, or `None` when it is not one of the five
/// platforms browserforge knows.
///
/// Python derives this from `platform.system()`; Rust uses `cfg!(target_os)`.
/// `BROWSER_RS_HOST_OS` overrides the detection — set it to `windows`, `macos`,
/// `linux`, `android`, `ios` or `unknown`. The "unknown" branch is the one
/// Python reaches on an unrecognised platform, where every OS and both device
/// classes become fair game.
#[must_use]
pub fn host_os() -> Option<OsName> {
    static DETECTED: LazyLock<Option<OsName>> = LazyLock::new(|| {
        if let Ok(raw) = std::env::var("BROWSER_RS_HOST_OS") {
            return match raw.trim().to_ascii_lowercase().as_str() {
                "windows" => Some(OsName::Windows),
                "macos" | "darwin" => Some(OsName::Macos),
                "linux" => Some(OsName::Linux),
                "android" => Some(OsName::Android),
                "ios" | "ipados" => Some(OsName::Ios),
                _ => None,
            };
        }
        match std::env::consts::OS {
            "windows" => Some(OsName::Windows),
            "macos" => Some(OsName::Macos),
            "linux" => Some(OsName::Linux),
            "android" => Some(OsName::Android),
            "ios" => Some(OsName::Ios),
            _ => None,
        }
    });
    *DETECTED
}

// ---------------------------------------------------------------------------
// Ordered header list
// ---------------------------------------------------------------------------

/// An **ordered** header list.
///
/// Order is the point: anti-bot stacks fingerprint the sequence a browser
/// emits, and `http::HeaderMap` gives no guarantee about iteration order
/// across names. Callers must hand this sequence to the client verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// An empty header list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The first value for `name`, compared ASCII-case-insensitively.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether `name` is present, compared ASCII-case-insensitively.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The `User-Agent` value, if the sampled profile carries one.
    #[must_use]
    pub fn user_agent(&self) -> Option<&str> {
        self.get("User-Agent")
    }

    /// Replace the `User-Agent` value in place, keeping its position.
    ///
    /// Does nothing when the list has no `User-Agent`, mirroring Python's
    /// `if ua := headers.get("User-Agent")` guard.
    pub fn set_user_agent(&mut self, value: impl Into<String>) {
        if let Some(slot) = self
            .0
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case("User-Agent"))
        {
            slot.1 = value.into();
        }
    }

    /// Append a header, preserving order. Does not deduplicate.
    pub fn push(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.push((name.into(), value.into()));
    }

    /// The number of headers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate `(name, value)` pairs in wire order.
    pub fn iter(&self) -> std::slice::Iter<'_, (String, String)> {
        self.0.iter()
    }

    /// Borrow the ordered pairs.
    #[must_use]
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0
    }

    /// Consume into the ordered pairs.
    #[must_use]
    pub fn into_vec(self) -> Vec<(String, String)> {
        self.0
    }
}

impl From<Vec<(String, String)>> for Headers {
    fn from(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }
}

impl<'a> IntoIterator for &'a Headers {
    type Item = &'a (String, String);
    type IntoIter = std::slice::Iter<'a, (String, String)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl IntoIterator for Headers {
    type Item = (String, String);
    type IntoIter = std::vec::IntoIter<(String, String)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

// ---------------------------------------------------------------------------
// The precomputed profile table
// ---------------------------------------------------------------------------

/// One precomputed, self-consistent header set for a concrete browser build.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HeaderProfile {
    /// The browser engine.
    pub family: BrowserFamily,
    /// The major version, used for the version-floor comparison.
    pub major: u32,
    /// The full marketing version (`"150.0.0.0"`, `"18.4"`).
    pub version: String,
    /// The operating system the profile describes.
    pub os: OsName,
    /// The device class the profile describes.
    pub device: Device,
    /// The header set, in the browser's real wire order.
    pub headers: Vec<(String, String)>,
}

impl HeaderProfile {
    /// The profile's headers as an ordered [`Headers`] list.
    #[must_use]
    pub fn headers(&self) -> Headers {
        Headers(self.headers.clone())
    }
}

#[derive(Deserialize)]
struct ProfileDocument {
    profiles: Vec<HeaderProfile>,
}

static PROFILE_JSON: &str = include_str!("../assets/header_profiles.json");

static PROFILES: LazyLock<Vec<HeaderProfile>> = LazyLock::new(|| {
    match serde_json::from_str::<ProfileDocument>(PROFILE_JSON) {
        Ok(doc) => doc.profiles,
        Err(error) => {
            // `build.rs` validates the asset, so this is unreachable short of
            // the asset and this model drifting apart.
            tracing::error!(%error, "built-in header profile table is malformed");
            Vec::new()
        }
    }
});

/// Every precomputed header profile.
///
/// Parsed lazily on first use: a CLI that never fetches pays nothing.
#[must_use]
pub fn profiles() -> &'static [HeaderProfile] {
    &PROFILES
}

/// Validate the built-in profile table, for tests and diagnostics.
///
/// # Errors
///
/// Returns [`FingerprintError::MalformedProfileTable`] when
/// `assets/header_profiles.json` cannot be deserialised into the runtime
/// model.
pub fn validate_profile_table() -> Result<usize, FingerprintError> {
    serde_json::from_str::<ProfileDocument>(PROFILE_JSON)
        .map(|doc| doc.profiles.len())
        .map_err(|error| FingerprintError::MalformedProfileTable {
            reason: error.to_string(),
        })
}

// ---------------------------------------------------------------------------
// The sampler
// ---------------------------------------------------------------------------

/// One entry of a [`HeaderGenerator`]'s browser constraint, mirroring
/// browserforge's `Browser(name, min_version)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserSpec {
    /// The engine to sample.
    pub family: BrowserFamily,
    /// The inclusive major-version floor, or `None` for "any version".
    pub min_version: Option<u32>,
}

impl BrowserSpec {
    /// A spec with a version floor.
    #[must_use]
    pub const fn new(family: BrowserFamily, min_version: u32) -> Self {
        Self {
            family,
            min_version: Some(min_version),
        }
    }

    /// A spec with no version floor.
    #[must_use]
    pub const fn any(family: BrowserFamily) -> Self {
        Self {
            family,
            min_version: None,
        }
    }

    /// The same spec with its floor removed — browserforge's retry step.
    #[must_use]
    pub const fn without_floor(self) -> Self {
        Self::any(self.family)
    }

    fn matches(&self, profile: &HeaderProfile) -> bool {
        profile.family == self.family.header_family()
            && self.min_version.is_none_or(|floor| profile.major >= floor)
    }
}

/// Samples a precomputed header profile under browser/OS/device constraints.
///
/// This is the drop-in for browserforge's `HeaderGenerator`. Locale and
/// HTTP-version constraints are baked into the asset instead of being
/// parameters, because the vendored profiles already carry realistic,
/// family-appropriate `Accept-Language` values in HTTP/2 casing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderGenerator {
    browsers: Vec<BrowserSpec>,
    operating_systems: Vec<OsName>,
    devices: Vec<Device>,
}

impl Default for HeaderGenerator {
    fn default() -> Self {
        Self {
            browsers: vec![BrowserSpec::new(BrowserFamily::Chrome, MINIMUM_VERSION)],
            operating_systems: DESKTOP_OS.to_vec(),
            devices: vec![Device::Desktop],
        }
    }
}

impl HeaderGenerator {
    /// A generator constrained to `browsers`, `operating_systems` and
    /// `devices`. An empty constraint list means "unconstrained", matching
    /// browserforge's behaviour when a dimension is relaxed away.
    #[must_use]
    pub fn new(
        browsers: Vec<BrowserSpec>,
        operating_systems: Vec<OsName>,
        devices: Vec<Device>,
    ) -> Self {
        Self {
            browsers,
            operating_systems,
            devices,
        }
    }

    /// Replace the browser constraint.
    #[must_use]
    pub fn with_browsers(mut self, browsers: Vec<BrowserSpec>) -> Self {
        self.browsers = browsers;
        self
    }

    /// Replace the OS constraint.
    #[must_use]
    pub fn with_operating_systems(mut self, operating_systems: Vec<OsName>) -> Self {
        self.operating_systems = operating_systems;
        self
    }

    /// Replace the device constraint.
    #[must_use]
    pub fn with_devices(mut self, devices: Vec<Device>) -> Self {
        self.devices = devices;
        self
    }

    /// Every profile satisfying the constraints, in table order.
    #[must_use]
    pub fn candidates(&self) -> Vec<&'static HeaderProfile> {
        profiles()
            .iter()
            .filter(|profile| {
                (self.browsers.is_empty() || self.browsers.iter().any(|spec| spec.matches(profile)))
                    && (self.operating_systems.is_empty()
                        || self.operating_systems.contains(&profile.os))
                    && (self.devices.is_empty() || self.devices.contains(&profile.device))
            })
            .collect()
    }

    /// Sample one profile uniformly.
    ///
    /// # Errors
    ///
    /// Returns [`FingerprintError::NoMatchingProfile`] when no vendored
    /// profile satisfies the constraints. This is the analogue of
    /// browserforge's `ValueError`, which `generate_headers` catches.
    pub fn sample(&self) -> Result<&'static HeaderProfile, FingerprintError> {
        let candidates = self.candidates();
        candidates.choose(&mut rand::rng()).copied().ok_or_else(|| {
            FingerprintError::NoMatchingProfile {
                constraints: self.describe(),
            }
        })
    }

    /// Sample one profile's headers uniformly.
    ///
    /// # Errors
    ///
    /// As [`HeaderGenerator::sample`].
    pub fn generate(&self) -> Result<Headers, FingerprintError> {
        self.sample().map(HeaderProfile::headers)
    }

    /// Sample after relaxing constraints in browserforge's own order —
    /// devices, then operating systems, then browsers — until something
    /// matches.
    ///
    /// # Errors
    ///
    /// Returns [`FingerprintError::NoMatchingProfile`] only when the profile
    /// table itself is empty.
    pub fn generate_relaxed(&self) -> Result<Headers, FingerprintError> {
        let mut relaxed = self.clone();
        for _ in 0..3 {
            match relaxed.generate() {
                Ok(headers) => return Ok(headers),
                Err(error) => {
                    if !relaxed.devices.is_empty() {
                        relaxed.devices.clear();
                    } else if !relaxed.operating_systems.is_empty() {
                        relaxed.operating_systems.clear();
                    } else if !relaxed.browsers.is_empty() {
                        relaxed.browsers.clear();
                    } else {
                        return Err(error);
                    }
                    tracing::debug!("relaxed header constraints: {}", relaxed.describe());
                }
            }
        }
        relaxed.generate()
    }

    fn describe(&self) -> String {
        let browsers = if self.browsers.is_empty() {
            "any".to_owned()
        } else {
            self.browsers
                .iter()
                .map(|spec| match spec.min_version {
                    Some(floor) => format!("{}>={floor}", spec.family),
                    None => spec.family.to_string(),
                })
                .collect::<Vec<_>>()
                .join("/")
        };
        let systems = if self.operating_systems.is_empty() {
            "any".to_owned()
        } else {
            self.operating_systems
                .iter()
                .map(|os| os.as_str())
                .collect::<Vec<_>>()
                .join("/")
        };
        let devices = if self.devices.is_empty() {
            "any".to_owned()
        } else {
            self.devices
                .iter()
                .map(|device| device.as_str())
                .collect::<Vec<_>>()
                .join("/")
        };
        format!("browser={browsers} os={systems} device={devices}")
    }
}

// ---------------------------------------------------------------------------
// the reference implementation's `generate_headers`
// ---------------------------------------------------------------------------

/// Which of the reference implementation's two header modes to generate for.
///
/// Python passes `browser_mode: bool | str` and truthy-tests it, so `True` and
/// the string `"chrome"` behave identically; both are kept here because the
/// call sites mean different things (`_config_tools.py` uses the string form
/// to build the "real Chrome" default UA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BrowserMode {
    /// `browser_mode=False`: the static/HTTP engine. Chrome + Firefox + Edge,
    /// desktop OS widened to all three so the real host OS does not leak, and
    /// the sampled `User-Agent` is left alone.
    #[default]
    Http,
    /// `browser_mode=True`: a Playwright/Patchright session. Chrome only, host
    /// OS only, and the `Chrome/…` token is rewritten to the version the
    /// driver actually launches.
    Browser,
    /// `browser_mode="chrome"`: identical to [`BrowserMode::Browser`]; used
    /// for the `real_chrome` default UA.
    Chrome,
}

impl BrowserMode {
    /// Python's truthy test on `browser_mode`.
    #[must_use]
    pub const fn is_browser(self) -> bool {
        matches!(self, Self::Browser | Self::Chrome)
    }
}

/// Generate a header set for `mode`, using the detected host OS and driver.
///
/// This is `fingerprints.py::generate_headers`. See
/// [`generate_headers_for`] for the deterministic, injectable form.
///
/// # Examples
///
/// ```
/// use browser_fingerprint::headers::{generate_headers, BrowserMode};
///
/// let headers = generate_headers(BrowserMode::Http);
/// assert!(headers.user_agent().is_some());
/// ```
#[must_use]
pub fn generate_headers(mode: BrowserMode) -> Headers {
    generate_headers_for(host_os(), mode, driven_browser_version())
}

/// The injectable form of [`generate_headers`].
///
/// `os_name` is `None` for an unrecognised host, which is Python's
/// "allow all five operating systems and both device classes" branch.
/// `driven_major` is the Chromium major version the automation driver ships,
/// and is only consulted in browser mode.
#[must_use]
pub fn generate_headers_for(
    os_name: Option<OsName>,
    mode: BrowserMode,
    driven_major: Option<u32>,
) -> Headers {
    let browser_mode = mode.is_browser();

    // device = "desktop" if desktop OS else "mobile" if known else both
    let devices = match os_name {
        Some(os) if os.is_desktop() => vec![Device::Desktop],
        Some(_) => vec![Device::Mobile],
        None => SUPPORTED_DEVICES.to_vec(),
    };

    // On the HTTP path a desktop host is widened to all three desktop OSes so
    // the headers do not leak which one we are really on.
    let operating_systems = match os_name {
        Some(os) if os.is_desktop() && !browser_mode => DESKTOP_OS.to_vec(),
        Some(os) => vec![os],
        None => SUPPORTED_OPERATING_SYSTEMS.to_vec(),
    };

    let mut browsers = vec![BrowserSpec::new(BrowserFamily::Chrome, MINIMUM_VERSION)];
    if !browser_mode {
        browsers.push(BrowserSpec::new(
            BrowserFamily::Firefox,
            FIREFOX_MINIMUM_VERSION,
        ));
        browsers.push(BrowserSpec::new(BrowserFamily::Edge, EDGE_MINIMUM_VERSION));
    }

    let generator = HeaderGenerator::new(browsers, operating_systems, devices);
    let mut headers = match generator.generate() {
        Ok(headers) => headers,
        Err(error) => {
            // Python catches browserforge's ValueError and retries with the
            // version floors dropped.
            tracing::debug!(%error, "retrying header generation without version floors");
            let relaxed = generator.clone().with_browsers(
                generator
                    .browsers
                    .iter()
                    .map(|spec| spec.without_floor())
                    .collect(),
            );
            relaxed.generate_relaxed().unwrap_or_default()
        }
    };

    if browser_mode {
        if let Some(driven) = driven_major {
            if let Some(ua) = headers.user_agent() {
                let rewritten = rewrite_chrome_version(ua, driven);
                headers.set_user_agent(rewritten);
            }
        }
    }

    headers
}

/// The header set a given impersonation target would send.
///
/// Not part of the Python API: `curl_cffi` builds these headers inside
/// curl-impersonate, where Rust cannot see them. Exposed so callers on a
/// non-curl transport can approximate the same header set for a chosen
/// [`ImpersonateProfile`]. Falls back to the nearest version of the same
/// family when the exact major is not vendored.
#[must_use]
pub fn headers_for_impersonation(profile: ImpersonateProfile) -> Option<Headers> {
    let family = profile.family().header_family();
    let device = if profile.is_mobile() {
        Device::Mobile
    } else {
        Device::Desktop
    };
    let matching: Vec<&HeaderProfile> = profiles()
        .iter()
        .filter(|candidate| candidate.family == family && candidate.device == device)
        .collect();
    if matching.is_empty() {
        return None;
    }
    let target = profile.major_version();
    let exact: Vec<&&HeaderProfile> = matching
        .iter()
        .filter(|candidate| candidate.major == target)
        .collect();
    let chosen = if exact.is_empty() {
        // Nearest vendored major of the same family.
        matching
            .iter()
            .min_by_key(|candidate| candidate.major.abs_diff(target))
            .copied()
    } else {
        exact.choose(&mut rand::rng()).copied().copied()
    };
    chosen.map(|candidate| candidate.headers())
}

/// Rewrite every `Chrome/<version>` token in `user_agent` to
/// `Chrome/{major}.0.0.0`.
///
/// Python uses `re.sub`, which replaces **all** occurrences, so this uses
/// `replace_all`. Only the `Chrome/…` token changes: the platform, the
/// `AppleWebKit/537.36` and `Safari/537.36` tokens and any `Mobile` marker are
/// kept, which is exactly what makes the rewritten UA still self-consistent.
///
/// # Examples
///
/// ```
/// use browser_fingerprint::headers::rewrite_chrome_version;
///
/// let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
///           (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
/// assert!(rewrite_chrome_version(ua, 142).contains("Chrome/142.0.0.0"));
/// ```
#[must_use]
pub fn rewrite_chrome_version(user_agent: &str, major: u32) -> String {
    static CHROME_TOKEN: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"Chrome/[\d.]+").ok());

    let Some(pattern) = CHROME_TOKEN.as_ref() else {
        return user_agent.to_owned();
    };
    let replacement = format!("Chrome/{major}.0.0.0");
    pattern
        .replace_all(user_agent, regex::NoExpand(&replacement))
        .into_owned()
}

// ---------------------------------------------------------------------------
// Driver version discovery
// ---------------------------------------------------------------------------

/// The Chromium major version the installed automation driver launches.
///
/// Python locates the package with `importlib.util.find_spec` and reads
/// `<pkg>/driver/package/browsers.json`. Rust has no import machinery, so the
/// driver root comes from configuration:
///
/// * `BROWSER_RS_DRIVEN_CHROME_MAJOR` — the answer, straight up;
/// * `PLAYWRIGHT_DRIVER_PATH`, then `PATCHRIGHT_DRIVER_PATH` — package roots
///   to probe, in Python's `playwright` → `patchright` fallback order.
///
/// Every I/O and parse failure is swallowed, exactly as Python does. The
/// result is cached for the process lifetime, matching `lru_cache(2)`.
#[must_use]
pub fn driven_browser_version() -> Option<u32> {
    static DRIVEN: LazyLock<Option<u32>> = LazyLock::new(|| {
        if let Some(major) = std::env::var("BROWSER_RS_DRIVEN_CHROME_MAJOR")
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
        {
            return Some(major);
        }
        ["PLAYWRIGHT_DRIVER_PATH", "PATCHRIGHT_DRIVER_PATH"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .find_map(|root| driven_browser_version_at(Path::new(&root)))
    });
    *DRIVEN
}

/// Read `<package_root>/driver/package/browsers.json` and return the Chromium
/// major version it names.
///
/// Returns `None` for a missing file, unreadable file, malformed JSON, a
/// missing `chromium` entry or an unparseable version — all the failures
/// Python swallows.
#[must_use]
pub fn driven_browser_version_at(package_root: &Path) -> Option<u32> {
    let path: PathBuf = package_root
        .join("driver")
        .join("package")
        .join("browsers.json");
    let raw = fs::read_to_string(&path).ok()?;
    let document: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let version = document
        .get("browsers")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some("chromium"))?
        .get("browserVersion")?
        .as_str()?;
    version.split('.').next()?.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_parses_and_matches_the_build_time_count() {
        let parsed = validate_profile_table().expect("asset must deserialise");
        assert_eq!(parsed, HEADER_PROFILE_COUNT);
        assert_eq!(profiles().len(), HEADER_PROFILE_COUNT);
        const { assert!(HEADER_PROFILE_COUNT >= 500) };
    }

    #[test]
    fn every_profile_is_self_consistent() {
        for profile in profiles() {
            assert!(!profile.headers.is_empty());
            assert_eq!(profile.os.device(), profile.device);

            let headers = profile.headers();
            let ua = headers.user_agent().expect("every profile carries a UA");
            assert!(headers.contains("Accept"));
            assert!(headers.contains("Accept-Encoding"));
            assert!(headers.contains("Accept-Language"));

            match profile.family {
                BrowserFamily::Chrome | BrowserFamily::Edge => {
                    assert!(ua.contains(&format!("Chrome/{}", profile.major)), "{ua}");
                    assert!(headers.contains("sec-ch-ua"));
                    assert!(headers.contains("sec-ch-ua-mobile"));
                    assert!(headers.contains("sec-ch-ua-platform"));
                    assert!(headers.contains("Upgrade-Insecure-Requests"));
                    assert!(headers.contains("Sec-Fetch-Site"));
                    assert!(headers.contains("Sec-Fetch-Mode"));
                    assert!(headers.contains("Sec-Fetch-User"));
                    assert!(headers.contains("Sec-Fetch-Dest"));
                    let mobile = headers.get("sec-ch-ua-mobile").unwrap_or_default();
                    assert_eq!(mobile == "?1", profile.device == Device::Mobile);
                    if profile.family == BrowserFamily::Edge {
                        assert!(ua.contains("Edg"), "{ua}");
                    }
                }
                BrowserFamily::Firefox => {
                    assert!(ua.contains(&format!("Firefox/{}", profile.major)), "{ua}");
                    assert!(
                        !headers.contains("sec-ch-ua"),
                        "firefox has no client hints"
                    );
                    assert!(headers.contains("Upgrade-Insecure-Requests"));
                }
                BrowserFamily::Safari => {
                    assert!(ua.contains("Safari/"), "{ua}");
                    assert!(!headers.contains("sec-ch-ua"), "safari has no client hints");
                }
                BrowserFamily::Tor => unreachable!("tor is never a profile family"),
            }
        }
    }

    #[test]
    fn header_names_are_unique_within_a_profile() {
        for profile in profiles() {
            let mut names: Vec<String> = profile
                .headers
                .iter()
                .map(|(k, _)| k.to_lowercase())
                .collect();
            names.sort();
            let before = names.len();
            names.dedup();
            assert_eq!(names.len(), before, "duplicate header in {profile:?}");
        }
    }

    #[test]
    fn chromium_header_order_is_the_real_wire_order() {
        let profile = profiles()
            .iter()
            .find(|p| p.family == BrowserFamily::Chrome && p.device == Device::Desktop)
            .expect("chrome desktop profiles exist");
        let names: Vec<&str> = profile.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "Upgrade-Insecure-Requests",
                "User-Agent",
                "Accept",
                "Sec-Fetch-Site",
                "Sec-Fetch-Mode",
                "Sec-Fetch-User",
                "Sec-Fetch-Dest",
                "Accept-Encoding",
                "Accept-Language",
            ]
        );
    }

    #[test]
    fn firefox_header_order_is_the_real_wire_order() {
        let profile = profiles()
            .iter()
            .find(|p| p.family == BrowserFamily::Firefox && p.device == Device::Desktop)
            .expect("firefox desktop profiles exist");
        let names: Vec<&str> = profile.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "User-Agent",
                "Accept",
                "Accept-Language",
                "Accept-Encoding",
                "Upgrade-Insecure-Requests",
                "Sec-Fetch-Dest",
                "Sec-Fetch-Mode",
                "Sec-Fetch-Site",
                "Sec-Fetch-User",
                "Priority",
            ]
        );
    }

    #[test]
    fn generated_headers_preserve_profile_order() {
        let headers = generate_headers_for(Some(OsName::Linux), BrowserMode::Http, None);
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        let profile = profiles()
            .iter()
            .find(|p| {
                p.headers
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .eq(names.iter().copied())
            })
            .expect("generated order must match some vendored profile");
        assert_eq!(profile.headers.len(), headers.len());
    }

    #[test]
    fn http_mode_honours_the_version_floors() {
        let generator = HeaderGenerator::new(
            vec![
                BrowserSpec::new(BrowserFamily::Chrome, MINIMUM_VERSION),
                BrowserSpec::new(BrowserFamily::Firefox, FIREFOX_MINIMUM_VERSION),
                BrowserSpec::new(BrowserFamily::Edge, EDGE_MINIMUM_VERSION),
            ],
            DESKTOP_OS.to_vec(),
            vec![Device::Desktop],
        );
        let candidates = generator.candidates();
        assert!(!candidates.is_empty());
        for profile in candidates {
            let floor = match profile.family {
                BrowserFamily::Chrome => MINIMUM_VERSION,
                BrowserFamily::Firefox => FIREFOX_MINIMUM_VERSION,
                BrowserFamily::Edge => EDGE_MINIMUM_VERSION,
                other => unreachable!("unexpected family {other}"),
            };
            assert!(profile.major >= floor, "{profile:?}");
            assert!(profile.os.is_desktop());
        }
    }

    #[test]
    fn http_mode_widens_a_desktop_host_to_all_three_desktop_systems() {
        // Sampling many times from a single host OS must produce all three
        // desktop platforms, i.e. the real OS never leaks.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..400 {
            let headers = generate_headers_for(Some(OsName::Macos), BrowserMode::Http, None);
            let ua = headers.user_agent().unwrap_or_default().to_owned();
            if ua.contains("Windows NT") {
                seen.insert(OsName::Windows);
            } else if ua.contains("Mac OS X") {
                seen.insert(OsName::Macos);
            } else if ua.contains("Linux") || ua.contains("X11") {
                seen.insert(OsName::Linux);
            }
        }
        assert_eq!(
            seen.len(),
            3,
            "expected all three desktop systems, saw {seen:?}"
        );
    }

    #[test]
    fn browser_mode_pins_the_host_os_and_chrome_only() {
        for _ in 0..200 {
            let headers = generate_headers_for(Some(OsName::Macos), BrowserMode::Browser, None);
            let ua = headers.user_agent().unwrap_or_default();
            assert!(ua.contains("Mac OS X"), "{ua}");
            assert!(ua.contains("Chrome/"), "{ua}");
            assert!(!ua.contains("Firefox/"), "{ua}");
            assert!(!ua.contains("Edg/"), "{ua}");
        }
    }

    #[test]
    fn chrome_mode_behaves_exactly_like_browser_mode() {
        assert!(BrowserMode::Chrome.is_browser());
        assert!(BrowserMode::Browser.is_browser());
        assert!(!BrowserMode::Http.is_browser());
        let headers = generate_headers_for(Some(OsName::Windows), BrowserMode::Chrome, None);
        let ua = headers.user_agent().unwrap_or_default();
        assert!(ua.contains("Windows NT"), "{ua}");
        assert!(ua.contains("Chrome/"), "{ua}");
    }

    #[test]
    fn browser_mode_rewrites_the_chrome_token_to_the_driven_version() {
        let headers = generate_headers_for(Some(OsName::Linux), BrowserMode::Browser, Some(133));
        let ua = headers.user_agent().unwrap_or_default();
        assert!(ua.contains("Chrome/133.0.0.0"), "{ua}");
        assert!(ua.contains("AppleWebKit/537.36"), "{ua}");
        assert!(ua.contains("Safari/537.36"), "{ua}");
    }

    #[test]
    fn http_mode_never_rewrites_the_chrome_token() {
        for _ in 0..50 {
            let headers = generate_headers_for(Some(OsName::Linux), BrowserMode::Http, Some(1));
            let ua = headers.user_agent().unwrap_or_default();
            assert!(!ua.contains("Chrome/1.0.0.0"), "{ua}");
        }
    }

    #[test]
    fn a_mobile_host_gets_mobile_headers_in_both_modes() {
        for mode in [BrowserMode::Http, BrowserMode::Browser] {
            let headers = generate_headers_for(Some(OsName::Android), mode, None);
            let ua = headers.user_agent().unwrap_or_default();
            assert!(ua.contains("Android"), "{mode:?}: {ua}");
            assert!(ua.contains("Mobile"), "{mode:?}: {ua}");
        }
    }

    #[test]
    fn an_unknown_host_allows_every_os_and_both_devices() {
        let mut devices = std::collections::HashSet::new();
        let mut systems = std::collections::HashSet::new();
        let generator = HeaderGenerator::new(
            vec![BrowserSpec::any(BrowserFamily::Chrome)],
            SUPPORTED_OPERATING_SYSTEMS.to_vec(),
            SUPPORTED_DEVICES.to_vec(),
        );
        for profile in generator.candidates() {
            devices.insert(profile.device);
            systems.insert(profile.os);
        }
        assert_eq!(devices.len(), 2);
        assert!(systems.len() >= 4);

        // And the wrapper reaches the same branch.
        let headers = generate_headers_for(None, BrowserMode::Http, None);
        assert!(headers.user_agent().is_some());
    }

    #[test]
    fn dropping_the_floors_rescues_an_unsatisfiable_constraint() {
        let strict = HeaderGenerator::new(
            vec![BrowserSpec::new(BrowserFamily::Safari, 999)],
            vec![OsName::Macos],
            vec![Device::Desktop],
        );
        assert!(strict.generate().is_err());
        let relaxed = strict.with_browsers(vec![BrowserSpec::any(BrowserFamily::Safari)]);
        assert!(relaxed.generate().is_ok());
    }

    #[test]
    fn relaxation_walks_devices_then_systems_then_browsers() {
        let impossible = HeaderGenerator::new(
            vec![BrowserSpec::new(BrowserFamily::Safari, 999)],
            vec![OsName::Windows],
            vec![Device::Mobile],
        );
        assert!(impossible.generate().is_err());
        assert!(impossible.generate_relaxed().is_ok());
    }

    #[test]
    fn no_matching_profile_error_names_the_constraints() {
        let generator = HeaderGenerator::new(
            vec![BrowserSpec::new(BrowserFamily::Chrome, 9_999)],
            vec![OsName::Ios],
            vec![Device::Mobile],
        );
        let rendered = generator.generate().unwrap_err().to_string();
        assert!(rendered.contains("chrome>=9999"), "{rendered}");
        assert!(rendered.contains("ios"), "{rendered}");
        assert!(rendered.contains("mobile"), "{rendered}");
    }

    #[test]
    fn tor_borrows_firefox_profiles() {
        let generator = HeaderGenerator::new(
            vec![BrowserSpec::any(BrowserFamily::Tor)],
            vec![OsName::Linux],
            vec![Device::Desktop],
        );
        let candidates = generator.candidates();
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .all(|profile| profile.family == BrowserFamily::Firefox));
    }

    #[test]
    fn the_chrome_rewrite_replaces_every_occurrence_and_nothing_else() {
        let ua = "Chrome/150.0.7000.1 Chrome/12.3";
        assert_eq!(
            rewrite_chrome_version(ua, 99),
            "Chrome/99.0.0.0 Chrome/99.0.0.0"
        );

        let full = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 \
                    (KHTML, like Gecko) Chrome/150.0.0.0 Mobile Safari/537.36";
        let rewritten = rewrite_chrome_version(full, 142);
        assert!(rewritten.contains("Chrome/142.0.0.0"));
        assert!(rewritten.contains("Mobile Safari/537.36"));
        assert!(rewritten.contains("Android 10; K"));

        // A UA with no Chrome token is returned unchanged.
        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:147.0) Gecko/20100101 Firefox/147.0";
        assert_eq!(rewrite_chrome_version(firefox, 150), firefox);
    }

    #[test]
    fn the_rewrite_does_not_expand_capture_syntax() {
        // `$` in the replacement would be an expansion sigil without NoExpand.
        let ua = "Chrome/1.2.3 $1";
        assert_eq!(rewrite_chrome_version(ua, 150), "Chrome/150.0.0.0 $1");
    }

    #[test]
    fn set_user_agent_keeps_its_position() {
        let mut headers = Headers::from(vec![
            ("Accept".to_owned(), "*/*".to_owned()),
            ("User-Agent".to_owned(), "old".to_owned()),
            ("Accept-Encoding".to_owned(), "gzip".to_owned()),
        ]);
        headers.set_user_agent("new");
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["Accept", "User-Agent", "Accept-Encoding"]);
        assert_eq!(headers.user_agent(), Some("new"));

        // No UA present -> no-op, mirroring Python's walrus guard.
        let mut without = Headers::from(vec![("Accept".to_owned(), "*/*".to_owned())]);
        without.set_user_agent("new");
        assert_eq!(without.user_agent(), None);
        assert_eq!(without.len(), 1);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let headers = Headers::from(vec![("User-Agent".to_owned(), "ua".to_owned())]);
        assert_eq!(headers.get("user-agent"), Some("ua"));
        assert_eq!(headers.get("USER-AGENT"), Some("ua"));
        assert!(headers.contains("uSeR-aGeNt"));
        assert_eq!(headers.get("Referer"), None);
        assert!(!Headers::new().contains("Accept"));
        assert!(Headers::new().is_empty());
    }

    #[test]
    fn headers_round_trip_through_vec() {
        let pairs = vec![
            ("A".to_owned(), "1".to_owned()),
            ("B".to_owned(), "2".to_owned()),
        ];
        let headers = Headers::from(pairs.clone());
        assert_eq!(headers.as_slice(), pairs.as_slice());
        assert_eq!(headers.clone().into_vec(), pairs);
        assert_eq!((&headers).into_iter().count(), 2);
        assert_eq!(headers.into_iter().count(), 2);
    }

    #[test]
    fn impersonation_targets_all_resolve_to_a_header_set() {
        for profile in ImpersonateProfile::ALL {
            let headers = headers_for_impersonation(*profile)
                .unwrap_or_else(|| panic!("no headers for {profile}"));
            let ua = headers.user_agent().unwrap_or_default();
            assert!(!ua.is_empty(), "{profile}");
            match profile.family().header_family() {
                BrowserFamily::Chrome | BrowserFamily::Edge => assert!(ua.contains("Chrome/")),
                BrowserFamily::Firefox => assert!(ua.contains("Firefox/")),
                BrowserFamily::Safari => assert!(ua.contains("Safari/")),
                BrowserFamily::Tor => unreachable!(),
            }
            if profile.is_mobile() {
                assert!(
                    ua.contains("Mobile") || ua.contains("iPhone") || ua.contains("Android"),
                    "{profile}: {ua}"
                );
            }
        }
    }

    #[test]
    fn impersonation_prefers_the_exact_major_when_vendored() {
        let headers = headers_for_impersonation(ImpersonateProfile::Chrome150)
            .expect("chrome150 is vendored");
        assert!(headers
            .user_agent()
            .unwrap_or_default()
            .contains("Chrome/150."));
    }

    #[test]
    fn driver_version_discovery_swallows_every_failure() {
        let missing = Path::new("/definitely/not/a/playwright/install");
        assert_eq!(driven_browser_version_at(missing), None);
    }

    #[test]
    fn driver_version_reads_browsers_json() {
        let dir = std::env::temp_dir().join(format!(
            "browser-fingerprint-driver-{}",
            std::process::id()
        ));
        let package = dir.join("driver").join("package");
        std::fs::create_dir_all(&package).expect("temp dir");
        std::fs::write(
            package.join("browsers.json"),
            r#"{"browsers":[{"name":"firefox","browserVersion":"1.0"},
                            {"name":"chromium","browserVersion":"149.0.7127.32"}]}"#,
        )
        .expect("write browsers.json");
        assert_eq!(driven_browser_version_at(&dir), Some(149));

        std::fs::write(package.join("browsers.json"), "not json").expect("write");
        assert_eq!(driven_browser_version_at(&dir), None);

        std::fs::write(package.join("browsers.json"), r#"{"browsers":[]}"#).expect("write");
        assert_eq!(driven_browser_version_at(&dir), None);

        std::fs::write(
            package.join("browsers.json"),
            r#"{"browsers":[{"name":"chromium","browserVersion":"oops"}]}"#,
        )
        .expect("write");
        assert_eq!(driven_browser_version_at(&dir), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn os_and_device_vocabulary_matches_python() {
        assert_eq!(SUPPORTED_OPERATING_SYSTEMS.len(), 5);
        assert_eq!(DESKTOP_OS, &[OsName::Linux, OsName::Macos, OsName::Windows]);
        assert_eq!(SUPPORTED_DEVICES, &[Device::Desktop, Device::Mobile]);
        for os in DESKTOP_OS {
            assert!(os.is_desktop());
            assert_eq!(os.device(), Device::Desktop);
        }
        for os in [OsName::Android, OsName::Ios] {
            assert!(!os.is_desktop());
            assert_eq!(os.device(), Device::Mobile);
        }
        assert_eq!(MINIMUM_VERSION, 149);
        assert_eq!(FIREFOX_MINIMUM_VERSION, 147);
        assert_eq!(EDGE_MINIMUM_VERSION, 145);
    }

    #[test]
    fn host_os_detection_returns_this_platform() {
        // The crate is built for a platform Rust supports, so detection can
        // only be `None` when someone sets BROWSER_RS_HOST_OS to nonsense.
        if std::env::var("BROWSER_RS_HOST_OS").is_err() {
            assert!(host_os().is_some());
        }
    }

    #[test]
    fn the_default_generator_is_the_http_desktop_shape() {
        let generator = HeaderGenerator::default();
        assert!(!generator.candidates().is_empty());
        assert!(generator.generate().is_ok());
    }
}
