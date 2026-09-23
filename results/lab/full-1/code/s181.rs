use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIX_HOURS_MINUTES: i64 = 360;
const MIN_COMPLETED_BARS: usize = 80;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct SixHourCandle {
    open: tape_guest::Price,
    high: tape_guest::Price,
    low: tape_guest::Price,
    close: tape_guest::Price,
}

struct PartialCandle {
    bucket: i64,
    count: usize,
    candle: SixHourCandle,
}

struct TrendPullbackReclaim {
    partial: Option<PartialCandle>,
    completed_bars: usize,
    ema20: Option<f64>,
    ema80: Option<f64>,
    ema20_history: VecDeque<f64>,
    prior_highs: VecDeque<i64>,

    entry_active: bool,
    target_entry_raw: i64,
    exit_active: bool,
    entry_qty_raw: i128,
    entry_value_qty: i128,
}

impl TrendPullbackReclaim {
    fn new() -> Self {
        Self {
            partial: None,
            completed_bars: 0,
            ema20: None,
            ema80: None,
            ema20_history: VecDeque::with_capacity(5),
            prior_highs: VecDeque::with_capacity(4),
            entry_active: false,
            target_entry_raw: 0,
            exit_active: false,
            entry_qty_raw: 0,
            entry_value_qty: 0,
        }
    }

    fn push_minute(&mut self, bar: &Bar) -> Option<SixHourCandle> {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute = bar.open_ts.as_nanos() / minute_ns;
        let bucket = minute.div_euclid(SIX_HOURS_MINUTES);
        let offset = minute.rem_euclid(SIX_HOURS_MINUTES);

        if self.partial.as_ref().map(|p| p.bucket) != Some(bucket) {
            self.partial = Some(PartialCandle {
                bucket,
                count: 0,
                candle: SixHourCandle {
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                },
            });
        }

        let partial = self.partial.as_mut().expect("partial candle initialized");
        if partial.count > 0 {
            partial.candle.high = partial.candle.high.max(bar.high);
            partial.candle.low = partial.candle.low.min(bar.low);
            partial.candle.close = bar.close;
        }
        partial.count += 1;

        if offset == SIX_HOURS_MINUTES - 1 && partial.count == SIX_HOURS_MINUTES as usize {
            Some(partial.candle)
        } else {
            None
        }
    }

    fn on_completed_candle(&mut self, candle: SixHourCandle, has_position: bool) -> bool {
        let close = candle.close.to_f64();
        let next_ema20 = match self.ema20 {
            Some(previous) => previous + (2.0 / 21.0) * (close - previous),
            None => close,
        };
        let next_ema80 = match self.ema80 {
            Some(previous) => previous + (2.0 / 81.0) * (close - previous),
            None => close,
        };
        self.ema20 = Some(next_ema20);
        self.ema80 = Some(next_ema80);
        self.completed_bars += 1;

        self.ema20_history.push_back(next_ema20);
        if self.ema20_history.len() > 5 {
            self.ema20_history.pop_front();
        }

        if has_position && close <= next_ema20 {
            self.exit_active = true;
        }

        let prior_high = self.prior_highs.iter().copied().max();
        let immediately_prior_high = self.prior_highs.back().copied();
        let ema20_four_bars_ago = if self.ema20_history.len() == 5 {
            self.ema20_history.front().copied()
        } else {
            None
        };

        let signal = if self.completed_bars >= MIN_COMPLETED_BARS {
            match (prior_high, immediately_prior_high, ema20_four_bars_ago) {
                (Some(high), Some(previous_high), Some(old_ema20)) => {
                    let close_raw = i128::from(candle.close.raw());
                    let high_raw = i128::from(high);
                    self.ema20.unwrap_or(0.0) > self.ema80.unwrap_or(0.0)
                        && next_ema20 > old_ema20
                        && close_raw * 1_000 <= high_raw * 985
                        && close_raw * 1_000 >= high_raw * 955
                        && candle.close.raw() > previous_high
                }
                _ => false,
            }
        } else {
            false
        };

        self.prior_highs.push_back(candle.high.raw());
        if self.prior_highs.len() > 4 {
            self.prior_highs.pop_front();
        }
        signal
    }

    fn clear_entry_basis(&mut self) {
        self.entry_qty_raw = 0;
        self.entry_value_qty = 0;
    }

    fn submit_buy(&mut self, ctx: &mut Ctx, price: tape_guest::Price, desired_raw: i64) {
        if desired_raw <= 0 || ctx.open_orders() != 0 {
            return;
        }
        let affordable = ctx.affordable_qty(INSTRUMENT, price, ctx.cash(), SIZING_MARGIN_PPM);
        let qty_raw = desired_raw.min(affordable.raw().max(0));
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty_raw),
            ));
        }
    }
}

impl Strategy for TrendPullbackReclaim {
    fn name(&self) -> String {
        "Trend Pullback Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let completed = self.push_minute(bar);
        let has_position = ctx.position(INSTRUMENT).is_positive();
        let entry_signal = completed
            .map(|candle| self.on_completed_candle(candle, has_position))
            .unwrap_or(false);

        if self.entry_qty_raw > 0 {
            let stop_raw = (self.entry_value_qty / self.entry_qty_raw) as i64;
            if i128::from(bar.low.raw()) * 100 <= i128::from(stop_raw) * 95 && has_position {
                self.exit_active = true;
            }
        }

        let position = ctx.position(INSTRUMENT);
        if self.exit_active {
            self.entry_active = false;
            self.target_entry_raw = 0;
            if position.is_positive() {
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            } else {
                self.exit_active = false;
                self.clear_entry_basis();
            }
            return;
        }

        if self.entry_active {
            let held_raw = position.raw().max(0);
            if held_raw >= self.target_entry_raw {
                self.entry_active = false;
                return;
            }
            if ctx.open_orders() == 0 {
                let remaining = self.target_entry_raw - held_raw;
                self.submit_buy(ctx, bar.close, remaining);
            }
            return;
        }

        if entry_signal && !position.is_positive() && ctx.open_orders() == 0 {
            let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
            if qty.is_positive() {
                self.target_entry_raw = qty.raw();
                self.entry_active = true;
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        let qty_raw = i128::from(fill.qty.raw());
        if fill.side == Side::Buy {
            self.entry_value_qty += i128::from(fill.price.raw()) * qty_raw;
            self.entry_qty_raw += qty_raw;
        } else if fill.side == Side::Sell && self.entry_qty_raw > 0 {
            let average_raw = self.entry_value_qty / self.entry_qty_raw;
            self.entry_qty_raw -= qty_raw;
            if self.entry_qty_raw <= 0 {
                self.clear_entry_basis();
            } else {
                self.entry_value_qty = average_raw * self.entry_qty_raw;
            }
        }
    }
}

export_strategy!(TrendPullbackReclaim::new());
