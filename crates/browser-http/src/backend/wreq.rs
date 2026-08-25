//! The `wreq` + `wreq-util` backend that supplies BoringSSL JA3/JA4
//! impersonation.
//!
//! `ARCHITECTURE.md` §2.3: plain `rustls` cannot reproduce curl-impersonate's
//! fingerprints — GREASE ordering, extension permutation, certificate
//! compression, ALPS/`application_settings`, the HTTP/2 SETTINGS and priority
//! frames and header order are all outside a TLS library's remit — so the port
//! depends on `wreq`, a reqwest-shaped client over BoringSSL whose emulation
//! profiles `wreq-util` supplies.
//!
//! # Profile mapping
//!
//! the reference implementation forwards an impersonation string straight to `curl_cffi`, and
//! `curl_cffi`'s target list is **not** `wreq-util`'s: `curl_cffi` 0.16.1 knows
//! `chrome150` and `safari2601`, `wreq-util` 0.1.0 stops at `Chrome137` and
//! `Safari18_5`. [`emulation_for`] therefore resolves the string through
//! [`browser_fingerprint::impersonate::ImpersonateProfile`] — which owns the
//! canonical list *and* the alias table — and then picks the closest profile of
//! the same browser family: the newest `wreq-util` build that is **no newer**
//! than the one asked for, or the oldest available when even that is too new.
//!
//! Asking for `chrome150` therefore gets Chrome 137's fingerprint rather than an
//! error, which is the right trade: a slightly older but *self-consistent*
//! Chrome fingerprint is far better than a mismatched one, and far better than
//! refusing to fetch. The substitution is logged at `debug`.
//!
//! # Known limitations of `wreq` 0.15.3
//!
//! These are transport gaps, recorded rather than hidden:
//!
//! * **`cert=` (client certificates)** has no `ClientBuilder` entry point in
//!   0.15.3, so a client certificate is refused with
//!   [`TransportError::Build`] instead of being silently ignored.
//! * **`http3=True`** cannot be honoured — there is no QUIC transport. Python
//!   only *warns* when `http3` is combined with `impersonate`, so this warns
//!   too and sends over HTTP/2.
//! * **`headers_order`** for user-supplied headers is the emulation profile's,
//!   not Python's insertion order; the profile's order is the fingerprint-safe
//!   one.
//! * **`interface=`** (bind to a named network interface) is `#[cfg]`-gated to a
//!   few platforms in 0.15.3, so it is not offered at all rather than working on
//!   some hosts and not others.
//!
//! Everything else in [`WireRequest`] is honoured.

use std::collections::HashMap;
use std::str::FromStr;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use parking_lot::Mutex;
use browser_fingerprint::impersonate::{BrowserFamily, ImpersonateProfile};
use wreq_util::{Emulation, EmulationOS, EmulationOption};

use super::{HttpBackend, WireRequest, WireResponse};
use crate::config::{Body, HeaderDict, HttpVersion, ProxySpec};
use crate::error::TransportError;
use browser_core::types::Proxy;

/// The pass-through keys this backend understands.
///
/// Everything else in [`WireRequest::extra`] is logged at `debug` and ignored,
/// which is what a `curl_cffi` option with no `wreq` analogue has to do.
pub const SUPPORTED_EXTRA_KEYS: [&str; 2] = ["referer", "accept_encoding"];

/// What distinguishes one pooled `wreq::Client` from another.
///
/// Only settings that must be fixed at *build* time are in the key; a proxy, a
/// timeout, headers, a body and the HTTP version are all per-request and never
/// fragment the pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ClientKey {
    emulation: Option<(Emulation, EmulationOS)>,
    verify: bool,
}

/// Map a the reference implementation impersonation string onto a `wreq-util` emulation profile.
///
/// Returns `None` for a string [`ImpersonateProfile`] does not know — the caller
/// then sends without emulation rather than guessing, which is what
/// `impersonate=None` means anyway.
///
/// # Examples
///
/// ```
/// use browser_http::backend::wreq::emulation_for;
/// use wreq_util::{Emulation, EmulationOS};
///
/// // Aliases resolve exactly as `curl_cffi` resolves them.
/// assert_eq!(emulation_for("firefox"), Some((Emulation::Firefox139, EmulationOS::Windows)));
/// // A target newer than anything wreq-util ships degrades to its newest.
/// assert_eq!(emulation_for("chrome150"), Some((Emulation::Chrome137, EmulationOS::Windows)));
/// // …and an exact match is used as-is.
/// assert_eq!(emulation_for("chrome120"), Some((Emulation::Chrome120, EmulationOS::Windows)));
/// // Mobile targets carry the matching OS.
/// assert_eq!(
///     emulation_for("chrome131_android"),
///     Some((Emulation::Chrome131, EmulationOS::Android)),
/// );
/// assert_eq!(emulation_for("netscape4"), None);
/// ```
#[must_use]
pub fn emulation_for(profile: &str) -> Option<(Emulation, EmulationOS)> {
    let resolved = ImpersonateProfile::parse(profile).ok().or_else(|| {
        tracing::debug!(
            profile,
            "unknown impersonation profile; sending without emulation"
        );
        None
    })?;

    let major = resolved.major_version();
    let mobile = resolved.is_mobile();
    // Tor Browser is Firefox ESR with the branding stripped, which is why
    // `header_family()` exists.
    let family = resolved.family().header_family();

    let (emulation, os) = match (family, mobile) {
        (BrowserFamily::Chrome | BrowserFamily::Tor, true) => {
            (nearest(major, CHROME_TABLE), EmulationOS::Android)
        }
        (BrowserFamily::Chrome | BrowserFamily::Tor, false) => {
            (nearest(major, CHROME_TABLE), EmulationOS::Windows)
        }
        (BrowserFamily::Edge, _) => (nearest(major, EDGE_TABLE), EmulationOS::Windows),
        (BrowserFamily::Safari, true) => (nearest(major, SAFARI_IOS_TABLE), EmulationOS::IOS),
        (BrowserFamily::Safari, false) => (nearest(major, SAFARI_TABLE), EmulationOS::MacOS),
        (BrowserFamily::Firefox, true) => (Emulation::FirefoxAndroid135, EmulationOS::Android),
        (BrowserFamily::Firefox, false) => (nearest(major, FIREFOX_TABLE), EmulationOS::Windows),
    };

    if resolved.as_str() != profile {
        tracing::debug!(
            requested = profile,
            resolved = resolved.as_str(),
            "resolved an impersonation alias"
        );
    }
    Some((emulation, os))
}

/// `(major version, profile)` pairs, ascending. `nearest` walks them once.
type ProfileTable = &'static [(u32, Emulation)];

const CHROME_TABLE: ProfileTable = &[
    (100, Emulation::Chrome100),
    (101, Emulation::Chrome101),
    (104, Emulation::Chrome104),
    (105, Emulation::Chrome105),
    (106, Emulation::Chrome106),
    (107, Emulation::Chrome107),
    (108, Emulation::Chrome108),
    (109, Emulation::Chrome109),
    (110, Emulation::Chrome110),
    (114, Emulation::Chrome114),
    (116, Emulation::Chrome116),
    (117, Emulation::Chrome117),
    (118, Emulation::Chrome118),
    (119, Emulation::Chrome119),
    (120, Emulation::Chrome120),
    (123, Emulation::Chrome123),
    (124, Emulation::Chrome124),
    (126, Emulation::Chrome126),
    (127, Emulation::Chrome127),
    (128, Emulation::Chrome128),
    (129, Emulation::Chrome129),
    (130, Emulation::Chrome130),
    (131, Emulation::Chrome131),
    (132, Emulation::Chrome132),
    (133, Emulation::Chrome133),
    (134, Emulation::Chrome134),
    (135, Emulation::Chrome135),
    (136, Emulation::Chrome136),
    (137, Emulation::Chrome137),
];

const EDGE_TABLE: ProfileTable = &[
    (101, Emulation::Edge101),
    (122, Emulation::Edge122),
    (127, Emulation::Edge127),
    (131, Emulation::Edge131),
    (134, Emulation::Edge134),
];

const FIREFOX_TABLE: ProfileTable = &[
    (109, Emulation::Firefox109),
    (117, Emulation::Firefox117),
    (128, Emulation::Firefox128),
    (133, Emulation::Firefox133),
    (135, Emulation::Firefox135),
    (136, Emulation::Firefox136),
    (139, Emulation::Firefox139),
];

const SAFARI_TABLE: ProfileTable = &[
    (15, Emulation::Safari15_6_1),
    (16, Emulation::Safari16_5),
    (17, Emulation::Safari17_5),
    (18, Emulation::Safari18_5),
];

const SAFARI_IOS_TABLE: ProfileTable = &[
    (16, Emulation::SafariIos16_5),
    (17, Emulation::SafariIos17_4_1),
    (18, Emulation::SafariIos18_1_1),
];

/// The newest table entry no newer than `major`, or the oldest when even that is
/// too new.
fn nearest(major: u32, table: ProfileTable) -> Emulation {
    let mut best = table[0].1;
    for (version, emulation) in table {
        if *version <= major {
            best = *emulation;
        } else {
            break;
        }
    }
    best
}

/// The `wreq` transport.
///
/// Holds a small cache of pooled clients keyed by the settings that must be
/// fixed at build time — the emulation profile and TLS verification. Everything
/// else is per request, so a session that rotates through a pool of
/// impersonation profiles reuses two or three clients rather than building one
/// per request.
#[derive(Debug, Default)]
pub struct WreqBackend {
    clients: Mutex<HashMap<ClientKey, ::wreq::Client>>,
}

impl WreqBackend {
    /// A backend with an empty client cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct clients are pooled. Diagnostics only.
    #[must_use]
    pub fn pooled_clients(&self) -> usize {
        self.clients.lock().len()
    }

    fn client_for(&self, key: ClientKey) -> Result<::wreq::Client, TransportError> {
        if let Some(client) = self.clients.lock().get(&key) {
            return Ok(client.clone());
        }

        let mut builder = ::wreq::Client::builder()
            // Every redirect decision belongs to `crate::redirect_safe`, which
            // is the only way `follow_redirects="safe"` can exist at all.
            .redirect(::wreq::redirect::Policy::none())
            .cert_verification(key.verify);
        if let Some((emulation, os)) = key.emulation {
            builder = builder.emulation(
                EmulationOption::builder()
                    .emulation(emulation)
                    .emulation_os(os)
                    .build(),
            );
        }

        // libcurl still accepts RFC 7230 obs-fold header continuations, so the
        // reference implementation reads them; hyper rejects them by default and
        // the whole exchange fails. `wreq` exposes the switch, and turning it on
        // is the only way a folded reply parses the way Python's does.
        builder = builder.http1(|mut http1| {
            http1.allow_obsolete_multiline_headers_in_responses(true);
        });

        let client = builder.build().map_err(|err| {
            TransportError::build(format!("Could not build an HTTP client: {err}"))
        })?;
        self.clients.lock().insert(key, client.clone());
        Ok(client)
    }
}

/// Pick the proxy for one request: the explicit one first, then the per-scheme
/// table (`all` last, so a scheme-specific entry wins).
fn choose_proxy(request: &WireRequest, url: &::wreq::Url) -> Option<String> {
    if let Some(proxy) = &request.proxy {
        return Some(crate::proxy::proxy_url(proxy));
    }
    proxy_for_scheme(&request.proxies, url.scheme()).map(str::to_owned)
}

fn proxy_for_scheme<'a>(proxies: &'a ProxySpec, scheme: &str) -> Option<&'a str> {
    let specific = match scheme {
        "http" => proxies.http.as_deref(),
        "https" => proxies.https.as_deref(),
        "ws" => proxies.ws.as_deref(),
        "wss" => proxies.wss.as_deref(),
        _ => None,
    };
    specific.or(proxies.all.as_deref())
}

/// Render a body, returning the bytes and the `Content-Type` it implies.
///
/// The header half is [`crate::backend::implied_content_type`], shared with the
/// redirect follower so a hop that drops the payload still sends the header the
/// payload implied — which is what `curl_cffi` does. An explicit `Content-Type`
/// still wins; see the `has_header` guard at the call site.
fn render_body(body: &Body) -> (Bytes, Option<&'static str>) {
    let bytes = match body {
        Body::Form(pairs) => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in pairs {
                serializer.append_pair(key, value);
            }
            Bytes::from(serializer.finish())
        }
        Body::Json(value) => Bytes::from(serde_json::to_vec(value).unwrap_or_default()),
        Body::Text(text) => Bytes::from(text.clone()),
        Body::Raw(bytes) => bytes.clone(),
    };
    (bytes, crate::backend::implied_content_type(body))
}

fn has_header(headers: &HeaderDict, name: &str) -> bool {
    headers.keys().any(|key| key.eq_ignore_ascii_case(name))
}

/// Collect a `wreq` header map into the ordered, case-preserving map the rest of
/// the crate uses.
///
/// `HeaderDict` is an `IndexMap`, so it cannot hold a repeated name — and a
/// reply may legitimately carry several `Set-Cookie` lines. **Every** repeat is
/// folded on `", "`, which is what RFC 9110 §5.3 says a repeated field means and,
/// more to the point, exactly what `dict(response.headers)` produces in Python:
/// `curl_cffi`'s `Headers.__iter__` joins its multidict with `", "` without
/// special-casing cookies. The folding is lossy for `Set-Cookie` — an `Expires`
/// attribute carries a comma of its own — so the unfolded lines are reported
/// separately in [`WireResponse::set_cookies`], and both the jar and
/// `Response.cookies` read *that*, never the header.
fn collect_headers(headers: &http::HeaderMap) -> HeaderDict {
    let mut out = HeaderDict::new();
    for (name, value) in headers {
        let text = value
            .to_str()
            .map_or_else(|_| format!("{value:?}"), str::to_owned);
        if let Some(existing) = out.get_mut(name.as_str()) {
            existing.push_str(", ");
            existing.push_str(&text);
        } else {
            out.insert(name.as_str().to_owned(), text);
        }
    }
    out
}

/// The `Set-Cookie` lines, unfolded and in wire order.
fn collect_set_cookies(headers: &http::HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::SET_COOKIE)
        .iter()
        .map(|value| {
            value
                .to_str()
                .map_or_else(|_| format!("{value:?}"), str::to_owned)
        })
        .collect()
}

fn classify(err: &::wreq::Error, proxy: Option<&str>) -> TransportError {
    let message = err.to_string();
    let lowered = message.to_lowercase();

    if err.is_timeout() {
        return TransportError::timeout(message);
    }
    if lowered.contains("certificate") || lowered.contains("ssl") || lowered.contains("tls") {
        return TransportError::tls(message);
    }
    if lowered.contains("dns error") || lowered.contains("failed to lookup address") {
        return TransportError::dns(format!("Could not resolve host: {message}"));
    }
    if err.is_connect() || err.is_connection_reset() {
        // Python's `is_proxy_error` greps the message to pick a log line, so a
        // proxy failure has to *say* it is one.
        return proxy.map_or_else(
            || TransportError::connect(format!("Failed to connect: {message}")),
            |proxy| TransportError::proxy(format!("Failed to connect to proxy {proxy}: {message}")),
        );
    }
    if err.is_body() || err.is_decode() {
        return TransportError::body(message);
    }
    if err.is_builder() || err.is_request() {
        return TransportError::build(message);
    }
    TransportError::other(message)
}

#[async_trait]
impl HttpBackend for WreqBackend {
    async fn execute(&self, request: WireRequest) -> Result<WireResponse, TransportError> {
        if request.cert.is_some() {
            return Err(TransportError::build(
                "client certificates (`cert=`) are not supported by the wreq backend".to_owned(),
            ));
        }

        let emulation = request.impersonate.as_deref().and_then(|profile| {
            if profile.is_empty() {
                None
            } else {
                emulation_for(profile)
            }
        });
        let client = self.client_for(ClientKey {
            emulation,
            verify: request.verify,
        })?;

        let url = request.effective_url();
        let proxy = choose_proxy(&request, &url);
        let mut builder = client
            .request(http::Method::from(request.method), url.clone())
            .redirect(::wreq::redirect::Policy::none());

        // -- headers ------------------------------------------------------
        for (name, value) in &request.headers {
            let name = HeaderName::from_str(name).map_err(|err| {
                TransportError::build(format!("Invalid header name {name:?}: {err}"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|err| {
                TransportError::build(format!("Invalid value for header {name:?}: {err}"))
            })?;
            builder = builder.header(name, value);
        }

        // -- cookies ------------------------------------------------------
        // The session's `JarObserver` normally materialises these into a
        // `Cookie` header; this branch covers a caller driving the backend
        // directly.
        if let Some(cookies) = &request.cookies {
            if !cookies.is_empty() && !has_header(&request.headers, "cookie") {
                let rendered = cookies
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                builder = builder.header(http::header::COOKIE, rendered);
            }
        }

        // -- body ---------------------------------------------------------
        if let Some(body) = &request.body {
            let (bytes, content_type) = render_body(body);
            if let Some(content_type) = content_type {
                if !has_header(&request.headers, "content-type") {
                    builder = builder.header(http::header::CONTENT_TYPE, content_type);
                }
            }
            builder = builder.body(bytes);
        }

        // -- auth, timeout, proxy, version --------------------------------
        if let Some((username, password)) = &request.auth {
            builder = builder.basic_auth(username, Some(password));
        }
        // libcurl reads `CURLOPT_TIMEOUT_MS = 0` as "no timeout"; a zero
        // `Duration` here would instead abort the request immediately.
        if let Some(timeout) = request.timeout.filter(|budget| !budget.is_zero()) {
            builder = builder.timeout(timeout);
        }
        if let Some(proxy_url) = &proxy {
            let mut proxy = ::wreq::Proxy::all(proxy_url.as_str()).map_err(|err| {
                TransportError::build(format!("Invalid proxy {proxy_url:?}: {err}"))
            })?;
            if let Some((username, password)) = proxy_auth_for(&request) {
                proxy = proxy.basic_auth(&username, &password);
            }
            builder = builder.proxy(proxy);
        }
        if let Some(version) = http_version(request.http_version) {
            builder = builder.version(version);
        }

        // -- the pass-through bag -----------------------------------------
        for (key, value) in &request.extra {
            match key.as_str() {
                "referer" => {
                    if let Some(referer) = value.as_str() {
                        builder = builder.header(http::header::REFERER, referer.to_owned());
                    }
                }
                "accept_encoding" => {
                    if let Some(encoding) = value.as_str() {
                        builder =
                            builder.header(http::header::ACCEPT_ENCODING, encoding.to_owned());
                    }
                }
                other => tracing::debug!(
                    option = other,
                    "the wreq backend has no analogue for this curl option; ignoring it"
                ),
            }
        }

        // -- send ---------------------------------------------------------
        let (client, built) = builder.build_split();
        let built = built.map_err(|err| classify(&err, proxy.as_deref()))?;
        // The headers the emulation profile contributes live on the client, so
        // the two have to be merged to report what actually goes out.
        let mut request_headers = collect_headers(&client.headers());
        for (name, value) in collect_headers(built.headers()) {
            request_headers.insert(name, value);
        }

        let response = client
            .execute(built)
            .await
            .map_err(|err| classify(&err, proxy.as_deref()))?;

        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let remote_addr = response.remote_addr();
        let headers = collect_headers(response.headers());
        let set_cookies = collect_set_cookies(response.headers());
        // hyper keeps a non-canonical reason phrase in a private extension type
        // this crate cannot name without depending on hyper directly, so the
        // canonical phrase is used and `StatusText` fills the gaps. A server
        // that sends a bespoke phrase is a declared corpus divergence.
        let reason = response.status().canonical_reason().map_or_else(
            || browser_fingerprint::status::StatusText::get(status).to_owned(),
            str::to_owned,
        );

        let body = response
            .bytes()
            .await
            .map_err(|err| classify(&err, proxy.as_deref()))?;

        Ok(WireResponse {
            status,
            reason,
            url: final_url,
            headers,
            set_cookies,
            request_headers,
            body,
            // `curl_cffi` resolves the charset itself; here
            // `crate::response::resolve_encoding` does it from the headers, so
            // there is nothing extra to report.
            encoding: None,
            remote_addr,
        })
    }

    fn name(&self) -> &'static str {
        "wreq"
    }
}

fn proxy_auth_for(request: &WireRequest) -> Option<(String, String)> {
    request.proxy_auth.clone().or_else(|| match &request.proxy {
        Some(Proxy::Config {
            username: Some(username),
            password,
            ..
        }) => Some((username.clone(), password.clone().unwrap_or_default())),
        _ => None,
    })
}

/// Map curl's `CurlHttpVersion` onto what `wreq` can actually negotiate.
///
/// HTTP/3 has no transport here, so `V3`/`V3ONLY` warn and fall through to
/// HTTP/2 — Python only warns about `http3` too, so nothing that worked there
/// fails here.
fn http_version(version: Option<HttpVersion>) -> Option<http::Version> {
    match version? {
        HttpVersion::None => None,
        HttpVersion::Http1_0 => Some(http::Version::HTTP_10),
        HttpVersion::Http1_1 => Some(http::Version::HTTP_11),
        HttpVersion::Http2 | HttpVersion::Http2Tls | HttpVersion::Http2PriorKnowledge => {
            Some(http::Version::HTTP_2)
        }
        HttpVersion::Http3 | HttpVersion::Http3Only => {
            tracing::warn!(
                "HTTP/3 is not available in the wreq backend; sending over HTTP/2 instead"
            );
            Some(http::Version::HTTP_2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Method;
    use url::Url;

    #[test]
    fn every_canonical_profile_maps_to_an_emulation() {
        // The whole `curl_cffi` 0.16.1 target list has to resolve to *some*
        // fingerprint; a gap would mean a silently un-impersonated request.
        for profile in ImpersonateProfile::ALL {
            let mapped = emulation_for(profile.as_str());
            assert!(mapped.is_some(), "{} has no emulation", profile.as_str());
        }
        // …and so do the aliases the reference implementation's docs advertise.
        for alias in [
            "edge",
            "chrome",
            "chrome_android",
            "safari",
            "safari_beta",
            "safari_ios",
            "safari_ios_beta",
            "firefox",
            "tor",
        ] {
            assert!(emulation_for(alias).is_some(), "{alias}");
        }
        assert_eq!(emulation_for("netscape4"), None);
        assert_eq!(emulation_for(""), None);
    }

    #[test]
    fn the_nearest_profile_is_never_newer_than_the_one_asked_for() {
        // Exact hits.
        assert_eq!(nearest(120, CHROME_TABLE), Emulation::Chrome120);
        assert_eq!(nearest(101, EDGE_TABLE), Emulation::Edge101);
        // Between two entries: round down.
        assert_eq!(nearest(121, CHROME_TABLE), Emulation::Chrome120);
        assert_eq!(nearest(125, CHROME_TABLE), Emulation::Chrome124);
        // Newer than anything shipped: the newest available.
        assert_eq!(nearest(150, CHROME_TABLE), Emulation::Chrome137);
        assert_eq!(nearest(26, SAFARI_TABLE), Emulation::Safari18_5);
        // Older than anything shipped: the oldest available, never a panic.
        assert_eq!(nearest(99, CHROME_TABLE), Emulation::Chrome100);
        assert_eq!(nearest(0, FIREFOX_TABLE), Emulation::Firefox109);
    }

    #[test]
    fn the_family_and_os_follow_the_profile() {
        assert_eq!(
            emulation_for("chrome99_android"),
            Some((Emulation::Chrome100, EmulationOS::Android))
        );
        assert_eq!(
            emulation_for("safari180_ios"),
            Some((Emulation::SafariIos18_1_1, EmulationOS::IOS))
        );
        assert_eq!(
            emulation_for("safari170"),
            Some((Emulation::Safari17_5, EmulationOS::MacOS))
        );
        assert_eq!(
            emulation_for("edge99"),
            Some((Emulation::Edge101, EmulationOS::Windows))
        );
        // Tor is Firefox ESR with the branding stripped.
        let (tor, os) = emulation_for("tor145").expect("tor maps");
        assert_eq!(os, EmulationOS::Windows);
        assert_eq!(tor, Emulation::Firefox139);
    }

    #[test]
    fn the_client_cache_keys_only_on_build_time_settings() {
        let backend = WreqBackend::new();
        let chrome = emulation_for("chrome");
        let firefox = emulation_for("firefox");

        backend
            .client_for(ClientKey {
                emulation: chrome,
                verify: true,
            })
            .expect("client");
        backend
            .client_for(ClientKey {
                emulation: chrome,
                verify: true,
            })
            .expect("client");
        assert_eq!(backend.pooled_clients(), 1, "the same key reuses a client");

        backend
            .client_for(ClientKey {
                emulation: firefox,
                verify: true,
            })
            .expect("client");
        backend
            .client_for(ClientKey {
                emulation: chrome,
                verify: false,
            })
            .expect("client");
        assert_eq!(backend.pooled_clients(), 3);
    }

    #[test]
    fn bodies_render_with_the_content_type_they_imply() {
        let (bytes, content_type) = render_body(&Body::Form(vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "two words".to_owned()),
        ]));
        assert_eq!(bytes, Bytes::from_static(b"a=1&b=two+words"));
        assert_eq!(content_type, Some("application/x-www-form-urlencoded"));

        let (bytes, content_type) = render_body(&Body::Json(serde_json::json!({"a": 1})));
        assert_eq!(bytes, Bytes::from_static(br#"{"a":1}"#));
        assert_eq!(content_type, Some("application/json"));

        // `curl_cffi` labels a raw payload `application/octet-stream`; measured
        // in `tools/corpus/u12` (`echo-post-data`, `echo-put`).
        let (bytes, content_type) = render_body(&Body::Text("raw".to_owned()));
        assert_eq!(bytes, Bytes::from_static(b"raw"));
        assert_eq!(content_type, Some("application/octet-stream"));

        let (bytes, content_type) = render_body(&Body::Raw(Bytes::from_static(b"\x00\x01")));
        assert_eq!(bytes, Bytes::from_static(b"\x00\x01"));
        assert_eq!(content_type, Some("application/octet-stream"));
    }

    #[test]
    fn the_proxy_table_prefers_the_specific_scheme() {
        let spec = ProxySpec {
            all: Some("http://all:1".to_owned()),
            https: Some("http://https:2".to_owned()),
            ..ProxySpec::default()
        };
        assert_eq!(proxy_for_scheme(&spec, "https"), Some("http://https:2"));
        assert_eq!(proxy_for_scheme(&spec, "http"), Some("http://all:1"));
        assert_eq!(proxy_for_scheme(&ProxySpec::default(), "https"), None);

        // An explicit `proxy=` beats the table.
        let mut request = WireRequest::get(Url::parse("https://a.test/").expect("url"));
        request.proxies = spec;
        request.proxy = Some(Proxy::Url("http://explicit:3".to_owned()));
        assert_eq!(
            choose_proxy(&request, &request.url),
            Some("http://explicit:3".to_owned())
        );
    }

    #[test]
    fn proxy_credentials_come_from_the_dict_when_proxy_auth_is_absent() {
        let mut request = WireRequest::get(Url::parse("https://a.test/").expect("url"));
        request.proxy = Some(Proxy::Config {
            server: "http://p:1".to_owned(),
            username: Some("u".to_owned()),
            password: Some("p".to_owned()),
        });
        assert_eq!(
            proxy_auth_for(&request),
            Some(("u".to_owned(), "p".to_owned()))
        );

        request.proxy_auth = Some(("explicit".to_owned(), "wins".to_owned()));
        assert_eq!(
            proxy_auth_for(&request),
            Some(("explicit".to_owned(), "wins".to_owned()))
        );
    }

    #[test]
    fn repeated_headers_fold_on_a_comma_and_cookies_are_reported_separately() {
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::SET_COOKIE,
            HeaderValue::from_static("a=1; Expires=Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        headers.append(
            http::header::SET_COOKIE,
            HeaderValue::from_static("b=2; Path=/"),
        );
        headers.append(http::header::VIA, HeaderValue::from_static("1.1 alpha"));
        headers.append(http::header::VIA, HeaderValue::from_static("1.1 beta"));
        headers.append(http::header::SERVER, HeaderValue::from_static("nginx"));

        let collected = collect_headers(&headers);
        // Every repeat folds on `", "`, which is what `dict(response.headers)`
        // produces in Python — cookies included, `Expires` comma and all.
        assert_eq!(
            collected.get("set-cookie").map(String::as_str),
            Some("a=1; Expires=Wed, 21 Oct 2026 07:28:00 GMT, b=2; Path=/")
        );
        // Everything else folds the RFC 9110 way.
        assert_eq!(
            collected.get("via").map(String::as_str),
            Some("1.1 alpha, 1.1 beta")
        );
        assert_eq!(collected.get("server").map(String::as_str), Some("nginx"));

        // …and the jar reads both cookies back out of the *unfolded* list,
        // which is exactly why that list exists.
        let unfolded = collect_set_cookies(&headers);
        assert_eq!(unfolded.len(), 2);
        let jar = crate::session::CookieJar::new();
        let url = Url::parse("https://a.test/").expect("url");
        jar.store_all(&url, &unfolded);
        assert_eq!(jar.matching(&url).len(), 2);
    }

    #[test]
    fn the_http_version_map_never_asks_for_quic() {
        assert_eq!(http_version(None), None);
        assert_eq!(http_version(Some(HttpVersion::None)), None);
        assert_eq!(
            http_version(Some(HttpVersion::Http1_1)),
            Some(http::Version::HTTP_11)
        );
        assert_eq!(
            http_version(Some(HttpVersion::Http3Only)),
            Some(http::Version::HTTP_2)
        );
    }

    #[tokio::test]
    async fn a_client_certificate_is_refused_rather_than_ignored() {
        let backend = WreqBackend::new();
        let mut request =
            WireRequest::new(Method::Get, Url::parse("https://a.test/").expect("url"));
        request.cert = Some(crate::config::ClientCert::File("/tmp/c.pem".into()));
        let err = backend.execute(request).await.expect_err("unsupported");
        assert!(err.to_string().contains("client certificates"));
    }

    // ---------------------------------------------------------------------
    // End-to-end over a real socket.
    //
    // The open internet is not reachable from the tests, and `wiremock` is not a
    // dev-dependency of this crate, so the server is thirty lines of `std::net`:
    // it binds `127.0.0.1:0`, serves a scripted reply per connection and hands
    // back the exact request bytes it saw. That is enough to prove the backend
    // really speaks HTTP — which no amount of mocking can.
    // ---------------------------------------------------------------------

    fn spawn_http_server(
        replies: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .expect("timeout");

                let mut raw = Vec::new();
                let mut chunk = [0_u8; 1024];
                let mut header_end = None;
                while header_end.is_none() {
                    let Ok(read) = stream.read(&mut chunk) else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..read]);
                    header_end = raw
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4);
                }
                // Drain a declared body so a POST can be asserted on too.
                if let Some(start) = header_end {
                    let headers = String::from_utf8_lossy(&raw[..start]).to_lowercase();
                    let declared = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while raw.len() < start + declared {
                        let Ok(read) = stream.read(&mut chunk) else {
                            break;
                        };
                        if read == 0 {
                            break;
                        }
                        raw.extend_from_slice(&chunk[..read]);
                    }
                }

                seen.push(String::from_utf8_lossy(&raw).into_owned());
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    const OK_PAGE: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "Set-Cookie: a=1; Path=/\r\n",
        "Set-Cookie: b=2; Path=/\r\n",
        "Content-Length: 26\r\n",
        "Connection: close\r\n",
        "\r\n",
        "<h1>hello from a socket</h1>",
    );

    #[tokio::test]
    async fn the_backend_really_speaks_http() {
        // `Content-Length` must match the body exactly or the client hangs.
        let body = "<h1>hi</h1>";
        let reply: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (base, server) = spawn_http_server(vec![reply]);

        let backend = WreqBackend::new();
        let mut request = WireRequest::get(Url::parse(&format!("{base}/page")).expect("url"));
        request.impersonate = Some("chrome".to_owned());
        request.params = Some(vec![("q".to_owned(), "rust".to_owned())]);
        request.headers.insert("X-Test".to_owned(), "1".to_owned());

        let response = backend.execute(request).await.expect("exchange");
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.body, Bytes::from_static(b"<h1>hi</h1>"));
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some("text/html; charset=utf-8")
        );
        assert!(response.remote_addr.is_some());

        let seen = server.join().expect("server thread");
        let raw = &seen[0];
        assert!(raw.starts_with("GET /page?q=rust HTTP/1.1\r\n"), "{raw}");
        assert!(raw.to_lowercase().contains("x-test: 1"), "{raw}");
        // The emulation profile supplies a matching browser header set, which is
        // the whole point of the backend.
        let lowered = raw.to_lowercase();
        assert!(lowered.contains("user-agent: mozilla/"), "{raw}");
        assert!(lowered.contains("sec-ch-ua"), "{raw}");
        // …and the response reports what actually went out.
        assert!(response
            .request_headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("user-agent")));
    }

    #[tokio::test]
    async fn the_backend_never_follows_a_redirect_itself() {
        // `follow_redirects="safe"` is only implementable because the transport
        // hands every 3xx straight back.
        let reply = concat!(
            "HTTP/1.1 302 Found\r\n",
            "Location: /elsewhere\r\n",
            "Content-Length: 0\r\n",
            "Connection: close\r\n",
            "\r\n",
        );
        let (base, server) = spawn_http_server(vec![reply]);

        let backend = WreqBackend::new();
        let response = backend
            .execute(WireRequest::get(Url::parse(&base).expect("url")))
            .await
            .expect("exchange");

        assert_eq!(response.status, 302);
        assert_eq!(
            response.headers.get("location").map(String::as_str),
            Some("/elsewhere")
        );
        assert_eq!(server.join().expect("server").len(), 1, "one hop only");
    }

    #[tokio::test]
    async fn a_post_body_and_repeated_set_cookies_survive_the_round_trip() {
        let (base, server) = spawn_http_server(vec![OK_PAGE]);

        let backend = WreqBackend::new();
        let mut request = WireRequest::new(
            crate::backend::Method::Post,
            Url::parse(&format!("{base}/submit")).expect("url"),
        );
        request.body = Some(Body::Form(vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "two words".to_owned()),
        ]));

        let response = backend.execute(request).await.expect("exchange");
        assert_eq!(response.status, 200);
        // Two `Set-Cookie` lines, reported unfolded so the jar can read them.
        let jar = crate::session::CookieJar::new();
        let url = Url::parse(&base).expect("url");
        jar.store_all(&url, &response.set_cookies);
        assert_eq!(jar.matching(&url).len(), 2);

        let seen = server.join().expect("server");
        let raw = &seen[0];
        assert!(raw.starts_with("POST /submit HTTP/1.1\r\n"), "{raw}");
        assert!(
            raw.to_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{raw}"
        );
        assert!(raw.ends_with("a=1&b=two+words"), "{raw}");
    }

    #[tokio::test]
    async fn a_real_chain_runs_through_the_guard_and_loopback_is_refused() {
        use crate::redirect_safe::{follow_redirects, NoObserver, RedirectPolicy, SystemResolver};
        use browser_core::types::{FollowRedirects, RedirectMode};

        let hop = concat!(
            "HTTP/1.1 301 Moved Permanently\r\n",
            "Location: /final\r\n",
            "Content-Length: 0\r\n",
            "Connection: close\r\n",
            "\r\n",
        );
        let body = "<p>arrived</p>";
        let last: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (base, server) = spawn_http_server(vec![hop, last]);

        let backend = WreqBackend::new();
        let outcome = follow_redirects(
            &backend,
            &SystemResolver,
            &NoObserver,
            WireRequest::get(Url::parse(&format!("{base}/start")).expect("url")),
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::All), 30),
        )
        .await
        .expect("chain");

        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.body, Bytes::from_static(b"<p>arrived</p>"));
        assert_eq!(outcome.history.len(), 1);
        assert_eq!(server.join().expect("server").len(), 2);

        // A chain that *changes origin* onto loopback is refused under
        // `"safe"`, against a real socket. The same-origin chain above is not,
        // which is what `curl_cffi` does too — see the module documentation.
        let cross: &'static str = Box::leak(
            concat!(
                "HTTP/1.1 302 Found\r\n",
                "Location: http://127.0.0.1:9/final\r\n",
                "Content-Length: 0\r\n",
                "Connection: close\r\n",
                "\r\n",
            )
            .to_owned()
            .into_boxed_str(),
        );
        let (base, server) = spawn_http_server(vec![cross]);
        let err = follow_redirects(
            &backend,
            &SystemResolver,
            &NoObserver,
            WireRequest::get(Url::parse(&format!("{base}/start")).expect("url")),
            RedirectPolicy::new(FollowRedirects::Mode(RedirectMode::Safe), 30),
        )
        .await
        .expect_err("loopback is private");
        assert!(matches!(
            err,
            crate::error::FetchError::UnsafeRedirect { ref reason, .. }
                if reason.contains("loopback")
        ));
        assert_eq!(server.join().expect("server").len(), 1);
    }

    #[test]
    fn the_supported_extra_keys_are_the_documented_ones() {
        assert_eq!(SUPPORTED_EXTRA_KEYS, ["referer", "accept_encoding"]);
    }
}
