//! Canonical re-serialisation — our equivalent of cssselect's `single.canonical()`, used as
//! the adaptive-storage identifier.
//!
//! # What it is for
//!
//! `parser.md` §3.4: when the adaptive feature is on and the selector contains a
//! comma, `Selector.css` stores one fingerprint per sub-selector, keyed by
//! `single.canonical()`:
//!
//! ```text
//! parse("#p1, #p2")     -> "#p1", "#p2"
//! parse("a::attr(href)") -> "a::attr(href)"
//! ```
//!
//! So the identifier must (a) be stable for a given selector, (b) differ between
//! the parts of a group, and (c) **include the pseudo-element**.
//!
//! # Divergence from cssselect, deliberately
//!
//! `ARCHITECTURE.md` §1.3 accepts that our strings are Servo's normalisation
//! rather than cssselect's, and records the consequence: adaptive `.db` files
//! are not identifier-compatible with Python's. (The fingerprint *JSON* stays
//! byte-compatible, which is the half that decides scoring.) The known cosmetic
//! differences:
//!
//! | selector | cssselect | here |
//! |---|---|---|
//! | `a[href^="/p/"]` | `a[href^='/p/']` | `a[href^="/p/"]` |
//! | `h1:contains("Phone")` | `h1:contains('Phone')` | `h1:contains("Phone")` |
//! | `DIV.a P` | `DIV.a P` | `div.a p` (Servo lower-cases HTML type selectors) |
//! | `div:is(.a,.b)` | `div:is(.a, .b)` | `div:is(.a, .b)` |
//!
//! Everything else in `parser.md` §6.3's table round-trips character for
//! character.

use cssparser::ToCss;
use selectors::parser::Selector;

use crate::impl_selector::BrowserImpl;

/// Re-serialise one parsed sub-selector.
///
/// Equivalent to `cssselect`'s `Selector.canonical()`, modulo the cosmetic
/// differences listed in the module docs.
#[must_use]
pub fn serialize(selector: &Selector<BrowserImpl>) -> String {
    selector.to_css_string()
}

/// Re-serialise a whole group, joining the parts with `", "`.
#[must_use]
pub fn serialize_group<'a, I>(selectors: I) -> String
where
    I: IntoIterator<Item = &'a Selector<BrowserImpl>>,
{
    let parts: Vec<String> = selectors.into_iter().map(serialize).collect();
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use crate::parse::parse_selector;

    fn canonical(selector: &str) -> Vec<String> {
        parse_selector(selector).expect("parses").canonical_parts()
    }

    #[test]
    fn a_group_canonicalises_part_by_part() {
        assert_eq!(canonical("#p1, #p2"), vec!["#p1", "#p2"]);
        assert_eq!(canonical("  #p1 ,#p2  "), vec!["#p1", "#p2"]);
    }

    #[test]
    fn the_pseudo_element_is_included() {
        assert_eq!(canonical("a::attr(href)"), vec!["a::attr(href)"]);
        assert_eq!(canonical("p::text"), vec!["p::text"]);
        assert_eq!(
            canonical("h1:contains(\"Phone\")::text"),
            vec!["h1:contains(\"Phone\")::text"]
        );
        assert_eq!(canonical("a::attr(data-id)"), vec!["a::attr(data-id)"]);
    }

    #[test]
    fn combinators_and_whitespace_normalise() {
        assert_eq!(canonical("div   >   p"), vec!["div > p"]);
        assert_eq!(canonical("div>p~span+b"), vec!["div > p ~ span + b"]);
        assert_eq!(canonical("div.a    p"), vec!["div.a p"]);
    }

    #[test]
    fn attribute_selectors_normalise_quotes() {
        assert_eq!(canonical("a[ href = \"x\" ]"), vec![r#"a[href="x"]"#]);
        assert_eq!(canonical("a[href^='/p/']"), vec![r#"a[href^="/p/"]"#]);
        assert_eq!(canonical("[data-x]"), vec!["[data-x]"]);
    }

    #[test]
    fn identifiers_survive_untouched() {
        assert_eq!(canonical("*"), vec!["*"]);
        assert_eq!(canonical("li:nth-of-type(2)"), vec!["li:nth-of-type(2)"]);
        assert_eq!(canonical("div:not(.hidden)"), vec!["div:not(.hidden)"]);
        assert_eq!(canonical(":scope > div::text"), vec![":scope > div::text"]);
    }

    #[test]
    fn canonical_output_reparses_to_itself() {
        for sel in [
            "#p1, #p2",
            "a::attr(href)",
            "p::text",
            "div > p ~ span + b",
            "a[href^='/p/']",
            "div:not(.hidden)",
            "h1:contains('Phone')::text",
            "div:is(.a, .b)",
            "div:has(> p)",
            "li:nth-child(2n+1)",
            "input:checked",
            "p:lang(en)",
            "*::text",
            "section#products .product-list article.product",
        ] {
            let once = canonical(sel);
            let twice = canonical(&once.join(", "));
            assert_eq!(once, twice, "canonical form must be a fixed point: {sel}");
        }
    }
}
