//! The adaptive-relocation fingerprint and its similarity scorer.
//!
//! [`ElementFingerprint`] is the Rust shape of the dict produced by the reference implementation's
//! `_StorageTools.element_to_dict()`. The serde field names and the
//! `skip_serializing_if = "Option::is_none"` rules reproduce Python's omission rules
//! exactly, so a `.db` written by the Python implementation deserialises here and scores
//! identically — and vice versa.
//!
//! Two asymmetries of the Python original are preserved deliberately:
//!
//! * `attributes` is *cleaned* (values stripped, empty and whitespace-only values
//!   dropped) while `parent_attribs` is copied verbatim.
//! * `siblings` includes comment nodes' tags while `children` excludes them.
//!
//! [`similarity_score`] is a verbatim port of `Selector.__calculate_similarity_score`
//! (7 steps, between 3 and 11 checks) and [`dict_diff`] of
//! `Selector.__calculate_dict_diff` (0.5 / 0.5 key / value weighting). Both build on
//! [`crate::similarity`], so the numbers match `difflib` bit for bit.
//!
//! ```
//! use browser_core::fingerprint::{ElementFingerprint, similarity_score};
//!
//! let fp = ElementFingerprint {
//!     tag: "p".into(),
//!     path: vec!["html".into(), "body".into(), "p".into()],
//!     ..Default::default()
//! };
//! assert_eq!(similarity_score(&fp, &fp), 100.0);
//! ```

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::numeric::round_half_even;
use crate::similarity::{ratio_seq, ratio_str};

/// An ordered attribute map. Order is load-bearing: [`dict_diff`] compares the *ordered*
/// key tuple and the *ordered* value tuple, so re-ordering changes the score.
pub type AttributeDict = IndexMap<String, String>;

/// The attributes given a similarity check of their own, in Python's iteration order.
///
/// A structural redesign usually keeps `class` / `id` / `href` / `src` even when
/// everything around the element moves, which is why they are weighted separately.
pub const DISTINGUISHING_ATTRIBUTES: [&str; 4] = ["class", "id", "href", "src"];

/// A stored snapshot of an element, used to relocate it after a page redesign.
///
/// Serialises to the same flat JSON document Python's `orjson.dumps` produces:
///
/// ```jsonc
/// {
///   "tag": "p",
///   "attributes": {"id": "target", "class": "main"},
///   "text": "Hello",
///   "path": ["html", "body", "div", "p"],
///   "parent_name": "div",          // present iff the element has a parent
///   "parent_attribs": {"class": "  wrap  "},
///   "parent_text": "prefix",
///   "siblings": ["span", "b"],     // present only when non-empty
///   "children": ["b", "i"]         // present only when non-empty
/// }
/// ```
///
/// `parent_name`, `parent_attribs` and `parent_text` are always written together — all
/// three present, or all three absent. A tidier `Option<ParentInfo>` would break the flat
/// JSON compatibility, so the flat fields are kept.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ElementFingerprint {
    /// The element's tag name.
    pub tag: String,
    /// Cleaned attributes: values stripped, empty and whitespace-only values dropped.
    pub attributes: AttributeDict,
    /// The element's *own* leading text (before the first child), stripped; `None` when
    /// the element has no such text.
    pub text: Option<String>,
    /// Tag names from the document root down to and including the element itself.
    pub path: Vec<String>,
    /// The parent's tag name. Present iff the element has a parent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_name: Option<String>,
    /// The parent's attributes, **raw** — not stripped, empties not dropped.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_attribs: Option<AttributeDict>,
    /// The parent's own leading text, stripped.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_text: Option<String>,
    /// Tag names of the parent's other children. Absent, not empty, when there are none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub siblings: Option<Vec<String>>,
    /// Tag names of the element's children, comments excluded. Absent when there are none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub children: Option<Vec<String>>,
}

/// An empty attribute map, for the `... or {}` fallbacks of the Python original.
fn empty_attrs() -> &'static AttributeDict {
    // `IndexMap::new` is not const, so build a leaked-free static via `OnceLock`.
    use std::sync::OnceLock;
    static EMPTY: OnceLock<AttributeDict> = OnceLock::new();
    EMPTY.get_or_init(AttributeDict::new)
}

/// Python truthiness for an `Option<String>`: present *and* non-empty.
fn truthy(value: Option<&String>) -> Option<&str> {
    match value {
        Some(s) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// `value or ""` — `None` and `Some("")` both collapse to the empty string.
fn or_empty(value: Option<&String>) -> &str {
    value.map_or("", String::as_str)
}

/// Similarity between two attribute maps, as `Selector.__calculate_dict_diff` computes it.
///
/// `0.5 * ratio(keys1, keys2) + 0.5 * ratio(values1, values2)`, where both tuples are
/// compared *in insertion order* by whole-string equality. Two empty maps score `1.0`,
/// because `difflib`'s ratio of two empty sequences is `1.0`.
///
/// ```
/// use indexmap::IndexMap;
/// use browser_core::fingerprint::dict_diff;
///
/// let empty = IndexMap::new();
/// assert_eq!(dict_diff(&empty, &empty), 1.0);
/// ```
#[must_use]
pub fn dict_diff(d1: &AttributeDict, d2: &AttributeDict) -> f64 {
    let k1: Vec<&str> = d1.keys().map(String::as_str).collect();
    let k2: Vec<&str> = d2.keys().map(String::as_str).collect();
    let v1: Vec<&str> = d1.values().map(String::as_str).collect();
    let v2: Vec<&str> = d2.values().map(String::as_str).collect();

    // Written in Python's exact evaluation order so the floating-point result is
    // bit-identical: `score = r_keys * 0.5` then `score += r_values * 0.5`.
    let mut score = ratio_seq(&k1, &k2) * 0.5;
    score += ratio_seq(&v1, &v2) * 0.5;
    score
}

/// Scores how similar `candidate` is to `original`, as a percentage in `[0.0, 100.0]`.
///
/// A verbatim port of `Selector.__calculate_similarity_score`. Each of the following
/// checks contributes a value in `[0.0, 1.0]` and increments the divisor, so the number
/// of checks varies between 3 and 11 depending on what the *original* fingerprint holds:
///
/// 1. tag equality (always) — `1.0` or `0.0`;
/// 2. text similarity, only if the original has non-empty text;
/// 3. [`dict_diff`] of the attribute maps (always — two attribute-less elements score
///    `1.0` here, "it still counts for something");
/// 4. one similarity check per non-empty [`DISTINGUISHING_ATTRIBUTES`] entry present on
///    the original;
/// 5. path similarity (always);
/// 6. if *both* fingerprints have a parent: parent-name similarity, parent-attribute
///    [`dict_diff`], and parent-text similarity when the original's parent text is
///    non-empty;
/// 7. sibling-list similarity, only if the original has siblings.
///
/// The result is `round((score / checks) * 100, 2)` via
/// [`crate::numeric::round_half_even`], Python's round-half-to-even on the binary double.
/// `f64::round` is half-away-from-zero and *will* flip comparisons against the relocation
/// threshold at exact ties, so it is not used here.
#[must_use]
pub fn similarity_score(original: &ElementFingerprint, candidate: &ElementFingerprint) -> f64 {
    let mut score = 0.0f64;
    let mut checks = 0u32;

    // 1. Tag.
    score += if original.tag == candidate.tag {
        1.0
    } else {
        0.0
    };
    checks += 1;

    // 2. Own text, only when the original has some.
    if let Some(text) = truthy(original.text.as_ref()) {
        score += ratio_str(text, or_empty(candidate.text.as_ref()));
        checks += 1;
    }

    // 3. Attributes as a whole. If neither has attributes it still counts for something.
    score += dict_diff(&original.attributes, &candidate.attributes);
    checks += 1;

    // 4. Separate checks for the attributes that survive structural changes.
    for attrib in DISTINGUISHING_ATTRIBUTES {
        if let Some(value) = truthy(original.attributes.get(attrib)) {
            score += ratio_str(value, or_empty(candidate.attributes.get(attrib)));
            checks += 1;
        }
    }

    // 5. Path from the root.
    score += ratio_seq(&original.path, &candidate.path);
    checks += 1;

    // 6. The parent, but only when both sides have one. (Python deliberately leaves the
    // "original has a parent and the candidate does not" branch unpenalised.)
    if let Some(parent_name) = truthy(original.parent_name.as_ref()) {
        if let Some(candidate_parent) = truthy(candidate.parent_name.as_ref()) {
            score += ratio_str(parent_name, candidate_parent);
            checks += 1;

            let original_parent_attribs = original.parent_attribs.as_ref().unwrap_or(empty_attrs());
            let candidate_parent_attribs =
                candidate.parent_attribs.as_ref().unwrap_or(empty_attrs());
            score += dict_diff(original_parent_attribs, candidate_parent_attribs);
            checks += 1;

            if let Some(parent_text) = truthy(original.parent_text.as_ref()) {
                score += ratio_str(parent_text, or_empty(candidate.parent_text.as_ref()));
                checks += 1;
            }
        }
    }

    // 7. Siblings, only when the original has some.
    match original.siblings.as_deref() {
        Some(siblings) if !siblings.is_empty() => {
            let candidate_siblings: &[String] = candidate.siblings.as_deref().unwrap_or(&[]);
            score += ratio_seq(siblings, candidate_siblings);
            checks += 1;
        }
        _ => {}
    }

    round_half_even(score / f64::from(checks) * 100.0, 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> AttributeDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn sample() -> ElementFingerprint {
        ElementFingerprint {
            tag: "p".into(),
            attributes: attrs(&[("id", "target"), ("class", "main")]),
            text: Some("Hello".into()),
            path: vec!["html".into(), "body".into(), "div".into(), "p".into()],
            parent_name: Some("div".into()),
            parent_attribs: Some(attrs(&[("class", "  wrap  ")])),
            parent_text: Some("prefix".into()),
            siblings: Some(vec!["span".into(), "b".into()]),
            children: Some(vec!["b".into(), "i".into()]),
        }
    }

    #[test]
    fn json_shape_matches_python_orjson_output() {
        let json = serde_json::to_string(&sample()).expect("serialise");
        assert_eq!(
            json,
            r#"{"tag":"p","attributes":{"id":"target","class":"main"},"text":"Hello","path":["html","body","div","p"],"parent_name":"div","parent_attribs":{"class":"  wrap  "},"parent_text":"prefix","siblings":["span","b"],"children":["b","i"]}"#
        );
    }

    #[test]
    fn optional_keys_are_omitted_not_nulled() {
        let fp = ElementFingerprint {
            tag: "div".into(),
            attributes: AttributeDict::new(),
            text: None,
            path: vec!["html".into(), "body".into(), "div".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&fp).expect("serialise");
        assert_eq!(
            json,
            r#"{"tag":"div","attributes":{},"text":null,"path":["html","body","div"]}"#
        );
        assert!(!json.contains("siblings"));
        assert!(!json.contains("parent_name"));
    }

    #[test]
    fn round_trips_through_json() {
        let fp = sample();
        let json = serde_json::to_string(&fp).expect("serialise");
        let back: ElementFingerprint = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(fp, back);
    }

    #[test]
    fn identical_fingerprints_score_one_hundred() {
        assert_eq!(similarity_score(&sample(), &sample()), 100.0);
    }

    #[test]
    fn empty_attribute_maps_score_one() {
        let empty = AttributeDict::new();
        assert_eq!(dict_diff(&empty, &empty), 1.0);
    }

    #[test]
    fn dict_diff_weights_keys_and_values_evenly() {
        // Same keys, completely different values: 0.5 * 1.0 + 0.5 * 0.0.
        let a = attrs(&[("id", "aaaa")]);
        let b = attrs(&[("id", "bbbb")]);
        assert_eq!(dict_diff(&a, &b), 0.5);
    }

    #[test]
    fn dict_diff_respects_insertion_order() {
        let a = attrs(&[("id", "x"), ("class", "y")]);
        let b = attrs(&[("class", "y"), ("id", "x")]);
        // Reordering costs: keys ratio 0.5, values ratio 0.5 -> 0.5.
        assert_eq!(dict_diff(&a, &b), 0.5);
    }

    #[test]
    fn check_count_varies_between_three_and_eleven() {
        // Minimum: tag + attributes + path == 3 checks, all perfect -> 100.0.
        let bare = ElementFingerprint {
            tag: "br".into(),
            path: vec!["html".into(), "br".into()],
            ..Default::default()
        };
        assert_eq!(similarity_score(&bare, &bare), 100.0);

        // A wrong tag with everything else perfect over exactly 3 checks -> 2/3.
        let mut other = bare.clone();
        other.tag = "hr".into();
        other.path = vec!["html".into(), "br".into()];
        assert_eq!(similarity_score(&bare, &other), 66.67);
    }

    #[test]
    fn candidate_without_a_parent_skips_the_parent_checks() {
        let original = sample();
        let mut candidate = sample();
        candidate.parent_name = None;
        candidate.parent_attribs = None;
        candidate.parent_text = None;
        // 8 checks instead of 11, and every remaining check is perfect.
        assert_eq!(similarity_score(&original, &candidate), 100.0);
    }

    #[test]
    fn empty_siblings_vector_is_not_truthy() {
        let mut original = sample();
        original.siblings = Some(Vec::new());
        let mut candidate = sample();
        candidate.siblings = Some(vec!["totally".into(), "different".into()]);
        // The sibling check is skipped entirely, so the score stays perfect.
        assert_eq!(similarity_score(&original, &candidate), 100.0);
    }

    #[test]
    fn scores_use_bankers_rounding_not_f64_round() {
        // score/checks*100 == 66.66666666666666 -> Python round(..., 2) == 66.67.
        let bare = ElementFingerprint {
            tag: "br".into(),
            path: vec!["html".into(), "br".into()],
            ..Default::default()
        };
        let mut other = bare.clone();
        other.tag = "hr".into();
        assert_eq!(similarity_score(&bare, &other), 66.67);
        // An exact tie: 5 of 8 checks -> 62.5, which round-half-to-even keeps at 62.5.
        assert_eq!(round_half_even(62.5, 2), 62.5);
        assert_eq!(round_half_even(0.125, 2), 0.12);
    }
}
