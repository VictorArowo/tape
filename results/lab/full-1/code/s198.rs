use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW: usize = 360;
const BUY_THRESHOLD: f64 = 0.15;
const SELL_THRESHOLD: f64 = -0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RollingFlow {
    window: VecDeque<(f64, f64)>,
    flow_sum: f64,
    volume_sum: f64,
}

impl RollingFlow {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW + 1),
            flow_sum: 0.0,
            volume_sum: 0.0,
        }
    }

    fn push(&mut self, bar: &Bar) {
        let volume = bar.volume.to_f64();
        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let flow = if bar.high == bar.low {
            0.0
        } else {
            ((2.0 * bar.close.to_f64() - high - low) / (high - low)) * volume
        };

        self.window.push_back((flow, volume));
        self.flow_sum += flow;
        self.volume_sum += volume;

        if self.window.len() > WINDOW {
            if let Some((old_flow, old_volume)) = self.window.pop_front() {
                self.flow_sum -= old_flow;
                self.volume_sum -= old_volume;
            }
        }
    }

    fn ratio(&self) -> Option<f64> {
        if self.window.len() < WINDOW {
            None
        } else if self.volume_sum == 0.0 {
            Some(0.0)
        } else {
            Some(self.flow_sum / self.volume_sum)
        }
    }
}

struct RollingSignedFlowPersistence {
    flow: RollingFlow,
}

impl Strategy for RollingSignedFlowPersistence {
    fn name(&self) -> String {
        "rolling-signed-flow-persistence(360)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.flow.push(bar);
        let Some(ratio) = self.flow.ratio() else {
            return;
        };

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if ratio >= BUY_THRESHOLD && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if ratio <= SELL_THRESHOLD && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(RollingSignedFlowPersistence {
    flow: RollingFlow::new(),
});
