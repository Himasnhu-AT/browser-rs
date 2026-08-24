//! [`ProxyRotator`] with a pluggable [`RotationStrategy`], [`is_proxy_error`] and
//! [`construct_proxy_dict`]. Zero I/O.
//!
//! This is the Rust shape of `reference/engines/toolbelt/proxy_rotation.py` plus
//! `construct_proxy_dict` from `reference/engines/toolbelt/navigation.py`.
//!
//! Three properties of the Python original are load bearing and reproduced exactly:
//!
//! * the rotator's lock covers the **strategy call**, not merely the index
//!   increment — a strategy is free to read and rewrite the index, and two threads
//!   must never observe the same index;
//! * the strategy fully owns index advancement, so a "sticky" strategy that returns
//!   the index unchanged makes the rotator hand out the same proxy forever;
//! * there is **no** failure tracking, ban list, cool-down or health scoring in this
//!   version. `is_proxy_error` only changes a log message; the failing proxy is not
//!   penalised. Resilience lives entirely in the caller's retry loop (see
//!   [`crate::retry`]).

use std::fmt;

use parking_lot::Mutex;
use browser_core::types::Proxy;
use thiserror::Error;
use url::{ParseError, Url};

/// The seven lowercase substrings that classify an error as proxy-related
/// (`_PROXY_ERROR_INDICATORS`).
///
/// The set is shared by the HTTP engine (curl/`wreq` errors) and the browser
/// engines (Chromium's `net::ERR_*` strings), which is why it mixes the two
/// vocabularies.
pub const PROXY_ERROR_INDICATORS: [&str; 7] = [
    "net::err_proxy",
    "net::err_tunnel",
    "connection refused",
    "connection reset",
    "connection timed out",
    "failed to connect",
    "could not resolve proxy",
];

/// The four URL schemes [`construct_proxy_dict`] accepts.
pub const PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks4", "socks5"];

/// Everything that can go wrong in this module.
///
/// Python raises `ValueError` and `TypeError` with these exact messages and the
/// the reference implementation test-suite pins them, so the `Display` text is byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProxyError {
    /// `ProxyRotator([])` — Python: `ValueError`.
    #[error("At least one proxy must be provided")]
    Empty,
    /// A [`Proxy::Config`] whose `server` is empty, seen by the rotator.
    /// Python: `ValueError` for a dict without a `"server"` key.
    #[error("Proxy dict must have a 'server' key")]
    DictMissingServer,
    /// The proxy string has no usable scheme or host. Python: `ValueError`.
    #[error("Invalid proxy string!")]
    InvalidProxyString,
    /// The proxy string parses but a component is out of range (a port above
    /// 65535, a malformed IPv6 literal). Python: `ValueError`, raised when
    /// `urlparse(...).port` is read.
    #[error("The proxy argument's string is in invalid format!")]
    InvalidFormat,
    /// A [`Proxy::Config`] that `msgspec` would have rejected. Python: `TypeError`.
    #[error("Invalid proxy dictionary: {0}")]
    InvalidProxyDict(String),
}

// ---------------------------------------------------------------------------
// is_proxy_error
// ---------------------------------------------------------------------------

/// `true` when the rendered error message contains any of
/// [`PROXY_ERROR_INDICATORS`].
///
/// Python does `str(error).lower()` and tests membership, so only the *top-level*
/// rendering is inspected — an error whose `source()` mentions a proxy but whose own
/// `Display` does not is **not** a proxy error. That asymmetry is preserved; walk the
/// chain yourself and call [`is_proxy_error_message`] per link if you want more.
///
/// # Examples
///
/// ```
/// use browser_http::proxy::is_proxy_error;
///
/// assert!(is_proxy_error("NET::ERR_PROXY_AUTH_FAILED"));
/// assert!(is_proxy_error("Connection refused by proxy"));
/// assert!(!is_proxy_error("404 Not Found"));
/// assert!(!is_proxy_error(""));
/// ```
#[must_use]
pub fn is_proxy_error<E: fmt::Display + ?Sized>(error: &E) -> bool {
    is_proxy_error_message(&error.to_string())
}

/// [`is_proxy_error`] for an already-rendered message.
///
/// # Examples
///
/// ```
/// use browser_http::proxy::is_proxy_error_message;
///
/// assert!(is_proxy_error_message("Could not resolve proxy host"));
/// assert!(!is_proxy_error_message("SSL certificate error"));
/// ```
#[must_use]
pub fn is_proxy_error_message(message: &str) -> bool {
    // Python allocates a lowercase copy of the whole message once and then runs
    // seven substring tests over it; matching that keeps the cost identical for
    // long libcurl messages.
    let lowered = message.to_lowercase();
    PROXY_ERROR_INDICATORS
        .iter()
        .any(|indicator| lowered.contains(indicator))
}

// ---------------------------------------------------------------------------
// proxy identity / rendering
// ---------------------------------------------------------------------------

/// The unique key Python builds in `_get_proxy_key`.
///
/// A URL proxy keys on itself; a dictionary proxy keys on `"{server}|{username}"`,
/// so the same endpoint with two different accounts counts as two proxies.
///
/// the reference implementation builds a `{key: index}` map with this and then never reads it. It is
/// kept here because it is the natural identity for the ban tracking this version
/// lacks.
///
/// # Examples
///
/// ```
/// use browser_core::types::Proxy;
/// use browser_http::proxy::proxy_key;
///
/// assert_eq!(proxy_key(&Proxy::Url("http://p:8080".into())), "http://p:8080");
/// assert_eq!(
///     proxy_key(&Proxy::Config {
///         server: "http://p:8080".into(),
///         username: Some("u".into()),
///         password: None,
///     }),
///     "http://p:8080|u"
/// );
/// ```
#[must_use]
pub fn proxy_key(proxy: &Proxy) -> String {
    match proxy {
        Proxy::Url(url) => url.clone(),
        Proxy::Config {
            server, username, ..
        } => format!("{server}|{}", username.as_deref().unwrap_or_default()),
    }
}

/// Render a proxy as the single URL string the HTTP transport wants.
///
/// **Deliberate deviation.** Python hands the rotator's element straight to
/// `curl_cffi`'s `proxy=` keyword, which accepts a string only — a Playwright-style
/// dictionary reaching the HTTP path is simply broken there (`http-fetch.md` §4.2
/// calls this out and says a port should "convert or reject"). This converts:
/// credentials are folded back into the URL's userinfo.
///
/// The six characters that would end the userinfo early — `@ : / ? #` and space —
/// are percent-encoded, because a Playwright dictionary carries *raw* credentials
/// and an unescaped `@` in a password would silently repoint the URL at another
/// host. `%` itself is left alone so a value that is already percent-encoded (what
/// [`construct_proxy_dict`] returns, since `urlparse` does not decode) round-trips
/// unchanged.
///
/// # Examples
///
/// ```
/// use browser_core::types::Proxy;
/// use browser_http::proxy::proxy_url;
///
/// let dict = Proxy::Config {
///     server: "http://host:8080".into(),
///     username: Some("u".into()),
///     password: Some("p".into()),
/// };
/// assert_eq!(proxy_url(&dict), "http://u:p@host:8080");
///
/// // A raw `@` cannot be allowed to move the host.
/// let tricky = Proxy::Config {
///     server: "http://host:8080".into(),
///     username: Some("u".into()),
///     password: Some("p@ss".into()),
/// };
/// assert_eq!(proxy_url(&tricky), "http://u:p%40ss@host:8080");
/// ```
#[must_use]
pub fn proxy_url(proxy: &Proxy) -> String {
    match proxy {
        Proxy::Url(url) => url.clone(),
        Proxy::Config {
            server,
            username,
            password,
        } => {
            let user = username.as_deref().unwrap_or_default();
            let pass = password.as_deref().unwrap_or_default();
            if user.is_empty() && pass.is_empty() {
                return server.clone();
            }
            let userinfo = if pass.is_empty() {
                escape_userinfo(user)
            } else {
                format!("{}:{}", escape_userinfo(user), escape_userinfo(pass))
            };
            match server.split_once("://") {
                Some((scheme, rest)) => format!("{scheme}://{userinfo}@{rest}"),
                None => format!("{userinfo}@{server}"),
            }
        }
    }
}

/// Percent-encode the characters that would terminate a URL's userinfo early.
///
/// `%` is intentionally *not* escaped — see [`proxy_url`].
fn escape_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '@' | ':' | '/' | '?' | '#' | ' ' => {
                out.push_str(&format!("%{:02X}", character as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// A proxy in Playwright's three-field shape, the output of
/// [`construct_proxy_dict`].
///
/// Unlike [`Proxy::Config`], `username` and `password` are always present because
/// Python's `ProxyDict` msgspec struct defaults them to `""`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ProxyDict {
    /// `scheme://host[:port]`, with any userinfo stripped out.
    pub server: String,
    /// Proxy username, `""` when absent.
    pub username: String,
    /// Proxy password, `""` when absent.
    pub password: String,
}

impl ProxyDict {
    /// Fold the credentials back into a single URL (the inverse of the split
    /// [`construct_proxy_dict`] performs).
    #[must_use]
    pub fn to_url(&self) -> String {
        proxy_url(&Proxy::Config {
            server: self.server.clone(),
            username: Some(self.username.clone()),
            password: Some(self.password.clone()),
        })
    }
}

impl From<ProxyDict> for Proxy {
    fn from(value: ProxyDict) -> Self {
        Self::Config {
            server: value.server,
            username: Some(value.username),
            password: Some(value.password),
        }
    }
}

/// Validate a proxy and normalise it into Playwright's dictionary form
/// (`toolbelt/navigation.py::construct_proxy_dict`).
///
/// String inputs are parsed; the scheme must be one of [`PROXY_SCHEMES`] and the
/// host must be non-empty, otherwise [`ProxyError::InvalidProxyString`]. The
/// userinfo is **moved out** of `server` into `username`/`password` and is left
/// percent-encoded exactly as `urllib` leaves it. A port is appended only when the
/// input spelled one out — `"http://h"` yields `"http://h"`, not `"http://h:80"`.
///
/// Dictionary inputs pass through, with absent credentials filled in with `""`.
///
/// # Errors
///
/// * [`ProxyError::InvalidProxyString`] — unusable scheme or empty host.
/// * [`ProxyError::InvalidFormat`] — the port (or an IPv6 literal) is out of range.
/// * [`ProxyError::InvalidProxyDict`] — a [`Proxy::Config`] with an empty `server`,
///   which is the closest analogue of Python's "dict without a `server` key".
///
/// # Examples
///
/// ```
/// use browser_core::types::Proxy;
/// use browser_http::proxy::{construct_proxy_dict, ProxyDict};
///
/// let got = construct_proxy_dict(&Proxy::Url("http://user:pass@Proxy.Example.com:8080".into()))?;
/// assert_eq!(
///     got,
///     ProxyDict {
///         server: "http://proxy.example.com:8080".into(),
///         username: "user".into(),
///         password: "pass".into(),
///     }
/// );
/// # Ok::<(), browser_http::proxy::ProxyError>(())
/// ```
pub fn construct_proxy_dict(proxy: &Proxy) -> Result<ProxyDict, ProxyError> {
    match proxy {
        Proxy::Url(value) => construct_proxy_dict_from_str(value),
        Proxy::Config {
            server,
            username,
            password,
        } => {
            if server.is_empty() {
                return Err(ProxyError::InvalidProxyDict(
                    "Object missing required field `server`".to_owned(),
                ));
            }
            Ok(ProxyDict {
                server: server.clone(),
                username: username.clone().unwrap_or_default(),
                password: password.clone().unwrap_or_default(),
            })
        }
    }
}

/// [`construct_proxy_dict`] for the string form.
///
/// # Errors
///
/// As [`construct_proxy_dict`].
///
/// # Examples
///
/// ```
/// use browser_http::proxy::construct_proxy_dict_from_str;
///
/// let dict = construct_proxy_dict_from_str("socks5://127.0.0.1:1080")?;
/// assert_eq!(dict.server, "socks5://127.0.0.1:1080");
/// assert_eq!(dict.username, "");
/// assert!(construct_proxy_dict_from_str("invalid-proxy-format").is_err());
/// # Ok::<(), browser_http::proxy::ProxyError>(())
/// ```
pub fn construct_proxy_dict_from_str(proxy_string: &str) -> Result<ProxyDict, ProxyError> {
    // `urlparse` never fails; it only raises later, from `.port`, when the port is
    // not an integer in 0..=65535. `Url::parse` front-loads that check, so the
    // error kind decides which of Python's two messages we reproduce.
    let parsed = Url::parse(proxy_string).map_err(|error| match error {
        ParseError::InvalidPort
        | ParseError::InvalidIpv6Address
        | ParseError::InvalidIpv4Address => ProxyError::InvalidFormat,
        _ => ProxyError::InvalidProxyString,
    })?;

    let scheme = parsed.scheme();
    let Some(host) = parsed.host_str().filter(|host| !host.is_empty()) else {
        return Err(ProxyError::InvalidProxyString);
    };
    if !PROXY_SCHEMES.contains(&scheme) {
        return Err(ProxyError::InvalidProxyString);
    }

    // NOTE: an IPv6 host keeps its brackets here (`http://[::1]:9050`), while
    // Python's `urlparse().hostname` strips them and emits the unusable
    // `http://::1:9050`. Keeping the brackets is the one place this function is
    // deliberately *more* correct than the original.
    // `Url` drops a port that equals the scheme's default (`http://h:80` becomes
    // `http://h`), while `urlparse(...).port` reports whatever was written. Python
    // appends the port whenever it was spelled out, so ask the raw string.
    // Python's guard is `if proxy.port:` — a *truthiness* test, so a literal
    // `:0` is dropped along with an absent port (`http://h:0` -> `http://h`).
    let port = parsed
        .port()
        .or_else(|| {
            if has_explicit_port(proxy_string) {
                parsed.port_or_known_default()
            } else {
                None
            }
        })
        .filter(|port| *port != 0);

    let mut server = format!("{scheme}://{host}");
    if let Some(port) = port {
        server.push(':');
        server.push_str(&port.to_string());
    }

    Ok(ProxyDict {
        server,
        username: parsed.username().to_owned(),
        password: parsed.password().unwrap_or_default().to_owned(),
    })
}

/// `true` when the authority of `input` spells out a numeric port.
///
/// Needed because `Url` normalises away a default port and `urlparse` does not;
/// see [`construct_proxy_dict_from_str`].
fn has_explicit_port(input: &str) -> bool {
    let Some((_, after_scheme)) = input.split_once("://") else {
        return false;
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Userinfo may contain ':' and ']', so drop it first.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // An IPv6 literal's colons live inside the brackets.
    let tail = match host.rfind(']') {
        Some(end) => &host[end + 1..],
        None => host,
    };
    tail.split_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// rotation strategies
// ---------------------------------------------------------------------------

/// How a [`ProxyRotator`] picks the next proxy.
///
/// Python's `RotationStrategy = Callable[[List[ProxyType], int], Tuple[ProxyType, int]]`
/// becomes a trait plus a blanket impl for closures, so both of these work:
///
/// ```
/// use browser_core::types::Proxy;
/// use browser_http::proxy::{Cyclic, ProxyRotator};
///
/// let cyclic = ProxyRotator::with_strategy(
///     [Proxy::Url("http://a".into()), Proxy::Url("http://b".into())],
///     Cyclic,
/// )?;
/// assert_eq!(cyclic.get_proxy(), Proxy::Url("http://a".into()));
///
/// // A "sticky" strategy: never advance the index.
/// let sticky = ProxyRotator::with_strategy(
///     [Proxy::Url("http://a".into()), Proxy::Url("http://b".into())],
///     |proxies: &[Proxy], index: usize| (proxies[0].clone(), index),
/// )?;
/// assert_eq!(sticky.get_proxy(), sticky.get_proxy());
/// # Ok::<(), browser_http::proxy::ProxyError>(())
/// ```
///
/// The returned index is stored verbatim: the strategy — not the rotator — owns
/// advancement.
pub trait RotationStrategy: Send + Sync {
    /// Pick a proxy for `current_index` and return it with the index to store.
    ///
    /// `proxies` is guaranteed non-empty: [`ProxyRotator::new`] rejects an empty
    /// list up front.
    fn next(&self, proxies: &[Proxy], current_index: usize) -> (Proxy, usize);
}

impl<F> RotationStrategy for F
where
    F: Fn(&[Proxy], usize) -> (Proxy, usize) + Send + Sync,
{
    fn next(&self, proxies: &[Proxy], current_index: usize) -> (Proxy, usize) {
        self(proxies, current_index)
    }
}

/// The default strategy: walk the list in order, wrapping at the end.
///
/// See [`cyclic_rotation`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Cyclic;

impl RotationStrategy for Cyclic {
    fn next(&self, proxies: &[Proxy], current_index: usize) -> (Proxy, usize) {
        cyclic_rotation(proxies, current_index)
    }
}

/// `cyclic_rotation(proxies, current_index) -> (proxy, next_index)`.
///
/// Both the read index and the stored index are wrapped, so an out-of-range
/// incoming index is tolerated: with two proxies, index `5` yields `proxies[1]`
/// and stores `0`.
///
/// # Panics
///
/// Panics when `proxies` is empty (Python raises `ZeroDivisionError` there).
/// [`ProxyRotator`] can never trigger this because it rejects empty lists.
///
/// # Examples
///
/// ```
/// use browser_core::types::Proxy;
/// use browser_http::proxy::cyclic_rotation;
///
/// let proxies = vec![Proxy::Url("p1".into()), Proxy::Url("p2".into())];
/// assert_eq!(cyclic_rotation(&proxies, 0), (Proxy::Url("p1".into()), 1));
/// assert_eq!(cyclic_rotation(&proxies, 5), (Proxy::Url("p2".into()), 0));
/// ```
#[must_use]
pub fn cyclic_rotation(proxies: &[Proxy], current_index: usize) -> (Proxy, usize) {
    assert!(
        !proxies.is_empty(),
        "cyclic_rotation requires a non-empty proxy list"
    );
    let idx = current_index % proxies.len();
    (proxies[idx].clone(), (idx + 1) % proxies.len())
}

// ---------------------------------------------------------------------------
// ProxyRotator
// ---------------------------------------------------------------------------

/// A thread-safe proxy rotator with a pluggable [`RotationStrategy`].
///
/// Share it as an `Arc<ProxyRotator>`: [`ProxyRotator::get_proxy`] takes `&self`
/// and every fetcher attempt draws from the same cursor.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use browser_core::types::Proxy;
/// use browser_http::proxy::ProxyRotator;
///
/// let rotator = Arc::new(ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080"])?);
/// assert_eq!(rotator.len(), 2);
/// assert_eq!(rotator.get_proxy(), Proxy::Url("http://p1:8080".into()));
/// assert_eq!(rotator.get_proxy(), Proxy::Url("http://p2:8080".into()));
/// assert_eq!(rotator.get_proxy(), Proxy::Url("http://p1:8080".into()));
/// assert_eq!(format!("{rotator:?}"), "ProxyRotator(proxies=2)");
/// # Ok::<(), browser_http::proxy::ProxyError>(())
/// ```
pub struct ProxyRotator {
    proxies: Vec<Proxy>,
    strategy: Box<dyn RotationStrategy>,
    // `parking_lot::Mutex` has no poisoning, which is what Python's
    // `threading.Lock` gives us: a panicking strategy must not brick the rotator.
    current_index: Mutex<usize>,
}

impl ProxyRotator {
    /// Build a rotator over `proxies` using [`Cyclic`].
    ///
    /// Proxies are stored **as supplied** — they are not normalised through
    /// [`construct_proxy_dict`], exactly as in Python.
    ///
    /// # Errors
    ///
    /// * [`ProxyError::Empty`] — the list is empty.
    /// * [`ProxyError::DictMissingServer`] — a [`Proxy::Config`] has an empty
    ///   `server`.
    pub fn new(proxies: impl IntoIterator<Item = Proxy>) -> Result<Self, ProxyError> {
        Self::with_strategy(proxies, Cyclic)
    }

    /// [`ProxyRotator::new`] from bare URL strings.
    ///
    /// # Errors
    ///
    /// As [`ProxyRotator::new`].
    pub fn from_urls<I, S>(urls: I) -> Result<Self, ProxyError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(urls.into_iter().map(|url| Proxy::Url(url.into())))
    }

    /// Build a rotator with a custom strategy.
    ///
    /// Python's "strategy must be callable" `TypeError` has no analogue: the type
    /// system rules it out.
    ///
    /// # Errors
    ///
    /// As [`ProxyRotator::new`].
    pub fn with_strategy(
        proxies: impl IntoIterator<Item = Proxy>,
        strategy: impl RotationStrategy + 'static,
    ) -> Result<Self, ProxyError> {
        let proxies: Vec<Proxy> = proxies.into_iter().collect();
        if proxies.is_empty() {
            return Err(ProxyError::Empty);
        }
        for proxy in &proxies {
            if let Proxy::Config { server, .. } = proxy {
                if server.is_empty() {
                    return Err(ProxyError::DictMissingServer);
                }
            }
        }
        Ok(Self {
            proxies,
            strategy: Box::new(strategy),
            current_index: Mutex::new(0),
        })
    }

    /// Draw the next proxy.
    ///
    /// The lock is held across the **whole** strategy call, not just the index
    /// write, so concurrent callers can never be handed the same index.
    #[must_use]
    pub fn get_proxy(&self) -> Proxy {
        let mut index = self.current_index.lock();
        let (proxy, next) = self.strategy.next(&self.proxies, *index);
        *index = next;
        proxy
    }

    /// A copy of the configured proxies; mutating it cannot affect the rotator.
    #[must_use]
    pub fn proxies(&self) -> Vec<Proxy> {
        self.proxies.clone()
    }

    /// Borrow the configured proxies without copying.
    #[must_use]
    pub fn as_slice(&self) -> &[Proxy] {
        &self.proxies
    }

    /// The number of configured proxies (Python's `__len__`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.proxies.len()
    }

    /// Always `false` — the constructor rejects empty lists. Present because
    /// clippy insists on it next to [`ProxyRotator::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.proxies.is_empty()
    }

    /// The index the next [`ProxyRotator::get_proxy`] will hand to the strategy.
    #[must_use]
    pub fn current_index(&self) -> usize {
        *self.current_index.lock()
    }
}

impl fmt::Debug for ProxyRotator {
    /// Reproduces Python's `__repr__`: `ProxyRotator(proxies=3)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProxyRotator(proxies={})", self.proxies.len())
    }
}

impl fmt::Display for ProxyRotator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    use super::*;

    fn urls(values: &[&str]) -> Vec<Proxy> {
        values.iter().map(|v| Proxy::Url((*v).to_owned())).collect()
    }

    // --- is_proxy_error ---------------------------------------------------

    #[test]
    fn is_proxy_error_matches_every_indicator_case_insensitively() {
        for message in [
            "net::err_proxy_connection_failed",
            "NET::ERR_PROXY_AUTH_FAILED",
            "net::err_tunnel_connection_failed",
            "Connection refused by proxy",
            "Connection reset by peer",
            "Connection timed out while connecting to proxy",
            "Failed to connect to proxy server",
            "Could not resolve proxy host",
            "Net::Err_Proxy",
            "CONNECTION REFUSED",
        ] {
            assert!(is_proxy_error(message), "expected proxy error: {message}");
        }
    }

    #[test]
    fn is_proxy_error_rejects_unrelated_messages() {
        for message in [
            "Page not found",
            "404 Not Found",
            "Internal server error",
            "DNS resolution failed",
            "SSL certificate error",
            "Timeout waiting for response",
            "Invalid JSON response",
            "",
        ] {
            assert!(
                !is_proxy_error(message),
                "unexpected proxy error: {message}"
            );
        }
    }

    #[test]
    fn is_proxy_error_accepts_any_display_including_std_errors() {
        #[derive(Debug, Error)]
        #[error("failed to connect to upstream")]
        struct Custom;

        assert!(is_proxy_error(&Custom));
        assert!(is_proxy_error(&String::from("connection reset")));
        assert!(!is_proxy_error(&std::io::Error::other("nothing to see")));
    }

    #[test]
    fn is_proxy_error_ignores_the_source_chain_like_python() {
        #[derive(Debug, Error)]
        #[error("request failed")]
        struct Outer(#[source] Inner);
        #[derive(Debug, Error)]
        #[error("connection refused")]
        struct Inner;

        assert!(!is_proxy_error(&Outer(Inner)));
        assert!(is_proxy_error(&Inner));
    }

    // --- keys and rendering ----------------------------------------------

    #[test]
    fn proxy_key_matches_python() {
        assert_eq!(proxy_key(&Proxy::Url("http://p".into())), "http://p");
        assert_eq!(
            proxy_key(&Proxy::Config {
                server: "http://p".into(),
                username: Some("u".into()),
                password: Some("secret".into()),
            }),
            "http://p|u"
        );
        assert_eq!(
            proxy_key(&Proxy::Config {
                server: "http://p".into(),
                username: None,
                password: None,
            }),
            "http://p|"
        );
    }

    #[test]
    fn proxy_url_folds_credentials_back_in() {
        assert_eq!(proxy_url(&Proxy::Url("http://p:1".into())), "http://p:1");
        assert_eq!(
            proxy_url(&Proxy::Config {
                server: "http://p:1".into(),
                username: None,
                password: None,
            }),
            "http://p:1"
        );
        assert_eq!(
            proxy_url(&Proxy::Config {
                server: "socks5://p:1".into(),
                username: Some("u".into()),
                password: None,
            }),
            "socks5://u@p:1"
        );
        assert_eq!(
            proxy_url(&Proxy::Config {
                server: "p:1".into(),
                username: Some("u".into()),
                password: Some("s".into()),
            }),
            "u:s@p:1"
        );
    }

    // --- construct_proxy_dict --------------------------------------------

    #[test]
    fn construct_proxy_dict_from_plain_url() {
        let got = construct_proxy_dict_from_str("http://proxy.example.com:8080").unwrap();
        assert_eq!(
            got,
            ProxyDict {
                server: "http://proxy.example.com:8080".to_owned(),
                username: String::new(),
                password: String::new(),
            }
        );
    }

    #[test]
    fn construct_proxy_dict_moves_userinfo_out_of_server() {
        let got = construct_proxy_dict_from_str("http://user:pass@Proxy.Example.com:8080").unwrap();
        assert_eq!(got.server, "http://proxy.example.com:8080");
        assert_eq!(got.username, "user");
        assert_eq!(got.password, "pass");
        // Percent escapes stay encoded, exactly as `urlparse` leaves them.
        let escaped = construct_proxy_dict_from_str("http://u%40x:p%3Ay@h:1").unwrap();
        assert_eq!(escaped.username, "u%40x");
        assert_eq!(escaped.password, "p%3Ay");
    }

    #[test]
    fn construct_proxy_dict_keeps_default_ports_implicit_and_drops_paths() {
        assert_eq!(
            construct_proxy_dict_from_str("http://h/path?x=1")
                .unwrap()
                .server,
            "http://h"
        );
        assert_eq!(
            construct_proxy_dict_from_str("HTTP://h:80").unwrap().server,
            "http://h:80"
        );
        assert_eq!(
            construct_proxy_dict_from_str("https://u@h").unwrap(),
            ProxyDict {
                server: "https://h".to_owned(),
                username: "u".to_owned(),
                password: String::new(),
            }
        );
    }

    #[test]
    fn explicit_default_ports_survive_url_normalisation() {
        assert!(has_explicit_port("http://h:80"));
        assert!(has_explicit_port("http://u:p@h:8080/x"));
        assert!(!has_explicit_port("http://h"));
        assert!(!has_explicit_port("http://u:p@h"));
        assert!(!has_explicit_port("http://h:"));
        assert!(has_explicit_port("socks5://[::1]:1080"));
        assert!(!has_explicit_port("socks5://[::1]"));
        assert!(!has_explicit_port("not-a-url"));

        assert_eq!(
            construct_proxy_dict_from_str("https://h:443")
                .unwrap()
                .server,
            "https://h:443"
        );
        assert_eq!(
            construct_proxy_dict_from_str("http://u:p@h:80/x")
                .unwrap()
                .server,
            "http://h:80"
        );
    }

    #[test]
    fn construct_proxy_dict_accepts_all_four_schemes() {
        for scheme in PROXY_SCHEMES {
            let got = construct_proxy_dict_from_str(&format!("{scheme}://h:1080")).unwrap();
            assert_eq!(got.server, format!("{scheme}://h:1080"));
        }
    }

    #[test]
    fn construct_proxy_dict_rejects_bad_strings() {
        for bad in ["invalid-proxy-format", "ftp://h:21", "http://:8080", ""] {
            assert_eq!(
                construct_proxy_dict_from_str(bad),
                Err(ProxyError::InvalidProxyString),
                "input: {bad:?}"
            );
        }
        assert_eq!(
            construct_proxy_dict_from_str("http://h:99999"),
            Err(ProxyError::InvalidFormat)
        );
    }

    #[test]
    fn construct_proxy_dict_error_messages_match_python() {
        assert_eq!(
            ProxyError::InvalidProxyString.to_string(),
            "Invalid proxy string!"
        );
        assert_eq!(
            ProxyError::InvalidFormat.to_string(),
            "The proxy argument's string is in invalid format!"
        );
        assert_eq!(
            ProxyError::Empty.to_string(),
            "At least one proxy must be provided"
        );
        assert_eq!(
            ProxyError::DictMissingServer.to_string(),
            "Proxy dict must have a 'server' key"
        );
    }

    #[test]
    fn construct_proxy_dict_fills_missing_credentials() {
        let got = construct_proxy_dict(&Proxy::Config {
            server: "http://h:1".into(),
            username: None,
            password: None,
        })
        .unwrap();
        assert_eq!(got.username, "");
        assert_eq!(got.password, "");
        assert_eq!(got.to_url(), "http://h:1");
    }

    #[test]
    fn construct_proxy_dict_rejects_a_dict_without_a_server() {
        let err = construct_proxy_dict(&Proxy::Config {
            server: String::new(),
            username: Some("u".into()),
            password: Some("p".into()),
        })
        .unwrap_err();
        assert!(matches!(err, ProxyError::InvalidProxyDict(_)));
        assert!(err.to_string().starts_with("Invalid proxy dictionary: "));
    }

    #[test]
    fn proxy_dict_round_trips_into_proxy() {
        let dict = ProxyDict {
            server: "http://h:1".to_owned(),
            username: "u".to_owned(),
            password: "p".to_owned(),
        };
        assert_eq!(
            Proxy::from(dict.clone()),
            Proxy::Config {
                server: "http://h:1".into(),
                username: Some("u".into()),
                password: Some("p".into()),
            }
        );
        assert_eq!(dict.to_url(), "http://u:p@h:1");
    }

    // --- cyclic_rotation --------------------------------------------------

    #[test]
    fn cyclic_rotation_wraps_both_indices() {
        let three = urls(&["p1", "p2", "p3"]);
        assert_eq!(cyclic_rotation(&three, 0), (Proxy::Url("p1".into()), 1));
        assert_eq!(cyclic_rotation(&three, 1), (Proxy::Url("p2".into()), 2));
        assert_eq!(cyclic_rotation(&three, 2), (Proxy::Url("p3".into()), 0));
        let two = urls(&["p1", "p2"]);
        assert_eq!(cyclic_rotation(&two, 5), (Proxy::Url("p2".into()), 0));
    }

    #[test]
    #[should_panic(expected = "non-empty proxy list")]
    fn cyclic_rotation_panics_on_an_empty_list() {
        let _ = cyclic_rotation(&[], 0);
    }

    // --- ProxyRotator -----------------------------------------------------

    #[test]
    fn rotator_rejects_an_empty_list() {
        assert_eq!(ProxyRotator::new([]).unwrap_err(), ProxyError::Empty);
        assert_eq!(
            ProxyRotator::from_urls(Vec::<String>::new()).unwrap_err(),
            ProxyError::Empty
        );
    }

    #[test]
    fn rotator_rejects_a_dict_without_a_server() {
        let err = ProxyRotator::new([Proxy::Config {
            server: String::new(),
            username: Some("u".into()),
            password: Some("p".into()),
        }])
        .unwrap_err();
        assert_eq!(err, ProxyError::DictMissingServer);
    }

    #[test]
    fn rotator_cycles_and_stores_proxies_verbatim() {
        let original = vec![
            Proxy::Url("http://p1:8080".into()),
            Proxy::Config {
                server: "http://p2:8080".into(),
                username: Some("u".into()),
                password: Some("p".into()),
            },
        ];
        let rotator = ProxyRotator::new(original.clone()).unwrap();
        assert_eq!(rotator.proxies(), original);
        assert_eq!(rotator.get_proxy(), original[0]);
        assert_eq!(rotator.get_proxy(), original[1]);
        assert_eq!(rotator.get_proxy(), original[0]);
    }

    #[test]
    fn rotator_with_one_proxy_always_returns_it() {
        let rotator = ProxyRotator::from_urls(["http://only:1"]).unwrap();
        for _ in 0..5 {
            assert_eq!(rotator.get_proxy(), Proxy::Url("http://only:1".into()));
        }
    }

    #[test]
    fn rotator_proxies_returns_a_copy() {
        let rotator = ProxyRotator::from_urls(["p1", "p2"]).unwrap();
        let mut copy = rotator.proxies();
        copy.push(Proxy::Url("p3".into()));
        assert_eq!(rotator.len(), 2);
        assert!(!rotator.is_empty());
        assert_eq!(rotator.as_slice().len(), 2);
    }

    #[test]
    fn rotator_repr_matches_python() {
        let rotator = ProxyRotator::from_urls(["p1", "p2", "p3"]).unwrap();
        assert_eq!(format!("{rotator:?}"), "ProxyRotator(proxies=3)");
        assert_eq!(rotator.to_string(), "ProxyRotator(proxies=3)");
    }

    #[test]
    fn custom_strategies_own_index_advancement() {
        let proxies = urls(&["p1", "p2", "p3"]);

        // Sticky: index never moves.
        let sticky =
            ProxyRotator::with_strategy(proxies.clone(), |p: &[Proxy], i: usize| (p[0].clone(), i))
                .unwrap();
        assert_eq!(sticky.get_proxy(), Proxy::Url("p1".into()));
        assert_eq!(sticky.get_proxy(), Proxy::Url("p1".into()));
        assert_eq!(sticky.current_index(), 0);

        // Last element, mirroring the lambda in Python's test-suite.
        let last = ProxyRotator::with_strategy(proxies.clone(), |p: &[Proxy], i: usize| {
            (p[p.len() - 1].clone(), i)
        })
        .unwrap();
        assert_eq!(last.get_proxy(), Proxy::Url("p3".into()));

        // Counter-based alternation, driven by the index the rotator stores.
        let alternating =
            ProxyRotator::with_strategy(urls(&["a", "b"]), |p: &[Proxy], i: usize| {
                (p[i % 2].clone(), i + 1)
            })
            .unwrap();
        assert_eq!(alternating.get_proxy(), Proxy::Url("a".into()));
        assert_eq!(alternating.get_proxy(), Proxy::Url("b".into()));
        assert_eq!(alternating.get_proxy(), Proxy::Url("a".into()));
        assert_eq!(alternating.current_index(), 3);
    }

    #[test]
    fn the_lock_covers_the_strategy_call_not_just_the_increment() {
        // A strategy that reads the index, yields, then writes back would produce
        // duplicate draws if the lock only guarded the store.
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let rotator = Arc::new(
            ProxyRotator::with_strategy(urls(&["p1", "p2"]), move |p: &[Proxy], i: usize| {
                let observed = counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(observed, i, "index advanced without the lock");
                thread::yield_now();
                (p[i % p.len()].clone(), i + 1)
            })
            .unwrap(),
        );

        let mut handles = Vec::new();
        for _ in 0..8 {
            let rotator = Arc::clone(&rotator);
            handles.push(thread::spawn(move || {
                for _ in 0..50 {
                    let _ = rotator.get_proxy();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(rotator.current_index(), 8 * 50);
    }

    #[test]
    fn rotator_is_thread_safe() {
        let rotator = Arc::new(ProxyRotator::from_urls(["p1", "p2", "p3"]).unwrap());
        let mut handles = Vec::new();
        for _ in 0..10 {
            let rotator = Arc::clone(&rotator);
            handles.push(thread::spawn(move || {
                let mut drawn = Vec::with_capacity(100);
                for _ in 0..100 {
                    drawn.push(rotator.get_proxy());
                }
                drawn
            }));
        }
        let mut total = 0;
        for handle in handles {
            for proxy in handle.join().unwrap() {
                assert!(rotator.as_slice().contains(&proxy));
                total += 1;
            }
        }
        assert_eq!(total, 1000);
        assert_eq!(rotator.current_index(), 1000 % 3);
    }
}
