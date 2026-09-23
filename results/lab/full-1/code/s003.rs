use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const BREAKOUT_LOOKBACK: usize = 12;
const ATR_PERIOD: usize = 14;

fn four_hour_nanos() -> i64 {
    TimeDelta::from_mins(1).as_nanos() * 240
}

#[derive(Clone, Copy)]
struct Candle {
    bucket: i64,
    high: i64,
    low: i64,
    close: i64,
}

impl Candle {
    fn from_bar(bucket: i64, bar: &Bar) -> Self {
        Self {
            bucket,
            high: bar.high.raw(),
            low: bar.low.raw(),
            close: bar.close.raw(),
        }
    }

    fn include(&mut self, bar: &Bar) {
        self.high = self.high.max(bar.high.raw());
        self.low = self.low.min(bar.low.raw());
        self.close = bar.close.raw();
    }
}

struct FilteredBreakout {
    building: Option<Candle>,
    prior_highs: VecDeque<i64>,
    ema_fast: Option<f64>,
    ema_slow: Option<f64>,
    previous_close: Option<f64>,
    true_ranges: VecDeque<f64>,
    true_range_sum: f64,
    highest_close_since_entry: Option<f64>,
}

impl FilteredBreakout {
    fn new() -> Self {
        Self {
            building: None,
            prior_highs: VecDeque::with_capacity(BREAKOUT_LOOKBACK + 1),
            ema_fast: None,
            ema_slow: None,
            previous_close: None,
            true_ranges: VecDeque::with_capacity(ATR_PERIOD + 1),
            true_range_sum: 0.0,
            highest_close_since_entry: None,
        }
    }

    fn update_ema(previous: Option<f64>, value: f64, period: f64) -> f64 {
        let alpha = 2.0 / (period + 1.0);
        match previous {
            Some(old) => alpha * value + (1.0 - alpha) * old,
            None => value,
        }
    }

    fn update_indicators(&mut self, candle: &Candle) {
        let close = candle.close as f64;
        self.ema_fast = Some(Self::update_ema(self.ema_fast, close, 6.0));
        self.ema_slow = Some(Self::update_ema(self.ema_slow, close, 18.0));

        let high = candle.high as f64;
        let low = candle.low as f64;
        let true_range = match self.previous_close {
            Some(previous) => (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
            None => high - low,
        };
        self.true_ranges.push_back(true_range);
        self.true_range_sum += true_range;
        if self.true_ranges.len() > ATR_PERIOD {
            self.true_range_sum -= self.true_ranges.pop_front().expect("non-empty ATR window");
        }
        self.previous_close = Some(close);
    }

    fn on_completed_candle(&mut self, ctx: &mut Ctx, candle: Candle) {
        let breakout_high = if self.prior_highs.len() == BREAKOUT_LOOKBACK {
            self.prior_highs.iter().copied().max()
        } else {
            None
        };

        self.update_indicators(&candle);
        let close = candle.close as f64;
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            let peak = self
                .highest_close_since_entry
                .map_or(close, |previous| previous.max(close));
            self.highest_close_since_entry = Some(peak);

            let ema_exit = self.ema_fast.is_some_and(|ema| close < ema);
            let atr = (self.true_ranges.len() == ATR_PERIOD)
                .then_some(self.true_range_sum / ATR_PERIOD as f64);
            let trailing_exit = atr.is_some_and(|value| close <= peak - 2.5 * value);

            if ctx.open_orders() == 0 && (ema_exit || trailing_exit) {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else {
            self.highest_close_since_entry = None;
            let trend_filter = matches!((self.ema_fast, self.ema_slow), (Some(fast), Some(slow)) if fast > slow);
            let breakout = breakout_high.is_some_and(|high| candle.close > high);

            if ctx.open_orders() == 0 && breakout && trend_filter {
                // The completed candle's close is the available estimate of the next-open fill price.
                let budget = ctx.cash().mul_ratio(95, 100);
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    tape_guest::Price::from_raw(candle.close),
                    budget,
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
        }

        self.prior_highs.push_back(candle.high);
        if self.prior_highs.len() > BREAKOUT_LOOKBACK {
            self.prior_highs.pop_front();
        }
    }
}

impl Strategy for FilteredBreakout {
    fn name(&self) -> String {
        "Filtered 48-Hour Breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let period = four_hour_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(period);
        let same_bucket = self
            .building
            .as_ref()
            .is_some_and(|candle| candle.bucket == bucket);

        if same_bucket {
            self.building
                .as_mut()
                .expect("candle exists")
                .include(bar);
        } else {
            // A bucket change without an observed closing boundary leaves the prior candle incomplete.
            self.building = Some(Candle::from_bar(bucket, bar));
        }

        let close_bucket = bar.close_ts.as_nanos().div_euclid(period);
        let completed = self
            .building
            .as_ref()
            .is_some_and(|candle| close_bucket > candle.bucket);
        if completed {
            let candle = self.building.take().expect("completed candle exists");
            self.on_completed_candle(ctx, candle);
        }
    }
}

export_strategy!(FilteredBreakout::new());
