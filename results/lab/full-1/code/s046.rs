use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ATR_LEN: usize = 360;
const RANGE_LEN: usize = 720;
const ATR_HISTORY_LEN: usize = 56;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct QuietRegimeBreakout {
    true_ranges: VecDeque<f64>,
    highs: VecDeque<i64>,
    previous_close: Option<i64>,
    atr_history: VecDeque<f64>,
    baseline_atr: Option<f64>,
    previous_eval_below_range: Option<bool>,
    pending_entry_atr: Option<f64>,
    entry_atr: Option<f64>,
    entry_ts_ns: Option<i64>,
    highest_high_since_entry: Option<i64>,
}

impl QuietRegimeBreakout {
    fn new() -> Self {
        Self {
            true_ranges: VecDeque::with_capacity(ATR_LEN + 1),
            highs: VecDeque::with_capacity(RANGE_LEN + 1),
            previous_close: None,
            atr_history: VecDeque::with_capacity(ATR_HISTORY_LEN + 1),
            baseline_atr: None,
            previous_eval_below_range: None,
            pending_entry_atr: None,
            entry_atr: None,
            entry_ts_ns: None,
            highest_high_since_entry: None,
        }
    }

    fn atr6(&self) -> Option<f64> {
        if self.true_ranges.len() != ATR_LEN {
            return None;
        }
        Some(self.true_ranges.iter().sum::<f64>() / ATR_LEN as f64)
    }

    fn historical_median(&self) -> Option<f64> {
        if self.atr_history.len() < ATR_HISTORY_LEN {
            return None;
        }
        let mut values: Vec<f64> = self.atr_history.iter().copied().collect();
        values.sort_by(f64::total_cmp);
        let mid = values.len() / 2;
        if values.len() % 2 == 0 {
            Some((values[mid - 1] + values[mid]) / 2.0)
        } else {
            Some(values[mid])
        }
    }
}

impl Strategy for QuietRegimeBreakout {
    fn name(&self) -> String {
        "quiet-regime-breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && ctx.open_orders() == 0 {
            self.pending_entry_atr = None;
        }

        // This window contains only bars preceding the latest completed bar.
        let preceding_range_high = if self.highs.len() == RANGE_LEN {
            self.highs.iter().copied().max()
        } else {
            None
        };

        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let true_range = if let Some(previous_close) = self.previous_close {
            let previous_close = PriceValue(previous_close).to_f64();
            (high - low)
                .max((high - previous_close).abs())
                .max((low - previous_close).abs())
        } else {
            high - low
        };
        self.true_ranges.push_back(true_range);
        if self.true_ranges.len() > ATR_LEN {
            self.true_ranges.pop_front();
        }
        self.previous_close = Some(bar.close.raw());

        self.highs.push_back(bar.high.raw());
        if self.highs.len() > RANGE_LEN {
            self.highs.pop_front();
        }

        let atr = self.atr6();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos() / minute_ns;
        let minute_of_day = minute_index.rem_euclid(1_440);

        // Update the baseline from prior completed 6-hour samples, then store this sample.
        if minute_of_day.rem_euclid(360) == 0 {
            if let Some(current_atr) = atr {
                if let Some(median) = self.historical_median() {
                    self.baseline_atr = Some(median);
                }
                self.atr_history.push_back(current_atr);
                if self.atr_history.len() > ATR_HISTORY_LEN {
                    self.atr_history.pop_front();
                }
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if self.entry_atr.is_none() {
                self.entry_atr = self.pending_entry_atr.take();
            }
            self.highest_high_since_entry = Some(
                self.highest_high_since_entry
                    .map_or(bar.high.raw(), |old| old.max(bar.high.raw())),
            );
        } else if self.pending_entry_atr.is_none() {
            self.entry_atr = None;
            self.entry_ts_ns = None;
            self.highest_high_since_entry = None;
        }

        if minute_index.rem_euclid(15) != 0 {
            return;
        }

        let current_below_range = preceding_range_high
            .map(|range_high| bar.close.raw() <= range_high)
            .unwrap_or(false);
        let previous_eval_below_range = self.previous_eval_below_range;
        self.previous_eval_below_range = Some(current_below_range);

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            let trailing_exit = match (self.highest_high_since_entry, self.entry_atr) {
                (Some(highest_high), Some(entry_atr)) => {
                    bar.close.to_f64() <= PriceValue(highest_high).to_f64() - 2.5 * entry_atr
                }
                _ => false,
            };
            let time_exit = self
                .entry_ts_ns
                .map(|entry_ts| {
                    bar.close_ts.as_nanos().saturating_sub(entry_ts)
                        >= TimeDelta::from_hours(72).as_nanos()
                })
                .unwrap_or(false);

            if trailing_exit || time_exit {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        let (Some(current_atr), Some(baseline), Some(range_high)) =
            (atr, self.baseline_atr, preceding_range_high)
        else {
            return;
        };
        let breakout = bar.close.raw() > range_high;
        let crossed_since_previous_eval = previous_eval_below_range == Some(true);
        if current_atr > 0.80 * baseline || !breakout || !crossed_since_previous_eval {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let volume_limit = bar.volume.mul_ratio(1, 10);
        let qty = if volume_limit.raw() < affordable.raw() {
            volume_limit
        } else {
            affordable
        };
        if qty.is_positive() {
            self.pending_entry_atr = Some(current_atr);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_atr.is_none() {
                    self.entry_atr = self.pending_entry_atr.take();
                    self.entry_ts_ns = Some(fill.ts.as_nanos());
                    self.highest_high_since_entry = Some(fill.price.raw());
                } else {
                    self.highest_high_since_entry = Some(
                        self.highest_high_since_entry
                            .map_or(fill.price.raw(), |old| old.max(fill.price.raw())),
                    );
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_atr = None;
                    self.entry_ts_ns = None;
                    self.highest_high_since_entry = None;
                    self.pending_entry_atr = None;
                }
            }
        }
    }
}

struct PriceValue(i64);

impl PriceValue {
    fn to_f64(self) -> f64 {
        self.0 as f64 / 100_000_000.0
    }
}

export_strategy!(QuietRegimeBreakout::new());
