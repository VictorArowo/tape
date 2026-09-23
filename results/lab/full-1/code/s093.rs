use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    Timestamp, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_HOURS: usize = 24;
const PERCENTILE_HOURS: usize = 336;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HourBar {
    bucket: i64,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    volume: Qty,
    close_ts: Timestamp,
}

impl HourBar {
    fn from_minute(bar: &Bar, bucket: i64) -> Self {
        Self {
            bucket,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
            close_ts: bar.close_ts,
        }
    }

    fn add_minute(&mut self, bar: &Bar) {
        if bar.high > self.high {
            self.high = bar.high;
        }
        if bar.low < self.low {
            self.low = bar.low;
        }
        self.close = bar.close;
        self.volume = self.volume + bar.volume;
        self.close_ts = bar.close_ts;
    }
}

struct SqueezeExpansionBreakout {
    current_hour: Option<HourBar>,
    prior_hours: VecDeque<HourBar>,
    prior_ratios: VecDeque<f64>,
    pending_entry_level: Option<Price>,
    breakout_level: Option<Price>,
    entry_ts_nanos: Option<i64>,
}

impl SqueezeExpansionBreakout {
    fn new() -> Self {
        Self {
            current_hour: None,
            prior_hours: VecDeque::with_capacity(LOOKBACK_HOURS + 1),
            prior_ratios: VecDeque::with_capacity(PERCENTILE_HOURS + 1),
            pending_entry_level: None,
            breakout_level: None,
            entry_ts_nanos: None,
        }
    }

    fn prior_24h_stats(&self) -> Option<(f64, Price)> {
        if self.prior_hours.len() < LOOKBACK_HOURS {
            return None;
        }

        let start = self.prior_hours.len() - LOOKBACK_HOURS;
        let mut highest = self.prior_hours[start].high;
        let mut lowest = self.prior_hours[start].low;
        for hour in self.prior_hours.iter().skip(start + 1) {
            if hour.high > highest {
                highest = hour.high;
            }
            if hour.low < lowest {
                lowest = hour.low;
            }
        }

        let last_close = self.prior_hours.back()?.close;
        if last_close.raw() <= 0 {
            return None;
        }
        let range = i128::from(highest.raw()) - i128::from(lowest.raw());
        let ratio = range as f64 / last_close.raw() as f64;
        Some((ratio, highest))
    }

    fn percentile_20(&self) -> Option<f64> {
        if self.prior_ratios.len() < PERCENTILE_HOURS {
            return None;
        }
        let mut sorted: Vec<f64> = self.prior_ratios.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);

        // Linear interpolation between adjacent sorted observations.
        let rank = (sorted.len() - 1) as f64 * 0.20;
        let lower = rank.floor() as usize;
        let upper = rank.ceil() as usize;
        let weight = rank - lower as f64;
        Some(sorted[lower] * (1.0 - weight) + sorted[upper] * weight)
    }

    fn on_completed_hour(&mut self, ctx: &mut Ctx, hour: HourBar) {
        let stats = self.prior_24h_stats();
        let threshold = self.percentile_20();
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            let failed = self
                .breakout_level
                .is_some_and(|level| hour.close <= level);
            let timed_out = self.entry_ts_nanos.is_some_and(|entry_ns| {
                hour.close_ts
                    .as_nanos()
                    .saturating_sub(entry_ns)
                    >= TimeDelta::from_hours(72).as_nanos()
            });

            if (failed || timed_out) && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if position.is_zero() && ctx.open_orders() == 0 {
            if let (Some((range_ratio, prior_high)), Some(percentile)) = (stats, threshold) {
                let qualifies = range_ratio <= percentile && hour.close > prior_high;
                if qualifies {
                    // Pad the sizing reference for the specified buy-side slippage.
                    // The execution environment applies slippage and fees to fills.
                    let sizing_price = hour.close.mul_ratio(10_002, 10_000);
                    let affordable = ctx.affordable_qty(
                        INSTRUMENT,
                        sizing_price,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    let volume_cap = hour.volume.mul_ratio(1, 10);
                    let qty = if affordable < volume_cap {
                        affordable
                    } else {
                        volume_cap
                    };

                    if qty.is_positive() {
                        self.pending_entry_level = Some(prior_high);
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }

        // Store this hour's trailing range ratio only after evaluating it, so the
        // percentile contains the 336 completed observations preceding this hour.
        if let Some((ratio, _)) = stats {
            self.prior_ratios.push_back(ratio);
            if self.prior_ratios.len() > PERCENTILE_HOURS {
                self.prior_ratios.pop_front();
            }
        }

        self.prior_hours.push_back(hour);
        if self.prior_hours.len() > LOOKBACK_HOURS {
            self.prior_hours.pop_front();
        }
    }
}

impl Strategy for SqueezeExpansionBreakout {
    fn name(&self) -> String {
        "squeeze-expansion-breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.pending_entry_level.is_some() && ctx.open_orders() == 0 {
            if ctx.position(INSTRUMENT).is_zero() {
                self.pending_entry_level = None;
            } else {
                // The fill callback has recorded the entry state; no pending order remains.
                self.pending_entry_level = None;
            }
        }

        let hour_nanos = TimeDelta::from_hours(1).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_nanos);

        if self
            .current_hour
            .as_ref()
            .is_some_and(|current| current.bucket != bucket)
        {
            if let Some(completed) = self.current_hour.take() {
                self.on_completed_hour(ctx, completed);
            }
        }

        if let Some(current) = self.current_hour.as_mut() {
            current.add_minute(bar);
        } else {
            self.current_hour = Some(HourBar::from_minute(bar, bucket));
        }

        // With bars grouped by their open time, the bar closing on the hour
        // boundary completes the hourly aggregate immediately.
        if bar.close_ts.as_nanos().rem_euclid(hour_nanos) == 0 {
            if let Some(completed) = self.current_hour.take() {
                self.on_completed_hour(ctx, completed);
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts_nanos.is_none() {
                    self.entry_ts_nanos = Some(fill.ts.as_nanos());
                    self.breakout_level = self.pending_entry_level;
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts_nanos = None;
                    self.breakout_level = None;
                    self.pending_entry_level = None;
                }
            }
        }
    }
}

export_strategy!(SqueezeExpansionBreakout::new());
