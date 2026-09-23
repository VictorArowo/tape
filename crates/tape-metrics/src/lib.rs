//! Performance statistics from a backtest's equity curve.
//!
//! This is the one part of tape that uses floating point: these numbers
//! describe a run, they never feed back into it.
//!
//! Returns are measured over fixed periods (daily by default), using the
//! last equity sample at or before each period boundary. Ratios are
//! annualized with a periods-per-year figure; crypto trades every day, so
//! daily data uses 365. The risk-free rate is taken as zero.

use serde::{Deserialize, Serialize};
use tape_core::{TimeDelta, Timestamp};
use tape_engine::EquityPoint;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub start: Timestamp,
    pub end: Timestamp,
    pub initial_equity: f64,
    pub final_equity: f64,
    /// Final over initial equity, minus one.
    pub total_return: f64,
    /// Compound annual growth rate. `None` for runs shorter than a day,
    /// where annualizing would be meaningless.
    pub annualized_return: Option<f64>,
    pub annualized_volatility: Option<f64>,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    /// Largest peak-to-trough fall, as a fraction of the peak.
    pub max_drawdown: f64,
    /// Longest time spent below a previous peak.
    pub max_drawdown_duration: TimeDelta,
    /// Number of periodic returns the ratios are based on.
    pub periods: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Periods {
    pub length: TimeDelta,
    pub per_year: f64,
}

impl Periods {
    /// Daily returns on a market that never closes.
    pub const DAILY_24_7: Self = Self {
        length: TimeDelta::from_days(1),
        per_year: 365.0,
    };

    pub const HOURLY_24_7: Self = Self {
        length: TimeDelta::from_hours(1),
        per_year: 365.0 * 24.0,
    };
}

const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 3600.0;

/// Equity at the end of each period, starting with the first sample.
fn resample(equity: &[EquityPoint], length: TimeDelta) -> Vec<f64> {
    let step = length.as_nanos().max(1);
    let mut out = vec![equity[0].equity.to_f64()];
    let mut boundary = (equity[0].ts.as_nanos().div_euclid(step) + 1) * step;
    let mut last = equity[0].equity.to_f64();
    for point in &equity[1..] {
        while point.ts.as_nanos() > boundary {
            out.push(last);
            boundary += step;
        }
        last = point.equity.to_f64();
    }
    // The final, possibly partial, period.
    if equity.len() > 1 {
        out.push(last);
    }
    out
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample standard deviation.
fn std_dev(xs: &[f64]) -> f64 {
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() as f64 - 1.0)).sqrt()
}

fn drawdowns(equity: &[EquityPoint]) -> (f64, TimeDelta) {
    let mut peak = equity[0].equity.to_f64();
    let mut peak_ts = equity[0].ts;
    let (mut worst, mut longest) = (0.0_f64, TimeDelta::ZERO);
    for point in equity {
        let value = point.equity.to_f64();
        if value >= peak {
            peak = value;
            peak_ts = point.ts;
        } else {
            if peak > 0.0 {
                worst = worst.max((peak - value) / peak);
            }
            longest = longest.max(point.ts - peak_ts);
        }
    }
    (worst, longest)
}

/// Computes statistics for an equity curve. Returns `None` if it's empty.
pub fn compute(equity: &[EquityPoint], periods: Periods) -> Option<Metrics> {
    let first = equity.first()?;
    let last = equity.last()?;
    let (initial, final_) = (first.equity.to_f64(), last.equity.to_f64());
    let total_return = if initial > 0.0 {
        final_ / initial - 1.0
    } else {
        0.0
    };

    let years = (last.ts - first.ts).as_secs_f64() / SECONDS_PER_YEAR;
    let annualized_return = (years >= 1.0 / 365.0 && initial > 0.0 && final_ > 0.0)
        .then(|| (final_ / initial).powf(1.0 / years) - 1.0);

    let closes = resample(equity, periods.length);
    let returns: Vec<f64> = closes
        .windows(2)
        .filter(|w| w[0] > 0.0)
        .map(|w| w[1] / w[0] - 1.0)
        .collect();
    let scale = periods.per_year.sqrt();
    let (annualized_volatility, sharpe, sortino) = if returns.len() >= 2 {
        let (m, sd) = (mean(&returns), std_dev(&returns));
        let downside =
            (returns.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / returns.len() as f64).sqrt();
        (
            Some(sd * scale),
            (sd > 0.0).then(|| m / sd * scale),
            (downside > 0.0).then(|| m / downside * scale),
        )
    } else {
        (None, None, None)
    };

    let (max_drawdown, max_drawdown_duration) = drawdowns(equity);
    Some(Metrics {
        start: first.ts,
        end: last.ts,
        initial_equity: initial,
        final_equity: final_,
        total_return,
        annualized_return,
        annualized_volatility,
        sharpe,
        sortino,
        max_drawdown,
        max_drawdown_duration,
        periods: returns.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_core::Money;

    fn curve(values: &[f64], step: TimeDelta) -> Vec<EquityPoint> {
        let start = Timestamp::from_ymd(2024, 1, 1);
        values
            .iter()
            .enumerate()
            .map(|(i, v)| EquityPoint {
                ts: start + TimeDelta::from_nanos(step.as_nanos() * i as i64),
                equity: Money::from_raw((v * 1e8).round() as i64),
            })
            .collect()
    }

    /// Equal to within one part per million. The test curves store equity
    /// at 10^-8 precision, which perturbs ratios slightly.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1e-3)
    }

    #[test]
    fn flat_curves_have_no_risk() {
        let m = compute(
            &curve(&[100.0; 10], TimeDelta::from_days(1)),
            Periods::DAILY_24_7,
        )
        .unwrap();
        assert_eq!(m.total_return, 0.0);
        assert_eq!(m.max_drawdown, 0.0);
        assert_eq!(m.sharpe, None, "zero volatility has no Sharpe ratio");
        assert_eq!(m.periods, 9);
    }

    #[test]
    fn drawdown_and_its_duration() {
        let m = compute(
            &curve(
                &[100.0, 120.0, 90.0, 110.0, 125.0, 100.0],
                TimeDelta::from_days(1),
            ),
            Periods::DAILY_24_7,
        )
        .unwrap();
        assert!(close(m.max_drawdown, 0.25), "120 -> 90");
        // Below the 125 peak for the last day, and below 120 for two days.
        assert_eq!(m.max_drawdown_duration, TimeDelta::from_days(2));
        assert!(close(m.total_return, 0.0));
    }

    #[test]
    fn sharpe_matches_a_hand_calculation() {
        // Daily returns alternate +2% and -1%.
        let mut values = vec![100.0];
        for i in 0..20 {
            let r = if i % 2 == 0 { 0.02 } else { -0.01 };
            values.push(values.last().unwrap() * (1.0 + r));
        }
        let m = compute(
            &curve(&values, TimeDelta::from_days(1)),
            Periods::DAILY_24_7,
        )
        .unwrap();
        let returns: Vec<f64> = (0..20)
            .map(|i| if i % 2 == 0 { 0.02 } else { -0.01 })
            .collect();
        let expected = mean(&returns) / std_dev(&returns) * 365f64.sqrt();
        assert_eq!(m.periods, 20);
        assert!(
            close(m.sharpe.unwrap(), expected),
            "{} vs {expected}",
            m.sharpe.unwrap()
        );
        let downside = (10.0 * 0.01f64.powi(2) / 20.0).sqrt();
        assert!(close(
            m.sortino.unwrap(),
            mean(&returns) / downside * 365f64.sqrt()
        ));
    }

    #[test]
    fn resamples_intraday_samples_to_periods() {
        // Hourly samples over two days, rising 1 per hour.
        let values: Vec<f64> = (0..=48).map(|h| 100.0 + h as f64).collect();
        let m = compute(
            &curve(&values, TimeDelta::from_hours(1)),
            Periods::DAILY_24_7,
        )
        .unwrap();
        assert_eq!(m.periods, 2);
        assert!(close(m.total_return, 0.48));
        assert!(m.annualized_return.unwrap() > 0.0);
    }

    #[test]
    fn short_runs_are_not_annualized() {
        let m = compute(
            &curve(&[100.0, 101.0], TimeDelta::from_mins(1)),
            Periods::DAILY_24_7,
        )
        .unwrap();
        assert_eq!(m.annualized_return, None);
        assert!(close(m.total_return, 0.01));
    }

    #[test]
    fn empty_curves_have_no_metrics() {
        assert_eq!(compute(&[], Periods::DAILY_24_7), None);
    }
}
