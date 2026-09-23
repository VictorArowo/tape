//! Backtest settings: starting capital and how orders are executed.

use serde::{Deserialize, Serialize};
use tape_core::{Liquidity, Money, TimeDelta};

/// Rates are in parts per million of notional: 1_000 ppm = 0.1% = 10 bps.
pub const PPM: i64 = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeModel {
    pub maker_ppm: i64,
    pub taker_ppm: i64,
}

impl FeeModel {
    pub const ZERO: Self = Self {
        maker_ppm: 0,
        taker_ppm: 0,
    };

    /// Binance spot's base tier: 0.1% for makers and takers.
    pub const BINANCE_SPOT: Self = Self {
        maker_ppm: 1_000,
        taker_ppm: 1_000,
    };

    pub fn rate_ppm(&self, liquidity: Liquidity) -> i64 {
        match liquidity {
            Liquidity::Maker => self.maker_ppm,
            Liquidity::Taker => self.taker_ppm,
        }
    }

    pub fn fee(&self, notional: Money, liquidity: Liquidity) -> Money {
        notional.mul_ratio(self.rate_ppm(liquidity), PPM)
    }
}

/// How the simulated exchange treats orders.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionModel {
    /// Time between a strategy sending an order (or cancel) and the
    /// exchange acting on it.
    pub latency: TimeDelta,
    pub fees: FeeModel,
    /// Adverse price move applied to every taker fill, in ppm of price. A
    /// stand-in for crossing the spread and moving the book.
    pub slippage_ppm: i64,
    /// Largest share of a bar's volume, in ppm, that all of the strategy's
    /// orders together can fill within that bar. `None` means unlimited.
    pub max_bar_participation_ppm: Option<i64>,
    /// When true, a resting limit order fills only if the market trades
    /// through its price. When false, touching the price is enough, which
    /// assumes you were first in the queue.
    pub require_trade_through: bool,
}

impl ExecutionModel {
    /// What most quick backtests assume: instant, free, unlimited fills.
    pub fn naive() -> Self {
        Self {
            latency: TimeDelta::ZERO,
            fees: FeeModel::ZERO,
            slippage_ppm: 0,
            max_bar_participation_ppm: None,
            require_trade_through: false,
        }
    }

    /// Conservative defaults for a retail account on a liquid crypto pair.
    pub fn realistic() -> Self {
        Self {
            latency: TimeDelta::from_millis(100),
            fees: FeeModel::BINANCE_SPOT,
            slippage_ppm: 200,
            max_bar_participation_ppm: Some(100_000),
            require_trade_through: true,
        }
    }
}

impl Default for ExecutionModel {
    fn default() -> Self {
        Self::realistic()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub initial_cash: Money,
    pub execution: ExecutionModel,
    /// Allow selling more than you hold. Spot accounts can't, so the
    /// default is false.
    pub allow_short: bool,
    /// How often equity is recorded for the equity curve and statistics.
    pub equity_interval: TimeDelta,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: Money::from_int(10_000).expect("in range"),
            execution: ExecutionModel::default(),
            allow_short: false,
            equity_interval: TimeDelta::from_mins(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fees_are_exact() {
        let notional: Money = "12345.67".parse().unwrap();
        assert_eq!(
            FeeModel::BINANCE_SPOT.fee(notional, Liquidity::Taker),
            "12.34567".parse().unwrap()
        );
        assert_eq!(FeeModel::ZERO.fee(notional, Liquidity::Maker), Money::ZERO);
    }
}
