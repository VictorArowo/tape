use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct AdaptiveTurnoverGate {
    // One history per UTC hour of day; each stores the latest 30 completed
    // hourly notional-volume observations.
    history: [VecDeque<i128>; 24],
    active_hour_key: Option<i64>,
    active_notional: i128,
}

impl AdaptiveTurnoverGate {
    fn new() -> Self {
        Self {
            history: std::array::from_fn(|_| VecDeque::with_capacity(LOOKBACK)),
            active_hour_key: None,
            active_notional: 0,
        }
    }

    // Compare against a linearly interpolated percentile of the prior values.
    fn below_percentile(history: &VecDeque<i128>, value: i128, percentile: usize) -> bool {
        let mut sorted: Vec<i128> = history.iter().copied().collect();
        sorted.sort_unstable();

        let scaled_index = percentile * (sorted.len() - 1);
        let lower = scaled_index / 100;
        let remainder = scaled_index % 100;
        let upper = (lower + 1).min(sorted.len() - 1);
        let threshold_numerator = sorted[lower] * (100 - remainder) as i128
            + sorted[upper] * remainder as i128;
        value * 100 < threshold_numerator
    }

    fn above_percentile(history: &VecDeque<i128>, value: i128, percentile: usize) -> bool {
        let mut sorted: Vec<i128> = history.iter().copied().collect();
        sorted.sort_unstable();

        let scaled_index = percentile * (sorted.len() - 1);
        let lower = scaled_index / 100;
        let remainder = scaled_index % 100;
        let upper = (lower + 1).min(sorted.len() - 1);
        let threshold_numerator = sorted[lower] * (100 - remainder) as i128
            + sorted[upper] * remainder as i128;
        value * 100 > threshold_numerator
    }

    fn finish_hour(&mut self, ctx: &mut Ctx, hour_of_day: usize, value: i128, price: Price) {
        let history = &self.history[hour_of_day];
        if history.len() == LOOKBACK && ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if Self::below_percentile(history, value, 20) {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            } else if position.is_zero() && Self::above_percentile(history, value, 60) {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    price,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        let history = &mut self.history[hour_of_day];
        history.push_back(value);
        if history.len() > LOOKBACK {
            history.pop_front();
        }
    }
}

impl Strategy for AdaptiveTurnoverGate {
    fn name(&self) -> String {
        "adaptive-turnover-gate(30,20/60) [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let hour_key = bar.open_ts.as_nanos().div_euclid(hour_ns);

        // If data has a gap across an hour boundary, close out the prior bucket
        // when the first bar of the new bucket arrives.
        if let Some(active_key) = self.active_hour_key {
            if active_key != hour_key {
                let hour_of_day = active_key.rem_euclid(24) as usize;
                self.finish_hour(ctx, hour_of_day, self.active_notional, bar.close);
                self.active_notional = 0;
                self.active_hour_key = None;
            }
        }

        if self.active_hour_key.is_none() {
            self.active_hour_key = Some(hour_key);
        }

        let bar_notional = i128::from((bar.close * bar.volume).raw());
        self.active_notional += bar_notional;

        // The bar whose close is on the hour completes the current UTC-hour bucket.
        if bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0 {
            let hour_of_day = hour_key.rem_euclid(24) as usize;
            let completed_notional = self.active_notional;
            self.finish_hour(ctx, hour_of_day, completed_notional, bar.close);
            self.active_notional = 0;
            self.active_hour_key = None;
        }
    }
}

export_strategy!(AdaptiveTurnoverGate::new());
