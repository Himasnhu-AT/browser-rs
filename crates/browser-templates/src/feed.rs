//! [`XMLFeedSpider`] (namespace stripping) and [`CSVFeedSpider`] (flexible
//! records, optional headers).
//!
//! A port of `reference/spiders/templates/feed.py` (`spider-support.md` §11).
//!
//! This module also owns [`XmlNode`], the small namespace-aware XML tree that
//! stands in for the `lxml` elements Python hands to `parse_node` — and that
//! [`crate::sitemap::SitemapSpider`] reads its `<urlset>` and `<sitemapindex>`
//! bodies out of. It is deliberately a *tree*, not a stream: Python's
//! `parse_node` receives a detached, namespace-stripped element it can call
//! `findtext` and XPath on, and reproducing that needs a value the callback can
//! walk.
//!
//! ```
//! use browser_templates::feed::parse_xml;
//!
//! let feed = parse_xml(br#"<rss><channel>
//!     <item><title>One</title></item>
//!     <item><title>Two</title></item>
//! </channel></rss>"#)?;
//!
//! let titles: Vec<&str> = feed
//!     .iter()
//!     .filter(|node| node.name == "item")
//!     .filter_map(|node| node.findtext("title"))
//!     .collect();
//! assert_eq!(titles, ["One", "Two"]);
//! # Ok::<(), browser_templates::error::TemplateError>(())
//! ```
//!
//! # Declared divergence: the XML declaration's encoding is ignored
//!
//! `lxml.etree.fromstring(bytes)` honours `<?xml encoding="ISO-8859-1"?>`.
//! `quick-xml` can too, but only with its `encoding` feature, which is not
//! enabled in this workspace and cannot be — dependency versions are frozen. A
//! non-UTF-8 body is therefore decoded lossily (invalid bytes become U+FFFD)
//! rather than transcoded. The sitemaps.org protocol mandates UTF-8 and every
//! mainstream feed uses it, so this bites only on genuinely legacy input.

use std::sync::Arc;

use async_trait::async_trait;
use browser_spider::request::Item;
use browser_spider::spider::{CallbackStream, CrawlResponse, Spider, SpiderSettings, Yielded};
use indexmap::IndexMap;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use serde_json::Value;

use crate::error::{Result, TemplateError};
use crate::gzip::decompress;

// ---------------------------------------------------------------------------
// The XML tree
// ---------------------------------------------------------------------------

/// One element of a parsed XML document.
///
/// The `lxml` members the template spiders actually use, and no more: the tag's
/// namespace and local name, its attributes, its `.text` (the characters before
/// the first child, which is lxml's rule, **not** the whole subtree's text) and
/// its children.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XmlNode {
    /// The namespace URI the tag is bound to, or `None` for no namespace.
    pub namespace: Option<String>,
    /// The tag's local name — `etree.QName(el.tag).localname`.
    pub name: String,
    /// Attributes in document order, keyed the way `lxml` keys `el.attrib`:
    /// `{uri}local` for a namespaced attribute, the bare local name otherwise.
    /// `xmlns` declarations are not attributes and are not here.
    pub attributes: IndexMap<String, String>,
    /// `el.text`: the character data before the first child element, `""` when
    /// there is none.
    pub text: String,
    /// Direct children, in document order. Comments and processing
    /// instructions are dropped, as `isinstance(el.tag, str)` drops them in
    /// Python.
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    /// One attribute, by the key `lxml` would use.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }

    /// The first direct child with this local name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|child| child.name == name)
    }

    /// Every direct child with this local name.
    pub fn find_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode> {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// `ElementTree.findtext`: the first matching child's `.text`, or `None`
    /// when there is no such child.
    ///
    /// A child that exists but is empty yields `Some("")`, which is Python's
    /// `elem.text or ""`.
    #[must_use]
    pub fn findtext(&self, name: &str) -> Option<&str> {
        self.find(name).map(|child| child.text.as_str())
    }

    /// Depth-first iteration, **including this node** — `lxml`'s `el.iter()`.
    ///
    /// The inclusion matters: an `<item>` document root matches
    /// `itertag = "item"`. `spider-support.md` §14.6 item 10.
    pub fn iter(&self) -> XmlNodeIter<'_> {
        XmlNodeIter { stack: vec![self] }
    }

    /// A copy of this subtree with every namespace removed.
    ///
    /// `XMLFeedSpider._strip_namespaces`: tags lose their URI, and every
    /// `{uri}local` attribute key is re-inserted under its local name **at the
    /// end** of the map, which is what `el.attrib[local] = el.attrib.pop(key)`
    /// does to a Python dict's ordering.
    #[must_use]
    pub fn stripped(&self) -> XmlNode {
        let mut attributes = IndexMap::with_capacity(self.attributes.len());
        let mut moved: Vec<(String, String)> = Vec::new();
        for (key, value) in &self.attributes {
            if key.starts_with('{') {
                let local = key.rsplit('}').next().unwrap_or(key);
                moved.push((local.to_owned(), value.clone()));
            } else {
                attributes.insert(key.clone(), value.clone());
            }
        }
        attributes.extend(moved);

        XmlNode {
            namespace: None,
            name: self.name.clone(),
            attributes,
            text: self.text.clone(),
            children: self.children.iter().map(XmlNode::stripped).collect(),
        }
    }
}

/// Depth-first iterator over an [`XmlNode`] and its descendants.
#[derive(Debug)]
pub struct XmlNodeIter<'a> {
    stack: Vec<&'a XmlNode>,
}

impl<'a> Iterator for XmlNodeIter<'a> {
    type Item = &'a XmlNode;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        // Push in reverse so the first child comes off next: document order.
        self.stack.extend(node.children.iter().rev());
        Some(node)
    }
}

/// Parse an XML document into its root [`XmlNode`].
///
/// # Errors
///
/// [`TemplateError::Xml`] for anything `lxml.etree.fromstring` would raise
/// `XMLSyntaxError` for, plus the empty-document case (Python: "Document is
/// empty").
pub fn parse_xml(body: &[u8]) -> Result<XmlNode> {
    // See the module docs: no `encoding` feature, so the declaration's charset
    // is not honoured and a non-UTF-8 body degrades rather than transcodes.
    let text = String::from_utf8_lossy(body);
    let mut reader = NsReader::from_str(&text);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_end_names = true;

    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root: Option<XmlNode> = None;

    loop {
        let (namespace, event) =
            reader
                .read_resolved_event()
                .map_err(|error| TemplateError::Xml {
                    message: error.to_string(),
                })?;
        match event {
            Event::Start(start) => {
                let namespace = match namespace {
                    ResolveResult::Bound(ns) => Some(ns.as_ref().to_owned()),
                    _ => None,
                };
                let name = start.local_name().as_ref().to_owned();
                let mut attributes = IndexMap::new();
                for attribute in start.attributes().with_checks(false) {
                    let attribute = attribute.map_err(|error| TemplateError::Xml {
                        message: error.to_string(),
                    })?;
                    let key = attribute.key;
                    // `xmlns` and `xmlns:x` are declarations, not attributes.
                    if key.as_ref() == "xmlns" || key.as_ref().starts_with("xmlns:") {
                        continue;
                    }
                    let local = key.local_name().as_ref().to_owned();
                    // An unprefixed attribute is in no namespace, per the XML
                    // spec, which is also how lxml keys `el.attrib`.
                    let full = match reader.resolver().resolve_attribute(key) {
                        (ResolveResult::Bound(ns), _) => {
                            format!("{{{}}}{local}", ns.as_ref())
                        }
                        _ => local,
                    };
                    let value = attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map(std::borrow::Cow::into_owned)
                        .unwrap_or_else(|_| attribute.value.as_ref().to_owned());
                    attributes.insert(full, value);
                }
                stack.push(XmlNode {
                    namespace,
                    name,
                    attributes,
                    text: String::new(),
                    children: Vec::new(),
                });
            }
            Event::End(_) => {
                let Some(node) = stack.pop() else {
                    return Err(TemplateError::Xml {
                        message: "unbalanced end tag".to_owned(),
                    });
                };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(text) => {
                if let Some(current) = stack.last_mut() {
                    // lxml's `.text` is what precedes the first child; anything
                    // after one is that child's `.tail`, which no template
                    // spider reads.
                    if current.children.is_empty() {
                        // `quick-xml` reports entity references as their own
                        // `GeneralRef` events, so plain text needs only EOL
                        // normalisation here.
                        current.text.push_str(text.xml10_content().as_ref());
                    }
                }
            }
            // `&amp;` and `&#39;` arrive as their own events; lxml folds them
            // straight into `el.text`, so this does too.
            Event::GeneralRef(reference) => {
                if let Some(current) = stack.last_mut() {
                    if current.children.is_empty() {
                        let name = reference.xml10_content();
                        let source = format!("&{name};");
                        match quick_xml::escape::unescape(&source) {
                            Ok(resolved) => current.text.push_str(resolved.as_ref()),
                            // An undeclared entity: lxml drops it too (there is
                            // no DTD to resolve it against).
                            Err(_) => tracing::debug!("unresolvable entity {source:?}"),
                        }
                    }
                }
            }
            Event::CData(data) => {
                if let Some(current) = stack.last_mut() {
                    if current.children.is_empty() {
                        current.text.push_str(data.as_ref());
                    }
                }
            }
            Event::Eof => break,
            // Comments, PIs, the declaration and the doctype are all dropped —
            // `isinstance(el.tag, str)` drops the first two in Python and the
            // rest are not elements.
            _ => {}
        }
    }

    root.ok_or(TemplateError::Xml {
        message: "Document is empty".to_owned(),
    })
}

// ---------------------------------------------------------------------------
// XMLFeedSpider
// ---------------------------------------------------------------------------

/// `parse_node(response, node)`.
pub type NodeCallbackFn = Arc<dyn Fn(&CrawlResponse, &XmlNode) -> Vec<Yielded> + Send + Sync>;

/// `parse_row(response, row)`.
pub type RowCallbackFn = Arc<dyn Fn(&CrawlResponse, &Item) -> Vec<Yielded> + Send + Sync>;

/// A spider that iterates over the nodes of an XML feed — RSS, Atom, product
/// feeds.
///
/// Each node reaches `parse_node` **namespace-stripped**, so
/// `node.findtext("title")` and `node.find("thumbnail")` work on any feed
/// without a namespace map. Gzipped feeds are inflated automatically.
///
/// ```
/// use browser_templates::feed::XMLFeedSpider;
///
/// let spider = XMLFeedSpider::builder("news")
///     .itertag("item")
///     .parse_node(|_response, node| {
///         let mut item = browser_spider::request::Item::new();
///         item.insert("title".to_owned(), node.findtext("title").unwrap_or("").into());
///         vec![browser_spider::spider::Yielded::item(item)]
///     })
///     .build();
/// assert_eq!(spider.itertag(), "item");
/// ```
pub struct XMLFeedSpider {
    name: String,
    settings: SpiderSettings,
    itertag: String,
    namespaces: Vec<(String, String)>,
    parse_node: Option<NodeCallbackFn>,
}

impl std::fmt::Debug for XMLFeedSpider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XMLFeedSpider")
            .field("name", &self.name)
            .field("itertag", &self.itertag)
            .field("namespaces", &self.namespaces)
            .field("parse_node", &self.parse_node.is_some())
            .finish()
    }
}

impl XMLFeedSpider {
    /// Start building one.
    #[must_use]
    pub fn builder(name: impl Into<String>) -> XMLFeedSpiderBuilder {
        XMLFeedSpiderBuilder::new(name)
    }

    /// The node name being iterated.
    #[must_use]
    pub fn itertag(&self) -> &str {
        &self.itertag
    }

    /// Resolve `itertag` into `(namespace uri or None, local name)`.
    ///
    /// A plain name matches in **any** namespace; a prefixed one matches only
    /// the URI the prefix maps to.
    ///
    /// # Errors
    ///
    /// [`TemplateError::UndefinedNamespacePrefix`] when the prefix is not in
    /// `namespaces`.
    pub fn wanted_tag(&self) -> Result<(Option<&str>, &str)> {
        // `rpartition(":")` — the *last* colon wins.
        let Some((prefix, name)) = self.itertag.rsplit_once(':') else {
            return Ok((None, self.itertag.as_str()));
        };
        if prefix.is_empty() {
            return Ok((None, name));
        }
        let uri = self
            .namespaces
            .iter()
            .find(|(candidate, _)| candidate == prefix)
            .map(|(_, uri)| uri.as_str())
            .filter(|uri| !uri.is_empty())
            .ok_or_else(|| TemplateError::UndefinedNamespacePrefix {
                prefix: prefix.to_owned(),
            })?;
        Ok((Some(uri), name))
    }

    /// Every matching node of `root`, namespace-stripped, in document order.
    ///
    /// # Errors
    ///
    /// As [`XMLFeedSpider::wanted_tag`].
    pub fn iter_nodes(&self, root: &XmlNode) -> Result<Vec<XmlNode>> {
        let (uri, name) = self.wanted_tag()?;
        Ok(root
            .iter()
            .filter(|node| {
                node.name == name && uri.is_none_or(|uri| node.namespace.as_deref() == Some(uri))
            })
            .map(XmlNode::stripped)
            .collect())
    }

    /// The whole of Python's `parse`, as a value the callback stream is built
    /// from.
    fn parse_yields(&self, response: &CrawlResponse) -> Vec<Yielded> {
        let content_type = response.header("content-type");
        let body = match decompress(response.body(), content_type) {
            Ok(body) => body,
            Err(TemplateError::GzipTruncated) => {
                // `EOFError` is not an `OSError`: Python does not catch it here.
                return vec![Yielded::error(TemplateError::GzipTruncated)];
            }
            Err(error) => {
                tracing::warn!("Failed to decompress feed: {error}");
                return Vec::new();
            }
        };

        let root = match parse_xml(&body) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!("Failed to parse XML feed from {}: {error}", response.url);
                return Vec::new();
            }
        };

        let nodes = match self.iter_nodes(&root) {
            Ok(nodes) => nodes,
            // Python raises `ValueError` out of `_wanted_tag`, which the engine
            // catches as a callback failure.
            Err(error) => return vec![Yielded::error(error)],
        };

        let Some(parse_node) = &self.parse_node else {
            return vec![Yielded::error(TemplateError::NotImplemented {
                spider: self.name.clone(),
                method: "parse_node",
            })];
        };

        let mut out = Vec::new();
        for node in &nodes {
            out.extend(parse_node(response, node));
        }
        out
    }
}

#[async_trait]
impl Spider for XMLFeedSpider {
    fn name(&self) -> &str {
        &self.name
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
        let yielded = self.parse_yields(&response);
        Box::pin(futures::stream::iter(yielded))
    }
}

/// Builder for [`XMLFeedSpider`].
#[derive(Default)]
pub struct XMLFeedSpiderBuilder {
    name: String,
    settings: SpiderSettings,
    itertag: Option<String>,
    namespaces: Vec<(String, String)>,
    parse_node: Option<NodeCallbackFn>,
}

impl std::fmt::Debug for XMLFeedSpiderBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XMLFeedSpiderBuilder")
            .field("name", &self.name)
            .field("itertag", &self.itertag)
            .field("namespaces", &self.namespaces)
            .finish()
    }
}

impl XMLFeedSpiderBuilder {
    /// A builder for a feed spider called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Every `Spider` class attribute.
    #[must_use]
    pub fn settings(mut self, settings: SpiderSettings) -> Self {
        self.settings = settings;
        self
    }

    /// `itertag=` — the node name to iterate. Default `"item"`.
    #[must_use]
    pub fn itertag(mut self, itertag: impl Into<String>) -> Self {
        self.itertag = Some(itertag.into());
        self
    }

    /// `namespaces=` — the `(prefix, uri)` pairs `itertag` may use.
    #[must_use]
    pub fn namespaces<I, P, U>(mut self, namespaces: I) -> Self
    where
        I: IntoIterator<Item = (P, U)>,
        P: Into<String>,
        U: Into<String>,
    {
        self.namespaces = namespaces
            .into_iter()
            .map(|(prefix, uri)| (prefix.into(), uri.into()))
            .collect();
        self
    }

    /// `parse_node()` — required, exactly as overriding it is in Python.
    #[must_use]
    pub fn parse_node<F>(mut self, handler: F) -> Self
    where
        F: Fn(&CrawlResponse, &XmlNode) -> Vec<Yielded> + Send + Sync + 'static,
    {
        self.parse_node = Some(Arc::new(handler));
        self
    }

    /// Freeze it.
    #[must_use]
    pub fn build(self) -> XMLFeedSpider {
        XMLFeedSpider {
            name: self.name,
            settings: self.settings,
            itertag: self.itertag.unwrap_or_else(|| "item".to_owned()),
            namespaces: self.namespaces,
            parse_node: self.parse_node,
        }
    }
}

// ---------------------------------------------------------------------------
// CSVFeedSpider
// ---------------------------------------------------------------------------

/// The key ragged extra columns land under.
///
/// Python's `csv.DictReader` uses `restkey=None`, so the surplus fields end up
/// under the dictionary key `None` as a `list`. A `serde_json::Map` cannot key
/// on `null`, so the port reserves the **empty string** and stores an array
/// there. `spider-support.md` §14.6 item 8 asks for a documented sentinel; this
/// is it.
pub const CSV_RESTKEY: &str = "";

/// A spider that iterates over the rows of a CSV feed.
///
/// Gzipped feeds are inflated automatically. Values are always strings — Python
/// does no coercion and neither does this.
///
/// ```
/// use browser_templates::feed::CSVFeedSpider;
///
/// let spider = CSVFeedSpider::builder("prices")
///     .delimiter(b';')
///     .parse_row(|_response, row| {
///         let mut item = browser_spider::request::Item::new();
///         item.clone_from(row);
///         vec![browser_spider::spider::Yielded::item(item)]
///     })
///     .build();
/// assert_eq!(spider.delimiter(), b';');
/// ```
pub struct CSVFeedSpider {
    name: String,
    settings: SpiderSettings,
    delimiter: u8,
    quote: u8,
    headers: Option<Vec<String>>,
    parse_row: Option<RowCallbackFn>,
}

impl std::fmt::Debug for CSVFeedSpider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSVFeedSpider")
            .field("name", &self.name)
            .field("delimiter", &char::from(self.delimiter))
            .field("quote", &char::from(self.quote))
            .field("headers", &self.headers)
            .field("parse_row", &self.parse_row.is_some())
            .finish()
    }
}

impl CSVFeedSpider {
    /// Start building one.
    #[must_use]
    pub fn builder(name: impl Into<String>) -> CSVFeedSpiderBuilder {
        CSVFeedSpiderBuilder::new(name)
    }

    /// The field separator.
    #[must_use]
    pub fn delimiter(&self) -> u8 {
        self.delimiter
    }

    /// The quote character.
    #[must_use]
    pub fn quote(&self) -> u8 {
        self.quote
    }

    /// Read `text` into `{column: value}` rows.
    ///
    /// Reproduces `csv.DictReader`:
    ///
    /// * `headers = None` makes the **first** record the field names, and that
    ///   record is not emitted — even when it is the empty record a leading
    ///   blank line produces, in which case every column of every row lands
    ///   under [`CSV_RESTKEY`];
    /// * a row with **more** fields than there are names collapses the surplus
    ///   into [`CSV_RESTKEY`] as an array (`restkey=None`);
    /// * a row with **fewer** gets `null` for the missing names
    ///   (`restval=None`);
    /// * blank lines are skipped among the data rows, exactly as
    ///   `DictReader.__next__`'s `while row == []` loop skips them. The `csv`
    ///   crate drops them before they are seen, which is the same thing — but
    ///   a line holding one *quoted empty* field (`""`) is **not** blank and is
    ///   emitted, because `csv.reader` reads it as `[""]` and only a literally
    ///   empty line as `[]`.
    ///
    /// # Errors
    ///
    /// [`TemplateError::Csv`] on a malformed record.
    ///
    /// **Declared divergence:** a lone `\r` inside an unquoted field makes
    /// CPython's reader raise `_csv.Error("new-line character seen in unquoted
    /// field")`, which `CSVFeedSpider.parse` does not catch — a legacy
    /// Mac-line-ending feed takes the callback down. The `csv` crate accepts
    /// `\r` as a record terminator and this reads the feed. Pinned by
    /// `testdata/u18_csv.jsonl`'s `lone-cr*` rows.
    pub fn read_rows(&self, text: &str) -> Result<Vec<Item>> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .quote(self.quote)
            .flexible(true)
            .has_headers(false)
            .from_reader(text.as_bytes());

        let mut records = reader.records();
        let names: Vec<String> = match &self.headers {
            Some(headers) => headers.clone(),
            None if starts_with_blank_line(text) => {
                // `DictReader.fieldnames` is a bare `next(self.reader)`: it
                // takes whatever the first record is, and a leading blank line
                // makes that the *empty* record. The `csv` crate never yields
                // an empty record — it drops blank lines outright — so the
                // leading one has to be spotted in the text. Getting this
                // wrong made a feed whose first line is blank lose a whole
                // column of names; the u18 fuzz caught it on 96 of 2000 rows.
                Vec::new()
            }
            None => match records.next() {
                None => return Ok(Vec::new()),
                Some(record) => record
                    .map_err(|error| TemplateError::Csv {
                        message: error.to_string(),
                    })?
                    .iter()
                    .map(str::to_owned)
                    .collect(),
            },
        };

        let mut rows = Vec::new();
        for record in records {
            let record = record.map_err(|error| TemplateError::Csv {
                message: error.to_string(),
            })?;
            let mut row = Item::new();
            for (index, name) in names.iter().enumerate() {
                let value = record
                    .get(index)
                    .map_or(Value::Null, |field| Value::String(field.to_owned()));
                row.insert(name.clone(), value);
            }
            if record.len() > names.len() {
                let rest: Vec<Value> = record
                    .iter()
                    .skip(names.len())
                    .map(|field| Value::String(field.to_owned()))
                    .collect();
                row.insert(CSV_RESTKEY.to_owned(), Value::Array(rest));
            }
            rows.push(row);
        }
        Ok(rows)
    }

    fn parse_yields(&self, response: &CrawlResponse) -> Vec<Yielded> {
        let content_type = response.header("content-type");
        let body = match decompress(response.body(), content_type) {
            Ok(body) => body,
            Err(TemplateError::GzipTruncated) => {
                return vec![Yielded::error(TemplateError::GzipTruncated)];
            }
            Err(error) => {
                tracing::warn!("Failed to decompress feed: {error}");
                return Vec::new();
            }
        };

        // Python: `body.decode(response.encoding or "utf-8", errors="replace")`.
        let text = String::from_utf8_lossy(&body);
        let rows = match self.read_rows(&text) {
            Ok(rows) => rows,
            Err(error) => return vec![Yielded::error(error)],
        };

        let Some(parse_row) = &self.parse_row else {
            return vec![Yielded::error(TemplateError::NotImplemented {
                spider: self.name.clone(),
                method: "parse_row",
            })];
        };

        let mut out = Vec::new();
        for row in &rows {
            out.extend(parse_row(response, row));
        }
        out
    }
}

/// Does `text` open with a line terminator — i.e. is `csv.reader`'s **first**
/// record the empty one?
///
/// `csv.reader(StringIO("\na,b"))` yields `[[], ["a", "b"]]`, so
/// `DictReader.fieldnames` is `[]` and `"a"`/`"b"` become *data*. The `csv`
/// crate drops blank lines before the caller sees them, so this is how that
/// first empty record is recovered.
fn starts_with_blank_line(text: &str) -> bool {
    text.starts_with('\n') || text.starts_with("\r\n") || text.starts_with('\r')
}

#[async_trait]
impl Spider for CSVFeedSpider {
    fn name(&self) -> &str {
        &self.name
    }

    fn settings(&self) -> &SpiderSettings {
        &self.settings
    }

    async fn parse(self: Arc<Self>, response: CrawlResponse) -> CallbackStream {
        let yielded = self.parse_yields(&response);
        Box::pin(futures::stream::iter(yielded))
    }
}

/// Builder for [`CSVFeedSpider`].
#[derive(Default)]
pub struct CSVFeedSpiderBuilder {
    name: String,
    settings: SpiderSettings,
    delimiter: Option<u8>,
    quote: Option<u8>,
    headers: Option<Vec<String>>,
    parse_row: Option<RowCallbackFn>,
}

impl std::fmt::Debug for CSVFeedSpiderBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSVFeedSpiderBuilder")
            .field("name", &self.name)
            .field("delimiter", &self.delimiter)
            .field("quote", &self.quote)
            .field("headers", &self.headers)
            .finish()
    }
}

impl CSVFeedSpiderBuilder {
    /// A builder for a CSV feed spider called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Every `Spider` class attribute.
    #[must_use]
    pub fn settings(mut self, settings: SpiderSettings) -> Self {
        self.settings = settings;
        self
    }

    /// `delimiter=`. Default `b','`.
    ///
    /// **Declared divergence:** Python's is a one-character `str` and may be
    /// non-ASCII; the `csv` crate's is a byte. A multi-byte delimiter cannot be
    /// expressed here.
    #[must_use]
    pub fn delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = Some(delimiter);
        self
    }

    /// `quotechar=`. Default `b'"'`. Same ASCII restriction as
    /// [`CSVFeedSpiderBuilder::delimiter`].
    #[must_use]
    pub fn quote(mut self, quote: u8) -> Self {
        self.quote = Some(quote);
        self
    }

    /// `headers=` — the column names. Unset means "the first row is the
    /// header", which is Python's `fieldnames=None`.
    #[must_use]
    pub fn headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = Some(headers.into_iter().map(Into::into).collect());
        self
    }

    /// `parse_row()` — required, exactly as overriding it is in Python.
    #[must_use]
    pub fn parse_row<F>(mut self, handler: F) -> Self
    where
        F: Fn(&CrawlResponse, &Item) -> Vec<Yielded> + Send + Sync + 'static,
    {
        self.parse_row = Some(Arc::new(handler));
        self
    }

    /// Freeze it.
    #[must_use]
    pub fn build(self) -> CSVFeedSpider {
        CSVFeedSpider {
            name: self.name,
            settings: self.settings,
            delimiter: self.delimiter.unwrap_or(b','),
            quote: self.quote.unwrap_or(b'"'),
            headers: self.headers,
            parse_row: self.parse_row,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use browser_spider::request::Request;
    use browser_spider::session::FetchedResponse;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use futures::StreamExt;

    use super::*;

    /// `DictReader.fieldnames` is a bare `next(self.reader)`, so a leading
    /// blank line makes the field names `[]` and every column lands under the
    /// restkey. Skipping to the first non-blank record — which the port used
    /// to do — silently ate a whole column.
    #[test]
    fn a_leading_blank_line_is_the_header() {
        let spider = CSVFeedSpider::builder("t").build();
        let rows = spider.read_rows("\na,b\n1,2\n").expect("reads");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get(CSV_RESTKEY),
            Some(&serde_json::json!(["a", "b"]))
        );
        assert_eq!(
            rows[1].get(CSV_RESTKEY),
            Some(&serde_json::json!(["1", "2"]))
        );
        // CRLF and a run of blanks behave the same way.
        assert_eq!(spider.read_rows("\r\na,b\r\n").expect("reads").len(), 1);
        assert_eq!(spider.read_rows("\n\na,b\n").expect("reads").len(), 1);
        // Blank lines among the *data* are skipped, as `while row == []` does.
        assert_eq!(spider.read_rows("a,b\n\n1,2\n\n").expect("reads").len(), 1);
        // Only blank lines: the header is `[]` and there is nothing after it.
        assert!(spider.read_rows("\n\n\n").expect("reads").is_empty());
        // With explicit `headers` the blank first line is just a blank row.
        let named = CSVFeedSpider::builder("t").headers(["a", "b"]).build();
        assert_eq!(named.read_rows("\n1,2\n").expect("reads").len(), 1);
    }

    /// `csv.reader` reads a line holding one *quoted empty* field as `[""]`
    /// and a literally empty line as `[]`, and only the latter is blank.
    #[test]
    fn a_quoted_empty_field_is_a_row_not_a_blank() {
        let spider = CSVFeedSpider::builder("t").build();
        let rows = spider.read_rows("a\n\"\"\nb\n").expect("reads");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("a"), Some(&serde_json::json!("")));
        assert_eq!(rows[1].get("a"), Some(&serde_json::json!("b")));
    }

    const RSS: &[u8] = br#"<?xml version="1.0"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/">
  <channel>
    <title>Feed</title>
    <item>
      <title>First</title>
      <link>https://example.com/1</link>
      <media:thumbnail url="https://example.com/1.png" />
    </item>
    <item>
      <link>https://example.com/2</link>
    </item>
  </channel>
</rss>"#;

    const ATOM: &[u8] = br#"<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>A</title></entry>
  <entry><title>B</title></entry>
</feed>"#;

    fn response(body: &[u8], content_type: Option<&str>) -> CrawlResponse {
        let mut fetched = FetchedResponse::new(200, "https://example.com/feed.xml", body.to_vec());
        if let Some(content_type) = content_type {
            fetched
                .headers
                .insert("content-type".to_owned(), content_type.to_owned());
        }
        fetched.attach_request(&Request::new("https://example.com/feed.xml").with_sid("default"));
        CrawlResponse::from_fetched(&fetched, &Default::default(), None)
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).expect("write");
        encoder.finish().expect("finish")
    }

    fn collect_titles(spider: &XMLFeedSpider, body: &[u8]) -> Vec<String> {
        let root = parse_xml(body).expect("well-formed");
        spider
            .iter_nodes(&root)
            .expect("a defined prefix")
            .iter()
            .map(|node| node.findtext("title").unwrap_or("<none>").to_owned())
            .collect()
    }

    // -- the XML tree ------------------------------------------------------

    #[test]
    fn parse_xml_builds_the_tree_lxml_would() {
        let root = parse_xml(RSS).expect("well-formed");
        assert_eq!(root.name, "rss");
        assert_eq!(root.get("version"), Some("2.0"));
        assert_eq!(root.children.len(), 1);

        let channel = &root.children[0];
        assert_eq!(channel.findtext("title"), Some("Feed"));
        let items: Vec<&XmlNode> = channel.find_all("item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].findtext("link"), Some("https://example.com/1"));
        assert_eq!(items[1].findtext("title"), None, "no such child at all");
    }

    #[test]
    fn text_is_what_precedes_the_first_child_not_the_whole_subtree() {
        let root = parse_xml(b"<a>before<b>inner</b>after</a>").expect("well-formed");
        assert_eq!(root.text, "before");
        assert_eq!(root.find("b").map(|node| node.text.as_str()), Some("inner"));
    }

    #[test]
    fn entities_and_cdata_both_decode() {
        let root =
            parse_xml(b"<a><b>a &amp; b</b><c><![CDATA[<raw> & ]]></c></a>").expect("well-formed");
        assert_eq!(root.findtext("b"), Some("a & b"));
        assert_eq!(root.findtext("c"), Some("<raw> & "));
    }

    #[test]
    fn iter_includes_the_root() {
        let root = parse_xml(b"<item><item/></item>").expect("well-formed");
        assert_eq!(
            root.iter().count(),
            2,
            "an <item> root matches itertag=item"
        );
    }

    #[test]
    fn iter_is_depth_first_in_document_order() {
        let root = parse_xml(b"<a><b><c/></b><d/></a>").expect("well-formed");
        let names: Vec<&str> = root.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }

    #[test]
    fn stripping_moves_namespaced_attributes_to_the_end() {
        let root =
            parse_xml(br#"<a xmlns:m="urn:m" id="1" m:kind="wide" title="t"><m:b m:x="1"/></a>"#)
                .expect("well-formed");
        assert_eq!(root.namespace, None);
        let keys: Vec<&str> = root.attributes.keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "{urn:m}kind", "title"]);

        let stripped = root.stripped();
        let keys: Vec<&str> = stripped.attributes.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["id", "title", "kind"],
            "the popped key is re-inserted at the end, as it is in a Python dict"
        );
        assert_eq!(stripped.children[0].name, "b");
        assert_eq!(stripped.children[0].get("x"), Some("1"));
        assert!(stripped.children[0].namespace.is_none());
    }

    #[test]
    fn comments_and_processing_instructions_are_dropped() {
        let root = parse_xml(b"<a><!-- c --><?pi x?><b/></a>").expect("well-formed");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].name, "b");
    }

    #[test]
    fn an_empty_element_is_a_node_with_no_children() {
        let root = parse_xml(b"<a><b/></a>").expect("well-formed");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].text, "");
    }

    #[test]
    fn malformed_and_empty_documents_are_typed_errors() {
        assert!(matches!(
            parse_xml(b"<a><b></a>").expect_err("mismatched"),
            TemplateError::Xml { .. }
        ));
        assert!(matches!(
            parse_xml(b"").expect_err("empty"),
            TemplateError::Xml { .. }
        ));
        assert!(matches!(
            parse_xml(b"not xml at all").expect_err("no element"),
            TemplateError::Xml { .. }
        ));
    }

    // -- XMLFeedSpider -----------------------------------------------------

    #[test]
    fn the_default_itertag_is_item_and_matches_any_namespace() {
        let spider = XMLFeedSpider::builder("feed").build();
        assert_eq!(spider.itertag(), "item");
        assert_eq!(spider.wanted_tag().expect("plain"), (None, "item"));
        assert_eq!(collect_titles(&spider, RSS), ["First", "<none>"]);
    }

    #[test]
    fn a_plain_itertag_matches_a_default_namespace() {
        let spider = XMLFeedSpider::builder("feed").itertag("entry").build();
        assert_eq!(collect_titles(&spider, ATOM), ["A", "B"]);
    }

    #[test]
    fn a_prefixed_itertag_matches_only_that_namespace() {
        let spider = XMLFeedSpider::builder("feed")
            .itertag("media:thumbnail")
            .namespaces([("media", "http://search.yahoo.com/mrss/")])
            .build();
        let root = parse_xml(RSS).expect("well-formed");
        let nodes = spider.iter_nodes(&root).expect("defined");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].get("url"), Some("https://example.com/1.png"));

        let wrong = XMLFeedSpider::builder("feed")
            .itertag("media:thumbnail")
            .namespaces([("media", "urn:nope")])
            .build();
        assert!(wrong.iter_nodes(&root).expect("defined").is_empty());
    }

    #[test]
    fn an_undeclared_prefix_is_an_error() {
        let spider = XMLFeedSpider::builder("feed")
            .itertag("media:thumbnail")
            .build();
        let error = spider.wanted_tag().expect_err("undeclared");
        assert!(error.to_string().contains("namespaces"), "{error}");
    }

    #[test]
    fn a_stripped_node_reaches_its_namespaced_child_by_local_name() {
        let spider = XMLFeedSpider::builder("feed").build();
        let root = parse_xml(RSS).expect("well-formed");
        let nodes = spider.iter_nodes(&root).expect("plain");
        assert_eq!(
            nodes[0].find("thumbnail").and_then(|node| node.get("url")),
            Some("https://example.com/1.png")
        );
    }

    #[tokio::test]
    async fn parse_dispatches_every_node_to_parse_node() {
        let spider = Arc::new(
            XMLFeedSpider::builder("feed")
                .parse_node(|_response, node| {
                    let mut item = Item::new();
                    item.insert(
                        "link".to_owned(),
                        node.findtext("link").unwrap_or("").into(),
                    );
                    vec![Yielded::item(item)]
                })
                .build(),
        );
        let yielded: Vec<Yielded> = spider.parse(response(RSS, None)).await.collect().await;
        assert_eq!(yielded.len(), 2);
    }

    #[tokio::test]
    async fn a_gzipped_feed_decompresses() {
        let spider = Arc::new(
            XMLFeedSpider::builder("feed")
                .parse_node(|_response, _node| vec![Yielded::item(Item::new())])
                .build(),
        );
        // Magic bytes only, no content-type — Python's second trigger.
        let yielded: Vec<Yielded> = spider
            .parse(response(&gzip(RSS), None))
            .await
            .collect()
            .await;
        assert_eq!(yielded.len(), 2);
    }

    #[tokio::test]
    async fn malformed_xml_yields_nothing_and_warns() {
        let spider = Arc::new(
            XMLFeedSpider::builder("feed")
                .parse_node(|_response, _node| vec![Yielded::item(Item::new())])
                .build(),
        );
        let yielded: Vec<Yielded> = spider
            .parse(response(b"<rss><item></rss>", None))
            .await
            .collect()
            .await;
        assert!(yielded.is_empty());
    }

    #[tokio::test]
    async fn an_unset_parse_node_is_pythons_not_implemented_error() {
        let spider = Arc::new(XMLFeedSpider::builder("feed").build());
        let yielded: Vec<Yielded> = spider.parse(response(RSS, None)).await.collect().await;
        assert_eq!(yielded.len(), 1);
        match &yielded[0] {
            Yielded::Error(error) => {
                assert!(error.to_string().contains("parse_node"), "{error}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_node_may_yield_requests() {
        let spider = Arc::new(
            XMLFeedSpider::builder("feed")
                .parse_node(|response, node| {
                    node.findtext("link")
                        .and_then(|link| response.follow(link).ok())
                        .map(Yielded::request)
                        .into_iter()
                        .collect()
                })
                .build(),
        );
        let yielded: Vec<Yielded> = spider.parse(response(RSS, None)).await.collect().await;
        assert_eq!(yielded.len(), 2);
        assert!(yielded.iter().all(|y| matches!(y, Yielded::Request(_))));
    }

    // -- CSVFeedSpider -----------------------------------------------------

    fn csv_spider() -> CSVFeedSpider {
        CSVFeedSpider::builder("csv").build()
    }

    #[test]
    fn the_first_row_becomes_the_header_by_default() {
        let rows = csv_spider()
            .read_rows("title,price,url\nFirst,10.5,https://example.com/products/1\nSecond,3,https://example.com/products/2\n")
            .expect("well-formed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["title"], "First");
        assert_eq!(rows[0]["price"], "10.5", "values are never coerced");
        assert_eq!(rows[0]["url"], "https://example.com/products/1");
    }

    #[test]
    fn explicit_headers_map_a_headerless_body() {
        let spider = CSVFeedSpider::builder("csv")
            .headers(["name", "cost"])
            .build();
        let rows = spider.read_rows("First,10.5\nSecond,3\n").expect("ok");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "First");
        assert_eq!(rows[1]["cost"], "3");
    }

    #[test]
    fn a_custom_delimiter_and_quote_are_honoured() {
        let spider = CSVFeedSpider::builder("csv")
            .delimiter(b';')
            .quote(b'\'')
            .headers(["title", "price"])
            .build();
        let rows = spider.read_rows("'First;Post';10.5\n").expect("ok");
        assert_eq!(rows[0]["title"], "First;Post");
        assert_eq!(rows[0]["price"], "10.5");
    }

    #[test]
    fn a_doubled_quote_inside_a_quoted_field_is_a_literal_quote() {
        let rows = csv_spider()
            .read_rows("a\n\"say \"\"hi\"\"\"\n")
            .expect("ok");
        assert_eq!(rows[0]["a"], r#"say "hi""#);
    }

    #[test]
    fn ragged_rows_use_restkey_and_restval() {
        let rows = csv_spider().read_rows("a,b,c\n1,2\n1,2,3,4\n").expect("ok");
        assert_eq!(rows[0]["a"], "1");
        assert_eq!(rows[0]["c"], Value::Null, "restval=None");
        assert_eq!(
            rows[1][CSV_RESTKEY],
            serde_json::json!(["4"]),
            "restkey=None"
        );
        assert!(!rows[0].contains_key(CSV_RESTKEY));
    }

    #[test]
    fn blank_lines_and_an_empty_body_yield_no_rows() {
        assert!(csv_spider().read_rows("").expect("ok").is_empty());
        let rows = csv_spider().read_rows("a,b\n\n1,2\n\n").expect("ok");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["b"], "2");
    }

    #[tokio::test]
    async fn parse_dispatches_every_row_and_decompresses() {
        let spider = Arc::new(
            CSVFeedSpider::builder("csv")
                .parse_row(|_response, row| {
                    let mut item = Item::new();
                    item.clone_from(row);
                    vec![Yielded::item(item)]
                })
                .build(),
        );
        let body = b"title,price\nFirst,1\nSecond,2\n";
        let yielded: Vec<Yielded> = spider
            .clone()
            .parse(response(body, Some("text/csv")))
            .await
            .collect()
            .await;
        assert_eq!(yielded.len(), 2);

        let gzipped: Vec<Yielded> = spider
            .parse(response(&gzip(body), Some("application/gzip")))
            .await
            .collect()
            .await;
        assert_eq!(gzipped.len(), 2);
    }

    #[tokio::test]
    async fn an_unset_parse_row_is_pythons_not_implemented_error() {
        let spider = Arc::new(csv_spider());
        let yielded: Vec<Yielded> = spider
            .parse(response(b"a\n1\n", None))
            .await
            .collect()
            .await;
        assert_eq!(yielded.len(), 1);
        match &yielded[0] {
            Yielded::Error(error) => assert!(error.to_string().contains("parse_row"), "{error}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_latin_one_body_degrades_rather_than_crashing() {
        // Python decodes with `errors="replace"`; the port decodes lossily,
        // which lands in the same place.
        let spider = Arc::new(
            CSVFeedSpider::builder("csv")
                .parse_row(|_response, row| {
                    let mut item = Item::new();
                    item.clone_from(row);
                    vec![Yielded::item(item)]
                })
                .build(),
        );
        let body = b"title\ncaf\xe9\n";
        let yielded: Vec<Yielded> = spider.parse(response(body, None)).await.collect().await;
        assert_eq!(yielded.len(), 1);
    }

    #[test]
    fn a_truncated_gzip_feed_escapes_the_warn_and_skip_path() {
        // `EOFError` is not an `OSError`, so Python does not catch it here.
        let full = gzip(RSS);
        let spider = CSVFeedSpider::builder("csv")
            .parse_row(|_, _| Vec::new())
            .build();
        let truncated = response(&full[..full.len() - 6], None);
        let yielded = spider.parse_yields(&truncated);
        assert!(
            matches!(yielded.as_slice(), [Yielded::Error(_)]),
            "{yielded:?}"
        );
    }
}
