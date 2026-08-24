//! Malformed-input behaviour.
//!
//! libxml2's `recover=True` parser and html5ever disagree on broken markup;
//! `ARCHITECTURE.md` §11.6 accepts that and asks for the html5ever behaviour to
//! be *pinned* instead. These tests are that pin: they document what our tree
//! looks like for input no sane page contains, so a future html5ever bump that
//! changes recovery cannot slip through silently.

use browser_dom::node::NodeRef;
use browser_dom::parse::ParseOptions;
use browser_dom::tree::Document;

fn body_html(html: &str) -> String {
    Document::parse(html)
        .body()
        .map(|b| b.inner_html())
        .unwrap_or_default()
}

#[test]
fn unclosed_block_tags_become_siblings() {
    assert_eq!(
        body_html("<div><p>a<p>b</div>"),
        "<div><p>a</p><p>b</p></div>"
    );
}

#[test]
fn stray_end_tags_are_recovered() {
    assert_eq!(body_html("</p>text"), "text");
    assert_eq!(body_html("<div>a</span>b</div>"), "<div>ab</div>");
}

#[test]
fn mis_nested_inline_tags_run_the_adoption_agency() {
    // `<b><i>x</b>y</i>` reopens <i> outside the </b>, like every browser.
    assert_eq!(body_html("<b><i>x</b>y</i>"), "<b><i>x</i></b><i>y</i>");
}

#[test]
fn unclosed_tags_at_eof_are_closed_implicitly() {
    assert_eq!(body_html("<div><span>text"), "<div><span>text</span></div>");
}

#[test]
fn content_inside_a_table_is_foster_parented() {
    let html = body_html("<table><div>stray</div><tr><td>1</td></tr></table>");
    assert!(html.starts_with("<div>stray</div><table>"), "{html}");
    assert!(html.contains("<td>1</td>"), "{html}");
}

#[test]
fn duplicate_attributes_keep_the_first_and_valueless_attributes_are_empty() {
    let doc = Document::parse("<div class=a class=b hidden>x</div>");
    let div = doc
        .elements()
        .find(|e| e.local_name() == Some("div"))
        .expect("div");
    assert_eq!(div.attr("class"), Some("a"));
    assert_eq!(div.attr("hidden"), Some(""));
    assert!(div.has_class("a"));
}

#[test]
fn text_before_any_tag_lands_in_the_body() {
    assert_eq!(body_html("hello<div>x</div>"), "hello<div>x</div>");
}

#[test]
fn an_unterminated_comment_swallows_the_rest_of_the_document() {
    let doc = Document::parse_with(
        "<div>a<!-- never closed",
        &ParseOptions::default().keep_comments(true),
    );
    let div = doc
        .elements()
        .find(|e| e.local_name() == Some("div"))
        .expect("div");
    assert_eq!(div.text(), Some("a"));
    let comment = div.children().find(NodeRef::is_comment).expect("comment");
    assert_eq!(comment.as_comment(), Some(" never closed"));
    // and with the default options the whole thing simply disappears
    assert_eq!(body_html("<div>a<!-- never closed"), "<div>a</div>");
}

#[test]
fn markup_inside_script_is_raw_text() {
    let doc = Document::parse("<script>var x = '</div>';</script>");
    let script = doc
        .elements()
        .find(|e| e.local_name() == Some("script"))
        .expect("script");
    assert_eq!(script.text(), Some("var x = '</div>';"));
    // raw text is neither escaped on the way in nor on the way out
    assert_eq!(script.outer_html(), "<script>var x = '</div>';</script>");
}

#[test]
fn unknown_entities_are_left_alone_and_known_ones_decoded() {
    let doc = Document::parse("<p>&nosuchentity; &amp; &lt;tag&gt;</p>");
    let p = doc
        .elements()
        .find(|e| e.local_name() == Some("p"))
        .expect("p");
    assert_eq!(p.text(), Some("&nosuchentity; & <tag>"));
    // and they re-escape on the way out
    assert_eq!(
        p.outer_html(),
        "<p>&amp;nosuchentity; &amp; &lt;tag&gt;</p>"
    );
}

#[test]
fn garbage_angle_brackets_do_not_panic() {
    for input in [
        "<<>><p>x",
        "<>",
        "<!>",
        "<!DOCTYPE",
        "<a href=>x</a>",
        "<div <span>x</div>",
        "</>",
        "<3>not a tag",
        "<p>unclosed &",
    ] {
        let doc = Document::parse(input);
        assert!(doc.root_element().is_some(), "no root for {input:?}");
        let _ = doc.html();
        let _ = doc.pretty();
    }
}

#[test]
fn a_bare_fragment_is_wrapped_in_html_body() {
    // pinned by Python's test_find_all_keeps_filtering_on_a_blank_class
    let doc = Document::parse(r#"<div class="">empty</div><div class="a">a</div><div>none</div>"#);
    assert_eq!(
        doc.root_element().and_then(|e| e.local_name()),
        Some("html")
    );
    let divs: Vec<_> = doc
        .elements()
        .filter(|e| e.local_name() == Some("div"))
        .collect();
    assert_eq!(divs.len(), 3);
    assert_eq!(divs[0].attr("class"), Some(""));
    assert!(divs[0].as_element().is_some_and(|e| e.classes().is_empty()));
    assert!(divs[2].attr("class").is_none());
}

#[test]
fn attribute_values_survive_newlines_and_quotes() {
    let doc = Document::parse("<a title=\"one\ntwo\" data-x='say \"hi\"'>x</a>");
    let a = doc
        .elements()
        .find(|e| e.local_name() == Some("a"))
        .expect("a");
    assert_eq!(a.attr("title"), Some("one\ntwo"));
    assert_eq!(a.attr("data-x"), Some("say \"hi\""));
}

#[test]
fn nul_bytes_never_reach_the_tree() {
    let doc = Document::parse("<div id=\"a\0b\">c\0d</div>");
    let div = doc
        .elements()
        .find(|e| e.local_name() == Some("div"))
        .expect("div");
    assert_eq!(div.element_id(), Some("ab"));
    assert_eq!(div.text(), Some("cd"));
}

#[test]
fn svg_foreign_content_keeps_its_namespace() {
    let doc = Document::parse("<div><svg><path d=\"M0 0\"/></svg></div>");
    let path = doc
        .elements()
        .find(|e| e.local_name() == Some("path"))
        .expect("path");
    let name = path.as_element().expect("element").name();
    assert_eq!(name.ns.as_ref(), "http://www.w3.org/2000/svg");
    assert_eq!(path.attr("d"), Some("M0 0"));
}

#[test]
fn deeply_broken_nesting_still_serialises_round_trip_stable() {
    let input = "<div><p><div><span><p>x</span></div></p></div>";
    let first = Document::parse(input);
    let once = first.html();
    let twice = Document::parse(&once).html();
    assert_eq!(once, twice, "serialisation is not idempotent");
}
