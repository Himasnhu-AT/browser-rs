//! [`QualName`] / [`LocalName`] helpers over `markup5ever`'s string interner.
//!
//! `markup5ever` interns every element and attribute name into an `Atom`, so
//! name comparisons are pointer comparisons and cloning a name is free. This
//! module is the only place in the crate that constructs names, so the
//! namespace conventions are stated once:
//!
//! * **Element names** live in the HTML namespace (`ns!(html)`) unless the
//!   parser put them in the SVG or MathML namespace (foreign content).
//! * **Attribute names** live in the *empty* namespace, exactly as html5ever
//!   produces them. Only `xml:`, `xlink:` and `xmlns:` attributes carry a real
//!   namespace, and those keep the prefix they were parsed with.

use markup5ever::{ns, LocalName, Namespace, Prefix, QualName};

pub use markup5ever::{LocalName as Local, Namespace as Ns, QualName as Name};

/// Build an element name in the HTML namespace.
///
/// ```
/// # use browser_dom::qualname::html_name;
/// assert_eq!(html_name("div").local.as_ref(), "div");
/// ```
#[must_use]
pub fn html_name(local: &str) -> QualName {
    QualName::new(None, ns!(html), LocalName::from(local))
}

/// Build an attribute name in the empty namespace (the html5ever convention).
///
/// ```
/// # use browser_dom::qualname::attr_name;
/// assert_eq!(attr_name("href").local.as_ref(), "href");
/// assert_eq!(attr_name("href").ns.as_ref(), "");
/// ```
#[must_use]
pub fn attr_name(local: &str) -> QualName {
    QualName::new(None, Namespace::from(""), LocalName::from(local))
}

/// Build a namespaced name with an explicit prefix (`xlink:href` and friends).
#[must_use]
pub fn prefixed_name(prefix: &str, ns: &str, local: &str) -> QualName {
    QualName::new(
        Some(Prefix::from(prefix)),
        Namespace::from(ns),
        LocalName::from(local),
    )
}

/// The serialised form of a name: `local`, or `prefix:local` when the name
/// carries a prefix (`xlink:href`).
#[must_use]
pub fn to_string(name: &QualName) -> String {
    match &name.prefix {
        Some(p) => format!("{}:{}", p.as_ref(), name.local.as_ref()),
        None => name.local.to_string(),
    }
}

/// HTML void elements — serialised without a closing tag.
///
/// The list is the one in the HTML standard (§13.1.2), matching html5ever's own
/// serialiser.
#[must_use]
pub fn is_void_element(local: &LocalName) -> bool {
    matches!(
        local.as_ref(),
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Elements whose text children are serialised **raw** (no entity escaping).
#[must_use]
pub fn is_raw_text_element(local: &LocalName) -> bool {
    matches!(
        local.as_ref(),
        "style" | "script" | "xmp" | "iframe" | "noembed" | "noframes" | "plaintext" | "noscript"
    )
}

/// Elements whose character data libxml2's HTML parser treats as CDATA, so
/// `areBlanks()` is never consulted for it and blank text always survives
/// [`crate::parse::remove_blank_text`].
///
/// Determined by probing libxml2 2.14.6 through `lxml` — `<X> </X>` keeps its
/// space for every name below, and `<X><ul></ul> <em>y</em></X>` keeps the
/// space too (proving the content never reaches `areBlanks()` rather than
/// merely being allowed PCDATA).
///
/// **Not** on this list, despite the name: `pre` and `listing`. libxml2 2.14
/// parses both as ordinary element content, so their blanks go through the
/// normal [`allows_pcdata`] rule — which keeps `<pre> </pre>` (because `pre`
/// allows PCDATA) but *drops* the space in `<pre><ul></ul> <em/></pre>`.
#[must_use]
pub fn has_cdata_content(local: &LocalName) -> bool {
    matches!(
        local.as_ref(),
        "script"
            | "style"
            | "textarea"
            | "title"
            | "xmp"
            | "noembed"
            | "noframes"
            | "plaintext"
            | "iframe"
    )
}

/// libxml2's `allowPCData` table: elements whose content model admits
/// character data, which is what its `areBlanks()` consults to decide whether a
/// whitespace-only run is ignorable.
///
/// The list is **empirical**, not copied from a header: every HTML4 and HTML5
/// element name (plus a handful of legacy and unknown ones) was fed to
/// libxml2 2.14.6 twice, as the blank's parent (`<X> </X>`) and as the blank's
/// preceding sibling (`<div><X></X> <em>y</em></div>`), and the two probes
/// agree everywhere they are both constructible. Notably it is *not* the
/// HTML 4.01 `%inline;` closure — `ul`, `ol`, `dl`, `dir`, `menu`, `map`, `sub`
/// and `sup` are all absent, while `noframes` and `noscript` are present.
///
/// Unknown element names (`<acme>`, `<foobar>`) are never on the list, so a
/// blank alone inside one is ignorable.
#[must_use]
pub fn allows_pcdata(local: &LocalName) -> bool {
    matches!(
        local.as_ref(),
        "a" | "abbr"
            | "acronym"
            | "address"
            | "applet"
            | "b"
            | "bdo"
            | "big"
            | "blockquote"
            | "body"
            | "button"
            | "caption"
            | "center"
            | "cite"
            | "code"
            | "dd"
            | "del"
            | "dfn"
            | "div"
            | "dt"
            | "em"
            | "font"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "i"
            | "iframe"
            | "ins"
            | "kbd"
            | "label"
            | "legend"
            | "li"
            | "noframes"
            | "noscript"
            | "object"
            | "p"
            | "pre"
            | "q"
            | "s"
            | "samp"
            | "small"
            | "span"
            | "strike"
            | "strong"
            | "td"
            | "th"
            | "tt"
            | "u"
            | "var"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_interned_and_namespaced() {
        let a = html_name("div");
        let b = html_name("div");
        assert_eq!(a, b);
        assert_eq!(a.ns, ns!(html));
        assert_eq!(attr_name("id").ns.as_ref(), "");
    }

    #[test]
    fn prefixed_names_serialise_with_their_prefix() {
        let n = prefixed_name("xlink", "http://www.w3.org/1999/xlink", "href");
        assert_eq!(to_string(&n), "xlink:href");
        assert_eq!(to_string(&html_name("p")), "p");
    }

    #[test]
    fn element_classification() {
        assert!(is_void_element(&LocalName::from("br")));
        assert!(!is_void_element(&LocalName::from("div")));
        assert!(is_raw_text_element(&LocalName::from("script")));
        assert!(has_cdata_content(&LocalName::from("script")));
        assert!(!has_cdata_content(&LocalName::from("pre")));
        assert!(!has_cdata_content(&LocalName::from("listing")));
        assert!(allows_pcdata(&LocalName::from("div")));
        assert!(allows_pcdata(&LocalName::from("pre")));
        assert!(allows_pcdata(&LocalName::from("body")));
        assert!(!allows_pcdata(&LocalName::from("ul")));
        assert!(!allows_pcdata(&LocalName::from("tr")));
        assert!(!allows_pcdata(&LocalName::from("sub")));
        assert!(!allows_pcdata(&LocalName::from("acme")));
    }
}
