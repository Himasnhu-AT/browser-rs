//! The Cloudflare Turnstile / Interstitial solver.
//!
//! A port of `StealthySession._cloudflare_solver` (`browser.md` §10) with every
//! magic number preserved: a **5 000 ms** first idle budget, **3** solve
//! attempts, a **20 × 500 ms** spinner wait, a **100 × 100 ms** clearance wait,
//! click offsets of **x + 26..=28** and **y + 25..=27** from the widget's
//! top-left corner, and a **100..=200 ms** hold on the mouse button.
//!
//! # Detection is language-independent
//!
//! [`detect_cloudflare`] keys on the `cType: '<x>'` marker the challenge script
//! carries, never on visible text, so a French challenge page
//! (`<title>Un instant…</title>`) is detected exactly like an English one. This
//! is pinned by `tests/fetchers/test_cloudflare_solver.py`.
//!
//! # The one deliberate deviation
//!
//! Python has **two** unbounded `while` loops in this function: the
//! `frame_element().is_visible()` poll, and the `non-interactive` wait. Both
//! poll forever if the page never changes. `ARCHITECTURE.md` §7 (unit 14)
//! agrees the first should take the request `timeout` as a deadline, "because
//! an unbounded loop in an async task is a hang, not a slow request"; the same
//! argument applies verbatim to the second, so [`solve_cloudflare`] takes a
//! single `deadline` and bounds both. Everything else — including the bounded
//! `for _ in range(20)` and `attempts >= 100` loops — keeps Python's exact
//! counts.
//!
//! # Testing
//!
//! The solver drives the [`CloudflarePage`] trait rather than [`crate::cdp::CdpPage`]
//! directly, which is how the click-count assertions of
//! `test_cloudflare_solver.py` are reproduced without a browser or a live
//! challenge.

use std::time::Duration;

use rand::RngExt;
use tokio::time::Instant;

use crate::cdp::isolated_world::BoundingBox;
use crate::error::Result;

/// `__CF_MAX_SOLVE_ATTEMPTS__`.
pub const CF_MAX_SOLVE_ATTEMPTS: usize = 3;

/// `_wait_for_networkidle(page, timeout=5000)` — the solver's first wait.
pub const INITIAL_IDLE_BUDGET: Duration = Duration::from_millis(5_000);

/// `for _ in range(20)` — how many times the spinner wait polls.
pub const SPINNER_POLLS: usize = 20;

/// `page.wait_for_timeout(500)` inside that loop.
pub const SPINNER_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// `if attempts >= 100` — the post-click clearance budget, in polls.
pub const CLEARANCE_POLLS: usize = 100;

/// `page.wait_for_timeout(100)` inside that loop.
pub const CLEARANCE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `page.wait_for_timeout(1000)` in the `non-interactive` wait, and again when
/// no bounding box could be measured.
pub const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(1_000);

/// `randint(26, 28)` — the horizontal click offset, inclusive on both ends.
pub const CLICK_OFFSET_X: std::ops::RangeInclusive<i32> = 26..=28;

/// `randint(25, 27)` — the vertical click offset, inclusive on both ends.
pub const CLICK_OFFSET_Y: std::ops::RangeInclusive<i32> = 25..=27;

/// `delay=randint(100, 200)` — how long the left button is held down, in
/// milliseconds.
pub const CLICK_HOLD_MS: std::ops::RangeInclusive<u64> = 100..=200;

/// The widget selector for an **embedded** Turnstile.
pub const EMBEDDED_BOX_SELECTOR: &str = "#cf_turnstile div, #cf-turnstile div, .turnstile>div>div";

/// The widget selector for a full challenge page.
pub const CHALLENGE_BOX_SELECTOR: &str = ".main-content p+div>div>div";

/// The `<script src=…>` substring that marks an embedded Turnstile widget.
pub const TURNSTILE_SCRIPT_MARKER: &str = "challenges.cloudflare.com/turnstile/v";

/// The prefix half of `__CF_PATTERN__`, after the scheme.
const CF_FRAME_PREFIX: &str = "challenges.cloudflare.com/cdn-cgi/challenge-platform/";

/// `__CF_PATTERN__`: `^https?://challenges\.cloudflare\.com/cdn-cgi/challenge-platform/.*`
///
/// Hand-written rather than compiled, because the pattern is a literal prefix
/// with an alternation in the scheme — and because this is called in a poll
/// loop.
///
/// ```
/// use browser_cdp::cloudflare::is_challenge_frame_url;
///
/// assert!(is_challenge_frame_url(
///     "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/123456"
/// ));
/// assert!(is_challenge_frame_url(
///     "http://challenges.cloudflare.com/cdn-cgi/challenge-platform/orchestrate/jsch/v1"
/// ));
/// assert!(!is_challenge_frame_url("https://challenges.cloudflare.com/other-path"));
/// assert!(!is_challenge_frame_url("https://cloudflare.com/something"));
/// assert!(!is_challenge_frame_url("https://example.com/challenge"));
/// ```
#[must_use]
pub fn is_challenge_frame_url(url: &str) -> bool {
    // `re.match` is anchored at the start only, and `.*` covers the rest.
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|rest| rest.starts_with(CF_FRAME_PREFIX))
}

/// Which Cloudflare challenge a page is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChallengeType {
    /// `cType: 'non-interactive'` — the "just a moment" wait page.
    NonInteractive,
    /// `cType: 'managed'`.
    Managed,
    /// `cType: 'interactive'`.
    Interactive,
    /// A Turnstile widget embedded in an otherwise normal customer page,
    /// usually inside a closed shadow iframe.
    Embedded,
}

impl ChallengeType {
    /// The string Python's `_detect_cloudflare` returns.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NonInteractive => "non-interactive",
            Self::Managed => "managed",
            Self::Interactive => "interactive",
            Self::Embedded => "embedded",
        }
    }

    /// The `cType: '<x>'` marker for the three script-driven types.
    #[must_use]
    pub const fn marker(self) -> Option<&'static str> {
        match self {
            Self::NonInteractive => Some("cType: 'non-interactive'"),
            Self::Managed => Some("cType: 'managed'"),
            Self::Interactive => Some("cType: 'interactive'"),
            Self::Embedded => None,
        }
    }

    /// Which widget selector this challenge type is measured with.
    #[must_use]
    pub const fn box_selector(self) -> &'static str {
        match self {
            Self::Embedded => EMBEDDED_BOX_SELECTOR,
            _ => CHALLENGE_BOX_SELECTOR,
        }
    }
}

impl std::fmt::Display for ChallengeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The order `_detect_cloudflare` probes the markers in.
///
/// It matters: a page carrying two markers reports the first of these.
const MARKER_ORDER: [ChallengeType; 3] = [
    ChallengeType::NonInteractive,
    ChallengeType::Managed,
    ChallengeType::Interactive,
];

/// `_detect_cloudflare(page_content)`.
///
/// ```
/// use browser_cdp::cloudflare::{detect_cloudflare, ChallengeType};
///
/// let french = "<html><head><title>Un instant…</title></head>\
///               <body><script>cType: 'interactive'</script></body></html>";
/// assert_eq!(detect_cloudflare(french), Some(ChallengeType::Interactive));
/// assert_eq!(detect_cloudflare("<html><body>content</body></html>"), None);
///
/// let embedded = r#"<script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script>"#;
/// assert_eq!(detect_cloudflare(embedded), Some(ChallengeType::Embedded));
/// ```
#[must_use]
pub fn detect_cloudflare(page_content: &str) -> Option<ChallengeType> {
    for challenge in MARKER_ORDER {
        if let Some(marker) = challenge.marker() {
            if page_content.contains(marker) {
                return Some(challenge);
            }
        }
    }
    if has_turnstile_script(page_content) {
        return Some(ChallengeType::Embedded);
    }
    None
}

/// `Selector(content=page_content).css('script[src*="…turnstile/v"]')`.
///
/// The substring test is a strict pre-filter — the CSS selector cannot match
/// unless the substring appears somewhere — so the HTML is only parsed when it
/// might match. That keeps the 100-poll clearance loop cheap on the overwhelming
/// majority of pages, which contain no such string at all.
#[must_use]
pub fn has_turnstile_script(page_content: &str) -> bool {
    if !page_content.contains(TURNSTILE_SCRIPT_MARKER) {
        return false;
    }
    // `browser-http`'s `Response` is the only parser this crate can reach,
    // and building one over the markup is exactly what Python does with
    // `Selector(content=…)`.
    let document = browser_http::response::Response::builder(200, "about:blank")
        .body(page_content.to_owned())
        .build();
    match document.css(&format!("script[src*=\"{TURNSTILE_SCRIPT_MARKER}\"]")) {
        Ok(nodes) => !nodes.is_empty(),
        Err(error) => {
            tracing::debug!("Turnstile script probe failed to parse the page: {error}");
            false
        }
    }
}

/// `_challenge_cleared(page_content, challenge_type)`.
///
/// An embedded widget stays in the DOM after it is solved, so it always reports
/// cleared — otherwise the solver would click it forever.
///
/// ```
/// use browser_cdp::cloudflare::{challenge_cleared, ChallengeType};
///
/// let french = "<script>cType: 'interactive'</script>";
/// assert!(!challenge_cleared(french, ChallengeType::Interactive));
/// assert!(challenge_cleared("<html>clean</html>", ChallengeType::Interactive));
/// assert!(challenge_cleared(french, ChallengeType::Embedded));
/// ```
#[must_use]
pub fn challenge_cleared(page_content: &str, challenge_type: ChallengeType) -> bool {
    challenge_type == ChallengeType::Embedded || detect_cloudflare(page_content).is_none()
}

/// Where the solver clicks, given the widget's bounding box.
///
/// `outer_box["x"] + randint(26, 28)`, `outer_box["y"] + randint(25, 27)`.
#[must_use]
pub fn click_point(outer_box: BoundingBox) -> (f64, f64) {
    let mut rng = rand::rng();
    let x = outer_box.x + f64::from(rng.random_range(CLICK_OFFSET_X));
    let y = outer_box.y + f64::from(rng.random_range(CLICK_OFFSET_Y));
    (x, y)
}

/// `delay=randint(100, 200)`.
#[must_use]
pub fn click_hold() -> Duration {
    Duration::from_millis(rand::rng().random_range(CLICK_HOLD_MS))
}

/// The page operations the solver needs.
///
/// Frames are addressed by their CDP frame id as a plain `String` so the trait
/// stays free of `chromiumoxide` types and can be implemented by a test double.
#[async_trait::async_trait]
pub trait CloudflarePage: Send + Sync {
    /// `ResponseFactory._get_page_content(page)`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::PageContentUnavailable`] after the 20
    /// retries of the Windows workaround.
    async fn content(&self) -> Result<String>;

    /// `page.wait_for_timeout(ms)`.
    async fn wait_for_timeout(&self, duration: Duration);

    /// `page.wait_for_load_state()` — the default state, `"load"`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Timeout`].
    async fn wait_for_load_state(&self) -> Result<()>;

    /// `_wait_for_networkidle(page, timeout)`; `None` means the page default.
    /// Never fails — Python swallows the timeout.
    async fn wait_for_networkidle(&self, budget: Option<Duration>);

    /// `_wait_for_page_stability(page, load_dom, network_idle)`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Timeout`].
    async fn wait_for_page_stability(&self, load_dom: bool, network_idle: bool) -> Result<()>;

    /// `page.frame(url=__CF_PATTERN__)`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Cdp`].
    async fn challenge_frame(&self) -> Result<Option<String>>;

    /// `_wait_for_page_stability(iframe, load_dom, network_idle)`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Timeout`].
    async fn frame_wait_for_page_stability(
        &self,
        frame: &str,
        load_dom: bool,
        network_idle: bool,
    ) -> Result<()>;

    /// `iframe.frame_element().is_visible()`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Cdp`].
    async fn frame_element_is_visible(&self, frame: &str) -> Result<bool>;

    /// `iframe.frame_element().bounding_box()`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Cdp`].
    async fn frame_element_bounding_box(&self, frame: &str) -> Result<Option<BoundingBox>>;

    /// `page.locator(selector).last.bounding_box()`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Cdp`].
    async fn last_bounding_box(&self, selector: &str) -> Result<Option<BoundingBox>>;

    /// `page.mouse.click(x, y, delay=…, button="left")`.
    ///
    /// # Errors
    ///
    /// [`crate::error::BrowserError::Cdp`].
    async fn click(&self, x: f64, y: f64, hold: Duration) -> Result<()>;
}

/// `_cloudflare_solver(page)`.
///
/// Python recurses; every recursive call is in tail position, so this is the
/// same algorithm written as a loop over the attempt counter. `deadline` bounds
/// the two loops Python leaves unbounded (see the module docs) and is otherwise
/// unused.
///
/// Like Python's, this never reports "unsolved": it returns `Ok(())` from every
/// branch and lets the caller build a response out of whatever is on screen.
/// It *does* propagate the errors Python propagates — a page whose content
/// cannot be read, or a load state that times out.
///
/// # Errors
///
/// Whatever [`CloudflarePage`] returns.
pub async fn solve_cloudflare<P>(page: &P, deadline: Instant) -> Result<()>
where
    P: CloudflarePage + ?Sized,
{
    let mut attempts = 0usize;
    loop {
        page.wait_for_networkidle(Some(INITIAL_IDLE_BUDGET)).await;

        let Some(challenge_type) = detect_cloudflare(&page.content().await?) else {
            tracing::error!("No Cloudflare challenge found.");
            return Ok(());
        };
        if attempts >= CF_MAX_SOLVE_ATTEMPTS {
            tracing::error!(
                "Failed to solve the Cloudflare challenge after {attempts} attempts, \
                 returning the page as is"
            );
            return Ok(());
        }
        tracing::info!("The turnstile version discovered is \"{challenge_type}\"");

        if challenge_type == ChallengeType::NonInteractive {
            // Unbounded in Python; see the module docs for the deadline.
            while detect_cloudflare(&page.content().await?) == Some(ChallengeType::NonInteractive) {
                if Instant::now() >= deadline {
                    tracing::warn!(
                        "Cloudflare wait page outlived the request timeout; continuing anyway"
                    );
                    break;
                }
                tracing::info!("Waiting for Cloudflare wait page to disappear.");
                page.wait_for_timeout(SLOW_POLL_INTERVAL).await;
                page.wait_for_load_state().await?;
            }
            if challenge_cleared(&page.content().await?, challenge_type) {
                tracing::info!("Cloudflare captcha is solved");
                return Ok(());
            }
            attempts += 1;
            continue;
        }

        // managed | interactive | embedded
        let box_selector = challenge_type.box_selector();
        if challenge_type != ChallengeType::Embedded {
            for _ in 0..SPINNER_POLLS {
                // Wait for the verify spinner to resolve into the widget
                // iframe, or for the challenge to pass on its own.
                if page.challenge_frame().await?.is_some()
                    || challenge_cleared(&page.content().await?, challenge_type)
                {
                    break;
                }
                page.wait_for_timeout(SPINNER_POLL_INTERVAL).await;
            }
        }

        let mut outer_box: Option<BoundingBox> = None;
        let frame = page.challenge_frame().await?;
        if let Some(frame) = frame.as_deref() {
            page.frame_wait_for_page_stability(frame, true, false)
                .await?;
            if challenge_type != ChallengeType::Embedded {
                // *** The agreed deviation ***: Python polls here forever.
                while !page.frame_element_is_visible(frame).await? {
                    if Instant::now() >= deadline {
                        tracing::warn!(
                            "Cloudflare widget never became visible before the request timeout"
                        );
                        break;
                    }
                    page.wait_for_timeout(SPINNER_POLL_INTERVAL).await;
                }
            }
            outer_box = page.frame_element_bounding_box(frame).await?;
        }

        if frame.is_none() || outer_box.is_none() {
            if challenge_cleared(&page.content().await?, challenge_type) {
                tracing::info!("Cloudflare captcha is solved");
                return Ok(());
            }
            outer_box = page.last_bounding_box(box_selector).await?;
            if outer_box.is_none() {
                page.wait_for_timeout(SLOW_POLL_INTERVAL).await;
                attempts += 1;
                continue;
            }
        }

        let Some(measured) = outer_box else {
            // Unreachable: the branch above either found a box or continued.
            attempts += 1;
            continue;
        };

        // Calculate the captcha coordinates for any viewport, then press and
        // hold the left mouse button over the widget.
        let (x, y) = click_point(measured);
        page.click(x, y, click_hold()).await?;
        page.wait_for_networkidle(None).await;

        if challenge_type != ChallengeType::Embedded {
            let mut polls = 0usize;
            while !challenge_cleared(&page.content().await?, challenge_type) {
                if polls >= CLEARANCE_POLLS {
                    tracing::info!("Cloudflare page didn't disappear after 10s, continuing...");
                    break;
                }
                page.wait_for_timeout(CLEARANCE_POLL_INTERVAL).await;
                polls += 1;
            }
        }

        page.wait_for_page_stability(true, false).await?;

        if challenge_cleared(&page.content().await?, challenge_type) {
            tracing::info!("Cloudflare captcha is solved");
            return Ok(());
        }
        tracing::info!("Looks like Cloudflare captcha is still present, solving again");
        attempts += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    const FRENCH_CHALLENGE: &str = "<html><head><title>Un instant…</title></head><body>\
         <script>cType: 'interactive'</script></body></html>";
    const CLEAN_PAGE: &str =
        "<html><head><title>La Redoute</title></head><body>content</body></html>";

    // -- detection --------------------------------------------------------

    /// `TestChallengeCleared::test_localized_challenge_is_not_cleared`.
    #[test]
    fn test_localized_challenge_is_not_cleared() {
        assert!(!challenge_cleared(
            FRENCH_CHALLENGE,
            ChallengeType::Interactive
        ));
    }

    /// `TestChallengeCleared::test_clean_page_is_cleared`.
    #[test]
    fn test_clean_page_is_cleared() {
        assert!(challenge_cleared(CLEAN_PAGE, ChallengeType::Interactive));
    }

    /// `TestChallengeCleared::test_embedded_is_always_cleared`.
    #[test]
    fn test_embedded_is_always_cleared() {
        let content =
            r#"<script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script>"#;
        assert!(challenge_cleared(content, ChallengeType::Embedded));
    }

    #[test]
    fn every_challenge_type_is_detected_by_its_marker() {
        for challenge in MARKER_ORDER {
            let marker = challenge.marker().expect("a script marker");
            let page = format!("<html><script>{marker}</script></html>");
            assert_eq!(detect_cloudflare(&page), Some(challenge), "{challenge}");
        }
        assert_eq!(detect_cloudflare("<html><body>hi</body></html>"), None);
    }

    #[test]
    fn the_marker_probe_order_is_pythons() {
        // A page carrying several markers reports the first in Python's tuple.
        let both = "cType: 'managed' cType: 'non-interactive'";
        assert_eq!(detect_cloudflare(both), Some(ChallengeType::NonInteractive));
        let managed_and_interactive = "cType: 'interactive' cType: 'managed'";
        assert_eq!(
            detect_cloudflare(managed_and_interactive),
            Some(ChallengeType::Managed)
        );
    }

    #[test]
    fn the_turnstile_script_must_be_a_script_src() {
        // The substring alone is not enough — Python runs a CSS selector.
        let prose = "<p>we use challenges.cloudflare.com/turnstile/v0 here</p>";
        assert!(!has_turnstile_script(prose));
        assert_eq!(detect_cloudflare(prose), None);

        let real =
            r#"<script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script>"#;
        assert!(has_turnstile_script(real));
        assert_eq!(detect_cloudflare(real), Some(ChallengeType::Embedded));

        // A `src` on the wrong element does not count either.
        let wrong_element = r#"<img src="https://challenges.cloudflare.com/turnstile/v0/api.js">"#;
        assert!(!has_turnstile_script(wrong_element));
    }

    #[test]
    fn the_cf_pattern_matches_the_paths_python_pins() {
        for url in [
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/123456",
            "http://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/123456",
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/orchestrate/jsch/v1",
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/scripts/abc",
        ] {
            assert!(is_challenge_frame_url(url), "{url}");
        }
        for url in [
            "https://challenges.cloudflare.com/other-path",
            "https://cloudflare.com/something",
            "https://example.com/challenge",
            "ws://challenges.cloudflare.com/cdn-cgi/challenge-platform/x",
            "",
        ] {
            assert!(!is_challenge_frame_url(url), "{url}");
        }
    }

    #[test]
    fn box_selectors_follow_the_challenge_type() {
        assert_eq!(
            ChallengeType::Embedded.box_selector(),
            "#cf_turnstile div, #cf-turnstile div, .turnstile>div>div"
        );
        for challenge in MARKER_ORDER {
            assert_eq!(challenge.box_selector(), ".main-content p+div>div>div");
        }
    }

    #[test]
    fn the_click_point_is_offset_the_way_python_offsets_it() {
        let widget = BoundingBox {
            x: 100.0,
            y: 200.0,
            width: 300.0,
            height: 65.0,
        };
        for _ in 0..200 {
            let (x, y) = click_point(widget);
            assert!((126.0..=128.0).contains(&x), "{x}");
            assert!((225.0..=227.0).contains(&y), "{y}");
            let hold = click_hold();
            assert!(hold >= Duration::from_millis(100));
            assert!(hold <= Duration::from_millis(200));
        }
    }

    #[test]
    fn the_magic_numbers_are_the_ones_the_spec_pins() {
        assert_eq!(CF_MAX_SOLVE_ATTEMPTS, 3);
        assert_eq!(INITIAL_IDLE_BUDGET, Duration::from_millis(5_000));
        assert_eq!(SPINNER_POLLS, 20);
        assert_eq!(SPINNER_POLL_INTERVAL, Duration::from_millis(500));
        assert_eq!(CLEARANCE_POLLS, 100);
        assert_eq!(CLEARANCE_POLL_INTERVAL, Duration::from_millis(100));
        assert_eq!(*CLICK_OFFSET_X.start(), 26);
        assert_eq!(*CLICK_OFFSET_X.end(), 28);
        assert_eq!(*CLICK_OFFSET_Y.start(), 25);
        assert_eq!(*CLICK_OFFSET_Y.end(), 27);
        assert_eq!(*CLICK_HOLD_MS.start(), 100);
        assert_eq!(*CLICK_HOLD_MS.end(), 200);
    }

    // -- the solver -------------------------------------------------------

    /// The `MagicMock()` page of `test_cloudflare_solver.py`: `page.frame()`
    /// returns `None` and `locator(...).last.bounding_box()` returns
    /// `{"x": 100, "y": 200}`.
    #[derive(Debug)]
    struct FakePage {
        content: Mutex<String>,
        /// What the content flips to on the first click, if anything.
        content_after_click: Option<String>,
        frame: Option<String>,
        frame_visible_after: usize,
        widget: Option<BoundingBox>,
        clicks: Mutex<Vec<(f64, f64, Duration)>>,
        timeouts: Mutex<Vec<Duration>>,
        visibility_polls: AtomicUsize,
        idle_budgets: Mutex<Vec<Option<Duration>>>,
        stability_calls: AtomicUsize,
    }

    impl FakePage {
        fn new(content: &str) -> Self {
            Self {
                content: Mutex::new(content.to_owned()),
                content_after_click: None,
                frame: None,
                frame_visible_after: 0,
                widget: Some(BoundingBox {
                    x: 100.0,
                    y: 200.0,
                    width: 300.0,
                    height: 65.0,
                }),
                clicks: Mutex::new(Vec::new()),
                timeouts: Mutex::new(Vec::new()),
                visibility_polls: AtomicUsize::new(0),
                idle_budgets: Mutex::new(Vec::new()),
                stability_calls: AtomicUsize::new(0),
            }
        }

        fn clearing_on_click(content: &str) -> Self {
            Self {
                content_after_click: Some(CLEAN_PAGE.to_owned()),
                ..Self::new(content)
            }
        }

        fn click_count(&self) -> usize {
            self.clicks.lock().len()
        }
    }

    #[async_trait::async_trait]
    impl CloudflarePage for FakePage {
        async fn content(&self) -> Result<String> {
            Ok(self.content.lock().clone())
        }

        async fn wait_for_timeout(&self, duration: Duration) {
            // Recorded, not slept: the Python test monkey-patches the waits
            // away for the same reason.
            self.timeouts.lock().push(duration);
        }

        async fn wait_for_load_state(&self) -> Result<()> {
            Ok(())
        }

        async fn wait_for_networkidle(&self, budget: Option<Duration>) {
            self.idle_budgets.lock().push(budget);
        }

        async fn wait_for_page_stability(
            &self,
            _load_dom: bool,
            _network_idle: bool,
        ) -> Result<()> {
            self.stability_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn challenge_frame(&self) -> Result<Option<String>> {
            Ok(self.frame.clone())
        }

        async fn frame_wait_for_page_stability(
            &self,
            _frame: &str,
            _load_dom: bool,
            _network_idle: bool,
        ) -> Result<()> {
            Ok(())
        }

        async fn frame_element_is_visible(&self, _frame: &str) -> Result<bool> {
            let seen = self.visibility_polls.fetch_add(1, Ordering::SeqCst);
            Ok(seen >= self.frame_visible_after)
        }

        async fn frame_element_bounding_box(&self, _frame: &str) -> Result<Option<BoundingBox>> {
            Ok(self.widget)
        }

        async fn last_bounding_box(&self, _selector: &str) -> Result<Option<BoundingBox>> {
            Ok(self.widget)
        }

        async fn click(&self, x: f64, y: f64, hold: Duration) -> Result<()> {
            self.clicks.lock().push((x, y, hold));
            if let Some(cleared) = self.content_after_click.as_ref() {
                *self.content.lock() = cleared.clone();
            }
            Ok(())
        }
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    /// `TestSyncSolver::test_solver_clicks_localized_challenge`.
    #[tokio::test]
    async fn test_solver_clicks_localized_challenge() {
        let page = FakePage::clearing_on_click(FRENCH_CHALLENGE);
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");

        assert_eq!(page.click_count(), 1);
        let (x, y, hold) = page.clicks.lock()[0];
        assert!((126.0..=128.0).contains(&x), "{x}");
        assert!((225.0..=227.0).contains(&y), "{y}");
        assert!(hold >= Duration::from_millis(100) && hold <= Duration::from_millis(200));
        assert_eq!(
            page.idle_budgets.lock().first().copied().flatten(),
            Some(INITIAL_IDLE_BUDGET),
            "the first idle wait gets the 5000 ms budget"
        );
    }

    /// `TestSyncSolver::test_solver_stops_after_max_attempts`.
    #[tokio::test]
    async fn test_solver_stops_after_max_attempts() {
        let page = FakePage::new(FRENCH_CHALLENGE);
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        assert_eq!(
            page.click_count(),
            CF_MAX_SOLVE_ATTEMPTS,
            "recursion depth 0, 1 and 2 click; the fourth entry hits the cap"
        );
    }

    #[tokio::test]
    async fn a_clean_page_is_not_a_challenge_and_is_never_clicked() {
        let page = FakePage::new(CLEAN_PAGE);
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        assert_eq!(page.click_count(), 0);
        assert!(page.timeouts.lock().is_empty());
    }

    #[tokio::test]
    async fn the_spinner_wait_polls_twenty_times_at_most() {
        let page = FakePage::new(FRENCH_CHALLENGE);
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        let spinner_polls = page
            .timeouts
            .lock()
            .iter()
            .filter(|duration| **duration == SPINNER_POLL_INTERVAL)
            .count();
        assert_eq!(
            spinner_polls,
            SPINNER_POLLS * CF_MAX_SOLVE_ATTEMPTS,
            "20 polls per attempt, and there are three attempts"
        );
    }

    #[tokio::test]
    async fn the_clearance_wait_polls_a_hundred_times_at_most() {
        let page = FakePage::new(FRENCH_CHALLENGE);
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        let clearance_polls = page
            .timeouts
            .lock()
            .iter()
            .filter(|duration| **duration == CLEARANCE_POLL_INTERVAL)
            .count();
        assert_eq!(clearance_polls, CLEARANCE_POLLS * CF_MAX_SOLVE_ATTEMPTS);
    }

    const EMBEDDED_PAGE: &str = r#"<html><script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script></html>"#;

    #[tokio::test]
    async fn an_embedded_widget_skips_the_spinner_and_the_clearance_waits() {
        let page = FakePage {
            frame: Some("CF".to_owned()),
            ..FakePage::new(EMBEDDED_PAGE)
        };
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        assert_eq!(page.click_count(), 1);
        assert_eq!(
            page.visibility_polls.load(Ordering::SeqCst),
            0,
            "`challenge_type != 'embedded'` guards the visibility loop"
        );
        assert!(
            page.timeouts.lock().is_empty(),
            "neither bounded poll loop runs for an embedded widget"
        );
    }

    #[tokio::test]
    async fn an_embedded_widget_with_no_iframe_returns_without_clicking() {
        // `_challenge_cleared` is unconditionally true for `embedded`, so the
        // `if not iframe or not outer_box:` branch reports success and returns
        // before the click. Faithful to Python, surprising enough to pin.
        let page = FakePage::new(EMBEDDED_PAGE);
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        assert_eq!(page.click_count(), 0);
    }

    #[tokio::test]
    async fn a_non_interactive_wait_page_is_never_clicked() {
        let page = FakePage::new("<html><script>cType: 'non-interactive'</script></html>");
        // The wait page never clears, so the deadline is what stops the loop.
        let deadline = Instant::now() + Duration::from_millis(1);
        tokio::time::sleep(Duration::from_millis(2)).await;
        solve_cloudflare(&page, deadline).await.expect("solve");
        assert_eq!(
            page.click_count(),
            0,
            "a non-interactive challenge is waited out, never clicked"
        );
    }

    #[tokio::test]
    async fn a_non_interactive_page_that_clears_reports_success() {
        struct ClearingPage {
            inner: FakePage,
            reads: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl CloudflarePage for ClearingPage {
            async fn content(&self) -> Result<String> {
                // First read shows the wait page, every later read is clean.
                if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok("<html><script>cType: 'non-interactive'</script></html>".to_owned());
                }
                Ok(CLEAN_PAGE.to_owned())
            }
            async fn wait_for_timeout(&self, duration: Duration) {
                self.inner.wait_for_timeout(duration).await;
            }
            async fn wait_for_load_state(&self) -> Result<()> {
                Ok(())
            }
            async fn wait_for_networkidle(&self, budget: Option<Duration>) {
                self.inner.wait_for_networkidle(budget).await;
            }
            async fn wait_for_page_stability(&self, load: bool, idle: bool) -> Result<()> {
                self.inner.wait_for_page_stability(load, idle).await
            }
            async fn challenge_frame(&self) -> Result<Option<String>> {
                Ok(None)
            }
            async fn frame_wait_for_page_stability(
                &self,
                _frame: &str,
                _load_dom: bool,
                _network_idle: bool,
            ) -> Result<()> {
                Ok(())
            }
            async fn frame_element_is_visible(&self, _frame: &str) -> Result<bool> {
                Ok(true)
            }
            async fn frame_element_bounding_box(
                &self,
                _frame: &str,
            ) -> Result<Option<BoundingBox>> {
                Ok(None)
            }
            async fn last_bounding_box(&self, _selector: &str) -> Result<Option<BoundingBox>> {
                Ok(None)
            }
            async fn click(&self, x: f64, y: f64, hold: Duration) -> Result<()> {
                self.inner.click(x, y, hold).await
            }
        }

        let page = ClearingPage {
            inner: FakePage::new(CLEAN_PAGE),
            reads: AtomicUsize::new(0),
        };
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        assert_eq!(page.inner.click_count(), 0);
    }

    #[tokio::test]
    async fn the_visibility_loop_is_bounded_by_the_deadline() {
        let page = Arc::new(FakePage {
            frame: Some("CF".to_owned()),
            // Never visible: Python would poll here forever.
            frame_visible_after: usize::MAX,
            widget: None,
            ..FakePage::new(FRENCH_CHALLENGE)
        });
        let deadline = Instant::now() + Duration::from_millis(30);
        // Must return rather than hang; the deviation is the whole point.
        tokio::time::timeout(Duration::from_secs(10), solve_cloudflare(&*page, deadline))
            .await
            .expect("the solver must not hang")
            .expect("solve");
        assert!(page.visibility_polls.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn a_widget_with_no_measurable_box_retries_after_a_second() {
        let page = FakePage {
            widget: None,
            ..FakePage::new(FRENCH_CHALLENGE)
        };
        solve_cloudflare(&page, far_deadline())
            .await
            .expect("solve");
        assert_eq!(page.click_count(), 0, "nothing to click");
        let slow_polls = page
            .timeouts
            .lock()
            .iter()
            .filter(|duration| **duration == SLOW_POLL_INTERVAL)
            .count();
        assert_eq!(
            slow_polls, CF_MAX_SOLVE_ATTEMPTS,
            "one 1000 ms pause before each retry"
        );
    }
}
