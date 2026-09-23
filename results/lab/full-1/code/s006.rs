use std::collections::VecDeque;

use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FAST_LEN: usize = 7 * 24 * 60;
const SLOW_LEN: usize = 30 * 24 * 60;
const VWAP_LEN: usize = 24 * 60;
const PRIOR_HOURS: usize = 24;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Sma {
    len: usize,
    window: VecDeque<i64>,
    sum: i128,
}

impl Sma {
    fn new(len: usize) -> Self {
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

    fn ready_sum(&self) -> Option<i128> {
        (self.window.len() == self.len).then_some(self.sum)
    }
}

struct RollingVwap {
    window: VecDeque<(f64, f64)>,
    weighted_sum: f64,
    volume_sum: f64,
}

impl RollingVwap {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(VWAP_LEN + 1),
            weighted_sum: 0.0,
            volume_sum: 0.0,
        }
    }

    fn push(&mut self, bar: &Bar) {
        let typical = (bar.high.to_f64() + bar.low.to_f64() + bar.close.to_f64()) / 3.0;
        let volume = bar.volume.to_f64();
        self.window.push_back((typical * volume, volume));
        self.weighted_sum += typical * volume;
        self.volume_sum += volume;

        if self.window.len() > VWAP_LEN {
            let (weighted, old_volume) = self.window.pop_front().expect("non-empty");
            self.weighted_sum -= weighted;
            self.volume_sum -= old_volume;
        }
    }

    fn ready_value(&self) -> Option<f64> {
        if self.window.len() == VWAP_LEN && self.volume_sum > 0.0 {
            Some(self.weighted_sum / self.volume_sum)
        } else {
            None
        }
    }
}

struct TrendGatedFlushReversion {
    fast: Sma,
    slow: Sma,
    closes: VecDeque<i64>,
    vwap: RollingVwap,
    completed_hour_volumes: VecDeque<i128>,
    current_hour_volume: i128,
    current_hour_bars: usize,
    pending_entry_target: Option<Price>,
    target_vwap: Option<Price>,
    entry_price: Option<Price>,
    entry_ts: Option<tape_guest::Timestamp>,
    exit_pending: bool,
}

impl TrendGatedFlushReversion {
    fn clear_position_state(&mut self) {
        self.target_vwap = None;
        self.entry_price = None;
        self.entry_ts = None;
        self.exit_pending = false;
    }

    fn median_pair_sum(&self) -> Option<i128> {
        if self.completed_hour_volumes.len() != PRIOR_HOURS {
            return None;
        }
        let mut sorted: Vec<i128> = self.completed_hour_volumes.iter().copied().collect();
        sorted.sort_unstable();
        Some(sorted[11] + sorted[12])
    }
}

impl Strategy for TrendGatedFlushReversion {
    fn name(&self) -> String {
        "trend-gated-flush-reversion".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A market order that did not fill leaves no position or open order.
        if self.pending_entry_target.is_some()
            && ctx.open_orders() == 0
            && ctx.position(INSTRUMENT).is_zero()
        {
            self.pending_entry_target = None;
        }

        self.fast.push(bar.close);
        self.slow.push(bar.close);
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > SLOW_LEN {
            self.closes.pop_front();
        }
        self.vwap.push(bar);

        self.current_hour_volume += i128::from(bar.volume.raw());
        self.current_hour_bars += 1;

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos() / minute_ns;
        let is_hour_close = minute_index.rem_euclid(60) == 0;

        let mut completed_hour = None;
        if is_hour_close {
            if self.current_hour_bars == 60 {
                // Save the preceding-hours median before adding this completed hour.
                let prior_median_pair_sum = self.median_pair_sum();
                completed_hour = Some((self.current_hour_volume, prior_median_pair_sum));
                if self.completed_hour_volumes.len() == PRIOR_HOURS {
                    self.completed_hour_volumes.pop_front();
                }
                self.completed_hour_volumes
                    .push_back(self.current_hour_volume);
            } else {
                // An incomplete hour cannot be part of the preceding-hours median.
                self.completed_hour_volumes.clear();
            }
            self.current_hour_volume = 0;
            self.current_hour_bars = 0;
        }

        let position = ctx.position(INSTRUMENT);
        if !position.is_zero() {
            if let Some(entry_ts) = self.entry_ts {
                let elapsed = bar
                    .close_ts
                    .as_nanos()
                    .saturating_sub(entry_ts.as_nanos());
                if elapsed >= TimeDelta::from_hours(24).as_nanos() {
                    self.exit_pending = true;
                }
            }

            if is_hour_close {
                if let Some(target) = self.target_vwap {
                    if bar.close >= target {
                        self.exit_pending = true;
                    }
                }
                if let Some(entry_price) = self.entry_price {
                    if bar.close <= entry_price.mul_ratio(97, 100) {
                        self.exit_pending = true;
                    }
                }
            }
        } else if self.exit_pending {
            self.clear_position_state();
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if self.exit_pending {
            let held = ctx.position(INSTRUMENT);
            if held.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, held));
            }
            return;
        }

        // Entry signals are evaluated only on completed UTC-hour closes.
        let Some((hour_volume, Some(median_pair_sum))) = completed_hour else {
            return;
        };
        if !ctx.position(INSTRUMENT).is_zero() || self.pending_entry_target.is_some() {
            return;
        }

        let (Some(fast_sum), Some(slow_sum), Some(vwap_value)) = (
            self.fast.ready_sum(),
            self.slow.ready_sum(),
            self.vwap.ready_value(),
        ) else {
            return;
        };
        if self.closes.len() < 61 {
            return;
        }

        let trend_up = fast_sum * SLOW_LEN as i128 > slow_sum * FAST_LEN as i128;
        let vwap_discount = bar.close.to_f64() <= vwap_value * 0.985;
        let close_60_bars_ago = self.closes[self.closes.len() - 61];
        let sharp_decline = i128::from(bar.close.raw()) * 100
            <= i128::from(close_60_bars_ago) * 99;
        let unusually_heavy_volume = hour_volume * 4 > median_pair_sum * 3;

        if !(trend_up && vwap_discount && sharp_decline && unusually_heavy_volume) {
            return;
        }

        let target_raw = (vwap_value * 100_000_000.0).round() as i64;
        let target = Price::from_raw(target_raw);
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            self.pending_entry_target = Some(target);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.entry_price.is_none() {
                self.entry_price = Some(fill.price);
                self.entry_ts = Some(fill.ts);
                self.target_vwap = self.pending_entry_target.take();
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.pending_entry_target = None;
            self.clear_position_state();
        }
    }
}

export_strategy!(TrendGatedFlushReversion {
    fast: Sma::new(FAST_LEN),
    slow: Sma::new(SLOW_LEN),
    closes: VecDeque::with_capacity(SLOW_LEN + 1),
    vwap: RollingVwap::new(),
    completed_hour_volumes: VecDeque::with_capacity(PRIOR_HOURS + 1),
    current_hour_volume: 0,
    current_hour_bars: 0,
    pending_entry_target: None,
    target_vwap: None,
    entry_price: None,
    entry_ts: None,
    exit_pending: false,
});