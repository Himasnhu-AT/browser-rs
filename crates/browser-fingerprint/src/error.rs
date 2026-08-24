//! The typed error enum for this crate: [`FingerprintError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.

use crate::impersonate::ALL_PROFILE_NAMES;

/// Everything that can go wrong inside `browser-fingerprint`.
///
/// The crate does no I/O beyond optionally reading an automation driver's
/// `browsers.json` (whose failures are swallowed, exactly as Python does), so
/// the surface is small: profile validation and header-profile lookup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FingerprintError {
    /// The string handed to [`ImpersonateProfile::parse`] is neither a
    /// canonical `curl_cffi` browser target, an alias, nor a deprecated alias.
    ///
    /// The message names every valid option so a caller can surface it
    /// directly to a user.
    ///
    /// [`ImpersonateProfile::parse`]: crate::impersonate::ImpersonateProfile::parse
    #[error("unknown impersonation profile {requested:?}; valid options are: {}", ALL_PROFILE_NAMES.join(", "))]
    UnknownImpersonateProfile {
        /// The rejected string, as supplied by the caller.
        requested: String,
    },

    /// No precomputed header profile satisfies the requested constraints, and
    /// dropping the version floors did not help either.
    ///
    /// This mirrors browserforge's
    /// `ValueError("No headers based on this input can be generated. …")`,
    /// which the reference implementation catches in `generate_headers`.
    #[error("no headers based on this input can be generated: no precomputed profile matches {constraints}")]
    NoMatchingProfile {
        /// A human-readable rendering of the constraint set that failed.
        constraints: String,
    },

    /// `assets/header_profiles.json` could not be deserialised.
    ///
    /// `build.rs` validates the asset, so reaching this variant means the
    /// compiled-in asset and the runtime model have drifted apart.
    #[error("the built-in header profile table is malformed: {reason}")]
    MalformedProfileTable {
        /// The underlying `serde_json` message.
        reason: String,
    },
}
