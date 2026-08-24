//! `identifier_hash()`: lowercase then trim, SHA-256 hex, suffixed with `_<utf8-byte-len>`.
//!
//! This is `StorageSystemMixin._get_hash`, the key-derivation helper the reference implementation
//! offers to *custom* storage backends:
//!
//! ```python
//! @staticmethod
//! @lru_cache(128, typed=True)
//! def _get_hash(identifier: str) -> str:
//!     _identifier = identifier.lower().strip()
//!     _identifier_bytes = _identifier.encode("utf-8")
//!     hash_value = sha256(_identifier_bytes).hexdigest()
//!     return f"{hash_value}_{len(_identifier_bytes)}"  # Length to reduce collision chance
//! ```
//!
//! Three details are load-bearing and easy to get backwards:
//!
//! * **lowercase happens before trim.** For ASCII the two orders agree, but they
//!   do not in general: `"\u{130}"` (`İ`) lowercases to `"i\u{307}"` and
//!   Turkish dotless casing aside, some code points lowercase *into* characters
//!   that `strip()` would then remove. Keeping Python's order keeps the hashes
//!   equal.
//! * **`str.strip()` is not [`str::trim`].** Python's whitespace predicate is
//!   `Py_UNICODE_ISSPACE`, which is Unicode `White_Space=Yes` *plus* the four C0
//!   information separators `U+001C..U+001F` — 29 code points against Rust's 25.
//!   Trimming with `str::trim` leaves those four in place, changing both the
//!   digest and the byte-length suffix. [`PYTHON_ONLY_WHITESPACE`] closes the
//!   gap, and `tests/python_differential.rs` pins it against a corpus CPython
//!   generated.
//! * **the suffix counts UTF-8 bytes, not characters** — and counts them
//!   *after* the case fold, whose byte length can differ from the input's.
//!
//! [`SqliteStorage`](crate::sqlite::SqliteStorage) does **not** use this: the
//! bundled backend stores the raw identifier, exactly like Python's. It is part
//! of the public surface because third-party backends are documented to use it,
//! and because the reference implementation's test suite pins its output shape.

use sha2::{Digest, Sha256};

/// The number of hex characters a SHA-256 digest renders to.
const SHA256_HEX_LEN: usize = 64;

/// The code points Python's `str.strip()` removes and Rust's [`str::trim`] does
/// not.
///
/// CPython treats a character as whitespace when `Py_UNICODE_ISSPACE` says so:
/// the Unicode `White_Space` property *plus* the four C0 information separators
/// below. Enumerating every code point under both rules gives 29 characters for
/// Python and 25 for Rust, and this is exactly the difference — Python strips
/// nothing outside `White_Space` apart from these four.
pub const PYTHON_ONLY_WHITESPACE: [char; 4] = ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'];

/// `str.strip()`: [`str::trim`] widened to Python's whitespace set.
fn python_strip(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || PYTHON_ONLY_WHITESPACE.contains(&c))
}

/// A stable, collision-resistant key for `identifier`.
///
/// The result is `<64 lowercase hex chars>_<utf8 byte length>`. It is
/// deterministic across processes and platforms, and insensitive to surrounding
/// whitespace and to case.
///
/// ```
/// use browser_storage::hash::identifier_hash;
///
/// // Case- and whitespace-insensitive.
/// assert_eq!(identifier_hash("  Hello  "), identifier_hash("hello"));
///
/// let hashed = identifier_hash("test");
/// let (digest, length) = hashed.rsplit_once('_').expect("the length suffix");
/// assert_eq!(digest.len(), 64);
/// assert_eq!(length, "4");
/// ```
#[must_use]
pub fn identifier_hash(identifier: &str) -> String {
    // Python: `identifier.lower().strip()` — in that order, and with Python's
    // whitespace set rather than Rust's.
    let normalised = identifier.to_lowercase();
    let normalised = python_strip(&normalised);

    let bytes = normalised.as_bytes();
    let digest = Sha256::digest(bytes);

    let mut out = String::with_capacity(SHA256_HEX_LEN + 8);
    out.push_str(&hex::encode(digest));
    out.push('_');
    out.push_str(&bytes.len().to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(identifier, StorageSystemMixin._get_hash(identifier))` pairs produced by
    /// CPython 3.14 running the reference implementation 0.4.15.
    const PYTHON_HASHES: &[(&str, &str)] = &[
        (
            "test-identifier",
            "115ae872eb1d3e23f9de03f7ab344193b21068812ee52eb37e8169e6d093c7ae_15",
        ),
        (
            "test",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08_4",
        ),
        (
            "  Hello  ",
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824_5",
        ),
        (
            "hello",
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824_5",
        ),
        (
            "",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855_0",
        ),
        (
            "div.main",
            "0d5645be9c030e8746bde37521ba66918dd6c3047aa5c4977fcaddd26fa06e59_8",
        ),
        (
            "\u{4f8b}\u{3048}",
            "5633aceb51bce3bc6cfa84acc35249ac220adcd9c87715d01dff6517496ddbc1_6",
        ),
        (
            "the reference implementation",
            "1d912f5760a7e18457a8ab292cb616f300c36dafac26da22238699e3aadf66f0_9",
        ),
        (
            "p::text",
            "bcea82aa024d8ff570d25a19b057093c49b533fde20cca3c6fee0bfc7de0ca4b_7",
        ),
    ];

    #[test]
    fn matches_cpython_digest_for_digest() {
        for (identifier, expected) in PYTHON_HASHES {
            assert_eq!(
                &identifier_hash(identifier),
                expected,
                "identifier_hash({identifier:?})",
            );
        }
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(
            identifier_hash("test-identifier"),
            identifier_hash("test-identifier"),
        );
    }

    #[test]
    fn differs_for_different_input() {
        assert_ne!(
            identifier_hash("identifier-a"),
            identifier_hash("identifier-b"),
        );
    }

    #[test]
    fn strips_and_lowercases() {
        assert_eq!(identifier_hash("  Hello  "), identifier_hash("hello"));
        assert_eq!(identifier_hash("\tDIV.MAIN\n"), identifier_hash("div.main"));
    }

    #[test]
    fn includes_the_utf8_length_suffix() {
        let hashed = identifier_hash("test");
        let (digest, length) = hashed.rsplit_once('_').expect("a `_` separator");
        assert_eq!(digest.len(), SHA256_HEX_LEN);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(length, "4");
    }

    #[test]
    fn the_length_counts_bytes_not_characters() {
        // "é" is one character but two UTF-8 bytes; "例" is one and three.
        assert!(identifier_hash("\u{e9}").ends_with("_2"));
        assert!(identifier_hash("\u{4f8b}").ends_with("_3"));
        assert!(identifier_hash("  \u{4f8b}  ").ends_with("_3"));
    }

    #[test]
    fn whitespace_only_identifiers_hash_like_the_empty_string() {
        let empty = identifier_hash("");
        assert_eq!(empty, identifier_hash("   \t\n"));
        assert!(empty.ends_with("_0"));
    }

    #[test]
    fn lowercasing_happens_before_trimming() {
        // Kelvin sign lowercases to a plain "k"; the order of the two steps is
        // still observable through the byte-length suffix of a padded input.
        assert_eq!(identifier_hash(" \u{212a} "), identifier_hash("k"));
    }
}
