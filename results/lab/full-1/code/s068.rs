use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EFFICIENCY_WINDOW: usize = 360;
const LONG_RETURN_WINDOW: usize = 1_440;
const COOLDOWN_BARS: usize = 60;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Metrics {
    efficiency: f64,
    return_360: f64,
    return_1440: f64,
}

struct PathEfficiencyParticipation {
    closes: VecDeque<i64>,
    cooldown_remaining: usize,
}

impl PathEfficiencyParticipation {
    fn metrics(&self) -> Option<Metrics> {
        let len = self.closes.len();
        if len < LONG_RETURN_WINDOW + 1 {
            return None;
        }

        let current = tape_guest::Price::from_raw(*self.closes.back()?).to_f64();
        let close_360 = tape_guest::Price::from_raw(self.closes[len - EFFICIENCY_WINDOW - 1]).to_f64();
        let close_1440 = tape_guest::Price::from_raw(self.closes[len - LONG_RETURN_WINDOW - 1]).to_f64();
        if current <= 0.0 || close_360 <= 0.0 || close_1440 <= 0.0 {
            return None;
        }

        let mut path_length = 0.0;
        let first_delta_index = len - EFFICIENCY_WINDOW + 1;
        for i in first_delta_index..len {
            let previous = tape_guest::Price::from_raw(self.closes[i - 1]).to_f64();
            let next = tape_guest::Price::from_raw(self.closes[i]).to_f64();
            path_length += (next - previous).abs();
        }

        let displacement = (current - close_360).abs();
        let efficiency = if path_length > 0.0 {
            displacement / path_length
        } else {
            0.0
        };

        Some(Metrics {
            efficiency,
            return_360: current / close_360 - 1.0,
            return_1440: current / close_1440 - 1.0,
        })
    }
}

impl Strategy for PathEfficiencyParticipation {
    fn name(&self) -> String {
        "path-efficiency-participation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.closes.push_back(bar.close.raw());
        if self.closes.len() > LONG_RETURN_WINDOW + 1 {
            self.closes.pop_front();
        }

        if self.cooldown_remaining > 0 {
            self.cooldown_remaining -= 1;
        }

        let Some(metrics) = self.metrics() else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if metrics.return_360 <= 0.0 || metrics.efficiency < 0.20 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_zero() || self.cooldown_remaining > 0 {
            return;
        }

        if metrics.efficiency >= 0.55
            && metrics.return_360 >= 0.01
            && metrics.return_1440 > 0.0
        {
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

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Sell {
            self.cooldown_remaining = COOLDOWN_BARS;
        }
    }
}

export_strategy!(PathEfficiencyParticipation {
    closes: VecDeque::with_capacity(LONG_RETURN_WINDOW + 1),
    cooldown_remaining: 0,
});
