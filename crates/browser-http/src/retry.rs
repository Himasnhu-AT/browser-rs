//! [`attempts_from_retries`] (`0 | -1 | None -> 1`), the retry-eligibility
//! predicate and the per-attempt proxy choice. Zero I/O.
//!
//! Port of the loop body of `_SyncSessionLogic._make_request`
//! (`reference/engines/static.py`). The sleeping and the transport belong to the
//! driver (work unit 12); everything a driver has to *decide* lives here so it can
//! be tested without a network:
//!
//! ```text
//! let policy = RetryPolicy::resolve(&config, &overrides);
//! for attempt in 0..policy.attempts {
//!     let proxy = retry::proxy_for_attempt(&config, &overrides);   // rotates per attempt
//!     match backend.send(merge_request_args(url, &config, &overrides, &ctx)).await {
//!         Ok(response) => return Ok(response),
//!         Err(error) => match policy.decide(attempt) {
//!             RetryDecision::Retry { delay } => { retry::log_retry(attempt, &error, proxy.as_ref(), delay);
//!                                                 sleeper.sleep(delay).await; }
//!             RetryDecision::GiveUp => { retry::log_failure(policy.attempts, &error); return Err(error); }
//!         },
//!     }
//! }
//! ```
//!
//! Two semantics are easy to get wrong and are pinned by tests:
//!
//! * `retries` is an **attempt count**, not a count of *extra* attempts —
//!   `retries=3` sends at most three requests, and `0`, `-1` and `None` all send
//!   exactly one;
//! * only **transport** failures retry. An HTTP 4xx/5xx is a success here and is
//!   returned as a response; `raise_for_status` is never enabled.

use std::fmt;
use std::time::Duration;

use browser_core::types::Proxy;

use crate::config::{FetcherConfig, RequestOverrides};
use crate::proxy::is_proxy_error;

/// Translate `retries` into the number of attempts to make.
///
/// Python computes `max(1, retries or 1)`, so the falsy values (`None` and `0`)
/// short-circuit to `1` and `max` clamps the negatives. The result is never zero:
/// a request is always attempted at least once.
///
/// # Examples
///
/// ```
/// use browser_http::retry::attempts_from_retries;
///
/// assert_eq!(attempts_from_retries(Some(3)), 3);   // the default
/// assert_eq!(attempts_from_retries(Some(1)), 1);
/// assert_eq!(attempts_from_retries(Some(0)), 1);   // falsy -> one attempt
/// assert_eq!(attempts_from_retries(Some(-1)), 1);  // clamped
/// assert_eq!(attempts_from_retries(None), 1);      // absent -> one attempt
/// ```
#[must_use]
pub fn attempts_from_retries(retries: Option<i32>) -> u32 {
    match retries {
        None | Some(0) => 1,
        // `max(1, n)` on an `i32`, then a widening cast that cannot truncate
        // because the value is at least 1.
        Some(n) => n.max(1) as u32,
    }
}

/// What a driver should do after a transport error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetryDecision {
    /// Sleep for `delay`, then make another attempt.
    Retry {
        /// The fixed pause before the next attempt — no backoff, no jitter.
        delay: Duration,
    },
    /// Attempts are exhausted; propagate the error.
    GiveUp,
}

/// The retry knobs for one request, resolved from the session and the overrides.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use browser_http::config::{FetcherConfig, RequestOverrides};
/// use browser_http::retry::{RetryDecision, RetryPolicy};
///
/// let config = FetcherConfig::default();                     // retries = 3, delay = 1s
/// let policy = RetryPolicy::resolve(&config, &RequestOverrides::new());
/// assert_eq!(policy.attempts, 3);
/// assert_eq!(policy.decide(0), RetryDecision::Retry { delay: Duration::from_secs(1) });
/// assert_eq!(policy.decide(2), RetryDecision::GiveUp);
///
/// // A per-request `retries=0` collapses to a single attempt.
/// let once = RetryPolicy::resolve(&config, &RequestOverrides::new().retries(Some(0)));
/// assert_eq!(once.attempts, 1);
/// assert_eq!(once.decide(0), RetryDecision::GiveUp);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RetryPolicy {
    /// Total attempts, always `>= 1`.
    pub attempts: u32,
    /// Pause between attempts.
    pub delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: attempts_from_retries(Some(crate::config::DEFAULT_RETRIES)),
            delay: crate::config::DEFAULT_RETRY_DELAY,
        }
    }
}

impl RetryPolicy {
    /// Build a policy from a raw `retries` value and delay.
    #[must_use]
    pub fn new(retries: Option<i32>, delay: Duration) -> Self {
        Self {
            attempts: attempts_from_retries(retries),
            delay,
        }
    }

    /// Resolve `retries` and `retry_delay` for one request.
    ///
    /// Both are presence-based: a per-request `retries=None` overrides a session
    /// default of 3 and yields a single attempt.
    #[must_use]
    pub fn resolve(config: &FetcherConfig, overrides: &RequestOverrides) -> Self {
        Self::new(
            overrides.retries.or_default(config.retries),
            overrides.retry_delay.or_default(config.retry_delay),
        )
    }

    /// Decide what to do after attempt `attempt` (0-based) failed at the transport
    /// level.
    ///
    /// Python's guard is `if attempt < max_retries - 1`, computed on the 0-based
    /// loop index; this is the same comparison without the underflow risk of
    /// `attempts - 1` on an unsigned zero.
    #[must_use]
    pub fn decide(&self, attempt: u32) -> RetryDecision {
        if attempt + 1 < self.attempts {
            RetryDecision::Retry { delay: self.delay }
        } else {
            RetryDecision::GiveUp
        }
    }

    /// `true` when another attempt follows `attempt`.
    #[must_use]
    pub fn should_retry(&self, attempt: u32) -> bool {
        matches!(self.decide(attempt), RetryDecision::Retry { .. })
    }
}

/// Pick the proxy for the next attempt.
///
/// * a rotator rotates on **every** attempt, so a failing proxy is swapped out
///   before the retry;
/// * a per-request `proxy=` disables the rotator for that request (Python's
///   `static_proxy is None` guard) — this is the `proxy` field of
///   [`RequestOverrides`], not the session default;
/// * without a rotator the same proxy is retried, falling back from the
///   per-request value to the session's.
///
/// Call it **once per attempt**: with a rotator it mutates the rotator's cursor.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use browser_core::types::Proxy;
/// use browser_http::config::{FetcherConfig, RequestOverrides};
/// use browser_http::proxy::ProxyRotator;
/// use browser_http::retry::proxy_for_attempt;
///
/// let rotator = Arc::new(ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080"])?);
/// let config = FetcherConfig::default().with_proxy_rotator(Some(rotator));
///
/// let plain = RequestOverrides::new();
/// assert_eq!(proxy_for_attempt(&config, &plain), Some(Proxy::Url("http://p1:8080".into())));
/// assert_eq!(proxy_for_attempt(&config, &plain), Some(Proxy::Url("http://p2:8080".into())));
///
/// // A per-request proxy pins every attempt and never touches the rotator.
/// let pinned = RequestOverrides::new().proxy_url("http://pinned:1");
/// assert_eq!(proxy_for_attempt(&config, &pinned), Some(Proxy::Url("http://pinned:1".into())));
/// # Ok::<(), browser_http::proxy::ProxyError>(())
/// ```
#[must_use]
pub fn proxy_for_attempt(config: &FetcherConfig, overrides: &RequestOverrides) -> Option<Proxy> {
    // `static_proxy = kwargs.pop("proxy", None)` — the per-request value only. The
    // rotator guard below is `static_proxy is None`, an *identity* test, not a
    // truthiness one: a per-request `proxy=""` therefore still disables the rotator,
    // and the `static_proxy or self._default_proxy` fallback then yields the session
    // proxy (which, with a rotator configured, is necessarily `None`).
    let request_proxy = overrides
        .proxy
        .as_ref()
        .into_option()
        .and_then(Option::as_ref);

    match (&config.proxy_rotator, request_proxy) {
        (Some(rotator), None) => Some(rotator.get_proxy()),
        // `static_proxy or self._default_proxy`
        _ => crate::merge::static_proxy(config, overrides),
    }
}

/// The warning Python logs before sleeping, chosen by [`is_proxy_error`].
///
/// `attempt` is the 0-based loop index; the message shows `attempt + 1`.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use browser_core::types::Proxy;
/// use browser_http::retry::retry_warning;
///
/// let proxy = Proxy::Url("http://p1:8080".into());
/// assert_eq!(
///     retry_warning(0, &"Connection refused", Some(&proxy), Duration::from_secs(1)),
///     "Proxy 'http://p1:8080' failed (attempt 1) | Retrying in 1 seconds..."
/// );
/// assert_eq!(
///     retry_warning(1, &"boom", None, Duration::from_millis(500)),
///     "Attempt 2 failed: boom. Retrying in 0.5 seconds..."
/// );
/// ```
#[must_use]
pub fn retry_warning<E: fmt::Display + ?Sized>(
    attempt: u32,
    error: &E,
    proxy: Option<&Proxy>,
    delay: Duration,
) -> String {
    let seconds = delay.as_secs_f64();
    if is_proxy_error(error) {
        let proxy = proxy.map_or_else(|| "None".to_owned(), render_proxy);
        format!(
            "Proxy '{proxy}' failed (attempt {}) | Retrying in {seconds} seconds...",
            attempt + 1
        )
    } else {
        format!(
            "Attempt {} failed: {error}. Retrying in {seconds} seconds...",
            attempt + 1
        )
    }
}

/// The error Python logs when every attempt has failed.
///
/// # Examples
///
/// ```
/// use browser_http::retry::failure_message;
///
/// assert_eq!(failure_message(3, &"boom"), "Failed after 3 attempts: boom");
/// ```
#[must_use]
pub fn failure_message<E: fmt::Display + ?Sized>(attempts: u32, error: &E) -> String {
    format!("Failed after {attempts} attempts: {error}")
}

/// Emit [`retry_warning`] at `WARN`, as Python does before sleeping.
pub fn log_retry<E: fmt::Display + ?Sized>(
    attempt: u32,
    error: &E,
    proxy: Option<&Proxy>,
    delay: Duration,
) {
    tracing::warn!("{}", retry_warning(attempt, error, proxy, delay));
}

/// Emit [`failure_message`] at `ERROR`, as Python does before re-raising.
pub fn log_failure<E: fmt::Display + ?Sized>(attempts: u32, error: &E) {
    tracing::error!("{}", failure_message(attempts, error));
}

/// How a proxy appears inside a log line.
///
/// Python interpolates the raw value, so a dictionary proxy would print as a
/// Python dict repr — including its password. Rendering only the endpoint keeps
/// credentials out of the log, which is the one deliberate difference.
fn render_proxy(proxy: &Proxy) -> String {
    match proxy {
        Proxy::Url(url) => url.clone(),
        Proxy::Config { server, .. } => server.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::ProxySpec;
    use crate::proxy::ProxyRotator;

    #[test]
    fn attempt_counts_match_python() {
        assert_eq!(attempts_from_retries(Some(3)), 3);
        assert_eq!(attempts_from_retries(Some(10)), 10);
        assert_eq!(attempts_from_retries(Some(1)), 1);
        assert_eq!(attempts_from_retries(Some(0)), 1);
        assert_eq!(attempts_from_retries(Some(-1)), 1);
        assert_eq!(attempts_from_retries(Some(i32::MIN)), 1);
        assert_eq!(attempts_from_retries(None), 1);
        assert_eq!(attempts_from_retries(Some(i32::MAX)), i32::MAX as u32);
    }

    #[test]
    fn a_session_level_falsy_retries_sends_exactly_one_request() {
        for retries in [Some(0), Some(-1), None] {
            let config = FetcherConfig::default().with_retries(retries);
            let policy = RetryPolicy::resolve(&config, &RequestOverrides::new());
            assert_eq!(policy.attempts, 1, "retries={retries:?}");
            assert_eq!(policy.decide(0), RetryDecision::GiveUp);
        }
    }

    #[test]
    fn a_per_request_retries_overrides_the_session() {
        let config = FetcherConfig::default().with_retries(Some(3));
        let policy = RetryPolicy::resolve(&config, &RequestOverrides::new().retries(Some(0)));
        assert_eq!(policy.attempts, 1);

        let explicit_none = RetryPolicy::resolve(&config, &RequestOverrides::new().retries(None));
        assert_eq!(explicit_none.attempts, 1);

        let more = RetryPolicy::resolve(&config, &RequestOverrides::new().retries(Some(5)));
        assert_eq!(more.attempts, 5);
    }

    #[test]
    fn the_default_policy_is_three_attempts_one_second_apart() {
        let policy = RetryPolicy::resolve(&FetcherConfig::default(), &RequestOverrides::new());
        assert_eq!(policy, RetryPolicy::default());
        assert_eq!(policy.attempts, 3);
        assert_eq!(policy.delay, Duration::from_secs(1));
        assert_eq!(
            policy.decide(0),
            RetryDecision::Retry {
                delay: Duration::from_secs(1)
            }
        );
        assert_eq!(
            policy.decide(1),
            RetryDecision::Retry {
                delay: Duration::from_secs(1)
            }
        );
        assert_eq!(policy.decide(2), RetryDecision::GiveUp);
        assert!(policy.should_retry(0));
        assert!(!policy.should_retry(2));
        // Defensive: an out-of-range index never loops forever.
        assert_eq!(policy.decide(99), RetryDecision::GiveUp);
    }

    #[test]
    fn the_delay_is_fixed_with_no_backoff() {
        let config = FetcherConfig::default().with_retry_delay(Duration::from_millis(250));
        let policy = RetryPolicy::resolve(&config, &RequestOverrides::new());
        let delays: Vec<_> = (0..policy.attempts - 1).map(|a| policy.decide(a)).collect();
        assert!(delays.iter().all(|decision| *decision
            == RetryDecision::Retry {
                delay: Duration::from_millis(250)
            }));

        let overridden = RetryPolicy::resolve(
            &config,
            &RequestOverrides::new().retry_delay(Duration::ZERO),
        );
        assert_eq!(overridden.delay, Duration::ZERO);
    }

    #[test]
    fn the_rotator_rotates_on_every_attempt() {
        let rotator =
            Arc::new(ProxyRotator::from_urls(["http://p1:8080", "http://p2:8080"]).unwrap());
        let config = FetcherConfig::default().with_proxy_rotator(Some(rotator));
        let overrides = RequestOverrides::new();
        let policy = RetryPolicy::new(Some(2), Duration::ZERO);

        let drawn: Vec<_> = (0..policy.attempts)
            .map(|_| proxy_for_attempt(&config, &overrides))
            .collect();
        assert_eq!(
            drawn,
            vec![
                Some(Proxy::Url("http://p1:8080".into())),
                Some(Proxy::Url("http://p2:8080".into())),
            ]
        );
    }

    #[test]
    fn a_per_request_proxy_disables_the_rotator() {
        let rotator = Arc::new(ProxyRotator::from_urls(["http://p1", "http://p2"]).unwrap());
        let config = FetcherConfig::default().with_proxy_rotator(Some(Arc::clone(&rotator)));
        let overrides = RequestOverrides::new().proxy_url("http://pinned");

        for _ in 0..3 {
            assert_eq!(
                proxy_for_attempt(&config, &overrides),
                Some(Proxy::Url("http://pinned".into()))
            );
        }
        assert_eq!(
            rotator.current_index(),
            0,
            "the rotator was never consulted"
        );
    }

    #[test]
    fn a_falsy_per_request_proxy_still_disables_the_rotator() {
        // Python guards on `static_proxy is None`, not on truthiness, so
        // `proxy=""` takes the *else* branch: `"" or self._default_proxy`, which with
        // a rotator configured is `None`. The rotator must not be advanced.
        let rotator = Arc::new(ProxyRotator::from_urls(["http://p1", "http://p2"]).unwrap());
        let config = FetcherConfig::default().with_proxy_rotator(Some(Arc::clone(&rotator)));

        assert_eq!(
            proxy_for_attempt(&config, &RequestOverrides::new().proxy_url("")),
            None
        );
        assert_eq!(
            rotator.current_index(),
            0,
            "the rotator was never consulted"
        );

        // An explicit `proxy=None` *is* `None`, so the rotator does run.
        assert_eq!(
            proxy_for_attempt(&config, &RequestOverrides::new().proxy(None)),
            Some(Proxy::Url("http://p1".into()))
        );
        assert_eq!(rotator.current_index(), 1);
    }

    #[test]
    fn without_a_rotator_the_same_proxy_is_reused() {
        let config = FetcherConfig::default().with_proxy_url("http://session");
        let overrides = RequestOverrides::new();
        assert_eq!(
            proxy_for_attempt(&config, &overrides),
            Some(Proxy::Url("http://session".into()))
        );
        assert_eq!(
            proxy_for_attempt(&config, &overrides),
            Some(Proxy::Url("http://session".into()))
        );

        // A per-request proxy still wins over the session's.
        assert_eq!(
            proxy_for_attempt(
                &config,
                &RequestOverrides::new().proxy_url("http://request")
            ),
            Some(Proxy::Url("http://request".into()))
        );
        // …and an explicit empty/None per-request value falls back, matching
        // Python's `static_proxy or self._default_proxy`.
        assert_eq!(
            proxy_for_attempt(&config, &RequestOverrides::new().proxy(None)),
            Some(Proxy::Url("http://session".into()))
        );
        assert_eq!(
            proxy_for_attempt(&config, &RequestOverrides::new().proxy_url("")),
            Some(Proxy::Url("http://session".into()))
        );
    }

    #[test]
    fn no_proxy_configured_yields_none() {
        assert_eq!(
            proxy_for_attempt(&FetcherConfig::default(), &RequestOverrides::new()),
            None
        );
        // A `proxies` table is a separate curl option and is not folded in here.
        let config = FetcherConfig::default().with_proxies(ProxySpec::all("http://spec"));
        assert_eq!(proxy_for_attempt(&config, &RequestOverrides::new()), None);
    }

    #[test]
    fn log_messages_match_python() {
        let proxy = Proxy::Url("http://p1:8080".into());
        assert_eq!(
            retry_warning(
                0,
                &"Connection refused by proxy",
                Some(&proxy),
                Duration::from_secs(1)
            ),
            "Proxy 'http://p1:8080' failed (attempt 1) | Retrying in 1 seconds..."
        );
        assert_eq!(
            retry_warning(2, &"CurlError: timed out", None, Duration::from_secs(2)),
            "Attempt 3 failed: CurlError: timed out. Retrying in 2 seconds..."
        );
        assert_eq!(
            failure_message(3, &"CurlError: timed out"),
            "Failed after 3 attempts: CurlError: timed out"
        );
    }

    #[test]
    fn a_proxy_message_hides_dictionary_credentials() {
        let proxy = Proxy::Config {
            server: "http://p:8080".into(),
            username: Some("user".into()),
            password: Some("hunter2".into()),
        };
        let message = retry_warning(0, &"connection reset", Some(&proxy), Duration::ZERO);
        assert!(message.contains("http://p:8080"));
        assert!(!message.contains("hunter2"));
    }

    #[test]
    fn logging_helpers_do_not_panic_without_a_subscriber() {
        log_retry(0, &"boom", None, Duration::ZERO);
        log_failure(1, &"boom");
    }
}
