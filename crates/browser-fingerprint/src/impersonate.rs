//! The impersonation-profile enum, the alias table and profile validation.
//!
//! The lists here are transcribed verbatim from `http-fetch.md` §2.2, which in
//! turn reads `curl_cffi` 0.16.1's `BrowserTypeLiteral` and `REAL_TARGET_MAP`.
//! the reference implementation itself never resolves an alias — it forwards the string straight
//! to `curl_cffi` — so [`ImpersonateProfile::parse`] keeps aliases as first
//! class values and only resolves them on demand via
//! [`ImpersonateProfile::resolve`].

use std::fmt;
use std::str::FromStr;

use crate::error::FingerprintError;

/// The browser engine behind an impersonation target.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum BrowserFamily {
    /// Google Chrome / Chromium.
    Chrome,
    /// Microsoft Edge (Chromium based).
    Edge,
    /// Apple Safari (WebKit).
    Safari,
    /// Mozilla Firefox (Gecko).
    Firefox,
    /// Tor Browser, a hardened Firefox ESR.
    Tor,
}

impl BrowserFamily {
    /// The lowercase family name, as used in `assets/header_profiles.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Safari => "safari",
            Self::Firefox => "firefox",
            Self::Tor => "tor",
        }
    }

    /// The family whose header profiles stand in for this one.
    ///
    /// Tor Browser is Firefox ESR with the branding stripped, so it borrows
    /// Firefox's header sets; every other family maps to itself.
    #[must_use]
    pub const fn header_family(self) -> Self {
        match self {
            Self::Tor => Self::Firefox,
            other => other,
        }
    }
}

impl fmt::Display for BrowserFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

macro_rules! impersonation_profiles {
    ($( $variant:ident => $name:literal, $family:ident, $major:literal, $mobile:literal );+ $(;)?) => {
        /// One concrete `curl_cffi` impersonation target.
        ///
        /// Every variant corresponds to exactly one string in
        /// `curl_cffi` 0.16.1's `BrowserTypeLiteral`. Aliases such as
        /// `"chrome"` are *not* variants: they are resolved by
        /// [`ImpersonateProfile::parse`] through [`ALIASES`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum ImpersonateProfile {
            $(
                #[doc = concat!("The `", $name, "` target.")]
                $variant,
            )+
        }

        /// Every canonical target string, in the order `http-fetch.md` §2.2 lists them.
        ///
        /// Used by [`FingerprintError::UnknownImpersonateProfile`] to name the
        /// valid options, and by callers that want to present a choice list.
        pub static ALL_PROFILE_NAMES: &[&str] = &[$($name),+];

        impl ImpersonateProfile {
            /// Every canonical target, in the order `http-fetch.md` §2.2 lists them.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The exact string `curl_cffi` expects for this target.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            /// The browser engine behind this target.
            #[must_use]
            pub const fn family(self) -> BrowserFamily {
                match self { $(Self::$variant => BrowserFamily::$family),+ }
            }

            /// The browser major version this target impersonates.
            ///
            /// For Safari this is the marketing major (`15`, `17`, `18`, `26`),
            /// so `safari153` and `safari155` both report `15`.
            #[must_use]
            pub const fn major_version(self) -> u32 {
                match self { $(Self::$variant => $major),+ }
            }

            /// Whether the target impersonates a mobile build (Android / iOS).
            #[must_use]
            pub const fn is_mobile(self) -> bool {
                match self { $(Self::$variant => $mobile),+ }
            }
        }
    };
}

impersonation_profiles! {
    // Edge
    Edge99            => "edge99",            Edge,    99, false;
    Edge101           => "edge101",           Edge,   101, false;
    // Chrome
    Chrome99          => "chrome99",          Chrome,  99, false;
    Chrome100         => "chrome100",         Chrome, 100, false;
    Chrome101         => "chrome101",         Chrome, 101, false;
    Chrome104         => "chrome104",         Chrome, 104, false;
    Chrome107         => "chrome107",         Chrome, 107, false;
    Chrome110         => "chrome110",         Chrome, 110, false;
    Chrome116         => "chrome116",         Chrome, 116, false;
    Chrome119         => "chrome119",         Chrome, 119, false;
    Chrome120         => "chrome120",         Chrome, 120, false;
    Chrome123         => "chrome123",         Chrome, 123, false;
    Chrome124         => "chrome124",         Chrome, 124, false;
    Chrome131         => "chrome131",         Chrome, 131, false;
    Chrome133A        => "chrome133a",        Chrome, 133, false;
    Chrome136         => "chrome136",         Chrome, 136, false;
    Chrome142         => "chrome142",         Chrome, 142, false;
    Chrome145         => "chrome145",         Chrome, 145, false;
    Chrome146         => "chrome146",         Chrome, 146, false;
    Chrome150         => "chrome150",         Chrome, 150, false;
    Chrome99Android   => "chrome99_android",  Chrome,  99, true;
    Chrome131Android  => "chrome131_android", Chrome, 131, true;
    // Safari
    Safari153         => "safari153",         Safari,  15, false;
    Safari155         => "safari155",         Safari,  15, false;
    Safari170         => "safari170",         Safari,  17, false;
    Safari172Ios      => "safari172_ios",     Safari,  17, true;
    Safari180         => "safari180",         Safari,  18, false;
    Safari180Ios      => "safari180_ios",     Safari,  18, true;
    Safari184         => "safari184",         Safari,  18, false;
    Safari184Ios      => "safari184_ios",     Safari,  18, true;
    Safari260         => "safari260",         Safari,  26, false;
    Safari2601        => "safari2601",        Safari,  26, false;
    Safari260Ios      => "safari260_ios",     Safari,  26, true;
    // Firefox
    Firefox133        => "firefox133",        Firefox, 133, false;
    Firefox135        => "firefox135",        Firefox, 135, false;
    Firefox144        => "firefox144",        Firefox, 144, false;
    Firefox147        => "firefox147",        Firefox, 147, false;
    // Tor
    Tor145            => "tor145",            Tor,     145, false;
}

/// The alias table (`curl_cffi`'s `REAL_TARGET_MAP` /
/// `resolve_latest_browser_type`) for this version.
///
/// `"tor"` is resolvable even though it is absent from `BrowserTypeLiteral`.
pub static ALIASES: phf::Map<&'static str, ImpersonateProfile> = phf::phf_map! {
    "chrome"          => ImpersonateProfile::Chrome150,
    "edge"            => ImpersonateProfile::Edge101,
    "safari"          => ImpersonateProfile::Safari2601,
    "safari_ios"      => ImpersonateProfile::Safari260Ios,
    "safari_beta"     => ImpersonateProfile::Safari2601,
    "safari_ios_beta" => ImpersonateProfile::Safari260Ios,
    "chrome_android"  => ImpersonateProfile::Chrome131Android,
    "firefox"         => ImpersonateProfile::Firefox147,
    "tor"             => ImpersonateProfile::Tor145,
};

/// Aliases `curl_cffi` still accepts but has deprecated in favour of the
/// compact spelling.
pub static DEPRECATED_ALIASES: phf::Map<&'static str, ImpersonateProfile> = phf::phf_map! {
    "safari15_3"     => ImpersonateProfile::Safari153,
    "safari15_5"     => ImpersonateProfile::Safari155,
    "safari17_0"     => ImpersonateProfile::Safari170,
    "safari17_2_ios" => ImpersonateProfile::Safari172Ios,
    "safari18_0"     => ImpersonateProfile::Safari180,
    "safari18_0_ios" => ImpersonateProfile::Safari180Ios,
    "safari18_4"     => ImpersonateProfile::Safari184,
    "safari18_4_ios" => ImpersonateProfile::Safari184Ios,
};

/// The alias families the reference implementation's own documentation advertises.
pub static ADVERTISED_ALIAS_FAMILIES: &[&str] = &[
    "edge",
    "chrome",
    "chrome_android",
    "safari",
    "safari_beta",
    "safari_ios",
    "safari_ios_beta",
    "firefox",
    "tor",
];

impl ImpersonateProfile {
    /// Validate an impersonation string.
    ///
    /// Accepts a canonical target, an alias from [`ALIASES`], or a deprecated
    /// alias from [`DEPRECATED_ALIASES`]; anything else yields
    /// [`FingerprintError::UnknownImpersonateProfile`], whose message names
    /// every valid option.
    ///
    /// Matching is exact and case-sensitive, matching `curl_cffi`.
    ///
    /// # Errors
    ///
    /// Returns [`FingerprintError::UnknownImpersonateProfile`] when `value` is
    /// not a recognised target.
    ///
    /// # Examples
    ///
    /// ```
    /// use browser_fingerprint::impersonate::ImpersonateProfile;
    ///
    /// assert_eq!(
    ///     ImpersonateProfile::parse("chrome")?,
    ///     ImpersonateProfile::Chrome150
    /// );
    /// assert!(ImpersonateProfile::parse("netscape").is_err());
    /// # Ok::<_, browser_fingerprint::error::FingerprintError>(())
    /// ```
    pub fn parse(value: &str) -> Result<Self, FingerprintError> {
        if let Some(found) = Self::ALL.iter().find(|p| p.as_str() == value) {
            return Ok(*found);
        }
        if let Some(target) = ALIASES.get(value) {
            tracing::debug!(
                alias = value,
                target = target.as_str(),
                "resolved impersonation alias"
            );
            return Ok(*target);
        }
        if let Some(target) = DEPRECATED_ALIASES.get(value) {
            tracing::debug!(
                alias = value,
                target = target.as_str(),
                "resolved deprecated impersonation alias"
            );
            return Ok(*target);
        }
        Err(FingerprintError::UnknownImpersonateProfile {
            requested: value.to_owned(),
        })
    }

    /// Resolve an alias string to its concrete target without constructing an
    /// error for unknown input.
    #[must_use]
    pub fn resolve(alias: &str) -> Option<Self> {
        ALIASES
            .get(alias)
            .or_else(|| DEPRECATED_ALIASES.get(alias))
            .copied()
    }

    /// Whether `value` is a canonical target string (not an alias).
    #[must_use]
    pub fn is_canonical(value: &str) -> bool {
        Self::ALL.iter().any(|p| p.as_str() == value)
    }
}

impl fmt::Display for ImpersonateProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ImpersonateProfile {
    type Err = FingerprintError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canonical_list_matches_the_spec_count() {
        // http-fetch.md §2.2: 2 edge + 20 chrome + 11 safari + 4 firefox + 1 tor.
        assert_eq!(ImpersonateProfile::ALL.len(), 38);
        assert_eq!(ALL_PROFILE_NAMES.len(), 38);
    }

    #[test]
    fn every_name_round_trips() {
        for profile in ImpersonateProfile::ALL {
            assert_eq!(ImpersonateProfile::parse(profile.as_str()), Ok(*profile));
            assert!(ImpersonateProfile::is_canonical(profile.as_str()));
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = ALL_PROFILE_NAMES.to_vec();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn alias_table_matches_the_spec() {
        let expected = [
            ("chrome", "chrome150"),
            ("edge", "edge101"),
            ("safari", "safari2601"),
            ("safari_ios", "safari260_ios"),
            ("safari_beta", "safari2601"),
            ("safari_ios_beta", "safari260_ios"),
            ("chrome_android", "chrome131_android"),
            ("firefox", "firefox147"),
            ("tor", "tor145"),
        ];
        assert_eq!(ALIASES.len(), expected.len());
        for (alias, target) in expected {
            assert_eq!(
                ImpersonateProfile::parse(alias).map(ImpersonateProfile::as_str),
                Ok(target),
                "alias {alias}"
            );
        }
    }

    #[test]
    fn deprecated_aliases_resolve() {
        let expected = [
            ("safari15_3", "safari153"),
            ("safari15_5", "safari155"),
            ("safari17_0", "safari170"),
            ("safari17_2_ios", "safari172_ios"),
            ("safari18_0", "safari180"),
            ("safari18_0_ios", "safari180_ios"),
            ("safari18_4", "safari184"),
            ("safari18_4_ios", "safari184_ios"),
        ];
        assert_eq!(DEPRECATED_ALIASES.len(), expected.len());
        for (alias, target) in expected {
            assert_eq!(
                ImpersonateProfile::parse(alias).map(ImpersonateProfile::as_str),
                Ok(target)
            );
            assert!(!ImpersonateProfile::is_canonical(alias));
        }
    }

    #[test]
    fn unknown_profile_names_the_valid_options() {
        let err = ImpersonateProfile::parse("chrome999").unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("chrome999"), "{rendered}");
        assert!(rendered.contains("chrome150"), "{rendered}");
        assert!(rendered.contains("safari260_ios"), "{rendered}");
        assert!(rendered.contains("tor145"), "{rendered}");
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(ImpersonateProfile::parse("Chrome").is_err());
        assert!(ImpersonateProfile::parse("CHROME150").is_err());
    }

    #[test]
    fn mobile_targets_are_flagged() {
        assert!(ImpersonateProfile::Chrome131Android.is_mobile());
        assert!(ImpersonateProfile::Safari260Ios.is_mobile());
        assert!(!ImpersonateProfile::Chrome150.is_mobile());
        assert!(!ImpersonateProfile::Safari2601.is_mobile());
    }

    #[test]
    fn tor_borrows_firefox_headers() {
        assert_eq!(ImpersonateProfile::Tor145.family(), BrowserFamily::Tor);
        assert_eq!(
            ImpersonateProfile::Tor145.family().header_family(),
            BrowserFamily::Firefox
        );
    }

    #[test]
    fn advertised_alias_families_are_all_resolvable() {
        for family in ADVERTISED_ALIAS_FAMILIES {
            assert!(
                ImpersonateProfile::resolve(family).is_some(),
                "alias family {family} is not resolvable"
            );
        }
    }

    #[test]
    fn from_str_delegates_to_parse() {
        let parsed: ImpersonateProfile = "firefox".parse().expect("alias must parse");
        assert_eq!(parsed, ImpersonateProfile::Firefox147);
    }
}
