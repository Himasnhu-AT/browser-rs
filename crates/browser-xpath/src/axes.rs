//! All 13 axes, and the node tests applied to them.
//!
//! Every function here is generic over [`TreeModel`], so the engine never touches
//! `browser-dom` directly.
//!
//! # Axis order is not document order
//!
//! [`axis_nodes`] returns nodes in **axis order**: forward axes in document order,
//! the four reverse axes (`ancestor`, `ancestor-or-self`, `preceding`,
//! `preceding-sibling`) nearest-first. That is what predicates need — a node's
//! `position()` is its *proximity position*, which counts along the axis, so
//! `preceding-sibling::*[1]` is the nearest preceding sibling and not the first
//! one in the document. [`crate::eval`] re-sorts into document order after the
//! predicates have run.

use crate::ast::{Axis, NodeTest};
use crate::error::Result;
use crate::eval::Namespaces;
use crate::model::{NodeKind, TreeModel};

/// The nodes on `axis` from `node`, in axis order.
///
/// Attribute and namespace nodes are treated as living *at* their element for the
/// `following`, `preceding`, `following-sibling` and `preceding-sibling` axes —
/// which is what `libxml2` does: `//a/@href/following::*` is everything after the
/// `<a>` element, excluding `<a>`'s own descendants.
#[must_use]
pub fn axis_nodes<M: TreeModel>(model: &M, node: M::Node, axis: Axis) -> Vec<M::Node> {
    match axis {
        Axis::SelfAxis => vec![node],
        Axis::Child => model.children(node).collect(),
        Axis::Parent => model.parent(node).into_iter().collect(),
        Axis::Attribute => model.attributes(node).collect(),
        Axis::Namespace => model.namespaces(node).collect(),
        Axis::Descendant => descendants(model, node),
        Axis::DescendantOrSelf => {
            let mut out = vec![node];
            out.extend(descendants(model, node));
            out
        }
        Axis::Ancestor => ancestors(model, node),
        Axis::AncestorOrSelf => {
            let mut out = vec![node];
            out.extend(ancestors(model, node));
            out
        }
        // Attribute and namespace nodes are not among their element's children,
        // so `siblings_*` correctly reports that they have no siblings at all —
        // which is what `libxml2` answers for `//a/@href/following-sibling::*`.
        Axis::FollowingSibling => siblings_after(model, node),
        Axis::PrecedingSibling => {
            let mut out = siblings_before(model, node);
            out.reverse(); // nearest first — the axis is a reverse axis
            out
        }
        Axis::Following => following(model, anchor(model, node)),
        Axis::Preceding => preceding(model, anchor(model, node)),
    }
}

/// Where a node sits for the following / preceding axes: an attribute or namespace
/// node sits at its element.
fn anchor<M: TreeModel>(model: &M, node: M::Node) -> M::Node {
    match model.kind(node) {
        NodeKind::Attr | NodeKind::Namespace => model.parent(node).unwrap_or(node),
        _ => node,
    }
}

/// Every descendant, in document order, excluding `node` itself.
///
/// Iterative on purpose: an HTML document can nest thousands of elements deep and
/// a recursive walk would blow the stack.
#[must_use]
pub fn descendants<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let mut out = Vec::new();
    let mut stack: Vec<M::Node> = model.children(node).collect();
    stack.reverse();
    while let Some(current) = stack.pop() {
        out.push(current);
        let mut children: Vec<M::Node> = model.children(current).collect();
        children.reverse();
        stack.extend(children);
    }
    out
}

/// `node` and every descendant, in **reverse** document order.
fn subtree_reversed<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let mut out = vec![node];
    out.extend(descendants(model, node));
    out.reverse();
    out
}

/// Every ancestor, nearest first.
fn ancestors<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let mut out = Vec::new();
    let mut current = model.parent(node);
    while let Some(ancestor) = current {
        out.push(ancestor);
        current = model.parent(ancestor);
    }
    out
}

/// The siblings after `node`, in document order.
fn siblings_after<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let Some(parent) = model.parent(node) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = false;
    for child in model.children(parent) {
        if seen {
            out.push(child);
        } else if child == node {
            seen = true;
        }
    }
    // `seen` stays false for attribute and namespace nodes, which are not among
    // their element's children — and which therefore have no siblings.
    if seen {
        out
    } else {
        Vec::new()
    }
}

/// The siblings before `node`, in document order.
fn siblings_before<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let Some(parent) = model.parent(node) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for child in model.children(parent) {
        if child == node {
            return out;
        }
        out.push(child);
    }
    Vec::new()
}

/// Everything after `node` in document order, excluding its descendants (and, by
/// construction, its ancestors).
fn following<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let mut out = Vec::new();
    let mut current = Some(node);
    while let Some(level) = current {
        for sibling in siblings_after(model, level) {
            out.push(sibling);
            out.extend(descendants(model, sibling));
        }
        current = model.parent(level);
    }
    out
}

/// Everything before `node` in document order, excluding its ancestors, nearest
/// first.
fn preceding<M: TreeModel>(model: &M, node: M::Node) -> Vec<M::Node> {
    let mut out = Vec::new();
    let mut current = Some(node);
    while let Some(level) = current {
        let mut before = siblings_before(model, level);
        before.reverse();
        for sibling in before {
            out.extend(subtree_reversed(model, sibling));
        }
        current = model.parent(level);
    }
    out
}

/// Whether `node` satisfies `test` on `axis`.
///
/// # Name tests are matched on the local name
///
/// An unprefixed name test matches by local name whatever the node's namespace.
/// Strict XPath 1.0 would match only nodes in *no* namespace — but html5ever puts
/// every HTML element in the XHTML namespace and every SVG element in the SVG one,
/// while `libxml2`'s HTML parser (the reference implementation for this port) puts
/// them in no namespace at all. Matching on the local name makes `//div`,
/// `//svg`, `//a/@href` behave identically on both trees, which is the whole
/// point. A *prefixed* test still compares the resolved namespace URI.
///
/// One consequence, shared with `libxml2`: html5ever keeps `xml:lang` on an *HTML*
/// element as a single un-namespaced attribute whose local name is literally
/// `xml:lang`, so `@xml:lang` does not match it — exactly as in lxml, where the
/// same expression finds nothing. Inside SVG/MathML, where html5ever does apply
/// the HTML5 "adjust foreign attributes" step, `@xml:lang` matches normally.
/// `@*[local-name() = 'xml:lang']` reaches the HTML spelling.
///
/// # Errors
///
/// [`crate::error::XPathError::UndefinedPrefix`] when a prefixed name test uses a
/// prefix that is not bound.
pub fn matches_test<M: TreeModel>(
    model: &M,
    node: M::Node,
    test: &NodeTest,
    axis: Axis,
    namespaces: &Namespaces,
) -> Result<bool> {
    let kind = model.kind(node);
    Ok(match test {
        NodeTest::Node => true,
        NodeTest::Text => kind == NodeKind::Text,
        NodeTest::Comment => kind == NodeKind::Comment,
        NodeTest::ProcessingInstruction(target) => {
            kind == NodeKind::Pi
                && match target {
                    None => true,
                    Some(target) => model.name(node).is_some_and(|(_, local)| local == target),
                }
        }
        NodeTest::Wildcard => kind == axis.principal_node_kind(),
        NodeTest::NamespaceWildcard(prefix) => {
            let uri = namespaces.resolve(prefix)?;
            kind == axis.principal_node_kind() && model.name(node).is_some_and(|(ns, _)| ns == uri)
        }
        NodeTest::Name { prefix, local } => {
            if kind != axis.principal_node_kind() {
                return Ok(false);
            }
            let Some((node_ns, node_local)) = model.name(node) else {
                return Ok(false);
            };
            match prefix {
                // XPath 1.0 §2.3: an unprefixed name test matches the null
                // namespace on the attribute axis. Element names are matched by
                // local name alone, because libxml2's HTML parser puts every
                // element — `svg` and `math` included — in no namespace, so
                // `//svg//rect` has to keep working; but `@href` must not reach
                // an `xlink:href`, which lxml only answers to `@xlink:href`.
                None if axis == Axis::Attribute => node_ns.is_empty() && node_local == local,
                None => node_local == local,
                Some(prefix) => node_ns == namespaces.resolve(prefix)? && node_local == local,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use browser_dom::Document;

    use super::*;
    use crate::model::{DomModel, XNode};

    const PAGE: &str =
        "<html><body><div id='d'><a href='/1'>one</a><b>B</b></div><p>after</p></body></html>";

    fn model_and(html: &str) -> Document {
        Document::parse(html)
    }

    fn element<'a>(doc: &'a Document, tag: &str) -> XNode<'a> {
        XNode::new(doc.elements().find(|e| e.tag() == tag).expect(tag))
    }

    fn tags<M: TreeModel>(model: &M, nodes: &[M::Node]) -> Vec<String> {
        nodes
            .iter()
            .map(|n| match model.kind(*n) {
                NodeKind::Element | NodeKind::Attr | NodeKind::Namespace => {
                    model.name(*n).map_or("?".to_owned(), |(_, l)| l.to_owned())
                }
                NodeKind::Text => format!("'{}'", model.string_value(*n)),
                other => other.as_str().to_owned(),
            })
            .collect()
    }

    #[test]
    fn forward_axes_walk_in_document_order() {
        let doc = model_and(PAGE);
        let model = DomModel::new(&doc);
        let link = element(&doc, "a");

        assert_eq!(
            tags(&model, &axis_nodes(&model, link, Axis::SelfAxis)),
            ["a"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, link, Axis::Child)),
            ["'one'"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, link, Axis::Parent)),
            ["div"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, link, Axis::Attribute)),
            ["href"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, link, Axis::FollowingSibling)),
            ["b"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, link, Axis::Following)),
            ["b", "'B'", "p", "'after'"]
        );
        let div = element(&doc, "div");
        assert_eq!(
            tags(&model, &axis_nodes(&model, div, Axis::Descendant)),
            ["a", "'one'", "b", "'B'"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, div, Axis::DescendantOrSelf)),
            ["div", "a", "'one'", "b", "'B'"]
        );
    }

    #[test]
    fn reverse_axes_walk_nearest_first() {
        let doc = model_and("<html><body><div><a></a><b></b><i></i><u></u></div></body></html>");
        let model = DomModel::new(&doc);
        let last = element(&doc, "u");

        assert_eq!(
            tags(&model, &axis_nodes(&model, last, Axis::PrecedingSibling)),
            ["i", "b", "a"]
        );
        let ancestors = axis_nodes(&model, last, Axis::Ancestor);
        assert_eq!(&tags(&model, &ancestors)[..3], ["div", "body", "html"]);
        assert_eq!(
            model.kind(*ancestors.last().expect("the document node")),
            NodeKind::Root,
            "the last ancestor is the document node"
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, last, Axis::Preceding)),
            ["i", "b", "a", "head"]
        );
        assert_eq!(
            &tags(&model, &axis_nodes(&model, last, Axis::AncestorOrSelf))[..2],
            ["u", "div"]
        );
    }

    #[test]
    fn preceding_visits_whole_subtrees_in_reverse() {
        let doc = model_and("<html><body><div><a>1</a><b>2</b></div><p>3</p></body></html>");
        let model = DomModel::new(&doc);
        let paragraph = element(&doc, "p");
        assert_eq!(
            tags(&model, &axis_nodes(&model, paragraph, Axis::Preceding)),
            // html5ever always builds a <head>, so it precedes everything in <body>.
            ["'2'", "b", "'1'", "a", "div", "head"]
        );
        // Ancestors are never on the preceding axis.
        let nodes = axis_nodes(&model, paragraph, Axis::Preceding);
        assert!(!nodes.contains(&element(&doc, "body")));
    }

    #[test]
    fn attribute_nodes_sit_at_their_element() {
        let doc = model_and(PAGE);
        let model = DomModel::new(&doc);
        let link = element(&doc, "a");
        let href = axis_nodes(&model, link, Axis::Attribute)[0];

        // `libxml2`: //a/@href/following::* -> b, p
        assert_eq!(
            tags(&model, &axis_nodes(&model, href, Axis::Following)),
            ["b", "'B'", "p", "'after'"]
        );
        assert!(axis_nodes(&model, href, Axis::FollowingSibling).is_empty());
        assert!(axis_nodes(&model, href, Axis::PrecedingSibling).is_empty());
        assert!(axis_nodes(&model, href, Axis::Child).is_empty());
        assert!(axis_nodes(&model, href, Axis::Descendant).is_empty());
        assert!(axis_nodes(&model, href, Axis::Attribute).is_empty());
        // …but the attribute's parent, and therefore its ancestors, do exist.
        assert_eq!(
            &tags(&model, &axis_nodes(&model, href, Axis::Ancestor))[..2],
            ["a", "div"]
        );
        assert_eq!(
            tags(&model, &axis_nodes(&model, href, Axis::SelfAxis)),
            ["href"]
        );
    }

    #[test]
    fn node_tests_select_by_kind_and_name() {
        let doc = Document::parse_with(
            "<html><body><div id='d'>text<!--c--><a/></div></body></html>",
            &browser_dom::ParseOptions::new().keep_comments(true),
        );
        let model = DomModel::new(&doc);
        let namespaces = Namespaces::new();
        let div = element(&doc, "div");
        let children = axis_nodes(&model, div, Axis::Child);

        let test = |test: &NodeTest, node| {
            matches_test(&model, node, test, Axis::Child, &namespaces).expect("no prefixes")
        };
        assert!(test(&NodeTest::Text, children[0]));
        assert!(!test(&NodeTest::Comment, children[0]));
        assert!(test(&NodeTest::Comment, children[1]));
        assert!(test(&NodeTest::Node, children[1]));
        assert!(test(&NodeTest::Wildcard, children[2]));
        assert!(!test(&NodeTest::Wildcard, children[0]));
        assert!(test(
            &NodeTest::Name {
                prefix: None,
                local: "a".into()
            },
            children[2]
        ));
        assert!(!test(
            &NodeTest::Name {
                prefix: None,
                local: "b".into()
            },
            children[2]
        ));

        // On the attribute axis, `*` is every attribute.
        let attrs = axis_nodes(&model, div, Axis::Attribute);
        assert!(matches_test(
            &model,
            attrs[0],
            &NodeTest::Wildcard,
            Axis::Attribute,
            &namespaces
        )
        .expect("no prefixes"));
        // …and never an element.
        assert!(!matches_test(
            &model,
            children[2],
            &NodeTest::Wildcard,
            Axis::Attribute,
            &namespaces
        )
        .expect("no prefixes"));
    }

    #[test]
    fn prefixed_name_tests_need_a_binding() {
        let doc = model_and(
            "<html><body><svg xmlns='http://www.w3.org/2000/svg'><rect/></svg></body></html>",
        );
        let model = DomModel::new(&doc);
        let rect = element(&doc, "rect");
        let test = NodeTest::Name {
            prefix: Some("svg".into()),
            local: "rect".into(),
        };

        let empty = Namespaces::new();
        assert!(matches_test(&model, rect, &test, Axis::Child, &empty).is_err());

        let mut bound = Namespaces::new();
        bound.insert("svg", "http://www.w3.org/2000/svg");
        assert!(matches_test(&model, rect, &test, Axis::Child, &bound).expect("bound"));

        let mut wrong = Namespaces::new();
        wrong.insert("svg", "http://example.com/");
        assert!(!matches_test(&model, rect, &test, Axis::Child, &wrong).expect("bound"));

        // The unprefixed test matches by local name whatever the namespace.
        assert!(matches_test(
            &model,
            rect,
            &NodeTest::Name {
                prefix: None,
                local: "rect".into()
            },
            Axis::Child,
            &empty
        )
        .expect("no prefixes"));
    }

    #[test]
    fn processing_instruction_tests_match_the_target() {
        // html5ever turns `<?pi?>` into a bogus comment, exactly as libxml2's HTML
        // parser does, so a PI node has to be built to be tested.
        let mut doc = Document::parse("<html><body><div/></body></html>");
        let div = doc
            .elements()
            .find(|e| e.tag() == "div")
            .expect("<div>")
            .id();
        doc.append_child(
            div,
            browser_dom::NodeData::ProcessingInstruction {
                target: "php".into(),
                data: "echo".into(),
            },
        )
        .expect("appended");
        let model = DomModel::new(&doc);
        let namespaces = Namespaces::new();
        let pi = axis_nodes(&model, element(&doc, "div"), Axis::Child)[0];

        assert_eq!(model.kind(pi), NodeKind::Pi);
        assert_eq!(model.string_value(pi), "echo");
        assert!(matches_test(
            &model,
            pi,
            &NodeTest::ProcessingInstruction(None),
            Axis::Child,
            &namespaces
        )
        .expect("no prefixes"));
        assert!(matches_test(
            &model,
            pi,
            &NodeTest::ProcessingInstruction(Some("php".into())),
            Axis::Child,
            &namespaces
        )
        .expect("no prefixes"));
        assert!(!matches_test(
            &model,
            pi,
            &NodeTest::ProcessingInstruction(Some("asp".into())),
            Axis::Child,
            &namespaces
        )
        .expect("no prefixes"));
        assert!(
            !matches_test(&model, pi, &NodeTest::Wildcard, Axis::Child, &namespaces)
                .expect("no prefixes")
        );
        assert!(
            matches_test(&model, pi, &NodeTest::Node, Axis::Child, &namespaces)
                .expect("no prefixes")
        );
    }

    #[test]
    fn namespace_axis_yields_in_scope_declarations() {
        let doc = model_and(
            "<html><body><svg xmlns='http://www.w3.org/2000/svg'><rect/></svg></body></html>",
        );
        let model = DomModel::new(&doc);
        let namespaces = Namespaces::new();
        let rect = element(&doc, "rect");
        let nodes = axis_nodes(&model, rect, Axis::Namespace);
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().all(|n| model.kind(*n) == NodeKind::Namespace));
        assert!(matches_test(
            &model,
            nodes[0],
            &NodeTest::Wildcard,
            Axis::Namespace,
            &namespaces
        )
        .expect("no prefixes"));
        // A namespace node is not an element, so `child::*` never sees one.
        assert!(!matches_test(
            &model,
            nodes[0],
            &NodeTest::Wildcard,
            Axis::Child,
            &namespaces
        )
        .expect("no prefixes"));
    }
}
