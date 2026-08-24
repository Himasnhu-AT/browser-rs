//! Shared vocabulary types: [`HttpMethod`], [`Proxy`], [`FollowRedirects`],
//! [`RedirectMode`], [`ExtractionType`], [`WaitState`], [`PageLoadState`],
//! [`SetCookieParam`] and [`Override`].
//!
//! These are the Rust equivalents of `reference/core/_types.py`, which is a pure
//! typing module in Python: a pile of `Literal[...]` aliases and one `TypedDict`.
//! Each `Literal` becomes an enum whose `serde` representation is byte-identical to
//! the Python string, so configuration files and JSON payloads round-trip unchanged.

use serde::{Deserialize, Serialize};

/// Distinguishes "the caller did not mention this key" from "the caller explicitly
/// passed `None`".
///
/// the reference implementation's `_get_param` helper keys on *presence*: for its twelve named request
/// keys an explicit `None` **overrides** the session default, while for pass-through
/// keys a `None` is dropped (`ARCHITECTURE.md` §6). A bare `Option<T>` cannot express
/// both, so builders wrap the value once more:
///
/// ```
/// use browser_core::types::Override;
/// use std::time::Duration;
///
/// // "not mentioned" — the session default wins.
/// let unset: Override<Option<Duration>> = Override::Unset;
/// assert_eq!(unset.or_default(Some(Duration::from_secs(30))), Some(Duration::from_secs(30)));
///
/// // "explicitly no timeout" — the default is overridden.
/// let explicit: Override<Option<Duration>> = Override::Set(None);
/// assert_eq!(explicit.or_default(Some(Duration::from_secs(30))), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Override<T> {
    /// The caller never mentioned this key.
    Unset,
    /// The caller supplied a value (possibly `None`, if `T` is itself an `Option`).
    Set(T),
}

// Hand-written so that `Override<T>: Default` does not require `T: Default`. The
// `#[derive(Default)]` expansion clippy suggests would add a `T: Default` bound, and
// `Override` is used to wrap types that have no sensible default (the whole point is
// that `Unset` means "no value"), so the derive is not an option here.
#[allow(clippy::derivable_impls)]
impl<T> Default for Override<T> {
    fn default() -> Self {
        Self::Unset
    }
}

impl<T> Override<T> {
    /// Return the wrapped value, or `d` when the key was never mentioned.
    pub fn or_default(self, d: T) -> T {
        match self {
            Self::Set(v) => v,
            Self::Unset => d,
        }
    }

    /// Return the wrapped value, or compute a default lazily.
    pub fn or_else_default<F: FnOnce() -> T>(self, f: F) -> T {
        match self {
            Self::Set(v) => v,
            Self::Unset => f(),
        }
    }

    /// `true` when the caller supplied a value.
    pub fn is_set(&self) -> bool {
        matches!(self, Self::Set(_))
    }

    /// `true` when the caller never mentioned this key.
    pub fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }

    /// Borrow the wrapped value.
    pub fn as_ref(&self) -> Override<&T> {
        match self {
            Self::Set(v) => Override::Set(v),
            Self::Unset => Override::Unset,
        }
    }

    /// Collapse to an `Option`, losing the presence/absence distinction.
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Set(v) => Some(v),
            Self::Unset => None,
        }
    }

    /// Apply `f` to the wrapped value, if any.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Override<U> {
        match self {
            Self::Set(v) => Override::Set(f(v)),
            Self::Unset => Override::Unset,
        }
    }
}

impl<T> From<T> for Override<T> {
    fn from(v: T) -> Self {
        Self::Set(v)
    }
}

/// The HTTP verbs the reference implementation's fetchers accept (`SUPPORTED_HTTP_METHODS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HttpMethod {
    /// `GET`
    #[serde(rename = "GET")]
    Get,
    /// `POST`
    #[serde(rename = "POST")]
    Post,
    /// `PUT`
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`
    #[serde(rename = "DELETE")]
    Delete,
}

impl HttpMethod {
    /// The uppercase wire name, exactly as Python spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to extract from a page (`extraction_types`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractionType {
    /// Visible text.
    #[default]
    Text,
    /// Raw HTML.
    Html,
    /// Markdown, via the `markdownify` port.
    Markdown,
}

impl ExtractionType {
    /// The lowercase name, exactly as Python spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Markdown => "markdown",
        }
    }
}

/// Selector wait states for the browser fetchers (`SelectorWaitStates`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitState {
    /// Present in the DOM.
    #[default]
    Attached,
    /// Absent from the DOM.
    Detached,
    /// In the DOM but not rendered.
    Hidden,
    /// In the DOM and rendered.
    Visible,
}

impl WaitState {
    /// The lowercase name, exactly as Python spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Detached => "detached",
            Self::Hidden => "hidden",
            Self::Visible => "visible",
        }
    }
}

/// Page load milestones for the browser fetchers (`PageLoadStates`).
///
/// Note the serialised spelling of [`PageLoadState::DomContentLoaded`]:
/// `domcontentloaded`, all lowercase and unseparated, as in Playwright.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageLoadState {
    /// Navigation committed.
    Commit,
    /// `DOMContentLoaded` fired.
    #[default]
    #[serde(rename = "domcontentloaded")]
    DomContentLoaded,
    /// `load` fired.
    Load,
    /// No network activity for 500 ms.
    #[serde(rename = "networkidle")]
    NetworkIdle,
}

impl PageLoadState {
    /// The lowercase name, exactly as Python spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::DomContentLoaded => "domcontentloaded",
            Self::Load => "load",
            Self::NetworkIdle => "networkidle",
        }
    }
}

/// A proxy, either as a bare URL or as Playwright's three-field dictionary
/// (`ProxyType = Union[str, Dict[str, str]]`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Proxy {
    /// `"http://user:pass@host:port"`.
    Url(String),
    /// `{"server": ..., "username": ..., "password": ...}`.
    Config {
        /// Proxy endpoint, e.g. `http://host:8080`.
        server: String,
        /// Optional username for proxy auth.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// Optional password for proxy auth.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

/// The string half of `FollowRedirects`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedirectMode {
    /// Follow redirects, but refuse ones that land on a private address.
    Safe,
    /// Follow every redirect.
    All,
    /// Follow only redirects whose status code says the method is preserved.
    #[serde(rename = "obeycode")]
    ObeyCode,
    /// Follow the first redirect only.
    #[serde(rename = "firstonly")]
    FirstOnly,
}

impl RedirectMode {
    /// The lowercase name, exactly as Python spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::All => "all",
            Self::ObeyCode => "obeycode",
            Self::FirstOnly => "firstonly",
        }
    }
}

/// `FollowRedirects = Union[bool, Literal["safe", "all", "obeycode", "firstonly"]]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FollowRedirects {
    /// `True` / `False`.
    Bool(bool),
    /// One of the four named policies.
    Mode(RedirectMode),
}

impl Default for FollowRedirects {
    fn default() -> Self {
        Self::Bool(true)
    }
}

/// `SameSite` attribute of a cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SameSite {
    /// `Lax`
    Lax,
    /// `None`
    None,
    /// `Strict`
    Strict,
}

/// Playwright's `SetCookieParam`, copied by the reference implementation into `core/_types.py`.
///
/// Python declares it `total=False`, so *every* key is technically optional there.
/// The port keeps `name` and `value` required and validates at the boundary, as
/// `ARCHITECTURE.md` §7.7 prescribes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SetCookieParam {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Either `url`, or `domain` + `path`, must be set for Playwright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Cookie domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Cookie path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Expiry as a Unix timestamp in seconds (a float in Python).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    /// `HttpOnly` flag.
    #[serde(rename = "httpOnly", default, skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    /// `Secure` flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    /// `SameSite` attribute.
    #[serde(rename = "sameSite", default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<SameSite>,
    /// CHIPS partition key.
    #[serde(
        rename = "partitionKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub partition_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_distinguishes_unset_from_explicit_none() {
        let unset: Override<Option<u32>> = Override::Unset;
        let explicit: Override<Option<u32>> = Override::Set(None);
        assert_eq!(unset.or_default(Some(30)), Some(30));
        assert_eq!(explicit.or_default(Some(30)), None);
        assert!(unset.is_unset());
        assert!(explicit.is_set());
    }

    #[test]
    fn override_default_does_not_require_inner_default() {
        // `String` has a Default, but the point is the bound: a type without one
        // still gets `Override::<T>::default()`.
        struct NoDefault;
        let v: Override<NoDefault> = Override::default();
        assert!(v.is_unset());
        assert_eq!(Override::<u8>::default(), Override::Unset);
    }

    #[test]
    fn override_map_and_into_option() {
        assert_eq!(Override::Set(2u8).map(|v| v * 2), Override::Set(4));
        assert_eq!(Override::<u8>::Unset.map(|v| v * 2), Override::Unset);
        assert_eq!(Override::Set(1u8).into_option(), Some(1));
        assert_eq!(Override::<u8>::Unset.into_option(), None);
        assert_eq!(Override::from(7u8), Override::Set(7));
    }

    #[test]
    fn literal_enums_serialise_like_python() {
        assert_eq!(serde_json::to_string(&HttpMethod::Get).unwrap(), r#""GET""#);
        assert_eq!(
            serde_json::to_string(&HttpMethod::Delete).unwrap(),
            r#""DELETE""#
        );
        assert_eq!(
            serde_json::to_string(&ExtractionType::Markdown).unwrap(),
            r#""markdown""#
        );
        assert_eq!(
            serde_json::to_string(&WaitState::Attached).unwrap(),
            r#""attached""#
        );
        assert_eq!(
            serde_json::to_string(&PageLoadState::DomContentLoaded).unwrap(),
            r#""domcontentloaded""#
        );
        assert_eq!(
            serde_json::to_string(&PageLoadState::NetworkIdle).unwrap(),
            r#""networkidle""#
        );
        assert_eq!(
            serde_json::to_string(&RedirectMode::ObeyCode).unwrap(),
            r#""obeycode""#
        );
        assert_eq!(
            serde_json::to_string(&RedirectMode::FirstOnly).unwrap(),
            r#""firstonly""#
        );
        assert_eq!(HttpMethod::Post.to_string(), "POST");
        assert_eq!(ExtractionType::default(), ExtractionType::Text);
    }

    #[test]
    fn follow_redirects_accepts_bool_and_mode() {
        let b: FollowRedirects = serde_json::from_str("true").unwrap();
        assert_eq!(b, FollowRedirects::Bool(true));
        let m: FollowRedirects = serde_json::from_str(r#""safe""#).unwrap();
        assert_eq!(m, FollowRedirects::Mode(RedirectMode::Safe));
        assert_eq!(FollowRedirects::default(), FollowRedirects::Bool(true));
        assert_eq!(serde_json::to_string(&m).unwrap(), r#""safe""#);
    }

    #[test]
    fn proxy_accepts_url_and_dict() {
        let p: Proxy = serde_json::from_str(r#""http://h:1""#).unwrap();
        assert_eq!(p, Proxy::Url("http://h:1".into()));
        let p: Proxy = serde_json::from_str(r#"{"server":"http://h:1","username":"u"}"#).unwrap();
        assert_eq!(
            p,
            Proxy::Config {
                server: "http://h:1".into(),
                username: Some("u".into()),
                password: None,
            }
        );
    }

    #[test]
    fn set_cookie_param_uses_playwright_key_spelling() {
        let c = SetCookieParam {
            name: "a".into(),
            value: "b".into(),
            http_only: Some(true),
            same_site: Some(SameSite::Lax),
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"name":"a","value":"b","httpOnly":true,"sameSite":"Lax"}"#
        );
        let back: SetCookieParam = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }
}
