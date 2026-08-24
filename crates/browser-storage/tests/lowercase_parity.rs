//! `str.lower()` vs [`char::to_lowercase`], over all 1 112 064 scalar values.
//!
//! Both of this crate's key-derivation paths begin with a case fold —
//! `identifier_hash` (`identifier.lower().strip()`) and `normalise_url`
//! (`url.lower()`) — so a single code point folded differently changes a
//! storage key, and the key is what makes a row findable. The hand-written
//! corpora cover the interesting *scripts*; this covers the whole space.
//!
//! `testdata/python_lowercase_map.jsonl` is CPython 3.14's complete mapping,
//! produced by `tools/gen_lowercase_map.py`. Its first line is a header
//! recording the Unicode version CPython was built against.
//!
//! # The known divergence: 28 code points
//!
//! CPython 3.14 ships **UCD 16.0.0**. Rust 1.97's `char::to_lowercase` tables
//! are newer, and 28 code points gained a lowercase mapping in between:
//!
//! * `U+A7CE`, `U+A7D2`, `U+A7D4` — Latin Extended-D additions;
//! * `U+16EA0..=U+16EBA` — the uppercase half of Medefaidrin's extended range.
//!
//! Every one is *unassigned* in Unicode 16, so Python leaves it alone while
//! Rust folds it. There is no third option short of vendoring a UCD-16 case
//! table into this crate; an identifier or URL would have to contain a
//! code point that did not exist when CPython 3.14 was built for the difference
//! to be observable, and the divergence is listed in the unit's
//! `remaining_gaps` rather than papered over.
//!
//! The test below asserts the divergence is *exactly* those 28 — an upgrade to
//! either toolchain that widens it fails here rather than in a user's database.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// The code points Rust folds and CPython 3.14 (UCD 16.0.0) does not.
///
/// Unassigned in Unicode 16, assigned with a lowercase mapping later.
const UNASSIGNED_IN_UCD_16: [u32; 28] = [
    0xA7CE, 0xA7D2, 0xA7D4, 0x16EA0, 0x16EA1, 0x16EA2, 0x16EA3, 0x16EA4, 0x16EA5, 0x16EA6, 0x16EA7,
    0x16EA8, 0x16EA9, 0x16EAA, 0x16EAB, 0x16EAC, 0x16EAD, 0x16EAE, 0x16EAF, 0x16EB0, 0x16EB1,
    0x16EB2, 0x16EB3, 0x16EB4, 0x16EB5, 0x16EB6, 0x16EB7, 0x16EB8,
];

/// `(header, code point -> folded code points)`.
fn python_lowercase_map() -> (serde_json::Value, HashMap<u32, Vec<u32>>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("python_lowercase_map.jsonl");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header: serde_json::Value =
        serde_json::from_str(lines.next().expect("a header line")).expect("header JSON");

    let map = lines
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).expect("row JSON");
            let cp = u32::try_from(row["cp"].as_u64().expect("cp")).expect("cp fits");
            let lower = row["lower"]
                .as_array()
                .expect("lower")
                .iter()
                .map(|v| u32::try_from(v.as_u64().expect("scalar")).expect("fits"))
                .collect::<Vec<u32>>();
            (cp, lower)
        })
        .collect::<HashMap<_, _>>();
    (header, map)
}

#[test]
fn to_lowercase_matches_str_lower_everywhere_but_the_28_known_code_points() {
    let (header, python) = python_lowercase_map();
    assert_eq!(
        header["count"].as_u64().expect("count") as usize,
        python.len(),
        "the golden's header disagrees with its own row count",
    );
    assert_eq!(
        header["unidata_version"], "16.0.0",
        "the reference interpreter's Unicode version changed; \
         re-derive UNASSIGNED_IN_UCD_16 before trusting this test",
    );

    let allowed: std::collections::HashSet<u32> = UNASSIGNED_IN_UCD_16.into_iter().collect();
    let mut unexpected = Vec::new();
    let mut healed = Vec::new();

    for cp in 0..0x11_0000u32 {
        let Some(ch) = char::from_u32(cp) else {
            continue; // surrogate
        };
        let rust: Vec<u32> = ch.to_lowercase().map(u32::from).collect();
        // Python's map only records the code points that actually change.
        let expected: Vec<u32> = python.get(&cp).cloned().unwrap_or_else(|| vec![cp]);

        match (rust == expected, allowed.contains(&cp)) {
            (true, false) => {}
            (true, true) => healed.push(cp),
            (false, true) => {}
            (false, false) => {
                if unexpected.len() < 40 {
                    unexpected.push(format!(
                        "  U+{cp:04X}: rust={rust:04X?} python={expected:04X?}"
                    ));
                } else if unexpected.len() == 40 {
                    unexpected.push("  …".to_owned());
                }
            }
        }
    }

    assert!(
        unexpected.is_empty(),
        "case folding diverges from CPython outside the recorded 28 code points:\n{}",
        unexpected.join("\n"),
    );
    assert!(
        healed.is_empty(),
        "these code points no longer diverge — shrink UNASSIGNED_IN_UCD_16: {:04X?}",
        healed,
    );
}

/// The recorded divergences all have the same cause, and it is the harmless one:
/// Python leaves the character alone because it does not know it yet.
#[test]
fn every_recorded_divergence_is_an_unassigned_code_point_in_python() {
    let (_, python) = python_lowercase_map();
    for cp in UNASSIGNED_IN_UCD_16 {
        assert!(
            !python.contains_key(&cp),
            "U+{cp:04X} is in CPython's folding map after all — it is not \
             an unassigned-in-UCD-16 case",
        );
        let ch = char::from_u32(cp).expect("scalar value");
        assert_ne!(
            ch.to_lowercase().collect::<String>(),
            ch.to_string(),
            "U+{cp:04X} does not fold in Rust either, so it is not a divergence",
        );
    }
}

/// The divergence cannot reach an ASCII identifier or an ASCII URL, which is
/// what every realistic key is.
#[test]
fn no_ascii_or_latin1_code_point_diverges() {
    for cp in UNASSIGNED_IN_UCD_16 {
        assert!(cp > 0xFF, "U+{cp:04X} is in Latin-1 — that would matter");
    }
}

// --------------------------------------------------------------------------- //
// `str.strip()`'s character set
// --------------------------------------------------------------------------- //

/// `identifier_hash` trims exactly the 29 code points `str.strip()` removes.
///
/// Python's predicate is `Py_UNICODE_ISSPACE`: Unicode `White_Space` **plus**
/// the four C0 information separators `U+001C..U+001F`. Rust's
/// [`char::is_whitespace`] is `White_Space` alone, 25 code points, so a plain
/// [`str::trim`] leaves the separators in — changing both the digest and the
/// `_<byte length>` suffix. `testdata/python_whitespace.json` is the set CPython
/// itself reports.
#[test]
fn identifier_hash_trims_exactly_pythons_whitespace_set() {
    use browser_storage::hash::identifier_hash;

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("python_whitespace.json");
    let golden: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())),
    )
    .expect("golden JSON");

    let expected: std::collections::HashSet<u32> = golden["code_points"]
        .as_array()
        .expect("code_points")
        .iter()
        .map(|v| u32::try_from(v.as_u64().expect("scalar")).expect("fits"))
        .collect();
    assert_eq!(expected.len(), 29, "the golden changed shape");

    // The value every whitespace-padded identifier must collapse to.
    let bare = identifier_hash("x");

    let mut wrongly_kept = Vec::new();
    let mut wrongly_trimmed = Vec::new();
    for cp in 0..0x11_0000u32 {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        let padded = format!("{ch}x{ch}");
        let trims = identifier_hash(&padded) == bare;
        match (trims, expected.contains(&cp)) {
            (true, true) | (false, false) => {}
            (false, true) => wrongly_kept.push(cp),
            (true, false) => wrongly_trimmed.push(cp),
        }
    }

    assert!(
        wrongly_kept.is_empty(),
        "Python strips these and identifier_hash does not: {:04X?}",
        wrongly_kept,
    );
    assert!(
        wrongly_trimmed.is_empty(),
        "identifier_hash strips these and Python does not: {:04X?}",
        wrongly_trimmed,
    );
}

/// Characters that *look* like whitespace but are not, in either implementation.
///
/// Zero-width space and word joiner are the ones that would quietly merge two
/// distinct identifiers if someone widened the trim predicate to "invisible".
#[test]
fn zero_width_characters_are_not_whitespace() {
    use browser_storage::hash::identifier_hash;

    let bare = identifier_hash("x");
    for ch in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'] {
        assert_ne!(
            identifier_hash(&format!("{ch}x{ch}")),
            bare,
            "U+{:04X} must not be trimmed",
            ch as u32,
        );
    }
}
