//! `SessionRegistry`: named, persistent browser sessions and their profile dirs.
//!
//! A session is a **name** plus a **directory**. The name is what an agent
//! writes in every request body; the directory is a real Chromium profile under
//! [`crate::config::ServerConfig::profile_root`], so a login performed once
//! through the dashboard is still there after the next request *and* after the
//! next restart. `API.md` §1 calls that persistence the point of the whole
//! design, and everything in this module exists to make it true:
//!
//! * **Reattaching by name never wipes the profile.** `BrowserSession` only
//!   deletes a profile directory it created itself (`user_data_dir = ""`);
//!   naming one hands ownership to us, so [`SessionRegistry::open`] on an
//!   existing name is a no-op on disk.
//! * **HTTP cookies persist too.** The browser's cookies live in the profile,
//!   but a `fetch` never launches a browser, and its jar is process memory.
//!   [`CookieStore`] mirrors every `Set-Cookie` the HTTP fetcher sees into
//!   `cookies.json` inside the same profile directory and seeds the jar from it
//!   at open time. See that type for what the mirror does and does not model.
//! * **Live browsers are capped and swept.** Each one is a Chrome process, so a
//!   session takes a permit from a semaphore before it launches and holds it
//!   until it closes, and an idle session is closed for you.
//!
//! Nothing here is an operation. Everything an operation *does* is in
//! [`crate::engine`]; this module only owns the identity, the directory, the
//! two transports and their lifetimes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use browser::{
    AsyncFetcherSession, BrowserConfig, BrowserSession, FetcherConfig, Impersonate, Proxy,
    Response, SessionHooks,
};
// The two CDP-level names, taken from `browser-cdp` directly rather than
// through the façade: `CdpPage` is not on the curated surface, and
// `browser::browser::cdp::CdpPage` reads like a typo.
use browser_cdp::cdp::CdpPage;
use browser_cdp::session::page_hook;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::config::ServerConfig;
use crate::error::{Result, ServerError};

/// The file, inside a session's profile directory, that mirrors the HTTP jar.
pub const COOKIE_FILE: &str = "cookies.json";

// ---------------------------------------------------------------------------
// Egress
// ---------------------------------------------------------------------------

/// What a session looks like from the outside: which way out, and as whom.
///
/// Both halves are surfaced rather than invented — the proxy is
/// [`browser_core::types::Proxy`], which `browser-http` and `browser-cdp`
/// already take, and `impersonate` is a `browser-fingerprint` profile name that
/// `browser-http` forwards verbatim to `wreq`. "IP spoofing" in this project
/// means choosing the egress proxy, and this is where the choice is recorded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Egress {
    /// The proxy, or `None` to fall back to the server default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// The TLS/HTTP2 impersonation profile, e.g. `"chrome"` or `"firefox147"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,
    /// The User-Agent, which the browser gets as a launch option and the HTTP
    /// fetcher as a header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl Egress {
    /// Fill every unset field from the server defaults.
    ///
    /// This is the precedence `API.md` §1 asks for, written once: **request,
    /// else session, else server default, else rotator.** The rotator is the
    /// tail of that chain and lives in `browser-http`'s own config — leaving
    /// `proxy` `None` here is exactly what lets it run.
    #[must_use]
    pub fn or_defaults(mut self, config: &ServerConfig) -> Self {
        if self.proxy.is_none() {
            self.proxy.clone_from(&config.default_proxy);
        }
        if self.impersonate.is_none() {
            self.impersonate.clone_from(&config.default_impersonate);
        }
        if self.user_agent.is_none() {
            self.user_agent.clone_from(&config.default_user_agent);
        }
        self
    }

    /// Overlay `other`'s set fields on top of these.
    #[must_use]
    pub fn overlay(mut self, other: &Self) -> Self {
        if other.proxy.is_some() {
            self.proxy.clone_from(&other.proxy);
        }
        if other.impersonate.is_some() {
            self.impersonate.clone_from(&other.impersonate);
        }
        if other.user_agent.is_some() {
            self.user_agent.clone_from(&other.user_agent);
        }
        self
    }

    /// The [`FetcherConfig`] this egress describes.
    #[must_use]
    pub fn fetcher_config(&self) -> FetcherConfig {
        let mut config = FetcherConfig::new();
        config.proxy.clone_from(&self.proxy);
        if let Some(profile) = &self.impersonate {
            config.impersonate = Impersonate::single(profile.clone());
        }
        if let Some(agent) = &self.user_agent {
            config
                .headers
                .insert("User-Agent".to_owned(), agent.clone());
        }
        config
    }

    /// The [`BrowserConfig`] this egress describes, rooted at `profile_dir`.
    ///
    /// `impersonate` is deliberately **not** applied here: it is a TLS/HTTP2
    /// fingerprint that `wreq` forges, and a real Chrome already presents a
    /// real Chrome handshake. Forcing a Firefox JA3 onto a Chromium process is
    /// not something CDP can do, and pretending otherwise would be a lie in the
    /// session listing.
    #[must_use]
    pub fn browser_config(&self, profile_dir: &Path, headless: bool) -> BrowserConfig {
        BrowserConfig {
            headless,
            user_data_dir: profile_dir.to_string_lossy().into_owned(),
            proxy: self.proxy.clone(),
            useragent: self.user_agent.clone(),
            ..BrowserConfig::default()
        }
    }
}

/// Where a [`CdpPage`] handle is parked between the hook that produces it and
/// whoever needs it.
pub type PageSlot = Arc<Mutex<Option<CdpPage>>>;

/// Hooks whose only job is to remember the tab a fetch ran on.
///
/// `browser-cdp` hands a page out in exactly one place — the `page_action`
/// hook — and `BrowserSession::fetch` gives back a [`Response`], not a tab. So
/// this is the *only* seam through which anything above `browser-cdp` can reach
/// a live page, and it is what `/api/screenshot` captures from and what the
/// dashboard's screencast and input need.
///
/// `page_action` runs after navigation and the stability wait, the latest point
/// `browser-cdp` offers. Doing work inside it would run that work on every
/// fetch; storing the handle costs an `Arc` clone and lets the caller decide.
#[must_use]
pub fn page_slot_hooks(slot: &PageSlot) -> SessionHooks {
    let slot = Arc::clone(slot);
    SessionHooks::new().page_action(page_hook(move |page| {
        let slot = Arc::clone(&slot);
        async move {
            *slot.lock().expect("not poisoned") = Some(page);
            Ok(())
        }
    }))
}

/// A proxy rendered for a listing, with any credentials removed.
///
/// `browser_cdp::session::describe_proxy` returns the URL verbatim, which is
/// right for a local retry warning and wrong for an HTTP response: a proxy URL
/// routinely carries `user:pass@`, and `GET /api/sessions` is how an agent
/// discovers what exists.
///
/// ```
/// use browser::Proxy;
/// use browser_server::session::redact_proxy;
///
/// assert_eq!(
///     redact_proxy(&Proxy::Url("http://alice:hunter2@egress.test:8080".into())),
///     "http://egress.test:8080/",
/// );
/// ```
#[must_use]
pub fn redact_proxy(proxy: &Proxy) -> String {
    let raw = match proxy {
        Proxy::Url(url) => url.clone(),
        Proxy::Config { server, .. } => server.clone(),
    };
    match Url::parse(&raw) {
        Ok(mut url) => {
            // A URL with no authority rejects both setters; there is nothing to
            // redact in that case either, so failure is simply ignored.
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        // Unparseable: report the scheme-ish prefix only rather than guess.
        Err(_) => "<proxy>".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// The cookie mirror
// ---------------------------------------------------------------------------

/// The HTTP cookie jar's on-disk mirror, one file per session profile.
///
/// # What it models, and what it does not
///
/// `browser_http::session::CookieJar` implements the parts of RFC 6265 that
/// matter — domain matching, path matching, `Secure`, `Max-Age` — but it is
/// process memory and exposes no iteration, so it cannot be serialised. This
/// type is therefore not a serialisation of the jar; it is a **record of what
/// was observed**: for each origin the session fetched, the `name=value` pairs
/// that origin's `Set-Cookie` headers last carried.
///
/// On [`SessionRegistry::open`] those pairs are replayed into a fresh jar as
/// `name=value; Path=/` against their origin, which is where the approximation
/// lives:
///
/// * a cookie set for a parent domain (`Domain=.example.com`) comes back scoped
///   to the exact host it was seen on, so it is *narrower* than it was;
/// * a cookie set for a sub-path comes back at `/`, so it is *wider*;
/// * expiry is not replayed — a restart resurrects a cookie that would have
///   expired while the process was down.
///
/// The direction of each error is the safe one for a scraping session: a
/// too-narrow cookie fails to authenticate and the agent logs in again, and a
/// resurrected one is rejected by the server and replaced. It is a convenience
/// for the browserless path; the **browser** path needs none of this, because
/// Chromium's own cookie database lives in the same profile directory and is
/// authoritative.
#[derive(Debug, Default)]
pub struct CookieStore {
    path: PathBuf,
    origins: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
}

impl CookieStore {
    /// Read the mirror for a profile directory, or start an empty one.
    ///
    /// A missing or corrupt file is not an error: the session simply starts
    /// with no cookies, which is the state it would have been in anyway.
    #[must_use]
    pub fn load(profile_dir: &Path) -> Self {
        let path = profile_dir.join(COOKIE_FILE);
        let origins = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            origins: Mutex::new(origins),
        }
    }

    /// Every origin the mirror knows, with its pairs.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.origins.lock().expect("not poisoned").clone()
    }

    /// Record the `Set-Cookie` pairs of one response.
    ///
    /// Returns `true` when something changed, so the caller can skip a write.
    fn record(&self, url: &str, cookies: &browser::http::config::HeaderDict) -> bool {
        if cookies.is_empty() {
            return false;
        }
        let Some(origin) = origin_of(url) else {
            return false;
        };
        let mut origins = self.origins.lock().expect("not poisoned");
        let entry = origins.entry(origin).or_default();
        let mut changed = false;
        for (name, value) in cookies {
            if entry.get(name) != Some(value) {
                entry.insert(name.clone(), value.clone());
                changed = true;
            }
        }
        changed
    }

    /// Write the mirror out.
    ///
    /// Best-effort: a session that cannot persist its cookies is still a
    /// working session, so a failure is logged and swallowed rather than
    /// turning a successful fetch into a 500.
    async fn save(&self) {
        let snapshot = self.snapshot();
        let Ok(text) = serde_json::to_string_pretty(&snapshot) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            if let Err(error) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!(%error, "could not create the session profile directory");
                return;
            }
        }
        if let Err(error) = tokio::fs::write(&self.path, text).await {
            tracing::warn!(%error, "could not persist the session cookie mirror");
        }
    }

    /// Replay the mirror into a jar.
    fn seed(&self, session: &AsyncFetcherSession) {
        for (origin, pairs) in self.snapshot() {
            let Ok(url) = Url::parse(&origin) else {
                continue;
            };
            for (name, value) in pairs {
                session
                    .cookies()
                    .store(&url, &format!("{name}={value}; Path=/"));
            }
        }
    }
}

/// `scheme://host[:port]` for a URL, or `None` if it is not one.
fn origin_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

// ---------------------------------------------------------------------------
// A session
// ---------------------------------------------------------------------------

/// One named session: a profile directory, an HTTP jar and, when something
/// needs it, a live browser.
#[derive(Debug)]
pub struct Session {
    /// The name an agent addresses this session by. Never sanitised — only the
    /// directory is.
    pub name: String,
    /// The persistent Chromium profile directory.
    pub profile_dir: PathBuf,
    /// The egress and fingerprint this session was opened with.
    pub egress: Egress,
    /// The HTTP fetcher, whose cookie jar is what makes two `fetch` calls with
    /// the same session name share a login.
    http: AsyncFetcherSession,
    cookies: CookieStore,
    /// `None` until something asks for a browser; a permit-holding handle after.
    browser: tokio::sync::Mutex<Option<LiveBrowser>>,
    /// The tab the last browser operation used. See [`Session::live_page`].
    live_page: PageSlot,
    created_at: SystemTime,
    last_used: Mutex<Instant>,
    requests: AtomicU64,
    headless: bool,
    max_browsers: usize,
}

/// A launched browser and the slot permit that entitles it to exist.
///
/// The permit is dropped with the struct, so every path that ends a browser —
/// an explicit close, the idle sweep, the registry being dropped — returns the
/// slot without anyone having to remember to.
#[derive(Debug)]
struct LiveBrowser {
    session: Arc<BrowserSession>,
    _permit: OwnedSemaphorePermit,
}

impl Session {
    /// The HTTP fetcher, cookie jar and all.
    #[must_use]
    pub fn http(&self) -> &AsyncFetcherSession {
        &self.http
    }

    /// The cookie mirror.
    #[must_use]
    pub fn cookies(&self) -> &CookieStore {
        &self.cookies
    }

    /// Mark the session used, resetting its idle clock.
    pub fn touch(&self) {
        *self.last_used.lock().expect("not poisoned") = Instant::now();
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// How long since the last [`Session::touch`].
    #[must_use]
    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().expect("not poisoned").elapsed()
    }

    /// Note a finished response: mirror its cookies and persist them.
    pub async fn observe(&self, response: &Response) {
        if self.cookies.record(&response.url, &response.cookies) {
            self.cookies.save().await;
        }
    }

    /// Whether a browser is currently running for this session.
    pub async fn browser_is_live(&self) -> bool {
        self.browser.lock().await.is_some()
    }

    /// The live browser, launching one if this session has none.
    ///
    /// # Errors
    ///
    /// [`ServerError::BrowserLimit`] when every slot is taken, or whatever the
    /// launch failed with.
    pub async fn browser(&self, slots: &Arc<Semaphore>) -> Result<Arc<BrowserSession>> {
        let mut guard = self.browser.lock().await;
        if let Some(live) = guard.as_ref() {
            if live.session.is_alive().await {
                return Ok(Arc::clone(&live.session));
            }
            // The process died under us — Chrome crashed, or the machine slept.
            // Drop it (releasing the permit) and launch a fresh one rather than
            // handing back a handle whose every call will fail.
            tracing::warn!(session = %self.name, "the browser died; relaunching");
            *guard = None;
        }

        let permit =
            Arc::clone(slots)
                .try_acquire_owned()
                .map_err(|_| ServerError::BrowserLimit {
                    max: self.max_browsers,
                })?;

        let config = self.egress.browser_config(&self.profile_dir, self.headless);
        let session = Arc::new(BrowserSession::with_hooks(
            config,
            page_slot_hooks(&self.live_page),
        )?);
        session.start().await?;
        let session_handle = Arc::clone(&session);
        *guard = Some(LiveBrowser {
            session,
            _permit: permit,
        });
        Ok(session_handle)
    }

    /// The tab the most recent browser operation ran on, via
    /// [`page_slot_hooks`].
    ///
    /// [`CdpPage`] is an `Arc` handle: cloning it is free and the clone stays
    /// valid until the browser closes. `None` before the session's first
    /// browser operation.
    #[must_use]
    pub fn live_page(&self) -> Option<CdpPage> {
        self.live_page.lock().expect("not poisoned").clone()
    }

    /// Close the browser, if one is running, and give the slot back.
    pub async fn close_browser(&self) {
        if let Some(live) = self.browser.lock().await.take() {
            live.session.close().await;
        }
        // The handle would keep an `Arc` to a dead tab alive otherwise, and
        // `live_page()` would hand it to a caller whose every call fails.
        *self.live_page.lock().expect("not poisoned") = None;
    }

    /// A listable description of this session.
    pub async fn info(&self) -> SessionInfo {
        SessionInfo {
            name: self.name.clone(),
            profile_dir: self.profile_dir.to_string_lossy().into_owned(),
            proxy: self.egress.proxy.as_ref().map(redact_proxy),
            impersonate: self.egress.impersonate.clone(),
            user_agent: self.egress.user_agent.clone(),
            browser_live: self.browser_is_live().await,
            created_at: self
                .created_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            idle_secs: self.idle_for().as_secs(),
            requests: self.requests.load(Ordering::Relaxed),
        }
    }
}

/// What `GET /api/sessions` returns, one per session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The session name.
    pub name: String,
    /// Its persistent profile directory.
    pub profile_dir: String,
    /// The egress proxy, credentials redacted. `None` means the server default.
    pub proxy: Option<String>,
    /// The impersonation profile, if the session pins one.
    pub impersonate: Option<String>,
    /// The User-Agent, if the session pins one.
    pub user_agent: Option<String>,
    /// Whether a Chrome process is running for it right now.
    pub browser_live: bool,
    /// When it was opened, as seconds since the Unix epoch.
    pub created_at: u64,
    /// Seconds since its last use.
    pub idle_secs: u64,
    /// How many operations have run on it.
    pub requests: u64,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Every open session, keyed by name.
#[derive(Debug)]
pub struct SessionRegistry {
    config: Arc<ServerConfig>,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    /// One permit per live browser. See [`Session::browser`].
    slots: Arc<Semaphore>,
}

impl SessionRegistry {
    /// A registry over a configuration.
    #[must_use]
    pub fn new(config: Arc<ServerConfig>) -> Self {
        let slots = Arc::new(Semaphore::new(config.max_live_browsers.max(1)));
        Self {
            config,
            sessions: Mutex::new(BTreeMap::new()),
            slots,
        }
    }

    /// The browser-slot semaphore, for [`Session::browser`].
    #[must_use]
    pub fn slots(&self) -> &Arc<Semaphore> {
        &self.slots
    }

    /// Open a session, or reattach to the one that already has this name.
    ///
    /// Reattaching keeps the existing profile directory, the existing cookie
    /// jar and the existing browser. `egress` is applied **only** when the
    /// session is created: changing the proxy of a logged-in session behind its
    /// own back is not something a caller can ask for by accident, and the
    /// honest way to do it is to close the session and open it again.
    ///
    /// # Errors
    ///
    /// [`ServerError::BadRequest`] for an empty name, or an internal error when
    /// the profile directory cannot be created or the fetcher cannot be built.
    pub async fn open(&self, name: &str, egress: Egress) -> Result<Arc<Session>> {
        if name.trim().is_empty() {
            return Err(ServerError::BadRequest(
                "a session needs a non-empty name".to_owned(),
            ));
        }
        // Sweeping here is what keeps a busy server tidy without a background
        // task; `sweep_and_close` rather than `sweep_idle` because an expired
        // session's Chrome has to be *stopped*, not merely forgotten.
        self.sweep_and_close().await;

        if let Some(existing) = self.get(name) {
            existing.touch();
            return Ok(existing);
        }

        let profile_dir = self.config.profile_dir(name);
        std::fs::create_dir_all(&profile_dir).map_err(|error| {
            ServerError::internal(format!("profile directory {profile_dir:?}: {error}"))
        })?;

        let egress = egress.or_defaults(&self.config);
        let http = AsyncFetcherSession::builder()
            .config(egress.fetcher_config())
            .build()
            .map_err(ServerError::from)?;
        let cookies = CookieStore::load(&profile_dir);
        cookies.seed(&http);

        let session = Arc::new(Session {
            name: name.to_owned(),
            profile_dir,
            egress,
            http,
            cookies,
            browser: tokio::sync::Mutex::new(None),
            live_page: Arc::new(Mutex::new(None)),
            created_at: SystemTime::now(),
            last_used: Mutex::new(Instant::now()),
            requests: AtomicU64::new(0),
            headless: self.config.headless,
            max_browsers: self.config.max_live_browsers.max(1),
        });
        self.sessions
            .lock()
            .expect("not poisoned")
            .insert(name.to_owned(), Arc::clone(&session));
        tracing::info!(session = %name, "session opened");
        Ok(session)
    }

    /// The session with this name, if it is open.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .expect("not poisoned")
            .get(name)
            .map(Arc::clone)
    }

    /// Every open session, name-ordered.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<Session>> {
        self.sessions
            .lock()
            .expect("not poisoned")
            .values()
            .map(Arc::clone)
            .collect()
    }

    /// Close a session: stop its browser, persist its cookies, forget the name.
    ///
    /// The **profile directory stays**. That is not an oversight — it is the
    /// feature. `false` when no session had that name.
    pub async fn close(&self, name: &str) -> bool {
        let removed = self.sessions.lock().expect("not poisoned").remove(name);
        match removed {
            Some(session) => {
                session.close_browser().await;
                session.cookies.save().await;
                tracing::info!(session = %name, "session closed");
                true
            }
            None => false,
        }
    }

    /// Close every session whose idle time has passed the TTL.
    ///
    /// Called on every [`SessionRegistry::open`] so a busy server sweeps itself,
    /// and periodically by [`SessionRegistry::run_sweeper`] so an idle one does
    /// too. Detaching under the lock and closing outside it is deliberate: a
    /// browser close is slow and awaiting it while holding the map would stall
    /// every other request.
    pub fn sweep_idle(&self) -> Vec<Arc<Session>> {
        let ttl = self.config.session_idle_ttl;
        if ttl.is_zero() {
            return Vec::new();
        }
        let mut expired = Vec::new();
        let mut sessions = self.sessions.lock().expect("not poisoned");
        sessions.retain(|name, session| {
            if session.idle_for() >= ttl {
                tracing::info!(session = %name, "session expired");
                expired.push(Arc::clone(session));
                false
            } else {
                true
            }
        });
        expired
    }

    /// Sweep, then actually shut the expired sessions down.
    pub async fn sweep_and_close(&self) {
        for session in self.sweep_idle() {
            session.close_browser().await;
            session.cookies.save().await;
        }
    }

    /// The maintenance loop: sweep every `interval` for ever.
    ///
    /// Spawned by the server binary. Nothing else needs it — the lazy sweep in
    /// [`SessionRegistry::open`] keeps a *busy* server tidy, and this keeps an
    /// idle one from holding a Chrome process open all night.
    pub async fn run_sweeper(self: Arc<Self>, interval: Duration) {
        let mut ticker = tokio::time::interval(interval.max(Duration::from_secs(1)));
        // The first tick fires immediately; skip it so a fresh server does not
        // sweep the session that was opened a microsecond ago.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            self.sweep_and_close().await;
        }
    }

    /// Close everything. Used when the process is shutting down.
    pub async fn close_all(&self) {
        let sessions: Vec<Arc<Session>> =
            std::mem::take(&mut *self.sessions.lock().expect("not poisoned"))
                .into_values()
                .collect();
        for session in sessions {
            session.close_browser().await;
            session.cookies.save().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> Arc<ServerConfig> {
        Arc::new(ServerConfig {
            profile_root: root.to_path_buf(),
            ..ServerConfig::default()
        })
    }

    #[tokio::test]
    async fn opening_the_same_name_twice_returns_the_same_session() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = SessionRegistry::new(config(dir.path()));

        let first = registry
            .open("shop", Egress::default())
            .await
            .expect("opened");
        let second = registry
            .open("shop", Egress::default())
            .await
            .expect("reattached");
        assert!(Arc::ptr_eq(&first, &second), "reattach, not re-create");
        assert_eq!(registry.all().len(), 1);
    }

    /// The property the whole design rests on, at the directory level: a
    /// reattach must not disturb what is already in the profile.
    #[tokio::test]
    async fn reattaching_does_not_wipe_the_profile_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = SessionRegistry::new(config(dir.path()));

        let first = registry
            .open("shop", Egress::default())
            .await
            .expect("opened");
        let marker = first.profile_dir.join("Default");
        std::fs::create_dir_all(&marker).expect("a profile subdirectory");
        std::fs::write(marker.join("Cookies"), b"pretend-sqlite").expect("written");

        registry.close("shop").await;
        let reopened = registry
            .open("shop", Egress::default())
            .await
            .expect("reopened");

        assert_eq!(reopened.profile_dir, first.profile_dir);
        assert!(
            marker.join("Cookies").exists(),
            "closing and reopening must leave the profile alone"
        );
    }

    #[tokio::test]
    async fn an_empty_name_is_rejected() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = SessionRegistry::new(config(dir.path()));
        let error = registry
            .open("   ", Egress::default())
            .await
            .expect_err("rejected");
        assert_eq!(error.kind(), "bad_request");
    }

    #[tokio::test]
    async fn closing_an_unknown_session_is_false_not_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = SessionRegistry::new(config(dir.path()));
        assert!(!registry.close("nothing").await);
    }

    #[tokio::test]
    async fn an_idle_session_is_swept() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = SessionRegistry::new(Arc::new(ServerConfig {
            profile_root: dir.path().to_path_buf(),
            session_idle_ttl: Duration::from_millis(1),
            ..ServerConfig::default()
        }));
        registry
            .open("shop", Egress::default())
            .await
            .expect("opened");
        tokio::time::sleep(Duration::from_millis(5)).await;

        let expired = registry.sweep_idle();
        assert_eq!(expired.len(), 1);
        assert!(registry.get("shop").is_none());
    }

    /// The cookie mirror round-trips through the profile directory, which is
    /// what makes an HTTP login survive a restart.
    #[test]
    fn the_cookie_mirror_round_trips_through_the_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = CookieStore::load(dir.path());

        let mut observed = browser::http::config::HeaderDict::new();
        observed.insert("sid".to_owned(), "abc123".to_owned());
        assert!(store.record("http://shop.test/login", &observed));
        assert!(
            !store.record("http://shop.test/login", &observed),
            "an unchanged cookie is not a write"
        );

        // A synchronous write, because this test is not async.
        std::fs::write(
            dir.path().join(COOKIE_FILE),
            serde_json::to_string(&store.snapshot()).expect("serialisable"),
        )
        .expect("written");

        let reloaded = CookieStore::load(dir.path());
        assert_eq!(
            reloaded.snapshot()["http://shop.test"]["sid"],
            "abc123",
            "a restart finds the cookie where it left it"
        );
    }

    #[test]
    fn a_corrupt_mirror_is_an_empty_mirror_not_a_failure() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join(COOKIE_FILE), b"{ not json").expect("written");
        assert!(CookieStore::load(dir.path()).snapshot().is_empty());
    }

    #[test]
    fn an_origin_keeps_its_port() {
        assert_eq!(
            origin_of("http://127.0.0.1:8123/a/b?c=d"),
            Some("http://127.0.0.1:8123".to_owned())
        );
        assert_eq!(
            origin_of("https://shop.test/a"),
            Some("https://shop.test".to_owned())
        );
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn proxy_credentials_never_reach_a_listing() {
        let proxy = Proxy::Url("http://alice:hunter2@egress.test:8080".to_owned());
        let shown = redact_proxy(&proxy);
        assert!(!shown.contains("alice"));
        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("egress.test"));
    }

    #[test]
    fn egress_falls_back_to_the_server_defaults() {
        let config = ServerConfig {
            default_proxy: Some(Proxy::Url("http://egress.test:8080".to_owned())),
            default_impersonate: Some("firefox147".to_owned()),
            ..ServerConfig::default()
        };
        let egress = Egress {
            impersonate: Some("chrome".to_owned()),
            ..Egress::default()
        }
        .or_defaults(&config);

        assert_eq!(
            egress.impersonate.as_deref(),
            Some("chrome"),
            "the request wins"
        );
        assert_eq!(
            egress.proxy,
            Some(Proxy::Url("http://egress.test:8080".to_owned())),
            "and the server default fills the rest"
        );
    }

    #[test]
    fn the_browser_config_names_the_profile_and_never_the_fingerprint() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let egress = Egress {
            impersonate: Some("firefox147".to_owned()),
            user_agent: Some("UA/1".to_owned()),
            ..Egress::default()
        };
        let config = egress.browser_config(dir.path(), true);
        assert_eq!(config.user_data_dir, dir.path().to_string_lossy());
        assert_eq!(config.useragent.as_deref(), Some("UA/1"));
        assert!(config.headless);
    }
}
