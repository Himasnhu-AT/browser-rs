//! [`round_half_even`]: `round(x, n)` with Python's exact semantics.
//!
//! The reference implementation rounds stats (`elapsed_seconds`, `requests_per_second`, AutoThrottle
//! delays) and the adaptive similarity score with the built-in `round()`, whose
//! result is compared against thresholds. `ARCHITECTURE.md` §4 calls this out: the
//! naive `(x * 10f64.powi(n)).round() / 10f64.powi(n)` is *not* the same function,
//! and the difference shows up exactly at ties — which is where a threshold
//! comparison flips.
//!
//! Python's `float.__round__` is `_Py_double_round`: format the double to `n`
//! fractional digits with correctly-rounded, ties-to-even decimal conversion
//! (`_Py_dg_dtoa`), then read the decimal back (`_Py_dg_strtod`). Rust's
//! `format!("{:.*}", n, x)` uses `core::fmt`'s exact `flt2dec` mode, which is
//! likewise correctly rounded with ties to even, and `str::parse::<f64>()` is
//! correctly rounded. Composing them reproduces CPython bit for bit, including the
//! famous `round(2.675, 2) == 2.67` (2.675 is really 2.674999…).

/// Round `x` to `digits` places after the decimal point, half-to-even, exactly as
/// Python's `round(x, digits)` does.
///
/// Non-finite inputs are returned unchanged, matching `round(float("inf"), 2)`.
///
/// ```
/// use browser_core::numeric::round_half_even;
///
/// // The binary value of 2.675 is just below the tie, so it rounds *down*.
/// assert_eq!(round_half_even(2.675, 2), 2.67);
/// // Genuine ties go to the even digit.
/// assert_eq!(round_half_even(0.125, 2), 0.12);
/// assert_eq!(round_half_even(0.135, 2), 0.14);
/// assert_eq!(round_half_even(2.5, 0), 2.0);
/// assert_eq!(round_half_even(1.5, 0), 2.0);
/// assert_eq!(round_half_even(0.5, 0), 0.0);
/// ```
pub fn round_half_even(x: f64, digits: u32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // A double needs at most ~1080 significant decimal digits; beyond ~330 fractional
    // digits every finite double is already exact, so formatting cannot change it.
    // Cap to keep the temporary allocation bounded for absurd `digits`.
    let digits = digits.min(1_100) as usize;
    let rendered = format!("{x:.digits$}");
    rendered.parse::<f64>().unwrap_or(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation below was produced by CPython 3.14 `repr(round(x, n))`.
    #[test]
    fn matches_cpython_round() {
        let cases: &[(f64, u32, f64)] = &[
            (2.675, 2, 2.67),
            (0.5, 0, 0.0),
            (1.5, 0, 2.0),
            (2.5, 0, 2.0),
            (-0.5, 0, -0.0),
            (-1.5, 0, -2.0),
            (-2.5, 0, -2.0),
            (0.125, 2, 0.12),
            (0.135, 2, 0.14),
            (1.005, 2, 1.0),
            (2.345, 2, 2.35),
            (-2.675, 2, -2.67),
            (0.0, 2, 0.0),
            (2.675, 3, 2.675),
            (1.0 / 3.0, 2, 0.33),
            (2.0 / 3.0, 2, 0.67),
            (1.0e16 + 2.0, 2, 1.0000000000000002e16),
            (123.456, 1, 123.5),
            (123.456, 0, 123.0),
            (-123.456, 0, -123.0),
            (1e-10, 2, 0.0),
            (9.995, 2, 9.99),
        ];
        for &(x, n, want) in cases {
            let got = round_half_even(x, n);
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "round_half_even({x}, {n}) = {got}, expected {want}"
            );
        }
    }

    #[test]
    fn naive_scaling_would_disagree() {
        // The trap this function exists to avoid.
        let naive = (2.675_f64 * 100.0).round() / 100.0;
        assert_eq!(naive, 2.68);
        assert_eq!(round_half_even(2.675, 2), 2.67);
    }

    #[test]
    fn non_finite_passes_through() {
        assert!(round_half_even(f64::NAN, 2).is_nan());
        assert_eq!(round_half_even(f64::INFINITY, 2), f64::INFINITY);
        assert_eq!(round_half_even(f64::NEG_INFINITY, 2), f64::NEG_INFINITY);
    }

    #[test]
    fn huge_digit_counts_are_clamped_but_lossless() {
        assert_eq!(round_half_even(1.0 / 3.0, u32::MAX), 1.0 / 3.0);
        assert_eq!(round_half_even(2.675, 50), 2.675);
    }
}
