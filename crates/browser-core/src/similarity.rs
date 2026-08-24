//! A faithful port of CPython's [`difflib.SequenceMatcher`] ratio algorithm.
//!
//! the reference implementation's adaptive element relocation scores candidate nodes with
//! `difflib.SequenceMatcher(None, a, b).ratio()`. No crate on crates.io reproduces that
//! number: `strsim` is edit distance and `similar` is Myers/Patience/LCS, and both would
//! silently change every relocation outcome (and the meaning of the `40 %` threshold).
//! So the algorithm is hand-ported here, including the three details that are easy to miss:
//!
//! * **autojunk is on.** When `b.len() >= 200`, any element occurring in more than
//!   `b.len() / 100 + 1` positions of `b` is dropped from the index and becomes
//!   unmatchable by the inner loop (it can still be picked up by the extension phase).
//! * **the tie-break.** [`SequenceMatcher::find_longest_match`] returns the match that
//!   starts earliest in `a`, and among those the one that starts earliest in `b`. That
//!   falls out of scanning `i` and `j` in ascending order and using a strict `>` against
//!   the incumbent best size.
//! * **`ratio("", "") == 1.0`.** Two empty sequences are perfectly similar, which is what
//!   makes two attribute-less elements score `1.0` on the attribute check.
//!
//! Python compares *characters*, not bytes, so [`ratio_str`] collects both sides into
//! `Vec<char>` before matching. [`ratio_seq`] is the element-wise variant used for tag
//! paths, sibling lists and the key/value tuples of [`crate::fingerprint::dict_diff`].
//!
//! the reference implementation never passes an `isjunk` predicate, so this port does not implement one; the
//! `bjunk` set is always empty and the junk-extension loops of the original are therefore
//! dead code and omitted.
//!
//! [`difflib.SequenceMatcher`]: https://docs.python.org/3/library/difflib.html#difflib.SequenceMatcher
//!
//! ```
//! use browser_core::similarity::ratio_str;
//!
//! assert_eq!(ratio_str("", ""), 1.0);
//! assert_eq!(ratio_str("abcd", "bcde"), 0.75);
//! ```

use std::collections::HashMap;
use std::hash::Hash;

/// Length of `b` at which CPython's autojunk heuristic switches on.
const AUTOJUNK_THRESHOLD: usize = 200;

/// A maximal matching block: `a[a .. a + size] == b[b .. b + size]`.
///
/// The field order is also the ordering used when sorting blocks, matching the tuple
/// ordering CPython relies on in `get_matching_blocks`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Match {
    /// Start index of the block in `a`.
    pub a: usize,
    /// Start index of the block in `b`.
    pub b: usize,
    /// Length of the block. Zero only for the terminating sentinel block.
    pub size: usize,
}

/// Compares two sequences the way `difflib.SequenceMatcher(None, a, b)` does.
///
/// Construction indexes `b` and applies the autojunk heuristic; the resulting matcher is
/// immutable and can be queried repeatedly. Unlike the Python original, matching blocks
/// are **not** memoised — [`SequenceMatcher::matching_blocks`] recomputes on every call,
/// which keeps the type `Send + Sync` so `relocate` can score candidates with `rayon`.
///
/// ```
/// use browser_core::similarity::SequenceMatcher;
///
/// let a: Vec<char> = " abcd".chars().collect();
/// let b: Vec<char> = "abcd abcd".chars().collect();
/// let m = SequenceMatcher::new(&a, &b);
/// let longest = m.find_longest_match(0, a.len(), 0, b.len());
/// assert_eq!((longest.a, longest.b, longest.size), (0, 4, 5));
/// ```
#[derive(Clone, Debug)]
pub struct SequenceMatcher<'a, T: Eq + Hash> {
    a: &'a [T],
    b: &'a [T],
    /// For every non-junk element of `b`, the ascending list of positions it occupies.
    b2j: HashMap<&'a T, Vec<usize>>,
}

impl<'a, T: Eq + Hash> SequenceMatcher<'a, T> {
    /// Builds a matcher over `a` and `b` with autojunk enabled, exactly like
    /// `difflib.SequenceMatcher(None, a, b)`.
    #[must_use]
    pub fn new(a: &'a [T], b: &'a [T]) -> Self {
        Self::with_autojunk(a, b, true)
    }

    /// Builds a matcher with the autojunk heuristic explicitly enabled or disabled.
    ///
    /// `autojunk = false` corresponds to `difflib.SequenceMatcher(None, a, b,
    /// autojunk=False)`. the reference implementation always uses the default (`true`); this constructor
    /// exists for tests and for callers that need to diff long sequences verbatim.
    #[must_use]
    pub fn with_autojunk(a: &'a [T], b: &'a [T], autojunk: bool) -> Self {
        let mut b2j: HashMap<&'a T, Vec<usize>> = HashMap::new();
        for (i, elt) in b.iter().enumerate() {
            b2j.entry(elt).or_default().push(i);
        }

        // Purge popular elements. CPython: `if self.autojunk and n >= 200`, with
        // `ntest = n // 100 + 1` and the strict `len(idxs) > ntest` test.
        let n = b.len();
        if autojunk && n >= AUTOJUNK_THRESHOLD {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }

        Self { a, b, b2j }
    }

    /// The left-hand sequence.
    #[must_use]
    pub fn a(&self) -> &'a [T] {
        self.a
    }

    /// The right-hand sequence.
    #[must_use]
    pub fn b(&self) -> &'a [T] {
        self.b
    }

    /// Finds the longest matching block in `a[alo..ahi]` and `b[blo..bhi]`.
    ///
    /// Of all maximal matching blocks this returns the one starting earliest in `a`, and
    /// among those the one starting earliest in `b`. Returns a zero-sized match anchored
    /// at `(alo, blo)` when nothing matches.
    ///
    /// # Panics
    ///
    /// Panics if `ahi > a.len()` or `bhi > b.len()`, as slice indexing would.
    #[must_use]
    pub fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);

        // `j2len[j]` is the length of the longest match ending with `a[i - 1]` and `b[j]`.
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            // `b2j` has no junk keys, so the loop is skipped entirely for junk elements.
            if let Some(js) = self.b2j.get(&self.a[i]) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    // Python's `j2len.get(j - 1, 0)`; at `j == 0` the key `-1` is absent.
                    let prev = match j.checked_sub(1) {
                        Some(jm1) => j2len.get(&jm1).copied().unwrap_or(0),
                        None => 0,
                    };
                    let k = prev + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }

        // Extend the best block by equal elements on each end. "Popular" elements were
        // purged from `b2j` and so never seed a match, but they can still extend one.
        while besti > alo && bestj > blo && self.a[besti - 1] == self.b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }

        Match {
            a: besti,
            b: bestj,
            size: bestsize,
        }
    }

    /// Returns the list of matching blocks, ascending in both `a` and `b`.
    ///
    /// Adjacent equal blocks are collapsed and the list always ends with the sentinel
    /// `Match { a: a.len(), b: b.len(), size: 0 }` — the only zero-sized entry. This
    /// mirrors `SequenceMatcher.get_matching_blocks()`, including its iterative
    /// (stack-based) rewrite of the recursion.
    #[must_use]
    pub fn matching_blocks(&self) -> Vec<Match> {
        let (la, lb) = (self.a.len(), self.b.len());

        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        let mut blocks: Vec<Match> = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(alo, ahi, blo, bhi);
            let (i, j, k) = (m.a, m.b, m.size);
            if k > 0 {
                blocks.push(m);
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort_unstable();

        let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
        let mut non_adjacent: Vec<Match> = Vec::new();
        for m in blocks {
            if i1 + k1 == m.a && j1 + k1 == m.b {
                k1 += m.size;
            } else {
                if k1 > 0 {
                    non_adjacent.push(Match {
                        a: i1,
                        b: j1,
                        size: k1,
                    });
                }
                i1 = m.a;
                j1 = m.b;
                k1 = m.size;
            }
        }
        if k1 > 0 {
            non_adjacent.push(Match {
                a: i1,
                b: j1,
                size: k1,
            });
        }
        non_adjacent.push(Match {
            a: la,
            b: lb,
            size: 0,
        });
        non_adjacent
    }

    /// Similarity in `[0.0, 1.0]`: `2 * M / T`, where `M` is the total size of all
    /// matching blocks and `T` is `a.len() + b.len()`.
    ///
    /// Two empty sequences yield `1.0`, matching `difflib._calculate_ratio`.
    #[must_use]
    pub fn ratio(&self) -> f64 {
        let matches: usize = self.matching_blocks().iter().map(|m| m.size).sum();
        calculate_ratio(matches, self.a.len() + self.b.len())
    }
}

/// `difflib._calculate_ratio`: `2.0 * matches / length`, or `1.0` when `length == 0`.
fn calculate_ratio(matches: usize, length: usize) -> f64 {
    if length > 0 {
        2.0 * matches as f64 / length as f64
    } else {
        1.0
    }
}

/// `SequenceMatcher(None, a, b).ratio()` over the **characters** of two strings.
///
/// Python strings are sequences of code points, so both sides are collected into
/// `Vec<char>`; comparing UTF-8 bytes would give different numbers for non-ASCII input.
///
/// ```
/// use browser_core::similarity::ratio_str;
///
/// assert_eq!(ratio_str("hello", "hello"), 1.0);
/// assert_eq!(ratio_str("", "abc"), 0.0);
/// ```
#[must_use]
pub fn ratio_str(a: &str, b: &str) -> f64 {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    SequenceMatcher::new(&av, &bv).ratio()
}

/// `SequenceMatcher(None, a, b).ratio()` over two sequences of strings.
///
/// Used for tag paths, sibling tag lists and the key/value tuples compared by
/// [`dict_diff`](crate::fingerprint::dict_diff). Elements are compared for equality as
/// whole strings, never character by character.
///
/// ```
/// use browser_core::similarity::ratio_seq;
///
/// let a = ["html", "body", "div", "p"];
/// let b = ["html", "body", "section", "p"];
/// assert_eq!(ratio_seq(&a, &b), 0.75);
/// ```
#[must_use]
pub fn ratio_seq<A: AsRef<str>, B: AsRef<str>>(a: &[A], b: &[B]) -> f64 {
    let av: Vec<&str> = a.iter().map(AsRef::as_ref).collect();
    let bv: Vec<&str> = b.iter().map(AsRef::as_ref).collect();
    SequenceMatcher::new(&av, &bv).ratio()
}

/// `SequenceMatcher(None, a, b).ratio()` over two slices of any hashable, comparable type.
///
/// The generic escape hatch behind [`ratio_str`] and [`ratio_seq`].
#[must_use]
pub fn ratio_slice<T: Eq + Hash>(a: &[T], b: &[T]) -> f64 {
    SequenceMatcher::new(a, b).ratio()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand so the CPython doctest expectations stay readable.
    fn m(a: usize, b: usize, size: usize) -> Match {
        Match { a, b, size }
    }

    #[test]
    fn empty_pair_is_perfectly_similar() {
        assert_eq!(ratio_str("", ""), 1.0);
        let empty: [&str; 0] = [];
        assert_eq!(ratio_seq(&empty, &empty), 1.0);
    }

    #[test]
    fn one_sided_empty_is_zero() {
        assert_eq!(ratio_str("abc", ""), 0.0);
        assert_eq!(ratio_str("", "abc"), 0.0);
    }

    #[test]
    fn identical_is_one() {
        assert_eq!(ratio_str("the quick brown fox", "the quick brown fox"), 1.0);
        assert_eq!(ratio_str("héllo wörld ünïcode", "héllo wörld ünïcode"), 1.0);
    }

    #[test]
    fn docstring_examples_from_cpython() {
        // >>> SequenceMatcher(None, "abcd", "bcde").ratio() == 0.75
        assert_eq!(ratio_str("abcd", "bcde"), 0.75);
        // >>> SequenceMatcher(None, " abcd", "abcd abcd").find_longest_match(0, 5, 0, 9)
        // Match(a=0, b=4, size=5)
        let a: Vec<char> = " abcd".chars().collect();
        let b: Vec<char> = "abcd abcd".chars().collect();
        let matcher = SequenceMatcher::new(&a, &b);
        assert_eq!(
            matcher.find_longest_match(0, a.len(), 0, b.len()),
            m(0, 4, 5)
        );
        // >>> SequenceMatcher(None, "ab", "c").find_longest_match(0, 2, 0, 1)
        // Match(a=0, b=0, size=0)
        let a: Vec<char> = "ab".chars().collect();
        let b: Vec<char> = "c".chars().collect();
        assert_eq!(
            SequenceMatcher::new(&a, &b).find_longest_match(0, 2, 0, 1),
            m(0, 0, 0)
        );
    }

    #[test]
    fn matching_blocks_example_from_cpython() {
        let a: Vec<char> = "abxcd".chars().collect();
        let b: Vec<char> = "abcd".chars().collect();
        let blocks = SequenceMatcher::new(&a, &b).matching_blocks();
        assert_eq!(blocks, vec![m(0, 0, 2), m(3, 2, 2), m(5, 4, 0),]);
    }

    #[test]
    fn tie_break_prefers_earliest_in_a_then_b() {
        // "ab" occurs twice in b; the earliest occurrence must win.
        let a: Vec<char> = "ab".chars().collect();
        let b: Vec<char> = "abab".chars().collect();
        let longest = SequenceMatcher::new(&a, &b).find_longest_match(0, 2, 0, 4);
        assert_eq!(longest, m(0, 0, 2));
    }

    #[test]
    fn autojunk_changes_the_answer_for_long_sequences() {
        // len(b) == 253 >= 200, so ntest == 3 and 'x' (250 occurrences) is purged. The
        // leading "abc" stops the extension phase from recovering the match, so the
        // ratio collapses to 0. Python agrees: 0.0 vs 0.07604562737642585.
        let a: Vec<char> = "x".repeat(10).chars().collect();
        let b: Vec<char> = format!("abc{}", "x".repeat(250)).chars().collect();
        assert_eq!(b.len(), 253);
        assert_eq!(SequenceMatcher::with_autojunk(&a, &b, true).ratio(), 0.0);
        assert_eq!(
            SequenceMatcher::with_autojunk(&a, &b, false).ratio(),
            0.076_045_627_376_425_85
        );
    }

    #[test]
    fn autojunk_is_off_below_two_hundred() {
        // Same shape, but len(b) == 193 < 200, so nothing is purged.
        let a: Vec<char> = "x".repeat(10).chars().collect();
        let b: Vec<char> = format!("abc{}", "x".repeat(190)).chars().collect();
        assert_eq!(b.len(), 193);
        let with = SequenceMatcher::with_autojunk(&a, &b, true).ratio();
        let without = SequenceMatcher::with_autojunk(&a, &b, false).ratio();
        assert_eq!(with, without);
        assert_eq!(with, 0.098_522_167_487_684_73);
    }

    #[test]
    fn purged_elements_can_still_extend_a_match() {
        // 'x' is purged (200 > 3) yet the ratio is unchanged, because the extension
        // phase walks over popular elements. Python: 0.04830917874396135 for both.
        let a: Vec<char> = "xxxxx".chars().collect();
        let b: Vec<char> = format!("{}yz", "x".repeat(200)).chars().collect();
        assert_eq!(b.len(), 202);
        let with = SequenceMatcher::with_autojunk(&a, &b, true).ratio();
        let without = SequenceMatcher::with_autojunk(&a, &b, false).ratio();
        assert_eq!(with, without);
        assert_eq!(with, 0.048_309_178_743_961_35);
    }

    #[test]
    fn characters_not_bytes() {
        // "é" is two UTF-8 bytes but one character; a byte-wise matcher would report a
        // different ratio here.
        assert_eq!(ratio_str("é", "e"), 0.0);
        assert_eq!(ratio_str("café", "cafe"), 0.75);
    }
}
