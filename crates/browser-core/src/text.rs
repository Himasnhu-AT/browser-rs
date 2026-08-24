//! The [`TextExt`] / [`TextsExt`] extension traits (`clean`, `sorted_chars`,
//! `json_value`, `re_all`, `re_first`, `re_check`), [`ReOptions`], [`RegexSpec`] and
//! the compiled-pattern cache.
//!
//! **Why a trait and not a newtype.** Python needs `TextHandler(str)` so that every
//! string the parser returns carries `.re()`, `.json()` and `.clean()`; twenty `str`
//! methods are overridden purely to re-wrap the result in the subclass. A blanket
//! `impl<T: AsRef<str> + ?Sized> TextExt for T` gives the same ergonomics for free,
//! so all twenty overrides simply vanish (`ARCHITECTURE.md` §1.1) — `"abc".clean(false)`
//! and `some_string.re_first(...)` just work, and the results stay ordinary `String`s.
//!
//! **Regex dialect.** Patterns are compiled with [`fancy_regex`], which delegates to
//! the linear-time `regex` engine unless the pattern uses backreferences or
//! look-around — so patterns pasted from Python keep working. It is still not
//! byte-identical to Python's `re`; that is a documented divergence.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use fancy_regex::{Regex, RegexBuilder};

use crate::entities::replace_entities;
use crate::error::RegexError;

/// The four flags every `re()`-shaped call in the reference implementation accepts, minus `check_match`
/// (which becomes [`TextExt::re_check`], because Python's return type varies with it).
///
/// The [`Default`] is Python's: `replace_entities = true`, `clean_match = false`,
/// `case_sensitive = true`.
///
/// ```
/// use browser_core::text::ReOptions;
///
/// let opts = ReOptions::default().case_sensitive(false).clean_match(true);
/// assert!(!opts.case_sensitive);
/// assert!(opts.clean_match);
/// assert!(opts.replace_entities);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReOptions {
    /// Run [`replace_entities`] over every produced match. Default `true`.
    pub replace_entities: bool,
    /// Run [`TextExt::clean`] over the *input* before matching, so the pattern can be
    /// written with single spaces. Default `false`.
    pub clean_match: bool,
    /// When `false`, compile string patterns case-insensitively. **Ignored** for an
    /// already-compiled [`RegexSpec::Compiled`], exactly as in Python. Default `true`.
    pub case_sensitive: bool,
}

impl Default for ReOptions {
    fn default() -> Self {
        Self {
            replace_entities: true,
            clean_match: false,
            case_sensitive: true,
        }
    }
}

impl ReOptions {
    /// Python's defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`ReOptions::replace_entities`].
    pub fn replace_entities(mut self, yes: bool) -> Self {
        self.replace_entities = yes;
        self
    }

    /// Set [`ReOptions::clean_match`].
    pub fn clean_match(mut self, yes: bool) -> Self {
        self.clean_match = yes;
        self
    }

    /// Set [`ReOptions::case_sensitive`].
    pub fn case_sensitive(mut self, yes: bool) -> Self {
        self.case_sensitive = yes;
        self
    }
}

/// A pattern, either as text to compile (and cache) or already compiled.
///
/// Mirrors Python's `regex: str | Pattern`. For [`RegexSpec::Compiled`],
/// [`ReOptions::case_sensitive`] is ignored — the flags baked into the pattern win.
#[derive(Clone, Copy, Debug)]
pub enum RegexSpec<'a> {
    /// Pattern text; compiled on demand and memoised.
    Pattern(&'a str),
    /// An already-compiled pattern.
    Compiled(&'a Regex),
}

impl<'a> From<&'a str> for RegexSpec<'a> {
    fn from(p: &'a str) -> Self {
        Self::Pattern(p)
    }
}

impl<'a> From<&'a String> for RegexSpec<'a> {
    fn from(p: &'a String) -> Self {
        Self::Pattern(p.as_str())
    }
}

impl<'a> From<&'a Regex> for RegexSpec<'a> {
    fn from(p: &'a Regex) -> Self {
        Self::Compiled(p)
    }
}

/// Upper bound on the pattern cache. Python gets memoisation for free from `re`'s
/// internal cache (512 entries, cleared wholesale when full); this mirrors it.
const PATTERN_CACHE_CAPACITY: usize = 512;

type PatternCache = Mutex<HashMap<(String, bool), Arc<Regex>>>;

fn pattern_cache() -> &'static PatternCache {
    static CACHE: OnceLock<PatternCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Compile `pattern`, reusing a previously compiled copy when possible.
///
/// The cache is keyed on `(pattern, case_sensitive)` and, like CPython's `re` cache,
/// is emptied wholesale rather than evicted one entry at a time when it fills up.
pub fn compile(pattern: &str, case_sensitive: bool) -> Result<Arc<Regex>, RegexError> {
    let key = (pattern.to_owned(), case_sensitive);
    // A poisoned cache is recoverable: the map is plain data and a panic mid-insert
    // cannot leave it inconsistent.
    {
        let cache = pattern_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(re) = cache.get(&key) {
            return Ok(Arc::clone(re));
        }
    }
    let compiled = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| RegexError::compile(pattern, e))?;
    let compiled = Arc::new(compiled);
    {
        let mut cache = pattern_cache().lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= PATTERN_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&compiled));
    }
    Ok(compiled)
}

/// Resolve a [`RegexSpec`] to something borrowable as a [`Regex`].
enum Resolved<'a> {
    Owned(Arc<Regex>),
    Borrowed(&'a Regex),
}

impl Resolved<'_> {
    fn get(&self) -> &Regex {
        match self {
            Self::Owned(r) => r,
            Self::Borrowed(r) => r,
        }
    }
}

fn resolve<'a>(spec: RegexSpec<'a>, case_sensitive: bool) -> Result<Resolved<'a>, RegexError> {
    match spec {
        RegexSpec::Pattern(p) => Ok(Resolved::Owned(compile(p, case_sensitive)?)),
        RegexSpec::Compiled(r) => Ok(Resolved::Borrowed(r)),
    }
}

/// Python's `re.findall` *plus* the reference implementation's flattening step, in one pass.
///
/// `re.findall` returns whole matches with 0 capture groups, the single group's text
/// with exactly 1, and a tuple of all groups with 2 or more. `TextHandler.re` then
/// flattens any tuples (`all(_is_iterable(r) for r in results)` — vacuously true for
/// an empty list — followed by `itertools.chain.from_iterable`). Emitting every group
/// in order reproduces both steps at once, because tuples only ever arise from ≥2
/// groups. Non-participating groups become `""`, as in Python.
fn findall(re: &Regex, hay: &str) -> Result<Vec<String>, RegexError> {
    let groups = re.captures_len().saturating_sub(1);
    let mut out = Vec::new();
    for caps in re.captures_iter(hay) {
        let caps = caps.map_err(|e| RegexError::Runtime(e.to_string()))?;
        if groups == 0 {
            out.push(caps.get(0).map(|m| m.as_str()).unwrap_or("").to_owned());
        } else {
            for i in 1..=groups {
                out.push(caps.get(i).map(|m| m.as_str()).unwrap_or("").to_owned());
            }
        }
    }
    Ok(out)
}

/// Collapse runs of ASCII spaces. Python uses `re.compile(r" +")` — literal spaces
/// only, **not** `\s+`.
fn collapse_spaces(s: &str) -> Cow<'_, str> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(" +").expect("static pattern is valid"));
    re.replace_all(s, " ")
}

/// `reference.core.utils.clean_spaces`: tab → space, CR and LF **deleted**, runs of
/// spaces collapsed, and — unlike [`TextExt::clean`] — **no** trimming.
///
/// Used to render a compact one-line preview of a document. Note this is a *different*
/// translation table from the one [`TextExt::clean`] uses; Python defines two and they
/// are not interchangeable.
///
/// ```
/// use browser_core::text::clean_spaces;
///
/// assert_eq!(clean_spaces(" <a>\n\t<b>\r </a> "), " <a> <b> </a> ");
/// ```
pub fn clean_spaces(s: &str) -> Cow<'_, str> {
    if !s.contains(['\t', '\n', '\r']) && !s.contains("  ") {
        return Cow::Borrowed(s);
    }
    let translated: String = s
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\n' | '\r' => None,
            other => Some(other),
        })
        .collect();
    Cow::Owned(collapse_spaces(&translated).into_owned())
}

/// Everything `TextHandler` adds to `str`, as an extension trait.
///
/// Implemented for every `T: AsRef<str> + ?Sized`, so `&str`, `String`, `Cow<str>` and
/// friends all get it by importing the trait.
///
/// ```
/// use browser_core::text::{ReOptions, RegexSpec, TextExt};
///
/// assert_eq!("  Hello World  ".clean(false), "Hello World");
/// assert_eq!("dcba".sorted_chars(false), "abcd");
///
/// let prices = "Price: $10.99, Sale: $8.99";
/// let found = prices.re_all(RegexSpec::Pattern(r"\$[\d.]+"), ReOptions::default()).unwrap();
/// assert_eq!(found, vec!["$10.99", "$8.99"]);
/// ```
pub trait TextExt: AsRef<str> {
    /// `TextHandler.clean()`: tab/CR/LF each become **one space**, optionally decode
    /// HTML character references, collapse runs of ASCII spaces, then strip.
    ///
    /// Only tab, CR and LF are translated — a vertical tab or a non-breaking space
    /// survives the translate step — and only ASCII spaces are collapsed. The final
    /// `strip` is Python's argument-less `str.strip()`, i.e. all whitespace.
    ///
    /// ```
    /// use browser_core::text::TextExt;
    ///
    /// assert_eq!("Hello\t&amp;\nWorld".clean(false), "Hello &amp; World");
    /// assert_eq!("Hello\t&amp;\nWorld".clean(true), "Hello & World");
    /// ```
    fn clean(&self, remove_entities: bool) -> String {
        let translated: String = self
            .as_ref()
            .chars()
            .map(|c| match c {
                '\t' | '\r' | '\n' => ' ',
                other => other,
            })
            .collect();
        let decoded = if remove_entities {
            Cow::Owned(replace_entities(&translated))
        } else {
            Cow::Borrowed(translated.as_str())
        };
        collapse_spaces(&decoded).trim().to_owned()
    }

    /// `TextHandler.sort()`: sort the string's **characters** by Unicode code point.
    ///
    /// ```
    /// use browser_core::text::TextExt;
    ///
    /// assert_eq!("dcba".sorted_chars(false), "abcd");
    /// assert_eq!("dcba".sorted_chars(true), "dcba");
    /// ```
    fn sorted_chars(&self, reverse: bool) -> String {
        let mut chars: Vec<char> = self.as_ref().chars().collect();
        chars.sort_unstable();
        if reverse {
            chars.reverse();
        }
        chars.into_iter().collect()
    }

    /// `TextHandler.json()`: parse as JSON into a [`serde_json::Value`].
    ///
    /// Like the Python original this returns whatever the document is — object, array,
    /// string, number, boolean, or `null` — not necessarily a map.
    fn json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(self.as_ref())
    }

    /// `TextHandler.json()` with a concrete target type.
    fn json_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(self.as_ref())
    }

    /// `TextHandler.re()`: every match, with Python's `findall` group semantics.
    ///
    /// 0 capture groups yields whole matches; exactly 1 yields that group; 2 or more
    /// yield every group, flattened into one sequence — so `r"name=(\w+) age=(\d+)"`
    /// over `"name=Alice age=30 name=Bob age=25"` gives four results, not two.
    fn re_all(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<Vec<String>, RegexError> {
        let re = resolve(pat, o.case_sensitive)?;
        let input: Cow<'_, str> = if o.clean_match {
            Cow::Owned(self.clean(false))
        } else {
            Cow::Borrowed(self.as_ref())
        };
        let mut results = findall(re.get(), &input)?;
        if o.replace_entities {
            for r in &mut results {
                *r = replace_entities(r);
            }
        }
        Ok(results)
    }

    /// `TextHandler.re_first()`: the first match, or `None`.
    ///
    /// Python's `default` argument is the caller's business here — use
    /// `.re_first(..)?.unwrap_or_else(|| default)`.
    fn re_first(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<Option<String>, RegexError> {
        Ok(self.re_all(pat, o)?.into_iter().next())
    }

    /// `TextHandler.re(check_match=True)`: whether the pattern matches at all.
    ///
    /// Short-circuits before any post-processing, so [`ReOptions::replace_entities`]
    /// has no effect on the answer.
    fn re_check(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<bool, RegexError> {
        let re = resolve(pat, o.case_sensitive)?;
        let input: Cow<'_, str> = if o.clean_match {
            Cow::Owned(self.clean(false))
        } else {
            Cow::Borrowed(self.as_ref())
        };
        re.get()
            .is_match(input.as_ref())
            .map_err(|e| RegexError::Runtime(e.to_string()))
    }
}

impl<T: AsRef<str> + ?Sized> TextExt for T {}

/// What `TextHandlers` adds to a list of strings.
///
/// The parsel compatibility aliases (`get`, `getall`, `extract`, `extract_first`) are
/// dead weight in Rust and are not ported; `first_or` and the slice's own methods
/// cover them.
pub trait TextsExt {
    /// `TextHandlers.re()`: apply the pattern to every element and concatenate the
    /// results, flattened, in order.
    fn re_all(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<Vec<String>, RegexError>;

    /// `TextHandlers.re_first()`: the first match found scanning elements in order,
    /// then that element's matches in order.
    fn re_first(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<Option<String>, RegexError>;

    /// `TextHandlers.get(default)`: the first element, or `default`.
    fn first_or<'a>(&'a self, default: &'a str) -> &'a str;
}

impl<S: AsRef<str>> TextsExt for [S] {
    fn re_all(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<Vec<String>, RegexError> {
        let mut out = Vec::new();
        for item in self {
            out.extend(item.as_ref().re_all(pat, o)?);
        }
        Ok(out)
    }

    fn re_first(&self, pat: RegexSpec<'_>, o: ReOptions) -> Result<Option<String>, RegexError> {
        for item in self {
            if let Some(hit) = item.as_ref().re_first(pat, o)? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn first_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.first().map_or(default, AsRef::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ReOptions {
        ReOptions::default()
    }

    // §6.32
    #[test]
    fn clean_normalises_whitespace() {
        assert_eq!("  Hello World  ".clean(false), "Hello World");
        assert_eq!("dcba".sorted_chars(false), "abcd");
        assert_eq!("a\tb\nc\rd".clean(false), "a b c d");
        assert_eq!("a  \t  b".clean(false), "a b");
        // Only ASCII spaces collapse; a non-breaking space survives.
        assert_eq!("a\u{a0}\u{a0}b".clean(false), "a\u{a0}\u{a0}b");
        // …but `strip()` removes all whitespace, including a trailing vertical tab.
        assert_eq!("x\u{b}".clean(false), "x");
    }

    // §6.40
    #[test]
    fn clean_with_entities() {
        assert_eq!("Hello\t&amp;\nWorld".clean(true), "Hello & World");
        assert_eq!("Hello\t&amp;\nWorld".clean(false), "Hello &amp; World");
    }

    #[test]
    fn sorted_chars_is_by_code_point() {
        assert_eq!("dcba".sorted_chars(true), "dcba");
        // Code point order, so uppercase sorts before lowercase.
        assert_eq!("Hello".sorted_chars(false), "Hello");
        assert_eq!("hELLO".sorted_chars(false), "ELLOh");
        assert_eq!("".sorted_chars(false), "");
    }

    // §6.33
    #[test]
    fn re_all_finds_every_match() {
        let found = "Price: $10.99, Sale: $8.99"
            .re_all(RegexSpec::Pattern(r"\$[\d.]+"), opts())
            .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], "$10.99");
    }

    // §6.34
    #[test]
    fn case_insensitive_matching() {
        let found = "HELLO hello HeLLo"
            .re_all(RegexSpec::Pattern("hello"), opts().case_sensitive(false))
            .unwrap();
        assert_eq!(found.len(), 3);
        let found = "HELLO hello HeLLo"
            .re_all(RegexSpec::Pattern("hello"), opts())
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    // §6.35
    #[test]
    fn clean_match_normalises_the_input_first() {
        let found = " He  l  lo  "
            .re_all(
                RegexSpec::Pattern("He l lo"),
                opts().clean_match(true).case_sensitive(false),
            )
            .unwrap();
        assert_eq!(found.len(), 1);
        // Without `clean_match` the doubled spaces defeat the pattern.
        let found = " He  l  lo  "
            .re_all(RegexSpec::Pattern("He l lo"), opts().case_sensitive(false))
            .unwrap();
        assert!(found.is_empty());
    }

    // §6.36
    #[test]
    fn re_check_returns_a_bool() {
        assert!("hello world"
            .re_check(RegexSpec::Pattern("world"), opts())
            .unwrap());
        assert!(!"hello world"
            .re_check(RegexSpec::Pattern("zzz"), opts())
            .unwrap());
        assert!("HELLO"
            .re_check(RegexSpec::Pattern("hello"), opts().case_sensitive(false))
            .unwrap());
    }

    // §6.37
    #[test]
    fn replace_entities_flag_controls_decoding_of_matches() {
        let literal = "value &amp; more"
            .re_all(RegexSpec::Pattern("&amp;"), opts().replace_entities(false))
            .unwrap();
        assert_eq!(literal, vec!["&amp;"]);
        let decoded = "value &amp; more"
            .re_all(RegexSpec::Pattern("&amp;"), opts())
            .unwrap();
        assert_eq!(decoded, vec!["&"]);
    }

    // §6.38
    #[test]
    fn multi_group_patterns_flatten() {
        let found = "name=Alice age=30 name=Bob age=25"
            .re_all(RegexSpec::Pattern(r"name=(\w+) age=(\d+)"), opts())
            .unwrap();
        assert_eq!(found, vec!["Alice", "30", "Bob", "25"]);
    }

    #[test]
    fn single_group_patterns_yield_the_group_only() {
        let found = "a1 b2"
            .re_all(RegexSpec::Pattern(r"([a-z])\d"), opts())
            .unwrap();
        assert_eq!(found, vec!["a", "b"]);
    }

    #[test]
    fn non_participating_groups_become_empty_strings() {
        let found = "ac".re_all(RegexSpec::Pattern("a(b)?(c)"), opts()).unwrap();
        assert_eq!(found, vec!["", "c"]);
    }

    // §6.39
    #[test]
    fn re_first_and_its_default() {
        let hit = "a1 b2 c3"
            .re_first(RegexSpec::Pattern(r"\d"), opts())
            .unwrap();
        assert_eq!(hit.as_deref(), Some("1"));
        let miss = "no digits here"
            .re_first(RegexSpec::Pattern(r"\d+"), opts())
            .unwrap();
        assert_eq!(miss.unwrap_or_else(|| "N/A".into()), "N/A");
    }

    #[test]
    fn no_match_yields_an_empty_vec() {
        let found = "abc".re_all(RegexSpec::Pattern(r"\d"), opts()).unwrap();
        assert!(found.is_empty());
    }

    // §6.41
    #[test]
    fn json_parsing() {
        let v = r#"{"key": "value", "num": 42}"#.json_value().unwrap();
        assert_eq!(v["num"], 42);
        assert!("not json".json_value().is_err());
        assert!("null".json_value().unwrap().is_null());
        assert_eq!("[1,2]".json_value().unwrap().as_array().unwrap().len(), 2);

        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Cfg {
            num: u8,
        }
        let cfg: Cfg = r#"{"num": 42}"#.json_as().unwrap();
        assert_eq!(cfg, Cfg { num: 42 });
    }

    // §6.42–43
    #[test]
    fn texts_ext_flattens_across_elements() {
        let handlers = ["a1 a2".to_string(), "b3 b4".to_string()];
        let found = handlers.re_all(RegexSpec::Pattern(r"\d"), opts()).unwrap();
        assert_eq!(found, vec!["1", "2", "3", "4"]);

        let empty: Vec<String> = Vec::new();
        assert!(empty
            .re_all(RegexSpec::Pattern(r"\d"), opts())
            .unwrap()
            .is_empty());
        assert!(handlers
            .re_all(RegexSpec::Pattern("zzz"), opts())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn texts_ext_re_first_scans_elements_in_order() {
        let handlers = ["no digits".to_string(), "b3 b4".to_string()];
        let hit = handlers
            .re_first(RegexSpec::Pattern(r"\d"), opts())
            .unwrap();
        assert_eq!(hit.as_deref(), Some("3"));
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            empty.re_first(RegexSpec::Pattern(r"\d"), opts()).unwrap(),
            None
        );
    }

    #[test]
    fn texts_ext_first_or() {
        let handlers = ["first".to_string(), "second".to_string()];
        assert_eq!(handlers.first_or("default"), "first");
        let empty: Vec<String> = Vec::new();
        assert_eq!(empty.first_or("default"), "default");
    }

    #[test]
    fn precompiled_patterns_ignore_case_sensitive() {
        let re = Regex::new("(?i)hello").unwrap();
        // `case_sensitive: true` is ignored for a compiled pattern, as in Python.
        let found = "HELLO".re_all(RegexSpec::Compiled(&re), opts()).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn pattern_cache_returns_the_same_compiled_regex() {
        let a = compile(r"\d+cachecheck", true).unwrap();
        let b = compile(r"\d+cachecheck", true).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // A different case-sensitivity is a different cache entry.
        let c = compile(r"\d+cachecheck", false).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn invalid_patterns_report_the_pattern_text() {
        let err = "x".re_all(RegexSpec::Pattern("("), opts()).unwrap_err();
        assert!(err.to_string().contains("invalid regular expression `(`"));
    }

    #[test]
    fn lookaround_and_backreferences_still_compile() {
        let found = "foo1 bar2"
            .re_all(RegexSpec::Pattern(r"\w+(?=\d)"), opts())
            .unwrap();
        assert_eq!(found, vec!["foo", "bar"]);
        let found = "abab cdcd"
            .re_all(RegexSpec::Pattern(r"(ab)\1"), opts())
            .unwrap();
        assert_eq!(found, vec!["ab"]);
    }

    #[test]
    fn clean_spaces_deletes_newlines_and_does_not_trim() {
        assert_eq!(clean_spaces(" <a>\n\t<b>\r </a> "), " <a> <b> </a> ");
        assert_eq!(clean_spaces("no change"), "no change");
        assert_eq!(clean_spaces("  lead"), " lead");
        assert!(matches!(clean_spaces("plain"), Cow::Borrowed(_)));
    }
}
