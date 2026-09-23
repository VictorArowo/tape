//! Reference strategies: simple, well-known baselines to test the engine
//! against and to compare anything cleverer with.

use std::collections::VecDeque;

use tape_core::{InstrumentId, MarketEvent, OrderRequest, Price, Side};
use tape_engine::{Context, Strategy};

/// Room left for slippage when sizing an all-in buy, in ppm.
const SIZING_MARGIN_PPM: i64 = 5_000;

/// Buys as much as it can on the first price it sees and holds.
pub struct BuyAndHold {
    instrument: InstrumentId,
    bought: bool,
}

impl BuyAndHold {
    pub fn new(instrument: InstrumentId) -> Self {
        Self {
            instrument,
            bought: false,
        }
    }
}

impl Strategy for BuyAndHold {
    fn name(&self) -> String {
        "buy-and-hold".into()
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent) {
        if self.bought || event.instrument() != self.instrument {
            return;
        }
        let Some(price) = ctx.last_price(self.instrument) else {
            return;
        };
        let qty = ctx.affordable_qty(self.instrument, price, ctx.cash(), SIZING_MARGIN_PPM);
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(self.instrument, Side::Buy, qty));
            self.bought = true;
        }
    }
}

/// Simple moving average of the last `len` bar closes, kept as an exact
/// running sum.
struct Sma {
    len: usize,
    window: VecDeque<i64>,
    sum: i128,
}

impl Sma {
    fn new(len: usize) -> Self {
        assert!(len > 0, "moving average length must be positive");
        Self {
            len,
            window: VecDeque::with_capacity(len + 1),
            sum: 0,
        }
    }

    fn push(&mut self, price: Price) {
        self.window.push_back(price.raw());
        self.sum += i128::from(price.raw());
        if self.window.len() > self.len {
            self.sum -= i128::from(self.window.pop_front().expect("non-empty"));
        }
    }

    /// Sum over the full window, or `None` until the window has filled.
    /// Comparing `a.sum * b.len` with `b.sum * a.len` avoids division.
    fn ready_sum(&self) -> Option<i128> {
        (self.window.len() == self.len).then_some(self.sum)
    }
}

/// Long when the fast moving average of closes is above the slow one, flat
/// otherwise. Trades with market orders, all in or all out.
pub struct SmaCross {
    instrument: InstrumentId,
    fast: Sma,
    slow: Sma,
}

impl SmaCross {
    pub fn new(instrument: InstrumentId, fast: usize, slow: usize) -> Self {
        assert!(fast < slow, "fast average must be shorter than slow");
        Self {
            instrument,
            fast: Sma::new(fast),
            slow: Sma::new(slow),
        }
    }
}

impl Strategy for SmaCross {
    fn name(&self) -> String {
        format!("sma-cross({},{})", self.fast.len, self.slow.len)
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent) {
        let MarketEvent::Bar(bar) = event else {
            return;
        };
        if bar.instrument != self.instrument {
            return;
        }
        self.fast.push(bar.close);
        self.slow.push(bar.close);
        let (Some(fast), Some(slow)) = (self.fast.ready_sum(), self.slow.ready_sum()) else {
            return;
        };
        if ctx.open_orders().next().is_some() {
            return; // wait for the last order to finish
        }
        let want_long = fast * self.slow.len as i128 > slow * self.fast.len as i128;
        let position = ctx.position(self.instrument);
        if want_long && position.is_zero() {
            let qty = ctx.affordable_qty(self.instrument, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(self.instrument, Side::Buy, qty));
            }
        } else if !want_long && position.is_positive() {
            ctx.submit(OrderRequest::market(self.instrument, Side::Sell, position));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tape_core::{Bar, Instrument, Money, Qty, TimeDelta, Timestamp};
    use tape_engine::{Backtest, BacktestConfig, ExecutionModel};

    const BTC: InstrumentId = InstrumentId(1);

    fn run(strategy: &mut dyn Strategy, closes: &[i64]) -> tape_engine::BacktestResult {
        let start = Timestamp::from_ymd(2024, 1, 1);
        let events = closes.iter().enumerate().map(|(i, c)| {
            let price = Price::from_int(*c).unwrap();
            let open_ts = start + TimeDelta::from_mins(i as i64);
            Ok::<_, Infallible>(MarketEvent::Bar(Bar {
                open_ts,
                close_ts: open_ts + TimeDelta::from_mins(1),
                instrument: BTC,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: Qty::from_int(1_000).unwrap(),
            }))
        });
        let config = BacktestConfig {
            initial_cash: Money::from_int(10_000).unwrap(),
            execution: ExecutionModel::naive(),
            ..BacktestConfig::default()
        };
        let btc = Instrument::new(
            BTC,
            "BTCUSDT",
            "0.01".parse().unwrap(),
            "0.00001".parse().unwrap(),
        );
        Backtest::new(config, [btc]).run(strategy, events).unwrap()
    }

    #[test]
    fn buy_and_hold_goes_all_in_once() {
        let result = run(&mut BuyAndHold::new(BTC), &[100, 100, 110, 120]);
        assert_eq!(result.fills.len(), 1);
        assert!(result.final_cash < Money::from_int(60).unwrap());
        assert!(result.final_equity > Money::from_int(11_900).unwrap());
    }

    #[test]
    fn sma_cross_follows_the_trend_and_exits() {
        // Rising, then falling: expect one entry and one exit.
        let closes: Vec<i64> = (0..20)
            .map(|i| 100 + i)
            .chain((0..20).map(|i| 120 - 2 * i))
            .collect();
        let result = run(&mut SmaCross::new(BTC, 3, 8), &closes);
        let sides: Vec<Side> = result.fills.iter().map(|f| f.side).collect();
        assert_eq!(sides, vec![Side::Buy, Side::Sell]);
        assert!(result.final_positions.is_empty());
    }

    #[test]
    fn sma_is_exact() {
        let mut sma = Sma::new(3);
        for p in ["1.1", "2.2", "3.3", "4.4"] {
            sma.push(p.parse().unwrap());
        }
        let expected: Price = "9.9".parse().unwrap(); // 2.2 + 3.3 + 4.4
        assert_eq!(sma.ready_sum(), Some(i128::from(expected.raw())));
    }
}
