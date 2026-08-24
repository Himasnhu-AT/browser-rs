//! Proof that the engine is not hard-wired to `browser-dom`.
//!
//! `ARCHITECTURE.md` §1.4 specifies the evaluator against a [`TreeModel`] trait.
//! This file implements that trait over a **completely different** tree — a flat
//! `Vec` arena with `usize` node handles, holding an XML-shaped document with real
//! namespaces, a processing instruction and a comment, none of which an HTML
//! parser can produce — and evaluates the same engine over it.
//!
//! It also exercises the trait's three defaulted methods
//! ([`TreeModel::namespaces`], [`TreeModel::root_of`] and
//! [`TreeModel::attribute_value`]), which this implementation deliberately does
//! **not** override.

use std::borrow::Cow;

use browser_xpath::eval::{Namespaces, Variables, XPath};
use browser_xpath::model::{NodeKind, TreeModel};

// ---------------------------------------------------------------------------
// A toy tree: the document below, as a flat arena.
//
// <?xml-stylesheet href="style.css"?>
// <catalog xmlns="urn:books">
//   <book id="b1" xml:lang="en"><title>First</title><price>10.50</price></book>
//   <!-- out of print -->
//   <book id="b2"><title>Second</title><price>20</price></book>
// </catalog>
// ---------------------------------------------------------------------------

const BOOKS: &str = "urn:books";
const XML: &str = "http://www.w3.org/XML/1998/namespace";

struct ToyNode {
    kind: NodeKind,
    /// `(namespace-uri, local-name)`.
    name: Option<(String, String)>,
    /// The node's own text: character data, a comment body or an attribute value.
    text: String,
    parent: Option<usize>,
    children: Vec<usize>,
    attributes: Vec<usize>,
}

struct ToyTree {
    nodes: Vec<ToyNode>,
}

impl ToyTree {
    fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    fn push(
        &mut self,
        kind: NodeKind,
        name: Option<(&str, &str)>,
        text: &str,
        parent: Option<usize>,
    ) -> usize {
        let id = self.nodes.len();
        self.nodes.push(ToyNode {
            kind,
            name: name.map(|(ns, local)| (ns.to_owned(), local.to_owned())),
            text: text.to_owned(),
            parent,
            children: Vec::new(),
            attributes: Vec::new(),
        });
        if let Some(parent) = parent {
            if kind == NodeKind::Attr {
                self.nodes[parent].attributes.push(id);
            } else {
                self.nodes[parent].children.push(id);
            }
        }
        id
    }

    /// Build the document above.
    fn catalog() -> Self {
        let mut tree = Self::new();
        let root = tree.push(NodeKind::Root, None, "", None);
        tree.push(
            NodeKind::Pi,
            Some(("", "xml-stylesheet")),
            "href=\"style.css\"",
            Some(root),
        );
        let catalog = tree.push(NodeKind::Element, Some((BOOKS, "catalog")), "", Some(root));

        let first = tree.push(NodeKind::Element, Some((BOOKS, "book")), "", Some(catalog));
        tree.push(NodeKind::Attr, Some(("", "id")), "b1", Some(first));
        tree.push(NodeKind::Attr, Some((XML, "lang")), "en", Some(first));
        let title = tree.push(NodeKind::Element, Some((BOOKS, "title")), "", Some(first));
        tree.push(NodeKind::Text, None, "First", Some(title));
        let price = tree.push(NodeKind::Element, Some((BOOKS, "price")), "", Some(first));
        tree.push(NodeKind::Text, None, "10.50", Some(price));

        tree.push(NodeKind::Comment, None, " out of print ", Some(catalog));

        let second = tree.push(NodeKind::Element, Some((BOOKS, "book")), "", Some(catalog));
        tree.push(NodeKind::Attr, Some(("", "id")), "b2", Some(second));
        let title = tree.push(NodeKind::Element, Some((BOOKS, "title")), "", Some(second));
        tree.push(NodeKind::Text, None, "Second", Some(title));
        let price = tree.push(NodeKind::Element, Some((BOOKS, "price")), "", Some(second));
        tree.push(NodeKind::Text, None, "20", Some(price));
        tree
    }

    /// The XPath string-value: for elements and the root, the concatenation of
    /// every descendant **text** node — comments and processing instructions
    /// contribute nothing.
    fn string_of(&self, id: usize) -> String {
        let node = &self.nodes[id];
        match node.kind {
            NodeKind::Element | NodeKind::Root => {
                let mut out = String::new();
                for child in &node.children {
                    out.push_str(&self.string_of(*child));
                }
                out
            }
            NodeKind::Text => node.text.clone(),
            _ => String::new(),
        }
    }

    /// Document order, computed the naive way: the pre-order index, with a node's
    /// attributes slotted in right after it.
    fn rank(&self, id: usize) -> u64 {
        let mut counter = 0u64;
        let mut found = None;
        self.walk(0, &mut counter, id, &mut found);
        found.unwrap_or(u64::MAX)
    }

    fn walk(&self, current: usize, counter: &mut u64, wanted: usize, found: &mut Option<u64>) {
        if current == wanted {
            *found = Some(*counter);
        }
        *counter += 1;
        for attribute in &self.nodes[current].attributes {
            if *attribute == wanted {
                *found = Some(*counter);
            }
            *counter += 1;
        }
        for child in &self.nodes[current].children {
            self.walk(*child, counter, wanted, found);
        }
    }
}

impl TreeModel for ToyTree {
    type Node = usize;

    fn parent(&self, n: usize) -> Option<usize> {
        self.nodes[n].parent
    }

    fn children(&self, n: usize) -> impl Iterator<Item = usize> {
        self.nodes[n].children.iter().copied()
    }

    fn kind(&self, n: usize) -> NodeKind {
        self.nodes[n].kind
    }

    fn name(&self, n: usize) -> Option<(&str, &str)> {
        self.nodes[n]
            .name
            .as_ref()
            .map(|(ns, local)| (ns.as_str(), local.as_str()))
    }

    fn string_value(&self, n: usize) -> Cow<'_, str> {
        match self.nodes[n].kind {
            NodeKind::Element | NodeKind::Root => Cow::Owned(self.string_of(n)),
            _ => Cow::Borrowed(&self.nodes[n].text),
        }
    }

    fn attributes(&self, n: usize) -> impl Iterator<Item = usize> {
        self.nodes[n].attributes.iter().copied()
    }

    fn document_order(&self, n: usize) -> u64 {
        self.rank(n)
    }
}

// ---------------------------------------------------------------------------

fn eval(tree: &ToyTree, expr: &str) -> String {
    let compiled = XPath::compile(expr).expect(expr);
    compiled
        .eval(tree, 0, &Variables::new())
        .expect(expr)
        .string_value(tree)
}

fn count(tree: &ToyTree, expr: &str) -> f64 {
    let compiled = XPath::compile(expr).expect(expr);
    compiled
        .eval(tree, 0, &Variables::new())
        .expect(expr)
        .number(tree)
}

#[test]
fn the_engine_runs_over_a_foreign_tree() {
    let tree = ToyTree::catalog();

    assert_eq!(count(&tree, "count(//book)"), 2.0);
    assert_eq!(count(&tree, "count(/catalog/book)"), 2.0);
    assert_eq!(count(&tree, "count(//book/@id)"), 2.0);
    assert_eq!(eval(&tree, "string(//book[1]/@id)"), "b1");
    assert_eq!(eval(&tree, "string(//book[2]/title)"), "Second");
    assert_eq!(eval(&tree, "string(//title/text())"), "First");
    assert_eq!(eval(&tree, "string(sum(//price))"), "30.5");
    assert_eq!(count(&tree, "count(//book[price > 15])"), 1.0);
    assert_eq!(eval(&tree, "string(//book[@id='b2']/price)"), "20");
    assert_eq!(eval(&tree, "name(/*)"), "catalog");
    assert_eq!(eval(&tree, "local-name(//book[1])"), "book");
    assert_eq!(eval(&tree, "namespace-uri(//book[1])"), BOOKS);
    assert_eq!(eval(&tree, "string(/)"), "First10.50Second20");
}

/// Node types an HTML parser never produces: a real PI and a preserved comment.
#[test]
fn comments_and_processing_instructions_are_addressable() {
    let tree = ToyTree::catalog();

    assert_eq!(count(&tree, "count(//comment())"), 1.0);
    assert_eq!(eval(&tree, "string(//comment())"), " out of print ");
    assert_eq!(count(&tree, "count(//processing-instruction())"), 1.0);
    assert_eq!(
        count(&tree, "count(//processing-instruction('xml-stylesheet'))"),
        1.0
    );
    assert_eq!(
        count(&tree, "count(//processing-instruction('other'))"),
        0.0
    );
    assert_eq!(
        eval(&tree, "name(//processing-instruction())"),
        "xml-stylesheet"
    );
    assert_eq!(
        eval(&tree, "string(//processing-instruction())"),
        "href=\"style.css\""
    );
    // The PI and the comment are on the child/descendant axes but are not
    // elements, so `*` never selects them.
    assert_eq!(count(&tree, "count(/node())"), 2.0);
    assert_eq!(count(&tree, "count(/*)"), 1.0);
    assert_eq!(count(&tree, "count(//node())"), 13.0);
}

/// Real namespaces: a prefixed name test resolves through the binding map, and an
/// unprefixed one matches on the local name whatever the namespace.
#[test]
fn prefixed_name_tests_resolve_against_the_binding_map() {
    let tree = ToyTree::catalog();
    let mut namespaces = Namespaces::new();
    namespaces.insert("b", BOOKS);
    let variables = Variables::new();

    let count_with = |expr: &str| -> f64 {
        XPath::compile(expr)
            .expect(expr)
            .eval_with(&tree, 0, &variables, &namespaces)
            .expect(expr)
            .number(&tree)
    };
    assert_eq!(count_with("count(//b:book)"), 2.0);
    assert_eq!(
        count_with("count(//b:*)"),
        7.0,
        "catalog, 2 books, 2 titles, 2 prices"
    );
    assert_eq!(
        count_with("count(//book)"),
        2.0,
        "unprefixed: local name only"
    );
    // `xml` is always bound, so the namespaced attribute is reachable.
    assert_eq!(count_with("count(//@xml:lang)"), 1.0);
    assert_eq!(count_with("count(//book[lang('en')])"), 1.0);
    assert_eq!(count_with("count(//book[lang('fr')])"), 0.0);

    // An unbound prefix is an error, never an empty node-set.
    let error = XPath::compile("//nope:book")
        .expect("compiles")
        .eval(&tree, 0, &variables)
        .expect_err("unbound prefix");
    assert!(error.to_string().contains("undefined namespace prefix"));
}

/// The three defaulted trait methods work without being overridden.
#[test]
fn the_defaulted_trait_methods_carry_this_implementation() {
    let tree = ToyTree::catalog();
    let title = tree
        .nodes
        .iter()
        .position(|node| {
            node.name
                .as_ref()
                .is_some_and(|(_, local)| local == "title")
        })
        .expect("a <title>");

    // `root_of` walks parents.
    assert_eq!(tree.root_of(title), 0);
    // `namespaces` defaults to empty, so the namespace axis selects nothing.
    assert_eq!(tree.namespaces(title).count(), 0);
    assert_eq!(count(&tree, "count(//*/namespace::*)"), 0.0);
    // `attribute_value` defaults to a scan of `attributes`.
    let book = tree.parent(title).expect("the <book>");
    assert_eq!(tree.attribute_value(book, "id").as_deref(), Some("b1"));
    assert_eq!(tree.attribute_value(book, "nope"), None);
    // …and `id()` is built on it.
    assert_eq!(count(&tree, "count(id('b1'))"), 1.0);
    assert_eq!(count(&tree, "count(id('b1 b2'))"), 2.0);
}

/// Axis order, proximity position and document-order results are all the trait's
/// business, not the tree's.
#[test]
fn axes_and_document_order_work_through_the_trait() {
    let tree = ToyTree::catalog();

    assert_eq!(
        eval(&tree, "string(//book[2]/preceding-sibling::*[1])"),
        "First10.50"
    );
    // `//price[1]` is the first price *of each parent*, so both books' — and
    // their ancestors de-duplicate to book, book, catalog.
    assert_eq!(count(&tree, "count(//price[1]/ancestor::*)"), 3.0);
    assert_eq!(count(&tree, "count((//price)[1]/ancestor::*)"), 2.0);
    assert_eq!(count(&tree, "count(//book[1]/following::*)"), 3.0);
    assert_eq!(count(&tree, "count(//book[2]/preceding::*)"), 3.0);
    assert_eq!(count(&tree, "count(//title | //price)"), 4.0);
    assert_eq!(count(&tree, "count(//title | //title)"), 2.0);
    // Union results come back in document order: the first is book 1's title.
    assert_eq!(eval(&tree, "string(//price | //title)"), "First");
    assert_eq!(count(&tree, "count(//*/parent::*)"), 3.0);
}
