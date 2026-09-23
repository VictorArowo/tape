use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EMA_PERIOD: usize = 720;
const EMA_LOOKBACK: usize = 168;
const PULLBACK_LOOKBACK: usize = 72;
const BREAKOUT_LOOKBACK: usize = 6;
const ATR_PERIOD: usize = 24;
const EXIT_HOURS: i64 = 168;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HourBar {
    bucket: i64,
    open_ts_ns: i64,
    close_ts_ns: i64,
    open: tape_guest::Price,
    high: tape_guest::Price,
    low: tape_guest::Price,
    close: tape_guest::Price,
    volume: Qty,
    minute_count: u16,
}

struct HourAggregator {
    current: Option<HourBar>,
}

impl HourAggregator {
    fn new() -> Self {
        Self { current: None }
    }

    fn push(&mut self, bar: &Bar) -> Vec<HourBar> {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let mut completed = Vec::with_capacity(2);

        if self.current.as_ref().is_some_and(|hour| hour.bucket != bucket) {
            if let Some(hour) = self.take_if_complete(hour_ns) {
                completed.push(hour);
            }
        }

        if self.current.is_none() {
            self.current = Some(HourBar {
                bucket,
                open_ts_ns: bar.open_ts.as_nanos(),
                close_ts_ns: bar.close_ts.as_nanos(),
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
                minute_count: 1,
            });
        } else if let Some(hour) = self.current.as_mut() {
            hour.high = if bar.high > hour.high { bar.high } else { hour.high };
            hour.low = if bar.low < hour.low { bar.low } else { hour.low };
            hour.close = bar.close;
            hour.close_ts_ns = bar.close_ts.as_nanos();
            hour.volume = hour.volume + bar.volume;
            hour.minute_count = hour.minute_count.saturating_add(1);
        }

        if bar.close_ts.as_nanos().div_euclid(hour_ns) != bucket {
            if let Some(hour) = self.take_if_complete(hour_ns) {
                completed.push(hour);
            }
        }
        completed
    }

    fn take_if_complete(&mut self, hour_ns: i64) -> Option<HourBar> {
        let hour = self.current.take()?;
        let starts_at_boundary = hour.open_ts_ns.rem_euclid(hour_ns) == 0;
        let ends_at_boundary = hour.close_ts_ns.div_euclid(hour_ns) != hour.bucket;
        if hour.minute_count == 60 && starts_at_boundary && ends_at_boundary {
            Some(hour)
        } else {
            None
        }
    }
}

struct Execution {
    side: Side,
    remaining: Qty,
}

struct TrendGatedPullbackReclaim {
    aggregator: HourAggregator,
    closes: VecDeque<f64>,
    highs: VecDeque<f64>,
    true_ranges: VecDeque<f64>,
    true_range_sum: f64,
    previous_close: Option<f64>,
    ema_seed_sum: f64,
    ema_seed_count: usize,
    ema: Option<f64>,
    ema_history: VecDeque<f64>,
    setup_armed: bool,
    execution: Option<Execution>,
    entry_ts_ns: Option<i64>,
    high_water_close: Option<f64>,
}

impl TrendGatedPullbackReclaim {
    fn new() -> Self {
        Self {
            aggregator: HourAggregator::new(),
            closes: VecDeque::with_capacity(PULLBACK_LOOKBACK),
            highs: VecDeque::with_capacity(BREAKOUT_LOOKBACK),
            true_ranges: VecDeque::with_capacity(ATR_PERIOD),
            true_range_sum: 0.0,
            previous_close: None,
            ema_seed_sum: 0.0,
            ema_seed_count: 0,
            ema: None,
            ema_history: VecDeque::with_capacity(EMA_LOOKBACK + 1),
            setup_armed: false,
            execution: None,
            entry_ts_ns: None,
            high_water_close: None,
        }
    }

    fn on_hour(&mut self, ctx: &mut Ctx, hour: HourBar) {
        let _aggregated_volume = hour.volume;
        let close = hour.close.to_f64();
        let high = hour.high.to_f64();
        let low = hour.low.to_f64();

        let prior_72_highest_close = if self.closes.len() == PULLBACK_LOOKBACK {
            self.closes.iter().copied().reduce(f64::max)
        } else {
            None
        };
        let prior_6_highest_high = if self.highs.len() == BREAKOUT_LOOKBACK {
            self.highs.iter().copied().reduce(f64::max)
        } else {
            None
        };

        let true_range = match self.previous_close {
            Some(previous) => (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
            None => high - low,
        };
        self.true_ranges.push_back(true_range);
        self.true_range_sum += true_range;
        if self.true_ranges.len() > ATR_PERIOD {
            self.true_range_sum -= self.true_ranges.pop_front().unwrap_or(0.0);
        }
        let atr = (self.true_ranges.len() == ATR_PERIOD)
            .then_some(self.true_range_sum / ATR_PERIOD as f64);

        if let Some(ema) = self.ema {
            let alpha = 2.0 / (EMA_PERIOD as f64 + 1.0);
            self.ema = Some(alpha * close + (1.0 - alpha) * ema);
        } else {
            self.ema_seed_sum += close;
            self.ema_seed_count += 1;
            if self.ema_seed_count == EMA_PERIOD {
                self.ema = Some(self.ema_seed_sum / EMA_PERIOD as f64);
            }
        }

        if let Some(ema) = self.ema {
            self.ema_history.push_back(ema);
            if self.ema_history.len() > EMA_LOOKBACK + 1 {
                self.ema_history.pop_front();
            }
        }

        let ema_168_hours_ago = (self.ema_history.len() == EMA_LOOKBACK + 1)
            .then(|| self.ema_history[0]);
        let current_ema = self.ema;
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            if self.entry_ts_ns.is_none() {
                self.entry_ts_ns = Some(hour.close_ts_ns);
            }
            let high_water = self.high_water_close.get_or_insert(close);
            if close > *high_water {
                *high_water = close;
            }

            let below_ema = current_ema.is_some_and(|ema| close < ema);
            let below_atr_trail = atr.is_some_and(|average_tr| {
                self.high_water_close
                    .is_some_and(|water| close < water - 2.5 * average_tr)
            });
            let time_exit = self.entry_ts_ns.is_some_and(|entry_ns| {
                hour.close_ts_ns.saturating_sub(entry_ns)
                    >= TimeDelta::from_hours(EXIT_HOURS).as_nanos()
            });

            if below_ema || below_atr_trail || time_exit {
                self.setup_armed = false;
                self.execution = Some(Execution {
                    side: Side::Sell,
                    remaining: position,
                });
            }
        } else {
            if self.entry_ts_ns.is_some() {
                self.entry_ts_ns = None;
                self.high_water_close = None;
                self.setup_armed = false;
            }

            let buy_in_progress = self
                .execution
                .as_ref()
                .is_some_and(|execution| matches!(execution.side, Side::Buy));
            let sell_in_progress = self
                .execution
                .as_ref()
                .is_some_and(|execution| matches!(execution.side, Side::Sell));

            if !buy_in_progress && !sell_in_progress {
                if let Some(reference) = prior_72_highest_close {
                    if close <= reference * 0.97 {
                        self.setup_armed = true;
                    }
                }

                let trend_ok = match (current_ema, ema_168_hours_ago) {
                    (Some(ema), Some(past_ema)) => close > ema && ema > past_ema,
                    _ => false,
                };
                let breakout_ok = prior_6_highest_high
                    .is_some_and(|previous_high| close > previous_high);

                if self.setup_armed && trend_ok && breakout_ok && hour.close.is_positive() {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        hour.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        self.setup_armed = false;
                        self.execution = Some(Execution {
                            side: Side::Buy,
                            remaining: qty,
                        });
                    }
                }
            }
        }

        self.previous_close = Some(close);
        self.closes.push_back(close);
        if self.closes.len() > PULLBACK_LOOKBACK {
            self.closes.pop_front();
        }
        self.highs.push_back(high);
        if self.highs.len() > BREAKOUT_LOOKBACK {
            self.highs.pop_front();
        }
    }

    fn drive_execution(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }

        let Some(execution) = self.execution.as_ref() else {
            return;
        };
        if !execution.remaining.is_positive() {
            self.execution = None;
            return;
        }

        let side = execution.side;
        let mut quantity = min_qty(execution.remaining, bar.volume.mul_ratio(1, 10));
        if matches!(side, Side::Buy) {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            quantity = min_qty(quantity, affordable);
        } else {
            let position = ctx.position(INSTRUMENT);
            if !position.is_positive() {
                self.execution = None;
                return;
            }
            quantity = min_qty(quantity, position);
        }

        if quantity.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, side, quantity)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }

    fn handle_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if let Some(execution) = self.execution.as_mut() {
            let matching_side = matches!(
                (execution.side, fill.side),
                (Side::Buy, Side::Buy) | (Side::Sell, Side::Sell)
            );
            if matching_side {
                execution.remaining = Qty::from_raw(
                    execution
                        .remaining
                        .raw()
                        .saturating_sub(fill.qty.raw())
                        .max(0),
                );
                if !execution.remaining.is_positive() {
                    self.execution = None;
                }
            }
        }

        if matches!(fill.side, Side::Buy) && self.entry_ts_ns.is_none() {
            self.entry_ts_ns = Some(fill.ts.as_nanos());
            self.high_water_close = None;
        }
        if matches!(fill.side, Side::Sell) && !ctx.position(INSTRUMENT).is_positive() {
            self.entry_ts_ns = None;
            self.high_water_close = None;
            self.setup_armed = false;
            if self
                .execution
                .as_ref()
                .is_some_and(|execution| matches!(execution.side, Side::Sell))
            {
                self.execution = None;
            }
        }
    }
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    Qty::from_raw(a.raw().min(b.raw()).max(0))
}

impl Strategy for TrendGatedPullbackReclaim {
    fn name(&self) -> String {
        "Trend-Gated Pullback Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        for hour in self.aggregator.push(bar) {
            self.on_hour(ctx, hour);
        }
        self.drive_execution(ctx, bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        self.handle_fill(ctx, fill);
    }
}

export_strategy!(TrendGatedPullbackReclaim::new());
