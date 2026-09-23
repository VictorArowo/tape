//! Two more reference strategies: a mean-reversion strategy that trades
//! with resting limit orders, and a channel breakout that trades with
//! market orders. Between them they exercise every part of the execution
//! model: maker and taker fees, fill-on-touch, volume caps and latency.

use std::collections::VecDeque;

use tape_core::{InstrumentId, MarketEvent, OrderRequest, OrderType, Price, Side};
use tape_engine::{Context, Strategy};

use crate::SIZING_MARGIN_PPM;

/// Rounds an f64 price to the instrument's tick, down for bids and up for
/// offers, so the strategy never quotes a better price than it computed.
fn to_tick(ctx: &Context<'_>, instrument: InstrumentId, price: f64, up: bool) -> Option<Price> {
    let tick = ctx.instrument(instrument)?.tick_size.raw();
    let raw = (price * 1e8).round() as i64;
    if raw <= 0 {
        return None;
    }
    let floor = raw.div_euclid(tick) * tick;
    Some(
        Price::from_raw(if up && floor != raw {
            floor + tick
        } else {
            floor
        })
        .max(Price::from_raw(tick)),
    )
}

/// Buys when the price falls `band` standard deviations below its moving
/// average, with a resting limit order at that level, and sells at the
/// average with another. Exits at market if the price keeps falling to
/// twice the band.
///
/// The average and variance come from exact integer running sums of raw
/// prices, so they're identical on every platform.
pub struct MeanReversion {
    instrument: InstrumentId,
    window: usize,
    band: f64,
    closes: VecDeque<i64>,
    sum: i128,
    sum_squares: i128,
}

impl MeanReversion {
    pub fn new(instrument: InstrumentId, window: usize, band: f64) -> Self {
        assert!(window >= 2, "window must be at least 2 bars");
        assert!(band > 0.0, "band must be positive");
        Self {
            instrument,
            window,
            band,
            closes: VecDeque::with_capacity(window + 1),
            sum: 0,
            sum_squares: 0,
        }
    }

    fn push(&mut self, close: Price) {
        let raw = close.raw();
        self.closes.push_back(raw);
        self.sum += i128::from(raw);
        self.sum_squares += i128::from(raw) * i128::from(raw);
        if self.closes.len() > self.window {
            let old = i128::from(self.closes.pop_front().expect("non-empty"));
            self.sum -= old;
            self.sum_squares -= old * old;
        }
    }

    /// Mean and standard deviation in whole price units, once the window
    /// is full.
    fn stats(&self) -> Option<(f64, f64)> {
        if self.closes.len() < self.window {
            return None;
        }
        let n = self.window as i128;
        // n^2 * variance, exactly, in raw units squared.
        let spread = n * self.sum_squares - self.sum * self.sum;
        let mean = self.sum as f64 / n as f64 / 1e8;
        let std = (spread.max(0) as f64).sqrt() / n as f64 / 1e8;
        Some((mean, std))
    }
}

impl Strategy for MeanReversion {
    fn name(&self) -> String {
        format!("mean-reversion({},{})", self.window, self.band)
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent) {
        let MarketEvent::Bar(bar) = event else {
            return;
        };
        if bar.instrument != self.instrument {
            return;
        }
        self.push(bar.close);
        let Some((mean, std)) = self.stats() else {
            return;
        };
        let id = self.instrument;
        let position = ctx.position(id);
        let open: Vec<(tape_core::OrderId, Side, Option<Price>)> = ctx
            .open_orders()
            .map(|(order_id, order)| {
                let price = match order.request.order_type {
                    OrderType::Limit { price } => Some(price),
                    _ => None,
                };
                (order_id, order.request.side, price)
            })
            .collect();

        if position.is_positive() && bar.close.to_f64() < mean - 2.0 * self.band * std {
            // The move kept going: get out, whatever is resting.
            for (order_id, _, _) in &open {
                ctx.cancel(*order_id);
            }
            if open
                .iter()
                .all(|(_, side, price)| !(*side == Side::Sell && price.is_none()))
            {
                ctx.submit(OrderRequest::market(id, Side::Sell, position));
            }
            return;
        }

        let (side, target) = if position.is_zero() {
            (Side::Buy, to_tick(ctx, id, mean - self.band * std, false))
        } else {
            (Side::Sell, to_tick(ctx, id, mean, true))
        };
        let Some(target) = target else {
            return;
        };
        match open.as_slice() {
            [] => {
                let qty = match side {
                    Side::Buy => ctx.affordable_qty(id, target, ctx.cash(), SIZING_MARGIN_PPM),
                    Side::Sell => position,
                };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::limit(id, side, qty, target));
                }
            }
            // Keep a resting order at the current level: replace it when the
            // band has moved, or when it's on the wrong side after a fill.
            orders => {
                for (order_id, order_side, price) in orders {
                    if *order_side != side || *price != Some(target) {
                        ctx.cancel(*order_id);
                    }
                }
            }
        }
    }
}

/// A rolling maximum (or minimum) over the last `len` values, in O(1)
/// amortized time per value.
struct Extreme {
    len: usize,
    max: bool,
    seen: usize,
    /// (index, value), with values monotonic from front to back.
    queue: VecDeque<(usize, Price)>,
}

impl Extreme {
    fn new(len: usize, max: bool) -> Self {
        Self {
            len,
            max,
            seen: 0,
            queue: VecDeque::new(),
        }
    }

    fn push(&mut self, value: Price) {
        let dominated = |old: Price| if self.max { old <= value } else { old >= value };
        while self.queue.back().is_some_and(|(_, old)| dominated(*old)) {
            self.queue.pop_back();
        }
        self.queue.push_back((self.seen, value));
        self.seen += 1;
        while self
            .queue
            .front()
            .is_some_and(|(i, _)| *i + self.len < self.seen)
        {
            self.queue.pop_front();
        }
    }

    fn value(&self) -> Option<Price> {
        (self.seen >= self.len).then(|| self.queue.front().map(|(_, v)| *v))?
    }
}

/// Buys when a bar closes above the highest high of the previous
/// `lookback` bars, and sells when one closes below the lowest low.
pub struct Breakout {
    instrument: InstrumentId,
    lookback: usize,
    highs: Extreme,
    lows: Extreme,
}

impl Breakout {
    pub fn new(instrument: InstrumentId, lookback: usize) -> Self {
        assert!(lookback >= 1, "lookback must be at least 1 bar");
        Self {
            instrument,
            lookback,
            highs: Extreme::new(lookback, true),
            lows: Extreme::new(lookback, false),
        }
    }
}

impl Strategy for Breakout {
    fn name(&self) -> String {
        format!("breakout({})", self.lookback)
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent) {
        let MarketEvent::Bar(bar) = event else {
            return;
        };
        if bar.instrument != self.instrument {
            return;
        }
        // Compare with the channel before this bar joins it.
        let (high, low) = (self.highs.value(), self.lows.value());
        self.highs.push(bar.high);
        self.lows.push(bar.low);
        let (Some(high), Some(low)) = (high, low) else {
            return;
        };
        if ctx.open_orders().next().is_some() {
            return;
        }
        let position = ctx.position(self.instrument);
        if position.is_zero() && bar.close > high {
            let qty = ctx.affordable_qty(self.instrument, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(self.instrument, Side::Buy, qty));
            }
        } else if position.is_positive() && bar.close < low {
            ctx.submit(OrderRequest::market(self.instrument, Side::Sell, position));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tape_core::{Bar, Instrument, Liquidity, Money, Qty, TimeDelta, Timestamp};
    use tape_engine::{Backtest, BacktestConfig, BacktestResult, ExecutionModel};

    const BTC: InstrumentId = InstrumentId(1);

    fn run(
        strategy: &mut dyn Strategy,
        bars: &[(f64, f64, f64, f64)],
        execution: ExecutionModel,
    ) -> BacktestResult {
        let start = Timestamp::from_ymd(2024, 1, 1);
        let p = |x: f64| Price::from_raw((x * 1e8).round() as i64);
        let events = bars.iter().enumerate().map(|(i, (o, h, l, c))| {
            let open_ts = start + TimeDelta::from_mins(i as i64);
            Ok::<_, Infallible>(MarketEvent::Bar(Bar {
                open_ts,
                close_ts: open_ts + TimeDelta::from_mins(1),
                instrument: BTC,
                open: p(*o),
                high: p(*h),
                low: p(*l),
                close: p(*c),
                volume: Qty::from_int(1_000).unwrap(),
            }))
        });
        let config = BacktestConfig {
            initial_cash: Money::from_int(10_000).unwrap(),
            execution,
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

    fn flat(price: f64) -> (f64, f64, f64, f64) {
        (price, price, price, price)
    }

    #[test]
    fn mean_reversion_buys_the_dip_with_a_limit_and_sells_at_the_mean() {
        // Oscillate around 100 so the band is well defined, dip, recover.
        let mut bars: Vec<_> = (0..40)
            .map(|i| flat(if i % 2 == 0 { 99.0 } else { 101.0 }))
            .collect();
        bars.push((100.0, 100.0, 96.0, 97.0)); // trades through the lower band
        bars.extend((0..5).map(|_| (100.0, 102.0, 99.5, 101.0))); // back above the mean
        let result = run(
            &mut MeanReversion::new(BTC, 20, 1.5),
            &bars,
            ExecutionModel::realistic(),
        );
        let entry = &result.fills[0];
        assert_eq!(
            (entry.side, entry.liquidity),
            (Side::Buy, Liquidity::Maker),
            "entered with a resting limit"
        );
        assert!(
            result.fills.iter().any(|f| f.side == Side::Sell),
            "{:?}",
            result.fills
        );
        assert!(result.final_positions.is_empty());
    }

    #[test]
    fn fill_on_touch_changes_mean_reversion_results() {
        // The dip only touches the band exactly: fills only if touching counts.
        let mut bars: Vec<_> = (0..40)
            .map(|i| flat(if i % 2 == 0 { 99.0 } else { 101.0 }))
            .collect();
        let (mean, std) = {
            let mut m = MeanReversion::new(BTC, 20, 1.5);
            for b in &bars {
                m.push(Price::from_raw((b.3 * 1e8) as i64));
            }
            m.stats().unwrap()
        };
        // The lower band sits at 98.5, below anything the oscillation reaches;
        // one bar dips to exactly that level and closes back at 100.
        let band = ((mean - 1.5 * std) * 100.0).floor() / 100.0;
        bars.push((100.0, 100.0, band, 100.0));
        bars.push(flat(100.0));
        let strict = run(
            &mut MeanReversion::new(BTC, 20, 1.5),
            &bars,
            ExecutionModel::realistic(),
        );
        let lenient = run(
            &mut MeanReversion::new(BTC, 20, 1.5),
            &bars,
            ExecutionModel::naive(),
        );
        assert!(strict.fills.is_empty(), "{:?}", strict.fills);
        assert_eq!(lenient.fills.len(), 1);
    }

    #[test]
    fn breakout_enters_on_a_new_high_and_exits_on_a_new_low() {
        let mut bars: Vec<_> = (0..10).map(|_| (100.0, 101.0, 99.0, 100.0)).collect();
        bars.push((100.0, 103.0, 100.0, 102.5)); // closes above the 101 channel top
        bars.extend((0..3).map(|_| (102.0, 103.0, 101.5, 102.0)));
        bars.push((100.0, 100.5, 97.0, 98.0)); // closes below the channel bottom
        bars.push(flat(98.0));
        let result = run(&mut Breakout::new(BTC, 5), &bars, ExecutionModel::naive());
        let sides: Vec<Side> = result.fills.iter().map(|f| f.side).collect();
        assert_eq!(sides, vec![Side::Buy, Side::Sell]);
    }

    #[test]
    fn rolling_extremes_match_a_brute_force_scan() {
        let values: Vec<Price> = (0..200)
            .map(|i: i64| Price::from_raw((i * 7919 % 101) * 1_000_000))
            .collect();
        let (mut max, mut min) = (Extreme::new(13, true), Extreme::new(13, false));
        for (i, v) in values.iter().enumerate() {
            max.push(*v);
            min.push(*v);
            if i + 1 >= 13 {
                let window = &values[i + 1 - 13..=i];
                assert_eq!(max.value(), window.iter().max().copied());
                assert_eq!(min.value(), window.iter().min().copied());
            } else {
                assert_eq!(max.value(), None);
            }
        }
    }
}
