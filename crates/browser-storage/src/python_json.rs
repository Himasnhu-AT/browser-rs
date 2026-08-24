//! Encodes an [`ElementFingerprint`] the way `orjson.dumps(element_to_dict(el))` does.
//!
//! # Why this exists
//!
//! [`ElementFingerprint`]'s own `Serialize` marks all five optional fields
//! `skip_serializing_if = "Option::is_none"`, which is *not* Python's rule.
//! `_StorageTools.element_to_dict` writes the parent keys as a block:
//!
//! ```python
//! if parent is not None:
//!     result.update({
//!         "parent_name":    parent.tag,
//!         "parent_attribs": dict(parent.attrib),          # never None, often {}
//!         "parent_text":    parent.text.strip() if parent.text else None,
//!     })                                                  # <- an explicit null
//! ```
//!
//! So a parent with no leading text yields `"parent_text": null` — the key is
//! *present*. Field-level skipping drops it, and that is not a cosmetic
//! difference: the reference implementation's relocation scorer subscripts those two keys directly
//! rather than going through `dict.get`,
//!
//! ```python
//! if original.get("parent_name"):
//!     ...
//!     score += self.__calculate_dict_diff(original["parent_attribs"], ...)   # KeyError
//!     if original["parent_text"]:                                            # KeyError
//! ```
//!
//! so a row written with the key omitted makes
//! `Selector.__calculate_similarity_score` raise `KeyError('parent_text')` the
//! first time the Python side tries to relocate that element. The row survives
//! `SQLiteStorageSystem.retrieve()` — it is `orjson.loads`, which does not care
//! — and only blows up later, in the adaptive path the storage exists to serve.
//!
//! [`to_string`] therefore encodes the map itself, in Python's key order, with
//! Python's presence rules:
//!
//! | key | written when |
//! |---|---|
//! | `tag`, `attributes`, `text`, `path` | always (`text` may be `null`) |
//! | `parent_name`, `parent_attribs`, `parent_text` | as a block, iff the element had a parent; `parent_attribs` falls back to `{}` and `parent_text` to `null` |
//! | `siblings`, `children` | only when non-empty — `element_to_dict` guards both with `if siblings:` / `if children:` |
//!
//! Deserialisation needs no equivalent: `Option` + `default` already accepts a
//! missing key and an explicit `null` alike, which is what lets this crate read
//! rows written by both implementations.

use browser_core::fingerprint::AttributeDict;
use browser_core::ElementFingerprint;
use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;

/// An empty attribute map, for the `parent_attribs` fallback.
fn empty_attributes() -> &'static AttributeDict {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<AttributeDict> = OnceLock::new();
    EMPTY.get_or_init(AttributeDict::new)
}

/// A borrowed [`ElementFingerprint`] that serialises with Python's key rules.
///
/// Public so that a caller writing to some other backend can reuse the encoding
/// — the compatibility rule belongs to the *format*, not to SQLite.
#[derive(Debug, Clone, Copy)]
pub struct PythonShaped<'a>(pub &'a ElementFingerprint);

impl Serialize for PythonShaped<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fingerprint = self.0;

        // `element_to_dict` records a parent iff `element.getparent()` is not
        // None, and writes all three keys in that case. `parent_name` is the
        // one that is unconditionally a string there, so it is the marker; the
        // other two are checked as well so a hand-built fingerprint that sets
        // only `parent_text` still round-trips.
        let has_parent = fingerprint.parent_name.is_some()
            || fingerprint.parent_attribs.is_some()
            || fingerprint.parent_text.is_some();

        // Python omits an *empty* sibling/child list rather than writing `[]`.
        let siblings = fingerprint
            .siblings
            .as_ref()
            .filter(|list| !list.is_empty());
        let children = fingerprint
            .children
            .as_ref()
            .filter(|list| !list.is_empty());

        let len = 4
            + usize::from(has_parent) * 3
            + usize::from(siblings.is_some())
            + usize::from(children.is_some());
        let mut map = serializer.serialize_map(Some(len))?;

        map.serialize_entry("tag", &fingerprint.tag)?;
        map.serialize_entry("attributes", &fingerprint.attributes)?;
        map.serialize_entry("text", &fingerprint.text)?;
        map.serialize_entry("path", &fingerprint.path)?;

        if has_parent {
            map.serialize_entry("parent_name", &fingerprint.parent_name)?;
            // `dict(parent.attrib)` is never None on the Python side; a `None`
            // here would make `__calculate_dict_diff(None, ...)` raise.
            map.serialize_entry(
                "parent_attribs",
                fingerprint
                    .parent_attribs
                    .as_ref()
                    .unwrap_or_else(|| empty_attributes()),
            )?;
            // May legitimately be `null`.
            map.serialize_entry("parent_text", &fingerprint.parent_text)?;
        }

        if let Some(siblings) = siblings {
            map.serialize_entry("siblings", siblings)?;
        }
        if let Some(children) = children {
            map.serialize_entry("children", children)?;
        }

        map.end()
    }
}

/// Encodes `fingerprint` exactly as `orjson.dumps(element_to_dict(element))`
/// would — no spaces, keys in Python's insertion order, absent-vs-null handled
/// as Python handles it.
///
/// # Errors
///
/// [`serde_json::Error`] only if the fingerprint cannot be represented, which
/// for this all-`String` shape means never in practice.
///
/// ```
/// use browser_core::ElementFingerprint;
/// use browser_storage::python_json::to_string;
///
/// let fingerprint = ElementFingerprint {
///     tag: "p".into(),
///     path: vec!["html".into(), "p".into()],
///     // A parent that has no leading text of its own.
///     parent_name: Some("div".into()),
///     ..Default::default()
/// };
///
/// // `parent_attribs` and `parent_text` are present, not skipped.
/// assert_eq!(
///     to_string(&fingerprint)?,
///     r#"{"tag":"p","attributes":{},"text":null,"path":["html","p"],"#.to_owned()
///         + r#""parent_name":"div","parent_attribs":{},"parent_text":null}"#,
/// );
/// # Ok::<(), serde_json::Error>(())
/// ```
pub fn to_string(fingerprint: &ElementFingerprint) -> Result<String, serde_json::Error> {
    serde_json::to_string(&PythonShaped(fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> AttributeDict {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_parentless_element_writes_no_parent_keys() {
        let fingerprint = ElementFingerprint {
            tag: "html".to_owned(),
            path: vec!["html".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            to_string(&fingerprint).expect("encode"),
            r#"{"tag":"html","attributes":{},"text":null,"path":["html"]}"#,
        );
    }

    #[test]
    fn a_parent_without_text_still_writes_parent_text_as_null() {
        let fingerprint = ElementFingerprint {
            tag: "p".to_owned(),
            path: vec!["html".to_owned(), "p".to_owned()],
            parent_name: Some("div".to_owned()),
            parent_attribs: Some(AttributeDict::new()),
            parent_text: None,
            ..Default::default()
        };
        let json = to_string(&fingerprint).expect("encode");
        assert!(json.contains(r#""parent_text":null"#), "{json}");
        assert!(json.contains(r#""parent_attribs":{}"#), "{json}");
    }

    #[test]
    fn a_missing_parent_attribs_becomes_an_empty_map_not_null() {
        // `__calculate_dict_diff(None, {})` would raise on the Python side.
        let fingerprint = ElementFingerprint {
            tag: "p".to_owned(),
            path: vec!["p".to_owned()],
            parent_name: Some("div".to_owned()),
            parent_attribs: None,
            ..Default::default()
        };
        let json = to_string(&fingerprint).expect("encode");
        assert!(json.contains(r#""parent_attribs":{}"#), "{json}");
    }

    #[test]
    fn empty_sibling_and_child_lists_are_omitted() {
        let fingerprint = ElementFingerprint {
            tag: "p".to_owned(),
            path: vec!["p".to_owned()],
            siblings: Some(Vec::new()),
            children: Some(Vec::new()),
            ..Default::default()
        };
        let json = to_string(&fingerprint).expect("encode");
        assert!(!json.contains("siblings"), "{json}");
        assert!(!json.contains("children"), "{json}");
    }

    #[test]
    fn non_empty_sibling_and_child_lists_are_written_last() {
        let fingerprint = ElementFingerprint {
            tag: "span".to_owned(),
            path: vec!["span".to_owned()],
            parent_name: Some("div".to_owned()),
            parent_attribs: Some(AttributeDict::new()),
            parent_text: None,
            siblings: Some(vec!["p".to_owned()]),
            children: Some(vec!["b".to_owned(), "i".to_owned()]),
            ..Default::default()
        };
        assert_eq!(
            to_string(&fingerprint).expect("encode"),
            r#"{"tag":"span","attributes":{},"text":null,"path":["span"],"parent_name":"div","parent_attribs":{},"parent_text":null,"siblings":["p"],"children":["b","i"]}"#,
        );
    }

    #[test]
    fn attribute_insertion_order_is_preserved() {
        let fingerprint = ElementFingerprint {
            tag: "p".to_owned(),
            attributes: attrs(&[("z", "1"), ("a", "2"), ("m", "3")]),
            path: vec!["p".to_owned()],
            ..Default::default()
        };
        let json = to_string(&fingerprint).expect("encode");
        assert!(
            json.contains(r#""attributes":{"z":"1","a":"2","m":"3"}"#),
            "{json}"
        );
    }

    #[test]
    fn the_encoding_round_trips_through_the_derived_deserialiser() {
        let fingerprint = ElementFingerprint {
            tag: "p".to_owned(),
            attributes: attrs(&[("id", "x")]),
            text: Some("hi".to_owned()),
            path: vec!["html".to_owned(), "p".to_owned()],
            parent_name: Some("div".to_owned()),
            parent_attribs: Some(attrs(&[("class", " wrap ")])),
            parent_text: None,
            siblings: Some(vec!["b".to_owned()]),
            children: None,
        };
        let json = to_string(&fingerprint).expect("encode");
        let back: ElementFingerprint = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, fingerprint);
    }
}
