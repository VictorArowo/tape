use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW: usize = 1_440;
const ENTRY_Z: f64 = -1.5;
const STOP_Z: f64 = -2.5;
const MAX_HOLD_BARS: usize = 720;
const COOLDOWN_BARS: usize = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeightedLogWindow {
    samples: VecDeque<(f64, f64)>,
    sum_w: f64,
    sum_wx: f64,
    sum_wx2: f64,
}

impl WeightedLogWindow {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(WINDOW + 1),
            sum_w: 0.0,
            sum_wx: 0.0,
            sum_wx2: 0.0,
        }
    }

    fn push(&mut self, bar: &Bar) {
        let typical = (bar.high.to_f64() + bar.low.to_f64() + bar.close.to_f64()) / 3.0;
        let volume = bar.volume.to_f64();
        let (x, weight) = if typical.is_finite() && typical > 0.0
            && volume.is_finite() && volume > 0.0
        {
            (typical.ln(), volume)
        } else {
            (0.0, 0.0)
        };

        self.samples.push_back((x, weight));
        self.sum_w += weight;
        self.sum_wx += weight * x;
        self.sum_wx2 += weight * x * x;

        if self.samples.len() > WINDOW {
            if let Some((old_x, old_weight)) = self.samples.pop_front() {
                self.sum_w -= old_weight;
                self.sum_wx -= old_weight * old_x;
                self.sum_wx2 -= old_weight * old_x * old_x;
            }
        }
    }

    fn z_score(&self, close: tape_guest::Price) -> Option<f64> {
        if self.samples.len() < WINDOW || !self.sum_w.is_finite() || self.sum_w <= 0.0 {
            return None;
        }

        let close_value = close.to_f64();
        if !close_value.is_finite() || close_value <= 0.0 {
            return None;
        }

        let mean = self.sum_wx / self.sum_w;
        let variance = (self.sum_wx2 / self.sum_w - mean * mean).max(0.0);
        let sigma = variance.sqrt();
        if !sigma.is_finite() || sigma <= 0.0 {
            return None;
        }

        let z = (close_value.ln() - mean) / sigma;
        z.is_finite().then_some(z)
    }
}

struct RollingVwapDislocation {
    window: WeightedLogWindow,
    held_bars: usize,
    cooldown_remaining: usize,
    exit_triggered: bool,
}

impl Strategy for RollingVwapDislocation {
    fn name(&self) -> String {
        "rolling-vwap-dislocation(1440)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.window.push(bar);
        let z = self.window.z_score(bar.close);
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            self.held_bars = self.held_bars.saturating_add(1);
            if self.held_bars >= MAX_HOLD_BARS
                || z.is_some_and(|value| value >= 0.0 || value <= STOP_Z)
            {
                self.exit_triggered = true;
            }

            if self.exit_triggered && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if self.cooldown_remaining > 0 {
            self.cooldown_remaining -= 1;
        }

        if !position.is_zero() || self.cooldown_remaining > 0 || ctx.open_orders() != 0 {
            return;
        }

        if z.is_some_and(|value| value <= ENTRY_Z) {
            let budget = ctx.equity().mul_ratio(1, 2);
            if budget.is_positive() {
                let qty = ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.held_bars = 0;
            self.exit_triggered = false;
            self.cooldown_remaining = COOLDOWN_BARS;
        }
    }
}

export_strategy!(RollingVwapDislocation {
    window: WeightedLogWindow::new(),
    held_bars: 0,
    cooldown_remaining: 0,
    exit_triggered: false,
});
