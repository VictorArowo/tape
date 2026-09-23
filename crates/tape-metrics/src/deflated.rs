//! How much to believe the best of many backtests.
//!
//! Try enough strategies and one of them will look good by chance. The
//! deflated Sharpe ratio (Bailey and López de Prado, 2014) is the
//! probability that a strategy's true Sharpe ratio is above zero, after
//! allowing for how many strategies were tried, how much their Sharpe
//! ratios varied, and how far the returns are from normally distributed
//! (skew and fat tails make a high Sharpe ratio less impressive).
//!
//! All Sharpe ratios here are per period (for example daily), not
//! annualized.

/// Euler–Mascheroni constant.
const EULER_GAMMA: f64 = 0.577_215_664_901_532_9;

/// Standard normal cumulative distribution function.
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// Complementary error function, accurate to about 1.2e-7 (Numerical
/// Recipes' Chebyshev approximation).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98
                                + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    let r = t * poly.exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// Inverse of the standard normal CDF (Acklam's algorithm, relative error
/// about 1.15e-9). Returns ±infinity at 0 and 1.
pub fn norm_inv(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let tail = |q: f64| {
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    const LOW: f64 = 0.024_25;
    if p < LOW {
        tail((-2.0 * p.ln()).sqrt())
    } else if p > 1.0 - LOW {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

/// Sample skewness and (non-excess) kurtosis. A normal distribution has
/// skew 0 and kurtosis 3.
fn moments(returns: &[f64]) -> (f64, f64) {
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let m2 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    if m2 == 0.0 {
        return (0.0, 3.0);
    }
    let m3 = returns.iter().map(|r| (r - mean).powi(3)).sum::<f64>() / n;
    let m4 = returns.iter().map(|r| (r - mean).powi(4)).sum::<f64>() / n;
    (m3 / m2.powf(1.5), m4 / (m2 * m2))
}

/// The Sharpe ratio the best of `trials` strategies would be expected to
/// reach by luck alone, if none had any skill and their Sharpe ratios
/// varied with variance `trial_sharpe_variance`.
pub fn expected_max_sharpe(trials: usize, trial_sharpe_variance: f64) -> f64 {
    if trials < 2 || trial_sharpe_variance <= 0.0 {
        return 0.0;
    }
    let n = trials as f64;
    trial_sharpe_variance.sqrt()
        * ((1.0 - EULER_GAMMA) * norm_inv(1.0 - 1.0 / n)
            + EULER_GAMMA * norm_inv(1.0 - 1.0 / (n * std::f64::consts::E)))
}

/// Probability that the true Sharpe ratio exceeds `benchmark`, given an
/// observed per-period Sharpe ratio over `returns` (the probabilistic
/// Sharpe ratio). `None` if there are fewer than three returns.
pub fn probabilistic_sharpe(sharpe: f64, returns: &[f64], benchmark: f64) -> Option<f64> {
    if returns.len() < 3 {
        return None;
    }
    let (skew, kurtosis) = moments(returns);
    let variance = 1.0 - skew * sharpe + (kurtosis - 1.0) / 4.0 * sharpe * sharpe;
    if variance <= 0.0 {
        return None;
    }
    let t = returns.len() as f64;
    Some(norm_cdf(
        (sharpe - benchmark) * (t - 1.0).sqrt() / variance.sqrt(),
    ))
}

/// The deflated Sharpe ratio: the probabilistic Sharpe ratio measured
/// against the best Sharpe ratio `trials` skill-less strategies would
/// have produced.
pub fn deflated_sharpe(
    sharpe: f64,
    returns: &[f64],
    trials: usize,
    trial_sharpe_variance: f64,
) -> Option<f64> {
    probabilistic_sharpe(
        sharpe,
        returns,
        expected_max_sharpe(trials, trial_sharpe_variance),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() < tolerance
    }

    #[test]
    fn normal_distribution_reference_values() {
        assert!(close(norm_cdf(0.0), 0.5, 1e-7));
        assert!(close(norm_cdf(1.959_964), 0.975, 1e-6));
        assert!(close(norm_cdf(-1.0), 0.158_655_25, 1e-6));
        assert!(close(norm_inv(0.975), 1.959_964, 1e-6));
        assert!(close(norm_inv(0.01), -2.326_348, 1e-6));
        for p in [0.001, 0.02, 0.3, 0.5, 0.7, 0.98, 0.999] {
            assert!(close(norm_cdf(norm_inv(p)), p, 1e-6), "p = {p}");
        }
    }

    #[test]
    fn more_trials_raise_the_bar_for_luck() {
        let one = expected_max_sharpe(1, 0.01);
        let ten = expected_max_sharpe(10, 0.01);
        let thousand = expected_max_sharpe(1_000, 0.01);
        assert_eq!(one, 0.0);
        assert!(0.0 < ten && ten < thousand);
        // Known value: E[max of 1000 standard normals] is about 3.24.
        assert!(close(expected_max_sharpe(1_000, 1.0), 3.25, 0.05));
    }

    #[test]
    fn deflating_for_many_trials_lowers_confidence() {
        // Two years of daily returns with a per-day Sharpe of 0.1
        // (about 1.9 annualized), alternating so skew is zero.
        let returns: Vec<f64> = (0..730)
            .map(|i| if i % 2 == 0 { 0.011 } else { -0.009 })
            .collect();
        let sharpe = 0.1;
        let alone = probabilistic_sharpe(sharpe, &returns, 0.0).unwrap();
        let after_200 = deflated_sharpe(sharpe, &returns, 200, 0.05_f64.powi(2)).unwrap();
        assert!(alone > 0.99, "{alone}");
        assert!(after_200 < alone, "{after_200} vs {alone}");
        assert!(
            after_200 < 0.5,
            "a Sharpe this size is what 200 tries produce: {after_200}"
        );
    }
}
