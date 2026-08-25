//! `Fetch`-domain interception: the CDP replacement for `page.route()`.
//!
//! Python installs one glob route per page per request:
//!
//! ```python
//! page.unroute_all(behavior="ignoreErrors")
//! if disable_resources or blocked_domains:
//!     page.route("**/*", create_intercept_handler(disable_resources, blocked_domains))
//! ```
//!
//! and the handler either `route.abort()`s or `route.continue_()`s. The CDP
//! equivalent is `Fetch.enable` with a single `{urlPattern: "*"}` pattern, then
//! answering every `Fetch.requestPaused` with `Fetch.failRequest` or
//! `Fetch.continueRequest`. Unit 13 already owns the decision —
//! [`RoutePolicy::decide_cdp`] takes the raw CDP `resourceType` and the request
//! host — so this module is only the wiring.
//!
//! # Two things share the domain
//!
//! Proxy authentication also runs through `Fetch` (`Fetch.authRequired`), and
//! `Fetch.enable` is not additive: a second call replaces the first
//! configuration wholesale. One [`FetchInterceptor`] per tab therefore owns the
//! domain and merges both needs, so that `unroute_all` between requests cannot
//! knock out the proxy credentials.
//!
//! # While `Fetch` is enabled every request stops
//!
//! Chrome pauses *all* matching requests and waits for an answer. If the pump
//! task dies, the page hangs forever. The task is therefore spawned once per
//! tab, outlives individual requests, and answers with `continueRequest` for
//! anything it cannot classify.
//!
//! # `route.abort()` is `Failed`, not `BlockedByClient`
//!
//! `ARCHITECTURE.md` §13.4 suggests `BlockedByClient`, but Playwright's
//! `Route.abort()` defaults to the error code `"failed"`, which its Chromium
//! backend maps to `ErrorReason::Failed` — so a the reference implementation-blocked resource
//! reports `net::ERR_FAILED` to the page, not `net::ERR_BLOCKED_BY_CLIENT`.
//! Pages that branch on the error text (ad-block detectors do) would see the
//! difference, so [`ABORT_ERROR_REASON`] follows Playwright.

use std::sync::Arc;

use chromiumoxide::cdp::browser_protocol::fetch::{
    AuthChallengeResponse, AuthChallengeResponseResponse, ContinueRequestParams,
    ContinueWithAuthParams, DisableParams, EnableParams, EventAuthRequired, EventRequestPaused,
    FailRequestParams, RequestPattern, RequestStage,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use chromiumoxide::Page;
use futures::StreamExt;
use parking_lot::RwLock;
use tokio::task::JoinHandle;

use crate::error::{BrowserError, Result};
use crate::route::{BlockReason, RouteDecision, RoutePolicy};

/// The glob Python routes on, translated to CDP's wildcard syntax.
///
/// Playwright's `"**/*"` matches every URL; CDP's `urlPattern` uses `*` for
/// "zero or more characters", so the equivalent is a bare `*`.
pub const INTERCEPT_URL_PATTERN: &str = "*";

/// The CDP error reason behind Playwright's `route.abort()`.
///
/// See the module docs for why this is not `BlockedByClient`.
pub const ABORT_ERROR_REASON: ErrorReason = ErrorReason::Failed;

/// The `Fetch.enable` patterns one intercepting page installs.
///
/// A single request-stage pattern over every URL: Chrome then pauses each
/// request *before* it goes out, which is the only stage at which
/// `failRequest` can prevent the fetch from happening at all.
#[must_use]
pub fn interception_patterns() -> Vec<RequestPattern> {
    vec![RequestPattern {
        url_pattern: Some(INTERCEPT_URL_PATTERN.to_owned()),
        resource_type: None,
        request_stage: Some(RequestStage::Request),
    }]
}

/// The credentials `--proxy-server` needs when the proxy asks for them.
///
/// Playwright passes these as part of the context `proxy` option and answers
/// `Fetch.authRequired` itself; over raw CDP that is our job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyCredentials {
    /// Proxy username.
    pub username: String,
    /// Proxy password.
    pub password: String,
}

impl ProxyCredentials {
    /// The credentials of a normalised proxy, if it carries any.
    ///
    /// A proxy with an empty username *and* an empty password carries none —
    /// `construct_proxy_dict` fills both with `""` rather than dropping them.
    #[must_use]
    pub fn from_proxy(proxy: &browser_core::types::Proxy) -> Option<Self> {
        let browser_core::types::Proxy::Config {
            username, password, ..
        } = proxy
        else {
            return None;
        };
        let username = username.clone().unwrap_or_default();
        let password = password.clone().unwrap_or_default();
        if username.is_empty() && password.is_empty() {
            return None;
        }
        Some(Self { username, password })
    }
}

/// What one tab's `Fetch` domain is currently doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InterceptSettings {
    /// The per-request blocking policy, or `None` between requests
    /// (`unroute_all`).
    pub policy: Option<RoutePolicy>,
    /// Proxy credentials, which outlive individual requests.
    pub credentials: Option<ProxyCredentials>,
}

impl InterceptSettings {
    /// Whether `Fetch` has to be enabled at all.
    ///
    /// Python's guard is `if disable_resources or blocked_domains`; a tab
    /// behind an authenticating proxy needs the domain regardless.
    #[must_use]
    pub fn is_needed(&self) -> bool {
        self.credentials.is_some()
            || self
                .policy
                .as_ref()
                .is_some_and(RoutePolicy::needs_interception)
    }
}

/// What to do with one `Fetch.requestPaused`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausedDecision {
    /// `Fetch.continueRequest` — Playwright's `route.continue_()`.
    Continue,
    /// `Fetch.failRequest` — Playwright's `route.abort()`.
    Fail(BlockReason),
}

impl PausedDecision {
    /// Whether the request is being aborted.
    #[must_use]
    pub const fn is_abort(self) -> bool {
        matches!(self, Self::Fail(_))
    }
}

/// The pure handler: one paused request in, one CDP answer out.
///
/// `cdp_resource_type` is `Fetch.requestPaused.resourceType` in CDP spelling
/// (`Image`, `Stylesheet`, `CSPViolationReport`, …); the Playwright translation
/// and the `EXTRA_RESOURCES` / blocklist decision both live in unit 13.
///
/// ```
/// use std::collections::HashSet;
/// use browser_cdp::cdp::fetch_domain::{decide_paused, PausedDecision};
/// use browser_cdp::route::{BlockReason, RoutePolicy};
///
/// let policy = RoutePolicy::new(true, Some(HashSet::from(["ads.test".to_owned()])));
/// assert_eq!(
///     decide_paused(&policy, "Image", "https://cdn.test/logo.png"),
///     PausedDecision::Fail(BlockReason::ResourceType),
/// );
/// assert_eq!(
///     decide_paused(&policy, "Document", "https://ads.test/"),
///     PausedDecision::Fail(BlockReason::BlockedDomain),
/// );
/// assert_eq!(
///     decide_paused(&policy, "Document", "https://shop.test/"),
///     PausedDecision::Continue,
/// );
/// ```
#[must_use]
pub fn decide_paused(policy: &RoutePolicy, cdp_resource_type: &str, url: &str) -> PausedDecision {
    let host = crate::route::host_of(url);
    let resource_type = crate::route::playwright_resource_type(cdp_resource_type);
    match policy.decide(resource_type, &host) {
        RouteDecision::Continue => PausedDecision::Continue,
        RouteDecision::Abort => policy
            .reason(resource_type, &host)
            .map_or(PausedDecision::Continue, PausedDecision::Fail),
    }
}

/// One tab's `Fetch` domain, plus the task that answers it.
///
/// Dropping the interceptor aborts the pump task; the tab is expected to be
/// closing at that point, because a paused request with nobody to answer it
/// hangs the page.
#[derive(Debug)]
pub struct FetchInterceptor {
    page: Page,
    settings: Arc<RwLock<InterceptSettings>>,
    enabled: Arc<RwLock<bool>>,
    pump: JoinHandle<()>,
}

impl Drop for FetchInterceptor {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl FetchInterceptor {
    /// Start the pump for a tab. `Fetch` itself stays disabled until
    /// [`FetchInterceptor::apply`] is given something to do.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when the event listeners cannot be registered.
    pub async fn install(page: &Page) -> Result<Self> {
        let settings = Arc::new(RwLock::new(InterceptSettings::default()));
        let enabled = Arc::new(RwLock::new(false));

        let mut paused = page
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Fetch.requestPaused listener".to_owned(),
                message: error.to_string(),
            })?;
        let mut auth = page
            .event_listener::<EventAuthRequired>()
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Fetch.authRequired listener".to_owned(),
                message: error.to_string(),
            })?;

        let pump_page = page.clone();
        let pump_settings = Arc::clone(&settings);
        let pump = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = paused.next() => match event {
                        Some(event) => {
                            let policy = pump_settings.read().policy.clone();
                            answer_paused(&pump_page, &event, policy.as_ref()).await;
                        }
                        None => break,
                    },
                    event = auth.next() => match event {
                        Some(event) => {
                            let credentials = pump_settings.read().credentials.clone();
                            answer_auth(&pump_page, &event, credentials.as_ref()).await;
                        }
                        None => break,
                    },
                }
            }
        });

        Ok(Self {
            page: page.clone(),
            settings,
            enabled,
            pump,
        })
    }

    /// The settings currently in force.
    #[must_use]
    pub fn settings(&self) -> InterceptSettings {
        self.settings.read().clone()
    }

    /// Whether `Fetch` is enabled on the tab right now.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        *self.enabled.read()
    }

    /// Install `settings`, enabling or disabling `Fetch` as needed.
    ///
    /// Both `page.route(...)` and `page.unroute_all(...)` funnel through here;
    /// the only difference is whether `settings.policy` is `Some`.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Fetch.enable` / `Fetch.disable` fails.
    pub async fn apply(&self, settings: InterceptSettings) -> Result<()> {
        let needed = settings.is_needed();
        *self.settings.write() = settings;
        let currently = *self.enabled.read();

        if needed {
            // `Fetch.enable` is idempotent but not additive: re-sending it
            // replaces the pattern list, which is exactly what a per-request
            // re-route wants. Re-sending an identical configuration is a
            // wasted round trip, so it is skipped.
            if !currently {
                let params = EnableParams {
                    patterns: Some(interception_patterns()),
                    handle_auth_requests: Some(true),
                };
                self.page
                    .execute(params)
                    .await
                    .map_err(|error| BrowserError::Cdp {
                        context: "Fetch.enable".to_owned(),
                        message: error.to_string(),
                    })?;
                *self.enabled.write() = true;
            }
        } else if currently {
            // Chrome auto-continues everything still paused, so this cannot
            // strand a request.
            self.page
                .execute(DisableParams::default())
                .await
                .map_err(|error| BrowserError::Cdp {
                    context: "Fetch.disable".to_owned(),
                    message: error.to_string(),
                })?;
            *self.enabled.write() = false;
        }
        Ok(())
    }

    /// `page.route("**/*", handler)` with a per-request policy.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn route(&self, policy: RoutePolicy) -> Result<()> {
        let credentials = self.settings.read().credentials.clone();
        self.apply(InterceptSettings {
            policy: Some(policy),
            credentials,
        })
        .await
    }

    /// `page.unroute_all(behavior="ignoreErrors")`.
    ///
    /// Proxy credentials survive, because they belong to the tab rather than to
    /// the request.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn unroute_all(&self) -> Result<()> {
        let credentials = self.settings.read().credentials.clone();
        self.apply(InterceptSettings {
            policy: None,
            credentials,
        })
        .await
    }

    /// Attach (or clear) the proxy credentials for this tab.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`].
    pub async fn set_credentials(&self, credentials: Option<ProxyCredentials>) -> Result<()> {
        let policy = self.settings.read().policy.clone();
        self.apply(InterceptSettings {
            policy,
            credentials,
        })
        .await
    }
}

async fn answer_paused(page: &Page, event: &EventRequestPaused, policy: Option<&RoutePolicy>) {
    let request_id = event.request_id.clone();
    let decision = policy.map_or(PausedDecision::Continue, |policy| {
        decide_paused(policy, event.resource_type.as_ref(), &event.request.url)
    });

    let outcome = match decision {
        PausedDecision::Fail(reason) => {
            // Python logs the same two lines at debug level from inside
            // `create_intercept_handler`.
            match reason {
                BlockReason::ResourceType => tracing::debug!(
                    "Blocked resource type {} at {}",
                    event.resource_type.as_ref(),
                    event.request.url
                ),
                BlockReason::BlockedDomain => {
                    tracing::debug!("Blocked domain request to {}", event.request.url);
                }
            }
            page.execute(FailRequestParams::new(request_id, ABORT_ERROR_REASON))
                .await
                .map(|_| ())
        }
        PausedDecision::Continue => page
            .execute(ContinueRequestParams::new(request_id))
            .await
            .map(|_| ()),
    };

    if let Err(error) = outcome {
        // A request that vanished before we answered (the tab navigated away)
        // is normal and must not kill the pump.
        tracing::debug!("Could not answer Fetch.requestPaused: {error}");
    }
}

async fn answer_auth(
    page: &Page,
    event: &EventAuthRequired,
    credentials: Option<&ProxyCredentials>,
) {
    let response = match credentials {
        Some(credentials) => AuthChallengeResponse {
            response: AuthChallengeResponseResponse::ProvideCredentials,
            username: Some(credentials.username.clone()),
            password: Some(credentials.password.clone()),
        },
        // No credentials: decline, so the request fails fast with
        // `net::ERR_INVALID_AUTH_CREDENTIALS` instead of stalling.
        None => AuthChallengeResponse {
            response: AuthChallengeResponseResponse::CancelAuth,
            username: None,
            password: None,
        },
    };
    if let Err(error) = page
        .execute(ContinueWithAuthParams::new(
            event.request_id.clone(),
            response,
        ))
        .await
    {
        tracing::debug!("Could not answer Fetch.authRequired: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use browser_core::types::Proxy;

    use super::*;

    fn blocklist(domains: &[&str]) -> Option<HashSet<String>> {
        Some(domains.iter().map(|d| (*d).to_owned()).collect())
    }

    #[test]
    fn the_pattern_list_is_one_request_stage_wildcard() {
        let patterns = interception_patterns();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].url_pattern.as_deref(), Some("*"));
        assert_eq!(patterns[0].request_stage, Some(RequestStage::Request));
        assert!(patterns[0].resource_type.is_none());
    }

    #[test]
    fn abort_uses_playwrights_default_error_code() {
        assert_eq!(ABORT_ERROR_REASON, ErrorReason::Failed);
    }

    #[test]
    fn resource_type_blocking_precedes_domain_blocking() {
        let policy = RoutePolicy::new(true, blocklist(&["safe.test"]));
        assert_eq!(
            decide_paused(&policy, "Image", "https://cdn.test/a.png"),
            PausedDecision::Fail(BlockReason::ResourceType)
        );
        assert_eq!(
            decide_paused(&policy, "Document", "https://safe.test/"),
            PausedDecision::Fail(BlockReason::BlockedDomain)
        );
        assert!(decide_paused(&policy, "Image", "https://safe.test/a.png").is_abort());
        assert_eq!(
            decide_paused(&policy, "Document", "https://cdn.test/"),
            PausedDecision::Continue
        );
    }

    #[test]
    fn subdomains_of_a_blocked_domain_are_blocked() {
        let policy = RoutePolicy::new(false, blocklist(&["doubleclick.net"]));
        assert!(decide_paused(
            &policy,
            "Script",
            "https://tracker.ads.doubleclick.net/x.js"
        )
        .is_abort());
        assert!(!decide_paused(&policy, "Script", "https://notdoubleclick.net/x.js").is_abort());
    }

    #[test]
    fn a_csp_report_survives_disable_resources() {
        // `EXTRA_RESOURCES` spells it `csp_report`, Playwright spells it
        // `cspreport`, so the set never matches. Bug-compatible on purpose.
        let policy = RoutePolicy::new(true, None);
        assert_eq!(
            decide_paused(&policy, "CSPViolationReport", "https://h/report"),
            PausedDecision::Continue
        );
    }

    #[test]
    fn an_unparsable_url_has_no_host_and_matches_no_blocklist() {
        let policy = RoutePolicy::new(false, blocklist(&["ads.test"]));
        assert_eq!(
            decide_paused(&policy, "Document", "not a url"),
            PausedDecision::Continue
        );
    }

    #[test]
    fn settings_are_needed_only_when_something_can_match() {
        assert!(!InterceptSettings::default().is_needed());
        assert!(
            !InterceptSettings {
                policy: Some(RoutePolicy::new(false, None)),
                credentials: None,
            }
            .is_needed(),
            "an empty policy is exactly Python's `if disable_resources or blocked_domains`"
        );
        assert!(InterceptSettings {
            policy: Some(RoutePolicy::new(true, None)),
            credentials: None,
        }
        .is_needed());
        assert!(
            InterceptSettings {
                policy: None,
                credentials: Some(ProxyCredentials {
                    username: "u".into(),
                    password: "p".into(),
                }),
            }
            .is_needed(),
            "an authenticating proxy needs the domain even between requests"
        );
    }

    #[test]
    fn credentials_are_taken_from_a_normalised_proxy() {
        assert_eq!(
            ProxyCredentials::from_proxy(&Proxy::Config {
                server: "http://h:1".into(),
                username: Some("u".into()),
                password: Some("p".into()),
            }),
            Some(ProxyCredentials {
                username: "u".into(),
                password: "p".into()
            })
        );
        assert_eq!(
            ProxyCredentials::from_proxy(&Proxy::Config {
                server: "http://h:1".into(),
                username: Some(String::new()),
                password: Some(String::new()),
            }),
            None,
            "`construct_proxy_dict` fills both with empty strings, which is no auth"
        );
        assert_eq!(
            ProxyCredentials::from_proxy(&Proxy::Url("http://u:p@h:1".into())),
            None,
            "an unnormalised proxy string is not this function's job"
        );
    }
}
