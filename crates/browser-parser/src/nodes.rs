//! [`Nodes`] — Python's `Selectors`, a `list` subclass with extras.
//!
//! `Nodes` derefs to `[Node]`, so every slice method (`len`, `is_empty`,
//! `first`, `last`, `iter`, indexing, slicing) is already there and Python's
//! `.length` / `.first` / `.last` need no special spelling. What this type adds
//! is the fan-out API: `css`, `xpath`, `re`, `re_first`, `search`, `filter`,
//! `get`, `getall` — plus the Rust-native conveniences `texts()`, `htmls()` and
//! `attrs(name)` that make `::text` / `::attr()` optional rather than mandatory
//! (`ARCHITECTURE.md` §3).
//!
//! ```
//! use browser_parser::document::Document;
//!
//! let doc = Document::parse(
//!     "<ul><li class='a'><a href='/1'>one</a></li><li><a href='/2'>two</a></li></ul>",
//! );
//! let links = doc.css("li a").unwrap();
//!
//! assert_eq!(links.len(), 2);
//! assert_eq!(links.texts(), ["one", "two"]);
//! assert_eq!(links.attrs("href"), ["/1", "/2"]);
//! assert_eq!(links.get().unwrap(), r#"<a href="/1">one</a>"#);
//! assert_eq!(links.filter(|n| n.text() == "two").len(), 1);
//! assert!(Nodes::new().first().is_none());
//! # use browser_parser::nodes::Nodes;
//! ```

use std::ops::{Deref, DerefMut};

use browser_core::text::{ReOptions, RegexSpec, TextExt, TextsExt};
use browser_selectors::Match;
use browser_xpath::eval::Variables;

use crate::config::CssOpts;
use crate::document::Document;
use crate::error::Result;
use crate::node::Node;

/// A list of [`Node`]s — Python's `Selectors`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nodes<'a>(Vec<Node<'a>>);

impl<'a> Nodes<'a> {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Wrap an existing vector.
    #[must_use]
    pub fn from_vec(nodes: Vec<Node<'a>>) -> Self {
        Self(nodes)
    }

    /// Unwrap into the backing vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<Node<'a>> {
        self.0
    }

    /// Convert a batch of CSS matches.
    #[must_use]
    pub fn from_matches(doc: &'a Document, matches: Vec<Match<'a>>) -> Self {
        Self(
            matches
                .iter()
                .map(|matched| Node::from_match(doc, matched))
                .collect(),
        )
    }

    /// Append one node.
    pub fn push(&mut self, node: Node<'a>) {
        self.0.push(node);
    }

    /// Append every node of `other` — Python's `results += …`.
    pub fn extend(&mut self, other: impl IntoIterator<Item = Node<'a>>) {
        self.0.extend(other);
    }

    /// Python's `Selectors.length`, spelled the Rust way by [`Deref`]; kept as a
    /// name for callers porting Python line by line.
    #[must_use]
    pub fn length(&self) -> usize {
        self.0.len()
    }

    // -- fan-out ----------------------------------------------------------

    /// `Selectors.css` — run the selector on every node and flatten.
    ///
    /// # Errors
    ///
    /// [`ParseError::CssSyntax`](crate::error::ParseError::CssSyntax).
    pub fn css(&self, selector: &str) -> Result<Nodes<'a>> {
        self.css_with(selector, &CssOpts::default())
    }

    /// `Selectors.css` with keyword arguments.
    ///
    /// Python's `Selectors.css` has **no** `adaptive` parameter — it always
    /// passes `False` — so [`CssOpts::adaptive`] is forced off here too.
    ///
    /// # Errors
    ///
    /// As [`Nodes::css`], plus storage errors from `auto_save`.
    pub fn css_with(&self, selector: &str, opts: &CssOpts) -> Result<Nodes<'a>> {
        let opts = CssOpts {
            adaptive: false,
            ..opts.clone()
        };
        let mut out = Nodes::new();
        for node in &self.0 {
            out.extend(node.css_with(selector, &opts)?);
        }
        Ok(out)
    }

    /// `Selectors.xpath` — run the expression on every node and flatten.
    ///
    /// # Errors
    ///
    /// [`ParseError::XPathSyntax`](crate::error::ParseError::XPathSyntax).
    pub fn xpath(&self, expression: &str) -> Result<Nodes<'a>> {
        self.xpath_with(expression, &CssOpts::default(), &Variables::new())
    }

    /// `Selectors.xpath` with keyword arguments and XPath variables.
    ///
    /// # Errors
    ///
    /// As [`Nodes::xpath`], plus storage errors from `auto_save`.
    pub fn xpath_with(
        &self,
        expression: &str,
        opts: &CssOpts,
        variables: &Variables,
    ) -> Result<Nodes<'a>> {
        let opts = CssOpts {
            adaptive: false,
            ..opts.clone()
        };
        let mut out = Nodes::new();
        for node in &self.0 {
            out.extend(node.xpath_with(expression, &opts, variables)?);
        }
        Ok(out)
    }

    /// `Selectors.re` — the pattern against each node's direct text, flattened.
    ///
    /// # Errors
    ///
    /// [`ParseError::Regex`](crate::error::ParseError::Regex).
    pub fn re(&self, pattern: RegexSpec<'_>, opts: ReOptions) -> Result<Vec<String>> {
        let texts: Vec<&str> = self.0.iter().map(Node::text).collect();
        Ok(texts.re_all(pattern, opts)?)
    }

    /// `Selectors.re_first` — the first match scanning nodes in order.
    ///
    /// # Errors
    ///
    /// As [`Nodes::re`].
    pub fn re_first(&self, pattern: RegexSpec<'_>, opts: ReOptions) -> Result<Option<String>> {
        for node in &self.0 {
            if let Some(hit) = node.text().re_first(pattern, opts)? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    /// `Selectors.search` — the first node satisfying `predicate`.
    #[must_use]
    pub fn search(&self, predicate: impl Fn(&Node<'a>) -> bool) -> Option<Node<'a>> {
        self.0.iter().find(|node| predicate(node)).cloned()
    }

    /// `Selectors.filter` — a new list, never `None`, always chainable.
    #[must_use]
    pub fn filter(&self, predicate: impl Fn(&Node<'a>) -> bool) -> Nodes<'a> {
        Self(
            self.0
                .iter()
                .filter(|node| predicate(node))
                .cloned()
                .collect(),
        )
    }

    // -- extraction -------------------------------------------------------

    /// `Selectors.get` / `extract_first` — the first node serialised.
    ///
    /// **Note the shadowing.** `Nodes` derefs to a slice, so `nodes.get(i)`
    /// would normally be the slice's index lookup; this inherent method wins,
    /// takes no index, and returns a *string*, because that is what Python's
    /// `Selectors.get()` does. Use [`Nodes::at`] for the slice lookup.
    #[must_use]
    pub fn get(&self) -> Option<String> {
        self.0.first().map(Node::get)
    }

    /// The node at `index`, or `None` — the slice lookup [`Nodes::get`] shadows.
    #[must_use]
    pub fn at(&self, index: usize) -> Option<&Node<'a>> {
        self.0.get(index)
    }

    /// `Selectors.get(default)`.
    #[must_use]
    pub fn get_or(&self, default: &str) -> String {
        self.get().unwrap_or_else(|| default.to_owned())
    }

    /// `Selectors.getall` / `extract` — every node serialised.
    #[must_use]
    pub fn getall(&self) -> Vec<String> {
        self.0.iter().map(Node::get).collect()
    }

    /// Every node's **direct** text — the Rust-native alternative to appending
    /// `::text` to the selector.
    #[must_use]
    pub fn texts(&self) -> Vec<String> {
        self.0.iter().map(|node| node.text().to_owned()).collect()
    }

    /// Every node's whole text content, `get_all_text` per node.
    #[must_use]
    pub fn all_texts(&self) -> Vec<String> {
        self.0.iter().map(Node::get_all_text).collect()
    }

    /// Every node's outer HTML — the same list [`Nodes::getall`] produces.
    #[must_use]
    pub fn htmls(&self) -> Vec<String> {
        self.getall()
    }

    /// One attribute from every node, skipping the nodes that lack it — the
    /// Rust-native alternative to `::attr(name)`.
    #[must_use]
    pub fn attrs(&self, name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|node| node.attr(name).map(ToOwned::to_owned))
            .collect()
    }
}

impl<'a> Deref for Nodes<'a> {
    type Target = [Node<'a>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Nodes<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'a> IntoIterator for Nodes<'a> {
    type Item = Node<'a>;
    type IntoIter = std::vec::IntoIter<Node<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'n, 'a> IntoIterator for &'n Nodes<'a> {
    type Item = &'n Node<'a>;
    type IntoIter = std::slice::Iter<'n, Node<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> FromIterator<Node<'a>> for Nodes<'a> {
    fn from_iter<T: IntoIterator<Item = Node<'a>>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'a> From<Vec<Node<'a>>> for Nodes<'a> {
    fn from(nodes: Vec<Node<'a>>) -> Self {
        Self(nodes)
    }
}

impl<'a> From<Nodes<'a>> for Vec<Node<'a>> {
    fn from(nodes: Nodes<'a>) -> Self {
        nodes.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Document {
        Document::parse(
            "<div id='wrap'>\
             <p class='price'>10.99</p>\
             <p class='price'>8.50</p>\
             <a href='/one'>one</a><a>no href</a>\
             </div>",
        )
    }

    #[test]
    fn an_empty_list_never_panics() {
        let empty = Nodes::new();
        assert!(empty.is_empty());
        assert_eq!(empty.length(), 0);
        assert_eq!(empty.first(), None);
        assert!(empty.last().is_none());
        assert_eq!(empty.get(), None);
        assert_eq!(empty.at(0), None);
        assert_eq!(empty.get_or("fallback"), "fallback");
        assert!(empty.getall().is_empty());
        assert!(empty.filter(|_| true).is_empty());
        assert!(empty.search(|_| true).is_none());
    }

    #[test]
    fn extraction_helpers_line_up_with_python() {
        let doc = page();
        let prices = doc.css(".price").expect("selects");
        assert_eq!(prices.length(), 2);
        assert_eq!(prices.texts(), ["10.99", "8.50"]);
        assert_eq!(
            prices.getall(),
            [
                r#"<p class="price">10.99</p>"#,
                r#"<p class="price">8.50</p>"#
            ]
        );
        assert_eq!(
            prices.get().expect("first"),
            r#"<p class="price">10.99</p>"#
        );
        assert_eq!(prices.htmls(), prices.getall());
        assert_eq!(prices.all_texts(), ["10.99", "8.50"]);
    }

    #[test]
    fn attrs_skips_nodes_without_the_attribute() {
        let doc = page();
        let links = doc.css("a").expect("selects");
        assert_eq!(links.len(), 2);
        assert_eq!(links.attrs("href"), ["/one"]);
    }

    #[test]
    fn filter_and_search_are_chainable() {
        let doc = page();
        let prices = doc.css("p").expect("selects");
        let cheap = prices
            .filter(|n| n.text().starts_with('8'))
            .filter(|n| n.has_class("price"));
        assert_eq!(cheap.len(), 1);
        assert_eq!(
            prices.search(|n| n.text() == "8.50").expect("found").text(),
            "8.50"
        );
        assert!(prices.search(|n| n.text() == "nope").is_none());
    }

    #[test]
    fn fan_out_css_and_xpath_flatten() {
        let doc = Document::parse("<ul><li><b>a</b></li><li><b>b</b></li></ul>");
        let items = doc.css("li").expect("selects");
        assert_eq!(items.css("b::text").expect("selects").texts(), ["a", "b"]);
        assert_eq!(
            items.xpath("./b/text()").expect("selects").texts(),
            ["a", "b"]
        );
    }

    #[test]
    fn regex_helpers_flatten_group_results() {
        let doc = Document::parse("<p>name=Alice age=30</p><p>name=Bob age=25</p>");
        let paragraphs = doc.css("p").expect("selects");
        let all = paragraphs
            .re(
                RegexSpec::Pattern(r"name=(\w+) age=(\d+)"),
                ReOptions::default(),
            )
            .expect("matches");
        assert_eq!(all, ["Alice", "30", "Bob", "25"]);
        assert_eq!(
            paragraphs
                .re_first(RegexSpec::Pattern(r"\d+"), ReOptions::default())
                .expect("matches"),
            Some("30".to_owned())
        );
    }

    #[test]
    fn conversions_round_trip() {
        let doc = page();
        let prices = doc.css(".price").expect("selects");
        let vector: Vec<_> = prices.clone().into();
        let back: Nodes<'_> = vector.into();
        assert_eq!(prices, back);
        assert_eq!(prices.iter().count(), 2);
        assert_eq!(prices.clone().into_iter().count(), 2);
    }
}
