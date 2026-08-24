//! `outer_html`, `inner_html` and `pretty` serialisation, plus the two text
//! helpers the `Debug` impls need (`clean_spaces`, 40-char truncation).
//!
//! Serialisation is written by hand rather than routed through html5ever's
//! `Serialize` trait: we need `pretty`, we need CDATA sections to round-trip,
//! and the traversal is an explicit stack walk (via `ego_tree`'s edge iterator)
//! so a pathologically deep document cannot overflow the stack.

use std::fmt;

use ego_tree::iter::Edge;

use crate::node::{cdata_payload, NodeData, NodeRef};
use crate::qualname::{is_raw_text_element, is_void_element, to_string as name_to_string};

/// The `__repr__` truncation limit from `parser.md` §3.2.
pub const REPR_LENGTH_LIMIT: usize = 40;

/// the reference implementation's module-level `clean_spaces` helper (`parser.md` §7.4).
///
/// `\t` becomes a space, `\n` and `\r` are **deleted** (not replaced), then runs
/// of spaces collapse to one. Unlike `TextHandler.clean()` the result is *not*
/// trimmed and entities are never replaced.
///
/// This is a re-export of [`browser_core::text::clean_spaces`]: both crates
/// landed an implementation of `reference.core.utils.clean_spaces` in parallel and
/// `browser-core` owns the canonical one. The alias stays so the `serialize`
/// module reads as a complete unit and existing imports keep working.
///
/// ```
/// # use browser_dom::serialize::clean_spaces;
/// assert_eq!(clean_spaces(" a\n  b\tc "), " a b c ");
/// ```
pub use browser_core::text::clean_spaces;

/// `content[:40].strip() + "..."` when longer than the limit, else `content`.
#[must_use]
pub fn truncate_repr(content: &str) -> String {
    if content.chars().count() <= REPR_LENGTH_LIMIT {
        return content.to_owned();
    }
    let head: String = content.chars().take(REPR_LENGTH_LIMIT).collect();
    format!("{}...", head.trim())
}

/// Escape a string for use in character data (`&`, `<`, `>`, NBSP).
#[must_use]
pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{00A0}' => out.push_str("&nbsp;"),
            other => out.push(other),
        }
    }
    out
}

/// Escape a string for use in a double-quoted attribute value.
#[must_use]
pub fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{00A0}' => out.push_str("&nbsp;"),
            other => out.push(other),
        }
    }
    out
}

fn write_open(node: NodeRef<'_>, out: &mut String) {
    match node.value() {
        NodeData::Document => {}
        NodeData::Doctype { name, .. } => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            out.push('>');
        }
        NodeData::Element(el) => {
            out.push('<');
            out.push_str(&name_to_string(el.name()));
            for (name, value) in el.attrs().iter_qual() {
                out.push(' ');
                out.push_str(&name_to_string(name));
                out.push_str("=\"");
                out.push_str(&escape_attr(value));
                out.push('"');
            }
            out.push('>');
        }
        NodeData::Text(text) => {
            let raw = node
                .parent()
                .and_then(|p| {
                    p.as_element()
                        .map(|el| is_raw_text_element(el.local_name()))
                })
                .unwrap_or(false);
            if raw {
                out.push_str(text);
            } else {
                out.push_str(&escape_text(text));
            }
        }
        NodeData::Comment(text) => match cdata_payload(text) {
            Some(payload) => {
                out.push_str("<![CDATA[");
                out.push_str(payload);
                out.push_str("]]>");
            }
            None => {
                out.push_str("<!--");
                out.push_str(text);
                out.push_str("-->");
            }
        },
        NodeData::ProcessingInstruction { target, data } => {
            out.push_str("<?");
            out.push_str(target);
            out.push(' ');
            out.push_str(data);
            out.push('>');
        }
    }
}

fn write_close(node: NodeRef<'_>, out: &mut String) {
    if let NodeData::Element(el) = node.value() {
        if !is_void_element(el.local_name()) {
            out.push_str("</");
            out.push_str(&name_to_string(el.name()));
            out.push('>');
        }
    }
}

fn serialize_subtree(node: NodeRef<'_>, out: &mut String) {
    let doc = node.document();
    for edge in node.ego().traverse() {
        match edge {
            Edge::Open(n) => write_open(NodeRef::new(doc, n.id()), out),
            Edge::Close(n) => write_close(NodeRef::new(doc, n.id()), out),
        }
    }
}

impl<'a> NodeRef<'a> {
    /// The node's own markup, tag included — lxml's
    /// `tostring(el, method="html", with_tail=False)`, i.e. Python's
    /// `html_content`.
    ///
    /// The node's tail text is **not** included.
    #[must_use]
    pub fn outer_html(&self) -> String {
        let mut out = String::new();
        serialize_subtree(*self, &mut out);
        out
    }

    /// The markup of the node's children, without its own tag.
    #[must_use]
    pub fn inner_html(&self) -> String {
        let mut out = String::new();
        for child in self.children() {
            serialize_subtree(child, &mut out);
        }
        out
    }

    /// Indented markup — Python's `prettify()` (`pretty_print=True`).
    ///
    /// An element that contains character data is emitted on a single line, so
    /// no whitespace is ever introduced into text content; only structural
    /// nesting is indented. The result ends with a newline.
    #[must_use]
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        match self.value() {
            NodeData::Document => {
                for child in self.children() {
                    pretty_node(child, 0, &mut out);
                }
            }
            _ => pretty_node(*self, 0, &mut out),
        }
        out
    }
}

fn pretty_node(node: NodeRef<'_>, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let inline = match node.value() {
        NodeData::Element(_) => !node.has_children() || node.children().any(|c| c.is_text()),
        _ => true,
    };
    if inline {
        out.push_str(&pad);
        let mut rendered = String::new();
        serialize_subtree(node, &mut rendered);
        out.push_str(rendered.trim_end_matches('\n'));
        out.push('\n');
        return;
    }
    let mut open = String::new();
    write_open(node, &mut open);
    out.push_str(&pad);
    out.push_str(&open);
    out.push('\n');
    for child in node.children() {
        pretty_node(child, depth + 1, out);
    }
    let mut close = String::new();
    write_close(node, &mut close);
    if !close.is_empty() {
        out.push_str(&pad);
        out.push_str(&close);
        out.push('\n');
    }
}

impl fmt::Debug for NodeRef<'_> {
    /// Python's `Selector.__repr__`: `<text='…'>` for text nodes, and
    /// `<data='…' parent='…'>` for elements, both `clean_spaces`-normalised and
    /// truncated to 40 characters.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(text) = self.as_text() {
            return write!(f, "<text='{}'>", truncate_repr(text));
        }
        let content = truncate_repr(&clean_spaces(&self.outer_html()));
        write!(f, "<data='{content}'")?;
        if let Some(parent) = self.parent_element() {
            let parent_content = truncate_repr(&clean_spaces(&parent.outer_html()));
            write!(f, " parent='{parent_content}'")?;
        }
        f.write_str(">")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOptions;
    use crate::tree::Document;

    #[test]
    fn clean_spaces_deletes_newlines_and_collapses_spaces() {
        assert_eq!(clean_spaces("a\nb"), "ab");
        assert_eq!(clean_spaces("a\r\nb"), "ab");
        assert_eq!(clean_spaces("a\tb"), "a b");
        assert_eq!(clean_spaces("a     b"), "a b");
        assert_eq!(clean_spaces("  Hello World  "), " Hello World ");
    }

    #[test]
    fn truncation_matches_python() {
        let short = "abc";
        assert_eq!(truncate_repr(short), "abc");
        let long = "x".repeat(45);
        assert_eq!(truncate_repr(&long), format!("{}...", "x".repeat(40)));
        let padded = format!("{}   tail", "y".repeat(38));
        assert_eq!(truncate_repr(&padded), format!("{}...", "y".repeat(38)));
    }

    #[test]
    fn outer_and_inner_html() {
        let doc = Document::parse(r#"<div class="a"><p>hi &amp; bye</p></div>"#);
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        assert_eq!(
            div.outer_html(),
            r#"<div class="a"><p>hi &amp; bye</p></div>"#
        );
        assert_eq!(div.inner_html(), "<p>hi &amp; bye</p>");
        assert_eq!(
            div.first_element_child().unwrap().inner_html(),
            "hi &amp; bye"
        );
    }

    #[test]
    fn tail_text_is_not_part_of_outer_html() {
        let doc = Document::parse("<div><b>bold</b>tail</div>");
        let b = doc
            .elements()
            .find(|e| e.local_name() == Some("b"))
            .unwrap();
        assert_eq!(b.outer_html(), "<b>bold</b>");
        assert_eq!(b.tail(), Some("tail"));
    }

    #[test]
    fn void_elements_have_no_closing_tag() {
        let doc = Document::parse(r#"<div><br><img src="x.png"></div>"#);
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        assert_eq!(div.outer_html(), r#"<div><br><img src="x.png"></div>"#);
    }

    #[test]
    fn raw_text_elements_are_not_escaped() {
        let doc = Document::parse("<script>if (a < b && c) x();</script>");
        let script = doc
            .elements()
            .find(|e| e.local_name() == Some("script"))
            .unwrap();
        assert_eq!(script.outer_html(), "<script>if (a < b && c) x();</script>");
    }

    #[test]
    fn attribute_values_are_escaped() {
        let doc = Document::parse(r#"<a title='say "hi" & bye'>x</a>"#);
        let a = doc
            .elements()
            .find(|e| e.local_name() == Some("a"))
            .unwrap();
        assert_eq!(
            a.outer_html(),
            r#"<a title="say &quot;hi&quot; &amp; bye">x</a>"#
        );
    }

    #[test]
    fn pretty_indents_structure_but_never_text() {
        let doc = Document::parse("<div><ul><li>one</li><li>two</li></ul></div>");
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        assert_eq!(
            div.pretty(),
            "<div>\n  <ul>\n    <li>one</li>\n    <li>two</li>\n  </ul>\n</div>\n"
        );
    }

    #[test]
    fn pretty_keeps_mixed_content_on_one_line() {
        let doc = Document::parse("<p>hello <b>world</b></p>");
        let p = doc
            .elements()
            .find(|e| e.local_name() == Some("p"))
            .unwrap();
        assert_eq!(p.pretty(), "<p>hello <b>world</b></p>\n");
    }

    #[test]
    fn document_serialisation_round_trips_the_doctype() {
        let doc = Document::parse("<!DOCTYPE html><html><body><p>x</p></body></html>");
        assert_eq!(
            doc.html(),
            "<!DOCTYPE html><html><head></head><body><p>x</p></body></html>"
        );
    }

    #[test]
    fn comments_serialise_when_kept() {
        let doc = Document::parse_with(
            "<div><!-- note --></div>",
            &ParseOptions::default().keep_comments(true),
        );
        let div = doc.body().and_then(|b| b.first_element_child()).unwrap();
        assert_eq!(div.outer_html(), "<div><!-- note --></div>");
    }

    #[test]
    fn debug_uses_clean_spaces_and_truncation() {
        let doc = Document::parse("<div>\n  <p>hello</p>\n</div>");
        let p = doc
            .elements()
            .find(|e| e.local_name() == Some("p"))
            .unwrap();
        assert_eq!(
            format!("{p:?}"),
            "<data='<p>hello</p>' parent='<div><p>hello</p></div>'>"
        );
        let text = p.first_child().unwrap();
        assert_eq!(format!("{text:?}"), "<text='hello'>");
        let html_el = doc.root_element().unwrap();
        assert!(format!("{html_el:?}").ends_with("'>"));
        assert!(!format!("{html_el:?}").contains("parent="));
    }

    #[test]
    fn display_is_outer_html() {
        let doc = Document::parse("<p>x</p>");
        let p = doc
            .elements()
            .find(|e| e.local_name() == Some("p"))
            .unwrap();
        assert_eq!(p.to_string(), "<p>x</p>");
    }
}
