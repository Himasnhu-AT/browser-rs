//! The extraction subcommands, mapping CLI arguments onto a fetch plus a conversion.
//!
//! This is `cli.py`'s `__ParseJSONData` / `__ParseExtractArguments` /
//! `__BuildRequest` / `__build_browser_kwargs` / `__Request_and_Save`
//! (`cli-ai.md` §2.6 and §2.7), and it is where the **seconds-versus-milliseconds
//! split** is finally resolved: [`build_http_plan`] produces a
//! [`std::time::Duration`] from `--timeout` **seconds**, and
//! [`build_browser_config`] produces `timeout_ms` from `--timeout`
//! **milliseconds**. Those are the only two places either unit is read.
//!
//! # Three quirks worth knowing before reading the code
//!
//! * **A parameter without `=` is silently dropped.** `-p broken` contributes
//!   nothing; Python's loop is `if "=" in param`, with no else.
//! * **Not passing `--impersonate` disables impersonation.** `__BuildRequest`
//!   always puts the key in the kwargs — as `None` — and `_get_param` keys on
//!   *presence*, so the session's `"chrome"` default never applies to a CLI
//!   request. `extract get` therefore runs with a plain header set unless you
//!   ask for a profile.
//! * **`--ai-targeted` does not switch ad-blocking on, despite appearances.**
//!   `__Request_and_Save` does `kwargs.setdefault("block_ads", True)`, but the
//!   browser commands have *already* put `block_ads` in the dict, so the
//!   `setdefault` is a no-op there; and the HTTP session lists `block_ads` in its
//!   skip set, so it is discarded there. The line has no observable effect in
//!   either direction, and this port reproduces that by doing nothing.

use std::fmt;
use std::time::Duration;

use browser::{
    AsyncFetcher, Body, Cookies, FollowRedirects, HeaderDict, Impersonate, Proxy, Query,
    RequestBuilder, Response,
};

use crate::args::{DataOptions, HttpOptions, Targets};
use crate::convert::{resolve_output_path, write_content_to_file};

/// What can go wrong before a request is even made.
#[derive(Debug)]
pub enum ExtractError {
    /// `ValueError(f"Invalid JSON data '{json_string}': {err}")`.
    InvalidJson {
        /// The string as given.
        json: String,
        /// `serde_json`'s complaint, standing in for `orjson`'s.
        message: String,
    },
    /// `_ParseHeaders` rejected a header line.
    Header(browser::HeaderParseError),
    /// `ValueError(f"Could not parse cookies '{cookies}': {err}")`.
    Cookies {
        /// The cookie string as given.
        cookies: String,
        /// What the parser said.
        message: String,
    },
    /// `BrowserConfig::validate` rejected the assembled configuration — a
    /// negative timeout, a missing executable, an unusable proxy string.
    #[cfg(feature = "browser")]
    Config(browser::browser::error::ConfigError),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson { json, message } => {
                write!(f, "Invalid JSON data '{json}': {message}")
            }
            Self::Header(error) => write!(f, "{error}"),
            Self::Cookies { cookies, message } => {
                write!(f, "Could not parse cookies '{cookies}': {message}")
            }
            #[cfg(feature = "browser")]
            Self::Config(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ExtractError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Header(error) => Some(error),
            #[cfg(feature = "browser")]
            Self::Config(error) => Some(error),
            Self::InvalidJson { .. } | Self::Cookies { .. } => None,
        }
    }
}

/// `__BuildRequest`'s result: everything a fetcher call needs, resolved.
///
/// Python hands `Fetcher.get` a `dict`; this is the same dict with its keys
/// given types. Where Python writes `None` to mean "explicitly cleared", the
/// corresponding field here is an empty collection or a `None`, and
/// [`apply_http_plan`] calls the setter **either way** — because in the reference implementation an
/// explicit `None` overrides the session default, and the CLI's dict always
/// carries these keys.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpPlan {
    /// `-H` headers, minus any `Cookie:` line.
    pub headers: HeaderDict,
    /// `-H "Cookie: …"` merged with, and then overwritten by, `--cookies`.
    pub cookies: Cookies,
    /// `-p key=value` pairs, in order.
    pub params: Query,
    /// `--json`, parsed.
    pub json: Option<serde_json::Value>,
    /// `--data`, verbatim.
    pub data: Option<String>,
    /// `--timeout`, **seconds**.
    pub timeout: Duration,
    /// `--follow-redirects` / `--no-follow-redirects`.
    pub follow_redirects: FollowRedirects,
    /// `--verify` / `--no-verify`.
    pub verify: bool,
    /// `--stealthy-headers`, which the fetcher methods pop as `stealth=`.
    pub stealthy_headers: bool,
    /// `--impersonate`, split on commas into a pool when it has any.
    pub impersonate: Impersonate,
    /// `--proxy`.
    pub proxy: Option<Proxy>,
}

/// `__ParseJSONData`.
///
/// An empty string is falsy in Python, so `--json ''` is "no JSON" rather than a
/// parse error.
///
/// # Errors
///
/// [`ExtractError::InvalidJson`] when the string is not valid JSON.
///
/// ```
/// use browser_cli::commands::extract::parse_json_data;
///
/// assert_eq!(parse_json_data(None).unwrap(), None);
/// assert_eq!(parse_json_data(Some("")).unwrap(), None);
/// assert_eq!(parse_json_data(Some("{\"a\": 1}")).unwrap(), Some(serde_json::json!({"a": 1})));
/// assert!(parse_json_data(Some("{oops}")).is_err());
/// ```
pub fn parse_json_data(json: Option<&str>) -> Result<Option<serde_json::Value>, ExtractError> {
    let Some(json) = json.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    serde_json::from_str(json)
        .map(Some)
        .map_err(|error| ExtractError::InvalidJson {
            json: json.to_owned(),
            message: error.to_string(),
        })
}

/// The `-p key=value` loop. A parameter without an `=` is dropped, silently.
///
/// ```
/// use browser_cli::commands::extract::parse_params;
///
/// let params = parse_params(&["q=widget".to_owned(), "broken".to_owned(), "r=a=b".to_owned()]);
/// assert_eq!(params, [("q".to_owned(), "widget".to_owned()), ("r".to_owned(), "a=b".to_owned())]);
/// ```
#[must_use]
pub fn parse_params(params: &[String]) -> Query {
    // Python builds a `dict`, so a repeated key keeps its first position and its
    // last value; an `IndexMap` reproduces that before flattening to pairs.
    let mut seen: browser::CookieMap = browser::CookieMap::new();
    for param in params {
        if let Some((key, value)) = param.split_once('=') {
            seen.insert(key.to_owned(), value.to_owned());
        }
    }
    seen.into_iter().collect()
}

/// `--impersonate`: a comma turns the value into a pool sampled per request.
///
/// The **absent** case is the interesting one — it is `Impersonate::None`, not
/// the `"chrome"` default, for the reason in the module docs.
///
/// ```
/// use browser::Impersonate;
/// use browser_cli::commands::extract::parse_impersonate;
///
/// assert_eq!(parse_impersonate(None), Impersonate::None);
/// assert_eq!(parse_impersonate(Some("chrome")), Impersonate::single("chrome"));
/// assert_eq!(
///     parse_impersonate(Some("chrome, firefox ,safari")),
///     Impersonate::pool(["chrome", "firefox", "safari"]),
/// );
/// ```
#[must_use]
pub fn parse_impersonate(impersonate: Option<&str>) -> Impersonate {
    match impersonate {
        None => Impersonate::None,
        Some(value) if value.contains(',') => {
            Impersonate::pool(value.split(',').map(str::trim).map(str::to_owned))
        }
        Some(value) => Impersonate::single(value),
    }
}

/// `__ParseExtractArguments` + `__BuildRequest`.
///
/// # Errors
///
/// [`ExtractError::Header`] for a header line with no colon, and
/// [`ExtractError::InvalidJson`] / [`ExtractError::Cookies`] for the other two.
pub fn build_http_plan(
    options: &HttpOptions,
    body: Option<&DataOptions>,
) -> Result<HttpPlan, ExtractError> {
    let (headers, mut cookies) =
        browser::parse_header_lines(&options.headers, true).map_err(ExtractError::Header)?;

    // `--cookies` merges *over* the ones a `Cookie:` header produced.
    if let Some(raw) = options.cookies.as_deref().filter(|value| !value.is_empty()) {
        let parsed =
            browser::parse_cookie_string(raw).map_err(|error| ExtractError::Cookies {
                cookies: raw.to_owned(),
                message: error.to_string(),
            })?;
        for (key, value) in parsed {
            cookies.insert(key, value);
        }
    }

    Ok(HttpPlan {
        headers,
        cookies,
        params: parse_params(&options.params),
        json: parse_json_data(body.and_then(|body| body.json.as_deref()))?,
        // `data` is forwarded verbatim, and dropped downstream when it is `None`.
        data: body.and_then(|body| body.data.clone()),
        timeout: Duration::from_secs(options.timeout_secs),
        follow_redirects: FollowRedirects::Bool(options.follow_redirects()),
        verify: options.verify(),
        stealthy_headers: options.stealthy_headers(),
        impersonate: parse_impersonate(options.impersonate.as_deref()),
        // Not filtered on emptiness: `__BuildRequest` forwards `--proxy ''`
        // verbatim, and `proxy_is_truthy` is what decides it means "no proxy".
        // Corpus row `get-proxy-empty` pins the difference.
        proxy: options
            .proxy
            .as_deref()
            .map(|value| Proxy::Url(value.to_owned())),
    })
}

/// Turn a [`HttpPlan`] into a configured [`RequestBuilder`].
///
/// Every setter is called unconditionally where Python always puts the key in
/// the kwargs dict, so an explicit "off" really overrides a default. The three
/// that Python *omits* when falsy — `json`, `params` and a `None` `data` — are
/// omitted here too.
#[must_use]
pub fn apply_http_plan(mut builder: RequestBuilder, plan: &HttpPlan) -> RequestBuilder {
    if !plan.headers.is_empty() {
        builder = builder.headers(plan.headers.clone());
    }
    if !plan.cookies.is_empty() {
        builder = builder.cookies(plan.cookies.clone());
    }
    if !plan.params.is_empty() {
        builder = builder.params(plan.params.clone());
    }
    // `json` wins over `data` when both are given. Python forwards both and lets
    // `curl_cffi` decide, and `curl_cffi/requests/utils.py` builds the body from
    // `data` and then does `if json is not None: body = dumps(json)` — an
    // unconditional overwrite. `Body` is one value here, so the precedence has to
    // be spelled out; corpus row `post-json-and-data` is where it is pinned.
    if let Some(json) = &plan.json {
        builder = builder.json(json.clone());
    } else if let Some(data) = &plan.data {
        builder = builder.body(Body::Text(data.clone()));
    }
    builder
        .timeout(Some(plan.timeout))
        .follow_redirects(plan.follow_redirects)
        .verify(plan.verify)
        .stealthy_headers(plan.stealthy_headers)
        .impersonate(plan.impersonate.clone())
        .proxy(plan.proxy.clone())
}

/// Which HTTP verb an `extract` subcommand runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVerb {
    /// `extract get`.
    Get,
    /// `extract post`.
    Post,
    /// `extract put`.
    Put,
    /// `extract delete`.
    Delete,
}

impl HttpVerb {
    /// Start the matching builder on the shared client.
    fn builder(self, url: String) -> RequestBuilder {
        match self {
            Self::Get => AsyncFetcher::get(url),
            Self::Post => AsyncFetcher::post(url),
            Self::Put => AsyncFetcher::put(url),
            Self::Delete => AsyncFetcher::delete(url),
        }
    }
}

/// `__http_command` + `__Request_and_Save`: fetch, then write.
///
/// # Errors
///
/// Argument-parsing failures, transport failures, and write failures.
pub async fn run_http(
    verb: HttpVerb,
    targets: &Targets,
    options: &HttpOptions,
    body: Option<&DataOptions>,
) -> anyhow::Result<()> {
    let output_path = resolve_output_path(&targets.output_file)?;
    // Reject an impossible output name before spending a request on it. Python
    // discovers it only after the fetch; failing early cannot change what a
    // successful run writes.
    let _ = crate::convert::extraction_for(&output_path.to_string_lossy())?;

    let plan = build_http_plan(options, body)?;
    let builder = apply_http_plan(verb.builder(targets.url.clone()), &plan);
    let response: Response = builder.send().await?;
    tracing::info!("<{} {}>", response.status, response.url);

    write_content_to_file(
        &response,
        &output_path,
        options.css_selector.as_deref(),
        options.ai_targeted,
    )?;
    tracing::info!("Content successfully saved to '{}'", output_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// The browser commands
// ---------------------------------------------------------------------------

/// `__build_browser_kwargs`, plus `stealthy-fetch`'s four extra knobs.
///
/// `--timeout` here is **milliseconds**, and it goes straight into
/// [`BrowserConfig::timeout_ms`](browser::BrowserConfig::timeout_ms).
///
/// The `if wait > 0` / `if wait_selector` / `if proxy` / `if parsed_headers`
/// guards are Python's; each of those keys is *absent* rather than `None` when
/// its value is falsy, and the browser config's own defaults then apply. Since
/// those defaults are the same values, the guards are reproduced for
/// faithfulness rather than for effect — except for `extra_headers`, where
/// `Some(empty map)` and `None` really are different to the browser engine.
///
/// # Errors
///
/// Whatever `BrowserConfig::validate` rejects — a `timeout` below zero, a
/// missing executable, an unusable proxy string.
#[cfg(feature = "browser")]
pub fn build_browser_config(
    options: &crate::args::BrowserOptions,
    stealth: Option<&crate::args::StealthyCommand>,
) -> Result<browser::BrowserConfig, ExtractError> {
    // `__post_init__` — the range checks, the proxy normalisation, the file
    // checks, the `block_ads` union and the `solve_cloudflare` timeout floor.
    assemble_browser_config(options, stealth)?
        .validate()
        .map(browser::ValidatedConfig::into_inner)
        .map_err(ExtractError::Config)
}

/// The **unvalidated** half of [`build_browser_config`]: exactly the kwargs dict
/// `__build_browser_kwargs` produces, as a `BrowserConfig`.
///
/// Split out because that dict is what the differential corpus records — Python
/// builds it *before* `PlaywrightConfig.__post_init__` runs, so comparing
/// against a validated config would be comparing against a later state.
///
/// # Errors
///
/// [`ExtractError::Header`] for a malformed `--extra-headers` line.
#[cfg(feature = "browser")]
pub fn assemble_browser_config(
    options: &crate::args::BrowserOptions,
    stealth: Option<&crate::args::StealthyCommand>,
) -> Result<browser::BrowserConfig, ExtractError> {
    use browser::{BrowserConfig, StealthExtras};

    let mut config = if stealth.is_some() {
        BrowserConfig::stealthy()
    } else {
        BrowserConfig::new()
    };

    config = config
        .headless(options.headless())
        .disable_resources(options.disable_resources())
        .network_idle(options.network_idle())
        .timeout_ms(
            // The one conversion in the whole crate that must NOT happen: the
            // browser group's `--timeout` is already milliseconds.
            #[allow(
                clippy::cast_precision_loss,
                reason = "a timeout in ms is far below 2^53"
            )]
            {
                options.timeout_ms as f64
            },
        )
        .real_chrome(options.real_chrome())
        .dns_over_https(options.dns_over_https())
        .block_ads(options.block_ads());

    if let Some(locale) = options.locale.as_deref() {
        config = config.locale(locale);
    }
    if options.wait_ms > 0 {
        #[allow(clippy::cast_precision_loss, reason = "a wait in ms is far below 2^53")]
        {
            config.wait_ms = options.wait_ms as f64;
        }
    }
    if let Some(selector) = options.wait_selector.as_deref().filter(|s| !s.is_empty()) {
        config = config.wait_selector(selector);
    }
    if let Some(proxy) = options.proxy.as_deref().filter(|s| !s.is_empty()) {
        config = config.proxy(Proxy::Url(proxy.to_owned()));
    }

    // `_ParseHeaders(extra_headers, False)` — cookies are *not* diverted here,
    // so a `Cookie:` line stays an ordinary header for the browser.
    let (parsed_headers, _) = browser::parse_header_lines(&options.extra_headers, false)
        .map_err(ExtractError::Header)?;
    if !parsed_headers.is_empty() {
        config = config.extra_headers(parsed_headers);
    }

    if let Some(path) = options.resolve_executable_path() {
        config = config.executable_path(path);
    }

    if let Some(stealth) = stealth {
        config.stealth = Some(
            StealthExtras::new()
                .allow_webgl(stealth.allow_webgl())
                .hide_canvas(stealth.hide_canvas())
                .block_webrtc(stealth.block_webrtc())
                .solve_cloudflare(stealth.solve_cloudflare()),
        );
    }

    // `validate()` runs `__post_init__`, including the `solve_cloudflare`
    // 60-second timeout floor.
    Ok(config)
}

/// `extract fetch` / `extract stealthy-fetch`.
///
/// # Errors
///
/// Configuration failures, browser failures, and write failures.
#[cfg(feature = "browser")]
pub async fn run_browser(
    targets: &Targets,
    options: &crate::args::BrowserOptions,
    stealth: Option<&crate::args::StealthyCommand>,
) -> anyhow::Result<()> {
    let output_path = resolve_output_path(&targets.output_file)?;
    let _ = crate::convert::extraction_for(&output_path.to_string_lossy())?;

    let config = build_browser_config(options, stealth)?;
    let response =
        browser::fetch_once(config, &targets.url, browser::FetchParams::new()).await?;
    tracing::info!("<{} {}>", response.status, response.url);

    write_content_to_file(
        &response,
        &output_path,
        options.css_selector.as_deref(),
        options.ai_targeted,
    )?;
    tracing::info!("Content successfully saved to '{}'", output_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::args::{Cli, Command, ExtractCommand};

    fn http_options(argv: &[&str]) -> HttpOptions {
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Extract {
                command: ExtractCommand::Get(command),
            } => command.options,
            other => panic!("{other:?}"),
        }
    }

    fn plan(argv: &[&str]) -> HttpPlan {
        build_http_plan(&http_options(argv), None).expect("builds")
    }

    // -- `__ParseJSONData` -------------------------------------------------

    #[test]
    fn json_parsing_treats_an_empty_string_as_absent() {
        assert_eq!(parse_json_data(None).expect("ok"), None);
        assert_eq!(parse_json_data(Some("")).expect("ok"), None);
    }

    #[test]
    fn invalid_json_reports_pythons_message_shape() {
        let error = parse_json_data(Some("{oops}")).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Invalid JSON data '{oops}': "),
            "{error}"
        );
    }

    #[test]
    fn json_scalars_and_arrays_are_accepted_like_orjson() {
        assert_eq!(
            parse_json_data(Some("[1, 2]")).expect("ok"),
            Some(serde_json::json!([1, 2]))
        );
        assert_eq!(
            parse_json_data(Some("123")).expect("ok"),
            Some(serde_json::json!(123))
        );
        // `orjson` rejects NaN/Infinity, and so does `serde_json`.
        assert!(parse_json_data(Some("NaN")).is_err());
    }

    // -- `-p` --------------------------------------------------------------

    #[test]
    fn a_parameter_without_an_equals_sign_is_dropped_silently() {
        assert_eq!(parse_params(&["broken".to_owned()]), Vec::new());
        assert_eq!(
            parse_params(&["a=1".to_owned(), "broken".to_owned(), "b=2".to_owned()]),
            [
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn a_parameter_splits_on_the_first_equals_only() {
        assert_eq!(
            parse_params(&["q=a=b".to_owned()]),
            [("q".to_owned(), "a=b".to_owned())]
        );
        assert_eq!(
            parse_params(&["q=".to_owned()]),
            [("q".to_owned(), String::new())]
        );
    }

    #[test]
    fn a_repeated_parameter_keeps_its_position_and_takes_the_last_value() {
        assert_eq!(
            parse_params(&["a=1".to_owned(), "b=2".to_owned(), "a=3".to_owned()]),
            [
                ("a".to_owned(), "3".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
    }

    // -- `--impersonate` ---------------------------------------------------

    #[test]
    fn a_single_browser_stays_a_single_browser() {
        assert_eq!(
            parse_impersonate(Some("chrome")),
            Impersonate::single("chrome")
        );
    }

    #[test]
    fn a_comma_list_becomes_a_trimmed_pool() {
        assert_eq!(
            parse_impersonate(Some("chrome,firefox,safari")),
            Impersonate::pool(["chrome", "firefox", "safari"])
        );
        assert_eq!(
            parse_impersonate(Some(" chrome , firefox ")),
            Impersonate::pool(["chrome", "firefox"])
        );
        // A trailing comma keeps the empty element, exactly like `str.split(",")`.
        assert_eq!(
            parse_impersonate(Some("chrome,")),
            Impersonate::pool(["chrome", ""])
        );
    }

    /// The quirk that surprises everyone: `__BuildRequest` always writes the
    /// `impersonate` key, so its `None` **overrides** the session's `"chrome"`.
    #[test]
    fn no_impersonate_flag_means_impersonation_off_not_the_chrome_default() {
        assert_eq!(parse_impersonate(None), Impersonate::None);
        assert!(!parse_impersonate(None).is_enabled());
        assert_ne!(parse_impersonate(None), Impersonate::default());
    }

    // -- `__BuildRequest` ---------------------------------------------------

    #[test]
    fn the_default_plan_matches_pythons_kwargs() {
        let plan = plan(&["browser-rs", "extract", "get", "https://x.test/", "o.md"]);
        assert!(plan.headers.is_empty());
        assert!(plan.cookies.is_empty());
        assert!(plan.params.is_empty());
        assert_eq!(plan.json, None);
        assert_eq!(plan.data, None);
        assert_eq!(plan.timeout, Duration::from_secs(30));
        assert_eq!(plan.follow_redirects, FollowRedirects::Bool(true));
        assert!(plan.verify);
        assert!(plan.stealthy_headers);
        assert_eq!(plan.impersonate, Impersonate::None);
        assert_eq!(plan.proxy, None);
    }

    /// `--timeout 5` is five **seconds** here. The same number on `fetch` is
    /// five milliseconds. This is the 1000× mistake, caught at the one place
    /// the unit is read.
    #[test]
    fn the_http_timeout_is_read_as_seconds() {
        let plan = plan(&[
            "browser-rs",
            "extract",
            "get",
            "--timeout",
            "5",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(plan.timeout, Duration::from_secs(5));
        assert_eq!(plan.timeout.as_millis(), 5000);
    }

    #[test]
    fn headers_and_cookies_are_separated_the_way_parse_headers_does() {
        let plan = plan(&[
            "browser-rs",
            "extract",
            "get",
            "-H",
            "User-Agent: x",
            "-H",
            "Cookie: a=1; b=2",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(plan.headers["User-Agent"], "x");
        assert!(!plan.headers.contains_key("Cookie"));
        assert_eq!(plan.cookies["a"], "1");
        assert_eq!(plan.cookies["b"], "2");
    }

    #[test]
    fn the_cookies_flag_merges_over_a_cookie_header() {
        let plan = plan(&[
            "browser-rs",
            "extract",
            "get",
            "-H",
            "Cookie: a=from-header; keep=me",
            "--cookies",
            "a=from-flag",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(plan.cookies["a"], "from-flag");
        assert_eq!(plan.cookies["keep"], "me", "the merge is not a replace");
    }

    #[test]
    fn a_header_without_a_colon_is_an_error() {
        let options = http_options(&[
            "browser-rs",
            "extract",
            "get",
            "-H",
            "InvalidHeaderWithoutColon",
            "https://x.test/",
            "o.md",
        ]);
        let error = build_http_plan(&options, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Could not parse header without colon"),
            "{error}"
        );
    }

    /// `_ParseHeaders`'s other branch: a colon-less line ending in `;` becomes
    /// a header with an empty value rather than an error.
    #[test]
    fn a_colon_less_header_ending_in_a_semicolon_is_accepted() {
        let options = http_options(&[
            "browser-rs",
            "extract",
            "get",
            "-H",
            "InvalidHeader;",
            "https://x.test/",
            "o.md",
        ]);
        let plan = build_http_plan(&options, None).expect("builds");
        assert_eq!(plan.headers["InvalidHeader"], "");
    }

    #[test]
    fn the_body_options_reach_the_plan_for_post() {
        let cli = Cli::try_parse_from([
            "browser-rs",
            "extract",
            "post",
            "-j",
            r#"{"a": 1}"#,
            "-d",
            "b=2",
            "https://x.test/",
            "o.md",
        ])
        .expect("parses");
        let Command::Extract {
            command: ExtractCommand::Post(command),
        } = cli.command
        else {
            panic!("expected post")
        };
        let plan = build_http_plan(&command.options, Some(&command.body)).expect("builds");
        assert_eq!(plan.json, Some(serde_json::json!({"a": 1})));
        assert_eq!(plan.data.as_deref(), Some("b=2"));
    }

    #[test]
    fn a_proxy_reaches_the_plan_as_a_url() {
        let plan = plan(&[
            "browser-rs",
            "extract",
            "get",
            "--proxy",
            "http://u:p@host:8080",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(
            plan.proxy,
            Some(Proxy::Url("http://u:p@host:8080".to_owned()))
        );
    }

    /// `--proxy ''` is forwarded as the empty string, exactly as Python does;
    /// it is `proxy_is_truthy` downstream that turns it into "no proxy", not the
    /// CLI. Corpus row `get-proxy-empty`.
    #[test]
    fn an_empty_proxy_string_is_forwarded_and_only_falsy_downstream() {
        let plan = plan(&[
            "browser-rs",
            "extract",
            "get",
            "--proxy",
            "",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(plan.proxy, Some(Proxy::Url(String::new())));
        assert!(!browser::http::config::proxy_is_truthy(
            plan.proxy.as_ref().expect("set")
        ));
    }

    // -- the builder --------------------------------------------------------

    #[test]
    fn the_plan_is_applied_to_a_request_builder() {
        let plan = plan(&[
            "browser-rs",
            "extract",
            "get",
            "-H",
            "A: 1",
            "-p",
            "q=x",
            "--cookies",
            "c=1",
            "--timeout",
            "7",
            "--no-verify",
            "--no-follow-redirects",
            "--no-stealthy-headers",
            "--impersonate",
            "firefox",
            "https://x.test/",
            "o.md",
        ]);
        let builder = apply_http_plan(AsyncFetcher::get("https://x.test/"), &plan);
        let overrides = builder.overrides();
        assert_eq!(
            overrides.timeout,
            browser::Override::Set(Some(Duration::from_secs(7)))
        );
        assert_eq!(overrides.verify, browser::Override::Set(false));
        assert_eq!(
            overrides.follow_redirects,
            browser::Override::Set(FollowRedirects::Bool(false))
        );
        assert_eq!(overrides.stealthy_headers, browser::Override::Set(false));
        assert_eq!(
            overrides.impersonate,
            browser::Override::Set(Impersonate::single("firefox"))
        );
        assert_eq!(
            overrides.params,
            browser::Override::Set(vec![("q".to_owned(), "x".to_owned())])
        );
    }

    /// The setters that Python always writes are always called, so an explicit
    /// "off" beats a session default — even the ones whose value is the same as
    /// the default.
    #[test]
    fn the_always_present_keys_are_always_set_on_the_builder() {
        let plan = plan(&["browser-rs", "extract", "get", "https://x.test/", "o.md"]);
        let overrides = apply_http_plan(AsyncFetcher::get("https://x.test/"), &plan)
            .overrides()
            .clone();
        assert!(!overrides.timeout.is_unset());
        assert!(!overrides.verify.is_unset());
        assert!(!overrides.follow_redirects.is_unset());
        assert!(!overrides.stealthy_headers.is_unset());
        assert!(!overrides.impersonate.is_unset());
        assert!(!overrides.proxy.is_unset());
        // …and the ones Python omits when falsy stay unset.
        assert!(overrides.headers.is_unset());
        assert!(overrides.cookies.is_unset());
        assert!(overrides.params.is_unset());
        assert!(overrides.body.is_unset());
    }

    #[test]
    fn json_wins_over_data_when_both_are_given() {
        let options = HttpOptions::default();
        let body = DataOptions {
            json: Some(r#"{"a": 1}"#.to_owned()),
            data: Some("b=2".to_owned()),
        };
        let plan = build_http_plan(&options, Some(&body)).expect("builds");
        let overrides = apply_http_plan(AsyncFetcher::post("https://x.test/"), &plan)
            .overrides()
            .clone();
        assert_eq!(
            overrides.body,
            browser::Override::Set(Body::Json(serde_json::json!({"a": 1})))
        );
    }

    // -- the browser config -------------------------------------------------

    #[cfg(feature = "browser")]
    fn browser_options(argv: &[&str]) -> crate::args::BrowserOptions {
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Extract {
                command: ExtractCommand::Fetch(command),
            } => command.options,
            other => panic!("{other:?}"),
        }
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_browser_defaults_reach_the_config_unchanged() {
        let options =
            browser_options(&["browser-rs", "extract", "fetch", "https://x.test/", "o.md"]);
        let config = build_browser_config(&options, None).expect("valid");
        assert!(config.headless);
        assert!(!config.disable_resources);
        assert!(!config.network_idle);
        assert!((config.timeout_ms - 30_000.0).abs() < f64::EPSILON);
        assert!((config.wait_ms - 0.0).abs() < f64::EPSILON);
        assert!(!config.real_chrome);
        assert!(!config.dns_over_https);
        assert!(!config.block_ads);
        assert_eq!(config.wait_selector, None);
        assert_eq!(config.proxy, None);
        assert_eq!(config.extra_headers, None);
        assert_eq!(config.executable_path, None);
        assert_eq!(config.stealth, None, "plain `fetch` is not stealthy");
    }

    /// `--timeout 5000` on a browser command is five **seconds**, because the
    /// number is already milliseconds. The HTTP command would have made it 5000
    /// seconds.
    #[cfg(feature = "browser")]
    #[test]
    fn the_browser_timeout_is_read_as_milliseconds() {
        let options = browser_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "--timeout",
            "5000",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&options, None).expect("valid");
        assert!((config.timeout_ms - 5000.0).abs() < f64::EPSILON);
        assert_eq!(
            Duration::from_secs_f64(config.timeout_ms / 1000.0),
            Duration::from_secs(5)
        );
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_browser_flags_reach_the_config() {
        let options = browser_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "--no-headless",
            "--disable-resources",
            "--network-idle",
            "--real-chrome",
            "--dns-over-https",
            "--block-ads",
            "--wait",
            "250",
            "--wait-selector",
            "#done",
            "--locale",
            "en-GB",
            "-H",
            "X-Test: 1",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&options, None).expect("valid");
        assert!(!config.headless);
        assert!(config.disable_resources);
        assert!(config.network_idle);
        assert!(config.real_chrome);
        assert!(config.dns_over_https);
        assert!(config.block_ads);
        assert!((config.wait_ms - 250.0).abs() < f64::EPSILON);
        assert_eq!(config.wait_selector.as_deref(), Some("#done"));
        assert_eq!(config.locale.as_deref(), Some("en-GB"));
        assert_eq!(config.extra_headers.expect("headers")["X-Test"], "1");
        assert!(
            config.blocked_domains.is_some_and(|set| !set.is_empty()),
            "`block_ads` unions in AD_DOMAINS during validation"
        );
    }

    /// `_ParseHeaders(extra_headers, parse_cookies=False)` — a `Cookie:` line is
    /// an ordinary header for a browser, not a cookie jar entry.
    #[cfg(feature = "browser")]
    #[test]
    fn extra_headers_do_not_divert_a_cookie_line() {
        let options = browser_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "-H",
            "Cookie: a=1",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&options, None).expect("valid");
        assert_eq!(config.extra_headers.expect("headers")["Cookie"], "a=1");
        assert!(config.cookies.is_empty());
    }

    #[cfg(feature = "browser")]
    fn stealthy(argv: &[&str]) -> crate::args::StealthyCommand {
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Extract {
                command: ExtractCommand::StealthyFetch(command),
            } => command,
            other => panic!("{other:?}"),
        }
    }

    #[cfg(feature = "browser")]
    #[test]
    fn stealthy_fetch_builds_a_stealthy_config() {
        let command = stealthy(&[
            "browser-rs",
            "extract",
            "stealthy-fetch",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&command.options, Some(&command)).expect("valid");
        let extras = config.stealth.expect("stealth extras");
        assert!(extras.allow_webgl);
        assert!(!extras.hide_canvas);
        assert!(!extras.block_webrtc);
        assert!(!extras.solve_cloudflare);
    }

    /// `StealthConfig.__post_init__` raises `timeout` to 60 s when
    /// `solve_cloudflare` is set at construction — which is exactly what
    /// `stealthy-fetch --solve-cloudflare` is.
    #[cfg(feature = "browser")]
    #[test]
    fn solve_cloudflare_raises_the_timeout_floor_to_sixty_seconds() {
        let command = stealthy(&[
            "browser-rs",
            "extract",
            "stealthy-fetch",
            "--solve-cloudflare",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&command.options, Some(&command)).expect("valid");
        assert!(
            (config.timeout_ms - 60_000.0).abs() < f64::EPSILON,
            "the default 30000 ms is raised, not kept: {}",
            config.timeout_ms
        );

        // An explicit larger timeout is left alone.
        let command = stealthy(&[
            "browser-rs",
            "extract",
            "stealthy-fetch",
            "--solve-cloudflare",
            "--timeout",
            "90000",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&command.options, Some(&command)).expect("valid");
        assert!((config.timeout_ms - 90_000.0).abs() < f64::EPSILON);
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_stealth_knobs_reach_the_extras() {
        let command = stealthy(&[
            "browser-rs",
            "extract",
            "stealthy-fetch",
            "--block-webrtc",
            "--hide-canvas",
            "--block-webgl",
            "https://x.test/",
            "o.md",
        ]);
        let config = build_browser_config(&command.options, Some(&command)).expect("valid");
        let extras = config.stealth.expect("stealth extras");
        assert!(extras.block_webrtc);
        assert!(extras.hide_canvas);
        assert!(!extras.allow_webgl);
    }

    #[cfg(feature = "browser")]
    #[test]
    fn a_missing_executable_is_rejected_by_validation() {
        let options = browser_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "--executable-path",
            "/definitely/not/a/browser",
            "https://x.test/",
            "o.md",
        ]);
        assert!(build_browser_config(&options, None).is_err());
    }

    #[cfg(feature = "browser")]
    #[test]
    fn a_malformed_extra_header_is_reported_rather_than_dropped() {
        let options = browser_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "-H",
            "no-colon-here",
            "https://x.test/",
            "o.md",
        ]);
        assert!(build_browser_config(&options, None).is_err());
    }
}
