//! Script evaluation in a `Page.createIsolatedWorld` context.
//!
//! # Why this module exists at all
//!
//! the reference implementation's stealth engine imports Playwright from **patchright**, a patched
//! driver whose entire value is three CDP-level behaviours (`ARCHITECTURE.md`
//! §8, "Patchright as a dependency"):
//!
//! 1. it never calls `Runtime.enable`, because that makes Chrome fire
//!    `Runtime.executionContextCreated` / `consoleAPICalled` and leaks a
//!    trivially detectable "a debugger is attached" signal into the page;
//! 2. it evaluates injected JavaScript in an **isolated world** instead of the
//!    page's main world, so nothing the page can reach ever sees the helper
//!    functions;
//! 3. it does not install Playwright's binding globals (`__playwright*`,
//!    `Runtime.addBinding`), which detectors enumerate directly.
//!
//! Driving raw CDP we do not need the fork — we simply never emit those
//! commands. This module is the "instead": [`IsolatedWorlds::context_for`]
//! creates a world per frame, and [`evaluate`] runs everything inside it.
//!
//! **Do not** add `Runtime.enable`, `Runtime.addBinding` or
//! `Page.addScriptToEvaluateOnNewDocument` *without* a `worldName* to this
//! crate. `Runtime.evaluate` and `Runtime.callFunctionOn` work perfectly well
//! with the domain disabled as long as an `executionContextId` is supplied,
//! which is exactly what a created isolated world hands back.
//!
//! One caveat is upstream and outside this crate's control: `chromiumoxide`'s
//! own `FrameManager::init_commands` issues `Runtime.enable` when it attaches
//! to a target, and its `Page::content()` / `Page::evaluate()` run in the main
//! world. Nothing here calls those; [`crate::cdp::CdpPage`] re-implements the
//! handful it needs on top of this module. See the crate-level docs.

use std::collections::HashMap;

use chromiumoxide::cdp::browser_protocol::page::{CreateIsolatedWorldParams, FrameId};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, ExecutionContextId,
};
use chromiumoxide::Page;
use parking_lot::Mutex;
use serde_json::Value;

use crate::error::{BrowserError, Result};

/// The name the isolated world is reported under in DevTools.
///
/// Playwright uses `__playwright_utility_world__`, which is itself a
/// fingerprint (`Runtime.executionContextCreated` carries the name, and
/// extensions can see it). A neutral name is used instead.
pub const ISOLATED_WORLD_NAME: &str = "__browser-rs_utility_world__";

/// Playwright's `Page.content()`, verbatim.
///
/// `document.documentElement.outerHTML` alone loses the doctype, which changes
/// how the parser treats the document; Playwright serialises the doctype
/// separately and prepends it. Reproduced exactly so a browser response and a
/// `curl` response of the same page parse the same way.
pub const PAGE_CONTENT_JS: &str = r"(() => {
  let retVal = '';
  if (document.doctype) retVal = new XMLSerializer().serializeToString(document.doctype);
  if (document.documentElement) retVal += document.documentElement.outerHTML;
  return retVal;
})()";

/// `element.bounding_box()`: `null` when the element is not rendered.
///
/// Playwright returns `null` for a zero-area box, which is what makes
/// `if not outer_box:` in the Cloudflare solver fire.
pub const BOUNDING_BOX_JS: &str = r"function () {
  const rect = this.getBoundingClientRect();
  if (!rect || (!rect.width && !rect.height)) return null;
  return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
}";

/// `element.is_visible()` in Playwright's sense.
///
/// "Visible" is *has a non-empty bounding box* **and** *`visibility` is not
/// `hidden`* — deliberately not `checkVisibility()`, which also fails for
/// `content-visibility: auto` off-screen content that Playwright still calls
/// visible.
pub const IS_VISIBLE_JS: &str = r"function () {
  const style = getComputedStyle(this);
  if (!style || style.visibility === 'hidden') return false;
  const rect = this.getBoundingClientRect();
  return !!(rect.width || rect.height);
}";

/// The state of the first element matching a selector, for `wait_selector`.
///
/// Returns `"missing"`, `"hidden"` or `"visible"` so one round trip answers all
/// four [`browser_core::types::WaitState`] variants.
pub const SELECTOR_STATE_JS: &str = r"(selector) => {
  const element = document.querySelector(selector);
  if (!element) return 'missing';
  const style = getComputedStyle(element);
  if (!style || style.visibility === 'hidden') return 'hidden';
  const rect = element.getBoundingClientRect();
  return (rect.width || rect.height) ? 'visible' : 'hidden';
}";

/// `page.locator(selector).last.bounding_box()`.
///
/// The Cloudflare solver reads the **last** match, not the first: a challenge
/// page can carry several `div`s under `.main-content p+div>div>div`, and the
/// widget is the last one.
pub const LAST_BOUNDING_BOX_JS: &str = r"(selector) => {
  const all = document.querySelectorAll(selector);
  const element = all[all.length - 1];
  if (!element) return null;
  const rect = element.getBoundingClientRect();
  if (!rect || (!rect.width && !rect.height)) return null;
  return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
}";

/// A rendered element's position and size, in CSS pixels of the main frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoundingBox {
    /// Distance from the left edge of the viewport.
    pub x: f64,
    /// Distance from the top edge of the viewport.
    pub y: f64,
    /// Rendered width.
    pub width: f64,
    /// Rendered height.
    pub height: f64,
}

impl BoundingBox {
    /// Read one out of the JSON [`BOUNDING_BOX_JS`] returns.
    ///
    /// ```
    /// use browser_cdp::cdp::isolated_world::BoundingBox;
    ///
    /// let box_ = BoundingBox::from_json(&serde_json::json!({
    ///     "x": 100.0, "y": 200.0, "width": 300.0, "height": 65.0
    /// }));
    /// assert_eq!(box_.expect("a box").x, 100.0);
    /// assert!(BoundingBox::from_json(&serde_json::Value::Null).is_none());
    /// ```
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let number = |key: &str| object.get(key).and_then(Value::as_f64);
        Some(Self {
            x: number("x")?,
            y: number("y")?,
            width: number("width")?,
            height: number("height")?,
        })
    }
}

/// One isolated world per frame, created on demand and re-created when the
/// frame navigates.
///
/// An execution context dies with its document, and the next `Runtime.evaluate`
/// against it fails with `Cannot find context with specified id`. Rather than
/// track `Runtime.executionContextDestroyed` — which would need
/// `Runtime.enable`, the one command this crate must not send — the cache is
/// invalidated lazily: a failed evaluation drops the entry and retries once.
#[derive(Debug, Default)]
pub struct IsolatedWorlds {
    contexts: Mutex<HashMap<FrameId, ExecutionContextId>>,
}

/// The CDP error text Chrome uses for an execution context that has gone.
const STALE_CONTEXT_MARKERS: [&str; 3] = [
    "Cannot find context with specified id",
    "Execution context was destroyed",
    "Inspected target navigated or closed",
];

/// Whether a CDP error means "that world is gone, make a new one".
#[must_use]
pub fn is_stale_context_error(message: &str) -> bool {
    STALE_CONTEXT_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

impl IsolatedWorlds {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The isolated world for `frame`, creating it if necessary.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Page.createIsolatedWorld` fails.
    pub async fn context_for(&self, page: &Page, frame: &FrameId) -> Result<ExecutionContextId> {
        if let Some(context) = self.contexts.lock().get(frame) {
            return Ok(*context);
        }
        let params = CreateIsolatedWorldParams {
            frame_id: frame.clone(),
            world_name: Some(ISOLATED_WORLD_NAME.to_owned()),
            // Playwright grants universal access so its helpers can reach
            // cross-origin frames; the Cloudflare solver needs exactly that.
            grant_univeral_access: Some(true),
        };
        let created = page
            .execute(params)
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.createIsolatedWorld".to_owned(),
                message: error.to_string(),
            })?;
        let context = created.result.execution_context_id;
        self.contexts.lock().insert(frame.clone(), context);
        Ok(context)
    }

    /// Forget the world for one frame, so the next call makes a new one.
    pub fn invalidate(&self, frame: &FrameId) {
        self.contexts.lock().remove(frame);
    }

    /// Forget every world.
    pub fn clear(&self) {
        self.contexts.lock().clear();
    }

    /// How many worlds are cached, for tests and logging.
    #[must_use]
    pub fn len(&self) -> usize {
        self.contexts.lock().len()
    }

    /// Whether no world is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.contexts.lock().is_empty()
    }

    /// Evaluate `expression` in `frame`'s isolated world, retrying once through
    /// a fresh world if the cached one had died.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] for a transport failure, or
    /// [`BrowserError::JavaScript`] when the expression threw.
    pub async fn evaluate(&self, page: &Page, frame: &FrameId, expression: &str) -> Result<Value> {
        let context = self.context_for(page, frame).await?;
        match evaluate(page, &context, expression).await {
            Err(BrowserError::Cdp {
                context: ctx,
                message,
            }) if is_stale_context_error(&message) => {
                tracing::debug!("isolated world for {frame:?} was destroyed ({ctx}); recreating");
                self.invalidate(frame);
                let context = self.context_for(page, frame).await?;
                evaluate(page, &context, expression).await
            }
            other => other,
        }
    }
}

/// `Runtime.evaluate` in an explicit execution context, by value.
///
/// `await_promise` is on so an `async` snippet resolves before returning, and
/// `return_by_value` is on so the result arrives as JSON rather than as a
/// remote handle that would need releasing.
///
/// # Errors
///
/// [`BrowserError::Cdp`] for a transport failure, [`BrowserError::JavaScript`]
/// when the expression threw.
pub async fn evaluate(
    page: &Page,
    context: &ExecutionContextId,
    expression: &str,
) -> Result<Value> {
    let mut params = EvaluateParams::new(expression);
    params.context_id = Some(*context);
    params.return_by_value = Some(true);
    params.await_promise = Some(true);
    // Never expose `$`, `$$`, `copy`, … — the command-line API is a
    // DevTools-only global and its presence is detectable.
    params.include_command_line_api = Some(false);
    params.user_gesture = Some(false);
    let returns = page
        .execute(params)
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Runtime.evaluate".to_owned(),
            message: error.to_string(),
        })?;
    if let Some(details) = returns.result.exception_details.as_ref() {
        return Err(BrowserError::JavaScript {
            message: details.text.clone(),
        });
    }
    Ok(returns.result.result.value.clone().unwrap_or(Value::Null))
}

/// `Runtime.callFunctionOn` with `this` bound to a remote object.
///
/// Used for the element-level helpers ([`BOUNDING_BOX_JS`], [`IS_VISIBLE_JS`]),
/// whose `this` is a node resolved into the isolated world by
/// `DOM.resolveNode`.
///
/// # Errors
///
/// [`BrowserError::Cdp`] for a transport failure, [`BrowserError::JavaScript`]
/// when the function threw.
pub async fn call_on(page: &Page, object_id: &str, function_declaration: &str) -> Result<Value> {
    let mut params = CallFunctionOnParams::new(function_declaration);
    params.object_id = Some(object_id.to_owned().into());
    params.return_by_value = Some(true);
    params.await_promise = Some(true);
    params.user_gesture = Some(false);
    let returns = page
        .execute(params)
        .await
        .map_err(|error| BrowserError::Cdp {
            context: "Runtime.callFunctionOn".to_owned(),
            message: error.to_string(),
        })?;
    if let Some(details) = returns.result.exception_details.as_ref() {
        return Err(BrowserError::JavaScript {
            message: details.text.clone(),
        });
    }
    Ok(returns.result.result.value.clone().unwrap_or(Value::Null))
}

/// A JavaScript string literal, safe to paste into an expression.
///
/// `serde_json::to_string` is a valid JS string literal for every input
/// (JSON string escaping is a subset of JS string escaping), which avoids
/// hand-rolled quoting bugs on selectors containing quotes or backslashes.
///
/// ```
/// use browser_cdp::cdp::isolated_world::js_string;
///
/// assert_eq!(js_string("a[b='c']"), r#""a[b='c']""#);
/// assert_eq!(js_string("back\\slash"), r#""back\\slash""#);
/// ```
#[must_use]
pub fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The one-shot expression that answers "what state is this selector in?".
///
/// ```
/// use browser_cdp::cdp::isolated_world::selector_state_expression;
///
/// assert!(selector_state_expression("div.price").contains("\"div.price\""));
/// ```
#[must_use]
pub fn selector_state_expression(selector: &str) -> String {
    format!("({SELECTOR_STATE_JS})({})", js_string(selector))
}

/// The one-shot expression behind `locator(selector).last.bounding_box()`.
///
/// ```
/// use browser_cdp::cdp::isolated_world::last_bounding_box_expression;
///
/// assert!(last_bounding_box_expression(".turnstile>div>div").contains("querySelectorAll"));
/// ```
#[must_use]
pub fn last_bounding_box_expression(selector: &str) -> String {
    format!("({LAST_BOUNDING_BOX_JS})({})", js_string(selector))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_name_is_not_playwrights() {
        assert!(
            !ISOLATED_WORLD_NAME.contains("playwright"),
            "the world name reaches Runtime.executionContextCreated and is a fingerprint"
        );
    }

    #[test]
    fn page_content_serialises_the_doctype_too() {
        assert!(PAGE_CONTENT_JS.contains("XMLSerializer"));
        assert!(PAGE_CONTENT_JS.contains("documentElement.outerHTML"));
    }

    #[test]
    fn bounding_box_reads_the_four_numbers() {
        let parsed = BoundingBox::from_json(&serde_json::json!({
            "x": 1.5, "y": 2.5, "width": 3.0, "height": 4.0
        }))
        .expect("a box");
        assert_eq!(parsed.x, 1.5);
        assert_eq!(parsed.y, 2.5);
        assert_eq!(parsed.width, 3.0);
        assert_eq!(parsed.height, 4.0);
    }

    #[test]
    fn a_partial_box_is_no_box() {
        assert!(BoundingBox::from_json(&serde_json::json!({ "x": 1.0, "y": 2.0 })).is_none());
        assert!(BoundingBox::from_json(&serde_json::json!("nonsense")).is_none());
    }

    #[test]
    fn stale_context_errors_are_recognised() {
        assert!(is_stale_context_error(
            "Cannot find context with specified id"
        ));
        assert!(is_stale_context_error("Execution context was destroyed."));
        assert!(!is_stale_context_error("Some other CDP failure"));
    }

    #[test]
    fn selectors_are_escaped_into_the_expression() {
        let expression = selector_state_expression(r#"a[href="/x"]"#);
        assert!(expression.contains(r#""a[href=\"/x\"]""#));
        assert!(expression.starts_with('('));
    }

    #[test]
    fn the_world_cache_is_per_frame() {
        let worlds = IsolatedWorlds::new();
        assert!(worlds.is_empty());
        worlds
            .contexts
            .lock()
            .insert(FrameId::new("A"), ExecutionContextId::new(1));
        worlds
            .contexts
            .lock()
            .insert(FrameId::new("B"), ExecutionContextId::new(2));
        assert_eq!(worlds.len(), 2);
        worlds.invalidate(&FrameId::new("A"));
        assert_eq!(worlds.len(), 1);
        worlds.clear();
        assert!(worlds.is_empty());
    }
}
