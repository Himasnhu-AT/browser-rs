//! `ServerConfig`: bind address, profile root, default egress, auth token.
//!
//! Everything here is read once, at start-up, and then treated as immutable.
//! [`ServerConfig::from_env`] is the operator-facing surface — one process, one
//! set of environment variables — while the struct itself is public so a test
//! (or an embedder) can build a server without touching the environment.
//!
//! # The one rule that is not a preference
//!
//! `AGENTS.md` and `API.md` §2 both say it: **a server with no
//! `BROWSER_RS_API_TOKEN` binds loopback only.** This process launches real
//! Chrome and fetches arbitrary URLs on the operator's network and with the
//! operator's cookies; an unauthenticated one reachable from another host is an
//! open proxy. [`ServerConfig::effective_bind`] enforces that by rewriting the
//! address, and says so in the log rather than failing silently.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use browser::Proxy;

/// The port `brs-serve` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8080;

/// How long a session may sit unused before it is swept.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(15 * 60);

/// How many live Chrome processes may exist at once.
///
/// Three, matching the semaphore `browser-cdp`'s live tests take permits from:
/// cold-starting six Chromes at once really does fail, and the number that was
/// measured is the number to use.
pub const DEFAULT_MAX_LIVE_BROWSERS: usize = 3;

/// The ceiling on one request, applied by `tower-http`'s timeout layer.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The ceiling on a request body, applied by `tower-http`'s limit layer.
pub const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// How much text one operation may return before it is truncated.
///
/// `API.md` §3: an agent wants a page it can read, not a megabyte of markup,
/// and truncation the model can see beats a response that blows its context.
/// The same cap applies to REST, so the two surfaces cannot disagree.
pub const DEFAULT_MAX_CONTENT_CHARS: usize = 200_000;

/// The ceiling on `max_pages` for one crawl, whatever the request asks for.
pub const DEFAULT_MAX_CRAWL_PAGES: usize = 200;

/// The server's settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The address to listen on. See [`ServerConfig::effective_bind`].
    pub bind: SocketAddr,
    /// The bearer token every `/api` route but `/api/health` requires.
    ///
    /// `None` means "no auth", which forces loopback binding.
    pub api_token: Option<String>,
    /// The directory that holds one persistent Chromium profile per session.
    pub profile_root: PathBuf,
    /// The egress proxy used when a request and its session both name none.
    pub default_proxy: Option<Proxy>,
    /// The impersonation profile used when a request and its session both name
    /// none. `None` leaves `browser-http`'s own default (`"chrome"`) alone.
    pub default_impersonate: Option<String>,
    /// The User-Agent used when a request and its session both name none.
    pub default_user_agent: Option<String>,
    /// The cap on concurrent live browsers.
    pub max_live_browsers: usize,
    /// How long a session may idle before it is swept.
    pub session_idle_ttl: Duration,
    /// The per-request timeout.
    pub request_timeout: Duration,
    /// The maximum request body size.
    pub max_body_bytes: usize,
    /// The truncation point for returned text.
    pub max_content_chars: usize,
    /// The hard ceiling on `max_pages` for a crawl.
    pub max_crawl_pages: usize,
    /// Whether browser sessions launch headless.
    pub headless: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
            api_token: None,
            profile_root: default_profile_root(),
            default_proxy: None,
            default_impersonate: None,
            default_user_agent: None,
            max_live_browsers: DEFAULT_MAX_LIVE_BROWSERS,
            session_idle_ttl: DEFAULT_SESSION_TTL,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_content_chars: DEFAULT_MAX_CONTENT_CHARS,
            max_crawl_pages: DEFAULT_MAX_CRAWL_PAGES,
            headless: true,
        }
    }
}

impl ServerConfig {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the configuration from the environment.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `BROWSER_RS_BIND` | `host:port`, default `127.0.0.1:8080` |
    /// | `BROWSER_RS_API_TOKEN` | the bearer token; unset means loopback-only |
    /// | `BROWSER_RS_PROFILE_ROOT` | where session profiles live |
    /// | `BROWSER_RS_PROXY` | default egress proxy URL |
    /// | `BROWSER_RS_IMPERSONATE` | default fingerprint profile, e.g. `chrome` |
    /// | `BROWSER_RS_USER_AGENT` | default User-Agent |
    /// | `BROWSER_RS_MAX_BROWSERS` | concurrent live browsers, default 3 |
    /// | `BROWSER_RS_SESSION_TTL_SECS` | session idle TTL, default 900 |
    /// | `BROWSER_RS_REQUEST_TIMEOUT_SECS` | per-request timeout, default 120 |
    /// | `BROWSER_RS_MAX_BODY_BYTES` | request body limit, default 2 MiB |
    /// | `BROWSER_RS_MAX_CONTENT_CHARS` | response truncation, default 200 000 |
    /// | `BROWSER_RS_MAX_CRAWL_PAGES` | crawl ceiling, default 200 |
    /// | `BROWSER_RS_HEADLESS` | `0`/`false` launches headful |
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] when a variable is present but unusable. An absent
    /// variable is never an error — every one of them has a default.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::default();

        if let Some(bind) = var("BROWSER_RS_BIND") {
            config.bind = bind
                .parse()
                .map_err(|_| ConfigError::invalid("BROWSER_RS_BIND", &bind))?;
        }
        config.api_token = var("BROWSER_RS_API_TOKEN");
        if let Some(root) = var("BROWSER_RS_PROFILE_ROOT") {
            config.profile_root = PathBuf::from(root);
        }
        config.default_proxy = var("BROWSER_RS_PROXY").map(Proxy::Url);
        config.default_impersonate = var("BROWSER_RS_IMPERSONATE");
        config.default_user_agent = var("BROWSER_RS_USER_AGENT");

        if let Some(value) = var("BROWSER_RS_MAX_BROWSERS") {
            config.max_live_browsers = parse_usize("BROWSER_RS_MAX_BROWSERS", &value)?.max(1);
        }
        if let Some(value) = var("BROWSER_RS_SESSION_TTL_SECS") {
            config.session_idle_ttl =
                Duration::from_secs(parse_u64("BROWSER_RS_SESSION_TTL_SECS", &value)?);
        }
        if let Some(value) = var("BROWSER_RS_REQUEST_TIMEOUT_SECS") {
            config.request_timeout =
                Duration::from_secs(parse_u64("BROWSER_RS_REQUEST_TIMEOUT_SECS", &value)?.max(1));
        }
        if let Some(value) = var("BROWSER_RS_MAX_BODY_BYTES") {
            config.max_body_bytes = parse_usize("BROWSER_RS_MAX_BODY_BYTES", &value)?.max(1);
        }
        if let Some(value) = var("BROWSER_RS_MAX_CONTENT_CHARS") {
            config.max_content_chars = parse_usize("BROWSER_RS_MAX_CONTENT_CHARS", &value)?.max(1);
        }
        if let Some(value) = var("BROWSER_RS_MAX_CRAWL_PAGES") {
            config.max_crawl_pages = parse_usize("BROWSER_RS_MAX_CRAWL_PAGES", &value)?.max(1);
        }
        if let Some(value) = var("BROWSER_RS_HEADLESS") {
            config.headless = !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            );
        }

        Ok(config)
    }

    /// Whether a bearer token is configured at all.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.api_token.is_some()
    }

    /// The address the listener should actually bind.
    ///
    /// Identical to [`ServerConfig::bind`] when a token is set. With no token
    /// the host is rewritten to loopback, keeping the requested port, because
    /// an unauthenticated server that drives a browser must not be reachable
    /// from another machine. The `bool` is `true` when the rewrite happened,
    /// so the caller can log it once instead of this function logging on every
    /// call.
    #[must_use]
    pub fn effective_bind(&self) -> (SocketAddr, bool) {
        if self.is_authenticated() || self.bind.ip().is_loopback() {
            return (self.bind, false);
        }
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.bind.port()),
            true,
        )
    }

    /// The profile directory for a session name.
    ///
    /// The name is sanitised, not merely joined: it arrives from an HTTP body,
    /// and `{"name": "../../etc"}` must not escape the profile root. See
    /// [`sanitize_session_name`].
    #[must_use]
    pub fn profile_dir(&self, session: &str) -> PathBuf {
        self.profile_root.join(sanitize_session_name(session))
    }
}

/// A configuration value that is present but unusable.
#[derive(Debug, thiserror::Error)]
#[error("{variable}: {message}")]
pub struct ConfigError {
    /// The environment variable at fault.
    pub variable: &'static str,
    /// What is wrong with it.
    pub message: String,
}

impl ConfigError {
    fn invalid(variable: &'static str, value: &str) -> Self {
        Self {
            variable,
            message: format!("{value:?} is not a valid value"),
        }
    }
}

/// Turn a session name into one path component that cannot escape its parent.
///
/// Anything outside `[A-Za-z0-9_-]` becomes `_` and the result is capped at 64
/// characters. The dot is **not** in the surviving alphabet, which is the whole
/// trick: with no dots there is no `.` or `..` component to traverse with, and
/// no leading-dot special case to get wrong. The registry keys on the original
/// string, so only the directory is sanitised and nothing user-visible changes.
///
/// # Why the suffix
///
/// Replacing characters makes the mapping many-to-one: `a.b`, `a/b` and `a b`
/// would all land on `a_b`. Two sessions sharing a profile directory is not a
/// cosmetic problem — Chromium takes an exclusive lock on a profile, so the
/// second one simply fails to launch, and the two would share cookies besides.
/// So a name that had to be changed carries an FNV-1a hash of the **original**
/// string, which makes the mapping injective in practice. A name that needed no
/// change keeps its exact spelling, which is the common case and keeps the
/// directory listing readable.
///
/// FNV-1a rather than `DefaultHasher` because this value ends up in a directory
/// name that has to survive a restart *and* a toolchain upgrade;
/// `DefaultHasher`'s output is explicitly not stable across releases.
///
/// ```
/// use browser_server::config::sanitize_session_name;
///
/// // Unchanged names keep their spelling.
/// assert_eq!(sanitize_session_name("shop"), "shop");
/// assert_eq!(sanitize_session_name("shop-eu_2"), "shop-eu_2");
///
/// // Changed ones are disambiguated, and cannot traverse.
/// assert!(sanitize_session_name("../../etc/passwd").starts_with("______etc_passwd-"));
/// assert_ne!(sanitize_session_name("a.b"), sanitize_session_name("a/b"));
/// assert_eq!(sanitize_session_name(""), "unnamed-811c9dc5");
/// ```
#[must_use]
pub fn sanitize_session_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .take(48)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    // Order matters: the empty name is the one case where `out == name` and
    // the result is still unusable as a directory.
    if out.is_empty() {
        return format!("unnamed-{:08x}", fnv1a(name.as_bytes()));
    }
    if out == name {
        return out;
    }
    out.push_str(&format!("-{:08x}", fnv1a(name.as_bytes())));
    out
}

/// FNV-1a, 32-bit. Nine lines, no dependency, and the same answer for ever.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Where session profiles live when `BROWSER_RS_PROFILE_ROOT` is unset.
///
/// `$HOME/.browser-rs/profiles`, falling back to the temp directory on a system
/// with no `HOME`. Persistence across a restart is the whole point of a
/// session, so the temp directory is the fallback and never the default.
fn default_profile_root() -> PathBuf {
    match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join(".browser-rs").join("profiles"),
        None => std::env::temp_dir().join("browser-rs-profiles"),
    }
}

/// An environment variable, treating the empty string as absent.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn parse_usize(variable: &'static str, value: &str) -> Result<usize, ConfigError> {
    value
        .trim()
        .parse()
        .map_err(|_| ConfigError::invalid(variable, value))
}

fn parse_u64(variable: &'static str, value: &str) -> Result<u64, ConfigError> {
    value
        .trim()
        .parse()
        .map_err(|_| ConfigError::invalid(variable, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_loopback_and_unauthenticated() {
        let config = ServerConfig::new();
        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.bind.port(), DEFAULT_PORT);
        assert!(!config.is_authenticated());
    }

    /// The rule from `AGENTS.md`, as an assertion: no token, no public bind.
    #[test]
    fn an_unauthenticated_server_is_forced_onto_loopback() {
        let config = ServerConfig {
            bind: "0.0.0.0:9000".parse().expect("a valid address"),
            api_token: None,
            ..ServerConfig::default()
        };
        let (bind, rewritten) = config.effective_bind();
        assert!(rewritten, "the caller must be told, so it can warn");
        assert!(bind.ip().is_loopback());
        assert_eq!(bind.port(), 9000, "the port the operator chose is kept");
    }

    #[test]
    fn a_token_lets_the_server_bind_where_it_was_told_to() {
        let config = ServerConfig {
            bind: "0.0.0.0:9000".parse().expect("a valid address"),
            api_token: Some("secret".to_owned()),
            ..ServerConfig::default()
        };
        let (bind, rewritten) = config.effective_bind();
        assert!(!rewritten);
        assert_eq!(bind, "0.0.0.0:9000".parse().expect("a valid address"));
    }

    #[test]
    fn a_session_name_cannot_escape_the_profile_root() {
        let config = ServerConfig {
            profile_root: PathBuf::from("/srv/profiles"),
            ..ServerConfig::default()
        };
        let dir = config.profile_dir("../../etc/passwd");
        assert!(dir.starts_with("/srv/profiles"));
        assert_eq!(dir.components().count(), 4, "root + srv + profiles + one");
    }

    #[test]
    fn a_dot_only_name_still_yields_a_usable_component() {
        assert_eq!(sanitize_session_name(".."), "__-a3d4a70d");
        assert_eq!(sanitize_session_name("."), "_-2b0c98f1");
        assert_eq!(sanitize_session_name("..hidden"), "__hidden-8f0f0771");
    }

    /// Sanitising is many-to-one, so two different names must not be able to
    /// claim the same profile directory — Chromium locks a profile, and the
    /// second session would simply fail to launch.
    #[test]
    fn two_names_that_sanitise_alike_get_different_directories() {
        for (left, right) in [("a.b", "a/b"), ("a b", "a-b"), ("x/", "x_")] {
            assert_ne!(
                sanitize_session_name(left),
                sanitize_session_name(right),
                "{left:?} and {right:?} collided"
            );
        }
    }

    /// The hash is in a directory name that has to survive a restart, so it is
    /// pinned, not merely "some hash".
    #[test]
    fn the_disambiguating_hash_is_fnv_1a_and_stays_that_way() {
        assert_eq!(fnv1a(b""), 0x811c_9dc5, "the FNV-1a offset basis");
        assert_eq!(fnv1a(b"a"), 0xe40c_292c, "the published test vector");
        assert_eq!(sanitize_session_name(""), "unnamed-811c9dc5");
    }

    #[test]
    fn a_long_name_is_capped() {
        let name = "a".repeat(200);
        // 48 kept characters, a dash and eight hex digits.
        assert_eq!(sanitize_session_name(&name).len(), 57);
    }
}
