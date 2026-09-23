use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 168;
const BARS_PER_HOUR: usize = 60;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct LeftTailSkewGate {
    minute_bars_in_hour: usize,
    previous_hour_close: Option<f64>,
    hourly_returns: VecDeque<f64>,
}

impl LeftTailSkewGate {
    fn on_completed_hour(&mut self, close: tape_guest::Price, ctx: &mut Ctx) {
        let close_value = close.to_f64();
        if !close.is_positive() || !close_value.is_finite() {
            self.previous_hour_close = None;
            self.hourly_returns.clear();
            return;
        }

        if let Some(previous) = self.previous_hour_close {
            let hourly_return = (close_value / previous).ln();
            if hourly_return.is_finite() {
                self.hourly_returns.push_back(hourly_return);
                if self.hourly_returns.len() > LOOKBACK {
                    self.hourly_returns.pop_front();
                }
            } else {
                self.hourly_returns.clear();
            }
        }
        self.previous_hour_close = Some(close_value);

        let Some(skewness) = self.skewness() else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && skewness >= 0.0 {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() && skewness <= -0.5 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn skewness(&self) -> Option<f64> {
        if self.hourly_returns.len() != LOOKBACK {
            return None;
        }

        let count = self.hourly_returns.len() as f64;
        let mean = self.hourly_returns.iter().sum::<f64>() / count;
        let mut second_moment = 0.0;
        let mut third_moment = 0.0;
        for value in &self.hourly_returns {
            let deviation = *value - mean;
            second_moment += deviation * deviation;
            third_moment += deviation * deviation * deviation;
        }
        second_moment /= count;
        third_moment /= count;

        if !second_moment.is_finite() || second_moment <= 0.0 {
            return None;
        }
        let std_dev = second_moment.sqrt();
        let skewness = third_moment / (std_dev * std_dev * std_dev);
        skewness.is_finite().then_some(skewness)
    }
}

impl Strategy for LeftTailSkewGate {
    fn name(&self) -> String {
        "Left-Tail Skew Gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.minute_bars_in_hour += 1;
        let hour_ns = TimeDelta::from_mins(1).as_nanos() * BARS_PER_HOUR as i64;
        if bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0 {
            if self.minute_bars_in_hour == BARS_PER_HOUR {
                self.on_completed_hour(bar.close, ctx);
            } else {
                // Discard partial hours and do not calculate returns across a gap.
                self.previous_hour_close = None;
                self.hourly_returns.clear();
            }
            self.minute_bars_in_hour = 0;
        }
    }
}

export_strategy!(LeftTailSkewGate {
    minute_bars_in_hour: 0,
    previous_hour_close: None,
    hourly_returns: VecDeque::with_capacity(LOOKBACK + 1),
});