//! Fixed-point score arithmetic shared by test weighting and report aggregation.
//!
//! All scores are carried end-to-end as integers in units of `1 / SCORE_SCALE` points.
//! `TestCase::weight` is the source of truth (already in fixed units); per-test scores are
//! `round(weight * ratio)`, and subtask / submission totals are integer sums compared with
//! integer maxima. Floating point values (`f64`) only appear at the reporting boundary and are
//! converted back losslessly with [`score_to_fixed`], so an all-accepted run always reaches
//! exactly the maximum score regardless of how many test cases the points are split across.

/// Number of fixed-point units per point (6 decimal digits of precision).
pub const SCORE_SCALE: u64 = 1_000_000;

/// Convert a (non-negative) point value into fixed-point units.
///
/// Non-finite and negative inputs map to `0`; values beyond the representable range saturate.
pub fn score_to_fixed(score: f64) -> u64 {
    if !score.is_finite() || score <= 0.0 {
        return 0;
    }
    let scaled = (score * SCORE_SCALE as f64).round();
    if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    }
}

/// Convert fixed-point units back into a point value for reporting.
pub fn fixed_to_score(fixed: u64) -> f64 {
    fixed as f64 / SCORE_SCALE as f64
}

/// Fixed-point score earned by a test case of `weight` units judged with `ratio` in `[0, 1]`.
///
/// `ratio` is clamped into `[0, 1]`; NaN is treated as `0`.
pub fn weighted_fixed(weight: u64, ratio: f64) -> u64 {
    let ratio = if ratio.is_nan() {
        0.0
    } else {
        ratio.clamp(0.0, 1.0)
    };
    if ratio >= 1.0 {
        return weight;
    }
    let v = (weight as f64 * ratio).round();
    if v <= 0.0 {
        0
    } else if v >= weight as f64 {
        weight
    } else {
        v as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_is_lossless_for_weights() {
        for w in [
            0u64,
            1,
            333_333,
            33_333_333,
            100_000_000,
            142_857_143,
            8_333_334,
        ] {
            assert_eq!(score_to_fixed(fixed_to_score(w)), w);
        }
    }

    #[test]
    fn test_weighted_fixed_clamps() {
        assert_eq!(weighted_fixed(1000, 1.0), 1000);
        assert_eq!(weighted_fixed(1000, 2.0), 1000);
        assert_eq!(weighted_fixed(1000, -1.0), 0);
        assert_eq!(weighted_fixed(1000, f64::NAN), 0);
        assert_eq!(weighted_fixed(1000, 0.5), 500);
    }

    #[test]
    fn test_score_to_fixed_edge_cases() {
        assert_eq!(score_to_fixed(f64::NAN), 0);
        assert_eq!(score_to_fixed(-3.0), 0);
        assert_eq!(score_to_fixed(33.333333), 33_333_333);
        assert_eq!(score_to_fixed(f64::INFINITY), 0);
        assert_eq!(score_to_fixed(1e300), u64::MAX);
    }
}
