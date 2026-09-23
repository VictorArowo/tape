use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURNS_REQUIRED: usize = 168;
const SHORT_WINDOW: usize = 24;
const HIGH_STREAK_REQUIRED: usize = 2;
const LOW_STREAK_REQUIRED: usize = 6;
const HIGH_RATIO: f64 = 1.75;
const LOW_RATIO: f64 = 1.15;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct VolatilityAccelerationBrake {
    returns: VecDeque<f64>,
    previous_hour_ts: Option<i64>,
    previous_hour_close: Option<f64>,
    high_streak: usize,
    low_streak: usize,
    started: bool,
}

impl VolatilityAccelerationBrake {
    fn new() -> Self {
        Self {
            returns: VecDeque::with_capacity(RETURNS_REQUIRED),
            previous_hour_ts: None,
            previous_hour_close: None,
            high_streak: 0,
            low_streak: 0,
            started: false,
        }
    }

    fn clear_volatility_history(&mut self) {
        self.returns.clear();
        self.high_streak = 0;
        self.low_streak = 0;
    }

    // A one-minute bar ending on a UTC hour boundary supplies that hour's close.
    fn observe_hourly_close(&mut self, bar: &Bar) {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let timestamp = bar.close_ts.as_nanos();
        if timestamp.rem_euclid(hour_ns) != 0 {
            return;
        }

        if let Some(previous_ts) = self.previous_hour_ts {
            if timestamp <= previous_ts {
                return;
            }

            let previous_close = self.previous_hour_close.unwrap_or(0.0);
            if timestamp - previous_ts == hour_ns
                && previous_close > 0.0
                && bar.close.is_positive()
            {
                let close = bar.close.to_f64();
                let hourly_return = (close / previous_close).ln();
                if hourly_return.is_finite() {
                    self.returns.push_back(hourly_return);
                    if self.returns.len() > RETURNS_REQUIRED {
                        self.returns.pop_front();
                    }
                } else {
                    self.clear_volatility_history();
                }
            } else {
                // A gap or invalid close does not represent a one-hour return.
                self.clear_volatility_history();
            }
        }

        self.previous_hour_ts = Some(timestamp);
        self.previous_hour_close = Some(bar.close.to_f64());

        if self.returns.len() != RETURNS_REQUIRED {
            self.high_streak = 0;
            self.low_streak = 0;
            return;
        }

        let short_start = self.returns.len() - SHORT_WINDOW;
        let short_std = population_std(self.returns.iter().skip(short_start).copied());
        let long_std = population_std(self.returns.iter().copied());
        let (Some(short_std), Some(long_std)) = (short_std, long_std) else {
            self.high_streak = 0;
            self.low_streak = 0;
            return;
        };

        if !short_std.is_finite() || !long_std.is_finite() || long_std <= 0.0 {
            self.high_streak = 0;
            self.low_streak = 0;
            return;
        }

        let ratio = short_std / long_std;
        if !ratio.is_finite() {
            self.high_streak = 0;
            self.low_streak = 0;
            return;
        }

        if ratio >= HIGH_RATIO {
            self.high_streak = self.high_streak.saturating_add(1);
        } else {
            self.high_streak = 0;
        }

        if ratio <= LOW_RATIO {
            self.low_streak = self.low_streak.saturating_add(1);
        } else {
            self.low_streak = 0;
        }
    }
}

fn population_std<I>(values: I) -> Option<f64>
where
    I: Iterator<Item = f64> + Clone,
{
    let count = values.clone().count();
    if count == 0 {
        return None;
    }

    let count_f64 = count as f64;
    let mean = values.clone().sum::<f64>() / count_f64;
    if !mean.is_finite() {
        return None;
    }

    let variance = values
        .map(|value| {
            let difference = value - mean;
            difference * difference
        })
        .sum::<f64>()
        / count_f64;
    if !variance.is_finite() {
        return None;
    }

    Some(variance.sqrt())
}

impl Strategy for VolatilityAccelerationBrake {
    fn name(&self) -> String {
        "Volatility Acceleration Brake".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.observe_hourly_close(bar);

        // Establish the initial spot position, retrying if there is no fill.
        if !self.started {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                self.started = true;
            } else {
                if position.is_zero() && ctx.open_orders() == 0 && bar.close.is_positive() {
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
                return;
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && self.high_streak >= HIGH_STREAK_REQUIRED {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        } else if position.is_zero() && self.low_streak >= LOW_STREAK_REQUIRED {
            if bar.close.is_positive() {
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
}

export_strategy!(VolatilityAccelerationBrake::new());
