//! [`FindQuery`] — `find_all(*args, **kwargs)` as a typed builder, with the
//! **fixed** evaluation order and the CSS-value escaping rules.
//!
//! `find_all` is the hardest signature in the reference implementation to translate: it takes tag
//! names, iterables of tag names, attribute dicts, compiled patterns and
//! callables, in any order, plus keyword arguments for attributes. Python
//! classifies the arguments and then evaluates in a **fixed** order regardless
//! of how they were passed (`parser.md` §3.6):
//!
//! 1. tags × attributes are compiled into one comma-separated CSS selector and
//!    run through [`Node::css`](crate::node::Node::css);
//! 2. every regex is applied to each survivor's **direct** text;
//! 3. every predicate is applied to what is left.
//!
//! When no tag/attribute selector could be built — only a regex or a predicate
//! was supplied — the search space is `below_elements`, i.e. every descendant
//! element, self excluded.
//!
//! ```
//! use browser_parser::document::Document;
//! use browser_parser::find::FindQuery;
//!
//! let doc = Document::parse(
//!     "<div class='hidden stock'>In stock: 5</div>\
//!      <div class='stock'>Out of stock</div>",
//! );
//!
//! // `class` is a token list, so this matches `class="hidden stock"` too.
//! assert_eq!(doc.find_all(&FindQuery::new().tag("div").class("stock")).unwrap().len(), 2);
//! // Regexes match the element's DIRECT text.
//! let query = FindQuery::new().tag("div").regex(r"In stock: \d+");
//! assert_eq!(doc.find_all(&query).unwrap().len(), 1);
//! ```
//!
//! # Two escaping rules, on purpose
//!
//! Attribute **keys are not escaped**, so a caller can smuggle a CSS operator
//! into the key: `.attr("href*", "/p/")` becomes `[href*="/p/"]`. Attribute
//! **values are escaped** by [`escape_css_string`]. Backslashes are left alone
//! deliberately — cssselect's unescaper drops them either way.
//!
//! # Two Python arguments with no Rust analogue
//!
//! * `{"class_": "class", "for_": "for"}` — Rust has no reserved-word problem,
//!   so `.attr("class", …)` and `.class(…)` both work and the mapping is gone.
//! * `inspect.signature(arg).parameters` arity checking — a Rust closure's
//!   signature is checked by the compiler (`ARCHITECTURE.md` §11.6).

use indexmap::{IndexMap, IndexSet};
use browser_core::text::{ReOptions, RegexSpec, TextExt};

use crate::error::{ParseError, Result};
use crate::node::Node;
use crate::nodes::Nodes;

/// `_escape_css_string`: the characters that cannot appear literally inside a
/// CSS double-quoted string.
///
/// Line breaks have to be written as hexadecimal escapes, and the trailing
/// space is what ends the escape. Backslashes are **not** escaped, matching
/// Python.
///
/// ```
/// use browser_parser::find::escape_css_string;
///
/// assert_eq!(escape_css_string(r#"say "hi""#), r#"say \"hi\""#);
/// assert_eq!(escape_css_string("one\ntwo"), r"one\A two");
/// assert_eq!(escape_css_string("a\rb\u{c}c"), r"a\D b\C c");
/// assert_eq!(escape_css_string(r"back\slash"), r"back\slash");
/// ```
#[must_use]
pub fn escape_css_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\A "),
            '\r' => out.push_str("\\D "),
            '\u{c}' => out.push_str("\\C "),
            other => out.push(other),
        }
    }
    out
}

/// A boxed predicate, higher-ranked so it accepts nodes of any lifetime.
type Predicate<'f> = Box<dyn for<'n> Fn(&Node<'n>) -> bool + Send + Sync + 'f>;

/// The typed replacement for `find_all(*args, **kwargs)`.
///
/// Build it with the chainable setters; the evaluation order is fixed by
/// [`find_all`], not by the order you call them in.
#[derive(Default)]
pub struct FindQuery<'f> {
    tags: IndexSet<String>,
    attributes: IndexMap<String, String>,
    patterns: Vec<RegexSpec<'f>>,
    predicates: Vec<Predicate<'f>>,
}

impl std::fmt::Debug for FindQuery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FindQuery")
            .field("tags", &self.tags)
            .field("attributes", &self.attributes)
            .field("patterns", &self.patterns.len())
            .field("predicates", &self.predicates.len())
            .finish()
    }
}

impl<'f> FindQuery<'f> {
    /// An empty query. Executing it is Python's argument-less `find_all()` and
    /// fails with [`ParseError::EmptyFindQuery`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one tag name.
    ///
    /// Tags are kept in an [`IndexSet`], so the generated sub-selectors come out
    /// in the order they were added. Python iterates a `set`, whose order is
    /// unspecified; making it deterministic is a strict improvement
    /// (`ARCHITECTURE.md` §11.7) — no test depends on the Python order.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.insert(tag.into());
        self
    }

    /// Add several tag names — Python's list/tuple/set argument.
    #[must_use]
    pub fn tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags.extend(tags.into_iter().map(Into::into));
        self
    }

    /// Add one attribute filter.
    ///
    /// The key is emitted **unescaped**, so CSS operators work:
    /// `.attr("href*", "/p/")` → `[href*="/p/"]`, `.attr("href$", "x")` →
    /// `[href$="x"]`. A later call with the same key wins, like `dict.update`.
    #[must_use]
    pub fn attr(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Add several attribute filters — Python's `dict` argument.
    #[must_use]
    pub fn attrs<I, K, V>(mut self, attributes: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (key, value) in attributes {
            self.attributes.insert(key.into(), value.into());
        }
        self
    }

    /// Filter on the `class` token list — Python's `class_=`.
    ///
    /// Every whitespace-separated token becomes its own `[class~="…"]`
    /// predicate, so `class("hidden stock")` matches `class="stock hidden"`. A
    /// blank value falls through to the exact match `[class=""]`.
    #[must_use]
    pub fn class(self, value: impl Into<String>) -> Self {
        self.attr("class", value)
    }

    /// Filter on `id`.
    #[must_use]
    pub fn id(self, value: impl Into<String>) -> Self {
        self.attr("id", value)
    }

    /// Add a regular expression, matched against each candidate's **direct**
    /// text with `check_match=True`.
    #[must_use]
    pub fn regex(mut self, pattern: &'f str) -> Self {
        self.patterns.push(RegexSpec::Pattern(pattern));
        self
    }

    /// Add an already-compiled pattern, or a pattern built elsewhere.
    #[must_use]
    pub fn regex_spec(mut self, pattern: RegexSpec<'f>) -> Self {
        self.patterns.push(pattern);
        self
    }

    /// Add a predicate — Python's callable filter.
    #[must_use]
    pub fn predicate<F>(mut self, predicate: F) -> Self
    where
        F: for<'n> Fn(&Node<'n>) -> bool + Send + Sync + 'f,
    {
        self.predicates.push(Box::new(predicate));
        self
    }

    /// Whether this query carries no filter at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
            && self.attributes.is_empty()
            && self.patterns.is_empty()
            && self.predicates.is_empty()
    }

    /// The tag names, in insertion order.
    #[must_use]
    pub fn tag_names(&self) -> &IndexSet<String> {
        &self.tags
    }

    /// The attribute filters, in insertion order.
    #[must_use]
    pub fn attribute_filters(&self) -> &IndexMap<String, String> {
        &self.attributes
    }

    /// The CSS selector this query compiles to, or `None` when it has no
    /// tag/attribute part and the search space is `below_elements` instead.
    ///
    /// ```
    /// use browser_parser::find::FindQuery;
    ///
    /// assert_eq!(FindQuery::new().tag("div").selector().as_deref(), Some("div"));
    /// assert_eq!(
    ///     FindQuery::new().tag("div").class("a b").selector().as_deref(),
    ///     Some(r#"div[class~="a"][class~="b"]"#)
    /// );
    /// assert_eq!(
    ///     FindQuery::new().attr("href*", "/p/").selector().as_deref(),
    ///     Some(r#"*[href*="/p/"]"#)
    /// );
    /// // `*` alone is dropped, which is what sends the query to below_elements.
    /// assert_eq!(FindQuery::new().tag("*").selector(), None);
    /// ```
    #[must_use]
    pub fn selector(&self) -> Option<String> {
        // `tags = tags or set("*")`
        let owned_star;
        let tags: &IndexSet<String> = if self.tags.is_empty() {
            owned_star = IndexSet::from(["*".to_owned()]);
            &owned_star
        } else {
            &self.tags
        };

        let mut selectors: Vec<String> = Vec::with_capacity(tags.len());
        for tag in tags {
            let mut selector = tag.clone();
            for (key, value) in &self.attributes {
                let tokens: Vec<&str> = value.split_whitespace().collect();
                if key == "class" && !tokens.is_empty() {
                    for token in tokens {
                        selector.push_str(&format!(r#"[class~="{}"]"#, escape_css_string(token)));
                    }
                } else {
                    selector.push_str(&format!(r#"[{}="{}"]"#, key, escape_css_string(value)));
                }
            }
            if selector != "*" {
                selectors.push(selector);
            }
        }

        (!selectors.is_empty()).then(|| selectors.join(", "))
    }
}

/// `Selector.find_all`, with the fixed evaluation order.
///
/// # Errors
///
/// [`ParseError::EmptyFindQuery`] when `query` carries no filters,
/// [`ParseError::CssSyntax`] when the generated selector does not parse, and
/// [`ParseError::Regex`] when a pattern does not compile.
pub fn find_all<'a>(node: &Node<'a>, query: &FindQuery<'_>) -> Result<Nodes<'a>> {
    // The text-node guard comes *before* the empty-query check in Python.
    if node.is_text_node() {
        return Ok(Nodes::new());
    }
    if query.is_empty() {
        return Err(ParseError::EmptyFindQuery);
    }

    let mut results = match query.selector() {
        Some(selector) => node.css(&selector)?,
        None => node.below_elements(),
    };

    for pattern in &query.patterns {
        results = filter_by_regex(results, *pattern)?;
    }
    for predicate in &query.predicates {
        results = results.filter(|candidate| predicate(candidate));
    }
    Ok(results)
}

/// `results.filter(lambda e: e.text.re(pattern, check_match=True))`.
fn filter_by_regex<'a>(nodes: Nodes<'a>, pattern: RegexSpec<'_>) -> Result<Nodes<'a>> {
    let mut kept = Nodes::new();
    for node in nodes {
        if node.text().re_check(pattern, ReOptions::default())? {
            kept.push(node);
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    const SHOP: &str = concat!(
        r#"<div class="hidden stock">In stock: 5</div>"#,
        r#"<div class="stock hidden">In stock: 3</div>"#,
        r#"<div class="stock">Out of stock</div>"#,
        r#"<span class="stock">span</span>"#,
    );

    #[test]
    fn an_empty_query_is_a_type_error() {
        let doc = Document::parse("<p>x</p>");
        let error = doc.find_all(&FindQuery::new()).expect_err("empty");
        assert!(matches!(error, ParseError::EmptyFindQuery));
        assert_eq!(
            error.to_string(),
            "You have to pass something to search with, like tag name(s), tag attributes, or both."
        );
    }

    #[test]
    fn a_text_node_returns_empty_before_the_empty_query_check() {
        let doc = Document::parse("<p>x</p>");
        let text = doc.css("p::text").expect("selects")[0].clone();
        assert!(text
            .find_all(&FindQuery::new())
            .expect("guarded")
            .is_empty());
    }

    #[test]
    fn class_is_a_token_list_and_blanks_fall_through_to_exact_match() {
        let doc = Document::parse(SHOP);
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("div").class("stock"))
                .expect("finds")
                .len(),
            3
        );
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("div").class("hidden"))
                .expect("finds")
                .len(),
            2
        );
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("div").class("hidden stock"))
                .expect("finds")
                .len(),
            2
        );
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("div").class("stock hidden"))
                .expect("finds")
                .len(),
            2,
            "`~=` predicates AND together, order-insensitively"
        );
        assert!(doc
            .find_all(&FindQuery::new().tag("div").class("hidden nonexistent"))
            .expect("finds")
            .is_empty());

        let blank =
            Document::parse(r#"<div class="">empty</div><div class="a">a</div><div>none</div>"#);
        assert_eq!(
            blank
                .find_all(&FindQuery::new().tag("div").class(""))
                .expect("finds")
                .texts(),
            ["empty"],
            r#"a blank class becomes [class=""]"#
        );
        assert!(
            blank
                .find_all(&FindQuery::new().tag("div").class("   "))
                .expect("finds")
                .is_empty(),
            "whitespace-only matches nothing at all"
        );
    }

    #[test]
    fn attribute_keys_are_not_escaped_so_operators_survive() {
        let doc = Document::parse(
            r#"<a href="/p/1">one</a><a href="/q/2">two</a><a href="x/Einstein">three</a>"#,
        );
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("a").attr("href*", "/p/"))
                .expect("finds")
                .texts(),
            ["one"]
        );
        assert_eq!(
            doc.find_all(&FindQuery::new().tag("a").attr("href$", "Einstein"))
                .expect("finds")
                .texts(),
            ["three"]
        );
    }

    #[test]
    fn attribute_values_are_escaped() {
        let doc = Document::parse("<a title='say \"hi\"'>x</a><a title=\"one\ntwo\">y</a>");
        assert_eq!(
            doc.find(&FindQuery::new().tag("a").attr("title", "say \"hi\""))
                .expect("finds")
                .expect("one")
                .text(),
            "x"
        );
        assert_eq!(
            doc.find(&FindQuery::new().tag("a").attr("title", "one\ntwo"))
                .expect("finds")
                .expect("one")
                .text(),
            "y"
        );
    }

    #[test]
    fn evaluation_order_is_fixed_regardless_of_builder_order() {
        let doc = Document::parse(SHOP);
        let a = FindQuery::new()
            .regex(r"In stock: \d+")
            .predicate(|n| n.has_class("hidden"))
            .tag("div");
        let b = FindQuery::new()
            .tag("div")
            .regex(r"In stock: \d+")
            .predicate(|n| n.has_class("hidden"));
        assert_eq!(
            doc.find_all(&a).expect("finds").texts(),
            doc.find_all(&b).expect("finds").texts()
        );
        assert_eq!(doc.find_all(&a).expect("finds").len(), 2);
    }

    #[test]
    fn a_regex_only_query_searches_below_elements() {
        let doc = Document::parse("<table><tr><td>Cell 1</td><td>Cell 2</td></tr></table>");
        let hits = doc
            .find_all(&FindQuery::new().regex(r"Cell \d+"))
            .expect("finds");
        assert_eq!(hits.texts(), ["Cell 1", "Cell 2"]);
    }

    #[test]
    fn several_tags_produce_one_comma_group() {
        let doc = Document::parse(SHOP);
        let query = FindQuery::new().tags(["div", "span"]).class("stock");
        assert_eq!(
            query.selector().as_deref(),
            Some(r#"div[class~="stock"], span[class~="stock"]"#)
        );
        assert_eq!(doc.find_all(&query).expect("finds").len(), 4);
    }

    #[test]
    fn find_returns_the_first_result_or_none() {
        let doc = Document::parse(SHOP);
        assert_eq!(
            doc.find(&FindQuery::new().tag("div"))
                .expect("finds")
                .expect("first")
                .text(),
            "In stock: 5"
        );
        assert!(doc
            .find(&FindQuery::new().tag("article"))
            .expect("finds")
            .is_none());
    }

    #[test]
    fn a_predicate_sees_a_real_node() {
        let doc = Document::parse(r#"<div a="1" b="2" c="3">x</div><div a="1">y</div>"#);
        let hits = doc
            .find_all(
                &FindQuery::new()
                    .tag("div")
                    .predicate(|n| n.attrib().len() > 2),
            )
            .expect("finds");
        assert_eq!(hits.texts(), ["x"]);
    }
}
