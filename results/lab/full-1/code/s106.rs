use std::collections::VecDeque;

use tape_guest::{export_strategy, Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_BARS: usize = 720;
const ENTRY_THRESHOLD: f64 = 0.20;
const EXIT_THRESHOLD: f64 = -0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct CloseLocationFlowPressure {
    window: VecDeque<(f64, f64)>,
    signed_volume_sum: f64,
    volume_sum: f64,
    entry_ts_nanos: Option<i64>,
}

impl CloseLocationFlowPressure {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_BARS + 1),
            signed_volume_sum: 0.0,
            volume_sum: 0.0,
            entry_ts_nanos: None,
        }
    }

    fn push_bar(&mut self, bar: &Bar) -> Option<f64> {
        let volume = bar.volume.to_f64();
        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let close = bar.close.to_f64();

        let signed_volume = if bar.high.raw() == bar.low.raw() {
            0.0
        } else {
            volume * ((2.0 * close - high - low) / (high - low))
        };

        self.window.push_back((signed_volume, volume));
        self.signed_volume_sum += signed_volume;
        self.volume_sum += volume;

        if self.window.len() > WINDOW_BARS {
            if let Some((old_signed_volume, old_volume)) = self.window.pop_front() {
                self.signed_volume_sum -= old_signed_volume;
                self.volume_sum -= old_volume;
            }
        }

        if self.window.len() == WINDOW_BARS && self.volume_sum > 0.0 {
            Some(self.signed_volume_sum / self.volume_sum)
        } else {
            None
        }
    }
}

impl Strategy for CloseLocationFlowPressure {
    fn name(&self) -> String {
        "close-location-flow-pressure(720)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let pressure = self.push_bar(bar);
        let position = ctx.position(INSTRUMENT);

        if position.is_zero() {
            self.entry_ts_nanos = None;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_zero() {
            if pressure.is_some_and(|value| value >= ENTRY_THRESHOLD) {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() && ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
            return;
        }

        if !position.is_positive() {
            return;
        }

        let time_exit = self.entry_ts_nanos.is_some_and(|entry_ts| {
            bar.close_ts
                .as_nanos()
                .saturating_sub(entry_ts)
                >= TimeDelta::from_hours(24).as_nanos()
        });
        let signal_exit = pressure.is_some_and(|value| value <= EXIT_THRESHOLD);

        if (time_exit || signal_exit) && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_ts_nanos.is_none() {
            self.entry_ts_nanos = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(CloseLocationFlowPressure::new());
