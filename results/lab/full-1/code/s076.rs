use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 60;
const HOURLY_RETURNS: usize = 720;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SQUEEZE_MULTIPLIER: f64 = 2.5;

struct PositiveSqueezeCooldown {
    closes: VecDeque<i64>,
    hourly_returns: VecDeque<f64>,
    last_hour_close: Option<(i64, f64)>,
    initial_filled: bool,
    cooldown_until_ns: Option<i64>,
}

impl PositiveSqueezeCooldown {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(LOOKBACK_BARS + 1),
            hourly_returns: VecDeque::with_capacity(HOURLY_RETURNS),
            last_hour_close: None,
            initial_filled: false,
            cooldown_until_ns: None,
        }
    }

    fn hourly_std(&self) -> Option<f64> {
        if self.hourly_returns.len() != HOURLY_RETURNS {
            return None;
        }

        let count = self.hourly_returns.len() as f64;
        let mean = self.hourly_returns.iter().sum::<f64>() / count;
        let variance = self
            .hourly_returns
            .iter()
            .map(|value| {
                let difference = value - mean;
                difference * difference
            })
            .sum::<f64>()
            / count;
        let std = variance.sqrt();
        std.is_finite().then_some(std)
    }

    fn push_hourly_return(&mut self, value: f64) {
        self.hourly_returns.push_back(value);
        if self.hourly_returns.len() > HOURLY_RETURNS {
            self.hourly_returns.pop_front();
        }
    }
}

impl Strategy for PositiveSqueezeCooldown {
    fn name(&self) -> String {
        "positive-squeeze-cooldown".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let current_close = bar.close.to_f64();
        let mut sixty_bar_return = None;
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > LOOKBACK_BARS + 1 {
            self.closes.pop_front();
        }
        if self.closes.len() == LOOKBACK_BARS + 1 {
            let earlier = self.closes.front().copied().unwrap_or(0) as f64 / 100_000_000.0;
            if earlier > 0.0 && current_close > 0.0 {
                let value = (current_close / earlier).ln();
                if value.is_finite() {
                    sixty_bar_return = Some(value);
                }
            }
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let minute_index = close_ns / minute_ns;
        let is_hour_close = minute_index % 60 == 0;
        let hour_bucket = close_ns / hour_ns;

        // At an exact UTC-hour close, evaluate the signal before adding the
        // return that has just completed, so the current hour is excluded.
        let completed_hour_return = if is_hour_close {
            self.last_hour_close.and_then(|(previous_bucket, previous_close)| {
                let consecutive = previous_bucket.checked_add(1) == Some(hour_bucket);
                if consecutive && previous_close > 0.0 && current_close > 0.0 {
                    let value = (current_close / previous_close).ln();
                    value.is_finite().then_some(value)
                } else {
                    None
                }
            })
        } else {
            None
        };

        let squeeze = match (sixty_bar_return, self.hourly_std()) {
            (Some(return_value), Some(std)) => {
                return_value > SQUEEZE_MULTIPLIER * std
            }
            _ => false,
        };

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if squeeze {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            } else if position.is_zero() {
                let should_buy = if !self.initial_filled {
                    true
                } else {
                    self.cooldown_until_ns
                        .is_some_and(|deadline| close_ns >= deadline)
                };

                if should_buy && bar.close.is_positive() {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }

        if is_hour_close {
            if current_close > 0.0 {
                self.last_hour_close = Some((hour_bucket, current_close));
            } else {
                self.last_hour_close = None;
            }
            if let Some(value) = completed_hour_return {
                self.push_hourly_return(value);
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                self.initial_filled = true;
            }
            Side::Sell => {
                let cooldown_ns = TimeDelta::from_hours(12).as_nanos();
                self.cooldown_until_ns =
                    Some(fill.ts.as_nanos().saturating_add(cooldown_ns));
            }
        }
    }
}

export_strategy!(PositiveSqueezeCooldown::new());
