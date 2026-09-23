use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EMA_4H_FAST: usize = 20;
const EMA_4H_SLOW: usize = 50;
const EMA_1H_FAST: usize = 10;
const EMA_1H_SLOW: usize = 20;
const ATR_LEN: usize = 14;
const BUY_MARGIN_PPM: i64 = 5_000;
const MINUTES_PER_HOUR: usize = 60;

#[derive(Clone, Copy)]
struct HourBar {
    start_ns: i64,
    high_raw: i64,
    low_raw: i64,
    close_raw: i64,
}

struct HourAccumulator {
    bucket: Option<i64>,
    count: usize,
    high_raw: i64,
    low_raw: i64,
    close_raw: i64,
}

impl HourAccumulator {
    fn new() -> Self {
        Self {
            bucket: None,
            count: 0,
            high_raw: 0,
            low_raw: 0,
            close_raw: 0,
        }
    }

    fn push(&mut self, bar: &Bar, hour_ns: i64) -> Option<HourBar> {
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_ns);
        if self.bucket != Some(bucket) {
            self.bucket = Some(bucket);
            self.count = 0;
            self.high_raw = bar.high.raw();
            self.low_raw = bar.low.raw();
            self.close_raw = bar.close.raw();
        }

        self.count += 1;
        self.high_raw = self.high_raw.max(bar.high.raw());
        self.low_raw = self.low_raw.min(bar.low.raw());
        self.close_raw = bar.close.raw();

        if bar.close_ts.as_nanos().rem_euclid(hour_ns) != 0 {
            return None;
        }

        let completed = if self.count == MINUTES_PER_HOUR {
            Some(HourBar {
                start_ns: bucket * hour_ns,
                high_raw: self.high_raw,
                low_raw: self.low_raw,
                close_raw: self.close_raw,
            })
        } else {
            None
        };
        self.bucket = None;
        self.count = 0;
        completed
    }
}

struct FourHourAccumulator {
    bucket: Option<i64>,
    count: usize,
    close_raw: i64,
}

impl FourHourAccumulator {
    fn new() -> Self {
        Self {
            bucket: None,
            count: 0,
            close_raw: 0,
        }
    }

    fn push(&mut self, hour: HourBar, hour_ns: i64, four_hours_ns: i64) -> Option<i64> {
        let bucket = hour.start_ns.div_euclid(four_hours_ns);
        if self.bucket != Some(bucket) {
            self.bucket = Some(bucket);
            self.count = 0;
        }
        self.count += 1;
        self.close_raw = hour.close_raw;

        let hour_close_ns = hour.start_ns + hour_ns;
        if hour_close_ns.rem_euclid(four_hours_ns) != 0 {
            return None;
        }

        let completed = (self.count == 4).then_some(self.close_raw);
        self.bucket = None;
        self.count = 0;
        completed
    }
}

#[derive(Default)]
struct Ema {
    value: Option<f64>,
}

impl Ema {
    fn push(&mut self, raw_value: i64, span: usize) -> f64 {
        let value = raw_value as f64;
        let alpha = 2.0 / (span as f64 + 1.0);
        let next = match self.value {
            Some(previous) => alpha * value + (1.0 - alpha) * previous,
            None => value,
        };
        self.value = Some(next);
        next
    }
}

struct WilderAtr {
    count: usize,
    seed_sum: f64,
    value: Option<f64>,
}

impl WilderAtr {
    fn new() -> Self {
        Self {
            count: 0,
            seed_sum: 0.0,
            value: None,
        }
    }

    fn push(&mut self, true_range_raw: f64) -> Option<f64> {
        if self.count < ATR_LEN {
            self.seed_sum += true_range_raw;
            self.count += 1;
            if self.count == ATR_LEN {
                self.value = Some(self.seed_sum / ATR_LEN as f64);
            }
        } else if let Some(previous) = self.value {
            self.value = Some(
                (previous * (ATR_LEN as f64 - 1.0) + true_range_raw) / ATR_LEN as f64,
            );
        }
        self.value
    }
}

struct HourlySignal {
    close_raw: i64,
    ema20_raw: f64,
    crossed_above_ema10: bool,
    pullback_found: bool,
}

struct HourlyIndicators {
    ema10: Ema,
    ema20: Ema,
    atr: WilderAtr,
    previous_close_raw: Option<i64>,
    closes: VecDeque<i64>,
}

impl HourlyIndicators {
    fn new() -> Self {
        Self {
            ema10: Ema::default(),
            ema20: Ema::default(),
            atr: WilderAtr::new(),
            previous_close_raw: None,
            closes: VecDeque::with_capacity(24),
        }
    }

    fn push(&mut self, hour: HourBar) -> HourlySignal {
        let close = hour.close_raw as f64;
        let previous_close = self.previous_close_raw;
        let previous_ema10 = self.ema10.value;

        let true_range = match previous_close {
            Some(previous) => {
                let previous = previous as f64;
                (hour.high_raw as f64 - hour.low_raw as f64)
                    .max((hour.high_raw as f64 - previous).abs())
                    .max((hour.low_raw as f64 - previous).abs())
            }
            None => (hour.high_raw - hour.low_raw).max(0) as f64,
        };
        let atr = self.atr.push(true_range);

        let ema10 = self.ema10.push(hour.close_raw, EMA_1H_FAST);
        let ema20 = self.ema20.push(hour.close_raw, EMA_1H_SLOW);
        let crossed_above_ema10 = matches!((previous_close, previous_ema10), (Some(pc), Some(pe)) if pc as f64 <= pe && close > ema10);

        let pullback_found = if self.closes.len() >= 24 {
            match atr {
                Some(current_atr) => {
                    let previous_24_max = self.closes.iter().copied().max();
                    let previous_12_min = self
                        .closes
                        .iter()
                        .skip(self.closes.len() - 12)
                        .copied()
                        .min();
                    matches!((previous_24_max, previous_12_min), (Some(max_close), Some(min_close))
                        if min_close as f64 <= max_close as f64 - 1.5 * current_atr)
                }
                None => false,
            }
        } else {
            false
        };

        self.closes.push_back(hour.close_raw);
        if self.closes.len() > 24 {
            self.closes.pop_front();
        }
        self.previous_close_raw = Some(hour.close_raw);

        HourlySignal {
            close_raw: hour.close_raw,
            ema20_raw: ema20,
            crossed_above_ema10,
            pullback_found,
        }
    }
}

struct FourHourIndicators {
    ema20: Ema,
    ema50: Ema,
    recent_ema20: VecDeque<f64>,
}

impl FourHourIndicators {
    fn new() -> Self {
        Self {
            ema20: Ema::default(),
            ema50: Ema::default(),
            recent_ema20: VecDeque::with_capacity(4),
        }
    }

    fn push(&mut self, close_raw: i64) {
        let ema20 = self.ema20.push(close_raw, EMA_4H_FAST);
        self.ema50.push(close_raw, EMA_4H_SLOW);
        self.recent_ema20.push_back(ema20);
        if self.recent_ema20.len() > 4 {
            self.recent_ema20.pop_front();
        }
    }

    fn uptrend(&self) -> bool {
        match (self.ema20.value, self.ema50.value, self.recent_ema20.front()) {
            (Some(ema20), Some(ema50), Some(ema20_three_bars_ago)) => {
                ema20 > ema50 && ema20 > *ema20_three_bars_ago
            }
            _ => false,
        }
    }

    fn bearish(&self) -> bool {
        matches!((self.ema20.value, self.ema50.value), (Some(ema20), Some(ema50)) if ema20 <= ema50)
    }
}

struct TrendPullbackReclaim {
    hour_ns: i64,
    four_hours_ns: i64,
    max_hold_ns: i64,
    hour_accumulator: HourAccumulator,
    four_hour_accumulator: FourHourAccumulator,
    hourly: HourlyIndicators,
    four_hourly: FourHourIndicators,
    entry_ts_ns: Option<i64>,
    exit_latched: bool,
}

impl TrendPullbackReclaim {
    fn new() -> Self {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        Self {
            hour_ns,
            four_hours_ns: TimeDelta::from_hours(4).as_nanos(),
            max_hold_ns: TimeDelta::from_hours(72).as_nanos(),
            hour_accumulator: HourAccumulator::new(),
            four_hour_accumulator: FourHourAccumulator::new(),
            hourly: HourlyIndicators::new(),
            four_hourly: FourHourIndicators::new(),
            entry_ts_ns: None,
            exit_latched: false,
        }
    }

    fn submit_exit_if_possible(&self, ctx: &mut Ctx) {
        if !self.exit_latched || ctx.open_orders() != 0 {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

impl Strategy for TrendPullbackReclaim {
    fn name(&self) -> String {
        "trend-pullback-reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_ts_ns = None;
            self.exit_latched = false;
        } else if let Some(entry_ts_ns) = self.entry_ts_ns {
            if ctx.now().as_nanos().saturating_sub(entry_ts_ns) >= self.max_hold_ns {
                self.exit_latched = true;
            }
        }

        if let Some(hour) = self.hour_accumulator.push(bar, self.hour_ns) {
            if let Some(four_hour_close) = self.four_hour_accumulator.push(
                hour,
                self.hour_ns,
                self.four_hours_ns,
            ) {
                self.four_hourly.push(four_hour_close);
            }

            let signal = self.hourly.push(hour);
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if (signal.close_raw as f64) < signal.ema20_raw || self.four_hourly.bearish() {
                    self.exit_latched = true;
                }
            } else if position.is_zero()
                && !self.exit_latched
                && ctx.open_orders() == 0
                && self.four_hourly.uptrend()
                && signal.pullback_found
                && signal.crossed_above_ema10
            {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    BUY_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        self.submit_exit_if_possible(ctx);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_ts_ns.is_none() {
            self.entry_ts_ns = Some(fill.ts.as_nanos());
            self.exit_latched = false;
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_ts_ns = None;
            self.exit_latched = false;
        }
    }
}

export_strategy!(TrendPullbackReclaim::new());
