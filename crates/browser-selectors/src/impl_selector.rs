//! `BrowserImpl: selectors::SelectorImpl` and `selectors::Element` for `NodeRef`.
//!
//! # Why there is a wrapper type
//!
//! `selectors::Element` and `browser_dom::NodeRef` are both foreign to this
//! crate, so the orphan rule forbids implementing the trait directly on
//! `NodeRef`. [`ElementRef`] is the newtype that carries the impl. It is `Copy`
//! and holds nothing but the node and an optional **scope**.
//!
//! # The scope, and why lxml needs it
//!
//! Python evaluates every CSS selector as XPath with a
//! `descendant-or-self::` prefix (`parser.md` §6.1), rooted at the selector's
//! own node. So `node.css(".container p")` requires the `.container` to be
//! **inside `node`'s subtree** — a browser's `querySelectorAll`, and Servo's
//! matcher, would happily use a `.container` ancestor of `node` instead.
//!
//! [`ElementRef`] reproduces lxml's answer by clipping the ancestor chain:
//! `parent_element()` returns `None` once the scope element has been yielded.
//! Combined with enumerating candidates over `descendant-or-self(root)` this is
//! exactly `descendant-or-self::A/…/B`.
//!
//! Sibling axes are *not* clipped, because cssselect does not clip them either
//! (`:first-child` is `count(preceding-sibling::*) = 0`, evaluated against the
//! real document), and neither is `:root`, which cssselect compiles to
//! `not(parent::*)` — the document's `<html>`, never the scope.

use std::borrow::Borrow;
use std::fmt;

use cssparser::{serialize_identifier, serialize_string, ToCss};
use markup5ever::{ns, LocalName, Namespace};
use precomputed_hash::PrecomputedHash;
use browser_dom::{Element as DomElement, NodeRef};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::matching::{ElementSelectorFlags, MatchingContext};
use selectors::{Element, OpaqueElement, SelectorImpl};

use crate::pseudo::{BrowserPseudoClass, BrowserPseudoElement};

/// The `SelectorImpl` this crate parses and matches against.
///
/// It is a zero-sized marker; all the interesting choices are the associated
/// types below and the [`Element`] impl for [`ElementRef`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserImpl;

impl SelectorImpl for BrowserImpl {
    type ExtraMatchingData<'a> = ();
    type AttrValue = CssString;
    type Identifier = CssLocalName;
    type LocalName = CssLocalName;
    type NamespaceUrl = CssNamespace;
    type NamespacePrefix = CssLocalName;
    type BorrowedNamespaceUrl = CssNamespace;
    type BorrowedLocalName = CssLocalName;
    type NonTSPseudoClass = BrowserPseudoClass;
    type PseudoElement = BrowserPseudoElement;
}

/// An interned name (element local name, class, id, namespace prefix).
///
/// Wrapping `markup5ever::LocalName` rather than `String` means `has_class`,
/// `has_local_name` and friends are atom-pointer comparisons and share the
/// interner with `browser-dom`'s `Element::classes()`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CssLocalName(pub LocalName);

impl CssLocalName {
    /// The interned name.
    #[must_use]
    pub fn as_local_name(&self) -> &LocalName {
        &self.0
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'a> From<&'a str> for CssLocalName {
    fn from(value: &'a str) -> Self {
        Self(LocalName::from(value))
    }
}

impl From<LocalName> for CssLocalName {
    fn from(value: LocalName) -> Self {
        Self(value)
    }
}

impl fmt::Display for CssLocalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToCss for CssLocalName {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        serialize_identifier(&self.0, dest)
    }
}

impl PrecomputedHash for CssLocalName {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

/// A namespace URL, as `selectors` wants it.
///
/// `Default` is the empty atom, which is `selectors`' spelling of "no
/// namespace" — the namespace html5ever gives every attribute outside of
/// SVG/MathML (see `tests/dom_seam.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CssNamespace(pub Namespace);

impl CssNamespace {
    /// The interned namespace URL.
    #[must_use]
    pub fn as_namespace(&self) -> &Namespace {
        &self.0
    }
}

impl<'a> From<&'a str> for CssNamespace {
    fn from(value: &'a str) -> Self {
        Self(Namespace::from(value))
    }
}

impl From<Namespace> for CssNamespace {
    fn from(value: Namespace) -> Self {
        Self(value)
    }
}

impl Borrow<Namespace> for CssNamespace {
    fn borrow(&self) -> &Namespace {
        &self.0
    }
}

impl ToCss for CssNamespace {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        dest.write_str(&self.0)
    }
}

impl PrecomputedHash for CssNamespace {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

/// An attribute *value* appearing inside `[attr="…"]`.
///
/// Values are never interned: they are arbitrary user data, frequently unique,
/// and only ever compared against a `&str`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CssString(pub String);

impl CssString {
    /// The value as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'a> From<&'a str> for CssString {
    fn from(value: &'a str) -> Self {
        Self(value.to_owned())
    }
}

impl AsRef<str> for CssString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CssString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToCss for CssString {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        serialize_string(&self.0, dest)
    }
}

/// The value of `name` on `node`, but only when the attribute carries **no
/// namespace prefix**.
///
/// `browser_dom::NodeRef::attr` looks attributes up by local name alone, so
/// it answers `href` with the value of `xlink:href` inside an inline `<svg>`.
/// libxml2's HTML parser has no namespaces: it stores the literal name
/// `xlink:href`, which `@href` never finds. Requiring the empty namespace here
/// reproduces that.
pub(crate) fn unprefixed_attr<'a>(node: NodeRef<'a>, name: &str) -> Option<&'a str> {
    let element = node.as_element()?;
    element
        .attrs()
        .iter_qual()
        .find(|(qual, _)| qual.local.as_ref() == name && qual.ns.is_empty())
        .map(|(_, value)| value)
}

/// A `browser_dom::NodeRef` that knows how to answer `selectors::Element`.
///
/// Construct one with [`ElementRef::new`] (unscoped — ancestors run all the way
/// to the document) or [`ElementRef::with_scope`] (ancestors stop at `scope`,
/// which is what `Node::css()` needs; see the module docs).
#[derive(Clone, Copy)]
pub struct ElementRef<'a> {
    node: NodeRef<'a>,
    scope: Option<NodeRef<'a>>,
}

impl fmt::Debug for ElementRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.node, f)
    }
}

impl PartialEq for ElementRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}

impl Eq for ElementRef<'_> {}

impl<'a> ElementRef<'a> {
    /// Wrap an element node, matching against the whole document.
    ///
    /// Returns `None` when `node` is not an element.
    #[must_use]
    pub fn new(node: NodeRef<'a>) -> Option<Self> {
        Self::with_scope(node, None)
    }

    /// Wrap an element node whose ancestor chain is clipped at `scope`.
    ///
    /// `scope` may be any node — including the document node, in which case the
    /// clipping never fires because `<html>` already has no parent element.
    /// Returns `None` when `node` is not an element.
    #[must_use]
    pub fn with_scope(node: NodeRef<'a>, scope: Option<NodeRef<'a>>) -> Option<Self> {
        node.is_element().then_some(Self { node, scope })
    }

    /// The wrapped node.
    #[must_use]
    pub fn node(&self) -> NodeRef<'a> {
        self.node
    }

    /// The scope this element's ancestor walk stops at, if any.
    #[must_use]
    pub fn scope(&self) -> Option<NodeRef<'a>> {
        self.scope
    }

    /// The element payload. Infallible: the constructors reject non-elements.
    fn element(&self) -> &'a DomElement {
        self.node
            .as_element()
            .unwrap_or_else(|| unreachable!("ElementRef only wraps element nodes"))
    }

    fn sibling(&self, node: Option<NodeRef<'a>>) -> Option<Self> {
        node.map(|node| Self {
            node,
            scope: self.scope,
        })
    }

    /// `true` when the element is in the HTML namespace.
    fn is_html(&self) -> bool {
        self.element().name().ns == ns!(html)
    }

    /// The element's local name, whatever namespace it is in.
    ///
    /// cssselect's HTML-specific pseudo-classes all test `name(.) = 'input'`
    /// &c. In libxml2's HTML tree *nothing* is namespaced — not even the
    /// contents of an inline `<svg>` — so `name()` is always the bare local
    /// name and the tests are namespace-blind. html5ever, by contrast, puts
    /// SVG and MathML content in their own namespaces, so gating these
    /// pseudo-classes on the HTML namespace would make
    /// `<svg><a href="…"></svg>` fail `:link` here while matching in Python.
    /// Matching on the local name alone reproduces the reference
    /// implementation (`tests/css_golden.rs`, page `svg`).
    fn pseudo_class_local_name(&self) -> Option<&'a LocalName> {
        Some(self.element().local_name())
    }

    /// An **unprefixed** attribute, by local name.
    ///
    /// `browser_dom::NodeRef::attr` is namespace-blind, which is wrong here:
    /// html5ever puts `xlink:href` in the XLink namespace, while libxml2's HTML
    /// parser stores the whole `xlink:href` string as the attribute's *name*.
    /// So Python's `@href` does **not** find `<svg><a xlink:href="…">` and
    /// neither may we — otherwise that `<a>` becomes a `:link` here and not in
    /// Python (`tests/css_golden.rs`, page `weird`).
    fn attr(&self, name: &str) -> Option<&'a str> {
        unprefixed_attr(self.node, name)
    }

    fn has_attr(&self, name: &str) -> bool {
        self.attr(name).is_some()
    }

    /// cssselect's `:contains('v')` → `contains(., 'v')`: a substring test on
    /// the element's XPath string-value, descendants included.
    fn matches_contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            // XPath: contains(x, '') is true for every x.
            return true;
        }
        self.node.text_content().contains(needle)
    }

    /// cssselect's HTML `:lang('v')`:
    /// `ancestor-or-self::*[@lang][1][starts-with(concat(lower(@lang), '-'), 'v-')]`
    /// where the literal is `value.lower() + '-'` — **both** sides are
    /// lower-cased, so `:lang(EN)` matches `lang="en-GB"` in Python and must
    /// here too.
    fn matches_lang(&self, value: &str) -> bool {
        let mut current = Some(self.node);
        while let Some(node) = current {
            if let Some(lang) = unprefixed_attr(node, "lang") {
                let mut owned = lang.to_ascii_lowercase();
                owned.push('-');
                let mut wanted = value.to_ascii_lowercase();
                wanted.push('-');
                return owned.starts_with(&wanted);
            }
            current = node.parent_element();
        }
        false
    }

    /// cssselect's `:checked`:
    /// `(@selected and option) or (@checked and (input|command) and @type in (checkbox, radio))`.
    fn matches_checked(&self) -> bool {
        let Some(local) = self.pseudo_class_local_name() else {
            return false;
        };
        if self.has_attr("selected") && local == &LocalName::from("option") {
            return true;
        }
        if !self.has_attr("checked") {
            return false;
        }
        if local != &LocalName::from("input") && local != &LocalName::from("command") {
            return false;
        }
        matches!(self.attr("type"), Some("checkbox" | "radio"))
    }

    /// `(name(.) = 'input' and @type != 'hidden')`.
    ///
    /// XPath's `@type != 'hidden'` is **false** when the attribute is missing
    /// (comparison with an empty node-set), which is why the `Some(..)` is not
    /// an `is_none_or`.
    fn is_non_hidden_input(&self, local: &LocalName) -> bool {
        local == &LocalName::from("input") && self.attr("type").is_some_and(|t| t != "hidden")
    }

    /// cssselect's HTML `:enabled` (see the translator output in `parser.md` §6.4).
    fn matches_enabled(&self) -> bool {
        let Some(local) = self.pseudo_class_local_name() else {
            return false;
        };
        let name = local.as_ref();

        if self.has_attr("href") && matches!(name, "a" | "link" | "area") {
            return true;
        }
        if matches!(name, "command" | "fieldset" | "optgroup") && !self.has_attr("disabled") {
            return true;
        }
        let form_control = self.is_non_hidden_input(local)
            || matches!(name, "button" | "select" | "textarea" | "keygen");
        if form_control
            && !self.has_attr("disabled")
            && !self.has_disabled_ancestor(&LocalName::from("fieldset"))
        {
            return true;
        }
        name == "option"
            && !self.has_attr("disabled")
            && !self.has_disabled_ancestor(&LocalName::from("optgroup"))
    }

    /// cssselect's HTML `:disabled`.
    fn matches_disabled(&self) -> bool {
        let Some(local) = self.pseudo_class_local_name() else {
            return false;
        };
        let name = local.as_ref();

        if self.has_attr("disabled")
            && (self.is_non_hidden_input(local)
                || matches!(
                    name,
                    "button"
                        | "select"
                        | "textarea"
                        | "command"
                        | "fieldset"
                        | "optgroup"
                        | "option"
                ))
        {
            return true;
        }
        let fieldset_control =
            self.is_non_hidden_input(local) || matches!(name, "button" | "select" | "textarea");
        fieldset_control && self.has_disabled_ancestor(&LocalName::from("fieldset"))
    }

    /// XPath `ancestor::<name>[@disabled]`.
    ///
    /// Strictly ancestors, never self, and never clipped by the scope — the
    /// XPath cssselect emits is not clipped either.
    fn has_disabled_ancestor(&self, name: &LocalName) -> bool {
        self.node.ancestors().any(|a| {
            // Namespace-blind, for the same reason `pseudo_class_local_name`
            // is: libxml2's HTML tree has no namespaces for `name()` to see.
            a.as_element().is_some_and(|el| el.local_name() == name)
                && a.attrs().contains("disabled")
        })
    }
}

impl<'a> Element for ElementRef<'a> {
    type Impl = BrowserImpl;

    fn opaque(&self) -> OpaqueElement {
        // `NodeRef` is `Copy` and rebuilt constantly, so its own address is
        // meaningless. `value()` borrows out of the arena, whose address is
        // stable for the life of the `Document` — see `tests/dom_seam.rs`.
        OpaqueElement::new(self.node.value())
    }

    fn parent_element(&self) -> Option<Self> {
        if self.scope == Some(self.node) {
            return None;
        }
        self.node.parent_element().map(|node| Self {
            node,
            scope: self.scope,
        })
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.sibling(self.node.prev_element_sibling())
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.sibling(self.node.next_element_sibling())
    }

    fn first_element_child(&self) -> Option<Self> {
        self.sibling(self.node.first_element_child())
    }

    fn is_html_element_in_html_document(&self) -> bool {
        // Everything `browser-dom` parses is an HTML document, so this
        // reduces to "is the element in the HTML namespace".
        self.is_html()
    }

    fn has_local_name(&self, local_name: &CssLocalName) -> bool {
        self.element().local_name() == &local_name.0
    }

    /// libxml2's HTML parser namespaces **nothing** — not even the contents of
    /// an inline `<svg>` — so cssselect's explicit-no-namespace type selector
    /// (`|div`) matches every `div` in Python, wherever it lives. html5ever
    /// does namespace its elements, so an equality test against the empty
    /// namespace would make `|div` select nothing at all.
    ///
    /// The empty namespace therefore means "any", which is the only reading
    /// that reproduces the reference implementation (`tests/css_golden.rs`
    /// exercises `|div` and `|a` on all ten pages, the SVG ones included).
    /// Non-empty constraints are unreachable: this crate binds no namespace
    /// prefix and declares no default namespace, exactly like `cssselect`'s
    /// bare-selector translator.
    fn has_namespace(&self, ns: &CssNamespace) -> bool {
        ns.0.is_empty() || self.element().name().ns == ns.0
    }

    fn is_same_type(&self, other: &Self) -> bool {
        let (a, b) = (self.element().name(), other.element().name());
        a.local == b.local && a.ns == b.ns
    }

    /// Attribute matching, always **case-sensitive** on the value.
    ///
    /// HTML defines a list of attributes (`type`, `rel`, `lang`, `align`, …)
    /// whose values selectors compare ASCII-case-insensitively in an HTML
    /// document, and `selectors` implements it: it hands us a
    /// `CaseSensitivity` already resolved through
    /// [`Element::is_html_element_in_html_document`]. cssselect knows nothing
    /// of that list — `[type="TEXT"]` becomes the XPath `@type = 'TEXT'`, a
    /// plain string comparison — so honouring it would select elements Python
    /// does not (`tests/css_golden.rs`, pages `forms`, `quirks`, `malformed`).
    ///
    /// The flag is therefore overridden to `CaseSensitive` here. The *other*
    /// source of case-insensitivity, an explicit `[a="b" i]`, cannot reach this
    /// point: [`crate::parse`] rejects the flag outright, because cssselect
    /// does.
    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&CssNamespace>,
        local_name: &CssLocalName,
        operation: &AttrSelectorOperation<&CssString>,
    ) -> bool {
        let operation = match *operation {
            AttrSelectorOperation::Exists => AttrSelectorOperation::Exists,
            AttrSelectorOperation::WithValue {
                operator,
                case_sensitivity: _,
                value,
            } => AttrSelectorOperation::WithValue {
                operator,
                case_sensitivity: CaseSensitivity::CaseSensitive,
                value,
            },
        };
        self.node.attrs().iter_qual().any(|(name, value)| {
            name.local == local_name.0
                && match ns {
                    NamespaceConstraint::Any => true,
                    NamespaceConstraint::Specific(url) => name.ns == url.0,
                }
                && operation.eval_str(value)
        })
    }

    fn has_attr_in_no_namespace(&self, local_name: &CssLocalName) -> bool {
        self.node
            .attrs()
            .iter_qual()
            .any(|(name, _)| name.local == local_name.0 && name.ns.is_empty())
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &BrowserPseudoClass,
        _context: &mut MatchingContext<'_, BrowserImpl>,
    ) -> bool {
        match pc {
            BrowserPseudoClass::Contains(value) => self.matches_contains(value),
            BrowserPseudoClass::Lang(value) => self.matches_lang(value),
            BrowserPseudoClass::Link => self.is_link(),
            // cssselect compiles these to the constant `0`.
            BrowserPseudoClass::Visited
            | BrowserPseudoClass::Hover
            | BrowserPseudoClass::Active
            | BrowserPseudoClass::Focus
            | BrowserPseudoClass::Target => false,
            BrowserPseudoClass::Checked => self.matches_checked(),
            BrowserPseudoClass::Enabled => self.matches_enabled(),
            BrowserPseudoClass::Disabled => self.matches_disabled(),
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &BrowserPseudoElement,
        _context: &mut MatchingContext<'_, BrowserImpl>,
    ) -> bool {
        // `::text` / `::attr()` place no condition on the element itself; they
        // only decide what the match *yields*. `crate::matching` matches such
        // selectors in `MatchingMode::ForStatelessPseudoElement`, which consumes
        // the pseudo-element before ever reaching here, so this is the answer
        // only for a caller that opts into `MatchingMode::Normal`.
        true
    }

    fn pseudo_element_originating_element(&self) -> Option<Self> {
        // The default implementation returns the *parent*, which is right for
        // `::before`/`::after` boxes and wrong for us: `p::text` describes text
        // owned by the `<p>` itself.
        Some(*self)
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {
        // The tree is immutable while matching, so there is no invalidation
        // bookkeeping to do.
    }

    fn is_link(&self) -> bool {
        self.pseudo_class_local_name()
            .is_some_and(|local| matches!(local.as_ref(), "a" | "link" | "area"))
            && self.has_attr("href")
    }

    fn is_html_slot_element(&self) -> bool {
        // Servo's own slot detection, not one of cssselect's pseudo-classes:
        // this one really is HTML-namespace-only.
        self.is_html() && self.element().local_name() == &LocalName::from("slot")
    }

    fn has_id(&self, id: &CssLocalName, case_sensitivity: CaseSensitivity) -> bool {
        self.element()
            .id()
            .is_some_and(|value| case_sensitivity.eq(value.as_bytes(), id.0.as_bytes()))
    }

    fn has_class(&self, name: &CssLocalName, case_sensitivity: CaseSensitivity) -> bool {
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => self.element().has_class(&name.0),
            CaseSensitivity::AsciiCaseInsensitive => {
                self.element().has_class_ignore_ascii_case(&name.0)
            }
        }
    }

    fn has_custom_state(&self, _name: &CssLocalName) -> bool {
        false
    }

    fn imported_part(&self, _name: &CssLocalName) -> Option<CssLocalName> {
        None
    }

    fn is_part(&self, _name: &CssLocalName) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        // Note that `remove_blank_text` has already deleted whitespace-only
        // text nodes (both here and in Python), so `<span> </span>` is `:empty`
        // — see `tests/dom_seam.rs`.
        self.node.element_children().next().is_none()
            && self
                .node
                .children()
                .all(|child| child.as_text().is_none_or(str::is_empty))
    }

    fn is_root(&self) -> bool {
        // cssselect: `not(parent::*)`. Deliberately not scope-relative.
        self.node.is_root_element()
    }

    fn add_element_unique_hashes(&self, _filter: &mut BloomFilter) -> bool {
        // We never build an ancestor bloom filter (we pass `None` for it in
        // `crate::matching`), so there is nothing useful to contribute.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::Document;

    fn first<'a>(doc: &'a Document, tag: &str) -> ElementRef<'a> {
        let node = doc
            .elements()
            .find(|e| e.tag() == tag)
            .expect("element exists");
        ElementRef::new(node).expect("an element")
    }

    #[test]
    fn wrappers_serialise_as_css() {
        assert_eq!(CssLocalName::from("data-id").to_css_string(), "data-id");
        assert_eq!(CssString::from("a\"b").to_css_string(), "\"a\\\"b\"");
        assert_eq!(CssLocalName::from("a b").to_css_string(), "a\\ b");
    }

    #[test]
    fn non_elements_are_rejected() {
        let doc = Document::parse("<p>hi</p>");
        let text = doc
            .nodes()
            .find(browser_dom::NodeRef::is_text)
            .expect("a text node");
        assert!(ElementRef::new(text).is_none());
    }

    #[test]
    fn scope_clips_the_ancestor_chain() {
        let doc = Document::parse("<div id=a><section><p>x</p></section></div>");
        let section = doc
            .elements()
            .find(|e| e.tag() == "section")
            .expect("section");
        let p = doc.elements().find(|e| e.tag() == "p").expect("p");

        let unscoped = ElementRef::new(p).expect("element");
        assert_eq!(
            unscoped.parent_element().map(|e| e.node().tag()),
            Some("section")
        );

        let scoped = ElementRef::with_scope(p, Some(p)).expect("element");
        assert!(scoped.parent_element().is_none());

        let scoped = ElementRef::with_scope(p, Some(section)).expect("element");
        assert_eq!(
            scoped.parent_element().map(|e| e.node().tag()),
            Some("section")
        );
        assert!(scoped
            .parent_element()
            .and_then(|e| e.parent_element())
            .is_none());
    }

    #[test]
    fn link_and_root_follow_cssselect() {
        let doc = Document::parse("<a href='/x'>x</a><a>y</a>");
        let linked = first(&doc, "a");
        assert!(linked.is_link());
        let bare = doc
            .elements()
            .filter(|e| e.tag() == "a")
            .nth(1)
            .and_then(ElementRef::new)
            .expect("second <a>");
        assert!(!bare.is_link());

        let html = first(&doc, "html");
        assert!(html.is_root());
        assert!(!linked.is_root());
    }

    #[test]
    fn contains_reads_the_whole_string_value() {
        let doc = Document::parse("<div><b>Ph</b>one</div>");
        let div = first(&doc, "div");
        assert!(div.matches_contains("Phone"));
        assert!(div.matches_contains(""));
        assert!(!div.matches_contains("Tablet"));
    }

    #[test]
    fn lang_uses_the_nearest_lang_ancestor() {
        let doc = Document::parse("<div lang='EN-gb'><p><span>x</span></p></div>");
        let span = first(&doc, "span");
        assert!(span.matches_lang("en"));
        // cssselect lower-cases *both* sides: the XPath literal it builds is
        // `value.lower() + '-'`, so `:lang(EN)` matches `lang="EN-gb"`.
        assert!(span.matches_lang("EN"));
        assert!(span.matches_lang("en-GB"));
        assert!(!span.matches_lang("fr"));

        let doc = Document::parse("<div lang='fr'><p lang='en'><span>x</span></p></div>");
        let span = first(&doc, "span");
        assert!(span.matches_lang("en"));
        assert!(!span.matches_lang("fr"), "only the *nearest* @lang counts");
    }

    #[test]
    fn checked_enabled_disabled_follow_cssselect() {
        let html = r#"
            <form>
              <input id="cb" type="checkbox" checked>
              <input id="rb" type="radio">
              <input id="bare" checked>
              <option id="opt" selected>o</option>
              <fieldset id="fs" disabled><input id="fsinput" type="text"><button id="fsbtn">b</button></fieldset>
              <select id="sel" disabled></select>
              <a id="link" href="/x">link</a>
              <input id="hid" type="hidden">
            </form>"#;
        let doc = Document::parse(html);

        fn ids<'d>(doc: &'d Document, pred: fn(&ElementRef<'d>) -> bool) -> Vec<&'d str> {
            doc.elements()
                .filter_map(ElementRef::new)
                .filter(pred)
                .filter_map(|e| e.node().element_id())
                .collect()
        }

        assert_eq!(
            ids(&doc, ElementRef::matches_checked),
            vec!["cb", "opt"],
            "cssselect: (@selected and option) or (@checked and input|command and checkbox|radio)"
        );

        // `<input checked>` has no @type, and XPath's `@type != 'hidden'` is
        // false for a missing attribute, so `#bare` is neither enabled nor
        // disabled. `#hid` is excluded for the same reason.
        assert_eq!(
            ids(&doc, ElementRef::matches_enabled),
            vec!["cb", "rb", "opt", "link"]
        );
        assert_eq!(
            ids(&doc, ElementRef::matches_disabled),
            vec!["fs", "fsinput", "fsbtn", "sel"]
        );
    }

    #[test]
    fn empty_matches_the_normalised_tree() {
        let doc = Document::parse("<span></span><span> </span><span>x</span><span><b></b></span>");
        let empties: Vec<bool> = doc
            .elements()
            .filter(|e| e.tag() == "span")
            .filter_map(ElementRef::new)
            .map(|e| e.is_empty())
            .collect();
        assert_eq!(empties, vec![true, true, false, false]);
    }
}
