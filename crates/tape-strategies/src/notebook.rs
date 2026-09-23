//! The backtest a quick notebook produces, for comparison.
//!
//! The common vectorized version of a moving-average crossover looks like:
//!
//! ```text
//! signal  = sma(close, fast) > sma(close, slow)
//! returns = close.pct_change()
//! equity  = (1 + signal * returns).cumprod()
//! ```
//!
//! It has no fees, no slippage and no volume limits, and it forgets to shift
//! the signal by one bar. Each bar's position is decided using that bar's
//! close and then paid that same bar's return: a one-bar look-ahead. The
//! engine makes this impossible; this module reproduces it on purpose so
//! the size of the error can be measured.

use tape_core::{Price, Timestamp};

/// Equity after each bar for an all-in, long-or-flat crossover computed
/// the notebook way, starting from `initial`.
pub fn sma_cross_with_lookahead(
    closes: &[(Timestamp, Price)],
    fast: usize,
    slow: usize,
    initial: f64,
) -> Vec<(Timestamp, f64)> {
    assert!(0 < fast && fast < slow, "need 0 < fast < slow");
    let prices: Vec<f64> = closes.iter().map(|(_, p)| p.to_f64()).collect();
    let mut equity = initial;
    let mut out = Vec::with_capacity(closes.len());
    let (mut fast_sum, mut slow_sum) = (0.0, 0.0);
    for (i, &(ts, _)) in closes.iter().enumerate() {
        fast_sum += prices[i];
        slow_sum += prices[i];
        if i >= fast {
            fast_sum -= prices[i - fast];
        }
        if i >= slow {
            slow_sum -= prices[i - slow];
        }
        if i > 0 && i + 1 >= slow {
            // The bug: this bar's signal includes this bar's close...
            let long = fast_sum / fast as f64 > slow_sum / slow as f64;
            // ...and is paid this bar's return.
            if long {
                equity *= prices[i] / prices[i - 1];
            }
        }
        out.push((ts, equity));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_core::TimeDelta;

    fn series(prices: &[f64]) -> Vec<(Timestamp, Price)> {
        let start = Timestamp::from_ymd(2024, 1, 1);
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    start + TimeDelta::from_mins(i as i64),
                    Price::from_raw((p * 1e8).round() as i64),
                )
            })
            .collect()
    }

    #[test]
    fn look_ahead_catches_the_move_that_triggers_the_signal() {
        // Flat, then one big jump. A correct backtest can only be long
        // after the jump is known; the notebook version is long for it.
        let prices = [100.0, 100.0, 100.0, 100.0, 110.0, 110.0];
        let equity = sma_cross_with_lookahead(&series(&prices), 1, 3, 1_000.0);
        assert!((equity.last().unwrap().1 - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn stays_flat_while_averages_are_warming_up() {
        let prices = [100.0, 120.0, 140.0];
        let equity = sma_cross_with_lookahead(&series(&prices), 1, 3, 1_000.0);
        assert_eq!(equity[1].1, 1_000.0);
    }
}
