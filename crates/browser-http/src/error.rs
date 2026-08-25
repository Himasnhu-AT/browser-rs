//! The typed error enum for this crate: [`FetchError`] and [`TransportError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! # The retry contract
//!
//! **Retries never key on error text.** `_make_request` only retries on
//! `curl_cffi.CurlError`, i.e. a transport failure; an HTTP 4xx or 5xx is a
//! *success* that returns a [`crate::response::Response`] carrying that status.
//! The Rust equivalent is `matches!(err, FetchError::Transport(_))`, which
//! [`FetchError::is_retryable`] spells out.
//!
//! [`crate::proxy::is_proxy_error`] exists purely to choose between two log
//! lines, exactly as in Python, and is never consulted for retry eligibility.
//!
//! # Mapping to Python
//!
//! | Python | here |
//! |---|---|
//! | `curl_cffi.curl.CurlError` and its subclasses | [`FetchError::Transport`] |
//! | `RuntimeError("No active session available.")` | [`FetchError::NoSession`] |
//! | `RuntimeError("This FetcherSession instance already has an active …")` | [`FetchError::SessionAlreadyActive`] |
//! | `RuntimeError("Cannot exit invalid session")` | [`FetchError::InvalidSessionExit`] |
//! | `ValueError("Cannot use 'proxy_rotator' together with …")` | [`FetchError::Config`] |
//! | `TypeError("Response meta should be dictionary …")` | the type system |
//! | `curl_cffi` rejecting a `CURLFOLLOW_SAFE` hop | [`FetchError::UnsafeRedirect`] |
//! | `curl_cffi.requests.exceptions.TooManyRedirects` | [`FetchError::TooManyRedirects`] |

use thiserror::Error;

use crate::config::ConfigError;
use crate::proxy::ProxyError;

/// A convenient alias for this crate's fallible operations.
pub type Result<T, E = FetchError> = std::result::Result<T, E>;

/// Everything the fetchers can fail with.
///
/// # Examples
///
/// ```
/// use browser_http::error::{FetchError, TransportError};
///
/// let transport = FetchError::from(TransportError::connect("connection refused"));
/// assert!(transport.is_retryable());
/// assert_eq!(transport.to_string(), "transport error: connection refused");
///
/// // Everything else is terminal: Python only ever retried `CurlError`.
/// assert!(!FetchError::NoSession.is_retryable());
/// assert_eq!(FetchError::NoSession.to_string(), "No active session available.");
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FetchError {
    /// A transport-level failure — the only class of error that is retried.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    /// `RuntimeError("No active session available.")`.
    #[error("No active session available.")]
    NoSession,

    /// `RuntimeError("This FetcherSession instance already has an active
    /// {synchronous,asynchronous} session.")`.
    ///
    /// The payload is already the whole Python message.
    #[error("{0}")]
    SessionAlreadyActive(&'static str),

    /// `RuntimeError("Cannot exit invalid session")`.
    #[error("Cannot exit invalid session")]
    InvalidSessionExit,

    /// A configuration invariant was violated before any I/O happened.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// A proxy could not be understood, or a rotator was built with no proxies.
    #[error(transparent)]
    Proxy(#[from] ProxyError),

    /// `follow_redirects="safe"` refused a redirect target.
    ///
    /// This is the hand-rolled `CURLFOLLOW_SAFE` guard of `ARCHITECTURE.md`
    /// §2.3. `reason` names the rule that fired, e.g. `loopback (127.0.0.0/8)`
    /// or `scheme downgrade https -> http`.
    #[error(
        "redirect to a private address blocked by follow_redirects=\"safe\": {url} ({reason})"
    )]
    UnsafeRedirect {
        /// The redirect target that was refused.
        url: String,
        /// Which rule refused it.
        reason: String,
    },

    /// The redirect chain exceeded `max_redirects`.
    ///
    /// `curl_cffi` raises `TooManyRedirects` here; the message is ours because
    /// Python's carries a `CurlError` code rather than a stable string.
    #[error("exceeded max_redirects ({max}) following {url}")]
    TooManyRedirects {
        /// The configured ceiling. `-1` never reaches here: it maps to
        /// [`u32::MAX`] (see [`crate::redirect_safe::redirect_limit`]).
        max: u32,
        /// The URL whose `Location` would have been hop `max + 1`.
        url: String,
    },

    /// A `Location` header could not be resolved against the current URL.
    #[error("invalid redirect target {location:?} from {base}: {source}")]
    InvalidRedirect {
        /// The raw `Location` value.
        location: String,
        /// The URL it was relative to.
        base: String,
        /// Why `Url::join` refused it.
        #[source]
        source: url::ParseError,
    },

    /// A URL was not parsable at all.
    #[error("invalid URL {url:?}: {source}")]
    InvalidUrl {
        /// The offending URL.
        url: String,
        /// Why it did not parse.
        #[source]
        source: url::ParseError,
    },

    /// A redirect pointed at a scheme the fetchers refuse to follow.
    ///
    /// curl restricts `CURLOPT_REDIR_PROTOCOLS` to HTTP and HTTPS by default,
    /// so this fires in **every** redirect mode, not only `"safe"`.
    #[error("refusing to follow a redirect to the {scheme:?} scheme: {url}")]
    UnsupportedRedirectScheme {
        /// The scheme of the target.
        scheme: String,
        /// The target itself.
        url: String,
    },

    /// The blocking façade was called from inside a `tokio` runtime.
    ///
    /// `ARCHITECTURE.md` §5.2 — fail loudly rather than deadlock.
    #[error("blocking API called from inside a tokio runtime; use the async API instead")]
    BlockingInsideRuntime,

    /// The blocking façade could not start its runtime.
    #[error("could not start the blocking runtime: {0}")]
    RuntimeStart(String),

    /// The crate was built without the `impersonate` feature, so there is no
    /// default transport to fall back on.
    #[error(
        "no HTTP backend is available: build `browser-http` with the `impersonate` feature, \
         or supply one with `with_backend`"
    )]
    NoBackend,

    /// Parsing the response body failed.
    #[error(transparent)]
    Parse(#[from] browser_parser::error::ParseError),

    /// A JSON body could not be deserialised.
    #[error("invalid JSON body: {0}")]
    Json(#[from] serde_json::Error),
}

impl FetchError {
    /// Whether the driver loop should try again.
    ///
    /// Only [`FetchError::Transport`] is retryable — `_make_request` catches
    /// `CurlError` and nothing else, and a 4xx/5xx never becomes an error at
    /// all.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    /// The transport failure inside this error, if it is one.
    #[must_use]
    pub fn as_transport(&self) -> Option<&TransportError> {
        match self {
            Self::Transport(inner) => Some(inner),
            _ => None,
        }
    }
}

/// A transport-level failure, i.e. the Rust shape of `curl_cffi`'s `CurlError`
/// family.
///
/// The variants mirror the subclasses `http-fetch.md` §3.8 lists, collapsed to
/// the distinctions a backend can actually make. The payload is always the
/// transport's own message, unchanged, because
/// [`crate::proxy::is_proxy_error`] matches on that text to pick a log line —
/// exactly as Python does.
///
/// # Examples
///
/// ```
/// use browser_http::error::TransportError;
/// use browser_http::proxy::is_proxy_error;
///
/// let err = TransportError::proxy("Failed to connect to proxy");
/// assert!(is_proxy_error(&err));
/// assert_eq!(err.to_string(), "Failed to connect to proxy");
///
/// assert!(!is_proxy_error(&TransportError::other("404 Not Found")));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The connection could not be established (`ConnectionError`).
    #[error("{0}")]
    Connect(String),

    /// DNS resolution failed (`DNSError`).
    #[error("{0}")]
    Dns(String),

    /// The proxy refused, or could not be reached (`ProxyError`).
    #[error("{0}")]
    Proxy(String),

    /// A TLS handshake or certificate check failed (`SSLError`,
    /// `CertificateVerifyError`).
    #[error("{0}")]
    Tls(String),

    /// The request did not finish inside `timeout` (`Timeout`,
    /// `ConnectTimeout`).
    #[error("{0}")]
    Timeout(String),

    /// The body could not be read or decoded (`IncompleteRead`,
    /// `ChunkedEncodingError`, `ContentDecodingError`).
    #[error("{0}")]
    Body(String),

    /// The request could not even be built (`InvalidURL`, `MissingSchema`,
    /// `InvalidSchema`, `InvalidHeader`, `ImpersonateError`).
    #[error("{0}")]
    Build(String),

    /// Anything the backend could not classify.
    #[error("{0}")]
    Other(String),
}

macro_rules! transport_ctors {
    ($( $ctor:ident => $variant:ident, $doc:literal );+ $(;)?) => {
        impl TransportError {
            $(
                #[doc = $doc]
                #[must_use]
                pub fn $ctor(message: impl Into<String>) -> Self {
                    Self::$variant(message.into())
                }
            )+

            /// The transport's own message, unclassified.
            #[must_use]
            pub fn message(&self) -> &str {
                match self {
                    Self::Connect(m)
                    | Self::Dns(m)
                    | Self::Proxy(m)
                    | Self::Tls(m)
                    | Self::Timeout(m)
                    | Self::Body(m)
                    | Self::Build(m)
                    | Self::Other(m) => m,
                }
            }
        }
    };
}

transport_ctors! {
    connect => Connect, "A connection failure.";
    dns     => Dns,     "A name-resolution failure.";
    proxy   => Proxy,   "A proxy failure.";
    tls     => Tls,     "A TLS failure.";
    timeout => Timeout, "A timeout.";
    body    => Body,    "A body read/decode failure.";
    build   => Build,   "A request-construction failure.";
    other   => Other,   "An unclassified failure.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::is_proxy_error;

    #[test]
    fn only_transport_errors_retry() {
        assert!(FetchError::from(TransportError::timeout("timed out")).is_retryable());
        assert!(FetchError::from(TransportError::proxy("connection refused")).is_retryable());
        assert!(!FetchError::NoSession.is_retryable());
        assert!(!FetchError::BlockingInsideRuntime.is_retryable());
        assert!(!FetchError::UnsafeRedirect {
            url: "http://127.0.0.1/".to_owned(),
            reason: "loopback".to_owned(),
        }
        .is_retryable());
    }

    #[test]
    fn the_pinned_messages_are_verbatim() {
        assert_eq!(
            FetchError::NoSession.to_string(),
            "No active session available."
        );
        assert_eq!(
            FetchError::InvalidSessionExit.to_string(),
            "Cannot exit invalid session"
        );
        assert_eq!(
            FetchError::SessionAlreadyActive(
                "This FetcherSession instance already has an active asynchronous session."
            )
            .to_string(),
            "This FetcherSession instance already has an active asynchronous session."
        );
        assert_eq!(
            FetchError::from(ConfigError::ProxyRotatorConflict).to_string(),
            "Cannot use 'proxy_rotator' together with 'proxy' or 'proxies'. \
             Use either a static proxy or proxy rotation, not both."
        );
    }

    #[test]
    fn a_transport_error_keeps_the_text_is_proxy_error_matches_on() {
        // `is_proxy_error` is Python's log-line chooser and greps the message.
        for message in [
            "net::ERR_PROXY_CONNECTION_FAILED",
            "Connection refused",
            "Could not resolve proxy: p1",
        ] {
            let err = TransportError::other(message);
            assert!(is_proxy_error(&err), "{message}");
            assert_eq!(err.message(), message);
            // …and through the `FetchError` wrapper the text is still findable.
            assert!(FetchError::from(err).to_string().contains(message));
        }
        assert!(!is_proxy_error(&TransportError::other(
            "Internal server error"
        )));
    }

    #[test]
    fn errors_are_send_sync_and_static() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<FetchError>();
        assert_send_sync::<TransportError>();
    }
}
