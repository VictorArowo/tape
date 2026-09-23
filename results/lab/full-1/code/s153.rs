use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 30;
const BREAKOUT_LOOKBACK: usize = 15;
const MAX_SIGNAL_RETURN: f64 = 0.002;
const MAX_SELL_PRESSURE_RATIO: f64 = -0.35;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HistoryBar {
    high: Price,
    close: Price,
    signed_volume: f64,
    volume: f64,
}

struct Entry {
    price: Price,
    ts_nanos: i64,
    qty: Qty,
}

struct QuietSellPressureAbsorption {
    history: VecDeque<HistoryBar>,
    entry: Option<Entry>,
    cooldown_until_nanos: i64,
}

impl Strategy for QuietSellPressureAbsorption {
    fn name(&self) -> String {
        "quiet-sell-pressure-absorption".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let close = bar.close.to_f64();
        let volume = bar.volume.to_f64();
        let clv = if bar.high == bar.low {
            0.0
        } else {
            (2.0 * close - high - low) / (high - low)
        };
        let current = HistoryBar {
            high: bar.high,
            close: bar.close,
            signed_volume: volume * clv,
            volume,
        };

        let previous_close = self.history.back().map(|h| h.close);
        let prior_high = self
            .history
            .iter()
            .rev()
            .take(BREAKOUT_LOOKBACK)
            .map(|h| h.high)
            .max();

        let mut signed_sum = current.signed_volume;
        let mut volume_sum = current.volume;
        for h in self.history.iter().rev().take(LOOKBACK - 1) {
            signed_sum += h.signed_volume;
            volume_sum += h.volume;
        }
        let pressure_ratio = if volume_sum > 0.0 {
            signed_sum / volume_sum
        } else {
            f64::INFINITY
        };

        let return_in_range = previous_close.is_some_and(|prev| {
            let prev = prev.to_f64();
            prev > 0.0 && ((close / prev) - 1.0).abs() <= MAX_SIGNAL_RETURN
        });
        let breakout = prior_high.is_some_and(|h| bar.close > h);
        let enough_history = self.history.len() >= LOOKBACK - 1;

        // Keep a rolling window after evaluating the current bar against prior bars.
        self.history.push_back(current);
        while self.history.len() > LOOKBACK {
            self.history.pop_front();
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if ctx.open_orders() > 0 {
                return;
            }
            let Some(entry) = self.entry.as_ref() else {
                return;
            };
            let close_raw = i128::from(bar.close.raw());
            let entry_raw = i128::from(entry.price.raw());
            let stop_hit = close_raw * 1_000 <= entry_raw * 992;
            let target_hit = close_raw * 1_000 >= entry_raw * 1_012;
            let time_hit = bar.close_ts.as_nanos().saturating_sub(entry.ts_nanos)
                >= TimeDelta::from_hours(4).as_nanos();
            if stop_hit || target_hit || time_hit {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_zero()
            || ctx.open_orders() > 0
            || bar.close_ts.as_nanos() < self.cooldown_until_nanos
            || !enough_history
            || pressure_ratio > MAX_SELL_PRESSURE_RATIO
            || !return_in_range
            || !breakout
        {
            return;
        }

        // Estimate the buy price with the requested 2 bp adverse slippage for sizing.
        // The market order itself is executed by the backtest harness.
        let sizing_price = bar.close.mul_ratio(10_002, 10_000);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            sizing_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        // The next bar's volume is not available yet. Use the signal bar's known volume
        // as the causal participation-cap estimate.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if affordable.raw() < volume_cap.raw() {
            affordable
        } else {
            volume_cap
        };
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.entry = Some(match self.entry.take() {
                Some(mut entry) => {
                    let old_qty = i128::from(entry.qty.raw());
                    let added_qty = i128::from(fill.qty.raw());
                    let total_qty = old_qty + added_qty;
                    if total_qty > 0 {
                        let weighted_price = (i128::from(entry.price.raw()) * old_qty
                            + i128::from(fill.price.raw()) * added_qty)
                            / total_qty;
                        entry.price = Price::from_raw(weighted_price as i64);
                        entry.qty = Qty::from_raw(total_qty as i64);
                    }
                    entry
                }
                None => Entry {
                    price: fill.price,
                    ts_nanos: fill.ts.as_nanos(),
                    qty: fill.qty,
                },
            });
        } else if fill.side == Side::Sell {
            let fully_exited = if let Some(entry) = self.entry.as_mut() {
                entry.qty = Qty::from_raw(
                    entry.qty.raw().saturating_sub(fill.qty.raw()).max(0),
                );
                entry.qty.is_zero()
            } else {
                false
            };
            if fully_exited {
                self.entry = None;
                self.cooldown_until_nanos = fill
                    .ts
                    .as_nanos()
                    .saturating_add(TimeDelta::from_hours(6).as_nanos());
            }
        }
    }
}

export_strategy!(QuietSellPressureAbsorption {
    history: VecDeque::with_capacity(LOOKBACK),
    entry: None,
    cooldown_until_nanos: 0,
});
