use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_RETURNS: usize = 2_016;
const MIN_NEGATIVE_RETURNS: usize = 100;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DownsideTailGate {
    last_five_minute_close: Option<Price>,
    returns: VecDeque<f64>,
    below_since_ns: Option<i64>,
}

impl DownsideTailGate {
    fn push_close(&mut self, close: Price) {
        if let Some(previous) = self.last_five_minute_close {
            let previous_value = previous.to_f64();
            if previous_value > 0.0 {
                let ret = close.to_f64() / previous_value - 1.0;
                if ret.is_finite() {
                    self.returns.push_back(ret);
                    if self.returns.len() > WINDOW_RETURNS {
                        self.returns.pop_front();
                    }
                }
            }
        }
        self.last_five_minute_close = Some(close);
    }

    fn tail_concentration(&self) -> Option<f64> {
        let mut losses: Vec<f64> = self
            .returns
            .iter()
            .filter_map(|ret| (*ret < 0.0).then_some(-*ret))
            .collect();

        if losses.len() < MIN_NEGATIVE_RETURNS {
            return None;
        }

        let total_loss: f64 = losses.iter().sum();
        if !total_loss.is_finite() || total_loss <= 0.0 {
            return None;
        }

        losses.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let top_count = (losses.len() + 19) / 20;
        let top_loss: f64 = losses.iter().take(top_count).sum();
        Some(top_loss / total_loss)
    }
}

impl Strategy for DownsideTailGate {
    fn name(&self) -> String {
        "downside-tail-concentration-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        if minute_ns <= 0 {
            return;
        }
        let minute = bar.close_ts.as_nanos() / minute_ns;
        if minute.rem_euclid(5) != 0 {
            return;
        }

        self.push_close(bar.close);
        let concentration = self.tail_concentration();
        let position = ctx.position(INSTRUMENT);
        let now_ns = bar.close_ts.as_nanos();

        if position.is_positive() {
            self.below_since_ns = None;
            if concentration.is_some_and(|value| value > 0.50) && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_zero() {
            self.below_since_ns = None;
            return;
        }

        let is_below_reentry_threshold = concentration.is_some_and(|value| value < 0.35);
        if !is_below_reentry_threshold {
            self.below_since_ns = None;
            return;
        }

        let since_ns = *self.below_since_ns.get_or_insert(now_ns);
        let recovery_duration_ns = TimeDelta::from_hours(12).as_nanos();
        if now_ns.saturating_sub(since_ns) < recovery_duration_ns || ctx.open_orders() != 0 {
            return;
        }

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

export_strategy!(DownsideTailGate {
    last_five_minute_close: None,
    returns: VecDeque::with_capacity(WINDOW_RETURNS),
    below_since_ns: None,
});
