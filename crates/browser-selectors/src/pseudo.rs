//! `BrowserPseudoClass::Contains` and `BrowserPseudoElement::{Text, Attr}` —
//! cssselect's `:contains()` plus the reference implementation's `::text` / `::attr()`.
//!
//! # What is in here and why
//!
//! Servo's `selectors` crate implements every *tree-structural* pseudo-class
//! itself (`:root`, `:empty`, `:first-child`, `:nth-of-type()`, `:not()`,
//! `:is()`, `:where()`, `:has()`, `:scope`, …). Everything else is delegated to
//! the embedder through `SelectorImpl::NonTSPseudoClass`. Python gets those from
//! `cssselect`'s `HTMLTranslator`, so this enum is exactly cssselect's
//! non-structural set (`parser.md` §6.4):
//!
//! | selector | cssselect XPath | here |
//! |---|---|---|
//! | `:contains('v')` | `contains(., 'v')` | [`BrowserPseudoClass::Contains`] |
//! | `:link` | `@href and name(.) in (a, link, area)` | [`BrowserPseudoClass::Link`] |
//! | `:visited` `:hover` `:active` `:focus` `:target` | the constant `0` | never match |
//! | `:checked` | see [`BrowserPseudoClass::Checked`] | ditto |
//! | `:enabled` / `:disabled` | the HTML5 form-control rules | ditto |
//! | `:lang('v')` | nearest `@lang` ancestor-or-self | [`BrowserPseudoClass::Lang`] |
//!
//! The pseudo-*elements* are the reference implementation's own two additions on top of cssselect
//! (`parser.md` §6.2). **`::data()` deliberately does not exist** — 0.4.15 adds
//! only `::text` and `::attr()`, and anything else raises `ExpressionError`.

use std::fmt;

use cssparser::{serialize_identifier, serialize_string, ToCss};
use markup5ever::LocalName;
use selectors::parser::{NonTSPseudoClass, PseudoElement};

use crate::impl_selector::BrowserImpl;

/// The non-tree-structural pseudo-classes this port understands.
///
/// This is cssselect's set, not a browser's: it is what `parser.md` §6.4
/// enumerates, no more and no less.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BrowserPseudoClass {
    /// `:contains("v")` — a dropped CSS3 draft feature that `cssselect` still
    /// implements as `contains(., 'v')`, i.e. a substring test against the
    /// element's **whole string-value including descendants**.
    ///
    /// Not a the reference implementation extension despite how it is usually used
    /// (`parser.md` §6.2).
    Contains(String),

    /// `:lang("v")` — cssselect's HTML translation walks
    /// `ancestor-or-self::*[@lang][1]` and tests
    /// `starts-with(concat(lower(@lang), '-'), 'v-')`.
    ///
    /// Note that only the *element's* value is lower-cased; the argument is
    /// used verbatim, so `:lang("EN")` matches nothing. That asymmetry is
    /// cssselect's and is reproduced here.
    Lang(String),

    /// `:link` — `@href` on `<a>`, `<link>` or `<area>`.
    Link,
    /// `:visited` — cssselect compiles this to the constant `0`; a static tree
    /// has no browsing history, so it never matches.
    Visited,
    /// `:hover` — never matches (constant `0` in cssselect).
    Hover,
    /// `:active` — never matches (constant `0` in cssselect).
    Active,
    /// `:focus` — never matches (constant `0` in cssselect).
    Focus,
    /// `:target` — never matches (constant `0` in cssselect).
    Target,

    /// `:checked` — `(@selected and option) or (@checked and (input|command) and
    /// @type in (checkbox, radio))`.
    Checked,
    /// `:enabled` — the HTML5 rules cssselect encodes; see
    /// `matching::matches_enabled`.
    Enabled,
    /// `:disabled` — the HTML5 rules cssselect encodes; see
    /// `matching::matches_disabled`.
    Disabled,
}

impl BrowserPseudoClass {
    /// The name as it appears in a selector, without the leading colon.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Contains(_) => "contains",
            Self::Lang(_) => "lang",
            Self::Link => "link",
            Self::Visited => "visited",
            Self::Hover => "hover",
            Self::Active => "active",
            Self::Focus => "focus",
            Self::Target => "target",
            Self::Checked => "checked",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    /// `true` for the pseudo-classes cssselect compiles to the constant `0`.
    ///
    /// They parse (so `div:hover` is not a syntax error) but can never select
    /// anything in a static document.
    #[must_use]
    pub fn never_matches(&self) -> bool {
        matches!(
            self,
            Self::Visited | Self::Hover | Self::Active | Self::Focus | Self::Target
        )
    }
}

impl ToCss for BrowserPseudoClass {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        dest.write_char(':')?;
        dest.write_str(self.name())?;
        match self {
            Self::Contains(value) | Self::Lang(value) => {
                dest.write_char('(')?;
                serialize_string(value, dest)?;
                dest.write_char(')')
            }
            _ => Ok(()),
        }
    }
}

impl NonTSPseudoClass for BrowserPseudoClass {
    type Impl = BrowserImpl;

    fn is_active_or_hover(&self) -> bool {
        matches!(self, Self::Active | Self::Hover)
    }

    fn is_user_action_state(&self) -> bool {
        matches!(self, Self::Active | Self::Hover | Self::Focus)
    }
}

/// the reference implementation's two pseudo-elements (`parser.md` §6.2).
///
/// Unlike a real CSS pseudo-element these do not describe a box; they change
/// *what a match yields*. `p::text` yields the `<p>`'s direct text-node
/// children, `a::attr(href)` yields the value of its `href` attribute. The
/// trailing pseudo-element of a parsed selector is lifted out into a [`Tail`]
/// by `parse`, and applied by `matching`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BrowserPseudoElement {
    /// `::text` — lxml's `/text()`.
    Text,
    /// `::attr(name)` — lxml's `/@name`.
    ///
    /// The name is **not** lower-cased, matching
    /// `css_to_xpath("a::attr(HREF)") == "descendant-or-self::a/@HREF"`. Since
    /// html5ever (like libxml2 in HTML mode) lower-cases attribute names at
    /// parse time, `::attr(HREF)` selects nothing — exactly as in Python.
    Attr(LocalName),
}

impl ToCss for BrowserPseudoElement {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        match self {
            Self::Text => dest.write_str("::text"),
            Self::Attr(name) => {
                dest.write_str("::attr(")?;
                serialize_identifier(name, dest)?;
                dest.write_char(')')
            }
        }
    }
}

impl PseudoElement for BrowserPseudoElement {
    type Impl = BrowserImpl;

    fn accepts_state_pseudo_classes(&self) -> bool {
        false
    }

    fn valid_after_slotted(&self) -> bool {
        false
    }
}

/// What a matched element actually yields — the trailing pseudo-element of a
/// sub-selector, lifted out of the compiled matcher.
///
/// This is `ARCHITECTURE.md` §1.3's `Tail`. `Tail::Element` is the ordinary
/// case (`div`, `#main`), and the other two are the reference implementation's pseudo-elements.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Tail {
    /// No pseudo-element: the match is the element itself.
    #[default]
    Element,
    /// `::text`: the match is each direct text-node child of the element.
    Text,
    /// `::attr(name)`: the match is the element's `name` attribute value, if present.
    Attr(LocalName),
}

impl Tail {
    /// Build a `Tail` from the pseudo-element `selectors` parsed for us.
    #[must_use]
    pub fn from_pseudo_element(pseudo: Option<&BrowserPseudoElement>) -> Self {
        match pseudo {
            None => Self::Element,
            Some(BrowserPseudoElement::Text) => Self::Text,
            Some(BrowserPseudoElement::Attr(name)) => Self::Attr(name.clone()),
        }
    }

    /// `true` when this tail turns element matches into string matches — that
    /// is, when the results are Python's `_ElementUnicodeResult` "smart
    /// strings" rather than elements (`parser.md` §2.1).
    #[must_use]
    pub fn yields_text_nodes(&self) -> bool {
        !matches!(self, Self::Element)
    }
}

impl fmt::Display for Tail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Element => Ok(()),
            Self::Text => f.write_str("::text"),
            Self::Attr(name) => write!(f, "::attr({name})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_class_serialisation() {
        assert_eq!(BrowserPseudoClass::Link.to_css_string(), ":link");
        assert_eq!(
            BrowserPseudoClass::Contains("Phone".into()).to_css_string(),
            ":contains(\"Phone\")"
        );
        assert_eq!(
            BrowserPseudoClass::Lang("en".into()).to_css_string(),
            ":lang(\"en\")"
        );
    }

    #[test]
    fn pseudo_element_serialisation() {
        assert_eq!(BrowserPseudoElement::Text.to_css_string(), "::text");
        assert_eq!(
            BrowserPseudoElement::Attr(LocalName::from("href")).to_css_string(),
            "::attr(href)"
        );
        assert_eq!(
            BrowserPseudoElement::Attr(LocalName::from("data-id")).to_css_string(),
            "::attr(data-id)"
        );
    }

    #[test]
    fn never_matching_set_is_cssselects() {
        for pc in [
            BrowserPseudoClass::Visited,
            BrowserPseudoClass::Hover,
            BrowserPseudoClass::Active,
            BrowserPseudoClass::Focus,
            BrowserPseudoClass::Target,
        ] {
            assert!(pc.never_matches(), "{pc:?}");
        }
        for pc in [
            BrowserPseudoClass::Link,
            BrowserPseudoClass::Checked,
            BrowserPseudoClass::Enabled,
            BrowserPseudoClass::Disabled,
            BrowserPseudoClass::Contains("x".into()),
            BrowserPseudoClass::Lang("en".into()),
        ] {
            assert!(!pc.never_matches(), "{pc:?}");
        }
    }

    #[test]
    fn tail_round_trips() {
        assert_eq!(Tail::from_pseudo_element(None), Tail::Element);
        assert_eq!(
            Tail::from_pseudo_element(Some(&BrowserPseudoElement::Text)),
            Tail::Text
        );
        assert_eq!(
            Tail::from_pseudo_element(Some(&BrowserPseudoElement::Attr(LocalName::from("id")))),
            Tail::Attr(LocalName::from("id"))
        );
        assert!(!Tail::Element.yields_text_nodes());
        assert!(Tail::Text.yields_text_nodes());
        assert_eq!(Tail::Attr(LocalName::from("id")).to_string(), "::attr(id)");
    }
}
