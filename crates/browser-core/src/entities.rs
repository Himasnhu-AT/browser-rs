//! Hand-port of `w3lib.html.replace_entities`, over the build-time HTML4 `phf::Map`
//! and the cp1252 `0x80..=0x9F` remap table.
//!
//! This is **not** HTML5 unescaping and no off-the-shelf Rust decoder matches it
//! (`ARCHITECTURE.md` §6). Four points where it differs from every HTML5 decoder:
//!
//! 1. Named references resolve against Python's HTML4 `html.entities.name2codepoint`
//!    table only, so `&apos;` — HTML5-only — is *not* a known name.
//! 2. The trailing `;` is optional: `&amp` decodes just like `&amp;`.
//! 3. An unknown reference that *is* `;`-terminated is **deleted**, not preserved
//!    (`&nosuch;` → `""`), while an unterminated one is passed through verbatim.
//! 4. Code points `0x80..=0x9F` are remapped through Windows-1252, the way browsers
//!    do, rather than emitted as C1 control characters.
//!
//! The reference implementation always calls it with the defaults, i.e. [`replace_entities`]:
//! `keep = ()`, `remove_illegal = true`.

use std::sync::OnceLock;

use regex::{Captures, Regex};

include!(concat!(env!("OUT_DIR"), "/html4_entities.rs"));

/// Windows-1252 mappings for the bytes `0x80..=0x9F`, indexed by `codepoint - 0x80`.
///
/// `None` marks the five byte values cp1252 leaves undefined (`0x81`, `0x8D`, `0x8F`,
/// `0x90`, `0x9D`). Python's `bytes((n,)).decode("cp1252")` raises `UnicodeDecodeError`
/// for those — a `ValueError` subclass, so w3lib's `except (ValueError, OverflowError)`
/// swallows it and the reference falls through to the delete/passthrough rule.
const CP1252: [Option<u32>; 32] = [
    Some(0x20AC), // 0x80 €
    None,         // 0x81
    Some(0x201A), // 0x82 ‚
    Some(0x0192), // 0x83 ƒ
    Some(0x201E), // 0x84 „
    Some(0x2026), // 0x85 …
    Some(0x2020), // 0x86 †
    Some(0x2021), // 0x87 ‡
    Some(0x02C6), // 0x88 ˆ
    Some(0x2030), // 0x89 ‰
    Some(0x0160), // 0x8A Š
    Some(0x2039), // 0x8B ‹
    Some(0x0152), // 0x8C Œ
    None,         // 0x8D
    Some(0x017D), // 0x8E Ž
    None,         // 0x8F
    None,         // 0x90
    Some(0x2018), // 0x91 '
    Some(0x2019), // 0x92 '
    Some(0x201C), // 0x93 "
    Some(0x201D), // 0x94 "
    Some(0x2022), // 0x95 •
    Some(0x2013), // 0x96 –
    Some(0x2014), // 0x97 —
    Some(0x02DC), // 0x98 ˜
    Some(0x2122), // 0x99 ™
    Some(0x0161), // 0x9A š
    Some(0x203A), // 0x9B ›
    Some(0x0153), // 0x9C œ
    None,         // 0x9D
    Some(0x017E), // 0x9E ž
    Some(0x0178), // 0x9F Ÿ
];

/// w3lib's `_ent_re`, transcribed. The alternation order matters: `#(?<dec>\d+)`
/// is tried before `#x(?<hex>[a-f\d]+)`, which is why `&#x27;` still reaches the
/// hex branch (`x` is not a digit, so the decimal branch cannot match).
fn entity_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)&((?P<named>[a-z\d]+)|#(?P<dec>\d+)|#x(?P<hex>[a-f\d]+))(?P<semi>;?)")
            .expect("the static entity pattern is valid")
    })
}

/// Look a named reference up the way w3lib does: exact match first, then the
/// lowercased name (`name2codepoint.get(n) or name2codepoint.get(n.lower())`).
///
/// The two-step lookup is why `&AMP;` decodes but `&Nbsp;` also does — the table
/// itself is mixed-case (`AElig` and `aelig` are separate entries).
fn lookup_named(name: &str) -> Option<u32> {
    if let Some(&cp) = HTML4_ENTITIES.get(name) {
        return Some(cp);
    }
    HTML4_ENTITIES.get(name.to_lowercase().as_str()).copied()
}

/// Turn a resolved code point into its replacement text, or `None` when Python
/// would have raised (and w3lib would have fallen through).
fn codepoint_to_string(number: u32) -> Option<String> {
    if (0x80..=0x9F).contains(&number) {
        // Browser-compatible Windows-1252 remap.
        let mapped = CP1252[(number - 0x80) as usize]?;
        return char::from_u32(mapped).map(String::from);
    }
    // `chr()` raises ValueError above 0x10FFFF, which w3lib swallows.
    //
    // Divergence: Python's `chr()` happily produces a *lone surrogate* for
    // 0xD800..=0xDFFF; Rust's `char` cannot represent one, so those fall through
    // to the delete/passthrough rule instead. See `docs/DIVERGENCES.md`.
    char::from_u32(number).map(String::from)
}

/// Replace HTML character references in `text`, exactly as
/// `w3lib.html.replace_entities(text)` does with its default arguments.
///
/// ```
/// use browser_core::entities::replace_entities;
///
/// assert_eq!(replace_entities("&lt;div&gt;"), "<div>");
/// assert_eq!(replace_entities("&amp"), "&");          // trailing `;` optional
/// assert_eq!(replace_entities("&nosuch;"), "");       // terminated + unknown → deleted
/// assert_eq!(replace_entities("&nosuch"), "&nosuch"); // unterminated → verbatim
/// assert_eq!(replace_entities("&#153;"), "™");        // cp1252 remap of 0x99
/// ```
pub fn replace_entities(text: &str) -> String {
    replace_entities_with(text, &[], true)
}

/// The full w3lib signature.
///
/// * `keep` — named references (compared **lowercased**) to leave untouched.
/// * `remove_illegal` — when `true`, an unresolvable but `;`-terminated reference is
///   deleted; when `false` it is passed through verbatim.
///
/// The `encoding` parameter of the Python function has no analogue: `&str` is already
/// decoded.
///
/// ```
/// use browser_core::entities::replace_entities;
/// use browser_core::entities::replace_entities_with;
///
/// assert_eq!(replace_entities_with("&amp;&lt;", &["amp"], true), "&amp;<");
/// assert_eq!(replace_entities_with("&nosuch;", &[], false), "&nosuch;");
/// assert_eq!(replace_entities("&nosuch;"), "");
/// ```
pub fn replace_entities_with(text: &str, keep: &[&str], remove_illegal: bool) -> String {
    // Cheap bail-out: no `&`, nothing to do. Matches the observable behaviour of the
    // regex substitution while skipping the scan on the overwhelmingly common case.
    if !text.contains('&') {
        return text.to_owned();
    }

    entity_re()
        .replace_all(text, |caps: &Captures<'_>| {
            convert_entity(caps, keep, remove_illegal)
        })
        .into_owned()
}

fn convert_entity(caps: &Captures<'_>, keep: &[&str], remove_illegal: bool) -> String {
    let whole = caps
        .get(0)
        .map(|m| m.as_str())
        .unwrap_or_default()
        .to_owned();
    let semicolon = caps.name("semi").is_some_and(|m| !m.as_str().is_empty());

    let number: Option<u32> = if let Some(dec) = caps.name("dec") {
        // Python parses arbitrary precision then `chr()` raises; a `u32` overflow
        // here lands in the same fall-through branch.
        dec.as_str().parse::<u32>().ok().or(Some(u32::MAX))
    } else if let Some(hex) = caps.name("hex") {
        u32::from_str_radix(hex.as_str(), 16)
            .ok()
            .or(Some(u32::MAX))
    } else if let Some(named) = caps.name("named") {
        let name = named.as_str();
        let lowered = name.to_lowercase();
        if keep.iter().any(|k| *k == lowered) {
            return whole;
        }
        lookup_named(name)
    } else {
        None
    };

    if let Some(number) = number {
        if let Some(s) = codepoint_to_string(number) {
            return s;
        }
    }

    if remove_illegal && semicolon {
        String::new()
    } else {
        whole
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_the_html4_set() {
        assert_eq!(HTML4_ENTITY_COUNT, 252);
        assert_eq!(HTML4_ENTITIES.get("amp").copied(), Some(38));
        assert_eq!(HTML4_ENTITIES.get("nbsp").copied(), Some(160));
        assert_eq!(HTML4_ENTITIES.get("AElig").copied(), Some(198));
        // HTML5-only, deliberately absent — this is the point of using HTML4.
        assert_eq!(HTML4_ENTITIES.get("apos").copied(), None);
    }

    #[test]
    fn named_references_decode() {
        assert_eq!(replace_entities("&amp;"), "&");
        assert_eq!(replace_entities("&lt;div&gt;"), "<div>");
        assert_eq!(replace_entities("&copy; 2024"), "\u{a9} 2024");
    }

    #[test]
    fn trailing_semicolon_is_optional() {
        assert_eq!(replace_entities("&amp"), "&");
        // …but the name is matched greedily, so `&ampx` is the *unknown* name
        // `ampx`, not `amp` followed by an `x`.
        assert_eq!(replace_entities("&ampx"), "&ampx");
    }

    #[test]
    fn lookup_is_case_insensitive_via_the_lowercase_fallback() {
        assert_eq!(replace_entities("&AMP;"), "&");
        assert_eq!(replace_entities("&Amp;"), "&");
    }

    #[test]
    fn apostrophe_is_not_an_html4_name() {
        // `&apos;` is unknown *and* terminated → deleted.
        assert_eq!(replace_entities("&apos;"), "");
        // Unterminated → verbatim.
        assert_eq!(replace_entities("&apos"), "&apos");
    }

    #[test]
    fn unknown_terminated_references_are_deleted() {
        assert_eq!(replace_entities("&nosuch;"), "");
        assert_eq!(replace_entities("a&nosuch;b"), "ab");
        assert_eq!(replace_entities("&nosuch"), "&nosuch");
    }

    #[test]
    fn numeric_references_decode() {
        assert_eq!(replace_entities("&#39;"), "'");
        assert_eq!(replace_entities("&#x27;"), "'");
        assert_eq!(replace_entities("&#X27;"), "'");
        assert_eq!(replace_entities("&#00039;"), "'");
        assert_eq!(replace_entities("&#0;"), "\0");
    }

    #[test]
    fn c1_range_is_remapped_through_cp1252() {
        assert_eq!(replace_entities("&#128;"), "\u{20ac}");
        assert_eq!(replace_entities("&#x80;"), "\u{20ac}");
        assert_eq!(replace_entities("&#153;"), "\u{2122}");
        assert_eq!(replace_entities("&#x99;"), "\u{2122}");
        // The five cp1252-undefined bytes fall through.
        assert_eq!(replace_entities("&#129;"), "");
        assert_eq!(replace_entities("&#141;"), "");
        assert_eq!(replace_entities("&#x81;"), "");
        assert_eq!(replace_entities("&#129"), "&#129");
    }

    #[test]
    fn out_of_range_numeric_references_fall_through() {
        assert_eq!(replace_entities("&#1114111;"), "\u{10ffff}");
        assert_eq!(replace_entities("&#1114112;"), "");
        assert_eq!(replace_entities("&#x110000;"), "");
        assert_eq!(replace_entities("&#99999999999999999999;"), "");
        assert_eq!(replace_entities("&#x110000"), "&#x110000");
    }

    #[test]
    fn keep_leaves_named_references_alone() {
        assert_eq!(replace_entities_with("&amp;&lt;", &["amp"], true), "&amp;<");
        assert_eq!(replace_entities_with("&AMP;&lt;", &["amp"], true), "&AMP;<");
    }

    #[test]
    fn remove_illegal_false_preserves_unknown_references() {
        assert_eq!(replace_entities_with("&nosuch;", &[], false), "&nosuch;");
        assert_eq!(
            replace_entities_with("&#x110000;", &[], false),
            "&#x110000;"
        );
    }

    #[test]
    fn no_ampersand_short_circuit_is_transparent() {
        assert_eq!(replace_entities(""), "");
        assert_eq!(replace_entities("plain"), "plain");
        assert_eq!(replace_entities("&"), "&");
        assert_eq!(replace_entities("&;"), "&;");
        assert_eq!(replace_entities("&#"), "&#");
    }

    #[test]
    fn double_encoding_is_only_undone_once() {
        // `&amp;amp;` → `&amp;`, not `&`.
        assert_eq!(replace_entities("&amp;amp;"), "&amp;");
    }
}
