//! The recursive walk: markdownify's `process_tag` / `process_text`, the
//! block-whitespace predicates, and the boundary-newline collapsing rule.
//!
//! # The conversion root stands in for BeautifulSoup's `[document]`
//!
//! The reference implementation calls `markdownify(page.html_content)` — it *re-serialises* the
//! element and hands the string to a fresh `BeautifulSoup`, so inside the
//! converter the element is a direct child of the soup and has no ancestors and
//! no siblings. We convert a live [`NodeRef`] instead, so [`Walker`] remembers
//! which node the conversion started at and refuses to walk above it or
//! sideways from it. That makes `to_markdown(node)` byte-identical to Python's
//! re-parse without paying for the re-parse.

use bitflags::bitflags;
use browser_dom::{NodeData, NodeId, NodeRef};

use crate::convert::MarkdownOptions;
use crate::escape::{escape, py_rstrip, py_strip, ASCII_STRIP_CHARS};

/// The name BeautifulSoup gives its document node.
pub const DOCUMENT_NAME: &str = "[document]";

bitflags! {
    /// markdownify's `parent_tags` set, reduced to the four memberships the
    /// converters actually test.
    ///
    /// Python carries every ancestor tag name in a `set`; the only lookups are
    /// `'pre'`, `'li'`, `'_inline'` and `'_noformat'`, so four bits do the job
    /// (see `ARCHITECTURE.md` §1.6).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ParentTags: u8 {
        /// Some ancestor is a `<pre>`: whitespace is significant.
        const PRE = 0b0001;
        /// Some ancestor is an `<li>`: a nested list must not add a blank line.
        const LI = 0b0010;
        /// markdownify's `_inline` pseudo-tag — inside a heading or a table cell.
        const INLINE = 0b0100;
        /// markdownify's `_noformat` pseudo-tag — inside `pre`/`code`/`kbd`/`samp`.
        const NOFORMAT = 0b1000;
    }
}

/// The block-level elements markdownify strips whitespace *inside* of.
const BLOCK_TAGS: [&str; 18] = [
    "p",
    "blockquote",
    "article",
    "div",
    "section",
    "ol",
    "ul",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
];

/// markdownify's `re_html_heading` — `h(\d+)`, matched as a **prefix**.
///
/// Returns the digits' value, saturated at 6 (Python's `int()` is unbounded but
/// `convert_hN` immediately clamps to `1..=6`).
#[must_use]
pub fn heading_level(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('h')?;
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    Some(digits.parse::<u32>().unwrap_or(u32::MAX))
}

/// Python truthiness of a bs4 node: a `Tag` is always truthy (bs4 ≥ 4.13 defines
/// `__bool__`), a `NavigableString` is truthy when it is non-empty.
#[must_use]
pub fn py_truthy(node: Option<NodeRef<'_>>) -> bool {
    match node {
        None => false,
        Some(node) => match node.value() {
            NodeData::Element(_) | NodeData::Document => true,
            NodeData::Text(text) | NodeData::Comment(text) => !text.is_empty(),
            NodeData::Doctype { name, .. } => !name.is_empty(),
            NodeData::ProcessingInstruction { target, .. } => !target.is_empty(),
        },
    }
}

/// markdownify's `should_remove_whitespace_inside`.
///
/// Non-elements have no `name` in bs4, so they never qualify.
#[must_use]
pub fn should_remove_whitespace_inside(node: Option<NodeRef<'_>>) -> bool {
    let Some(node) = node else { return false };
    if !py_truthy(Some(node)) {
        return false;
    }
    let Some(name) = node.local_name() else {
        return false;
    };
    heading_level(name).is_some() || BLOCK_TAGS.contains(&name)
}

/// markdownify's `should_remove_whitespace_outside` — the same set plus `pre`.
#[must_use]
pub fn should_remove_whitespace_outside(node: Option<NodeRef<'_>>) -> bool {
    should_remove_whitespace_inside(node)
        || node.is_some_and(|node| py_truthy(Some(node)) && node.local_name() == Some("pre"))
}

/// markdownify's `_is_block_content_element`: tags, and text that is not all
/// whitespace. Comments and doctypes never count.
#[must_use]
pub fn is_block_content(node: NodeRef<'_>) -> bool {
    match node.value() {
        NodeData::Element(_) => true,
        NodeData::Text(text) => !py_strip(text).is_empty(),
        _ => false,
    }
}

/// Split a string into `(leading_newlines, content, trailing_newlines)`.
///
/// This is markdownify's `re_extract_newlines`
/// (`(?s)^(\n*)((?:.*[^\n])?)(\n*)$`) without the regex engine: the pattern is
/// total and unambiguous, so two scans do the same job.
#[must_use]
pub fn extract_newlines(text: &str) -> (usize, &str, usize) {
    let leading = text.len() - text.trim_start_matches('\n').len();
    let rest = &text[leading..];
    let trailing = rest.len() - rest.trim_end_matches('\n').len();
    (leading, &rest[..rest.len() - trailing], trailing)
}

/// One HTML→Markdown conversion in progress.
///
/// Borrowed, cheap to build, and immutable: everything the walk needs is the
/// option set plus the id of the node the conversion started at.
pub struct Walker<'o> {
    opts: &'o MarkdownOptions,
    root: NodeId,
}

impl<'o> Walker<'o> {
    /// Start a walk rooted at `root`.
    #[must_use]
    pub fn new(opts: &'o MarkdownOptions, root: NodeRef<'_>) -> Self {
        Self {
            opts,
            root: root.id(),
        }
    }

    /// The options this walk runs with.
    #[must_use]
    pub fn options(&self) -> &'o MarkdownOptions {
        self.opts
    }

    /// The parent, unless `node` is the conversion root (whose parent in Python
    /// is the synthetic `[document]`).
    #[must_use]
    pub fn parent_of<'a>(&self, node: NodeRef<'a>) -> Option<NodeRef<'a>> {
        if node.id() == self.root {
            None
        } else {
            node.parent()
        }
    }

    /// The previous sibling of any kind, unless `node` is the conversion root.
    #[must_use]
    pub fn prev_sibling_of<'a>(&self, node: NodeRef<'a>) -> Option<NodeRef<'a>> {
        if node.id() == self.root {
            None
        } else {
            node.prev_sibling()
        }
    }

    /// The next sibling of any kind, unless `node` is the conversion root.
    #[must_use]
    pub fn next_sibling_of<'a>(&self, node: NodeRef<'a>) -> Option<NodeRef<'a>> {
        if node.id() == self.root {
            None
        } else {
            node.next_sibling()
        }
    }

    /// The previous *element* sibling — bs4's `find_previous_sibling()`.
    #[must_use]
    pub fn prev_element_sibling_of<'a>(&self, node: NodeRef<'a>) -> Option<NodeRef<'a>> {
        if node.id() == self.root {
            None
        } else {
            node.prev_element_sibling()
        }
    }

    /// markdownify's `_next_block_content_sibling`.
    #[must_use]
    pub fn next_block_content_sibling<'a>(&self, node: NodeRef<'a>) -> Option<NodeRef<'a>> {
        let mut current = self.next_sibling_of(node);
        while let Some(sibling) = current {
            if is_block_content(sibling) {
                return Some(sibling);
            }
            current = sibling.next_sibling();
        }
        None
    }

    /// markdownify's `process_element`.
    #[must_use]
    pub fn process_element(&self, node: NodeRef<'_>, parent_tags: ParentTags) -> String {
        if node.is_text() {
            self.process_text(node, parent_tags)
        } else {
            self.process_tag(node, parent_tags)
        }
    }

    /// markdownify's `process_tag`.
    #[must_use]
    pub fn process_tag(&self, node: NodeRef<'_>, parent_tags: ParentTags) -> String {
        let name = tag_name(node);
        let should_remove_inside = should_remove_whitespace_inside(Some(node));

        let child_tags = self.child_tags(name, parent_tags);
        let inside_pre = name == "pre" || parent_tags.contains(ParentTags::PRE);

        let mut child_strings: Vec<String> = Vec::new();
        for child in node.children() {
            if can_ignore(child, should_remove_inside) {
                continue;
            }
            let converted = self.process_element(child, child_tags);
            if !converted.is_empty() {
                child_strings.push(converted);
            }
        }

        let text = if inside_pre {
            child_strings.concat()
        } else {
            collapse_boundary_newlines(&child_strings)
        };

        // `get_conv_fn` returning `None` means "no converter for this tag" —
        // the children's text passes straight through (this is why `<span>` and
        // `<title>` contribute their text to the output).
        match self.convert_tag(name, node, &text, parent_tags) {
            Some(converted) => converted,
            None => text,
        }
    }

    /// markdownify's `process_text`.
    #[must_use]
    pub fn process_text(&self, node: NodeRef<'_>, parent_tags: ParentTags) -> String {
        let raw = node.as_text().unwrap_or("");
        let mut text = if parent_tags.contains(ParentTags::PRE) {
            raw.to_owned()
        } else if self.opts.wrap {
            crate::convert::collapse_all_whitespace(raw)
        } else {
            let collapsed = crate::convert::collapse_newline_whitespace(raw);
            crate::convert::collapse_inline_whitespace(&collapsed)
        };

        if !parent_tags.contains(ParentTags::NOFORMAT) {
            text = escape(&text, self.opts).into_owned();
        }

        let prev = self.prev_sibling_of(node);
        let next = self.next_sibling_of(node);
        let parent = self.parent_of(node);

        if should_remove_whitespace_outside(prev)
            || (should_remove_whitespace_inside(parent) && !py_truthy(prev))
        {
            text = crate::escape::lstrip_chars(&text, &ASCII_STRIP_CHARS).to_owned();
        }
        if should_remove_whitespace_outside(next)
            || (should_remove_whitespace_inside(parent) && !py_truthy(next))
        {
            text = py_rstrip(&text).to_owned();
        }
        text
    }

    /// `parent_tags_for_children`: this tag plus the two pseudo-tags.
    fn child_tags(&self, name: &str, parent_tags: ParentTags) -> ParentTags {
        let mut tags = parent_tags;
        match name {
            "pre" => tags |= ParentTags::PRE | ParentTags::NOFORMAT,
            "code" | "kbd" | "samp" => tags |= ParentTags::NOFORMAT,
            "li" => tags |= ParentTags::LI,
            "td" | "th" => tags |= ParentTags::INLINE,
            _ => {}
        }
        if heading_level(name).is_some() {
            tags |= ParentTags::INLINE;
        }
        tags
    }
}

/// The bs4 `node.name` of a node reaching `process_tag`.
#[must_use]
pub fn tag_name(node: NodeRef<'_>) -> &str {
    if node.is_document() {
        DOCUMENT_NAME
    } else {
        node.local_name().unwrap_or("")
    }
}

/// markdownify's `_can_ignore`, evaluated for one child of a tag.
#[must_use]
pub fn can_ignore(node: NodeRef<'_>, should_remove_inside: bool) -> bool {
    match node.value() {
        NodeData::Element(_) => false,
        NodeData::Text(text) => {
            if !py_strip(text).is_empty() {
                return false;
            }
            let prev = node.prev_sibling();
            let next = node.next_sibling();
            if should_remove_inside && (!py_truthy(prev) || !py_truthy(next)) {
                return true;
            }
            should_remove_whitespace_outside(prev) || should_remove_whitespace_outside(next)
        }
        // Comments, doctypes and CDATA sections are always ignored. (Python
        // routes processing instructions through the text branch instead; the
        // HTML tokenizer never produces one, so we drop them here.)
        _ => true,
    }
}

/// markdownify's newline-collapsing pass over a tag's converted children.
///
/// When one child ends in newlines and the next starts with newlines, both runs
/// become a single run of `min(2, max(a, b))` newlines. Skipped inside `<pre>`.
#[must_use]
pub fn collapse_boundary_newlines(child_strings: &[String]) -> String {
    // `updated` mirrors Python's list of (leading, content, trailing) triples;
    // only the last pushed piece is ever revisited, so a running string plus the
    // length of the trailing run is enough.
    let mut out = String::new();
    let mut pending_trailing = 0usize;

    for child in child_strings {
        let (leading, content, trailing) = extract_newlines(child);
        let mut leading = leading;
        if pending_trailing > 0 && leading > 0 {
            // Replace both runs with the collapsed one.
            out.truncate(out.len() - pending_trailing);
            leading = std::cmp::min(2, std::cmp::max(pending_trailing, leading));
        }
        for _ in 0..leading {
            out.push('\n');
        }
        out.push_str(content);
        for _ in 0..trailing {
            out.push('\n');
        }
        pending_trailing = trailing;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::Document;

    #[test]
    fn heading_levels() {
        assert_eq!(heading_level("h1"), Some(1));
        assert_eq!(heading_level("h9"), Some(9));
        assert_eq!(heading_level("h12"), Some(12));
        assert_eq!(heading_level("hr"), None);
        assert_eq!(heading_level("header"), None);
        assert_eq!(heading_level("p"), None);
        // A prefix match, exactly like Python's `re.match`.
        assert_eq!(heading_level("h3x"), Some(3));
    }

    #[test]
    fn newline_extraction() {
        assert_eq!(extract_newlines("\n\nfoo\n"), (2, "foo", 1));
        assert_eq!(extract_newlines("\n\n\n"), (3, "", 0));
        assert_eq!(extract_newlines(""), (0, "", 0));
        assert_eq!(extract_newlines("a\nb"), (0, "a\nb", 0));
    }

    #[test]
    fn boundary_collapsing() {
        let pieces = vec!["a\n\n\n".to_owned(), "\n\n\nb".to_owned()];
        assert_eq!(collapse_boundary_newlines(&pieces), "a\n\nb");
        let pieces = vec!["a\n".to_owned(), "\nb".to_owned()];
        assert_eq!(collapse_boundary_newlines(&pieces), "a\nb");
        let pieces = vec!["a".to_owned(), "\n\nb".to_owned()];
        assert_eq!(collapse_boundary_newlines(&pieces), "a\n\nb");
    }

    #[test]
    fn block_predicates() {
        let doc = Document::parse("<div><p>x</p><pre>y</pre><span>z</span></div>");
        let p = doc.elements().find(|e| e.tag() == "p");
        let pre = doc.elements().find(|e| e.tag() == "pre");
        let span = doc.elements().find(|e| e.tag() == "span");
        assert!(should_remove_whitespace_inside(p));
        assert!(!should_remove_whitespace_inside(pre));
        assert!(should_remove_whitespace_outside(pre));
        assert!(!should_remove_whitespace_outside(span));
    }
}
