//! Configuration: [`DocumentConfig`] plus one option struct per Python keyword
//! block.
//!
//! `ARCHITECTURE.md` §11.2 / §5.6: Python's rich keyword defaults become
//! builder structs implementing [`Default`], so `node.css("div")` stays a
//! one-liner while `node.css_with("div", &CssOpts { adaptive: true,
//! ..Default::default() })` reaches every knob. **Every keyword argument in
//! `parser.md` has a field here.**

use std::fmt;
use std::sync::Arc;

#[cfg(feature = "adaptive")]
use browser_storage::StorageSystem;

/// The relocation threshold, in percent — Python's `percentage = 40`.
pub const DEFAULT_PERCENTAGE: u8 = 40;

/// [`SimilarOpts::threshold`] — Python's `similarity_threshold = 0.2`.
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.2;

/// [`SimilarOpts::ignore_attributes`] — Python's `("href", "src")`.
pub const DEFAULT_IGNORE_ATTRIBUTES: [&str; 2] = ["href", "src"];

/// [`AllTextOpts::ignore_tags`] — Python's `("script", "style")`.
pub const DEFAULT_IGNORE_TAGS: [&str; 2] = ["script", "style"];

/// [`AllTextOpts::separator`] — Python's `"\n"`.
pub const DEFAULT_SEPARATOR: &str = "\n";

/// `Selector.__repr__`'s truncation width.
pub const REPR_LENGTH_LIMIT: usize = 40;

/// The file name a document falls back to when adaptive storage is switched on
/// but no backend was supplied.
///
/// Python uses `<browser-rs package dir>/elements_storage.db`; a Rust crate has
/// no writable package directory, so the fallback is this name in the process's
/// working directory, overridable with the `BROWSER_RS_ADAPTIVE_DB` environment
/// variable. Recorded in `docs/DIVERGENCES.md`.
pub const DEFAULT_DB_FILE: &str = "elements_storage.db";

/// The environment variable that overrides [`DEFAULT_DB_FILE`].
pub const DB_FILE_ENV: &str = "BROWSER_RS_ADAPTIVE_DB";

/// Everything `Selector.__init__` accepts, minus `content` and `root`.
///
/// This is Python's *selector config*: the seven `parser_keywords` every
/// fetcher forwards (`huge_tree`, `keep_comments`, `keep_cdata`, `adaptive`,
/// `storage`, `storage_args`, `adaptive_domain`), plus `url` and `encoding`.
///
/// ```
/// use browser_parser::config::DocumentConfig;
///
/// let config = DocumentConfig::new()
///     .url("https://example.com/page")
///     .keep_comments(true);
/// assert_eq!(config.url, "https://example.com/page");
/// assert!(config.keep_comments);
/// assert!(!config.keep_cdata);
/// assert!(config.huge_tree); // Python's default, accepted and ignored
/// ```
#[derive(Clone)]
pub struct DocumentConfig {
    /// Stored on the document; the base URL for parsing and `urljoin`, and the
    /// adaptive-storage domain key. Python's default is `""`.
    pub url: String,
    /// Parser encoding, also used when serialising. Python's default is
    /// `"utf-8"`; the empty string here means the same.
    pub encoding: String,
    /// `remove_comments = !keep_comments`.
    pub keep_comments: bool,
    /// `strip_cdata = !keep_cdata`.
    pub keep_cdata: bool,
    /// libxml2's memory-limit flag. **Accepted and ignored** — html5ever has no
    /// analogue (`ARCHITECTURE.md` §3). Kept so fetcher argument lists round-trip.
    pub huge_tree: bool,
    /// The master switch for adaptive relocation. When `false`, `adaptive` /
    /// `auto_save` on `css`/`xpath` are ignored *with a warning*, and
    /// `save`/`retrieve` fail with [`AdaptiveDisabled`](crate::error::ParseError::AdaptiveDisabled).
    pub adaptive: bool,
    /// Overrides [`DocumentConfig::url`] as the adaptive-storage partition key.
    ///
    /// `Response.__init__` pops this out of the selector config and passes it as
    /// `url`; an empty string means "not set".
    pub adaptive_domain: Option<String>,
    /// An already-constructed storage backend, shared by every node of the tree.
    ///
    /// Replaces Python's `_storage=` plus the `storage=` / `storage_args=` pair
    /// and their two `ValueError` validations (`ARCHITECTURE.md` §4c).
    #[cfg(feature = "adaptive")]
    pub storage: Option<Arc<dyn StorageSystem>>,
    /// Keeps the struct non-exhaustively constructible on both feature sets.
    #[cfg(not(feature = "adaptive"))]
    #[doc(hidden)]
    pub storage: Option<Arc<()>>,
}

impl fmt::Debug for DocumentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentConfig")
            .field("url", &self.url)
            .field("encoding", &self.encoding)
            .field("keep_comments", &self.keep_comments)
            .field("keep_cdata", &self.keep_cdata)
            .field("huge_tree", &self.huge_tree)
            .field("adaptive", &self.adaptive)
            .field("adaptive_domain", &self.adaptive_domain)
            .field("storage", &self.storage.is_some())
            .finish()
    }
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentConfig {
    /// Python's defaults: no URL, UTF-8, comments and CDATA stripped,
    /// `huge_tree=True`, adaptive off.
    #[must_use]
    pub fn new() -> Self {
        Self {
            url: String::new(),
            encoding: "utf-8".to_owned(),
            keep_comments: false,
            keep_cdata: false,
            huge_tree: true,
            adaptive: false,
            adaptive_domain: None,
            storage: None,
        }
    }

    /// Set [`DocumentConfig::url`].
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Set [`DocumentConfig::encoding`].
    #[must_use]
    pub fn encoding(mut self, encoding: impl Into<String>) -> Self {
        self.encoding = encoding.into();
        self
    }

    /// Set [`DocumentConfig::keep_comments`].
    #[must_use]
    pub fn keep_comments(mut self, keep: bool) -> Self {
        self.keep_comments = keep;
        self
    }

    /// Set [`DocumentConfig::keep_cdata`].
    #[must_use]
    pub fn keep_cdata(mut self, keep: bool) -> Self {
        self.keep_cdata = keep;
        self
    }

    /// Set [`DocumentConfig::huge_tree`] (accepted and ignored).
    #[must_use]
    pub fn huge_tree(mut self, huge: bool) -> Self {
        self.huge_tree = huge;
        self
    }

    /// Set [`DocumentConfig::adaptive`].
    #[must_use]
    pub fn adaptive(mut self, adaptive: bool) -> Self {
        self.adaptive = adaptive;
        self
    }

    /// Set [`DocumentConfig::adaptive_domain`]. An empty string clears it,
    /// exactly like Python's falsy check.
    #[must_use]
    pub fn adaptive_domain(mut self, domain: impl Into<String>) -> Self {
        let domain = domain.into();
        self.adaptive_domain = (!domain.is_empty()).then_some(domain);
        self
    }

    /// Attach a storage backend and switch [`DocumentConfig::adaptive`] on.
    #[cfg(feature = "adaptive")]
    #[must_use]
    pub fn storage(mut self, storage: Arc<dyn StorageSystem>) -> Self {
        self.storage = Some(storage);
        self.adaptive = true;
        self
    }

    /// The URL adaptive storage should be partitioned by:
    /// [`DocumentConfig::adaptive_domain`] when set, else
    /// [`DocumentConfig::url`].
    #[must_use]
    pub fn effective_url(&self) -> &str {
        self.adaptive_domain.as_deref().unwrap_or(&self.url)
    }
}

/// Keyword arguments of `Selector.css`.
///
/// ```
/// use browser_parser::config::CssOpts;
///
/// let o = CssOpts::default();
/// assert_eq!(o.percentage, 40);
/// assert!(!o.adaptive && !o.auto_save && o.identifier.is_none());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssOpts {
    /// The adaptive-storage key. `None` means "use the selector itself"
    /// (Python's `identifier or selector`).
    pub identifier: Option<String>,
    /// Try to relocate a previously saved element when the selector matches
    /// nothing.
    pub adaptive: bool,
    /// Save the **first** matched element under `identifier`.
    pub auto_save: bool,
    /// The relocation threshold in percent.
    pub percentage: u8,
}

impl Default for CssOpts {
    fn default() -> Self {
        Self {
            identifier: None,
            adaptive: false,
            auto_save: false,
            percentage: DEFAULT_PERCENTAGE,
        }
    }
}

impl CssOpts {
    /// Python's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`CssOpts::identifier`].
    #[must_use]
    pub fn identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Set [`CssOpts::adaptive`].
    #[must_use]
    pub fn adaptive(mut self, adaptive: bool) -> Self {
        self.adaptive = adaptive;
        self
    }

    /// Set [`CssOpts::auto_save`].
    #[must_use]
    pub fn auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = auto_save;
        self
    }

    /// Set [`CssOpts::percentage`].
    #[must_use]
    pub fn percentage(mut self, percentage: u8) -> Self {
        self.percentage = percentage;
        self
    }
}

/// Keyword arguments of `Selector.get_all_text`.
///
/// ```
/// use browser_parser::config::AllTextOpts;
///
/// let o = AllTextOpts::default();
/// assert_eq!(o.separator, "\n");
/// assert_eq!(o.ignore_tags, ["script", "style"]);
/// assert!(!o.strip && o.valid_values);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllTextOpts {
    /// Joined between the collected strings.
    pub separator: String,
    /// Strip each string before joining.
    pub strip: bool,
    /// Element names whose whole subtree contributes no text.
    pub ignore_tags: Vec<String>,
    /// Drop strings that are empty or whitespace-only.
    pub valid_values: bool,
}

impl Default for AllTextOpts {
    fn default() -> Self {
        Self {
            separator: DEFAULT_SEPARATOR.to_owned(),
            strip: false,
            ignore_tags: DEFAULT_IGNORE_TAGS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            valid_values: true,
        }
    }
}

impl AllTextOpts {
    /// Python's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`AllTextOpts::separator`].
    #[must_use]
    pub fn separator(mut self, separator: impl Into<String>) -> Self {
        self.separator = separator.into();
        self
    }

    /// Set [`AllTextOpts::strip`].
    #[must_use]
    pub fn strip(mut self, strip: bool) -> Self {
        self.strip = strip;
        self
    }

    /// Replace [`AllTextOpts::ignore_tags`]. An empty list disables the pass
    /// entirely, exactly like Python's `if ignore_tags:` guard.
    #[must_use]
    pub fn ignore_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ignore_tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Set [`AllTextOpts::valid_values`].
    #[must_use]
    pub fn valid_values(mut self, valid: bool) -> Self {
        self.valid_values = valid;
        self
    }
}

/// Keyword arguments of `Selector.find_similar`.
///
/// ```
/// use browser_parser::config::SimilarOpts;
///
/// let o = SimilarOpts::default();
/// assert_eq!(o.threshold, 0.2);
/// assert_eq!(o.ignore_attributes, ["href", "src"]);
/// assert!(!o.match_text);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarOpts {
    /// The rounded score a candidate must reach, in `0.0..=1.0`.
    pub threshold: f64,
    /// Attribute names excluded from the comparison. **An empty list turns the
    /// filtering off on both sides** — Python's `if ignore_attributes:` picks
    /// the raw `attrib` map instead, which is not the same as filtering nothing
    /// out of a copy (`parser.md` §3.7).
    pub ignore_attributes: Vec<String>,
    /// Also compare the elements' direct text.
    pub match_text: bool,
}

impl Default for SimilarOpts {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_SIMILARITY_THRESHOLD,
            ignore_attributes: DEFAULT_IGNORE_ATTRIBUTES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            match_text: false,
        }
    }
}

impl SimilarOpts {
    /// Python's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`SimilarOpts::threshold`].
    #[must_use]
    pub fn threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// Replace [`SimilarOpts::ignore_attributes`].
    #[must_use]
    pub fn ignore_attributes<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ignore_attributes = names.into_iter().map(Into::into).collect();
        self
    }

    /// Set [`SimilarOpts::match_text`].
    #[must_use]
    pub fn match_text(mut self, match_text: bool) -> Self {
        self.match_text = match_text;
        self
    }
}

/// Keyword arguments of `Selector.find_by_text`.
///
/// ```
/// use browser_parser::config::ByTextOpts;
///
/// let o = ByTextOpts::default();
/// assert!(o.first_match && o.clean_match);
/// assert!(!o.partial && !o.case_sensitive);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByTextOpts {
    /// Stop at the first hit. Python then returns a bare `Selector`; here the
    /// result stays a [`Nodes`](crate::nodes::Nodes) of length 0 or 1 — see
    /// [`crate::text`] for why.
    pub first_match: bool,
    /// Substring instead of equality.
    pub partial: bool,
    /// Compare case-sensitively.
    pub case_sensitive: bool,
    /// Run `TextHandler.clean()` over the candidate's text before comparing.
    pub clean_match: bool,
}

impl Default for ByTextOpts {
    fn default() -> Self {
        Self {
            first_match: true,
            partial: false,
            case_sensitive: false,
            clean_match: true,
        }
    }
}

impl ByTextOpts {
    /// Python's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`ByTextOpts::first_match`].
    #[must_use]
    pub fn first_match(mut self, first: bool) -> Self {
        self.first_match = first;
        self
    }

    /// Set [`ByTextOpts::partial`].
    #[must_use]
    pub fn partial(mut self, partial: bool) -> Self {
        self.partial = partial;
        self
    }

    /// Set [`ByTextOpts::case_sensitive`].
    #[must_use]
    pub fn case_sensitive(mut self, sensitive: bool) -> Self {
        self.case_sensitive = sensitive;
        self
    }

    /// Set [`ByTextOpts::clean_match`].
    #[must_use]
    pub fn clean_match(mut self, clean: bool) -> Self {
        self.clean_match = clean;
        self
    }
}

/// Keyword arguments of `Selector.find_by_regex`.
///
/// Identical to [`ByTextOpts`] minus `partial`, which a regex does not need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByRegexOpts {
    /// Stop at the first hit.
    pub first_match: bool,
    /// Compile the pattern case-sensitively. Ignored for an already-compiled
    /// pattern, exactly as in Python.
    pub case_sensitive: bool,
    /// Run `TextHandler.clean()` over the candidate's text before matching.
    pub clean_match: bool,
}

impl Default for ByRegexOpts {
    fn default() -> Self {
        Self {
            first_match: true,
            case_sensitive: false,
            clean_match: true,
        }
    }
}

impl ByRegexOpts {
    /// Python's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`ByRegexOpts::first_match`].
    #[must_use]
    pub fn first_match(mut self, first: bool) -> Self {
        self.first_match = first;
        self
    }

    /// Set [`ByRegexOpts::case_sensitive`].
    #[must_use]
    pub fn case_sensitive(mut self, sensitive: bool) -> Self {
        self.case_sensitive = sensitive;
        self
    }

    /// Set [`ByRegexOpts::clean_match`].
    #[must_use]
    pub fn clean_match(mut self, clean: bool) -> Self {
        self.clean_match = clean;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_config_defaults_match_python() {
        let config = DocumentConfig::new();
        assert_eq!(config.url, "");
        assert_eq!(config.encoding, "utf-8");
        assert!(!config.keep_comments);
        assert!(!config.keep_cdata);
        assert!(config.huge_tree);
        assert!(!config.adaptive);
        assert_eq!(config.adaptive_domain, None);
        assert_eq!(config.effective_url(), "");
        assert_eq!(
            format!("{:?}", DocumentConfig::default()),
            format!("{config:?}"),
            "`Default` must be `new()`, not the derived all-zeroes"
        );
    }

    #[test]
    fn adaptive_domain_replaces_the_url_as_the_storage_key() {
        let config = DocumentConfig::new()
            .url("https://web.archive.org/web/1/https://stackoverflow.com")
            .adaptive_domain("stackoverflow.com");
        assert_eq!(config.effective_url(), "stackoverflow.com");
        assert_eq!(
            DocumentConfig::new()
                .url("a.com")
                .adaptive_domain("")
                .effective_url(),
            "a.com",
            "an empty adaptive_domain is falsy in Python and must not win"
        );
    }

    #[test]
    fn option_structs_carry_the_python_defaults() {
        assert_eq!(CssOpts::default().percentage, DEFAULT_PERCENTAGE);
        assert!(AllTextOpts::default().valid_values);
        assert_eq!(AllTextOpts::default().separator, "\n");
        assert_eq!(SimilarOpts::default().threshold, 0.2);
        assert!(ByTextOpts::default().first_match);
        assert!(ByTextOpts::default().clean_match);
        assert!(!ByTextOpts::default().case_sensitive);
        assert!(ByRegexOpts::default().first_match);
    }

    #[test]
    fn builders_are_chainable() {
        let o = CssOpts::new()
            .identifier("#p1")
            .adaptive(true)
            .auto_save(true)
            .percentage(95);
        assert_eq!(o.identifier.as_deref(), Some("#p1"));
        assert!(o.adaptive && o.auto_save);
        assert_eq!(o.percentage, 95);

        let t = AllTextOpts::new()
            .separator(" | ")
            .strip(true)
            .ignore_tags(["table"])
            .valid_values(false);
        assert_eq!(t.separator, " | ");
        assert_eq!(t.ignore_tags, ["table"]);
        assert!(t.strip && !t.valid_values);

        let s = SimilarOpts::new()
            .threshold(0.9)
            .ignore_attributes(Vec::<String>::new())
            .match_text(true);
        assert!(s.ignore_attributes.is_empty() && s.match_text);
        assert_eq!(s.threshold, 0.9);
    }
}
