use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const RANGE_HOURS: usize = 12;
const VOLUME_HOURS: usize = 24;
const EXIT_RANGE_HOURS: usize = 6;
const VOL_HISTORY_HOURS: usize = 168;
const HOLD_HOURS: i64 = 72;
const HISTORY_CAP: usize = 168;

fn hour_nanos() -> i64 {
    TimeDelta::from_hours(1).as_nanos()
}

fn buy_reference_price(price: Price) -> Price {
    // Reserve for the stated 0.02% adverse buy-side slippage when sizing.
    price.mul_ratio(10_002, 10_000)
}

#[derive(Clone, Copy)]
struct HourBar {
    high: Price,
    low: Price,
    close: Price,
    volume_raw: i128,
}

impl HourBar {
    fn from_minute(bar: &Bar) -> Self {
        Self {
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume_raw: i128::from(bar.volume.raw()),
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
        self.volume_raw += i128::from(bar.volume.raw());
    }
}

#[derive(Clone, Copy)]
enum Intent {
    Enter { target_raw: i64 },
    Exit,
}

struct CompressionReleaseBreakout {
    hour_key: Option<i64>,
    current_hour: Option<HourBar>,
    completed_hours: VecDeque<HourBar>,
    six_hour_rv: VecDeque<f64>,
    intent: Option<Intent>,
    first_fill_ns: Option<i64>,
}

impl CompressionReleaseBreakout {
    fn new() -> Self {
        Self {
            hour_key: None,
            current_hour: None,
            completed_hours: VecDeque::with_capacity(VOLUME_HOURS + 1),
            six_hour_rv: VecDeque::with_capacity(HISTORY_CAP + 1),
            intent: None,
            first_fill_ns: None,
        }
    }

    fn ingest_minute(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let hour_ns = hour_nanos();
        let key = bar.open_ts.as_nanos().div_euclid(hour_ns);

        if self.hour_key.is_some_and(|old_key| old_key != key) {
            if let Some(completed) = self.current_hour.take() {
                self.finish_hour(ctx, completed);
            }
            self.hour_key = None;
        }

        if self.current_hour.is_none() {
            self.hour_key = Some(key);
            self.current_hour = Some(HourBar::from_minute(bar));
        } else if let Some(hour) = self.current_hour.as_mut() {
            hour.add_minute(bar);
        }

        if bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0 {
            if let Some(completed) = self.current_hour.take() {
                self.finish_hour(ctx, completed);
            }
            self.hour_key = None;
        }
    }

    fn finish_hour(&mut self, ctx: &mut Ctx, hour: HourBar) {
        let prior_len = self.completed_hours.len();
        let breakout_high = if prior_len >= RANGE_HOURS {
            self.completed_hours
                .iter()
                .rev()
                .take(RANGE_HOURS)
                .map(|h| h.high.raw())
                .max()
        } else {
            None
        };
        let exit_low = if prior_len >= EXIT_RANGE_HOURS {
            self.completed_hours
                .iter()
                .rev()
                .take(EXIT_RANGE_HOURS)
                .map(|h| h.low.raw())
                .min()
        } else {
            None
        };
        let prior_volume_sum: i128 = if prior_len >= VOLUME_HOURS {
            self.completed_hours
                .iter()
                .rev()
                .take(VOLUME_HOURS)
                .map(|h| h.volume_raw)
                .sum()
        } else {
            0
        };

        let volatility_compressed = self.compressed_volatility();
        let entry_signal = breakout_high.is_some_and(|high| hour.close.raw() > high)
            && prior_len >= VOLUME_HOURS
            && hour.volume_raw * 16 > prior_volume_sum
            && volatility_compressed;
        let exit_signal = exit_low.is_some_and(|low| hour.close.raw() < low);

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if exit_signal {
                self.intent = Some(Intent::Exit);
            }
        } else if position.is_zero() && self.intent.is_none() && entry_signal {
            let reference = buy_reference_price(hour.close);
            let target = ctx.affordable_qty(
                INSTRUMENT,
                reference,
                ctx.equity(),
                SIZING_MARGIN_PPM,
            );
            if target.is_positive() {
                self.intent = Some(Intent::Enter {
                    target_raw: target.raw(),
                });
            }
        }

        if let Some(rv) = self.calculate_six_hour_rv(hour) {
            self.six_hour_rv.push_back(rv);
            while self.six_hour_rv.len() > HISTORY_CAP {
                self.six_hour_rv.pop_front();
            }
        }

        self.completed_hours.push_back(hour);
        while self.completed_hours.len() > VOLUME_HOURS {
            self.completed_hours.pop_front();
        }
    }

    fn compressed_volatility(&self) -> bool {
        if self.six_hour_rv.len() < VOL_HISTORY_HOURS {
            return false;
        }
        // The newest stored observation ends at the hour immediately before
        // the one being evaluated; the current hour is excluded.
        let Some(current_rv) = self.six_hour_rv.back().copied() else {
            return false;
        };
        let mut observations: Vec<f64> = self
            .six_hour_rv
            .iter()
            .rev()
            .take(VOL_HISTORY_HOURS)
            .copied()
            .collect();
        observations.sort_by(f64::total_cmp);
        let rank = 0.30 * (observations.len() - 1) as f64;
        let lower = rank.floor() as usize;
        let upper = rank.ceil() as usize;
        let fraction = rank - lower as f64;
        let percentile = observations[lower] * (1.0 - fraction) + observations[upper] * fraction;
        current_rv <= percentile
    }

    fn calculate_six_hour_rv(&self, current: HourBar) -> Option<f64> {
        if self.completed_hours.len() < 6 {
            return None;
        }
        let start = self.completed_hours.len() - 6;
        let mut previous = self.completed_hours.get(start)?.close.to_f64();
        if previous <= 0.0 {
            return None;
        }
        let mut sum_squared_returns = 0.0;
        for index in (start + 1)..self.completed_hours.len() {
            let close = self.completed_hours.get(index)?.close.to_f64();
            if close <= 0.0 {
                return None;
            }
            let ret = (close / previous).ln();
            sum_squared_returns += ret * ret;
            previous = close;
        }
        let close = current.close.to_f64();
        if close <= 0.0 {
            return None;
        }
        let ret = (close / previous).ln();
        sum_squared_returns += ret * ret;
        Some(sum_squared_returns.sqrt())
    }

    fn service_intent(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }

        match self.intent {
            Some(Intent::Enter { target_raw }) => {
                let position = ctx.position(INSTRUMENT);
                if position.raw() >= target_raw {
                    self.intent = None;
                    return;
                }
                let remaining_raw = target_raw - position.raw();
                let reference = buy_reference_price(
                    ctx.last_price(INSTRUMENT).unwrap_or(bar.close),
                );
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    reference,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let qty_raw = remaining_raw.min(affordable.raw());
                if qty_raw > 0 {
                    ctx.submit(OrderRequest::market(
                        INSTRUMENT,
                        Side::Buy,
                        Qty::from_raw(qty_raw),
                    ));
                }
            }
            Some(Intent::Exit) => {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                } else {
                    self.intent = None;
                    self.first_fill_ns = None;
                }
            }
            None => {}
        }
    }
}

impl Strategy for CompressionReleaseBreakout {
    fn name(&self) -> String {
        "compression-release-breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if ctx.position(INSTRUMENT).is_zero()
            && matches!(self.intent, Some(Intent::Exit))
            && ctx.open_orders() == 0
        {
            self.intent = None;
            self.first_fill_ns = None;
        }

        if ctx.position(INSTRUMENT).is_positive() {
            if let Some(entry_ns) = self.first_fill_ns {
                let held_ns = bar.close_ts.as_nanos().saturating_sub(entry_ns);
                if held_ns >= TimeDelta::from_hours(HOLD_HOURS).as_nanos() {
                    self.intent = Some(Intent::Exit);
                }
            }
        }

        self.ingest_minute(ctx, bar);
        self.service_intent(ctx, bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.first_fill_ns.is_none() {
                self.first_fill_ns = Some(fill.ts.as_nanos());
            }
        } else if ctx.position(INSTRUMENT).is_zero() {
            self.first_fill_ns = None;
        }
    }
}

export_strategy!(CompressionReleaseBreakout::new());
