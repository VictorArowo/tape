use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    Timestamp, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const PRICE_SCALE: f64 = 100_000_000.0;
const PRECEDING_HOURS: usize = 24;

#[derive(Clone, Copy)]
struct HourBar {
    high: Price,
    close: Price,
}

struct HourBuilder {
    bucket: Option<i64>,
    high: Price,
    close: Price,
}

impl HourBuilder {
    fn new() -> Self {
        Self {
            bucket: None,
            high: Price::from_raw(0),
            close: Price::from_raw(0),
        }
    }

    fn finish(&mut self) -> Option<HourBar> {
        self.bucket.take().map(|_| HourBar {
            high: self.high,
            close: self.close,
        })
    }

    fn push(&mut self, bar: &Bar) -> Option<HourBar> {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let mut completed = None;

        if self.bucket != Some(bucket) {
            completed = self.finish();
            self.bucket = Some(bucket);
            self.high = bar.high;
            self.close = bar.close;
        } else {
            if bar.high > self.high {
                self.high = bar.high;
            }
            self.close = bar.close;
        }

        let hour_end = (i128::from(bucket) + 1) * i128::from(hour_ns);
        if i128::from(bar.close_ts.as_nanos()) >= hour_end {
            let just_finished = self.finish();
            if completed.is_none() {
                completed = just_finished;
            }
        }

        completed
    }
}

struct HourlySignals {
    ema12: Option<f64>,
    ema48: Option<f64>,
    ema200: Option<f64>,
    previous_close: Option<f64>,
    previous_ema12: Option<f64>,
    ema200_history: VecDeque<f64>,
    preceding_highs: VecDeque<f64>,
}

impl HourlySignals {
    fn new() -> Self {
        Self {
            ema12: None,
            ema48: None,
            ema200: None,
            previous_close: None,
            previous_ema12: None,
            ema200_history: VecDeque::with_capacity(PRECEDING_HOURS + 1),
            preceding_highs: VecDeque::with_capacity(PRECEDING_HOURS + 1),
        }
    }

    fn update_ema(previous: Option<f64>, price: f64, period: usize) -> f64 {
        let alpha = 2.0 / (period as f64 + 1.0);
        match previous {
            Some(value) => value + alpha * (price - value),
            None => price,
        }
    }

    fn on_hour(&mut self, bar: HourBar) -> (bool, bool) {
        let close = bar.close.raw() as f64 / PRICE_SCALE;
        let high = bar.high.raw() as f64 / PRICE_SCALE;

        let prior_ema200 = if self.ema200_history.len() >= PRECEDING_HOURS {
            self.ema200_history
                .get(self.ema200_history.len() - PRECEDING_HOURS)
                .copied()
        } else {
            None
        };
        let prior_highest = if self.preceding_highs.len() >= PRECEDING_HOURS {
            self.preceding_highs.iter().copied().reduce(f64::max)
        } else {
            None
        };

        let ema12 = Self::update_ema(self.ema12, close, 12);
        let ema48 = Self::update_ema(self.ema48, close, 48);
        let ema200 = Self::update_ema(self.ema200, close, 200);

        let crossed_up = matches!(
            (self.previous_close, self.previous_ema12),
            (Some(previous_close), Some(previous_ema12))
                if previous_close <= previous_ema12 && close > ema12
        );

        let exit_below_ema48 = close < ema48;
        let entry_signal = match (prior_ema200, prior_highest) {
            (Some(old_ema200), Some(highest)) if highest > 0.0 => {
                let pullback = (highest - close) / highest;
                ema48 > ema200
                    && ema200 > old_ema200
                    && crossed_up
                    && (0.015..=0.04).contains(&pullback)
                    && close > ema200
            }
            _ => false,
        };

        self.ema200_history.push_back(ema200);
        while self.ema200_history.len() > PRECEDING_HOURS {
            self.ema200_history.pop_front();
        }
        self.preceding_highs.push_back(high);
        while self.preceding_highs.len() > PRECEDING_HOURS {
            self.preceding_highs.pop_front();
        }
        self.previous_close = Some(close);
        self.previous_ema12 = Some(ema12);
        self.ema12 = Some(ema12);
        self.ema48 = Some(ema48);
        self.ema200 = Some(ema200);

        (entry_signal, exit_below_ema48)
    }
}

struct OrderlyPullbackReclaim {
    hourly: HourBuilder,
    signals: HourlySignals,
    entry_time: Option<Timestamp>,
    exit_latched: bool,
}

impl OrderlyPullbackReclaim {
    fn new() -> Self {
        Self {
            hourly: HourBuilder::new(),
            signals: HourlySignals::new(),
            entry_time: None,
            exit_latched: false,
        }
    }
}

impl Strategy for OrderlyPullbackReclaim {
    fn name(&self) -> String {
        "orderly-pullback-reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let mut entry_signal = false;
        if let Some(hour) = self.hourly.push(bar) {
            let (entry, exit) = self.signals.on_hour(hour);
            entry_signal = entry;
            if exit && ctx.position(INSTRUMENT).is_positive() {
                self.exit_latched = true;
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_time = None;
            self.exit_latched = false;
        }

        if position.is_positive() {
            if let Some(entry_time) = self.entry_time {
                let held_ns = i128::from(bar.close_ts.as_nanos())
                    - i128::from(entry_time.as_nanos());
                let limit_ns = i128::from(TimeDelta::from_hours(120).as_nanos());
                if held_ns >= limit_ns {
                    self.exit_latched = true;
                }
            }

            if self.exit_latched && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if entry_signal && !self.exit_latched && ctx.open_orders() == 0 {
            let budget: Money = ctx.cash().mul_ratio(95, 100);
            let qty: Qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                budget,
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_time.is_none() {
            self.entry_time = Some(fill.ts);
            self.exit_latched = false;
        }
    }
}

export_strategy!(OrderlyPullbackReclaim::new());
