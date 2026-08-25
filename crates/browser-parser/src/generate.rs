//! The four selector-generation properties — `SelectorsGeneration` in Python.
//!
//! All four delegate to one walk, [`general_selection`]:
//!
//! | property | call |
//! |---|---|
//! | `generate_css_selector` | `general_selection(node, css = true, full_path = false)` |
//! | `generate_full_css_selector` | `general_selection(node, css = true, full_path = true)` |
//! | `generate_xpath_selector` | `general_selection(node, css = false, full_path = false)` |
//! | `generate_full_xpath_selector` | `general_selection(node, css = false, full_path = true)` |
//!
//! The walk climbs from the node towards `<html>`, emitting one segment per
//! level:
//!
//! * an element carrying a **non-empty `id`** emits `#id` (CSS) or `[@id='id']`
//!   (XPath), and in the *short* form that ends the walk immediately — which is
//!   why a short XPath comes out as `//*[@id='x']/…`, with the `//*` prefix
//!   supplying the element the predicate applies to;
//! * otherwise the tag name, plus `:nth-of-type(n)` / `[n]` when the element is
//!   not the first of its tag among its parent's
//!   [`children`](crate::node::Node::children);
//! * the walk stops **at** `<html>`, so a full path starts at `body`.
//!
//! Classes are deliberately unused: some sites reuse identical class sets
//! across elements, and the Python source comments the branch out.
//!
//! ```
//! use browser_parser::document::Document;
//!
//! let doc = Document::parse(
//!     "<div id='wrap'><section><article><p><span id='target'>t</span></p></article></section></div>",
//! );
//! let target = doc.css("#target").unwrap().first().cloned().unwrap();
//!
//! assert_eq!(target.css_path(), "#target");
//! assert_eq!(target.xpath_path(), "//*[@id='target']");
//! assert_eq!(target.css_path_full(), "body > #wrap > section > article > p > #target");
//! assert_eq!(
//!     target.xpath_path_full(),
//!     "//body/*[@id='wrap']/section/article/p/*[@id='target']"
//! );
//!
//! // The document root has no parent and yields the degenerate answers.
//! assert_eq!(doc.root().css_path(), "");
//! assert_eq!(doc.root().xpath_path(), "//");
//! ```

use crate::node::Node;

/// `SelectorsGeneration._general_selection`.
///
/// `css = false` produces XPath. A string node yields `""`.
#[must_use]
pub fn general_selection(node: &Node<'_>, css: bool, full_path: bool) -> String {
    if node.is_text_node() {
        return String::new();
    }

    let mut segments: Vec<String> = Vec::new();
    let mut target = node.clone();

    while let Some(parent) = target.parent() {
        match target.attr("id").filter(|id| !id.is_empty()) {
            Some(id) => {
                let segment = if css {
                    format!("#{id}")
                } else if full_path {
                    format!("*[@id='{id}']")
                } else {
                    format!("[@id='{id}']")
                };
                segments.push(segment);
                if !full_path {
                    // `id` is enough: stop here, and prefix the XPath with the
                    // `//*` the predicate needs.
                    return if css {
                        join_css(&segments)
                    } else {
                        format!("//*{}", join_segments(&segments))
                    };
                }
            }
            None => {
                let mut segment = target.tag().to_owned();
                let index = position_among_siblings(&parent, &target);
                if index > 1 {
                    if css {
                        segment.push_str(&format!(":nth-of-type({index})"));
                    } else {
                        segment.push_str(&format!("[{index}]"));
                    }
                }
                segments.push(segment);
            }
        }

        target = parent;
        if target.tag() == "html" {
            return finish(css, &segments);
        }
    }

    finish(css, &segments)
}

/// The 1-based index of `target` among the same-tag entries of
/// `parent.children()`, counted in document order.
///
/// This is `nth-of-type` computed over Python's `children` — lxml's
/// `iterchildren()` minus comments — so a comment between two `<p>`s does not
/// shift the index, while a processing instruction would.
fn position_among_siblings(parent: &Node<'_>, target: &Node<'_>) -> usize {
    let mut count = 0_usize;
    let tag = target.tag();
    for child in &parent.children() {
        if child.tag() == tag {
            count += 1;
        }
        if child == target {
            break;
        }
    }
    count.max(1)
}

fn join_segments(segments: &[String]) -> String {
    segments
        .iter()
        .rev()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("/")
}

fn join_css(segments: &[String]) -> String {
    segments
        .iter()
        .rev()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" > ")
}

fn finish(css: bool, segments: &[String]) -> String {
    if css {
        join_css(segments)
    } else {
        format!("//{}", join_segments(segments))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    const DEEP: &str = concat!(
        "<html><body><div id='wrap'><section><article><p>",
        "<span id='target'>t</span></p></article></section></div></body></html>",
    );

    #[test]
    fn short_paths_stop_at_the_nearest_id() {
        let doc = Document::parse(DEEP);
        let target = doc.css("#target").expect("selects")[0].clone();
        assert_eq!(target.css_path(), "#target");
        assert_eq!(target.xpath_path(), "//*[@id='target']");

        let article = doc.css("article").expect("selects")[0].clone();
        assert_eq!(article.css_path(), "#wrap > section > article");
        assert_eq!(article.xpath_path(), "//*[@id='wrap']/section/article");
    }

    #[test]
    fn full_paths_walk_to_body_without_duplicating_ids() {
        let doc = Document::parse(DEEP);
        let target = doc.css("#target").expect("selects")[0].clone();
        let css_full = target.css_path_full();
        let xpath_full = target.xpath_path_full();
        assert_eq!(css_full, "body > #wrap > section > article > p > #target");
        assert_eq!(
            xpath_full,
            "//body/*[@id='wrap']/section/article/p/*[@id='target']"
        );
        assert_eq!(css_full.matches("#target").count(), 1);
        assert_eq!(xpath_full.matches("@id='target'").count(), 1);
    }

    #[test]
    fn the_generated_selectors_re_select_exactly_one_element() {
        let doc = Document::parse(DEEP);
        for node in doc.css("*").expect("selects") {
            let short = node.css_path();
            if short.is_empty() {
                continue;
            }
            let hits = doc.css(&short).expect("generated CSS parses");
            assert!(!hits.is_empty(), "`{short}` selected nothing");
            let full = node.css_path_full();
            let hits = doc.css(&full).expect("generated CSS parses");
            assert_eq!(hits.len(), 1, "`{full}` selected {}", hits.len());
            assert_eq!(hits[0], node);

            let xpath_full = node.xpath_path_full();
            let hits = doc.xpath(&xpath_full).expect("generated XPath parses");
            assert_eq!(hits.len(), 1, "`{xpath_full}` selected {}", hits.len());
            assert_eq!(hits[0], node);
        }
    }

    #[test]
    fn positional_disambiguation_uses_nth_of_type() {
        let doc = Document::parse("<div><p>a</p><span>s</span><p>b</p><p>c</p></div>");
        let paragraphs = doc.css("p").expect("selects");
        assert_eq!(paragraphs[0].css_path(), "body > div > p");
        assert_eq!(paragraphs[1].css_path(), "body > div > p:nth-of-type(2)");
        assert_eq!(paragraphs[2].css_path(), "body > div > p:nth-of-type(3)");
        assert_eq!(paragraphs[1].xpath_path(), "//body/div/p[2]");
        // The <span> is the only one of its type, so no index.
        assert_eq!(
            doc.css("span").expect("selects")[0].css_path(),
            "body > div > span"
        );
    }

    #[test]
    fn comments_do_not_shift_the_index() {
        use crate::config::DocumentConfig;
        let doc = Document::parse_with(
            "<div><p>a</p><!-- c --><p>b</p></div>",
            &DocumentConfig::new().keep_comments(true),
        );
        let second = doc.css("p").expect("selects")[1].clone();
        assert_eq!(second.css_path(), "body > div > p:nth-of-type(2)");
    }

    #[test]
    fn an_empty_id_falls_through_to_the_tag_branch() {
        let doc = Document::parse("<div id=''><p>a</p></div>");
        let p = doc.css("p").expect("selects")[0].clone();
        assert_eq!(p.css_path(), "body > div > p");
    }

    #[test]
    fn the_root_element_yields_the_degenerate_answers() {
        let doc = Document::parse("<p>x</p>");
        assert_eq!(doc.root().css_path(), "");
        assert_eq!(doc.root().xpath_path(), "//");
        assert_eq!(doc.root().css_path_full(), "");
        assert_eq!(doc.root().xpath_path_full(), "//");
    }

    #[test]
    fn body_is_one_segment() {
        let doc = Document::parse("<p>x</p>");
        let body = doc.css("body").expect("selects")[0].clone();
        assert_eq!(body.css_path(), "body");
        assert_eq!(body.xpath_path(), "//body");
    }

    #[test]
    fn a_text_node_generates_nothing() {
        let doc = Document::parse("<p>x</p>");
        let text = doc.css("p::text").expect("selects")[0].clone();
        assert_eq!(text.css_path(), "");
        assert_eq!(text.css_path_full(), "");
        assert_eq!(text.xpath_path(), "");
        assert_eq!(text.xpath_path_full(), "");
    }

    #[test]
    fn general_selection_is_reachable_directly() {
        let doc = Document::parse(DEEP);
        let target = doc.css("#target").expect("selects")[0].clone();
        assert_eq!(general_selection(&target, true, false), target.css_path());
        assert_eq!(
            general_selection(&target, false, true),
            target.xpath_path_full()
        );
    }
}
