//! Selector parsing and selector-group splitting, so each sub-selector gets its own
//! adaptive identifier.
//!
//! # Why the group is split
//!
//! `parser.md` §3.4:
//!
//! ```python
//! if (not self.__adaptive_enabled) or ("," not in selector):
//!     return self.xpath(css_to_xpath(selector), identifier or selector, …)
//! results = Selectors()
//! for single in cssselect.parse(selector):       # splits the selector group
//!     results += self.xpath(css_to_xpath(single.canonical()),
//!                           identifier or single.canonical(), …)
//! ```
//!
//! The split exists **only** so the adaptive feature stores one fingerprint per
//! sub-selector (`css("#p1, #p2", auto_save=True)` writes entries `#p1` and
//! `#p2`, pinned by `test_element_relocation`). [`SelectorGroup`] therefore
//! keeps the parts addressable — [`SelectorGroup::parts`] — while
//! [`SelectorGroup::select`] reproduces the *non*-adaptive path, an XPath union
//! (document order, duplicates removed).

use browser_dom::NodeRef;
use cssparser::{
    match_ignore_ascii_case, CowRcStr, Parser as CssParser, ParserInput, SourceLocation, ToCss,
    Token,
};
use markup5ever::LocalName;
use selectors::attr::{ParsedAttrSelectorOperation, ParsedCaseSensitivity};
use selectors::parser::{
    Combinator, Component, ParseRelative, RelativeSelector, Selector, SelectorList,
    SelectorParseErrorKind,
};
use selectors::visitor::SelectorVisitor;

use crate::canonical;
use crate::impl_selector::{BrowserImpl, CssLocalName, CssNamespace};
use crate::matching::{self, Match};
use crate::pseudo::Tail;

// Re-exported so that `use browser_selectors::parse::*` is the single import
// a downstream crate needs. `lib.rs` carries no re-exports of its own — see
// `API.md`.
pub use crate::error::{Result, SelectorError};
pub use crate::matching::{MatchValue, TextRole};
pub use crate::pseudo::{BrowserPseudoClass, BrowserPseudoElement, Tail as SelectorTail};

/// The `selectors::Parser` that teaches Servo's grammar the reference implementation's vocabulary.
///
/// Everything structural (`:not`, `:is`, `:where`, `:has`, `:nth-*`, `:scope`,
/// `:root`, attribute selectors, combinators, namespaces) is Servo's; what this
/// adds is cssselect's non-structural pseudo-classes and the reference implementation's two
/// pseudo-elements.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectorParser;

impl SelectorParser {
    /// Read the single `IDENT` or `STRING` argument that `::attr()`,
    /// `:contains()` and `:lang()` all take, and render whatever was there
    /// instead on failure so the error message can name it.
    fn single_ident_or_string<'i, 't>(
        input: &mut CssParser<'i, 't>,
    ) -> std::result::Result<String, String> {
        let mut seen: Vec<String> = Vec::new();
        let mut value: Option<String> = None;
        // Drain the whole block rather than stopping at the first token, so the
        // error message can list everything the way cssselect's does
        // (`got [<IDENT 'a' at 8>, <IDENT 'b' at 10>]`).
        while let Ok(token) = input.next() {
            seen.push(render_token(token));
            value = match token {
                Token::Ident(name) => Some(name.as_ref().to_owned()),
                Token::QuotedString(s) => Some(s.as_ref().to_owned()),
                _ => None,
            };
        }
        match (value, seen.len()) {
            (Some(v), 1) => Ok(v),
            _ => Err(format!("[{}]", seen.join(", "))),
        }
    }
}

fn render_token(token: &Token<'_>) -> String {
    use cssparser::ToCss as _;
    let mut out = String::new();
    if token.to_css(&mut out).is_err() {
        return format!("{token:?}");
    }
    out
}

impl<'i> selectors::Parser<'i> for SelectorParser {
    type Impl = BrowserImpl;
    type Error = SelectorError;

    fn parse_is_and_where(&self) -> bool {
        true
    }

    /// CSS Selectors 4 makes `:is()` / `:where()` *forgiving* — an unparsable
    /// branch is dropped instead of failing the whole selector. cssselect is a
    /// CSS3 parser and raises, so `div:is(.a, %%%)` is an error in Python.
    /// Turning recovery off keeps the two in step
    /// (`tests/css_golden.rs`, selector `div:is(.a, %%%)`).
    fn allow_forgiving_selectors(&self) -> bool {
        false
    }

    fn parse_has(&self) -> bool {
        true
    }

    fn parse_nth_child_of(&self) -> bool {
        true
    }

    fn parse_part(&self) -> bool {
        false
    }

    fn parse_slotted(&self) -> bool {
        false
    }

    fn parse_host(&self) -> bool {
        false
    }

    fn parse_parent_selector(&self) -> bool {
        false
    }

    /// cssselect implements `:matches()` as the old spelling of `:is()`, and
    /// browsers also accept `:any()`. Both are aliases here.
    fn is_is_alias(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("matches") || name.eq_ignore_ascii_case("any")
    }

    /// No default namespace, so a bare type selector matches in *any*
    /// namespace.
    ///
    /// That is what Python does, if by accident: libxml2's HTML parser gives
    /// every element — including the contents of `<svg>` — an empty namespace,
    /// so lxml's `descendant-or-self::a` matches an SVG `<a>` as readily as an
    /// HTML one. html5ever is spec-correct and namespaces foreign content, so
    /// leaving the constraint off is what keeps the two agreeing.
    fn default_namespace(&self) -> Option<CssNamespace> {
        None
    }

    /// **No prefix is ever bound**, so `svg|a` is a syntax error.
    ///
    /// This is not a limitation, it is fidelity. cssselect happily parses a
    /// prefixed selector and translates it into the XPath name test `svg:a`,
    /// but lxml then refuses to evaluate it because the prefix is not in the
    /// namespace map — so *every* prefixed selector raises in Python:
    ///
    /// ```text
    /// >>> Selector(html).css("svg|a")
    /// SelectorSyntaxError: Invalid CSS selector 'svg|a':
    ///     Invalid XPath selector: descendant-or-self::svg:a
    /// ```
    ///
    /// Pre-declaring the prefixes html5ever can produce would make us accept
    /// (and answer) selectors Python rejects. `*|a` and a bare `a` are
    /// unaffected: `default_namespace()` is `None`, so a type selector
    /// with no prefix already matches in every namespace, which is exactly
    /// what an SVG-aware query needs (`tests/css_golden.rs`, page `svg`).
    fn namespace_for_prefix(&self, _prefix: &CssLocalName) -> Option<CssNamespace> {
        None
    }

    fn parse_non_ts_pseudo_class(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> std::result::Result<BrowserPseudoClass, cssparser::ParseError<'i, SelectorError>> {
        use BrowserPseudoClass as P;
        let class = match_ignore_ascii_case! { &name,
            "link" => P::Link,
            "visited" => P::Visited,
            "hover" => P::Hover,
            "active" => P::Active,
            "focus" => P::Focus,
            "target" => P::Target,
            "checked" => P::Checked,
            "enabled" => P::Enabled,
            "disabled" => P::Disabled,
            _ => return Err(location.new_custom_error(
                SelectorError::UnknownPseudoClass(name.as_ref().to_owned()),
            )),
        };
        Ok(class)
    }

    fn parse_non_ts_functional_pseudo_class<'t>(
        &self,
        name: CowRcStr<'i>,
        parser: &mut CssParser<'i, 't>,
        _after_part: bool,
    ) -> std::result::Result<BrowserPseudoClass, cssparser::ParseError<'i, SelectorError>> {
        let lowercase = name.as_ref().to_ascii_lowercase();
        match lowercase.as_str() {
            "contains" | "lang" => match Self::single_ident_or_string(parser) {
                Ok(value) => Ok(if lowercase == "contains" {
                    BrowserPseudoClass::Contains(value)
                } else {
                    BrowserPseudoClass::Lang(value)
                }),
                Err(got) => Err(
                    parser.new_custom_error(SelectorError::BadPseudoClassArgument {
                        name: lowercase,
                        got,
                    }),
                ),
            },
            _ => Err(
                parser.new_custom_error(SelectorError::UnknownFunctionalPseudoClass(
                    name.as_ref().to_owned(),
                )),
            ),
        }
    }

    fn parse_pseudo_element(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> std::result::Result<BrowserPseudoElement, cssparser::ParseError<'i, SelectorError>> {
        if name.as_ref().eq_ignore_ascii_case("text") {
            return Ok(BrowserPseudoElement::Text);
        }
        Err(
            location.new_custom_error(SelectorError::UnknownPseudoElement(
                name.as_ref().to_owned(),
            )),
        )
    }

    fn parse_functional_pseudo_element<'t>(
        &self,
        name: CowRcStr<'i>,
        arguments: &mut CssParser<'i, 't>,
    ) -> std::result::Result<BrowserPseudoElement, cssparser::ParseError<'i, SelectorError>> {
        if name.as_ref().eq_ignore_ascii_case("attr") {
            return match Self::single_ident_or_string(arguments) {
                Ok(value) => Ok(BrowserPseudoElement::Attr(LocalName::from(&*value))),
                Err(got) => Err(arguments.new_custom_error(SelectorError::BadAttrArgument(got))),
            };
        }
        Err(
            arguments.new_custom_error(SelectorError::UnknownFunctionalPseudoElement(
                name.as_ref().to_owned(),
            )),
        )
    }
}

// `SelectorParseErrorKind` reaches us only after `From` has turned it into a
// `SelectorError`; this `use` documents the seam and keeps the bound visible.
const _: fn(SelectorParseErrorKind<'static>) -> SelectorError = SelectorError::from;

/// Rewrite cssselect's non-standard `[attr!=value]` into standard CSS.
///
/// `!=` is not in any CSS Selectors level and Servo's attribute-selector
/// grammar — which is not extensible from outside the crate — rejects it. But
/// `parser.md` §6.4 lists it, so we translate at the source level, exactly
/// reproducing cssselect's XPath:
///
/// | cssselect | XPath | rewritten to |
/// |---|---|---|
/// | `[n!='v']` | `not(@n) or @n != 'v'` | `:not([n='v'])` |
/// | `[n!='']`  | `@n != ''`             | `[n]:not([n=''])` |
///
/// The empty-value case is special because cssselect drops the `not(@n)` arm
/// there: `[n!='']` requires the attribute to be *present* and non-empty.
///
/// The scan is string-literal aware and leaves everything else byte-identical;
/// a selector without a `!` is returned borrowed.
fn rewrite_not_equal(selector: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;

    if !selector.contains('!') {
        return Cow::Borrowed(selector);
    }
    let mut out = String::with_capacity(selector.len() + 8);
    let mut rest = selector;
    let mut changed = false;

    while !rest.is_empty() {
        let Some(open) = find_unquoted(rest, '[') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = find_unquoted(after, ']') else {
            out.push_str(&rest[open..]);
            break;
        };
        let inner = &after[..close];
        match find_bang_equals(inner) {
            Some(at) => {
                let name = inner[..at].trim();
                let value = inner[at + 2..].trim();
                if value == "\"\"" || value == "''" {
                    out.push_str(&format!("[{name}]:not([{name}={value}])"));
                } else {
                    out.push_str(&format!(":not([{name}={value}])"));
                }
                changed = true;
            }
            None => {
                out.push('[');
                out.push_str(inner);
                out.push(']');
            }
        }
        rest = &after[close + 1..];
    }

    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(selector)
    }
}

/// Reject a selector with an unclosed `[`, `(` or string literal.
///
/// The CSS tokenizer specifies EOF recovery — `[href` tokenizes as a complete
/// `[href]` block and `:contains("x` as a complete string — so `cssparser`
/// accepts both. cssselect does not:
///
/// ```text
/// >>> Selector(html).css("[href")
/// SelectorSyntaxError: Invalid CSS selector '[href': Operator expected, got <EOF at 5>
/// ```
///
/// Recovering here would mean silently answering a query Python refuses, so a
/// cheap string-literal-aware balance check runs first
/// (`tests/css_golden.rs`, selector `[href`).
///
/// # Errors
///
/// [`SelectorError::Syntax`] naming the delimiter that was never closed.
fn check_balanced(selector: &str) -> Result<()> {
    let mut stack: Vec<char> = Vec::new();
    let mut chars = selector.char_indices();
    while let Some((_, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '\'' | '"' => {
                let mut closed = false;
                while let Some((_, inner)) = chars.next() {
                    if inner == '\\' {
                        chars.next();
                    } else if inner == c {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(SelectorError::Syntax(format!(
                        "unexpected EOF: unterminated string literal opened by {c}"
                    )));
                }
            }
            '[' | '(' => stack.push(c),
            ']' | ')' => {
                let want = if c == ']' { '[' } else { '(' };
                match stack.pop() {
                    Some(open) if open == want => {}
                    _ => {
                        return Err(SelectorError::Syntax(format!(
                            "unexpected token: got <DELIM '{c}'>"
                        )))
                    }
                }
            }
            _ => {}
        }
    }
    match stack.last() {
        None => Ok(()),
        Some(open) => Err(SelectorError::Syntax(format!(
            "unexpected EOF: unterminated block opened by {open}"
        ))),
    }
}

/// Byte offset of the first `needle` outside a CSS string literal.
fn find_unquoted(haystack: &str, needle: char) -> Option<usize> {
    let mut chars = haystack.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '\'' | '"' => {
                while let Some((_, inner)) = chars.next() {
                    if inner == '\\' {
                        chars.next();
                    } else if inner == c {
                        break;
                    }
                }
            }
            _ if c == needle => return Some(i),
            _ => {}
        }
    }
    None
}

/// Byte offset of an unquoted `!=` inside the body of an attribute selector.
fn find_bang_equals(inner: &str) -> Option<usize> {
    let at = find_unquoted(inner, '!')?;
    inner[at + 1..].starts_with('=').then_some(at)
}

/// One sub-selector of a comma-separated group: `#p1` out of `"#p1, #p2"`.
///
/// Holds the compiled matcher, the trailing pseudo-element lifted out as a
/// [`Tail`], and the canonical re-serialisation used as the adaptive-storage
/// identifier.
#[derive(Clone, Debug)]
pub struct SubSelector {
    matcher: Selector<BrowserImpl>,
    tail: Tail,
    canonical: String,
    has_combinator: bool,
    /// The extra matcher a trailing bare `*` demands; see
    /// [`SubSelector::subtree_head`].
    subtree_head: Option<(Selector<BrowserImpl>, bool)>,
}

impl SubSelector {
    fn new(matcher: Selector<BrowserImpl>) -> Self {
        let tail = Tail::from_pseudo_element(matcher.pseudo_element());
        let canonical = canonical::serialize(&matcher);
        let has_combinator = {
            let mut iter = matcher.iter();
            let mut found = false;
            loop {
                // Walk off the current compound; a combinator follows iff
                // there is another compound to its left.
                for _ in iter.by_ref() {}
                match iter.next_sequence() {
                    None => break,
                    // `a::attr(href)` is stored as two "compounds" joined by a
                    // pseudo-element pseudo-combinator. That is not a
                    // combinator in the CSS sense and must not count.
                    Some(
                        Combinator::PseudoElement | Combinator::SlotAssignment | Combinator::Part,
                    ) => {}
                    Some(_) => {
                        found = true;
                        break;
                    }
                }
            }
            found
        };
        Self {
            matcher,
            tail,
            canonical,
            has_combinator,
            subtree_head: None,
        }
    }

    /// The second matcher `X *::text` / `X *::attr(a)` needs, if any.
    ///
    /// The reference implementation's translator does not render a trailing bare `*` as a step of
    /// its own when a pseudo-element follows a **descendant** combinator — it
    /// folds it into the pseudo-element instead
    /// (`reference/core/translator.py`, `XPathExpr.__str__`):
    ///
    /// ```text
    /// div *::text        -> descendant-or-self::div/descendant-or-self::text()
    /// div *::attr(class) -> descendant-or-self::div/descendant-or-self::*/@class
    /// div > *::text      -> descendant-or-self::div/*/text()          (untouched)
    /// div p::text        -> descendant-or-self::div//p/text()         (untouched)
    /// ```
    ///
    /// So `div *::text` is *every* text node under a `div`, at any depth,
    /// including the `div`'s own — not the text of the elements matching
    /// `div *`. Since every text node is the direct child of exactly one
    /// element of `descendant-or-self::div`, that set is exactly the union of
    /// our element-wise reading of `div *::text` with the same reading of
    /// `div::text`, and the second matcher here is that `div::text`.
    ///
    /// The head is used *literally* — its own trailing `*`, if any, is not
    /// expanded again, because `div * *::text` must not reach the text of the
    /// `div` itself.
    #[must_use]
    pub fn subtree_head(&self) -> Option<&Selector<BrowserImpl>> {
        self.subtree_head.as_ref().map(|(matcher, _)| matcher)
    }

    /// Every matcher a hit may come from, with a flag saying whether it
    /// contains a top-level combinator.
    pub(crate) fn matchers(&self) -> impl Iterator<Item = (&Selector<BrowserImpl>, bool)> {
        std::iter::once((&self.matcher, self.has_combinator)).chain(
            self.subtree_head
                .as_ref()
                .map(|(matcher, has_combinator)| (matcher, *has_combinator)),
        )
    }

    /// Does the sub-selector contain a top-level combinator (` `, `>`, `+`,
    /// `~`)?
    ///
    /// `crate::matching` needs this to reproduce lxml's scoping: a selector
    /// with a combinator can never select the node the query was issued on,
    /// because every anchor it could use (an ancestor, or a preceding sibling)
    /// lies outside `descendant-or-self::` of that node. Combinators *inside*
    /// `:not()` / `:has()` do not count — those are self-contained.
    #[must_use]
    pub fn has_combinator(&self) -> bool {
        self.has_combinator
    }

    /// The compiled Servo matcher.
    #[must_use]
    pub fn matcher(&self) -> &Selector<BrowserImpl> {
        &self.matcher
    }

    /// What a match yields: the element, its text nodes, or an attribute value.
    #[must_use]
    pub fn tail(&self) -> &Tail {
        &self.tail
    }

    /// cssselect's `single.canonical()` — the adaptive-storage identifier.
    ///
    /// Includes the pseudo-element (`"a::attr(href)"`), exactly as Python's
    /// does. The *spelling* is Servo's, not cssselect's; see
    /// [`crate::canonical`] for the register of cosmetic differences.
    #[must_use]
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// CSS specificity of the sub-selector. Not used by the reference implementation; exposed
    /// because it is free and callers ordering rules will want it.
    #[must_use]
    pub fn specificity(&self) -> u32 {
        self.matcher.specificity()
    }

    /// Does `element` match, ignoring the tail?
    ///
    /// Ancestors are followed all the way to the document root, i.e. this is
    /// `element.matches(sel)` in DOM terms, not a scoped `descendant-or-self::`.
    #[must_use]
    pub fn matches(&self, element: NodeRef<'_>) -> bool {
        matching::matches_one(self, element, None)
    }

    /// Does `element` match when the ancestor chain is clipped at `scope`?
    #[must_use]
    pub fn matches_scoped(&self, element: NodeRef<'_>, scope: NodeRef<'_>) -> bool {
        matching::matches_one(self, element, Some(scope))
    }

    /// Everything this sub-selector selects under `root`, in document order.
    ///
    /// `root` is included in the candidate set (Python's prefix is
    /// `descendant-or-self::`), and the ancestor chain of every candidate is
    /// clipped at `root`.
    #[must_use]
    pub fn select<'a>(&self, root: NodeRef<'a>) -> Vec<Match<'a>> {
        matching::select_one(self, root)
    }
}

/// A parsed CSS selector *group* — the whole comma-separated list.
///
/// This is `ARCHITECTURE.md` §1.3's `CssSelector`; [`CssSelector`] is an alias
/// for it.
#[derive(Clone, Debug)]
pub struct SelectorGroup {
    parts: Vec<SubSelector>,
}

/// `ARCHITECTURE.md` §1.3 spells this type `CssSelector`.
pub type CssSelector = SelectorGroup;

impl SelectorGroup {
    /// Parse a CSS selector group.
    ///
    /// # Errors
    ///
    /// [`SelectorError::Syntax`] when the input is not a selector, and one of
    /// the `Unknown*` / `Bad*` variants when it names a construct this port
    /// does not implement (Python's `cssselect.ExpressionError`).
    pub fn parse(selector: &str) -> Result<Self> {
        if selector.trim().is_empty() {
            return Err(SelectorError::Empty);
        }
        check_balanced(selector)?;
        let rewritten = rewrite_not_equal(selector);
        let source: &str = &rewritten;
        let mut input = ParserInput::new(source);
        let mut parser = cssparser::Parser::new(&mut input);
        let list = SelectorList::parse(&SelectorParser, &mut parser, ParseRelative::No)
            .map_err(|e| SelectorError::from_parse_error(source, e))?;
        for selector in list.slice() {
            reject_attribute_case_flags(selector)?;
        }
        let mut parts: Vec<SubSelector> =
            list.slice().iter().cloned().map(SubSelector::new).collect();
        for part in &mut parts {
            if let Some(head) = subtree_head_for(part)? {
                part.subtree_head = Some((head.matcher, head.has_combinator));
            }
        }
        if parts.is_empty() {
            return Err(SelectorError::Empty);
        }
        Ok(Self { parts })
    }

    /// The sub-selectors, in source order.
    #[must_use]
    pub fn parts(&self) -> &[SubSelector] {
        &self.parts
    }

    /// Number of sub-selectors. Always at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Always `false` — a successfully parsed group has at least one part.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// `true` when the source contained a comma, i.e. Python would take the
    /// per-sub-selector adaptive path.
    #[must_use]
    pub fn is_group(&self) -> bool {
        self.parts.len() > 1
    }

    /// cssselect's `[s.canonical() for s in cssselect.parse(sel)]` — the
    /// adaptive-storage identifiers, one per sub-selector.
    #[must_use]
    pub fn canonical_parts(&self) -> Vec<String> {
        self.parts.iter().map(|p| p.canonical.clone()).collect()
    }

    /// The whole group re-serialised, parts joined with `", "`.
    #[must_use]
    pub fn canonical(&self) -> String {
        self.canonical_parts().join(", ")
    }

    /// The tail shared by every part, if they agree.
    ///
    /// `None` for a mixed group such as `"p::text, a::attr(href)"`, which is
    /// legal and yields both kinds of result.
    #[must_use]
    pub fn common_tail(&self) -> Option<&Tail> {
        let first = self.parts.first()?.tail();
        self.parts
            .iter()
            .all(|p| p.tail() == first)
            .then_some(first)
    }

    /// Does `element` match any part of the group? (`Element.matches()`.)
    #[must_use]
    pub fn matches(&self, element: NodeRef<'_>) -> bool {
        self.parts.iter().any(|p| p.matches(element))
    }

    /// Everything the group selects under `root`, in document order with
    /// duplicates removed — the semantics of the XPath union
    /// `A | B` that Python evaluates on the non-adaptive path.
    #[must_use]
    pub fn select<'a>(&self, root: NodeRef<'a>) -> Vec<Match<'a>> {
        matching::select_group(&self.parts, root)
    }

    /// The first match in document order, short-circuiting the walk.
    #[must_use]
    pub fn select_first<'a>(&self, root: NodeRef<'a>) -> Option<Match<'a>> {
        matching::select_group_first(&self.parts, root)
    }

    /// Only the matched *elements*, ignoring every tail.
    ///
    /// Useful for `find_all`-style callers that build a selector purely as a
    /// filter (`parser.md` §3.6).
    #[must_use]
    pub fn select_elements<'a>(&self, root: NodeRef<'a>) -> Vec<NodeRef<'a>> {
        matching::select_group_elements(&self.parts, root)
    }

    /// Python's adaptive path: one result vector per sub-selector, concatenated
    /// **without** de-duplication or a document-order merge.
    ///
    /// `parser.md` §3.4 runs a separate XPath per part in that mode, so
    /// `css("p, p")` legitimately returns each `<p>` twice. Use
    /// [`SelectorGroup::select`] unless you are reproducing that path.
    #[must_use]
    pub fn select_per_part<'a>(&self, root: NodeRef<'a>) -> Vec<Vec<Match<'a>>> {
        self.parts.iter().map(|p| p.select(root)).collect()
    }
}

impl std::str::FromStr for SelectorGroup {
    type Err = SelectorError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl std::fmt::Display for SelectorGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Rejects `[a="b" i]` and `[a="b" s]`, which cssselect cannot parse.
///
/// CSS Selectors 4 added the case-sensitivity flags and Servo implements them;
/// cssselect (a CSS3-era parser) stops at the identifier and raises
/// `SelectorSyntaxError: Expected ']', got <IDENT 'i' at N>`. Accepting them
/// would be a superset like `:has(a[href])` — except that this one would
/// silently change what an *existing* selector matches, because
/// [`crate::impl_selector`] pins attribute comparison to case-sensitive to
/// reproduce cssselect's plain XPath string test. Erroring out is the honest
/// answer: the flag is never quietly ignored.
struct RejectAttrCaseFlags {
    offender: Option<&'static str>,
}

impl SelectorVisitor for RejectAttrCaseFlags {
    type Impl = BrowserImpl;

    fn visit_simple_selector(&mut self, component: &Component<BrowserImpl>) -> bool {
        let parsed = match component {
            Component::AttributeInNoNamespace {
                case_sensitivity, ..
            } => Some(*case_sensitivity),
            Component::AttributeOther(attr) => match attr.operation {
                ParsedAttrSelectorOperation::WithValue {
                    case_sensitivity, ..
                } => Some(case_sensitivity),
                ParsedAttrSelectorOperation::Exists => None,
            },
            _ => None,
        };
        self.offender = match parsed {
            Some(ParsedCaseSensitivity::AsciiCaseInsensitive) => Some("i"),
            Some(ParsedCaseSensitivity::ExplicitCaseSensitive) => Some("s"),
            // The two remaining variants are what an *unflagged* `[a=b]`
            // parses to; `attr_matches` collapses both to case-sensitive.
            _ => return true,
        };
        false
    }

    fn visit_relative_selector_list(&mut self, list: &[RelativeSelector<BrowserImpl>]) -> bool {
        // The default implementation does not descend into `:has()`.
        list.iter().all(|relative| relative.selector.visit(self))
    }
}

/// `Err` when `selector` carries an explicit attribute case-sensitivity flag.
fn reject_attribute_case_flags(selector: &Selector<BrowserImpl>) -> Result<()> {
    let mut visitor = RejectAttrCaseFlags { offender: None };
    selector.visit(&mut visitor);
    match visitor.offender {
        None => Ok(()),
        Some(flag) => Err(SelectorError::Syntax(format!(
            "Expected ']', got <IDENT '{flag}'>"
        ))),
    }
}

/// Does the selector end in a bare `*` reached by a **descendant** combinator?
///
/// That is the shape the reference implementation's translator folds into the pseudo-element; see
/// [`SubSelector::subtree_head`]. "Bare" means the compound places no condition
/// on the element at all: `*`, `*|*`, `|*` and the implicit universal of
/// `div ::text` all qualify, `*:first-child` and `p` do not.
fn ends_in_a_bare_descendant_universal(matcher: &Selector<BrowserImpl>) -> bool {
    let mut iter = matcher.iter();
    loop {
        let mut bare = true;
        for component in &mut iter {
            if !matches!(
                component,
                Component::ExplicitUniversalType
                    | Component::ExplicitAnyNamespace
                    | Component::ExplicitNoNamespace
                    | Component::DefaultNamespace(_)
                    | Component::PseudoElement(_)
            ) {
                bare = false;
            }
        }
        match iter.next_sequence() {
            // The pseudo-element is a "compound" of its own in parse order;
            // step over it to reach the compound it is attached to.
            Some(Combinator::PseudoElement | Combinator::SlotAssignment | Combinator::Part) => {}
            Some(Combinator::Descendant) => return bare,
            _ => return false,
        }
    }
}

/// Build the extra matcher for `X *::text` / `X *::attr(a)`, if the shape calls
/// for one. See [`SubSelector::subtree_head`].
fn subtree_head_for(part: &SubSelector) -> Result<Option<SubSelector>> {
    if part.tail() == &Tail::Element || !ends_in_a_bare_descendant_universal(part.matcher()) {
        return Ok(None);
    }
    let pseudo = part
        .matcher()
        .pseudo_element()
        .map(ToCss::to_css_string)
        .unwrap_or_default();
    // Our own serialisation renders the bare compound as nothing at all, so the
    // canonical string is `"<head> ::text"`; dropping the pseudo-element and the
    // space that precedes it leaves the head.
    let Some(head) = part.canonical().strip_suffix(&pseudo) else {
        return Ok(None);
    };
    let head = head.trim_end();
    if head.is_empty() {
        // `*::text` — no head to speak of, and the translator's `path == "*"`
        // branch already means the same thing as our element-wise reading.
        return Ok(None);
    }
    let source = format!("{head}{pseudo}");
    let mut group = SelectorGroup::parse(&source)?;
    if group.parts.len() != 1 {
        return Ok(None);
    }
    Ok(Some(group.parts.remove(0)))
}

/// Parse a CSS selector group — the crate's entry point.
///
/// # Errors
///
/// See [`SelectorGroup::parse`].
pub fn parse_selector(selector: &str) -> Result<SelectorGroup> {
    SelectorGroup::parse(selector)
}

/// Parse a selector and return only its canonical parts.
///
/// This is `[s.canonical() for s in cssselect.parse(sel)]`, the exact call
/// `Selector.css` makes when the adaptive feature is on and the selector
/// contains a comma.
///
/// # Errors
///
/// See [`SelectorGroup::parse`].
pub fn canonical_parts(selector: &str) -> Result<Vec<String>> {
    Ok(parse_selector(selector)?.canonical_parts())
}

/// Re-serialise one compiled sub-selector. Exposed for tests and tooling.
#[must_use]
pub fn to_css_string(selector: &Selector<BrowserImpl>) -> String {
    selector.to_css_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use markup5ever::LocalName;

    #[test]
    fn splits_a_group_into_parts() {
        let g = parse_selector("#p1, #p2").expect("parses");
        assert_eq!(g.len(), 2);
        assert!(g.is_group());
        assert_eq!(g.canonical_parts(), vec!["#p1", "#p2"]);
    }

    #[test]
    fn single_selector_is_a_one_part_group() {
        let g = parse_selector("div.a p").expect("parses");
        assert_eq!(g.len(), 1);
        assert!(!g.is_group());
        assert_eq!(g.parts()[0].tail(), &Tail::Element);
    }

    #[test]
    fn tail_is_lifted_out_of_the_matcher() {
        let g = parse_selector("p::text").expect("parses");
        assert_eq!(g.parts()[0].tail(), &Tail::Text);

        let g = parse_selector("a::attr(href)").expect("parses");
        assert_eq!(
            g.parts()[0].tail(),
            &Tail::Attr(LocalName::from("href")),
            "the attribute name survives"
        );

        // cssselect does not lower-case the ::attr() argument.
        let g = parse_selector("a::attr(HREF)").expect("parses");
        assert_eq!(g.parts()[0].tail(), &Tail::Attr(LocalName::from("HREF")));

        // ...but it does lower-case the pseudo-element name itself.
        assert_eq!(parse_selector("A::TEXT").expect("parses").len(), 1);
        assert_eq!(
            parse_selector("A::TEXT").expect("parses").parts()[0].tail(),
            &Tail::Text
        );
    }

    #[test]
    fn mixed_groups_have_no_common_tail() {
        let g = parse_selector("p::text, a::attr(href)").expect("parses");
        assert!(g.common_tail().is_none());
        let g = parse_selector("p::text, span::text").expect("parses");
        assert_eq!(g.common_tail(), Some(&Tail::Text));
    }

    #[test]
    fn syntax_errors_are_reported() {
        let err = parse_selector("4 ayo").expect_err("not a selector");
        assert!(matches!(err, SelectorError::Syntax(_)), "{err:?}");
        assert!(!err.is_expression_error());

        assert!(matches!(parse_selector(""), Err(SelectorError::Empty)));
        assert!(parse_selector("   ").is_err());
        assert!(parse_selector("div >").is_err());
        assert!(parse_selector("p >> span").is_err());
        assert!(parse_selector("]").is_err());

        // cssparser closes an unterminated block at EOF, so `[href` would
        // tokenize as `[href]`. cssselect raises instead, so `check_balanced`
        // rejects it before the tokenizer ever sees it — likewise an
        // unterminated string literal.
        assert!(parse_selector("[href").is_err());
        assert!(parse_selector("a:contains(\"x").is_err());
        assert!(parse_selector("li:nth-child(2").is_err());
        // ...but a delimiter inside a string is not a delimiter.
        assert!(parse_selector(r#"[title="a[b(c"]"#).is_ok());
        assert!(parse_selector(r"[title=a\[b]").is_ok());
    }

    #[test]
    fn unknown_pseudo_elements_match_cssselect_messages() {
        let err = parse_selector("a::foo").expect_err("unknown");
        assert_eq!(err.to_string(), "The pseudo-element ::foo is unknown");
        assert!(err.is_expression_error());

        let err = parse_selector("a::data(x)").expect_err("unknown");
        assert_eq!(
            err.to_string(),
            "The functional pseudo-element ::data() is unknown"
        );
    }

    #[test]
    fn attr_argument_must_be_a_single_ident_or_string() {
        assert_eq!(
            parse_selector("a::attr()").expect_err("empty").to_string(),
            "Expected a single string or ident for ::attr(), got []"
        );
        let err = parse_selector("a::attr(a b)").expect_err("two tokens");
        assert!(
            err.to_string()
                .starts_with("Expected a single string or ident for ::attr(), got "),
            "{err}"
        );
        // A quoted argument is accepted, like cssselect's ::attr("href").
        assert_eq!(
            parse_selector("a::attr(\"href\")").expect("parses").parts()[0].tail(),
            &Tail::Attr(LocalName::from("href"))
        );
    }

    #[test]
    fn pseudo_element_must_be_last() {
        // cssselect: "Got pseudo-element ::text not at the end of a selector".
        assert!(parse_selector("a::text::attr(x)").is_err());
        assert!(parse_selector("a::text b").is_err());
    }

    #[test]
    fn contains_takes_one_ident_or_string() {
        assert!(parse_selector("a:contains(\"Phone\")").is_ok());
        assert!(parse_selector("a:contains(Phone)").is_ok());
        let err = parse_selector("a:contains()").expect_err("empty");
        assert_eq!(
            err.to_string(),
            "Expected a single string or ident for :contains(), got []"
        );
    }

    #[test]
    fn unknown_pseudo_classes_are_rejected() {
        let err = parse_selector("a:nonesuch").expect_err("unknown");
        assert_eq!(err.to_string(), "The pseudo-class :nonesuch is unknown");
        let err = parse_selector("a:nonesuch(3)").expect_err("unknown");
        assert_eq!(err.to_string(), "The pseudo-class :nonesuch() is unknown");
    }

    #[test]
    fn structural_and_logical_pseudo_classes_parse() {
        for sel in [
            "div:first-child",
            "li:nth-of-type(2)",
            "e:nth-child(2n+1)",
            "li:nth-last-child(1)",
            "li:only-child",
            "li:only-of-type",
            "li:first-of-type",
            "li:last-of-type",
            "div:empty",
            ":root",
            ":scope > div",
            "div:not(.hidden)",
            "div:is(.a, .b)",
            "div:where(.a)",
            "div:has(> p)",
            "div:has(p)",
            "input:checked",
            "input:enabled",
            "input:disabled",
            "a:link",
            "a:visited",
            "a:hover",
            "a:active",
            "a:focus",
            "a:target",
            "p:lang(en)",
            "*",
            "[data-x]",
            "a[href^='/p/']",
            "div > p ~ span + b",
        ] {
            assert!(parse_selector(sel).is_ok(), "should parse: {sel}");
        }
    }

    #[test]
    fn foreign_content_is_reached_without_a_namespace_prefix() {
        use browser_dom::Document;

        let doc = Document::parse("<a href='/h'>html</a><svg><a href='/s'>svg</a></svg>");
        let root = doc.root();

        // No default namespace: a bare `a` matches both, exactly as lxml's
        // namespace-free HTML tree does. `*|a` is the same thing spelled out.
        assert_eq!(parse_selector("a").expect("parses").select(root).len(), 2);
        assert_eq!(parse_selector("*|a").expect("parses").select(root).len(), 2);

        // cssselect's HTML pseudo-classes test `name(.)`, which in libxml2's
        // HTML tree is namespace-free — so the SVG `<a href>` is a `:link`
        // there and must be one here too.
        assert_eq!(
            parse_selector(":link").expect("parses").select(root).len(),
            2
        );

        // Every *prefixed* selector raises in Python: cssselect translates it
        // to the XPath name test `svg:a`, which lxml cannot evaluate because
        // the prefix is unbound. We reject it at parse time instead.
        for sel in ["svg|a", "html|a", "xhtml|a", "bogus|a"] {
            assert!(parse_selector(sel).is_err(), "should not parse: {sel}");
        }
    }

    #[test]
    fn the_empty_namespace_means_any_namespace() {
        use browser_dom::Document;

        // cssselect turns `|div` into the XPath name test `div`, and libxml2's
        // HTML tree namespaces *nothing* — not even the contents of an inline
        // <svg> — so Python matches every `div` and every `a`, wherever they
        // are. Comparing against html5ever's real namespaces would select
        // nothing at all.
        let doc = Document::parse(
            "<div id='h'><a href='/a'>x</a></div><svg><a href='/s'><foreignObject>\
             <div id='f' xmlns='http://www.w3.org/1999/xhtml'>y</div></foreignObject></a></svg>",
        );
        let root = doc.root();
        let count = |sel: &str| parse_selector(sel).expect("parses").select(root).len();
        assert_eq!(count("|div"), 2);
        assert_eq!(count("|a"), 2);
        assert_eq!(count("*|div"), 2);
        assert_eq!(count("div"), 2);
    }

    #[test]
    fn not_equals_is_rewritten_to_standard_css() {
        assert_eq!(rewrite_not_equal("div"), "div");
        assert_eq!(rewrite_not_equal(r#"[a!="b"]"#), r#":not([a="b"])"#);
        assert_eq!(
            rewrite_not_equal(r#"p[a!="b"] > i"#),
            r#"p:not([a="b"]) > i"#
        );
        // The empty-value case keeps cssselect's `@n != ''`, which requires the
        // attribute to be present.
        assert_eq!(rewrite_not_equal(r#"[a!=""]"#), r#"[a]:not([a=""])"#);
        // A `!` inside a string is not an operator.
        assert_eq!(rewrite_not_equal(r#"[a="b!=c"]"#), r#"[a="b!=c"]"#);
        assert_eq!(rewrite_not_equal("[a=b][c!=d]"), r#"[a=b]:not([c=d])"#);
    }

    #[test]
    fn not_equals_selects_like_cssselect() {
        use browser_dom::Document;

        let doc = Document::parse(
            "<p id=a title=x>1</p><p id=b title=y>2</p><p id=c>3</p><p id=d title=''>4</p>",
        );
        let root = doc.root();
        let ids = |sel: &str| -> Vec<&str> {
            parse_selector(sel)
                .expect("parses")
                .select(root)
                .iter()
                .filter_map(|m| m.element().element_id())
                .collect()
        };
        // cssselect: `not(@title) or @title != 'x'`
        assert_eq!(ids("p[title!='x']"), vec!["b", "c", "d"]);
        // cssselect: `@title != ''` — the attribute must exist.
        assert_eq!(ids("p[title!='']"), vec!["a", "b"]);
    }

    #[test]
    fn selectors_level_4_extras_the_docs_promise() {
        use browser_dom::Document;

        let doc = Document::parse(
            "<p class='A' data-x='HELLO'>1</p><p class='b' data-x='hello'>2</p><span>3</span>",
        );
        let root = doc.root();
        let count = |sel: &str| parse_selector(sel).expect("parses").select(root).len();

        // Attribute values compare case-sensitively, always — cssselect emits a
        // plain XPath string test and knows nothing of HTML's
        // case-insensitive-attribute list, so `[type=TEXT]` must not match
        // `type="text"` here either (`tests/css_golden.rs`, page `quirks`).
        assert_eq!(count("[data-x='hello']"), 1);
        assert_eq!(count("[data-x='HELLO']"), 1);
        // And the Selectors-4 flags that would reintroduce it are rejected,
        // exactly as cssselect rejects them, rather than silently ignored.
        for flagged in [
            "[data-x='hello' i]",
            "[data-x='hello' s]",
            "p:not([class='a' i])",
        ] {
            let err = parse_selector(flagged).expect_err("cssselect rejects the flag");
            assert!(err.to_string().contains("Expected ']'"), "{flagged}: {err}");
        }

        // `:matches()` / `:any()` are cssselect's and the browsers' spellings
        // of `:is()`.
        assert_eq!(count("p:is(.A, .b)"), 2);
        assert_eq!(count("p:matches(.A, .b)"), 2);
        assert_eq!(count("p:any(.A, .b)"), 2);

        // cssselect raises `*:nth-of-type() is not implemented`; Servo does not.
        assert_eq!(
            count("*:nth-of-type(1)"),
            5,
            "html, head, body, the first <p>, the <span>"
        );

        // CSS Selectors 4 makes `:is()`/`:where()` forgiving selector lists;
        // cssselect (a CSS3 parser) raises, so `allow_forgiving_selectors` is
        // off and an unparsable branch fails the whole selector.
        assert!(parse_selector("p:is(.A, ::bogus)").is_err());
        assert!(parse_selector("p:where(.A, %%%)").is_err());
    }

    #[test]
    fn from_str_and_display_round_trip() {
        let g: SelectorGroup = "#p1, #p2".parse().expect("parses");
        assert_eq!(g.to_string(), "#p1, #p2");
    }
}
