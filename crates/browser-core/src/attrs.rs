//! [`AttributeMap`]: an insertion-ordered, read-only attribute view with
//! [`AttributeMap::search_values`] and [`AttributeMap::json_string`].
//!
//! The Python original, `AttributesHandler`, is a `collections.abc.Mapping` backed by a
//! `types.MappingProxyType`, i.e. a *read-only view*: there is no `__setitem__`, and
//! `attrs["id"] = "x"` raises `TypeError`. Rust gets that for free — the map exposes
//! only `&self` accessors and no mutator, so the type is immutable once built.
//!
//! Two behaviours are load-bearing and easy to lose:
//!
//! * **Iteration order is insertion order**, i.e. document order of the attributes.
//!   That is why the backing store is an [`IndexMap`] and not a `HashMap`.
//! * **`search_values` is case-sensitive in both modes.** `search_values("MAIN", false)`
//!   finds nothing when the value is `"main"`; a test pins it.
//!
//! `AttributesHandler` does no entity decoding of its own — the HTML parser has already
//! decoded attribute values by the time they get here.

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// An element's attributes: ordered, read-only, string-to-string.
///
/// ```
/// use browser_core::attrs::AttributeMap;
///
/// let attrs: AttributeMap = [("id", "main"), ("class", "container wide")]
///     .into_iter()
///     .collect();
///
/// assert_eq!(attrs.get("id"), Some("main"));
/// assert!(attrs.contains_key("class"));
/// assert_eq!(attrs.get("nope"), None);
/// assert_eq!(attrs.get_or("nope", "default"), "default");
/// assert_eq!(attrs.len(), 2);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttributeMap {
    data: IndexMap<String, String>,
}

impl AttributeMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from anything that yields key/value pairs, preserving order.
    ///
    /// Later duplicates overwrite earlier ones *in place*, keeping the first
    /// occurrence's position — the same as assigning into a Python `dict`.
    pub fn from_pairs<K, V, I>(pairs: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        Self {
            data: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// The value of `key`, or `None`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(String::as_str)
    }

    /// The value of `key`, or `default` — the analogue of `attrs.get(key, default)`.
    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    /// Whether `key` is present. Boolean HTML attributes (`required`, `disabled`)
    /// are present with an empty value, so this returns `true` for them.
    pub fn contains_key(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// Number of attributes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the element has no attributes.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Key/value pairs in insertion (document) order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.data.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Attribute names in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        self.data.keys().map(String::as_str)
    }

    /// Attribute values in insertion order.
    pub fn values(&self) -> impl Iterator<Item = &str> + '_ {
        self.data.values().map(String::as_str)
    }

    /// Yield every attribute whose **value** matches `keyword`, in insertion order.
    ///
    /// `partial = false` requires an exact match, `partial = true` a substring match.
    /// Both are **case-sensitive** — this mirrors `AttributesHandler.search_values`
    /// and is pinned by a test (`search_values("MAIN", false)` finds nothing when the
    /// value is `"main"`).
    ///
    /// Python yields a one-entry `AttributesHandler` per hit; yielding the pair itself
    /// is the idiomatic Rust shape (`ARCHITECTURE.md` §7.5). Use
    /// [`AttributeMap::search_values_as_maps`] when the Python shape is needed.
    ///
    /// ```
    /// use browser_core::attrs::AttributeMap;
    ///
    /// let attrs = AttributeMap::from_pairs([("id", "main"), ("class", "container")]);
    /// let hits: Vec<_> = attrs.search_values("main", false).collect();
    /// assert_eq!(hits, vec![("id", "main")]);
    /// assert_eq!(attrs.search_values("MAIN", false).count(), 0); // case-sensitive
    /// assert_eq!(attrs.search_values("contain", true).count(), 1);
    /// ```
    pub fn search_values<'a>(
        &'a self,
        keyword: &'a str,
        partial: bool,
    ) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        self.iter().filter(move |(_, value)| {
            if partial {
                value.contains(keyword)
            } else {
                *value == keyword
            }
        })
    }

    /// [`AttributeMap::search_values`] in Python's shape: one single-entry map per hit.
    pub fn search_values_as_maps<'a>(
        &'a self,
        keyword: &'a str,
        partial: bool,
    ) -> impl Iterator<Item = AttributeMap> + 'a {
        self.search_values(keyword, partial)
            .map(|(k, v)| AttributeMap::from_pairs([(k, v)]))
    }

    /// The attributes as compact JSON **bytes**.
    ///
    /// Python's `json_string` property returns `bytes` (`orjson.dumps`), not `str`,
    /// and a test asserts exactly that; the port keeps the byte-ness.
    ///
    /// ```
    /// use browser_core::attrs::AttributeMap;
    ///
    /// let attrs = AttributeMap::from_pairs([("id", "main"), ("a", "b")]);
    /// assert_eq!(attrs.json_string().unwrap(), br#"{"id":"main","a":"b"}"#.to_vec());
    /// ```
    pub fn json_string(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.data)
    }

    /// Borrow the backing ordered map.
    pub fn as_index_map(&self) -> &IndexMap<String, String> {
        &self.data
    }

    /// Consume into a mutable ordered map — the analogue of Python's `dict(attrs)`,
    /// which users are told to call when they need a mutable copy.
    pub fn into_inner(self) -> IndexMap<String, String> {
        self.data
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for AttributeMap {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self::from_pairs(iter)
    }
}

impl From<IndexMap<String, String>> for AttributeMap {
    fn from(data: IndexMap<String, String>) -> Self {
        Self { data }
    }
}

impl<'a> IntoIterator for &'a AttributeMap {
    type Item = (&'a str, &'a str);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a str)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl std::ops::Index<&str> for AttributeMap {
    type Output = str;

    /// Panics for a missing key, the way Python's `attrs["missing"]` raises `KeyError`.
    fn index(&self, key: &str) -> &str {
        self.get(key)
            .unwrap_or_else(|| panic!("no attribute named {key:?}"))
    }
}

/// Renders like a Python `dict` — `{'id': 'main', 'class': 'x'}` — because
/// `str(AttributesHandler(...))` is the `mappingproxy`'s underlying dict repr and the
/// parser's `__str__`/`__repr__` embed it.
///
/// Quoting is Python-repr *shaped* but deliberately simple: values are single-quoted
/// and a contained `'` is backslash-escaped, rather than switching the outer quote.
impl fmt::Display for AttributeMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, (k, v)) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "'{}': '{}'", escape(k), escape(v))?;
        }
        f.write_str("}")
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextExt;

    fn sample() -> AttributeMap {
        AttributeMap::from_pairs([
            ("id", "main"),
            ("class", "container wide"),
            ("data-config", r#"{"theme": "dark", "version": 2.5}"#),
            ("data-empty", ""),
            ("required", ""),
        ])
    }

    // §6.16
    #[test]
    fn mapping_access() {
        let a = sample();
        assert_eq!(a.get("id"), Some("main"));
        assert!(a.contains_key("id"));
        assert!(!a.contains_key("nonexistent"));
        assert_eq!(a.get("nonexistent"), None);
        assert_eq!(a.get_or("nonexistent", "default"), "default");
        assert_eq!(&a["id"], "main");
    }

    // §6.17
    #[test]
    fn keys_values_items_and_len_behave_like_a_dict() {
        let a = AttributeMap::from_pairs([("id", "main"), ("class", "x")]);
        assert_eq!(a.keys().collect::<Vec<_>>(), vec!["id", "class"]);
        assert_eq!(a.values().collect::<Vec<_>>(), vec!["main", "x"]);
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            vec![("id", "main"), ("class", "x")]
        );
        assert_eq!(a.len(), 2);
        assert!(!a.is_empty());
    }

    // §6.18 — values carry the text helpers, so `.json_value()` works on them.
    #[test]
    fn values_parse_as_json() {
        let a = sample();
        let cfg = a.get("data-config").unwrap().json_value().unwrap();
        assert_eq!(cfg["version"].as_f64(), Some(2.5));
        assert_eq!(cfg["theme"].as_str(), Some("dark"));

        let a = AttributeMap::from_pairs([
            ("list", "[1, 2, 3, 4, 5]"),
            ("nested", r#"{"a": {"b": [1]}}"#),
            ("null", "null"),
        ]);
        assert_eq!(
            a.get("list")
                .unwrap()
                .json_value()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            a.get("nested").unwrap().json_value().unwrap()["a"]["b"][0],
            1
        );
        assert!(a.get("null").unwrap().json_value().unwrap().is_null());
    }

    // §6.19
    #[test]
    fn invalid_json_errors_and_missing_key_is_none_first() {
        let a = AttributeMap::from_pairs([("broken", r#"{"broken: json}"#)]);
        assert!(a.get("broken").unwrap().json_value().is_err());
        assert!(a.get("missing").is_none());
    }

    // §6.20
    #[test]
    fn json_string_is_bytes_and_round_trips() {
        let a = sample();
        let bytes: Vec<u8> = a.json_string().unwrap();
        let back: IndexMap<String, String> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.len(), a.len());
        assert_eq!(back["id"], "main");
        assert_eq!(back["data-empty"], "");
    }

    // §6.21
    #[test]
    fn search_values_is_case_sensitive() {
        let a = sample();
        let exact: Vec<_> = a.search_values("main", false).collect();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].0, "id");
        assert!(a.search_values("container", true).count() >= 1);
        assert_eq!(a.search_values("MAIN", false).count(), 0);
        assert_eq!(a.search_values("definitely-not-here", true).count(), 0);

        let numeric = AttributeMap::from_pairs([("a", "x2"), ("b", "22"), ("c", "3")]);
        assert!(numeric.search_values("2", true).count() > 1);
    }

    #[test]
    fn search_values_as_maps_matches_the_python_shape() {
        let a = sample();
        let hits: Vec<_> = a.search_values("main", false).collect();
        let maps: Vec<_> = a.search_values_as_maps("main", false).collect();
        assert_eq!(maps.len(), hits.len());
        assert_eq!(maps[0], AttributeMap::from_pairs([("id", "main")]));
        assert_eq!(maps[0].len(), 1);
    }

    // §6.22
    #[test]
    fn boolean_and_empty_attributes_survive() {
        let a = AttributeMap::from_pairs([("required", ""), ("disabled", ""), ("data-empty", "")]);
        assert!(a.contains_key("required"));
        assert!(a.contains_key("disabled"));
        assert_eq!(a.get("data-empty"), Some(""));
        assert_eq!(a.len(), 3);
    }

    // §6.24
    #[test]
    fn display_mentions_names_and_values() {
        let a = AttributeMap::from_pairs([("id", "main")]);
        assert_eq!(a.to_string(), "{'id': 'main'}");
        assert!(format!("{a:?}").contains("main"));
        let quoted = AttributeMap::from_pairs([("k", "it's")]);
        assert_eq!(quoted.to_string(), r"{'k': 'it\'s'}");
    }

    // §6.25
    #[test]
    fn empty_map() {
        let a = AttributeMap::new();
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());
        assert_eq!(a.keys().count(), 0);
        assert_eq!(a.get("any"), None);
        assert_eq!(a.to_string(), "{}");
    }

    // §6.27
    #[test]
    fn equality_compares_as_mappings() {
        let a = AttributeMap::from_pairs([("id", "a")]);
        let b = AttributeMap::from_pairs([("id", "b")]);
        let c = AttributeMap::from_pairs([("id", "a")]);
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    // §6.28
    #[test]
    fn one_hundred_and_one_attributes_are_preserved() {
        let a: AttributeMap = (0..101)
            .map(|i| (format!("data-{i}"), format!("value-{i}")))
            .collect();
        assert_eq!(a.len(), 101);
        assert_eq!(a.search_values("value-77", false).count(), 1);
        // Order is document order, not hash order.
        assert_eq!(a.keys().next(), Some("data-0"));
        assert_eq!(a.keys().last(), Some("data-100"));
    }

    // §6.29
    #[test]
    fn unicode_values_round_trip() {
        let a = AttributeMap::from_pairs([
            ("emoji", "🎉🚀"),
            ("chinese", "你好世界"),
            ("arabic", "مرحبا"),
            ("accented", "café"),
        ]);
        assert_eq!(a.get("emoji"), Some("🎉🚀"));
        assert_eq!(a.search_values("你好", true).count(), 1);
        let bytes = a.json_string().unwrap();
        let back: IndexMap<String, String> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back["arabic"], "مرحبا");
    }

    #[test]
    fn duplicate_keys_keep_the_first_position() {
        let a = AttributeMap::from_pairs([("id", "one"), ("class", "c"), ("id", "two")]);
        assert_eq!(a.get("id"), Some("two"));
        assert_eq!(a.keys().collect::<Vec<_>>(), vec!["id", "class"]);
    }

    #[test]
    fn into_inner_gives_a_mutable_copy() {
        let mut d = sample().into_inner();
        d.insert("id".into(), "changed".into());
        assert_eq!(d["id"], "changed");
    }
}
