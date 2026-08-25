//! [`ItemList`] and the JSON / JSONL / CSV / XML exporters, including XML tag sanitising
//! and control-character deletion.
//!
//! Python's `ItemList` is a `list` subclass with four `to_*` methods. The port is a
//! `Vec<Item>` newtype that `Deref`s to a slice, so it reads like a list, plus the same
//! four exporters — all of which create parent directories and log
//! `"Saved N items to PATH"` at `INFO`.
//!
//! Every exporter is byte-compatible with Python's, pinned by
//! `tests/item_export_golden.rs` against files the reference interpreter wrote. That
//! matters more than it sounds: the XML exporter has to reproduce
//! `xml.etree.ElementTree`'s exact spelling (`<?xml version='1.0' encoding='utf-8'?>`,
//! single quotes, `<tag />` for empty elements, two-space indentation with the dedent on
//! the last child) and the CSV exporter has to reproduce `csv.DictWriter`'s `\r\n`
//! terminator and `QUOTE_MINIMAL` rules.
//!
//! # Value stringification
//!
//! Non-string values follow Python's `_stringify`: `null` becomes `""`, objects and arrays
//! become compact JSON, and everything else becomes `str(value)` — which is why booleans
//! export as `True` / `False` rather than `true` / `false`.

use std::path::Path;

use serde_json::Value;

use crate::error::{Result, SpiderError};
use crate::request::Item;

/// A list of scraped items with four exporters.
///
/// ```
/// use browser_spider::items::ItemList;
/// use browser_spider::request::Item;
///
/// let mut items = ItemList::new();
/// let mut item = Item::new();
/// item.insert("name".into(), "Widget".into());
/// items.push(item);
///
/// assert_eq!(items.len(), 1);
/// assert_eq!(items[0]["name"], "Widget");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemList(Vec<Item>);

impl ItemList {
    /// An empty list.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Append one item.
    pub fn push(&mut self, item: Item) {
        self.0.push(item);
    }

    /// Drop every item, keeping the allocation. The engine does this at the start of each
    /// run (Python clears `items` in place rather than replacing it).
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The items as a slice.
    pub fn as_slice(&self) -> &[Item] {
        &self.0
    }

    /// Consume the list, yielding its items.
    pub fn into_vec(self) -> Vec<Item> {
        self.0
    }

    /// Export to a single JSON array.
    ///
    /// `indent` switches on two-space pretty printing, matching `orjson`'s `OPT_INDENT_2`.
    /// No trailing newline is written, again matching `orjson`.
    ///
    /// # Errors
    ///
    /// [`SpiderError::Export`] if the parent directory cannot be created or the file
    /// cannot be written.
    pub fn to_json(&self, path: impl AsRef<Path>, indent: bool) -> Result<()> {
        let path = path.as_ref();
        create_parent(path)?;

        let encoded = if indent {
            let mut buffer = Vec::new();
            let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
            let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
            serde::Serialize::serialize(&self.0, &mut serializer).map_err(|source| {
                SpiderError::Serialize {
                    what: "the item list",
                    source,
                }
            })?;
            buffer
        } else {
            serde_json::to_vec(&self.0).map_err(|source| SpiderError::Serialize {
                what: "the item list",
                source,
            })?
        };

        write_file(path, &encoded)?;
        tracing::info!("Saved {} items to {}", self.len(), path.display());
        Ok(())
    }

    /// Export as JSON Lines: one compact object per line, each followed by `\n`.
    ///
    /// # Errors
    ///
    /// [`SpiderError::Export`] on any I/O failure.
    pub fn to_jsonl(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        create_parent(path)?;

        let mut buffer = Vec::new();
        for item in &self.0 {
            serde_json::to_writer(&mut buffer, item).map_err(|source| SpiderError::Serialize {
                what: "an item",
                source,
            })?;
            buffer.push(b'\n');
        }

        write_file(path, &buffer)?;
        tracing::info!("Saved {} items to {}", self.len(), path.display());
        Ok(())
    }

    /// Export to CSV.
    ///
    /// Columns default to the union of every key in the list, in first-seen order. Items
    /// missing a column get an empty cell, and keys outside the column list are ignored —
    /// Python's `extrasaction="ignore"`. The header row is always written, the terminator
    /// is `\r\n`, and quoting is minimal.
    ///
    /// # Errors
    ///
    /// [`SpiderError::Export`] on any I/O failure.
    pub fn to_csv(&self, path: impl AsRef<Path>, options: &CsvOptions) -> Result<()> {
        let path = path.as_ref();
        create_parent(path)?;

        let columns: Vec<String> = match &options.fields {
            Some(fields) => fields.clone(),
            None => self.column_union(),
        };

        if columns.is_empty() {
            // A zero-column CSV. Python's `csv.writer.writerow([])` emits the line
            // terminator and nothing else; the `csv` crate would write `""` instead,
            // because it disambiguates an empty *single-field* record that way. One
            // terminator for the header plus one per row.
            let mut encoded = Vec::new();
            for _ in 0..=self.len() {
                encoded.extend_from_slice(b"\r\n");
            }
            write_file(path, &encoded)?;
            tracing::info!("Saved {} items to {}", self.len(), path.display());
            return Ok(());
        }

        let mut writer = csv::WriterBuilder::new()
            .delimiter(options.delimiter)
            .terminator(csv::Terminator::CRLF)
            .quote_style(csv::QuoteStyle::Necessary)
            .from_writer(Vec::new());

        writer
            .write_record(&columns)
            .map_err(|source| export_error(path, source))?;
        for item in &self.0 {
            let row: Vec<String> = columns
                .iter()
                .map(|column| stringify(item.get(column).unwrap_or(&Value::Null)))
                .collect();
            writer
                .write_record(&row)
                .map_err(|source| export_error(path, source))?;
        }
        let encoded = writer
            .into_inner()
            .map_err(|error| export_error(path, error.into_error()))?;

        write_file(path, &encoded)?;
        tracing::info!("Saved {} items to {}", self.len(), path.display());
        Ok(())
    }

    /// Export to XML.
    ///
    /// Each item becomes an `<item>` element whose children are named after its keys. A key
    /// that is not a valid XML name is rewritten (every character outside `[\w.-]` becomes
    /// `_`, and a name that does not start with a letter or `_` gets one prepended) and the
    /// element keeps the original in a `name` attribute. Characters XML cannot represent —
    /// control characters other than tab, CR and LF — are **deleted** from both text and
    /// the `name` attribute.
    ///
    /// # Errors
    ///
    /// [`SpiderError::Export`] on any I/O failure.
    pub fn to_xml(&self, path: impl AsRef<Path>, options: &XmlOptions) -> Result<()> {
        let path = path.as_ref();
        create_parent(path)?;

        let encoded = self.render_xml(options);
        write_file(path, encoded.as_bytes())?;
        tracing::info!("Saved {} items to {}", self.len(), path.display());
        Ok(())
    }

    /// The XML document [`ItemList::to_xml`] would write, as a string.
    ///
    /// Exposed because it is far easier to test — and to embed in a feed — than a file.
    pub fn render_xml(&self, options: &XmlOptions) -> String {
        let mut out = String::from("<?xml version='1.0' encoding='utf-8'?>\n");
        let root = &options.root_tag;

        if self.0.is_empty() {
            // `ElementTree` writes a childless element as `<items />`.
            out.push_str(&format!("<{root} />"));
            return out;
        }

        // `ElementTree.indent(space="  ")` puts the first child on a new line indented by
        // one level, gives every child that same tail, and dedents after the last one.
        let (nl1, nl2, close_item, close_root) = if options.indent {
            ("\n  ", "\n    ", "\n  ", "\n")
        } else {
            ("", "", "", "")
        };

        out.push_str(&format!("<{root}>"));
        for item in &self.0 {
            out.push_str(nl1);
            let item_tag = &options.item_tag;
            if item.is_empty() {
                out.push_str(&format!("<{item_tag} />"));
                continue;
            }
            out.push_str(&format!("<{item_tag}>"));
            for (key, value) in item {
                out.push_str(nl2);
                let tag = xml_tag(key);
                let text = scrub_xml(&stringify(value));
                let attribute = if tag == *key {
                    String::new()
                } else {
                    format!(" name=\"{}\"", escape_xml_attribute(&scrub_xml(key)))
                };
                if text.is_empty() {
                    out.push_str(&format!("<{tag}{attribute} />"));
                } else {
                    out.push_str(&format!(
                        "<{tag}{attribute}>{}</{tag}>",
                        escape_xml_text(&text)
                    ));
                }
            }
            out.push_str(close_item);
            out.push_str(&format!("</{item_tag}>"));
        }
        out.push_str(close_root);
        out.push_str(&format!("</{root}>"));
        out
    }

    /// The union of every key, in the order the keys were first seen.
    ///
    /// Python spells it `list({key: None for item in self for key in item})`.
    fn column_union(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut columns = Vec::new();
        for item in &self.0 {
            for key in item.keys() {
                if seen.insert(key.clone()) {
                    columns.push(key.clone());
                }
            }
        }
        columns
    }
}

impl std::ops::Deref for ItemList {
    type Target = Vec<Item>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ItemList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<Item>> for ItemList {
    fn from(items: Vec<Item>) -> Self {
        Self(items)
    }
}

impl FromIterator<Item> for ItemList {
    fn from_iter<I: IntoIterator<Item = Item>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for ItemList {
    type Item = Item;
    type IntoIter = std::vec::IntoIter<Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a ItemList {
    type Item = &'a Item;
    type IntoIter = std::slice::Iter<'a, Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Extend<Item> for ItemList {
    fn extend<I: IntoIterator<Item = Item>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

/// Options for [`ItemList::to_csv`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvOptions {
    /// The columns to write. `None` uses the union of every key, in first-seen order.
    pub fields: Option<Vec<String>>,
    /// The column separator. Python's default is `,`.
    pub delimiter: u8,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            fields: None,
            delimiter: b',',
        }
    }
}

/// Options for [`ItemList::to_xml`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XmlOptions {
    /// The element wrapping every item. Python's default is `items`.
    pub root_tag: String,
    /// The element wrapping one item. Python's default is `item`.
    pub item_tag: String,
    /// Pretty-print with two-space indentation. Python's default is `true`.
    pub indent: bool,
}

impl Default for XmlOptions {
    fn default() -> Self {
        Self {
            root_tag: "items".to_owned(),
            item_tag: "item".to_owned(),
            indent: true,
        }
    }
}

/// Python's `_stringify`: turn one item value into text without silently dropping data.
///
/// `null` → `""`, containers → compact JSON, everything else → `str(value)`. Note that
/// booleans go through `str()`, so they render as `True` / `False`.
pub fn stringify(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

/// Python's `_xml_tag`: make an item key usable as an XML element name.
///
/// ```
/// use browser_spider::items::xml_tag;
///
/// assert_eq!(xml_tag("product name"), "product_name");
/// assert_eq!(xml_tag("2nd"), "_2nd");
/// assert_eq!(xml_tag(""), "_");
/// assert_eq!(xml_tag("price.usd-net"), "price.usd-net");
/// ```
pub fn xml_tag(key: &str) -> String {
    let tag: String = key
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    match tag.chars().next() {
        Some(first) if first.is_alphabetic() || first == '_' => tag,
        _ => format!("_{tag}"),
    }
}

/// Python's `_XML_FORBIDDEN_CHARS.sub("", text)`: delete every character XML 1.0 cannot
/// represent. Tabs, carriage returns and newlines survive.
///
/// ```
/// use browser_spider::items::scrub_xml;
///
/// assert_eq!(scrub_xml("bad\u{8}char\u{b}here"), "badcharhere");
/// assert_eq!(scrub_xml("keep\tthese\r\n"), "keep\tthese\r\n");
/// ```
pub fn scrub_xml(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            matches!(c, '\t' | '\n' | '\r')
                || ('\u{20}'..='\u{d7ff}').contains(&c)
                || ('\u{e000}'..='\u{fffd}').contains(&c)
                || c >= '\u{10000}'
        })
        .collect()
}

/// `ElementTree._escape_cdata`: `&`, `<` and `>` only.
fn escape_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `ElementTree._escape_attrib`: like the text escape, plus quotes and the three
/// whitespace characters that would otherwise be normalised away by an XML parser.
fn escape_xml_attribute(text: &str) -> String {
    escape_xml_text(text)
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#09;")
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| SpiderError::Export {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|source| SpiderError::Export {
        path: path.to_path_buf(),
        source,
    })
}

fn export_error(path: &Path, source: impl Into<CsvIoError>) -> SpiderError {
    SpiderError::Export {
        path: path.to_path_buf(),
        source: source.into().0,
    }
}

/// Wrapper so both `csv::Error` and `std::io::Error` convert into the same variant.
struct CsvIoError(std::io::Error);

impl From<csv::Error> for CsvIoError {
    fn from(error: csv::Error) -> Self {
        Self(std::io::Error::other(error))
    }
}

impl From<std::io::Error> for CsvIoError {
    fn from(error: std::io::Error) -> Self {
        Self(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(pairs: &[(&str, Value)]) -> Item {
        let mut item = Item::new();
        for (key, value) in pairs {
            item.insert((*key).to_owned(), value.clone());
        }
        item
    }

    fn sample() -> ItemList {
        ItemList::from(vec![
            item(&[("name", json!("Widget")), ("price", json!(9.99))]),
            item(&[("name", json!("Gadget")), ("tags", json!(["a", "b"]))]),
        ])
    }

    #[test]
    fn stringify_follows_pythons_rules() {
        assert_eq!(stringify(&Value::Null), "");
        assert_eq!(stringify(&json!(true)), "True");
        assert_eq!(stringify(&json!(false)), "False");
        assert_eq!(stringify(&json!(3)), "3");
        assert_eq!(stringify(&json!(1.5)), "1.5");
        assert_eq!(stringify(&json!("text")), "text");
        assert_eq!(stringify(&json!({"b": 1, "a": 2})), r#"{"b":1,"a":2}"#);
        assert_eq!(stringify(&json!([1, "x"])), r#"[1,"x"]"#);
    }

    #[test]
    fn columns_are_the_union_in_first_seen_order() {
        let items = ItemList::from(vec![
            item(&[("b", json!(1)), ("a", json!(2))]),
            item(&[("c", json!(3)), ("a", json!(4))]),
        ]);
        assert_eq!(items.column_union(), ["b", "a", "c"]);
    }

    #[test]
    fn json_export_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/items.json");
        sample().to_json(&path, false).expect("export");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with('['));
        assert!(!text.ends_with('\n'));
        let parsed: Vec<Item> = serde_json::from_str(&text).expect("parse");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn jsonl_export_writes_one_line_per_item() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("items.jsonl");
        sample().to_jsonl(&path).expect("export");

        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn an_empty_list_exports_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let items = ItemList::new();

        items
            .to_json(dir.path().join("a.json"), false)
            .expect("json");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.json")).unwrap(),
            "[]"
        );

        items.to_jsonl(dir.path().join("a.jsonl")).expect("jsonl");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.jsonl")).unwrap(),
            ""
        );

        items
            .to_xml(dir.path().join("a.xml"), &XmlOptions::default())
            .expect("xml");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.xml")).unwrap(),
            "<?xml version='1.0' encoding='utf-8'?>\n<items />"
        );
    }

    #[test]
    fn csv_uses_the_given_fields_and_ignores_extras() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("items.csv");
        sample()
            .to_csv(
                &path,
                &CsvOptions {
                    fields: Some(vec!["name".to_owned(), "missing".to_owned()]),
                    delimiter: b';',
                },
            )
            .expect("export");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "name;missing\r\nWidget;\r\nGadget;\r\n"
        );
    }

    #[test]
    fn xml_sanitises_tags_and_keeps_the_original_in_an_attribute() {
        let items = ItemList::from(vec![item(&[
            ("product name", json!("Widget")),
            ("2nd", json!("x")),
            ("ok", json!("y")),
        ])]);
        let xml = items.render_xml(&XmlOptions::default());
        assert!(xml.contains(r#"<product_name name="product name">Widget</product_name>"#));
        assert!(xml.contains(r#"<_2nd name="2nd">x</_2nd>"#));
        assert!(xml.contains("<ok>y</ok>"));
    }

    #[test]
    fn xml_deletes_control_characters_but_keeps_tabs() {
        let items = ItemList::from(vec![item(&[("v", json!("bad\u{8}char\u{b}here\tkept"))])]);
        let xml = items.render_xml(&XmlOptions::default());
        assert!(xml.contains("<v>badcharhere\tkept</v>"));
    }

    #[test]
    fn xml_escapes_markup_in_text() {
        let items = ItemList::from(vec![item(&[("v", json!("a & b < c > d"))])]);
        let xml = items.render_xml(&XmlOptions::default());
        assert!(xml.contains("<v>a &amp; b &lt; c &gt; d</v>"));
    }

    #[test]
    fn xml_writes_an_empty_element_for_a_null_value() {
        let items = ItemList::from(vec![item(&[("v", Value::Null)])]);
        let xml = items.render_xml(&XmlOptions::default());
        assert!(xml.contains("<v />"), "{xml}");
    }

    #[test]
    fn xml_without_indentation_is_one_line() {
        let items = sample();
        let xml = items.render_xml(&XmlOptions {
            indent: false,
            ..XmlOptions::default()
        });
        assert_eq!(xml.lines().count(), 2); // declaration + document
    }

    #[test]
    fn item_list_behaves_like_a_list() {
        let mut items = ItemList::new();
        assert!(items.is_empty());
        items.push(item(&[("a", json!(1))]));
        items.extend(vec![item(&[("b", json!(2))])]);
        assert_eq!(items.len(), 2);
        assert_eq!(items.iter().count(), 2);
        items.clear();
        assert!(items.is_empty());
    }
}
