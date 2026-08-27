//! The server error type and its mapping onto HTTP status codes.
//!
//! One error type for the whole server, as `API.md` §2 requires: every route
//! and every [`crate::engine`] operation returns [`ServerError`], and
//! [`ServerError::into_response`] is the single place a status code is chosen.
//! The MCP layer converts the same enum into a tool error, which is why the
//! variants describe *what went wrong* and never *which route it happened on*.
//!
//! # What a client is told
//!
//! The body is always the same shape, so an agent can parse it without knowing
//! the route:
//!
//! ```jsonc
//! {"error": {"kind": "bad_request", "message": "…"}}
//! ```
//!
//! [`ServerError::Internal`] is the one variant whose message is **not**
//! forwarded. Everything below this layer reports filesystem paths in its
//! `Display` output — `browser-cdp` names the profile directory, `browser-http`
//! names the client certificate — and a scraping server hands its errors to
//! whoever can reach the port. So the detail is logged at `error` level and the
//! client gets a fixed string. Every other variant is one this crate
//! constructs itself, with a message written to be read by an agent.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// The result type every engine operation and every route handler returns.
pub type Result<T> = std::result::Result<T, ServerError>;

/// Everything the server can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// The bearer token was missing or wrong. `401`.
    #[error("missing or invalid bearer token")]
    Unauthorized,

    /// The request was understood but is not valid: a malformed URL, a
    /// selector that does not parse, a limit outside its range. `400`.
    #[error("{0}")]
    BadRequest(String),

    /// A named session, or a route, does not exist. `404`.
    #[error("{0}")]
    NotFound(String),

    /// The cap on concurrent live browsers is reached. `429`, because the
    /// caller should retry rather than change the request.
    #[error("all {max} browser slots are in use; close a session or retry")]
    BrowserLimit {
        /// The configured ceiling.
        max: usize,
    },

    /// The operation ran out of time. `504`.
    #[error("{0}")]
    Timeout(String),

    /// The site, the proxy or the browser failed. `502` — the fault is
    /// upstream of this server, and the message is safe to forward because it
    /// is built from a URL and a status the caller already supplied.
    #[error("{0}")]
    Upstream(String),

    /// The operation exists but this build cannot perform it. `501`.
    #[error("{0}")]
    Unsupported(String),

    /// Anything else. The message is logged, never returned. `500`.
    #[error("internal error")]
    Internal(String),
}

impl ServerError {
    /// A `400` from anything with a `Display`.
    pub fn bad_request(message: impl std::fmt::Display) -> Self {
        Self::BadRequest(message.to_string())
    }

    /// A `502` from anything with a `Display`.
    pub fn upstream(message: impl std::fmt::Display) -> Self {
        Self::Upstream(message.to_string())
    }

    /// A `500` from anything with a `Display`. The text stays server-side.
    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::Internal(message.to_string())
    }

    /// The status code this variant maps to.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BrowserLimit { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable machine-readable tag in the JSON body.
    ///
    /// Agents branch on this; it is part of the contract and does not change
    /// when a message is reworded.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::BrowserLimit { .. } => "browser_limit",
            Self::Timeout(_) => "timeout",
            Self::Upstream(_) => "upstream",
            Self::Unsupported(_) => "unsupported",
            Self::Internal(_) => "internal",
        }
    }

    /// The message a client is allowed to see.
    ///
    /// See the module docs for why [`ServerError::Internal`] is redacted.
    #[must_use]
    pub fn public_message(&self) -> String {
        match self {
            Self::Internal(_) => "internal error".to_owned(),
            other => other.to_string(),
        }
    }
}

/// The JSON body every failing route returns.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// The error itself.
    pub error: ErrorDetail,
}

/// The `error` object inside [`ErrorBody`].
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    /// [`ServerError::kind`].
    pub kind: &'static str,
    /// [`ServerError::public_message`].
    pub message: String,
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        // The one place the redacted detail is not thrown away: an operator
        // reading the log needs the path that a client must not be shown.
        if let Self::Internal(detail) = &self {
            tracing::error!(detail = %detail, "internal server error");
        }
        let body = ErrorBody {
            error: ErrorDetail {
                kind: self.kind(),
                message: self.public_message(),
            },
        };
        (self.status(), Json(body)).into_response()
    }
}

// ---------------------------------------------------------------------------
// Conversions from the layers below.
//
// Each one classifies rather than blanket-wrapping: a 30-second timeout and a
// bad selector are different problems for the caller, and collapsing both into
// a 500 is what makes a server unpleasant to drive from an agent.
// ---------------------------------------------------------------------------

impl From<browser::FetchError> for ServerError {
    fn from(error: browser::FetchError) -> Self {
        use browser::{FetchError, TransportError};

        match &error {
            FetchError::Transport(TransportError::Timeout(_)) => Self::Timeout(error.to_string()),
            // The caller's own input, arriving through the fetcher's error type
            // because `Response::css` and friends live on the response. A
            // malformed selector is a `400`, not a `502`, and getting that
            // wrong is what makes an agent retry a request it should fix.
            FetchError::Parse(inner) => Self::BadRequest(inner.to_string()),
            FetchError::Config(inner) => Self::BadRequest(inner.to_string()),
            FetchError::Proxy(inner) => Self::BadRequest(inner.to_string()),
            FetchError::InvalidUrl { .. } | FetchError::Json(_) => {
                Self::BadRequest(error.to_string())
            }
            // A refused connection, a dead proxy, a TLS failure and an unsafe
            // redirect are all "the other end of the wire", and all of them
            // carry a message the caller needs to see to fix their request.
            _ => Self::Upstream(error.to_string()),
        }
    }
}

impl From<browser::ParseError> for ServerError {
    /// A parse error is nearly always the caller's selector, so it is a `400`.
    fn from(error: browser::ParseError) -> Self {
        Self::BadRequest(error.to_string())
    }
}

impl From<browser::BrowserError> for ServerError {
    fn from(error: browser::BrowserError) -> Self {
        use browser::BrowserError;

        match &error {
            BrowserError::Config(inner) => Self::BadRequest(inner.to_string()),
            BrowserError::Timeout { .. } => Self::Timeout(error.to_string()),
            // Chrome's own `net::ERR_…` text, and the "no response at all"
            // family: the fault is at the far end and the message says which.
            BrowserError::NavigationFailed { .. }
            | BrowserError::NoNavigationResponse { .. }
            | BrowserError::NoResponseFromPage
            | BrowserError::PageContentUnavailable { .. } => Self::Upstream(error.to_string()),
            // The operator has not installed a browser. That is a capability
            // this build genuinely lacks, and the message is actionable.
            BrowserError::ExecutableNotFound { .. } => Self::Unsupported(error.to_string()),
            // Everything else — a failed launch, a dead tab, a raw CDP error —
            // names a local path or a protocol frame often enough that it is
            // not safe to forward verbatim.
            _ => Self::Internal(error.to_string()),
        }
    }
}

impl From<browser::spider::spider::CrawlError> for ServerError {
    fn from(error: browser::spider::spider::CrawlError) -> Self {
        use browser::spider::spider::CrawlError;

        match &error {
            CrawlError::NoStartingPoint | CrawlError::MissingName => {
                Self::BadRequest(error.to_string())
            }
            _ => Self::Internal(error.to_string()),
        }
    }
}

impl From<url::ParseError> for ServerError {
    fn from(error: url::ParseError) -> Self {
        Self::BadRequest(format!("not a valid URL: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_its_own_status_and_kind() {
        let cases: Vec<(ServerError, StatusCode, &str)> = vec![
            (
                ServerError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (
                ServerError::BadRequest("x".into()),
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                ServerError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                ServerError::BrowserLimit { max: 3 },
                StatusCode::TOO_MANY_REQUESTS,
                "browser_limit",
            ),
            (
                ServerError::Timeout("x".into()),
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
            ),
            (
                ServerError::Upstream("x".into()),
                StatusCode::BAD_GATEWAY,
                "upstream",
            ),
            (
                ServerError::Unsupported("x".into()),
                StatusCode::NOT_IMPLEMENTED,
                "unsupported",
            ),
            (
                ServerError::Internal("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
            ),
        ];
        for (error, status, kind) in cases {
            assert_eq!(error.status(), status);
            assert_eq!(error.kind(), kind);
        }
    }

    /// The whole point of the `Internal` variant: the detail is kept for the
    /// log and never reaches the wire.
    #[test]
    fn the_internal_variant_does_not_leak_its_detail() {
        let error = ServerError::internal("/home/someone/.browser-rs/profiles/shop is locked");
        assert_eq!(error.public_message(), "internal error");
        assert!(!error.public_message().contains("profiles"));
    }

    #[test]
    fn a_classified_variant_keeps_its_message() {
        let error = ServerError::bad_request("selector 'a[' is malformed");
        assert_eq!(error.public_message(), "selector 'a[' is malformed");
    }

    #[test]
    fn a_bad_url_is_a_four_hundred() {
        let error: ServerError = url::Url::parse("not a url").expect_err("invalid").into();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }
}
