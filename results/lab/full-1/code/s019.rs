use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LONG_FLOW_LEN: usize = 120;
const SHORT_FLOW_LEN: usize = 30;
const EMA_LEN: f64 = 240.0;
const EXIT_STREAK_LEN: usize = 10;
const ENTRY_FLOW_THRESHOLD: f64 = 0.20;
const EXIT_FLOW_THRESHOLD: f64 = -0.15;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FlowWindow {
    len: usize,
    values: VecDeque<(f64, f64)>,
    signed_volume_sum: f64,
    volume_sum: f64,
}

impl FlowWindow {
    fn new(len: usize) -> Self {
        Self {
            len,
            values: VecDeque::with_capacity(len + 1),
            signed_volume_sum: 0.0,
            volume_sum: 0.0,
        }
    }

    fn push(&mut self, signed_volume: f64, volume: f64) {
        self.values.push_back((signed_volume, volume));
        self.signed_volume_sum += signed_volume;
        self.volume_sum += volume;

        if self.values.len() > self.len {
            if let Some((old_signed, old_volume)) = self.values.pop_front() {
                self.signed_volume_sum -= old_signed;
                self.volume_sum -= old_volume;
            }
        }
    }

    fn flow(&self) -> Option<f64> {
        if self.values.len() == self.len && self.volume_sum > 0.0 {
            Some(self.signed_volume_sum / self.volume_sum)
        } else {
            None
        }
    }
}

struct FlowConfirmedTrend {
    flow_120: FlowWindow,
    flow_30: FlowWindow,
    ema: Option<f64>,
    ema_alpha: f64,
    negative_flow_streak: usize,
}

impl FlowConfirmedTrend {
    fn new() -> Self {
        Self {
            flow_120: FlowWindow::new(LONG_FLOW_LEN),
            flow_30: FlowWindow::new(SHORT_FLOW_LEN),
            ema: None,
            ema_alpha: 2.0 / (EMA_LEN + 1.0),
            negative_flow_streak: 0,
        }
    }
}

impl Strategy for FlowConfirmedTrend {
    fn name(&self) -> String {
        "Flow-Confirmed Trend".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let close = bar.close.to_f64();
        let volume = bar.volume.to_f64();
        let signed_volume = if bar.high == bar.low {
            0.0
        } else {
            volume * ((2.0 * close - high - low) / (high - low))
        };

        self.flow_120.push(signed_volume, volume);
        self.flow_30.push(signed_volume, volume);

        let close_raw = bar.close.raw() as f64;
        self.ema = Some(match self.ema {
            Some(previous) => {
                self.ema_alpha * close_raw + (1.0 - self.ema_alpha) * previous
            }
            None => close_raw,
        });

        let position = ctx.position(INSTRUMENT);
        let flow_30 = self.flow_30.flow();
        if position.is_positive() {
            if flow_30.is_some_and(|flow| flow < EXIT_FLOW_THRESHOLD) {
                self.negative_flow_streak = self.negative_flow_streak.saturating_add(1);
            } else {
                self.negative_flow_streak = 0;
            }
        } else {
            self.negative_flow_streak = 0;
        }

        let Some(ema) = self.ema else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            if close_raw < ema || self.negative_flow_streak >= EXIT_STREAK_LEN {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if let Some(flow_120) = self.flow_120.flow() {
            if flow_120 > ENTRY_FLOW_THRESHOLD && close_raw > ema {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
        }
    }
}

export_strategy!(FlowConfirmedTrend::new());
