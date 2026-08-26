//! Does this API actually carry the algorithms units 5/6/8/10 have to write?
//!
//! Each test re-implements a downstream algorithm on top of the public DOM API
//! only, and checks it against a behaviour `parser.md` pins for the Python
//! implementation. If one of these stops compiling, the DOM API lost something
//! a consumer needs.

use std::collections::HashSet;

use browser_dom::node::NodeRef;
use browser_dom::parse::ParseOptions;
use browser_dom::tree::Document;
use ego_tree::NodeId;

/// `Selector.get_all_text` (parser.md §3.2), written against the DOM API.
///
/// Note what is *not* here: no tail/text distinction. Python needs
/// `owner = parent.getparent() if text_node.is_tail else parent`; in this tree
/// the owner is always `text_node.parent()`.
fn get_all_text(
    node: NodeRef<'_>,
    separator: &str,
    strip: bool,
    ignore_tags: &[&str],
    valid_values: bool,
) -> String {
    let mut ignored: HashSet<NodeId> = HashSet::new();
    if !ignore_tags.is_empty() {
        for element in node.descendants_and_self() {
            if element
                .local_name()
                .is_some_and(|name| ignore_tags.contains(&name))
            {
                ignored.extend(element.descendants_and_self().map(|n| n.id()));
            }
        }
    }

    let mut parts: Vec<&str> = Vec::new();
    for text_node in node.descendant_text_nodes() {
        let Some(text) = text_node.as_text() else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let Some(owner) = text_node.parent() else {
            continue;
        };
        if ignored.contains(&owner.id()) {
            continue;
        }
        let processed = if strip { text.trim() } else { text };
        if !valid_values || !processed.trim().is_empty() {
            parts.push(processed);
        }
    }
    parts.join(separator)
}

#[test]
fn get_all_text_preserves_interleaved_text_nodes() {
    let html = r"
        <html>
        <body>
            <main>
                string1
                <b>string2</b>
                string3
                <div>
                    <span>string4</span>
                </div>
                string5
                <script>ignored</script>
                string6
                <style>ignored</style>
                string7
            </main>
        </body>
        </html>
    ";
    let doc = Document::parse(html);
    let main = doc
        .elements()
        .find(|e| e.local_name() == Some("main"))
        .expect("main");
    assert_eq!(
        get_all_text(main, "\n", true, &["script", "style"], true),
        "string1\nstring2\nstring3\nstring4\nstring5\nstring6\nstring7"
    );
}

#[test]
fn get_all_text_keeps_text_after_a_comment() {
    let doc = Document::parse_with(
        "<div>before<!-- c -->after<p>x</p></div>",
        &ParseOptions::default().keep_comments(true),
    );
    assert_eq!(
        get_all_text(doc.root(), "\n", false, &["script", "style"], true),
        "before\nafter\nx"
    );
}

#[test]
fn get_all_text_skips_ignored_subtrees_however_deep() {
    let depth = 500;
    let mut html = String::from("<div>");
    for i in 0..depth {
        html.push_str(&format!("<span>t{i}"));
    }
    html.push_str("leaf");
    for _ in 0..depth {
        html.push_str("</span>");
    }
    html.push_str("end</div>");
    let doc = Document::parse(&html);
    let text = get_all_text(doc.root(), "\n", true, &["script", "style"], true);
    assert!(text.starts_with("t0\nt1\n"), "{}", &text[..20]);
    assert!(text.contains("leaf"));
    assert!(text.ends_with("end"));
    assert_eq!(text.matches('\n').count(), depth);

    let mut ignored = String::from("<div>keep<script>");
    for _ in 0..depth {
        ignored.push_str("<b>x</b>");
    }
    ignored.push_str("</script>done</div>");
    let doc = Document::parse(&ignored);
    assert_eq!(
        get_all_text(doc.root(), "\n", true, &["script", "style"], true),
        "keep\ndone"
    );
}

#[test]
fn get_all_text_option_matrix() {
    let doc = Document::parse("<div> a <b> b </b><table><td>Cell</td></table></div>");
    let root = doc.root();
    assert!(get_all_text(root, " | ", true, &["table"], true).contains("a | b"));
    assert!(!get_all_text(root, " | ", true, &["table"], true).contains("Cell"));
    assert!(!get_all_text(root, "\n", false, &[], false).is_empty());
}

/// `_StorageTools.element_to_dict` (parser.md §4.1), written against the DOM API.
#[test]
fn element_fingerprint_can_be_assembled() {
    let doc = Document::parse_with(
        r#"<div id="wrap" data-x=" v "><!--c--><p class="a">text</p><span></span></div>"#,
        &ParseOptions::default().keep_comments(true),
    );
    let p = doc
        .elements()
        .find(|e| e.local_name() == Some("p"))
        .expect("p");

    // tag / attributes (cleaned: empty values dropped, values stripped)
    assert_eq!(p.tag(), "p");
    let attributes: Vec<(&str, String)> = p
        .attrs()
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| (k, v.trim().to_owned()))
        .collect();
    assert_eq!(attributes, [("class", "a".to_owned())]);

    // text: element.text, stripped, None when blank
    assert_eq!(p.text().map(str::trim), Some("text"));

    // path: root-first chain of tag names including the element itself
    let mut path: Vec<&str> = p.ancestors().filter_map(|a| a.local_name()).collect();
    path.reverse();
    path.push("p");
    assert_eq!(path, ["html", "body", "div", "p"]);

    // parent_name / parent_attribs (uncleaned) / parent_text
    let parent = p.parent_element().expect("parent");
    assert_eq!(parent.tag(), "div");
    assert_eq!(
        parent.attrs().iter().collect::<Vec<_>>(),
        [("id", "wrap"), ("data-x", " v ")]
    );
    assert_eq!(parent.text(), None);

    // siblings: lxml's iterchildren, which does NOT filter comments
    let siblings: Vec<&str> = p
        .siblings()
        .filter(|n| !n.is_text())
        .map(|n| n.tag())
        .collect();
    assert_eq!(siblings, ["#comment", "span"]);

    // children: filtered by html_forbidden (comments)
    let children: Vec<&str> = parent.non_comment_children().map(|n| n.tag()).collect();
    assert_eq!(children, ["p", "span"]);
}

/// `SelectorsGeneration._general_selection` (parser.md §3.11), CSS flavour.
fn css_path(node: NodeRef<'_>, full: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut target = node;
    loop {
        if target.parent_element().is_none() {
            break;
        }
        match target.element_id() {
            Some(id) if !id.is_empty() => {
                parts.push(format!("#{id}"));
                if !full {
                    parts.reverse();
                    return parts.join(" > ");
                }
            }
            _ => {
                let mut part = target.local_name().unwrap_or_default().to_owned();
                let nth = target.nth_of_type();
                if nth > 1 {
                    part.push_str(&format!(":nth-of-type({nth})"));
                }
                parts.push(part);
            }
        }
        let Some(parent) = target.parent_element() else {
            break;
        };
        target = parent;
        if target.local_name() == Some("html") {
            break;
        }
    }
    parts.reverse();
    parts.join(" > ")
}

#[test]
fn selector_generation_walks_work() {
    let html = r#"<body><div><div><span>a</span><span id="target">b</span></div></div></body>"#;
    let doc = Document::parse(html);
    let target = doc
        .elements()
        .find(|e| e.element_id() == Some("target"))
        .expect("target");

    assert_eq!(css_path(target, false), "#target");
    let full = css_path(target, true);
    assert_eq!(full, "body > div > div > #target");
    assert_eq!(full.matches("#target").count(), 1);

    let first_span = doc
        .elements()
        .find(|e| e.local_name() == Some("span"))
        .expect("span");
    assert_eq!(css_path(first_span, true), "body > div > div > span");
    assert_eq!(target.nth_of_type(), 2);
}

/// `find_similar`'s hard structural filter (parser.md §3.7): same tag chain,
/// same depth, whole document, receiver excluded.
#[test]
fn find_similar_structural_filter_can_be_expressed() {
    let html = r#"
        <main><div class="wrap">
            <article class="product"><span class="price">$10</span></article>
            <article class="product"><span class="price">$20</span></article>
            <article class="product"><span class="price">$30</span></article>
            <section class="product">not an article</section>
        </div></main>"#;
    let doc = Document::parse(html);
    let first = doc
        .elements()
        .find(|e| e.local_name() == Some("article"))
        .expect("article");

    let depth = first.depth();
    let chain: Vec<&str> = {
        let mut chain = vec![first.local_name().unwrap_or_default()];
        if let Some(parent) = first.parent_element() {
            chain.insert(0, parent.local_name().unwrap_or_default());
            if let Some(grand) = parent.parent_element() {
                chain.insert(0, grand.local_name().unwrap_or_default());
            }
        }
        chain
    };
    assert_eq!(chain, ["main", "div", "article"]);

    let candidates: Vec<NodeRef<'_>> = doc
        .elements()
        .filter(|el| el.depth() == depth && *el != first)
        .filter(|el| {
            let mut names = vec![el.local_name().unwrap_or_default()];
            let mut cursor = *el;
            for _ in 0..2 {
                match cursor.parent_element() {
                    Some(parent) => {
                        names.insert(0, parent.local_name().unwrap_or_default());
                        cursor = parent;
                    }
                    None => break,
                }
            }
            names == chain
        })
        .collect();
    assert_eq!(candidates.len(), 2);
}

/// The XPath engine needs stable document order over mixed node kinds.
#[test]
fn document_order_holds_across_node_kinds() {
    let doc = Document::parse_with(
        "<div>t1<!--c1--><p>t2</p>t3<span><!--c2-->t4</span></div>",
        &ParseOptions::default().keep_comments(true),
    );
    let ordered: Vec<String> = doc
        .root()
        .descendants()
        .map(|n| match n.as_text() {
            Some(text) => text.to_owned(),
            None => n.tag().to_owned(),
        })
        .collect();
    let interesting: Vec<&str> = ordered
        .iter()
        .map(String::as_str)
        .filter(|s| s.starts_with('t') || *s == "#comment" || *s == "p" || *s == "span")
        .collect();
    assert_eq!(
        interesting,
        ["t1", "#comment", "p", "t2", "t3", "span", "#comment", "t4"]
    );

    let nodes: Vec<NodeRef<'_>> = doc.root().descendants().collect();
    for window in nodes.windows(2) {
        assert_eq!(
            window[0].cmp_document_order(&window[1]),
            std::cmp::Ordering::Less,
            "{:?} should precede {:?}",
            window[0],
            window[1]
        );
    }
}

/// `strip_noise_tags` / `sanitize_for_ai` from `browser-markdown`.
#[test]
fn markdown_style_sanitisation_is_expressible() {
    let mut doc = Document::parse(
        r#"<body><script>x</script><div aria-hidden="true">hidden</div><p style="display:none">gone</p><p>keep</p><noscript>n</noscript></body>"#,
    );
    doc.remove_elements_named(&["script", "style", "noscript", "svg"]);
    doc.remove_matching(|node| {
        node.attr("aria-hidden") == Some("true")
            || node
                .attr("style")
                .is_some_and(|s| s.replace(' ', "").contains("display:none"))
    });
    assert_eq!(
        doc.body().map(|b| b.inner_html()),
        Some("<p>keep</p>".to_owned())
    );
}

/// `_StorageTools.element_to_dict`'s sibling logic (unit 10) counts an
/// element's position among the children lxml's `iterchildren()` yields:
/// elements, comments and processing instructions, never text.
///
/// The three child iterators are deliberately different, and this pins all
/// three against the same tree.
#[test]
fn iterchildren_matches_lxmls_child_sequence() {
    let doc = Document::parse_with(
        "<div>t0<!--c--><p>a</p>t1<span></span> </div>",
        &ParseOptions::new().keep_comments(true),
    );
    let div = doc
        .elements()
        .find(|e| e.tag() == "div")
        .expect("the <div>");

    // lxml: `[<!--c-->, <p>, <span>]` — `list(div)` and `div.iterchildren()`.
    assert_eq!(
        div.iterchildren().map(|n| n.tag()).collect::<Vec<_>>(),
        ["#comment", "p", "span"]
    );
    // Elements only.
    assert_eq!(
        div.element_children().map(|n| n.tag()).collect::<Vec<_>>(),
        ["p", "span"]
    );
    // Everything but comments — text included, which is the difference.
    assert_eq!(
        div.non_comment_children()
            .map(|n| n.tag())
            .collect::<Vec<_>>(),
        ["#text", "p", "#text", "span", "#text"]
    );

    // The index within `iterchildren()` is what `element_to_dict` stores.
    let span = doc
        .elements()
        .find(|e| e.tag() == "span")
        .expect("the <span>");
    assert_eq!(
        div.iterchildren().position(|n| n == span),
        Some(2),
        "the comment counts, the text does not"
    );
}
