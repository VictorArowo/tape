use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const BREAKOUT_HOURS: usize = 24;
const COMPRESSION_HOURS: usize = 12;
const LOOKBACK_HOURS: usize = 14 * 24;
const HISTORY_CAP: usize = LOOKBACK_HOURS + 24;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct HourBar {
    hour_id: i64,
    high: Price,
    low: Price,
    close: Price,
}

struct HourAccumulator {
    hour_id: i64,
    high: Price,
    low: Price,
    close: Price,
}

impl HourAccumulator {
    fn new(hour_id: i64, bar: &Bar) -> Self {
        Self {
            hour_id,
            high: bar.high,
            low: bar.low,
            close: bar.close,
        }
    }

    fn add(&mut self, bar: &Bar) {
        if bar.high > self.high {
            self.high = bar.high;
        }
        if bar.low < self.low {
            self.low = bar.low;
        }
        self.close = bar.close;
    }

    fn finish(self) -> HourBar {
        HourBar {
            hour_id: self.hour_id,
            high: self.high,
            low: self.low,
            close: self.close,
        }
    }
}

struct CompressionRangeBreakout {
    current_hour: Option<HourAccumulator>,
    history: VecDeque<HourBar>,
}

impl CompressionRangeBreakout {
    fn completed_hour(&mut self, ctx: &mut Ctx, hour: HourBar) {
        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if let Some((_, prior_low)) = trailing_extremes(
                    &self.history,
                    COMPRESSION_HOURS,
                    hour.hour_id - 1,
                ) {
                    if hour.close < prior_low {
                        ctx.submit(OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            position,
                        ));
                    }
                }
            } else if position.is_zero() {
                self.try_entry(ctx, hour);
            }
        }

        self.history.push_back(hour);
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    fn try_entry(&self, ctx: &mut Ctx, hour: HourBar) {
        let Some((prior_high, _)) = trailing_extremes(
            &self.history,
            BREAKOUT_HOURS,
            hour.hour_id - 1,
        ) else {
            return;
        };
        if hour.close <= prior_high {
            return;
        }

        let Some((compression_high, compression_low)) = trailing_extremes(
            &self.history,
            COMPRESSION_HOURS,
            hour.hour_id - 1,
        ) else {
            return;
        };
        let Some(current_range) = percentage_range(compression_high, compression_low) else {
            return;
        };
        let Some(threshold) = historical_range_percentile(&self.history, hour.hour_id) else {
            return;
        };
        if current_range > threshold {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            hour.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

fn trailing_extremes(
    history: &VecDeque<HourBar>,
    count: usize,
    expected_last_id: i64,
) -> Option<(Price, Price)> {
    if history.len() < count {
        return None;
    }
    let start = history.len() - count;
    let expected_first_id = expected_last_id - (count as i64) + 1;
    let first = *history.get(start)?;
    if first.hour_id != expected_first_id {
        return None;
    }

    let mut high = first.high;
    let mut low = first.low;
    for offset in 0..count {
        let item = *history.get(start + offset)?;
        if item.hour_id != expected_first_id + offset as i64 {
            return None;
        }
        if item.high > high {
            high = item.high;
        }
        if item.low < low {
            low = item.low;
        }
    }
    Some((high, low))
}

fn percentage_range(high: Price, low: Price) -> Option<f64> {
    if !low.is_positive() {
        return None;
    }
    let value = (high.to_f64() - low.to_f64()) / low.to_f64() * 100.0;
    value.is_finite().then_some(value)
}

fn range_ending_at(history: &VecDeque<HourBar>, end_index: usize) -> Option<f64> {
    if end_index + 1 < COMPRESSION_HOURS {
        return None;
    }
    let start = end_index + 1 - COMPRESSION_HOURS;
    let first = *history.get(start)?;
    let mut high = first.high;
    let mut low = first.low;
    for offset in 0..COMPRESSION_HOURS {
        let item = *history.get(start + offset)?;
        if item.hour_id != first.hour_id + offset as i64 {
            return None;
        }
        if item.high > high {
            high = item.high;
        }
        if item.low < low {
            low = item.low;
        }
    }
    percentage_range(high, low)
}

fn historical_range_percentile(history: &VecDeque<HourBar>, current_hour_id: i64) -> Option<f64> {
    let first_id = current_hour_id - LOOKBACK_HOURS as i64;
    let mut expected_id = first_id;
    let mut ranges = Vec::with_capacity(LOOKBACK_HOURS);

    for (index, item) in history.iter().enumerate() {
        if item.hour_id < first_id || item.hour_id >= current_hour_id {
            continue;
        }
        if item.hour_id != expected_id {
            return None;
        }
        ranges.push(range_ending_at(history, index)?);
        expected_id += 1;
    }
    if expected_id != current_hour_id || ranges.len() != LOOKBACK_HOURS {
        return None;
    }

    ranges.sort_by(f64::total_cmp);
    let position = 0.20 * (ranges.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let fraction = position - lower as f64;
    Some(ranges[lower] + (ranges[upper] - ranges[lower]) * fraction)
}

impl Strategy for CompressionRangeBreakout {
    fn name(&self) -> String {
        "compression-range-breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let hour_id = bar.open_ts.as_nanos().div_euclid(hour_ns);
        if self
            .current_hour
            .as_ref()
            .is_some_and(|current| current.hour_id != hour_id)
        {
            self.current_hour = None;
        }

        match self.current_hour.as_mut() {
            Some(current) => current.add(bar),
            None => self.current_hour = Some(HourAccumulator::new(hour_id, bar)),
        }

        let hour_end = hour_id.saturating_add(1).saturating_mul(hour_ns);
        if bar.close_ts.as_nanos() >= hour_end {
            if let Some(current) = self.current_hour.take() {
                self.completed_hour(ctx, current.finish());
            }
        }
    }
}

export_strategy!(CompressionRangeBreakout {
    current_hour: None,
    history: VecDeque::with_capacity(HISTORY_CAP),
});
