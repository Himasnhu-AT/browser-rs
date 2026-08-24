//! [`headers_job`]: header synthesis, including the
//! headers-keys-snapshot-before-mutation subtlety. Zero I/O.
//!
//! Port of `_ConfigurationLogic._headers_job` (`reference/engines/static.py`).
//! Three jobs, in this order:
//!
//! 1. merge session headers with per-request headers (per-request wins);
//! 2. when `stealthy_headers` is on, add a Google referer and — only if
//!    impersonation is **off** — a generated, self-consistent browser header set;
//! 3. when `stealthy_headers` is off, fall back to a single static `User-Agent`,
//!    again only if impersonation is off.
//!
//! When impersonation is on, curl-impersonate/BoringSSL emits a header set that
//! matches the TLS fingerprint, so the reference implementation adds **nothing** but the referer and
//! the user's own headers. Injecting browserforge headers there would contradict
//! the fingerprint.

use browser_fingerprint::headers::{generate_headers, BrowserMode, Headers};
use browser_fingerprint::useragent::default_ua;

use crate::config::HeaderDict;

/// The referer stealth mode adds when the caller supplied none.
pub const GOOGLE_REFERER: &str = "https://www.google.com/";

/// Build the final header map for one request.
///
/// * `url` is accepted and **ignored** — Python takes it too and never reads it.
///   It is kept in the signature so the two implementations line up.
/// * `session_headers` are the session defaults (`self._default_headers`).
/// * `headers` is the *resolved* per-request value, which
///   [`crate::merge::merge_request_args`] defaults to `session_headers`. The two
///   are therefore merged twice in the common path — harmless, idempotent, and
///   exactly what Python does.
/// * `stealth` is the resolved `stealthy_headers` flag.
/// * `impersonate_enabled` is `bool(impersonate)` **after** profile selection.
///
/// # The snapshot subtlety
///
/// `headers_keys` — the lowercase set used for every "did the caller already supply
/// this?" test — is taken **before** the referer is inserted. A generated header set
/// that contained `Referer` could therefore overwrite the Google referer. It cannot
/// silently *merge* with it either: keys are case-sensitive, so a generated
/// `Referer` and the inserted `referer` would coexist as two entries, precisely as
/// they do in a Python `dict`. This is preserved rather than "fixed".
///
/// # Examples
///
/// ```
/// use browser_http::config::HeaderDict;
/// use browser_http::headers_job::{headers_job, GOOGLE_REFERER};
///
/// // Impersonation on: only the referer is added, curl supplies the rest.
/// let headers = headers_job("https://example.com", &HeaderDict::new(), &HeaderDict::new(), true, true);
/// assert_eq!(headers.get("referer").map(String::as_str), Some(GOOGLE_REFERER));
/// assert_eq!(headers.len(), 1);
///
/// // A user-supplied referer is never overwritten, whatever its case.
/// let mut mine = HeaderDict::new();
/// mine.insert("Referer".to_owned(), "https://mine.test/".to_owned());
/// let headers = headers_job("https://example.com", &HeaderDict::new(), &mine, true, true);
/// assert_eq!(headers.get("Referer").map(String::as_str), Some("https://mine.test/"));
/// assert!(headers.get("referer").is_none());
/// ```
#[must_use]
pub fn headers_job(
    url: &str,
    session_headers: &HeaderDict,
    headers: &HeaderDict,
    stealth: bool,
    impersonate_enabled: bool,
) -> HeaderDict {
    headers_job_with(
        url,
        session_headers,
        headers,
        stealth,
        impersonate_enabled,
        || generate_headers(BrowserMode::Http),
        default_ua(),
    )
}

/// [`headers_job`] with the two non-deterministic inputs injected.
///
/// `generate_headers` is only called when it is actually needed (stealth on,
/// impersonation off), matching Python's lazy call site; `fallback_user_agent`
/// stands in for `__default_useragent__`, which Python computes once at import time
/// and this crate exposes as
/// [`browser_fingerprint::useragent::default_ua`].
///
/// # Examples
///
/// ```
/// use browser_fingerprint::headers::Headers;
/// use browser_http::config::HeaderDict;
/// use browser_http::headers_job::headers_job_with;
///
/// let mut generated = Headers::new();
/// generated.push("User-Agent", "Generated/1.0");
/// generated.push("Accept-Language", "en-US;q=1.0");
///
/// let mut mine = HeaderDict::new();
/// mine.insert("user-agent".to_owned(), "Mine/1.0".to_owned());
///
/// let headers = headers_job_with(
///     "https://example.com",
///     &HeaderDict::new(),
///     &mine,
///     true,
///     false,
///     || generated,
///     "Fallback/1.0",
/// );
///
/// // The user's UA survives despite the case difference…
/// assert_eq!(headers.get("user-agent").map(String::as_str), Some("Mine/1.0"));
/// assert!(headers.get("User-Agent").is_none());
/// // …and the rest of the generated set is still added.
/// assert_eq!(headers.get("Accept-Language").map(String::as_str), Some("en-US;q=1.0"));
/// ```
#[must_use]
pub fn headers_job_with<F>(
    url: &str,
    session_headers: &HeaderDict,
    headers: &HeaderDict,
    stealth: bool,
    impersonate_enabled: bool,
    generator: F,
    fallback_user_agent: &str,
) -> HeaderDict
where
    F: FnOnce() -> Headers,
{
    // Python takes `url` and never reads it; keeping the parameter documents the
    // parity rather than hiding a difference.
    let _ = url;

    // `{**self._default_headers, **(headers if headers else {})}` — request wins
    // key for key, and `IndexMap::insert` keeps the session key's position when
    // both spell it the same way.
    let mut final_headers = session_headers.clone();
    for (name, value) in headers {
        final_headers.insert(name.clone(), value.clone());
    }

    // Snapshot BEFORE any mutation. Everything below tests against this set, not
    // against the live map.
    let headers_keys: Vec<String> = final_headers.keys().map(|key| key.to_lowercase()).collect();
    let already_set = |name: &str| headers_keys.iter().any(|key| key == name);

    if stealth {
        if !already_set("referer") {
            final_headers.insert("referer".to_owned(), GOOGLE_REFERER.to_owned());
        }

        if !impersonate_enabled {
            // curl-impersonate would otherwise generate a matching set itself.
            for (name, value) in generator().into_vec() {
                if !already_set(&name.to_lowercase()) {
                    final_headers.insert(name, value);
                }
            }
        }
    } else if !already_set("user-agent") && !impersonate_enabled {
        final_headers.insert("User-Agent".to_owned(), fallback_user_agent.to_owned());
        tracing::debug!("Can't find useragent in headers so '{fallback_user_agent}' was used.");
    }

    final_headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, &str)]) -> HeaderDict {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn generated() -> Headers {
        let mut headers = Headers::new();
        headers.push("User-Agent", "Generated/1.0");
        headers.push("Accept", "text/html");
        headers.push("Accept-Language", "en-US;q=1.0");
        headers.push("sec-ch-ua-mobile", "?0");
        headers
    }

    fn job(
        session: &HeaderDict,
        request: &HeaderDict,
        stealth: bool,
        impersonate: bool,
    ) -> HeaderDict {
        headers_job_with(
            "https://example.com",
            session,
            request,
            stealth,
            impersonate,
            generated,
            "Fallback/1.0",
        )
    }

    #[test]
    fn request_headers_beat_session_headers_and_keep_their_position() {
        let session = dict(&[("X-Session", "1"), ("Accept", "session")]);
        let request = dict(&[("Accept", "request"), ("X-Request", "2")]);
        let headers = job(&session, &request, false, true);

        assert_eq!(headers.get("Accept").map(String::as_str), Some("request"));
        assert_eq!(
            headers.keys().collect::<Vec<_>>(),
            vec!["X-Session", "Accept", "X-Request"]
        );
    }

    #[test]
    fn merging_is_case_sensitive_like_a_python_dict() {
        let session = dict(&[("Accept", "session")]);
        let request = dict(&[("accept", "request")]);
        let headers = job(&session, &request, false, true);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("Accept").map(String::as_str), Some("session"));
        assert_eq!(headers.get("accept").map(String::as_str), Some("request"));
    }

    #[test]
    fn stealth_with_impersonation_adds_only_the_referer() {
        let headers = job(&HeaderDict::new(), &HeaderDict::new(), true, true);
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers.get("referer").map(String::as_str),
            Some(GOOGLE_REFERER)
        );
    }

    #[test]
    fn stealth_without_impersonation_adds_generated_headers_after_the_referer() {
        let headers = job(&HeaderDict::new(), &HeaderDict::new(), true, false);
        assert_eq!(
            headers.get("referer").map(String::as_str),
            Some(GOOGLE_REFERER)
        );
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("Generated/1.0")
        );
        assert_eq!(headers.get("Accept").map(String::as_str), Some("text/html"));
        assert_eq!(
            headers.get("sec-ch-ua-mobile").map(String::as_str),
            Some("?0")
        );
        assert_eq!(
            headers.keys().next().map(String::as_str),
            Some("referer"),
            "the referer is inserted before the generated set"
        );
    }

    #[test]
    fn generated_headers_never_overwrite_user_headers_whatever_the_case() {
        let request = dict(&[("USER-AGENT", "Mine/1.0"), ("accept", "application/json")]);
        let headers = job(&HeaderDict::new(), &request, true, false);
        assert_eq!(
            headers.get("USER-AGENT").map(String::as_str),
            Some("Mine/1.0")
        );
        assert!(headers.get("User-Agent").is_none());
        assert_eq!(
            headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert!(headers.get("Accept").is_none());
        // Untouched generated keys still land.
        assert_eq!(
            headers.get("Accept-Language").map(String::as_str),
            Some("en-US;q=1.0")
        );
    }

    #[test]
    fn a_user_referer_suppresses_the_google_one() {
        let request = dict(&[("Referer", "https://mine.test/")]);
        let headers = job(&HeaderDict::new(), &request, true, true);
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers.get("Referer").map(String::as_str),
            Some("https://mine.test/")
        );
    }

    #[test]
    fn a_generated_referer_can_still_land_because_of_the_snapshot() {
        // The snapshot is taken before the Google referer is inserted, so the
        // generator is not told about it. Python behaves identically: the keys
        // differ in case, so both entries coexist.
        let mut with_referer = Headers::new();
        with_referer.push("Referer", "https://generated.test/");
        let headers = headers_job_with(
            "https://example.com",
            &HeaderDict::new(),
            &HeaderDict::new(),
            true,
            false,
            || with_referer,
            "Fallback/1.0",
        );
        assert_eq!(
            headers.get("referer").map(String::as_str),
            Some(GOOGLE_REFERER)
        );
        assert_eq!(
            headers.get("Referer").map(String::as_str),
            Some("https://generated.test/")
        );
    }

    #[test]
    fn no_stealth_no_impersonation_falls_back_to_the_static_user_agent() {
        let headers = job(&HeaderDict::new(), &HeaderDict::new(), false, false);
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("Fallback/1.0")
        );
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn no_stealth_with_impersonation_adds_nothing_at_all() {
        let headers = job(&HeaderDict::new(), &HeaderDict::new(), false, true);
        assert!(headers.is_empty());
    }

    #[test]
    fn the_fallback_user_agent_respects_a_user_supplied_one() {
        let request = dict(&[("user-agent", "Mine/1.0")]);
        let headers = job(&HeaderDict::new(), &request, false, false);
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some("Mine/1.0")
        );
    }

    #[test]
    fn the_generator_is_not_called_when_it_is_not_needed() {
        for (stealth, impersonate) in [(true, true), (false, true), (false, false)] {
            let headers = headers_job_with(
                "https://example.com",
                &HeaderDict::new(),
                &HeaderDict::new(),
                stealth,
                impersonate,
                || panic!("generator must not run for stealth={stealth} impersonate={impersonate}"),
                "Fallback/1.0",
            );
            assert!(headers.len() <= 1);
        }
    }

    #[test]
    fn the_real_generator_produces_a_user_agent() {
        let headers = headers_job(
            "https://example.com",
            &HeaderDict::new(),
            &HeaderDict::new(),
            true,
            false,
        );
        assert!(headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("user-agent") && !value.is_empty()));
        assert_eq!(
            headers.get("referer").map(String::as_str),
            Some(GOOGLE_REFERER)
        );
    }
}
