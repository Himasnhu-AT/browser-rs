//! The typed error enum for this crate: [`BrowserError`].
//!
//! `ARCHITECTURE.md` §5.3: every library crate owns exactly one `thiserror`
//! enum — no `Box<dyn Error>` and no `anyhow` below the CLI. Error message
//! strings that the specs pin as observable are reproduced verbatim.
//!
//! # How Python surfaces these
//!
//! The reference implementation validates a session's keyword arguments with `msgspec.convert`,
//! and `msgspec` converts *any* `ValueError`/`TypeError` raised from
//! `__post_init__` into a `msgspec.ValidationError`. `validate()` then
//! re-raises that as `TypeError(f"Invalid argument type: {e}")`. The practical
//! consequence, verified against `the reference implementation`, is that **every**
//! configuration failure reaches user code as a `TypeError` whose text is
//! `"Invalid argument type: "` followed by the message reproduced by
//! [`ConfigError`]'s `Display`:
//!
//! ```text
//! >>> DynamicFetcher.fetch("https://example.com", cdp_url="blahblah")
//! TypeError: Invalid argument type: CDP URL must use 'ws://', 'wss://', 'http://', or 'https://' scheme
//! ```
//!
//! [`ConfigError::python_message`] rebuilds that exact string for callers that
//! need to stay bug-compatible with Python (the CLI, and the MCP server which
//! forwards the text to a model).
//!
//! Ownership note: unit 14 adds the CDP and Cloudflare variants. Units 13 and 14
//! run in different waves, so they never edit this file concurrently.

use thiserror::Error;

/// Label used by the `init_script` file check, as spelled by Python.
pub const INIT_SCRIPT_LABEL: &str = "Init script";

/// Label used by the `executable_path` file check, as spelled by Python.
pub const EXECUTABLE_LABEL: &str = "Browser executable";

/// A configuration value was rejected by the range constraints or by the
/// `__post_init__` validation chain of `PlaywrightConfig` / `StealthConfig`.
///
/// The `Display` text of every variant is the Python message **verbatim**,
/// without the `"Invalid argument type: "` prefix that `validate()` prepends;
/// use [`ConfigError::python_message`] when the prefix is wanted.
#[derive(Debug, Clone, PartialEq, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// An integer field is below its `msgspec` `Meta(ge=…)` bound.
    ///
    /// `max_pages` is `1..=50`, `retries` is `1..=10`.
    #[error("Expected `int` >= {min} - at `$.{field}`")]
    IntTooSmall {
        /// Struct field name, as spelled in Python.
        field: &'static str,
        /// The inclusive lower bound.
        min: i64,
    },

    /// An integer field is above its `msgspec` `Meta(le=…)` bound.
    #[error("Expected `int` <= {max} - at `$.{field}`")]
    IntTooLarge {
        /// Struct field name, as spelled in Python.
        field: &'static str,
        /// The inclusive upper bound.
        max: i64,
    },

    /// A `Seconds = Annotated[float, Meta(ge=0)]` field went negative.
    ///
    /// Despite the alias name, `timeout` and `wait` are **milliseconds** and
    /// only `retry_delay` is really seconds; see [`crate::config`].
    #[error("Expected `float` >= {min:?} - at `$.{field}`")]
    FloatTooSmall {
        /// Struct field name, as spelled in Python.
        field: &'static str,
        /// The inclusive lower bound (always `0.0` today).
        min: f64,
    },

    /// A `Seconds` field was `NaN`, which no comparison can accept.
    ///
    /// Python has no separate branch for this — `NaN >= 0` is `False`, so
    /// `msgspec` reports the same "not >= 0" message. Kept as its own variant
    /// so a Rust caller can tell the two apart while `Display` stays identical.
    #[error("Expected `float` >= {min:?} - at `$.{field}`")]
    FloatNotANumber {
        /// Struct field name, as spelled in Python.
        field: &'static str,
        /// The inclusive lower bound (always `0.0` today).
        min: f64,
    },

    /// Both a static `proxy` and a `proxy_rotator` were supplied.
    #[error(
        "Cannot use 'proxy_rotator' together with 'proxy'. \
         Use either a static proxy or proxy rotation, not both."
    )]
    ProxyAndRotator,

    /// `cdp_url` does not start with `ws://`, `wss://`, `http://` or `https://`.
    #[error("CDP URL must use 'ws://', 'wss://', 'http://', or 'https://' scheme")]
    CdpUrlScheme,

    /// `cdp_url` has an accepted scheme but an empty authority (`http://`).
    #[error("Invalid hostname for the CDP URL")]
    CdpUrlHostname,

    /// An `init_script` / `executable_path` does not exist.
    ///
    /// Python checks *existence first*, so a relative path that does not exist
    /// reports "path not found" rather than "is not an absolute path".
    #[error("{label} path not found: {path}")]
    FileNotFound {
        /// [`INIT_SCRIPT_LABEL`] or [`EXECUTABLE_LABEL`].
        label: &'static str,
        /// The path as the caller supplied it.
        path: String,
    },

    /// An `init_script` / `executable_path` exists but is a directory (or any
    /// non-regular file).
    #[error("{label} is not a file: {path}")]
    NotAFile {
        /// [`INIT_SCRIPT_LABEL`] or [`EXECUTABLE_LABEL`].
        label: &'static str,
        /// The path as the caller supplied it.
        path: String,
    },

    /// An `init_script` / `executable_path` exists and is a file but is
    /// relative.
    #[error("{label} is not an absolute path: {path}")]
    NotAbsolute {
        /// [`INIT_SCRIPT_LABEL`] or [`EXECUTABLE_LABEL`].
        label: &'static str,
        /// The path as the caller supplied it.
        path: String,
    },

    /// A proxy string has an unsupported scheme or no hostname.
    ///
    /// Accepted schemes are `http`, `https`, `socks4` and `socks5`.
    #[error("Invalid proxy string!")]
    InvalidProxyString,

    /// A proxy string's port could not be parsed as an integer in `0..=65535`.
    ///
    /// Python reaches this through `urlparse(...).port` raising `ValueError`
    /// inside `construct_proxy_dict`'s `try` block.
    #[error("The proxy argument's string is in invalid format!")]
    InvalidProxyFormat,

    /// A proxy dictionary has no `server` key.
    #[error("Invalid proxy dictionary: Object missing required field `server`")]
    ProxyDictMissingServer,

    /// A URL's authority has unbalanced `[` / `]`, or data outside the
    /// brackets.
    ///
    /// Raised by CPython's `urlsplit` itself, i.e. *before*
    /// `construct_proxy_dict`'s `try` block, so it is not laundered into
    /// [`ConfigError::InvalidProxyFormat`].
    #[error("Invalid IPv6 URL")]
    InvalidIpv6Url,

    /// A bracketed host is neither an IPv6 literal nor an IPv4 literal.
    ///
    /// `ipaddress.ip_address` raises this from `_check_bracketed_host`.
    #[error("'{host}' does not appear to be an IPv4 or IPv6 address")]
    BracketedHostNotAnIp {
        /// The text between the brackets.
        host: String,
    },

    /// A bracketed host parsed as IPv4; brackets are for IPv6 only.
    #[error("An IPv4 address cannot be in brackets")]
    Ipv4InBrackets,

    /// A bracketed host looked like an IPvFuture literal (`v<hex>.<rest>`) but
    /// did not match `\Av[a-fA-F0-9]+\..+\z`.
    #[error("IPvFuture address is invalid")]
    IpvFutureInvalid,

    /// A non-ASCII authority whose NFKC normalisation grows a URL delimiter.
    ///
    /// CPython's `_checknetloc` closes a genuine spoofing hole:
    /// `http://example.com＠evil.com` (`U+FF20 FULLWIDTH COMMERCIAL AT`)
    /// normalises to an authority containing a real `@`, so an IDNA-normalising
    /// resolver sees `evil.com` where a naive splitter sees one long host.
    /// Raised by `urlsplit` itself, so — like [`ConfigError::InvalidIpv6Url`] —
    /// it is not laundered into [`ConfigError::InvalidProxyFormat`].
    #[error("netloc '{netloc}' contains invalid characters under NFKC normalization")]
    NetlocNfkc {
        /// The authority, as `SplitResult.netloc` reports it.
        netloc: String,
    },
}

impl ConfigError {
    /// The message Python raises, prefix included.
    ///
    /// ```
    /// use browser_cdp::error::ConfigError;
    ///
    /// assert_eq!(
    ///     ConfigError::CdpUrlScheme.python_message(),
    ///     "Invalid argument type: CDP URL must use 'ws://', 'wss://', 'http://', or 'https://' scheme",
    /// );
    /// ```
    #[must_use]
    pub fn python_message(&self) -> String {
        format!("Invalid argument type: {self}")
    }

    /// The Python exception class user code actually catches: always
    /// `"TypeError"`.
    ///
    /// This is not a simplification. `msgspec` launders *every* exception a
    /// `Struct.__post_init__` raises — `ValueError` and `TypeError` alike —
    /// into a `msgspec.ValidationError`, and `validate()` re-raises that as
    /// `TypeError(f"Invalid argument type: {e}")`. Verified against
    /// `the reference implementation` for all 27 failing rows of
    /// `testdata/config_golden.jsonl`, including the `ValueError` branches:
    ///
    /// ```text
    /// >>> validate({"proxy": "http://h:1", "proxy_rotator": rot}, PlaywrightConfig)
    /// TypeError: Invalid argument type: Cannot use 'proxy_rotator' together with 'proxy'. …
    /// ```
    ///
    /// Use [`ConfigError::post_init_exception`] for the class the Python source
    /// *writes*, which is what `ARCHITECTURE.md` §11.3 tabulates but which no
    /// caller ever observes.
    #[must_use]
    pub const fn python_exception(&self) -> &'static str {
        "TypeError"
    }

    /// The exception class the Python source raises *inside* `__post_init__`,
    /// before `msgspec` launders it.
    ///
    /// Documentation value only — [`ConfigError::python_exception`] is what
    /// user code sees. Range-constraint failures never reach `__post_init__`
    /// at all; they are reported as `"ValidationError"`.
    #[must_use]
    pub const fn post_init_exception(&self) -> &'static str {
        match self {
            Self::IntTooSmall { .. }
            | Self::IntTooLarge { .. }
            | Self::FloatTooSmall { .. }
            | Self::FloatNotANumber { .. } => "ValidationError",
            Self::ProxyDictMissingServer => "TypeError",
            _ => "ValueError",
        }
    }
}

/// A page-pool state-machine failure.
///
/// Both messages are pinned by `tests/fetchers/test_pages.py` and
/// `tests/fetchers/test_page_reuse.py`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PoolError {
    /// `add_page` was called with `pages_count >= max_pages`.
    ///
    /// Python raises `RuntimeError(f"Maximum page limit ({max_pages}) reached")`.
    #[error("Maximum page limit ({max_pages}) reached")]
    MaxPagesReached {
        /// The pool's capacity.
        max_pages: usize,
    },

    /// The async acquire path waited out its budget without a tab coming free.
    ///
    /// Python raises
    /// `TimeoutError(f"No pages finished to clear place in the pool within the {n}s timeout period")`.
    #[error(
        "No pages finished to clear place in the pool within the {timeout_secs}s timeout period"
    )]
    NoFreePage {
        /// The budget that elapsed, in **seconds** (`_max_wait_for_page`).
        timeout_secs: u64,
    },
}

/// Everything that can go wrong inside `browser-cdp`.
///
/// Unit 14 extends this with the CDP transport and Cloudflare-solver variants;
/// the enum is `#[non_exhaustive]` so that stays a non-breaking change.
#[derive(Debug, Clone, PartialEq, Error)]
#[non_exhaustive]
pub enum BrowserError {
    /// A session keyword argument failed validation.
    #[error("{0}")]
    Config(#[from] ConfigError),

    /// The page pool refused a request.
    #[error("{0}")]
    Pool(#[from] PoolError),

    /// `start()` was called on a session that is already running.
    #[error("Session has been already started")]
    AlreadyStarted,

    /// `fetch()` was called after `close()`.
    #[error("Context manager has been closed")]
    SessionClosed,

    /// Proxy-rotation mode needs a launched browser to open per-request
    /// contexts from, and there is none.
    #[error("Browser not initialized for proxy rotation mode")]
    BrowserNotInitialized,

    /// Navigation produced no response object at all.
    #[error("Failed to get response for {url}")]
    NoNavigationResponse {
        /// The URL that was navigated to.
        url: String,
    },

    /// `page.content()` kept failing; Python retries 20 × 500 ms.
    #[error("Failed to retrieve the page content after retrying for {elapsed_ms}ms.")]
    PageContentUnavailable {
        /// `max_retries * 500`, i.e. `10000` with the shipped defaults.
        elapsed_ms: u64,
    },

    /// The response factory was handed neither a first nor a final response.
    #[error("Failed to get a response from the page")]
    NoResponseFromPage,

    /// The retry loop fell out of its range without returning or raising.
    ///
    /// Unreachable in practice; Python raises the same `RuntimeError`.
    #[error("Request failed")]
    RequestFailed,

    // -- unit 14: the CDP transport ---------------------------------------
    /// A CDP command failed, or the connection to the browser dropped.
    ///
    /// `chromiumoxide::error::CdpError` is neither `Clone` nor `PartialEq`, so
    /// its `Display` text is captured instead — which is also what
    /// [`browser_http::proxy::is_proxy_error`] needs, since Chrome reports
    /// proxy failures as `net::ERR_PROXY_CONNECTION_FAILED` inside that text.
    #[error("{context}: {message}")]
    Cdp {
        /// The CDP method or operation that failed, for the log line.
        context: String,
        /// The driver's message, verbatim.
        message: String,
    },

    /// The browser process could not be started.
    #[error("Failed to launch the browser: {0}")]
    Launch(String),

    /// `cdp_url` was well-formed but nothing answered on it.
    #[error("Failed to connect to the browser at {url}: {message}")]
    Connect {
        /// The `cdp_url` that was tried.
        url: String,
        /// The driver's message.
        message: String,
    },

    /// No browser binary could be found for the configured channel.
    ///
    /// Python never raises this: Playwright ships its own Chromium and resolves
    /// `channel="chrome"` through its registry. Driving CDP directly, the
    /// binary has to be located on this machine.
    #[error(
        "No {channel} executable found. Set `executable_path`, or install the browser \
         with `brs install`."
    )]
    ExecutableNotFound {
        /// `"chrome"` or `"chromium"`.
        channel: &'static str,
    },

    /// `page.goto()` failed — a DNS failure, a refused connection, a dead
    /// proxy.
    ///
    /// The message keeps Chrome's `net::ERR_…` text so
    /// [`browser_http::proxy::is_proxy_error`] can classify it for the retry
    /// warning, exactly as it classifies Playwright's.
    #[error("page.goto: {message} at {url}")]
    NavigationFailed {
        /// The URL that was navigated to.
        url: String,
        /// Chrome's `errorText`, e.g. `net::ERR_PROXY_CONNECTION_FAILED`.
        message: String,
    },

    /// A wait ran out of time.
    ///
    /// Playwright's `TimeoutError` text is
    /// `Timeout 30000ms exceeded.`; the operation is appended so a log line
    /// says *which* wait gave up.
    #[error("Timeout {timeout_ms}ms exceeded while waiting for {operation}.")]
    Timeout {
        /// What was being waited for.
        operation: String,
        /// The budget that elapsed, in milliseconds.
        timeout_ms: u64,
    },

    /// `capture_xhr` is not a valid regular expression.
    ///
    /// Python only finds out when the first `xhr`/`fetch` response arrives and
    /// `re.search` raises; compiling up front turns a mid-flight crash into a
    /// clean `fetch()` error.
    #[error("Invalid `capture_xhr` pattern {pattern:?}: {message}")]
    InvalidXhrPattern {
        /// The pattern as configured.
        pattern: String,
        /// The regex engine's message.
        message: String,
    },

    /// An evaluated script threw.
    ///
    /// Playwright surfaces this as `Error: <text>` from `page.evaluate`; the
    /// text is whatever `Runtime.exceptionDetails.text` said.
    #[error("Evaluation failed: {message}")]
    JavaScript {
        /// The exception text Chrome reported.
        message: String,
    },

    /// A filesystem operation failed: reading `init_script`, or creating the
    /// temporary user-data directory.
    #[error("{context}: {message}")]
    Io {
        /// What was being done.
        context: String,
        /// The OS error text.
        message: String,
    },
}

/// `Result` alias for this crate.
pub type Result<T, E = BrowserError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_messages_match_msgspec_verbatim() {
        // Captured from the reference implementation + msgspec, via
        // `validate({"max_pages": 0}, PlaywrightConfig)`.
        assert_eq!(
            ConfigError::IntTooSmall {
                field: "max_pages",
                min: 1
            }
            .to_string(),
            "Expected `int` >= 1 - at `$.max_pages`"
        );
        assert_eq!(
            ConfigError::IntTooLarge {
                field: "max_pages",
                max: 50
            }
            .to_string(),
            "Expected `int` <= 50 - at `$.max_pages`"
        );
        assert_eq!(
            ConfigError::IntTooLarge {
                field: "retries",
                max: 10
            }
            .to_string(),
            "Expected `int` <= 10 - at `$.retries`"
        );
        assert_eq!(
            ConfigError::FloatTooSmall {
                field: "timeout",
                min: 0.0
            }
            .to_string(),
            "Expected `float` >= 0.0 - at `$.timeout`"
        );
    }

    #[test]
    fn post_init_messages_match_python_verbatim() {
        assert_eq!(
            ConfigError::ProxyAndRotator.to_string(),
            "Cannot use 'proxy_rotator' together with 'proxy'. \
             Use either a static proxy or proxy rotation, not both."
        );
        assert_eq!(
            ConfigError::FileNotFound {
                label: INIT_SCRIPT_LABEL,
                path: "relative.js".into()
            }
            .to_string(),
            "Init script path not found: relative.js"
        );
        assert_eq!(
            ConfigError::NotAbsolute {
                label: INIT_SCRIPT_LABEL,
                path: "rel_exists.js".into()
            }
            .to_string(),
            "Init script is not an absolute path: rel_exists.js"
        );
        assert_eq!(
            ConfigError::NotAFile {
                label: EXECUTABLE_LABEL,
                path: "/tmp".into()
            }
            .to_string(),
            "Browser executable is not a file: /tmp"
        );
        assert_eq!(
            ConfigError::ProxyDictMissingServer.to_string(),
            "Invalid proxy dictionary: Object missing required field `server`"
        );
    }

    #[test]
    fn pool_messages_match_python_verbatim() {
        assert_eq!(
            PoolError::MaxPagesReached { max_pages: 3 }.to_string(),
            "Maximum page limit (3) reached"
        );
        assert_eq!(
            PoolError::NoFreePage { timeout_secs: 60 }.to_string(),
            "No pages finished to clear place in the pool within the 60s timeout period"
        );
    }

    #[test]
    fn session_messages_match_python_verbatim() {
        assert_eq!(
            BrowserError::AlreadyStarted.to_string(),
            "Session has been already started"
        );
        assert_eq!(
            BrowserError::SessionClosed.to_string(),
            "Context manager has been closed"
        );
        assert_eq!(
            BrowserError::NoNavigationResponse {
                url: "https://example.com".into()
            }
            .to_string(),
            "Failed to get response for https://example.com"
        );
        assert_eq!(
            BrowserError::PageContentUnavailable { elapsed_ms: 10_000 }.to_string(),
            "Failed to retrieve the page content after retrying for 10000ms."
        );
    }

    #[test]
    fn config_errors_flow_into_browser_error() {
        let err: BrowserError = ConfigError::CdpUrlScheme.into();
        assert_eq!(
            err.to_string(),
            "CDP URL must use 'ws://', 'wss://', 'http://', or 'https://' scheme"
        );
        // Everything user code catches is a `TypeError`, whatever
        // `__post_init__` originally raised.
        for err in [
            ConfigError::CdpUrlScheme,
            ConfigError::ProxyAndRotator,
            ConfigError::ProxyDictMissingServer,
            ConfigError::IntTooSmall {
                field: "max_pages",
                min: 1,
            },
        ] {
            assert_eq!(err.python_exception(), "TypeError", "{err}");
        }
        assert_eq!(
            ConfigError::CdpUrlScheme.post_init_exception(),
            "ValueError"
        );
        assert_eq!(
            ConfigError::ProxyDictMissingServer.post_init_exception(),
            "TypeError"
        );
        assert_eq!(
            ConfigError::IntTooSmall {
                field: "max_pages",
                min: 1,
            }
            .post_init_exception(),
            "ValidationError",
            "range constraints never reach __post_init__"
        );
    }
}
