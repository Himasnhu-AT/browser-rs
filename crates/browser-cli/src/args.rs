//! The `clap` derive tree: `get` / `post` / `put` / `delete` / `fetch` / `stealthy-fetch`,
//! the shared option groups, the `--flag` / `--no-flag` pairs and the repeatable `-H` /
//! `-p` options.
//!
//! This is a transcription of `reference/cli.py`'s Click decorators
//! (`cli-ai.md` §2.1 and §2.5). Every option name, short form, default and help
//! string is Python's; the help *layout* is `clap`'s, which is the one visible
//! difference — Click renders a boolean pair as a single
//! `--verify / --no-verify` line and `clap` renders two.
//!
//! # The seconds-versus-milliseconds split
//!
//! `--timeout` means **seconds** on `get` / `post` / `put` / `delete` (default
//! `30`) and **milliseconds** on `fetch` / `stealthy-fetch` (default `30000`).
//! Nothing in the type system catches a mix-up — both are integers — so the two
//! groups keep the unit in the field name ([`HttpOptions::timeout_secs`],
//! [`BrowserOptions::timeout_ms`]) and the conversion happens exactly once, in
//! [`crate::commands::extract`].
//!
//! # What is not here
//!
//! Python's `shell` and `mcp` commands are out of scope for the port
//! (`ARCHITECTURE.md` §8: no IPython shell, no MCP server). `uncurl` takes the
//! place of the shell's `uncurl()` / `curl2fetcher()` helpers, which were the
//! only two shell features that do not need a REPL.

use clap::{Args, Parser, Subcommand};

/// Help text appended to every `extract` subcommand, verbatim from
/// `__OUTPUT_FILE_HELP__`.
pub const OUTPUT_FILE_HELP: &str = "The output file path can be an HTML file, a Markdown file of \
     the HTML content, or the text content itself. Use file extensions (`.html`/`.md`/`.txt`) \
     respectively.";

/// The `extract` group's own help line, verbatim.
pub const EXTRACT_ABOUT: &str = "Fetch web pages using various fetchers and extract full/selected \
     HTML content as HTML, Markdown, or extract text content.";

/// The environment variable `--executable-path` falls back to.
pub const EXECUTABLE_PATH_ENV: &str = "BROWSER_RS_EXECUTABLE_PATH";

/// The `browser-rs` command line.
///
/// `display_name` is set so `--version` prints `the reference implementation, version <v>`, which
/// is what Click's `version_option(prog_name="the reference implementation")` produces. The version
/// number is **this crate's**, not `the reference implementation`'s.
#[derive(Debug, Parser)]
#[command(
    name = "browser-rs",
    display_name = "the reference implementation, version",
    version,
    about = "An undetectable, adaptive web-scraping toolkit.",
    long_about = None,
    propagate_version = false,
)]
pub struct Cli {
    /// Log level.
    ///
    /// Python only exposes this on `shell`; here it is global, because there is
    /// no REPL to hang it off. `RUST_LOG`, when set, wins.
    #[arg(
        short = 'L',
        long = "loglevel",
        value_name = "LEVEL",
        help = "Log level (default: INFO). `RUST_LOG` overrides it",
        long_help = None,
        default_value = "info",
        global = true,
        ignore_case = true,
        value_parser = ["debug", "info", "warning", "error", "critical", "fatal"],
        overrides_with = "loglevel",
    )]
    pub loglevel: String,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The top-level command tree.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install all the reference implementation's Fetchers dependencies
    #[cfg(feature = "browser")]
    #[command(about = "Install all the reference implementation's Fetchers dependencies")]
    Install(InstallOptions),

    /// Fetch web pages and extract HTML, Markdown or text.
    #[command(about = EXTRACT_ABOUT)]
    Extract {
        /// Which fetcher to use.
        #[command(subcommand)]
        command: ExtractCommand,
    },

    /// Convert a `curl` command line into a the reference implementation request.
    ///
    /// This is the shell's `uncurl()` — and, with `--output`, its
    /// `curl2fetcher()` — as a subcommand.
    Uncurl(UncurlOptions),
}

/// `browser-rs extract <...>`.
#[derive(Debug, Subcommand)]
pub enum ExtractCommand {
    /// Perform a GET request and save the content to a file.
    #[command(about = "Perform a GET request and save the content to a file.", after_help = OUTPUT_FILE_HELP)]
    Get(HttpCommand),

    /// Perform a POST request and save the content to a file.
    #[command(about = "Perform a POST request and save the content to a file.", after_help = OUTPUT_FILE_HELP)]
    Post(HttpBodyCommand),

    /// Perform a PUT request and save the content to a file.
    #[command(about = "Perform a PUT request and save the content to a file.", after_help = OUTPUT_FILE_HELP)]
    Put(HttpBodyCommand),

    /// Perform a DELETE request and save the content to a file.
    #[command(about = "Perform a DELETE request and save the content to a file.", after_help = OUTPUT_FILE_HELP)]
    Delete(HttpCommand),

    /// Use DynamicFetcher to fetch content with browser automation.
    #[cfg(feature = "browser")]
    #[command(about = "Use DynamicFetcher to fetch content with browser automation.", after_help = OUTPUT_FILE_HELP)]
    Fetch(BrowserCommand),

    /// Use StealthyFetcher to fetch content with advanced stealth features.
    #[cfg(feature = "browser")]
    #[command(about = "Use StealthyFetcher to fetch content with advanced stealth features.", after_help = OUTPUT_FILE_HELP)]
    StealthyFetch(StealthyCommand),
}

// ---------------------------------------------------------------------------
// The two positional arguments every `extract` subcommand takes.
// ---------------------------------------------------------------------------

/// `URL OUTPUT_FILE`, in Click's order.
#[derive(Debug, Args, Clone)]
pub struct Targets {
    /// The URL to fetch.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Where to write the extracted content. The suffix picks the format.
    #[arg(value_name = "OUTPUT_FILE")]
    pub output_file: String,
}

// ---------------------------------------------------------------------------
// `_common_http_options`
// ---------------------------------------------------------------------------

/// The option group `get`, `post`, `put` and `delete` share.
#[derive(Debug, Args, Clone, Default)]
pub struct HttpOptions {
    /// HTTP headers in format "Key: Value" (can be used multiple times)
    #[arg(short = 'H', long = "headers", value_name = "TEXT")]
    pub headers: Vec<String>,

    /// Cookies string in format "name1=value1; name2=value2"
    #[arg(long, value_name = "TEXT", overrides_with = "cookies")]
    pub cookies: Option<String>,

    /// Request timeout in seconds (default: 30)
    #[arg(
        long = "timeout",
        value_name = "INTEGER",
        default_value_t = 30,
        overrides_with = "timeout_secs"
    )]
    pub timeout_secs: u64,

    /// Proxy URL in format "http://username:password@host:port"
    #[arg(long, value_name = "TEXT", overrides_with = "proxy")]
    pub proxy: Option<String>,

    /// CSS selector to extract specific content from the page. It returns all matches.
    #[arg(
        short = 's',
        long,
        value_name = "TEXT",
        overrides_with = "css_selector"
    )]
    pub css_selector: Option<String>,

    /// Query parameters in format "key=value" (can be used multiple times)
    #[arg(short = 'p', long = "params", value_name = "TEXT")]
    pub params: Vec<String>,

    /// Whether to follow redirects (default: True)
    #[arg(long = "follow-redirects", overrides_with_all = ["follow_redirects_flag", "no_follow_redirects"])]
    pub follow_redirects_flag: bool,

    /// Turn off --follow-redirects.
    #[arg(long = "no-follow-redirects", overrides_with_all = ["no_follow_redirects", "follow_redirects_flag"])]
    pub no_follow_redirects: bool,

    /// Whether to verify SSL certificates (default: True)
    #[arg(long = "verify", overrides_with_all = ["verify_flag", "no_verify"])]
    pub verify_flag: bool,

    /// Turn off --verify.
    #[arg(long = "no-verify", overrides_with_all = ["no_verify", "verify_flag"])]
    pub no_verify: bool,

    /// Browser to impersonate. Can be a single browser (e.g., chrome) or comma-separated
    /// list for random selection (e.g., chrome,firefox,safari).
    #[arg(long, value_name = "TEXT", overrides_with = "impersonate")]
    pub impersonate: Option<String>,

    /// Use stealthy browser headers (default: True)
    #[arg(long = "stealthy-headers", overrides_with_all = ["stealthy_headers_flag", "no_stealthy_headers"])]
    pub stealthy_headers_flag: bool,

    /// Turn off --stealthy-headers.
    #[arg(long = "no-stealthy-headers", overrides_with_all = ["no_stealthy_headers", "stealthy_headers_flag"])]
    pub no_stealthy_headers: bool,

    /// Extract only main content and sanitize hidden elements for AI consumption
    /// (default: False)
    #[arg(long = "ai-targeted", overrides_with = "ai_targeted")]
    pub ai_targeted: bool,
}

impl HttpOptions {
    /// `--follow-redirects` / `--no-follow-redirects`, default `True`.
    #[must_use]
    pub const fn follow_redirects(&self) -> bool {
        !self.no_follow_redirects
    }

    /// `--verify` / `--no-verify`, default `True`.
    #[must_use]
    pub const fn verify(&self) -> bool {
        !self.no_verify
    }

    /// `--stealthy-headers` / `--no-stealthy-headers`, default `True`.
    #[must_use]
    pub const fn stealthy_headers(&self) -> bool {
        !self.no_stealthy_headers
    }
}

/// `_data_options`, applied to `post` and `put` only.
#[derive(Debug, Args, Clone, Default)]
pub struct DataOptions {
    /// JSON data to include in the request body (as string)
    #[arg(short = 'j', long, value_name = "TEXT", overrides_with = "json")]
    pub json: Option<String>,

    /// Form data to include in the request body (as string, ex:
    /// "param1=value1&param2=value2")
    #[arg(short = 'd', long, value_name = "TEXT", overrides_with = "data")]
    pub data: Option<String>,
}

/// `get` and `delete`.
#[derive(Debug, Args, Clone)]
pub struct HttpCommand {
    /// `URL OUTPUT_FILE`.
    #[command(flatten)]
    pub targets: Targets,
    /// The shared HTTP option group.
    #[command(flatten)]
    pub options: HttpOptions,
}

/// `post` and `put` — the same, plus a body.
#[derive(Debug, Args, Clone)]
pub struct HttpBodyCommand {
    /// `URL OUTPUT_FILE`.
    #[command(flatten)]
    pub targets: Targets,
    /// `--json` / `--data`.
    #[command(flatten)]
    pub body: DataOptions,
    /// The shared HTTP option group.
    #[command(flatten)]
    pub options: HttpOptions,
}

// ---------------------------------------------------------------------------
// `_common_browser_options`
// ---------------------------------------------------------------------------

/// The option group `fetch` and `stealthy-fetch` share.
#[cfg(feature = "browser")]
#[derive(Debug, Args, Clone, Default)]
pub struct BrowserOptions {
    /// Extra headers in format "Key: Value" (can be used multiple times)
    #[arg(short = 'H', long = "extra-headers", value_name = "TEXT")]
    pub extra_headers: Vec<String>,

    /// Proxy URL in format "http://username:password@host:port"
    #[arg(long, value_name = "TEXT", overrides_with = "proxy")]
    pub proxy: Option<String>,

    /// Specify user locale. Defaults to the system default locale.
    #[arg(long, value_name = "TEXT", overrides_with = "locale")]
    pub locale: Option<String>,

    /// CSS selector to wait for before proceeding
    #[arg(
        long = "wait-selector",
        value_name = "TEXT",
        overrides_with = "wait_selector"
    )]
    pub wait_selector: Option<String>,

    /// CSS selector to extract specific content from the page. It returns all matches.
    #[arg(
        short = 's',
        long,
        value_name = "TEXT",
        overrides_with = "css_selector"
    )]
    pub css_selector: Option<String>,

    /// Additional wait time in milliseconds after page load (default: 0)
    #[arg(
        long = "wait",
        value_name = "INTEGER",
        default_value_t = 0,
        overrides_with = "wait_ms"
    )]
    pub wait_ms: u64,

    /// Timeout in milliseconds (default: 30000)
    #[arg(
        long = "timeout",
        value_name = "INTEGER",
        default_value_t = 30_000,
        overrides_with = "timeout_ms"
    )]
    pub timeout_ms: u64,

    /// Path to a custom Chromium-compatible browser executable. Falls back to the
    /// BROWSER_RS_EXECUTABLE_PATH environment variable when not set.
    #[arg(
        long = "executable-path",
        value_name = "TEXT",
        overrides_with = "executable_path"
    )]
    pub executable_path: Option<String>,

    /// Run browser in headless mode (default: True)
    #[arg(long = "headless", overrides_with_all = ["headless_flag", "no_headless"])]
    pub headless_flag: bool,

    /// Turn off --headless.
    #[arg(long = "no-headless", overrides_with_all = ["no_headless", "headless_flag"])]
    pub no_headless: bool,

    /// Drop unnecessary resources for speed boost (default: False)
    #[arg(long = "disable-resources", overrides_with_all = ["disable_resources_flag", "enable_resources"])]
    pub disable_resources_flag: bool,

    /// Keep every resource — the negative half of --disable-resources.
    #[arg(long = "enable-resources", overrides_with_all = ["enable_resources", "disable_resources_flag"])]
    pub enable_resources: bool,

    /// Wait for network idle (default: False)
    #[arg(long = "network-idle", overrides_with_all = ["network_idle_flag", "no_network_idle"])]
    pub network_idle_flag: bool,

    /// Turn off --network-idle.
    #[arg(long = "no-network-idle", overrides_with_all = ["no_network_idle", "network_idle_flag"])]
    pub no_network_idle: bool,

    /// If you have a Chrome browser installed on your device, enable this, and the
    /// Fetcher will launch an instance of your browser and use it. (default: False)
    #[arg(long = "real-chrome", overrides_with_all = ["real_chrome_flag", "no_real_chrome"])]
    pub real_chrome_flag: bool,

    /// Turn off --real-chrome.
    #[arg(long = "no-real-chrome", overrides_with_all = ["no_real_chrome", "real_chrome_flag"])]
    pub no_real_chrome: bool,

    /// Route DNS through Cloudflare's DoH to prevent DNS leaks when using proxies
    /// (default: False)
    #[arg(long = "dns-over-https", overrides_with_all = ["dns_over_https_flag", "no_dns_over_https"])]
    pub dns_over_https_flag: bool,

    /// Turn off --dns-over-https.
    #[arg(long = "no-dns-over-https", overrides_with_all = ["no_dns_over_https", "dns_over_https_flag"])]
    pub no_dns_over_https: bool,

    /// Block requests to known ad and tracker domains (default: False)
    #[arg(long = "block-ads", overrides_with_all = ["block_ads_flag", "no_block_ads"])]
    pub block_ads_flag: bool,

    /// Turn off --block-ads.
    #[arg(long = "no-block-ads", overrides_with_all = ["no_block_ads", "block_ads_flag"])]
    pub no_block_ads: bool,

    /// Extract only main content and sanitize hidden elements for AI consumption
    /// (default: False)
    #[arg(long = "ai-targeted", overrides_with = "ai_targeted")]
    pub ai_targeted: bool,
}

#[cfg(feature = "browser")]
impl BrowserOptions {
    /// `--headless` / `--no-headless`, default `True`.
    #[must_use]
    pub const fn headless(&self) -> bool {
        !self.no_headless
    }

    /// `--disable-resources` / `--enable-resources`, default `False`.
    #[must_use]
    pub const fn disable_resources(&self) -> bool {
        self.disable_resources_flag
    }

    /// `--network-idle` / `--no-network-idle`, default `False`.
    #[must_use]
    pub const fn network_idle(&self) -> bool {
        self.network_idle_flag
    }

    /// `--real-chrome` / `--no-real-chrome`, default `False`.
    #[must_use]
    pub const fn real_chrome(&self) -> bool {
        self.real_chrome_flag
    }

    /// `--dns-over-https` / `--no-dns-over-https`, default `False`.
    #[must_use]
    pub const fn dns_over_https(&self) -> bool {
        self.dns_over_https_flag
    }

    /// `--block-ads` / `--no-block-ads`, default `False`.
    #[must_use]
    pub const fn block_ads(&self) -> bool {
        self.block_ads_flag
    }

    /// The resolved browser binary, in `cli-ai.md` §1394's precedence:
    /// `--executable-path` > `$BROWSER_RS_EXECUTABLE_PATH` > absent.
    ///
    /// "Absent" is the load-bearing third case: Python leaves the key **out of
    /// the kwargs dict** rather than passing `None`, so the session falls
    /// through to Playwright's own browser discovery. Here that is
    /// `BrowserConfig::executable_path == None`, which lets
    /// `browser_cdp::session::resolve_executable` fall through to
    /// `chromiumoxide`'s detection (which honours `$CHROME`).
    ///
    /// An **empty** `--executable-path ""` is falsy in Python, so it falls
    /// through to the environment; the same is done here.
    #[must_use]
    pub fn resolve_executable_path(&self) -> Option<String> {
        resolve_executable_path(self.executable_path.as_deref(), |name: &str| {
            std::env::var_os(name)
        })
    }
}

/// The pure half of [`BrowserOptions::resolve_executable_path`], with the
/// environment injected so it can be tested without touching the process.
///
/// ```
/// use std::ffi::OsString;
/// use browser_cli::args::resolve_executable_path;
///
/// let env = |name: &str| (name == "BROWSER_RS_EXECUTABLE_PATH").then(|| OsString::from("/env/chrome"));
/// assert_eq!(resolve_executable_path(Some("/flag/chrome"), env).as_deref(), Some("/flag/chrome"));
/// assert_eq!(resolve_executable_path(None, env).as_deref(), Some("/env/chrome"));
/// assert_eq!(resolve_executable_path(Some(""), env).as_deref(), Some("/env/chrome"));
/// assert_eq!(resolve_executable_path(None, |_: &str| None).as_deref(), None);
/// ```
pub fn resolve_executable_path<F>(flag: Option<&str>, env: F) -> Option<String>
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    // `executable_path or environ.get(...) or None`: Python's `or` treats the
    // empty string as absent on *both* sides.
    let from_flag = flag.filter(|value| !value.is_empty()).map(str::to_owned);
    from_flag.or_else(|| {
        env(EXECUTABLE_PATH_ENV)
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty())
    })
}

/// `fetch`.
#[cfg(feature = "browser")]
#[derive(Debug, Args, Clone)]
pub struct BrowserCommand {
    /// `URL OUTPUT_FILE`.
    #[command(flatten)]
    pub targets: Targets,
    /// The shared browser option group.
    #[command(flatten)]
    pub options: BrowserOptions,
}

/// `stealthy-fetch` — the browser group plus four stealth knobs.
#[cfg(feature = "browser")]
#[derive(Debug, Args, Clone)]
pub struct StealthyCommand {
    /// `URL OUTPUT_FILE`.
    #[command(flatten)]
    pub targets: Targets,

    /// Block WebRTC entirely (default: False)
    #[arg(long = "block-webrtc", overrides_with_all = ["block_webrtc_flag", "allow_webrtc"])]
    pub block_webrtc_flag: bool,

    /// Allow WebRTC — the negative half of --block-webrtc.
    #[arg(long = "allow-webrtc", overrides_with_all = ["allow_webrtc", "block_webrtc_flag"])]
    pub allow_webrtc: bool,

    /// Solve Cloudflare challenges (default: False)
    #[arg(long = "solve-cloudflare", overrides_with_all = ["solve_cloudflare_flag", "no_solve_cloudflare"])]
    pub solve_cloudflare_flag: bool,

    /// Turn off --solve-cloudflare.
    #[arg(long = "no-solve-cloudflare", overrides_with_all = ["no_solve_cloudflare", "solve_cloudflare_flag"])]
    pub no_solve_cloudflare: bool,

    /// Allow WebGL (default: True)
    #[arg(long = "allow-webgl", overrides_with_all = ["allow_webgl_flag", "block_webgl"])]
    pub allow_webgl_flag: bool,

    /// Block WebGL — the negative half of --allow-webgl.
    #[arg(long = "block-webgl", overrides_with_all = ["block_webgl", "allow_webgl_flag"])]
    pub block_webgl: bool,

    /// Add noise to canvas operations (default: False)
    #[arg(long = "hide-canvas", overrides_with_all = ["hide_canvas_flag", "show_canvas"])]
    pub hide_canvas_flag: bool,

    /// Leave the canvas alone — the negative half of --hide-canvas.
    #[arg(long = "show-canvas", overrides_with_all = ["show_canvas", "hide_canvas_flag"])]
    pub show_canvas: bool,

    /// The shared browser option group.
    #[command(flatten)]
    pub options: BrowserOptions,
}

#[cfg(feature = "browser")]
impl StealthyCommand {
    /// `--block-webrtc` / `--allow-webrtc`, default `False`.
    #[must_use]
    pub const fn block_webrtc(&self) -> bool {
        self.block_webrtc_flag
    }

    /// `--solve-cloudflare` / `--no-solve-cloudflare`, default `False`.
    #[must_use]
    pub const fn solve_cloudflare(&self) -> bool {
        self.solve_cloudflare_flag
    }

    /// `--allow-webgl` / `--block-webgl`, default `True`.
    #[must_use]
    pub const fn allow_webgl(&self) -> bool {
        !self.block_webgl
    }

    /// `--hide-canvas` / `--show-canvas`, default `False`.
    #[must_use]
    pub const fn hide_canvas(&self) -> bool {
        self.hide_canvas_flag
    }
}

// ---------------------------------------------------------------------------
// `install` and `uncurl`
// ---------------------------------------------------------------------------

/// `browser-rs install`.
#[cfg(feature = "browser")]
#[derive(Debug, Args, Clone, Default)]
pub struct InstallOptions {
    /// Force the reference implementation to reinstall all Fetchers dependencies
    #[arg(short = 'f', long = "force", overrides_with = "force")]
    pub force: bool,

    /// Where to install the browser. Defaults to `chromiumoxide`'s own cache
    /// directory, which is what `browser-cdp` will later find.
    #[arg(long = "path", value_name = "DIR", overrides_with = "path")]
    pub path: Option<std::path::PathBuf>,
}

/// `browser-rs uncurl`.
#[derive(Debug, Args, Clone, Default)]
pub struct UncurlOptions {
    /// Fetch the request and write the page here instead of printing the parsed
    /// request. The suffix picks the format, as for `extract`.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT_FILE",
        overrides_with = "output_file"
    )]
    pub output_file: Option<String>,

    /// CSS selector to extract specific content from the page. It returns all matches.
    #[arg(
        short = 's',
        long,
        value_name = "TEXT",
        overrides_with = "css_selector"
    )]
    pub css_selector: Option<String>,

    /// Extract only main content and sanitize hidden elements for AI consumption
    /// (default: False)
    #[arg(long = "ai-targeted", overrides_with = "ai_targeted")]
    pub ai_targeted: bool,

    /// The `curl` command.
    ///
    /// Give it as **one quoted string** — the DevTools "Copy as cURL" form —
    /// and it is split with shell rules. If several words are given instead,
    /// they are taken as already-split tokens. Because this swallows everything
    /// that follows, `--output` and friends must come first.
    #[arg(
        value_name = "CURL_COMMAND",
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(argv: &[&str]) -> Cli {
        Cli::try_parse_from(argv).expect("parses")
    }

    fn get_options(argv: &[&str]) -> HttpOptions {
        match parse(argv).command {
            Command::Extract {
                command: ExtractCommand::Get(command),
            } => command.options,
            other => panic!("expected `extract get`, got {other:?}"),
        }
    }

    #[test]
    fn the_command_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    // -- the shape of the tree --------------------------------------------

    #[test]
    fn the_six_extract_subcommands_are_named_as_click_names_them() {
        let extract = Cli::command()
            .get_subcommands()
            .find(|sub| sub.get_name() == "extract")
            .expect("`extract` exists")
            .clone();
        let names: Vec<_> = extract
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert!(names.contains(&"get"));
        assert!(names.contains(&"post"));
        assert!(names.contains(&"put"));
        assert!(names.contains(&"delete"));
        #[cfg(feature = "browser")]
        {
            assert!(names.contains(&"fetch"));
            // Click >= 8.2 turns `stealthy_fetch` into `stealthy-fetch`; so does
            // `clap`'s derive, for the same reason.
            assert!(names.contains(&"stealthy-fetch"), "{names:?}");
            assert!(!names.contains(&"stealthy_fetch"));
        }
    }

    #[test]
    fn both_positionals_are_required() {
        assert!(Cli::try_parse_from(["browser-rs", "extract", "get"]).is_err());
        assert!(Cli::try_parse_from(["browser-rs", "extract", "get", "https://x.test/"]).is_err());
        assert!(
            Cli::try_parse_from(["browser-rs", "extract", "get", "https://x.test/", "o.md"]).is_ok()
        );
    }

    #[test]
    fn a_missing_argument_is_a_usage_error_not_a_panic() {
        let error = Cli::try_parse_from(["browser-rs", "extract", "get"]).unwrap_err();
        // Click exits 2 for a usage error; so does `clap`.
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn version_prints_click_s_prog_name_form() {
        let rendered = Cli::command().render_version();
        assert!(
            rendered.starts_with("the reference implementation, version "),
            "{rendered:?} should look like Click's `the reference implementation, version 0.4.15`"
        );
    }

    // -- the HTTP option group --------------------------------------------

    #[test]
    fn the_http_defaults_match_python() {
        let options = get_options(&["browser-rs", "extract", "get", "https://x.test/", "o.md"]);
        assert_eq!(options.timeout_secs, 30, "seconds, not milliseconds");
        assert!(options.follow_redirects());
        assert!(options.verify());
        assert!(options.stealthy_headers());
        assert!(!options.ai_targeted);
        assert!(options.headers.is_empty());
        assert!(options.params.is_empty());
        assert_eq!(options.cookies, None);
        assert_eq!(options.proxy, None);
        assert_eq!(options.impersonate, None);
        assert_eq!(options.css_selector, None);
    }

    #[test]
    fn headers_and_params_are_repeatable_and_keep_their_order() {
        let options = get_options(&[
            "browser-rs",
            "extract",
            "get",
            "-H",
            "A: 1",
            "--headers",
            "B: 2",
            "-p",
            "q=x",
            "--params",
            "r=y",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(options.headers, ["A: 1", "B: 2"]);
        assert_eq!(options.params, ["q=x", "r=y"]);
    }

    #[test]
    fn the_boolean_pairs_are_mutually_overriding_and_last_wins() {
        let url = ["browser-rs", "extract", "get", "https://x.test/", "o.md"];

        let mut argv = url.to_vec();
        argv.push("--no-verify");
        assert!(!Cli::try_parse_from(&argv)
            .map(|cli| match cli.command {
                Command::Extract {
                    command: ExtractCommand::Get(command),
                } => command.options.verify(),
                _ => unreachable!(),
            })
            .expect("parses"));

        // `--no-verify --verify` -> the later one wins.
        let options = get_options(&[
            "browser-rs",
            "extract",
            "get",
            "--no-verify",
            "--verify",
            "https://x.test/",
            "o.md",
        ]);
        assert!(options.verify());

        // …and the other way round.
        let options = get_options(&[
            "browser-rs",
            "extract",
            "get",
            "--verify",
            "--no-verify",
            "https://x.test/",
            "o.md",
        ]);
        assert!(!options.verify());
    }

    #[test]
    fn every_http_boolean_pair_can_be_turned_off() {
        let options = get_options(&[
            "browser-rs",
            "extract",
            "get",
            "--no-follow-redirects",
            "--no-verify",
            "--no-stealthy-headers",
            "--ai-targeted",
            "https://x.test/",
            "o.md",
        ]);
        assert!(!options.follow_redirects());
        assert!(!options.verify());
        assert!(!options.stealthy_headers());
        assert!(options.ai_targeted);
    }

    #[test]
    fn post_and_put_take_a_body_and_get_and_delete_do_not() {
        let cli = parse(&[
            "browser-rs",
            "extract",
            "post",
            "-j",
            "{\"a\": 1}",
            "-d",
            "a=1",
            "https://x.test/",
            "o.md",
        ]);
        match cli.command {
            Command::Extract {
                command: ExtractCommand::Post(command),
            } => {
                assert_eq!(command.body.json.as_deref(), Some("{\"a\": 1}"));
                assert_eq!(command.body.data.as_deref(), Some("a=1"));
            }
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from([
            "browser-rs",
            "extract",
            "get",
            "-d",
            "a=1",
            "https://x.test/",
            "o.md"
        ])
        .is_err());
    }

    // -- the browser option group -----------------------------------------

    #[cfg(feature = "browser")]
    fn fetch_options(argv: &[&str]) -> BrowserOptions {
        match parse(argv).command {
            Command::Extract {
                command: ExtractCommand::Fetch(command),
            } => command.options,
            other => panic!("expected `extract fetch`, got {other:?}"),
        }
    }

    /// The 1000× mistake, pinned: the browser group's `--timeout` default is
    /// 30000 **milliseconds** where the HTTP group's is 30 **seconds**.
    #[cfg(feature = "browser")]
    #[test]
    fn the_browser_timeout_is_milliseconds_and_the_http_one_is_seconds() {
        let http = get_options(&["browser-rs", "extract", "get", "https://x.test/", "o.md"]);
        let browser = fetch_options(&["browser-rs", "extract", "fetch", "https://x.test/", "o.md"]);
        assert_eq!(http.timeout_secs, 30);
        assert_eq!(browser.timeout_ms, 30_000);
        assert_eq!(
            browser.timeout_ms,
            http.timeout_secs * 1000,
            "the two defaults describe the same wall-clock duration"
        );
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_browser_defaults_match_python() {
        let options = fetch_options(&["browser-rs", "extract", "fetch", "https://x.test/", "o.md"]);
        assert!(options.headless());
        assert!(!options.disable_resources());
        assert!(!options.network_idle());
        assert!(!options.real_chrome());
        assert!(!options.dns_over_https());
        assert!(!options.block_ads());
        assert_eq!(options.wait_ms, 0);
        assert_eq!(options.locale, None);
        assert_eq!(options.wait_selector, None);
        assert_eq!(options.executable_path, None);
        assert!(!options.ai_targeted);
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_asymmetric_browser_pairs_use_pythons_names() {
        let options = fetch_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "--disable-resources",
            "https://x.test/",
            "o.md",
        ]);
        assert!(options.disable_resources());
        let options = fetch_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "--disable-resources",
            "--enable-resources",
            "https://x.test/",
            "o.md",
        ]);
        assert!(!options.disable_resources());
    }

    #[cfg(feature = "browser")]
    #[test]
    fn extra_headers_uses_capital_h_like_the_http_group() {
        let options = fetch_options(&[
            "browser-rs",
            "extract",
            "fetch",
            "-H",
            "A: 1",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(options.extra_headers, ["A: 1"]);
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_stealth_only_options_default_the_way_python_does() {
        let cli = parse(&[
            "browser-rs",
            "extract",
            "stealthy-fetch",
            "https://x.test/",
            "o.md",
        ]);
        let Command::Extract {
            command: ExtractCommand::StealthyFetch(command),
        } = cli.command
        else {
            panic!("expected stealthy-fetch")
        };
        assert!(!command.block_webrtc());
        assert!(!command.solve_cloudflare());
        assert!(command.allow_webgl(), "the one that defaults to True");
        assert!(!command.hide_canvas());
    }

    #[cfg(feature = "browser")]
    #[test]
    fn the_stealth_only_options_can_all_be_flipped() {
        let cli = parse(&[
            "browser-rs",
            "extract",
            "stealthy-fetch",
            "--block-webrtc",
            "--solve-cloudflare",
            "--block-webgl",
            "--hide-canvas",
            "https://x.test/",
            "o.md",
        ]);
        let Command::Extract {
            command: ExtractCommand::StealthyFetch(command),
        } = cli.command
        else {
            panic!("expected stealthy-fetch")
        };
        assert!(command.block_webrtc());
        assert!(command.solve_cloudflare());
        assert!(!command.allow_webgl());
        assert!(command.hide_canvas());
    }

    /// The stealth-only flags are refused by plain `fetch`, exactly as Click
    /// refuses them (they decorate `stealthy_fetch` only).
    #[cfg(feature = "browser")]
    #[test]
    fn fetch_does_not_accept_the_stealth_only_flags() {
        for flag in [
            "--block-webrtc",
            "--solve-cloudflare",
            "--block-webgl",
            "--hide-canvas",
        ] {
            assert!(
                Cli::try_parse_from([
                    "browser-rs",
                    "extract",
                    "fetch",
                    flag,
                    "https://x.test/",
                    "o.md"
                ])
                .is_err(),
                "`fetch` must not accept {flag}"
            );
        }
    }

    // -- `BROWSER_RS_EXECUTABLE_PATH` --------------------------------------

    #[test]
    fn the_flag_beats_the_environment_which_beats_absent() {
        let env =
            |name: &str| (name == EXECUTABLE_PATH_ENV).then(|| std::ffi::OsString::from("/env/x"));
        assert_eq!(
            resolve_executable_path(Some("/flag/x"), env).as_deref(),
            Some("/flag/x")
        );
        assert_eq!(
            resolve_executable_path(None, env).as_deref(),
            Some("/env/x")
        );
        assert_eq!(resolve_executable_path(None, |_: &str| None), None);
    }

    /// Python's `executable_path or environ.get(...) or None` — an empty string
    /// is falsy on both sides, so it never reaches the fetcher.
    #[test]
    fn an_empty_value_is_falsy_on_both_sides() {
        let env =
            |name: &str| (name == EXECUTABLE_PATH_ENV).then(|| std::ffi::OsString::from("/env/x"));
        assert_eq!(
            resolve_executable_path(Some(""), env).as_deref(),
            Some("/env/x")
        );
        let empty_env =
            |name: &str| (name == EXECUTABLE_PATH_ENV).then(|| std::ffi::OsString::from(""));
        assert_eq!(resolve_executable_path(Some(""), empty_env), None);
        assert_eq!(resolve_executable_path(None, empty_env), None);
    }

    #[test]
    fn no_other_environment_variable_is_consulted() {
        let env = |name: &str| (name == "CHROME").then(|| std::ffi::OsString::from("/chrome"));
        assert_eq!(
            resolve_executable_path(None, env),
            None,
            "`$CHROME` is `chromiumoxide`'s business, not the CLI's"
        );
    }

    // -- `uncurl` ----------------------------------------------------------

    #[test]
    fn uncurl_takes_the_whole_command_as_one_quoted_string() {
        let cli = parse(&["browser-rs", "uncurl", "curl https://x.test/ -H 'A: 1'"]);
        let Command::Uncurl(options) = cli.command else {
            panic!("expected uncurl")
        };
        assert_eq!(options.command, ["curl https://x.test/ -H 'A: 1'"]);
        assert_eq!(options.output_file, None);
    }

    #[test]
    fn uncurl_also_accepts_already_split_tokens_including_hyphenated_ones() {
        let cli = parse(&[
            "browser-rs",
            "uncurl",
            "curl",
            "https://x.test/",
            "-H",
            "A: 1",
        ]);
        let Command::Uncurl(options) = cli.command else {
            panic!("expected uncurl")
        };
        assert_eq!(options.command, ["curl", "https://x.test/", "-H", "A: 1"]);
    }

    #[test]
    fn uncurl_options_have_to_come_before_the_command() {
        let cli = parse(&[
            "browser-rs",
            "uncurl",
            "-o",
            "out.md",
            "curl https://x.test/",
        ]);
        let Command::Uncurl(options) = cli.command else {
            panic!("expected uncurl")
        };
        assert_eq!(options.output_file.as_deref(), Some("out.md"));
        assert_eq!(options.command, ["curl https://x.test/"]);
    }

    #[test]
    fn uncurl_needs_a_command() {
        assert!(Cli::try_parse_from(["browser-rs", "uncurl"]).is_err());
    }

    // -- `install` ---------------------------------------------------------

    #[cfg(feature = "browser")]
    #[test]
    fn install_defaults_to_no_force_and_no_path() {
        let cli = parse(&["browser-rs", "install"]);
        let Command::Install(options) = cli.command else {
            panic!("expected install")
        };
        assert!(!options.force);
        assert_eq!(options.path, None);
    }

    #[cfg(feature = "browser")]
    #[test]
    fn install_accepts_both_spellings_of_force() {
        for flag in ["-f", "--force"] {
            let cli = parse(&["browser-rs", "install", flag]);
            let Command::Install(options) = cli.command else {
                panic!("expected install")
            };
            assert!(options.force);
        }
    }

    // -- the global log level ---------------------------------------------

    #[test]
    fn the_log_level_defaults_to_info_and_accepts_pythons_six_names() {
        assert_eq!(parse(&["browser-rs", "uncurl", "curl x"]).loglevel, "info");
        for level in ["debug", "info", "warning", "error", "critical", "fatal"] {
            let cli = parse(&["browser-rs", "-L", level, "uncurl", "curl x"]);
            assert_eq!(cli.loglevel, level);
        }
        // `click.Choice(..., case_sensitive=False)`.
        assert_eq!(
            parse(&["browser-rs", "-L", "DEBUG", "uncurl", "curl x"]).loglevel,
            "DEBUG"
        );
        assert!(Cli::try_parse_from(["browser-rs", "-L", "trace", "uncurl", "curl x"]).is_err());
    }

    #[test]
    fn the_log_level_is_global_so_it_can_follow_the_subcommand() {
        let cli = parse(&[
            "browser-rs",
            "extract",
            "get",
            "--loglevel",
            "error",
            "https://x.test/",
            "o.md",
        ]);
        assert_eq!(cli.loglevel, "error");
    }
}
