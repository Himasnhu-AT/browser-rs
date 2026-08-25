//! The hand-rolled `follow_redirects="safe"` mode: walk the chain ourselves and
//! reject hops that resolve to private address space.
//!
//! `CURLFOLLOW_SAFE` (=4) is a **curl-impersonate / `curl_cffi` extension**, not
//! stock libcurl, and it is the reference implementation's *default*. `curl_cffi` 0.16.1 documents it
//! as "Follow redirects, but reject redirects to internal/private IP addresses
//! (SSRF protection)". No Rust client implements it, so `ARCHITECTURE.md` §2.3
//! makes it this crate's job: turn the transport's own redirect following **off**,
//! walk the chain here, and check every hop.
//!
//! # What the guard checks, in order, for each hop
//!
//! 1. **Hop budget.** `max_redirects` hops are allowed; `-1` means unlimited and
//!    maps to [`u32::MAX`] ([`redirect_limit`]). `0` refuses every redirect,
//!    exactly like `CURLOPT_MAXREDIRS`.
//! 2. **Scheme.** Only `http` and `https` targets are followed, in **every** mode —
//!    curl's `CURLOPT_REDIR_PROTOCOLS` has defaulted to HTTP+HTTPS since 7.65, so
//!    a `file:`/`gopher:`/`data:` `Location` is refused rather than fetched.
//! 3. **Scheme downgrade** *(`"safe"` only)*. `https:` → `http:` is refused: a
//!    redirect that drops TLS hands the whole exchange, cookies included, to
//!    anyone on the path, and it is the classic way to strip an SSRF guard that
//!    only inspects the first URL.
//! 4. **Address space** *(`"safe"` only, cross-origin hops only)*. The target
//!    host is resolved and every address it resolves to is tested against
//!    [`blocked_range`]. One private address is enough to refuse the hop — a name
//!    that resolves to both a public and a loopback address is a DNS-rebinding
//!    attack, not a coincidence.
//! 5. **Post-flight address** *(`"safe"` only, cross-origin hops only)*. When the
//!    backend reports the address it actually connected to, that address is
//!    re-checked *after* the hop completes. Steps 4 and 5 together shrink, but
//!    cannot fully close, the check-then-connect window; see
//!    [the TOCTOU note](#dns-rebinding-and-toctou).
//!
//! # Why a same-origin hop is exempt
//!
//! Steps 4 and 5 run **only when the hop changes origin**. That is not a
//! relaxation invented here: it is what `curl_cffi` does. `CURLFOLLOW_SAFE` is
//! implemented inside curl's connection setup, so a redirect that reuses the
//! open connection is never checked at all. Measured against the reference
//! implementation (`tools/corpus/u12`), `Fetcher.get("http://127.0.0.1:8000/r")`
//! with the default `follow_redirects="safe"` follows a same-host `Location`
//! happily, and refuses the same hop the moment the host **or the port** changes
//! — including `127.0.0.1:8000` -> `127.0.0.1:8001` and
//! `127.0.0.1:8000` -> `localhost:8000`.
//!
//! Reproducing curl exactly would mean observing connection reuse, which a port
//! that shares a pool between requests cannot do; an origin comparison is the
//! closest deterministic equivalent, and it keeps the security property that
//! matters — a redirect can never *pivot* to an address the caller had not
//! already reached. The one measured difference is a same-origin hop over a
//! `Connection: close` reply, which curl checks (it must dial again) and this
//! guard does not; it is a declared corpus divergence.
//!
//! Independently of the mode, **credentials are stripped on a cross-origin hop**
//! (see [`SENSITIVE_HEADERS`], plus [`WireRequest::auth`]) — the origin
//! comparison is scheme + host + port, and a port-only change counts. Cookies are
//! deliberately *not* in that set; see the constant's own documentation.
//!
//! # DNS rebinding and TOCTOU
//!
//! The guard resolves the host, then the transport resolves it again when it
//! opens the connection. An attacker controlling the authoritative name server
//! can answer differently the second time. Step 5 above catches that *after* the
//! response arrives but *before* the body is handed to the caller, which is the
//! strongest guarantee available without pinning the resolved address into the
//! connection — and pinning is impossible here, because the connection pool is
//! shared between requests by design. This is a deliberate, documented limit of
//! the port; `curl_cffi`'s own implementation has the same shape.
//!
//! # Method and body across a hop
//!
//! `curl_cffi` sends `POST` through `CURLOPT_POST` and every other method through
//! `CURLOPT_CUSTOMREQUEST`, which is what makes the three "custom method" modes
//! differ. [`next_method`] reproduces the resulting table.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use async_trait::async_trait;
use browser_core::types::{FollowRedirects, RedirectMode};
use url::Url;

use crate::backend::{HttpBackend, Method, WireRequest, WireResponse};
use crate::config::HeaderDict;
use crate::error::{FetchError, TransportError};

/// The status codes that carry a `Location` worth following.
///
/// `curl_cffi.requests.models.REDIRECT_STATI`, verbatim.
pub const REDIRECT_STATI: [u16; 5] = [301, 302, 303, 307, 308];

/// Headers dropped when a redirect crosses to another origin.
///
/// A `Location` pointing at another origin must not carry the credentials the
/// caller meant for the first one; [`WireRequest::auth`] is cleared alongside
/// them.
///
/// `Cookie` is **not** in this list, and that is measured behaviour rather than
/// an oversight: cookies are scoped by the jar's domain and path rules, not by
/// origin, so curl sends a `127.0.0.1` cookie to `127.0.0.1:9999` (cookies ignore
/// ports) and never sends it to another host at all.
/// [`crate::session::CookieJar`] re-derives the `Cookie` header from the jar on
/// every hop, which reproduces both halves; blanket-stripping the header here
/// would only break the first.
pub const SENSITIVE_HEADERS: [&str; 3] =
    ["authorization", "proxy-authorization", "www-authenticate"];

/// One completed redirect hop.
///
/// Python leaks raw `curl_cffi` `Response` objects into `Response.history`
/// (upstream issue `lexiforest/curl_cffi#82` is quoted in the source); a port gets
/// to define a proper struct. Redirect **bodies are genuinely unavailable** — curl
/// discards them — so there is no `body` field, matching the Python objects'
/// behaviour rather than their type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectHop {
    /// The URL that produced this redirect.
    pub url: String,
    /// Its status code.
    pub status: u16,
    /// Its reason phrase.
    pub reason: String,
    /// Its response headers.
    pub headers: HeaderDict,
    /// The `Location` it pointed at, already resolved to an absolute URL.
    pub location: String,
}

impl std::fmt::Display for RedirectHop {
    /// The same `<{status} {url}>` shape `Response` uses.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} {}>", self.status, self.url)
    }
}

/// Turn `max_redirects` into a hop budget.
///
/// `-1` means unlimited, which `ARCHITECTURE.md` §2.3 pins to [`u32::MAX`]. Any
/// other negative is treated the same way, as `CURLOPT_MAXREDIRS` does.
///
/// # Examples
///
/// ```
/// use browser_http::redirect_safe::redirect_limit;
///
/// assert_eq!(redirect_limit(30), 30);          // the default
/// assert_eq!(redirect_limit(0), 0);            // refuse every redirect
/// assert_eq!(redirect_limit(-1), u32::MAX);    // unlimited
/// assert_eq!(redirect_limit(i32::MIN), u32::MAX);
/// ```
#[must_use]
pub const fn redirect_limit(max_redirects: i32) -> u32 {
    if max_redirects < 0 {
        u32::MAX
    } else {
        max_redirects as u32
    }
}

/// Whether a policy follows redirects at all.
///
/// `False` is `CURLFOLLOW_*` 0; everything else follows.
#[must_use]
pub const fn follows(policy: FollowRedirects) -> bool {
    !matches!(policy, FollowRedirects::Bool(false))
}

/// Whether a policy runs the SSRF guard.
#[must_use]
pub const fn is_safe_mode(policy: FollowRedirects) -> bool {
    matches!(policy, FollowRedirects::Mode(RedirectMode::Safe))
}

/// The method the next hop uses, and whether the body survives.
///
/// | status | `all` / `safe` | `obeycode` | `firstonly` |
/// |---|---|---|---|
/// | 301/302/303 after `POST` | `GET`, body dropped | `GET`, body dropped | `GET`, body dropped |
/// | 301/302/303 after `PUT`/`DELETE` | method kept, **body dropped** | `GET`, body dropped | `GET` from hop 2 |
/// | 307/308 | unchanged, body kept | unchanged, body kept | `GET` from hop 2 |
///
/// A 301/302/303 always drops the payload — libcurl clears it whenever it
/// rewrites the request — while the `Content-Type` the payload implied stays on
/// the wire. Both halves are pinned by `tools/corpus/u12`.
///
/// `POST` is the odd one out because `curl_cffi` sends it through
/// `CURLOPT_POST` rather than `CURLOPT_CUSTOMREQUEST`, so it is not a "custom
/// method" and libcurl's default `CURLOPT_POSTREDIR` (rewrite to `GET` on
/// 301/302/303) applies in every mode. `GET` never changes.
///
/// `hop` is 0-based: hop 0 is the first redirect being followed.
///
/// # Examples
///
/// ```
/// use browser_core::types::{FollowRedirects, RedirectMode};
/// use browser_http::backend::Method;
/// use browser_http::redirect_safe::next_method;
///
/// let all = FollowRedirects::Mode(RedirectMode::All);
/// let obey = FollowRedirects::Mode(RedirectMode::ObeyCode);
///
/// // A custom method survives 302 under `all` — but its body never does.
/// assert_eq!(next_method(Method::Put, 302, all, 0), (Method::Put, false));
/// // …and the method itself does not survive under `obeycode`.
/// assert_eq!(next_method(Method::Put, 302, obey, 0), (Method::Get, false));
/// // 307 preserves both.
/// assert_eq!(next_method(Method::Put, 307, all, 0), (Method::Put, true));
/// // POST always becomes GET on 303, and keeps its body on 307.
/// assert_eq!(next_method(Method::Post, 303, all, 0), (Method::Get, false));
/// assert_eq!(next_method(Method::Post, 307, all, 0), (Method::Post, true));
/// ```
#[must_use]
pub fn next_method(
    method: Method,
    status: u16,
    policy: FollowRedirects,
    hop: u32,
) -> (Method, bool) {
    let rewrites_to_get = matches!(status, 301..=303);

    // `firstonly` resets the custom method from the *second* request onwards,
    // whatever the status code says.
    if matches!(policy, FollowRedirects::Mode(RedirectMode::FirstOnly)) && hop >= 1 {
        return (Method::Get, false);
    }

    match method {
        // GET has no body and no rewrite to perform.
        Method::Get => (Method::Get, false),
        // POST is `CURLOPT_POST`, never a custom method: libcurl's default
        // `CURLOPT_POSTREDIR` rewrites it to GET on 301/302/303 in every mode.
        Method::Post if rewrites_to_get => (Method::Get, false),
        Method::Post => (Method::Post, true),
        // PUT and DELETE go through `CURLOPT_CUSTOMREQUEST`, so the mode decides.
        other => {
            let keep_custom = match policy {
                FollowRedirects::Bool(_) => true,
                FollowRedirects::Mode(RedirectMode::All | RedirectMode::Safe) => true,
                FollowRedirects::Mode(RedirectMode::ObeyCode) => !rewrites_to_get,
                // `firstonly` at hop 0 still uses the custom method.
                FollowRedirects::Mode(RedirectMode::FirstOnly) => true,
            };
            if keep_custom {
                // The *method* survives a 301/302/303; the **body never does**.
                // Measured: `PUT /redirect/303 -> /end/echo` with `data=` arrives
                // as a bodiless PUT (`tools/corpus/u12`, `redirect-303echo-put`).
                // libcurl clears the payload whenever it rewrites the request,
                // and only 307/308 preserve it.
                (other, !rewrites_to_get)
            } else {
                (Method::Get, false)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Address-space classification
// ---------------------------------------------------------------------------

/// Name the blocked range `ip` falls in, or `None` when it is publicly routable.
///
/// The ranges `ARCHITECTURE.md` §2.3 pins are `10/8`, `172.16/12`, `192.168/16`,
/// `127/8`, `169.254/16`, `100.64/10`, `0.0.0.0/8`, `::1`, `fc00::/7` and
/// `fe80::/10`. Four more non-routable ranges are added — IPv4 multicast, the
/// limited broadcast address, `198.18/15` (benchmarking) and the unspecified IPv6
/// address — because a redirect to any of them is never a legitimate fetch and
/// leaving them out would be a gap, not a fidelity win.
///
/// IPv4-mapped and IPv4-compatible IPv6 addresses are unwrapped first, so
/// `::ffff:127.0.0.1` is caught as loopback rather than waved through as "some
/// IPv6 address".
///
/// # Examples
///
/// ```
/// use std::net::IpAddr;
/// use browser_http::redirect_safe::blocked_range;
///
/// assert_eq!(blocked_range("127.0.0.1".parse::<IpAddr>()?), Some("loopback (127.0.0.0/8)"));
/// assert_eq!(blocked_range("169.254.169.254".parse::<IpAddr>()?), Some("link-local (169.254.0.0/16)"));
/// assert_eq!(blocked_range("::ffff:10.0.0.1".parse::<IpAddr>()?), Some("private (10.0.0.0/8)"));
/// assert_eq!(blocked_range("93.184.216.34".parse::<IpAddr>()?), None);
/// # Ok::<(), std::net::AddrParseError>(())
/// ```
#[must_use]
pub fn blocked_range(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_range_v4(v4),
        IpAddr::V6(v6) => {
            // `::ffff:a.b.c.d` and the deprecated `::a.b.c.d` are IPv4 in
            // disguise; classify them as IPv4 or the guard is trivially bypassed.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_range_v4(v4);
            }
            if let Some(v4) = ipv4_compatible(v6) {
                return blocked_range_v4(v4);
            }
            blocked_range_v6(v6)
        }
    }
}

/// `::a.b.c.d`, the deprecated IPv4-compatible form, excluding `::` and `::1`.
fn ipv4_compatible(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = v6.segments();
    if segments[..6] != [0, 0, 0, 0, 0, 0] {
        return None;
    }
    let tail = (u32::from(segments[6]) << 16) | u32::from(segments[7]);
    // `::` and `::1` are their own things, handled by `blocked_range_v6`.
    if tail <= 1 {
        return None;
    }
    Some(Ipv4Addr::from(tail))
}

fn blocked_range_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, _, _] = ip.octets();
    match (a, b) {
        (0, _) => Some("unspecified / this-network (0.0.0.0/8)"),
        (10, _) => Some("private (10.0.0.0/8)"),
        (100, 64..=127) => Some("carrier-grade NAT (100.64.0.0/10)"),
        (127, _) => Some("loopback (127.0.0.0/8)"),
        (169, 254) => Some("link-local (169.254.0.0/16)"),
        (172, 16..=31) => Some("private (172.16.0.0/12)"),
        (192, 168) => Some("private (192.168.0.0/16)"),
        (198, 18 | 19) => Some("benchmarking (198.18.0.0/15)"),
        (224..=239, _) => Some("multicast (224.0.0.0/4)"),
        (240..=255, _) if ip == Ipv4Addr::BROADCAST => Some("broadcast (255.255.255.255)"),
        _ => None,
    }
}

fn blocked_range_v6(ip: Ipv6Addr) -> Option<&'static str> {
    if ip == Ipv6Addr::LOCALHOST {
        return Some("loopback (::1)");
    }
    if ip == Ipv6Addr::UNSPECIFIED {
        return Some("unspecified (::)");
    }
    let first = ip.segments()[0];
    if first & 0xfe00 == 0xfc00 {
        return Some("unique local (fc00::/7)");
    }
    if first & 0xffc0 == 0xfe80 {
        return Some("link-local (fe80::/10)");
    }
    if first & 0xff00 == 0xff00 {
        return Some("multicast (ff00::/8)");
    }
    None
}

// ---------------------------------------------------------------------------
// Host resolution
// ---------------------------------------------------------------------------

/// Resolves a host name to addresses so the guard can classify them.
///
/// Injectable so the guard is testable without a network — and so an application
/// with its own resolver (or its own allow-list) can supply it.
#[async_trait]
pub trait HostResolver: Send + Sync {
    /// Resolve `host` to every address it currently answers with.
    ///
    /// # Errors
    ///
    /// A [`TransportError::Dns`] when the name does not resolve. The guard turns
    /// that into a refusal, never into a silent pass.
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, TransportError>;
}

/// The platform resolver, run on `tokio`'s blocking pool.
///
/// `std::net::ToSocketAddrs` is synchronous, and `tokio::net` is deliberately not
/// a dependency of this crate (`ARCHITECTURE.md` §5.1 keeps the feature set
/// minimal), so the lookup goes through `spawn_blocking`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, TransportError> {
        // A literal never needs DNS — and must not, or a hostile resolver could
        // be asked about `127.0.0.1`.
        if let Some(ip) = parse_host_literal(host) {
            return Ok(vec![ip]);
        }

        let owned = host.to_owned();
        let joined = tokio::task::spawn_blocking(move || {
            (owned.as_str(), 0u16)
                .to_socket_addrs()
                .map(|addrs| addrs.map(|addr: SocketAddr| addr.ip()).collect::<Vec<_>>())
        })
        .await;

        match joined {
            Ok(Ok(addresses)) => Ok(addresses),
            Ok(Err(err)) => Err(TransportError::dns(format!(
                "Could not resolve host: {host} ({err})"
            ))),
            Err(err) => Err(TransportError::dns(format!(
                "Could not resolve host: {host} ({err})"
            ))),
        }
    }
}

/// Parse a host that is already an IP literal, with or without IPv6 brackets.
#[must_use]
pub fn parse_host_literal(host: &str) -> Option<IpAddr> {
    let trimmed = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    trimmed.parse::<IpAddr>().ok()
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// The redirect policy for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedirectPolicy {
    /// `follow_redirects` / `allow_redirects`.
    pub mode: FollowRedirects,
    /// The hop budget; `-1` is unlimited.
    pub max_redirects: i32,
}

impl Default for RedirectPolicy {
    fn default() -> Self {
        Self {
            mode: crate::config::default_follow_redirects(),
            max_redirects: crate::config::DEFAULT_MAX_REDIRECTS,
        }
    }
}

impl RedirectPolicy {
    /// Build a policy.
    #[must_use]
    pub const fn new(mode: FollowRedirects, max_redirects: i32) -> Self {
        Self {
            mode,
            max_redirects,
        }
    }

    /// The hop budget as an unsigned count.
    #[must_use]
    pub const fn limit(self) -> u32 {
        redirect_limit(self.max_redirects)
    }

    /// Whether redirects are followed at all.
    #[must_use]
    pub const fn follows(self) -> bool {
        follows(self.mode)
    }

    /// Whether the SSRF guard runs.
    #[must_use]
    pub const fn is_safe(self) -> bool {
        is_safe_mode(self.mode)
    }
}

/// Refuse `url` when `"safe"` mode disallows it.
///
/// Split out of the follower so an application can run the same check on a URL it
/// is about to hand to some other subsystem.
///
/// # Errors
///
/// * [`FetchError::UnsupportedRedirectScheme`] for a non-HTTP(S) target;
/// * [`FetchError::UnsafeRedirect`] for a scheme downgrade or a private address;
/// * [`FetchError::Transport`] when the host does not resolve.
pub async fn check_target(
    resolver: &dyn HostResolver,
    from: &Url,
    to: &Url,
    safe_mode: bool,
) -> Result<(), FetchError> {
    // Rule 2 — every mode. curl's REDIR_PROTOCOLS default is HTTP + HTTPS.
    if !matches!(to.scheme(), "http" | "https") {
        return Err(FetchError::UnsupportedRedirectScheme {
            scheme: to.scheme().to_owned(),
            url: to.to_string(),
        });
    }

    if !safe_mode {
        return Ok(());
    }

    // Rule 2a — a hop that stays on the same origin reaches nothing the caller
    // has not already reached. curl-impersonate expresses this as "the check
    // runs when the redirect needs a NEW connection"; an origin comparison is
    // the closest a port that does not own the connection pool can get, and it
    // is what keeps `follow_redirects="safe"` — the the reference implementation default — usable
    // against a host that is itself private, exactly as Python is. See
    // `docs/DIVERGENCES.md`.
    if same_origin(from, to) {
        return Ok(());
    }

    // Rule 3 — a TLS downgrade discards every guarantee the first hop had.
    if from.scheme() == "https" && to.scheme() == "http" {
        return Err(FetchError::UnsafeRedirect {
            url: to.to_string(),
            reason: "scheme downgrade https -> http".to_owned(),
        });
    }

    // Rule 4 — resolve and classify.
    let Some(host) = to.host_str() else {
        return Err(FetchError::UnsafeRedirect {
            url: to.to_string(),
            reason: "no host to resolve".to_owned(),
        });
    };
    let addresses = resolver.resolve(host).await?;
    if addresses.is_empty() {
        return Err(FetchError::UnsafeRedirect {
            url: to.to_string(),
            reason: format!("host {host} resolved to no addresses"),
        });
    }
    // *Any* private answer refuses the hop: a name answering with both a public
    // and a loopback address is a rebinding attack, not a coincidence.
    for ip in addresses {
        if let Some(range) = blocked_range(ip) {
            return Err(FetchError::UnsafeRedirect {
                url: to.to_string(),
                reason: format!("{host} resolves to {ip}, which is {range}"),
            });
        }
    }
    Ok(())
}

/// Re-check the address a hop actually connected to (rule 5).
///
/// # Errors
///
/// [`FetchError::UnsafeRedirect`] when the peer turned out to be private after
/// all.
pub fn check_peer(url: &Url, peer: Option<SocketAddr>, safe_mode: bool) -> Result<(), FetchError> {
    if !safe_mode {
        return Ok(());
    }
    let Some(peer) = peer else { return Ok(()) };
    if let Some(range) = blocked_range(peer.ip()) {
        return Err(FetchError::UnsafeRedirect {
            url: url.to_string(),
            reason: format!(
                "connected to {}, which is {range} (DNS rebinding?)",
                peer.ip()
            ),
        });
    }
    Ok(())
}

/// Whether two URLs share an origin, i.e. scheme **and** host **and** port.
#[must_use]
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Drop [`SENSITIVE_HEADERS`] from `headers`, case-insensitively.
///
/// Returns the names that were removed, so the caller can log them.
pub fn strip_sensitive_headers(headers: &mut HeaderDict) -> Vec<String> {
    let doomed: Vec<String> = headers
        .keys()
        .filter(|name| {
            SENSITIVE_HEADERS
                .iter()
                .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
        })
        .cloned()
        .collect();
    for name in &doomed {
        headers.shift_remove(name);
    }
    doomed
}

/// Called around each hop, so a session can apply and harvest its cookie jar
/// without the follower knowing what a cookie is.
pub trait HopObserver: Send + Sync {
    /// Mutate the request just before it goes out.
    fn before_send(&self, request: &mut WireRequest) {
        let _ = request;
    }

    /// Inspect the reply just after it comes back.
    fn after_receive(&self, request: &WireRequest, response: &WireResponse) {
        let _ = (request, response);
    }
}

/// The no-op observer.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoObserver;

impl HopObserver for NoObserver {}

/// The outcome of walking one redirect chain.
#[derive(Debug)]
pub struct FollowOutcome {
    /// The final reply.
    pub response: WireResponse,
    /// Every hop before it, oldest first.
    pub history: Vec<RedirectHop>,
    /// The headers the **final** request actually carried.
    pub request_headers: HeaderDict,
    /// The method the final request used.
    pub method: Method,
}

/// Walk a redirect chain, applying the policy at every hop.
///
/// The transport's own redirect following must be off; the follower is the only
/// thing that moves between hops, which is what makes `"safe"` implementable and
/// what gives [`RedirectHop`] its per-hop headers.
///
/// # Errors
///
/// Anything [`check_target`] raises, [`FetchError::TooManyRedirects`] when the
/// budget runs out, [`FetchError::InvalidRedirect`] for an unresolvable
/// `Location`, and any [`FetchError::Transport`] the backend produces.
pub async fn follow_redirects(
    backend: &dyn HttpBackend,
    resolver: &dyn HostResolver,
    observer: &dyn HopObserver,
    mut request: WireRequest,
    policy: RedirectPolicy,
) -> Result<FollowOutcome, FetchError> {
    let limit = policy.limit();
    let safe_mode = policy.is_safe();
    let mut history: Vec<RedirectHop> = Vec::new();
    let mut hop: u32 = 0;
    // Whether the hop about to be sent changed origin. Only such a hop opens a
    // connection to somewhere new, so only such a hop is worth re-checking
    // against the address the transport actually reached.
    let mut changed_origin = false;

    loop {
        observer.before_send(&mut request);
        let sent_headers = request.headers.clone();
        let sent_method = request.method;
        let current_url = request.url.clone();

        let response = backend.execute(request.clone()).await?;
        observer.after_receive(&request, &response);

        // Rule 5: only for hops we followed, never for the URL the caller asked
        // for. `"safe"` guards *redirect targets*, exactly as `curl_cffi`
        // documents it — fetching `http://localhost/` directly stays legal.
        if hop > 0 && changed_origin {
            check_peer(&current_url, response.remote_addr, safe_mode)?;
        }

        let is_redirect = REDIRECT_STATI.contains(&response.status);
        if !is_redirect || !policy.follows() {
            return Ok(FollowOutcome {
                response,
                history,
                request_headers: sent_headers,
                method: sent_method,
            });
        }

        let Some(location) = header_value(&response.headers, "location") else {
            // A 3xx with no `Location` is a normal response, not a chain.
            return Ok(FollowOutcome {
                response,
                history,
                request_headers: sent_headers,
                method: sent_method,
            });
        };
        let location = location.to_owned();

        if hop >= limit {
            return Err(FetchError::TooManyRedirects {
                max: limit,
                url: current_url.to_string(),
            });
        }

        let target = current_url
            .join(&location)
            .map_err(|source| FetchError::InvalidRedirect {
                location: location.clone(),
                base: current_url.to_string(),
                source,
            })?;

        check_target(resolver, &current_url, &target, safe_mode).await?;

        history.push(RedirectHop {
            url: current_url.to_string(),
            status: response.status,
            reason: response.reason.clone(),
            headers: response.headers.clone(),
            location: target.to_string(),
        });

        let (method, keep_body) = next_method(sent_method, response.status, policy.mode, hop);
        let cross_origin = !same_origin(&current_url, &target);

        request.url = target.clone();
        request.method = method;
        if !keep_body {
            // curl drops the payload but leaves the request headers alone, so
            // the `Content-Type` the payload implied is still sent on the next
            // hop. Materialise it before the body goes, or the backend — which
            // derives the header from the body — would silently lose it.
            if let Some(body) = &request.body {
                if let Some(content_type) = crate::backend::implied_content_type(body) {
                    if !request
                        .headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("content-type"))
                    {
                        request
                            .headers
                            .insert("Content-Type".to_owned(), content_type.to_owned());
                    }
                }
            }
            request.body = None;
        }
        // The query string was already baked into `url` for the first hop; a
        // `Location` carries its own, so `params` must not be re-appended.
        request.params = None;
        changed_origin = cross_origin;
        if cross_origin {
            // Credentials the caller pinned to the first origin never follow it
            // elsewhere. Cookies are **not** in this set: they are governed by
            // the cookie jar's domain/path rules, which is what curl does too —
            // a cookie set for `127.0.0.1` reaches `127.0.0.1:9999` because
            // cookies ignore ports, and a cookie for another host never leaves
            // it. `crate::session::JarObserver` re-derives the `Cookie` header
            // from the jar on every hop.
            request.auth = None;
            let dropped = strip_sensitive_headers(&mut request.headers);
            if !dropped.is_empty() {
                tracing::debug!(
                    from = %current_url,
                    to = %target,
                    dropped = ?dropped,
                    "stripped credentials on a cross-origin redirect"
                );
            }
        }

        tracing::debug!(
            from = %current_url,
            to = %target,
            status = response.status,
            method = method.as_str(),
            "following redirect"
        );
        hop += 1;
    }
}

/// Case-insensitive header lookup over an ordered, case-preserving map.
#[must_use]
pub fn header_value<'a>(headers: &'a HeaderDict, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::backend::mock::{MockBackend, MockReply};

    struct StubResolver(HashMap<String, Vec<IpAddr>>);

    impl StubResolver {
        fn new<const N: usize>(entries: [(&str, &str); N]) -> Self {
            let mut map = HashMap::new();
            for (host, ips) in entries {
                map.insert(
                    host.to_owned(),
                    ips.split(',')
                        .map(|ip| ip.parse().expect("test ip"))
                        .collect(),
                );
            }
            Self(map)
        }
    }

    #[async_trait]
    impl HostResolver for StubResolver {
        async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, TransportError> {
            if let Some(ip) = parse_host_literal(host) {
                return Ok(vec![ip]);
            }
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| TransportError::dns(format!("Could not resolve host: {host}")))
        }
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test url")
    }

    #[test]
    fn the_pinned_ranges_are_all_blocked() {
        for (ip, expected) in [
            ("0.0.0.0", "unspecified / this-network (0.0.0.0/8)"),
            ("10.1.2.3", "private (10.0.0.0/8)"),
            ("100.64.0.1", "carrier-grade NAT (100.64.0.0/10)"),
            ("100.127.255.255", "carrier-grade NAT (100.64.0.0/10)"),
            ("127.0.0.1", "loopback (127.0.0.0/8)"),
            ("169.254.169.254", "link-local (169.254.0.0/16)"),
            ("172.16.0.1", "private (172.16.0.0/12)"),
            ("172.31.255.254", "private (172.16.0.0/12)"),
            ("192.168.1.1", "private (192.168.0.0/16)"),
            ("::1", "loopback (::1)"),
            ("fc00::1", "unique local (fc00::/7)"),
            ("fd12:3456::1", "unique local (fc00::/7)"),
            ("fe80::1", "link-local (fe80::/10)"),
        ] {
            assert_eq!(
                blocked_range(ip.parse().expect("ip")),
                Some(expected),
                "{ip}"
            );
        }
    }

    #[test]
    fn the_public_neighbours_of_each_range_are_not_blocked() {
        // The addresses one step outside every boundary must stay reachable, or
        // the guard is a denial of service.
        for ip in [
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.1",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "8.8.8.8",
            "93.184.216.34",
            "2606:2800:220:1:248:1893:25c8:1946",
            "fb00::1",
            "fec0::1",
        ] {
            assert_eq!(blocked_range(ip.parse().expect("ip")), None, "{ip}");
        }
    }

    #[test]
    fn ipv4_in_ipv6_clothing_does_not_slip_through() {
        assert_eq!(
            blocked_range("::ffff:127.0.0.1".parse().expect("ip")),
            Some("loopback (127.0.0.0/8)")
        );
        assert_eq!(
            blocked_range("::ffff:169.254.169.254".parse().expect("ip")),
            Some("link-local (169.254.0.0/16)")
        );
        // The deprecated `::a.b.c.d` form too.
        assert_eq!(
            blocked_range("::10.0.0.1".parse().expect("ip")),
            Some("private (10.0.0.0/8)")
        );
        // …and a genuinely public mapped address is still allowed.
        assert_eq!(blocked_range("::ffff:8.8.8.8".parse().expect("ip")), None);
    }

    #[test]
    fn a_bracketed_ipv6_literal_needs_no_dns() {
        assert_eq!(
            parse_host_literal("[::1]"),
            Some("::1".parse::<IpAddr>().expect("ip"))
        );
        assert_eq!(
            parse_host_literal("127.0.0.1"),
            Some("127.0.0.1".parse::<IpAddr>().expect("ip"))
        );
        assert_eq!(parse_host_literal("example.com"), None);
    }

    #[test]
    fn the_hop_budget_maps_minus_one_to_u32_max() {
        assert_eq!(RedirectPolicy::default().limit(), 30);
        assert_eq!(
            RedirectPolicy::new(FollowRedirects::Bool(true), -1).limit(),
            u32::MAX
        );
        assert_eq!(
            RedirectPolicy::new(FollowRedirects::Bool(true), 0).limit(),
            0
        );
    }

    #[test]
    fn the_method_table_matches_curls_three_custom_method_modes() {
        let all = FollowRedirects::Mode(RedirectMode::All);
        let safe = FollowRedirects::Mode(RedirectMode::Safe);
        let obey = FollowRedirects::Mode(RedirectMode::ObeyCode);
        let first = FollowRedirects::Mode(RedirectMode::FirstOnly);

        for status in [301, 302, 303] {
            // POST is `CURLOPT_POST`, so it is rewritten in every mode.
            for mode in [all, safe, obey, first] {
                assert_eq!(
                    next_method(Method::Post, status, mode, 0),
                    (Method::Get, false)
                );
            }
            // PUT/DELETE are custom methods: the method survives, the body
            // does not — 301/302/303 always clear the payload.
            assert_eq!(
                next_method(Method::Put, status, all, 0),
                (Method::Put, false)
            );
            assert_eq!(
                next_method(Method::Delete, status, safe, 0),
                (Method::Delete, false)
            );
            assert_eq!(
                next_method(Method::Put, status, obey, 0),
                (Method::Get, false)
            );
            assert_eq!(
                next_method(Method::Put, status, first, 0),
                (Method::Put, false)
            );
            assert_eq!(
                next_method(Method::Put, status, first, 1),
                (Method::Get, false)
            );
        }
        for status in [307, 308] {
            assert_eq!(
                next_method(Method::Post, status, all, 0),
                (Method::Post, true)
            );
            assert_eq!(
                next_method(Method::Put, status, obey, 0),
                (Method::Put, true)
            );
            assert_eq!(
                next_method(Method::Put, status, first, 1),
                (Method::Get, false)
            );
        }
        assert_eq!(next_method(Method::Get, 302, all, 0), (Method::Get, false));
    }

    #[test]
    fn origins_compare_on_scheme_host_and_port() {
        assert!(same_origin(
            &url("https://a.test/x"),
            &url("https://a.test/y")
        ));
        assert!(same_origin(
            &url("https://a.test/x"),
            &url("https://a.test:443/y")
        ));
        assert!(!same_origin(
            &url("https://a.test/"),
            &url("http://a.test/")
        ));
        assert!(!same_origin(
            &url("https://a.test/"),
            &url("https://b.test/")
        ));
        assert!(!same_origin(
            &url("https://a.test/"),
            &url("https://a.test:8443/")
        ));
    }

    #[test]
    fn sensitive_headers_are_dropped_case_insensitively() {
        let mut headers = HeaderDict::new();
        headers.insert("Authorization".to_owned(), "Bearer t".to_owned());
        headers.insert("COOKIE".to_owned(), "a=b".to_owned());
        headers.insert("Proxy-Authorization".to_owned(), "Basic x".to_owned());
        headers.insert("X-Keep".to_owned(), "1".to_owned());

        let dropped = strip_sensitive_headers(&mut headers);
        // `Cookie` is deliberately not in the set — the jar decides which
        // cookies a hop carries, and curl sends a host's cookies to that host
        // on whatever port. See `SENSITIVE_HEADERS`.
        assert_eq!(dropped.len(), 2);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("X-Keep").map(String::as_str), Some("1"));
        assert_eq!(headers.get("COOKIE").map(String::as_str), Some("a=b"));
    }

    #[tokio::test]
    async fn safe_mode_refuses_a_private_target_and_names_the_range() {
        let resolver = StubResolver::new([("evil.test", "169.254.169.254")]);
        let err = check_target(
            &resolver,
            &url("https://public.test/"),
            &url("https://evil.test/latest/meta-data/"),
            true,
        )
        .await
        .expect_err("must refuse");
        match err {
            FetchError::UnsafeRedirect { url, reason } => {
                assert_eq!(url, "https://evil.test/latest/meta-data/");
                assert!(reason.contains("link-local (169.254.0.0/16)"), "{reason}");
            }
            other => panic!("wrong error: {other}"),
        }

        // …and the same target is fine when the guard is off.
        check_target(
            &resolver,
            &url("https://public.test/"),
            &url("https://evil.test/"),
            false,
        )
        .await
        .expect("no guard, no refusal");
    }

    #[tokio::test]
    async fn safe_mode_refuses_a_scheme_downgrade_and_a_foreign_scheme() {
        let resolver = StubResolver::new([("a.test", "93.184.216.34")]);
        let err = check_target(
            &resolver,
            &url("https://a.test/"),
            &url("http://a.test/"),
            true,
        )
        .await
        .expect_err("downgrade");
        assert!(
            matches!(err, FetchError::UnsafeRedirect { ref reason, .. } if reason.contains("downgrade"))
        );

        // An upgrade is fine.
        check_target(
            &resolver,
            &url("http://a.test/"),
            &url("https://a.test/"),
            true,
        )
        .await
        .expect("upgrades are safe");

        // A non-HTTP scheme is refused in every mode, safe or not.
        for safe in [true, false] {
            let err = check_target(
                &resolver,
                &url("https://a.test/"),
                &url("file:///etc/passwd"),
                safe,
            )
            .await
            .expect_err("scheme");
            assert!(matches!(err, FetchError::UnsupportedRedirectScheme { .. }));
        }
    }

    #[tokio::test]
    async fn one_private_answer_among_many_refuses_the_hop() {
        // The DNS-rebinding shape: a public A record next to a loopback one.
        let resolver = StubResolver::new([("rebind.test", "93.184.216.34,127.0.0.1")]);
        let err = check_target(
            &resolver,
            &url("https://a.test/"),
            &url("https://rebind.test/"),
            true,
        )
        .await
        .expect_err("must refuse");
        assert!(
            matches!(err, FetchError::UnsafeRedirect { ref reason, .. } if reason.contains("loopback"))
        );
    }

    #[tokio::test]
    async fn an_unresolvable_host_is_a_transport_error_not_a_pass() {
        let resolver = StubResolver::new([]);
        let err = check_target(
            &resolver,
            &url("https://a.test/"),
            &url("https://nowhere.invalid/"),
            true,
        )
        .await
        .expect_err("must fail closed");
        assert!(matches!(err, FetchError::Transport(TransportError::Dns(_))));
    }

    #[test]
    fn a_post_flight_private_peer_is_caught() {
        let peer: SocketAddr = "127.0.0.1:8080".parse().expect("addr");
        let err = check_peer(&url("https://a.test/"), Some(peer), true).expect_err("rebind");
        assert!(
            matches!(err, FetchError::UnsafeRedirect { ref reason, .. } if reason.contains("rebinding"))
        );
        // Off in the other modes, and a public peer is fine.
        check_peer(&url("https://a.test/"), Some(peer), false).expect("guard off");
        let public: SocketAddr = "93.184.216.34:443".parse().expect("addr");
        check_peer(&url("https://a.test/"), Some(public), true).expect("public");
    }

    // --- the follower ----------------------------------------------------

    fn resolver_for_tests() -> StubResolver {
        StubResolver::new([
            ("a.test", "93.184.216.34"),
            ("b.test", "93.184.216.35"),
            ("internal.test", "10.0.0.7"),
        ])
    }

    fn redirect(location: &str) -> MockReply {
        MockReply::new(302).header("Location", location)
    }

    #[tokio::test]
    async fn the_chain_is_walked_and_recorded() {
        let backend = MockBackend::new([
            ("https://a.test/one", redirect("/two")),
            ("https://a.test/two", redirect("https://b.test/three")),
            ("https://b.test/three", MockReply::new(200).body("done")),
        ]);
        let outcome = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/one")),
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::Safe), 30),
        )
        .await
        .expect("chain");

        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.url, "https://b.test/three");
        assert_eq!(
            outcome
                .history
                .iter()
                .map(|hop| hop.url.as_str())
                .collect::<Vec<_>>(),
            ["https://a.test/one", "https://a.test/two"]
        );
        assert_eq!(outcome.history[0].location, "https://a.test/two");
        assert_eq!(outcome.history[0].to_string(), "<302 https://a.test/one>");
    }

    #[tokio::test]
    async fn a_redirect_into_private_space_is_refused_mid_chain() {
        let backend = MockBackend::new([
            ("https://a.test/", redirect("https://internal.test/admin")),
            (
                "https://internal.test/admin",
                MockReply::new(200).body("secrets"),
            ),
        ]);
        let err = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/")),
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::Safe), 30),
        )
        .await
        .expect_err("must refuse");
        assert!(matches!(err, FetchError::UnsafeRedirect { .. }));
        // The second URL was never requested.
        assert_eq!(backend.requested(), ["https://a.test/"]);
    }

    #[tokio::test]
    async fn the_same_chain_is_followed_when_the_guard_is_off() {
        let backend = MockBackend::new([
            ("https://a.test/", redirect("https://internal.test/admin")),
            (
                "https://internal.test/admin",
                MockReply::new(200).body("secrets"),
            ),
        ]);
        let outcome = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/")),
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::All), 30),
        )
        .await
        .expect("all");
        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.history.len(), 1);
    }

    #[tokio::test]
    async fn follow_redirects_false_returns_the_three_hundred_itself() {
        let backend = MockBackend::new([("https://a.test/", redirect("https://b.test/"))]);
        let outcome = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/")),
            RedirectPolicy::new(FollowRedirects::Bool(false), 30),
        )
        .await
        .expect("no follow");
        assert_eq!(outcome.response.status, 302);
        assert!(outcome.history.is_empty());
        assert_eq!(backend.requested().len(), 1);
    }

    #[tokio::test]
    async fn the_budget_is_enforced_and_zero_means_none() {
        let backend = MockBackend::new([
            ("https://a.test/1", redirect("/2")),
            ("https://a.test/2", redirect("/3")),
            ("https://a.test/3", MockReply::new(200)),
        ]);
        let err = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/1")),
            RedirectPolicy::new(FollowRedirects::Bool(true), 1),
        )
        .await
        .expect_err("budget");
        assert!(matches!(err, FetchError::TooManyRedirects { max: 1, .. }));

        let err = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/1")),
            RedirectPolicy::new(FollowRedirects::Bool(true), 0),
        )
        .await
        .expect_err("zero budget");
        assert!(matches!(err, FetchError::TooManyRedirects { max: 0, .. }));
    }

    #[tokio::test]
    async fn credentials_do_not_cross_an_origin_boundary() {
        let backend = MockBackend::new([
            ("https://a.test/", redirect("https://b.test/")),
            ("https://b.test/", MockReply::new(200)),
        ]);
        let mut request = WireRequest::get(url("https://a.test/"));
        request
            .headers
            .insert("Authorization".to_owned(), "Bearer secret".to_owned());
        request.headers.insert("X-Keep".to_owned(), "1".to_owned());

        follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            request,
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::Safe), 30),
        )
        .await
        .expect("chain");

        let sent = backend.sent();
        assert_eq!(
            header_value(&sent[0].headers, "authorization"),
            Some("Bearer secret")
        );
        assert_eq!(header_value(&sent[1].headers, "authorization"), None);
        assert_eq!(header_value(&sent[1].headers, "x-keep"), Some("1"));
    }

    #[tokio::test]
    async fn credentials_survive_a_same_origin_hop() {
        let backend = MockBackend::new([
            ("https://a.test/one", redirect("/two")),
            ("https://a.test/two", MockReply::new(200)),
        ]);
        let mut request = WireRequest::get(url("https://a.test/one"));
        request
            .headers
            .insert("Authorization".to_owned(), "Bearer secret".to_owned());

        follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            request,
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::Safe), 30),
        )
        .await
        .expect("chain");
        assert_eq!(
            header_value(&backend.sent()[1].headers, "authorization"),
            Some("Bearer secret")
        );
    }

    #[tokio::test]
    async fn a_post_becomes_a_get_and_loses_its_body_on_303() {
        let backend = MockBackend::new([
            (
                "https://a.test/submit",
                MockReply::new(303).header("Location", "/done"),
            ),
            ("https://a.test/done", MockReply::new(200)),
        ]);
        let mut request = WireRequest::new(Method::Post, url("https://a.test/submit"));
        request.body = Some(crate::config::Body::Text("a=1".to_owned()));

        let outcome = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            request,
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::Safe), 30),
        )
        .await
        .expect("chain");

        assert_eq!(outcome.method, Method::Get);
        let sent = backend.sent();
        assert_eq!(sent[0].method, Method::Post);
        assert!(sent[0].body.is_some());
        assert_eq!(sent[1].method, Method::Get);
        assert!(sent[1].body.is_none());
    }

    #[tokio::test]
    async fn a_three_hundred_without_a_location_is_just_a_response() {
        let backend = MockBackend::new([("https://a.test/", MockReply::new(302))]);
        let outcome = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/")),
            RedirectPolicy::default(),
        )
        .await
        .expect("no location");
        assert_eq!(outcome.response.status, 302);
        assert!(outcome.history.is_empty());
    }

    #[tokio::test]
    async fn the_observer_sees_every_hop_in_order() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<String>>);

        impl HopObserver for Recorder {
            fn before_send(&self, request: &mut WireRequest) {
                self.0
                    .lock()
                    .expect("lock")
                    .push(format!("-> {}", request.url));
                request
                    .headers
                    .insert("X-Hop".to_owned(), request.url.path().to_owned());
            }

            fn after_receive(&self, _request: &WireRequest, response: &WireResponse) {
                self.0
                    .lock()
                    .expect("lock")
                    .push(format!("<- {}", response.status));
            }
        }

        let backend = MockBackend::new([
            ("https://a.test/one", redirect("/two")),
            ("https://a.test/two", MockReply::new(200)),
        ]);
        let recorder = Recorder::default();
        follow_redirects(
            &backend,
            &resolver_for_tests(),
            &recorder,
            WireRequest::get(url("https://a.test/one")),
            RedirectPolicy::default(),
        )
        .await
        .expect("chain");

        assert_eq!(
            *recorder.0.lock().expect("lock"),
            [
                "-> https://a.test/one",
                "<- 302",
                "-> https://a.test/two",
                "<- 200",
            ]
        );
        assert_eq!(
            header_value(&backend.sent()[1].headers, "x-hop"),
            Some("/two")
        );
    }

    #[tokio::test]
    async fn an_unparsable_location_is_reported_not_swallowed() {
        let backend = MockBackend::new([(
            "https://a.test/",
            MockReply::new(302).header("Location", "http://[bad"),
        )]);
        let err = follow_redirects(
            &backend,
            &resolver_for_tests(),
            &NoObserver,
            WireRequest::get(url("https://a.test/")),
            RedirectPolicy::default(),
        )
        .await
        .expect_err("bad location");
        assert!(matches!(err, FetchError::InvalidRedirect { .. }));
    }
}
